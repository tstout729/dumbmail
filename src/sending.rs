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
use crate::models::{Account, Profile};
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

async fn throttle(channel: &'static str, per_sec: u32) {
    let gap = Duration::from_secs_f64(1.0 / per_sec.max(1) as f64);
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
    let ok = account.sending_domain_status == "verified"
        && account.sending_domain.as_deref().is_some_and(|d| from_domain == d || from_domain.ends_with(&format!(".{d}")));
    if ok { Ok(()) } else { Err(format!("Verify {from_domain} in Settings → Sending before sending from {from_email}")) }
}

pub fn unsubscribe_url(state: &AppState, account: &Account, message_id: Uuid) -> String {
    format!("{}/u/{}", crate::domains::link_base(state, account), crate::auth::sign(&state.cfg.secret_key, &format!("u{}", message_id.simple())))
}

pub async fn send(state: &AppState, spec: SendSpec<'_>) -> AppResult<Outcome> {
    let p = spec.profile;
    let to = match spec.channel {
        "email" => {
            if !spec.is_test && (p.email_consent == "unsubscribed" || p.email.is_none()) {
                return Ok(Outcome::Skipped("not emailable"));
            }
            if !spec.is_test {
                let suppressed: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE id = $1")
                    .bind(p.id)
                    .fetch_one(&state.db)
                    .await?;
                if suppressed.is_some() {
                    return Ok(Outcome::Skipped("suppressed"));
                }
            }
            spec.to_override.clone().or_else(|| p.email.clone())
        }
        "sms" => {
            if !spec.is_test && (p.sms_consent != "subscribed" || p.phone_number.is_none()) {
                return Ok(Outcome::Skipped("no sms consent"));
            }
            if !spec.is_test {
                if let Some(until) = sms_quiet_until(spec.account, Utc::now()) {
                    return Ok(Outcome::Deferred(until));
                }
            }
            spec.to_override.clone().or_else(|| p.phone_number.clone())
        }
        _ => return Err(AppError::bad("channel must be email or sms")),
    };
    let Some(to) = to else { return Ok(Outcome::Skipped("no address")) };

    let message_id = spec.message_id.unwrap_or_else(Uuid::now_v7);
    let unsub = unsubscribe_url(state, spec.account, message_id);
    let ctx = render::context(spec.account, Some(p), spec.event, &unsub);
    let link_base = crate::domains::link_base(state, spec.account);
    let tracking = Tracking {
        public_url: &link_base,
        secret: &state.cfg.secret_key,
        message_id,
        unsubscribe_url: unsub.clone(),
    };

    // Render first so template errors never leave a half-sent message.
    enum Rendered {
        Email(OutgoingEmail),
        /// Message, provider, and whether that provider is the customer's own account (not billed).
        Sms(OutgoingSms, std::sync::Arc<dyn crate::providers::sms::SmsProvider>, bool),
    }
    let (subject, rendered) = if spec.channel == "email" {
        let content: EmailContent = serde_json::from_value(spec.content.clone())?;
        let from_email = content
            .from_email
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| spec.account.from_email.clone())
            .ok_or_else(|| AppError::bad("set a sender email in Settings → Sending before sending email"))?;
        check_sender_domain(state, spec.account, &from_email)?;
        let r = render::render_email(spec.account, &content, &ctx, Some(&tracking))?;
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
        let content: SmsContent = serde_json::from_value(spec.content.clone())?;
        let (provider, from, own_provider) = crate::phone::sender(state, spec.account.id).await?;
        let prior_sms: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE profile_id = $1 AND channel = 'sms' AND status IN ('sent','delivered'))")
            .bind(p.id)
            .fetch_one(&state.db)
            .await?;
        let body = render::render_sms(spec.account, &content, &ctx, Some(&tracking), !prior_sms)?;
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
        }
        if spec.is_test {
            return Err(AppError::LimitReached("your balance is empty; add credit in Settings → Billing".into()));
        }
        return Ok(Outcome::Skipped("insufficient credit"));
    }

    if spec.message_id.is_none() {
        sqlx::query(
            "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, subject, is_test)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
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
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query("UPDATE messages SET subject = $2, to_address = $3 WHERE id = $1")
            .bind(message_id)
            .bind(&subject)
            .bind(&to)
            .execute(&state.db)
            .await?;
    }

    let result = match &rendered {
        Rendered::Email(e) => {
            throttle("email", state.cfg.send_rate_per_sec).await;
            state.email.send(e).await
        }
        Rendered::Sms(s, provider, _) => {
            throttle("sms", state.cfg.send_rate_per_sec).await;
            provider.send(s).await
        }
    };
    match result {
        Ok(provider_id) => {
            let cost = crate::billing::charge_message(state, spec.account.id, spec.channel, segments, is_mms, own_provider).await?;
            sqlx::query("UPDATE messages SET status = 'sent', provider_message_id = $2, sent_at = now(), error = NULL, segments = $3, cost_micros = $4 WHERE id = $1")
                .bind(message_id)
                .bind(&provider_id)
                .bind(segments as i32)
                .bind(cost)
                .execute(&state.db)
                .await?;
            if !spec.is_test {
                let metric = if spec.channel == "email" { "Received Email" } else { "Received SMS" };
                record_event(
                    &state.db,
                    NewEvent {
                        account_id: spec.account.id,
                        profile_id: p.id,
                        metric,
                        properties: json!({"subject": subject, "campaign_id": spec.campaign_id, "flow_id": spec.flow_id, "message_id": message_id}),
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
            let msg = format!("{e:#}");
            tracing::warn!(message = %message_id, error = %msg, "send failed");
            sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1")
                .bind(message_id)
                .bind(&msg)
                .execute(&state.db)
                .await?;
            Ok(Outcome::Failed(msg))
        }
    }
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
            plan: "free".into(),
            onboarding: json!({}),
            sending_domain: None,
            sending_domain_status: String::new(),
            sending_domain_records: json!([]),
            conversion_metric: "Placed Order".into(),
            created_at: chrono::Utc::now(),
            mail_from_domain: None,
            link_domain: None,
            link_domain_status: "not_started".into(),
            link_domain_records: json!([]),
        }
    }
}
