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

use crate::email::{send_verification_code, SmtpConfig};
use crate::response::{self, ApiResponse};
use crate::AppState;

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
    Ok(())
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
                tracing::error!("GMAIL_USER / GMAIL_APP_PASSWORD not configured");
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
                    "Не удалось отправить письмо. Попробуйте позже или напишите на quazarvs@gmail.com",
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

    match consume_verification_code(&req.email, &req.code) {
        Ok(()) => Json(ApiResponse::success(VerifyCodeResponse {
            message: "Email подтверждён".to_string(),
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
}
