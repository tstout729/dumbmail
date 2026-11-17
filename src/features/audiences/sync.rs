//! Running a sync. Each step is a short job that queues the next one; a run survives crashes and
//! retries because everything it does is written down before it's done.
//!
//! Who qualifies is one SQL predicate (`push_live`): in the list or segment, email consent
//! `subscribed`, not suppressed, no marketing hold, and none of the opt-out signals set. Browser
//! data can only take people out.
//!
//! Phases:
//! - `scan`: hash the identifiers of everyone who qualifies into `ad_audience_desired`.
//! - `veto`: hash the identifiers of everyone in the account who opted out, unsubscribed or is
//!   suppressed. Any identifier one of them maps to is removed and never added, even if another
//!   profile that qualifies maps to it too.
//! - `plan`: identifiers that may be on the platform and no longer qualify (or are vetoed) become
//!   `removing`.
//! - `tiktok_min`: TikTok refuses changes that leave fewer than 1,000; if removals would, the whole
//!   audience is deleted (opt-outs win over keeping an audience alive).
//! - `remove`, `add`: batches. A batch is written (rows, op, id, Meta session id) before the call
//!   and settled in one transaction after it. Additions are claimed by a statement that joins the
//!   staged rows back to the live predicate, so anyone who opted out, was deleted or changed
//!   address since the scan isn't sent.
//! - `sweep`: re-check everyone staged against the live predicate; anyone already sent who no
//!   longer qualifies goes back through `remove` in the same run.
//! - `finish`: Google's processing results, the platform's audience size, counts, next run.
//!
//! One worker owns a run at a time: a lease on the sync row, checked (`FOR UPDATE`) in every write.

use chrono::{Duration, Utc};
use rand::Rng;
use serde_json::{json, Value};
use sqlx::{Postgres, QueryBuilder, Transaction};
use uuid::Uuid;

use super::normalize::{self, Person};
use super::platforms::{self, Kind, Op, Outcome, PErr, Platform, RequestStatus, Target};
use super::store::{self, SyncRow};
use crate::state::AppState;

pub const JOB_KIND: &str = "audiences_run";
const SCAN_PAGE: i64 = 5_000;
const SCAN_PAGES_PER_STEP: usize = 4;
const PLAN_CHUNK: i64 = 20_000;
const PLAN_CHUNKS_PER_STEP: usize = 10;
const LEASE_SECS: i64 = 600;
/// Unreadable answers for one batch before DumbMail stops resending it and counts its rows as
/// "sent, not confirmed".
const MAX_ATTEMPTS: i32 = 5;
const MAX_RETRIES: i32 = 10;
const MAX_SWEEPS: i32 = 3;
/// Google drops Customer Match members after `GOOGLE_MEMBERSHIP_DAYS`; resend before that.
const GOOGLE_REFRESH_DAYS: i64 = 450;

// ---------- Who qualifies ----------

/// A JSON property counts as set unless it's false-like: missing, null, `false`, `"false"`, `"0"`,
/// `"no"` or empty, after trimming, in any case. Deny-first: anything else (`" TRUE "`,
/// `"denied"`, `1`, `{}`) is set.
fn flag_set_sql(a: &str, key: &str) -> String {
    format!("COALESCE(lower(btrim({a}.properties->>'{key}', E' \\t\\r\\n\\f')) NOT IN ('false', '0', 'no', ''), false)")
}

/// True when the profile opted out of ads: `ads_opt_out`, `do_not_sell` or `gpc` set (see
/// `flag_set_sql`), or `ads_consent` present and false-like. `ads_consent` never grants anything.
pub fn opted_out_sql(a: &str) -> String {
    format!(
        "({} OR {} OR {} OR COALESCE(lower(btrim({a}.properties->>'ads_consent', E' \\t\\r\\n\\f')) IN ('false', '0', 'no', ''), false))",
        flag_set_sql(a, "ads_opt_out"),
        flag_set_sql(a, "do_not_sell"),
        flag_set_sql(a, "gpc"),
    )
}

/// May be sent to an ad platform (apart from being in the list or segment).
pub fn eligible_sql(a: &str) -> String {
    format!(
        "({a}.email_consent = 'subscribed' AND {a}.email_suppressed_reason IS NULL \
         /* MARKETING HOLD: when profiles.marketing_hold (PR #15) is on main, add: AND NOT {a}.marketing_hold */ \
         AND NOT {})",
        opted_out_sql(a)
    )
}

/// Vetoes every identifier the profile maps to: opted out of ads, unsubscribed or suppressed.
pub fn veto_sql(a: &str) -> String {
    format!("({a}.email_consent = 'unsubscribed' OR {a}.email_suppressed_reason IS NOT NULL OR {})", opted_out_sql(a))
}

/// The list or segment a sync uses, loaded once per step.
pub enum Source {
    List(Uuid),
    Segment(Box<crate::segments::Definition>),
    /// Gone or unusable: an error code for the sync.
    Gone(&'static str),
}

pub async fn load_source(db: &sqlx::PgPool, account_id: Uuid, source_type: &str, source_id: Uuid) -> anyhow::Result<Source> {
    if source_type == "list" {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM lists WHERE id = $1 AND account_id = $2)").bind(source_id).bind(account_id).fetch_one(db).await?;
        return Ok(if exists { Source::List(source_id) } else { Source::Gone("source_deleted") });
    }
    let def: Option<Value> = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2").bind(source_id).bind(account_id).fetch_optional(db).await?;
    let Some(def) = def else { return Ok(Source::Gone("source_deleted")) };
    Ok(match crate::segments::load(db, account_id, &def, Some(source_id)).await {
        Ok(d) => Source::Segment(Box::new(d)),
        Err(_) => Source::Gone("segment_invalid"),
    })
}

/// The one predicate for "this profile (alias `p`) may be in this sync's audience": in this
/// account, in the list or segment, and eligible.
pub fn push_live(qb: &mut QueryBuilder<'_, Postgres>, account_id: Uuid, src: &Source) {
    qb.push("(p.account_id = ").push_bind(account_id).push(" AND ");
    match src {
        Source::List(id) => {
            qb.push("EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ").push_bind(*id).push(" AND lm.profile_id = p.id)");
        }
        Source::Segment(def) => {
            qb.push("(");
            crate::segments::push_definition(qb, def);
            qb.push(")");
        }
        Source::Gone(_) => {
            qb.push("false");
        }
    }
    qb.push(" AND ").push(eligible_sql("p")).push(")");
}

/// The staged row's identifier is still the profile's (alias `d` for the staged row, `p` the profile).
const SAME_ADDRESS_SQL: &str = "d.src_check = encode(sha256(convert_to(CASE d.key_type WHEN 'email' THEN p.email ELSE p.phone_number END, 'UTF8')), 'hex') \
     AND (d.key_type <> 'phone' OR p.sms_consent = 'subscribed')";

/// No profile mapping to the identifier (staged as `d`) opted out, as staged or now.
const NOT_VETOED_SQL_HEAD: &str = "NOT EXISTS (SELECT 1 FROM ad_audience_desired v WHERE v.sync_id = d.sync_id AND v.key_type = d.key_type AND v.key_hash = d.key_hash \
     AND (v.veto OR EXISTS (SELECT 1 FROM profiles vp WHERE vp.id = v.profile_id AND ";

fn push_not_vetoed(qb: &mut QueryBuilder<'_, Postgres>) {
    qb.push(NOT_VETOED_SQL_HEAD).push(veto_sql("vp")).push(")))");
}

// ---------- Errors ----------

/// Plain words for a sync's error code: DumbMail's own, or a platform's (see `platforms::words`).
pub fn error_words(p: Platform, code: &str) -> String {
    match code {
        "source_deleted" => "The list or segment this sync uses was deleted. DumbMail still removes people who opt out; delete the sync, or the audience on the platform, if you don't need it.".into(),
        "segment_invalid" => "The segment's rules can't be used right now. DumbMail still removes people who opt out. Fix the segment to carry on.".into(),
        "connection_removed" => "The ad platform connection was removed.".into(),
        "too_many_retries" => format!("{} kept failing, so this run stopped. DumbMail tries again at the next scheduled time.", p.label()),
        "stopped_responding" => "The last sync stopped responding. It will run again at the next scheduled time.".into(),
        other => platforms::words(p, other),
    }
}

// ---------- Runs ----------

#[derive(Debug, Clone, sqlx::FromRow)]
struct RunRow {
    id: Uuid,
    sync_id: Uuid,
    status: String,
    mode: String,
    phase: String,
    cursor_id: Option<Uuid>,
    sweeps: i32,
    retries: i32,
}

/// Start a run unless one is already going. Returns the run id. A paused sync gets a run that only
/// removes (people who left or opted out); a sync paused below TikTok's minimum gets a full run,
/// which recreates the audience once enough people qualify.
pub async fn start_run(state: &AppState, sync_id: Uuid, trigger: &str) -> sqlx::Result<Option<Uuid>> {
    let run_id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    let row: Option<(Uuid, String, Option<String>)> =
        sqlx::query_as("UPDATE ad_audience_syncs SET current_run_id = $2, updated_at = now() WHERE id = $1 AND current_run_id IS NULL RETURNING account_id, status, pause_reason")
            .bind(sync_id)
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((account_id, status, reason)) = row else { return Ok(None) };
    let mode = if status == "active" || reason.as_deref() == Some("tiktok_minimum") { "full" } else { "remove_only" };
    sqlx::query("INSERT INTO ad_audience_runs (id, sync_id, account_id, trigger, mode) VALUES ($1, $2, $3, $4, $5)")
        .bind(run_id)
        .bind(sync_id)
        .bind(account_id)
        .bind(trigger)
        .bind(mode)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM ad_audience_desired WHERE sync_id = $1").bind(sync_id).execute(&mut *tx).await?;
    crate::jobs::enqueue(&mut *tx, JOB_KIND, json!({"run_id": run_id}), None).await?;
    tx.commit().await?;
    Ok(Some(run_id))
}

/// Take the sync's lease for this run (only if nobody holds a live one).
async fn claim_lease(db: &sqlx::PgPool, sync_id: Uuid, run_id: Uuid) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar(&format!(
        "UPDATE ad_audience_syncs SET lease_token = $3, lease_until = now() + interval '{LEASE_SECS} seconds'
         WHERE id = $1 AND current_run_id = $2 AND (lease_until IS NULL OR lease_until < now()) RETURNING lease_token"
    ))
    .bind(sync_id)
    .bind(run_id)
    .bind(Uuid::new_v4())
    .fetch_optional(db)
    .await
}

/// A transaction that holds the sync row and proves this worker still owns the lease. None: the
/// lease was lost (another worker took over), so this one must stop without writing.
async fn fenced(state: &AppState, sync_id: Uuid, tok: Uuid) -> sqlx::Result<Option<Transaction<'static, Postgres>>> {
    let mut tx = state.db.begin().await?;
    let ok: Option<i32> = sqlx::query_scalar("SELECT 1 FROM ad_audience_syncs WHERE id = $1 AND lease_token = $2 FOR UPDATE").bind(sync_id).bind(tok).fetch_optional(&mut *tx).await?;
    Ok(ok.map(|_| tx))
}

enum Flow {
    /// Queue the next step (after this many seconds).
    Next(i64),
    /// The run ended, or this worker lost the lease.
    Stop,
}

type Step = anyhow::Result<Flow>;

/// One step of a run (the job). Platform problems are handled here; only database errors fail the
/// job, and the queue retries those.
pub async fn step(state: &AppState, run_id: Uuid) -> anyhow::Result<()> {
    let run: Option<RunRow> = sqlx::query_as("SELECT id, sync_id, status, mode, phase, cursor_id, sweeps, retries FROM ad_audience_runs WHERE id = $1")
        .bind(run_id)
        .fetch_optional(&state.db)
        .await?;
    let Some(run) = run.filter(|r| r.status == "running") else { return Ok(()) };
    let Some(tok) = claim_lease(&state.db, run.sync_id, run.id).await? else { return Ok(()) };
    sqlx::query("UPDATE ad_audience_runs SET heartbeat_at = now() WHERE id = $1").bind(run.id).execute(&state.db).await?;
    let flow = match step_inner(state, &run, tok).await {
        Ok(f) => f,
        Err(e) => {
            release(state, run.sync_id, tok, None).await?;
            return Err(e);
        }
    };
    let next = match flow {
        Flow::Next(delay) => Some((run.id, delay)),
        Flow::Stop => None,
    };
    release(state, run.sync_id, tok, next).await
}

/// Give up the lease and, in the same transaction, queue the run's next step.
async fn release(state: &AppState, sync_id: Uuid, tok: Uuid, next: Option<(Uuid, i64)>) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    let n = sqlx::query("UPDATE ad_audience_syncs SET lease_token = NULL, lease_until = NULL WHERE id = $1 AND lease_token = $2").bind(sync_id).bind(tok).execute(&mut *tx).await?.rows_affected();
    if let (1, Some((run_id, delay))) = (n, next) {
        let at = (delay > 0).then(|| Utc::now() + Duration::seconds(delay));
        crate::jobs::enqueue(&mut *tx, JOB_KIND, json!({"run_id": run_id}), at).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn step_inner(state: &AppState, run: &RunRow, tok: Uuid) -> Step {
    let Some(sync) = store::sync(&state.db, run.sync_id).await? else { return Ok(Flow::Stop) };
    let Some(conn) = store::connection_by_id(&state.db, sync.connection_id).await? else {
        return end_run(state, &sync, run, tok, "failed", Some("connection_removed")).await;
    };
    let token = match store::access_token(state, &conn).await {
        Ok(t) => t,
        Err(e) => return handle(state, &sync, run, tok, conn.generation, e).await,
    };
    let target = Target { platform: sync.platform(), token: &token, ad_account_id: &sync.ad_account_id, login_account_id: sync.login_account_id.as_deref() };
    let ctx = Ctx { state, sync: &sync, run, tok, target: &target };
    let res = match run.phase.as_str() {
        "scan" => ctx.scan(false).await?,
        "veto" => ctx.scan(true).await?,
        "plan" => ctx.plan().await?,
        "tiktok_min" => ctx.tiktok_min().await?,
        "remove" => ctx.push(Op::Remove).await?,
        "add" => ctx.push(Op::Add).await?,
        "sweep" => ctx.sweep().await?,
        _ => ctx.finish().await?,
    };
    match res {
        Ok(flow) => {
            if run.retries > 0 && matches!(flow, Flow::Next(_)) {
                sqlx::query("UPDATE ad_audience_runs SET retries = 0 WHERE id = $1").bind(run.id).execute(&state.db).await?;
            }
            Ok(flow)
        }
        Err(e) => handle(state, &sync, run, tok, conn.generation, e).await,
    }
}

/// Retry rate limits, outages and unreadable answers with growing waits; stop on everything else.
async fn handle(state: &AppState, sync: &SyncRow, run: &RunRow, tok: Uuid, generation: i64, e: PErr) -> Step {
    match e.kind {
        Kind::RateLimited | Kind::Transient | Kind::Unclear if run.retries < MAX_RETRIES => {
            let wait = (60i64 * 2i64.pow(run.retries as u32)).min(3600);
            sqlx::query("UPDATE ad_audience_runs SET retries = retries + 1, error_code = $2 WHERE id = $1").bind(run.id).bind(e.code).execute(&state.db).await?;
            Ok(Flow::Next(wait))
        }
        Kind::RateLimited | Kind::Transient | Kind::Unclear => end_run(state, sync, run, tok, "failed", Some("too_many_retries")).await,
        Kind::NotFound => {
            // Possibly wrong: keep the audience id and every record; ask before making a new one.
            sqlx::query("UPDATE ad_audience_syncs SET status = 'paused', pause_reason = 'audience_missing', updated_at = now() WHERE id = $1 AND lease_token = $2").bind(sync.id).bind(tok).execute(&state.db).await?;
            end_run(state, sync, run, tok, "failed", Some(e.code)).await
        }
        Kind::Auth => {
            store::mark_needs_reconnect(&state.db, sync.connection_id, generation, e.code).await?;
            end_run(state, sync, run, tok, "failed", Some(e.code)).await
        }
        Kind::Permission | Kind::Refused | Kind::Unavailable => end_run(state, sync, run, tok, "failed", Some(e.code)).await,
    }
}

/// Counts of identifiers by what the platform said, for the sync's status.
const COUNTS_SQL: &str = "sent_count = (SELECT count(*) FROM ad_audience_members WHERE sync_id = s.id AND state IN ('adding', 'confirmed', 'unconfirmed')),
     confirmed_count = (SELECT count(*) FROM ad_audience_members WHERE sync_id = s.id AND state = 'confirmed'),
     pending_removal_count = (SELECT count(*) FROM ad_audience_members WHERE sync_id = s.id AND state = 'removing')";

async fn end_run(state: &AppState, sync: &SyncRow, run: &RunRow, tok: Uuid, status: &str, code: Option<&str>) -> Step {
    let Some(mut tx) = fenced(state, sync.id, tok).await? else { return Ok(Flow::Stop) };
    sqlx::query("UPDATE ad_audience_runs SET status = $2, error_code = COALESCE($3, error_code), finished_at = now() WHERE id = $1").bind(run.id).bind(status).bind(code).execute(&mut *tx).await?;
    sqlx::query(&format!(
        "UPDATE ad_audience_syncs s SET current_run_id = NULL, last_run_at = now(), last_error_code = $2, {COUNTS_SQL},
             last_added = r.added, last_removed = r.removed, next_run_at = now() + make_interval(hours => s.interval_hours), updated_at = now()
         FROM ad_audience_runs r WHERE s.id = $1 AND r.id = $3"
    ))
    .bind(sync.id)
    .bind(code)
    .bind(run.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM ad_audience_desired WHERE sync_id = $1").bind(sync.id).execute(&mut *tx).await?;
    prune(&mut tx, sync.id).await?;
    tx.commit().await?;
    if status == "failed" {
        tracing::info!(sync = %sync.id, platform = %sync.platform, code = code.unwrap_or_default(), "audience sync run failed");
    }
    Ok(Flow::Stop)
}

/// Keep the last 30 runs, and settled batches for 30 days.
async fn prune(tx: &mut Transaction<'_, Postgres>, sync_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM ad_audience_runs WHERE sync_id = $1 AND id NOT IN (SELECT id FROM ad_audience_runs WHERE sync_id = $1 ORDER BY started_at DESC LIMIT 30)")
        .bind(sync_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM ad_audience_batches WHERE sync_id = $1 AND status NOT IN ('pending', 'accepted') AND created_at < now() - interval '30 days'")
        .bind(sync_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct ScanRow {
    id: Uuid,
    email: Option<String>,
    phone_number: Option<String>,
    sms_consent: String,
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
    Person {
        email: r.email.clone(),
        phone: r.phone_number.clone(),
        phone_trusted: r.sms_consent == "subscribed",
        first_name: r.first_name.clone(),
        last_name: r.last_name.clone(),
        city: prop(p, &["city"]),
        state: prop(p, &["region", "state", "province"]),
        zip: prop(p, &["zip", "postal_code", "zip_code", "postcode"]),
        country: prop(p, &["country", "country_code"]),
    }
}

#[derive(sqlx::FromRow)]
struct Claimed {
    key_type: String,
    key_hash: String,
    hints: Option<Value>,
}

#[derive(sqlx::FromRow)]
struct BatchRow {
    id: Uuid,
    op: String,
    rows: Value,
    row_count: i32,
    attempts: i32,
    platform_ref: Option<String>,
    sent_at: Option<chrono::DateTime<Utc>>,
}

const BATCH_COLS: &str = "id, op, rows, row_count, attempts, platform_ref, sent_at";

/// How a batch's rows ended.
enum Settle {
    /// Every row confirmed.
    All,
    /// These row indexes weren't confirmed; the rest were.
    Except(Vec<usize>),
    /// Google is processing it.
    Accepted(String),
    /// Refused or given up on: no row confirmed.
    None(&'static str),
}

struct Ctx<'a> {
    state: &'a AppState,
    sync: &'a SyncRow,
    run: &'a RunRow,
    tok: Uuid,
    target: &'a Target<'a>,
}

type PStep = anyhow::Result<Result<Flow, PErr>>;

impl Ctx<'_> {
    fn platform(&self) -> Platform {
        self.sync.platform()
    }

    async fn set_phase(&self, phase: &str) -> PStep {
        let Some(mut tx) = fenced(self.state, self.sync.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
        sqlx::query("UPDATE ad_audience_runs SET phase = $2, cursor_id = NULL WHERE id = $1").bind(self.run.id).bind(phase).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Ok(Flow::Next(0)))
    }

    /// `scan` (veto=false): everyone who qualifies. `veto` (veto=true): everyone in the account who
    /// opted out, unsubscribed or is suppressed.
    async fn scan(&self, veto: bool) -> PStep {
        let s = self.sync;
        let src = if veto { None } else { Some(load_source(&self.state.db, s.account_id, &s.source_type, s.source_id).await?) };
        if let Some(Source::Gone(code)) = &src {
            // Without a source, nobody is added and nobody is removed for leaving it; people who opt
            // out are still removed.
            let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
            sqlx::query("UPDATE ad_audience_runs SET mode = 'veto_only', error_code = $2, phase = 'veto', cursor_id = NULL WHERE id = $1").bind(self.run.id).bind(*code).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(Ok(Flow::Next(0)));
        }
        let mut cursor = self.run.cursor_id.unwrap_or(Uuid::nil());
        let mut skipped = 0i32;
        for _ in 0..SCAN_PAGES_PER_STEP {
            let mut qb = QueryBuilder::new("SELECT p.id, p.email, p.phone_number, p.sms_consent, p.first_name, p.last_name, p.properties FROM profiles p WHERE ");
            match &src {
                Some(src) => push_live(&mut qb, s.account_id, src),
                None => {
                    qb.push("p.account_id = ").push_bind(s.account_id).push(" AND ").push(veto_sql("p"));
                }
            }
            qb.push(" AND p.id > ").push_bind(cursor).push(" ORDER BY p.id LIMIT ").push_bind(SCAN_PAGE);
            let rows: Vec<ScanRow> = qb.build_query_as().fetch_all(&self.state.db).await?;
            let done = (rows.len() as i64) < SCAN_PAGE;
            let (mut pids, mut types, mut hashes, mut checks, mut hints) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for r in &rows {
                let p = person(r);
                let keys = normalize::keys(self.platform(), &p, veto);
                if keys.is_empty() && !veto {
                    skipped += 1;
                }
                let h = if !veto && s.with_hints() { normalize::meta_hints(&p).unwrap_or(Value::Null) } else { Value::Null };
                for k in keys {
                    pids.push(r.id);
                    types.push(k.kind.as_str());
                    hashes.push(k.hash);
                    checks.push(k.src_check);
                    hints.push(h.clone());
                }
            }
            if let Some(last) = rows.last() {
                cursor = last.id;
            }
            let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
            if !pids.is_empty() {
                sqlx::query(
                    "INSERT INTO ad_audience_desired (sync_id, profile_id, key_type, key_hash, src_check, hints, veto)
                     SELECT $1, u.pid, u.kt, u.kh, u.sc, NULLIF(u.hi, 'null'::jsonb), $7 FROM UNNEST($2::uuid[], $3::text[], $4::text[], $5::text[], $6::jsonb[]) AS u(pid, kt, kh, sc, hi)
                     ON CONFLICT (sync_id, profile_id, key_type) DO UPDATE SET key_hash = EXCLUDED.key_hash, src_check = EXCLUDED.src_check, hints = EXCLUDED.hints,
                         veto = ad_audience_desired.veto OR EXCLUDED.veto",
                )
                .bind(s.id)
                .bind(&pids)
                .bind(&types)
                .bind(&hashes)
                .bind(&checks)
                .bind(&hints)
                .bind(veto)
                .execute(&mut *tx)
                .await?;
            }
            if done {
                sqlx::query("UPDATE ad_audience_runs SET phase = $2, cursor_id = NULL, skipped = skipped + $3 WHERE id = $1")
                    .bind(self.run.id)
                    .bind(if veto { "plan" } else { "veto" })
                    .bind(skipped)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(Ok(Flow::Next(0)));
            }
            sqlx::query("UPDATE ad_audience_runs SET cursor_id = $2, skipped = skipped + $3 WHERE id = $1").bind(self.run.id).bind(cursor).bind(skipped).execute(&mut *tx).await?;
            tx.commit().await?;
            skipped = 0;
        }
        Ok(Ok(Flow::Next(0)))
    }

    /// Mark what has to be removed (and what that was waiting for removal qualifies again).
    async fn plan(&self) -> PStep {
        let s = self.sync;
        let leaving = self.run.mode != "veto_only";
        if self.run.cursor_id.is_none() {
            // The staging table was just filled: give the planner fresh statistics, or the joins
            // below fall back to nested loops over every staged row.
            sqlx::query("ANALYZE ad_audience_desired, ad_audience_members").execute(&self.state.db).await?;
        }
        for _ in 0..PLAN_CHUNKS_PER_STEP {
            let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
            let n = sqlx::query(
                "UPDATE ad_audience_members m SET state = 'removing', tried_run_id = NULL FROM (
                     SELECT m2.key_type, m2.key_hash FROM ad_audience_members m2
                     WHERE m2.sync_id = $1 AND m2.state IN ('confirmed', 'unconfirmed') AND m2.batch_id IS NULL
                       AND (EXISTS (SELECT 1 FROM ad_audience_desired d WHERE d.sync_id = m2.sync_id AND d.key_type = m2.key_type AND d.key_hash = m2.key_hash AND d.veto)
                            OR ($2 AND NOT EXISTS (SELECT 1 FROM ad_audience_desired d WHERE d.sync_id = m2.sync_id AND d.key_type = m2.key_type AND d.key_hash = m2.key_hash AND NOT d.veto)))
                     LIMIT $3) x
                 WHERE m.sync_id = $1 AND m.key_type = x.key_type AND m.key_hash = x.key_hash",
            )
            .bind(s.id)
            .bind(leaving)
            .bind(PLAN_CHUNK)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
            if (n as i64) < PLAN_CHUNK {
                let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
                if self.run.mode == "full" {
                    // Waiting for removal but qualifies again: send it as an addition instead.
                    sqlx::query(
                        "UPDATE ad_audience_members m SET state = 'unconfirmed', tried_run_id = NULL WHERE m.sync_id = $1 AND m.state = 'removing' AND m.batch_id IS NULL
                           AND EXISTS (SELECT 1 FROM ad_audience_desired d WHERE d.sync_id = m.sync_id AND d.key_type = m.key_type AND d.key_hash = m.key_hash AND NOT d.veto)
                           AND NOT EXISTS (SELECT 1 FROM ad_audience_desired d WHERE d.sync_id = m.sync_id AND d.key_type = m.key_type AND d.key_hash = m.key_hash AND d.veto)",
                    )
                    .bind(s.id)
                    .execute(&mut *tx)
                    .await?;
                }
                // Staged identifiers already on the platform (or on their way off it) aren't
                // candidates for adding: mark them once here, so each batch claim needn't look.
                sqlx::query(&format!(
                    "UPDATE ad_audience_desired d SET claimed = true FROM ad_audience_members m
                     WHERE d.sync_id = $1 AND m.sync_id = d.sync_id AND m.key_type = d.key_type AND m.key_hash = d.key_hash AND NOT d.claimed
                       AND NOT (m.batch_id IS NULL AND (m.state = 'unconfirmed' OR (m.state = 'confirmed' AND $2 AND m.confirmed_at < now() - interval '{GOOGLE_REFRESH_DAYS} days')))"
                ))
                .bind(s.id)
                .bind(self.platform() == Platform::Google)
                .execute(&mut *tx)
                .await?;
                let next = if self.platform() == Platform::Tiktok { "tiktok_min" } else { "remove" };
                sqlx::query("UPDATE ad_audience_runs SET phase = $2, cursor_id = NULL WHERE id = $1").bind(self.run.id).bind(next).execute(&mut *tx).await?;
                tx.commit().await?;
                return Ok(Ok(Flow::Next(0)));
            }
        }
        Ok(Ok(Flow::Next(0)))
    }

    /// TikTok refuses any change that leaves the audience under 1,000. When the removals due would,
    /// delete the whole audience (one DumbMail created) and pause until enough people qualify.
    async fn tiktok_min(&self) -> PStep {
        let s = self.sync;
        let (removing, remaining): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE state = 'removing'), count(*) FILTER (WHERE state <> 'removing') FROM ad_audience_members WHERE sync_id = $1",
        )
        .bind(s.id)
        .fetch_one(&self.state.db)
        .await?;
        if removing > 0 && remaining < Platform::Tiktok.minimum() {
            return self.delete_whole_audience().await;
        }
        self.set_phase("remove").await
    }

    /// Delete the platform audience and forget its members (only once the platform confirms).
    async fn delete_whole_audience(&self) -> PStep {
        let s = self.sync;
        let Some(aud) = s.external_audience_id.as_deref() else {
            // Never created: nothing can be on TikTok.
            let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
            sqlx::query("DELETE FROM ad_audience_members WHERE sync_id = $1").bind(s.id).execute(&mut *tx).await?;
            tx.commit().await?;
            return self.set_phase("finish").await;
        };
        if let Err(e) = platforms::delete_audience(self.state, self.target, aud).await {
            return Ok(Err(e));
        }
        let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
        let n = sqlx::query("DELETE FROM ad_audience_members WHERE sync_id = $1").bind(s.id).execute(&mut *tx).await?.rows_affected();
        sqlx::query("UPDATE ad_audience_batches SET status = 'abandoned', result_code = 'audience_deleted', settled_at = now() WHERE sync_id = $1 AND status IN ('pending', 'accepted')")
            .bind(s.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE ad_audience_syncs SET external_audience_id = NULL, audience_created_at = NULL, status = 'paused', pause_reason = 'tiktok_minimum',
                 platform_size = NULL, platform_size_upper = NULL, platform_status = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(s.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE ad_audience_runs SET removed = removed + $2, phase = 'finish', cursor_id = NULL WHERE id = $1").bind(self.run.id).bind(n as i32).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Ok(Flow::Next(0)))
    }

    /// Whether this run may add people now (rechecked each batch, so a pause stops additions).
    async fn may_add(&self) -> anyhow::Result<bool> {
        if self.run.mode != "full" {
            return Ok(false);
        }
        let ok: bool = sqlx::query_scalar(&format!(
            "SELECT (s.status = 'active' OR s.pause_reason = 'tiktok_minimum') AND (s.platform <> 'google' OR {}) FROM ad_audience_syncs s WHERE s.id = $1",
            store::GOOGLE_CONSENT_SQL
        ))
        .bind(self.sync.id)
        .fetch_optional(&self.state.db)
        .await?
        .unwrap_or(false);
        if !ok && self.platform() == Platform::Google {
            // The consent setting was turned off: Google syncs pause.
            sqlx::query(&format!(
                "UPDATE ad_audience_syncs s SET status = 'paused', pause_reason = 'google_consent', updated_at = now() WHERE s.id = $1 AND s.status = 'active' AND NOT {}",
                store::GOOGLE_CONSENT_SQL
            ))
            .bind(self.sync.id)
            .execute(&self.state.db)
            .await?;
        }
        Ok(ok)
    }

    /// Send batches: an unsettled one first (same id and rows), then new ones.
    async fn push(&self, op: Op) -> PStep {
        let s = self.sync;
        let (next, op_s) = match op {
            Op::Remove => ("add", "remove"),
            Op::Add => ("sweep", "add"),
        };
        if op == Op::Add && !self.may_add().await? {
            return self.set_phase(next).await;
        }
        let mut audience = s.external_audience_id.clone();
        if op == Op::Remove && audience.is_none() {
            // Nothing was ever created, so nothing can be on the platform.
            return self.set_phase(next).await;
        }
        if op == Op::Add && audience.is_none() {
            if self.platform() == Platform::Tiktok {
                return self.tiktok_first_upload().await;
            }
            match platforms::create_audience(self.state, self.target, &s.name).await {
                Ok(id) => {
                    let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
                    sqlx::query("UPDATE ad_audience_syncs SET external_audience_id = $2, audience_created_at = now(), updated_at = now() WHERE id = $1 AND external_audience_id IS NULL")
                        .bind(s.id)
                        .bind(&id)
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                    audience = Some(id);
                }
                Err(e) => return Ok(Err(e)),
            }
        }
        let aud = audience.unwrap_or_default();
        let batches_per_step = if self.platform() == Platform::Tiktok { 2 } else { 5 };
        for _ in 0..batches_per_step {
            let batch = match self.open_batch(op_s).await? {
                Some(b) => Some(b),
                None => self.claim(op).await?,
            };
            let Some(batch) = batch else { return self.set_phase(next).await };
            let Some(session) = self.mark_sending(&batch).await? else { return Ok(Ok(Flow::Stop)) };
            let rows: Vec<Value> = batch.rows.as_array().cloned().unwrap_or_default();
            let res = platforms::send_batch(self.state, self.target, &aud, op, &rows, session).await;
            match res {
                Ok(Outcome::Confirmed) => self.settle(&batch, Settle::All).await?,
                Ok(Outcome::Partial(bad)) => self.settle(&batch, Settle::Except(bad)).await?,
                Ok(Outcome::Accepted(req)) => self.settle(&batch, Settle::Accepted(req)).await?,
                Err(e) => match e.kind {
                    Kind::Unclear => {
                        if batch.attempts + 1 >= MAX_ATTEMPTS {
                            self.settle(&batch, Settle::None("abandoned")).await?;
                        }
                        return Ok(Err(e));
                    }
                    // Not taken: the batch stays as it is and is resent.
                    Kind::RateLimited | Kind::Transient | Kind::Auth | Kind::NotFound | Kind::Unavailable => return Ok(Err(e)),
                    Kind::Refused if op == Op::Remove && self.platform() == Platform::Tiktok => {
                        // TikTok refused a removal (it refuses any that leave fewer than 1,000).
                        // Opt-outs win: delete the audience.
                        self.settle(&batch, Settle::None("rejected")).await?;
                        return self.delete_whole_audience().await;
                    }
                    Kind::Refused => {
                        self.settle(&batch, Settle::None("rejected")).await?;
                        sqlx::query("UPDATE ad_audience_runs SET error_code = $2 WHERE id = $1").bind(self.run.id).bind(e.code).execute(&self.state.db).await?;
                    }
                    Kind::Permission => {
                        self.settle(&batch, Settle::None("rejected")).await?;
                        return Ok(Err(e));
                    }
                },
            }
        }
        Ok(Ok(Flow::Next(0)))
    }

    /// The oldest batch of this op that was written but never settled (a crash, or no readable
    /// answer). An addition is resent only if every row still qualifies; otherwise it's given up
    /// on (its rows count as possibly sent, so anyone who no longer qualifies is removed) and the
    /// rest go out in a new batch.
    async fn open_batch(&self, op: &str) -> anyhow::Result<Option<BatchRow>> {
        let b: Option<BatchRow> = sqlx::query_as(&format!("SELECT {BATCH_COLS} FROM ad_audience_batches WHERE sync_id = $1 AND op = $2 AND status = 'pending' ORDER BY created_at LIMIT 1"))
            .bind(self.sync.id)
            .bind(op)
            .fetch_optional(&self.state.db)
            .await?;
        let Some(b) = b else { return Ok(None) };
        if op == "remove" {
            return Ok(Some(b));
        }
        let src = load_source(&self.state.db, self.sync.account_id, &self.sync.source_type, self.sync.source_id).await?;
        let mut qb = QueryBuilder::new("SELECT count(DISTINCT (r->>'t', r->>'h')) FROM jsonb_array_elements(");
        qb.push_bind(&b.rows).push(") r WHERE EXISTS (SELECT 1 FROM ad_audience_desired d JOIN profiles p ON p.id = d.profile_id WHERE d.sync_id = ").push_bind(self.sync.id);
        qb.push(" AND d.key_type = r->>'t' AND d.key_hash = r->>'h' AND NOT d.veto AND ");
        push_live(&mut qb, self.sync.account_id, &src);
        qb.push(" AND ").push(SAME_ADDRESS_SQL).push(" AND ");
        push_not_vetoed(&mut qb);
        qb.push(")");
        let still: i64 = qb.build_query_scalar().fetch_one(&self.state.db).await?;
        if still < b.row_count as i64 {
            // "stale": the rows that still qualify may go out again in this run.
            self.settle(&b, Settle::None("stale")).await?;
            return Ok(None);
        }
        Ok(Some(b))
    }

    /// Claim the next batch: pick the identifiers and write the batch, in one transaction, before
    /// anything is sent. Additions are picked by joining the staged rows to the live predicate.
    async fn claim(&self, op: Op) -> anyhow::Result<Option<BatchRow>> {
        let s = self.sync;
        let batch_id = Uuid::now_v7();
        let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(None) };
        let picked: Vec<Claimed> = match op {
            Op::Remove => {
                sqlx::query_as(
                    "WITH c AS (SELECT key_type, key_hash FROM ad_audience_members WHERE sync_id = $1 AND state = 'removing' AND batch_id IS NULL
                                AND tried_run_id IS DISTINCT FROM $3 ORDER BY key_type, key_hash LIMIT $4)
                     UPDATE ad_audience_members m SET batch_id = $2, tried_run_id = $3 FROM c WHERE m.sync_id = $1 AND m.key_type = c.key_type AND m.key_hash = c.key_hash
                     RETURNING m.key_type, m.key_hash, NULL::jsonb AS hints",
                )
                .bind(s.id)
                .bind(batch_id)
                .bind(self.run.id)
                .bind(self.platform().batch_size())
                .fetch_all(&mut *tx)
                .await?
            }
            Op::Add => {
                let src = load_source(&self.state.db, s.account_id, &s.source_type, s.source_id).await?;
                // One statement: pick the identifiers whose people still qualify right now, and
                // claim them. Never one that's in flight, waiting for removal, or was tried this run.
                let mut qb = QueryBuilder::new("WITH c AS (SELECT d.key_type, d.key_hash FROM ad_audience_desired d JOIN profiles p ON p.id = d.profile_id WHERE d.sync_id = ");
                qb.push_bind(s.id).push(" AND NOT d.veto AND NOT d.claimed AND ");
                push_live(&mut qb, s.account_id, &src);
                qb.push(" AND ").push(SAME_ADDRESS_SQL).push(" AND ");
                push_not_vetoed(&mut qb);
                qb.push(" GROUP BY d.key_type, d.key_hash ORDER BY d.key_type, d.key_hash LIMIT ")
                    .push_bind(self.platform().batch_size())
                    .push(") INSERT INTO ad_audience_members (sync_id, key_type, key_hash, state, batch_id, tried_run_id) SELECT ")
                    .push_bind(s.id)
                    .push(", c.key_type, c.key_hash, 'adding', ")
                    .push_bind(batch_id)
                    .push(", ")
                    .push_bind(self.run.id)
                    .push(
                        " FROM c ON CONFLICT (sync_id, key_type, key_hash) DO UPDATE SET batch_id = EXCLUDED.batch_id, tried_run_id = EXCLUDED.tried_run_id,
                           state = CASE WHEN ad_audience_members.state = 'confirmed' THEN 'confirmed' ELSE 'adding' END
                         WHERE ad_audience_members.batch_id IS NULL AND ad_audience_members.state IN ('unconfirmed', 'confirmed')
                           AND ad_audience_members.tried_run_id IS DISTINCT FROM EXCLUDED.tried_run_id
                         RETURNING key_type, key_hash",
                    );
                let taken: Vec<(String, String)> = qb.build_query_as().fetch_all(&mut *tx).await?;
                let (types, hashes): (Vec<String>, Vec<String>) = taken.into_iter().unzip();
                // Mark them taken for this run, and pick up Meta's optional match fields.
                let staged: Vec<(String, String, Option<Value>)> = sqlx::query_as(
                    "UPDATE ad_audience_desired d SET claimed = true FROM UNNEST($2::text[], $3::text[]) AS u(t, h)
                     WHERE d.sync_id = $1 AND d.key_type = u.t AND d.key_hash = u.h RETURNING d.key_type, d.key_hash, d.hints",
                )
                .bind(s.id)
                .bind(&types)
                .bind(&hashes)
                .fetch_all(&mut *tx)
                .await?;
                let mut hints: std::collections::HashMap<(String, String), Value> = std::collections::HashMap::new();
                for (t, h, x) in staged {
                    if let Some(x) = x {
                        hints.entry((t, h)).or_insert(x);
                    }
                }
                let mut out: Vec<Claimed> = types.into_iter().zip(hashes).map(|(t, h)| Claimed { hints: hints.get(&(t.clone(), h.clone())).cloned(), key_type: t, key_hash: h }).collect();
                out.sort_by(|a, b| (&a.key_type, &a.key_hash).cmp(&(&b.key_type, &b.key_hash)));
                out
            }
        };
        if picked.is_empty() {
            tx.commit().await?;
            return Ok(None);
        }
        let rows: Vec<Value> = picked.iter().map(|c| if let Some(h) = &c.hints { json!({"t": c.key_type, "h": c.key_hash, "x": h}) } else { json!({"t": c.key_type, "h": c.key_hash}) }).collect();
        let b: BatchRow = sqlx::query_as(&format!("INSERT INTO ad_audience_batches (id, sync_id, run_id, op, rows, row_count) VALUES ($1, $2, $3, $4, $5, $6) RETURNING {BATCH_COLS}"))
            .bind(batch_id)
            .bind(s.id)
            .bind(self.run.id)
            .bind(if op == Op::Add { "add" } else { "remove" })
            .bind(Value::Array(rows))
            .bind(picked.len() as i32)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(b))
    }

    /// Record the attempt (and, for Meta, a fresh session id) before the call.
    async fn mark_sending(&self, b: &BatchRow) -> anyhow::Result<Option<i64>> {
        let session: i64 = rand::thread_rng().gen_range(1..i64::MAX);
        let Some(mut tx) = fenced(self.state, self.sync.id, self.tok).await? else { return Ok(None) };
        sqlx::query("UPDATE ad_audience_batches SET attempts = attempts + 1, meta_session_id = $2, sent_at = now() WHERE id = $1").bind(b.id).bind(session).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Some(session))
    }

    /// Write what the platform said, for the batch and each of its identifiers, in one transaction.
    async fn settle(&self, b: &BatchRow, how: Settle) -> anyhow::Result<()> {
        let Some(mut tx) = fenced(self.state, self.sync.id, self.tok).await? else { return Ok(()) };
        settle_in(&mut tx, self.sync.id, self.run.id, b, how).await?;
        tx.commit().await?;
        Ok(())
    }

    /// TikTok makes the audience from its first file, and only one of at least 1,000 people.
    async fn tiktok_first_upload(&self) -> PStep {
        let s = self.sync;
        let src = load_source(&self.state.db, s.account_id, &s.source_type, s.source_id).await?;
        let mut qb = QueryBuilder::new("SELECT count(DISTINCT d.key_hash) FROM ad_audience_desired d JOIN profiles p ON p.id = d.profile_id WHERE d.sync_id = ");
        qb.push_bind(s.id).push(" AND NOT d.veto AND ");
        push_live(&mut qb, s.account_id, &src);
        qb.push(" AND ").push(SAME_ADDRESS_SQL).push(" AND ");
        push_not_vetoed(&mut qb);
        let qualifying: i64 = qb.build_query_scalar().fetch_one(&self.state.db).await?;
        if qualifying < Platform::Tiktok.minimum() {
            sqlx::query("UPDATE ad_audience_syncs SET platform_status = $2 WHERE id = $1 AND lease_token = $3")
                .bind(s.id)
                .bind(format!("Waiting: TikTok needs at least 1,000 people, and {qualifying} qualify"))
                .bind(self.tok)
                .execute(&self.state.db)
                .await?;
            return self.set_phase("sweep").await;
        }
        let Some(batch) = self.claim(Op::Add).await? else { return self.set_phase("sweep").await };
        if self.mark_sending(&batch).await?.is_none() {
            return Ok(Ok(Flow::Stop));
        }
        let rows: Vec<Value> = batch.rows.as_array().cloned().unwrap_or_default();
        match platforms::tiktok_create(self.state, self.target, &s.name, &rows).await {
            Ok(id) => {
                let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
                sqlx::query(
                    "UPDATE ad_audience_syncs SET external_audience_id = $2, audience_created_at = now(), platform_status = NULL,
                         status = CASE WHEN pause_reason = 'tiktok_minimum' THEN 'active' ELSE status END,
                         pause_reason = CASE WHEN pause_reason = 'tiktok_minimum' THEN NULL ELSE pause_reason END, updated_at = now() WHERE id = $1",
                )
                .bind(s.id)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                settle_in(&mut tx, s.id, self.run.id, &batch, Settle::All).await?;
                tx.commit().await?;
                Ok(Ok(Flow::Next(0)))
            }
            Err(e) if e.kind == Kind::Unclear => {
                // TikTok may have made the audience, with these people in it, without DumbMail
                // learning its id. Creating it again would make a second one: stop and say so.
                let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
                sqlx::query("UPDATE ad_audience_batches SET status = 'abandoned', result_code = 'create_unclear', settled_at = now() WHERE id = $1").bind(batch.id).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM ad_audience_members WHERE sync_id = $1 AND batch_id = $2").bind(s.id).bind(batch.id).execute(&mut *tx).await?;
                sqlx::query("UPDATE ad_audience_syncs SET status = 'paused', pause_reason = 'create_unclear', updated_at = now() WHERE id = $1").bind(s.id).execute(&mut *tx).await?;
                tx.commit().await?;
                end_run(self.state, s, self.run, self.tok, "failed", Some(e.code)).await.map(Ok)
            }
            Err(e) => {
                // Not created: nobody reached TikTok. A rate limit or outage tries again.
                let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
                sqlx::query("UPDATE ad_audience_batches SET status = 'rejected', result_code = $2, settled_at = now() WHERE id = $1").bind(batch.id).bind(e.code).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM ad_audience_members WHERE sync_id = $1 AND batch_id = $2").bind(s.id).bind(batch.id).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(Err(e))
            }
        }
    }

    /// Re-check everyone staged this run against the live predicate: identifiers already sent
    /// whose people have all stopped qualifying, or any of whom opted out, go back to `remove`.
    async fn sweep(&self) -> PStep {
        let s = self.sync;
        if self.run.mode == "veto_only" || self.run.sweeps >= MAX_SWEEPS {
            return self.set_phase("finish").await;
        }
        let src = load_source(&self.state.db, s.account_id, &s.source_type, s.source_id).await?;
        let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
        let mut qb = QueryBuilder::new("WITH k AS (SELECT d.key_type, d.key_hash, COALESCE(bool_or(p.id IS NOT NULL AND ");
        push_live(&mut qb, s.account_id, &src);
        qb.push(" AND ").push(SAME_ADDRESS_SQL).push("), false) AS ok, COALESCE(bool_or(p.id IS NOT NULL AND ").push(veto_sql("p")).push("), false) AS vetoed");
        qb.push(" FROM ad_audience_desired d LEFT JOIN profiles p ON p.id = d.profile_id WHERE d.sync_id = ").push_bind(s.id).push(" AND NOT d.veto GROUP BY d.key_type, d.key_hash)");
        qb.push(" UPDATE ad_audience_members m SET state = 'removing', tried_run_id = NULL FROM k WHERE m.sync_id = ").push_bind(s.id);
        qb.push(" AND m.key_type = k.key_type AND m.key_hash = k.key_hash AND m.state IN ('confirmed', 'unconfirmed') AND m.batch_id IS NULL AND (NOT k.ok OR k.vetoed)");
        let marked = qb.build().execute(&mut *tx).await?.rows_affected();
        let next = if marked > 0 { if self.platform() == Platform::Tiktok { "tiktok_min" } else { "remove" } } else { "finish" };
        sqlx::query("UPDATE ad_audience_runs SET phase = $2, cursor_id = NULL, sweeps = sweeps + 1 WHERE id = $1").bind(self.run.id).bind(next).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Ok(Flow::Next(0)))
    }

    /// Google's processing results for accepted batches.
    async fn poll_google(&self) -> anyhow::Result<Result<i64, PErr>> {
        let open: Vec<BatchRow> = sqlx::query_as(&format!("SELECT {BATCH_COLS} FROM ad_audience_batches WHERE sync_id = $1 AND status = 'accepted' ORDER BY created_at LIMIT 20"))
            .bind(self.sync.id)
            .fetch_all(&self.state.db)
            .await?;
        let mut still = 0i64;
        for b in &open {
            let Some(req) = b.platform_ref.as_deref() else { continue };
            match platforms::google_request_status(self.state, self.target, req).await {
                Ok(RequestStatus::Success) => self.settle(b, Settle::All).await?,
                Ok(RequestStatus::Failed) => self.settle(b, Settle::None("rejected")).await?,
                Ok(RequestStatus::Partial(Some(bad))) => self.settle(b, Settle::Except(bad)).await?,
                Ok(RequestStatus::Partial(None)) => self.settle(b, Settle::None("partial_unknown")).await?,
                Ok(RequestStatus::Processing) if b.sent_at.is_some_and(|t| t < Utc::now() - Duration::hours(24)) => self.settle(b, Settle::None("abandoned")).await?,
                Ok(RequestStatus::Processing) => still += 1,
                Err(e) if e.kind == Kind::Unclear || e.kind == Kind::Transient || e.kind == Kind::RateLimited => still += 1,
                Err(e) => return Ok(Err(e)),
            }
        }
        Ok(Ok(still))
    }

    async fn finish(&self) -> PStep {
        let s = self.sync;
        let mut waiting = 0i64;
        if self.platform() == Platform::Google {
            waiting = match self.poll_google().await? {
                Ok(n) => n,
                Err(e) => return Ok(Err(e)),
            };
        }
        let audience = store::sync(&self.state.db, s.id).await?.and_then(|x| x.external_audience_id);
        let stats = match &audience {
            Some(a) => match platforms::stats(self.state, self.target, a).await {
                Ok(st) => Some(st),
                Err(e) => {
                    tracing::info!(sync = %s.id, code = e.code, "couldn't read the audience size");
                    None
                }
            },
            None => None,
        };
        let Some(mut tx) = fenced(self.state, s.id, self.tok).await? else { return Ok(Ok(Flow::Stop)) };
        sqlx::query("UPDATE ad_audience_runs SET status = 'done', finished_at = now() WHERE id = $1").bind(self.run.id).execute(&mut *tx).await?;
        let mut status_text = stats.as_ref().and_then(|x| x.status.clone());
        if waiting > 0 {
            status_text = Some(format!("Google is still processing {waiting} batch{}; DumbMail checks again shortly", if waiting == 1 { "" } else { "es" }));
        }
        sqlx::query(&format!(
            "UPDATE ad_audience_syncs s SET current_run_id = NULL, last_run_at = now(),
                 last_success_at = CASE WHEN r.mode = 'full' AND r.error_code IS NULL THEN now() ELSE s.last_success_at END,
                 last_error_code = r.error_code, {COUNTS_SQL}, last_added = r.added, last_removed = r.removed,
                 platform_size = CASE WHEN $3 THEN $4 ELSE s.platform_size END, platform_size_upper = CASE WHEN $3 THEN $5 ELSE s.platform_size_upper END,
                 match_rate = CASE WHEN $3 THEN $6 ELSE s.match_rate END,
                 platform_status = COALESCE($7::text, s.platform_status),
                 next_run_at = CASE WHEN $8 THEN LEAST(now() + interval '15 minutes', now() + make_interval(hours => s.interval_hours))
                                    ELSE now() + make_interval(hours => s.interval_hours) END,
                 updated_at = now()
             FROM ad_audience_runs r WHERE s.id = $1 AND r.id = $2"
        ))
        .bind(s.id)
        .bind(self.run.id)
        .bind(stats.is_some())
        .bind(stats.as_ref().and_then(|x| x.size))
        .bind(stats.as_ref().and_then(|x| x.size_upper))
        .bind(stats.as_ref().and_then(|x| x.match_rate))
        .bind(status_text)
        .bind(waiting > 0)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM ad_audience_desired WHERE sync_id = $1").bind(s.id).execute(&mut *tx).await?;
        prune(&mut tx, s.id).await?;
        tx.commit().await?;
        Ok(Ok(Flow::Stop))
    }
}

/// Settle a batch inside the caller's (fenced) transaction.
async fn settle_in(tx: &mut Transaction<'_, Postgres>, sync_id: Uuid, run_id: Uuid, b: &BatchRow, how: Settle) -> anyhow::Result<()> {
    let rows = b.rows.as_array().cloned().unwrap_or_default();
    let all: Vec<usize> = (0..rows.len()).collect();
    let (bad, status, code): (Vec<usize>, &str, Option<&str>) = match how {
        Settle::Accepted(req) => {
            sqlx::query("UPDATE ad_audience_batches SET status = 'accepted', platform_ref = $2 WHERE id = $1").bind(b.id).bind(req).execute(&mut **tx).await?;
            return Ok(());
        }
        Settle::All => (Vec::new(), "confirmed", None),
        Settle::Except(bad) if bad.is_empty() => (Vec::new(), "confirmed", None),
        Settle::Except(bad) => (bad.into_iter().filter(|i| *i < rows.len()).collect(), "partial", Some("partial")),
        Settle::None(code) => (all, if code == "rejected" { "rejected" } else { "abandoned" }, Some(code)),
    };
    let key = |i: usize| (rows[i]["t"].as_str().unwrap_or_default().to_string(), rows[i]["h"].as_str().unwrap_or_default().to_string());
    let (bad_t, bad_h): (Vec<String>, Vec<String>) = bad.iter().map(|i| key(*i)).unzip();
    let ok = rows.len() - bad.len();
    if b.op == "add" {
        // Not confirmed: possibly in the audience, so it's removed if it stops qualifying.
        sqlx::query(
            "UPDATE ad_audience_members m SET state = 'unconfirmed', batch_id = NULL FROM UNNEST($3::text[], $4::text[]) AS u(t, h)
             WHERE m.sync_id = $1 AND m.batch_id = $2 AND m.key_type = u.t AND m.key_hash = u.h",
        )
        .bind(sync_id)
        .bind(b.id)
        .bind(&bad_t)
        .bind(&bad_h)
        .execute(&mut **tx)
        .await?;
        if code == Some("stale") {
            sqlx::query(
                "UPDATE ad_audience_members m SET tried_run_id = NULL FROM UNNEST($2::text[], $3::text[]) AS u(t, h) WHERE m.sync_id = $1 AND m.key_type = u.t AND m.key_hash = u.h AND m.batch_id IS NULL",
            )
            .bind(sync_id)
            .bind(&bad_t)
            .bind(&bad_h)
            .execute(&mut **tx)
            .await?;
            sqlx::query("UPDATE ad_audience_desired d SET claimed = false FROM UNNEST($2::text[], $3::text[]) AS u(t, h) WHERE d.sync_id = $1 AND d.key_type = u.t AND d.key_hash = u.h")
                .bind(sync_id)
                .bind(&bad_t)
                .bind(&bad_h)
                .execute(&mut **tx)
                .await?;
        }
        sqlx::query("UPDATE ad_audience_members SET state = 'confirmed', confirmed_at = now(), batch_id = NULL WHERE sync_id = $1 AND batch_id = $2").bind(sync_id).bind(b.id).execute(&mut **tx).await?;
        sqlx::query("UPDATE ad_audience_runs SET added = added + $2, rejected = rejected + $3 WHERE id = $1").bind(run_id).bind(ok as i32).bind(bad.len() as i32).execute(&mut **tx).await?;
    } else {
        // Not confirmed: still waiting for removal, retried on a later run.
        sqlx::query(
            "UPDATE ad_audience_members m SET batch_id = NULL FROM UNNEST($3::text[], $4::text[]) AS u(t, h)
             WHERE m.sync_id = $1 AND m.batch_id = $2 AND m.key_type = u.t AND m.key_hash = u.h",
        )
        .bind(sync_id)
        .bind(b.id)
        .bind(&bad_t)
        .bind(&bad_h)
        .execute(&mut **tx)
        .await?;
        sqlx::query("DELETE FROM ad_audience_members WHERE sync_id = $1 AND batch_id = $2").bind(sync_id).bind(b.id).execute(&mut **tx).await?;
        sqlx::query("UPDATE ad_audience_runs SET removed = removed + $2, rejected = rejected + $3 WHERE id = $1").bind(run_id).bind(ok as i32).bind(bad.len() as i32).execute(&mut **tx).await?;
    }
    sqlx::query("UPDATE ad_audience_batches SET status = $2, result_code = $3, confirmed_count = $4, rejected_count = $5, settled_at = now() WHERE id = $1")
        .bind(b.id)
        .bind(status)
        .bind(code)
        .bind(ok as i32)
        .bind(bad.len() as i32)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Scheduler: start due syncs, pick up runs whose job died, prune sign-in states.
pub async fn schedule(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT s.id FROM ad_audience_syncs s JOIN ad_connections c ON c.id = s.connection_id
         WHERE s.current_run_id IS NULL AND s.next_run_at <= now() AND c.status = 'connected'
           AND (s.status = 'active' OR s.pause_reason = 'tiktok_minimum'
                OR (s.pause_reason IN ('user', 'google_consent') AND EXISTS (SELECT 1 FROM ad_audience_members m WHERE m.sync_id = s.id)))
         ORDER BY s.next_run_at LIMIT 50",
    )
    .fetch_all(&state.db)
    .await?;
    for id in due {
        start_run(state, id, "scheduled").await?;
    }
    // Waits top out at an hour, so a run silent for two whose lease is free lost its job: queue it
    // again. It picks up from what it wrote down (an unsettled batch is resent first).
    let stalled: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE ad_audience_runs r SET heartbeat_at = now() FROM ad_audience_syncs s
         WHERE r.status = 'running' AND r.heartbeat_at < now() - interval '2 hours' AND s.id = r.sync_id AND s.current_run_id = r.id
           AND (s.lease_until IS NULL OR s.lease_until < now())
         RETURNING r.id",
    )
    .fetch_all(&state.db)
    .await?;
    for run_id in stalled {
        crate::jobs::enqueue(&state.db, JOB_KIND, json!({"run_id": run_id}), None).await?;
    }
    // Runs whose sync moved on (deleted, or the run was replaced) are closed.
    sqlx::query(
        "UPDATE ad_audience_runs r SET status = 'failed', error_code = 'stopped_responding', finished_at = now()
         WHERE r.status = 'running' AND NOT EXISTS (SELECT 1 FROM ad_audience_syncs s WHERE s.id = r.sync_id AND s.current_run_id = r.id)",
    )
    .execute(&state.db)
    .await?;
    sqlx::query("DELETE FROM ad_oauth_states WHERE created_at < now() - interval '1 hour'").execute(&state.db).await?;
    Ok(())
}
