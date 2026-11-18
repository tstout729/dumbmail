//! Back-populate: add people who already qualify to a list-, segment- or date-triggered flow.
//!
//! The dry run counts who would be added. Starting one fixes the set of people in the background
//! job's first step (`flow_backfill_members`, at most the cap), then adds them in batches on the
//! background lane. Each person is claimed and their run started in one transaction, so a retried
//! batch adds nobody twice; re-entry rules, the profile filter and the exit condition are checked
//! for each person as they're added, and consent is checked when each message goes out, as usual.
//! One back-populate per flow runs at a time (a unique index), and at most
//! [`MAX_RUNNING_PER_ACCOUNT`] per account.
//!
//! Metric-triggered flows can't be back-populated: past events don't start flows.
//!
//! Date-triggered flows: a person is added when their date's trigger day already passed (so the
//! daily date check won't start them) but the flow still has something ahead for them. They are
//! placed where the flow would be for them now: delays already over are skipped, a delay still
//! running is joined with the time it has left, and steps whose time has passed (an email that would
//! have gone out yesterday) are skipped. Planning stops at the first split or "wait until" delay;
//! the person starts there and it's decided when they reach it.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use super::delay;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::sendtime::clock;
use crate::flows::{FlowDef, Node, Trigger};
use crate::models::FlowRow;
use crate::segments;
use crate::state::AppState;

pub const JOB: &str = "flows_plus_backfill";
pub const DEFAULT_MAX: i64 = 50_000;
pub const MAX_RUNNING_PER_ACCOUNT: i64 = 3;
const BATCH: i64 = 200;
/// The daily date check starts date-triggered flows after 9:00 local.
const DATE_TRIGGER_MINUTE: u32 = 9 * 60;

/// The most people one back-populate may add: `FLOW_BACKFILL_MAX` (default 50,000).
pub fn max_people() -> i64 {
    std::env::var("FLOW_BACKFILL_MAX").ok().and_then(|v| v.parse::<i64>().ok()).filter(|n| *n > 0).unwrap_or(DEFAULT_MAX).min(1_000_000)
}

fn node<'a>(def: &'a FlowDef, id: &str) -> Option<&'a Node> {
    def.nodes.iter().find(|n| n.id == id)
}

fn is_branch(n: &Node) -> bool {
    matches!(n.kind.as_str(), "condition" | "random_split" | "wait_for_reply")
}

/// Where a back-populated person starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub node: String,
    pub run_at: Option<DateTime<Utc>>,
    pub context: Value,
}

/// For a date flow whose trigger fired for this person at `trigger_at`: where they'd be now. None:
/// nothing in the flow is still ahead of them.
pub fn place(def: &FlowDef, trigger_at: DateTime<Utc>, now: DateTime<Utc>, tz: Tz, mut context: Value) -> Option<Placement> {
    let mut t = trigger_at;
    let mut cur = def.entry.clone();
    for _ in 0..=def.nodes.len() {
        let id = cur?;
        let n = node(def, &id)?;
        if n.kind == "delay" {
            let plan = match delay::parse(n) {
                Ok(p) => p,
                Err(_) => return Some(Placement { node: id, run_at: None, context }),
            };
            if plan.as_ref().is_some_and(|p| p.until.is_some()) {
                return Some(Placement { node: id, run_at: None, context });
            }
            let secs = delay::fixed_secs(n).unwrap_or(0.0);
            let base = t + Duration::milliseconds((secs * 1000.0) as i64);
            let end = plan.as_ref().map_or(base, |p| delay::align(p, base, tz));
            if end > now {
                context["_waited"] = json!(id);
                if plan.is_some() {
                    context["_wait_until"] = json!(end);
                    context["_wait_from"] = json!(t);
                }
                return Some(Placement { node: id, run_at: Some(end), context });
            }
            t = end;
        } else if is_branch(n) {
            return Some(Placement { node: id, run_at: None, context });
        }
        // Any other step reached before now: its time has passed, so it's skipped.
        cur = n.next().map(str::to_string);
    }
    None
}

/// How far (in days, rounded up) a date flow's plan reaches past its trigger, up to a year.
pub fn plan_days(def: &FlowDef) -> i64 {
    let mut secs = 0.0;
    let mut cur = def.entry.clone();
    for _ in 0..=def.nodes.len() {
        let Some(n) = cur.as_deref().and_then(|id| node(def, id)) else { break };
        if is_branch(n) {
            break;
        }
        if n.kind == "delay" {
            match (delay::fixed_secs(n), delay::parse(n)) {
                (Some(s), Ok(p)) => {
                    secs += s;
                    if let Some(p) = p {
                        secs += if p.weekdays.is_some() { 7.0 * 86400.0 } else if p.at.is_some() { 86400.0 } else { 0.0 };
                    }
                }
                _ => break,
            }
        }
        cur = n.next().map(str::to_string);
    }
    ((secs / 86400.0).ceil() as i64).clamp(0, 366)
}

/// The latest trigger day for a date `d` (month and day, every year) that is before `today` and
/// within `window` days of it.
pub fn missed_trigger_day(d: NaiveDate, offset_days: i64, today: NaiveDate, window: i64) -> Option<NaiveDate> {
    (today.year() - 1..=today.year() + 1)
        .filter_map(|y| NaiveDate::from_ymd_opt(y, d.month(), d.day()))
        .map(|occ| occ + Duration::days(offset_days))
        .filter(|t| *t < today && *t >= today - Duration::days(window))
        .max()
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

/// Push SQL selecting `p.id` of everyone the flow would take now (before the cap).
async fn push_candidates(state: &AppState, flow: &FlowRow, def: &FlowDef, qb: &mut QueryBuilder<'static, Postgres>) -> AppResult<()> {
    let account = crate::campaigns::load_account(&state.db, flow.account_id).await?;
    qb.push("SELECT p.id FROM profiles p WHERE p.account_id = ");
    qb.push_bind(flow.account_id);
    match crate::flows::parse_trigger(&flow.trigger)? {
        Trigger::Metric { .. } => {
            return Err(AppError::bad("metric-triggered flows can't be back-populated: past events don't start flows, only new ones do"));
        }
        Trigger::List { list_id } => {
            qb.push(" AND EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ").push_bind(list_id).push(" AND lm.profile_id = p.id)");
        }
        Trigger::Segment { segment_id } => {
            let raw: Value = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2")
                .bind(segment_id)
                .bind(flow.account_id)
                .fetch_optional(&state.db)
                .await?
                .ok_or_else(|| AppError::not_found("segment"))?;
            let seg = segments::load(&state.db, flow.account_id, &raw, Some(segment_id)).await?;
            qb.push(" AND ");
            segments::push_definition(qb, &seg);
        }
        Trigger::DateProperty { field, offset_days } => {
            let today = Utc::now().with_timezone(&crate::sending::account_tz(&account)).date_naive();
            let window = plan_days(def) + 1;
            let (mut months, mut days) = (Vec::new(), Vec::new());
            for k in 1..=window {
                let d = today - Duration::days(k) - Duration::days(offset_days);
                months.push(d.month() as i32);
                days.push(d.day() as i32);
            }
            qb.push(" AND (extract(month FROM pl_ts(p.properties->>").push_bind(field.clone()).push("))::int, extract(day FROM pl_ts(p.properties->>").push_bind(field);
            qb.push("))::int) IN (SELECT * FROM unnest(").push_bind(months).push("::int[], ").push_bind(days).push("::int[]))");
            // The date check's own rule: at most once in 300 days.
            qb.push(" AND NOT EXISTS (SELECT 1 FROM flow_runs r3 WHERE r3.flow_id = ").push_bind(flow.id).push(" AND r3.profile_id = p.id AND r3.started_at > now() - interval '300 days')");
        }
    }
    if let Some(f) = &def.settings.profile_filter {
        let f = segments::load(&state.db, flow.account_id, f, None).await?;
        qb.push(" AND ");
        segments::push_definition(qb, &f);
    }
    if let Some(x) = &def.settings.exit_condition {
        let x = segments::load(&state.db, flow.account_id, x, None).await?;
        qb.push(" AND NOT ");
        segments::push_definition(qb, &x);
    }
    push_not_blocked(qb, flow.id, def);
    Ok(())
}

fn trigger_kind(flow: &FlowRow) -> &'static str {
    match crate::flows::parse_trigger(&flow.trigger) {
        Ok(Trigger::List { .. }) => "list",
        Ok(Trigger::Segment { .. }) => "segment",
        Ok(Trigger::DateProperty { .. }) => "date_property",
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
    if !confirm {
        let mut qb: QueryBuilder<'static, Postgres> = QueryBuilder::new("SELECT count(*) FROM (");
        push_candidates(state, &flow, &def, &mut qb).await?;
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
            "note": if trigger_kind(&flow) == "date_property" {
                "Counts people whose trigger day already passed and who still have a step ahead; some may have nothing left when they're placed."
            } else {
                "Counts people who qualify now, after the flow's re-entry rules, profile filter and exit condition. Consent is checked when each message goes out."
            },
            "confirm": "Call again with confirm: true to add them.",
        }));
    }
    if flow.status != "live" {
        return Err(AppError::Conflict("turn the flow on before adding people to it".into()));
    }
    // Refuses metric triggers and missing segments before anything is recorded.
    push_candidates(state, &flow, &def, &mut QueryBuilder::new("")).await?;
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

/// Job: one step of a back-populate (fix who's in it, then one batch at a time).
pub async fn run(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["backfill_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let row: Option<(Uuid, String, i32, Option<DateTime<Utc>>)> = sqlx::query_as("SELECT flow_id, status, max_people, snapshot_at FROM flow_backfills WHERE id = $1").bind(id).fetch_optional(&state.db).await?;
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
    if snapshot_at.is_none() {
        let mut qb: QueryBuilder<'static, Postgres> = QueryBuilder::new("INSERT INTO flow_backfill_members (backfill_id, profile_id) SELECT ");
        qb.push_bind(id).push(", c.id FROM (");
        match push_candidates(state, &flow, &def, &mut qb).await {
            Ok(()) => {}
            Err(e @ (AppError::BadRequest(_) | AppError::NotFound(_))) => return Ok(stop(state, id, "failed", Some(&e.public_message())).await?),
            Err(e) => return Err(anyhow::anyhow!(e.public_message())),
        }
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
    let todo: Vec<Uuid> = sqlx::query_scalar("SELECT profile_id FROM flow_backfill_members WHERE backfill_id = $1 AND done_at IS NULL ORDER BY profile_id LIMIT $2")
        .bind(id)
        .bind(BATCH)
        .fetch_all(&state.db)
        .await?;
    let account = crate::campaigns::load_account(&state.db, flow.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let account_tz = crate::sending::account_tz(&account);
    let trigger = crate::flows::parse_trigger(&flow.trigger).map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let base_context = match &trigger {
        Trigger::List { list_id } => json!({"list_id": list_id, "backfill_id": id}),
        Trigger::Segment { segment_id } => json!({"segment_id": segment_id, "backfill_id": id}),
        Trigger::DateProperty { field, .. } => json!({"date_field": field, "backfill_id": id}),
        Trigger::Metric { .. } => return Ok(stop(state, id, "failed", Some("metric-triggered flows can't be back-populated")).await?),
    };
    let filter = match &def.settings.profile_filter {
        Some(f) => Some(segments::load(&state.db, flow.account_id, f, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?),
        None => None,
    };
    let exit = match &def.settings.exit_condition {
        Some(f) => Some(segments::load(&state.db, flow.account_id, f, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?),
        None => None,
    };
    let now = Utc::now();
    for pid in &todo {
        // Who this person is now: still qualifying and, for date flows, where they'd be.
        let mut skip: Option<&str> = None;
        if let Some(f) = &filter {
            if !segments::profile_matches(&state.db, *pid, f).await.map_err(|e| anyhow::anyhow!(e.public_message()))? {
                skip = Some("skipped: profile filter");
            }
        }
        if let (None, Some(x)) = (skip, &exit) {
            if segments::profile_matches(&state.db, *pid, x).await.map_err(|e| anyhow::anyhow!(e.public_message()))? {
                skip = Some("skipped: exit condition");
            }
        }
        let placement = match (&trigger, skip) {
            (_, Some(_)) => None,
            (Trigger::DateProperty { field, offset_days }, None) => {
                let row: Option<(Value, Option<String>)> = sqlx::query_as("SELECT properties, phone_number FROM profiles WHERE id = $1").bind(pid).fetch_optional(&state.db).await?;
                let Some((props, phone)) = row else { continue };
                let today = now.with_timezone(&account_tz).date_naive();
                let window = plan_days(&def) + 1;
                let day = props.get(field).and_then(|v| delay::parse_when(v, account_tz)).map(|t| t.with_timezone(&account_tz).date_naive());
                let tz = crate::features::sendtime::zone::resolve_parts(&props, phone.as_deref(), account_tz).0;
                let p = day
                    .and_then(|d| missed_trigger_day(d, *offset_days, today, window))
                    .and_then(|t| place(&def, clock::local_instant(account_tz, t, DATE_TRIGGER_MINUTE), now, tz, base_context.clone()));
                if p.is_none() {
                    skip = Some("skipped: nothing left in the flow for their date");
                }
                p
            }
            (_, None) => def.entry.clone().map(|e| Placement { node: e, run_at: None, context: base_context.clone() }),
        };
        let mut tx = state.db.begin().await?;
        let claimed = sqlx::query(
            "UPDATE flow_backfill_members m SET done_at = now(), outcome = 'claimed' WHERE m.backfill_id = $1 AND m.profile_id = $2 AND m.done_at IS NULL
               AND EXISTS (SELECT 1 FROM flow_backfills b WHERE b.id = $1 AND b.status = 'running')",
        )
        .bind(id)
        .bind(pid)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if claimed == 0 {
            // Cancelled, or another attempt got here first.
            tx.rollback().await?;
            continue;
        }
        let (outcome, run_id) = match placement {
            Some(p) => match crate::flows::enter(&mut tx, &flow, &def, *pid, &p.node, &p.context, p.run_at).await.map_err(|e| anyhow::anyhow!(e.public_message()))? {
                Some(run) => ("added", Some(run)),
                None => ("skipped: already in the flow (re-entry rules)", None),
            },
            None => (skip.unwrap_or("skipped"), None),
        };
        sqlx::query("UPDATE flow_backfill_members SET outcome = $3, run_id = $4 WHERE backfill_id = $1 AND profile_id = $2").bind(id).bind(pid).bind(outcome).bind(run_id).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    let mut tx = state.db.begin().await?;
    let (left,): (i64,) = sqlx::query_as(
        "WITH c AS (SELECT count(*) FILTER (WHERE outcome = 'added') AS added, count(*) FILTER (WHERE done_at IS NOT NULL AND outcome <> 'added') AS skipped,
                           count(*) FILTER (WHERE done_at IS NULL) AS left FROM flow_backfill_members WHERE backfill_id = $1)
         UPDATE flow_backfills b SET added = c.added, skipped = c.skipped FROM c WHERE b.id = $1 RETURNING c.left",
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
