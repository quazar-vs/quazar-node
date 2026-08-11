use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

pub use crate::types::Role;

use crate::citizen::hash_key;
use crate::types::CandidateType;

static NODE_SECRET: OnceLock<String> = OnceLock::new();
static REG_SECRET: OnceLock<String> = OnceLock::new();

pub fn init_node_secret() {
    let secret = std::env::var("QUAZAR_NODE_SECRET")
        .expect("QUAZAR_NODE_SECRET must be set (e.g. in .env)");
    if secret.trim().is_empty() {
        panic!("QUAZAR_NODE_SECRET must not be empty");
    }
    NODE_SECRET
        .set(secret)
        .expect("init_node_secret called twice");
}

pub fn init_reg_secret() {
    let secret = std::env::var("QUAZAR_REG_SECRET")
        .expect("QUAZAR_REG_SECRET must be set (e.g. in .env)");
    if secret.trim().is_empty() {
        panic!("QUAZAR_REG_SECRET must not be empty");
    }
    REG_SECRET
        .set(secret)
        .expect("init_reg_secret called twice");
}

pub fn node_secret() -> &'static str {
    NODE_SECRET.get().expect("Node secret not initialized")
}

pub fn reg_secret() -> &'static str {
    REG_SECRET.get().expect("Registration secret not initialized")
}

pub fn is_node_secret(key: &str) -> bool {
    key == node_secret()
}

/// Production mode: `QUAZAR_STRICT_SECRETS=true|1`.
pub fn is_production() -> bool {
    std::env::var("QUAZAR_STRICT_SECRETS")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

/// In STRICT mode both reCAPTCHA keys are mandatory at process start.
pub fn assert_recaptcha_in_production() {
    if !is_production() {
        return;
    }
    let secret_ok = std::env::var("RECAPTCHA_SECRET_KEY")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let site_ok = std::env::var("RECAPTCHA_SITE_KEY")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !secret_ok || !site_ok {
        panic!(
            "QUAZAR_STRICT_SECRETS: RECAPTCHA_SITE_KEY and RECAPTCHA_SECRET_KEY must both be set \
             (public-config would otherwise advertise captcha without a usable site key)"
        );
    }
}

/// In production `RECAPTCHA_SECRET_KEY` is mandatory; in dev an empty/missing key is allowed.
pub fn ensure_recaptcha_secret() -> Result<(), String> {
    let configured = std::env::var("RECAPTCHA_SECRET_KEY")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if configured {
        return Ok(());
    }
    if is_production() {
        return Err("RECAPTCHA_SECRET_KEY must be set in production".to_string());
    }
    Ok(())
}

pub fn compute_registration_signature(
    citizen_id: &str,
    public_key: &str,
    event_hash: &str,
    secret: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"quazar_reg_sig:");
    hasher.update(citizen_id.as_bytes());
    hasher.update(public_key.as_bytes());
    hasher.update(event_hash.as_bytes());
    hasher.update(secret.as_bytes());
    format!("reg_sig_{:x}", hasher.finalize())
}

pub fn compute_node_signature(event_id: &str, event_hash: &str, secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"quazar_node_sig:");
    hasher.update(event_id.as_bytes());
    hasher.update(event_hash.as_bytes());
    hasher.update(secret.as_bytes());
    format!("node_sig_{:x}", hasher.finalize())
}

pub fn registration_signature(citizen_id: &str, public_key: &str, event_hash: &str) -> String {
    compute_registration_signature(citizen_id, public_key, event_hash, reg_secret())
}

pub fn internal_node_signature(event_id: &str, event_hash: &str) -> String {
    compute_node_signature(event_id, event_hash, node_secret())
}

/// Validated API key identity. `key` holds the SHA-256 hex (`key_hash`); plaintext is never stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiKey {
    pub id: String,
    pub key: String,
    pub role: Role,
    pub citizen_name: String,
    pub citizen_id: String,
}

#[derive(sqlx::FromRow)]
struct ApiKeyJoinRow {
    id: String,
    key_hash: String,
    role: String,
    citizen_id: String,
    citizen_name: String,
}

impl From<ApiKeyJoinRow> for ApiKey {
    fn from(row: ApiKeyJoinRow) -> Self {
        Self {
            id: row.id,
            key: row.key_hash,
            role: Role::from_str(&row.role).unwrap_or(Role::Citizen),
            citizen_name: row.citizen_name,
            citizen_id: row.citizen_id,
        }
    }
}

pub struct KeyStore;

impl KeyStore {
    /// Validate a raw API key. Uses `citizen::hash_key` for DB lookup.
    pub async fn validate_key(pool: &PgPool, key: &str) -> Option<ApiKey> {
        if key.is_empty() {
            return None;
        }

        let key_hash = hash_key(key);
        let row = sqlx::query_as::<_, ApiKeyJoinRow>(
            r#"
            SELECT
                k.id,
                k.key_hash,
                k.role,
                k.citizen_id,
                c.name AS citizen_name
            FROM api_keys k
            JOIN citizens c ON c.id = k.citizen_id
            WHERE k.key_hash = $1
              AND k.is_active = TRUE
              AND (k.expires_at IS NULL OR k.expires_at > NOW())
            "#,
        )
        .bind(&key_hash)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;

        let _ = sqlx::query("UPDATE api_keys SET last_used = NOW() WHERE id = $1")
            .bind(&row.id)
            .execute(pool)
            .await;

        Some(ApiKey::from(row))
    }

    /// Store a plaintext key as SHA-256 hash bound to the citizen by name.
    pub async fn upsert_key(
        pool: &PgPool,
        key: &str,
        role: Role,
        citizen_name: &str,
    ) -> Result<(), sqlx::Error> {
        let key_hash = hash_key(key);
        Self::upsert_hashed_key(pool, &key_hash, role, citizen_name).await
    }

    /// Store an already-computed `key_hash` (P2P sync of hashed keys).
    pub async fn upsert_hashed_key(
        pool: &PgPool,
        key_hash: &str,
        role: Role,
        citizen_name: &str,
    ) -> Result<(), sqlx::Error> {
        let citizen_row: Option<(String, String, String)> =
            sqlx::query_as("SELECT id, status, role FROM citizens WHERE name = $1")
                .bind(citizen_name)
                .fetch_optional(pool)
                .await?;

        let Some((citizen_id, status, citizen_role)) = citizen_row else {
            tracing::warn!(
                citizen = %citizen_name,
                "upsert API key skipped: citizen not found"
            );
            return Ok(());
        };

        if status == "applicant" || citizen_role == Role::Candidate.as_str() {
            tracing::warn!(
                citizen = %citizen_name,
                status = %status,
                role = %citizen_role,
                "upsert API key skipped: Candidate/applicant cannot hold API keys"
            );
            return Ok(());
        }

        let existing_id: Option<String> =
            sqlx::query_scalar("SELECT id FROM api_keys WHERE key_hash = $1")
                .bind(key_hash)
                .fetch_optional(pool)
                .await?;

        if let Some(id) = existing_id {
            sqlx::query(
                r#"
                UPDATE api_keys
                SET citizen_id = $1,
                    role = $2,
                    is_active = TRUE
                WHERE id = $3
                "#,
            )
            .bind(&citizen_id)
            .bind(role.as_str())
            .bind(&id)
            .execute(pool)
            .await?;
        } else {
            let id = Uuid::new_v4().to_string();
            sqlx::query(
                r#"
                INSERT INTO api_keys (id, citizen_id, key_hash, role, created_at, expires_at, is_active, last_used)
                VALUES ($1, $2, $3, $4, NOW(), NULL, TRUE, NULL)
                "#,
            )
            .bind(&id)
            .bind(&citizen_id)
            .bind(key_hash)
            .bind(role.as_str())
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    /// Revoke by plaintext key (hashed for lookup).
    pub async fn revoke_key(pool: &PgPool, key: &str) -> Result<bool, sqlx::Error> {
        let key_hash = hash_key(key);
        Self::revoke_hashed_key(pool, &key_hash).await
    }

    pub async fn revoke_hashed_key(pool: &PgPool, key_hash: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE api_keys SET is_active = FALSE WHERE key_hash = $1 AND is_active = TRUE",
        )
        .bind(key_hash)
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn count_active(pool: &PgPool) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE is_active = TRUE")
            .fetch_one(pool)
            .await
    }

    pub async fn list_active(pool: &PgPool) -> Result<Vec<ApiKeySummary>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ApiKeyJoinRow>(
            r#"
            SELECT
                k.id,
                k.key_hash,
                k.role,
                k.citizen_id,
                c.name AS citizen_name
            FROM api_keys k
            JOIN citizens c ON c.id = k.citizen_id
            WHERE k.is_active = TRUE
            ORDER BY k.created_at
            "#,
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| ApiKeySummary {
                key_masked: mask_key(&row.key_hash),
                role: row.role,
                citizen_name: row.citizen_name,
            })
            .collect())
    }

    pub async fn list_active_full(pool: &PgPool) -> Result<Vec<ApiKey>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ApiKeyJoinRow>(
            r#"
            SELECT
                k.id,
                k.key_hash,
                k.role,
                k.citizen_id,
                c.name AS citizen_name
            FROM api_keys k
            JOIN citizens c ON c.id = k.citizen_id
            WHERE k.is_active = TRUE
            ORDER BY k.created_at
            "#,
        )
        .fetch_all(pool)
        .await?;

        Ok(rows.into_iter().map(ApiKey::from).collect())
    }

    pub async fn lookup_citizen_role(pool: &PgPool, citizen_name: &str) -> Option<Role> {
        sqlx::query_scalar::<_, String>("SELECT role FROM citizens WHERE name = $1")
            .bind(citizen_name)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|role_str| Role::from_str(&role_str))
    }

    pub async fn sync_roles_from_citizens(pool: &PgPool) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE api_keys SET role = citizens.role
            FROM citizens
            WHERE api_keys.citizen_id = citizens.id
              AND api_keys.role IS DISTINCT FROM citizens.role
            "#,
        )
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn sync_citizen_key_roles(pool: &PgPool, citizen_name: &str, role: Role) {
        if let Err(e) = sqlx::query(
            r#"
            UPDATE api_keys
            SET role = $1
            WHERE citizen_id = (SELECT id FROM citizens WHERE name = $2)
              AND is_active = TRUE
              AND role IS DISTINCT FROM $1
            "#,
        )
        .bind(role.as_str())
        .bind(citizen_name)
        .execute(pool)
        .await
        {
            tracing::warn!(
                citizen = %citizen_name,
                error = %e,
                "failed to sync api_keys.role from citizens"
            );
        }
    }

    pub async fn sync_from_env(pool: &PgPool) -> Result<(), sqlx::Error> {
        sync_env_keys(pool).await?;
        sync_test_keys(pool).await?;
        let synced = Self::sync_roles_from_citizens(pool).await.unwrap_or(0);
        if synced > 0 {
            tracing::info!(synced, "api_keys.role aligned with citizens after env sync");
        }
        let count = Self::count_active(pool).await.unwrap_or(0);
        tracing::info!(count, "API keys synced to PostgreSQL");
        Ok(())
    }
}

#[derive(Debug, serde::Serialize)]
pub struct ApiKeySummary {
    pub key_masked: String,
    pub role: String,
    pub citizen_name: String,
}

pub fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return "<empty>".to_string();
    }
    if key.len() <= 8 {
        return "***".to_string();
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
}

async fn sync_env_keys(pool: &PgPool) -> Result<(), sqlx::Error> {
    let Ok(raw) = std::env::var("QUAZAR_API_KEYS") else {
        return Ok(());
    };

    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = entry.split(':').map(str::trim).collect();
        if parts.len() != 3 {
            tracing::warn!(entry = %entry, "skipped API key: invalid format");
            continue;
        }
        let (key, role_str, citizen_name) = (parts[0], parts[1], parts[2]);
        let Some(env_role) = Role::from_str(role_str) else {
            tracing::warn!(entry = %entry, role = %role_str, "skipped API key: unknown role");
            continue;
        };
        let role = KeyStore::lookup_citizen_role(pool, citizen_name)
            .await
            .unwrap_or(env_role);
        KeyStore::upsert_key(pool, key, role, citizen_name).await?;
        tracing::info!(key = %mask_key(key), citizen = %citizen_name, role = %role_str, "synced API key from env");
    }
    Ok(())
}

async fn sync_test_keys(pool: &PgPool) -> Result<(), sqlx::Error> {
    let enabled = std::env::var("QUAZAR_INIT_TEST_KEYS")
        .ok()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    if !enabled {
        return Ok(());
    }

    let test_keys = [
        ("test_citizen_key_2026", Role::Citizen, "testcitizen"),
        ("buyer_key_2026", Role::Citizen, "buyercitizen"),
        ("seller_key_2026", Role::Citizen, "sellercitizen"),
    ];

    for (key, role, name) in &test_keys {
        KeyStore::upsert_key(pool, key, role.clone(), name).await?;
    }
    tracing::info!(count = test_keys.len(), "test API keys synced");
    Ok(())
}

/// Known Agent permission strings (must match `agent_permissions_permission_check`).
pub const PERM_CHAT_READ: &str = "chat.read";
pub const PERM_CHAT_SEND: &str = "chat.send";
pub const PERM_CHAT_MANAGE: &str = "chat.manage";
pub const PERM_BLOCKCHAIN_READ: &str = "blockchain.read";

/// Default permissions granted when a new Agent is created (verify / digital apply).
pub const DEFAULT_AGENT_PERMISSIONS: &[&str] = &[PERM_CHAT_READ, PERM_CHAT_SEND];

pub fn is_valid_agent_permission(permission: &str) -> bool {
    matches!(
        permission,
        PERM_CHAT_READ | PERM_CHAT_SEND | PERM_CHAT_MANAGE | PERM_BLOCKCHAIN_READ
    )
}

/// Auth identity resolved from a citizen API key (or node secret).
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub citizen_name: String,
    pub role: Role,
    pub is_node: bool,
    /// Citizen UUID when resolved during auth (needed for Agent permission checks).
    pub citizen_id: Option<String>,
    /// Loaded from `agent_permissions` for Agent; empty for other roles.
    pub agent_permissions: Vec<String>,
}

impl AuthContext {
    pub fn node() -> Self {
        Self {
            citizen_name: "__node__".to_string(),
            role: Role::Guardian,
            is_node: true,
            citizen_id: None,
            agent_permissions: Vec::new(),
        }
    }

    pub fn from_api_key(citizen_name: String, role: Role) -> Self {
        Self {
            citizen_name,
            role,
            is_node: false,
            citizen_id: None,
            agent_permissions: Vec::new(),
        }
    }

    pub fn with_citizen_id(mut self, citizen_id: impl Into<String>) -> Self {
        self.citizen_id = Some(citizen_id.into());
        self
    }

    pub fn with_agent_permissions(mut self, permissions: Vec<String>) -> Self {
        self.agent_permissions = permissions;
        self
    }

    /// Whether this Agent has a specific permission in the loaded set.
    /// Founder/Aiya always true for Agent-scoped checks; non-Agent returns false.
    pub fn agent_has_permission(&self, permission: &str) -> bool {
        if self.is_node {
            return false;
        }
        if matches!(self.role, Role::Founder | Role::Aiya) {
            return true;
        }
        if !self.role.is_agent() {
            return false;
        }
        self.agent_permissions.iter().any(|p| p == permission)
    }

    /// Admin roles: Founder and Aiya. Agent is read-only; Candidate is public-read only.
    pub fn is_admin(&self) -> bool {
        !self.is_node && matches!(self.role, Role::Founder | Role::Aiya)
    }

    pub fn has_permission(&self, required: &Role) -> bool {
        !self.is_node && !self.role.is_candidate() && self.role.satisfies(required)
    }

    /// Authenticated read for non-node identities. Candidate may only use public (unauthenticated) routes.
    pub fn can_read(&self) -> bool {
        self.has_system_access()
    }

    /// Bounded command execution (queries, actions on own resources). Agent is allowed; Candidate is not.
    pub fn can_execute_commands(&self) -> bool {
        self.has_system_access()
    }

    /// Candidate (and node) have no system API access — public routes only.
    pub fn has_system_access(&self) -> bool {
        !self.is_node && !self.role.is_candidate()
    }

    /// Candidate must not hold API keys.
    pub fn can_hold_api_key(&self) -> bool {
        !self.is_node && !self.role.is_candidate()
    }

    /// Register new citizens — Founder and Aiya only.
    pub fn can_register_citizens(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(self.role, Role::Founder | Role::Aiya)
    }

    /// Add peers to the registry and PeerListUpdate events — Founder and Aiya only.
    pub fn can_manage_peers(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(self.role, Role::Founder | Role::Aiya)
    }

    pub fn can_add_balance(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(self.role, Role::Founder | Role::Aiya)
    }

    pub fn can_manage_svod(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(self.role, Role::Founder | Role::Aiya)
    }

    /// Exchange Security (СБ): verify sell requests and confirm delivery. Not Aiya deal-flow.
    pub fn can_exchange_security(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(self.role, Role::Guardian | Role::Founder)
    }

    /// Биржа и агенты СБ: только Страж и выше (КВА / Свод).
    pub fn can_use_exchange(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(
                self.role,
                Role::Guardian | Role::Judge | Role::Mayor | Role::Aiya | Role::Founder
            )
    }

    /// Update citizen status — Founder and Aiya only.
    pub fn can_update_citizen_status(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(self.role, Role::Founder | Role::Aiya)
    }

    /// Issue / revoke passports (and related citizen admin ops).
    pub fn can_manage_citizens(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(
                self.role,
                Role::Founder | Role::Aiya | Role::Mayor | Role::Guardian
            )
    }

    /// Start / advance mayor elections (not available to Agent/Candidate).
    pub fn can_manage_mayor_elections(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(
                self.role,
                Role::Founder | Role::Aiya | Role::Mayor | Role::Guardian
            )
    }

    /// SB verification: Candidate → Agent (Founder, Aiya, Guardian).
    pub fn can_verify_agent(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(self.role, Role::Founder | Role::Aiya | Role::Guardian)
    }

    /// Judge approval: Agent → Citizen + passport.
    pub fn can_approve_citizen(&self) -> bool {
        self.has_system_access() && self.role == Role::Judge
    }

    pub fn can_change_citizen_role(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(self.role, Role::Founder | Role::Aiya)
    }

    /// Whether `self` may grant `desired_role` given the candidate's type / target.
    ///
    /// - Citizenship candidate → only `Citizen`
    /// - Role candidate → only the exact `target_role`
    /// - Assigner cannot grant a role above their own hierarchy (`satisfies`)
    /// - Aiya/Founder additionally cannot grant above the candidacy `target_role`
    pub fn can_assign_role(
        &self,
        candidate_type: Option<&CandidateType>,
        target_role: Option<&str>,
        desired_role: &Role,
    ) -> bool {
        can_assign_role(&self.role, candidate_type, target_role, desired_role)
    }

    pub fn can_manage_api_keys(&self) -> bool {
        self.has_system_access()
            && !self.role.is_agent()
            && matches!(self.role, Role::Founder | Role::Aiya)
    }

    /// Cast votes in governance / referendums / initiatives / generic votes.
    /// Agent and Candidate are excluded; Citizen and higher political roles may vote.
    pub fn can_vote(&self) -> bool {
        self.has_system_access()
            && matches!(
                self.role,
                Role::Citizen
                    | Role::Judge
                    | Role::Guardian
                    | Role::Mayor
                    | Role::Aiya
                    | Role::Founder
            )
    }

    /// Vote in mayor elections (same electorate as `can_vote`).
    pub fn can_vote_in_election(&self) -> bool {
        self.can_vote()
    }

    /// Read chat messages, session, history, and search.
    /// Citizen+ roles: always. Agent: requires `chat.read` in `agent_permissions`. Candidate: no.
    pub fn can_read_chat(&self) -> bool {
        if !self.has_system_access() {
            return false;
        }
        if self.role.is_agent() {
            return self.agent_has_permission(PERM_CHAT_READ);
        }
        matches!(
            self.role,
            Role::Citizen
                | Role::Judge
                | Role::Guardian
                | Role::Mayor
                | Role::Aiya
                | Role::Founder
        )
    }

    /// Send chat messages. Citizen+: always. Agent: requires `chat.send`.
    pub fn can_send_chat(&self) -> bool {
        if !self.has_system_access() {
            return false;
        }
        if self.role.is_agent() {
            return self.agent_has_permission(PERM_CHAT_SEND);
        }
        self.can_read_chat()
    }

    /// Chat moderation / admin actions (delete, ban, admin bot send).
    /// Founder/Aiya: always. Agent: requires `chat.manage` (rarely granted).
    pub fn can_manage_chat(&self) -> bool {
        if self.is_admin() {
            return true;
        }
        self.role.is_agent() && self.agent_has_permission(PERM_CHAT_MANAGE)
    }

    pub fn insufficient_permissions_message(&self) -> &'static str {
        if self.role.is_candidate() {
            "Insufficient permissions. Candidate role allows public read only"
        } else if self.role.is_agent() {
            "Insufficient permissions. Agent lacks the required permission"
        } else {
            "Insufficient permissions"
        }
    }

    pub async fn authorize_citizen_ref(&self, db: &PgPool, citizen_ref: &str) -> bool {
        if self.is_node {
            return false;
        }
        if self.citizen_name == citizen_ref {
            return true;
        }
        let db_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM citizens WHERE name = $1",
        )
        .bind(&self.citizen_name)
        .fetch_optional(db)
        .await
        .unwrap_or(None);
        db_id.as_deref() == Some(citizen_ref)
    }

    pub async fn resolve_account_id(&self, db: &PgPool) -> String {
        if self.is_node {
            return self.citizen_name.clone();
        }
        if let Some(id) = &self.citizen_id {
            return id.clone();
        }
        sqlx::query_scalar("SELECT id FROM citizens WHERE name = $1")
            .bind(&self.citizen_name)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| self.citizen_name.clone())
    }
}

/// Whether `agent_id` has `permission` in `agent_permissions`.
/// Founder/Aiya callers should use role checks; this is for Agent row lookups.
pub async fn has_agent_permission(pool: &PgPool, agent_id: &str, permission: &str) -> bool {
    if agent_id.is_empty() || permission.is_empty() {
        return false;
    }
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM agent_permissions
            WHERE agent_id = $1 AND permission = $2
        )
        "#,
    )
    .bind(agent_id)
    .bind(permission)
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}

/// Load all permission strings for an Agent.
pub async fn list_agent_permissions(pool: &PgPool, agent_id: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT permission FROM agent_permissions
        WHERE agent_id = $1
        ORDER BY permission
        "#,
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await
}

/// Grant a single permission (idempotent via ON CONFLICT DO NOTHING).
pub async fn grant_agent_permission(
    pool: &PgPool,
    agent_id: &str,
    permission: &str,
    granted_by: Option<&str>,
) -> Result<bool, sqlx::Error> {
    if !is_valid_agent_permission(permission) {
        return Ok(false);
    }
    let id = Uuid::new_v4().to_string();
    let result = sqlx::query(
        r#"
        INSERT INTO agent_permissions (id, agent_id, permission, granted_by, granted_at)
        VALUES ($1, $2, $3, $4, NOW())
        ON CONFLICT (agent_id, permission) DO NOTHING
        "#,
    )
    .bind(&id)
    .bind(agent_id)
    .bind(permission)
    .bind(granted_by)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Grant multiple permissions; returns count of newly inserted rows.
pub async fn grant_agent_permissions(
    pool: &PgPool,
    agent_id: &str,
    permissions: &[&str],
    granted_by: Option<&str>,
) -> Result<u64, sqlx::Error> {
    let mut granted = 0u64;
    for perm in permissions {
        if grant_agent_permission(pool, agent_id, perm, granted_by).await? {
            granted += 1;
        }
    }
    Ok(granted)
}

/// Revoke a permission. Returns true if a row was deleted.
pub async fn revoke_agent_permission(
    pool: &PgPool,
    agent_id: &str,
    permission: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM agent_permissions WHERE agent_id = $1 AND permission = $2",
    )
    .bind(agent_id)
    .bind(permission)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Seed default chat permissions for a newly created Agent.
pub async fn grant_default_agent_permissions(
    pool: &PgPool,
    agent_id: &str,
    granted_by: Option<&str>,
) -> Result<u64, sqlx::Error> {
    grant_agent_permissions(pool, agent_id, DEFAULT_AGENT_PERMISSIONS, granted_by).await
}

/// Pure rule for assigning a role to a candidacy-bound identity.
///
/// `candidate_type` / `target_role` come from the citizen row (or candidacy).
/// When `candidate_type` is `None`, only the assigner hierarchy check applies
/// (legacy / non-candidate updates); callers should still block governance via candidacy.
pub fn can_assign_role(
    assigner: &Role,
    candidate_type: Option<&CandidateType>,
    target_role: Option<&str>,
    desired_role: &Role,
) -> bool {
    // Never assign above the assigner's own hierarchy.
    if !assigner.satisfies(desired_role) {
        return false;
    }

    match candidate_type {
        Some(CandidateType::Citizenship) => {
            // Citizenship path: only Citizen (target_role is Citizen or unset).
            if *desired_role != Role::Citizen {
                return false;
            }
            match target_role.map(str::trim).filter(|s| !s.is_empty()) {
                None | Some("Citizen") => true,
                Some(_) => false,
            }
        }
        Some(CandidateType::Role) => {
            let Some(target) = target_role.map(str::trim).filter(|s| !s.is_empty()) else {
                return false;
            };
            // Exact match only — never above the nominated target_role.
            desired_role.as_str() == target
        }
        None => {
            // No candidacy metadata: allow only non-governance assignments
            // that the assigner hierarchy already permits.
            !desired_role.is_governance() && !desired_role.is_mayor()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CandidateType;

    fn ctx(role: Role) -> AuthContext {
        AuthContext::from_api_key("voter".into(), role)
    }

    fn agent_with(perms: &[&str]) -> AuthContext {
        AuthContext::from_api_key("agent".into(), Role::Agent)
            .with_citizen_id("agent-id")
            .with_agent_permissions(perms.iter().map(|s| (*s).to_string()).collect())
    }

    #[test]
    fn can_vote_allows_citizen_and_officials_not_agent() {
        assert!(ctx(Role::Citizen).can_vote());
        assert!(ctx(Role::Judge).can_vote());
        assert!(ctx(Role::Guardian).can_vote());
        assert!(ctx(Role::Mayor).can_vote());
        assert!(ctx(Role::Aiya).can_vote());
        assert!(ctx(Role::Founder).can_vote());

        assert!(!ctx(Role::Agent).can_vote());
        assert!(!ctx(Role::Agent).can_vote_in_election());
        assert!(!ctx(Role::Candidate).can_vote());
        assert!(!AuthContext::node().can_vote());
    }

    #[test]
    fn can_vote_in_election_mirrors_can_vote() {
        for role in [
            Role::Citizen,
            Role::Judge,
            Role::Guardian,
            Role::Mayor,
            Role::Aiya,
            Role::Founder,
            Role::Agent,
            Role::Candidate,
        ] {
            let a = ctx(role);
            assert_eq!(a.can_vote(), a.can_vote_in_election());
        }
    }

    #[test]
    fn can_manage_peers_founder_and_aiya_only() {
        assert!(ctx(Role::Founder).can_manage_peers());
        assert!(ctx(Role::Aiya).can_manage_peers());
        assert!(!ctx(Role::Mayor).can_manage_peers());
        assert!(!ctx(Role::Guardian).can_manage_peers());
        assert!(!ctx(Role::Citizen).can_manage_peers());
        assert!(!ctx(Role::Agent).can_manage_peers());
        assert!(!AuthContext::node().can_manage_peers());
    }

    #[test]
    fn candidate_has_no_privileged_permissions() {
        let c = ctx(Role::Candidate);
        assert!(!c.can_read());
        assert!(!c.can_execute_commands());
        assert!(!c.has_system_access());
        assert!(!c.can_hold_api_key());
        assert!(!c.can_vote());
        assert!(!c.can_vote_in_election());
        assert!(!c.can_register_citizens());
        assert!(!c.can_manage_peers());
        assert!(!c.can_add_balance());
        assert!(!c.can_manage_svod());
        assert!(!c.can_update_citizen_status());
        assert!(!c.can_manage_citizens());
        assert!(!c.can_manage_mayor_elections());
        assert!(!c.can_verify_agent());
        assert!(!c.can_approve_citizen());
        assert!(!c.can_change_citizen_role());
        assert!(!c.can_manage_api_keys());
        assert!(!c.is_admin());
        assert!(!c.can_read_chat());
        assert!(!c.can_send_chat());
        assert!(!c.can_manage_chat());
        assert!(!c.has_permission(&Role::Candidate));
    }

    #[test]
    fn agent_without_permissions_denied_chat() {
        let agent = ctx(Role::Agent);
        assert!(!agent.can_read_chat());
        assert!(!agent.can_send_chat());
        assert!(!agent.can_manage_chat());
        assert!(!agent.is_admin());
    }

    #[test]
    fn agent_with_chat_permissions() {
        let read_only = agent_with(&[PERM_CHAT_READ]);
        assert!(read_only.can_read_chat());
        assert!(!read_only.can_send_chat());
        assert!(!read_only.can_manage_chat());

        let send = agent_with(&[PERM_CHAT_READ, PERM_CHAT_SEND]);
        assert!(send.can_read_chat());
        assert!(send.can_send_chat());
        assert!(!send.can_manage_chat());

        let manage = agent_with(&[PERM_CHAT_MANAGE]);
        assert!(!manage.can_read_chat());
        assert!(!manage.can_send_chat());
        assert!(manage.can_manage_chat());
    }

    #[test]
    fn non_agent_roles_chat_unchanged() {
        for role in [
            Role::Citizen,
            Role::Judge,
            Role::Guardian,
            Role::Mayor,
            Role::Aiya,
            Role::Founder,
        ] {
            let a = ctx(role.clone());
            assert!(a.can_read_chat(), "{role:?} should read chat");
            assert!(a.can_send_chat(), "{role:?} should send chat");
        }

        assert!(ctx(Role::Founder).can_manage_chat());
        assert!(ctx(Role::Aiya).can_manage_chat());
        assert!(!ctx(Role::Citizen).can_manage_chat());
        assert!(!ctx(Role::Judge).can_manage_chat());
        assert!(!ctx(Role::Guardian).can_manage_chat());
        assert!(!ctx(Role::Mayor).can_manage_chat());
        assert!(!AuthContext::node().can_read_chat());
        assert!(!AuthContext::node().can_send_chat());
        assert!(!AuthContext::node().can_manage_chat());
    }

    #[test]
    fn founder_aiya_agent_has_permission_shortcut() {
        assert!(ctx(Role::Founder).agent_has_permission(PERM_CHAT_READ));
        assert!(ctx(Role::Aiya).agent_has_permission(PERM_CHAT_SEND));
        assert!(!ctx(Role::Citizen).agent_has_permission(PERM_CHAT_READ));
        assert!(!agent_with(&[]).agent_has_permission(PERM_CHAT_READ));
        assert!(agent_with(&[PERM_CHAT_READ]).agent_has_permission(PERM_CHAT_READ));
    }

    #[test]
    fn is_valid_agent_permission_strings() {
        assert!(is_valid_agent_permission(PERM_CHAT_READ));
        assert!(is_valid_agent_permission(PERM_CHAT_SEND));
        assert!(is_valid_agent_permission(PERM_CHAT_MANAGE));
        assert!(is_valid_agent_permission(PERM_BLOCKCHAIN_READ));
        assert!(!is_valid_agent_permission("chat.delete"));
        assert!(!is_valid_agent_permission(""));
    }

    #[test]
    fn verify_and_approve_permissions() {
        assert!(ctx(Role::Founder).can_verify_agent());
        assert!(ctx(Role::Aiya).can_verify_agent());
        assert!(ctx(Role::Guardian).can_verify_agent());
        assert!(!ctx(Role::Judge).can_verify_agent());
        assert!(!ctx(Role::Citizen).can_verify_agent());
        assert!(!ctx(Role::Agent).can_verify_agent());
        assert!(!ctx(Role::Candidate).can_verify_agent());

        assert!(ctx(Role::Judge).can_approve_citizen());
        assert!(!ctx(Role::Founder).can_approve_citizen());
        assert!(!ctx(Role::Guardian).can_approve_citizen());
        assert!(!ctx(Role::Aiya).can_approve_citizen());
        assert!(!ctx(Role::Agent).can_approve_citizen());
        assert!(!ctx(Role::Citizen).can_approve_citizen());
        assert!(!ctx(Role::Candidate).can_approve_citizen());
    }

    #[test]
    fn can_assign_role_citizenship_only_citizen() {
        let aiya = Role::Aiya;
        let judge = Role::Judge;
        let ct = CandidateType::Citizenship;

        assert!(can_assign_role(
            &aiya,
            Some(&ct),
            Some("Citizen"),
            &Role::Citizen
        ));
        assert!(can_assign_role(&judge, Some(&ct), None, &Role::Citizen));
        assert!(!can_assign_role(
            &aiya,
            Some(&ct),
            Some("Citizen"),
            &Role::Guardian
        ));
        assert!(!can_assign_role(
            &aiya,
            Some(&ct),
            Some("Citizen"),
            &Role::Agent
        ));
        assert!(ctx(Role::Aiya).can_assign_role(Some(&ct), Some("Citizen"), &Role::Citizen));
    }

    #[test]
    fn can_assign_role_role_exact_target_only() {
        let aiya = Role::Aiya;
        let ct = CandidateType::Role;

        assert!(can_assign_role(
            &aiya,
            Some(&ct),
            Some("Guardian"),
            &Role::Guardian
        ));
        assert!(!can_assign_role(
            &aiya,
            Some(&ct),
            Some("Guardian"),
            &Role::Judge
        ));
        assert!(!can_assign_role(
            &aiya,
            Some(&ct),
            Some("Guardian"),
            &Role::Aiya
        ));
        assert!(!can_assign_role(
            &aiya,
            Some(&ct),
            Some("Guardian"),
            &Role::Citizen
        ));
        // Cannot assign above nominated target (Aiya when target is Judge).
        assert!(!can_assign_role(
            &aiya,
            Some(&ct),
            Some("Judge"),
            &Role::Aiya
        ));
        assert!(can_assign_role(&aiya, Some(&ct), Some("Aiya"), &Role::Aiya));
    }

    #[test]
    fn can_assign_role_respects_assigner_hierarchy() {
        let guardian = Role::Guardian;
        let ct = CandidateType::Role;

        // Guardian cannot assign Aiya (above hierarchy) even if target says Aiya.
        assert!(!can_assign_role(
            &guardian,
            Some(&ct),
            Some("Aiya"),
            &Role::Aiya
        ));
        // Guardian can assign Guardian if that is the target.
        assert!(can_assign_role(
            &guardian,
            Some(&ct),
            Some("Guardian"),
            &Role::Guardian
        ));
        // Aiya cannot assign Founder.
        assert!(!can_assign_role(
            &Role::Aiya,
            Some(&ct),
            Some("Founder"),
            &Role::Founder
        ));
    }

    #[test]
    fn can_assign_role_without_candidacy_blocks_governance() {
        assert!(can_assign_role(
            &Role::Aiya,
            None,
            None,
            &Role::Citizen
        ));
        assert!(can_assign_role(&Role::Aiya, None, None, &Role::Agent));
        assert!(!can_assign_role(
            &Role::Aiya,
            None,
            None,
            &Role::Guardian
        ));
        assert!(!can_assign_role(&Role::Aiya, None, None, &Role::Mayor));
    }
}
