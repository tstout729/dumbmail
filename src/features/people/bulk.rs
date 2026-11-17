//! Bulk actions on profiles: add to or remove from a list, set or clear a property, suppress, delete
//! or export. Starting one saves the exact set of profiles counted, then a background job works
//! through them in batches. One bulk action per account at a time. There is no bulk subscribe.

use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use super::consent::{self, Channel, ConsentSource};
use super::suppression::{self, Address};
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, normalize_phone};
use crate::segments;
use crate::state::AppState;

pub const JOB: &str = "people_bulk";
pub const MAX_PROFILES: i64 = 250_000;
const BATCH: i64 = 500;
const MAX_IDS: usize = 10_000;

pub const ACTIONS: &[&str] = &["add_to_list", "remove_from_list", "set_property", "clear_property", "suppress", "delete", "export"];
/// Actions that need the count typed to confirm in the dashboard.
pub const DESTRUCTIVE: &[&str] = &["remove_from_list", "clear_property", "suppress", "delete"];

/// Which profiles: the ones picked, or everyone matching a search, list or segment.
#[derive(Debug, Deserialize, Default, Clone, serde::Serialize)]
pub struct Selection {
    #[serde(default)]
    pub profile_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub list_id: Option<Uuid>,
    #[serde(default)]
    pub segment_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct BulkBody {
    pub action: String,
    #[serde(default)]
    pub selection: Selection,
    #[serde(default)]
    pub list_id: Option<Uuid>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
    /// For suppress: "email", "sms" or both (default email).
    #[serde(default)]
    pub channels: Option<Vec<String>>,
    #[serde(default)]
    pub confirm: bool,
    /// The count shown before starting. Required with confirm; if the matching set changed since,
    /// nothing runs.
    #[serde(default)]
    pub expected_count: Option<i64>,
}

/// Adds the WHERE clause for a selection (the same filters as the Profiles page).
fn push_selection(qb: &mut QueryBuilder<'_, sqlx::Postgres>, account_id: Uuid, sel: &Selection, seg: Option<&segments::Definition>) {
    qb.push(" FROM profiles p WHERE p.account_id = ").push_bind(account_id);
    if let Some(ids) = &sel.profile_ids {
        qb.push(" AND p.id = ANY(").push_bind(ids.clone()).push(")");
    }
    if let Some(s) = sel.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let like = format!("%{}%", s.replace('%', "\\%").replace('_', "\\_"));
        qb.push(" AND (p.email ILIKE ")
            .push_bind(like.clone())
            .push(" OR p.phone_number ILIKE ")
            .push_bind(like.clone())
            .push(" OR (coalesce(p.first_name,'') || ' ' || coalesce(p.last_name,'')) ILIKE ")
            .push_bind(like)
            .push(" OR p.external_id = ")
            .push_bind(s.to_string())
            .push(")");
    }
    if let Some(l) = sel.list_id {
        qb.push(" AND EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ").push_bind(l).push(" AND lm.profile_id = p.id)");
    }
    if let Some(def) = seg {
        qb.push(" AND ");
        segments::push_definition(qb, def);
    }
}

async fn segment_def(db: &PgPool, account_id: Uuid, sel: &Selection) -> AppResult<Option<segments::Definition>> {
    let Some(id) = sel.segment_id else { return Ok(None) };
    let d: Value = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::not_found("segment"))?;
    Ok(Some(segments::load(db, account_id, &d, Some(id)).await?))
}

/// How many profiles a selection matches right now.
pub async fn count(db: &PgPool, account_id: Uuid, sel: &Selection) -> AppResult<i64> {
    let seg = segment_def(db, account_id, sel).await?;
    let mut qb = QueryBuilder::new("SELECT count(*)");
    push_selection(&mut qb, account_id, sel, seg.as_ref());
    Ok(qb.build_query_scalar().fetch_one(db).await?)
}

fn validate(db_lists: bool, b: &BulkBody) -> AppResult<Value> {
    if !ACTIONS.contains(&b.action.as_str()) {
        let extra = if b.action == "subscribe" { " (there's no bulk subscribe: people subscribe themselves)" } else { "" };
        return Err(AppError::bad(format!("action must be one of {}{extra}", ACTIONS.join(", "))));
    }
    if let Some(ids) = &b.selection.profile_ids {
        if ids.is_empty() || ids.len() > MAX_IDS {
            return Err(AppError::bad(format!("pick 1 to {MAX_IDS} profiles, or select everyone matching a search, list or segment")));
        }
    }
    let mut params = json!({});
    match b.action.as_str() {
        "add_to_list" | "remove_from_list" => {
            let l = b.list_id.ok_or_else(|| AppError::bad("list_id is required"))?;
            if !db_lists {
                return Err(AppError::not_found("list"));
            }
            params["list_id"] = json!(l);
        }
        "set_property" | "clear_property" => {
            let k = b.key.as_deref().map(str::trim).filter(|k| !k.is_empty() && k.len() <= 128).ok_or_else(|| AppError::bad("key is required (up to 128 characters)"))?;
            if k.starts_with('$') {
                return Err(AppError::bad("property keys can't start with $"));
            }
            params["key"] = json!(k);
            if b.action == "set_property" {
                let v = b.value.clone().ok_or_else(|| AppError::bad("value is required"))?;
                if v.to_string().len() > 10_000 {
                    return Err(AppError::bad("value is too large (10 KB at most)"));
                }
                params["value"] = v;
            }
        }
        "suppress" => {
            let chans = b.channels.clone().unwrap_or_else(|| vec!["email".into()]);
            if chans.is_empty() {
                return Err(AppError::bad("channels must list email, sms or both"));
            }
            for c in &chans {
                Channel::parse(c)?;
            }
            params["channels"] = json!(chans);
        }
        _ => {}
    }
    Ok(params)
}

fn describe(action: &str, n: i64) -> String {
    let who = if n == 1 { "1 profile".to_string() } else { format!("{n} profiles") };
    match action {
        "add_to_list" => format!("Add {who} to the list. List flows start for people who are new to it."),
        "remove_from_list" => format!("Remove {who} from the list."),
        "set_property" => format!("Set the property on {who}."),
        "clear_property" => format!("Remove the property from {who}."),
        "suppress" => format!("Suppress {who}. They get no marketing on that channel until someone lifts it from the suppression list."),
        "delete" => format!("Delete {who} with their events, messages and consent history. This can't be undone; people who had unsubscribed stay suppressed if they come back."),
        _ => format!("Export {who} as a CSV file."),
    }
}

/// Start a bulk action. Without `confirm` (or `expected_count`), it returns the count and changes
/// nothing. With them, the matching profiles are saved and a job works through them.
pub async fn start(state: &AppState, account_id: Uuid, b: &BulkBody, src: &ConsentSource) -> AppResult<Value> {
    let list_ok = match b.list_id {
        Some(l) => sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM lists WHERE id = $1 AND account_id = $2)").bind(l).bind(account_id).fetch_one(&state.db).await?,
        None => true,
    };
    let params = validate(list_ok, b)?;
    let n = count(&state.db, account_id, &b.selection).await?;
    if !b.confirm || b.expected_count.is_none() {
        return Ok(json!({"dry_run": true, "count": n, "action": b.action, "destructive": DESTRUCTIVE.contains(&b.action.as_str()), "note": format!("Nothing changed yet. {} Call again with confirm=true and expected_count={n} once the user approves.", describe(&b.action, n))}));
    }
    if n > MAX_PROFILES {
        return Err(AppError::bad(format!("a bulk action can cover at most {MAX_PROFILES} profiles; narrow the selection")));
    }
    let expected = b.expected_count.unwrap_or(-1);
    let seg = segment_def(&state.db, account_id, &b.selection).await?;
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query("INSERT INTO bulk_actions (id, account_id, action, params, selection, created_by, via) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(id)
        .bind(account_id)
        .bind(&b.action)
        .bind(&params)
        .bind(serde_json::to_value(&b.selection).map_err(|e| AppError::Internal(e.into()))?)
        .bind(src.actor_user_id)
        .bind(src.via)
        .execute(&mut *tx)
        .await;
    if let Err(sqlx::Error::Database(e)) = &inserted {
        if e.code().as_deref() == Some("23505") {
            return Err(AppError::Conflict("Another bulk action is still running in this account. Wait for it to finish, then try again.".into()));
        }
    }
    inserted?;
    let mut qb = QueryBuilder::new("INSERT INTO bulk_action_targets (bulk_id, seq, profile_id) SELECT ");
    qb.push_bind(id).push(", (row_number() OVER (ORDER BY p.id))::int, p.id");
    push_selection(&mut qb, account_id, &b.selection, seg.as_ref());
    let saved = qb.build().execute(&mut *tx).await?.rows_affected() as i64;
    if saved != expected {
        return Err(AppError::Conflict(format!("The number of matching profiles changed from {expected} to {saved}. Check the new count and confirm again.")));
    }
    let mut file_id = None;
    if b.action == "export" {
        let fid = Uuid::now_v7();
        sqlx::query("INSERT INTO people_files (id, account_id, kind, filename, content_type, bytes, expires_at) VALUES ($1, $2, 'bulk_export', $3, 'text/csv; charset=utf-8', $4, now() + make_interval(days => $5))")
            .bind(fid)
            .bind(account_id)
            .bind(format!("profiles-{}.csv", chrono::Utc::now().format("%Y-%m-%d")))
            .bind(EXPORT_HEADER.as_bytes())
            .bind(super::privacy::FILE_DAYS as i32)
            .execute(&mut *tx)
            .await?;
        file_id = Some(fid);
    }
    sqlx::query("UPDATE bulk_actions SET total = $2, file_id = $3 WHERE id = $1").bind(id).bind(saved as i32).bind(file_id).execute(&mut *tx).await?;
    crate::jobs::enqueue(&mut *tx, JOB, json!({"bulk_id": id}), None).await?;
    tx.commit().await?;
    get(&state.db, account_id, id).await
}

/// Suppress a long list of addresses in the background (from the suppression list page).
pub async fn start_address_job(state: &AppState, account_id: Uuid, addresses: &[String], src: &ConsentSource) -> AppResult<Value> {
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query("INSERT INTO bulk_actions (id, account_id, action, params, total, created_by, via) VALUES ($1, $2, 'suppress_addresses', '{}', $3, $4, $5)")
        .bind(id)
        .bind(account_id)
        .bind(addresses.len() as i32)
        .bind(src.actor_user_id)
        .bind(src.via)
        .execute(&mut *tx)
        .await;
    if let Err(sqlx::Error::Database(e)) = &inserted {
        if e.code().as_deref() == Some("23505") {
            return Err(AppError::Conflict("Another bulk action is still running in this account. Wait for it to finish, then try again.".into()));
        }
    }
    inserted?;
    for (i, chunk) in addresses.chunks(5000).enumerate() {
        let seqs: Vec<i32> = (0..chunk.len()).map(|j| (i * 5000 + j + 1) as i32).collect();
        sqlx::query("INSERT INTO bulk_action_targets (bulk_id, seq, address) SELECT $1, s, a FROM unnest($2::int[], $3::text[]) AS t(s, a)")
            .bind(id)
            .bind(&seqs)
            .bind(chunk)
            .execute(&mut *tx)
            .await?;
    }
    crate::jobs::enqueue(&mut *tx, JOB, json!({"bulk_id": id}), None).await?;
    tx.commit().await?;
    get(&state.db, account_id, id).await
}

const EXPORT_HEADER: &str = "id,email,phone_number,external_id,first_name,last_name,email_consent,sms_consent,email_suppressed_reason,sms_suppressed_reason,created_at,last_event_at,properties\n";

type BulkRow = (Uuid, String, Value, Value, String, i32, i32, i32, i32, Value, Option<String>, Option<Uuid>, Option<String>, String, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>);
const BULK_COLS: &str = "b.id, b.action, b.params, b.selection, b.status, b.total, b.processed, b.succeeded, b.skipped, b.result, b.error, b.file_id, u.email, b.via, b.created_at, b.finished_at";

fn bulk_json(r: BulkRow) -> Value {
    json!({
        "id": r.0, "action": r.1, "params": r.2, "selection": r.3, "status": r.4, "total": r.5, "processed": r.6, "succeeded": r.7,
        "skipped": r.8, "result": r.9, "error": r.10, "file_url": r.11.map(super::privacy::file_url), "created_by": r.12, "via": r.13,
        "created_at": r.14, "finished_at": r.15,
    })
}

pub async fn get(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let r: BulkRow = sqlx::query_as(&format!("SELECT {BULK_COLS} FROM bulk_actions b LEFT JOIN users u ON u.id = b.created_by WHERE b.id = $1 AND b.account_id = $2"))
        .bind(id)
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::not_found("bulk action"))?;
    Ok(bulk_json(r))
}

pub async fn recent(db: &PgPool, account_id: Uuid, limit: i64) -> AppResult<Vec<Value>> {
    let rows: Vec<BulkRow> = sqlx::query_as(&format!("SELECT {BULK_COLS} FROM bulk_actions b LEFT JOIN users u ON u.id = b.created_by WHERE b.account_id = $1 ORDER BY b.created_at DESC LIMIT $2"))
        .bind(account_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(bulk_json).collect())
}

/// One batch of a bulk action; queues the next batch until it's done. Failures end the action with
/// the error (so the account isn't left blocked) instead of retrying.
pub async fn run(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["bulk_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    if let Err(e) = run_batch(state, id).await {
        let msg = e.public_message();
        tracing::error!(bulk = %id, error = ?e, "bulk action failed");
        sqlx::query("UPDATE bulk_actions SET status = 'failed', error = $2, finished_at = now() WHERE id = $1 AND status IN ('queued', 'running')")
            .bind(id)
            .bind(msg)
            .execute(&state.db)
            .await?;
        sqlx::query("DELETE FROM bulk_action_targets WHERE bulk_id = $1").bind(id).execute(&state.db).await?;
    }
    Ok(())
}

async fn run_batch(state: &AppState, id: Uuid) -> AppResult<()> {
    let db = &state.db;
    let row: Option<(Uuid, String, Value, String, i32, Option<Uuid>, Option<Uuid>, String)> =
        sqlx::query_as("SELECT account_id, action, params, status, cursor_seq, file_id, created_by, via FROM bulk_actions WHERE id = $1").bind(id).fetch_optional(db).await?;
    let Some((account_id, action, params, status, cursor, file_id, created_by, via)) = row else { return Ok(()) };
    if status != "queued" && status != "running" {
        return Ok(());
    }
    sqlx::query("UPDATE bulk_actions SET status = 'running' WHERE id = $1 AND status = 'queued'").bind(id).execute(db).await?;
    let targets: Vec<(i32, Option<Uuid>, Option<String>)> =
        sqlx::query_as("SELECT seq, profile_id, address FROM bulk_action_targets WHERE bulk_id = $1 AND seq > $2 ORDER BY seq LIMIT $3").bind(id).bind(cursor).bind(BATCH).fetch_all(db).await?;
    let Some(last) = targets.last().map(|t| t.0) else {
        sqlx::query("UPDATE bulk_actions SET status = 'done', finished_at = now() WHERE id = $1").bind(id).execute(db).await?;
        sqlx::query("DELETE FROM bulk_action_targets WHERE bulk_id = $1").bind(id).execute(db).await?;
        return Ok(());
    };
    let via_static: &'static str = match via.as_str() {
        "dashboard" => "dashboard",
        "mcp" => "mcp",
        _ => "api",
    };
    let src = ConsentSource { actor_user_id: created_by, ..ConsentSource::new(if action == "suppress_addresses" { "suppression_list" } else { "bulk_action" }, via_static) }
        .with_detail("bulk_action_id", json!(id));
    let ids: Vec<Uuid> = targets.iter().filter_map(|t| t.1).collect();
    let mut done: i64 = 0;
    let mut tx = db.begin().await?;
    match action.as_str() {
        "add_to_list" => {
            let list: Uuid = params["list_id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("bulk action has no list"))?;
            crate::audience::list_belongs(db, account_id, list).await.map_err(|_| AppError::bad("The list was deleted while the bulk action ran."))?;
            done = crate::audience::add_to_list(db, account_id, list, &ids).await? as i64;
        }
        "remove_from_list" => {
            let list: Uuid = params["list_id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad("bulk action has no list"))?;
            done = sqlx::query("DELETE FROM list_members lm USING lists l WHERE lm.list_id = $1 AND l.id = lm.list_id AND l.account_id = $2 AND lm.profile_id = ANY($3)")
                .bind(list)
                .bind(account_id)
                .bind(&ids)
                .execute(db)
                .await?
                .rows_affected() as i64;
        }
        "set_property" => {
            done = sqlx::query("UPDATE profiles SET properties = properties || jsonb_build_object($3::text, $4::jsonb), updated_at = now() WHERE account_id = $1 AND id = ANY($2)")
                .bind(account_id)
                .bind(&ids)
                .bind(params["key"].as_str().unwrap_or_default())
                .bind(&params["value"])
                .execute(db)
                .await?
                .rows_affected() as i64;
        }
        "clear_property" => {
            done = sqlx::query("UPDATE profiles SET properties = properties - $3::text, updated_at = now() WHERE account_id = $1 AND id = ANY($2) AND properties ? $3::text")
                .bind(account_id)
                .bind(&ids)
                .bind(params["key"].as_str().unwrap_or_default())
                .execute(db)
                .await?
                .rows_affected() as i64;
        }
        "suppress" => {
            let chans: Vec<Channel> = params["channels"].as_array().into_iter().flatten().filter_map(|c| c.as_str()).filter_map(|c| Channel::parse(c).ok()).collect();
            for pid in &ids {
                let mut changed = false;
                for ch in &chans {
                    changed |= consent::suppress(db, account_id, *pid, *ch, "manual", false, false, &src).await?;
                }
                done += i64::from(changed);
            }
        }
        "delete" => {
            for pid in &ids {
                if super::privacy::delete_profile_fully(db, account_id, *pid).await?.is_some() {
                    done += 1;
                }
            }
        }
        "export" => {
            let rows: Vec<(Uuid, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, String, String, Option<String>, Option<String>, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>, Value)> = sqlx::query_as(
                "SELECT id, email, phone_number, external_id, first_name, last_name, email_consent, sms_consent, email_suppressed_reason, sms_suppressed_reason, created_at, last_event_at, properties
                 FROM profiles WHERE account_id = $1 AND id = ANY($2) ORDER BY id",
            )
            .bind(account_id)
            .bind(&ids)
            .fetch_all(db)
            .await?;
            let mut chunk = String::new();
            for r in &rows {
                let o = |s: &Option<String>| s.clone().unwrap_or_default();
                chunk.push_str(&suppression::csv_line(&[
                    &r.0.to_string(), &o(&r.1), &o(&r.2), &o(&r.3), &o(&r.4), &o(&r.5), &r.6, &r.7, &o(&r.8), &o(&r.9),
                    &r.10.to_rfc3339(), &r.11.map(|d| d.to_rfc3339()).unwrap_or_default(), &r.12.to_string(),
                ]));
            }
            done = rows.len() as i64;
            // The file and the progress move together, so a retried batch can't add rows twice.
            sqlx::query("UPDATE people_files SET bytes = bytes || $2 WHERE id = $1").bind(file_id).bind(chunk.as_bytes()).execute(&mut *tx).await?;
        }
        "suppress_addresses" => {
            for t in &targets {
                let Some(a) = t.2.as_deref() else { continue };
                let addr = if a.contains('@') { normalize_email(a).map(Address::Email) } else { normalize_phone(a).map(Address::Phone) };
                if let Some(addr) = addr {
                    if suppression::suppress_address(state, account_id, &addr, &src).await? {
                        done += 1;
                    }
                }
            }
        }
        other => return Err(AppError::bad(format!("unknown bulk action {other}"))),
    }
    let n = targets.len() as i64;
    sqlx::query("UPDATE bulk_actions SET cursor_seq = $2, processed = processed + $3, succeeded = succeeded + $4, skipped = skipped + $5 WHERE id = $1")
        .bind(id)
        .bind(last)
        .bind(n as i32)
        .bind(done as i32)
        .bind((n - done).max(0) as i32)
        .execute(&mut *tx)
        .await?;
    crate::jobs::enqueue(&mut *tx, JOB, json!({"bulk_id": id}), None).await?;
    tx.commit().await?;
    Ok(())
}

/// Bulk actions whose job vanished (a crash, a dead job) are marked failed so the account can start another.
pub async fn fail_stuck(db: &PgPool) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE bulk_actions b SET status = 'failed', error = 'Stopped unexpectedly. Start it again to finish the rest.', finished_at = now()
         WHERE b.status IN ('queued', 'running') AND b.created_at < now() - interval '10 minutes'
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = $1 AND j.payload->>'bulk_id' = b.id::text AND j.run_at <> 'infinity')",
    )
    .bind(JOB)
    .execute(db)
    .await?;
    Ok(())
}
