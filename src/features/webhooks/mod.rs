//! Outgoing webhooks, REST hooks for Zapier and Make, and the signature on flow webhook steps.
//!
//! Anything that changes calls one of the small `pub` functions here ([`emit`], [`profile_saved`],
//! [`list_changed`] ...). Each one checks a briefly cached list of the account's subscriptions first,
//! so accounts without webhooks pay nothing. Matching endpoints get one delivery row per event; a
//! `webhooks_deliver` job per endpoint sends them (see `deliver`), so a slow or dead endpoint never
//! holds up sending or other endpoints.

mod api;
mod deliver;
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
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::AppResult;
use crate::models::Profile;
use crate::state::AppState;

/// The `api_version` in every envelope. Bumped only for changes that could break a receiver.
pub const API_VERSION: &str = "2026-11-17";

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
        match kind {
            "webhooks_deliver" => {
                let id = payload["endpoint_id"].as_str().and_then(|s| s.parse::<Uuid>().ok());
                Some(match id {
                    Some(id) => deliver::run(state, id).await,
                    None => Ok(()),
                })
            }
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            deliver::sweep(state).await?;
        }
        if tick.is_multiple_of(180) {
            deliver::prune(&state.db).await?;
        }
        Ok(())
    }

    async fn wants_event(&self, db: &PgPool, account_id: Uuid, metric: &str) -> bool {
        let subs = subs(db, account_id).await;
        wanted(&subs, "event.tracked", Some(metric)) || topics::derived(metric).iter().any(|t| wanted(&subs, t, None))
    }

    async fn on_event(&self, state: &AppState, event_id: Uuid) -> anyhow::Result<()> {
        event_hook(&state.db, event_id).await
    }
}

// ---------- Who is subscribed (cached briefly) ----------

struct Sub {
    topics: Vec<String>,
    metrics: Vec<String>,
}

type SubCache = Mutex<HashMap<Uuid, (Instant, Arc<Vec<Sub>>)>>;
static SUBS: LazyLock<SubCache> = LazyLock::new(Default::default);
const SUBS_TTL: Duration = Duration::from_secs(30);

/// The account's enabled endpoints' topics, read at most every 30 seconds (and right after this
/// server changes an endpoint).
async fn subs(db: &PgPool, account_id: Uuid) -> Arc<Vec<Sub>> {
    if let Some((at, s)) = SUBS.lock().ok().and_then(|m| m.get(&account_id).map(|(a, s)| (*a, s.clone()))) {
        if at.elapsed() < SUBS_TTL {
            return s;
        }
    }
    let rows: Result<Vec<(Vec<String>, Vec<String>)>, _> =
        sqlx::query_as("SELECT topics, metrics FROM webhook_endpoints WHERE account_id = $1 AND enabled").bind(account_id).fetch_all(db).await;
    let Ok(rows) = rows else { return Arc::new(Vec::new()) };
    let s = Arc::new(rows.into_iter().map(|(topics, metrics)| Sub { topics, metrics }).collect::<Vec<_>>());
    if let Ok(mut m) = SUBS.lock() {
        if m.len() > 20_000 {
            m.retain(|_, (at, _)| at.elapsed() < SUBS_TTL);
        }
        m.insert(account_id, (Instant::now(), s.clone()));
    }
    s
}

/// Drop the cached subscriptions after an endpoint changes.
fn forget(account_id: Uuid) {
    if let Ok(mut m) = SUBS.lock() {
        m.remove(&account_id);
    }
}

fn wanted(subs: &[Sub], topic: &str, metric: Option<&str>) -> bool {
    subs.iter().any(|s| s.topics.iter().any(|t| t == topic) && (topic != "event.tracked" || s.metrics.is_empty() || metric.is_some_and(|m| s.metrics.iter().any(|x| x == m))))
}

async fn wants(db: &PgPool, account_id: Uuid, topic: &str) -> bool {
    wanted(&subs(db, account_id).await, topic, None)
}

// ---------- Emitting ----------

/// Send `data` to every endpoint of the account subscribed to `topic`. Never fails the caller:
/// problems are logged.
pub async fn emit(db: &PgPool, account_id: Uuid, topic: &str, data: Value) {
    emit_many(db, account_id, topic, vec![data]).await
}

/// Like [`emit`] for many events of one topic at once (one envelope each).
pub async fn emit_many(db: &PgPool, account_id: Uuid, topic: &str, items: Vec<Value>) {
    if let Err(e) = enqueue(db, account_id, topic, None, items).await {
        tracing::warn!(account = %account_id, topic, error = %format!("{e:#}"), "couldn't queue webhook deliveries");
    }
}

#[derive(sqlx::FromRow)]
struct Target {
    id: Uuid,
    filter: Option<Value>,
}

fn passes(filter: &Option<Value>, data: &Value) -> bool {
    let Some(f) = filter else { return true };
    let Ok(conds) = serde_json::from_value::<Vec<crate::segments::Filter>>(f.clone()) else { return true };
    conds.iter().all(|c| crate::segments::json_filter_matches(data, c))
}

pub(crate) fn envelope(event_id: Uuid, account_id: Uuid, topic: &str, data: Value) -> Value {
    json!({"id": event_id, "topic": topic, "created_at": Utc::now(), "account_id": account_id, "api_version": API_VERSION, "data": data})
}

/// Queue deliveries. Returns how many were queued.
async fn enqueue(db: &PgPool, account_id: Uuid, topic: &str, metric: Option<&str>, items: Vec<Value>) -> anyhow::Result<usize> {
    if items.is_empty() || !wanted(&subs(db, account_id).await, topic, metric) {
        return Ok(0);
    }
    let targets: Vec<Target> = sqlx::query_as(
        "SELECT id, filter FROM webhook_endpoints WHERE account_id = $1 AND enabled AND $2 = ANY(topics)
           AND ($3::text IS NULL OR cardinality(metrics) = 0 OR $3 = ANY(metrics))",
    )
    .bind(account_id)
    .bind(topic)
    .bind(metric)
    .fetch_all(db)
    .await?;
    if targets.is_empty() {
        return Ok(0);
    }
    let mut queued = 0;
    let mut woken: Vec<Uuid> = Vec::new();
    for chunk in items.chunks(500) {
        let (mut ids, mut eps, mut events, mut payloads) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for data in chunk {
            let event_id = Uuid::now_v7();
            let env = envelope(event_id, account_id, topic, data.clone());
            for t in &targets {
                if passes(&t.filter, data) {
                    ids.push(Uuid::now_v7());
                    eps.push(t.id);
                    events.push(event_id);
                    payloads.push(env.clone());
                }
            }
        }
        if ids.is_empty() {
            continue;
        }
        queued += ids.len();
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, account_id, endpoint_id, event_id, topic, payload)
             SELECT x.id, $2, x.ep, x.ev, $3, x.payload FROM UNNEST($1::uuid[], $4::uuid[], $5::uuid[], $6::jsonb[]) AS x(id, ep, ev, payload)",
        )
        .bind(&ids)
        .bind(account_id)
        .bind(topic)
        .bind(&eps)
        .bind(&events)
        .bind(&payloads)
        .execute(db)
        .await?;
        for e in eps {
            if !woken.contains(&e) {
                woken.push(e);
            }
        }
    }
    deliver::wake(db, &woken).await?;
    Ok(queued)
}

// ---------- Hooks the rest of the app calls where things change ----------

pub(crate) fn profile_summary(p: &Profile) -> Value {
    json!({"id": p.id, "email": p.email, "phone_number": p.phone_number, "external_id": p.external_id, "first_name": p.first_name, "last_name": p.last_name})
}

async fn summaries(db: &PgPool, account_id: Uuid, ids: &[Uuid]) -> anyhow::Result<HashMap<Uuid, Value>> {
    let rows: Vec<(Uuid, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, email, phone_number, external_id, first_name, last_name FROM profiles WHERE account_id = $1 AND id = ANY($2)",
    )
    .bind(account_id)
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.0, json!({"id": r.0, "email": r.1, "phone_number": r.2, "external_id": r.3, "first_name": r.4, "last_name": r.5})))
        .collect())
}

/// After a profile is created or saved: `profile.created`, or `profile.updated` when something
/// actually changed, plus `profile.subscribed` / `profile.unsubscribed` for consent changes.
pub async fn profile_saved(db: &PgPool, account_id: Uuid, before: Option<&Profile>, after: &Profile) {
    let subs = subs(db, account_id).await;
    if !["profile.created", "profile.updated", "profile.subscribed", "profile.unsubscribed"].iter().any(|t| wanted(&subs, t, None)) {
        return;
    }
    let full = serde_json::to_value(after).unwrap_or(Value::Null);
    let summary = profile_summary(after);
    let consent = |b: Option<&str>, a: &str, channel: &str| -> Option<(&'static str, Value)> {
        if b == Some(a) {
            return None;
        }
        let reason = if b.is_none() { "created" } else { "profile_update" };
        match a {
            "subscribed" => Some(("profile.subscribed", json!({"channel": channel, "reason": reason, "profile": summary}))),
            "unsubscribed" if b.is_some() => Some(("profile.unsubscribed", json!({"channel": channel, "reason": reason, "profile": summary}))),
            _ => None,
        }
    };
    let mut out: Vec<(&str, Value)> = Vec::new();
    match before {
        None => out.push(("profile.created", json!({"profile": full}))),
        Some(b) => {
            let mut changed = Vec::new();
            for (k, same) in [
                ("email", b.email == after.email),
                ("phone_number", b.phone_number == after.phone_number),
                ("external_id", b.external_id == after.external_id),
                ("first_name", b.first_name == after.first_name),
                ("last_name", b.last_name == after.last_name),
                ("properties", b.properties == after.properties),
                ("email_consent", b.email_consent == after.email_consent),
                ("sms_consent", b.sms_consent == after.sms_consent),
            ] {
                if !same {
                    changed.push(k);
                }
            }
            if !changed.is_empty() {
                out.push(("profile.updated", json!({"profile": full, "changed": changed})));
            }
        }
    }
    out.extend(consent(before.map(|b| b.email_consent.as_str()), &after.email_consent, "email"));
    out.extend(consent(before.map(|b| b.sms_consent.as_str()), &after.sms_consent, "sms"));
    for (topic, data) in out {
        emit(db, account_id, topic, data).await;
    }
}

/// A profile was deleted. `summary` is what it was ({id, email, phone_number, external_id}).
pub async fn profile_deleted(db: &PgPool, account_id: Uuid, summary: Value) {
    if wants(db, account_id, "profile.deleted").await {
        emit(db, account_id, "profile.deleted", json!({"profile": summary})).await;
    }
}

/// Consent changed outside a profile save (e.g. double opt-in confirmed).
pub async fn consent_changed(db: &PgPool, account_id: Uuid, profile_id: Uuid, channel: &str, subscribed: bool, reason: &str) {
    let topic = if subscribed { "profile.subscribed" } else { "profile.unsubscribed" };
    if !wants(db, account_id, topic).await {
        return;
    }
    let profile = summaries(db, account_id, &[profile_id]).await.ok().and_then(|mut m| m.remove(&profile_id)).unwrap_or(json!({"id": profile_id}));
    emit(db, account_id, topic, json!({"channel": channel, "reason": reason, "profile": profile})).await;
}

/// Profiles joined (`added`) or left a list.
pub async fn list_changed(db: &PgPool, account_id: Uuid, list_id: Uuid, profile_ids: &[Uuid], added: bool) {
    let topic = if added { "list.member_added" } else { "list.member_removed" };
    if profile_ids.is_empty() || !wants(db, account_id, topic).await {
        return;
    }
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM lists WHERE id = $1").bind(list_id).fetch_optional(db).await.ok().flatten();
    let list = json!({"id": list_id, "name": name});
    for chunk in profile_ids.chunks(1000) {
        let people = summaries(db, account_id, chunk).await.unwrap_or_default();
        let items = chunk.iter().map(|id| json!({"list": list, "profile": people.get(id).cloned().unwrap_or(json!({"id": id}))})).collect();
        emit_many(db, account_id, topic, items).await;
    }
}

/// A segment refresh found people who entered and left.
pub async fn segment_changed(db: &PgPool, account_id: Uuid, segment_id: Uuid, entered: &[Uuid], left: &[Uuid]) {
    let subs = subs(db, account_id).await;
    let (want_in, want_out) = (wanted(&subs, "segment.entered", None), wanted(&subs, "segment.left", None));
    if !want_in && !want_out {
        return;
    }
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM segments WHERE id = $1").bind(segment_id).fetch_optional(db).await.ok().flatten();
    let segment = json!({"id": segment_id, "name": name});
    for (topic, ids, on) in [("segment.entered", entered, want_in), ("segment.left", left, want_out)] {
        if !on {
            continue;
        }
        for chunk in ids.chunks(1000) {
            // People who left may be deleted already; they still get an event with their id.
            let people = summaries(db, account_id, chunk).await.unwrap_or_default();
            let items = chunk.iter().map(|id| json!({"segment": segment, "profile": people.get(id).cloned().unwrap_or(json!({"id": id}))})).collect();
            emit_many(db, account_id, topic, items).await;
        }
    }
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: Uuid,
    account_id: Uuid,
    profile_id: Uuid,
    channel: String,
    subject: Option<String>,
    status: String,
    campaign_id: Option<Uuid>,
    flow_id: Option<Uuid>,
    flow_node_id: Option<String>,
    sent_at: Option<chrono::DateTime<Utc>>,
    is_test: bool,
}

async fn message_row(db: &PgPool, id: Uuid) -> anyhow::Result<Option<MessageRow>> {
    Ok(sqlx::query_as("SELECT id, account_id, profile_id, channel, subject, status, campaign_id, flow_id, flow_node_id, sent_at, is_test FROM messages WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?)
}

fn message_json(m: &MessageRow) -> Value {
    json!({"id": m.id, "channel": m.channel, "subject": m.subject, "status": m.status, "campaign_id": m.campaign_id, "flow_id": m.flow_id, "flow_node_id": m.flow_node_id, "sent_at": m.sent_at})
}

/// The provider confirmed a message was delivered.
pub async fn message_delivered(db: &PgPool, message_id: Uuid) {
    let Ok(Some(m)) = message_row(db, message_id).await else { return };
    if m.is_test || !wants(db, m.account_id, "message.delivered").await {
        return;
    }
    let profile = summaries(db, m.account_id, &[m.profile_id]).await.ok().and_then(|mut x| x.remove(&m.profile_id)).unwrap_or(json!({"id": m.profile_id}));
    emit(db, m.account_id, "message.delivered", json!({"message": message_json(&m), "profile": profile})).await;
}

/// Someone signed up through a form.
pub async fn form_submitted(db: &PgPool, account_id: Uuid, form_id: Uuid, profile_id: Uuid, variant: Option<&str>) {
    if !wants(db, account_id, "form.submitted").await {
        return;
    }
    let form: Option<(String, String, Option<Uuid>)> =
        sqlx::query_as("SELECT name, kind, list_id FROM forms WHERE id = $1 AND account_id = $2").bind(form_id).bind(account_id).fetch_optional(db).await.ok().flatten();
    let Some((name, kind, list_id)) = form else { return };
    let profile = summaries(db, account_id, &[profile_id]).await.ok().and_then(|mut x| x.remove(&profile_id)).unwrap_or(json!({"id": profile_id}));
    emit(db, account_id, "form.submitted", json!({"form": {"id": form_id, "name": name, "kind": kind}, "list_id": list_id, "variant": variant.unwrap_or("a"), "profile": profile})).await;
}

/// `campaign.sent` (started sending) or `campaign.finished`.
pub async fn campaign_event(db: &PgPool, campaign_id: Uuid, topic: &str) {
    let row: Option<(Uuid, String, String, String, Option<chrono::DateTime<Utc>>)> =
        sqlx::query_as("SELECT account_id, name, channel, status, sent_at FROM campaigns WHERE id = $1").bind(campaign_id).fetch_optional(db).await.ok().flatten();
    let Some((account_id, name, channel, status, sent_at)) = row else { return };
    if !wants(db, account_id, topic).await {
        return;
    }
    let recipients: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE campaign_id = $1 AND NOT is_test").bind(campaign_id).fetch_one(db).await.unwrap_or(0);
    emit(db, account_id, topic, json!({"campaign": {"id": campaign_id, "name": name, "channel": channel, "status": status, "sent_at": sent_at, "recipients": recipients}})).await;
}

/// A flow was turned on, paused or set to draft.
pub async fn flow_status_changed(db: &PgPool, account_id: Uuid, flow_id: Uuid, name: &str, previous: &str, status: &str, trigger: &Value) {
    if previous != status && wants(db, account_id, "flow.status_changed").await {
        emit(db, account_id, "flow.status_changed", json!({"flow": {"id": flow_id, "name": name, "status": status, "previous_status": previous, "trigger": trigger}})).await;
    }
}

/// A profile import finished. `import` holds its counts.
pub async fn import_finished(db: &PgPool, account_id: Uuid, import: Value) {
    if wants(db, account_id, "import.finished").await {
        emit(db, account_id, "import.finished", json!({"import": import})).await;
    }
}

/// `event.tracked`, and the message and consent topics that built-in events stand for.
async fn event_hook(db: &PgPool, event_id: Uuid) -> anyhow::Result<()> {
    #[derive(sqlx::FromRow)]
    struct Ev {
        account_id: Uuid,
        profile_id: Uuid,
        metric: String,
        properties: Value,
        value: Option<f64>,
        occurred_at: chrono::DateTime<Utc>,
        unique_id: Option<String>,
        message_id: Option<Uuid>,
    }
    let ev: Option<Ev> = sqlx::query_as(
        "SELECT e.account_id, e.profile_id, m.name AS metric, e.properties, e.value, e.occurred_at, e.unique_id, e.message_id
         FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.id = $1",
    )
    .bind(event_id)
    .fetch_optional(db)
    .await?;
    let Some(ev) = ev else { return Ok(()) };
    let subs = subs(db, ev.account_id).await;
    let tracked = wanted(&subs, "event.tracked", Some(&ev.metric));
    let derived: Vec<&str> = topics::derived(&ev.metric).iter().copied().filter(|t| wanted(&subs, t, None)).collect();
    if !tracked && derived.is_empty() {
        return Ok(());
    }
    let state: Option<(Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, String, String, Option<String>)> = sqlx::query_as(
        "SELECT email, phone_number, external_id, first_name, last_name, email_consent, sms_consent, email_suppressed_reason FROM profiles WHERE id = $1",
    )
    .bind(ev.profile_id)
    .fetch_optional(db)
    .await?;
    let Some((email, phone, external_id, first, last, email_consent, sms_consent, suppressed)) = state else { return Ok(()) };
    let profile = json!({"id": ev.profile_id, "email": email, "phone_number": phone, "external_id": external_id, "first_name": first, "last_name": last});
    if tracked {
        let data = json!({
            "event": {"id": event_id, "metric": ev.metric, "properties": ev.properties, "value": ev.value, "occurred_at": ev.occurred_at, "unique_id": ev.unique_id, "message_id": ev.message_id},
            "profile": profile,
        });
        enqueue(db, ev.account_id, "event.tracked", Some(&ev.metric), vec![data]).await?;
    }
    if derived.is_empty() {
        return Ok(());
    }
    let message = match ev.message_id {
        Some(id) => message_row(db, id).await?.filter(|m| m.account_id == ev.account_id),
        None => None,
    };
    let sms = ev.metric.contains("SMS");
    for topic in derived {
        let data = match topic {
            "message.sent" | "message.opened" | "message.clicked" | "message.bounced" | "message.complained" => {
                let Some(m) = &message else { continue };
                let mut d = json!({"message": message_json(m), "profile": profile});
                if topic == "message.clicked" {
                    d["url"] = ev.properties.get("url").cloned().unwrap_or(Value::Null);
                }
                if topic == "message.bounced" {
                    d["bounce_type"] = ev.properties.get("bounce_type").cloned().unwrap_or(Value::Null);
                }
                d
            }
            "profile.suppressed" => {
                let permanent = ev.metric == "Marked Email as Spam" || ev.properties["bounce_type"].as_str() == Some("Permanent");
                let Some(reason) = suppressed.as_deref().filter(|_| permanent) else { continue };
                json!({"channel": "email", "reason": reason, "message_id": ev.message_id, "profile": profile})
            }
            "profile.unsubscribed" => {
                let (channel, now) = if sms { ("sms", &sms_consent) } else { ("email", &email_consent) };
                if now != "unsubscribed" {
                    continue;
                }
                let reason = if ev.metric == "Marked Email as Spam" {
                    "spam_complaint".to_string()
                } else if ev.properties.get("keyword").is_some() {
                    "stop_keyword".to_string()
                } else {
                    match ev.properties["method"].as_str() {
                        Some("link") | None => "unsubscribe_link".to_string(),
                        Some(m) => m.chars().take(40).collect(),
                    }
                };
                json!({"channel": channel, "reason": reason, "message_id": ev.message_id, "profile": profile})
            }
            "profile.subscribed" => {
                if sms_consent != "subscribed" {
                    continue;
                }
                json!({"channel": "sms", "reason": "start_keyword", "profile": profile})
            }
            _ => continue,
        };
        enqueue(db, ev.account_id, topic, None, vec![data]).await?;
    }
    Ok(())
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

/// Email the account's first member a notice from SUPPORT_EMAIL. Failures are only logged.
pub async fn alert_account(state: &AppState, account_id: Uuid, subject: &str, text: &str) {
    let Some(from) = state.cfg.support_email.as_deref().filter(|s| s.contains('@')) else {
        tracing::warn!(account = %account_id, subject, "no SUPPORT_EMAIL set; account alert not emailed");
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
    let email = crate::providers::email::OutgoingEmail {
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
        tracing::error!(account = %account_id, error = %format!("{e:#}"), "couldn't email account alert");
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
    fn metric_filter_applies_only_to_tracked_events() {
        let subs = vec![Sub { topics: vec!["event.tracked".into(), "message.opened".into()], metrics: vec!["Placed Order".into()] }];
        assert!(wanted(&subs, "event.tracked", Some("Placed Order")));
        assert!(!wanted(&subs, "event.tracked", Some("Viewed Product")));
        assert!(wanted(&subs, "message.opened", None));
        assert!(!wanted(&subs, "profile.created", None));
    }
}
