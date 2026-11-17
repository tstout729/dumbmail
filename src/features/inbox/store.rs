//! Storing inbound texts and their effect on consent (one transaction), carrier-level opt-outs,
//! reply windows, the final check before any text goes to the provider, and the text of each SMS
//! sent (so a thread shows campaign, flow and reply texts too).

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::keywords::{self, Keyword};
use super::optout::{self, Class};
use crate::audience::{self, ProfileInput};
use crate::error::AppResult;
use crate::sending::Conversational;
use crate::state::AppState;

/// Longest inbound text kept (Twilio's limit; Telnyx's is lower).
pub const MAX_BODY_CHARS: usize = 1600;
/// Most attachments kept from one MMS.
pub const MAX_MEDIA: usize = 10;
/// Someone not subscribed to marketing texts can be answered this long after their last text.
pub const REPLY_WINDOW_HOURS: i64 = 24;
/// Inbound kinds that don't open a reply window: an opt-out, texts from opted-out numbers, and
/// HELP (answered on its own; it doesn't invite other replies).
pub const NO_WINDOW_KINDS: &str = "('stop', 'blocked', 'help')";

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
    /// STOP | START | HELP when the provider handled the text as one.
    pub provider_signal: Option<String>,
    /// When the provider received it, if it said.
    pub received_at: Option<DateTime<Utc>>,
}

fn media_link(url: &str) -> Option<String> {
    let url = url.trim();
    (url.len() <= 2000 && (url.starts_with("https://") || url.starts_with("http://"))).then(|| url.to_string())
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn signal(raw: Option<&str>) -> Option<String> {
    raw.map(|s| s.trim().to_ascii_uppercase()).filter(|s| matches!(s.as_str(), "STOP" | "START" | "HELP"))
}

impl Inbound {
    /// Telnyx `message.received`: `data.payload` with `id`, `from.phone_number`, `to[0].phone_number`,
    /// `text`, `media[{url, content_type}]`, `received_at` and, when Telnyx answered it as an opt-out
    /// keyword, `autoresponse_type`.
    pub fn from_telnyx(p: &Value) -> Self {
        let media = p["media"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| Some((media_link(m["url"].as_str()?)?, m["content_type"].as_str().map(|c| clip(c, 100))))).take(MAX_MEDIA).collect())
            .unwrap_or_default();
        Self {
            provider: "telnyx",
            provider_message_id: p["id"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(|s| clip(s, 100)),
            from: p["from"]["phone_number"].as_str().unwrap_or_default().to_string(),
            to: p["to"][0]["phone_number"].as_str().unwrap_or_default().to_string(),
            text: p["text"].as_str().unwrap_or_default().to_string(),
            media,
            provider_signal: signal(p["autoresponse_type"].as_str()),
            received_at: p["received_at"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc)),
        }
    }

    /// Twilio's incoming-message webhook: form fields `MessageSid`, `From`, `To`, `Body`, `NumMedia`,
    /// `MediaUrl{N}`, `MediaContentType{N}` and, with Advanced Opt-Out, `OptOutType`. None for
    /// anything else (delivery status callbacks).
    pub fn from_twilio(f: &[(String, String)]) -> Option<Self> {
        let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let (from, to, body) = (get("From")?, get("To")?, get("Body")?);
        let n = get("NumMedia").and_then(|n| n.trim().parse::<usize>().ok()).unwrap_or(0).min(MAX_MEDIA);
        let media = (0..n)
            .filter_map(|i| Some((media_link(get(&format!("MediaUrl{i}"))?)?, get(&format!("MediaContentType{i}")).map(|c| clip(c, 100)))))
            .collect();
        Some(Self {
            provider: "twilio",
            provider_message_id: get("MessageSid").map(str::trim).filter(|s| !s.is_empty()).map(|s| clip(s, 100)),
            from: from.to_string(),
            to: to.to_string(),
            text: body.to_string(),
            media,
            provider_signal: signal(get("OptOutType")),
            received_at: None,
        })
    }
}

/// A stored inbound text, handed on to [`super::process`].
#[derive(Debug, Clone)]
pub struct Received {
    pub id: Uuid,
    pub account_id: Uuid,
    pub profile_id: Uuid,
    pub conversation_id: Uuid,
    pub provider: String,
    pub provider_message_id: String,
    /// message | stop | start | help | keyword | confirm | blocked
    pub kind: String,
    /// The keyword it matched (kind keyword, or blocked when they texted one while opted out).
    pub keyword: Option<String>,
    /// The text in lowercase, letters and digits only (how keywords are matched).
    pub word: String,
    pub text: String,
    pub from: String,
    pub to: String,
    pub verified: bool,
    pub provider_signal: Option<String>,
    pub media_count: usize,
    pub received_at: DateTime<Utc>,
    /// Everything after storing it already ran (a provider retry of a finished text).
    pub processed: bool,
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

/// Serializes everything that changes or checks one number's opt-out state in one account: an
/// inbound text's consent effects and the final check before a text goes to the provider.
pub async fn lock_number(conn: &mut PgConnection, account_id: Uuid, phone: &str) -> AppResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || ':' || $2, 64))").bind(account_id).bind(phone).execute(conn).await?;
    Ok(())
}

pub async fn is_opted_out(conn: &mut PgConnection, account_id: Uuid, phone: &str) -> AppResult<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sms_opt_outs WHERE account_id = $1 AND phone_number = $2)").bind(account_id).bind(phone).fetch_one(conn).await?)
}

/// Opt a number out: block it at the carrier level, mark its profile unsubscribed from texts,
/// and cancel its queued texts and pending double opt-ins. Call with the number locked.
async fn apply_opt_out(conn: &mut PgConnection, account_id: Uuid, phone: &str, ours: &str, source: &str, inbound_id: Option<Uuid>) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO sms_opt_outs (account_id, phone_number, our_number, source, inbound_id) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT DO NOTHING",
    )
    .bind(account_id)
    .bind(phone)
    .bind(ours)
    .bind(source)
    .bind(inbound_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE profiles SET sms_consent = 'unsubscribed', updated_at = now() WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(phone).execute(&mut *conn).await?;
    sqlx::query("UPDATE messages SET status = 'skipped', error = 'opted out' WHERE account_id = $1 AND channel = 'sms' AND to_address = $2 AND status = 'queued'")
        .bind(account_id)
        .bind(phone)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE sms_auto_sends SET status = 'cancelled', error = 'opted out', finished_at = now() WHERE account_id = $1 AND to_number = $2 AND status = 'queued'")
        .bind(account_id)
        .bind(phone)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM sms_pending_optins WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(phone).execute(&mut *conn).await?;
    Ok(())
}

/// An opt-out that can't be stored as a text (no message id, or it came through a connection the
/// number doesn't use) but whose number belongs to exactly one account: the opt-out is still
/// applied there (opting out is always the safe direction).
pub async fn opt_out_only(db: &PgPool, account_id: Uuid, phone: &str, ours: &str) -> AppResult<()> {
    let mut tx = db.begin().await?;
    lock_number(&mut tx, account_id, phone).await?;
    apply_opt_out(&mut tx, account_id, phone, ours, "keyword", None).await?;
    tx.commit().await?;
    Ok(())
}

fn opt_out_source(msg: &Inbound) -> &'static str {
    if msg.provider_signal.as_deref() == Some("STOP") {
        "provider"
    } else if optout::STANDALONE_STOP.contains(&optout::word(&msg.text).as_str()) {
        "keyword"
    } else {
        "phrase"
    }
}

#[derive(sqlx::FromRow)]
struct InboundRow {
    id: Uuid,
    profile_id: Uuid,
    conversation_id: Uuid,
    provider: String,
    provider_message_id: String,
    kind: String,
    keyword: Option<String>,
    body: String,
    from_number: String,
    to_number: String,
    verified: bool,
    provider_signal: Option<String>,
    media_count: i32,
    provider_received_at: DateTime<Utc>,
    processed_at: Option<DateTime<Utc>>,
}

const INBOUND_COLS: &str = "id, profile_id, conversation_id, provider, provider_message_id, kind, keyword, body, from_number, to_number, verified, provider_signal,
    jsonb_array_length(media)::int AS media_count, provider_received_at, processed_at";

impl InboundRow {
    fn into_received(self, account_id: Uuid) -> Received {
        Received {
            id: self.id,
            account_id,
            profile_id: self.profile_id,
            conversation_id: self.conversation_id,
            provider: self.provider,
            provider_message_id: self.provider_message_id,
            kind: self.kind,
            keyword: self.keyword,
            word: optout::word(&self.body),
            text: self.body,
            from: self.from_number,
            to: self.to_number,
            verified: self.verified,
            provider_signal: self.provider_signal,
            media_count: self.media_count.max(0) as usize,
            received_at: self.provider_received_at,
            processed: self.processed_at.is_some(),
        }
    }
}

/// Store an inbound text for one account and apply its effect on consent, in one transaction:
/// an opt-out (which wins over everything), START lifting one, or nothing. The text's kind says
/// what happens next. A provider retry returns the stored text (`processed` says whether
/// everything after storing it already ran).
pub async fn receive(state: &AppState, account_id: Uuid, from: &str, to: &str, msg: &Inbound, provider_message_id: &str, verified: bool) -> AppResult<Received> {
    let db = &state.db;
    let class = optout::classify(&msg.text, msg.provider_signal.as_deref());
    let word = optout::word(&msg.text);
    // Words that grant permission (START, UNSTOP, YES, Y, sign-up keywords) only count as the whole text.
    let strict = optout::strict_word(&msg.text);
    let keyword: Option<Keyword> = if class == Class::Other { keywords::matching(db, account_id, &word, &strict).await? } else { None };

    let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(from).fetch_optional(db).await?;
    let (profile_id, new_profile) = match existing {
        Some(id) => (id, false),
        None => {
            // A number we haven't seen: a profile with just the number. Texting in isn't consent.
            let u = audience::upsert_profile(db, account_id, &ProfileInput { phone_number: Some(from.to_string()), ..Default::default() }).await?;
            (u.profile.id, u.created)
        }
    };
    let text = clip(&msg.text, MAX_BODY_CHARS);
    let media = Value::Array(msg.media.iter().map(|(url, ct)| json!({"url": url, "content_type": ct})).collect());
    // The provider's time, but never in the future.
    let now = Utc::now();
    let received_at = msg.received_at.map_or(now, |t| t.min(now));

    let mut tx = db.begin().await?;
    lock_number(&mut tx, account_id, from).await?;
    // Their opt-outs, one per number of ours that got one.
    let blocked_on: Vec<String> = sqlx::query_scalar("SELECT our_number FROM sms_opt_outs WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(from).fetch_all(&mut *tx).await?;
    let opted_out = !blocked_on.is_empty();
    // A re-opt-in word lifts only the opt-out made on the number it was sent to.
    let lift_here = verified && blocked_on.iter().any(|o| o == to) && (class == Class::ProviderStart || optout::lifts_opt_out(msg.provider, to, &strict));
    let lifts_all = lift_here && blocked_on.iter().all(|o| o == to);
    // YES or Y is consent only as the answer to a prompt that went to this number before it was received.
    let pending_confirm = verified
        && !opted_out
        && matches!(strict.as_str(), "y" | "yes")
        && keywords::pending_optin(&mut tx, account_id, from, to, received_at).await?.is_some();
    let start_word = class == Class::ProviderStart || optout::START_WORDS.contains(&strict.as_str());
    // What this text is. STOP is decided first; an opted-out number can only opt back in (with the
    // provider's own words, on the number that got the opt-out) or ask for help: anything else from
    // it does nothing.
    let (kind, kw): (&str, Option<String>) = match class {
        Class::Stop => ("stop", None),
        _ if lifts_all => ("start", None),
        Class::Help => ("help", None),
        _ if opted_out => ("blocked", keyword.as_ref().map(|k| k.keyword.clone())),
        _ if pending_confirm => ("confirm", None),
        // As before the inbox: START or UNSTOP from a known number subscribes it.
        _ if start_word && verified && !new_profile => ("start", None),
        _ => match &keyword {
            Some(k) => ("keyword", Some(k.keyword.clone())),
            None => ("message", None),
        },
    };

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
        "INSERT INTO sms_inbound (id, account_id, conversation_id, profile_id, provider, provider_message_id, from_number, to_number, body, kind, keyword, media,
                                  verified, provider_signal, provider_received_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
         ON CONFLICT (account_id, provider, provider_message_id) DO NOTHING
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(conversation_id)
    .bind(profile_id)
    .bind(msg.provider)
    .bind(provider_message_id)
    .bind(from)
    .bind(to)
    .bind(&text)
    .bind(kind)
    .bind(&kw)
    .bind(&media)
    .bind(verified)
    .bind(&msg.provider_signal)
    .bind(received_at)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        // The provider already delivered it once; its consent effect committed with it.
        tx.rollback().await?;
        return load(db, account_id, msg.provider, provider_message_id).await;
    };
    match kind {
        "stop" => apply_opt_out(&mut tx, account_id, from, to, opt_out_source(msg), Some(id)).await?,
        "start" => {
            sqlx::query("DELETE FROM sms_opt_outs WHERE account_id = $1 AND phone_number = $2 AND our_number = $3").bind(account_id).bind(from).bind(to).execute(&mut *tx).await?;
            // Only while the profile still has the number that texted.
            sqlx::query("UPDATE profiles SET sms_consent = 'subscribed', updated_at = now() WHERE id = $1 AND phone_number = $2").bind(profile_id).bind(from).execute(&mut *tx).await?;
        }
        // START on this number while another of ours still has their opt-out: lift this one only.
        _ if lift_here => {
            sqlx::query("DELETE FROM sms_opt_outs WHERE account_id = $1 AND phone_number = $2 AND our_number = $3").bind(account_id).bind(from).bind(to).execute(&mut *tx).await?;
        }
        _ => {}
    }
    // A new text reopens a closed conversation.
    sqlx::query(
        "UPDATE sms_conversations SET unread_count = unread_count + 1, last_message_at = now(), last_inbound_at = GREATEST(COALESCE(last_inbound_at, $3), $3),
                last_direction = 'inbound', last_preview = $2, status = 'open', closed_at = NULL
         WHERE id = $1",
    )
    .bind(conversation_id)
    .bind(preview(&text, msg.media.len()))
    .bind(received_at)
    .execute(&mut *tx)
    .await?;
    if !msg.media.is_empty() {
        crate::jobs::enqueue(&mut *tx, "inbox_media", json!({"inbound_id": id}), None).await?;
    }
    tx.commit().await?;
    load(db, account_id, msg.provider, provider_message_id).await
}

async fn load(db: &PgPool, account_id: Uuid, provider: &str, provider_message_id: &str) -> AppResult<Received> {
    let q = format!("SELECT {INBOUND_COLS} FROM sms_inbound WHERE account_id = $1 AND provider = $2 AND provider_message_id = $3");
    let row: InboundRow = sqlx::query_as(&q).bind(account_id).bind(provider).bind(provider_message_id).fetch_one(db).await?;
    Ok(row.into_received(account_id))
}

pub async fn set_kind(db: &PgPool, inbound_id: Uuid, kind: &str, keyword: Option<&str>) -> AppResult<()> {
    sqlx::query("UPDATE sms_inbound SET kind = $2, keyword = COALESCE($3, keyword) WHERE id = $1").bind(inbound_id).bind(kind).bind(keyword).execute(db).await?;
    Ok(())
}

pub async fn mark_processed(db: &PgPool, inbound_id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE sms_inbound SET processed_at = now() WHERE id = $1").bind(inbound_id).execute(db).await?;
    Ok(())
}

/// The numbers a reply in this conversation goes between: theirs and ours, from their latest
/// verified text that can be answered. Without one (only unverified texts so far), the profile's
/// number and the account's sending number: a reply can then go only if they're subscribed on that
/// number, and an unverified text never picks which of our numbers it goes from. It never follows
/// later edits to the profile's phone number while there's a verified text.
pub async fn reply_target(db: &PgPool, account_id: Uuid, conversation_id: Uuid) -> AppResult<Option<(String, String)>> {
    let verified: Option<(String, String)> = sqlx::query_as(&format!(
        "SELECT from_number, to_number FROM sms_inbound WHERE conversation_id = $1 AND account_id = $2 AND verified AND kind NOT IN {NO_WINDOW_KINDS}
         ORDER BY provider_received_at DESC, id DESC LIMIT 1"
    ))
    .bind(conversation_id)
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    if verified.is_some() {
        return Ok(verified);
    }
    Ok(sqlx::query_as(
        "SELECT p.phone_number, a.sms_from FROM sms_conversations c JOIN profiles p ON p.id = c.profile_id JOIN accounts a ON a.id = c.account_id
         WHERE c.id = $1 AND c.account_id = $2 AND p.phone_number IS NOT NULL AND a.sms_from IS NOT NULL",
    )
    .bind(conversation_id)
    .bind(account_id)
    .fetch_optional(db)
    .await?)
}

/// When the latest verified text from `theirs` to `ours` arrived (the start of its reply window).
pub async fn window_start(conn: &mut PgConnection, account_id: Uuid, theirs: &str, ours: &str) -> AppResult<Option<DateTime<Utc>>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT max(provider_received_at) FROM sms_inbound WHERE account_id = $1 AND from_number = $2 AND to_number = $3 AND verified AND kind NOT IN {NO_WINDOW_KINDS}"
    ))
    .bind(account_id)
    .bind(theirs)
    .bind(ours)
    .fetch_one(conn)
    .await?)
}

pub fn window_open(start: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    start.is_some_and(|at| now - at < chrono::Duration::hours(REPLY_WINDOW_HOURS))
}

/// The inbox's reply rules for a conversational text, from state read under the number's lock in
/// the same transaction that claims the message (`sending::claim_text`; the carrier-level opt-out
/// is checked there first). Returns why not, if it can't go.
///
/// - It goes from one of the account's numbers.
/// - HELP answers: always (they're administrative; the provider still enforces its own block).
/// - Otherwise the person hasn't unsubscribed, and either the reply window from their last verified
///   text to that number is open or (for a reply typed by a person) they're subscribed on that number.
pub fn reply_allowed(c: &Conversational, consent: &str, phone: Option<&str>, from_ours: bool, window_start: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Result<(), &'static str> {
    if !from_ours {
        return Err("the number they texted is no longer this account's");
    }
    if c.kind == "help" {
        return Ok(());
    }
    if consent == "unsubscribed" {
        return Err("unsubscribed from texts");
    }
    let subscribed_here = c.kind == "reply" && consent == "subscribed" && phone == Some(c.to.as_str());
    if !window_open(window_start, now) && !subscribed_here {
        return Err("reply window closed");
    }
    Ok(())
}

/// "Received SMS Reply": flows and segments use it (a flow trigger, "engaged with SMS").
pub async fn record_reply_event(db: &PgPool, rx: &Received, keyword: Option<&str>) -> AppResult<()> {
    audience::record_event(
        db,
        audience::NewEvent {
            account_id: rx.account_id,
            profile_id: rx.profile_id,
            metric: "Received SMS Reply",
            properties: json!({"text": rx.text, "keyword": keyword, "to": rx.to, "media_count": rx.media_count, "inbound_message_id": rx.id}),
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

/// Wake flow runs waiting for this person's reply, so they take the "replied" path now: only the
/// waits whose window this text was received in (an older text delivered late wakes nothing).
pub async fn wake_waiting_flows(db: &PgPool, profile_id: Uuid, received_at: DateTime<Utc>) -> AppResult<()> {
    sqlx::query(
        "UPDATE jobs SET run_at = now()
         WHERE kind = 'flow_step' AND locked_at IS NULL AND run_at > now() AND run_at <> 'infinity'
           AND payload->>'run_id' IN (SELECT id::text FROM flow_runs WHERE profile_id = $1 AND status = 'active' AND context ? '_reply_since'
                                       AND (context->>'_reply_since')::timestamptz <= $2 AND (context->>'_reply_until')::timestamptz >= $2)",
    )
    .bind(profile_id)
    .bind(received_at)
    .execute(db)
    .await?;
    Ok(())
}

/// For the flow step "wait for a reply": did this person send a verified text that counts as a
/// reply (not an opt-out, START, HELP or a text while opted out) between `since` and `until`?
pub async fn replied_between(db: &PgPool, profile_id: Uuid, since: DateTime<Utc>, until: DateTime<Utc>) -> AppResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sms_inbound WHERE profile_id = $1 AND verified AND provider_received_at >= $2 AND provider_received_at <= $3
                        AND kind IN ('message', 'keyword', 'confirm'))",
    )
    .bind(profile_id)
    .bind(since)
    .bind(until)
    .fetch_one(db)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_providers() {
        let t = Inbound::from_telnyx(&json!({
            "id": "b301ed3f", "text": "Hi", "type": "MMS", "direction": "inbound", "received_at": "2026-11-17T18:00:00.000+00:00",
            "from": {"phone_number": "+13125550001", "carrier": "T-Mobile"},
            "to": [{"phone_number": "+13125550002", "status": "webhook_delivered"}],
            "media": [{"url": "https://media.telnyx.example/a.png", "content_type": "image/png", "size": 10}, {"url": "javascript:alert(1)"}],
            "autoresponse_type": "stop"
        }));
        assert_eq!(t.provider_message_id.as_deref(), Some("b301ed3f"));
        assert_eq!(t.from, "+13125550001");
        assert_eq!(t.to, "+13125550002");
        assert_eq!(t.media, vec![("https://media.telnyx.example/a.png".to_string(), Some("image/png".to_string()))]);
        assert_eq!(t.provider_signal.as_deref(), Some("STOP"));
        assert_eq!(t.received_at.unwrap().to_rfc3339(), "2026-11-17T18:00:00+00:00");

        let f: Vec<(String, String)> = [("MessageSid", "SM1"), ("From", "+15035550199"), ("To", "+15035550100"), ("Body", ""), ("NumMedia", "2"), ("MediaUrl0", "https://api.twilio.com/m/0"), ("MediaContentType0", "image/jpeg"), ("MediaUrl1", "https://api.twilio.com/m/1"), ("OptOutType", "HELP")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let w = Inbound::from_twilio(&f).unwrap();
        assert_eq!(w.provider_message_id.as_deref(), Some("SM1"));
        assert_eq!(w.media.len(), 2);
        assert_eq!(w.media[1].1, None);
        assert_eq!(w.provider_signal.as_deref(), Some("HELP"));
        let status: Vec<(String, String)> = [("MessageSid", "SM1"), ("MessageStatus", "delivered")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert!(Inbound::from_twilio(&status).is_none());
        assert!(Inbound::from_telnyx(&json!({"id": " ", "text": "STOP"})).provider_message_id.is_none());
    }

    #[test]
    fn previews_and_windows() {
        assert_eq!(preview("  hello\n there ", 0), "hello there");
        assert_eq!(preview("", 2), "Sent 2 attachments");
        assert!(preview(&"a".repeat(300), 0).ends_with('…'));
        let now = Utc::now();
        assert!(window_open(Some(now - chrono::Duration::hours(23)), now));
        assert!(!window_open(Some(now - chrono::Duration::hours(25)), now));
        assert!(!window_open(None, now));
    }
}
