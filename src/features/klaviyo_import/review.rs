//! Clearing reviews and listing what still needs one, for the import page. The checks themselves are in
//! `crate::import_review` (they run on every send path, so they're platform-neutral and cost no SQL).

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
pub use crate::import_review::{column, mirror};

pub const KINDS: &[&str] = &["campaign", "flow", "template", "form"];

/// A person has reviewed an imported item: clear it everywhere (its column and the marker in its content).
pub async fn clear(db: &PgPool, account_id: Uuid, kind: &str, local_id: Uuid) -> AppResult<()> {
    if !KINDS.contains(&kind) {
        return Err(AppError::bad("kind must be campaign, flow, template or form"));
    }
    let n = match kind {
        "campaign" => {
            sqlx::query(
                "UPDATE campaigns SET import_review = NULL, content = content #- '{imported,review}',
                    ab_test = CASE WHEN jsonb_typeof(ab_test->'variants') = 'array' THEN jsonb_set(ab_test, '{variants}',
                        (SELECT COALESCE(jsonb_agg(v #- '{content,imported,review}' ORDER BY i), '[]'::jsonb) FROM jsonb_array_elements(ab_test->'variants') WITH ORDINALITY AS x(v, i)))
                        ELSE ab_test END,
                    updated_at = now()
                 WHERE id = $1 AND account_id = $2 AND status IN ('draft', 'scheduled')",
            )
            .bind(local_id)
            .bind(account_id)
            .execute(db)
            .await?
            .rows_affected()
        }
        "template" => sqlx::query("UPDATE templates SET import_review = NULL, content = content #- '{imported,review}', updated_at = now() WHERE id = $1 AND account_id = $2").bind(local_id).bind(account_id).execute(db).await?.rows_affected(),
        "form" => sqlx::query("UPDATE forms SET import_review = NULL, updated_at = now() WHERE id = $1 AND account_id = $2").bind(local_id).bind(account_id).execute(db).await?.rows_affected(),
        _ => sqlx::query(
            "UPDATE flows SET import_review = NULL, definition = jsonb_set(definition #- '{imported,review}', '{nodes}',
                (SELECT COALESCE(jsonb_agg(n #- '{content,imported,review}' ORDER BY i), '[]'::jsonb)
                 FROM jsonb_array_elements(CASE WHEN jsonb_typeof(definition->'nodes') = 'array' THEN definition->'nodes' ELSE '[]'::jsonb END) WITH ORDINALITY AS x(n, i))),
                updated_at = now()
             WHERE id = $1 AND account_id = $2",
        )
        .bind(local_id)
        .bind(account_id)
        .execute(db)
        .await?
        .rows_affected(),
    };
    if n == 0 {
        return Err(AppError::not_found(kind));
    }
    Ok(())
}

/// Everything still waiting for review, with names, for the import page: items imported with notes, and
/// copies made from them (their content carries the marker).
pub async fn list(db: &PgPool, account_id: Uuid) -> sqlx::Result<Vec<Value>> {
    let rows: Vec<(String, Uuid, String, Value)> = sqlx::query_as(
        "SELECT 'campaign', id, name, COALESCE(import_review, content->'imported'->'review', ab_test #> '{variants,0,content,imported,review}', ab_test #> '{variants,1,content,imported,review}', '[]')
           FROM campaigns WHERE account_id = $1 AND status IN ('draft', 'scheduled')
            AND (import_review IS NOT NULL OR content->'imported' ? 'review' OR COALESCE(ab_test, '{}') @? '$.variants[*].content.imported.review')
         UNION ALL
         SELECT 'flow', id, name, COALESCE(import_review, definition->'imported'->'review', jsonb_path_query_first(definition, '$.nodes[*].content.imported.review'), '[]')
           FROM flows WHERE account_id = $1 AND (import_review IS NOT NULL OR definition->'imported' ? 'review' OR definition @? '$.nodes[*].content.imported.review')
         UNION ALL
         SELECT 'template', id, name, COALESCE(import_review, content->'imported'->'review', '[]')
           FROM templates WHERE account_id = $1 AND (import_review IS NOT NULL OR content->'imported' ? 'review')
         UNION ALL
         SELECT 'form', id, name, import_review FROM forms WHERE account_id = $1 AND import_review IS NOT NULL
         ORDER BY 1, 3 LIMIT 500",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(kind, id, name, notes)| json!({"kind": kind, "id": id, "name": name, "notes": notes})).collect())
}
