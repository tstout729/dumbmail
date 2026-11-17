//! Storing inbound texts: matching them to a person, deduping provider retries, the conversation
//! row, and the text of each SMS sent (so a thread shows campaign, flow and reply texts too).

use std::collections::HashMap;

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::audience::{self, NewEvent, ProfileInput};
use crate::error::AppResult;
use crate::sending::Conversational;
use crate::state::AppState;

/// Longest inbound text kept (Twilio's limit; Telnyx's is lower).
pub const MAX_BODY_CHARS: usize = 1600;
/// Most attachments kept from one MMS.
pub const MAX_MEDIA: usize = 10;
pub const HELP_WORDS: &[&str] = &["help", "info"];

/// One inbound text as the provider described it.
#[derive(Debug, Clone, Default)]
pub struct Inbound {
    pub provider: &'static str,
    pub provider_message_id: Option<String>,
    pub from: String,
    pub to: String,
    pub text: String,
    /// Attachment links and their content types.
    pub media: Vec<(String, Option<String>)>,
}

fn media_link(url: &str) -> Option<String> {
    let url = url.trim();
    (url.len() <= 2000 && (url.starts_with("https://") || url.starts_with("http://"))).then(|| url.to_string())
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

impl Inbound {
    /// Telnyx `message.received`: `data.payload` with `id`, `from.phone_number`, `to[0].phone_number`,
    /// `text` and `media[{url, content_type}]`.
    pub fn from_telnyx(p: &Value) -> Self {
        let media = p["media"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| Some((media_link(m["url"].as_str()?)?, m["content_type"].as_str().map(|c| clip(c, 100))))).take(MAX_MEDIA).collect())
            .unwrap_or_default();
        Self {
            provider: "telnyx",
            provider_message_id: p["id"].as_str().filter(|s| !s.is_empty()).map(|s| clip(s, 100)),
            from: p["from"]["phone_number"].as_str().unwrap_or_default().to_string(),
            to: p["to"][0]["phone_number"].as_str().unwrap_or_default().to_string(),
            text: p["text"].as_str().unwrap_or_default().to_string(),
            media,
        }
    }

    /// Twilio's incoming-message webhook: form fields `MessageSid`, `From`, `To`, `Body`, `NumMedia`,
    /// `MediaUrl{N}` and `MediaContentType{N}`. None for anything else (delivery status callbacks).
    pub fn from_twilio(f: &HashMap<String, String>) -> Option<Self> {
        let (from, to, body) = (f.get("From")?, f.get("To")?, f.get("Body")?);
        let n = f.get("NumMedia").and_then(|n| n.trim().parse::<usize>().ok()).unwrap_or(0).min(MAX_MEDIA);
        let media = (0..n)
            .filter_map(|i| Some((media_link(f.get(&format!("MediaUrl{i}"))?)?, f.get(&format!("MediaContentType{i}")).map(|c| clip(c, 100)))))
            .collect();
        Some(Self {
            provider: "twilio",
            provider_message_id: f.get("MessageSid").filter(|s| !s.is_empty()).map(|s| clip(s, 100)),
            from: from.clone(),
            to: to.clone(),
            text: body.clone(),
            media,
        })
    }
}

/// A stored inbound text, handed back to the webhook for STOP/START and then to [`super::received`].
#[derive(Debug, Clone)]
pub struct Received {
    pub id: Uuid,
    pub account_id: Uuid,
    pub profile_id: Uuid,
    pub conversation_id: Uuid,
    /// The number had no profile before this text.
    pub new_profile: bool,
    /// The person's SMS consent before this text changed anything.
    pub prior_consent: String,
    /// message | stop | start | help
    pub kind: &'static str,
    /// The text in lowercase, letters and digits only (how keywords are matched).
    pub word: String,
    pub text: String,
    pub from: String,
    pub to: String,
}

pub fn kind_of(word: &str) -> &'static str {
    if crate::public::STOP_WORDS.contains(&word) {
        "stop"
    } else if crate::public::START_WORDS.contains(&word) {
        "start"
    } else if HELP_WORDS.contains(&word) {
        "help"
    } else {
        "message"
    }
}

pub fn preview(text: &str, media: usize) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return match media {
            0 => String::new(),
            1 => "Sent an attachment".into(),
            n => format!("Sent {n} attachments"),
        };
    }
    let clipped = clip(&t, 160);
    if clipped.len() < t.len() { format!("{clipped}…") } else { clipped }
}

/// Store an inbound text for one account. Returns None when the provider already delivered it.
pub async fn receive(state: &AppState, account_id: Uuid, from: &str, to: &str, msg: &Inbound, word: &str) -> AppResult<Option<Received>> {
    let existing: Option<(Uuid, String)> = sqlx::query_as("SELECT id, sms_consent FROM profiles WHERE account_id = $1 AND phone_number = $2")
        .bind(account_id)
        .bind(from)
        .fetch_optional(&state.db)
        .await?;
    let (profile_id, prior_consent, new_profile) = match existing {
        Some((id, consent)) => (id, consent, false),
        None => {
            // A number we haven't seen: a profile with just the number. Texting in isn't consent.
            let u = audience::upsert_profile(&state.db, account_id, &ProfileInput { phone_number: Some(from.to_string()), ..Default::default() }).await?;
            (u.profile.id, u.profile.sms_consent, u.created)
        }
    };
    let text = clip(&msg.text, MAX_BODY_CHARS);
    let media = Value::Array(msg.media.iter().map(|(url, ct)| json!({"url": url, "content_type": ct})).collect());
    let kind = kind_of(word);

    let mut tx = state.db.begin().await?;
    let conversation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO sms_conversations (id, account_id, profile_id) VALUES ($1, $2, $3)
         ON CONFLICT (profile_id) DO UPDATE SET profile_id = EXCLUDED.profile_id RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(profile_id)
    .fetch_one(&mut *tx)
    .await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO sms_inbound (id, account_id, conversation_id, profile_id, provider, provider_message_id, from_number, to_number, body, kind, media)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (account_id, provider, provider_message_id) WHERE provider_message_id IS NOT NULL DO NOTHING
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(conversation_id)
    .bind(profile_id)
    .bind(msg.provider)
    .bind(&msg.provider_message_id)
    .bind(from)
    .bind(to)
    .bind(&text)
    .bind(kind)
    .bind(&media)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        tx.rollback().await?;
        return Ok(None);
    };
    // A new text reopens a closed conversation.
    sqlx::query(
        "UPDATE sms_conversations SET unread_count = unread_count + 1, last_message_at = now(), last_inbound_at = now(),
                last_direction = 'inbound', last_preview = $2, status = 'open', closed_at = NULL
         WHERE id = $1",
    )
    .bind(conversation_id)
    .bind(preview(&text, msg.media.len()))
    .execute(&mut *tx)
    .await?;
    if !msg.media.is_empty() {
        crate::jobs::enqueue(&mut *tx, "inbox_media", json!({"inbound_id": id}), None).await?;
    }
    tx.commit().await?;
    Ok(Some(Received {
        id,
        account_id,
        profile_id,
        conversation_id,
        new_profile,
        prior_consent,
        kind,
        word: word.to_string(),
        text,
        from: from.to_string(),
        to: to.to_string(),
    }))
}

pub async fn set_kind(db: &PgPool, inbound_id: Uuid, kind: &str, keyword: Option<&str>) -> AppResult<()> {
    sqlx::query("UPDATE sms_inbound SET kind = $2, keyword = $3 WHERE id = $1").bind(inbound_id).bind(kind).bind(keyword).execute(db).await?;
    Ok(())
}

/// "Received SMS Reply": flows and segments use it (a flow trigger, "engaged with SMS").
pub async fn record_reply_event(db: &PgPool, rx: &Received, keyword: Option<&str>, media: usize) -> AppResult<()> {
    audience::record_event(
        db,
        NewEvent {
            account_id: rx.account_id,
            profile_id: rx.profile_id,
            metric: "Received SMS Reply",
            properties: json!({"text": rx.text, "keyword": keyword, "to": rx.to, "media_count": media, "inbound_message_id": rx.id}),
            value: None,
            occurred_at: None,
            unique_id: Some(format!("sms-in:{}", rx.id)),
            message_id: None,
        },
    )
    .await?;
    Ok(())
}

/// Called by `sending` for every real SMS just before it goes to the provider.
pub async fn record_sent(db: &PgPool, account_id: Uuid, profile_id: Uuid, message_id: Uuid, body: &str, media_url: Option<&str>, conversational: Option<&Conversational>) -> AppResult<()> {
    let kind = conversational.map_or("marketing", |c| c.kind);
    sqlx::query(
        "INSERT INTO sms_sent_texts (message_id, account_id, body, media_url, kind, sent_by) VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (message_id) DO UPDATE SET body = EXCLUDED.body, media_url = EXCLUDED.media_url",
    )
    .bind(message_id)
    .bind(account_id)
    .bind(body)
    .bind(media_url)
    .bind(kind)
    .bind(conversational.and_then(|c| c.sent_by))
    .execute(db)
    .await?;
    // Replies move the conversation to the top; campaign and flow texts show in the thread without reordering it.
    if let Some(c) = conversational {
        sqlx::query(
            "UPDATE sms_conversations SET last_message_at = now(), last_direction = 'outbound', last_preview = $2,
                    unread_count = CASE WHEN $3 THEN 0 ELSE unread_count END
             WHERE profile_id = $1",
        )
        .bind(profile_id)
        .bind(preview(body, usize::from(media_url.is_some())))
        .bind(c.kind == "reply")
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Wake flow runs waiting for this person's reply, so they take the "replied" path now.
pub async fn wake_waiting_flows(db: &PgPool, profile_id: Uuid) -> AppResult<()> {
    sqlx::query(
        "UPDATE jobs SET run_at = now()
         WHERE kind = 'flow_step' AND locked_at IS NULL AND run_at > now() AND run_at <> 'infinity'
           AND payload->>'run_id' IN (SELECT id::text FROM flow_runs WHERE profile_id = $1 AND status = 'active' AND context ? '_reply_since')",
    )
    .bind(profile_id)
    .execute(db)
    .await?;
    Ok(())
}

/// For the flow step "wait for a reply": did this person text in (not STOP, START or HELP) since `since`?
pub async fn replied_since(db: &PgPool, profile_id: Uuid, since: chrono::DateTime<chrono::Utc>) -> AppResult<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sms_inbound WHERE profile_id = $1 AND received_at >= $2 AND kind NOT IN ('stop', 'start', 'help'))")
        .bind(profile_id)
        .bind(since)
        .fetch_one(db)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_providers() {
        let t = Inbound::from_telnyx(&json!({
            "id": "b301ed3f", "text": "Hi", "type": "MMS", "direction": "inbound",
            "from": {"phone_number": "+13125550001", "carrier": "T-Mobile"},
            "to": [{"phone_number": "+13125550002", "status": "webhook_delivered"}],
            "media": [{"url": "https://media.telnyx.example/a.png", "content_type": "image/png", "size": 10}, {"url": "javascript:alert(1)"}]
        }));
        assert_eq!(t.provider_message_id.as_deref(), Some("b301ed3f"));
        assert_eq!(t.from, "+13125550001");
        assert_eq!(t.to, "+13125550002");
        assert_eq!(t.media, vec![("https://media.telnyx.example/a.png".to_string(), Some("image/png".to_string()))]);

        let f: HashMap<String, String> = [("MessageSid", "SM1"), ("From", "+15035550199"), ("To", "+15035550100"), ("Body", ""), ("NumMedia", "2"), ("MediaUrl0", "https://api.twilio.com/m/0"), ("MediaContentType0", "image/jpeg"), ("MediaUrl1", "https://api.twilio.com/m/1")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let w = Inbound::from_twilio(&f).unwrap();
        assert_eq!(w.provider_message_id.as_deref(), Some("SM1"));
        assert_eq!(w.media.len(), 2);
        assert_eq!(w.media[1].1, None);
        let status: HashMap<String, String> = [("MessageSid", "SM1"), ("MessageStatus", "delivered")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert!(Inbound::from_twilio(&status).is_none());
    }

    #[test]
    fn kinds_and_previews() {
        assert_eq!(kind_of("stop"), "stop");
        assert_eq!(kind_of("unstop"), "start");
        assert_eq!(kind_of("info"), "help");
        assert_eq!(kind_of("join"), "message");
        assert_eq!(preview("  hello\n there ", 0), "hello there");
        assert_eq!(preview("", 2), "Sent 2 attachments");
        assert!(preview(&"a".repeat(300), 0).ends_with('…'));
    }
}
