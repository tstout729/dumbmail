//! A small client for Klaviyo's REST API (https://developers.klaviyo.com/en/reference/api_overview).
//!
//! Every request sends the private key as `Authorization: Klaviyo-API-Key …` and the `revision` header.
//! Requests are paced per endpoint to stay under Klaviyo's steady rate limits, and a 429 waits for
//! `Retry-After` (short waits inline, long ones by rescheduling the job). Only Klaviyo's own host is ever
//! called: `links.next` URLs from responses are followed only when they point back at it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The API version this importer was built and tested against.
pub const REVISION: &str = "2026-10-15";
const DEFAULT_BASE: &str = "https://a.klaviyo.com";
/// Largest response body read (a page of 1,000 events with big properties fits easily).
const MAX_BODY: usize = 24 * 1024 * 1024;

static BASE_OVERRIDE: OnceLock<String> = OnceLock::new();

/// Point the importer at another host speaking Klaviyo's API. Tests use a local stand-in; nothing a
/// customer types can change this.
pub fn set_api_base(url: &str) {
    let _ = BASE_OVERRIDE.set(url.trim_end_matches('/').to_string());
}

pub fn api_base() -> &'static str {
    BASE_OVERRIDE.get().map(String::as_str).unwrap_or(DEFAULT_BASE)
}

#[derive(Debug, thiserror::Error)]
pub enum KlaviyoError {
    /// The key is wrong or was revoked.
    #[error("Klaviyo didn't accept this API key. Check it was copied whole, or make a new one.")]
    BadKey,
    /// The key works but lacks a scope this request needs.
    #[error("The Klaviyo key can't read {0}. Give it read access to {0} and try again.")]
    MissingScope(String),
    /// Klaviyo asked us to wait this long before the next request.
    #[error("Klaviyo asked us to slow down; continuing in {0} seconds")]
    RateLimited(u64),
    /// Only Klaviyo's status and error code are kept: its detail text is never stored, logged or shown.
    #[error("Klaviyo returned {status} ({code})")]
    Status { status: u16, code: String },
    #[error("couldn't reach Klaviyo: {0}")]
    Network(String),
    #[error("Klaviyo sent a response we couldn't read: {0}")]
    Decode(String),
}

impl KlaviyoError {
    /// Worth trying again later without the customer doing anything.
    pub fn is_transient(&self) -> bool {
        matches!(self, KlaviyoError::RateLimited(_) | KlaviyoError::Network(_)) || matches!(self, KlaviyoError::Status { status, .. } if *status >= 500)
    }
}

pub type KResult<T> = Result<T, KlaviyoError>;

/// Endpoint families with their own steady limits (requests per minute) from Klaviyo's reference.
fn steady_per_minute(path: &str) -> u32 {
    let p = path.trim_start_matches("/api/");
    if p.starts_with("events") {
        3500
    } else if p.starts_with("campaign-values-reports") || p.starts_with("flow-values-reports") {
        2
    } else if p.starts_with("metric-aggregates") || p.starts_with("flows") || p.starts_with("forms") {
        60
    } else if p.starts_with("accounts") {
        15
    } else if p.starts_with("campaign") || p.starts_with("metrics") {
        150
    } else {
        750
    }
}

/// When each endpoint family was last called, per Klaviyo key, shared by every job in this process (a
/// survey and an import, or two jobs for one account, pace together).
static PACE: OnceLock<Mutex<HashMap<(u64, String), Instant>>> = OnceLock::new();

fn key_id(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

pub struct Client {
    http: reqwest::Client,
    key: String,
    key_id: u64,
    base: String,
    /// Longest a 429 may be waited out inside one call before giving the job back.
    pub max_inline_wait: Duration,
    pub calls: u64,
}

impl Client {
    pub fn new(http: reqwest::Client, key: &str) -> Self {
        let key = key.trim().to_string();
        Client { http, key_id: key_id(&key), key, base: api_base().to_string(), max_inline_wait: Duration::from_secs(20), calls: 0 }
    }

    fn url(&self, path_or_url: &str) -> KResult<String> {
        if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            // Only follow pagination links back to Klaviyo itself.
            if path_or_url.starts_with(&format!("{}/", self.base)) {
                return Ok(path_or_url.to_string());
            }
            return Err(KlaviyoError::Decode("a pagination link pointed somewhere other than Klaviyo".into()));
        }
        Ok(format!("{}{}", self.base, path_or_url))
    }

    /// Wait so this endpoint family stays under its steady rate (custom limits: special-cased paths).
    async fn pace(&mut self, path: &str, per_minute_override: Option<u32>) {
        let family = path.trim_start_matches(&self.base).trim_start_matches("/api/").split(['/', '?']).next().unwrap_or("").to_string();
        let per_min = per_minute_override.unwrap_or_else(|| steady_per_minute(&format!("/api/{family}")));
        let gap = Duration::from_millis(60_000 / per_min.max(1) as u64);
        // Reserve the next slot for this key and family, then wait for it outside the lock.
        let wait = {
            let mut m = PACE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let slot = match m.get(&(self.key_id, family.clone())) {
                Some(last) if *last + gap > now => *last + gap,
                _ => now,
            };
            m.insert((self.key_id, family), slot);
            if m.len() > 10_000 {
                m.retain(|_, t| t.elapsed() < Duration::from_secs(600));
            }
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    pub async fn get(&mut self, path_or_url: &str) -> KResult<Value> {
        self.send(reqwest::Method::GET, path_or_url, None, None).await
    }

    /// A GET on an endpoint with a slower limit than its family (e.g. `profile_count`, 15/m).
    pub async fn get_paced(&mut self, path_or_url: &str, per_minute: u32) -> KResult<Value> {
        self.send(reqwest::Method::GET, path_or_url, None, Some(per_minute)).await
    }

    pub async fn post(&mut self, path: &str, body: &Value) -> KResult<Value> {
        self.send(reqwest::Method::POST, path, Some(body), None).await
    }

    async fn send(&mut self, method: reqwest::Method, path_or_url: &str, body: Option<&Value>, per_minute: Option<u32>) -> KResult<Value> {
        let url = self.url(path_or_url)?;
        let mut waited = Duration::ZERO;
        let mut server_errors = 0;
        loop {
            self.pace(&url, per_minute).await;
            self.calls += 1;
            let mut req = self
                .http
                .request(method.clone(), &url)
                .header("Authorization", format!("Klaviyo-API-Key {}", self.key))
                .header("revision", REVISION)
                .header("Accept", "application/vnd.api+json")
                .timeout(Duration::from_secs(60));
            if let Some(b) = body {
                req = req.header("Content-Type", "application/vnd.api+json").body(b.to_string());
            }
            let res = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    server_errors += 1;
                    if server_errors < 3 {
                        tokio::time::sleep(Duration::from_secs(2 * server_errors)).await;
                        continue;
                    }
                    // Never echo the request (it carries the key); the error text has no headers.
                    return Err(KlaviyoError::Network(e.without_url().to_string()));
                }
            };
            let status = res.status().as_u16();
            if status == 429 {
                let secs = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(5).clamp(1, 3600);
                let wait = Duration::from_secs(secs);
                if waited + wait <= self.max_inline_wait {
                    waited += wait;
                    tokio::time::sleep(wait).await;
                    continue;
                }
                return Err(KlaviyoError::RateLimited(secs));
            }
            // Out of steady quota: sleep until the window resets rather than collect a 429.
            let remaining = res.headers().get("ratelimit-remaining").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            let reset = res.headers().get("ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            let bytes = read_capped(res).await?;
            if status >= 500 {
                server_errors += 1;
                if server_errors < 3 {
                    tokio::time::sleep(Duration::from_secs(3 * server_errors)).await;
                    continue;
                }
            }
            if !(200..300).contains(&status) {
                return Err(error_for(status, &bytes));
            }
            if remaining == Some(0) {
                if let Some(r) = reset.filter(|r| *r <= 60) {
                    tokio::time::sleep(Duration::from_secs(r)).await;
                }
            }
            if bytes.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_slice(&bytes).map_err(|e| KlaviyoError::Decode(e.to_string()));
        }
    }
}

async fn read_capped(mut res: reqwest::Response) -> KResult<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| KlaviyoError::Network(e.without_url().to_string()))? {
        if out.len() + chunk.len() > MAX_BODY {
            return Err(KlaviyoError::Decode("response too large".into()));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn error_for(status: u16, body: &[u8]) -> KlaviyoError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let first = &v["errors"][0];
    // The detail text is read only to tell a bad key or a missing scope; it's never kept.
    let code: String = first["code"].as_str().unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(60).collect();
    let detail = first["detail"].as_str().or(first["title"].as_str()).unwrap_or("").to_lowercase();
    match status {
        401 => KlaviyoError::BadKey,
        400 if code.contains("authenticat") || detail.contains("api key") => KlaviyoError::BadKey,
        403 => KlaviyoError::MissingScope(scope_hint(&detail)),
        _ => KlaviyoError::Status { status, code: if code.is_empty() { "no code".into() } else { code } },
    }
}

fn scope_hint(detail: &str) -> String {
    let d = detail.to_lowercase();
    for s in ["profiles", "lists", "segments", "metrics", "events", "templates", "campaigns", "flows", "forms", "catalogs", "coupons", "accounts"] {
        if d.contains(s) {
            return s.to_string();
        }
    }
    "this data".to_string()
}

/// `links.next` of a JSON:API page.
pub fn next_link(page: &Value) -> Option<String> {
    page["links"]["next"].as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// Encode a value for a query string (filters contain quotes, commas and parentheses).
pub fn q(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The scopes a read-only key needs, in the words Klaviyo's key screen uses.
pub const SCOPES: &[(&str, &str)] = &[
    ("Accounts", "accounts:read"),
    ("Campaigns", "campaigns:read"),
    ("Catalogs", "catalogs:read"),
    ("Coupons", "coupons:read"),
    ("Events", "events:read"),
    ("Flows", "flows:read"),
    ("Forms", "forms:read"),
    ("Lists", "lists:read"),
    ("Metrics", "metrics:read"),
    ("Profiles", "profiles:read"),
    ("Segments", "segments:read"),
    ("Templates", "templates:read"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_follow_the_reference() {
        assert_eq!(steady_per_minute("/api/events"), 3500);
        assert_eq!(steady_per_minute("/api/profiles"), 750);
        assert_eq!(steady_per_minute("/api/flows/abc"), 60);
        assert_eq!(steady_per_minute("/api/campaign-values-reports"), 2);
        assert_eq!(steady_per_minute("/api/campaign-messages/x"), 150);
    }

    #[test]
    fn errors_map_to_plain_messages() {
        assert!(matches!(error_for(401, b"{}"), KlaviyoError::BadKey));
        let e = error_for(403, br#"{"errors":[{"code":"permission_denied","detail":"Missing scope: flows:read"}]}"#);
        assert_eq!(e.to_string(), "The Klaviyo key can't read flows. Give it read access to flows and try again.");
        assert!(error_for(503, b"").is_transient());
        let e = error_for(400, br#"{"errors":[{"code":"invalid","detail":"Bad filter for pk_secret123 at profile ann@example.com"}]}"#);
        assert_eq!(e.to_string(), "Klaviyo returned 400 (invalid)", "Klaviyo's detail text is never kept");
    }

    #[test]
    fn pagination_links_stay_on_klaviyo() {
        let c = Client::new(reqwest::Client::new(), "pk_x");
        assert!(c.url("https://a.klaviyo.com/api/profiles?page%5Bcursor%5D=x").is_ok());
        assert!(c.url("https://evil.test/api/profiles").is_err());
        assert!(c.url("https://a.klaviyo.com.evil.test/api").is_err());
    }
}
