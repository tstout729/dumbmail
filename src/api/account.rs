use axum::extract::{Path, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::ApiJson;
use crate::auth::{self, Ctx, API_KEY_PREFIX, SESSION_COOKIE};
use crate::campaigns::load_account;
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, normalize_phone, Account, User};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/signup", post(signup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/account", axum::routing::patch(update_account))
        .route("/api/account/sending", get(sending))
        .route("/api/account/domain", post(add_domain))
        .route("/api/account/domain/verify", post(verify_domain))
        .route("/api/api-keys", get(list_keys).post(create_key))
        .route("/api/api-keys/{id}", delete(revoke_key))
        .route("/api/billing", get(billing))
}

#[derive(Deserialize)]
struct SignupBody {
    email: String,
    password: String,
    name: Option<String>,
    company: String,
}

fn session_cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        .secure(state.cfg.secure_cookies)
        .same_site(SameSite::Lax)
        .max_age(time_days(30))
        .build()
}

fn time_days(d: i64) -> time::Duration {
    time::Duration::days(d)
}

async fn new_session(state: &AppState, user_id: Uuid, account_id: Uuid) -> AppResult<String> {
    let token = auth::random_token(32);
    sqlx::query("INSERT INTO sessions (token_hash, user_id, account_id, expires_at) VALUES ($1, $2, $3, now() + interval '30 days')")
        .bind(auth::sha256_hex(&token))
        .bind(user_id)
        .bind(account_id)
        .execute(&state.db)
        .await?;
    Ok(token)
}

/// Create an account and its owner. Shared by the signup route and the `create-account` command.
pub async fn create_account(state: &AppState, email: &str, password: &str, name: Option<String>, company: &str) -> AppResult<(User, Account)> {
    let email = normalize_email(email).ok_or_else(|| AppError::bad("enter a valid email"))?;
    if password.len() < 8 {
        return Err(AppError::bad("use a password of at least 8 characters"));
    }
    let company = company.trim();
    if company.is_empty() {
        return Err(AppError::bad("enter your company or brand name"));
    }
    let hash = auth::hash_password(password)?;
    let mut tx = state.db.begin().await?;
    let account: Account = sqlx::query_as("INSERT INTO accounts (id, public_key, name, from_name) VALUES ($1, $2, $3, $3) RETURNING *")
        .bind(Uuid::now_v7())
        .bind(format!("pk_{}", auth::random_token(12)))
        .bind(company)
        .fetch_one(&mut *tx)
        .await?;
    let user: User = sqlx::query_as("INSERT INTO users (id, email, name, password_hash) VALUES ($1, $2, $3, $4) RETURNING id, email, name")
        .bind(Uuid::now_v7())
        .bind(&email)
        .bind(name.filter(|n| !n.trim().is_empty()))
        .bind(hash)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match AppError::from(e) {
            AppError::Conflict(_) => AppError::Conflict("an account with that email already exists; sign in instead".into()),
            other => other,
        })?;
    sqlx::query("INSERT INTO memberships (user_id, account_id) VALUES ($1, $2)").bind(user.id).bind(account.id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok((user, account))
}

async fn signup(State(state): State<AppState>, jar: CookieJar, ApiJson(b): ApiJson<SignupBody>) -> AppResult<(CookieJar, Json<Value>)> {
    if !state.cfg.allow_signup {
        return Err(AppError::Forbidden("sign-ups are turned off on this server".into()));
    }
    let (user, account) = create_account(&state, &b.email, &b.password, b.name, &b.company).await?;
    let token = new_session(&state, user.id, account.id).await?;
    Ok((jar.add(session_cookie(&state, token)), Json(json!({"user": user, "account": account}))))
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
}

async fn login(State(state): State<AppState>, jar: CookieJar, ApiJson(b): ApiJson<LoginBody>) -> AppResult<(CookieJar, Json<Value>)> {
    let email = b.email.trim().to_lowercase();
    let row: Option<(Uuid, String, Option<String>, String)> = sqlx::query_as("SELECT id, email, name, password_hash FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&state.db)
        .await?;
    let bad = || AppError::BadRequest("that email and password don't match".into());
    let (id, email, name, hash) = row.ok_or_else(bad)?;
    if !auth::verify_password(&b.password, &hash) {
        return Err(bad());
    }
    let account_id: Uuid = sqlx::query_scalar("SELECT account_id FROM memberships WHERE user_id = $1 ORDER BY created_at LIMIT 1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    let account = load_account(&state.db, account_id).await?;
    let token = new_session(&state, id, account_id).await?;
    Ok((jar.add(session_cookie(&state, token)), Json(json!({"user": User { id, email, name }, "account": account}))))
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> AppResult<(CookieJar, Json<Value>)> {
    if let Some(c) = jar.get(SESSION_COOKIE) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1").bind(auth::sha256_hex(c.value())).execute(&state.db).await?;
    }
    Ok((jar.remove(Cookie::build(SESSION_COOKIE).path("/").build()), Json(json!({"ok": true}))))
}

async fn me(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let account = load_account(&state.db, ctx.account_id).await?;
    let user: Option<User> = match ctx.user_id {
        Some(uid) => sqlx::query_as("SELECT id, email, name FROM users WHERE id = $1").bind(uid).fetch_optional(&state.db).await?,
        None => None,
    };
    Ok(Json(json!({"user": user, "account": account})))
}

#[derive(Deserialize)]
pub struct AccountPatch {
    pub name: Option<String>,
    pub website: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub reply_to: Option<String>,
    pub sms_from: Option<String>,
    pub timezone: Option<String>,
    pub address: Option<String>,
    pub brand_color: Option<String>,
    pub logo_url: Option<String>,
    pub conversion_metric: Option<String>,
    pub onboarding: Option<serde_json::Map<String, Value>>,
}

fn blank_to_none(v: Option<String>) -> Option<Option<String>> {
    v.map(|s| if s.trim().is_empty() { None } else { Some(s.trim().to_string()) })
}

pub async fn patch_account(state: &AppState, account_id: Uuid, b: AccountPatch) -> AppResult<Account> {
    let mut account = load_account(&state.db, account_id).await?;
    if let Some(n) = b.name.filter(|n| !n.trim().is_empty()) {
        account.name = n.trim().to_string();
    }
    if let Some(w) = blank_to_none(b.website) {
        account.website = w.map(|w| if w.starts_with("http") { w } else { format!("https://{w}") });
    }
    if let Some(v) = blank_to_none(b.from_name) {
        account.from_name = v;
    }
    if let Some(v) = blank_to_none(b.from_email) {
        account.from_email = match v {
            Some(e) => Some(normalize_email(&e).ok_or_else(|| AppError::bad("sender email is not valid"))?),
            None => None,
        };
    }
    if let Some(v) = blank_to_none(b.reply_to) {
        account.reply_to = match v {
            Some(e) => Some(normalize_email(&e).ok_or_else(|| AppError::bad("reply-to email is not valid"))?),
            None => None,
        };
    }
    if let Some(v) = blank_to_none(b.sms_from) {
        account.sms_from = match v {
            Some(p) => Some(normalize_phone(&p).ok_or_else(|| AppError::bad("SMS number must be a phone number like +15035550100"))?),
            None => None,
        };
    }
    if let Some(tz) = b.timezone {
        tz.parse::<chrono_tz::Tz>().map_err(|_| AppError::bad("unknown time zone (use e.g. America/Los_Angeles)"))?;
        account.timezone = tz;
    }
    if let Some(v) = blank_to_none(b.address) {
        account.address = v;
    }
    if let Some(c) = b.brand_color {
        let ok = c.len() <= 9 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
        if !ok {
            return Err(AppError::bad("brand color must be a hex color like #1f6f5c"));
        }
        account.brand_color = c;
    }
    if let Some(v) = blank_to_none(b.logo_url) {
        account.logo_url = v;
    }
    if let Some(m) = b.conversion_metric.filter(|m| !m.trim().is_empty()) {
        account.conversion_metric = m;
    }
    if let Some(o) = b.onboarding {
        if let Some(cur) = account.onboarding.as_object_mut() {
            for (k, v) in o {
                if v.is_boolean() {
                    cur.insert(k, v);
                }
            }
        }
    }
    let saved: Account = sqlx::query_as(
        "UPDATE accounts SET name=$2, website=$3, from_name=$4, from_email=$5, reply_to=$6, sms_from=$7, timezone=$8, address=$9,
            brand_color=$10, logo_url=$11, conversion_metric=$12, onboarding=$13 WHERE id=$1 RETURNING *",
    )
    .bind(account.id)
    .bind(&account.name)
    .bind(&account.website)
    .bind(&account.from_name)
    .bind(&account.from_email)
    .bind(&account.reply_to)
    .bind(&account.sms_from)
    .bind(&account.timezone)
    .bind(&account.address)
    .bind(&account.brand_color)
    .bind(&account.logo_url)
    .bind(&account.conversion_metric)
    .bind(&account.onboarding)
    .fetch_one(&state.db)
    .await?;
    Ok(saved)
}

async fn update_account(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<AccountPatch>) -> AppResult<Json<Account>> {
    Ok(Json(patch_account(&state, ctx.account_id, b).await?))
}

pub fn sending_json(state: &AppState, a: &Account) -> Value {
    json!({
        "email": {
            "provider": state.email.name(),
            "domain": a.sending_domain,
            "status": a.sending_domain_status,
            "dns_records": a.sending_domain_records,
            "from_email": a.from_email,
        },
        "sms": {
            "provider": state.sms.name(),
            "from_number": a.sms_from,
            "status": if a.sms_from.is_some() { "ready" } else { "not_started" },
        }
    })
}

async fn sending(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let a = load_account(&state.db, ctx.account_id).await?;
    Ok(Json(sending_json(&state, &a)))
}

#[derive(Deserialize)]
struct DomainBody {
    domain: String,
}

pub async fn set_domain(state: &AppState, account_id: Uuid, domain: &str) -> AppResult<Value> {
    let domain = domain.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_lowercase();
    let valid = domain.contains('.') && domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') && domain.len() < 254;
    if !valid {
        return Err(AppError::bad("enter a domain like mail.yourbrand.com"));
    }
    let st = state.email.add_domain(&domain).await.map_err(|e| AppError::bad(format!("could not start domain setup: {e:#}")))?;
    let a: Account = sqlx::query_as("UPDATE accounts SET sending_domain = $2, sending_domain_status = $3, sending_domain_records = $4 WHERE id = $1 RETURNING *")
        .bind(account_id)
        .bind(&domain)
        .bind(&st.status)
        .bind(serde_json::to_value(&st.records)?)
        .fetch_one(&state.db)
        .await?;
    Ok(sending_json(state, &a))
}

async fn add_domain(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<DomainBody>) -> AppResult<Json<Value>> {
    Ok(Json(set_domain(&state, ctx.account_id, &b.domain).await?))
}

async fn verify_domain(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let a = load_account(&state.db, ctx.account_id).await?;
    let domain = a.sending_domain.clone().ok_or_else(|| AppError::bad("add a sending domain first"))?;
    let st = state.email.domain_state(&domain).await.map_err(|e| AppError::bad(format!("could not check DNS: {e:#}")))?;
    let a: Account = sqlx::query_as("UPDATE accounts SET sending_domain_status = $2, sending_domain_records = $3 WHERE id = $1 RETURNING *")
        .bind(ctx.account_id)
        .bind(&st.status)
        .bind(serde_json::to_value(&st.records)?)
        .fetch_one(&state.db)
        .await?;
    if st.status == "verified" {
        sqlx::query("UPDATE accounts SET onboarding = onboarding || '{\"sender\": true}' WHERE id = $1").bind(ctx.account_id).execute(&state.db).await?;
    }
    Ok(Json(sending_json(&state, &a)))
}

async fn list_keys(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let rows: Vec<(Uuid, String, String, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT id, name, prefix, created_at, last_used_at FROM api_keys WHERE account_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC",
    )
    .bind(ctx.account_id)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, prefix, created_at, last_used_at)| json!({"id": id, "name": name, "prefix": prefix, "created_at": created_at, "last_used_at": last_used_at}))
        .collect();
    Ok(Json(json!({"data": data})))
}

#[derive(Deserialize)]
struct KeyBody {
    name: Option<String>,
}

pub async fn new_api_key(state: &AppState, account_id: Uuid, name: &str) -> AppResult<Value> {
    let key = format!("{API_KEY_PREFIX}{}", auth::random_token(24));
    let prefix: String = key.chars().take(12).collect();
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO api_keys (id, account_id, name, prefix, key_hash) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(account_id)
        .bind(name)
        .bind(&prefix)
        .bind(auth::sha256_hex(&key))
        .execute(&state.db)
        .await?;
    Ok(json!({"id": id, "name": name, "prefix": prefix, "key": key}))
}

async fn create_key(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<KeyBody>) -> AppResult<Json<Value>> {
    if ctx.via_api_key {
        return Err(AppError::Forbidden("create API keys from the dashboard".into()));
    }
    let name = b.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "API key".into());
    Ok(Json(new_api_key(&state, ctx.account_id, &name).await?))
}

async fn revoke_key(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let n = sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND account_id = $2 AND revoked_at IS NULL")
        .bind(id)
        .bind(ctx.account_id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("API key"));
    }
    Ok(Json(json!({"ok": true})))
}

pub async fn billing_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let a = load_account(&state.db, account_id).await?;
    let plan = if state.cfg.hosted { a.plan.clone() } else { "self_hosted".into() };
    let limits = crate::plans::limits(&plan);
    let usage = crate::plans::usage(&state.db, account_id).await?;
    let cap = |v: i64| if v == i64::MAX { Value::Null } else { json!(v) };
    Ok(json!({
        "plan": plan,
        "limits": {"profiles": cap(limits.profiles), "emails_per_month": cap(limits.emails_per_month), "sms_per_month": cap(limits.sms_per_month)},
        "usage": usage,
    }))
}

async fn billing(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(billing_json(&state, ctx.account_id).await?))
}
