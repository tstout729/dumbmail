//! Calls to Meta's Graph API (the WhatsApp Cloud API and the WhatsApp Business Management API).
//!
//! Every request goes to `graph.facebook.com` (or, in tests, the local stand-in named by
//! `WHATSAPP_API_BASE`), never to an address a customer typed, with the account's own access token
//! as a bearer header. No proxies, no redirects, bounded time and bounded answers. Meta's own error
//! text is never kept: a failure is reduced to its code and a short class of our own.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;

/// The Graph API version DumbMail is built and tested against.
pub const VERSION: &str = "v27.0";
/// Largest answer read from Meta (a page of templates is the biggest).
const MAX_BODY: usize = 2 * 1024 * 1024;

fn base(cfg: &Config) -> String {
    match cfg.whatsapp.whatsapp_api_base.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(b) => format!("{}/{VERSION}", b.trim_end_matches('/')),
        None => format!("https://graph.facebook.com/{VERSION}"),
    }
}

fn client(cfg: &Config) -> &'static reqwest::Client {
    static PROD: OnceLock<reqwest::Client> = OnceLock::new();
    static LOOPBACK: OnceLock<reqwest::Client> = OnceLock::new();
    let build = |https_only: bool| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .https_only(https_only)
            .user_agent("dumbmail/0.1")
            .build()
            .expect("http client")
    };
    if cfg.whatsapp.whatsapp_api_base.is_some() { LOOPBACK.get_or_init(|| build(false)) } else { PROD.get_or_init(|| build(true)) }
}

/// Ids Meta gives (WhatsApp Business Account, phone number, template) are digits.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_digit())
}

/// What a failed call comes down to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphError {
    /// Meta's error code (0 when there was no readable answer).
    pub code: i64,
    pub subcode: Option<i64>,
    /// The HTTP status (0 when the request never got an answer).
    pub status: u16,
    /// The request may have reached Meta and we don't know what happened (time-out, cut-off answer).
    pub unclear: bool,
}

/// How DumbMail treats an error code. See Meta's error code reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// 131026: not a WhatsApp number (or one that can't get messages). Don't retry.
    NotOnWhatsApp,
    /// 131050: the person stopped marketing messages from this business on WhatsApp.
    MarketingOptOut,
    /// 130429, 131056, 80007, 80008, 4: throughput, pair rate, account or template API rate limits.
    /// Back off and retry.
    RateLimited,
    /// 132015, 132016: the template was paused or disabled for quality.
    TemplatePaused,
    /// 132001: the template doesn't exist in that language or isn't approved.
    TemplateMissing,
    /// 131047: more than 24 hours since they last wrote, so only a template can go.
    WindowClosed,
    /// 131049: Meta held it back to protect engagement. Don't retry soon.
    Ecosystem,
    /// 190, 0, 3, 10, 200-299, 131005: the token or its permissions. Reconnect.
    Auth,
    /// 131048, 368, 131031, 130497, 131042: the account or number is restricted.
    Restricted,
    /// 1, 2, 131000, 131016, 133004, 131057 and HTTP 5xx: Meta's side. Retry later.
    Transient,
    /// Anything else: the request itself was refused (bad parameters and so on).
    Refused,
    /// No answer we could read; the message may or may not have gone.
    Unclear,
}

impl GraphError {
    pub fn class(&self) -> Class {
        if self.unclear {
            return Class::Unclear;
        }
        // No code: no readable error object (code 0 from Meta itself is an auth problem, sent with a 401).
        if self.code == 0 {
            return match self.status {
                401 | 403 => Class::Auth,
                0 | 500.. => Class::Transient,
                _ => Class::Refused,
            };
        }
        match self.code {
            131026 => Class::NotOnWhatsApp,
            131050 => Class::MarketingOptOut,
            130429 | 131056 | 80007 | 80008 | 4 => Class::RateLimited,
            132015 | 132016 => Class::TemplatePaused,
            132001 => Class::TemplateMissing,
            131047 => Class::WindowClosed,
            131049 => Class::Ecosystem,
            3 | 10 | 190 | 131005 => Class::Auth,
            200..=299 => Class::Auth,
            131048 | 368 | 131031 | 130497 | 131042 => Class::Restricted,
            1 | 2 | 131000 | 131016 | 133004 | 131057 => Class::Transient,
            _ if self.status >= 500 || self.status == 0 => Class::Transient,
            _ => Class::Refused,
        }
    }

    /// A short description for people (stored on messages and settings; never Meta's own text).
    pub fn describe(&self) -> String {
        let what = match self.class() {
            Class::NotOnWhatsApp => "this number can't get WhatsApp messages",
            Class::MarketingOptOut => "they turned off marketing messages from you on WhatsApp",
            Class::RateLimited => "Meta's sending limit was reached",
            Class::TemplatePaused => "Meta paused or disabled this template",
            Class::TemplateMissing => "the template isn't approved in that language",
            Class::WindowClosed => "more than 24 hours since they last wrote, so only a template can go",
            Class::Ecosystem => "Meta held it back to limit marketing messages to this person",
            Class::Auth => "the access token expired or lost its permissions; reconnect WhatsApp",
            Class::Restricted => "Meta restricted this WhatsApp account or number",
            Class::Transient => "Meta couldn't take it right now",
            Class::Refused => "Meta refused the request",
            Class::Unclear => "no answer from Meta; not sent again",
        };
        if self.code > 0 {
            format!("{what} (WhatsApp error {})", self.code)
        } else if self.status > 0 {
            format!("{what} (HTTP {})", self.status)
        } else {
            what.to_string()
        }
    }
}

pub type GResult<T> = Result<T, GraphError>;

fn net_error(e: &reqwest::Error, sent: bool) -> GraphError {
    // A connection that never opened is a certain refusal; a time-out after sending isn't.
    let unclear = sent && !e.is_connect();
    GraphError { code: 0, subcode: None, status: 0, unclear }
}

async fn read_capped(res: reqwest::Response) -> Result<Vec<u8>, ()> {
    let mut res = res;
    let mut out = Vec::new();
    loop {
        match res.chunk().await {
            Ok(Some(c)) => {
                if out.len() + c.len() > MAX_BODY {
                    return Err(());
                }
                out.extend_from_slice(&c);
            }
            Ok(None) => return Ok(out),
            Err(_) => return Err(()),
        }
    }
}

/// One request. `write`: whether it changes something at Meta (then a lost answer is "unclear").
async fn call(cfg: &Config, method: reqwest::Method, path: &str, query: &[(&str, &str)], token: &str, body: Option<&Value>, write: bool) -> GResult<Value> {
    let url = format!("{}/{}", base(cfg), path.trim_start_matches('/'));
    let mut req = client(cfg).request(method, &url).bearer_auth(token).query(query);
    if let Some(b) = body {
        req = req.json(b);
    }
    let res = match req.send().await {
        Ok(r) => r,
        Err(e) => return Err(net_error(&e, write)),
    };
    let status = res.status().as_u16();
    let bytes = match read_capped(res).await {
        Ok(b) => b,
        Err(()) => return Err(GraphError { code: 0, subcode: None, status, unclear: write && status < 300 }),
    };
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        if v.is_null() {
            return Err(GraphError { code: 0, subcode: None, status, unclear: write });
        }
        return Ok(v);
    }
    let e = &v["error"];
    let code = e["code"].as_i64().unwrap_or(0);
    // A change that got a gateway error (502, 503 ...) or no definite Meta error code may have been
    // made anyway: it's never repeated.
    let unclear = write && (status >= 500 || code == 0);
    Err(GraphError { code, subcode: e["error_subcode"].as_i64(), status, unclear })
}

/// The phone number's name, quality and messaging limit (Check connection).
pub async fn phone_number(cfg: &Config, token: &str, phone_number_id: &str) -> GResult<Value> {
    call(
        cfg,
        reqwest::Method::GET,
        phone_number_id,
        &[("fields", "display_phone_number,verified_name,quality_rating,whatsapp_business_manager_messaging_limit,code_verification_status,status")],
        token,
        None,
        false,
    )
    .await
}

/// One page of the account's templates (`after`: the cursor from the last page).
pub async fn templates_page(cfg: &Config, token: &str, waba_id: &str, after: Option<&str>) -> GResult<Value> {
    let mut q = vec![("fields", "id,name,language,status,category,components,parameter_format,quality_score,rejected_reason"), ("limit", "100")];
    if let Some(a) = after {
        q.push(("after", a));
    }
    call(cfg, reqwest::Method::GET, &format!("{waba_id}/message_templates"), &q, token, None, false).await
}

/// Submit a template for Meta's review. Answers `{id, status, category}`.
pub async fn create_template(cfg: &Config, token: &str, waba_id: &str, body: &Value) -> GResult<Value> {
    call(cfg, reqwest::Method::POST, &format!("{waba_id}/message_templates"), &[], token, Some(body), true).await
}

/// What a successful send gave back.
pub struct Accepted {
    pub wamid: String,
    pub wa_id: Option<String>,
    /// Meta's pacing answer: accepted, held_for_quality_assessment or paused.
    pub pacing: Option<String>,
}

/// Send one message. `payload` is the message without `messaging_product` and the recipient.
pub async fn send_message(cfg: &Config, token: &str, phone_number_id: &str, to: &str, payload: &Value) -> GResult<Accepted> {
    let mut body = payload.clone();
    if let Some(o) = body.as_object_mut() {
        o.insert("messaging_product".into(), json!("whatsapp"));
        o.insert("recipient_type".into(), json!("individual"));
        o.insert("to".into(), json!(to));
    }
    let v = call(cfg, reqwest::Method::POST, &format!("{phone_number_id}/messages"), &[], token, Some(&body), true).await?;
    match v["messages"][0]["id"].as_str().filter(|s| !s.is_empty() && s.len() <= 200) {
        Some(id) => Ok(Accepted {
            wamid: id.to_string(),
            wa_id: v["contacts"][0]["wa_id"].as_str().map(|s| s.chars().take(40).collect()),
            pacing: v["messages"][0]["message_status"].as_str().map(|s| s.chars().take(40).collect()),
        }),
        None => Err(GraphError { code: 0, subcode: None, status: 200, unclear: true }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes() {
        let e = |code| GraphError { code, subcode: None, status: 400, unclear: false };
        assert_eq!(e(131026).class(), Class::NotOnWhatsApp);
        assert_eq!(e(131050).class(), Class::MarketingOptOut);
        assert_eq!(e(130429).class(), Class::RateLimited);
        assert_eq!(e(131056).class(), Class::RateLimited);
        assert_eq!(e(132015).class(), Class::TemplatePaused);
        assert_eq!(e(190).class(), Class::Auth);
        assert_eq!(e(100).class(), Class::Refused);
        assert_eq!(GraphError { code: 0, subcode: None, status: 503, unclear: false }.class(), Class::Transient);
        assert_eq!(e(80008).class(), Class::RateLimited);
        assert_eq!(GraphError { code: 0, subcode: None, status: 0, unclear: true }.class(), Class::Unclear);
        assert!(e(131026).describe().contains("131026"));
        assert!(valid_id("106540352242922"));
        assert!(!valid_id("../x"));
    }
}
