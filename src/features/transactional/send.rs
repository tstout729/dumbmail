//! Sending one transactional email: validate, reserve (idempotency and caps), resolve the
//! recipient, honor suppression, render, check the balance, send, and record.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::guard;
use super::review;
use crate::audience::{record_event, NewEvent, ProfileInput};
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, Account, Profile};
use crate::providers::email::OutgoingEmail;
use crate::render::{self, EmailContent, EmailOptions, Footer, Tracking};
use crate::state::AppState;

/// The event on the person's timeline when a transactional email goes out.
pub const METRIC: &str = "Received Transactional Email";

const MAX_HTML: usize = 512 * 1024;
const MAX_TEXT: usize = 128 * 1024;
const MAX_VARIABLES: usize = 64 * 1024;
const MAX_METADATA: usize = 8 * 1024;
const MAX_METADATA_KEYS: usize = 50;
const MAX_BLOCKS: usize = 200;

fn yes() -> bool {
    true
}

/// `POST /api/transactional/send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendBody {
    /// One recipient: an email address, or {email, name?, first_name?, last_name?, external_id?, properties?}.
    pub to: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<Uuid>,
    /// Inline content: {subject, preheader?, blocks? | html? | text?}.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    /// Overrides the template's or content's subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(default)]
    pub track_clicks: bool,
    #[serde(default = "yes")]
    pub track_opens: bool,
    /// Add the account's name and postal address at the bottom (no unsubscribe link either way).
    #[serde(default)]
    pub address_footer: bool,
}

/// The one person a send goes to.
#[derive(Debug, Clone)]
struct Recipient {
    email: String,
    first_name: Option<String>,
    last_name: Option<String>,
    external_id: Option<String>,
    properties: Option<Map<String, Value>>,
}

fn short(v: Option<&Value>, k: &str, max: usize) -> AppResult<Option<String>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.chars().count() <= max => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        Some(Value::String(_)) => Err(AppError::bad(format!("to.{k} is too long (at most {max} characters)"))),
        Some(_) => Err(AppError::bad(format!("to.{k} must be text"))),
    }
}

fn parse_to(v: &Value) -> AppResult<Recipient> {
    let one = "a transactional email goes to exactly one person; send one request per recipient";
    let (raw, o) = match v {
        Value::String(s) => (s.clone(), None),
        Value::Object(o) => (o.get("email").and_then(Value::as_str).unwrap_or("").to_string(), Some(o)),
        Value::Array(_) => return Err(AppError::bad(one)),
        _ => return Err(AppError::bad("`to` must be an email address or {email, name?, ...}")),
    };
    if raw.contains(',') || raw.contains(';') || raw.split_whitespace().count() > 1 {
        return Err(AppError::bad(one));
    }
    let email = normalize_email(&raw).ok_or_else(|| AppError::bad(format!("{raw:?} is not a valid email address")))?;
    let Some(o) = o else { return Ok(Recipient { email, first_name: None, last_name: None, external_id: None, properties: None }) };
    for k in o.keys() {
        if !matches!(k.as_str(), "email" | "name" | "first_name" | "last_name" | "external_id" | "properties") {
            return Err(AppError::bad(format!("unknown field to.{k}")));
        }
    }
    let name = short(o.get("name"), "name", 200)?;
    let (mut first, mut last) = (short(o.get("first_name"), "first_name", 100)?, short(o.get("last_name"), "last_name", 100)?);
    if first.is_none() && last.is_none() {
        if let Some(n) = name {
            let mut parts = n.splitn(2, ' ');
            first = parts.next().map(str::to_string);
            last = parts.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        }
    }
    let properties = match o.get("properties") {
        None | Some(Value::Null) => None,
        Some(Value::Object(p)) if p.len() <= MAX_METADATA_KEYS && serde_json::to_string(p).map_or(0, |s| s.len()) <= MAX_METADATA => Some(p.clone()),
        Some(Value::Object(_)) => return Err(AppError::bad("to.properties is too big (at most 50 keys and 8 KB)")),
        Some(_) => return Err(AppError::bad("to.properties must be an object")),
    };
    Ok(Recipient { email, first_name: first, last_name: last, external_id: short(o.get("external_id"), "external_id", 200)?, properties })
}

/// Size and shape checks that need no database.
fn check_body(b: &SendBody) -> AppResult<()> {
    if b.template_id.is_some() == b.content.is_some() {
        return Err(AppError::bad("give either template_id or content"));
    }
    if let Some(k) = &b.idempotency_key {
        if k.is_empty() || k.len() > 255 || k.chars().any(|c| c.is_control()) {
            return Err(AppError::bad("idempotency_key must be 1-255 printable characters"));
        }
    }
    if b.subject.as_deref().is_some_and(|s| s.chars().count() > 998) {
        return Err(AppError::bad("subject is too long"));
    }
    if b.variables.as_ref().is_some_and(|v| serde_json::to_string(v).map_or(0, |s| s.len()) > MAX_VARIABLES) {
        return Err(AppError::bad("variables are too big (at most 64 KB)"));
    }
    if let Some(m) = &b.metadata {
        if m.len() > MAX_METADATA_KEYS || serde_json::to_string(m).map_or(0, |s| s.len()) > MAX_METADATA {
            return Err(AppError::bad("metadata is too big (at most 50 keys and 8 KB)"));
        }
    }
    if let Some(c) = &b.content {
        let o = c.as_object().ok_or_else(|| AppError::bad("content must be an object"))?;
        let len = |k: &str| o.get(k).and_then(Value::as_str).map_or(0, str::len);
        if len("html") > MAX_HTML || len("text") > MAX_TEXT {
            return Err(AppError::bad("content is too big (html at most 512 KB, text at most 128 KB)"));
        }
        if o.get("blocks").and_then(Value::as_array).is_some_and(|b| b.len() > MAX_BLOCKS) {
            return Err(AppError::bad("content has too many blocks (at most 200)"));
        }
    }
    for (k, v) in [("from_email", &b.from_email), ("from_name", &b.from_name), ("reply_to", &b.reply_to)] {
        if v.as_deref().is_some_and(|s| s.len() > 320 || s.contains(['\r', '\n'])) {
            return Err(AppError::bad(format!("{k} is invalid")));
        }
    }
    parse_to(&b.to)?;
    Ok(())
}

/// The content to send: the template's (it must be a transactional template) or the inline content,
/// with the subject override applied, after the promotional-content review.
async fn resolve_content(db: &PgPool, account_id: Uuid, b: &SendBody) -> AppResult<(Value, Option<Uuid>)> {
    let mut content = match b.template_id {
        Some(id) => {
            let row: Option<(Value, bool, String)> = sqlx::query_as("SELECT content, transactional, channel FROM templates WHERE id = $1 AND account_id = $2")
                .bind(id)
                .bind(account_id)
                .fetch_optional(db)
                .await?;
            let (content, transactional, channel) = row.ok_or_else(|| AppError::not_found("template"))?;
            if !transactional || channel != "email" {
                return Err(AppError::bad("that template isn't a transactional email template; create one with POST /api/transactional/templates"));
            }
            content
        }
        None => b.content.clone().unwrap_or(json!({})),
    };
    if let (Some(s), Some(o)) = (&b.subject, content.as_object_mut()) {
        o.insert("subject".into(), json!(s));
    }
    let r = review::review(&content);
    if r.blocked {
        return Err(AppError::bad(review::blocked_message(&r)));
    }
    let o = content.as_object().ok_or_else(|| AppError::bad("content must be an object"))?;
    if o.get("subject").and_then(Value::as_str).is_none_or(|s| s.trim().is_empty()) {
        return Err(AppError::bad("give the email a subject"));
    }
    let has = |k: &str| o.get(k).and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty());
    if !has("html") && !has("text") && o.get("blocks").and_then(Value::as_array).is_none_or(|b| b.is_empty()) {
        return Err(AppError::bad("give the email blocks, html or text"));
    }
    Ok((content, b.template_id))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The rendered email for this person (`message_id` is used for tracking links).
fn render_for(state: &AppState, account: &Account, profile: &Profile, b: &SendBody, content: &Value, message_id: Uuid) -> AppResult<OutgoingEmail> {
    let mut ec: EmailContent = serde_json::from_value(content.clone())?;
    let text_only = content.get("text").and_then(Value::as_str).filter(|t| !t.trim().is_empty());
    let has_body = ec.blocks.as_ref().is_some_and(|b| !b.is_empty()) || ec.html.as_deref().is_some_and(|h| !h.trim().is_empty());
    if !has_body {
        if let Some(t) = text_only {
            ec.html = Some(t.split("\n\n").map(|p| format!("<p>{}</p>", escape(p).replace('\n', "<br>"))).collect());
        }
    }
    let mut ctx = render::context(account, Some(profile), None, "");
    if let (Some(vars), Some(root)) = (&b.variables, ctx.as_object_mut()) {
        for (k, v) in vars {
            if !matches!(k.as_str(), "person" | "organization" | "unsubscribe_url") {
                root.insert(k.clone(), v.clone());
            }
        }
        root.insert("variables".into(), Value::Object(vars.clone()));
    }
    let link_base = crate::domains::link_base(state, account);
    let tracking = Tracking { public_url: &link_base, secret: &state.cfg.secret_key, message_id, unsubscribe_url: String::new() };
    let opts = EmailOptions { footer: if b.address_footer { Footer::Address } else { Footer::None }, track_clicks: b.track_clicks, track_opens: b.track_opens };
    let mut r = render::render_email_with(account, &ec, &ctx, Some(&tracking), opts)?;
    if let (Some(t), true) = (text_only, has_body) {
        r.text = render::render_str(t, &ctx, false)?;
    }
    let from_email = b
        .from_email
        .clone()
        .or(ec.from_email.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| account.from_email.clone())
        .ok_or_else(|| AppError::bad("set a sender email in Settings → Sending, or pass from_email"))?;
    if normalize_email(&from_email).is_none() {
        return Err(AppError::bad("from_email is not a valid email address"));
    }
    crate::sending::check_sender_domain(state, account, &from_email)?;
    Ok(OutgoingEmail {
        message_id,
        to: profile.email.clone().unwrap_or_default(),
        from_name: b.from_name.clone().or(ec.from_name.clone()).filter(|s| !s.is_empty()).or_else(|| account.from_name.clone()).unwrap_or_else(|| account.name.clone()),
        from_email,
        reply_to: b.reply_to.clone().or(ec.reply_to.clone()).filter(|s| !s.is_empty()).or_else(|| account.reply_to.clone()),
        subject: r.subject,
        html: r.html,
        text: r.text,
        unsubscribe_url: None,
    })
}

/// A profile for someone who has none yet, for dry runs (nothing is saved).
fn unsaved_profile(account_id: Uuid, r: &Recipient) -> Profile {
    let now = Utc::now();
    Profile {
        id: Uuid::nil(),
        account_id,
        email: Some(r.email.clone()),
        phone_number: None,
        external_id: r.external_id.clone(),
        first_name: r.first_name.clone(),
        last_name: r.last_name.clone(),
        properties: Value::Object(r.properties.clone().unwrap_or_default()),
        email_consent: "never_subscribed".into(),
        sms_consent: "never_subscribed".into(),
        created_at: now,
        updated_at: now,
        last_event_at: None,
    }
}

fn input(r: &Recipient) -> ProfileInput {
    ProfileInput {
        email: Some(r.email.clone()),
        external_id: r.external_id.clone(),
        first_name: r.first_name.clone(),
        last_name: r.last_name.clone(),
        properties: r.properties.clone(),
        ..Default::default()
    }
}

/// Why this address can't get email at all (hard bounce or spam complaint), if it can't.
async fn suppressed(db: &PgPool, profile_id: Uuid) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar::<_, Option<String>>("SELECT email_suppressed_reason FROM profiles WHERE id = $1").bind(profile_id).fetch_one(db).await
}

fn suppressed_message(reason: &str) -> String {
    match reason {
        "complaint" => "not sent: this address marked an earlier email as spam".into(),
        _ => "not sent: this address hard bounced, so email to it can't be delivered".into(),
    }
}

fn request_hash(b: &SendBody) -> String {
    let v = serde_json::to_value(b).unwrap_or(Value::Null);
    crate::auth::sha256_hex(&v.to_string())
}

/// Send one transactional email. Returns the send record and whether it was replayed from an
/// earlier request with the same idempotency key.
pub async fn send(state: &AppState, account_id: Uuid, b: SendBody, via: &str, resent_from: Option<Uuid>) -> AppResult<(Value, bool)> {
    check_body(&b)?;
    let hash = request_hash(&b);
    if let Some(key) = &b.idempotency_key {
        if let Some(prior) = replay(&state.db, account_id, key, &hash).await? {
            return Ok((prior, true));
        }
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    guard::check_open(state, &account).await?;
    let (content, template_id) = resolve_content(&state.db, account_id, &b).await?;
    // Checked before anything is saved, so a bad sender never leaves a record.
    if let Some(f) = b.from_email.as_deref().or(content.get("from_email").and_then(Value::as_str)).filter(|s| !s.is_empty()).or(account.from_email.as_deref()) {
        crate::sending::check_sender_domain(state, &account, f)?;
    }
    let to = parse_to(&b.to)?;
    let id = Uuid::now_v7();
    let reserved = guard::reserve(state, account_id, id, &to.email, template_id, &b, &hash, via, resent_from).await?;
    if !reserved {
        // Lost a race with the same idempotency key: the first one's result stands.
        let key = b.idempotency_key.as_deref().unwrap_or_default();
        return Ok((replay(&state.db, account_id, key, &hash).await?.ok_or_else(|| AppError::Conflict("please retry".into()))?, true));
    }
    match deliver(state, &account, id, &b, &to, &content).await {
        Ok(()) => Ok((get(&state.db, account_id, id).await?, false)),
        Err(e) => {
            // Nothing went out: drop the record so a corrected retry with the same key can send.
            sqlx::query("DELETE FROM transactional_sends WHERE id = $1 AND status = 'queued' AND message_id IS NULL").bind(id).execute(&state.db).await?;
            Err(e)
        }
    }
}

/// The earlier send for this idempotency key within 24 hours, if any. A key reused for a different
/// request is an error; a key older than 24 hours is released for reuse.
async fn replay(db: &PgPool, account_id: Uuid, key: &str, hash: &str) -> AppResult<Option<Value>> {
    let row: Option<(Uuid, String, DateTime<Utc>)> = sqlx::query_as("SELECT id, request_hash, created_at FROM transactional_sends WHERE account_id = $1 AND idempotency_key = $2")
        .bind(account_id)
        .bind(key)
        .fetch_optional(db)
        .await?;
    let Some((id, prior_hash, created)) = row else { return Ok(None) };
    if created < Utc::now() - chrono::Duration::hours(24) {
        sqlx::query("UPDATE transactional_sends SET idempotency_key = NULL WHERE id = $1").bind(id).execute(db).await?;
        return Ok(None);
    }
    if prior_hash != hash {
        return Err(AppError::Conflict("this idempotency_key was already used for a different email in the last 24 hours".into()));
    }
    Ok(Some(get(db, account_id, id).await?))
}

/// Resolve the person, honor suppression, render, check the balance and send. Errors mean nothing
/// went out; a suppressed address is recorded as skipped (not an error).
async fn deliver(state: &AppState, account: &Account, id: Uuid, b: &SendBody, to: &Recipient, content: &Value) -> AppResult<()> {
    let profile = crate::audience::upsert_profile(&state.db, account.id, &input(to)).await?.profile;
    sqlx::query("UPDATE transactional_sends SET profile_id = $2 WHERE id = $1").bind(id).bind(profile.id).execute(&state.db).await?;
    if let Some(reason) = suppressed(&state.db, profile.id).await? {
        sqlx::query("UPDATE transactional_sends SET status = 'skipped', error = $2 WHERE id = $1").bind(id).bind(suppressed_message(&reason)).execute(&state.db).await?;
        return Ok(());
    }
    let message_id = Uuid::now_v7();
    let email = render_for(state, account, &profile, b, content, message_id)?;
    let price = crate::billing::quote(state, account.id, "email", 1, false, false).await?;
    if price > 0 && crate::billing::balance(&state.db, account.id).await? < price {
        return Err(AppError::LimitReached("your balance is empty; add credit in Settings → Billing".into()));
    }
    sqlx::query(
        "INSERT INTO messages (id, account_id, profile_id, channel, to_address, subject, transactional) VALUES ($1, $2, $3, 'email', $4, $5, true)",
    )
    .bind(message_id)
    .bind(account.id)
    .bind(profile.id)
    .bind(&email.to)
    .bind(&email.subject)
    .execute(&state.db)
    .await?;
    sqlx::query("UPDATE transactional_sends SET message_id = $2, subject = $3 WHERE id = $1").bind(id).bind(message_id).bind(&email.subject).execute(&state.db).await?;
    send_now(state, account, id, message_id, profile.id, &email, b.metadata.as_ref()).await
}

/// Hand the rendered email to the provider and record the outcome. A busy provider leaves the send
/// queued and retries it from a job.
async fn send_now(state: &AppState, account: &Account, id: Uuid, message_id: Uuid, profile_id: Uuid, email: &OutgoingEmail, metadata: Option<&Map<String, Value>>) -> AppResult<()> {
    crate::sending::throttle_email(state).await;
    match state.email.send(email).await {
        Ok(provider_id) => {
            let cost = crate::billing::charge_message(state, account.id, "email", 1, false, false).await?;
            sqlx::query("UPDATE messages SET status = 'sent', provider_message_id = $2, sent_at = now(), cost_micros = $3 WHERE id = $1")
                .bind(message_id)
                .bind(&provider_id)
                .bind(cost)
                .execute(&state.db)
                .await?;
            // Counts toward the bounce and complaint rates that pause an account, not the warm-up limit.
            crate::deliverability::record_confirmation_send(&state.db, message_id, &provider_id, account.id).await?;
            sqlx::query("UPDATE transactional_sends SET status = 'sent', sent_at = now(), error = NULL WHERE id = $1").bind(id).execute(&state.db).await?;
            record_event(
                &state.db,
                NewEvent {
                    account_id: account.id,
                    profile_id,
                    metric: METRIC,
                    properties: json!({"subject": email.subject, "message_id": message_id, "transactional_send_id": id, "metadata": metadata}),
                    value: None,
                    occurred_at: None,
                    unique_id: Some(message_id.to_string()),
                    message_id: Some(message_id),
                },
            )
            .await?;
            Ok(())
        }
        Err(e) => {
            if let Some(busy) = e.downcast_ref::<crate::providers::email::ProviderBusy>() {
                let wait = chrono::Duration::from_std(busy.retry_after).unwrap_or(chrono::Duration::minutes(5)).min(chrono::Duration::hours(1));
                tracing::warn!(send = %id, reason = %busy.reason, "email provider busy; transactional email will retry");
                crate::jobs::enqueue(&state.db, super::RETRY_JOB, json!({"send_id": id}), Some(Utc::now() + wait)).await?;
                return Ok(());
            }
            let msg = format!("{e:#}");
            tracing::warn!(send = %id, error = %msg, "transactional send failed");
            sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1").bind(message_id).bind(&msg).execute(&state.db).await?;
            sqlx::query("UPDATE transactional_sends SET status = 'failed', error = $2 WHERE id = $1").bind(id).bind(&msg).execute(&state.db).await?;
            Ok(())
        }
    }
}

/// Job: retry a send the provider was too busy to take. Re-renders from the stored request.
pub async fn retry(state: &AppState, id: Uuid) -> anyhow::Result<()> {
    let row: Option<(Uuid, Value, Option<Uuid>, Option<Uuid>, String)> =
        sqlx::query_as("SELECT account_id, request, message_id, profile_id, status FROM transactional_sends WHERE id = $1").bind(id).fetch_optional(&state.db).await?;
    let Some((account_id, request, message_id, profile_id, status)) = row else { return Ok(()) };
    let (Some(message_id), Some(profile_id), "queued") = (message_id, profile_id, status.as_str()) else { return Ok(()) };
    let fail = |msg: String| async move {
        sqlx::query("UPDATE transactional_sends SET status = 'failed', error = $2 WHERE id = $1").bind(id).bind(&msg).execute(&state.db).await?;
        sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1").bind(message_id).bind(&msg).execute(&state.db).await?;
        anyhow::Ok(())
    };
    let account = crate::campaigns::load_account(&state.db, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if let Err(e) = guard::check_paused(state, &account).await {
        return fail(e.public_message()).await;
    }
    let b: SendBody = serde_json::from_value(request)?;
    let q = format!("SELECT {} FROM profiles p WHERE p.id = $1", crate::models::PROFILE_COLS);
    let Some(profile) = sqlx::query_as::<_, Profile>(&q).bind(profile_id).fetch_optional(&state.db).await? else { return fail("profile deleted".into()).await };
    if let Some(reason) = suppressed(&state.db, profile.id).await? {
        return fail(suppressed_message(&reason)).await;
    }
    let email = match resolve_content(&state.db, account_id, &b).await.and_then(|(c, _)| render_for(state, &account, &profile, &b, &c, message_id)) {
        Ok(e) => e,
        Err(e) => return fail(e.public_message()).await,
    };
    send_now(state, &account, id, message_id, profile.id, &email, b.metadata.as_ref()).await.map_err(|e| anyhow::anyhow!(e.public_message()))
}

/// Dry run: what would be sent, without sending or saving anything.
pub async fn dry_run(state: &AppState, account_id: Uuid, b: SendBody, include_html: bool) -> AppResult<Value> {
    check_body(&b)?;
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let open = guard::check_open(state, &account).await.err().map(|e| e.public_message());
    let (content, _) = resolve_content(&state.db, account_id, &b).await?;
    let to = parse_to(&b.to)?;
    let existing = crate::audience::find_profile(&state.db, account_id, &input(&to)).await?;
    let blocked = match &existing {
        Some(p) => suppressed(&state.db, p.id).await?.map(|r| suppressed_message(&r)),
        None => None,
    };
    let profile = existing.unwrap_or_else(|| unsaved_profile(account_id, &to));
    let email = render_for(state, &account, &profile, &b, &content, Uuid::nil())?;
    let r = review::review(&content);
    let mut out = json!({
        "dry_run": true,
        "to": email.to, "from": {"email": email.from_email, "name": email.from_name}, "reply_to": email.reply_to,
        "subject": email.subject, "text": email.text, "html_bytes": email.html.len(),
        "review_warnings": r.flags,
        "would_send": open.is_none() && blocked.is_none(),
        "not_sent_because": open.or(blocked),
        "note": "Nothing sent. Call again with confirm=true to send it.",
    });
    if include_html {
        out["html"] = json!(email.html);
    }
    Ok(out)
}

/// One send as the API returns it, with delivery and engagement from its message.
pub async fn get(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let rows = list_rows(db, account_id, Some(id), None, None, 1, 0).await?;
    let mut v = rows.into_iter().next().ok_or_else(|| AppError::not_found("transactional send"))?;
    let (request, resent_from): (Value, Option<Uuid>) = sqlx::query_as("SELECT request, resent_from FROM transactional_sends WHERE id = $1").bind(id).fetch_one(db).await?;
    v["request"] = request;
    v["resent_from"] = json!(resent_from);
    Ok(v)
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    to_email: String,
    subject: Option<String>,
    status: String,
    error: Option<String>,
    template_id: Option<Uuid>,
    profile_id: Option<Uuid>,
    message_id: Option<Uuid>,
    idempotency_key: Option<String>,
    metadata: Value,
    track_clicks: bool,
    via: String,
    created_at: DateTime<Utc>,
    sent_at: Option<DateTime<Utc>>,
    message_status: Option<String>,
    delivered_at: Option<DateTime<Utc>>,
    opened_at: Option<DateTime<Utc>>,
    clicked_at: Option<DateTime<Utc>>,
    bounce_type: Option<String>,
}

/// The send's state in one word: queued, sent, delivered, opened, clicked, bounced, complained, skipped or failed.
fn state_of(r: &Row) -> &str {
    match r.message_status.as_deref() {
        Some("bounced") => "bounced",
        Some("complained") => "complained",
        Some("failed") => "failed",
        _ if r.clicked_at.is_some() => "clicked",
        _ if r.opened_at.is_some() => "opened",
        Some("delivered") => "delivered",
        _ => &r.status,
    }
}

pub async fn list_rows(db: &PgPool, account_id: Uuid, id: Option<Uuid>, q: Option<&str>, status: Option<&str>, limit: i64, offset: i64) -> AppResult<Vec<Value>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT t.id, t.to_email, t.subject, t.status, t.error, t.template_id, t.profile_id, t.message_id, t.idempotency_key, t.metadata,
                t.track_clicks, t.via, t.created_at, t.sent_at, m.status AS message_status, m.delivered_at, m.opened_at, m.clicked_at, m.bounce_type
         FROM transactional_sends t LEFT JOIN messages m ON m.id = t.message_id
         WHERE t.account_id = $1 AND ($2::uuid IS NULL OR t.id = $2)
           AND ($3::text IS NULL OR t.to_email LIKE '%' || $3 || '%' OR t.subject ILIKE '%' || $3 || '%')
           AND ($4::text IS NULL OR $4 = CASE WHEN m.status IN ('bounced','complained','failed') THEN m.status
                                              WHEN m.clicked_at IS NOT NULL THEN 'clicked' WHEN m.opened_at IS NOT NULL THEN 'opened'
                                              WHEN m.status = 'delivered' THEN 'delivered' ELSE t.status END)
         ORDER BY t.created_at DESC LIMIT $5 OFFSET $6",
    )
    .bind(account_id)
    .bind(id)
    .bind(q.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).map(|s| s.replace('%', "\\%").replace('_', "\\_")))
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id, "to": r.to_email, "subject": r.subject, "status": state_of(r), "error": r.error,
                "template_id": r.template_id, "profile_id": r.profile_id, "message_id": r.message_id,
                "idempotency_key": r.idempotency_key, "metadata": r.metadata, "track_clicks": r.track_clicks, "via": r.via,
                "created_at": r.created_at, "sent_at": r.sent_at, "delivered_at": r.delivered_at, "opened_at": r.opened_at,
                "clicked_at": r.clicked_at, "bounce_type": r.bounce_type,
            })
        })
        .collect())
}

/// Send the same email again to the same person (a new send, checked like any other).
pub async fn resend(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let row: Option<Value> = sqlx::query_scalar("SELECT request FROM transactional_sends WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).fetch_optional(&state.db).await?;
    let mut b: SendBody = serde_json::from_value(row.ok_or_else(|| AppError::not_found("transactional send"))?)?;
    b.idempotency_key = None;
    Ok(send(state, account_id, b, "resend", Some(id)).await?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_recipient_only() {
        assert_eq!(parse_to(&json!("Ann@Example.com")).unwrap().email, "ann@example.com");
        assert!(parse_to(&json!(["a@x.test", "b@x.test"])).is_err());
        assert!(parse_to(&json!("a@x.test, b@x.test")).is_err());
        assert!(parse_to(&json!("a@x.test b@x.test")).is_err());
        assert!(parse_to(&json!({"email": "a@x.test", "cc": "b@x.test"})).is_err());
        let r = parse_to(&json!({"email": "a@x.test", "name": "Ann Lee Smith"})).unwrap();
        assert_eq!((r.first_name.as_deref(), r.last_name.as_deref()), (Some("Ann"), Some("Lee Smith")));
        assert!(parse_to(&json!("nope")).is_err());
    }
}
