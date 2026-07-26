use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

pub use crate::types::Role;

use crate::auth::{
    grant_default_agent_permissions, list_agent_permissions, registration_signature, AuthContext,
};
use crate::applications::{recaptcha_required, verify_recaptcha};
use crate::block_producer;
use crate::blockchain::{
    build_citizen_added_event, build_signed_citizen_role_event, build_signed_citizen_status_event,
    build_signed_passport_issued_event, build_signed_passport_revoked_event, compute_event_hash,
};
use crate::models::Event;
use crate::pending;
use crate::register_verify::{
    clear_recent_email_verification, consume_verification_code, email_belongs_to_citizen,
    issue_verification_code, is_valid_email, normalize_email, peek_recent_email_verification,
    citizen_use_app_forbidden,
};
use crate::response::ApiResponse;
use crate::types::{is_role_candidacy_target, CandidateType};
use crate::validator::{normalize_and_validate_citizen_name, EventValidator};
use crate::AppState;
use std::net::SocketAddr;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum CitizenStatus {
    /// Application submitted; not reviewed yet. No API key, no system access.
    Applicant,
    /// Registered / awaiting passport.
    Pending,
    Active,
    Suspended,
    Revoked,
}

impl CitizenStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CitizenStatus::Applicant => "applicant",
            CitizenStatus::Pending => "pending",
            CitizenStatus::Active => "active",
            CitizenStatus::Suspended => "suspended",
            CitizenStatus::Revoked => "revoked",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "applicant" => Some(CitizenStatus::Applicant),
            "pending" => Some(CitizenStatus::Pending),
            "active" => Some(CitizenStatus::Active),
            "suspended" => Some(CitizenStatus::Suspended),
            "revoked" => Some(CitizenStatus::Revoked),
            _ => None,
        }
    }

    /// Applicant: no login, no API, no routes. Pending may touch passport endpoints only.
    pub fn has_system_access(&self) -> bool {
        !matches!(
            self,
            CitizenStatus::Applicant | CitizenStatus::Revoked | CitizenStatus::Suspended
        )
    }

    /// Applicants must not hold API keys (nor revoked).
    pub fn can_hold_api_key(&self) -> bool {
        !matches!(
            self,
            CitizenStatus::Applicant | CitizenStatus::Revoked
        )
    }

    pub fn is_applicant(&self) -> bool {
        matches!(self, CitizenStatus::Applicant)
    }
}

#[derive(Debug, Clone)]
pub struct Citizen {
    pub id: String,
    pub name: String,
    pub public_key: String,
    pub status: CitizenStatus,
    pub role: Role,
    pub created_at: i64,
    pub passport_issued: bool,
    pub passport_expires: Option<i64>,
    /// True for digital (software) agents; they cannot become Citizens.
    pub is_digital: bool,
    pub email_verified: bool,
    /// `citizenship` | `role` while on a candidacy path; cleared after final role.
    pub candidate_type: Option<CandidateType>,
    /// For citizenship: `Citizen` (or unset). For role: exact elevated role.
    pub target_role: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct CitizenRow {
    id: String,
    name: String,
    public_key: String,
    status: String,
    role: String,
    created_at: i64,
    passport_issued: bool,
    passport_expires: Option<i64>,
    is_digital: bool,
    email_verified: bool,
    candidate_type: Option<String>,
    target_role: Option<String>,
}

impl TryFrom<CitizenRow> for Citizen {
    type Error = CitizenError;

    fn try_from(row: CitizenRow) -> Result<Self, Self::Error> {
        let candidate_type = match row.candidate_type.as_deref() {
            None | Some("") => None,
            Some(s) => Some(CandidateType::from_str(s).ok_or(CitizenError::InvalidRole)?),
        };
        Ok(Citizen {
            id: row.id,
            name: row.name,
            public_key: row.public_key,
            status: CitizenStatus::from_str(&row.status).ok_or(CitizenError::InvalidStatus)?,
            role: Role::from_str(&row.role).ok_or(CitizenError::InvalidRole)?,
            created_at: row.created_at,
            passport_issued: row.passport_issued,
            passport_expires: row.passport_expires,
            is_digital: row.is_digital,
            email_verified: row.email_verified,
            candidate_type,
            target_role: row
                .target_role
                .filter(|s| !s.trim().is_empty()),
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterCitizenRequest {
    pub name: String,
    pub public_key: String,
    pub role: Option<String>,
    pub birth_place: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IssuePassportRequest {
    pub expires_in_days: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateStatusRequest {
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRoleRequest {
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

#[derive(Debug, Serialize)]
pub struct CitizenResponse {
    pub id: String,
    pub name: String,
    pub public_key: String,
    pub status: String,
    pub role: String,
    pub created_at: i64,
    pub passport_issued: bool,
    pub passport_expires: Option<i64>,
    pub is_digital: bool,
    pub email_verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_role: Option<String>,
}

impl From<Citizen> for CitizenResponse {
    fn from(c: Citizen) -> Self {
        CitizenResponse {
            id: c.id,
            name: c.name,
            public_key: c.public_key,
            status: c.status.as_str().to_string(),
            role: c.role.as_str().to_string(),
            created_at: c.created_at,
            passport_issued: c.passport_issued,
            passport_expires: c.passport_expires,
            is_digital: c.is_digital,
            email_verified: c.email_verified,
            candidate_type: c.candidate_type.map(|t| t.as_str().to_string()),
            target_role: c.target_role,
        }
    }
}

/// Hashed API key record (migration 015).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApiKey {
    pub id: String,
    pub citizen_id: String,
    pub key_hash: String,
    pub role: String,
    pub created_at: chrono::DateTime<Utc>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
    pub is_active: bool,
    pub last_used: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct GenerateApiKeyRequest {
    pub expires_in_days: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyCitizenshipRequest {
    pub name: String,
    pub public_key: String,
    pub reason: String,
    /// Optional hint: "human" | "digital". Final type is auto-detected (headers / behavior / CAPTCHA).
    #[serde(default)]
    pub applicant_type: Option<String>,
    pub email: Option<String>,
    pub birth_place: Option<String>,
    /// Google reCAPTCHA v2 token (required for humans when RECAPTCHA_SECRET_KEY is set).
    pub captcha_token: Option<String>,
    /// Honeypot: must stay empty (bots often fill hidden fields).
    #[serde(default)]
    pub website: Option<String>,
    /// Milliseconds since the form was opened (too-fast submit → bot).
    pub form_opened_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ApplyCitizenshipResponse {
    pub citizen: CitizenResponse,
    pub application_id: String,
    pub message: String,
    /// Present when auto-classified as digital Agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    pub email_verification_required: bool,
}

#[derive(Debug, Deserialize)]
pub struct VerifyEmailRequest {
    pub email: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct VerifyEmailResponse {
    pub citizen_id: String,
    pub email_verified: bool,
    pub message: String,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CitizenshipApplicationRow {
    pub id: String,
    pub citizen_id: String,
    pub citizen_name: String,
    pub citizen_status: String,
    pub citizen_role: String,
    pub is_digital: bool,
    pub email: Option<String>,
    pub reason: String,
    pub applicant_type: String,
    pub status: String,
    pub verified_by: Option<String>,
    pub verified_at: Option<chrono::DateTime<Utc>>,
    pub approved_by: Option<String>,
    pub approved_at: Option<chrono::DateTime<Utc>>,
    pub created_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct VerifyAgentResponse {
    pub citizen: CitizenResponse,
    pub api_key: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct ApproveCitizenResponse {
    pub citizen: CitizenResponse,
    pub passport_id: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub citizen: CitizenResponse,
}

#[derive(Debug, Serialize)]
pub struct RegisterCitizenResponse {
    pub id: String,
    pub name: String,
    pub public_key: String,
    pub role: String,
    pub registration_status: String,
    pub event_id: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct PassportResponse {
    pub citizen_id: String,
    pub passport_id: String,
    pub event_id: String,
    pub event_type: String,
    pub expires_at: Option<i64>,
    pub status: String,
    pub api_key: String,
}

/// Generate a new opaque API key (plaintext; store only the hash).
pub fn generate_random_key() -> String {
    format!("qz_{}", Uuid::new_v4().simple())
}

/// SHA-256 hex digest of an API key.
pub fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

async fn insert_api_key_record(
    pool: &PgPool,
    citizen_id: &str,
    role: &Role,
    expires_in_days: Option<u64>,
) -> Result<(String, ApiKey), CitizenError> {
    let raw_key = generate_random_key();
    let key_hash = hash_key(&raw_key);
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let expires_at = expires_in_days.map(|days| now + Duration::days(days as i64));

    let row = sqlx::query_as::<_, ApiKey>(
        r#"
        INSERT INTO api_keys (id, citizen_id, key_hash, role, created_at, expires_at, is_active, last_used)
        VALUES ($1, $2, $3, $4, $5, $6, TRUE, NULL)
        RETURNING id, citizen_id, key_hash, role, created_at, expires_at, is_active, last_used
        "#,
    )
    .bind(&id)
    .bind(citizen_id)
    .bind(&key_hash)
    .bind(role.as_str())
    .bind(now)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .map_err(|e| CitizenError::DatabaseError(e.to_string()))?;

    Ok((raw_key, row))
}

/// Validate a raw API key against hashed rows; updates `last_used` on success.
pub async fn authenticate_request(pool: &PgPool, raw_key: &str) -> Option<AuthContext> {
    if raw_key.is_empty() {
        return None;
    }

    let key_hash = hash_key(raw_key);
    let row = sqlx::query_as::<_, ApiKey>(
        r#"
        SELECT id, citizen_id, key_hash, role, created_at, expires_at, is_active, last_used
        FROM api_keys
        WHERE key_hash = $1
          AND is_active = TRUE
          AND (expires_at IS NULL OR expires_at > NOW())
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

    let citizen_name: String = sqlx::query_scalar("SELECT name FROM citizens WHERE id = $1")
        .bind(&row.citizen_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;

    let role = Role::from_str(&row.role).unwrap_or(Role::Citizen);
    let agent_permissions = if role.is_agent() {
        list_agent_permissions(pool, &row.citizen_id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Some(
        AuthContext::from_api_key(citizen_name, role)
            .with_citizen_id(row.citizen_id)
            .with_agent_permissions(agent_permissions),
    )
}

#[derive(Debug, Serialize)]
pub struct PendingCitizenEventResponse {
    pub citizen_id: String,
    pub event_id: String,
    pub event_type: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct PendingPassportEventResponse {
    pub citizen_id: String,
    pub passport_id: String,
    pub event_id: String,
    pub event_type: String,
    pub expires_at: Option<i64>,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct CitizensListResponse {
    pub citizens: Vec<CitizenResponse>,
    pub total: usize,
}

#[derive(Debug)]
pub enum CitizenError {
    CitizenAlreadyExists,
    CitizenNotFound,
    ApiKeyNotFound,
    InvalidRole,
    InvalidStatus,
    PassportAlreadyIssued,
    PassportNotFound,
    InsufficientPermissions,
    GovernanceRoleRequiresCandidacy,
    InvalidCitizenName(String),
    InvalidPublicKey,
    DatabaseError(String),
}

impl CitizenError {
    pub fn to_response(&self) -> (StatusCode, Json<ApiResponse>) {
        let (status, message) = match self {
            CitizenError::CitizenAlreadyExists => {
                (StatusCode::CONFLICT, "Гражданин с таким именем уже существует".to_string())
            }
            CitizenError::CitizenNotFound => {
                (StatusCode::NOT_FOUND, "Гражданин не найден".to_string())
            }
            CitizenError::ApiKeyNotFound => {
                (StatusCode::NOT_FOUND, "API-ключ не найден".to_string())
            }
            CitizenError::InvalidRole => {
                (StatusCode::BAD_REQUEST, "Некорректная роль".to_string())
            }
            CitizenError::InvalidStatus => {
                (StatusCode::BAD_REQUEST, "Некорректный статус".to_string())
            }
            CitizenError::PassportAlreadyIssued => {
                (StatusCode::CONFLICT, "Паспорт уже выдан".to_string())
            }
            CitizenError::PassportNotFound => {
                (StatusCode::NOT_FOUND, "Паспорт не найден".to_string())
            }
            CitizenError::InsufficientPermissions => {
                (StatusCode::FORBIDDEN, "Недостаточно прав".to_string())
            }
            CitizenError::GovernanceRoleRequiresCandidacy => (
                StatusCode::BAD_REQUEST,
                "Роли Guardian, Judge и Aiya назначаются только через процесс кандидатуры (nominate → vote → appoint)".to_string(),
            ),
            CitizenError::InvalidCitizenName(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            CitizenError::InvalidPublicKey => (
                StatusCode::BAD_REQUEST,
                "public_key должен быть 32 байта в hex (64 символа, допускается префикс 0x)".to_string(),
            ),
            CitizenError::DatabaseError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Ошибка базы данных: {}", msg),
            ),
        };
        (status, Json(ApiResponse::error(message)))
    }
}

fn reject_node(auth: &AuthContext) -> Option<(StatusCode, Json<ApiResponse>)> {
    if auth.is_node {
        Some(CitizenError::InsufficientPermissions.to_response())
    } else {
        None
    }
}

fn permission_denied(auth: &AuthContext) -> (StatusCode, Json<ApiResponse>) {
    (
        StatusCode::FORBIDDEN,
        Json(ApiResponse::error(
            auth.insufficient_permissions_message().to_string(),
        )),
    )
}

const CITIZEN_SELECT: &str =
    "SELECT id, name, public_key, status, role, created_at, passport_issued, passport_expires, is_digital, email_verified, candidate_type, target_role";

fn parse_applicant_type(raw: &str) -> Option<(String, bool)> {
    match raw.trim().to_lowercase().as_str() {
        "human" => Some(("human".to_string(), false)),
        "digital" => Some(("digital".to_string(), true)),
        _ => None,
    }
}

/// Persist metadata for apply: verification_method + detected_type.
fn apply_detection_meta(is_digital: bool) -> (&'static str, &'static str) {
    if is_digital {
        ("auto", "digital")
    } else {
        ("email", "human")
    }
}

/// Auto-detect digital agent / bot vs human from headers, honeypot, timing, and CAPTCHA outcome.
fn detect_digital_applicant(
    headers: &HeaderMap,
    applicant_type_hint: Option<&str>,
    honeypot: Option<&str>,
    form_opened_ms: Option<u64>,
    captcha_failed: bool,
) -> bool {
    let hint = applicant_type_hint
        .unwrap_or("human")
        .trim()
        .to_lowercase();
    if hint == "digital" {
        return true;
    }
    if honeypot.map(|s| !s.trim().is_empty()).unwrap_or(false) {
        return true;
    }
    if form_opened_ms.map(|ms| ms < 2_000).unwrap_or(false) {
        return true;
    }

    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if ua.is_empty()
        || ua.contains("bot")
        || ua.contains("crawl")
        || ua.contains("spider")
        || ua.contains("curl/")
        || ua.contains("wget")
        || ua.contains("python-requests")
        || ua.contains("httpclient")
        || ua.contains("scrapy")
        || ua.contains("headless")
        || ua.contains("selenium")
        || ua.contains("puppeteer")
    {
        return true;
    }

    if headers.get(axum::http::header::ACCEPT_LANGUAGE).is_none() {
        return true;
    }

    // Failed CAPTCHA (when configured) is treated as automated traffic → digital Agent.
    if captcha_failed {
        return true;
    }

    false
}

fn apply_client_ip(
    headers: &HeaderMap,
    connect: Option<&ConnectInfo<SocketAddr>>,
    trusted: &crate::client_ip::TrustedNets,
) -> String {
    crate::client_ip::resolve_from_connect(headers, connect, trusted)
}

async fn get_citizen_by_name(pool: &PgPool, name: &str) -> Result<Option<Citizen>, CitizenError> {
    let row = sqlx::query_as::<_, CitizenRow>(sqlx::AssertSqlSafe(format!("{CITIZEN_SELECT} FROM citizens WHERE LOWER(name) = LOWER($1)")))
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(|e| CitizenError::DatabaseError(e.to_string()))?;

    // CitizenStatus::Applicant / Pending / Active / Suspended / Revoked via TryFrom.
    row.map(Citizen::try_from).transpose()
}

async fn get_citizen_by_id(pool: &PgPool, id: &str) -> Result<Option<Citizen>, CitizenError> {
    let row = sqlx::query_as::<_, CitizenRow>(sqlx::AssertSqlSafe(format!("{CITIZEN_SELECT} FROM citizens WHERE id = $1")))
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| CitizenError::DatabaseError(e.to_string()))?;

    // Includes CitizenStatus::Pending (pre-passport registrations).
    row.map(Citizen::try_from).transpose()
}

async fn citizen_name_taken(pool: &PgPool, name: &str) -> Result<bool, CitizenError> {
    if get_citizen_by_name(pool, name).await?.is_some() {
        return Ok(true);
    }
    let in_pending: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM pending_events
            WHERE event_data::jsonb->>'event_type' = 'CitizenAdded'
              AND LOWER(event_data::jsonb->'data'->>'citizen_name') = LOWER($1)
        )
        "#,
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .map_err(|e| CitizenError::DatabaseError(e.to_string()))?;
    Ok(in_pending)
}

async fn pending_event_for_citizen(
    pool: &PgPool,
    event_type: &str,
    citizen_id: &str,
) -> Result<bool, CitizenError> {
    let exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM pending_events
            WHERE event_data::jsonb->>'event_type' = $1
              AND event_data::jsonb->'data'->>'citizen_id' = $2
        )
        "#,
    )
    .bind(event_type)
    .bind(citizen_id)
    .fetch_one(pool)
    .await
    .map_err(|e| CitizenError::DatabaseError(e.to_string()))?;
    Ok(exists)
}

async fn get_valid_passport_id(pool: &PgPool, citizen_id: &str) -> Result<Option<String>, CitizenError> {
    sqlx::query_scalar(
        "SELECT id FROM passports WHERE citizen_id = $1 AND is_valid = TRUE ORDER BY issued_at DESC LIMIT 1",
    )
    .bind(citizen_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| CitizenError::DatabaseError(e.to_string()))
}

async fn submit_pending_event(
    state: Arc<AppState>,
    event: Event,
) -> Result<(), (StatusCode, Json<ApiResponse>)> {
    if let Err(e) = EventValidator::validate_event(&event, &state.db).await {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(e.message())),
        ));
    }

    match pending::insert(&state.db, &event).await {
        Ok(pending::PendingInsertResult::Inserted) => {}
        Ok(pending::PendingInsertResult::AlreadyExists) => {
            return Ok(());
        }
        Err(e) => {
            return Err(CitizenError::DatabaseError(format!("Не удалось добавить событие: {}", e))
                .to_response());
        }
    }

    let state_gossip = state.clone();
    let event_gossip = event.clone();
    tokio::spawn(async move {
        crate::gossip::push_pending_to_peers(&state_gossip, &event_gossip).await;
    });

    let state_clone = state.clone();
    tokio::spawn(async move {
        block_producer::try_create_block(state_clone).await;
    });

    Ok(())
}

pub async fn register_citizen(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<RegisterCitizenRequest>,
) -> impl IntoResponse {
    if auth.is_node || !auth.can_register_citizens() {
        return permission_denied(&auth);
    }

    let name = match normalize_and_validate_citizen_name(&req.name) {
        Ok(n) => n,
        Err(msg) => return CitizenError::InvalidCitizenName(msg.to_string()).to_response(),
    };

    let public_key = match crate::crypto::validate_public_key_hex(&req.public_key) {
        Ok(key) => key,
        Err(_) => return CitizenError::InvalidPublicKey.to_response(),
    };

    if citizen_name_taken(&state.db, &name).await.unwrap_or(false) {
        return CitizenError::CitizenAlreadyExists.to_response();
    }

    // Default Citizen on registration is membership, not governance role assignment.
    let role = match req.role {
        Some(r) => match Role::from_str(&r) {
            Some(role) => role,
            None => return CitizenError::InvalidRole.to_response(),
        },
        None => Role::Citizen,
    };

    if role.is_agent() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Роль Agent назначается только администратором".to_string(),
            )),
        );
    }

    if role.is_mayor() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Роль Mayor назначается только через выборы мэра (/mayor/*)".to_string(),
            )),
        );
    }

    if role.is_governance() {
        return CitizenError::GovernanceRoleRequiresCandidacy.to_response();
    }

    let citizen_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();
    let birth_place = req
        .birth_place
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Quazar".to_string());
    let event_id = format!("citizen_add_{}", citizen_id);

    let mut event = build_citizen_added_event(
        &event_id,
        &citizen_id,
        &name,
        &public_key,
        &birth_place,
        role.as_str(),
        &auth.citizen_name,
        now,
    );
    event.hash = Some(compute_event_hash(&event));
    event.signatures = vec![registration_signature(
        &citizen_id,
        &public_key,
        event.hash.as_deref().unwrap_or(""),
    )];

    // Provisional citizen row (status=pending) before block projection.
    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO citizens (id, name, public_key, status, role, created_at, passport_issued)
        VALUES ($1, $2, $3, $4, $5, $6, FALSE)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(&citizen_id)
    .bind(&name)
    .bind(&public_key)
    .bind(CitizenStatus::Pending.as_str())
    .bind(role.as_str())
    .bind(now)
    .execute(&state.db)
    .await
    {
        return CitizenError::DatabaseError(e.to_string()).to_response();
    }

    if let Err(resp) = submit_pending_event(state.clone(), event).await {
        return resp;
    }

    let citizen = CitizenResponse {
        id: citizen_id,
        name,
        public_key,
        status: CitizenStatus::Pending.as_str().to_string(),
        role: role.as_str().to_string(),
        created_at: now,
        passport_issued: false,
        passport_expires: None,
        is_digital: false,
        email_verified: false,
        candidate_type: None,
        target_role: None,
    };

    (
        StatusCode::ACCEPTED,
        Json(ApiResponse::success(RegisterResponse { citizen })),
    )
}

pub async fn list_citizens(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> impl IntoResponse {
    if let Some(resp) = reject_node(&auth) {
        return resp;
    }

    let rows = match sqlx::query_as::<_, CitizenRow>(sqlx::AssertSqlSafe(format!("{CITIZEN_SELECT} FROM citizens ORDER BY created_at DESC")))
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => return CitizenError::DatabaseError(e.to_string()).to_response(),
    };

    let mut citizens = Vec::new();
    for row in rows {
        match Citizen::try_from(row) {
            Ok(c) => citizens.push(CitizenResponse::from(c)),
            Err(e) => return e.to_response(),
        }
    }

    let response = CitizensListResponse {
        total: citizens.len(),
        citizens,
    };
    (StatusCode::OK, Json(ApiResponse::success(response)))
}

pub async fn get_citizen(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = reject_node(&auth) {
        return resp;
    }

    let citizen = match get_citizen_by_id(&state.db, &id).await {
        Ok(Some(c)) => c,
        Ok(None) => return CitizenError::CitizenNotFound.to_response(),
        Err(e) => return e.to_response(),
    };

    (StatusCode::OK, Json(ApiResponse::success(CitizenResponse::from(citizen))))
}

pub async fn update_status(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(req): Json<UpdateStatusRequest>,
) -> impl IntoResponse {
    if !auth.can_update_citizen_status() {
        return permission_denied(&auth);
    }

    let status = match CitizenStatus::from_str(&req.status) {
        Some(s) => s,
        None => return CitizenError::InvalidStatus.to_response(),
    };

    let citizen = match get_citizen_by_id(&state.db, &id).await {
        Ok(Some(c)) => c,
        Ok(None) => return CitizenError::CitizenNotFound.to_response(),
        Err(e) => return e.to_response(),
    };

    if citizen.status == status {
        return (
            StatusCode::OK,
            Json(ApiResponse::success(CitizenResponse::from(citizen))),
        );
    }

    let now = chrono::Utc::now().timestamp();
    let event_id = format!("citizen_status_{}_{}", citizen.id, now);
    let event = build_signed_citizen_status_event(
        &event_id,
        &citizen.id,
        &citizen.name,
        status.as_str(),
        &auth.citizen_name,
        now,
    );

    if let Err(resp) = submit_pending_event(state.clone(), event.clone()).await {
        return resp;
    }

    // Applicants / Candidates never hold API keys.
    if status.is_applicant() || citizen.role.is_candidate() {
        let _ = sqlx::query(
            "UPDATE api_keys SET is_active = FALSE WHERE citizen_id = $1 AND is_active = TRUE",
        )
        .bind(&citizen.id)
        .execute(&state.db)
        .await;
    }

    (
        StatusCode::ACCEPTED,
        Json(ApiResponse::success(PendingCitizenEventResponse {
            citizen_id: citizen.id,
            event_id: event.event_id,
            event_type: event.event_type,
            status: "pending".to_string(),
        })),
    )
}

pub async fn update_role(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(req): Json<UpdateRoleRequest>,
) -> impl IntoResponse {
    if !auth.can_change_citizen_role() {
        return permission_denied(&auth);
    }

    let new_role = match Role::from_str(&req.role) {
        Some(role) => role,
        None => return CitizenError::InvalidRole.to_response(),
    };

    let mut citizen = match get_citizen_by_id(&state.db, &id).await {
        Ok(Some(c)) => c,
        Ok(None) => return CitizenError::CitizenNotFound.to_response(),
        Err(e) => return e.to_response(),
    };

    if citizen.role == new_role {
        return (
            StatusCode::OK,
            Json(ApiResponse::success(CitizenResponse::from(citizen))),
        );
    }

    if new_role.is_mayor() && citizen.candidate_type.as_ref() != Some(&CandidateType::Role) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Роль Mayor назначается только через выборы мэра (/mayor/*)".to_string(),
            )),
        );
    }

    if new_role.is_governance()
        && citizen.candidate_type.as_ref() != Some(&CandidateType::Role)
    {
        return CitizenError::GovernanceRoleRequiresCandidacy.to_response();
    }

    if !auth.can_assign_role(
        citizen.candidate_type.as_ref(),
        citizen.target_role.as_deref(),
        &new_role,
    ) {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiResponse::error(
                "Нельзя назначить эту роль: ограничение типа кандидатуры / иерархии".to_string(),
            )),
        );
    }

    let now = chrono::Utc::now().timestamp();
    let event_id = format!("citizen_role_{}_{}", citizen.id, now);
    let event = build_signed_citizen_role_event(
        &event_id,
        &citizen.id,
        &citizen.name,
        new_role.as_str(),
        &auth.citizen_name,
        now,
    );

    if let Err(resp) = submit_pending_event(state.clone(), event).await {
        return resp;
    }

    // Clear candidacy metadata once a final role is granted.
    if citizen.candidate_type.is_some() {
        let _ = sqlx::query(
            "UPDATE citizens SET candidate_type = NULL, target_role = NULL WHERE id = $1",
        )
        .bind(&citizen.id)
        .execute(&state.db)
        .await;
        citizen.candidate_type = None;
        citizen.target_role = None;
    }

    citizen.role = new_role;
    (
        StatusCode::OK,
        Json(ApiResponse::success(CitizenResponse::from(citizen))),
    )
}

pub async fn issue_passport(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(req): Json<IssuePassportRequest>,
) -> impl IntoResponse {
    if !auth.can_manage_citizens() {
        return permission_denied(&auth);
    }

    let citizen = match get_citizen_by_id(&state.db, &id).await {
        Ok(Some(c)) => c,
        Ok(None) => return CitizenError::CitizenNotFound.to_response(),
        Err(e) => return e.to_response(),
    };

    if citizen.status.is_applicant() || citizen.role.is_candidate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Candidate ещё не проверен: сначала verify (СБ), затем approve (судья) / паспорт"
                    .to_string(),
            )),
        );
    }

    if citizen.passport_issued {
        return CitizenError::PassportAlreadyIssued.to_response();
    }

    if pending_event_for_citizen(&state.db, "PassportIssued", &citizen.id)
        .await
        .unwrap_or(false)
    {
        return CitizenError::PassportAlreadyIssued.to_response();
    }

    let passport_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();
    let expires_in_days = req.expires_in_days.unwrap_or(365);
    let expires_at = now + (expires_in_days * 24 * 60 * 60) as i64;
    let event_id = format!("passport_issue_{}", passport_id);

    let event = build_signed_passport_issued_event(
        &event_id,
        &passport_id,
        &citizen.id,
        &citizen.name,
        now,
        expires_at,
        &auth.citizen_name,
        now,
    );

    if let Err(resp) = submit_pending_event(state.clone(), event.clone()).await {
        return resp;
    }

    // API key is issued together with the passport (not at registration).
    let (raw_api_key, _) =
        match insert_api_key_record(&state.db, &citizen.id, &citizen.role, None).await {
            Ok(v) => v,
            Err(e) => return e.to_response(),
        };

    (
        StatusCode::ACCEPTED,
        Json(ApiResponse::success(PassportResponse {
            citizen_id: citizen.id,
            passport_id,
            event_id: event.event_id,
            event_type: event.event_type,
            expires_at: Some(expires_at),
            status: "pending".to_string(),
            api_key: raw_api_key,
        })),
    )
}

pub async fn revoke_passport(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if !auth.can_manage_citizens() {
        return permission_denied(&auth);
    }

    let citizen = match get_citizen_by_id(&state.db, &id).await {
        Ok(Some(c)) => c,
        Ok(None) => return CitizenError::CitizenNotFound.to_response(),
        Err(e) => return e.to_response(),
    };

    if !citizen.passport_issued {
        return CitizenError::PassportNotFound.to_response();
    }

    if pending_event_for_citizen(&state.db, "PassportRevoked", &citizen.id)
        .await
        .unwrap_or(false)
    {
        return (
            StatusCode::CONFLICT,
            Json(ApiResponse::error("Аннулирование паспорта уже ожидает подтверждения")),
        );
    }

    let passport_id = match get_valid_passport_id(&state.db, &citizen.id).await {
        Ok(Some(id)) => id,
        Ok(None) => return CitizenError::PassportNotFound.to_response(),
        Err(e) => return e.to_response(),
    };

    let now = chrono::Utc::now().timestamp();
    let event_id = format!("passport_revoke_{}", passport_id);
    let event = build_signed_passport_revoked_event(
        &event_id,
        &passport_id,
        &citizen.id,
        &citizen.name,
        &auth.citizen_name,
        now,
    );

    if let Err(resp) = submit_pending_event(state, event.clone()).await {
        return resp;
    }

    (
        StatusCode::ACCEPTED,
        Json(ApiResponse::success(PendingPassportEventResponse {
            citizen_id: citizen.id,
            passport_id,
            event_id: event.event_id,
            event_type: event.event_type,
            expires_at: None,
            status: "pending".to_string(),
        })),
    )
}

pub async fn search_citizens(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    if let Some(resp) = reject_node(&auth) {
        return resp;
    }

    let search_pattern = format!("%{}%", query.q);
    let rows = match sqlx::query_as::<_, CitizenRow>(sqlx::AssertSqlSafe(format!("{CITIZEN_SELECT} FROM citizens WHERE name LIKE $1 ORDER BY created_at DESC")))
    .bind(search_pattern)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => return CitizenError::DatabaseError(e.to_string()).to_response(),
    };

    let mut citizens = Vec::new();
    for row in rows {
        match Citizen::try_from(row) {
            Ok(c) => citizens.push(CitizenResponse::from(c)),
            Err(e) => return e.to_response(),
        }
    }

    let response = CitizensListResponse {
        total: citizens.len(),
        citizens,
    };
    (StatusCode::OK, Json(ApiResponse::success(response)))
}

async fn resolve_auth_citizen_id(pool: &PgPool, auth: &AuthContext) -> Result<String, CitizenError> {
    if auth.is_node {
        return Err(CitizenError::InsufficientPermissions);
    }
    get_citizen_by_name(pool, &auth.citizen_name)
        .await?
        .map(|c| c.id)
        .ok_or(CitizenError::CitizenNotFound)
}

/// POST /citizen/key — generate a new API key for the authenticated citizen.
pub async fn generate_api_key(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<GenerateApiKeyRequest>,
) -> impl IntoResponse {
    if let Some(resp) = reject_node(&auth) {
        return resp;
    }

    let citizen = match get_citizen_by_name(&state.db, &auth.citizen_name).await {
        Ok(Some(c)) => c,
        Ok(None) => return CitizenError::CitizenNotFound.to_response(),
        Err(e) => return e.to_response(),
    };

    if !citizen.status.can_hold_api_key() || citizen.role.is_candidate() {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiResponse::error(
                "Candidate не имеет API-ключа и доступа к системе".to_string(),
            )),
        );
    }

    let (raw_key, record) =
        match insert_api_key_record(&state.db, &citizen.id, &citizen.role, req.expires_in_days)
            .await
        {
            Ok(v) => v,
            Err(e) => return e.to_response(),
        };

    (
        StatusCode::CREATED,
        Json(ApiResponse::success(serde_json::json!({
            "api_key": raw_key,
            "id": record.id,
            "citizen_id": record.citizen_id,
            "role": record.role,
            "created_at": record.created_at,
            "expires_at": record.expires_at,
            "is_active": record.is_active,
        }))),
    )
}

/// GET /citizen/keys — list API keys for the authenticated citizen.
pub async fn list_api_keys(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> impl IntoResponse {
    if let Some(resp) = reject_node(&auth) {
        return resp;
    }

    let citizen_id = match resolve_auth_citizen_id(&state.db, &auth).await {
        Ok(id) => id,
        Err(e) => return e.to_response(),
    };

    let keys = match sqlx::query_as::<_, ApiKey>(
        r#"
        SELECT id, citizen_id, key_hash, role, created_at, expires_at, is_active, last_used
        FROM api_keys
        WHERE citizen_id = $1
        ORDER BY created_at DESC
        "#,
    )
    .bind(&citizen_id)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => return CitizenError::DatabaseError(e.to_string()).to_response(),
    };

    (StatusCode::OK, Json(ApiResponse::success(keys)))
}

/// DELETE /citizen/key/:id — revoke an API key owned by the authenticated citizen.
pub async fn revoke_api_key(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = reject_node(&auth) {
        return resp;
    }

    let citizen_id = match resolve_auth_citizen_id(&state.db, &auth).await {
        Ok(id) => id,
        Err(e) => return e.to_response(),
    };

    let result = match sqlx::query(
        r#"
        UPDATE api_keys
        SET is_active = FALSE
        WHERE id = $1 AND citizen_id = $2 AND is_active = TRUE
        "#,
    )
    .bind(&id)
    .bind(&citizen_id)
    .execute(&state.db)
    .await
    {
        Ok(r) => r,
        Err(e) => return CitizenError::DatabaseError(e.to_string()).to_response(),
    };

    if result.rows_affected() == 0 {
        return CitizenError::ApiKeyNotFound.to_response();
    }

    (
        StatusCode::OK,
        Json(ApiResponse::success(serde_json::json!({
            "id": id,
            "revoked": true,
        }))),
    )
}

#[derive(Debug, Serialize)]
pub struct PublicStatsResponse {
    citizens_with_passport: i64,
}

pub async fn public_stats_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let count: i64 = match sqlx::query_scalar(
        "SELECT COUNT(*) FROM citizens WHERE passport_issued = TRUE",
    )
    .fetch_one(&state.db)
    .await
    {
        Ok(count) => count,
        Err(e) => {
            tracing::error!(error = %e, "failed to count citizens with passport");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::error("Database error")),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(ApiResponse::success(PublicStatsResponse {
            citizens_with_passport: count,
        })),
    )
        .into_response()
}

/// Read-only citizen directory for verified-email web sessions (candidates).
/// Safe fields only: id, name, role — no email, keys, or secrets.
#[derive(Debug, Deserialize)]
pub struct ListCitizensSessionRequest {
    pub email: String,
    /// Must be true; server still requires a live register-verify mark (peek).
    #[serde(default)]
    pub email_already_verified: Option<bool>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct PublicCitizenRow {
    id: String,
    name: String,
    role: String,
}

#[derive(Debug, Serialize)]
pub struct PublicCitizenItem {
    pub id: String,
    pub name: String,
    pub role: String,
}

#[derive(Debug, Serialize)]
pub struct PublicCitizensResponse {
    pub total: usize,
    pub citizens: Vec<PublicCitizenItem>,
}

/// POST /api/citizens — list passport holders for a verified email session.
///
/// Auth mirrors `/api/my-applications`: `email` + `email_already_verified: true` and a live
/// in-memory register-verify mark. Citizens are refused (`citizen_use_app`).
pub async fn session_citizens_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ListCitizensSessionRequest>,
) -> impl IntoResponse {
    let email = normalize_email(&req.email);
    if !is_valid_email(&email) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error("Введите корректный email-адрес")),
        )
            .into_response();
    }

    match email_belongs_to_citizen(&state.db, &email).await {
        Ok(true) => {
            clear_recent_email_verification(&email);
            return citizen_use_app_forbidden();
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "failed to check citizen email for /api/citizens");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::error("Database error")),
            )
                .into_response();
        }
    }

    let client_claims_verified = req.email_already_verified.unwrap_or(false);
    if !client_claims_verified || !peek_recent_email_verification(&email) {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiResponse::error(
                "Сначала подтвердите email через вход на сайте, затем повторите запрос",
            )),
        )
            .into_response();
    }

    let rows = match sqlx::query_as::<_, PublicCitizenRow>(
        "SELECT id, name, role FROM citizens \
         WHERE passport_issued = TRUE \
         ORDER BY name ASC",
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "failed to list session citizens");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::error("Database error")),
            )
                .into_response();
        }
    };

    let citizens: Vec<PublicCitizenItem> = rows
        .into_iter()
        .map(|r| PublicCitizenItem {
            id: r.id,
            name: r.name,
            role: r.role,
        })
        .collect();

    (
        StatusCode::OK,
        Json(ApiResponse::success(PublicCitizensResponse {
            total: citizens.len(),
            citizens,
        })),
    )
        .into_response()
}

// --- Citizenship application workflow: Candidate → Agent → Citizen ---

/// POST /citizen/apply and /citizen/apply/citizenship — public.
/// Auto-classifies bot → Agent, human → Candidate (citizenship) (+ email code).
pub async fn apply_for_citizenship(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<SocketAddr>>,
    Json(req): Json<ApplyCitizenshipRequest>,
) -> Response {
    let name = match normalize_and_validate_citizen_name(&req.name) {
        Ok(n) => n,
        Err(msg) => {
            return CitizenError::InvalidCitizenName(msg.to_string())
                .to_response()
                .into_response();
        }
    };
    let reason = req.reason.trim().to_string();
    let email_raw = req
        .email
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if reason.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error("Укажите причину / чем можете быть полезны".to_string())),
        )
            .into_response();
    }

    let captcha_token = req.captcha_token.as_deref().unwrap_or("");
    // Production without RECAPTCHA_SECRET_KEY must fail before digital/human branching.
    if let Err(msg) = crate::auth::ensure_recaptcha_secret() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiResponse::error(msg))).into_response();
    }
    let captcha_failed = if recaptcha_required() {
        verify_recaptcha(captcha_token).await.is_err()
    } else {
        false
    };

    let is_digital = detect_digital_applicant(
        &headers,
        req.applicant_type.as_deref(),
        req.website.as_deref(),
        req.form_opened_ms,
        captcha_failed,
    );
    let applicant_type = if is_digital { "digital" } else { "human" };

    // Humans must pass CAPTCHA when required (bots take the digital Agent path instead).
    if !is_digital && recaptcha_required() {
        if let Err(msg) = verify_recaptcha(captcha_token).await {
            return (StatusCode::BAD_REQUEST, Json(ApiResponse::error(msg))).into_response();
        }
    }

    let email = if is_digital {
        email_raw.map(|e| normalize_email(&e))
    } else {
        let Some(raw) = email_raw else {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::error(
                    "Для людей обязателен email — на него придёт код подтверждения".to_string(),
                )),
            )
                .into_response();
        };
        let normalized = normalize_email(&raw);
        if !is_valid_email(&normalized) {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::error("Введите корректный email-адрес".to_string())),
            )
                .into_response();
        }
        Some(normalized)
    };

    let public_key = match crate::crypto::validate_public_key_hex(&req.public_key) {
        Ok(key) => key,
        Err(_) => return CitizenError::InvalidPublicKey.to_response().into_response(),
    };

    if citizen_name_taken(&state.db, &name).await.unwrap_or(false) {
        return CitizenError::CitizenAlreadyExists.to_response().into_response();
    }

    let citizen_id = Uuid::new_v4().to_string();
    let application_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let birth_place = req
        .birth_place
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Quazar".to_string());

    let (role, status, email_verified, cand_type, cand_target) = if is_digital {
        (Role::Agent, CitizenStatus::Active, true, None, None)
    } else {
        (
            Role::Candidate,
            CitizenStatus::Pending,
            false,
            Some(CandidateType::Citizenship.as_str()),
            Some(Role::Citizen.as_str()),
        )
    };
    let (verification_method, detected_type) = apply_detection_meta(is_digital);

    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO citizens (
            id, name, public_key, status, role, created_at, passport_issued,
            is_digital, email_verified, email, candidate_type, target_role
        )
        VALUES ($1, $2, $3, $4, $5, $6, FALSE, $7, $8, $9, $10, $11)
        "#,
    )
    .bind(&citizen_id)
    .bind(&name)
    .bind(&public_key)
    .bind(status.as_str())
    .bind(role.as_str())
    .bind(now)
    .bind(is_digital)
    .bind(email_verified)
    .bind(&email)
    .bind(cand_type)
    .bind(cand_target)
    .execute(&state.db)
    .await
    {
        return CitizenError::DatabaseError(e.to_string())
            .to_response()
            .into_response();
    }

    let app_status = if is_digital { "Verified" } else { "Submitted" };
    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO citizenship_applications (
            id, citizen_id, email, reason, status, applicant_type,
            verification_method, detected_type
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(&application_id)
    .bind(&citizen_id)
    .bind(&email)
    .bind(&reason)
    .bind(app_status)
    .bind(applicant_type)
    .bind(verification_method)
    .bind(detected_type)
    .execute(&state.db)
    .await
    {
        let _ = sqlx::query("DELETE FROM citizens WHERE id = $1")
            .bind(&citizen_id)
            .execute(&state.db)
            .await;
        return CitizenError::DatabaseError(e.to_string())
            .to_response()
            .into_response();
    }

    let event_id = format!("citizen_apply_{}", citizen_id);
    let mut event = build_citizen_added_event(
        &event_id,
        &citizen_id,
        &name,
        &public_key,
        &birth_place,
        role.as_str(),
        "public_apply",
        now,
    );
    event.hash = Some(compute_event_hash(&event));
    event.signatures = vec![registration_signature(
        &citizen_id,
        &public_key,
        event.hash.as_deref().unwrap_or(""),
    )];
    let _ = submit_pending_event(state.clone(), event).await;

    let mut api_key = None;
    let message;
    let email_verification_required = !is_digital;

    if is_digital {
        match insert_api_key_record(&state.db, &citizen_id, &Role::Agent, None).await {
            Ok((raw, _)) => {
                if let Err(e) =
                    grant_default_agent_permissions(&state.db, &citizen_id, Some("system")).await
                {
                    tracing::warn!(
                        citizen = %citizen_id,
                        error = %e,
                        "failed to grant default agent permissions"
                    );
                }
                api_key = Some(raw);
                message = "Цифровой агент зарегистрирован как Agent. API-ключ выдан.".to_string();
            }
            Err(e) => {
                return e.to_response().into_response();
            }
        }
    } else {
        let trusted = state.trusted_proxies.snapshot().await;
        let ip = apply_client_ip(&headers, connect.as_ref(), &trusted);
        let email_addr = email.clone().expect("human email required");
        match issue_verification_code(&email_addr, &ip).await {
            Ok(code) => {
                let expires = Utc::now() + Duration::minutes(10);
                if let Err(e) = sqlx::query(
                    r#"
                    UPDATE citizens
                    SET verification_code = $1, verification_expires = $2
                    WHERE id = $3
                    "#,
                )
                .bind(&code)
                .bind(expires)
                .bind(&citizen_id)
                .execute(&state.db)
                .await
                {
                    return CitizenError::DatabaseError(e.to_string())
                        .to_response()
                        .into_response();
                }
                message =
                    "Заявка принята как Candidate (citizenship). Подтвердите email кодом из письма (POST /citizen/verify-email)."
                        .to_string();
            }
            Err(err) => {
                tracing::warn!(citizen = %name, error = %err, "failed to send citizenship email code");
                message = format!(
                    "Заявка принята как Candidate (citizenship). Не удалось отправить код на email ({err}). Запросите код повторно через /auth/register/send-code."
                );
            }
        }
    }

    tracing::info!(
        citizen = %name,
        application = %application_id,
        applicant_type,
        detected_type,
        verification_method,
        is_digital,
        role = %role.as_str(),
        candidate_type = ?cand_type,
        "citizenship application submitted"
    );

    (
        StatusCode::CREATED,
        Json(ApiResponse::success(ApplyCitizenshipResponse {
            citizen: CitizenResponse {
                id: citizen_id,
                name,
                public_key,
                status: status.as_str().to_string(),
                role: role.as_str().to_string(),
                created_at: now,
                passport_issued: false,
                passport_expires: None,
                is_digital,
                email_verified,
                candidate_type: cand_type.map(|s| s.to_string()),
                target_role: cand_target.map(|s| s.to_string()),
            },
            application_id,
            message,
            api_key,
            email_verification_required,
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct ApplyRoleRequest {
    pub target_role: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApplyRoleResponse {
    pub candidacy: crate::candidacy::CandidacyRow,
    pub message: String,
}

/// POST /citizen/apply/role — authenticated Citizen+. Creates a governance candidacy
/// that requires voting (nominate → vote → approve → appoint). Does not assign the role.
pub async fn apply_for_role(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<ApplyRoleRequest>,
) -> Response {
    if auth.is_node || !auth.can_vote() {
        return permission_denied(&auth).into_response();
    }

    let target_role = req.target_role.trim().to_string();
    if !is_role_candidacy_target(&target_role) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "target_role must be Citizen, Guardian, Judge, Aiya, or Mayor".to_string(),
            )),
        )
            .into_response();
    }

    let applicant_id = auth.resolve_account_id(&state.db).await;
    let citizen = match get_citizen_by_id(&state.db, &applicant_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return CitizenError::CitizenNotFound.to_response().into_response(),
        Err(e) => return e.to_response().into_response(),
    };

    if citizen.status != CitizenStatus::Active {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Подать заявку на роль может только активный гражданин".to_string(),
            )),
        )
            .into_response();
    }
    if citizen.role.is_candidate() || citizen.role.is_agent() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Заявка на роль доступна гражданам с полным статусом Citizen и выше".to_string(),
            )),
        )
            .into_response();
    }
    if citizen.candidate_type.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "У вас уже есть активная кандидатура".to_string(),
            )),
        )
            .into_response();
    }

    let _ = req.reason; // optional audit note; nomination event carries target_role

    match crate::candidacy::nominate_candidate(
        state,
        &auth,
        crate::candidacy::NominateRequest {
            candidate_id: applicant_id,
            target_role,
        },
    )
    .await
    {
        Ok(row) => (
            StatusCode::ACCEPTED,
            Json(ApiResponse::success(ApplyRoleResponse {
                message: format!(
                    "Кандидатура на роль {} создана. Требуется голосование граждан, затем назначение Айей.",
                    row.target_role
                ),
                candidacy: row,
            })),
        )
            .into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

/// POST /citizen/verify-email — confirm Candidate email with 6-digit code (public).
pub async fn verify_citizenship_email(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerifyEmailRequest>,
) -> Response {
    let email = normalize_email(&req.email);
    let code = req.code.trim().to_string();

    if !is_valid_email(&email) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error("Введите корректный email-адрес".to_string())),
        )
            .into_response();
    }
    if !code.chars().all(|c| c.is_ascii_digit()) || code.len() != 6 {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error("Код должен состоять из 6 цифр".to_string())),
        )
            .into_response();
    }

    let row: Option<(
        String,
        String,
        bool,
        Option<String>,
        Option<chrono::DateTime<Utc>>,
    )> = sqlx::query_as(
        r#"
        SELECT id, role, email_verified, verification_code, verification_expires
        FROM citizens
        WHERE lower(email) = $1
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(&email)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let Some((citizen_id, role, already, stored_code, expires)) = row else {
        // Нет заявки: принимаем код из POST /auth/register/send-code (in-memory).
        return match consume_verification_code(&email, &code) {
            Ok(()) => (
                StatusCode::OK,
                Json(ApiResponse::success(VerifyEmailResponse {
                    citizen_id: String::new(),
                    email_verified: true,
                    message: "Email подтверждён".to_string(),
                })),
            )
                .into_response(),
            Err(err) => (
                StatusCode::BAD_REQUEST,
                Json(ApiResponse::error(err)),
            )
                .into_response(),
        };
    };

    if already {
        return (
            StatusCode::OK,
            Json(ApiResponse::success(VerifyEmailResponse {
                citizen_id,
                email_verified: true,
                message: "Email уже подтверждён".to_string(),
            })),
        )
            .into_response();
    }

    if role != Role::Candidate.as_str() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Подтверждение email доступно только для роли Candidate".to_string(),
            )),
        )
            .into_response();
    }

    let db_unexpired = expires.map(|e| e > Utc::now()).unwrap_or(false);
    let db_code_ok = stored_code.as_ref().map(|c| c == &code).unwrap_or(false) && db_unexpired;

    if !db_code_ok {
        // Fallback: code issued via /auth/register/send-code (in-memory).
        if consume_verification_code(&email, &code).is_err() {
            let err = if stored_code.is_none() {
                "Код не найден. Запросите новый код.".to_string()
            } else if !db_unexpired {
                "Код истёк. Запросите новый код.".to_string()
            } else {
                "Неверный код. Проверьте письмо и попробуйте снова.".to_string()
            };
            return (StatusCode::BAD_REQUEST, Json(ApiResponse::error(err))).into_response();
        }
    }

    if let Err(e) = sqlx::query(
        r#"
        UPDATE citizens
        SET email_verified = TRUE,
            verification_code = NULL,
            verification_expires = NULL
        WHERE id = $1
        "#,
    )
    .bind(&citizen_id)
    .execute(&state.db)
    .await
    {
        return CitizenError::DatabaseError(e.to_string())
            .to_response()
            .into_response();
    }

    tracing::info!(citizen_id = %citizen_id, email = %email, "citizenship email verified");

    (
        StatusCode::OK,
        Json(ApiResponse::success(VerifyEmailResponse {
            citizen_id,
            email_verified: true,
            message: "Email подтверждён. Роль Candidate — ожидайте проверки службы безопасности."
                .to_string(),
        })),
    )
        .into_response()
}

/// POST /citizen/:id/verify — Candidate → Agent + API key (Founder / Aiya / Guardian).
pub async fn verify_agent(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if !auth.can_verify_agent() {
        return permission_denied(&auth);
    }

    let mut citizen = match get_citizen_by_id(&state.db, &id).await {
        Ok(Some(c)) => c,
        Ok(None) => return CitizenError::CitizenNotFound.to_response(),
        Err(e) => return e.to_response(),
    };

    // Digital applicants are already Agent (+ API key) from /citizen/apply.
    if citizen.is_digital {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Цифровой агент уже имеет роль Agent; проверка СБ не требуется".to_string(),
            )),
        );
    }

    // Human flow: Candidate (citizenship) → Agent after email verify.
    let is_candidate = citizen.role == Role::Candidate || citizen.status.is_applicant();
    if !is_candidate {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Проверка СБ доступна только для роли Candidate".to_string(),
            )),
        );
    }

    if citizen.candidate_type.as_ref() == Some(&CandidateType::Role) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Candidate (role) проходит через appoint, не через проверку СБ".to_string(),
            )),
        );
    }

    if citizen.role == Role::Candidate && !citizen.email_verified {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Сначала подтвердите email (POST /citizen/verify-email)".to_string(),
            )),
        );
    }

    let applicant_type: String = sqlx::query_scalar(
        "SELECT COALESCE(detected_type, applicant_type) FROM citizenship_applications WHERE citizen_id = $1",
    )
    .bind(&citizen.id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| "human".to_string());
    let is_digital = applicant_type == "digital";

    let now = Utc::now();
    // Preserve citizenship candidacy metadata through Agent until Judge approve.
    if let Err(e) = sqlx::query(
        r#"
        UPDATE citizens
        SET status = $1,
            role = $2,
            is_digital = $3,
            candidate_type = COALESCE(candidate_type, $4),
            target_role = COALESCE(target_role, $5)
        WHERE id = $6
        "#,
    )
    .bind(CitizenStatus::Active.as_str())
    .bind(Role::Agent.as_str())
    .bind(is_digital)
    .bind(CandidateType::Citizenship.as_str())
    .bind(Role::Citizen.as_str())
    .bind(&citizen.id)
    .execute(&state.db)
    .await
    {
        return CitizenError::DatabaseError(e.to_string()).to_response();
    }

    let _ = sqlx::query(
        r#"
        UPDATE citizenship_applications
        SET status = 'Verified', verified_by = $1, verified_at = $2, applicant_type = $3
        WHERE citizen_id = $4 AND status = 'Submitted'
        "#,
    )
    .bind(&auth.citizen_name)
    .bind(now)
    .bind(&applicant_type)
    .bind(&citizen.id)
    .execute(&state.db)
    .await;

    let (raw_api_key, _) =
        match insert_api_key_record(&state.db, &citizen.id, &Role::Agent, None).await {
            Ok(v) => v,
            Err(e) => return e.to_response(),
        };

    if let Err(e) =
        grant_default_agent_permissions(&state.db, &citizen.id, Some(&auth.citizen_name)).await
    {
        tracing::warn!(
            citizen = %citizen.id,
            error = %e,
            "failed to grant default agent permissions after verify"
        );
    }

    let role_event_id = format!("citizen_role_{}_{}", citizen.id, now.timestamp());
    let role_event = build_signed_citizen_role_event(
        &role_event_id,
        &citizen.id,
        &citizen.name,
        Role::Agent.as_str(),
        &auth.citizen_name,
        now.timestamp(),
    );
    let _ = submit_pending_event(state.clone(), role_event).await;

    citizen.status = CitizenStatus::Active;
    citizen.role = Role::Agent;
    citizen.is_digital = is_digital;
    if citizen.candidate_type.is_none() {
        citizen.candidate_type = Some(CandidateType::Citizenship);
        citizen.target_role = Some(Role::Citizen.as_str().to_string());
    }

    (
        StatusCode::OK,
        Json(ApiResponse::success(VerifyAgentResponse {
            citizen: CitizenResponse::from(citizen),
            api_key: raw_api_key,
            message: format!(
                "Проверка СБ пройдена. Выдан ключ Agent ({applicant_type})."
            ),
        })),
    )
}

/// POST /citizen/:id/approve — Agent → Citizen + passport (Judge).
pub async fn approve_citizen(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if !auth.can_approve_citizen() {
        return permission_denied(&auth);
    }

    let mut citizen = match get_citizen_by_id(&state.db, &id).await {
        Ok(Some(c)) => c,
        Ok(None) => return CitizenError::CitizenNotFound.to_response(),
        Err(e) => return e.to_response(),
    };

    if citizen.role != Role::Agent {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Утверждение судьёй доступно только для роли Agent".to_string(),
            )),
        );
    }
    if citizen.is_digital {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiResponse::error(
                "Цифрового агента нельзя утвердить в Citizen".to_string(),
            )),
        );
    }
    if citizen.passport_issued {
        return CitizenError::PassportAlreadyIssued.to_response();
    }

    let cand_type = citizen
        .candidate_type
        .clone()
        .or(Some(CandidateType::Citizenship));
    if !crate::auth::can_assign_role(
        &auth.role,
        cand_type.as_ref(),
        citizen
            .target_role
            .as_deref()
            .or(Some(Role::Citizen.as_str())),
        &Role::Citizen,
    ) {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiResponse::error(
                "Нельзя утвердить в Citizen: ограничение типа кандидатуры".to_string(),
            )),
        );
    }

    let now = Utc::now();
    let passport_id = Uuid::new_v4().to_string();
    let expires_at = now.timestamp() + 365 * 24 * 60 * 60;

    if let Err(e) = sqlx::query(
        r#"
        UPDATE citizens
        SET role = $1,
            status = $2,
            passport_issued = TRUE,
            passport_expires = $3,
            candidate_type = NULL,
            target_role = NULL
        WHERE id = $4
        "#,
    )
    .bind(Role::Citizen.as_str())
    .bind(CitizenStatus::Active.as_str())
    .bind(expires_at)
    .bind(&citizen.id)
    .execute(&state.db)
    .await
    {
        return CitizenError::DatabaseError(e.to_string()).to_response();
    }

    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO passports (id, citizen_id, issued_at, expires_at, is_valid)
        VALUES ($1, $2, $3, $4, TRUE)
        "#,
    )
    .bind(&passport_id)
    .bind(&citizen.id)
    .bind(now.timestamp())
    .bind(expires_at)
    .execute(&state.db)
    .await
    {
        return CitizenError::DatabaseError(e.to_string()).to_response();
    }

    let _ = sqlx::query(
        r#"
        UPDATE api_keys SET role = $1
        WHERE citizen_id = $2 AND is_active = TRUE
        "#,
    )
    .bind(Role::Citizen.as_str())
    .bind(&citizen.id)
    .execute(&state.db)
    .await;

    let _ = sqlx::query(
        r#"
        UPDATE citizenship_applications
        SET status = 'Approved', approved_by = $1, approved_at = $2
        WHERE citizen_id = $3
        "#,
    )
    .bind(&auth.citizen_name)
    .bind(now)
    .bind(&citizen.id)
    .execute(&state.db)
    .await;

    let pass_event = build_signed_passport_issued_event(
        &format!("passport_issue_{}", passport_id),
        &passport_id,
        &citizen.id,
        &citizen.name,
        now.timestamp(),
        expires_at,
        &auth.citizen_name,
        now.timestamp(),
    );
    let _ = submit_pending_event(state.clone(), pass_event).await;

    let role_event = build_signed_citizen_role_event(
        &format!("citizen_role_{}_{}", citizen.id, now.timestamp()),
        &citizen.id,
        &citizen.name,
        Role::Citizen.as_str(),
        &auth.citizen_name,
        now.timestamp(),
    );
    let _ = submit_pending_event(state.clone(), role_event).await;

    citizen.role = Role::Citizen;
    citizen.status = CitizenStatus::Active;
    citizen.passport_issued = true;
    citizen.passport_expires = Some(expires_at);
    citizen.candidate_type = None;
    citizen.target_role = None;

    (
        StatusCode::OK,
        Json(ApiResponse::success(ApproveCitizenResponse {
            citizen: CitizenResponse::from(citizen),
            passport_id,
            message: "Судья утвердил гражданство. Паспорт выдан.".to_string(),
        })),
    )
}

/// GET /citizen/pending — open applications (Submitted / Verified).
pub async fn list_pending_applications(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> impl IntoResponse {
    if !auth.can_verify_agent() && !auth.can_approve_citizen() {
        return permission_denied(&auth);
    }

    let rows = match sqlx::query_as::<_, CitizenshipApplicationRow>(
        r#"
        SELECT
            a.id,
            a.citizen_id,
            c.name AS citizen_name,
            c.status AS citizen_status,
            c.role AS citizen_role,
            c.is_digital,
            a.email,
            a.reason,
            a.applicant_type,
            a.status,
            a.verified_by,
            a.verified_at,
            a.approved_by,
            a.approved_at,
            a.created_at
        FROM citizenship_applications a
        JOIN citizens c ON c.id = a.citizen_id
        WHERE a.status IN ('Submitted', 'Verified')
        ORDER BY a.created_at ASC
        "#,
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => return CitizenError::DatabaseError(e.to_string()).to_response(),
    };

    (
        StatusCode::OK,
        Json(ApiResponse::success(serde_json::json!({
            "applications": rows,
            "total": rows.len(),
        }))),
    )
}

#[cfg(test)]
mod status_tests {
    use super::{
        apply_detection_meta, detect_digital_applicant, parse_applicant_type, Citizen,
        CitizenStatus, Role,
    };
    use crate::auth::AuthContext;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn applicant_status_roundtrip() {
        assert_eq!(CitizenStatus::Applicant.as_str(), "applicant");
        assert_eq!(
            CitizenStatus::from_str("applicant"),
            Some(CitizenStatus::Applicant)
        );
        assert_eq!(
            CitizenStatus::from_str("Applicant"),
            Some(CitizenStatus::Applicant)
        );
    }

    #[test]
    fn candidate_role_has_no_system_access_or_api_key() {
        let auth = AuthContext::from_api_key("cand".into(), Role::Candidate);
        assert!(!auth.has_system_access());
        assert!(!auth.can_hold_api_key());
        assert!(!auth.can_vote());
        assert!(!auth.can_read());
        assert!(Role::Candidate.is_candidate());
    }

    #[test]
    fn workflow_candidate_to_agent_to_citizen() {
        let candidate = AuthContext::from_api_key("c".into(), Role::Candidate);
        assert!(!candidate.can_hold_api_key());
        assert!(!candidate.has_system_access());

        let agent = AuthContext::from_api_key("a".into(), Role::Agent);
        assert!(agent.can_hold_api_key());
        assert!(agent.has_system_access());
        assert!(!agent.can_vote());

        let citizen = AuthContext::from_api_key("u".into(), Role::Citizen);
        assert!(citizen.can_hold_api_key());
        assert!(citizen.has_system_access());
        assert!(citizen.can_vote());
    }

    #[test]
    fn detect_bot_by_user_agent_and_honeypot() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::USER_AGENT,
            HeaderValue::from_static("python-requests/2.31"),
        );
        assert!(detect_digital_applicant(
            &headers,
            Some("human"),
            None,
            Some(10_000),
            false
        ));

        let mut browser = HeaderMap::new();
        browser.insert(
            axum::http::header::USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120.0.0.0",
            ),
        );
        browser.insert(
            axum::http::header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("ru-RU,ru;q=0.9"),
        );
        assert!(!detect_digital_applicant(
            &browser,
            Some("human"),
            None,
            Some(8_000),
            false
        ));
        assert!(detect_digital_applicant(
            &browser,
            Some("human"),
            Some("http://spam"),
            Some(8_000),
            false
        ));
        assert!(detect_digital_applicant(
            &browser,
            Some("digital"),
            None,
            Some(8_000),
            false
        ));
        assert!(detect_digital_applicant(
            &browser,
            Some("human"),
            None,
            Some(500),
            false
        ));
        assert!(detect_digital_applicant(
            &browser,
            Some("human"),
            None,
            Some(8_000),
            true
        ));
    }

    #[test]
    fn parse_applicant_type_human_and_digital() {
        assert_eq!(
            parse_applicant_type("human"),
            Some(("human".into(), false))
        );
        assert_eq!(
            parse_applicant_type("DIGITAL"),
            Some(("digital".into(), true))
        );
        assert!(parse_applicant_type("bot").is_none());
    }

    #[test]
    fn digital_agent_cannot_become_citizen_flag() {
        let digital = Citizen {
            id: "1".into(),
            name: "bot".into(),
            public_key: "00".into(),
            status: CitizenStatus::Active,
            role: Role::Agent,
            created_at: 0,
            passport_issued: false,
            passport_expires: None,
            is_digital: true,
            email_verified: true,
            candidate_type: None,
            target_role: None,
        };
        assert!(digital.is_digital);
        assert!(!Citizen {
            is_digital: false,
            ..digital.clone()
        }
        .is_digital);
    }

    #[test]
    fn apply_detection_meta_maps_human_and_digital() {
        assert_eq!(apply_detection_meta(false), ("email", "human"));
        assert_eq!(apply_detection_meta(true), ("auto", "digital"));
    }

    #[test]
    fn human_candidate_needs_email_before_sb() {
        let human = Citizen {
            id: "1".into(),
            name: "alice".into(),
            public_key: "00".into(),
            status: CitizenStatus::Pending,
            role: Role::Candidate,
            created_at: 0,
            passport_issued: false,
            passport_expires: None,
            is_digital: false,
            email_verified: false,
            candidate_type: Some(crate::types::CandidateType::Citizenship),
            target_role: Some("Citizen".into()),
        };
        assert!(human.role == Role::Candidate);
        assert!(!human.is_digital);
        assert!(!human.email_verified);
        assert_eq!(
            human.candidate_type,
            Some(crate::types::CandidateType::Citizenship)
        );
    }
}
