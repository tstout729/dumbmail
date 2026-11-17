//! Single sign-on with OpenID Connect (authorization code flow with PKCE, state and nonce). The
//! `openidconnect` crate does discovery and checks the ID token's signature (against the
//! provider's published keys) and its issuer, audience, expiry and nonce.
//!
//! Trust is kept narrow:
//! - only owners set it up;
//! - each email domain an account's SSO covers must be one of its verified sending domains
//!   (exactly), and only one account can claim a domain;
//! - it links to an existing login only when that login is already a member of the account;
//!   anyone else with a login is invited first. So an identity provider can't take over a login
//!   it doesn't already share an account with, or one squatted before the company set it up;
//! - an identity provider can only sign people in with emails on those domains;
//! - a single sign-on session opens only that account. It can't open the person's other accounts
//!   or change their password, two-factor or sessions (see `access::allows`).
//!
//! All calls to the provider go to an address the customer typed, so they use the same protections
//! as webhooks: public addresses only, pinned DNS, no redirects, a timeout and a size cap.
//! SAML isn't supported.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Redirect;
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::access::Role;
use super::audit::{audit, record};
use super::login::{self, ReqMeta};
use super::{mail, ratelimit};
use crate::api::account::{session_cookie, with_cookie_domain};
use crate::api::ApiJson;
use crate::auth::{self, Ctx};
use crate::error::{AppError, AppResult};
use crate::models::normalize_email;
use crate::state::AppState;

const SSO_COOKIE: &str = "pl_sso";
const SSO_COOKIE_PATH: &str = "/api/auth/sso";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
const MAX_RESPONSE: usize = 1024 * 1024;

static ALLOW_PRIVATE_HOSTS: AtomicBool = AtomicBool::new(false);

/// Tests run their identity provider on 127.0.0.1, which the address check refuses. Only test
/// code calls this; there's no setting for it.
#[doc(hidden)]
pub fn allow_private_hosts_for_tests() {
    ALLOW_PRIVATE_HOSTS.store(true, Ordering::SeqCst);
}

fn private_ok() -> bool {
    ALLOW_PRIVATE_HOSTS.load(Ordering::SeqCst)
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(String);

/// The HTTP client handed to `openidconnect`.
async fn http(req: openidconnect::HttpRequest) -> Result<openidconnect::HttpResponse, HttpError> {
    let url = req.uri().to_string();
    let (host, addr) = if private_ok() {
        let u = url::Url::parse(&url).map_err(|_| HttpError("bad url".into()))?;
        let host = u.host_str().ok_or_else(|| HttpError("no host".into()))?.trim_matches(['[', ']']).to_string();
        let port = u.port_or_known_default().unwrap_or(443);
        let addr = tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| HttpError(e.to_string()))?.next().ok_or_else(|| HttpError("no address".into()))?;
        (host, addr)
    } else {
        if !url.starts_with("https://") {
            return Err(HttpError("identity provider addresses must use https".into()));
        }
        crate::flows::check_webhook_url(&url).await.map_err(|e| HttpError(e.public_message()))?
    };
    let client = reqwest::Client::builder()
        .resolve(&host, addr)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("DumbMail SSO")
        .build()
        .map_err(|e| HttpError(e.to_string()))?;
    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes()).map_err(|e| HttpError(e.to_string()))?;
    let mut rb = client.request(method, &url);
    for (k, v) in req.headers() {
        rb = rb.header(k.as_str(), v.as_bytes());
    }
    let mut res = rb.body(req.body().clone()).send().await.map_err(|e| HttpError(e.without_url().to_string()))?;
    let mut out = openidconnect::http::Response::builder().status(res.status().as_u16());
    for (k, v) in res.headers() {
        out = out.header(k.as_str(), v.as_bytes());
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| HttpError(e.without_url().to_string()))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_RESPONSE {
            return Err(HttpError("identity provider response too large".into()));
        }
    }
    out.body(body).map_err(|e| HttpError(e.to_string()))
}

async fn discover(issuer: &str) -> Result<CoreProviderMetadata, String> {
    let url = IssuerUrl::new(issuer.to_string()).map_err(|_| "the issuer URL isn't valid".to_string())?;
    let client = |r| http(r);
    CoreProviderMetadata::discover_async(url, &client).await.map_err(|e| {
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            msg = format!("{msg}: {s}");
            src = s.source();
        }
        msg
    })
}

fn redirect_uri(state: &AppState) -> String {
    format!("{}/api/auth/sso/callback", state.cfg.public_url())
}

// ---- Settings ----

#[derive(sqlx::FromRow)]
struct Config {
    account_id: Uuid,
    enabled: bool,
    issuer: String,
    client_id: String,
    client_secret_enc: String,
    secret_hint: String,
    domains: Vec<String>,
    enforce: bool,
    auto_join: bool,
    default_role: String,
}

async fn load(state: &AppState, account_id: Uuid) -> AppResult<Option<Config>> {
    Ok(sqlx::query_as("SELECT account_id, enabled, issuer, client_id, client_secret_enc, secret_hint, domains, enforce, auto_join, default_role FROM account_sso WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?)
}

/// The account's SSO settings without the secret, or null.
pub async fn summary(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    Ok(match load(state, account_id).await? {
        Some(c) => json!({
            "enabled": c.enabled, "issuer": c.issuer, "client_id": c.client_id, "client_secret_hint": c.secret_hint, "domains": c.domains,
            "enforce": c.enforce, "auto_join": c.auto_join, "default_role": c.default_role,
            "redirect_uri": redirect_uri(state), "login_url": format!("{}/api/auth/sso?account={}", state.cfg.public_url(), c.account_id),
        }),
        None => Value::Null,
    })
}

pub async fn get_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(json!({"sso": summary(&state, ctx.account_id).await?, "redirect_uri": redirect_uri(&state)})))
}

#[derive(Deserialize)]
pub struct SsoBody {
    pub issuer: String,
    pub client_id: String,
    /// Required the first time; leave out to keep the saved one.
    pub client_secret: Option<String>,
    pub domains: Vec<String>,
    #[serde(default)]
    pub enforce: bool,
    #[serde(default = "yes")]
    pub auto_join: bool,
    #[serde(default = "analyst")]
    pub default_role: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}
fn analyst() -> String {
    "analyst".into()
}

fn clean_domain(d: &str) -> Option<String> {
    let d = d.trim().trim_start_matches('@').trim_end_matches('.').to_ascii_lowercase();
    let ok = d.len() <= 253 && d.contains('.') && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !l.starts_with('-'));
    ok.then_some(d)
}

/// `PUT /api/team/sso`: save the identity provider (checked by reading its discovery document).
pub async fn put_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<SsoBody>) -> AppResult<Json<Value>> {
    let issuer = b.issuer.trim().trim_end_matches('/').to_string();
    let parsed = url::Url::parse(&issuer).map_err(|_| AppError::bad("enter the issuer URL your identity provider shows, like https://acme.okta.com"))?;
    if issuer.len() > 300 || !(parsed.scheme() == "https" || (private_ok() && parsed.scheme() == "http")) {
        return Err(AppError::bad("the issuer URL must start with https://"));
    }
    let client_id = b.client_id.trim().to_string();
    if client_id.is_empty() || client_id.len() > 300 {
        return Err(AppError::bad("enter the client ID"));
    }
    let role = Role::parse(&b.default_role).filter(|r| matches!(r, Role::Manager | Role::Analyst | Role::Content)).ok_or_else(|| AppError::bad("people who join through SSO can be a manager, analyst or content member"))?;
    if b.domains.is_empty() || b.domains.len() > 10 {
        return Err(AppError::bad("list between 1 and 10 email domains"));
    }
    let mut domains = Vec::new();
    for d in &b.domains {
        let d = clean_domain(d).ok_or_else(|| AppError::bad(format!("{d:?} isn't a domain")))?;
        if !domains.contains(&d) {
            domains.push(d);
        }
    }
    for d in &domains {
        let verified: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sending_domains WHERE account_id = $1 AND status = 'verified' AND domain = $2)")
            .bind(ctx.account_id)
            .bind(d)
            .fetch_one(&state.db)
            .await?;
        if !verified {
            return Err(AppError::bad(format!("verify {d} itself as a sending domain first (Settings → Domains); that's how we know it's yours")));
        }
        let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM account_sso WHERE account_id <> $1 AND $2 = ANY(domains))").bind(ctx.account_id).bind(d).fetch_one(&state.db).await?;
        if taken {
            return Err(AppError::Conflict(format!("another account already uses single sign-on for {d}")));
        }
    }
    let existing = load(&state, ctx.account_id).await?;
    let (secret_enc, hint) = match b.client_secret.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(s) if s.len() <= 1000 => (crate::crypto::encrypt(&state.cfg.secret_key, &s)?, format!("…{}", s.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<String>())),
        Some(_) => return Err(AppError::bad("that client secret is too long")),
        None => match &existing {
            Some(c) => (c.client_secret_enc.clone(), c.secret_hint.clone()),
            None => return Err(AppError::bad("enter the client secret")),
        },
    };
    let meta = discover(&issuer).await.map_err(|e| AppError::bad(format!("couldn't read the identity provider's settings at {issuer}/.well-known/openid-configuration ({})", mail::plain(&e, 200))))?;
    if meta.token_endpoint().is_none() {
        return Err(AppError::bad("that identity provider doesn't list a token endpoint"));
    }
    sqlx::query(
        "INSERT INTO account_sso (account_id, enabled, issuer, client_id, client_secret_enc, secret_hint, domains, enforce, auto_join, default_role)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (account_id) DO UPDATE SET enabled = $2, issuer = $3, client_id = $4, client_secret_enc = $5, secret_hint = $6, domains = $7,
            enforce = $8, auto_join = $9, default_role = $10, updated_at = now()",
    )
    .bind(ctx.account_id)
    .bind(b.enabled)
    .bind(&issuer)
    .bind(&client_id)
    .bind(&secret_enc)
    .bind(&hint)
    .bind(&domains)
    .bind(b.enforce)
    .bind(b.auto_join)
    .bind(role.as_str())
    .execute(&state.db)
    .await?;
    audit(&state, &ctx, if existing.is_some() { "sso.update" } else { "sso.configure" }, Some(&issuer), json!({"domains": domains, "enforce": b.enforce, "auto_join": b.auto_join, "default_role": role.as_str(), "enabled": b.enabled})).await;
    Ok(Json(json!({"sso": summary(&state, ctx.account_id).await?, "redirect_uri": redirect_uri(&state)})))
}

pub async fn delete_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let n = sqlx::query("DELETE FROM account_sso WHERE account_id = $1").bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("single sign-on settings"));
    }
    audit(&state, &ctx, "sso.remove", None, json!({})).await;
    Ok(Json(json!({"sso": null, "redirect_uri": redirect_uri(&state)})))
}

// ---- Signing in ----

#[derive(Deserialize)]
pub struct StartQuery {
    pub email: Option<String>,
    pub account: Option<Uuid>,
    pub next: Option<String>,
}

fn login_error(state: &AppState, code: &str) -> Redirect {
    Redirect::to(&format!("{}/login?error={code}", state.cfg.public_url()))
}

fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && n.len() < 300 && !n.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') => n.to_string(),
        _ => "/".into(),
    }
}

/// `GET /api/auth/sso?email=…` (or `?account=<id>`): send the person to their identity provider.
pub async fn start_h(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar, Query(q): Query<StartQuery>) -> (CookieJar, Redirect) {
    if ratelimit::hit_ip(&state, "sso_start", login::client_ip(&headers).as_deref(), 30, 900).await.is_err() {
        return (jar, login_error(&state, "sso_busy"));
    }
    let email = q.email.as_deref().and_then(normalize_email);
    let account_id: Option<Uuid> = match (q.account, &email) {
        (Some(a), _) => Some(a),
        (None, Some(e)) => {
            let domain = e.rsplit('@').next().unwrap_or("").to_string();
            sqlx::query_scalar("SELECT account_id FROM account_sso WHERE enabled AND $1 = ANY(domains) LIMIT 1").bind(domain).fetch_optional(&state.db).await.ok().flatten()
        }
        _ => None,
    };
    let Some(cfg) = (match account_id {
        Some(a) => load(&state, a).await.ok().flatten().filter(|c| c.enabled),
        None => None,
    }) else {
        return (jar, login_error(&state, "sso_unavailable"));
    };
    let Ok(meta) = discover(&cfg.issuer).await else {
        tracing::warn!(account = %cfg.account_id, "SSO discovery failed");
        return (jar, login_error(&state, "sso_failed"));
    };
    let Ok(redirect) = RedirectUrl::new(redirect_uri(&state)) else { return (jar, login_error(&state, "sso_failed")) };
    let client = CoreClient::from_provider_metadata(meta, ClientId::new(cfg.client_id.clone()), None).set_redirect_uri(redirect);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut req = client
        .authorize_url(CoreAuthenticationFlow::AuthorizationCode, CsrfToken::new_random, Nonce::new_random)
        .add_scope(Scope::new("email".into()))
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge);
    if let Some(e) = &email {
        req = req.add_extra_param("login_hint", e.clone());
    }
    let (url, csrf, nonce) = req.url();
    let next = B64.encode(safe_next(q.next.as_deref()));
    let payload = format!("{}|{}|{}|{}|{next}", cfg.account_id, csrf.secret(), nonce.secret(), verifier.secret());
    let cookie = Cookie::build((SSO_COOKIE, auth::sign(&state.cfg.secret_key, &payload)))
        .path(SSO_COOKIE_PATH)
        .http_only(true)
        .secure(state.cfg.secure_cookies)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::minutes(10))
        .build();
    (jar.add(with_cookie_domain(&state, cookie)), Redirect::to(url.as_str()))
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// `GET /api/auth/sso/callback`: finish the sign-in. Problems go back to the login page as a code.
pub async fn callback_h(State(state): State<AppState>, headers: HeaderMap, jar: CookieJar, Query(q): Query<CallbackQuery>) -> (CookieJar, Redirect) {
    let cookie = jar.get(SSO_COOKIE).map(|c| c.value().to_string());
    let jar = jar.remove(with_cookie_domain(&state, Cookie::build(SSO_COOKIE).path(SSO_COOKIE_PATH).build()));
    let meta = ReqMeta::from_headers(&headers);
    if ratelimit::hit_ip(&state, "sso_callback", meta.ip.as_deref(), 30, 900).await.is_err() {
        return (jar, login_error(&state, "sso_busy"));
    }
    match finish(&state, cookie, q, &meta).await {
        Ok((user_id, account_id, next)) => match login::start_session(&state, user_id, account_id, "sso", Some(account_id), &meta).await {
            Ok(token) => {
                record(&state, account_id, &meta.actor(Some(user_id)), "login", None, json!({"method": "sso"})).await;
                (jar.add(session_cookie(&state, token)), Redirect::to(&format!("{}{next}", state.cfg.public_url())))
            }
            Err(_) => (jar, login_error(&state, "sso_failed")),
        },
        Err((code, account)) => {
            if let Some(a) = account {
                record(&state, a, &meta.actor(None), "login.failed", None, json!({"method": "sso", "reason": code})).await;
            }
            (jar, login_error(&state, code))
        }
    }
}

type Fail = (&'static str, Option<Uuid>);

async fn finish(state: &AppState, cookie: Option<String>, q: CallbackQuery, _meta: &ReqMeta) -> Result<(Uuid, Uuid, String), Fail> {
    let payload = cookie.and_then(|c| auth::verify(&state.cfg.secret_key, &c)).ok_or(("sso_expired", None))?;
    let parts: Vec<&str> = payload.split('|').collect();
    let [account, csrf, nonce, verifier, next] = parts.as_slice() else { return Err(("sso_expired", None)) };
    let account_id: Uuid = account.parse().map_err(|_| ("sso_expired", None))?;
    if q.error.is_some() {
        return Err(("sso_cancelled", Some(account_id)));
    }
    if !q.state.as_deref().is_some_and(|s| login::same(s, csrf)) {
        return Err(("sso_expired", Some(account_id)));
    }
    let code = q.code.filter(|c| !c.is_empty() && c.len() < 4096).ok_or(("sso_expired", Some(account_id)))?;
    let next = B64.decode(next).ok().and_then(|b| String::from_utf8(b).ok()).map(|n| safe_next(Some(&n))).unwrap_or_else(|| "/".into());
    let fail = |c: &'static str| (c, Some(account_id));
    let cfg = load(state, account_id).await.ok().flatten().filter(|c| c.enabled).ok_or(fail("sso_unavailable"))?;
    let secret = crate::crypto::decrypt(&state.cfg.secret_key, &cfg.client_secret_enc).map_err(|_| fail("sso_failed"))?;
    let meta = discover(&cfg.issuer).await.map_err(|_| fail("sso_failed"))?;
    let redirect = RedirectUrl::new(redirect_uri(state)).map_err(|_| fail("sso_failed"))?;
    let client = CoreClient::from_provider_metadata(meta, ClientId::new(cfg.client_id.clone()), Some(ClientSecret::new(secret))).set_redirect_uri(redirect);
    let http_client = |r| http(r);
    let token = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| fail("sso_failed"))?
        .set_pkce_verifier(PkceCodeVerifier::new(verifier.to_string()))
        .request_async(&http_client)
        .await
        .map_err(|e| {
            tracing::warn!(account = %account_id, error = %e, "SSO code exchange failed");
            fail("sso_failed")
        })?;
    let id_token = token.id_token().ok_or(fail("sso_failed"))?;
    let verifier = client.id_token_verifier();
    let claims = id_token.claims(&verifier, &Nonce::new(nonce.to_string())).map_err(|e| {
        tracing::warn!(account = %account_id, error = %e, "SSO ID token rejected");
        fail("sso_failed")
    })?;
    if let Some(expected) = claims.access_token_hash() {
        let alg = id_token.signing_alg().map_err(|_| fail("sso_failed"))?;
        let key = id_token.signing_key(&verifier).map_err(|_| fail("sso_failed"))?;
        let actual = AccessTokenHash::from_token(token.access_token(), alg, key).map_err(|_| fail("sso_failed"))?;
        if actual != *expected {
            return Err(fail("sso_failed"));
        }
    }
    if claims.email_verified() == Some(false) {
        return Err(fail("sso_unverified"));
    }
    let email = claims.email().and_then(|e| normalize_email(e.as_str())).ok_or(fail("sso_unverified"))?;
    let domain = email.rsplit('@').next().unwrap_or("");
    if !cfg.domains.iter().any(|d| d.eq_ignore_ascii_case(domain)) {
        return Err(fail("sso_domain"));
    }
    let subject = claims.subject().as_str().to_string();
    let name = claims.name().and_then(|n| n.get(None)).map(|n| mail::plain(n.as_str(), 100)).filter(|n| !n.is_empty());
    let user_id = match link_user(state, &cfg, &subject, &email, name).await {
        Ok(Some(uid)) => uid,
        Ok(None) => return Err(fail("sso_not_member")),
        Err(_) => return Err(fail("sso_failed")),
    };
    let member: Option<String> = sqlx::query_scalar("SELECT role FROM memberships WHERE user_id = $1 AND account_id = $2").bind(user_id).bind(account_id).fetch_optional(&state.db).await.map_err(|_| fail("sso_failed"))?;
    if member.is_none() {
        if !cfg.auto_join {
            return Err(fail("sso_not_member"));
        }
        sqlx::query("INSERT INTO memberships (user_id, account_id, role) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(user_id)
            .bind(account_id)
            .bind(&cfg.default_role)
            .execute(&state.db)
            .await
            .map_err(|_| fail("sso_failed"))?;
        record(state, account_id, &super::audit::Actor { user_id: Some(user_id), ..Default::default() }, "team.join", Some(&email), json!({"role": cfg.default_role, "via": "sso"})).await;
    }
    Ok((user_id, account_id, next))
}

/// The login for this identity: one already linked to it; else the login with this email, but only
/// when it's already a member of this account; else a new login (with auto-join). `None`: there's a
/// login for this email outside the account (they need an invite first), or auto-join is off.
async fn link_user(state: &AppState, cfg: &Config, subject: &str, email: &str, name: Option<String>) -> AppResult<Option<Uuid>> {
    if let Some(uid) = sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM sso_identities WHERE issuer = $1 AND subject = $2").bind(&cfg.issuer).bind(subject).fetch_optional(&state.db).await? {
        return Ok(Some(uid));
    }
    let uid = match sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = $1").bind(email).fetch_optional(&state.db).await? {
        Some(uid) => {
            let member: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND account_id = $2)").bind(uid).bind(cfg.account_id).fetch_one(&state.db).await?;
            if !member {
                return Ok(None);
            }
            uid
        }
        None if cfg.auto_join => {
            let uid = Uuid::now_v7();
            sqlx::query("INSERT INTO users (id, email, name) VALUES ($1, $2, $3)").bind(uid).bind(email).bind(name).execute(&state.db).await?;
            uid
        }
        None => return Ok(None),
    };
    sqlx::query("INSERT INTO sso_identities (issuer, subject, user_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING").bind(&cfg.issuer).bind(subject).bind(uid).execute(&state.db).await?;
    Ok(Some(uid))
}

#[cfg(test)]
mod tests {
    #[test]
    fn domains() {
        assert_eq!(super::clean_domain(" @Acme.com. ").as_deref(), Some("acme.com"));
        assert!(super::clean_domain("localhost").is_none());
        assert!(super::clean_domain("a..b").is_none());
        assert!(super::clean_domain("evil.com/x").is_none());
    }
}
