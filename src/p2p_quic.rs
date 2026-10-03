//! QUIC mesh transport + 1-hop relay.
//!
//! Identity stays `node_id` + shared node secret (Hello frame).
//! TLS certs are ephemeral self-signed; peer auth is the Hello token, not the cert.
//! Reachability tips use `quic://host:port` in `node_endpoints`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, BufMut, BytesMut};
use quinn::{crypto::rustls::QuicClientConfig, ClientConfig, Endpoint, RecvStream, SendStream, ServerConfig};
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde::{Deserialize, Serialize};

use crate::auth::node_secret;
use crate::chat::ChatMessageRow;
use crate::models::Event;
use crate::nodes::NodeStatus;
use crate::p2p;
use crate::AppState;

const MAX_FRAME: usize = 2 * 1024 * 1024;
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
const RELAY_HOP_BUDGET: u8 = 1;

pub fn quic_port() -> u16 {
    std::env::var("QUAZAR_QUIC_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8443)
}

/// Advertise host for `quic://` tips (IP or DNS). Empty → derive from public HTTP URL.
pub fn quic_advertise_host() -> Option<String> {
    if let Ok(h) = std::env::var("QUAZAR_QUIC_ADVERTISE") {
        let h = h.trim().trim_start_matches("quic://");
        let h = h.split(':').next().unwrap_or("").trim();
        if !h.is_empty() {
            return Some(h.to_string());
        }
    }
    for key in ["QUAZAR_PUBLIC_URL", "QUAZAR_NODE_URL"] {
        if let Ok(u) = std::env::var(key) {
            if let Some(host) = host_from_url(&u) {
                if host != "127.0.0.1" && host != "localhost" {
                    return Some(host);
                }
            }
        }
    }
    None
}

pub fn quic_endpoint_tip() -> Option<String> {
    let host = quic_advertise_host()?;
    Some(format!("quic://{}:{}", host, quic_port()))
}

fn host_from_url(url: &str) -> Option<String> {
    let u = url.trim();
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .or_else(|| u.strip_prefix("quic://"))
        .unwrap_or(u);
    let host = rest.split('/').next()?.split(':').next()?.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Parse `quic://host:port` or fall back to HTTP tip host + QUAZAR_QUIC_PORT.
pub async fn resolve_quic_addr(pool: &sqlx::PgPool, node_id: &str) -> Option<SocketAddr> {
    let fresh_cut = chrono::Utc::now() - chrono::Duration::minutes(30);
    let tips: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT endpoint FROM node_endpoints
        WHERE node_id = $1 AND last_seen >= $2
        ORDER BY last_seen DESC
        "#,
    )
    .bind(node_id)
    .bind(fresh_cut)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    for tip in &tips {
        if let Some(addr) = parse_quic_endpoint(tip) {
            return Some(addr);
        }
    }
    // Any tip (stale) quic://
    let any: Vec<String> = sqlx::query_scalar(
        "SELECT endpoint FROM node_endpoints WHERE node_id = $1 ORDER BY last_seen DESC",
    )
    .bind(node_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for tip in &any {
        if let Some(addr) = parse_quic_endpoint(tip) {
            return Some(addr);
        }
    }
    // Derive from HTTP tip
    if let Some(http) = p2p::resolve_base_url(pool, node_id).await {
        if let Some(host) = host_from_url(&http) {
            return resolve_host_port(&host, quic_port());
        }
    }
    None
}

fn parse_quic_endpoint(tip: &str) -> Option<SocketAddr> {
    let tip = tip.trim();
    if !tip.starts_with("quic://") {
        return None;
    }
    let rest = tip.trim_start_matches("quic://");
    let (host, port) = if let Some((h, p)) = rest.rsplit_once(':') {
        let port: u16 = p.parse().ok()?;
        (h, port)
    } else {
        (rest, quic_port())
    };
    resolve_host_port(host, port)
}

fn resolve_host_port(host: &str, port: u16) -> Option<SocketAddr> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    format!("{host}:{port}")
        .to_socket_addrs()
        .ok()?
        .next()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum MeshMsg {
    Hello {
        node_id: String,
        token: String,
    },
    HelloOk {
        node_id: String,
    },
    HelloErr {
        error: String,
    },
    Ping,
    Pong,
    GossipEvent {
        event: Event,
    },
    GossipChat {
        message: ChatMessageRow,
    },
    GossipOk {
        id: String,
    },
    GossipErr {
        error: String,
    },
    /// Deliver `inner` to `to` (direct or via further relay if hop_budget > 0).
    Relay {
        to: String,
        hop_budget: u8,
        inner: Box<MeshMsg>,
    },
    RelayOk {
        via: String,
    },
    RelayErr {
        error: String,
    },
}

fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn make_server_config() -> Result<ServerConfig, String> {
    install_crypto();
    let params = CertificateParams::new(vec!["quazar-node".into()])
        .map_err(|e| format!("cert params: {e}"))?;
    let key_pair =
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| format!("key: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("self_signed: {e}"))?;
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| format!("tls server: {e}"))?;
    server_crypto.alpn_protocols = vec![b"quazar-p2p".to_vec()];
    let mut server = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
            .map_err(|e| format!("quic server: {e}"))?,
    ));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()));
    transport.keep_alive_interval(Some(Duration::from_secs(10)));
    server.transport_config(Arc::new(transport));
    Ok(server)
}

fn make_client_config() -> Result<ClientConfig, String> {
    install_crypto();
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"quazar-p2p".to_vec()];
    let mut client = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).map_err(|e| format!("quic client: {e}"))?,
    ));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()));
    transport.keep_alive_interval(Some(Duration::from_secs(10)));
    client.transport_config(Arc::new(transport));
    Ok(client)
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
    }
}

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

async fn write_frame(send: &mut SendStream, msg: &MeshMsg) -> Result<(), String> {
    let body = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    if body.len() > MAX_FRAME {
        return Err("frame too large".into());
    }
    let mut hdr = BytesMut::with_capacity(4 + body.len());
    hdr.put_u32(body.len() as u32);
    hdr.extend_from_slice(&body);
    send.write_all(&hdr)
        .await
        .map_err(|e| format!("write: {e}"))?;
    Ok(())
}

async fn read_frame(recv: &mut RecvStream) -> Result<MeshMsg, String> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read len: {e}"))?;
    let len = (&len_buf[..]).get_u32() as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(format!("bad frame len {len}"));
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .map_err(|e| format!("read body: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("json: {e}"))
}

async fn open_session(
    addr: SocketAddr,
) -> Result<(quinn::Connection, SendStream, RecvStream), String> {
    let client_cfg = make_client_config()?;
    let mut endpoint = Endpoint::client(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
        .map_err(|e| format!("endpoint: {e}"))?;
    endpoint.set_default_client_config(client_cfg);
    let connecting = endpoint
        .connect(addr, "quazar-node")
        .map_err(|e| format!("connect: {e}"))?;
    let conn = tokio::time::timeout(DIAL_TIMEOUT, connecting)
        .await
        .map_err(|_| "dial timeout".to_string())?
        .map_err(|e| format!("handshake: {e}"))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| format!("open_bi: {e}"))?;
    // Hello
    let hello = MeshMsg::Hello {
        node_id: std::env::var("QUAZAR_NODE_ID").unwrap_or_else(|_| "unknown".into()),
        token: node_secret().to_string(),
    };
    write_frame(&mut send, &hello).await?;
    match read_frame(&mut recv).await? {
        MeshMsg::HelloOk { .. } => Ok((conn, send, recv)),
        MeshMsg::HelloErr { error } => Err(format!("hello rejected: {error}")),
        other => Err(format!("unexpected hello reply: {other:?}")),
    }
}

/// One-shot request/response over QUIC to `addr`.
async fn rpc(addr: SocketAddr, msg: MeshMsg) -> Result<MeshMsg, String> {
    let (_conn, mut send, mut recv) = open_session(addr).await?;
    write_frame(&mut send, &msg).await?;
    let reply = read_frame(&mut recv).await?;
    let _ = send.finish();
    Ok(reply)
}

/// Deliver `msg` to `target_id`: direct QUIC, else relay via any other reachable peer.
pub async fn deliver(state: &Arc<AppState>, target_id: &str, msg: MeshMsg) -> Result<(), String> {
    if let Some(addr) = resolve_quic_addr(&state.db, target_id).await {
        match rpc(addr, msg.clone()).await {
            Ok(MeshMsg::GossipOk { .. } | MeshMsg::RelayOk { .. } | MeshMsg::Pong) => {
                return Ok(());
            }
            Ok(MeshMsg::GossipErr { error } | MeshMsg::RelayErr { error }) => {
                tracing::debug!(target = %target_id, %error, "quic direct soft-fail, try relay");
            }
            Ok(other) => {
                tracing::debug!(target = %target_id, reply = ?other, "quic direct unexpected, try relay");
            }
            Err(e) => {
                tracing::debug!(target = %target_id, error = %e, "quic direct fail, try relay");
            }
        }
    }

    // Relay via any other peer with a QUIC tip.
    let peers = state
        .node_registry
        .get_all_nodes()
        .await
        .map_err(|e| e.to_string())?;
    for peer in peers {
        if peer.id == state.node_id || peer.id == target_id || peer.status != NodeStatus::Alive {
            continue;
        }
        let Some(via_addr) = resolve_quic_addr(&state.db, &peer.id).await else {
            continue;
        };
        let relay = MeshMsg::Relay {
            to: target_id.to_string(),
            hop_budget: RELAY_HOP_BUDGET,
            inner: Box::new(msg.clone()),
        };
        match rpc(via_addr, relay).await {
            Ok(MeshMsg::RelayOk { via }) => {
                tracing::info!(target = %target_id, %via, "quic relay ok");
                return Ok(());
            }
            Ok(MeshMsg::RelayErr { error }) => {
                tracing::debug!(via = %peer.id, %error, "quic relay hop failed");
            }
            Ok(_) | Err(_) => {}
        }
    }

    // HTTP relay fallback (when UDP blocked but HTTPS works).
    http_relay_fallback(state, target_id, &msg).await
}

async fn http_relay_fallback(
    state: &Arc<AppState>,
    target_id: &str,
    msg: &MeshMsg,
) -> Result<(), String> {
    let peers = state
        .node_registry
        .get_all_nodes()
        .await
        .map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let body = serde_json::json!({
        "to": target_id,
        "hop_budget": RELAY_HOP_BUDGET,
        "inner": msg,
    });
    for peer in peers {
        if peer.id == state.node_id || peer.id == target_id || peer.status != NodeStatus::Alive {
            continue;
        }
        let Some(base) = p2p::resolve_base_url(&state.db, &peer.id).await else {
            continue;
        };
        let url = format!("{}/p2p/relay", base.trim_end_matches('/'));
        match client
            .post(&url)
            .header("Authorization", format!("Bearer {}", node_secret()))
            .json(&body)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(target = %target_id, via = %peer.id, "http relay ok");
                return Ok(());
            }
            _ => {}
        }
    }
    Err(format!("no path to {target_id} (quic/relay exhausted)"))
}

pub async fn push_gossip_event(state: Arc<AppState>, event: Event) {
    let peers = match state.node_registry.get_all_nodes().await {
        Ok(p) => p,
        Err(_) => return,
    };
    let my = state.node_id.clone();
    for peer in peers {
        if peer.id == my || peer.status != NodeStatus::Alive {
            continue;
        }
        let state = state.clone();
        let event = event.clone();
        let peer_id = peer.id.clone();
        tokio::spawn(async move {
            let msg = MeshMsg::GossipEvent { event: event.clone() };
            if let Err(e) = deliver(&state, &peer_id, msg).await {
                // Last resort: classic HTTP gossip
                if let Some(base) = p2p::resolve_base_url(&state.db, &peer_id).await {
                    let url = format!("{}/events/gossip", base.trim_end_matches('/'));
                    let _ = reqwest::Client::new()
                        .post(&url)
                        .header("Authorization", format!("Bearer {}", node_secret()))
                        .json(&event)
                        .timeout(Duration::from_secs(3))
                        .send()
                        .await;
                    tracing::debug!(peer = %peer_id, error = %e, "quic mesh miss → http gossip");
                } else {
                    tracing::warn!(peer = %peer_id, error = %e, "gossip undeliverable");
                }
            }
        });
    }
}

pub async fn push_gossip_chat(state: Arc<AppState>, message: ChatMessageRow) {
    let peers = match state.node_registry.get_all_nodes().await {
        Ok(p) => p,
        Err(_) => return,
    };
    let my = state.node_id.clone();
    for peer in peers {
        if peer.id == my || peer.status != NodeStatus::Alive {
            continue;
        }
        let state = state.clone();
        let message = message.clone();
        let peer_id = peer.id.clone();
        tokio::spawn(async move {
            let msg = MeshMsg::GossipChat {
                message: message.clone(),
            };
            if let Err(e) = deliver(&state, &peer_id, msg).await {
                if let Some(base) = p2p::resolve_base_url(&state.db, &peer_id).await {
                    let url = format!("{}/chat/gossip", base.trim_end_matches('/'));
                    let _ = reqwest::Client::new()
                        .post(&url)
                        .header("Authorization", format!("Bearer {}", node_secret()))
                        .json(&message)
                        .timeout(Duration::from_secs(3))
                        .send()
                        .await;
                } else {
                    tracing::warn!(peer = %peer_id, error = %e, "chat gossip undeliverable");
                }
            }
        });
    }
}

/// Handle inbound bi-stream after QUIC accept.
async fn handle_stream(state: Arc<AppState>, mut send: SendStream, mut recv: RecvStream) {
    let peer_node = match read_frame(&mut recv).await {
        Ok(MeshMsg::Hello { node_id, token }) => {
            if token != node_secret() {
                let _ = write_frame(
                    &mut send,
                    &MeshMsg::HelloErr {
                        error: "bad token".into(),
                    },
                )
                .await;
                return;
            }
            let _ = write_frame(
                &mut send,
                &MeshMsg::HelloOk {
                    node_id: state.node_id.clone(),
                },
            )
            .await;
            node_id
        }
        _ => return,
    };

    let msg = match read_frame(&mut recv).await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, from = %peer_node, "quic read fail");
            return;
        }
    };

    let reply = process_msg(&state, msg).await;
    let _ = write_frame(&mut send, &reply).await;
    let _ = send.finish();
}

async fn process_msg(state: &Arc<AppState>, msg: MeshMsg) -> MeshMsg {
    match msg {
        MeshMsg::Ping => MeshMsg::Pong,
        MeshMsg::GossipEvent { event } => match crate::gossip::receive_gossip_event(state, event).await
        {
            Ok(resp) => {
                let id = resp
                    .data
                    .as_ref()
                    .and_then(|d| d.get("event_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                MeshMsg::GossipOk { id }
            }
            Err(e) => MeshMsg::GossipErr { error: e },
        },
        MeshMsg::GossipChat { message } => {
            match crate::gossip::receive_gossip_chat_message(state, message).await {
                Ok(resp) => {
                    let id = resp
                        .data
                        .as_ref()
                        .and_then(|d| d.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    MeshMsg::GossipOk { id }
                }
                Err(e) => MeshMsg::GossipErr { error: e },
            }
        }
        MeshMsg::Relay {
            to,
            hop_budget,
            inner,
        } => Box::pin(handle_relay(state, to, hop_budget, *inner)).await,
        other => MeshMsg::RelayErr {
            error: format!("unsupported: {other:?}"),
        },
    }
}

fn handle_relay<'a>(
    state: &'a Arc<AppState>,
    to: String,
    hop_budget: u8,
    inner: MeshMsg,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = MeshMsg> + Send + 'a>> {
    Box::pin(async move {
        if to == state.node_id {
            return process_msg(state, inner).await;
        }
        if let Some(addr) = resolve_quic_addr(&state.db, &to).await {
            match rpc(addr, inner.clone()).await {
                Ok(MeshMsg::GossipOk { .. } | MeshMsg::Pong) => {
                    return MeshMsg::RelayOk {
                        via: state.node_id.clone(),
                    };
                }
                Ok(MeshMsg::GossipErr { error }) => {
                    return MeshMsg::RelayErr { error };
                }
                Ok(_) | Err(_) => {}
            }
        }
        if hop_budget == 0 {
            return MeshMsg::RelayErr {
                error: format!("no quic path to {to}"),
            };
        }
        let peers = match state.node_registry.get_all_nodes().await {
            Ok(p) => p,
            Err(e) => {
                return MeshMsg::RelayErr {
                    error: e.to_string(),
                };
            }
        };
        for peer in peers {
            if peer.id == state.node_id || peer.id == to || peer.status != NodeStatus::Alive {
                continue;
            }
            let Some(addr) = resolve_quic_addr(&state.db, &peer.id).await else {
                continue;
            };
            let next = MeshMsg::Relay {
                to: to.clone(),
                hop_budget: hop_budget.saturating_sub(1),
                inner: Box::new(inner.clone()),
            };
            if let Ok(MeshMsg::RelayOk { .. }) = rpc(addr, next).await {
                return MeshMsg::RelayOk {
                    via: peer.id.clone(),
                };
            }
        }
        MeshMsg::RelayErr {
            error: format!("relay exhausted for {to}"),
        }
    })
}

/// Bind UDP and accept QUIC connections forever.
pub async fn run_listener(state: Arc<AppState>) {
    let port = quic_port();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    let server_cfg = match make_server_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "quic: cannot build server config");
            return;
        }
    };
    let endpoint = match Endpoint::server(server_cfg, addr) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(%addr, error = %e, "quic: bind failed");
            return;
        }
    };
    tracing::info!(%addr, tip = ?quic_endpoint_tip(), "quic mesh listening");

    // Register our tip locally.
    if let Some(tip) = quic_endpoint_tip() {
        let _ = p2p::upsert_endpoint(&state.db, &state.node_id, &tip, "self_quic").await;
    }

    loop {
        let Some(connecting) = endpoint.accept().await else {
            break;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let conn = match connecting.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(error = %e, "quic accept handshake fail");
                    return;
                }
            };
            loop {
                match conn.accept_bi().await {
                    Ok((send, recv)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            handle_stream(state, send, recv).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        });
    }
}

// --- HTTP relay API (UDP-blocked peers) ---

#[derive(Debug, Deserialize)]
pub struct HttpRelayBody {
    pub to: String,
    pub hop_budget: Option<u8>,
    pub inner: MeshMsg,
}

pub async fn relay_http_handler(
    axum::extract::Extension(auth): axum::extract::Extension<crate::auth::AuthContext>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::Json(body): axum::Json<HttpRelayBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use axum::Json;
    use serde_json::json;

    use crate::response::{self, ApiResponse};

    if !auth.is_node && !auth.can_manage_peers() {
        return response::forbidden("Only node credentials may relay");
    }
    let to = body.to.trim();
    if to.is_empty() {
        return response::bad_request("to required");
    }
    let hop = body.hop_budget.unwrap_or(RELAY_HOP_BUDGET);
    let reply = handle_relay(&state, body.to.trim().to_string(), hop, body.inner).await;
    match reply {
        MeshMsg::RelayOk { via } | MeshMsg::GossipOk { id: via } => {
            Json(ApiResponse::success(json!({ "via": via }))).into_response()
        }
        MeshMsg::RelayErr { error } | MeshMsg::GossipErr { error } => {
            response::bad_request(error)
        }
        other => Json(ApiResponse::success(json!({ "reply": other }))).into_response(),
    }
}
