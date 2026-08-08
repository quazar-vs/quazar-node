//! Hermes → Quazar chain logging.
//!
//! Hermes (or Cursor on its behalf) registers Quazar-related actions as blockchain events.
//! Server builds a node-signed pending event so Hermes does not need a citizen ed25519 key.

use std::sync::Arc;

use axum::{
    extract::{Extension, State},
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::block_producer;
use crate::blockchain::signed_pending_event_public;
use crate::gossip;
use crate::models::Event;
use crate::pending;
use crate::response::{self, ApiResponse};
use crate::types::QuazarEventType;
use crate::validator::EventValidator;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct HermesLogEventRequest {
    pub event_type: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub data: serde_json::Value,
    /// Optional stable id (idempotent). Default: hermes_<uuid>
    pub event_id: Option<String>,
    pub public: Option<bool>,
}

pub async fn list_event_types_handler() -> impl IntoResponse {
    Json(ApiResponse::success(json!({
        "event_types": QuazarEventType::all_type_names(),
        "note": "Unknown types cannot be registered at runtime. Ask permission, then add to QuazarEventType via Cursor/deploy.",
    })))
}

pub async fn log_event_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<HermesLogEventRequest>,
) -> impl IntoResponse {
    if auth.is_node {
        return response::forbidden("Node credentials cannot use /hermes/event");
    }

    let event_type = body.event_type.trim().to_string();
    if QuazarEventType::from_str(&event_type).is_err() {
        return response::bad_request(format!(
            "Unknown event_type: {}. Нет такого типа события в Квазаре. Нужно разрешение на создание типа и правка QuazarEventType (через Cursor).",
            event_type
        ));
    }

    if body.title.trim().is_empty() || body.description.trim().is_empty() {
        return response::bad_request("title and description are required");
    }

    let event_id = body
        .event_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("hermes_{}", Uuid::new_v4()));

    let mut data = body.data;
    if !data.is_object() {
        data = json!({});
    }
    if let Some(obj) = data.as_object_mut() {
        obj.entry("source".to_string())
            .or_insert_with(|| json!("hermes"));
        obj.entry("logged_by".to_string())
            .or_insert_with(|| json!(auth.citizen_name.clone()));
    }

    let mut event = Event {
        event_id: event_id.clone(),
        timestamp: Utc::now().timestamp(),
        event_type,
        title: body.title.trim().to_string(),
        description: body.description.trim().to_string(),
        initiator: auth.citizen_name.clone(),
        data,
        previous_hash: "0".to_string(),
        signatures: vec![],
        hash: None,
        public: body.public.unwrap_or(true),
    };
    event = signed_pending_event_public(event);

    if let Err(e) = EventValidator::validate_event(&event, &state.db).await {
        let msg = e.message();
        if msg.contains("already exists") {
            return Json(ApiResponse::success(json!({
                "event_id": event_id,
                "status": "already_exists",
                "message": msg,
            })))
            .into_response();
        }
        tracing::warn!(error = %msg, "hermes event validation failed");
        return response::bad_request(msg);
    }

    match pending::insert(&state.db, &event).await {
        Ok(pending::PendingInsertResult::Inserted) => {
            let count = pending::count(&state.db).await.unwrap_or(0);
            let state_gossip = state.clone();
            let event_gossip = event.clone();
            tokio::spawn(async move {
                gossip::push_pending_to_peers(&state_gossip, &event_gossip).await;
            });
            let state_clone = state.clone();
            tokio::spawn(async move {
                block_producer::try_create_block(state_clone).await;
            });
            Json(ApiResponse::success(json!({
                "event_id": event.event_id,
                "pending_count": count,
                "status": "inserted",
                "message": "Hermes event added to pending queue",
            })))
            .into_response()
        }
        Ok(pending::PendingInsertResult::AlreadyExists) => Json(ApiResponse::success(json!({
            "event_id": event.event_id,
            "status": "already_exists",
            "message": "Event already in pending queue",
        })))
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "hermes failed to add pending event");
            response::internal_error(e)
        }
    }
}
