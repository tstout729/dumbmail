//! The import row, Klaviyo id references and the issue log.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const JOB_RUN: &str = "klaviyo_import_run";
pub const JOB_SURVEY: &str = "klaviyo_import_survey";

/// Import steps in the order they run. Profiles (with consent and suppressions) always come first;
/// campaigns and flows come before events so past emails can be tied to them.
pub const STEPS: &[&str] = &[
    "metrics", "profiles", "lists", "list_members", "segments", "segment_members", "templates", "campaigns", "campaign_stats", "flows", "forms", "events", "compare",
];
/// What a customer can choose to leave out (profiles and consent always come across).
pub const CHOOSABLE: &[&str] = &["lists", "segments", "templates", "campaigns", "flows", "forms", "events"];

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Import {
    pub id: Uuid,
    #[serde(skip)]
    pub account_id: Uuid,
    pub status: String,
    #[serde(skip)]
    pub api_key_enc: Option<String>,
    pub key_hint: String,
    pub klaviyo_account: Value,
    pub survey: Option<Value>,
    pub options: Value,
    pub phase: Option<String>,
    #[serde(skip)]
    pub cursor: Option<String>,
    #[serde(skip)]
    pub step_state: Value,
    pub progress: Value,
    #[serde(skip)]
    pub source_counts: Value,
    pub checklist: Value,
    pub error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub compared_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip)]
    pub lease_owner: Option<Uuid>,
    #[serde(skip)]
    pub lease_until: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub history_open: bool,
    #[serde(skip)]
    pub report_cache: Option<Value>,
    #[serde(skip)]
    pub report_cached_at: Option<DateTime<Utc>>,
}

impl Import {
    pub fn wants(&self, what: &str) -> bool {
        match self.options["what"].as_array() {
            Some(a) => a.iter().any(|x| x == what),
            None => true,
        }
    }

    /// Earliest event to import; None = all of it. The chosen day starts at midnight in the account's
    /// time zone (resolved when the import starts, kept as `events_since`).
    pub fn events_since(&self) -> Option<DateTime<Utc>> {
        if self.options["events_from"] == "all" {
            return None;
        }
        if let Some(t) = self.options["events_since"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()) {
            return Some(t.with_timezone(&Utc));
        }
        match self.options["events_from"].as_str() {
            Some(s) => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc()),
            None => Some(self.created_at - chrono::Duration::days(730)),
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "surveying" | "ready" | "importing" | "paused")
    }
}

pub async fn latest(db: &PgPool, account_id: Uuid) -> AppResult<Option<Import>> {
    Ok(sqlx::query_as("SELECT * FROM klaviyo_imports WHERE account_id = $1 ORDER BY created_at DESC LIMIT 1").bind(account_id).fetch_optional(db).await?)
}

pub async fn load(db: &PgPool, id: Uuid) -> anyhow::Result<Option<Import>> {
    Ok(sqlx::query_as("SELECT * FROM klaviyo_imports WHERE id = $1").bind(id).fetch_optional(db).await?)
}

pub async fn current(db: &PgPool, account_id: Uuid) -> AppResult<Import> {
    latest(db, account_id).await?.ok_or_else(|| AppError::NotFound("No Klaviyo import yet. Connect a Klaviyo API key first.".into()))
}

pub fn key(state: &AppState, imp: &Import) -> Option<String> {
    imp.api_key_enc.as_deref().and_then(|k| crate::crypto::decrypt(&state.cfg.secret_key, k).ok())
}

/// Queue the next run of an import unless one is already waiting.
pub async fn enqueue(db: &PgPool, kind: &str, import_id: Uuid, run_at: Option<DateTime<Utc>>) -> sqlx::Result<()> {
    let queued: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = $1 AND payload->>'import_id' = $2 AND run_at <> 'infinity' AND locked_at IS NULL)")
        .bind(kind)
        .bind(import_id.to_string())
        .fetch_one(db)
        .await?;
    if !queued {
        crate::jobs::enqueue(db, kind, json!({"import_id": import_id}), run_at).await?;
    }
    Ok(())
}

/// Add to a step's done/total counters.
pub async fn bump<'e>(db: impl sqlx::PgExecutor<'e>, id: Uuid, step: &str, done: i64, skipped: i64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE klaviyo_imports SET progress = jsonb_set(progress, ARRAY[$2],
            jsonb_build_object('done', COALESCE((progress->$2->>'done')::bigint, 0) + $3,
                               'skipped', COALESCE((progress->$2->>'skipped')::bigint, 0) + $4,
                               'finished', COALESCE((progress->$2->>'finished')::boolean, false)), true), updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(step)
    .bind(done)
    .bind(skipped)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn finish_step<'e>(db: impl sqlx::PgExecutor<'e>, id: Uuid, step: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE klaviyo_imports SET progress = jsonb_set(progress, ARRAY[$2],
            COALESCE(progress->$2, '{\"done\":0,\"skipped\":0}'::jsonb) || '{\"finished\":true}'::jsonb, true), updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(step)
    .execute(db)
    .await?;
    Ok(())
}

/// Most issues one import records; past this they're only counted.
const MAX_ISSUES: i64 = 5000;

/// Record an issue, in the caller's transaction (the one that wrote the item it's about).
#[allow(clippy::too_many_arguments)]
pub async fn issue(db: &mut sqlx::PgConnection, imp: &Import, step: &str, kind: &str, item: &str, klaviyo_id: Option<&str>, local_id: Option<Uuid>, message: &str) -> sqlx::Result<()> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM klaviyo_import_issues WHERE import_id = $1").bind(imp.id).fetch_one(&mut *db).await?;
    if n >= MAX_ISSUES {
        sqlx::query("UPDATE klaviyo_imports SET step_state = jsonb_set(step_state, '{issues_over_limit}', to_jsonb(COALESCE((step_state->>'issues_over_limit')::bigint, 0) + 1)) WHERE id = $1")
            .bind(imp.id)
            .execute(&mut *db)
            .await?;
        return Ok(());
    }
    let item: String = item.chars().take(300).collect();
    let message: String = message.chars().take(2000).collect();
    sqlx::query(
        "INSERT INTO klaviyo_import_issues (import_id, account_id, step, kind, item, klaviyo_id, local_id, message) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT DO NOTHING",
    )
    .bind(imp.id)
    .bind(imp.account_id)
    .bind(step)
    .bind(kind)
    .bind(item)
    .bind(klaviyo_id)
    .bind(local_id)
    .bind(message)
    .execute(&mut *db)
    .await?;
    Ok(())
}

pub async fn ref_local(db: &PgPool, account_id: Uuid, kind: &str, klaviyo_id: &str) -> sqlx::Result<Option<(Option<Uuid>, Value)>> {
    sqlx::query_as("SELECT local_id, detail FROM klaviyo_refs WHERE account_id = $1 AND kind = $2 AND klaviyo_id = $3")
        .bind(account_id)
        .bind(kind)
        .bind(klaviyo_id)
        .fetch_optional(db)
        .await
}

pub async fn set_ref<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid, kind: &str, klaviyo_id: &str, local_id: Option<Uuid>, name: &str, detail: &Value) -> sqlx::Result<()> {
    let name: String = name.chars().take(300).collect();
    sqlx::query(
        "INSERT INTO klaviyo_refs (account_id, kind, klaviyo_id, local_id, name, detail) VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (account_id, kind, klaviyo_id) DO UPDATE SET local_id = COALESCE(EXCLUDED.local_id, klaviyo_refs.local_id),
             name = EXCLUDED.name, detail = klaviyo_refs.detail || EXCLUDED.detail, updated_at = now()",
    )
    .bind(account_id)
    .bind(kind)
    .bind(klaviyo_id)
    .bind(local_id)
    .bind(name)
    .bind(detail)
    .execute(db)
    .await?;
    Ok(())
}

/// Many references of one kind in one statement (a page of profiles).
pub async fn set_refs(db: &mut sqlx::PgConnection, account_id: Uuid, kind: &str, rows: &[(String, Option<Uuid>, String, Value)]) -> sqlx::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    // A page can mention the same Klaviyo id twice; keep the last.
    let mut seen = std::collections::HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        seen.insert(r.0.as_str(), i);
    }
    let mut keep: Vec<usize> = seen.into_values().collect();
    keep.sort_unstable();
    let kid: Vec<&str> = keep.iter().map(|i| rows[*i].0.as_str()).collect();
    let local: Vec<Option<Uuid>> = keep.iter().map(|i| rows[*i].1).collect();
    let name: Vec<String> = keep.iter().map(|i| rows[*i].2.chars().take(300).collect()).collect();
    let detail: Vec<&Value> = keep.iter().map(|i| &rows[*i].3).collect();
    sqlx::query(
        "INSERT INTO klaviyo_refs (account_id, kind, klaviyo_id, local_id, name, detail)
         SELECT $1, $2, x.k, x.l, x.n, x.d FROM unnest($3::text[], $4::uuid[], $5::text[], $6::jsonb[]) AS x(k, l, n, d)
         ON CONFLICT (account_id, kind, klaviyo_id) DO UPDATE SET local_id = COALESCE(EXCLUDED.local_id, klaviyo_refs.local_id),
             name = EXCLUDED.name, detail = klaviyo_refs.detail || EXCLUDED.detail, updated_at = now()",
    )
    .bind(account_id)
    .bind(kind)
    .bind(&kid)
    .bind(&local)
    .bind(&name)
    .bind(&detail)
    .execute(db)
    .await?;
    Ok(())
}

/// Klaviyo id → DumbMail id for one kind.
pub async fn ref_map(db: &PgPool, account_id: Uuid, kind: &str) -> sqlx::Result<std::collections::HashMap<String, Uuid>> {
    let rows: Vec<(String, Uuid)> = sqlx::query_as("SELECT klaviyo_id, local_id FROM klaviyo_refs WHERE account_id = $1 AND kind = $2 AND local_id IS NOT NULL")
        .bind(account_id)
        .bind(kind)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().collect())
}

/// Stop an import for good: the key is deleted.
pub async fn close(db: &PgPool, id: Uuid, status: &str, error: Option<&str>) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE klaviyo_imports SET status = $2, error = COALESCE($3, error), api_key_enc = NULL,
            finished_at = COALESCE(finished_at, now()), lease_owner = NULL, lease_until = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    // History writing that stopped part-way ends here: segments are re-baselined and run normally again.
    super::history::close(&mut tx, id).await?;
    sqlx::query("DELETE FROM jobs WHERE kind IN ($1, $2) AND payload->>'import_id' = $3 AND locked_at IS NULL")
        .bind(JOB_RUN)
        .bind(JOB_SURVEY)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
