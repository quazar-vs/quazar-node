use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use sqlx::PgPool;

use crate::email::{send_verification_code, SmtpConfig};
use crate::response::{self, ApiResponse};
use crate::AppState;

/// UI + API message when a registered citizen tries to use the website login/session.
pub const CITIZEN_USE_APP_MESSAGE: &str = "Вход для граждан через приложение";
/// Machine-readable code paired with [`CITIZEN_USE_APP_MESSAGE`].
pub const CITIZEN_USE_APP_CODE: &str = "citizen_use_app";

/// TTL for the 6-digit verification code and for the post-verify session mark.
/// Mark lifetime must match the code so the verified-email session expires with it.
const CODE_TTL: Duration = Duration::from_secs(10 * 60);
const RESEND_COOLDOWN: Duration = Duration::from_secs(60);
const MAX_SENDS_PER_IP_PER_HOUR: u32 = 10;

#[derive(Clone)]
struct PendingCode {
    code: String,
    expires_at: Instant,
    last_sent_at: Instant,
}

static CODES: LazyLock<Mutex<HashMap<String, PendingCode>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static IP_SEND_COUNTS: LazyLock<Mutex<HashMap<String, (u32, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Emails that passed register verify-code recently (in-memory; used to skip a second apply code).
static RECENTLY_VERIFIED: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

pub fn is_valid_email(email: &str) -> bool {
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

/// Send a 6-digit code to `email` (rate-limited by IP). Used by register and citizenship apply.
/// Returns the issued code so callers (e.g. citizenship apply) can persist it.
pub async fn issue_verification_code(email: &str, ip: &str) -> Result<String, String> {
    let email = normalize_email(email);
    if !is_valid_email(&email) {
        return Err("Введите корректный email-адрес".to_string());
    }
    check_ip_rate_limit(ip)?;

    let Some(smtp) = SmtpConfig::from_env() else {
        return Err("Отправка email не настроена на сервере".to_string());
    };

    let now = Instant::now();
    {
        let mut codes = CODES.lock().expect("verification codes lock");
        purge_expired_codes(&mut codes);
        if let Some(existing) = codes.get(&email) {
            if now.duration_since(existing.last_sent_at) < RESEND_COOLDOWN {
                return Err(
                    "Код уже отправлен. Подождите минуту перед повторной отправкой.".to_string(),
                );
            }
        }
    }

    let code = generate_code();
    send_verification_code(&smtp, &email, &code)
        .await
        .map_err(|e| format!("Не удалось отправить письмо: {e}"))?;

    {
        let mut codes = CODES.lock().expect("verification codes lock");
        codes.insert(
            email.clone(),
            PendingCode {
                code: code.clone(),
                expires_at: now + CODE_TTL,
                last_sent_at: now,
            },
        );
    }
    record_ip_send(ip);
    Ok(code)
}

/// Consume a previously issued verification code. Removes it on success.
pub fn consume_verification_code(email: &str, code: &str) -> Result<(), String> {
    let email = normalize_email(email);
    let code = code.trim();
    if !is_valid_email(&email) {
        return Err("Введите корректный email-адрес".to_string());
    }
    if !code.chars().all(|c| c.is_ascii_digit()) || code.len() != 6 {
        return Err("Код должен состоять из 6 цифр".to_string());
    }

    let mut codes = CODES.lock().expect("verification codes lock");
    purge_expired_codes(&mut codes);

    let Some(pending) = codes.get(&email) else {
        return Err("Код не найден. Запросите новый код.".to_string());
    };
    if pending.expires_at <= Instant::now() {
        codes.remove(&email);
        return Err("Код истёк. Запросите новый код.".to_string());
    }
    if pending.code != code {
        return Err("Неверный код. Проверьте письмо и попробуйте снова.".to_string());
    }
    codes.remove(&email);
    drop(codes);
    mark_email_recently_verified(&email);
    Ok(())
}

fn purge_expired_verified(map: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    map.retain(|_, expires_at| *expires_at > now);
}

/// Record that `email` completed register verify-code (for apply auto-confirm / session).
/// Lives for [`CODE_TTL`] — the same window as the verification code.
pub fn mark_email_recently_verified(email: &str) {
    let email = normalize_email(email);
    if !is_valid_email(&email) {
        return;
    }
    let mut map = RECENTLY_VERIFIED.lock().expect("recently verified lock");
    purge_expired_verified(&mut map);
    map.insert(email, Instant::now() + CODE_TTL);
}

/// Peek whether a recent register email verification is still valid (does not consume).
/// Used by `/api/my-applications` so lookups do not burn the apply auto-confirm mark.
pub fn peek_recent_email_verification(email: &str) -> bool {
    recent_email_verification_expires_in(email).is_some()
}

/// Remaining TTL of the verified-email mark, if still valid (does not consume or refresh).
pub fn recent_email_verification_expires_in(email: &str) -> Option<u64> {
    let email = normalize_email(email);
    if !is_valid_email(&email) {
        return None;
    }
    let mut map = RECENTLY_VERIFIED.lock().expect("recently verified lock");
    purge_expired_verified(&mut map);
    let expires_at = map.get(&email)?;
    let now = Instant::now();
    if *expires_at <= now {
        return None;
    }
    Some(expires_at.duration_since(now).as_secs().max(1))
}

/// Extend a live verified-email mark by [`CODE_TTL`] from now (activity refresh).
/// Returns new `expires_in_secs`, or `None` if the mark was missing/expired.
pub fn refresh_recent_email_verification(email: &str) -> Option<u64> {
    let email = normalize_email(email);
    if !is_valid_email(&email) {
        return None;
    }
    let mut map = RECENTLY_VERIFIED.lock().expect("recently verified lock");
    purge_expired_verified(&mut map);
    let now = Instant::now();
    let expires_at = map.get(&email)?;
    if *expires_at <= now {
        return None;
    }
    map.insert(email, now + CODE_TTL);
    Some(CODE_TTL.as_secs())
}

/// Drop a verified-email mark without consuming it for apply auto-confirm semantics.
pub fn clear_recent_email_verification(email: &str) {
    let email = normalize_email(email);
    if !is_valid_email(&email) {
        return;
    }
    let mut map = RECENTLY_VERIFIED.lock().expect("recently verified lock");
    map.remove(&email);
}

/// Consume a recent register email verification, if still valid.
/// Returns `true` once; subsequent calls return `false` until email is verified again.
pub fn take_recent_email_verification(email: &str) -> bool {
    let email = normalize_email(email);
    if !is_valid_email(&email) {
        return false;
    }
    let mut map = RECENTLY_VERIFIED.lock().expect("recently verified lock");
    purge_expired_verified(&mut map);
    map.remove(&email).is_some()
}

/// True if `email` belongs to a real citizen (not a chat/portal placeholder).
///
/// Excludes applicant Candidates and synthetic portal chat rows (`cand-*`, `guest-*`).
pub async fn email_belongs_to_citizen(pool: &PgPool, email: &str) -> Result<bool, sqlx::Error> {
    let email = normalize_email(email);
    sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM citizens
            WHERE lower(email) = lower($1)
              AND id NOT LIKE 'cand-%'
              AND id NOT LIKE 'guest-%'
              AND NOT (
                status = 'applicant'
                AND COALESCE(role, '') = 'Candidate'
              )
            LIMIT 1
        )
        "#,
    )
    .bind(&email)
    .fetch_one(pool)
    .await
}

/// Citizenship and/or legacy street-mayor application exists for this email.
pub async fn email_has_candidate_application(
    pool: &PgPool,
    email: &str,
) -> Result<bool, sqlx::Error> {
    let email = normalize_email(email);
    crate::candidates::email_has_application(pool, &email).await
}

/// 403 with UI text «Вход для граждан через приложение» and `data.code = citizen_use_app`.
pub fn citizen_use_app_forbidden() -> Response {
    (
        axum::http::StatusCode::FORBIDDEN,
        Json(ApiResponse {
            status: "error".to_string(),
            data: Some(serde_json::json!({ "code": CITIZEN_USE_APP_CODE })),
            error: Some(CITIZEN_USE_APP_MESSAGE.to_string()),
        }),
    )
        .into_response()
}

fn generate_code() -> String {
    format!("{:06}", uuid::Uuid::new_v4().as_u128() % 1_000_000)
}

fn purge_expired_codes(codes: &mut HashMap<String, PendingCode>) {
    let now = Instant::now();
    codes.retain(|_, entry| entry.expires_at > now);
}

fn check_ip_rate_limit(ip: &str) -> Result<(), String> {
    let mut counts = IP_SEND_COUNTS.lock().expect("ip send counts lock");
    let now = Instant::now();
    let hour = Duration::from_secs(3600);

    counts.retain(|_, (_, started_at)| now.duration_since(*started_at) < hour);

    let entry = counts.get(ip).copied().unwrap_or((0, now));
    let (count, started_at) = if now.duration_since(entry.1) >= hour {
        (0, now)
    } else {
        entry
    };
    if count >= MAX_SENDS_PER_IP_PER_HOUR {
        return Err("Слишком много запросов. Попробуйте позже.".to_string());
    }
    let _ = (count, started_at);
    Ok(())
}

fn record_ip_send(ip: &str) {
    let mut counts = IP_SEND_COUNTS.lock().expect("ip send counts lock");
    let now = Instant::now();
    let hour = Duration::from_secs(3600);
    counts.retain(|_, (_, started_at)| now.duration_since(*started_at) < hour);
    let entry = counts.entry(ip.to_string()).or_insert((0, now));
    if now.duration_since(entry.1) >= hour {
        *entry = (0, now);
    }
    entry.0 += 1;
}

fn client_ip(connect_info: Option<&ConnectInfo<SocketAddr>>) -> String {
    connect_info
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[derive(Deserialize)]
pub struct SendCodeRequest {
    pub email: String,
    /// reCAPTCHA token — required when RECAPTCHA is configured (before email OTP).
    pub captcha_token: Option<String>,
}

#[derive(Serialize)]
pub struct SendCodeResponse {
    pub message: String,
    pub expires_in_secs: u64,
}

#[derive(Deserialize)]
pub struct VerifyCodeRequest {
    pub email: String,
    pub code: String,
}

#[derive(Serialize)]
pub struct VerifyCodeResponse {
    pub message: String,
    /// How long the verified-email mark / session remains valid (same as code TTL).
    pub expires_in_secs: u64,
}

fn parse_json_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Response> {
    if body.is_empty() {
        return Err(response::bad_request(
            "Пустое тело запроса. Отправьте JSON с данными (например, {\"email\":\"you@example.com\"}).",
        ));
    }

    match serde_json::from_slice(body) {
        Ok(value) => Ok(value),
        Err(err) => {
            let message = if err.is_eof() {
                "Пустое тело запроса. Отправьте JSON с данными."
            } else {
                "Некорректный JSON в теле запроса."
            };
            Err(response::bad_request(message))
        }
    }
}

pub async fn send_code_handler(
    State(_state): State<Arc<AppState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    body: Bytes,
) -> impl IntoResponse {
    let req: SendCodeRequest = match parse_json_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };

    let email = normalize_email(&req.email);
    let ip = client_ip(connect_info.as_ref());

    // Phase D: reCAPTCHA before email OTP when configured.
    if crate::applications::recaptcha_required() {
        let token = req.captcha_token.as_deref().unwrap_or("").trim();
        if token.is_empty() {
            return response::bad_request("Пройдите reCAPTCHA перед отправкой кода");
        }
        if let Err(msg) =
            crate::applications::verify_recaptcha_with_ip(token, Some(&ip)).await
        {
            tracing::warn!(error = %msg, "recaptcha verify failed on send-code");
            return response::bad_request(msg);
        }
    }

    // Citizens may verify email for portal chat; apply/my-applications still gate separately.

    match issue_verification_code(&email, &ip).await {
        Ok(_code) => {
            tracing::info!(to = %email, client_ip = %ip, "verification code sent");
            Json(ApiResponse::success(SendCodeResponse {
                message: "Код отправлен на ваш email".to_string(),
                expires_in_secs: CODE_TTL.as_secs(),
            }))
            .into_response()
        }
        Err(message) => {
            if message.contains("не настроена") {
                tracing::error!("SMTP not configured (SMTP_* / GMAIL_*)");
                return response::internal_error(
                    "Отправка email не настроена на сервере. Обратитесь к администратору.",
                );
            }
            if message.contains("Слишком много") {
                return response::err_response(axum::http::StatusCode::TOO_MANY_REQUESTS, message);
            }
            if message.contains("Не удалось отправить") {
                tracing::error!(to = %email, error = %message, "failed to send verification email");
                return response::internal_error(
                    "Не удалось отправить письмо. Попробуйте позже или напишите на quazardr@gmail.com",
                );
            }
            response::bad_request(message)
        }
    }
}

pub async fn verify_code_handler(
    State(_state): State<Arc<AppState>>,
    body: Bytes,
) -> impl IntoResponse {
    let req: VerifyCodeRequest = match parse_json_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };

    let email = normalize_email(&req.email);

    match consume_verification_code(&email, &req.code) {
        Ok(()) => Json(ApiResponse::success(VerifyCodeResponse {
            message: "Email подтверждён".to_string(),
            expires_in_secs: CODE_TTL.as_secs(),
        }))
        .into_response(),
        Err(message) => response::bad_request(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_emails() {
        assert!(is_valid_email("user@example.com"));
        assert!(is_valid_email("  User@Example.COM  "));
    }

    #[test]
    fn rejects_invalid_emails() {
        assert!(!is_valid_email(""));
        assert!(!is_valid_email("not-an-email"));
        assert!(!is_valid_email("@example.com"));
        assert!(!is_valid_email("user@"));
    }

    #[test]
    fn generate_code_is_six_digits() {
        let code = generate_code();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn parse_json_body_rejects_empty() {
        let body = Bytes::new();
        let result: Result<SendCodeRequest, _> = parse_json_body(&body);
        assert!(result.is_err());
    }

    #[test]
    fn recent_verify_mark_can_be_taken_once() {
        let email = "apply-auto-confirm@example.com";
        mark_email_recently_verified(email);
        assert!(take_recent_email_verification(email));
        assert!(!take_recent_email_verification(email));
    }

    #[test]
    fn recent_verify_peek_does_not_consume() {
        let email = "apply-peek@example.com";
        mark_email_recently_verified(email);
        assert!(peek_recent_email_verification(email));
        assert!(peek_recent_email_verification(email));
        assert!(take_recent_email_verification(email));
        assert!(!peek_recent_email_verification(email));
    }

    #[test]
    fn recent_verify_refresh_extends_ttl() {
        let email = "apply-refresh@example.com";
        mark_email_recently_verified(email);
        let before = recent_email_verification_expires_in(email).expect("mark present");
        assert!(before > 0);
        let refreshed = refresh_recent_email_verification(email).expect("refresh ok");
        assert_eq!(refreshed, CODE_TTL.as_secs());
        assert!(peek_recent_email_verification(email));
        assert!(refresh_recent_email_verification("missing-refresh@example.com").is_none());
    }
}
