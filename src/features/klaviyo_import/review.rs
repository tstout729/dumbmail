//! Imported items that need a person's review: a campaign whose audience, exclusions or send settings
//! didn't map exactly, a flow with an ambiguous step, a template whose conversion had to guess. Until a
//! person clears it, the campaign can't send and the flow can't go live.
//!
//! The notes live in `klaviyo_reviews` (the authority, which edits can't lose) and are mirrored into the
//! content as `imported.review`, so the campaign's own readiness check can show them.

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

pub const KINDS: &[&str] = &["campaign", "flow", "template"];

/// Record review notes for an imported item (nothing when there are none).
pub async fn flag<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid, kind: &str, local_id: Uuid, notes: &[String]) -> sqlx::Result<()> {
    if notes.is_empty() {
        return Ok(());
    }
    let notes: Vec<String> = notes.iter().take(50).map(|n| n.chars().take(500).collect()).collect();
    sqlx::query(
        "INSERT INTO klaviyo_reviews (account_id, kind, local_id, notes) VALUES ($1, $2, $3, $4)
         ON CONFLICT (kind, local_id) DO UPDATE SET notes = EXCLUDED.notes, cleared_at = NULL",
    )
    .bind(account_id)
    .bind(kind)
    .bind(local_id)
    .bind(json!(notes))
    .execute(db)
    .await?;
    Ok(())
}

/// Put the notes into content (`imported.review`), or take them out when there are none.
pub fn mirror(content: &mut Value, notes: &[String]) {
    if notes.is_empty() {
        if let Some(i) = content.get_mut("imported").and_then(|i| i.as_object_mut()) {
            i.remove("review");
        }
        return;
    }
    if !content["imported"].is_object() {
        content["imported"] = json!({"from": "klaviyo"});
    }
    content["imported"]["review"] = json!(notes);
}

/// For a campaign's readiness: what stops content imported from Klaviyo from being sent.
pub fn problems(content: &Value) -> Vec<String> {
    let notes = super::templates::review_notes(content);
    match notes.len() {
        0 => Vec::new(),
        n => vec![format!(
            "Imported from Klaviyo and needs review: {}{}. Fix it, then mark it reviewed under Settings → Move from Klaviyo",
            notes[0].trim_end_matches('.'),
            if n > 1 { format!(" (and {} more)", n - 1) } else { String::new() }
        )],
    }
}

/// Uncleared notes for one item.
pub async fn pending(db: &PgPool, kind: &str, local_id: Uuid) -> sqlx::Result<Option<Vec<String>>> {
    let v: Option<Value> = sqlx::query_scalar("SELECT notes FROM klaviyo_reviews WHERE kind = $1 AND local_id = $2 AND cleared_at IS NULL").bind(kind).bind(local_id).fetch_optional(db).await?;
    Ok(v.map(|v| v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()))
}

/// Before a flow goes live: refuse while an imported flow, or an imported email in it, needs review.
pub async fn check_flow_live(db: &PgPool, flow_id: Uuid, definition: &Value) -> AppResult<()> {
    let mut notes = pending(db, "flow", flow_id).await?.unwrap_or_default();
    for n in definition["nodes"].as_array().into_iter().flatten() {
        notes.extend(super::templates::review_notes(&n["content"]));
    }
    if notes.is_empty() {
        return Ok(());
    }
    Err(AppError::bad(format!(
        "This flow was imported from Klaviyo and needs review before it can go live: {}{}. Fix it, then mark it reviewed under Settings → Move from Klaviyo.",
        notes[0].trim_end_matches('.'),
        if notes.len() > 1 { format!(" (and {} more)", notes.len() - 1) } else { String::new() }
    )))
}

/// A person has reviewed an imported item: clear it everywhere.
pub async fn clear(db: &PgPool, account_id: Uuid, kind: &str, local_id: Uuid) -> AppResult<()> {
    if !KINDS.contains(&kind) {
        return Err(AppError::bad("kind must be campaign, flow or template"));
    }
    let mut tx = db.begin().await?;
    let found = match kind {
        "campaign" => sqlx::query("UPDATE campaigns SET content = content #- '{imported,review}', updated_at = now() WHERE id = $1 AND account_id = $2").bind(local_id).bind(account_id).execute(&mut *tx).await?.rows_affected(),
        "template" => sqlx::query("UPDATE templates SET content = content #- '{imported,review}' WHERE id = $1 AND account_id = $2").bind(local_id).bind(account_id).execute(&mut *tx).await?.rows_affected(),
        _ => {
            let def: Option<Value> = sqlx::query_scalar("SELECT definition FROM flows WHERE id = $1 AND account_id = $2 FOR UPDATE").bind(local_id).bind(account_id).fetch_optional(&mut *tx).await?;
            match def {
                Some(mut d) => {
                    for n in d["nodes"].as_array_mut().into_iter().flatten() {
                        if let Some(i) = n.get_mut("content").and_then(|c| c.get_mut("imported")).and_then(|i| i.as_object_mut()) {
                            i.remove("review");
                        }
                    }
                    sqlx::query("UPDATE flows SET definition = $3, updated_at = now() WHERE id = $1 AND account_id = $2").bind(local_id).bind(account_id).bind(&d).execute(&mut *tx).await?;
                    1
                }
                None => 0,
            }
        }
    };
    if found == 0 {
        return Err(AppError::not_found(kind));
    }
    sqlx::query("UPDATE klaviyo_reviews SET cleared_at = now() WHERE kind = $1 AND local_id = $2 AND account_id = $3 AND cleared_at IS NULL").bind(kind).bind(local_id).bind(account_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Everything still waiting for review, with names, for the import page.
pub async fn list(db: &PgPool, account_id: Uuid) -> sqlx::Result<Vec<Value>> {
    let rows: Vec<(String, Uuid, Value, Option<String>)> = sqlx::query_as(
        "SELECT r.kind, r.local_id, r.notes, COALESCE(c.name, f.name, t.name) FROM klaviyo_reviews r
         LEFT JOIN campaigns c ON r.kind = 'campaign' AND c.id = r.local_id
         LEFT JOIN flows f ON r.kind = 'flow' AND f.id = r.local_id
         LEFT JOIN templates t ON r.kind = 'template' AND t.id = r.local_id
         WHERE r.account_id = $1 AND r.cleared_at IS NULL AND COALESCE(c.id, f.id, t.id) IS NOT NULL
         ORDER BY r.kind, r.created_at LIMIT 500",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(kind, id, notes, name)| json!({"kind": kind, "id": id, "name": name, "notes": notes})).collect())
}
