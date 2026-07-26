use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Message, SmtpTransport, Transport};
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub user: String,
    pub username: String,
    pub app_password: String,
    pub from_name: String,
    pub host: String,
    pub port: u16,
    pub tls_insecure: bool,
}

impl SmtpConfig {
    pub fn from_env() -> Option<Self> {
        // SMTP_FROM for envelope From; SMTP_USERNAME / SMTP_USER for auth login.
        let user = std::env::var("SMTP_FROM")
            .or_else(|_| std::env::var("SMTP_USERNAME"))
            .or_else(|_| std::env::var("SMTP_USER"))
            .or_else(|_| std::env::var("GMAIL_USER"))
            .ok()?;
        let username = std::env::var("SMTP_USERNAME")
            .or_else(|_| std::env::var("SMTP_USER"))
            .or_else(|_| std::env::var("GMAIL_USER"))
            .unwrap_or_else(|_| user.clone());
        let app_password = std::env::var("SMTP_PASSWORD")
            .or_else(|_| std::env::var("GMAIL_APP_PASSWORD"))
            .ok()?;
        if user.trim().is_empty() || app_password.trim().is_empty() {
            return None;
        }
        let host = match std::env::var("SMTP_HOST") {
            // Explicit empty host disables outbound SMTP (soft-fail at call sites).
            Ok(h) if h.trim().is_empty() => return None,
            Ok(h) => h.trim().to_string(),
            Err(_) => "smtp.gmail.com".to_string(),
        };
        let port = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(587);
        let from_name = std::env::var("SMTP_FROM_NAME")
            .or_else(|_| std::env::var("GMAIL_FROM_NAME"))
            .unwrap_or_else(|_| "Квазар".to_string());
        let tls_insecure = std::env::var("SMTP_TLS_INSECURE")
            .map(|v| {
                let t = v.trim().to_ascii_lowercase();
                t == "1" || t == "true" || t == "yes" || t == "on"
            })
            .unwrap_or(false);
        Some(Self {
            user: user.trim().to_string(),
            username: username.trim().to_string(),
            app_password: app_password.trim().to_string(),
            from_name,
            host,
            port,
            tls_insecure,
        })
    }
}

/// Admin notification recipient (`NOTIFY_EMAIL`), if set.
pub fn notify_email() -> Option<String> {
    std::env::var("NOTIFY_EMAIL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn passport_download_url() -> String {
    std::env::var("PASSPORT_DOWNLOAD_URL").unwrap_or_else(|_| {
        "https://github.com/quazar-vs/quazar-passport/releases".to_string()
    })
}

pub fn registration_credentials_body(
    qz_id: &str,
    name: &str,
    api_key: &str,
    passport_url: &str,
) -> String {
    format!(
        "Здравствуйте!\n\n\
         Ваша заявка в Квазаре одобрена. Данные для входа:\n\n\
         QZ-ID: {qz_id}\n\
         Имя: {name}\n\
         API-ключ: {api_key}\n\n\
         Скачать Quazar Passport:\n\
         {passport_url}\n\n\
         Инструкция:\n\
         1. Сохраните API-ключ в безопасном месте.\n\
         2. Войдите в приложение Quazar Passport с вашим логином и API-ключом.\n\
         3. При первом входе приложение само создаст криптоключи на устройстве \
(приватный ключ останется только на телефоне).\n\n\
         С уважением,\n\
         Квазар\n"
    )
}

/// Best-effort: send registration credentials to confirmed external email.
/// Returns Ok(()) if SMTP is missing or send fails after logging — callers may ignore Err
/// or treat it as soft failure; prefer `send_registration_credentials_soft`.
pub async fn send_registration_credentials(
    to: &str,
    qz_id: &str,
    name: &str,
    api_key: &str,
    _public_key: &str,
    passport_url: Option<&str>,
) -> Result<(), String> {
    let Some(config) = SmtpConfig::from_env() else {
        return Err("SMTP not configured".into());
    };
    let url = passport_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(passport_download_url);
    let body = registration_credentials_body(qz_id, name, api_key, &url);
    send_plain_email(
        &config,
        to,
        "Данные для входа в Квазар",
        body,
    )
    .await
}

/// Soft-fail wrapper: empty/missing `to` or send errors are logged, never panic.
pub async fn send_registration_credentials_soft(
    to: Option<&str>,
    qz_id: &str,
    name: &str,
    api_key: &str,
    public_key: &str,
) {
    let Some(to) = to.map(str::trim).filter(|s| !s.is_empty()) else {
        tracing::info!(%qz_id, "no external_email — skip credentials email");
        return;
    };
    if let Err(e) =
        send_registration_credentials(to, qz_id, name, api_key, public_key, None).await
    {
        tracing::warn!(to = %to, error = %e, "failed to send registration credentials email");
    }
}

fn build_tls_parameters(config: &SmtpConfig) -> Result<TlsParameters, String> {
    let mut builder = TlsParameters::builder(config.host.clone());
    if config.tls_insecure {
        builder = builder.dangerous_accept_invalid_certs(true);
    }
    builder
        .build()
        .map_err(|e| format!("TLS parameters error: {e}"))
}

fn build_mailer(config: &SmtpConfig) -> Result<SmtpTransport, String> {
    let creds = Credentials::new(config.username.clone(), config.app_password.clone());
    let tls = build_tls_parameters(config)?;
    if config.port == 465 {
        Ok(SmtpTransport::relay(&config.host)
            .map_err(|e| format!("SMTP relay error: {e}"))?
            .port(465)
            .tls(Tls::Wrapper(tls))
            .credentials(creds)
            .timeout(Some(Duration::from_secs(20)))
            .build())
    } else {
        // STARTTLS on 587 (and similar): use relay + Required TLS with optional insecure.
        Ok(SmtpTransport::relay(&config.host)
            .map_err(|e| format!("SMTP relay error: {e}"))?
            .port(config.port)
            .tls(Tls::Required(tls))
            .credentials(creds)
            .timeout(Some(Duration::from_secs(20)))
            .build())
    }
}

fn send_plain_email_blocking(
    config: &SmtpConfig,
    to_email: &str,
    subject: &str,
    body: String,
) -> Result<(), String> {
    let from_mailbox: Mailbox = Mailbox::new(
        Some(config.from_name.clone()),
        config
            .user
            .parse()
            .map_err(|e| format!("invalid from address: {e}"))?,
    );

    let email = Message::builder()
        .from(from_mailbox)
        .to(to_email
            .parse()
            .map_err(|e| format!("invalid recipient address: {e}"))?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body)
        .map_err(|e| format!("failed to build email: {e}"))?;

    build_mailer(config)?
        .send(&email)
        .map_err(|e| format!("failed to send email: {e}"))?;

    Ok(())
}

pub async fn send_plain_email_public(
    config: &SmtpConfig,
    to_email: &str,
    subject: &str,
    body: String,
) -> Result<(), String> {
    send_plain_email(config, to_email, subject, body).await
}

async fn send_plain_email(
    config: &SmtpConfig,
    to_email: &str,
    subject: &str,
    body: String,
) -> Result<(), String> {
    let config = config.clone();
    let to_email = to_email.to_string();
    let subject = subject.to_string();

    timeout(
        Duration::from_secs(25),
        tokio::task::spawn_blocking(move || {
            send_plain_email_blocking(&config, &to_email, &subject, body)
        }),
    )
    .await
    .map_err(|_| "SMTP send timed out (check network or SMTP credentials)".to_string())?
    .map_err(|e| format!("SMTP task failed: {e}"))?
}

fn send_verification_code_blocking(
    config: &SmtpConfig,
    to_email: &str,
    code: &str,
) -> Result<(), String> {
    let body = format!(
        "Здравствуйте!\n\n\
         Ваш код подтверждения регистрации в Квазаре: {code}\n\n\
         Код действителен 10 минут.\n\n\
         С уважением,\n\
         {from_name}\n\
         {from_email}\n\n\
         Если вы не запрашивали регистрацию — проигнорируйте это письмо.",
        from_name = config.from_name,
        from_email = config.user,
    );
    send_plain_email_blocking(
        config,
        to_email,
        "Код подтверждения регистрации в Квазаре",
        body,
    )
}

pub async fn send_verification_code(
    config: &SmtpConfig,
    to_email: &str,
    code: &str,
) -> Result<(), String> {
    let config = config.clone();
    let to_email = to_email.to_string();
    let code = code.to_string();

    timeout(
        Duration::from_secs(25),
        tokio::task::spawn_blocking(move || {
            send_verification_code_blocking(&config, &to_email, &code)
        }),
    )
    .await
    .map_err(|_| "SMTP send timed out (check network or SMTP credentials)".to_string())?
    .map_err(|e| format!("SMTP task failed: {e}"))?
}

fn send_citizenship_application_blocking(
    config: &SmtpConfig,
    to_email: &str,
    name: &str,
    applicant_email: &str,
    reason: &str,
) -> Result<(), String> {
    let body = format!(
        "--- НОВАЯ ЗАЯВКА НА ГРАЖДАНСТВО КВАЗАРА ---\n\
         Имя: {name}\n\
         Email: {email}\n\
         Полезность: {reason}\n\
         --- КОНЕЦ ЗАЯВКИ ---",
        email = applicant_email,
    );
    send_plain_email_blocking(
        config,
        to_email,
        "Новая заявка на гражданство Квазара",
        body,
    )
}

pub async fn send_citizenship_application(
    config: &SmtpConfig,
    to_email: &str,
    name: &str,
    applicant_email: &str,
    reason: &str,
) -> Result<(), String> {
    let config = config.clone();
    let to_email = to_email.to_string();
    let name = name.to_string();
    let applicant_email = applicant_email.to_string();
    let reason = reason.to_string();

    timeout(
        Duration::from_secs(25),
        tokio::task::spawn_blocking(move || {
            send_citizenship_application_blocking(
                &config,
                &to_email,
                &name,
                &applicant_email,
                &reason,
            )
        }),
    )
    .await
    .map_err(|_| "SMTP send timed out (check network or SMTP credentials)".to_string())?
    .map_err(|e| format!("SMTP task failed: {e}"))?
}

fn send_mayor_street_application_blocking(
    config: &SmtpConfig,
    to_email: &str,
    name: &str,
    applicant_email: &str,
    login: &str,
    passport_data: &str,
    reason: &str,
) -> Result<(), String> {
    let body = format!(
        "--- НОВАЯ ЗАЯВКА НА МЭРСТВО (С САЙТА) ---\n\
         ФИО: {name}\n\
         Логин: {login}\n\
         Email: {email}\n\
         Паспорт: {passport}\n\
         Причина: {reason}\n\
         --- КОНЕЦ ЗАЯВКИ ---",
        email = applicant_email,
        passport = passport_data,
    );
    send_plain_email_blocking(
        config,
        to_email,
        "Новая заявка на мэрство Квазара",
        body,
    )
}

pub async fn send_mayor_street_application(
    config: &SmtpConfig,
    to_email: &str,
    name: &str,
    applicant_email: &str,
    login: &str,
    passport_data: &str,
    reason: &str,
) -> Result<(), String> {
    let config = config.clone();
    let to_email = to_email.to_string();
    let name = name.to_string();
    let applicant_email = applicant_email.to_string();
    let login = login.to_string();
    let passport_data = passport_data.to_string();
    let reason = reason.to_string();

    timeout(
        Duration::from_secs(25),
        tokio::task::spawn_blocking(move || {
            send_mayor_street_application_blocking(
                &config,
                &to_email,
                &name,
                &applicant_email,
                &login,
                &passport_data,
                &reason,
            )
        }),
    )
    .await
    .map_err(|_| "SMTP send timed out (check network or SMTP credentials)".to_string())?
    .map_err(|e| format!("SMTP task failed: {e}"))?
}

pub async fn send_mayor_street_confirmation(
    config: &SmtpConfig,
    to_email: &str,
    application_id: &str,
) -> Result<(), String> {
    tracing::info!(email = %to_email, application_id = %application_id, "📧 Отправка письма о принятии заявки на мэрство");

    let body = format!(
        "Здравствуйте!\n\n\
         Ваша заявка на мэрство Квазара принята. Email подтверждён.\n\n\
         Номер заявки: {application_id}\n\
         Статус: подтверждена, ожидает решения администрации.\n\n\
         Вы можете проверить статус заявки на портале Квазара.\n\n\
         С уважением,\n\
         {from_name}\n\
         {from_email}",
        from_name = config.from_name,
        from_email = config.user,
    );

    send_plain_email(
        config,
        to_email,
        "Заявка на мэрство Квазара принята",
        body,
    )
    .await?;

    tracing::info!(email = %to_email, application_id = %application_id, "📧 Письмо о принятии заявки на мэрство отправлено");
    Ok(())
}

pub async fn send_citizenship_confirmation(
    config: &SmtpConfig,
    to_email: &str,
    application_id: &str,
) -> Result<(), String> {
    tracing::info!(email = %to_email, application_id = %application_id, "📧 Отправка письма о принятии");

    let body = format!(
        "Здравствуйте!\n\n\
         Ваша заявка на гражданство Квазара принята. Email подтверждён.\n\n\
         Номер заявки: {application_id}\n\
         Статус: подтверждена, ожидает решения администрации.\n\n\
         Вы можете проверить статус заявки на портале Квазара.\n\n\
         С уважением,\n\
         {from_name}\n\
         {from_email}",
        from_name = config.from_name,
        from_email = config.user,
    );

    send_plain_email(
        config,
        to_email,
        "Заявка на гражданство Квазара принята",
        body,
    )
    .await?;

    tracing::info!(email = %to_email, application_id = %application_id, "📧 Письмо о принятии отправлено");
    Ok(())
}

fn send_citizenship_approval_blocking(
    config: &SmtpConfig,
    to_email: &str,
    login: &str,
    api_key: &str,
) -> Result<(), String> {
    let body = format!(
        "Здравствуйте!\n\n\
         Ваша заявка на гражданство Квазара одобрена.\n\n\
         Логин: {login}\n\
         API-ключ: {api_key}\n\n\
         Сохраните ключ в безопасном месте. Он понадобится для входа в API Квазара.\n\n\
         С уважением,\n\
         {from_name}\n\
         {from_email}",
        from_name = config.from_name,
        from_email = config.user,
    );
    send_plain_email_blocking(
        config,
        to_email,
        "Ваша заявка на гражданство Квазара одобрена",
        body,
    )
}

pub async fn send_citizenship_approval(
    config: &SmtpConfig,
    to_email: &str,
    login: &str,
    api_key: &str,
) -> Result<(), String> {
    let config = config.clone();
    let to_email = to_email.to_string();
    let login = login.to_string();
    let api_key = api_key.to_string();

    timeout(
        Duration::from_secs(25),
        tokio::task::spawn_blocking(move || {
            send_citizenship_approval_blocking(&config, &to_email, &login, &api_key)
        }),
    )
    .await
    .map_err(|_| "SMTP send timed out (check network or SMTP credentials)".to_string())?
    .map_err(|e| format!("SMTP task failed: {e}"))?
}

fn send_citizenship_rejection_blocking(
    config: &SmtpConfig,
    to_email: &str,
    reason: &str,
) -> Result<(), String> {
    let body = format!(
        "Здравствуйте!\n\n\
         К сожалению, ваша заявка на гражданство Квазара отклонена.\n\n\
         Причина отказа:\n\
         {reason}\n\n\
         При необходимости вы можете подать новую заявку позже.\n\n\
         С уважением,\n\
         {from_name}\n\
         {from_email}",
        from_name = config.from_name,
        from_email = config.user,
    );
    send_plain_email_blocking(
        config,
        to_email,
        "Ваша заявка на гражданство Квазара отклонена",
        body,
    )
}

pub async fn send_citizenship_rejection(
    config: &SmtpConfig,
    to_email: &str,
    reason: &str,
) -> Result<(), String> {
    let config = config.clone();
    let to_email = to_email.to_string();
    let reason = reason.to_string();

    timeout(
        Duration::from_secs(25),
        tokio::task::spawn_blocking(move || {
            send_citizenship_rejection_blocking(&config, &to_email, &reason)
        }),
    )
    .await
    .map_err(|_| "SMTP send timed out (check network or SMTP credentials)".to_string())?
    .map_err(|e| format!("SMTP task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_body_contains_fields() {
        let body = registration_credentials_body(
            "qz-1",
            "alice",
            "key-xyz",
            "https://example.com/passport",
        );
        assert!(body.contains("QZ-ID: qz-1"));
        assert!(body.contains("Имя: alice"));
        assert!(body.contains("API-ключ: key-xyz"));
        assert!(!body.contains("Публичный ключ"));
        assert!(!body.contains("приватн"));
        assert!(body.contains("https://example.com/passport"));
    }

    #[test]
    fn from_env_empty_host_disables() {
        std::env::set_var("SMTP_FROM", "a@b.c");
        std::env::set_var("SMTP_USERNAME", "a@b.c");
        std::env::set_var("SMTP_PASSWORD", "secret");
        std::env::set_var("SMTP_HOST", "");
        assert!(SmtpConfig::from_env().is_none());
        std::env::remove_var("SMTP_FROM");
        std::env::remove_var("SMTP_USERNAME");
        std::env::remove_var("SMTP_PASSWORD");
        std::env::remove_var("SMTP_HOST");
    }

    #[test]
    fn smtp_user_alias_works() {
        std::env::remove_var("SMTP_USERNAME");
        std::env::remove_var("GMAIL_USER");
        std::env::set_var("SMTP_FROM", "from@example.com");
        std::env::set_var("SMTP_USER", "user@example.com");
        std::env::set_var("SMTP_PASSWORD", "secret");
        std::env::set_var("SMTP_HOST", "smtp.example.com");
        let cfg = SmtpConfig::from_env().expect("config");
        assert_eq!(cfg.username, "user@example.com");
        assert_eq!(cfg.user, "from@example.com");
        assert_eq!(cfg.host, "smtp.example.com");
        std::env::remove_var("SMTP_FROM");
        std::env::remove_var("SMTP_USER");
        std::env::remove_var("SMTP_PASSWORD");
        std::env::remove_var("SMTP_HOST");
    }
}
