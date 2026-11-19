//! First answers to inbound WhatsApp messages, when the store turns them on (off by default).
//!
//! The WhatsApp inbox hands over an ordinary text message that has Meta's time (so the service
//! window is open) with [`take`]; the agent's job answers it once (a claim on the inbound message)
//! and the answer goes out through WhatsApp's own conversational path (`whatsapp::send`): its final
//! claim checks the 24-hour window, opt-outs and the sender at send time, and bills per message.
//! The text is sent as written (never rendered as a template). A person writing from the number on
//! their profile sees order status and tracking only, and only when the store gave that number.

use serde_json::{json, Value};
use uuid::Uuid;

use super::chat::{self, Orders, Start, Who};
use super::settings;
use crate::error::AppResult;
use crate::state::AppState;

pub const JOB: &str = "agent_whatsapp_answer";
const MAX_REPLY_CHARS: usize = 1000;
/// Automatic answers per person per hour (the WhatsApp inbox's own allowance).
const AUTO_PER_HOUR: i32 = 5;

/// Whether the agent answers this WhatsApp message (its job is queued).
pub async fn take(state: &AppState, account_id: Uuid, inbound_id: Uuid, msg_type: &str, timed: bool) -> AppResult<bool> {
    if msg_type != "text" || !timed {
        return Ok(false);
    }
    let s = settings::load(&state.db, account_id).await?;
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    if !chat::available(state, &account, &s, "whatsapp") {
        return Ok(false);
    }
    crate::jobs::enqueue_background(&state.db, JOB, json!({"inbound_id": inbound_id}), None).await?;
    Ok(true)
}

/// Job: answer one inbound WhatsApp message, at most once.
pub async fn run(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["inbound_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    #[allow(clippy::type_complexity)]
    let row: Option<(Uuid, Uuid, Uuid, String, String, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT account_id, conversation_id, profile_id, from_number, msg_type, body, kind, received_at FROM whatsapp_inbound WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, conversation_id, profile_id, from, msg_type, body, kind, received_at)) = row else { return Ok(()) };
    if kind != "message" || msg_type != "text" || body.trim().is_empty() {
        return Ok(());
    }
    let claimed: Option<Uuid> = sqlx::query_scalar("INSERT INTO agent_whatsapp_claims (inbound_id, account_id) VALUES ($1, $2) ON CONFLICT DO NOTHING RETURNING inbound_id")
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    if claimed.is_none() {
        return Ok(());
    }
    // A person on the team already answered after this message: leave it to them.
    let answered: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM whatsapp_sent w JOIN messages m ON m.id = w.message_id
         WHERE m.account_id = $1 AND m.profile_id = $2 AND w.kind = 'reply' AND m.created_at > $3)",
    )
    .bind(account_id)
    .bind(profile_id)
    .bind(received_at)
    .fetch_one(&state.db)
    .await?;
    if answered {
        return Ok(());
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let phone: Option<(Option<String>, bool)> = sqlx::query_as(&format!("SELECT p.phone_number, {} FROM profiles p WHERE p.id = $1 AND p.account_id = $2", chat::TRUSTED_NUMBER_SQL))
        .bind(profile_id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let Some((number, trusted)) = phone else { return Ok(()) };
    let orders = if trusted && number.as_deref() == Some(from.as_str()) { Orders::StatusOnly } else { Orders::None };
    let st = Start {
        account: &account,
        place: "whatsapp",
        who: Who { profile_id: Some(profile_id), orders },
        token: None,
        sms_conversation_id: None,
        whatsapp_conversation_id: Some(conversation_id),
        ip: None,
    };
    let out = chat::say(state, st, &body, |_| {}).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if let Some(c) = out.conversation_id {
        sqlx::query("UPDATE agent_whatsapp_claims SET conversation_id = $2 WHERE inbound_id = $1").bind(id).bind(c).execute(&state.db).await?;
    }
    if out.refused.is_some() {
        return Ok(());
    }
    let Some(reply) = out.reply.filter(|r| !r.trim().is_empty()) else { return Ok(()) };
    let reply: String = reply.chars().take(MAX_REPLY_CHARS).collect();
    let mut tx = state.db.begin().await?;
    // The same per-person hourly allowance the inbox's own automatic answers use, taken in the write.
    let allowed: bool = sqlx::query_scalar(
        "UPDATE whatsapp_conversations SET
            auto_window_count = CASE WHEN auto_window_start IS NULL OR auto_window_start < now() - interval '1 hour' THEN 1 ELSE auto_window_count + 1 END,
            auto_window_start = CASE WHEN auto_window_start IS NULL OR auto_window_start < now() - interval '1 hour' THEN now() ELSE auto_window_start END
         WHERE id = $1 AND account_id = $2 RETURNING auto_window_count <= $3",
    )
    .bind(conversation_id)
    .bind(account_id)
    .bind(AUTO_PER_HOUR)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if !allowed {
        tracing::info!(conversation = %conversation_id, "automatic WhatsApp answer limit for this person reached; not sending");
        return Ok(());
    }
    let mid = crate::features::whatsapp::send::prepare_conversational(&mut tx, account_id, profile_id, &from, "agent_reply", None, &reply, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    crate::jobs::enqueue(&mut *tx, "whatsapp_send", json!({"message_id": mid}), None).await?;
    tx.commit().await?;
    Ok(())
}
