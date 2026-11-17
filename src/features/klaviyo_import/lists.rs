//! Leaving a list in DumbMail, for lists that came from Klaviyo.

use sqlx::PgPool;
use uuid::Uuid;

/// Take people off a list (the dashboard, the API and MCP, the preference page). For a list imported
/// from Klaviyo the leave is recorded first, in the same transaction, so a later import never adds them
/// back. The leave is written before the removal, so an import adding the same person at the same moment
/// either sees the leave or has its addition removed here. For any other list this is just the removal.
pub async fn remove_members(db: &PgPool, account_id: Uuid, list_id: Uuid, profile_ids: &[Uuid]) -> sqlx::Result<u64> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO klaviyo_memberships (account_id, list_id, profile_id, state, left_via)
         SELECT $1, $2, p.id, 'left', 'dumbmail' FROM profiles p
         WHERE p.account_id = $1 AND p.id = ANY($3)
           AND EXISTS (SELECT 1 FROM klaviyo_refs r WHERE r.account_id = $1 AND r.kind IN ('list', 'segment') AND r.local_id = $2)
         ON CONFLICT (list_id, profile_id) DO UPDATE SET state = 'left', left_via = 'dumbmail', updated_at = now()",
    )
    .bind(account_id)
    .bind(list_id)
    .bind(profile_ids)
    .execute(&mut *tx)
    .await?;
    let n = sqlx::query("DELETE FROM list_members WHERE list_id = $1 AND profile_id = ANY($2)").bind(list_id).bind(profile_ids).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(n)
}
