//! Sending a text as RCS, and the one SMS fallback.
//!
//! `sending::send` calls [`plan`] for a marketing or flow text (not a conversational reply, not an
//! MMS) after rendering it. With RCS on, the phone able to show this text's rich version, and the
//! text short enough for RCS, it gets a [`Plan`]; otherwise the text goes as SMS exactly as before.
//! The text then goes through the normal SMS steps (the message row, the rate limiter, the last
//! check and claim under the number's opt-out lock), and [`hand_off`] sends it to Telnyx as RCS
//! instead of SMS:
//!
//! - The `rcs_messages` row is written before the request (only while RCS is still on for that
//!   agent), with the exact text, numbers and payload: a retry never sends it again.
//! - Telnyx takes it: the message is sent, billed once at the RCS price in the same transaction.
//! - Telnyx refuses it with a definite error (nothing went out): it goes as SMS right away.
//! - No clear answer (time-out, gateway error, no error code): it's marked failed and never sent
//!   again, by either route.
//!
//! When Telnyx later reports `sending_failed` or `delivery_failed` for a message it took (and
//! before any delivered or read), [`super::webhook`] moves it to `fallback_queued`, refunds the RCS
//! charge and queues [`fallback`] in one transaction. The fallback claims the message under the
//! number's lock after the same checks as any text (opt-out, consent, the same number, campaign not
//! cancelled, marketing hold, quiet hours: it waits for the window), sends the claimed SMS text
//! once, and bills it as SMS. `delivery_unconfirmed` is not a failure: nothing is resent.

use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use super::content::{self, Prepared};
use super::settings::{self, Active};
use crate::audience::{record_event, NewEvent};
use crate::error::AppResult;
use crate::providers::sms::OutgoingSms;
use crate::render::Tracking;
use crate::sending::SendSpec;
use crate::state::AppState;

/// How long a hand-off may wait on Telnyx before it counts as unclear (never sent again).
pub const PROVIDER_TIMEOUT: Duration = crate::sending::SMS_PROVIDER_TIMEOUT;

/// A text that goes as RCS.
pub struct Plan {
    active: Active,
    prepared: Prepared,
    /// DumbMail's price for it as RCS.
    pub price: i64,
}

/// Whether this text goes as RCS. None: send it as SMS, as before. One cheap lookup when RCS is off.
pub async fn plan(state: &AppState, spec: &SendSpec<'_>, content: &Value, ctx: &Value, tracking: &Tracking<'_>, to: &str, has_media: bool) -> AppResult<Option<Plan>> {
    if spec.channel != "sms" || has_media {
        return Ok(None);
    }
    let Some(active) = settings::active(state, spec.account.id).await? else { return Ok(None) };
    let rich = match content::parse(content) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(account = %spec.account.id, error = %e.public_message(), "invalid rich version; sending the text as SMS");
            return Ok(None);
        }
    };
    let features = super::capability::features(state, spec.account.id, &active, to).await?;
    if features.is_empty() || !content::needs(rich.as_ref()).iter().all(|f| features.iter().any(|g| g == f)) {
        return Ok(None);
    }
    let prepared = content::prepare(rich.as_ref(), ctx, Some(tracking))?;
    let price = settings::price(state, active.creds.own);
    Ok(Some(Plan { active, prepared, price }))
}

/// What happened when the text was handed to Telnyx as RCS.
pub enum Handoff {
    /// Telnyx took it as RCS: sent, billed and recorded.
    Sent,
    /// Nothing went out as RCS: send it as SMS now (the claim still holds).
    AsSms,
    /// Telnyx may have it and didn't say: failed, never sent again (the message is marked).
    Unclear(String),
}

/// Hand a claimed text to Telnyx as RCS. `sms` is the text exactly as claimed.
pub async fn hand_off(state: &AppState, spec: &SendSpec<'_>, plan: &Plan, sms: &OutgoingSms, strict: bool) -> AppResult<Handoff> {
    let account_id = spec.account.id;
    let Some(message) = content::agent_message(&sms.body, &plan.prepared) else { return Ok(Handoff::AsSms) };
    let a = &plan.active;
    let body = json!({
        "agent_id": a.agent_id, "to": sms.to, "messaging_profile_id": a.messaging_profile_id, "type": "RCS",
        "webhook_url": settings::webhook_url(state, account_id).await?, "agent_message": message,
    });
    // The record comes first, and only while RCS is still on for this agent. An existing record
    // means an earlier attempt already went to Telnyx: never again.
    let written: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO rcs_messages (message_id, account_id, agent_id, to_number, from_number, sms_body, payload, strict, is_test, creds_key)
         SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10 FROM rcs_settings s WHERE s.account_id = $2 AND s.enabled AND s.checked_agent_id = $3 AND s.checked_key = $10
         ON CONFLICT (message_id) DO NOTHING RETURNING message_id",
    )
    .bind(sms.message_id)
    .bind(account_id)
    .bind(&a.agent_id)
    .bind(&sms.to)
    .bind(&sms.from)
    .bind(&sms.body)
    .bind(&body)
    .bind(strict)
    .bind(spec.is_test)
    .bind(a.creds.fingerprint())
    .fetch_optional(&state.db)
    .await?;
    if written.is_none() {
        let earlier: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM rcs_messages WHERE message_id = $1)").bind(sms.message_id).fetch_one(&state.db).await?;
        if earlier {
            return Ok(Handoff::Unclear(mark_unclear(state, account_id, sms.message_id, spec.is_test, "already handed to Telnyx once; not sent again").await?));
        }
        return Ok(Handoff::AsSms);
    }
    let key = sms.message_id.simple().to_string();
    match super::telnyx::send(&state.cfg, &a.creds.api_key, &body, &key, PROVIDER_TIMEOUT).await {
        Ok(acc) if acc.kind == "SMS" => {
            // Telnyx sent it as a text itself (it shouldn't: no fallback is asked for). It's an SMS.
            Ok(Handoff::Unclear(mark_unclear(state, account_id, sms.message_id, spec.is_test, "Telnyx sent it as SMS instead of RCS; not sent again").await?))
        }
        Ok(acc) => {
            let mut tx = state.db.begin().await?;
            let moved: Option<Uuid> = sqlx::query_scalar(
                "UPDATE rcs_messages SET state = 'sent', rcs_provider_id = $2, sent_at = now(), rcs_charge_micros = $3 WHERE message_id = $1 AND state = 'sending' RETURNING message_id",
            )
            .bind(sms.message_id)
            .bind(&acc.id)
            .bind(plan.price)
            .fetch_optional(&mut *tx)
            .await?;
            if moved.is_some() {
                sqlx::query(
                    "UPDATE messages SET status = 'sent', provider_message_id = $2, sent_at = now(), error = NULL, segments = 1, cost_micros = $3 WHERE id = $1 AND status = 'sending'",
                )
                .bind(sms.message_id)
                .bind(format!("rcs:{}", acc.id))
                .bind(plan.price)
                .execute(&mut *tx)
                .await?;
                if plan.price > 0 {
                    crate::billing::debit_usage_tx(&mut tx, account_id, "rcs", 1, plan.price).await?;
                }
            }
            tx.commit().await?;
            if !spec.is_test {
                record_event(
                    &state.db,
                    NewEvent {
                        account_id,
                        profile_id: spec.profile.id,
                        metric: "Received SMS",
                        properties: json!({"campaign_id": spec.campaign_id, "flow_id": spec.flow_id, "message_id": sms.message_id, "rcs": true}),
                        value: None,
                        occurred_at: None,
                        unique_id: Some(sms.message_id.to_string()),
                        message_id: Some(sms.message_id),
                    },
                )
                .await?;
            }
            Ok(Handoff::Sent)
        }
        Err(e) if e.unclear => Ok(Handoff::Unclear(mark_unclear(state, account_id, sms.message_id, spec.is_test, &format!("{}; not sent again", e.describe())).await?)),
        Err(e) => {
            // Telnyx definitely didn't take it: the text goes as SMS now.
            sqlx::query("UPDATE rcs_messages SET state = 'refused', route = 'sms', fallback_reason = $2 WHERE message_id = $1 AND state = 'sending'")
                .bind(sms.message_id)
                .bind(e.describe())
                .execute(&state.db)
                .await?;
            tracing::info!(message = %sms.message_id, error = %e.describe(), "Telnyx refused the RCS message; sending it as SMS");
            Ok(Handoff::AsSms)
        }
    }
}

async fn mark_unclear(state: &AppState, account_id: Uuid, message_id: Uuid, is_test: bool, why: &str) -> AppResult<String> {
    let msg = format!("RCS: {why}");
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE rcs_messages SET state = 'unknown', fallback_reason = $2 WHERE message_id = $1 AND state = 'sending'").bind(message_id).bind(why).execute(&mut *tx).await?;
    let n = sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1 AND status = 'sending'").bind(message_id).bind(&msg).execute(&mut *tx).await?.rows_affected();
    if n > 0 && !is_test {
        crate::features::messages_unsent_in(&mut tx, &state.db, account_id, &[message_id]).await;
    }
    tx.commit().await?;
    Ok(msg)
}

#[derive(sqlx::FromRow)]
struct FallbackRow {
    account_id: Uuid,
    profile_id: Uuid,
    campaign_id: Option<Uuid>,
    to_number: String,
    from_number: String,
    sms_body: String,
    strict: bool,
    is_test: bool,
}

/// Why a fallback text doesn't go (yet).
enum Stop {
    Skip(&'static str),
    Wait(chrono::DateTime<Utc>),
}

/// Job: the one SMS fallback for an RCS message Telnyx reported undelivered.
pub async fn fallback(state: &AppState, message_id: Uuid) -> anyhow::Result<()> {
    let row: Option<FallbackRow> = sqlx::query_as(
        "SELECT r.account_id, m.profile_id, m.campaign_id, r.to_number, r.from_number, r.sms_body, r.strict, r.is_test
         FROM rcs_messages r JOIN messages m ON m.id = r.message_id WHERE r.message_id = $1 AND r.state = 'fallback_queued'",
    )
    .bind(message_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(r) = row else { return Ok(()) };
    let fail = |e: crate::error::AppError| anyhow::anyhow!(e.public_message());
    // The provider and price are read before the claim (no network wait under the lock).
    let sender = crate::phone::sender(state, r.account_id).await;
    let segments = crate::providers::sms::segment_count(&r.sms_body).1 as i64;
    let (provider, own) = match sender {
        Ok((p, _, own)) => (Some(p), own),
        Err(_) => (None, false),
    };
    let cost = crate::billing::quote(state, r.account_id, "sms", segments, false, own).await.map_err(fail)?;
    let funded = cost <= 0 || crate::billing::balance(&state.db, r.account_id).await.map_err(fail)? >= cost;

    let mut tx = state.db.begin().await?;
    crate::features::inbox::store::lock_number(&mut tx, r.account_id, &r.to_number).await.map_err(fail)?;
    type Check = (bool, Option<String>, Option<String>, Option<Value>, bool, String, bool, Option<i32>, Option<i32>, Option<String>);
    let (opted_out, consent, phone, props, held, tz, closed, start, end, conflict): Check = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM sms_opt_outs o WHERE o.account_id = a.id AND o.phone_number = $3),
                p.sms_consent, p.phone_number, p.properties, COALESCE(p.marketing_hold, false), a.timezone, a.closed_at IS NOT NULL,
                ss.sms_start_minute, ss.sms_end_minute, ss.sms_zone_conflict
         FROM accounts a LEFT JOIN profiles p ON p.id = $2 AND p.account_id = a.id LEFT JOIN send_settings ss ON ss.account_id = a.id
         WHERE a.id = $1 FOR SHARE OF a",
    )
    .bind(r.account_id)
    .bind(r.profile_id)
    .bind(&r.to_number)
    .fetch_one(&mut *tx)
    .await?;
    // A cancel either lands before this claim (and is seen) or waits for it.
    let campaign: Option<String> = match r.campaign_id {
        Some(c) => sqlx::query_scalar("SELECT status FROM campaigns WHERE id = $1 FOR SHARE").bind(c).fetch_optional(&mut *tx).await?,
        None => None,
    };
    let same_number = phone.as_deref().and_then(crate::models::normalize_phone).as_deref() == Some(r.to_number.as_str());
    let stop = if closed {
        Some(Stop::Skip("account closed"))
    } else if opted_out {
        Some(Stop::Skip("opted out"))
    } else if consent.is_none() {
        Some(Stop::Skip("profile deleted"))
    } else if r.campaign_id.is_some() && matches!(campaign.as_deref(), Some("cancelled") | None) {
        Some(Stop::Skip("campaign cancelled"))
    } else if provider.is_none() {
        Some(Stop::Skip("texting isn't set up"))
    } else if !funded {
        Some(Stop::Skip("insufficient credit"))
    } else if r.is_test {
        None
    } else if consent.as_deref() != Some("subscribed") {
        Some(Stop::Skip("no sms consent"))
    } else if !same_number {
        Some(Stop::Skip("their number changed"))
    } else if r.strict && held {
        Some(Stop::Skip(crate::marketing_hold::SKIP_REASON))
    } else {
        let tz: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::America::Los_Angeles);
        let ((s, e), skip_conflicts) = crate::features::sendtime::settings::window_from(start.zip(end), conflict.as_deref());
        let zones = crate::features::sendtime::settings::sms_zones(props.as_ref().unwrap_or(&Value::Null), phone.as_deref(), tz);
        let hours = crate::features::sendtime::settings::decide(&zones, Utc::now(), s, e, skip_conflicts);
        if hours.skip {
            Some(Stop::Skip("time zones disagree"))
        } else {
            hours.wait.map(Stop::Wait)
        }
    };
    match stop {
        Some(Stop::Wait(until)) => {
            // Quiet hours: the fallback waits for the window (still queued; checked again then).
            crate::jobs::enqueue(&mut *tx, "rcs_fallback", json!({"message_id": message_id}), Some(until)).await?;
            tx.commit().await?;
            return Ok(());
        }
        Some(Stop::Skip(why)) => {
            let n = sqlx::query("UPDATE rcs_messages SET state = 'fallback_skipped', fallback_reason = $2 WHERE message_id = $1 AND state = 'fallback_queued'")
                .bind(message_id)
                .bind(why)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if n > 0 {
                sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1").bind(message_id).bind(format!("RCS wasn't delivered and the text wasn't sent: {why}")).execute(&mut *tx).await?;
                if !r.is_test {
                    crate::features::messages_unsent_in(&mut tx, &state.db, r.account_id, &[message_id]).await;
                }
            }
            tx.commit().await?;
            return Ok(());
        }
        None => {}
    }
    let claimed: Option<Uuid> = sqlx::query_scalar("UPDATE rcs_messages SET state = 'fallback_sending' WHERE message_id = $1 AND state = 'fallback_queued' RETURNING message_id")
        .bind(message_id)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    if claimed.is_none() {
        return Ok(());
    }
    let Some(provider) = provider else { return Ok(()) };
    let sms = OutgoingSms { message_id, to: r.to_number.clone(), from: r.from_number.clone(), body: r.sms_body.clone(), media_url: None };
    let result = match tokio::time::timeout(PROVIDER_TIMEOUT, provider.send(&sms)).await {
        Ok(res) => res,
        Err(_) => Err(anyhow::anyhow!("the texting provider didn't answer within {} seconds; not sent again", PROVIDER_TIMEOUT.as_secs())),
    };
    let mut tx = state.db.begin().await?;
    match result {
        Ok(pid) => {
            let moved: Option<Uuid> = sqlx::query_scalar(
                "UPDATE rcs_messages SET state = 'fallback_sent', route = 'sms', sms_provider_id = $2 WHERE message_id = $1 AND state = 'fallback_sending' RETURNING message_id",
            )
            .bind(message_id)
            .bind(&pid)
            .fetch_optional(&mut *tx)
            .await?;
            if moved.is_some() {
                sqlx::query("UPDATE messages SET status = 'sent', provider_message_id = $2, error = NULL, segments = $3, cost_micros = $4, delivered_at = NULL WHERE id = $1")
                    .bind(message_id)
                    .bind(&pid)
                    .bind(segments as i32)
                    .bind(cost)
                    .execute(&mut *tx)
                    .await?;
                if cost > 0 {
                    crate::billing::debit_usage_tx(&mut tx, r.account_id, "sms", segments.max(1), cost).await.map_err(fail)?;
                }
            }
        }
        Err(e) => {
            let msg = format!("RCS wasn't delivered and the text failed: {e:#}");
            sqlx::query("UPDATE rcs_messages SET state = 'failed', fallback_reason = $2 WHERE message_id = $1 AND state = 'fallback_sending'").bind(message_id).bind(&msg).execute(&mut *tx).await?;
            sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1").bind(message_id).bind(&msg).execute(&mut *tx).await?;
            if !r.is_test {
                crate::features::messages_unsent_in(&mut tx, &state.db, r.account_id, &[message_id]).await;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}
