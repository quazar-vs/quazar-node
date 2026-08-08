use std::sync::Arc;

use axum::{
    extract::State,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::chain_events;
use crate::email::{notify_email, send_citizenship_application, SmtpConfig};
use crate::register_verify::normalize_email;
use crate::response::{self, ApiResponse};
use crate::validator::normalize_and_validate_citizen_name;
use crate::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitizenshipAppChainStatus {
    Pending,
    Approved,
    Rejected,
}

impl CitizenshipAppChainStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

/// Latest citizenship-application status on chain (blocks + pending), by email and/or name.
///
/// Fold order: CitizenRequested → pending, CitizenApplicationRejected → rejected,
/// CitizenAdded (correlated) → approved. Re-apply is allowed only when the latest
/// status is rejected (or there is no history).
pub async fn lookup_citizenship_app_status(
    db: &PgPool,
    email: Option<&str>,
    desired_name: Option<&str>,
) -> Result<Option<CitizenshipAppChainStatus>, String> {
    let email_norm = email
        .map(normalize_email)
        .filter(|s| !s.is_empty());
    let name_norm = desired_name
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());

    if email_norm.is_none() && name_norm.is_none() {
        return Ok(None);
    }

    let rows: Vec<(String, serde_json::Value, i64)> = sqlx::query_as(
        r#"
        SELECT event_type, data, ts FROM (
            SELECT
                event_type,
                data::jsonb AS data,
                timestamp AS ts
            FROM events
            WHERE event_type IN (
                'CitizenRequested',
                'CitizenApplicationRejected',
                'CitizenAdded'
            )
            UNION ALL
            SELECT
                event_data::jsonb->>'event_type' AS event_type,
                event_data::jsonb->'data' AS data,
                COALESCE((event_data::jsonb->>'timestamp')::bigint, 0) AS ts
            FROM pending_events
            WHERE event_data::jsonb->>'event_type' IN (
                'CitizenRequested',
                'CitizenApplicationRejected',
                'CitizenAdded'
            )
        ) e
        WHERE (
                $1::text IS NOT NULL
                AND lower(COALESCE(data->>'email', '')) = $1
            )
            OR (
                $2::text IS NOT NULL
                AND lower(COALESCE(
                    data->>'desired_name',
                    data->>'citizen_name',
                    ''
                )) = $2
            )
        ORDER BY ts ASC, event_type ASC
        "#,
    )
    .bind(email_norm.as_deref())
    .bind(name_norm.as_deref())
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;

    let mut status: Option<CitizenshipAppChainStatus> = None;
    for (event_type, data, _) in rows {
        match event_type.as_str() {
            "CitizenRequested" => status = Some(CitizenshipAppChainStatus::Pending),
            "CitizenApplicationRejected" => status = Some(CitizenshipAppChainStatus::Rejected),
            "CitizenAdded" => {
                // Correlate with application fields when present; otherwise name/email match
                // from the SQL filter is enough (legacy CitizenAdded without email).
                let _ = data;
                status = Some(CitizenshipAppChainStatus::Approved);
            }
            _ => {}
        }
    }
    Ok(status)
}

/// Block new applications when chain history shows pending or already approved.
pub fn reject_message_for_status(status: CitizenshipAppChainStatus) -> &'static str {
    match status {
        CitizenshipAppChainStatus::Pending => {
            "Заявка с этими данными уже подана и ожидает рассмотрения"
        }
        CitizenshipAppChainStatus::Approved => {
            "Гражданство по этим данным уже одобрено"
        }
        CitizenshipAppChainStatus::Rejected => {
            // Should not be used as a hard reject — re-apply is allowed.
            "Предыдущая заявка была отклонена"
        }
    }
}

pub async fn emit_citizen_requested(
    state: &Arc<AppState>,
    application_id: &str,
    email: &str,
    desired_name: &str,
    reason: &str,
    channel: &str,
) -> Result<(), String> {
    let email = normalize_email(email);
    let event_id = format!("citizen_req_{application_id}");
    chain_events::emit_signed_event(
        state,
        "CitizenRequested",
        desired_name,
        "Citizenship application submitted",
        &format!("application {application_id} via {channel}"),
        Some(&event_id),
        serde_json::json!({
            "application_id": application_id,
            "email": email,
            "desired_name": desired_name,
            "citizen_name": desired_name,
            "reason": reason,
            "channel": channel,
        }),
    )
    .await
}

pub async fn emit_citizen_application_rejected(
    state: &Arc<AppState>,
    application_id: &str,
    email: &str,
    desired_name: Option<&str>,
    reason: &str,
    rejected_by: &str,
) -> Result<(), String> {
    let email = normalize_email(email);
    let event_id = format!("citizen_rej_{application_id}");
    let mut data = serde_json::json!({
        "application_id": application_id,
        "email": email,
        "reason": reason,
        "rejected_by": rejected_by,
    });
    if let Some(name) = desired_name.filter(|s| !s.is_empty()) {
        data["desired_name"] = serde_json::json!(name);
        data["citizen_name"] = serde_json::json!(name);
    }
    chain_events::emit_signed_event(
        state,
        "CitizenApplicationRejected",
        rejected_by,
        "Citizenship application rejected",
        &format!("application {application_id} rejected"),
        Some(&event_id),
        data,
    )
    .await
}

#[derive(Deserialize)]
pub struct CitizenshipApplicationRequest {
    pub name: String,
    pub email: String,
    pub reason: String,
}

#[derive(Serialize)]
pub struct CitizenshipApplicationResponse {
    pub message: String,
}

fn is_valid_email(email: &str) -> bool {
    let email = email.trim();
    if email.len() < 5 || email.len() > 254 {
        return false;
    }
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !email.contains(char::is_whitespace)
}

pub async fn citizenship_application_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CitizenshipApplicationRequest>,
) -> impl IntoResponse {
    let name = match normalize_and_validate_citizen_name(&req.name) {
        Ok(n) => n,
        Err(msg) => return response::bad_request(msg),
    };
    let email = normalize_email(&req.email);
    let reason = req.reason.trim();

    if email.is_empty() {
        return response::bad_request("Укажите email");
    }
    if reason.is_empty() {
        return response::bad_request("Опишите, чем вы можете быть полезны");
    }
    if !is_valid_email(&email) {
        return response::bad_request("Введите корректный email-адрес");
    }

    match lookup_citizenship_app_status(&state.db, Some(&email), Some(&name)).await {
        Ok(Some(status @ (CitizenshipAppChainStatus::Pending | CitizenshipAppChainStatus::Approved))) => {
            return response::forbidden(reject_message_for_status(status));
        }
        Ok(Some(CitizenshipAppChainStatus::Rejected) | None) => {}
        Err(e) => {
            tracing::error!(error = %e, "failed to lookup citizenship chain status");
            return response::internal_error("Не удалось проверить статус заявки в цепочке");
        }
    }

    let Some(smtp) = SmtpConfig::from_env() else {
        tracing::error!("SMTP not configured (SMTP_* / GMAIL_*)");
        return response::internal_error(
            "Отправка email не настроена на сервере. Обратитесь к администратору.",
        );
    };

    let application_id = Uuid::new_v4().to_string();
    if let Err(e) = emit_citizen_requested(
        &state,
        &application_id,
        &email,
        &name,
        reason,
        "landing",
    )
    .await
    {
        tracing::error!(
            applicant = %email,
            error = %e,
            "failed to emit CitizenRequested"
        );
        return response::internal_error(
            "Не удалось зафиксировать заявку в цепочке. Попробуйте позже.",
        );
    }

    let notify_to = notify_email().unwrap_or_else(|| smtp.user.clone());
    if let Err(e) = send_citizenship_application(&smtp, &notify_to, &name, &email, reason).await {
        tracing::error!(
            applicant = %email,
            error = %e,
            "failed to send citizenship application email (chain event already recorded)"
        );
    }

    tracing::info!(
        applicant = %email,
        name = %name,
        application_id = %application_id,
        "citizenship application received"
    );

    Json(ApiResponse::success(CitizenshipApplicationResponse {
        message: "Заявка получена".to_string(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_emails() {
        assert!(is_valid_email("user@example.com"));
    }

    #[test]
    fn rejects_invalid_emails() {
        assert!(!is_valid_email(""));
        assert!(!is_valid_email("not-an-email"));
    }
}
