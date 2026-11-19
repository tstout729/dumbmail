//! The conversation fee (`PRICE_AGENT_CONVERSATION`, hosted service only): reserved when a
//! conversation is admitted (the live account open and in good standing, its balance covering this
//! fee on top of every fee still held), captured once when its first answer is delivered, and
//! released when nothing went out (a failure, a busy server, a skipped send). A released fee is
//! reserved again before the conversation's next turn.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::AppResult;
use crate::state::AppState;

/// The fee for a conversation at `place` (0: free here).
pub fn price(state: &AppState, place: &str) -> i64 {
    if !state.cfg.hosted || place == "test" {
        return 0;
    }
    crate::billing::to_micros(state.cfg.price_agent_conversation).max(0)
}

/// Hold `amount` for conversation `id` inside the caller's transaction (the conversation row may be
/// inserted later in it). False when the account is closed, not in good standing, or can't cover it.
pub async fn reserve(state: &AppState, tx: &mut PgConnection, account_id: Uuid, id: Uuid, place: &str, amount: i64) -> AppResult<bool> {
    let row: Option<(i64, bool)> = sqlx::query_as("SELECT balance_micros, closed_at IS NOT NULL FROM accounts WHERE id = $1 FOR UPDATE").bind(account_id).fetch_optional(&mut *tx).await?;
    let Some((balance, closed)) = row else { return Ok(false) };
    if closed || !super::standing_allows(state, account_id, place, super::Moment::Admission).await {
        return Ok(false);
    }
    if amount <= 0 {
        return Ok(true);
    }
    let held: i64 = sqlx::query_scalar("SELECT COALESCE(sum(fee_micros), 0)::bigint FROM agent_conversations WHERE account_id = $1 AND fee_state = 'reserved' AND id <> $2")
        .bind(account_id)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    Ok(balance - held >= amount)
}

/// Mark an existing conversation's fee held (after [`reserve`] said yes, same transaction).
pub async fn mark_reserved(tx: &mut PgConnection, id: Uuid, amount: i64) -> AppResult<()> {
    if amount > 0 {
        sqlx::query("UPDATE agent_conversations SET fee_state = 'reserved', fee_micros = $2, fee_reserved_at = now() WHERE id = $1 AND fee_state <> 'captured'")
            .bind(id)
            .bind(amount)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

/// Before a later turn: hold the fee again if it was released (never captured). False: no credit.
pub async fn ensure(state: &AppState, account_id: Uuid, id: Uuid, place: &str) -> AppResult<bool> {
    let amount = price(state, place);
    let mut tx = state.db.begin().await?;
    let st: Option<String> = sqlx::query_scalar("SELECT fee_state FROM agent_conversations WHERE id = $1 FOR UPDATE").bind(id).fetch_optional(&mut *tx).await?;
    if matches!(st.as_deref(), Some("captured" | "reserved")) {
        return Ok(true);
    }
    if !reserve(state, &mut tx, account_id, id, place, amount).await? {
        return Ok(false);
    }
    mark_reserved(&mut tx, id, amount).await?;
    tx.commit().await?;
    Ok(true)
}

/// An answer went out: charge the held fee, once.
pub async fn capture(state: &AppState, id: Uuid) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    let row: Option<(Uuid, i64)> = sqlx::query_as("UPDATE agent_conversations SET fee_state = 'captured' WHERE id = $1 AND fee_state = 'reserved' RETURNING account_id, fee_micros")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some((account_id, amount)) = row.filter(|r| r.1 > 0) {
        crate::billing::post_entry_tx(&mut tx, account_id, "agent", -amount, "Shopping agent conversation", &format!("agent:{id}")).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Nothing went out: give the held fee back.
pub async fn release(state: &AppState, id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE agent_conversations SET fee_state = 'released' WHERE id = $1 AND fee_state = 'reserved'").bind(id).execute(&state.db).await?;
    Ok(())
}

/// Upkeep: holds still open a day later (a turn or a send that died) are given back.
pub async fn release_stale(db: &sqlx::PgPool) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE agent_conversations SET fee_state = 'released' WHERE id IN (
            SELECT id FROM agent_conversations WHERE fee_state = 'reserved' AND fee_reserved_at < now() - interval '1 day' LIMIT 500)",
    )
    .execute(db)
    .await?;
    Ok(())
}
