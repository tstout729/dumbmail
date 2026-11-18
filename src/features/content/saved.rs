//! Saved blocks: a named group of email blocks an account reuses across emails.
//!
//! An email holds `{"type": "saved", "saved_block_id": "...", "name": "...", "blocks": [copy]}`.
//! When it renders, the saved block's current content is used (see `prepare`), so editing a saved
//! block changes every email that uses it and hasn't been sent; a campaign copies the content in
//! when its send starts. Detaching is replacing the reference with its `blocks`. Deleting hides the
//! saved block; emails that still point at it keep showing its last content.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::FromRow;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const MAX_PER_ACCOUNT: i64 = 200;
pub const MAX_BLOCKS: usize = 50;
pub const MAX_BYTES: usize = 200 * 1024;

#[derive(Debug, Serialize, FromRow)]
pub struct SavedBlock {
    pub id: Uuid,
    pub name: String,
    pub blocks: Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: Option<String>,
    pub blocks: Option<Value>,
}

fn check_name(raw: &str) -> AppResult<String> {
    let n = raw.trim();
    if n.is_empty() {
        return Err(AppError::bad("give the saved block a name"));
    }
    if n.chars().count() > 100 {
        return Err(AppError::bad("saved block names can be at most 100 characters"));
    }
    Ok(n.to_string())
}

/// A saved block's content: 1-50 blocks, each an object with a type, none of them a saved block.
pub fn check_blocks(v: &Value) -> AppResult<Value> {
    let list = v.as_array().ok_or_else(|| AppError::bad("blocks must be a list of email blocks"))?;
    if list.is_empty() || list.len() > MAX_BLOCKS {
        return Err(AppError::bad(format!("a saved block holds 1 to {MAX_BLOCKS} blocks")));
    }
    if serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX) > MAX_BYTES {
        return Err(AppError::bad("that saved block is too big (200 KB at most)"));
    }
    fn walk(list: &[Value], nested: bool) -> AppResult<()> {
        for b in list {
            let t = b.get("type").and_then(|t| t.as_str()).ok_or_else(|| AppError::bad("every block needs a type"))?;
            match t {
                "saved" => return Err(AppError::bad("a saved block can't contain another saved block; detach it first")),
                "if_open" | "if_close" => return Err(AppError::bad(format!("{t} isn't a block type"))),
                "columns" if nested => return Err(AppError::bad("columns can't go inside columns")),
                "columns" => {
                    for col in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten() {
                        walk(col.as_array().map(|a| a.as_slice()).unwrap_or(&[]), true)?;
                    }
                }
                _ => {}
            }
            if let Some(r) = b.get("show_if").filter(|r| !r.is_null()) {
                super::prepare::parse_rules(r).map_err(AppError::bad)?;
            }
        }
        Ok(())
    }
    walk(list, false)?;
    // Coupon tags follow the same rules as in an email (see features::coupons).
    crate::features::coupons::template::check_content("email", &serde_json::json!({"subject": "", "blocks": v}))?;
    Ok(v.clone())
}

pub async fn list(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<SavedBlock> = sqlx::query_as("SELECT id, name, blocks, created_at, updated_at FROM saved_blocks WHERE account_id = $1 AND deleted_at IS NULL ORDER BY updated_at DESC LIMIT 500")
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    let counts = use_counts(state, account_id).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_default();
            v["uses"] = json!(counts.iter().find(|(id, _)| *id == r.id.to_string()).map(|(_, n)| *n).unwrap_or(0));
            v
        })
        .collect();
    let total = data.len();
    Ok(json!({"data": data, "total": total}))
}

// Strict mode: lax mode's `.**` visits list items twice and would count them twice.
const USES_PATH: &str = r#"strict $.** ? (@.type == "saved" && exists(@.saved_block_id)).saved_block_id"#;

/// How many places in unsent emails point at each saved block: templates, draft or scheduled
/// campaigns (and their A/B versions) and flow emails.
async fn use_counts(state: &AppState, account_id: Uuid) -> AppResult<Vec<(String, i64)>> {
    Ok(sqlx::query_as(
        "SELECT id, count(*) FROM (
           SELECT jsonb_path_query(content, $2::jsonpath) #>> '{}' AS id FROM templates WHERE account_id = $1
           UNION ALL SELECT jsonb_path_query(content, $2::jsonpath) #>> '{}' FROM campaigns WHERE account_id = $1 AND status IN ('draft', 'scheduled')
           UNION ALL SELECT jsonb_path_query(COALESCE(ab_test, 'null'), $2::jsonpath) #>> '{}' FROM campaigns WHERE account_id = $1 AND status IN ('draft', 'scheduled')
           UNION ALL SELECT jsonb_path_query(definition, $2::jsonpath) #>> '{}' FROM flows WHERE account_id = $1
         ) q GROUP BY id",
    )
    .bind(account_id)
    .bind(USES_PATH)
    .fetch_all(&state.db)
    .await?)
}

/// Every email that points at saved block `id`, and whether it will pick up edits.
pub async fn usage(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let path = r#"$.** ? (@.type == "saved" && @.saved_block_id == $id)"#;
    let rows: Vec<(String, Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT 'template', id, name, NULL FROM templates WHERE account_id = $1 AND jsonb_path_exists(content, $2::jsonpath, jsonb_build_object('id', $3::text))
         UNION ALL SELECT 'campaign', id, name, status FROM campaigns WHERE account_id = $1
           AND (jsonb_path_exists(content, $2::jsonpath, jsonb_build_object('id', $3::text)) OR jsonb_path_exists(COALESCE(ab_test, 'null'), $2::jsonpath, jsonb_build_object('id', $3::text)))
         UNION ALL SELECT 'flow', id, name, status FROM flows WHERE account_id = $1 AND jsonb_path_exists(definition, $2::jsonpath, jsonb_build_object('id', $3::text))
         LIMIT 200",
    )
    .bind(account_id)
    .bind(path)
    .bind(id.to_string())
    .fetch_all(&state.db)
    .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(kind, id, name, status)| {
            // Campaigns that started sending keep the copy taken then.
            let updates = kind != "campaign" || matches!(status.as_deref(), Some("draft" | "scheduled"));
            json!({"kind": kind, "id": id, "name": name, "status": status, "picks_up_changes": updates})
        })
        .collect();
    Ok(json!({"data": items}))
}

pub async fn get(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    get_with(state, account_id, id, false).await
}

/// A saved block; with `include_deleted`, also one that was deleted (emails that still point at it
/// keep showing its last content, so detaching one copies that).
pub async fn get_with(state: &AppState, account_id: Uuid, id: Uuid, include_deleted: bool) -> AppResult<Value> {
    let row: Option<(Uuid, String, Value, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, bool)> = sqlx::query_as(
        "SELECT id, name, blocks, created_at, updated_at, deleted_at IS NOT NULL FROM saved_blocks WHERE id = $1 AND account_id = $2 AND (deleted_at IS NULL OR $3)",
    )
    .bind(id)
    .bind(account_id)
    .bind(include_deleted)
    .fetch_optional(&state.db)
    .await?;
    let (id, name, blocks, created_at, updated_at, deleted) = row.ok_or_else(|| AppError::not_found("saved block"))?;
    let mut v = serde_json::to_value(SavedBlock { id, name, blocks, created_at, updated_at })?;
    if deleted {
        v["deleted"] = json!(true);
    }
    v["used_in"] = usage(state, account_id, id).await?["data"].clone();
    Ok(v)
}

fn name_taken(e: AppError) -> AppError {
    match e {
        AppError::Conflict(_) => AppError::Conflict("there's already a saved block with that name".into()),
        other => other,
    }
}

pub async fn create(state: &AppState, account_id: Uuid, user_id: Option<Uuid>, b: Input) -> AppResult<Value> {
    let name = check_name(b.name.as_deref().unwrap_or(""))?;
    let blocks = check_blocks(b.blocks.as_ref().ok_or_else(|| AppError::bad("blocks is required"))?)?;
    // Count and insert under a per-account lock, so simultaneous saves can't pass the limit together.
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 49))").bind(account_id).execute(&mut *tx).await?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM saved_blocks WHERE account_id = $1 AND deleted_at IS NULL").bind(account_id).fetch_one(&mut *tx).await?;
    if n >= MAX_PER_ACCOUNT {
        return Err(AppError::Conflict(format!("an account can keep {MAX_PER_ACCOUNT} saved blocks; delete ones you no longer use first")));
    }
    let row: SavedBlock = sqlx::query_as("INSERT INTO saved_blocks (id, account_id, name, blocks, created_by) VALUES ($1, $2, $3, $4, $5) RETURNING id, name, blocks, created_at, updated_at")
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(&name)
        .bind(&blocks)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| name_taken(e.into()))?;
    tx.commit().await?;
    Ok(serde_json::to_value(row)?)
}

/// Emails that use saved block `id` and can send without anyone looking at them again (scheduled
/// or starting campaigns and their A/B versions, live flows), each checked with `blocks` standing
/// in for the saved block's content. Returns the ones that couldn't be sent then, with why.
pub async fn check_live_uses(state: &AppState, account_id: Uuid, id: Uuid, blocks: &Value) -> AppResult<Vec<String>> {
    let path = r#"$.** ? (@.type == "saved" && @.saved_block_id == $id)"#;
    let campaigns: Vec<(String, Value, Option<Value>)> = sqlx::query_as(
        "SELECT name, content, ab_test FROM campaigns WHERE account_id = $1 AND channel = 'email' AND status IN ('scheduled', 'sending')
           AND (jsonb_path_exists(content, $2::jsonpath, jsonb_build_object('id', $3::text)) OR jsonb_path_exists(COALESCE(ab_test, 'null'), $2::jsonpath, jsonb_build_object('id', $3::text)))
         LIMIT 200",
    )
    .bind(account_id)
    .bind(path)
    .bind(id.to_string())
    .fetch_all(&state.db)
    .await?;
    let flows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT name, definition FROM flows WHERE account_id = $1 AND status = 'live' AND jsonb_path_exists(definition, $2::jsonpath, jsonb_build_object('id', $3::text)) LIMIT 200",
    )
    .bind(account_id)
    .bind(path)
    .bind(id.to_string())
    .fetch_all(&state.db)
    .await?;
    if campaigns.is_empty() && flows.is_empty() {
        return Ok(Vec::new());
    }
    let mut uses: Vec<(String, Value)> = Vec::new();
    for (name, content, ab) in campaigns {
        uses.push((format!("campaign {name:?}"), content));
        for v in ab.as_ref().and_then(|t| t.get("variants")).and_then(|v| v.as_array()).into_iter().flatten() {
            uses.push((format!("campaign {name:?} version {}", v["id"].as_str().unwrap_or("").to_uppercase()), v["content"].clone()));
        }
    }
    for (name, def) in flows {
        let Ok(def) = crate::flows::parse_definition(&def) else { continue };
        for n in def.nodes.iter().filter(|n| n.kind == "email") {
            if let Some(c) = n.data.get("content") {
                uses.push((format!("flow {name:?}"), c.clone()));
            }
        }
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let mut problems = Vec::new();
    for (label, content) in uses {
        if !super::prepare::content_refs(&content).contains(&id) {
            continue;
        }
        let mut saved = super::prepare::load_saved_for(state, &account, &content).await?;
        saved.insert(id, (String::new(), blocks.clone()));
        if let Err(e) = super::prepare::check_sendable(state, &account, &content, &saved) {
            let p = format!("{label}: {e}");
            if !problems.contains(&p) {
                problems.push(p);
            }
        }
    }
    Ok(problems)
}

pub async fn update(state: &AppState, account_id: Uuid, id: Uuid, b: Input) -> AppResult<Value> {
    let name = b.name.as_deref().map(check_name).transpose()?;
    let blocks = b.blocks.as_ref().map(check_blocks).transpose()?;
    if let Some(blocks) = &blocks {
        // Emails set to send pick the change up with nobody looking again: it mustn't break any of them.
        let problems = check_live_uses(state, account_id, id, blocks).await?;
        if !problems.is_empty() {
            return Err(AppError::Conflict(format!(
                "This change would stop {} from sending: {}. Fix the saved block, or detach it in those emails first.",
                if problems.len() == 1 { "an email that's set to go out".to_string() } else { format!("{} emails that are set to go out", problems.len()) },
                problems.join("; ")
            )));
        }
    }
    let row: Option<SavedBlock> = sqlx::query_as(
        "UPDATE saved_blocks SET name = COALESCE($3, name), blocks = COALESCE($4, blocks), updated_at = now()
         WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL RETURNING id, name, blocks, created_at, updated_at",
    )
    .bind(id)
    .bind(account_id)
    .bind(name)
    .bind(blocks)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| name_taken(e.into()))?;
    let row = row.ok_or_else(|| AppError::not_found("saved block"))?;
    let mut v = serde_json::to_value(&row)?;
    v["used_in"] = usage(state, account_id, id).await?["data"].clone();
    Ok(v)
}

pub async fn delete(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("UPDATE saved_blocks SET deleted_at = now() WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL")
        .bind(id)
        .bind(account_id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("saved block"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_saved_block_content() {
        assert!(check_blocks(&json!([{"type": "text", "html": "<p>Hi</p>"}])).is_ok());
        assert!(check_blocks(&json!([])).is_err());
        assert!(check_blocks(&json!({"type": "text"})).is_err());
        assert!(check_blocks(&json!([{"html": "x"}])).is_err());
        assert!(check_blocks(&json!([{"type": "saved", "saved_block_id": Uuid::nil()}])).is_err());
        assert!(check_blocks(&json!([{"type": "columns", "columns": [[{"type": "saved"}]]}])).is_err());
        assert!(check_blocks(&json!([{"type": "text", "html": "x", "show_if": {"field": "person.x", "op": "nope"}}])).is_err());
        let many: Vec<Value> = (0..51).map(|_| json!({"type": "divider"})).collect();
        assert!(check_blocks(&Value::Array(many)).is_err());
        assert!(check_name("  ").is_err());
        assert_eq!(check_name("  Footer ").unwrap(), "Footer");
    }
}
