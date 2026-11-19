//! Pay-as-you-go billing for the hosted service: prepaid credit, per-message charges at cost plus
//! markup, a free monthly email allowance, Stripe Checkout top-ups and automatic recharge.
//! Self-hosted installs (HOSTED=false) never charge.

use chrono::{Datelike, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const MICROS: f64 = 1_000_000.0;

/// Smallest card top-up. Stripe takes 2.9% + 30¢ per payment, so the fixed 30¢ makes small
/// top-ups expensive: 5.9% at $10, 4.1% at $25.
pub const MIN_TOPUP_USD: f64 = 25.0;

pub fn to_micros(usd: f64) -> i64 {
    (usd * MICROS).round() as i64
}

pub fn to_usd(micros: i64) -> f64 {
    (micros as f64 / MICROS * 1_000_000.0).round() / 1_000_000.0
}

pub struct Prices {
    pub email: i64,
    pub sms_segment: i64,
    pub mms: i64,
    pub number_monthly: i64,
    /// Per push notification (one person, all their devices); 0 unless PRICE_PUSH is set.
    pub push: i64,
}

pub fn prices(state: &AppState) -> Prices {
    let c = &state.cfg;
    Prices {
        email: to_micros(c.price_email_per_1000 / 1000.0),
        sms_segment: to_micros(c.price_sms_segment),
        mms: to_micros(c.price_mms),
        number_monthly: to_micros(c.price_number_monthly),
        push: to_micros(c.price_push.max(0.0)),
    }
}

/// Calendar month key ("2026-11") in the account's time zone.
async fn month_key<'e>(db: impl sqlx::PgExecutor<'e>, account_id: Uuid) -> AppResult<String> {
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(db).await?;
    let tz: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    let now = Utc::now().with_timezone(&tz);
    Ok(format!("{:04}-{:02}", now.year(), now.month()))
}

pub async fn balance(db: &PgPool, account_id: Uuid) -> AppResult<i64> {
    Ok(sqlx::query_scalar("SELECT balance_micros FROM accounts WHERE id = $1").bind(account_id).fetch_one(db).await?)
}

pub async fn free_emails_remaining(state: &AppState, account_id: Uuid) -> AppResult<i64> {
    let month = month_key(&state.db, account_id).await?;
    let (m, used): (Option<String>, i32) = sqlx::query_as("SELECT free_month, free_emails_used FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let used = if m.as_deref() == Some(month.as_str()) { used as i64 } else { 0 };
    Ok((state.cfg.free_emails_per_month - used).max(0))
}

/// Campaign pre-check estimate only: texts through the customer's own Telnyx or Twilio account are
/// free. Each message is still quoted and charged from the provider that actually sends it.
async fn sms_billed(state: &AppState, account_id: Uuid) -> AppResult<bool> {
    let mode: String = sqlx::query_scalar("SELECT sms_mode FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    Ok(mode != "byo")
}

/// Price of one message before sending (0 when free, self-hosted, or texting through the customer's own provider).
/// `own_provider` must come from the provider that actually sends the text (`phone::sender`).
pub async fn quote(state: &AppState, account_id: Uuid, channel: &str, segments: i64, mms: bool, own_provider: bool) -> AppResult<i64> {
    if !state.cfg.hosted {
        return Ok(0);
    }
    let p = prices(state);
    Ok(match channel {
        "email" => {
            if free_emails_remaining(state, account_id).await? > 0 || crate::subscription::emails_remaining(&state.db, account_id).await? > 0 {
                0
            } else {
                p.email
            }
        }
        "push" => p.push,
        "whatsapp" => crate::features::whatsapp::send::price(state),
        _ if own_provider => 0,
        _ if mms => p.mms,
        _ => p.sms_segment * segments.max(1),
    })
}

/// Refuse a send the balance can't cover.
pub async fn ensure_funds(state: &AppState, account_id: Uuid, needed: i64, what: &str) -> AppResult<()> {
    if !state.cfg.hosted || needed <= 0 {
        return Ok(());
    }
    let bal = balance(&state.db, account_id).await?;
    if bal < needed {
        return Err(AppError::LimitReached(format!(
            "{what} costs ${:.2} but your balance is ${:.2}. Add credit in Settings → Billing.",
            to_usd(needed),
            to_usd(bal.max(0))
        )));
    }
    Ok(())
}

/// Estimate a campaign's cost (free emails first) and check the balance covers it.
pub async fn check_campaign(state: &AppState, account_id: Uuid, channel: &str, recipients: i64, segments: i64, mms: bool) -> AppResult<i64> {
    if !state.cfg.hosted {
        return Ok(0);
    }
    let p = prices(state);
    let cost = if channel == "email" {
        let covered = free_emails_remaining(state, account_id).await? + crate::subscription::emails_remaining(&state.db, account_id).await?;
        (recipients - covered).max(0) * p.email
    } else if channel == "push" {
        recipients * p.push
    } else if channel == "whatsapp" {
        recipients * crate::features::whatsapp::send::price(state)
    } else if !sms_billed(state, account_id).await? {
        0
    } else if mms {
        recipients * p.mms
    } else {
        recipients * segments.max(1) * p.sms_segment
    };
    ensure_funds(state, account_id, cost, "This send").await?;
    Ok(cost)
}

/// Record a sent message: email uses the free allowance, then the monthly plan; everything else
/// is charged to the balance.
/// Returns the amount charged.
pub async fn charge_message(state: &AppState, account_id: Uuid, channel: &str, segments: i64, mms: bool, own_provider: bool) -> AppResult<i64> {
    if !state.cfg.hosted {
        return Ok(0);
    }
    let p = prices(state);
    let (kind, qty, amount) = if channel == "email" {
        let month = month_key(&state.db, account_id).await?;
        let used: i32 = sqlx::query_scalar(
            "UPDATE accounts SET free_emails_used = CASE WHEN free_month = $2 THEN free_emails_used + 1 ELSE 1 END, free_month = $2
             WHERE id = $1 RETURNING free_emails_used",
        )
        .bind(account_id)
        .bind(&month)
        .fetch_one(&state.db)
        .await?;
        // Free allowance first, then the monthly plan, then prepaid credit.
        let covered = used as i64 <= state.cfg.free_emails_per_month || crate::subscription::use_email(&state.db, account_id).await?;
        ("email", 1i64, if covered { 0 } else { p.email })
    } else if channel == "push" {
        if p.push <= 0 {
            return Ok(0);
        }
        ("push", 1, p.push)
    } else if own_provider {
        return Ok(0);
    } else if mms {
        ("mms", 1, p.mms)
    } else {
        ("sms", segments.max(1), p.sms_segment * segments.max(1))
    };
    debit_usage(&state.db, account_id, kind, qty, amount).await?;
    Ok(amount)
}

/// [`charge_message`] for one email, inside the caller's transaction, so the caller can record the
/// charge with its own row and charge it once (transactional email does).
pub async fn charge_email_tx(state: &AppState, tx: &mut sqlx::PgConnection, account_id: Uuid) -> AppResult<i64> {
    if !state.cfg.hosted {
        return Ok(0);
    }
    let month = month_key(&mut *tx, account_id).await?;
    let used: i32 = sqlx::query_scalar(
        "UPDATE accounts SET free_emails_used = CASE WHEN free_month = $2 THEN free_emails_used + 1 ELSE 1 END, free_month = $2
         WHERE id = $1 RETURNING free_emails_used",
    )
    .bind(account_id)
    .bind(&month)
    .fetch_one(&mut *tx)
    .await?;
    let covered = used as i64 <= state.cfg.free_emails_per_month || crate::subscription::use_email(&mut *tx, account_id).await?;
    let amount = if covered { 0 } else { prices(state).email };
    debit_usage_tx(tx, account_id, "email", 1, amount).await?;
    Ok(amount)
}

/// Debit the balance and roll the charge into today's usage row for this kind.
pub async fn debit_usage(db: &PgPool, account_id: Uuid, kind: &str, qty: i64, amount: i64) -> AppResult<()> {
    let mut tx = db.begin().await?;
    debit_usage_tx(&mut tx, account_id, kind, qty, amount).await?;
    tx.commit().await?;
    Ok(())
}

/// [`debit_usage`] inside the caller's transaction.
pub async fn debit_usage_tx(tx: &mut sqlx::PgConnection, account_id: Uuid, kind: &str, qty: i64, amount: i64) -> AppResult<()> {
    let label = match kind {
        "email" => "Emails sent",
        "sms" => "SMS segments sent",
        "mms" => "MMS sent",
        "push" => "Push notifications sent",
        "whatsapp" => "WhatsApp messages sent",
        _ => "Usage",
    };
    sqlx::query("UPDATE accounts SET balance_micros = balance_micros - $2 WHERE id = $1").bind(account_id).bind(amount).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO ledger (id, account_id, kind, day, quantity, amount_micros, description)
         VALUES ($1, $2, $3, (now() AT TIME ZONE (SELECT timezone FROM accounts WHERE id = $2))::date, $4, $5, $6)
         ON CONFLICT (account_id, kind, day) WHERE day IS NOT NULL
         DO UPDATE SET quantity = ledger.quantity + EXCLUDED.quantity, amount_micros = ledger.amount_micros + EXCLUDED.amount_micros",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(kind)
    .bind(qty)
    .bind(-amount)
    .bind(label)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// One-off charge or credit with a unique reference (idempotent). Positive amount = credit.
pub async fn post_entry(db: &PgPool, account_id: Uuid, kind: &str, amount: i64, description: &str, reference: &str) -> AppResult<bool> {
    let mut tx = db.begin().await?;
    let posted = post_entry_tx(&mut tx, account_id, kind, amount, description, reference).await?;
    tx.commit().await?;
    Ok(posted)
}

/// `post_entry` inside the caller's transaction.
pub async fn post_entry_tx(tx: &mut sqlx::PgConnection, account_id: Uuid, kind: &str, amount: i64, description: &str, reference: &str) -> AppResult<bool> {
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO ledger (id, account_id, kind, quantity, amount_micros, description, reference) VALUES ($1, $2, $3, 1, $4, $5, $6)
         ON CONFLICT (kind, reference) WHERE reference IS NOT NULL AND day IS NULL DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(kind)
    .bind(amount)
    .bind(description)
    .bind(reference)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        return Ok(false);
    }
    sqlx::query("UPDATE accounts SET balance_micros = balance_micros + $2 WHERE id = $1").bind(account_id).bind(amount).execute(&mut *tx).await?;
    Ok(true)
}

pub async fn billing_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let p = prices(state);
    let row: (i64, bool, i64, i64, Option<String>) = sqlx::query_as(
        "SELECT balance_micros, auto_recharge_enabled, auto_recharge_threshold_micros, auto_recharge_amount_micros, stripe_payment_method_id FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let usage = crate::plans::usage(&state.db, account_id).await?;
    let free_left = free_emails_remaining(state, account_id).await?;
    let daily: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT to_char(day, 'YYYY-MM-DD'), kind, quantity, -amount_micros FROM ledger
         WHERE account_id = $1 AND day IS NOT NULL AND day > current_date - 31 ORDER BY day",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let ledger: Vec<(Uuid, String, String, i64, chrono::DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, kind, CASE WHEN day IS NOT NULL THEN description || ' (' || quantity || ')' ELSE description END, amount_micros, created_at
         FROM ledger WHERE account_id = $1 ORDER BY COALESCE(day::timestamptz, created_at) DESC, created_at DESC LIMIT 50",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    Ok(json!({
        "mode": if state.cfg.hosted { "hosted" } else { "self_hosted" },
        "balance": to_usd(row.0),
        "free_emails_per_month": state.cfg.free_emails_per_month,
        "emails_this_month": usage.emails_this_month,
        "free_emails_remaining": free_left,
        "sms_this_month": usage.sms_this_month,
        "prices": {
            "email_per_1000": to_usd(p.email * 1000),
            "sms_per_segment": to_usd(p.sms_segment),
            "mms_per_message": to_usd(p.mms),
            "phone_number_monthly": to_usd(p.number_monthly),
        },
        "auto_recharge": {"enabled": row.1, "threshold": to_usd(row.2), "amount": to_usd(row.3)},
        "has_payment_method": row.4.is_some(),
        "stripe_enabled": state.cfg.stripe_secret_key.is_some(),
        "usage": daily.into_iter().map(|(d, k, q, a)| json!({"date": d, "kind": k, "quantity": q, "amount": to_usd(a)})).collect::<Vec<_>>(),
        "ledger": ledger.into_iter().map(|(id, k, d, a, t)| json!({"id": id, "kind": k, "description": d, "amount": to_usd(a), "created_at": t})).collect::<Vec<_>>(),
        "limits": {"profiles": Value::Null},
        "plan": crate::subscription::plan_json(&state.db, account_id).await?,
        "plan_options": crate::subscription::options_json(prices(state).email),
        "plan_payments": crate::subscription::payments_json(&state.db, account_id).await?,
        "min_topup": MIN_TOPUP_USD,
    }))
}

// ---------- Stripe ----------

fn stripe_key(state: &AppState) -> AppResult<&str> {
    state.cfg.stripe_secret_key.as_deref().ok_or_else(|| AppError::bad("card payments aren't configured on this server"))
}

/// POST to Stripe. 4xx answers come back as `BadRequest`, network and 5xx failures as `Internal`.
/// For charges that must happen exactly once, use `stripe_charge`.
pub(crate) async fn stripe_post(state: &AppState, path: &str, form: &[(String, String)]) -> AppResult<Value> {
    let key = stripe_key(state)?;
    let req = state.http.post(format!("{}/{path}", state.cfg.stripe_api_base)).basic_auth(key, Some("")).form(form);
    let res = req.send().await.map_err(|e| AppError::Internal(e.into()))?;
    let status = res.status();
    let v: Value = res.json().await.map_err(|e| AppError::Internal(e.into()))?;
    if !status.is_success() {
        let msg = v["error"]["message"].as_str().unwrap_or("Stripe request failed").to_string();
        return Err(if status.is_client_error() { AppError::BadRequest(msg) } else { AppError::Internal(anyhow::anyhow!(msg)) });
    }
    Ok(v)
}

/// What a Stripe call that moves money came back with.
pub(crate) enum StripeOutcome {
    Ok(Value),
    /// A definite "no" (a declined card, an invalid request). Stripe's error body is included.
    Refused(Value),
    /// Unknown: network trouble, a 5xx, a 409 (the same idempotent request is still running) or a
    /// 429 (rate limited). The money may or may not have moved.
    Unknown(String),
}

/// POST with an idempotency key, sorting the answer into ok / refused / unknown.
pub(crate) async fn stripe_charge(state: &AppState, path: &str, form: &[(String, String)], idempotency_key: &str) -> AppResult<StripeOutcome> {
    let key = stripe_key(state)?;
    let res = match state
        .http
        .post(format!("{}/{path}", state.cfg.stripe_api_base))
        .basic_auth(key, Some(""))
        .header("Idempotency-Key", idempotency_key)
        .form(form)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(StripeOutcome::Unknown(e.to_string())),
    };
    let status = res.status();
    let body: Value = match res.json().await {
        Ok(v) => v,
        Err(e) => return Ok(StripeOutcome::Unknown(format!("HTTP {status}: unreadable body: {e}"))),
    };
    Ok(match status.as_u16() {
        200..=299 => StripeOutcome::Ok(body),
        409 | 429 => StripeOutcome::Unknown(format!("HTTP {status}: {}", body["error"]["message"].as_str().unwrap_or(""))),
        400..=499 => StripeOutcome::Refused(body),
        _ => StripeOutcome::Unknown(format!("HTTP {status}: {}", body["error"]["message"].as_str().unwrap_or(""))),
    })
}

pub(crate) async fn stripe_get(state: &AppState, path: &str) -> AppResult<Value> {
    let key = stripe_key(state)?;
    let res = state.http.get(format!("{}/{path}", state.cfg.stripe_api_base)).basic_auth(key, Some("")).send().await.map_err(|e| AppError::Internal(e.into()))?;
    res.json().await.map_err(|e| AppError::Internal(e.into()))
}

/// True when Stripe says a saved customer doesn't exist under the current key: one saved in test
/// mode before the server switched to live keys, or one deleted in the Dashboard.
pub(crate) fn customer_gone(v: &Value) -> bool {
    v["error"]["code"] == "resource_missing" || v["deleted"] == true
}

/// Forget a saved customer and card that no longer exist in Stripe, so the next top-up starts
/// fresh and the billing page asks for a card again.
async fn forget_stripe_customer(state: &AppState, account_id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE accounts SET stripe_customer_id = NULL, stripe_payment_method_id = NULL WHERE id = $1").bind(account_id).execute(&state.db).await?;
    Ok(())
}

pub(crate) async fn ensure_customer(state: &AppState, account_id: Uuid) -> AppResult<String> {
    let (existing, name): (Option<String>, String) = sqlx::query_as("SELECT stripe_customer_id, name FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    if let Some(c) = existing {
        if !customer_gone(&stripe_get(state, &format!("customers/{c}")).await?) {
            return Ok(c);
        }
        tracing::info!(account = %account_id, "saved Stripe customer no longer exists; creating a new one");
        forget_stripe_customer(state, account_id).await?;
    }
    let email: Option<String> = sqlx::query_scalar("SELECT u.email FROM users u JOIN memberships m ON m.user_id = u.id WHERE m.account_id = $1 ORDER BY m.created_at LIMIT 1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let mut form = vec![("name".to_string(), name), ("metadata[account_id]".to_string(), account_id.to_string())];
    if let Some(e) = email {
        form.push(("email".into(), e));
    }
    let c = stripe_post(state, "customers", &form).await?;
    let id = c["id"].as_str().unwrap_or_default().to_string();
    sqlx::query("UPDATE accounts SET stripe_customer_id = $2 WHERE id = $1").bind(account_id).bind(&id).execute(&state.db).await?;
    Ok(id)
}

/// Start a Stripe Checkout session for a credit top-up; the card is saved for auto-recharge.
pub async fn checkout(state: &AppState, account_id: Uuid, amount_usd: f64) -> AppResult<String> {
    if !state.cfg.hosted {
        return Err(AppError::bad("self-hosted installs don't use billing"));
    }
    if !(MIN_TOPUP_USD..=5000.0).contains(&amount_usd) {
        return Err(AppError::bad("top-ups are between $25 and $5,000"));
    }
    let cents = (amount_usd * 100.0).round() as i64;
    let customer = ensure_customer(state, account_id).await?;
    let base = state.cfg.public_url();
    let mut form: Vec<(String, String)> = vec![
        ("mode".into(), "payment".into()),
        ("customer".into(), customer),
        ("client_reference_id".into(), account_id.to_string()),
        ("line_items[0][quantity]".into(), "1".into()),
        ("line_items[0][price_data][currency]".into(), "usd".into()),
        ("line_items[0][price_data][unit_amount]".into(), cents.to_string()),
        ("line_items[0][price_data][product_data][name]".into(), "DumbMail credit".into()),
        ("line_items[0][price_data][product_data][description]".into(), "Prepaid credit for emails and texts".into()),
        ("payment_intent_data[setup_future_usage]".into(), "off_session".into()),
        ("payment_intent_data[metadata][account_id]".into(), account_id.to_string()),
        ("metadata[account_id]".into(), account_id.to_string()),
        ("metadata[kind]".into(), "topup".into()),
        ("success_url".into(), format!("{base}/settings/billing?topup=success")),
        ("cancel_url".into(), format!("{base}/settings/billing?topup=cancelled")),
    ];
    cards_only(&mut form);
    let s = stripe_post(state, "checkout/sessions", &form).await?;
    s["url"].as_str().map(String::from).ok_or_else(|| AppError::Internal(anyhow::anyhow!("Stripe returned no checkout url")))
}

/// Payment methods Checkout must not offer. The card saved at checkout is charged later without the
/// customer present (auto-recharge, plan renewals), which needs a card or Link: bank debits, buy now
/// pay later and wallets like Cash App can't be confirmed on the spot. (Checkout no longer takes an
/// allow-list, so this excludes the non-card methods Stripe offers US accounts;
/// `save_payment_method` also refuses anything else that slips through.)
const NON_CARD_METHODS: &[&str] = &[
    "affirm", "afterpay_clearpay", "alipay", "amazon_pay", "cashapp", "crypto", "customer_balance", "klarna", "pay_by_bank", "paypal",
    "revolut_pay", "sunbit", "us_bank_account", "wechat_pay", "zip",
];

pub(crate) fn cards_only(form: &mut Vec<(String, String)>) {
    for (i, t) in NON_CARD_METHODS.iter().enumerate() {
        form.push((format!("excluded_payment_method_types[{i}]"), (*t).into()));
    }
}

/// Verify a Stripe-Signature header (t=...,v1=...) within a 5 minute tolerance.
pub fn verify_stripe_signature(secret: &str, header: &str, body: &[u8], now: i64) -> bool {
    let mut t = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        if let Some((k, v)) = part.trim().split_once('=') {
            match k {
                "t" => t = v.parse::<i64>().ok(),
                "v1" => sigs.push(v.to_string()),
                _ => {}
            }
        }
    }
    let Some(t) = t else { return false };
    if (now - t).abs() > 300 {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());
    sigs.iter().any(|s| s.len() == expected.len() && s.bytes().zip(expected.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0)
}

/// Apply a verified Stripe event. Credits are idempotent by Stripe object id.
pub async fn handle_stripe_event(state: &AppState, ev: &Value) -> AppResult<()> {
    let obj = &ev["data"]["object"];
    match ev["type"].as_str().unwrap_or("") {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            if obj["payment_status"] != "paid" {
                return Ok(());
            }
            let Some(account_id) = obj["metadata"]["account_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
            match obj["metadata"]["kind"].as_str() {
                Some("topup") => {
                    let cents = obj["amount_total"].as_i64().unwrap_or(0);
                    let session = obj["id"].as_str().unwrap_or_default();
                    post_entry(&state.db, account_id, "topup", cents * 10_000, &format!("Added credit (${:.2})", cents as f64 / 100.0), session).await?;
                }
                Some("plan") => crate::subscription::apply_checkout(&state.db, account_id, obj).await?,
                _ => return Ok(()),
            }
            save_payment_method(state, account_id, obj).await?;
        }
        // Normally the scheduler records a renewal itself; this covers a response it never got.
        "payment_intent.succeeded" if obj["metadata"]["kind"] == "plan_renewal" => {
            if let (Some(renewal), Some(pi)) = (obj["metadata"]["renewal_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()), obj["id"].as_str()) {
                crate::subscription::renewal_succeeded(&state.db, renewal, pi).await?;
            }
        }
        "payment_intent.succeeded" if obj["metadata"]["kind"] == "auto_recharge" => {
            if let Some(account_id) = obj["metadata"]["account_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
                let cents = obj["amount_received"].as_i64().unwrap_or(0);
                post_entry(&state.db, account_id, "auto_recharge", cents * 10_000, &format!("Auto-recharge (${:.2})", cents as f64 / 100.0), obj["id"].as_str().unwrap_or_default()).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Keep the card from a paid Checkout session for auto-recharge and plan renewals.
async fn save_payment_method(state: &AppState, account_id: Uuid, session: &Value) -> AppResult<()> {
    let Some(pi) = session["payment_intent"].as_str() else { return Ok(()) };
    let intent = stripe_get(state, &format!("payment_intents/{pi}?expand[]=payment_method")).await?;
    let method = &intent["payment_method"];
    let kind = method["type"].as_str().unwrap_or_default();
    if !matches!(kind, "card" | "link") {
        tracing::info!(account = %account_id, kind, "not saving a payment method that can't be charged automatically");
        return Ok(());
    }
    if let Some(pm) = method["id"].as_str() {
        sqlx::query(
            "UPDATE accounts SET stripe_payment_method_id = $2, stripe_customer_id = COALESCE(stripe_customer_id, $3), auto_recharge_failed_at = NULL,
                    email_plan_renewal_failed_at = NULL
             WHERE id = $1",
        )
        .bind(account_id)
        .bind(pm)
        .bind(session["customer"].as_str())
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

pub async fn set_auto_recharge(state: &AppState, account_id: Uuid, enabled: bool, threshold: f64, amount: f64) -> AppResult<()> {
    if !(0.0..=1000.0).contains(&threshold) || !(MIN_TOPUP_USD..=1000.0).contains(&amount) {
        return Err(AppError::bad("threshold must be $0–1,000 and recharge amount $25–1,000"));
    }
    if enabled {
        let pm: Option<String> = sqlx::query_scalar("SELECT stripe_payment_method_id FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
        if pm.is_none() {
            return Err(AppError::bad("add credit with a card once before turning on auto-recharge"));
        }
    }
    sqlx::query("UPDATE accounts SET auto_recharge_enabled = $2, auto_recharge_threshold_micros = $3, auto_recharge_amount_micros = $4, auto_recharge_failed_at = NULL WHERE id = $1")
        .bind(account_id)
        .bind(enabled)
        .bind(to_micros(threshold))
        .bind(to_micros(amount))
        .execute(&state.db)
        .await?;
    Ok(())
}

/// Scheduler: charge saved cards for accounts below their auto-recharge threshold.
pub async fn run_auto_recharge(state: &AppState) -> anyhow::Result<()> {
    if !state.cfg.hosted || state.cfg.stripe_secret_key.is_none() {
        return Ok(());
    }
    let due: Vec<(Uuid, String, String, i64)> = sqlx::query_as(
        "SELECT id, stripe_customer_id, stripe_payment_method_id, auto_recharge_amount_micros FROM accounts
         WHERE auto_recharge_enabled AND closed_at IS NULL AND stripe_payment_method_id IS NOT NULL AND stripe_customer_id IS NOT NULL
           AND balance_micros < auto_recharge_threshold_micros
           AND (auto_recharge_failed_at IS NULL OR auto_recharge_failed_at < now() - interval '1 day')
         LIMIT 50",
    )
    .fetch_all(&state.db)
    .await?;
    for (account_id, customer, pm, amount) in due {
        if customer_gone(&stripe_get(state, &format!("customers/{customer}")).await.map_err(|e| anyhow::anyhow!(e.public_message()))?) {
            tracing::info!(account = %account_id, "auto-recharge card belongs to a Stripe customer that no longer exists; forgetting it");
            forget_stripe_customer(state, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
            continue;
        }
        let cents = amount / 10_000;
        let form: Vec<(String, String)> = vec![
            ("amount".into(), cents.to_string()),
            ("currency".into(), "usd".into()),
            ("customer".into(), customer),
            ("payment_method".into(), pm),
            ("off_session".into(), "true".into()),
            ("confirm".into(), "true".into()),
            ("description".into(), "DumbMail auto-recharge".into()),
            ("metadata[account_id]".into(), account_id.to_string()),
            ("metadata[kind]".into(), "auto_recharge".into()),
        ];
        match stripe_post(state, "payment_intents", &form).await {
            Ok(pi) if pi["status"] == "succeeded" => {
                post_entry(&state.db, account_id, "auto_recharge", cents * 10_000, &format!("Auto-recharge (${:.2})", cents as f64 / 100.0), pi["id"].as_str().unwrap_or_default())
                    .await
                    .map_err(|e| anyhow::anyhow!(e.public_message()))?;
            }
            other => {
                tracing::warn!(account = %account_id, result = ?other.err().map(|e| e.public_message()), "auto-recharge did not succeed");
                sqlx::query("UPDATE accounts SET auto_recharge_failed_at = now() WHERE id = $1").bind(account_id).execute(&state.db).await?;
            }
        }
    }
    Ok(())
}

/// How long a number stays past due before it's released.
pub fn number_grace() -> chrono::Duration {
    chrono::Duration::days(7)
}

/// Scheduler: monthly phone number rent. Rent is charged only when the balance covers it, so it
/// never pushes a balance negative. An unpaid number is marked past due (auto-recharge gets a
/// chance to top up) and released after the grace period. The first month is paid at claim, so
/// the operator's provider cost for a number never exceeds what the customer paid for it.
pub async fn charge_numbers(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<(Uuid, Uuid, String, i64, Option<chrono::DateTime<Utc>>, chrono::DateTime<Utc>)> = sqlx::query_as(
        // Only rows that need action: newly due, past due but now covered by the balance, or past
        // the grace period (failed releases retry hourly). Past-due rows still inside the grace
        // period are skipped so they can't crowd newly due numbers out of the batch.
        "SELECT p.id, p.account_id, p.phone_number, p.monthly_price_micros, p.past_due_since, p.next_charge_at
         FROM phone_numbers p JOIN accounts a ON a.id = p.account_id
         WHERE p.status <> 'released' AND p.monthly_price_micros > 0 AND p.next_charge_at <= now() AND a.closed_at IS NULL
           AND (p.past_due_since IS NULL
                OR a.balance_micros >= p.monthly_price_micros
                OR (p.past_due_since <= now() - make_interval(days => $1)
                    AND (p.release_failed_at IS NULL OR p.release_failed_at < now() - interval '1 hour')))
         ORDER BY p.next_charge_at LIMIT 200",
    )
    .bind(number_grace().num_days() as i32)
    .fetch_all(&state.db)
    .await?;
    for (id, account_id, number, price, past_due_since, due_at) in due {
        if !state.cfg.hosted || balance(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))? >= price {
            // Under the account's row lock, taken before charging: an account closed a moment ago
            // pays nothing (closing holds the same row).
            let mut tx = state.db.begin().await?;
            let closed: Option<bool> = sqlx::query_scalar("SELECT closed_at IS NOT NULL FROM accounts WHERE id = $1 FOR UPDATE").bind(account_id).fetch_optional(&mut *tx).await?;
            if closed != Some(false) {
                continue;
            }
            if state.cfg.hosted {
                // Keyed by the billing period (not today's date), so a late payment can't make the
                // next period's charge look like a duplicate.
                let reference = format!("{id}:{}", due_at.format("%Y-%m-%d"));
                post_entry_tx(&mut tx, account_id, "number", -price, &format!("Phone number {number} (monthly)"), &reference).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
            }
            sqlx::query("UPDATE phone_numbers SET next_charge_at = next_charge_at + interval '1 month', past_due_since = NULL WHERE id = $1").bind(id).execute(&mut *tx).await?;
            tx.commit().await?;
            continue;
        }
        match past_due_since {
            None => {
                sqlx::query("UPDATE phone_numbers SET past_due_since = now() WHERE id = $1").bind(id).execute(&state.db).await?;
            }
            Some(since) if Utc::now() - since >= number_grace() => {
                tracing::warn!(%account_id, number, "releasing phone number: monthly rent unpaid");
                if let Err(e) = crate::phone::release(state, account_id, id).await {
                    tracing::error!(%account_id, number, error = %e.public_message(), "couldn't release past-due number; retrying in an hour");
                    sqlx::query("UPDATE phone_numbers SET release_failed_at = now() WHERE id = $1").bind(id).execute(&state.db).await?;
                }
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stripe_customer_gone() {
        assert!(customer_gone(&serde_json::json!({"error": {"code": "resource_missing", "message": "No such customer: 'cus_test'"}})));
        assert!(customer_gone(&serde_json::json!({"id": "cus_1", "deleted": true})));
        assert!(!customer_gone(&serde_json::json!({"id": "cus_1", "object": "customer"})));
        assert!(!customer_gone(&serde_json::json!({"error": {"code": "rate_limit"}})));
    }

    #[test]
    fn stripe_signatures() {
        let body = br#"{"id":"evt_1"}"#;
        let t = 1_700_000_000i64;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_test").unwrap();
        mac.update(format!("{t}.").as_bytes());
        mac.update(body);
        let sig = hex::encode(mac.finalize().into_bytes());
        let header = format!("t={t},v1={sig}");
        assert!(verify_stripe_signature("whsec_test", &header, body, t + 10));
        assert!(!verify_stripe_signature("whsec_test", &header, body, t + 1000), "too old");
        assert!(!verify_stripe_signature("whsec_other", &header, body, t));
        assert!(!verify_stripe_signature("whsec_test", &header, b"{}", t));
    }

    #[test]
    fn money() {
        assert_eq!(to_micros(0.20 / 1000.0), 200);
        assert_eq!(to_micros(0.0099), 9900);
        assert_eq!(to_usd(2_200_000), 2.2);
    }
}
