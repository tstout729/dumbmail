//! WhatsApp as a channel: sending one message (a `messages` row, channel `whatsapp`, with its
//! `whatsapp_sent` row) for campaigns, flows, tests and inbox replies.
//!
//! A send happens in three writes:
//!
//! 1. **Prepare** (one transaction): the message row and what it sends (`whatsapp_sent.payload`),
//!    filled once for the connected sender; a retry sends the same thing.
//! 2. **The final claim**, right before the provider call, in one transaction holding, in this order,
//!    the number's lock (the one an inbound STOP and Meta's opt-out signal take), the sender's
//!    settings row (a disconnect or a new sender waits for it, or lands first and stops it) and the
//!    campaign row (a cancel does the same). It reads the live state: the account is open; the
//!    message was prepared for this sender; the person still has this number; it isn't opted out of
//!    WhatsApp or known to be off it; the campaign is still sending; the template is still approved,
//!    with its *current* category deciding the rules; Meta's back-off for the sender (or the sender
//!    and this person) is over; then consent, the marketing hold and quiet hours for marketing, the
//!    24-hour window for free-form replies, and a current member's verified number for tests. It
//!    holds the credit (when a price is set), records the number and payload it hands over, and
//!    reads the credentials; the worker sends exactly that. A message handed over without an answer
//!    is never sent again.
//! 3. **The result**: accepted, or a refusal by class. Rate limits and Meta outages wait (on the
//!    background lane, backing off per sender, or per sender and person for pair limits); anything
//!    that may have gone (a time-out, a gateway error, an answer without Meta's error code) is never
//!    repeated. The credit is charged once: when Meta accepts it on DumbMail's own call, or, for a
//!    message Meta holds for a quality check, when Meta's status says it was sent. It's released when
//!    Meta refuses it, or drops a held one. (Statuses are signed with the customer's own app secret,
//!    so they can only ever turn a hold into a charge or release a hold Meta itself kept.)

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use super::content::{self, Rendered};
use super::graph::{self, Class};
use super::templates::{self, Template};
use super::{consent, settings};
use crate::audience::{record_event, NewEvent};
use crate::error::{AppError, AppResult};
use crate::models::normalize_phone;
use crate::sending::{Outcome, SendSpec};
use crate::state::AppState;

/// Tries for a message Meta refused with a "try later" answer, and the waits between them.
const RETRY_WAITS: [i64; 4] = [60, 300, 1800, 7200];
/// Waits before trying an undeliverable message again (131026: an old app or terms not accepted
/// may pass), then it fails.
const UNDELIVERABLE_WAITS: [i64; 2] = [6 * 3600, 24 * 3600];
/// Undeliverable answers in a row before a number counts as off WhatsApp.
pub const UNDELIVERABLE_LIMIT: i32 = 3;
/// Longest wait for Meta's answer to a send; past it the outcome is unknown and it isn't sent again.
const PROVIDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Hours after their last message during which a free-form reply may go.
pub const WINDOW_HOURS: i64 = 24;

/// Price per WhatsApp message (0 unless PRICE_WHATSAPP is set; Meta bills the business itself).
pub fn price(state: &AppState) -> i64 {
    if !state.cfg.hosted {
        return 0;
    }
    crate::billing::to_micros(state.cfg.whatsapp.price_whatsapp.max(0.0))
}

/// Why a send stopped before the hand-off.
#[derive(Debug)]
enum Stop {
    Skip(&'static str),
    Defer(DateTime<Utc>),
}

/// Mark a message that won't go (only while it's still waiting), and report it.
async fn mark(state: &AppState, account_id: Uuid, message_id: Uuid, status: &str, why: &str, is_test: bool) -> AppResult<()> {
    let n = sqlx::query("UPDATE messages SET status = $2, error = $3 WHERE id = $1 AND status IN ('queued', 'sending', 'retrying')")
        .bind(message_id)
        .bind(status)
        .bind(why)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n > 0 && !is_test {
        crate::features::messages_unsent(&state.db, account_id, &[message_id]).await;
    }
    Ok(())
}

/// Stop a send that has no prepared row yet: a campaign's queued row is marked; tests say why.
async fn early(state: &AppState, spec: &SendSpec<'_>, why: &'static str, test_message: &str) -> AppResult<Outcome> {
    if spec.is_test {
        return Err(AppError::bad(test_message.to_string()));
    }
    if let (Some(mid), Some(_)) = (spec.message_id, spec.campaign_id) {
        mark(state, spec.account.id, mid, "skipped", why, false).await?;
    }
    Ok(Outcome::Skipped(why))
}

/// The channel's entry point from `sending::send` (campaigns, flows and campaign tests). A flow
/// passes the message id it stored with its run, so a re-run of the step finds the message instead
/// of making another. `strict`: the strict marketing rules (`sending::send_strict_marketing`).
pub async fn send(state: &AppState, spec: &SendSpec<'_>, strict: bool) -> AppResult<Outcome> {
    let account = spec.account;
    let p = spec.profile;
    if !settings::connected(&state.db, account.id).await? {
        return early(state, spec, "WhatsApp isn't connected", "Connect WhatsApp in Settings → WhatsApp first").await;
    }
    let c = content::parse(spec.content)?;
    let Some(tid) = c.template_id else {
        return early(state, spec, "no WhatsApp template chosen", "Pick a WhatsApp template first").await;
    };
    let Ok(t) = templates::get(&state.db, account.id, tid).await else {
        return early(state, spec, "WhatsApp template missing", "That WhatsApp template isn't in this account").await;
    };
    if !t.approved() {
        return early(state, spec, "template not approved", &format!("The template {} is {} at Meta, not approved", t.name, t.status.to_lowercase())).await;
    }
    let raw = spec.to_override.as_deref().or(p.phone_number.as_deref());
    let Some(phone) = raw.and_then(normalize_phone) else {
        return early(state, spec, "no phone number", "That isn't a phone number (use +15035550100)").await;
    };
    // A message already handled (a flow step run again) is never sent again.
    if let Some(mid) = spec.message_id {
        let st: Option<(String, Option<DateTime<Utc>>)> =
            sqlx::query_as("SELECT m.status, w.handed_at FROM messages m LEFT JOIN whatsapp_sent w ON w.message_id = m.id WHERE m.id = $1")
                .bind(mid)
                .fetch_optional(&state.db)
                .await?;
        if let Some((status, handed)) = st {
            if !matches!(status.as_str(), "queued" | "sending" | "retrying") || handed.is_some() {
                return Ok(Outcome::Skipped("already handled"));
            }
        }
    }
    if !spec.is_test {
        // Quiet hours in their phone number's zone, as for texts (checked again at the claim).
        let hours = crate::features::sendtime::settings::sms_hours(&state.db, account, p, Utc::now()).await?;
        if hours.skip {
            return early(state, spec, "time zones disagree", "").await;
        }
        if let Some(until) = hours.wait {
            return Ok(Outcome::Deferred(until));
        }
    }
    let message_id = spec.message_id.unwrap_or_else(Uuid::now_v7);
    let ctx = crate::render::context(account, Some(p), spec.event, "");
    let rendered = match content::render(&t, &c, &ctx, Some((&state.cfg.secret_key, message_id))) {
        Ok(r) => r,
        Err(e) if spec.is_test => return Err(e),
        Err(e) => {
            let why = e.public_message();
            if spec.campaign_id.is_some() {
                mark(state, account.id, message_id, "failed", &why, false).await?;
            }
            return Ok(Outcome::Failed(why));
        }
    };
    let Some(()) = prepare(state, spec, message_id, &phone, &t, &rendered).await? else {
        return Ok(Outcome::Skipped("already handled"));
    };
    hand_off(state, message_id, strict).await
}

/// Write the message and what it sends, for the current sender. None: the message was already
/// settled (or its campaign row is gone with its person).
async fn prepare(state: &AppState, spec: &SendSpec<'_>, message_id: Uuid, phone: &str, t: &Template, r: &Rendered) -> AppResult<Option<()>> {
    let mut tx = state.db.begin().await?;
    let row: Option<Uuid> = if spec.campaign_id.is_some() && spec.message_id.is_some() && !spec.is_test {
        sqlx::query_scalar("UPDATE messages SET to_address = $2, subject = $3 WHERE id = $1 AND status IN ('queued', 'sending', 'retrying') RETURNING id")
            .bind(message_id)
            .bind(phone)
            .bind(&t.name)
            .fetch_optional(&mut *tx)
            .await?
    } else {
        sqlx::query_scalar(
            "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, subject, is_test)
             VALUES ($1, $2, $3, 'whatsapp', $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO UPDATE SET to_address = EXCLUDED.to_address WHERE messages.status IN ('queued', 'sending', 'retrying')
             RETURNING id",
        )
        .bind(message_id)
        .bind(spec.account.id)
        .bind(spec.profile.id)
        .bind(spec.campaign_id)
        .bind(spec.flow_id)
        .bind(spec.flow_node_id)
        .bind(phone)
        .bind(&t.name)
        .bind(spec.is_test)
        .fetch_optional(&mut *tx)
        .await?
    };
    if row.is_none() {
        return Ok(None);
    }
    let sender = settings::sender_id(&mut *tx, spec.account.id).await?;
    sqlx::query(
        "INSERT INTO whatsapp_sent (message_id, account_id, kind, sender_id, template_id, template_name, category, body, payload)
         VALUES ($1, $2, 'marketing', $3, $4, $5, $6, $7, $8) ON CONFLICT (message_id) DO NOTHING",
    )
    .bind(message_id)
    .bind(spec.account.id)
    .bind(sender)
    .bind(t.id)
    .bind(&t.name)
    .bind(&t.category)
    .bind(&r.text)
    .bind(&r.payload)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(()))
}

/// A conversational message (an inbox reply, a keyword's answer, a confirmation): written queued
/// for the current sender, inside the caller's transaction. `template`: an approved template and its
/// filled payload; otherwise free-form `text`.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_conversational(
    conn: &mut sqlx::PgConnection,
    account_id: Uuid,
    profile_id: Uuid,
    phone: &str,
    kind: &str,
    sent_by: Option<Uuid>,
    text: &str,
    template: Option<(&Template, &Rendered)>,
) -> AppResult<Uuid> {
    let message_id = Uuid::now_v7();
    let subject = template.map(|(t, _)| t.name.clone());
    sqlx::query("INSERT INTO messages (id, account_id, profile_id, channel, to_address, subject) VALUES ($1, $2, $3, 'whatsapp', $4, $5)")
        .bind(message_id)
        .bind(account_id)
        .bind(profile_id)
        .bind(phone)
        .bind(&subject)
        .execute(&mut *conn)
        .await?;
    let (payload, body) = match template {
        Some((_, r)) => (r.payload.clone(), r.text.clone()),
        None => (content::text_payload(text), text.to_string()),
    };
    let sender = settings::sender_id(&mut *conn, account_id).await?;
    sqlx::query(
        "INSERT INTO whatsapp_sent (message_id, account_id, kind, sent_by, sender_id, template_id, template_name, category, body, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(message_id)
    .bind(account_id)
    .bind(kind)
    .bind(sent_by)
    .bind(sender)
    .bind(template.map(|(t, _)| t.id))
    .bind(&subject)
    .bind(template.map(|(t, _)| t.category.clone()))
    .bind(&body)
    .bind(&payload)
    .execute(&mut *conn)
    .await?;
    Ok(message_id)
}

/// What the final claim reads about the message, its person and its template, in one statement.
#[derive(sqlx::FromRow, Debug)]
struct ClaimRow {
    account_id: Uuid,
    status: String,
    to_address: String,
    campaign_id: Option<Uuid>,
    flow_id: Option<Uuid>,
    is_test: bool,
    closed: bool,
    profile_exists: bool,
    live_phone: Option<String>,
    held: bool,
    properties: Option<Value>,
    consent: Option<String>,
    unreachable: bool,
    last_inbound_at: Option<DateTime<Utc>>,
    service_flow: bool,
    start_minute: Option<i32>,
    end_minute: Option<i32>,
    zone_conflict: Option<String>,
    template_status: Option<String>,
    /// The template's category now (Meta can recategorize it after the message was prepared).
    template_category: Option<String>,
    kind: String,
    free_form: bool,
    attempts: i32,
    message_sender: Option<Uuid>,
    tester: bool,
    backoff_until: Option<DateTime<Utc>>,
    /// Already charged on an earlier attempt (a message id is charged at most once). A retry after a
    /// failure Meta reported later is free; retries are capped (`RETRY_WAITS`), and each one costs the
    /// customer Meta's own fee, so a forged failure status gains nothing.
    charged: bool,
}

const CLAIM_SQL: &str = "SELECT m.account_id, m.status, m.to_address, m.campaign_id, m.flow_id, m.is_test, a.closed_at IS NOT NULL AS closed,
        p.id IS NOT NULL AS profile_exists, p.phone_number AS live_phone, COALESCE(p.marketing_hold, false) AS held, p.properties,
        wc.consent, wc.unreachable_at IS NOT NULL AS unreachable, wc.last_inbound_at,
        COALESCE((SELECT f.definition->'settings'->>'service_messages' = 'true' FROM flows f WHERE f.id = m.flow_id AND f.account_id = m.account_id), false) AS service_flow,
        ss.sms_start_minute AS start_minute, ss.sms_end_minute AS end_minute, ss.sms_zone_conflict AS zone_conflict,
        t.status AS template_status, t.category AS template_category,
        w.kind, w.payload->>'type' = 'text' AS free_form, w.attempts, w.sender_id AS message_sender,
        EXISTS (SELECT 1 FROM whatsapp_test_numbers tn JOIN memberships mb ON mb.user_id = tn.user_id AND mb.account_id = tn.account_id
                WHERE tn.account_id = m.account_id AND tn.phone_number = m.to_address AND tn.verified_at IS NOT NULL) AS tester,
        (SELECT max(b.until) FROM whatsapp_backoff b WHERE b.account_id = m.account_id AND b.scope IN ('sender', 'pair:' || m.to_address)) AS backoff_until,
        w.charged_at IS NOT NULL AS charged
     FROM messages m JOIN accounts a ON a.id = m.account_id JOIN whatsapp_sent w ON w.message_id = m.id
     LEFT JOIN profiles p ON p.id = m.profile_id
     LEFT JOIN whatsapp_templates t ON t.id = w.template_id
     LEFT JOIN whatsapp_contacts wc ON wc.account_id = m.account_id AND wc.phone_number = m.to_address
     LEFT JOIN send_settings ss ON ss.account_id = m.account_id
     WHERE m.id = $1 FOR SHARE OF a";

/// The rules, from the state read under the locks. `sender`: the connected sender now;
/// `campaign`: the campaign's status now. None: it may go now.
fn decide(r: &ClaimRow, account: &crate::models::Account, sender: Option<Uuid>, campaign: Option<&str>, strict: bool, now: DateTime<Utc>) -> Option<Stop> {
    if r.closed {
        return Some(Stop::Skip("account closed"));
    }
    if !r.profile_exists {
        return Some(Stop::Skip("profile deleted"));
    }
    if !matches!(r.status.as_str(), "queued" | "sending" | "retrying") {
        return Some(Stop::Skip("no longer queued"));
    }
    if sender.is_none() || r.message_sender != sender {
        return Some(Stop::Skip("WhatsApp sender changed"));
    }
    if r.unreachable {
        return Some(Stop::Skip("not on WhatsApp"));
    }
    let consent = r.consent.as_deref().unwrap_or("never_subscribed");
    // An opt-out stops every template; someone who opted out and writes in can still be answered
    // in words inside the window (and told their opt-out worked).
    if consent == "unsubscribed" && !(r.kind != "marketing" && r.free_form) {
        return Some(Stop::Skip("opted out of WhatsApp"));
    }
    if r.campaign_id.is_some() && !r.is_test {
        // A retry of a message already handed over once may finish after its campaign did.
        let ok = matches!(campaign, Some("sending")) || (campaign == Some("sent") && r.attempts > 0);
        if !ok {
            return Some(Stop::Skip(if matches!(campaign, Some("cancelled") | None) { "campaign cancelled" } else { "campaign stopped" }));
        }
    }
    if !r.free_form && r.template_status.as_deref() != Some("APPROVED") {
        return Some(Stop::Skip("template not approved"));
    }
    if let Some(until) = r.backoff_until.filter(|u| *u > now) {
        return Some(Stop::Defer(until));
    }
    if r.is_test {
        return (!r.tester).then_some(Stop::Skip("not a team member's verified test number"));
    }
    // The category Meta gives the template now decides, not the one it had when prepared.
    let marketing_template = !r.free_form && !matches!(r.template_category.as_deref(), Some("UTILITY" | "AUTHENTICATION"));
    let subscribed = consent == "subscribed";
    if marketing_template && r.held {
        return Some(Stop::Skip(crate::marketing_hold::SKIP_REASON));
    }
    if r.kind != "marketing" {
        // Conversational: free-form only inside the window; templates by their category.
        if r.free_form && !r.last_inbound_at.is_some_and(|at| now - at < Duration::hours(WINDOW_HOURS)) {
            return Some(Stop::Skip("24-hour window closed"));
        }
        if marketing_template && !subscribed {
            return Some(Stop::Skip("no WhatsApp opt-in"));
        }
        return None;
    }
    // Campaigns and flows go to the number the person has now.
    if r.live_phone.as_deref() != Some(r.to_address.as_str()) {
        return Some(Stop::Skip("phone number changed"));
    }
    if strict && r.held {
        return Some(Stop::Skip(crate::marketing_hold::SKIP_REASON));
    }
    if marketing_template {
        if !subscribed {
            return Some(Stop::Skip("no WhatsApp opt-in"));
        }
    } else if !subscribed && !(r.campaign_id.is_none() && r.flow_id.is_some() && r.service_flow) {
        // A utility template to someone who never opted in to WhatsApp: only from a flow marked as
        // service messages (their own order or booking).
        return Some(Stop::Skip(if r.held { crate::marketing_hold::SKIP_REASON } else { "no WhatsApp opt-in" }));
    }
    let ((s, e), skip_conflicts) = crate::features::sendtime::settings::window_from(r.start_minute.zip(r.end_minute), r.zone_conflict.as_deref());
    let zones = crate::features::sendtime::settings::sms_zones(r.properties.as_ref().unwrap_or(&Value::Null), Some(&r.to_address), crate::sending::account_tz(account));
    let hours = crate::features::sendtime::settings::decide(&zones, now, s, e, skip_conflicts);
    if hours.skip {
        return Some(Stop::Skip("time zones disagree"));
    }
    hours.wait.map(Stop::Defer)
}

/// What a claim hands to the worker: exactly what it sends, where, and with which credentials.
struct Claimed {
    row: ClaimRow,
    to: String,
    payload: Value,
    phone_number_id: String,
    token: String,
}

/// The final claim.
async fn claim(state: &AppState, message_id: Uuid, strict: bool) -> AppResult<Result<Claimed, Stop>> {
    let meta: Option<(Uuid, String, Option<Uuid>)> = sqlx::query_as("SELECT account_id, to_address, campaign_id FROM messages WHERE id = $1").bind(message_id).fetch_optional(&state.db).await?;
    let Some((account_id, to, campaign_id)) = meta else { return Ok(Err(Stop::Skip("message deleted"))) };
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let mut tx = state.db.begin().await?;
    // Lock order: the number, the sender's settings, the campaign, then the message.
    consent::lock(&mut tx, account_id, &to).await?;
    let (sender, token_enc, phone_number_id): (Option<Uuid>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT sender_id, access_token_enc, phone_number_id FROM whatsapp_settings WHERE account_id = $1 FOR SHARE")
            .bind(account_id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or((None, None, None));
    let campaign: Option<String> = match campaign_id {
        Some(c) => sqlx::query_scalar("SELECT status FROM campaigns WHERE id = $1 FOR SHARE").bind(c).fetch_optional(&mut *tx).await?,
        None => None,
    };
    let Some(r) = sqlx::query_as::<_, ClaimRow>(CLAIM_SQL).bind(message_id).fetch_optional(&mut *tx).await? else {
        return Ok(Err(Stop::Skip("message deleted")));
    };
    let stop = if r.to_address != to { Some(Stop::Skip("phone number changed")) } else { decide(&r, &account, sender, campaign.as_deref(), strict, Utc::now()) };
    let stop = match stop {
        None => {
            // Hold the credit for it (released if it doesn't go out).
            let amount = if r.is_test || r.charged { 0 } else { price(state) };
            let held = amount <= 0
                || sqlx::query("UPDATE accounts SET balance_micros = balance_micros - $2 WHERE id = $1 AND balance_micros >= $2")
                    .bind(account_id)
                    .bind(amount)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
                    == 1;
            if held {
                let handed: Option<Value> = sqlx::query_scalar(
                    "UPDATE whatsapp_sent SET handed_at = now(), attempts = attempts + 1, sent_to = $2, reserved_micros = $3 WHERE message_id = $1 AND handed_at IS NULL RETURNING payload",
                )
                .bind(message_id)
                .bind(&to)
                .bind(amount.max(0))
                .fetch_optional(&mut *tx)
                .await?;
                let Some(payload) = handed else {
                    // Another worker has it (or had it): nothing here changes.
                    return Ok(Err(Stop::Skip("already handed over")));
                };
                sqlx::query("UPDATE messages SET status = 'sending', claimed_at = COALESCE(claimed_at, now()), error = NULL WHERE id = $1").bind(message_id).execute(&mut *tx).await?;
                tx.commit().await?;
                let token = settings::decrypt(state, token_enc.as_deref().unwrap_or_default())?;
                return Ok(Ok(Claimed { row: r, to, payload, phone_number_id: phone_number_id.unwrap_or_default(), token }));
            }
            Stop::Skip("insufficient credit")
        }
        Some(s) => s,
    };
    if let Stop::Skip(why) = stop {
        let n = sqlx::query("UPDATE messages SET status = 'skipped', error = $2 WHERE id = $1 AND status IN ('queued', 'sending', 'retrying')")
            .bind(message_id)
            .bind(why)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n > 0 && !r.is_test {
            crate::features::messages_unsent_in(&mut tx, &state.db, account_id, &[message_id]).await;
        }
    }
    tx.commit().await?;
    Ok(Err(stop))
}

/// Test hook: hold an account's hand-offs right before their final claim, so a test can change the
/// world exactly there. Never set outside tests.
#[doc(hidden)]
pub mod hook {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use once_cell::sync::Lazy;
    use tokio::sync::Semaphore;
    use uuid::Uuid;

    pub struct Pause {
        arrived: Semaphore,
        go: Semaphore,
    }

    static PAUSES: Lazy<Mutex<HashMap<Uuid, Arc<Pause>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

    impl Pause {
        pub fn hold(account_id: Uuid) -> Arc<Pause> {
            let p = Arc::new(Pause { arrived: Semaphore::new(0), go: Semaphore::new(0) });
            PAUSES.lock().unwrap_or_else(|e| e.into_inner()).insert(account_id, p.clone());
            p
        }
        pub async fn arrived(&self) {
            if let Ok(permit) = self.arrived.acquire().await {
                permit.forget();
            }
        }
        pub fn release(&self, account_id: Uuid) {
            PAUSES.lock().unwrap_or_else(|e| e.into_inner()).remove(&account_id);
            self.go.add_permits(1000);
        }
    }

    pub(super) async fn wait(account_id: Uuid) {
        let p = PAUSES.lock().unwrap_or_else(|e| e.into_inner()).get(&account_id).cloned();
        if let Some(p) = p {
            p.arrived.add_permits(1);
            if let Ok(permit) = p.go.acquire().await {
                permit.forget();
            }
        }
    }
}

/// Claim, call Meta and record the result for a prepared message.
pub async fn hand_off(state: &AppState, message_id: Uuid, strict: bool) -> AppResult<Outcome> {
    let account: Option<Uuid> = sqlx::query_scalar("SELECT account_id FROM messages WHERE id = $1").bind(message_id).fetch_optional(&state.db).await?;
    let Some(account_id) = account else { return Ok(Outcome::Skipped("message deleted")) };
    crate::sending::throttle("whatsapp", f64::from(state.cfg.send_rate_per_sec.max(1))).await;
    hook::wait(account_id).await;
    let c = match claim(state, message_id, strict).await? {
        Ok(c) => c,
        Err(Stop::Skip(why)) => return Ok(Outcome::Skipped(why)),
        Err(Stop::Defer(at)) => return Ok(Outcome::Deferred(at)),
    };
    let result = match tokio::time::timeout(PROVIDER_TIMEOUT, graph::send_message(&state.cfg, &c.token, &c.phone_number_id, &c.to, &c.payload)).await {
        Ok(r) => r,
        Err(_) => Err(graph::GraphError { code: 0, subcode: None, status: 0, unclear: true }),
    };
    match result {
        Ok(acc) => sent(state, &c.row, message_id, &acc).await,
        Err(e) => refused(state, &c.row, message_id, &e).await,
    }
}

async fn sent(state: &AppState, r: &ClaimRow, message_id: Uuid, acc: &graph::Accepted) -> AppResult<Outcome> {
    let mut tx = state.db.begin().await?;
    let changed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE messages SET status = 'sent', provider_message_id = $2, sent_at = now(), error = NULL WHERE id = $1 AND status = 'sending' RETURNING profile_id",
    )
    .bind(message_id)
    .bind(&acc.wamid)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query("UPDATE whatsapp_sent SET wamid = $2, pacing = $3 WHERE message_id = $1").bind(message_id).bind(&acc.wamid).bind(&acc.pacing).execute(&mut *tx).await?;
    // Meta accepted it on DumbMail's own call: that's what's charged. A message Meta holds for a
    // quality check or pacing waits for Meta's "sent" status (or is released if Meta drops it).
    if !matches!(acc.pacing.as_deref(), Some("held_for_quality_assessment" | "paused")) {
        settle(&mut tx, r.account_id, message_id).await?;
    }
    if let Some(wa_id) = &acc.wa_id {
        sqlx::query("UPDATE whatsapp_contacts SET wa_id = $3 WHERE account_id = $1 AND phone_number = $2").bind(r.account_id).bind(&r.to_address).bind(wa_id).execute(&mut *tx).await?;
    }
    if r.kind != "marketing" {
        let preview: Option<String> = sqlx::query_scalar("SELECT body FROM whatsapp_sent WHERE message_id = $1").bind(message_id).fetch_optional(&mut *tx).await?;
        sqlx::query(
            "UPDATE whatsapp_conversations SET last_message_at = now(), last_direction = 'outbound', last_preview = $3,
                    unread_count = CASE WHEN $4 THEN 0 ELSE unread_count END
             WHERE account_id = $1 AND phone_number = $2",
        )
        .bind(r.account_id)
        .bind(&r.to_address)
        .bind(crate::features::inbox::store::preview(&preview.unwrap_or_default(), 0))
        .bind(r.kind == "reply")
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if let (Some(profile_id), false) = (changed, r.is_test) {
        record_event(
            &state.db,
            NewEvent {
                account_id: r.account_id,
                profile_id,
                metric: "Received WhatsApp",
                properties: json!({"campaign_id": r.campaign_id, "flow_id": r.flow_id, "message_id": message_id, "kind": r.kind}),
                value: None,
                occurred_at: None,
                unique_id: Some(message_id.to_string()),
                message_id: Some(message_id),
            },
        )
        .await?;
    }
    Ok(Outcome::Sent(message_id))
}

/// Charge a message's held credit, once, when Meta says it went out. Inside the caller's transaction.
pub async fn settle(conn: &mut sqlx::PgConnection, account_id: Uuid, message_id: Uuid) -> AppResult<()> {
    let held: Option<i64> =
        sqlx::query_scalar(
            "WITH old AS (SELECT message_id, reserved_micros FROM whatsapp_sent WHERE message_id = $1 AND reserved_micros > 0 AND charged_at IS NULL FOR UPDATE)
             UPDATE whatsapp_sent w SET charged_at = now(), reserved_micros = 0 FROM old WHERE w.message_id = old.message_id RETURNING old.reserved_micros",
        )
            .bind(message_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(amount) = held.filter(|a| *a > 0) else { return Ok(()) };
    // The hold becomes the charge: put it back, then debit it through the ledger.
    sqlx::query("UPDATE accounts SET balance_micros = balance_micros + $2 WHERE id = $1").bind(account_id).bind(amount).execute(&mut *conn).await?;
    crate::billing::debit_usage_tx(&mut *conn, account_id, "whatsapp", 1, amount).await?;
    sqlx::query("UPDATE messages SET cost_micros = $2 WHERE id = $1").bind(message_id).bind(amount).execute(&mut *conn).await?;
    Ok(())
}

/// Give back a message's held credit (it didn't go out, or may never be confirmed). Inside the
/// caller's transaction.
pub async fn release(conn: &mut sqlx::PgConnection, account_id: Uuid, message_id: Uuid) -> AppResult<()> {
    let held: Option<i64> =
        sqlx::query_scalar(
            "WITH old AS (SELECT message_id, reserved_micros FROM whatsapp_sent WHERE message_id = $1 AND reserved_micros > 0 AND charged_at IS NULL FOR UPDATE)
             UPDATE whatsapp_sent w SET reserved_micros = 0 FROM old WHERE w.message_id = old.message_id RETURNING old.reserved_micros",
        )
            .bind(message_id)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(amount) = held.filter(|a| *a > 0) {
        sqlx::query("UPDATE accounts SET balance_micros = balance_micros + $2 WHERE id = $1").bind(account_id).bind(amount).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Wait before anything more goes from this sender (or to this person from it, for Meta's pair
/// limit).
pub async fn back_off(db: &sqlx::PgPool, account_id: Uuid, to: &str, code: i64, until: DateTime<Utc>) -> sqlx::Result<()> {
    let scope = if code == 131056 { format!("pair:{to}") } else { "sender".to_string() };
    sqlx::query("INSERT INTO whatsapp_backoff (account_id, scope, until) VALUES ($1, $2, $3) ON CONFLICT (account_id, scope) DO UPDATE SET until = GREATEST(whatsapp_backoff.until, EXCLUDED.until)")
        .bind(account_id)
        .bind(scope)
        .bind(until)
        .execute(db)
        .await?;
    Ok(())
}

/// When a message refused for this reason (after `attempts` hand-offs) may be tried again, if at all.
fn retry_at(class: Class, attempts: i32, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let waits: &[i64] = match class {
        Class::RateLimited | Class::Transient => &RETRY_WAITS,
        Class::NotOnWhatsApp => &UNDELIVERABLE_WAITS,
        _ => return None,
    };
    waits.get((attempts.max(1) - 1) as usize).map(|s| now + Duration::seconds(*s))
}

/// Put a message Meta refused (or failed later) back to wait for `at`: nothing was handed over, its
/// credit is released, and the background lane tries it again (campaigns and flows re-run it
/// themselves when `in_flight`).
pub async fn reschedule(state: &AppState, account_id: Uuid, message_id: Uuid, at: DateTime<Utc>, why: &str, in_flight: Option<(bool, bool)>) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE whatsapp_sent SET handed_at = NULL, wamid = NULL WHERE message_id = $1").bind(message_id).execute(&mut *tx).await?;
    release(&mut tx, account_id, message_id).await?;
    match in_flight {
        // A campaign batch puts it back in its queue with this time (from `sending`).
        Some((true, _)) => {}
        // A flow waits for it and runs the step again.
        Some((false, true)) => {
            sqlx::query("UPDATE messages SET status = 'retrying', error = $2 WHERE id = $1 AND status = 'sending'").bind(message_id).bind(why).execute(&mut *tx).await?;
        }
        _ => {
            sqlx::query("UPDATE messages SET status = 'retrying', error = $2 WHERE id = $1 AND status IN ('sending', 'sent')").bind(message_id).bind(why).execute(&mut *tx).await?;
            crate::jobs::enqueue_background(&mut *tx, "whatsapp_send", json!({"message_id": message_id}), Some(at)).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn refused(state: &AppState, r: &ClaimRow, message_id: Uuid, e: &graph::GraphError) -> AppResult<Outcome> {
    let why = e.describe();
    tracing::warn!(message = %message_id, code = e.code, status = e.status, "WhatsApp send not accepted");
    let class = e.class();
    if class == Class::RateLimited {
        let wait = RETRY_WAITS.get((r.attempts.max(0)) as usize).copied().unwrap_or(7200);
        back_off(&state.db, r.account_id, &r.to_address, e.code, Utc::now() + Duration::seconds(wait)).await?;
    }
    if class == Class::NotOnWhatsApp {
        consent::undeliverable(&state.db, r.account_id, &r.to_address, UNDELIVERABLE_LIMIT).await?;
    }
    // `attempts` counted this hand-off.
    let attempts = r.attempts + 1;
    if let (Some(at), false) = (retry_at(class, attempts, Utc::now()), r.is_test) {
        let in_flight = Some((r.campaign_id.is_some(), r.flow_id.is_some())).filter(|(c, f)| *c || *f);
        reschedule(state, r.account_id, message_id, at, &why, in_flight).await?;
        return Ok(Outcome::Deferred(at));
    }
    match class {
        Class::MarketingOptOut => {
            let mut tx = state.db.begin().await?;
            consent::lock(&mut tx, r.account_id, &r.to_address).await?;
            let profile: Option<Uuid> = sqlx::query_scalar("SELECT profile_id FROM messages WHERE id = $1").bind(message_id).fetch_optional(&mut *tx).await?;
            consent::set(&mut tx, &state.db, r.account_id, &r.to_address, profile, "unsubscribed", "meta_error", json!({"code": e.code, "message_id": message_id}), Utc::now()).await?;
            fail(&mut tx, state, r, message_id, &why, e.code).await?;
            tx.commit().await?;
            return finish_failed(state, r, message_id, why).await;
        }
        Class::TemplatePaused => {
            let tid: Option<Uuid> = sqlx::query_scalar("SELECT template_id FROM whatsapp_sent WHERE message_id = $1").bind(message_id).fetch_optional(&state.db).await?.flatten();
            if let Some(tid) = tid {
                if templates::mark_paused(&state.db, r.account_id, tid, e.code).await? {
                    let name: String = sqlx::query_scalar("SELECT name FROM whatsapp_templates WHERE id = $1").bind(tid).fetch_one(&state.db).await?;
                    super::alert_template(state, r.account_id, &name, if e.code == 132016 { "DISABLED" } else { "PAUSED" }).await;
                }
            }
        }
        Class::Restricted => {
            settings::record_error(&state.db, r.account_id, &why).await?;
            super::alert_quality(state, r.account_id, &why).await;
        }
        Class::Auth => {
            settings::record_error(&state.db, r.account_id, &why).await?;
        }
        _ => {}
    }
    let fail_why = if class == Class::Unclear { "no answer from Meta; not sent again".to_string() } else { why };
    let mut tx = state.db.begin().await?;
    fail(&mut tx, state, r, message_id, &fail_why, e.code).await?;
    tx.commit().await?;
    finish_failed(state, r, message_id, fail_why).await
}

/// Mark a handed-over message failed and give back its credit, inside the caller's transaction.
async fn fail(tx: &mut sqlx::PgConnection, state: &AppState, r: &ClaimRow, message_id: Uuid, why: &str, code: i64) -> AppResult<()> {
    let n = sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1 AND status = 'sending'").bind(message_id).bind(why).execute(&mut *tx).await?.rows_affected();
    sqlx::query("UPDATE whatsapp_sent SET wa_status = 'failed', wa_status_at = now(), error_code = $2 WHERE message_id = $1").bind(message_id).bind(code as i32).execute(&mut *tx).await?;
    release(&mut *tx, r.account_id, message_id).await?;
    if n > 0 && !r.is_test {
        crate::features::messages_unsent_in(&mut *tx, &state.db, r.account_id, &[message_id]).await;
    }
    Ok(())
}

async fn finish_failed(_state: &AppState, r: &ClaimRow, _message_id: Uuid, why: String) -> AppResult<Outcome> {
    if r.is_test {
        return Err(AppError::bad(format!("The test wasn't sent: {why}")));
    }
    Ok(Outcome::Failed(why))
}

/// The background job for a queued conversational message (keyword answers) and for retries.
pub async fn run_job(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["message_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let out = hand_off(state, id, false).await;
    // A shopping agent answer: its conversation's fee is charged only when Meta took it.
    match &out {
        Ok(Outcome::Sent(_)) => crate::features::agent::delivery::whatsapp_outcome(state, id, true).await,
        Ok(Outcome::Skipped(_) | Outcome::Failed(_)) | Err(_) => crate::features::agent::delivery::whatsapp_outcome(state, id, false).await,
        Ok(Outcome::Deferred(_)) => {}
    }
    match out {
        // A wait at the claim (quiet hours, Meta's back-off): try again then, on the background lane.
        Ok(Outcome::Deferred(at)) => {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM whatsapp_sent WHERE message_id = $1 AND handed_at IS NULL)").bind(id).fetch_one(&state.db).await?;
            let queued: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = 'whatsapp_send' AND payload->>'message_id' = $1 AND run_at >= $2)")
                .bind(id.to_string())
                .bind(at)
                .fetch_one(&state.db)
                .await?;
            if waiting && !queued {
                crate::jobs::enqueue_background(&state.db, "whatsapp_send", json!({"message_id": id}), Some(at)).await?;
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(message = %id, error = %e.public_message(), "WhatsApp message not sent"),
    }
    Ok(())
}

/// Upkeep: settle credit still held three days after the hand-off. Meta took a message it
/// gave an id for, so that's charged; one without an id (no answer) is given back.
pub async fn release_stale_holds(db: &sqlx::PgPool) -> anyhow::Result<()> {
    let stale: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(
        "SELECT account_id, message_id, wamid IS NOT NULL FROM whatsapp_sent WHERE reserved_micros > 0 AND charged_at IS NULL AND handed_at < now() - interval '3 days' LIMIT 500",
    )
    .fetch_all(db)
    .await?;
    for (account_id, message_id, accepted) in stale {
        let mut tx = db.begin().await?;
        if accepted {
            settle(&mut tx, account_id, message_id).await?;
        } else {
            release(&mut tx, account_id, message_id).await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> ClaimRow {
        ClaimRow {
            account_id: Uuid::nil(),
            status: "queued".into(),
            to_address: "+15035550100".into(),
            campaign_id: None,
            flow_id: Some(Uuid::nil()),
            is_test: false,
            closed: false,
            profile_exists: true,
            live_phone: Some("+15035550100".into()),
            held: false,
            properties: None,
            consent: None,
            unreachable: false,
            last_inbound_at: None,
            service_flow: false,
            start_minute: Some(0),
            end_minute: Some(24 * 60),
            zone_conflict: None,
            template_status: Some("APPROVED".into()),
            template_category: Some("MARKETING".into()),
            kind: "marketing".into(),
            free_form: false,
            attempts: 0,
            message_sender: Some(Uuid::nil()),
            tester: false,
            backoff_until: None,
            charged: false,
        }
    }

    fn account() -> crate::models::Account {
        crate::models::Account {
            id: Uuid::nil(),
            public_key: String::new(),
            name: "A".into(),
            website: None,
            from_name: None,
            from_email: None,
            reply_to: None,
            sms_from: None,
            timezone: "UTC".into(),
            address: None,
            brand_color: "#000".into(),
            logo_url: None,
            social_links: json!({}),
            plan: "free".into(),
            onboarding: json!({}),
            conversion_metric: "Placed Order".into(),
            created_at: Utc::now(),
            sending_paused_at: None,
            sending_paused_reason: None,
            closed_at: None,
            email_daily_limit_override: None,
            email_sunset_enabled: true,
            link_domain: None,
            link_domain_status: "not_started".into(),
            link_domain_records: json!([]),
            verified_domains: Vec::new(),
        }
    }

    #[test]
    fn rules() {
        let a = account();
        let now = Utc::now();
        let s = Some(Uuid::nil());
        let skip = |r: &ClaimRow, strict| match decide(r, &a, s, None, strict, now) {
            Some(Stop::Skip(w)) => Some(w),
            _ => None,
        };
        // Marketing needs a WhatsApp opt-in, whatever their texts say.
        assert_eq!(skip(&row(), false), Some("no WhatsApp opt-in"));
        let mut r = row();
        r.consent = Some("subscribed".into());
        assert_eq!(skip(&r, false), None);
        // The marketing hold applies to marketing templates on every path.
        r.held = true;
        assert_eq!(skip(&r, false), Some(crate::marketing_hold::SKIP_REASON));
        r.held = false;
        r.consent = Some("unsubscribed".into());
        assert_eq!(skip(&r, false), Some("opted out of WhatsApp"));
        // Utility to someone without an opt-in: only from a service-messages flow.
        let mut r = row();
        r.template_category = Some("UTILITY".into());
        r.held = true;
        assert_eq!(skip(&r, false), Some(crate::marketing_hold::SKIP_REASON));
        r.service_flow = true;
        assert_eq!(skip(&r, false), None);
        // Recategorized as marketing since it was prepared: the hold and the opt-in rule apply.
        r.template_category = Some("MARKETING".into());
        assert_eq!(skip(&r, false), Some(crate::marketing_hold::SKIP_REASON));
        // Free-form replies only inside the window.
        let mut r = row();
        r.kind = "reply".into();
        r.free_form = true;
        r.template_status = None;
        r.last_inbound_at = Some(now - Duration::hours(25));
        assert_eq!(skip(&r, false), Some("24-hour window closed"));
        r.last_inbound_at = Some(now - Duration::hours(2));
        assert_eq!(skip(&r, false), None);
        // Another sender, another number, a test to a non-member: stopped.
        let mut r = row();
        r.consent = Some("subscribed".into());
        r.message_sender = Some(Uuid::now_v7());
        assert_eq!(skip(&r, false), Some("WhatsApp sender changed"));
        let mut r = row();
        r.consent = Some("subscribed".into());
        r.live_phone = Some("+15035550111".into());
        assert_eq!(skip(&r, false), Some("phone number changed"));
        let mut r = row();
        r.is_test = true;
        assert_eq!(skip(&r, false), Some("not a team member's verified test number"));
        r.tester = true;
        assert_eq!(skip(&r, false), None);
        // A back-off waits.
        let mut r = row();
        r.consent = Some("subscribed".into());
        r.backoff_until = Some(now + Duration::minutes(5));
        assert!(matches!(decide(&r, &a, s, None, false, now), Some(Stop::Defer(_))));
    }

    #[test]
    fn retries_are_bounded() {
        let now = Utc::now();
        assert!(retry_at(Class::RateLimited, 1, now).is_some());
        assert!(retry_at(Class::RateLimited, 5, now).is_none());
        assert!(retry_at(Class::NotOnWhatsApp, 2, now).is_some());
        assert!(retry_at(Class::NotOnWhatsApp, 3, now).is_none());
        assert!(retry_at(Class::Unclear, 1, now).is_none());
    }
}
