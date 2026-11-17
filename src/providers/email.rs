use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use base64::Engine;

use super::DnsRecord;

#[derive(Debug, Clone)]
pub struct OutgoingEmail {
    pub message_id: uuid::Uuid,
    pub to: String,
    pub from_name: String,
    pub from_email: String,
    pub reply_to: Option<String>,
    pub subject: String,
    pub html: String,
    pub text: String,
    /// One-click unsubscribe URL (RFC 8058). Omitted for transactional/test mail.
    pub unsubscribe_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DomainState {
    pub status: String, // pending | verified | failed
    pub records: Vec<DnsRecord>,
}

/// The provider can't take mail right now: its daily quota is used up, it keeps asking us to slow down,
/// or sending is paused on its side. The message should wait and try again later instead of failing.
#[derive(Debug)]
pub struct ProviderBusy {
    pub retry_after: Duration,
    pub reason: String,
}

impl std::fmt::Display for ProviderBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "email provider busy: {}", self.reason)
    }
}

impl std::error::Error for ProviderBusy {}

#[async_trait]
pub trait EmailProvider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Most emails per second the provider accepts, when it says (SES: the account's sending rate).
    fn max_send_rate(&self) -> Option<f64> {
        None
    }
    /// Re-read the provider's sending limits.
    async fn refresh_limits(&self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Sends the message and returns the provider's message id.
    async fn send(&self, email: &OutgoingEmail) -> anyhow::Result<String>;
    async fn add_domain(&self, domain: &str) -> anyhow::Result<DomainState>;
    async fn domain_state(&self, domain: &str) -> anyhow::Result<DomainState>;
    /// Use a custom MAIL FROM subdomain so SPF aligns with the sending domain (DMARC).
    async fn set_mail_from(&self, _domain: &str, _mail_from: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn remove_domain(&self, _domain: &str) -> anyhow::Result<()> {
        Ok(())
    }
    /// Sent messages kept in memory (log provider only).
    fn sent(&self) -> Vec<OutgoingEmail> {
        Vec::new()
    }
}

/// Build an RFC 5322 MIME message with text and HTML parts.
pub fn build_mime(email: &OutgoingEmail, config_set_header: Option<&str>) -> String {
    let boundary = format!("pl-{}", email.message_id.simple());
    let mut out = String::new();
    let from = format!("{} <{}>", encode_header(&email.from_name), email.from_email);
    out.push_str(&format!("From: {from}\r\n"));
    out.push_str(&format!("To: {}\r\n", email.to));
    if let Some(r) = &email.reply_to {
        out.push_str(&format!("Reply-To: {r}\r\n"));
    }
    out.push_str(&format!("Subject: {}\r\n", encode_header(&email.subject)));
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str(&format!("X-DumbMail-Message: {}\r\n", email.message_id));
    if let Some(cs) = config_set_header {
        out.push_str(&format!("X-SES-CONFIGURATION-SET: {cs}\r\n"));
    }
    if let Some(u) = &email.unsubscribe_url {
        out.push_str(&format!("List-Unsubscribe: <{u}>\r\n"));
        out.push_str("List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n");
    }
    out.push_str(&format!("Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n"));
    for (ctype, body) in [("text/plain", &email.text), ("text/html", &email.html)] {
        out.push_str(&format!("--{boundary}\r\n"));
        out.push_str(&format!("Content-Type: {ctype}; charset=UTF-8\r\n"));
        out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        let b64 = base64::engine::general_purpose::STANDARD.encode(body.as_bytes());
        for chunk in b64.as_bytes().chunks(76) {
            out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
            out.push_str("\r\n");
        }
    }
    out.push_str(&format!("--{boundary}--\r\n"));
    out
}

fn encode_header(s: &str) -> String {
    let clean: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    if clean.is_ascii() && !clean.contains(['"', '<', '>', ',', ';']) {
        clean
    } else {
        format!(
            "=?UTF-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode(clean.as_bytes())
        )
    }
}

pub struct LogEmail {
    sent: Mutex<Vec<OutgoingEmail>>,
}

impl Default for LogEmail {
    fn default() -> Self {
        Self::new()
    }
}

impl LogEmail {
    pub fn new() -> Self {
        Self { sent: Mutex::new(Vec::new()) }
    }
}

#[async_trait]
impl EmailProvider for LogEmail {
    fn name(&self) -> &'static str {
        "log"
    }
    async fn send(&self, email: &OutgoingEmail) -> anyhow::Result<String> {
        tracing::info!(to = %email.to, subject = %email.subject, "email (log provider, not sent)");
        let mut sent = self.sent.lock().map_err(|_| anyhow!("poisoned"))?;
        if sent.len() > 500 {
            sent.remove(0);
        }
        sent.push(email.clone());
        Ok(format!("log-{}", email.message_id))
    }
    async fn add_domain(&self, domain: &str) -> anyhow::Result<DomainState> {
        Ok(DomainState { status: "pending".into(), records: fake_records(domain) })
    }
    async fn domain_state(&self, domain: &str) -> anyhow::Result<DomainState> {
        self.add_domain(domain).await
    }
    fn sent(&self) -> Vec<OutgoingEmail> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Dev stand-in for DKIM records: deterministic CNAMEs that look like SES's.
fn fake_records(domain: &str) -> Vec<DnsRecord> {
    (1..=3)
        .map(|i| {
            let token = format!("pl{i}{:x}", domain.len() * 7919 + i);
            DnsRecord { kind: "CNAME".into(), name: format!("{token}._domainkey.{domain}"), value: format!("{token}.dkim.amazonses.com"), status: "pending".into() }
        })
        .collect()
}

pub struct SesEmail {
    client: aws_sdk_sesv2::Client,
    configuration_set: Option<String>,
    /// The account's maximum send rate (f64 bits; 0 until read).
    max_rate: AtomicU64,
}

/// Retries of one email when SES asks us to slow down, and the wait before the first.
const SES_RETRIES: u32 = 3;
const SES_FIRST_BACKOFF: Duration = Duration::from_millis(1000);
/// How long an email waits when the daily quota is used up or sending is paused on SES's side.
const SES_BUSY_WAIT: Duration = Duration::from_secs(3600);

/// What to do about a failed SendEmail call.
#[derive(Debug, PartialEq, Eq)]
enum SesFailure {
    /// Sending too fast: wait a moment and try the same email again.
    SlowDown,
    /// Daily quota used up, or sending paused or suspended: hold the email for a while.
    Busy,
    /// Anything else (a bad address, an unverified sender): the email fails.
    Fail,
}

fn classify_ses_error(code: Option<&str>, message: &str) -> SesFailure {
    let m = message.to_lowercase();
    // The quota message wins over the code: a used-up daily quota can come back as a throttling error.
    if m.contains("daily message quota") {
        return SesFailure::Busy;
    }
    match code.unwrap_or("") {
        "TooManyRequestsException" | "Throttling" | "ThrottlingException" => SesFailure::SlowDown,
        "LimitExceededException" if m.contains("rate") => SesFailure::SlowDown,
        "LimitExceededException" | "SendingPausedException" | "AccountSuspendedException" => SesFailure::Busy,
        _ if m.contains("maximum sending rate") => SesFailure::SlowDown,
        _ if m.contains("daily message quota") => SesFailure::Busy,
        _ => SesFailure::Fail,
    }
}

impl SesEmail {
    pub async fn new(configuration_set: Option<String>) -> Self {
        let conf = aws_config::load_from_env().await;
        let ses = Self { client: aws_sdk_sesv2::Client::new(&conf), configuration_set, max_rate: AtomicU64::new(0) };
        if let Err(e) = ses.refresh_limits().await {
            tracing::warn!(error = %format!("{e:#}"), "couldn't read the SES sending rate; using SEND_RATE_PER_SEC");
        }
        ses
    }

    fn records_from(domain: &str, tokens: &[String], dkim_status: &str) -> Vec<DnsRecord> {
        let status = match dkim_status {
            "SUCCESS" => "verified",
            "FAILED" | "TEMPORARY_FAILURE" => "failed",
            _ => "pending",
        };
        let mut records: Vec<DnsRecord> = tokens
            .iter()
            .map(|t| DnsRecord {
                kind: "CNAME".into(),
                name: format!("{t}._domainkey.{domain}"),
                value: format!("{t}.dkim.amazonses.com"),
                status: status.into(),
            })
            .collect();
        records.push(DnsRecord {
            kind: "TXT".into(),
            name: format!("_dmarc.{domain}"),
            value: "v=DMARC1; p=none;".into(),
            status: "recommended".into(),
        });
        records
    }
}

#[async_trait]
impl EmailProvider for SesEmail {
    fn name(&self) -> &'static str {
        "ses"
    }

    fn max_send_rate(&self) -> Option<f64> {
        let r = f64::from_bits(self.max_rate.load(Ordering::Relaxed));
        (r > 0.0).then_some(r)
    }

    async fn refresh_limits(&self) -> anyhow::Result<()> {
        let out = self.client.get_account().send().await.context("SES GetAccount failed")?;
        if let Some(q) = out.send_quota() {
            if q.max_send_rate() > 0.0 {
                self.max_rate.store(q.max_send_rate().to_bits(), Ordering::Relaxed);
            }
        }
        Ok(())
    }

    async fn send(&self, email: &OutgoingEmail) -> anyhow::Result<String> {
        use aws_sdk_sesv2::error::ProvideErrorMetadata;
        use aws_sdk_sesv2::primitives::Blob;
        use aws_sdk_sesv2::types::{Destination, EmailContent, RawMessage};
        let raw = build_mime(email, None).into_bytes();
        let mut backoff = SES_FIRST_BACKOFF;
        let mut attempt = 0;
        loop {
            let mut req = self
                .client
                .send_email()
                .from_email_address(format!("{} <{}>", encode_header(&email.from_name), email.from_email))
                .destination(Destination::builder().to_addresses(email.to.clone()).build())
                .content(EmailContent::builder().raw(RawMessage::builder().data(Blob::new(raw.clone())).build()?).build());
            if let Some(cs) = &self.configuration_set {
                req = req.configuration_set_name(cs);
            }
            let err = match req.send().await {
                Ok(out) => return Ok(out.message_id().unwrap_or_default().to_string()),
                Err(e) => e,
            };
            let (code, message) = (err.code().map(str::to_string), err.message().unwrap_or_default().to_string());
            match classify_ses_error(code.as_deref(), &message) {
                SesFailure::SlowDown if attempt < SES_RETRIES => {
                    attempt += 1;
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
                SesFailure::SlowDown => {
                    return Err(ProviderBusy { retry_after: Duration::from_secs(60), reason: format!("SES kept throttling: {message}") }.into());
                }
                SesFailure::Busy => {
                    return Err(ProviderBusy { retry_after: SES_BUSY_WAIT, reason: format!("{}: {message}", code.unwrap_or_default()) }.into());
                }
                SesFailure::Fail => return Err(anyhow::Error::new(err).context("SES SendEmail failed")),
            }
        }
    }

    async fn add_domain(&self, domain: &str) -> anyhow::Result<DomainState> {
        let created = self.client.create_email_identity().email_identity(domain).send().await;
        match created {
            Ok(out) => {
                let tokens = out.dkim_attributes().map(|d| d.tokens().to_vec()).unwrap_or_default();
                Ok(DomainState { status: "pending".into(), records: Self::records_from(domain, &tokens, "PENDING") })
            }
            // Already exists: fall through to reading its state.
            Err(_) => self.domain_state(domain).await,
        }
    }

    async fn set_mail_from(&self, domain: &str, mail_from: &str) -> anyhow::Result<()> {
        use aws_sdk_sesv2::types::BehaviorOnMxFailure;
        self.client
            .put_email_identity_mail_from_attributes()
            .email_identity(domain)
            .mail_from_domain(mail_from)
            .behavior_on_mx_failure(BehaviorOnMxFailure::UseDefaultValue)
            .send()
            .await
            .context("SES PutEmailIdentityMailFromAttributes failed")?;
        Ok(())
    }

    async fn remove_domain(&self, domain: &str) -> anyhow::Result<()> {
        self.client.delete_email_identity().email_identity(domain).send().await.context("SES DeleteEmailIdentity failed")?;
        Ok(())
    }

    async fn domain_state(&self, domain: &str) -> anyhow::Result<DomainState> {
        let out = self
            .client
            .get_email_identity()
            .email_identity(domain)
            .send()
            .await
            .context("SES GetEmailIdentity failed")?;
        let dkim = out.dkim_attributes();
        let tokens = dkim.map(|d| d.tokens().to_vec()).unwrap_or_default();
        let dkim_status = dkim.and_then(|d| d.status()).map(|s| s.as_str().to_string()).unwrap_or_default();
        let status = if out.verified_for_sending_status() {
            "verified"
        } else if dkim_status == "FAILED" {
            "failed"
        } else {
            "pending"
        };
        Ok(DomainState { status: status.into(), records: Self::records_from(domain, &tokens, &dkim_status) })
    }
}

#[cfg(test)]
mod ses_error_tests {
    use super::*;

    #[test]
    fn ses_errors_are_sorted_into_retry_wait_or_fail() {
        assert_eq!(classify_ses_error(Some("TooManyRequestsException"), "Too many requests"), SesFailure::SlowDown);
        assert_eq!(classify_ses_error(Some("LimitExceededException"), "Maximum sending rate exceeded."), SesFailure::SlowDown);
        assert_eq!(classify_ses_error(Some("LimitExceededException"), "Daily message quota exceeded."), SesFailure::Busy);
        assert_eq!(classify_ses_error(Some("Throttling"), "Daily message quota exceeded."), SesFailure::Busy);
        assert_eq!(classify_ses_error(Some("SendingPausedException"), "Sending is paused for this account"), SesFailure::Busy);
        assert_eq!(classify_ses_error(Some("MessageRejected"), "Email address is not verified."), SesFailure::Fail);
        assert_eq!(classify_ses_error(None, "dispatch failure"), SesFailure::Fail);
    }
}
