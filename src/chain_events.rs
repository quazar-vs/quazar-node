//! In-core signed pending events (exchange, svod, etc.).

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::block_producer;
use crate::blockchain::signed_pending_event_public;
use crate::models::Event;
use crate::pending;
use crate::validator::EventValidator;
use crate::AppState;

pub async fn emit_signed_event(
    state: &Arc<AppState>,
    event_type: &str,
    initiator: &str,
    title: &str,
    description: &str,
    event_id: Option<&str>,
    data: serde_json::Value,
) -> Result<(), String> {
    let event_id = event_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("evt_{}", Uuid::new_v4()));
    let mut event = Event {
        event_id,
        timestamp: Utc::now().timestamp(),
        event_type: event_type.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        initiator: initiator.to_string(),
        data,
        previous_hash: "0".to_string(),
        signatures: vec![],
        hash: None,
        public: true,
    };
    event = signed_pending_event_public(event);
    if let Err(e) = EventValidator::validate_event(&event, &state.db).await {
        let msg = e.message();
        if msg.contains("already exists") {
            return Ok(());
        }
        return Err(msg);
    }
    match pending::insert(&state.db, &event).await {
        Ok(pending::PendingInsertResult::Inserted)
        | Ok(pending::PendingInsertResult::AlreadyExists) => {}
        Err(e) => return Err(e.to_string()),
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
