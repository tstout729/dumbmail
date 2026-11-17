//! Replying: who can be answered, replies typed in the dashboard (or sent over the API and MCP),
//! and automatic replies (keywords, the default reply, the away reply), all sent through
//! `sending::send_conversational` so billing, the provider and bookkeeping are the normal ones.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use super::settings;
use super::store::Received;
use crate::error::{AppError, AppResult};
use crate::models::{Profile, PROFILE_COLS};
use crate::render::SmsContent;
use crate::sending::{self, Conversational, Outcome, SendSpec};
use crate::state::AppState;

/// Someone not subscribed to marketing texts can be answered this long after their last text.
pub const REPLY_WINDOW_HOURS: i64 = 24;
pub const MAX_TEXT_CHARS: usize = 1600;

/// Why this person can't be texted a reply right now, or None when they can.
pub fn reply_block(p: &Profile, last_inbound_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    if p.phone_number.is_none() {
        return Some("This person has no phone number.".into());
    }
    if p.sms_consent == "unsubscribed" {
        return Some("They texted STOP, so they can't get texts until they text START again.".into());
    }
    if p.sms_consent == "subscribed" {
        return None;
    }
    match last_inbound_at {
        Some(at) if now - at < Duration::hours(REPLY_WINDOW_HOURS) => None,
        Some(_) => Some(format!(
            "They aren't subscribed to texts and their last message was more than {REPLY_WINDOW_HOURS} hours ago. You can reply within {REPLY_WINDOW_HOURS} hours of their last text."
        )),
        None => Some("They aren't subscribed to texts and haven't texted you.".into()),
    }
}

/// When the reply window closes for someone who isn't subscribed (None if it doesn't apply).
pub fn window_ends(p: &Profile, last_inbound_at: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    if p.sms_consent == "subscribed" || p.sms_consent == "unsubscribed" {
        return None;
    }
    last_inbound_at.map(|at| at + Duration::hours(REPLY_WINDOW_HOURS))
}

pub async fn load_profile(db: &sqlx::PgPool, account_id: Uuid, id: Uuid) -> AppResult<Profile> {
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1 AND p.account_id = $2");
    sqlx::query_as::<_, Profile>(&q).bind(id).bind(account_id).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("profile"))
}

/// Reply to a conversation. Without `confirm` it only checks the rules and shows the text as it
/// would go out (personalization filled in, sender name and STOP line added where required).
pub async fn send_reply(state: &AppState, account_id: Uuid, conversation_id: Uuid, body: &str, sent_by: Option<Uuid>, confirm: bool) -> AppResult<Value> {
    let body = body.trim();
    if body.is_empty() {
        return Err(AppError::bad("write a reply first"));
    }
    if body.chars().count() > MAX_TEXT_CHARS {
        return Err(AppError::bad(format!("a reply can be at most {MAX_TEXT_CHARS} characters")));
    }
    let row: Option<(Uuid, Option<DateTime<Utc>>)> = sqlx::query_as("SELECT profile_id, last_inbound_at FROM sms_conversations WHERE id = $1 AND account_id = $2")
        .bind(conversation_id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let (profile_id, last_inbound_at) = row.ok_or_else(|| AppError::not_found("conversation"))?;
    let profile = load_profile(&state.db, account_id, profile_id).await?;
    if let Some(why) = reply_block(&profile, last_inbound_at, Utc::now()) {
        return Err(AppError::Conflict(why));
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let content = json!({"body": body});
    if !confirm {
        let ctx = crate::render::context(&account, Some(&profile), None, "");
        let sms = SmsContent { body: body.to_string(), media_url: None };
        let prior: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE profile_id = $1 AND channel = 'sms' AND status IN ('sent','delivered'))")
            .bind(profile.id)
            .fetch_one(&state.db)
            .await?;
        let text = crate::render::render_sms(&account, &sms, &ctx, None, !prior)?;
        let (_, segments) = crate::providers::sms::segment_count(&text);
        return Ok(json!({
            "dry_run": true, "to": profile.phone_number, "body": text, "segments": segments,
            "note": "Nothing sent. Call again with confirm=true to text this person.",
        }));
    }
    let out = sending::send_conversational(
        state,
        SendSpec {
            account: &account,
            profile: &profile,
            channel: "sms",
            content: &content,
            campaign_id: None,
            flow_id: None,
            flow_node_id: None,
            event: None,
            is_test: false,
            message_id: None,
            to_override: None,
        },
        &Conversational { kind: "reply", sent_by },
    )
    .await?;
    match out {
        Outcome::Sent(message_id) => Ok(json!({"sent": true, "message_id": message_id})),
        Outcome::Skipped("insufficient credit") => Err(AppError::LimitReached("your balance is empty; add credit in Settings → Billing".into())),
        Outcome::Skipped(why) => Err(AppError::Conflict(format!("not sent: {why}"))),
        Outcome::Failed(why) => Err(AppError::BadRequest(format!("the text didn't send: {why}"))),
        Outcome::Deferred(_) => Err(AppError::Conflict("the text couldn't send right now; try again in a minute".into())),
    }
}

/// Count one automatic text against this person's limits (at most 5 an hour; default and away
/// replies at most once every 12 hours). Returns false when the text must not go.
async fn take_auto_slot(db: &sqlx::PgPool, conversation_id: Uuid, default_reply: bool) -> AppResult<bool> {
    let ok: Option<Uuid> = sqlx::query_scalar(
        "UPDATE sms_conversations SET
            auto_window_count = CASE WHEN auto_window_start > now() - interval '1 hour' THEN auto_window_count + 1 ELSE 1 END,
            auto_window_start = CASE WHEN auto_window_start > now() - interval '1 hour' THEN auto_window_start ELSE now() END,
            last_default_reply_at = CASE WHEN $2 THEN now() ELSE last_default_reply_at END
         WHERE id = $1
           AND (auto_window_start IS NULL OR auto_window_start <= now() - interval '1 hour' OR auto_window_count < 5)
           AND (NOT $2 OR last_default_reply_at IS NULL OR last_default_reply_at <= now() - interval '12 hours')
         RETURNING id",
    )
    .bind(conversation_id)
    .bind(default_reply)
    .fetch_optional(db)
    .await?;
    Ok(ok.is_some())
}

/// Queue an automatic text to the sender of `rx`, within the limits above. Sent by a job so the
/// provider's webhook gets its answer right away.
pub async fn queue_auto(state: &AppState, rx: &Received, body: &str, default_reply: bool) -> AppResult<()> {
    if body.trim().is_empty() || rx.from == rx.to {
        return Ok(());
    }
    if !take_auto_slot(&state.db, rx.conversation_id, default_reply).await? {
        tracing::info!(conversation = %rx.conversation_id, "automatic reply limit reached; not replying");
        return Ok(());
    }
    crate::jobs::enqueue(&state.db, "inbox_auto_reply", json!({"account_id": rx.account_id, "profile_id": rx.profile_id, "body": body}), None).await?;
    Ok(())
}

/// The default reply for texts no keyword matched, or the away reply outside opening hours.
pub async fn default_reply(state: &AppState, rx: &Received) -> AppResult<()> {
    let s = settings::load(&state.db, rx.account_id).await?;
    if !s.default_reply_enabled && !s.away_reply_enabled {
        return Ok(());
    }
    let account = crate::campaigns::load_account(&state.db, rx.account_id).await?;
    let body = if s.away_reply_enabled && !s.is_open(&account, Utc::now()) {
        &s.away_reply
    } else if s.default_reply_enabled {
        &s.default_reply
    } else {
        return Ok(());
    };
    queue_auto(state, rx, body, true).await
}

/// Job: send one automatic text. The person's consent is read again now; anyone who texted STOP
/// since is skipped by `send_conversational`.
pub async fn run_auto_reply(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let id = |k: &str| payload[k].as_str().and_then(|s| s.parse::<Uuid>().ok()).ok_or_else(|| anyhow::anyhow!("job payload missing {k}"));
    let (account_id, profile_id) = (id("account_id")?, id("profile_id")?);
    let body = payload["body"].as_str().unwrap_or_default();
    let Ok(profile) = load_profile(&state.db, account_id, profile_id).await else { return Ok(()) };
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let out = sending::send_conversational(
        state,
        SendSpec {
            account: &account,
            profile: &profile,
            channel: "sms",
            content: &json!({"body": body}),
            campaign_id: None,
            flow_id: None,
            flow_node_id: None,
            event: None,
            is_test: false,
            message_id: None,
            to_override: None,
        },
        &Conversational { kind: "auto_reply", sent_by: None },
    )
    .await;
    // No texting number yet, a template error or an empty balance won't fix itself on a retry.
    match out {
        Ok(Outcome::Sent(_)) => {}
        Ok(other) => tracing::info!(profile = %profile_id, outcome = ?other, "automatic reply not sent"),
        Err(e) => tracing::warn!(profile = %profile_id, error = %e.public_message(), "automatic reply not sent"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(consent: &str) -> Profile {
        Profile {
            id: Uuid::nil(),
            account_id: Uuid::nil(),
            email: None,
            phone_number: Some("+15035550199".into()),
            external_id: None,
            first_name: None,
            last_name: None,
            properties: json!({}),
            email_consent: "never_subscribed".into(),
            sms_consent: consent.into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_event_at: None,
        }
    }

    #[test]
    fn reply_rules() {
        let now = Utc::now();
        let recent = Some(now - Duration::hours(2));
        let old = Some(now - Duration::hours(30));
        assert!(reply_block(&person("never_subscribed"), recent, now).is_none());
        assert!(reply_block(&person("never_subscribed"), old, now).is_some());
        assert!(reply_block(&person("subscribed"), old, now).is_none());
        assert!(reply_block(&person("unsubscribed"), recent, now).unwrap().contains("STOP"));
        assert!(reply_block(&person("pending"), recent, now).is_none());
        assert_eq!(window_ends(&person("never_subscribed"), recent), recent.map(|t| t + Duration::hours(24)));
        assert_eq!(window_ends(&person("subscribed"), recent), None);
    }
}
