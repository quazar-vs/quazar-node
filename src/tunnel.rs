use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::{
    extract::State,
    http::HeaderMap,
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::citizen::authenticate_request;
use crate::response::{self, ApiResponse};
use crate::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct SshTunnelConfig {
    pub enabled: bool,
    pub ssh_host: String,
    pub ssh_port: u16,
    pub gateway_ports: bool,
    pub keepalive_interval_secs: u64,
    pub keepalive_count_max: u64,
    pub client_local_port: u16,
    pub remote_api_host: String,
    pub remote_api_port: u16,
    pub api_base_url_via_tunnel: String,
    pub ssh_local_forward_command: String,
    pub public_api_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelClientRecord {
    pub client_id: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub connects: u64,
    pub heartbeats: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelStats {
    pub enabled: bool,
    pub active_clients: usize,
    pub total_connects: u64,
    pub total_heartbeats: u64,
    pub total_purged: u64,
    pub idle_ttl_secs: u64,
}

#[derive(Debug, Clone)]
struct ClientEntry {
    client_id: String,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    connects: u64,
    heartbeats: u64,
}

#[derive(Debug, Default)]
pub struct TunnelRegistry {
    clients: RwLock<HashMap<String, ClientEntry>>,
    total_connects: RwLock<u64>,
    total_heartbeats: RwLock<u64>,
    total_purged: RwLock<u64>,
}

impl TunnelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn register_connect(&self, client_id: &str, now: DateTime<Utc>) {
        let mut clients = self.clients.write().expect("tunnel registry lock");
        clients
            .entry(client_id.to_string())
            .and_modify(|e| {
                e.last_seen = now;
                e.connects = e.connects.saturating_add(1);
            })
            .or_insert_with(|| ClientEntry {
                client_id: client_id.to_string(),
                first_seen: now,
                last_seen: now,
                connects: 1,
                heartbeats: 0,
            });
        *self.total_connects.write().expect("tunnel registry lock") += 1;
    }

    fn register_heartbeat(&self, client_id: &str, now: DateTime<Utc>) -> bool {
        let mut clients = self.clients.write().expect("tunnel registry lock");
        if let Some(e) = clients.get_mut(client_id) {
            e.last_seen = now;
            e.heartbeats = e.heartbeats.saturating_add(1);
            *self.total_heartbeats.write().expect("tunnel registry lock") += 1;
            true
        } else {
            clients.insert(
                client_id.to_string(),
                ClientEntry {
                    client_id: client_id.to_string(),
                    first_seen: now,
                    last_seen: now,
                    connects: 0,
                    heartbeats: 1,
                },
            );
            *self.total_heartbeats.write().expect("tunnel registry lock") += 1;
            true
        }
    }

    fn purge(&self, now: DateTime<Utc>, ttl_secs: u64) -> usize {
        let mut clients = self.clients.write().expect("tunnel registry lock");
        let removed = purge_stale_clients(&mut clients, now, ttl_secs);
        if removed > 0 {
            *self.total_purged.write().expect("tunnel registry lock") += removed as u64;
        }
        removed
    }

    fn active_count(&self) -> usize {
        self.clients.read().expect("tunnel registry lock").len()
    }

    fn list_clients(&self) -> Vec<TunnelClientRecord> {
        self.clients
            .read()
            .expect("tunnel registry lock")
            .values()
            .map(|e| TunnelClientRecord {
                client_id: e.client_id.clone(),
                first_seen: e.first_seen,
                last_seen: e.last_seen,
                connects: e.connects,
                heartbeats: e.heartbeats,
            })
            .collect()
    }

    fn stats(&self, enabled: bool, idle_ttl_secs: u64) -> TunnelStats {
        TunnelStats {
            enabled,
            active_clients: self.active_count(),
            total_connects: *self.total_connects.read().expect("tunnel registry lock"),
            total_heartbeats: *self.total_heartbeats.read().expect("tunnel registry lock"),
            total_purged: *self.total_purged.read().expect("tunnel registry lock"),
            idle_ttl_secs,
        }
    }
}

/// TTL before a client is dropped from the in-memory registry.
pub fn idle_ttl_secs(keepalive_interval_secs: u64, keepalive_count_max: u64) -> u64 {
    std::env::var("SSH_TUNNEL_IDLE_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(keepalive_interval_secs.saturating_mul(keepalive_count_max))
}

pub fn is_client_stale(last_seen: DateTime<Utc>, now: DateTime<Utc>, ttl_secs: u64) -> bool {
    let elapsed = (now - last_seen).num_seconds();
    elapsed >= 0 && elapsed as u64 >= ttl_secs
}

fn purge_stale_clients(
    clients: &mut HashMap<String, ClientEntry>,
    now: DateTime<Utc>,
    ttl_secs: u64,
) -> usize {
    let before = clients.len();
    clients.retain(|_, c| !is_client_stale(c.last_seen, now, ttl_secs));
    before.saturating_sub(clients.len())
}

fn cleanup_dead_tunnels(registry: &TunnelRegistry, settings: &Settings) -> usize {
    let ttl = idle_ttl_secs(
        settings.config.keepalive_interval_secs,
        settings.config.keepalive_count_max,
    );
    registry.purge(Utc::now(), ttl)
}

#[derive(Debug, Clone)]
struct Settings {
    config: SshTunnelConfig,
}

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| {
            let t = v.trim().to_ascii_lowercase();
            t == "1" || t == "true" || t == "yes" || t == "on"
        })
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u16(key: &str, default: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn host_from_node_url() -> Option<String> {
    let raw = std::env::var("QUAZAR_NODE_URL").ok()?;
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("https://") {
        s = rest;
    } else if let Some(rest) = s.strip_prefix("http://") {
        s = rest;
    }
    let host = s.split('/').next()?.split(':').next()?.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn load_settings() -> Settings {
    let enabled = env_bool("SSH_TUNNEL_ENABLED", false);
    let gateway_ports = env_bool("SSH_GATEWAY_PORTS", true);
    let keepalive_interval_secs = env_u64("SSH_KEEPALIVE_INTERVAL", 60);
    let keepalive_count_max = env_u64("SSH_KEEPALIVE_COUNT_MAX", 3);

    let ssh_host = std::env::var("SSH_TUNNEL_HOST")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(host_from_node_url)
        .unwrap_or_else(|| "176.98.191.38".to_string());

    let ssh_port = env_u16("SSH_TUNNEL_PORT", 22);
    let client_local_port = env_u16("SSH_TUNNEL_CLIENT_LOCAL_PORT", 18443);
    let remote_api_host = std::env::var("SSH_TUNNEL_REMOTE_API_HOST")
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let remote_api_port = env_u16("SSH_TUNNEL_REMOTE_API_PORT", 8080);

    let public_api_url = std::env::var("QUAZAR_NODE_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let api_base_url_via_tunnel = format!("http://127.0.0.1:{client_local_port}");
    let ssh_local_forward_command = format!(
        "ssh -N -o ServerAliveInterval={keepalive_interval_secs} -o ServerAliveCountMax={keepalive_count_max} \
-L {client_local_port}:{remote_api_host}:{remote_api_port} USER@{ssh_host} -p {ssh_port}"
    );

    Settings {
        config: SshTunnelConfig {
            enabled,
            ssh_host,
            ssh_port,
            gateway_ports,
            keepalive_interval_secs,
            keepalive_count_max,
            client_local_port,
            remote_api_host,
            remote_api_port,
            api_base_url_via_tunnel,
            ssh_local_forward_command,
            public_api_url,
        },
    }
}

fn settings() -> Settings {
    load_settings()
}

/// Public tunnel config snapshot for VPN / client hints.
pub fn tunnel_config() -> SshTunnelConfig {
    load_settings().config
}

/// Whether SSH tunnel / VPN key issuance is enabled on this node.
pub fn tunnel_enabled() -> bool {
    load_settings().config.enabled
}

fn disabled_response() -> ApiResponse {
    ApiResponse::error("SSH tunnel support is disabled on this node (SSH_TUNNEL_ENABLED=false)")
}

fn registry_from_state(state: &AppState) -> &Arc<TunnelRegistry> {
    &state.tunnel_registry
}

/// GET /api/tunnel/config
pub async fn tunnel_config_handler() -> impl IntoResponse {
    let s = settings();
    if !s.config.enabled {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(disabled_response()),
        )
            .into_response();
    }
    Json(ApiResponse::success(s.config)).into_response()
}

#[derive(Debug, Serialize)]
struct TunnelStatus {
    enabled: bool,
    ssh_reachable: bool,
    gateway_ports: bool,
    keepalive_interval_secs: u64,
    keepalive_count_max: u64,
    idle_ttl_secs: u64,
    active_clients: usize,
    ssh_host: String,
    ssh_port: u16,
}

async fn probe_ssh_port(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    timeout_connect(&addr).await
}

async fn timeout_connect(addr: &str) -> bool {
    let addr = addr.to_string();
    tokio::time::timeout(Duration::from_secs(3), async move {
        tokio::net::TcpStream::connect(addr).await.is_ok()
    })
    .await
    .unwrap_or(false)
}

/// GET /api/tunnel/status
pub async fn tunnel_status_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let s = settings();
    let registry = registry_from_state(&state);
    let ttl = idle_ttl_secs(
        s.config.keepalive_interval_secs,
        s.config.keepalive_count_max,
    );
    let _purged = cleanup_dead_tunnels(registry, &s);

    if !s.config.enabled {
        return Json(ApiResponse::success(TunnelStatus {
            enabled: false,
            ssh_reachable: false,
            gateway_ports: s.config.gateway_ports,
            keepalive_interval_secs: s.config.keepalive_interval_secs,
            keepalive_count_max: s.config.keepalive_count_max,
            idle_ttl_secs: ttl,
            active_clients: registry.active_count(),
            ssh_host: s.config.ssh_host.clone(),
            ssh_port: s.config.ssh_port,
        }))
        .into_response();
    }

    let host = s.config.ssh_host.clone();
    let port = s.config.ssh_port;
    let reachable = probe_ssh_port(&host, port).await;

    Json(ApiResponse::success(TunnelStatus {
        enabled: true,
        ssh_reachable: reachable,
        gateway_ports: s.config.gateway_ports,
        keepalive_interval_secs: s.config.keepalive_interval_secs,
        keepalive_count_max: s.config.keepalive_count_max,
        idle_ttl_secs: ttl,
        active_clients: registry.active_count(),
        ssh_host: host,
        ssh_port: port,
    }))
    .into_response()
}

/// GET /api/tunnel/stats — public aggregate usage.
pub async fn tunnel_stats_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let s = settings();
    let registry = registry_from_state(&state);
    let ttl = idle_ttl_secs(
        s.config.keepalive_interval_secs,
        s.config.keepalive_count_max,
    );
    let _purged = cleanup_dead_tunnels(registry, &s);
    Json(ApiResponse::success(registry.stats(s.config.enabled, ttl))).into_response()
}

async fn tunnel_admin_authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let token = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s).trim())
        .unwrap_or("");

    if token.is_empty() {
        return false;
    }

    let admin_key = std::env::var("ADMIN_API_KEY").unwrap_or_default();
    if !admin_key.is_empty() && token == admin_key {
        return true;
    }

    if let Some(auth) = authenticate_request(&state.db, token).await {
        return auth.is_admin();
    }

    false
}

/// GET /api/tunnel/clients — Founder/Aiya API key or `ADMIN_API_KEY` Bearer.
pub async fn tunnel_clients_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let s = settings();
    if !tunnel_admin_authorized(&state, &headers).await {
        return response::unauthorized(
            "Tunnel client list requires Founder/Aiya API key or ADMIN_API_KEY Bearer",
        );
    }

    let registry = registry_from_state(&state);
    let _purged = cleanup_dead_tunnels(registry, &s);
    let mut clients = registry.list_clients();
    clients.sort_by(|a, b| a.client_id.cmp(&b.client_id));
    Json(ApiResponse::success(clients)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct TunnelConnectRequest {
    pub client_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct TunnelConnectResponse {
    ready: bool,
    message: String,
    client_id: String,
    config: SshTunnelConfig,
}

/// POST /api/tunnel/connect — readiness check; registers client in the in-memory registry.
pub async fn tunnel_connect_handler(
    State(state): State<Arc<AppState>>,
    body: Option<Json<TunnelConnectRequest>>,
) -> impl IntoResponse {
    let s = settings();
    if !s.config.enabled {
        return response::service_unavailable("SSH tunnel support is disabled on this node");
    }

    let registry = registry_from_state(&state);
    let _purged = cleanup_dead_tunnels(registry, &s);

    let client_id = body
        .as_ref()
        .and_then(|b| b.client_id.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("anonymous")
        .to_string();

    registry.register_connect(&client_id, Utc::now());
    tracing::info!(client_id = %client_id, "tunnel connect requested");

    let host = s.config.ssh_host.clone();
    let port = s.config.ssh_port;
    let reachable = probe_ssh_port(&host, port).await;
    if !reachable {
        return response::service_unavailable(format!(
            "SSH port {host}:{port} is not reachable from the node"
        ));
    }

    let message = format!(
        "Open an SSH local forward, then point the app to {}. Replace USER with your SSH login.",
        s.config.api_base_url_via_tunnel
    );

    Json(ApiResponse::success(TunnelConnectResponse {
        ready: true,
        message,
        client_id,
        config: s.config,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct TunnelHeartbeatRequest {
    pub client_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct TunnelHeartbeatResponse {
    ok: bool,
    client_id: String,
}

/// POST /api/tunnel/heartbeat — optional body `client_id` (defaults to anonymous).
pub async fn tunnel_heartbeat_handler(
    State(state): State<Arc<AppState>>,
    body: Option<Json<TunnelHeartbeatRequest>>,
) -> impl IntoResponse {
    let s = settings();
    let registry = registry_from_state(&state);
    let _purged = cleanup_dead_tunnels(registry, &s);

    let client_id = body
        .as_ref()
        .and_then(|b| b.client_id.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("anonymous")
        .to_string();

    registry.register_heartbeat(&client_id, Utc::now());
    Json(ApiResponse::success(TunnelHeartbeatResponse {
        ok: true,
        client_id,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn tunnel_disabled_by_default_without_env() {
        std::env::remove_var("SSH_TUNNEL_ENABLED");
        let s = load_settings();
        assert!(!s.config.enabled);
    }

    #[test]
    fn tunnel_enabled_parses_env() {
        std::env::set_var("SSH_TUNNEL_ENABLED", "true");
        std::env::set_var("SSH_KEEPALIVE_INTERVAL", "60");
        std::env::set_var("SSH_GATEWAY_PORTS", "yes");
        let s = load_settings();
        assert!(s.config.enabled);
        assert!(s.config.gateway_ports);
        assert_eq!(s.config.keepalive_interval_secs, 60);
        std::env::remove_var("SSH_TUNNEL_ENABLED");
        std::env::remove_var("SSH_KEEPALIVE_INTERVAL");
        std::env::remove_var("SSH_GATEWAY_PORTS");
    }

    #[test]
    fn idle_ttl_defaults_to_keepalive_product() {
        std::env::remove_var("SSH_TUNNEL_IDLE_SECS");
        assert_eq!(idle_ttl_secs(60, 3), 180);
    }

    #[test]
    fn idle_ttl_env_override() {
        std::env::set_var("SSH_TUNNEL_IDLE_SECS", "90");
        assert_eq!(idle_ttl_secs(60, 3), 90);
        std::env::remove_var("SSH_TUNNEL_IDLE_SECS");
    }

    #[test]
    fn is_client_stale_respects_ttl() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let fresh = now - chrono::Duration::seconds(179);
        let stale = now - chrono::Duration::seconds(180);
        assert!(!is_client_stale(fresh, now, 180));
        assert!(is_client_stale(stale, now, 180));
    }

    #[test]
    fn purge_stale_clients_removes_only_expired() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let mut map = HashMap::new();
        map.insert(
            "alive".to_string(),
            ClientEntry {
                client_id: "alive".into(),
                first_seen: now,
                last_seen: now - chrono::Duration::seconds(10),
                connects: 1,
                heartbeats: 0,
            },
        );
        map.insert(
            "dead".to_string(),
            ClientEntry {
                client_id: "dead".into(),
                first_seen: now - chrono::Duration::seconds(500),
                last_seen: now - chrono::Duration::seconds(300),
                connects: 2,
                heartbeats: 1,
            },
        );
        let removed = purge_stale_clients(&mut map, now, 180);
        assert_eq!(removed, 1);
        assert!(map.contains_key("alive"));
        assert!(!map.contains_key("dead"));
    }

    #[test]
    fn registry_connect_and_purge() {
        let reg = TunnelRegistry::new();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        reg.register_connect("device-1", t0);
        assert_eq!(reg.active_count(), 1);
        let purged = reg.purge(t0 + chrono::Duration::seconds(181), 180);
        assert_eq!(purged, 1);
        assert_eq!(reg.active_count(), 0);
    }
}
