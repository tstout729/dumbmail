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

// ---------- The history window ----------
//
// Imported orders and customers can move people into segments; those entries mustn't start
// segment-triggered flows. The same mechanism as the Klaviyo import (`segments.absorb_entries`): the
// window opens while a page of this store's history is being written and closes as soon as the
// import pauses, finishes, fails or is cancelled; then each marked segment is refreshed once more
// (quietly: its new baseline includes everyone history moved in) and unmarked. Live webhooks
// written while no history page is being written start segment flows as usual.

pub const JOB_REBASELINE: &str = "shopify_rebaseline_segments";
const REBASELINE_BATCH: i64 = 25;

/// Open the window for this history import (or keep it open), and mark segments made since.
pub async fn open_history(db: &PgPool, import_id: Uuid, account_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "WITH i AS (UPDATE shopify_imports SET history_open = true WHERE id = $1 AND kind = 'history' AND NOT history_open RETURNING 1)
         UPDATE segments SET absorb_entries = true WHERE account_id = $2 AND NOT absorb_entries AND EXISTS (SELECT 1 FROM shopify_imports WHERE id = $1 AND kind = 'history')",
    )
    .bind(import_id)
    .bind(account_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Close the window of every import of the account that isn't running any more (paused, done,
/// failed, cancelled), in the caller's transaction, and queue the re-baseline with it.
pub async fn close_history(conn: &mut sqlx::PgConnection, account_id: Uuid) -> sqlx::Result<()> {
    let closed: Vec<Uuid> = sqlx::query_scalar("UPDATE shopify_imports SET history_open = false WHERE account_id = $1 AND history_open AND status <> 'running' RETURNING id")
        .bind(account_id)
        .fetch_all(&mut *conn)
        .await?;
    if !closed.is_empty() {
        crate::jobs::enqueue_background(&mut *conn, JOB_REBASELINE, serde_json::json!({"account_id": account_id}), None).await?;
    }
    Ok(())
}

/// [`close_history`] in its own transaction.
pub async fn close_history_now(db: &PgPool, account_id: Uuid) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    close_history(&mut tx, account_id).await?;
    tx.commit().await
}

/// Is a page of this account's Shopify history being written now? `flows::refresh_segment` asks
/// (with `absorb_entries`), so another import's re-baseline can't let entries through early.
pub async fn history_open(db: &PgPool, account_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM shopify_imports WHERE account_id = $1 AND history_open)").bind(account_id).fetch_one(db).await
}

/// Job: refresh the account's marked segments once more, quietly, then unmark them, a batch at a time.
/// If a window (this store's or a Klaviyo import's) opened again meanwhile, its own close does this.
pub async fn rebaseline(state: &crate::state::AppState, account_id: Uuid) -> anyhow::Result<()> {
    const ANY_OPEN: &str = "(EXISTS (SELECT 1 FROM shopify_imports WHERE account_id = $1 AND history_open) OR EXISTS (SELECT 1 FROM klaviyo_imports WHERE account_id = $1 AND history_open))";
    let open: bool = sqlx::query_scalar(&format!("SELECT {ANY_OPEN}")).bind(account_id).fetch_one(&state.db).await?;
    if open {
        return Ok(());
    }
    let segs: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM segments WHERE account_id = $1 AND absorb_entries ORDER BY id LIMIT $2").bind(account_id).bind(REBASELINE_BATCH).fetch_all(&state.db).await?;
    for sid in &segs {
        crate::flows::refresh_segment(state, *sid).await?;
        sqlx::query(&format!("UPDATE segments SET absorb_entries = false WHERE id = $2 AND NOT {ANY_OPEN}")).bind(account_id).bind(sid).execute(&state.db).await?;
    }
    if segs.len() as i64 == REBASELINE_BATCH {
        crate::jobs::enqueue_background(&state.db, JOB_REBASELINE, serde_json::json!({"account_id": account_id}), None).await?;
    }
    Ok(())
}
