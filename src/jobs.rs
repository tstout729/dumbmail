//! Durable Postgres job queue and the worker/scheduler loops.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::state::AppState;

/// The lane sending, flows and segments run in. [`enqueue`] uses it.
pub const MAIN: &str = "main";
/// The lane for integration work that must never hold up sending (webhooks, exports, syncs). Only
/// the background workers (`BACKGROUND_WORKERS`, default 2) take these jobs, and they take nothing
/// else. Use [`enqueue_background`].
pub const BACKGROUND: &str = "background";

pub async fn enqueue<'e>(db: impl sqlx::PgExecutor<'e>, kind: &str, payload: Value, run_at: Option<DateTime<Utc>>) -> sqlx::Result<i64> {
    enqueue_in(db, MAIN, kind, payload, run_at).await
}

/// Like [`enqueue`], in the background lane.
pub async fn enqueue_background<'e>(db: impl sqlx::PgExecutor<'e>, kind: &str, payload: Value, run_at: Option<DateTime<Utc>>) -> sqlx::Result<i64> {
    enqueue_in(db, BACKGROUND, kind, payload, run_at).await
}

async fn enqueue_in<'e>(db: impl sqlx::PgExecutor<'e>, lane: &str, kind: &str, payload: Value, run_at: Option<DateTime<Utc>>) -> sqlx::Result<i64> {
    sqlx::query_scalar("INSERT INTO jobs (kind, payload, run_at, lane) VALUES ($1, $2, COALESCE($3, now()), $4) RETURNING id")
        .bind(kind)
        .bind(payload)
        .bind(run_at)
        .bind(lane)
        .fetch_one(db)
        .await
}

#[derive(sqlx::FromRow)]
struct Job {
    id: i64,
    kind: String,
    payload: Value,
    attempts: i32,
    max_attempts: i32,
}

/// Claim and run one ready main-lane job. Returns false when there is none.
pub async fn run_one(state: &AppState) -> anyhow::Result<bool> {
    run_one_in(state, MAIN).await
}

/// Claim and run one ready job of `lane`. Returns false when there is none.
pub async fn run_one_in(state: &AppState, lane: &str) -> anyhow::Result<bool> {
    let job: Option<Job> = sqlx::query_as(
        "UPDATE jobs SET locked_at = now(), attempts = attempts + 1
         WHERE id = (SELECT id FROM jobs WHERE locked_at IS NULL AND run_at <= now() AND lane = $1
                     ORDER BY run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED)
         RETURNING id, kind, payload, attempts, max_attempts",
    )
    .bind(lane)
    .fetch_optional(&state.db)
    .await?;
    let Some(job) = job else { return Ok(false) };
    let res = dispatch(state, &job.kind, &job.payload).await;
    match res {
        Ok(()) => {
            sqlx::query("DELETE FROM jobs WHERE id = $1").bind(job.id).execute(&state.db).await?;
        }
        Err(e) => {
            let msg = format!("{e:#}");
            tracing::warn!(job = job.id, kind = %job.kind, attempt = job.attempts, error = %msg, "job failed");
            if job.attempts >= job.max_attempts {
                // A back-populate is marked failed (freeing its flow) in the same transaction.
                let mut tx = state.db.begin().await?;
                sqlx::query("UPDATE jobs SET run_at = 'infinity', last_error = $2 WHERE id = $1")
                    .bind(job.id)
                    .bind(&msg)
                    .execute(&mut *tx)
                    .await?;
                if job.kind == crate::features::flows_plus::backfill::JOB {
                    crate::features::flows_plus::backfill::job_dead_in(&mut tx, &job.payload, &msg).await?;
                }
                tx.commit().await?;
                on_dead(state, &job.kind, &job.payload, &msg).await;
            } else {
                let backoff = 30i64 * 2i64.pow(job.attempts as u32);
                sqlx::query("UPDATE jobs SET locked_at = NULL, run_at = now() + make_interval(secs => $2), last_error = $3 WHERE id = $1")
                    .bind(job.id)
                    .bind(backoff as f64)
                    .bind(&msg)
                    .execute(&state.db)
                    .await?;
            }
        }
    }
    Ok(true)
}

async fn on_dead(state: &AppState, kind: &str, payload: &Value, msg: &str) {
    if kind == "campaign_send" {
        if let Some(id) = payload["campaign_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
            let _ = sqlx::query("UPDATE campaigns SET status = 'failed', error = $2 WHERE id = $1")
                .bind(id)
                .bind(msg)
                .execute(&state.db)
                .await;
        }
    }
}

/// Jobs that start work for an account, and how each finds that account (payload key, query).
/// [`dispatch`] skips them when the account is closed (see `models::account_closed`), so nothing
/// runs for a closed account, whichever feature queued the work. A job kind that starts work for
/// an account belongs here, unless it must settle what it was holding instead (a transactional
/// retry marks its message skipped, "account closed": see `transactional::send`).
const ACCOUNT_JOBS: &[(&str, &str, &str)] = &[
    ("campaign_send", "campaign_id", "SELECT account_id FROM campaigns WHERE id = $1"),
    ("campaign_batch", "campaign_id", "SELECT account_id FROM campaigns WHERE id = $1"),
    ("flow_event", "event_id", "SELECT account_id FROM events WHERE id = $1"),
    ("flow_list", "list_id", "SELECT account_id FROM lists WHERE id = $1"),
    ("flow_step", "run_id", "SELECT f.account_id FROM flow_runs r JOIN flows f ON f.id = r.flow_id WHERE r.id = $1"),
    ("segment_refresh", "segment_id", "SELECT account_id FROM segments WHERE id = $1"),
    ("catalog_feed_sync", "feed_id", "SELECT account_id FROM catalog_feeds WHERE id = $1"),
    ("catalog_verify", "product_id", "SELECT account_id FROM catalog_products WHERE id = $1"),
    ("catalog_back_in_stock", "restock_id", "SELECT account_id FROM catalog_restocks WHERE id = $1"),
    ("catalog_price_drop", "account_id", "SELECT $1::uuid"),
    ("reports_scheduled_send", "report_id", "SELECT account_id FROM saved_reports WHERE id = $1"),
    ("reports_export", "export_id", "SELECT account_id FROM exports WHERE id = $1"),
    ("reports_mx_lookup", "account_id", "SELECT $1::uuid"),
    ("webhooks_fanout", "account_id", "SELECT $1::uuid"),
    ("webhooks_deliver", "endpoint_id", "SELECT account_id FROM webhook_endpoints WHERE id = $1"),
    ("warehouse_run", "run_id", "SELECT account_id FROM warehouse_runs WHERE id = $1"),
    ("inbox_auto_send", "id", "SELECT account_id FROM sms_auto_sends WHERE id = $1"),
    ("inbox_digest", "account_id", "SELECT $1::uuid"),
    ("coupons_alert", "coupon_id", "SELECT account_id FROM coupons WHERE id = $1"),
    ("klaviyo_import_survey", "import_id", "SELECT account_id FROM klaviyo_imports WHERE id = $1"),
    ("klaviyo_import_run", "import_id", "SELECT account_id FROM klaviyo_imports WHERE id = $1"),
    ("klaviyo_rebaseline_segments", "account_id", "SELECT $1::uuid"),
    ("objects_change", "change_id", "SELECT account_id FROM object_changes WHERE id = $1"),
    ("objects_bulk", "bulk_id", "SELECT account_id FROM object_bulk_upserts WHERE id = $1"),
    ("flows_plus_backfill", "backfill_id", "SELECT account_id FROM flow_backfills WHERE id = $1"),
    ("flows_plus_confirm_address", "account_id", "SELECT $1::uuid"),
    // Shopify: everything but privacy requests (`shopify_privacy`) and the store worker
    // (`shopify_work`), which for a closed account only does privacy requests and opt-out
    // write-backs (see `features::shopify`).
    ("shopify_setup", "account_id", "SELECT $1::uuid"),
    ("shopify_rebaseline_segments", "account_id", "SELECT $1::uuid"),
];

/// Whether this job is for a closed account (one indexed lookup).
async fn for_closed_account(state: &AppState, kind: &str, p: &Value) -> sqlx::Result<bool> {
    let Some((_, key, owner)) = ACCOUNT_JOBS.iter().find(|(k, ..)| *k == kind) else { return Ok(false) };
    let Some(id) = p[*key].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(false) };
    sqlx::query_scalar(&format!("SELECT EXISTS (SELECT 1 FROM accounts a WHERE a.closed_at IS NOT NULL AND a.id = ({owner}))")).bind(id).fetch_one(&state.db).await
}

async fn dispatch(state: &AppState, kind: &str, p: &Value) -> anyhow::Result<()> {
    if for_closed_account(state, kind, p).await? {
        tracing::info!(kind, "job skipped: its account is closed");
        return Ok(());
    }
    let uuid = |k: &str| -> anyhow::Result<Uuid> {
        p[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("job payload missing {k}"))
    };
    match kind {
        "campaign_send" => crate::campaigns::prepare_send(state, uuid("campaign_id")?).await,
        "campaign_batch" => {
            let ids: Vec<Uuid> = serde_json::from_value(p["message_ids"].clone())?;
            crate::campaigns::send_batch(state, uuid("campaign_id")?, &ids).await
        }
        "flow_event" => {
            let id = uuid("event_id")?;
            crate::flows::on_event(state, id).await?;
            crate::features::on_event(state, id).await;
            Ok(())
        }
        "flow_list" => {
            let ids: Vec<Uuid> = serde_json::from_value(p["profile_ids"].clone())?;
            crate::flows::on_list_add(state, uuid("list_id")?, &ids).await
        }
        "flow_step" => crate::flows::step_job(state, p).await,
        "segment_refresh" => crate::flows::refresh_segment(state, uuid("segment_id")?).await,
        other => match crate::features::run_job(state, other, p).await {
            Some(r) => r,
            None => {
                tracing::error!(kind = other, "unknown job kind; dropping");
                Ok(())
            }
        },
    }
}

/// Run until both lanes are empty (used by tests and the `work-once` command).
pub async fn drain(state: &AppState) -> anyhow::Result<usize> {
    let mut n = 0;
    loop {
        let mut ran = false;
        for lane in [MAIN, BACKGROUND] {
            while run_one_in(state, lane).await? {
                ran = true;
                n += 1;
                if n > 100_000 {
                    return Ok(n);
                }
            }
        }
        if !ran {
            return Ok(n);
        }
    }
}

/// Run until `lane` is empty, leaving the other lane's jobs where they are.
pub async fn drain_lane(state: &AppState, lane: &str) -> anyhow::Result<usize> {
    let mut n = 0;
    while n <= 100_000 && run_one_in(state, lane).await? {
        n += 1;
    }
    Ok(n)
}

pub fn spawn_workers(state: AppState) {
    crate::features::spawn_after_start(state.clone());
    let lanes = std::iter::repeat_n(MAIN, state.cfg.workers.max(1)).chain(std::iter::repeat_n(BACKGROUND, state.cfg.background_workers.max(1)));
    for (i, lane) in lanes.enumerate() {
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                match run_one_in(&st, lane).await {
                    Ok(true) => {}
                    Ok(false) => tokio::time::sleep(Duration::from_millis(500)).await,
                    Err(e) => {
                        tracing::error!(worker = i, lane, error = %e, "worker error");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });
    }
    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            if let Err(e) = scheduler_tick(&state, tick).await {
                tracing::error!(error = %e, "scheduler error");
            }
            tick += 1;
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    });
}

/// Periodic work: due campaigns, stale locks, segment refreshes and date-based flows.
pub async fn scheduler_tick(state: &AppState, tick: u64) -> anyhow::Result<()> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE campaigns SET status = 'sending', updated_at = now()
         WHERE status = 'scheduled' AND send_at <= now()
           AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = campaigns.account_id AND a.closed_at IS NOT NULL) RETURNING id",
    )
    .fetch_all(&state.db)
    .await?;
    for id in due {
        enqueue(&state.db, "campaign_send", json!({"campaign_id": id}), None).await?;
    }
    crate::campaigns::release_abandoned_claims(state).await?;
    crate::campaigns::finish_due_tests(state).await?;
    crate::deliverability::check_pending(state).await?;
    if tick.is_multiple_of(180) {
        sqlx::query("DELETE FROM site_tests WHERE expires_at < now() - interval '7 days'").execute(&state.db).await?;
        crate::deliverability::prune(&state.db).await?;
    }
    sqlx::query("UPDATE jobs SET locked_at = NULL WHERE locked_at < now() - interval '15 minutes' AND run_at <> 'infinity'")
        .execute(&state.db)
        .await?;
    // Every ~5 minutes: refresh segments used by live flows, and every other segment once it's 15
    // minutes old, so member counts stay current. Skips segments that already have a refresh queued.
    // Hourly-ish: date property flows.
    if tick.is_multiple_of(15) {
        let segs: Vec<Uuid> = sqlx::query_scalar(
            "SELECT s.id FROM segments s
             WHERE (s.refreshed_at IS NULL OR s.refreshed_at < now() - interval '15 minutes'
                    OR EXISTS (SELECT 1 FROM flows f WHERE f.status = 'live' AND f.trigger->>'type' = 'segment' AND f.trigger->>'segment_id' = s.id::text))
               AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'segment_refresh' AND j.payload->>'segment_id' = s.id::text AND j.run_at <> 'infinity')
               -- Closed accounts' segments never take a place in the batch.
               AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = s.account_id AND a.closed_at IS NOT NULL)
             ORDER BY s.refreshed_at NULLS FIRST
             LIMIT 500",
        )
        .fetch_all(&state.db)
        .await?;
        for s in segs {
            enqueue(&state.db, "segment_refresh", json!({"segment_id": s}), None).await?;
        }
    }
    if tick.is_multiple_of(3) {
        crate::billing::run_auto_recharge(state).await?;
        crate::subscription::run_renewals(state).await?;
        crate::billing::charge_numbers(state).await?;
    }
    if tick.is_multiple_of(45) {
        crate::phone::poll_verifications(state).await?;
    }
    // Every ~10 minutes: the provider's sending rate can change (SES raises it as the account grows).
    if tick.is_multiple_of(30) {
        if let Err(e) = state.email.refresh_limits().await {
            tracing::warn!(error = %format!("{e:#}"), "couldn't refresh the email provider's sending limits");
        }
    }
    if tick.is_multiple_of(90) {
        crate::flows::date_triggers(state).await?;
    }
    crate::features::tick(state, tick).await;
    Ok(())
}
