//! Running a sync: find who belongs in the audience, diff against what was last sent, then send the
//! removals and additions in batches. Each step is a short job that queues the next one, so a big
//! audience never ties up a worker, and a run picks up where it stopped after a crash or a retry.
//!
//! Phases: `scan` (build `ad_audience_desired` from the list or segment, 5,000 people a page) →
//! `remove` (snapshot rows with no matching desired row) → `add` (desired rows with no matching
//! snapshot row) → `finish` (audience size from the platform, summary, next run time). The snapshot
//! (`ad_audience_members`) only changes after the platform accepts a batch, so re-running is exact.

use chrono::{Duration, Utc};
use rand::Rng;
use serde_json::{json, Value};
use sqlx::QueryBuilder;
use uuid::Uuid;

use super::normalize::{self, Person, Skip};
use super::platforms::{self, MetaSession, Op, PErr, Platform, Target};
use super::store::{self, SyncRow};
use crate::state::AppState;

pub const JOB_KIND: &str = "audiences_run";
const SCAN_PAGE: i64 = 5_000;
const SCAN_PAGES_PER_STEP: usize = 4;
/// Meta's upload sessions close 90 minutes after the first batch; start a fresh one before that.
const META_SESSION_MINUTES: i64 = 80;

#[derive(Debug, sqlx::FromRow)]
struct RunRow {
    id: Uuid,
    sync_id: Uuid,
    status: String,
    phase: String,
    cursor_id: Option<Uuid>,
    state: Value,
}

/// Start a run unless one is already going. Returns the run id.
pub async fn start_run(state: &AppState, sync_id: Uuid, trigger: &str) -> sqlx::Result<Option<Uuid>> {
    let run_id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    let account: Option<Uuid> = sqlx::query_scalar("UPDATE ad_audience_syncs SET current_run_id = $2, updated_at = now() WHERE id = $1 AND current_run_id IS NULL RETURNING account_id")
        .bind(sync_id)
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(account_id) = account else { return Ok(None) };
    sqlx::query("INSERT INTO ad_audience_runs (id, sync_id, account_id, trigger) VALUES ($1, $2, $3, $4)").bind(run_id).bind(sync_id).bind(account_id).bind(trigger).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM ad_audience_desired WHERE sync_id = $1").bind(sync_id).execute(&mut *tx).await?;
    crate::jobs::enqueue(&mut *tx, JOB_KIND, json!({"run_id": run_id}), None).await?;
    tx.commit().await?;
    Ok(Some(run_id))
}

async fn requeue(state: &AppState, run_id: Uuid, delay_secs: i64) -> anyhow::Result<()> {
    let at = (delay_secs > 0).then(|| Utc::now() + Duration::seconds(delay_secs));
    crate::jobs::enqueue(&state.db, JOB_KIND, json!({"run_id": run_id}), at).await?;
    Ok(())
}

/// One step of a run (the job). Platform problems are handled here (retried with backoff or shown on
/// the sync); only database errors fail the job, and the queue retries those.
pub async fn step(state: &AppState, run_id: Uuid) -> anyhow::Result<()> {
    let run: Option<RunRow> = sqlx::query_as("SELECT id, sync_id, status, phase, cursor_id, state FROM ad_audience_runs WHERE id = $1").bind(run_id).fetch_optional(&state.db).await?;
    let Some(run) = run else { return Ok(()) };
    if run.status != "running" {
        return Ok(());
    }
    let Some(sync) = store::sync(&state.db, run.sync_id).await? else { return Ok(()) };
    if sync.current_run_id != Some(run.id) {
        return Ok(());
    }
    sqlx::query("UPDATE ad_audience_runs SET heartbeat_at = now() WHERE id = $1").bind(run.id).execute(&state.db).await?;
    if sync.status == "paused" {
        return end_run(state, &sync, &run, "stopped", Some("Paused before it finished.")).await;
    }
    let Some(conn) = store::connection_by_id(&state.db, sync.connection_id).await? else {
        return fail(state, &sync, &run, "The ad platform connection was removed.").await;
    };
    let token = match store::access_token(state, &conn).await {
        Ok(t) => t,
        Err(e) => return handle(state, &sync, &run, e).await,
    };
    let target = Target { platform: sync.platform(), token: &token, ad_account_id: &sync.ad_account_id, login_account_id: sync.login_account_id.as_deref() };
    let res = match run.phase.as_str() {
        "scan" => scan(state, &sync, &run).await?,
        "remove" => push(state, &sync, &run, &target, Op::Remove).await?,
        "add" => push(state, &sync, &run, &target, Op::Add).await?,
        _ => return finish(state, &sync, &run, &target).await,
    };
    match res {
        Ok(()) => {
            if run.state["retries"].as_i64().unwrap_or(0) > 0 {
                sqlx::query("UPDATE ad_audience_runs SET state = state - 'retries' WHERE id = $1").bind(run.id).execute(&state.db).await?;
            }
            requeue(state, run.id, 0).await
        }
        Err(e) => handle(state, &sync, &run, e).await,
    }
}

/// Retry rate limits and outages with growing waits; show everything else on the sync.
async fn handle(state: &AppState, sync: &SyncRow, run: &RunRow, e: PErr) -> anyhow::Result<()> {
    let retries = run.state["retries"].as_i64().unwrap_or(0);
    match &e {
        PErr::RateLimited(_) | PErr::Transient(_) if retries < 10 => {
            let wait = (60i64 * 2i64.pow(retries as u32)).min(3600);
            sqlx::query("UPDATE ad_audience_runs SET state = jsonb_set(state, '{retries}', to_jsonb($2::int)), error = $3 WHERE id = $1")
                .bind(run.id)
                .bind(retries as i32 + 1)
                .bind(e.message())
                .execute(&state.db)
                .await?;
            requeue(state, run.id, wait).await
        }
        PErr::NotFound(_) => {
            // The audience was deleted on the platform. Start over with a new one next time.
            sqlx::query("UPDATE ad_audience_syncs SET external_audience_id = NULL WHERE id = $1").bind(sync.id).execute(&state.db).await?;
            sqlx::query("DELETE FROM ad_audience_members WHERE sync_id = $1").bind(sync.id).execute(&state.db).await?;
            let msg = format!("The audience was deleted on {}. DumbMail will make a new one on the next sync.", sync.platform().label());
            fail(state, sync, run, &msg).await
        }
        PErr::Auth(m) => {
            let _ = store::mark_needs_reconnect(&state.db, sync.connection_id, m).await;
            fail(state, sync, run, m).await
        }
        other => fail(state, sync, run, other.message()).await,
    }
}

async fn fail(state: &AppState, sync: &SyncRow, run: &RunRow, msg: &str) -> anyhow::Result<()> {
    end_run(state, sync, run, "failed", Some(msg)).await
}

async fn end_run(state: &AppState, sync: &SyncRow, run: &RunRow, status: &str, msg: Option<&str>) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE ad_audience_runs SET status = $2, error = $3, finished_at = now() WHERE id = $1").bind(run.id).bind(status).bind(msg).execute(&mut *tx).await?;
    let set_error = status == "failed";
    sqlx::query(
        "UPDATE ad_audience_syncs SET current_run_id = NULL, last_run_at = now(), last_error = CASE WHEN $3 THEN $2 ELSE last_error END,
             members_sent = (SELECT count(*) FROM ad_audience_members WHERE sync_id = $1),
             next_run_at = now() + make_interval(hours => interval_hours), updated_at = now()
         WHERE id = $1 AND current_run_id = $4",
    )
    .bind(sync.id)
    .bind(msg)
    .bind(set_error)
    .bind(run.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM ad_audience_desired WHERE sync_id = $1").bind(sync.id).execute(&mut *tx).await?;
    tx.commit().await?;
    if status == "failed" {
        tracing::info!(sync = %sync.id, platform = %sync.platform, error = msg.unwrap_or_default(), "audience sync run failed");
    }
    Ok(())
}

/// `true`/`yes`/`1` in a profile property.
fn push_flag(qb: &mut QueryBuilder<'_, sqlx::Postgres>, key: &'static str, values: &'static str) {
    qb.push(format!("lower(coalesce(p.properties->>'{key}', '')) IN {values}"));
}

/// Who may never be sent, whatever the source says: people who opted out of ads (the `ads_opt_out`
/// property, a recorded "do not sell or share" or Global Privacy Control signal, or `ads_consent`
/// false), and, unless the sync is an exclusion audience that asked for them, people who
/// unsubscribed from email or whose email is suppressed.
pub fn push_eligible(qb: &mut QueryBuilder<'_, sqlx::Postgres>, include_unsubscribed: bool) {
    const YES: &str = "('true', 'yes', '1')";
    const NO: &str = "('false', 'no', '0')";
    qb.push(" AND NOT (");
    push_flag(qb, "ads_opt_out", YES);
    qb.push(" OR ");
    push_flag(qb, "do_not_sell", YES);
    qb.push(" OR ");
    push_flag(qb, "gpc", YES);
    qb.push(" OR ");
    push_flag(qb, "ads_consent", NO);
    qb.push(")");
    if !include_unsubscribed {
        qb.push(" AND p.email_consent <> 'unsubscribed' AND p.email_suppressed_reason IS NULL");
    }
}

/// `FROM profiles p ... WHERE` for the sync's list or segment, with the eligibility rules applied.
/// Err is a message for the sync (the list or segment is gone, or its rules don't load).
pub async fn push_source(qb: &mut QueryBuilder<'_, sqlx::Postgres>, db: &sqlx::PgPool, account_id: Uuid, source_type: &str, source_id: Uuid, include_unsubscribed: bool) -> anyhow::Result<Result<(), String>> {
    if source_type == "list" {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM lists WHERE id = $1 AND account_id = $2)").bind(source_id).bind(account_id).fetch_one(db).await?;
        if !exists {
            return Ok(Err("The list this sync uses was deleted. Delete the sync, or remove the audience on the ad platform.".into()));
        }
        qb.push(" FROM profiles p JOIN list_members lm ON lm.profile_id = p.id AND lm.list_id = ").push_bind(source_id).push(" WHERE p.account_id = ").push_bind(account_id);
    } else {
        let def: Option<Value> = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2").bind(source_id).bind(account_id).fetch_optional(db).await?;
        let Some(def) = def else { return Ok(Err("The segment this sync uses was deleted. Delete the sync, or remove the audience on the ad platform.".into())) };
        let def = match crate::segments::load(db, account_id, &def, Some(source_id)).await {
            Ok(d) => d,
            Err(e) => return Ok(Err(format!("The segment's rules can't be used: {}", e.public_message()))),
        };
        qb.push(" FROM profiles p WHERE p.account_id = ").push_bind(account_id).push(" AND ");
        crate::segments::push_definition(qb, &def);
    }
    push_eligible(qb, include_unsubscribed);
    Ok(Ok(()))
}

#[derive(sqlx::FromRow)]
struct ScanRow {
    id: Uuid,
    email: Option<String>,
    phone_number: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    properties: Value,
}

fn prop(props: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match &props[*k] {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn person(r: &ScanRow) -> Person {
    let p = &r.properties;
    let consent = match &p["ads_consent"] {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "1" | "granted" => Some(true),
            "false" | "no" | "0" | "denied" => Some(false),
            _ => None,
        },
        _ => None,
    };
    Person {
        email: r.email.clone(),
        phone: r.phone_number.clone(),
        first_name: r.first_name.clone(),
        last_name: r.last_name.clone(),
        city: prop(p, &["city"]),
        state: prop(p, &["region", "state", "province"]),
        zip: prop(p, &["zip", "postal_code", "zip_code", "postcode"]),
        country: prop(p, &["country", "country_code"]),
        ads_consent: consent,
    }
}

type Step = anyhow::Result<Result<(), PErr>>;

async fn scan(state: &AppState, sync: &SyncRow, run: &RunRow) -> Step {
    let platform = sync.platform();
    let mut cursor = run.cursor_id.unwrap_or(Uuid::nil());
    let mut skipped = 0i32;
    for _ in 0..SCAN_PAGES_PER_STEP {
        let mut qb = QueryBuilder::new("SELECT p.id, p.email, p.phone_number, p.first_name, p.last_name, p.properties");
        if let Err(msg) = push_source(&mut qb, &state.db, sync.account_id, &sync.source_type, sync.source_id, sync.include_unsubscribed).await? {
            return Ok(Err(PErr::Invalid(msg)));
        }
        qb.push(" AND p.id > ").push_bind(cursor).push(" ORDER BY p.id LIMIT ").push_bind(SCAN_PAGE);
        let rows: Vec<ScanRow> = qb.build_query_as().fetch_all(&state.db).await?;
        let done = (rows.len() as i64) < SCAN_PAGE;
        let (mut pids, mut digests, mut ids) = (Vec::new(), Vec::new(), Vec::new());
        for r in &rows {
            match normalize::identifiers(platform, &person(r), sync.with_location()) {
                Ok(m) => {
                    digests.push(normalize::digest(&m));
                    ids.push(Value::Object(m));
                    pids.push(r.id);
                }
                Err(Skip::NoIdentifiers | Skip::NoConsent) => skipped += 1,
            }
        }
        if !pids.is_empty() {
            sqlx::query(
                "INSERT INTO ad_audience_desired (sync_id, profile_id, digest, ids) SELECT $1, * FROM UNNEST($2::uuid[], $3::text[], $4::jsonb[])
                 ON CONFLICT (sync_id, profile_id) DO UPDATE SET digest = EXCLUDED.digest, ids = EXCLUDED.ids",
            )
            .bind(sync.id)
            .bind(&pids)
            .bind(&digests)
            .bind(&ids)
            .execute(&state.db)
            .await?;
        }
        if let Some(last) = rows.last() {
            cursor = last.id;
        }
        if done {
            // Diff once, so batches know how many are left (Meta flags the last batch of a session).
            let (to_remove, to_add): (i64, i64) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM ad_audience_members m WHERE m.sync_id = $1
                         AND NOT EXISTS (SELECT 1 FROM ad_audience_desired d WHERE d.sync_id = m.sync_id AND d.profile_id = m.profile_id AND d.digest = m.digest)),
                        (SELECT count(*) FROM ad_audience_desired d WHERE d.sync_id = $1
                         AND NOT EXISTS (SELECT 1 FROM ad_audience_members m WHERE m.sync_id = d.sync_id AND m.profile_id = d.profile_id AND m.digest = d.digest))",
            )
            .bind(sync.id)
            .fetch_one(&state.db)
            .await?;
            sqlx::query(
                "UPDATE ad_audience_runs SET phase = 'remove', cursor_id = NULL, skipped = skipped + $2,
                     state = state || jsonb_build_object('remove_left', $3::bigint, 'add_left', $4::bigint) WHERE id = $1",
            )
            .bind(run.id)
            .bind(skipped)
            .bind(to_remove)
            .bind(to_add)
            .execute(&state.db)
            .await?;
            return Ok(Ok(()));
        }
    }
    sqlx::query("UPDATE ad_audience_runs SET cursor_id = $2, skipped = skipped + $3 WHERE id = $1").bind(run.id).bind(cursor).bind(skipped).execute(&state.db).await?;
    Ok(Ok(()))
}

#[derive(sqlx::FromRow)]
struct PendingRow {
    profile_id: Uuid,
    digest: String,
    ids: Value,
}

/// Make sure the platform audience exists before adding (Meta and Google; TikTok creates it with the
/// first upload). Looks for one this sync already made before creating another.
async fn ensure_audience(state: &AppState, sync: &SyncRow, t: &Target<'_>) -> anyhow::Result<Result<Option<String>, PErr>> {
    if let Some(id) = &sync.external_audience_id {
        return Ok(Ok(Some(id.clone())));
    }
    let marker = platforms::marker(sync.id);
    let found = match platforms::find_audience(state, t, &sync.name, &marker).await {
        Ok(f) => f,
        Err(e) => return Ok(Err(e)),
    };
    let id = match found {
        Some(id) => Some(id),
        None => match platforms::create_audience(state, t, &sync.name, &marker).await {
            Ok(id) => id,
            Err(e) => return Ok(Err(e)),
        },
    };
    if let Some(id) = &id {
        sqlx::query("UPDATE ad_audience_syncs SET external_audience_id = $2, updated_at = now() WHERE id = $1").bind(sync.id).bind(id).execute(&state.db).await?;
    }
    Ok(Ok(id))
}

async fn push(state: &AppState, sync: &SyncRow, run: &RunRow, t: &Target<'_>, op: Op) -> Step {
    let platform = sync.platform();
    let (phase, next, left_key) = match op {
        Op::Remove => ("remove", "add", "remove_left"),
        Op::Add => ("add", "finish", "add_left"),
    };
    let mut audience = sync.external_audience_id.clone();
    if op == Op::Add {
        audience = match ensure_audience(state, sync, t).await? {
            Ok(a) => a,
            Err(e) => return Ok(Err(e)),
        };
    }
    let batches_per_step = if platform == Platform::Tiktok { 2 } else { 5 };
    let mut cursor = run.cursor_id.unwrap_or(Uuid::nil());
    let mut left = run.state[left_key].as_i64().unwrap_or(0);
    let mut meta_state = run.state[format!("meta_{phase}")].clone();
    for _ in 0..batches_per_step {
        let sql = match op {
            Op::Remove => {
                "SELECT m.profile_id, m.digest, m.ids FROM ad_audience_members m WHERE m.sync_id = $1 AND m.profile_id > $2
                 AND NOT EXISTS (SELECT 1 FROM ad_audience_desired d WHERE d.sync_id = m.sync_id AND d.profile_id = m.profile_id AND d.digest = m.digest)
                 ORDER BY m.profile_id LIMIT $3"
            }
            Op::Add => {
                "SELECT d.profile_id, d.digest, d.ids FROM ad_audience_desired d WHERE d.sync_id = $1 AND d.profile_id > $2
                 AND NOT EXISTS (SELECT 1 FROM ad_audience_members m WHERE m.sync_id = d.sync_id AND m.profile_id = d.profile_id AND m.digest = d.digest)
                 ORDER BY d.profile_id LIMIT $3"
            }
        };
        let rows: Vec<PendingRow> = sqlx::query_as(sql).bind(sync.id).bind(cursor).bind(platform.batch_size()).fetch_all(&state.db).await?;
        if rows.is_empty() {
            sqlx::query("UPDATE ad_audience_runs SET phase = $2, cursor_id = NULL WHERE id = $1").bind(run.id).bind(next).execute(&state.db).await?;
            return Ok(Ok(()));
        }
        let ids: Vec<Value> = rows.iter().map(|r| r.ids.clone()).collect();
        let pids: Vec<Uuid> = rows.iter().map(|r| r.profile_id).collect();
        let remaining_after = (left - rows.len() as i64).max(0);
        let full_batch = rows.len() as i64 == platform.batch_size();
        // Removals from an audience that was never created (TikTok before its first upload) have
        // nothing to remove on the platform.
        let result = if op == Op::Remove && audience.is_none() {
            platforms::BatchResult::default()
        } else {
            let session = if platform == Platform::Meta {
                let fresh = meta_state["started"].as_i64().is_none_or(|s| Utc::now().timestamp() - s > META_SESSION_MINUTES * 60);
                if fresh {
                    meta_state = json!({"id": rand::thread_rng().gen_range(1..i64::MAX), "seq": 0, "started": Utc::now().timestamp()});
                }
                let seq = meta_state["seq"].as_i64().unwrap_or(0) + 1;
                meta_state["seq"] = json!(seq);
                Some(MetaSession { id: meta_state["id"].as_i64().unwrap_or(1), seq, last: remaining_after == 0 || !full_batch, estimated_total: left.max(rows.len() as i64) })
            } else {
                None
            };
            match platforms::send_batch(state, t, audience.as_deref(), &sync.name, op, &ids, sync.with_location(), session).await {
                Ok(r) => r,
                Err(e) => {
                    // Keep where we got to; the failed batch is resent on retry (adds and removes are
                    // idempotent on every platform). A retried Meta batch reuses its sequence number.
                    if platform == Platform::Meta {
                        meta_state["seq"] = json!(meta_state["seq"].as_i64().unwrap_or(1) - 1);
                    }
                    save_push_state(state, run.id, cursor, left_key, left, phase, &meta_state).await?;
                    return Ok(Err(e));
                }
            }
        };
        if let Some(created) = &result.created_audience_id {
            sqlx::query("UPDATE ad_audience_syncs SET external_audience_id = $2, updated_at = now() WHERE id = $1").bind(sync.id).bind(created).execute(&state.db).await?;
            audience = Some(created.clone());
        }
        let mut tx = state.db.begin().await?;
        match op {
            Op::Remove => {
                sqlx::query("DELETE FROM ad_audience_members WHERE sync_id = $1 AND profile_id = ANY($2)").bind(sync.id).bind(&pids).execute(&mut *tx).await?;
                sqlx::query("UPDATE ad_audience_runs SET removed = removed + $2, invalid = invalid + $3 WHERE id = $1").bind(run.id).bind(rows.len() as i32).bind(result.invalid as i32).execute(&mut *tx).await?;
            }
            Op::Add => {
                let digests: Vec<&str> = rows.iter().map(|r| r.digest.as_str()).collect();
                sqlx::query(
                    "INSERT INTO ad_audience_members (sync_id, profile_id, digest, ids) SELECT $1, * FROM UNNEST($2::uuid[], $3::text[], $4::jsonb[])
                     ON CONFLICT (sync_id, profile_id) DO UPDATE SET digest = EXCLUDED.digest, ids = EXCLUDED.ids, sent_at = now()",
                )
                .bind(sync.id)
                .bind(&pids)
                .bind(&digests)
                .bind(&ids)
                .execute(&mut *tx)
                .await?;
                sqlx::query("UPDATE ad_audience_runs SET added = added + $2, invalid = invalid + $3 WHERE id = $1").bind(run.id).bind(rows.len() as i32).bind(result.invalid as i32).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        cursor = *pids.last().expect("non-empty batch");
        left = remaining_after;
        if !full_batch {
            sqlx::query("UPDATE ad_audience_runs SET phase = $2, cursor_id = NULL, state = state || jsonb_build_object($3::text, 0) WHERE id = $1")
                .bind(run.id)
                .bind(next)
                .bind(left_key)
                .execute(&state.db)
                .await?;
            return Ok(Ok(()));
        }
    }
    save_push_state(state, run.id, cursor, left_key, left, phase, &meta_state).await?;
    Ok(Ok(()))
}

async fn save_push_state(state: &AppState, run_id: Uuid, cursor: Uuid, left_key: &str, left: i64, phase: &str, meta_state: &Value) -> anyhow::Result<()> {
    sqlx::query("UPDATE ad_audience_runs SET cursor_id = $2, state = state || jsonb_build_object($3::text, $4::bigint, $5::text, $6::jsonb) WHERE id = $1")
        .bind(run_id)
        .bind(cursor)
        .bind(left_key)
        .bind(left)
        .bind(format!("meta_{phase}"))
        .bind(meta_state)
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn finish(state: &AppState, sync: &SyncRow, run: &RunRow, t: &Target<'_>) -> anyhow::Result<()> {
    // Meta and Google audiences exist even when nobody qualifies yet.
    let audience = if sync.platform() == Platform::Tiktok {
        store::sync(&state.db, sync.id).await?.and_then(|s| s.external_audience_id)
    } else {
        match ensure_audience(state, sync, t).await? {
            Ok(a) => a,
            Err(e) => return handle(state, sync, run, e).await,
        }
    };
    let stats = match &audience {
        Some(a) => match platforms::stats(state, t, a).await {
            Ok(s) => Some(s),
            Err(e) => {
                // The upload worked; the size can wait for the next run.
                tracing::info!(sync = %sync.id, error = e.message(), "couldn't read the audience size");
                None
            }
        },
        None => Some(platforms::Stats { status: Some("Waiting for people: TikTok creates the audience with the first upload".into()), ..Default::default() }),
    };
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE ad_audience_runs SET status = 'done', error = NULL, finished_at = now() WHERE id = $1").bind(run.id).execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE ad_audience_syncs s SET current_run_id = NULL, last_run_at = now(), last_success_at = now(), last_error = NULL,
             members_sent = (SELECT count(*) FROM ad_audience_members WHERE sync_id = s.id),
             last_added = r.added, last_removed = r.removed, last_invalid = r.invalid,
             platform_size = CASE WHEN $3 THEN $4 ELSE s.platform_size END, platform_size_upper = CASE WHEN $3 THEN $5 ELSE s.platform_size_upper END,
             match_rate = CASE WHEN $3 THEN $6 ELSE s.match_rate END, platform_status = CASE WHEN $3 THEN $7 ELSE s.platform_status END,
             next_run_at = now() + make_interval(hours => s.interval_hours), updated_at = now()
         FROM ad_audience_runs r WHERE s.id = $1 AND r.id = $2 AND s.current_run_id = $2",
    )
    .bind(sync.id)
    .bind(run.id)
    .bind(stats.is_some())
    .bind(stats.as_ref().and_then(|s| s.size))
    .bind(stats.as_ref().and_then(|s| s.size_upper))
    .bind(stats.as_ref().and_then(|s| s.match_rate))
    .bind(stats.as_ref().and_then(|s| s.status.clone()))
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM ad_audience_desired WHERE sync_id = $1").bind(sync.id).execute(&mut *tx).await?;
    // Keep the last 30 runs of history.
    sqlx::query("DELETE FROM ad_audience_runs WHERE sync_id = $1 AND id NOT IN (SELECT id FROM ad_audience_runs WHERE sync_id = $1 ORDER BY started_at DESC LIMIT 30)")
        .bind(sync.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Scheduler: start due syncs, give up on runs that stopped responding, prune sign-in states.
pub async fn schedule(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT s.id FROM ad_audience_syncs s JOIN ad_connections c ON c.id = s.connection_id
         WHERE s.status = 'active' AND s.current_run_id IS NULL AND s.next_run_at <= now() AND c.status = 'connected'
         ORDER BY s.next_run_at LIMIT 50",
    )
    .fetch_all(&state.db)
    .await?;
    for id in due {
        start_run(state, id, "scheduled").await?;
    }
    // Backoff waits top out at an hour, so a run silent for three is stuck (its job died).
    let stuck: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE ad_audience_runs SET status = 'failed', error = 'This sync stopped responding. It will run again at the next scheduled time.', finished_at = now()
         WHERE status = 'running' AND heartbeat_at < now() - interval '3 hours' RETURNING id, sync_id",
    )
    .fetch_all(&state.db)
    .await?;
    for (run_id, sync_id) in stuck {
        sqlx::query(
            "UPDATE ad_audience_syncs SET current_run_id = NULL, last_error = 'The last sync stopped responding. It will run again at the next scheduled time.',
                 next_run_at = now() + make_interval(hours => interval_hours) WHERE id = $1 AND current_run_id = $2",
        )
        .bind(sync_id)
        .bind(run_id)
        .execute(&state.db)
        .await?;
    }
    sqlx::query("DELETE FROM ad_oauth_states WHERE created_at < now() - interval '1 hour'").execute(&state.db).await?;
    Ok(())
}
