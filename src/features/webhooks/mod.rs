//! Outgoing webhooks, REST hooks for Zapier and Make, and the signature on flow webhook steps.
//!
//! Watching a change must never break or slow the change. So the rest of the app reports changes
//! through the small `pub` functions here ([`emit`], [`profile_saved`], [`list_changed`] ...),
//! and each one first asks the server's snapshot of who is subscribed (`features::observe`, no
//! SQL): an account with no endpoint wanting the topic costs nothing, not even building the
//! payload. Otherwise it does one append-only `INSERT` into `webhook_outbox`: inside the caller's
//! transaction when there is one (under a savepoint with short timeouts, so it can neither spoil
//! nor stall it), otherwise in a short transaction of its own. Failures are logged and counted,
//! never returned.
//!
//! The insert also queues a `webhooks_fanout` job for the account in the background job lane (see
//! `jobs::BACKGROUND`) unless one is waiting, which turns outbox rows into per-endpoint deliveries
//! in bounded batches (`fanout`). A `webhooks_deliver` job per endpoint sends them (`deliver`), one
//! request at a time, so a slow or dead endpoint never holds up sending, other accounts or other
//! endpoints. Upkeep (expiry, disabling, missed wake-ups) runs as a `webhooks_sweep` job in the
//! same lane, queued by the scheduler.

mod api;
mod deliver;
mod fanout;
pub mod net;
mod tools;
pub mod topics;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::team::access::Access;
use super::{Feature, McpDoc};
pub use crate::audience::EventSource as Source;
pub use fanout::{max_outbox, max_pending, set_limits_for_tests};
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
            "webhooks_sweep" => Some(sweep(state, payload["hourly"].as_bool() == Some(true)).await),
            _ => None,
        }
    }

    /// The scheduler only queues the upkeep; it runs in the background lane, off the shared tick.
    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(3) {
            sqlx::query(
                "INSERT INTO jobs (kind, payload, lane) SELECT 'webhooks_sweep', jsonb_build_object('hourly', $1::bool), 'background'
                 WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE kind = 'webhooks_sweep' AND lane = 'background' AND locked_at IS NULL)",
            )
            .bind(tick.is_multiple_of(180))
            .execute(&state.db)
            .await?;
        }
        Ok(())
    }

    fn access(&self) -> Access {
        Access { routes: api::ROUTES, tools: tools::TOOLS }
    }
}

/// Upkeep: fan out what was missed, expire and disable, count the outbox; hourly, prune and recount.
async fn sweep(state: &AppState, hourly: bool) -> anyhow::Result<()> {
    fanout::sweep(state).await?;
    deliver::sweep(state).await?;
    if hourly {
        deliver::prune(&state.db).await?;
        deliver::recount(&state.db).await?;
    }
    Ok(())
}

// ---------- Who is subscribed ----------

use crate::features::observe::{self, Sub};

fn wanted(subs: &[Sub], topic: &str, metric: Option<&str>, source: Source) -> bool {
    subs.iter().any(|s| {
        s.topics.iter().any(|t| t == topic)
            && (metric.is_none() || s.metrics.is_empty() || metric.is_some_and(|m| s.metrics.iter().any(|x| x == m)))
            && !(s.trusted_only && source == Source::Browser)
    })
}

/// What the account's endpoints want, from the server's snapshot (no SQL). `Some(false)`: nothing
/// configured for any of `topics`, so don't build anything. `None`: the snapshot isn't loaded yet;
/// the insert checks.
fn wants(db: &PgPool, account_id: Uuid, topics: &[&str]) -> bool {
    match observe::peek(db) {
        Some(i) => i.account(account_id).is_some_and(|a| topics.iter().any(|t| wanted(&a.subs, t, None, Source::System) || wanted(&a.subs, t, None, Source::Browser))),
        None => true,
    }
}

/// Keep the rows some endpoint of the account may want, within the account's outbox limit.
fn keep(db: &PgPool, account_id: Uuid, rows: Vec<Out>) -> Vec<Out> {
    let mut rows: Vec<Out> = match observe::peek(db) {
        Some(i) => match i.account(account_id) {
            Some(a) => rows.into_iter().filter(|r| wanted(&a.subs, &r.topic, r.metric.as_deref(), r.source)).collect(),
            None => return Vec::new(),
        },
        None => rows,
    };
    let room = observe::outbox_room(db, account_id, rows.len() as i64, max_outbox()) as usize;
    if room < rows.len() {
        dropped(db, account_id, (rows.len() - room) as i64);
        rows.truncate(room);
    }
    rows
}

/// Reload the subscriptions after an endpoint changes on this server.
async fn forget(db: &PgPool) {
    observe::changed(db).await;
}

type DropCounts = Mutex<HashMap<Uuid, i64>>;
static DROPPED: LazyLock<DropCounts> = LazyLock::new(Default::default);

/// Rows not queued because the account's outbox was full: counted on its endpoints a second later
/// (one update for all the drops of that second), off the caller's path. The sweep tells the owner.
fn dropped(db: &PgPool, account_id: Uuid, n: i64) {
    let first = match DROPPED.lock() {
        Ok(mut m) => {
            let e = m.entry(account_id).or_insert(0);
            *e += n;
            *e == n
        }
        Err(_) => return,
    };
    tracing::warn!(account = %account_id, rows = n, "webhook outbox full; events dropped");
    if first {
        let db = db.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let n = DROPPED.lock().ok().and_then(|mut m| m.remove(&account_id)).unwrap_or(0);
            let res = sqlx::query("UPDATE webhook_endpoints SET dropped_total = dropped_total + $2, last_dropped_at = now() WHERE account_id = $1 AND enabled")
                .bind(account_id)
                .bind(n)
                .execute(&db)
                .await;
            if let Err(e) = res {
                tracing::error!(account = %account_id, rows = n, error = %e, "couldn't count dropped webhook events");
            }
        });
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

/// After an outbox insert (a CTE named `o` returning a row per inserted row): queue a fan-out job
/// for the account unless one is already waiting. Both are plain inserts: nothing here waits on a
/// row lock. Two writers at once may both queue one; the second finds nothing left to do.
macro_rules! wake_fanout {
    () => {
        "INSERT INTO jobs (kind, payload, lane)
         SELECT 'webhooks_fanout', jsonb_build_object('account_id', $1::uuid), 'background'
         WHERE EXISTS (SELECT 1 FROM o)
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'webhooks_fanout' AND j.lane = 'background' AND j.locked_at IS NULL AND j.payload->>'account_id' = $1::text)"
    };
}
pub(super) use wake_fanout;

/// Rows only an endpoint wants are kept (the snapshot may be a few seconds old).
const OUTBOX_INSERT: &str = concat!(
    "WITH o AS (
        INSERT INTO webhook_outbox (account_id, event_id, topic, metric, source, data)
        SELECT $1, x.ev, x.topic, x.metric, x.source, x.data
        FROM UNNEST($2::uuid[], $3::text[], $4::text[], $5::text[], $6::jsonb[]) AS x(ev, topic, metric, source, data)
        WHERE EXISTS (SELECT 1 FROM webhook_endpoints e WHERE e.account_id = $1 AND e.enabled AND x.topic = ANY(e.topics)
                        AND (x.metric IS NULL OR cardinality(e.metrics) = 0 OR x.metric = ANY(e.metrics))
                        AND NOT (e.trusted_only AND x.source = 'browser'))
        RETURNING 1
    ) ",
    wake_fanout!()
);

/// Write rows (already filtered by [`keep`]) on a connection the caller set up with `observe`.
pub(crate) async fn write_out(conn: &mut PgConnection, account_id: Uuid, rows: &[Out]) -> sqlx::Result<()> {
    for chunk in rows.chunks(500) {
        let ids: Vec<Uuid> = chunk.iter().map(|r| r.event_id).collect();
        let topics: Vec<&str> = chunk.iter().map(|r| r.topic.as_str()).collect();
        let metrics: Vec<Option<&str>> = chunk.iter().map(|r| r.metric.as_deref()).collect();
        let sources: Vec<&str> = chunk.iter().map(|r| r.source.as_str()).collect();
        let data: Vec<&Value> = chunk.iter().map(|r| &r.data).collect();
        sqlx::query(OUTBOX_INSERT).bind(account_id).bind(&ids).bind(&topics).bind(&metrics).bind(&sources).bind(&data).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Queue `rows` for the account's endpoints in a short transaction of their own. Never fails or
/// holds up the caller (see `observe`).
pub(crate) async fn emit_rows(db: &PgPool, account_id: Uuid, rows: Vec<Out>) {
    let rows = keep(db, account_id, rows);
    if rows.is_empty() {
        return;
    }
    let Some(mut tx) = observe::begin(db, "webhooks").await else { return };
    let res = write_out(&mut tx, account_id, &rows).await;
    observe::commit(tx, res, "webhooks").await;
}

/// Queue `rows` inside the caller's transaction, so they go out only if it commits; under a
/// savepoint with short timeouts, so it can neither spoil nor stall that transaction.
pub(crate) async fn emit_rows_in(conn: &mut PgConnection, db: &PgPool, account_id: Uuid, rows: Vec<Out>) {
    let rows = keep(db, account_id, rows);
    if rows.is_empty() {
        return;
    }
    let Some(saved) = observe::enter(conn, "webhooks").await else { return };
    let res = write_out(conn, account_id, &rows).await;
    observe::leave(conn, saved, res, "webhooks").await;
}

/// Send `data` to every endpoint of the account subscribed to `topic`. Never fails the caller.
pub async fn emit(db: &PgPool, account_id: Uuid, topic: &str, source: Source, data: Value) {
    if wants(db, account_id, &[topic]) {
        emit_rows(db, account_id, vec![Out::new(topic, source, data)]).await
    }
}

fn profile_ref(id: Uuid) -> Value {
    json!({"$profile": id})
}

// ---------- Hooks the rest of the app calls where things change ----------

pub(crate) fn profile_summary(p: &Profile) -> Value {
    json!({"id": p.id, "email": p.email, "phone_number": p.phone_number, "external_id": p.external_id, "first_name": p.first_name, "last_name": p.last_name})
}

/// An event was recorded: `event.tracked`, and for DumbMail's own message events the message
/// topic it stands for (`message.sent`, `message.opened` ...). The rows to write, inside
/// `audience::record_event`'s transaction; empty (and nothing built) when no endpoint wants them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn event_rows(db: &PgPool, account_id: Uuid, event_id: Uuid, metric: &str, source: Source, message_id: Option<Uuid>, properties: &Value) -> Vec<Out> {
    let message_topic = match (source, message_id) {
        // Message topics come only from DumbMail's own records of its messages, never from an
        // event someone sent in with the same name.
        (Source::System, Some(_)) => topics::message_topic(metric),
        _ => None,
    };
    let mut want = vec!["event.tracked"];
    want.extend(message_topic);
    if !wants(db, account_id, &want) {
        return Vec::new();
    }
    let mut rows = vec![Out { event_id, topic: "event.tracked".into(), metric: Some(metric.to_string()), source, data: json!({"$event": event_id}) }];
    if let (Some(topic), Some(mid)) = (message_topic, message_id) {
        let mut data = json!({"$message": mid});
        match topic {
            "message.clicked" => data["url"] = properties.get("url").cloned().unwrap_or(Value::Null),
            "message.bounced" => data["bounce_type"] = properties.get("bounce_type").cloned().unwrap_or(Value::Null),
            _ => {}
        }
        rows.push(Out { event_id: stable_id(&[topic, &event_id.to_string()]), topic: topic.into(), metric: None, source, data });
    }
    keep(db, account_id, rows)
}

const PROFILE_TOPICS: &[&str] = &["profile.created", "profile.updated", "profile.subscribed", "profile.unsubscribed"];

/// After a profile is created or saved: `profile.created`, or `profile.updated` when something
/// actually changed, plus `profile.subscribed` / `profile.unsubscribed` for consent changes.
/// `consent_before` is (email, sms) consent as the saving statement found it (None for a new
/// profile), so a transition is reported only when that write made it.
pub async fn profile_saved(db: &PgPool, account_id: Uuid, before: Option<&Profile>, consent_before: Option<(&str, &str)>, after: &Profile, source: Source) {
    if !wants(db, account_id, PROFILE_TOPICS) {
        return;
    }
    let summary = profile_summary(after);
    let mut out: Vec<Out> = Vec::new();
    match before {
        None => out.push(Out::new("profile.created", source, json!({"profile": after}))),
        Some(b) => {
            let (email_was, sms_was) = consent_before.unwrap_or((b.email_consent.as_str(), b.sms_consent.as_str()));
            let changed: Vec<&str> = [
                ("email", b.email == after.email),
                ("phone_number", b.phone_number == after.phone_number),
                ("external_id", b.external_id == after.external_id),
                ("first_name", b.first_name == after.first_name),
                ("last_name", b.last_name == after.last_name),
                ("properties", b.properties == after.properties),
                ("email_consent", email_was == after.email_consent),
                ("sms_consent", sms_was == after.sms_consent),
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
    let (email_was, sms_was) = match (before, consent_before) {
        (None, _) => ("never_subscribed", "never_subscribed"),
        (Some(_), Some(c)) => c,
        (Some(b), None) => (b.email_consent.as_str(), b.sms_consent.as_str()),
    };
    for (channel, b, a) in [("email", email_was, after.email_consent.as_str()), ("sms", sms_was, after.sms_consent.as_str())] {
        if let Some(o) = consent_row(channel, b, a, reason, None, source, summary.clone()) {
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
    if !wants(db, account_id, &["profile.subscribed", "profile.unsubscribed"]) {
        return;
    }
    if let Some(o) = consent_row(channel, before, after, reason, message_id, Source::System, profile_ref(profile_id)) {
        emit_rows(db, account_id, vec![o]).await
    }
}

/// An address was suppressed (`hard_bounce` or `complaint`) and wasn't before.
pub async fn suppressed(db: &PgPool, account_id: Uuid, profile_id: Uuid, reason: &str, message_id: Uuid) {
    if !wants(db, account_id, &["profile.suppressed"]) {
        return;
    }
    let data = json!({"channel": "email", "reason": reason, "message_id": message_id, "profile": profile_ref(profile_id)});
    let o = Out { event_id: stable_id(&["profile.suppressed", &message_id.to_string()]), ..Out::new("profile.suppressed", Source::System, data) };
    emit_rows(db, account_id, vec![o]).await
}

/// A profile was deleted. `summary` is what it was ({id, email, phone_number, external_id}).
pub async fn profile_deleted(db: &PgPool, account_id: Uuid, summary: Value) {
    emit(db, account_id, "profile.deleted", Source::Api, json!({"profile": summary})).await
}

/// Whether an endpoint of the account wants list membership changes (`added` or removed). Lets the
/// caller skip collecting who changed.
pub fn wants_list_changes(db: &PgPool, account_id: Uuid, added: bool) -> bool {
    wants(db, account_id, &[if added { "list.member_added" } else { "list.member_removed" }])
}

/// Profiles joined (`added`) or left a list.
pub async fn list_changed(db: &PgPool, account_id: Uuid, list_id: Uuid, profile_ids: &[Uuid], added: bool) {
    let topic = if added { "list.member_added" } else { "list.member_removed" };
    if profile_ids.is_empty() || !wants(db, account_id, &[topic]) {
        return;
    }
    let Some(ids) = members_room(db, account_id, profile_ids) else { return };
    let Some(mut tx) = observe::begin(db, "webhooks").await else { return };
    let res = fanout::insert_members(&mut tx, account_id, topic, "list", list_id, ids).await;
    observe::commit(tx, res, "webhooks").await;
}

fn members_room<'a>(db: &PgPool, account_id: Uuid, ids: &'a [Uuid]) -> Option<&'a [Uuid]> {
    let room = observe::outbox_room(db, account_id, ids.len() as i64, max_outbox()) as usize;
    if room < ids.len() {
        dropped(db, account_id, (ids.len() - room) as i64);
    }
    (room > 0).then(|| &ids[..room])
}

/// Whether an endpoint of the account wants segment entries or exits. Lets a segment refresh skip
/// collecting who left.
pub fn wants_segment_changes(db: &PgPool, account_id: Uuid) -> bool {
    wants(db, account_id, &["segment.entered", "segment.left"])
}

/// A segment refresh found people who entered and left. Called inside the refresh's transaction.
pub async fn segment_changed_in(conn: &mut PgConnection, db: &PgPool, account_id: Uuid, segment_id: Uuid, entered: &[Uuid], left: &[Uuid]) {
    let mut work: Vec<(&str, &[Uuid])> = Vec::new();
    for (topic, ids) in [("segment.entered", entered), ("segment.left", left)] {
        if ids.is_empty() || !wants(db, account_id, &[topic]) {
            continue;
        }
        if let Some(ids) = members_room(db, account_id, ids) {
            work.push((topic, ids));
        }
    }
    if work.is_empty() {
        return;
    }
    let Some(saved) = observe::enter(conn, "webhooks").await else { return };
    let mut res = Ok(());
    for (topic, ids) in work {
        res = fanout::insert_members(&mut *conn, account_id, topic, "segment", segment_id, ids).await;
        if res.is_err() {
            break;
        }
    }
    observe::leave(conn, saved, res, "webhooks").await;
}

/// Messages ended `skipped` or `failed`: `message.skipped` / `message.failed`, with the reason.
/// One statement over the ids (reading what the write left), for any number of them. The event id
/// is the message's and its status, so a second report of the same outcome goes out once.
const UNSENT_INSERT: &str = concat!(
    "WITH o AS (
        INSERT INTO webhook_outbox (account_id, event_id, topic, source, data)
        SELECT $1, md5('message.' || m.status || ':' || m.id::text)::uuid, 'message.' || m.status, 'system',
               jsonb_build_object('$message', m.id, 'reason', left(m.error, 200))
        FROM messages m
        WHERE m.account_id = $1 AND m.id = ANY($2) AND m.status IN ('skipped', 'failed') AND NOT m.is_test
          AND EXISTS (SELECT 1 FROM webhook_endpoints e WHERE e.account_id = $1 AND e.enabled AND ('message.' || m.status) = ANY(e.topics))
        RETURNING 1
    ) ",
    wake_fanout!()
);

/// Whether an endpoint of the account wants skipped or failed messages.
pub fn wants_unsent(db: &PgPool, account_id: Uuid) -> bool {
    wants(db, account_id, &["message.skipped", "message.failed"])
}

/// Write `message.skipped` / `message.failed` rows for these ids on a connection set up with `observe`.
pub(crate) async fn write_unsent(conn: &mut PgConnection, db: &PgPool, account_id: Uuid, ids: &[Uuid]) -> sqlx::Result<()> {
    let Some(ids) = members_room(db, account_id, ids) else { return Ok(()) };
    for chunk in ids.chunks(5_000) {
        sqlx::query(UNSENT_INSERT).bind(account_id).bind(chunk).execute(&mut *conn).await?;
    }
    Ok(())
}

/// The provider confirmed a message was delivered (called only the first time it does).
pub async fn message_delivered(db: &PgPool, account_id: Uuid, message_id: Uuid) {
    if !wants(db, account_id, &["message.delivered"]) {
        return;
    }
    let o = Out { event_id: stable_id(&["message.delivered", &message_id.to_string()]), ..Out::new("message.delivered", Source::System, json!({"$message": message_id})) };
    emit_rows(db, account_id, vec![o]).await
}

/// Someone signed up through a form.
pub async fn form_submitted(db: &PgPool, account_id: Uuid, form_id: Uuid, profile_id: Uuid, variant: Option<&str>) {
    if !wants(db, account_id, &["form.submitted"]) {
        return;
    }
    emit_rows(db, account_id, vec![Out::new("form.submitted", Source::Browser, json!({"form": {"$form": form_id}, "variant": variant.unwrap_or("a"), "profile": profile_ref(profile_id)}))]).await
}

/// A campaign started sending. Called inside the transaction that marks it sent.
pub async fn campaign_sent_in(conn: &mut PgConnection, db: &PgPool, account_id: Uuid, campaign_id: Uuid) {
    if !wants(db, account_id, &["campaign.sent"]) {
        return;
    }
    let o = Out { event_id: stable_id(&["campaign.sent", &campaign_id.to_string()]), ..Out::new("campaign.sent", Source::System, json!({"campaign": {"$campaign": campaign_id}})) };
    emit_rows_in(conn, db, account_id, vec![o]).await
}

/// A campaign finished sending.
pub async fn campaign_finished(db: &PgPool, account_id: Uuid, campaign_id: Uuid) {
    if !wants(db, account_id, &["campaign.finished"]) {
        return;
    }
    let o =
        Out { event_id: stable_id(&["campaign.finished", &campaign_id.to_string()]), ..Out::new("campaign.finished", Source::System, json!({"campaign": {"$campaign": campaign_id}})) };
    emit_rows(db, account_id, vec![o]).await
}

/// A flow was turned on, paused or set to draft.
pub async fn flow_status_changed(db: &PgPool, account_id: Uuid, flow_id: Uuid, name: &str, previous: &str, status: &str, trigger: &Value) {
    if previous != status && wants(db, account_id, &["flow.status_changed"]) {
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
    fn stable_ids_repeat() {
        assert_eq!(stable_id(&["message.delivered", "a"]), stable_id(&["message.delivered", "a"]));
        assert_ne!(stable_id(&["message.delivered", "a"]), stable_id(&["message.delivered", "b"]));
        assert_ne!(stable_id(&["ab", "c"]), stable_id(&["a", "bc"]));
        assert_eq!(display_name("", "https://hooks.zapier.com/hooks/catch/1/secret/"), "the endpoint at hooks.zapier.com");
        assert_eq!(display_name("CRM", "https://crm.example.com/x?token=1"), "\"CRM\" (crm.example.com)");
    }
}
