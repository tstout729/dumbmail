//! An account's standing, the screenings that decided it, and what happens when it changes.

use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::Verdict;
use crate::deliverability::plain;
use crate::models::Account;
use crate::providers::email::OutgoingEmail;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Standing {
    /// New: checked before its first real send.
    Pending,
    Approved,
    /// A person is checking it.
    Review,
    Blocked,
}

impl Standing {
    pub fn as_str(self) -> &'static str {
        match self {
            Standing::Pending => "pending",
            Standing::Approved => "approved",
            Standing::Review => "review",
            Standing::Blocked => "blocked",
        }
    }

    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("pending") => Standing::Pending,
            Some("review") => Standing::Review,
            Some("blocked") => Standing::Blocked,
            _ => Standing::Approved,
        }
    }
}

/// The account's standing as sending sees it (always approved when screening is off).
pub fn of(state: &AppState, account: &Account) -> Standing {
    if !super::on(state) {
        return Standing::Approved;
    }
    Standing::parse(account.sending_review.as_deref())
}

pub async fn read(state: &AppState, account_id: Uuid) -> sqlx::Result<Standing> {
    if !super::on(state) {
        return Ok(Standing::Approved);
    }
    let s: Option<String> = sqlx::query_scalar("SELECT sending_review FROM accounts WHERE id = $1").bind(account_id).fetch_optional(&state.db).await?;
    Ok(Standing::parse(s.as_deref()))
}

pub const PENDING_MESSAGE: &str = "Your account is being checked before its first send. This usually takes a few minutes.";
pub const REVIEW_MESSAGE: &str = "Your account is being checked before it can send again: a person is checking it, and we'll email you.";
pub const BLOCKED_MESSAGE: &str = "This account can't send marketing email or texts. If you think this is a mistake, reply to our email or contact support.";
pub const ANSWERS_NEEDED: &str = "Before your first send, tell us about your business: what it does, how the people on your list signed up, and how many emails you expect to send a month (Settings → Sending → Account review).";

/// What a customer is told when their standing holds a send.
pub fn hold_message(s: Standing) -> &'static str {
    match s {
        Standing::Pending => PENDING_MESSAGE,
        Standing::Review => REVIEW_MESSAGE,
        Standing::Blocked => BLOCKED_MESSAGE,
        Standing::Approved => "",
    }
}

/// A screening to record.
pub struct New<'a> {
    pub kind: &'static str,
    pub subject: Option<&'static str>,
    pub subject_id: Option<Uuid>,
    pub subject_name: Option<String>,
    pub verdict: Verdict,
    pub reasons: &'a [String],
    pub signals: Value,
    pub source: &'static str,
    pub model: Option<String>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub content_hash: Option<String>,
    /// Each message's own hash (see `content`).
    pub part_hashes: Vec<String>,
}

impl<'a> New<'a> {
    pub fn rules(kind: &'static str, verdict: Verdict, reasons: &'a [String], signals: Value) -> Self {
        Self {
            kind,
            subject: None,
            subject_id: None,
            subject_name: None,
            verdict,
            reasons,
            signals,
            source: "rules",
            model: None,
            input_tokens: None,
            output_tokens: None,
            content_hash: None,
            part_hashes: Vec::new(),
        }
    }

    /// Mark it as decided by the model (with its name and token counts).
    pub fn judged(mut self, j: &super::ai::Judgment) -> Self {
        self.source = "ai";
        self.model = Some(j.model.clone());
        self.input_tokens = j.input_tokens;
        self.output_tokens = j.output_tokens;
        self
    }
}

pub async fn record(db: &sqlx::PgPool, account_id: Uuid, s: New<'_>) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    let subject_name = s.subject_name.map(|n| plain(&n, 200));
    sqlx::query(
        "INSERT INTO screenings (id, account_id, kind, subject, subject_id, subject_name, verdict, reasons, signals, source, model, input_tokens, output_tokens, content_hash, part_hashes)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(id)
    .bind(account_id)
    .bind(s.kind)
    .bind(s.subject)
    .bind(s.subject_id)
    .bind(subject_name)
    .bind(s.verdict.as_str())
    .bind(json!(s.reasons))
    .bind(s.signals)
    .bind(s.source)
    .bind(s.model)
    .bind(s.input_tokens)
    .bind(s.output_tokens)
    .bind(s.content_hash)
    .bind(s.part_hashes)
    .execute(db)
    .await?;
    Ok(id)
}

/// Move the account to `to` if it's now one of `from`, with `note` (shown to the customer).
/// `why` is the operator's version. Returns the standing it moved from, or None when nothing
/// changed. The alert, the owner's email and stopping sends happen after the write commits.
pub async fn move_to(state: &AppState, account_id: Uuid, from: &[Standing], to: Standing, note: &str, why: &str) -> anyhow::Result<Option<Standing>> {
    let from_s: Vec<&str> = from.iter().filter(|s| **s != to).map(|s| s.as_str()).collect();
    let note = plain(note, 600);
    let old: Option<(String, String)> = sqlx::query_as(
        "UPDATE accounts a SET sending_review = $2, sending_review_at = now(), sending_review_note = NULLIF($3, '')
         FROM (SELECT id, sending_review FROM accounts WHERE id = $1 FOR UPDATE) old
         WHERE a.id = old.id AND old.sending_review = ANY($4) RETURNING old.sending_review, a.name",
    )
    .bind(account_id)
    .bind(to.as_str())
    .bind(&note)
    .bind(&from_s)
    .fetch_optional(&state.db)
    .await?;
    let Some((old, name)) = old else { return Ok(None) };
    let old = Standing::parse(Some(&old));
    after_move(state, account_id, &name, old, to, &note, why).await?;
    Ok(Some(old))
}

/// What follows a change of standing (after it committed): campaigns stopped, waiting flow
/// messages released, the owner told, the operator alerted.
pub async fn after_move(state: &AppState, account_id: Uuid, name: &str, old: Standing, to: Standing, note: &str, why: &str) -> anyhow::Result<()> {
    tracing::warn!(account = %account_id, from = old.as_str(), to = to.as_str(), "sending review changed");
    match to {
        Standing::Approved => {
            sqlx::query("DELETE FROM screening_waits WHERE account_id = $1").bind(account_id).execute(&state.db).await?;
            notify_owner(
                state,
                account_id,
                "Your DumbMail account is approved to send",
                "Good news: we checked your account and it's approved to send. Campaigns you launch now go out, and flow messages that were waiting are on their way.\n\nThanks for sending only to people who asked to hear from you.\n\nThe DumbMail team",
            )
            .await;
        }
        Standing::Blocked => {
            stop_sends(state, account_id, true).await?;
            let reasons = if note.is_empty() { String::new() } else { format!("Why: {note}\n\n") };
            notify_owner(
                state,
                account_id,
                "Your DumbMail account can't send",
                &format!(
                    "We checked your account and it can't send marketing email or texts through DumbMail.\n\n{reasons}Campaigns in progress were stopped and flow messages won't go out. You can still see your billing, export your data or close the account.\n\nIf you think this is a mistake, reply to this email and a person will look at it.\n\nThe DumbMail team"
                ),
            )
            .await;
            alert_operator(state, account_id, name, to, why).await;
        }
        Standing::Review => {
            stop_sends(state, account_id, false).await?;
            alert_operator(state, account_id, name, to, why).await;
        }
        Standing::Pending => {}
    }
    Ok(())
}

/// Stop the account's campaigns (email and text): those sending, and when `all` scheduled ones
/// too. Their queued messages are skipped with the reason, never dropped.
async fn stop_sends(state: &AppState, account_id: Uuid, all: bool) -> anyhow::Result<()> {
    let (msg, reason) = if all { (BLOCKED_MESSAGE, "account blocked") } else { (REVIEW_MESSAGE, "account not approved") };
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM campaigns WHERE account_id = $1 AND channel IN ('email', 'sms') AND (status IN ('sending', 'testing') OR ($2 AND status = 'scheduled'))",
    )
    .bind(account_id)
    .bind(all)
    .fetch_all(&state.db)
    .await?;
    for id in ids {
        stop_campaign(&state.db, id, msg, reason).await?;
    }
    Ok(())
}

/// Fail a campaign that can't send because of the account's standing; queued messages are
/// skipped with `reason`. Stopped first in the same transaction (as `deliverability::stop_campaign`).
pub async fn stop_campaign(db: &sqlx::PgPool, campaign_id: Uuid, msg: &str, reason: &str) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    let account: Option<Uuid> =
        sqlx::query_scalar("UPDATE campaigns SET status = 'failed', error = $2, updated_at = now() WHERE id = $1 AND status IN ('sending','scheduled','testing') RETURNING account_id")
            .bind(campaign_id)
            .bind(msg)
            .fetch_optional(&mut *tx)
            .await?;
    let skipped: Vec<Uuid> = sqlx::query_scalar("UPDATE messages SET status = 'skipped', error = $2 WHERE campaign_id = $1 AND status = 'queued' AND NOT is_test RETURNING id")
        .bind(campaign_id)
        .bind(reason)
        .fetch_all(&mut *tx)
        .await?;
    if let Some(a) = account {
        crate::features::messages_unsent_in(&mut tx, db, a, &skipped).await;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn owner_email(db: &sqlx::PgPool, account_id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 ORDER BY (m.role = 'owner') DESC, m.created_at LIMIT 1")
        .bind(account_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
}

/// Email the account's owner from SUPPORT_EMAIL (the platform's own address). Failures are logged.
pub async fn notify_owner(state: &AppState, account_id: Uuid, subject: &str, text: &str) {
    let Some(from) = state.cfg.support_email.as_deref().filter(|s| s.contains('@')) else {
        tracing::warn!(account = %account_id, subject, "no SUPPORT_EMAIL set; sending review notice not emailed");
        return;
    };
    let Some(to) = owner_email(&state.db, account_id).await else { return };
    let html = format!(
        "<div style=\"font-family:system-ui,sans-serif;font-size:15px;line-height:1.5;white-space:pre-wrap\">{}</div>",
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    );
    let email = OutgoingEmail {
        message_id: Uuid::now_v7(),
        to,
        from_name: "DumbMail".into(),
        from_email: from.to_string(),
        reply_to: Some(from.to_string()),
        subject: subject.to_string(),
        html,
        text: text.to_string(),
        unsubscribe_url: None,
    };
    if let Err(e) = state.email.send(&email).await {
        tracing::error!(account = %account_id, error = %format!("{e:#}"), "couldn't email the sending review notice");
    }
}

/// Tell the operator an account needs a person (or was blocked). Tenant text is flattened.
pub async fn alert_operator(state: &AppState, account_id: Uuid, name: &str, to: Standing, why: &str) {
    let owner = owner_email(&state.db, account_id).await;
    let text = format!(
        "An account's sending review changed to {}.\n\nAccount name: {}\nAccount id: {account_id}\nOwner email: {}\nWhy: {}\n\n\
         See every account waiting on a person, with its screenings:\n\n  curl {base}/admin/screening -H \"Authorization: Bearer $ADMIN_TOKEN\"\n\n\
         Decide (approve also lifts an email pause):\n\n  curl -X POST {base}/admin/screening/decide -H \"Authorization: Bearer $ADMIN_TOKEN\" -H 'Content-Type: application/json' -d '{{\"account_id\":\"{account_id}\",\"decision\":\"approve\",\"note\":\"\"}}'\n",
        to.as_str(),
        plain(name, 120),
        plain(owner.as_deref().unwrap_or("(unknown)"), 254),
        plain(why, 1500),
        base = state.cfg.public_url()
    );
    let subject = match to {
        Standing::Blocked => format!("Sender blocked: {}", plain(name, 60)),
        _ => format!("Sender needs review: {}", plain(name, 60)),
    };
    crate::deliverability::email_operator(state, account_id, &subject, text).await;
}

/// Alert the operator about a held campaign or flow (the account's standing doesn't change).
pub async fn alert_held(state: &AppState, account_id: Uuid, what: &str, screening_id: Uuid, why: &str) {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1").bind(account_id).fetch_optional(&state.db).await.ok().flatten();
    let text = format!(
        "Content was held for a person's check.\n\nAccount name: {}\nAccount id: {account_id}\nWhat: {}\nWhy: {}\n\n\
         If it's fine, let it go out (the customer launches it again):\n\n  curl -X POST {}/admin/screening/pass -H \"Authorization: Bearer $ADMIN_TOKEN\" -H 'Content-Type: application/json' -d '{{\"screening_id\":\"{screening_id}\"}}'\n",
        plain(name.as_deref().unwrap_or(""), 120),
        plain(what, 200),
        plain(why, 1500),
        state.cfg.public_url()
    );
    crate::deliverability::email_operator(state, account_id, &format!("Content held: {}", plain(name.as_deref().unwrap_or(""), 60)), text).await;
}
