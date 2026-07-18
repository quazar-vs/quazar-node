use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::num::NonZeroU32;

use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use governor::{Quota, RateLimiter};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::auth::{is_node_secret, list_agent_permissions, AuthContext, KeyStore};
use crate::response::{self, ApiResponse};
use crate::types::Role;
use crate::AppState;

type KeyedLimiter = RateLimiter<
    String,
    governor::state::keyed::DashMapStateStore<String>,
    governor::clock::DefaultClock,
>;

static RATE_LIMITER: OnceLock<Arc<KeyedLimiter>> = OnceLock::new();
static PUBLIC_REGISTER_LIMITER: OnceLock<Arc<KeyedLimiter>> = OnceLock::new();
static PUBLIC_AUTH_LIMITER: OnceLock<Arc<KeyedLimiter>> = OnceLock::new();
static PUBLIC_APPLICATION_LIMITER: OnceLock<Arc<KeyedLimiter>> = OnceLock::new();
static PUBLIC_ABUSE_LIMITER: OnceLock<Arc<KeyedLimiter>> = OnceLock::new();

fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return "<empty>".to_string();
    }
    if key.len() <= 8 {
        return "***".to_string();
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
}

fn rate_limiter() -> Arc<KeyedLimiter> {
    RATE_LIMITER
        .get_or_init(|| {
            let rps = std::env::var("QUAZAR_RATE_LIMIT_RPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);
            let rps = NonZeroU32::new(rps.max(1)).unwrap_or(NonZeroU32::MIN);
            Arc::new(RateLimiter::keyed(Quota::per_second(rps)))
        })
        .clone()
}

fn per_minute_limiter(rpm: u32) -> Arc<KeyedLimiter> {
    let rpm = NonZeroU32::new(rpm.max(1)).unwrap_or(NonZeroU32::MIN);
    Arc::new(RateLimiter::keyed(Quota::per_minute(rpm)))
}

fn public_register_limiter() -> Arc<KeyedLimiter> {
    PUBLIC_REGISTER_LIMITER
        .get_or_init(|| per_minute_limiter(5))
        .clone()
}

fn public_auth_limiter() -> Arc<KeyedLimiter> {
    PUBLIC_AUTH_LIMITER
        .get_or_init(|| per_minute_limiter(10))
        .clone()
}

fn public_application_limiter() -> Arc<KeyedLimiter> {
    PUBLIC_APPLICATION_LIMITER
        .get_or_init(|| per_minute_limiter(10))
        .clone()
}

fn public_abuse_limiter() -> Arc<KeyedLimiter> {
    PUBLIC_ABUSE_LIMITER
        .get_or_init(|| per_minute_limiter(20))
        .clone()
}

/// Register / citizenship apply: 5 req/min per IP.
fn is_public_register_path(path: &str) -> bool {
    path == "/api/register"
        || path.starts_with("/api/register/")
        || path == "/auth/register"
        || path.starts_with("/auth/register/")
        || path == "/citizen/apply"
        || path.starts_with("/citizen/apply/")
        || path == "/citizen/verify-email"
        || path == "/api/citizenship-application"
}

/// Auth endpoints: `/api/auth*`, `/auth/login`, `/auth/check` — 10 req/min per IP.
fn is_public_auth_path(path: &str) -> bool {
    path == "/api/auth"
        || path.starts_with("/api/auth/")
        || path == "/auth/login"
        || path == "/auth/check"
}

/// Application endpoints: `/api/application*` — 10 req/min per IP.
fn is_public_application_path(path: &str) -> bool {
    path == "/api/application"
        || path.starts_with("/api/application/")
        || path == "/api/mayor/setup-package"
        || path.starts_with("/api/mayor/status/")
        || path == "/api/mayor/node-started"
        || path == "/api/mayor/confirm"
}

/// Other abuse-sensitive public endpoints — 20 req/min per IP.
fn is_public_abuse_path(path: &str) -> bool {
    path.starts_with("/api/channel/")
        || path == "/chat/admin/send"
        || path == "/api/search"
}

fn rate_limit_exceeded(path: &str, ip: &str) -> Response {
    tracing::warn!(path = %path, client_ip = %ip, "rate limit exceeded");
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(ApiResponse::error("Rate limit exceeded")),
    )
        .into_response()
}

fn client_ip(
    req: &axum::http::Request<axum::body::Body>,
    trusted: &crate::client_ip::TrustedNets,
) -> String {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    crate::client_ip::resolve_client_ip(req.headers(), peer, trusted)
}

fn strict_secrets_enabled() -> bool {
    std::env::var("QUAZAR_STRICT_SECRETS")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

fn default_dev_cors_origins() -> Vec<HeaderValue> {
    [
        "http://localhost:8080",
        "http://127.0.0.1:8080",
        "http://localhost:8081",
        "http://127.0.0.1:8081",
        "http://localhost:3000",
        "http://127.0.0.1:3000",
        "http://localhost:5173",
        "http://127.0.0.1:5173",
        "http://localhost:8000",
        "http://127.0.0.1:8000",
        "http://localhost:5500",
        "http://127.0.0.1:5500",
        "https://quazar.website",
        "https://www.quazar.website",
        "null",
    ]
    .iter()
    .filter_map(|origin| origin.parse().ok())
    .collect()
}

pub fn build_cors_layer() -> CorsLayer {
    match std::env::var("QUAZAR_CORS_ORIGINS") {
        Ok(origins) if origins.trim() == "*" => {
            tracing::warn!("CORS permissive (QUAZAR_CORS_ORIGINS=*)");
            CorsLayer::permissive()
        }
        Ok(origins) if !origins.trim().is_empty() => {
            let allowed: Vec<HeaderValue> = origins
                .split(',')
                .filter_map(|origin| origin.trim().parse().ok())
                .collect();
            if allowed.is_empty() {
                tracing::warn!("QUAZAR_CORS_ORIGINS is empty after parsing, using dev defaults");
                return CorsLayer::new()
                    .allow_origin(AllowOrigin::list(default_dev_cors_origins()))
                    .allow_methods(Any)
                    .allow_headers(Any);
            }
            tracing::info!(count = allowed.len(), "CORS restricted to configured origins");
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(allowed))
                .allow_methods(Any)
                .allow_headers(Any)
        }
        _ if strict_secrets_enabled() => {
            tracing::warn!(
                "QUAZAR_STRICT_SECRETS: cross-origin CORS disabled until QUAZAR_CORS_ORIGINS is set"
            );
            CorsLayer::new().allow_methods(Any).allow_headers(Any)
        }
        _ => {
            tracing::info!("CORS dev defaults (set QUAZAR_CORS_ORIGINS or * for other modes)");
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(default_dev_cors_origins()))
                .allow_methods(Any)
                .allow_headers(Any)
        }
    }
}

fn enforce_path_rbac(path: &str, auth: &AuthContext) -> Option<Response> {
    if (path == "/peers" || path == "/peers/network") && !auth.can_manage_peers() {
        return Some(response::forbidden(
            "Insufficient permissions. Only Aiya, Founder, Mayor and Guardian can manage peers (Agent: read-only)",
        ));
    }
    if path == "/exchange/balance/add" && !auth.can_add_balance() {
        return Some(response::forbidden(
            "Insufficient permissions. Only Aiya and Founder can add balance (Agent: read-only)",
        ));
    }
    None
}

/// Pending citizens may only touch passport endpoints until activated.
fn allows_pending_citizen(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    parts.len() >= 3 && parts[0] == "citizen" && parts[2] == "passport"
}

pub async fn rate_limit_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    let trusted = state.trusted_proxies.snapshot().await;
    let ip = client_ip(&req, &trusted);
    let path = req.uri().path();
    if rate_limiter().check_key(&ip).is_err() {
        return rate_limit_exceeded(path, &ip);
    }
    next.run(req).await
}

/// Stricter per-minute limits for public auth / register / application routes.
pub async fn public_rate_limit_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    let trusted = state.trusted_proxies.snapshot().await;
    let ip = client_ip(&req, &trusted);
    let path = req.uri().path();

    let limiter = if is_public_register_path(path) {
        Some(public_register_limiter())
    } else if is_public_auth_path(path) {
        Some(public_auth_limiter())
    } else if is_public_application_path(path) {
        Some(public_application_limiter())
    } else if is_public_abuse_path(path) {
        Some(public_abuse_limiter())
    } else {
        None
    };

    if let Some(limiter) = limiter {
        if limiter.check_key(&ip).is_err() {
            return rate_limit_exceeded(path, &ip);
        }
    }

    next.run(req).await
}

pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let x_api_key = req
        .headers()
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    tracing::debug!(
        path = %path,
        authorization = ?auth_header.as_deref().map(mask_key),
        x_api_key = ?x_api_key.as_deref().map(mask_key),
        "auth request"
    );

    let api_key = auth_header
        .as_deref()
        .and_then(|v| v.strip_prefix("Bearer ").map(|rest| rest.to_string()))
        .or(x_api_key)
        .unwrap_or_default();

    if api_key.is_empty() {
        tracing::warn!(path = %path, "missing API key");
        return response::unauthorized("Invalid or missing API key");
    }

    if is_node_secret(&api_key) {
        let method = req.method();
        let node_allowed = *method == Method::GET
            && (path == "/events"
                || path == "/blocks"
                || path == "/keys/internal/export")
            || *method == Method::POST
                && (path == "/events/gossip"
                    || path == "/chat/gossip"
                    || path == "/keys/internal/upsert"
                    || path == "/keys/internal/revoke");
        if node_allowed {
            tracing::debug!(path = %path, "node secret accepted");
            req.extensions_mut().insert(AuthContext::node());
            return next.run(req).await;
        }
        tracing::warn!(path = %path, method = %method, "node secret rejected for route");
        return response::forbidden("Node secret valid only for GET /events, GET /blocks, GET /keys/internal/export and POST /events/gossip, /chat/gossip, /keys/internal/*");
    }

    let key_data = KeyStore::validate_key(&state.db, &api_key).await;

    if let Some(k) = key_data {
        let citizen_name = k.citizen_name;
        let citizen_id = k.citizen_id;
        let key_role = k.role;
        let (role, citizen_status) =
            resolve_role_and_status_for_citizen(&state.db, &citizen_name, key_role, &api_key).await;
        if citizen_status == "applicant" || role.is_candidate() {
            tracing::warn!(
                path = %path,
                citizen = %citizen_name,
                ?role,
                "Candidate/applicant API access denied"
            );
            return response::forbidden(
                "Доступ запрещён: роль Candidate — только публичные данные. API-ключ и доступ к системе недоступны.",
            );
        }
        if citizen_status == "pending" && !allows_pending_citizen(&path) {
            tracing::warn!(path = %path, citizen = %citizen_name, "pending citizen API access denied");
            return response::forbidden(
                "Доступ запрещён: паспорт ещё не выдан (статус pending). Разрешены только операции с /passport.",
            );
        }
        tracing::debug!(path = %path, citizen = %citizen_name, ?role, "API key accepted");
        let agent_permissions = if role.is_agent() {
            list_agent_permissions(&state.db, &citizen_id)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let auth = AuthContext::from_api_key(citizen_name, role)
            .with_citizen_id(citizen_id)
            .with_agent_permissions(agent_permissions);
        if let Some(response) = enforce_path_rbac(&path, &auth) {
            return response;
        }
        req.extensions_mut().insert(auth);
        return next.run(req).await;
    }

    tracing::warn!(path = %path, key = %mask_key(&api_key), "API key not found");
    response::unauthorized("Invalid or missing API key")
}

async fn resolve_role_and_status_for_citizen(
    pool: &sqlx::PgPool,
    citizen_name: &str,
    key_role: Role,
    api_key: &str,
) -> (Role, String) {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT role, status FROM citizens WHERE name = $1",
    )
    .bind(citizen_name)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);

    match row {
        Some((role_str, status)) => {
            let role = Role::from_str(&role_str).unwrap_or_else(|| key_role.clone());
            if role != key_role {
                KeyStore::sync_citizen_key_roles(pool, citizen_name, role.clone()).await;
                tracing::info!(
                    citizen = %citizen_name,
                    key = %mask_key(api_key),
                    api_keys_role = %key_role.as_str(),
                    citizens_role = %role.as_str(),
                    "api_keys.role synced from citizens on auth"
                );
            }
            (role, status)
        }
        None => (key_role, "active".to_string()),
    }
}
