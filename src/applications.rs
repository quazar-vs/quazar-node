use std::sync::Arc;

use axum::{
    extract::{Extension, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::AuthContext;
use crate::response::{self, ApiResponse};
use crate::AppState;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ApplicationRow {
    pub id: String,
    pub login: String,
    pub full_name: String,
    pub email: String,
    pub reason: String,
    pub status: String,
    pub citizen_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub review_note: Option<String>,
    pub reviewed_by: Option<String>,
    pub reviewed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
}

pub(crate) fn recaptcha_configured() -> bool {
    std::env::var("RECAPTCHA_SECRET_KEY")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Captcha is required when configured, or always in production.
pub(crate) fn recaptcha_required() -> bool {
    crate::auth::is_production() || recaptcha_configured()
}

pub(crate) async fn verify_recaptcha(token: &str) -> Result<(), String> {
    crate::auth::ensure_recaptcha_secret()?;

    let Ok(secret) = std::env::var("RECAPTCHA_SECRET_KEY") else {
        tracing::warn!("RECAPTCHA_SECRET_KEY unset — captcha skipped (dev)");
        return Ok(());
    };
    if secret.trim().is_empty() {
        tracing::warn!("RECAPTCHA_SECRET_KEY empty — captcha skipped (dev)");
        return Ok(());
    }
    if token.trim().is_empty() {
        return Err("Пройдите проверку reCAPTCHA".to_string());
    }

    let client = reqwest::Client::new();
    let resp = client
        .post("https://www.google.com/recaptcha/api/siteverify")
        .form(&[("secret", secret.as_str()), ("response", token)])
        .send()
        .await
        .map_err(|e| format!("reCAPTCHA unreachable: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("reCAPTCHA parse error: {e}"))?;

    if body.get("success").and_then(|v| v.as_bool()) == Some(true) {
        Ok(())
    } else {
        Err("Проверка reCAPTCHA не пройдена".to_string())
    }
}

/// GET /api/applications — list applications (Bearer API key).
pub async fn list_applications(
    Extension(auth): Extension<AuthContext>,
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    if auth.is_node {
        return response::forbidden("Node credentials cannot list applications");
    }

    let status_filter = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let rows = if let Some(status) = status_filter {
        if !matches!(status, "pending" | "confirmed" | "approved" | "rejected") {
            return response::bad_request("status must be pending, confirmed, approved, or rejected");
        }
        sqlx::query_as::<_, ApplicationRow>(
            r#"
            SELECT
                id::text AS id,
                login,
                full_name,
                email,
                reason,
                status,
                citizen_id,
                created_at,
                updated_at,
                confirmed_at,
                review_note,
                reviewed_by,
                reviewed_at
            FROM applications
            WHERE status = $1
            ORDER BY created_at DESC
            LIMIT 200
            "#,
        )
        .bind(status)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query_as::<_, ApplicationRow>(
            r#"
            SELECT
                id::text AS id,
                login,
                full_name,
                email,
                reason,
                status,
                citizen_id,
                created_at,
                updated_at,
                confirmed_at,
                review_note,
                reviewed_by,
                reviewed_at
            FROM applications
            ORDER BY
              CASE status
                  WHEN 'pending' THEN 0
                  WHEN 'confirmed' THEN 1
                  WHEN 'approved' THEN 2
                  ELSE 3
              END,
              created_at DESC
            LIMIT 200
            "#,
        )
        .fetch_all(&state.db)
        .await
    };

    match rows {
        Ok(list) => Json(ApiResponse::success(serde_json::json!({
            "applications": list,
            "total": list.len(),
        })))
        .into_response(),
        Err(e) => response::internal_error(format!("Failed to list applications: {e}")),
    }
}
