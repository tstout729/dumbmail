//! Handing one notification to one device's push service: Web Push (RFC 8030 with VAPID and
//! aes128gcm), Apple Push Notification service (HTTP/2, token auth) and Firebase Cloud Messaging
//! HTTP v1 (OAuth 2 with the service account). Each result is reduced to a [`Sent`] with a short
//! class; vendor response bodies and credentials never reach a stored error or a log line.
//!
//! Where requests go is fixed: web push only to the browsers' own push services (an allowlist of
//! hosts, https only), APNs and FCM only to Apple's and Google's API hosts. `PUSH_API_BASE` (tests)
//! points all three at a stand-in.
//!
//! A request that may have reached the service but got no answer (a timeout, a dropped connection)
//! is [`Sent::Unknown`]: it's never sent again. Only a refusal to connect, or an explicit 429 or
//! 5xx, is [`Sent::Retry`].

use std::collections::HashMap;
use std::time::Duration;

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::crypto;
use super::settings::{Row, ServiceAccount, GOOGLE_TOKEN_URI};
use crate::state::AppState;

/// What happened to one hand-off.
#[derive(Debug, Clone, PartialEq)]
pub enum Sent {
    /// Accepted, with the service's id for it when it gives one.
    Ok(Option<String>),
    /// The service says this device registration no longer exists: remove it. For APNs, the time
    /// (ms since the epoch) it stopped being valid, when Apple says.
    Gone(String, Option<i64>),
    /// The service certainly didn't take it (refused to connect, or said 429/5xx): try again later
    /// (after this long, when the service said).
    Retry(String, Option<Duration>),
    /// Sent, but no answer came back: it may have arrived. Never sent again.
    Unknown(String),
    /// Refused for this device (it counts towards turning the device off).
    Failed(String),
    /// Refused because of this message (too large, a bad field): not the device's fault.
    Rejected(String),
    /// Refused because of the account's setup (credentials, project, topic, environment). Nothing
    /// is deleted; the platform is marked as needing attention in Settings.
    Account(String),
}

const TIMEOUT: Duration = Duration::from_secs(15);
/// Most of a response body read (anything longer is cut off before decoding).
pub const MAX_RESPONSE: usize = 64 * 1024;

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .user_agent("dumbmail-push/1")
}

/// Web push and FCM: no proxy, no redirects, bounded time.
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| builder().build().expect("http client"));

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

/// A request error. Only a failure to connect means nothing was sent; anything after that (a
/// timeout, a reset while waiting for the answer) may have reached the service.
fn network(e: &reqwest::Error) -> Sent {
    // Without the URL: endpoints and tokens are secrets of a kind.
    if e.is_connect() {
        Sent::Retry("couldn't connect".into(), None)
    } else if e.is_timeout() {
        Sent::Unknown("no answer in time".into())
    } else {
        Sent::Unknown("the connection dropped before an answer".into())
    }
}

/// Read at most [`MAX_RESPONSE`] bytes of a response body.
async fn read_capped(mut res: reqwest::Response) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = res.chunk().await {
        let room = MAX_RESPONSE.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= MAX_RESPONSE {
            break;
        }
    }
    out
}

async fn json_capped(res: reqwest::Response) -> Value {
    serde_json::from_slice(&read_capped(res).await).unwrap_or(Value::Null)
}

fn cache_key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

/// Keep a cache under `max` entries: drop what `stale` says first, then anything.
fn bound<V>(m: &mut HashMap<String, V>, max: usize, stale: impl Fn(&V) -> bool) {
    if m.len() < max {
        return;
    }
    m.retain(|_, v| !stale(v));
    while m.len() >= max {
        let Some(k) = m.keys().next().cloned() else { break };
        m.remove(&k);
    }
}

/// Forget an account's provider connections and tokens (its credentials changed).
pub async fn forget_account(account_id: Uuid) {
    let prefix = format!("{account_id}:");
    APNS_POOLS.lock().await.retain(|k, _| !k.starts_with(&prefix));
    APNS_TOKENS.lock().await.retain(|k, _| !k.starts_with(&prefix));
    FCM_TOKENS.lock().await.retain(|k, _| !k.starts_with(&prefix));
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
        return Sent::Gone("not a browser push service".into(), None);
    }
    let (Some(public), Some(private_enc)) = (row.vapid_public_key.as_deref(), row.vapid_private_enc.as_deref()) else {
        return Sent::Account("web push isn't set up".into());
    };
    let Ok(private) = crate::crypto::decrypt(&state.cfg.secret_key, private_enc) else { return Sent::Account("the web push key can't be read".into()) };
    let body = match crypto::encrypt(target.p256dh, target.auth, payload) {
        Ok(b) => b,
        Err(_) => return Sent::Failed("the browser's keys are invalid".into()),
    };
    let Ok(u) = url::Url::parse(target.endpoint) else { return Sent::Gone("bad endpoint".into(), None) };
    let audience = u.origin().ascii_serialization();
    let Ok(authorization) = crypto::vapid_authorization(&private, public, &audience, &super::settings::vapid_subject(state), chrono::Utc::now().timestamp()) else {
        return Sent::Account("the web push key can't be read".into());
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
    let id = res.headers().get("location").and_then(|v| v.to_str().ok()).map(|l| l.rsplit('/').next().unwrap_or(l).chars().take(200).collect());
    let after = retry_after(res.headers());
    let _ = read_capped(res).await;
    match status {
        200..=299 => Sent::Ok(id),
        // RFC 8030: the subscription expired or was removed.
        404 | 410 => Sent::Gone(format!("web push HTTP {status}"), None),
        413 => Sent::Rejected("web push HTTP 413 (too large)".into()),
        429 | 500..=599 => Sent::Retry(format!("web push HTTP {status}"), after),
        _ => Sent::Failed(format!("web push HTTP {status}")),
    }
}

// ---------- APNs ----------

/// Provider tokens per account, credential version, team and key: Apple wants one reused for 20 to 60 minutes.
static APNS_TOKENS: Lazy<Mutex<HashMap<String, (String, i64)>>> = Lazy::new(|| Mutex::new(HashMap::new()));
/// HTTP/2 connections per account, credential version, team, key and environment. Apple binds a
/// connection to the team whose key signs its first request, so teams never share one.
static APNS_POOLS: Lazy<Mutex<HashMap<String, reqwest::Client>>> = Lazy::new(|| Mutex::new(HashMap::new()));
const MAX_CACHED: usize = 1000;

/// An APNs environment: "production" or "sandbox".
pub fn apns_environment(row: &Row, device: Option<&str>) -> &'static str {
    match device {
        Some("sandbox") => "sandbox",
        Some("production") => "production",
        _ if row.apns_production => "production",
        _ => "sandbox",
    }
}

fn apns_key(row: &Row, env: &str) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        row.account_id,
        row.credentials_version,
        row.apns_team_id.as_deref().unwrap_or(""),
        row.apns_key_id.as_deref().unwrap_or(""),
        env
    )
}

async fn apns_client(row: &Row, env: &str) -> Option<reqwest::Client> {
    let key = apns_key(row, env);
    let mut m = APNS_POOLS.lock().await;
    if let Some(c) = m.get(&key) {
        return Some(c.clone());
    }
    bound(&mut m, MAX_CACHED, |_| false);
    let c = builder().http2_prior_knowledge().build().ok()?;
    m.insert(key, c.clone());
    Some(c)
}

/// APNs connection pools held for an account (tests).
#[doc(hidden)]
pub async fn apns_pools_for(account_id: Uuid) -> usize {
    let prefix = format!("{account_id}:");
    APNS_POOLS.lock().await.keys().filter(|k| k.starts_with(&prefix)).count()
}

async fn apns_token(row: &Row, p8: &str, env: &str) -> Option<(String, String)> {
    let (key_id, team_id) = (row.apns_key_id.as_deref()?, row.apns_team_id.as_deref()?);
    let key = apns_key(row, env);
    let now = chrono::Utc::now().timestamp();
    let mut m = APNS_TOKENS.lock().await;
    if let Some((t, at)) = m.get(&key) {
        if now - at < 40 * 60 {
            return Some((key, t.clone()));
        }
    }
    bound(&mut m, MAX_CACHED, |(_, at)| now - at >= 40 * 60);
    let t = crypto::apns_token(p8, key_id, team_id, now).ok()?;
    m.insert(key.clone(), (t.clone(), now));
    Some((key, t))
}

pub struct ApnsMessage<'a> {
    pub device_token: &'a str,
    pub delivery_id: Uuid,
    pub payload: &'a Value,
    pub ttl: i64,
    pub high: bool,
    pub collapse_id: Option<&'a str>,
}

/// What a provider call needs, got before the final claim so nothing waits between the claim and the call.
pub enum Creds {
    Web,
    Apns { client: reqwest::Client, token: String, cache: String, base: String, topic: String, env: &'static str },
    Fcm { token: String, cache: String, url: String },
}

/// Get a platform's credentials (a provider token; for Firebase, possibly a sign-in) for a device.
/// An error is the outcome to record without calling the provider.
pub async fn prepare(state: &AppState, row: &Row, kind: &str, apns_env: Option<&str>) -> Result<Creds, Sent> {
    match kind {
        "web" => Ok(Creds::Web),
        "ios" => {
            let env = apns_environment(row, apns_env);
            let (Some(enc), Some(topic)) = (row.apns_key_enc.as_deref(), row.apns_bundle_id.as_deref()) else { return Err(Sent::Account("APNs isn't set up".into())) };
            let Ok(p8) = crate::crypto::decrypt(&state.cfg.secret_key, enc) else { return Err(Sent::Account("the APNs key can't be read".into())) };
            let Some((cache, token)) = apns_token(row, &p8, env).await else { return Err(Sent::Account("the APNs key can't sign".into())) };
            let Some(client) = apns_client(row, env).await else { return Err(Sent::Retry("couldn't connect".into(), None)) };
            let base = match (test_base(state), env) {
                (Some(b), "sandbox") => format!("{b}/apns-sandbox"),
                (Some(b), _) => format!("{b}/apns"),
                (None, "sandbox") => "https://api.sandbox.push.apple.com".into(),
                (None, _) => "https://api.push.apple.com".into(),
            };
            Ok(Creds::Apns { client, token, cache, base, topic: topic.to_string(), env })
        }
        _ => {
            let (Some(enc), Some(project)) = (row.fcm_credentials_enc.as_deref(), row.fcm_project_id.as_deref()) else { return Err(Sent::Account("Firebase isn't set up".into())) };
            let Ok(raw) = crate::crypto::decrypt(&state.cfg.secret_key, enc) else { return Err(Sent::Account("the service account can't be read".into())) };
            let Ok(sa) = serde_json::from_str::<ServiceAccount>(&raw) else { return Err(Sent::Account("the service account can't be read".into())) };
            let (cache, token) = fcm_token(state, row, &sa, &raw).await?;
            let base = test_base(state).map(|b| format!("{b}/fcm")).unwrap_or_else(|| "https://fcm.googleapis.com".into());
            Ok(Creds::Fcm { token, cache, url: format!("{base}/v1/projects/{project}/messages:send") })
        }
    }
}

/// Send to APNs. `own_environment`: the device said which environment it belongs to, so a
/// `BadDeviceToken` from that environment is about the device.
pub async fn send_apns(creds: &Creds, m: &ApnsMessage<'_>, own_environment: bool) -> Sent {
    let Creds::Apns { client, token, cache, base, topic, env } = creds else { return Sent::Account("APNs isn't set up".into()) };
    let (cache, env) = (cache.clone(), *env);
    if !m.device_token.chars().all(|c| c.is_ascii_hexdigit()) || m.device_token.len() > 200 {
        return Sent::Gone("APNs BadDeviceToken".into(), None);
    }
    let expiration = if m.ttl == 0 { 0 } else { chrono::Utc::now().timestamp() + m.ttl };
    let mut req = client
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
    let body = json_capped(res).await;
    if (200..300).contains(&status) {
        return Sent::Ok(id);
    }
    // Only Apple's documented reason names are kept (they're short fixed words).
    let reason = body["reason"].as_str().filter(|r| r.len() <= 40 && r.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or("").to_string();
    let class = if reason.is_empty() { format!("APNs HTTP {status}") } else { format!("APNs {reason}") };
    match (status, reason.as_str()) {
        // The token is no longer valid for this app: device-specific.
        (410, "Unregistered" | "ExpiredToken") => Sent::Gone(class, body["timestamp"].as_i64()),
        // An invalid token, or one from the other environment: about the device only when it was
        // tried in the environment its app said it belongs to.
        (400, "BadDeviceToken") if own_environment => Sent::Gone(class, None),
        (400, "BadDeviceToken") => Sent::Account(format!("{class} ({env}): have the app say whether it's a development (sandbox) or App Store (production) build when it registers")),
        (400, "DeviceTokenNotForTopic" | "BadTopic" | "TopicDisallowed" | "MissingTopic") => Sent::Account(class),
        (403, "ExpiredProviderToken" | "InvalidProviderToken" | "MissingProviderToken") => {
            APNS_TOKENS.lock().await.remove(&cache);
            Sent::Account(class)
        }
        (403, _) | (404, _) | (405, _) | (410, _) => Sent::Account(class),
        (429, _) | (500..=599, _) => Sent::Retry(class, after),
        (413, _) | (400, _) => Sent::Rejected(class),
        _ => Sent::Failed(class),
    }
}

// ---------- FCM ----------

/// Access tokens per account and credential version, until shortly before they expire.
static FCM_TOKENS: Lazy<Mutex<HashMap<String, (String, i64)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Sign-ins in progress, per credential set.
static FCM_FLIGHTS: Lazy<Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";

async fn fcm_token(state: &AppState, row: &Row, sa: &ServiceAccount, raw: &str) -> Result<(String, String), Sent> {
    let token_url = match test_base(state) {
        Some(b) => format!("{b}/oauth2/token"),
        None => GOOGLE_TOKEN_URI.to_string(),
    };
    let key = format!("{}:{}:{}", row.account_id, row.credentials_version, cache_key(&[raw, &token_url]));
    let now = chrono::Utc::now().timestamp();
    let cached = || async {
        FCM_TOKENS.lock().await.get(&key).filter(|(_, exp)| *exp - 300 > now).map(|(t, _)| t.clone())
    };
    if let Some(t) = cached().await {
        return Ok((key, t));
    }
    // One sign-in per credential set at a time: devices sent to at once share it.
    let flight = {
        let mut m = FCM_FLIGHTS.lock().await;
        bound(&mut m, MAX_CACHED, |f: &std::sync::Arc<Mutex<()>>| std::sync::Arc::strong_count(f) == 1);
        m.entry(key.clone()).or_insert_with(|| std::sync::Arc::new(Mutex::new(()))).clone()
    };
    let _turn = flight.lock().await;
    if let Some(t) = cached().await {
        return Ok((key, t));
    }
    let assertion = crypto::google_assertion(&sa.private_key, &sa.private_key_id, &sa.client_email, FCM_SCOPE, GOOGLE_TOKEN_URI, now).map_err(|_| Sent::Account("the service account key can't sign".into()))?;
    let res = HTTP
        .post(&token_url)
        .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", assertion.as_str())])
        .send()
        .await
        // Nothing was sent to FCM yet: a failed sign-in can always be retried.
        .map_err(|e| if e.is_connect() { Sent::Retry("couldn't reach Google sign-in".into(), None) } else { Sent::Retry("Google sign-in didn't answer".into(), None) })?;
    let status = res.status().as_u16();
    let after = retry_after(res.headers());
    let v = json_capped(res).await;
    if status == 429 || status >= 500 {
        return Err(Sent::Retry(format!("Google sign-in HTTP {status}"), after));
    }
    if !(200..300).contains(&status) {
        return Err(Sent::Account(format!("Google refused the service account (HTTP {status})")));
    }
    let token = v["access_token"].as_str().filter(|t| !t.is_empty() && t.len() < 4096).ok_or_else(|| Sent::Retry("Google sign-in answered oddly".into(), None))?.to_string();
    let exp = now + v["expires_in"].as_i64().unwrap_or(3600).clamp(60, 3600);
    let mut m = FCM_TOKENS.lock().await;
    bound(&mut m, MAX_CACHED, |(_, e)| *e - 300 <= now);
    m.insert(key.clone(), (token.clone(), exp));
    Ok((key, token))
}

/// Firebase tokens cached for an account (tests).
#[doc(hidden)]
pub async fn fcm_tokens_for(account_id: Uuid) -> usize {
    let prefix = format!("{account_id}:");
    FCM_TOKENS.lock().await.keys().filter(|k| k.starts_with(&prefix)).count()
}

pub async fn send_fcm(creds: &Creds, message: &Value) -> Sent {
    let Creds::Fcm { token, cache, url } = creds else { return Sent::Account("Firebase isn't set up".into()) };
    let cache = cache.clone();
    let res = match HTTP.post(url).bearer_auth(token).json(&json!({"message": message})).send().await {
        Ok(r) => r,
        Err(e) => return network(&e),
    };
    let status = res.status().as_u16();
    let after = retry_after(res.headers());
    let v = json_capped(res).await;
    if (200..300).contains(&status) {
        return Sent::Ok(v["name"].as_str().map(|n| n.rsplit('/').next().unwrap_or(n).chars().take(100).collect()));
    }
    // The FCM error code is in details[].errorCode; the top-level status is the gRPC one.
    let code = v["error"]["details"]
        .as_array()
        .and_then(|d| d.iter().find_map(|x| x["errorCode"].as_str()))
        .filter(|c| c.len() <= 40 && c.chars().all(|ch| ch.is_ascii_uppercase() || ch == '_'))
        .unwrap_or("")
        .to_string();
    let class = if code.is_empty() { format!("FCM HTTP {status}") } else { format!("FCM {code}") };
    match (status, code.as_str()) {
        // The only device-specific answer: the token is gone (uninstalled, expired, unregistered).
        (_, "UNREGISTERED") => Sent::Gone(class, None),
        (401, _) => {
            FCM_TOKENS.lock().await.remove(&cache);
            Sent::Account(class)
        }
        // A token from another Firebase project, credentials or permissions, a project that
        // doesn't exist (a bare 404): the account's setup, not the device.
        (_, "SENDER_ID_MISMATCH" | "THIRD_PARTY_AUTH_ERROR" | "THIRD_PARTY_PERMISSION_DENIED") | (403, _) | (404, _) => Sent::Account(class),
        (_, "QUOTA_EXCEEDED" | "UNAVAILABLE" | "INTERNAL") | (429, _) | (500..=599, _) => Sent::Retry(class, after),
        // INVALID_ARGUMENT: a malformed token or field; counts against the device.
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

    #[tokio::test]
    async fn response_bodies_are_capped() {
        let big = vec![b'x'; 4 * super::MAX_RESPONSE];
        let res = reqwest::Response::from(axum::http::Response::new(big));
        assert_eq!(super::read_capped(res).await.len(), super::MAX_RESPONSE);
        let res = reqwest::Response::from(axum::http::Response::new(r#"{"reason":"Unregistered"}"#.to_string()));
        assert_eq!(super::json_capped(res).await["reason"], "Unregistered");
    }

    #[test]
    fn caches_stay_bounded() {
        let mut m: std::collections::HashMap<String, i64> = (0..10).map(|i| (i.to_string(), i)).collect();
        super::bound(&mut m, 5, |v| *v < 3);
        assert!(m.len() < 5);
    }
}
