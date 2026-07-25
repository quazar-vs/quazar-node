//! Unified candidates store (citizenship / mayor / role).
//!
//! `candidates` is the source of truth for portal kinds (citizenship / mayor).
//! Legacy tables may still exist for migrations and role-election votes;
//! HTTP facades (`/api/mayor/*`, `/api/applications`) read/write via this module.
//! Role candidacies keep vote tallies in `candidacies`; listing mirrors into `candidates`.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::response::{self, ApiResponse};
use crate::AppState;

pub const KIND_CITIZENSHIP: &str = "citizenship";
pub const KIND_MAYOR: &str = "mayor";
pub const KIND_ROLE: &str = "role";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CandidateRow {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub payload: Value,
    pub source: Option<String>,
    pub citizen_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListCandidatesQuery {
    pub kind: Option<String>,
    pub status: Option<String>,
    /// When true (default for mayor without kind filter in Passport), include all kinds.
    pub street_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewCandidateRequest {
    pub note: Option<String>,
    pub ip: Option<String>,
}

const CANDIDATE_SELECT: &str = r#"
    SELECT
        id::text AS id,
        kind,
        status,
        email,
        display_name,
        payload,
        source,
        citizen_id,
        created_at,
        updated_at,
        decided_at,
        decided_by
    FROM candidates
"#;

pub async fn get_candidate(pool: &PgPool, id: Uuid) -> Result<Option<CandidateRow>, sqlx::Error> {
    sqlx::query_as::<_, CandidateRow>(
        r#"
        SELECT
            id::text AS id,
            kind,
            status,
            email,
            display_name,
            payload,
            source,
            citizen_id,
            created_at,
            updated_at,
            decided_at,
            decided_by
        FROM candidates
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn list_candidates(
    pool: &PgPool,
    query: &ListCandidatesQuery,
) -> Result<Vec<CandidateRow>, sqlx::Error> {
    let kind = query
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let status = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let street_only = query.street_only.unwrap_or(false);

    if let Some(k) = kind {
        if !matches!(k, KIND_CITIZENSHIP | KIND_MAYOR | KIND_ROLE) {
            return Ok(Vec::new());
        }
    }

    let mut sql = format!("{CANDIDATE_SELECT} WHERE 1=1");
    let mut bind_kind: Option<&str> = None;
    let mut bind_status: Option<&str> = None;
    let mut idx = 1u32;

    if let Some(k) = kind {
        sql.push_str(&format!(" AND kind = ${idx}"));
        bind_kind = Some(k);
        idx += 1;
    }
    if let Some(s) = status {
        sql.push_str(&format!(" AND status = ${idx}"));
        bind_status = Some(s);
        idx += 1;
    }
    if street_only {
        sql.push_str(" AND kind = 'mayor' AND COALESCE((payload->>'street')::boolean, false) = true");
    }
    let _ = idx;
    sql.push_str(
        r#"
        ORDER BY
          CASE status
              WHEN 'pending' THEN 0
              WHEN 'confirmed' THEN 1
              WHEN 'Active' THEN 1
              WHEN 'approved' THEN 2
              WHEN 'Approved' THEN 2
              ELSE 3
          END,
          created_at DESC
        LIMIT 500
        "#,
    );

    let mut q = sqlx::query_as::<_, CandidateRow>(sqlx::AssertSqlSafe(sql));
    if let Some(k) = bind_kind {
        q = q.bind(k);
    }
    if let Some(s) = bind_status {
        q = q.bind(s);
    }
    q.fetch_all(pool).await
}

/// Upsert a citizenship candidate (same id as applications.id).
pub async fn upsert_citizenship(
    pool: &PgPool,
    id: Uuid,
    email: &str,
    display_name: &str,
    status: &str,
    citizen_id: Option<&str>,
    payload: Value,
    decided_at: Option<DateTime<Utc>>,
    decided_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO candidates (
            id, kind, status, email, display_name, payload, source,
            citizen_id, created_at, updated_at, decided_at, decided_by
        )
        VALUES (
            $1, 'citizenship', $2, $3, $4, $5, 'portal',
            $6, NOW(), NOW(), $7, $8
        )
        ON CONFLICT (id) DO UPDATE SET
            status = EXCLUDED.status,
            email = EXCLUDED.email,
            display_name = EXCLUDED.display_name,
            payload = candidates.payload || EXCLUDED.payload,
            citizen_id = COALESCE(EXCLUDED.citizen_id, candidates.citizen_id),
            updated_at = NOW(),
            decided_at = COALESCE(EXCLUDED.decided_at, candidates.decided_at),
            decided_by = COALESCE(EXCLUDED.decided_by, candidates.decided_by)
        "#,
    )
    .bind(id)
    .bind(status)
    .bind(email)
    .bind(display_name)
    .bind(payload)
    .bind(citizen_id)
    .bind(decided_at)
    .bind(decided_by)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_citizenship_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    email: &str,
    display_name: &str,
    status: &str,
    citizen_id: Option<&str>,
    payload: Value,
    decided_at: Option<DateTime<Utc>>,
    decided_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO candidates (
            id, kind, status, email, display_name, payload, source,
            citizen_id, created_at, updated_at, decided_at, decided_by
        )
        VALUES (
            $1, 'citizenship', $2, $3, $4, $5, 'portal',
            $6, NOW(), NOW(), $7, $8
        )
        ON CONFLICT (id) DO UPDATE SET
            status = EXCLUDED.status,
            email = EXCLUDED.email,
            display_name = EXCLUDED.display_name,
            payload = candidates.payload || EXCLUDED.payload,
            citizen_id = COALESCE(EXCLUDED.citizen_id, candidates.citizen_id),
            updated_at = NOW(),
            decided_at = COALESCE(EXCLUDED.decided_at, candidates.decided_at),
            decided_by = COALESCE(EXCLUDED.decided_by, candidates.decided_by)
        "#,
    )
    .bind(id)
    .bind(status)
    .bind(email)
    .bind(display_name)
    .bind(payload)
    .bind(citizen_id)
    .bind(decided_at)
    .bind(decided_by)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Upsert a mayor candidate (`kind=mayor`; id is the application UUID).
pub async fn upsert_mayor(
    pool: &PgPool,
    id: Uuid,
    email: Option<&str>,
    display_name: Option<&str>,
    status: &str,
    citizen_id: Option<&str>,
    source: &str,
    payload: Value,
    decided_at: Option<DateTime<Utc>>,
    decided_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO candidates (
            id, kind, status, email, display_name, payload, source,
            citizen_id, created_at, updated_at, decided_at, decided_by
        )
        VALUES (
            $1, 'mayor', $2, $3, $4, $5, $6,
            $7, NOW(), NOW(), $8, $9
        )
        ON CONFLICT (id) DO UPDATE SET
            status = EXCLUDED.status,
            email = COALESCE(EXCLUDED.email, candidates.email),
            display_name = COALESCE(EXCLUDED.display_name, candidates.display_name),
            payload = candidates.payload || EXCLUDED.payload,
            source = EXCLUDED.source,
            citizen_id = COALESCE(EXCLUDED.citizen_id, candidates.citizen_id),
            updated_at = NOW(),
            decided_at = COALESCE(EXCLUDED.decided_at, candidates.decided_at),
            decided_by = COALESCE(EXCLUDED.decided_by, candidates.decided_by)
        "#,
    )
    .bind(id)
    .bind(status)
    .bind(email)
    .bind(display_name)
    .bind(payload)
    .bind(source)
    .bind(citizen_id)
    .bind(decided_at)
    .bind(decided_by)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_mayor_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    email: Option<&str>,
    display_name: Option<&str>,
    status: &str,
    citizen_id: Option<&str>,
    source: &str,
    payload: Value,
    decided_at: Option<DateTime<Utc>>,
    decided_by: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO candidates (
            id, kind, status, email, display_name, payload, source,
            citizen_id, created_at, updated_at, decided_at, decided_by
        )
        VALUES (
            $1, 'mayor', $2, $3, $4, $5, $6,
            $7, NOW(), NOW(), $8, $9
        )
        ON CONFLICT (id) DO UPDATE SET
            status = EXCLUDED.status,
            email = COALESCE(EXCLUDED.email, candidates.email),
            display_name = COALESCE(EXCLUDED.display_name, candidates.display_name),
            payload = candidates.payload || EXCLUDED.payload,
            source = EXCLUDED.source,
            citizen_id = COALESCE(EXCLUDED.citizen_id, candidates.citizen_id),
            updated_at = NOW(),
            decided_at = COALESCE(EXCLUDED.decided_at, candidates.decided_at),
            decided_by = COALESCE(EXCLUDED.decided_by, candidates.decided_by)
        "#,
    )
    .bind(id)
    .bind(status)
    .bind(email)
    .bind(display_name)
    .bind(payload)
    .bind(source)
    .bind(citizen_id)
    .bind(decided_at)
    .bind(decided_by)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Upsert a role candidacy row into candidates (id matches candidacies.id UUID text).
pub async fn upsert_role(
    pool: &PgPool,
    id: &str,
    email: Option<&str>,
    display_name: &str,
    status: &str,
    citizen_id: &str,
    payload: Value,
    decided_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    let Ok(uuid) = Uuid::parse_str(id) else {
        tracing::warn!(id = %id, "skip candidates upsert: non-uuid candidacy id");
        return Ok(());
    };
    sqlx::query(
        r#"
        INSERT INTO candidates (
            id, kind, status, email, display_name, payload, source,
            citizen_id, created_at, updated_at, decided_at, decided_by
        )
        VALUES (
            $1, 'role', $2, $3, $4, $5, 'candidacy',
            $6, NOW(), NOW(), $7, NULL
        )
        ON CONFLICT (id) DO UPDATE SET
            status = EXCLUDED.status,
            email = COALESCE(EXCLUDED.email, candidates.email),
            display_name = EXCLUDED.display_name,
            payload = candidates.payload || EXCLUDED.payload,
            citizen_id = EXCLUDED.citizen_id,
            updated_at = NOW(),
            decided_at = COALESCE(EXCLUDED.decided_at, candidates.decided_at)
        "#,
    )
    .bind(uuid)
    .bind(status)
    .bind(email)
    .bind(display_name)
    .bind(payload)
    .bind(citizen_id)
    .bind(decided_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_status(
    pool: &PgPool,
    id: Uuid,
    status: &str,
    decided_by: Option<&str>,
    extra_payload: Option<Value>,
    citizen_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let decided = matches!(
        status,
        "approved" | "rejected" | "Approved" | "Appointed" | "Rejected" | "timed_out"
    );
    sqlx::query(
        r#"
        UPDATE candidates
        SET status = $2,
            payload = CASE
                WHEN $5::jsonb IS NULL THEN payload
                ELSE payload || $5::jsonb
            END,
            citizen_id = COALESCE($6, citizen_id),
            updated_at = $3,
            decided_at = CASE WHEN $4 THEN $3 ELSE decided_at END,
            decided_by = CASE WHEN $4 THEN COALESCE($7, decided_by) ELSE decided_by END
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(status)
    .bind(now)
    .bind(decided)
    .bind(extra_payload)
    .bind(citizen_id)
    .bind(decided_by)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_status_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    status: &str,
    decided_by: Option<&str>,
    extra_payload: Option<Value>,
    citizen_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let decided = matches!(
        status,
        "approved" | "rejected" | "Approved" | "Appointed" | "Rejected" | "timed_out"
    );
    sqlx::query(
        r#"
        UPDATE candidates
        SET status = $2,
            payload = CASE
                WHEN $5::jsonb IS NULL THEN payload
                ELSE payload || $5::jsonb
            END,
            citizen_id = COALESCE($6, citizen_id),
            updated_at = $3,
            decided_at = CASE WHEN $4 THEN $3 ELSE decided_at END,
            decided_by = CASE WHEN $4 THEN COALESCE($7, decided_by) ELSE decided_by END
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(status)
    .bind(now)
    .bind(decided)
    .bind(extra_payload)
    .bind(citizen_id)
    .bind(decided_by)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// True if email has any candidate application (any kind).
pub async fn email_has_application(pool: &PgPool, email: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM candidates
            WHERE lower(email) = lower($1)
            LIMIT 1
        )
        "#,
    )
    .bind(email)
    .fetch_one(pool)
    .await
}

/// Latest candidate rows for my-applications (citizenship + mayor).
pub async fn latest_by_email_kind(
    pool: &PgPool,
    email: &str,
    kind: &str,
) -> Result<Option<(String, String, Option<String>, DateTime<Utc>)>, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT
            id::text,
            status,
            COALESCE(payload->>'login', display_name) AS login,
            created_at
        FROM candidates
        WHERE kind = $2 AND lower(email) = lower($1)
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(email)
    .bind(kind)
    .fetch_optional(pool)
    .await
}

pub async fn find_latest_login_by_email(
    pool: &PgPool,
    email: &str,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT id::text, COALESCE(payload->>'login', display_name)
        FROM candidates
        WHERE lower(email) = lower($1)
          AND kind IN ('citizenship', 'mayor')
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(email)
    .fetch_optional(pool)
    .await
}

// --- HTTP handlers ---

/// GET /api/candidates — unified list (Founder/Aiya).
pub async fn list_candidates_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListCandidatesQuery>,
) -> impl IntoResponse {
    if auth.is_node {
        return response::forbidden("Node credentials cannot list candidates");
    }
    if !auth.can_change_citizen_role() {
        return response::forbidden("Только Aiya или Founder могут просматривать кандидатов");
    }

    if let Some(kind) = query.kind.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if !matches!(kind, KIND_CITIZENSHIP | KIND_MAYOR | KIND_ROLE) {
            return response::bad_request("kind must be citizenship, mayor, or role");
        }
    }

    match list_candidates(&state.db, &query).await {
        Ok(list) => Json(ApiResponse::success(json!({
            "candidates": list,
            "total": list.len(),
        })))
        .into_response(),
        Err(e) => response::internal_error(format!("Failed to list candidates: {e}")),
    }
}

/// GET /api/candidates/:id
pub async fn get_candidate_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if auth.is_node {
        return response::forbidden("Node credentials cannot view candidates");
    }
    if !auth.can_change_citizen_role() {
        return response::forbidden("Только Aiya или Founder могут просматривать кандидатов");
    }

    let id = match Uuid::parse_str(&id) {
        Ok(v) => v,
        Err(_) => return response::bad_request("Некорректный id кандидата"),
    };

    match get_candidate(&state.db, id).await {
        Ok(Some(row)) => Json(ApiResponse::success(row)).into_response(),
        Ok(None) => response::not_found("Кандидат не найден"),
        Err(e) => response::internal_error(format!("Failed to load candidate: {e}")),
    }
}

/// POST /api/candidates/:id/approve — dispatch by kind to existing flows.
pub async fn approve_candidate_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ReviewCandidateRequest>,
) -> impl IntoResponse {
    if auth.is_node || !auth.can_change_citizen_role() {
        return response::forbidden("Только Aiya или Founder могут одобрять кандидатов");
    }

    let uuid = match Uuid::parse_str(&id) {
        Ok(v) => v,
        Err(_) => return response::bad_request("Некорректный id кандидата"),
    };

    let row = match get_candidate(&state.db, uuid).await {
        Ok(Some(r)) => r,
        Ok(None) => return response::not_found("Кандидат не найден"),
        Err(e) => return response::internal_error(format!("Failed to load candidate: {e}")),
    };

    match row.kind.as_str() {
        KIND_CITIZENSHIP => {
            crate::citizenship_portal::approve_application(
                State(state),
                Extension(auth),
                Path(id),
                Json(crate::citizenship_portal::ReviewApplicationRequest { note: req.note }),
            )
            .await
            .into_response()
        }
        KIND_MAYOR => {
            let street = row
                .payload
                .get("street")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !street && row.citizen_id.is_some() {
                return response::bad_request(
                    "Заявка гражданина с узлом одобряется через поток пакета узла, не через /api/candidates",
                );
            }
            match crate::mayor::approve_street_mayor_application(
                state,
                &auth,
                uuid,
                crate::mayor::ReviewMayorApplicationRequest {
                    note: req.note,
                    ip: req.ip,
                },
            )
            .await
            {
                Ok(v) => Json(ApiResponse::success(v)).into_response(),
                Err(e) => e.to_response().into_response(),
            }
        }
        KIND_ROLE => {
            // Admin shortcut: same as threshold auto-approve (marks Approved).
            match crate::candidacy::approve_candidate(
                state.clone(),
                &auth.citizen_name,
                &id,
            )
            .await
            {
                Ok(row) => {
                    let _ = mark_status(
                        &state.db,
                        uuid,
                        &row.status,
                        Some(&auth.citizen_name),
                        Some(json!({
                            "votes_for": row.votes_for,
                            "votes_against": row.votes_against,
                            "approved_at": row.approved_at,
                        })),
                        Some(&row.citizen_id),
                    )
                    .await;
                    Json(ApiResponse::success(row)).into_response()
                }
                Err(e) => e.to_response().into_response(),
            }
        }
        other => response::bad_request(format!("Unknown candidate kind: {other}")),
    }
}

/// POST /api/candidates/:id/reject
pub async fn reject_candidate_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ReviewCandidateRequest>,
) -> impl IntoResponse {
    if auth.is_node || !auth.can_change_citizen_role() {
        return response::forbidden("Только Aiya или Founder могут отклонять кандидатов");
    }

    let uuid = match Uuid::parse_str(&id) {
        Ok(v) => v,
        Err(_) => return response::bad_request("Некорректный id кандидата"),
    };

    let row = match get_candidate(&state.db, uuid).await {
        Ok(Some(r)) => r,
        Ok(None) => return response::not_found("Кандидат не найден"),
        Err(e) => return response::internal_error(format!("Failed to load candidate: {e}")),
    };

    match row.kind.as_str() {
        KIND_CITIZENSHIP => {
            crate::citizenship_portal::reject_application(
                State(state),
                Extension(auth),
                Path(id),
                Json(crate::citizenship_portal::ReviewApplicationRequest { note: req.note }),
            )
            .await
            .into_response()
        }
        KIND_MAYOR => {
            match crate::mayor::reject_street_mayor_application(
                state,
                &auth,
                uuid,
                crate::mayor::ReviewMayorApplicationRequest {
                    note: req.note,
                    ip: req.ip,
                },
            )
            .await
            {
                Ok(v) => Json(ApiResponse::success(v)).into_response(),
                Err(e) => e.to_response().into_response(),
            }
        }
        KIND_ROLE => {
            // Soft-reject: mark candidates + candidacies Rejected without appoint.
            let note = req
                .note
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            if note.is_none() {
                return response::bad_request("Для отклонения укажите причину");
            }
            let updated = sqlx::query(
                r#"
                UPDATE candidacies
                SET status = 'Rejected'
                WHERE id = $1 AND status IN ('Active', 'Approved')
                "#,
            )
            .bind(&id)
            .execute(&state.db)
            .await;
            match updated {
                Ok(r) if r.rows_affected() == 0 => {
                    response::bad_request("Кандидатуру нельзя отклонить в текущем статусе")
                }
                Ok(_) => {
                    let _ = mark_status(
                        &state.db,
                        uuid,
                        "Rejected",
                        Some(&auth.citizen_name),
                        Some(json!({ "review_note": note })),
                        None,
                    )
                    .await;
                    Json(ApiResponse::success(json!({
                        "id": id,
                        "status": "Rejected",
                        "message": "Кандидатура отклонена",
                    })))
                    .into_response()
                }
                Err(e) => response::internal_error(format!("Failed to reject candidacy: {e}")),
            }
        }
        other => response::bad_request(format!("Unknown candidate kind: {other}")),
    }
}
