//! Team members, invites, roles, ownership, and several accounts per login.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use axum_extra::extract::cookie::CookieJar;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::access::Role;
use super::audit::{audit, record};
use super::login::{self, ReqMeta};
use super::{mail, ratelimit};
use crate::api::ApiJson;
use crate::auth::{self, Ctx};
use crate::error::{AppError, AppResult};
use crate::models::normalize_email;
use crate::state::AppState;

/// How long an invite link works.
pub const INVITE_DAYS: i64 = 7;
pub const MAX_MEMBERS: i64 = 200;
pub const MAX_OPEN_INVITES: i64 = 100;
/// Accounts one login can belong to.
pub const MAX_ACCOUNTS_PER_LOGIN: i64 = 50;

fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => format!("{}***@{domain}", local.chars().next().unwrap_or('*')),
        None => "***".into(),
    }
}

// ---- Listing ----

pub async fn team_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let members: Vec<(Uuid, String, Option<String>, String, DateTime<Utc>, bool, Option<DateTime<Utc>>, bool)> = sqlx::query_as(
        "SELECT u.id, u.email, u.name, m.role, m.created_at, u.totp_enabled_at IS NOT NULL,
                (SELECT max(s.last_seen_at) FROM sessions s WHERE s.user_id = u.id AND s.account_id = m.account_id),
                EXISTS (SELECT 1 FROM sso_identities i WHERE i.user_id = u.id)
         FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1
         ORDER BY CASE m.role WHEN 'owner' THEN 0 WHEN 'admin' THEN 1 WHEN 'manager' THEN 2 WHEN 'analyst' THEN 3 ELSE 4 END, lower(u.email) LIMIT 500",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let invites: Vec<(Uuid, String, String, Option<String>, DateTime<Utc>, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT i.id, i.email, i.role, u.email, i.created_at, i.sent_at, i.expires_at FROM team_invites i LEFT JOIN users u ON u.id = i.invited_by
         WHERE i.account_id = $1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL ORDER BY i.created_at DESC LIMIT 200",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let now = Utc::now();
    Ok(json!({
        "members": members.into_iter().map(|(id, email, name, role, joined, tfa, seen, sso)| json!({
            "user_id": id, "email": email, "name": name, "role": role, "joined_at": joined, "two_factor": tfa, "last_active_at": seen, "sso_linked": sso,
        })).collect::<Vec<_>>(),
        "invites": invites.into_iter().map(|(id, email, role, by, created, sent, expires)| json!({
            "id": id, "email": email, "role": role, "invited_by": by, "created_at": created, "sent_at": sent, "expires_at": expires, "expired": expires < now,
        })).collect::<Vec<_>>(),
        "roles": Role::ALL.iter().map(|r| json!({"role": r.as_str(), "label": r.label()})).collect::<Vec<_>>(),
    }))
}

pub async fn team_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(team_json(&state, ctx.account_id).await?))
}

// ---- Invites ----

#[derive(Deserialize)]
pub struct InviteBody {
    pub email: String,
    pub role: String,
}

fn parse_role(s: &str) -> AppResult<Role> {
    Role::parse(s).ok_or_else(|| AppError::bad("role must be owner, admin, manager, analyst or content"))
}

fn may_assign(ctx: &Ctx, role: Role) -> AppResult<()> {
    if role == Role::Owner && ctx.role != Role::Owner {
        return Err(AppError::Forbidden("only owners can make someone an owner".into()));
    }
    Ok(())
}

async fn inviter_name(state: &AppState, ctx: &Ctx) -> AppResult<String> {
    if let Some(uid) = ctx.user_id {
        let (email, name): (String, Option<String>) = sqlx::query_as("SELECT email, name FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
        return Ok(mail::plain(name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&email), 80));
    }
    Ok("A teammate".into())
}

async fn send_invite(state: &AppState, ctx: &Ctx, email: &str, role: Role, token: &str) -> AppResult<bool> {
    let account: String = sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1").bind(ctx.account_id).fetch_one(&state.db).await?;
    let account = mail::plain(&account, 80);
    let who = inviter_name(state, ctx).await?;
    let url = format!("{}/invite/{token}", state.cfg.public_url());
    Ok(mail::send(
        state,
        email,
        &format!("{who} invited you to {account} on DumbMail"),
        &[format!("{who} invited you to join {account} on DumbMail as {}.", role.label()), format!("The link works for {INVITE_DAYS} days.")],
        ("Accept the invite", &url),
        "If you weren't expecting this, you can ignore this email.",
    )
    .await)
}

fn invite_result(state: &AppState, id: Uuid, email: &str, role: Role, expires: DateTime<Utc>, sent: bool, token: &str) -> Value {
    let mut v = json!({"invite": {"id": id, "email": email, "role": role.as_str(), "expires_at": expires}, "email_sent": sent});
    // Without account email (no SUPPORT_EMAIL) the inviter passes the link on themselves.
    if !mail::available(state) {
        v["invite_url"] = json!(format!("{}/invite/{token}", state.cfg.public_url()));
    }
    v
}

/// Invite someone. With `dry_run` nothing is saved or sent.
pub async fn invite(state: &AppState, ctx: &Ctx, b: InviteBody, dry_run: bool) -> AppResult<Value> {
    let role = parse_role(&b.role)?;
    may_assign(ctx, role)?;
    let email = normalize_email(&b.email).ok_or_else(|| AppError::bad("enter a valid email"))?;
    let member: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 AND u.email = $2)")
        .bind(ctx.account_id)
        .bind(&email)
        .fetch_one(&state.db)
        .await?;
    if member {
        return Err(AppError::Conflict(format!("{email} is already on the team")));
    }
    let (members, open): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM memberships WHERE account_id = $1),
                (SELECT count(*) FROM team_invites WHERE account_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL)",
    )
    .bind(ctx.account_id)
    .fetch_one(&state.db)
    .await?;
    if open >= MAX_OPEN_INVITES || members + open >= MAX_MEMBERS {
        return Err(AppError::bad(format!("an account can have up to {MAX_MEMBERS} members and {MAX_OPEN_INVITES} open invites; revoke old invites first")));
    }
    let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM team_invites WHERE account_id = $1 AND email = $2 AND accepted_at IS NULL AND revoked_at IS NULL")
        .bind(ctx.account_id)
        .bind(&email)
        .fetch_optional(&state.db)
        .await?;
    if existing.is_some() {
        return Err(AppError::Conflict(format!("{email} already has an open invite; resend it instead")));
    }
    if dry_run {
        return Ok(json!({"dry_run": true, "would_invite": {"email": email, "role": role.as_str()}, "message": "Sends an invite email to this address. Call again with confirm=true to send it."}));
    }
    ratelimit::hit(state, &format!("invite:account:{}", ctx.account_id), 30, 3600).await?;
    let token = auth::random_token(32);
    let id = Uuid::now_v7();
    let expires: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO team_invites (id, account_id, email, role, token_hash, invited_by, expires_at) VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(days => $7)) RETURNING expires_at",
    )
    .bind(id)
    .bind(ctx.account_id)
    .bind(&email)
    .bind(role.as_str())
    .bind(auth::sha256_hex(&token))
    .bind(ctx.user_id)
    .bind(INVITE_DAYS as i32)
    .fetch_one(&state.db)
    .await?;
    let sent = send_invite(state, ctx, &email, role, &token).await?;
    audit(state, ctx, "team.invite", Some(&email), json!({"role": role.as_str(), "email_sent": sent})).await;
    Ok(invite_result(state, id, &email, role, expires, sent, &token))
}

pub async fn invite_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<InviteBody>) -> AppResult<Json<Value>> {
    Ok(Json(invite(&state, &ctx, b, false).await?))
}

/// A new link (the old one stops working) and another 7 days.
pub async fn resend(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    ratelimit::hit(state, &format!("invite:resend:{id}"), 5, 3600).await?;
    let row: Option<(String, String)> = sqlx::query_as("SELECT email, role FROM team_invites WHERE id = $1 AND account_id = $2 AND accepted_at IS NULL AND revoked_at IS NULL")
        .bind(id)
        .bind(ctx.account_id)
        .fetch_optional(&state.db)
        .await?;
    let (email, role) = row.ok_or_else(|| AppError::not_found("invite"))?;
    let role = parse_role(&role)?;
    may_assign(ctx, role)?;
    let token = auth::random_token(32);
    let expires: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE team_invites SET token_hash = $3, sent_at = now(), expires_at = now() + make_interval(days => $4) WHERE id = $1 AND account_id = $2 RETURNING expires_at",
    )
    .bind(id)
    .bind(ctx.account_id)
    .bind(auth::sha256_hex(&token))
    .bind(INVITE_DAYS as i32)
    .fetch_one(&state.db)
    .await?;
    let sent = send_invite(state, ctx, &email, role, &token).await?;
    audit(state, ctx, "team.invite_resend", Some(&email), json!({"role": role.as_str(), "email_sent": sent})).await;
    Ok(invite_result(state, id, &email, role, expires, sent, &token))
}

pub async fn resend_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(resend(&state, &ctx, id).await?))
}

pub async fn revoke_invite(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let email: Option<String> = sqlx::query_scalar("UPDATE team_invites SET revoked_at = now() WHERE id = $1 AND account_id = $2 AND accepted_at IS NULL AND revoked_at IS NULL RETURNING email")
        .bind(id)
        .bind(ctx.account_id)
        .fetch_optional(&state.db)
        .await?;
    let email = email.ok_or_else(|| AppError::not_found("invite"))?;
    audit(state, ctx, "team.invite_revoke", Some(&email), json!({})).await;
    Ok(json!({"ok": true}))
}

pub async fn revoke_invite_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(revoke_invite(&state, &ctx, id).await?))
}

#[derive(sqlx::FromRow)]
struct OpenInvite {
    id: Uuid,
    account_id: Uuid,
    email: String,
    role: String,
    expires_at: DateTime<Utc>,
    account_name: String,
    invited_by: Option<String>,
}

async fn open_invite(state: &AppState, token: &str) -> AppResult<OpenInvite> {
    let gone = || AppError::NotFound("this invite link is no longer valid; ask for a new one".into());
    if token.len() > 200 {
        return Err(gone());
    }
    sqlx::query_as(
        "SELECT i.id, i.account_id, i.email, i.role, i.expires_at, a.name AS account_name, COALESCE(u.name, u.email) AS invited_by
         FROM team_invites i JOIN accounts a ON a.id = i.account_id LEFT JOIN users u ON u.id = i.invited_by
         WHERE i.token_hash = $1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL AND i.expires_at > now()",
    )
    .bind(auth::sha256_hex(token))
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(gone)
}

fn session_user(ctx: &Result<Ctx, AppError>) -> Option<&Ctx> {
    ctx.as_ref().ok().filter(|c| !c.via_api_key && c.user_id.is_some())
}

/// `GET /api/invites/{token}`: what the invite is for. Only someone holding the link sees this.
pub async fn show_invite_h(State(state): State<AppState>, headers: HeaderMap, ctx: Result<Ctx, AppError>, Path(token): Path<String>) -> AppResult<Json<Value>> {
    ratelimit::hit_ip(&state, "invite_view", login::client_ip(&headers).as_deref(), 60, 900).await?;
    let inv = open_invite(&state, &token).await?;
    let has_login: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE email = $1)").bind(&inv.email).fetch_one(&state.db).await?;
    let signed_in_as: Option<String> = match session_user(&ctx).and_then(|c| c.user_id) {
        Some(uid) => sqlx::query_scalar("SELECT email FROM users WHERE id = $1").bind(uid).fetch_optional(&state.db).await?,
        None => None,
    };
    Ok(Json(json!({
        "account_name": inv.account_name, "email": inv.email, "role": inv.role, "invited_by": inv.invited_by, "expires_at": inv.expires_at,
        "has_login": has_login, "signed_in_as": signed_in_as,
    })))
}

#[derive(Deserialize, Default)]
pub struct AcceptBody {
    pub name: Option<String>,
    pub password: Option<String>,
}

/// Join the account in one transaction; false when the invite was used or revoked meanwhile.
async fn join(state: &AppState, inv: &OpenInvite, user_id: Uuid) -> AppResult<bool> {
    let mut tx = state.db.begin().await?;
    let n = sqlx::query("UPDATE team_invites SET accepted_at = now(), accepted_by = $2 WHERE id = $1 AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now()")
        .bind(inv.id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n != 1 {
        return Ok(false);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships WHERE user_id = $1").bind(user_id).fetch_one(&mut *tx).await?;
    if count >= MAX_ACCOUNTS_PER_LOGIN {
        return Err(AppError::bad(format!("a login can belong to up to {MAX_ACCOUNTS_PER_LOGIN} accounts")));
    }
    sqlx::query("INSERT INTO memberships (user_id, account_id, role) VALUES ($1, $2, $3) ON CONFLICT (user_id, account_id) DO NOTHING")
        .bind(user_id)
        .bind(inv.account_id)
        .bind(&inv.role)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// `POST /api/invites/{token}/accept`. Signed in: the login's email must be the invited one.
/// Signed out: creates a login for the invited email (the link proves they got the email); if that
/// email already has a login, they sign in first.
pub async fn accept_h(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    ctx: Result<Ctx, AppError>,
    Path(token): Path<String>,
    body: axum::body::Bytes,
) -> AppResult<(CookieJar, Json<Value>)> {
    let meta = ReqMeta::from_headers(&headers);
    ratelimit::hit_ip(&state, "invite_accept", meta.ip.as_deref(), 20, 900).await?;
    let b: AcceptBody = if body.iter().all(|c| c.is_ascii_whitespace()) { AcceptBody::default() } else { serde_json::from_slice(&body)? };
    let inv = open_invite(&state, &token).await?;
    let gone = || AppError::NotFound("this invite link is no longer valid; ask for a new one".into());
    if let Some(c) = session_user(&ctx) {
        let uid = c.user_id.expect("session user");
        let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
        if !login::same(&email, &inv.email) {
            return Err(AppError::Forbidden(format!("this invite was sent to {}; sign in with that email to accept it", mask_email(&inv.email))));
        }
        if !join(&state, &inv, uid).await? {
            return Err(gone());
        }
        record(&state, inv.account_id, &meta.actor(Some(uid)), "team.join", Some(&inv.email), json!({"role": inv.role})).await;
        // Open the new account, unless this is a single sign-on session (trusted for its own account only).
        if !c.sso_session {
            if let Some(sid) = c.session_id {
                sqlx::query("UPDATE sessions SET account_id = $2 WHERE id = $1").bind(sid).bind(inv.account_id).execute(&state.db).await?;
                sqlx::query("UPDATE users SET last_account_id = $2 WHERE id = $1").bind(uid).bind(inv.account_id).execute(&state.db).await?;
                let fresh = login::ctx_for_session_id(&state, sid, &meta).await?;
                return Ok((jar, Json(login::me_json(&state, &fresh).await?)));
            }
        }
        return Ok((jar, Json(login::me_json(&state, c).await?)));
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE email = $1)").bind(&inv.email).fetch_one(&state.db).await?;
    if exists {
        return Err(AppError::Conflict(format!("{} already has a DumbMail login; sign in with it to accept", inv.email)));
    }
    let password = b.password.unwrap_or_default();
    if password.chars().count() < 8 || password.len() > 200 {
        return Err(AppError::bad("use a password of at least 8 characters"));
    }
    let uid = Uuid::now_v7();
    let name = b.name.map(|n| mail::plain(&n, 100)).filter(|n| !n.is_empty());
    sqlx::query("INSERT INTO users (id, email, name, password_hash) VALUES ($1, $2, $3, $4)")
        .bind(uid)
        .bind(&inv.email)
        .bind(name)
        .bind(auth::hash_password(&password)?)
        .execute(&state.db)
        .await?;
    if !join(&state, &inv, uid).await? {
        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&state.db).await?;
        return Err(gone());
    }
    record(&state, inv.account_id, &meta.actor(Some(uid)), "team.join", Some(&inv.email), json!({"role": inv.role, "new_login": true})).await;
    let token = login::start_session(&state, uid, inv.account_id, "password", None, &meta).await?;
    let me = login::me_for_token(&state, &token, &meta).await?;
    Ok((jar.add(crate::api::account::session_cookie(&state, token)), Json(me)))
}

/// Google sign-in coming back from an invite link (`next=/invite/<token>`) for an email that has
/// no login yet: make the login and join the invited account, instead of starting a new account.
pub async fn google_invite_signup(state: &AppState, next: &str, email: &str, google_sub: &str, name: Option<String>) -> AppResult<Option<(Uuid, Uuid)>> {
    let Some(token) = next.strip_prefix("/invite/").map(|t| t.split(['?', '#', '/']).next().unwrap_or("")) else { return Ok(None) };
    let Ok(inv) = open_invite(state, token).await else { return Ok(None) };
    if !login::same(&inv.email, email) {
        return Ok(None);
    }
    let uid = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, name, google_sub) VALUES ($1, $2, $3, $4)").bind(uid).bind(email).bind(name).bind(google_sub).execute(&state.db).await?;
    if !join(state, &inv, uid).await? {
        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&state.db).await?;
        return Ok(None);
    }
    record(state, inv.account_id, &super::audit::Actor { user_id: Some(uid), ..Default::default() }, "team.join", Some(email), json!({"role": inv.role, "new_login": true, "method": "google"})).await;
    Ok(Some((uid, inv.account_id)))
}

// ---- Roles, removal and ownership ----

#[derive(Deserialize)]
pub struct RoleBody {
    pub role: String,
}

async fn member_row(tx: &mut sqlx::PgConnection, account_id: Uuid, user_id: Uuid) -> AppResult<(String, Role)> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT u.email, m.role FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 AND m.user_id = $2 FOR UPDATE OF m")
        .bind(account_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let (email, role) = row.ok_or_else(|| AppError::not_found("member"))?;
    Ok((email, Role::parse(&role).unwrap_or(Role::Analyst)))
}

/// Lock the account's owners and count them, so two changes can't both remove "the other" owner.
async fn owners(tx: &mut sqlx::PgConnection, account_id: Uuid) -> AppResult<i64> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM memberships WHERE account_id = $1 AND role = 'owner' FOR UPDATE").bind(account_id).fetch_all(&mut *tx).await?;
    Ok(ids.len() as i64)
}

pub async fn change_role(state: &AppState, ctx: &Ctx, user_id: Uuid, role: &str) -> AppResult<Value> {
    let new = parse_role(role)?;
    let mut tx = state.db.begin().await?;
    let owner_count = owners(&mut tx, ctx.account_id).await?;
    let (email, old) = member_row(&mut tx, ctx.account_id, user_id).await?;
    if (old == Role::Owner || new == Role::Owner) && ctx.role != Role::Owner {
        return Err(AppError::Forbidden("only owners can make or change owners".into()));
    }
    if old == Role::Owner && new != Role::Owner && owner_count <= 1 {
        return Err(AppError::Conflict("the last owner can't be demoted; make someone else an owner first, or transfer ownership".into()));
    }
    sqlx::query("UPDATE memberships SET role = $3 WHERE account_id = $1 AND user_id = $2").bind(ctx.account_id).bind(user_id).bind(new.as_str()).execute(&mut *tx).await?;
    tx.commit().await?;
    if old != new {
        audit(state, ctx, "team.role_change", Some(&email), json!({"from": old.as_str(), "to": new.as_str()})).await;
    }
    Ok(json!({"user_id": user_id, "email": email, "role": new.as_str()}))
}

pub async fn change_role_h(State(state): State<AppState>, ctx: Ctx, Path(user_id): Path<Uuid>, ApiJson(b): ApiJson<RoleBody>) -> AppResult<Json<Value>> {
    Ok(Json(change_role(&state, &ctx, user_id, &b.role).await?))
}

/// Take someone off the account: their membership, the API keys they made here, and their
/// sessions on it (moved to another account of theirs when they have one).
pub async fn remove(state: &AppState, ctx: &Ctx, user_id: Uuid, dry_run: bool) -> AppResult<Value> {
    let mut tx = state.db.begin().await?;
    let owner_count = owners(&mut tx, ctx.account_id).await?;
    let (email, role) = member_row(&mut tx, ctx.account_id, user_id).await?;
    let leaving = Some(user_id) == ctx.user_id;
    if role == Role::Owner && ctx.role != Role::Owner && !leaving {
        return Err(AppError::Forbidden("only owners can remove an owner".into()));
    }
    if role == Role::Owner && owner_count <= 1 {
        return Err(AppError::Conflict("the last owner can't be removed; transfer ownership first".into()));
    }
    if dry_run {
        return Ok(json!({"dry_run": true, "would_remove": {"user_id": user_id, "email": email, "role": role.as_str()}, "message": "Removes their access to this account and revokes the API keys they made here. Call again with confirm=true."}));
    }
    sqlx::query("DELETE FROM memberships WHERE account_id = $1 AND user_id = $2").bind(ctx.account_id).bind(user_id).execute(&mut *tx).await?;
    let keys = sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE account_id = $1 AND created_by = $2 AND revoked_at IS NULL").bind(ctx.account_id).bind(user_id).execute(&mut *tx).await?.rows_affected();
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND (sso_account_id = $2 OR (account_id = $2 AND NOT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1)))")
        .bind(user_id)
        .bind(ctx.account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE sessions SET account_id = (SELECT account_id FROM memberships WHERE user_id = $1 ORDER BY created_at LIMIT 1) WHERE user_id = $1 AND account_id = $2")
        .bind(user_id)
        .bind(ctx.account_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    audit(state, ctx, if leaving { "team.leave" } else { "team.member_remove" }, Some(&email), json!({"role": role.as_str(), "api_keys_revoked": keys})).await;
    Ok(json!({"ok": true, "email": email, "api_keys_revoked": keys}))
}

pub async fn remove_h(State(state): State<AppState>, ctx: Ctx, Path(user_id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(remove(&state, &ctx, user_id, false).await?))
}

/// `POST /api/team/leave`: take yourself off this account.
pub async fn leave_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let uid = ctx.user_id.ok_or(AppError::Unauthorized)?;
    Ok(Json(remove(&state, &ctx, uid, false).await?))
}

#[derive(Deserialize)]
pub struct TransferBody {
    pub user_id: Uuid,
}

/// `POST /api/team/transfer`: make another member the owner; you become an admin.
pub async fn transfer_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TransferBody>) -> AppResult<Json<Value>> {
    let me = ctx.user_id.ok_or(AppError::Unauthorized)?;
    if b.user_id == me {
        return Err(AppError::bad("pick another member to transfer ownership to"));
    }
    let mut tx = state.db.begin().await?;
    owners(&mut tx, ctx.account_id).await?;
    let (email, _) = member_row(&mut tx, ctx.account_id, b.user_id).await?;
    sqlx::query("UPDATE memberships SET role = 'owner' WHERE account_id = $1 AND user_id = $2").bind(ctx.account_id).bind(b.user_id).execute(&mut *tx).await?;
    sqlx::query("UPDATE memberships SET role = 'admin' WHERE account_id = $1 AND user_id = $2").bind(ctx.account_id).bind(me).execute(&mut *tx).await?;
    tx.commit().await?;
    audit(&state, &ctx, "team.ownership_transfer", Some(&email), json!({"previous_owner_now": "admin"})).await;
    Ok(Json(team_json(&state, ctx.account_id).await?))
}

// ---- Several accounts per login ----

/// `GET /api/accounts`
pub async fn accounts_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let me = login::me_json(&state, &ctx).await?;
    Ok(Json(json!({"data": me["accounts"]})))
}

#[derive(Deserialize)]
pub struct SwitchBody {
    pub account_id: Uuid,
}

/// `POST /api/accounts/switch`: open another account this login belongs to.
pub async fn switch_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<SwitchBody>) -> AppResult<Json<Value>> {
    let uid = ctx.user_id.ok_or(AppError::Unauthorized)?;
    let sid = ctx.session_id.ok_or(AppError::Unauthorized)?;
    let member: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM memberships WHERE user_id = $1 AND account_id = $2)").bind(uid).bind(b.account_id).fetch_one(&state.db).await?;
    if !member {
        return Err(AppError::not_found("account"));
    }
    sqlx::query("UPDATE sessions SET account_id = $2 WHERE id = $1").bind(sid).bind(b.account_id).execute(&state.db).await?;
    sqlx::query("UPDATE users SET last_account_id = $2 WHERE id = $1").bind(uid).bind(b.account_id).execute(&state.db).await?;
    let fresh = login::ctx_for_session_id(&state, sid, &ReqMeta { ip: ctx.ip.clone(), user_agent: ctx.user_agent.clone() }).await?;
    Ok(Json(login::me_json(&state, &fresh).await?))
}

#[derive(Deserialize)]
pub struct NewAccountBody {
    pub name: String,
}

/// `POST /api/accounts`: start another account (another brand) with you as its owner, and open it.
pub async fn create_account_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<NewAccountBody>) -> AppResult<Json<Value>> {
    let uid = ctx.user_id.ok_or(AppError::Unauthorized)?;
    let sid = ctx.session_id.ok_or(AppError::Unauthorized)?;
    let name = mail::plain(&b.name, 100);
    if name.is_empty() {
        return Err(AppError::bad("enter the brand or company name"));
    }
    ratelimit::hit(&state, &format!("account_create:user:{uid}"), 10, 86400).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships WHERE user_id = $1").bind(uid).fetch_one(&state.db).await?;
    if count >= MAX_ACCOUNTS_PER_LOGIN {
        return Err(AppError::bad(format!("a login can belong to up to {MAX_ACCOUNTS_PER_LOGIN} accounts")));
    }
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    sqlx::query("INSERT INTO accounts (id, public_key, name, from_name) VALUES ($1, $2, $3, $3)").bind(id).bind(format!("pk_{}", auth::random_token(12))).bind(&name).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO memberships (user_id, account_id, role) VALUES ($1, $2, 'owner')").bind(uid).bind(id).execute(&mut *tx).await?;
    sqlx::query("UPDATE sessions SET account_id = $2 WHERE id = $1").bind(sid).bind(id).execute(&mut *tx).await?;
    sqlx::query("UPDATE users SET last_account_id = $2 WHERE id = $1").bind(uid).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    let fresh = login::ctx_for_session_id(&state, sid, &ReqMeta { ip: ctx.ip.clone(), user_agent: ctx.user_agent.clone() }).await?;
    audit(&state, &fresh, "account.create", Some(&name), json!({"from_account": ctx.account_id})).await;
    Ok(Json(login::me_json(&state, &fresh).await?))
}

// ---- Account security settings ----

pub async fn security_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let (require, without): (bool, i64) = sqlx::query_as(
        "SELECT a.require_two_factor, (SELECT count(*) FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = a.id AND u.totp_enabled_at IS NULL)
         FROM accounts a WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let sso = super::sso::summary(state, account_id).await?;
    Ok(json!({"require_two_factor": require, "members_without_two_factor": without, "sso": sso}))
}

pub async fn security_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(security_json(&state, ctx.account_id).await?))
}

#[derive(Deserialize)]
pub struct SecurityBody {
    pub require_two_factor: bool,
}

/// `PATCH /api/team/security`: require two-factor login for every member.
pub async fn update_security_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<SecurityBody>) -> AppResult<Json<Value>> {
    let uid = ctx.user_id.ok_or(AppError::Unauthorized)?;
    if b.require_two_factor && !ctx.sso_session {
        let on: bool = sqlx::query_scalar("SELECT totp_enabled_at IS NOT NULL FROM users WHERE id = $1").bind(uid).fetch_one(&state.db).await?;
        if !on {
            return Err(AppError::bad("turn on two-factor login for yourself first (Your two-factor login, above)"));
        }
    }
    sqlx::query("UPDATE accounts SET require_two_factor = $2 WHERE id = $1").bind(ctx.account_id).bind(b.require_two_factor).execute(&state.db).await?;
    audit(&state, &ctx, "security.two_factor_requirement", None, json!({"required": b.require_two_factor})).await;
    Ok(Json(security_json(&state, ctx.account_id).await?))
}

#[cfg(test)]
mod tests {
    #[test]
    fn masks() {
        assert_eq!(super::mask_email("ann@acme.com"), "a***@acme.com");
    }
}
