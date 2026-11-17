//! Export runs. A run is a list of datasets, each with a window and a keyset cursor. Each
//! `warehouse_run` job writes one gzip file (up to `FILE_ROWS` rows, streamed to the bucket in 8 MB
//! parts) and saves the cursor, so a run resumes where it stopped and a retried file is rewritten
//! under the same name. When every dataset is done the run writes its manifest and moves the
//! incremental watermarks forward.

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
const FILE_TIME: Duration = Duration::from_secs(90);
const PART_BYTES: usize = 8 * 1024 * 1024;
const MAX_ATTEMPTS: i32 = 5;
/// Rows changed in the last few minutes wait for the next run, so transactions still in flight
/// when a run starts aren't skipped.
const SETTLE_MINUTES: i64 = 5;

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

pub(super) fn bucket(state: &AppState, d: &Dest) -> anyhow::Result<Bucket> {
    Ok(Bucket {
        endpoint: d.endpoint.clone(),
        region: d.region.clone(),
        bucket: d.bucket.clone(),
        path_style: d.path_style,
        access_key: crate::crypto::decrypt(&state.cfg.secret_key, &d.access_key_enc)?,
        secret_key: crate::crypto::decrypt(&state.cfg.secret_key, &d.secret_key_enc)?,
    })
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

/// Start a run (`incremental` or `full`). Only one run per destination at a time.
pub(super) async fn start(state: &AppState, d: &Dest, kind: &str) -> AppResult<Uuid> {
    let window_end = Utc::now() - chrono::Duration::minutes(SETTLE_MINUTES);
    let marks: Vec<(String, DateTime<Utc>)> = sqlx::query_as("SELECT dataset, exported_until FROM warehouse_watermarks WHERE destination_id = $1")
        .bind(d.id)
        .fetch_all(&state.db)
        .await?;
    let tz = account_tz(state, d.account_id).await;
    let progress = Progress {
        date: Utc::now().with_timezone(&tz).format("%Y-%m-%d").to_string(),
        datasets: datasets::DATASETS
            .iter()
            .filter(|ds| d.datasets.iter().any(|n| n == ds.name))
            .map(|ds| DatasetProgress {
                name: ds.name.to_string(),
                incremental: ds.incremental,
                from: if ds.incremental && kind == "incremental" { marks.iter().find(|(n, _)| n == ds.name).map(|(_, t)| *t) } else { None },
                to: window_end,
                cursor: None,
                files: Vec::new(),
                rows: 0,
                done: false,
            })
            .collect(),
    };
    let id = Uuid::now_v7();
    let res = sqlx::query("INSERT INTO warehouse_runs (id, account_id, destination_id, kind, progress, window_end) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(d.account_id)
        .bind(d.id)
        .bind(kind)
        .bind(serde_json::to_value(&progress).map_err(|e| AppError::Internal(e.into()))?)
        .bind(window_end)
        .execute(&state.db)
        .await;
    match res {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
            return Err(AppError::Conflict("an export to this destination is already running; wait for it to finish".into()))
        }
        Err(e) => return Err(e.into()),
    }
    crate::jobs::enqueue(&state.db, "warehouse_run", json!({"run_id": id}), None).await?;
    Ok(id)
}

#[derive(sqlx::FromRow)]
struct RunRow {
    id: Uuid,
    account_id: Uuid,
    destination_id: Uuid,
    kind: String,
    status: String,
    progress: Value,
    window_end: DateTime<Utc>,
    attempts: i32,
    created_at: DateTime<Utc>,
}

/// Job: do the next piece of a run.
pub(super) async fn step(state: &AppState, run_id: Uuid) -> anyhow::Result<()> {
    let run: Option<RunRow> = sqlx::query_as(
        "SELECT id, account_id, destination_id, kind, status, progress, window_end, attempts, created_at FROM warehouse_runs WHERE id = $1 AND status IN ('queued', 'running')",
    )
    .bind(run_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(run) = run else { return Ok(()) };
    let dest: Option<Dest> = sqlx::query_as(&format!("SELECT {} FROM warehouse_destinations WHERE id = $1", super::DEST_COLS)).bind(run.destination_id).fetch_optional(&state.db).await?;
    let Some(dest) = dest else { return Ok(()) };
    if run.status == "queued" {
        sqlx::query("UPDATE warehouse_runs SET status = 'running', started_at = now() WHERE id = $1").bind(run.id).execute(&state.db).await?;
    }
    match advance(state, &run, &dest).await {
        Ok(true) => {
            crate::jobs::enqueue(&state.db, "warehouse_run", json!({"run_id": run.id}), None).await?;
        }
        Ok(false) => {}
        Err(e) => {
            let msg = format!("{e:#}");
            let attempts = run.attempts + 1;
            if attempts >= MAX_ATTEMPTS {
                sqlx::query("UPDATE warehouse_runs SET status = 'failed', attempts = $2, error = $3, finished_at = now() WHERE id = $1").bind(run.id).bind(attempts).bind(&msg).execute(&state.db).await?;
                let text = format!(
                    "The {} export to \"{}\" (bucket {}) failed after {attempts} tries:\n\n{msg}\n\nCheck the destination in Settings → Data export. The next scheduled export starts from where the last good one ended, so nothing is skipped once it works again.",
                    run.kind, dest.name, dest.bucket
                );
                crate::features::webhooks::alert_account(state, run.account_id, "A data export failed", &text).await;
            } else {
                sqlx::query("UPDATE warehouse_runs SET attempts = $2, error = $3 WHERE id = $1").bind(run.id).bind(attempts).bind(&msg).execute(&state.db).await?;
                let wait = chrono::Duration::seconds(60 * 2i64.pow(attempts as u32));
                crate::jobs::enqueue(&state.db, "warehouse_run", json!({"run_id": run.id}), Some(Utc::now() + wait)).await?;
            }
        }
    }
    Ok(())
}

/// Write one file, or finish the run. Returns whether there's more to do.
async fn advance(state: &AppState, run: &RunRow, dest: &Dest) -> anyhow::Result<bool> {
    let mut progress: Progress = serde_json::from_value(run.progress.clone())?;
    let bucket = bucket(state, dest)?;
    let Some(i) = progress.datasets.iter().position(|d| !d.done) else {
        finish(state, run, dest, &bucket, &progress).await?;
        return Ok(false);
    };
    let ds = datasets::find(&progress.datasets[i].name).ok_or_else(|| anyhow::anyhow!("unknown dataset {}", progress.datasets[i].name))?;
    let prefix = key_prefix(&dest.prefix);
    let file = write_file(state, &bucket, run, &prefix, &progress.date, &dest.format, ds, &mut progress.datasets[i]).await?;
    let (rows, bytes) = file.as_ref().map(|f| (f.rows as i64, f.bytes as i64)).unwrap_or((0, 0));
    sqlx::query("UPDATE warehouse_runs SET progress = $2, rows = rows + $3, files = files + $4, bytes = bytes + $5, attempts = 0, error = NULL WHERE id = $1")
        .bind(run.id)
        .bind(serde_json::to_value(&progress)?)
        .bind(rows)
        .bind(file.is_some() as i32)
        .bind(bytes)
        .execute(&state.db)
        .await?;
    Ok(true)
}

/// Gzip output that goes to the bucket in parts as it grows: one PUT for a small file, a
/// multipart upload for a big one. Memory stays around one part.
struct Upload<'a> {
    bucket: &'a Bucket,
    key: String,
    gz: GzEncoder<Vec<u8>>,
    upload_id: Option<String>,
    parts: Vec<String>,
    bytes: u64,
}

impl<'a> Upload<'a> {
    fn new(bucket: &'a Bucket, key: String) -> Self {
        Upload { bucket, key, gz: GzEncoder::new(Vec::new(), Compression::default()), upload_id: None, parts: Vec::new(), bytes: 0 }
    }

    async fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.gz.write_all(data)?;
        if self.gz.get_ref().len() >= PART_BYTES {
            let chunk = std::mem::take(self.gz.get_mut());
            self.part(chunk).await?;
        }
        Ok(())
    }

    async fn part(&mut self, chunk: Vec<u8>) -> anyhow::Result<()> {
        if self.upload_id.is_none() {
            self.upload_id = Some(self.bucket.create_multipart(&self.key, "application/gzip").await?);
        }
        self.bytes += chunk.len() as u64;
        let id = self.upload_id.clone().unwrap_or_default();
        let etag = self.bucket.upload_part(&self.key, &id, self.parts.len() + 1, chunk).await?;
        self.parts.push(etag);
        Ok(())
    }

    async fn finish(mut self) -> anyhow::Result<u64> {
        let rest = std::mem::replace(&mut self.gz, GzEncoder::new(Vec::new(), Compression::default())).finish()?;
        match self.upload_id.clone() {
            None => {
                self.bytes = rest.len() as u64;
                self.bucket.put(&self.key, rest, "application/gzip").await?;
            }
            Some(id) => {
                if !rest.is_empty() {
                    self.part(rest).await?;
                }
                self.bucket.complete_multipart(&self.key, &id, &self.parts).await?;
            }
        }
        Ok(self.bytes)
    }

    async fn abort(self) {
        if let Some(id) = &self.upload_id {
            self.bucket.abort_multipart(&self.key, id).await;
        }
    }
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

/// Write the next file of a dataset. None when there was nothing (more) to write.
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
    let key = format!("{prefix}{}/dt={date}/run={}/part-{:05}.{}.gz", ds.name, run.id, p.files.len(), if csv { "csv" } else { "jsonl" });
    let mut up = Upload::new(bucket, key.clone());
    let res: anyhow::Result<(usize, Option<(String, String)>, bool)> = async {
        if csv {
            up.write(&csv_line(&ds.columns.iter().map(|c| c.to_string()).collect::<Vec<_>>())?).await?;
        }
        let started = Instant::now();
        let mut cursor = p.cursor.clone();
        let mut rows = 0usize;
        loop {
            let batch: Vec<(Value, String, String)> = sqlx::query_as(ds.sql)
                .bind(run.account_id)
                .bind(p.from)
                .bind(p.to)
                .bind(cursor.as_ref().map(|c| c.0.clone()))
                .bind(cursor.as_ref().map(|c| c.1.clone()).filter(|s| !s.is_empty()))
                .bind(BATCH_ROWS)
                .fetch_all(&state.db)
                .await?;
            let n = batch.len();
            let mut buf = Vec::with_capacity(n * 256);
            for (row, a, b) in batch {
                if csv {
                    buf.extend(csv_line(&ds.columns.iter().map(|c| csv_cell(row.get(*c))).collect::<Vec<_>>())?);
                } else {
                    serde_json::to_writer(&mut buf, &row)?;
                    buf.push(b'\n');
                }
                cursor = Some((a, b));
            }
            up.write(&buf).await?;
            rows += n;
            if (n as i64) < BATCH_ROWS {
                return Ok((rows, cursor, true));
            }
            if rows >= FILE_ROWS || started.elapsed() > FILE_TIME {
                return Ok((rows, cursor, false));
            }
        }
    }
    .await;
    let (rows, cursor, done) = match res {
        Ok(r) => r,
        Err(e) => {
            up.abort().await;
            return Err(e);
        }
    };
    if rows == 0 {
        up.abort().await;
        p.done = true;
        return Ok(None);
    }
    let bytes = up.finish().await?;
    let info = FileInfo { key, rows, bytes };
    p.files.push(info.clone());
    p.cursor = cursor;
    p.rows += rows as i64;
    p.done = done;
    Ok(Some(info))
}

/// Write the manifest, move watermarks forward and mark the run done.
async fn finish(state: &AppState, run: &RunRow, dest: &Dest, bucket: &Bucket, p: &Progress) -> anyhow::Result<()> {
    let prefix = key_prefix(&dest.prefix);
    let manifest_key = format!("{prefix}_manifests/dt={}/run={}.json", p.date, run.id);
    let manifest = json!({
        "run_id": run.id, "account_id": run.account_id, "destination_id": dest.id, "kind": run.kind,
        "format": if dest.format == "csv" { "csv" } else { "jsonl" }, "compression": "gzip", "schema_version": 1,
        "started_at": run.created_at, "finished_at": Utc::now(), "window_end": run.window_end, "date": p.date,
        "datasets": p.datasets.iter().map(|d| json!({
            "name": d.name, "mode": if d.incremental { "incremental" } else { "snapshot" },
            "window": if d.incremental { json!({"after": d.from, "through": d.to}) } else { Value::Null },
            "rows": d.rows, "files": d.files,
        })).collect::<Vec<_>>(),
    });
    bucket.put(&manifest_key, serde_json::to_vec_pretty(&manifest)?, "application/json").await?;
    for d in p.datasets.iter().filter(|d| d.incremental) {
        sqlx::query(
            "INSERT INTO warehouse_watermarks (destination_id, dataset, exported_until) VALUES ($1, $2, $3)
             ON CONFLICT (destination_id, dataset) DO UPDATE SET exported_until = GREATEST(warehouse_watermarks.exported_until, EXCLUDED.exported_until)",
        )
        .bind(dest.id)
        .bind(&d.name)
        .bind(d.to)
        .execute(&state.db)
        .await?;
    }
    sqlx::query("UPDATE warehouse_runs SET status = 'succeeded', finished_at = now(), manifest_key = $2, error = NULL WHERE id = $1").bind(run.id).bind(&manifest_key).execute(&state.db).await?;
    sqlx::query("UPDATE warehouse_destinations SET last_run_at = now() WHERE id = $1").bind(dest.id).execute(&state.db).await?;
    Ok(())
}

/// Every minute: start due scheduled runs, and requeue runs whose job went missing.
pub(super) async fn tick(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<Dest> = sqlx::query_as(&format!(
        "SELECT {} FROM warehouse_destinations WHERE enabled AND schedule <> 'manual' AND next_run_at <= now() ORDER BY next_run_at LIMIT 100",
        super::DEST_COLS
    ))
    .fetch_all(&state.db)
    .await?;
    for d in due {
        let tz = account_tz(state, d.account_id).await;
        sqlx::query("UPDATE warehouse_destinations SET next_run_at = $2 WHERE id = $1").bind(d.id).bind(next_slot(&d.schedule, tz, Utc::now())).execute(&state.db).await?;
        match start(state, &d, "incremental").await {
            Ok(_) | Err(AppError::Conflict(_)) => {}
            Err(e) => tracing::warn!(destination = %d.id, error = %e.public_message(), "couldn't start scheduled export"),
        }
    }
    let orphans: Vec<Uuid> = sqlx::query_scalar(
        "SELECT r.id FROM warehouse_runs r WHERE r.status IN ('queued', 'running') AND r.created_at < now() - interval '2 minutes'
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'warehouse_run' AND j.payload->>'run_id' = r.id::text AND j.run_at <> 'infinity')
         LIMIT 100",
    )
    .fetch_all(&state.db)
    .await?;
    for id in orphans {
        crate::jobs::enqueue(&state.db, "warehouse_run", json!({"run_id": id}), None).await?;
    }
    Ok(())
}

pub(super) async fn prune(state: &AppState) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM warehouse_runs WHERE created_at < now() - interval '90 days' AND status IN ('succeeded', 'failed')").execute(&state.db).await?;
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
    fn csv_cells() {
        assert_eq!(csv_cell(Some(&json!({"a": 1}))), "{\"a\":1}");
        assert_eq!(csv_cell(None), "");
        assert_eq!(String::from_utf8(csv_line(&["a,b".into(), "c".into()]).unwrap()).unwrap(), "\"a,b\",c\n");
    }
}
