//! Durable Postgres job queue and the worker/scheduler loops.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::state::AppState;

pub async fn enqueue<'e>(db: impl sqlx::PgExecutor<'e>, kind: &str, payload: Value, run_at: Option<DateTime<Utc>>) -> sqlx::Result<i64> {
    sqlx::query_scalar("INSERT INTO jobs (kind, payload, run_at) VALUES ($1, $2, COALESCE($3, now())) RETURNING id")
        .bind(kind)
        .bind(payload)
        .bind(run_at)
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

/// Claim and run one ready job. Returns false when the queue is empty.
pub async fn run_one(state: &AppState) -> anyhow::Result<bool> {
    let job: Option<Job> = sqlx::query_as(
        "UPDATE jobs SET locked_at = now(), attempts = attempts + 1
         WHERE id = (SELECT id FROM jobs WHERE locked_at IS NULL AND run_at <= now()
                     ORDER BY run_at, id LIMIT 1 FOR UPDATE SKIP LOCKED)
         RETURNING id, kind, payload, attempts, max_attempts",
    )
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
        "flow_event" => crate::flows::on_event(state, uuid("event_id")?).await,
        "flow_list" => {
            let ids: Vec<Uuid> = serde_json::from_value(p["profile_ids"].clone())?;
            crate::flows::on_list_add(state, uuid("list_id")?, &ids).await
        }
        "flow_step" => crate::flows::step(state, uuid("run_id")?).await,
        "segment_refresh" => crate::flows::refresh_segment(state, uuid("segment_id")?).await,
        other => {
            tracing::error!(kind = other, "unknown job kind; dropping");
            Ok(())
        }
    }
}

/// Run until the queue is empty (used by tests and the `work-once` command).
pub async fn drain(state: &AppState) -> anyhow::Result<usize> {
    let mut n = 0;
    while run_one(state).await? {
        n += 1;
        if n > 100_000 {
            break;
        }
    }
    Ok(n)
}

pub fn spawn_workers(state: AppState) {
    for i in 0..state.cfg.workers.max(1) {
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                match run_one(&st).await {
                    Ok(true) => {}
                    Ok(false) => tokio::time::sleep(Duration::from_millis(500)).await,
                    Err(e) => {
                        tracing::error!(worker = i, error = %e, "worker error");
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
         WHERE status = 'scheduled' AND send_at <= now() RETURNING id",
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
    }
    sqlx::query("UPDATE jobs SET locked_at = NULL WHERE locked_at < now() - interval '15 minutes' AND run_at <> 'infinity'")
        .execute(&state.db)
        .await?;
    // Every ~5 minutes: refresh segments used by live flows; hourly-ish: date property flows.
    if tick.is_multiple_of(15) {
        let segs: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT (trigger->>'segment_id')::uuid FROM flows WHERE status = 'live' AND trigger->>'type' = 'segment'",
        )
        .fetch_all(&state.db)
        .await?;
        for s in segs {
            enqueue(&state.db, "segment_refresh", json!({"segment_id": s}), None).await?;
        }
    }
    if tick.is_multiple_of(3) {
        crate::billing::run_auto_recharge(state).await?;
        crate::billing::charge_numbers(state).await?;
    }
    if tick.is_multiple_of(45) {
        crate::phone::poll_verifications(state).await?;
    }
    if tick.is_multiple_of(90) {
        crate::flows::date_triggers(state).await?;
    }
    Ok(())
}
