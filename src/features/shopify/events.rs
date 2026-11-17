//! Recording what Shopify's servers report as events.
//!
//! The browser snippet and checkout pixel send `Placed Order` with `unique_id` `order:<id>` and
//! `Started Checkout` with `checkout:<token>`; this module uses the same ids, so one order is one
//! event whichever arrives first. When the browser's copy is already there, the server's facts
//! replace it in place and it becomes trusted (`from_browser = false`); when the server's copy is
//! there first, the browser's is dropped as a duplicate. Either way revenue is counted once.
//!
//! New activity (within a day) goes through `audience::record_event`, so flows start. Anything
//! older, and everything a history import brings in, is recorded quietly: no flows start, nothing is
//! sent or billed, and the profile's last activity moves only as far as the event's own time.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::audience::{self, NewEvent};

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
pub async fn put(db: &PgPool, metrics: &mut Metrics, account_id: Uuid, ev: ServerEvent<'_>, quiet: bool, merge: Merge<'_>) -> anyhow::Result<Put> {
    let metric_id = metrics.id(db, account_id, ev.metric).await?;
    for _ in 0..2 {
        let existing: Option<(Uuid, Option<bool>, Value, Option<f64>, Uuid)> = sqlx::query_as(
            "SELECT id, from_browser, properties, value, profile_id FROM events WHERE account_id = $1 AND metric_id = $2 AND unique_id = $3",
        )
        .bind(account_id)
        .bind(metric_id)
        .bind(&ev.unique_id)
        .fetch_optional(db)
        .await?;
        if let Some((id, from_browser, old_props, old_value, old_profile)) = existing {
            let (props, value) = match merge {
                Some(f) => f(&old_props),
                None => (ev.properties.clone(), ev.value),
            };
            let browser = from_browser != Some(false);
            if !browser && props == old_props && value == old_value && old_profile == ev.profile_id {
                return Ok(Put::Unchanged);
            }
            // The browser's copy takes the order's real time, person and amounts.
            sqlx::query(
                "UPDATE events SET properties = $2, value = $3, from_browser = false, profile_id = $4,
                    occurred_at = CASE WHEN from_browser IS DISTINCT FROM false THEN $5 ELSE occurred_at END
                 WHERE id = $1",
            )
            .bind(id)
            .bind(&props)
            .bind(value)
            .bind(ev.profile_id)
            .bind(ev.at)
            .execute(db)
            .await?;
            return Ok(Put::Updated);
        }
        let live = !quiet && ev.at > Utc::now() - Duration::hours(LIVE_WINDOW_HOURS);
        let inserted = if live {
            audience::record_event(
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
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.public_message()))?
            .is_some()
        } else {
            record_history(db, account_id, ev.profile_id, metric_id, &ev.properties, ev.value, ev.at, &ev.unique_id).await?
        };
        if inserted {
            return Ok(Put::Created);
        }
        // Lost a race with another writer of the same event: update theirs.
    }
    Ok(Put::Unchanged)
}

/// Record a past event quietly: no flows, no sends, no billing; `last_event_at` moves only up to
/// the event's own time. Returns false when it was already there.
///
/// This is the smallest form of the history path another change (`audience::record_history`) adds;
/// once that is on `main`, this should call it instead.
pub async fn record_history(db: &PgPool, account_id: Uuid, profile_id: Uuid, metric_id: Uuid, properties: &Value, value: Option<f64>, at: DateTime<Utc>, unique_id: &str) -> anyhow::Result<bool> {
    let value = value.or_else(|| properties.get("$value").and_then(|v| v.as_f64()));
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO events (id, account_id, profile_id, metric_id, properties, value, occurred_at, unique_id, from_browser)
         SELECT $1, $2, p.id, $4, $5, $6, $7, $8, false FROM profiles p WHERE p.id = $3 AND p.account_id = $2
         ON CONFLICT (account_id, metric_id, unique_id) WHERE unique_id IS NOT NULL DO NOTHING
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(profile_id)
    .bind(metric_id)
    .bind(properties)
    .bind(value)
    .bind(at)
    .bind(unique_id)
    .fetch_optional(db)
    .await?;
    if inserted.is_some() {
        sqlx::query("UPDATE profiles SET last_event_at = $2 WHERE id = $1 AND (last_event_at IS NULL OR last_event_at < $2)")
            .bind(profile_id)
            .bind(at)
            .execute(db)
            .await?;
    }
    Ok(inserted.is_some())
}

/// Segment-triggered flows must not start for people who entered a segment only because imported
/// history arrived. True while a Shopify history import is running for the account, or when one
/// ended since the segment's last refresh: that refresh records who is in without starting anyone.
pub async fn absorb_segment_entries(db: &PgPool, account_id: Uuid, refreshed_at: Option<DateTime<Utc>>) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM shopify_imports WHERE account_id = $1 AND kind = 'history'
            AND (status IN ('running', 'paused') OR finished_at >= COALESCE($2, '-infinity'::timestamptz)))",
    )
    .bind(account_id)
    .bind(refreshed_at)
    .fetch_one(db)
    .await?)
}
