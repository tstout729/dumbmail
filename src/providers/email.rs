use std::sync::Mutex;

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

#[async_trait]
pub trait EmailProvider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Sends the message and returns the provider's message id.
    async fn send(&self, email: &OutgoingEmail) -> anyhow::Result<String>;
    async fn add_domain(&self, domain: &str) -> anyhow::Result<DomainState>;
    async fn domain_state(&self, domain: &str) -> anyhow::Result<DomainState>;
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
    out.push_str(&format!("X-Postlark-Message: {}\r\n", email.message_id));
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
        Ok(DomainState { status: "verified".into(), records: fake_records(domain, "verified") })
    }
    async fn domain_state(&self, domain: &str) -> anyhow::Result<DomainState> {
        self.add_domain(domain).await
    }
    fn sent(&self) -> Vec<OutgoingEmail> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

fn fake_records(domain: &str, status: &str) -> Vec<DnsRecord> {
    vec![DnsRecord {
        kind: "TXT".into(),
        name: format!("_dmarc.{domain}"),
        value: "v=DMARC1; p=none;".into(),
        status: status.into(),
    }]
}

pub struct SesEmail {
    client: aws_sdk_sesv2::Client,
    configuration_set: Option<String>,
}

impl SesEmail {
    pub async fn new(configuration_set: Option<String>) -> Self {
        let conf = aws_config::load_from_env().await;
        Self { client: aws_sdk_sesv2::Client::new(&conf), configuration_set }
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

    async fn send(&self, email: &OutgoingEmail) -> anyhow::Result<String> {
        use aws_sdk_sesv2::primitives::Blob;
        use aws_sdk_sesv2::types::{Destination, EmailContent, RawMessage};
        let raw = build_mime(email, None);
        let mut req = self
            .client
            .send_email()
            .from_email_address(format!("{} <{}>", encode_header(&email.from_name), email.from_email))
            .destination(Destination::builder().to_addresses(email.to.clone()).build())
            .content(
                EmailContent::builder()
                    .raw(RawMessage::builder().data(Blob::new(raw.into_bytes())).build()?)
                    .build(),
            );
        if let Some(cs) = &self.configuration_set {
            req = req.configuration_set_name(cs);
        }
        let out = req.send().await.context("SES SendEmail failed")?;
        Ok(out.message_id().unwrap_or_default().to_string())
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
