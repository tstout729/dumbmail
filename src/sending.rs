//! Delivering one message: eligibility, quiet hours, rendering, provider call, bookkeeping.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::audience::{record_event, NewEvent};
use crate::error::{AppError, AppResult};
use crate::models::{normalize_phone, Account, Profile};
use crate::providers::email::OutgoingEmail;
use crate::providers::sms::OutgoingSms;
use crate::render::{self, EmailContent, SmsContent, Tracking};
use crate::state::AppState;

pub struct SendSpec<'a> {
    pub account: &'a Account,
    pub profile: &'a Profile,
    pub channel: &'a str,
    pub content: &'a Value,
    pub campaign_id: Option<Uuid>,
    pub flow_id: Option<Uuid>,
    pub flow_node_id: Option<&'a str>,
    pub event: Option<&'a Value>,
    pub is_test: bool,
    /// Existing queued message row (campaign sends pre-create them).
    pub message_id: Option<Uuid>,
    /// Override destination (test sends to arbitrary addresses).
    pub to_override: Option<String>,
}

#[derive(Debug)]
pub enum Outcome {
    Sent(Uuid),
    Skipped(&'static str),
    Deferred(DateTime<Utc>),
    Failed(String),
}

static LIMITER: Lazy<Mutex<HashMap<&'static str, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) async fn throttle(channel: &'static str, per_sec: f64) {
    let gap = Duration::from_secs_f64(1.0 / per_sec.max(0.1));
    let wait = {
        let mut m = LIMITER.lock().await;
        let now = Instant::now();
        let next = m.get(channel).copied().unwrap_or(now).max(now);
        m.insert(channel, next + gap);
        next.saturating_duration_since(now)
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

/// How long a claimed text may wait on the provider before it counts as failed (not sent again).
pub const SMS_PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);

/// Test hook: hold an account's texts right after their rate-limiter wait, so a test can land an
/// opt-out exactly there. Never set outside tests.
#[doc(hidden)]
pub struct LimiterPause {
    arrived: tokio::sync::Semaphore,
    go: tokio::sync::Semaphore,
}

static LIMITER_PAUSES: Lazy<std::sync::Mutex<HashMap<Uuid, std::sync::Arc<LimiterPause>>>> = Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

impl LimiterPause {
    /// Hold every text this account sends after its limiter wait until [`LimiterPause::release`].
    #[doc(hidden)]
    pub fn hold(account_id: Uuid) -> std::sync::Arc<Self> {
        let p = std::sync::Arc::new(Self { arrived: tokio::sync::Semaphore::new(0), go: tokio::sync::Semaphore::new(0) });
        if let Ok(mut m) = LIMITER_PAUSES.lock() {
            m.insert(account_id, p.clone());
        }
        p
    }

    /// Wait until a text is being held.
    pub async fn arrived(&self) {
        if let Ok(permit) = self.arrived.acquire().await {
            permit.forget();
        }
    }

    /// Let held texts go, and stop holding this account's texts.
    pub fn release(&self, account_id: Uuid) {
        if let Ok(mut m) = LIMITER_PAUSES.lock() {
            m.remove(&account_id);
        }
        self.go.add_permits(1000);
    }
}

async fn limiter_pause(account_id: Uuid) {
    let pause = LIMITER_PAUSES.lock().ok().and_then(|m| m.get(&account_id).cloned());
    if let Some(p) = pause {
        p.arrived.add_permits(1);
        if let Ok(permit) = p.go.acquire().await {
            permit.forget();
        }
    }
}

/// Wait for the next email slot: never faster than the provider allows (SES's rate for the account), nor than configured.
pub async fn throttle_email(state: &AppState) {
    let configured = f64::from(state.cfg.send_rate_per_sec.max(1));
    throttle("email", state.email.max_send_rate().map_or(configured, |r| r.min(configured))).await;
}

pub fn account_tz(account: &Account) -> chrono_tz::Tz {
    account.timezone.parse().unwrap_or(chrono_tz::America::Los_Angeles)
}

/// SMS may only go out 9am–8pm in the account's time zone. Returns the next allowed time if outside.
pub fn sms_quiet_until(account: &Account, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let tz = account_tz(account);
    let local = now.with_timezone(&tz);
    if (9..20).contains(&local.hour()) {
        return None;
    }
    let day = if local.hour() >= 20 { local.date_naive().succ_opt()? } else { local.date_naive() };
    let at = tz.with_ymd_and_hms(day.year(), day.month(), day.day(), 9, 0, 0).earliest()?;
    Some(at.with_timezone(&Utc))
}

/// With a shared SES account, each customer may only send from the domain they verified.
pub fn check_sender_domain(state: &AppState, account: &Account, from_email: &str) -> AppResult<()> {
    if state.email.name() != "ses" {
        return Ok(());
    }
    check_sender_domain_name(account, from_email).map_err(AppError::BadRequest)
}

pub fn check_sender_domain_name(account: &Account, from_email: &str) -> Result<(), String> {
    let from_domain = from_email.rsplit_once('@').map(|(_, d)| d.to_lowercase()).unwrap_or_default();
    let ok = account.verified_domains.iter().any(|d| from_domain == *d || from_domain.ends_with(&format!(".{d}")));
    if ok { Ok(()) } else { Err(format!("Verify {from_domain} in Settings → Domains before sending from {from_email}")) }
}

pub fn unsubscribe_url(state: &AppState, account: &Account, message_id: Uuid) -> String {
    format!("{}/u/{}", crate::domains::link_base(state, account), crate::auth::sign(&state.cfg.secret_key, &format!("u{}", message_id.simple())))
}

/// A conversational text: an answer to someone who texted in (see `features::inbox`). It skips the
/// marketing-consent check and quiet hours (the inbox's reply rules apply instead, checked again
/// right before the provider call), goes to the number that texted from the number they texted,
/// never reaches a number that opted out, and is billed and recorded like any other text.
pub struct Conversational {
    /// `reply` (typed by a person), `auto_reply`, `keyword_reply`, `confirmation`, `optin_request`
    /// or `help`. None of these count as marketing for smart sending.
    pub kind: &'static str,
    pub sent_by: Option<Uuid>,
    /// Their number and ours, from the text being answered.
    pub to: String,
    pub from: String,
}

pub async fn send(state: &AppState, spec: SendSpec<'_>) -> AppResult<Outcome> {
    send_with(state, spec, None, false).await
}

/// [`send`] under the strict marketing rules, which the last check before the provider applies in
/// its own statement: an email needs the person subscribed (not just never asked), and a marketing
/// hold blocks it with no exception (service-message flows included); a text is held back by a
/// marketing hold too. Also used by review reminders: the same check refuses one whose request was
/// answered meanwhile (see `features::reviews::requests::answered_sql`).
pub async fn send_strict_marketing(state: &AppState, spec: SendSpec<'_>) -> AppResult<Outcome> {
    send_with(state, spec, None, true).await
}

/// Send a conversational SMS through the normal path (billing, provider, bookkeeping, events).
pub async fn send_conversational(state: &AppState, spec: SendSpec<'_>, conversational: &Conversational) -> AppResult<Outcome> {
    send_with(state, spec, Some(conversational), false).await
}

async fn send_with(state: &AppState, spec: SendSpec<'_>, conversational: Option<&Conversational>, strict: bool) -> AppResult<Outcome> {
    // A closed account sends nothing on any channel: campaigns, flows, replies, alerts, tests.
    if spec.account.closed_at.is_some() {
        if spec.is_test {
            return Err(AppError::Conflict("this account is closed, so it can't send".into()));
        }
        if let Some(mid) = spec.message_id {
            sqlx::query("UPDATE messages SET status = 'skipped', error = 'account closed' WHERE id = $1 AND status IN ('queued', 'sending')").bind(mid).execute(&state.db).await?;
        }
        return Ok(Outcome::Skipped("account closed"));
    }
    // Push has its own consent, claim and providers (see features::push::send); the steps match these.
    if spec.channel == "push" && conversational.is_none() {
        return crate::features::push::send::send(state, &spec).await;
    }
    let p = spec.profile;
    // SMS: a note when the phone's and the profile's time zones disagreed (kept on the message).
    let mut send_note: Option<String> = None;
    let to = match spec.channel {
        "email" => {
            // `pending`: a double opt-in sign-up that hasn't confirmed gets nothing until it does.
            if !spec.is_test && (p.email_consent == "unsubscribed" || p.email_consent == "pending" || p.email.is_none()) {
                return Ok(Outcome::Skipped("not emailable"));
            }
            if !spec.is_test {
                // No row: the person was deleted after the caller loaded them (a campaign batch, a flow step).
                let Some((suppressed, held)): Option<(Option<String>, bool)> =
                    sqlx::query_as(&format!("SELECT p.email_suppressed_reason, {} FROM profiles p WHERE p.id = $1", crate::marketing_hold::blocks_email_sql("$2")))
                        .bind(p.id)
                        .bind(spec.flow_id)
                        .fetch_optional(&state.db)
                        .await?
                else {
                    return Ok(Outcome::Skipped("profile deleted"));
                };
                if suppressed.is_some() {
                    return Ok(Outcome::Skipped("suppressed"));
                }
                if held {
                    return Ok(Outcome::Skipped(crate::marketing_hold::SKIP_REASON));
                }
            }
            spec.to_override.clone().or_else(|| p.email.clone())
        }
        "sms" if conversational.is_some() => {
            let c = conversational.expect("checked");
            if p.sms_consent == "unsubscribed" && c.kind != "help" {
                return Ok(Outcome::Skipped("texted STOP"));
            }
            normalize_phone(&c.to)
        }
        "sms" => {
            if !spec.is_test && (p.sms_consent != "subscribed" || p.phone_number.is_none()) {
                return Ok(Outcome::Skipped("no sms consent"));
            }
            // Quiet hours in the recipient's own time zone: their phone number's (the account's when we can't tell).
            if !spec.is_test {
                let hours = crate::features::sendtime::sms_hours(&state.db, spec.account, p, Utc::now()).await?;
                if hours.skip {
                    let why = "time zones disagree";
                    match spec.message_id {
                        Some(mid) => {
                            sqlx::query("UPDATE messages SET send_note = $2 WHERE id = $1").bind(mid).bind(&hours.note).execute(&state.db).await?;
                        }
                        None => {
                            let mid = Uuid::now_v7();
                            sqlx::query(
                                "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, status, error, send_note)
                                 VALUES ($1, $2, $3, 'sms', $4, $5, $6, $7, 'skipped', $8, $9)",
                            )
                            .bind(mid)
                            .bind(spec.account.id)
                            .bind(p.id)
                            .bind(spec.campaign_id)
                            .bind(spec.flow_id)
                            .bind(spec.flow_node_id)
                            .bind(p.phone_number.as_deref().unwrap_or_default())
                            .bind(why)
                            .bind(&hours.note)
                            .execute(&state.db)
                            .await?;
                            crate::features::messages_unsent(&state.db, spec.account.id, &[mid]).await;
                        }
                    }
                    return Ok(Outcome::Skipped(why));
                }
                if let Some(until) = hours.wait {
                    return Ok(Outcome::Deferred(until));
                }
                send_note = hours.note;
            }
            // One form of the number for the opt-out check, the lock and the provider alike.
            match spec.to_override.as_deref().or(p.phone_number.as_deref()) {
                Some(raw) => match normalize_phone(raw) {
                    Some(n) => Some(n),
                    None if spec.is_test => return Err(AppError::bad(format!("{raw:?} isn't a phone number (use +15035550100)"))),
                    None => return Ok(Outcome::Skipped("not a phone number")),
                },
                None => None,
            }
        }
        _ => return Err(AppError::bad("channel must be email, sms or push")),
    };
    let Some(to) = to else { return Ok(Outcome::Skipped("no address")) };

    // A paused account sends no email (see `deliverability`). Tests say why; real sends keep a
    // skipped message row with the reason instead of disappearing.
    if spec.channel == "email" && spec.account.sending_paused_at.is_some() {
        if spec.is_test {
            return Err(AppError::Conflict(crate::deliverability::paused_message(spec.account.sending_paused_reason.as_deref())));
        }
        match spec.message_id {
            Some(mid) => {
                sqlx::query("UPDATE messages SET status = 'skipped', error = 'sending paused' WHERE id = $1").bind(mid).execute(&state.db).await?;
                crate::features::messages_unsent(&state.db, spec.account.id, &[mid]).await;
            }
            None => {
                let mid = Uuid::now_v7();
                sqlx::query(
                    "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, status, error)
                     VALUES ($1, $2, $3, 'email', $4, $5, $6, $7, 'skipped', 'sending paused')",
                )
                .bind(mid)
                .bind(spec.account.id)
                .bind(p.id)
                .bind(spec.campaign_id)
                .bind(spec.flow_id)
                .bind(spec.flow_node_id)
                .bind(&to)
                .execute(&state.db)
                .await?;
                crate::features::messages_unsent(&state.db, spec.account.id, &[mid]).await;
            }
        }
        return Ok(Outcome::Skipped("sending paused"));
    }

    let message_id = spec.message_id.unwrap_or_else(Uuid::now_v7);
    let unsub = unsubscribe_url(state, spec.account, message_id);
    let mut ctx = render::context(spec.account, Some(p), spec.event, &unsub);
    // Saved blocks, display rules and the content module's blocks are worked out per recipient (see
    // features::content); email that uses none of them goes on untouched. When every block is hidden
    // for this person they get nothing (a test send says why). This runs before coupon codes are
    // claimed, so a code is only used for what this person will see.
    let Some(prepared) = crate::features::translations::with_hint(p.locale_hint.clone(), crate::features::content::prepare::prepare(state, spec.account, spec.content, &ctx)).await? else {
        if spec.is_test {
            return Err(AppError::bad("Nothing to show for this person: every block is hidden by a display rule, so they'd be skipped."));
        }
        return Ok(Outcome::Skipped(crate::features::content::prepare::NOTHING_TO_SHOW));
    };
    // The language version picked for this person (see features::translations), kept on the message
    // row for reports (written with the row, below).
    let language = prepared.get("_lang").and_then(|l| l.as_str()).filter(|l| !l.is_empty()).map(str::to_string);
    // Unique coupon codes are claimed here, after the checks above and just before rendering, for the
    // coupons the prepared content shows; a code stays the person's if a later check skips or defers
    // this attempt. An empty pool holds the message. Messages without coupons do no coupon work (see
    // features::coupons).
    let coupons = match crate::features::coupons::assign::prepare_send_for(state, &spec, &prepared, message_id).await? {
        crate::features::coupons::Prepared::Ready(c) => c,
        crate::features::coupons::Prepared::Hold(until) => return Ok(Outcome::Deferred(until)),
    };
    coupons.fill(&mut ctx);
    // Product blocks and catalog variables are filled per recipient (see features::catalog).
    let content = crate::features::catalog::personalize(&state.db, spec.account, Some(p), spec.event, &prepared, &mut ctx, false).await?;
    // Review request blocks and links are made for this person (see features::reviews).
    let filled = match crate::features::reviews::personalize(state, spec.account, Some(p), spec.event, &content, spec.channel, spec.is_test, Some(message_id)).await? {
        std::borrow::Cow::Owned(v) => Some(v),
        std::borrow::Cow::Borrowed(_) => None,
    };
    let content = match filled {
        Some(v) => std::borrow::Cow::Owned(v),
        None => content,
    };
    let link_base = crate::domains::link_base(state, spec.account);
    let tracking = Tracking {
        public_url: &link_base,
        secret: &state.cfg.secret_key,
        message_id,
        unsubscribe_url: unsub.clone(),
    };
    // Link tagging (UTM, email only): resolved tags ride on the content; none renders as before (features::campaigns_plus::utm).
    let link_tags = crate::features::campaigns_plus::utm::Tagger::from_content(state, spec.account, spec.content);
    let link_hook = link_tags.as_ref().map(|t| t as &dyn render::LinkHook);
    // Short links in texts (features::short_links): at most one statement, none for a text without
    // links or a campaign text (its links were set up when its audience was fixed).
    let short_links = crate::features::short_links::for_send(state, &spec, &content, link_hook, message_id, &link_base).await?;

    // Render first so template errors never leave a half-sent message.
    enum Rendered {
        Email(OutgoingEmail),
        /// Message, provider, and whether that provider is the customer's own account (not billed).
        Sms(OutgoingSms, std::sync::Arc<dyn crate::providers::sms::SmsProvider>, bool),
    }
    let (subject, mut rendered) = if spec.channel == "email" {
        let content: EmailContent = serde_json::from_value(content.into_owned())?;
        let from_email = content
            .from_email
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| spec.account.from_email.clone())
            .ok_or_else(|| AppError::bad("set a sender email in Settings → Sending before sending email"))?;
        check_sender_domain(state, spec.account, &from_email)?;
        let r = render::render_email_hooked(spec.account, &content, &ctx, Some(&tracking), render::EmailOptions::default(), link_hook)?;
        let subject = if spec.is_test { format!("[Test] {}", r.subject) } else { r.subject.clone() };
        let email = OutgoingEmail {
            message_id,
            to: to.clone(),
            from_name: content.from_name.clone().filter(|s| !s.is_empty()).or_else(|| spec.account.from_name.clone()).unwrap_or_else(|| spec.account.name.clone()),
            from_email,
            reply_to: content.reply_to.clone().filter(|s| !s.is_empty()).or_else(|| spec.account.reply_to.clone()),
            subject: subject.clone(),
            html: r.html,
            text: r.text,
            unsubscribe_url: Some(unsub.clone()),
        };
        (Some(subject), Rendered::Email(email))
    } else {
        let content: SmsContent = serde_json::from_value(content.into_owned())?;
        let (provider, mut from, own_provider) = crate::phone::sender(state, spec.account.id).await?;
        if let Some(c) = conversational {
            from = c.from.clone();
        }
        let prior_sms: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE profile_id = $1 AND channel = 'sms' AND status IN ('sent','delivered'))")
            .bind(p.id)
            .fetch_one(&state.db)
            .await?;
        let body = render::render_sms_with(spec.account, &content, &ctx, Some(&tracking), !prior_sms, short_links.as_ref().map(|s| s as &dyn render::SmsLinks))?;
        (None, Rendered::Sms(OutgoingSms { message_id, to: to.clone(), from, body, media_url: content.media_url.clone() }, provider, own_provider))
    };

    // Pay-as-you-go: make sure the balance covers this message before it goes out.
    let (segments, is_mms, own_provider) = match &rendered {
        Rendered::Sms(s, _, own) => (crate::providers::sms::segment_count(&s.body).1 as i64, s.media_url.is_some(), *own),
        Rendered::Email(_) => (1, false, false),
    };
    let price = crate::billing::quote(state, spec.account.id, spec.channel, segments, is_mms, own_provider).await?;
    if price > 0 && crate::billing::balance(&state.db, spec.account.id).await? < price {
        if let Some(mid) = spec.message_id {
            sqlx::query("UPDATE messages SET status = 'skipped', error = 'insufficient credit' WHERE id = $1").bind(mid).execute(&state.db).await?;
            crate::features::messages_unsent(&state.db, spec.account.id, &[mid]).await;
        }
        if spec.is_test {
            return Err(AppError::LimitReached("your balance is empty; add credit in Settings → Billing".into()));
        }
        return Ok(Outcome::Skipped("insufficient credit"));
    }

    // Warm-up: real email counts against the account's 24-hour limit; past it, the email waits.
    let limited = spec.channel == "email" && !spec.is_test;
    if limited {
        if let Err(until) = crate::deliverability::take_daily_slot(state, spec.account.id).await.map_err(AppError::Internal)? {
            return Ok(Outcome::Deferred(until));
        }
    }

    // A text with short links takes the first of its references not already used in the account
    // (features::short_links); the text is written with the one it got. Another text taking the same
    // one at the same moment makes the write fail on the unique index: the next try takes the next.
    let mut clashes = 0;
    // None: a campaign message's row is gone (its person was deleted while the batch ran).
    let written_row: Option<Option<String>> = loop {
        let written: sqlx::Result<Option<Option<String>>> = if spec.message_id.is_none() {
            sqlx::query_scalar(
                "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, subject, is_test, send_note, language, short_ref)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, (SELECT c.r FROM unnest($13::text[]) WITH ORDINALITY AS c(r, i)
                     WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.account_id = $2 AND m.short_ref = c.r) ORDER BY c.i LIMIT 1))
                 RETURNING short_ref",
            )
            .bind(message_id)
            .bind(spec.account.id)
            .bind(p.id)
            .bind(spec.channel)
            .bind(spec.campaign_id)
            .bind(spec.flow_id)
            .bind(spec.flow_node_id)
            .bind(&to)
            .bind(&subject)
            .bind(spec.is_test)
            .bind(&send_note)
            .bind(&language)
            .bind(short_links.as_ref().map(|s| s.ref_candidates()))
            .fetch_one(&state.db)
            .await
            .map(Some)
        } else {
            sqlx::query_scalar(
                "UPDATE messages u SET subject = $2, to_address = $3, send_note = COALESCE($4, send_note), language = COALESCE($5, language),
                    short_ref = (SELECT c.r FROM unnest($6::text[]) WITH ORDINALITY AS c(r, i)
                        WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.account_id = u.account_id AND m.short_ref = c.r AND m.id <> u.id) ORDER BY c.i LIMIT 1)
                 WHERE u.id = $1 AND (u.to_address IS NULL OR u.to_address = $3 OR $7) RETURNING u.short_ref",
            )
            .bind(message_id)
            .bind(&subject)
            .bind(&to)
            .bind(&send_note)
            .bind(&language)
            .bind(short_links.as_ref().map(|s| s.ref_candidates()))
            .bind(spec.is_test)
            .fetch_optional(&state.db)
            .await
        };
        match written {
            Err(e) if clashes < 3 && crate::features::short_links::is_ref_clash(&e) => clashes += 1,
            written => break written?,
        }
    };
    let Some(short_ref) = written_row else {
        if limited {
            crate::deliverability::return_daily_slot(state, spec.account.id);
        }
        // A queued message names the address it was meant for. If the profile's address changed since
        // (an edit, a merge), it isn't sent to the new one (the write above refused it).
        if let Some(mid) = spec.message_id {
            let n = sqlx::query("UPDATE messages SET status = 'skipped', error = 'address changed' WHERE id = $1 AND status IN ('queued', 'sending')").bind(mid).execute(&state.db).await?.rows_affected();
            if n > 0 {
                crate::features::messages_unsent(&state.db, spec.account.id, &[mid]).await;
                return Ok(Outcome::Skipped("address changed"));
            }
        }
        return Ok(Outcome::Skipped("profile deleted"));
    };
    if let (Some(links), Rendered::Sms(s, _, _)) = (&short_links, &mut rendered) {
        let chosen = short_ref.ok_or_else(|| AppError::Internal(anyhow::anyhow!("no free short link reference for this text")))?;
        s.body = links.with_ref(&s.body, &chosen);
    }

    match &rendered {
        Rendered::Email(_) => throttle_email(state).await,
        Rendered::Sms(..) => {
            throttle("sms", f64::from(state.cfg.send_rate_per_sec.max(1))).await;
            limiter_pause(spec.account.id).await;
        }
    }
    // An opt-out, a cancel, a pause or (texts) the closing of the window can land while waiting for a
    // slot: check again right before the hand-off. A text is checked and claimed in one short
    // transaction under the same lock an inbound STOP takes (tests too: a test can't reach a number
    // that opted out), so an opt-out that committed first always wins and nothing can mark it
    // skipped after it's handed over.
    let stop = match &rendered {
        Rendered::Sms(s, _, _) => claim_text(state, &spec, message_id, &s.to, &s.from, conversational, strict).await?,
        Rendered::Email(_) if !spec.is_test => last_check(state, &spec, p.id, message_id, &to, strict).await?,
        Rendered::Email(_) => None,
    };
    if let Some(stop) = stop {
        if limited {
            crate::deliverability::return_daily_slot(state, spec.account.id);
        }
        return match stop {
            Stop::Defer(until) => {
                if spec.message_id.is_none() {
                    // A flow re-runs this step then and creates a new message.
                    sqlx::query("DELETE FROM messages WHERE id = $1").bind(message_id).execute(&state.db).await?;
                }
                Ok(Outcome::Deferred(until))
            }
            Stop::Skip(why, note) => {
                // A text's claim already recorded why (without overwriting a STOP's mark).
                if spec.channel == "email" {
                    sqlx::query("UPDATE messages SET status = 'skipped', error = $2, send_note = COALESCE($3, send_note) WHERE id = $1")
                        .bind(message_id)
                        .bind(why)
                        .bind(note)
                        .execute(&state.db)
                        .await?;
                    crate::features::messages_unsent(&state.db, spec.account.id, &[message_id]).await;
                }
                Ok(Outcome::Skipped(why))
            }
        };
    }
    // Keep the text as sent, so the SMS inbox can show campaign, flow and reply texts in each thread.
    if let (Rendered::Sms(s, _, _), false) = (&rendered, spec.is_test) {
        crate::features::inbox::record_sent(&state.db, spec.account.id, p.id, message_id, &s.body, s.media_url.as_deref(), conversational).await?;
    }
    let result = match &rendered {
        Rendered::Email(e) => state.email.send(e).await,
        // Bounded, so a claimed text never waits on the provider indefinitely.
        Rendered::Sms(s, provider, _) => match tokio::time::timeout(SMS_PROVIDER_TIMEOUT, provider.send(s)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("the texting provider didn't answer within {} seconds; not sent again", SMS_PROVIDER_TIMEOUT.as_secs())),
        },
    };
    match result {
        Ok(provider_id) => {
            coupons.delivered(&state.db).await;
            let cost = crate::billing::charge_message(state, spec.account.id, spec.channel, segments, is_mms, own_provider).await?;
            let from_domain = match &rendered {
                Rendered::Email(e) => crate::features::reports::delivery::sender_domain(&e.from_email),
                Rendered::Sms(..) => None,
            };
            // A text's result is written only over its own claim ('sending'), never over another mark.
            sqlx::query(
                "UPDATE messages SET status = 'sent', provider_message_id = $2, sent_at = now(), error = NULL, segments = $3, cost_micros = $4, from_domain = $5
                 WHERE id = $1 AND (channel <> 'sms' OR status = 'sending')",
            )
            .bind(message_id)
            .bind(&provider_id)
            .bind(segments as i32)
            .bind(cost)
            .bind(from_domain)
            .execute(&state.db)
            .await?;
            if limited {
                crate::deliverability::record_send(&state.db, message_id, &provider_id, spec.account.id, spec.campaign_id).await?;
                crate::deliverability::complete_daily_slot(state, spec.account.id);
            }
            if !spec.is_test {
                let metric = if spec.channel == "email" { "Received Email" } else { "Received SMS" };
                record_event(
                    &state.db,
                    NewEvent {
                        account_id: spec.account.id,
                        profile_id: p.id,
                        metric,
                        properties: match &send_note {
                            Some(note) => json!({"subject": subject, "campaign_id": spec.campaign_id, "flow_id": spec.flow_id, "message_id": message_id, "time_zone_note": note}),
                            None => json!({"subject": subject, "campaign_id": spec.campaign_id, "flow_id": spec.flow_id, "message_id": message_id}),
                        },
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
        Err(e) => {
            if limited {
                crate::deliverability::return_daily_slot(state, spec.account.id);
            }
            // The provider can't take mail right now (quota used up, throttled, paused on its side): wait.
            if let Some(busy) = e.downcast_ref::<crate::providers::email::ProviderBusy>() {
                tracing::warn!(message = %message_id, reason = %busy.reason, "email provider busy; holding the email");
                if spec.is_test {
                    sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1").bind(message_id).bind(busy.to_string()).execute(&state.db).await?;
                    return Err(AppError::Conflict("Amazon isn't accepting more email from DumbMail right now. Try the test again later.".into()));
                }
                if spec.message_id.is_none() {
                    // A flow re-runs this step later and creates a new message then.
                    sqlx::query("DELETE FROM messages WHERE id = $1").bind(message_id).execute(&state.db).await?;
                }
                let wait = chrono::Duration::from_std(busy.retry_after).unwrap_or(chrono::Duration::hours(1));
                return Ok(Outcome::Deferred(Utc::now() + wait));
            }
            let msg = format!("{e:#}");
            tracing::warn!(message = %message_id, error = %msg, "send failed");
            sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1 AND (channel <> 'sms' OR status = 'sending')")
                .bind(message_id)
                .bind(&msg)
                .execute(&state.db)
                .await?;
            if !spec.is_test {
                crate::features::messages_unsent(&state.db, spec.account.id, &[message_id]).await;
            }
            Ok(Outcome::Failed(msg))
        }
    }
}

/// Why a message stops at the last check: skipped (with a note for the message), or deferred.
enum Stop {
    Skip(&'static str, Option<String>),
    Defer(DateTime<Utc>),
}

/// The last check before the provider call, in one query against the current state: the person's
/// consent and suppression for the channel, the account's email pause, the campaign's status, and for
/// texts the whole quiet-hours decision. None: send it.
async fn last_check(state: &AppState, spec: &SendSpec<'_>, profile_id: Uuid, message_id: Uuid, to: &str, strict: bool) -> AppResult<Option<Stop>> {
    type Row = (Option<String>, String, String, bool, Option<String>, Option<Value>, bool, Option<String>, Option<i32>, Option<i32>, Option<String>, bool, bool, Option<String>, bool);
    let (account, channel, campaign_id) = (spec.account, spec.channel, spec.campaign_id);
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT p.email, p.email_consent, p.sms_consent, p.email_suppressed_reason IS NOT NULL, p.phone_number,
                CASE WHEN $3 = 'sms' THEN p.properties END, a.sending_paused_at IS NOT NULL,
                (SELECT c.status FROM campaigns c WHERE c.id = $2),
                ss.sms_start_minute, ss.sms_end_minute, ss.sms_zone_conflict, CASE WHEN $5 THEN p.marketing_hold ELSE {} END, {}, {}
         FROM profiles p JOIN accounts a ON a.id = p.account_id LEFT JOIN send_settings ss ON ss.account_id = a.id
         WHERE p.id = $1",
        crate::marketing_hold::blocks_email_sql("$4"),
        crate::features::campaigns_plus::resend::HANDOFF_COLUMNS,
        crate::features::reviews::requests::answered_sql("$5", "$6")
    ))
    .bind(profile_id)
    .bind(campaign_id)
    .bind(channel)
    .bind(spec.flow_id)
    .bind(strict)
    .bind(message_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((email, email_consent, sms_consent, suppressed, phone, props, paused, campaign, start, end, conflict, held, resend, original_to, answered)) = row else {
        return Ok(Some(Stop::Skip("profile deleted", None)));
    };
    if answered {
        return Ok(Some(Stop::Skip(crate::features::reviews::requests::ANSWERED, None)));
    }
    match campaign.as_deref() {
        _ if campaign_id.is_none() => {}
        Some("sending") => {}
        Some("cancelled") | None => return Ok(Some(Stop::Skip("campaign cancelled", None))),
        Some(_) => return Ok(Some(Stop::Skip("campaign stopped", None))),
    }
    if channel == "email" {
        if email.is_none() || email_consent == "unsubscribed" || email_consent == "pending" {
            return Ok(Some(Stop::Skip("not emailable", None)));
        }
        if strict && email_consent != "subscribed" {
            return Ok(Some(Stop::Skip(crate::marketing_hold::SKIP_REASON, None)));
        }
        if suppressed {
            return Ok(Some(Stop::Skip("suppressed", None)));
        }
        // The address it's going to must still be the profile's (an edit or a merge may have moved it).
        if spec.to_override.is_none() && email.as_deref() != Some(to) {
            return Ok(Some(Stop::Skip("address changed", None)));
        }
        if held {
            return Ok(Some(Stop::Skip(crate::marketing_hold::SKIP_REASON, None)));
        }
        if paused {
            return Ok(Some(Stop::Skip("sending paused", None)));
        }
        // A resend: only to the address that got the original, and only while subscribed.
        if let Some(why) = crate::features::campaigns_plus::resend::handoff_stop(resend, &email_consent, email.as_deref(), original_to.as_deref(), to) {
            return Ok(Some(Stop::Skip(why, None)));
        }
        return Ok(None);
    }
    Ok(marketing_text_stop(account, &sms_consent, phone.as_deref(), props.as_ref(), start, end, conflict.as_deref()))
}

/// A marketing text's consent and the whole quiet-hours decision, from the current row.
fn marketing_text_stop(account: &Account, sms_consent: &str, phone: Option<&str>, props: Option<&Value>, start: Option<i32>, end: Option<i32>, conflict: Option<&str>) -> Option<Stop> {
    if sms_consent != "subscribed" || phone.is_none() {
        return Some(Stop::Skip("no sms consent", None));
    }
    let ((s, e), skip_conflicts) = crate::features::sendtime::settings::window_from(start.zip(end), conflict);
    let zones = crate::features::sendtime::settings::sms_zones(props.unwrap_or(&Value::Null), phone, account_tz(account));
    let hours = crate::features::sendtime::settings::decide(&zones, Utc::now(), s, e, skip_conflicts);
    if hours.skip {
        return Some(Stop::Skip("time zones disagree", hours.note));
    }
    hours.wait.map(Stop::Defer)
}

/// The last check before a text goes to the provider, and its claim, in one short transaction under
/// the lock an inbound opt-out takes for this number (see `features::inbox`): the number hasn't
/// opted out, the campaign is still sending, and then either the marketing rules (consent, quiet
/// hours; not for tests) or the inbox's reply rules. If it may go, the message is claimed
/// (`queued`/`sending` → `sending`); if not, it's marked skipped unless something else already
/// settled it. None: claimed, send it.
async fn claim_text(state: &AppState, spec: &SendSpec<'_>, message_id: Uuid, to: &str, from: &str, conversational: Option<&Conversational>, strict: bool) -> AppResult<Option<Stop>> {
    type Row = (bool, Option<String>, Option<String>, Option<Value>, Option<String>, Option<i32>, Option<i32>, Option<String>, bool, Option<DateTime<Utc>>, bool, bool, bool, bool);
    let account = spec.account;
    let mut tx = state.db.begin().await?;
    crate::features::inbox::store::lock_number(&mut tx, account.id, to).await?;
    let ours = conversational.map(|_| from);
    // The account row is held (shared) until the claim commits, so an account closing right now
    // either finishes first (and this sees it) or waits for this claim.
    let (opted_out, consent, phone, props, campaign, start, end, conflict, from_ours, window_start, closed, held, answered, suppressed): Row = sqlx::query_as(&format!(
        "SELECT EXISTS (SELECT 1 FROM sms_opt_outs o WHERE o.account_id = a.id AND o.phone_number = $3),
                p.sms_consent, p.phone_number, p.properties, (SELECT c.status FROM campaigns c WHERE c.id = $4),
                ss.sms_start_minute, ss.sms_end_minute, ss.sms_zone_conflict,
                $5::text IS NOT NULL AND (a.sms_from = $5 OR EXISTS (SELECT 1 FROM phone_numbers n WHERE n.account_id = a.id AND n.phone_number = $5 AND n.status IN ('verified', 'active'))),
                (SELECT max(i.provider_received_at) FROM sms_inbound i
                  WHERE $5::text IS NOT NULL AND i.account_id = a.id AND i.from_number = $3 AND i.to_number = $5 AND i.verified AND i.kind NOT IN {}),
                a.closed_at IS NOT NULL, $6 AND COALESCE(p.marketing_hold, false), {}, p.sms_suppressed_reason IS NOT NULL
         FROM accounts a LEFT JOIN profiles p ON p.id = $2 AND p.account_id = a.id LEFT JOIN send_settings ss ON ss.account_id = a.id
         WHERE a.id = $1 FOR SHARE OF a",
        crate::features::inbox::store::NO_WINDOW_KINDS,
        crate::features::reviews::requests::answered_sql("$6", "$7")
    ))
    .bind(account.id)
    .bind(spec.profile.id)
    .bind(to)
    .bind(spec.campaign_id)
    .bind(ours)
    .bind(strict)
    .bind(message_id)
    .fetch_one(&mut *tx)
    .await?;
    let help = conversational.is_some_and(|c| c.kind == "help");
    let stop = if closed {
        Some(Stop::Skip("account closed", None))
    } else if opted_out && !help {
        Some(Stop::Skip("opted out", None))
    } else if consent.is_none() {
        Some(Stop::Skip("profile deleted", None))
    } else if suppressed && !help {
        // A number on the suppression list (features::people) gets no texts at all, tests included.
        Some(Stop::Skip("suppressed", None))
    } else if conversational.is_none() && spec.to_override.is_none() && phone.as_deref().and_then(normalize_phone).as_deref() != Some(to) {
        // The number it's going to must still be the profile's (an edit or a merge may have moved it).
        Some(Stop::Skip("address changed", None))
    } else if answered {
        Some(Stop::Skip(crate::features::reviews::requests::ANSWERED, None))
    } else if held {
        Some(Stop::Skip(crate::marketing_hold::SKIP_REASON, None))
    } else if !spec.is_test && spec.campaign_id.is_some() && campaign.as_deref() != Some("sending") {
        Some(Stop::Skip(if matches!(campaign.as_deref(), Some("cancelled") | None) { "campaign cancelled" } else { "campaign stopped" }, None))
    } else if let Some(c) = conversational {
        crate::features::inbox::store::reply_allowed(c, consent.as_deref().unwrap_or_default(), phone.as_deref(), from_ours, window_start, Utc::now()).err().map(|why| Stop::Skip(why, None))
    } else if spec.is_test {
        None
    } else {
        marketing_text_stop(account, consent.as_deref().unwrap_or_default(), phone.as_deref(), props.as_ref(), start, end, conflict.as_deref())
    };
    match &stop {
        Some(Stop::Skip(why, note)) => {
            let n = sqlx::query("UPDATE messages SET status = 'skipped', error = $2, send_note = COALESCE($3, send_note) WHERE id = $1 AND status IN ('queued', 'sending')")
                .bind(message_id)
                .bind(why)
                .bind(note)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if n > 0 && !spec.is_test {
                crate::features::messages_unsent_in(&mut tx, &state.db, account.id, &[message_id]).await;
            }
        }
        Some(Stop::Defer(_)) => {}
        None => {
            let claimed: Option<Uuid> =
                sqlx::query_scalar("UPDATE messages SET status = 'sending', claimed_at = COALESCE(claimed_at, now()) WHERE id = $1 AND status IN ('queued', 'sending') RETURNING id")
                    .bind(message_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if claimed.is_none() {
                tx.commit().await?;
                return Ok(Some(Stop::Skip("no longer queued", None)));
            }
        }
    }
    tx.commit().await?;
    Ok(stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_hours_respect_timezone() {
        let mut acct = test_account();
        acct.timezone = "America/Los_Angeles".into();
        // 2026-11-14 03:00 UTC = 19:00 PST (allowed)
        let ok = Utc.with_ymd_and_hms(2026, 11, 14, 3, 0, 0).unwrap();
        assert!(sms_quiet_until(&acct, ok).is_none());
        // 2026-11-14 06:00 UTC = 22:00 PST → next day 09:00 PST = 17:00 UTC
        let late = Utc.with_ymd_and_hms(2026, 11, 14, 6, 0, 0).unwrap();
        assert_eq!(sms_quiet_until(&acct, late), Some(Utc.with_ymd_and_hms(2026, 11, 14, 17, 0, 0).unwrap()));
    }

    fn test_account() -> Account {
        Account {
            id: Uuid::nil(),
            public_key: String::new(),
            name: "A".into(),
            website: None,
            from_name: None,
            from_email: None,
            reply_to: None,
            sms_from: None,
            timezone: "America/Los_Angeles".into(),
            address: None,
            brand_color: "#000".into(),
            logo_url: None,
            social_links: serde_json::json!({}),
            plan: "free".into(),
            onboarding: json!({}),
            conversion_metric: "Placed Order".into(),
            created_at: chrono::Utc::now(),
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
}
