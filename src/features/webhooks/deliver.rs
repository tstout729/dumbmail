//! Sending queued deliveries to an endpoint.
//!
//! One `webhooks_deliver` job per endpoint at a time (the `wake_queued` flag, plus a lease so two
//! workers never send to one endpoint at once). A run sends due deliveries oldest first, up to
//! `batch_size` per request and `rate_per_second` requests, for at most ~20 seconds, then queues
//! itself again if more are waiting. A failure backs off that delivery and the whole endpoint, so a
//! dead endpoint costs one request per back-off period and never holds up anyone else.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::net;
use crate::state::AppState;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const RUN_BUDGET: Duration = Duration::from_secs(20);
const MAX_REQUESTS_PER_RUN: usize = 100;
/// Response bodies kept in the delivery log.
const LOG_BODY_BYTES: usize = 1024;
/// Endpoints failing this long are turned off.
const DISABLE_AFTER_DAYS: f64 = 3.0;

#[derive(sqlx::FromRow)]
pub(super) struct Endpoint {
    pub id: Uuid,
    pub account_id: Uuid,
    pub url: String,
    pub source: String,
    pub batch_size: i32,
    pub rate_per_second: i32,
    pub secret_enc: String,
    pub previous_secret_enc: Option<String>,
    pub previous_secret_until: Option<DateTime<Utc>>,
    pub retry_at: Option<DateTime<Utc>>,
}

pub(super) const ENDPOINT_COLS: &str = "id, account_id, url, source, batch_size, rate_per_second, secret_enc, previous_secret_enc, previous_secret_until, retry_at";

/// Queue a delivery job for each endpoint that doesn't have one yet.
pub(super) async fn wake(db: &PgPool, endpoint_ids: &[Uuid]) -> anyhow::Result<()> {
    if endpoint_ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<(Uuid, Option<DateTime<Utc>>)> =
        sqlx::query_as("UPDATE webhook_endpoints SET wake_queued = true WHERE id = ANY($1) AND enabled AND NOT wake_queued RETURNING id, retry_at")
            .bind(endpoint_ids)
            .fetch_all(db)
            .await?;
    for (id, retry_at) in rows {
        let at = retry_at.filter(|t| *t > Utc::now());
        crate::jobs::enqueue(db, "webhooks_deliver", json!({"endpoint_id": id}), at).await?;
    }
    Ok(())
}

/// After a run: queue the next one for when the earliest waiting delivery is due.
async fn reschedule(db: &PgPool, id: Uuid) -> anyhow::Result<()> {
    let next: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT GREATEST(min(d.next_attempt_at), max(e.retry_at)) FROM webhook_deliveries d JOIN webhook_endpoints e ON e.id = d.endpoint_id
         WHERE d.endpoint_id = $1 AND d.status = 'pending' AND e.enabled",
    )
    .bind(id)
    .fetch_one(db)
    .await?;
    let Some(next) = next else { return Ok(()) };
    let queued: Option<Uuid> = sqlx::query_scalar("UPDATE webhook_endpoints SET wake_queued = true WHERE id = $1 AND enabled AND NOT wake_queued RETURNING id")
        .bind(id)
        .fetch_optional(db)
        .await?;
    if queued.is_some() {
        crate::jobs::enqueue(db, "webhooks_deliver", json!({"endpoint_id": id}), Some(next.max(Utc::now()))).await?;
    }
    Ok(())
}

pub(super) fn secrets(state: &AppState, ep: &Endpoint) -> anyhow::Result<Vec<String>> {
    let mut out = vec![crate::crypto::decrypt(&state.cfg.secret_key, &ep.secret_enc)?];
    if let (Some(prev), Some(until)) = (&ep.previous_secret_enc, ep.previous_secret_until) {
        if until > Utc::now() {
            out.push(crate::crypto::decrypt(&state.cfg.secret_key, prev)?);
        }
    }
    Ok(out)
}

pub(super) struct Outcome {
    pub ok: bool,
    pub code: Option<i32>,
    pub body: Option<String>,
    pub error: Option<String>,
    pub ms: i32,
}

/// POST one request body to the endpoint, signed with every live secret.
pub(super) async fn send(url: &str, secrets: &[String], topic: Option<&str>, body: &[u8]) -> Outcome {
    let started = Instant::now();
    let fail = |error: String, code: Option<i32>, body: Option<String>| Outcome { ok: false, code, body, error: Some(error), ms: started.elapsed().as_millis() as i32 };
    let target = match net::check(url, "webhook url").await {
        Ok(t) => t,
        Err(e) => return fail(e.public_message(), None, None),
    };
    let client = match net::client(&target, REQUEST_TIMEOUT) {
        Ok(c) => c,
        Err(e) => return fail(format!("{e:#}"), None, None),
    };
    let mut req = client.post(target.url.clone()).header("content-type", "application/json").body(body.to_vec());
    for (k, v) in super::signature_headers(secrets, body) {
        req = req.header(k, v);
    }
    if let Some(t) = topic {
        req = req.header("DumbMail-Webhook-Topic", t);
    }
    let res = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return fail(format!("no response within {} seconds", REQUEST_TIMEOUT.as_secs()), None, None),
        Err(e) if e.is_connect() => return fail("couldn't connect".into(), None, None),
        Err(e) => return fail(format!("request failed: {}", e.without_url()), None, None),
    };
    let status = res.status();
    let code = Some(status.as_u16() as i32);
    let text = String::from_utf8_lossy(&net::read_capped(res, LOG_BODY_BYTES).await).into_owned();
    let body = Some(text).filter(|t| !t.is_empty());
    let ms = started.elapsed().as_millis() as i32;
    if status.is_success() {
        Outcome { ok: true, code, body, error: None, ms }
    } else if status.is_redirection() {
        Outcome { ok: false, code, body, error: Some(format!("HTTP {}: redirects aren't followed; use the final URL", status.as_u16())), ms }
    } else {
        Outcome { ok: false, code, body, error: Some(format!("HTTP {}", status.as_u16())), ms }
    }
}

/// Record an attempt on deliveries. Failed ones back off (30s doubling, at most 2 hours) and are
/// marked failed once the next try would be past their 24 hours.
pub(super) async fn record(db: &PgPool, ids: &[Uuid], o: &Outcome) -> anyhow::Result<()> {
    if o.ok {
        sqlx::query(
            "UPDATE webhook_deliveries SET status = 'delivered', attempts = attempts + 1, last_attempt_at = now(), delivered_at = now(),
               response_code = $2, response_body = $3, error = NULL, duration_ms = $4 WHERE id = ANY($1)",
        )
        .bind(ids)
        .bind(o.code)
        .bind(&o.body)
        .bind(o.ms)
        .execute(db)
        .await?;
    } else {
        sqlx::query(
            "UPDATE webhook_deliveries SET attempts = attempts + 1, last_attempt_at = now(), response_code = $2, response_body = $3, error = $4, duration_ms = $5,
               next_attempt_at = now() + make_interval(secs => LEAST(30 * power(2, LEAST(attempts, 12)), 7200)),
               status = CASE WHEN now() + make_interval(secs => LEAST(30 * power(2, LEAST(attempts, 12)), 7200)) > expires_at THEN 'failed' ELSE 'pending' END
             WHERE id = ANY($1)",
        )
        .bind(ids)
        .bind(o.code)
        .bind(&o.body)
        .bind(&o.error)
        .bind(o.ms)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Send what's due for one endpoint.
pub(super) async fn run(state: &AppState, id: Uuid) -> anyhow::Result<()> {
    let db = &state.db;
    sqlx::query("UPDATE webhook_endpoints SET wake_queued = false WHERE id = $1").bind(id).execute(db).await?;
    let ep: Option<Endpoint> = sqlx::query_as(&format!(
        "UPDATE webhook_endpoints SET lease_until = now() + interval '2 minutes'
         WHERE id = $1 AND enabled AND (lease_until IS NULL OR lease_until < now()) RETURNING {ENDPOINT_COLS}"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;
    // Turned off, deleted, or another worker is sending (it checks for more when it's done).
    let Some(ep) = ep else { return Ok(()) };
    let res = send_due(state, &ep).await;
    sqlx::query("UPDATE webhook_endpoints SET lease_until = NULL WHERE id = $1").bind(id).execute(db).await?;
    if let Err(e) = &res {
        tracing::warn!(endpoint = %id, error = %format!("{e:#}"), "webhook delivery run failed");
    }
    reschedule(db, id).await
}

async fn send_due(state: &AppState, ep: &Endpoint) -> anyhow::Result<()> {
    let db = &state.db;
    if ep.retry_at.is_some_and(|t| t > Utc::now()) {
        return Ok(());
    }
    let secrets = secrets(state, ep)?;
    let started = Instant::now();
    let pause = Duration::from_millis(1000 / ep.rate_per_second.clamp(1, 100) as u64);
    for n in 0..MAX_REQUESTS_PER_RUN {
        if n > 0 {
            if started.elapsed() > RUN_BUDGET {
                break;
            }
            tokio::time::sleep(pause).await;
        }
        sqlx::query("UPDATE webhook_deliveries SET status = 'failed', error = COALESCE(error, 'not delivered within 24 hours') WHERE endpoint_id = $1 AND status = 'pending' AND expires_at < now()")
            .bind(ep.id)
            .execute(db)
            .await?;
        let batch: Vec<(Uuid, String, Value)> = sqlx::query_as(
            "SELECT id, topic, payload FROM webhook_deliveries WHERE endpoint_id = $1 AND status = 'pending' AND next_attempt_at <= now()
             ORDER BY created_at, id LIMIT $2",
        )
        .bind(ep.id)
        .bind(ep.batch_size.clamp(1, 100) as i64)
        .fetch_all(db)
        .await?;
        if batch.is_empty() {
            break;
        }
        let ids: Vec<Uuid> = batch.iter().map(|b| b.0).collect();
        let (body, topic) = if ep.batch_size <= 1 {
            (serde_json::to_vec(&batch[0].2)?, Some(batch[0].1.as_str()))
        } else {
            (serde_json::to_vec(&Value::Array(batch.iter().map(|b| b.2.clone()).collect()))?, None)
        };
        let outcome = send(&ep.url, &secrets, topic, &body).await;
        record(db, &ids, &outcome).await?;
        if outcome.ok {
            sqlx::query("UPDATE webhook_endpoints SET consecutive_failures = 0, failing_since = NULL, retry_at = NULL, last_success_at = now() WHERE id = $1")
                .bind(ep.id)
                .execute(db)
                .await?;
            continue;
        }
        if outcome.code == Some(410) {
            gone(state, ep).await?;
            return Ok(());
        }
        let failing_since: DateTime<Utc> = sqlx::query_scalar(
            "UPDATE webhook_endpoints SET consecutive_failures = consecutive_failures + 1, failing_since = COALESCE(failing_since, now()), last_failure_at = now(),
               retry_at = now() + make_interval(secs => LEAST(30 * power(2, LEAST(consecutive_failures, 12)), 3600))
             WHERE id = $1 RETURNING failing_since",
        )
        .bind(ep.id)
        .fetch_one(db)
        .await?;
        if (Utc::now() - failing_since).num_seconds() as f64 > DISABLE_AFTER_DAYS * 86_400.0 {
            disable(state, ep.id, ep.account_id, &ep.url, "It failed every delivery for 3 days, so it was turned off.").await?;
        }
        break;
    }
    Ok(())
}

/// HTTP 410 means "stop sending": Zapier and Make return it once a hook is gone. Hooks made
/// through the API are removed; dashboard endpoints are turned off.
async fn gone(state: &AppState, ep: &Endpoint) -> anyhow::Result<()> {
    if ep.source == "dashboard" {
        disable(state, ep.id, ep.account_id, &ep.url, "It answered 410 Gone, which means it no longer wants deliveries, so it was turned off.").await
    } else {
        sqlx::query("DELETE FROM webhook_endpoints WHERE id = $1").bind(ep.id).execute(&state.db).await?;
        super::forget(ep.account_id);
        Ok(())
    }
}

/// Turn an endpoint off, fail what it still had waiting, and tell the account.
pub(super) async fn disable(state: &AppState, id: Uuid, account_id: Uuid, url: &str, why: &str) -> anyhow::Result<()> {
    let changed: Option<Uuid> = sqlx::query_scalar("UPDATE webhook_endpoints SET enabled = false, disabled_reason = $2, updated_at = now() WHERE id = $1 AND enabled RETURNING id")
        .bind(id)
        .bind(why)
        .fetch_optional(&state.db)
        .await?;
    if changed.is_none() {
        return Ok(());
    }
    sqlx::query("UPDATE webhook_deliveries SET status = 'failed', error = COALESCE(error, 'endpoint turned off') WHERE endpoint_id = $1 AND status = 'pending'")
        .bind(id)
        .execute(&state.db)
        .await?;
    super::forget(account_id);
    let text = format!(
        "Your webhook endpoint {url} was turned off. {why}\n\nNothing more is sent to it until you turn it back on in Settings → Webhooks. After you fix it, you can resend the failed deliveries from its log there."
    );
    super::alert_account(state, account_id, "A webhook endpoint was turned off", &text).await;
    Ok(())
}

/// Every minute: queue endpoints that have deliveries waiting but no job (after a crash or a
/// resend), and clear a stale "job queued" flag whose job is gone.
pub(super) async fn sweep(state: &AppState) -> anyhow::Result<()> {
    let db = &state.db;
    sqlx::query(
        "UPDATE webhook_endpoints e SET wake_queued = false WHERE e.wake_queued
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'webhooks_deliver' AND j.payload->>'endpoint_id' = e.id::text AND j.run_at <> 'infinity')",
    )
    .execute(db)
    .await?;
    let idle: Vec<Uuid> = sqlx::query_scalar(
        "SELECT e.id FROM webhook_endpoints e WHERE e.enabled AND NOT e.wake_queued AND (e.lease_until IS NULL OR e.lease_until < now())
           AND EXISTS (SELECT 1 FROM webhook_deliveries d WHERE d.endpoint_id = e.id AND d.status = 'pending')
         LIMIT 1000",
    )
    .fetch_all(db)
    .await?;
    for id in idle {
        reschedule(db, id).await?;
    }
    Ok(())
}

/// Delivery logs (and their payloads) are kept for 14 days.
pub(super) async fn prune(db: &PgPool) -> anyhow::Result<()> {
    for _ in 0..20 {
        let n = sqlx::query(
            "DELETE FROM webhook_deliveries WHERE id IN (SELECT id FROM webhook_deliveries WHERE created_at < now() - interval '14 days' AND status <> 'pending' LIMIT 10000)",
        )
        .execute(db)
        .await?
        .rows_affected();
        if n < 10000 {
            break;
        }
    }
    Ok(())
}
