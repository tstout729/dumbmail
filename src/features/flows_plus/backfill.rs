//! Back-populate: add people who already qualify to a list- or segment-triggered flow.
//!
//! The dry run counts who would be added. Starting one fixes the set of people in the background
//! job's first step (`flow_backfill_members`, at most the cap), then adds them in paced batches on
//! the background lane. One back-populate per flow runs at a time (a unique index), and at most
//! [`MAX_RUNNING_PER_ACCOUNT`] per account.
//!
//! Each person is enrolled in one transaction that takes, in this order: the profile (so deleting it
//! waits, or it's already gone and the person is skipped), the back-populate's own row (so a cancel
//! that returned can't be followed by another enrollment), and the person's claim. Then it checks
//! that they are still in the trigger list or segment, still pass the profile filter and don't match
//! the exit condition, and starts their run under the flow's re-entry rules. A retried batch adds
//! nobody twice. Consent is checked when each message goes out, as usual.
//!
//! Pacing: each batch enrols only as many as there is room for: at most [`FLOW_CAP`] of a flow's
//! back-populated runs, and [`server_cap`] across all flows on the server, may be waiting for their
//! first step at once (however long they've waited), so
//! back-populated first steps never crowd out newer sends.
//!
//! Metric- and date-triggered flows can't be back-populated: past events don't start flows, and
//! people whose date is still ahead enter on their own when it arrives.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::flows::{FlowDef, Trigger};
use crate::models::FlowRow;
use crate::segments;
use crate::state::AppState;

pub const JOB: &str = "flows_plus_backfill";
pub const DEFAULT_MAX: i64 = 50_000;
pub const MAX_RUNNING_PER_ACCOUNT: i64 = 3;
/// The most of one flow's back-populated runs waiting for their first step at once.
pub const FLOW_CAP: i64 = 200;
/// The most of one account's, so no account can fill the server's room alone.
pub const ACCOUNT_CAP: i64 = 400;
/// The most back-populated runs waiting for their first step across all flows on the server:
/// `FLOW_BACKFILL_SERVER_CAP` (default 1,000).
pub fn server_cap() -> i64 {
    std::env::var("FLOW_BACKFILL_SERVER_CAP").ok().and_then(|v| v.parse::<i64>().ok()).filter(|n| *n > 0).unwrap_or(1_000)
}
const DATE_REFUSED: &str = "date-triggered flows can't be back-populated: people whose date is still ahead will enter on their own when it arrives";
const METRIC_REFUSED: &str = "metric-triggered flows can't be back-populated: past events don't start flows, only new ones do";

/// The most people one back-populate may add: `FLOW_BACKFILL_MAX` (default 50,000).
pub fn max_people() -> i64 {
    std::env::var("FLOW_BACKFILL_MAX").ok().and_then(|v| v.parse::<i64>().ok()).filter(|n| *n > 0).unwrap_or(DEFAULT_MAX).min(1_000_000)
}

/// Test hook: hold a back-populate's enrollment after it has read everything the run insert depends
/// on and just before the insert (holding the profile, the back-populate's row, the entry lock, the
/// flow and the list membership), so a test can land a delete, a cancel, a list removal or a trigger
/// change exactly there. Never set outside tests.
#[doc(hidden)]
pub struct EnrollPause {
    arrived: tokio::sync::Semaphore,
    go: tokio::sync::Semaphore,
}

static PAUSES: Lazy<Mutex<HashMap<Uuid, Arc<EnrollPause>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

impl EnrollPause {
    #[doc(hidden)]
    pub fn hold(backfill_id: Uuid) -> Arc<Self> {
        let p = Arc::new(Self { arrived: tokio::sync::Semaphore::new(0), go: tokio::sync::Semaphore::new(0) });
        if let Ok(mut m) = PAUSES.lock() {
            m.insert(backfill_id, p.clone());
        }
        p
    }

    /// Wait until an enrollment is being held.
    pub async fn arrived(&self) {
        if let Ok(permit) = self.arrived.acquire().await {
            permit.forget();
        }
    }

    /// Let it go, and stop holding this back-populate.
    pub fn release(&self, backfill_id: Uuid) {
        if let Ok(mut m) = PAUSES.lock() {
            m.remove(&backfill_id);
        }
        self.go.add_permits(1000);
    }
}

async fn enroll_pause(backfill_id: Uuid) {
    let pause = PAUSES.lock().ok().and_then(|m| m.get(&backfill_id).cloned());
    if let Some(p) = pause {
        p.arrived.add_permits(1);
        if let Ok(permit) = p.go.acquire().await {
            permit.forget();
        }
    }
}

fn push_not_blocked(qb: &mut QueryBuilder<'_, Postgres>, flow_id: Uuid, def: &FlowDef) {
    let reentry = def.settings.reentry.clone().unwrap_or(json!("never"));
    qb.push(" AND NOT EXISTS (SELECT 1 FROM flow_runs r WHERE r.flow_id = ").push_bind(flow_id).push(" AND r.profile_id = p.id");
    match (&reentry, reentry.get("after_days").and_then(Value::as_f64)) {
        (Value::String(s), _) if s == "always" => {
            qb.push(" AND r.status = 'active'");
        }
        (_, Some(days)) => {
            qb.push(" AND (r.status = 'active' OR r.started_at > now() - make_interval(secs => ").push_bind(days * 86400.0).push("))");
        }
        _ => {}
    }
    qb.push(")");
}

/// What a back-populate checks about each person: the trigger, the filter and the exit condition.
struct Rules {
    trigger: Trigger,
    segment: Option<segments::Definition>,
    filter: Option<segments::Definition>,
    exit: Option<segments::Definition>,
}

async fn rules(state: &AppState, flow: &FlowRow, def: &FlowDef) -> AppResult<Rules> {
    let trigger = crate::flows::parse_trigger(&flow.trigger)?;
    let segment = match &trigger {
        Trigger::Metric { .. } => return Err(AppError::bad(METRIC_REFUSED)),
        Trigger::DateProperty { .. } | Trigger::EventDate { .. } => return Err(AppError::bad(DATE_REFUSED)),
        Trigger::List { .. } => None,
        Trigger::Segment { segment_id } => {
            let raw: Value = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2")
                .bind(segment_id)
                .bind(flow.account_id)
                .fetch_optional(&state.db)
                .await?
                .ok_or_else(|| AppError::not_found("segment"))?;
            Some(segments::load(&state.db, flow.account_id, &raw, Some(*segment_id)).await?)
        }
    };
    let load = |raw: &Option<Value>| {
        let raw = raw.clone();
        async move {
            match raw {
                Some(r) => segments::load(&state.db, flow.account_id, &r, None).await.map(Some),
                None => Ok(None),
            }
        }
    };
    Ok(Rules { trigger, segment, filter: load(&def.settings.profile_filter).await?, exit: load(&def.settings.exit_condition).await? })
}

/// Push an SQL condition on profile `p`: in the trigger list or segment now.
fn push_in_trigger(qb: &mut QueryBuilder<'static, Postgres>, r: &Rules) {
    match (&r.trigger, &r.segment) {
        (Trigger::List { list_id }, _) => {
            qb.push("EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ").push_bind(*list_id).push(" AND lm.profile_id = p.id)");
        }
        (_, Some(seg)) => segments::push_definition(qb, seg),
        _ => {
            qb.push("FALSE");
        }
    }
}

/// Push SQL selecting `p.id` of everyone the flow would take now (before the cap).
fn push_candidates(flow: &FlowRow, def: &FlowDef, r: &Rules, qb: &mut QueryBuilder<'static, Postgres>) {
    qb.push("SELECT p.id FROM profiles p WHERE p.account_id = ").push_bind(flow.account_id).push(" AND ");
    push_in_trigger(qb, r);
    if let Some(f) = &r.filter {
        qb.push(" AND ");
        segments::push_definition(qb, f);
    }
    if let Some(x) = &r.exit {
        qb.push(" AND NOT ");
        segments::push_definition(qb, x);
    }
    push_not_blocked(qb, flow.id, def);
}

fn trigger_kind(flow: &FlowRow) -> &'static str {
    match crate::flows::parse_trigger(&flow.trigger) {
        Ok(Trigger::List { .. }) => "list",
        Ok(Trigger::Segment { .. }) => "segment",
        Ok(Trigger::DateProperty { .. }) => "date_property",
        Ok(Trigger::EventDate { .. }) => "event_date",
        _ => "metric",
    }
}

pub fn to_json(r: &Row) -> Value {
    json!({"id": r.id, "flow_id": r.flow_id, "status": r.status, "max_people": r.max_people, "total": r.total, "added": r.added, "skipped": r.skipped,
           "error": r.error, "created_at": r.created_at, "finished_at": r.finished_at})
}

#[derive(sqlx::FromRow, Debug)]
pub struct Row {
    pub id: Uuid,
    pub flow_id: Uuid,
    pub status: String,
    pub max_people: i32,
    pub total: Option<i32>,
    pub added: i32,
    pub skipped: i32,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

const COLS: &str = "id, flow_id, status, max_people, total, added, skipped, error, created_at, finished_at";

pub async fn list(state: &AppState, account_id: Uuid, flow_id: Uuid) -> AppResult<Vec<Value>> {
    crate::api::messaging::load_flow(state, account_id, flow_id).await?;
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {COLS} FROM flow_backfills WHERE flow_id = $1 AND account_id = $2 ORDER BY created_at DESC LIMIT 20"))
        .bind(flow_id)
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    Ok(rows.iter().map(to_json).collect())
}

/// Count who would be added (`confirm` false), or start adding them. `limit` lowers the cap.
pub async fn start(state: &AppState, ctx: &Ctx, flow_id: Uuid, confirm: bool, limit: Option<i64>) -> AppResult<Value> {
    let flow = crate::api::messaging::load_flow(state, ctx.account_id, flow_id).await?;
    let def = crate::flows::parse_definition(&flow.definition)?;
    let max = limit.unwrap_or(i64::MAX).clamp(1, max_people());
    let r = rules(state, &flow, &def).await?;
    if !confirm {
        let mut qb: QueryBuilder<'static, Postgres> = QueryBuilder::new("SELECT count(*) FROM (");
        push_candidates(&flow, &def, &r, &mut qb);
        qb.push(" LIMIT ").push_bind(max + 1).push(") x");
        let mut tx = state.db.begin().await?;
        sqlx::query("SET LOCAL statement_timeout = '15s'").execute(&mut *tx).await?;
        let n: i64 = qb.build_query_scalar().fetch_one(&mut *tx).await?;
        tx.commit().await?;
        let running: Option<Row> = sqlx::query_as(&format!("SELECT {COLS} FROM flow_backfills WHERE flow_id = $1 AND status = 'running'")).bind(flow.id).fetch_optional(&state.db).await?;
        return Ok(json!({
            "dry_run": true, "trigger": trigger_kind(&flow), "flow_status": flow.status,
            "would_add": n.min(max), "more_than_max": n > max, "max_people": max,
            "running": running.as_ref().map(to_json),
            "note": "Counts people who qualify now, after the flow's re-entry rules, profile filter and exit condition. Each is checked again as they're added, and consent is checked when each message goes out.",
            "confirm": "Call again with confirm: true to add them.",
        }));
    }
    if flow.status != "live" {
        return Err(AppError::Conflict("turn the flow on before adding people to it".into()));
    }
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT 1 FROM accounts WHERE id = $1 FOR UPDATE").bind(ctx.account_id).execute(&mut *tx).await?;
    let running: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_backfills WHERE account_id = $1 AND status = 'running'").bind(ctx.account_id).fetch_one(&mut *tx).await?;
    if running >= MAX_RUNNING_PER_ACCOUNT {
        return Err(AppError::Conflict(format!("{MAX_RUNNING_PER_ACCOUNT} flows are already adding people; wait for one to finish")));
    }
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO flow_backfills (id, account_id, flow_id, max_people, created_by) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (flow_id) WHERE status = 'running' DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(ctx.account_id)
    .bind(flow.id)
    .bind(max as i32)
    .bind(ctx.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        return Err(AppError::Conflict("this flow is already adding people; wait for it to finish or cancel it".into()));
    }
    crate::jobs::enqueue_background(&mut *tx, JOB, json!({"backfill_id": id}), None).await?;
    tx.commit().await?;
    crate::features::team::audit(state, ctx, "flow.backfill_start", Some(&flow.id.to_string()), json!({"flow": flow.name, "backfill_id": id, "max_people": max})).await;
    let row: Row = sqlx::query_as(&format!("SELECT {COLS} FROM flow_backfills WHERE id = $1")).bind(id).fetch_one(&state.db).await?;
    Ok(to_json(&row))
}

pub async fn cancel(state: &AppState, ctx: &Ctx, flow_id: Uuid, backfill_id: Uuid) -> AppResult<Value> {
    crate::api::messaging::load_flow(state, ctx.account_id, flow_id).await?;
    let row: Option<Row> = sqlx::query_as(&format!(
        "UPDATE flow_backfills SET status = 'cancelled', finished_at = now(), error = 'cancelled' WHERE id = $1 AND flow_id = $2 AND account_id = $3 AND status = 'running' RETURNING {COLS}"
    ))
    .bind(backfill_id)
    .bind(flow_id)
    .bind(ctx.account_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flow_backfills WHERE id = $1 AND flow_id = $2 AND account_id = $3)")
            .bind(backfill_id)
            .bind(flow_id)
            .bind(ctx.account_id)
            .fetch_one(&state.db)
            .await?;
        return Err(if exists { AppError::Conflict("it already finished".into()) } else { AppError::not_found("back-populate") });
    };
    crate::features::team::audit(state, ctx, "flow.backfill_cancel", Some(&flow_id.to_string()), json!({"backfill_id": backfill_id, "added": row.added})).await;
    Ok(to_json(&row))
}

async fn stop(state: &AppState, id: Uuid, status: &str, why: Option<&str>) -> sqlx::Result<()> {
    sqlx::query("UPDATE flow_backfills SET status = $2, error = $3, finished_at = now() WHERE id = $1 AND status = 'running'").bind(id).bind(status).bind(why).execute(&state.db).await?;
    Ok(())
}

/// The job ran out of retries (called in the transaction that marks the job dead): the
/// back-populate failed, with the last error, and the flow can start another.
pub async fn job_dead_in(tx: &mut sqlx::PgConnection, payload: &Value, error: &str) -> sqlx::Result<()> {
    let Some(id) = payload["backfill_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    // Only a short category is shown; the full error stays with the job and in the logs.
    let e = error.to_ascii_lowercase();
    let why = if e.contains("timeout") || e.contains("canceling statement") {
        "the database took too long"
    } else if e.contains("database") || e.contains("relation") || e.contains("column") {
        "a database error"
    } else {
        "an unexpected error"
    };
    sqlx::query("UPDATE flow_backfills SET status = 'failed', error = $2, finished_at = now() WHERE id = $1 AND status = 'running'")
        .bind(id)
        .bind(format!("stopped after repeated errors ({why}); start it again to add the rest"))
        .execute(tx)
        .await?;
    Ok(())
}

/// Job: one step of a back-populate (fix who's in it, then one paced batch at a time).
pub async fn run(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["backfill_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let row: Option<(Uuid, String, i32, Option<DateTime<Utc>>)> =
        sqlx::query_as("SELECT flow_id, status, max_people, snapshot_at FROM flow_backfills WHERE id = $1").bind(id).fetch_optional(&state.db).await?;
    let Some((flow_id, status, max, snapshot_at)) = row else { return Ok(()) };
    if status != "running" {
        return Ok(());
    }
    let Some(flow) = sqlx::query_as::<_, FlowRow>("SELECT * FROM flows WHERE id = $1").bind(flow_id).fetch_optional(&state.db).await? else { return Ok(()) };
    match flow.status.as_str() {
        "live" => {}
        "paused" => {
            crate::jobs::enqueue_background(&state.db, JOB, json!({"backfill_id": id}), Some(Utc::now() + Duration::minutes(15))).await?;
            return Ok(());
        }
        _ => return Ok(stop(state, id, "cancelled", Some("the flow was turned off")).await?),
    }
    let def = match crate::flows::parse_definition(&flow.definition) {
        Ok(d) => d,
        Err(e) => return Ok(stop(state, id, "failed", Some(&e.public_message())).await?),
    };
    let r = match rules(state, &flow, &def).await {
        Ok(r) => r,
        Err(e @ (AppError::BadRequest(_) | AppError::NotFound(_))) => return Ok(stop(state, id, "failed", Some(&e.public_message())).await?),
        Err(e) => return Err(anyhow::anyhow!(e.public_message())),
    };
    if snapshot_at.is_none() {
        let mut qb: QueryBuilder<'static, Postgres> = QueryBuilder::new("INSERT INTO flow_backfill_members (backfill_id, profile_id) SELECT ");
        qb.push_bind(id).push(", c.id FROM (");
        push_candidates(&flow, &def, &r, &mut qb);
        qb.push(" ORDER BY p.id LIMIT ").push_bind(i64::from(max)).push(") c ON CONFLICT DO NOTHING");
        let mut tx = state.db.begin().await?;
        sqlx::query("SET LOCAL statement_timeout = '120s'").execute(&mut *tx).await?;
        let total = qb.build().execute(&mut *tx).await?.rows_affected();
        let fixed = sqlx::query("UPDATE flow_backfills SET snapshot_at = now(), total = (SELECT count(*) FROM flow_backfill_members WHERE backfill_id = $1) WHERE id = $1 AND status = 'running' AND snapshot_at IS NULL")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if fixed == 1 {
            crate::jobs::enqueue_background(&mut *tx, JOB, json!({"backfill_id": id}), None).await?;
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        tracing::info!(backfill = %id, flow = %flow.id, people = total, "back-populate: people fixed");
        return Ok(());
    }
    // Pacing: enrol only as many as there is room for, for this flow and across the server.
    // Counted: runs still waiting for their first step in live flows of open accounts (a paused flow
    // or a closed account isn't taking anyone's room).
    let (this_flow, this_account, all): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE m.backfill_id = $1), count(*) FILTER (WHERE b.account_id = $2), count(*) FROM flow_backfill_members m
         JOIN flow_backfills b ON b.id = m.backfill_id AND (b.status = 'running' OR b.finished_at > now() - interval '1 day')
         JOIN flow_runs r ON r.id = m.run_id
         JOIN flows f ON f.id = r.flow_id AND f.status = 'live'
         JOIN accounts a ON a.id = b.account_id AND a.closed_at IS NULL
         WHERE m.done_at IS NOT NULL AND r.status = 'active' AND r.visited = '[]'::jsonb",
    )
    .bind(id)
    .bind(flow.account_id)
    .fetch_one(&state.db)
    .await?;
    let room = (FLOW_CAP - this_flow).min(ACCOUNT_CAP - this_account).min(server_cap() - all);
    if room <= 0 {
        crate::jobs::enqueue_background(&state.db, JOB, json!({"backfill_id": id}), Some(Utc::now() + Duration::seconds(15))).await?;
        return Ok(());
    }
    let todo: Vec<Uuid> = sqlx::query_scalar("SELECT profile_id FROM flow_backfill_members WHERE backfill_id = $1 AND done_at IS NULL ORDER BY profile_id LIMIT $2")
        .bind(id)
        .bind(room)
        .fetch_all(&state.db)
        .await?;
    let base_context = match &r.trigger {
        Trigger::List { list_id } => json!({"list_id": list_id, "backfill_id": id}),
        Trigger::Segment { segment_id } => json!({"segment_id": segment_id, "backfill_id": id}),
        _ => return Ok(()),
    };
    let Some(entry) = def.entry.clone() else { return Ok(stop(state, id, "failed", Some("the flow has no steps")).await?) };
    for pid in &todo {
        if !enroll(state, id, &flow, &def, &r, *pid, &entry, &base_context).await? {
            break;
        }
    }
    let mut tx = state.db.begin().await?;
    // A profile deleted meanwhile takes its member row with it; it counts as skipped.
    let (left,): (i64,) = sqlx::query_as(
        "WITH c AS (SELECT count(*) FILTER (WHERE outcome = 'added') AS added, count(*) FILTER (WHERE done_at IS NULL) AS left FROM flow_backfill_members WHERE backfill_id = $1)
         UPDATE flow_backfills b SET added = c.added, skipped = GREATEST(COALESCE(b.total, 0) - c.added - c.left, 0) FROM c WHERE b.id = $1 RETURNING c.left",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if left == 0 {
        sqlx::query("UPDATE flow_backfills SET status = 'done', finished_at = now() WHERE id = $1 AND status = 'running'").bind(id).execute(&mut *tx).await?;
    } else {
        crate::jobs::enqueue_background(&mut *tx, JOB, json!({"backfill_id": id}), None).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Enroll one person (see the module docs for the order of locks). False: the back-populate is no
/// longer running, so stop the batch.
#[allow(clippy::too_many_arguments)]
async fn enroll(state: &AppState, id: Uuid, flow: &FlowRow, def: &FlowDef, r: &Rules, pid: Uuid, entry: &str, context: &Value) -> anyhow::Result<bool> {
    let mut tx = state.db.begin().await?;
    // 1. The profile: a delete or a change to it (which could move them out of a segment) waits
    //    for this enrollment, or finished first and is seen below.
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM profiles WHERE id = $1 FOR SHARE").bind(pid).fetch_optional(&mut *tx).await?;
    if exists.is_none() {
        tx.rollback().await?;
        return Ok(true);
    }
    // 2. The back-populate's own row: a cancel waits for this enrollment, or this one sees it.
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM flow_backfills WHERE id = $1 FOR SHARE").bind(id).fetch_optional(&mut *tx).await?;
    if status.as_deref() != Some("running") {
        tx.rollback().await?;
        return Ok(false);
    }
    // 3. The claim.
    let claimed = sqlx::query("UPDATE flow_backfill_members SET done_at = now(), outcome = 'claimed' WHERE backfill_id = $1 AND profile_id = $2 AND done_at IS NULL")
        .bind(id)
        .bind(pid)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if claimed == 0 {
        tx.rollback().await?;
        return Ok(true);
    }
    // 4. The entry lock (the one `flows::enter` takes), then everything the insert depends on is
    //    read after it, in this transaction, and held until the run is written.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || $2::text, 7))").bind(flow.id).bind(pid).execute(&mut *tx).await?;
    let now: Option<(String, Value)> = sqlx::query_as("SELECT status, trigger FROM flows WHERE id = $1 FOR SHARE").bind(flow.id).fetch_optional(&mut *tx).await?;
    let changed = match &now {
        None => Some("the flow was deleted"),
        Some((st, _)) if st != "live" => Some("the flow was turned off"),
        Some((_, t)) if *t != flow.trigger => Some("the flow's trigger changed"),
        _ => None,
    };
    if let Some(why) = changed {
        sqlx::query("UPDATE flow_backfill_members SET outcome = $3 WHERE backfill_id = $1 AND profile_id = $2").bind(id).bind(pid).bind(format!("skipped: {why}")).execute(&mut *tx).await?;
        sqlx::query("UPDATE flow_backfills SET status = 'cancelled', error = $2, finished_at = now() WHERE id = $1 AND status = 'running'").bind(id).bind(why).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(false);
    }
    // A list membership is locked, so taking them off the list waits for this enrollment.
    if let Trigger::List { list_id } = &r.trigger {
        sqlx::query("SELECT 1 FROM list_members WHERE list_id = $1 AND profile_id = $2 FOR KEY SHARE").bind(list_id).bind(pid).execute(&mut *tx).await?;
    }
    // Still in the trigger list or segment, still passing the filter, not matching the exit condition.
    let mut qb: QueryBuilder<'static, Postgres> = QueryBuilder::new("SELECT ");
    push_in_trigger(&mut qb, r);
    qb.push(", ");
    match &r.filter {
        Some(f) => segments::push_definition(&mut qb, f),
        None => {
            qb.push("TRUE");
        }
    }
    qb.push(", ");
    match &r.exit {
        Some(x) => segments::push_definition(&mut qb, x),
        None => {
            qb.push("FALSE");
        }
    }
    qb.push(" FROM profiles p WHERE p.id = ").push_bind(pid);
    let (in_trigger, filter_ok, exited): (bool, bool, bool) = qb.build_query_as().fetch_one(&mut *tx).await?;
    enroll_pause(id).await;
    let (outcome, run_id) = if !in_trigger {
        ("skipped: no longer in the trigger list or segment", None)
    } else if !filter_ok {
        ("skipped: profile filter", None)
    } else if exited {
        ("skipped: exit condition", None)
    } else {
        match crate::flows::enter(&mut tx, flow, def, pid, entry, context, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))? {
            Some(run) => ("added", Some(run)),
            None => ("skipped: already in the flow (re-entry rules)", None),
        }
    };
    sqlx::query("UPDATE flow_backfill_members SET outcome = $3, run_id = $4 WHERE backfill_id = $1 AND profile_id = $2").bind(id).bind(pid).bind(outcome).bind(run_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
