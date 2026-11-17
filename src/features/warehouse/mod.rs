//! Scheduled exports to S3-compatible object storage, for loading into a data warehouse
//! (Snowflake, BigQuery, Redshift and friends read the files straight from the bucket).
//!
//! A destination is a bucket with credentials (stored encrypted), a format (gzip'd JSON lines or
//! CSV), the datasets to export and a schedule. Runs are background jobs that write bounded files
//! in keyset order (see `run`), with a manifest per run. docs/DATA_EXPORT.md has the file layout,
//! the schemas and ready-to-paste warehouse SQL.
//!
//! Changing where or how files are written (endpoint, region, bucket, prefix, path style, format)
//! starts a new revision: runs in progress are cancelled, the watermarks are cleared so the next
//! run is a full baseline, and if requests would go to a different host the keys must be given
//! again (DumbMail never signs a request to a new host with keys saved for another).

mod datasets;
mod run;
mod s3;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::team::access::{rule, Access, Perm, RouteRule, ToolRule};
use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{obj, tool};
use crate::state::AppState;

const MAX_DESTINATIONS: i64 = 10;

/// Everything here copies every profile out of DumbMail, so it's for Admins and owners.
static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/warehouse/datasets", Perm::ExportsManage),
    rule("GET", "/api/warehouse/destinations", Perm::ExportsManage),
    rule("POST", "/api/warehouse/destinations", Perm::ExportsManage),
    rule("GET", "/api/warehouse/destinations/{id}", Perm::ExportsManage),
    rule("PATCH", "/api/warehouse/destinations/{id}", Perm::ExportsManage),
    rule("DELETE", "/api/warehouse/destinations/{id}", Perm::ExportsManage),
    rule("POST", "/api/warehouse/destinations/{id}/test", Perm::ExportsManage),
    rule("GET", "/api/warehouse/destinations/{id}/runs", Perm::ExportsManage),
    rule("POST", "/api/warehouse/destinations/{id}/runs", Perm::ExportsManage),
    rule("GET", "/api/warehouse/runs/{id}", Perm::ExportsManage),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "list_export_datasets", perm: Perm::ExportsManage },
    ToolRule { name: "list_export_destinations", perm: Perm::ExportsManage },
    ToolRule { name: "create_export_destination", perm: Perm::ExportsManage },
    ToolRule { name: "update_export_destination", perm: Perm::ExportsManage },
    ToolRule { name: "delete_export_destination", perm: Perm::ExportsManage },
    ToolRule { name: "test_export_destination", perm: Perm::ExportsManage },
    ToolRule { name: "start_warehouse_export", perm: Perm::ExportsManage },
    ToolRule { name: "list_export_runs", perm: Perm::ExportsManage },
];

pub struct Module;

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "warehouse"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/warehouse/datasets", get(datasets_h))
            .route("/api/warehouse/destinations", get(list_h).post(create_h))
            .route("/api/warehouse/destinations/{id}", get(get_h).patch(patch_h).delete(delete_h))
            .route("/api/warehouse/destinations/{id}/test", post(test_h))
            .route("/api/warehouse/destinations/{id}/runs", get(runs_h).post(start_h))
            .route("/api/warehouse/runs/{id}", get(run_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tool_defs()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        call_tool(state, ctx, name, args).await
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "data-export",
            name: "Data export",
            description: "Scheduled exports to S3, R2, GCS or Backblaze: datasets and their columns, file layout, manifests, and COPY / external table SQL for Snowflake, BigQuery and Redshift",
            text: include_str!("../../../docs/DATA_EXPORT.md"),
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            "warehouse_run" => Some(match payload["run_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
                Some(id) => run::step(state, id).await,
                None => Ok(()),
            }),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            run::tick(state).await?;
        }
        if tick.is_multiple_of(180) {
            run::prune(state).await?;
        }
        Ok(())
    }

    /// Incremental exports read profiles by `updated_at` and events by `created_at`. On big
    /// accounts that wants these indexes; exports work without them, slower.
    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    async fn after_start(&self, state: &AppState) -> anyhow::Result<()> {
        crate::features::reports::indexes::build(state, INDEX_LOCK_KEY, "data export", INDEXES).await
    }
}

const INDEX_LOCK_KEY: i64 = 0x0d4d_7761_7265_686f;
const INDEXES: &[(&str, &str)] = &[
    ("profiles_account_updated_idx", "CREATE INDEX CONCURRENTLY profiles_account_updated_idx ON profiles (account_id, updated_at, id)"),
    ("events_account_created_idx", "CREATE INDEX CONCURRENTLY events_account_created_idx ON events (account_id, created_at, id)"),
];

// ---------- The change log ----------

/// Which of `rows` (dataset, row id) the account's enabled destinations export, from the server's
/// snapshot (`features::observe`, no SQL; reloaded every few seconds). A new or moved
/// destination's first run waits [`run::BASELINE_DELAY_SECS`] (longer than a reload), so by the
/// time it reads its baseline every server is logging changes for it.
pub(crate) fn change_rows<'a>(db: &sqlx::PgPool, account_id: Uuid, rows: &[(&'a str, Uuid)]) -> Vec<(&'a str, Uuid)> {
    match crate::features::observe::peek(db) {
        Some(i) => match i.account(account_id) {
            Some(a) => rows.iter().copied().filter(|(d, _)| a.datasets.iter().any(|s| s == d)).collect(),
            None => Vec::new(),
        },
        // Not loaded yet: let the insert check.
        None => rows.to_vec(),
    }
}

/// Whether the account exports `dataset` (or the snapshot isn't loaded yet).
pub fn exports(db: &sqlx::PgPool, account_id: Uuid, dataset: &str) -> bool {
    !change_rows(db, account_id, &[(dataset, Uuid::nil())]).is_empty()
}

/// Append rows to the change log on a connection set up with `observe`. Plain inserts: a row that
/// changes twice is logged twice, and nothing here waits on another writer.
pub(crate) async fn write_changes(conn: &mut sqlx::PgConnection, account_id: Uuid, rows: &[(&str, Uuid)]) -> sqlx::Result<()> {
    for chunk in rows.chunks(5_000) {
        let (sets, ids): (Vec<&str>, Vec<Uuid>) = chunk.iter().copied().unzip();
        sqlx::query(
            "INSERT INTO warehouse_changes (account_id, dataset, row_id)
             SELECT $1, x.dataset, x.row_id FROM UNNEST($2::text[], $3::uuid[]) AS x(dataset, row_id)
             WHERE EXISTS (SELECT 1 FROM warehouse_destinations d WHERE d.account_id = $1 AND d.enabled AND x.dataset = ANY(d.datasets))",
        )
        .bind(account_id)
        .bind(&sets)
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Note that rows of a mutable dataset changed where they have no change column of their own (a
/// message's delivery, open, click or bounce; a profile's last activity), so incremental exports
/// pick them up again. Only for accounts exporting that dataset. Never fails or holds up the caller.
pub async fn changed_many(db: &sqlx::PgPool, account_id: Uuid, rows: &[(&str, Uuid)]) {
    let rows = change_rows(db, account_id, rows);
    if rows.is_empty() {
        return;
    }
    let Some(mut tx) = crate::features::observe::begin(db, "data export").await else { return };
    let res = write_changes(&mut tx, account_id, &rows).await;
    crate::features::observe::commit(tx, res, "data export").await;
}

/// [`changed_many`] for one row.
pub async fn changed(db: &sqlx::PgPool, account_id: Uuid, dataset: &str, row_id: Uuid) {
    changed_many(db, account_id, &[(dataset, row_id)]).await
}

/// The change-log rows for a recorded event: its profile's last activity moved, and a message
/// event (an open, a click, a bounce ...) changed its message.
pub(crate) fn event_change_rows(db: &sqlx::PgPool, account_id: Uuid, profile_id: Uuid, message_id: Option<Uuid>) -> Vec<(&'static str, Uuid)> {
    let mut rows = vec![("profiles", profile_id)];
    if let Some(m) = message_id {
        rows.push(("messages", m));
    }
    change_rows(db, account_id, &rows)
}

#[derive(sqlx::FromRow)]
pub(crate) struct Dest {
    id: Uuid,
    account_id: Uuid,
    name: String,
    endpoint: Option<String>,
    region: String,
    bucket: String,
    prefix: String,
    path_style: bool,
    access_key_enc: String,
    secret_key_enc: String,
    access_key_hint: String,
    format: String,
    datasets: Vec<String>,
    schedule: String,
    enabled: bool,
    revision: i32,
    baseline_after: DateTime<Utc>,
    next_run_at: Option<DateTime<Utc>>,
    last_run_at: Option<DateTime<Utc>>,
    last_test_at: Option<DateTime<Utc>>,
    last_test_error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

const DEST_COLS: &str = "id, account_id, name, endpoint, region, bucket, prefix, path_style, access_key_enc, secret_key_enc, access_key_hint, format, datasets, schedule,
    enabled, revision, baseline_after, next_run_at, last_run_at, last_test_at, last_test_error, created_at, updated_at";

async fn load(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Dest> {
    sqlx::query_as::<_, Dest>(&format!("SELECT {DEST_COLS} FROM warehouse_destinations WHERE id = $1 AND account_id = $2"))
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::not_found("export destination"))
}

async fn dest_json(state: &AppState, d: &Dest) -> AppResult<Value> {
    let last: Option<RunRow> = sqlx::query_as(&format!("SELECT {RUN_COLS} FROM warehouse_runs WHERE destination_id = $1 ORDER BY created_at DESC LIMIT 1")).bind(d.id).fetch_optional(&state.db).await?;
    Ok(json!({
        "id": d.id, "name": d.name, "endpoint": d.endpoint, "region": d.region, "bucket": d.bucket, "prefix": d.prefix, "path_style": d.path_style,
        "access_key_hint": d.access_key_hint, "format": d.format, "datasets": d.datasets, "schedule": d.schedule, "enabled": d.enabled,
        "next_run_at": d.next_run_at.filter(|_| d.enabled && d.schedule != "manual"), "last_run_at": d.last_run_at,
        "last_test_at": d.last_test_at, "last_test_error": d.last_test_error, "created_at": d.created_at, "updated_at": d.updated_at,
        "last_run": last.as_ref().map(run_json),
    }))
}

#[derive(sqlx::FromRow)]
struct RunRow {
    id: Uuid,
    destination_id: Uuid,
    kind: String,
    status: String,
    progress: Value,
    window_end: DateTime<Utc>,
    rows: i64,
    files: i32,
    bytes: i64,
    attempts: i32,
    error: Option<String>,
    manifest_key: Option<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

const RUN_COLS: &str = "id, destination_id, kind, status, progress, window_end, rows, files, bytes, attempts, error, manifest_key, created_at, started_at, finished_at";

fn run_json(r: &RunRow) -> Value {
    let datasets: Vec<Value> = r.progress["datasets"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|d| json!({"name": d["name"], "mode": if d["incremental"] == json!(true) { "incremental" } else { "snapshot" }, "rows": d["rows"],
                    "files": d["files"].as_array().map(|f| f.len()).unwrap_or(0), "done": d["done"], "after": d["from"], "through": d["to"]}))
                .collect()
        })
        .unwrap_or_default();
    json!({"id": r.id, "destination_id": r.destination_id, "kind": r.kind, "status": r.status, "rows": r.rows, "files": r.files, "bytes": r.bytes,
        "attempts": r.attempts, "error": r.error, "manifest_key": r.manifest_key, "window_end": r.window_end, "created_at": r.created_at,
        "started_at": r.started_at, "finished_at": r.finished_at, "datasets": datasets})
}

// ---------- Validation ----------

#[derive(Default)]
struct Fields {
    name: Option<String>,
    endpoint: Option<Option<String>>,
    region: Option<String>,
    bucket: Option<String>,
    prefix: Option<String>,
    path_style: Option<bool>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    format: Option<String>,
    datasets: Option<Vec<String>>,
    schedule: Option<String>,
    enabled: Option<bool>,
}

impl Fields {
    fn touches_storage(&self) -> bool {
        self.endpoint.is_some() || self.region.is_some() || self.bucket.is_some() || self.prefix.is_some() || self.path_style.is_some() || self.access_key_id.is_some() || self.secret_access_key.is_some()
    }
}

fn parse_fields(b: &Value) -> AppResult<Fields> {
    let obj = b.as_object().ok_or_else(|| AppError::bad("send a JSON object"))?;
    let allowed = ["name", "endpoint", "region", "bucket", "prefix", "path_style", "access_key_id", "secret_access_key", "format", "datasets", "schedule", "enabled", "revision"];
    if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(AppError::bad(format!("unknown field {k:?}; allowed: {}", allowed.join(", "))));
    }
    let text = |k: &str, max: usize| -> AppResult<Option<String>> {
        match obj.get(k) {
            None => Ok(None),
            Some(Value::String(s)) if s.trim().chars().count() <= max => Ok(Some(s.trim().to_string())),
            Some(Value::String(_)) => Err(AppError::bad(format!("{k} is {max} characters at most"))),
            Some(_) => Err(AppError::bad(format!("{k} must be a string"))),
        }
    };
    let mut f = Fields { name: text("name", 100)?, region: text("region", 50)?, bucket: text("bucket", 255)?, prefix: text("prefix", 500)?, ..Default::default() };
    if f.name.as_deref() == Some("") {
        return Err(AppError::bad("give the destination a name"));
    }
    if let Some(r) = &f.region {
        if r.is_empty() || !r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(AppError::bad("region looks like us-east-1 (or auto for Cloudflare R2)"));
        }
    }
    if let Some(bk) = &f.bucket {
        if bk.len() < 3 || !bk.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_')) {
            return Err(AppError::bad("bucket is the bucket's name: 3-255 lowercase letters, digits, dots, dashes or underscores"));
        }
    }
    if let Some(p) = &f.prefix {
        if !p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '=')) || p.contains("..") || s3::has_dot_segment(p) {
            return Err(AppError::bad("prefix may use letters, digits and / - _ . = (and no folder named . or ..)"));
        }
    }
    if let Some(v) = obj.get("endpoint") {
        let e = v.as_str().map(str::trim).unwrap_or("");
        f.endpoint = Some(if e.is_empty() {
            None
        } else {
            let u = url::Url::parse(e).map_err(|_| AppError::bad("endpoint must be a URL like https://<account id>.r2.cloudflarestorage.com"))?;
            if u.query().is_some() || u.username() != "" || u.password().is_some() {
                return Err(AppError::bad("endpoint can't include a query, user name or password"));
            }
            Some(e.trim_end_matches('/').to_string())
        });
    }
    if let Some(v) = obj.get("path_style") {
        f.path_style = Some(v.as_bool().ok_or_else(|| AppError::bad("path_style must be true or false"))?);
    }
    f.access_key_id = text("access_key_id", 256)?;
    f.secret_access_key = text("secret_access_key", 256)?;
    if f.access_key_id.as_deref() == Some("") || f.secret_access_key.as_deref() == Some("") {
        return Err(AppError::bad("access_key_id and secret_access_key can't be empty"));
    }
    if let Some(v) = text("format", 10)? {
        if !matches!(v.as_str(), "jsonl" | "csv") {
            return Err(AppError::bad("format must be jsonl or csv"));
        }
        f.format = Some(v);
    }
    if let Some(v) = obj.get("datasets") {
        let list: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| AppError::bad("datasets must be a list of dataset names"))?;
        let mut out = Vec::new();
        for d in list {
            if datasets::find(&d).is_none() {
                return Err(AppError::bad(format!("unknown dataset {d:?}; choose from {}", datasets::all_names().join(", "))));
            }
            if !out.contains(&d) {
                out.push(d);
            }
        }
        if out.is_empty() {
            return Err(AppError::bad("pick at least one dataset"));
        }
        f.datasets = Some(out);
    }
    if let Some(v) = text("schedule", 10)? {
        if !matches!(v.as_str(), "hourly" | "daily" | "manual") {
            return Err(AppError::bad("schedule must be hourly, daily or manual"));
        }
        f.schedule = Some(v);
    }
    if let Some(v) = obj.get("enabled") {
        f.enabled = Some(v.as_bool().ok_or_else(|| AppError::bad("enabled must be true or false"))?);
    }
    Ok(f)
}

fn key_hint(key: &str) -> String {
    let n = key.chars().count();
    if n <= 8 {
        return "…".into();
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// Write and delete a small file. The delete may be refused (write-only keys); that's fine. The
/// error is a fixed phrase with a status and an allowlisted code (see s3.rs), safe to show and keep.
async fn test_write(b: &s3::Bucket, prefix: &str) -> Result<(), String> {
    let key = format!("{}_dumbmail_test/{}.txt", run::key_prefix(prefix), Uuid::now_v7());
    b.put(&key, b"DumbMail checked that it can write here. You can delete this file.".to_vec(), "text/plain").await.map_err(|e| format!("{e:#}"))?;
    let _ = b.delete(&key).await;
    Ok(())
}

// ---------- Actions ----------

async fn list(state: &AppState, account_id: Uuid) -> AppResult<Vec<Value>> {
    let rows: Vec<Dest> = sqlx::query_as(&format!("SELECT {DEST_COLS} FROM warehouse_destinations WHERE account_id = $1 ORDER BY created_at")).bind(account_id).fetch_all(&state.db).await?;
    let mut out = Vec::new();
    for d in &rows {
        out.push(dest_json(state, d).await?);
    }
    Ok(out)
}

async fn create(state: &AppState, ctx: &Ctx, body: &Value) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let f = parse_fields(body)?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM warehouse_destinations WHERE account_id = $1").bind(account_id).fetch_one(&state.db).await?;
    if n >= MAX_DESTINATIONS {
        return Err(AppError::Conflict(format!("an account can have {MAX_DESTINATIONS} export destinations")));
    }
    let name = f.name.clone().ok_or_else(|| AppError::bad("name is required"))?;
    let region = f.region.clone().ok_or_else(|| AppError::bad("region is required (e.g. us-east-1, or auto for Cloudflare R2)"))?;
    let bucket_name = f.bucket.clone().ok_or_else(|| AppError::bad("bucket is required"))?;
    let access = f.access_key_id.clone().ok_or_else(|| AppError::bad("access_key_id is required"))?;
    let secret = f.secret_access_key.clone().ok_or_else(|| AppError::bad("secret_access_key is required"))?;
    let endpoint = f.endpoint.clone().flatten();
    let path_style = f.path_style.unwrap_or(endpoint.is_some() || bucket_name.contains('.'));
    let prefix = f.prefix.clone().unwrap_or_default();
    let b = s3::Bucket { endpoint: endpoint.clone(), region: region.clone(), bucket: bucket_name.clone(), path_style, access_key: access.clone(), secret_key: secret.clone() };
    test_write(&b, &prefix).await.map_err(|e| AppError::bad(format!("DumbMail couldn't write to that bucket: {e}")))?;
    let schedule = f.schedule.clone().unwrap_or_else(|| "daily".into());
    let enabled = f.enabled.unwrap_or(true);
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO warehouse_destinations (id, account_id, name, endpoint, region, bucket, prefix, path_style, access_key_enc, secret_key_enc, access_key_hint,
            format, datasets, schedule, enabled, next_run_at, last_test_at
            , baseline_after)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, CASE WHEN $14 = 'manual' THEN NULL ELSE now() END, now(),
            now() + make_interval(secs => $16))",
    )
    .bind(id)
    .bind(account_id)
    .bind(&name)
    .bind(&endpoint)
    .bind(&region)
    .bind(&bucket_name)
    .bind(&prefix)
    .bind(path_style)
    .bind(crate::crypto::encrypt(&state.cfg.secret_key, &access)?)
    .bind(crate::crypto::encrypt(&state.cfg.secret_key, &secret)?)
    .bind(key_hint(&access))
    .bind(f.format.clone().unwrap_or_else(|| "jsonl".into()))
    .bind(f.datasets.clone().unwrap_or_else(datasets::all_names))
    .bind(&schedule)
    .bind(enabled)
    .bind(run::BASELINE_DELAY_SECS as f64)
    .execute(&state.db)
    .await?;
    crate::features::observe::changed(&state.db).await;
    crate::features::team::audit(state, ctx, "export_destination.create", Some(&id.to_string()), json!({"bucket": bucket_name, "host": host_of(&b.origin())})).await;
    dest_json(state, &load(state, account_id, id).await?).await
}

/// Applies only to the revision the editor read (`revision` in the body, or the one read here): an
/// edit made against an older revision is refused, so it can't put an old bucket back or attach
/// keys meant for one place to another.
async fn update(state: &AppState, ctx: &Ctx, id: Uuid, body: &Value) -> AppResult<Value> {
    let account_id = ctx.account_id;
    let d = load(state, account_id, id).await?;
    let f = parse_fields(body)?;
    let read = match body.get("revision") {
        None | Some(Value::Null) => d.revision,
        Some(v) => v.as_i64().ok_or_else(|| AppError::bad("revision must be a whole number"))? as i32,
    };
    let stale = || AppError::Conflict("this destination changed since you opened it; reload it and try again".into());
    if read != d.revision {
        return Err(stale());
    }
    // Where requests would go, and where files would land, after this change.
    let mut b = run::bucket(state, &d).map_err(AppError::Internal)?;
    let old_origin = b.origin();
    b.endpoint = f.endpoint.clone().unwrap_or(d.endpoint.clone());
    b.region = f.region.clone().unwrap_or(d.region.clone());
    b.bucket = f.bucket.clone().unwrap_or(d.bucket.clone());
    b.path_style = f.path_style.unwrap_or(d.path_style);
    let prefix = f.prefix.clone().unwrap_or(d.prefix.clone());
    let format = f.format.clone().unwrap_or(d.format.clone());
    let new_origin = b.origin();
    if new_origin != old_origin && (f.access_key_id.is_none() || f.secret_access_key.is_none()) {
        return Err(AppError::bad(format!(
            "this change sends requests to {} instead of {}; give access_key_id and secret_access_key for it (saved keys are only ever sent to the host they were saved for)",
            host_of(&new_origin),
            host_of(&old_origin)
        )));
    }
    if let Some(a) = &f.access_key_id {
        b.access_key = a.clone();
    }
    if let Some(s) = &f.secret_access_key {
        b.secret_key = s.clone();
    }
    // A new revision: files would go somewhere else, or in another format.
    let moved = b.endpoint != d.endpoint || b.region != d.region || b.bucket != d.bucket || b.path_style != d.path_style || !key_prefix_eq(&prefix, &d.prefix) || format != d.format;
    // Turned back on: changes weren't logged while it was off, so it starts over too.
    let reenabled = f.enabled == Some(true) && !d.enabled;
    if f.touches_storage() {
        test_write(&b, &prefix).await.map_err(|e| AppError::bad(format!("DumbMail couldn't write to that bucket: {e}")))?;
    }
    let enc = |v: &Option<String>| -> AppResult<Option<String>> { v.as_ref().map(|s| crate::crypto::encrypt(&state.cfg.secret_key, s)).transpose().map_err(AppError::Internal) };
    let schedule_changed = f.schedule.as_ref().is_some_and(|s| *s != d.schedule) || reenabled;
    let mut tx = state.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE warehouse_destinations SET name = COALESCE($3, name), endpoint = CASE WHEN $4 THEN $5 ELSE endpoint END, region = COALESCE($6, region),
           bucket = COALESCE($7, bucket), prefix = COALESCE($8, prefix), path_style = COALESCE($9, path_style),
           access_key_enc = COALESCE($10, access_key_enc), secret_key_enc = COALESCE($11, secret_key_enc), access_key_hint = COALESCE($12, access_key_hint),
           format = COALESCE($13, format), datasets = COALESCE($14, datasets), schedule = COALESCE($15, schedule), enabled = COALESCE($16, enabled),
           next_run_at = CASE WHEN $17 THEN (CASE WHEN COALESCE($15, schedule) = 'manual' THEN NULL ELSE now() END) ELSE next_run_at END,
           last_test_at = CASE WHEN $18 THEN now() ELSE last_test_at END, last_test_error = CASE WHEN $18 THEN NULL ELSE last_test_error END,
           revision = revision + CASE WHEN $19 THEN 1 ELSE 0 END,
           baseline_after = CASE WHEN $19 OR $20 THEN now() + make_interval(secs => $21) ELSE baseline_after END,
           updated_at = now()
         WHERE id = $1 AND account_id = $2 AND revision = $22",
    )
    .bind(id)
    .bind(account_id)
    .bind(&f.name)
    .bind(f.endpoint.is_some())
    .bind(f.endpoint.clone().flatten())
    .bind(&f.region)
    .bind(&f.bucket)
    .bind(&f.prefix)
    .bind(f.path_style)
    .bind(enc(&f.access_key_id)?)
    .bind(enc(&f.secret_access_key)?)
    .bind(f.access_key_id.as_deref().map(key_hint))
    .bind(&f.format)
    .bind(&f.datasets)
    .bind(&f.schedule)
    .bind(f.enabled)
    .bind(schedule_changed)
    .bind(f.touches_storage())
    .bind(moved)
    .bind(reenabled)
    .bind(run::BASELINE_DELAY_SECS as f64)
    .bind(read)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(stale());
    }
    if let Some(keep) = &f.datasets {
        // A dataset taken off the destination isn't logged while it's off: added back, it starts
        // from a fresh baseline.
        sqlx::query("DELETE FROM warehouse_watermarks WHERE destination_id = $1 AND NOT (dataset = ANY($2))").bind(id).bind(keep).execute(&mut *tx).await?;
    }
    if moved || reenabled {
        // Nothing queued for the old place goes to the new one, and the next run is a fresh baseline.
        let why = if moved { "destination_changed" } else { "destination_reenabled" };
        let active: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM warehouse_runs WHERE destination_id = $1 AND status IN ('queued', 'running')").bind(id).fetch_all(&mut *tx).await?;
        for r in active {
            run::cancel(&mut *tx, r, why).await?;
        }
        sqlx::query("DELETE FROM warehouse_watermarks WHERE destination_id = $1").bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    crate::features::observe::changed(&state.db).await;
    let fields: Vec<&str> = body.as_object().map(|o| o.keys().map(String::as_str).filter(|k| *k != "revision").collect()).unwrap_or_default();
    let mut details = json!({"fields": fields, "moved": moved});
    if moved {
        details["bucket"] = json!(b.bucket);
        details["host"] = json!(host_of(&new_origin));
    }
    crate::features::team::audit(state, ctx, "export_destination.update", Some(&id.to_string()), details).await;
    dest_json(state, &load(state, account_id, id).await?).await
}

fn key_prefix_eq(a: &str, b: &str) -> bool {
    run::key_prefix(a) == run::key_prefix(b)
}

fn host_of(origin: &str) -> String {
    url::Url::parse(origin).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_else(|| origin.to_string())
}

async fn delete(state: &AppState, ctx: &Ctx, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM warehouse_destinations WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("export destination"));
    }
    crate::features::observe::changed(&state.db).await;
    crate::features::team::audit(state, ctx, "export_destination.delete", Some(&id.to_string()), json!({})).await;
    Ok(())
}

async fn test(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let d = load(state, account_id, id).await?;
    let b = run::bucket(state, &d).map_err(AppError::Internal)?;
    let res = test_write(&b, &d.prefix).await;
    sqlx::query("UPDATE warehouse_destinations SET last_test_at = now(), last_test_error = $2 WHERE id = $1").bind(id).bind(res.as_ref().err()).execute(&state.db).await?;
    Ok(match res {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e}),
    })
}

async fn start(state: &AppState, ctx: &Ctx, id: Uuid, kind: &str) -> AppResult<Value> {
    if !matches!(kind, "incremental" | "full") {
        return Err(AppError::bad("kind must be incremental or full"));
    }
    let d = load(state, ctx.account_id, id).await?;
    let run_id = run::start(state, &d, kind).await?;
    crate::features::team::audit(state, ctx, if kind == "full" { "export.full" } else { "export.start" }, Some(&run_id.to_string()), json!({"destination_id": id, "datasets": d.datasets})).await;
    get_run(state, ctx.account_id, run_id).await
}

async fn runs(state: &AppState, account_id: Uuid, id: Uuid, limit: i64) -> AppResult<Vec<Value>> {
    load(state, account_id, id).await?;
    let rows: Vec<RunRow> = sqlx::query_as(&format!("SELECT {RUN_COLS} FROM warehouse_runs WHERE destination_id = $1 AND account_id = $2 ORDER BY created_at DESC LIMIT $3"))
        .bind(id)
        .bind(account_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(&state.db)
        .await?;
    Ok(rows.iter().map(run_json).collect())
}

async fn get_run(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let r: Option<RunRow> = sqlx::query_as(&format!("SELECT {RUN_COLS} FROM warehouse_runs WHERE id = $1 AND account_id = $2")).bind(id).bind(account_id).fetch_optional(&state.db).await?;
    r.as_ref().map(run_json).ok_or_else(|| AppError::not_found("export run"))
}

fn datasets_json() -> Value {
    json!(datasets::DATASETS.iter().map(|d| json!({"name": d.name, "mode": if d.incremental { "incremental" } else { "snapshot" }, "description": d.description, "key": d.key, "columns": d.columns})).collect::<Vec<_>>())
}

// ---------- Handlers ----------

async fn datasets_h(_ctx: Ctx) -> Json<Value> {
    Json(json!({"data": datasets_json()}))
}

async fn list_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let rows = list(&state, ctx.account_id).await?;
    let n = rows.len();
    Ok(Json(json!({"data": rows, "total": n})))
}

async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<Value>) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::CREATED, Json(create(&state, &ctx, &b).await?)))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(dest_json(&state, &load(&state, ctx.account_id, id).await?).await?))
}

async fn patch_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<Value>) -> AppResult<Json<Value>> {
    Ok(Json(update(&state, &ctx, id, &b).await?))
}

async fn delete_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    delete(&state, &ctx, id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn test_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(test(&state, ctx.account_id, id).await?))
}

#[derive(Deserialize)]
struct StartBody {
    kind: String,
}

async fn start_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<StartBody>) -> AppResult<(StatusCode, Json<Value>)> {
    Ok((StatusCode::ACCEPTED, Json(start(&state, &ctx, id, &b.kind).await?)))
}

#[derive(Deserialize)]
struct RunsQuery {
    limit: Option<i64>,
}

async fn runs_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, Query(q): Query<RunsQuery>) -> AppResult<Json<Value>> {
    let rows = runs(&state, ctx.account_id, id, q.limit.unwrap_or(20)).await?;
    let n = rows.len();
    Ok(Json(json!({"data": rows, "total": n})))
}

async fn run_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(get_run(&state, ctx.account_id, id).await?))
}

// ---------- MCP ----------

fn dest_props() -> Value {
    json!({
        "name": {"type": "string", "maxLength": 100},
        "endpoint": {"type": ["string", "null"], "description": "S3-compatible endpoint URL. Leave out for AWS S3. Cloudflare R2: https://<account id>.r2.cloudflarestorage.com; Google Cloud Storage: https://storage.googleapis.com (HMAC keys); Backblaze B2: https://s3.<region>.backblazeb2.com."},
        "region": {"type": "string", "description": "e.g. us-east-1; auto for R2 and GCS."},
        "bucket": {"type": "string"},
        "prefix": {"type": "string", "description": "Folder inside the bucket, e.g. dumbmail/."},
        "path_style": {"type": "boolean", "description": "Put the bucket in the path instead of the host name. Default: true with a custom endpoint, false for AWS."},
        "access_key_id": {"type": "string"},
        "secret_access_key": {"type": "string", "description": "Stored encrypted and never shown again."},
        "format": {"enum": ["jsonl", "csv"], "description": "gzip'd JSON lines (default) or gzip'd CSV."},
        "datasets": {"type": "array", "items": {"enum": datasets::all_names()}, "description": "Default: all of them."},
        "schedule": {"enum": ["hourly", "daily", "manual"], "description": "hourly, daily (1am account time, the default) or manual."},
        "enabled": {"type": "boolean"}
    })
}

fn tool_defs() -> Vec<Value> {
    let mut update = dest_props();
    update["id"] = json!({"type": "string"});
    update["revision"] = json!({"type": "integer", "description": "The destination's revision as you read it. If where files go changed since, the update is refused."});
    vec![
        tool("list_export_datasets", "List export datasets", "The datasets a data export can include, whether each is incremental or a snapshot, and their columns.", obj(json!({}), &[]), true, false),
        tool("list_export_destinations", "List export destinations", "The account's data export destinations (buckets), schedules and their latest run. Keys are never shown, only a hint.", obj(json!({}), &[]), true, false),
        tool("create_export_destination", "Add an export destination", "Add an S3-compatible bucket (AWS S3, Cloudflare R2, Google Cloud Storage, Backblaze B2) that DumbMail exports data to on a schedule. It writes and deletes a small test file first and refuses the destination if that fails. Read dumbmail://docs/data-export for the file layout and warehouse SQL.",
            obj(dest_props(), &["name", "region", "bucket", "access_key_id", "secret_access_key"]), false, false),
        tool("update_export_destination", "Update an export destination", "Change a destination's bucket, keys, format, datasets or schedule, or turn it on or off. Storage changes are checked with a test write first.", obj(update, &["id"]), false, false),
        tool("delete_export_destination", "Delete an export destination", "Stop exporting to a destination and forget its keys and run history (files already in the bucket stay). Without confirm=true it only describes what would happen.",
            obj(json!({"id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
        tool("test_export_destination", "Test an export destination", "Write and delete a small test file in the destination's bucket and report the result.", obj(json!({"id": {"type": "string"}}), &["id"]), false, false),
        tool("start_warehouse_export", "Start a data export now", "Start an export run now. incremental exports what changed since the last run; full exports every row of every dataset (can be large). A full export requires confirm=true.",
            obj(json!({"id": {"type": "string"}, "kind": {"enum": ["incremental", "full"]}, "confirm": {"type": "boolean"}}), &["id", "kind"]), false, false),
        tool("list_export_runs", "List export runs", "A destination's recent runs: status, rows and files per dataset, errors and the manifest's key. Pass run_id for one run.",
            obj(json!({"id": {"type": "string"}, "run_id": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 100}}), &[]), true, false),
    ]
}

fn id_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("{k} must be an id")))
}

async fn call_tool(state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
    let acct = ctx.account_id;
    let r = match name {
        "list_export_datasets" => Ok(json!({"datasets": datasets_json()})),
        "list_export_destinations" => list(state, acct).await.map(|v| json!({"destinations": v})),
        "create_export_destination" => create(state, ctx, a).await,
        "update_export_destination" => async {
            let id = id_arg(a, "id")?;
            let mut body: Map<String, Value> = a.as_object().cloned().unwrap_or_default();
            body.remove("id");
            update(state, ctx, id, &Value::Object(body)).await
        }
        .await,
        "delete_export_destination" => async {
            let id = id_arg(a, "id")?;
            let d = load(state, acct, id).await?;
            if a["confirm"].as_bool() != Some(true) {
                return Ok(json!({"dry_run": true, "destination": dest_json(state, &d).await?, "note": "Nothing deleted. This stops exports to this bucket and forgets its keys and run history; files already written stay in the bucket. Call again with confirm=true once the user approves."}));
            }
            delete(state, ctx, id).await?;
            Ok(json!({"deleted": true}))
        }
        .await,
        "test_export_destination" => match id_arg(a, "id") {
            Ok(id) => test(state, acct, id).await,
            Err(e) => Err(e),
        },
        "start_warehouse_export" => async {
            let id = id_arg(a, "id")?;
            let kind = a["kind"].as_str().unwrap_or("");
            if kind == "full" && a["confirm"].as_bool() != Some(true) {
                let d = load(state, acct, id).await?;
                return Ok(json!({"dry_run": true, "note": format!("Nothing started. A full export writes every row of {} to bucket {}; it can take a while and add storage costs. Call again with confirm=true once the user approves.", d.datasets.join(", "), d.bucket)}));
            }
            start(state, ctx, id, kind).await
        }
        .await,
        "list_export_runs" => async {
            if a.get("run_id").is_some_and(|v| !v.is_null()) {
                return get_run(state, acct, id_arg(a, "run_id")?).await;
            }
            let id = id_arg(a, "id")?;
            Ok(json!({"runs": runs(state, acct, id, a["limit"].as_i64().unwrap_or(20)).await?}))
        }
        .await,
        _ => return None,
    };
    Some(r)
}
