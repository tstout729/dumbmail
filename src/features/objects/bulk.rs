//! Bulk upserts: the batch is checked and stored when it arrives, then written by a background job
//! a chunk at a time. Progress is saved after each chunk; a job that dies part-way carries on from
//! the last saved chunk (writing the same values again changes nothing).

use serde_json::{json, Value};
use uuid::Uuid;

use super::store::{self, UpsertInput, Writer};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const JOB: &str = "objects_bulk";
/// Most objects in one bulk upsert.
pub const MAX_ITEMS: usize = 1000;
/// Largest stored batch.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Bulk upserts one account may have waiting or running.
pub const MAX_OPEN: i64 = 3;
/// Objects written per job run.
const CHUNK: usize = 100;
/// Errors kept per bulk upsert.
const MAX_ERRORS: usize = 50;

/// Check and store a batch, and queue its job (in one transaction). Returns its id.
pub async fn start(state: &AppState, account_id: Uuid, source: &str, items: &[UpsertInput]) -> AppResult<Value> {
    if items.is_empty() || items.len() > MAX_ITEMS {
        return Err(AppError::bad(format!("send 1 to {MAX_ITEMS} objects")));
    }
    for (i, b) in items.iter().enumerate() {
        store::check_input(b).map_err(|e| AppError::bad(format!("object {i}: {}", e.public_message())))?;
    }
    let raw = serde_json::to_value(items)?;
    if raw.to_string().len() > MAX_BYTES {
        return Err(AppError::bad("the batch is over 8 MB; send it in smaller parts"));
    }
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('object_bulk:' || $1::text, 0))").bind(account_id).execute(&mut *tx).await?;
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM object_bulk_upserts WHERE account_id = $1 AND status IN ('queued', 'running')").bind(account_id).fetch_one(&mut *tx).await?;
    if open >= MAX_OPEN {
        return Err(AppError::Busy(format!("{open} bulk upserts are still running for this account; wait for one to finish")));
    }
    sqlx::query("INSERT INTO object_bulk_upserts (id, account_id, source, items, total) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(account_id)
        .bind(source)
        .bind(&raw)
        .bind(items.len() as i32)
        .execute(&mut *tx)
        .await?;
    crate::jobs::enqueue_background(&mut *tx, JOB, json!({"bulk_id": id}), None).await?;
    tx.commit().await?;
    Ok(json!({"id": id, "status": "queued", "total": items.len()}))
}

pub async fn view(db: &sqlx::PgPool, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let row: Option<(Uuid, String, i32, i32, i32, i32, i32, i32, Value, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT id, status, total, done, created, updated, unchanged, failed, errors, created_at, finished_at FROM object_bulk_upserts WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    let (id, status, total, done, created, updated, unchanged, failed, errors, at, fin) = row.ok_or_else(|| AppError::not_found("bulk upsert"))?;
    Ok(json!({"id": id, "status": status, "total": total, "done": done, "created": created, "updated": updated, "unchanged": unchanged, "failed": failed, "errors": errors, "created_at": at, "finished_at": fin}))
}

/// Job: write the next chunk of a batch, save progress, and queue the rest.
pub async fn run(state: &AppState, bulk_id: Uuid) -> anyhow::Result<()> {
    let row: Option<(Uuid, String, Option<Value>, i32, String)> =
        sqlx::query_as("SELECT account_id, source, items, done, status FROM object_bulk_upserts WHERE id = $1").bind(bulk_id).fetch_optional(&state.db).await?;
    let Some((account_id, source, Some(items), done, status)) = row else { return Ok(()) };
    if status == "done" || status == "failed" {
        return Ok(());
    }
    let items: Vec<UpsertInput> = match serde_json::from_value(items) {
        Ok(v) => v,
        Err(_) => {
            sqlx::query("UPDATE object_bulk_upserts SET status = 'failed', items = NULL, finished_at = now() WHERE id = $1").bind(bulk_id).execute(&state.db).await?;
            return Ok(());
        }
    };
    let w = Writer::new(account_id, &source);
    let start = done.max(0) as usize;
    let end = (start + CHUNK).min(items.len());
    let (mut c, mut u, mut same, mut failed) = (0, 0, 0, 0);
    let mut errors = Vec::new();
    for (i, b) in items.iter().enumerate().take(end).skip(start) {
        match store::upsert(state, &w, b).await {
            Ok(r) => match r.result {
                "created" => c += 1,
                "updated" => u += 1,
                _ => same += 1,
            },
            Err(AppError::Internal(e)) => return Err(e),
            Err(e) => {
                failed += 1;
                if errors.len() < MAX_ERRORS {
                    errors.push(json!({"index": i, "external_id": b.external_id, "error": e.public_message()}));
                }
            }
        }
    }
    let finished = end >= items.len();
    let mut tx = state.db.begin().await?;
    // Only the run that is still at `done` saves (a duplicate job for the same chunk changes nothing).
    let n = sqlx::query(
        "UPDATE object_bulk_upserts SET done = $3, created = created + $4, updated = updated + $5, unchanged = unchanged + $6, failed = failed + $7,
            errors = CASE WHEN jsonb_array_length(errors) < 50 THEN errors || $8 ELSE errors END,
            status = CASE WHEN $9 THEN 'done' ELSE 'running' END, items = CASE WHEN $9 THEN NULL ELSE items END,
            finished_at = CASE WHEN $9 THEN now() ELSE NULL END
         WHERE id = $1 AND done = $2",
    )
    .bind(bulk_id)
    .bind(done)
    .bind(end as i32)
    .bind(c)
    .bind(u)
    .bind(same)
    .bind(failed)
    .bind(Value::Array(errors))
    .bind(finished)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n > 0 && !finished {
        crate::jobs::enqueue_background(&mut *tx, JOB, json!({"bulk_id": bulk_id}), None).await?;
    }
    tx.commit().await?;
    Ok(())
}
