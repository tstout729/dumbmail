//! Running integrations: processing deliveries, pulls and backfills, test runs and replays.
//!
//! - Admission: a delivery is stored and its job queued in one transaction, within per-integration
//!   and per-account limits that count everything still to run (new, retrying, replayed).
//! - Fairness: each script run needs a [`Slots`] slot (2 per account, 8 in all by default). A job
//!   that can't get one is put back with a short delay instead of waiting on a worker.
//! - Idempotence: each delivery keeps which actions were written (index → fingerprint); a retry or
//!   replay skips them, and events without a `unique_id` get one from the original delivery.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rand::Rng;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::actions::{self, Action, NextRequest, Outcome, Source};
use super::model::{self, Integration};
use super::pull::{self, TokenFor, Vars};
use super::sandbox::{self, Slot, Slots};
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
/// Pages one backfill run may fetch; past it the backfill is "capped" and can be resumed.
pub const BACKFILL_MAX_PAGES: u32 = 5000;
/// Deliveries still to run (pending or waiting to retry) one integration may have.
pub const MAX_OPEN_PER_INTEGRATION: i64 = 5000;
/// The same across all of an account's integrations.
pub const MAX_OPEN_PER_ACCOUNT: i64 = 20_000;
/// Stored payload bytes per account; past it the oldest payloads are dropped (their log lines stay).
pub const PAYLOAD_BUDGET_BYTES: i64 = 256 * 1024 * 1024;
/// Deliveries and their payloads are kept this long.
pub const RETENTION_DAYS: i64 = 30;

pub const JOB_DELIVERY: &str = "integrations_delivery";
pub const JOB_PULL: &str = "integrations_pull";

fn source_of(i: &Integration, delivery: Option<Uuid>) -> Source {
    Source { integration_id: i.id, account_id: i.account_id, name: i.name.clone(), contacts_opted_in: i.contacts_opted_in, delivery }
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
async fn translate(slot: &Slot, script: &str, input: Value, pull: bool) -> Result<(Value, Vec<Action>), String> {
    let raw = sandbox::run(slot, script, &input).await?;
    let acts = actions::parse(&raw, pull)?;
    Ok((raw, acts))
}

fn raw_list(raw: &Value) -> Vec<Value> {
    raw.as_array().cloned().unwrap_or_default()
}

/// A job that couldn't get a script slot goes back in the queue a few seconds later.
async fn requeue(state: &AppState, kind: &str, payload: Value) -> anyhow::Result<()> {
    let wait = rand::thread_rng().gen_range(2_000..8_000);
    crate::jobs::enqueue(&state.db, kind, payload, Some(Utc::now() + chrono::Duration::milliseconds(wait))).await?;
    Ok(())
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
    type Row = (Uuid, Uuid, String, String, Option<String>, Value, Value, Option<String>, i32, Option<Value>, Option<Uuid>, Value);
    let row: Option<Row> = sqlx::query_as(
        "SELECT integration_id, account_id, source, status, content_type, headers, query, body, attempts, request, replay_of, applied FROM integration_deliveries WHERE id = $1",
    )
    .bind(delivery_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((integration_id, account_id, source, status, content_type, headers, query, body, attempts, request, replay_of, applied)) = row else { return Ok(()) };
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
    let Some(slot) = Slots::try_take(account_id) else {
        return requeue(state, JOB_DELIVERY, json!({"delivery_id": delivery_id})).await;
    };
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
    let skip = applied.as_object().cloned().unwrap_or_default();
    // Events without a unique_id take theirs from the original delivery, so a replay can't repeat them.
    let src = source_of(&integ, Some(replay_of.unwrap_or(delivery_id)));
    let res = match translate(&slot, &integ.script, script_in, shown_source == "pull").await {
        Ok((raw, acts)) => {
            let keep: Vec<(Action, Value)> = acts.into_iter().zip(raw_list(&raw)).filter(|(a, _)| !matches!(a, Action::NextRequest(_))).collect();
            let (acts, raws): (Vec<Action>, Vec<Value>) = keep.into_iter().unzip();
            let (o, err) = actions::apply(state, &src, &acts, &raws, &skip, &integ.state_map(), false).await;
            save_applied(state, delivery_id, &o).await?;
            match err {
                None => {
                    save_live_state(state, integ.id, &acts).await?;
                    Ok((o, acts.len()))
                }
                Some(e) => Err(e.public_message()),
            }
        }
        Err(e) => Err(e),
    };
    drop(slot);
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
            let msg = integ.redact(state, &msg).await;
            let tries = attempts + 1;
            let retry_at: Option<DateTime<Utc>> = (tries < MAX_ATTEMPTS).then(|| Utc::now() + chrono::Duration::minutes(BACKOFF_MINUTES[(tries as usize - 1).min(BACKOFF_MINUTES.len() - 1)]));
            let mut tx = state.db.begin().await?;
            sqlx::query("UPDATE integration_deliveries SET status = 'error', error = $2, duration_ms = $3, attempts = $4, next_attempt_at = $5, processed_at = now() WHERE id = $1")
                .bind(delivery_id)
                .bind(&msg)
                .bind(ms)
                .bind(tries)
                .bind(retry_at)
                .execute(&mut *tx)
                .await?;
            let failed_for_good = retry_at.is_none();
            sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + $2, last_delivery_at = now(), last_error = $3, last_error_at = now() WHERE id = $1")
                .bind(integ.id)
                .bind(i64::from(failed_for_good))
                .bind(&msg)
                .execute(&mut *tx)
                .await?;
            if let Some(at) = retry_at {
                crate::jobs::enqueue(&mut *tx, JOB_DELIVERY, json!({"delivery_id": delivery_id}), Some(at)).await?;
            }
            tx.commit().await?;
        }
    }
    Ok(())
}

async fn save_applied(state: &AppState, delivery_id: Uuid, o: &Outcome) -> anyhow::Result<()> {
    if !o.applied.is_empty() {
        sqlx::query("UPDATE integration_deliveries SET applied = applied || $2 WHERE id = $1").bind(delivery_id).bind(Value::Object(o.applied.clone())).execute(&state.db).await?;
    }
    Ok(())
}

/// A delivery to store.
pub struct NewDelivery<'a> {
    pub source: &'a str,
    pub content_type: Option<&'a str>,
    pub headers: &'a Value,
    pub query: &'a Value,
    pub body: Option<&'a str>,
    pub request: Option<Value>,
    pub status: &'a str,
    pub error: Option<&'a str>,
    pub replay_of: Option<Uuid>,
    pub message_id: Option<String>,
    pub applied: Option<Value>,
}

impl<'a> NewDelivery<'a> {
    pub fn log(source: &'a str, status: &'a str, error: Option<&'a str>) -> Self {
        static EMPTY: once_cell::sync::Lazy<Value> = once_cell::sync::Lazy::new(|| json!({}));
        NewDelivery { source, content_type: None, headers: &EMPTY, query: &EMPTY, body: None, request: None, status, error, replay_of: None, message_id: None, applied: None }
    }
}

/// Why a delivery wasn't stored.
#[derive(Debug)]
pub enum Refused {
    /// The app already sent a message with this id; it was acknowledged and ignored.
    Duplicate(Uuid),
    /// Too many deliveries are waiting to run.
    Busy(String),
    Error(AppError),
}

impl From<sqlx::Error> for Refused {
    fn from(e: sqlx::Error) -> Self {
        Refused::Error(e.into())
    }
}

/// Store a delivery and, when it is pending, queue its job, in one transaction. Pending deliveries
/// count against the per-integration and per-account limits on work still to run.
pub async fn store_delivery(state: &AppState, integ: &Integration, d: NewDelivery<'_>) -> Result<Uuid, Refused> {
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    if d.status == "pending" {
        // One admission at a time per account, so two requests can't both slip under the limit.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7021))").bind(integ.account_id.to_string()).execute(&mut *tx).await?;
        let (mine, all): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE integration_id = $1), count(*) FROM integration_deliveries
             WHERE account_id = $2 AND (status = 'pending' OR (status = 'error' AND next_attempt_at IS NOT NULL))",
        )
        .bind(integ.id)
        .bind(integ.account_id)
        .fetch_one(&mut *tx)
        .await?;
        if mine >= MAX_OPEN_PER_INTEGRATION || all >= MAX_OPEN_PER_ACCOUNT {
            return Err(Refused::Busy("too many deliveries are waiting to be processed; retry later".into()));
        }
    }
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO integration_deliveries (id, integration_id, account_id, source, status, content_type, headers, query, body, body_bytes, request, error, replay_of, message_id, applied, processed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, COALESCE(octet_length($9), 0), $10, $11, $12, $13, COALESCE($14, '{}'), CASE WHEN $5 = 'pending' THEN NULL ELSE now() END)
         ON CONFLICT (integration_id, message_id) WHERE message_id IS NOT NULL DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(integ.id)
    .bind(integ.account_id)
    .bind(d.source)
    .bind(d.status)
    .bind(d.content_type)
    .bind(d.headers)
    .bind(d.query)
    .bind(d.body)
    .bind(d.request)
    .bind(d.error)
    .bind(d.replay_of)
    .bind(&d.message_id)
    .bind(d.applied)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        let first: Uuid = sqlx::query_scalar("SELECT id FROM integration_deliveries WHERE integration_id = $1 AND message_id = $2").bind(integ.id).bind(&d.message_id).fetch_one(&mut *tx).await?;
        return Err(Refused::Duplicate(first));
    }
    if d.status == "pending" {
        crate::jobs::enqueue(&mut *tx, JOB_DELIVERY, json!({"delivery_id": id}), None).await?;
    }
    tx.commit().await?;
    Ok(id)
}

/// Store a finished pull page's delivery (never deduplicated, never queued).
async fn store_log(state: &AppState, integ: &Integration, d: NewDelivery<'_>) -> anyhow::Result<Uuid> {
    match store_delivery(state, integ, d).await {
        Ok(id) => Ok(id),
        Err(Refused::Error(e)) => Err(anyhow::anyhow!(e.public_message())),
        Err(other) => Err(anyhow::anyhow!("couldn't store the delivery: {other:?}")),
    }
}

// ---------- Test runs ----------

/// Run the script on a sample (or, with `pull`, on the first page fetched now) and say exactly what
/// would be written. Writes nothing. A pull test reuses the cached access token and asks for a new
/// one only when none is cached.
pub async fn test(state: &AppState, integ: &Integration, script: Option<&str>, sample: TestSample) -> AppResult<Value> {
    let script = script.unwrap_or(&integ.script);
    let started = Instant::now();
    let slot = Slots::take_within(integ.account_id, Duration::from_secs(5))
        .await
        .ok_or_else(|| AppError::Conflict("too many scripts are running for this account right now; try again in a few seconds".into()))?;
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
            let shown = Vars { secrets: &creds, state: &st, settings: &settings, now, since: now - chrono::Duration::days(1), redact: true };
            let req = pull::build(&cfg, &NextRequest::default(), &vars).map_err(AppError::BadRequest)?;
            let logged = pull::build(&cfg, &NextRequest::default(), &shown).map_err(AppError::BadRequest)?;
            let origin = cfg.origin();
            let who = TokenFor { integration_id: integ.id, auth_version: integ.auth_version, origin: &origin };
            let fetched = async {
                let token = pull::oauth_token(state, &who, &cfg.auth, &vars).await?;
                pull::send(&req, token.as_deref()).await
            }
            .await;
            let resp = match fetched {
                Ok(r) => r,
                Err(e) => {
                    let msg = integ.redact(state, &e.public_message()).await;
                    return Ok(json!({"ok": false, "error": msg, "pull": {"request": logged.log_json()}, "duration_ms": started.elapsed().as_millis() as u64}));
                }
            };
            let mut extra = Map::new();
            let response = json!({"status": resp.status, "headers": resp.headers});
            extra.insert("response".into(), response.clone());
            extra.insert("request".into(), logged.log_json());
            extra.insert("page".into(), json!(1));
            extra.insert("backfill".into(), Value::Null);
            let meta = json!({"request": logged.log_json(), "response": response});
            if !(200..300).contains(&resp.status) {
                let msg = format!("{} answered HTTP {} ({})", req.place(), resp.status, pull::status_class(resp.status));
                return Ok(json!({"ok": false, "error": integ.redact(state, &msg).await, "pull": meta, "duration_ms": started.elapsed().as_millis() as u64}));
            }
            (Input { source: "pull".into(), content_type: resp.headers.get("content-type").and_then(|v| v.as_str()).map(String::from), body: resp.body, extra, ..Default::default() }, Some(meta))
        }
    };
    let is_pull = input.source == "pull";
    let script_in = script_input(integ, &input, &integ.state_map());
    let (raw, acts) = match translate(&slot, script, script_in, is_pull).await {
        Ok(x) => x,
        Err(e) => return Ok(json!({"ok": false, "error": e, "pull": pull_meta, "duration_ms": started.elapsed().as_millis() as u64})),
    };
    drop(slot);
    let (o, err) = actions::apply(state, &source_of(integ, None), &acts, &raw_list(&raw), &Map::new(), &integ.state_map(), true).await;
    match err {
        None => Ok(json!({
            "ok": true, "actions": raw, "would_write": o.counts, "effects": o.effects, "logs": o.logs,
            "state_after": if o.state.is_empty() { Value::Null } else { Value::Object(o.state) },
            "next_request": o.next_request, "pull": pull_meta, "duration_ms": started.elapsed().as_millis() as u64,
        })),
        Some(e) => Ok(json!({"ok": false, "actions": raw, "error": e.public_message(), "pull": pull_meta, "duration_ms": started.elapsed().as_millis() as u64})),
    }
}

pub enum TestSample {
    Payload { payload: Value, headers: Option<Value>, query: Option<Value>, source: Option<String> },
    Pull,
}

// ---------- Replays ----------

/// Queue failed (or chosen) deliveries to run again with the current script. Actions the original
/// already wrote are skipped, and its events can't be recorded twice. Counts against the same
/// limits as new deliveries. Returns how many.
pub async fn replay(state: &AppState, integ: &Integration, ids: &[Uuid], all_failed: bool, dry: bool) -> AppResult<Value> {
    if ids.len() > 500 {
        return Err(AppError::bad("replay up to 500 deliveries at a time"));
    }
    type Row = (Uuid, String, Option<String>, Value, Value, Option<String>, Option<Value>, Option<Uuid>, Value);
    let rows: Vec<Row> = if all_failed {
        sqlx::query_as(
            "SELECT id, source, content_type, headers, query, body, request, replay_of, applied FROM integration_deliveries
             WHERE integration_id = $1 AND status = 'error' AND next_attempt_at IS NULL AND body IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM integration_deliveries r WHERE r.replay_of = integration_deliveries.id AND r.status IN ('pending', 'ok', 'ignored'))
             ORDER BY created_at LIMIT 500",
        )
        .bind(integ.id)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as("SELECT id, source, content_type, headers, query, body, request, replay_of, applied FROM integration_deliveries WHERE integration_id = $1 AND id = ANY($2) AND body IS NOT NULL ORDER BY created_at")
            .bind(integ.id)
            .bind(ids)
            .fetch_all(&state.db)
            .await?
    };
    if dry {
        return Ok(json!({"dry_run": true, "would_replay": rows.len(), "delivery_ids": rows.iter().map(|r| r.0).collect::<Vec<_>>()}));
    }
    let mut new_ids = Vec::new();
    for (id, source, ct, headers, query, body, request, replay_of, applied) in rows {
        let orig = if source == "replay" { request.as_ref().and_then(|r| r["original_source"].as_str()).unwrap_or("webhook").to_string() } else { source };
        let mut req = request.unwrap_or(json!({}));
        req["original_source"] = json!(orig);
        // Point at the original delivery, and carry over what it (and earlier replays) wrote.
        let root = replay_of.unwrap_or(id);
        let written: Value = sqlx::query_scalar("SELECT COALESCE(jsonb_object_agg(k, v), '{}') FROM integration_deliveries d, jsonb_each(d.applied) AS e(k, v) WHERE d.integration_id = $1 AND (d.id = $2 OR d.replay_of = $2)")
            .bind(integ.id)
            .bind(root)
            .fetch_one(&state.db)
            .await?;
        let mut all = applied.as_object().cloned().unwrap_or_default();
        if let Some(o) = written.as_object() {
            all.extend(o.clone());
        }
        let d = NewDelivery { source: "replay", content_type: ct.as_deref(), headers: &headers, query: &query, body: body.as_deref(), request: Some(req), status: "pending", error: None, replay_of: Some(root), message_id: None, applied: Some(Value::Object(all)) };
        match store_delivery(state, integ, d).await {
            Ok(nid) => new_ids.push(nid),
            Err(Refused::Busy(msg)) => {
                return Ok(json!({"replayed": new_ids.len(), "delivery_ids": new_ids, "stopped": msg}));
            }
            Err(Refused::Duplicate(_)) => {}
            Err(Refused::Error(e)) => return Err(e),
        }
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

/// Start a pull now, or a backfill from `since`, or (`resume`) continue a backfill that stopped at
/// its page limit or failed, from where it got to. Returns what was started.
pub async fn start_pull(state: &AppState, integ: &Integration, backfill_since: Option<DateTime<Utc>>, resume: bool) -> AppResult<Value> {
    integ.pull_config().ok_or_else(|| AppError::bad("this integration has no pull settings; add a pull first"))?;
    if integ.status != "enabled" {
        return Err(AppError::Conflict("turn the integration on before pulling".into()));
    }
    let missing = integ.missing_credentials();
    if !missing.is_empty() {
        return Err(AppError::bad(format!("add these credentials first: {}", missing.join(", "))));
    }
    if resume {
        let b = integ.backfill.clone().unwrap_or(Value::Null);
        if !matches!(b["status"].as_str(), Some("capped" | "failed")) {
            return Err(AppError::Conflict("there's no stopped backfill to resume".into()));
        }
        let run_id = Uuid::now_v7();
        let patch = json!({"id": run_id, "status": "running", "resumed_at": Utc::now(), "error": null, "finished_at": null, "note": null});
        sqlx::query("UPDATE integrations SET backfill = backfill || $2 WHERE id = $1").bind(integ.id).bind(&patch).execute(&state.db).await?;
        crate::jobs::enqueue(&state.db, JOB_PULL, json!({"integration_id": integ.id, "mode": "backfill", "run_id": run_id, "next": b["next"].clone(), "pages": 0}), None).await?;
        return Ok(json!({"started": "backfill", "resumed": true, "backfill": {"id": run_id, "pages": b["pages"]}}));
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
            let b = json!({"id": run_id, "status": "running", "since": since, "started_at": Utc::now(), "pages": 0, "errors": 0, "state": {}, "next": {}});
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

/// Update a backfill's progress, only while it is still the same run (a replaced or cancelled
/// backfill is never overwritten by an old page). Returns false when the run is no longer current.
async fn backfill_update(state: &AppState, id: Uuid, run_id: &str, patch: Value) -> anyhow::Result<bool> {
    let n = sqlx::query("UPDATE integrations SET backfill = backfill || $2 WHERE id = $1 AND backfill->>'id' = $3 AND backfill->>'status' = 'running'")
        .bind(id)
        .bind(patch)
        .bind(run_id)
        .execute(&state.db)
        .await?
        .rows_affected();
    Ok(n > 0)
}

async fn pull_failed(state: &AppState, integ: &Integration, run_id: Option<&str>, msg: &str) -> anyhow::Result<()> {
    let msg = integ.redact(state, msg).await;
    sqlx::query("UPDATE integrations SET last_error = $2, last_error_at = now() WHERE id = $1").bind(integ.id).bind(&msg).execute(&state.db).await?;
    if let Some(run) = run_id {
        backfill_update(state, integ.id, run, json!({"status": "failed", "error": msg, "finished_at": Utc::now()})).await?;
    }
    Ok(())
}

/// Job: fetch up to a few pages of a pull (or backfill), running the script on each, then hand
/// over to a new job if the script asks for more.
pub async fn run_pull(state: &AppState, p: &Value) -> anyhow::Result<()> {
    let Some(id) = p["integration_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let backfill = p["mode"] == "backfill";
    let run_id = p["run_id"].as_str().map(String::from);
    let run = if backfill { run_id.as_deref() } else { None };
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
        // Scheduled pulls wait quietly until the credentials are saved (the page asks for them).
        if !backfill && pages == 0 && !integ.missing_credentials().is_empty() {
            return Ok(());
        }
        let Some(slot) = Slots::try_take(integ.account_id) else {
            let mut payload = p.clone();
            payload["next"] = serde_json::to_value(&next)?;
            payload["pages"] = json!(pages);
            return requeue(state, JOB_PULL, payload).await;
        };
        let limit = if backfill { cfg.backfill_max_pages.unwrap_or(BACKFILL_MAX_PAGES) } else { cfg.max_pages.unwrap_or(10) };
        let creds = integ.credentials(state).map_err(|e| anyhow::anyhow!(e.public_message()))?;
        let settings = integ.settings_map();
        let run_state = if backfill { bf["state"].as_object().cloned().unwrap_or_default() } else { integ.state_map() };
        let now = Utc::now();
        let since = if backfill { bf["since"].as_str().and_then(actions::parse_time).unwrap_or(now) } else { now - chrono::Duration::days(1) };
        let vars = Vars { secrets: &creds, state: &run_state, settings: &settings, now, since, redact: false };
        let shown = Vars { secrets: &creds, state: &run_state, settings: &settings, now, since, redact: true };
        let (req, logged) = match (pull::build(&cfg, &next, &vars), pull::build(&cfg, &next, &shown)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return pull_failed(state, &integ, run, &e).await,
        };
        let origin = cfg.origin();
        let who = TokenFor { integration_id: integ.id, auth_version: integ.auth_version, origin: &origin };
        let token = match pull::oauth_token(state, &who, &cfg.auth, &vars).await {
            Ok(t) => t,
            Err(e) => return pull_failed(state, &integ, run, &format!("couldn't get an access token: {}", e.public_message())).await,
        };
        let started = Instant::now();
        let resp = match pull::send(&req, token.as_deref()).await {
            Ok(r) => r,
            Err(e) => return pull_failed(state, &integ, run, &e.public_message()).await,
        };
        let response = json!({"status": resp.status, "headers": resp.headers});
        let mut logged_req = logged.log_json();
        logged_req["response"] = response.clone();
        let ct = resp.headers.get("content-type").and_then(|v| v.as_str()).map(String::from);
        if !(200..300).contains(&resp.status) {
            if token.is_some() && matches!(resp.status, 401 | 403) {
                pull::drop_token(state, integ.id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
            }
            // The app's own error text is never kept: it can echo what was sent.
            let msg = format!("{} answered HTTP {} ({})", req.place(), resp.status, pull::status_class(resp.status));
            let msg = integ.redact(state, &msg).await;
            store_log(state, &integ, NewDelivery { request: Some(logged_req), ..NewDelivery::log("pull", "error", Some(&msg)) }).await?;
            sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + 1 WHERE id = $1").bind(integ.id).execute(&state.db).await?;
            return pull_failed(state, &integ, run, &msg).await;
        }
        let body = (resp.body.len() <= BODY_MAX_BYTES).then_some(resp.body.as_str());
        let mut extra = Map::new();
        extra.insert("response".into(), response);
        extra.insert("request".into(), logged.log_json());
        extra.insert("page".into(), json!(pages + 1));
        extra.insert("backfill".into(), if backfill { json!({"since": bf["since"]}) } else { Value::Null });
        let input = Input { source: "pull".into(), content_type: ct.clone(), body: resp.body.clone(), extra, ..Default::default() };
        let delivery = store_log(state, &integ, NewDelivery { content_type: ct.as_deref(), body, request: Some(logged_req), ..NewDelivery::log("pull", "ok", None) }).await?;
        let res = match translate(&slot, &integ.script, script_input(&integ, &input, &run_state), true).await {
            Ok((raw, acts)) => {
                let (o, err) = actions::apply(state, &source_of(&integ, Some(delivery)), &acts, &raw_list(&raw), &Map::new(), &run_state, false).await;
                save_applied(state, delivery, &o).await?;
                match err {
                    None => Ok((o, acts)),
                    Some(e) => Err(e.public_message()),
                }
            }
            Err(e) => Err(e),
        };
        drop(slot);
        let ms = started.elapsed().as_millis() as i32;
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
                let more = o.next_request.clone();
                let mut upcoming = next.clone();
                if let Some(n) = more.clone() {
                    merge_next(&mut upcoming, n);
                }
                if let Some(run) = run {
                    // Save resumable progress after every page: the state and the next request.
                    let (set, removed) = state_patch(&acts);
                    let mut st = run_state.clone();
                    for k in removed {
                        st.remove(&k);
                    }
                    st.extend(set);
                    let total = bf["pages"].as_u64().unwrap_or(0) + 1;
                    let current = backfill_update(state, integ.id, run, json!({"pages": total, "state": st, "next": if more.is_some() { serde_json::to_value(&upcoming)? } else { json!({}) }})).await?;
                    if !current {
                        return Ok(()); // cancelled or replaced while this page ran
                    }
                } else {
                    save_live_state(state, integ.id, &acts).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                }
                match more {
                    Some(_) if pages < limit => next = upcoming,
                    Some(_) => {
                        if let Some(run) = run {
                            backfill_update(state, integ.id, run, json!({"status": "capped", "finished_at": Utc::now(), "note": format!("stopped after {limit} pages; resume to continue from where it got to")})).await?;
                        }
                        return Ok(());
                    }
                    None => {
                        if let Some(run) = run {
                            backfill_update(state, integ.id, run, json!({"status": "done", "finished_at": Utc::now()})).await?;
                        } else {
                            sqlx::query("UPDATE integrations SET last_error = NULL WHERE id = $1 AND last_error_at < now() - interval '1 second'").bind(integ.id).execute(&state.db).await?;
                        }
                        return Ok(());
                    }
                }
            }
            Err(msg) => {
                let msg = integ.redact(state, &msg).await;
                sqlx::query("UPDATE integration_deliveries SET status = 'error', error = $2, duration_ms = $3, attempts = 1 WHERE id = $1").bind(delivery).bind(&msg).bind(ms).execute(&state.db).await?;
                sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + 1 WHERE id = $1").bind(integ.id).execute(&state.db).await?;
                return pull_failed(state, &integ, run, &msg).await;
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

/// Scheduler: deliveries are kept at most 30 days, and each account's stored payloads stay under a
/// byte budget (oldest payloads go first; their log lines stay). Works in batches until done,
/// bounded by time per run, so it never falls behind a fixed row cap.
pub async fn prune(state: &AppState) -> anyhow::Result<()> {
    prune_with(state, Duration::from_secs(20), PAYLOAD_BUDGET_BYTES).await
}

/// [`prune`] with an explicit time budget and per-account payload byte budget.
pub async fn prune_with(state: &AppState, budget: Duration, bytes_per_account: i64) -> anyhow::Result<()> {
    let started = Instant::now();
    loop {
        let n = sqlx::query(
            "DELETE FROM integration_deliveries WHERE id IN (SELECT id FROM integration_deliveries WHERE created_at < now() - make_interval(days => $1) LIMIT 5000)",
        )
        .bind(RETENTION_DAYS as i32)
        .execute(&state.db)
        .await?
        .rows_affected();
        if n < 5000 || started.elapsed() > budget {
            break;
        }
    }
    let over: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT account_id, sum(body_bytes)::bigint FROM integration_deliveries WHERE body IS NOT NULL GROUP BY account_id HAVING sum(body_bytes) > $1",
    )
    .bind(bytes_per_account)
    .fetch_all(&state.db)
    .await?;
    for (account, mut total) in over {
        while total > bytes_per_account && started.elapsed() <= budget {
            // Just enough of the oldest finished payloads to get back under the budget (at most
            // 1000 a round); a delivery still waiting to run keeps its payload.
            let freed: Option<i64> = sqlx::query_scalar(
                "WITH ranked AS (
                    SELECT id, body_bytes, sum(body_bytes) OVER (ORDER BY created_at, id) AS running FROM integration_deliveries
                    WHERE account_id = $1 AND body IS NOT NULL AND status <> 'pending' AND NOT (status = 'error' AND next_attempt_at IS NOT NULL)
                    ORDER BY created_at, id LIMIT 1000),
                 pick AS (SELECT id, body_bytes FROM ranked WHERE running - body_bytes < $2),
                 cleared AS (UPDATE integration_deliveries d SET body = NULL, headers = '{}', query = '{}', body_bytes = 0 FROM pick WHERE d.id = pick.id RETURNING pick.body_bytes)
                 SELECT sum(body_bytes)::bigint FROM cleared",
            )
            .bind(account)
            .bind(total - bytes_per_account)
            .fetch_one(&state.db)
            .await?;
            match freed {
                Some(f) if f > 0 => total -= f,
                _ => break,
            }
        }
    }
    Ok(())
}
