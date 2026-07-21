use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Extension, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use ssh_key::{Algorithm, LineEnding, PrivateKey};
use uuid::Uuid;

use crate::auth::{AuthContext, Role};
use crate::response::{self, ApiResponse};
use crate::tunnel;
use crate::AppState;

const DEFAULT_TTL_HOURS: i64 = 24;
const MIN_TTL_HOURS: i64 = 1;
const MAX_TTL_HOURS: i64 = 168; // 7 days
const IDLE_REVOKE_HOURS: i64 = 24;
pub const VPN_CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Admin list row — never includes `private_key`.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct VpnKeyInfo {
    pub id: String,
    pub citizen_id: String,
    pub public_key: String,
    pub expires_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub is_active: Option<bool>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Issued key returned once from generate (includes private material).
#[derive(Debug, Clone, Serialize)]
pub struct VpnKeyResponse {
    pub id: String,
    pub citizen_id: String,
    pub private_key: String,
    pub public_key: String,
    pub expires_at: DateTime<Utc>,
}

/// Citizen-facing active-key snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct VpnStatus {
    pub is_active: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, Default)]
pub struct GenerateVpnKeyRequest {
    #[serde(default)]
    pub ttl_hours: Option<i64>,
}

fn default_ttl_hours() -> i64 {
    std::env::var("VPN_KEY_TTL_HOURS")
        .ok()
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

/// Constant-time-ish equality for stored plaintext private keys (do not log either side).
fn secure_bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Pure expiry check used by cleanup / status (unit-tested).
pub fn is_vpn_key_expired(expires_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    expires_at <= now
}

/// Idle revoke: last_used older than IDLE_REVOKE_HOURS (unused keys keep expires_at only).
pub fn is_vpn_key_idle(
    last_used: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    idle_hours: i64,
) -> bool {
    match last_used {
        Some(ts) => ts <= now - ChronoDuration::hours(idle_hours),
        None => false,
    }
}

/// Simulate rotate: previous active keys become inactive when a new key is issued.
fn deactivate_on_new_key_ids(active_ids: &[String], new_id: &str) -> Vec<String> {
    let mut out: Vec<String> = active_ids
        .iter()
        .filter(|id| id.as_str() != new_id)
        .cloned()
        .collect();
    // After rotate, only the new id remains active.
    out.clear();
    out.push(new_id.to_string());
    out
}

fn require_citizen(auth: &AuthContext) -> Option<axum::response::Response> {
    if auth.is_node {
        return Some(response::forbidden(
            "Node credentials cannot manage VPN keys",
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
            "Node credentials cannot list VPN keys",
        ));
    }
    if !auth.is_admin() {
        return Some(response::forbidden(
            "Only Aiya and Founder can list active VPN keys",
        ));
    }
    None
}

fn generate_openssh_keypair(comment: &str) -> Result<(String, String), String> {
    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .map_err(|e| format!("Failed to generate Ed25519 key: {e}"))?;
    key.set_comment(comment);
    let private_openssh = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| format!("Failed to encode OpenSSH private key: {e}"))?;
    let public_openssh = key
        .public_key()
        .to_openssh()
        .map_err(|e| format!("Failed to encode OpenSSH public key: {e}"))?;
    Ok((private_openssh.to_string(), public_openssh))
}

const VPN_INACTIVE_RETENTION_DAYS: i64 = 7;

fn authorized_keys_path() -> Option<std::path::PathBuf> {
    let path = std::env::var("VPN_AUTHORIZED_KEYS_PATH").ok()?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    Some(Path::new(path).to_path_buf())
}

/// Rewrite `authorized_keys` from currently active, non-expired public keys only.
pub async fn rewrite_authorized_keys(pool: &PgPool) {
    let Some(path) = authorized_keys_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %e, path = %parent.display(), "VPN authorized_keys parent dir");
            return;
        }
    }

    let keys: Result<Vec<(String,)>, sqlx::Error> = sqlx::query_as(
        r#"
        SELECT public_key
        FROM vpn_keys
        WHERE is_active = TRUE
          AND expires_at > NOW()
        ORDER BY created_at ASC
        "#,
    )
    .fetch_all(pool)
    .await;

    let keys = match keys {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to load VPN public keys for authorized_keys rewrite");
            return;
        }
    };

    let mut content = String::new();
    for (public_key,) in &keys {
        let line = public_key.trim();
        if line.is_empty() {
            continue;
        }
        content.push_str(line);
        content.push('\n');
    }

    let tmp = path.with_extension("authorized_keys.tmp");
    if let Err(e) = std::fs::write(&tmp, content.as_bytes()) {
        tracing::warn!(
            error = %e,
            path = %tmp.display(),
            "Failed to write VPN authorized_keys temp file"
        );
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            "Failed to replace VPN authorized_keys"
        );
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    // sshd opens AuthorizedKeysFile as quazar-tunnel — root:root 0600 => Permission denied.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        let owner = std::env::var("VPN_AUTHORIZED_KEYS_OWNER")
            .unwrap_or_else(|_| "999:987".to_string());
        let status = std::process::Command::new("chown")
            .arg(&owner)
            .arg(&path)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => tracing::warn!(code = ?s.code(), owner = %owner, "chown authorized_keys failed"),
            Err(e) => tracing::warn!(error = %e, owner = %owner, "chown authorized_keys failed"),
        }
    }
    tracing::info!(
        path = %path.display(),
        active_keys = keys.len(),
        "VPN authorized_keys rewritten from active keys"
    );
}

/// Delete inactive VPN rows older than retention (private material already stripped).
pub async fn gc_inactive_vpn_keys(pool: &PgPool) -> Result<u64, String> {
    let result = sqlx::query(
        r#"
        DELETE FROM vpn_keys
        WHERE COALESCE(is_active, FALSE) = FALSE
          AND created_at < NOW() - make_interval(days => $1::int)
        "#,
    )
    .bind(VPN_INACTIVE_RETENTION_DAYS as i32)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to GC inactive VPN keys: {e}"))?;

    Ok(result.rows_affected())
}

/// Generate Ed25519 OpenSSH keypair, deactivate prior active keys, persist, return private once.
pub async fn generate_vpn_key(
    pool: &PgPool,
    citizen_id: &str,
    ttl_hours: i64,
) -> Result<VpnKeyResponse, String> {
    let ttl_hours = clamp_ttl_hours(ttl_hours)?;
    let id = Uuid::new_v4().to_string();
    let comment = format!("quazar-vpn-{id}");
    let (private_key, public_key) = generate_openssh_keypair(&comment)?;
    let now = Utc::now();
    let expires_at = now + ChronoDuration::hours(ttl_hours);

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("Failed to start VPN key transaction: {e}"))?;

    sqlx::query(
        r#"
        UPDATE vpn_keys
        SET is_active = FALSE, private_key = ''
        WHERE citizen_id = $1 AND is_active = TRUE
        "#,
    )
    .bind(citizen_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("Failed to deactivate previous VPN keys: {e}"))?;

    sqlx::query(
        r#"
        INSERT INTO vpn_keys (id, citizen_id, private_key, public_key, expires_at, last_used, is_active, created_at)
        VALUES ($1, $2, $3, $4, $5, NULL, TRUE, $6)
        "#,
    )
    .bind(&id)
    .bind(citizen_id)
    .bind(&private_key)
    .bind(&public_key)
    .bind(expires_at)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("Failed to store VPN key: {e}"))?;

    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit VPN key: {e}"))?;

    rewrite_authorized_keys(pool).await;

    Ok(VpnKeyResponse {
        id,
        citizen_id: citizen_id.to_string(),
        private_key,
        public_key,
        expires_at,
    })
}

/// Check that a private key exists, is active, and not expired for this citizen.
/// Compares stored plaintext with a length-checked XOR fold (never log the key).
/// Updates `last_used` on success.
pub async fn validate_vpn_key(
    pool: &PgPool,
    citizen_id: &str,
    private_key: &str,
) -> Result<bool, String> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT id, private_key
        FROM vpn_keys
        WHERE citizen_id = $1
          AND is_active = TRUE
          AND expires_at > NOW()
        "#,
    )
    .bind(citizen_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to validate VPN key: {e}"))?;

    let presented = private_key.as_bytes();
    for (id, stored) in rows {
        if secure_bytes_eq(stored.as_bytes(), presented) {
            let now = Utc::now();
            sqlx::query(
                r#"
                UPDATE vpn_keys
                SET last_used = $2
                WHERE id = $1
                "#,
            )
            .bind(&id)
            .bind(now)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to update VPN key last_used: {e}"))?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Active key status for a citizen: `is_active`, `expires_at`, `last_used`.
pub async fn get_vpn_status(pool: &PgPool, citizen_id: &str) -> Result<VpnStatus, String> {
    let row: Option<(DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT expires_at, last_used
        FROM vpn_keys
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
    .map_err(|e| format!("Failed to load VPN status: {e}"))?;

    Ok(match row {
        Some((expires_at, last_used)) => VpnStatus {
            is_active: true,
            expires_at: Some(expires_at),
            last_used,
        },
        None => VpnStatus {
            is_active: false,
            expires_at: None,
            last_used: None,
        },
    })
}

/// Revoke all active keys for a citizen (`is_active = false`). Returns whether any row changed.
pub async fn revoke_vpn_key(pool: &PgPool, citizen_id: &str) -> Result<bool, String> {
    let result = sqlx::query(
        r#"
        UPDATE vpn_keys
        SET is_active = FALSE, private_key = ''
        WHERE citizen_id = $1 AND is_active = TRUE
        "#,
    )
    .bind(citizen_id)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to revoke VPN key: {e}"))?;

    let changed = result.rows_affected() > 0;
    if changed {
        rewrite_authorized_keys(pool).await;
    }
    Ok(changed)
}

/// Deactivate expired or idle keys:
/// `expires_at < NOW()` OR `last_used < NOW() - 24h`.
pub async fn cleanup_expired_keys(pool: &PgPool) -> Result<u64, String> {
    let result = sqlx::query(
        r#"
        UPDATE vpn_keys
        SET is_active = FALSE, private_key = ''
        WHERE is_active = TRUE
          AND (
            expires_at < NOW()
            OR last_used < NOW() - INTERVAL '24 hours'
          )
        "#,
    )
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to cleanup VPN keys: {e}"))?;

    let deactivated = result.rows_affected();
    // Always rewrite so authorized_keys cannot drift from DB (stale pubkeys).
    rewrite_authorized_keys(pool).await;

    match gc_inactive_vpn_keys(pool).await {
        Ok(n) if n > 0 => {
            tracing::info!(deleted = n, "VPN inactive keys garbage-collected");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "VPN inactive key GC failed"),
    }

    Ok(deactivated)
}

/// List all currently active (and not expired) VPN keys — admin view (no private keys).
pub async fn list_active_keys(pool: &PgPool) -> Result<Vec<VpnKeyInfo>, String> {
    sqlx::query_as::<_, VpnKeyInfo>(
        r#"
        SELECT id, citizen_id, public_key, expires_at, last_used, is_active, created_at
        FROM vpn_keys
        WHERE is_active = TRUE
          AND expires_at > NOW()
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to list VPN keys: {e}"))
}

/// POST /api/vpn/key — Citizen+
pub async fn generate_vpn_key_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    body: Option<Json<GenerateVpnKeyRequest>>,
) -> impl IntoResponse {
    if !tunnel::tunnel_enabled() {
        return response::service_unavailable(
            "SSH tunnel / VPN support is disabled (SSH_TUNNEL_ENABLED=false)",
        );
    }
    if let Some(resp) = require_citizen(&auth) {
        return resp;
    }

    if let Err(e) = cleanup_expired_keys(&state.db).await {
        tracing::warn!(error = %e, "VPN on-demand cleanup before issue failed");
    }

    let ttl_hours = body
        .and_then(|Json(b)| b.ttl_hours)
        .unwrap_or_else(default_ttl_hours);

    let citizen_id = auth.resolve_account_id(&state.db).await;
    match generate_vpn_key(&state.db, &citizen_id, ttl_hours).await {
        Ok(key) => {
            // Do not log private_key.
            tracing::info!(
                citizen_id = %key.citizen_id,
                key_id = %key.id,
                expires_at = %key.expires_at,
                "VPN temporary SSH key issued"
            );
            let tunnel_cfg = tunnel::tunnel_config();
            Json(ApiResponse::success(serde_json::json!({
                "id": key.id,
                "citizen_id": key.citizen_id,
                "private_key": key.private_key,
                "public_key": key.public_key,
                "expires_at": key.expires_at,
                "ttl_hours": ttl_hours,
                "ssh_user": std::env::var("SSH_TUNNEL_USER").unwrap_or_else(|_| "quazar-tunnel".into()),
                "tunnel": tunnel_cfg,
            })))
            .into_response()
        }
        Err(e) => {
            if e.contains("ttl_hours") {
                response::bad_request(e)
            } else {
                tracing::error!(error = %e, "VPN key generation failed");
                response::internal_error("Failed to generate VPN key")
            }
        }
    }
}

/// Alias used by main.rs / older call sites.
pub async fn create_vpn_key_handler(
    auth: Extension<AuthContext>,
    state: State<Arc<AppState>>,
    body: Option<Json<GenerateVpnKeyRequest>>,
) -> impl IntoResponse {
    generate_vpn_key_handler(auth, state, body).await
}

/// GET /api/vpn/status — Citizen+
pub async fn vpn_status_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(resp) = require_citizen(&auth) {
        return resp;
    }

    if let Err(e) = cleanup_expired_keys(&state.db).await {
        tracing::warn!(error = %e, "VPN on-demand cleanup before status failed");
    }

    let citizen_id = auth.resolve_account_id(&state.db).await;
    let tunnel_cfg = tunnel::tunnel_config();
    match get_vpn_status(&state.db, &citizen_id).await {
        Ok(status) => {
            let now = Utc::now();
            let seconds_remaining = status
                .expires_at
                .map(|exp| (exp - now).num_seconds().max(0))
                .unwrap_or(0);
            Json(ApiResponse::success(serde_json::json!({
                "is_active": status.is_active,
                "has_active_key": status.is_active,
                "expires_at": status.expires_at,
                "last_used": status.last_used,
                "seconds_remaining": seconds_remaining,
                "ssh_user": std::env::var("SSH_TUNNEL_USER").unwrap_or_else(|_| "quazar-tunnel".into()),
                "tunnel": tunnel_cfg,
            })))
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "VPN status failed");
            response::internal_error("Failed to load VPN status")
        }
    }
}

/// POST /api/vpn/revoke — Citizen+
pub async fn revoke_vpn_key_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(resp) = require_citizen(&auth) {
        return resp;
    }

    if let Err(e) = cleanup_expired_keys(&state.db).await {
        tracing::warn!(error = %e, "VPN on-demand cleanup before revoke failed");
    }

    let citizen_id = auth.resolve_account_id(&state.db).await;

    match revoke_vpn_key(&state.db, &citizen_id).await {
        Ok(false) => response::bad_request("No active VPN key to revoke"),
        Ok(true) => {
            tracing::info!(citizen_id = %citizen_id, "VPN key(s) revoked");
            Json(ApiResponse::success(serde_json::json!({
                "message": "VPN key revoked",
                "revoked": true,
            })))
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "VPN revoke failed");
            response::internal_error("Failed to revoke VPN key")
        }
    }
}

/// GET /api/vpn/keys — Aiya / Founder
pub async fn list_vpn_keys_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(resp) = require_aiya_admin(&auth) {
        return resp;
    }

    match list_active_keys(&state.db).await {
        Ok(keys) => Json(ApiResponse::success(serde_json::json!({ "keys": keys }))).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "VPN list failed");
            response::internal_error("Failed to list VPN keys")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn clamp_ttl_rejects_out_of_range() {
        assert!(clamp_ttl_hours(0).is_err());
        assert!(clamp_ttl_hours(169).is_err());
        assert_eq!(clamp_ttl_hours(24).unwrap(), 24);
    }

    #[test]
    fn openssh_keypair_roundtrip_shapes() {
        let (priv_pem, pub_line) = generate_openssh_keypair("test-comment").unwrap();
        assert!(priv_pem.contains("OPENSSH PRIVATE KEY"));
        assert!(pub_line.starts_with("ssh-ed25519 "));
        let parsed = PrivateKey::from_openssh(&priv_pem).unwrap();
        assert_eq!(parsed.comment(), "test-comment");
        assert!(!pub_line.contains("PRIVATE KEY"));
    }

    #[test]
    fn expiry_detects_past_and_boundary() {
        let now = Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap();
        assert!(!is_vpn_key_expired(now + ChronoDuration::hours(1), now));
        assert!(is_vpn_key_expired(now - ChronoDuration::seconds(1), now));
        assert!(is_vpn_key_expired(now, now));
    }

    #[test]
    fn idle_revoke_respects_last_used() {
        let now = Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap();
        assert!(!is_vpn_key_idle(None, now, 24));
        assert!(!is_vpn_key_idle(
            Some(now - ChronoDuration::hours(23)),
            now,
            24
        ));
        assert!(is_vpn_key_idle(
            Some(now - ChronoDuration::hours(24)),
            now,
            24
        ));
    }

    #[test]
    fn deactivate_on_new_key_keeps_only_new() {
        let previous = vec!["old-a".into(), "old-b".into()];
        let active = deactivate_on_new_key_ids(&previous, "new-1");
        assert_eq!(active, vec!["new-1".to_string()]);
        assert!(!active.iter().any(|id| id.starts_with("old-")));
    }

    #[test]
    fn secure_bytes_eq_matches_and_rejects() {
        assert!(secure_bytes_eq(b"abc", b"abc"));
        assert!(!secure_bytes_eq(b"abc", b"abd"));
        assert!(!secure_bytes_eq(b"abc", b"ab"));
    }
}
