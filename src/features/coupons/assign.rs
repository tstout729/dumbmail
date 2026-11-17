//! Giving out codes: one per person per coupon (or one per send), atomically, and for good.
//!
//! - A send claims its codes inside `sending::send`, after the checks that can skip or defer it
//!   before rendering (consent, suppression, quiet hours, the account pause) and just before it
//!   renders, so a message that is only planned or waiting holds no code.
//! - Every code a message needs is claimed in one transaction, coupons in id order (so two messages
//!   can't deadlock): if any pool is empty nothing is claimed and the message is held.
//! - A code given to a person stays theirs, whether or not that message went out: if a later check
//!   skips or defers the message, or the provider fails, they get the same code next time. Codes are
//!   never returned to the pool, so two people can never hold the same one.
//! - "New code each time" coupons key a code to the send: a campaign's message, or a flow run's step,
//!   so a retry of the same send gets the same code.
//! - An empty pool holds the message (campaigns and flows wait and retry) and alerts the account's
//!   owner once per empty spell; the alert re-arms when codes are added.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sqlx::{Acquire, PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::store::{self, Coupon};
use super::template::{self, CTX_KEY};
use crate::error::{AppError, AppResult};
use crate::models::{Account, FormRow};
use crate::sending::SendSpec;
use crate::state::AppState;

/// How long a send waits before trying again when a coupon has no codes left.
pub const HOLD_MINUTES: i64 = 10;
/// How long a send waits when every free code was being claimed by other sends at that moment.
const BUSY_SECONDS: i64 = 30;
/// Most codes sign-up forms hand to one visitor (IP address) in an hour, across all accounts.
/// Sign-ups with no usable address share one such allowance.
pub const FORM_CODES_PER_IP_HOUR: i64 = 3;
/// How long after confirming the confirmation page still shows the code.
const CONFIRM_SHOW_HOURS: i32 = 24;

/// A code given to someone.
#[derive(Debug, Clone)]
pub struct Given {
    pub id: i64,
    pub code: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// How a code is being given out.
pub enum Via<'a> {
    /// For a message about to be sent. `send` names this send (stable across its retries); it keys
    /// the code when the coupon gives a new code each time.
    Message { message_id: Uuid, send: &'a str },
    /// Shown on a sign-up form's thank-you screen or confirmation page (delivered at once).
    Form { form_id: Uuid, ip_hash: Option<&'a str> },
}

pub type StoreCodeFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>>;

/// Creates a real discount code in the store for one person, for when the pool is empty. A
/// store integration (Shopify) passes one to [`give_code`]; it's called with no database lock held.
pub type CreateInStore<'a> = &'a (dyn Fn(&'a Coupon, Uuid) -> StoreCodeFuture<'a> + Send + Sync);

fn send_key(c: &Coupon, via: &Via<'_>) -> Option<String> {
    if !c.new_code_each_time {
        return None;
    }
    Some(match via {
        Via::Message { send, .. } => (*send).to_string(),
        Via::Form { form_id, .. } => format!("form:{form_id}"),
    })
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

enum One {
    Got(Given),
    /// No codes left.
    Empty,
    /// Every free code is being claimed by other sends right now.
    Busy,
}

/// The person's existing code for this coupon (and send). A code not yet sent gets its "N days
/// after it's given" expiry restarted, so the clock starts when it really goes out.
async fn existing(tx: &mut Transaction<'_, Postgres>, c: &Coupon, profile_id: Uuid, key: Option<&str>) -> AppResult<Option<Given>> {
    let row: Option<(i64, String, Option<DateTime<Utc>>, bool)> = match key {
        None => sqlx::query_as("SELECT id, code, expires_at, delivered_at IS NOT NULL FROM coupon_codes WHERE coupon_id = $1 AND profile_id = $2 AND send_key IS NULL")
            .bind(c.id)
            .bind(profile_id)
            .fetch_optional(&mut **tx)
            .await?,
        Some(k) => sqlx::query_as("SELECT id, code, expires_at, delivered_at IS NOT NULL FROM coupon_codes WHERE coupon_id = $1 AND profile_id = $2 AND send_key = $3")
            .bind(c.id)
            .bind(profile_id)
            .bind(k)
            .fetch_optional(&mut **tx)
            .await?,
    };
    let Some((id, code, mut expires_at, delivered)) = row else { return Ok(None) };
    if let (Some(days), false) = (c.expires_after_days, delivered) {
        expires_at = sqlx::query_scalar("UPDATE coupon_codes SET expires_at = now() + make_interval(days => $2) WHERE id = $1 RETURNING expires_at")
            .bind(id)
            .bind(days)
            .fetch_one(&mut **tx)
            .await?;
    }
    Ok(Some(Given { id, code, expires_at }))
}

/// Claim inside `tx`: the person's code if they have one, else the next free code.
async fn claim_in(tx: &mut Transaction<'_, Postgres>, c: &Coupon, profile_id: Uuid, via: &Via<'_>) -> AppResult<One> {
    let key = send_key(c, via);
    let (message_id, form_id, ip_hash, delivered, how) = match via {
        Via::Message { message_id, .. } => (Some(*message_id), None, None, false, "message"),
        Via::Form { form_id, ip_hash } => (None, Some(*form_id), *ip_hash, true, "form"),
    };
    for attempt in 0..5u64 {
        if let Some(g) = existing(tx, c, profile_id, key.as_deref()).await? {
            return Ok(One::Got(g));
        }
        // A savepoint: if another send just gave this person a code, take theirs instead.
        let mut sp = tx.begin().await?;
        let picked = sqlx::query_as::<_, (i64, String, Option<DateTime<Utc>>)>(
            "UPDATE coupon_codes SET status = 'assigned', profile_id = $2, send_key = $3, assigned_via = $4, message_id = $5, form_id = $6, ip_hash = $7,
                    assigned_at = now(), delivered_at = CASE WHEN $8 THEN now() END, expires_at = $9
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
            Ok(Some((id, code, expires_at))) => {
                sp.commit().await?;
                return Ok(One::Got(Given { id, code, expires_at }));
            }
            Ok(None) => {
                sp.commit().await?;
                // Nothing free to lock: really empty, or every free code is locked by another send
                // claiming at this moment (which may still roll back). Only the first is "empty".
                let any: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM coupon_codes WHERE coupon_id = $1 AND status = 'available')")
                    .bind(c.id)
                    .fetch_one(&mut **tx)
                    .await?;
                if !any {
                    return Ok(One::Empty);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10 * (attempt + 1))).await;
            }
            Err(e) if is_unique_violation(&e) => sp.rollback().await?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(One::Busy)
}

enum Claim {
    Got(Vec<Given>),
    /// This coupon (an index into the list) is out of codes; nothing was claimed.
    Empty(usize),
    Busy,
}

/// Claim a code from every coupon in `coupons` (sorted by id) in one transaction: all or none.
async fn claim_all(db: &PgPool, coupons: &[Coupon], profile_id: Uuid, via: &Via<'_>) -> AppResult<Claim> {
    let mut tx = db.begin().await?;
    let mut out = Vec::with_capacity(coupons.len());
    for (i, c) in coupons.iter().enumerate() {
        match claim_in(&mut tx, c, profile_id, via).await? {
            One::Got(g) => out.push(g),
            One::Empty => {
                tx.rollback().await?;
                return Ok(Claim::Empty(i));
            }
            One::Busy => {
                tx.rollback().await?;
                return Ok(Claim::Busy);
            }
        }
    }
    tx.commit().await?;
    Ok(Claim::Got(out))
}

/// Give `profile_id` a code from coupon `c`: the same one they already have (unless the coupon gives
/// a new code each time), otherwise the next free one. `Ok(None)` means the pool is empty (and `store`,
/// when given, couldn't create one either). Errors when the coupon has expired.
pub async fn give_code<'a>(db: &PgPool, c: &'a Coupon, profile_id: Uuid, via: Via<'_>, store: Option<CreateInStore<'a>>) -> AppResult<Option<Given>> {
    expired(c)?;
    match claim_all(db, std::slice::from_ref(c), profile_id, &via).await? {
        Claim::Got(mut g) => return Ok(g.pop()),
        Claim::Busy => return Err(AppError::Conflict("every free code is being given out right now; try again".into())),
        Claim::Empty(_) => {}
    }
    let Some(make) = store else { return Ok(None) };
    // No lock is held while the store is asked for a code.
    let code = make(c, profile_id).await.map_err(|e| AppError::Internal(e.context("the store couldn't create a discount code")))?;
    if !store::valid_code(&code) {
        return Err(AppError::Internal(anyhow::anyhow!("the store returned an unusable discount code")));
    }
    let key = send_key(c, &via);
    let (message_id, form_id, delivered) = match via {
        Via::Message { message_id, .. } => (Some(message_id), None, false),
        Via::Form { form_id, .. } => (None, Some(form_id), true),
    };
    let inserted: Result<Option<(i64, Option<DateTime<Utc>>)>, sqlx::Error> = sqlx::query_as(
        "INSERT INTO coupon_codes (account_id, coupon_id, code, code_norm, status, profile_id, send_key, assigned_via, message_id, form_id, assigned_at, delivered_at, expires_at)
         VALUES ($1, $2, $3, upper($3), 'assigned', $4, $5, 'store', $6, $7, now(), CASE WHEN $8 THEN now() END, $9)
         ON CONFLICT (coupon_id, code_norm) DO NOTHING RETURNING id, expires_at",
    )
    .bind(c.account_id)
    .bind(c.id)
    .bind(&code)
    .bind(profile_id)
    .bind(&key)
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
        // Someone else gave this person a code meanwhile: keep the new store code in the pool, use theirs.
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
            let g = existing(&mut tx, c, profile_id, key.as_deref()).await?;
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
    profile_id: Option<Uuid>,
    sample: bool,
}

pub enum Prepared {
    Ready(SendCoupons),
    /// A coupon is out of codes: try this message again at that time.
    Hold(DateTime<Utc>),
}

impl SendCoupons {
    /// Put the codes where the template tags read them (a test send: the sample). A message with no
    /// coupons gets nothing, so a coupon tag can never render in it.
    pub fn fill(&self, ctx: &mut Value) {
        let Some(o) = ctx.as_object_mut() else { return };
        if self.sample {
            o.insert(CTX_KEY.into(), json!({"sample": true}));
            return;
        }
        if self.entries.is_empty() {
            // A profile property of that name must never stand in for prepared codes.
            o.remove(CTX_KEY);
            return;
        }
        let tz = self.tz.unwrap_or(chrono_tz::America::Los_Angeles);
        let mut m = Map::new();
        for (name, g) in &self.entries {
            let expires = g.expires_at.map(|t| t.with_timezone(&tz).format("%B %-d, %Y").to_string()).unwrap_or_default();
            m.insert(name.clone(), json!({"code": g.code, "expires": expires}));
        }
        o.insert(CTX_KEY.into(), json!({"codes": m}));
    }

    /// The provider accepted the message: record when these codes first reached the person.
    pub async fn delivered(&self, db: &PgPool) {
        let (Some(pid), false) = (self.profile_id, self.entries.is_empty()) else { return };
        let ids: Vec<i64> = self.entries.iter().map(|(_, g)| g.id).collect();
        if let Err(e) = sqlx::query("UPDATE coupon_codes SET delivered_at = COALESCE(delivered_at, now()) WHERE id = ANY($1) AND profile_id = $2").bind(&ids).bind(pid).execute(db).await {
            // The codes stay the person's either way; only the "given" date is missing.
            tracing::error!(error = %e, "couldn't mark coupon codes delivered");
        }
    }
}

/// What names this send for "new code each time" coupons: a campaign's message (its id is kept
/// across retries), or a flow run's step (a flow makes a new message id on every attempt).
async fn send_name(state: &AppState, spec: &SendSpec<'_>, message_id: Uuid) -> AppResult<String> {
    if let (None, Some(flow_id), Some(node)) = (spec.campaign_id, spec.flow_id, spec.flow_node_id) {
        let run: Option<Uuid> = sqlx::query_scalar("SELECT id FROM flow_runs WHERE flow_id = $1 AND profile_id = $2 AND status = 'active' ORDER BY started_at DESC LIMIT 1")
            .bind(flow_id)
            .bind(spec.profile.id)
            .fetch_optional(&state.db)
            .await?;
        if let Some(run) = run {
            return Ok(format!("flow:{run}:{node}"));
        }
    }
    Ok(match spec.campaign_id {
        Some(_) => format!("campaign:{message_id}"),
        None => format!("message:{message_id}"),
    })
}

/// Just before a message renders (in `sending::send`): claim a code from every coupon its content
/// shows. Messages without coupons return at once with no database work. Test sends show a sample
/// and use no code.
pub async fn prepare_send(state: &AppState, spec: &SendSpec<'_>, message_id: Uuid) -> AppResult<Prepared> {
    if !template::may_use_coupons(spec.content) {
        return Ok(Prepared::Ready(SendCoupons::default()));
    }
    let names = template::message_names(spec.channel, spec.content)?;
    if names.is_empty() {
        return Ok(Prepared::Ready(SendCoupons::default()));
    }
    if spec.is_test {
        return Ok(Prepared::Ready(SendCoupons { sample: true, ..Default::default() }));
    }
    let account = spec.account;
    let mut coupons = Vec::with_capacity(names.len());
    for name in &names {
        match store::by_name(&state.db, account.id, name).await? {
            Some(c) => {
                expired(&c)?;
                coupons.push(c);
            }
            None => return Err(AppError::bad(format!("there's no coupon named {name:?}; create it under Content → Coupons or fix the message"))),
        }
    }
    coupons.sort_by_key(|c| c.id);
    let send = if coupons.iter().any(|c| c.new_code_each_time) { send_name(state, spec, message_id).await? } else { String::new() };
    match claim_all(&state.db, &coupons, spec.profile.id, &Via::Message { message_id, send: &send }).await? {
        Claim::Got(given) => {
            for c in coupons.iter().filter(|c| c.out_of_codes_at.is_some()) {
                lift_hold(state, c, spec.campaign_id, spec.flow_id).await?;
            }
            let entries = coupons.iter().map(|c| c.name.to_lowercase()).zip(given).collect();
            Ok(Prepared::Ready(SendCoupons { entries, tz: account.timezone.parse().ok(), profile_id: Some(spec.profile.id), sample: false }))
        }
        Claim::Empty(i) => {
            hold(state, &coupons[i], spec.campaign_id, spec.flow_id).await?;
            Ok(Prepared::Hold(Utc::now() + chrono::Duration::minutes(HOLD_MINUTES)))
        }
        Claim::Busy => Ok(Prepared::Hold(Utc::now() + chrono::Duration::seconds(BUSY_SECONDS))),
    }
}

fn hold_subject(campaign_id: Option<Uuid>, flow_id: Option<Uuid>) -> Option<String> {
    match (campaign_id, flow_id) {
        (Some(id), _) => Some(format!("campaign:{id}")),
        (None, Some(id)) => Some(format!("flow:{id}")),
        (None, None) => None,
    }
}

/// Record that `c` ran out: alert once, show the reason on the waiting campaign, remember who waits.
async fn hold(state: &AppState, c: &Coupon, campaign_id: Option<Uuid>, flow_id: Option<Uuid>) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    mark_out_of_codes(&mut tx, c).await?;
    if let Some(subject) = hold_subject(campaign_id, flow_id) {
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
    }
    if let Some(id) = campaign_id {
        let msg = format!("Coupon {:?} has no codes left. Add codes under Content → Coupons and the campaign carries on within {HOLD_MINUTES} minutes.", c.name);
        sqlx::query("UPDATE campaigns SET error = $2, updated_at = now() WHERE id = $1 AND status = 'sending' AND error IS DISTINCT FROM $2")
            .bind(id)
            .bind(&msg)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// This sender got its codes again (people who already had one, say) while the coupon is still
/// marked empty: it no longer waits. The empty alert stays until codes are added.
async fn lift_hold(state: &AppState, c: &Coupon, campaign_id: Option<Uuid>, flow_id: Option<Uuid>) -> AppResult<()> {
    let Some(subject) = hold_subject(campaign_id, flow_id) else { return Ok(()) };
    let mut tx = state.db.begin().await?;
    let held: Option<Option<Uuid>> = sqlx::query_scalar("DELETE FROM coupon_holds WHERE coupon_id = $1 AND subject = $2 RETURNING campaign_id")
        .bind(c.id)
        .bind(&subject)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(Some(id)) = held {
        sqlx::query("UPDATE campaigns SET error = NULL, updated_at = now() WHERE id = $1 AND status = 'sending'").bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The first time a coupon runs out in an empty spell, queue an alert (in the same transaction, so
/// it's sent once). Adding codes ends the spell (see `store::restocked`).
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

fn form_coupon_name(form: &FormRow) -> Option<&str> {
    form.config.get("coupon").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// A sign-up form that hands out codes from a coupon (`config.coupon`), on a list without double
/// opt-in: only after a real sign-up, only to a person this sign-up created (anyone can type someone
/// else's address, and their code must not be shown to a stranger), one per person, within the
/// coupon's hourly limit and the per-visitor limit. The form's fixed `discount_code` isn't used then.
/// Never fails the sign-up.
pub async fn form_sign_up(state: &AppState, account: &Account, form: &FormRow, profile_id: Uuid, created: bool, ip: Option<&str>, out: &mut Value) {
    let Some(name) = form_coupon_name(form) else { return };
    if let Some(o) = out.as_object_mut() {
        o.remove("discount_code");
    }
    if !created {
        return;
    }
    let res = async {
        let Some(c) = store::by_name(&state.db, account.id, name).await? else {
            tracing::warn!(account = %account.id, form = %form.id, "the form's coupon doesn't exist; no code given");
            return Ok(None);
        };
        // The same address check the other public endpoints use; no usable address shares one allowance.
        let visitor = crate::auth::sha256_hex(&format!("coupon-ip:{}", ip.filter(|s| !s.is_empty()).unwrap_or("none")));
        form_code(state, &c, form.id, profile_id, Some(&visitor)).await
    }
    .await;
    match res {
        Ok(Some(code)) => out["discount_code"] = json!(code.code),
        Ok(None) => {}
        Err(e) => tracing::warn!(account = %account.id, form = %form.id, error = %e.public_message(), "couldn't give a sign-up form coupon code"),
    }
}

/// Give a form code within the coupon's hourly form limit and (with `visitor`) the per-visitor limit.
async fn form_code(state: &AppState, c: &Coupon, form_id: Uuid, profile_id: Uuid, visitor: Option<&str>) -> AppResult<Option<Given>> {
    expired(c)?;
    let mut tx = state.db.begin().await?;
    // Checked and recorded under locks on the coupon and the visitor, always in that order, so
    // simultaneous sign-ups can't get past a limit together.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("coupon-form:{}", c.id)).execute(&mut *tx).await?;
    if let Some(h) = visitor {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("coupon-ip:{h}")).execute(&mut *tx).await?;
    }
    let via = Via::Form { form_id, ip_hash: visitor };
    if let Some(g) = existing(&mut tx, c, profile_id, send_key(c, &via).as_deref()).await? {
        tx.commit().await?;
        return Ok(Some(g));
    }
    let last_hour: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE coupon_id = $1 AND assigned_via = 'form' AND assigned_at > now() - interval '1 hour'")
        .bind(c.id)
        .fetch_one(&mut *tx)
        .await?;
    if last_hour >= i64::from(c.form_codes_per_hour) {
        tracing::warn!(account = %c.account_id, coupon = %c.id, "sign-up form code limit for the hour reached; no code given");
        return Ok(None);
    }
    if let Some(h) = visitor {
        let from_ip: i64 = sqlx::query_scalar("SELECT count(*) FROM coupon_codes WHERE ip_hash = $1 AND assigned_at > now() - interval '1 hour'")
            .bind(h)
            .fetch_one(&mut *tx)
            .await?;
        if from_ip >= FORM_CODES_PER_IP_HOUR {
            tracing::warn!(account = %c.account_id, coupon = %c.id, "per-visitor sign-up form code limit reached; no code given");
            return Ok(None);
        }
    }
    let got = match claim_in(&mut tx, c, profile_id, &via).await? {
        One::Got(g) => Some(g),
        One::Empty => {
            mark_out_of_codes(&mut tx, c).await?;
            None
        }
        One::Busy => None,
    };
    tx.commit().await?;
    Ok(got)
}

/// A double opt-in sign-up through a form with a coupon is waiting to confirm: record, with the
/// confirmation, the coupon the form has now, if this sign-up created the person. Confirming gives
/// that code (see [`confirmation_code`]); a sign-up that didn't earn one never shows one.
pub async fn form_pending(state: &AppState, account: &Account, form: &FormRow, profile_id: Uuid, created: bool) {
    let Some(name) = form_coupon_name(form) else { return };
    if !created {
        return;
    }
    let res: AppResult<()> = async {
        let Some(c) = store::by_name(&state.db, account.id, name).await? else { return Ok(()) };
        sqlx::query(
            "INSERT INTO coupon_signup_grants (confirmation_id, account_id, coupon_id, form_id, profile_id)
             SELECT o.id, $2, $3, $4, $1 FROM optin_confirmations o
             WHERE o.profile_id = $1 AND o.form_id = $4 AND o.account_id = $2 AND o.confirmed_at IS NULL
             ORDER BY o.created_at DESC LIMIT 1
             ON CONFLICT (confirmation_id) DO NOTHING",
        )
        .bind(profile_id)
        .bind(account.id)
        .bind(c.id)
        .bind(form.id)
        .execute(&state.db)
        .await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::warn!(account = %account.id, form = %form.id, error = %e.public_message(), "couldn't record a sign-up's coupon");
    }
}

/// The code a confirmed double opt-in sign-up earned, for the confirmation page: given once (the
/// coupon its form had at sign-up), shown only within 24 hours of confirming. A link whose sign-up
/// earned nothing, or an older one, shows no code. The link reached the address, so no per-visitor
/// limit applies; the coupon's hourly form limit does.
pub async fn confirmation_code(state: &AppState, confirmation_id: Uuid) -> Option<String> {
    let res: AppResult<Option<String>> = async {
        let row: Option<(Uuid, Uuid, Uuid, Uuid, Option<i64>)> = sqlx::query_as(
            "SELECT g.account_id, g.coupon_id, g.form_id, g.profile_id, g.code_id FROM coupon_signup_grants g JOIN optin_confirmations o ON o.id = g.confirmation_id
             WHERE g.confirmation_id = $1 AND o.confirmed_at IS NOT NULL AND o.confirmed_at > now() - make_interval(hours => $2)",
        )
        .bind(confirmation_id)
        .bind(CONFIRM_SHOW_HOURS)
        .fetch_optional(&state.db)
        .await?;
        let Some((account_id, coupon_id, form_id, profile_id, code_id)) = row else { return Ok(None) };
        if let Some(id) = code_id {
            return Ok(sqlx::query_scalar("SELECT code FROM coupon_codes WHERE id = $1 AND profile_id = $2").bind(id).bind(profile_id).fetch_optional(&state.db).await?);
        }
        let Ok(c) = store::load(&state.db, account_id, coupon_id).await else { return Ok(None) };
        let Some(g) = form_code(state, &c, form_id, profile_id, None).await? else { return Ok(None) };
        sqlx::query("UPDATE coupon_signup_grants SET code_id = $2 WHERE confirmation_id = $1 AND code_id IS NULL").bind(confirmation_id).bind(g.id).execute(&state.db).await?;
        Ok(Some(g.code))
    }
    .await;
    res.unwrap_or_else(|e| {
        tracing::warn!(error = %e.public_message(), "couldn't give a confirmed sign-up its coupon code");
        None
    })
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
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 ORDER BY (m.role = 'owner') DESC, m.created_at LIMIT 1",
    )
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

/// The person's own codes in a clicked link, replaced before the click is stored (the redirect
/// itself is unchanged). Cheap for people without codes: one indexed lookup.
pub async fn mask_link(db: &PgPool, profile_id: Uuid, url: &str) -> String {
    let codes: Vec<String> = sqlx::query_scalar("SELECT code_norm FROM coupon_codes WHERE profile_id = $1 AND status = 'assigned' LIMIT 200")
        .bind(profile_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    mask_codes(url, &codes)
}

/// Replace each of `codes` (upper-cased) in `url`, in any letter case, with `{coupon_code}`.
pub fn mask_codes(url: &str, codes: &[String]) -> String {
    // Upper-casing only changes ASCII bytes, so positions in it are positions in `url`.
    let upper = url.to_ascii_uppercase();
    let mut ranges: Vec<(usize, usize)> = codes.iter().filter(|c| !c.is_empty()).flat_map(|c| upper.match_indices(c.as_str()).map(|(i, m)| (i, i + m.len())).collect::<Vec<_>>()).collect();
    if ranges.is_empty() {
        return url.to_string();
    }
    ranges.sort_unstable();
    let mut out = String::with_capacity(url.len());
    let mut at = 0;
    for (start, end) in ranges {
        if end <= at {
            continue;
        }
        out.push_str(&url[at..start.max(at)]);
        out.push_str("{coupon_code}");
        at = end;
    }
    out.push_str(&url[at..]);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_lose_the_persons_codes() {
        let codes = vec!["SAVE-ABC123".to_string()];
        assert_eq!(super::mask_codes("https://shop.test/discount/save-abc123?next=/cart", &codes), "https://shop.test/discount/{coupon_code}?next=/cart");
        assert_eq!(super::mask_codes("https://shop.test/", &codes), "https://shop.test/");
    }
}
