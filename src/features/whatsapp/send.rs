//! WhatsApp as a channel: sending one message (a `messages` row, channel `whatsapp`, with its
//! `whatsapp_sent` row) for campaigns, flows, tests and inbox replies.
//!
//! A send happens in three writes:
//!
//! 1. **Prepare** (one transaction): the message row and what it sends (`whatsapp_sent.payload`),
//!    filled once; a retry sends the same thing.
//! 2. **The final claim**, right before the provider call, in one transaction holding the number's
//!    lock (the one an inbound STOP and Meta's opt-out signal take): the account is open, the person
//!    exists, the number isn't opted out of WhatsApp or known to be off it, the campaign is still
//!    sending, the template is still approved, and then the rules for the kind of message (consent
//!    and quiet hours for marketing; the 24-hour window for free-form replies). It marks the message
//!    `sending` and `handed_at`; a message handed over without an answer is never sent again.
//! 3. **The result** (one transaction): the status write from `sending` to `sent` carries the charge,
//!    so a message is billed once. A certain refusal (rate limits, Meta busy) clears the hand-off and
//!    waits; the error classes Meta documents mark the number off WhatsApp, record an opt-out, or stop
//!    a paused template.

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
    let amount = if spec.is_test { 0 } else { price(state) };
    if amount > 0 && crate::billing::balance(&state.db, account.id).await? < amount {
        if spec.is_test {
            return Err(AppError::LimitReached("your balance is empty; add credit in Settings → Billing".into()));
        }
        return early(state, spec, "insufficient credit", "").await;
    }
    let Some(()) = prepare(state, spec, message_id, &phone, &t, &rendered).await? else {
        return Ok(Outcome::Skipped("already handled"));
    };
    hand_off(state, message_id, strict).await
}

/// Write the message and what it sends. None: the message was already settled (or its campaign
/// row is gone with its person).
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
    sqlx::query(
        "INSERT INTO whatsapp_sent (message_id, account_id, kind, template_id, template_name, category, body, payload)
         VALUES ($1, $2, 'marketing', $3, $4, $5, $6, $7) ON CONFLICT (message_id) DO NOTHING",
    )
    .bind(message_id)
    .bind(spec.account.id)
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

/// A conversational message (an inbox reply, a keyword's answer, a confirmation): written queued,
/// inside the caller's transaction when given. `template`: an approved template and its filled
/// payload; otherwise free-form `text`.
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
    sqlx::query(
        "INSERT INTO messages (id, account_id, profile_id, channel, to_address, subject) VALUES ($1, $2, $3, 'whatsapp', $4, $5)",
    )
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
    sqlx::query(
        "INSERT INTO whatsapp_sent (message_id, account_id, kind, sent_by, template_id, template_name, category, body, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(message_id)
    .bind(account_id)
    .bind(kind)
    .bind(sent_by)
    .bind(template.map(|(t, _)| t.id))
    .bind(&subject)
    .bind(template.map(|(t, _)| t.category.clone()))
    .bind(&body)
    .bind(&payload)
    .execute(&mut *conn)
    .await?;
    Ok(message_id)
}

/// What the final claim reads, in one statement.
#[derive(sqlx::FromRow)]
struct ClaimRow {
    account_id: Uuid,
    status: String,
    to_address: String,
    campaign_id: Option<Uuid>,
    flow_id: Option<Uuid>,
    is_test: bool,
    closed: bool,
    profile_exists: bool,
    held: bool,
    properties: Option<Value>,
    consent: Option<String>,
    unreachable: bool,
    last_inbound_at: Option<DateTime<Utc>>,
    campaign_status: Option<String>,
    service_flow: bool,
    start_minute: Option<i32>,
    end_minute: Option<i32>,
    zone_conflict: Option<String>,
    template_status: Option<String>,
    kind: String,
    category: Option<String>,
    free_form: bool,
}

const CLAIM_SQL: &str = "SELECT m.account_id, m.status, m.to_address, m.campaign_id, m.flow_id, m.is_test, a.closed_at IS NOT NULL AS closed,
        p.id IS NOT NULL AS profile_exists, COALESCE(p.marketing_hold, false) AS held, p.properties,
        wc.consent, wc.unreachable_at IS NOT NULL AS unreachable, wc.last_inbound_at,
        (SELECT c.status FROM campaigns c WHERE c.id = m.campaign_id) AS campaign_status,
        COALESCE((SELECT f.definition->'settings'->>'service_messages' = 'true' FROM flows f WHERE f.id = m.flow_id AND f.account_id = m.account_id), false) AS service_flow,
        ss.sms_start_minute AS start_minute, ss.sms_end_minute AS end_minute, ss.sms_zone_conflict AS zone_conflict,
        (SELECT t.status FROM whatsapp_templates t WHERE t.id = w.template_id) AS template_status,
        w.kind, w.category, w.payload->>'type' = 'text' AS free_form
     FROM messages m JOIN accounts a ON a.id = m.account_id JOIN whatsapp_sent w ON w.message_id = m.id
     LEFT JOIN profiles p ON p.id = m.profile_id
     LEFT JOIN whatsapp_contacts wc ON wc.account_id = m.account_id AND wc.phone_number = m.to_address
     LEFT JOIN send_settings ss ON ss.account_id = m.account_id
     WHERE m.id = $1 FOR SHARE OF a";

/// The rules, from the row read under the number's lock. None: it may go now.
fn decide(r: &ClaimRow, account: &crate::models::Account, strict: bool, now: DateTime<Utc>) -> Option<Stop> {
    if r.closed {
        return Some(Stop::Skip("account closed"));
    }
    if !r.profile_exists {
        return Some(Stop::Skip("profile deleted"));
    }
    if !matches!(r.status.as_str(), "queued" | "sending" | "retrying") {
        return Some(Stop::Skip("no longer queued"));
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
    if !r.is_test && r.campaign_id.is_some() && r.campaign_status.as_deref() != Some("sending") {
        return Some(Stop::Skip(if matches!(r.campaign_status.as_deref(), Some("cancelled") | None) { "campaign cancelled" } else { "campaign stopped" }));
    }
    if !r.free_form && r.template_status.as_deref() != Some("APPROVED") {
        return Some(Stop::Skip("template not approved"));
    }
    if r.is_test {
        return None;
    }
    let marketing_template = !r.free_form && !matches!(r.category.as_deref(), Some("UTILITY" | "AUTHENTICATION"));
    let subscribed = consent == "subscribed";
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

/// The final claim. Ok(None): claimed, call Meta now.
async fn claim(state: &AppState, message_id: Uuid, strict: bool) -> AppResult<Result<ClaimRow, Stop>> {
    let meta: Option<(Uuid, String)> = sqlx::query_as("SELECT account_id, to_address FROM messages WHERE id = $1").bind(message_id).fetch_optional(&state.db).await?;
    let Some((account_id, to)) = meta else { return Ok(Err(Stop::Skip("message deleted"))) };
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let mut tx = state.db.begin().await?;
    consent::lock(&mut tx, account_id, &to).await?;
    let Some(r) = sqlx::query_as::<_, ClaimRow>(CLAIM_SQL).bind(message_id).fetch_optional(&mut *tx).await? else {
        return Ok(Err(Stop::Skip("message deleted")));
    };
    match decide(&r, &account, strict, Utc::now()) {
        Some(Stop::Skip(why)) => {
            let n = sqlx::query("UPDATE messages SET status = 'skipped', error = $2 WHERE id = $1 AND status IN ('queued', 'sending', 'retrying')")
                .bind(message_id)
                .bind(why)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if n > 0 && !r.is_test {
                crate::features::messages_unsent_in(&mut tx, &state.db, account_id, &[message_id]).await;
            }
            tx.commit().await?;
            Ok(Err(Stop::Skip(why)))
        }
        Some(Stop::Defer(at)) => {
            tx.commit().await?;
            Ok(Err(Stop::Defer(at)))
        }
        None => {
            let handed: Option<Uuid> = sqlx::query_scalar("UPDATE whatsapp_sent SET handed_at = now(), attempts = attempts + 1 WHERE message_id = $1 AND handed_at IS NULL RETURNING message_id")
                .bind(message_id)
                .fetch_optional(&mut *tx)
                .await?;
            if handed.is_none() {
                tx.commit().await?;
                return Ok(Err(Stop::Skip("already handed over")));
            }
            sqlx::query("UPDATE messages SET status = 'sending', claimed_at = COALESCE(claimed_at, now()) WHERE id = $1").bind(message_id).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(Ok(r))
        }
    }
}

/// Test hook: hold an account's hand-offs right before their final claim, so a test can land an
/// opt-out exactly there. Never set outside tests.
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
    let meta: Option<(Uuid, String, bool, Value)> = sqlx::query_as(
        "SELECT m.account_id, m.to_address, m.is_test, w.payload FROM messages m JOIN whatsapp_sent w ON w.message_id = m.id WHERE m.id = $1",
    )
    .bind(message_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, to, is_test, payload)) = meta else { return Ok(Outcome::Skipped("message deleted")) };
    let Some(conn) = settings::load(state, account_id).await? else {
        mark(state, account_id, message_id, "skipped", "WhatsApp isn't connected", is_test).await?;
        return Ok(Outcome::Skipped("WhatsApp isn't connected"));
    };
    crate::sending::throttle("whatsapp", f64::from(state.cfg.send_rate_per_sec.max(1))).await;
    hook::wait(account_id).await;
    let r = match claim(state, message_id, strict).await? {
        Ok(r) => r,
        Err(Stop::Skip(why)) => return Ok(Outcome::Skipped(why)),
        Err(Stop::Defer(at)) => return Ok(Outcome::Deferred(at)),
    };
    let result = match tokio::time::timeout(PROVIDER_TIMEOUT, graph::send_message(&state.cfg, &conn.token, &conn.phone_number_id, &to, &payload)).await {
        Ok(r) => r,
        Err(_) => Err(graph::GraphError { code: 0, subcode: None, status: 0, unclear: true }),
    };
    match result {
        Ok(acc) => sent(state, &r, message_id, &acc).await,
        Err(e) => refused(state, &r, message_id, &e).await,
    }
}

async fn sent(state: &AppState, r: &ClaimRow, message_id: Uuid, acc: &graph::Accepted) -> AppResult<Outcome> {
    let amount = if r.is_test { 0 } else { price(state) };
    let mut tx = state.db.begin().await?;
    let changed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE messages SET status = 'sent', provider_message_id = $2, sent_at = now(), error = NULL, cost_micros = $3 WHERE id = $1 AND status = 'sending' RETURNING profile_id",
    )
    .bind(message_id)
    .bind(&acc.wamid)
    .bind(amount)
    .fetch_optional(&mut *tx)
    .await?;
    // The charge goes with the one write that marks it sent.
    if changed.is_some() && amount > 0 {
        crate::billing::debit_usage_tx(&mut tx, r.account_id, "whatsapp", 1, amount).await?;
    }
    sqlx::query("UPDATE whatsapp_sent SET wamid = $2, wa_status = COALESCE(wa_status, 'sent'), wa_status_at = COALESCE(wa_status_at, now()) WHERE message_id = $1")
        .bind(message_id)
        .bind(&acc.wamid)
        .execute(&mut *tx)
        .await?;
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

async fn refused(state: &AppState, r: &ClaimRow, message_id: Uuid, e: &graph::GraphError) -> AppResult<Outcome> {
    let why = e.describe();
    tracing::warn!(message = %message_id, code = e.code, status = e.status, "WhatsApp send not accepted");
    match e.class() {
        // A certain refusal: nothing went. Clear the hand-off and wait, a few times.
        Class::RateLimited | Class::Transient => {
            let attempts: i32 = sqlx::query_scalar("UPDATE whatsapp_sent SET handed_at = NULL WHERE message_id = $1 RETURNING attempts").bind(message_id).fetch_one(&state.db).await?;
            let wait = RETRY_WAITS.get((attempts.max(1) - 1) as usize).copied();
            match wait {
                Some(secs) if !r.is_test => {
                    let at = Utc::now() + Duration::seconds(secs);
                    if r.campaign_id.is_some() {
                        // Back to 'sending': the campaign batch puts it back in the queue with its time.
                        return Ok(Outcome::Deferred(at));
                    }
                    sqlx::query("UPDATE messages SET status = 'retrying', error = $2 WHERE id = $1 AND status = 'sending'").bind(message_id).bind(&why).execute(&state.db).await?;
                    if r.flow_id.is_none() {
                        // Inbox replies and keyword answers retry in their own job.
                        crate::jobs::enqueue(&state.db, "whatsapp_send", json!({"message_id": message_id}), Some(at)).await?;
                    }
                    return Ok(Outcome::Deferred(at));
                }
                _ => {}
            }
        }
        Class::NotOnWhatsApp => {
            consent::mark_unreachable(&state.db, r.account_id, &r.to_address).await?;
        }
        Class::MarketingOptOut => {
            let mut tx = state.db.begin().await?;
            consent::lock(&mut tx, r.account_id, &r.to_address).await?;
            let profile: Option<Uuid> = sqlx::query_scalar("SELECT profile_id FROM messages WHERE id = $1").bind(message_id).fetch_optional(&mut *tx).await?;
            consent::set(&mut tx, &state.db, r.account_id, &r.to_address, profile, "unsubscribed", "meta_error", json!({"code": e.code, "message_id": message_id})).await?;
            tx.commit().await?;
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
        Class::Auth => {
            settings::record_error(&state.db, r.account_id, &why).await?;
        }
        _ => {}
    }
    let fail = if e.class() == Class::Unclear { "no answer from Meta; not sent again".to_string() } else { why.clone() };
    sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1 AND status = 'sending'").bind(message_id).bind(&fail).execute(&state.db).await?;
    sqlx::query("UPDATE whatsapp_sent SET wa_status = 'failed', wa_status_at = now(), error_code = $2 WHERE message_id = $1").bind(message_id).bind(e.code as i32).execute(&state.db).await?;
    if !r.is_test {
        crate::features::messages_unsent(&state.db, r.account_id, &[message_id]).await;
    }
    if r.is_test {
        return Err(AppError::bad(format!("The test wasn't sent: {fail}")));
    }
    Ok(Outcome::Failed(fail))
}

/// The background job for a queued conversational message (keyword answers, retries of replies).
pub async fn run_job(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(id) = payload["message_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    if let Err(e) = hand_off(state, id, false).await {
        tracing::warn!(message = %id, error = %e.public_message(), "WhatsApp message not sent");
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
            held: false,
            properties: None,
            consent: None,
            unreachable: false,
            last_inbound_at: None,
            campaign_status: None,
            service_flow: false,
            start_minute: Some(0),
            end_minute: Some(24 * 60),
            zone_conflict: None,
            template_status: Some("APPROVED".into()),
            kind: "marketing".into(),
            category: Some("MARKETING".into()),
            free_form: false,
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
        let skip = |r: &ClaimRow, strict| match decide(r, &a, strict, now) {
            Some(Stop::Skip(w)) => Some(w),
            _ => None,
        };
        // Marketing needs a WhatsApp opt-in, whatever their texts say.
        assert_eq!(skip(&row(), false), Some("no WhatsApp opt-in"));
        let mut r = row();
        r.consent = Some("subscribed".into());
        assert_eq!(skip(&r, false), None);
        r.consent = Some("unsubscribed".into());
        assert_eq!(skip(&r, false), Some("opted out of WhatsApp"));
        // Utility to someone without an opt-in: only from a service-messages flow.
        let mut r = row();
        r.category = Some("UTILITY".into());
        r.held = true;
        assert_eq!(skip(&r, false), Some(crate::marketing_hold::SKIP_REASON));
        r.service_flow = true;
        assert_eq!(skip(&r, false), None);
        r.campaign_id = Some(Uuid::nil());
        r.campaign_status = Some("sending".into());
        assert!(skip(&r, false).is_some(), "campaigns never reach people without an opt-in");
        // Free-form replies only inside the window.
        let mut r = row();
        r.kind = "reply".into();
        r.free_form = true;
        r.template_status = None;
        r.last_inbound_at = Some(now - Duration::hours(25));
        assert_eq!(skip(&r, false), Some("24-hour window closed"));
        r.last_inbound_at = Some(now - Duration::hours(2));
        assert_eq!(skip(&r, false), None);
        let mut r = row();
        r.unreachable = true;
        r.consent = Some("subscribed".into());
        assert_eq!(skip(&r, false), Some("not on WhatsApp"));
    }
}
