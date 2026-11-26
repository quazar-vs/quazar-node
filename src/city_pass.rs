//! City registration (прописка) and city passes (пропуски).
//!
//! - `home_node_id` on citizen + passport = city of registration.
//! - Home pass is auto-issued with the passport.
//! - Visit passes to other cities are issued by that city's Mayor / СБ (Guardian+) .

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::chain_events;
use crate::offices;
use crate::response::ApiResponse;
use crate::types::Role;
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CityPass {
    pub id: String,
    pub citizen_id: String,
    pub passport_id: String,
    pub node_id: String,
    pub kind: String,
    pub status: String,
    pub issued_by: String,
    pub issued_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PassportBook {
    pub passport_id: String,
    pub citizen_id: String,
    pub citizen_name: String,
    pub home_node_id: Option<String>,
    pub issued_at: i64,
    pub expires_at: i64,
    pub is_valid: bool,
    pub passes: Vec<CityPass>,
}

#[derive(Debug, Deserialize)]
pub struct IssueVisitPassRequest {
    pub citizen_id: String,
    pub expires_in_days: Option<u64>,
}

/// After passport row exists: set прописка and ensure home pass.
pub async fn ensure_home_registration(
    pool: &PgPool,
    citizen_id: &str,
    passport_id: &str,
    home_node_id: &str,
    issued_at: i64,
    expires_at: i64,
    issued_by: &str,
) -> Result<(), String> {
    let home = home_node_id.trim();
    if home.is_empty() {
        return Err("home_node_id required".into());
    }

    sqlx::query(
        "UPDATE citizens SET home_node_id = COALESCE(NULLIF(btrim(home_node_id), ''), $1) WHERE id = $2",
    )
    .bind(home)
    .bind(citizen_id)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query(
        "UPDATE passports SET home_node_id = $1 WHERE id = $2",
    )
    .bind(home)
    .bind(passport_id)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    // Revoke stale active home pass on a different node (re-issue edge case).
    sqlx::query(
        r#"
        UPDATE city_passes
        SET status = 'revoked', revoked_at = $1
        WHERE citizen_id = $2
          AND kind = 'home'
          AND status = 'active'
          AND node_id <> $3
        "#,
    )
    .bind(issued_at)
    .bind(citizen_id)
    .bind(home)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    let existing: Option<String> = sqlx::query_scalar(
        r#"
        SELECT id FROM city_passes
        WHERE citizen_id = $1 AND node_id = $2 AND status = 'active'
        LIMIT 1
        "#,
    )
    .bind(citizen_id)
    .bind(home)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;

    if existing.is_some() {
        return Ok(());
    }

    let pass_id = format!("pass_home_{passport_id}");
    sqlx::query(
        r#"
        INSERT INTO city_passes (
            id, citizen_id, passport_id, node_id, kind, status, issued_by, issued_at, expires_at
        ) VALUES ($1, $2, $3, $4, 'home', 'active', $5, $6, $7)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(&pass_id)
    .bind(citizen_id)
    .bind(passport_id)
    .bind(home)
    .bind(issued_by)
    .bind(issued_at)
    .bind(expires_at)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(())
}

pub async fn list_passes_for_citizen(
    pool: &PgPool,
    citizen_id: &str,
) -> Result<Vec<CityPass>, sqlx::Error> {
    sqlx::query_as::<_, CityPass>(
        r#"
        SELECT id, citizen_id, passport_id, node_id, kind, status,
               issued_by, issued_at, expires_at, revoked_at
        FROM city_passes
        WHERE citizen_id = $1
        ORDER BY kind ASC, issued_at DESC
        "#,
    )
    .bind(citizen_id)
    .fetch_all(pool)
    .await
}

pub async fn load_passport_book(
    pool: &PgPool,
    citizen_id: &str,
) -> Result<Option<PassportBook>, sqlx::Error> {
    let row: Option<(String, String, Option<String>, i64, i64, bool)> = sqlx::query_as(
        r#"
        SELECT p.id, c.name, p.home_node_id, p.issued_at, p.expires_at, p.is_valid
        FROM passports p
        JOIN citizens c ON c.id = p.citizen_id
        WHERE p.citizen_id = $1 AND p.is_valid = TRUE
        ORDER BY p.issued_at DESC
        LIMIT 1
        "#,
    )
    .bind(citizen_id)
    .fetch_optional(pool)
    .await?;

    let Some((passport_id, citizen_name, home_node_id, issued_at, expires_at, is_valid)) = row
    else {
        return Ok(None);
    };

    let passes = list_passes_for_citizen(pool, citizen_id).await?;
    Ok(Some(PassportBook {
        passport_id,
        citizen_id: citizen_id.to_string(),
        citizen_name,
        home_node_id,
        issued_at,
        expires_at,
        is_valid,
        passes,
    }))
}

/// Valid access to a city: home registration or active visit pass (not expired).
pub async fn has_city_access(
    pool: &PgPool,
    citizen_id: &str,
    node_id: &str,
) -> Result<bool, sqlx::Error> {
    let now = Utc::now().timestamp();
    let home: Option<String> =
        sqlx::query_scalar("SELECT home_node_id FROM citizens WHERE id = $1")
            .bind(citizen_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    if home.as_deref() == Some(node_id) {
        return Ok(true);
    }
    let n: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint FROM city_passes
        WHERE citizen_id = $1
          AND node_id = $2
          AND status = 'active'
          AND (expires_at IS NULL OR expires_at > $3)
        "#,
    )
    .bind(citizen_id)
    .bind(node_id)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

async fn can_issue_city_pass(
    state: &AppState,
    auth: &AuthContext,
    node_id: &str,
) -> Result<bool, String> {
    // Republic level: СБ — функция Айи.
    if matches!(auth.role, Role::Founder | Role::Aiya) {
        return Ok(true);
    }
    // Мэр — только своего города (офис на node_id).
    let actor = auth.resolve_account_id(&state.db).await;
    let mine = offices::active_mayor_of(&state.db, node_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(matches!(mine, Some((cid, _)) if cid == actor))
}

pub async fn issue_visit_pass(
    state: &Arc<AppState>,
    auth: &AuthContext,
    node_id: &str,
    citizen_id: &str,
    expires_in_days: Option<u64>,
) -> Result<CityPass, (StatusCode, String)> {
    let node_id = node_id.trim();
    let citizen_id = citizen_id.trim();
    if node_id.is_empty() || citizen_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "node_id and citizen_id required".into()));
    }

    match can_issue_city_pass(state, auth, node_id).await {
        Ok(true) => {}
        Ok(false) => {
            return Err((
                StatusCode::FORBIDDEN,
                "Только СБ/Мэр этого города (или Aiya/Founder) может выдать пропуск".into(),
            ));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
    }

    let book = load_passport_book(&state.db, citizen_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::BAD_REQUEST,
            "У гражданина нет действующего паспорта".to_string(),
        ))?;

    if !book.is_valid {
        return Err((StatusCode::BAD_REQUEST, "Паспорт недействителен".into()));
    }

    if book.home_node_id.as_deref() == Some(node_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            "В городе прописки пропуск выдаётся автоматически с паспортом".into(),
        ));
    }

    let now = Utc::now().timestamp();
    let expires_at = expires_in_days
        .map(|d| now + (d as i64) * 86400)
        .or(Some(book.expires_at));

    // Replace previous active visit pass for this city.
    sqlx::query(
        r#"
        UPDATE city_passes
        SET status = 'revoked', revoked_at = $1
        WHERE citizen_id = $2 AND node_id = $3 AND status = 'active'
        "#,
    )
    .bind(now)
    .bind(citizen_id)
    .bind(node_id)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let pass_id = format!("pass_visit_{}", Uuid::new_v4());
    let issued_by = if auth.citizen_name.is_empty() {
        auth.role.as_str().to_string()
    } else {
        auth.citizen_name.clone()
    };

    sqlx::query(
        r#"
        INSERT INTO city_passes (
            id, citizen_id, passport_id, node_id, kind, status, issued_by, issued_at, expires_at
        ) VALUES ($1, $2, $3, $4, 'visit', 'active', $5, $6, $7)
        "#,
    )
    .bind(&pass_id)
    .bind(citizen_id)
    .bind(&book.passport_id)
    .bind(node_id)
    .bind(&issued_by)
    .bind(now)
    .bind(expires_at)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let _ = chain_events::emit_signed_event(
        state,
        "CityPassIssued",
        &issued_by,
        "City visit pass issued",
        &format!("{citizen_id} → {node_id}"),
        Some(&format!("evt_city_pass_{pass_id}")),
        json!({
            "pass_id": pass_id,
            "citizen_id": citizen_id,
            "passport_id": book.passport_id,
            "node_id": node_id,
            "kind": "visit",
            "expires_at": expires_at,
        }),
    )
    .await;

    sqlx::query_as::<_, CityPass>(
        r#"
        SELECT id, citizen_id, passport_id, node_id, kind, status,
               issued_by, issued_at, expires_at, revoked_at
        FROM city_passes WHERE id = $1
        "#,
    )
    .bind(&pass_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn revoke_pass(
    state: &Arc<AppState>,
    auth: &AuthContext,
    node_id: &str,
    pass_id: &str,
) -> Result<CityPass, (StatusCode, String)> {
    let pass: CityPass = sqlx::query_as(
        r#"
        SELECT id, citizen_id, passport_id, node_id, kind, status,
               issued_by, issued_at, expires_at, revoked_at
        FROM city_passes WHERE id = $1
        "#,
    )
    .bind(pass_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .ok_or((StatusCode::NOT_FOUND, "Пропуск не найден".into()))?;

    if pass.node_id != node_id {
        return Err((StatusCode::BAD_REQUEST, "Пропуск принадлежит другому городу".into()));
    }
    if pass.kind == "home" {
        return Err((
            StatusCode::BAD_REQUEST,
            "Домашний пропуск (прописка) отзывается только с паспортом".into(),
        ));
    }
    if pass.status != "active" {
        return Err((StatusCode::BAD_REQUEST, "Пропуск уже неактивен".into()));
    }

    match can_issue_city_pass(state, auth, node_id).await {
        Ok(true) => {}
        Ok(false) => {
            return Err((
                StatusCode::FORBIDDEN,
                "Недостаточно прав для отзыва пропуска".into(),
            ));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
    }

    let now = Utc::now().timestamp();
    sqlx::query(
        "UPDATE city_passes SET status = 'revoked', revoked_at = $1 WHERE id = $2",
    )
    .bind(now)
    .bind(pass_id)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let issued_by = if auth.citizen_name.is_empty() {
        auth.role.as_str().to_string()
    } else {
        auth.citizen_name.clone()
    };
    let _ = chain_events::emit_signed_event(
        state,
        "CityPassRevoked",
        &issued_by,
        "City visit pass revoked",
        &format!("{} @ {}", pass.citizen_id, node_id),
        Some(&format!("evt_city_pass_rev_{pass_id}")),
        json!({
            "pass_id": pass_id,
            "citizen_id": pass.citizen_id,
            "node_id": node_id,
        }),
    )
    .await;

    sqlx::query_as::<_, CityPass>(
        r#"
        SELECT id, citizen_id, passport_id, node_id, kind, status,
               issued_by, issued_at, expires_at, revoked_at
        FROM city_passes WHERE id = $1
        "#,
    )
    .bind(pass_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn revoke_all_for_passport(pool: &PgPool, passport_id: &str, at: i64) -> Result<(), String> {
    sqlx::query(
        r#"
        UPDATE city_passes
        SET status = 'revoked', revoked_at = $1
        WHERE passport_id = $2 AND status = 'active'
        "#,
    )
    .bind(at)
    .bind(passport_id)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

// --- HTTP ---

pub async fn get_passport_book_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(citizen_id): Path<String>,
) -> impl IntoResponse {
    let self_ok = auth.citizen_id.as_deref() == Some(citizen_id.as_str());
    if !self_ok && !auth.can_manage_citizens() {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiResponse::error("Недостаточно прав".to_string())),
        )
            .into_response();
    }
    match load_passport_book(&state.db, &citizen_id).await {
        Ok(Some(book)) => (StatusCode::OK, Json(ApiResponse::success(book))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ApiResponse::error("Паспорт не найден".to_string())),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse::error(e.to_string())),
        )
            .into_response(),
    }
}

pub async fn issue_visit_pass_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(node_id): Path<String>,
    Json(req): Json<IssueVisitPassRequest>,
) -> impl IntoResponse {
    match issue_visit_pass(&state, &auth, &node_id, &req.citizen_id, req.expires_in_days).await {
        Ok(pass) => (StatusCode::OK, Json(ApiResponse::success(pass))).into_response(),
        Err((code, msg)) => (code, Json(ApiResponse::error(msg))).into_response(),
    }
}

pub async fn revoke_pass_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path((node_id, pass_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match revoke_pass(&state, &auth, &node_id, &pass_id).await {
        Ok(pass) => (StatusCode::OK, Json(ApiResponse::success(pass))).into_response(),
        Err((code, msg)) => (code, Json(ApiResponse::error(msg))).into_response(),
    }
}

pub async fn check_city_access_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path((node_id, citizen_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let self_ok = auth.citizen_id.as_deref() == Some(citizen_id.as_str());
    if !self_ok && !auth.can_manage_citizens() && !matches!(auth.role, Role::Mayor) {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiResponse::error("Недостаточно прав".to_string())),
        )
            .into_response();
    }
    match has_city_access(&state.db, &citizen_id, &node_id).await {
        Ok(allowed) => (
            StatusCode::OK,
            Json(ApiResponse::success(json!({
                "citizen_id": citizen_id,
                "node_id": node_id,
                "allowed": allowed,
            }))),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse::error(e.to_string())),
        )
            .into_response(),
    }
}
