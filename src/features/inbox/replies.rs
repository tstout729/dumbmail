//! Replying: who can be answered, replies typed in the dashboard (or sent over the API and MCP),
//! and automatic texts (keyword answers and sign-up texts, the default and away replies, HELP), all
//! sent through `sending::send_conversational` so billing, the provider and bookkeeping are the
//! normal ones, and every one passes `store::claim_send` right before the provider call.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::keywords::{self, Keyword};
use super::settings::{self, Settings};
use super::store::{self, Received, REPLY_WINDOW_HOURS};
use crate::error::{AppError, AppResult};
use crate::models::{Account, Profile, PROFILE_COLS};
use crate::render::SmsContent;
use crate::sending::{self, Conversational, Outcome, SendSpec};
use crate::state::AppState;

pub const MAX_TEXT_CHARS: usize = 1600;
/// Automatic texts to one person per hour, whatever their kind (two bots can't text each other forever).
pub const PER_PERSON_PER_HOUR: i32 = 5;

/// What decides whether a conversation can be answered now.
#[derive(Debug, Clone, Default)]
pub struct ReplyFacts {
    pub consent: String,
    pub phone: Option<String>,
    /// Their number and ours, from their latest text.
    pub target: Option<(String, String)>,
    /// Their number opted out at the carrier level.
    pub opted_out: bool,
    /// Their latest verified text to our number (the start of the reply window).
    pub window_start: Option<DateTime<Utc>>,
    /// Their latest text couldn't be verified.
    pub latest_unverified: bool,
}

impl ReplyFacts {
    fn subscribed_here(&self) -> bool {
        self.consent == "subscribed" && self.target.as_ref().is_some_and(|(theirs, _)| self.phone.as_deref() == Some(theirs))
    }

    /// Why this person can't be texted a reply right now, or None when they can.
    pub fn block(&self, now: DateTime<Utc>) -> Option<String> {
        if self.target.is_none() {
            return Some("They haven't texted you anything you can reply to.".into());
        }
        if self.opted_out {
            return Some("They opted out (texted STOP), so they can't get texts until they text START again.".into());
        }
        if self.consent == "unsubscribed" {
            return Some("They unsubscribed from texts, so they can't get replies.".into());
        }
        if self.subscribed_here() || store::window_open(self.window_start, now) {
            return None;
        }
        if self.window_start.is_none() && self.latest_unverified {
            return Some("Their text couldn't be verified as coming from your texting provider, so you can reply only if they're subscribed. Add the webhook signing key in Settings → Inbox.".into());
        }
        Some(format!(
            "They aren't subscribed to texts and their last message was more than {REPLY_WINDOW_HOURS} hours ago. You can reply within {REPLY_WINDOW_HOURS} hours of their last text."
        ))
    }

    /// When the reply window closes for someone who isn't subscribed (None if it doesn't apply).
    pub fn window_ends(&self) -> Option<DateTime<Utc>> {
        if self.subscribed_here() || self.opted_out || self.consent == "unsubscribed" {
            return None;
        }
        self.window_start.map(|at| at + Duration::hours(REPLY_WINDOW_HOURS))
    }
}

pub async fn reply_facts(db: &sqlx::PgPool, account_id: Uuid, conversation_id: Uuid, profile: &Profile) -> AppResult<ReplyFacts> {
    let target = store::reply_target(db, conversation_id).await?;
    let mut conn = db.acquire().await?;
    let (opted_out, window_start, latest_unverified) = match &target {
        Some((theirs, ours)) => {
            let latest_verified: Option<bool> = sqlx::query_scalar("SELECT verified FROM sms_inbound WHERE conversation_id = $1 ORDER BY provider_received_at DESC, id DESC LIMIT 1")
                .bind(conversation_id)
                .fetch_optional(&mut *conn)
                .await?;
            (store::is_opted_out(&mut conn, account_id, theirs).await?, store::window_start(&mut conn, account_id, theirs, ours).await?, latest_verified == Some(false))
        }
        None => (false, None, false),
    };
    Ok(ReplyFacts { consent: profile.sms_consent.clone(), phone: profile.phone_number.clone(), target, opted_out, window_start, latest_unverified })
}

pub async fn load_profile(db: &sqlx::PgPool, account_id: Uuid, id: Uuid) -> AppResult<Profile> {
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1 AND p.account_id = $2");
    sqlx::query_as::<_, Profile>(&q).bind(id).bind(account_id).fetch_optional(db).await?.ok_or_else(|| AppError::not_found("profile"))
}

fn spec<'a>(account: &'a Account, profile: &'a Profile, content: &'a Value, message_id: Option<Uuid>) -> SendSpec<'a> {
    SendSpec {
        account,
        profile,
        channel: "sms",
        content,
        campaign_id: None,
        flow_id: None,
        flow_node_id: None,
        event: None,
        is_test: false,
        message_id,
        to_override: None,
    }
}

/// Reply to a conversation, to the number that texted, from the number they texted. Without
/// `confirm` it only checks the rules and shows the text as it would go out. The rules are checked
/// again at send time, so a STOP that lands first still wins.
pub async fn send_reply(state: &AppState, account_id: Uuid, conversation_id: Uuid, body: &str, sent_by: Option<Uuid>, confirm: bool) -> AppResult<Value> {
    let body = body.trim();
    if body.is_empty() {
        return Err(AppError::bad("write a reply first"));
    }
    if body.chars().count() > MAX_TEXT_CHARS {
        return Err(AppError::bad(format!("a reply can be at most {MAX_TEXT_CHARS} characters")));
    }
    let profile_id: Option<Uuid> = sqlx::query_scalar("SELECT profile_id FROM sms_conversations WHERE id = $1 AND account_id = $2").bind(conversation_id).bind(account_id).fetch_optional(&state.db).await?;
    let profile = load_profile(&state.db, account_id, profile_id.ok_or_else(|| AppError::not_found("conversation"))?).await?;
    let facts = reply_facts(&state.db, account_id, conversation_id, &profile).await?;
    if let Some(why) = facts.block(Utc::now()) {
        return Err(AppError::Conflict(why));
    }
    let Some((theirs, ours)) = facts.target.clone() else { return Err(AppError::Conflict("They haven't texted you.".into())) };
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
            "dry_run": true, "to": theirs, "from": ours, "body": text, "segments": segments,
            "note": "Nothing sent. Call again with confirm=true to text this person.",
        }));
    }
    let out = sending::send_conversational(state, spec(&account, &profile, &content, None), &Conversational { kind: "reply", sent_by, to: theirs, from: ours }).await?;
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
async fn take_person_slot(conn: &mut PgConnection, conversation_id: Uuid, default_reply: bool) -> AppResult<bool> {
    let ok: Option<Uuid> = sqlx::query_scalar(
        "UPDATE sms_conversations SET
            auto_window_count = CASE WHEN auto_window_start > now() - interval '1 hour' THEN auto_window_count + 1 ELSE 1 END,
            auto_window_start = CASE WHEN auto_window_start > now() - interval '1 hour' THEN auto_window_start ELSE now() END,
            last_default_reply_at = CASE WHEN $2 THEN now() ELSE last_default_reply_at END
         WHERE id = $1
           AND (auto_window_start IS NULL OR auto_window_start <= now() - interval '1 hour' OR auto_window_count < $3)
           AND (NOT $2 OR last_default_reply_at IS NULL OR last_default_reply_at <= now() - interval '12 hours')
         RETURNING id",
    )
    .bind(conversation_id)
    .bind(default_reply)
    .bind(PER_PERSON_PER_HOUR)
    .fetch_optional(conn)
    .await?;
    Ok(ok.is_some())
}

/// Count one automatic text against an hourly budget ('account', or the receiving number).
async fn take_budget(conn: &mut PgConnection, account_id: Uuid, scope: &str, limit: i32) -> AppResult<bool> {
    let n: Option<i32> = sqlx::query_scalar(
        "INSERT INTO sms_auto_budget (account_id, scope, hour, count) SELECT $1, $2, date_trunc('hour', now()), 1 WHERE $3 > 0
         ON CONFLICT (account_id, scope, hour) DO UPDATE SET count = sms_auto_budget.count + 1 WHERE sms_auto_budget.count < $3
         RETURNING count",
    )
    .bind(account_id)
    .bind(scope)
    .bind(limit)
    .fetch_optional(conn)
    .await?;
    Ok(n.is_some())
}

fn static_purpose(p: &str) -> Option<&'static str> {
    ["auto_reply", "keyword_reply", "confirmation", "optin_request", "help"].into_iter().find(|k| *k == p)
}

/// Queue an automatic text answering `rx`: written down first (its id is the message's id, so it
/// can only ever go once), counted against the person's limit and the account's and receiving
/// number's hourly budgets, then sent by a job so the webhook answers right away. Returns None
/// when it won't go (already queued for this text, opted out, a limit or budget reached).
pub async fn queue_auto(state: &AppState, rx: &Received, purpose: &'static str, body: &str, keyword: Option<&Keyword>, context: Value) -> AppResult<Option<Uuid>> {
    if body.trim().is_empty() || rx.from == rx.to || !rx.verified {
        return Ok(None);
    }
    let s = settings::load(&state.db, rx.account_id).await?;
    let id = Uuid::now_v7();
    let mut tx = state.db.begin().await?;
    let help = purpose == "help";
    if help {
        let recent: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM sms_auto_sends WHERE account_id = $1 AND to_number = $2 AND purpose = 'help' AND created_at > now() - interval '1 hour')",
        )
        .bind(rx.account_id)
        .bind(&rx.from)
        .fetch_one(&mut *tx)
        .await?;
        if recent {
            return Ok(None);
        }
    } else if store::is_opted_out(&mut tx, rx.account_id, &rx.from).await? {
        return Ok(None);
    }
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO sms_auto_sends (id, account_id, profile_id, conversation_id, inbound_id, purpose, body, to_number, from_number, keyword_id, keyword_version, context)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         ON CONFLICT (inbound_id, purpose) DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(rx.account_id)
    .bind(rx.profile_id)
    .bind(rx.conversation_id)
    .bind(rx.id)
    .bind(purpose)
    .bind(body)
    .bind(&rx.from)
    .bind(&rx.to)
    .bind(keyword.map(|k| k.id))
    .bind(keyword.map(|k| k.version))
    .bind(&context)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        // A retry of a text already answered.
        return Ok(None);
    }
    if !help && !take_person_slot(&mut tx, rx.conversation_id, purpose == "auto_reply").await? {
        tracing::info!(conversation = %rx.conversation_id, purpose, "automatic text limit for this person reached; not sending");
        return Ok(None);
    }
    for (scope, limit) in [("account", s.auto_budget_account), (rx.to.as_str(), s.auto_budget_number)] {
        if !take_budget(&mut tx, rx.account_id, scope, limit).await? {
            tx.rollback().await?;
            budget_hit(state, rx.account_id, scope).await?;
            return Ok(None);
        }
    }
    sqlx::query("INSERT INTO messages (id, account_id, profile_id, channel, to_address) VALUES ($1, $2, $3, 'sms', $4)")
        .bind(id)
        .bind(rx.account_id)
        .bind(rx.profile_id)
        .bind(&rx.from)
        .execute(&mut *tx)
        .await?;
    crate::jobs::enqueue(&mut *tx, "inbox_auto_send", json!({"id": id}), None).await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// An hourly budget ran out: automatic texts stop for the rest of the hour, and the dashboard says so.
async fn budget_hit(state: &AppState, account_id: Uuid, scope: &str) -> AppResult<()> {
    tracing::warn!(account = %account_id, scope, "hourly budget for automatic texts reached; automatic texts paused for the hour");
    sqlx::query(
        "INSERT INTO sms_inbox_settings (account_id, auto_budget_hit_at, auto_budget_hit_scope, notified_through) VALUES ($1, now(), $2, now())
         ON CONFLICT (account_id) DO UPDATE SET auto_budget_hit_at = now(), auto_budget_hit_scope = $2",
    )
    .bind(account_id)
    .bind(scope)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The fixed, non-promotional answer to HELP and INFO.
pub fn help_text(s: &Settings, account: &Account) -> String {
    let program = if s.program_name.is_empty() { account.name.clone() } else { s.program_name.clone() };
    let contact = Some(s.support_contact.clone()).filter(|c| !c.is_empty()).or_else(|| account.reply_to.clone()).or_else(|| account.from_email.clone());
    match contact {
        Some(c) => format!("{program}: For help, contact {c}. Msg & data rates may apply. Reply STOP to cancel."),
        None => format!("{program}: For help, reply to this message. Msg & data rates may apply. Reply STOP to cancel."),
    }
}

/// Whether DumbMail should answer HELP itself for this text. `auto` follows the providers' docs:
/// Twilio answers HELP itself on long codes and toll-free numbers (the only kinds DumbMail uses);
/// Telnyx documents no default HELP answer for them, so DumbMail answers. Either way, a text the
/// provider says it already answered as HELP isn't answered twice.
pub fn dumbmail_answers_help(mode: &str, provider: &str, provider_signal: Option<&str>) -> bool {
    match mode {
        "dumbmail" => true,
        "provider" => false,
        _ => provider != "twilio" && provider_signal != Some("HELP"),
    }
}

pub async fn help_reply(state: &AppState, rx: &Received) -> AppResult<()> {
    let s = settings::load(&state.db, rx.account_id).await?;
    if !dumbmail_answers_help(&s.help_replies, &rx.provider, rx.provider_signal.as_deref()) {
        return Ok(());
    }
    let account = crate::campaigns::load_account(&state.db, rx.account_id).await?;
    queue_auto(state, rx, "help", &help_text(&s, &account), None, json!({})).await.map(|_| ())
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
    queue_auto(state, rx, "auto_reply", body, None, json!({})).await.map(|_| ())
}

/// One automatic text, as written down by [`queue_auto`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AutoSend {
    pub id: Uuid,
    pub account_id: Uuid,
    pub profile_id: Uuid,
    pub inbound_id: Uuid,
    pub purpose: String,
    pub body: String,
    pub to_number: String,
    pub from_number: String,
    pub keyword_id: Option<Uuid>,
    pub keyword_version: Option<i32>,
    pub context: Value,
    pub status: String,
}

const AUTO_COLS: &str = "id, account_id, profile_id, inbound_id, purpose, body, to_number, from_number, keyword_id, keyword_version, context, status";

async fn finish(state: &AppState, id: Uuid, status: &str, error: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("UPDATE sms_auto_sends SET status = $2, error = $3, finished_at = now() WHERE id = $1").bind(id).bind(status).bind(error).execute(&state.db).await?;
    if status != "sent" {
        sqlx::query("UPDATE messages SET status = 'skipped', error = $2 WHERE id = $1 AND status = 'queued'").bind(id).bind(error.unwrap_or(status)).execute(&state.db).await?;
    }
    Ok(())
}

/// What happens once the provider accepted an automatic text: a sign-up confirmation subscribes
/// them, a double opt-in prompt starts its 7 days. Safe to run again.
async fn after_sent(state: &AppState, a: &AutoSend) -> anyhow::Result<()> {
    let e = |e: AppError| anyhow::anyhow!(e.public_message());
    match a.purpose.as_str() {
        "confirmation" => keywords::subscribed(state, a, a.id).await.map_err(e)?,
        "optin_request" => keywords::prompt_sent(state, a).await.map_err(e)?,
        _ => {}
    }
    finish(state, a.id, "sent", None).await
}

/// Job: send one automatic text, at most once. The rules (opt-out, reply window, the number still
/// being the account's) are checked again at send time.
pub async fn run_auto_send(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let q = format!("SELECT {AUTO_COLS} FROM sms_auto_sends WHERE id = $1");
    let Some(row) = sqlx::query_as::<_, AutoSend>(&q).bind(id).fetch_optional(&state.db).await? else { return Ok(()) };
    match row.status.as_str() {
        "queued" => {}
        // A job that stopped part way: send nothing again. If the provider took it, finish the bookkeeping.
        "sending" => {
            let status: Option<String> = sqlx::query_scalar("SELECT status FROM messages WHERE id = $1").bind(id).fetch_optional(&state.db).await?;
            return match status.as_deref() {
                Some("sent" | "delivered") => after_sent(state, &row).await,
                _ => finish(state, id, "failed", Some("interrupted before the provider answered; not sent again")).await,
            };
        }
        _ => return Ok(()),
    }
    let q = format!("UPDATE sms_auto_sends SET status = 'sending' WHERE id = $1 AND status = 'queued' RETURNING {AUTO_COLS}");
    let Some(row) = sqlx::query_as::<_, AutoSend>(&q).bind(id).fetch_optional(&state.db).await? else { return Ok(()) };
    let Some(kind) = static_purpose(&row.purpose) else { return finish(state, id, "failed", Some("unknown kind")).await };
    let Ok(profile) = load_profile(&state.db, row.account_id, row.profile_id).await else { return finish(state, id, "skipped", Some("profile deleted")).await };
    let account = crate::campaigns::load_account(&state.db, row.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let content = json!({"body": row.body});
    let out = sending::send_conversational(
        state,
        spec(&account, &profile, &content, Some(id)),
        &Conversational { kind, sent_by: None, to: row.to_number.clone(), from: row.from_number.clone() },
    )
    .await;
    // No texting number yet, a template error or an empty balance won't fix itself on a retry.
    match out {
        Ok(Outcome::Sent(_)) => after_sent(state, &row).await,
        Ok(Outcome::Skipped(why)) => {
            tracing::info!(auto = %id, why, "automatic text not sent");
            finish(state, id, "skipped", Some(why)).await
        }
        Ok(Outcome::Failed(why)) => finish(state, id, "failed", Some(&why)).await,
        Ok(Outcome::Deferred(_)) => finish(state, id, "failed", Some("the provider was busy")).await,
        Err(e) => {
            tracing::warn!(auto = %id, error = %e.public_message(), "automatic text not sent");
            finish(state, id, "failed", Some(&e.public_message())).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(consent: &str, window_hours_ago: Option<i64>) -> ReplyFacts {
        ReplyFacts {
            consent: consent.into(),
            phone: Some("+15035550199".into()),
            target: Some(("+15035550199".into(), "+18335550100".into())),
            opted_out: false,
            window_start: window_hours_ago.map(|h| Utc::now() - Duration::hours(h)),
            latest_unverified: false,
        }
    }

    #[test]
    fn reply_rules() {
        let now = Utc::now();
        assert!(facts("never_subscribed", Some(2)).block(now).is_none());
        assert!(facts("never_subscribed", Some(30)).block(now).unwrap().contains("24 hours"));
        assert!(facts("subscribed", Some(30)).block(now).is_none());
        assert!(facts("subscribed", None).block(now).is_none());
        assert!(facts("unsubscribed", Some(2)).block(now).is_some());
        let mut f = facts("subscribed", Some(1));
        f.opted_out = true;
        assert!(f.block(now).unwrap().contains("STOP"));
        // Subscribed on another number (they edited the profile's phone): only the window counts.
        let mut f = facts("subscribed", Some(30));
        f.phone = Some("+15035550000".into());
        assert!(f.block(now).is_some());
        let mut f = facts("never_subscribed", None);
        f.latest_unverified = true;
        assert!(f.block(now).unwrap().contains("verified"));
        assert!(facts("never_subscribed", Some(2)).window_ends().is_some());
        assert_eq!(facts("subscribed", Some(2)).window_ends(), None);
        assert!(ReplyFacts::default().block(now).is_some());
    }

    #[test]
    fn help_answers_follow_each_provider() {
        assert!(dumbmail_answers_help("auto", "telnyx", None));
        assert!(!dumbmail_answers_help("auto", "telnyx", Some("HELP")), "Telnyx already answered it");
        assert!(!dumbmail_answers_help("auto", "twilio", None), "Twilio answers HELP on long codes and toll-free");
        assert!(dumbmail_answers_help("dumbmail", "twilio", Some("HELP")));
        assert!(!dumbmail_answers_help("provider", "telnyx", None));
    }
}
