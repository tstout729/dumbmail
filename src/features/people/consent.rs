//! Consent history: every change to a person's email or SMS consent, their suppression, or their
//! "do not sell or share" choice is recorded in `consent_events` with how it happened.
//!
//! Write sites call [`record_consent_change`] (or one of the setters here, which change the profile
//! and record in one transaction). Who made a change (a team member in the dashboard, an API key,
//! the MCP server, a form) is set once per request with [`with_source`]; the shared profile upsert
//! reads it, so the code in between doesn't pass it along. Rows are never updated (a trigger refuses)
//! and the app has no path that deletes them; they go only with the profile.

use std::future::Future;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::audience::{ProfileInput, Upserted};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::Profile;

/// How a consent change happened. `method` is one of the names in the `consent_events` migration.
#[derive(Debug, Clone)]
pub struct ConsentSource {
    pub method: &'static str,
    pub via: &'static str,
    pub actor_user_id: Option<Uuid>,
    pub form_id: Option<Uuid>,
    pub message_id: Option<Uuid>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub detail: Map<String, Value>,
}

impl Default for ConsentSource {
    fn default() -> Self {
        Self::new("other", "system")
    }
}

impl ConsentSource {
    pub fn new(method: &'static str, via: &'static str) -> Self {
        Self { method, via, actor_user_id: None, form_id: None, message_id: None, ip: None, user_agent: None, detail: Map::new() }
    }

    /// A dashboard request (the signed-in member) or a REST API call (an API key).
    pub fn from_ctx(ctx: &Ctx) -> Self {
        match (ctx.via_api_key, ctx.user_id) {
            (false, Some(user)) => Self { actor_user_id: Some(user), ..Self::new("dashboard", "dashboard") },
            _ => Self::new("api", "api"),
        }
    }

    /// A call through the MCP server.
    pub fn mcp(ctx: &Ctx) -> Self {
        Self { actor_user_id: ctx.user_id, ..Self::new("mcp", "mcp") }
    }

    /// A sign-up form submission.
    pub fn form(form_id: Uuid, ip: Option<&str>, user_agent: Option<&str>) -> Self {
        Self::new("form", "public").with_form(form_id).with_ip(ip).with_user_agent(user_agent)
    }

    /// Same caller, another method (an import started from the dashboard is still that member's).
    pub fn method(mut self, method: &'static str) -> Self {
        self.method = method;
        self
    }

    pub fn with_form(mut self, id: Uuid) -> Self {
        self.form_id = Some(id);
        self
    }

    pub fn with_message(mut self, id: Uuid) -> Self {
        self.message_id = Some(id);
        self
    }

    pub fn with_ip(mut self, ip: Option<&str>) -> Self {
        self.ip = ip.map(|s| s.chars().take(64).collect());
        self
    }

    pub fn with_user_agent(mut self, ua: Option<&str>) -> Self {
        self.user_agent = ua.map(|s| s.chars().take(400).collect());
        self
    }

    pub fn with_detail(mut self, key: &str, value: Value) -> Self {
        self.detail.insert(key.into(), value);
        self
    }

    /// The person themselves asked to subscribe (so a deleted person's old unsubscribe gives way).
    fn is_own_opt_in(&self) -> bool {
        matches!(self.method, "form" | "double_opt_in" | "sms_keyword")
    }
}

tokio::task_local! {
    static SOURCE: ConsentSource;
}

/// Run `f` with `src` as the source of any consent change it makes.
pub async fn with_source<F: Future>(src: ConsentSource, f: F) -> F::Output {
    SOURCE.scope(src, f).await
}

/// Run `f` as the current caller, but with another method (e.g. "import").
pub async fn with_method<F: Future>(method: &'static str, f: F) -> F::Output {
    with_source(current_source().method(method), f).await
}

/// The source set by the nearest [`with_source`], or "other" when none was set.
pub fn current_source() -> ConsentSource {
    SOURCE.try_with(|s| s.clone()).unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Email,
    Sms,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Email => "email",
            Channel::Sms => "sms",
        }
    }
    pub fn parse(s: &str) -> AppResult<Self> {
        match s {
            "email" => Ok(Channel::Email),
            "sms" => Ok(Channel::Sms),
            _ => Err(AppError::bad("channel must be email or sms")),
        }
    }
    fn consent_col(self) -> &'static str {
        match self {
            Channel::Email => "email_consent",
            Channel::Sms => "sms_consent",
        }
    }
}

/// One change to record.
pub struct Change<'a> {
    pub account_id: Uuid,
    pub profile_id: Uuid,
    /// email | sms | ads
    pub channel: &'a str,
    /// consent | suppression | opt_out
    pub field: &'a str,
    pub old: Option<&'a str>,
    pub new: Option<&'a str>,
    pub address: Option<&'a str>,
}

/// Add one row to the consent history. Every consent, suppression and "do not sell or share"
/// write goes through here.
pub async fn record_consent_change(conn: &mut PgConnection, c: &Change<'_>, src: &ConsentSource) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO consent_events (id, account_id, profile_id, channel, field, old_value, new_value, method, via, address,
                                     actor_user_id, form_id, message_id, ip, user_agent, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
    )
    .bind(Uuid::now_v7())
    .bind(c.account_id)
    .bind(c.profile_id)
    .bind(c.channel)
    .bind(c.field)
    .bind(c.old)
    .bind(c.new)
    .bind(src.method)
    .bind(src.via)
    .bind(c.address)
    .bind(src.actor_user_id)
    .bind(src.form_id)
    .bind(src.message_id)
    .bind(&src.ip)
    .bind(&src.user_agent)
    .bind(Value::Object(src.detail.clone()))
    .execute(conn)
    .await?;
    Ok(())
}

/// The consent-related columns of a profile.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConsentState {
    pub email: Option<String>,
    pub phone_number: Option<String>,
    pub email_consent: String,
    pub sms_consent: String,
    pub email_suppressed_reason: Option<String>,
    pub sms_suppressed_reason: Option<String>,
    pub ads_opt_out: bool,
}

pub const STATE_COLS: &str = "email, phone_number, email_consent, sms_consent, email_suppressed_reason, sms_suppressed_reason, ads_opt_out";

impl ConsentState {
    fn consent(&self, ch: Channel) -> &str {
        match ch {
            Channel::Email => &self.email_consent,
            Channel::Sms => &self.sms_consent,
        }
    }
    fn suppressed(&self, ch: Channel) -> Option<&str> {
        match ch {
            Channel::Email => self.email_suppressed_reason.as_deref(),
            Channel::Sms => self.sms_suppressed_reason.as_deref(),
        }
    }
    fn address(&self, ch: Channel) -> Option<&str> {
        match ch {
            Channel::Email => self.email.as_deref(),
            Channel::Sms => self.phone_number.as_deref(),
        }
    }
}

pub async fn load_state(conn: &mut PgConnection, account_id: Uuid, profile_id: Uuid, lock: bool) -> sqlx::Result<Option<ConsentState>> {
    let q = format!("SELECT {STATE_COLS} FROM profiles WHERE id = $1 AND account_id = $2{}", if lock { " FOR UPDATE" } else { "" });
    sqlx::query_as(&q).bind(profile_id).bind(account_id).fetch_optional(conn).await
}

/// Record every difference between two states of one profile. `before` is None for a new profile,
/// whose defaults (never subscribed, not suppressed) aren't changes.
pub async fn record_diff(conn: &mut PgConnection, account_id: Uuid, profile_id: Uuid, before: Option<&ConsentState>, after: &ConsentState, src: &ConsentSource) -> sqlx::Result<usize> {
    let mut n = 0;
    for ch in [Channel::Email, Channel::Sms] {
        let old_consent = before.map(|b| b.consent(ch)).unwrap_or("never_subscribed");
        if old_consent != after.consent(ch) {
            record_consent_change(conn, &Change { account_id, profile_id, channel: ch.as_str(), field: "consent", old: Some(old_consent), new: Some(after.consent(ch)), address: after.address(ch) }, src).await?;
            n += 1;
        }
        let old_sup = before.and_then(|b| b.suppressed(ch));
        if old_sup != after.suppressed(ch) {
            record_consent_change(conn, &Change { account_id, profile_id, channel: ch.as_str(), field: "suppression", old: old_sup, new: after.suppressed(ch), address: after.address(ch) }, src).await?;
            n += 1;
        }
    }
    if before.map(|b| b.ads_opt_out).unwrap_or(false) != after.ads_opt_out {
        let v = |b: bool| if b { "on" } else { "off" };
        record_consent_change(
            conn,
            &Change { account_id, profile_id, channel: "ads", field: "opt_out", old: before.map(|b| v(b.ads_opt_out)), new: Some(v(after.ads_opt_out)), address: None },
            src,
        )
        .await?;
        n += 1;
    }
    Ok(n)
}

/// Set one channel's consent and record it, in one transaction. With `only_from`, the change happens
/// only when the current value is one of those. Returns the old value when something changed.
pub async fn set_consent(db: &PgPool, account_id: Uuid, profile_id: Uuid, ch: Channel, new: &str, only_from: Option<&[&str]>, src: &ConsentSource) -> AppResult<Option<String>> {
    let mut tx = db.begin().await?;
    let Some(before) = load_state(&mut tx, account_id, profile_id, true).await? else { return Ok(None) };
    let old = before.consent(ch).to_string();
    if old == new || only_from.is_some_and(|o| !o.contains(&old.as_str())) {
        return Ok(None);
    }
    sqlx::query(&format!("UPDATE profiles SET {} = $2, updated_at = now() WHERE id = $1", ch.consent_col())).bind(profile_id).bind(new).execute(&mut *tx).await?;
    record_consent_change(&mut tx, &Change { account_id, profile_id, channel: ch.as_str(), field: "consent", old: Some(&old), new: Some(new), address: before.address(ch) }, src).await?;
    tx.commit().await?;
    Ok(Some(old))
}

/// Suppress a channel for a reason (hard_bounce, complaint, manual). `overwrite` replaces another
/// reason (a bounce or complaint does; a manual suppression never replaces a stronger one).
/// `unsubscribe` also sets the channel's consent to unsubscribed. Returns whether anything changed.
pub async fn suppress(db: &PgPool, account_id: Uuid, profile_id: Uuid, ch: Channel, reason: &str, overwrite: bool, unsubscribe: bool, src: &ConsentSource) -> AppResult<bool> {
    let mut tx = db.begin().await?;
    let Some(before) = load_state(&mut tx, account_id, profile_id, true).await? else { return Ok(false) };
    let mut after = before.clone();
    let slot = match ch {
        Channel::Email => &mut after.email_suppressed_reason,
        Channel::Sms => &mut after.sms_suppressed_reason,
    };
    if slot.is_none() || (overwrite && slot.as_deref() != Some(reason)) {
        *slot = Some(reason.to_string());
    }
    if unsubscribe {
        match ch {
            Channel::Email => after.email_consent = "unsubscribed".into(),
            Channel::Sms => after.sms_consent = "unsubscribed".into(),
        }
    }
    let changed = write_state(&mut tx, account_id, profile_id, &before, &after, src).await?;
    tx.commit().await?;
    Ok(changed)
}

/// Why a suppression can't be lifted from the suppression list, in words for the person asking.
pub fn why_not_unsuppressible(ch: Channel, reason: Option<&str>, consent: &str) -> Option<String> {
    match (ch, reason) {
        (_, Some("manual")) => None,
        (Channel::Email, Some("hard_bounce")) => Some("This address hard bounced: the mail server said it doesn't exist. Sending to it again would hurt your sender reputation, so it stays suppressed.".into()),
        (Channel::Email, Some("complaint")) => Some("This person marked one of your emails as spam. That can't be undone here; only a new sign-up from them can add them back.".into()),
        (_, Some(other)) => Some(format!("This suppression ({other}) wasn't added by hand, so it can't be lifted here.")),
        (Channel::Email, None) if consent == "unsubscribed" => Some("This person unsubscribed themselves. Only they can subscribe again, for example with one of your sign-up forms.".into()),
        (Channel::Email, None) if consent == "pending" => Some("This person hasn't confirmed their sign-up yet. They need to click the link in the confirmation email.".into()),
        (Channel::Sms, None) if consent == "unsubscribed" => Some("This person opted out of texts (for example by replying STOP). Only they can opt back in, by texting START.".into()),
        (_, None) => Some("This person isn't suppressed on that channel.".into()),
    }
}

/// Lift a manual suppression. Refuses (with the reason) anything else: a bounce, a complaint, or the
/// person's own unsubscribe. Consent is left as it is, so this never subscribes anyone.
pub async fn unsuppress_manual(db: &PgPool, account_id: Uuid, profile_id: Uuid, ch: Channel, src: &ConsentSource) -> AppResult<()> {
    let mut tx = db.begin().await?;
    let before = load_state(&mut tx, account_id, profile_id, true).await?.ok_or_else(|| AppError::not_found("profile"))?;
    if let Some(why) = why_not_unsuppressible(ch, before.suppressed(ch), before.consent(ch)) {
        return Err(AppError::Conflict(why));
    }
    let mut after = before.clone();
    match ch {
        Channel::Email => after.email_suppressed_reason = None,
        Channel::Sms => after.sms_suppressed_reason = None,
    }
    write_state(&mut tx, account_id, profile_id, &before, &after, src).await?;
    tx.commit().await?;
    Ok(())
}

/// Turn "do not sell or share" on or off and record it. Returns whether it changed.
pub async fn set_ads_opt_out(db: &PgPool, account_id: Uuid, profile_id: Uuid, value: bool, src: &ConsentSource) -> AppResult<bool> {
    let mut tx = db.begin().await?;
    let before = load_state(&mut tx, account_id, profile_id, true).await?.ok_or_else(|| AppError::not_found("profile"))?;
    if before.ads_opt_out == value {
        return Ok(false);
    }
    sqlx::query("UPDATE profiles SET ads_opt_out = $2, ads_opt_out_at = now(), ads_opt_out_source = $3, updated_at = now() WHERE id = $1")
        .bind(profile_id)
        .bind(value)
        .bind(src.method)
        .execute(&mut *tx)
        .await?;
    let after = ConsentState { ads_opt_out: value, ..before.clone() };
    record_diff(&mut tx, account_id, profile_id, Some(&before), &after, src).await?;
    tx.commit().await?;
    Ok(true)
}

/// Write the consent columns that differ and record each difference.
pub async fn write_state(conn: &mut PgConnection, account_id: Uuid, profile_id: Uuid, before: &ConsentState, after: &ConsentState, src: &ConsentSource) -> sqlx::Result<bool> {
    if before.email_consent == after.email_consent
        && before.sms_consent == after.sms_consent
        && before.email_suppressed_reason == after.email_suppressed_reason
        && before.sms_suppressed_reason == after.sms_suppressed_reason
    {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE profiles SET email_consent = $2, sms_consent = $3, email_suppressed_reason = $4, sms_suppressed_reason = $5, updated_at = now() WHERE id = $1",
    )
    .bind(profile_id)
    .bind(&after.email_consent)
    .bind(&after.sms_consent)
    .bind(&after.email_suppressed_reason)
    .bind(&after.sms_suppressed_reason)
    .execute(&mut *conn)
    .await?;
    record_diff(conn, account_id, profile_id, Some(before), after, src).await?;
    Ok(true)
}

fn state_of(p: &Profile) -> ConsentState {
    ConsentState {
        email: p.email.clone(),
        phone_number: p.phone_number.clone(),
        email_consent: p.email_consent.clone(),
        sms_consent: p.sms_consent.clone(),
        email_suppressed_reason: None,
        sms_suppressed_reason: None,
        ads_opt_out: false,
    }
}

/// Called by the shared profile upsert after it saved a profile: records consent changes, gives a
/// re-created deleted person their old suppression back, and notes identifiers that belong to two
/// different profiles. Returns the profile as it now is.
pub async fn after_upsert(db: &PgPool, account_id: Uuid, input: &ProfileInput, before: Option<&Profile>, up: Upserted) -> AppResult<Upserted> {
    let src = current_source();
    let mut conn = db.acquire().await?;
    // The upsert only writes consent (never suppression or the opt-out), so those parts stay equal.
    let after = state_of(&up.profile);
    let prev = before.map(state_of);
    record_diff(&mut conn, account_id, up.profile.id, if up.created { None } else { prev.as_ref() }, &after, &src).await?;
    drop(conn);
    note_identity_conflict(db, account_id, input).await;
    // A new profile, or an address new to this one: a deleted person's suppression record applies.
    let new_address = before.is_none_or(|b| b.email != up.profile.email || b.phone_number != up.profile.phone_number);
    if !up.created && !new_address {
        return Ok(up);
    }
    let restored = super::privacy::apply_tombstones(db, account_id, &up.profile, input, src.is_own_opt_in()).await?;
    if !restored {
        return Ok(up);
    }
    let profile = super::load_profile(db, account_id, up.profile.id).await?;
    Ok(Upserted { profile, created: up.created })
}

/// After an email or phone number on a profile was changed directly (not through the upsert): a
/// deleted person's suppression record for the new address applies. Returns the profile as it now is.
pub async fn after_address_change(db: &PgPool, account_id: Uuid, profile: Profile) -> AppResult<Profile> {
    if super::privacy::apply_tombstones(db, account_id, &profile, &ProfileInput::default(), false).await? {
        return super::load_profile(db, account_id, profile.id).await;
    }
    Ok(profile)
}

/// An upsert named identifiers held by different profiles (an email on one, a phone number on
/// another). Nothing is changed; the pair is listed by the duplicates finder. Never fails the upsert.
pub async fn note_identity_conflict(db: &PgPool, account_id: Uuid, input: &ProfileInput) {
    let email = input.email.as_deref().and_then(crate::models::normalize_email);
    let phone = input.phone_number.as_deref().and_then(crate::models::normalize_phone);
    let external = input.external_id.as_deref().filter(|s| !s.is_empty());
    if [email.is_some(), phone.is_some(), external.is_some()].iter().filter(|b| **b).count() < 2 {
        return;
    }
    let res: sqlx::Result<()> = async {
        let owners: Vec<(Uuid, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT id, CASE WHEN email = $2 THEN email END, CASE WHEN phone_number = $3 THEN phone_number END, CASE WHEN external_id = $4 THEN external_id END
             FROM profiles WHERE account_id = $1 AND (email = $2 OR phone_number = $3 OR external_id = $4) ORDER BY id LIMIT 3",
        )
        .bind(account_id)
        .bind(&email)
        .bind(&phone)
        .bind(external)
        .fetch_all(db)
        .await?;
        for (i, a) in owners.iter().enumerate() {
            for b in owners.iter().skip(i + 1) {
                let mut ids = Map::new();
                for (k, x, y) in [("email", &a.1, &b.1), ("phone_number", &a.2, &b.2), ("external_id", &a.3, &b.3)] {
                    if let Some(v) = x.as_ref().or(y.as_ref()) {
                        ids.insert(k.into(), json!(v));
                    }
                }
                sqlx::query(
                    "INSERT INTO identity_conflicts (account_id, profile_a, profile_b, identifiers) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (account_id, profile_a, profile_b) DO UPDATE SET times_seen = identity_conflicts.times_seen + 1, last_seen = now(), identifiers = EXCLUDED.identifiers",
                )
                .bind(account_id)
                .bind(a.0)
                .bind(b.0)
                .bind(Value::Object(ids))
                .execute(db)
                .await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::warn!(account = %account_id, error = %e, "couldn't note an identity conflict");
    }
}

/// When consent history starts for this account: the day recording began, or the account's start.
pub async fn history_started_at(db: &PgPool, account_id: Uuid) -> AppResult<DateTime<Utc>> {
    Ok(sqlx::query_scalar(
        "SELECT GREATEST(a.created_at, COALESCE((SELECT value FROM people_settings WHERE key = 'consent_history_started_at'), a.created_at)) FROM accounts a WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?)
}

fn label(v: Option<&str>) -> &str {
    match v {
        None => "none",
        Some("subscribed") => "subscribed",
        Some("unsubscribed") => "unsubscribed",
        Some("never_subscribed") => "never subscribed",
        Some("pending") => "waiting to confirm",
        Some("hard_bounce") => "hard bounce",
        Some("complaint") => "spam complaint",
        Some("manual") => "suppressed by hand",
        Some(other) => other,
    }
}

/// A sentence saying what changed.
pub fn describe_change(channel: &str, field: &str, old: Option<&str>, new: Option<&str>) -> String {
    let ch = match channel {
        "email" => "Email",
        "sms" => "SMS",
        _ => "Do not sell or share",
    };
    match field {
        "consent" => format!("{ch} consent: {} → {}", label(old), label(new)),
        "suppression" => match new {
            Some(r) => format!("{ch} suppressed ({})", label(Some(r))),
            None => format!("{ch} suppression lifted (was {})", label(old)),
        },
        _ => format!("{ch}: turned {}", new.unwrap_or("off")),
    }
}

/// A sentence saying how a change happened.
pub fn describe_how(method: &str, actor: Option<&str>, form: Option<&str>, subject: Option<&str>, detail: &Value) -> String {
    let quoted = |s: Option<&str>, fallback: &str| s.map(|s| format!("“{s}”")).unwrap_or_else(|| fallback.to_string());
    match method {
        "form" => format!("Sign-up form {}", quoted(form, "(since deleted)")),
        "import" => "CSV import".into(),
        "api" => "API".into(),
        "mcp" => "AI assistant (MCP)".into(),
        "dashboard" => format!("Changed in the dashboard by {}", actor.unwrap_or("a former team member")),
        "unsubscribe_link" => format!("Unsubscribe link in {}", quoted(subject, "a message")),
        "preference_page" => format!("Preference page, from {}", quoted(subject, "a message")),
        "one_click" => format!("One-click unsubscribe in their mail app, on {}", quoted(subject, "a message")),
        "sms_keyword" => format!("Texted {}", detail["keyword"].as_str().map(|k| format!("“{}”", k.to_uppercase())).unwrap_or_else(|| "a keyword".into())),
        "double_opt_in" => "Confirmed their sign-up by email (double opt-in)".into(),
        "hard_bounce" => format!("Email to them hard bounced{}", subject.map(|s| format!(" (“{s}”)")).unwrap_or_default()),
        "spam_complaint" => format!("Marked {} as spam", quoted(subject, "an email")),
        "integration" => format!("Integration{}", detail["integration"].as_str().map(|i| format!(": {i}")).unwrap_or_default()),
        "klaviyo_import" => "Klaviyo import".into(),
        "merge" => "Merged with a duplicate profile".into(),
        "bulk_action" => format!("Bulk action{}", actor.map(|a| format!(" by {a}")).unwrap_or_default()),
        "suppression_list" => format!("Suppression list{}", actor.map(|a| format!(", by {a}")).unwrap_or_default()),
        "privacy_request" => "Privacy request".into(),
        "deleted_profile" => "Matched the suppression record kept for a deleted profile".into(),
        "gpc" => "Global Privacy Control signal from their browser".into(),
        _ => "Other".into(),
    }
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct HistoryRow {
    id: Uuid,
    channel: String,
    field: String,
    old_value: Option<String>,
    new_value: Option<String>,
    method: String,
    via: Option<String>,
    address: Option<String>,
    actor_user_id: Option<Uuid>,
    actor_email: Option<String>,
    form_id: Option<Uuid>,
    form_name: Option<String>,
    message_id: Option<Uuid>,
    message_subject: Option<String>,
    ip: Option<String>,
    user_agent: Option<String>,
    detail: Value,
    occurred_at: DateTime<Utc>,
}

/// One page of a profile's consent history, newest first.
pub async fn history(db: &PgPool, account_id: Uuid, profile_id: Uuid, limit: i64, offset: i64) -> AppResult<(Vec<Value>, i64)> {
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM consent_events WHERE account_id = $1 AND profile_id = $2").bind(account_id).bind(profile_id).fetch_one(db).await?;
    let rows: Vec<HistoryRow> = sqlx::query_as(
        "SELECT e.id, e.channel, e.field, e.old_value, e.new_value, e.method, e.via, e.address, e.actor_user_id, u.email AS actor_email,
                e.form_id, f.name AS form_name, e.message_id, m.subject AS message_subject, e.ip, e.user_agent, e.detail, e.occurred_at
         FROM consent_events e
         LEFT JOIN users u ON u.id = e.actor_user_id
         LEFT JOIN forms f ON f.id = e.form_id AND f.account_id = e.account_id
         LEFT JOIN messages m ON m.id = e.message_id AND m.account_id = e.account_id
         WHERE e.account_id = $1 AND e.profile_id = $2
         ORDER BY e.occurred_at DESC, e.id DESC LIMIT $3 OFFSET $4",
    )
    .bind(account_id)
    .bind(profile_id)
    .bind(limit.clamp(1, 500))
    .bind(offset.max(0))
    .fetch_all(db)
    .await?;
    let data = rows
        .into_iter()
        .map(|r| {
            let summary = describe_change(&r.channel, &r.field, r.old_value.as_deref(), r.new_value.as_deref());
            let how = describe_how(&r.method, r.actor_email.as_deref(), r.form_name.as_deref(), r.message_subject.as_deref(), &r.detail);
            let mut v = serde_json::to_value(&r).unwrap_or_default();
            v["summary"] = json!(summary);
            v["how"] = json!(how);
            v
        })
        .collect();
    Ok((data, total))
}

/// Whether the person can be emailed and texted, and if not, every reason why.
pub async fn reachability(db: &PgPool, account_id: Uuid, profile_id: Uuid) -> AppResult<Value> {
    let mut conn = db.acquire().await?;
    let s = load_state(&mut conn, account_id, profile_id, false).await?.ok_or_else(|| AppError::not_found("profile"))?;
    drop(conn);
    let paused: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT sending_paused_at FROM accounts WHERE id = $1").bind(account_id).fetch_one(db).await?;
    // The latest recorded change that put each channel in its current state, for "since" and "how".
    let latest = |channel: &'static str, field: &'static str, value: String| async move {
        let row: Option<(DateTime<Utc>, String, Option<String>, Option<String>, Option<String>, Value)> = sqlx::query_as(
            "SELECT e.occurred_at, e.method, u.email, f.name, m.subject, e.detail FROM consent_events e
             LEFT JOIN users u ON u.id = e.actor_user_id
             LEFT JOIN forms f ON f.id = e.form_id AND f.account_id = e.account_id
             LEFT JOIN messages m ON m.id = e.message_id AND m.account_id = e.account_id
             WHERE e.account_id = $1 AND e.profile_id = $2 AND e.channel = $3 AND e.field = $4 AND e.new_value = $5
             ORDER BY e.occurred_at DESC LIMIT 1",
        )
        .bind(account_id)
        .bind(profile_id)
        .bind(channel)
        .bind(field)
        .bind(value)
        .fetch_optional(db)
        .await?;
        AppResult::Ok(row.map(|r| json!({"since": r.0, "how": describe_how(&r.1, r.2.as_deref(), r.3.as_deref(), r.4.as_deref(), &r.5)})))
    };
    let reason = |code: &str, text: &str, when: Option<Value>| {
        let mut v = json!({"code": code, "text": text});
        if let Some(Value::Object(w)) = when {
            v.as_object_mut().expect("object").extend(w);
        }
        v
    };

    let mut email = Vec::new();
    if s.email.is_none() {
        email.push(reason("no_address", "There's no email address on this profile.", None));
    }
    match s.email_consent.as_str() {
        "unsubscribed" => email.push(reason("unsubscribed", "They unsubscribed from email.", latest("email", "consent", "unsubscribed".into()).await?)),
        "pending" => email.push(reason("pending", "They signed up to a double opt-in list and haven't clicked the confirmation link yet.", latest("email", "consent", "pending".into()).await?)),
        _ => {}
    }
    if let Some(r) = s.email_suppressed_reason.clone() {
        let text = match r.as_str() {
            "hard_bounce" => "An email to this address hard bounced (the address doesn't exist), so it's suppressed.",
            "complaint" => "They marked one of your emails as spam, so they're suppressed.",
            "manual" => "Someone on your team added them to the suppression list.",
            _ => "This address is suppressed.",
        };
        email.push(reason(&r, text, latest("email", "suppression", r.clone()).await?));
    }
    if paused.is_some() {
        email.push(reason("account_paused", "Email sending is paused for your whole account right now.", Some(json!({"since": paused}))));
    }

    let mut sms = Vec::new();
    if s.phone_number.is_none() {
        sms.push(reason("no_address", "There's no phone number on this profile.", None));
    }
    match s.sms_consent.as_str() {
        "subscribed" => {}
        "unsubscribed" => sms.push(reason("unsubscribed", "They opted out of texts.", latest("sms", "consent", "unsubscribed".into()).await?)),
        _ => sms.push(reason("not_subscribed", "They haven't agreed to get texts. Texting needs their explicit consent.", None)),
    }
    if let Some(r) = s.sms_suppressed_reason.clone() {
        sms.push(reason(&r, "Someone on your team added this number to the suppression list.", latest("sms", "suppression", r.clone()).await?));
    }
    Ok(json!({
        "email": {"can_send": email.is_empty(), "reasons": email},
        "sms": {"can_send": sms.is_empty(), "reasons": sms},
    }))
}
