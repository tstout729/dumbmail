//! Recording what Shopify's servers report as events.
//!
//! The browser snippet and checkout pixel send `Placed Order` with `unique_id` `order:<id>` and
//! `Started Checkout` with `checkout:<token>`; this module uses the same ids, so one order is one
//! event whichever arrives first. When the browser's copy is already there, it is replaced by a fresh
//! trusted row (nothing of the browser's is kept); when the server's copy is there first, the
//! browser's is dropped as a duplicate. Either way revenue is counted once.
//!
//! New activity (within a day) goes through `audience::record_event_from(.., Integration)`, so flows
//! start. Anything older, and everything a history import brings in, goes through
//! `audience::record_history`: no flows start, nothing is sent or billed, and the profile's last
//! activity moves only as far as the event's own time.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::audience::{self, EventSource, HistoryEvent, NewEvent};

/// Events older than this when they reach us are history, not something happening now.
pub const LIVE_WINDOW_HOURS: i64 = 24;

pub struct ServerEvent<'a> {
    pub profile_id: Uuid,
    pub metric: &'a str,
    pub unique_id: String,
    pub properties: Value,
    pub value: Option<f64>,
    pub at: DateTime<Utc>,
}

/// Metric ids looked up once per worker run.
#[derive(Default)]
pub struct Metrics(HashMap<String, Uuid>);

impl Metrics {
    pub async fn id(&mut self, db: &PgPool, account_id: Uuid, name: &str) -> anyhow::Result<Uuid> {
        if let Some(id) = self.0.get(name) {
            return Ok(*id);
        }
        let id = audience::metric_id(db, account_id, name).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        self.0.insert(name.to_string(), id);
        Ok(id)
    }
}

/// How an existing copy of the event is brought up to date: given its properties, the new
/// properties and value. `None` uses the new event's as they are.
pub type Merge<'a> = Option<&'a (dyn Fn(&Value) -> (Value, Option<f64>) + Send + Sync)>;

pub enum Put {
    Created,
    Updated,
    Unchanged,
}

/// Save one server event (see the module docs). `quiet` forces history (imports).
///
/// A copy only a browser recorded (same `unique_id`) is never updated or merged from: anything it
/// carries could be forged. The trusted event is recorded as a fresh row through the shared paths
/// (`audience::record_event_from(.., Integration)`, which replaces the browser's copy in the same
/// transaction, or `audience::record_history` for the past), so every consumer (flows, catalog
/// learning, coupon redemption, webhooks) sees it as new.
pub async fn put(db: &PgPool, metrics: &mut Metrics, account_id: Uuid, ev: ServerEvent<'_>, quiet: bool, merge: Merge<'_>) -> anyhow::Result<Put> {
    let metric_id = metrics.id(db, account_id, ev.metric).await?;
    for _ in 0..2 {
        let existing: Option<(Uuid, Value, Option<f64>, Uuid)> = sqlx::query_as(
            "SELECT id, properties, value, profile_id FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id = $3 AND from_browser = false",
        )
        .bind(account_id)
        .bind(metric_id)
        .bind(&ev.unique_id)
        .fetch_optional(db)
        .await?;
        if let Some((id, old_props, old_value, old_profile)) = existing {
            let (props, value) = match merge {
                Some(f) => f(&old_props),
                None => (ev.properties.clone(), ev.value),
            };
            if props == old_props && value == old_value && old_profile == ev.profile_id {
                return Ok(Put::Unchanged);
            }
            sqlx::query("UPDATE events SET properties = $2, value = $3, profile_id = $4 WHERE id = $1 AND from_browser = false")
                .bind(id)
                .bind(&props)
                .bind(value)
                .bind(ev.profile_id)
                .execute(db)
                .await?;
            return Ok(Put::Updated);
        }
        let live = !quiet && ev.at > Utc::now() - Duration::hours(LIVE_WINDOW_HOURS);
        let inserted = if live {
            audience::record_event_from(
                db,
                NewEvent {
                    account_id,
                    profile_id: ev.profile_id,
                    metric: ev.metric,
                    properties: ev.properties.clone(),
                    value: ev.value,
                    occurred_at: Some(ev.at),
                    unique_id: Some(ev.unique_id.clone()),
                    message_id: None,
                },
                EventSource::Integration,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.public_message()))?
            .is_some()
        } else {
            // History: no flows start, nothing is sent or billed. A browser's copy goes in the same
            // transaction, so the trusted one is a fresh row.
            let mut tx = db.begin().await?;
            sqlx::query("DELETE FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id = $3 AND from_browser")
                .bind(account_id)
                .bind(metric_id)
                .bind(&ev.unique_id)
                .execute(&mut *tx)
                .await?;
            let n = audience::record_history(
                &mut *tx,
                account_id,
                &[HistoryEvent { profile_id: ev.profile_id, metric_id, properties: ev.properties.clone(), value: ev.value, occurred_at: ev.at, unique_id: ev.unique_id.clone() }],
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.public_message()))?;
            tx.commit().await?;
            n > 0
        };
        if inserted {
            return Ok(Put::Created);
        }
        // Lost a race with another writer of the same event: update theirs.
    }
    Ok(Put::Unchanged)
}

/// Record new server events that belong together (an order and its lines) all at once or not at
/// all: through `audience::record_events_from` (live) or one transaction of `record_history`
/// (history), replacing browser copies as `put` does. `false`: one of them was already there as a
/// server event (another writer got there first); use `put` for each instead.
pub async fn create_together(db: &PgPool, metrics: &mut Metrics, account_id: Uuid, evs: Vec<ServerEvent<'_>>, quiet: bool) -> anyhow::Result<bool> {
    let Some(first) = evs.first() else { return Ok(true) };
    let live = !quiet && first.at > Utc::now() - Duration::hours(LIVE_WINDOW_HOURS);
    let mut ids = Vec::with_capacity(evs.len());
    for ev in &evs {
        ids.push(metrics.id(db, account_id, ev.metric).await?);
    }
    let mut tx = db.begin().await?;
    let uniques: Vec<String> = evs.iter().map(|e| e.unique_id.clone()).collect();
    // Lock-free check inside the transaction; the inserts below are ON CONFLICT DO NOTHING anyway.
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM events WHERE account_id = $1 AND metric_id = ANY($2) AND unique_id = ANY($3) AND from_browser = false)")
        .bind(account_id)
        .bind(&ids)
        .bind(&uniques)
        .fetch_one(&mut *tx)
        .await?;
    if taken {
        return Ok(false);
    }
    if live {
        drop(tx);
        let news: Vec<NewEvent<'_>> = evs
            .iter()
            .map(|ev| NewEvent { account_id, profile_id: ev.profile_id, metric: ev.metric, properties: ev.properties.clone(), value: ev.value, occurred_at: Some(ev.at), unique_id: Some(ev.unique_id.clone()), message_id: None })
            .collect();
        let made = audience::record_events_from(db, news, EventSource::Integration).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        return Ok(made.iter().all(|m| m.is_some()));
    }
    for (ev, mid) in evs.iter().zip(&ids) {
        sqlx::query("DELETE FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id = $3 AND from_browser").bind(account_id).bind(mid).bind(&ev.unique_id).execute(&mut *tx).await?;
    }
    let history: Vec<HistoryEvent> = evs
        .iter()
        .zip(&ids)
        .map(|(ev, mid)| HistoryEvent { profile_id: ev.profile_id, metric_id: *mid, properties: ev.properties.clone(), value: ev.value, occurred_at: ev.at, unique_id: ev.unique_id.clone() })
        .collect();
    let n = audience::record_history(&mut *tx, account_id, &history).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    tx.commit().await?;
    Ok(n as usize == history.len())
}

// ---------- History and segment-triggered flows ----------
//
// Imported orders and customers can move people into segments; those entries mustn't start
// segment-triggered flows. Each person a history import writes about is recorded first
// (`shopify_history_touched`, committed before their history is), and `flows::refresh_segment`
// leaves them out of the people it starts flows for, checked after the membership refresh: so it
// holds however the refresh and the import interleave. Nobody else is affected: a real entry while
// an import runs, is paused, or just finished starts flows as usual (no window, no re-baseline).
// An account that never ran a Shopify history import runs exactly the statements it always did.

/// Record that a history import is about to write about these people.
pub async fn touch_history(db: &PgPool, account_id: Uuid, profile_ids: &[Uuid]) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO shopify_history_touched (account_id, profile_id) SELECT $1, p FROM unnest($2::uuid[]) AS p
         ON CONFLICT (account_id, profile_id) DO UPDATE SET touched_at = now()",
    )
    .bind(account_id)
    .bind(profile_ids)
    .execute(db)
    .await?;
    Ok(())
}

/// `entered` without the people history moved in (for `flows::refresh_segment`).
pub async fn without_history(db: &PgPool, account_id: Uuid, entered: Vec<Uuid>) -> sqlx::Result<Vec<Uuid>> {
    let touched: Vec<Uuid> = sqlx::query_scalar("SELECT profile_id FROM shopify_history_touched WHERE account_id = $1 AND profile_id = ANY($2)")
        .bind(account_id)
        .bind(&entered)
        .fetch_all(db)
        .await?;
    Ok(entered.into_iter().filter(|p| !touched.contains(p)).collect())
}

/// Forget history marks every segment of the account has been refreshed past (with an hour's
/// margin for a refresh that was running when the history was written).
pub async fn prune_history(db: &PgPool) -> sqlx::Result<()> {
    sqlx::query(
        "DELETE FROM shopify_history_touched t
         WHERE t.touched_at < COALESCE((SELECT min(s.refreshed_at) FROM segments s WHERE s.account_id = t.account_id), now()) - interval '1 hour'",
    )
    .execute(db)
    .await?;
    Ok(())
}
