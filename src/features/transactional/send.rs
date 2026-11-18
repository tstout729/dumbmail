//! Sending one transactional email. A send is addressed to a mailbox: it always goes to exactly the
//! address that was asked for, on every attempt, and is linked to a profile only by that email
//! (never changing the profile). Each attempt claims the send before handing it to the provider, is
//! billed once by the send's id, and an attempt cut off after the hand-off is marked `unknown`
//! rather than risk a second password reset. Variables and rendered bodies are never kept in the
//! clear: a retry renders from an encrypted payload that is deleted once the send is final.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::guard::{self, Access, Reservation, Reserved};
use super::review;
use crate::audience::{record_event, NewEvent};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, Account, Profile, PROFILE_COLS};
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
/// Restricted mode: each variable at most this many characters, and all of them this many bytes.
pub const RESTRICTED_VARIABLE_CHARS: usize = 500;
pub const RESTRICTED_VARIABLES_BYTES: usize = 4 * 1024;

/// Attempts before a send the provider keeps refusing as busy is given up.
pub const MAX_ATTEMPTS: i32 = 6;
/// An attempt still 'sending' after this long was cut off; it's marked unknown, never retried.
pub const CLAIM_TIMEOUT_MINUTES: i32 = 10;
/// The job written with each reservation runs this long after it, in case the request never gets to send.
pub const BACKSTOP_SECS: i64 = 120;

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

/// The one mailbox a send goes to, and the names and fields used to fill in this email.
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
    let email = mailbox(&raw).ok_or_else(|| AppError::bad(format!("{raw:?} is not a valid email address")))?;
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

/// A normalized address that's safe in a mail header (no control characters, at most 320 bytes).
fn mailbox(s: &str) -> Option<String> {
    if s.len() > 320 || s.chars().any(char::is_control) {
        return None;
    }
    normalize_email(s)
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
        if v.as_deref().is_some_and(|s| s.len() > 320 || s.chars().any(char::is_control)) {
            return Err(AppError::bad(format!("{k} is invalid")));
        }
    }
    parse_to(&b.to)?;
    Ok(())
}

/// What this account's mode allows for this request. Restricted mode (hosted accounts without full
/// mode) sends only saved transactional templates, as saved: no inline content, no subject override,
/// and only short variables, so a template that passed the content check can't be turned into
/// something else at send time.
fn check_mode(state: &AppState, access: &Access, b: &SendBody) -> AppResult<()> {
    access.allows(state, b.content.is_some())?;
    if access.full {
        return Ok(());
    }
    let restricted = |what: String| AppError::Forbidden(format!("This account's transactional email is in restricted mode: {what}."));
    if b.subject.is_some() {
        return Err(restricted("the subject comes from the saved template and can't be changed when sending".into()));
    }
    if let Some(vars) = &b.variables {
        for (k, v) in vars {
            let len = match v {
                Value::String(s) => s.chars().count(),
                other => other.to_string().chars().count(),
            };
            if len > RESTRICTED_VARIABLE_CHARS {
                return Err(restricted(format!("each variable can be at most {RESTRICTED_VARIABLE_CHARS} characters, and {k:?} is {len}")));
            }
        }
        if serde_json::to_string(vars)?.len() > RESTRICTED_VARIABLES_BYTES {
            return Err(restricted("the variables together can be at most 4 KB".into()));
        }
    }
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
    check_review(&content)?;
    if let Some(p) = crate::features::content::prepare::transactional_problem(&content) {
        return Err(AppError::bad(p));
    }
    // Coupon codes aren't supported in transactional email (templates saved before that rule included).
    crate::features::coupons::template::forbid(&content)?;
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

fn check_review(content: &Value) -> AppResult<()> {
    let r = review::review(content);
    if r.blocked { Err(AppError::bad(review::blocked_message(&r))) } else { Ok(()) }
}

/// The review again on what would actually go out, so variables can't carry a promotion past it.
fn check_rendered(email: &OutgoingEmail) -> AppResult<()> {
    check_review(&json!({"subject": email.subject, "from_name": email.from_name, "html": email.html, "text": email.text}))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The person as this email sees them: the linked profile, with the names and fields given in `to`
/// for this send only (nothing is saved).
fn person(base: &Profile, r: &Recipient) -> Profile {
    let mut p = base.clone();
    p.email = Some(r.email.clone());
    if r.first_name.is_some() || r.last_name.is_some() {
        p.first_name = r.first_name.clone();
        p.last_name = r.last_name.clone();
    }
    if r.external_id.is_some() {
        p.external_id = r.external_id.clone();
    }
    if let (Some(extra), Some(props)) = (&r.properties, p.properties.as_object_mut()) {
        props.extend(extra.clone());
    }
    p
}

/// Reply-To, wherever it came from (the request, the content or the account's default), must be the
/// sender address of this email or on one of the account's verified sending domains (on the hosted
/// service and with SES), so replies can't be routed to an unrelated mailbox. `from_email` has been
/// checked already.
fn check_reply_to(state: &AppState, account: &Account, addr: &str, from_email: &str) -> AppResult<String> {
    let n = mailbox(addr).ok_or_else(|| AppError::bad("reply_to is not a valid email address"))?;
    if n != from_email && (state.cfg.hosted || state.email.name() == "ses") && crate::sending::check_sender_domain_name(account, &n).is_err() {
        let domain = n.rsplit_once('@').map_or("", |(_, d)| d);
        let own = account.reply_to.as_deref().and_then(normalize_email).as_deref() == Some(n.as_str());
        let whose = if own { "Your account's reply-to address" } else { "reply_to" };
        return Err(AppError::bad(format!("{whose} {n} must be on one of your verified sending domains (verify {domain} in Settings → Domains), or pass a reply_to that is")));
    }
    Ok(n)
}

/// The rendered email for this person (`message_id` is used for tracking links). Sender, name and
/// Reply-To are checked as finally resolved (request, then content, then account), before any MIME.
fn render_for(state: &AppState, account: &Account, base: &Profile, to: &Recipient, b: &SendBody, content: &Value, message_id: Uuid) -> AppResult<OutgoingEmail> {
    let mut ec: EmailContent = serde_json::from_value(content.clone())?;
    let text_only = content.get("text").and_then(Value::as_str).filter(|t| !t.trim().is_empty());
    let has_body = ec.blocks.as_ref().is_some_and(|b| !b.is_empty()) || ec.html.as_deref().is_some_and(|h| !h.trim().is_empty());
    if !has_body {
        if let Some(t) = text_only {
            ec.html = Some(t.split("\n\n").map(|p| format!("<p>{}</p>", escape(p).replace('\n', "<br>"))).collect());
        }
    }
    let profile = person(base, to);
    let mut ctx = render::context(account, Some(&profile), None, "");
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
    let footer = || if b.address_footer { Footer::Address } else { Footer::None };
    // The content rules read the links as written, so they also review a copy without click tracking.
    let mut plain = render::render_email_with(account, &ec, &ctx, None, EmailOptions { footer: footer(), track_clicks: false, track_opens: false })?;
    let mut r = render::render_email_with(account, &ec, &ctx, Some(&tracking), EmailOptions { footer: footer(), track_clicks: b.track_clicks, track_opens: b.track_opens })?;
    if let (Some(t), true) = (text_only, has_body) {
        r.text = render::render_str(t, &ctx, false)?;
        plain.text = r.text.clone();
    }
    let from_email = b
        .from_email
        .clone()
        .or(ec.from_email.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| account.from_email.clone())
        .ok_or_else(|| AppError::bad("set a sender email in Settings → Sending, or pass from_email"))?;
    let from_email = mailbox(&from_email).ok_or_else(|| AppError::bad("from_email is not a valid email address"))?;
    crate::sending::check_sender_domain(state, account, &from_email)?;
    let from_name = b.from_name.clone().or(ec.from_name.clone()).filter(|s| !s.is_empty()).or_else(|| account.from_name.clone()).unwrap_or_else(|| account.name.clone());
    if from_name.chars().count() > 200 || from_name.chars().any(char::is_control) {
        return Err(AppError::bad("from_name is invalid (at most 200 characters, one line)"));
    }
    let reply_to = match b.reply_to.clone().or(ec.reply_to.clone()).filter(|s| !s.trim().is_empty()).or_else(|| account.reply_to.clone()) {
        Some(r) => Some(check_reply_to(state, account, &r, &from_email)?),
        None => None,
    };
    let email = OutgoingEmail { message_id, to: to.email.clone(), from_name, from_email, reply_to, subject: r.subject, html: r.html, text: r.text, unsubscribe_url: None };
    // The exact subject, sender name, HTML and plain text that will go out, and the HTML as written.
    check_rendered(&email)?;
    check_review(&json!({"subject": email.subject, "from_name": email.from_name, "html": plain.html, "text": plain.text}))?;
    Ok(email)
}

/// The account's profile with exactly this email, if there is one.
async fn profile_by_email(db: &PgPool, account_id: Uuid, email: &str) -> sqlx::Result<Option<Profile>> {
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.account_id = $1 AND p.email = $2");
    sqlx::query_as::<_, Profile>(&q).bind(account_id).bind(email).fetch_optional(db).await
}

/// The profile this send is linked to: the one with this email, or a new one with just the email
/// and name. An existing profile is never changed (identifiers, names, properties and consent stay as
/// they are), and other identifiers in `to` never merge or move anything.
async fn link_profile(db: &PgPool, account_id: Uuid, to: &Recipient) -> AppResult<Profile> {
    if let Some(p) = profile_by_email(db, account_id, &to.email).await? {
        return Ok(p);
    }
    let q = format!(
        "INSERT INTO profiles AS p (id, account_id, email, first_name, last_name) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (account_id, email) WHERE email IS NOT NULL DO NOTHING RETURNING {PROFILE_COLS}"
    );
    let created = sqlx::query_as::<_, Profile>(&q).bind(Uuid::now_v7()).bind(account_id).bind(&to.email).bind(&to.first_name).bind(&to.last_name).fetch_optional(db).await?;
    match created {
        Some(p) => Ok(p),
        // Someone else created it a moment ago.
        None => profile_by_email(db, account_id, &to.email).await?.ok_or_else(|| AppError::Conflict("please retry".into())),
    }
}

/// A profile for someone who has none yet, for dry runs (nothing is saved).
fn unsaved_profile(account_id: Uuid, email: &str) -> Profile {
    let now = Utc::now();
    Profile {
        id: Uuid::nil(),
        account_id,
        email: Some(email.to_string()),
        phone_number: None,
        external_id: None,
        first_name: None,
        last_name: None,
        properties: json!({}),
        email_consent: "never_subscribed".into(),
        sms_consent: "never_subscribed".into(),
        created_at: now,
        updated_at: now,
        last_event_at: None,
        locale: None,
    }
}

/// Why mailbox $2 of account $1 can't get email (see [`suppressed_address`]), as an SQL expression.
/// Fast with the background index `messages_bad_address_idx` (see `super::INDEXES`), correct without it.
const SUPPRESSED: &str = "COALESCE(
            (SELECT email_suppressed_reason FROM profiles WHERE account_id = $1 AND email = $2),
            (SELECT CASE WHEN bool_or(status = 'complained') THEN 'complaint' ELSE 'hard_bounce' END FROM messages
             WHERE account_id = $1 AND to_address = $2 AND (status = 'complained' OR (status = 'bounced' AND bounce_type = 'Permanent'))
             HAVING count(*) > 0))";

/// Why this mailbox can't get email at all, if it can't: its profile is suppressed, or mail to this
/// address hard bounced or drew a spam complaint (even under a profile that has since changed email).
async fn suppressed_address(db: &PgPool, account_id: Uuid, email: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(&format!("SELECT {SUPPRESSED}"))
    .bind(account_id)
    .bind(email)
    .fetch_one(db)
    .await
}

fn suppressed_message(reason: &str) -> String {
    match reason {
        "complaint" => "not sent: this address marked an earlier email as spam".into(),
        _ => "not sent: this address hard bounced, so email to it can't be delivered".into(),
    }
}

/// Fingerprint of a request for idempotency, keyed with SECRET_KEY so short codes in the variables
/// can't be guessed back from it.
fn request_hash(secret: &str, b: &SendBody) -> String {
    let v = serde_json::to_value(b).unwrap_or(Value::Null);
    crate::auth::sign(secret, &crate::auth::sha256_hex(&v.to_string())).rsplit_once('.').map_or_else(String::new, |(_, sig)| sig.to_string())
}

async fn balance_ok(state: &AppState, account_id: Uuid) -> AppResult<bool> {
    let price = crate::billing::quote(state, account_id, "email", 1, false, false).await?;
    Ok(price <= 0 || crate::billing::balance(&state.db, account_id).await? >= price)
}

const NO_BALANCE: &str = "your balance is empty; add credit in Settings → Billing";

/// Send one transactional email. Returns the send record and whether it was replayed from an
/// earlier request with the same idempotency key. Anything refused before the send is recorded
/// leaves no record, so a corrected retry with the same key can send.
pub async fn send(state: &AppState, ctx: &Ctx, b: SendBody, via: &str) -> AppResult<(Value, bool)> {
    check_body(&b)?;
    let account_id = ctx.account_id;
    let hash = request_hash(&state.cfg.secret_key, &b);
    if let Some(key) = &b.idempotency_key {
        let mut conn = state.db.acquire().await?;
        if let Some(prior) = guard::find_key(&mut conn, account_id, key, &hash).await? {
            drop(conn);
            return Ok((get(&state.db, account_id, prior).await?, true));
        }
    }
    // Anything refused before the reservation (a cap, the pause, the balance) is checked against the
    // key again under the reservation lock: a request accepted a moment ago is replayed, not refused.
    let checked = async {
        let access = guard::may_send_transactional(state, ctx).await?;
        check_mode(state, &access, &b)?;
        let account = crate::campaigns::load_account(&state.db, account_id).await?;
        guard::check_open(state, &account, &access).await?;
        let (content, template_id) = resolve_content(&state.db, account_id, &b).await?;
        let to = parse_to(&b.to)?;
        let profile = link_profile(&state.db, account_id, &to).await?;
        let message_id = Uuid::now_v7();
        let email = render_for(state, &account, &profile, &to, &b, &content, message_id)?;
        let skipped = suppressed_address(&state.db, account_id, &to.email).await?.map(|r| suppressed_message(&r));
        if skipped.is_none() && !balance_ok(state, account_id).await? {
            return Err(AppError::LimitReached(NO_BALANCE.into()));
        }
        Ok((access, template_id, to, profile, message_id, email, skipped))
    }
    .await;
    let (access, template_id, to, profile, message_id, email, skipped) = match checked {
        Ok(v) => v,
        Err(e) => {
            if let Some(key) = &b.idempotency_key {
                if let Some(prior) = guard::find_key_locked(&state.db, account_id, key, &hash).await? {
                    return Ok((get(&state.db, account_id, prior).await?, true));
                }
            }
            return Err(e);
        }
    };
    let payload_enc = match skipped {
        None => Some(crate::crypto::encrypt(&state.cfg.secret_key, &serde_json::to_string(&b)?)?),
        Some(_) => None,
    };
    let id = Uuid::now_v7();
    let reserved = guard::reserve(
        state,
        account_id,
        &access,
        Reservation {
            id,
            message_id: Some(message_id),
            to: &to.email,
            profile_id: profile.id,
            template_id,
            subject: &email.subject,
            idempotency_key: b.idempotency_key.as_deref(),
            hash: &hash,
            metadata: json!(b.metadata.clone().unwrap_or_default()),
            track_clicks: b.track_clicks,
            via,
            skipped: skipped.clone(),
            payload_enc,
        },
    )
    .await?;
    if let Reserved::Replay(prior) = reserved {
        return Ok((get(&state.db, account_id, prior).await?, true));
    }
    if skipped.is_none() {
        attempt(state, id, Some(email)).await?;
    }
    Ok((get(&state.db, account_id, id).await?, false))
}

/// What an attempt found when it rechecked everything just before sending.
enum Ready {
    Send(Box<(Account, OutgoingEmail, Uuid)>),
    /// Final: not sent.
    Fail(String),
    /// Final: the address can't get email.
    Skip(String),
    /// Try again later (counts as an attempt).
    Wait(chrono::Duration, String),
}

/// Everything rechecked for an attempt: the pause, that it's still on and in its mode, the caps, the
/// address's suppression and the balance, then the email itself (from `rendered`, or re-rendered from
/// the encrypted payload to exactly the address recorded on the send).
#[allow(clippy::too_many_arguments)]
async fn prepare(state: &AppState, account_id: Uuid, profile_id: Option<Uuid>, to_email: &str, attempts: i32, payload: Option<&str>, rendered: Option<OutgoingEmail>, message_id: Uuid) -> AppResult<Ready> {
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    if let Err(e) = guard::check_paused(state, &account).await {
        return Ok(Ready::Fail(e.public_message()));
    }
    let access: Access = match guard::access(state, account_id).await {
        Ok(a) => a,
        Err(AppError::Forbidden(m)) => return Ok(Ready::Fail(m)),
        Err(e) => return Err(e),
    };
    let (minute, day) = guard::usage(&state.db, account_id).await?;
    // This send is already counted in the day; a lowered cap stops it.
    if day > access.daily {
        return Ok(Ready::Fail(format!("not sent: over this account's limit of {} transactional emails in 24 hours", access.daily)));
    }
    if attempts > 1 && minute >= access.per_minute {
        return Ok(Ready::Wait(chrono::Duration::minutes(1), "waiting for the per-minute limit".into()));
    }
    if let Some(reason) = suppressed_address(&state.db, account_id, to_email).await? {
        return Ok(Ready::Skip(suppressed_message(&reason)));
    }
    if !balance_ok(state, account_id).await? {
        return Ok(Ready::Fail(format!("not sent: {NO_BALANCE}")));
    }
    let email = match rendered {
        Some(e) => e,
        None => {
            let Some(enc) = payload else { return Ok(Ready::Fail("not sent: its content expired before it could go out".into())) };
            let b: SendBody = serde_json::from_str(&crate::crypto::decrypt(&state.cfg.secret_key, enc)?)?;
            if let Err(e) = check_mode(state, &access, &b) {
                return Ok(Ready::Fail(e.public_message()));
            }
            let to = parse_to(&b.to)?;
            let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1 AND p.account_id = $2");
            let profile = match profile_id {
                Some(pid) => sqlx::query_as::<_, Profile>(&q).bind(pid).bind(account_id).fetch_optional(&state.db).await?,
                None => None,
            };
            let Some(profile) = profile else { return Ok(Ready::Fail("not sent: the profile was deleted".into())) };
            match resolve_content(&state.db, account_id, &b).await.and_then(|(c, _)| render_for(state, &account, &profile, &to, &b, &c, message_id)) {
                Ok(e) => e,
                Err(e) => return Ok(Ready::Fail(e.public_message())),
            }
        }
    };
    // Bound to the mailbox asked for: never anyone else, whatever happened to the profile since.
    if email.to != to_email {
        return Ok(Ready::Fail("not sent: the recipient doesn't match the recorded address".into()));
    }
    if let Err(e) = check_rendered(&email) {
        return Ok(Ready::Fail(e.public_message()));
    }
    Ok(Ready::Send(Box::new((account, email, profile_id.unwrap_or_default()))))
}

/// End a claimed send without a confirmed hand-off: `status` is failed or skipped (not sent), or
/// unknown (the provider may have accepted it; never sent again and not charged). The retry payload goes.
async fn finish_unsent(db: &PgPool, id: Uuid, message_id: Uuid, status: &str, msg: &str) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE transactional_sends SET status = $2, error = $3, payload_enc = NULL, claimed_at = NULL WHERE id = $1 AND status = 'sending'")
        .bind(id)
        .bind(status)
        .bind(msg)
        .execute(&mut *tx)
        .await?;
    let account: Option<Uuid> = sqlx::query_scalar("UPDATE messages SET status = $2, error = $3 WHERE id = $1 AND status = 'queued' RETURNING account_id")
        .bind(message_id)
        .bind(if status == "skipped" { "skipped" } else { "failed" })
        .bind(msg)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(account) = account {
        crate::features::messages_unsent_in(&mut tx, db, account, &[message_id]).await;
    }
    tx.commit().await
}

/// Put a claimed send back in the queue with its retry job, in one transaction. No attempt can claim
/// it before `next_attempt_at` (the reservation's backstop job included).
async fn requeue(db: &PgPool, id: Uuid, wait: chrono::Duration, msg: &str) -> sqlx::Result<()> {
    let at = Utc::now() + wait;
    let mut tx = db.begin().await?;
    let n = sqlx::query("UPDATE transactional_sends SET status = 'queued', claimed_at = NULL, next_attempt_at = $3, error = $2 WHERE id = $1 AND status = 'sending'")
        .bind(id)
        .bind(format!("waiting to retry: {msg}"))
        .bind(at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 1 {
        crate::jobs::enqueue(&mut *tx, super::RETRY_JOB, json!({"send_id": id}), Some(at)).await?;
    }
    tx.commit().await
}

/// How long to wait before attempt `attempts + 1` when the provider is busy.
fn backoff(attempts: i32, asked: std::time::Duration) -> chrono::Duration {
    let floor = chrono::Duration::minutes(1) * 2i32.pow(attempts.clamp(1, 10) as u32 - 1);
    chrono::Duration::from_std(asked).unwrap_or(floor).max(floor).min(chrono::Duration::hours(1))
}

/// One delivery attempt. It claims the send (queued → sending) so no other attempt can hold it,
/// rechecks everything, hands it to the provider and records the outcome. A send already claimed or
/// final is left alone.
async fn attempt(state: &AppState, id: Uuid, rendered: Option<OutgoingEmail>) -> AppResult<()> {
    let claimed: Option<(Uuid, Option<Uuid>, Option<Uuid>, String, i32, Option<String>, Value)> = sqlx::query_as(
        "UPDATE transactional_sends SET status = 'sending', claimed_at = now(), attempts = attempts + 1
         WHERE id = $1 AND status = 'queued' AND (next_attempt_at IS NULL OR next_attempt_at <= now())
         RETURNING account_id, message_id, profile_id, to_email, attempts, payload_enc, metadata",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, message_id, profile_id, to_email, attempts, payload, metadata)) = claimed else { return Ok(()) };
    let Some(message_id) = message_id else {
        sqlx::query("UPDATE transactional_sends SET status = 'failed', error = 'nothing to send', payload_enc = NULL WHERE id = $1").bind(id).execute(&state.db).await?;
        return Ok(());
    };
    // A closed account sends nothing; the send and its message are settled, not left queued.
    if crate::models::account_closed(&state.db, account_id).await? {
        return Ok(finish_unsent(&state.db, id, message_id, "skipped", "account closed").await?);
    }
    let (account, email, profile_id) = match prepare(state, account_id, profile_id, &to_email, attempts, payload.as_deref(), rendered, message_id).await? {
        Ready::Send(r) => *r,
        Ready::Fail(msg) => return Ok(finish_unsent(&state.db, id, message_id, "failed", &msg).await?),
        Ready::Skip(msg) => return Ok(finish_unsent(&state.db, id, message_id, "skipped", &msg).await?),
        Ready::Wait(wait, msg) if attempts < MAX_ATTEMPTS => return Ok(requeue(&state.db, id, wait, &msg).await?),
        Ready::Wait(_, msg) => return Ok(finish_unsent(&state.db, id, message_id, "failed", &format!("not sent after {attempts} attempts ({msg})")).await?),
    };
    crate::sending::throttle_email(state).await;
    // A pause, turning it off, or a bounce or complaint can land while waiting for a slot.
    if let Some((status, msg)) = last_check(&state.db, account.id, &to_email).await? {
        return Ok(finish_unsent(&state.db, id, message_id, status, &msg).await?);
    }
    // One hand-off, no automatic retries: a second try after a lost answer could deliver it twice.
    match state.email.send_once(&email).await {
        Ok(provider_id) => {
            // Charged once per send id, in the same transaction that marks it sent.
            let mut tx = state.db.begin().await?;
            let first: Option<Uuid> = sqlx::query_scalar("UPDATE transactional_sends SET charged_at = now() WHERE id = $1 AND charged_at IS NULL RETURNING id").bind(id).fetch_optional(&mut *tx).await?;
            let cost = if first.is_some() { crate::billing::charge_email_tx(state, &mut tx, account.id).await? } else { 0 };
            sqlx::query("UPDATE transactional_sends SET status = 'sent', sent_at = now(), error = NULL, payload_enc = NULL, claimed_at = NULL, cost_micros = COALESCE(cost_micros, $2) WHERE id = $1")
                .bind(id)
                .bind(cost)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE messages SET status = 'sent', provider_message_id = $2, sent_at = now(), cost_micros = $3, from_domain = $4 WHERE id = $1")
                .bind(message_id)
                .bind(&provider_id)
                .bind(cost)
                .bind(crate::features::reports::delivery::sender_domain(&email.from_email))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            // Counts toward the bounce and complaint rates that pause an account, not the warm-up limit.
            crate::deliverability::record_confirmation_send(&state.db, message_id, &provider_id, account.id).await?;
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
                tracing::warn!(send = %id, attempt = attempts, reason = %busy.reason, "email provider busy; transactional email will retry");
                if attempts >= MAX_ATTEMPTS {
                    return Ok(finish_unsent(&state.db, id, message_id, "failed", &format!("not sent: the email provider was busy for {attempts} attempts")).await?);
                }
                return Ok(requeue(&state.db, id, backoff(attempts, busy.retry_after), "the email provider is busy").await?);
            }
            if let Some(maybe) = e.downcast_ref::<crate::providers::email::ProviderMaybeSent>() {
                tracing::warn!(send = %id, reason = %maybe.reason, "transactional email may have gone out; not sending it again");
                let msg = format!("{maybe}; it is not sent again and not charged");
                return Ok(finish_unsent(&state.db, id, message_id, "unknown", &msg).await?);
            }
            let msg = format!("{e:#}");
            tracing::warn!(send = %id, error = %msg, "transactional send failed");
            Ok(finish_unsent(&state.db, id, message_id, "failed", &msg).await?)
        }
    }
}

/// The last check before the hand-off, in one query against the current state: all email paused for
/// the account, transactional email paused or turned off, or the mailbox suppressed. Returns the
/// final status and why, or None to send.
async fn last_check(db: &PgPool, account_id: Uuid, to_email: &str) -> sqlx::Result<Option<(&'static str, String)>> {
    let (closed, all_paused, off, paused, suppressed): (bool, bool, bool, bool, Option<String>) = sqlx::query_as(&format!(
        "SELECT COALESCE((SELECT closed_at IS NOT NULL FROM accounts WHERE id = $1), true),
                COALESCE((SELECT sending_paused_at IS NOT NULL FROM accounts WHERE id = $1), true),
                COALESCE((SELECT enabled_at IS NULL FROM transactional_settings WHERE account_id = $1), true),
                COALESCE((SELECT paused_at IS NOT NULL FROM transactional_settings WHERE account_id = $1), false),
                {SUPPRESSED}"
    ))
    .bind(account_id)
    .bind(to_email)
    .fetch_one(db)
    .await?;
    Ok(if closed {
        Some(("skipped", "account closed".into()))
    } else if let Some(reason) = suppressed {
        Some(("skipped", suppressed_message(&reason)))
    } else if all_paused {
        Some(("failed", "not sent: email sending is paused for this account".into()))
    } else if off {
        Some(("failed", "not sent: transactional email was turned off".into()))
    } else if paused {
        Some(("failed", "not sent: transactional email is paused for this account".into()))
    } else {
        None
    })
}

/// Job: an attempt for a send that's waiting (the provider was busy, or the request never got to it).
pub async fn retry(state: &AppState, id: Uuid) -> anyhow::Result<()> {
    attempt(state, id, None).await.map_err(|e| match e {
        AppError::Internal(e) => e,
        other => anyhow::anyhow!(other.public_message()),
    })
}

/// Scheduler: attempts cut off mid-send become `unknown` (they may have gone out, so they're never
/// sent again), and retry payloads older than 24 hours are deleted (a send still waiting then fails).
pub async fn sweep(db: &PgPool) -> sqlx::Result<()> {
    let interrupted: Vec<Option<Uuid>> = sqlx::query_scalar(
        "UPDATE transactional_sends SET status = 'unknown', claimed_at = NULL, payload_enc = NULL,
                error = 'interrupted while sending: it may have gone out, so it is not sent again'
         WHERE status = 'sending' AND claimed_at < now() - make_interval(mins => $1) RETURNING message_id",
    )
    .bind(CLAIM_TIMEOUT_MINUTES)
    .fetch_all(db)
    .await?;
    let expired: Vec<Option<Uuid>> = sqlx::query_scalar(
        "UPDATE transactional_sends SET payload_enc = NULL,
                error = CASE WHEN status = 'queued' THEN 'not sent within 24 hours' ELSE error END,
                status = CASE WHEN status = 'queued' THEN 'failed' ELSE status END
         WHERE payload_enc IS NOT NULL AND created_at < now() - interval '24 hours' RETURNING CASE WHEN status = 'failed' THEN message_id END",
    )
    .fetch_all(db)
    .await?;
    let ids: Vec<Uuid> = interrupted.into_iter().chain(expired).flatten().collect();
    if !ids.is_empty() {
        const FAIL: &str = "UPDATE messages SET status = 'failed', error = 'transactional send interrupted or expired' WHERE id = ANY($1) AND status = 'queued'";
        if crate::features::observes_any(db) {
            let failed: Vec<(Uuid, Uuid)> = sqlx::query_as(&format!("{FAIL} RETURNING account_id, id")).bind(&ids).fetch_all(db).await?;
            crate::features::messages_unsent_rows(db, &failed).await;
        } else {
            sqlx::query(FAIL).bind(&ids).execute(db).await?;
        }
    }
    Ok(())
}

/// Dry run: what would be sent, without sending or saving anything.
pub async fn dry_run(state: &AppState, ctx: &Ctx, b: SendBody, include_html: bool) -> AppResult<Value> {
    check_body(&b)?;
    let account_id = ctx.account_id;
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let gate = match guard::may_send_transactional(state, ctx).await {
        Ok(a) => match check_mode(state, &a, &b) {
            Ok(()) => guard::check_open(state, &account, &a).await.err(),
            Err(e) => Some(e),
        },
        Err(e) => Some(e),
    };
    let (content, _) = resolve_content(&state.db, account_id, &b).await?;
    let to = parse_to(&b.to)?;
    let existing = profile_by_email(&state.db, account_id, &to.email).await?;
    let blocked = suppressed_address(&state.db, account_id, &to.email).await?.map(|r| suppressed_message(&r));
    let profile = existing.unwrap_or_else(|| unsaved_profile(account_id, &to.email));
    let email = render_for(state, &account, &profile, &to, &b, &content, Uuid::nil())?;
    let mut out = json!({
        "dry_run": true,
        "to": email.to, "from": {"email": email.from_email, "name": email.from_name}, "reply_to": email.reply_to,
        "subject": email.subject, "text": email.text, "html_bytes": email.html.len(),
        "would_send": gate.is_none() && blocked.is_none(),
        "not_sent_because": gate.map(|e| e.public_message()).or(blocked),
        "note": "Nothing sent. Call again with confirm=true to send it.",
    });
    if include_html {
        out["html"] = json!(email.html);
    }
    Ok(out)
}

/// One send as the API returns it: recipient, subject, template, status, times, metadata and
/// tracking. Never the variables or the body.
pub async fn get(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let rows = list_rows(db, account_id, Some(id), None, None, 1, 0).await?;
    rows.into_iter().next().ok_or_else(|| AppError::not_found("transactional send"))
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
    attempts: i32,
    created_at: DateTime<Utc>,
    sent_at: Option<DateTime<Utc>>,
    message_status: Option<String>,
    delivered_at: Option<DateTime<Utc>>,
    opened_at: Option<DateTime<Utc>>,
    clicked_at: Option<DateTime<Utc>>,
    bounce_type: Option<String>,
}

/// The send's state in one word: queued, sending, sent, delivered, opened, clicked, bounced,
/// complained, skipped, failed or unknown. Delivery and engagement come from the message once it's sent.
fn state_of(r: &Row) -> &str {
    if r.status != "sent" {
        return &r.status;
    }
    match r.message_status.as_deref() {
        Some("bounced") => "bounced",
        Some("complained") => "complained",
        _ if r.clicked_at.is_some() => "clicked",
        _ if r.opened_at.is_some() => "opened",
        Some("delivered") => "delivered",
        _ => "sent",
    }
}

pub async fn list_rows(db: &PgPool, account_id: Uuid, id: Option<Uuid>, q: Option<&str>, status: Option<&str>, limit: i64, offset: i64) -> AppResult<Vec<Value>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT t.id, t.to_email, t.subject, t.status, t.error, t.template_id, t.profile_id, t.message_id, t.idempotency_key, t.metadata,
                t.track_clicks, t.via, t.attempts, t.created_at, t.sent_at, m.status AS message_status, m.delivered_at, m.opened_at, m.clicked_at, m.bounce_type
         FROM transactional_sends t LEFT JOIN messages m ON m.id = t.message_id
         WHERE t.account_id = $1 AND ($2::uuid IS NULL OR t.id = $2)
           AND ($3::text IS NULL OR t.to_email LIKE '%' || $3 || '%' OR t.subject ILIKE '%' || $3 || '%')
           AND ($4::text IS NULL OR $4 = CASE WHEN t.status <> 'sent' THEN t.status
                                              WHEN m.status IN ('bounced','complained') THEN m.status
                                              WHEN m.clicked_at IS NOT NULL THEN 'clicked' WHEN m.opened_at IS NOT NULL THEN 'opened'
                                              WHEN m.status = 'delivered' THEN 'delivered' ELSE 'sent' END)
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
                "attempts": r.attempts, "created_at": r.created_at, "sent_at": r.sent_at, "delivered_at": r.delivered_at,
                "opened_at": r.opened_at, "clicked_at": r.clicked_at, "bounce_type": r.bounce_type,
            })
        })
        .collect())
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
        assert!(parse_to(&json!("a@x.test\r\nBcc: b@x.test")).is_err());
        assert!(parse_to(&json!({"email": "a@x.test", "cc": "b@x.test"})).is_err());
        let r = parse_to(&json!({"email": "a@x.test", "name": "Ann Lee Smith"})).unwrap();
        assert_eq!((r.first_name.as_deref(), r.last_name.as_deref()), (Some("Ann"), Some("Lee Smith")));
        assert!(parse_to(&json!("nope")).is_err());
    }

    #[test]
    fn busy_backoff_grows_and_is_bounded() {
        let asked = std::time::Duration::from_secs(5);
        let mins: Vec<i64> = (1..=6).map(|a| backoff(a, asked).num_minutes()).collect();
        assert_eq!(mins, vec![1, 2, 4, 8, 16, 32]);
        assert_eq!(backoff(3, std::time::Duration::from_secs(7200)).num_minutes(), 60);
    }
}
