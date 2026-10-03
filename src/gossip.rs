use std::sync::Arc;

use crate::chat::{self, ChatMessageRow};
use crate::models::Event;
use crate::pending;
use crate::response::ApiResponse;
use crate::validator::EventValidator;
use crate::AppState;

pub async fn push_pending_to_peers(state: &Arc<AppState>, event: &Event) {
    // QUIC mesh (+ relay) first; HTTP gossip as last resort inside p2p_quic.
    crate::p2p_quic::push_gossip_event(state.clone(), event.clone()).await;
}

pub async fn receive_gossip_event(
    state: &Arc<AppState>,
    event: Event,
) -> Result<ApiResponse, String> {
    if pending::exists(&state.db, &event.event_id)
        .await
        .unwrap_or(false)
    {
        return Ok(ApiResponse::success(serde_json::json!({
            "event_id": event.event_id,
            "message": "Already in pending, skipped"
        })));
    }

    let confirmed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM events WHERE event_id = $1)",
    )
    .bind(&event.event_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);

    if confirmed {
        return Ok(ApiResponse::success(serde_json::json!({
            "event_id": event.event_id,
            "message": "Already confirmed, skipped"
        })));
    }

    if let Err(e) = EventValidator::validate_event(&event, &state.db).await {
        return Err(e.message());
    }

    match pending::insert(&state.db, &event).await {
        Ok(pending::PendingInsertResult::Inserted) => {}
        Ok(pending::PendingInsertResult::AlreadyExists) => {
            return Ok(ApiResponse::success(serde_json::json!({
                "event_id": event.event_id,
                "message": "Already in pending, skipped"
            })));
        }
        Err(e) => return Err(format!("Failed to store gossip event: {}", e)),
    }

    Ok(ApiResponse::success(serde_json::json!({
        "event_id": event.event_id,
        "message": "Gossip event accepted"
    })))
}

pub async fn push_chat_message_to_peers(state: &Arc<AppState>, message: &ChatMessageRow) {
    crate::p2p_quic::push_gossip_chat(state.clone(), message.clone()).await;
}

pub async fn receive_gossip_chat_message(
    state: &Arc<AppState>,
    message: ChatMessageRow,
) -> Result<ApiResponse, String> {
    match chat::insert_gossip_message(&state.db, &message).await {
        Ok(chat::GossipInsertResult::Inserted) => {
            crate::channel::notify_chat_message(state, &message, None).await;
            Ok(ApiResponse::success(serde_json::json!({
                "id": message.id,
                "message": "Chat message accepted"
            })))
        }
        Ok(chat::GossipInsertResult::AlreadyExists) => Ok(ApiResponse::success(serde_json::json!({
            "id": message.id,
            "message": "Already exists, skipped"
        }))),
        Err(e) => Err(format!("Failed to store gossip chat message: {}", e)),
    }
}
