//! Export runs, in the background job lane.
//!
//! A run is a list of datasets, each with a window and a keyset cursor. It copies the
//! destination's settings when it starts and uses only that copy, so editing the destination can't
//! split a run across two buckets or formats (an edit to where files go cancels the run instead).
//! Each `warehouse_run` job claims the run (one worker at a time), writes one gzip file and saves
//! the cursor, so a run resumes where it stopped and a retried file is rewritten under the same
//! name. A file's rows are read in one read-only REPEATABLE READ transaction (so the file is
//! consistent in itself), compressed in memory up to [`FILE_BYTES`], and uploaded after that
//! transaction ends (no database connection is held while waiting on the network). A file counts
//! only once the storage service confirms it completely; anything else aborts it and the job tries
//! again. When every dataset is done the run writes its manifest, then moves the watermarks
//! forward and marks itself done in one transaction.
//!
//! Incremental windows start [`OVERLAP_MINUTES`] before the last watermark, so a row committed
//! late isn't skipped; rows carry their key and `_updated_at`, and warehouses dedupe on the key.

use std::io::Write;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::datasets::{self, Dataset};
use super::s3::Bucket;
use super::Dest;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

const BATCH_ROWS: i64 = 5_000;
const FILE_ROWS: usize = 250_000;
/// Compressed bytes held in memory for one file before it's uploaded.
const FILE_BYTES: usize = 64 * 1024 * 1024;
const FILE_TIME: Duration = Duration::from_secs(90);
const PART_BYTES: usize = 8 * 1024 * 1024;
const MAX_ATTEMPTS: i32 = 5;
/// Each incremental run re-reads this far behind its last watermark.
pub(super) const OVERLAP_MINUTES: i64 = 15;
/// The change log is kept this long; a destination that hasn't exported for longer starts over.
pub(super) const CHANGE_DAYS: i64 = 7;
/// A new or moved destination's first run waits this long, so every server has noticed it and is
/// writing the change log before the baseline is read (see `super::changed`).
pub(super) const BASELINE_DELAY_SECS: i64 = 60;

#[derive(Serialize, Deserialize, Clone)]
pub(super) struct FileInfo {
    pub key: String,
    pub rows: usize,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub(super) struct DatasetProgress {
    pub name: String,
    pub incremental: bool,
    pub from: Option<DateTime<Utc>>,
    pub to: DateTime<Utc>,
    #[serde(default)]
    pub pass: usize,
    pub cursor: Option<(String, String)>,
    pub files: Vec<FileInfo>,
    pub rows: i64,
    pub done: bool,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Progress {
    pub date: String,
    pub datasets: Vec<DatasetProgress>,
}

/// The destination as a run saw it when it started.
#[derive(Serialize, Deserialize, Clone)]
pub(super) struct Settings {
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub path_style: bool,
    pub access_key_enc: String,
    pub secret_key_enc: String,
    pub format: String,
    pub name: String,
}

impl Settings {
    fn of(d: &Dest) -> Self {
        Settings {
            endpoint: d.endpoint.clone(),
            region: d.region.clone(),
            bucket: d.bucket.clone(),
            prefix: d.prefix.clone(),
            path_style: d.path_style,
            access_key_enc: d.access_key_enc.clone(),
            secret_key_enc: d.secret_key_enc.clone(),
            format: d.format.clone(),
            name: d.name.clone(),
        }
    }

    fn bucket(&self, state: &AppState) -> anyhow::Result<Bucket> {
        Ok(Bucket {
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            bucket: self.bucket.clone(),
            path_style: self.path_style,
            access_key: crate::crypto::decrypt(&state.cfg.secret_key, &self.access_key_enc)?,
            secret_key: crate::crypto::decrypt(&state.cfg.secret_key, &self.secret_key_enc)?,
        })
    }
}

pub(super) fn bucket(state: &AppState, d: &Dest) -> anyhow::Result<Bucket> {
    Settings::of(d).bucket(state)
}

pub(super) fn key_prefix(prefix: &str) -> String {
    let p = prefix.trim().trim_matches('/');
    if p.is_empty() { String::new() } else { format!("{p}/") }
}

async fn account_tz(state: &AppState, account_id: Uuid) -> chrono_tz::Tz {
    let tz: Option<String> = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_optional(&state.db).await.ok().flatten();
    tz.and_then(|t| t.parse().ok()).unwrap_or(chrono_tz::America::Los_Angeles)
}

/// The next scheduled run after `now`: the top of the next hour, or 1am tomorrow in the account's time zone.
pub(super) fn next_slot(schedule: &str, tz: chrono_tz::Tz, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    use chrono::{Duration as D, TimeZone, Timelike};
    match schedule {
        "hourly" => Some(now.with_minute(0)?.with_second(0)?.with_nanosecond(0)? + D::hours(1)),
        "daily" => {
            let local = now.with_timezone(&tz);
            let mut day = local.date_naive();
            for _ in 0..3 {
                let at = tz.from_local_datetime(&day.and_hms_opt(1, 0, 0)?).earliest().map(|t| t.with_timezone(&Utc));
                if let Some(at) = at.filter(|t| *t > now) {
                    return Some(at);
                }
                day = day.succ_opt()?;
            }
            None
        }
        _ => None,
    }
}

/// Where an incremental dataset's window starts: an overlap behind its watermark, or the beginning
/// (a baseline) when there's no watermark or the change log no longer reaches back to it.
pub(super) fn window_start(ds: &Dataset, mark: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mark = mark?;
    if ds.changes_sql.is_some() && mark < now - chrono::Duration::days(CHANGE_DAYS) + chrono::Duration::hours(1) {
        return None;
    }
    Some(mark - chrono::Duration::minutes(OVERLAP_MINUTES))
}

/// Start a run (`incremental` or `full`). Only one run per destination at a time.
pub(super) async fn start(state: &AppState, d: &Dest, kind: &str) -> AppResult<Uuid> {
    let now = Utc::now();
    let marks: Vec<(String, DateTime<Utc>)> = sqlx::query_as("SELECT dataset, exported_until FROM warehouse_watermarks WHERE destination_id = $1")
        .bind(d.id)
        .fetch_all(&state.db)
        .await?;
    let tz = account_tz(state, d.account_id).await;
    let progress = Progress {
        date: now.with_timezone(&tz).format("%Y-%m-%d").to_string(),
        datasets: datasets::DATASETS
            .iter()
            .filter(|ds| d.datasets.iter().any(|n| n == ds.name))
            .map(|ds| DatasetProgress {
                name: ds.name.to_string(),
                incremental: ds.incremental,
                from: if ds.incremental && kind == "incremental" { window_start(ds, marks.iter().find(|(n, _)| n == ds.name).map(|(_, t)| *t), now) } else { None },
                to: now,
                pass: 0,
                cursor: None,
                files: Vec::new(),
                rows: 0,
                done: false,
            })
            .collect(),
    };
    let id = Uuid::now_v7();
    let res = sqlx::query("INSERT INTO warehouse_runs (id, account_id, destination_id, revision, settings, kind, progress, window_end) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(id)
        .bind(d.account_id)
        .bind(d.id)
        .bind(d.revision)
        .bind(serde_json::to_value(Settings::of(d)).map_err(|e| AppError::Internal(e.into()))?)
        .bind(kind)
        .bind(serde_json::to_value(&progress).map_err(|e| AppError::Internal(e.into()))?)
        .bind(now)
        .execute(&state.db)
        .await;
    match res {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
            return Err(AppError::Conflict("an export to this destination is already running; wait for it to finish".into()))
        }
        Err(e) => return Err(e.into()),
    }
    // A new or moved destination waits until every server is writing its change log.
    let at = Some(d.baseline_after).filter(|t| *t > now);
    crate::jobs::enqueue_background(&state.db, "warehouse_run", json!({"run_id": id}), at).await?;
    Ok(id)
}

#[derive(sqlx::FromRow)]
struct RunRow {
    id: Uuid,
    account_id: Uuid,
    destination_id: Uuid,
    revision: i32,
    settings: Value,
    kind: String,
    progress: Value,
    window_end: DateTime<Utc>,
    attempts: i32,
    created_at: DateTime<Utc>,
}

/// Job: claim the run and do its next piece.
pub(super) async fn step(state: &AppState, run_id: Uuid) -> anyhow::Result<()> {
    // The claim: one worker per run, however many jobs or sweeps point at it.
    let run: Option<RunRow> = sqlx::query_as(
        "UPDATE warehouse_runs SET lease_until = now() + interval '10 minutes', status = 'running', started_at = COALESCE(started_at, now())
         WHERE id = $1 AND status IN ('queued', 'running') AND (lease_until IS NULL OR lease_until < now())
         RETURNING id, account_id, destination_id, revision, settings, kind, progress, window_end, attempts, created_at",
    )
    .bind(run_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(run) = run else { return Ok(()) };
    let res = step_claimed(state, &run).await;
    sqlx::query("UPDATE warehouse_runs SET lease_until = NULL WHERE id = $1").bind(run.id).execute(&state.db).await?;
    res
}

async fn step_claimed(state: &AppState, run: &RunRow) -> anyhow::Result<()> {
    let current: Option<i32> = sqlx::query_scalar("SELECT revision FROM warehouse_destinations WHERE id = $1").bind(run.destination_id).fetch_optional(&state.db).await?;
    if current != Some(run.revision) {
        cancel(&state.db, run.id, "destination_changed").await?;
        return Ok(());
    }
    let settings: Settings = serde_json::from_value(run.settings.clone())?;
    match advance(state, run, &settings).await {
        Ok(true) => {
            crate::jobs::enqueue_background(&state.db, "warehouse_run", json!({"run_id": run.id}), None).await?;
        }
        Ok(false) => {}
        Err(e) => {
            // Errors are built from fixed phrases, status codes and allowlisted codes (see s3.rs):
            // never a response body or a signed URL.
            let msg = format!("{e:#}").chars().take(300).collect::<String>();
            let attempts = run.attempts + 1;
            if attempts >= MAX_ATTEMPTS {
                sqlx::query("UPDATE warehouse_runs SET status = 'failed', attempts = $2, error = $3, finished_at = now() WHERE id = $1 AND status = 'running'")
                    .bind(run.id)
                    .bind(attempts)
                    .bind(&msg)
                    .execute(&state.db)
                    .await?;
                let text = format!(
                    "The {} export to \"{}\" (bucket {}) failed after {attempts} tries: {msg}\n\nCheck the destination in Settings → Data export. The next scheduled export starts from where the last good one ended, so nothing is skipped once it works again.",
                    run.kind,
                    settings.name.chars().take(100).collect::<String>(),
                    settings.bucket
                );
                crate::features::webhooks::alert_account(state, run.account_id, "A data export failed", &text).await;
            } else {
                sqlx::query("UPDATE warehouse_runs SET attempts = $2, error = $3 WHERE id = $1").bind(run.id).bind(attempts).bind(&msg).execute(&state.db).await?;
                let wait = chrono::Duration::seconds(60 * 2i64.pow(attempts as u32));
                crate::jobs::enqueue_background(&state.db, "warehouse_run", json!({"run_id": run.id}), Some(Utc::now() + wait)).await?;
            }
        }
    }
    Ok(())
}

/// Stop a run that hasn't finished, saying why.
pub(super) async fn cancel<'c>(db: impl sqlx::PgExecutor<'c>, run_id: Uuid, why: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE warehouse_runs SET status = 'cancelled', error = $2, finished_at = now() WHERE id = $1 AND status IN ('queued', 'running')")
        .bind(run_id)
        .bind(why)
        .execute(db)
        .await?;
    Ok(())
}

/// Write one file, or finish the run. Returns whether there's more to do.
async fn advance(state: &AppState, run: &RunRow, settings: &Settings) -> anyhow::Result<bool> {
    let mut progress: Progress = serde_json::from_value(run.progress.clone())?;
    let bucket = settings.bucket(state)?;
    let Some(i) = progress.datasets.iter().position(|d| !d.done) else {
        finish(state, run, settings, &bucket, &progress).await?;
        return Ok(false);
    };
    let ds = datasets::find(&progress.datasets[i].name).ok_or_else(|| anyhow::anyhow!("unknown dataset {}", progress.datasets[i].name))?;
    let prefix = key_prefix(&settings.prefix);
    let file = write_file(state, &bucket, run, &prefix, &progress.date, &settings.format, ds, &mut progress.datasets[i]).await?;
    let (rows, bytes) = file.as_ref().map(|f| (f.rows as i64, f.bytes as i64)).unwrap_or((0, 0));
    sqlx::query("UPDATE warehouse_runs SET progress = $2, rows = rows + $3, files = files + $4, bytes = bytes + $5, attempts = 0, error = NULL WHERE id = $1 AND status = 'running'")
        .bind(run.id)
        .bind(serde_json::to_value(&progress)?)
        .bind(rows)
        .bind(file.is_some() as i32)
        .bind(bytes)
        .execute(&state.db)
        .await?;
    Ok(true)
}

/// Upload a finished file: one PUT, or a multipart upload in [`PART_BYTES`] parts for a big one.
/// Any failure (an error, a timeout, an answer that doesn't confirm it) aborts the upload.
async fn upload(bucket: &Bucket, key: &str, data: Vec<u8>) -> anyhow::Result<()> {
    if data.len() <= PART_BYTES {
        return bucket.put(key, data, "application/gzip").await;
    }
    let id = bucket.create_multipart(key, "application/gzip").await?;
    let res: anyhow::Result<()> = async {
        let mut etags = Vec::new();
        for (n, chunk) in data.chunks(PART_BYTES).enumerate() {
            etags.push(bucket.upload_part(key, &id, n + 1, chunk.to_vec()).await?);
        }
        bucket.complete_multipart(key, &id, &etags).await
    }
    .await;
    if res.is_err() {
        bucket.abort_multipart(key, &id).await;
    }
    res
}

fn csv_cell(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

fn csv_line(fields: &[String]) -> anyhow::Result<Vec<u8>> {
    let mut w = csv::WriterBuilder::new().terminator(csv::Terminator::Any(b'\n')).from_writer(Vec::new());
    w.write_record(fields)?;
    w.into_inner().map_err(|e| anyhow::anyhow!("csv: {e}"))
}

/// Read the next file's rows in one consistent snapshot and compress them. Returns the gzip
/// bytes, the row count, the cursor after the last row, and whether the pass is done.
#[allow(clippy::type_complexity)]
async fn read_file(state: &AppState, run: &RunRow, csv: bool, ds: &Dataset, sql: &str, p: &DatasetProgress) -> anyhow::Result<(Vec<u8>, usize, Option<(String, String)>, bool)> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    let columns: Vec<&str> = ds.columns.iter().copied().chain(["_updated_at"]).collect();
    if csv {
        gz.write_all(&csv_line(&columns.iter().map(|c| c.to_string()).collect::<Vec<_>>())?)?;
    }
    let mut tx = state.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;
    // The snapshot's time: every row in this file is as of then.
    let as_of: DateTime<Utc> = sqlx::query_scalar("SELECT now()").fetch_one(&mut *tx).await?;
    let started = Instant::now();
    let mut cursor = p.cursor.clone();
    let mut rows = 0usize;
    let done = loop {
        let batch: Vec<(Value, String, String)> = sqlx::query_as(sql)
            .bind(run.account_id)
            .bind(p.from)
            .bind(p.to)
            .bind(cursor.as_ref().map(|c| c.0.clone()))
            .bind(cursor.as_ref().map(|c| c.1.clone()).filter(|s| !s.is_empty()))
            .bind(BATCH_ROWS)
            .bind(ds.name)
            .fetch_all(&mut *tx)
            .await?;
        let n = batch.len();
        for (mut row, a, b) in batch {
            row["_updated_at"] = json!(as_of);
            if csv {
                gz.write_all(&csv_line(&columns.iter().map(|c| csv_cell(row.get(*c))).collect::<Vec<_>>())?)?;
            } else {
                serde_json::to_writer(&mut gz, &row)?;
                gz.write_all(b"\n")?;
            }
            cursor = Some((a, b));
        }
        rows += n;
        if (n as i64) < BATCH_ROWS {
            break true;
        }
        if rows >= FILE_ROWS || gz.get_ref().len() >= FILE_BYTES || started.elapsed() > FILE_TIME {
            break false;
        }
    };
    tx.commit().await?;
    Ok((gz.finish()?, rows, cursor, done))
}

/// Write the next file of a dataset. None when there was nothing (more) to write.
#[allow(clippy::too_many_arguments)]
async fn write_file(
    state: &AppState,
    bucket: &Bucket,
    run: &RunRow,
    prefix: &str,
    date: &str,
    format: &str,
    ds: &Dataset,
    p: &mut DatasetProgress,
) -> anyhow::Result<Option<FileInfo>> {
    let csv = format == "csv";
    let passes = ds.passes(p.from.is_none());
    let Some(sql) = passes.get(p.pass).copied() else {
        p.done = true;
        return Ok(None);
    };
    let (data, rows, cursor, pass_done) = read_file(state, run, csv, ds, sql, p).await?;
    let next_pass = |p: &mut DatasetProgress| {
        p.pass += 1;
        p.cursor = None;
        p.done = p.pass >= passes.len();
    };
    if rows == 0 {
        next_pass(p);
        return Ok(None);
    }
    let key = format!("{prefix}{}/dt={date}/run={}/part-{:05}.{}.gz", ds.name, run.id, p.files.len(), if csv { "csv" } else { "jsonl" });
    let bytes = data.len() as u64;
    upload(bucket, &key, data).await?;
    let info = FileInfo { key, rows, bytes };
    p.files.push(info.clone());
    p.rows += rows as i64;
    if pass_done {
        next_pass(p);
    } else {
        p.cursor = cursor;
    }
    Ok(Some(info))
}

/// Write the manifest, then move watermarks forward and mark the run done in one transaction.
async fn finish(state: &AppState, run: &RunRow, settings: &Settings, bucket: &Bucket, p: &Progress) -> anyhow::Result<()> {
    let prefix = key_prefix(&settings.prefix);
    let manifest_key = format!("{prefix}_manifests/dt={}/run={}.json", p.date, run.id);
    let manifest = json!({
        "run_id": run.id, "account_id": run.account_id, "destination_id": run.destination_id, "kind": run.kind,
        "format": if settings.format == "csv" { "csv" } else { "jsonl" }, "compression": "gzip", "schema_version": 2,
        "started_at": run.created_at, "finished_at": Utc::now(), "window_end": run.window_end, "date": p.date,
        "datasets": p.datasets.iter().map(|d| {
            let ds = datasets::find(&d.name);
            json!({
                "name": d.name, "mode": if d.incremental { "incremental" } else { "snapshot" },
                "key": ds.map(|x| x.key).unwrap_or(&[]),
                "window": if d.incremental { json!({"after": d.from, "through": d.to}) } else { Value::Null },
                "rows": d.rows, "files": d.files,
            })
        }).collect::<Vec<_>>(),
    });
    bucket.put(&manifest_key, serde_json::to_vec_pretty(&manifest)?, "application/json").await?;
    let mut tx = state.db.begin().await?;
    let done = sqlx::query("UPDATE warehouse_runs SET status = 'succeeded', finished_at = now(), manifest_key = $2, error = NULL WHERE id = $1 AND status = 'running'")
        .bind(run.id)
        .bind(&manifest_key)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if done == 0 {
        // Cancelled meanwhile (the destination changed): its watermarks don't move.
        return Ok(());
    }
    for d in p.datasets.iter().filter(|d| d.incremental) {
        sqlx::query(
            "INSERT INTO warehouse_watermarks (destination_id, dataset, exported_until) SELECT $1, $2, $3
             WHERE EXISTS (SELECT 1 FROM warehouse_destinations WHERE id = $1 AND revision = $4)
             ON CONFLICT (destination_id, dataset) DO UPDATE SET exported_until = GREATEST(warehouse_watermarks.exported_until, EXCLUDED.exported_until)",
        )
        .bind(run.destination_id)
        .bind(&d.name)
        .bind(d.to)
        .bind(run.revision)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE warehouse_destinations SET last_run_at = now() WHERE id = $1").bind(run.destination_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Every minute: start due scheduled runs, and requeue runs whose job went missing.
pub(super) async fn tick(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM warehouse_destinations WHERE enabled AND schedule <> 'manual' AND next_run_at <= now() ORDER BY next_run_at LIMIT 100")
        .fetch_all(&state.db)
        .await?;
    for id in due {
        let d: Option<Dest> = sqlx::query_as(&format!("SELECT {} FROM warehouse_destinations WHERE id = $1", super::DEST_COLS)).bind(id).fetch_optional(&state.db).await?;
        let Some(d) = d else { continue };
        let tz = account_tz(state, d.account_id).await;
        // The claim: only the server that moves next_run_at on starts this slot's run.
        let claimed = sqlx::query("UPDATE warehouse_destinations SET next_run_at = $2 WHERE id = $1 AND next_run_at <= now()")
            .bind(d.id)
            .bind(next_slot(&d.schedule, tz, Utc::now()))
            .execute(&state.db)
            .await?
            .rows_affected();
        if claimed == 0 {
            continue;
        }
        match start(state, &d, "incremental").await {
            Ok(_) | Err(AppError::Conflict(_)) => {}
            Err(e) => tracing::warn!(destination = %d.id, error = %e.public_message(), "couldn't start scheduled export"),
        }
    }
    let orphans: Vec<Uuid> = sqlx::query_scalar(
        "SELECT r.id FROM warehouse_runs r WHERE r.status IN ('queued', 'running') AND r.created_at < now() - interval '2 minutes'
           AND (r.lease_until IS NULL OR r.lease_until < now())
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'warehouse_run' AND j.payload->>'run_id' = r.id::text AND j.run_at <> 'infinity')
         LIMIT 100",
    )
    .fetch_all(&state.db)
    .await?;
    for id in orphans {
        crate::jobs::enqueue_background(&state.db, "warehouse_run", json!({"run_id": id}), None).await?;
    }
    Ok(())
}

pub(super) async fn prune(state: &AppState) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM warehouse_runs WHERE created_at < now() - interval '90 days' AND status IN ('succeeded', 'failed', 'cancelled')").execute(&state.db).await?;
    for _ in 0..20 {
        let n = sqlx::query("DELETE FROM warehouse_changes WHERE (account_id, dataset, row_id) IN (SELECT account_id, dataset, row_id FROM warehouse_changes WHERE changed_at < now() - make_interval(days => $1) LIMIT 10000)")
            .bind(CHANGE_DAYS as i32)
            .execute(&state.db)
            .await?
            .rows_affected();
        if n < 10000 {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn schedules() {
        let tz: chrono_tz::Tz = "America/Los_Angeles".parse().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 17, 20, 30, 0).unwrap(); // 12:30 in Los Angeles
        assert_eq!(next_slot("hourly", tz, now), Some(Utc.with_ymd_and_hms(2026, 11, 17, 21, 0, 0).unwrap()));
        assert_eq!(next_slot("daily", tz, now), Some(Utc.with_ymd_and_hms(2026, 11, 18, 9, 0, 0).unwrap()));
        assert_eq!(next_slot("manual", tz, now), None);
        assert_eq!(key_prefix(" /dumbmail/exports/ "), "dumbmail/exports/");
        assert_eq!(key_prefix(""), "");
    }

    #[test]
    fn windows_overlap_and_restart_when_the_change_log_is_gone() {
        let now = Utc.with_ymd_and_hms(2026, 11, 17, 20, 0, 0).unwrap();
        let messages = datasets::find("messages").unwrap();
        let events = datasets::find("events").unwrap();
        let mark = now - chrono::Duration::hours(1);
        assert_eq!(window_start(messages, Some(mark), now), Some(mark - chrono::Duration::minutes(OVERLAP_MINUTES)));
        assert_eq!(window_start(messages, None, now), None);
        let old = now - chrono::Duration::days(CHANGE_DAYS);
        assert_eq!(window_start(messages, Some(old), now), None, "the change log no longer covers it: start over");
        assert!(window_start(events, Some(old), now).is_some(), "events don't use the change log");
    }

    #[test]
    fn csv_cells() {
        assert_eq!(csv_cell(Some(&json!({"a": 1}))), "{\"a\":1}");
        assert_eq!(csv_cell(None), "");
        assert_eq!(String::from_utf8(csv_line(&["a,b".into(), "c".into()]).unwrap()).unwrap(), "\"a,b\",c\n");
    }
}
