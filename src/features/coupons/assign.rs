//! Giving out codes: one per person per coupon (or one per message), atomically, exactly once.
//!
//! - A code is assigned in the database before the message renders: the pool row is picked with
//!   `FOR UPDATE SKIP LOCKED`, and a unique index on (coupon, assign_key) means two sends racing for
//!   the same person end up with the same code, never two.
//! - Consent, suppression and the account pause are checked before a code is touched. A message
//!   that is still not sent after that (no credit, the daily limit, the provider busy, an opt-out
//!   caught at the last moment) leaves its code undelivered, and [`release_unsent`] puts it back
//!   once the message is known not to have gone out. A message the provider may have taken keeps
//!   its code: the person keeps it and gets the same code next time.
//! - An empty pool holds the message (campaigns wait and retry, flows wait and retry) instead of
//!   sending a blank code, and the account is alerted.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sqlx::{Acquire, PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::store::{self, Coupon};
use super::template::{self, CTX_KEY};
use crate::error::{AppError, AppResult};
use crate::models::{Account, FormRow, Profile};
use crate::state::AppState;

/// How long a send waits before trying again when a coupon has no codes left.
pub const HOLD_MINUTES: i64 = 10;
/// An undelivered code whose message didn't go out returns to the pool after this long.
const RELEASE_AFTER_MINUTES: i32 = 15;
/// Most codes sign-up forms hand to one visitor (IP address) in an hour, across all accounts.
pub const FORM_CODES_PER_IP_HOUR: i64 = 3;

/// A code given to someone.
#[derive(Debug, Clone)]
pub struct Given {
    pub id: i64,
    pub code: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// How a code is being given out.
pub enum Via<'a> {
    /// For a message about to be sent (not yet delivered).
    Message(Uuid),
    /// Shown on a sign-up form's thank-you screen or confirmation page (delivered at once).
    Form { form_id: Uuid, ip_hash: Option<&'a str> },
}

pub type StoreCodeFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>>;

/// Creates a real discount code in the store for one person, for when the pool is empty. A
/// store integration (Shopify) passes one to [`give_code`]; it's called with no database lock held.
pub type CreateInStore<'a> = &'a (dyn Fn(&'a Coupon, Uuid) -> StoreCodeFuture<'a> + Send + Sync);

fn key_for(c: &Coupon, profile_id: Uuid, via: &Via<'_>) -> String {
    match (c.new_code_each_time, via) {
        (true, Via::Message(m)) => format!("{profile_id}:{m}"),
        (true, Via::Form { .. }) => format!("{profile_id}:form"),
        (false, _) => profile_id.to_string(),
    }
}

fn expiry_for(c: &Coupon) -> Option<DateTime<Utc>> {
    c.expires_at.or_else(|| c.expires_after_days.map(|d| Utc::now() + chrono::Duration::days(i64::from(d))))
}

fn expired(c: &Coupon) -> AppResult<()> {
    match c.expires_at {
        Some(at) if at <= Utc::now() => Err(AppError::Conflict(format!("coupon {:?} expired on {}; change its expiry or use another coupon", c.name, at.format("%Y-%m-%d")))),
        _ => Ok(()),
    }
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505"))
}

/// The person's existing code for this key, marked as being prepared again.
async fn reuse(tx: &mut Transaction<'_, Postgres>, c: &Coupon, key: &str, message_id: Option<Uuid>, delivered: bool) -> AppResult<Option<Given>> {
    Ok(sqlx::query_as::<_, (i64, String, Option<DateTime<Utc>>)>(
        "UPDATE coupon_codes SET claimed_at = now(),
                message_id = CASE WHEN delivered_at IS NULL THEN COALESCE($3, message_id) ELSE message_id END,
                delivered_at = CASE WHEN $4 THEN COALESCE(delivered_at, now()) ELSE delivered_at END
         WHERE coupon_id = $1 AND assign_key = $2 RETURNING id, code, expires_at",
    )
    .bind(c.id)
    .bind(key)
    .bind(message_id)
    .bind(delivered)
    .fetch_optional(&mut **tx)
    .await?
    .map(|(id, code, expires_at)| Given { id, code, expires_at }))
}

/// Assign inside `tx`: the person's code if they have one, else the next free code. None = pool empty.
async fn give_in(tx: &mut Transaction<'_, Postgres>, c: &Coupon, profile_id: Uuid, via: &Via<'_>) -> AppResult<Option<Given>> {
    let key = key_for(c, profile_id, via);
    let (message_id, form_id, ip_hash, delivered, how) = match via {
        Via::Message(m) => (Some(*m), None, None, false, "message"),
        Via::Form { form_id, ip_hash } => (None, Some(*form_id), *ip_hash, true, "form"),
    };
    for _ in 0..3 {
        if let Some(g) = reuse(tx, c, &key, message_id, delivered).await? {
            return Ok(Some(g));
        }
        // A savepoint: if another send just gave this person a code, take theirs instead.
        let mut sp = tx.begin().await?;
        let picked = sqlx::query_as::<_, (i64, String, Option<DateTime<Utc>>)>(
            "UPDATE coupon_codes SET status = 'assigned', profile_id = $2, assign_key = $3, assigned_via = $4, message_id = $5, form_id = $6, ip_hash = $7,
                    assigned_at = now(), claimed_at = now(), delivered_at = CASE WHEN $8 THEN now() END, expires_at = $9
             WHERE id = (SELECT id FROM coupon_codes WHERE coupon_id = $1 AND status = 'available' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED)
               AND status = 'available'
             RETURNING id, code, expires_at",
        )
        .bind(c.id)
        .bind(profile_id)
        .bind(&key)
        .bind(how)
        .bind(message_id)
        .bind(form_id)
        .bind(ip_hash)
        .bind(delivered)
        .bind(expiry_for(c))
        .fetch_optional(&mut *sp)
        .await;
        match picked {
            Ok(row) => {
                sp.commit().await?;
                return Ok(row.map(|(id, code, expires_at)| Given { id, code, expires_at }));
            }
            Err(e) if is_unique_violation(&e) => sp.rollback().await?,
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::Conflict("couldn't give out a code; try again".into()))
}

/// Give `profile_id` a code from coupon `c`: the same one they already have (unless the coupon gives
/// a new code each time), otherwise the next free one. `Ok(None)` means the pool is empty (and `store`,
/// when given, couldn't create one either). Errors when the coupon has expired.
pub async fn give_code<'a>(db: &PgPool, c: &'a Coupon, profile_id: Uuid, via: Via<'_>, store: Option<CreateInStore<'a>>) -> AppResult<Option<Given>> {
    expired(c)?;
    let mut tx = db.begin().await?;
    let got = give_in(&mut tx, c, profile_id, &via).await?;
    tx.commit().await?;
    if got.is_some() {
        return Ok(got);
    }
    let Some(make) = store else { return Ok(None) };
    // No lock is held while the store is asked for a code.
    let code = make(c, profile_id).await.map_err(|e| AppError::Internal(e.context("the store couldn't create a discount code")))?;
    if !store::valid_code(&code) {
        return Err(AppError::Internal(anyhow::anyhow!("the store returned an unusable discount code")));
    }
    let key = key_for(c, profile_id, &via);
    let (message_id, form_id, delivered, how) = match via {
        Via::Message(m) => (Some(m), None, false, "message"),
        Via::Form { form_id, .. } => (None, Some(form_id), true, "form"),
    };
    let inserted: Result<Option<(i64, Option<DateTime<Utc>>)>, sqlx::Error> = sqlx::query_as(
        "INSERT INTO coupon_codes (account_id, coupon_id, code, code_norm, status, profile_id, assign_key, assigned_via, message_id, form_id, assigned_at, claimed_at, delivered_at, expires_at)
         VALUES ($1, $2, $3, upper($3), 'assigned', $4, $5, $6, $7, $8, now(), now(), CASE WHEN $9 THEN now() END, $10)
         ON CONFLICT (coupon_id, code_norm) DO NOTHING RETURNING id, expires_at",
    )
    .bind(c.account_id)
    .bind(c.id)
    .bind(&code)
    .bind(profile_id)
    .bind(&key)
    .bind(how)
    .bind(message_id)
    .bind(form_id)
    .bind(delivered)
    .bind(expiry_for(c))
    .fetch_optional(db)
    .await;
    match inserted {
        Ok(Some((id, expires_at))) => {
            sqlx::query("UPDATE coupons SET codes_total = codes_total + 1 WHERE id = $1").bind(c.id).execute(db).await?;
            Ok(Some(Given { id, code, expires_at }))
        }
        // Someone else gave this person a code meanwhile: keep the new store code in the pool.
        Err(e) if is_unique_violation(&e) => {
            let n = sqlx::query("INSERT INTO coupon_codes (account_id, coupon_id, code, code_norm) VALUES ($1, $2, $3, upper($3)) ON CONFLICT (coupon_id, code_norm) DO NOTHING")
                .bind(c.account_id)
                .bind(c.id)
                .bind(&code)
                .execute(db)
                .await?
                .rows_affected();
            sqlx::query("UPDATE coupons SET codes_total = codes_total + $2 WHERE id = $1").bind(c.id).bind(n as i64).execute(db).await?;
            let mut tx = db.begin().await?;
            let g = reuse(&mut tx, c, &key, message_id, delivered).await?;
            tx.commit().await?;
            Ok(g)
        }
        Ok(None) => Err(AppError::Internal(anyhow::anyhow!("the store returned a discount code this coupon already has"))),
        Err(e) => Err(e.into()),
    }
}

/// The codes one message carries.
#[derive(Default)]
pub struct SendCoupons {
    entries: Vec<(String, Given)>,
    tz: Option<chrono_tz::Tz>,
}

pub enum Prepared {
    Ready(SendCoupons),
    /// A coupon is out of codes: try this message again at that time.
    Hold(DateTime<Utc>),
}

impl SendCoupons {
    /// Put the codes where the template functions read them. Without this they show a sample.
    pub fn fill(&self, ctx: &mut Value) {
        if self.entries.is_empty() {
            return;
        }
        let tz = self.tz.unwrap_or(chrono_tz::America::Los_Angeles);
        let mut m = Map::new();
        for (name, g) in &self.entries {
            let expires = g.expires_at.map(|t| t.with_timezone(&tz).format("%B %-d, %Y").to_string()).unwrap_or_default();
            m.insert(name.clone(), json!({"code": g.code, "expires": expires}));
        }
        if let Some(o) = ctx.as_object_mut() {
            o.insert(CTX_KEY.into(), Value::Object(m));
        }
    }

    /// The provider accepted the message: these codes now belong to the person for good.
    pub async fn delivered(&self, db: &PgPool) {
        if self.entries.is_empty() {
            return;
        }
        let ids: Vec<i64> = self.entries.iter().map(|(_, g)| g.id).collect();
        if let Err(e) = sqlx::query("UPDATE coupon_codes SET delivered_at = COALESCE(delivered_at, now()) WHERE id = ANY($1)").bind(&ids).execute(db).await {
            // The message row says it was sent, which also keeps these codes from being released.
            tracing::error!(error = %e, "couldn't mark coupon codes delivered");
        }
    }
}

/// Before a real message renders: give the person a code from every coupon its content uses.
/// Test sends get nothing (the template shows a sample code and no code is used up).
#[allow(clippy::too_many_arguments)]
pub async fn prepare_send(
    state: &AppState,
    account: &Account,
    profile: &Profile,
    content: &Value,
    campaign_id: Option<Uuid>,
    flow_id: Option<Uuid>,
    message_id: Uuid,
    is_test: bool,
) -> AppResult<Prepared> {
    let names = template::coupon_names(content);
    if names.is_empty() || is_test {
        return Ok(Prepared::Ready(SendCoupons::default()));
    }
    if names.len() > template::MAX_PER_MESSAGE {
        return Err(AppError::bad(format!("a message can use at most {} coupons", template::MAX_PER_MESSAGE)));
    }
    let mut out = SendCoupons { entries: Vec::new(), tz: account.timezone.parse().ok() };
    for name in names {
        let c = match store::by_name(&state.db, account.id, &name).await? {
            Some(c) if template::valid_name(&c.name) => c,
            _ => return Err(AppError::bad(format!("there's no coupon named {name:?}; create it under Content → Coupons or fix the message"))),
        };
        match give_code(&state.db, &c, profile.id, Via::Message(message_id), None).await? {
            Some(g) => out.entries.push((name, g)),
            None => {
                hold(state, &c, campaign_id, flow_id).await?;
                return Ok(Prepared::Hold(Utc::now() + chrono::Duration::minutes(HOLD_MINUTES)));
            }
        }
    }
    Ok(Prepared::Ready(out))
}

/// Record that `c` ran out: alert once, show the reason on the waiting campaign, remember who waits.
async fn hold(state: &AppState, c: &Coupon, campaign_id: Option<Uuid>, flow_id: Option<Uuid>) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    mark_out_of_codes(&mut tx, c).await?;
    let subject = match (campaign_id, flow_id) {
        (Some(id), _) => format!("campaign:{id}"),
        (None, Some(id)) => format!("flow:{id}"),
        (None, None) => {
            tx.commit().await?;
            return Ok(());
        }
    };
    sqlx::query(
        "INSERT INTO coupon_holds (coupon_id, subject, account_id, campaign_id, flow_id) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (coupon_id, subject) DO UPDATE SET last_at = now()",
    )
    .bind(c.id)
    .bind(&subject)
    .bind(c.account_id)
    .bind(campaign_id)
    .bind(flow_id)
    .execute(&mut *tx)
    .await?;
    if let Some(id) = campaign_id {
        let msg = format!("Paused: coupon {:?} has no codes left. Add codes under Content → Coupons and the campaign carries on within {HOLD_MINUTES} minutes.", c.name);
        sqlx::query("UPDATE campaigns SET error = $2, updated_at = now() WHERE id = $1 AND status = 'sending' AND error IS DISTINCT FROM $2")
            .bind(id)
            .bind(&msg)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The first time a coupon runs out, queue an alert (in the same transaction, so it's sent once).
async fn mark_out_of_codes(tx: &mut Transaction<'_, Postgres>, c: &Coupon) -> AppResult<()> {
    let first: Option<Uuid> = sqlx::query_scalar("UPDATE coupons SET out_of_codes_at = now() WHERE id = $1 AND out_of_codes_at IS NULL RETURNING id")
        .bind(c.id)
        .fetch_optional(&mut **tx)
        .await?;
    if first.is_some() {
        crate::jobs::enqueue(&mut **tx, "coupons_alert", json!({"coupon_id": c.id, "kind": "empty"}), None).await?;
    }
    Ok(())
}

/// A sign-up form that hands out codes from a coupon (`config.coupon`): only after a real sign-up,
/// only to a person this sign-up created (anyone can type someone else's address, and their code must
/// not be shown to a stranger), one per person, within the coupon's hourly limit and the per-visitor
/// limit. The form's fixed `discount_code` isn't used then. Never fails the sign-up.
pub async fn form_sign_up(state: &AppState, account: &Account, form: &FormRow, profile_id: Uuid, created: bool, ip: Option<&str>, out: &mut Value) {
    let Some(name) = form.config.get("coupon").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else { return };
    if let Some(o) = out.as_object_mut() {
        o.remove("discount_code");
    }
    if !created {
        return;
    }
    match form_code(state, account.id, form.id, name, profile_id, ip).await {
        Ok(Some(code)) => out["discount_code"] = json!(code),
        Ok(None) => {}
        Err(e) => tracing::warn!(account = %account.id, form = %form.id, error = %e.public_message(), "couldn't give a sign-up form coupon code"),
    }
}

/// The coupon's hourly form limit always applies; the per-visitor limit applies when the visitor's address is known.
async fn form_code(state: &AppState, account_id: Uuid, form_id: Uuid, name: &str, profile_id: Uuid, ip: Option<&str>) -> AppResult<Option<String>> {
    let Some(c) = store::by_name(&state.db, account_id, name).await? else {
        tracing::warn!(account = %account_id, form = %form_id, "the form's coupon doesn't exist; no code given");
        return Ok(None);
    };
    expired(&c)?;
    let ip_hash = ip.filter(|s| !s.is_empty()).map(|s| crate::auth::sha256_hex(&format!("coupon-ip:{s}")));
    let mut tx = state.db.begin().await?;
    {
        // Checked and recorded under locks on the coupon and the visitor, always in that order, so
        // simultaneous sign-ups can't get past a limit together.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("coupon-form:{}", c.id)).execute(&mut *tx).await?;
        if let Some(h) = &ip_hash {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("coupon-ip:{h}")).execute(&mut *tx).await?;
        }
        let has: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM coupon_codes WHERE coupon_id = $1 AND assign_key = $2)")
            .bind(c.id)
            .bind(key_for(&c, profile_id, &Via::Form { form_id, ip_hash: None }))
            .fetch_one(&mut *tx)
            .await?;
        if !has {
            let last_hour: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE coupon_id = $1 AND assigned_via = 'form' AND assigned_at > now() - interval '1 hour'")
                .bind(c.id)
                .fetch_one(&mut *tx)
                .await?;
            if last_hour >= i64::from(c.form_codes_per_hour) {
                tracing::warn!(account = %account_id, coupon = %c.id, "sign-up form code limit for the hour reached; no code given");
                return Ok(None);
            }
            if let Some(h) = &ip_hash {
                let from_ip: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE ip_hash = $1 AND assigned_at > now() - interval '1 hour'")
                    .bind(h)
                    .fetch_one(&mut *tx)
                    .await?;
                if from_ip >= FORM_CODES_PER_IP_HOUR {
                    tracing::warn!(account = %account_id, coupon = %c.id, "per-visitor sign-up form code limit reached; no code given");
                    return Ok(None);
                }
            }
        }
    }
    let got = give_in(&mut tx, &c, profile_id, &Via::Form { form_id, ip_hash: ip_hash.as_deref() }).await?;
    if got.is_none() {
        mark_out_of_codes(&mut tx, &c).await?;
    }
    tx.commit().await?;
    Ok(got.map(|g| g.code))
}

/// A double opt-in sign-up was confirmed (from the link in its email): the code the form hands out,
/// shown on the confirmation page. The link reached the address, so no per-visitor limit applies;
/// the coupon's hourly form limit does (past it, a later visit to the link shows the code).
pub async fn confirmation_code(state: &AppState, confirmation_id: Uuid) -> Option<String> {
    let row: Option<(Uuid, Uuid, Uuid, Option<String>)> = sqlx::query_as(
        "SELECT f.account_id, f.id, o.profile_id, f.config->>'coupon' FROM optin_confirmations o JOIN forms f ON f.id = o.form_id
         WHERE o.id = $1 AND o.confirmed_at IS NOT NULL",
    )
    .bind(confirmation_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let (account_id, form_id, profile_id, name) = row?;
    let name = name.filter(|n| !n.trim().is_empty())?;
    match form_code(state, account_id, form_id, name.trim(), profile_id, None).await {
        Ok(code) => code,
        Err(e) => {
            tracing::warn!(account = %account_id, form = %form_id, error = %e.public_message(), "couldn't give a confirmed sign-up its coupon code");
            None
        }
    }
}

/// Scheduler: codes prepared for a message that never went out go back to the pool. Only when the
/// message is known not to have been sent: it was skipped, or there's no message row at all (a flow
/// send that stopped before creating one, or whose provider was busy). Queued, sending, sent and
/// failed messages keep their code.
pub async fn release_unsent(db: &PgPool) -> anyhow::Result<u64> {
    Ok(sqlx::query(
        "UPDATE coupon_codes cc SET status = 'available', profile_id = NULL, assign_key = NULL, assigned_via = NULL, message_id = NULL,
                assigned_at = NULL, claimed_at = NULL, expires_at = NULL
         WHERE cc.id IN (
             SELECT x.id FROM coupon_codes x
             WHERE x.status = 'assigned' AND x.delivered_at IS NULL AND x.assigned_via = 'message'
               AND x.claimed_at < now() - make_interval(mins => $1)
               AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = x.message_id AND m.status <> 'skipped')
             ORDER BY x.claimed_at LIMIT 1000 FOR UPDATE SKIP LOCKED)
           AND cc.status = 'assigned' AND cc.delivered_at IS NULL AND cc.claimed_at < now() - make_interval(mins => $1)",
    )
    .bind(RELEASE_AFTER_MINUTES)
    .execute(db)
    .await?
    .rows_affected())
}

/// Scheduler: alert once when a coupon with a low-stock threshold drops below it.
pub async fn check_low_stock(db: &PgPool) -> anyhow::Result<()> {
    let due: Vec<(Uuid, i32)> = sqlx::query_as(
        "UPDATE coupons SET low_stock_checked_at = now() WHERE id IN (
             SELECT id FROM coupons WHERE low_stock_threshold IS NOT NULL AND deleted_at IS NULL AND low_stock_alerted_at IS NULL AND codes_total > 0
             ORDER BY low_stock_checked_at NULLS FIRST LIMIT 100 FOR UPDATE SKIP LOCKED)
         RETURNING id, low_stock_threshold",
    )
    .fetch_all(db)
    .await?;
    for (id, threshold) in due {
        // Counting stops at the threshold, so a big pool is cheap to check.
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM coupon_codes WHERE coupon_id = $1 AND status = 'available' LIMIT $2) x")
            .bind(id)
            .bind(i64::from(threshold))
            .fetch_one(db)
            .await?;
        if left < i64::from(threshold) {
            let mut tx = db.begin().await?;
            let first: Option<Uuid> = sqlx::query_scalar("UPDATE coupons SET low_stock_alerted_at = now() WHERE id = $1 AND low_stock_alerted_at IS NULL RETURNING id")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            if first.is_some() {
                crate::jobs::enqueue(&mut *tx, "coupons_alert", json!({"coupon_id": id, "kind": "low", "left": left}), None).await?;
            }
            tx.commit().await?;
        }
    }
    Ok(())
}

/// Job: email the account's owner that a coupon is low or out of codes. The dashboard shows the
/// same alert on the Coupons page.
pub async fn send_alert(state: &AppState, coupon_id: Uuid, kind: &str, left: Option<i64>) -> anyhow::Result<()> {
    let Some((account_id, name)): Option<(Uuid, String)> = sqlx::query_as("SELECT account_id, name FROM coupons WHERE id = $1 AND deleted_at IS NULL")
        .bind(coupon_id)
        .fetch_optional(&state.db)
        .await?
    else {
        return Ok(());
    };
    let Some(from) = state.cfg.support_email.as_deref().filter(|s| s.contains('@')) else {
        tracing::warn!(account = %account_id, "no SUPPORT_EMAIL set; coupon alert only shown in the dashboard");
        return Ok(());
    };
    let owner: Option<String> = sqlx::query_scalar("SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 ORDER BY m.created_at LIMIT 1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
    let Some(to) = owner else { return Ok(()) };
    let link = format!("{}/content/coupons/{coupon_id}", state.cfg.public_url());
    let (subject, body) = if kind == "empty" {
        (
            format!("Coupon {name} is out of codes"),
            format!(
                "Your coupon \"{name}\" has no codes left.\n\nCampaigns that use it are paused and flow messages are waiting. Sign-up forms that hand out its codes show no code until you add more.\n\nAdd codes here and everything carries on within {HOLD_MINUTES} minutes:\n{link}\n"
            ),
        )
    } else {
        (
            format!("Coupon {name} is running low"),
            format!("Your coupon \"{name}\" has {} codes left, below the alert level you set.\n\nAdd codes before it runs out:\n{link}\n", left.unwrap_or(0)),
        )
    };
    let html = format!(
        "<div style=\"font-family:system-ui,sans-serif;font-size:15px;line-height:1.5;white-space:pre-wrap\">{}</div>",
        body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    );
    let email = crate::providers::email::OutgoingEmail {
        message_id: Uuid::now_v7(),
        to,
        from_name: "DumbMail".into(),
        from_email: from.to_string(),
        reply_to: Some(from.to_string()),
        subject,
        html,
        text: body,
        unsubscribe_url: None,
    };
    if let Err(e) = state.email.send(&email).await {
        tracing::error!(account = %account_id, error = %format!("{e:#}"), "couldn't email a coupon alert");
    }
    Ok(())
}
