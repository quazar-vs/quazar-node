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
}

impl SmtpConfig {
    pub fn from_env() -> Option<Self> {
        let user = std::env::var("SMTP_FROM")
            .or_else(|_| std::env::var("SMTP_USERNAME"))
            .or_else(|_| std::env::var("GMAIL_USER"))
            .ok()?;
        let username = std::env::var("SMTP_USERNAME")
            .or_else(|_| std::env::var("GMAIL_USER"))
            .unwrap_or_else(|_| user.clone());
        let app_password = std::env::var("SMTP_PASSWORD")
            .or_else(|_| std::env::var("GMAIL_APP_PASSWORD"))
            .ok()?;
        if user.trim().is_empty() || app_password.trim().is_empty() {
            return None;
        }
        let host = std::env::var("SMTP_HOST").unwrap_or_else(|_| "smtp.gmail.com".to_string());
        let port = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(587);
        let from_name = std::env::var("GMAIL_FROM_NAME")
            .unwrap_or_else(|_| "Квазар".to_string());
        Some(Self {
            user: user.trim().to_string(),
            username: username.trim().to_string(),
            app_password: app_password.trim().to_string(),
            from_name,
            host: host.trim().to_string(),
            port,
        })
    }
}

fn build_mailer(config: &SmtpConfig) -> Result<SmtpTransport, String> {
    let creds = Credentials::new(config.username.clone(), config.app_password.clone());
    if config.port == 465 {
        let tls = TlsParameters::new(config.host.clone())
            .map_err(|e| format!("TLS parameters error: {e}"))?;
        Ok(SmtpTransport::relay(&config.host)
            .map_err(|e| format!("SMTP relay error: {e}"))?
            .port(465)
            .tls(Tls::Wrapper(tls))
            .credentials(creds)
            .timeout(Some(Duration::from_secs(20)))
            .build())
    } else {
        Ok(SmtpTransport::starttls_relay(&config.host)
            .map_err(|e| format!("SMTP relay error: {e}"))?
            .port(config.port)
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
