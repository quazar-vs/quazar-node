use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Extension, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::time;
use uuid::Uuid;

use crate::auth::{AuthContext, KeyStore};
use crate::blockchain::build_signed_chat_history_added_event;
use crate::block_producer;
use crate::pending;
use crate::response::{self, ApiResponse};
use crate::validator::EventValidator;
use crate::AppState;

pub const MAX_MESSAGE_LEN: usize = 2000;
pub const SYSTEM_BOT_ID: &str = "system_bot";
pub const SYSTEM_BOT_NAME: &str = "Квазар";
pub const SESSION_INACTIVITY: Duration = Duration::from_secs(30 * 60);
pub const ARCHIVE_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 100;
pub const DEFAULT_SEARCH_LIMIT: i64 = 10;
const MAX_SEARCH_LIMIT: i64 = 50;

const MSG_COLS: &str = "id, citizen_id, citizen_name, content, created_at, session_id";

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct AdminSendMessageRequest {
    pub content: String,
    pub admin_key: String,
}

#[derive(Debug, Deserialize)]
pub struct ListMessagesQuery {
    pub limit: Option<i64>,
    pub before: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchMessagesQuery {
    pub q: String,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct DbSearchQuery {
    pub q: String,
    pub scope: Option<String>,
    pub limit: Option<i64>,
    pub token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Chat,
    Citizens,
    Votes,
    Events,
    Nodes,
    All,
}

impl SearchScope {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "chat" => Some(Self::Chat),
            "citizens" => Some(Self::Citizens),
            "votes" => Some(Self::Votes),
            "events" => Some(Self::Events),
            "nodes" => Some(Self::Nodes),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Citizens => "citizens",
            Self::Votes => "votes",
            Self::Events => "events",
            Self::Nodes => "nodes",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct DbSearchResult {
    pub id: String,
    pub title: String,
    pub content: String,
    pub created_at: Option<chrono::DateTime<Utc>>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChatMessageRow {
    pub id: String,
    pub citizen_id: String,
    pub citizen_name: String,
    pub content: String,
    pub created_at: chrono::DateTime<Utc>,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChatSessionRow {
    pub id: String,
    pub user_id: String,
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChatArchiveRow {
    pub id: String,
    pub session_id: String,
    pub original_size: i32,
    pub compressed_size: i32,
    pub created_at: DateTime<Utc>,
    pub event_id: Option<String>,
    pub is_synced: bool,
}

#[derive(Debug)]
pub enum ChatError {
    Message(String),
    Forbidden(String),
    Database(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatError::Message(m) => write!(f, "{m}"),
            ChatError::Forbidden(m) => write!(f, "{m}"),
            ChatError::Database(m) => write!(f, "database error: {m}"),
        }
    }
}

impl ChatError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }

    fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }

    fn to_response(&self) -> (StatusCode, Json<ApiResponse>) {
        let (status, message) = match &self {
            ChatError::Message(m) => (StatusCode::BAD_REQUEST, m.clone()),
            ChatError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            ChatError::Database(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        };
        (status, Json(ApiResponse::error(message)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GossipInsertResult {
    Inserted,
    AlreadyExists,
}

fn compress_zstd(data: &[u8]) -> Result<Vec<u8>, ChatError> {
    zstd::encode_all(data, 3).map_err(|e| ChatError::Database(format!("zstd compress: {e}")))
}

#[cfg(test)]
fn decompress_zstd(data: &[u8]) -> Result<Vec<u8>, ChatError> {
    zstd::decode_all(data).map_err(|e| ChatError::Database(format!("zstd decompress: {e}")))
}

/// Ensures a remote session row exists for gossiped messages (activates after closing conflicts).
async fn ensure_session_for_gossip(
    pool: &PgPool,
    session_id: &str,
    user_id: &str,
    activity: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE chat_sessions SET is_active = FALSE
         WHERE user_id = $1 AND is_active = TRUE AND id <> $2",
    )
    .bind(user_id)
    .bind(session_id)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO chat_sessions (id, user_id, started_at, last_activity, is_active)
        VALUES ($1, $2, $3, $3, TRUE)
        ON CONFLICT (id) DO UPDATE SET
            last_activity = GREATEST(chat_sessions.last_activity, EXCLUDED.last_activity),
            is_active = TRUE
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .bind(activity)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn insert_gossip_message(
    pool: &PgPool,
    message: &ChatMessageRow,
) -> Result<GossipInsertResult, sqlx::Error> {
    ensure_session_for_gossip(pool, &message.session_id, &message.citizen_id, message.created_at)
        .await?;

    let result = sqlx::query(
        "INSERT INTO chat_messages (id, citizen_id, citizen_name, content, created_at, session_id)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&message.id)
    .bind(&message.citizen_id)
    .bind(&message.citizen_name)
    .bind(&message.content)
    .bind(message.created_at)
    .bind(&message.session_id)
    .execute(pool)
    .await?;

    if result.rows_affected() > 0 {
        tracing::debug!(message_id = %message.id, "chat gossip message inserted");
        Ok(GossipInsertResult::Inserted)
    } else {
        Ok(GossipInsertResult::AlreadyExists)
    }
}

pub fn validate_message_content(content: &str) -> Result<String, ChatError> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(ChatError::bad_request("Сообщение не может быть пустым"));
    }
    if trimmed.len() > MAX_MESSAGE_LEN {
        return Err(ChatError::bad_request(format!(
            "Сообщение слишком длинное (макс. {MAX_MESSAGE_LEN} символов)"
        )));
    }
    Ok(trimmed.to_string())
}

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

async fn load_citizen(pool: &PgPool, citizen_id: &str) -> Result<(String, String, String), ChatError> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT id, name, status FROM citizens WHERE id = $1",
    )
    .bind(citizen_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))?;

    row.ok_or_else(|| ChatError::bad_request("Гражданин не найден"))
}

fn ensure_active_status(status: &str) -> Result<(), ChatError> {
    if status != "active" {
        return Err(ChatError::bad_request(format!(
            "Отправка недоступна: статус {status}"
        )));
    }
    Ok(())
}

async fn resolve_auth_citizen_id(auth: &AuthContext, pool: &PgPool) -> Result<String, ChatError> {
    if auth.is_node {
        return Err(ChatError::bad_request(
            "Учётные данные узла не могут участвовать в чате",
        ));
    }
    Ok(auth.resolve_account_id(pool).await)
}

fn reject_chat_read(auth: &AuthContext) -> Option<axum::response::Response> {
    if auth.is_node {
        return Some(response::forbidden(
            "Node credentials cannot access chat",
        ));
    }
    if !auth.can_read_chat() {
        return Some(response::forbidden(auth.insufficient_permissions_message()));
    }
    None
}

fn reject_chat_send(auth: &AuthContext) -> Option<axum::response::Response> {
    if auth.is_node {
        return Some(response::forbidden(
            "Node credentials cannot send chat messages",
        ));
    }
    if !auth.can_send_chat() {
        return Some(response::forbidden(auth.insufficient_permissions_message()));
    }
    None
}

/// One active session per user. Archives stale sessions before opening a new one.
pub async fn get_or_create_session(
    state: &Arc<AppState>,
    user_id: &str,
) -> Result<String, ChatError> {
    let pool = &state.db;
    let existing: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, last_activity FROM chat_sessions
         WHERE user_id = $1 AND is_active = TRUE
         LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))?;

    if let Some((session_id, last_activity)) = existing {
        let age = Utc::now()
            .signed_duration_since(last_activity)
            .to_std()
            .unwrap_or(Duration::ZERO);
        if age < SESSION_INACTIVITY {
            return Ok(session_id);
        }
        close_and_archive_session(pool, Some(state), &session_id).await?;
    }

    let session_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO chat_sessions (id, user_id, started_at, last_activity, is_active)
         VALUES ($1, $2, $3, $3, TRUE)",
    )
    .bind(&session_id)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))?;

    Ok(session_id)
}

pub async fn get_active_session(
    pool: &PgPool,
    user_id: &str,
) -> Result<Option<ChatSessionRow>, ChatError> {
    sqlx::query_as::<_, ChatSessionRow>(
        "SELECT id, user_id, started_at, last_activity, is_active
         FROM chat_sessions
         WHERE user_id = $1 AND is_active = TRUE
         LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))
}

pub async fn get_messages(
    pool: &PgPool,
    session_id: &str,
) -> Result<Vec<ChatMessageRow>, ChatError> {
    let sql = format!(
        "SELECT {MSG_COLS} FROM chat_messages
         WHERE session_id = $1
         ORDER BY created_at ASC, id ASC"
    );
    sqlx::query_as::<_, ChatMessageRow>(sqlx::AssertSqlSafe(sql))
        .bind(session_id)
        .fetch_all(pool)
        .await
        .map_err(|e| ChatError::Database(e.to_string()))
}

pub async fn check_inactive_sessions(pool: &PgPool) -> Result<Vec<String>, ChatError> {
    let cutoff = Utc::now() - chrono::Duration::from_std(SESSION_INACTIVITY).unwrap();
    sqlx::query_scalar(
        "SELECT id FROM chat_sessions
         WHERE is_active = TRUE AND last_activity < $1",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))
}

async fn create_chat_history_event(
    state: &Arc<AppState>,
    archive_id: &str,
    session_id: &str,
    user_id: &str,
    compressed_hex: &str,
    original_size: i32,
    compressed_size: i32,
    message_count: i32,
) -> Result<String, ChatError> {
    let event_id = format!("chat_history_{archive_id}");
    let now = Utc::now().timestamp();
    let event = build_signed_chat_history_added_event(
        &event_id,
        archive_id,
        session_id,
        user_id,
        compressed_hex,
        original_size,
        compressed_size,
        message_count,
        &state.node_id,
        now,
    );

    if let Err(e) = EventValidator::validate_event(&event, &state.db).await {
        return Err(ChatError::bad_request(e.message()));
    }

    match pending::insert(&state.db, &event).await {
        Ok(pending::PendingInsertResult::Inserted) => {}
        Ok(pending::PendingInsertResult::AlreadyExists) => {
            return Ok(event_id);
        }
        Err(e) => {
            return Err(ChatError::Database(format!(
                "Не удалось добавить ChatHistoryAdded: {e}"
            )));
        }
    }

    let state_gossip = state.clone();
    let event_gossip = event.clone();
    tokio::spawn(async move {
        crate::gossip::push_pending_to_peers(&state_gossip, &event_gossip).await;
    });

    let state_block = state.clone();
    tokio::spawn(async move {
        block_producer::try_create_block(state_block).await;
    });

    Ok(event_id)
}

/// Archive session messages with zstd, emit ChatHistoryAdded, clear active messages.
pub async fn close_and_archive_session(
    pool: &PgPool,
    state: Option<&Arc<AppState>>,
    session_id: &str,
) -> Result<Option<String>, ChatError> {
    let session: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT id, user_id, is_active FROM chat_sessions WHERE id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))?;

    let Some((_, user_id, is_active)) = session else {
        return Err(ChatError::bad_request("Сеанс не найден"));
    };

    if !is_active {
        return Ok(None);
    }

    let messages = get_messages(pool, session_id).await?;

    if messages.is_empty() {
        sqlx::query("UPDATE chat_sessions SET is_active = FALSE WHERE id = $1")
            .bind(session_id)
            .execute(pool)
            .await
            .map_err(|e| ChatError::Database(e.to_string()))?;
        tracing::info!(session_id = %session_id, "empty chat session closed without archive");
        return Ok(None);
    }

    let payload = serde_json::json!({
        "session_id": session_id,
        "user_id": user_id,
        "messages": messages,
    });
    let original = serde_json::to_vec(&payload)
        .map_err(|e| ChatError::Database(format!("serialize archive: {e}")))?;
    let original_size = original.len() as i32;
    let compressed = compress_zstd(&original)?;
    let compressed_size = compressed.len() as i32;
    let archive_id = Uuid::new_v4().to_string();
    let compressed_hex = hex::encode(&compressed);

    sqlx::query(
        r#"
        INSERT INTO chat_archives (
            id, session_id, compressed_data, original_size, compressed_size,
            created_at, event_id, is_synced
        )
        VALUES ($1, $2, $3, $4, $5, NOW(), NULL, FALSE)
        "#,
    )
    .bind(&archive_id)
    .bind(session_id)
    .bind(&compressed)
    .bind(original_size)
    .bind(compressed_size)
    .execute(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))?;

    sqlx::query("DELETE FROM chat_messages WHERE session_id = $1")
        .bind(session_id)
        .execute(pool)
        .await
        .map_err(|e| ChatError::Database(e.to_string()))?;

    sqlx::query("UPDATE chat_sessions SET is_active = FALSE WHERE id = $1")
        .bind(session_id)
        .execute(pool)
        .await
        .map_err(|e| ChatError::Database(e.to_string()))?;

    let event_id = if let Some(state) = state {
        match create_chat_history_event(
            state,
            &archive_id,
            session_id,
            &user_id,
            &compressed_hex,
            original_size,
            compressed_size,
            messages.len() as i32,
        )
        .await
        {
            Ok(eid) => {
                let _ = sqlx::query(
                    "UPDATE chat_archives SET event_id = $1 WHERE id = $2",
                )
                .bind(&eid)
                .bind(&archive_id)
                .execute(pool)
                .await;
                Some(eid)
            }
            Err(e) => {
                tracing::error!(
                    session_id = %session_id,
                    archive_id = %archive_id,
                    error = %e,
                    "failed to emit ChatHistoryAdded"
                );
                None
            }
        }
    } else {
        None
    };

    tracing::info!(
        session_id = %session_id,
        archive_id = %archive_id,
        messages = messages.len(),
        event_id = ?event_id,
        "chat session archived"
    );

    Ok(Some(archive_id))
}

pub async fn archive_inactive_sessions(state: &Arc<AppState>) {
    let ids = match check_inactive_sessions(&state.db).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "check_inactive_sessions failed");
            return;
        }
    };

    for session_id in ids {
        if let Err(e) = close_and_archive_session(&state.db, Some(state), &session_id).await {
            tracing::warn!(session_id = %session_id, error = %e, "archive inactive session failed");
        }
    }
}

pub async fn background_chat_archive(state: Arc<AppState>) {
    let mut interval = time::interval(ARCHIVE_CHECK_INTERVAL);
    loop {
        interval.tick().await;
        tracing::debug!("chat archive tick");
        archive_inactive_sessions(&state).await;
    }
}

pub async fn list_history(
    pool: &PgPool,
    user_id: &str,
) -> Result<Vec<ChatArchiveRow>, ChatError> {
    sqlx::query_as::<_, ChatArchiveRow>(
        r#"
        SELECT a.id, a.session_id, a.original_size, a.compressed_size,
               a.created_at, a.event_id, a.is_synced
        FROM chat_archives a
        JOIN chat_sessions s ON s.id = a.session_id
        WHERE s.user_id = $1
        ORDER BY a.created_at DESC
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))
}

pub async fn send_message(
    state: Arc<AppState>,
    auth: &AuthContext,
    req: SendMessageRequest,
) -> Result<ChatMessageRow, ChatError> {
    if !auth.can_send_chat() {
        return Err(ChatError::forbidden(
            auth.insufficient_permissions_message(),
        ));
    }

    let citizen_id = resolve_auth_citizen_id(auth, &state.db).await?;
    let (_, citizen_name, status) = load_citizen(&state.db, &citizen_id).await?;
    ensure_active_status(&status)?;

    let content = validate_message_content(&req.content)?;
    let session_id = get_or_create_session(&state, &citizen_id).await?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        "INSERT INTO chat_messages (id, citizen_id, citizen_name, content, created_at, session_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&id)
    .bind(&citizen_id)
    .bind(&citizen_name)
    .bind(&content)
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

    tracing::info!(
        citizen = %citizen_name,
        message_id = %id,
        session_id = %session_id,
        "chat message sent"
    );

    let row = ChatMessageRow {
        id,
        citizen_id,
        citizen_name,
        content,
        created_at: now,
        session_id,
    };

    crate::channel::notify_user_message(&state, &row).await;

    let state_gossip = state.clone();
    let row_gossip = row.clone();
    tokio::spawn(async move {
        crate::gossip::push_chat_message_to_peers(&state_gossip, &row_gossip).await;
    });

    Ok(row)
}

async fn ensure_system_bot(pool: &PgPool) -> Result<(), ChatError> {
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO citizens (id, name, public_key, status, role, created_at, passport_issued)
         VALUES ($1, $2, $3, 'active', 'Citizen', $4, false)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(SYSTEM_BOT_ID)
    .bind(SYSTEM_BOT_NAME)
    .bind("0000000000000000000000000000000000000000000000000000000000000000")
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))?;
    Ok(())
}

pub async fn send_message_as_bot(
    state: &Arc<AppState>,
    content: &str,
) -> Result<ChatMessageRow, ChatError> {
    ensure_system_bot(&state.db).await?;

    let content = validate_message_content(content)?;
    let session_id = get_or_create_session(state, SYSTEM_BOT_ID).await?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        "INSERT INTO chat_messages (id, citizen_id, citizen_name, content, created_at, session_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&id)
    .bind(SYSTEM_BOT_ID)
    .bind(SYSTEM_BOT_NAME)
    .bind(&content)
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

    tracing::info!(citizen = %SYSTEM_BOT_NAME, message_id = %id, "chat admin message sent");

    let row = ChatMessageRow {
        id,
        citizen_id: SYSTEM_BOT_ID.to_string(),
        citizen_name: SYSTEM_BOT_NAME.to_string(),
        content,
        created_at: now,
        session_id,
    };

    let state_gossip = state.clone();
    let row_gossip = row.clone();
    tokio::spawn(async move {
        crate::gossip::push_chat_message_to_peers(&state_gossip, &row_gossip).await;
    });

    Ok(row)
}

pub async fn list_messages(
    pool: &PgPool,
    query: ListMessagesQuery,
) -> Result<Vec<ChatMessageRow>, ChatError> {
    let limit = clamp_limit(query.limit);
    let session_filter = query
        .session_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let mut rows = if let Some(before_id) = query
        .before
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let cursor: Option<(chrono::DateTime<Utc>, String)> = sqlx::query_as(
            "SELECT created_at, id FROM chat_messages WHERE id = $1",
        )
        .bind(before_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ChatError::Database(e.to_string()))?;

        let Some((before_ts, before_msg_id)) = cursor else {
            return Err(ChatError::bad_request("Сообщение для пагинации не найдено"));
        };

        if let Some(ref sid) = session_filter {
            let sql = format!(
                r#"
                SELECT {MSG_COLS}
                FROM chat_messages
                WHERE session_id = $4
                  AND (created_at < $1 OR (created_at = $1 AND id < $2))
                ORDER BY created_at DESC, id DESC
                LIMIT $3
                "#
            );
            sqlx::query_as::<_, ChatMessageRow>(sqlx::AssertSqlSafe(sql))
                .bind(before_ts)
                .bind(before_msg_id)
                .bind(limit)
                .bind(sid)
                .fetch_all(pool)
                .await
                .map_err(|e| ChatError::Database(e.to_string()))?
        } else {
            let sql = format!(
                r#"
                SELECT {MSG_COLS}
                FROM chat_messages
                WHERE created_at < $1 OR (created_at = $1 AND id < $2)
                ORDER BY created_at DESC, id DESC
                LIMIT $3
                "#
            );
            sqlx::query_as::<_, ChatMessageRow>(sqlx::AssertSqlSafe(sql))
                .bind(before_ts)
                .bind(before_msg_id)
                .bind(limit)
                .fetch_all(pool)
                .await
                .map_err(|e| ChatError::Database(e.to_string()))?
        }
    } else if let Some(ref sid) = session_filter {
        let sql = format!(
            r#"
            SELECT {MSG_COLS}
            FROM chat_messages
            WHERE session_id = $1
            ORDER BY created_at DESC, id DESC
            LIMIT $2
            "#
        );
        sqlx::query_as::<_, ChatMessageRow>(sqlx::AssertSqlSafe(sql))
            .bind(sid)
            .bind(limit)
            .fetch_all(pool)
            .await
            .map_err(|e| ChatError::Database(e.to_string()))?
    } else {
        let sql = format!(
            r#"
            SELECT {MSG_COLS}
            FROM chat_messages
            ORDER BY created_at DESC, id DESC
            LIMIT $1
            "#
        );
        sqlx::query_as::<_, ChatMessageRow>(sqlx::AssertSqlSafe(sql))
            .bind(limit)
            .fetch_all(pool)
            .await
            .map_err(|e| ChatError::Database(e.to_string()))?
    };

    rows.reverse();
    Ok(rows
        .into_iter()
        .filter(|row| !crate::channel::is_tui_chat_message(row))
        .collect())
}

fn clamp_search_limit(limit: Option<i64>) -> i64 {
    limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT)
}

pub async fn search_messages(
    pool: &PgPool,
    query: &str,
    limit: i64,
    exclude_id: Option<&str>,
) -> Result<Vec<ChatMessageRow>, ChatError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let rows = if let Some(exclude) = exclude_id.filter(|s| !s.is_empty()) {
        let sql = format!(
            r#"
            SELECT {MSG_COLS}
            FROM chat_messages, websearch_to_tsquery('russian', $1) AS query
            WHERE to_tsvector('russian', content) @@ query
              AND id <> $3
            ORDER BY ts_rank(to_tsvector('russian', content), query) DESC
            LIMIT $2
            "#
        );
        sqlx::query_as::<_, ChatMessageRow>(sqlx::AssertSqlSafe(sql))
            .bind(trimmed)
            .bind(limit)
            .bind(exclude)
            .fetch_all(pool)
            .await
    } else {
        let sql = format!(
            r#"
            SELECT {MSG_COLS}
            FROM chat_messages, websearch_to_tsquery('russian', $1) AS query
            WHERE to_tsvector('russian', content) @@ query
            ORDER BY ts_rank(to_tsvector('russian', content), query) DESC
            LIMIT $2
            "#
        );
        sqlx::query_as::<_, ChatMessageRow>(sqlx::AssertSqlSafe(sql))
            .bind(trimmed)
            .bind(limit)
            .fetch_all(pool)
            .await
    }
    .map_err(|e| ChatError::Database(e.to_string()))?;

    Ok(rows)
}

async fn search_chat_results(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<DbSearchResult>, ChatError> {
    sqlx::query_as::<_, DbSearchResult>(
        r#"
        SELECT id,
               citizen_name AS title,
               content,
               created_at,
               'chat' AS source
        FROM chat_messages, websearch_to_tsquery('russian', $1) AS q
        WHERE to_tsvector('russian', content) @@ q
        ORDER BY ts_rank(to_tsvector('russian', content), q) DESC
        LIMIT $2
        "#,
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))
}

async fn search_citizens_results(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<DbSearchResult>, ChatError> {
    sqlx::query_as::<_, DbSearchResult>(
        r#"
        SELECT id,
               name AS title,
               ('статус: ' || status || ', роль: ' || role) AS content,
               to_timestamp(created_at) AS created_at,
               'citizens' AS source
        FROM citizens, websearch_to_tsquery('russian', $1) AS q
        WHERE to_tsvector(
                  'russian',
                  coalesce(name, '') || ' ' || coalesce(status, '') || ' ' || coalesce(role, '') || ' ' || coalesce(id, '')
              ) @@ q
        ORDER BY ts_rank(
                  to_tsvector(
                      'russian',
                      coalesce(name, '') || ' ' || coalesce(status, '') || ' ' || coalesce(role, '') || ' ' || coalesce(id, '')
                  ),
                  q
              ) DESC
        LIMIT $2
        "#,
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))
}

async fn search_votes_results(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<DbSearchResult>, ChatError> {
    sqlx::query_as::<_, DbSearchResult>(
        r#"
        SELECT v.vote_id AS id,
               v.title,
               v.description
                   || E'\nСтатус: ' || v.status
                   || COALESCE(
                       (SELECT E'\nГолосов: ' || count(*)::text
                        FROM vote_choices vc
                        WHERE vc.vote_id = v.vote_id),
                       ''
                   ) AS content,
               v.start_time AS created_at,
               'votes' AS source
        FROM votes v, websearch_to_tsquery('russian', $1) AS q
        WHERE to_tsvector(
                  'russian',
                  coalesce(v.title, '') || ' ' || coalesce(v.description, '') || ' ' || coalesce(v.status, '')
              ) @@ q
        ORDER BY ts_rank(
                  to_tsvector(
                      'russian',
                      coalesce(v.title, '') || ' ' || coalesce(v.description, '') || ' ' || coalesce(v.status, '')
                  ),
                  q
              ) DESC
        LIMIT $2
        "#,
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))
}

async fn search_events_results(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<DbSearchResult>, ChatError> {
    sqlx::query_as::<_, DbSearchResult>(
        r#"
        SELECT event_id AS id,
               title,
               description
                   || E'\nТип: ' || event_type
                   || E', инициатор: ' || initiator AS content,
               created_at,
               'events' AS source
        FROM events, websearch_to_tsquery('russian', $1) AS q
        WHERE to_tsvector(
                  'russian',
                  coalesce(title, '') || ' ' || coalesce(description, '') || ' ' || coalesce(event_type, '') || ' ' || coalesce(initiator, '')
              ) @@ q
        ORDER BY ts_rank(
                  to_tsvector(
                      'russian',
                      coalesce(title, '') || ' ' || coalesce(description, '') || ' ' || coalesce(event_type, '') || ' ' || coalesce(initiator, '')
                  ),
                  q
              ) DESC
        LIMIT $2
        "#,
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))
}

async fn search_nodes_results(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<DbSearchResult>, ChatError> {
    sqlx::query_as::<_, DbSearchResult>(
        r#"
        SELECT id,
               id AS title,
               url || E' · статус: ' || status || E', версия: ' || version AS content,
               last_seen AS created_at,
               'nodes' AS source
        FROM nodes, websearch_to_tsquery('russian', $1) AS q
        WHERE to_tsvector(
                  'russian',
                  coalesce(id, '') || ' ' || coalesce(url, '') || ' ' || coalesce(status, '') || ' ' || coalesce(version, '')
              ) @@ q
        ORDER BY ts_rank(
                  to_tsvector(
                      'russian',
                      coalesce(id, '') || ' ' || coalesce(url, '') || ' ' || coalesce(status, '') || ' ' || coalesce(version, '')
                  ),
                  q
              ) DESC
        LIMIT $2
        "#,
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| ChatError::Database(e.to_string()))
}

pub async fn search_db(
    pool: &PgPool,
    query: &str,
    scope: SearchScope,
    limit: i64,
) -> Result<Vec<DbSearchResult>, ChatError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    match scope {
        SearchScope::Chat => search_chat_results(pool, trimmed, limit).await,
        SearchScope::Citizens => search_citizens_results(pool, trimmed, limit).await,
        SearchScope::Votes => search_votes_results(pool, trimmed, limit).await,
        SearchScope::Events => search_events_results(pool, trimmed, limit).await,
        SearchScope::Nodes => search_nodes_results(pool, trimmed, limit).await,
        SearchScope::All => {
            let mut results = Vec::new();
            results.extend(search_chat_results(pool, trimmed, limit).await?);
            results.extend(search_citizens_results(pool, trimmed, limit).await?);
            results.extend(search_votes_results(pool, trimmed, limit).await?);
            results.extend(search_events_results(pool, trimmed, limit).await?);
            results.extend(search_nodes_results(pool, trimmed, limit).await?);
            results.sort_by_key(|r| std::cmp::Reverse(r.created_at));
            results.truncate(limit as usize);
            Ok(results)
        }
    }
}

fn extract_search_token(headers: &HeaderMap, query_token: Option<&str>) -> Option<String> {
    if let Some(token) = query_token.map(str::trim).filter(|t| !t.is_empty()) {
        return Some(token.to_string());
    }

    headers
        .get("x-access-token")
        .or_else(|| headers.get("X-Access-Token"))
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or_else(|| {
            headers
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(String::from)
        })
}

async fn validate_search_access(state: &Arc<AppState>, token: &str) -> bool {
    let admin_key = std::env::var("ADMIN_API_KEY").unwrap_or_default();
    if !admin_key.is_empty() && token == admin_key {
        return true;
    }

    KeyStore::validate_key(&state.db, token).await.is_some()
}

pub async fn search_db_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<DbSearchQuery>,
) -> impl IntoResponse {
    let token = match extract_search_token(&headers, query.token.as_deref()) {
        Some(token) => token,
        None => {
            return response::unauthorized("Missing access token");
        }
    };

    if !validate_search_access(&state, &token).await {
        return response::unauthorized("Invalid access token");
    }

    let q = query.q.trim();
    if q.is_empty() {
        return ChatError::bad_request("Параметр q обязателен")
            .to_response()
            .into_response();
    }

    let scope = query
        .scope
        .as_deref()
        .and_then(SearchScope::parse)
        .unwrap_or(SearchScope::All);

    let limit = clamp_search_limit(query.limit);
    match search_db(&state.db, q, scope, limit).await {
        Ok(results) => Json(ApiResponse::success(serde_json::json!({
            "query": q,
            "scope": scope.as_str(),
            "results": results,
            "total": results.len(),
        })))
        .into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn search_messages_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(query): Query<SearchMessagesQuery>,
) -> impl IntoResponse {
    if let Some(resp) = reject_chat_read(&auth) {
        return resp;
    }

    let q = query.q.trim();
    if q.is_empty() {
        return ChatError::bad_request("Параметр q обязателен")
            .to_response()
            .into_response();
    }

    let limit = clamp_search_limit(query.limit);
    match search_messages(&state.db, q, limit, None).await {
        Ok(rows) => Json(ApiResponse::success(serde_json::json!({
            "messages": rows,
            "total": rows.len(),
        })))
        .into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn list_messages_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListMessagesQuery>,
) -> impl IntoResponse {
    if let Some(resp) = reject_chat_read(&auth) {
        return resp;
    }

    match list_messages(&state.db, query).await {
        Ok(rows) => Json(ApiResponse::success(serde_json::json!({
            "messages": rows,
            "total": rows.len(),
        })))
        .into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn get_session_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(resp) = reject_chat_read(&auth) {
        return resp;
    }

    let citizen_id = match resolve_auth_citizen_id(&auth, &state.db).await {
        Ok(id) => id,
        Err(e) => return e.to_response().into_response(),
    };

    match get_active_session(&state.db, &citizen_id).await {
        Ok(Some(session)) => Json(ApiResponse::success(session)).into_response(),
        Ok(None) => Json(ApiResponse::success(serde_json::json!({
            "session": null,
            "message": "Нет активного сеанса"
        })))
        .into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn list_history_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if let Some(resp) = reject_chat_read(&auth) {
        return resp;
    }

    let citizen_id = match resolve_auth_citizen_id(&auth, &state.db).await {
        Ok(id) => id,
        Err(e) => return e.to_response().into_response(),
    };

    match list_history(&state.db, &citizen_id).await {
        Ok(archives) => Json(ApiResponse::success(serde_json::json!({
            "archives": archives,
            "total": archives.len(),
        })))
        .into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn send_message_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<SendMessageRequest>,
) -> impl IntoResponse {
    if let Some(resp) = reject_chat_send(&auth) {
        return resp;
    }

    match send_message(state, &auth, req).await {
        Ok(row) => (StatusCode::CREATED, Json(ApiResponse::success(row))).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn admin_send_message_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AdminSendMessageRequest>,
) -> impl IntoResponse {
    // Admin bot send: ADMIN_API_KEY only (not Agent / citizen API keys).
    let admin_key = std::env::var("ADMIN_API_KEY").unwrap_or_default();
    if admin_key.is_empty() || req.admin_key != admin_key {
        return response::unauthorized("Invalid admin key");
    }

    match send_message_as_bot(&state, &req.content).await {
        Ok(row) => (StatusCode::CREATED, Json(ApiResponse::success(row))).into_response(),
        Err(e) => e.to_response().into_response(),
    }
}

pub async fn gossip_chat_message_handler(
    State(state): State<Arc<AppState>>,
    Json(message): Json<ChatMessageRow>,
) -> impl IntoResponse {
    match crate::gossip::receive_gossip_chat_message(&state, message).await {
        Ok(resp) => Json(resp).into_response(),
        Err(msg) => response::bad_request(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_message_rejects_empty() {
        assert!(validate_message_content("").is_err());
        assert!(validate_message_content("   ").is_err());
    }

    #[test]
    fn validate_message_trims_and_accepts() {
        assert_eq!(validate_message_content("  hello  ").unwrap(), "hello");
    }

    #[test]
    fn validate_message_rejects_too_long() {
        let long = "x".repeat(MAX_MESSAGE_LEN + 1);
        assert!(validate_message_content(&long).is_err());
    }

    #[test]
    fn validate_message_accepts_max_length() {
        let max = "x".repeat(MAX_MESSAGE_LEN);
        assert!(validate_message_content(&max).is_ok());
    }

    #[test]
    fn clamp_limit_bounds() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(500)), MAX_LIMIT);
    }

    #[test]
    fn clamp_search_limit_bounds() {
        assert_eq!(clamp_search_limit(None), DEFAULT_SEARCH_LIMIT);
        assert_eq!(clamp_search_limit(Some(0)), 1);
        assert_eq!(clamp_search_limit(Some(500)), MAX_SEARCH_LIMIT);
    }

    #[test]
    fn search_scope_parse() {
        assert_eq!(SearchScope::parse("chat"), Some(SearchScope::Chat));
        assert_eq!(SearchScope::parse("CITIZENS"), Some(SearchScope::Citizens));
        assert_eq!(SearchScope::parse("all"), Some(SearchScope::All));
        assert_eq!(SearchScope::parse("unknown"), None);
    }

    #[test]
    fn zstd_roundtrip() {
        let raw = br#"{"messages":[{"content":"hello"}]}"#;
        let compressed = compress_zstd(raw).unwrap();
        assert!(compressed.len() < raw.len() + 64);
        assert_eq!(decompress_zstd(&compressed).unwrap(), raw);
    }

    #[test]
    fn agent_without_permissions_rejected_from_chat() {
        let agent = AuthContext::from_api_key("agent".into(), crate::types::Role::Agent);
        assert!(reject_chat_read(&agent).is_some());
        assert!(reject_chat_send(&agent).is_some());
        assert!(!agent.can_manage_chat());
    }

    #[test]
    fn agent_with_chat_permissions_passes_guards() {
        let agent = AuthContext::from_api_key("agent".into(), crate::types::Role::Agent)
            .with_agent_permissions(vec![
                crate::auth::PERM_CHAT_READ.to_string(),
                crate::auth::PERM_CHAT_SEND.to_string(),
            ]);
        assert!(reject_chat_read(&agent).is_none());
        assert!(reject_chat_send(&agent).is_none());
        assert!(!agent.can_manage_chat());
    }

    #[test]
    fn candidate_and_node_rejected_from_chat() {
        let candidate =
            AuthContext::from_api_key("cand".into(), crate::types::Role::Candidate);
        assert!(reject_chat_read(&candidate).is_some());
        assert!(reject_chat_send(&candidate).is_some());

        let node = AuthContext::node();
        assert!(reject_chat_read(&node).is_some());
        assert!(reject_chat_send(&node).is_some());
    }
}
