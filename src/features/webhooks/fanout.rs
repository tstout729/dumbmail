//! Turning outbox rows into deliveries, in the background job lane.
//!
//! A `webhooks_fanout` job runs per account (an outbox insert queues one unless one is waiting). A
//! run takes up to [`OUTBOX_BATCH`] rows at a time, fills in the parts the emitter only referenced
//! (the event, the message, the profile ...), matches them against the account's endpoints and
//! writes the deliveries, all in one transaction, so a crash either keeps the rows in the outbox or
//! has written their deliveries. It stops after a few seconds and queues itself again if more is
//! waiting.
//!
//! An endpoint gets only rows captured since its current revision started (a URL change starts a
//! new one), so nothing captured for one address goes to another; and an event id reaches an
//! endpoint once, however many times it was reported.
//!
//! Backlogs are bounded: an endpoint keeps at most [`max_pending`] undelivered rows (the oldest are
//! dropped beyond that, counted on the endpoint, and the owner is told once), and an account keeps
//! at most [`max_outbox`] rows waiting to fan out (checked where rows are written, against the
//! sweep's count; see `observe::outbox_room`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::state::AppState;

pub(super) const OUTBOX_BATCH: i64 = 500;
const RUN_BUDGET: Duration = Duration::from_secs(10);

static MAX_PENDING: AtomicI64 = AtomicI64::new(100_000);
static MAX_OUTBOX: AtomicI64 = AtomicI64::new(200_000);

/// Most undelivered rows an endpoint keeps.
pub fn max_pending() -> i64 {
    MAX_PENDING.load(Ordering::Relaxed)
}

/// Most rows an account keeps waiting to fan out.
pub fn max_outbox() -> i64 {
    MAX_OUTBOX.load(Ordering::Relaxed)
}

/// Lower the backlog limits so a test can reach them. Never called by the server itself.
pub fn set_limits_for_tests(pending: i64, outbox: i64) {
    MAX_PENDING.store(pending, Ordering::Relaxed);
    MAX_OUTBOX.store(outbox, Ordering::Relaxed);
}

/// Membership changes (a list or a segment) for many profiles in one statement, without building
/// them in memory. Who they are is taken now (someone who left may be deleted by the time it fans
/// out). `kind` is "list" or "segment". The caller has checked the account's outbox has room.
pub(super) async fn insert_members(conn: &mut PgConnection, account_id: Uuid, topic: &str, kind: &str, group_id: Uuid, ids: &[Uuid]) -> sqlx::Result<()> {
    for chunk in ids.chunks(5_000) {
        sqlx::query(concat!(
            "WITH o AS (
                INSERT INTO webhook_outbox (account_id, event_id, topic, source, data)
                SELECT $1, md5($2 || ':' || $3::text || ':' || x.pid::text || ':' || clock_timestamp()::text)::uuid, $2, 'system',
                       jsonb_build_object($4::text, jsonb_build_object('$' || $4, $3), 'profile',
                           CASE WHEN p.id IS NULL THEN jsonb_build_object('id', x.pid)
                                ELSE jsonb_build_object('id', p.id, 'email', p.email, 'phone_number', p.phone_number, 'external_id', p.external_id,
                                                        'first_name', p.first_name, 'last_name', p.last_name) END)
                FROM unnest($5::uuid[]) AS x(pid) LEFT JOIN profiles p ON p.id = x.pid AND p.account_id = $1
                WHERE EXISTS (SELECT 1 FROM webhook_endpoints e WHERE e.account_id = $1 AND e.enabled AND $2 = ANY(e.topics))
                RETURNING 1
             ) ",
            super::wake_fanout!()
        ))
        .bind(account_id)
        .bind(topic)
        .bind(group_id)
        .bind(kind)
        .bind(chunk)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Queue a fan-out job for the account unless one is waiting.
async fn rearm(db: &PgPool, account_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO jobs (kind, payload, lane) SELECT 'webhooks_fanout', jsonb_build_object('account_id', $1::uuid), 'background'
         WHERE NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'webhooks_fanout' AND j.lane = 'background' AND j.locked_at IS NULL AND j.payload->>'account_id' = $1::text)",
    )
    .bind(account_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Job: fan out the account's outbox. A row written while this runs queues another job (this one
/// is claimed, so it doesn't count as waiting), so nothing is left behind.
pub(super) async fn run(state: &AppState, account_id: Uuid) -> anyhow::Result<()> {
    let db = &state.db;
    let started = Instant::now();
    loop {
        let (n, alerts) = batch(db, account_id).await?;
        for a in alerts {
            backlog_alert(state, &a).await;
        }
        if (n as i64) < OUTBOX_BATCH {
            return Ok(());
        }
        if started.elapsed() > RUN_BUDGET {
            rearm(db, account_id).await?;
            return Ok(());
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    event_id: Uuid,
    topic: String,
    metric: Option<String>,
    source: String,
    data: Value,
    created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct Ep {
    id: Uuid,
    revision: i32,
    revision_started_at: DateTime<Utc>,
    topics: Vec<String>,
    metrics: Vec<String>,
    filter: Option<Value>,
    trusted_only: bool,
}

impl Ep {
    fn wants(&self, r: &Row, data: &Value) -> bool {
        // Captured before this revision (an earlier URL): not this address's to receive.
        r.created_at >= self.revision_started_at
            && self.topics.contains(&r.topic)
            && (r.metric.is_none() || self.metrics.is_empty() || r.metric.as_ref().is_some_and(|m| self.metrics.contains(m)))
            && !(self.trusted_only && r.source == "browser")
            && passes(&self.filter, data)
    }
}

pub(super) fn passes(filter: &Option<Value>, data: &Value) -> bool {
    let Some(f) = filter else { return true };
    let Ok(conds) = serde_json::from_value::<Vec<crate::segments::Filter>>(f.clone()) else { return true };
    conds.iter().all(|c| crate::segments::json_filter_matches(data, c))
}

pub(super) struct BacklogAlert {
    account_id: Uuid,
    url: String,
    description: String,
}

/// One transaction: take a batch from the outbox, write its deliveries. Returns how many rows it took.
async fn batch(db: &PgPool, account_id: Uuid) -> anyhow::Result<(usize, Vec<BacklogAlert>)> {
    let mut tx = db.begin().await?;
    let mut rows: Vec<Row> = sqlx::query_as(
        "DELETE FROM webhook_outbox WHERE id IN (SELECT id FROM webhook_outbox WHERE account_id = $1 ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED)
         RETURNING id, event_id, topic, metric, source, data, created_at",
    )
    .bind(account_id)
    .bind(OUTBOX_BATCH)
    .fetch_all(&mut *tx)
    .await?;
    let n = rows.len();
    if n == 0 {
        tx.commit().await?;
        return Ok((0, Vec::new()));
    }
    rows.sort_by_key(|r| r.id);
    // A REST hook stops with its API key: a revoked key's hooks get nothing more.
    let eps: Vec<Ep> = sqlx::query_as(
        "SELECT e.id, e.revision, e.revision_started_at, e.topics, e.metrics, e.filter, e.trusted_only FROM webhook_endpoints e WHERE e.account_id = $1 AND e.enabled
           AND (e.api_key_id IS NULL OR EXISTS (SELECT 1 FROM api_keys k WHERE k.id = e.api_key_id AND k.revoked_at IS NULL))",
    )
        .bind(account_id)
        .fetch_all(&mut *tx)
        .await?;
    let lookups = Lookups::load(&mut tx, account_id, &rows).await?;
    let (mut ids, mut ep_ids, mut revs, mut events, mut topics, mut payloads) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // One delivery per event per endpoint, even when a change was reported twice (also enforced by
    // a unique index, against rows from an earlier batch).
    let mut seen: HashSet<(Uuid, Uuid)> = HashSet::new();
    for r in &rows {
        let Some(data) = lookups.fill(&r.data) else { continue };
        let env = json!({"id": r.event_id, "topic": r.topic, "created_at": r.created_at, "account_id": account_id, "api_version": super::API_VERSION,
            "source": r.source, "data": data});
        for e in eps.iter().filter(|e| e.wants(r, &data)) {
            if !seen.insert((e.id, r.event_id)) {
                continue;
            }
            ids.push(Uuid::now_v7());
            ep_ids.push(e.id);
            revs.push(e.revision);
            events.push(r.event_id);
            topics.push(r.topic.as_str());
            payloads.push(env.clone());
        }
    }
    let mut alerts = Vec::new();
    if !ids.is_empty() {
        let added: Vec<Uuid> = sqlx::query_scalar(
            "INSERT INTO webhook_deliveries (id, account_id, endpoint_id, revision, event_id, topic, payload)
             SELECT x.id, $2, x.ep, x.rev, x.ev, x.topic, x.payload FROM UNNEST($1::uuid[], $3::uuid[], $4::int[], $5::uuid[], $6::text[], $7::jsonb[]) AS x(id, ep, rev, ev, topic, payload)
             ON CONFLICT (endpoint_id, event_id) WHERE NOT is_test DO NOTHING
             RETURNING endpoint_id",
        )
        .bind(&ids)
        .bind(account_id)
        .bind(&ep_ids)
        .bind(&revs)
        .bind(&events)
        .bind(&topics)
        .bind(&payloads)
        .fetch_all(&mut *tx)
        .await?;
        let mut per: HashMap<Uuid, i32> = HashMap::new();
        for e in &added {
            *per.entry(*e).or_default() += 1;
        }
        for (ep, added) in &per {
            if let Some(a) = add_pending(&mut tx, *ep, *added).await? {
                alerts.push(a);
            }
        }
        let woken: Vec<Uuid> = per.into_keys().collect();
        super::deliver::wake(&mut tx, &woken).await?;
    }
    tx.commit().await?;
    Ok((n, alerts))
}

/// Count new undelivered rows on an endpoint and drop the oldest beyond the limit. Returns an alert
/// when the endpoint just started dropping.
async fn add_pending(tx: &mut PgConnection, ep: Uuid, added: i32) -> sqlx::Result<Option<BacklogAlert>> {
    let pending: i32 = sqlx::query_scalar("UPDATE webhook_endpoints SET pending = pending + $2 WHERE id = $1 RETURNING pending").bind(ep).bind(added).fetch_one(&mut *tx).await?;
    let over = i64::from(pending) - max_pending();
    if over <= 0 {
        return Ok(None);
    }
    sqlx::query(
        "WITH d AS (
            UPDATE webhook_deliveries SET status = 'dropped', error = 'backlog_full'
            WHERE id IN (SELECT id FROM webhook_deliveries WHERE endpoint_id = $1 AND status = 'pending' ORDER BY created_at, id LIMIT $2)
            RETURNING 1
         )
         UPDATE webhook_endpoints SET pending = pending - (SELECT count(*) FROM d), dropped_total = dropped_total + (SELECT count(*) FROM d), last_dropped_at = now()
         WHERE id = $1",
    )
    .bind(ep)
    .bind(over)
    .execute(&mut *tx)
    .await?;
    let first: Option<(Uuid, String, String)> =
        sqlx::query_as("UPDATE webhook_endpoints SET drop_alerted = true WHERE id = $1 AND NOT drop_alerted RETURNING account_id, url, description")
            .bind(ep)
            .fetch_optional(&mut *tx)
            .await?;
    Ok(first.map(|(account_id, url, description)| BacklogAlert { account_id, url, description }))
}

async fn backlog_alert(state: &AppState, a: &BacklogAlert) {
    let text = format!(
        "Your webhook endpoint {} has more than {} events waiting, so DumbMail is dropping the oldest ones it hasn't delivered yet.\n\nThis usually means the endpoint is down or slower than your events. You can see how many were dropped, and resend them for up to 14 days, from its log in Settings → Webhooks. We'll tell you again if it happens after it catches up.",
        super::display_name(&a.description, &a.url),
        max_pending()
    );
    super::alert_account(state, a.account_id, "A webhook endpoint is falling behind", &text).await;
}

// ---------- Filling in references ----------

/// What the batch's references point to, loaded with one query per kind.
#[derive(Default)]
struct Lookups {
    events: HashMap<Uuid, (Uuid, Value)>,
    messages: HashMap<Uuid, (Uuid, Value)>,
    profiles: HashMap<Uuid, Value>,
    lists: HashMap<Uuid, Value>,
    segments: HashMap<Uuid, Value>,
    forms: HashMap<Uuid, (Value, Option<Uuid>)>,
    campaigns: HashMap<Uuid, Value>,
}

fn ref_id(v: &Value, key: &str) -> Option<Uuid> {
    v.get(key).and_then(|x| x.as_str()).and_then(|s| s.parse().ok())
}

/// `{"$kind": id}` with nothing else in it.
fn as_ref(v: &Value) -> Option<(&str, Uuid)> {
    let o = v.as_object()?;
    if o.len() != 1 {
        return None;
    }
    let (k, id) = o.iter().next()?;
    let kind = k.strip_prefix('$')?;
    Some((kind, id.as_str()?.parse().ok()?))
}

impl Lookups {
    async fn load(tx: &mut PgConnection, account_id: Uuid, rows: &[Row]) -> sqlx::Result<Lookups> {
        let mut want: HashMap<&str, HashSet<Uuid>> = HashMap::new();
        for r in rows {
            if let Some(id) = ref_id(&r.data, "$event") {
                want.entry("event").or_default().insert(id);
            }
            if let Some(id) = ref_id(&r.data, "$message") {
                want.entry("message").or_default().insert(id);
            }
            if let Some(o) = r.data.as_object() {
                for v in o.values() {
                    if let Some((kind, id)) = as_ref(v) {
                        let kind = match kind {
                            "profile" => "profile",
                            "list" => "list",
                            "segment" => "segment",
                            "form" => "form",
                            "campaign" => "campaign",
                            _ => continue,
                        };
                        want.entry(kind).or_default().insert(id);
                    }
                }
            }
        }
        let ids = |k: &str| want.get(k).map(|s| s.iter().copied().collect::<Vec<_>>()).unwrap_or_default();
        let mut l = Lookups::default();
        let mut people: HashSet<Uuid> = want.get("profile").cloned().unwrap_or_default();

        let ev = ids("event");
        if !ev.is_empty() {
            let rows: Vec<(Uuid, Uuid, String, Value, Option<f64>, DateTime<Utc>, Option<String>, Option<Uuid>)> = sqlx::query_as(
                "SELECT e.id, e.profile_id, m.name, e.properties, e.value, e.occurred_at, e.unique_id, e.message_id
                 FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.account_id = $1 AND e.id = ANY($2)",
            )
            .bind(account_id)
            .bind(&ev)
            .fetch_all(&mut *tx)
            .await?;
            for (id, pid, metric, props, value, at, unique_id, message_id) in rows {
                people.insert(pid);
                l.events.insert(id, (pid, json!({"id": id, "metric": metric, "properties": props, "value": value, "occurred_at": at, "unique_id": unique_id, "message_id": message_id})));
            }
        }
        let ms = ids("message");
        if !ms.is_empty() {
            // Transactional sends are flagged, with their own status (which can be `unknown`: it may
            // have gone out). Their variables and bodies aren't stored, so they're never sent.
            #[allow(clippy::type_complexity)]
            let rows: Vec<(Uuid, Uuid, String, Option<String>, String, Option<Uuid>, Option<Uuid>, Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<DateTime<Utc>>, bool, Option<Value>)> =
                sqlx::query_as(
                    "SELECT m.id, m.profile_id, m.channel, m.subject, m.status, m.campaign_id, m.flow_id, m.flow_node_id, m.sent_at, m.delivered_at, m.send_after, m.transactional,
                            (SELECT jsonb_build_object('id', t.id, 'status', t.status) FROM transactional_sends t WHERE t.message_id = m.id AND t.account_id = $1 LIMIT 1)
                     FROM messages m WHERE m.account_id = $1 AND m.id = ANY($2) AND NOT m.is_test",
                )
                .bind(account_id)
                .bind(&ms)
                .fetch_all(&mut *tx)
                .await?;
            for r in rows {
                people.insert(r.1);
                l.messages.insert(
                    r.0,
                    (r.1, json!({"id": r.0, "channel": r.2, "subject": r.3, "status": r.4, "campaign_id": r.5, "flow_id": r.6, "flow_node_id": r.7, "sent_at": r.8,
                        "delivered_at": r.9, "send_after": r.10, "transactional": r.11, "transactional_send": r.12})),
                );
            }
        }
        if !people.is_empty() {
            let ids: Vec<Uuid> = people.into_iter().collect();
            let rows: Vec<(Uuid, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT id, email, phone_number, external_id, first_name, last_name FROM profiles WHERE account_id = $1 AND id = ANY($2)",
            )
            .bind(account_id)
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
            for r in rows {
                l.profiles.insert(r.0, json!({"id": r.0, "email": r.1, "phone_number": r.2, "external_id": r.3, "first_name": r.4, "last_name": r.5}));
            }
        }
        for (kind, sql) in [
            ("list", "SELECT id, name FROM lists WHERE account_id = $1 AND id = ANY($2)"),
            ("segment", "SELECT id, name FROM segments WHERE account_id = $1 AND id = ANY($2)"),
        ] {
            let want = ids(kind);
            if want.is_empty() {
                continue;
            }
            let rows: Vec<(Uuid, String)> = sqlx::query_as(sql).bind(account_id).bind(&want).fetch_all(&mut *tx).await?;
            let map = if kind == "list" { &mut l.lists } else { &mut l.segments };
            for (id, name) in rows {
                map.insert(id, json!({"id": id, "name": name}));
            }
        }
        let fs = ids("form");
        if !fs.is_empty() {
            let rows: Vec<(Uuid, String, String, Option<Uuid>)> =
                sqlx::query_as("SELECT id, name, kind, list_id FROM forms WHERE account_id = $1 AND id = ANY($2)").bind(account_id).bind(&fs).fetch_all(&mut *tx).await?;
            for (id, name, kind, list_id) in rows {
                l.forms.insert(id, (json!({"id": id, "name": name, "kind": kind}), list_id));
            }
        }
        let cs = ids("campaign");
        if !cs.is_empty() {
            let rows: Vec<(Uuid, String, String, String, Option<DateTime<Utc>>, i64)> = sqlx::query_as(
                "SELECT c.id, c.name, c.channel, c.status, c.sent_at, (SELECT count(*) FROM messages m WHERE m.campaign_id = c.id AND NOT m.is_test)
                 FROM campaigns c WHERE c.account_id = $1 AND c.id = ANY($2)",
            )
            .bind(account_id)
            .bind(&cs)
            .fetch_all(&mut *tx)
            .await?;
            for (id, name, channel, status, sent_at, recipients) in rows {
                l.campaigns.insert(id, json!({"id": id, "name": name, "channel": channel, "status": status, "sent_at": sent_at, "recipients": recipients}));
            }
        }
        Ok(l)
    }

    fn profile(&self, id: Uuid) -> Value {
        self.profiles.get(&id).cloned().unwrap_or_else(|| json!({"id": id}))
    }

    /// The data with its references filled in, or None when what it describes is gone (an event
    /// or a message deleted with its profile).
    fn fill(&self, data: &Value) -> Option<Value> {
        let mut d = data.clone();
        let o = d.as_object_mut()?;
        if let Some(id) = ref_id(data, "$event") {
            let (pid, ev) = self.events.get(&id)?;
            o.remove("$event");
            o.insert("event".into(), ev.clone());
            o.insert("profile".into(), self.profile(*pid));
        }
        if let Some(id) = ref_id(data, "$message") {
            let (pid, m) = self.messages.get(&id)?;
            o.remove("$message");
            o.insert("message".into(), m.clone());
            o.entry("profile").or_insert_with(|| self.profile(*pid));
        }
        let mut extra: Vec<(String, Value)> = Vec::new();
        for v in o.values_mut() {
            let Some((kind, id)) = as_ref(v) else { continue };
            *v = match kind {
                "profile" => self.profile(id),
                "list" => self.lists.get(&id).cloned().unwrap_or_else(|| json!({"id": id, "name": null})),
                "segment" => self.segments.get(&id).cloned().unwrap_or_else(|| json!({"id": id, "name": null})),
                "form" => {
                    let (f, list_id) = self.forms.get(&id)?;
                    extra.push(("list_id".into(), json!(list_id)));
                    f.clone()
                }
                "campaign" => self.campaigns.get(&id)?.clone(),
                _ => continue,
            };
        }
        for (k, v) in extra {
            o.entry(k).or_insert(v);
        }
        Some(d)
    }
}

// ---------- Upkeep ----------

/// Accounts the sweep looks at per run.
const SWEEP_ACCOUNTS: i64 = 10_000;
/// Rows trimmed from one account's outbox per statement, and statements per sweep.
const TRIM_BATCH: i64 = 5_000;
const TRIM_BATCHES: usize = 20;

/// Every minute (a background job): for each account with rows in its outbox, count them (servers
/// read the count to stop adding rows once it's full), trim it back under the limit in bounded
/// batches, and queue fan-out where rows have waited without a job (after a crash, or a job that
/// gave up). Then tell owners whose endpoints started dropping events.
pub(super) async fn sweep(state: &AppState) -> anyhow::Result<()> {
    let db = &state.db;
    // The accounts with outbox rows, read off the (account_id, id) index one account at a time.
    let accounts: Vec<Uuid> = sqlx::query_scalar(
        "WITH RECURSIVE a AS (
            (SELECT account_id FROM webhook_outbox ORDER BY account_id LIMIT 1)
            UNION ALL
            SELECT (SELECT o.account_id FROM webhook_outbox o WHERE o.account_id > a.account_id ORDER BY o.account_id LIMIT 1) FROM a WHERE a.account_id IS NOT NULL
         )
         SELECT account_id FROM a WHERE account_id IS NOT NULL LIMIT $1",
    )
    .bind(SWEEP_ACCOUNTS)
    .fetch_all(db)
    .await?;
    let max = max_outbox();
    for &account_id in &accounts {
        let (n, oldest): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT count(*), min(created_at) FROM (SELECT created_at FROM webhook_outbox WHERE account_id = $1 ORDER BY id LIMIT $2) c",
        )
        .bind(account_id)
        // Counted as far as one sweep can trim, so the count stays cheap.
        .bind(max + TRIM_BATCH * TRIM_BATCHES as i64)
        .fetch_one(db)
        .await?;
        let mut left = n;
        if n > max {
            let mut dropped = 0i64;
            for _ in 0..TRIM_BATCHES {
                let want = (left - max).min(TRIM_BATCH);
                if want <= 0 {
                    break;
                }
                let d = sqlx::query("DELETE FROM webhook_outbox WHERE id IN (SELECT id FROM webhook_outbox WHERE account_id = $1 ORDER BY id LIMIT $2)")
                    .bind(account_id)
                    .bind(want)
                    .execute(db)
                    .await?
                    .rows_affected() as i64;
                dropped += d;
                left -= d;
                if d < want {
                    break;
                }
            }
            sqlx::query("UPDATE webhook_endpoints SET dropped_total = dropped_total + $2, last_dropped_at = now() WHERE account_id = $1 AND enabled")
                .bind(account_id)
                .bind(dropped)
                .execute(db)
                .await?;
        }
        sqlx::query(
            "INSERT INTO webhook_outbox_counts (account_id, rows, counted_at) VALUES ($1, $2, now())
             ON CONFLICT (account_id) DO UPDATE SET rows = EXCLUDED.rows, counted_at = EXCLUDED.counted_at",
        )
        .bind(account_id)
        .bind(left)
        .execute(db)
        .await?;
        if oldest.is_some_and(|t| t < Utc::now() - chrono::Duration::minutes(1)) {
            rearm(db, account_id).await?;
        }
    }
    sqlx::query("DELETE FROM webhook_outbox_counts WHERE account_id <> $2 AND NOT (account_id = ANY($1))").bind(&accounts).bind(crate::features::observe::SWEPT).execute(db).await?;
    // When this count was taken: servers forget the rows they added before it.
    sqlx::query(
        "INSERT INTO webhook_outbox_counts (account_id, rows, counted_at) VALUES ($1, 0, now())
         ON CONFLICT (account_id) DO UPDATE SET counted_at = EXCLUDED.counted_at",
    )
    .bind(crate::features::observe::SWEPT)
    .execute(db)
    .await?;
    // Endpoints that started dropping (a full outbox, counted above or by a server that refused
    // rows) and whose owner hasn't been told yet.
    let first: Vec<(Uuid, String, String)> = sqlx::query_as(
        "UPDATE webhook_endpoints SET drop_alerted = true
         WHERE id IN (SELECT id FROM webhook_endpoints WHERE NOT drop_alerted AND last_dropped_at > now() - interval '1 day' LIMIT 100)
         RETURNING account_id, url, description",
    )
    .fetch_all(db)
    .await?;
    for (account_id, url, description) in first {
        backlog_alert(state, &BacklogAlert { account_id, url, description }).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_references() {
        let (p, l) = (Uuid::now_v7(), Uuid::now_v7());
        let mut look = Lookups::default();
        look.profiles.insert(p, json!({"id": p, "email": "a@x.test"}));
        let got = look.fill(&json!({"list": {"$list": l}, "profile": {"$profile": p}, "note": {"a": 1}})).unwrap();
        assert_eq!(got["profile"]["email"], "a@x.test");
        assert_eq!(got["list"], json!({"id": l, "name": null}));
        assert_eq!(got["note"], json!({"a": 1}));
        // An event that's gone (deleted with its profile) isn't sent.
        assert!(look.fill(&json!({"$event": Uuid::now_v7()})).is_none());
    }
}
