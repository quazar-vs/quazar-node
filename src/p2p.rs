//! P2P reachability: announce / tip / resolve endpoint by node_id.
//! Trust identity stays in `nodes`; live dial tips in `node_endpoints`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Extension, Path, State},
    response::IntoResponse,
    Json,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::auth::{node_secret, AuthContext};
use crate::nodes::{Node, NodeRegistry, NodeStatus};
use crate::response::{self, ApiResponse};
use crate::AppState;

/// Prefer endpoints fresher than this for dialing.
const ENDPOINT_FRESH: ChronoDuration = ChronoDuration::minutes(30);

pub async fn upsert_endpoint(
    pool: &sqlx::PgPool,
    node_id: &str,
    endpoint: &str,
    source: &str,
) -> Result<(), sqlx::Error> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        return Ok(());
    }
    // Ensure trust row exists (identity without requiring url).
    sqlx::query(
        r#"
        INSERT INTO nodes (id, url, public_key, status, last_seen, version, created_at)
        VALUES ($1, '', '', 'alive', NOW(), '0.7.0', NOW())
        ON CONFLICT (id) DO UPDATE SET
            status = 'alive',
            last_seen = NOW()
        "#,
    )
    .bind(node_id)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO node_endpoints (node_id, endpoint, last_seen, source)
        VALUES ($1, $2, NOW(), $3)
        ON CONFLICT (node_id, endpoint) DO UPDATE SET
            last_seen = NOW(),
            source = EXCLUDED.source
        "#,
    )
    .bind(node_id)
    .bind(endpoint)
    .bind(source)
    .execute(pool)
    .await?;
    Ok(())
}

fn is_http_endpoint(ep: &str) -> bool {
    let e = ep.trim();
    e.starts_with("https://") || e.starts_with("http://")
}

/// Best HTTP(S) base URL to dial `node_id` (never returns `quic://`).
pub async fn resolve_base_url(pool: &sqlx::PgPool, node_id: &str) -> Option<String> {
    let fresh_cut = Utc::now() - ENDPOINT_FRESH;
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
    for ep in tips {
        let ep = ep.trim().trim_end_matches('/');
        if is_http_endpoint(ep) {
            return Some(ep.to_string());
        }
    }

    // Any HTTP endpoint (stale) still better than nothing for retry.
    let any: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT endpoint FROM node_endpoints
        WHERE node_id = $1
        ORDER BY last_seen DESC
        "#,
    )
    .bind(node_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for ep in any {
        let ep = ep.trim().trim_end_matches('/');
        if is_http_endpoint(ep) {
            return Some(ep.to_string());
        }
    }

    let legacy: Option<String> =
        sqlx::query_scalar("SELECT COALESCE(url, '') FROM nodes WHERE id = $1")
            .bind(node_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    legacy
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| is_http_endpoint(u))
}

#[derive(Debug, Deserialize)]
pub struct AnnounceBody {
    pub node_id: String,
    pub endpoints: Vec<String>,
    pub version: Option<String>,
    pub public_key: Option<String>,
}

/// POST /p2p/announce — node reports current reachability tips (dynamic IP ok).
pub async fn announce_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<AnnounceBody>,
) -> impl IntoResponse {
    if !auth.is_node && !auth.can_manage_peers() {
        return response::forbidden("Only node credentials (or Aiya/Founder) may announce");
    }
    let node_id = body.node_id.trim();
    if node_id.is_empty() {
        return response::bad_request("node_id required");
    }
    let endpoints: Vec<String> = body
        .endpoints
        .iter()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if endpoints.is_empty() {
        return response::bad_request("endpoints must be non-empty");
    }

    let version = body
        .version
        .as_deref()
        .unwrap_or("0.7.0")
        .trim()
        .to_string();
    let node = Node {
        id: node_id.to_string(),
        url: endpoints[0].clone(),
        public_key: body.public_key.clone(),
        status: NodeStatus::Alive,
        last_seen: Utc::now(),
        version,
    };
    if let Err(e) = state.node_registry.upsert_node(&node).await {
        return response::internal_error(format!("upsert node: {e}"));
    }
    for ep in &endpoints {
        if let Err(e) = upsert_endpoint(&state.db, node_id, ep, "announce").await {
            return response::internal_error(format!("endpoint: {e}"));
        }
    }
    Json(ApiResponse::success(json!({
        "node_id": node_id,
        "endpoints": endpoints,
        "message": "announce accepted",
    })))
    .into_response()
}

/// GET /p2p/tip/:node_id — current dial tips for a trusted node.
pub async fn tip_handler(
    State(state): State<Arc<AppState>>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let node_id = node_id.trim();
    let http = resolve_base_url(&state.db, node_id).await;
    let quic = crate::p2p_quic::resolve_quic_addr(&state.db, node_id)
        .await
        .map(|a| format!("quic://{}", a));
    if http.is_none() && quic.is_none() {
        return response::not_found(format!("no endpoint for {node_id}"));
    }
    Json(ApiResponse::success(json!({
        "node_id": node_id,
        "endpoint": http.clone().or_else(|| quic.clone()),
        "http": http,
        "quic": quic,
    })))
    .into_response()
}

/// GET /p2p/trusted — trust list (IDs), endpoints optional.
pub async fn trusted_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let nodes = match state.node_registry.get_all_nodes().await {
        Ok(n) => n,
        Err(e) => return response::internal_error(e.to_string()),
    };
    let mut out = Vec::new();
    for n in nodes {
        let tip = resolve_base_url(&state.db, &n.id).await;
        let quic = crate::p2p_quic::resolve_quic_addr(&state.db, &n.id)
            .await
            .map(|a| format!("quic://{}", a));
        out.push(json!({
            "node_id": n.id,
            "public_key": n.public_key,
            "status": n.status.to_string(),
            "endpoint": tip,
            "quic": quic,
            "version": n.version,
            "last_seen": n.last_seen.to_rfc3339(),
        }));
    }
    Json(ApiResponse::success(json!({
        "count": out.len(),
        "nodes": out,
    })))
    .into_response()
}

fn public_announce_endpoints(node_url: &str) -> Vec<String> {
    let mut eps = Vec::new();
    if let Ok(u) = std::env::var("QUAZAR_PUBLIC_URL") {
        let u = u.trim().trim_end_matches('/').to_string();
        if !u.is_empty() {
            eps.push(u);
        }
    }
    let u = node_url.trim().trim_end_matches('/');
    if !u.is_empty()
        && !u.contains("127.0.0.1")
        && !u.contains("localhost")
        && !eps.iter().any(|e| e == u)
    {
        eps.push(u.to_string());
    }
    if let Some(quic) = crate::p2p_quic::quic_endpoint_tip() {
        if !eps.iter().any(|e| e == &quic) {
            eps.push(quic);
        }
    }
    eps
}

/// Periodically announce our public tips to reachable peers (outbound).
pub async fn background_announce(state: Arc<AppState>, node_url: String) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        let endpoints = public_announce_endpoints(&node_url);
        if endpoints.is_empty() {
            tracing::debug!("p2p announce skipped: no QUAZAR_PUBLIC_URL / public node url");
            continue;
        }
        // Record locally.
        for ep in &endpoints {
            let _ = upsert_endpoint(&state.db, &state.node_id, ep, "self").await;
        }
        let peers = match state.node_registry.get_all_nodes().await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => continue,
        };
        let body = json!({
            "node_id": state.node_id,
            "endpoints": endpoints,
            "version": "0.7.0",
        });
        for peer in peers {
            if peer.id == state.node_id || peer.status != NodeStatus::Alive {
                continue;
            }
            let Some(base) = resolve_base_url(&state.db, &peer.id).await else {
                continue;
            };
            let url = format!("{}/p2p/announce", base.trim_end_matches('/'));
            let client = client.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let _ = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", node_secret()))
                    .json(&body)
                    .send()
                    .await;
            });
        }
    }
}

/// After seeds applied: try outbound sync already done by background_sync;
/// also pull pending events from first reachable seed explicitly once.
pub async fn bootstrap_pull_once(state: &Arc<AppState>) {
    let peers = match state.node_registry.get_all_nodes().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "bootstrap pull: no peers");
            return;
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "bootstrap pull: http client");
            return;
        }
    };
    for peer in peers {
        if peer.id == state.node_id {
            continue;
        }
        let Some(base) = resolve_base_url(&state.db, &peer.id).await else {
            continue;
        };
        let url = format!("{}/p2p/trusted", base.trim_end_matches('/'));
        match client
            .get(&url)
            .header("Authorization", format!("Bearer {}", node_secret()))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<ApiResponse>().await {
                    if let Some(nodes) = body.data.as_ref().and_then(|d| d.get("nodes")) {
                        merge_trusted_from_peer(state, nodes).await;
                    }
                }
                tracing::info!(peer = %peer.id, %base, "bootstrap: trusted list pulled");
                return;
            }
            Ok(resp) => {
                tracing::debug!(peer = %peer.id, status = %resp.status(), "bootstrap trusted pull miss");
            }
            Err(e) => {
                tracing::debug!(peer = %peer.id, error = %e, "bootstrap trusted pull failed");
            }
        }
    }
    tracing::warn!("bootstrap: no reachable seed answered /p2p/trusted");
}

async fn merge_trusted_from_peer(state: &Arc<AppState>, nodes: &serde_json::Value) {
    let Some(arr) = nodes.as_array() else {
        return;
    };
    for n in arr {
        let id = n.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() || id == state.node_id {
            continue;
        }
        let endpoint = n
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let pubkey = n
            .get("public_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let version = n
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.7.0")
            .to_string();
        let node = Node {
            id: id.to_string(),
            url: endpoint.to_string(),
            public_key: pubkey,
            status: NodeStatus::Alive,
            last_seen: Utc::now(),
            version,
        };
        let _ = state.node_registry.upsert_node(&node).await;
        if !endpoint.is_empty() {
            let _ = upsert_endpoint(&state.db, id, endpoint, "trusted_pull").await;
        }
    }
}

/// Resolve dial URL for gossip/sync helpers.
pub async fn peer_dial_url(registry: &NodeRegistry, pool: &sqlx::PgPool, peer: &Node) -> Option<String> {
    let _ = registry;
    resolve_base_url(pool, &peer.id).await
}
