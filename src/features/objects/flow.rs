//! Flows off objects.
//!
//! - `object` triggers: when an object of a type is created or updated (optionally only when one
//!   field changed, or changed to a value), for each person linked to it. The change record's job
//!   claims (change, flow) in the transaction that starts the runs, so a retried job starts nothing
//!   twice. Re-entry rules count runs for the same object only: a guest with two reservations goes
//!   through the flow for each.
//! - `object_date` triggers: N days (at a local hour) or N hours before or after a date field of
//!   each object. They run on the event-date machinery in `flows` (the same scheduler tick, catch-up
//!   window, missed rule and claim-and-start transaction), with fires kept per (flow, object, due
//!   moment) in `flow_object_date_fires`. A date that moves drops the pending fire and schedules the
//!   new moment; a moment that already fired never fires again.
//! - Before each step of an object run, [`check_run`] makes the person leave when the object was
//!   deleted, they were unlinked, its date moved (a new fire is scheduled) or it no longer matches
//!   the trigger's filters (e.g. its status became cancelled). The object's current fields are what
//!   emails and texts see as `object.*`.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::flows::{self, Trigger};
use crate::models::FlowRow;
use crate::segments::{self, Filter, PropOp};
use crate::state::AppState;

pub const JOB_CHANGE: &str = "objects_change";

/// The account's time zone name (Los Angeles when unset or unknown).
pub async fn account_tz(db: &PgPool, account_id: Uuid) -> AppResult<String> {
    let tz: Option<String> = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_optional(db).await?;
    Ok(tz.and_then(|t| t.parse::<chrono_tz::Tz>().ok()).unwrap_or(chrono_tz::America::Los_Angeles).name().to_string())
}

fn tz_of(name: &str) -> chrono_tz::Tz {
    name.parse().unwrap_or(chrono_tz::America::Los_Angeles)
}

// ---------- Trigger checks ----------

/// Checks on an object trigger's shape (called from `flows::parse_trigger`).
pub fn validate_trigger(t: &Trigger) -> AppResult<()> {
    let key_ok = |k: &str| super::types::valid_type_key(k.trim());
    let field_ok = |k: &str| super::types::valid_field_key(k.trim());
    let rel_ok = |r: &Option<String>| r.as_deref().is_none_or(|r| !r.trim().is_empty() && r.chars().count() <= super::store::MAX_RELATIONSHIP);
    let filters_ok = |f: &[Filter]| -> AppResult<()> {
        if f.len() > 20 {
            return Err(AppError::bad("an object trigger takes at most 20 filters"));
        }
        if f.iter().any(|x| !field_ok(&x.field)) {
            return Err(AppError::bad("object trigger filters name one of the type's fields"));
        }
        segments::parse(&json!({"conditions": f.iter().map(|x| json!({"type": "property", "field": x.field, "op": x.op, "value": x.value})).collect::<Vec<_>>()}))?;
        Ok(())
    };
    match t {
        Trigger::Object { object_type, on, field, to, filters, relationship } => {
            if !key_ok(object_type) {
                return Err(AppError::bad("an object trigger needs the object type's key (e.g. reservation)"));
            }
            if on.as_deref().is_some_and(|o| !matches!(o, "created" | "updated" | "any")) {
                return Err(AppError::bad("`on` is created, updated or any"));
            }
            if field.as_deref().is_some_and(|f| !field_ok(f)) {
                return Err(AppError::bad("`field` must be one of the type's field keys"));
            }
            if to.is_some() && field.is_none() {
                return Err(AppError::bad("`to` goes with `field` (the field that changed to this value)"));
            }
            if to.as_ref().is_some_and(|v| v.is_object() || v.is_array()) {
                return Err(AppError::bad("`to` is a plain value"));
            }
            if !rel_ok(relationship) {
                return Err(AppError::bad("relationship is 1-50 characters (e.g. guest)"));
            }
            filters_ok(filters)
        }
        Trigger::ObjectDate { object_type, field, offset_days, offset_hours, hour, filters, relationship } => {
            if !key_ok(object_type) || !field_ok(field) {
                return Err(AppError::bad("an object date trigger needs the object type's key and its date field"));
            }
            if !(-365..=365).contains(offset_days) {
                return Err(AppError::bad("offset_days must be -365 to 365"));
            }
            if let Some(h) = offset_hours {
                if !(-8760..=8760).contains(h) {
                    return Err(AppError::bad("offset_hours must be -8760 to 8760"));
                }
                if *offset_days != 0 || hour.is_some() {
                    return Err(AppError::bad("use offset_hours on its own, or offset_days with an hour"));
                }
            }
            if hour.is_some_and(|h| h > 23) {
                return Err(AppError::bad("hour must be 0-23"));
            }
            if !rel_ok(relationship) {
                return Err(AppError::bad("relationship is 1-50 characters (e.g. guest)"));
            }
            filters_ok(filters)
        }
        _ => Ok(()),
    }
}

/// The type (and for date triggers, its date field) must exist in the account.
pub async fn check_trigger_refs(db: &PgPool, account_id: Uuid, t: &Trigger) -> AppResult<()> {
    let (key, date_field, filters, changed) = match t {
        Trigger::Object { object_type, filters, field, .. } => (object_type, None, filters, field.as_deref()),
        Trigger::ObjectDate { object_type, field, filters, .. } => (object_type, Some(field), filters, None),
        _ => return Ok(()),
    };
    let ty = super::types::require(db, account_id, key).await?;
    let mut tested: Vec<&str> = filters.iter().map(|f| f.field.as_str()).collect();
    tested.extend(changed);
    tested.extend(date_field.map(String::as_str));
    super::types::check_rule_fields(db, account_id, &ty.key, &tested).await?;
    if let Some(f) = date_field {
        match ty.field(f.trim()) {
            Some(d) if d.kind.is_date() => {}
            Some(_) => return Err(AppError::bad(format!("{f:?} isn't a date field of {}", ty.key))),
            None => return Err(AppError::bad(format!("{} has no field {f:?}", ty.key))),
        }
    }
    Ok(())
}

// ---------- Object-date fires ----------

/// The moment an object is due for an object-date flow, from field $4 (a plain `YYYY-MM-DD` as
/// written, a date-time read in time zone $5): the day plus $6 days at hour $7 local time, or (with
/// $8 hours) the moment plus $8 hours (a plain date's moment is its local midnight). `o` is the object.
const DUE: &str = "CROSS JOIN LATERAL (SELECT o.fields->>$4::text AS raw) r
     CROSS JOIN LATERAL (SELECT CASE WHEN r.raw ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}$' THEN pl_ts(r.raw)::date ELSE (pl_ts(r.raw) AT TIME ZONE $5::text)::date END AS day,
                                CASE WHEN r.raw ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}$' THEN (pl_ts(r.raw)::date + time '00:00') AT TIME ZONE $5::text ELSE pl_ts(r.raw) END AS moment) x
     CROSS JOIN LATERAL (SELECT CASE WHEN $8::int IS NULL THEN ((x.day + $6::int) + make_time($7::int, 0, 0)) AT TIME ZONE $5::text
                                     ELSE x.moment + make_interval(hours => $8::int) END AS due) d";

/// Fires first found more than a day after they were due are recorded as missed (the event-date rule).
const INSERT: &str = "INSERT INTO flow_object_date_fires (flow_id, object_id, account_id, due_at, trigger_key, outcome, fired_at)
     SELECT $1, o.id, o.account_id, d.due, $10::text,
            CASE WHEN d.due > now() - interval '24 hours' THEN 'pending' ELSE 'missed' END,
            CASE WHEN d.due > now() - interval '24 hours' THEN NULL ELSE now() END
     FROM objects o JOIN object_types t ON t.id = o.type_id AND t.account_id = o.account_id AND t.key = $3::text
     {DUE}
     WHERE o.account_id = $2 AND x.day IS NOT NULL AND d.due IS NOT NULL";

/// A short fingerprint of a flow's trigger: a fire made for another trigger never starts.
pub fn trigger_key(trigger: &Value) -> String {
    crate::auth::sha256_hex(&trigger.to_string())[..16].to_string()
}

/// The moment `object_id` is due now (from its current date, the trigger and the time zone).
async fn due_now(conn: &mut PgConnection, account_id: Uuid, object_id: Uuid, key: &str, a: &DateArgs, tz: &str) -> sqlx::Result<Option<DateTime<Utc>>> {
    let q = format!("SELECT d.due FROM objects o {DUE} WHERE o.id = $1 AND o.account_id = $2 AND $3::text IS NOT NULL AND x.day IS NOT NULL");
    let due: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(&q)
        .bind(object_id)
        .bind(account_id)
        .bind(key)
        .bind(&a.field)
        .bind(tz)
        .bind(a.offset_days)
        .bind(a.hour)
        .bind(a.offset_hours)
        .fetch_optional(conn)
        .await?;
    Ok(due.flatten())
}

struct DateArgs {
    field: String,
    offset_days: i32,
    hour: i32,
    offset_hours: Option<i32>,
}

fn date_args(t: &Trigger) -> Option<(String, DateArgs)> {
    match t {
        Trigger::ObjectDate { object_type, field, offset_days, offset_hours, hour, .. } => Some((
            object_type.trim().to_string(),
            DateArgs { field: field.trim().to_string(), offset_days: *offset_days as i32, hour: hour.unwrap_or(9) as i32, offset_hours: offset_hours.map(|h| h as i32) },
        )),
        _ => None,
    }
}

/// Object-date flows (any status: one turned on later still has its upcoming dates) of a type.
async fn date_flows(conn: &mut PgConnection, account_id: Uuid, key: &str) -> sqlx::Result<Vec<FlowRow>> {
    sqlx::query_as("SELECT * FROM flows WHERE account_id = $1 AND trigger->>'type' = 'object_date' AND trigger->>'object_type' = $2").bind(account_id).bind(key).fetch_all(conn).await
}

/// (Re)schedule one object for its type's object-date flows, inside the upsert's transaction: a
/// pending fire for a moment that no longer applies goes, and the current moment is added (unless
/// it already fired).
pub async fn schedule_object(conn: &mut PgConnection, account_id: Uuid, key: &str, object_id: Uuid, tz: &str) -> AppResult<()> {
    for f in date_flows(conn, account_id, key).await? {
        let Ok(t) = flows::parse_trigger(&f.trigger) else { continue };
        let Some((key, a)) = date_args(&t) else { continue };
        // A pending fire for another moment (or another trigger) goes; a filtered one still to come is
        // armed again (the object changed: it may match now, e.g. it's been paid).
        let drop = format!(
            "DELETE FROM flow_object_date_fires f WHERE f.flow_id = $1 AND f.object_id = $9 AND f.outcome = 'pending' AND $3::text IS NOT NULL
               AND (f.trigger_key <> $10::text OR f.due_at IS DISTINCT FROM (SELECT d.due FROM objects o {DUE} WHERE o.id = $9 AND o.account_id = $2 AND x.day IS NOT NULL))"
        );
        let rearm = "UPDATE flow_object_date_fires SET outcome = 'pending', fired_at = NULL
                     WHERE flow_id = $1 AND object_id = $9 AND account_id = $2 AND outcome = 'filtered' AND trigger_key = $10::text
                       AND due_at > now() - interval '24 hours' AND $3::text IS NOT NULL AND $4::text IS NOT NULL AND $5::text IS NOT NULL
                       AND $6::int IS NOT NULL AND $7::int IS NOT NULL AND ($8::int IS NULL OR TRUE)".to_string();
        let add = format!("{} AND o.id = $9 ON CONFLICT DO NOTHING", INSERT.replace("{DUE}", DUE));
        let tk = trigger_key(&f.trigger);
        for q in [drop, rearm, add] {
            sqlx::query(&q)
                .bind(f.id)
                .bind(account_id)
                .bind(&key)
                .bind(&a.field)
                .bind(tz)
                .bind(a.offset_days)
                .bind(a.hour)
                .bind(a.offset_hours)
                .bind(object_id)
                .bind(&tk)
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(())
}

/// The account's time zone changed: pending fires move to the same local moment in the new one.
pub async fn reschedule(db: &PgPool, account_id: Uuid) -> anyhow::Result<()> {
    let tz = account_tz(db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let flows: Vec<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE account_id = $1 AND trigger->>'type' = 'object_date'").bind(account_id).fetch_all(db).await?;
    for f in flows {
        let Ok(t) = flows::parse_trigger(&f.trigger) else { continue };
        let Some((key, a)) = date_args(&t) else { continue };
        // A pending fire whose new moment already has a fire (it ran, or is waiting) is obsolete;
        // the others move to their new moment.
        let moved = |set: &str, cond: &str| {
            format!(
                "UPDATE flow_object_date_fires f SET {set} FROM objects o JOIN object_types t ON t.id = o.type_id AND t.key = $3::text {DUE}
                 WHERE f.flow_id = $1 AND f.account_id = $2 AND f.outcome = 'pending' AND o.id = f.object_id AND o.account_id = $2 AND x.day IS NOT NULL AND d.due IS NOT NULL
                   AND d.due <> f.due_at AND {cond} EXISTS (SELECT 1 FROM flow_object_date_fires g WHERE g.flow_id = f.flow_id AND g.object_id = f.object_id AND g.due_at = d.due)"
            )
        };
        for q in [moved("outcome = 'obsolete', fired_at = now()", ""), moved("due_at = d.due", "NOT")] {
            sqlx::query(&q).bind(f.id).bind(account_id).bind(&key).bind(&a.field).bind(&tz).bind(a.offset_days).bind(a.hour).bind(a.offset_hours).execute(db).await?;
        }
    }
    Ok(())
}

/// Fires due longer ago than the catch-up window are missed (or not_live), as for event dates.
pub async fn mark_missed(db: &PgPool, catch_up_hours: i64) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE flow_object_date_fires x SET outcome = CASE WHEN fl.status = 'live' THEN 'missed' ELSE 'not_live' END, fired_at = now()
         FROM flows fl WHERE fl.id = x.flow_id AND x.outcome = 'pending' AND x.due_at <= now() - make_interval(hours => $1)",
    )
    .bind(catch_up_hours as i32)
    .execute(db)
    .await?;
    Ok(())
}

/// Start up to `max` due fires.
pub async fn fire_due(state: &AppState, max: usize) -> anyhow::Result<()> {
    for _ in 0..max {
        if !fire_one(state).await? {
            break;
        }
    }
    Ok(())
}

/// Claim one due fire and start its runs, in one transaction (the event-date claim).
async fn fire_one(state: &AppState) -> anyhow::Result<bool> {
    let mut tx = state.db.begin().await?;
    // Each live flow's earliest due fire, from the (flow, due) index: paused flows' fires are never
    // read. Inside the catch-up window only (older ones are missed).
    let claimed: Option<(Uuid, Uuid, DateTime<Utc>, String, Uuid)> = sqlx::query_as(
        "SELECT x.flow_id, x.object_id, x.due_at, x.trigger_key, fl.account_id FROM flows fl
         CROSS JOIN LATERAL (SELECT f.flow_id, f.object_id, f.due_at, f.trigger_key FROM flow_object_date_fires f
                             WHERE f.flow_id = fl.id AND f.outcome = 'pending' AND f.due_at <= now() AND f.due_at > now() - make_interval(hours => $1)
                             ORDER BY f.due_at LIMIT 1 FOR UPDATE SKIP LOCKED) x
         WHERE fl.status = 'live' AND fl.trigger->>'type' = 'object_date'
           AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = fl.account_id AND a.closed_at IS NOT NULL)
         ORDER BY x.due_at LIMIT 1",
    )
    .bind(flows::EVENT_DATE_CATCH_UP_HOURS as i32)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((flow_id, object_id, due, key, account_id)) = claimed else { return Ok(false) };
    super::hooks::pause(account_id, "fire_claimed").await;
    let f: FlowRow = sqlx::query_as("SELECT * FROM flows WHERE id = $1").bind(flow_id).fetch_one(&mut *tx).await?;
    let (label, run, n) = match date_start(state, &mut tx, &f, object_id, due, &key).await? {
        Ok((run, n)) => ("started", Some(run), n),
        Err(why) => (why, None, 0),
    };
    sqlx::query("UPDATE flow_object_date_fires SET outcome = $4, run_id = $5, runs = $6, fired_at = now() WHERE flow_id = $1 AND object_id = $2 AND due_at = $3")
        .bind(flow_id)
        .bind(object_id)
        .bind(due)
        .bind(label)
        .bind(run)
        .bind(n)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

struct Obj {
    type_key: String,
    external_id: String,
    /// Only the fields rules may test (see `types::rule_fields`).
    fields: Map<String, Value>,
    /// The keys of those fields in the type (defined, not sensitive).
    rule_keys: Vec<String>,
}

async fn load_obj(conn: &mut PgConnection, account_id: Uuid, object_id: Uuid) -> AppResult<Option<Obj>> {
    let row: Option<(Uuid, String, Value)> = sqlx::query_as("SELECT o.type_id, o.external_id, o.fields FROM objects o WHERE o.id = $1 AND o.account_id = $2")
        .bind(object_id)
        .bind(account_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some((type_id, external_id, fields)) = row else { return Ok(None) };
    let Some(t) = super::types::by_id(&mut *conn, account_id, type_id).await? else { return Ok(None) };
    let fields = super::types::rule_fields(&t, fields.as_object().unwrap_or(&Map::new()));
    let rule_keys = t.fields.iter().filter(|f| !f.sensitive).map(|f| f.key.clone()).collect();
    Ok(Some(Obj { type_key: t.key, external_id, fields, rule_keys }))
}

async fn linked(conn: &mut PgConnection, object_id: Uuid, relationship: Option<&str>) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar("SELECT DISTINCT profile_id FROM object_links WHERE object_id = $1 AND ($2::text IS NULL OR relationship = lower($2)) ORDER BY profile_id")
        .bind(object_id)
        .bind(relationship.map(str::trim))
        .fetch_all(conn)
        .await
}

async fn passes_profile_filter(state: &AppState, f: &FlowRow, def: &flows::FlowDef, profile_id: Uuid) -> anyhow::Result<bool> {
    let Some(pf) = &def.settings.profile_filter else { return Ok(true) };
    let seg = segments::load(&state.db, f.account_id, pf, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    segments::profile_matches(&state.db, profile_id, &seg).await.map_err(|e| anyhow::anyhow!(e.public_message()))
}


/// Checks and starts for one object-date fire, inside the claiming transaction: one run per linked
/// person (re-entry doesn't apply: each object's date starts the flow once).
/// Everything about a fire is checked again at claim, in the claim's transaction: the trigger is
/// the one it was made for, the due moment recomputed from the object's current date (and the
/// account's time zone) is this fire's, the object matches the filters, and each person still has
/// the relationship the trigger asks for.
async fn date_start(state: &AppState, tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, f: &FlowRow, object_id: Uuid, due: DateTime<Utc>, key: &str) -> anyhow::Result<Result<(Uuid, i32), &'static str>> {
    let Ok(t) = flows::parse_trigger(&f.trigger) else { return Ok(Err("not_started")) };
    if key != trigger_key(&f.trigger) {
        return Ok(Err("obsolete"));
    }
    let Some((type_key, args)) = date_args(&t) else { return Ok(Err("not_started")) };
    let Trigger::ObjectDate { field, filters, relationship, .. } = t else { return Ok(Err("not_started")) };
    let Some(o) = load_obj(tx, f.account_id, object_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))? else { return Ok(Err("not_started")) };
    // The type changed under the flow (another type, or the date field is now sensitive or gone).
    if o.type_key != type_key || !o.rule_keys.iter().any(|k| k == field.trim()) {
        return Ok(Err("not_started"));
    }
    let tz = account_tz(&state.db, f.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if due_now(tx, f.account_id, object_id, &type_key, &args, &tz).await? != Some(due) {
        return Ok(Err("obsolete"));
    }
    if !filters.iter().all(|flt| rule_matches(&o.fields, &o.rule_keys, flt, &tz, Utc::now())) {
        return Ok(Err("filtered"));
    }
    let def = flows::parse_definition(&f.definition).map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let Some(entry) = def.entry.clone() else { return Ok(Err("not_started")) };
    let ctx = json!({"object": {"id": object_id, "type": o.type_key, "external_id": o.external_id, "date_field": field, "date": o.fields.get(field.trim()), "due_at": due, "trigger_key": key}});
    let mut first = None;
    let mut n = 0;
    for pid in linked(tx, object_id, relationship.as_deref()).await? {
        if !passes_profile_filter(state, f, &def, pid).await? {
            continue;
        }
        let Some(id) = flows::enter(tx, f, &def, pid, &entry, &ctx, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))? else { continue };
        first.get_or_insert(id);
        n += 1;
    }
    Ok(first.map(|id| (id, n)).ok_or("not_started"))
}

/// Events-like seeding for live object-date flows whose objects existed before the flow (or its
/// current trigger): a batch at a time, within the tick's remaining work budget.
pub async fn seed(state: &AppState, mut budget: i64) -> anyhow::Result<()> {
    let flows: Vec<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE status = 'live' AND trigger->>'type' = 'object_date' ORDER BY created_at").fetch_all(&state.db).await?;
    for f in &flows {
        if budget <= 0 {
            break;
        }
        let Ok(t) = flows::parse_trigger(&f.trigger) else { continue };
        let Some((key, a)) = date_args(&t) else { continue };
        let seed: Option<(Value, Option<Uuid>, bool)> = sqlx::query_as("SELECT trigger, last_object_id, done FROM flow_object_date_seeds WHERE flow_id = $1").bind(f.id).fetch_optional(&state.db).await?;
        let (mut last, done) = match seed {
            Some((t, last, done)) if t == f.trigger => (last, done),
            other => {
                let mut tx = state.db.begin().await?;
                if other.is_some() {
                    sqlx::query("DELETE FROM flow_object_date_fires WHERE flow_id = $1 AND outcome = 'pending'").bind(f.id).execute(&mut *tx).await?;
                }
                sqlx::query("INSERT INTO flow_object_date_seeds (flow_id, trigger, last_object_id, done) VALUES ($1, $2, NULL, false) ON CONFLICT (flow_id) DO UPDATE SET trigger = $2, last_object_id = NULL, done = false")
                    .bind(f.id)
                    .bind(&f.trigger)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                (None, false)
            }
        };
        if done {
            continue;
        }
        let tz = account_tz(&state.db, f.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        while budget > 0 {
            let size = budget.min(5000);
            let batch: Vec<Uuid> = sqlx::query_scalar(
                "SELECT o.id FROM objects o JOIN object_types t ON t.id = o.type_id WHERE t.account_id = $1 AND t.key = $2 AND o.account_id = $1 AND ($3::uuid IS NULL OR o.id > $3) ORDER BY o.id LIMIT $4",
            )
            .bind(f.account_id)
            .bind(&key)
            .bind(last)
            .bind(size)
            .fetch_all(&state.db)
            .await?;
            budget -= batch.len().max(1) as i64;
            if let Some(max) = batch.last() {
                let q = format!("{} AND o.id = ANY($9) ON CONFLICT DO NOTHING", INSERT.replace("{DUE}", DUE));
                sqlx::query(&q)
                    .bind(f.id)
                    .bind(f.account_id)
                    .bind(&key)
                    .bind(&a.field)
                    .bind(&tz)
                    .bind(a.offset_days)
                    .bind(a.hour)
                    .bind(a.offset_hours)
                    .bind(&batch)
                    .bind(trigger_key(&f.trigger))
                    .execute(&state.db)
                    .await?;
                last = Some(*max);
            }
            let finished = (batch.len() as i64) < size;
            sqlx::query("UPDATE flow_object_date_seeds SET last_object_id = $2, done = $3 WHERE flow_id = $1").bind(f.id).bind(last).bind(finished).execute(&state.db).await?;
            if finished {
                break;
            }
        }
    }
    Ok(())
}

/// Object-date flows: fires waiting, started, filtered and missed, and the next one due.
pub async fn report(db: &PgPool, f: &FlowRow) -> AppResult<Value> {
    // Checked against the type as it is now: a date field that's sensitive or gone shows nothing.
    if let Ok(t) = flows::parse_trigger(&f.trigger) {
        if let Some((key, a)) = date_args(&t) {
            if super::types::check_rule_fields(db, f.account_id, &key, &[a.field.as_str()]).await.is_err() {
                return Ok(json!({"counts": {}, "next_due_at": null, "hidden": true, "note": "This flow's date field is sensitive or no longer on its type, so it starts nothing and shows nothing."}));
            }
        }
    }
    let flow_id = f.id;
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT outcome, count(*) FROM flow_object_date_fires WHERE flow_id = $1 GROUP BY outcome").bind(flow_id).fetch_all(db).await?;
    let next: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT min(due_at) FROM flow_object_date_fires WHERE flow_id = $1 AND outcome = 'pending'").bind(flow_id).fetch_one(db).await?;
    let counts: Map<String, Value> = rows.into_iter().map(|(k, n)| (k, json!(n))).collect();
    Ok(json!({"counts": counts, "next_due_at": next}))
}

// ---------- Created / updated triggers ----------

/// Inside an upsert's transaction: queue the change for object-triggered flows, when any live one
/// watches this type (one cheap query; nothing else for accounts without them).
pub async fn queue_change(conn: &mut PgConnection, account_id: Uuid, key: &str, change_id: Uuid) -> AppResult<()> {
    let wanted: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flows WHERE account_id = $1 AND status = 'live' AND trigger->>'type' = 'object' AND trigger->>'object_type' = $2)")
        .bind(account_id)
        .bind(key)
        .fetch_one(&mut *conn)
        .await?;
    if wanted {
        crate::jobs::enqueue(&mut *conn, JOB_CHANGE, json!({"change_id": change_id}), None).await?;
    }
    Ok(())
}

fn same_value(a: Option<&Value>, want: &Value) -> bool {
    let Some(a) = a else { return want.is_null() };
    match (a.as_f64(), want.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => text(a).eq_ignore_ascii_case(&text(want)),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Job: one object change; start the live object-triggered flows it matches.
pub async fn run_change(state: &AppState, change_id: Uuid) -> anyhow::Result<()> {
    let row: Option<(Uuid, Uuid, String, Vec<String>, Value)> = sqlx::query_as("SELECT object_id, account_id, kind, changed, after FROM object_changes WHERE id = $1").bind(change_id).fetch_optional(&state.db).await?;
    let Some((object_id, account_id, kind, changed, after)) = row else { return Ok(()) };
    let mut conn = state.db.acquire().await?;
    let Some(o) = load_obj(&mut conn, account_id, object_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))? else { return Ok(()) };
    drop(conn);
    let flows: Vec<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE account_id = $1 AND status = 'live' AND trigger->>'type' = 'object' AND trigger->>'object_type' = $2 ORDER BY created_at")
        .bind(account_id)
        .bind(&o.type_key)
        .fetch_all(&state.db)
        .await?;
    if flows.is_empty() {
        return Ok(());
    }
    let tz = account_tz(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    for f in flows {
        let Ok(Trigger::Object { on, field, to, filters, relationship, .. }) = flows::parse_trigger(&f.trigger) else { continue };
        match on.as_deref().unwrap_or("any") {
            "created" if kind != "created" => continue,
            "updated" if kind != "updated" => continue,
            _ => {}
        }
        if let Some(fk) = field.as_deref().map(str::trim) {
            if !changed.iter().any(|c| c == fk) {
                continue;
            }
            // A field rules can't test (sensitive, or no longer defined) never matches.
            if !o.rule_keys.iter().any(|k| k == fk) {
                continue;
            }
            if let Some(want) = &to {
                if !same_value(after.get(fk), want) {
                    continue;
                }
            }
        }
        if !filters.iter().all(|flt| rule_matches(&o.fields, &o.rule_keys, flt, &tz, Utc::now())) {
            continue;
        }
        change_start(state, &f, change_id, object_id, &o, &kind, &changed, relationship.as_deref()).await?;
    }
    Ok(())
}

/// Claim (change, flow) and start its runs in one transaction. Re-entry is counted for this object.
#[allow(clippy::too_many_arguments)]
async fn change_start(state: &AppState, f: &FlowRow, change_id: Uuid, object_id: Uuid, o: &Obj, kind: &str, changed: &[String], relationship: Option<&str>) -> anyhow::Result<()> {
    let def = flows::parse_definition(&f.definition).map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let Some(entry) = def.entry.clone() else { return Ok(()) };
    let mut tx = state.db.begin().await?;
    let claimed: Option<i32> = sqlx::query_scalar("INSERT INTO object_flow_starts (change_id, flow_id) VALUES ($1, $2) ON CONFLICT DO NOTHING RETURNING runs").bind(change_id).bind(f.id).fetch_optional(&mut *tx).await?;
    if claimed.is_none() {
        return Ok(());
    }
    let ctx = json!({"object": {"id": object_id, "type": o.type_key, "external_id": o.external_id, "change_id": change_id, "change": kind, "changed": changed}});
    let mut n = 0;
    for pid in linked(&mut tx, object_id, relationship).await? {
        if !passes_profile_filter(state, f, &def, pid).await? {
            continue;
        }
        // Re-entry is checked under the flow's per-person lock, for this object only (see
        // `flows::enter`), so two change jobs for one object can't both start a run.
        if flows::enter(&mut tx, f, &def, pid, &entry, &ctx, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?.is_some() {
            n += 1;
        }
    }
    sqlx::query("UPDATE object_flow_starts SET runs = $3 WHERE change_id = $1 AND flow_id = $2").bind(change_id).bind(f.id).bind(n).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

// ---------- Runs ----------

/// What rules (trigger filters, condition splits) may test about a run's object: the fields the
/// type defines and doesn't mark sensitive, and the account's time zone.
pub struct RuleCtx {
    pub fields: Map<String, Value>,
    pub keys: Vec<String>,
    pub tz: String,
}

/// What [`check_run`] found.
pub enum Check {
    /// Not an object run: nothing was read.
    NotObject,
    /// The person leaves the flow.
    Exit(&'static str),
    /// The object as templates see it (`object.*`: never its sensitive fields) and what rules see.
    Live { view: Value, rules: RuleCtx },
}

/// Before each step of a run that belongs to an object (the claim's rules again): the object still
/// exists, the person still has the relationship the trigger asks for, the trigger is the one the
/// run started under, a date run's due moment recomputed now is the one it started for, and the
/// object matches the trigger's filters. Runs without an object cost nothing.
pub async fn check_run(db: &PgPool, f: &FlowRow, context: &Value, profile_id: Uuid) -> anyhow::Result<Check> {
    let Some(object_id) = context.get("object").and_then(|o| o.get("id")).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(Check::NotObject) };
    let trigger = flows::parse_trigger(&f.trigger);
    let rel = match &trigger {
        Ok(Trigger::ObjectDate { relationship, .. } | Trigger::Object { relationship, .. }) => relationship.as_deref().map(|r| r.trim().to_lowercase()),
        _ => None,
    };
    let row: Option<(String, String, Value, bool, Option<String>, Value)> = sqlx::query_as(
        "SELECT t.key, o.external_id, o.fields, EXISTS (SELECT 1 FROM object_links l WHERE l.object_id = o.id AND l.profile_id = $3 AND ($4::text IS NULL OR l.relationship = $4)),
                (SELECT a.timezone FROM accounts a WHERE a.id = o.account_id), t.fields
         FROM objects o JOIN object_types t ON t.id = o.type_id WHERE o.id = $1 AND o.account_id = $2",
    )
    .bind(object_id)
    .bind(f.account_id)
    .bind(profile_id)
    .bind(&rel)
    .fetch_optional(db)
    .await?;
    let Some((key, external_id, all, is_linked, tz, defs)) = row else { return Ok(Check::Exit("object deleted")) };
    if !is_linked {
        return Ok(Check::Exit("no longer linked to the object that way"));
    }
    let all = all.as_object().cloned().unwrap_or_default();
    let defs: Vec<super::types::FieldDef> = serde_json::from_value(defs).unwrap_or_default();
    let keys: Vec<String> = defs.iter().filter(|d| !d.sensitive).map(|d| d.key.clone()).collect();
    let fields: Map<String, Value> = all.iter().filter(|(k, _)| keys.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let tz = tz.and_then(|t| t.parse::<chrono_tz::Tz>().ok()).unwrap_or(chrono_tz::America::Los_Angeles).name().to_string();
    let started_under = context["object"].get("trigger_key").and_then(Value::as_str);
    let filters = match trigger {
        Ok(t @ Trigger::ObjectDate { .. }) => {
            let Some((type_key, args)) = date_args(&t) else { return Ok(Check::Exit("trigger changed")) };
            let Trigger::ObjectDate { filters, .. } = t else { return Ok(Check::Exit("trigger changed")) };
            if type_key != key || started_under.is_some_and(|k| k != trigger_key(&f.trigger)) || !keys.contains(&args.field) {
                return Ok(Check::Exit("trigger changed"));
            }
            let started: Option<DateTime<Utc>> = context["object"].get("due_at").and_then(|v| serde_json::from_value(v.clone()).ok());
            let mut conn = db.acquire().await?;
            if started.is_none() || due_now(&mut conn, f.account_id, object_id, &type_key, &args, &tz).await? != started {
                return Ok(Check::Exit("date moved"));
            }
            filters
        }
        Ok(Trigger::Object { object_type, filters, .. }) if object_type.trim() == key => filters,
        _ => return Ok(Check::Exit("trigger changed")),
    };
    if !filters.iter().all(|flt| rule_matches(&fields, &keys, flt, &tz, Utc::now())) {
        return Ok(Check::Exit("no longer matches the trigger"));
    }
    let mut view = fields.clone();
    view.entry("external_id").or_insert(json!(external_id));
    view.entry("type").or_insert(json!(key));
    Ok(Check::Live { view: Value::Object(view), rules: RuleCtx { fields, keys, tz } })
}

/// A flow condition split's `object_filters` (all must hold) against the run's object; a rule on a
/// field rules can't test never holds.
pub fn condition(node: &flows::Node, object: Option<&RuleCtx>) -> Option<bool> {
    let raw = node.data.get("object_filters")?;
    let filters: Vec<Filter> = serde_json::from_value(raw.clone()).unwrap_or_default();
    Some(match object {
        Some(r) => filters.iter().all(|f| rule_matches(&r.fields, &r.keys, f, &r.tz, Utc::now())),
        None => false,
    })
}

/// Checks on a condition split's `object_filters` (called from `flows::parse_definition`).
pub fn validate_condition(node: &flows::Node) -> AppResult<()> {
    let Some(raw) = node.data.get("object_filters") else { return Ok(()) };
    let filters: Vec<Filter> = serde_json::from_value(raw.clone()).map_err(|e| AppError::bad(format!("condition {}: object_filters: {e}", node.id)))?;
    if filters.len() > 20 {
        return Err(AppError::bad(format!("condition {}: at most 20 object filters", node.id)));
    }
    segments::parse(&json!({"conditions": filters.iter().map(|x| json!({"type": "property", "field": x.field, "op": x.op, "value": x.value})).collect::<Vec<_>>()}))?;
    Ok(())
}

// ---------- Field rules in Rust (triggers, exits, splits) ----------

enum When {
    Day(NaiveDate),
    At(DateTime<Utc>),
}

fn when(v: &Value) -> Option<When> {
    let s = v.as_str()?.trim();
    if s.len() == 10 {
        return NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(When::Day);
    }
    DateTime::parse_from_rfc3339(s).ok().map(|d| When::At(d.with_timezone(&Utc)))
}

fn days(v: &Option<Value>) -> f64 {
    v.as_ref().and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(0.0)
}

/// [`filter_matches`] that fails closed: a rule on a field the type doesn't define, or marks
/// sensitive, never holds (whatever its operator, `not_exists` and `neq` included).
pub fn rule_matches(fields: &Map<String, Value>, keys: &[String], f: &Filter, tz: &str, now: DateTime<Utc>) -> bool {
    keys.iter().any(|k| k == f.field.trim()) && filter_matches(fields, f, tz, now)
}

/// One field rule on an object's fields. Dates are compared as dates: a plain date by calendar day
/// in the account's time zone, a date-time as a moment. Other rules work as event filters do.
pub fn filter_matches(fields: &Map<String, Value>, f: &Filter, tz: &str, now: DateTime<Utc>) -> bool {
    let got = fields.get(f.field.trim());
    let today = now.with_timezone(&tz_of(tz)).date_naive();
    let date_rule = matches!(f.op, PropOp::WithinNextDays | PropOp::InFuture | PropOp::InPast | PropOp::WithinLastDays | PropOp::MoreThanDaysAgo);
    if date_rule {
        let Some(w) = got.and_then(when) else { return false };
        let n = days(&f.value);
        let span = Duration::seconds((n * 86400.0) as i64);
        let nd = n.floor() as i64;
        return match (f.op, w) {
            (PropOp::WithinNextDays, When::Day(d)) => d >= today && d <= today + Duration::days(nd),
            (PropOp::WithinNextDays, When::At(t)) => t >= now && t <= now + span,
            (PropOp::InFuture, When::Day(d)) => d >= today,
            (PropOp::InFuture, When::At(t)) => t > now,
            (PropOp::InPast, When::Day(d)) => d < today,
            (PropOp::InPast, When::At(t)) => t <= now,
            (PropOp::WithinLastDays, When::Day(d)) => d >= today - Duration::days(nd) && d <= today,
            (PropOp::WithinLastDays, When::At(t)) => t >= now - span && t <= now,
            (PropOp::MoreThanDaysAgo, When::Day(d)) => d < today - Duration::days(nd),
            (PropOp::MoreThanDaysAgo, When::At(t)) => t < now - span,
            _ => false,
        };
    }
    // Dates against dates: before, after, between.
    if let (Some(g), true) = (got.and_then(when), matches!(f.op, PropOp::Gt | PropOp::Gte | PropOp::Lt | PropOp::Lte | PropOp::Between)) {
        let key = |w: &When| match w {
            When::Day(d) => d.and_hms_opt(0, 0, 0).map(|x| x.and_utc()).unwrap_or(now),
            When::At(t) => *t,
        };
        let a = key(&g);
        let want = f.value.clone().unwrap_or(Value::Null);
        let one = |v: &Value| when(v).map(|w| key(&w));
        return match f.op {
            PropOp::Gt => one(&want).is_some_and(|b| a > b),
            PropOp::Gte => one(&want).is_some_and(|b| a >= b),
            PropOp::Lt => one(&want).is_some_and(|b| a < b),
            PropOp::Lte => one(&want).is_some_and(|b| a <= b),
            _ => match want.as_array().map(Vec::as_slice) {
                Some([lo, hi]) => matches!((one(lo), one(hi)), (Some(lo), Some(hi)) if a >= lo && a <= hi),
                _ => false,
            },
        };
    }
    segments::json_filter_matches(&Value::Object(fields.clone()), f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(field: &str, op: PropOp, value: Value) -> Filter {
        Filter { field: field.into(), op, value: if value.is_null() { None } else { Some(value) } }
    }

    #[test]
    fn date_rules_use_the_accounts_calendar() {
        // 2026-11-18 03:00 UTC is still the 17th in Los Angeles.
        let now = DateTime::parse_from_rfc3339("2026-11-18T03:00:00Z").unwrap().with_timezone(&Utc);
        let tz = "America/Los_Angeles";
        let obj = |d: &str| json!({"check_in": d, "status": "confirmed"}).as_object().unwrap().clone();
        assert!(filter_matches(&obj("2026-11-17"), &f("check_in", PropOp::InFuture, Value::Null), tz, now), "today counts as upcoming");
        assert!(!filter_matches(&obj("2026-11-17"), &f("check_in", PropOp::InFuture, Value::Null), "UTC", now), "in UTC it's already the 18th");
        assert!(filter_matches(&obj("2026-12-01"), &f("check_in", PropOp::WithinNextDays, json!(14)), tz, now));
        assert!(!filter_matches(&obj("2026-12-02"), &f("check_in", PropOp::WithinNextDays, json!(14)), tz, now));
        assert!(filter_matches(&obj("2026-11-16"), &f("check_in", PropOp::InPast, Value::Null), tz, now));
        assert!(filter_matches(&obj("2026-11-20T10:00:00Z"), &f("check_in", PropOp::WithinNextDays, json!(3)), tz, now));
        assert!(filter_matches(&obj("2026-11-20"), &f("check_in", PropOp::Gt, json!("2026-11-19")), tz, now));
        assert!(filter_matches(&obj("2026-11-20"), &f("check_in", PropOp::Between, json!(["2026-11-01", "2026-11-30"])), tz, now));
        assert!(filter_matches(&obj("2026-11-20"), &f("status", PropOp::Eq, json!("Confirmed")), tz, now));
        assert!(!filter_matches(&obj("2026-11-20"), &f("missing", PropOp::InFuture, Value::Null), tz, now));
    }

    #[test]
    fn triggers_are_checked() {
        let t = |v: Value| flows::parse_trigger(&v);
        assert!(t(json!({"type": "object", "object_type": "reservation", "on": "updated", "field": "status", "to": "cancelled"})).is_ok());
        assert!(t(json!({"type": "object", "object_type": "reservation", "to": "cancelled"})).is_err(), "to needs field");
        assert!(t(json!({"type": "object", "object_type": "Reservation"})).is_err());
        assert!(t(json!({"type": "object", "object_type": "reservation", "on": "deleted"})).is_err());
        assert!(t(json!({"type": "object_date", "object_type": "reservation", "field": "check_in", "offset_days": -2, "hour": 10})).is_ok());
        assert!(t(json!({"type": "object_date", "object_type": "reservation", "field": "check_in", "offset_hours": -3})).is_ok());
        assert!(t(json!({"type": "object_date", "object_type": "reservation", "field": "check_in", "offset_hours": -3, "hour": 9})).is_err());
        assert!(t(json!({"type": "object_date", "object_type": "reservation", "field": "check_in", "hour": 24})).is_err());
        assert!(t(json!({"type": "object_date", "object_type": "reservation", "field": "check_in", "filters": [{"field": "status", "op": "eq"}]})).is_err());
    }
}
