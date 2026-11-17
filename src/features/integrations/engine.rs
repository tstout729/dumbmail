//! Running integrations: processing deliveries, pulls and backfills, test runs and replays.

use std::time::Instant;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::actions::{self, Action, NextRequest, Outcome, Source};
use super::model::{self, Integration};
use super::pull::{self, Vars};
use super::sandbox;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Tries for a webhook delivery before it stays failed.
pub const MAX_ATTEMPTS: i32 = 5;
/// Minutes to wait before each retry.
const BACKOFF_MINUTES: [i64; 4] = [1, 5, 30, 120];
/// Largest body kept with a delivery (webhook bodies are capped at this when received).
pub const BODY_MAX_BYTES: usize = 1024 * 1024;
/// Pages one pull job fetches before handing over to the next job.
const PAGES_PER_JOB: u32 = 5;
/// Pages a backfill may fetch in total.
const BACKFILL_MAX_PAGES: u32 = 5000;

pub const JOB_DELIVERY: &str = "integrations_delivery";
pub const JOB_PULL: &str = "integrations_pull";

fn source_of(i: &Integration) -> Source {
    Source { integration_id: i.id, account_id: i.account_id, name: i.name.clone() }
}

/// Turn a stored body into the script's `payload`: JSON, a form, or the text itself.
pub fn parse_body(content_type: Option<&str>, body: &str) -> Value {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if ct.contains("application/x-www-form-urlencoded") {
        let mut m = Map::new();
        for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
            m.insert(k.into_owned(), Value::String(v.into_owned()));
        }
        return Value::Object(m);
    }
    if body.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
}

#[derive(Default)]
pub struct Input {
    pub source: String,
    pub replay: bool,
    pub content_type: Option<String>,
    pub body: String,
    pub headers: Value,
    pub query: Value,
    pub extra: Map<String, Value>,
}

fn script_input(i: &Integration, inp: &Input, state: &Map<String, Value>) -> Value {
    let mut v = json!({
        "source": inp.source, "replay": inp.replay,
        "payload": parse_body(inp.content_type.as_deref(), &inp.body),
        "body": inp.body, "headers": inp.headers, "query": inp.query,
        "settings": i.settings, "state": state,
        "now": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "integration": {"id": i.id, "name": i.name},
    });
    if let Some(o) = v.as_object_mut() {
        for (k, x) in &inp.extra {
            o.insert(k.clone(), x.clone());
        }
    }
    v
}

/// Run the script and validate its actions. Errors are messages for the delivery log.
async fn translate(script: &str, input: Value, pull: bool) -> Result<(Value, Vec<Action>), String> {
    let raw = sandbox::run(script.to_string(), input).await?;
    let acts = actions::parse(&raw, pull)?;
    Ok((raw, acts))
}

/// Save a run's `set_state` changes into `target` (the live state or a backfill's own).
fn state_patch(acts: &[Action]) -> (Map<String, Value>, Vec<String>) {
    let mut set = Map::new();
    let mut removed = Vec::new();
    for a in acts {
        if let Action::SetState(s) = a {
            for (k, v) in &s.values {
                if v.is_null() {
                    set.remove(k);
                    removed.push(k.clone());
                } else {
                    removed.retain(|r| r != k);
                    set.insert(k.clone(), v.clone());
                }
            }
        }
    }
    (set, removed)
}

async fn save_live_state(state: &AppState, id: Uuid, acts: &[Action]) -> AppResult<()> {
    let (set, removed) = state_patch(acts);
    if set.is_empty() && removed.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE integrations SET state = (state || $2) - $3::text[] WHERE id = $1").bind(id).bind(Value::Object(set)).bind(&removed).execute(&state.db).await?;
    Ok(())
}

fn result_json(o: &Outcome) -> Value {
    json!({"counts": o.counts, "effects": o.effects, "logs": o.logs})
}

fn status_for(n_actions: usize) -> &'static str {
    if n_actions == 0 { "ignored" } else { "ok" }
}

/// Job: process one stored delivery (webhook or replay).
pub async fn process_delivery(state: &AppState, delivery_id: Uuid) -> anyhow::Result<()> {
    let row: Option<(Uuid, String, String, Option<String>, Value, Value, Option<String>, i32, Option<Value>)> =
        sqlx::query_as("SELECT integration_id, source, status, content_type, headers, query, body, attempts, request FROM integration_deliveries WHERE id = $1")
            .bind(delivery_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((integration_id, source, status, content_type, headers, query, body, attempts, request)) = row else { return Ok(()) };
    if !matches!(status.as_str(), "pending" | "error") {
        return Ok(());
    }
    let Some(integ) = model::load_any(state, integration_id).await? else { return Ok(()) };
    if integ.status != "enabled" {
        sqlx::query("UPDATE integration_deliveries SET status = 'ignored', error = 'the integration is turned off', next_attempt_at = NULL, processed_at = now() WHERE id = $1")
            .bind(delivery_id)
            .execute(&state.db)
            .await?;
        return Ok(());
    }
    let started = Instant::now();
    // A replayed pull page runs as a pull again (minus fetching more pages).
    let original = request.as_ref().and_then(|r| r["original_source"].as_str()).map(String::from);
    let replay = source == "replay";
    let shown_source = if replay { original.clone().unwrap_or_else(|| "webhook".into()) } else { source.clone() };
    let mut extra = Map::new();
    if let Some(r) = &request {
        if let Some(resp) = r.get("response") {
            extra.insert("response".into(), resp.clone());
        }
    }
    let input = Input { source: shown_source.clone(), replay, content_type, body: body.unwrap_or_default(), headers, query, extra };
    let script_in = script_input(&integ, &input, &integ.state_map());
    let res = match translate(&integ.script, script_in, shown_source == "pull").await {
        Ok((_, acts)) => {
            let acts: Vec<Action> = acts.into_iter().filter(|a| !matches!(a, Action::NextRequest(_))).collect();
            match actions::apply(state, &source_of(&integ), &acts, &integ.state_map(), false).await {
                Ok(o) => {
                    save_live_state(state, integ.id, &acts).await?;
                    Ok((o, acts.len()))
                }
                Err(e) => Err(e.public_message()),
            }
        }
        Err(e) => Err(e),
    };
    let ms = started.elapsed().as_millis() as i32;
    match res {
        Ok((o, n)) => {
            sqlx::query("UPDATE integration_deliveries SET status = $2, result = $3, error = NULL, duration_ms = $4, attempts = attempts + 1, next_attempt_at = NULL, processed_at = now() WHERE id = $1")
                .bind(delivery_id)
                .bind(status_for(n))
                .bind(result_json(&o))
                .bind(ms)
                .execute(&state.db)
                .await?;
            sqlx::query("UPDATE integrations SET deliveries_ok = deliveries_ok + 1, last_delivery_at = now() WHERE id = $1").bind(integ.id).execute(&state.db).await?;
        }
        Err(msg) => {
            let tries = attempts + 1;
            let retry_at: Option<DateTime<Utc>> = (tries < MAX_ATTEMPTS).then(|| Utc::now() + chrono::Duration::minutes(BACKOFF_MINUTES[(tries as usize - 1).min(BACKOFF_MINUTES.len() - 1)]));
            sqlx::query("UPDATE integration_deliveries SET status = 'error', error = $2, duration_ms = $3, attempts = $4, next_attempt_at = $5, processed_at = now() WHERE id = $1")
                .bind(delivery_id)
                .bind(&msg)
                .bind(ms)
                .bind(tries)
                .bind(retry_at)
                .execute(&state.db)
                .await?;
            let failed_for_good = retry_at.is_none();
            sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + $2, last_delivery_at = now(), last_error = $3, last_error_at = now() WHERE id = $1")
                .bind(integ.id)
                .bind(i64::from(failed_for_good))
                .bind(&msg)
                .execute(&state.db)
                .await?;
            if let Some(at) = retry_at {
                crate::jobs::enqueue(&state.db, JOB_DELIVERY, json!({"delivery_id": delivery_id}), Some(at)).await?;
            }
        }
    }
    Ok(())
}

/// Store a delivery and queue it. Returns its id.
pub async fn store_delivery(state: &AppState, integ: &Integration, source: &str, content_type: Option<&str>, headers: &Value, query: &Value, body: Option<&str>, request: Option<Value>, status: &str, error: Option<&str>, replay_of: Option<Uuid>) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO integration_deliveries (id, integration_id, account_id, source, status, content_type, headers, query, body, request, error, replay_of, processed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, CASE WHEN $5 = 'pending' THEN NULL ELSE now() END)",
    )
    .bind(id)
    .bind(integ.id)
    .bind(integ.account_id)
    .bind(source)
    .bind(status)
    .bind(content_type)
    .bind(headers)
    .bind(query)
    .bind(body)
    .bind(request)
    .bind(error)
    .bind(replay_of)
    .execute(&state.db)
    .await?;
    if status == "pending" {
        crate::jobs::enqueue(&state.db, JOB_DELIVERY, json!({"delivery_id": id}), None).await?;
    }
    Ok(id)
}

// ---------- Test runs ----------

/// Run the script on a sample (or, with `pull`, on the first page fetched now) and say exactly what
/// would be written. Writes nothing. A pull test reuses the cached access token and asks for a new
/// one only when none is cached.
pub async fn test(state: &AppState, integ: &Integration, script: Option<&str>, sample: TestSample) -> AppResult<Value> {
    let script = script.unwrap_or(&integ.script);
    let started = Instant::now();
    let (input, pull_meta) = match sample {
        TestSample::Payload { payload, headers, query, source } => {
            let body = match &payload {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let ct = if payload.is_string() { None } else { Some("application/json".to_string()) };
            (Input { source: source.unwrap_or_else(|| "test".into()), content_type: ct, body, headers: headers.unwrap_or(json!({})), query: query.unwrap_or(json!({})), ..Default::default() }, None)
        }
        TestSample::Pull => {
            let cfg = integ.pull_config().ok_or_else(|| AppError::bad("this integration has no pull settings"))?;
            let creds = integ.credentials(state)?;
            let st = integ.state_map();
            let settings = integ.settings_map();
            let now = Utc::now();
            let vars = Vars { secrets: &creds, state: &st, settings: &settings, now, since: now - chrono::Duration::days(1), redact: false };
            let shown = Vars { redact: true, ..vars };
            let req = pull::build(&cfg, &NextRequest::default(), &Vars { redact: false, ..shown }).map_err(AppError::BadRequest)?;
            let logged = pull::build(&cfg, &NextRequest::default(), &Vars { secrets: &creds, state: &st, settings: &settings, now, since: now - chrono::Duration::days(1), redact: true }).map_err(AppError::BadRequest)?;
            let vars = Vars { secrets: &creds, state: &st, settings: &settings, now, since: now - chrono::Duration::days(1), redact: false };
            let token = pull::oauth_token(state, integ.id, &cfg.auth, &vars).await?;
            let resp = pull::send(&req, token.as_deref()).await?;
            let mut extra = Map::new();
            let response = json!({"status": resp.status, "headers": resp.headers});
            extra.insert("response".into(), response.clone());
            extra.insert("request".into(), logged.log_json());
            extra.insert("page".into(), json!(1));
            extra.insert("backfill".into(), Value::Null);
            let meta = json!({"request": logged.log_json(), "response": response});
            if !(200..300).contains(&resp.status) {
                let detail: String = resp.body.chars().take(500).collect();
                return Ok(json!({"ok": false, "error": format!("the app answered HTTP {}: {detail}", resp.status), "pull": meta, "duration_ms": started.elapsed().as_millis() as u64}));
            }
            (Input { source: "pull".into(), content_type: resp.headers.get("content-type").and_then(|v| v.as_str()).map(String::from), body: resp.body, extra, ..Default::default() }, Some(meta))
        }
    };
    let is_pull = input.source == "pull";
    let script_in = script_input(integ, &input, &integ.state_map());
    let (raw, acts) = match translate(script, script_in, is_pull).await {
        Ok(x) => x,
        Err(e) => return Ok(json!({"ok": false, "error": e, "pull": pull_meta, "duration_ms": started.elapsed().as_millis() as u64})),
    };
    match actions::apply(state, &source_of(integ), &acts, &integ.state_map(), true).await {
        Ok(o) => Ok(json!({
            "ok": true, "actions": raw, "would_write": o.counts, "effects": o.effects, "logs": o.logs,
            "state_after": if o.state.is_empty() { Value::Null } else { Value::Object(o.state) },
            "next_request": o.next_request, "pull": pull_meta, "duration_ms": started.elapsed().as_millis() as u64,
        })),
        Err(e) => Ok(json!({"ok": false, "actions": raw, "error": e.public_message(), "pull": pull_meta, "duration_ms": started.elapsed().as_millis() as u64})),
    }
}

pub enum TestSample {
    Payload { payload: Value, headers: Option<Value>, query: Option<Value>, source: Option<String> },
    Pull,
}

// ---------- Replays ----------

/// Queue failed (or chosen) deliveries to run again with the current script. Returns how many.
pub async fn replay(state: &AppState, integ: &Integration, ids: &[Uuid], all_failed: bool, dry: bool) -> AppResult<Value> {
    if ids.len() > 500 {
        return Err(AppError::bad("replay up to 500 deliveries at a time"));
    }
    let rows: Vec<(Uuid, String, Option<String>, Value, Value, Option<String>, Option<Value>)> = if all_failed {
        sqlx::query_as(
            "SELECT id, source, content_type, headers, query, body, request FROM integration_deliveries
             WHERE integration_id = $1 AND status = 'error' AND next_attempt_at IS NULL AND body IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM integration_deliveries r WHERE r.replay_of = integration_deliveries.id)
             ORDER BY created_at LIMIT 500",
        )
        .bind(integ.id)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as("SELECT id, source, content_type, headers, query, body, request FROM integration_deliveries WHERE integration_id = $1 AND id = ANY($2) AND body IS NOT NULL ORDER BY created_at")
            .bind(integ.id)
            .bind(ids)
            .fetch_all(&state.db)
            .await?
    };
    if dry {
        return Ok(json!({"dry_run": true, "would_replay": rows.len(), "delivery_ids": rows.iter().map(|r| r.0).collect::<Vec<_>>()}));
    }
    let mut new_ids = Vec::new();
    for (id, source, ct, headers, query, body, request) in rows {
        let orig = if source == "replay" { request.as_ref().and_then(|r| r["original_source"].as_str()).unwrap_or("webhook").to_string() } else { source };
        let mut req = request.unwrap_or(json!({}));
        req["original_source"] = json!(orig);
        let nid = store_delivery(state, integ, "replay", ct.as_deref(), &headers, &query, body.as_deref(), Some(req), "pending", None, Some(id)).await?;
        new_ids.push(nid);
    }
    Ok(json!({"replayed": new_ids.len(), "delivery_ids": new_ids}))
}

// ---------- Pulls ----------

fn merge_next(acc: &mut NextRequest, n: NextRequest) {
    if n.url.is_some() {
        acc.url = n.url;
    }
    if n.method.is_some() {
        acc.method = n.method;
    }
    if let Some(q) = n.query {
        let m = acc.query.get_or_insert_with(Map::new);
        for (k, v) in q {
            m.insert(k, v);
        }
    }
    if let Some(h) = n.headers {
        let m = acc.headers.get_or_insert_with(Map::new);
        for (k, v) in h {
            m.insert(k, v);
        }
    }
    if n.body.is_some() {
        acc.body = n.body;
    }
}

/// Start a pull now, or a backfill from `since`. Returns what was started.
pub async fn start_pull(state: &AppState, integ: &Integration, backfill_since: Option<DateTime<Utc>>) -> AppResult<Value> {
    integ.pull_config().ok_or_else(|| AppError::bad("this integration has no pull settings; add a pull first"))?;
    if integ.status != "enabled" {
        return Err(AppError::Conflict("turn the integration on before pulling".into()));
    }
    let missing = integ.missing_credentials();
    if !missing.is_empty() {
        return Err(AppError::bad(format!("add these credentials first: {}", missing.join(", "))));
    }
    match backfill_since {
        Some(since) => {
            if integ.backfill.as_ref().is_some_and(|b| b["status"] == "running") {
                return Err(AppError::Conflict("a backfill is already running; cancel it first".into()));
            }
            if since > Utc::now() {
                return Err(AppError::bad("the backfill start date is in the future"));
            }
            let run_id = Uuid::now_v7();
            let b = json!({"id": run_id, "status": "running", "since": since, "started_at": Utc::now(), "pages": 0, "errors": 0, "state": {}});
            sqlx::query("UPDATE integrations SET backfill = $2 WHERE id = $1").bind(integ.id).bind(&b).execute(&state.db).await?;
            crate::jobs::enqueue(&state.db, JOB_PULL, json!({"integration_id": integ.id, "mode": "backfill", "run_id": run_id, "next": {}, "pages": 0}), None).await?;
            Ok(json!({"started": "backfill", "backfill": b}))
        }
        None => {
            crate::jobs::enqueue(&state.db, JOB_PULL, json!({"integration_id": integ.id, "mode": "live", "next": {}, "pages": 0}), None).await?;
            Ok(json!({"started": "pull"}))
        }
    }
}

pub async fn cancel_backfill(state: &AppState, integ: &Integration) -> AppResult<Value> {
    let n = sqlx::query("UPDATE integrations SET backfill = backfill || jsonb_build_object('status', 'cancelled', 'finished_at', now()) WHERE id = $1 AND backfill->>'status' = 'running'")
        .bind(integ.id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::Conflict("no backfill is running".into()));
    }
    Ok(json!({"cancelled": true}))
}

async fn backfill_update(state: &AppState, id: Uuid, patch: Value) -> anyhow::Result<()> {
    sqlx::query("UPDATE integrations SET backfill = backfill || $2 WHERE id = $1").bind(id).bind(patch).execute(&state.db).await?;
    Ok(())
}

async fn pull_failed(state: &AppState, integ: &Integration, backfill: bool, msg: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE integrations SET last_error = $2, last_error_at = now() WHERE id = $1").bind(integ.id).bind(msg).execute(&state.db).await?;
    if backfill {
        backfill_update(state, integ.id, json!({"status": "failed", "error": msg, "finished_at": Utc::now()})).await?;
    }
    Ok(())
}

/// Job: fetch up to a few pages of a pull (or backfill), running the script on each, then hand
/// over to a new job if the script asks for more.
pub async fn run_pull(state: &AppState, p: &Value) -> anyhow::Result<()> {
    let Some(id) = p["integration_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let backfill = p["mode"] == "backfill";
    let run_id = p["run_id"].as_str().map(String::from);
    let mut next: NextRequest = serde_json::from_value(p["next"].clone()).unwrap_or_default();
    let mut pages = p["pages"].as_u64().unwrap_or(0) as u32;
    for _ in 0..PAGES_PER_JOB {
        let Some(integ) = model::load_any(state, id).await? else { return Ok(()) };
        if integ.status != "enabled" {
            return Ok(());
        }
        let bf = integ.backfill.clone().unwrap_or(Value::Null);
        if backfill && (bf["status"] != "running" || bf["id"].as_str() != run_id.as_deref()) {
            return Ok(()); // cancelled or replaced
        }
        let Some(cfg) = integ.pull_config() else { return Ok(()) };
        let limit = if backfill { BACKFILL_MAX_PAGES } else { cfg.max_pages.unwrap_or(10) };
        let creds = integ.credentials(state).map_err(|e| anyhow::anyhow!(e.public_message()))?;
        let settings = integ.settings_map();
        let run_state = if backfill { bf["state"].as_object().cloned().unwrap_or_default() } else { integ.state_map() };
        let now = Utc::now();
        let since = if backfill { bf["since"].as_str().and_then(actions::parse_time).unwrap_or(now) } else { now - chrono::Duration::days(1) };
        let vars = Vars { secrets: &creds, state: &run_state, settings: &settings, now, since, redact: false };
        let shown = Vars { secrets: &creds, state: &run_state, settings: &settings, now, since, redact: true };
        let (req, logged) = match (pull::build(&cfg, &next, &vars), pull::build(&cfg, &next, &shown)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return pull_failed(state, &integ, backfill, &e).await,
        };
        let token = match pull::oauth_token(state, integ.id, &cfg.auth, &vars).await {
            Ok(t) => t,
            Err(e) => return pull_failed(state, &integ, backfill, &format!("couldn't get an access token: {}", e.public_message())).await,
        };
        let started = Instant::now();
        let resp = match pull::send(&req, token.as_deref()).await {
            Ok(r) => r,
            Err(e) => return pull_failed(state, &integ, backfill, &e.public_message()).await,
        };
        let response = json!({"status": resp.status, "headers": resp.headers});
        let mut logged_req = logged.log_json();
        logged_req["response"] = response.clone();
        let body = (resp.body.len() <= BODY_MAX_BYTES).then_some(resp.body.as_str());
        let ct = resp.headers.get("content-type").and_then(|v| v.as_str()).map(String::from);
        if !(200..300).contains(&resp.status) {
            if token.is_some() && matches!(resp.status, 401 | 403) {
                pull::drop_token(state, integ.id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
            }
            let detail: String = resp.body.chars().take(300).collect();
            let msg = format!("the app answered HTTP {}: {detail}", resp.status);
            store_delivery(state, &integ, "pull", ct.as_deref(), &json!({}), &json!({}), body, Some(logged_req), "error", Some(&msg), None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
            sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + 1 WHERE id = $1").bind(integ.id).execute(&state.db).await?;
            return pull_failed(state, &integ, backfill, &msg).await;
        }
        let mut extra = Map::new();
        extra.insert("response".into(), response);
        extra.insert("request".into(), logged.log_json());
        extra.insert("page".into(), json!(pages + 1));
        extra.insert("backfill".into(), if backfill { json!({"since": bf["since"]}) } else { Value::Null });
        let input = Input { source: "pull".into(), content_type: ct.clone(), body: resp.body.clone(), extra, ..Default::default() };
        let res = match translate(&integ.script, script_input(&integ, &input, &run_state), true).await {
            Ok((_, acts)) => actions::apply(state, &source_of(&integ), &acts, &run_state, false).await.map(|o| (o, acts)).map_err(|e| e.public_message()),
            Err(e) => Err(e),
        };
        let ms = started.elapsed().as_millis() as i32;
        let delivery = store_delivery(state, &integ, "pull", ct.as_deref(), &json!({}), &json!({}), body, Some(logged_req), "ok", None, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        pages += 1;
        match res {
            Ok((o, acts)) => {
                sqlx::query("UPDATE integration_deliveries SET status = $2, result = $3, duration_ms = $4, attempts = 1 WHERE id = $1")
                    .bind(delivery)
                    .bind(status_for(acts.len()))
                    .bind(result_json(&o))
                    .bind(ms)
                    .execute(&state.db)
                    .await?;
                sqlx::query("UPDATE integrations SET deliveries_ok = deliveries_ok + 1, last_delivery_at = now() WHERE id = $1").bind(integ.id).execute(&state.db).await?;
                if backfill {
                    let (set, removed) = state_patch(&acts);
                    let mut st = run_state.clone();
                    for k in removed {
                        st.remove(&k);
                    }
                    st.extend(set);
                    backfill_update(state, integ.id, json!({"pages": pages, "state": st})).await?;
                } else {
                    save_live_state(state, integ.id, &acts).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                }
                match o.next_request {
                    Some(n) if pages < limit => merge_next(&mut next, n),
                    Some(_) => {
                        if backfill {
                            backfill_update(state, integ.id, json!({"status": "done", "finished_at": Utc::now(), "note": format!("stopped at the {limit}-page limit")})).await?;
                        }
                        return Ok(());
                    }
                    None => {
                        if backfill {
                            backfill_update(state, integ.id, json!({"status": "done", "finished_at": Utc::now()})).await?;
                        } else {
                            sqlx::query("UPDATE integrations SET last_error = NULL WHERE id = $1 AND last_error_at < now() - interval '1 second'").bind(integ.id).execute(&state.db).await?;
                        }
                        return Ok(());
                    }
                }
            }
            Err(msg) => {
                sqlx::query("UPDATE integration_deliveries SET status = 'error', error = $2, duration_ms = $3, attempts = 1 WHERE id = $1").bind(delivery).bind(&msg).bind(ms).execute(&state.db).await?;
                sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + 1 WHERE id = $1").bind(integ.id).execute(&state.db).await?;
                return pull_failed(state, &integ, backfill, &msg).await;
            }
        }
    }
    let mut payload = p.clone();
    payload["next"] = serde_json::to_value(&next)?;
    payload["pages"] = json!(pages);
    crate::jobs::enqueue(&state.db, JOB_PULL, payload, None).await?;
    Ok(())
}

/// Scheduler: start pulls that are due, at most once per interval each.
pub async fn schedule_pulls(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<(Uuid, Value)> = sqlx::query_as(
        "UPDATE integrations SET pull_next_at = now() + make_interval(mins => (pull->>'interval_minutes')::int)
         WHERE pull_next_at <= now() AND status = 'enabled' AND pull ? 'interval_minutes'
         RETURNING id, pull",
    )
    .fetch_all(&state.db)
    .await?;
    for (id, _) in due {
        let busy: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = $1 AND payload->>'integration_id' = $2 AND payload->>'mode' = 'live' AND run_at <> 'infinity')")
            .bind(JOB_PULL)
            .bind(id.to_string())
            .fetch_one(&state.db)
            .await?;
        if !busy {
            crate::jobs::enqueue(&state.db, JOB_PULL, json!({"integration_id": id, "mode": "live", "next": {}, "pages": 0}), None).await?;
        }
    }
    Ok(())
}

/// Scheduler: deliveries are kept 30 days.
pub async fn prune(state: &AppState) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM integration_deliveries WHERE id IN (SELECT id FROM integration_deliveries WHERE created_at < now() - interval '30 days' LIMIT 5000)").execute(&state.db).await?;
    Ok(())
}
