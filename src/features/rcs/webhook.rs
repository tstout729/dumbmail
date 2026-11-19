//! Telnyx's RCS events at `/webhooks/rcs/{account webhook token}`: the agent's webhook URL (inbound
//! messages) and each message's `webhook_url` (`message.sent`, `message.finalized`, `message.read`).
//!
//! Nothing is read before the Ed25519 signature checks out (Telnyx signs `timestamp|body`, see
//! `inbox::verify`); without a signing key the events are refused. Effects apply only for this
//! account's confirmed agent and its own messages. Statuses only move forward
//! (sent → delivered → read), so a repeated or late event changes nothing:
//!
//! - `delivered`: the text is delivered. `read` (or `message.read`): read, and a `Read SMS` event.
//! - `sending_failed` / `delivery_failed` before any delivered or read: the one SMS fallback is
//!   queued, and the RCS charge refunded, in one transaction.
//! - `delivery_unconfirmed`: Telnyx can't say. It may have arrived, so nothing is resent.
//!
//! An inbound message (typed, or a tap on a suggested reply) becomes an inbound text to the
//! account's texting number, so STOP, HELP, keywords, the inbox and "wait for a reply" treat it
//! exactly like an SMS reply. One for an agent that isn't this account's confirmed agent is only
//! honoured as an opt-out.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::audience::{record_event, NewEvent};
use crate::error::{AppError, AppResult};
use crate::features::inbox::{self, Inbound, Proof};
use crate::state::AppState;

/// An event for a message we don't know yet may be racing our own record of the send: ask Telnyx
/// to try again (it retries a few times) while the event is this fresh.
const RETRY_UNKNOWN_SECS: i64 = 300;

pub async fn handle(State(state): State<AppState>, Path(token): Path<String>, headers: HeaderMap, body: Bytes) -> AppResult<StatusCode> {
    if token.is_empty() || token.len() > 200 {
        return Err(AppError::NotFound("not found".into()));
    }
    let account: Option<(Uuid, Option<String>)> = sqlx::query_as("SELECT id, sms_from FROM accounts WHERE webhook_token = $1").bind(&token).fetch_optional(&state.db).await?;
    let Some((account_id, sms_from)) = account else { return Err(AppError::NotFound("not found".into())) };
    if inbox::verify::telnyx(&state, Some(account_id), &headers, &body).await? != Proof::Verified {
        tracing::warn!(account = %account_id, "refused an RCS webhook: no Telnyx signing key to check it with");
        return Err(AppError::Forbidden("RCS events need Telnyx's webhook signing key".into()));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|_| AppError::bad("not JSON"))?;
    let data = &v["data"];
    let p = &data["payload"];
    let agent: Option<String> = sqlx::query_scalar("SELECT checked_agent_id FROM rcs_settings WHERE account_id = $1").bind(account_id).fetch_optional(&state.db).await?.flatten();
    match data["event_type"].as_str().unwrap_or_default() {
        "message.received" => received(&state, account_id, sms_from.as_deref(), agent.as_deref(), p).await?,
        kind @ ("message.sent" | "message.finalized" | "message.read") => {
            let occurred = data["occurred_at"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc));
            return status(&state, account_id, kind, p, occurred).await;
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}

/// What a tap or a typed message says: the text, or the tapped suggestion's label.
fn inbound_text(p: &Value) -> String {
    let b = &p["body"];
    let t = b["text"].as_str().or_else(|| b["suggestion_response"]["text"].as_str()).or_else(|| p["text"].as_str()).unwrap_or_default();
    t.chars().take(inbox::store::MAX_BODY_CHARS).collect()
}

async fn received(state: &AppState, account_id: Uuid, sms_from: Option<&str>, agent: Option<&str>, p: &Value) -> AppResult<()> {
    let Some(from) = p["from"]["phone_number"].as_str().and_then(crate::models::normalize_phone) else {
        tracing::warn!(account = %account_id, "RCS message without a readable sender number; not stored");
        return Ok(());
    };
    let to_agent = p["to"][0]["agent_id"].as_str();
    let text = inbound_text(p);
    let stop = inbox::optout::classify(&text, None) == inbox::optout::Class::Stop;
    let ours = agent.filter(|a| to_agent == Some(*a));
    let Some(our_number) = sms_from.and_then(crate::models::normalize_phone) else {
        if stop {
            inbox::store::opt_out_only(&state.db, account_id, &from, "rcs").await?;
        }
        tracing::warn!(account = %account_id, "RCS message for an account with no texting number; only an opt-out is kept");
        return Ok(());
    };
    if ours.is_none() {
        // Not this account's confirmed agent (or no agent named): only an opt-out counts.
        if stop {
            inbox::store::opt_out_only(&state.db, account_id, &from, &our_number).await?;
        }
        tracing::warn!(account = %account_id, "RCS message for an agent this account hasn't confirmed; only an opt-out is kept");
        return Ok(());
    }
    let media = p["body"]["user_file"]["payload"]["file_uri"]
        .as_str()
        .filter(|u| u.starts_with("https://") && u.len() <= 2000)
        .map(|u| vec![(u.to_string(), p["body"]["user_file"]["payload"]["mime_type"].as_str().map(|m| m.chars().take(100).collect()))])
        .unwrap_or_default();
    let msg = Inbound {
        provider: "telnyx",
        provider_message_id: p["id"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("rcs:{}", s.chars().take(90).collect::<String>())),
        from,
        to: our_number,
        text,
        media,
        provider_signal: None,
        received_at: p["received_at"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc)),
    };
    inbox::inbound(state, Some(account_id), &msg, Proof::Verified).await
}

#[derive(sqlx::FromRow)]
struct Known {
    message_id: Uuid,
    agent_id: String,
    is_test: bool,
    creds_key: String,
}

async fn status(state: &AppState, account_id: Uuid, kind: &str, p: &Value, occurred: Option<DateTime<Utc>>) -> AppResult<StatusCode> {
    let Some(id) = p["id"].as_str().filter(|s| !s.is_empty() && s.len() <= 100) else { return Ok(StatusCode::OK) };
    let known: Option<Known> = sqlx::query_as("SELECT message_id, agent_id, is_test, creds_key FROM rcs_messages WHERE rcs_provider_id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let Some(k) = known else {
        let fresh = occurred.is_none_or(|t| (Utc::now() - t).num_seconds() < RETRY_UNKNOWN_SECS);
        // A failure for a message we may be about to record must not be lost.
        let failure = matches!(p["to"][0]["status"].as_str(), Some("sending_failed" | "delivery_failed"));
        return Ok(if fresh && failure { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK });
    };
    // The key that verified this event belongs to the account's current Telnyx connection: it
    // speaks only for messages that connection sent.
    let current = super::settings::creds(state, account_id).await?.ok().map(|c| c.fingerprint());
    if current.as_deref() != Some(k.creds_key.as_str()) {
        tracing::warn!(account = %account_id, "RCS status for a message sent through a different Telnyx connection; ignored");
        return Ok(StatusCode::OK);
    }
    if p["from"]["agent_id"].as_str().is_some_and(|a| a != k.agent_id) {
        tracing::warn!(account = %account_id, "RCS status for a different agent than the message's; ignored");
        return Ok(StatusCode::OK);
    }
    let st = if kind == "message.read" { "read" } else { p["to"][0]["status"].as_str().unwrap_or_default() };
    match st {
        "delivered" => delivered(state, account_id, &k).await?,
        "read" => {
            delivered(state, account_id, &k).await?;
            read(state, account_id, &k).await?;
        }
        "sending_failed" | "delivery_failed" => failed(state, account_id, &k, st).await?,
        "delivery_unconfirmed" => {
            sqlx::query("UPDATE rcs_messages SET state = 'unconfirmed' WHERE message_id = $1 AND state = 'sent'").bind(k.message_id).execute(&state.db).await?;
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}

async fn delivered(state: &AppState, account_id: Uuid, k: &Known) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    let moved: Option<Uuid> = sqlx::query_scalar(
        "UPDATE rcs_messages SET state = 'delivered', delivered_at = COALESCE(delivered_at, now()) WHERE message_id = $1 AND state IN ('sent', 'unconfirmed') RETURNING message_id",
    )
    .bind(k.message_id)
    .fetch_optional(&mut *tx)
    .await?;
    let first = if moved.is_some() {
        sqlx::query("UPDATE messages SET status = 'delivered', delivered_at = now() WHERE id = $1 AND status = 'sent'").bind(k.message_id).execute(&mut *tx).await?.rows_affected() > 0
    } else {
        false
    };
    tx.commit().await?;
    if first && !k.is_test {
        crate::features::warehouse::changed(&state.db, account_id, "messages", k.message_id).await;
        crate::features::webhooks::message_delivered(&state.db, account_id, k.message_id).await;
    }
    Ok(())
}

async fn read(state: &AppState, account_id: Uuid, k: &Known) -> AppResult<()> {
    let row: Option<(Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "WITH r AS (UPDATE rcs_messages SET state = 'read', read_at = COALESCE(read_at, now()) WHERE message_id = $1 AND state IN ('sent', 'unconfirmed', 'delivered') RETURNING message_id)
         SELECT m.profile_id, m.campaign_id, m.flow_id FROM messages m JOIN r ON r.message_id = m.id",
    )
    .bind(k.message_id)
    .fetch_optional(&state.db)
    .await?;
    if let (Some((profile_id, campaign_id, flow_id)), false) = (row, k.is_test) {
        record_event(
            &state.db,
            NewEvent {
                account_id,
                profile_id,
                metric: super::READ_METRIC,
                properties: json!({"message_id": k.message_id, "campaign_id": campaign_id, "flow_id": flow_id, "rcs": true}),
                value: None,
                occurred_at: None,
                unique_id: Some(format!("rcs-read:{}", k.message_id)),
                message_id: Some(k.message_id),
            },
        )
        .await?;
    }
    Ok(())
}

async fn failed(state: &AppState, account_id: Uuid, k: &Known, why: &str) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    // Only a message Telnyx took and nobody saw arrive: never after delivered, read or unconfirmed.
    let charge: Option<i64> = sqlx::query_scalar(
        "UPDATE rcs_messages SET state = 'fallback_queued', fallback_reason = $2 WHERE message_id = $1 AND state = 'sent' RETURNING rcs_charge_micros",
    )
    .bind(k.message_id)
    .bind(why)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(charge) = charge else {
        tx.commit().await?;
        return Ok(());
    };
    if charge > 0 {
        // Nothing went out as RCS: its charge comes back (the text, if it goes, is billed as SMS).
        crate::billing::post_entry_tx(&mut tx, account_id, "rcs_refund", charge, "RCS message not delivered: refunded (sent as a text instead)", &k.message_id.to_string()).await?;
        sqlx::query("UPDATE messages SET cost_micros = 0 WHERE id = $1").bind(k.message_id).execute(&mut *tx).await?;
    }
    crate::jobs::enqueue(&mut *tx, "rcs_fallback", json!({"message_id": k.message_id}), None).await?;
    tx.commit().await?;
    Ok(())
}
