//! While event history is being imported, segment-triggered flows take in new members without starting
//! runs.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Called by a segment refresh before it starts segment-triggered flows. True: take the new members in
/// without starting runs, because event history is being imported (or was, before this segment's last
/// refresh).
pub async fn absorb_segment_entries(db: &PgPool, account_id: Uuid, previous_refresh: Option<DateTime<Utc>>) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM klaviyo_imports WHERE account_id = $1 AND history_started_at IS NOT NULL
            AND (history_done_at IS NULL OR history_done_at < history_started_at OR COALESCE($2, '-infinity') < history_done_at))",
    )
    .bind(account_id)
    .bind(previous_refresh)
    .fetch_one(db)
    .await?)
}
