//! Typed Algonism agents: secretary / sb / exchange × state|city. Peer messages via core.

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::chain_events;
use crate::response::ApiResponse;
use crate::AppState;

fn now() -> i64 {
    Utc::now().timestamp()
}

#[derive(Debug)]
pub enum AgentApiError {
    Db(String),
    Bad(String),
    Forbidden(String),
    NotFound(String),
}

impl IntoResponse for AgentApiError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match &self {
            AgentApiError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            AgentApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            AgentApiError::Bad(m) => (StatusCode::BAD_REQUEST, m.clone()),
            AgentApiError::Db(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        };
        (code, Json(ApiResponse::error(msg))).into_response()
    }
}

type R<T> = Result<T, AgentApiError>;

pub async fn ensure_state_agents(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Map existing secretary citizen if present
    let secretary_id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM citizens WHERE name = 'secretary' OR id = 'secretary' LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    if let Some(cid) = secretary_id {
        upsert_agent(pool, &cid, "secretary", "state", None, None).await?;
    }
    // SB / Exchange: virtual bindings to Founder ops until dedicated citizens exist —
    // use QZ-1 as holder for state sb/exchange agent rows (no vote).
    upsert_agent(pool, "QZ-1", "sb", "state", None, None).await?;
    upsert_agent(pool, "QZ-1", "exchange", "state", None, None).await?;
    upsert_agent(pool, "QZ-1", "lawyer", "state", None, None).await?;
    Ok(())
}

async fn upsert_agent(
    pool: &PgPool,
    citizen_id: &str,
    kind: &str,
    scope: &str,
    city_id: Option<&str>,
    node_id: Option<&str>,
) -> Result<String, sqlx::Error> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM algonism_agents
         WHERE agent_kind = $1 AND scope = $2
           AND COALESCE(city_id,'') = COALESCE($3,'')
           AND COALESCE(node_id,'') = COALESCE($4,'')
         LIMIT 1",
    )
    .bind(kind)
    .bind(scope)
    .bind(city_id)
    .bind(node_id)
    .fetch_optional(pool)
    .await?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let id = format!("agent_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO algonism_agents
         (id, citizen_id, agent_kind, scope, city_id, node_id, status, created_at)
         VALUES ($1,$2,$3,$4,$5,$6,'active',$7)",
    )
    .bind(&id)
    .bind(citizen_id)
    .bind(kind)
    .bind(scope)
    .bind(city_id)
    .bind(node_id)
    .bind(now())
    .execute(pool)
    .await?;
    Ok(id)
}

/// Create city triple when mayor office starts.
pub async fn ensure_city_agents(
    pool: &PgPool,
    node_id: &str,
    city_id: &str,
    mayor_citizen_id: &str,
) -> Result<(), sqlx::Error> {
    upsert_agent(pool, mayor_citizen_id, "secretary", "city", Some(city_id), Some(node_id)).await?;
    upsert_agent(pool, mayor_citizen_id, "sb", "city", Some(city_id), Some(node_id)).await?;
    upsert_agent(pool, mayor_citizen_id, "exchange", "city", Some(city_id), Some(node_id)).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ListAgentsQuery {
    pub scope: Option<String>,
    pub agent_kind: Option<String>,
    pub node_id: Option<String>,
}

pub async fn list_agents(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListAgentsQuery>,
) -> R<Json<ApiResponse>> {
    let _ = ensure_state_agents(&state.db)
        .await
        .map_err(|e| AgentApiError::Db(e.to_string()))?;
    let rows: Vec<(String, String, String, String, Option<String>, Option<String>, String, i64)> =
        sqlx::query_as(
            "SELECT id, citizen_id, agent_kind, scope, city_id, node_id, status, created_at
             FROM algonism_agents
             WHERE status = 'active'
               AND ($1::text IS NULL OR scope = $1)
               AND ($2::text IS NULL OR agent_kind = $2)
               AND ($3::text IS NULL OR node_id = $3)
             ORDER BY scope, agent_kind",
        )
        .bind(q.scope.as_deref())
        .bind(q.agent_kind.as_deref())
        .bind(q.node_id.as_deref())
        .fetch_all(&state.db)
        .await
        .map_err(|e| AgentApiError::Db(e.to_string()))?;

    let out: Vec<_> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.0,
                "citizen_id": r.1,
                "agent_kind": r.2,
                "scope": r.3,
                "city_id": r.4,
                "node_id": r.5,
                "status": r.6,
                "created_at": r.7,
            })
        })
        .collect();
    Ok(Json(ApiResponse::success(out)))
}

#[derive(Debug, Deserialize)]
pub struct AgentMessageBody {
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub body: String,
}

/// POST /agents/messages — peer channel (local ↔ state on equal footing).
pub async fn post_agent_message(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<AgentMessageBody>,
) -> R<Json<ApiResponse>> {
    if body.body.trim().is_empty() {
        return Err(AgentApiError::Bad("body required".into()));
    }
    // Agent role or Guardian+ / Founder
    if !(auth.role.is_agent()
        || auth.can_exchange_security()
        || auth.can_add_balance()
        || auth.can_submit_need_for_citizen())
    {
        return Err(AgentApiError::Forbidden(
            "Only agents or Страж+ may use agent peer channel".into(),
        ));
    }

    let from_ok: Option<String> = sqlx::query_scalar(
        "SELECT id FROM algonism_agents WHERE id = $1 AND status = 'active'",
    )
    .bind(&body.from_agent_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AgentApiError::Db(e.to_string()))?;
    let to_ok: Option<String> = sqlx::query_scalar(
        "SELECT id FROM algonism_agents WHERE id = $1 AND status = 'active'",
    )
    .bind(&body.to_agent_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AgentApiError::Db(e.to_string()))?;
    if from_ok.is_none() || to_ok.is_none() {
        return Err(AgentApiError::NotFound("agent not found".into()));
    }

    let id = format!("amsg_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO agent_messages (id, from_agent_id, to_agent_id, body, created_at)
         VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(&id)
    .bind(&body.from_agent_id)
    .bind(&body.to_agent_id)
    .bind(body.body.trim())
    .bind(now())
    .execute(&state.db)
    .await
    .map_err(|e| AgentApiError::Db(e.to_string()))?;

    let desc = format!("{} → {}", body.from_agent_id, body.to_agent_id);
    let evt_id = format!("evt_{id}");
    let _ = chain_events::emit_signed_event(
        &state,
        "HermesExternalAction",
        &auth.citizen_name,
        "Agent peer message",
        &desc,
        Some(&evt_id),
        json!({
            "action": "agent_peer_message",
            "source": "algonism_agents",
            "message_id": id,
            "from_agent_id": body.from_agent_id,
            "to_agent_id": body.to_agent_id,
        }),
    )
    .await;

    Ok(Json(ApiResponse::success(json!({
        "id": id,
        "from_agent_id": body.from_agent_id,
        "to_agent_id": body.to_agent_id,
    }))))
}

#[derive(Debug, Deserialize)]
pub struct LawyerImpulseBody {
    pub action: String,
    pub reflex: String,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub citizen: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub extra: Option<serde_json::Value>,
}

/// POST /agents/lawyer/impulse — Lawyer cite/drift/boundary → реестр (LawyerAction).
pub async fn post_lawyer_impulse(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<LawyerImpulseBody>,
) -> R<Json<ApiResponse>> {
    if !(auth.role.is_agent()
        || auth.can_exchange_security()
        || auth.can_add_balance()
        || auth.can_submit_need_for_citizen())
    {
        return Err(AgentApiError::Forbidden(
            "Only agents or Страж+ may log Lawyer impulses".into(),
        ));
    }
    let action = body.action.trim().to_string();
    let reflex = body.reflex.trim().to_string();
    if action.is_empty() || reflex.is_empty() {
        return Err(AgentApiError::Bad("action and reflex required".into()));
    }
    let query = body
        .query
        .as_deref()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect::<String>();
    let summary = body
        .summary
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.chars().take(400).collect::<String>())
        .unwrap_or_else(|| format!("Lawyer {action} ({reflex})"));
    let evt_id = format!("evt_lawyer_{}", Uuid::new_v4());
    let mut data = json!({
        "action": action,
        "source": "lawyer_agent",
        "reflex": reflex,
        "logged_by": auth.citizen_name,
    });
    if let Some(obj) = data.as_object_mut() {
        if !query.is_empty() {
            obj.insert("query".into(), json!(query));
        }
        if let Some(c) = body.citizen.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            obj.insert("citizen".into(), json!(c));
        }
        if let Some(extra) = body.extra {
            obj.insert("extra".into(), extra);
        }
    }
    chain_events::emit_signed_event(
        &state,
        "LawyerAction",
        &auth.citizen_name,
        "Lawyer action",
        &summary,
        Some(&evt_id),
        data,
    )
    .await
    .map_err(AgentApiError::Bad)?;
    Ok(Json(ApiResponse::success(json!({
        "event_id": evt_id,
        "event_type": "LawyerAction",
        "action": action,
        "reflex": reflex,
        "status": "inserted",
    }))))
}

#[derive(Debug, Deserialize)]
pub struct EnsureCityAgentsBody {
    pub node_id: String,
    pub city_id: Option<String>,
    pub mayor_citizen_id: String,
}

pub async fn ensure_city_agents_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<EnsureCityAgentsBody>,
) -> R<Json<ApiResponse>> {
    if !(auth.can_add_balance() || auth.can_submit_need_for_citizen()) {
        return Err(AgentApiError::Forbidden(
            "Only Aiya/Secretary may bootstrap city agents".into(),
        ));
    }
    let city = body.city_id.unwrap_or_else(|| body.node_id.clone());
    ensure_city_agents(&state.db, &body.node_id, &city, &body.mayor_citizen_id)
        .await
        .map_err(|e| AgentApiError::Db(e.to_string()))?;
    Ok(Json(ApiResponse::success(json!({
        "node_id": body.node_id,
        "city_id": city,
        "agents": ["secretary", "sb", "exchange"],
        "scope": "city",
    }))))
}
