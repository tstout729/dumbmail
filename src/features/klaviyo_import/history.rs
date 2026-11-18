//! The window while event history is being written. Imported orders and views can move people into
//! segments; those entries mustn't start segment-triggered flows. So while a run is writing history
//! for an account, the account's segments are marked `absorb_entries` and a refresh takes new members
//! in quietly (`flows::refresh_segment`). The window opens with each page of history written and closes
//! as soon as the run pauses, finishes, fails or is cancelled; then each marked segment is refreshed once
//! more (still quietly: its new baseline includes everyone history moved in) and unmarked.
//!
//! An account with no history being imported pays nothing: the flag is a column the refresh already
//! reads.

use serde_json::json;
use uuid::Uuid;

use crate::state::AppState;

pub const JOB_REBASELINE: &str = "klaviyo_rebaseline_segments";
/// Segments refreshed by one run of the re-baseline job.
const BATCH: i64 = 25;

/// Open the window (or keep it open), in the transaction that writes a page of history. Also marks
/// segments made since the last page.
pub async fn open(tx: &mut sqlx::PgConnection, import_id: Uuid, account_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "WITH i AS (UPDATE klaviyo_imports SET history_open = true WHERE id = $1 AND NOT history_open RETURNING 1)
         UPDATE segments SET absorb_entries = true WHERE account_id = $2 AND NOT absorb_entries",
    )
    .bind(import_id)
    .bind(account_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Close the window, in the transaction that pauses, finishes, fails or cancels the run, and queue the
/// re-baseline in the same transaction.
pub async fn close(tx: &mut sqlx::PgConnection, import_id: Uuid) -> sqlx::Result<()> {
    let account: Option<Uuid> = sqlx::query_scalar("UPDATE klaviyo_imports SET history_open = false WHERE id = $1 AND history_open RETURNING account_id")
        .bind(import_id)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(a) = account {
        crate::jobs::enqueue_background(&mut *tx, JOB_REBASELINE, json!({"account_id": a}), None).await?;
    }
    Ok(())
}

/// Refresh the account's marked segments once more, quietly, then unmark them, a batch at a time. If
/// another run opened the window again meanwhile, its own close does this instead.
pub async fn rebaseline(state: &AppState, account_id: Uuid) -> anyhow::Result<()> {
    let open = |db: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM klaviyo_imports WHERE account_id = $1 AND history_open)").bind(account_id).fetch_one(&db).await
    };
    if open(state.db.clone()).await? {
        return Ok(());
    }
    let segs: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM segments WHERE account_id = $1 AND absorb_entries ORDER BY id LIMIT $2").bind(account_id).bind(BATCH).fetch_all(&state.db).await?;
    for sid in &segs {
        crate::flows::refresh_segment(state, *sid).await?;
        sqlx::query("UPDATE segments SET absorb_entries = false WHERE id = $1 AND NOT EXISTS (SELECT 1 FROM klaviyo_imports WHERE account_id = $2 AND history_open)")
            .bind(sid)
            .bind(account_id)
            .execute(&state.db)
            .await?;
    }
    if segs.len() as i64 == BATCH {
        crate::jobs::enqueue_background(&state.db, JOB_REBASELINE, json!({"account_id": account_id}), None).await?;
    }
    Ok(())
}
