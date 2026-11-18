//! "Continue with Google": the OAuth 2.0 authorization-code flow with PKCE.
//!
//! The browser goes to `/api/auth/google`, which remembers a random `state` and PKCE verifier in
//! a short-lived signed cookie and sends it to Google. Google sends it back to the callback with a
//! code, which we trade (with our client secret) for an ID token naming the person.
//!
//! With `?reauth=1` a signed-in person who uses Google confirms it's them instead of signing in:
//! Google is asked to make them sign in again (`max_age=0`) and to say when they did
//! (`auth_time`). The callback checks it's the same Google account as the login's, that the
//! sign-in happened in the last few minutes, and that the browser still holds the session that
//! asked; then it marks that session as just confirmed (as `POST /api/me/reauth` does), without
//! starting a new one.
//!
//! A Google account is matched to an existing login by email only when Google is the authority
//! for that address: a Gmail address, or a Workspace account whose `hd` claim is the address's own
//! domain (Google's "Verify the Google ID token" guide: otherwise `email_verified` can stay true
//! after the mailbox changed hands). Then it links to that login only when the login's email is
//! confirmed (see `features::team::confirm`); otherwise the person chooses on the `/link-google`
//! page (see `features::team::google_link`). A link is saved only once the sign-in that makes it
//! is complete, two-factor included.

use axum::extract::{Query, State};
use axum::response::Redirect;
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::account::{insert_owner, new_session, session_cookie, with_cookie_domain};
use crate::features::team::google_link::{self, LinkPlan};
use crate::auth;
use crate::error::{AppError, AppResult};
use crate::models::normalize_email;
use crate::state::AppState;

const OAUTH_COOKIE: &str = "pl_oauth";
const OAUTH_COOKIE_PATH: &str = "/api/auth/google";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/options", get(options))
        .route("/api/auth/google", get(start))
        .route("/api/auth/google/callback", get(callback))
}

fn client(state: &AppState) -> Option<(&str, &str)> {
    let id = state.cfg.google_client_id.as_deref().filter(|s| !s.trim().is_empty())?;
    let secret = state.cfg.google_client_secret.as_deref().filter(|s| !s.trim().is_empty())?;
    Some((id, secret))
}

/// What the sign-in pages should offer.
async fn options(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"google": client(&state).is_some(), "signup": state.cfg.allow_signup, "site_url": state.cfg.site_url()}))
}

fn redirect_uri(state: &AppState) -> String {
    format!("{}/api/auth/google/callback", state.cfg.google_redirect_base())
}

/// Where the callback sends people: a same-site path, made absolute on PUBLIC_URL when Google
/// returns them to another host (GOOGLE_REDIRECT_BASE), so they land in the app.
fn app_location(state: &AppState, path: &str) -> String {
    if state.cfg.google_redirect_base() == state.cfg.public_url() {
        path.to_string()
    } else {
        format!("{}{path}", state.cfg.public_url())
    }
}

/// Only same-site paths, so `?next=//evil.com` can't send people elsewhere after signing in.
/// Browsers drop tabs and newlines and read `\\` as `/`, so any of those (or other control
/// characters) is refused, and the result is re-resolved against a fixed origin to be sure.
fn safe_next(next: Option<&str>) -> String {
    let Some(n) = next else { return "/".into() };
    if !n.starts_with('/') || n.starts_with("//") || n.len() >= 500 || n.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
        return "/".into();
    }
    let base = url::Url::parse("https://dumbmail.invalid/").expect("static url");
    match base.join(n) {
        Ok(u) if u.origin() == base.origin() => match u.query() {
            Some(q) => format!("{}?{q}", u.path()),
            None => u.path().to_string(),
        },
        _ => "/".into(),
    }
}

#[derive(Deserialize)]
struct StartQuery {
    next: Option<String>,
    /// Confirm it's you with Google instead of signing in.
    reauth: Option<String>,
}

/// How long ago Google's sign-in may have been for it to confirm it's you.
const REAUTH_MAX_AGE_SECS: i64 = 300;

/// The signed-in session asking to confirm with Google: its id, and the login's Google account.
async fn reauth_session(state: &AppState, jar: &CookieJar, headers: &axum::http::HeaderMap) -> Option<(Uuid, String, String)> {
    let token = jar.get(auth::SESSION_COOKIE)?.value().to_string();
    let meta = crate::features::team::login::ReqMeta::from_headers(headers);
    let ctx = crate::features::team::login::session_ctx(state, &token, &meta).await.ok()?;
    if ctx.restriction.is_some() || ctx.sso_session {
        return None;
    }
    let (sid, uid) = (ctx.session_id?, ctx.user_id?);
    let (sub, email): (Option<String>, String) = sqlx::query_as("SELECT google_sub, email FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await.ok()?;
    Some((sid, sub?, email))
}

async fn start(State(state): State<AppState>, headers: axum::http::HeaderMap, jar: CookieJar, Query(q): Query<StartQuery>) -> AppResult<(CookieJar, Redirect)> {
    let (client_id, _) = client(&state).ok_or_else(|| AppError::NotFound("Google sign-in isn't set up on this server".into()))?;
    let next = safe_next(q.next.as_deref());
    let reauth = match q.reauth.as_deref() {
        Some("1" | "true") => match reauth_session(&state, &jar, &headers).await {
            Some(r) => Some(r),
            None => return Ok((jar, Redirect::to(&with_param(&next, "confirm_error", "google_unavailable")))),
        },
        _ => None,
    };
    let st = auth::random_token(24);
    let verifier = auth::random_token(48);
    let mut payload = format!("{st}|{verifier}|{}", B64.encode(&next));
    if let Some((sid, sub, _)) = &reauth {
        payload.push_str(&format!("|{}", B64.encode(format!("{sid}:{sub}"))));
    }
    let cookie = Cookie::build((OAUTH_COOKIE, auth::sign(&state.cfg.secret_key, &payload)))
        .path(OAUTH_COOKIE_PATH)
        .http_only(true)
        .secure(state.cfg.secure_cookies)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::minutes(10))
        .build();
    let cookie = with_cookie_domain(&state, cookie);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").expect("static url");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect_uri(&state))
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile")
        .append_pair("state", &st)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "select_account");
    if let Some((_, _, email)) = &reauth {
        // Make them sign in to Google again, and tell us when they did.
        url.query_pairs_mut()
            .append_pair("max_age", "0")
            .append_pair("claims", r#"{"id_token":{"auth_time":{"essential":true}}}"#)
            .append_pair("login_hint", email);
    }
    Ok((jar.add(cookie), Redirect::to(url.as_str())))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `path` with one more query parameter.
fn with_param(path: &str, k: &str, v: &str) -> String {
    format!("{path}{}{k}={v}", if path.contains('?') { '&' } else { '?' })
}

async fn callback(State(state): State<AppState>, headers: axum::http::HeaderMap, jar: CookieJar, Query(q): Query<CallbackQuery>) -> (CookieJar, Redirect) {
    let meta = crate::features::team::login::ReqMeta::from_headers(&headers);
    let cookie = jar.get(OAUTH_COOKIE).map(|c| c.value().to_string());
    let jar = jar.remove(with_cookie_domain(&state, Cookie::build(OAUTH_COOKIE).path(OAUTH_COOKIE_PATH).build()));
    // Confirming it's you: no new session, back to the page that asked.
    if let Some((flow, sid, sub)) = cookie.as_deref().and_then(|c| reauth_payload(&state, c)) {
        let next = flow.next.clone();
        let (dest, ok) = match confirm(&state, &jar, &meta, &flow, q, sid, &sub).await {
            Ok(()) => (with_param(&next, "confirmed", "google"), true),
            Err(code) => (with_param(&next, "confirm_error", code), false),
        };
        if !ok {
            tracing::info!("Google confirmation refused");
        }
        return (jar, Redirect::to(&app_location(&state, &dest)));
    }
    // Errors go back to the login page as a short code it explains, never as free text, so a
    // crafted link can't put arbitrary words on our login page.
    let fail = |jar: CookieJar, code: &str| (jar, Redirect::to(&app_location(&state, &format!("/login?error={code}"))));
    match finish(&state, cookie, q).await {
        // An unconfirmed password login has this address: the person chooses on the next page.
        Ok(Finished::Choose(token)) => (jar.add(google_link::cookie(&state, token)), Redirect::to(&app_location(&state, "/link-google"))),
        Ok(Finished::SignedIn(user_id, account_id, dest)) => match new_session(&state, user_id, account_id, "google", &meta).await {
            Ok(token) => {
                crate::features::team::audit::record(&state, account_id, &meta.actor(Some(user_id)), "login", None, json!({"method": "google"})).await;
                (jar.add(session_cookie(&state, token)), Redirect::to(&app_location(&state, &dest)))
            }
            Err(_) => fail(jar, "failed"),
        },
        // A confirmed login gets Google linked once this sign-in is complete (two-factor too).
        Ok(Finished::Link(plan, account_id)) => match google_link::linking_session(&state, &plan, None, account_id, "google", &meta).await {
            Ok(token) => {
                crate::features::team::audit::record(&state, account_id, &meta.actor(Some(plan.user_id)), "login", None, json!({"method": "google"})).await;
                (jar.add(session_cookie(&state, token)), Redirect::to(&app_location(&state, &plan.next)))
            }
            Err(_) => fail(jar, "failed"),
        },
        Err(code) => fail(jar, code),
    }
}

/// What the signed sign-in cookie remembers.
struct Flow {
    state: String,
    verifier: String,
    next: String,
    /// For confirming it's you: "<session id>:<Google subject>".
    reauth: Option<String>,
}

fn read_flow(state: &AppState, cookie: &str) -> Option<Flow> {
    let payload = auth::verify(&state.cfg.secret_key, cookie)?;
    let parts: Vec<&str> = payload.split('|').collect();
    let (st, verifier, next, reauth) = match parts.as_slice() {
        [st, verifier, next] => (st, verifier, next, None),
        [st, verifier, next, who] => (st, verifier, next, Some(String::from_utf8(B64.decode(who).ok()?).ok()?)),
        _ => return None,
    };
    let next = B64.decode(next).ok().and_then(|b| String::from_utf8(b).ok()).map(|n| safe_next(Some(&n))).unwrap_or_else(|| "/".into());
    Some(Flow { state: st.to_string(), verifier: verifier.to_string(), next, reauth })
}

/// The flow, the session and the Google account, when the sign-in cookie is for confirming it's you.
fn reauth_payload(state: &AppState, cookie: &str) -> Option<(Flow, Uuid, String)> {
    let flow = read_flow(state, cookie)?;
    let (sid, sub) = flow.reauth.as_deref()?.split_once(':')?;
    let (sid, sub) = (sid.parse().ok()?, sub.to_string());
    Some((flow, sid, sub))
}

static TOKEN_URL_FOR_TESTS: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Tests stand in for Google's token endpoint. Only test code calls this; there's no setting.
#[doc(hidden)]
pub fn use_token_url_for_tests(url: &str) {
    *TOKEN_URL_FOR_TESTS.write().expect("token url lock") = Some(url.to_string());
}

fn token_url() -> String {
    TOKEN_URL_FOR_TESTS.read().ok().and_then(|u| u.clone()).unwrap_or_else(|| "https://oauth2.googleapis.com/token".into())
}

/// Check the returned state, trade the code for Google's ID token and read it. Also returns the
/// token's claims as sent, for the ones `GoogleClaims` leaves out (`auth_time`).
async fn exchange(state: &AppState, flow: &Flow, q: &CallbackQuery) -> Result<(GoogleClaims, Value), &'static str> {
    let (client_id, secret) = client(state).ok_or("unavailable")?;
    if q.state.as_deref() != Some(flow.state.as_str()) {
        return Err("expired");
    }
    let code = q.code.as_deref().ok_or("expired")?;
    let res = state
        .http
        .post(token_url())
        .form(&[
            ("code", code),
            ("client_id", client_id),
            ("client_secret", secret),
            ("redirect_uri", &redirect_uri(state)),
            ("grant_type", "authorization_code"),
            ("code_verifier", flow.verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|_| "google")?;
    if !res.status().is_success() {
        tracing::warn!(status = %res.status(), "google token exchange failed");
        return Err("google");
    }
    let body: Value = res.json().await.map_err(|_| "google")?;
    let id_token = body["id_token"].as_str().ok_or("google")?;
    let claims = parse_id_token(id_token, client_id, chrono::Utc::now().timestamp()).map_err(|e| if e.contains("verified") { "unverified" } else { "google" })?;
    let raw = id_token.split('.').nth(1).and_then(|p| B64.decode(p.trim_end_matches('=')).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    Ok((claims, raw))
}

/// Trade the code, check it's the login's Google account and a sign-in from the last few minutes,
/// and mark the session that asked (still held by this browser) as just confirmed.
async fn confirm(state: &AppState, jar: &CookieJar, meta: &crate::features::team::login::ReqMeta, flow: &Flow, q: CallbackQuery, sid: Uuid, sub: &str) -> Result<(), &'static str> {
    if q.error.is_some() {
        return Err("google_cancelled");
    }
    let held = match jar.get(auth::SESSION_COOKIE) {
        Some(c) => crate::features::team::login::session_ctx(state, c.value(), meta).await.ok(),
        None => None,
    };
    let Some(ctx) = held.filter(|c| c.session_id == Some(sid) && c.restriction.is_none()) else { return Err("google_session") };
    let (claims, raw) = exchange(state, flow, &q).await.map_err(|_| "google_failed")?;
    if claims.sub != sub {
        return Err("google_other_account");
    }
    let now = chrono::Utc::now().timestamp();
    if !raw["auth_time"].as_i64().is_some_and(|t| t >= now - REAUTH_MAX_AGE_SECS && t <= now + 60) {
        return Err("google_not_fresh");
    }
    let n = sqlx::query("UPDATE sessions s SET reauth_at = now() FROM users u WHERE s.id = $1 AND u.id = s.user_id AND u.google_sub = $2 AND s.expires_at > now()")
        .bind(sid)
        .bind(sub)
        .execute(&state.db)
        .await
        .map_err(|_| "google_failed")?
        .rows_affected();
    if n != 1 {
        return Err("google_session");
    }
    crate::features::team::audit(state, &ctx, "login.reauth", None, json!({"method": "google"})).await;
    Ok(())
}

/// How a Google sign-in ended.
enum Finished {
    /// Sign this login in to this account, then go here.
    SignedIn(Uuid, Uuid, String),
    /// Sign the confirmed login in to this account; link Google once the sign-in is complete.
    Link(LinkPlan, Uuid),
    /// The address has a password login whose email was never confirmed: the person chooses what
    /// happens on the `/link-google` page. The token is for its cookie.
    Choose(String),
}

/// Finish the sign-in. Errors are codes the login page explains: cancelled, expired,
/// unavailable, google, unverified, conflict, use_password, closed, failed.
async fn finish(state: &AppState, cookie: Option<String>, q: CallbackQuery) -> Result<Finished, &'static str> {
    if let Some(e) = &q.error {
        return Err(if e == "access_denied" { "cancelled" } else { "google" });
    }
    client(state).ok_or("unavailable")?;
    let flow = cookie.and_then(|c| read_flow(state, &c)).ok_or("expired")?;
    let (claims, _) = exchange(state, &flow, &q).await?;
    let next = flow.next;
    // Coming back from an invite link with no login yet: join that account instead of starting one.
    let known: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE google_sub = $1 OR email = $2)").bind(&claims.sub).bind(&claims.email).fetch_one(&state.db).await.map_err(|_| "failed")?;
    if !known {
        let invited = crate::features::team::members::google_invite_signup(state, &next, &claims.email, &claims.sub, claims.name.clone(), claims.authoritative()).await.map_err(|_| "failed")?;
        if let Some((uid, account)) = invited {
            return Ok(Finished::SignedIn(uid, account, "/".into()));
        }
    }
    let found = google_user(state, &claims).await.map_err(|e| match e {
        AppError::Conflict(_) => "conflict",
        AppError::Forbidden(_) => "closed",
        _ => "failed",
    })?;
    Ok(match found {
        GoogleUser::SignedIn(user_id, account_id, created) => Finished::SignedIn(user_id, account_id, if created { "/onboarding".into() } else { next }),
        GoogleUser::Link { user_id, account_id, credential_version } => {
            Finished::Link(LinkPlan::new(user_id, &claims, credential_version, true, &next), account_id)
        }
        GoogleUser::Unconfirmed(user_id) => Finished::Choose(google_link::create(state, user_id, &claims, &next).await.map_err(|_| "failed")?),
        GoogleUser::UsePassword => return Err("use_password"),
    })
}

/// Who Google says signed in.
#[derive(Debug, Clone)]
pub struct GoogleClaims {
    pub sub: String,
    pub email: String,
    pub name: Option<String>,
    pub given_name: Option<String>,
    /// The Google Workspace domain the account belongs to (`hd`), if any.
    pub hd: Option<String>,
}

impl GoogleClaims {
    /// Whether Google is the authority for this address, so it proves who owns the mailbox now:
    /// a Gmail address, or a Workspace account (`hd`) on the address's own domain. Any other
    /// address was only verified once, when the Google account was made, and may have changed
    /// hands since. (`email_verified` is already required by [`parse_id_token`].)
    pub fn authoritative(&self) -> bool {
        is_authoritative(&self.email, self.hd.as_deref())
    }
}

/// See [`GoogleClaims::authoritative`].
pub fn is_authoritative(email: &str, hd: Option<&str>) -> bool {
    let domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
    matches!(domain, "gmail.com" | "googlemail.com") || hd.is_some_and(|h| !h.is_empty() && h.eq_ignore_ascii_case(domain))
}

/// Read the ID token from Google's token endpoint. It came straight from Google over TLS in answer
/// to our authenticated request, so OpenID Connect (Core §3.1.3.7) lets us rely on TLS instead of
/// checking its signature; we still check who it is for, who issued it and that it's current.
pub fn parse_id_token(token: &str, client_id: &str, now: i64) -> Result<GoogleClaims, String> {
    let bad = || "Google sent an unexpected answer. Try again.".to_string();
    let payload = token.split('.').nth(1).ok_or_else(bad)?;
    let bytes = B64.decode(payload.trim_end_matches('=')).map_err(|_| bad())?;
    let c: Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    let iss = c["iss"].as_str().unwrap_or_default();
    let aud_ok = match &c["aud"] {
        Value::String(a) => a == client_id,
        Value::Array(a) => a.iter().any(|x| x.as_str() == Some(client_id)),
        _ => false,
    };
    let exp = c["exp"].as_i64().unwrap_or(0);
    if !matches!(iss, "https://accounts.google.com" | "accounts.google.com") || !aud_ok || exp < now - 60 {
        return Err(bad());
    }
    let verified = c["email_verified"].as_bool().unwrap_or(false) || c["email_verified"].as_str() == Some("true");
    let email = c["email"].as_str().and_then(normalize_email).filter(|_| verified).ok_or("Your Google account needs a verified email address.")?;
    let sub = c["sub"].as_str().filter(|s| !s.is_empty()).ok_or_else(bad)?.to_string();
    let text = |k: &str| c[k].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    Ok(GoogleClaims { sub, email, name: text("name"), given_name: text("given_name"), hd: text("hd").map(|h| h.to_ascii_lowercase()) })
}

const FREE_MAIL: [&str; 16] = [
    "gmail.com", "googlemail.com", "yahoo.com", "ymail.com", "outlook.com", "hotmail.com", "live.com", "msn.com", "icloud.com", "me.com", "mac.com", "aol.com",
    "proton.me", "protonmail.com", "gmx.com", "mail.com",
];

/// Who a Google sign-in is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoogleUser {
    /// Sign in as (user, account, created): a login already linked to this Google account, or a
    /// new one.
    SignedIn(Uuid, Uuid, bool),
    /// A login whose email is confirmed has this address: sign it in, and link Google once the
    /// sign-in is complete, if the login still has this credential version, no Google account and
    /// a confirmed email.
    Link { user_id: Uuid, account_id: Uuid, credential_version: i32 },
    /// A login whose email was never confirmed has this address, so it may not be the address
    /// owner's: nothing is linked and nobody is signed in (see `features::team::google_link`).
    Unconfirmed(Uuid),
    /// A login has this address, but Google isn't the authority for it (see
    /// [`GoogleClaims::authoritative`]): use that login's password.
    UsePassword,
}

/// Find or create the DumbMail user for a Google sign-in. Nothing here changes an existing login:
/// a link is saved later, once the sign-in that makes it is complete (see `google_link`).
pub async fn google_user(state: &AppState, c: &GoogleClaims) -> AppResult<GoogleUser> {
    let first_account = |uid: Uuid| async move {
        crate::features::team::login::pick_account(state, uid).await?.ok_or_else(|| AppError::Forbidden("this login isn't part of any account anymore".into()))
    };
    if let Some(uid) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE google_sub = $1 AND disabled_at IS NULL").bind(&c.sub).fetch_optional(&state.db).await? {
        return Ok(GoogleUser::SignedIn(uid, first_account(uid).await?, false));
    }
    let existing: Option<(Uuid, Option<String>, bool, i32)> =
        sqlx::query_as("SELECT id, google_sub, email_confirmed_at IS NOT NULL, credential_version FROM users WHERE email = $1 AND disabled_at IS NULL")
            .bind(&c.email)
            .fetch_optional(&state.db)
            .await?;
    if let Some((uid, linked, confirmed, credential_version)) = existing {
        if !c.authoritative() {
            return Ok(GoogleUser::UsePassword);
        }
        if linked.is_some() {
            return Err(AppError::Conflict("this email is already linked to a different Google account".into()));
        }
        if confirmed {
            return Ok(GoogleUser::Link { user_id: uid, account_id: first_account(uid).await?, credential_version });
        }
        return Ok(GoogleUser::Unconfirmed(uid));
    }
    if !state.cfg.allow_signup {
        return Err(AppError::Forbidden("no DumbMail account uses this Google address, and sign-ups are off on this server".into()));
    }
    let (company, website) = brand_for(c);
    let (user, account) = insert_owner(state, &c.email, None, Some(&c.sub), c.authoritative(), c.name.clone(), &company, website).await?;
    Ok(GoogleUser::SignedIn(user.id, account.id, true))
}

/// A first-time Google user's account name and website.
pub fn brand_for(c: &GoogleClaims) -> (String, Option<String>) {
    let domain = c.email.rsplit('@').next().unwrap_or_default().to_string();
    if FREE_MAIL.contains(&domain.as_str()) {
        let who = c.given_name.clone().or_else(|| c.name.clone());
        (who.map(|n| format!("{n}'s brand")).unwrap_or_else(|| "My brand".into()), None)
    } else {
        // A work address names the brand: start them on their own domain.
        (domain.clone(), Some(format!("https://{domain}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(claims: Value) -> String {
        format!("e30.{}.sig", B64.encode(claims.to_string()))
    }

    #[test]
    fn id_token_checks() {
        let good = json!({"iss": "https://accounts.google.com", "aud": "cid", "sub": "123", "email": "Ann@Acme.com", "email_verified": true, "exp": 2000, "given_name": "Ann"});
        let c = parse_id_token(&token(good.clone()), "cid", 1000).unwrap();
        assert_eq!((c.sub.as_str(), c.email.as_str(), c.given_name.as_deref()), ("123", "ann@acme.com", Some("Ann")));
        for (k, v) in [("aud", json!("other")), ("iss", json!("https://evil.com")), ("exp", json!(10)), ("email_verified", json!(false))] {
            let mut bad = good.clone();
            bad[k] = v;
            assert!(parse_id_token(&token(bad), "cid", 1000).is_err(), "{k}");
        }
        assert!(parse_id_token("garbage", "cid", 1000).is_err());
    }

    #[test]
    fn next_paths() {
        assert_eq!(safe_next(Some("/campaigns?x=1")), "/campaigns?x=1");
        assert_eq!(safe_next(Some("//evil.com")), "/");
        assert_eq!(safe_next(Some("/\\evil.com")), "/");
        assert_eq!(safe_next(Some("https://evil.com")), "/");
        assert_eq!(safe_next(Some("/\t/evil.com")), "/");
        assert_eq!(safe_next(Some("/\n/evil.com")), "/");
        assert_eq!(safe_next(Some("/a\\b")), "/");
        assert_eq!(safe_next(Some("/flows/../settings")), "/settings");
        assert_eq!(safe_next(None), "/");
    }
}
