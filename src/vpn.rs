//! Citizen VPN via Shadowsocks (ssserver). SSH tunnel key issuance removed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Extension, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::auth::{AuthContext, Role};
use crate::response::{self, ApiResponse};
use crate::AppState;

const DEFAULT_TTL_HOURS: i64 = 24;
const MIN_TTL_HOURS: i64 = 1;
const MAX_TTL_HOURS: i64 = 168;
const DEFAULT_SS_PORT: u16 = 8388;
const DEFAULT_SS_METHOD: &str = "chacha20-ietf-poly1305";
const ACL_PROFILE: &str = "citizen_youtube_telegram";
const SS_INACTIVE_RETENTION_DAYS: i64 = 7;
pub const VPN_CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct SsCredentialInfo {
    pub id: String,
    pub citizen_id: String,
    pub method: String,
    pub port: i32,
    pub acl_profile: String,
    pub expires_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub is_active: Option<bool>,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SsCredentialResponse {
    pub session_id: String,
    pub server: String,
    pub host: String,
    pub port: u16,
    pub method: String,
    pub password: String,
    pub acl_profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_acl: Option<serde_json::Value>,
    pub ttl_sec: i64,
    pub transport: &'static str,
    pub expires_at: DateTime<Utc>,
    pub citizen_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SsStatus {
    pub is_active: bool,
    pub session_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used: Option<DateTime<Utc>>,
    pub transport: &'static str,
    pub server: Option<String>,
    pub port: Option<u16>,
    pub method: Option<String>,
    pub acl_profile: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct IssueSsCredentialRequest {
    #[serde(default)]
    pub citizen_id: Option<String>,
    #[serde(default)]
    pub ttl_hours: Option<i64>,
    #[serde(default)]
    pub services: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RevokeSsCredentialRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub citizen_id: Option<String>,
}

fn default_ttl_hours() -> i64 {
    std::env::var("SS_TTL_HOURS")
        .ok()
        .or_else(|| std::env::var("VPN_KEY_TTL_HOURS").ok())
        .and_then(|v| v.trim().parse().ok())
        .filter(|h| *h >= MIN_TTL_HOURS && *h <= MAX_TTL_HOURS)
        .unwrap_or(DEFAULT_TTL_HOURS)
}

fn clamp_ttl_hours(ttl: i64) -> Result<i64, String> {
    if ttl < MIN_TTL_HOURS || ttl > MAX_TTL_HOURS {
        return Err(format!(
            "ttl_hours must be between {MIN_TTL_HOURS} and {MAX_TTL_HOURS}"
        ));
    }
    Ok(ttl)
}

fn ss_enabled() -> bool {
    match std::env::var("SS_ENABLED") {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            !(t == "0" || t == "false" || t == "no" || t == "off")
        }
        Err(_) => true,
    }
}

fn ss_port() -> u16 {
    std::env::var("SS_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_SS_PORT)
}

fn ss_method() -> String {
    std::env::var("SS_METHOD")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SS_METHOD.to_string())
}

fn ss_public_host() -> String {
    std::env::var("SS_PUBLIC_HOST")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("QUAZAR_PUBLIC_HOST")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "quazar.website".to_string())
}

fn ss_dir() -> PathBuf {
    std::env::var("SS_CONFIG_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/quazar/ss"))
}

fn require_citizen(auth: &AuthContext) -> Option<axum::response::Response> {
    if auth.is_node {
        return Some(response::forbidden(
            "Node credentials cannot manage VPN sessions",
        ));
    }
    if !auth.has_permission(&Role::Citizen) {
        return Some(response::forbidden(auth.insufficient_permissions_message()));
    }
    None
}

fn require_aiya_admin(auth: &AuthContext) -> Option<axum::response::Response> {
    if auth.is_node {
        return Some(response::forbidden(
            "Node credentials cannot list VPN sessions",
        ));
    }
    if !auth.is_admin() {
        return Some(response::forbidden(
            "Only Aiya and Founder can list active VPN sessions",
        ));
    }
    None
}

fn role_display_ru(role: &Role) -> &'static str {
    match role {
        Role::Citizen => "Гражданин",
        Role::Guardian => "Страж",
        Role::Judge => "Судья",
        Role::Mayor => "Мэр",
        Role::Aiya => "Айя",
        Role::Founder => "Основатель",
        Role::Agent => "Агент",
        Role::Candidate => "Кандидат",
    }
}

fn access_for_role(role: &Role) -> Vec<&'static str> {
    if matches!(
        role,
        Role::Guardian | Role::Judge | Role::Mayor | Role::Aiya | Role::Founder
    ) {
        vec!["global_lots", "local_lots", "verify_trades"]
    } else {
        vec!["global_lots"]
    }
}

fn role_to_apply(raw: &str) -> Role {
    match Role::from_str(raw) {
        Some(Role::Candidate) | Some(Role::Agent) | None => Role::Citizen,
        Some(r) => r,
    }
}

fn is_accepted_status_role(role: &Role) -> bool {
    matches!(
        role,
        Role::Citizen
            | Role::Guardian
            | Role::Judge
            | Role::Mayor
            | Role::Aiya
            | Role::Founder
    )
}

fn role_from_appointed_target(target: &str) -> Option<Role> {
    match Role::from_str(target.trim()) {
        Some(r @ (Role::Citizen | Role::Guardian | Role::Judge | Role::Mayor | Role::Aiya)) => {
            Some(r)
        }
        _ => None,
    }
}

fn prefer_vpn_role(current_raw: &str, appointed_target: Option<&str>) -> Role {
    if let Some(current) = Role::from_str(current_raw) {
        if is_accepted_status_role(&current) && current != Role::Citizen {
            return current;
        }
        if current == Role::Citizen {
            if let Some(appointed) = appointed_target.and_then(role_from_appointed_target) {
                return appointed;
            }
            return Role::Citizen;
        }
    }
    if let Some(appointed) = appointed_target.and_then(role_from_appointed_target) {
        return appointed;
    }
    role_to_apply(current_raw)
}

async fn latest_appointed_target(pool: &PgPool, citizen_id: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT target_role FROM candidacies
         WHERE citizen_id = $1 AND status = 'Appointed'
         ORDER BY appointed_at DESC NULLS LAST, created_at DESC
         LIMIT 1",
    )
    .bind(citizen_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

async fn resolve_vpn_role(pool: &PgPool, citizen_id: &str, role_raw: &str) -> Role {
    let appointed = latest_appointed_target(pool, citizen_id).await;
    prefer_vpn_role(role_raw, appointed.as_deref())
}

fn generate_ss_password() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..24)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

/// Scheme: one listen port + one shared server password among currently active sessions.
/// Classic AEAD (chacha20-ietf-poly1305 / aes-256-gcm) — one password per listener.
/// On revoke of any session we rotate the password for remaining actives so the revoked
/// client is cut off; remaining clients should re-request `/api/vpn/ss/credential`.
pub async fn sync_ssserver_config(pool: &PgPool) {
    if !ss_enabled() {
        return;
    }
    let dir = ss_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, path = %dir.display(), "SS config dir create failed");
        return;
    }

    let acl_src = Path::new("/opt/quazar/deploy/ss/citizen.acl");
    let acl_dst = dir.join("citizen.acl");
    if acl_src.exists() {
        let src = std::fs::read(acl_src).ok();
        let dst = std::fs::read(&acl_dst).ok();
        if src != dst {
            if let Err(e) = std::fs::copy(acl_src, &acl_dst) {
                tracing::warn!(error = %e, "SS ACL copy failed");
            }
        }
    }

    let config_path = dir.join("config.json");
    let active: Result<Vec<(String,)>, sqlx::Error> = sqlx::query_as(
        r#"
        SELECT password
        FROM vpn_ss_credentials
        WHERE is_active = TRUE AND expires_at > NOW()
        ORDER BY created_at ASC
        LIMIT 1
        "#,
    )
    .fetch_all(pool)
    .await;

    let password = match active {
        Ok(rows) if !rows.is_empty() => rows[0].0.clone(),
        Ok(_) => read_existing_ss_password(&config_path).unwrap_or_else(generate_ss_password),
        Err(e) => {
            tracing::warn!(error = %e, "SS config: failed to load active passwords");
            return;
        }
    };

    let method = ss_method();
    let port = ss_port();
    let acl_path = acl_dst.to_string_lossy().to_string();
    // ipv6_first=false: this VPS has no global IPv6 route; googlevideo AAAA
    // otherwise fails with "No route to host" before IPv4 fallback.
    let config = serde_json::json!({
        "server": "0.0.0.0",
        "server_port": port,
        "method": method,
        "password": password,
        "mode": "tcp_and_udp",
        "timeout": 300,
        "ipv6_first": false,
        "acl": acl_path,
    });

    let tmp = dir.join("config.json.tmp");
    let body = match serde_json::to_vec_pretty(&config) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "SS config serialize failed");
            return;
        }
    };
    // systemd path unit restarts ssserver on any inode/mtime change. Cleanup
    // used to rewrite identical JSON every 5 minutes and kill YouTube tunnels.
    if let Ok(existing) = std::fs::read(&config_path) {
        if existing == body {
            tracing::debug!(
                path = %config_path.display(),
                "SS ssserver config unchanged"
            );
            return;
        }
    }
    if let Err(e) = std::fs::write(&tmp, &body) {
        tracing::warn!(error = %e, "SS config temp write failed");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = std::fs::rename(&tmp, &config_path) {
        tracing::warn!(error = %e, "SS config replace failed");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    tracing::info!(
        path = %config_path.display(),
        port = port,
        "SS ssserver config rewritten"
    );
}

fn read_existing_ss_password(config_path: &Path) -> Option<String> {
    let raw = std::fs::read(config_path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("password")?.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

async fn current_shared_password(pool: &PgPool) -> Result<Option<String>, String> {
    sqlx::query_scalar(
        r#"
        SELECT password
        FROM vpn_ss_credentials
        WHERE is_active = TRUE AND expires_at > NOW()
        ORDER BY created_at ASC
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("Failed to load shared SS password: {e}"))
}

async fn rotate_remaining_passwords(pool: &PgPool, new_password: &str) -> Result<(), String> {
    sqlx::query(
        r#"
        UPDATE vpn_ss_credentials
        SET password = $1
        WHERE is_active = TRUE AND expires_at > NOW()
        "#,
    )
    .bind(new_password)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to rotate SS passwords: {e}"))?;
    Ok(())
}

pub async fn issue_ss_credential(
    pool: &PgPool,
    citizen_id: &str,
    ttl_hours: i64,
) -> Result<SsCredentialResponse, String> {
    let ttl_hours = clamp_ttl_hours(ttl_hours)?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let expires_at = now + ChronoDuration::hours(ttl_hours);
    let method = ss_method();
    let port = ss_port();
    let password = match current_shared_password(pool).await? {
        Some(p) => p,
        None => generate_ss_password(),
    };

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("Failed to start SS credential transaction: {e}"))?;

    sqlx::query(
        r#"
        UPDATE vpn_ss_credentials
        SET is_active = FALSE, password = ''
        WHERE citizen_id = $1 AND is_active = TRUE
        "#,
    )
    .bind(citizen_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("Failed to deactivate previous SS credentials: {e}"))?;

    sqlx::query(
        r#"
        INSERT INTO vpn_ss_credentials
            (id, citizen_id, password, method, port, acl_profile, expires_at, last_used, is_active, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, TRUE, $8)
        "#,
    )
    .bind(&id)
    .bind(citizen_id)
    .bind(&password)
    .bind(&method)
    .bind(port as i32)
    .bind(ACL_PROFILE)
    .bind(expires_at)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("Failed to store SS credential: {e}"))?;

    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit SS credential: {e}"))?;

    sync_ssserver_config(pool).await;

    let host = ss_public_host();
    Ok(SsCredentialResponse {
        session_id: id,
        server: host.clone(),
        host,
        port,
        method,
        password,
        acl_profile: ACL_PROFILE.to_string(),
        suggested_acl: None,
        ttl_sec: ttl_hours * 3600,
        transport: "shadowsocks",
        expires_at,
        citizen_id: citizen_id.to_string(),
    })
}

/// Revoke by session_id and/or all sessions for citizen_id. Idempotent.
pub async fn revoke_ss_credentials(
    pool: &PgPool,
    citizen_id: Option<&str>,
    session_id: Option<&str>,
) -> Result<u64, String> {
    let result = if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        if let Some(cid) = citizen_id.filter(|s| !s.is_empty()) {
            sqlx::query(
                r#"
                UPDATE vpn_ss_credentials
                SET is_active = FALSE, password = ''
                WHERE id = $1 AND citizen_id = $2 AND is_active = TRUE
                "#,
            )
            .bind(sid)
            .bind(cid)
            .execute(pool)
            .await
        } else {
            sqlx::query(
                r#"
                UPDATE vpn_ss_credentials
                SET is_active = FALSE, password = ''
                WHERE id = $1 AND is_active = TRUE
                "#,
            )
            .bind(sid)
            .execute(pool)
            .await
        }
    } else if let Some(cid) = citizen_id.filter(|s| !s.is_empty()) {
        sqlx::query(
            r#"
            UPDATE vpn_ss_credentials
            SET is_active = FALSE, password = ''
            WHERE citizen_id = $1 AND is_active = TRUE
            "#,
        )
        .bind(cid)
        .execute(pool)
        .await
    } else {
        return Err("session_id or citizen_id is required".into());
    }
    .map_err(|e| format!("Failed to revoke SS credential: {e}"))?;

    let n = result.rows_affected();
    if n > 0 {
        let new_password = generate_ss_password();
        rotate_remaining_passwords(pool, &new_password).await?;
        sync_ssserver_config(pool).await;
    }
    Ok(n)
}

pub async fn revoke_ss_for_citizen(pool: &PgPool, citizen_id: &str) -> Result<u64, String> {
    revoke_ss_credentials(pool, Some(citizen_id), None).await
}

pub async fn get_ss_status(pool: &PgPool, citizen_id: &str) -> Result<SsStatus, String> {
    let row: Option<(String, DateTime<Utc>, Option<DateTime<Utc>>, String, i32, String)> =
        sqlx::query_as(
            r#"
            SELECT id, expires_at, last_used, method, port, acl_profile
            FROM vpn_ss_credentials
            WHERE citizen_id = $1
              AND is_active = TRUE
              AND expires_at > NOW()
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(citizen_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("Failed to load SS status: {e}"))?;

    Ok(match row {
        Some((id, expires_at, last_used, method, port, acl_profile)) => SsStatus {
            is_active: true,
            session_id: Some(id),
            expires_at: Some(expires_at),
            last_used,
            transport: "shadowsocks",
            server: Some(ss_public_host()),
            port: Some(port as u16),
            method: Some(method),
            acl_profile: Some(acl_profile),
        },
        None => SsStatus {
            is_active: false,
            session_id: None,
            expires_at: None,
            last_used: None,
            transport: "shadowsocks",
            server: None,
            port: None,
            method: None,
            acl_profile: None,
        },
    })
}

pub async fn list_active_ss_credentials(pool: &PgPool) -> Result<Vec<SsCredentialInfo>, String> {
    sqlx::query_as::<_, SsCredentialInfo>(
        r#"
        SELECT id, citizen_id, method, port, acl_profile, expires_at, last_used, is_active, created_at
        FROM vpn_ss_credentials
        WHERE is_active = TRUE AND expires_at > NOW()
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to list SS credentials: {e}"))
}

pub async fn cleanup_expired_ss_credentials(pool: &PgPool) -> Result<u64, String> {
    let result = sqlx::query(
        r#"
        UPDATE vpn_ss_credentials
        SET is_active = FALSE, password = ''
        WHERE is_active = TRUE
          AND (
            expires_at < NOW()
            OR last_used < NOW() - INTERVAL '24 hours'
          )
        "#,
    )
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to cleanup SS credentials: {e}"))?;

    let deactivated = result.rows_affected();
    if deactivated > 0 {
        let new_password = generate_ss_password();
        let _ = rotate_remaining_passwords(pool, &new_password).await;
        sync_ssserver_config(pool).await;
    }

    let _ = sqlx::query(
        r#"
        DELETE FROM vpn_ss_credentials
        WHERE COALESCE(is_active, FALSE) = FALSE
          AND created_at < NOW() - make_interval(days => $1::int)
        "#,
    )
    .bind(SS_INACTIVE_RETENTION_DAYS as i32)
    .execute(pool)
    .await;

    Ok(deactivated)
}

/// Alias for background cleanup loop.
pub async fn cleanup_expired_keys(pool: &PgPool) -> Result<u64, String> {
    cleanup_expired_ss_credentials(pool).await
}

async fn ss_session_valid(pool: &PgPool, citizen_id: &str, session_id: &str) -> bool {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM vpn_ss_credentials
            WHERE id = $1 AND citizen_id = $2 AND is_active = TRUE AND expires_at > NOW()
        )
        "#,
    )
    .bind(session_id)
    .bind(citizen_id)
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}

/// POST /api/vpn/ss/credential — Citizen+
pub async fn issue_ss_credential_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    body: Option<Json<IssueSsCredentialRequest>>,
) -> impl IntoResponse {
    if !ss_enabled() {
        return response::service_unavailable("Shadowsocks VPN is disabled (SS_ENABLED=false)");
    }
    if let Some(resp) = require_citizen(&auth) {
        return resp;
    }

    if let Err(e) = cleanup_expired_ss_credentials(&state.db).await {
        tracing::warn!(error = %e, "SS on-demand cleanup before issue failed");
    }

    let req = body.map(|Json(b)| b).unwrap_or_default();
    let auth_citizen = auth.resolve_account_id(&state.db).await;
    let citizen_id = req
        .citizen_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(auth_citizen.as_str());
    if citizen_id != auth_citizen && !auth.is_admin() {
        return response::forbidden("Cannot issue SS credentials for another citizen");
    }

    let ttl_hours = req.ttl_hours.unwrap_or_else(default_ttl_hours);
    let _services = req.services;

    match issue_ss_credential(&state.db, citizen_id, ttl_hours).await {
        Ok(cred) => {
            tracing::info!(
                citizen_id = %cred.citizen_id,
                session_id = %cred.session_id,
                expires_at = %cred.expires_at,
                "SS credential issued"
            );
            Json(ApiResponse::success(cred)).into_response()
        }
        Err(e) => {
            if e.contains("ttl_hours") {
                response::bad_request(e)
            } else {
                tracing::error!(error = %e, "SS credential issue failed");
                response::internal_error("Failed to issue SS credential")
            }
        }
    }
}

/// POST /api/vpn/ss/revoke — Citizen+
pub async fn revoke_ss_credential_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    body: Option<Json<RevokeSsCredentialRequest>>,
) -> impl IntoResponse {
    if let Some(resp) = require_citizen(&auth) {
        return resp;
    }

    let req = body.map(|Json(b)| b).unwrap_or_default();
    let auth_citizen = auth.resolve_account_id(&state.db).await;
    let citizen_id = req
        .citizen_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(auth_citizen.as_str());
    if citizen_id != auth_citizen && !auth.is_admin() {
        return response::forbidden("Cannot revoke SS credentials for another citizen");
    }

    let session_id = req.session_id.as_deref().map(str::trim).filter(|s| !s.is_empty());

    match revoke_ss_credentials(&state.db, Some(citizen_id), session_id).await {
        Ok(_) => Json(ApiResponse::success(serde_json::json!({
            "ok": true,
            "revoked": true,
            "transport": "shadowsocks",
        })))
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "SS revoke failed");
            response::internal_error("Failed to revoke SS credential")
        }
    }
}

/// GET /api/vpn/ss/status — Citizen+
pub async fn ss_status_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(resp) = require_citizen(&auth) {
        return resp;
    }
    let citizen_id = auth.resolve_account_id(&state.db).await;
    match get_ss_status(&state.db, &citizen_id).await {
        Ok(status) => {
            let now = Utc::now();
            let seconds_remaining = status
                .expires_at
                .map(|exp| (exp - now).num_seconds().max(0))
                .unwrap_or(0);
            Json(ApiResponse::success(serde_json::json!({
                "is_active": status.is_active,
                "session_id": status.session_id,
                "expires_at": status.expires_at,
                "last_used": status.last_used,
                "seconds_remaining": seconds_remaining,
                "transport": status.transport,
                "server": status.server,
                "host": status.server,
                "port": status.port,
                "method": status.method,
                "acl_profile": status.acl_profile,
            })))
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "SS status failed");
            response::internal_error("Failed to load SS status")
        }
    }
}

/// GET /api/vpn/status — SS status (SSH tunnel removed).
pub async fn vpn_status_handler(
    auth: Extension<AuthContext>,
    state: State<Arc<AppState>>,
) -> impl IntoResponse {
    ss_status_handler(auth, state).await
}

/// POST /api/vpn/revoke — revoke SS credentials (SSH tunnel removed).
pub async fn revoke_vpn_key_handler(
    auth: Extension<AuthContext>,
    state: State<Arc<AppState>>,
    body: Option<Json<RevokeSsCredentialRequest>>,
) -> impl IntoResponse {
    revoke_ss_credential_handler(auth, state, body).await
}

/// GET /api/vpn/keys — admin list of active SS sessions (no passwords).
pub async fn list_vpn_keys_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(resp) = require_aiya_admin(&auth) {
        return resp;
    }
    match list_active_ss_credentials(&state.db).await {
        Ok(keys) => Json(ApiResponse::success(serde_json::json!({
            "keys": keys,
            "transport": "shadowsocks",
        })))
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "SS list failed");
            response::internal_error("Failed to list SS credentials")
        }
    }
}

/// POST /api/vpn/key — removed (was SSH tunnel). Use /api/vpn/ss/credential.
pub async fn generate_vpn_key_handler() -> impl IntoResponse {
    response::gone(
        "SSH tunnel VPN removed. Use POST /api/vpn/ss/credential (Shadowsocks).",
    )
}

#[derive(Debug, Deserialize)]
pub struct ActivateVpnBody {
    pub vpn_session_id: String,
}

/// POST /api/citizen/activate_vpn — vpn_session_id = SS session_id.
pub async fn activate_vpn_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<ActivateVpnBody>,
) -> impl IntoResponse {
    if let Some(resp) = require_citizen(&auth) {
        return resp;
    }
    let session = body.vpn_session_id.trim();
    if session.is_empty() {
        return response::bad_request("vpn_session_id is required");
    }

    let citizen_id = auth.resolve_account_id(&state.db).await;
    let row: Option<(String, String, bool)> = match sqlx::query_as(
        "SELECT status, role, passport_issued FROM citizens WHERE id = $1",
    )
    .bind(&citizen_id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "activate_vpn citizen lookup failed");
            return response::internal_error("Database error");
        }
    };
    let Some((status, role_raw, passport_issued)) = row else {
        return response::bad_request("Citizen not found");
    };
    if !passport_issued {
        return response::bad_request("Паспорт ещё не выдан — ВПН-активация недоступна");
    }
    if matches!(status.as_str(), "suspended" | "revoked" | "applicant") {
        return response::bad_request(format!("Статус {status} не допускает активацию ВПН"));
    }

    let applied = resolve_vpn_role(&state.db, &citizen_id, &role_raw).await;
    let access = access_for_role(&applied);
    let role_ru = role_display_ru(&applied);
    let role_already_ok = role_raw == applied.as_str();
    let was_active = status == "active";

    if was_active && role_already_ok {
        let _ = sqlx::query(
            "UPDATE vpn_ss_credentials SET last_used = NOW()
             WHERE citizen_id = $1 AND is_active = TRUE AND expires_at > NOW()
               AND ($2 = '' OR id = $2)",
        )
        .bind(&citizen_id)
        .bind(session)
        .execute(&state.db)
        .await;
        return Json(ApiResponse::success(serde_json::json!({
            "status": "active",
            "role": role_ru,
            "role_code": applied.as_str(),
            "access": access,
            "idempotent": true,
            "vpn_session_id": session,
            "transport": "shadowsocks",
        })))
        .into_response();
    }

    if !ss_session_valid(&state.db, &citizen_id, session).await {
        return response::bad_request(
            "Недействительная ВПН-сессия: получите credentials (POST /api/vpn/ss/credential) и передайте session_id как vpn_session_id",
        );
    }

    if let Err(e) = sqlx::query(
        "UPDATE citizens SET status = 'active', role = $1 WHERE id = $2",
    )
    .bind(applied.as_str())
    .bind(&citizen_id)
    .execute(&state.db)
    .await
    {
        tracing::error!(error = %e, "activate_vpn update failed");
        return response::internal_error("Failed to activate citizen");
    }

    let _ = sqlx::query(
        "UPDATE api_keys SET role = $1 WHERE citizen_id = $2 AND is_active = TRUE",
    )
    .bind(applied.as_str())
    .bind(&citizen_id)
    .execute(&state.db)
    .await;

    let _ = sqlx::query(
        "UPDATE vpn_ss_credentials SET last_used = NOW() WHERE id = $1 AND citizen_id = $2",
    )
    .bind(session)
    .bind(&citizen_id)
    .execute(&state.db)
    .await;

    let now_ts = Utc::now().timestamp();
    let _ = sqlx::query(
        "INSERT INTO citizen_status (citizen_id, status, last_seen) VALUES ($1, 'online', $2)
         ON CONFLICT (citizen_id) DO UPDATE SET status = 'online', last_seen = EXCLUDED.last_seen",
    )
    .bind(&citizen_id)
    .bind(now_ts)
    .execute(&state.db)
    .await;

    tracing::info!(
        citizen_id = %citizen_id,
        role = %applied.as_str(),
        previous_role = %role_raw,
        vpn_session_id = %session,
        healed = was_active && !role_already_ok,
        "VPN activated: citizen active + role applied (shadowsocks)"
    );

    Json(ApiResponse::success(serde_json::json!({
        "status": "active",
        "role": role_ru,
        "role_code": applied.as_str(),
        "access": access,
        "idempotent": false,
        "vpn_session_id": session,
        "transport": "shadowsocks",
        "message": "ВПН включён — роль принята, доступ к экономике открыт",
    })))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_ttl_rejects_out_of_range() {
        assert!(clamp_ttl_hours(0).is_err());
        assert!(clamp_ttl_hours(169).is_err());
        assert_eq!(clamp_ttl_hours(24).unwrap(), 24);
    }

    #[test]
    fn password_has_expected_len() {
        let p = generate_ss_password();
        assert_eq!(p.len(), 24);
    }

    #[test]
    fn read_existing_ss_password_parses_json() {
        let dir = std::env::temp_dir().join(format!("ss-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"password":"abc123"}"#).unwrap();
        assert_eq!(read_existing_ss_password(&path).as_deref(), Some("abc123"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefer_vpn_role_uses_appointed_when_citizen() {
        assert_eq!(prefer_vpn_role("Citizen", Some("Aiya")), Role::Aiya);
        assert_eq!(
            prefer_vpn_role("Candidate", Some("Guardian")),
            Role::Guardian
        );
        assert_eq!(prefer_vpn_role("Citizen", None), Role::Citizen);
        assert_eq!(prefer_vpn_role("Candidate", None), Role::Citizen);
    }

    #[test]
    fn prefer_vpn_role_keeps_existing_governance() {
        assert_eq!(prefer_vpn_role("Aiya", Some("Guardian")), Role::Aiya);
        assert_eq!(prefer_vpn_role("Guardian", None), Role::Guardian);
        assert_eq!(prefer_vpn_role("Founder", Some("Aiya")), Role::Founder);
    }
}
