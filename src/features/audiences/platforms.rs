//! The three ad platforms' APIs: OAuth, ad accounts, and customer-list audiences.
//!
//! - Meta Marketing API (Graph v27.0): Facebook Login, `/me/adaccounts`, Custom Audiences from a
//!   customer list (`/act_{id}/customaudiences`, `/{audience}/users` with upload sessions).
//! - Google: OAuth 2.0 with PKCE and offline refresh tokens; Customer Match through the Data Manager
//!   API (`userLists`, `audienceMembers:ingest` / `:remove`), which Google requires for Customer
//!   Match uploads since April 2026; the Google Ads API (v26) only to list reachable ad accounts.
//! - TikTok API for Business (v1.3): advertiser authorization, customer file audiences
//!   (`/dmp/custom_audience/file/upload/`, `create`, `update` with APPEND / REMOVE).
//!
//! Hosts are fixed by DumbMail (never typed by customers). `AD_PLATFORM_API_BASE` swaps them all for
//! a local stand-in in tests.

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

    /// People per platform request: Meta and Google take at most 10,000; TikTok takes a file.
    pub fn batch_size(self) -> i64 {
        match self {
            Platform::Meta | Platform::Google => 10_000,
            Platform::Tiktok => 50_000,
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

pub fn google_developer_token(cfg: &Config) -> Option<&str> {
    set(&cfg.google_ads_developer_token)
}

const META_VERSION: &str = "v27.0";
const GOOGLE_ADS_VERSION: &str = "v26";

struct Hosts {
    meta: String,
    google_token: String,
    google_revoke: String,
    datamanager: String,
    google_ads: String,
    tiktok: String,
}

fn hosts(cfg: &Config) -> Hosts {
    match set(&cfg.ad_platform_api_base) {
        Some(b) => {
            let b = b.trim_end_matches('/');
            Hosts {
                meta: format!("{b}/meta"),
                google_token: format!("{b}/google/token"),
                google_revoke: format!("{b}/google/revoke"),
                datamanager: format!("{b}/datamanager"),
                google_ads: format!("{b}/googleads"),
                tiktok: format!("{b}/tiktok"),
            }
        }
        None => Hosts {
            meta: format!("https://graph.facebook.com/{META_VERSION}"),
            google_token: "https://oauth2.googleapis.com/token".into(),
            google_revoke: "https://oauth2.googleapis.com/revoke".into(),
            datamanager: "https://datamanager.googleapis.com/v1".into(),
            google_ads: format!("https://googleads.googleapis.com/{GOOGLE_ADS_VERSION}"),
            tiktok: "https://business-api.tiktok.com/open_api/v1.3".into(),
        },
    }
}

/// A platform call that didn't work, in words a customer can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PErr {
    /// Slow down and try again later.
    RateLimited(String),
    /// The sign-in expired or was revoked: reconnect.
    Auth(String),
    /// The ad account doesn't allow this (permissions, terms not accepted).
    Permission(String),
    /// The platform refused the request.
    Invalid(String),
    /// The audience no longer exists on the platform.
    NotFound(String),
    /// Network trouble or a platform outage: try again.
    Transient(String),
}

impl PErr {
    pub fn message(&self) -> &str {
        match self {
            PErr::RateLimited(m) | PErr::Auth(m) | PErr::Permission(m) | PErr::Invalid(m) | PErr::NotFound(m) | PErr::Transient(m) => m,
        }
    }
}

pub type PResult<T> = Result<T, PErr>;

fn short(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 300 { format!("{}…", s.chars().take(300).collect::<String>()) } else { s.to_string() }
}

fn network(p: Platform, e: reqwest::Error) -> PErr {
    tracing::warn!(platform = p.as_str(), error = %e.without_url(), "ad platform request failed");
    PErr::Transient(format!("Couldn't reach {}. DumbMail will try again.", p.label()))
}

/// Read a response body as JSON (an empty or non-JSON body becomes `{}`).
async fn body(p: Platform, res: reqwest::Response) -> PResult<(u16, Value)> {
    let status = res.status().as_u16();
    let bytes = res.bytes().await.map_err(|e| network(p, e))?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(json!({}))))
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
pub const GOOGLE_SCOPES: &str = "openid email https://www.googleapis.com/auth/datamanager https://www.googleapis.com/auth/adwords";

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
                    .append_pair("include_granted_scopes", "true")
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

fn unavailable(p: Platform) -> PErr {
    PErr::Invalid(format!("{} isn't available on this server.", p.label()))
}

/// Trade the callback's code for tokens.
pub async fn exchange(state: &AppState, p: Platform, code: &str, verifier: &str) -> PResult<Tokens> {
    let a = app(&state.cfg, p).ok_or_else(|| unavailable(p))?;
    let h = hosts(&state.cfg);
    let redirect = redirect_uri(&state.cfg, p);
    match p {
        Platform::Meta => {
            let res = state
                .http
                .get(format!("{}/oauth/access_token", h.meta))
                .query(&[("client_id", a.id), ("redirect_uri", &redirect), ("client_secret", a.secret), ("code", code)])
                .send()
                .await
                .map_err(|e| network(p, e))?;
            let (status, v) = body(p, res).await?;
            let short_lived = meta_ok(status, &v)?["access_token"].as_str().ok_or_else(|| PErr::Invalid("Meta didn't return a sign-in token.".into()))?.to_string();
            // Trade the short-lived token (about an hour) for a long-lived one (about 60 days).
            let mut t = meta_long_lived(state, &short_lived).await?;
            let me = meta_get(state, &t.access, "me", &[("fields", "name")]).await.ok();
            t.connected_as = me.and_then(|m| m["name"].as_str().map(String::from));
            Ok(t)
        }
        Platform::Google => {
            let res = state
                .http
                .post(&h.google_token)
                .form(&[
                    ("code", code),
                    ("client_id", a.id),
                    ("client_secret", a.secret),
                    ("redirect_uri", &redirect),
                    ("grant_type", "authorization_code"),
                    ("code_verifier", verifier),
                ])
                .send()
                .await
                .map_err(|e| network(p, e))?;
            let (status, v) = body(p, res).await?;
            let v = google_token_ok(status, &v)?;
            let access = v["access_token"].as_str().ok_or_else(|| PErr::Invalid("Google didn't return a sign-in token.".into()))?.to_string();
            let refresh = v["refresh_token"].as_str().map(String::from);
            if refresh.is_none() {
                return Err(PErr::Invalid("Google didn't allow offline access. Try connecting again and allow every permission.".into()));
            }
            let connected_as = v["id_token"].as_str().and_then(|t| crate::api::google::parse_id_token(t, a.id, chrono::Utc::now().timestamp()).ok()).map(|c| c.email);
            Ok(Tokens { access, refresh, expires_in: v["expires_in"].as_i64(), connected_as })
        }
        Platform::Tiktok => {
            let res = state
                .http
                .post(format!("{}/oauth2/access_token/", h.tiktok))
                .json(&json!({"app_id": a.id, "secret": a.secret, "auth_code": code}))
                .send()
                .await
                .map_err(|e| network(p, e))?;
            let (status, v) = body(p, res).await?;
            let data = tiktok_ok(status, &v)?;
            let access = data["access_token"].as_str().ok_or_else(|| PErr::Invalid("TikTok didn't return a sign-in token.".into()))?.to_string();
            let n = data["advertiser_ids"].as_array().map(Vec::len).unwrap_or(0);
            // Marketing API advertiser tokens don't expire until revoked; take any expiry TikTok sends.
            Ok(Tokens {
                access,
                refresh: data["refresh_token"].as_str().map(String::from),
                expires_in: data["expires_in"].as_i64(),
                connected_as: Some(format!("{n} advertiser account{}", if n == 1 { "" } else { "s" })),
            })
        }
    }
}

async fn meta_long_lived(state: &AppState, token: &str) -> PResult<Tokens> {
    let a = app(&state.cfg, Platform::Meta).ok_or_else(|| unavailable(Platform::Meta))?;
    let res = state
        .http
        .get(format!("{}/oauth/access_token", hosts(&state.cfg).meta))
        .query(&[("grant_type", "fb_exchange_token"), ("client_id", a.id), ("client_secret", a.secret), ("fb_exchange_token", token)])
        .send()
        .await
        .map_err(|e| network(Platform::Meta, e))?;
    let (status, v) = body(Platform::Meta, res).await?;
    let v = meta_ok(status, &v)?;
    let access = v["access_token"].as_str().ok_or_else(|| PErr::Invalid("Meta didn't return a sign-in token.".into()))?.to_string();
    Ok(Tokens { access, refresh: None, expires_in: v["expires_in"].as_i64(), connected_as: None })
}

/// Get a fresh access token before the current one expires. Google: the refresh token. Meta: trade
/// the long-lived token for a new one. TikTok tokens don't expire.
pub async fn refresh(state: &AppState, p: Platform, access: &str, refresh: Option<&str>) -> PResult<Tokens> {
    match p {
        Platform::Meta => meta_long_lived(state, access).await,
        Platform::Google => {
            let a = app(&state.cfg, p).ok_or_else(|| unavailable(p))?;
            let rt = refresh.ok_or_else(|| PErr::Auth("Google sign-in expired. Reconnect Google Ads.".into()))?;
            let res = state
                .http
                .post(hosts(&state.cfg).google_token)
                .form(&[("client_id", a.id), ("client_secret", a.secret), ("refresh_token", rt), ("grant_type", "refresh_token")])
                .send()
                .await
                .map_err(|e| network(p, e))?;
            let (status, v) = body(p, res).await?;
            let v = google_token_ok(status, &v)?;
            let access = v["access_token"].as_str().ok_or_else(|| PErr::Invalid("Google didn't return a sign-in token.".into()))?.to_string();
            Ok(Tokens { access, refresh: v["refresh_token"].as_str().map(String::from), expires_in: v["expires_in"].as_i64(), connected_as: None })
        }
        Platform::Tiktok => Ok(Tokens { access: access.to_string(), refresh: refresh.map(String::from), expires_in: None, connected_as: None }),
    }
}

/// Revoke DumbMail's access on the platform (best effort; tokens are deleted either way).
pub async fn revoke(state: &AppState, p: Platform, access: &str, refresh: Option<&str>) -> PResult<()> {
    let a = app(&state.cfg, p).ok_or_else(|| unavailable(p))?;
    let h = hosts(&state.cfg);
    let res = match p {
        Platform::Meta => state
            .http
            .delete(format!("{}/me/permissions", h.meta))
            .query(&[("access_token", access), ("appsecret_proof", &appsecret_proof(a.secret, access))])
            .send()
            .await
            .map_err(|e| network(p, e))?,
        Platform::Google => state.http.post(&h.google_revoke).form(&[("token", refresh.unwrap_or(access))]).send().await.map_err(|e| network(p, e))?,
        Platform::Tiktok => state
            .http
            .post(format!("{}/oauth2/revoke_token/", h.tiktok))
            .json(&json!({"app_id": a.id, "secret": a.secret, "access_token": access}))
            .send()
            .await
            .map_err(|e| network(p, e))?,
    };
    let (status, v) = body(p, res).await?;
    match p {
        Platform::Meta => meta_ok(status, &v).map(|_| ()),
        Platform::Google => google_ok(status, &v).map(|_| ()),
        Platform::Tiktok => tiktok_ok(status, &v).map(|_| ()),
    }
}

// ---------- Error mapping ----------

fn meta_ok(status: u16, v: &Value) -> PResult<&Value> {
    if (200..300).contains(&status) && v.get("error").is_none() {
        return Ok(v);
    }
    let e = &v["error"];
    let code = e["code"].as_i64().unwrap_or(0);
    let sub = e["error_subcode"].as_i64().unwrap_or(0);
    let msg = short(e["error_user_msg"].as_str().or(e["message"].as_str()).unwrap_or("unknown error"));
    Err(match (code, sub) {
        (190, _) | (102, _) => PErr::Auth("Meta sign-in expired or was revoked. Reconnect Meta.".into()),
        (4 | 17 | 32 | 613 | 80003 | 80004, _) => PErr::RateLimited("Meta asked DumbMail to slow down. It will try again shortly.".into()),
        (200, 1870090) => PErr::Permission("Accept Meta's Custom Audience terms for this ad account in Ads Manager, then sync again.".into()),
        (200, 1870050) => PErr::Permission("This ad account needs to belong to a Meta business portfolio before it can use customer list audiences.".into()),
        (10 | 200 | 270 | 294 | 2635, _) => PErr::Permission(format!("This Meta ad account doesn't let you manage audiences. Meta said: {msg}")),
        (1 | 2, _) => PErr::Transient("Meta had a temporary problem. DumbMail will try again.".into()),
        (100, 33) => PErr::NotFound("The audience no longer exists on Meta.".into()),
        _ if status >= 500 => PErr::Transient("Meta had a temporary problem. DumbMail will try again.".into()),
        _ => PErr::Invalid(format!("Meta refused the request: {msg}")),
    })
}

fn google_token_ok(status: u16, v: &Value) -> PResult<&Value> {
    if (200..300).contains(&status) && v.get("error").is_none() {
        return Ok(v);
    }
    match v["error"].as_str() {
        Some("invalid_grant") => Err(PErr::Auth("Google sign-in expired or was revoked. Reconnect Google Ads.".into())),
        _ if status >= 500 => Err(PErr::Transient("Google had a temporary problem. DumbMail will try again.".into())),
        other => Err(PErr::Invalid(format!("Google refused the sign-in ({}).", other.unwrap_or("unknown error")))),
    }
}

fn google_ok(status: u16, v: &Value) -> PResult<&Value> {
    if (200..300).contains(&status) {
        return Ok(v);
    }
    let e = &v["error"];
    let msg = short(e["message"].as_str().unwrap_or("unknown error"));
    let s = e["status"].as_str().unwrap_or_default();
    Err(match (status, s) {
        (401, _) | (_, "UNAUTHENTICATED") => PErr::Auth("Google sign-in expired or was revoked. Reconnect Google Ads.".into()),
        (429, _) | (_, "RESOURCE_EXHAUSTED") => PErr::RateLimited("Google asked DumbMail to slow down. It will try again shortly.".into()),
        (403, _) | (_, "PERMISSION_DENIED") => PErr::Permission(format!("This Google account can't manage audiences in this Google Ads account. Google said: {msg}")),
        (404, _) | (_, "NOT_FOUND") => PErr::NotFound("The audience no longer exists in Google Ads.".into()),
        (s, _) if s >= 500 => PErr::Transient("Google had a temporary problem. DumbMail will try again.".into()),
        _ => PErr::Invalid(format!("Google refused the request: {msg}")),
    })
}

/// TikTok answers HTTP 200 with `code` 0 on success and a non-zero `code` on failure.
fn tiktok_ok(status: u16, v: &Value) -> PResult<&Value> {
    let code = v["code"].as_i64().unwrap_or(if (200..300).contains(&status) { 0 } else { -1 });
    if (200..300).contains(&status) && code == 0 {
        return Ok(&v["data"]);
    }
    let msg = short(v["message"].as_str().unwrap_or("unknown error"));
    Err(match code {
        40100 => PErr::RateLimited("TikTok asked DumbMail to slow down. It will try again shortly.".into()),
        40102 | 40104 | 40105 => PErr::Auth("TikTok sign-in expired or was revoked. Reconnect TikTok.".into()),
        40001 => PErr::Permission(format!("This TikTok ad account doesn't let you manage audiences. TikTok said: {msg}")),
        c if (50000..60000).contains(&c) => PErr::Transient("TikTok had a temporary problem. DumbMail will try again.".into()),
        _ if status == 429 => PErr::RateLimited("TikTok asked DumbMail to slow down. It will try again shortly.".into()),
        _ if status >= 500 => PErr::Transient("TikTok had a temporary problem. DumbMail will try again.".into()),
        _ => PErr::Invalid(format!("TikTok refused the request: {msg}")),
    })
}

// ---------- Meta helpers ----------

async fn meta_get(state: &AppState, token: &str, path: &str, params: &[(&str, &str)]) -> PResult<Value> {
    let a = app(&state.cfg, Platform::Meta).ok_or_else(|| unavailable(Platform::Meta))?;
    let proof = appsecret_proof(a.secret, token);
    let mut q: Vec<(&str, &str)> = vec![("access_token", token), ("appsecret_proof", &proof)];
    q.extend_from_slice(params);
    let res = state.http.get(format!("{}/{path}", hosts(&state.cfg).meta)).query(&q).send().await.map_err(|e| network(Platform::Meta, e))?;
    let (status, v) = body(Platform::Meta, res).await?;
    meta_ok(status, &v)?;
    Ok(v)
}

async fn meta_send(state: &AppState, method: reqwest::Method, token: &str, path: &str, params: &[(&str, &str)]) -> PResult<Value> {
    let a = app(&state.cfg, Platform::Meta).ok_or_else(|| unavailable(Platform::Meta))?;
    let proof = appsecret_proof(a.secret, token);
    let mut form: Vec<(&str, &str)> = vec![("access_token", token), ("appsecret_proof", &proof)];
    form.extend_from_slice(params);
    let res = state
        .http
        .request(method, format!("{}/{path}", hosts(&state.cfg).meta))
        .timeout(Duration::from_secs(60))
        .form(&form)
        .send()
        .await
        .map_err(|e| network(Platform::Meta, e))?;
    let (status, v) = body(Platform::Meta, res).await?;
    meta_ok(status, &v)?;
    Ok(v)
}

// ---------- Ad accounts ----------

#[derive(Debug, Clone, Serialize)]
pub struct AdAccount {
    pub id: String,
    pub name: Option<String>,
    /// Google Ads manager (MCC) accounts can't hold Customer Match lists themselves.
    pub manager: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdAccounts {
    pub accounts: Vec<AdAccount>,
    /// True when the server can't list accounts and people type the ID instead (Google without a
    /// developer token).
    pub manual_entry: bool,
}

pub async fn list_accounts(state: &AppState, p: Platform, token: &str) -> PResult<AdAccounts> {
    let h = hosts(&state.cfg);
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
                for a in v["data"].as_array().into_iter().flatten() {
                    let id = a["account_id"].as_str().map(String::from).or_else(|| a["id"].as_str().map(|s| s.trim_start_matches("act_").to_string()));
                    if let Some(id) = id {
                        out.push(AdAccount { id, name: a["name"].as_str().map(String::from), manager: false });
                    }
                }
                after = v["paging"]["cursors"]["after"].as_str().filter(|_| v["paging"]["next"].is_string()).map(String::from);
                if after.is_none() {
                    break;
                }
            }
            Ok(AdAccounts { accounts: out, manual_entry: false })
        }
        Platform::Google => {
            let Some(dev) = google_developer_token(&state.cfg) else { return Ok(AdAccounts { accounts: Vec::new(), manual_entry: true }) };
            let res = state
                .http
                .get(format!("{}/customers:listAccessibleCustomers", h.google_ads))
                .bearer_auth(token)
                .header("developer-token", dev)
                .send()
                .await
                .map_err(|e| network(p, e))?;
            let (status, v) = body(p, res).await?;
            let v = google_ok(status, &v)?;
            let ids: Vec<String> = v["resourceNames"].as_array().into_iter().flatten().filter_map(|r| r.as_str()?.strip_prefix("customers/").map(String::from)).take(25).collect();
            let mut out = Vec::new();
            for id in ids {
                // The account's name; a failure just leaves the name blank.
                let res = state
                    .http
                    .post(format!("{}/customers/{id}/googleAds:search", h.google_ads))
                    .bearer_auth(token)
                    .header("developer-token", dev)
                    .header("login-customer-id", &id)
                    .json(&json!({"query": "SELECT customer.id, customer.descriptive_name, customer.manager FROM customer LIMIT 1"}))
                    .send()
                    .await;
                let c = match res {
                    Ok(r) if r.status().is_success() => r.json::<Value>().await.ok().map(|v| v["results"][0]["customer"].clone()),
                    _ => None,
                };
                let name = c.as_ref().and_then(|c| c["descriptiveName"].as_str()).map(String::from);
                let manager = c.as_ref().and_then(|c| c["manager"].as_bool()).unwrap_or(false);
                out.push(AdAccount { id, name, manager });
            }
            Ok(AdAccounts { accounts: out, manual_entry: true })
        }
        Platform::Tiktok => {
            let a = app(&state.cfg, p).ok_or_else(|| unavailable(p))?;
            let res = state
                .http
                .get(format!("{}/oauth2/advertiser/get/", h.tiktok))
                .header("Access-Token", token)
                .query(&[("app_id", a.id), ("secret", a.secret)])
                .send()
                .await
                .map_err(|e| network(p, e))?;
            let (status, v) = body(p, res).await?;
            let data = tiktok_ok(status, &v)?;
            let accounts = data["list"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| {
                    let id = a["advertiser_id"].as_str().map(String::from).or_else(|| a["advertiser_id"].as_i64().map(|n| n.to_string()))?;
                    Some(AdAccount { id, name: a["advertiser_name"].as_str().map(String::from), manager: false })
                })
                .collect();
            Ok(AdAccounts { accounts, manual_entry: false })
        }
    }
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

    fn google_req(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let rb = rb.bearer_auth(self.token);
        match self.login_account_id {
            Some(m) => rb.header("login-account", format!("accountTypes/GOOGLE_ADS/accounts/{m}")),
            None => rb,
        }
    }

    fn google_destination(&self, audience_id: &str) -> Value {
        let mut d = json!({"operatingAccount": {"accountType": "GOOGLE_ADS", "accountId": self.ad_account_id}, "productDestinationId": audience_id});
        if let Some(m) = self.login_account_id {
            d["loginAccount"] = json!({"accountType": "GOOGLE_ADS", "accountId": m});
        }
        d
    }
}

/// Text DumbMail puts on audiences it creates, so it can find its own audience again instead of
/// making a second one if a run stopped right after creating it.
pub fn marker(sync_id: uuid::Uuid) -> String {
    format!("Synced by DumbMail (sync {})", sync_id.simple())
}

/// Find an audience DumbMail already created for this sync.
pub async fn find_audience(state: &AppState, t: &Target<'_>, name: &str, marker: &str) -> PResult<Option<String>> {
    let h = hosts(&state.cfg);
    match t.platform {
        Platform::Meta => {
            let mut after: Option<String> = None;
            for _ in 0..5 {
                let mut params = vec![("fields", "id,name,description"), ("limit", "100")];
                if let Some(a) = after.as_deref() {
                    params.push(("after", a));
                }
                let v = meta_get(state, t.token, &format!("act_{}/customaudiences", t.ad_account_id), &params).await?;
                if let Some(a) = v["data"].as_array().into_iter().flatten().find(|a| a["description"].as_str() == Some(marker)) {
                    return Ok(a["id"].as_str().map(String::from));
                }
                after = v["paging"]["cursors"]["after"].as_str().filter(|_| v["paging"]["next"].is_string()).map(String::from);
                if after.is_none() {
                    break;
                }
            }
            Ok(None)
        }
        Platform::Google => {
            let mut page: Option<String> = None;
            for _ in 0..5 {
                let mut rb = state.http.get(format!("{}/{}/userLists", h.datamanager, t.google_parent())).query(&[("pageSize", "100")]);
                if let Some(p) = page.as_deref() {
                    rb = rb.query(&[("pageToken", p)]);
                }
                let res = t.google_req(rb).send().await.map_err(|e| network(t.platform, e))?;
                let (status, v) = body(t.platform, res).await?;
                let v = google_ok(status, &v)?;
                if let Some(l) = v["userLists"].as_array().into_iter().flatten().find(|l| l["description"].as_str() == Some(marker)) {
                    return Ok(l["id"].as_str().map(String::from));
                }
                page = v["nextPageToken"].as_str().filter(|s| !s.is_empty()).map(String::from);
                if page.is_none() {
                    break;
                }
            }
            Ok(None)
        }
        Platform::Tiktok => {
            for page in 1..=5 {
                let res = state
                    .http
                    .get(format!("{}/dmp/custom_audience/list/", h.tiktok))
                    .header("Access-Token", t.token)
                    .query(&[("advertiser_id", t.ad_account_id), ("page", &page.to_string()), ("page_size", "100")])
                    .send()
                    .await
                    .map_err(|e| network(t.platform, e))?;
                let (status, v) = body(t.platform, res).await?;
                let data = tiktok_ok(status, &v)?;
                if let Some(a) = data["list"].as_array().into_iter().flatten().find(|a| a["name"].as_str() == Some(name)) {
                    return Ok(a["audience_id"].as_str().map(String::from).or_else(|| a["audience_id"].as_i64().map(|n| n.to_string())));
                }
                if page >= data["page_info"]["total_page"].as_i64().unwrap_or(1) {
                    break;
                }
            }
            Ok(None)
        }
    }
}

/// Create an empty customer-list audience. TikTok can't create an empty one: it's created with the
/// first upload (see [`add`]), so this returns None there.
pub async fn create_audience(state: &AppState, t: &Target<'_>, name: &str, description: &str) -> PResult<Option<String>> {
    let h = hosts(&state.cfg);
    match t.platform {
        Platform::Meta => {
            let v = meta_send(
                state,
                reqwest::Method::POST,
                t.token,
                &format!("act_{}/customaudiences", t.ad_account_id),
                &[("name", name), ("subtype", "CUSTOM"), ("description", description), ("customer_file_source", "USER_PROVIDED_ONLY")],
            )
            .await?;
            Ok(v["id"].as_str().map(String::from))
        }
        Platform::Google => {
            let body_json = json!({
                "displayName": name,
                "description": description,
                "membershipStatus": "OPEN",
                "ingestedUserListInfo": {"uploadKeyTypes": ["CONTACT_ID"], "contactIdInfo": {"dataSourceType": "DATA_SOURCE_TYPE_FIRST_PARTY"}},
            });
            let rb = state.http.post(format!("{}/{}/userLists", h.datamanager, t.google_parent())).json(&body_json);
            let res = t.google_req(rb).send().await.map_err(|e| network(t.platform, e))?;
            let (status, v) = body(t.platform, res).await?;
            let v = google_ok(status, &v)?;
            Ok(v["id"].as_str().map(String::from).or_else(|| v["name"].as_str().and_then(|n| n.rsplit('/').next()).map(String::from)))
        }
        Platform::Tiktok => Ok(None),
    }
}

/// A Meta upload session: batches of one run share an id, numbered from 1, the last one flagged.
#[derive(Debug, Clone, Copy)]
pub struct MetaSession {
    pub id: i64,
    pub seq: i64,
    pub last: bool,
    pub estimated_total: i64,
}

/// What the platform said about one batch.
#[derive(Debug, Default, Clone)]
pub struct BatchResult {
    /// Rows the platform couldn't read (Meta's approximate num_invalid_entries).
    pub invalid: i64,
    /// TikTok: the audience created by this first upload.
    pub created_audience_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Remove,
}

/// Send one batch of hashed identifier sets (`ids` as stored in the snapshot).
pub async fn send_batch(
    state: &AppState,
    t: &Target<'_>,
    audience_id: Option<&str>,
    new_audience_name: &str,
    op: Op,
    ids: &[Value],
    with_location: bool,
    session: Option<MetaSession>,
) -> PResult<BatchResult> {
    let h = hosts(&state.cfg);
    match t.platform {
        Platform::Meta => {
            let aud = audience_id.ok_or_else(|| PErr::Invalid("The Meta audience hasn't been created yet.".into()))?;
            let schema = super::normalize::meta_schema(with_location);
            let data: Vec<Vec<String>> = ids.iter().map(|i| super::normalize::meta_row(i, &schema)).collect();
            let payload = json!({"schema": schema, "data": data}).to_string();
            let s = session.unwrap_or(MetaSession { id: 1, seq: 1, last: true, estimated_total: ids.len() as i64 });
            let session_json = json!({"session_id": s.id, "batch_seq": s.seq, "last_batch_flag": s.last, "estimated_num_total": s.estimated_total}).to_string();
            let method = if op == Op::Add { reqwest::Method::POST } else { reqwest::Method::DELETE };
            let v = meta_send(state, method, t.token, &format!("{aud}/users"), &[("payload", &payload), ("session", &session_json)]).await?;
            Ok(BatchResult { invalid: v["num_invalid_entries"].as_i64().unwrap_or(0), created_audience_id: None })
        }
        Platform::Google => {
            let aud = audience_id.ok_or_else(|| PErr::Invalid("The Google Ads audience hasn't been created yet.".into()))?;
            let members: Vec<Value> = ids.iter().map(super::normalize::google_member).collect();
            let mut req = json!({"destinations": [t.google_destination(aud)], "audienceMembers": members, "encoding": "HEX"});
            let path = if op == Op::Add {
                // The person accepted Google's Customer Match terms when creating the sync.
                req["termsOfService"] = json!({"customerMatchTermsOfServiceStatus": "ACCEPTED"});
                "audienceMembers:ingest"
            } else {
                "audienceMembers:remove"
            };
            let rb = state.http.post(format!("{}/{path}", h.datamanager)).timeout(Duration::from_secs(60)).json(&req);
            let res = t.google_req(rb).send().await.map_err(|e| network(t.platform, e))?;
            let (status, v) = body(t.platform, res).await?;
            let v = google_ok(status, &v)?;
            Ok(BatchResult { invalid: v["fieldWarnings"].as_array().map(|w| w.len() as i64).unwrap_or(0), created_audience_id: None })
        }
        Platform::Tiktok => {
            let lines: Vec<&str> = ids.iter().filter_map(|i| i["email"].as_str()).collect();
            let file_path = tiktok_upload(state, t, &lines).await?;
            match audience_id {
                None if op == Op::Add => {
                    let res = state
                        .http
                        .post(format!("{}/dmp/custom_audience/create/", h.tiktok))
                        .header("Access-Token", t.token)
                        .json(&json!({"advertiser_id": t.ad_account_id, "custom_audience_name": new_audience_name, "file_paths": [file_path], "calculate_type": "EMAIL_SHA256"}))
                        .send()
                        .await
                        .map_err(|e| network(t.platform, e))?;
                    let (status, v) = body(t.platform, res).await?;
                    let data = tiktok_ok(status, &v)?;
                    let id = data["custom_audience_id"].as_str().map(String::from).or_else(|| data["custom_audience_id"].as_i64().map(|n| n.to_string()));
                    Ok(BatchResult { invalid: 0, created_audience_id: id })
                }
                None => Ok(BatchResult::default()),
                Some(aud) => {
                    let action = if op == Op::Add { "APPEND" } else { "REMOVE" };
                    let res = state
                        .http
                        .post(format!("{}/dmp/custom_audience/update/", h.tiktok))
                        .header("Access-Token", t.token)
                        .json(&json!({"advertiser_id": t.ad_account_id, "custom_audience_id": aud, "file_paths": [file_path], "action": action}))
                        .send()
                        .await
                        .map_err(|e| network(t.platform, e))?;
                    let (status, v) = body(t.platform, res).await?;
                    tiktok_ok(status, &v)?;
                    Ok(BatchResult::default())
                }
            }
        }
    }
}

/// Upload a customer file of SHA-256 email hashes, one per line; returns TikTok's file_path.
async fn tiktok_upload(state: &AppState, t: &Target<'_>, lines: &[&str]) -> PResult<String> {
    use md5::Md5;
    let mut file = lines.join("\n");
    file.push('\n');
    let signature = hex::encode(Md5::digest(file.as_bytes()));
    let boundary = format!("dumbmail{}", uuid::Uuid::new_v4().simple());
    let mut body_bytes = Vec::with_capacity(file.len() + 1024);
    for (k, v) in [("advertiser_id", t.ad_account_id), ("calculate_type", "EMAIL_SHA256"), ("file_signature", signature.as_str())] {
        body_bytes.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    body_bytes.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"dumbmail-audience.csv\"\r\nContent-Type: text/csv\r\n\r\n").as_bytes());
    body_bytes.extend_from_slice(file.as_bytes());
    body_bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let res = state
        .http
        .post(format!("{}/dmp/custom_audience/file/upload/", hosts(&state.cfg).tiktok))
        .timeout(Duration::from_secs(120))
        .header("Access-Token", t.token)
        .header(reqwest::header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| network(t.platform, e))?;
    let (status, v) = body(t.platform, res).await?;
    let data = tiktok_ok(status, &v)?;
    data["file_path"].as_str().map(String::from).ok_or_else(|| PErr::Invalid("TikTok didn't accept the customer file.".into()))
}

/// What the platform reports about an audience.
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
                (_, Some(471)) => "Flagged by Meta for review".to_string(),
                (Some(300), _) => "Too small to use yet".to_string(),
                (Some(c), _) if c >= 400 => short(v["delivery_status"]["description"].as_str().unwrap_or("Not usable for ads")),
                (_, Some(411)) => "Ready, low match rate".to_string(),
                (_, Some(441)) => "Still filling, usable".to_string(),
                (Some(200), _) => "Ready".to_string(),
                _ => "Processing".to_string(),
            };
            Ok(Stats { size: num("approximate_count_lower_bound"), size_upper: num("approximate_count_upper_bound"), match_rate: None, status: Some(status) })
        }
        Platform::Google => {
            let rb = state.http.get(format!("{}/{}/userLists/{audience_id}", h.datamanager, t.google_parent()));
            let res = t.google_req(rb).send().await.map_err(|e| network(t.platform, e))?;
            let (status, v) = body(t.platform, res).await?;
            let v = google_ok(status, &v)?;
            let count = |k: &str| v["sizeInfo"][k].as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| v["sizeInfo"][k].as_i64());
            let search = count("searchNetworkMembersCount");
            let display = count("displayNetworkMembersCount");
            let size = match (search, display) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            let rate = v["ingestedUserListInfo"]["contactIdInfo"]["matchRatePercentage"].as_f64().map(|r| r as f32);
            let status = match size {
                Some(n) if n >= 1000 => "Ready",
                Some(n) if n >= 100 => "Ready for Display and YouTube; Search needs 1,000",
                Some(_) => "Too small to use yet (Google needs 100 people)",
                None => "Processing",
            };
            Ok(Stats { size, size_upper: None, match_rate: rate, status: Some(status.into()) })
        }
        Platform::Tiktok => {
            let ids = json!([audience_id]).to_string();
            let res = state
                .http
                .get(format!("{}/dmp/custom_audience/get/", h.tiktok))
                .header("Access-Token", t.token)
                .query(&[("advertiser_id", t.ad_account_id), ("custom_audience_ids", ids.as_str())])
                .send()
                .await
                .map_err(|e| network(t.platform, e))?;
            let (status, v) = body(t.platform, res).await?;
            let data = tiktok_ok(status, &v)?;
            let d = &data["list"][0]["audience_details"];
            let size = d["cover_num"].as_i64();
            let status = if d["is_expired"].as_bool() == Some(true) {
                "Expired on TikTok"
            } else if d["is_valid"].as_bool() == Some(true) {
                "Ready"
            } else if size.is_some_and(|n| n < 1000) {
                "Too small to use yet (TikTok needs 1,000 people)"
            } else {
                "Processing"
            };
            Ok(Stats { size, size_upper: None, match_rate: None, status: Some(status.into()) })
        }
    }
}

/// Delete the audience on the platform. Ads that use it stop.
pub async fn delete_audience(state: &AppState, t: &Target<'_>, audience_id: &str) -> PResult<()> {
    let h = hosts(&state.cfg);
    match t.platform {
        Platform::Meta => {
            let a = app(&state.cfg, Platform::Meta).ok_or_else(|| unavailable(Platform::Meta))?;
            let res = state
                .http
                .delete(format!("{}/{audience_id}", h.meta))
                .query(&[("access_token", t.token), ("appsecret_proof", &appsecret_proof(a.secret, t.token))])
                .send()
                .await
                .map_err(|e| network(t.platform, e))?;
            let (status, v) = body(t.platform, res).await?;
            if v["error"]["code"].as_i64() == Some(2656) {
                return Err(PErr::Invalid("Meta won't delete this audience while lookalike audiences are built from it. Delete those first, or keep the audience.".into()));
            }
            meta_ok(status, &v).map(|_| ())
        }
        Platform::Google => {
            let rb = state.http.delete(format!("{}/{}/userLists/{audience_id}", h.datamanager, t.google_parent()));
            let res = t.google_req(rb).send().await.map_err(|e| network(t.platform, e))?;
            let (status, v) = body(t.platform, res).await?;
            google_ok(status, &v).map(|_| ())
        }
        Platform::Tiktok => {
            let res = state
                .http
                .post(format!("{}/dmp/custom_audience/delete/", h.tiktok))
                .header("Access-Token", t.token)
                .json(&json!({"advertiser_id": t.ad_account_id, "custom_audience_ids": [audience_id]}))
                .send()
                .await
                .map_err(|e| network(t.platform, e))?;
            let (status, v) = body(t.platform, res).await?;
            tiktok_ok(status, &v).map(|_| ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_mapping() {
        let e = |code: i64, sub: i64| meta_ok(400, &json!({"error": {"code": code, "error_subcode": sub, "message": "x"}})).unwrap_err();
        assert!(matches!(e(190, 0), PErr::Auth(_)));
        assert!(matches!(e(80003, 0), PErr::RateLimited(_)));
        assert!(matches!(e(200, 1870090), PErr::Permission(m) if m.contains("terms")));
        assert!(matches!(e(100, 0), PErr::Invalid(_)));
        assert!(matches!(google_ok(429, &json!({"error": {"status": "RESOURCE_EXHAUSTED"}})), Err(PErr::RateLimited(_))));
        assert!(matches!(google_ok(403, &json!({"error": {"status": "PERMISSION_DENIED", "message": "no"}})), Err(PErr::Permission(_))));
        assert!(matches!(google_token_ok(400, &json!({"error": "invalid_grant"})), Err(PErr::Auth(_))));
        assert!(matches!(tiktok_ok(200, &json!({"code": 40100, "message": "slow"})), Err(PErr::RateLimited(_))));
        assert!(matches!(tiktok_ok(200, &json!({"code": 40105, "message": "bad token"})), Err(PErr::Auth(_))));
        assert_eq!(tiktok_ok(200, &json!({"code": 0, "data": {"a": 1}})).unwrap()["a"], 1);
    }

    #[test]
    fn pkce() {
        // S256: unpadded base64url of the SHA-256 of the verifier (43 characters).
        let c = pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r7wW1gFWFOEjXk");
        assert_eq!(c, "bwWFMyPfdG9qreDhH2lmftFx_dFeLDalzcT1gb_j68g");
        assert_eq!(c.len(), 43);
    }
}
