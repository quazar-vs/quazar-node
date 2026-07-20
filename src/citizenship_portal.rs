use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    extract::{ConnectInfo, Extension, Path, State},
    http::StatusCode,
    http::HeaderMap,
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use sha2::{Digest, Sha256};

use crate::applications::{recaptcha_required, verify_recaptcha_with_ip};
use crate::auth::{AuthContext, Role};
use crate::block_producer;
use crate::blockchain::{
    build_signed_citizen_added_event, build_signed_citizen_role_event, build_signed_passport_issued_event,
};
use crate::citizen::hash_key;
use crate::client_ip;
use crate::email::{
    send_citizenship_application, send_citizenship_approval, send_citizenship_confirmation,
    send_citizenship_rejection, SmtpConfig,
};
use crate::gossip;
use crate::keys::push_key_to_peers;
use crate::models::Event;
use crate::pending;
use crate::register_verify::{consume_verification_code, issue_verification_code, is_valid_email, normalize_email};
use crate::response::{self, ApiResponse};
use crate::validator::{normalize_and_validate_citizen_name, EventValidator};
use crate::AppState;

static SUBMIT_RATE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn allow_submit(ip: &str) -> bool {
    let mutex = SUBMIT_RATE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = mutex.lock().expect("submit rate mutex");

    let now = Instant::now();
    let max_age = Duration::from_secs(120);
    map.retain(|_, t| now.duration_since(*t) < max_age);

    let min_interval = Duration::from_secs(60);
    if let Some(last) = map.get(ip) {
        if now.duration_since(*last) < min_interval {
            return false;
        }
    }

    map.insert(ip.to_string(), now);
    true
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn generate_temp_public_key_hex() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn generate_citizen_api_key() -> String {
    format!("qz_citizen_{}", Uuid::new_v4().simple())
}

async fn submit_pending_event(
    state: Arc<AppState>,
    event: Event,
) -> Result<(), (StatusCode, Json<ApiResponse>)> {
    if let Err(e) = EventValidator::validate_event(&event, &state.db).await {
        return Err((StatusCode::BAD_REQUEST, Json(ApiResponse::error(e.message()))));
    }

    match pending::insert(&state.db, &event).await {
        Ok(pending::PendingInsertResult::Inserted) => {}
        Ok(pending::PendingInsertResult::AlreadyExists) => return Ok(()),
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::error(format!("Не удалось добавить событие: {e}"))),
            ));
        }
    }

    let state_gossip = state.clone();
    let event_gossip = event.clone();
    tokio::spawn(async move {
        gossip::push_pending_to_peers(&state_gossip, &event_gossip).await;
    });

    let state_clone = state.clone();
    tokio::spawn(async move {
        block_producer::try_create_block(state_clone).await;
    });

    Ok(())
}

fn validate_login(login: &str) -> Result<String, &'static str> {
    normalize_and_validate_citizen_name(login)
}

fn validate_non_empty_len(value: &str, field: &'static str, max: usize) -> Result<String, String> {
    let s = value.trim().to_string();
    if s.is_empty() {
        return Err(format!("{field} обязателен"));
    }
    if s.len() > max {
        return Err(format!("{field} слишком длинное (макс. {max} символов)"));
    }
    Ok(s)
}

#[derive(Debug, Deserialize)]
pub struct SubmitApplicationRequest {
    pub login: String,
    pub full_name: String,
    pub passport_data: String,
    pub email: String,
    pub reason: String,
    /// Google reCAPTCHA v2/v3 response token.
    #[serde(default)]
    pub captcha_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SubmitApplicationResponse {
    pub id: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct VerifyApplicationRequest {
    pub id: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct VerifyApplicationResponse {
    pub id: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, sqlx::FromRow)]
struct ApplicationStatusRow {
    pub id: Uuid,
    pub login: String,
    pub full_name: String,
    pub status: String,
    pub citizen_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub review_note: Option<String>,
    pub reviewed_by: Option<String>,
    pub reviewed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ApplicationStatusResponse {
    pub id: String,
    pub login: String,
    pub full_name: String,
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
pub struct ReviewApplicationRequest {
    pub note: Option<String>,
}

/// POST /api/application — submit citizenship application.
///
/// Public endpoint (no Authorization header).
pub async fn submit_application(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    connect: ConnectInfo<SocketAddr>,
    Json(req): Json<SubmitApplicationRequest>,
) -> impl IntoResponse {
    // Client-side attackers can still hammer the endpoint; keep a cheap per-IP throttle.
    let trusted = state.trusted_proxies.snapshot().await;
    let ip = client_ip::resolve_from_connect(&headers, Some(&connect), &trusted);
    if !allow_submit(&ip) {
        return response::too_many_requests(
            "Слишком часто. Подождите минуту и попробуйте снова.",
        );
    }

    if let Err(msg) = crate::auth::ensure_recaptcha_secret() {
        return response::service_unavailable(msg);
    }
    if recaptcha_required() {
        let token = req.captcha_token.as_deref().unwrap_or("");
        if let Err(msg) = verify_recaptcha_with_ip(token, Some(&ip)).await {
            return response::bad_request(msg);
        }
    }

    let login = match validate_login(&req.login) {
        Ok(v) => v,
        Err(msg) => return response::bad_request(msg),
    };

    let full_name = match validate_non_empty_len(&req.full_name, "ФИО", 200) {
        Ok(v) => v,
        Err(msg) => return response::bad_request(msg),
    };
    let passport_data = match validate_non_empty_len(&req.passport_data, "Данные паспорта", 2000) {
        Ok(v) => v,
        Err(msg) => return response::bad_request(msg),
    };
    let reason = match validate_non_empty_len(&req.reason, "Причина", 5000) {
        Ok(v) => v,
        Err(msg) => return response::bad_request(msg),
    };

    let email = normalize_email(&req.email);
    if !is_valid_email(&email) {
        return response::bad_request("Введите корректный email-адрес");
    }

    // 1) Check email: no more than one application per calendar month.
    let email_month_exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM applications
            WHERE lower(email) = lower($1)
              AND created_at >= date_trunc('month', now())
              AND created_at <  (date_trunc('month', now()) + interval '1 month')
        )
        "#,
    )
    .bind(&email)
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);

    if email_month_exists {
        return response::forbidden("На этот email уже есть заявка в текущем месяце");
    }

    // 2) Check login: free and citizen not in DB.
    let citizen_exists_by_login: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM citizens WHERE LOWER(name) = LOWER($1) LIMIT 1)",
    )
    .bind(&login)
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);
    if citizen_exists_by_login {
        return response::bad_request("Гражданин с таким логином уже существует в системе");
    }

    let citizen_exists_by_email: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM citizens WHERE lower(email) = lower($1) LIMIT 1)",
    )
    .bind(&email)
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);
    if citizen_exists_by_email {
        return response::bad_request("Гражданин с таким email уже существует в системе");
    }

    let app_login_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM applications WHERE LOWER(login) = LOWER($1) LIMIT 1)",
    )
    .bind(&login)
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);
    if app_login_exists {
        return response::bad_request("Логин уже используется в заявках");
    }

    // Generate + send the verification code to applicant email (1st email).
    let code = match issue_verification_code(&email, &ip).await {
        Ok(c) => c,
        Err(msg) => return response::bad_request(msg),
    };
    let code_hash = sha256_hex(&code);

    let id = Uuid::new_v4();
    let now = Utc::now();

    // Best-effort: create internal mailbox and email credentials to external address.
    let (internal_email, mail_password, mail_created) =
        crate::mail::provision_mailbox_for_application(&login, &email).await;

    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO applications
            (id, login, full_name, passport_data, email, reason, status, verification_code,
             created_at, updated_at, confirmed_at,
             external_email, internal_email, mail_password, mail_created)
        VALUES
            ($1, $2, $3, $4, $5, $6, 'pending', $7, $8, $8, NULL,
             $5, $9, $10, $11)
        "#,
    )
    .bind(id)
    .bind(&login)
    .bind(&full_name)
    .bind(&passport_data)
    .bind(&email)
    .bind(&reason)
    .bind(&code_hash)
    .bind(now)
    .bind(&internal_email)
    .bind(&mail_password)
    .bind(mail_created)
    .execute(&state.db)
    .await
    {
        return response::internal_error(format!("Failed to save application: {e}"));
    }

    // 2nd email: notify Quazar team (best-effort).
    if let Some(smtp) = SmtpConfig::from_env() {
        if let Err(e) = send_citizenship_application(&smtp, &smtp.user, &full_name, &email, &reason)
            .await
        {
            tracing::warn!(to = %smtp.user, error = %e, "failed to send citizenship application notification");
        }
    } else {
        tracing::warn!("GMAIL_USER / GMAIL_APP_PASSWORD not configured — skipping notification email");
    }

    Json(ApiResponse::success(SubmitApplicationResponse {
        id: id.to_string(),
        status: "pending".to_string(),
        message: "Заявка принята. Проверьте email: пришёл код подтверждения.".to_string(),
    }))
    .into_response()
}

/// POST /api/application/verify — confirm email verification code.
pub async fn verify_application_code(
    State(state): State<Arc<AppState>>,
    Json(req): Json<VerifyApplicationRequest>,
) -> impl IntoResponse {
    let code = req.code.trim().to_string();
    if !code.chars().all(|c| c.is_ascii_digit()) || code.len() != 6 {
        return response::bad_request("Код должен состоять из 6 цифр");
    }

    let id = match Uuid::parse_str(&req.id) {
        Ok(v) => v,
        Err(_) => return response::bad_request("Некорректный id заявки"),
    };

    let row: Option<(String, String, String)> = sqlx::query_as(
        r#"
        SELECT email, verification_code, status
        FROM applications
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let Some((email, stored_hash, status)) = row else {
        return response::not_found("Заявка не найдена");
    };

    if status != "pending" {
        return response::bad_request("Код больше не требуется для этой заявки");
    }

    let provided_hash = sha256_hex(&code);
    if provided_hash != stored_hash {
        return response::bad_request("Неверный код подтверждения");
    }

    // Validate TTL/rate-limit via in-memory code store (consume removes the code).
    if let Err(msg) = consume_verification_code(&email, &code) {
        return response::bad_request(msg);
    }

    let updated = sqlx::query(
        r#"
        UPDATE applications
        SET status = 'confirmed',
            confirmed_at = now(),
            updated_at = now()
        WHERE id = $1
          AND status = 'pending'
        "#,
    )
    .bind(id)
    .execute(&state.db)
    .await
    .map(|r| r.rows_affected() as i64)
    .unwrap_or(0);

    if updated == 0 {
        return response::bad_request("Заявка уже подтверждена или находится в другом статусе");
    }

    if let Some(smtp) = SmtpConfig::from_env() {
        if let Err(e) = send_citizenship_confirmation(&smtp, &email, &id.to_string()).await {
            tracing::warn!(
                email = %email,
                application_id = %id,
                error = %e,
                "failed to send citizenship confirmation email"
            );
        }
    } else {
        tracing::warn!("SMTP not configured — skipping confirmation email");
    }

    Json(ApiResponse::success(VerifyApplicationResponse {
        id: id.to_string(),
        status: "confirmed".to_string(),
        message: "Email подтверждён. Статус заявки обновлён.".to_string(),
    }))
    .into_response()
}

/// GET /api/application/:id — show current status.
pub async fn get_application_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let id = match Uuid::parse_str(&id) {
        Ok(v) => v,
        Err(_) => return response::bad_request("Некорректный id заявки"),
    };

    match sqlx::query_as::<_, ApplicationStatusRow>(
        r#"
        SELECT id, login, full_name, status, citizen_id, created_at, updated_at, confirmed_at,
               review_note, reviewed_by, reviewed_at
        FROM applications
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(row)) => {
            let out = ApplicationStatusResponse {
                id: row.id.to_string(),
                login: row.login,
                full_name: row.full_name,
                status: row.status,
                citizen_id: row.citizen_id,
                created_at: row.created_at,
                updated_at: row.updated_at,
                confirmed_at: row.confirmed_at,
                review_note: row.review_note,
                reviewed_by: row.reviewed_by,
                reviewed_at: row.reviewed_at,
            };
            Json(ApiResponse::success(out)).into_response()
        }
        Ok(None) => response::not_found("Application not found"),
        Err(e) => response::internal_error(format!("Failed to load application: {e}")),
    }
}

async fn update_application_review_status(
    state: Arc<AppState>,
    auth: AuthContext,
    id: String,
    next_status: &str,
    note: Option<String>,
) -> axum::response::Response {
    if auth.is_node || !auth.can_change_citizen_role() {
        return response::forbidden("Только Aiya или Founder могут менять статус заявки");
    }

    let id = match Uuid::parse_str(&id) {
        Ok(v) => v,
        Err(_) => return response::bad_request("Некорректный id заявки"),
    };

    let current_status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM applications WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let Some(current_status) = current_status else {
        return response::not_found("Заявка не найдена");
    };

    match next_status {
        "approved" if current_status != "confirmed" => {
            return response::bad_request("Одобрить можно только подтверждённую заявку");
        }
        "rejected" if !matches!(current_status.as_str(), "pending" | "confirmed") => {
            return response::bad_request("Отклонить можно только pending или confirmed заявку");
        }
        _ => {}
    }

    let note = note.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if next_status == "rejected" && note.is_none() {
        return response::bad_request("Для отклонения укажите причину");
    }
    let note_for_email = note.clone();

    if next_status == "approved" {
        let app_row: Option<(String, String, String, String, String, Option<String>)> = sqlx::query_as(
            r#"
            SELECT login, full_name, passport_data, email, status, internal_email
            FROM applications
            WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        let Some((login, _full_name, _passport_data, email, status, internal_email)) = app_row else {
            return response::not_found("Заявка не найдена");
        };

        if status != "confirmed" {
            return response::bad_request("Одобрить можно только подтверждённую заявку");
        }

        let citizen_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM citizens WHERE LOWER(name) = LOWER($1) LIMIT 1)",
        )
        .bind(&login)
        .fetch_one(&state.db)
        .await
        .unwrap_or(false);
        if citizen_exists {
            return response::bad_request("Гражданин с таким логином уже существует");
        }

        let citizen_id = Uuid::new_v4().to_string();
        let passport_id = Uuid::new_v4().to_string();
        let api_key = generate_citizen_api_key();
        let api_key_hash = hash_key(&api_key);
        let api_key_id = Uuid::new_v4().to_string();
        let public_key = generate_temp_public_key_hex();
        let now = Utc::now();
        let now_ts = now.timestamp();
        let expires_at = now_ts + 365 * 24 * 60 * 60;

        let mut tx: Transaction<'_, Postgres> = match state.db.begin().await {
            Ok(tx) => tx,
            Err(e) => return response::internal_error(format!("Failed to begin transaction: {e}")),
        };

        let tx_result = async {
            sqlx::query(
                r#"
                INSERT INTO citizens
                    (id, name, public_key, status, role, created_at, passport_issued, passport_expires, is_digital, email, email_verified)
                VALUES
                    ($1, $2, $3, $4, $5, $6, TRUE, $7, FALSE, $8, TRUE)
                "#,
            )
            .bind(&citizen_id)
            .bind(&login)
            .bind(&public_key)
            .bind("active")
            .bind(Role::Citizen.as_str())
            .bind(now_ts)
            .bind(expires_at)
            .bind(&email)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                r#"
                INSERT INTO passports (id, citizen_id, issued_at, expires_at, is_valid)
                VALUES ($1, $2, $3, $4, TRUE)
                "#,
            )
            .bind(&passport_id)
            .bind(&citizen_id)
            .bind(now_ts)
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                r#"
                INSERT INTO api_keys (id, citizen_id, key_hash, role, created_at, expires_at, is_active, last_used)
                VALUES ($1, $2, $3, $4, $5, NULL, TRUE, NULL)
                "#,
            )
            .bind(&api_key_id)
            .bind(&citizen_id)
            .bind(&api_key_hash)
            .bind(Role::Citizen.as_str())
            .bind(now)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                r#"
                UPDATE applications
                SET status = 'approved',
                    citizen_id = $2,
                    review_note = $3,
                    reviewed_by = $4,
                    reviewed_at = $5,
                    updated_at = $5
                WHERE id = $1
                "#,
            )
            .bind(id)
            .bind(&citizen_id)
            .bind(note.clone())
            .bind(&auth.citizen_name)
            .bind(now)
            .execute(&mut *tx)
            .await?;

            Ok::<(), sqlx::Error>(())
        }
        .await;

        if let Err(e) = tx_result {
            let _ = tx.rollback().await;
            return response::internal_error(format!("Failed to approve application: {e}"));
        }

        if let Err(e) = tx.commit().await {
            return response::internal_error(format!("Failed to commit application approval: {e}"));
        }

        let citizen_added_event = build_signed_citizen_added_event(
            &format!("citizen_add_{}", citizen_id),
            &citizen_id,
            &login,
            &public_key,
            "Quazar Portal",
            Role::Citizen.as_str(),
            &auth.citizen_name,
            now_ts,
        );
        let _ = submit_pending_event(state.clone(), citizen_added_event).await;

        let passport_event = build_signed_passport_issued_event(
            &format!("passport_issue_{}", passport_id),
            &passport_id,
            &citizen_id,
            &login,
            now_ts,
            expires_at,
            &auth.citizen_name,
            now_ts,
        );
        let _ = submit_pending_event(state.clone(), passport_event).await;

        let role_event = build_signed_citizen_role_event(
            &format!("citizen_role_{}_{}", citizen_id, now_ts),
            &citizen_id,
            &login,
            Role::Citizen.as_str(),
            &auth.citizen_name,
            now_ts,
        );
        let _ = submit_pending_event(state.clone(), role_event).await;

        push_key_to_peers(&state, &api_key, Role::Citizen, &login).await;

        // Credentials go to internal mailbox when provisioned; fallback to external email.
        crate::mail::send_credentials_to_internal_mailbox(
            internal_email.as_deref(),
            &citizen_id,
            &login,
            &api_key,
            &public_key,
        )
        .await;

        if internal_email.as_deref().unwrap_or("").is_empty() {
            if let Some(smtp) = SmtpConfig::from_env() {
                if let Err(e) = send_citizenship_approval(&smtp, &email, &login, &api_key).await {
                    tracing::warn!(email = %email, error = %e, "failed to send citizenship approval email");
                }
            } else {
                tracing::warn!("SMTP not configured — skipping approval email");
            }
        }

        return Json(ApiResponse::success(serde_json::json!({
            "id": id.to_string(),
            "status": "approved",
            "citizen_id": citizen_id,
            "passport_id": passport_id,
            "api_key": api_key,
            "message": "Заявка одобрена. Гражданин, паспорт и API-ключ созданы."
        })))
        .into_response();
    }

    let target_email: Option<String> = if next_status == "rejected" {
        sqlx::query_scalar("SELECT email FROM applications WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    match sqlx::query(
        r#"
        UPDATE applications
        SET status = $2,
            review_note = $3,
            reviewed_by = $4,
            reviewed_at = now(),
            updated_at = now()
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(next_status)
    .bind(&note)
    .bind(&auth.citizen_name)
    .execute(&state.db)
    .await
    {
        Ok(_) => {
            if next_status == "rejected" {
                if let (Some(email), Some(reason), Some(smtp)) =
                    (target_email, note_for_email, SmtpConfig::from_env())
                {
                    if let Err(e) = send_citizenship_rejection(&smtp, &email, &reason).await {
                        tracing::warn!(email = %email, error = %e, "failed to send citizenship rejection email");
                    }
                }
            }

            Json(ApiResponse::success(serde_json::json!({
                "id": id.to_string(),
                "status": next_status,
                "message": match next_status {
                    "approved" => "Заявка одобрена",
                    "rejected" => "Заявка отклонена",
                    _ => "Статус обновлён",
                }
            })))
            .into_response()
        }
        Err(e) => response::internal_error(format!("Failed to update application: {e}")),
    }
}

pub async fn approve_application(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(req): Json<ReviewApplicationRequest>,
) -> impl IntoResponse {
    update_application_review_status(state, auth, id, "approved", req.note).await
}

pub async fn reject_application(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(req): Json<ReviewApplicationRequest>,
) -> impl IntoResponse {
    update_application_review_status(state, auth, id, "rejected", req.note).await
}

