//! Mayor as temporary node office (not a status). Phase A / 2B.

use axum::{
    extract::{Extension, Path, State},
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
use crate::types::Role;
use crate::AppState;

fn now() -> i64 {
    Utc::now().timestamp()
}

#[derive(Debug)]
pub enum OfficeError {
    Db(String),
    Bad(String),
    Forbidden(String),
    NotFound(String),
}

impl IntoResponse for OfficeError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match &self {
            OfficeError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            OfficeError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            OfficeError::Bad(m) => (StatusCode::BAD_REQUEST, m.clone()),
            OfficeError::Db(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        };
        (code, Json(ApiResponse::error(msg))).into_response()
    }
}

type R<T> = Result<T, OfficeError>;

pub async fn active_mayor_of(pool: &PgPool, node_id: &str) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT citizen_id, previous_role FROM node_offices
         WHERE node_id = $1 AND office = 'mayor' AND status = 'active'
         LIMIT 1",
    )
    .bind(node_id)
    .fetch_optional(pool)
    .await
}

pub async fn is_active_mayor(pool: &PgPool, citizen_id: &str) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM node_offices
         WHERE citizen_id = $1 AND office = 'mayor' AND status = 'active'",
    )
    .bind(citizen_id)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// Appoint mayor office without changing base Role (stores previous_role).
pub async fn appoint_mayor(
    pool: &PgPool,
    node_id: &str,
    citizen_id: &str,
    previous_role: &str,
) -> Result<String, sqlx::Error> {
    // End any active mayor on this node
    sqlx::query(
        "UPDATE node_offices SET status = 'ended', ended_at = $1
         WHERE node_id = $2 AND office = 'mayor' AND status = 'active'",
    )
    .bind(now())
    .bind(node_id)
    .execute(pool)
    .await?;

    let id = format!("office_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO node_offices
         (id, node_id, office, citizen_id, previous_role, status, started_at)
         VALUES ($1,$2,'mayor',$3,$4,'active',$5)",
    )
    .bind(&id)
    .bind(node_id)
    .bind(citizen_id)
    .bind(previous_role)
    .bind(now())
    .execute(pool)
    .await?;
    Ok(id)
}

/// Leave mayor office and restore previous_role on citizens.role.
pub async fn leave_mayor(pool: &PgPool, node_id: &str) -> Result<Option<(String, String)>, sqlx::Error> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT id, citizen_id, previous_role FROM node_offices
         WHERE node_id = $1 AND office = 'mayor' AND status = 'active'
         FOR UPDATE",
    )
    .bind(node_id)
    .fetch_optional(pool)
    .await?;

    let Some((oid, citizen_id, previous_role)) = row else {
        return Ok(None);
    };

    sqlx::query(
        "UPDATE node_offices SET status = 'ended', ended_at = $1 WHERE id = $2",
    )
    .bind(now())
    .bind(&oid)
    .execute(pool)
    .await?;

    sqlx::query("UPDATE citizens SET role = $1 WHERE id = $2")
        .bind(&previous_role)
        .bind(&citizen_id)
        .execute(pool)
        .await?;

    Ok(Some((citizen_id, previous_role)))
}

#[derive(Debug, Deserialize)]
pub struct AppointCityRoleBody {
    pub citizen_id: String,
    pub role: String, // Guardian | Judge
    pub node_id: String,
}

/// POST /city/appoint — Мэр назначает Стража/Судью своего города.
pub async fn appoint_city_role(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<AppointCityRoleBody>,
) -> R<Json<ApiResponse>> {
    let mayor_id = auth.resolve_account_id(&state.db).await;
    let is_mayor = is_active_mayor(&state.db, &mayor_id)
        .await
        .map_err(|e| OfficeError::Db(e.to_string()))?;
    if !is_mayor && !auth.can_add_balance() {
        return Err(OfficeError::Forbidden(
            "Только Мэр узла (или Aiya) может назначать Стража/Судью города".into(),
        ));
    }
    if is_mayor {
        let mine = active_mayor_of(&state.db, &body.node_id)
            .await
            .map_err(|e| OfficeError::Db(e.to_string()))?;
        match mine {
            Some((cid, _)) if cid == mayor_id => {}
            _ => {
                return Err(OfficeError::Forbidden(
                    "Мэр может назначать только в своём городе".into(),
                ));
            }
        }
    }

    let role = match body.role.trim() {
        "Guardian" | "Страж" => Role::Guardian,
        "Judge" | "Судья" => Role::Judge,
        other => {
            return Err(OfficeError::Bad(format!(
                "role must be Guardian or Judge, got {other}"
            )));
        }
    };

    let exists: Option<String> = sqlx::query_scalar(
        "SELECT id FROM citizens WHERE id = $1 AND status = 'active'",
    )
    .bind(&body.citizen_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| OfficeError::Db(e.to_string()))?;
    if exists.is_none() {
        return Err(OfficeError::NotFound("citizen not found".into()));
    }

    sqlx::query("UPDATE citizens SET role = $1 WHERE id = $2")
        .bind(role.as_str())
        .bind(&body.citizen_id)
        .execute(&state.db)
        .await
        .map_err(|e| OfficeError::Db(e.to_string()))?;

    let desc = format!("{} → {}", body.citizen_id, role.as_str());
    let evt_id = format!("evt_city_appt_{}", Uuid::new_v4());
    let _ = chain_events::emit_signed_event(
        &state,
        "HermesExternalAction",
        &auth.citizen_name,
        "City role appointed by Mayor",
        &desc,
        Some(&evt_id),
        json!({
            "action": "city_role_appointed",
            "source": "mayor_office",
            "citizen_id": body.citizen_id,
            "role": role.as_str(),
            "node_id": body.node_id,
            "appointed_by": mayor_id,
        }),
    )
    .await;

    Ok(Json(ApiResponse::success(json!({
        "citizen_id": body.citizen_id,
        "role": role.as_str(),
        "node_id": body.node_id,
    }))))
}

#[derive(Debug, Deserialize)]
pub struct MayorOfficeBody {
    pub node_id: String,
    pub citizen_id: String,
}

/// POST /offices/mayor — назначить должность Мэра (новый узел / после выборов).
pub async fn appoint_mayor_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<MayorOfficeBody>,
) -> R<Json<ApiResponse>> {
    if !(auth.can_add_balance() || auth.can_submit_need_for_citizen()) {
        return Err(OfficeError::Forbidden(
            "Назначение Мэра — Aiya/Founder или Секретарь от имени Айи".into(),
        ));
    }
    let prev: Option<String> = sqlx::query_scalar("SELECT role FROM citizens WHERE id = $1")
        .bind(&body.citizen_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| OfficeError::Db(e.to_string()))?;
    let Some(previous_role) = prev else {
        return Err(OfficeError::NotFound("citizen not found".into()));
    };
    // Do not overwrite base role with Mayor — office table is source of truth.
    let office_id = appoint_mayor(&state.db, &body.node_id, &body.citizen_id, &previous_role)
        .await
        .map_err(|e| OfficeError::Db(e.to_string()))?;

    let _ = crate::agents::ensure_city_agents(
        &state.db,
        &body.node_id,
        &body.node_id,
        &body.citizen_id,
    )
    .await;

    let desc = format!("{} mayor of {}", body.citizen_id, body.node_id);
    let evt_id = format!("evt_{office_id}");
    let _ = chain_events::emit_signed_event(
        &state,
        "MayorConfirmed",
        &auth.citizen_name,
        "Mayor office appointed",
        &desc,
        Some(&evt_id),
        json!({
            "office_id": office_id,
            "application_id": office_id,
            "node_id": body.node_id,
            "citizen_id": body.citizen_id,
            "previous_role": previous_role,
        }),
    )
    .await;

    Ok(Json(ApiResponse::success(json!({
        "office_id": office_id,
        "node_id": body.node_id,
        "citizen_id": body.citizen_id,
        "previous_role": previous_role,
        "status": "active",
    }))))
}

/// POST /offices/mayor/:node_id/leave — сложить полномочия, restore previous_role.
pub async fn leave_mayor_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(node_id): Path<String>,
) -> R<Json<ApiResponse>> {
    let caller = auth.resolve_account_id(&state.db).await;
    let active = active_mayor_of(&state.db, &node_id)
        .await
        .map_err(|e| OfficeError::Db(e.to_string()))?;
    let Some((mayor_id, _)) = &active else {
        return Err(OfficeError::NotFound("no active mayor".into()));
    };
    if mayor_id != &caller && !auth.can_add_balance() {
        return Err(OfficeError::Forbidden("not your office".into()));
    }
    let restored = leave_mayor(&state.db, &node_id)
        .await
        .map_err(|e| OfficeError::Db(e.to_string()))?;
    Ok(Json(ApiResponse::success(json!({
        "node_id": node_id,
        "restored": restored,
    }))))
}

/// GET /offices/mayor/:node_id
pub async fn get_mayor_office(
    State(state): State<Arc<AppState>>,
    Path(node_id): Path<String>,
) -> R<Json<ApiResponse>> {
    let row: Option<(String, String, String, i64)> = sqlx::query_as(
        "SELECT id, citizen_id, previous_role, started_at FROM node_offices
         WHERE node_id = $1 AND office = 'mayor' AND status = 'active'",
    )
    .bind(&node_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| OfficeError::Db(e.to_string()))?;
    match row {
        Some((id, citizen_id, previous_role, started_at)) => Ok(Json(ApiResponse::success(json!({
            "office_id": id,
            "node_id": node_id,
            "citizen_id": citizen_id,
            "previous_role": previous_role,
            "started_at": started_at,
            "office": "mayor",
        })))),
        None => Err(OfficeError::NotFound("no active mayor".into())),
    }
}
