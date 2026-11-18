//! Pulls: fetching data from an app on a schedule (or for a backfill), declared as data.
//!
//! The request is a template (`{{secret.NAME}}`, `{{state.KEY}}`, `{{settings.KEY}}`, `{{now}}`,
//! `{{since}}`, with `|` fallbacks) whose **origin is fixed when it is saved**: the scheme, host and
//! port are literal text (no placeholders, no user:password), HTTPS only. Pages a script asks for
//! may change only the path and query. Credentials go only in the auth block, as secret references,
//! and the access token is only ever sent to the origin it was configured for. Requests go only to
//! public internet addresses with pinned DNS, no proxy, no redirects, a timeout and a size cap.
//!
//! OAuth2 client-credentials tokens are cached encrypted, keyed to everything that determines them
//! (token URL, client, scope, audience, the pull's origin), reused until shortly before they expire,
//! and capped per day because some apps (Guesty) allow only a handful. A refresh takes a short
//! lease on the row so two runs never both ask, without holding a transaction over the network.

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
/// How long a token refresh may hold its lease.
const LEASE_SECONDS: i64 = 45;

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
    /// Pages one backfill run may fetch before it stops as "capped" (resume to continue). Default 5000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfill_max_pages: Option<u32>,
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

/// How a pull authenticates. Every credential field is a secret reference (`{{secret.NAME}}`),
/// never the value itself, so the saved settings never hold a secret.
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
        /// Literal URL on a fixed HTTPS origin (no placeholders).
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

static PLACEHOLDER: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"\{\{\s*([^{}]+?)\s*\}\}").unwrap());
static SECRET_REF: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"^\{\{\s*secret\.[A-Za-z0-9_-]{1,64}\s*\}\}$").unwrap());

/// Headers neither the saved settings nor a script may set: they decide where a request goes, who
/// it claims to be, or how it is framed. `Authorization` comes from the auth block.
pub fn forbidden_header(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    matches!(n.as_str(), "host" | "authorization" | "forwarded" | "connection" | "transfer-encoding" | "content-length" | "cookie" | "te" | "upgrade" | "keep-alive")
        || n.starts_with("proxy-")
        || n.starts_with("x-forwarded-")
}

/// Header names that usually carry a credential: their value must be a secret reference.
fn credential_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["key", "token", "secret", "auth", "password", "signature", "session"].iter().any(|w| n.contains(w))
}

fn is_secret_ref(v: &str) -> bool {
    SECRET_REF.is_match(v.trim())
}

fn require_ref(field: &str, v: &str) -> AppResult<()> {
    if is_secret_ref(v) {
        Ok(())
    } else {
        Err(AppError::bad(format!(
            "pull auth `{field}` must be a secret reference like {{{{secret.{field}}}}}, not the value itself; save the value under credentials"
        )))
    }
}

impl PullConfig {
    pub fn parse(v: &Value) -> AppResult<Self> {
        let c: PullConfig = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid pull settings: {e}")))?;
        if !matches!(c.method.as_str(), "GET" | "POST") {
            return Err(AppError::bad("pull method must be GET or POST"));
        }
        check_url(&c.url, "the pull URL")?;
        if let Some(i) = c.interval_minutes {
            if !(MIN_INTERVAL_MINUTES..=7 * 24 * 60).contains(&i) {
                return Err(AppError::bad(format!("pull interval_minutes must be {MIN_INTERVAL_MINUTES} to 10080")));
            }
        }
        if c.max_pages.is_some_and(|p| !(1..=50).contains(&p)) {
            return Err(AppError::bad("pull max_pages must be 1 to 50"));
        }
        if c.backfill_max_pages.is_some_and(|p| !(1..=5000).contains(&p)) {
            return Err(AppError::bad("pull backfill_max_pages must be 1 to 5000"));
        }
        if c.headers.len() > 30 || c.query.len() > 50 {
            return Err(AppError::bad("at most 30 headers and 50 query parameters"));
        }
        for (k, v) in c.headers.iter().chain(c.query.iter()) {
            if !(v.is_string() || v.is_number() || v.is_boolean()) {
                return Err(AppError::bad(format!("pull header/query {k:?} must be a string, number or true/false")));
            }
        }
        for (k, v) in &c.headers {
            if axum::http::HeaderName::from_bytes(k.as_bytes()).is_err() {
                return Err(AppError::bad(format!("{k:?} is not a valid header name")));
            }
            if forbidden_header(k) {
                return Err(AppError::bad(format!("pull headers can't set {k:?}; credentials go in the auth block")));
            }
            let text = plain(v).unwrap_or_default();
            if credential_header(k) && !text.contains("{{secret.") {
                return Err(AppError::bad(format!("header {k:?} looks like a credential: save the value under credentials and use {{{{secret.NAME}}}} here")));
            }
        }
        if serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) > 32 * 1024 {
            return Err(AppError::bad("pull settings are over 32 KB"));
        }
        match &c.auth {
            Auth::None => {}
            Auth::Bearer { token } => require_ref("token", token)?,
            Auth::Basic { username, password } => {
                require_ref("username", username)?;
                require_ref("password", password)?;
            }
            Auth::Header { name, value } => {
                if axum::http::HeaderName::from_bytes(name.as_bytes()).is_err() || (forbidden_header(name) && !name.eq_ignore_ascii_case("authorization")) {
                    return Err(AppError::bad(format!("{name:?} can't be the auth header")));
                }
                require_ref("value", value)?;
            }
            Auth::Oauth2ClientCredentials { token_url, client_id, client_secret, scope, audience, client_auth, max_tokens_per_day } => {
                check_url(token_url, "the token URL")?;
                if PLACEHOLDER.is_match(token_url) {
                    return Err(AppError::bad("the token URL is fixed text; it can't use placeholders"));
                }
                require_ref("client_id", client_id)?;
                require_ref("client_secret", client_secret)?;
                for (f, v) in [("scope", scope), ("audience", audience)] {
                    if v.as_deref().is_some_and(|s| PLACEHOLDER.is_match(s) || s.len() > 500) {
                        return Err(AppError::bad(format!("oauth2 {f} is fixed text (no placeholders, up to 500 characters)")));
                    }
                }
                if client_auth.as_deref().is_some_and(|a| !matches!(a, "body" | "basic")) {
                    return Err(AppError::bad("oauth2 client_auth must be body or basic"));
                }
                if max_tokens_per_day.is_some_and(|m| !(1..=100).contains(&m)) {
                    return Err(AppError::bad("oauth2 max_tokens_per_day must be 1 to 100"));
                }
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

    /// The origin the pull (and its access token) is bound to, e.g. `https://open-api.guesty.com`.
    pub fn origin(&self) -> String {
        url::Url::parse(&PLACEHOLDER.replace_all(&self.url, "x")).map(|u| u.origin().ascii_serialization()).unwrap_or_default()
    }
}

/// A saved URL template: literal scheme, host and port (no placeholders, no user:password), HTTPS
/// (HTTP only for a local test server).
fn check_url(u: &str, what: &str) -> AppResult<()> {
    if u.len() > 2000 {
        return Err(AppError::bad(format!("{what} is too long")));
    }
    let Some((scheme, rest)) = u.split_once("://") else { return Err(AppError::bad(format!("{what} {u:?} is not a valid URL"))) };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if scheme.contains("{{") || authority.contains("{{") || authority.contains("}}") {
        return Err(AppError::bad(format!("{what}'s site (scheme, host and port) is fixed text; placeholders may go only in the path or query")));
    }
    let base = PLACEHOLDER.replace_all(u, "x");
    let parsed = url::Url::parse(&base).map_err(|_| AppError::bad(format!("{what} {u:?} is not a valid URL")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() || authority.contains('@') {
        return Err(AppError::bad(format!("{what} can't include a user name or password; use the auth block")));
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if is_test_origin(&parsed) => Ok(()),
        _ => Err(AppError::bad(format!("{what} must use https"))),
    }
}

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

impl Vars<'_> {
    fn lookup(&self, name: &str, encode: bool) -> Result<Option<String>, String> {
        let name = name.trim();
        if let Some(lit) = name.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            return Ok(Some(lit.to_string()));
        }
        let iso = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let enc = |v: String| if encode { url::form_urlencoded::byte_serialize(v.as_bytes()).collect() } else { v };
        Ok(match name {
            "now" => Some(iso(self.now)),
            "since" => Some(iso(self.since)),
            "now_unix" => Some(self.now.timestamp().to_string()),
            "since_unix" => Some(self.since.timestamp().to_string()),
            _ => {
                if let Some(k) = name.strip_prefix("secret.") {
                    match self.secrets.get(k).and_then(plain) {
                        Some(_) if self.redact => Some("•••".into()),
                        Some(v) => Some(enc(v)),
                        None => None,
                    }
                } else if let Some(k) = name.strip_prefix("state.") {
                    // State comes from script output (and so from payloads): in a URL it is percent-encoded.
                    self.state.get(k).and_then(plain).map(enc)
                } else if let Some(k) = name.strip_prefix("settings.") {
                    self.settings.get(k).and_then(plain).map(enc)
                } else {
                    return Err(format!("unknown placeholder {{{{{name}}}}} (use secret.NAME, state.KEY, settings.KEY, now, since or a 'quoted' fallback)"));
                }
            }
        })
    }

    pub fn render(&self, s: &str) -> Result<String, String> {
        self.render_with(s, false)
    }

    /// Render a URL template. Its site is literal text (checked when saved); values are
    /// percent-encoded, and the result must still be on that site.
    pub fn render_url(&self, s: &str) -> Result<String, String> {
        let out = self.render_with(s, true)?;
        let want = url::Url::parse(&PLACEHOLDER.replace_all(s, "x")).map(|u| u.origin()).map_err(|_| "the URL isn't valid".to_string())?;
        let got = url::Url::parse(&out).map_err(|_| "the rendered URL isn't valid".to_string())?;
        if got.origin() != want || !got.username().is_empty() || got.password().is_some() {
            return Err("a placeholder can't change the URL's site".into());
        }
        Ok(out)
    }

    fn render_with(&self, s: &str, encode: bool) -> Result<String, String> {
        let mut err = None;
        let out = PLACEHOLDER.replace_all(s, |c: &regex::Captures<'_>| {
            let expr = &c[1];
            for alt in expr.split('|') {
                match self.lookup(alt, encode) {
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
    /// What the delivery log shows (secrets already masked when rendered with `redact`; auth
    /// headers are never shown).
    pub fn log_json(&self) -> Value {
        let headers: Map<String, Value> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), json!(if k.eq_ignore_ascii_case("authorization") || credential_header(k) { "•••" } else { v.as_str() })))
            .collect();
        json!({"method": self.method, "url": self.url, "query": self.query.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(), "headers": headers, "body": self.body})
    }

    /// Where it went, for error messages: origin and path, with query values hidden.
    pub fn place(&self) -> String {
        place_of(&self.url, self.query.iter().map(|(k, _)| k.as_str()))
    }
}

fn place_of<'a>(raw: &str, extra_keys: impl Iterator<Item = &'a str>) -> String {
    let Ok(u) = url::Url::parse(raw) else { return "the app".into() };
    let mut keys: Vec<String> = u.query_pairs().map(|(k, _)| k.into_owned()).collect();
    keys.extend(extra_keys.map(String::from));
    keys.dedup();
    let q = if keys.is_empty() { String::new() } else { format!("?{}", keys.iter().map(|k| format!("{k}=…")).collect::<Vec<_>>().join("&")) };
    format!("{}{}{q}", u.origin().ascii_serialization(), u.path())
}

/// Build the request for one page: the config's templates rendered, then the script's
/// `next_request` changes applied as literal values (they never see or expand secrets). A script
/// may change the path and query only; the site stays the saved one.
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
        if u.contains('@') && (u.contains("://") || u.starts_with("//")) {
            return Err("next_request url can't include a user name or password".into());
        }
        let joined = base.join(u).map_err(|_| "next_request url isn't valid".to_string())?;
        if joined.origin() != base.origin() || !joined.username().is_empty() || joined.password().is_some() {
            return Err(format!("next_request may change only the path and query; it must stay on {}", base.origin().ascii_serialization()));
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
            if forbidden_header(k) || axum::http::HeaderName::from_bytes(k.as_bytes()).is_err() {
                return Err(format!("next_request can't set the {k:?} header"));
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

// ---------- Errors and redaction ----------

/// Mask every secret value (credentials, signing secret, access token, the encoded Basic value)
/// wherever it appears in text that is stored or shown, as it is and as it appears in a URL.
/// Secrets of four bytes or more are masked wherever they appear; shorter ones (a PIN, say) only
/// as a whole token, so that "12" doesn't mask every 12 in a page of numbers.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    let mut list: Vec<String> = Vec::new();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        list.push(s.clone());
        let form: String = url::form_urlencoded::byte_serialize(s.as_bytes()).collect();
        list.push(form.replace('+', "%20"));
        list.push(form);
    }
    list.sort_by_key(|s| std::cmp::Reverse(s.len()));
    list.dedup();
    for s in list {
        if s.len() >= 4 {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), "•••");
            }
        } else {
            out = replace_whole_token(&out, &s);
        }
    }
    out
}

/// Replace `needle` where it stands alone: not touching a letter, digit or `_` on either side.
fn replace_whole_token(text: &str, needle: &str) -> String {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(needle) {
        let before = if i > 0 { rest[..i].chars().last() } else { out.chars().last() };
        let after = rest[i + needle.len()..].chars().next();
        out.push_str(&rest[..i]);
        if word(before) || word(after) {
            out.push_str(needle);
        } else {
            out.push_str("•••");
        }
        rest = &rest[i + needle.len()..];
    }
    out.push_str(rest);
    out
}

/// Everything secret about one run: the integration's saved secrets plus what its requests
/// actually sent (the bearer token, the encoded Basic value, credential headers). Applied at the
/// boundary where anything is stored or returned: delivery bodies, response headers (`Link`
/// included, on any status), the request shown, errors, script output and test results.
#[derive(Debug, Clone, Default)]
pub struct Scrub {
    secrets: Vec<String>,
}

impl Scrub {
    pub fn new(secrets: Vec<String>) -> Self {
        let mut s = Scrub { secrets: Vec::new() };
        for x in secrets {
            s.add(&x);
        }
        s
    }

    pub fn add(&mut self, secret: &str) {
        let t = secret.trim();
        if !t.is_empty() && !self.secrets.iter().any(|x| x == t) {
            self.secrets.push(t.to_string());
        }
    }

    /// Add the credentials a request carries (auth headers, whole and without the scheme word).
    pub fn add_request(&mut self, req: &Request) {
        for (k, v) in &req.headers {
            if k.eq_ignore_ascii_case("authorization") || credential_header(k) {
                self.add(v);
                if let Some((_, rest)) = v.split_once(' ') {
                    self.add(rest);
                }
            }
        }
    }

    pub fn text(&self, t: &str) -> String {
        redact(t, &self.secrets)
    }

    /// Every string in a JSON value (and every key), masked.
    pub fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (self.text(k), self.value(x))).collect()),
            other => other.clone(),
        }
    }

    /// Whether a value holds one of the secrets (so it mustn't be saved as it is).
    pub fn finds_in(&self, v: &Value) -> bool {
        self.value(v) != *v
    }
}

/// A short description of an HTTP status, for error messages (never the app's own text).
pub fn status_class(status: u16) -> &'static str {
    match status {
        400 => "bad request",
        401 => "unauthorized: check the credentials",
        403 => "forbidden: the credentials lack permission",
        404 => "not found: check the URL",
        408 | 504 => "timed out",
        409 => "conflict",
        422 => "the app rejected the request",
        429 => "rate limited: the app asked us to slow down",
        500..=599 => "the app had an error",
        _ => "unexpected answer",
    }
}

fn reqwest_class(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "couldn't connect"
    } else if e.is_body() || e.is_decode() {
        "the answer was cut off"
    } else {
        "the request failed"
    }
}

// ---------- Outbound HTTP ----------

/// Addresses tests may reach despite being on loopback (a local mock of an app's API). Empty in production.
static TEST_ADDRESSES: Lazy<Mutex<HashSet<SocketAddr>>> = Lazy::new(Default::default);

/// Let requests reach one local test server (plain HTTP allowed there). For tests only; production never calls this.
#[doc(hidden)]
pub fn allow_test_address(addr: SocketAddr) {
    TEST_ADDRESSES.lock().unwrap().insert(addr);
}

fn test_addr(u: &url::Url) -> Option<SocketAddr> {
    if let (Some(url::Host::Ipv4(ip)), Some(port)) = (u.host(), u.port_or_known_default()) {
        let addr = SocketAddr::new(ip.into(), port);
        if TEST_ADDRESSES.lock().unwrap().contains(&addr) {
            return Some(addr);
        }
    }
    None
}

fn is_test_origin(u: &url::Url) -> bool {
    test_addr(u).is_some()
}

/// The SSRF-safe address check (`flows::check_webhook_url`): public addresses only, pinned.
async fn vet(u: &url::Url) -> AppResult<(String, SocketAddr)> {
    if let Some(addr) = test_addr(u) {
        return Ok((addr.ip().to_string(), addr));
    }
    if u.scheme() != "https" {
        return Err(AppError::bad("pull URLs must use https"));
    }
    crate::flows::check_webhook_url(u.as_str()).await.map_err(|_| AppError::bad("pull URLs must point to a public internet address"))
}

fn client(host: &str, addr: SocketAddr) -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .resolve(host, addr)
        .redirect(reqwest::redirect::Policy::none())
        // No proxy: the pinned, checked address is the one connected to.
        .no_proxy()
        .timeout(TIMEOUT)
        .user_agent("dumbmail-integrations/0.1")
        .build()
        .map_err(|e| AppError::Internal(e.into()))
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

/// Send a request to a public address: pinned DNS, no proxy, no redirects, a timeout and a size cap.
/// `bearer` is the access token for this pull's own origin.
pub async fn send(req: &Request, bearer: Option<&str>) -> AppResult<Response> {
    let mut url = url::Url::parse(&req.url).map_err(|_| AppError::bad("the pull URL isn't valid"))?;
    if !req.query.is_empty() {
        let keep: Vec<(String, String)> = url.query_pairs().filter(|(k, _)| !req.query.iter().any(|(q, _)| q == k)).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        url.query_pairs_mut().clear().extend_pairs(keep).extend_pairs(req.query.iter());
    }
    let (host, addr) = vet(&url).await?;
    let client = client(&host, addr)?;
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
    let res = b.send().await.map_err(|e| AppError::bad(format!("couldn't reach {} ({})", req.place(), reqwest_class(&e))))?;
    read(res, &req.place()).await
}

async fn read(res: reqwest::Response, place: &str) -> AppResult<Response> {
    let status = res.status().as_u16();
    if res.status().is_redirection() {
        return Err(AppError::bad(format!("{place} answered with a redirect (HTTP {status}); redirects aren't followed, so use the final address")));
    }
    let keep = ["content-type", "link", "x-total-count", "ratelimit-remaining", "ratelimit-reset", "retry-after", "x-ratelimit-remaining"];
    let headers: Map<String, Value> = res
        .headers()
        .iter()
        .filter(|(k, _)| keep.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str().to_string(), json!(v.to_str().unwrap_or("").chars().take(500).collect::<String>())))
        .collect();
    let mut body = Vec::new();
    let mut res = res;
    while let Some(chunk) = res.chunk().await.map_err(|e| AppError::bad(format!("couldn't read the answer from {place} ({})", reqwest_class(&e))))? {
        body.extend_from_slice(&chunk);
        if body.len() > RESPONSE_MAX_BYTES {
            return Err(AppError::bad(format!("the answer from {place} is over {} MB; ask for smaller pages", RESPONSE_MAX_BYTES / (1024 * 1024))));
        }
    }
    Ok(Response { status, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

// ---------- OAuth2 client credentials ----------

/// Who is asking for a token: the integration, the version of its settings the run started with,
/// and the origin its pull (and so the token) is for.
pub struct TokenFor<'a> {
    pub integration_id: Uuid,
    pub auth_version: i64,
    pub origin: &'a str,
}

fn sha256_hex(s: &str) -> String {
    crate::auth::sha256_hex(s)
}

/// A bearer token for an OAuth2 client-credentials pull: the cached one when it was issued for
/// exactly these settings and has more than five minutes left, otherwise a new one (unless today's
/// cap is reached). Only one run refreshes at a time (a short lease on the row; nobody holds a
/// transaction or a connection over the network), and a run whose settings changed meanwhile never
/// writes its token into the cache.
pub async fn oauth_token(state: &AppState, who: &TokenFor<'_>, auth: &Auth, vars: &Vars<'_>) -> AppResult<Option<String>> {
    let Auth::Oauth2ClientCredentials { token_url, client_id, client_secret, scope, audience, client_auth, max_tokens_per_day } = auth else { return Ok(None) };
    let id = vars.render(client_id).map_err(AppError::BadRequest)?;
    let secret = vars.render(client_secret).map_err(AppError::BadRequest)?;
    let key = sha256_hex(&json!([token_url, id, sha256_hex(&secret), scope, audience, client_auth, who.origin]).to_string());
    let lease = Uuid::now_v7();
    let cap = max_tokens_per_day.unwrap_or(4) as usize;
    let started = std::time::Instant::now();
    loop {
        let mut tx = state.db.begin().await?;
        let row: (Option<String>, Option<DateTime<Utc>>, Option<String>, Vec<DateTime<Utc>>, Option<DateTime<Utc>>, i64) = sqlx::query_as(
            "SELECT oauth_token_enc, oauth_token_expires_at, oauth_token_key, oauth_token_requests, oauth_lease_until, auth_version FROM integrations WHERE id = $1 FOR UPDATE",
        )
        .bind(who.integration_id)
        .fetch_one(&mut *tx)
        .await?;
        let (cached, expires, cached_key, requests, lease_until, version) = row;
        if version != who.auth_version {
            tx.commit().await?;
            return Err(AppError::Conflict("the pull settings changed while this run was starting; the next run uses the new settings".into()));
        }
        if let (Some(enc), Some(exp), Some(k)) = (&cached, expires, &cached_key) {
            if *k == key && exp > Utc::now() + chrono::Duration::minutes(5) {
                tx.commit().await?;
                return Ok(Some(crate::crypto::decrypt(&state.cfg.secret_key, enc)?));
            }
        }
        if lease_until.is_some_and(|t| t > Utc::now()) {
            // Another run is asking for a token right now: wait for its answer.
            tx.commit().await?;
            if started.elapsed() > Duration::from_secs(LEASE_SECONDS as u64 + 5) {
                return Err(AppError::Conflict("another run is still getting an access token; try again shortly".into()));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        }
        let recent: Vec<DateTime<Utc>> = requests.into_iter().filter(|t| *t > Utc::now() - chrono::Duration::hours(24)).collect();
        if recent.len() >= cap {
            tx.commit().await?;
            let next = recent.iter().min().map(|t| *t + chrono::Duration::hours(24)).unwrap_or_else(Utc::now);
            return Err(AppError::Conflict(format!(
                "already asked for {cap} access tokens in the last 24 hours (the daily cap); the next one can be requested after {}",
                next.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            )));
        }
        // Count the request before sending it: a token the app issued counts against its cap even if we never read the answer.
        let mut all = recent;
        all.push(Utc::now());
        sqlx::query("UPDATE integrations SET oauth_token_requests = $2, oauth_lease_id = $3, oauth_lease_until = now() + make_interval(secs => $4) WHERE id = $1")
            .bind(who.integration_id)
            .bind(&all)
            .bind(lease)
            .bind(LEASE_SECONDS as f64)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        break;
    }
    let res = fetch_token(token_url, &id, &secret, scope.as_deref(), audience.as_deref(), client_auth.as_deref() == Some("basic")).await;
    match res {
        Ok((token, ttl)) => {
            // Cache it only if this run still holds the lease and the settings haven't changed.
            sqlx::query(
                "UPDATE integrations SET oauth_token_enc = $3, oauth_token_expires_at = now() + make_interval(secs => $4), oauth_token_key = $5, oauth_lease_id = NULL, oauth_lease_until = NULL
                 WHERE id = $1 AND oauth_lease_id = $2 AND auth_version = $6",
            )
            .bind(who.integration_id)
            .bind(lease)
            .bind(crate::crypto::encrypt(&state.cfg.secret_key, &token)?)
            .bind(ttl as f64)
            .bind(&key)
            .bind(who.auth_version)
            .execute(&state.db)
            .await?;
            Ok(Some(token))
        }
        Err(e) => {
            sqlx::query("UPDATE integrations SET oauth_lease_id = NULL, oauth_lease_until = NULL WHERE id = $1 AND oauth_lease_id = $2").bind(who.integration_id).bind(lease).execute(&state.db).await?;
            Err(e)
        }
    }
}

async fn fetch_token(token_url: &str, id: &str, secret: &str, scope: Option<&str>, audience: Option<&str>, basic: bool) -> AppResult<(String, i64)> {
    let mut form: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
    if let Some(s) = scope {
        form.push(("scope", s));
    }
    if let Some(a) = audience {
        form.push(("audience", a));
    }
    if !basic {
        form.push(("client_id", id));
        form.push(("client_secret", secret));
    }
    let url = url::Url::parse(token_url).map_err(|_| AppError::bad("the token URL isn't valid"))?;
    let place = place_of(token_url, std::iter::empty());
    let (host, addr) = vet(&url).await?;
    let client = client(&host, addr)?;
    let mut b = client.post(url).header("accept", "application/json").form(&form);
    if basic {
        b = b.basic_auth(id, Some(secret));
    }
    let res = b.send().await.map_err(|e| AppError::bad(format!("couldn't reach the token URL {place} ({})", reqwest_class(&e))))?;
    let res = read(res, &place).await?;
    if !(200..300).contains(&res.status) {
        return Err(AppError::bad(format!("the token URL {place} answered HTTP {} ({})", res.status, status_class(res.status))));
    }
    let v = res.json();
    let Some(token) = v["access_token"].as_str().filter(|t| !t.is_empty()).map(String::from) else {
        return Err(AppError::bad(format!("the answer from the token URL {place} has no access_token")));
    };
    let ttl = v["expires_in"].as_i64().or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok())).unwrap_or(3600).clamp(60, 30 * 86400);
    Ok((token, ttl))
}

/// Forget a cached token the app no longer accepts.
pub async fn drop_token(state: &AppState, integration_id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE integrations SET oauth_token_enc = NULL, oauth_token_expires_at = NULL, oauth_token_key = NULL WHERE id = $1").bind(integration_id).execute(&state.db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(secrets: &'a Map<String, Value>, st: &'a Map<String, Value>, settings: &'a Map<String, Value>, redact: bool) -> Vars<'a> {
        let now = DateTime::parse_from_rfc3339("2026-11-17T12:00:00Z").unwrap().with_timezone(&Utc);
        Vars { secrets, state: st, settings, now, since: now - chrono::Duration::days(1), redact }
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn renders_placeholders_with_fallbacks() {
        let secrets = obj(json!({"key": "sk_live_1"}));
        let st = obj(json!({"cursor": "c9"}));
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
    fn the_site_is_fixed_text() {
        let auth = json!({"type": "bearer", "token": "{{secret.key}}"});
        for bad in [
            "https://{{state.host}}/v1",
            "https://{{settings.host}}/v1",
            "https://{{secret.host}}/v1",
            "{{settings.scheme}}://api.example.com/v1",
            "https://api.example.com:{{settings.port}}/v1",
            "https://api.example.com{{state.x}}/v1",
            "https://user:pass@api.example.com/v1",
            "https://user@api.example.com/v1",
            "https://{{secret.u}}@api.example.com/v1",
            "http://api.example.com/v1",
            "ftp://api.example.com/v1",
        ] {
            assert!(PullConfig::parse(&json!({"url": bad, "auth": auth})).is_err(), "{bad}");
        }
        let ok = PullConfig::parse(&json!({"url": "https://api.example.com/v1/{{settings.region}}/items?x={{state.cursor|since}}", "auth": auth})).unwrap();
        assert_eq!(ok.origin(), "https://api.example.com");
        // A path value can't move the request to another site either.
        let empty = Map::new();
        let evil = obj(json!({"path": "x/../../@evil.example.net?a=1#", "page": "2"}));
        let v = vars(&empty, &evil, &empty, false);
        let u = v.render_url("https://api.example.com/v1/{{state.path}}").unwrap();
        assert_eq!(url::Url::parse(&u).unwrap().host_str(), Some("api.example.com"), "{u}");
        assert!(!u.contains('@') && !u.contains('#'), "{u}");
    }

    #[test]
    fn credentials_are_references_only() {
        let url = "https://api.example.com/v1";
        assert!(PullConfig::parse(&json!({"url": url, "auth": {"type": "bearer", "token": "sk_live_literal"}})).unwrap_err().public_message().contains("secret reference"));
        assert!(PullConfig::parse(&json!({"url": url, "auth": {"type": "bearer", "token": "x {{secret.key}}"}})).is_err());
        assert!(PullConfig::parse(&json!({"url": url, "auth": {"type": "basic", "username": "{{secret.u}}", "password": "hunter2"}})).is_err());
        assert!(PullConfig::parse(&json!({"url": url, "auth": {"type": "header", "name": "X-Api-Key", "value": "abc"}})).is_err());
        assert!(PullConfig::parse(&json!({"url": url, "auth": {"type": "header", "name": "X-Api-Key", "value": "{{secret.key}}"}})).is_ok());
        assert!(PullConfig::parse(&json!({"url": url, "headers": {"X-Api-Key": "literal"}})).is_err());
        assert!(PullConfig::parse(&json!({"url": url, "headers": {"X-Api-Key": "{{secret.key}}"}})).is_ok());
        let oauth = |cid: &str, turl: &str| json!({"url": url, "auth": {"type": "oauth2_client_credentials", "token_url": turl, "client_id": cid, "client_secret": "{{secret.client_secret}}"}});
        assert!(PullConfig::parse(&oauth("literal-id", "https://api.example.com/token")).is_err());
        assert!(PullConfig::parse(&oauth("{{secret.client_id}}", "https://api.example.com/{{settings.t}}")).is_err());
        assert!(PullConfig::parse(&oauth("{{secret.client_id}}", "https://{{settings.h}}/token")).is_err());
        let c = PullConfig::parse(&oauth("{{secret.client_id}}", "https://api.example.com/token")).unwrap();
        assert_eq!(c.secret_names(), vec!["client_id", "client_secret"]);
    }

    #[test]
    fn routing_headers_are_refused_everywhere() {
        for h in ["Host", "authorization", "Proxy-Authorization", "Forwarded", "X-Forwarded-Host", "X-Forwarded-For", "Connection", "Transfer-Encoding", "Content-Length", "Cookie"] {
            assert!(PullConfig::parse(&json!({"url": "https://api.example.com/v1", "headers": {h: "x"}})).is_err(), "{h}");
            let cfg = PullConfig::parse(&json!({"url": "https://api.example.com/v1"})).unwrap();
            let empty = Map::new();
            let v = vars(&empty, &empty, &empty, false);
            let next = NextRequest { headers: Some(obj(json!({h: "x"}))), ..Default::default() };
            assert!(build(&cfg, &next, &v).is_err(), "{h}");
        }
    }

    #[test]
    fn next_request_changes_only_path_and_query() {
        let cfg = PullConfig::parse(&json!({"url": "https://api.example.com/v1/items", "query": {"limit": 100, "since": "{{since}}"}, "auth": {"type": "bearer", "token": "{{secret.key}}"}})).unwrap();
        let secrets = obj(json!({"key": "sk"}));
        let empty = Map::new();
        let v = vars(&secrets, &empty, &empty, false);
        let next = NextRequest { query: Some(obj(json!({"skip": 100, "since": "{{secret.key}}"}))), ..Default::default() };
        let r = build(&cfg, &next, &v).unwrap();
        assert!(r.query.contains(&("skip".into(), "100".into())));
        assert!(r.query.contains(&("since".into(), "{{secret.key}}".into())), "literal, not expanded");
        assert!(r.headers.contains(&("Authorization".into(), "Bearer sk".into())));
        for away in ["https://evil.example.net/steal", "//evil.example.net/x", "https://api.example.com:8443/x", "http://api.example.com/x", "https://u:p@api.example.com/x", "https://u@api.example.com/x"] {
            let n = NextRequest { url: Some(away.into()), ..Default::default() };
            assert!(build(&cfg, &n, &v).is_err(), "{away}");
        }
        let rel = NextRequest { url: Some("/v1/items?page=2".into()), ..Default::default() };
        assert_eq!(build(&cfg, &rel, &v).unwrap().url, "https://api.example.com/v1/items?page=2");
    }

    #[test]
    fn errors_hide_query_values_and_secrets() {
        assert_eq!(place_of("https://api.example.com/v1/items?token=abc&page=2", std::iter::once("since")), "https://api.example.com/v1/items?token=…&page=…&since=…");
        assert_eq!(redact("Bearer sk_live_123 and sk_live_123x", &["sk_live_123".into(), "ab".into()]), "Bearer ••• and •••x");
        // Secrets under four bytes are masked as whole tokens only.
        assert_eq!(redact("pin=12&page=123&x=12", &["12".into()]), "pin=•••&page=123&x=•••");
        assert_eq!(redact("abc", &["abc".into()]), "•••");
        assert_eq!(redact("abcd abc", &["abc".into()]), "abcd •••");
        // The Basic value and a bearer token a request sent, wherever they're echoed.
        let req = Request { method: "GET".into(), url: "https://a.example/x".into(), headers: vec![("Authorization".into(), "Basic dTpw".into()), ("X-Api-Key".into(), "k9".into())], query: vec![], body: None, form: false };
        let mut sc = Scrub::new(vec![]);
        sc.add_request(&req);
        sc.add("tok_live_1");
        let v = sc.value(&json!({"link": "<https://a.example/x?page=2&access_token=tok_live_1>; rel=\"next\"", "echo": ["auth was Basic dTpw", "key k9 ok"]}));
        assert_eq!(v, json!({"link": "<https://a.example/x?page=2&access_token=•••>; rel=\"next\"", "echo": ["auth was •••", "key ••• ok"]}));
        // A secret in a URL path is percent-encoded there; it's masked too.
        assert_eq!(redact("https://api.example.com/v1/k%2Fx%3Dy%20z/items", &["k/x=y z".into()]), "https://api.example.com/v1/•••/items");
    }
}
