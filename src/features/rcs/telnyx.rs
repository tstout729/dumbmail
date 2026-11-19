//! Calls to Telnyx's RCS API, as Telnyx documents it (developers.telnyx.com, "RCS"):
//!
//! - `GET /v2/messaging/rcs/agents/{id}`: the agent (`agent_id`, `agent_name`, `enabled`,
//!   `profile_id`, `webhook_url`).
//! - `GET /v2/messaging/rcs/capabilities/{agent_id}/{phone_number}` and
//!   `POST /v2/messaging/rcs/bulk_capabilities` (`{agent_id, phone_numbers}`, up to 100): the
//!   features a number supports; `features` empty or null means no RCS.
//! - `POST /v2/messages/rcs` (`agent_id`, `to`, `messaging_profile_id`, `agent_message`, `type`,
//!   `webhook_url`), with an `Idempotency-Key`: answers `data.id` and `data.type`.
//!
//! Every request goes to `api.telnyx.com` (or, in tests, a loopback stand-in), never to an address
//! a customer typed. No proxies, no redirects, bounded time and bounded answers. Telnyx's own error
//! text is never kept: a failure comes down to its HTTP status and Telnyx's error code.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;

/// Largest answer read from Telnyx.
const MAX_BODY: usize = 512 * 1024;
/// Most numbers one bulk capability check takes (Telnyx's limit).
pub const BULK_MAX: usize = 100;

/// A loopback stand-in for tests (`RcsConfig::api_base`), or None.
fn loopback_base(cfg: &Config) -> Option<String> {
    let b = cfg.rcs.api_base.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    let loopback = b.starts_with("http://127.0.0.1:") || b.starts_with("http://localhost:");
    loopback.then(|| b.trim_end_matches('/').to_string())
}

fn base(cfg: &Config) -> String {
    loopback_base(cfg).unwrap_or_else(|| "https://api.telnyx.com".into()) + "/v2"
}

/// Whether requests go to a test stand-in.
pub fn stand_in(cfg: &Config) -> bool {
    loopback_base(cfg).is_some()
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
    if stand_in(cfg) { LOOPBACK.get_or_init(|| build(false)) } else { PROD.get_or_init(|| build(true)) }
}

/// What a failed call comes down to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallError {
    /// The HTTP status (0 when no answer came).
    pub status: u16,
    /// Telnyx's error code (`errors[0].code`), when it gave one.
    pub code: Option<String>,
    /// The request may have reached Telnyx and we can't tell what it did (a time-out, a cut-off or
    /// unreadable answer, a gateway error, an error without a code, or "still being processed").
    pub unclear: bool,
}

impl CallError {
    /// A short description for people (never Telnyx's own text).
    pub fn describe(&self) -> String {
        let what = match self.status {
            0 if self.unclear => "no answer from Telnyx",
            0 => "couldn't reach Telnyx",
            401 | 403 => "Telnyx refused the API key",
            404 => "Telnyx doesn't know this agent",
            429 => "Telnyx's rate limit was reached",
            _ if self.unclear => "Telnyx didn't confirm it",
            400..=499 => "Telnyx refused the request",
            _ => "Telnyx couldn't take it right now",
        };
        match (&self.code, self.status) {
            (Some(c), _) => format!("{what} (Telnyx error {c})"),
            (None, s) if s > 0 => format!("{what} (HTTP {s})"),
            _ => what.to_string(),
        }
    }
}

pub type TResult<T> = Result<T, CallError>;

async fn read_capped(mut res: reqwest::Response) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        match res.chunk().await {
            Ok(Some(c)) => {
                if out.len() + c.len() > MAX_BODY {
                    return None;
                }
                out.extend_from_slice(&c);
            }
            Ok(None) => return Some(out),
            Err(_) => return None,
        }
    }
}

/// Telnyx error codes are digits; anything else isn't kept.
fn error_code(v: &Value) -> Option<String> {
    let c = &v["errors"][0]["code"];
    let s = c.as_str().map(str::to_string).or_else(|| c.as_i64().map(|n| n.to_string()))?;
    (!s.is_empty() && s.len() <= 10 && s.bytes().all(|b| b.is_ascii_digit())).then_some(s)
}

/// One request. `write`: it sends something (then a lost answer is unclear, never "not sent").
async fn call(cfg: &Config, key: &str, method: reqwest::Method, path: &str, body: Option<&Value>, idempotency: Option<&str>, write: bool, timeout: Duration) -> TResult<Value> {
    let url = format!("{}/{}", base(cfg), path.trim_start_matches('/'));
    let mut req = client(cfg).request(method, &url).bearer_auth(key).timeout(timeout);
    if let Some(b) = body {
        req = req.json(b);
    }
    if let Some(k) = idempotency {
        req = req.header("Idempotency-Key", k);
    }
    let res = match req.send().await {
        Ok(r) => r,
        // A connection that never opened is a certain refusal; anything later isn't.
        Err(e) => return Err(CallError { status: 0, code: None, unclear: write && !e.is_connect() }),
    };
    let status = res.status().as_u16();
    let Some(bytes) = read_capped(res).await else {
        return Err(CallError { status, code: None, unclear: write });
    };
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        if !v["data"].is_object() && !v["data"].is_array() {
            return Err(CallError { status, code: None, unclear: write });
        }
        return Ok(v);
    }
    let code = error_code(&v);
    // A send answered with a gateway error, no definite error code, or "a request with this key is
    // still being processed" (409, 10036) may have gone out: it's never repeated.
    let unclear = write && (status >= 500 || code.is_none() || status == 409);
    Err(CallError { status, code, unclear })
}

/// Agent ids are Telnyx's own short ids: letters, digits, `_` and `-`.
pub fn valid_agent_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The agent as Telnyx describes it.
pub async fn agent(cfg: &Config, key: &str, agent_id: &str) -> TResult<Value> {
    let v = call(cfg, key, reqwest::Method::GET, &format!("messaging/rcs/agents/{agent_id}"), None, None, false, Duration::from_secs(15)).await?;
    Ok(v["data"].clone())
}

/// A number's RCS features for the agent (empty: it can't get RCS from it).
fn features(d: &Value) -> Vec<String> {
    d["features"].as_array().map(|a| a.iter().filter_map(|f| f.as_str()).filter(|f| f.len() <= 60).take(30).map(str::to_string).collect()).unwrap_or_default()
}

/// One number's capabilities. Telnyx says these lookups can take several seconds; `timeout` bounds it.
pub async fn capabilities(cfg: &Config, key: &str, agent_id: &str, phone: &str, timeout: Duration) -> TResult<Vec<String>> {
    let v = call(cfg, key, reqwest::Method::GET, &format!("messaging/rcs/capabilities/{agent_id}/{}", urlencode(phone)), None, None, false, timeout).await?;
    Ok(features(&v["data"]))
}

/// Up to [`BULK_MAX`] numbers at once: (number, features) for each number Telnyx answered for.
pub async fn bulk_capabilities(cfg: &Config, key: &str, agent_id: &str, phones: &[String], timeout: Duration) -> TResult<Vec<(String, Vec<String>)>> {
    let phones = &phones[..phones.len().min(BULK_MAX)];
    let body = json!({"agent_id": agent_id, "phone_numbers": phones});
    let v = call(cfg, key, reqwest::Method::POST, "messaging/rcs/bulk_capabilities", Some(&body), None, false, timeout).await?;
    Ok(v["data"]
        .as_array()
        .map(|a| a.iter().filter_map(|d| Some((crate::models::normalize_phone(d["phone_number"].as_str()?)?, features(d)))).filter(|(p, _)| phones.contains(p)).collect())
        .unwrap_or_default())
}

/// What Telnyx said when it took a message.
#[derive(Debug, Clone)]
pub struct Accepted {
    pub id: String,
    /// `RCS`, or `SMS` if Telnyx itself sent it as a text.
    pub kind: String,
}

/// Send one RCS message, keyed by our message id so a repeated request can't send it twice.
pub async fn send(cfg: &Config, key: &str, body: &Value, idempotency_key: &str, timeout: Duration) -> TResult<Accepted> {
    let v = call(cfg, key, reqwest::Method::POST, "messages/rcs", Some(body), Some(idempotency_key), true, timeout).await?;
    let id = v["data"]["id"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 100 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    match id {
        Some(id) => Ok(Accepted { id: id.to_string(), kind: v["data"]["type"].as_str().unwrap_or("RCS").chars().take(10).collect::<String>().to_ascii_uppercase() }),
        None => Err(CallError { status: 200, code: None, unclear: true }),
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_and_ids() {
        assert!(valid_agent_id("Agent007"));
        assert!(valid_agent_id("e4448a5c0670c2a9"));
        assert!(!valid_agent_id("../x"));
        assert!(!valid_agent_id(""));
        assert_eq!(error_code(&json!({"errors": [{"code": "10016", "title": "x"}]})).as_deref(), Some("10016"));
        assert_eq!(error_code(&json!({"errors": [{"code": "<script>"}]})), None);
        let e = CallError { status: 422, code: Some("40001".into()), unclear: false };
        assert_eq!(e.describe(), "Telnyx refused the request (Telnyx error 40001)");
        assert_eq!(CallError { status: 0, code: None, unclear: true }.describe(), "no answer from Telnyx");
    }
}
