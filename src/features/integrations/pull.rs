//! Pulls: fetching data from an app on a schedule (or for a backfill), declared as data.
//!
//! The request is a template (`{{secret.NAME}}`, `{{state.KEY}}`, `{{settings.KEY}}`, `{{now}}`,
//! `{{since}}`, with `|` fallbacks), sent only to public internet addresses with pinned DNS, no
//! redirects, a timeout and a response size cap. OAuth2 client-credentials tokens are cached
//! encrypted and reused until shortly before they expire, and the number of tokens asked for in a
//! day is capped, because some apps (Guesty) allow only a handful.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::actions::NextRequest;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Largest response read from an app.
pub const RESPONSE_MAX_BYTES: usize = 5 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
pub const MIN_INTERVAL_MINUTES: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PullConfig {
    #[serde(default = "get")]
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub headers: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub query: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// `json` (default) or `form` for the request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_format: Option<BodyFormat>,
    #[serde(default)]
    pub auth: Auth,
    /// Run on a schedule this often. Leave out to pull only when asked (or for a backfill).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_minutes: Option<u32>,
    /// Pages one scheduled run may fetch (the script asks for more with `next_request`). Default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<u32>,
}

fn get() -> String {
    "GET".into()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    Json,
    Form,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    #[default]
    None,
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: String,
    },
    Header {
        name: String,
        value: String,
    },
    Oauth2ClientCredentials {
        token_url: String,
        client_id: String,
        client_secret: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audience: Option<String>,
        /// `body` (default) sends the client id and secret as form fields; `basic` as HTTP Basic auth.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_auth: Option<String>,
        /// Never ask for more tokens than this in 24 hours. Default 4 (Guesty allows 5).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_tokens_per_day: Option<u32>,
    },
}

impl PullConfig {
    pub fn parse(v: &Value) -> AppResult<Self> {
        let c: PullConfig = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid pull settings: {e}")))?;
        if !matches!(c.method.as_str(), "GET" | "POST") {
            return Err(AppError::bad("pull method must be GET or POST"));
        }
        check_url_shape(&c.url)?;
        if let Some(i) = c.interval_minutes {
            if !(MIN_INTERVAL_MINUTES..=7 * 24 * 60).contains(&i) {
                return Err(AppError::bad(format!("pull interval_minutes must be {MIN_INTERVAL_MINUTES} to 10080")));
            }
        }
        if c.max_pages.is_some_and(|p| !(1..=50).contains(&p)) {
            return Err(AppError::bad("pull max_pages must be 1 to 50"));
        }
        if c.headers.len() > 30 || c.query.len() > 50 {
            return Err(AppError::bad("at most 30 headers and 50 query parameters"));
        }
        for (k, v) in c.headers.iter().chain(c.query.iter()) {
            if !(v.is_string() || v.is_number() || v.is_boolean()) {
                return Err(AppError::bad(format!("pull header/query {k:?} must be a string, number or true/false")));
            }
        }
        for k in c.headers.keys() {
            if axum::http::HeaderName::from_bytes(k.as_bytes()).is_err() {
                return Err(AppError::bad(format!("{k:?} is not a valid header name")));
            }
        }
        if serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) > 32 * 1024 {
            return Err(AppError::bad("pull settings are over 32 KB"));
        }
        if let Auth::Oauth2ClientCredentials { token_url, client_auth, max_tokens_per_day, .. } = &c.auth {
            check_url_shape(token_url)?;
            if client_auth.as_deref().is_some_and(|a| !matches!(a, "body" | "basic")) {
                return Err(AppError::bad("oauth2 client_auth must be body or basic"));
            }
            if max_tokens_per_day.is_some_and(|m| !(1..=100).contains(&m)) {
                return Err(AppError::bad("oauth2 max_tokens_per_day must be 1 to 100"));
            }
        }
        Ok(c)
    }

    /// Credential names the templates use (`{{secret.NAME}}`).
    pub fn secret_names(&self) -> Vec<String> {
        let text = serde_json::to_string(self).unwrap_or_default();
        let mut out = Vec::new();
        for cap in PLACEHOLDER.captures_iter(&text) {
            for alt in cap[1].split('|') {
                if let Some(n) = alt.trim().strip_prefix("secret.") {
                    if !out.iter().any(|x: &String| x == n) {
                        out.push(n.to_string());
                    }
                }
            }
        }
        out
    }
}

fn check_url_shape(u: &str) -> AppResult<()> {
    let base = PLACEHOLDER.replace_all(u, "x");
    let parsed = url::Url::parse(&base).map_err(|_| AppError::bad(format!("{u:?} is not a valid URL")))?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err(AppError::bad("pull URLs must be http(s)"));
    }
    if u.len() > 2000 {
        return Err(AppError::bad("pull URL is too long"));
    }
    Ok(())
}

static PLACEHOLDER: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"\{\{\s*([^{}]+?)\s*\}\}").unwrap());

/// Values placeholders can use.
pub struct Vars<'a> {
    pub secrets: &'a Map<String, Value>,
    pub state: &'a Map<String, Value>,
    pub settings: &'a Map<String, Value>,
    pub now: DateTime<Utc>,
    pub since: DateTime<Utc>,
    /// Write `•••` instead of secrets (for the delivery log).
    pub redact: bool,
}

fn plain(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// How `{{state.KEY}}` renders. State comes from script output (and so from payloads), so in a URL
/// it is percent-encoded and must never decide where the request goes.
#[derive(Clone, Copy, PartialEq)]
enum StateMode {
    Plain,
    Encoded,
    Marker,
}

impl Vars<'_> {
    fn lookup(&self, name: &str, mode: StateMode) -> Result<Option<String>, String> {
        let name = name.trim();
        if let Some(lit) = name.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            return Ok(Some(lit.to_string()));
        }
        let iso = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Ok(match name {
            "now" => Some(iso(self.now)),
            "since" => Some(iso(self.since)),
            "now_unix" => Some(self.now.timestamp().to_string()),
            "since_unix" => Some(self.since.timestamp().to_string()),
            _ => {
                if let Some(k) = name.strip_prefix("secret.") {
                    match self.secrets.get(k).and_then(plain) {
                        Some(_) if self.redact => Some("•••".into()),
                        Some(v) => Some(v),
                        None => None,
                    }
                } else if let Some(k) = name.strip_prefix("state.") {
                    self.state.get(k).and_then(plain).map(|v| match mode {
                        StateMode::Plain => v,
                        StateMode::Encoded => url::form_urlencoded::byte_serialize(v.as_bytes()).collect(),
                        StateMode::Marker => "statevalue".into(),
                    })
                } else if let Some(k) = name.strip_prefix("settings.") {
                    self.settings.get(k).and_then(plain)
                } else {
                    return Err(format!("unknown placeholder {{{{{name}}}}} (use secret.NAME, state.KEY, settings.KEY, now, since or a 'quoted' fallback)"));
                }
            }
        })
    }

    pub fn render(&self, s: &str) -> Result<String, String> {
        self.render_with(s, StateMode::Plain)
    }

    /// Render a URL template. Saved state is percent-encoded, and the scheme, host and port must be
    /// the same whatever the state holds, so a payload can never send credentials to another site.
    pub fn render_url(&self, s: &str) -> Result<String, String> {
        let real = self.render_with(s, StateMode::Encoded)?;
        let probe = self.render_with(s, StateMode::Marker)?;
        let origin = |u: &str| url::Url::parse(u).map(|u| u.origin().ascii_serialization()).map_err(|_| format!("{u:?} isn't a valid URL"));
        if origin(&real)? != origin(&probe)? {
            return Err("saved state ({{state.…}}) can't change a URL's site; use it only in the path or query".into());
        }
        Ok(real)
    }

    fn render_with(&self, s: &str, mode: StateMode) -> Result<String, String> {
        let mut err = None;
        let out = PLACEHOLDER.replace_all(s, |c: &regex::Captures<'_>| {
            let expr = &c[1];
            for alt in expr.split('|') {
                match self.lookup(alt, mode) {
                    Ok(Some(v)) => return v,
                    Ok(None) => {}
                    Err(e) => {
                        err.get_or_insert(e);
                        return String::new();
                    }
                }
            }
            let first = expr.split('|').next().unwrap_or("").trim();
            err.get_or_insert(match first.strip_prefix("secret.") {
                Some(n) => format!("credential {n:?} isn't saved yet; add it in the integration's credentials"),
                None => format!("{{{{{expr}}}}} has no value (add a fallback like {{{{{first}|since}}}})"),
            });
            String::new()
        });
        match err {
            Some(e) => Err(e),
            None => Ok(out.into_owned()),
        }
    }

    fn render_value(&self, v: &Value) -> Result<Value, String> {
        Ok(match v {
            Value::String(s) => Value::String(self.render(s)?),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.render_value(x)).collect::<Result<_, _>>()?),
            Value::Object(o) => Value::Object(o.iter().map(|(k, x)| Ok((k.clone(), self.render_value(x)?))).collect::<Result<_, String>>()?),
            other => other.clone(),
        })
    }
}

/// A request ready to send.
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip)]
    pub form: bool,
}

impl Request {
    /// What the delivery log shows (secrets already redacted when rendered with `redact`).
    pub fn log_json(&self) -> Value {
        let hidden = ["authorization", "cookie", "x-api-key", "api-key", "apikey"];
        let headers: Map<String, Value> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), json!(if hidden.contains(&k.to_ascii_lowercase().as_str()) { "•••" } else { v.as_str() })))
            .collect();
        json!({"method": self.method, "url": self.url, "query": self.query.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(), "headers": headers, "body": self.body})
    }
}

/// Build the request for one page: the config's templates rendered, then the script's
/// `next_request` changes applied as literal values (they never see or expand secrets).
pub fn build(cfg: &PullConfig, next: &NextRequest, vars: &Vars<'_>) -> Result<Request, String> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for (k, v) in &cfg.headers {
        headers.push((k.clone(), vars.render(&plain(v).unwrap_or_default())?));
    }
    match &cfg.auth {
        Auth::None | Auth::Oauth2ClientCredentials { .. } => {}
        Auth::Bearer { token } => headers.push(("Authorization".into(), format!("Bearer {}", vars.render(token)?))),
        Auth::Basic { username, password } => {
            let raw = format!("{}:{}", vars.render(username)?, vars.render(password)?);
            let v = if vars.redact { "•••".to_string() } else { base64::engine::general_purpose::STANDARD.encode(raw) };
            headers.push(("Authorization".into(), format!("Basic {v}")));
        }
        Auth::Header { name, value } => headers.push((name.clone(), vars.render(value)?)),
    }
    let mut query: Vec<(String, String)> = Vec::new();
    for (k, v) in &cfg.query {
        query.push((k.clone(), vars.render(&plain(v).unwrap_or_default())?));
    }
    let mut url = vars.render_url(&cfg.url)?;
    let mut method = cfg.method.clone();
    let mut body = match &cfg.body {
        Some(b) => Some(vars.render_value(b)?),
        None => None,
    };
    if let Some(u) = &next.url {
        let base = url::Url::parse(&url).map_err(|_| "the pull URL isn't valid".to_string())?;
        let joined = base.join(u).map_err(|_| format!("next_request url {u:?} isn't valid"))?;
        if joined.origin() != base.origin() {
            return Err(format!("next_request url must stay on {} (got {})", base.origin().ascii_serialization(), joined.origin().ascii_serialization()));
        }
        // A full next-page URL carries its own query; the configured query still applies unless replaced.
        url = joined.to_string();
    }
    if let Some(m) = &next.method {
        method = m.to_ascii_uppercase();
    }
    if let Some(q) = &next.query {
        for (k, v) in q {
            query.retain(|(qk, _)| qk != k);
            if let Some(s) = plain(v) {
                query.push((k.clone(), s));
            } else if !v.is_null() {
                query.push((k.clone(), v.to_string()));
            }
        }
    }
    if let Some(h) = &next.headers {
        for (k, v) in h {
            if k.eq_ignore_ascii_case("authorization") || k.eq_ignore_ascii_case("host") {
                return Err("next_request can't change the Authorization or Host header".into());
            }
            headers.retain(|(hk, _)| !hk.eq_ignore_ascii_case(k));
            if let Some(s) = plain(v) {
                headers.push((k.clone(), s));
            }
        }
    }
    if let Some(b) = &next.body {
        body = Some(b.clone());
    }
    Ok(Request { method, url, headers, query, body, form: cfg.body_format == Some(BodyFormat::Form) })
}

// ---------- Outbound HTTP ----------

/// Addresses tests may reach despite being on loopback (a local mock of an app's API). Empty in production.
static TEST_ADDRESSES: Lazy<Mutex<HashSet<SocketAddr>>> = Lazy::new(Default::default);

/// Let requests reach one local test server. For tests only; production never calls this.
#[doc(hidden)]
pub fn allow_test_address(addr: SocketAddr) {
    TEST_ADDRESSES.lock().unwrap().insert(addr);
}

/// The SSRF-safe address check (`flows::check_webhook_url`): public addresses only, pinned.
async fn vet(url: &str) -> AppResult<(String, SocketAddr)> {
    if let Ok(u) = url::Url::parse(url) {
        if let (Some(url::Host::Ipv4(ip)), Some(port)) = (u.host(), u.port_or_known_default()) {
            let addr = SocketAddr::new(ip.into(), port);
            if TEST_ADDRESSES.lock().unwrap().contains(&addr) {
                return Ok((ip.to_string(), addr));
            }
        }
    }
    crate::flows::check_webhook_url(url).await.map_err(|_| AppError::bad("pull URLs must point to a public internet address"))
}

pub struct Response {
    pub status: u16,
    pub headers: Map<String, Value>,
    pub body: String,
}

impl Response {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| Value::String(self.body.clone()))
    }
}

/// Send a request to a public address: pinned DNS, no redirects, a timeout and a size cap.
pub async fn send(req: &Request, bearer: Option<&str>) -> AppResult<Response> {
    let mut url = url::Url::parse(&req.url).map_err(|_| AppError::bad("the pull URL isn't valid"))?;
    if !req.query.is_empty() {
        let keep: Vec<(String, String)> = url.query_pairs().filter(|(k, _)| !req.query.iter().any(|(q, _)| q == k)).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        url.query_pairs_mut().clear().extend_pairs(keep).extend_pairs(req.query.iter());
    }
    let (host, addr) = vet(url.as_str()).await?;
    let client = reqwest::Client::builder()
        .resolve(&host, addr)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .user_agent("dumbmail-integrations/0.1")
        .build()
        .map_err(|e| AppError::Internal(e.into()))?;
    let method = if req.method == "POST" { reqwest::Method::POST } else { reqwest::Method::GET };
    let mut b = client.request(method, url.clone()).header("accept", "application/json");
    for (k, v) in &req.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(body) = &req.body {
        b = if req.form {
            let pairs: Vec<(String, String)> = body.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), plain(v).unwrap_or_else(|| v.to_string()))).collect()).unwrap_or_default();
            b.form(&pairs)
        } else {
            b.json(body)
        };
    }
    let res = b.send().await.map_err(|e| AppError::bad(format!("couldn't reach {}: {}", url.host_str().unwrap_or(""), e.without_url())))?;
    read(res).await
}

async fn read(res: reqwest::Response) -> AppResult<Response> {
    let status = res.status().as_u16();
    if res.status().is_redirection() {
        let loc = res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        return Err(AppError::bad(format!("the app redirected to {loc:?}; use that address in the pull URL")));
    }
    let keep = ["content-type", "link", "x-total-count", "ratelimit-remaining", "ratelimit-reset", "retry-after", "x-ratelimit-remaining"];
    let headers: Map<String, Value> = res
        .headers()
        .iter()
        .filter(|(k, _)| keep.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str().to_string(), json!(v.to_str().unwrap_or(""))))
        .collect();
    let mut body = Vec::new();
    let mut res = res;
    while let Some(chunk) = res.chunk().await.map_err(|e| AppError::bad(format!("couldn't read the app's answer: {}", e.without_url())))? {
        body.extend_from_slice(&chunk);
        if body.len() > RESPONSE_MAX_BYTES {
            return Err(AppError::bad(format!("the app's answer is over {} MB; ask for smaller pages", RESPONSE_MAX_BYTES / (1024 * 1024))));
        }
    }
    Ok(Response { status, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

// ---------- OAuth2 client credentials ----------

/// A bearer token for an OAuth2 client-credentials pull: the cached one while it has more than
/// five minutes left, otherwise a new one (unless today's cap is reached). The integration's row
/// is locked while deciding, so two pulls at once never both ask for a token.
pub async fn oauth_token(state: &AppState, integration_id: Uuid, auth: &Auth, vars: &Vars<'_>) -> AppResult<Option<String>> {
    let Auth::Oauth2ClientCredentials { token_url, client_id, client_secret, scope, audience, client_auth, max_tokens_per_day } = auth else { return Ok(None) };
    let mut tx = state.db.begin().await?;
    let (cached, expires, requests): (Option<String>, Option<DateTime<Utc>>, Vec<DateTime<Utc>>) =
        sqlx::query_as("SELECT oauth_token_enc, oauth_token_expires_at, oauth_token_requests FROM integrations WHERE id = $1 FOR UPDATE")
            .bind(integration_id)
            .fetch_one(&mut *tx)
            .await?;
    if let (Some(enc), Some(exp)) = (&cached, expires) {
        if exp > Utc::now() + chrono::Duration::minutes(5) {
            tx.commit().await?;
            return Ok(Some(crate::crypto::decrypt(&state.cfg.secret_key, enc)?));
        }
    }
    let recent: Vec<DateTime<Utc>> = requests.into_iter().filter(|t| *t > Utc::now() - chrono::Duration::hours(24)).collect();
    let cap = max_tokens_per_day.unwrap_or(4) as usize;
    if recent.len() >= cap {
        let next = recent.iter().min().map(|t| *t + chrono::Duration::hours(24)).unwrap_or_else(Utc::now);
        return Err(AppError::Conflict(format!(
            "already asked for {cap} access tokens in the last 24 hours (the daily cap); the next one can be requested after {}",
            next.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        )));
    }
    let id = vars.render(client_id).map_err(AppError::BadRequest)?;
    let secret = vars.render(client_secret).map_err(AppError::BadRequest)?;
    let mut form: Vec<(String, String)> = vec![("grant_type".into(), "client_credentials".into())];
    if let Some(s) = scope {
        form.push(("scope".into(), vars.render(s).map_err(AppError::BadRequest)?));
    }
    if let Some(a) = audience {
        form.push(("audience".into(), vars.render(a).map_err(AppError::BadRequest)?));
    }
    let basic = client_auth.as_deref() == Some("basic");
    if !basic {
        form.push(("client_id".into(), id.clone()));
        form.push(("client_secret".into(), secret.clone()));
    }
    let token_url = vars.render_url(token_url).map_err(AppError::BadRequest)?;
    // Count the request before sending it: a token the app issued counts against its cap even if we never read the answer.
    let mut all = recent.clone();
    all.push(Utc::now());
    sqlx::query("UPDATE integrations SET oauth_token_requests = $2 WHERE id = $1").bind(integration_id).bind(&all).execute(&mut *tx).await?;
    let (host, addr) = vet(&token_url).await?;
    let client = reqwest::Client::builder()
        .resolve(&host, addr)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .user_agent("dumbmail-integrations/0.1")
        .build()
        .map_err(|e| AppError::Internal(e.into()))?;
    let mut b = client.post(&token_url).header("accept", "application/json").form(&form);
    if basic {
        b = b.basic_auth(&id, Some(&secret));
    }
    let sent = b.send().await;
    let res = match sent {
        Ok(r) => read(r).await,
        Err(e) => Err(AppError::bad(format!("couldn't reach the token URL: {}", e.without_url()))),
    };
    let res = match res {
        Ok(r) => r,
        Err(e) => {
            tx.commit().await?;
            return Err(e);
        }
    };
    if !(200..300).contains(&res.status) {
        tx.commit().await?;
        let detail: String = res.body.chars().take(300).collect();
        return Err(AppError::bad(format!("the token URL answered HTTP {}: {detail}", res.status)));
    }
    let v = res.json();
    let Some(token) = v["access_token"].as_str().filter(|t| !t.is_empty()).map(String::from) else {
        tx.commit().await?;
        return Err(AppError::bad("the token URL's answer has no access_token"));
    };
    let ttl = v["expires_in"].as_i64().or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok())).unwrap_or(3600).clamp(60, 30 * 86400);
    sqlx::query("UPDATE integrations SET oauth_token_enc = $2, oauth_token_expires_at = now() + make_interval(secs => $3) WHERE id = $1")
        .bind(integration_id)
        .bind(crate::crypto::encrypt(&state.cfg.secret_key, &token)?)
        .bind(ttl as f64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(token))
}

/// Forget a cached token the app no longer accepts.
pub async fn drop_token(state: &AppState, integration_id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE integrations SET oauth_token_enc = NULL, oauth_token_expires_at = NULL WHERE id = $1").bind(integration_id).execute(&state.db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(secrets: &'a Map<String, Value>, st: &'a Map<String, Value>, settings: &'a Map<String, Value>, redact: bool) -> Vars<'a> {
        let now = DateTime::parse_from_rfc3339("2026-11-17T12:00:00Z").unwrap().with_timezone(&Utc);
        Vars { secrets, state: st, settings, now, since: now - chrono::Duration::days(1), redact }
    }

    #[test]
    fn renders_placeholders_with_fallbacks() {
        let secrets = json!({"key": "sk_live_1"}).as_object().cloned().unwrap();
        let st = json!({"cursor": "c9"}).as_object().cloned().unwrap();
        let empty = Map::new();
        let v = vars(&secrets, &st, &empty, false);
        assert_eq!(v.render("Bearer {{secret.key}}").unwrap(), "Bearer sk_live_1");
        assert_eq!(v.render("{{ state.cursor }}").unwrap(), "c9");
        assert_eq!(v.render("{{state.missing|since}}").unwrap(), "2026-11-16T12:00:00.000Z");
        assert_eq!(v.render("{{state.missing|'0'}}").unwrap(), "0");
        assert!(v.render("{{state.missing}}").unwrap_err().contains("fallback"));
        assert!(v.render("{{secret.other}}").unwrap_err().contains("isn't saved"));
        assert!(v.render("{{env.HOME}}").unwrap_err().contains("unknown placeholder"));
        let r = vars(&secrets, &st, &empty, true);
        assert_eq!(r.render("Bearer {{secret.key}}").unwrap(), "Bearer •••");
    }

    #[test]
    fn next_request_stays_on_the_same_site_and_never_expands_secrets() {
        let cfg = PullConfig::parse(&json!({"url": "https://api.example.com/v1/items", "query": {"limit": 100, "since": "{{since}}"}, "auth": {"type": "bearer", "token": "{{secret.key}}"}})).unwrap();
        let secrets = json!({"key": "sk"}).as_object().cloned().unwrap();
        let empty = Map::new();
        let v = vars(&secrets, &empty, &empty, false);
        let next = NextRequest { query: Some(json!({"skip": 100, "since": "{{secret.key}}"}).as_object().cloned().unwrap()), ..Default::default() };
        let r = build(&cfg, &next, &v).unwrap();
        assert!(r.query.contains(&("skip".into(), "100".into())));
        assert!(r.query.contains(&("since".into(), "{{secret.key}}".into())), "literal, not expanded");
        assert!(r.headers.contains(&("Authorization".into(), "Bearer sk".into())));
        let away = NextRequest { url: Some("https://evil.example.net/steal".into()), ..Default::default() };
        assert!(build(&cfg, &away, &v).unwrap_err().contains("must stay on"));
        let rel = NextRequest { url: Some("/v1/items?page=2".into()), ..Default::default() };
        assert_eq!(build(&cfg, &rel, &v).unwrap().url, "https://api.example.com/v1/items?page=2");
        let hdr = NextRequest { headers: Some(json!({"Authorization": "x"}).as_object().cloned().unwrap()), ..Default::default() };
        assert!(build(&cfg, &hdr, &v).is_err());
    }

    #[test]
    fn saved_state_never_picks_the_site() {
        let empty = Map::new();
        let evil = json!({"host": "evil.example.net", "path": "x/../../@evil.example.net?a=1#", "page": "2"}).as_object().cloned().unwrap();
        let v = vars(&empty, &evil, &empty, false);
        assert!(v.render_url("https://{{state.host}}/v1").unwrap_err().contains("can't change"));
        assert!(v.render_url("https://api.example.com@{{state.host}}/v1").unwrap_err().contains("can't change"));
        let u = v.render_url("https://api.example.com/v1/{{state.path}}").unwrap();
        assert_eq!(url::Url::parse(&u).unwrap().host_str(), Some("api.example.com"), "{u}");
        assert!(!u.contains('@') && !u.contains('#'), "{u}");
        assert_eq!(v.render_url("https://api.example.com/v1?page={{state.page}}").unwrap(), "https://api.example.com/v1?page=2");
        let cfg = PullConfig::parse(&json!({"url": "https://{{state.host}}/x", "auth": {"type": "bearer", "token": "t"}})).unwrap();
        assert!(build(&cfg, &NextRequest::default(), &v).is_err());
    }

    #[test]
    fn validates_config() {
        assert!(PullConfig::parse(&json!({"url": "ftp://x.com"})).is_err());
        assert!(PullConfig::parse(&json!({"url": "https://x.com", "interval_minutes": 1})).is_err());
        assert!(PullConfig::parse(&json!({"url": "https://x.com", "method": "DELETE"})).is_err());
        assert!(PullConfig::parse(&json!({"url": "https://x.com", "extra": 1})).is_err());
        let c = PullConfig::parse(&json!({"url": "https://x.com/{{settings.region}}", "interval_minutes": 15,
            "auth": {"type": "oauth2_client_credentials", "token_url": "https://x.com/token", "client_id": "{{secret.client_id}}", "client_secret": "{{secret.client_secret}}"}})).unwrap();
        assert_eq!(c.secret_names(), vec!["client_id", "client_secret"]);
    }
}
