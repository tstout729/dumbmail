//! First answers to inbound texts, when the store turns them on (off by default).
//!
//! The inbox hands over an ordinary verified text (not STOP, HELP, a keyword or a confirmation)
//! with [`take`]; the agent's job answers it once (a claim on the inbound text, whatever retries
//! happen) and the answer goes out as an automatic text through the inbox's own path
//! (`inbox::replies::queue_auto`): the per-person limit, the hourly budgets, the opt-out check at
//! send time and the per-message billing all apply. A texter is verified for their own profile by
//! texting from its number, and sees order status and tracking only, and only when the store gave
//! that number (`phone_source`). A conversation the agent handed off isn't answered again by it.

use serde_json::{json, Value};
use uuid::Uuid;

use super::chat::{self, Orders, Start, Who};
use super::settings;
use crate::error::AppResult;
use crate::features::inbox::store::Received;
use crate::state::AppState;

pub const JOB: &str = "agent_sms_answer";
/// The longest text the agent sends.
const MAX_REPLY_CHARS: usize = 640;

/// The model's words as they must reach the texter: automatic texts are rendered as message
/// templates, so template markers (`{{ }}`, `{% %}`, `{# #}`) are broken up and can never run a
/// tag (a coupon code, a link, someone's details), whatever a page or a shopper got the model to say.
pub fn plain_text(s: &str) -> String {
    let mut out = s.to_string();
    loop {
        let next = out.replace("{{", "{ {").replace("}}", "} }").replace("{%", "{ %").replace("%}", "% }").replace("{#", "{ #").replace("#}", "# }");
        if next == out {
            return out;
        }
        out = next;
    }
}

/// Whether the agent answers this text (its job is queued; the inbox's default reply doesn't go).
pub async fn take(state: &AppState, rx: &Received) -> AppResult<bool> {
    if !rx.verified || rx.from == rx.to || rx.text.trim().is_empty() {
        return Ok(false);
    }
    let s = settings::load(&state.db, rx.account_id).await?;
    let account = crate::campaigns::load_account(&state.db, rx.account_id).await?;
    if !chat::available(state, &account, &s, "sms") {
        return Ok(false);
    }
    crate::jobs::enqueue_background(&state.db, JOB, json!({"inbound_id": rx.id}), None).await?;
    Ok(true)
}

async fn load(state: &AppState, id: Uuid) -> anyhow::Result<Option<Received>> {
    #[allow(clippy::type_complexity)]
    let row: Option<(Uuid, Uuid, Uuid, Uuid, String, String, String, Option<String>, String, String, String, bool, Option<String>, i32, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, account_id, profile_id, conversation_id, provider, provider_message_id, kind, keyword, body, from_number, to_number, verified, provider_signal,
                jsonb_array_length(media), provider_received_at
         FROM sms_inbound WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|r| Received {
        id: r.0,
        account_id: r.1,
        profile_id: r.2,
        conversation_id: r.3,
        provider: r.4,
        provider_message_id: r.5,
        kind: r.6,
        keyword: r.7,
        word: crate::features::inbox::optout::word(&r.8),
        text: r.8,
        from: r.9,
        to: r.10,
        verified: r.11,
        provider_signal: r.12,
        media_count: r.13.max(0) as usize,
        received_at: r.14,
        processed: true,
    }))
}

/// Job: answer one inbound text, at most once.
pub async fn run(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["inbound_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let Some(rx) = load(state, id).await? else { return Ok(()) };
    if rx.kind != "message" || !rx.verified {
        return Ok(());
    }
    let claimed: Option<Uuid> = sqlx::query_scalar("INSERT INTO agent_sms_claims (inbound_id, account_id) VALUES ($1, $2) ON CONFLICT DO NOTHING RETURNING inbound_id")
        .bind(rx.id)
        .bind(rx.account_id)
        .fetch_optional(&state.db)
        .await?;
    if claimed.is_none() {
        return Ok(());
    }
    // A person on the team already answered after this text: leave it to them.
    let answered: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sms_sent_texts t JOIN messages m ON m.id = t.message_id
         WHERE m.account_id = $1 AND m.profile_id = $2 AND t.kind = 'reply' AND t.created_at > $3)",
    )
    .bind(rx.account_id)
    .bind(rx.profile_id)
    .bind(rx.received_at)
    .fetch_one(&state.db)
    .await?;
    if answered {
        return Ok(());
    }
    let account = crate::campaigns::load_account(&state.db, rx.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    // Their own profile by their number; orders only when the store gave that number.
    let phone: Option<(Option<String>, bool)> = sqlx::query_as(&format!("SELECT p.phone_number, {} FROM profiles p WHERE p.id = $1 AND p.account_id = $2", chat::TRUSTED_NUMBER_SQL))
        .bind(rx.profile_id)
        .bind(rx.account_id)
        .fetch_optional(&state.db)
        .await?;
    let Some((number, trusted)) = phone else { return Ok(()) };
    let orders = if trusted && number.as_deref() == Some(rx.from.as_str()) { Orders::StatusOnly } else { Orders::None };
    let st = Start { account: &account, place: "sms", who: Who { profile_id: Some(rx.profile_id), orders }, token: None, sms_conversation_id: Some(rx.conversation_id), ip: None };
    let out = chat::say(state, st, &rx.text, |_| {}).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if let Some(c) = out.conversation_id {
        sqlx::query("UPDATE agent_sms_claims SET conversation_id = $2 WHERE inbound_id = $1").bind(rx.id).bind(c).execute(&state.db).await?;
    }
    // Off, the monthly cap or no credit: the inbox's own default or away reply, as without the agent.
    if out.refused.is_some() {
        return crate::features::inbox::replies::default_reply(state, &rx).await.map_err(|e| anyhow::anyhow!(e.public_message()));
    }
    let Some(reply) = out.reply.filter(|r| !r.trim().is_empty()) else { return Ok(()) };
    let reply = plain_text(&reply.chars().take(MAX_REPLY_CHARS).collect::<String>());
    crate::features::inbox::replies::queue_auto(state, &rx, "agent_reply", &reply, None, json!({"agent_conversation": out.conversation_id}))
        .await
        .map_err(|e| anyhow::anyhow!(e.public_message()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_never_carry_template_tags() {
        for s in ["{% coupon_code 'VIP' %}", "{{ person.email }}", "{{{ x }}}", "{#{% raw %}#}", "%}}{{%"] {
            let p = plain_text(s);
            assert!(!["{{", "}}", "{%", "%}", "{#", "#}"].iter().any(|m| p.contains(m)), "{s} -> {p}");
        }
        assert_eq!(plain_text("Use code SAVE10 at {store}"), "Use code SAVE10 at {store}");
    }
}
