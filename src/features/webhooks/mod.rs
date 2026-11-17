//! Outgoing webhooks, REST hooks for Zapier and Make, and the signature on flow webhook steps.
//!
//! Watching a change must never break or slow the change. So the rest of the app reports changes
//! through the small `pub` functions here ([`emit`], [`event_recorded`], [`profile_saved`],
//! [`list_changed`] ...), and each one does at most one cheap `INSERT` into `webhook_outbox`:
//! inside the caller's transaction when there is one (under a savepoint, so a failure can't spoil
//! it), otherwise on its own, with any failure logged and swallowed. None of them returns
//! anything a caller could propagate. Accounts with no endpoint wanting the topic skip even that
//! (a short-lived cache, see [`SUBSCRIBE_DELAY_SECS`]).
//!
//! The insert also queues one `webhooks_fanout` job per account in the background job lane (see
//! `jobs::BACKGROUND`), which turns outbox rows into per-endpoint deliveries in bounded batches
//! (`fanout`). A `webhooks_deliver` job per endpoint sends them (`deliver`), one request at a time,
//! so a slow or dead endpoint never holds up sending, other accounts or other endpoints.

mod api;
mod deliver;
mod fanout;
pub mod net;
mod tools;
pub mod topics;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{Feature, McpDoc};
pub use crate::audience::EventSource as Source;
pub use fanout::{max_outbox, max_pending, set_limits_for_tests};
use crate::auth::Ctx;
use crate::error::AppResult;
use crate::models::Profile;
use crate::state::AppState;

/// The `api_version` in every envelope. Bumped only for changes that could break a receiver.
pub const API_VERSION: &str = "2026-11-17";

/// A new endpoint (or a new topic on one, or one turned back on) receives every matching event
/// from this many seconds after the change; events in those first seconds may or may not reach it.
/// It's how long a server may keep believing an account has no endpoint for a topic.
pub const SUBSCRIBE_DELAY_SECS: u64 = 5;

pub struct Module;

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "webhooks"
    }

    fn routes(&self) -> Router<AppState> {
        api::routes()
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools::defs()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        tools::call(state, ctx, name, args).await
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "webhooks",
            name: "Webhooks",
            description: "Outgoing webhook topics, the envelope, signature verification (Node, Python, Ruby, PHP, Rust), retries, REST hooks for Zapier and Make",
            text: include_str!("../../../docs/WEBHOOKS.md"),
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        let id = |k: &str| payload[k].as_str().and_then(|s| s.parse::<Uuid>().ok());
        match kind {
            "webhooks_fanout" => Some(match id("account_id") {
                Some(a) => fanout::run(state, a).await,
                None => Ok(()),
            }),
            "webhooks_deliver" => Some(match id("endpoint_id") {
                Some(e) => deliver::run(state, e).await,
                None => Ok(()),
            }),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            fanout::sweep(state).await?;
            deliver::sweep(state).await?;
        }
        if tick.is_multiple_of(180) {
            deliver::prune(&state.db).await?;
            deliver::recount(&state.db).await?;
        }
        Ok(())
    }
}

// ---------- Who is subscribed (cached briefly) ----------

pub(crate) struct Sub {
    topics: Vec<String>,
    metrics: Vec<String>,
    trusted_only: bool,
}

type SubCache = Mutex<HashMap<Uuid, (Instant, Arc<Vec<Sub>>)>>;
static SUBS: LazyLock<SubCache> = LazyLock::new(Default::default);
/// "Yes, someone wants this" is trusted this long: a stale yes only costs an insert that matches nothing.
const POSITIVE_TTL: Duration = Duration::from_secs(30);
/// "Nobody wants this" is trusted only briefly, so a new endpoint starts receiving within seconds.
const NEGATIVE_TTL: Duration = Duration::from_secs(SUBSCRIBE_DELAY_SECS);

fn wanted(subs: &[Sub], topic: &str, metric: Option<&str>, source: Source) -> bool {
    subs.iter().any(|s| {
        s.topics.iter().any(|t| t == topic)
            && (metric.is_none() || s.metrics.is_empty() || metric.is_some_and(|m| s.metrics.iter().any(|x| x == m)))
            && !(s.trusted_only && source == Source::Browser)
    })
}

fn cached(account_id: Uuid) -> Option<(Duration, Arc<Vec<Sub>>)> {
    SUBS.lock().ok().and_then(|m| m.get(&account_id).map(|(at, s)| (at.elapsed(), s.clone())))
}

/// What a cache entry of this age says, or None when it's too old to say it.
fn decide(age: Duration, subs: &[Sub], topic: &str, metric: Option<&str>, source: Source) -> Option<bool> {
    let yes = wanted(subs, topic, metric, source);
    if age < NEGATIVE_TTL || (yes && age < POSITIVE_TTL) {
        Some(yes)
    } else {
        None
    }
}

async fn load_subs(db: &PgPool, account_id: Uuid) -> Option<Arc<Vec<Sub>>> {
    let rows: Vec<(Vec<String>, Vec<String>, bool)> =
        sqlx::query_as("SELECT topics, metrics, trusted_only FROM webhook_endpoints WHERE account_id = $1 AND enabled").bind(account_id).fetch_all(db).await.ok()?;
    let s = Arc::new(rows.into_iter().map(|(topics, metrics, trusted_only)| Sub { topics, metrics, trusted_only }).collect::<Vec<_>>());
    if let Ok(mut m) = SUBS.lock() {
        if m.len() > 20_000 {
            m.retain(|_, (at, _)| at.elapsed() < POSITIVE_TTL);
        }
        m.insert(account_id, (Instant::now(), s.clone()));
    }
    Some(s)
}

/// Whether an endpoint of the account may want this. When unsure (the database didn't answer) it
/// says yes, and the insert itself checks again.
async fn may_want(db: &PgPool, account_id: Uuid, topic: &str, metric: Option<&str>, source: Source) -> bool {
    if let Some((age, s)) = cached(account_id) {
        if let Some(d) = decide(age, &s, topic, metric, source) {
            return d;
        }
    }
    match load_subs(db, account_id).await {
        Some(s) => wanted(&s, topic, metric, source),
        None => true,
    }
}

/// [`may_want`] without touching the database (inside a caller's transaction).
fn may_want_cached(account_id: Uuid, topic: &str) -> bool {
    match cached(account_id) {
        Some((age, s)) => decide(age, &s, topic, None, Source::System).unwrap_or(true),
        None => true,
    }
}

/// Drop the cached subscriptions after an endpoint changes on this server.
fn forget(account_id: Uuid) {
    if let Ok(mut m) = SUBS.lock() {
        m.remove(&account_id);
    }
}

// ---------- The outbox ----------

/// One change waiting to go out.
pub(crate) struct Out {
    pub event_id: Uuid,
    pub topic: String,
    pub metric: Option<String>,
    pub source: Source,
    /// The envelope's `data`. `{"$profile": id}`-style parts are filled in when it fans out.
    pub data: Value,
}

impl Out {
    fn new(topic: &str, source: Source, data: Value) -> Self {
        Out { event_id: Uuid::now_v7(), topic: topic.to_string(), metric: None, source, data }
    }
}

/// The same id for the same thing: a receiver can drop a repeat.
pub(crate) fn stable_id(parts: &[&str]) -> Uuid {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    uuid::Builder::from_custom_bytes(b).into_uuid()
}

/// Rows only an endpoint wants are kept; the first row for an account with nothing queued also
/// queues its fan-out job. One statement, so the job can't be lost between two writes.
const OUTBOX_INSERT: &str = "
    WITH o AS (
        INSERT INTO webhook_outbox (account_id, event_id, topic, metric, source, data)
        SELECT $1, x.ev, x.topic, x.metric, x.source, x.data
        FROM UNNEST($2::uuid[], $3::text[], $4::text[], $5::text[], $6::jsonb[]) AS x(ev, topic, metric, source, data)
        WHERE EXISTS (SELECT 1 FROM webhook_endpoints e WHERE e.account_id = $1 AND e.enabled AND x.topic = ANY(e.topics)
                        AND (x.metric IS NULL OR cardinality(e.metrics) = 0 OR x.metric = ANY(e.metrics))
                        AND NOT (e.trusted_only AND x.source = 'browser'))
        RETURNING account_id
    ), m AS (
        INSERT INTO webhook_fanout_pending (account_id) SELECT $1 WHERE EXISTS (SELECT 1 FROM o)
        ON CONFLICT DO NOTHING RETURNING account_id
    )
    INSERT INTO jobs (kind, payload, lane) SELECT 'webhooks_fanout', jsonb_build_object('account_id', m.account_id), 'background' FROM m";

async fn write<'c>(db: impl sqlx::PgExecutor<'c>, account_id: Uuid, rows: &[Out]) -> sqlx::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = rows.iter().map(|r| r.event_id).collect();
    let topics: Vec<&str> = rows.iter().map(|r| r.topic.as_str()).collect();
    let metrics: Vec<Option<&str>> = rows.iter().map(|r| r.metric.as_deref()).collect();
    let sources: Vec<&str> = rows.iter().map(|r| r.source.as_str()).collect();
    let data: Vec<&Value> = rows.iter().map(|r| &r.data).collect();
    sqlx::query(OUTBOX_INSERT).bind(account_id).bind(&ids).bind(&topics).bind(&metrics).bind(&sources).bind(&data).execute(db).await?;
    Ok(())
}

/// Queue `rows` for the account's endpoints. Never fails the caller: problems are logged.
pub(crate) async fn emit_rows(db: &PgPool, account_id: Uuid, mut rows: Vec<Out>) {
    let mut keep = Vec::with_capacity(rows.len());
    for r in rows.drain(..) {
        if may_want(db, account_id, &r.topic, r.metric.as_deref(), r.source).await {
            keep.push(r);
        }
    }
    for chunk in keep.chunks(500) {
        if let Err(e) = write(db, account_id, chunk).await {
            tracing::error!(account = %account_id, topic = %chunk[0].topic, rows = chunk.len(), error = %e, "couldn't queue webhook events");
        }
    }
}

/// Queue `rows` inside the caller's transaction, so they go out only if it commits. A failure
/// rolls back only this insert (a savepoint), is logged, and leaves the caller's transaction fine.
pub(crate) async fn emit_rows_in(conn: &mut PgConnection, account_id: Uuid, rows: Vec<Out>) {
    let rows: Vec<Out> = rows.into_iter().filter(|r| may_want_cached(account_id, &r.topic)).collect();
    if rows.is_empty() {
        return;
    }
    let res: sqlx::Result<()> = async {
        let mut sp = sqlx::Connection::begin(&mut *conn).await?;
        for chunk in rows.chunks(500) {
            write(&mut *sp, account_id, chunk).await?;
        }
        sp.commit().await
    }
    .await;
    if let Err(e) = res {
        tracing::error!(account = %account_id, topic = %rows[0].topic, error = %e, "couldn't queue webhook events");
    }
}

/// Send `data` to every endpoint of the account subscribed to `topic`. Never fails the caller.
pub async fn emit(db: &PgPool, account_id: Uuid, topic: &str, source: Source, data: Value) {
    emit_rows(db, account_id, vec![Out::new(topic, source, data)]).await
}

fn profile_ref(id: Uuid) -> Value {
    json!({"$profile": id})
}

// ---------- Hooks the rest of the app calls where things change ----------

pub(crate) fn profile_summary(p: &Profile) -> Value {
    json!({"id": p.id, "email": p.email, "phone_number": p.phone_number, "external_id": p.external_id, "first_name": p.first_name, "last_name": p.last_name})
}

/// An event was recorded: `event.tracked`, and for DumbMail's own message events the message
/// topic it stands for (`message.sent`, `message.opened` ...). Called by `audience::record_event`.
pub async fn event_recorded(db: &PgPool, account_id: Uuid, event_id: Uuid, metric: &str, source: Source, message_id: Option<Uuid>, properties: &Value) {
    let mut rows = vec![Out { event_id, topic: "event.tracked".into(), metric: Some(metric.to_string()), source, data: json!({"$event": event_id}) }];
    // Message topics come only from DumbMail's own records of its messages, never from an event
    // someone sent in with the same name.
    if let (Source::System, Some(mid)) = (source, message_id) {
        if let Some(topic) = topics::message_topic(metric) {
            let mut data = json!({"$message": mid});
            match topic {
                "message.clicked" => data["url"] = properties.get("url").cloned().unwrap_or(Value::Null),
                "message.bounced" => data["bounce_type"] = properties.get("bounce_type").cloned().unwrap_or(Value::Null),
                _ => {}
            }
            rows.push(Out { event_id: stable_id(&[topic, &event_id.to_string()]), topic: topic.into(), metric: None, source, data });
        }
    }
    emit_rows(db, account_id, rows).await
}

/// After a profile is created or saved: `profile.created`, or `profile.updated` when something
/// actually changed, plus `profile.subscribed` / `profile.unsubscribed` for consent changes.
pub async fn profile_saved(db: &PgPool, account_id: Uuid, before: Option<&Profile>, after: &Profile, source: Source) {
    let summary = profile_summary(after);
    let mut out: Vec<Out> = Vec::new();
    match before {
        None => out.push(Out::new("profile.created", source, json!({"profile": after}))),
        Some(b) => {
            let changed: Vec<&str> = [
                ("email", b.email == after.email),
                ("phone_number", b.phone_number == after.phone_number),
                ("external_id", b.external_id == after.external_id),
                ("first_name", b.first_name == after.first_name),
                ("last_name", b.last_name == after.last_name),
                ("properties", b.properties == after.properties),
                ("email_consent", b.email_consent == after.email_consent),
                ("sms_consent", b.sms_consent == after.sms_consent),
            ]
            .into_iter()
            .filter(|(_, same)| !same)
            .map(|(k, _)| k)
            .collect();
            if !changed.is_empty() {
                out.push(Out::new("profile.updated", source, json!({"profile": after, "changed": changed})));
            }
        }
    }
    let reason = if before.is_none() { "created" } else { "profile_update" };
    for (channel, b, a) in [("email", before.map(|b| b.email_consent.as_str()), after.email_consent.as_str()), ("sms", before.map(|b| b.sms_consent.as_str()), after.sms_consent.as_str())] {
        if let Some(o) = consent_row(channel, b.unwrap_or("never_subscribed"), a, reason, None, source, summary.clone()) {
            out.push(o);
        }
    }
    emit_rows(db, account_id, out).await
}

fn consent_row(channel: &str, before: &str, after: &str, reason: &str, message_id: Option<Uuid>, source: Source, profile: Value) -> Option<Out> {
    let topic = match after {
        _ if before == after => return None,
        "subscribed" => "profile.subscribed",
        "unsubscribed" => "profile.unsubscribed",
        _ => return None,
    };
    let mut data = json!({"channel": channel, "reason": reason, "before": before, "after": after, "profile": profile});
    if let Some(m) = message_id {
        data["message_id"] = json!(m);
    }
    Some(Out::new(topic, source, data))
}

/// Consent was written somewhere other than a profile save (an unsubscribe link, a STOP or START
/// text, a spam complaint, a double opt-in). `before` and `after` are what the write changed.
#[allow(clippy::too_many_arguments)]
pub async fn consent_changed(db: &PgPool, account_id: Uuid, profile_id: Uuid, channel: &str, before: &str, after: &str, reason: &str, message_id: Option<Uuid>) {
    if let Some(o) = consent_row(channel, before, after, reason, message_id, Source::System, profile_ref(profile_id)) {
        emit_rows(db, account_id, vec![o]).await
    }
}

/// An address was suppressed (`hard_bounce` or `complaint`) and wasn't before.
pub async fn suppressed(db: &PgPool, account_id: Uuid, profile_id: Uuid, reason: &str, message_id: Uuid) {
    let data = json!({"channel": "email", "reason": reason, "message_id": message_id, "profile": profile_ref(profile_id)});
    let o = Out { event_id: stable_id(&["profile.suppressed", &message_id.to_string()]), ..Out::new("profile.suppressed", Source::System, data) };
    emit_rows(db, account_id, vec![o]).await
}

/// A profile was deleted. `summary` is what it was ({id, email, phone_number, external_id}).
pub async fn profile_deleted(db: &PgPool, account_id: Uuid, summary: Value) {
    emit(db, account_id, "profile.deleted", Source::Api, json!({"profile": summary})).await
}

/// Profiles joined (`added`) or left a list.
pub async fn list_changed(db: &PgPool, account_id: Uuid, list_id: Uuid, profile_ids: &[Uuid], added: bool) {
    let topic = if added { "list.member_added" } else { "list.member_removed" };
    if profile_ids.is_empty() || !may_want(db, account_id, topic, None, Source::System).await {
        return;
    }
    if let Err(e) = fanout::insert_members(db, account_id, topic, "list", list_id, profile_ids).await {
        tracing::error!(account = %account_id, topic, error = %e, "couldn't queue webhook events");
    }
}

/// A segment refresh found people who entered and left. Called inside the refresh's transaction.
pub async fn segment_changed_in(conn: &mut PgConnection, account_id: Uuid, segment_id: Uuid, entered: &[Uuid], left: &[Uuid]) {
    for (topic, ids) in [("segment.entered", entered), ("segment.left", left)] {
        if ids.is_empty() || !may_want_cached(account_id, topic) {
            continue;
        }
        let res: sqlx::Result<()> = async {
            let mut sp = sqlx::Connection::begin(&mut *conn).await?;
            fanout::insert_members(&mut *sp, account_id, topic, "segment", segment_id, ids).await?;
            sp.commit().await
        }
        .await;
        if let Err(e) = res {
            tracing::error!(account = %account_id, topic, error = %e, "couldn't queue webhook events");
        }
    }
}

/// The provider confirmed a message was delivered (called only the first time it does).
pub async fn message_delivered(db: &PgPool, account_id: Uuid, message_id: Uuid) {
    let o = Out { event_id: stable_id(&["message.delivered", &message_id.to_string()]), ..Out::new("message.delivered", Source::System, json!({"$message": message_id})) };
    emit_rows(db, account_id, vec![o]).await
}

/// Someone signed up through a form.
pub async fn form_submitted(db: &PgPool, account_id: Uuid, form_id: Uuid, profile_id: Uuid, variant: Option<&str>) {
    emit(db, account_id, "form.submitted", Source::Browser, json!({"form": {"$form": form_id}, "variant": variant.unwrap_or("a"), "profile": profile_ref(profile_id)})).await
}

/// A campaign started sending. Called inside the transaction that marks it sent.
pub async fn campaign_sent_in(conn: &mut PgConnection, account_id: Uuid, campaign_id: Uuid) {
    let o = Out { event_id: stable_id(&["campaign.sent", &campaign_id.to_string()]), ..Out::new("campaign.sent", Source::System, json!({"campaign": {"$campaign": campaign_id}})) };
    emit_rows_in(conn, account_id, vec![o]).await
}

/// A campaign finished sending.
pub async fn campaign_finished(db: &PgPool, account_id: Uuid, campaign_id: Uuid) {
    let o =
        Out { event_id: stable_id(&["campaign.finished", &campaign_id.to_string()]), ..Out::new("campaign.finished", Source::System, json!({"campaign": {"$campaign": campaign_id}})) };
    emit_rows(db, account_id, vec![o]).await
}

/// A flow was turned on, paused or set to draft.
pub async fn flow_status_changed(db: &PgPool, account_id: Uuid, flow_id: Uuid, name: &str, previous: &str, status: &str, trigger: &Value) {
    if previous != status {
        emit(db, account_id, "flow.status_changed", Source::Api, json!({"flow": {"id": flow_id, "name": name, "status": status, "previous_status": previous, "trigger": trigger}}))
            .await;
    }
}

/// A profile import finished. `import` holds its counts.
pub async fn import_finished(db: &PgPool, account_id: Uuid, import: Value) {
    emit(db, account_id, "import.finished", Source::Api, json!({"import": import})).await
}

// ---------- Signing ----------

pub fn new_secret() -> String {
    format!("whsec_{}", crate::auth::random_token(24))
}

pub fn secret_hint(secret: &str) -> String {
    let tail: String = secret.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("whsec_…{tail}")
}

/// Hex HMAC-SHA256 of `{timestamp}.{body}`.
pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// The signature headers for a request body, one `v1=` value per secret.
pub fn signature_headers(secrets: &[String], body: &[u8]) -> Vec<(&'static str, String)> {
    let ts = Utc::now().timestamp();
    let sig = secrets.iter().map(|s| format!("v1={}", sign(s, ts, body))).collect::<Vec<_>>().join(",");
    vec![("DumbMail-Webhook-Id", Uuid::now_v7().to_string()), ("DumbMail-Webhook-Timestamp", ts.to_string()), ("DumbMail-Webhook-Signature", sig)]
}

/// The account's secret for flow webhook steps, created the first time it's needed.
async fn flow_secret(state: &AppState, account_id: Uuid) -> anyhow::Result<String> {
    let enc: Option<String> = sqlx::query_scalar("SELECT secret_enc FROM webhook_signing_secrets WHERE account_id = $1").bind(account_id).fetch_optional(&state.db).await?;
    if let Some(enc) = enc {
        return crate::crypto::decrypt(&state.cfg.secret_key, &enc);
    }
    let secret = new_secret();
    sqlx::query("INSERT INTO webhook_signing_secrets (account_id, secret_enc, secret_hint) VALUES ($1, $2, $3) ON CONFLICT (account_id) DO NOTHING")
        .bind(account_id)
        .bind(crate::crypto::encrypt(&state.cfg.secret_key, &secret)?)
        .bind(secret_hint(&secret))
        .execute(&state.db)
        .await?;
    let enc: String = sqlx::query_scalar("SELECT secret_enc FROM webhook_signing_secrets WHERE account_id = $1").bind(account_id).fetch_one(&state.db).await?;
    crate::crypto::decrypt(&state.cfg.secret_key, &enc)
}

/// Signature headers for a flow webhook step's request body. If the secret can't be read the
/// request goes out unsigned rather than not at all (flows sent unsigned webhooks before).
pub async fn flow_signature_headers(state: &AppState, account_id: Uuid, body: &[u8]) -> Vec<(&'static str, String)> {
    match flow_secret(state, account_id).await {
        Ok(s) => signature_headers(&[s], body),
        Err(e) => {
            tracing::warn!(account = %account_id, error = %format!("{e:#}"), "couldn't sign flow webhook");
            Vec::new()
        }
    }
}

// ---------- Alerts ----------

/// How an alert names a destination: its name and host, never the full URL (which can itself be
/// a credential, like a Zapier or Make hook URL).
pub fn display_name(name: &str, url: &str) -> String {
    let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_else(|| "an unknown host".into());
    let name = name.trim();
    if name.is_empty() {
        format!("the endpoint at {host}")
    } else {
        format!("\"{}\" ({host})", name.chars().take(100).collect::<String>())
    }
}

/// Email the account's owners a notice from SUPPORT_EMAIL. Failures are only logged. `text` must
/// not contain secrets, response bodies or full URLs.
pub async fn alert_account(state: &AppState, account_id: Uuid, subject: &str, text: &str) {
    let Some(from) = state.cfg.support_email.as_deref().filter(|s| s.contains('@')) else {
        tracing::warn!(account = %account_id, subject, "no SUPPORT_EMAIL set; account alert not emailed");
        return;
    };
    let owners: Vec<String> =
        sqlx::query_scalar("SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 AND m.role = 'owner' ORDER BY m.created_at LIMIT 5")
            .bind(account_id)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();
    if owners.is_empty() {
        tracing::warn!(account = %account_id, subject, "account has no owner to alert");
    }
    let html = format!(
        "<div style=\"font-family:system-ui,sans-serif;font-size:15px;line-height:1.5;white-space:pre-wrap\">{}</div>",
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    );
    for to in owners {
        let email = crate::providers::email::OutgoingEmail {
            message_id: Uuid::now_v7(),
            to,
            from_name: "DumbMail".into(),
            from_email: from.to_string(),
            reply_to: Some(from.to_string()),
            subject: subject.to_string(),
            html: html.clone(),
            text: text.to_string(),
            unsubscribe_url: None,
        };
        if let Err(e) = state.email.send(&email).await {
            tracing::error!(account = %account_id, error = %format!("{e:#}"), "couldn't email account alert");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_timestamp_dot_body() {
        // printf '1700000000.{"a":1}' | openssl dgst -sha256 -hmac whsec_test
        assert_eq!(sign("whsec_test", 1_700_000_000, br#"{"a":1}"#), "38877139021993b830af32feea6e18a8da83eb2f6e49ee50bd9e4cf4ca4d3789");
        assert_eq!(sign("whsec_test", 1, b"x"), sign("whsec_test", 1, b"x"));
        assert_ne!(sign("whsec_test", 1, b"x"), sign("whsec_test", 2, b"x"));
        let h = signature_headers(&["a".into(), "b".into()], b"{}");
        assert_eq!(h[2].1.matches("v1=").count(), 2);
    }

    #[test]
    fn subscriptions_match_topic_metric_and_source() {
        let subs = vec![Sub { topics: vec!["event.tracked".into(), "message.opened".into()], metrics: vec!["Placed Order".into()], trusted_only: true }];
        assert!(wanted(&subs, "event.tracked", Some("Placed Order"), Source::Api));
        assert!(!wanted(&subs, "event.tracked", Some("Placed Order"), Source::Browser), "trusted sources only");
        assert!(!wanted(&subs, "event.tracked", Some("Viewed Product"), Source::Api));
        assert!(wanted(&subs, "message.opened", None, Source::System));
        assert!(!wanted(&subs, "profile.created", None, Source::System));
    }

    #[test]
    fn a_stale_no_is_never_trusted_for_long() {
        let none: Vec<Sub> = Vec::new();
        assert_eq!(decide(Duration::from_secs(1), &none, "profile.created", None, Source::System), Some(false));
        assert_eq!(decide(NEGATIVE_TTL + Duration::from_millis(1), &none, "profile.created", None, Source::System), None);
        let some = vec![Sub { topics: vec!["profile.created".into()], metrics: vec![], trusted_only: false }];
        assert_eq!(decide(Duration::from_secs(20), &some, "profile.created", None, Source::System), Some(true));
        assert_eq!(decide(POSITIVE_TTL, &some, "profile.created", None, Source::System), None);
    }

    #[test]
    fn stable_ids_repeat() {
        assert_eq!(stable_id(&["message.delivered", "a"]), stable_id(&["message.delivered", "a"]));
        assert_ne!(stable_id(&["message.delivered", "a"]), stable_id(&["message.delivered", "b"]));
        assert_ne!(stable_id(&["ab", "c"]), stable_id(&["a", "bc"]));
        assert_eq!(display_name("", "https://hooks.zapier.com/hooks/catch/1/secret/"), "the endpoint at hooks.zapier.com");
        assert_eq!(display_name("CRM", "https://crm.example.com/x?token=1"), "\"CRM\" (crm.example.com)");
    }
}
