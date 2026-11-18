//! Single sign-on with OpenID Connect (authorization code flow with PKCE, state and nonce). The
//! `openidconnect` crate does discovery and checks the ID token's signature (against the
//! provider's published keys) and its issuer, audience, expiry and nonce.
//!
//! Trust is kept narrow:
//! - only owners set it up;
//! - each email domain an account's SSO covers must be one of its verified sending domains
//!   (exactly), and only one account can claim a domain;
//! - it links to an existing login only when that login is already a member of the account and
//!   the ID token says the email is verified (`email_verified: true`; a missing claim counts as
//!   unverified unless the owner said this provider only issues verified emails). Anyone else with
//!   a login is invited first. So an identity provider can't take over a login it doesn't already
//!   share an account with, or one squatted before the company set it up;
//! - a login linked to one identity at a provider is never moved to another by an email match;
//!   an owner resets the link first (`DELETE /api/team/members/{user_id}/sso`). Links belong to
//!   the account: the same provider used by another account has its own links;
//! - someone removed from the account isn't added back by just-in-time joining until an owner
//!   or admin invites them again (`team_removals`);
//! - when the account requires two-factor login, a sign-in counts only if the ID token's `amr`
//!   claim shows two factors (`mfa`, or two different kinds, see [`amr_shows_second_factor`]),
//!   or the owner said the provider enforces one; otherwise the person also enters their
//!   DumbMail code;
//! - the settings carry a revision that goes up whenever something that decides who gets in
//!   changes (provider, client, secret, domains, turning it off, the verified-email and two-step
//!   promises). A sign-in started under an older revision gets no session, and sessions started
//!   under one stop working; removing single sign-on ends them too;
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
    verified_emails_only: bool,
    idp_enforces_mfa: bool,
    revision: i32,
}

async fn load(state: &AppState, account_id: Uuid) -> AppResult<Option<Config>> {
    Ok(sqlx::query_as(
        "SELECT account_id, enabled, issuer, client_id, client_secret_enc, secret_hint, domains, enforce, auto_join, default_role, verified_emails_only, idp_enforces_mfa, revision
         FROM account_sso WHERE account_id = $1",
    )
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
            "verified_emails_only": c.verified_emails_only, "idp_enforces_mfa": c.idp_enforces_mfa,
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
    /// This provider only issues verified emails (for providers that never send `email_verified`).
    #[serde(default)]
    pub verified_emails_only: bool,
    /// This provider makes everyone use two-step verification.
    #[serde(default)]
    pub idp_enforces_mfa: bool,
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
    // Anything that decides who gets in: a new revision, so sign-ins started under the old
    // settings get no session and sessions they started end (in the same transaction). Only the
    // enforcement, joining and default-role settings change without one.
    let replaced = existing.as_ref().is_some_and(|c| {
        let plain = |enc: &str| crate::crypto::decrypt(&state.cfg.secret_key, enc).ok();
        let new_secret = c.client_secret_enc != secret_enc && plain(&c.client_secret_enc).zip(plain(&secret_enc)).is_none_or(|(old, new)| !login::same(&old, &new));
        c.issuer != issuer
            || c.client_id != client_id
            || new_secret
            || c.enabled != b.enabled
            || c.domains != domains
            || c.verified_emails_only != b.verified_emails_only
            || c.idp_enforces_mfa != b.idp_enforces_mfa
    });
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO account_sso (account_id, enabled, issuer, client_id, client_secret_enc, secret_hint, domains, enforce, auto_join, default_role, verified_emails_only, idp_enforces_mfa)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         ON CONFLICT (account_id) DO UPDATE SET enabled = $2, issuer = $3, client_id = $4, client_secret_enc = $5, secret_hint = $6, domains = $7,
            enforce = $8, auto_join = $9, default_role = $10, verified_emails_only = $11, idp_enforces_mfa = $12, updated_at = now(),
            revision = account_sso.revision + CASE WHEN $13 THEN 1 ELSE 0 END",
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
    .bind(b.verified_emails_only)
    .bind(b.idp_enforces_mfa)
    .bind(replaced)
    .execute(&mut *tx)
    .await?;
    let ended = if replaced { end_sso_sessions(&mut tx, ctx.account_id).await? } else { 0 };
    tx.commit().await?;
    audit(
        &state,
        &ctx,
        if existing.is_some() { "sso.update" } else { "sso.configure" },
        Some(&issuer),
        json!({"domains": domains, "enforce": b.enforce, "auto_join": b.auto_join, "default_role": role.as_str(), "enabled": b.enabled,
               "verified_emails_only": b.verified_emails_only, "idp_enforces_mfa": b.idp_enforces_mfa, "sessions_ended": ended}),
    )
    .await;
    Ok(Json(json!({"sso": summary(&state, ctx.account_id).await?, "redirect_uri": redirect_uri(&state)})))
}

/// End every session this account's single sign-on started.
async fn end_sso_sessions(tx: &mut sqlx::PgConnection, account_id: Uuid) -> AppResult<u64> {
    Ok(sqlx::query("DELETE FROM sessions WHERE sso_account_id = $1").bind(account_id).execute(&mut *tx).await?.rows_affected())
}

pub async fn delete_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let mut tx = state.db.begin().await?;
    let n = sqlx::query("DELETE FROM account_sso WHERE account_id = $1").bind(ctx.account_id).execute(&mut *tx).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("single sign-on settings"));
    }
    let ended = end_sso_sessions(&mut tx, ctx.account_id).await?;
    tx.commit().await?;
    audit(&state, &ctx, "sso.remove", None, json!({"sessions_ended": ended})).await;
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
    let payload = format!("{}|{}|{}|{}|{next}|{}", cfg.account_id, csrf.secret(), nonce.secret(), verifier.secret(), cfg.revision);
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
        Ok(Done { user_id, account_id, next, mfa, revision }) => match login::start_session_with(
            &state,
            user_id,
            account_id,
            "sso",
            Some(account_id),
            &meta,
            login::SessionStart { mfa_ok: mfa, sso_revision: Some(revision), ..Default::default() },
        )
        .await
        {
            Ok(token) => {
                record(&state, account_id, &meta.actor(Some(user_id)), "login", None, json!({"method": "sso", "second_factor": mfa})).await;
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

struct Done {
    user_id: Uuid,
    account_id: Uuid,
    next: String,
    /// The sign-in had a second factor (by the `amr` claim, or the owner's word for the provider).
    mfa: bool,
    /// The settings revision the sign-in started (and finished) under.
    revision: i32,
}

/// `amr` values (RFC 8176) by the kind of factor they are: something known, something held,
/// something you are.
const KNOWN_AMR: [&str; 3] = ["pwd", "pin", "kba"];
const HELD_AMR: [&str; 7] = ["otp", "hwk", "swk", "sms", "tel", "sc", "pop"];
const INHERENT_AMR: [&str; 5] = ["fpt", "face", "iris", "retina", "vbm"];

/// Whether an ID token's `amr` shows two factors: `mfa` itself, or at least two different kinds
/// (a password and a one-time code, say). One method on its own, even a code or a security key,
/// isn't proof of two.
pub fn amr_shows_second_factor<'a>(amr: impl IntoIterator<Item = &'a str>) -> bool {
    let (mut known, mut held, mut inherent) = (false, false, false);
    for v in amr {
        let v = v.trim().to_ascii_lowercase();
        if v == "mfa" {
            return true;
        }
        known |= KNOWN_AMR.contains(&v.as_str());
        held |= HELD_AMR.contains(&v.as_str());
        inherent |= INHERENT_AMR.contains(&v.as_str());
    }
    [known, held, inherent].iter().filter(|k| **k).count() >= 2
}

async fn finish(state: &AppState, cookie: Option<String>, q: CallbackQuery, _meta: &ReqMeta) -> Result<Done, Fail> {
    let payload = cookie.and_then(|c| auth::verify(&state.cfg.secret_key, &c)).ok_or(("sso_expired", None))?;
    let parts: Vec<&str> = payload.split('|').collect();
    let [account, csrf, nonce, verifier, next, revision] = parts.as_slice() else { return Err(("sso_expired", None)) };
    let account_id: Uuid = account.parse().map_err(|_| ("sso_expired", None))?;
    let revision: i32 = revision.parse().map_err(|_| ("sso_expired", None))?;
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
    // The settings changed after this sign-in started: start again under the new ones.
    if cfg.revision != revision {
        return Err(fail("sso_changed"));
    }
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
    // A missing claim counts as unverified, unless the owner said this provider only issues
    // verified emails.
    let verified = claims.email_verified() == Some(true) || cfg.verified_emails_only;
    let mfa = cfg.idp_enforces_mfa || claims.auth_method_refs().is_some_and(|amr| amr_shows_second_factor(amr.iter().map(|r| r.as_str())));
    let email = claims.email().and_then(|e| normalize_email(e.as_str())).ok_or(fail("sso_unverified"))?;
    let domain = email.rsplit('@').next().unwrap_or("");
    if !cfg.domains.iter().any(|d| d.eq_ignore_ascii_case(domain)) {
        return Err(fail("sso_domain"));
    }
    let subject = claims.subject().as_str().to_string();
    let name = claims.name().and_then(|n| n.get(None)).map(|n| mail::plain(n.as_str(), 100)).filter(|n| !n.is_empty());
    match sign_in(state, &cfg, &subject, &email, verified, name).await {
        Ok(Ok(user_id)) => Ok(Done { user_id, account_id, next, mfa, revision }),
        Ok(Err(code)) => Err(fail(code)),
        Err(e) => {
            tracing::warn!(account = %account_id, error = %e, "SSO sign-in failed");
            Err(fail("sso_failed"))
        }
    }
}

/// Find or link the login for this identity and make sure it's a member, all in one transaction.
/// The login is the one already linked to this subject; else the login with this (verified)
/// email, only when it's already a member and isn't linked to another subject at this provider;
/// else a new login (with auto-join). Just-in-time joining refuses anyone removed from the
/// account until they're invited again. `Ok(Err(code))` is a refusal for the login page.
async fn sign_in(state: &AppState, cfg: &Config, subject: &str, email: &str, verified: bool, name: Option<String>) -> AppResult<Result<Uuid, &'static str>> {
    let mut tx = state.db.begin().await?;
    // One sign-in per email at a time, and never alongside a removal of the same person.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("team-member:{}:{email}", cfg.account_id)).execute(&mut *tx).await?;
    let linked: Option<Uuid> = sqlx::query_scalar("SELECT user_id FROM sso_identities WHERE account_id = $1 AND issuer = $2 AND subject = $3")
        .bind(cfg.account_id)
        .bind(&cfg.issuer)
        .bind(subject)
        .fetch_optional(&mut *tx)
        .await?;
    // An open invite for this verified email counts as being let in (even after a removal).
    // Owner invites are accepted through their link only.
    let invite: Option<(Uuid, String)> = if verified {
        sqlx::query_as(
            "SELECT id, role FROM team_invites WHERE account_id = $1 AND email = $2 AND role <> 'owner'
               AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now() FOR UPDATE",
        )
        .bind(cfg.account_id)
        .bind(email)
        .fetch_optional(&mut *tx)
        .await?
    } else {
        None
    };
    let (uid, new_login) = match linked {
        Some(uid) => (uid, false),
        None => {
            if !verified {
                return Ok(Err("sso_unverified"));
            }
            match sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE email = $1 FOR UPDATE").bind(email).fetch_optional(&mut *tx).await? {
                Some(uid) => {
                    let member: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND account_id = $2)").bind(uid).bind(cfg.account_id).fetch_one(&mut *tx).await?;
                    // Never link a login outside the account, even with an invite: whoever made that
                    // login may not be the person the provider vouches for. They accept the invite
                    // signed in to their own login instead.
                    if !member {
                        return Ok(Err("sso_not_member"));
                    }
                    let other: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sso_identities WHERE account_id = $1 AND issuer = $2 AND user_id = $3)")
                        .bind(cfg.account_id)
                        .bind(&cfg.issuer)
                        .bind(uid)
                        .fetch_one(&mut *tx)
                        .await?;
                    if other {
                        return Ok(Err("sso_relink"));
                    }
                    (uid, false)
                }
                None if cfg.auto_join || invite.is_some() => {
                    if invite.is_none() && removed(&mut tx, cfg, subject, email).await? {
                        return Ok(Err("sso_removed"));
                    }
                    let uid: Option<Uuid> = sqlx::query_scalar("INSERT INTO users (id, email, name) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING RETURNING id")
                        .bind(Uuid::now_v7())
                        .bind(email)
                        .bind(name)
                        .fetch_optional(&mut *tx)
                        .await?;
                    let Some(uid) = uid else { return Ok(Err("sso_failed")) };
                    (uid, true)
                }
                None => return Ok(Err("sso_not_member")),
            }
        }
    };
    if linked.is_none() {
        // The primary key (account, issuer, subject) and the unique (account, issuer, user_id)
        // settle a race: whoever loses inserts nothing and is refused.
        let n = sqlx::query("INSERT INTO sso_identities (account_id, issuer, subject, user_id) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
            .bind(cfg.account_id)
            .bind(&cfg.issuer)
            .bind(subject)
            .bind(uid)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n != 1 {
            return Ok(Err("sso_relink"));
        }
    }
    let member: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND account_id = $2)").bind(uid).bind(cfg.account_id).fetch_one(&mut *tx).await?;
    let mut joined_as = None;
    if !member {
        let role = match &invite {
            Some((invite_id, role)) => {
                sqlx::query("UPDATE team_invites SET accepted_at = now(), accepted_by = $2 WHERE id = $1").bind(invite_id).bind(uid).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM team_removals WHERE account_id = $1 AND (email = $2 OR (issuer = $3 AND subject = $4))")
                    .bind(cfg.account_id)
                    .bind(email)
                    .bind(&cfg.issuer)
                    .bind(subject)
                    .execute(&mut *tx)
                    .await?;
                role.clone()
            }
            None => {
                if !cfg.auto_join {
                    return Ok(Err("sso_not_member"));
                }
                if removed(&mut tx, cfg, subject, email).await? {
                    return Ok(Err("sso_removed"));
                }
                cfg.default_role.clone()
            }
        };
        sqlx::query("INSERT INTO memberships (user_id, account_id, role) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(uid)
            .bind(cfg.account_id)
            .bind(&role)
            .execute(&mut *tx)
            .await?;
        joined_as = Some((role, invite.is_some()));
    }
    tx.commit().await?;
    if let Some((role, invited)) = joined_as {
        record(state, cfg.account_id, &super::audit::Actor { user_id: Some(uid), ..Default::default() }, "team.join", Some(email), json!({"role": role, "via": "sso", "new_login": new_login, "invited": invited})).await;
    }
    Ok(Ok(uid))
}

/// Whether this person was taken off the account and hasn't been invited back.
async fn removed(tx: &mut sqlx::PgConnection, cfg: &Config, subject: &str, email: &str) -> AppResult<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM team_removals WHERE account_id = $1 AND (email = $2 OR (issuer = $3 AND subject = $4)))")
        .bind(cfg.account_id)
        .bind(email)
        .bind(&cfg.issuer)
        .bind(subject)
        .fetch_one(&mut *tx)
        .await?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn second_factor_from_amr() {
        assert!(super::amr_shows_second_factor(["pwd", "mfa"]));
        assert!(super::amr_shows_second_factor(["mfa"]));
        assert!(super::amr_shows_second_factor(["pwd", "OTP"]));
        assert!(super::amr_shows_second_factor(["hwk", "fpt"]));
        assert!(super::amr_shows_second_factor(["pin", "face"]));
        // One method alone isn't two factors, nor are two of the same kind.
        assert!(!super::amr_shows_second_factor(["otp"]));
        assert!(!super::amr_shows_second_factor(["hwk"]));
        assert!(!super::amr_shows_second_factor(["otp", "sms"]));
        assert!(!super::amr_shows_second_factor(["mca"]));
        assert!(!super::amr_shows_second_factor(["pwd"]));
        assert!(!super::amr_shows_second_factor(["pwd", "kba", "pin"]));
        assert!(!super::amr_shows_second_factor(Vec::<&str>::new()));
    }

    #[test]
    fn domains() {
        assert_eq!(super::clean_domain(" @Acme.com. ").as_deref(), Some("acme.com"));
        assert!(super::clean_domain("localhost").is_none());
        assert!(super::clean_domain("a..b").is_none());
        assert!(super::clean_domain("evil.com/x").is_none());
    }
}
