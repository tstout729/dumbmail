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
                sqlx::query("UPDATE jobs SET run_at = 'infinity', last_error = $2 WHERE id = $1")
                    .bind(job.id)
                    .bind(&msg)
                    .execute(&state.db)
                    .await?;
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

async fn dispatch(state: &AppState, kind: &str, p: &Value) -> anyhow::Result<()> {
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
        "flow_step" => crate::flows::step(state, uuid("run_id")?).await,
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
