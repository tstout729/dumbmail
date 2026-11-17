//! CSV exports. A background job writes the file in batches of rows into `export_chunks`; members
//! download it while signed in, for 7 days. One export runs per account at a time and a file can't
//! grow past [`MAX_BYTES`]: rows are read one at a time (only the requested custom properties), and
//! each is counted as it's written, so neither a chunk nor the file can pass its limit in memory.

use chrono::{DateTime, NaiveDate, Utc};
use futures_util::TryStreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::query::{self, local_ts, Run};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const MAX_BYTES: i64 = 100 * 1024 * 1024;
/// Most rows written per job run (one chunk).
const BATCH: i64 = 5000;
/// Most bytes in one chunk; a run stops early once its chunk reaches this.
pub const CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// How big a file and each of its chunks may get.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_bytes: i64,
    pub chunk_bytes: usize,
}

pub const LIMITS: Limits = Limits { max_bytes: MAX_BYTES, chunk_bytes: CHUNK_BYTES };
const TOO_BIG: &str = "This export is over 100 MB. Export a list or segment, or fewer columns, instead.";
const MAX_PROPERTIES: usize = 50;

pub const KINDS: &[&str] = &["profiles", "events", "campaign_recipients", "suppressions"];

pub const PROFILE_COLUMNS: &[(&str, &str)] = &[
    ("id", "Profile ID"),
    ("email", "Email"),
    ("phone_number", "Phone number"),
    ("external_id", "External ID"),
    ("first_name", "First name"),
    ("last_name", "Last name"),
    ("email_consent", "Email consent"),
    ("sms_consent", "SMS consent"),
    ("email_suppressed_reason", "Email suppressed reason"),
    ("created_at", "Created"),
    ("updated_at", "Updated"),
    ("last_event_at", "Last active"),
];
const DEFAULT_PROFILE_COLUMNS: &[&str] = &["email", "phone_number", "first_name", "last_name", "email_consent", "sms_consent", "created_at"];

/// Earlier than any row (and inside Postgres' timestamp range, unlike `DateTime::MIN_UTC`).
fn floor() -> DateTime<Utc> {
    DateTime::from_timestamp(-2_208_988_800, 0).unwrap_or(DateTime::UNIX_EPOCH)
}

/// Keep a spreadsheet from running a cell as a formula (CSV injection).
pub fn safe_cell(s: &str) -> String {
    if s.starts_with(['=', '+', '-', '@', '\t', '\r']) && s.parse::<f64>().is_err() {
        format!("'{s}")
    } else {
        s.to_string()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub kind: String,
    /// profiles: all | list | segment (default all)
    pub scope: Option<String>,
    pub list_id: Option<Uuid>,
    pub segment_id: Option<Uuid>,
    /// profiles: columns to include (default: email, phone, names, consent, created).
    pub columns: Option<Vec<String>>,
    /// profiles: custom property names to add as columns.
    pub properties: Option<Vec<String>>,
    /// events: the metric's name.
    pub metric: Option<String>,
    /// events: first and last day (account time zone), default the last 30 days.
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
    /// campaign_recipients: the campaign.
    pub campaign_id: Option<Uuid>,
}

async fn validate(state: &AppState, account_id: Uuid, r: &Request) -> AppResult<(Value, String)> {
    let (_, tz) = query::account_tz(state, account_id).await?;
    let today = Utc::now().with_timezone(&tz).format("%Y-%m-%d").to_string();
    match r.kind.as_str() {
        "profiles" => {
            let scope = r.scope.as_deref().unwrap_or("all");
            let (label, list_id, segment_id) = match scope {
                "all" => ("all".to_string(), None, None),
                "list" => {
                    let id = r.list_id.ok_or_else(|| AppError::bad("pick the list to export"))?;
                    let name: String = sqlx::query_scalar("SELECT name FROM lists WHERE id = $1 AND account_id = $2")
                        .bind(id)
                        .bind(account_id)
                        .fetch_optional(&state.db)
                        .await?
                        .ok_or_else(|| AppError::not_found("list"))?;
                    (name, Some(id), None)
                }
                "segment" => {
                    let id = r.segment_id.ok_or_else(|| AppError::bad("pick the segment to export"))?;
                    let name: String = sqlx::query_scalar("SELECT name FROM segments WHERE id = $1 AND account_id = $2")
                        .bind(id)
                        .bind(account_id)
                        .fetch_optional(&state.db)
                        .await?
                        .ok_or_else(|| AppError::not_found("segment"))?;
                    (name, None, Some(id))
                }
                _ => return Err(AppError::bad("scope must be all, list or segment")),
            };
            let mut columns: Vec<String> = Vec::new();
            for c in r.columns.clone().unwrap_or_else(|| DEFAULT_PROFILE_COLUMNS.iter().map(|s| s.to_string()).collect()) {
                if !columns.contains(&c) {
                    columns.push(c);
                }
            }
            if columns.is_empty() && r.properties.as_ref().is_none_or(|p| p.is_empty()) {
                return Err(AppError::bad("pick at least one column"));
            }
            for c in &columns {
                if !PROFILE_COLUMNS.iter().any(|(k, _)| k == c) {
                    return Err(AppError::bad(format!("unknown column {c:?}; use: {}", PROFILE_COLUMNS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", "))));
                }
            }
            let mut props: Vec<String> = Vec::new();
            for p in r.properties.clone().unwrap_or_default() {
                let p = p.trim().to_string();
                if p.is_empty() || p.len() > 100 {
                    return Err(AppError::bad("property names must be 1-100 characters"));
                }
                if !props.contains(&p) {
                    props.push(p);
                }
            }
            if props.len() > MAX_PROPERTIES {
                return Err(AppError::bad(format!("export at most {MAX_PROPERTIES} custom properties")));
            }
            Ok((json!({"scope": scope, "list_id": list_id, "segment_id": segment_id, "columns": columns, "properties": props}), format!("profiles-{}-{today}.csv", slug(&label))))
        }
        "events" => {
            let metric = r.metric.as_deref().map(str::trim).filter(|m| !m.is_empty() && m.len() <= 200).ok_or_else(|| AppError::bad("pick the metric to export"))?;
            let w = query::resolve_range(tz, Some(30), r.start, r.end)?;
            Ok((json!({"metric": metric, "start": w.start, "end": w.end}), format!("events-{}-{today}.csv", slug(metric))))
        }
        "campaign_recipients" => {
            let id = r.campaign_id.ok_or_else(|| AppError::bad("pick the campaign to export"))?;
            let name: String = sqlx::query_scalar("SELECT name FROM campaigns WHERE id = $1 AND account_id = $2")
                .bind(id)
                .bind(account_id)
                .fetch_optional(&state.db)
                .await?
                .ok_or_else(|| AppError::not_found("campaign"))?;
            Ok((json!({"campaign_id": id}), format!("recipients-{}-{today}.csv", slug(&name))))
        }
        "suppressions" => Ok((json!({}), format!("unsubscribes-and-suppressions-{today}.csv"))),
        _ => Err(AppError::bad(format!("kind must be one of: {}", KINDS.join(", ")))),
    }
}

fn slug(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let mut out = String::new();
    for part in s.split('-').filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(part);
    }
    out.chars().take(60).collect::<String>().trim_end_matches('-').to_string()
}

type Row = (Uuid, String, Value, String, String, i64, i64, Option<String>, DateTime<Utc>, Option<DateTime<Utc>>, DateTime<Utc>);
const COLS: &str = "id, kind, params, status, filename, row_count, size_bytes, error, created_at, finished_at, expires_at";

fn row_json(r: Row) -> Value {
    let expired = r.10 <= Utc::now();
    json!({
        "id": r.0, "kind": r.1, "params": r.2, "status": if expired && r.3 == "done" { "expired" } else { r.3.as_str() },
        "filename": r.4, "row_count": r.5, "size_bytes": r.6, "error": r.7, "created_at": r.8, "finished_at": r.9, "expires_at": r.10,
        "download_url": (r.3 == "done" && !expired).then(|| format!("/api/reports/exports/{}/download", r.0)),
    })
}

/// Start an export for `ctx` (recorded in the audit log as soon as it's queued).
pub async fn start(state: &AppState, ctx: &crate::auth::Ctx, r: Request) -> AppResult<Value> {
    let (account_id, user_id) = (ctx.account_id, ctx.user_id);
    let (params, filename) = validate(state, account_id, &r).await?;
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query("INSERT INTO exports (id, account_id, kind, params, filename, created_by) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(account_id)
        .bind(&r.kind)
        .bind(&params)
        .bind(&filename)
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if inserted == 0 {
        return Err(AppError::Conflict("An export is already running for this account. Wait for it to finish, then start the next one.".into()));
    }
    crate::jobs::enqueue(&mut *tx, "reports_export", json!({"export_id": id}), None).await?;
    tx.commit().await?;
    crate::features::team::audit(state, ctx, "export.start", Some(&id.to_string()), json!({"kind": r.kind})).await;
    get(state, account_id, id).await
}

pub async fn list(state: &AppState, account_id: Uuid) -> AppResult<Vec<Value>> {
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {COLS} FROM exports WHERE account_id = $1 AND expires_at > now() - interval '1 day' ORDER BY created_at DESC LIMIT 50"))
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    Ok(rows.into_iter().map(row_json).collect())
}

pub async fn get(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let r: Option<Row> = sqlx::query_as(&format!("SELECT {COLS} FROM exports WHERE id = $1 AND account_id = $2"))
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    r.map(row_json).ok_or_else(|| AppError::not_found("export"))
}

/// The file's name and chunk count, if it's ready and not expired.
pub async fn ready(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<(String, i32, i64)> {
    let r: Option<(String, String, i32, i64, DateTime<Utc>)> = sqlx::query_as("SELECT status, filename, chunks, size_bytes, expires_at FROM exports WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let (status, filename, chunks, size, expires) = r.ok_or_else(|| AppError::not_found("export"))?;
    if expires <= Utc::now() {
        return Err(AppError::NotFound("This export expired. Start a new one.".into()));
    }
    if status != "done" {
        return Err(AppError::Conflict("This export isn't finished yet.".into()));
    }
    Ok((filename, chunks, size))
}

pub async fn chunk(state: &AppState, id: Uuid, seq: i32) -> anyhow::Result<Vec<u8>> {
    Ok(sqlx::query_scalar("SELECT bytes FROM export_chunks WHERE export_id = $1 AND seq = $2").bind(id).bind(seq).fetch_one(&state.db).await?)
}

/// Store a finished file in one go (scheduled report CSVs, which are small). Returns the export id.
pub async fn store_file(state: &AppState, account_id: Uuid, kind: &str, params: Value, filename: &str, bytes: &[u8]) -> anyhow::Result<Uuid> {
    if bytes.len() > CHUNK_BYTES {
        anyhow::bail!("report file is {} bytes, over the {CHUNK_BYTES}-byte limit", bytes.len());
    }
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    sqlx::query("INSERT INTO exports (id, account_id, kind, params, filename, status, row_count, size_bytes, chunks, finished_at) VALUES ($1, $2, $3, $4, $5, 'done', 0, $6, 1, now())")
        .bind(id)
        .bind(account_id)
        .bind(kind)
        .bind(params)
        .bind(filename)
        .bind(bytes.len() as i64)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO export_chunks (export_id, seq, bytes) VALUES ($1, 0, $2)").bind(id).bind(bytes).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(id)
}

/// A CSV chunk that knows its size and refuses to pass the file's limit.
struct Chunk {
    w: csv::Writer<Vec<u8>>,
    /// Bytes this chunk may hold before the file is over its limit.
    room: usize,
    /// Bytes after which this run stops and leaves the rest for the next run.
    target: usize,
    rows: i64,
    over: bool,
}

impl Chunk {
    fn new(file_size: i64, limits: Limits) -> Self {
        let room = usize::try_from((limits.max_bytes - file_size).max(0)).unwrap_or(usize::MAX);
        Chunk { w: csv::WriterBuilder::new().has_headers(false).from_writer(Vec::new()), room, target: limits.chunk_bytes.min(room), rows: 0, over: false }
    }

    fn len(&self) -> usize {
        self.w.get_ref().len()
    }

    /// Write one record. False when the run should stop: the chunk is full, or the file would pass
    /// its limit (then `over` is set).
    fn write(&mut self, cells: &[String], is_row: bool) -> anyhow::Result<bool> {
        self.w.write_record(cells.iter().map(|c| safe_cell(c)))?;
        self.w.flush()?;
        if self.len() > self.room {
            self.over = true;
            return Ok(false);
        }
        if is_row {
            self.rows += 1;
        }
        Ok(self.len() < self.target)
    }

    fn into_bytes(self) -> anyhow::Result<Vec<u8>> {
        self.w.into_inner().map_err(|e| anyhow::anyhow!("csv: {e}"))
    }
}

/// One job run: write the next batch of rows as a chunk, then queue the next run or finish.
pub async fn run_batch(state: &AppState, export_id: Uuid) -> anyhow::Result<()> {
    run_batch_with(state, export_id, LIMITS).await
}

/// [`run_batch`] with other size limits (tests use small ones).
pub async fn run_batch_with(state: &AppState, export_id: Uuid, limits: Limits) -> anyhow::Result<()> {
    let row: Option<(Uuid, String, Value, String, i32, i64, Option<Value>)> =
        sqlx::query_as("SELECT account_id, kind, params, status, chunks, size_bytes, cursor FROM exports WHERE id = $1")
            .bind(export_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((account_id, kind, params, status, chunks, size, cursor)) = row else { return Ok(()) };
    if status != "queued" && status != "running" {
        return Ok(());
    }
    let res = write_batch(state, account_id, &kind, &params, chunks, cursor, Chunk::new(size, limits)).await;
    match res {
        Ok((chunk, next)) => {
            if chunk.over {
                fail(state, export_id, TOO_BIG).await?;
                return Ok(());
            }
            let rows = chunk.rows;
            let bytes = chunk.into_bytes()?;
            let new_size = size + bytes.len() as i64;
            let mut tx = state.db.begin().await?;
            if !bytes.is_empty() {
                sqlx::query("INSERT INTO export_chunks (export_id, seq, bytes) VALUES ($1, $2, $3) ON CONFLICT (export_id, seq) DO UPDATE SET bytes = $3")
                    .bind(export_id)
                    .bind(chunks)
                    .bind(&bytes)
                    .execute(&mut *tx)
                    .await?;
            }
            let added_chunk = i32::from(!bytes.is_empty());
            match next {
                Some(c) => {
                    sqlx::query("UPDATE exports SET status = 'running', chunks = chunks + $2, size_bytes = $3, row_count = row_count + $4, cursor = $5 WHERE id = $1")
                        .bind(export_id)
                        .bind(added_chunk)
                        .bind(new_size)
                        .bind(rows)
                        .bind(c)
                        .execute(&mut *tx)
                        .await?;
                    crate::jobs::enqueue(&mut *tx, "reports_export", json!({"export_id": export_id}), None).await?;
                }
                None => {
                    sqlx::query(
                        "UPDATE exports SET status = 'done', chunks = chunks + $2, size_bytes = $3, row_count = row_count + $4, cursor = NULL, finished_at = now(), expires_at = now() + interval '7 days' WHERE id = $1",
                    )
                    .bind(export_id)
                    .bind(added_chunk)
                    .bind(new_size)
                    .bind(rows)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            tx.commit().await?;
            Ok(())
        }
        Err(e) => {
            // Too much reporting running right now: try this batch again later.
            if let Some(AppError::Busy(_)) = e.downcast_ref::<AppError>() {
                return Err(e);
            }
            tracing::error!(export = %export_id, error = %format!("{e:#}"), "export batch failed");
            let msg = match e.downcast_ref::<AppError>() {
                Some(a) => a.public_message(),
                None => "Something went wrong writing this export. Try again.".into(),
            };
            fail(state, export_id, &msg).await?;
            Ok(())
        }
    }
}

async fn fail(state: &AppState, export_id: Uuid, msg: &str) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM export_chunks WHERE export_id = $1").bind(export_id).execute(&mut *tx).await?;
    sqlx::query("UPDATE exports SET status = 'failed', error = $2, finished_at = now(), chunks = 0, size_bytes = 0 WHERE id = $1").bind(export_id).bind(msg).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

type ProfileRow = (Uuid, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, String, String, Option<String>, DateTime<Utc>, DateTime<Utc>, Option<DateTime<Utc>>, Vec<Option<String>>);
type EventRow = (Uuid, DateTime<Utc>, Uuid, Option<String>, Option<String>, Option<f64>, String);
type RecipientRow = (Uuid, String, Option<String>, String, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, i64, f64);

/// Write one batch into `out`, row by row as they're read. Returns the chunk and the cursor for the
/// next batch (None: done).
async fn write_batch(state: &AppState, account_id: Uuid, kind: &str, p: &Value, chunks: i32, cursor: Option<Value>, mut out: Chunk) -> anyhow::Result<(Chunk, Option<Value>)> {
    let (_, tz) = query::account_tz(state, account_id).await?;
    let first = chunks == 0 && cursor.is_none();
    // Keyset cursor: the last written row's sort key.
    let after_ts: DateTime<Utc> = cursor.as_ref().and_then(|c| c["t"].as_str()).and_then(|s| s.parse().ok()).unwrap_or(floor());
    let after_id: Uuid = cursor.as_ref().and_then(|c| c["id"].as_str()).and_then(|s| s.parse().ok()).unwrap_or(Uuid::nil());
    let run = Run::background(query::JOB_BUDGET).await?;
    let mut tx = run.tx(state).await?;
    // Ends when the query runs out of rows, the batch's row limit is hit, the chunk is full or the
    // file would get too big; `last` is the last row written.
    let mut stopped = false;
    let mut last: Option<Value> = None;
    let mut read: i64 = 0;
    match kind {
        "profiles" | "suppressions" => {
            let columns: Vec<String> = if kind == "profiles" {
                serde_json::from_value(p["columns"].clone())?
            } else {
                ["email", "phone_number", "email_consent", "sms_consent", "email_suppressed_reason", "updated_at"].iter().map(|s| s.to_string()).collect()
            };
            let props: Vec<String> = serde_json::from_value(p["properties"].clone()).unwrap_or_default();
            if first {
                let mut h: Vec<String> = columns.iter().map(|c| PROFILE_COLUMNS.iter().find(|(k, _)| k == c).map(|(_, l)| l.to_string()).unwrap_or_default()).collect();
                h.extend(props.iter().cloned());
                if !out.write(&h, false)? {
                    stopped = true;
                }
            }
            let scope = p["scope"].as_str().unwrap_or("all");
            let join = match (kind, scope) {
                ("profiles", "list") => "JOIN list_members x ON x.profile_id = p.id AND x.list_id = $4",
                ("profiles", "segment") => "JOIN segment_members x ON x.profile_id = p.id AND x.segment_id = $4",
                _ => "",
            };
            let only = if kind == "suppressions" {
                " AND (p.email_consent = 'unsubscribed' OR p.sms_consent = 'unsubscribed' OR p.email_suppressed_reason IS NOT NULL)"
            } else {
                ""
            };
            let scope_id: Option<Uuid> = p["list_id"].as_str().or(p["segment_id"].as_str()).and_then(|s| s.parse().ok());
            // Only the custom properties asked for leave the database, not the whole properties object.
            let sql = format!(
                "SELECT p.id, p.email, p.phone_number, p.external_id, p.first_name, p.last_name, p.email_consent, p.sms_consent, p.email_suppressed_reason,
                        p.created_at, p.updated_at, p.last_event_at,
                        ARRAY(SELECT p.properties->>u.k FROM unnest($5::text[]) WITH ORDINALITY u(k, i) ORDER BY u.i)
                 FROM profiles p {join}
                 WHERE p.account_id = $1 AND (p.created_at, p.id) > ($2, $3){only}
                 ORDER BY p.created_at, p.id LIMIT {BATCH}"
            );
            let conn = tx.arm().await?;
            let mut rows = sqlx::query_as::<_, ProfileRow>(&sql).bind(account_id).bind(after_ts).bind(after_id).bind(scope_id).bind(&props).fetch(&mut **conn);
            while !stopped {
                let Some(r) = rows.try_next().await.map_err(query::db_err)? else { break };
                read += 1;
                let mut cells = Vec::with_capacity(columns.len() + props.len());
                for c in &columns {
                    cells.push(match c.as_str() {
                        "id" => r.0.to_string(),
                        "email" => r.1.clone().unwrap_or_default(),
                        "phone_number" => r.2.clone().unwrap_or_default(),
                        "external_id" => r.3.clone().unwrap_or_default(),
                        "first_name" => r.4.clone().unwrap_or_default(),
                        "last_name" => r.5.clone().unwrap_or_default(),
                        "email_consent" => r.6.clone(),
                        "sms_consent" => r.7.clone(),
                        "email_suppressed_reason" => r.8.clone().unwrap_or_default(),
                        "created_at" => local_ts(tz, Some(r.9)),
                        "updated_at" => local_ts(tz, Some(r.10)),
                        "last_event_at" => local_ts(tz, r.11),
                        _ => String::new(),
                    });
                }
                cells.extend(r.12.into_iter().map(Option::unwrap_or_default));
                stopped = !out.write(&cells, true)?;
                last = Some(json!({"t": r.9, "id": r.0}));
            }
        }
        "events" => {
            if first && !out.write(&["Event ID", "Metric", "Time", "Profile ID", "Email", "Phone number", "Value", "Properties"].map(String::from), false)? {
                stopped = true;
            }
            let metric = p["metric"].as_str().unwrap_or("");
            let start: DateTime<Utc> = p["start"].as_str().and_then(|s| s.parse().ok()).unwrap_or(floor());
            let end: DateTime<Utc> = p["end"].as_str().and_then(|s| s.parse().ok()).unwrap_or_else(Utc::now);
            let from = if cursor.is_some() { after_ts } else { start };
            let sql = format!(
                "SELECT e.id, e.occurred_at, e.profile_id, p.email, p.phone_number, e.value, e.properties::text
                 FROM events e JOIN profiles p ON p.id = e.profile_id
                 WHERE e.account_id = $1 AND e.metric_id = (SELECT id FROM metrics WHERE account_id = $1 AND name = $2)
                   AND e.occurred_at < $4 AND (e.occurred_at, e.id) > ($3, $5) AND e.occurred_at >= $6
                 ORDER BY e.occurred_at, e.id LIMIT {BATCH}"
            );
            let conn = tx.arm().await?;
            let mut rows = sqlx::query_as::<_, EventRow>(&sql)
                .bind(account_id)
                .bind(metric)
                .bind(from)
                .bind(end)
                .bind(if cursor.is_some() { after_id } else { Uuid::nil() })
                .bind(start)
                .fetch(&mut **conn);
            while !stopped {
                let Some(r) = rows.try_next().await.map_err(query::db_err)? else { break };
                read += 1;
                let cells = [r.0.to_string(), metric.to_string(), local_ts(tz, Some(r.1)), r.2.to_string(), r.3.unwrap_or_default(), r.4.unwrap_or_default(), r.5.map(|v| v.to_string()).unwrap_or_default(), r.6];
                stopped = !out.write(&cells, true)?;
                last = Some(json!({"t": r.1, "id": r.0}));
            }
        }
        "campaign_recipients" => {
            if first
                && !out.write(
                    &["Profile ID", "Email or phone", "Version", "Status", "Bounce type", "Sent", "Delivered", "Opened", "Clicked", "Unsubscribed", "Conversions", "Revenue"].map(String::from),
                    false,
                )?
            {
                stopped = true;
            }
            let campaign: Uuid = p["campaign_id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("export has no campaign"))?;
            let sql = format!(
                "SELECT m.profile_id, m.to_address, m.variant, m.status, m.bounce_type, m.sent_at, m.delivered_at, m.opened_at, m.clicked_at, m.unsubscribed_at,
                        COALESCE(conv.n, 0), COALESCE(conv.v, 0)::float8
                 FROM messages m
                 LEFT JOIN LATERAL (SELECT count(*) AS n, sum(x.value) AS v FROM ({} AND e.profile_id = m.profile_id) x WHERE x.campaign_id = $2) conv ON true
                 WHERE m.campaign_id = $2 AND m.account_id = $1 AND NOT m.is_test AND m.profile_id > $3
                 ORDER BY m.profile_id LIMIT {BATCH}",
                super::attribution::ATTRIBUTION_SQL
            );
            let conn = tx.arm().await?;
            let mut rows = sqlx::query_as::<_, RecipientRow>(&sql).bind(account_id).bind(campaign).bind(after_id).fetch(&mut **conn);
            while !stopped {
                let Some(r) = rows.try_next().await.map_err(query::db_err)? else { break };
                read += 1;
                let cells = [
                    r.0.to_string(),
                    r.1,
                    r.2.unwrap_or_default().to_uppercase(),
                    r.3,
                    r.4.unwrap_or_default(),
                    local_ts(tz, r.5),
                    local_ts(tz, r.6),
                    local_ts(tz, r.7),
                    local_ts(tz, r.8),
                    local_ts(tz, r.9),
                    r.10.to_string(),
                    format!("{:.2}", r.11),
                ];
                stopped = !out.write(&cells, true)?;
                last = Some(json!({"id": r.0}));
            }
        }
        _ => anyhow::bail!("unknown export kind {kind}"),
    }
    tx.commit().await?;
    // More to do when this run stopped early or read a full batch.
    // (A chunk filled by the header alone continues from the first row.)
    let next = if out.over { None } else if stopped || read == BATCH { Some(last.unwrap_or_else(|| json!({}))) } else { None };
    Ok((out, next))
}

/// Delete files a day after they expire.
pub async fn prune(state: &AppState) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM exports WHERE id IN (SELECT id FROM exports WHERE expires_at < now() - interval '1 day' LIMIT 500)").execute(&state.db).await?;
    // An export whose job died (worker restarted past its retries) is failed after a day so the account can start another.
    sqlx::query("UPDATE exports SET status = 'failed', error = 'This export stopped before it finished. Start it again.', finished_at = now()
                 WHERE status IN ('queued', 'running') AND created_at < now() - interval '1 day'")
        .execute(&state.db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_and_names() {
        assert_eq!(safe_cell("=1+1"), "'=1+1");
        assert_eq!(safe_cell("-5"), "-5");
        assert_eq!(safe_cell("-cmd"), "'-cmd");
        assert_eq!(safe_cell("@x"), "'@x");
        assert_eq!(safe_cell("hello"), "hello");
        assert_eq!(slug("VIP List / Spring!"), "vip-list-spring");
    }
}
