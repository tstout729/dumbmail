//! "Continue with Google": the OAuth 2.0 authorization-code flow with PKCE.
//!
//! The browser goes to `/api/auth/google`, which remembers a random `state` and PKCE verifier in
//! a short-lived signed cookie and sends it to Google. Google sends it back to the callback with a
//! code, which we trade (with our client secret) for an ID token naming the person.

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

use super::account::{insert_owner, new_session, session_cookie};
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
    Json(json!({"google": client(&state).is_some(), "signup": state.cfg.allow_signup}))
}

fn redirect_uri(state: &AppState) -> String {
    format!("{}/api/auth/google/callback", state.cfg.public_url())
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
}

async fn start(State(state): State<AppState>, jar: CookieJar, Query(q): Query<StartQuery>) -> AppResult<(CookieJar, Redirect)> {
    let (client_id, _) = client(&state).ok_or_else(|| AppError::NotFound("Google sign-in isn't set up on this server".into()))?;
    let st = auth::random_token(24);
    let verifier = auth::random_token(48);
    let payload = format!("{st}|{verifier}|{}", B64.encode(safe_next(q.next.as_deref())));
    let cookie = Cookie::build((OAUTH_COOKIE, auth::sign(&state.cfg.secret_key, &payload)))
        .path(OAUTH_COOKIE_PATH)
        .http_only(true)
        .secure(state.cfg.secure_cookies)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::minutes(10))
        .build();
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
    Ok((jar.add(cookie), Redirect::to(url.as_str())))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(State(state): State<AppState>, jar: CookieJar, Query(q): Query<CallbackQuery>) -> (CookieJar, Redirect) {
    let cookie = jar.get(OAUTH_COOKIE).map(|c| c.value().to_string());
    let jar = jar.remove(Cookie::build(OAUTH_COOKIE).path(OAUTH_COOKIE_PATH).build());
    // Errors go back to the login page as a short code it explains, never as free text, so a
    // crafted link can't put arbitrary words on our login page.
    let fail = |jar: CookieJar, code: &str| (jar, Redirect::to(&format!("/login?error={code}")));
    match finish(&state, cookie, q).await {
        Ok((user_id, account_id, dest)) => match new_session(&state, user_id, account_id).await {
            Ok(token) => (jar.add(session_cookie(&state, token)), Redirect::to(&dest)),
            Err(_) => fail(jar, "failed"),
        },
        Err(code) => fail(jar, code),
    }
}

/// Finish the sign-in. Errors are codes the login page explains:
/// cancelled, expired, unavailable, google, unverified, conflict, closed, failed.
async fn finish(state: &AppState, cookie: Option<String>, q: CallbackQuery) -> Result<(Uuid, Uuid, String), &'static str> {
    if let Some(e) = q.error {
        return Err(if e == "access_denied" { "cancelled" } else { "google" });
    }
    let (client_id, secret) = client(state).ok_or("unavailable")?;
    let payload = cookie.and_then(|c| auth::verify(&state.cfg.secret_key, &c)).ok_or("expired")?;
    let mut parts = payload.splitn(3, '|');
    let (Some(st), Some(verifier), Some(next)) = (parts.next(), parts.next(), parts.next()) else { return Err("expired") };
    if q.state.as_deref() != Some(st) {
        return Err("expired");
    }
    let next = B64.decode(next).ok().and_then(|b| String::from_utf8(b).ok()).map(|n| safe_next(Some(&n))).unwrap_or_else(|| "/".into());
    let code = q.code.ok_or("expired")?;
    let res = state
        .http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code.as_str()),
            ("client_id", client_id),
            ("client_secret", secret),
            ("redirect_uri", &redirect_uri(state)),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
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
    let (user_id, account_id, created) = google_user(state, &claims).await.map_err(|e| match e {
        AppError::Conflict(_) => "conflict",
        AppError::Forbidden(_) => "closed",
        _ => "failed",
    })?;
    Ok((user_id, account_id, if created { "/onboarding".into() } else { next }))
}

/// Who Google says signed in.
#[derive(Debug, Clone)]
pub struct GoogleClaims {
    pub sub: String,
    pub email: String,
    pub name: Option<String>,
    pub given_name: Option<String>,
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
    Ok(GoogleClaims { sub, email, name: text("name"), given_name: text("given_name") })
}

const FREE_MAIL: [&str; 16] = [
    "gmail.com", "googlemail.com", "yahoo.com", "ymail.com", "outlook.com", "hotmail.com", "live.com", "msn.com", "icloud.com", "me.com", "mac.com", "aol.com",
    "proton.me", "protonmail.com", "gmx.com", "mail.com",
];

/// Find or create the DumbMail user for a Google sign-in. Returns (user, account, created).
pub async fn google_user(state: &AppState, c: &GoogleClaims) -> AppResult<(Uuid, Uuid, bool)> {
    let first_account = |uid: Uuid| async move {
        sqlx::query_scalar::<_, Uuid>("SELECT account_id FROM memberships WHERE user_id = $1 ORDER BY created_at LIMIT 1").bind(uid).fetch_one(&state.db).await
    };
    if let Some(uid) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE google_sub = $1").bind(&c.sub).fetch_optional(&state.db).await? {
        return Ok((uid, first_account(uid).await?, false));
    }
    let existing: Option<(Uuid, Option<String>)> = sqlx::query_as("SELECT id, google_sub FROM users WHERE email = $1").bind(&c.email).fetch_optional(&state.db).await?;
    if let Some((uid, linked)) = existing {
        if linked.is_some() {
            return Err(AppError::Conflict("this email is already linked to a different Google account".into()));
        }
        // Sign-up doesn't confirm email addresses, so whoever set this password may not own the
        // address. Linking cuts off every way they could still get in: the password, every older
        // session and every API key on the user's accounts (the owner makes new keys if needed).
        // Accounts have no team invites, so the user's memberships are only accounts they created.
        let mut tx = state.db.begin().await?;
        sqlx::query("UPDATE users SET google_sub = $2, password_hash = NULL WHERE id = $1").bind(uid).bind(&c.sub).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM sessions WHERE user_id = $1").bind(uid).execute(&mut *tx).await?;
        sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE revoked_at IS NULL AND account_id IN (SELECT account_id FROM memberships WHERE user_id = $1)")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok((uid, first_account(uid).await?, false));
    }
    if !state.cfg.allow_signup {
        return Err(AppError::Forbidden("no DumbMail account uses this Google address, and sign-ups are off on this server".into()));
    }
    let domain = c.email.rsplit('@').next().unwrap_or_default().to_string();
    let (company, website) = if FREE_MAIL.contains(&domain.as_str()) {
        let who = c.given_name.clone().or_else(|| c.name.clone());
        (who.map(|n| format!("{n}'s brand")).unwrap_or_else(|| "My brand".into()), None)
    } else {
        // A work address names the brand: start them on their own domain.
        (domain.clone(), Some(format!("https://{domain}")))
    };
    let (user, account) = insert_owner(state, &c.email, None, Some(&c.sub), c.name.clone(), &company, website).await?;
    Ok((user.id, account.id, true))
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
