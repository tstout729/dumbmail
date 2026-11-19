//! An agent answer by text or WhatsApp is checked again just before it goes to the provider, and
//! its conversation's fee is charged only when the provider took it.
//!
//! Before: the account is open and in good standing for automatic messages, and when the
//! conversation has order access, the proof behind it (the number they write from, still theirs
//! and the store's) still holds. After: delivered, the held fee is captured once; skipped or failed,
//! it's given back.

use uuid::Uuid;

use super::{fee, proof, standing_allows, Moment};
use crate::error::AppResult;
use crate::state::AppState;

/// Why this conversation's answer must not go out now, or None when it may.
pub async fn before_send(state: &AppState, conversation_id: Uuid) -> AppResult<Option<&'static str>> {
    let row: Option<(Uuid, String, Option<Uuid>, Option<serde_json::Value>, bool)> =
        sqlx::query_as("SELECT account_id, place, profile_id, proof, verified FROM agent_conversations WHERE id = $1").bind(conversation_id).fetch_optional(&state.db).await?;
    let Some((account_id, place, profile_id, proof_v, verified)) = row else { return Ok(Some("the conversation is gone")) };
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    if account.closed_at.is_some() {
        return Ok(Some("account closed"));
    }
    if !standing_allows(state, account_id, &place, Moment::Delivery).await {
        return Ok(Some("account standing"));
    }
    if let Some(p) = proof_v.filter(|p| verified && !p.is_null()) {
        if !proof::holds(state, &account, profile_id, &p).await? {
            return Ok(Some("the number is no longer the shopper's"));
        }
    }
    Ok(None)
}

/// The provider took the answer (`delivered`) or it didn't go out.
pub async fn after_send(state: &AppState, conversation_id: Uuid, delivered: bool) -> AppResult<()> {
    if delivered {
        fee::capture(state, conversation_id).await
    } else {
        fee::release(state, conversation_id).await
    }
}

/// The conversation an outgoing message carries an answer for, if any.
pub async fn conversation_of(db: &sqlx::PgPool, message_id: Uuid) -> AppResult<Option<Uuid>> {
    Ok(sqlx::query_scalar("SELECT conversation_id FROM agent_deliveries WHERE message_id = $1").bind(message_id).fetch_optional(db).await?)
}

/// A WhatsApp send finished (called by WhatsApp's send job for every final outcome).
pub async fn whatsapp_outcome(state: &AppState, message_id: Uuid, delivered: bool) {
    let run = async {
        if let Some(c) = conversation_of(&state.db, message_id).await? {
            after_send(state, c, delivered).await?;
        }
        Ok::<(), crate::error::AppError>(())
    };
    if let Err(e) = run.await {
        tracing::warn!(message = %message_id, error = %e.public_message(), "agent fee not settled for a WhatsApp answer");
    }
}
