//! Notification steps: email the team (or a confirmed outside address) when someone reaches a step.
//!
//! A `notify` step emails up to five addresses: fixed ones (`to`) and/or the address in one of the
//! person's profile properties (`to_property`). Only the account's team members' addresses and
//! addresses that clicked a confirmation link get anything; the first time an outside address is
//! saved in a flow it is sent that link. A notification is a service message from the account's
//! sender: no unsubscribe footer, a plain line saying who set it up, billed like any email, held to
//! the sending rate and the account's pause, and capped per flow per local day (`notify_daily_cap`
//! in the flow's settings, default 200, at most 2,000). Runs started by an event from the website
//! script get no notification unless the step sets `allow_browser_events`.
//!
//! Exactly once: each (run, step, address) is claimed in `flow_notifications` before the provider
//! call, together with the day's count. A claim cut off before its result is recorded becomes
//! `unknown` and is never sent again.

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
     OR EXISTS (SELECT 1 FROM flow_notify_addresses a WHERE a.account_id = $1 AND a.email = $2 AND a.confirmed_at IS NOT NULL))";

/// SQL (binds: $1 account, $2 address): mail to this address bounced hard or drew a complaint.
const BAD_ADDRESS: &str = "(EXISTS (SELECT 1 FROM profiles WHERE account_id = $1 AND email = $2 AND email_suppressed_reason IS NOT NULL)
     OR EXISTS (SELECT 1 FROM messages WHERE account_id = $1 AND to_address = $2 AND (status = 'complained' OR (status = 'bounced' AND bounce_type = 'Permanent'))))";

/// Confirmation emails an account may send in 24 hours.
pub const MAX_CONFIRMATIONS_PER_DAY: i64 = 50;

/// Ask outside addresses to confirm (team members' addresses need nothing). An address never seen
/// gets a confirmation email; `ask_again` (someone added the address by hand) sends another to one
/// still waiting or removed earlier. Each address gets at most one an hour and each account at most
/// [`MAX_CONFIRMATIONS_PER_DAY`] a day; addresses that bounced or complained, and accounts whose
/// sending is paused, get none. Returns each address's status: team, confirmed, pending (a link was
/// sent), waiting (one was sent within the hour, or it was removed and saving a flow doesn't ask
/// again) or unreachable.
pub async fn request_confirmations(state: &AppState, account_id: Uuid, emails: &[String], ask_again: bool) -> AppResult<Vec<(String, &'static str)>> {
    let emails: Vec<String> = emails.iter().filter_map(|e| normalize_email(e)).collect::<HashSet<_>>().into_iter().collect();
    let team = team_addresses(&state.db, account_id, &emails).await?;
    let mut out = Vec::new();
    for e in emails {
        if team.contains(&e) {
            out.push((e, "team"));
            continue;
        }
        let mut tx = state.db.begin().await?;
        // One at a time per account, so the limits below hold.
        let paused: bool = sqlx::query_scalar("SELECT sending_paused_at IS NOT NULL FROM accounts WHERE id = $1 FOR UPDATE").bind(account_id).fetch_one(&mut *tx).await?;
        type Known = (Option<chrono::DateTime<Utc>>, Option<chrono::DateTime<Utc>>);
        let existing: Option<Known> =
            sqlx::query_as("SELECT confirmed_at, removed_at FROM flow_notify_addresses WHERE account_id = $1 AND email = $2").bind(account_id).bind(&e).fetch_optional(&mut *tx).await?;
        if let Some((Some(_), None)) = existing {
            out.push((e, "confirmed"));
            continue;
        }
        let bad: bool = sqlx::query_scalar(&format!("SELECT {BAD_ADDRESS}")).bind(account_id).bind(&e).fetch_one(&mut *tx).await?;
        if bad {
            out.push((e, "unreachable"));
            continue;
        }
        match existing {
            None => {
                let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_notify_addresses WHERE account_id = $1 AND confirmed_at IS NULL AND removed_at IS NULL")
                    .bind(account_id)
                    .fetch_one(&mut *tx)
                    .await?;
                if pending >= MAX_PENDING {
                    return Err(AppError::LimitReached(format!("{MAX_PENDING} addresses are already waiting to confirm; remove some before adding more")));
                }
                sqlx::query("INSERT INTO flow_notify_addresses (account_id, email) VALUES ($1, $2)").bind(account_id).bind(&e).execute(&mut *tx).await?;
            }
            Some((_, removed)) => {
                if !ask_again {
                    tx.commit().await?;
                    out.push((e, "waiting"));
                    continue;
                }
                if removed.is_some() {
                    sqlx::query("UPDATE flow_notify_addresses SET removed_at = NULL, confirmed_at = NULL, requested_at = now() WHERE account_id = $1 AND email = $2").bind(account_id).bind(&e).execute(&mut *tx).await?;
                }
            }
        }
        let (this_hour, today): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE email = $2 AND sent_at > now() - interval '1 hour'), count(*) FROM flow_notify_confirm_sends WHERE account_id = $1 AND sent_at > now() - interval '24 hours'",
        )
        .bind(account_id)
        .bind(&e)
        .fetch_one(&mut *tx)
        .await?;
        let status = if paused || this_hour > 0 {
            "waiting"
        } else if today >= MAX_CONFIRMATIONS_PER_DAY {
            tx.commit().await?;
            return Err(AppError::LimitReached(format!("this account sent {MAX_CONFIRMATIONS_PER_DAY} confirmation emails in the last 24 hours; try again tomorrow")));
        } else {
            sqlx::query("INSERT INTO flow_notify_confirm_sends (account_id, email) VALUES ($1, $2)").bind(account_id).bind(&e).execute(&mut *tx).await?;
            crate::jobs::enqueue_background(&mut *tx, CONFIRM_JOB, json!({"account_id": account_id, "email": e}), None).await?;
            "pending"
        };
        tx.commit().await?;
        out.push((e, status));
    }
    Ok(out)
}

/// Job: send an outside address its confirmation link (a fresh one; earlier links stop working).
pub async fn send_confirmation(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let (Some(account_id), Some(email)) = (payload["account_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()), payload["email"].as_str()) else { return Ok(()) };
    let token = crate::auth::random_token(32);
    let updated: Option<String> = sqlx::query_scalar(
        "UPDATE flow_notify_addresses SET token_hash = $3, requested_at = now() WHERE account_id = $1 AND email = $2 AND confirmed_at IS NULL AND removed_at IS NULL RETURNING email",
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

pub async fn list_addresses(db: &PgPool, account_id: Uuid) -> AppResult<Vec<Value>> {
    let rows: Vec<(String, Option<chrono::DateTime<Utc>>, chrono::DateTime<Utc>)> =
        sqlx::query_as("SELECT email, confirmed_at, requested_at FROM flow_notify_addresses WHERE account_id = $1 AND removed_at IS NULL ORDER BY email LIMIT 500").bind(account_id).fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|(email, confirmed_at, requested_at)| json!({"email": email, "status": if confirmed_at.is_some() { "confirmed" } else { "pending" }, "confirmed_at": confirmed_at, "requested_at": requested_at}))
        .collect())
}

/// The status of each address for notifications: team, confirmed, pending, removed or new (never asked).
pub async fn statuses(db: &PgPool, account_id: Uuid, emails: &[String]) -> AppResult<Vec<Value>> {
    let emails: Vec<String> = emails.iter().filter_map(|e| normalize_email(e)).take(50).collect();
    let team = team_addresses(db, account_id, &emails).await?;
    let known: Vec<(String, bool, bool)> = sqlx::query_as("SELECT email, confirmed_at IS NOT NULL, removed_at IS NOT NULL FROM flow_notify_addresses WHERE account_id = $1 AND email = ANY($2)")
        .bind(account_id)
        .bind(&emails)
        .fetch_all(db)
        .await?;
    Ok(emails
        .iter()
        .map(|e| {
            let status = if team.contains(e) {
                "team"
            } else {
                match known.iter().find(|k| &k.0 == e) {
                    Some((_, _, true)) => "removed",
                    Some((_, true, _)) => "confirmed",
                    Some((_, false, _)) => "pending",
                    None => "new",
                }
            };
            json!({"email": e, "status": status})
        })
        .collect())
}

pub async fn remove_address(db: &PgPool, account_id: Uuid, email: &str) -> AppResult<bool> {
    let email = normalize_email(email).ok_or_else(|| AppError::bad("that isn't an email address"))?;
    // Kept as removed, so adding it again is held to the same limits as asking again.
    Ok(sqlx::query("UPDATE flow_notify_addresses SET removed_at = now(), confirmed_at = NULL, token_hash = NULL WHERE account_id = $1 AND email = $2 AND removed_at IS NULL")
        .bind(account_id)
        .bind(&email)
        .execute(db)
        .await?
        .rows_affected()
        > 0)
}

/// When a flow is saved: outside fixed addresses in its notification steps that were never asked
/// get a confirmation link.
pub async fn on_save(state: &AppState, account_id: Uuid, def: &crate::flows::FlowDef) -> AppResult<()> {
    let mut emails = Vec::new();
    for n in def.nodes.iter().filter(|n| n.kind == "notify") {
        emails.extend(read(n)?.to);
    }
    if emails.is_empty() {
        return Ok(());
    }
    request_confirmations(state, account_id, &emails, false).await.map(|_| ())
}

// ---------- Sending ----------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Skip a recipient, unless this (run, step, address) was already handled.
async fn skip(db: &PgPool, account_id: Uuid, flow_id: Uuid, run_id: Uuid, node_id: &str, to: &str, why: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO flow_notifications (id, account_id, flow_id, run_id, node_id, to_email, status, error) VALUES ($1, $2, $3, $4, $5, $6, 'skipped', $7)
         ON CONFLICT (run_id, node_id, to_email) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(flow_id)
    .bind(run_id)
    .bind(node_id)
    .bind(to)
    .bind(why)
    .execute(db)
    .await?;
    Ok(())
}

async fn settle(db: &PgPool, id: Uuid, status: &str, why: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE flow_notifications SET status = $2, error = $3 WHERE id = $1 AND status = 'sending'").bind(id).bind(status).bind(why).execute(db).await?;
    Ok(())
}

/// Run a notification step for one person. Never stops the flow: problems are recorded per
/// recipient. Database errors are returned (the job retries, and claims keep it from sending twice).
#[allow(clippy::too_many_arguments)]
pub async fn run(state: &AppState, account: &Account, flow: &FlowRow, person: &Profile, run_id: Uuid, node_id: &str, node: &Node, context: &Value) -> anyhow::Result<()> {
    let Ok(step) = read(node) else { return Ok(()) };
    let db = &state.db;
    let mut to: Vec<String> = Vec::new();
    for e in step.to.iter().cloned().chain(step.to_property.as_ref().and_then(|k| person.properties.get(k)).and_then(Value::as_str).and_then(normalize_email)) {
        if !to.contains(&e) && to.len() < MAX_RECIPIENTS {
            to.push(e);
        }
    }
    // Unknown counts as from a browser, as everywhere in flows.
    let from_browser = context.get("event").is_some_and(|e| e.get("from_browser").and_then(Value::as_bool) != Some(false));
    if from_browser && !step.allow_browser_events {
        for e in &to {
            skip(db, account.id, flow.id, run_id, node_id, e, "started by a website event").await?;
        }
        return Ok(());
    }
    let ctx = crate::render::context(account, Some(person), context.get("event"), "");
    let rendered = crate::render::render_str(&step.subject, &ctx, false).and_then(|s| Ok((s, crate::render::render_str(&step.body, &ctx, false)?)));
    let Ok((subject, body)) = rendered else {
        for e in &to {
            skip(db, account.id, flow.id, run_id, node_id, e, "template error").await?;
        }
        return Ok(());
    };
    let subject = crate::features::team::mail::plain(&subject, SUBJECT_MAX);
    let cap = daily_cap(&flow.definition);
    for e in &to {
        send_one(state, account, flow, run_id, node_id, e, &subject, &body, cap).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_one(state: &AppState, account: &Account, flow: &FlowRow, run_id: Uuid, node_id: &str, to: &str, subject: &str, body: &str, cap: i64) -> anyhow::Result<()> {
    let db = &state.db;
    let (allowed, bad): (bool, bool) = sqlx::query_as(&format!("SELECT {ALLOWED}, {BAD_ADDRESS}")).bind(account.id).bind(to).fetch_one(db).await?;
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
        return Ok(skip(db, account.id, flow.id, run_id, node_id, to, why).await?);
    }
    let price = crate::billing::quote(state, account.id, "email", 1, false, false).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if price > 0 && crate::billing::balance(db, account.id).await.map_err(|e| anyhow::anyhow!(e.public_message()))? < price {
        return Ok(skip(db, account.id, flow.id, run_id, node_id, to, "insufficient credit").await?);
    }
    // The claim and the day's count, together: a retry finds the claim and sends nothing.
    let id = Uuid::now_v7();
    let day = Utc::now().with_timezone(&crate::sending::account_tz(account)).date_naive();
    let mut tx = db.begin().await?;
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO flow_notifications (id, account_id, flow_id, run_id, node_id, to_email, status, subject) VALUES ($1, $2, $3, $4, $5, $6, 'sending', $7)
         ON CONFLICT (run_id, node_id, to_email) DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(account.id)
    .bind(flow.id)
    .bind(run_id)
    .bind(node_id)
    .bind(to)
    .bind(subject)
    .fetch_optional(&mut *tx)
    .await?;
    if claimed.is_none() {
        tx.rollback().await?;
        return Ok(());
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
        tx.commit().await?;
        return Ok(());
    }
    tx.commit().await?;

    crate::sending::throttle_email(state).await;
    // A pause, a removed member or address, or a bounce can land while waiting for a slot.
    let (allowed, bad, paused): (bool, bool, bool) =
        sqlx::query_as(&format!("SELECT {ALLOWED}, {BAD_ADDRESS}, COALESCE((SELECT sending_paused_at IS NOT NULL FROM accounts WHERE id = $1), true)")).bind(account.id).bind(to).fetch_one(db).await?;
    if !allowed || bad || paused {
        settle(db, id, "skipped", if paused { "sending paused" } else if bad { "address bounced or complained" } else { "address not confirmed" }).await?;
        return Ok(());
    }
    let footer = format!("You get this because {} set up a notification in a DumbMail flow ({}).", crate::features::team::mail::plain(&account.name, 80), crate::features::team::mail::plain(&flow.name, 80));
    let paras: String = body.split("\n\n").map(|p| format!("<p style=\"margin:0 0 14px\">{}</p>", esc(p.trim()).replace('\n', "<br>"))).collect();
    let html = format!(
        "<div style=\"font-family:system-ui,-apple-system,sans-serif;font-size:15px;line-height:1.5;color:#18181b;max-width:560px\">{paras}<p style=\"margin:24px 0 0;font-size:12px;color:#71717a\">{}</p></div>",
        esc(&footer)
    );
    let email = OutgoingEmail {
        message_id: id,
        to: to.to_string(),
        from_name: account.from_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| account.name.clone()),
        from_email: from_email.unwrap_or_default(),
        reply_to: account.reply_to.clone().filter(|s| !s.is_empty()),
        subject: subject.to_string(),
        html,
        text: format!("{body}\n\n--\n{footer}\n"),
        unsubscribe_url: None,
    };
    match state.email.send_once(&email).await {
        Ok(provider_id) => {
            let mut tx = db.begin().await?;
            let first: Option<Uuid> = sqlx::query_scalar("UPDATE flow_notifications SET status = 'sent', sent_at = now(), provider_message_id = $2, error = NULL WHERE id = $1 AND status = 'sending' RETURNING id")
                .bind(id)
                .bind(&provider_id)
                .fetch_optional(&mut *tx)
                .await?;
            if first.is_some() {
                let cost = crate::billing::charge_email_tx(state, &mut tx, account.id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                sqlx::query("UPDATE flow_notifications SET cost_micros = $2 WHERE id = $1").bind(id).bind(cost).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            // Bounces and complaints count toward the account's rates (not its warm-up limit).
            crate::deliverability::record_confirmation_send(db, id, &provider_id, account.id).await?;
        }
        Err(e) => {
            // Only a short class is kept: a provider's own words can echo addresses or settings.
            let (status, why) = if e.downcast_ref::<crate::providers::email::ProviderBusy>().is_some() {
                ("failed", "the email provider was busy")
            } else if e.downcast_ref::<crate::providers::email::ProviderMaybeSent>().is_some() {
                ("unknown", "may have gone out; not sent again")
            } else {
                ("failed", "the email provider refused it")
            };
            tracing::warn!(notification = %id, flow = %flow.id, "flow notification not sent: {why}");
            settle(db, id, status, why).await?;
        }
    }
    Ok(())
}

/// Scheduler: claims cut off before their result was recorded may have gone out; they become
/// `unknown` and are never sent again.
pub async fn sweep(db: &PgPool) -> sqlx::Result<()> {
    sqlx::query("UPDATE flow_notifications SET status = 'unknown', error = 'cut off while sending; may have gone out, not sent again' WHERE status = 'sending' AND created_at < now() - interval '15 minutes'")
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM flow_notify_usage WHERE day < (now() - interval '3 days')::date").execute(db).await?;
    Ok(())
}
