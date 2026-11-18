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
use super::pull::{self, PullConfig, Scrub, TokenFor, Vars};
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
pub const JOB_FETCH: &str = "integrations_fetch";
/// Fetches one webhook may chain: its script asks for one, the script run on that answer may ask
/// for another, and so on, up to this many.
pub const MAX_FETCH_HOPS: u64 = 3;

fn source_of(i: &Integration, delivery: Option<Uuid>, ledger: Option<Uuid>) -> Source {
    Source { integration_id: i.id, account_id: i.account_id, name: i.name.clone(), contacts_opted_in: i.contacts_opted_in, metrics: i.metrics.clone(), delivery, ledger }
}

/// Everything secret about this integration, to scrub from what is stored or shown.
async fn scrub_for(state: &AppState, i: &Integration) -> Scrub {
    Scrub::new(i.secret_values(state).await)
}

// ---------- Concurrent pulls ----------

/// Pulls running at once: one per account, and a global cap (default 4, `INTEGRATION_PULL_SLOTS`).
/// A pull job that can't get a slot goes back in the queue. (In memory: production runs one
/// server process.)
pub struct PullSlots;

pub const PULLS_PER_ACCOUNT: usize = 1;

static PULL_STATE: once_cell::sync::Lazy<std::sync::Mutex<(usize, std::collections::HashMap<Uuid, usize>)>> = once_cell::sync::Lazy::new(Default::default);
static PULL_CAP: once_cell::sync::Lazy<usize> =
    once_cell::sync::Lazy::new(|| std::env::var("INTEGRATION_PULL_SLOTS").ok().and_then(|v| v.parse().ok()).filter(|n: &usize| *n > 0).unwrap_or(4));
static PULL_CAP_OVERRIDE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Tests: a different global cap (many test accounts pull at once in one process).
pub fn set_pull_cap(n: usize) {
    PULL_CAP_OVERRIDE.store(n, std::sync::atomic::Ordering::Relaxed);
}

fn pull_cap() -> usize {
    match PULL_CAP_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => *PULL_CAP,
        n => n,
    }
}

pub struct PullSlot {
    account: Uuid,
}

impl Drop for PullSlot {
    fn drop(&mut self) {
        let mut s = PULL_STATE.lock().unwrap();
        s.0 = s.0.saturating_sub(1);
        if let Some(n) = s.1.get_mut(&self.account) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                s.1.remove(&self.account);
            }
        }
    }
}

impl PullSlots {
    pub fn try_take(account: Uuid) -> Option<PullSlot> {
        let mut s = PULL_STATE.lock().unwrap();
        let mine = s.1.get(&account).copied().unwrap_or(0);
        if s.0 >= pull_cap() || mine >= PULLS_PER_ACCOUNT {
            return None;
        }
        s.0 += 1;
        *s.1.entry(account).or_insert(0) += 1;
        Some(PullSlot { account })
    }
}

// ---------- Payload budget ----------

static BUDGET_OVERRIDES: once_cell::sync::Lazy<std::sync::Mutex<std::collections::HashMap<Uuid, i64>>> = once_cell::sync::Lazy::new(Default::default);

/// Use a smaller stored-payload budget for one account (tests only).
#[doc(hidden)]
pub fn set_payload_budget_for(account: Uuid, bytes: i64) {
    BUDGET_OVERRIDES.lock().unwrap().insert(account, bytes);
}

fn payload_budget(account: Uuid) -> i64 {
    BUDGET_OVERRIDES.lock().unwrap().get(&account).copied().unwrap_or(PAYLOAD_BUDGET_BYTES)
}

/// Pagination data between pages (the next request can carry whatever the app put in a `Link`
/// header) is kept encrypted whenever it leaves memory: in a job's payload and in a backfill's
/// progress.
fn seal_next(state: &AppState, n: &NextRequest) -> AppResult<Value> {
    if serde_json::to_value(n)?.as_object().is_none_or(|o| o.is_empty()) {
        return Ok(Value::Null);
    }
    Ok(json!(crate::crypto::encrypt(&state.cfg.secret_key, &serde_json::to_string(n)?)?))
}

fn open_next(state: &AppState, sealed: &Value) -> NextRequest {
    sealed
        .as_str()
        .and_then(|c| crate::crypto::decrypt(&state.cfg.secret_key, c).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
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

fn raw_list(raw: &Value) -> Vec<Value> {
    raw.as_array().cloned().unwrap_or_default()
}

/// A job that couldn't get a slot goes back in the (background) queue a few seconds later.
async fn requeue(state: &AppState, kind: &str, payload: Value) -> anyhow::Result<()> {
    let wait = rand::thread_rng().gen_range(2_000..8_000);
    crate::jobs::enqueue_background(&state.db, kind, payload, Some(Utc::now() + chrono::Duration::milliseconds(wait))).await?;
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

async fn save_live_state<'e>(db: impl sqlx::PgExecutor<'e>, id: Uuid, acts: &[Action]) -> AppResult<()> {
    let (set, removed) = state_patch(acts);
    if set.is_empty() && removed.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE integrations SET state = (state || $2) - $3::text[] WHERE id = $1").bind(id).bind(Value::Object(set)).bind(&removed).execute(db).await?;
    Ok(())
}

fn result_json(o: &Outcome) -> Value {
    json!({"counts": o.counts, "effects": o.effects, "logs": o.logs})
}

fn status_for(n_actions: usize) -> &'static str {
    if n_actions == 0 { "ignored" } else { "ok" }
}

/// The action list to keep for a delivery, fixed the first time its script succeeds: events
/// without a `unique_id` get one from the original delivery, the action's index and its content,
/// and events without a `time` are dated when the script ran. Retries apply exactly this list.
pub fn canonical(raw: &[Value], root: Uuid, ran_at: DateTime<Utc>) -> Vec<Value> {
    raw.iter()
        .enumerate()
        .map(|(i, a)| {
            let mut a = a.clone();
            if a["type"] == "track_event" {
                if a.get("unique_id").is_none_or(Value::is_null) {
                    let print = actions::fingerprint(&a);
                    a["unique_id"] = json!(format!("delivery:{root}:{i}:{print}"));
                }
                if a.get("time").is_none_or(Value::is_null) {
                    a["time"] = json!(ran_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
                }
            }
            a
        })
        .collect()
}

/// Run the script and fix its actions: validated, in canonical form, and with every credential
/// masked (the list is stored, and what is stored is what is applied, now and on a retry). The
/// script's `next_request` comes back separately, as it was: it can carry what the app put in a
/// `Link` header, so it stays in memory (and encrypted when it must be kept).
async fn script_actions(slot: &Slot, script: &str, input: Value, pull: bool, root: Uuid, scrub: &Scrub) -> Result<(Vec<Value>, Option<NextRequest>), String> {
    let raw = sandbox::run(slot, script, &input).await?;
    let parsed = actions::parse(&raw, pull)?;
    let next = parsed.into_iter().find_map(|a| if let Action::NextRequest(n) = a { Some(n) } else { None });
    let list = canonical(&raw_list(&raw), root, Utc::now());
    let list: Vec<Value> = list.iter().map(|a| scrub.value(a)).collect();
    Ok((list, next))
}

/// Job: process one stored delivery (webhook or replay). The first successful script run fixes the
/// delivery's action list; a retry applies that list and never runs the script again.
pub async fn process_delivery(state: &AppState, delivery_id: Uuid) -> anyhow::Result<()> {
    type Row = (Uuid, Uuid, String, String, Option<String>, Value, Value, Option<String>, i32, Option<Value>, Option<Uuid>, Value, Option<Value>);
    let row: Option<Row> = sqlx::query_as(
        "SELECT integration_id, account_id, source, status, content_type, headers, query, body, attempts, request, replay_of, applied, actions FROM integration_deliveries WHERE id = $1",
    )
    .bind(delivery_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((integration_id, account_id, source, status, content_type, headers, query, body, attempts, request, replay_of, applied, stored)) = row else { return Ok(()) };
    if !matches!(status.as_str(), "pending" | "error") {
        return Ok(());
    }
    let Some(integ) = model::load_any(state, integration_id).await? else { return Ok(()) };
    if integ.status != "enabled" {
        // Turned off while it waited: nothing is written, and its payload isn't kept.
        sqlx::query("UPDATE integration_deliveries SET status = 'ignored', error = 'the integration is turned off', next_attempt_at = NULL, processed_at = now(), body = NULL, headers = '{}', query = '{}', body_bytes = 0 WHERE id = $1")
            .bind(delivery_id)
            .execute(&state.db)
            .await?;
        return Ok(());
    }
    let scrub = scrub_for(state, &integ).await;
    let started = Instant::now();
    // A replayed pull page runs as a pull again (minus fetching more pages).
    let original = request.as_ref().and_then(|r| r["original_source"].as_str()).map(String::from);
    let replay = source == "replay";
    let shown_source = if replay { original.clone().unwrap_or_else(|| "webhook".into()) } else { source.clone() };
    let root = replay_of.unwrap_or(delivery_id);
    // A webhook (or the answer to a fetch it asked for) may ask to look up the record it names,
    // when the integration's pull settings declare which lookups are allowed (`fetch`).
    let hop = request.as_ref().and_then(|r| r["fetch"]["hop"].as_u64()).unwrap_or(0);
    let rules = integ.pull_config().and_then(|c| c.fetch);
    let can_fetch = rules.is_some() && matches!(shown_source.as_str(), "webhook" | "fetch") && hop < MAX_FETCH_HOPS;
    let mut fetch_next = request.as_ref().map(|r| r["fetch_enc"].clone()).unwrap_or(Value::Null);
    let list: Result<Vec<Value>, String> = match stored.as_ref().and_then(|v| v.as_array()) {
        Some(a) => Ok(a.clone()),
        None => {
            let Some(slot) = Slots::try_take(account_id) else {
                return requeue(state, JOB_DELIVERY, json!({"delivery_id": delivery_id})).await;
            };
            let mut extra = Map::new();
            if let Some(r) = request.as_ref() {
                if shown_source == "fetch" {
                    // A lookup's run sees the request made, the answer's status and headers, and the
                    // (declared, redacted, sealed) context the webhook carried.
                    let mut made = r["made"].clone();
                    extra.insert("response".into(), made["response"].clone());
                    if let Some(o) = made.as_object_mut() {
                        o.remove("response");
                    }
                    extra.insert("request".into(), made);
                    extra.insert("context".into(), open_next(state, &r["lookup_enc"]).context.unwrap_or(Value::Null));
                } else if let Some(resp) = r.get("response") {
                    extra.insert("response".into(), resp.clone());
                }
            }
            let input = Input { source: shown_source.clone(), replay, content_type, body: body.unwrap_or_default(), headers, query, extra };
            let ran = script_actions(&slot, &integ.script, script_input(&integ, &input, &integ.state_map()), shown_source == "pull" || can_fetch, root, &scrub).await;
            drop(slot);
            match ran {
                Ok((list, next)) => {
                    let list: Vec<Value> = list.into_iter().filter(|a| a["type"] != "next_request").collect();
                    // Pull deliveries (replayed pages) never fetch more; a webhook's fetch is kept
                    // encrypted with its action list (it can carry what the app sent).
                    let lookup = match (next.filter(|_| can_fetch), &rules) {
                        (Some(n), Some(r)) => Some(checked_lookup(&integ, r, &n)),
                        _ => None,
                    };
                    let sealed = match lookup {
                        Some(Ok(n)) => seal_next(state, &n).map_err(|e| anyhow::anyhow!(e.public_message()))?,
                        Some(Err(e)) => return delivery_failed(state, &integ, &scrub, delivery_id, attempts, started.elapsed().as_millis() as i32, &e).await,
                        None => Value::Null,
                    };
                    // Whoever stored a list first wins; this run applies that one (and its fetch).
                    let (kept, kept_fetch): (Value, Option<Value>) = sqlx::query_as(
                        "UPDATE integration_deliveries
                         SET request = CASE WHEN actions IS NULL THEN jsonb_strip_nulls((COALESCE(request, '{}'::jsonb) - 'fetch_enc') || jsonb_build_object('fetch_enc', $3::jsonb)) ELSE request END,
                             actions = COALESCE(actions, $2)
                         WHERE id = $1 RETURNING actions, request->'fetch_enc'",
                    )
                    .bind(delivery_id)
                    .bind(Value::Array(list))
                    .bind(sealed)
                    .fetch_one(&state.db)
                    .await?;
                    fetch_next = kept_fetch.unwrap_or(Value::Null);
                    Ok(kept.as_array().cloned().unwrap_or_default())
                }
                Err(e) => Err(e),
            }
        }
    };
    let skip = applied.as_object().cloned().unwrap_or_default();
    let src = source_of(&integ, Some(root), Some(delivery_id));
    let res = match list {
        Ok(list) => match actions::parse(&Value::Array(list.clone()), true) {
            Ok(acts) => {
                let (o, err) = actions::apply(state, &src, &acts, &list, &skip, &integ.state_map(), false).await;
                match err {
                    None => {
                        save_live_state(&state.db, integ.id, &acts).await?;
                        Ok((o, acts))
                    }
                    Some(e) => Err(e.public_message()),
                }
            }
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    let ms = started.elapsed().as_millis() as i32;
    match res {
        Ok((o, acts)) => {
            let n = acts.len();
            // The fetch it asked for is queued in the same transaction that marks it done.
            let fetch = can_fetch && fetch_next.is_string();
            let mut result = result_json(&o);
            if fetch {
                result["fetch"] = json!("queued");
            }
            let mut tx = state.db.begin().await?;
            sqlx::query("UPDATE integration_deliveries SET status = $2, result = $3, error = NULL, duration_ms = $4, attempts = attempts + 1, next_attempt_at = NULL, processed_at = now() WHERE id = $1")
                .bind(delivery_id)
                .bind(if fetch { "ok" } else { status_for(n) })
                .bind(scrub.value(&result))
                .bind(ms)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE integrations SET deliveries_ok = deliveries_ok + 1, last_delivery_at = now() WHERE id = $1").bind(integ.id).execute(&mut *tx).await?;
            if can_fetch {
                match queue_lookups(&mut tx, state, &integ, delivery_id, hop + 1, &acts).await {
                    Ok(_) | Err(Refused::Duplicate(_)) => {}
                    Err(Refused::Busy(m) | Refused::OverBudget(m)) => {
                        tx.rollback().await?;
                        return delivery_failed(state, &integ, &scrub, delivery_id, attempts, ms, &m).await;
                    }
                    Err(Refused::Error(e)) => return Err(anyhow::anyhow!(e.public_message())),
                }
            }
            if fetch {
                match queue_fetch(&mut tx, state, &integ, delivery_id, hop + 1, &fetch_next).await {
                    Ok(()) | Err(Refused::Duplicate(_)) => {}
                    // The fetch can't be reserved now: the delivery is tried again later (what it
                    // already wrote is skipped then).
                    Err(Refused::Busy(m) | Refused::OverBudget(m)) => {
                        tx.rollback().await?;
                        return delivery_failed(state, &integ, &scrub, delivery_id, attempts, ms, &m).await;
                    }
                    Err(Refused::Error(e)) => return Err(anyhow::anyhow!(e.public_message())),
                }
            }
            tx.commit().await?;
        }
        Err(msg) => return delivery_failed(state, &integ, &scrub, delivery_id, attempts, ms, &msg).await,
    }
    Ok(())
}

/// Reserve every `lookup` a run asked for (inside the transaction that settles the run), checked
/// like a webhook's fetch. Returns how many were reserved and how many the fetch rules refused.
async fn queue_lookups(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, state: &AppState, integ: &Integration, parent: Uuid, hop: u64, acts: &[Action]) -> Result<(usize, usize), Refused> {
    let Some(rules) = integ.pull_config().and_then(|c| c.fetch) else { return Ok((0, acts.iter().filter(|a| matches!(a, Action::Lookup(_))).count())) };
    let (mut queued, mut refused) = (0, 0);
    for a in acts {
        let Action::Lookup(n) = a else { continue };
        match checked_lookup(integ, &rules, n) {
            Ok(ok) => {
                let sealed = seal_next(state, &ok).map_err(Refused::Error)?;
                queue_fetch(tx, state, integ, parent, hop, &sealed).await?;
                queued += 1;
            }
            Err(_) => refused += 1,
        }
    }
    Ok((queued, refused))
}

/// A lookup a webhook's script asked for, made safe to keep: allowed by the integration's fetch
/// rules (a GET of a declared path and query), its context cut to the declared keys, put through
/// the integration's redact list and capped at 16 KB.
fn checked_lookup(integ: &Integration, rules: &pull::FetchRules, n: &NextRequest) -> Result<NextRequest, String> {
    let settings = integ.settings_map();
    let st = integ.state_map();
    let none = Map::new();
    let now = Utc::now();
    let vars = Vars { secrets: &none, state: &st, settings: &settings, now, since: now, redact: true };
    let mut ok = rules.allow(n, &vars)?;
    if let Some(c) = ok.context.take() {
        // The redact list's removals apply to the context (a keep list describes the payload).
        let removals: Vec<_> = super::redact::paths_with(&integ.redact, &settings).into_iter().filter(|p| !p.is_keep()).collect();
        let kept = super::redact::body(Some("application/json"), &c.to_string(), &removals).and_then(|t| serde_json::from_str::<Value>(&t).ok());
        if kept.as_ref().is_some_and(|k| k.to_string().len() > actions::MAX_CONTEXT_BYTES) {
            return Err("the fetch's context is over 16 KB".into());
        }
        ok.context = kept.filter(|k| k.as_object().is_some_and(|o| !o.is_empty()));
    }
    Ok(ok)
}

/// A delivery whose script or writes failed (or whose fetch couldn't be reserved): noted, and
/// tried again after 1, 5, 30 and 120 minutes.
async fn delivery_failed(state: &AppState, integ: &Integration, scrub: &Scrub, delivery_id: Uuid, attempts: i32, ms: i32, msg: &str) -> anyhow::Result<()> {
    let msg = scrub.text(msg);
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
        crate::jobs::enqueue_background(&mut *tx, JOB_DELIVERY, json!({"delivery_id": delivery_id}), Some(at)).await?;
    }
    tx.commit().await?;
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
    /// With a message id: a resend counts as one only within this many minutes (for apps whose id
    /// is a hash of the content, which a later, real change can repeat).
    pub dedupe_minutes: Option<i64>,
}

impl<'a> NewDelivery<'a> {
    pub fn log(source: &'a str, status: &'a str, error: Option<&'a str>) -> Self {
        static EMPTY: once_cell::sync::Lazy<Value> = once_cell::sync::Lazy::new(|| json!({}));
        NewDelivery { source, content_type: None, headers: &EMPTY, query: &EMPTY, body: None, request: None, status, error, replay_of: None, message_id: None, applied: None, dedupe_minutes: None }
    }
}

/// Why a delivery wasn't stored.
#[derive(Debug)]
pub enum Refused {
    /// The app already sent a message with this id; it was acknowledged and ignored.
    Duplicate(Uuid),
    /// Too many deliveries are waiting to run.
    Busy(String),
    /// Keeping its payload would take the account's stored payloads over their budget, and the
    /// payloads still waiting to run already fill it.
    OverBudget(String),
    Error(AppError),
}

impl From<sqlx::Error> for Refused {
    fn from(e: sqlx::Error) -> Self {
        Refused::Error(e.into())
    }
}

/// What to keep of a payload: the integration's redact list applied (a body that can't be checked
/// against it isn't kept at all), then every secret masked.
fn kept_body(integ: &Integration, scrub: &Scrub, content_type: Option<&str>, body: Option<&str>) -> (Option<String>, bool) {
    let Some(b) = body else { return (None, false) };
    match super::redact::body(content_type, b, &super::redact::paths_with(&integ.redact, &integ.settings_map())) {
        Some(kept) => (Some(scrub.text(&kept)), false),
        None => (None, true),
    }
}

/// Store a delivery and, when it is pending, queue its job, in one transaction. Pending deliveries
/// count against the per-integration and per-account limits on work still to run, and a payload is
/// kept only within the account's stored-payload budget (oldest finished payloads make room;
/// payloads still waiting to run can't). Everything stored is scrubbed of secrets first.
pub async fn store_delivery(state: &AppState, integ: &Integration, scrub: &Scrub, d: NewDelivery<'_>) -> Result<Uuid, Refused> {
    let id = Uuid::now_v7();
    let (body, unkept) = kept_body(integ, scrub, d.content_type, d.body);
    let note = unkept.then_some("the payload isn't JSON or a form, so it can't be checked against the redact list and wasn't kept");
    let error = d.error.map(|e| scrub.text(e)).or(note.map(String::from).filter(|_| d.status != "pending"));
    let headers = scrub.value(d.headers);
    let query = scrub.value(d.query);
    let request = d.request.as_ref().map(|r| scrub.value(r));
    let mut tx = state.db.begin().await?;
    if d.status == "pending" || body.is_some() {
        // One admission at a time per account, so two requests can't both slip under a limit.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7021))").bind(integ.account_id.to_string()).execute(&mut *tx).await?;
    }
    if d.status == "pending" {
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
    // A windowed message id: a copy within the window is a resend; after it, the same id is new.
    let mut message_id = d.message_id.clone();
    if let (Some(mid), Some(minutes)) = (&d.message_id, d.dedupe_minutes) {
        let like = format!("{}@%", mid.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let seen: Option<Uuid> = sqlx::query_scalar("SELECT id FROM integration_deliveries WHERE integration_id = $1 AND message_id LIKE $2 AND created_at > now() - make_interval(mins => $3::int) ORDER BY created_at LIMIT 1")
            .bind(integ.id)
            .bind(&like)
            .bind(minutes as i32)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(first) = seen {
            return Err(Refused::Duplicate(first));
        }
        message_id = Some(format!("{mid}@{id}"));
    }
    if let Some(b) = &body {
        make_room(&mut tx, integ.account_id, b.len() as i64).await?;
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
    .bind(&headers)
    .bind(&query)
    .bind(&body)
    .bind(&request)
    .bind(&error)
    .bind(d.replay_of)
    .bind(&message_id)
    .bind(d.applied)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        let first: Uuid = sqlx::query_scalar("SELECT id FROM integration_deliveries WHERE integration_id = $1 AND message_id = $2").bind(integ.id).bind(&message_id).fetch_one(&mut *tx).await?;
        return Err(Refused::Duplicate(first));
    }
    if d.status == "pending" {
        crate::jobs::enqueue_background(&mut *tx, JOB_DELIVERY, json!({"delivery_id": id}), None).await?;
    }
    tx.commit().await?;
    Ok(id)
}

/// Inside an admission: make room for `need` more payload bytes under the account's budget by
/// dropping the oldest finished payloads (their log lines stay). Refused when the payloads still
/// waiting to run (pending or retrying) leave no room.
async fn make_room(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, account: Uuid, need: i64) -> Result<(), Refused> {
    let budget = payload_budget(account);
    let used: i64 = sqlx::query_scalar("SELECT bytes FROM integration_payload_bytes WHERE account_id = $1").bind(account).fetch_optional(&mut **tx).await?.unwrap_or(0);
    if used + need <= budget {
        return Ok(());
    }
    let open: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(body_bytes), 0)::bigint FROM integration_deliveries
         WHERE account_id = $1 AND ((body IS NOT NULL AND (status = 'pending' OR (status = 'error' AND next_attempt_at IS NOT NULL)))
             OR (source = 'fetch' AND body IS NULL AND status = 'pending'))",
    )
    .bind(account)
    .fetch_one(&mut **tx)
    .await?;
    let full = || Refused::OverBudget(format!("the payloads waiting to be processed already fill this account's {} MB of storage; retry later", budget / (1024 * 1024)));
    if open + need > budget {
        return Err(full());
    }
    let mut over = used + need - budget;
    for _ in 0..20 {
        if over <= 0 {
            return Ok(());
        }
        let freed: Option<i64> = sqlx::query_scalar(FREE_OLDEST_SQL).bind(account).bind(over).fetch_one(&mut **tx).await?;
        match freed {
            Some(f) if f > 0 => over -= f,
            _ => break,
        }
    }
    if over <= 0 { Ok(()) } else { Err(full()) }
}

/// Clear just enough of an account's oldest finished payloads ($1) to free $2 bytes (at most 1000
/// rows a round). Returns the bytes freed.
const FREE_OLDEST_SQL: &str = "WITH ranked AS (
        SELECT id, body_bytes, sum(body_bytes) OVER (ORDER BY created_at, id) AS running FROM integration_deliveries
        WHERE account_id = $1 AND body IS NOT NULL AND status <> 'pending' AND NOT (status = 'error' AND next_attempt_at IS NOT NULL)
        ORDER BY created_at, id LIMIT 1000),
     pick AS (SELECT id, body_bytes FROM ranked WHERE running - body_bytes < $2),
     cleared AS (UPDATE integration_deliveries d SET body = NULL, headers = '{}', query = '{}', body_bytes = 0 FROM pick WHERE d.id = pick.id RETURNING pick.body_bytes)
     SELECT sum(body_bytes)::bigint FROM cleared";

/// Store a pull page's delivery (never deduplicated, never queued). Over the payload budget, the
/// log line is kept without the page.
async fn store_log(state: &AppState, integ: &Integration, scrub: &Scrub, d: NewDelivery<'_>) -> anyhow::Result<Uuid> {
    let retry = NewDelivery { body: None, content_type: d.content_type, headers: d.headers, query: d.query, request: d.request.clone(), error: d.error, ..NewDelivery::log(d.source, d.status, None) };
    match store_delivery(state, integ, scrub, d).await {
        Ok(id) => Ok(id),
        Err(Refused::OverBudget(msg)) => {
            let note = format!("page not kept: {msg}");
            match store_delivery(state, integ, scrub, NewDelivery { error: Some(&note), ..retry }).await {
                Ok(id) => Ok(id),
                Err(other) => Err(anyhow::anyhow!("couldn't store the delivery: {other:?}")),
            }
        }
        Err(Refused::Error(e)) => Err(anyhow::anyhow!(e.public_message())),
        Err(other) => Err(anyhow::anyhow!("couldn't store the delivery: {other:?}")),
    }
}

// ---------- Test runs ----------

/// Run the script on a sample (or, with `pull`, on the first page fetched now) and say exactly what
/// would be written. Writes nothing. A pull test reuses the cached access token and asks for a new
/// one only when none is cached. The answer is scrubbed of every secret, including what the
/// request sent and anything the app echoed back.
pub async fn test(state: &AppState, integ: &Integration, script: Option<&str>, sample: TestSample) -> AppResult<Value> {
    let mut scrub = scrub_for(state, integ).await;
    let out = test_inner(state, integ, script, sample, &mut scrub).await?;
    Ok(scrub.value(&out))
}

async fn test_inner(state: &AppState, integ: &Integration, script: Option<&str>, sample: TestSample, scrub: &mut Scrub) -> AppResult<Value> {
    let script = script.unwrap_or(&integ.script);
    let started = Instant::now();
    let slot = Slots::take_within(integ.account_id, Duration::from_secs(5))
        .await
        .ok_or_else(|| AppError::Busy("too many scripts are running for this account right now; try again in a few seconds".into()))?;
    let (input, pull_meta) = match sample {
        TestSample::Payload { payload, headers, query, source } => {
            let body = match &payload {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let ct = if payload.is_string() { None } else { Some("application/json".to_string()) };
            // A dry run gives the script what a real delivery's run would get: the payload as it
            // would be stored (redacted, or projected by a keep list), so its actions and logs can't
            // show more than a live run.
            let (kept, _) = kept_body(integ, scrub, ct.as_deref(), Some(&body));
            let body = kept.unwrap_or_default();
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
            scrub.add_request(&req);
            let origin = cfg.origin();
            let who = TokenFor { integration_id: integ.id, auth_version: integ.auth_version, origin: &origin };
            let token = match pull::oauth_token(state, &who, &cfg.auth, &vars).await {
                Ok(t) => t,
                Err(e) => return Ok(json!({"ok": false, "error": e.public_message(), "pull": {"request": logged.log_json()}, "duration_ms": started.elapsed().as_millis() as u64})),
            };
            if let Some(t) = &token {
                scrub.add(t);
            }
            let resp = match pull::send(&req, token.as_deref()).await {
                Ok(r) => r,
                Err(e) => return Ok(json!({"ok": false, "error": e.public_message(), "pull": {"request": logged.log_json()}, "duration_ms": started.elapsed().as_millis() as u64})),
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
                return Ok(json!({"ok": false, "error": msg, "pull": meta, "duration_ms": started.elapsed().as_millis() as u64}));
            }
            (Input { source: "pull".into(), content_type: resp.headers.get("content-type").and_then(|v| v.as_str()).map(String::from), body: resp.body, extra, ..Default::default() }, Some(meta))
        }
    };
    // With pull settings, a webhook's script may ask to fetch the record it names (shown, not run).
    let is_pull = input.source == "pull" || integ.pull_config().is_some_and(|c| c.fetch.is_some());
    let script_in = script_input(integ, &input, &integ.state_map());
    let raw = match sandbox::run(&slot, script, &script_in).await {
        Ok(x) => x,
        Err(e) => return Ok(json!({"ok": false, "error": e, "pull": pull_meta, "duration_ms": started.elapsed().as_millis() as u64})),
    };
    drop(slot);
    let acts = match actions::parse(&raw, is_pull) {
        Ok(a) => a,
        Err(e) => return Ok(json!({"ok": false, "actions": raw, "error": e, "pull": pull_meta, "duration_ms": started.elapsed().as_millis() as u64})),
    };
    let (o, err) = actions::apply(state, &source_of(integ, None, None), &acts, &raw_list(&raw), &Map::new(), &integ.state_map(), true).await;
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
    let scrub = scrub_for(state, integ).await;
    let mut new_ids = Vec::new();
    for (id, source, ct, headers, query, body, request, replay_of, applied) in rows {
        let orig = if source == "replay" { request.as_ref().and_then(|r| r["original_source"].as_str()).unwrap_or("webhook").to_string() } else { source };
        let mut req = request.unwrap_or(json!({}));
        req["original_source"] = json!(orig);
        // A replay runs the current script again, which decides afresh what to fetch.
        if let Some(o) = req.as_object_mut() {
            o.remove("fetch_enc");
        }
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
        let d = NewDelivery { source: "replay", content_type: ct.as_deref(), headers: &headers, query: &query, body: body.as_deref(), request: Some(req), status: "pending", error: None, replay_of: Some(root), message_id: None, applied: Some(Value::Object(all)), dedupe_minutes: None };
        match store_delivery(state, integ, &scrub, d).await {
            Ok(nid) => new_ids.push(nid),
            Err(Refused::Busy(msg) | Refused::OverBudget(msg)) => {
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
    acc.context = n.context;
}

/// Queue a live pull unless one for this integration is already waiting or running. The check and
/// the insert happen in one transaction under a lock on the integration, so two schedulers (or a
/// scheduler and a "pull now") can't both queue one. Returns whether one was queued.
pub async fn enqueue_live_pull(state: &AppState, integration_id: Uuid) -> AppResult<bool> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7022))").bind(integration_id.to_string()).execute(&mut *tx).await?;
    let busy: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = $1 AND payload->>'integration_id' = $2 AND payload->>'mode' = 'live' AND run_at <> 'infinity')")
        .bind(JOB_PULL)
        .bind(integration_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
    if !busy {
        crate::jobs::enqueue_background(&mut *tx, JOB_PULL, json!({"integration_id": integration_id, "mode": "live", "next": null, "pages": 0}), None).await?;
    }
    tx.commit().await?;
    Ok(!busy)
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
        let mut tx = state.db.begin().await?;
        sqlx::query("UPDATE integrations SET backfill = backfill || $2 WHERE id = $1").bind(integ.id).bind(&patch).execute(&mut *tx).await?;
        crate::jobs::enqueue_background(&mut *tx, JOB_PULL, json!({"integration_id": integ.id, "mode": "backfill", "run_id": run_id, "next_enc": b["next_enc"].clone(), "pages": 0}), None).await?;
        tx.commit().await?;
        return Ok(json!({"started": "backfill", "resumed": true, "backfill": {"id": run_id, "pages": b["pages"]}}));
    }
    match backfill_since {
        Some(since) => {
            if since > Utc::now() {
                return Err(AppError::bad("the backfill start date is in the future"));
            }
            let run_id = Uuid::now_v7();
            let b = json!({"id": run_id, "status": "running", "since": since, "started_at": Utc::now(), "pages": 0, "errors": 0, "state": {}, "next_enc": null});
            let mut tx = state.db.begin().await?;
            // Only when no backfill is running, checked by the write itself.
            let n = sqlx::query("UPDATE integrations SET backfill = $2 WHERE id = $1 AND (backfill IS NULL OR backfill->>'status' IS DISTINCT FROM 'running')")
                .bind(integ.id)
                .bind(&b)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if n == 0 {
                return Err(AppError::Conflict("a backfill is already running; cancel it first".into()));
            }
            crate::jobs::enqueue_background(&mut *tx, JOB_PULL, json!({"integration_id": integ.id, "mode": "backfill", "run_id": run_id, "next_enc": null, "pages": 0}), None).await?;
            tx.commit().await?;
            Ok(json!({"started": "backfill", "backfill": b}))
        }
        None => {
            let queued = enqueue_live_pull(state, integ.id).await?;
            Ok(if queued { json!({"started": "pull"}) } else { json!({"started": "pull", "already_queued": true}) })
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
async fn backfill_update<'e>(db: impl sqlx::PgExecutor<'e>, scrub: &Scrub, id: Uuid, run_id: &str, patch: Value) -> anyhow::Result<bool> {
    let n = sqlx::query("UPDATE integrations SET backfill = backfill || $2 WHERE id = $1 AND backfill->>'id' = $3 AND backfill->>'status' = 'running'")
        .bind(id)
        .bind(scrub.value(&patch))
        .bind(run_id)
        .execute(db)
        .await?
        .rows_affected();
    Ok(n > 0)
}

async fn pull_failed(state: &AppState, integ: &Integration, scrub: &Scrub, run_id: Option<&str>, msg: &str) -> anyhow::Result<()> {
    let msg = scrub.text(msg);
    sqlx::query("UPDATE integrations SET last_error = $2, last_error_at = now() WHERE id = $1").bind(integ.id).bind(&msg).execute(&state.db).await?;
    if let Some(run) = run_id {
        backfill_update(&state.db, scrub, integ.id, run, json!({"status": "failed", "error": msg, "finished_at": Utc::now()})).await?;
    }
    Ok(())
}

/// Job: fetch up to a few pages of a pull (or backfill), running the script on each, then hand
/// over to a new job if the script asks for more. At most [`PULLS_PER_ACCOUNT`] run at once per
/// account (and a few in all); the rest wait in the queue.
pub async fn run_pull(state: &AppState, p: &Value) -> anyhow::Result<()> {
    let Some(id) = p["integration_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let Some(first) = model::load_any(state, id).await? else { return Ok(()) };
    let Some(_pull_slot) = PullSlots::try_take(first.account_id) else {
        return requeue(state, JOB_PULL, p.clone()).await;
    };
    let backfill = p["mode"] == "backfill";
    let run_id = p["run_id"].as_str().map(String::from);
    let run = if backfill { run_id.as_deref() } else { None };
    let mut next: NextRequest = open_next(state, &p["next_enc"]);
    let mut pages = p["pages"].as_u64().unwrap_or(0) as u32;
    let continue_with = |next: &NextRequest, pages: u32| -> AppResult<Value> {
        let mut payload = p.clone();
        if let Some(o) = payload.as_object_mut() {
            o.remove("next");
        }
        payload["next_enc"] = seal_next(state, next)?;
        payload["pages"] = json!(pages);
        Ok(payload)
    };
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
        let mut scrub = scrub_for(state, &integ).await;
        let Some(slot) = Slots::try_take(integ.account_id) else {
            return requeue(state, JOB_PULL, continue_with(&next, pages)?).await;
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
            (Err(e), _) | (_, Err(e)) => return pull_failed(state, &integ, &scrub, run, &e).await,
        };
        scrub.add_request(&req);
        let origin = cfg.origin();
        let who = TokenFor { integration_id: integ.id, auth_version: integ.auth_version, origin: &origin };
        let token = match pull::oauth_token(state, &who, &cfg.auth, &vars).await {
            Ok(t) => t,
            Err(e) => return pull_failed(state, &integ, &scrub, run, &format!("couldn't get an access token: {}", e.public_message())).await,
        };
        if let Some(t) = &token {
            scrub.add(t);
        }
        let started = Instant::now();
        let resp = match pull::send(&req, token.as_deref()).await {
            Ok(r) => r,
            Err(e) => return pull_failed(state, &integ, &scrub, run, &e.public_message()).await,
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
            store_log(state, &integ, &scrub, NewDelivery { request: Some(logged_req), ..NewDelivery::log("pull", "error", Some(&msg)) }).await?;
            sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + 1 WHERE id = $1").bind(integ.id).execute(&state.db).await?;
            return pull_failed(state, &integ, &scrub, run, &msg).await;
        }
        let body = (resp.body.len() <= BODY_MAX_BYTES).then_some(resp.body.as_str());
        let mut extra = Map::new();
        extra.insert("response".into(), response);
        extra.insert("request".into(), logged.log_json());
        extra.insert("page".into(), json!(pages + 1));
        extra.insert("backfill".into(), if backfill { json!({"since": bf["since"]}) } else { Value::Null });
        let input = Input { source: "pull".into(), content_type: ct.clone(), body: resp.body.clone(), extra, ..Default::default() };
        let delivery = store_log(state, &integ, &scrub, NewDelivery { content_type: ct.as_deref(), body, request: Some(logged_req), ..NewDelivery::log("pull", "ok", None) }).await?;
        let ran = script_actions(&slot, &integ.script, script_input(&integ, &input, &run_state), true, delivery, &scrub).await;
        drop(slot);
        // A backfill page is applied under a lock on the integration that a replacement or a
        // cancel must wait for, and only while its run is still the current one: a replaced run
        // writes nothing. (A cancel waits for this page, then stops the run.)
        let mut guard = state.db.begin().await?;
        if let Some(run) = run {
            let current: Option<Uuid> = sqlx::query_scalar("SELECT id FROM integrations WHERE id = $1 AND backfill->>'id' = $2 AND backfill->>'status' = 'running' FOR SHARE")
                .bind(integ.id)
                .bind(run)
                .fetch_optional(&mut *guard)
                .await?;
            if current.is_none() {
                guard.rollback().await?;
                sqlx::query("UPDATE integration_deliveries SET status = 'ignored', error = 'the backfill was replaced or cancelled before this page was written', attempts = 1 WHERE id = $1")
                    .bind(delivery)
                    .execute(&state.db)
                    .await?;
                return Ok(());
            }
        }
        let mut raw_next = None;
        let res = match ran {
            Ok((list, next)) => {
                raw_next = next;
                sqlx::query("UPDATE integration_deliveries SET actions = $2 WHERE id = $1").bind(delivery).bind(Value::Array(list.clone())).execute(&state.db).await?;
                match actions::parse(&Value::Array(list.clone()), true) {
                    Ok(acts) => {
                        let (o, err) = actions::apply(state, &source_of(&integ, Some(delivery), Some(delivery)), &acts, &list, &Map::new(), &run_state, false).await;
                        match err {
                            None => Ok((o, acts)),
                            Some(e) => Err(e.public_message()),
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };
        let ms = started.elapsed().as_millis() as i32;
        pages += 1;
        match res {
            Ok((o, acts)) => {
                sqlx::query("UPDATE integration_deliveries SET status = $2, result = $3, duration_ms = $4, attempts = 1 WHERE id = $1")
                    .bind(delivery)
                    .bind(status_for(acts.len()))
                    .bind(scrub.value(&result_json(&o)))
                    .bind(ms)
                    .execute(&mut *guard)
                    .await?;
                sqlx::query("UPDATE integrations SET deliveries_ok = deliveries_ok + 1, last_delivery_at = now() WHERE id = $1").bind(integ.id).execute(&mut *guard).await?;
                // Lookups the page asked for (a record's details, say) are reserved with the page.
                match queue_lookups(&mut guard, state, &integ, delivery, 1, &acts).await {
                    Ok(_) | Err(Refused::Duplicate(_)) => {}
                    Err(Refused::Busy(_) | Refused::OverBudget(_)) => {
                        // No room for more work now: the page stands, its lookups are skipped and noted.
                        sqlx::query("UPDATE integration_deliveries SET error = 'its lookups were skipped: too much work is waiting' WHERE id = $1").bind(delivery).execute(&mut *guard).await?;
                    }
                    Err(Refused::Error(e)) => return Err(anyhow::anyhow!(e.public_message())),
                }
                // The page asked for: as the script wrote it (the stored list has it masked).
                let more = raw_next.clone().filter(|_| o.next_request.is_some());
                let mut upcoming = next.clone();
                if let Some(n) = more.clone() {
                    merge_next(&mut upcoming, n);
                }
                if let Some(run) = run {
                    // Save resumable progress after every page: the state and the next request
                    // (encrypted: it can hold what the app put in a Link header).
                    let (set, removed) = state_patch(&acts);
                    let mut st = run_state.clone();
                    for k in removed {
                        st.remove(&k);
                    }
                    st.extend(set);
                    let total = bf["pages"].as_u64().unwrap_or(0) + 1;
                    let sealed = if more.is_some() { seal_next(state, &upcoming)? } else { Value::Null };
                    backfill_update(&mut *guard, &scrub, integ.id, run, json!({"pages": total, "state": st, "next_enc": sealed})).await?;
                } else {
                    save_live_state(&mut *guard, integ.id, &acts).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                }
                guard.commit().await?;
                match more {
                    Some(_) if pages < limit => next = upcoming,
                    Some(_) => {
                        if let Some(run) = run {
                            backfill_update(&state.db, &scrub, integ.id, run, json!({"status": "capped", "finished_at": Utc::now(), "note": format!("stopped after {limit} pages; resume to continue from where it got to")})).await?;
                        }
                        return Ok(());
                    }
                    None => {
                        if let Some(run) = run {
                            backfill_update(&state.db, &scrub, integ.id, run, json!({"status": "done", "finished_at": Utc::now()})).await?;
                        } else {
                            sqlx::query("UPDATE integrations SET last_error = NULL WHERE id = $1 AND last_error_at < now() - interval '1 second'").bind(integ.id).execute(&state.db).await?;
                        }
                        return Ok(());
                    }
                }
            }
            Err(msg) => {
                let msg = scrub.text(&msg);
                sqlx::query("UPDATE integration_deliveries SET status = 'error', error = $2, duration_ms = $3, attempts = 1 WHERE id = $1").bind(delivery).bind(&msg).bind(ms).execute(&mut *guard).await?;
                sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + 1 WHERE id = $1").bind(integ.id).execute(&mut *guard).await?;
                guard.commit().await?;
                return pull_failed(state, &integ, &scrub, run, &msg).await;
            }
        }
    }
    crate::jobs::enqueue_background(&state.db, JOB_PULL, continue_with(&next, pages)?, None).await?;
    Ok(())
}

/// Record that a webhook wants a lookup (its `next_request`, already checked against the
/// integration's fetch rules and sealed), inside the transaction that marks the webhook done. The
/// intent is a delivery row (source `fetch`, no body yet) that counts against the account's limits
/// on work still to run, unique on the integration and what's being fetched (path, query and
/// context) while it's outstanding, so a second webhook for the same record reuses it. Its job is
/// queued in the same transaction; only that job makes the request.
async fn queue_fetch(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, state: &AppState, integ: &Integration, parent: Uuid, hop: u64, sealed: &Value) -> Result<(), Refused> {
    let next = open_next(state, sealed);
    let context = next.context.as_ref().map(Value::to_string).unwrap_or_default();
    let mut query: Vec<String> = next.query.iter().flatten().map(|(k, v)| format!("{k}={v}")).collect();
    query.sort();
    // The record alone (its path and query) is the intent's identity; a second callback for it
    // while it waits merges its context in (the latest wins).
    let key = crate::auth::sha256_hex(&format!("{}\n{}", next.url.as_deref().unwrap_or(""), query.join("&")));
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7021))").bind(integ.account_id.to_string()).execute(&mut **tx).await?;
    let (mine, all): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE integration_id = $1), count(*) FROM integration_deliveries
         WHERE account_id = $2 AND (status = 'pending' OR (status = 'error' AND next_attempt_at IS NOT NULL))",
    )
    .bind(integ.id)
    .bind(integ.account_id)
    .fetch_one(&mut **tx)
    .await?;
    if mine >= MAX_OPEN_PER_INTEGRATION || all >= MAX_OPEN_PER_ACCOUNT {
        return Err(Refused::Busy("too many deliveries and fetches are waiting; the fetch will be asked for again".into()));
    }
    make_room(tx, integ.account_id, context.len() as i64).await?;
    let id = Uuid::now_v7();
    // Only a waiting fetch intent takes the merge; any other row holding the id is left alone.
    let row: Option<(Uuid, bool)> = sqlx::query_as(
        "INSERT INTO integration_deliveries AS d (id, integration_id, account_id, source, status, body_bytes, request, message_id)
         VALUES ($1, $2, $3, 'fetch', 'pending', $4, $5, $6)
         ON CONFLICT (integration_id, message_id) WHERE message_id IS NOT NULL DO UPDATE
             SET request = jsonb_set(jsonb_set(d.request, '{lookup_enc}', EXCLUDED.request->'lookup_enc'), '{ctx_bytes}', to_jsonb($4::int)),
                 body_bytes = $4 + COALESCE((d.request->>'reserved')::int, 0)
             WHERE d.source = 'fetch' AND d.status = 'pending' AND d.body IS NULL
         RETURNING id, (xmax = 0)",
    )
    .bind(id)
    .bind(integ.id)
    .bind(integ.account_id)
    .bind(context.len() as i32)
    .bind(json!({"fetch": {"of": parent, "hop": hop, "attempts": 0}, "lookup_enc": sealed, "ctx_bytes": context.len()}))
    .bind(format!("fetch:{key}"))
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((got, true)) = row {
        crate::jobs::enqueue_background(&mut **tx, JOB_FETCH, json!({"delivery_id": got}), None).await?;
    }
    Ok(())
}

/// How long the app asked us to wait: `Retry-After` (seconds or an HTTP date) or BigCommerce's
/// `X-Rate-Limit-Time-Reset-Ms`. At most two hours.
fn cooldown(headers: &Map<String, Value>) -> Option<chrono::Duration> {
    let get = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).and_then(|(_, v)| v.as_str()).map(str::trim);
    // Values are clamped before a duration is built, so an absurd header can't overflow it.
    let ms: i64 = if let Some(ms) = get("x-rate-limit-time-reset-ms").and_then(|v| v.parse::<i64>().ok()) {
        ms
    } else {
        let v = get("retry-after")?;
        match v.parse::<i64>() {
            Ok(s) => s.clamp(0, 7200) * 1000,
            Err(_) => (DateTime::parse_from_rfc2822(v).ok()?.timestamp() - Utc::now().timestamp()).clamp(0, 7200) * 1000,
        }
    };
    Some(chrono::Duration::milliseconds(ms.clamp(1000, 7_200_000)))
}

/// Job: make the lookup a fetch intent holds, then keep the answer on that same row and queue its
/// script run. The job first claims the intent (so only one request is ever in flight for it),
/// makes room for the answer in the account's payload budget, and only then sends the request: a
/// `GET` of the checked path and query, with the pull's site, headers and credentials. An answer
/// that says "not yet" (404, 409, 425, 429, a server error) or a lost connection is tried again
/// after 1, 5, 30 and 120 minutes, or after the wait the app asked for.
pub async fn run_fetch(state: &AppState, p: &Value) -> anyhow::Result<()> {
    let Some(id) = p["delivery_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT account_id FROM integration_deliveries WHERE id = $1 AND source = 'fetch' AND status = 'pending' AND body IS NULL").bind(id).fetch_optional(&state.db).await?;
    let Some(account) = owner else { return Ok(()) };
    let Some(_pull_slot) = PullSlots::try_take(account) else {
        return requeue(state, JOB_FETCH, p.clone()).await;
    };
    // Claim it, only once it's due (a vendor's cooldown or the retry wait has passed), and reserve
    // room for a whole answer in the account's payload budget, in one transaction: the room is held
    // until the answer is stored (or the attempt fails), so nothing else can take it meanwhile.
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7021))").bind(account.to_string()).execute(&mut *tx).await?;
    let row: Option<(Uuid, Value)> = sqlx::query_as(
        "SELECT integration_id, request FROM integration_deliveries
         WHERE id = $1 AND source = 'fetch' AND status = 'pending' AND body IS NULL
           AND COALESCE(next_attempt_at, '-infinity'::timestamptz) <= now()
           AND COALESCE((request->>'claimed_until')::timestamptz, '-infinity'::timestamptz) < now()
         FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((integration_id, request)) = row else { return Ok(()) };
    let Some(integ) = model::load_any(state, integration_id).await? else { return Ok(()) };
    let mut scrub = scrub_for(state, &integ).await;
    let attempts = request["fetch"]["attempts"].as_i64().unwrap_or(0) as i32;
    let reserve = BODY_MAX_BYTES as i64;
    match make_room(&mut tx, account, reserve).await {
        Ok(()) => {}
        Err(Refused::OverBudget(msg)) => {
            tx.rollback().await?;
            return fetch_failed(state, &integ, &scrub, id, attempts, None, &msg, None, true).await;
        }
        Err(Refused::Error(e)) => return Err(anyhow::anyhow!(e.public_message())),
        Err(other) => return Err(anyhow::anyhow!("{other:?}")),
    }
    sqlx::query(
        "UPDATE integration_deliveries SET request = request || jsonb_build_object('claimed_until', now() + interval '2 minutes', 'reserved', $2::int),
             body_bytes = COALESCE((request->>'ctx_bytes')::int, body_bytes) + $2
         WHERE id = $1",
    )
    .bind(id)
    .bind(reserve as i32)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if integ.status != "enabled" {
        sqlx::query("UPDATE integration_deliveries SET status = 'ignored', error = 'the integration is turned off', message_id = NULL, processed_at = now(),
             body_bytes = COALESCE((request->>'ctx_bytes')::int, body_bytes - COALESCE((request->>'reserved')::int, 0)), request = request - 'claimed_until' - 'reserved' WHERE id = $1").bind(id).execute(&state.db).await?;
        return Ok(());
    }
    let Some((cfg, rules)) = integ.pull_config().and_then(|c| c.fetch.clone().map(|r| (c, r))) else {
        return fetch_failed(state, &integ, &scrub, id, attempts, None, "the integration no longer allows fetches", None, false).await;
    };
    if !integ.missing_credentials().is_empty() {
        return fetch_failed(state, &integ, &scrub, id, attempts, None, "the integration's credentials aren't saved yet", None, true).await;
    }
    let creds = integ.credentials(state).map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let settings = integ.settings_map();
    let st = integ.state_map();
    let now = Utc::now();
    let since = now - chrono::Duration::days(1);
    let vars = Vars { secrets: &creds, state: &st, settings: &settings, now, since, redact: false };
    let shown = Vars { secrets: &creds, state: &st, settings: &settings, now, since, redact: true };
    // Checked again against the current rules: only a GET of an allowed path and query.
    let next = match rules.allow(&open_next(state, &request["lookup_enc"]), &vars) {
        Ok(n) => n,
        Err(e) => return fetch_failed(state, &integ, &scrub, id, attempts, None, &e, None, false).await,
    };
    let base = PullConfig { method: "GET".into(), query: Map::new(), body: None, body_format: None, ..cfg.clone() };
    let (req, logged) = match (pull::build(&base, &next, &vars), pull::build(&base, &next, &shown)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return fetch_failed(state, &integ, &scrub, id, attempts, None, &e, None, false).await,
    };
    if !rules.within(&req.url, &vars) {
        return fetch_failed(state, &integ, &scrub, id, attempts, None, "the lookup isn't under the integration's fetch prefix", None, false).await;
    }
    scrub.add_request(&req);
    let origin = cfg.origin();
    let who = TokenFor { integration_id: integ.id, auth_version: integ.auth_version, origin: &origin };
    let token = match pull::oauth_token(state, &who, &cfg.auth, &vars).await {
        Ok(t) => t,
        Err(e) => return fetch_failed(state, &integ, &scrub, id, attempts, Some(logged.log_json()), &format!("couldn't get an access token: {}", e.public_message()), None, true).await,
    };
    if let Some(t) = &token {
        scrub.add(t);
    }
    let resp = match pull::send(&req, token.as_deref()).await {
        Ok(r) => r,
        Err(e) => return fetch_failed(state, &integ, &scrub, id, attempts, Some(logged.log_json()), &e.public_message(), None, true).await,
    };
    let mut made = logged.log_json();
    made["response"] = json!({"status": resp.status, "headers": resp.headers});
    if !(200..300).contains(&resp.status) {
        if token.is_some() && matches!(resp.status, 401 | 403) {
            pull::drop_token(state, integ.id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        }
        let again = matches!(resp.status, 401 | 403 | 404 | 408 | 409 | 425 | 429) || resp.status >= 500;
        let wait = if matches!(resp.status, 429 | 503) { cooldown(&resp.headers) } else { None };
        let msg = format!("{} answered HTTP {} ({})", req.place(), resp.status, pull::status_class(resp.status));
        return fetch_failed(state, &integ, &scrub, id, attempts, Some(made), &msg, wait, again).await;
    }
    if resp.body.len() > BODY_MAX_BYTES {
        return fetch_failed(state, &integ, &scrub, id, attempts, Some(made), &format!("{} answered with more than 1 MB", req.place()), None, false).await;
    }
    let ct = resp.headers.get("content-type").and_then(|v| v.as_str()).map(String::from);
    let (body, unkept) = kept_body(&integ, &scrub, ct.as_deref(), Some(&resp.body));
    let Some(body) = body.filter(|_| !unkept) else {
        return fetch_failed(state, &integ, &scrub, id, attempts, Some(made), "the answer isn't JSON or a form, so it can't be checked against the redact list", None, false).await;
    };
    // What's stored never exceeds the room reserved for it.
    if body.len() > BODY_MAX_BYTES {
        return fetch_failed(state, &integ, &scrub, id, attempts, Some(made), &format!("{} answered with more than 1 MB", req.place()), None, false).await;
    }
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7021))").bind(integ.account_id.to_string()).execute(&mut *tx).await?;
    // The answer joins the intent's row; the record can be asked for again from now on.
    let stored = sqlx::query(
        "UPDATE integration_deliveries SET body = $2, body_bytes = octet_length($2) + COALESCE((request->>'ctx_bytes')::int, 0), content_type = $3, error = NULL, next_attempt_at = NULL, message_id = NULL,
             request = (request - 'claimed_until' - 'reserved') || jsonb_build_object('made', $4::jsonb)
         WHERE id = $1 AND status = 'pending' AND body IS NULL",
    )
    .bind(id)
    .bind(&body)
    .bind(&ct)
    .bind(scrub.value(&made))
    .execute(&mut *tx)
    .await?;
    if stored.rows_affected() == 1 {
        crate::jobs::enqueue_background(&mut *tx, JOB_DELIVERY, json!({"delivery_id": id}), None).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A fetch that didn't work: noted on its row, and tried again later (after the app's own wait
/// when it gave one) while it may still work; otherwise it fails for good and the record can be
/// asked for again.
#[allow(clippy::too_many_arguments)]
async fn fetch_failed(state: &AppState, integ: &Integration, scrub: &Scrub, id: Uuid, attempts: i32, made: Option<Value>, msg: &str, wait: Option<chrono::Duration>, retry: bool) -> anyhow::Result<()> {
    let tries = attempts + 1;
    let retry_at = (retry && tries < MAX_ATTEMPTS).then(|| Utc::now() + wait.unwrap_or_else(|| chrono::Duration::minutes(BACKOFF_MINUTES[(tries as usize - 1).min(BACKOFF_MINUTES.len() - 1)])));
    let msg = scrub.text(&match retry_at {
        Some(_) => format!("fetch failed: {msg}; it will be tried again"),
        None => format!("fetch failed: {msg}"),
    });
    let patch = json!({"made": made.map(|m| scrub.value(&m))});
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE integration_deliveries SET error = $2, next_attempt_at = $3,
             status = CASE WHEN $3::timestamptz IS NULL THEN 'error' ELSE status END,
             message_id = CASE WHEN $3::timestamptz IS NULL THEN NULL ELSE message_id END,
             processed_at = CASE WHEN $3::timestamptz IS NULL THEN now() ELSE processed_at END,
             body_bytes = COALESCE((request->>'ctx_bytes')::int, body_bytes - COALESCE((request->>'reserved')::int, 0)),
             request = jsonb_set((request - 'claimed_until' - 'reserved') || jsonb_strip_nulls($4::jsonb), '{fetch,attempts}', to_jsonb($5::int))
         WHERE id = $1",
    )
    .bind(id)
    .bind(&msg)
    .bind(retry_at)
    .bind(patch)
    .bind(tries)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE integrations SET deliveries_failed = deliveries_failed + $2, last_delivery_at = now(), last_error = $3, last_error_at = now() WHERE id = $1")
        .bind(integ.id)
        .bind(i64::from(retry_at.is_none()))
        .bind(&msg)
        .execute(&mut *tx)
        .await?;
    if let Some(at) = retry_at {
        crate::jobs::enqueue_background(&mut *tx, JOB_FETCH, json!({"delivery_id": id}), Some(at)).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Scheduler: start pulls that are due, at most once per interval each, and never a second one
/// while one is waiting or running.
pub async fn schedule_pulls(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<(Uuid, Value)> = sqlx::query_as(
        "UPDATE integrations SET pull_next_at = now() + make_interval(mins => (pull->>'interval_minutes')::int)
         WHERE pull_next_at <= now() AND status = 'enabled' AND pull ? 'interval_minutes'
         RETURNING id, pull",
    )
    .fetch_all(&state.db)
    .await?;
    for (id, _) in due {
        enqueue_live_pull(state, id).await?;
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
            let freed: Option<i64> = sqlx::query_scalar(FREE_OLDEST_SQL)
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
