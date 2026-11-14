use std::sync::Mutex;

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct OutgoingSms {
    pub message_id: uuid::Uuid,
    pub to: String,
    pub from: String,
    pub body: String,
    pub media_url: Option<String>,
}

#[async_trait]
pub trait SmsProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn send(&self, sms: &OutgoingSms) -> anyhow::Result<String>;
    fn sent(&self) -> Vec<OutgoingSms> {
        Vec::new()
    }
}

pub struct LogSms {
    sent: Mutex<Vec<OutgoingSms>>,
}

impl Default for LogSms {
    fn default() -> Self {
        Self::new()
    }
}

impl LogSms {
    pub fn new() -> Self {
        Self { sent: Mutex::new(Vec::new()) }
    }
}

#[async_trait]
impl SmsProvider for LogSms {
    fn name(&self) -> &'static str {
        "log"
    }
    async fn send(&self, sms: &OutgoingSms) -> anyhow::Result<String> {
        tracing::info!(to = %sms.to, body = %sms.body, "sms (log provider, not sent)");
        let mut sent = self.sent.lock().map_err(|_| anyhow!("poisoned"))?;
        if sent.len() > 500 {
            sent.remove(0);
        }
        sent.push(sms.clone());
        Ok(format!("log-{}", sms.message_id))
    }
    fn sent(&self) -> Vec<OutgoingSms> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Telnyx Messaging API v2 — about half Twilio's per-message price in the US.
pub struct TelnyxSms {
    http: reqwest::Client,
    api_key: String,
    messaging_profile_id: Option<String>,
}

impl TelnyxSms {
    pub fn new(http: reqwest::Client, api_key: String, messaging_profile_id: Option<String>) -> Self {
        Self { http, api_key, messaging_profile_id }
    }
}

#[async_trait]
impl SmsProvider for TelnyxSms {
    fn name(&self) -> &'static str {
        "telnyx"
    }
    async fn send(&self, sms: &OutgoingSms) -> anyhow::Result<String> {
        let mut body = json!({ "from": sms.from, "to": sms.to, "text": sms.body });
        if let Some(p) = &self.messaging_profile_id {
            body["messaging_profile_id"] = json!(p);
        }
        if let Some(m) = &sms.media_url {
            body["media_urls"] = json!([m]);
        }
        let res = self
            .http
            .post("https://api.telnyx.com/v2/messages")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("Telnyx request failed")?;
        let status = res.status();
        let v: serde_json::Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Telnyx {status}: {}", v["errors"][0]["detail"].as_str().unwrap_or("unknown error"));
        }
        Ok(v["data"]["id"].as_str().unwrap_or_default().to_string())
    }
}

pub struct TwilioSms {
    http: reqwest::Client,
    account_sid: String,
    auth_token: String,
}

impl TwilioSms {
    pub fn new(http: reqwest::Client, account_sid: String, auth_token: String) -> Self {
        Self { http, account_sid, auth_token }
    }
}

#[async_trait]
impl SmsProvider for TwilioSms {
    fn name(&self) -> &'static str {
        "twilio"
    }
    async fn send(&self, sms: &OutgoingSms) -> anyhow::Result<String> {
        let url = format!("https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json", self.account_sid);
        let mut form = vec![("To", sms.to.clone()), ("From", sms.from.clone()), ("Body", sms.body.clone())];
        if let Some(m) = &sms.media_url {
            form.push(("MediaUrl", m.clone()));
        }
        let res = self
            .http
            .post(url)
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .form(&form)
            .send()
            .await
            .context("Twilio request failed")?;
        let status = res.status();
        let v: serde_json::Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Twilio {status}: {}", v["message"].as_str().unwrap_or("unknown error"));
        }
        Ok(v["sid"].as_str().unwrap_or_default().to_string())
    }
}

/// GSM-7 vs UCS-2 segment counting so the UI and MCP can show cost before sending.
pub fn segment_count(body: &str) -> (usize, usize) {
    const GSM: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿abcdefghijklmnopqrstuvwxyzäöñüà";
    const GSM_EXT: &str = "^{}\\[~]|€\x0c";
    let mut gsm_len = 0usize;
    let mut is_gsm = true;
    for c in body.chars() {
        if GSM.contains(c) {
            gsm_len += 1;
        } else if GSM_EXT.contains(c) {
            gsm_len += 2;
        } else {
            is_gsm = false;
            break;
        }
    }
    if is_gsm {
        let segs = if gsm_len <= 160 { 1 } else { gsm_len.div_ceil(153) };
        (gsm_len, segs.max(1))
    } else {
        let units: usize = body.encode_utf16().count();
        let segs = if units <= 70 { 1 } else { units.div_ceil(67) };
        (units, segs.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::segment_count;

    #[test]
    fn counts_segments() {
        assert_eq!(segment_count("hello"), (5, 1));
        assert_eq!(segment_count(&"a".repeat(160)).1, 1);
        assert_eq!(segment_count(&"a".repeat(161)).1, 2);
        assert_eq!(segment_count("hi 👋").1, 1);
        assert_eq!(segment_count(&"é👋".repeat(40)).1, 2);
    }
}
