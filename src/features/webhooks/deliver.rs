//! Sending queued deliveries to an endpoint, in the background job lane.
//!
//! One `webhooks_deliver` job per endpoint at a time (the `wake_queued` flag, plus a lease so two
//! workers never send to one endpoint at once: a slow or dead endpoint occupies at most one
//! background worker). A run sends due deliveries oldest first, up to `batch_size` events and
//! [`MAX_REQUEST_BYTES`] per request and `rate_per_second` requests, for at most ~20 seconds, then
//! queues itself again if more are waiting. A failure backs off that delivery and the whole
//! endpoint, so a dead endpoint costs one request per back-off period.
//!
//! Only deliveries queued for the endpoint's current revision are sent: changing the URL cancels
//! the rest rather than sending them somewhere new. Stored errors are a short class from a fixed
//! list (see [`Outcome`]) and the response code, never a response body.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::net;
use crate::state::AppState;

/// One deadline for checking the address, connecting, sending and reading the answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const RUN_BUDGET: Duration = Duration::from_secs(20);
const MAX_REQUESTS_PER_RUN: usize = 100;
/// A request carries at most this many bytes of events. A single event bigger than this isn't sent:
/// it's recorded as failed with `too_large`.
pub(super) const MAX_REQUEST_BYTES: usize = 1_000_000;

#[derive(sqlx::FromRow)]
pub(super) struct Endpoint {
    pub id: Uuid,
    pub account_id: Uuid,
    pub url: String,
    pub description: String,
    pub source: String,
    pub revision: i32,
    pub batch_size: i32,
    pub rate_per_second: i32,
    pub secret_enc: String,
    pub previous_secret_enc: Option<String>,
    pub previous_secret_until: Option<DateTime<Utc>>,
    pub retry_at: Option<DateTime<Utc>>,
}

/// The endpoint (as `webhook_endpoints`) isn't a REST hook of a revoked API key.
pub(super) const KEY_LIVE: &str = "(webhook_endpoints.api_key_id IS NULL OR EXISTS (SELECT 1 FROM api_keys k WHERE k.id = webhook_endpoints.api_key_id AND k.revoked_at IS NULL))";

pub(super) const ENDPOINT_COLS: &str =
    "id, account_id, url, description, source, revision, batch_size, rate_per_second, secret_enc, previous_secret_enc, previous_secret_until, retry_at";

/// Queue a delivery job for each endpoint that doesn't have one yet (in the caller's transaction).
pub(super) async fn wake(conn: &mut PgConnection, endpoint_ids: &[Uuid]) -> sqlx::Result<()> {
    if endpoint_ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<(Uuid, Option<DateTime<Utc>>)> =
        sqlx::query_as("UPDATE webhook_endpoints SET wake_queued = true WHERE id = ANY($1) AND enabled AND NOT wake_queued RETURNING id, retry_at")
            .bind(endpoint_ids)
            .fetch_all(&mut *conn)
            .await?;
    for (id, retry_at) in rows {
        let at = retry_at.filter(|t| *t > Utc::now());
        crate::jobs::enqueue_background(&mut *conn, "webhooks_deliver", json!({"endpoint_id": id}), at).await?;
    }
    Ok(())
}

/// After a run: queue the next one for when the earliest waiting delivery is due.
async fn reschedule(db: &PgPool, id: Uuid) -> anyhow::Result<()> {
    let next: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT GREATEST(min(d.next_attempt_at), max(e.retry_at)) FROM webhook_deliveries d JOIN webhook_endpoints e ON e.id = d.endpoint_id
         WHERE d.endpoint_id = $1 AND d.status = 'pending' AND d.revision = e.revision AND d.expires_at > now() AND e.enabled",
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
        crate::jobs::enqueue_background(db, "webhooks_deliver", json!({"endpoint_id": id}), Some(next.max(Utc::now()))).await?;
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

/// What happened to a request. `error` is one of: `address_rejected` (the URL isn't a public
/// HTTPS address), `timeout`, `connect_failed`, `request_failed`, `redirect` (a 3xx: use the final
/// URL), `http_error` (any other non-2xx; see the code), `internal`. Deliveries never sent say
/// `too_large` (one event over [`MAX_REQUEST_BYTES`]) or, cancelled, `untrusted_source` (the
/// endpoint now takes only trusted sources) or `url_changed`.
pub(super) struct Outcome {
    pub ok: bool,
    pub code: Option<i32>,
    pub error: Option<&'static str>,
    pub ms: i32,
}

/// POST one request body to the endpoint, signed with every live secret.
pub(super) async fn send(url: &str, secrets: &[String], topic: Option<&str>, body: &[u8]) -> Outcome {
    let started = Instant::now();
    let res = net::within(REQUEST_TIMEOUT, async {
        let target = match net::check(url, "webhook url").await {
            Ok(t) => t,
            Err(_) => return (None, Some("address_rejected")),
        };
        let Ok(client) = net::client(&target, REQUEST_TIMEOUT) else { return (None, Some("internal")) };
        let mut req = client.post(target.url.clone()).header("content-type", "application/json").body(body.to_vec());
        for (k, v) in super::signature_headers(secrets, body) {
            req = req.header(k, v);
        }
        if let Some(t) = topic {
            req = req.header("DumbMail-Webhook-Topic", t);
        }
        let res = match req.send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return (None, Some("timeout")),
            Err(e) if e.is_connect() => return (None, Some("connect_failed")),
            Err(_) => return (None, Some("request_failed")),
        };
        let status = res.status();
        // Read (a little of) the answer so the connection finishes cleanly; it's never kept.
        let _ = net::read_capped(res, 4096).await;
        let code = Some(status.as_u16() as i32);
        if status.is_success() {
            (code, None)
        } else if status.is_redirection() {
            (code, Some("redirect"))
        } else {
            (code, Some("http_error"))
        }
    })
    .await;
    let (code, error) = res.unwrap_or((None, Some("timeout")));
    Outcome { ok: error.is_none(), code, error, ms: started.elapsed().as_millis() as i32 }
}

/// Lower an endpoint's undelivered count after rows left `pending`.
async fn left_pending<'c>(db: impl sqlx::PgExecutor<'c>, endpoint_id: Uuid, n: i64) -> sqlx::Result<()> {
    if n > 0 {
        sqlx::query("UPDATE webhook_endpoints SET pending = GREATEST(0, pending - $2) WHERE id = $1").bind(endpoint_id).bind(n as i32).execute(db).await?;
    }
    Ok(())
}

/// End pending deliveries without sending them (`status` failed or cancelled, with a short class).
async fn close(db: &PgPool, endpoint_id: Uuid, ids: &[Uuid], status: &str, error: &str) -> sqlx::Result<()> {
    let n = sqlx::query("UPDATE webhook_deliveries SET status = $2, error = $3 WHERE id = ANY($1) AND status = 'pending'")
        .bind(ids)
        .bind(status)
        .bind(error)
        .execute(db)
        .await?
        .rows_affected();
    left_pending(db, endpoint_id, n as i64).await
}

/// Record an attempt on pending deliveries. Failed ones back off (30s doubling, at most 2 hours)
/// and are marked failed once the next try would be past their 24 hours.
pub(super) async fn record(db: &PgPool, endpoint_id: Uuid, ids: &[Uuid], o: &Outcome) -> anyhow::Result<()> {
    let left: i64 = if o.ok {
        sqlx::query(
            "UPDATE webhook_deliveries SET status = 'delivered', attempts = attempts + 1, last_attempt_at = now(), delivered_at = now(),
               response_code = $2, error = NULL, duration_ms = $3 WHERE id = ANY($1) AND status = 'pending'",
        )
        .bind(ids)
        .bind(o.code)
        .bind(o.ms)
        .execute(db)
        .await?
        .rows_affected() as i64
    } else {
        let statuses: Vec<String> = sqlx::query_scalar(
            "UPDATE webhook_deliveries SET attempts = attempts + 1, last_attempt_at = now(), response_code = $2, error = $3, duration_ms = $4,
               next_attempt_at = now() + make_interval(secs => LEAST(30 * power(2, LEAST(attempts, 12)), 7200)),
               status = CASE WHEN now() + make_interval(secs => LEAST(30 * power(2, LEAST(attempts, 12)), 7200)) > expires_at THEN 'failed' ELSE 'pending' END
             WHERE id = ANY($1) AND status = 'pending' RETURNING status",
        )
        .bind(ids)
        .bind(o.code)
        .bind(o.error)
        .bind(o.ms)
        .fetch_all(db)
        .await?;
        statuses.iter().filter(|s| *s == "failed").count() as i64
    };
    left_pending(db, endpoint_id, left).await?;
    Ok(())
}

/// Send what's due for one endpoint.
pub(super) async fn run(state: &AppState, id: Uuid) -> anyhow::Result<()> {
    let db = &state.db;
    sqlx::query("UPDATE webhook_endpoints SET wake_queued = false WHERE id = $1").bind(id).execute(db).await?;
    // A REST hook whose API key was revoked gets nothing more, from this moment (the sweep then
    // deletes it).
    let ep: Option<Endpoint> = sqlx::query_as(&format!(
        "UPDATE webhook_endpoints SET lease_until = now() + interval '2 minutes'
         WHERE id = $1 AND enabled AND (lease_until IS NULL OR lease_until < now()) AND {KEY_LIVE} RETURNING {ENDPOINT_COLS}"
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
        // The endpoint is checked on every batch: a URL change (a new revision), turning it off, a
        // revoked API key or switching to trusted sources only stops this run from sending
        // anything it no longer should, including deliveries queued again by a resend.
        let batch: Vec<(Uuid, String, Value, bool)> = sqlx::query_as(&format!(
            "SELECT d.id, d.topic, d.payload, (webhook_endpoints.trusted_only AND d.payload->>'source' = 'browser') AS untrusted
             FROM webhook_deliveries d JOIN webhook_endpoints ON webhook_endpoints.id = d.endpoint_id
             WHERE d.endpoint_id = $1 AND d.status = 'pending' AND d.revision = $2 AND d.next_attempt_at <= now() AND d.expires_at > now()
               AND webhook_endpoints.enabled AND webhook_endpoints.revision = $2 AND {KEY_LIVE}
             ORDER BY d.created_at, d.id LIMIT $3"
        ))
        .bind(ep.id)
        .bind(ep.revision)
        .bind(ep.batch_size.clamp(1, 100) as i64)
        .fetch_all(db)
        .await?;
        if batch.is_empty() {
            break;
        }
        let untrusted: Vec<Uuid> = batch.iter().filter(|b| b.3).map(|b| b.0).collect();
        if !untrusted.is_empty() {
            close(db, ep.id, &untrusted, "cancelled", "untrusted_source").await?;
        }
        // Keep the request under the byte limit. An event too big to go in any request is recorded
        // as such and never sent.
        let mut take: Vec<usize> = Vec::new();
        let mut too_large: Vec<Uuid> = Vec::new();
        let mut bytes = 0;
        let mut bodies = Vec::new();
        for (i, (id, _, payload, skip)) in batch.iter().enumerate() {
            if *skip {
                continue;
            }
            let b = serde_json::to_vec(payload)?;
            if b.len() + 2 > MAX_REQUEST_BYTES {
                too_large.push(*id);
                continue;
            }
            if bytes + b.len() + 2 > MAX_REQUEST_BYTES {
                break;
            }
            bytes += b.len() + 1;
            bodies.push(b);
            take.push(i);
        }
        if !too_large.is_empty() {
            close(db, ep.id, &too_large, "failed", "too_large").await?;
        }
        if take.is_empty() {
            continue;
        }
        let ids: Vec<Uuid> = take.iter().map(|&i| batch[i].0).collect();
        let (body, topic) = if ep.batch_size <= 1 {
            (bodies.swap_remove(0), Some(batch[take[0]].1.as_str()))
        } else {
            let mut out = Vec::with_capacity(bytes + 2);
            out.push(b'[');
            for (i, b) in bodies.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(b);
            }
            out.push(b']');
            (out, None)
        };
        let outcome = send(&ep.url, &secrets, topic, &body).await;
        record(db, ep.id, &ids, &outcome).await?;
        if outcome.ok {
            sqlx::query(
                "UPDATE webhook_endpoints SET consecutive_failures = 0, failing_since = NULL, retry_at = NULL, last_success_at = now(),
                   drop_alerted = drop_alerted AND pending > $2 / 2 WHERE id = $1",
            )
            .bind(ep.id)
            .bind(super::fanout::max_pending() as i32)
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
        // Also checked every minute by `sweep`, so it happens even when nothing more is sent.
        if Utc::now() - failing_since > chrono::Duration::days(3) {
            disable(state, ep.id, ep.account_id, &ep.url, &ep.description, "It failed every delivery for 3 days, so it was turned off.").await?;
        }
        break;
    }
    Ok(())
}

/// HTTP 410 means "stop sending": Zapier and Make return it once a hook is gone. Hooks made
/// through the API are removed; dashboard endpoints are turned off.
async fn gone(state: &AppState, ep: &Endpoint) -> anyhow::Result<()> {
    if ep.source == "dashboard" {
        disable(state, ep.id, ep.account_id, &ep.url, &ep.description, "It answered 410 Gone, which means it no longer wants deliveries, so it was turned off.").await
    } else {
        sqlx::query("DELETE FROM webhook_endpoints WHERE id = $1").bind(ep.id).execute(&state.db).await?;
        super::forget(&state.db).await;
        Ok(())
    }
}

/// Turn an endpoint off and tell the account. What it still had waiting stays for 24 hours (and
/// goes out if it's turned back on in time), then fails like any undelivered event.
pub(super) async fn disable(state: &AppState, id: Uuid, account_id: Uuid, url: &str, description: &str, why: &str) -> anyhow::Result<()> {
    let changed: Option<Uuid> = sqlx::query_scalar("UPDATE webhook_endpoints SET enabled = false, disabled_reason = $2, updated_at = now() WHERE id = $1 AND enabled RETURNING id")
        .bind(id)
        .bind(why)
        .fetch_optional(&state.db)
        .await?;
    if changed.is_none() {
        return Ok(());
    }
    super::forget(&state.db).await;
    let text = format!(
        "Your webhook endpoint {} was turned off. {why}\n\nNothing more is sent to it until you turn it back on in Settings → Webhooks. After you fix it, you can resend the failed deliveries from its log there for up to 14 days.",
        super::display_name(description, url)
    );
    super::alert_account(state, account_id, "A webhook endpoint was turned off", &text).await;
    Ok(())
}

/// Every minute, whatever traffic there is: fail deliveries past their 24 hours (on every
/// endpoint, on or off), turn off endpoints that have failed for 3 days, queue endpoints that have
/// deliveries waiting but no job (after a crash or a resend), and clear a stale "job queued" flag.
pub(super) async fn sweep(state: &AppState) -> anyhow::Result<()> {
    let db = &state.db;
    // REST hooks whose API key was revoked are removed with it.
    let revoked: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM webhook_endpoints e USING api_keys k WHERE e.api_key_id = k.id AND k.revoked_at IS NOT NULL RETURNING e.account_id",
    )
    .fetch_all(db)
    .await?;
    if !revoked.is_empty() {
        super::forget(db).await;
    }
    for _ in 0..10 {
        let expired: Vec<(Uuid, i64)> = sqlx::query_as(
            "WITH x AS (UPDATE webhook_deliveries SET status = 'failed', error = 'expired'
                        WHERE id IN (SELECT id FROM webhook_deliveries WHERE status = 'pending' AND expires_at < now() LIMIT 5000) RETURNING endpoint_id)
             SELECT endpoint_id, count(*) FROM x GROUP BY endpoint_id",
        )
        .fetch_all(db)
        .await?;
        let total: i64 = expired.iter().map(|e| e.1).sum();
        for (ep, n) in expired {
            left_pending(db, ep, n).await?;
        }
        if total < 5000 {
            break;
        }
    }
    let failing: Vec<(Uuid, Uuid, String, String)> =
        sqlx::query_as("SELECT id, account_id, url, description FROM webhook_endpoints WHERE enabled AND failing_since < now() - interval '3 days' LIMIT 100")
            .fetch_all(db)
            .await?;
    for (id, account_id, url, description) in failing {
        disable(state, id, account_id, &url, &description, "It failed every delivery for 3 days, so it was turned off.").await?;
    }
    sqlx::query(
        "UPDATE webhook_endpoints e SET wake_queued = false WHERE e.wake_queued
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'webhooks_deliver' AND j.payload->>'endpoint_id' = e.id::text AND j.run_at <> 'infinity')",
    )
    .execute(db)
    .await?;
    let idle: Vec<Uuid> = sqlx::query_scalar(
        "SELECT e.id FROM webhook_endpoints e WHERE e.enabled AND NOT e.wake_queued AND (e.lease_until IS NULL OR e.lease_until < now()) AND e.pending > 0
         LIMIT 1000",
    )
    .fetch_all(db)
    .await?;
    for id in idle {
        reschedule(db, id).await?;
    }
    Ok(())
}

/// Delivery logs (and their payloads) are kept for 14 days, whatever their status.
pub(super) async fn prune(db: &PgPool) -> anyhow::Result<()> {
    for _ in 0..20 {
        let n = sqlx::query("DELETE FROM webhook_deliveries WHERE id IN (SELECT id FROM webhook_deliveries WHERE created_at < now() - interval '14 days' LIMIT 10000)")
            .execute(db)
            .await?
            .rows_affected();
        if n < 10000 {
            break;
        }
    }
    Ok(())
}

/// Hourly: set each endpoint's undelivered count from the rows themselves, in case it drifted.
pub(super) async fn recount(db: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE webhook_endpoints e SET pending = c.n
         FROM (SELECT e2.id, (SELECT count(*) FROM webhook_deliveries d WHERE d.endpoint_id = e2.id AND d.status = 'pending')::int AS n FROM webhook_endpoints e2) c
         WHERE c.id = e.id AND e.pending <> c.n",
    )
    .execute(db)
    .await?;
    Ok(())
}
