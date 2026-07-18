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

/// Public site key for frontend widgets (v2 checkbox / v3). Never secret.
pub fn recaptcha_site_key() -> Option<String> {
    std::env::var("RECAPTCHA_SITE_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Both site key and secret must be present before captcha is treated as configured.
pub(crate) fn recaptcha_keys_ready() -> bool {
    recaptcha_configured() && recaptcha_site_key().is_some()
}

/// Captcha is required only when both RECAPTCHA_SECRET_KEY and RECAPTCHA_SITE_KEY are set.
/// (STRICT mode without keys fails at startup — see `assert_recaptcha_in_production`.)
pub(crate) fn recaptcha_required() -> bool {
    recaptcha_keys_ready()
}

fn recaptcha_min_score() -> f64 {
    std::env::var("RECAPTCHA_MIN_SCORE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.5)
}

pub(crate) async fn verify_recaptcha(token: &str) -> Result<(), String> {
    verify_recaptcha_with_ip(token, None).await
}

/// Verify Google reCAPTCHA v2/v3 token via siteverify.
///
/// For v3 responses that include `score`, require `score >= RECAPTCHA_MIN_SCORE` (default 0.5).
/// Optional `RECAPTCHA_EXPECTED_ACTION` must match the `action` field when set.
pub(crate) async fn verify_recaptcha_with_ip(
    token: &str,
    remote_ip: Option<&str>,
) -> Result<(), String> {
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

    let mut form = vec![
        ("secret", secret),
        ("response", token.trim().to_string()),
    ];
    if let Some(ip) = remote_ip.map(str::trim).filter(|s| !s.is_empty() && *s != "unknown") {
        form.push(("remoteip", ip.to_string()));
    }

    let client = reqwest::Client::new();
    let resp = client
        .post("https://www.google.com/recaptcha/api/siteverify")
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("reCAPTCHA unreachable: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("reCAPTCHA parse error: {e}"))?;

    if body.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return Err("Проверка reCAPTCHA не пройдена".to_string());
    }

    // v3: score present — enforce minimum.
    if let Some(score) = body.get("score").and_then(|v| v.as_f64()) {
        let min = recaptcha_min_score();
        if score < min {
            tracing::warn!(score, min, "reCAPTCHA v3 score too low");
            return Err("Проверка reCAPTCHA не пройдена".to_string());
        }
    }

    if let Ok(expected) = std::env::var("RECAPTCHA_EXPECTED_ACTION") {
        let expected = expected.trim();
        if !expected.is_empty() {
            let action = body
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if action != expected {
                tracing::warn!(action, expected, "reCAPTCHA action mismatch");
                return Err("Проверка reCAPTCHA не пройдена".to_string());
            }
        }
    }

    Ok(())
}

/// GET /api/public-config — non-secret frontend config (reCAPTCHA site key).
pub async fn public_config_handler() -> impl IntoResponse {
    Json(ApiResponse::success(serde_json::json!({
        "recaptcha_site_key": recaptcha_site_key(),
        "recaptcha_required": recaptcha_required(),
    })))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recaptcha_required_only_when_both_keys_set() {
        // Isolate from ambient process env.
        std::env::remove_var("RECAPTCHA_SECRET_KEY");
        std::env::remove_var("RECAPTCHA_SITE_KEY");
        std::env::remove_var("QUAZAR_STRICT_SECRETS");

        assert!(!recaptcha_required());
        assert!(recaptcha_site_key().is_none());

        std::env::set_var("RECAPTCHA_SECRET_KEY", "secret-only");
        assert!(!recaptcha_required());
        assert!(recaptcha_site_key().is_none());

        std::env::set_var("RECAPTCHA_SITE_KEY", "site-key");
        assert!(recaptcha_keys_ready());
        assert!(recaptcha_required());
        assert_eq!(recaptcha_site_key().as_deref(), Some("site-key"));

        // STRICT alone must not force required=true without a site key.
        std::env::remove_var("RECAPTCHA_SITE_KEY");
        std::env::set_var("QUAZAR_STRICT_SECRETS", "true");
        assert!(!recaptcha_required());

        std::env::remove_var("RECAPTCHA_SECRET_KEY");
        std::env::remove_var("QUAZAR_STRICT_SECRETS");
    }
}
