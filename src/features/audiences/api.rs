//! REST routes for ad audiences. The MCP tools call the same functions.

use axum::extract::{Path, Query, State};
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::QueryBuilder;
use uuid::Uuid;

use super::platforms::{self, PErr, Platform, Target};
use super::store::{self, ConnRow, SyncRow, SYNC_COLS};
use super::sync;
use crate::api::ApiJson;
use crate::auth::{self, Ctx};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const SETTINGS_PATH: &str = "/settings/ad-audiences";
const MAX_SYNCS: i64 = 50;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/ad-audiences/connections", get(connections_h))
        .route("/api/ad-audiences/connections/{platform}", axum::routing::delete(disconnect_h))
        .route("/api/ad-audiences/connections/{platform}/start", post(start_h))
        .route("/api/ad-audiences/connections/{platform}/accounts", get(accounts_h))
        .route("/api/ad-audiences/oauth/{platform}/callback", get(callback_h))
        .route("/api/ad-audiences/syncs", get(syncs_h).post(create_h))
        .route("/api/ad-audiences/syncs/{id}", get(sync_h).patch(update_h).delete(delete_h))
        .route("/api/ad-audiences/syncs/{id}/run", post(run_h))
}

fn platform(s: &str) -> AppResult<Platform> {
    Platform::parse(s).ok_or_else(|| AppError::bad("platform must be meta, google or tiktok"))
}

fn perr(e: PErr) -> AppError {
    AppError::BadRequest(e.message().to_string())
}

// ---------- Connections ----------

pub async fn connections(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<ConnRow> = sqlx::query_as(&format!("SELECT {} FROM ad_connections WHERE account_id = $1", store::CONN_COLS)).bind(account_id).fetch_all(&state.db).await?;
    let counts: Vec<(String, i64)> = sqlx::query_as("SELECT platform, count(*) FROM ad_audience_syncs WHERE account_id = $1 GROUP BY platform").bind(account_id).fetch_all(&state.db).await?;
    let out: Vec<Value> = Platform::ALL
        .iter()
        .map(|p| {
            let c = rows.iter().find(|r| r.platform == p.as_str());
            let status = match c.map(|c| c.status.as_str()) {
                Some("connected") => "connected",
                Some("needs_reconnect") => "needs_reconnect",
                _ => "not_connected",
            };
            let connected = status != "not_connected";
            json!({
                "platform": p.as_str(),
                "label": p.label(),
                "available": platforms::app(&state.cfg, *p).is_some(),
                "status": status,
                "connected_as": c.filter(|_| connected).and_then(|c| c.connected_as.clone()),
                "connected_at": c.filter(|_| connected).and_then(|c| c.connected_at),
                "token_hint": c.filter(|_| connected).and_then(|c| c.token_hint.clone()),
                "token_expires_at": c.filter(|_| connected).and_then(|c| c.token_expires_at),
                "error": c.filter(|_| status == "needs_reconnect").and_then(|c| c.error.clone()),
                "manual_account_entry": *p == Platform::Google && platforms::google_developer_token(&state.cfg).is_none(),
                "syncs": counts.iter().find(|(k, _)| k == p.as_str()).map(|(_, n)| *n).unwrap_or(0),
            })
        })
        .collect();
    Ok(json!({"data": out, "connect_url": format!("{}{SETTINGS_PATH}", state.cfg.public_url())}))
}

async fn connections_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(connections(&state, ctx.account_id).await?))
}

/// Begin a sign-in: remember a random state (hashed) and, for Google, a PKCE verifier; return the
/// platform's sign-in URL for the browser to open.
async fn start_h(State(state): State<AppState>, ctx: Ctx, Path(p): Path<String>) -> AppResult<Json<Value>> {
    let p = platform(&p)?;
    if platforms::app(&state.cfg, p).is_none() {
        return Err(AppError::BadRequest(format!("{} isn't available on this server.", p.label())));
    }
    let st = auth::random_token(32);
    let verifier = auth::random_token(48);
    let verifier_enc = crate::crypto::encrypt(&state.cfg.secret_key, &verifier)?;
    sqlx::query("INSERT INTO ad_oauth_states (state_hash, account_id, user_id, platform, verifier_enc) VALUES ($1, $2, $3, $4, $5)")
        .bind(auth::sha256_hex(&st))
        .bind(ctx.account_id)
        .bind(ctx.user_id)
        .bind(p.as_str())
        .bind(verifier_enc)
        .execute(&state.db)
        .await?;
    let url = platforms::authorize_url(&state.cfg, p, &st, &platforms::pkce_challenge(&verifier)).ok_or_else(|| AppError::bad("sign-in isn't available"))?;
    Ok(Json(json!({"url": url})))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    auth_code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// The platform sends the browser back here. Errors go to the settings page as a short code it
/// explains, never as free text. The signed-in session must be the account (and person) that
/// started the sign-in, so nobody can attach their ad account to someone else's DumbMail account or
/// the reverse with a crafted link.
async fn callback_h(State(state): State<AppState>, ctx: Result<Ctx, AppError>, Path(p): Path<String>, Query(q): Query<CallbackQuery>) -> Redirect {
    let Some(p) = Platform::parse(&p) else { return Redirect::to(&format!("{SETTINGS_PATH}?error=failed")) };
    let back = |code: &str| Redirect::to(&format!("{SETTINGS_PATH}?platform={}&error={code}", p.as_str()));
    let Ok(ctx) = ctx else { return back("signed_out") };
    let Some(st) = q.state.as_deref().filter(|s| !s.is_empty()) else { return back("expired") };
    let row: Option<(Uuid, Option<Uuid>, String, Option<String>)> = match sqlx::query_as(
        "DELETE FROM ad_oauth_states WHERE state_hash = $1 AND created_at > now() - interval '15 minutes' RETURNING account_id, user_id, platform, verifier_enc",
    )
    .bind(auth::sha256_hex(st))
    .fetch_optional(&state.db)
    .await
    {
        Ok(r) => r,
        Err(_) => return back("failed"),
    };
    let Some((account_id, user_id, started_platform, verifier_enc)) = row else { return back("expired") };
    if account_id != ctx.account_id || started_platform != p.as_str() || (user_id.is_some() && user_id != ctx.user_id) {
        return back("wrong_account");
    }
    if let Some(e) = q.error.as_deref() {
        return back(if e.contains("denied") || e == "access_denied" { "cancelled" } else { "failed" });
    }
    let Some(code) = q.code.as_deref().or(q.auth_code.as_deref()).filter(|c| !c.is_empty()) else { return back("cancelled") };
    let verifier = verifier_enc.and_then(|v| crate::crypto::decrypt(&state.cfg.secret_key, &v).ok()).unwrap_or_default();
    let tokens = match platforms::exchange(&state, p, code, &verifier).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(platform = p.as_str(), error = e.message(), "ad platform sign-in failed");
            return back("failed");
        }
    };
    if store::save_tokens(&state, account_id, ctx.user_id, p, &tokens).await.is_err() {
        return back("failed");
    }
    Redirect::to(&format!("{SETTINGS_PATH}?connected={}", p.as_str()))
}

pub async fn disconnect(state: &AppState, account_id: Uuid, p: Platform) -> AppResult<Value> {
    let c = store::connection(&state.db, account_id, p).await?.filter(|c| c.status != "disconnected").ok_or_else(|| AppError::NotFound(format!("{} isn't connected", p.label())))?;
    let key = &state.cfg.secret_key;
    let access = c.access_token_enc.as_deref().and_then(|t| crate::crypto::decrypt(key, t).ok());
    let refresh = c.refresh_token_enc.as_deref().and_then(|t| crate::crypto::decrypt(key, t).ok());
    let mut revoked = false;
    if let Some(a) = access.as_deref() {
        match platforms::revoke(state, p, a, refresh.as_deref()).await {
            Ok(()) => revoked = true,
            Err(e) => tracing::info!(platform = p.as_str(), error = e.message(), "couldn't revoke ad platform access; deleting tokens anyway"),
        }
    }
    sqlx::query(
        "UPDATE ad_connections SET status = 'disconnected', access_token_enc = NULL, refresh_token_enc = NULL, token_hint = NULL, token_expires_at = NULL, error = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(c.id)
    .execute(&state.db)
    .await?;
    Ok(json!({"disconnected": true, "revoked": revoked, "note": format!("Syncs to {} wait until you connect it again.", p.label())}))
}

async fn disconnect_h(State(state): State<AppState>, ctx: Ctx, Path(p): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(disconnect(&state, ctx.account_id, platform(&p)?).await?))
}

async fn connected(state: &AppState, account_id: Uuid, p: Platform) -> AppResult<(ConnRow, String)> {
    let c = store::connection(&state.db, account_id, p).await?.filter(|c| c.status != "disconnected").ok_or_else(|| AppError::BadRequest(format!("Connect {} first.", p.label())))?;
    let token = store::access_token(state, &c).await.map_err(perr)?;
    Ok((c, token))
}

pub async fn ad_accounts(state: &AppState, account_id: Uuid, p: Platform) -> AppResult<Value> {
    let (c, token) = connected(state, account_id, p).await?;
    match platforms::list_accounts(state, p, &token).await {
        Ok(a) => Ok(json!({"data": a.accounts, "manual_entry": a.manual_entry})),
        Err(PErr::Auth(m)) => {
            store::mark_needs_reconnect(&state.db, c.id, &m).await?;
            Err(AppError::BadRequest(m))
        }
        Err(e) => Err(perr(e)),
    }
}

async fn accounts_h(State(state): State<AppState>, ctx: Ctx, Path(p): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(ad_accounts(&state, ctx.account_id, platform(&p)?).await?))
}

// ---------- Syncs ----------

#[derive(sqlx::FromRow)]
struct SyncListRow {
    #[sqlx(flatten)]
    sync: SyncRow,
    source_name: Option<String>,
    connection_status: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct SyncFilter {
    pub source_type: Option<String>,
    pub source_id: Option<Uuid>,
}

async fn load_syncs(state: &AppState, account_id: Uuid, id: Option<Uuid>, f: &SyncFilter) -> AppResult<Vec<Value>> {
    let mut qb = QueryBuilder::new(format!(
        "SELECT {SYNC_COLS}, COALESCE(l.name, g.name) AS source_name, c.status AS connection_status FROM ad_audience_syncs s
         LEFT JOIN lists l ON s.source_type = 'list' AND l.id = s.source_id AND l.account_id = s.account_id
         LEFT JOIN segments g ON s.source_type = 'segment' AND g.id = s.source_id AND g.account_id = s.account_id
         LEFT JOIN ad_connections c ON c.id = s.connection_id
         WHERE s.account_id = "
    ));
    qb.push_bind(account_id);
    if let Some(id) = id {
        qb.push(" AND s.id = ").push_bind(id);
    }
    if let Some(t) = f.source_type.as_deref() {
        qb.push(" AND s.source_type = ").push_bind(t.to_string());
    }
    if let Some(sid) = f.source_id {
        qb.push(" AND s.source_id = ").push_bind(sid);
    }
    qb.push(" ORDER BY s.created_at DESC LIMIT 200");
    let rows: Vec<SyncListRow> = qb.build_query_as().fetch_all(&state.db).await?;
    Ok(rows.iter().map(|r| store::sync_json(&r.sync, r.source_name.as_deref(), r.connection_status.as_deref())).collect())
}

pub async fn list_syncs(state: &AppState, account_id: Uuid, f: &SyncFilter) -> AppResult<Value> {
    Ok(json!({"data": load_syncs(state, account_id, None, f).await?}))
}

async fn syncs_h(State(state): State<AppState>, ctx: Ctx, Query(f): Query<SyncFilter>) -> AppResult<Json<Value>> {
    Ok(Json(list_syncs(&state, ctx.account_id, &f).await?))
}

pub async fn get_sync(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let mut v = load_syncs(state, account_id, Some(id), &SyncFilter::default()).await?.pop().ok_or_else(|| AppError::not_found("sync"))?;
    let runs: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'trigger', trigger, 'status', status, 'phase', phase, 'added', added, 'removed', removed, 'invalid', invalid,
                'skipped', skipped, 'error', error, 'started_at', started_at, 'finished_at', finished_at)
         FROM ad_audience_runs WHERE sync_id = $1 AND account_id = $2 ORDER BY started_at DESC LIMIT 30",
    )
    .bind(id)
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    v["runs"] = json!(runs);
    Ok(v)
}

async fn sync_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(get_sync(&state, ctx.account_id, id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    pub platform: String,
    pub ad_account_id: String,
    pub login_account_id: Option<String>,
    pub source_type: String,
    pub source_id: Uuid,
    pub name: Option<String>,
    pub include_unsubscribed: Option<bool>,
    pub match_fields: Option<String>,
    pub interval_hours: Option<i32>,
    pub accept_terms: Option<bool>,
    /// MCP only: without it the tool returns a dry run.
    pub confirm: Option<bool>,
}

fn account_digits(p: Platform, raw: &str) -> AppResult<String> {
    let s = raw.trim();
    let s = if p == Platform::Meta { s.trim_start_matches("act_") } else { s };
    let s: String = if p == Platform::Google { s.chars().filter(|c| *c != '-').collect() } else { s.to_string() };
    if s.is_empty() || s.len() > 24 || !s.chars().all(|c| c.is_ascii_digit()) {
        return Err(AppError::bad("enter the ad account's ID (numbers only)"));
    }
    Ok(s)
}

fn check_match_fields(m: &str) -> AppResult<()> {
    if matches!(m, "contact" | "contact_and_location") { Ok(()) } else { Err(AppError::bad("match_fields must be contact or contact_and_location")) }
}

fn check_interval(h: i32) -> AppResult<()> {
    if (1..=168).contains(&h) { Ok(()) } else { Err(AppError::bad("interval_hours must be between 1 (hourly) and 168 (weekly)")) }
}

async fn source_name(state: &AppState, account_id: Uuid, source_type: &str, id: Uuid) -> AppResult<String> {
    let sql = match source_type {
        "list" => "SELECT name FROM lists WHERE id = $1 AND account_id = $2",
        "segment" => "SELECT name FROM segments WHERE id = $1 AND account_id = $2",
        _ => return Err(AppError::bad("source_type must be list or segment")),
    };
    sqlx::query_scalar(sql).bind(id).bind(account_id).fetch_optional(&state.db).await?.ok_or_else(|| AppError::not_found(source_type))
}

/// How many people in the source could be sent now (before checking each has an identifier the
/// platform takes).
pub async fn estimate(state: &AppState, account_id: Uuid, source_type: &str, source_id: Uuid, include_unsubscribed: bool) -> AppResult<i64> {
    let mut qb = QueryBuilder::new("SELECT count(*)");
    if let Err(msg) = sync::push_source(&mut qb, &state.db, account_id, source_type, source_id, include_unsubscribed).await? {
        return Err(AppError::BadRequest(msg));
    }
    Ok(qb.build_query_scalar().fetch_one(&state.db).await?)
}

pub async fn create_sync(state: &AppState, account_id: Uuid, b: CreateBody, dry_run: bool) -> AppResult<Value> {
    let p = platform(&b.platform)?;
    let ad_account = account_digits(p, &b.ad_account_id)?;
    let login = match b.login_account_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(l) if p == Platform::Google => Some(account_digits(p, l)?),
        Some(_) => return Err(AppError::bad("login_account_id is only for Google Ads manager accounts")),
        None => None,
    };
    let source = source_name(state, account_id, &b.source_type, b.source_id).await?;
    let name = b.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(String::from).unwrap_or_else(|| format!("{source} (DumbMail)"));
    if name.chars().count() > 100 {
        return Err(AppError::bad("name must be 100 characters or fewer"));
    }
    let match_fields = b.match_fields.unwrap_or_else(|| "contact".into());
    check_match_fields(&match_fields)?;
    let interval = b.interval_hours.unwrap_or(6);
    check_interval(interval)?;
    let include_unsub = b.include_unsubscribed.unwrap_or(false);
    let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM ad_audience_syncs WHERE account_id = $1").bind(account_id).fetch_one(&state.db).await?;
    if existing >= MAX_SYNCS {
        return Err(AppError::LimitReached(format!("An account can have up to {MAX_SYNCS} audience syncs. Delete one first.")));
    }
    let dup: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ad_audience_syncs WHERE account_id = $1 AND platform = $2 AND ad_account_id = $3 AND source_type = $4 AND source_id = $5)")
        .bind(account_id)
        .bind(p.as_str())
        .bind(&ad_account)
        .bind(&b.source_type)
        .bind(b.source_id)
        .fetch_one(&state.db)
        .await?;
    if dup {
        return Err(AppError::Conflict(format!("This {} already syncs to that {} ad account.", b.source_type, p.label())));
    }
    let (conn, token) = connected(state, account_id, p).await?;
    // Check the ad account is one this sign-in can reach, and pick up its name.
    let mut account_name = None;
    let listed = platforms::list_accounts(state, p, &token).await.map_err(perr)?;
    if !listed.accounts.is_empty() || !listed.manual_entry {
        match listed.accounts.iter().find(|a| a.id == ad_account) {
            Some(a) if a.manager => return Err(AppError::bad("That's a Google Ads manager account. Pick one of the client accounts under it, and enter the manager's ID as login_account_id.")),
            Some(a) => account_name = a.name.clone(),
            None if login.is_none() => return Err(AppError::BadRequest(format!("{} doesn't list ad account {ad_account} for this sign-in. Pick one from the list.", p.label()))),
            None => {}
        }
    }
    let people = estimate(state, account_id, &b.source_type, b.source_id, include_unsub).await?;
    if dry_run {
        return Ok(json!({
            "dry_run": true,
            "platform": p.as_str(),
            "ad_account_id": ad_account,
            "ad_account_name": account_name,
            "source": {"type": b.source_type, "id": b.source_id, "name": source},
            "name": name,
            "people_now": people,
            "note": format!(
                "Nothing created. This would create the audience \"{name}\" in {} and upload hashed contact details for about {people} people from \"{source}\", then keep it in step every {interval} hours. Call again with confirm=true and accept_terms=true once the user approves.",
                p.label()
            ),
        }));
    }
    if b.accept_terms != Some(true) {
        return Err(AppError::bad(format!(
            "Confirm that these people gave you their contact details directly and that you accept {}'s customer list terms (accept_terms=true).",
            p.label()
        )));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO ad_audience_syncs (id, account_id, connection_id, platform, ad_account_id, ad_account_name, login_account_id, source_type, source_id, name,
             include_unsubscribed, match_fields, interval_hours, terms_accepted_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, now())",
    )
    .bind(id)
    .bind(account_id)
    .bind(conn.id)
    .bind(p.as_str())
    .bind(&ad_account)
    .bind(&account_name)
    .bind(&login)
    .bind(&b.source_type)
    .bind(b.source_id)
    .bind(&name)
    .bind(include_unsub)
    .bind(&match_fields)
    .bind(interval)
    .execute(&state.db)
    .await?;
    sync::start_run(state, id, "created").await?;
    get_sync(state, account_id, id).await
}

async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<CreateBody>) -> AppResult<Json<Value>> {
    Ok(Json(create_sync(&state, ctx.account_id, b, false).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateBody {
    pub status: Option<String>,
    pub interval_hours: Option<i32>,
    pub include_unsubscribed: Option<bool>,
    pub match_fields: Option<String>,
}

pub async fn update_sync(state: &AppState, account_id: Uuid, id: Uuid, b: UpdateBody) -> AppResult<Value> {
    let s = store::account_sync(&state.db, account_id, id).await?.ok_or_else(|| AppError::not_found("sync"))?;
    if let Some(st) = b.status.as_deref() {
        if !matches!(st, "active" | "paused") {
            return Err(AppError::bad("status must be active or paused"));
        }
    }
    if let Some(h) = b.interval_hours {
        check_interval(h)?;
    }
    if let Some(m) = b.match_fields.as_deref() {
        check_match_fields(m)?;
    }
    let resumed = b.status.as_deref() == Some("active") && s.status == "paused";
    let changed_who = b.include_unsubscribed.is_some_and(|v| v != s.include_unsubscribed) || b.match_fields.as_deref().is_some_and(|m| m != s.match_fields);
    sqlx::query(
        "UPDATE ad_audience_syncs SET status = COALESCE($3, status), interval_hours = COALESCE($4, interval_hours), include_unsubscribed = COALESCE($5, include_unsubscribed),
             match_fields = COALESCE($6, match_fields), next_run_at = CASE WHEN $7 THEN now() WHEN $4::int IS NOT NULL THEN COALESCE(last_run_at, now()) + make_interval(hours => $4::int) ELSE next_run_at END,
             updated_at = now()
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .bind(&b.status)
    .bind(b.interval_hours)
    .bind(b.include_unsubscribed)
    .bind(&b.match_fields)
    .bind(resumed || changed_who)
    .execute(&state.db)
    .await?;
    get_sync(state, account_id, id).await
}

async fn update_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<UpdateBody>) -> AppResult<Json<Value>> {
    Ok(Json(update_sync(&state, ctx.account_id, id, b).await?))
}

pub async fn run_now(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let s = store::account_sync(&state.db, account_id, id).await?.ok_or_else(|| AppError::not_found("sync"))?;
    if s.status == "paused" {
        return Err(AppError::bad("This sync is paused. Resume it first."));
    }
    let started = sync::start_run(state, id, "manual").await?;
    let mut v = get_sync(state, account_id, id).await?;
    v["started"] = json!(started.is_some());
    if started.is_none() {
        v["note"] = json!("A sync is already running.");
    }
    Ok(v)
}

async fn run_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(run_now(&state, ctx.account_id, id).await?))
}

#[derive(Deserialize, Default)]
pub struct DeleteQuery {
    pub delete_on_platform: Option<bool>,
}

/// Delete a sync. With `delete_on_platform` the audience is deleted on the ad platform first (ads
/// that use it stop); if that fails, nothing is deleted.
pub async fn delete_sync(state: &AppState, account_id: Uuid, id: Uuid, on_platform: bool) -> AppResult<Value> {
    let s = store::account_sync(&state.db, account_id, id).await?.ok_or_else(|| AppError::not_found("sync"))?;
    let p = s.platform();
    let mut deleted_on_platform = false;
    if on_platform {
        if let Some(aud) = s.external_audience_id.as_deref() {
            let (_, token) = connected(state, account_id, p).await?;
            let t = Target { platform: p, token: &token, ad_account_id: &s.ad_account_id, login_account_id: s.login_account_id.as_deref() };
            match platforms::delete_audience(state, &t, aud).await {
                Ok(()) | Err(PErr::NotFound(_)) => deleted_on_platform = true,
                Err(e) => return Err(perr(e)),
            }
        }
    }
    sqlx::query("DELETE FROM ad_audience_syncs WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(&state.db).await?;
    Ok(json!({"deleted": true, "deleted_on_platform": deleted_on_platform, "audience_id": s.external_audience_id}))
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<DeleteQuery>) -> AppResult<Json<Value>> {
    Ok(Json(delete_sync(&state, ctx.account_id, id, q.delete_on_platform.unwrap_or(false)).await?))
}
