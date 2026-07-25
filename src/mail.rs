//! Automatic mailbox provisioning on docker-mailserver for applications.
//!
//! Failures are logged and never block citizenship / mayor registration.

use std::process::Command;
use std::time::Duration;

use rand::Rng;
use tokio::time::timeout;

use crate::email::{send_plain_email_public, SmtpConfig};
use crate::register_verify::{is_valid_email, normalize_email};

const DEFAULT_MAIL_DOMAIN: &str = "quazar.website";
const DEFAULT_MAIL_FROM: &str = "no-reply@quazar.website";
const DEFAULT_CONTAINER: &str = "quazar-mailserver-1";
const MIN_PASSWORD_LEN: usize = 16;

pub fn mail_auto_create_enabled() -> bool {
    match std::env::var("MAIL_AUTO_CREATE") {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            !(t == "0" || t == "false" || t == "no" || t == "off")
        }
        Err(_) => true,
    }
}

pub fn mail_domain() -> String {
    std::env::var("MAIL_DOMAIN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MAIL_DOMAIN.to_string())
}

pub fn mail_from() -> String {
    std::env::var("MAIL_FROM")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MAIL_FROM.to_string())
}

fn mail_container() -> String {
    std::env::var("MAIL_CONTAINER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CONTAINER.to_string())
}

/// Random mailbox password (16+ chars, alphanumeric + safe punctuation).
pub fn generate_mail_password() -> String {
    const CHARSET: &[u8] =
        b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789!@#$%^&*-_";
    let mut rng = rand::thread_rng();
    (0..MIN_PASSWORD_LEN)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Build `<local>@MAIL_DOMAIN` from login or QZ-ID. Sanitizes local-part.
pub fn build_internal_email(preferred_local: &str) -> String {
    let domain = mail_domain();
    let mut local: String = preferred_local
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    while local.contains("..") {
        local = local.replace("..", ".");
    }
    local = local.trim_matches('.').to_string();
    if local.is_empty() {
        local = format!("qz{}", UuidLite::new());
    }
    if local.len() > 64 {
        local.truncate(64);
    }
    format!("{local}@{domain}")
}

/// Tiny helper without pulling uuid into every call site for fallback.
struct UuidLite;
impl UuidLite {
    fn new() -> String {
        format!("{:x}", rand::thread_rng().gen::<u64>())
    }
}

fn create_mailbox_blocking(email: &str, password: &str) -> Result<(), String> {
    let email = normalize_email(email);
    if !is_valid_email(&email) {
        return Err("invalid mailbox address".into());
    }
    if password.len() < MIN_PASSWORD_LEN {
        return Err("password too short".into());
    }

    let container = mail_container();
    let output = Command::new("docker")
        .args([
            "exec",
            &container,
            "setup",
            "email",
            "add",
            &email,
            password,
        ])
        .output()
        .map_err(|e| format!("docker exec failed (mailserver unavailable?): {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}").to_ascii_lowercase();
    // Idempotent: account already exists
    if combined.contains("already exists") || combined.contains("exists") {
        tracing::warn!(%email, "mailbox already exists — treating as ok");
        return Ok(());
    }
    Err(format!(
        "setup email add failed (status {:?}): {}",
        output.status.code(),
        combined.trim()
    ))
}

/// Create mailbox via `docker exec … setup email add`. Never panics.
pub async fn create_mailbox(email: &str, password: &str) -> Result<(), String> {
    let email = email.to_string();
    let password = password.to_string();
    timeout(
        Duration::from_secs(45),
        tokio::task::spawn_blocking(move || create_mailbox_blocking(&email, &password)),
    )
    .await
    .map_err(|_| "create_mailbox timed out".to_string())?
    .map_err(|e| format!("create_mailbox task failed: {e}"))?
}

fn smtp_for_mail_from() -> Option<SmtpConfig> {
    let mut cfg = SmtpConfig::from_env()?;
    let from = mail_from();
    if !from.is_empty() {
        cfg.user = from;
    }
    if let Ok(name) = std::env::var("SMTP_FROM_NAME") {
        if !name.trim().is_empty() {
            cfg.from_name = name;
        }
    }
    Some(cfg)
}

pub async fn send_external_email(to: &str, subject: &str, body: &str) -> Result<(), String> {
    let Some(cfg) = smtp_for_mail_from() else {
        return Err("SMTP not configured".into());
    };
    send_plain_email_public(&cfg, to, subject, body.to_string()).await
}

pub async fn send_internal_email(to: &str, subject: &str, body: &str) -> Result<(), String> {
    send_external_email(to, subject, body).await
}

pub fn mailbox_credentials_body(internal_email: &str, password: &str) -> String {
    format!(
        "Здравствуйте!\n\n\
         Для вас создан внутренний почтовый ящик Квазара.\n\n\
         Логин (внутренний email): {internal_email}\n\
         Пароль: {password}\n\n\
         Войдите во внутренний ящик, чтобы получить данные для входа в Квазар \
         (после одобрения заявки).\n\n\
         Сервер IMAP/SMTP: mail.{domain}\n\
         Домен: {domain}\n\n\
         С уважением,\n\
         Квазар\n",
        domain = mail_domain(),
    )
}

pub fn quazar_credentials_body(
    qz_id: &str,
    name: &str,
    api_key: &str,
    public_key: &str,
    passport_url: &str,
) -> String {
    format!(
        "Здравствуйте!\n\n\
         Ваша заявка в Квазаре одобрена. Данные для входа:\n\n\
         QZ-ID: {qz_id}\n\
         Имя: {name}\n\
         API-ключ: {api_key}\n\
         Публичный ключ: {public_key}\n\n\
         Скачать Quazar Passport:\n\
         {passport_url}\n\n\
         Инструкция:\n\
         1. Сохраните API-ключ в безопасном месте.\n\
         2. Войдите на портал Квазара с вашим логином и API-ключом.\n\
         3. При необходимости импортируйте паспорт по ссылке выше.\n\n\
         С уважением,\n\
         Квазар\n"
    )
}

/// Best-effort: create mailbox + email external address. Returns (internal_email, password, created).
pub async fn provision_mailbox_for_application(
    preferred_local: &str,
    external_email: &str,
) -> (String, String, bool) {
    let internal = build_internal_email(preferred_local);
    let password = generate_mail_password();

    if !mail_auto_create_enabled() {
        tracing::info!(%internal, "MAIL_AUTO_CREATE disabled — skipping mailbox");
        return (internal, password, false);
    }

    match create_mailbox(&internal, &password).await {
        Ok(()) => {
            let body = mailbox_credentials_body(&internal, &password);
            if let Err(e) = send_external_email(
                external_email,
                "Внутренний почтовый ящик Квазара",
                &body,
            )
            .await
            {
                tracing::warn!(
                    to = %external_email,
                    error = %e,
                    "mailbox created but failed to send credentials to external email"
                );
            }
            (internal, password, true)
        }
        Err(e) => {
            tracing::warn!(
                internal = %internal,
                error = %e,
                "failed to create mailbox — continuing registration without mail"
            );
            (internal, password, false)
        }
    }
}

/// Best-effort send of Quazar credentials to the internal mailbox.
pub async fn send_credentials_to_internal_mailbox(
    internal_email: Option<&str>,
    qz_id: &str,
    name: &str,
    api_key: &str,
    public_key: &str,
) {
    let Some(to) = internal_email.map(str::trim).filter(|s| !s.is_empty()) else {
        tracing::info!("no internal_email — skip credentials mail");
        return;
    };
    // App download link (GitHub releases / APK), not the node API.
    let passport_url = std::env::var("PASSPORT_DOWNLOAD_URL").unwrap_or_else(|_| {
        "https://github.com/quazar-vs/quazar-passport/releases".to_string()
    });
    let body = quazar_credentials_body(qz_id, name, api_key, public_key, &passport_url);
    if let Err(e) = send_internal_email(to, "Данные для входа в Квазар", &body).await {
        tracing::warn!(to = %to, error = %e, "failed to send credentials to internal mailbox");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_email_sanitizes_login() {
        std::env::set_var("MAIL_DOMAIN", "quazar.website");
        let e = build_internal_email("Test User!");
        assert!(e.ends_with("@quazar.website"));
        assert!(!e.contains(' '));
        assert!(!e.contains('!'));
    }

    #[test]
    fn password_long_enough() {
        let p = generate_mail_password();
        assert!(p.len() >= 16);
    }
}
