//! Portal chat via verified-email session (no citizen API key in the browser).
//!
//! Auth mirrors `/api/my-applications`: client sends `email` + `email_already_verified: true`,
//! server peeks the in-memory register-verify mark. Sending a message refreshes the mark TTL.
//!
//! Open to guest (verified email), candidate (application), and citizen (real passport holder).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::chat::{
    get_or_create_session, list_messages, validate_message_content, ChatError, ChatMessageRow,
    ListMessagesQuery, SYSTEM_BOT_ID,
};
use crate::register_verify::{
    email_has_candidate_application, is_valid_email, normalize_email,
    peek_recent_email_verification, refresh_recent_email_verification,
    recent_email_verification_expires_in,
};
use crate::response::{self, ApiResponse};
use crate::types::Role;
use crate::AppState;

const CANDIDATE_CHAT_FORBIDDEN: &str =
    "Сначала подтвердите email через вход на сайте, затем повторите запрос";

#[derive(Debug, Deserialize)]
pub struct CandidateChatAuthBody {
    pub email: String,
    #[serde(default)]
    pub email_already_verified: Option<bool>,
    pub limit: Option<i64>,
    pub before: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CandidateSendMessageRequest {
    pub email: String,
    #[serde(default)]
    pub email_already_verified: Option<bool>,
    /// Message text (alias of `content` for the site form).
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PortalChatMessageView {
    #[serde(flatten)]
    pub message: ChatMessageRow,
    pub chat_role: String,
    pub status_label: String,
}

#[derive(Debug, Serialize)]
pub struct CandidateSendMessageResponse {
    #[serde(flatten)]
    pub message: PortalChatMessageView,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatRole {
    Guest,
    Candidate,
    Citizen,
}

impl ChatRole {
    fn as_str(self) -> &'static str {
        match self {
            ChatRole::Guest => "guest",
            ChatRole::Candidate => "candidate",
            ChatRole::Citizen => "citizen",
        }
    }

    fn status_label(self) -> &'static str {
        match self {
            ChatRole::Guest => "Гость",
            ChatRole::Candidate => "Кандидат",
            ChatRole::Citizen => "Гражданин",
        }
    }

    fn from_db(status: &str, role: &str, citizen_id: &str) -> Self {
        if citizen_id.starts_with("guest-") {
            return ChatRole::Guest;
        }
        if citizen_id.starts_with("cand-")
            || status == "applicant"
            || role == Role::Candidate.as_str()
        {
            return ChatRole::Candidate;
        }
        if matches!(status, "active" | "online" | "pending") {
            // Governance roles still chat as citizens on the portal.
            return ChatRole::Citizen;
        }
        ChatRole::Guest
    }
}

#[derive(Debug, Clone)]
struct PortalIdentity {
    citizen_id: String,
    display_name: String,
    chat_role: ChatRole,
}

fn require_verified_email_session(
    email: &str,
    claimed: Option<bool>,
) -> Result<String, axum::response::Response> {
    let email = normalize_email(email);
    if !is_valid_email(&email) {
        return Err(response::bad_request("Введите корректный email-адрес"));
    }
    let client_claims_verified = claimed.unwrap_or(false);
    if !client_claims_verified || !peek_recent_email_verification(&email) {
        return Err(response::forbidden(CANDIDATE_CHAT_FORBIDDEN));
    }
    Ok(email)
}

fn email_hash_hex(email: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(email.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn display_name_from_email(email: &str) -> String {
    email
        .split('@')
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("гость")
        .chars()
        .take(40)
        .collect()
}

fn placeholder_public_key(email: &str) -> String {
    let hash = email_hash_hex(email);
    format!("{}{}", &hash[..32], &hash[32..64])
}

async fn find_real_citizen_by_email(
    pool: &PgPool,
    email: &str,
) -> Result<Option<(String, String, String, String)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String, String, String)>(
        r#"
        SELECT
            id,
            COALESCE(NULLIF(BTRIM(display_name), ''), name) AS name,
            status,
            COALESCE(role, '') AS role
        FROM citizens
        WHERE lower(email) = lower($1)
          AND id NOT LIKE 'cand-%'
          AND id NOT LIKE 'guest-%'
          AND NOT (
            status = 'applicant'
            AND COALESCE(role, '') = 'Candidate'
          )
        ORDER BY
          CASE
            WHEN status IN ('active', 'online') THEN 0
            WHEN status = 'pending' THEN 1
            ELSE 2
          END,
          created_at DESC
        LIMIT 1
        "#,
    )
    .bind(email)
    .fetch_optional(pool)
    .await
}

async fn find_candidate_placeholder_by_email(
    pool: &PgPool,
    email: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT id, COALESCE(NULLIF(BTRIM(display_name), ''), name) AS name
        FROM citizens
        WHERE lower(email) = lower($1)
          AND status = 'applicant'
          AND COALESCE(role, '') = 'Candidate'
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(email)
    .fetch_optional(pool)
    .await
}

async fn find_citizenship_application(
    pool: &PgPool,
    email: &str,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    match crate::candidates::latest_by_email_kind(
        pool,
        email,
        crate::candidates::KIND_CITIZENSHIP,
    )
    .await?
    {
        Some((id, _, login, _)) => Ok(Some((id, login))),
        None => Ok(None),
    }
}

async fn find_mayor_application(
    pool: &PgPool,
    email: &str,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    match crate::candidates::latest_by_email_kind(pool, email, crate::candidates::KIND_MAYOR)
        .await?
    {
        Some((id, _, login, _)) => Ok(Some((id, login))),
        None => Ok(None),
    }
}

async fn ensure_placeholder_citizen(
    pool: &PgPool,
    citizen_id: &str,
    email: &str,
    status: &str,
    role: Option<&str>,
) -> Result<(), ChatError> {
    let now = Utc::now().timestamp();
    let public_key = placeholder_public_key(email);
    let name = citizen_id.to_string();
    sqlx::query(
        r#"
        INSERT INTO citizens
            (id, name, public_key, status, role, created_at, passport_issued,
             email, email_verified, candidate_type, target_role)
        VALUES
            ($1, $2, $3, $4, $5, $6, FALSE,
             $7, TRUE, 'citizenship', 'Citizen')
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(citizen_id)
    .bind(&name)
    .bind(&public_key)
    .bind(status)
    .bind(role)
    .bind(now)
    .bind(email)
    .execute(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))?;
    Ok(())
}

/// Resolve chat identity for a verified email: citizen → candidate → guest.
async fn resolve_portal_identity(
    pool: &PgPool,
    email: &str,
) -> Result<PortalIdentity, ChatError> {
    if let Some((id, name, status, role)) = find_real_citizen_by_email(pool, email)
        .await
        .map_err(|e| ChatError::Database(e.to_string()))?
    {
        if id != SYSTEM_BOT_ID {
            let chat_role = ChatRole::from_db(&status, &role, &id);
            return Ok(PortalIdentity {
                citizen_id: id,
                display_name: name,
                chat_role,
            });
        }
    }

    let has_app = email_has_candidate_application(pool, email)
        .await
        .map_err(|e| ChatError::Database(e.to_string()))?;

    if has_app {
        if let Some((id, name)) = find_candidate_placeholder_by_email(pool, email)
            .await
            .map_err(|e| ChatError::Database(e.to_string()))?
        {
            if id != SYSTEM_BOT_ID {
                return Ok(PortalIdentity {
                    citizen_id: id,
                    display_name: name,
                    chat_role: ChatRole::Candidate,
                });
            }
        }

        if let Some((app_id, login)) = find_citizenship_application(pool, email)
            .await
            .map_err(|e| ChatError::Database(e.to_string()))?
        {
            let citizen_id = format!("cand-app-{app_id}");
            ensure_placeholder_citizen(
                pool,
                &citizen_id,
                email,
                "applicant",
                Some(Role::Candidate.as_str()),
            )
            .await?;
            let display_name = login
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| display_name_from_email(email));
            return Ok(PortalIdentity {
                citizen_id,
                display_name,
                chat_role: ChatRole::Candidate,
            });
        }

        if let Some((app_id, login)) = find_mayor_application(pool, email)
            .await
            .map_err(|e| ChatError::Database(e.to_string()))?
        {
            let citizen_id = format!("cand-mayor-{app_id}");
            ensure_placeholder_citizen(
                pool,
                &citizen_id,
                email,
                "applicant",
                Some(Role::Candidate.as_str()),
            )
            .await?;
            let display_name = login
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| display_name_from_email(email));
            return Ok(PortalIdentity {
                citizen_id,
                display_name,
                chat_role: ChatRole::Candidate,
            });
        }
    }

    // Verified email without application / citizenship → guest portal chat.
    let hash = email_hash_hex(email);
    let citizen_id = format!("guest-{}", &hash[..24]);
    ensure_placeholder_citizen(pool, &citizen_id, email, "pending", None).await?;
    Ok(PortalIdentity {
        citizen_id,
        display_name: display_name_from_email(email),
        chat_role: ChatRole::Guest,
    })
}

async fn load_status_map(
    pool: &PgPool,
    citizen_ids: &[String],
) -> Result<HashMap<String, (String, String)>, sqlx::Error> {
    if citizen_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT id, status, COALESCE(role, '') AS role
        FROM citizens
        WHERE id = ANY($1)
        "#,
    )
    .bind(citizen_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, status, role)| (id, (status, role)))
        .collect())
}

fn enrich_message(row: ChatMessageRow, status_map: &HashMap<String, (String, String)>) -> PortalChatMessageView {
    if row.citizen_id == SYSTEM_BOT_ID {
        return PortalChatMessageView {
            message: row,
            chat_role: "secretary".to_string(),
            status_label: "Секретарь".to_string(),
        };
    }
    let chat_role = match status_map.get(&row.citizen_id) {
        Some((status, role)) => ChatRole::from_db(status, role, &row.citizen_id),
        None => {
            if row.citizen_id.starts_with("guest-") {
                ChatRole::Guest
            } else if row.citizen_id.starts_with("cand-") {
                ChatRole::Candidate
            } else {
                ChatRole::Citizen
            }
        }
    };
    PortalChatMessageView {
        message: row,
        chat_role: chat_role.as_str().to_string(),
        status_label: chat_role.status_label().to_string(),
    }
}

async fn enrich_messages(
    pool: &PgPool,
    rows: Vec<ChatMessageRow>,
) -> Result<Vec<PortalChatMessageView>, ChatError> {
    let ids: Vec<String> = rows
        .iter()
        .map(|r| r.citizen_id.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let status_map = load_status_map(pool, &ids)
        .await
        .map_err(|e| ChatError::Database(e.to_string()))?;
    Ok(rows
        .into_iter()
        .map(|row| enrich_message(row, &status_map))
        .collect())
}

async fn insert_portal_message(
    state: &Arc<AppState>,
    identity: &PortalIdentity,
    content: &str,
) -> Result<ChatMessageRow, ChatError> {
    let session_id = get_or_create_session(state, &identity.citizen_id).await?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        "INSERT INTO chat_messages (id, citizen_id, citizen_name, content, created_at, session_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&id)
    .bind(&identity.citizen_id)
    .bind(&identity.display_name)
    .bind(content)
    .bind(now)
    .bind(&session_id)
    .execute(&state.db)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))?;

    sqlx::query("UPDATE chat_sessions SET last_activity = $1 WHERE id = $2")
        .bind(now)
        .bind(&session_id)
        .execute(&state.db)
        .await
        .map_err(|e| ChatError::Database(e.to_string()))?;

    let row = ChatMessageRow {
        id,
        citizen_id: identity.citizen_id.clone(),
        citizen_name: identity.display_name.clone(),
        content: content.to_string(),
        created_at: now,
        session_id,
    };

    crate::channel::notify_user_message(state, &row).await;

    let state_gossip = state.clone();
    let row_gossip = row.clone();
    tokio::spawn(async move {
        crate::gossip::push_chat_message_to_peers(&state_gossip, &row_gossip).await;
    });

    Ok(row)
}

/// POST /api/chat/candidate/messages — list recent chat messages (peek mark, no refresh).
pub async fn candidate_list_messages_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CandidateChatAuthBody>,
) -> impl IntoResponse {
    let email = match require_verified_email_session(&req.email, req.email_already_verified) {
        Ok(e) => e,
        Err(resp) => return resp,
    };

    let identity = match resolve_portal_identity(&state.db, &email).await {
        Ok(id) => id,
        Err(e) => return e.into_api_response(),
    };

    let expires_in_secs = recent_email_verification_expires_in(&email).unwrap_or(0);

    match list_messages(
        &state.db,
        ListMessagesQuery {
            limit: req.limit,
            before: req.before,
            session_id: None,
        },
    )
    .await
    {
        Ok(rows) => match enrich_messages(&state.db, rows).await {
            Ok(messages) => Json(ApiResponse::success(serde_json::json!({
                "messages": messages,
                "total": messages.len(),
                "citizen_id": identity.citizen_id,
                "citizen_name": identity.display_name,
                "chat_role": identity.chat_role.as_str(),
                "status_label": identity.chat_role.status_label(),
                "expires_in_secs": expires_in_secs,
            })))
            .into_response(),
            Err(e) => e.into_api_response(),
        },
        Err(e) => e.into_api_response(),
    }
}

/// POST /api/chat/candidate/send — send a chat message; refreshes email-mark TTL on success.
pub async fn candidate_send_message_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CandidateSendMessageRequest>,
) -> impl IntoResponse {
    let email = match require_verified_email_session(&req.email, req.email_already_verified) {
        Ok(e) => e,
        Err(resp) => return resp,
    };

    let raw = req
        .text
        .as_deref()
        .or(req.content.as_deref())
        .unwrap_or("");
    let content = match validate_message_content(raw) {
        Ok(c) => c,
        Err(e) => return e.into_api_response(),
    };

    let identity = match resolve_portal_identity(&state.db, &email).await {
        Ok(id) => id,
        Err(e) => return e.into_api_response(),
    };

    let row = match insert_portal_message(&state, &identity, &content).await {
        Ok(r) => r,
        Err(e) => return e.into_api_response(),
    };

    let Some(expires_in_secs) = refresh_recent_email_verification(&email) else {
        return response::forbidden(CANDIDATE_CHAT_FORBIDDEN);
    };

    tracing::info!(
        email = %email,
        citizen_id = %identity.citizen_id,
        chat_role = %identity.chat_role.as_str(),
        message_id = %row.id,
        "portal chat message sent"
    );

    let view = PortalChatMessageView {
        message: row,
        chat_role: identity.chat_role.as_str().to_string(),
        status_label: identity.chat_role.status_label().to_string(),
    };

    (
        StatusCode::CREATED,
        Json(ApiResponse::success(CandidateSendMessageResponse {
            message: view,
            expires_in_secs,
        })),
    )
        .into_response()
}
