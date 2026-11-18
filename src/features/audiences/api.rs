//! REST routes for ad audiences. The MCP tools call the same functions.

use axum::extract::{Path, Query, State};
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::QueryBuilder;
use uuid::Uuid;

use super::platforms::{self, Kind, PErr, Platform, Target};
use super::store::{self, ConnRow, SyncRow, SYNC_COLS};
use super::sync;
use crate::api::ApiJson;
use crate::auth::{self, Ctx};
use crate::error::{AppError, AppResult};
use crate::features::team::{self, access::Perm};
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
        .route("/api/ad-audiences/settings", get(settings_h).patch(patch_settings_h))
        .route("/api/ad-audiences/syncs", get(syncs_h).post(create_h))
        .route("/api/ad-audiences/syncs/{id}", get(sync_h).patch(update_h).delete(delete_h))
        .route("/api/ad-audiences/syncs/{id}/run", post(run_h))
}

fn platform(s: &str) -> AppResult<Platform> {
    Platform::parse(s).ok_or_else(|| AppError::bad("platform must be meta, google or tiktok"))
}

fn perr(p: Platform, e: PErr) -> AppError {
    let msg = sync::error_words(p, e.code);
    match e.kind {
        Kind::RateLimited | Kind::Transient | Kind::Unclear => AppError::Busy(msg),
        _ => AppError::BadRequest(msg),
    }
}

/// A member signed in to the dashboard (not an API key) who may manage ad audiences (the route's
/// rule checks the permission too), and their email.
async fn browser_manager(state: &AppState, ctx: &Ctx) -> AppResult<(Uuid, String)> {
    let user = ctx.user_id.filter(|_| !ctx.via_api_key).ok_or_else(|| AppError::Forbidden("This needs someone signed in to DumbMail in a browser, not an API key.".into()))?;
    team::access::require(ctx, Perm::AdAudiencesManage)?;
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1").bind(user).fetch_one(&state.db).await?;
    Ok((user, email))
}

// ---------- Connections and settings ----------

fn settings_json(st: &store::Settings) -> Value {
    json!({
        "google_consent": st.google_consent_at.is_some(),
        "google_consent_at": st.google_consent_at,
        "google_consent_by": st.google_consent_by_email,
    })
}

pub async fn connections(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<ConnRow> = sqlx::query_as(&format!("SELECT {} FROM ad_connections WHERE account_id = $1", store::CONN_COLS)).bind(account_id).fetch_all(&state.db).await?;
    let counts: Vec<(String, i64)> = sqlx::query_as("SELECT platform, count(*) FROM ad_audience_syncs WHERE account_id = $1 GROUP BY platform").bind(account_id).fetch_all(&state.db).await?;
    let settings = store::settings(&state.db, account_id).await?;
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
                "token_expires_at": c.filter(|_| connected).and_then(|c| c.token_expires_at),
                // Meta's sign-ins aren't renewed on the server: ask for a reconnect in the last week.
                "expires_soon": c.filter(|_| status == "connected" && *p == Platform::Meta).and_then(|c| c.token_expires_at).is_some_and(|e| e < chrono::Utc::now() + chrono::Duration::days(7)),
                "error": c.filter(|_| status == "needs_reconnect").and_then(|c| c.error_code.as_deref()).map(|code| platforms::words(*p, code)),
                "last_revoke": c.filter(|_| !connected).and_then(|c| c.revoke_status.clone()),
                "manual_account_entry": *p == Platform::Google,
                "verified_live": false,
                "syncs": counts.iter().find(|(k, _)| k == p.as_str()).map(|(_, n)| *n).unwrap_or(0),
            })
        })
        .collect();
    Ok(json!({"data": out, "settings": settings_json(&settings), "connect_url": format!("{}{SETTINGS_PATH}", state.cfg.public_url())}))
}

async fn connections_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(connections(&state, ctx.account_id).await?))
}

/// Begin a sign-in for the signed-in person: remember a random state (hashed) and, for Google, a
/// PKCE verifier; return the platform's sign-in URL for the browser to open. API keys can't start
/// one (the MCP tool returns the settings page link instead).
async fn start_h(State(state): State<AppState>, ctx: Ctx, Path(p): Path<String>) -> AppResult<Json<Value>> {
    let p = platform(&p)?;
    let (user, _) = browser_manager(&state, &ctx).await?;
    if platforms::app(&state.cfg, p).is_none() {
        return Err(AppError::BadRequest(platforms::words(p, "not_available")));
    }
    let st = auth::random_token(32);
    let verifier = auth::random_token(48);
    let verifier_enc = crate::crypto::encrypt(&state.cfg.secret_key, &verifier)?;
    sqlx::query("INSERT INTO ad_oauth_states (state_hash, account_id, user_id, platform, verifier_enc) VALUES ($1, $2, $3, $4, $5)")
        .bind(auth::sha256_hex(&st))
        .bind(ctx.account_id)
        .bind(user)
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

/// The platform sends the browser back here (a public route: the platform's redirect carries no
/// DumbMail headers). The state is consumed only by the same account, person and platform that
/// started it, in one statement, so a different session can't use or burn it, and only while that
/// person may still manage ad audiences. Errors go to the settings page as a short code it
/// explains, never as free text. The query string (code and state) is never logged.
async fn callback_h(State(state): State<AppState>, ctx: Result<Ctx, AppError>, Path(p): Path<String>, Query(q): Query<CallbackQuery>) -> Redirect {
    let Some(p) = Platform::parse(&p) else { return Redirect::to(&format!("{SETTINGS_PATH}?error=failed")) };
    let back = |code: &str| Redirect::to(&format!("{SETTINGS_PATH}?platform={}&error={code}", p.as_str()));
    let Ok(ctx) = ctx else { return back("signed_out") };
    let Some(user) = ctx.user_id.filter(|_| !ctx.via_api_key) else { return back("signed_out") };
    if !ctx.can(Perm::AdAudiencesManage) {
        return back("not_allowed");
    }
    let Some(st) = q.state.as_deref().filter(|s| !s.is_empty()) else { return back("expired") };
    let hash = auth::sha256_hex(st);
    let row: Result<Option<Option<String>>, _> = sqlx::query_scalar(
        "DELETE FROM ad_oauth_states WHERE state_hash = $1 AND account_id = $2 AND user_id = $3 AND platform = $4 AND created_at > now() - interval '15 minutes' RETURNING verifier_enc",
    )
    .bind(&hash)
    .bind(ctx.account_id)
    .bind(user)
    .bind(p.as_str())
    .fetch_optional(&state.db)
    .await;
    let verifier_enc = match row {
        Ok(Some(v)) => v,
        Ok(None) => {
            let someone_elses: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ad_oauth_states WHERE state_hash = $1)").bind(&hash).fetch_one(&state.db).await.unwrap_or(false);
            return back(if someone_elses { "wrong_account" } else { "expired" });
        }
        Err(_) => return back("failed"),
    };
    if let Some(e) = q.error.as_deref() {
        return back(if e.contains("denied") || e == "access_denied" { "cancelled" } else { "failed" });
    }
    let Some(code) = q.code.as_deref().or(q.auth_code.as_deref()).filter(|c| !c.is_empty()) else { return back("cancelled") };
    let verifier = verifier_enc.and_then(|v| crate::crypto::decrypt(&state.cfg.secret_key, &v).ok()).unwrap_or_default();
    let tokens = match platforms::exchange(&state, p, code, &verifier).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(platform = p.as_str(), code = e.code, "ad platform sign-in failed");
            return back("failed");
        }
    };
    if store::save_tokens(&state, ctx.account_id, user, p, &tokens).await.is_err() {
        return back("failed");
    }
    team::audit(&state, &ctx, "ad_audiences.connect", Some(p.as_str()), json!({"platform": p.as_str()})).await;
    Redirect::to(&format!("{SETTINGS_PATH}?connected={}", p.as_str()))
}

pub async fn disconnect(state: &AppState, ctx: &Ctx, p: Platform) -> AppResult<Value> {
    let d = store::disconnect(state, ctx.account_id, p).await?.ok_or_else(|| AppError::NotFound(format!("{} isn't connected", p.label())))?;
    let note = if d.revoked {
        format!("DumbMail's access was revoked on {} and its sign-in deleted. Syncs to {} wait until you connect it again.", p.label(), p.label())
    } else {
        format!(
            "DumbMail deleted its sign-in, but {} didn't confirm it revoked DumbMail's access. Remove DumbMail from your {} account's connected apps to be sure. Syncs wait until you connect again.",
            p.label(),
            p.label()
        )
    };
    team::audit(state, ctx, "ad_audiences.disconnect", Some(p.as_str()), json!({"platform": p.as_str(), "revoked": d.revoked})).await;
    Ok(json!({"disconnected": true, "revoked": d.revoked, "note": note}))
}

async fn disconnect_h(State(state): State<AppState>, ctx: Ctx, Path(p): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(disconnect(&state, &ctx, platform(&p)?).await?))
}

async fn connected(state: &AppState, account_id: Uuid, p: Platform) -> AppResult<(ConnRow, String)> {
    let c = store::connection(&state.db, account_id, p).await?.filter(|c| c.status != "disconnected").ok_or_else(|| AppError::BadRequest(format!("Connect {} first.", p.label())))?;
    let token = store::access_token(state, &c).await.map_err(|e| perr(p, e))?;
    Ok((c, token))
}

pub async fn ad_accounts(state: &AppState, account_id: Uuid, p: Platform) -> AppResult<Value> {
    let (c, token) = connected(state, account_id, p).await?;
    match platforms::list_accounts(state, p, &token).await {
        Ok(a) => Ok(json!({"data": a.accounts, "manual_entry": a.manual_entry})),
        Err(e) if e.kind == Kind::Auth => {
            store::mark_needs_reconnect(&state.db, c.id, c.generation, e.code).await?;
            Err(perr(p, e))
        }
        Err(e) => Err(perr(p, e)),
    }
}

async fn accounts_h(State(state): State<AppState>, ctx: Ctx, Path(p): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(ad_accounts(&state, ctx.account_id, platform(&p)?).await?))
}

pub async fn get_settings(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    Ok(settings_json(&store::settings(&state.db, account_id).await?))
}

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(get_settings(&state, ctx.account_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    pub google_consent: bool,
}

/// The Google consent setting: an owner or admin confirms the account's marketing consent covers
/// advertising use. Turning it off pauses every Google sync.
async fn patch_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<SettingsBody>) -> AppResult<Json<Value>> {
    let (user, email) = browser_manager(&state, &ctx).await?;
    let mut tx = state.db.begin().await?;
    if b.google_consent {
        sqlx::query(
            "INSERT INTO ad_audience_settings (account_id, google_consent_at, google_consent_by, google_consent_by_email) VALUES ($1, now(), $2, $3)
             ON CONFLICT (account_id) DO UPDATE SET google_consent_at = COALESCE(ad_audience_settings.google_consent_at, now()),
                 google_consent_by = CASE WHEN ad_audience_settings.google_consent_at IS NULL THEN EXCLUDED.google_consent_by ELSE ad_audience_settings.google_consent_by END,
                 google_consent_by_email = CASE WHEN ad_audience_settings.google_consent_at IS NULL THEN EXCLUDED.google_consent_by_email ELSE ad_audience_settings.google_consent_by_email END,
                 updated_at = now()",
        )
        .bind(ctx.account_id)
        .bind(user)
        .bind(&email)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO ad_audience_settings (account_id) VALUES ($1)
             ON CONFLICT (account_id) DO UPDATE SET google_consent_at = NULL, google_consent_by = NULL, google_consent_by_email = NULL, updated_at = now()",
        )
        .bind(ctx.account_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE ad_audience_syncs SET status = 'paused', pause_reason = 'google_consent', updated_at = now() WHERE account_id = $1 AND platform = 'google' AND status = 'active'")
            .bind(ctx.account_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(get_settings(&state, ctx.account_id).await?))
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
    let p = Platform::parse(v["platform"].as_str().unwrap_or_default()).unwrap_or(Platform::Meta);
    let runs: Vec<(Value, Option<String>)> = sqlx::query_as(
        "SELECT jsonb_build_object('id', id, 'trigger', trigger, 'mode', mode, 'status', status, 'phase', phase, 'added', added, 'removed', removed,
                'rejected', rejected, 'skipped', skipped, 'started_at', started_at, 'finished_at', finished_at), error_code
         FROM ad_audience_runs WHERE sync_id = $1 AND account_id = $2 ORDER BY started_at DESC LIMIT 30",
    )
    .bind(id)
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let runs: Vec<Value> = runs
        .into_iter()
        .map(|(mut r, code)| {
            r["error_code"] = json!(code);
            r["error"] = json!(code.as_deref().map(|c| sync::error_words(p, c)));
            r
        })
        .collect();
    // Batches still waiting for the platform, by what they're waiting for.
    let open: Vec<(String, String, i64)> = sqlx::query_as("SELECT op, status, coalesce(sum(row_count), 0)::bigint FROM ad_audience_batches WHERE sync_id = $1 AND status IN ('pending', 'accepted') GROUP BY 1, 2")
        .bind(id)
        .fetch_all(&state.db)
        .await?;
    v["runs"] = json!(runs);
    v["waiting"] = json!(open.iter().map(|(op, st, n)| json!({"op": op, "status": st, "count": n})).collect::<Vec<_>>());
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

/// How many people in the source qualify now (before checking each has an identifier the platform takes).
pub async fn estimate(state: &AppState, account_id: Uuid, source_type: &str, source_id: Uuid) -> AppResult<i64> {
    let src = sync::load_source(&state.db, account_id, source_type, source_id).await?;
    if let sync::Source::Gone(code) = src {
        return Err(AppError::BadRequest(sync::error_words(Platform::Meta, code)));
    }
    let mut qb = QueryBuilder::new("SELECT count(*) FROM profiles p WHERE ");
    sync::push_live(&mut qb, account_id, &src);
    Ok(qb.build_query_scalar().fetch_one(&state.db).await?)
}

const GOOGLE_CONSENT_NEEDED: &str = "Google syncs need the account's owner or an admin to confirm, in Settings → Ad audiences, that your marketing consent covers using customer data for advertising.";

pub async fn create_sync(state: &AppState, ctx: &Ctx, b: CreateBody, dry_run: bool) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let p = platform(&b.platform)?;
    let ad_account = account_digits(p, &b.ad_account_id)?;
    let login = match b.login_account_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(l) if p == Platform::Google => Some(account_digits(p, l)?),
        Some(_) => return Err(AppError::bad("login_account_id is only for Google Ads manager accounts")),
        None => None,
    };
    if p == Platform::Google && store::settings(&state.db, account_id).await?.google_consent_at.is_none() {
        return Err(AppError::BadRequest(GOOGLE_CONSENT_NEEDED.into()));
    }
    let source = source_name(state, account_id, &b.source_type, b.source_id).await?;
    let name = b.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(String::from).unwrap_or_else(|| format!("{source} (DumbMail)"));
    if name.chars().count() > 100 {
        return Err(AppError::bad("name must be 100 characters or fewer"));
    }
    let interval = b.interval_hours.unwrap_or(6);
    check_interval(interval)?;
    let (conn, token) = connected(state, account_id, p).await?;
    // Check the ad account is one this sign-in can reach, and pick up its name.
    let mut account_name = None;
    if p == Platform::Google {
        let t = Target { platform: p, token: &token, ad_account_id: &ad_account, login_account_id: login.as_deref() };
        platforms::verify_google_account(state, &t).await.map_err(|e| match e.kind {
            Kind::Permission | Kind::NotFound | Kind::Refused => AppError::BadRequest(format!(
                "This Google sign-in can't manage audiences in Google Ads account {ad_account}{}. Check the customer ID (and the manager account ID, if you reach it through one).",
                login.as_deref().map(|l| format!(" through manager account {l}")).unwrap_or_default()
            )),
            _ => perr(p, e),
        })?;
    } else {
        let listed = platforms::list_accounts(state, p, &token).await.map_err(|e| perr(p, e))?;
        match listed.accounts.iter().find(|a| a.id == ad_account) {
            Some(a) => account_name = a.name.clone(),
            None => return Err(AppError::BadRequest(format!("{} doesn't list ad account {ad_account} for this sign-in. Pick one from the list.", p.label()))),
        }
    }
    let people = estimate(state, account_id, &b.source_type, b.source_id).await?;
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
                "Nothing created. This would create the audience \"{name}\" in {} and upload hashed contact details for about {people} people from \"{source}\" who are subscribed to email, aren't suppressed or on a marketing hold and haven't opted out of ads, then keep it in step every {interval} hours. Call again with confirm=true and accept_terms=true once the user approves.",
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
    let mut tx = state.db.begin().await?;
    // The per-account cap is checked under a per-account lock, so two requests can't both pass it.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('ad_audience_syncs:' || $1::text, 0))").bind(account_id).execute(&mut *tx).await?;
    let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM ad_audience_syncs WHERE account_id = $1").bind(account_id).fetch_one(&mut *tx).await?;
    if existing >= MAX_SYNCS {
        return Err(AppError::LimitReached(format!("An account can have up to {MAX_SYNCS} audience syncs. Delete one first.")));
    }
    let inserted = sqlx::query(
        "INSERT INTO ad_audience_syncs (id, account_id, connection_id, platform, ad_account_id, ad_account_name, login_account_id, source_type, source_id, name,
             interval_hours, terms_accepted_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now()) ON CONFLICT DO NOTHING",
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
    .bind(interval)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(AppError::Conflict(format!("This {} already syncs to that {} ad account.", b.source_type, p.label())));
    }
    tx.commit().await?;
    team::audit(
        state,
        ctx,
        "ad_audiences.sync_create",
        Some(&id.to_string()),
        json!({"platform": p.as_str(), "ad_account_id": ad_account, "source_type": b.source_type, "source_id": b.source_id, "name": name, "interval_hours": interval}),
    )
    .await;
    sync::start_run(state, id, "created").await?;
    get_sync(state, account_id, id).await
}

async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<CreateBody>) -> AppResult<Json<Value>> {
    Ok(Json(create_sync(&state, &ctx, b, false).await?))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateBody {
    pub status: Option<String>,
    pub interval_hours: Option<i32>,
    /// After the platform said the audience no longer exists (or a TikTok create couldn't be
    /// confirmed): make a new audience and upload everyone again. Needs `confirm: true`.
    pub recreate_audience: Option<bool>,
    pub confirm: Option<bool>,
}

/// Take the sync's lease for an API change that must not overlap a batch in flight.
async fn api_lease(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Uuid> {
    let tok: Option<Uuid> = sqlx::query_scalar(
        "UPDATE ad_audience_syncs SET lease_token = $3, lease_until = now() + interval '5 minutes' WHERE id = $1 AND account_id = $2 AND (lease_until IS NULL OR lease_until < now()) RETURNING lease_token",
    )
    .bind(id)
    .bind(account_id)
    .bind(Uuid::new_v4())
    .fetch_optional(&state.db)
    .await?;
    tok.ok_or_else(|| AppError::Busy("This sync is sending a batch to the platform right now. Try again in a minute.".into()))
}

async fn api_release(state: &AppState, id: Uuid, tok: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE ad_audience_syncs SET lease_token = NULL, lease_until = NULL WHERE id = $1 AND lease_token = $2").bind(id).bind(tok).execute(&state.db).await?;
    Ok(())
}

pub async fn update_sync(state: &AppState, ctx: &Ctx, id: Uuid, b: UpdateBody) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let s = store::account_sync(&state.db, account_id, id).await?.ok_or_else(|| AppError::not_found("sync"))?;
    let p = s.platform();
    if s.removing() {
        return Err(AppError::bad("This sync is being deleted: DumbMail is removing everyone it uploaded. It can't be changed."));
    }
    if let Some(st) = b.status.as_deref() {
        if !matches!(st, "active" | "paused") {
            return Err(AppError::bad("status must be active or paused"));
        }
    }
    if let Some(h) = b.interval_hours {
        check_interval(h)?;
    }
    if b.recreate_audience == Some(true) {
        if !matches!(s.pause_reason.as_deref(), Some("audience_missing" | "create_unclear")) {
            return Err(AppError::bad("recreate_audience is only for a sync whose audience the platform says is gone, or whose TikTok audience couldn't be confirmed"));
        }
        if b.confirm != Some(true) {
            return Err(AppError::bad(format!("This makes a new audience on {} and uploads everyone who qualifies. Send confirm=true to go ahead.", p.label())));
        }
        if p == Platform::Google && store::settings(&state.db, account_id).await?.google_consent_at.is_none() {
            return Err(AppError::BadRequest(GOOGLE_CONSENT_NEEDED.into()));
        }
        let tok = api_lease(state, account_id, id).await?;
        let mut tx = state.db.begin().await?;
        sqlx::query("DELETE FROM ad_audience_members WHERE sync_id = $1").bind(id).execute(&mut *tx).await?;
        sqlx::query("UPDATE ad_audience_batches SET status = 'abandoned', result_code = 'recreated', settled_at = now() WHERE sync_id = $1 AND status IN ('pending', 'accepted')").bind(id).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE ad_audience_syncs SET external_audience_id = NULL, audience_created_at = NULL, status = 'active', pause_reason = NULL, last_error_code = NULL,
                 platform_size = NULL, platform_size_upper = NULL, match_rate = NULL, platform_status = NULL, next_run_at = now(), updated_at = now()
             WHERE id = $1 AND lease_token = $2",
        )
        .bind(id)
        .bind(tok)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        api_release(state, id, tok).await?;
        team::audit(state, ctx, "ad_audiences.sync_update", Some(&id.to_string()), json!({"platform": p.as_str(), "recreate_audience": true})).await;
        sync::start_run(state, id, "manual").await?;
        return get_sync(state, account_id, id).await;
    }
    let resumed = b.status.as_deref() == Some("active") && s.status == "paused";
    if resumed {
        match s.pause_reason.as_deref() {
            Some("google_consent") if store::settings(&state.db, account_id).await?.google_consent_at.is_none() => return Err(AppError::BadRequest(GOOGLE_CONSENT_NEEDED.into())),
            Some("audience_missing" | "create_unclear") => return Err(AppError::bad("Make a new audience for this sync instead (recreate_audience=true with confirm=true).")),
            _ => {}
        }
    }
    let paused = b.status.as_deref() == Some("paused") && s.status == "active";
    // The status check in the statement: a delete that started meanwhile wins.
    let n = sqlx::query(
        "UPDATE ad_audience_syncs SET status = COALESCE($3, status), pause_reason = CASE WHEN $6 THEN 'user' WHEN $5 THEN NULL ELSE pause_reason END,
             interval_hours = COALESCE($4, interval_hours),
             next_run_at = CASE WHEN $5 THEN now() WHEN $4::int IS NOT NULL THEN COALESCE(last_run_at, now()) + make_interval(hours => $4::int) ELSE next_run_at END,
             updated_at = now()
         WHERE id = $1 AND account_id = $2 AND status <> 'removing'",
    )
    .bind(id)
    .bind(account_id)
    .bind(&b.status)
    .bind(b.interval_hours)
    .bind(resumed)
    .bind(paused)
    .execute(&state.db)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::bad("This sync is being deleted: DumbMail is removing everyone it uploaded. It can't be changed."));
    }
    if b.status.as_deref().is_some_and(|st| st != s.status) || b.interval_hours.is_some_and(|h| h != s.interval_hours) {
        team::audit(state, ctx, "ad_audiences.sync_update", Some(&id.to_string()), json!({"platform": p.as_str(), "status": b.status, "interval_hours": b.interval_hours})).await;
    }
    get_sync(state, account_id, id).await
}

async fn update_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<UpdateBody>) -> AppResult<Json<Value>> {
    Ok(Json(update_sync(&state, &ctx, id, b).await?))
}

pub async fn run_now(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let s = store::account_sync(&state.db, account_id, id).await?.ok_or_else(|| AppError::not_found("sync"))?;
    if s.status == "paused" && s.pause_reason.as_deref() != Some("tiktok_minimum") {
        return Err(AppError::bad("This sync is paused. Resume it first."));
    }
    let started = sync::start_run(state, id, "manual").await?;
    if started.is_some() {
        team::audit(state, ctx, "ad_audiences.sync_run", Some(&id.to_string()), json!({"platform": s.platform, "removing": s.removing()})).await;
    }
    let mut v = get_sync(state, account_id, id).await?;
    v["started"] = json!(started.is_some());
    if started.is_none() {
        v["note"] = json!("A sync is already running.");
    }
    Ok(v)
}

async fn run_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(run_now(&state, &ctx, id).await?))
}

#[derive(Deserialize, Default)]
pub struct DeleteQuery {
    pub delete_on_platform: Option<bool>,
    pub confirm: Option<bool>,
}

/// What deleting would do, for a dry run.
pub fn delete_note(s: &SyncRow, on_platform: bool) -> String {
    let l = s.platform().label();
    let audience = if on_platform || s.platform() == Platform::Tiktok {
        format!("then delete the audience on {l} (ads that use it stop){}", if on_platform { "" } else { ": TikTok can't keep an audience under 1,000 people" })
    } else {
        format!("and keep the empty audience on {l}")
    };
    format!(
        "Nothing deleted. This would stop syncing \"{}\", remove everyone DumbMail uploaded from the audience on {l}, {audience}. The sync shows as \"Removing\" until {l} confirms every removal, and DumbMail keeps trying until it does. Call again with confirm=true once the user approves.",
        s.name
    )
}

/// Delete a sync. It always withdraws everyone DumbMail uploaded first: the sync becomes
/// `removing`, and a run removes every identifier that may be on the platform. Only once the
/// platform confirmed them all is the audience deleted there (with `delete_on_platform`; always on
/// TikTok, which can't keep fewer than 1,000) and the sync row deleted. Until then it stays
/// `removing` and is retried. Deleting a sync that's already being removed changes only whether
/// the audience is deleted at the end. Refuses while a batch is in flight.
pub async fn delete_sync(state: &AppState, ctx: &Ctx, id: Uuid, on_platform: bool) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let s = store::account_sync(&state.db, account_id, id).await?.ok_or_else(|| AppError::not_found("sync"))?;
    let tok = api_lease(state, account_id, id).await?;
    let mut tx = state.db.begin().await?;
    // Nothing was ever created on the platform and nothing is waiting: nothing to withdraw.
    let nothing_sent: bool = sqlx::query_scalar(
        "SELECT s.external_audience_id IS NULL AND NOT EXISTS (SELECT 1 FROM ad_audience_members m WHERE m.sync_id = s.id)
             AND NOT EXISTS (SELECT 1 FROM ad_audience_batches b WHERE b.sync_id = s.id AND b.status IN ('pending', 'accepted'))
         FROM ad_audience_syncs s WHERE s.id = $1 AND s.lease_token = $2",
    )
    .bind(id)
    .bind(tok)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if nothing_sent {
        sqlx::query("DELETE FROM ad_audience_syncs WHERE id = $1 AND account_id = $2 AND lease_token = $3").bind(id).bind(account_id).bind(tok).execute(&mut *tx).await?;
        tx.commit().await?;
    } else {
        // A run already going keeps its mode; replacing current_run_id stops it at its next step
        // (a batch it has in flight was settled before the lease above was free).
        sqlx::query(
            "UPDATE ad_audience_syncs SET status = 'removing', pause_reason = NULL, delete_audience = $3, current_run_id = NULL, next_run_at = now(), updated_at = now()
             WHERE id = $1 AND lease_token = $2",
        )
        .bind(id)
        .bind(tok)
        .bind(on_platform)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        api_release(state, id, tok).await?;
        sync::start_run(state, id, "deleted").await?;
    }
    if !s.removing() {
        team::audit(state, ctx, "ad_audiences.sync_delete", Some(&id.to_string()), json!({"platform": s.platform, "delete_on_platform": on_platform})).await;
    }
    if nothing_sent {
        return Ok(json!({"deleted": true, "removing": false, "note": "Deleted. Nothing had been uploaded to the platform."}));
    }
    let mut v = get_sync(state, account_id, id).await?;
    v["deleted"] = json!(false);
    v["note"] = v["removing_message"].clone();
    Ok(v)
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<DeleteQuery>) -> AppResult<Json<Value>> {
    let on_platform = q.delete_on_platform.unwrap_or(false);
    if q.confirm != Some(true) {
        let s = store::account_sync(&state.db, ctx.account_id, id).await?.ok_or_else(|| AppError::not_found("sync"))?;
        return Ok(Json(json!({"dry_run": true, "note": delete_note(&s, on_platform)})));
    }
    Ok(Json(delete_sync(&state, &ctx, id, on_platform).await?))
}
