//! Handing one notification to one device's push service: Web Push (RFC 8030 with VAPID and
//! aes128gcm), Apple Push Notification service (HTTP/2, token auth) and Firebase Cloud Messaging
//! HTTP v1 (OAuth 2 with the service account). Each result is reduced to a [`Sent`] with a short
//! class; vendor response bodies and credentials never reach a stored error or a log line.
//!
//! Where requests go is fixed: web push only to the browsers' own push services (an allowlist of
//! hosts, https only), APNs and FCM only to Apple's and Google's API hosts. `PUSH_API_BASE` (tests)
//! points all three at a stand-in.

use std::collections::HashMap;
use std::time::Duration;

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::crypto;
use super::settings::{Row, ServiceAccount, GOOGLE_TOKEN_URI};
use crate::state::AppState;

/// What happened to one hand-off.
#[derive(Debug, Clone, PartialEq)]
pub enum Sent {
    /// Accepted, with the service's id for it when it gives one.
    Ok(Option<String>),
    /// The device no longer exists (unsubscribed, uninstalled, token expired): remove it.
    Gone(String),
    /// Busy or down: try again later (after this long, when the service said).
    Retry(String, Option<Duration>),
    /// Refused for this device (it counts towards disabling the device).
    Failed(String),
    /// Refused because of the account's credentials: not the device's fault.
    Credentials(String),
}

const TIMEOUT: Duration = Duration::from_secs(15);

/// Web push and FCM: no proxy, no redirects, bounded time.
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .user_agent("dumbmail-push/1")
        .build()
        .expect("http client")
});

/// APNs only speaks HTTP/2.
static APNS: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http2_prior_knowledge()
        .timeout(TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .user_agent("dumbmail-push/1")
        .build()
        .expect("apns client")
});

/// The browsers' push services. A subscription endpoint must be on one of these (https, default port).
const PUSH_HOSTS: &[&str] = &["fcm.googleapis.com", "android.googleapis.com", "updates.push.services.mozilla.com", "web.push.apple.com"];
const PUSH_HOST_SUFFIXES: &[&str] = &[".push.services.mozilla.com", ".notify.windows.com", ".push.apple.com"];

fn test_base(state: &AppState) -> Option<&str> {
    state.cfg.push_api_base.as_deref().map(|b| b.trim_end_matches('/'))
}

/// Whether a browser's subscription endpoint may be sent to (checked when it registers and again
/// before each send).
pub fn endpoint_allowed(state: &AppState, endpoint: &str) -> bool {
    if endpoint.len() > 2000 {
        return false;
    }
    if let Some(base) = test_base(state) {
        if endpoint.starts_with(&format!("{base}/webpush/")) {
            return true;
        }
    }
    let Ok(u) = url::Url::parse(endpoint) else { return false };
    if u.scheme() != "https" || u.port().is_some() || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    let Some(host) = u.host_str().map(str::to_ascii_lowercase) else { return false };
    PUSH_HOSTS.contains(&host.as_str()) || PUSH_HOST_SUFFIXES.iter().any(|s| host.ends_with(s))
}

fn retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    h.get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok()).map(|s| Duration::from_secs(s.min(3600)))
}

fn network(e: &reqwest::Error) -> Sent {
    // Without the URL: endpoints and tokens are secrets of a kind.
    if e.is_timeout() { Sent::Retry("timed out".into(), None) } else { Sent::Retry("couldn't connect".into(), None) }
}

// ---------- Web push ----------

pub struct WebTarget<'a> {
    pub endpoint: &'a str,
    pub p256dh: &'a str,
    pub auth: &'a str,
}

pub struct Options<'a> {
    pub ttl: i64,
    /// very-low, low, normal or high.
    pub urgency: &'a str,
    pub topic: Option<&'a str>,
}

pub async fn send_web(state: &AppState, row: &Row, target: &WebTarget<'_>, payload: &[u8], o: &Options<'_>) -> Sent {
    if !endpoint_allowed(state, target.endpoint) {
        return Sent::Gone("not a browser push service".into());
    }
    let (Some(public), Some(private_enc)) = (row.vapid_public_key.as_deref(), row.vapid_private_enc.as_deref()) else {
        return Sent::Credentials("web push isn't set up".into());
    };
    let Ok(private) = crate::crypto::decrypt(&state.cfg.secret_key, private_enc) else { return Sent::Credentials("the web push key can't be read".into()) };
    let body = match crypto::encrypt(target.p256dh, target.auth, payload) {
        Ok(b) => b,
        Err(_) => return Sent::Failed("the browser's keys are invalid".into()),
    };
    let Ok(u) = url::Url::parse(target.endpoint) else { return Sent::Gone("bad endpoint".into()) };
    let audience = u.origin().ascii_serialization();
    let Ok(authorization) = crypto::vapid_authorization(&private, public, &audience, &super::settings::vapid_subject(state), chrono::Utc::now().timestamp()) else {
        return Sent::Credentials("the web push key can't be read".into());
    };
    let mut req = HTTP
        .post(target.endpoint)
        .header("authorization", authorization)
        .header("ttl", o.ttl.to_string())
        .header("urgency", o.urgency)
        .header("content-encoding", "aes128gcm")
        .header("content-type", "application/octet-stream")
        .body(body);
    if let Some(t) = o.topic {
        req = req.header("topic", t);
    }
    let res = match req.send().await {
        Ok(r) => r,
        Err(e) => return network(&e),
    };
    let status = res.status().as_u16();
    match status {
        200..=299 => Sent::Ok(res.headers().get("location").and_then(|v| v.to_str().ok()).map(|l| l.rsplit('/').next().unwrap_or(l).chars().take(200).collect())),
        404 | 410 => Sent::Gone(format!("web push HTTP {status}")),
        429 | 500..=599 => Sent::Retry(format!("web push HTTP {status}"), retry_after(res.headers())),
        _ => Sent::Failed(format!("web push HTTP {status}")),
    }
}

// ---------- APNs ----------

/// Provider tokens, per key: Apple wants a token reused for 20 to 60 minutes.
static APNS_TOKENS: Lazy<Mutex<HashMap<String, (String, i64)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn cache_key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

async fn apns_token(row: &Row, p8: &str) -> Option<(String, String)> {
    let (key_id, team_id) = (row.apns_key_id.as_deref()?, row.apns_team_id.as_deref()?);
    let key = cache_key(&[key_id, team_id, p8]);
    let now = chrono::Utc::now().timestamp();
    let mut m = APNS_TOKENS.lock().await;
    if let Some((t, at)) = m.get(&key) {
        if now - at < 40 * 60 {
            return Some((key, t.clone()));
        }
    }
    let t = crypto::apns_token(p8, key_id, team_id, now).ok()?;
    m.insert(key.clone(), (t.clone(), now));
    Some((key, t))
}

pub struct ApnsMessage<'a> {
    pub device_token: &'a str,
    pub delivery_id: uuid::Uuid,
    pub payload: &'a Value,
    pub ttl: i64,
    pub high: bool,
    pub collapse_id: Option<&'a str>,
}

pub async fn send_apns(state: &AppState, row: &Row, m: &ApnsMessage<'_>) -> Sent {
    let (Some(enc), Some(topic)) = (row.apns_key_enc.as_deref(), row.apns_bundle_id.as_deref()) else { return Sent::Credentials("APNs isn't set up".into()) };
    let Ok(p8) = crate::crypto::decrypt(&state.cfg.secret_key, enc) else { return Sent::Credentials("the APNs key can't be read".into()) };
    let Some((cache, token)) = apns_token(row, &p8).await else { return Sent::Credentials("the APNs key can't sign".into()) };
    if !m.device_token.chars().all(|c| c.is_ascii_hexdigit()) || m.device_token.len() > 200 {
        return Sent::Gone("APNs BadDeviceToken".into());
    }
    let base = match test_base(state) {
        Some(b) => format!("{b}/apns"),
        None if row.apns_production => "https://api.push.apple.com".into(),
        None => "https://api.sandbox.push.apple.com".into(),
    };
    let expiration = if m.ttl == 0 { 0 } else { chrono::Utc::now().timestamp() + m.ttl };
    let mut req = APNS
        .post(format!("{base}/3/device/{}", m.device_token))
        .header("authorization", format!("bearer {token}"))
        .header("apns-topic", topic)
        .header("apns-push-type", "alert")
        .header("apns-id", m.delivery_id.to_string())
        .header("apns-priority", if m.high { "10" } else { "5" })
        .header("apns-expiration", expiration.to_string())
        .json(m.payload);
    if let Some(c) = m.collapse_id {
        req = req.header("apns-collapse-id", c);
    }
    let res = match req.send().await {
        Ok(r) => r,
        Err(e) => return network(&e),
    };
    let status = res.status().as_u16();
    let id = res.headers().get("apns-id").and_then(|v| v.to_str().ok()).map(|s| s.chars().take(64).collect::<String>());
    let after = retry_after(res.headers());
    if (200..300).contains(&status) {
        return Sent::Ok(id);
    }
    // Only Apple's documented reason names are kept (they're short fixed words).
    let reason = res.json::<Value>().await.ok().and_then(|v| v["reason"].as_str().map(String::from)).filter(|r| r.len() <= 40 && r.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or_default();
    let class = if reason.is_empty() { format!("APNs HTTP {status}") } else { format!("APNs {reason}") };
    match (status, reason.as_str()) {
        (410, _) | (400, "BadDeviceToken" | "DeviceTokenNotForTopic") => Sent::Gone(class),
        (403, "ExpiredProviderToken" | "InvalidProviderToken") | (403, "MissingProviderToken") => {
            APNS_TOKENS.lock().await.remove(&cache);
            Sent::Credentials(class)
        }
        (400, "TopicDisallowed" | "BadTopic") | (403, _) => Sent::Credentials(class),
        (429, _) | (500..=599, _) => Sent::Retry(class, after),
        _ => Sent::Failed(class),
    }
}

// ---------- FCM ----------

static FCM_TOKENS: Lazy<Mutex<HashMap<String, (String, i64)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";

/// An access token for the service account, cached until 5 minutes before it expires.
async fn fcm_token(state: &AppState, sa: &ServiceAccount, raw: &str) -> Result<(String, String), Sent> {
    let token_url = match test_base(state) {
        Some(b) => format!("{b}/oauth2/token"),
        None => GOOGLE_TOKEN_URI.to_string(),
    };
    let key = cache_key(&[raw, &token_url]);
    let now = chrono::Utc::now().timestamp();
    if let Some((t, exp)) = FCM_TOKENS.lock().await.get(&key) {
        if *exp - 300 > now {
            return Ok((key, t.clone()));
        }
    }
    let assertion = crypto::google_assertion(&sa.private_key, &sa.private_key_id, &sa.client_email, FCM_SCOPE, GOOGLE_TOKEN_URI, now).map_err(|_| Sent::Credentials("the service account key can't sign".into()))?;
    let res = HTTP
        .post(&token_url)
        .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", assertion.as_str())])
        .send()
        .await
        .map_err(|e| network(&e))?;
    let status = res.status().as_u16();
    if status == 429 || status >= 500 {
        return Err(Sent::Retry(format!("Google sign-in HTTP {status}"), retry_after(res.headers())));
    }
    if !(200..300).contains(&status) {
        return Err(Sent::Credentials(format!("Google refused the service account (HTTP {status})")));
    }
    let v: Value = res.json().await.map_err(|_| Sent::Retry("Google sign-in answered oddly".into(), None))?;
    let token = v["access_token"].as_str().filter(|t| !t.is_empty() && t.len() < 4096).ok_or_else(|| Sent::Retry("Google sign-in answered oddly".into(), None))?.to_string();
    let exp = now + v["expires_in"].as_i64().unwrap_or(3600).clamp(60, 3600);
    FCM_TOKENS.lock().await.insert(key.clone(), (token.clone(), exp));
    Ok((key, token))
}

pub async fn send_fcm(state: &AppState, row: &Row, message: &Value) -> Sent {
    let (Some(enc), Some(project)) = (row.fcm_credentials_enc.as_deref(), row.fcm_project_id.as_deref()) else { return Sent::Credentials("Firebase isn't set up".into()) };
    let Ok(raw) = crate::crypto::decrypt(&state.cfg.secret_key, enc) else { return Sent::Credentials("the service account can't be read".into()) };
    let Ok(sa) = serde_json::from_str::<ServiceAccount>(&raw) else { return Sent::Credentials("the service account can't be read".into()) };
    let (cache, token) = match fcm_token(state, &sa, &raw).await {
        Ok(t) => t,
        Err(s) => return s,
    };
    let base = test_base(state).map(|b| format!("{b}/fcm")).unwrap_or_else(|| "https://fcm.googleapis.com".into());
    let res = match HTTP.post(format!("{base}/v1/projects/{project}/messages:send")).bearer_auth(&token).json(&json!({"message": message})).send().await {
        Ok(r) => r,
        Err(e) => return network(&e),
    };
    let status = res.status().as_u16();
    let after = retry_after(res.headers());
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Sent::Ok(v["name"].as_str().map(|n| n.rsplit('/').next().unwrap_or(n).chars().take(100).collect()));
    }
    // The FCM error code lives in details[].errorCode; the top-level status is the gRPC one.
    let code = v["error"]["details"]
        .as_array()
        .and_then(|d| d.iter().find_map(|x| x["errorCode"].as_str()))
        .or_else(|| v["error"]["status"].as_str())
        .filter(|c| c.len() <= 40 && c.chars().all(|ch| ch.is_ascii_uppercase() || ch == '_'))
        .unwrap_or("")
        .to_string();
    let class = if code.is_empty() { format!("FCM HTTP {status}") } else { format!("FCM {code}") };
    match (status, code.as_str()) {
        (_, "UNREGISTERED") | (404, _) => Sent::Gone(class),
        (_, "SENDER_ID_MISMATCH") => Sent::Gone(class),
        (401, _) => {
            FCM_TOKENS.lock().await.remove(&cache);
            Sent::Credentials(class)
        }
        (_, "THIRD_PARTY_AUTH_ERROR" | "PERMISSION_DENIED") | (403, _) => Sent::Credentials(class),
        (429, _) | (500..=599, _) => Sent::Retry(class, after),
        _ => Sent::Failed(class),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn push_hosts() {
        let ok = |h: &str| {
            let u = url::Url::parse(h).unwrap();
            let host = u.host_str().unwrap().to_string();
            u.scheme() == "https" && (super::PUSH_HOSTS.contains(&host.as_str()) || super::PUSH_HOST_SUFFIXES.iter().any(|s| host.ends_with(s)))
        };
        assert!(ok("https://fcm.googleapis.com/fcm/send/abc"));
        assert!(ok("https://wns2-par02p.notify.windows.com/w/?token=x"));
        assert!(ok("https://web.push.apple.com/QK"));
        assert!(!ok("https://evilnotify.windows.com.attacker.test/"));
        assert!(!ok("https://169.254.169.254/latest"));
    }
}
