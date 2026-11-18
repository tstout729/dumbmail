//! Notification steps: email the team (or a confirmed outside address) when someone reaches a step.
//!
//! A `notify` step emails up to five addresses: fixed ones (`to`) and/or the address in one of the
//! person's profile properties (`to_property`). Only the account's team members' addresses and
//! outside addresses that confirmed get anything. Saving a flow only records an outside address as
//! unconfirmed; the confirmation link goes out only when someone who can change account settings
//! asks for it (and never while sending is paused). A notification is a service message from the
//! account's sender: no unsubscribe footer, a plain line saying who set it up, billed like any email,
//! counted in the account's daily allowance, held to the sending rate and the account's pause, and
//! capped per flow per local day (`notify_daily_cap`, default 200, at most 2,000). Runs started by an
//! event from the website script get no notification unless the step sets `allow_browser_events`. A
//! hard bounce or complaint from a notification address blocks it (an Admin can clear a bounce).
//!
//! Exactly once: the first time the step runs for a run, its recipients and rendered message are
//! stored (`pending`), with the day's count; a retry uses that list. Each send is marked `handing`
//! just before the provider call: a crash before that leaves it `pending` and it's sent on the retry;
//! a crash after it makes it `unconfirmed` (never sent again, billed since it may have gone out).

use std::collections::HashSet;

use chrono::Utc;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::flows::Node;
use crate::models::{normalize_email, Account, FlowRow, Profile};
use crate::providers::email::OutgoingEmail;
use crate::state::AppState;

pub const MAX_RECIPIENTS: usize = 5;
pub const DEFAULT_DAILY_CAP: i64 = 200;
pub const MAX_DAILY_CAP: i64 = 2_000;
/// Outside addresses an account may have waiting for confirmation at once.
pub const MAX_PENDING: i64 = 20;
/// Confirmation emails an account may send in 24 hours (and each address gets at most one a day).
pub const MAX_CONFIRMATIONS_PER_DAY: i64 = 50;
const SUBJECT_MAX: usize = 200;
const BODY_MAX: usize = 5_000;
/// How long a confirmation link works.
pub const LINK_DAYS: i64 = 7;
pub const CONFIRM_JOB: &str = "flows_plus_confirm_address";

#[derive(Debug, Clone)]
pub struct Step {
    pub to: Vec<String>,
    pub to_property: Option<String>,
    pub subject: String,
    pub body: String,
    pub allow_browser_events: bool,
}

fn property_name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '$' | '.'))
}

pub fn read(n: &Node) -> AppResult<Step> {
    let bad = |m: &str| AppError::bad(format!("notification {}: {m}", n.id));
    let to: Vec<String> = match n.data.get("to") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().and_then(normalize_email).ok_or_else(|| bad(&format!("{v} isn't an email address"))))
            .collect::<AppResult<_>>()?,
        Some(_) => return Err(bad("`to` must be a list of email addresses")),
    };
    let to_property = match n.data.get("to_property") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) if property_name_ok(s.trim()) => Some(s.trim().to_string()),
        Some(_) => return Err(bad("`to_property` must be the name of a profile property")),
    };
    let unique: HashSet<&String> = to.iter().collect();
    let count = unique.len() + usize::from(to_property.is_some());
    if count == 0 {
        return Err(bad("add at least one address (`to`) or a profile property that holds one (`to_property`)"));
    }
    if count > MAX_RECIPIENTS {
        return Err(bad(&format!("at most {MAX_RECIPIENTS} recipients")));
    }
    let text = |k: &str, max: usize| -> AppResult<String> {
        let s = n.data.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        if s.is_empty() {
            return Err(bad(&format!("needs a `{k}`")));
        }
        if s.chars().count() > max {
            return Err(bad(&format!("`{k}` can be at most {max} characters")));
        }
        Ok(s)
    };
    let (subject, body) = (text("subject", SUBJECT_MAX)?, text("body", BODY_MAX)?);
    let allow_browser_events = match n.data.get("allow_browser_events") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad("`allow_browser_events` must be true or false")),
    };
    Ok(Step { to, to_property, subject, body, allow_browser_events })
}

/// Check a notification step when a flow is saved: recipients, lengths, no coupon codes, and
/// templates that compile.
pub fn validate(n: &Node) -> AppResult<()> {
    let s = read(n)?;
    for (k, v) in [("subject", &s.subject), ("body", &s.body)] {
        if crate::features::coupons::template::may_use_coupons(&json!(v)) {
            return Err(AppError::bad(format!("notification {}: coupon codes can't go in notifications", n.id)));
        }
        crate::render::render_str(v, &json!({"person": {"properties": {}}, "event": {"properties": {}}}), false)
            .map_err(|e| AppError::bad(format!("notification {} {k}: {}", n.id, e.public_message())))?;
    }
    Ok(())
}

/// The flow's daily cap on notifications.
pub fn daily_cap(raw_definition: &Value) -> i64 {
    raw_definition.pointer("/settings/notify_daily_cap").and_then(Value::as_i64).unwrap_or(DEFAULT_DAILY_CAP).clamp(1, MAX_DAILY_CAP)
}

pub fn validate_settings(raw_definition: &Value) -> AppResult<()> {
    match raw_definition.pointer("/settings/notify_daily_cap") {
        None | Some(Value::Null) => Ok(()),
        Some(v) if v.as_i64().is_some_and(|c| (1..=MAX_DAILY_CAP).contains(&c)) => Ok(()),
        Some(_) => Err(AppError::bad(format!("settings.notify_daily_cap must be a whole number from 1 to {MAX_DAILY_CAP}"))),
    }
}

// ---------- Who may receive them ----------

/// Of `emails`, the ones that are this account's team members' addresses.
pub async fn team_addresses(db: &PgPool, account_id: Uuid, emails: &[String]) -> sqlx::Result<HashSet<String>> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT lower(u.email) FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 AND lower(u.email) = ANY($2)")
        .bind(account_id)
        .bind(emails)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().collect())
}

/// SQL (binds: $1 account, $2 address): the address may receive notifications now.
const ALLOWED: &str = "(EXISTS (SELECT 1 FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 AND lower(u.email) = $2)
     OR EXISTS (SELECT 1 FROM flow_notify_addresses a WHERE a.account_id = $1 AND a.email = $2 AND a.confirmed_at IS NOT NULL AND a.removed_at IS NULL))";

/// SQL (binds: $1 account, $2 address): mail to this address bounced hard or drew a complaint
/// (as a profile, a message, or a notification).
const BAD_ADDRESS: &str = "(EXISTS (SELECT 1 FROM profiles WHERE account_id = $1 AND email = $2 AND email_suppressed_reason IS NOT NULL)
     OR EXISTS (SELECT 1 FROM messages WHERE account_id = $1 AND to_address = $2 AND (status = 'complained' OR (status = 'bounced' AND bounce_type = 'Permanent')))
     OR EXISTS (SELECT 1 FROM flow_notify_addresses WHERE account_id = $1 AND email = $2 AND blocked_reason IS NOT NULL))";

/// Record outside addresses as unconfirmed (no email is sent; see [`send_confirmation_request`]).
/// Returns each address's status.
pub async fn record_addresses(state: &AppState, account_id: Uuid, emails: &[String]) -> AppResult<Vec<Value>> {
    let emails: Vec<String> = emails.iter().filter_map(|e| normalize_email(e)).collect::<HashSet<_>>().into_iter().collect();
    let team = team_addresses(&state.db, account_id, &emails).await?;
    let outside: Vec<String> = emails.iter().filter(|e| !team.contains(*e)).cloned().collect();
    if !outside.is_empty() {
        let mut tx = state.db.begin().await?;
        // One at a time per account, so the limit on waiting addresses holds.
        sqlx::query("SELECT 1 FROM accounts WHERE id = $1 FOR UPDATE").bind(account_id).execute(&mut *tx).await?;
        let new: i64 = sqlx::query_scalar("SELECT count(*) FROM unnest($2::text[]) e WHERE NOT EXISTS (SELECT 1 FROM flow_notify_addresses a WHERE a.account_id = $1 AND a.email = e)")
            .bind(account_id)
            .bind(&outside)
            .fetch_one(&mut *tx)
            .await?;
        let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_notify_addresses WHERE account_id = $1 AND confirmed_at IS NULL AND removed_at IS NULL AND blocked_reason IS NULL")
            .bind(account_id)
            .fetch_one(&mut *tx)
            .await?;
        if new > 0 && pending + new > MAX_PENDING {
            return Err(AppError::Conflict(format!("at most {MAX_PENDING} outside addresses can wait to confirm; remove some first")));
        }
        sqlx::query("INSERT INTO flow_notify_addresses (account_id, email) SELECT $1, e FROM unnest($2::text[]) e ON CONFLICT DO NOTHING").bind(account_id).bind(&outside).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    statuses(&state.db, account_id, &emails).await
}

/// Ask an outside address to confirm: without `confirm`, says whether a link would go out. Refused
/// while the account's sending is paused, for team members (they need none), for confirmed,
/// bounced or complaining addresses. Limits, enforced in the insert that records it: one per
/// address per day across all accounts, except that each account's first request to an address
/// always goes (so another account can't block it), and [`MAX_CONFIRMATIONS_PER_DAY`] per account.
pub async fn send_confirmation_request(state: &AppState, account_id: Uuid, email: &str, confirm: bool) -> AppResult<Value> {
    let e = normalize_email(email).ok_or_else(|| AppError::bad("that isn't an email address"))?;
    if !team_addresses(&state.db, account_id, std::slice::from_ref(&e)).await?.is_empty() {
        return Err(AppError::bad("team members' addresses get notifications without confirming"));
    }
    let (paused, bad, confirmed): (bool, bool, bool) = sqlx::query_as(&format!(
        "SELECT COALESCE((SELECT sending_paused_at IS NOT NULL FROM accounts WHERE id = $1), true), {BAD_ADDRESS},
                EXISTS (SELECT 1 FROM flow_notify_addresses WHERE account_id = $1 AND email = $2 AND confirmed_at IS NOT NULL AND removed_at IS NULL)"
    ))
    .bind(account_id)
    .bind(&e)
    .fetch_one(&state.db)
    .await?;
    if paused {
        return Err(AppError::Conflict("sending is paused for this account, so no confirmation email can go out".into()));
    }
    if bad {
        return Err(AppError::bad(format!("mail to {e} bounced or drew a complaint, so it can't be asked")));
    }
    if confirmed {
        return Err(AppError::bad(format!("{e} already confirmed")));
    }
    if !confirm {
        return Ok(json!({"dry_run": true, "email": e, "confirm": "Call again with confirm: true to email this address a link to confirm."}));
    }
    record_addresses(state, account_id, std::slice::from_ref(&e)).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT 1 FROM accounts WHERE id = $1 FOR UPDATE").bind(account_id).execute(&mut *tx).await?;
    // One per address a day across all accounts: serialize on the address.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 11))").bind(&e).execute(&mut *tx).await?;
    // Asking a removed address again brings it back: it counts against the addresses waiting.
    let reviving: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flow_notify_addresses WHERE account_id = $1 AND email = $2 AND removed_at IS NOT NULL)")
        .bind(account_id)
        .bind(&e)
        .fetch_one(&mut *tx)
        .await?;
    if reviving {
        let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_notify_addresses WHERE account_id = $1 AND confirmed_at IS NULL AND removed_at IS NULL AND blocked_reason IS NULL")
            .bind(account_id)
            .fetch_one(&mut *tx)
            .await?;
        if pending >= MAX_PENDING {
            return Err(AppError::Conflict(format!("at most {MAX_PENDING} outside addresses can wait to confirm; remove some first")));
        }
    }
    let sent: Option<i64> = sqlx::query_scalar(
        "INSERT INTO flow_notify_confirm_sends (account_id, email) SELECT $1, $2
         WHERE (NOT EXISTS (SELECT 1 FROM flow_notify_confirm_sends WHERE email = $2 AND sent_at > now() - interval '24 hours')
                OR NOT EXISTS (SELECT 1 FROM flow_notify_confirm_sends WHERE account_id = $1 AND email = $2))
           AND (SELECT count(*) FROM flow_notify_confirm_sends WHERE account_id = $1 AND sent_at > now() - interval '24 hours') < $3
         RETURNING id",
    )
    .bind(account_id)
    .bind(&e)
    .bind(MAX_CONFIRMATIONS_PER_DAY)
    .fetch_optional(&mut *tx)
    .await?;
    if sent.is_none() {
        let address_today: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flow_notify_confirm_sends WHERE email = $2 AND sent_at > now() - interval '24 hours') AND EXISTS (SELECT 1 FROM flow_notify_confirm_sends WHERE account_id = $1 AND email = $2)")
            .bind(account_id)
            .bind(&e)
            .fetch_one(&mut *tx)
            .await?;
        return Err(AppError::Busy(if address_today {
            format!("{e} was sent a confirmation link in the last 24 hours; try again tomorrow")
        } else {
            format!("this account sent {MAX_CONFIRMATIONS_PER_DAY} confirmation emails in the last 24 hours; try again tomorrow")
        }));
    }
    sqlx::query("UPDATE flow_notify_addresses SET requested_at = now(), removed_at = NULL WHERE account_id = $1 AND email = $2").bind(account_id).bind(&e).execute(&mut *tx).await?;
    crate::jobs::enqueue_background(&mut *tx, CONFIRM_JOB, json!({"account_id": account_id, "email": e}), None).await?;
    tx.commit().await?;
    Ok(json!({"email": e, "status": "pending", "sent": true}))
}

/// Job: send an outside address its confirmation link (a fresh one; earlier links stop working).
/// Pause and suppression are checked again right before it's handed over.
pub async fn send_confirmation(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let (Some(account_id), Some(email)) = (payload["account_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()), payload["email"].as_str()) else { return Ok(()) };
    let (paused, bad): (bool, bool) = sqlx::query_as(&format!("SELECT COALESCE((SELECT sending_paused_at IS NOT NULL FROM accounts WHERE id = $1), true), {BAD_ADDRESS}"))
        .bind(account_id)
        .bind(email)
        .fetch_one(&state.db)
        .await?;
    if paused || bad {
        tracing::info!(account = %account_id, "confirmation email not sent: sending paused or the address bounced");
        return Ok(());
    }
    let token = crate::auth::random_token(32);
    let updated: Option<String> = sqlx::query_scalar(
        "UPDATE flow_notify_addresses SET token_hash = $3, requested_at = now() WHERE account_id = $1 AND email = $2 AND confirmed_at IS NULL AND removed_at IS NULL AND blocked_reason IS NULL RETURNING email",
    )
    .bind(account_id)
    .bind(email)
    .bind(crate::auth::sha256_hex(&token))
    .fetch_optional(&state.db)
    .await?;
    if updated.is_none() {
        return Ok(());
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let name = crate::features::team::mail::plain(&account.name, 80);
    let url = format!("{}/flow-notify/confirm?t={token}", state.cfg.public_url());
    let lines = [
        format!("{name} wants to email you notifications from one of their automated flows in DumbMail (for example, when a booking or an order comes in)."),
        "Confirm below to start getting them. If you don't know them or don't want these, ignore this email: you'll get nothing.".to_string(),
    ];
    crate::features::team::mail::send(state, email, &format!("Confirm notifications from {name}"), &lines, ("Confirm notifications", &url), &format!("This link works for {LINK_DAYS} days.")).await;
    Ok(())
}

/// The account a pending confirmation link belongs to, if it still works.
pub async fn link_account(db: &PgPool, token: &str) -> sqlx::Result<Option<(String, String)>> {
    sqlx::query_as(&format!(
        "SELECT ac.name, a.email FROM flow_notify_addresses a JOIN accounts ac ON ac.id = a.account_id
         WHERE a.token_hash = $1 AND a.confirmed_at IS NULL AND a.removed_at IS NULL AND a.requested_at > now() - interval '{LINK_DAYS} days'"
    ))
    .bind(crate::auth::sha256_hex(token))
    .fetch_optional(db)
    .await
}

/// Confirm with a link's token. Returns the account's name and the address.
pub async fn confirm(db: &PgPool, token: &str) -> sqlx::Result<Option<(String, String)>> {
    sqlx::query_as(&format!(
        "UPDATE flow_notify_addresses a SET confirmed_at = now(), token_hash = NULL FROM accounts ac
         WHERE ac.id = a.account_id AND a.token_hash = $1 AND a.confirmed_at IS NULL AND a.removed_at IS NULL AND a.requested_at > now() - interval '{LINK_DAYS} days'
         RETURNING ac.name, a.email"
    ))
    .bind(crate::auth::sha256_hex(token))
    .fetch_optional(db)
    .await
}

fn row_status(confirmed: bool, removed: bool, blocked: Option<&str>) -> &'static str {
    match (blocked, removed, confirmed) {
        (Some("complaint"), ..) => "complained",
        (Some(_), ..) => "bounced",
        (None, true, _) => "removed",
        (None, false, true) => "confirmed",
        (None, false, false) => "unconfirmed",
    }
}

pub async fn list_addresses(db: &PgPool, account_id: Uuid) -> AppResult<Vec<Value>> {
    type R = (String, Option<chrono::DateTime<Utc>>, chrono::DateTime<Utc>, Option<String>, Option<chrono::DateTime<Utc>>);
    let rows: Vec<R> = sqlx::query_as(
        "SELECT email, confirmed_at, requested_at, blocked_reason, blocked_at FROM flow_notify_addresses WHERE account_id = $1 AND (removed_at IS NULL OR blocked_reason IS NOT NULL) ORDER BY email LIMIT 500",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(email, confirmed_at, requested_at, blocked, blocked_at)| {
            json!({"email": email, "status": row_status(confirmed_at.is_some(), false, blocked.as_deref()), "confirmed_at": confirmed_at, "requested_at": requested_at, "blocked_reason": blocked, "blocked_at": blocked_at})
        })
        .collect())
}

/// The status of each address for notifications: team, confirmed, unconfirmed, removed, bounced,
/// complained or new (never recorded). A team member's address can be bounced or complained too.
pub async fn statuses(db: &PgPool, account_id: Uuid, emails: &[String]) -> AppResult<Vec<Value>> {
    let emails: Vec<String> = emails.iter().filter_map(|e| normalize_email(e)).take(50).collect();
    let team = team_addresses(db, account_id, &emails).await?;
    let known: Vec<(String, bool, bool, Option<String>)> =
        sqlx::query_as("SELECT email, confirmed_at IS NOT NULL, removed_at IS NOT NULL, blocked_reason FROM flow_notify_addresses WHERE account_id = $1 AND email = ANY($2)")
            .bind(account_id)
            .bind(&emails)
            .fetch_all(db)
            .await?;
    Ok(emails
        .iter()
        .map(|e| {
            let k = known.iter().find(|k| &k.0 == e);
            let status = match (k, team.contains(e)) {
                (Some((_, _, _, Some(b))), _) => row_status(false, false, Some(b)),
                (_, true) => "team",
                (Some((_, c, r, None)), false) => row_status(*c, *r, None),
                (None, false) => "new",
            };
            json!({"email": e, "status": status, "blocked_reason": k.and_then(|k| k.3.clone())})
        })
        .collect())
}

pub async fn remove_address(db: &PgPool, account_id: Uuid, email: &str) -> AppResult<bool> {
    let email = normalize_email(email).ok_or_else(|| AppError::bad("that isn't an email address"))?;
    // Kept as removed (with any bounce or complaint), so adding it again starts unconfirmed.
    Ok(sqlx::query("UPDATE flow_notify_addresses SET removed_at = now(), confirmed_at = NULL, token_hash = NULL WHERE account_id = $1 AND email = $2 AND removed_at IS NULL")
        .bind(account_id)
        .bind(&email)
        .execute(db)
        .await?
        .rows_affected()
        > 0)
}

/// Clear a hard bounce after the address was fixed. Complaints can't be cleared.
pub async fn clear_bounce(db: &PgPool, account_id: Uuid, email: &str) -> AppResult<bool> {
    let email = normalize_email(email).ok_or_else(|| AppError::bad("that isn't an email address"))?;
    let reason: Option<Option<String>> = sqlx::query_scalar("SELECT blocked_reason FROM flow_notify_addresses WHERE account_id = $1 AND email = $2").bind(account_id).bind(&email).fetch_optional(db).await?;
    match reason.flatten().as_deref() {
        Some("complaint") => Err(AppError::bad(format!("{email} marked a notification as spam; that can't be cleared"))),
        Some(_) => Ok(sqlx::query("UPDATE flow_notify_addresses SET blocked_reason = NULL, blocked_at = NULL WHERE account_id = $1 AND email = $2 AND blocked_reason = 'hard_bounce'")
            .bind(account_id)
            .bind(&email)
            .execute(db)
            .await?
            .rows_affected()
            > 0),
        None => Ok(false),
    }
}

/// The email provider reported a hard bounce or a complaint for message `provider_message_id`.
/// If it was a notification, block its address for that account. Returns whether it was one.
pub async fn provider_strike(db: &PgPool, provider_message_id: &str, complaint: bool) -> sqlx::Result<bool> {
    let row: Option<(Uuid, String)> = sqlx::query_as("SELECT account_id, to_email FROM flow_notifications WHERE provider_message_id = $1 LIMIT 1").bind(provider_message_id).fetch_optional(db).await?;
    let Some((account_id, email)) = row else { return Ok(false) };
    let reason = if complaint { "complaint" } else { "hard_bounce" };
    // A complaint is never downgraded to a bounce.
    sqlx::query(
        "INSERT INTO flow_notify_addresses (account_id, email, blocked_reason, blocked_at, removed_at) VALUES ($1, $2, $3, now(), now())
         ON CONFLICT (account_id, email) DO UPDATE SET blocked_reason = CASE WHEN flow_notify_addresses.blocked_reason = 'complaint' THEN 'complaint' ELSE EXCLUDED.blocked_reason END, blocked_at = now()",
    )
    .bind(account_id)
    .bind(&email)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(true)
}

/// When a flow is saved: outside fixed addresses in its notification steps are recorded as
/// unconfirmed. Nothing is emailed.
pub async fn on_save(state: &AppState, account_id: Uuid, def: &crate::flows::FlowDef) -> AppResult<()> {
    let mut emails = Vec::new();
    for n in def.nodes.iter().filter(|n| n.kind == "notify") {
        emails.extend(read(n)?.to);
    }
    if emails.is_empty() {
        return Ok(());
    }
    record_addresses(state, account_id, &emails).await.map(|_| ())
}

// ---------- Sending ----------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A stored recipient of a step for a run.
#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    to_email: String,
    status: String,
    subject: Option<String>,
    body: Option<String>,
}

async fn stored(db: &PgPool, run_id: Uuid, node_id: &str) -> sqlx::Result<Vec<Row>> {
    sqlx::query_as("SELECT id, to_email, status, subject, body FROM flow_notifications WHERE run_id = $1 AND node_id = $2 ORDER BY to_email").bind(run_id).bind(node_id).fetch_all(db).await
}

async fn settle(db: &PgPool, id: Uuid, from: &str, status: &str, why: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE flow_notifications SET status = $3, error = $4 WHERE id = $1 AND status = $2").bind(id).bind(from).bind(status).bind(why).execute(db).await?;
    Ok(())
}

/// Run a notification step for one person. Never stops the flow: problems are recorded per
/// recipient. `Some(t)`: the account's daily allowance (or the provider) says wait until `t`; the
/// run waits there and the retry sends what's still pending. Database errors are returned (the job
/// retries with the stored list).
#[allow(clippy::too_many_arguments)]
pub async fn run(state: &AppState, account: &Account, flow: &FlowRow, person: &Profile, run_id: Uuid, node_id: &str, node: &Node, context: &Value) -> anyhow::Result<Option<chrono::DateTime<Utc>>> {
    let db = &state.db;
    let mut rows = stored(db, run_id, node_id).await?;
    if rows.is_empty() {
        let Ok(step) = read(node) else { return Ok(None) };
        record_recipients(state, account, flow, person, run_id, node_id, &step, context).await?;
        rows = stored(db, run_id, node_id).await?;
    }
    for r in rows.iter().filter(|r| r.status == "pending") {
        if let Some(until) = send_one(state, account, flow, r).await? {
            return Ok(Some(until));
        }
    }
    Ok(None)
}

/// Fix who this step emails for this run, and what it says, in one transaction with the day's count.
#[allow(clippy::too_many_arguments)]
async fn record_recipients(state: &AppState, account: &Account, flow: &FlowRow, person: &Profile, run_id: Uuid, node_id: &str, step: &Step, context: &Value) -> anyhow::Result<()> {
    let mut to: Vec<String> = Vec::new();
    for e in step.to.iter().cloned().chain(step.to_property.as_ref().and_then(|k| person.properties.get(k)).and_then(Value::as_str).and_then(normalize_email)) {
        if !to.contains(&e) && to.len() < MAX_RECIPIENTS {
            to.push(e);
        }
    }
    // Unknown counts as from a browser, as everywhere in flows.
    let from_browser = context.get("event").is_some_and(|e| e.get("from_browser").and_then(Value::as_bool) != Some(false));
    let ctx = crate::render::context(account, Some(person), context.get("event"), "");
    let rendered = crate::render::render_str(&step.subject, &ctx, false).and_then(|s| Ok((s, crate::render::render_str(&step.body, &ctx, false)?)));
    let skip = if from_browser && !step.allow_browser_events {
        Some("started by a website event")
    } else if rendered.is_err() {
        Some("template error")
    } else {
        None
    };
    let (subject, body) = rendered.map(|(s, b)| (crate::features::team::mail::plain(&s, SUBJECT_MAX), b)).unwrap_or_default();
    let cap = daily_cap(&flow.definition);
    let day = Utc::now().with_timezone(&crate::sending::account_tz(account)).date_naive();
    let mut tx = state.db.begin().await?;
    for e in &to {
        // Only addresses that may get it now are pending (and use the day's count); checked again
        // when each is sent.
        let (allowed, bad): (bool, bool) = sqlx::query_as(&format!("SELECT {ALLOWED}, {BAD_ADDRESS}")).bind(account.id).bind(e).fetch_one(&mut *tx).await?;
        let why = skip.or(if !allowed {
            Some("address not confirmed")
        } else if bad {
            Some("address bounced or complained")
        } else {
            None
        });
        let status = if why.is_some() { "skipped" } else { "pending" };
        let id: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO flow_notifications (id, account_id, flow_id, run_id, node_id, to_email, status, error, subject, body) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (run_id, node_id, to_email) DO NOTHING RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(account.id)
        .bind(flow.id)
        .bind(run_id)
        .bind(node_id)
        .bind(e)
        .bind(status)
        .bind(why)
        .bind(&subject)
        .bind(&body)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else { continue };
        if status != "pending" {
            continue;
        }
        let counted: Option<i32> = sqlx::query_scalar(
            "INSERT INTO flow_notify_usage (flow_id, day, sent) VALUES ($1, $2, 1)
             ON CONFLICT (flow_id, day) DO UPDATE SET sent = flow_notify_usage.sent + 1 WHERE flow_notify_usage.sent < $3 RETURNING sent",
        )
        .bind(flow.id)
        .bind(day)
        .bind(cap as i32)
        .fetch_optional(&mut *tx)
        .await?;
        if counted.is_none() {
            sqlx::query("UPDATE flow_notifications SET status = 'skipped', error = 'daily limit reached' WHERE id = $1").bind(id).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Send one stored, pending notification. `Some(t)`: wait until `t` and try again (it stays pending).
async fn send_one(state: &AppState, account: &Account, flow: &FlowRow, r: &Row) -> anyhow::Result<Option<chrono::DateTime<Utc>>> {
    let db = &state.db;
    let (allowed, bad): (bool, bool) = sqlx::query_as(&format!("SELECT {ALLOWED}, {BAD_ADDRESS}")).bind(account.id).bind(&r.to_email).fetch_one(db).await?;
    let from_email = account.from_email.clone().filter(|s| !s.trim().is_empty());
    let why = if !allowed {
        Some("address not confirmed")
    } else if bad {
        Some("address bounced or complained")
    } else if account.sending_paused_at.is_some() {
        Some("sending paused")
    } else if from_email.is_none() {
        Some("no sender email")
    } else if from_email.as_deref().is_some_and(|f| crate::sending::check_sender_domain(state, account, f).is_err()) {
        Some("sender domain not verified")
    } else {
        None
    };
    if let Some(why) = why {
        settle(db, r.id, "pending", "skipped", why).await?;
        return Ok(None);
    }
    let price = crate::billing::quote(state, account.id, "email", 1, false, false).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if price > 0 && crate::billing::balance(db, account.id).await.map_err(|e| anyhow::anyhow!(e.public_message()))? < price {
        settle(db, r.id, "pending", "skipped", "insufficient credit").await?;
        return Ok(None);
    }
    // The account's daily allowance (warm-up), like any email: past it, the step waits.
    if let Err(until) = crate::deliverability::take_daily_slot(state, account.id).await? {
        return Ok(Some(until));
    }
    crate::sending::throttle_email(state).await;
    // A pause, a removed member or address, or a bounce can land while waiting for a slot.
    let (allowed, bad, paused): (bool, bool, bool) =
        sqlx::query_as(&format!("SELECT {ALLOWED}, {BAD_ADDRESS}, COALESCE((SELECT sending_paused_at IS NOT NULL FROM accounts WHERE id = $1), true)")).bind(account.id).bind(&r.to_email).fetch_one(db).await?;
    let stop = if paused { Some("sending paused") } else if bad { Some("address bounced or complained") } else if !allowed { Some("address not confirmed") } else { None };
    if let Some(why) = stop {
        crate::deliverability::return_daily_slot(state, account.id);
        settle(db, r.id, "pending", "skipped", why).await?;
        return Ok(None);
    }
    // Recorded as handed over before the call: a crash after this never sends it again.
    let handing: Option<Uuid> = sqlx::query_scalar("UPDATE flow_notifications SET status = 'handing', handed_at = now() WHERE id = $1 AND status = 'pending' RETURNING id").bind(r.id).fetch_optional(db).await?;
    if handing.is_none() {
        crate::deliverability::return_daily_slot(state, account.id);
        return Ok(None);
    }
    let subject = r.subject.clone().unwrap_or_default();
    let body = r.body.clone().unwrap_or_default();
    let footer = format!("You get this because {} set up a notification in a DumbMail flow ({}).", crate::features::team::mail::plain(&account.name, 80), crate::features::team::mail::plain(&flow.name, 80));
    let paras: String = body.split("\n\n").map(|p| format!("<p style=\"margin:0 0 14px\">{}</p>", esc(p.trim()).replace('\n', "<br>"))).collect();
    let html = format!(
        "<div style=\"font-family:system-ui,-apple-system,sans-serif;font-size:15px;line-height:1.5;color:#18181b;max-width:560px\">{paras}<p style=\"margin:24px 0 0;font-size:12px;color:#71717a\">{}</p></div>",
        esc(&footer)
    );
    let email = OutgoingEmail {
        message_id: r.id,
        to: r.to_email.clone(),
        from_name: account.from_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| account.name.clone()),
        from_email: from_email.unwrap_or_default(),
        reply_to: account.reply_to.clone().filter(|s| !s.is_empty()),
        subject,
        html,
        text: format!("{body}\n\n--\n{footer}\n"),
        unsubscribe_url: None,
    };
    match state.email.send_once(&email).await {
        Ok(provider_id) => {
            let mut tx = db.begin().await?;
            let first: Option<Uuid> = sqlx::query_scalar("UPDATE flow_notifications SET status = 'sent', sent_at = now(), provider_message_id = $2, error = NULL WHERE id = $1 AND status = 'handing' RETURNING id")
                .bind(r.id)
                .bind(&provider_id)
                .fetch_optional(&mut *tx)
                .await?;
            if first.is_some() {
                charge(state, &mut tx, account.id, r.id).await?;
            }
            tx.commit().await?;
            // Counts toward the account's limit and its bounce and complaint rates, like any email.
            crate::deliverability::record_send(db, r.id, &provider_id, account.id, None).await?;
            crate::deliverability::complete_daily_slot(state, account.id);
            Ok(None)
        }
        Err(e) => {
            if let Some(busy) = e.downcast_ref::<crate::providers::email::ProviderBusy>() {
                // Refused for now, so not sent: back to pending, and the step waits.
                crate::deliverability::return_daily_slot(state, account.id);
                settle(db, r.id, "handing", "pending", "the email provider was busy").await?;
                let wait = chrono::Duration::from_std(busy.retry_after).unwrap_or(chrono::Duration::minutes(15)).clamp(chrono::Duration::minutes(1), chrono::Duration::hours(1));
                return Ok(Some(Utc::now() + wait));
            }
            if e.downcast_ref::<crate::providers::email::ProviderMaybeSent>().is_some() {
                crate::deliverability::complete_daily_slot(state, account.id);
                unconfirmed(state, r.id, "the email provider may have taken it; not sent again").await?;
                return Ok(None);
            }
            // Only a short class is kept: a provider's own words can echo addresses or settings.
            tracing::warn!(notification = %r.id, flow = %flow.id, "flow notification refused by the email provider");
            crate::deliverability::return_daily_slot(state, account.id);
            settle(db, r.id, "handing", "failed", "the email provider refused it").await?;
            Ok(None)
        }
    }
}

/// Charge one notification once, with its own row.
async fn charge(state: &AppState, tx: &mut sqlx::PgConnection, account_id: Uuid, id: Uuid) -> anyhow::Result<()> {
    let first: Option<Uuid> = sqlx::query_scalar("UPDATE flow_notifications SET cost_micros = 0 WHERE id = $1 AND cost_micros IS NULL RETURNING id").bind(id).fetch_optional(&mut *tx).await?;
    if first.is_some() {
        let cost = crate::billing::charge_email_tx(state, &mut *tx, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        sqlx::query("UPDATE flow_notifications SET cost_micros = $2 WHERE id = $1").bind(id).bind(cost).execute(&mut *tx).await?;
    }
    Ok(())
}

/// A send whose handoff is uncertain: never sent again, and billed, since it may have gone out.
async fn unconfirmed(state: &AppState, id: Uuid, why: &str) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    let row: Option<Uuid> = sqlx::query_scalar("UPDATE flow_notifications SET status = 'unconfirmed', error = $2 WHERE id = $1 AND status = 'handing' RETURNING account_id")
        .bind(id)
        .bind(why)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(account_id) = row {
        charge(state, &mut tx, account_id, id).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Scheduler: handoffs cut off before their result was recorded may have gone out; they become
/// `unconfirmed` (never sent again, billed). Old daily counters are deleted.
pub async fn sweep(state: &AppState) -> anyhow::Result<()> {
    let cut: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM flow_notifications WHERE status = 'handing' AND handed_at < now() - interval '15 minutes' LIMIT 500").fetch_all(&state.db).await?;
    for id in cut {
        unconfirmed(state, id, "cut off while being handed to the email provider; not sent again").await?;
    }
    sqlx::query("DELETE FROM flow_notify_usage WHERE day < (now() - interval '3 days')::date").execute(&state.db).await?;
    Ok(())
}
