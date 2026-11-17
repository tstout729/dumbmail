//! Monthly email plans for the hosted service. A plan is a prepaid monthly email allowance at about
//! 17% below pay-as-you-go. Each tier doubles the one below it in both price and emails, so an
//! upgrade mid-month costs exactly the price difference and adds exactly the extra emails.
//!
//! Payments go through Stripe directly rather than Stripe Billing (which adds 0.7%): starting or
//! upgrading a plan is a Checkout payment (the customer is present, so card checks like 3-D Secure
//! work), and monthly renewals charge the saved card from the scheduler, like auto-recharge.
//! Emails past the free allowance use the plan first and prepaid credit after that. A renewal that
//! is declined is retried daily; after three declines the plan ends and the account is
//! pay-as-you-go. See `run_renewals` for how charges stay exactly-once.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::billing::{self, to_usd};
use crate::error::{AppError, AppResult};
use crate::providers::email::OutgoingEmail;
use crate::site::thousands;
use crate::state::AppState;

pub const TIERS: i16 = 6;
const BASE_CENTS: i64 = 1_000;
const BASE_EMAILS: i64 = 30_000;
const MAX_RENEWAL_ATTEMPTS: i16 = 3;

pub fn valid_tier(tier: i16) -> bool {
    (1..=TIERS).contains(&tier)
}

/// Monthly price of a tier in cents: $10, $20, $40, ...
pub fn price_cents(tier: i16) -> i64 {
    BASE_CENTS << (tier - 1)
}

/// Emails per month for a tier: 30,000, 60,000, 120,000, ...
pub fn emails(tier: i16) -> i64 {
    BASE_EMAILS << (tier - 1)
}

fn usd(cents: i64) -> String {
    format!("${}", cents / 100)
}

pub fn options_json(payg_email_micros: i64) -> Value {
    json!((1..=TIERS)
        .map(|t| json!({
            "tier": t,
            "price": price_cents(t) as f64 / 100.0,
            "emails": emails(t),
            "price_per_1000": (price_cents(t) as f64 / 100.0) / (emails(t) as f64 / 1000.0),
            "pay_as_you_go_price": to_usd(payg_email_micros * emails(t)),
        }))
        .collect::<Vec<_>>())
}

#[derive(sqlx::FromRow)]
struct PlanRow {
    email_plan_tier: Option<i16>,
    email_plan_next_tier: Option<i16>,
    email_plan_cancel_at_period_end: bool,
    email_plan_period_start: Option<DateTime<Utc>>,
    email_plan_period_end: Option<DateTime<Utc>>,
    email_plan_emails_included: i64,
    email_plan_emails_used: i64,
    email_plan_renewal_attempts: i16,
    email_plan_renewal_failed_at: Option<DateTime<Utc>>,
    email_plan_ended_at: Option<DateTime<Utc>>,
    email_plan_ended_reason: Option<String>,
}

impl PlanRow {
    fn active(&self) -> bool {
        self.email_plan_tier.is_some() && self.email_plan_period_end.is_some_and(|e| e > Utc::now())
    }
}

async fn load(db: &PgPool, account_id: Uuid) -> AppResult<PlanRow> {
    Ok(sqlx::query_as(
        "SELECT email_plan_tier, email_plan_next_tier, email_plan_cancel_at_period_end, email_plan_period_start, email_plan_period_end,
                email_plan_emails_included, email_plan_emails_used, email_plan_renewal_attempts, email_plan_renewal_failed_at,
                email_plan_ended_at, email_plan_ended_reason
         FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?)
}

/// Plan emails left in the current period (0 without an active plan).
pub async fn emails_remaining(db: &PgPool, account_id: Uuid) -> AppResult<i64> {
    let left: Option<i64> = sqlx::query_scalar(
        "SELECT GREATEST(email_plan_emails_included - email_plan_emails_used, 0) FROM accounts
         WHERE id = $1 AND email_plan_tier IS NOT NULL AND email_plan_period_end > now()",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    Ok(left.unwrap_or(0))
}

/// Take one email from the plan. False when there's no active plan or it's used up.
pub async fn use_email(db: &PgPool, account_id: Uuid) -> AppResult<bool> {
    let n = sqlx::query(
        "UPDATE accounts SET email_plan_emails_used = email_plan_emails_used + 1
         WHERE id = $1 AND email_plan_tier IS NOT NULL AND email_plan_period_end > now()
           AND email_plan_emails_used < email_plan_emails_included",
    )
    .bind(account_id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(n == 1)
}

pub async fn plan_json(db: &PgPool, account_id: Uuid) -> AppResult<Value> {
    let r = load(db, account_id).await?;
    let Some(tier) = r.email_plan_tier else {
        let recently_ended = r.email_plan_ended_at.filter(|t| Utc::now() - *t < chrono::Duration::days(30));
        return Ok(json!({
            "status": "none",
            "ended_at": recently_ended,
            "ended_reason": recently_ended.and(r.email_plan_ended_reason),
        }));
    };
    let active = r.active();
    let next = r.email_plan_next_tier.filter(|t| *t != tier);
    let status = if r.email_plan_cancel_at_period_end {
        "cancelling"
    } else if active {
        "active"
    } else if r.email_plan_renewal_attempts > 0 {
        "renewal_failed"
    } else {
        // The month has ended and the renewal charge is on its way.
        "renewing"
    };
    Ok(json!({
        "status": status,
        "tier": tier,
        "price": price_cents(tier) as f64 / 100.0,
        "emails_included": r.email_plan_emails_included,
        "emails_used": r.email_plan_emails_used,
        "emails_remaining": if active { (r.email_plan_emails_included - r.email_plan_emails_used).max(0) } else { 0 },
        "period_start": r.email_plan_period_start,
        "period_end": r.email_plan_period_end,
        "next_tier": next,
        "next_price": next.map(|t| price_cents(t) as f64 / 100.0),
        "next_emails": next.map(emails),
        "renewal_attempts": r.email_plan_renewal_attempts,
        "renewal_failed_at": r.email_plan_renewal_failed_at,
        "max_renewal_attempts": MAX_RENEWAL_ATTEMPTS,
    }))
}

pub async fn payments_json(db: &PgPool, account_id: Uuid) -> AppResult<Value> {
    let rows: Vec<(Uuid, String, i16, i64, bool, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, kind, tier, amount_cents, credited_to_balance, created_at FROM plan_payments WHERE account_id = $1 ORDER BY created_at DESC LIMIT 24",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(json!(rows
        .into_iter()
        .map(|(id, kind, tier, cents, credited, at)| {
            let what = match kind.as_str() {
                "start" => format!("Email plan: {} emails a month", thousands(emails(tier))),
                "upgrade" => format!("Upgrade to {} emails a month", thousands(emails(tier))),
                _ => format!("Email plan renewal: {} emails", thousands(emails(tier))),
            };
            let description = if credited { format!("{what} (plan had changed; added as credit)") } else { what };
            json!({"id": id, "kind": kind, "tier": tier, "amount": cents as f64 / 100.0, "description": description, "created_at": at})
        })
        .collect::<Vec<_>>()))
}

/// Choose a plan. Starting one or moving up returns a Stripe Checkout URL for the amount due now
/// (the full price, or the difference for an upgrade). Moving down takes effect at the next
/// renewal, and choosing the current tier undoes a pending change or cancellation; both return None.
pub async fn choose(state: &AppState, account_id: Uuid, tier: i16) -> AppResult<Option<String>> {
    if !state.cfg.hosted {
        return Err(AppError::bad("self-hosted installs don't use billing"));
    }
    if !valid_tier(tier) {
        return Err(AppError::bad(format!("plan must be 1 to {TIERS}")));
    }
    let r = load(&state.db, account_id).await?;
    let current = r.email_plan_tier.filter(|_| r.active());
    match current {
        None => checkout(state, account_id, 0, tier).await.map(Some),
        Some(cur) if tier > cur => checkout(state, account_id, cur, tier).await.map(Some),
        Some(cur) => {
            let next = (tier < cur).then_some(tier);
            sqlx::query("UPDATE accounts SET email_plan_next_tier = $2, email_plan_cancel_at_period_end = false WHERE id = $1")
                .bind(account_id)
                .bind(next)
                .execute(&state.db)
                .await?;
            Ok(None)
        }
    }
}

/// Stop renewing; the plan lasts to the end of the paid month.
pub async fn cancel(db: &PgPool, account_id: Uuid) -> AppResult<()> {
    let n = sqlx::query("UPDATE accounts SET email_plan_cancel_at_period_end = true, email_plan_next_tier = NULL WHERE id = $1 AND email_plan_tier IS NOT NULL")
        .bind(account_id)
        .execute(db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::bad("there's no plan to cancel"));
    }
    Ok(())
}

/// Undo a cancellation before the month ends.
pub async fn resume(db: &PgPool, account_id: Uuid) -> AppResult<()> {
    let n = sqlx::query("UPDATE accounts SET email_plan_cancel_at_period_end = false WHERE id = $1 AND email_plan_tier IS NOT NULL AND email_plan_period_end > now()")
        .bind(account_id)
        .execute(db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::bad("there's no plan to keep; choose one instead"));
    }
    Ok(())
}

async fn checkout(state: &AppState, account_id: Uuid, from_tier: i16, tier: i16) -> AppResult<String> {
    let due = price_cents(tier) - if from_tier > 0 { price_cents(from_tier) } else { 0 };
    let customer = billing::ensure_customer(state, account_id).await?;
    let base = state.cfg.public_url();
    let monthly = format!("{} emails a month", thousands(emails(tier)));
    let (name, description, note) = if from_tier == 0 {
        (
            "DumbMail email plan".to_string(),
            format!("{monthly}, renews monthly"),
            format!("Renews every month at {} until you cancel. Cancel any time in Settings → Billing.", usd(price_cents(tier))),
        )
    } else {
        (
            "DumbMail email plan upgrade".to_string(),
            format!("{} more emails this month, then {monthly}", thousands(emails(tier) - emails(from_tier))),
            format!("Your plan then renews every month at {} until you cancel. Cancel any time in Settings → Billing.", usd(price_cents(tier))),
        )
    };
    let mut form: Vec<(String, String)> = vec![
        ("mode".into(), "payment".into()),
        ("customer".into(), customer),
        ("client_reference_id".into(), account_id.to_string()),
        ("line_items[0][quantity]".into(), "1".into()),
        ("line_items[0][price_data][currency]".into(), "usd".into()),
        ("line_items[0][price_data][unit_amount]".into(), due.to_string()),
        ("line_items[0][price_data][product_data][name]".into(), name),
        ("line_items[0][price_data][product_data][description]".into(), description),
        ("payment_intent_data[setup_future_usage]".into(), "off_session".into()),
        ("payment_intent_data[metadata][account_id]".into(), account_id.to_string()),
        ("payment_intent_data[metadata][kind]".into(), "plan".into()),
        ("metadata[account_id]".into(), account_id.to_string()),
        ("metadata[kind]".into(), "plan".into()),
        ("metadata[tier]".into(), tier.to_string()),
        ("metadata[from_tier]".into(), from_tier.to_string()),
        ("custom_text[submit][message]".into(), note),
        // Short-lived, so an old tab can't pay for a plan change that no longer makes sense.
        ("expires_at".into(), (Utc::now().timestamp() + 31 * 60).to_string()),
        ("success_url".into(), format!("{base}/settings/billing?plan=success")),
        ("cancel_url".into(), format!("{base}/settings/billing?plan=cancelled")),
    ];
    billing::cards_only(&mut form);
    let s = billing::stripe_post(state, "checkout/sessions", &form).await?;
    s["url"].as_str().map(String::from).ok_or_else(|| AppError::Internal(anyhow::anyhow!("Stripe returned no checkout url")))
}

/// Apply a paid plan Checkout session (from the verified webhook). Runs once per session. If the
/// plan changed between checkout and payment so the payment no longer fits (a second start, or an
/// upgrade after the plan ended), the money is added to prepaid credit instead of being lost.
pub async fn apply_checkout(db: &PgPool, account_id: Uuid, session: &Value) -> AppResult<()> {
    let meta = &session["metadata"];
    let tier = meta["tier"].as_str().and_then(|s| s.parse::<i16>().ok()).filter(|t| valid_tier(*t));
    let from_tier = meta["from_tier"].as_str().and_then(|s| s.parse::<i16>().ok()).unwrap_or(0);
    let (Some(tier), Some(session_id)) = (tier, session["id"].as_str()) else { return Ok(()) };
    let cents = session["amount_total"].as_i64().unwrap_or(0);
    let mut tx = db.begin().await?;
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO plan_payments (id, account_id, kind, tier, amount_cents, stripe_ref) VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (stripe_ref) DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(if from_tier == 0 { "start" } else { "upgrade" })
    .bind(tier)
    .bind(cents)
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(payment_id) = inserted else { return Ok(()) };
    let (cur, end): (Option<i16>, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT email_plan_tier, email_plan_period_end FROM accounts WHERE id = $1 FOR UPDATE").bind(account_id).fetch_one(&mut *tx).await?;
    let active = cur.is_some() && end.is_some_and(|e| e > Utc::now());
    if from_tier == 0 && !active {
        let now = Utc::now();
        sqlx::query(
            "UPDATE accounts SET email_plan_tier = $2, email_plan_next_tier = NULL, email_plan_cancel_at_period_end = false,
                    email_plan_period_start = $4, email_plan_period_end = $5,
                    email_plan_emails_included = $3, email_plan_emails_used = 0, email_plan_renewal_attempts = 0,
                    email_plan_renewal_failed_at = NULL, email_plan_ended_at = NULL, email_plan_ended_reason = NULL
             WHERE id = $1",
        )
        .bind(account_id)
        .bind(tier)
        .bind(emails(tier))
        .bind(now)
        .bind(add_month(now))
        .execute(&mut *tx)
        .await?;
    } else if from_tier > 0 && valid_tier(from_tier) && tier > from_tier && active {
        // Paid the difference between from_tier and tier, so add exactly the difference in emails.
        sqlx::query(
            "UPDATE accounts SET email_plan_tier = GREATEST(email_plan_tier, $2), email_plan_emails_included = email_plan_emails_included + $3,
                    email_plan_next_tier = NULL, email_plan_cancel_at_period_end = false
             WHERE id = $1",
        )
        .bind(account_id)
        .bind(tier)
        .bind(emails(tier) - emails(from_tier))
        .execute(&mut *tx)
        .await?;
    } else {
        tracing::warn!(account = %account_id, session = session_id, "plan payment no longer fits the account's plan; adding it as credit");
        billing::post_entry_tx(&mut tx, account_id, "plan_credit", cents * 10_000, &format!("Plan payment added as credit (${:.2})", cents as f64 / 100.0), session_id).await?;
        sqlx::query("UPDATE plan_payments SET credited_to_balance = true WHERE id = $1").bind(payment_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// One month later on the Pacific calendar (the business's time zone), so a plan bought on the
/// 30th at 5 p.m. renews on the 30th (or the month's last day) at 5 p.m. Pacific, whatever the
/// database session's time zone.
pub fn add_month(t: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::{Months, TimeZone};
    let tz = chrono_tz::America::Los_Angeles;
    let local = t.with_timezone(&tz).naive_local();
    let Some(next) = local.checked_add_months(Months::new(1)) else { return t + chrono::Duration::days(30) };
    // A time skipped by the spring-forward change doesn't exist; use the hour after.
    tz.from_local_datetime(&next)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(next + chrono::Duration::hours(1))).earliest())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or(t + chrono::Duration::days(30))
}

/// Scheduler: renew plans whose month has ended.
///
/// Each charge attempt is first written to `plan_renewals` with its exact request (card, amount,
/// idempotency key), then sent. A definite decline counts as a failed attempt; a declined renewal
/// is retried a day later and the plan ends after `MAX_RENEWAL_ATTEMPTS`. Anything else (network
/// trouble, Stripe errors, a 409 or 429) leaves the attempt pending, and before it's ever sent
/// again Stripe is asked whether that payment already exists. So an attempt is always settled
/// before the next one starts, and a lost response can't turn into a second charge.
///
/// Results only change the account while it still has the month being renewed, so an old attempt
/// can't undo a plan bought or changed since; a payment that no longer fits becomes credit.
pub async fn run_renewals(state: &AppState) -> anyhow::Result<()> {
    if !state.cfg.hosted || state.cfg.stripe_secret_key.is_none() {
        return Ok(());
    }
    // 1. Attempts whose outcome isn't known yet. Claiming one keeps other workers off it.
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE plan_renewals SET claimed_at = now()
         WHERE id IN (SELECT id FROM plan_renewals WHERE status = 'pending' AND claimed_at < now() - interval '2 minutes'
                      ORDER BY claimed_at LIMIT 50 FOR UPDATE SKIP LOCKED)
         RETURNING id",
    )
    .fetch_all(&state.db)
    .await?;
    for id in stale {
        if let Err(e) = resolve(state, id).await {
            tracing::error!(renewal = %id, error = %format!("{e:#}"), "couldn't settle plan renewal");
        }
    }
    // 2. Plans due for renewal with nothing in flight. A cancelled plan ends right away, even
    //    while a failed renewal is waiting for its retry.
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT a.id FROM accounts a
         WHERE a.email_plan_tier IS NOT NULL AND a.email_plan_period_end <= now()
           AND (a.email_plan_cancel_at_period_end OR a.email_plan_renewal_failed_at IS NULL OR a.email_plan_renewal_failed_at < now() - interval '1 day')
           AND NOT EXISTS (SELECT 1 FROM plan_renewals r WHERE r.account_id = a.id AND r.period_end = a.email_plan_period_end AND r.status = 'pending')
         ORDER BY a.email_plan_period_end LIMIT 50",
    )
    .fetch_all(&state.db)
    .await?;
    for account_id in due {
        match start_attempt(&state.db, account_id).await {
            Ok(Some(id)) => {
                if let Err(e) = resolve(state, id).await {
                    tracing::error!(renewal = %id, error = %format!("{e:#}"), "couldn't settle plan renewal");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::error!(account = %account_id, error = %format!("{e:#}"), "couldn't start plan renewal"),
        }
    }
    Ok(())
}

/// Under the account's lock, re-check that the plan is still due (it may have been cancelled or
/// changed since the scheduler looked), then record the attempt. None when there's nothing to charge.
async fn start_attempt(db: &PgPool, account_id: Uuid) -> anyhow::Result<Option<Uuid>> {
    let mut tx = db.begin().await?;
    let row: Option<(i16, Option<i16>, bool, DateTime<Utc>, i16, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT email_plan_tier, email_plan_next_tier, email_plan_cancel_at_period_end, email_plan_period_end, email_plan_renewal_attempts,
                stripe_customer_id, stripe_payment_method_id
         FROM accounts
         WHERE id = $1 AND email_plan_tier IS NOT NULL AND email_plan_period_end <= now()
           AND (email_plan_cancel_at_period_end OR email_plan_renewal_failed_at IS NULL OR email_plan_renewal_failed_at < now() - interval '1 day')
         FOR UPDATE SKIP LOCKED",
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((tier, next, cancelling, period_end, attempts, customer, pm)) = row else { return Ok(None) };
    if cancelling {
        end_plan(&mut tx, account_id, period_end, "cancelled").await?;
        tx.commit().await?;
        return Ok(None);
    }
    let in_flight: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM plan_renewals WHERE account_id = $1 AND period_end = $2 AND status = 'pending')")
        .bind(account_id)
        .bind(period_end)
        .fetch_one(&mut *tx)
        .await?;
    if in_flight {
        return Ok(None);
    }
    let tier = next.filter(|t| valid_tier(*t)).unwrap_or(tier);
    let id = Uuid::now_v7();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO plan_renewals (id, account_id, period_end, attempt, tier, amount_cents, stripe_customer, stripe_payment_method, idempotency_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (account_id, period_end, attempt) DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(account_id)
    .bind(period_end)
    .bind(attempts)
    .bind(tier)
    .bind(price_cents(tier))
    .bind(customer)
    .bind(pm)
    .bind(format!("plan-renewal-{id}"))
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(inserted)
}

#[derive(sqlx::FromRow)]
struct Renewal {
    id: Uuid,
    account_id: Uuid,
    period_end: DateTime<Utc>,
    attempt: i16,
    tier: i16,
    amount_cents: i64,
    stripe_customer: Option<String>,
    stripe_payment_method: Option<String>,
    idempotency_key: String,
    created_at: DateTime<Utc>,
}

/// Settle a pending attempt: find out whether Stripe already has its payment, otherwise send it.
async fn resolve(state: &AppState, id: Uuid) -> anyhow::Result<()> {
    let r: Option<Renewal> = sqlx::query_as(
        "SELECT id, account_id, period_end, attempt, tier, amount_cents, stripe_customer, stripe_payment_method, idempotency_key, created_at
         FROM plan_renewals WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some(r) = r else { return Ok(()) };
    let (Some(customer), Some(pm)) = (r.stripe_customer.clone(), r.stripe_payment_method.clone()) else {
        return declined(state, &r, "no saved card").await;
    };
    match find_intent(state, &r, &customer).await {
        Err(e) => {
            tracing::warn!(renewal = %r.id, error = %e, "couldn't check Stripe for a renewal payment; will retry");
            return Ok(());
        }
        Ok(Some(pi)) => return settle(state, &r, &pi).await,
        Ok(None) => {}
    }
    // Stripe keeps a key's first answer, including a 500, for about a day. If an earlier send left
    // nothing behind (no payment exists for this attempt), a fresh key lets the retry go through.
    let key = if Utc::now() - r.created_at > chrono::Duration::minutes(10) {
        let k = format!("plan-renewal-{}-{}", r.id, Utc::now().timestamp());
        sqlx::query("UPDATE plan_renewals SET idempotency_key = $2 WHERE id = $1").bind(r.id).bind(&k).execute(&state.db).await?;
        k
    } else {
        r.idempotency_key.clone()
    };
    let form: Vec<(String, String)> = vec![
        ("amount".into(), r.amount_cents.to_string()),
        ("currency".into(), "usd".into()),
        ("customer".into(), customer),
        ("payment_method".into(), pm),
        ("off_session".into(), "true".into()),
        ("confirm".into(), "true".into()),
        ("description".into(), format!("DumbMail email plan: {} emails a month", thousands(emails(r.tier)))),
        ("metadata[account_id]".into(), r.account_id.to_string()),
        ("metadata[kind]".into(), "plan_renewal".into()),
        ("metadata[renewal_id]".into(), r.id.to_string()),
        ("metadata[tier]".into(), r.tier.to_string()),
    ];
    match billing::stripe_charge(state, "payment_intents", &form, &key).await.map_err(|e| anyhow::anyhow!(e.public_message()))? {
        billing::StripeOutcome::Ok(pi) => settle(state, &r, &pi).await,
        billing::StripeOutcome::Refused(body) => {
            let why = body["error"]["message"].as_str().unwrap_or("payment refused").to_string();
            declined(state, &r, &why).await
        }
        billing::StripeOutcome::Unknown(e) => {
            tracing::warn!(renewal = %r.id, error = %e, "plan renewal outcome unknown; will check again");
            Ok(())
        }
    }
}

/// The PaymentIntent Stripe has for this attempt, if any (matched by its renewal id).
async fn find_intent(state: &AppState, r: &Renewal, customer: &str) -> Result<Option<Value>, String> {
    let since = (r.created_at - chrono::Duration::minutes(5)).timestamp();
    let v = billing::stripe_get(state, &format!("payment_intents?customer={customer}&created[gte]={since}&limit=100"))
        .await
        .map_err(|e| e.public_message())?;
    let Some(list) = v["data"].as_array() else { return Err(format!("unexpected answer: {}", v["error"]["message"].as_str().unwrap_or("no data"))) };
    let id = r.id.to_string();
    Ok(list.iter().find(|pi| pi["metadata"]["renewal_id"].as_str() == Some(id.as_str())).cloned())
}

async fn settle(state: &AppState, r: &Renewal, pi: &Value) -> anyhow::Result<()> {
    let pi_id = pi["id"].as_str().unwrap_or_default();
    match pi["status"].as_str().unwrap_or_default() {
        "succeeded" => renewal_succeeded(&state.db, r.id, pi_id).await.map_err(|e| anyhow::anyhow!(e.public_message())),
        "requires_action" => {
            // The bank wants the customer present (e.g. 3-D Secure). Don't leave it half-done.
            let _ = billing::stripe_post(state, &format!("payment_intents/{pi_id}/cancel"), &[]).await;
            declined(state, r, "the card needs the customer to approve the payment").await
        }
        "requires_payment_method" | "canceled" => {
            let why = pi["last_payment_error"]["message"].as_str().unwrap_or("payment failed").to_string();
            declined(state, r, &why).await
        }
        // processing and the like: check again later.
        other => {
            tracing::info!(renewal = %r.id, status = other, "plan renewal payment not final yet");
            Ok(())
        }
    }
}

/// A renewal payment went through: record it once and start the next month, as long as the account
/// still has the month this attempt was for. Otherwise the money becomes prepaid credit. Called by
/// the scheduler and by Stripe's webhook, whichever sees it first.
pub async fn renewal_succeeded(db: &PgPool, renewal_id: Uuid, payment_intent: &str) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let r: Option<(Uuid, DateTime<Utc>, i16, i64, String)> =
        sqlx::query_as("SELECT account_id, period_end, tier, amount_cents, status FROM plan_renewals WHERE id = $1 FOR UPDATE")
            .bind(renewal_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((account_id, period_end, tier, cents, status)) = r else { return Ok(()) };
    if status == "succeeded" {
        return Ok(());
    }
    sqlx::query("UPDATE plan_renewals SET status = 'succeeded', payment_intent = $2, resolved_at = now() WHERE id = $1")
        .bind(renewal_id)
        .bind(payment_intent)
        .execute(&mut *tx)
        .await?;
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO plan_payments (id, account_id, kind, tier, amount_cents, stripe_ref) VALUES ($1, $2, 'renewal', $3, $4, $5)
         ON CONFLICT (stripe_ref) DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(tier)
    .bind(cents)
    .bind(payment_intent)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(payment_id) = inserted else {
        tx.commit().await?;
        return Ok(());
    };
    let cur: (Option<i16>, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT email_plan_tier, email_plan_period_end FROM accounts WHERE id = $1 FOR UPDATE").bind(account_id).fetch_one(&mut *tx).await?;
    if cur.0.is_some() && cur.1 == Some(period_end) {
        // On time, the new month starts where the old one ended; after retries, it starts now so the
        // customer gets a full month for what they paid.
        let now = Utc::now();
        let start = if period_end > now - chrono::Duration::hours(1) { period_end } else { now };
        sqlx::query(
            "UPDATE accounts SET email_plan_tier = $2, email_plan_next_tier = NULL, email_plan_period_start = $3, email_plan_period_end = $4,
                    email_plan_emails_included = $5, email_plan_emails_used = 0, email_plan_renewal_attempts = 0, email_plan_renewal_failed_at = NULL
             WHERE id = $1",
        )
        .bind(account_id)
        .bind(tier)
        .bind(start)
        .bind(add_month(start))
        .bind(emails(tier))
        .execute(&mut *tx)
        .await?;
    } else {
        tracing::warn!(account = %account_id, payment_intent, "plan changed before this renewal settled; adding the payment as credit");
        billing::post_entry_tx(&mut tx, account_id, "plan_credit", cents * 10_000, &format!("Plan payment added as credit (${:.2})", cents as f64 / 100.0), payment_intent).await?;
        sqlx::query("UPDATE plan_payments SET credited_to_balance = true WHERE id = $1").bind(payment_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A definite decline: count it against the month it was for (only if the account still has that
/// month), end the plan after the last try, and tell the owner.
async fn declined(state: &AppState, r: &Renewal, why: &str) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    let marked = sqlx::query("UPDATE plan_renewals SET status = 'declined', failure = $2, resolved_at = now() WHERE id = $1 AND status = 'pending'")
        .bind(r.id)
        .bind(why)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if marked == 0 {
        return Ok(());
    }
    let attempts: Option<i16> = sqlx::query_scalar(
        "UPDATE accounts SET email_plan_renewal_attempts = GREATEST(email_plan_renewal_attempts, $3), email_plan_renewal_failed_at = now()
         WHERE id = $1 AND email_plan_tier IS NOT NULL AND email_plan_period_end = $2
         RETURNING email_plan_renewal_attempts",
    )
    .bind(r.account_id)
    .bind(r.period_end)
    .bind(r.attempt + 1)
    .fetch_optional(&mut *tx)
    .await?;
    let ended = match attempts {
        Some(a) if a >= MAX_RENEWAL_ATTEMPTS => end_plan(&mut tx, r.account_id, r.period_end, "renewal_failed").await?,
        _ => false,
    };
    tx.commit().await?;
    tracing::warn!(account = %r.account_id, renewal = %r.id, attempts = ?attempts, reason = why, "plan renewal declined");
    let base = state.cfg.public_url();
    if ended {
        notify_owner(
            state,
            r.account_id,
            "Your DumbMail email plan has ended",
            &format!(
                "We couldn't charge your card for your DumbMail email plan after {MAX_RENEWAL_ATTEMPTS} tries, so the plan has ended.\n\n\
                 Your account now uses pay-as-you-go: emails past the free allowance come out of your prepaid credit. \
                 To start a plan again, go to {base}/settings/billing."
            ),
        )
        .await;
    } else if attempts == Some(1) {
        notify_owner(
            state,
            r.account_id,
            "We couldn't renew your DumbMail email plan",
            &format!(
                "We couldn't charge your card for this month's DumbMail email plan ({}). We'll try again tomorrow.\n\n\
                 Until it goes through, emails past the free allowance come out of your prepaid credit. \
                 To use a different card, add credit or choose your plan again at {base}/settings/billing. \
                 You can also cancel the plan there.",
                usd(r.amount_cents)
            ),
        )
        .await;
    }
    Ok(())
}

/// End the plan, but only if it's still the one for `period_end`. True when it ended.
async fn end_plan(tx: &mut sqlx::PgConnection, account_id: Uuid, period_end: DateTime<Utc>, reason: &str) -> sqlx::Result<bool> {
    let n = sqlx::query(
        "UPDATE accounts SET email_plan_tier = NULL, email_plan_next_tier = NULL, email_plan_cancel_at_period_end = false,
                email_plan_emails_included = 0, email_plan_emails_used = 0, email_plan_renewal_attempts = 0,
                email_plan_renewal_failed_at = NULL, email_plan_ended_at = now(), email_plan_ended_reason = $3
         WHERE id = $1 AND email_plan_tier IS NOT NULL AND email_plan_period_end = $2",
    )
    .bind(account_id)
    .bind(period_end)
    .bind(reason)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Email the account's first member from SUPPORT_EMAIL. Failures are only logged.
async fn notify_owner(state: &AppState, account_id: Uuid, subject: &str, text: &str) {
    let Some(from) = state.cfg.support_email.as_deref().filter(|s| s.contains('@')) else {
        tracing::warn!(account = %account_id, subject, "no SUPPORT_EMAIL set; billing notice not emailed");
        return;
    };
    let owner: Option<String> = sqlx::query_scalar("SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 ORDER BY m.created_at LIMIT 1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(to) = owner else { return };
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
        tracing::error!(account = %account_id, error = %format!("{e:#}"), "couldn't email billing notice");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_follow_the_pacific_calendar() {
        let pt = |s: &str| {
            use chrono::TimeZone;
            chrono_tz::America::Los_Angeles.from_local_datetime(&chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()).unwrap().with_timezone(&Utc)
        };
        // 5 p.m. Pacific on Jan 30 is Jan 31 in UTC; the plan still renews on Feb 28 at 5 p.m. Pacific.
        assert_eq!(add_month(pt("2027-01-30 17:00")), pt("2027-02-28 17:00"));
        assert_eq!(add_month(pt("2026-11-16 22:31")), pt("2026-12-16 22:31"));
        // Across daylight saving: same wall-clock time.
        assert_eq!(add_month(pt("2027-02-20 09:00")), pt("2027-03-20 09:00"));
        // 2:30 a.m. on Mar 14, 2027 doesn't exist in Pacific time; it moves to 3:30.
        assert_eq!(add_month(pt("2027-02-14 02:30")), pt("2027-03-14 03:30"));
    }

    #[test]
    fn tiers_double_and_undercut_pay_as_you_go() {
        assert_eq!((price_cents(1), emails(1)), (1_000, 30_000));
        assert_eq!((price_cents(6), emails(6)), (32_000, 960_000));
        for t in 1..TIERS {
            assert_eq!(price_cents(t + 1), 2 * price_cents(t));
            assert_eq!(emails(t + 1), 2 * emails(t));
        }
        // $0.40 per 1,000 pay-as-you-go → every tier is ~17% cheaper.
        let per_1000 = price_cents(1) as f64 / 100.0 / (emails(1) as f64 / 1000.0);
        assert!((per_1000 - 0.3333).abs() < 0.001);
        assert!(!valid_tier(0) && !valid_tier(TIERS + 1) && valid_tier(1) && valid_tier(TIERS));
    }

    #[test]
    fn numbers_read_naturally() {
        assert_eq!(thousands(emails(6)), "960,000");
        assert_eq!(usd(32_000), "$320");
    }
}
