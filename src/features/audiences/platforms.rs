//! The three ad platforms' APIs: OAuth, ad accounts, and customer-list audiences.
//!
//! - Meta Marketing API (Graph v27.0): Facebook Login, `/me/adaccounts`, Custom Audiences from a
//!   customer list (`/act_{id}/customaudiences`, `/{audience}/users`).
//! - Google: OAuth 2.0 with PKCE and offline refresh tokens; Customer Match through the Data Manager
//!   API (`userLists`, `audienceMembers:ingest` / `:remove`, `requestStatus:retrieve`).
//! - TikTok API for Business (v1.3): advertiser authorization, customer file audiences
//!   (`/dmp/custom_audience/file/upload/`, `create`, `update` with APPEND / REMOVE, `delete`).
//!
//! Requests go only to the platforms' own HTTPS hosts (an allowlist checked on every request), with
//! no redirects, no proxy and capped response sizes. A platform's own error text is never kept:
//! every failure is reduced to a kind and a code from a fixed list.

use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Meta,
    Google,
    Tiktok,
}

impl Platform {
    pub const ALL: [Platform; 3] = [Platform::Meta, Platform::Google, Platform::Tiktok];

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Meta => "meta",
            Platform::Google => "google",
            Platform::Tiktok => "tiktok",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Platform::Meta => "Meta",
            Platform::Google => "Google Ads",
            Platform::Tiktok => "TikTok",
        }
    }

    /// Identifiers per platform request: Meta and Google take at most 10,000; TikTok takes a file.
    pub fn batch_size(self) -> i64 {
        match self {
            Platform::Meta | Platform::Google => 10_000,
            Platform::Tiktok => 50_000,
        }
    }

    /// The smallest audience the platform keeps: TikTok refuses changes that leave fewer than this.
    pub fn minimum(self) -> i64 {
        match self {
            Platform::Tiktok => 1_000,
            _ => 0,
        }
    }
}

/// The operator's app credentials for a platform, when configured.
pub struct App<'a> {
    pub id: &'a str,
    pub secret: &'a str,
}

fn set(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub fn app(cfg: &Config, p: Platform) -> Option<App<'_>> {
    let (id, secret) = match p {
        Platform::Meta => (set(&cfg.meta_app_id)?, set(&cfg.meta_app_secret)?),
        Platform::Google => (set(&cfg.google_ads_client_id)?, set(&cfg.google_ads_client_secret)?),
        Platform::Tiktok => (set(&cfg.tiktok_app_id)?, set(&cfg.tiktok_app_secret)?),
    };
    Some(App { id, secret })
}

const META_VERSION: &str = "v27.0";

/// The only hosts DumbMail sends ad platform requests to.
pub const ALLOWED_HOSTS: &[&str] = &["graph.facebook.com", "oauth2.googleapis.com", "datamanager.googleapis.com", "business-api.tiktok.com"];

/// Response bodies larger than this are treated as unreadable.
const MAX_BODY: usize = 2 * 1024 * 1024;

struct Hosts {
    meta: String,
    google_token: String,
    google_revoke: String,
    datamanager: String,
    tiktok: String,
}

/// A loopback stand-in for all three platforms. `Config::ad_platform_api_base` can't be set from
/// the environment or the command line (tests set it in code), and only loopback HTTP is accepted.
fn test_base(cfg: &Config) -> Option<&str> {
    loopback(cfg.ad_platform_api_base.as_deref())
}

fn loopback(base: Option<&str>) -> Option<&str> {
    base.map(str::trim).filter(|b| b.starts_with("http://127.0.0.1:") || b.starts_with("http://localhost:"))
}

fn hosts(cfg: &Config) -> Hosts {
    hosts_for(test_base(cfg))
}

fn hosts_for(base: Option<&str>) -> Hosts {
    match base {
        Some(b) => {
            let b = b.trim_end_matches('/');
            Hosts {
                meta: format!("{b}/meta"),
                google_token: format!("{b}/google/token"),
                google_revoke: format!("{b}/google/revoke"),
                datamanager: format!("{b}/datamanager"),
                tiktok: format!("{b}/tiktok"),
            }
        }
        None => Hosts {
            meta: format!("https://graph.facebook.com/{META_VERSION}"),
            google_token: "https://oauth2.googleapis.com/token".into(),
            google_revoke: "https://oauth2.googleapis.com/revoke".into(),
            datamanager: "https://datamanager.googleapis.com/v1".into(),
            tiktok: "https://business-api.tiktok.com/open_api/v1.3".into(),
        },
    }
}

fn client(cfg: &Config) -> &'static reqwest::Client {
    static PROD: OnceLock<reqwest::Client> = OnceLock::new();
    static LOOPBACK: OnceLock<reqwest::Client> = OnceLock::new();
    let build = |https_only: bool| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .https_only(https_only)
            .user_agent("dumbmail/0.1")
            .build()
            .expect("http client")
    };
    if test_base(cfg).is_some() { LOOPBACK.get_or_init(|| build(false)) } else { PROD.get_or_init(|| build(true)) }
}

/// Whether a request may go to this URL.
pub fn allowed_url(cfg: &Config, url: &reqwest::Url) -> bool {
    url_allowed(test_base(cfg), url)
}

fn url_allowed(base: Option<&str>, url: &reqwest::Url) -> bool {
    if base.is_some() {
        return url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    }
    url.scheme() == "https" && url.port().is_none() && url.username().is_empty() && url.password().is_none() && url.host_str().is_some_and(|h| ALLOWED_HOSTS.contains(&h))
}

// ---------- Errors ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Slow down and try again later.
    RateLimited,
    /// The sign-in expired or was revoked: reconnect.
    Auth,
    /// The ad account doesn't allow this (permissions, terms not accepted).
    Permission,
    /// The platform refused the request.
    Refused,
    /// The platform says the audience doesn't exist (which can be wrong: DumbMail keeps its records).
    NotFound,
    /// A platform outage, or the request never got there: try again.
    Transient,
    /// The request may have reached the platform, and the answer couldn't be read.
    Unclear,
    /// This server has no app credentials for the platform.
    Unavailable,
}

/// A platform call that didn't work: a kind, and a code from a fixed list (see `words`). The
/// platform's own text is never kept, logged or shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PErr {
    pub kind: Kind,
    pub code: &'static str,
}

impl PErr {
    const fn new(kind: Kind, code: &'static str) -> Self {
        PErr { kind, code }
    }
}

pub type PResult<T> = Result<T, PErr>;

/// Every code a platform failure can be reduced to.
pub const ERROR_CODES: &[&str] = &[
    "rate_limited", "token_expired", "permission_denied", "meta_terms", "meta_business", "refused", "audience_missing", "platform_down",
    "unreadable_answer", "network", "not_available", "lookalikes_depend", "no_offline_access",
];

/// Plain words for a platform error code.
pub fn words(p: Platform, code: &str) -> String {
    let l = p.label();
    match code {
        "rate_limited" => format!("{l} asked DumbMail to slow down. It will try again shortly."),
        "token_expired" => format!("The {l} sign-in expired or was revoked. Reconnect {l} in Settings → Ad audiences."),
        "permission_denied" => format!("The person who connected {l} can't manage audiences in this ad account. Give them that permission on {l}, then reconnect."),
        "meta_terms" => "Accept Meta's Custom Audience terms for this ad account in Ads Manager, then sync again.".into(),
        "meta_business" => "This ad account needs to belong to a Meta business portfolio before it can use customer list audiences.".into(),
        "refused" => format!("{l} refused the request."),
        "audience_missing" => format!("{l} says this audience no longer exists."),
        "platform_down" => format!("{l} had a temporary problem. DumbMail will try again."),
        "unreadable_answer" => format!("{l}'s answer couldn't be read, so DumbMail doesn't count it as done. It will try again."),
        "network" => format!("Couldn't reach {l}. DumbMail will try again."),
        "not_available" => format!("{l} isn't available on this server."),
        "lookalikes_depend" => "Meta won't delete this audience while lookalike audiences are built from it. Delete those first.".into(),
        "no_offline_access" => "Google didn't allow offline access. Connect again and allow every permission.".into(),
        _ => format!("{l} returned an error."),
    }
}

fn unavailable() -> PErr {
    PErr::new(Kind::Unavailable, "not_available")
}

fn unclear() -> PErr {
    PErr::new(Kind::Unclear, "unreadable_answer")
}

/// Send one request: the URL must be on the allowlist; the body is read up to `MAX_BODY`. Returns
/// the status and the JSON body (`Null` when it isn't JSON). A network failure after the request
/// may have been sent is `Unclear`.
async fn send(state: &AppState, p: Platform, rb: reqwest::RequestBuilder) -> PResult<(u16, Value)> {
    let req = rb.build().map_err(|_| PErr::new(Kind::Refused, "refused"))?;
    if !allowed_url(&state.cfg, req.url()) {
        tracing::error!(platform = p.as_str(), "refused an ad platform request to a host outside the allowlist");
        return Err(PErr::new(Kind::Refused, "refused"));
    }
    let mut res = client(&state.cfg).execute(req).await.map_err(|e| {
        tracing::warn!(platform = p.as_str(), connect = e.is_connect(), timeout = e.is_timeout(), "ad platform request failed");
        if e.is_connect() { PErr::new(Kind::Transient, "network") } else { unclear() }
    })?;
    let status = res.status().as_u16();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match res.chunk().await {
            Ok(Some(c)) => {
                if buf.len() + c.len() > MAX_BODY {
                    return Err(unclear());
                }
                buf.extend_from_slice(&c);
            }
            Ok(None) => break,
            Err(_) => return Err(unclear()),
        }
    }
    Ok((status, serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

fn rb(state: &AppState, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
    client(&state.cfg).request(method, url)
}

// ---------- OAuth ----------

/// Tokens from a finished sign-in or a refresh.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_in: Option<i64>,
    pub connected_as: Option<String>,
}

pub fn redirect_uri(cfg: &Config, p: Platform) -> String {
    format!("{}/api/ad-audiences/oauth/{}/callback", cfg.public_url(), p.as_str())
}

pub const META_SCOPES: &str = "ads_management";
pub const GOOGLE_SCOPES: &str = "openid email https://www.googleapis.com/auth/datamanager";

/// Where to send the person to sign in. `challenge` is the PKCE S256 challenge (Google only; Meta's
/// and TikTok's authorization-code flows don't take PKCE).
pub fn authorize_url(cfg: &Config, p: Platform, state: &str, challenge: &str) -> Option<String> {
    let a = app(cfg, p)?;
    let redirect = redirect_uri(cfg, p);
    let mut url = match p {
        Platform::Meta => url::Url::parse(&format!("https://www.facebook.com/{META_VERSION}/dialog/oauth")).ok()?,
        Platform::Google => url::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").ok()?,
        Platform::Tiktok => url::Url::parse("https://business-api.tiktok.com/portal/auth").ok()?,
    };
    {
        let mut q = url.query_pairs_mut();
        match p {
            Platform::Meta => {
                q.append_pair("client_id", a.id).append_pair("redirect_uri", &redirect).append_pair("state", state).append_pair("response_type", "code").append_pair("scope", META_SCOPES);
            }
            Platform::Google => {
                q.append_pair("client_id", a.id)
                    .append_pair("redirect_uri", &redirect)
                    .append_pair("response_type", "code")
                    .append_pair("scope", GOOGLE_SCOPES)
                    .append_pair("access_type", "offline")
                    .append_pair("prompt", "consent")
                    .append_pair("state", state)
                    .append_pair("code_challenge", challenge)
                    .append_pair("code_challenge_method", "S256");
            }
            Platform::Tiktok => {
                q.append_pair("app_id", a.id).append_pair("state", state).append_pair("redirect_uri", &redirect);
            }
        }
    }
    Some(url.into())
}

pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Meta's appsecret_proof: proves calls come from the app's server (Meta can require it).
fn appsecret_proof(secret: &str, token: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(token.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn nonempty(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Trade the callback's code for tokens.
pub async fn exchange(state: &AppState, p: Platform, code: &str, verifier: &str) -> PResult<Tokens> {
    let a = app(&state.cfg, p).ok_or_else(unavailable)?;
    let h = hosts(&state.cfg);
    let redirect = redirect_uri(&state.cfg, p);
    match p {
        Platform::Meta => {
            let req = rb(state, reqwest::Method::GET, format!("{}/oauth/access_token", h.meta)).query(&[("client_id", a.id), ("redirect_uri", &redirect), ("client_secret", a.secret), ("code", code)]);
            let (status, v) = send(state, p, req).await?;
            let short_lived = nonempty(&meta_ok(status, &v)?["access_token"]).ok_or_else(unclear)?;
            // Trade the short-lived token (about an hour) for a long-lived one (about 60 days).
            let req = rb(state, reqwest::Method::GET, format!("{}/oauth/access_token", h.meta))
                .query(&[("grant_type", "fb_exchange_token"), ("client_id", a.id), ("client_secret", a.secret), ("fb_exchange_token", short_lived.as_str())]);
            let (status, v) = send(state, p, req).await?;
            let v = meta_ok(status, &v)?;
            let access = nonempty(&v["access_token"]).ok_or_else(unclear)?;
            let expires_in = v["expires_in"].as_i64();
            let me = meta_get(state, &access, "me", &[("fields", "name")]).await.ok();
            Ok(Tokens { access, refresh: None, expires_in, connected_as: me.and_then(|m| nonempty(&m["name"])) })
        }
        Platform::Google => {
            let req = rb(state, reqwest::Method::POST, h.google_token).form(&[
                ("code", code),
                ("client_id", a.id),
                ("client_secret", a.secret),
                ("redirect_uri", &redirect),
                ("grant_type", "authorization_code"),
                ("code_verifier", verifier),
            ]);
            let (status, v) = send(state, p, req).await?;
            let v = google_token_ok(status, &v)?;
            let access = nonempty(&v["access_token"]).ok_or_else(unclear)?;
            let refresh = nonempty(&v["refresh_token"]).ok_or(PErr::new(Kind::Refused, "no_offline_access"))?;
            let connected_as = v["id_token"].as_str().and_then(|t| crate::api::google::parse_id_token(t, a.id, chrono::Utc::now().timestamp()).ok()).map(|c| c.email);
            Ok(Tokens { access, refresh: Some(refresh), expires_in: v["expires_in"].as_i64(), connected_as })
        }
        Platform::Tiktok => {
            let req = rb(state, reqwest::Method::POST, format!("{}/oauth2/access_token/", h.tiktok)).json(&json!({"app_id": a.id, "secret": a.secret, "auth_code": code}));
            let (status, v) = send(state, p, req).await?;
            let data = tiktok_ok(status, &v)?;
            let access = nonempty(&data["access_token"]).ok_or_else(unclear)?;
            let n = data["advertiser_ids"].as_array().map(Vec::len).unwrap_or(0);
            // Marketing API advertiser tokens don't expire until revoked; take any expiry TikTok sends.
            Ok(Tokens {
                access,
                refresh: nonempty(&data["refresh_token"]),
                expires_in: data["expires_in"].as_i64(),
                connected_as: Some(format!("{n} advertiser account{}", if n == 1 { "" } else { "s" })),
            })
        }
    }
}

/// A fresh Google access token from the refresh token. Meta's long-lived tokens are not renewed on
/// the server (Meta doesn't document that for this flow): the connection asks to be reconnected
/// before it expires. TikTok's don't expire.
pub async fn refresh_google(state: &AppState, refresh: &str) -> PResult<Tokens> {
    let a = app(&state.cfg, Platform::Google).ok_or_else(unavailable)?;
    let req = rb(state, reqwest::Method::POST, hosts(&state.cfg).google_token).form(&[("client_id", a.id), ("client_secret", a.secret), ("refresh_token", refresh), ("grant_type", "refresh_token")]);
    let (status, v) = send(state, Platform::Google, req).await?;
    let v = google_token_ok(status, &v)?;
    let access = nonempty(&v["access_token"]).ok_or_else(unclear)?;
    Ok(Tokens { access, refresh: nonempty(&v["refresh_token"]), expires_in: v["expires_in"].as_i64(), connected_as: None })
}

/// Revoke DumbMail's access on the platform. Ok means the platform confirmed it.
pub async fn revoke(state: &AppState, p: Platform, access: &str, refresh: Option<&str>) -> PResult<()> {
    let a = app(&state.cfg, p).ok_or_else(unavailable)?;
    let h = hosts(&state.cfg);
    match p {
        Platform::Meta => {
            let req = rb(state, reqwest::Method::DELETE, format!("{}/me/permissions", h.meta)).query(&[("access_token", access), ("appsecret_proof", &appsecret_proof(a.secret, access))]);
            let (status, v) = send(state, p, req).await?;
            if meta_ok(status, &v)?["success"] == true { Ok(()) } else { Err(unclear()) }
        }
        Platform::Google => {
            let req = rb(state, reqwest::Method::POST, h.google_revoke).form(&[("token", refresh.unwrap_or(access))]);
            let (status, v) = send(state, p, req).await?;
            google_ok(status, &v).map(|_| ())
        }
        Platform::Tiktok => {
            let req = rb(state, reqwest::Method::POST, format!("{}/oauth2/revoke_token/", h.tiktok)).json(&json!({"app_id": a.id, "secret": a.secret, "access_token": access}));
            let (status, v) = send(state, p, req).await?;
            tiktok_ok(status, &v).map(|_| ())
        }
    }
}

// ---------- Error mapping ----------

fn meta_ok(status: u16, v: &Value) -> PResult<&Value> {
    if (200..300).contains(&status) && v.is_object() && v.get("error").is_none() {
        return Ok(v);
    }
    let e = &v["error"];
    let code = e["code"].as_i64().unwrap_or(0);
    let sub = e["error_subcode"].as_i64().unwrap_or(0);
    Err(match (code, sub) {
        _ if (200..300).contains(&status) => unclear(),
        (190, _) | (102, _) => PErr::new(Kind::Auth, "token_expired"),
        (4 | 17 | 32 | 613 | 80003 | 80004, _) => PErr::new(Kind::RateLimited, "rate_limited"),
        (200, 1870090) => PErr::new(Kind::Permission, "meta_terms"),
        (200, 1870050) => PErr::new(Kind::Permission, "meta_business"),
        (2656, _) => PErr::new(Kind::Refused, "lookalikes_depend"),
        (10 | 200 | 270 | 294 | 2635, _) => PErr::new(Kind::Permission, "permission_denied"),
        (1 | 2, _) => PErr::new(Kind::Transient, "platform_down"),
        (100, 33) => PErr::new(Kind::NotFound, "audience_missing"),
        _ if status == 429 => PErr::new(Kind::RateLimited, "rate_limited"),
        _ if status >= 500 => PErr::new(Kind::Transient, "platform_down"),
        _ => PErr::new(Kind::Refused, "refused"),
    })
}

fn google_token_ok(status: u16, v: &Value) -> PResult<&Value> {
    if (200..300).contains(&status) && v.is_object() && v.get("error").is_none() {
        return Ok(v);
    }
    Err(match v["error"].as_str() {
        _ if (200..300).contains(&status) => unclear(),
        Some("invalid_grant") => PErr::new(Kind::Auth, "token_expired"),
        _ if status >= 500 => PErr::new(Kind::Transient, "platform_down"),
        _ => PErr::new(Kind::Refused, "refused"),
    })
}

fn google_ok(status: u16, v: &Value) -> PResult<&Value> {
    if (200..300).contains(&status) {
        return if v.is_object() { Ok(v) } else { Err(unclear()) };
    }
    let s = v["error"]["status"].as_str().unwrap_or_default();
    Err(match (status, s) {
        (401, _) | (_, "UNAUTHENTICATED") => PErr::new(Kind::Auth, "token_expired"),
        (429, _) | (_, "RESOURCE_EXHAUSTED") => PErr::new(Kind::RateLimited, "rate_limited"),
        (403, _) | (_, "PERMISSION_DENIED") => PErr::new(Kind::Permission, "permission_denied"),
        (404, _) | (_, "NOT_FOUND") => PErr::new(Kind::NotFound, "audience_missing"),
        (s, _) if s >= 500 => PErr::new(Kind::Transient, "platform_down"),
        _ => PErr::new(Kind::Refused, "refused"),
    })
}

/// TikTok answers HTTP 200 with `code` 0 and a `data` object on success, a non-zero `code` on
/// failure. Anything else (no code, no data) is an answer DumbMail can't read.
fn tiktok_ok(status: u16, v: &Value) -> PResult<&Value> {
    let code = v["code"].as_i64();
    if (200..300).contains(&status) && code == Some(0) {
        return if v["data"].is_object() { Ok(&v["data"]) } else { Err(unclear()) };
    }
    Err(match code {
        None if (200..300).contains(&status) => unclear(),
        Some(40100) => PErr::new(Kind::RateLimited, "rate_limited"),
        Some(40102 | 40104 | 40105) => PErr::new(Kind::Auth, "token_expired"),
        Some(40001) => PErr::new(Kind::Permission, "permission_denied"),
        Some(c) if (50000..60000).contains(&c) => PErr::new(Kind::Transient, "platform_down"),
        _ if status == 429 => PErr::new(Kind::RateLimited, "rate_limited"),
        _ if status >= 500 => PErr::new(Kind::Transient, "platform_down"),
        _ => PErr::new(Kind::Refused, "refused"),
    })
}

// ---------- Meta helpers ----------

async fn meta_get(state: &AppState, token: &str, path: &str, params: &[(&str, &str)]) -> PResult<Value> {
    let a = app(&state.cfg, Platform::Meta).ok_or_else(unavailable)?;
    let proof = appsecret_proof(a.secret, token);
    let mut q: Vec<(&str, &str)> = vec![("access_token", token), ("appsecret_proof", &proof)];
    q.extend_from_slice(params);
    let (status, v) = send(state, Platform::Meta, rb(state, reqwest::Method::GET, format!("{}/{path}", hosts(&state.cfg).meta)).query(&q)).await?;
    meta_ok(status, &v)?;
    Ok(v)
}

async fn meta_send(state: &AppState, method: reqwest::Method, token: &str, path: &str, params: &[(&str, &str)]) -> PResult<Value> {
    let a = app(&state.cfg, Platform::Meta).ok_or_else(unavailable)?;
    let proof = appsecret_proof(a.secret, token);
    let mut form: Vec<(&str, &str)> = vec![("access_token", token), ("appsecret_proof", &proof)];
    form.extend_from_slice(params);
    let (status, v) = send(state, Platform::Meta, rb(state, method, format!("{}/{path}", hosts(&state.cfg).meta)).form(&form)).await?;
    meta_ok(status, &v)?;
    Ok(v)
}

// ---------- Ad accounts ----------

#[derive(Debug, Clone, Serialize)]
pub struct AdAccount {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdAccounts {
    pub accounts: Vec<AdAccount>,
    /// Google: people type their customer ID (and manager ID), checked with a cheap call.
    pub manual_entry: bool,
}

pub async fn list_accounts(state: &AppState, p: Platform, token: &str) -> PResult<AdAccounts> {
    match p {
        Platform::Meta => {
            let mut out = Vec::new();
            let mut after: Option<String> = None;
            for _ in 0..5 {
                let mut params = vec![("fields", "account_id,name"), ("limit", "100")];
                if let Some(a) = after.as_deref() {
                    params.push(("after", a));
                }
                let v = meta_get(state, token, "me/adaccounts", &params).await?;
                let data = v["data"].as_array().ok_or_else(unclear)?;
                for a in data {
                    let id = nonempty(&a["account_id"]).or_else(|| nonempty(&a["id"]).map(|s| s.trim_start_matches("act_").to_string()));
                    if let Some(id) = id {
                        out.push(AdAccount { id, name: nonempty(&a["name"]) });
                    }
                }
                after = v["paging"]["cursors"]["after"].as_str().filter(|_| v["paging"]["next"].is_string()).map(String::from);
                if after.is_none() {
                    break;
                }
            }
            Ok(AdAccounts { accounts: out, manual_entry: false })
        }
        Platform::Google => Ok(AdAccounts { accounts: Vec::new(), manual_entry: true }),
        Platform::Tiktok => {
            let a = app(&state.cfg, p).ok_or_else(unavailable)?;
            let req = rb(state, reqwest::Method::GET, format!("{}/oauth2/advertiser/get/", hosts(&state.cfg).tiktok)).header("Access-Token", token).query(&[("app_id", a.id), ("secret", a.secret)]);
            let (status, v) = send(state, p, req).await?;
            let data = tiktok_ok(status, &v)?;
            let accounts = data["list"]
                .as_array()
                .ok_or_else(unclear)?
                .iter()
                .filter_map(|a| {
                    let id = nonempty(&a["advertiser_id"]).or_else(|| a["advertiser_id"].as_i64().map(|n| n.to_string()))?;
                    Some(AdAccount { id, name: nonempty(&a["advertiser_name"]) })
                })
                .collect();
            Ok(AdAccounts { accounts, manual_entry: false })
        }
    }
}

/// Google: check the sign-in can manage Customer Match lists in this account (a one-item list call).
pub async fn verify_google_account(state: &AppState, t: &Target<'_>) -> PResult<()> {
    let req = t.google_req(rb(state, reqwest::Method::GET, format!("{}/{}/userLists", hosts(&state.cfg).datamanager, t.google_parent())).query(&[("pageSize", "1")]));
    let (status, v) = send(state, t.platform, req).await?;
    google_ok(status, &v).map(|_| ())
}

// ---------- Audiences ----------

/// Who and where: the token and the ad account a sync writes to.
pub struct Target<'a> {
    pub platform: Platform,
    pub token: &'a str,
    pub ad_account_id: &'a str,
    /// Google Ads manager account, when the signed-in Google user reaches the ad account through one.
    pub login_account_id: Option<&'a str>,
}

impl Target<'_> {
    fn google_parent(&self) -> String {
        format!("accountTypes/GOOGLE_ADS/accounts/{}", self.ad_account_id)
    }

    /// List management: the manager account goes in the `login-account` header.
    fn google_req(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let rb = rb.bearer_auth(self.token);
        match self.login_account_id {
            Some(m) => rb.header("login-account", format!("accountTypes/GOOGLE_ADS/accounts/{m}")),
            None => rb,
        }
    }

    /// Member calls: the manager account goes in `Destination.loginAccount`.
    fn google_destination(&self, audience_id: &str) -> Value {
        let mut d = json!({"operatingAccount": {"accountType": "GOOGLE_ADS", "accountId": self.ad_account_id}, "productDestinationId": audience_id});
        if let Some(m) = self.login_account_id {
            d["loginAccount"] = json!({"accountType": "GOOGLE_ADS", "accountId": m});
        }
        d
    }
}

/// Customer Match keeps members for at most 540 days; set it explicitly so refreshes can be timed.
pub const GOOGLE_MEMBERSHIP_DAYS: i64 = 540;

/// Create an empty customer-list audience on Meta or Google and return the id the platform gave
/// it. That id is the only way DumbMail ever finds the audience again.
pub async fn create_audience(state: &AppState, t: &Target<'_>, name: &str) -> PResult<String> {
    let h = hosts(&state.cfg);
    match t.platform {
        Platform::Meta => {
            let v = meta_send(
                state,
                reqwest::Method::POST,
                t.token,
                &format!("act_{}/customaudiences", t.ad_account_id),
                &[("name", name), ("subtype", "CUSTOM"), ("description", "Kept in step by DumbMail"), ("customer_file_source", "USER_PROVIDED_ONLY")],
            )
            .await?;
            nonempty(&v["id"]).filter(|id| id.chars().all(|c| c.is_ascii_digit())).ok_or_else(unclear)
        }
        Platform::Google => {
            let body_json = json!({
                "displayName": name,
                "description": "Kept in step by DumbMail",
                "membershipStatus": "OPEN",
                "membershipDuration": format!("{}s", GOOGLE_MEMBERSHIP_DAYS * 86_400),
                "ingestedUserListInfo": {"uploadKeyTypes": ["CONTACT_ID"], "contactIdInfo": {"dataSourceType": "DATA_SOURCE_TYPE_FIRST_PARTY"}},
            });
            let req = t.google_req(rb(state, reqwest::Method::POST, format!("{}/{}/userLists", h.datamanager, t.google_parent())).json(&body_json));
            let (status, v) = send(state, t.platform, req).await?;
            let v = google_ok(status, &v)?;
            nonempty(&v["id"]).or_else(|| v["name"].as_str().and_then(|n| n.strip_prefix(&format!("{}/userLists/", t.google_parent()))).map(String::from)).ok_or_else(unclear)
        }
        // TikTok creates the audience with its first file; see `tiktok_create`.
        Platform::Tiktok => Err(PErr::new(Kind::Refused, "refused")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Remove,
}

/// What the platform said about one batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The platform confirmed every row.
    Confirmed,
    /// The platform confirmed some rows; these (indexes into the batch) it didn't.
    Partial(Vec<usize>),
    /// Google took the batch and is processing it: check `request_status` with this id.
    Accepted(String),
}

/// Send one batch of hashed rows (`[{"t", "h"}]` as stored in the batch).
pub async fn send_batch(state: &AppState, t: &Target<'_>, audience_id: &str, op: Op, rows: &[Value], meta_session_id: i64) -> PResult<Outcome> {
    let h = hosts(&state.cfg);
    match t.platform {
        Platform::Meta => {
            let data: Vec<Vec<String>> = rows.iter().map(super::normalize::meta_row).collect();
            let payload = json!({"schema": super::normalize::META_SCHEMA, "data": data}).to_string();
            // One batch per session: a resend gets a new session id, so no sequence is ever reused.
            let session = json!({"session_id": meta_session_id, "batch_seq": 1, "last_batch_flag": true}).to_string();
            let method = if op == Op::Add { reqwest::Method::POST } else { reqwest::Method::DELETE };
            let v = meta_send(state, method, t.token, &format!("{audience_id}/users"), &[("payload", &payload), ("session", &session)]).await?;
            let received = v["num_received"].as_i64().ok_or_else(unclear)?;
            let invalid = v["num_invalid_entries"].as_i64().ok_or_else(unclear)?;
            if received != rows.len() as i64 || invalid < 0 || v["session_id"].as_i64().is_some_and(|s| s != meta_session_id) {
                return Err(unclear());
            }
            if invalid == 0 {
                return Ok(Outcome::Confirmed);
            }
            // Meta gives up to 100 samples of the rows it couldn't read. When they name exactly the
            // rows counted as invalid, only those are unconfirmed; otherwise none of them is confirmed.
            let samples: Vec<String> = v["invalid_entry_samples"]
                .as_array()
                .map(|a| a.iter().flat_map(|s| if let Some(o) = s.as_object() { o.values().filter_map(|x| x.as_str().map(String::from)).collect() } else { s.as_str().map(String::from).into_iter().collect::<Vec<_>>() }).collect())
                .unwrap_or_default();
            let named: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r["h"].as_str().is_some_and(|h| samples.iter().any(|s| s.contains(h)))).map(|(i, _)| i).collect();
            if named.len() as i64 == invalid { Ok(Outcome::Partial(named)) } else { Ok(Outcome::Partial((0..rows.len()).collect())) }
        }
        Platform::Google => {
            let members: Vec<Value> = rows.iter().map(super::normalize::google_member).collect();
            let mut req = json!({"destinations": [t.google_destination(audience_id)], "audienceMembers": members, "encoding": "HEX"});
            let path = if op == Op::Add {
                // The account confirmed its consent covers advertising use (see ad_audience_settings),
                // and the person who created the sync accepted Google's Customer Match terms.
                req["consent"] = json!({"adUserData": "CONSENT_GRANTED", "adPersonalization": "CONSENT_GRANTED"});
                req["termsOfService"] = json!({"customerMatchTermsOfServiceStatus": "ACCEPTED"});
                "audienceMembers:ingest"
            } else {
                "audienceMembers:remove"
            };
            let r = t.google_req(rb(state, reqwest::Method::POST, format!("{}/{path}", h.datamanager)).json(&req));
            let (status, v) = send(state, t.platform, r).await?;
            let v = google_ok(status, &v)?;
            nonempty(&v["requestId"]).map(Outcome::Accepted).ok_or_else(unclear)
        }
        Platform::Tiktok => {
            let file_path = tiktok_upload(state, t, rows).await?;
            let action = if op == Op::Add { "APPEND" } else { "REMOVE" };
            let req = rb(state, reqwest::Method::POST, format!("{}/dmp/custom_audience/update/", h.tiktok))
                .header("Access-Token", t.token)
                .json(&json!({"advertiser_id": t.ad_account_id, "custom_audience_id": audience_id, "file_paths": [file_path], "action": action}));
            let (status, v) = send(state, t.platform, req).await?;
            tiktok_ok(status, &v)?;
            Ok(Outcome::Confirmed)
        }
    }
}

/// TikTok: create the audience from its first file (TikTok can't create an empty one). Returns the
/// audience id. Not repeatable: an unreadable answer may mean TikTok made it anyway.
pub async fn tiktok_create(state: &AppState, t: &Target<'_>, name: &str, rows: &[Value]) -> PResult<String> {
    let file_path = tiktok_upload(state, t, rows).await?;
    let req = rb(state, reqwest::Method::POST, format!("{}/dmp/custom_audience/create/", hosts(&state.cfg).tiktok))
        .header("Access-Token", t.token)
        .json(&json!({"advertiser_id": t.ad_account_id, "custom_audience_name": name, "file_paths": [file_path], "calculate_type": "EMAIL_SHA256"}));
    let (status, v) = send(state, t.platform, req).await?;
    let data = tiktok_ok(status, &v)?;
    nonempty(&data["custom_audience_id"]).or_else(|| data["custom_audience_id"].as_i64().map(|n| n.to_string())).ok_or_else(unclear)
}

/// The customer file DumbMail uploads to TikTok: a header line, then one SHA-256 email hash per line.
pub fn tiktok_file(rows: &[Value]) -> String {
    let mut file = String::from("email_SHA256\n");
    for r in rows.iter().filter(|r| r["t"] == "email") {
        file.push_str(r["h"].as_str().unwrap_or_default());
        file.push('\n');
    }
    file
}

/// Upload a customer file; returns TikTok's file_path.
async fn tiktok_upload(state: &AppState, t: &Target<'_>, rows: &[Value]) -> PResult<String> {
    use md5::Md5;
    let file = tiktok_file(rows);
    let signature = hex::encode(Md5::digest(file.as_bytes()));
    let boundary = format!("dumbmail{}", uuid::Uuid::new_v4().simple());
    let mut body_bytes = Vec::with_capacity(file.len() + 1024);
    for (k, v) in [("advertiser_id", t.ad_account_id), ("calculate_type", "EMAIL_SHA256"), ("file_signature", signature.as_str())] {
        body_bytes.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    body_bytes.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"dumbmail-audience.csv\"\r\nContent-Type: text/csv\r\n\r\n").as_bytes());
    body_bytes.extend_from_slice(file.as_bytes());
    body_bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let req = rb(state, reqwest::Method::POST, format!("{}/dmp/custom_audience/file/upload/", hosts(&state.cfg).tiktok))
        .timeout(Duration::from_secs(120))
        .header("Access-Token", t.token)
        .header(reqwest::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .body(body_bytes);
    let (status, v) = send(state, t.platform, req).await?;
    nonempty(&tiktok_ok(status, &v)?["file_path"]).ok_or_else(unclear)
}

/// Google's processing result for an accepted batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestStatus {
    Processing,
    Success,
    Failed,
    /// Indexes of the records that failed, when Google lists them; None when it doesn't.
    Partial(Option<Vec<usize>>),
}

pub async fn google_request_status(state: &AppState, t: &Target<'_>, request_id: &str) -> PResult<RequestStatus> {
    let req = t.google_req(rb(state, reqwest::Method::GET, format!("{}/requestStatus:retrieve", hosts(&state.cfg).datamanager)).query(&[("requestId", request_id)]));
    let (status, v) = send(state, t.platform, req).await?;
    let v = google_ok(status, &v)?;
    let d = v["requestStatusPerDestination"].as_array().and_then(|a| a.first()).ok_or_else(unclear)?;
    Ok(match d["requestStatus"].as_str() {
        Some("SUCCESS") => RequestStatus::Success,
        Some("FAILED") => RequestStatus::Failed,
        Some("PROCESSING") | Some("REQUEST_STATUS_UNKNOWN") => RequestStatus::Processing,
        Some("PARTIAL_SUCCESS") => {
            let mut idx: Vec<usize> = Vec::new();
            let mut listed = 0i64;
            let mut total = 0i64;
            for c in d["errorInfo"]["errorCounts"].as_array().into_iter().flatten() {
                total += c["recordCount"].as_str().and_then(|s| s.parse().ok()).or_else(|| c["recordCount"].as_i64()).unwrap_or(i64::MAX / 4);
                for i in c["recordIndices"].as_array().into_iter().flatten() {
                    if let Some(i) = i.as_u64() {
                        idx.push(i as usize);
                        listed += 1;
                    }
                }
            }
            RequestStatus::Partial((listed > 0 && listed == total).then_some(idx))
        }
        _ => return Err(unclear()),
    })
}

/// What the platform reports about an audience. Sizes are the platform's estimates.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub size: Option<i64>,
    pub size_upper: Option<i64>,
    pub match_rate: Option<f32>,
    /// A short plain status ("Ready", "Too small to use yet", …).
    pub status: Option<String>,
}

pub async fn stats(state: &AppState, t: &Target<'_>, audience_id: &str) -> PResult<Stats> {
    let h = hosts(&state.cfg);
    match t.platform {
        Platform::Meta => {
            let v = meta_get(state, t.token, audience_id, &[("fields", "approximate_count_lower_bound,approximate_count_upper_bound,delivery_status,operation_status")]).await?;
            let num = |k: &str| v[k].as_i64().filter(|n| *n >= 0);
            let delivery = v["delivery_status"]["code"].as_i64();
            let operation = v["operation_status"]["code"].as_i64();
            let status = match (delivery, operation) {
                (_, Some(471)) => "Flagged by Meta for review",
                (Some(300), _) => "Too small to use yet",
                (Some(c), _) if c >= 400 => "Not usable for ads yet",
                (_, Some(411)) => "Ready, low match rate",
                (_, Some(441)) => "Still filling, usable",
                (Some(200), _) => "Ready",
                _ => "Processing",
            };
            Ok(Stats { size: num("approximate_count_lower_bound"), size_upper: num("approximate_count_upper_bound"), match_rate: None, status: Some(status.into()) })
        }
        Platform::Google => {
            let req = t.google_req(rb(state, reqwest::Method::GET, format!("{}/{}/userLists/{audience_id}", h.datamanager, t.google_parent())));
            let (status, v) = send(state, t.platform, req).await?;
            let v = google_ok(status, &v)?;
            let count = |k: &str| v["sizeInfo"][k].as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| v["sizeInfo"][k].as_i64());
            let size = [count("searchNetworkMembersCount"), count("displayNetworkMembersCount"), count("youtubeMembersCount"), count("gmailMembersCount")].into_iter().flatten().max();
            let rate = v["ingestedUserListInfo"]["contactIdInfo"]["matchRatePercentage"].as_f64().map(|r| r as f32);
            let status = match size {
                Some(n) if n >= 100 => "Ready",
                Some(_) => "Too small to use yet (Google needs 100 active people)",
                None => "Processing",
            };
            Ok(Stats { size, size_upper: None, match_rate: rate, status: Some(status.into()) })
        }
        Platform::Tiktok => {
            let ids = json!([audience_id]).to_string();
            let req = rb(state, reqwest::Method::GET, format!("{}/dmp/custom_audience/get/", h.tiktok)).header("Access-Token", t.token).query(&[("advertiser_id", t.ad_account_id), ("custom_audience_ids", ids.as_str())]);
            let (status, v) = send(state, t.platform, req).await?;
            let data = tiktok_ok(status, &v)?;
            let d = &data["list"][0]["audience_details"];
            let size = d["cover_num"].as_i64();
            let status = if d["is_expired"].as_bool() == Some(true) {
                "Expired on TikTok"
            } else if d["is_valid"].as_bool() == Some(true) {
                "Ready"
            } else {
                "Processing"
            };
            Ok(Stats { size, size_upper: None, match_rate: None, status: Some(status.into()) })
        }
    }
}

/// Delete the audience on the platform. Ads that use it stop. Ok means the platform confirmed it.
pub async fn delete_audience(state: &AppState, t: &Target<'_>, audience_id: &str) -> PResult<()> {
    let h = hosts(&state.cfg);
    match t.platform {
        Platform::Meta => {
            let a = app(&state.cfg, Platform::Meta).ok_or_else(unavailable)?;
            let req = rb(state, reqwest::Method::DELETE, format!("{}/{audience_id}", h.meta)).query(&[("access_token", t.token), ("appsecret_proof", &appsecret_proof(a.secret, t.token))]);
            let (status, v) = send(state, t.platform, req).await?;
            if meta_ok(status, &v)?["success"] == true { Ok(()) } else { Err(unclear()) }
        }
        Platform::Google => {
            let req = t.google_req(rb(state, reqwest::Method::DELETE, format!("{}/{}/userLists/{audience_id}", h.datamanager, t.google_parent())));
            let (status, v) = send(state, t.platform, req).await?;
            google_ok(status, &v).map(|_| ())
        }
        Platform::Tiktok => {
            let req = rb(state, reqwest::Method::POST, format!("{}/dmp/custom_audience/delete/", h.tiktok))
                .header("Access-Token", t.token)
                .json(&json!({"advertiser_id": t.ad_account_id, "custom_audience_ids": [audience_id]}));
            let (status, v) = send(state, t.platform, req).await?;
            tiktok_ok(status, &v).map(|_| ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_mapping() {
        let e = |code: i64, sub: i64| meta_ok(400, &json!({"error": {"code": code, "error_subcode": sub, "message": "secret token abc"}})).unwrap_err();
        assert_eq!(e(190, 0).kind, Kind::Auth);
        assert_eq!(e(80003, 0).kind, Kind::RateLimited);
        assert_eq!(e(200, 1870090).code, "meta_terms");
        assert_eq!(e(100, 0).kind, Kind::Refused);
        assert_eq!(e(100, 33).kind, Kind::NotFound);
        assert_eq!(meta_ok(200, &json!("not an object")).unwrap_err().kind, Kind::Unclear);
        assert_eq!(google_ok(429, &json!({"error": {"status": "RESOURCE_EXHAUSTED"}})).unwrap_err().kind, Kind::RateLimited);
        assert_eq!(google_ok(403, &json!({"error": {"status": "PERMISSION_DENIED", "message": "no"}})).unwrap_err().kind, Kind::Permission);
        assert_eq!(google_ok(200, &Value::Null).unwrap_err().kind, Kind::Unclear);
        assert_eq!(google_token_ok(400, &json!({"error": "invalid_grant"})).unwrap_err().kind, Kind::Auth);
        assert_eq!(tiktok_ok(200, &json!({"code": 40100, "message": "slow"})).unwrap_err().kind, Kind::RateLimited);
        assert_eq!(tiktok_ok(200, &json!({"code": 40105, "message": "bad token"})).unwrap_err().kind, Kind::Auth);
        assert_eq!(tiktok_ok(200, &json!({"code": 0, "data": {"a": 1}})).unwrap()["a"], 1);
        // A 2xx without TikTok's code or data isn't success.
        assert_eq!(tiktok_ok(200, &json!({"data": {"a": 1}})).unwrap_err().kind, Kind::Unclear);
        assert_eq!(tiktok_ok(200, &json!({"code": 0})).unwrap_err().kind, Kind::Unclear);
        // Every code has words of DumbMail's own, never the platform's text.
        for c in ERROR_CODES {
            assert!(!words(Platform::Meta, c).contains("secret"));
        }
    }

    #[test]
    fn pkce() {
        // S256: unpadded base64url of the SHA-256 of the verifier (43 characters).
        let c = pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r7wW1gFWFOEjXk");
        assert_eq!(c, "bwWFMyPfdG9qreDhH2lmftFx_dFeLDalzcT1gb_j68g");
        assert_eq!(c.len(), 43);
    }

    #[test]
    fn only_platform_hosts() {
        let u = |s: &str| reqwest::Url::parse(s).unwrap();
        let prod = None;
        assert!(url_allowed(prod, &u("https://graph.facebook.com/v27.0/me")));
        assert!(url_allowed(prod, &u("https://datamanager.googleapis.com/v1/audienceMembers:ingest")));
        assert!(!url_allowed(prod, &u("http://graph.facebook.com/v27.0/me")), "HTTPS only");
        assert!(!url_allowed(prod, &u("https://graph.facebook.com.evil.test/")));
        assert!(!url_allowed(prod, &u("https://user:pw@graph.facebook.com/")));
        assert!(!url_allowed(prod, &u("https://graph.facebook.com:8443/")));
        assert!(!url_allowed(prod, &u("http://127.0.0.1:9/")));
        // A stand-in base that isn't loopback is ignored: production hosts stay in force.
        assert_eq!(loopback(Some("https://mock.evil.test")), None);
        assert!(hosts_for(loopback(Some("https://mock.evil.test"))).meta.starts_with("https://graph.facebook.com/"));
        let test = loopback(Some("http://127.0.0.1:4000"));
        assert!(url_allowed(test, &u("http://127.0.0.1:4000/meta/me")));
        assert!(!url_allowed(test, &u("https://graph.facebook.com/v27.0/me")));
    }

    #[test]
    fn tiktok_file_format() {
        let rows = vec![json!({"t": "email", "h": "aa"}), json!({"t": "email", "h": "bb"})];
        assert_eq!(tiktok_file(&rows), "email_SHA256\naa\nbb\n");
    }
}
