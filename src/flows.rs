//! Flow automation: validation, triggers, the step engine and starter blueprints.

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Duration, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::{FlowRow, MessageStats, Profile, PROFILE_COLS, STATS_SELECT};
use crate::segments::{self, Filter};
use crate::sending::{self, Outcome, SendSpec};
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    Metric {
        metric: String,
        #[serde(default)]
        filters: Vec<Filter>,
        /// With `cancelled_by`: the event property that identifies the thing (e.g. `reservation_id`).
        /// People leave the flow when a cancelling event with the same value comes later, or a newer
        /// event of this metric replaces this one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cancelled_by: Vec<String>,
    },
    List {
        list_id: Uuid,
    },
    Segment {
        segment_id: Uuid,
    },
    DateProperty {
        field: String,
        #[serde(default)]
        offset_days: i64,
    },
    /// N days before or after a date inside each event, e.g. 2 days before `check_in` on
    /// "Reservation Confirmed". Each event starts the flow at most once.
    EventDate {
        metric: String,
        /// The event property holding the date (dots reach nested keys).
        field: String,
        /// Days after the date (negative: before).
        #[serde(default)]
        offset_days: i64,
        /// Hour of the day to start, in the account's time zone. Default 9.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hour: Option<u32>,
        #[serde(default)]
        filters: Vec<Filter>,
        /// The event property that identifies the thing (e.g. `reservation_id`). A newer event of the
        /// same metric with the same value replaces the older one, and `cancelled_by` matches on it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        /// Metrics that cancel it when they happen later with the same `key` value.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cancelled_by: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub data: serde_json::Map<String, Value>,
}

impl Node {
    fn str(&self, k: &str) -> Option<&str> {
        self.data.get(k).and_then(|v| v.as_str())
    }
    pub fn next(&self) -> Option<&str> {
        self.str("next")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub reentry: Option<Value>,
    #[serde(default)]
    pub exit_condition: Option<Value>,
    #[serde(default)]
    pub profile_filter: Option<Value>,
    #[serde(default)]
    pub smart_sending_hours: Option<i64>,
    /// Also send to people who haven't agreed to marketing (`marketing_hold`). Only for messages
    /// about the person's own booking or order. Unsubscribed and suppressed people still get nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_messages: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlowDef {
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub settings: Settings,
}

impl FlowDef {
    fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

pub fn parse_trigger(v: &Value) -> AppResult<Trigger> {
    let t: Trigger = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid trigger: {e}")))?;
    let name_ok = |s: &str| !s.trim().is_empty() && s.len() <= 128;
    if let Trigger::EventDate { metric, field, offset_days, hour, .. } = &t {
        if !name_ok(metric) || !name_ok(field) {
            return Err(AppError::bad("an event date trigger needs a metric and the event property holding the date"));
        }
        if !(-365..=365).contains(offset_days) {
            return Err(AppError::bad("offset_days must be -365 to 365"));
        }
        if hour.is_some_and(|h| h > 23) {
            return Err(AppError::bad("hour must be 0-23"));
        }
    }
    // Event-date triggers are new, so their filters are capped; metric triggers keep no cap, as before.
    if let Trigger::EventDate { filters, .. } = &t {
        if filters.len() > 20 {
            return Err(AppError::bad("an event date trigger takes at most 20 filters"));
        }
    }
    if let Trigger::EventDate { key, cancelled_by, .. } | Trigger::Metric { key, cancelled_by, .. } = &t {
        if cancelled_by.len() > 5 || !cancelled_by.iter().all(|m| name_ok(m)) {
            return Err(AppError::bad("at most 5 cancelling metrics, each 1-128 characters"));
        }
        if key.as_deref().is_some_and(|k| !name_ok(k)) {
            return Err(AppError::bad("key must be an event property name"));
        }
        if !cancelled_by.is_empty() && key.is_none() {
            return Err(AppError::bad("cancelled_by needs a key (the property both events share, e.g. reservation_id)"));
        }
    }
    Ok(t)
}

fn json_path(field: &str) -> Vec<String> {
    field.split('.').map(str::to_string).collect()
}

/// Whether an event-date trigger's event no longer stands: a newer event of the same metric with
/// the same key replaced it, or a cancelling event with the same key came after it.
async fn event_date_stale(db: &PgPool, account_id: Uuid, event_id: Uuid, key: Option<&str>, cancelled_by: &[String]) -> sqlx::Result<Option<&'static str>> {
    let row: Option<(Uuid, Option<String>, chrono::DateTime<Utc>)> = sqlx::query_as("SELECT metric_id, properties #>> $3, occurred_at FROM events WHERE id = $1 AND account_id = $2")
        .bind(event_id)
        .bind(account_id)
        .bind(json_path(key.unwrap_or("")))
        .fetch_optional(db)
        .await?;
    let Some((metric_id, value, at)) = row else { return Ok(Some("event deleted")) };
    let (Some(key), Some(value)) = (key, value) else { return Ok(None) };
    let superseded: bool = sqlx::query_scalar(
        // A browser event of a server-only metric (recorded before it became one) never counts.
        "SELECT EXISTS (SELECT 1 FROM events n JOIN metrics nm ON nm.id = n.metric_id WHERE n.account_id = $1 AND n.metric_id = $2 AND n.properties #>> $3 = $4 AND (n.occurred_at, n.id) > ($5, $6)
           AND NOT (n.from_browser IS TRUE AND nm.server_only))",
    )
    .bind(account_id)
    .bind(metric_id)
    .bind(json_path(key))
    .bind(&value)
    .bind(at)
    .bind(event_id)
    .fetch_one(db)
    .await?;
    if superseded {
        return Ok(Some("superseded"));
    }
    if cancelled_by.is_empty() {
        return Ok(None);
    }
    let cancelled: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM events c JOIN metrics m ON m.id = c.metric_id
          WHERE c.account_id = $1 AND m.account_id = $1 AND m.name = ANY($2) AND c.properties #>> $3 = $4 AND c.occurred_at >= $5
            AND NOT (c.from_browser IS TRUE AND m.server_only))",
    )
    .bind(account_id)
    .bind(cancelled_by)
    .bind(json_path(key))
    .bind(&value)
    .bind(at)
    .fetch_one(db)
    .await?;
    Ok(cancelled.then_some("cancelled"))
}

/// A random split's share of people sent down path A (`yes`); the rest take path B (`no`).
fn split_percent(n: &Node) -> Option<u32> {
    n.data.get("percent").and_then(|v| v.as_u64()).filter(|p| (1..=99).contains(p)).map(|p| p as u32)
}

/// Validate a flow graph: known node types, required fields, edges point at real nodes, no cycles.
pub fn parse_definition(v: &Value) -> AppResult<FlowDef> {
    let def: FlowDef = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid flow definition: {e}")))?;
    if def.nodes.len() > 200 {
        return Err(AppError::bad("a flow can have at most 200 steps"));
    }
    let ids: HashSet<&str> = def.nodes.iter().map(|n| n.id.as_str()).collect();
    if ids.len() != def.nodes.len() {
        return Err(AppError::bad("flow node ids must be unique"));
    }
    if let Some(e) = &def.entry {
        if !ids.contains(e.as_str()) {
            return Err(AppError::bad(format!("entry {e:?} is not a node")));
        }
    } else if !def.nodes.is_empty() {
        return Err(AppError::bad("set `entry` to the first node's id"));
    }
    for n in &def.nodes {
        let edges: Vec<&str> = match n.kind.as_str() {
            "condition" | "random_split" | "wait_for_reply" => vec!["yes", "no"],
            "delay" | "email" | "sms" | "update_profile" | "add_to_list" | "remove_from_list" | "webhook" | "notify" => vec!["next"],
            other => return Err(AppError::bad(format!("unknown step type {other:?} (node {})", n.id))),
        };
        for e in edges {
            if let Some(t) = n.str(e) {
                if !ids.contains(t) {
                    return Err(AppError::bad(format!("node {} points {e} at missing node {t:?}", n.id)));
                }
            }
        }
        let need = |k: &str| -> AppResult<()> {
            if n.data.get(k).is_none_or(|v| v.is_null()) {
                Err(AppError::bad(format!("{} step {} needs `{k}`", n.kind, n.id)))
            } else {
                Ok(())
            }
        };
        match n.kind.as_str() {
            "delay" => {
                let amount = n.data.get("amount").and_then(|v| v.as_f64()).unwrap_or(-1.0);
                if amount < 0.0 {
                    return Err(AppError::bad(format!("delay {} needs a non-negative amount", n.id)));
                }
                if !matches!(n.str("unit"), Some("minutes" | "hours" | "days")) {
                    return Err(AppError::bad(format!("delay {} unit must be minutes, hours or days", n.id)));
                }
            }
            "email" | "sms" => need("content")?,
            "condition" => {
                need("condition")?;
                segments::parse(&n.data["condition"])?;
            }
            "random_split" => {
                if split_percent(n).is_none() {
                    return Err(AppError::bad(format!("random split {} needs `percent` (whole number 1-99, the share sent down path A)", n.id)));
                }
            }
            "update_profile" => need("properties")?,
            "wait_for_reply" => {
                let amount = n.data.get("amount").and_then(|v| v.as_f64()).unwrap_or(-1.0);
                let unit = match n.str("unit") {
                    Some("minutes") => 60.0,
                    Some("hours") => 3600.0,
                    Some("days") => 86400.0,
                    _ => 0.0,
                };
                if amount <= 0.0 || unit == 0.0 || amount * unit > 30.0 * 86400.0 {
                    return Err(AppError::bad(format!("wait for reply {} needs a positive amount and a unit (minutes, hours or days), up to 30 days", n.id)));
                }
            }
            "add_to_list" | "remove_from_list" => need("list_id")?,
            "webhook" => {
                let u = n.str("url").unwrap_or("");
                if !(u.starts_with("https://") || u.starts_with("http://")) {
                    return Err(AppError::bad(format!("webhook {} needs an http(s) url", n.id)));
                }
            }
            _ => {}
        }
    }
    // Cycle check (DFS from every node).
    let edges: HashMap<&str, Vec<&str>> = def
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), ["next", "yes", "no"].iter().filter_map(|k| n.str(k)).collect()))
        .collect();
    fn visit<'a>(n: &'a str, edges: &HashMap<&'a str, Vec<&'a str>>, stack: &mut HashSet<&'a str>, done: &mut HashSet<&'a str>) -> bool {
        if stack.contains(n) {
            return false;
        }
        if done.contains(n) {
            return true;
        }
        stack.insert(n);
        for m in edges.get(n).into_iter().flatten() {
            if !visit(m, edges, stack, done) {
                return false;
            }
        }
        stack.remove(n);
        done.insert(n);
        true
    }
    let mut done = HashSet::new();
    for n in &def.nodes {
        if !visit(&n.id, &edges, &mut HashSet::new(), &mut done) {
            return Err(AppError::bad("flow steps form a loop; every path must end"));
        }
    }
    for key in [&def.settings.exit_condition, &def.settings.profile_filter].into_iter().flatten() {
        segments::parse(key)?;
    }
    // Delay options, trigger splits and notification steps (see features::flows_plus).
    crate::features::flows_plus::validate(&def, v)?;
    Ok(def)
}

fn ip_is_public(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_broadcast() || v4.is_unspecified()
                || v4.is_documentation() || o[0] == 0 || (o[0] == 100 && (o[1] & 0xc0) == 64) || (o[0] == 198 && (o[1] & 0xfe) == 18) || o[0] >= 224)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_public(IpAddr::V4(v4));
            }
            let (seg, o) = (v6.segments(), v6.octets());
            // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) addresses carry an IPv4 address: judge that one.
            if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return ip_is_public(IpAddr::V4(std::net::Ipv4Addr::new(o[12], o[13], o[14], o[15])));
            }
            if seg[0] == 0x2002 {
                return ip_is_public(IpAddr::V4(std::net::Ipv4Addr::new(o[2], o[3], o[4], o[5])));
            }
            // Otherwise only global unicast (2000::/3), minus Teredo tunnels and the documentation range.
            // That leaves out loopback, IPv4-compatible, unique-local, link-local, site-local and multicast.
            (seg[0] & 0xe000) == 0x2000 && !(seg[0] == 0x2001 && (seg[1] == 0 || seg[1] == 0x0db8))
        }
    }
}

/// Webhook steps may only call public internet hosts (no localhost, private networks or cloud metadata).
/// Returns the host and a vetted address to pin the connection to, so DNS can't be swapped afterwards.
pub async fn check_webhook_url(raw: &str) -> AppResult<(String, std::net::SocketAddr)> {
    let u = url::Url::parse(raw).map_err(|_| AppError::bad("webhook url is not a valid URL"))?;
    if !matches!(u.scheme(), "https" | "http") {
        return Err(AppError::bad("webhook url must be http(s)"));
    }
    let host = u.host_str().ok_or_else(|| AppError::bad("webhook url needs a host"))?.trim_matches(['[', ']']).to_string();
    let port = u.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| AppError::bad(format!("webhook host {host:?} doesn't resolve")))?
        .collect();
    // Development and tests may allow localhost (WEBHOOKS_ALLOW_LOCALHOST); never private networks.
    let local_ok = crate::features::webhooks::net::localhost_allowed() && !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback());
    if !local_ok && (addrs.is_empty() || addrs.iter().any(|a| !ip_is_public(a.ip()))) {
        return Err(AppError::bad("webhook url must point to a public internet address"));
    }
    Ok((host, addrs[0]))
}

/// Fetch a public web page (for checking a customer's site for the snippet): public IPs only,
/// pinned DNS, a few redirects re-checked hop by hop, and at most 3 MB read.
pub async fn fetch_public_page(raw: &str) -> AppResult<String> {
    let page = fetch_public(raw, 3 * 1024 * 1024).await?;
    Ok(String::from_utf8_lossy(&page.body).into_owned())
}

/// A fetched public resource: where it ended up after redirects, its content type and (at most `max`) bytes.
pub struct Fetched {
    pub url: String,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Fetch any public http(s) resource with the same protections as [`fetch_public_page`].
pub async fn fetch_public(raw: &str, max: usize) -> AppResult<Fetched> {
    let mut url = raw.to_string();
    for _ in 0..5 {
        let (host, addr) = tokio::time::timeout(std::time::Duration::from_secs(5), check_webhook_url(&url))
            .await
            .map_err(|_| AppError::bad("that address didn't resolve in time"))?
            .map_err(|_| AppError::bad("enter a public website address"))?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve(&host, addr)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Mozilla/5.0 (compatible; DumbMail/0.3; +https://dumbmail.ai)")
            .no_proxy()
            .build()
            .map_err(|e| AppError::Internal(e.into()))?;
        let res = client
            .get(&url)
            .header("accept", "text/html,application/xhtml+xml,text/css,image/*;q=0.9,*/*;q=0.8")
            .send()
            .await
            .map_err(|e| AppError::bad(format!("couldn't load that page: {}", e.without_url())))?;
        if res.status().is_redirection() {
            let loc = res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default();
            url = url::Url::parse(&url).and_then(|b| b.join(loc)).map(|u| u.to_string()).map_err(|_| AppError::bad("the page redirected somewhere invalid"))?;
            continue;
        }
        if !res.status().is_success() {
            return Err(AppError::bad(format!("that page returned HTTP {}", res.status().as_u16())));
        }
        let content_type = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
        let mut body = Vec::new();
        let mut stream = res;
        while let Some(chunk) = stream.chunk().await.map_err(|e| AppError::bad(format!("couldn't read that page: {}", e.without_url())))? {
            body.extend_from_slice(&chunk);
            if body.len() > max {
                body.truncate(max);
                break;
            }
        }
        return Ok(Fetched { url, content_type, body });
    }
    Err(AppError::bad("that page redirects too many times"))
}

/// Signed with the account's flow webhook secret (see docs/WEBHOOKS.md); a GET signs an empty body.
/// One 10-second deadline covers resolving and checking the address, reading the signing secret
/// (only once the address passed) and the request; no proxy is used, so the connection goes to the
/// address that was checked.
async fn call_webhook(state: &AppState, account_id: Uuid, url: &str, get: bool, payload: &Value) -> anyhow::Result<()> {
    let body = if get { Vec::new() } else { serde_json::to_vec(payload)? };
    let send = async {
        let (host, addr) = check_webhook_url(url).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        let headers = crate::features::webhooks::flow_signature_headers(state, account_id, &body).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve(&host, addr)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("dumbmail-webhook/0.1")
            .build()?;
        let mut req = if get { client.get(url) } else { client.post(url).header("content-type", "application/json").body(body.clone()) };
        for (k, v) in headers {
            req = req.header(k, v);
        }
        req.send().await?.error_for_status()?;
        anyhow::Ok(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), send).await.map_err(|_| anyhow::anyhow!("no response within 10 seconds"))?
}

async fn profile(db: &PgPool, id: Uuid) -> AppResult<Option<Profile>> {
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1");
    Ok(sqlx::query_as::<_, Profile>(&q).bind(id).fetch_optional(db).await?)
}

/// Start a run unless re-entry rules or the profile filter say no. Returns the run id.
pub async fn start(state: &AppState, flow: &FlowRow, profile_id: Uuid, context: Value) -> AppResult<Option<Uuid>> {
    let def = parse_definition(&flow.definition)?;
    let Some(entry) = def.entry.clone() else { return Ok(None) };
    // The cheap rejection first, with no lock or transaction: most repeat triggers stop here.
    // Event-date flows start once per event (a guest with two bookings gets both), so re-entry
    // doesn't apply to them.
    let (blocked, days) = reentry_rule(flow, &def, 3);
    let sql = format!("SELECT {blocked}");
    let mut q = sqlx::query_scalar::<_, bool>(&sql).bind(flow.id).bind(profile_id);
    if let Some(d) = days {
        q = q.bind(d);
    }
    if q.fetch_one(&state.db).await? {
        return Ok(None);
    }
    if let Some(f) = &def.settings.profile_filter {
        if !segments::profile_matches(&state.db, profile_id, &segments::load(&state.db, flow.account_id, f, None).await?).await? {
            return Ok(None);
        }
    }
    let mut tx = state.db.begin().await?;
    let id = enter(&mut tx, flow, &def, profile_id, &entry, &context, None).await?;
    tx.commit().await?;
    Ok(id)
}

/// The re-entry rule as an SQL condition that is true when person $2 may not enter flow $1 now,
/// and the seconds to bind as `$window` when the rule has a window (then bind it last). Event-date
/// flows have none: they start once per event.
fn reentry_rule(flow: &FlowRow, def: &FlowDef, window: usize) -> (String, Option<f64>) {
    if matches!(parse_trigger(&flow.trigger), Ok(Trigger::EventDate { .. })) {
        return ("FALSE".into(), None);
    }
    let reentry = def.settings.reentry.clone().unwrap_or(json!("never"));
    match (&reentry, reentry.get("after_days").and_then(|d| d.as_f64())) {
        (Value::String(s), _) if s == "always" => ("EXISTS (SELECT 1 FROM flow_runs WHERE flow_id = $1 AND profile_id = $2 AND status = 'active')".into(), None),
        (_, Some(days)) => (
            format!("EXISTS (SELECT 1 FROM flow_runs WHERE flow_id = $1 AND profile_id = $2 AND (status = 'active' OR started_at > now() - make_interval(secs => ${window})))"),
            Some(days * 86400.0),
        ),
        _ => ("EXISTS (SELECT 1 FROM flow_runs WHERE flow_id = $1 AND profile_id = $2)".into(), None),
    }
}

/// Start a run at step `node` inside `tx` unless re-entry rules say no, with its first job at
/// `run_at` (now when None). Under a lock on this flow and person, the rule is checked again and the
/// run and its job are written in one statement, so two starts can't both pass it.
pub async fn enter(
    tx: &mut sqlx::PgConnection,
    flow: &FlowRow,
    def: &FlowDef,
    profile_id: Uuid,
    node: &str,
    context: &Value,
    run_at: Option<chrono::DateTime<Utc>>,
) -> AppResult<Option<Uuid>> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || $2::text, 7))").bind(flow.id).bind(profile_id).execute(&mut *tx).await?;
    let (blocked, days) = reentry_rule(flow, def, 9);
    let sql = format!(
        "WITH r AS (
            INSERT INTO flow_runs (id, flow_id, account_id, profile_id, current_node, context)
            SELECT $3, $1, $4, $2, $5, $6 WHERE NOT {blocked}
            RETURNING id)
         INSERT INTO jobs (kind, payload, run_at, lane) SELECT 'flow_step', jsonb_build_object('run_id', r.id), COALESCE($7, now()), $8 FROM r
         RETURNING (payload->>'run_id')::uuid"
    );
    let mut q = sqlx::query_scalar::<_, Uuid>(&sql)
        .bind(flow.id)
        .bind(profile_id)
        .bind(Uuid::now_v7())
        .bind(flow.account_id)
        .bind(node)
        .bind(context)
        .bind(run_at)
        .bind(crate::jobs::MAIN);
    if let Some(d) = days {
        q = q.bind(d);
    }
    Ok(q.fetch_optional(&mut *tx).await?)
}

/// Job: an event happened; start matching metric-triggered flows.
pub async fn on_event(state: &AppState, event_id: Uuid) -> anyhow::Result<()> {
    let row: Option<(Uuid, Uuid, String, Value, Option<f64>, chrono::DateTime<Utc>, Option<bool>)> = sqlx::query_as(
        "SELECT e.account_id, e.profile_id, m.name, e.properties, e.value, e.occurred_at, e.from_browser FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.id = $1",
    )
    .bind(event_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, profile_id, metric, props, value, at, from_browser)) = row else { return Ok(()) };
    // Message, consent and alert metrics start flows only when DumbMail recorded them itself.
    // The same for an account's server-only metrics (an integration's bookings and orders).
    if from_browser == Some(true) && (crate::audience::is_reserved_metric(&metric) || crate::audience::is_account_server_only_metric(&state.db, account_id, &metric).await.map_err(|e| anyhow::anyhow!(e.public_message()))?) {
        return Ok(());
    }
    schedule_event_dates(&state.db, event_id).await?;
    let flows: Vec<FlowRow> = sqlx::query_as(
        "SELECT * FROM flows WHERE account_id = $1 AND status = 'live' AND trigger->>'type' = 'metric' AND trigger->>'metric' = $2",
    )
    .bind(account_id)
    .bind(&metric)
    .fetch_all(&state.db)
    .await?;
    let mut obj = props.clone();
    if let (Some(v), Some(o)) = (value, obj.as_object_mut()) {
        o.entry("value").or_insert(json!(v));
    }
    for f in flows {
        if let Ok(Trigger::Metric { filters, .. }) = parse_trigger(&f.trigger) {
            if filters.iter().all(|flt| segments::json_filter_matches(&obj, flt)) {
                // `from_browser`: recorded by the website script (anyone can send those); unknown counts as true.
                let ctx = json!({"event": {"id": event_id, "metric": metric, "properties": props, "value": value, "occurred_at": at, "from_browser": from_browser != Some(false)}});
                start(state, &f, profile_id, ctx).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
            }
        }
    }
    Ok(())
}

pub async fn on_list_add(state: &AppState, list_id: Uuid, profile_ids: &[Uuid]) -> anyhow::Result<()> {
    let flows: Vec<FlowRow> = sqlx::query_as(
        "SELECT f.* FROM flows f JOIN lists l ON l.id = $1 AND l.account_id = f.account_id
         WHERE f.status = 'live' AND f.trigger->>'type' = 'list' AND f.trigger->>'list_id' = $1::text",
    )
        .bind(list_id)
        .fetch_all(&state.db)
        .await?;
    for f in &flows {
        for pid in profile_ids {
            start(state, f, *pid, json!({"list_id": list_id})).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        }
    }
    Ok(())
}

/// Job: refresh a segment's membership and start segment-triggered flows for new members.
pub async fn refresh_segment(state: &AppState, segment_id: Uuid) -> anyhow::Result<()> {
    let row: Option<(Uuid, Value, Option<chrono::DateTime<Utc>>, bool)> = sqlx::query_as("SELECT account_id, definition, refreshed_at, absorb_entries FROM segments WHERE id = $1")
        .bind(segment_id)
        .fetch_optional(&state.db)
        .await?;
    let Some((account_id, def, refreshed_at, absorb)) = row else { return Ok(()) };
    // Rules that can't load (e.g. they include a segment that was since deleted) won't fix themselves
    // on a retry: log it and try again on the next scheduled refresh.
    let def = match segments::load(&state.db, account_id, &def, Some(segment_id)).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(segment = %segment_id, error = %e.public_message(), "segment rules couldn't load; not refreshed");
            return Ok(());
        }
    };
    let entered = segments::refresh(&state.db, account_id, segment_id, &def).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    // The first refresh only records who is already in the segment; it does not trigger anyone. Neither
    // does one while past activity is being imported into the account (`absorb_entries`).
    if refreshed_at.is_none() || absorb {
        return Ok(());
    }
    let flows: Vec<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE account_id = $2 AND status = 'live' AND trigger->>'type' = 'segment' AND trigger->>'segment_id' = $1")
        .bind(segment_id.to_string())
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    for f in &flows {
        for pid in &entered {
            start(state, f, *pid, json!({"segment_id": segment_id})).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        }
    }
    Ok(())
}

/// Start date-property flows (birthdays, anniversaries) for profiles whose date is today, after 9am local.
pub async fn date_triggers(state: &AppState) -> anyhow::Result<()> {
    let flows: Vec<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE status = 'live' AND trigger->>'type' = 'date_property'")
        .fetch_all(&state.db)
        .await?;
    for f in flows {
        let Ok(Trigger::DateProperty { field, offset_days }) = parse_trigger(&f.trigger) else { continue };
        let account = crate::campaigns::load_account(&state.db, f.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        let local = Utc::now().with_timezone(&sending::account_tz(&account));
        if local.hour() < 9 {
            continue;
        }
        // The property date that fires today is today minus the offset.
        let target = local.date_naive() - Duration::days(offset_days);
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT p.id FROM profiles p WHERE p.account_id = $1
               AND extract(month FROM pl_ts(p.properties->>$2)) = $3 AND extract(day FROM pl_ts(p.properties->>$2)) = $4
               AND NOT EXISTS (SELECT 1 FROM flow_runs r WHERE r.flow_id = $5 AND r.profile_id = p.id AND r.started_at > now() - interval '300 days')
             LIMIT 10000",
        )
        .bind(f.account_id)
        .bind(&field)
        .bind(target.month() as f64)
        .bind(target.day() as f64)
        .bind(f.id)
        .fetch_all(&state.db)
        .await?;
        for pid in ids {
            start(state, &f, pid, json!({"date_field": field})).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        }
    }
    event_date_triggers(state).await
}

/// The moment an event is due for an event-date flow: the date in property `field` ($4: a plain
/// `YYYY-MM-DD` as written, a date-time read in the account's time zone $5), plus `offset_days`
/// ($6), at `hour` ($7) local time. `e` is the event.
const EVENT_DATE_DUE: &str = "CROSS JOIN LATERAL (SELECT CASE WHEN (e.properties #>> $4) ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'
                                     THEN pl_ts(e.properties #>> $4)::date
                                     ELSE (pl_ts(e.properties #>> $4) AT TIME ZONE $5)::date END AS day) x
     CROSS JOIN LATERAL (SELECT ((x.day + $6::int) + make_time($7, 0, 0)) AT TIME ZONE $5 AS due) d";

/// How each event is scheduled for an event-date flow (see [`EVENT_DATE_DUE`]). A fire first found
/// more than a day after it was due (old history, or an event recorded late) is recorded as missed,
/// so the flow's results show it, instead of sending a stale reminder or dropping it silently.
const EVENT_DATE_INSERT: &str = "INSERT INTO flow_event_date_fires (flow_id, event_id, account_id, due_at, outcome, fired_at)
     SELECT $1, e.id, e.account_id, d.due,
            CASE WHEN d.due > now() - interval '24 hours' THEN 'pending' ELSE 'missed' END,
            CASE WHEN d.due > now() - interval '24 hours' THEN NULL ELSE now() END
     FROM events e
     JOIN metrics m ON m.id = e.metric_id AND m.account_id = e.account_id AND m.name = $3
     {DUE}
     WHERE e.account_id = $2 AND x.day IS NOT NULL";

fn event_date_insert() -> String {
    EVENT_DATE_INSERT.replace("{DUE}", EVENT_DATE_DUE)
}

/// Events one scheduler tick may schedule for event-date flows that went live (or changed trigger)
/// with history, across all flows. The rest wait for the next tick.
const EVENT_DATE_SEED_BUDGET: i64 = 20_000;

/// Catch-up window: a fire due within this long ago still starts; older ones are marked missed.
const EVENT_DATE_CATCH_UP_HOURS: i64 = 24;

async fn event_date_tz(db: &PgPool, account_id: Uuid) -> anyhow::Result<String> {
    let tz: Option<String> = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_optional(db).await?;
    Ok(tz.and_then(|t| t.parse::<chrono_tz::Tz>().ok()).unwrap_or(chrono_tz::America::Los_Angeles).name().to_string())
}

/// Schedule one event for the account's event-date flows on its metric (any status: a flow
/// turned on later still has its upcoming dates). Called when the event is recorded.
pub async fn schedule_event_dates(db: &PgPool, event_id: Uuid) -> anyhow::Result<()> {
    let row: Option<(Uuid, String)> = sqlx::query_as("SELECT e.account_id, m.name FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.id = $1").bind(event_id).fetch_optional(db).await?;
    let Some((account_id, metric)) = row else { return Ok(()) };
    let flows: Vec<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE account_id = $1 AND trigger->>'type' = 'event_date' AND trigger->>'metric' = $2").bind(account_id).bind(&metric).fetch_all(db).await?;
    if flows.is_empty() {
        return Ok(());
    }
    let tz = event_date_tz(db, account_id).await?;
    for f in flows {
        let Ok(Trigger::EventDate { field, offset_days, hour, .. }) = parse_trigger(&f.trigger) else { continue };
        let q = format!("{} AND e.id = $8 ON CONFLICT DO NOTHING", event_date_insert());
        sqlx::query(&q)
            .bind(f.id)
            .bind(account_id)
            .bind(&metric)
            .bind(json_path(&field))
            .bind(&tz)
            .bind(offset_days as i32)
            .bind(hour.unwrap_or(9) as i32)
            .bind(event_id)
            .execute(db)
            .await?;
    }
    Ok(())
}

/// The account's time zone changed: due times computed in the old one move to the new one (a
/// date-only check-in is "2 days before, at 10:00" wherever the account now is).
pub async fn reschedule_event_dates(db: &PgPool, account_id: Uuid) -> anyhow::Result<()> {
    let flows: Vec<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE account_id = $1 AND trigger->>'type' = 'event_date'").bind(account_id).fetch_all(db).await?;
    if flows.is_empty() {
        return Ok(());
    }
    let tz = event_date_tz(db, account_id).await?;
    for f in flows {
        let Ok(Trigger::EventDate { field, offset_days, hour, .. }) = parse_trigger(&f.trigger) else { continue };
        let q = format!(
            "UPDATE flow_event_date_fires f SET due_at = d.due FROM events e {EVENT_DATE_DUE}
             WHERE f.flow_id = $1 AND f.account_id = $2 AND f.outcome = 'pending' AND e.id = f.event_id AND e.account_id = $2
               AND e.metric_id = (SELECT id FROM metrics WHERE account_id = $2 AND name = $3) AND x.day IS NOT NULL"
        );
        sqlx::query(&q)
            .bind(f.id)
            .bind(account_id)
            .bind(f.trigger["metric"].as_str().unwrap_or(""))
            .bind(json_path(&field))
            .bind(&tz)
            .bind(offset_days as i32)
            .bind(hour.unwrap_or(9) as i32)
            .execute(db)
            .await?;
    }
    Ok(())
}

/// Scheduler: start every fire that is due first (each claimed and started in one transaction;
/// fires more than a day late are marked missed instead of sending late), then, within one work
/// budget for the tick across all flows, schedule the events that existed before a live flow (or
/// its current trigger), a batch at a time.
async fn event_date_triggers(state: &AppState) -> anyhow::Result<()> {
    event_date_tick(state, EVENT_DATE_SEED_BUDGET).await
}

/// One event-date tick with an explicit seeding budget (events across all flows); tests use it.
pub async fn event_date_tick(state: &AppState, seed_budget: i64) -> anyhow::Result<()> {
    // Too late to be useful: say so in the flow report rather than send a stale reminder.
    sqlx::query(
        "UPDATE flow_event_date_fires x SET outcome = CASE WHEN fl.status = 'live' THEN 'missed' ELSE 'not_live' END, fired_at = now()
         FROM flows fl WHERE fl.id = x.flow_id AND x.outcome = 'pending' AND x.due_at <= now() - make_interval(hours => $1)",
    )
    .bind(EVENT_DATE_CATCH_UP_HOURS as i32)
    .execute(&state.db)
    .await?;
    for _ in 0..2000 {
        if !fire_one_event_date(state).await? {
            break;
        }
    }
    let flows: Vec<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE status = 'live' AND trigger->>'type' = 'event_date' ORDER BY created_at").fetch_all(&state.db).await?;
    let mut budget = seed_budget;
    for f in &flows {
        if budget <= 0 {
            break;
        }
        let Ok(Trigger::EventDate { metric, field, offset_days, hour, .. }) = parse_trigger(&f.trigger) else { continue };
        let seed: Option<(Value, Option<Uuid>, bool)> = sqlx::query_as("SELECT trigger, last_event_id, done FROM flow_event_date_seeds WHERE flow_id = $1").bind(f.id).fetch_optional(&state.db).await?;
        let (mut last, done) = match seed {
            Some((t, last, done)) if t == f.trigger => (last, done),
            other => {
                // A new flow, or a changed trigger: schedule its events from scratch (a changed
                // trigger first drops the fires computed for the old one).
                let mut tx = state.db.begin().await?;
                if other.is_some() {
                    sqlx::query("DELETE FROM flow_event_date_fires WHERE flow_id = $1 AND outcome = 'pending'").bind(f.id).execute(&mut *tx).await?;
                }
                sqlx::query("INSERT INTO flow_event_date_seeds (flow_id, trigger, last_event_id, done) VALUES ($1, $2, NULL, false) ON CONFLICT (flow_id) DO UPDATE SET trigger = $2, last_event_id = NULL, done = false")
                    .bind(f.id)
                    .bind(&f.trigger)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                (None, false)
            }
        };
        if done {
            continue;
        }
        let tz = event_date_tz(&state.db, f.account_id).await?;
        while budget > 0 {
            let size = budget.min(5000);
            let batch: Vec<Uuid> = sqlx::query_scalar(
                "SELECT e.id FROM events e JOIN metrics m ON m.id = e.metric_id AND m.account_id = e.account_id
                 WHERE e.account_id = $1 AND m.name = $2 AND ($3::uuid IS NULL OR e.id > $3) ORDER BY e.id LIMIT $4",
            )
            .bind(f.account_id)
            .bind(&metric)
            .bind(last)
            .bind(size)
            .fetch_all(&state.db)
            .await?;
            budget -= batch.len().max(1) as i64;
            if let Some(max) = batch.last() {
                let q = format!("{} AND e.id = ANY($8) ON CONFLICT DO NOTHING", event_date_insert());
                sqlx::query(&q)
                    .bind(f.id)
                    .bind(f.account_id)
                    .bind(&metric)
                    .bind(json_path(&field))
                    .bind(&tz)
                    .bind(offset_days as i32)
                    .bind(hour.unwrap_or(9) as i32)
                    .bind(&batch)
                    .execute(&state.db)
                    .await?;
                last = Some(*max);
            }
            let finished = (batch.len() as i64) < size;
            sqlx::query("UPDATE flow_event_date_seeds SET last_event_id = $2, done = $3 WHERE flow_id = $1").bind(f.id).bind(last).bind(finished).execute(&state.db).await?;
            if finished {
                break;
            }
        }
    }
    // Fires the seeding just found due start now (fires due later wait for their tick).
    for _ in 0..2000 {
        if !fire_one_event_date(state).await? {
            break;
        }
    }
    Ok(())
}

/// Claim one due fire and start its flow, in one transaction: either the run exists and the fire
/// says so, or neither happened. Returns false when nothing is due.
async fn fire_one_event_date(state: &AppState) -> anyhow::Result<bool> {
    let mut tx = state.db.begin().await?;
    let claimed: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT x.flow_id, x.event_id FROM flow_event_date_fires x JOIN flows fl ON fl.id = x.flow_id AND fl.status = 'live'
         WHERE x.outcome = 'pending' AND x.due_at <= now() ORDER BY x.due_at LIMIT 1 FOR UPDATE OF x SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some((flow_id, event_id)) = claimed else { return Ok(false) };
    let f: FlowRow = sqlx::query_as("SELECT * FROM flows WHERE id = $1").bind(flow_id).fetch_one(&mut *tx).await?;
    let outcome = event_date_start(state, &mut tx, &f, event_id).await?;
    let (label, run) = match outcome {
        Ok(run) => ("started", Some(run)),
        Err(why) => (why, None),
    };
    sqlx::query("UPDATE flow_event_date_fires SET outcome = $3, run_id = $4, fired_at = now() WHERE flow_id = $1 AND event_id = $2")
        .bind(flow_id)
        .bind(event_id)
        .bind(label)
        .bind(run)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Checks and start for one event-date fire, inside the claiming transaction.
async fn event_date_start(state: &AppState, tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, f: &FlowRow, event_id: Uuid) -> anyhow::Result<Result<Uuid, &'static str>> {
    let Ok(Trigger::EventDate { metric, field, filters, key, cancelled_by, .. }) = parse_trigger(&f.trigger) else { return Ok(Err("not_started")) };
    let row: Option<(Uuid, Value, Option<f64>, chrono::DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT e.profile_id, e.properties, e.value, e.occurred_at, (e.from_browser IS TRUE AND m.server_only) FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.id = $1 AND e.account_id = $2",
    )
    .bind(event_id)
    .bind(f.account_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((profile_id, props, value, at, forged)) = row else { return Ok(Err("not_started")) };
    // A browser event of a server-only metric (recorded before it became one) never starts it.
    if forged {
        return Ok(Err("not_started"));
    }
    let mut obj = props.clone();
    if let (Some(v), Some(o)) = (value, obj.as_object_mut()) {
        o.entry("value").or_insert(json!(v));
    }
    if !filters.iter().all(|flt| segments::json_filter_matches(&obj, flt)) {
        return Ok(Err("filtered"));
    }
    if let Some(why) = event_date_stale(&state.db, f.account_id, event_id, key.as_deref(), &cancelled_by).await? {
        return Ok(Err(if why == "cancelled" { "cancelled" } else { "superseded" }));
    }
    // A newer version of the same booking (same key) that didn't move its date (only its notes
    // changed, say) doesn't send the reminder a second time: one start per key and due time.
    if let Some(k) = key.as_deref() {
        let kv = obj.pointer(&format!("/{}", k.replace('.', "/"))).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        if let Some(kv) = kv {
            let again: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM flow_event_date_fires x JOIN events e ON e.id = x.event_id
                   WHERE x.flow_id = $1 AND x.outcome = 'started' AND e.profile_id = $2 AND e.properties #>> $3 = $4
                     AND x.due_at = (SELECT due_at FROM flow_event_date_fires WHERE flow_id = $1 AND event_id = $5))",
            )
            .bind(f.id)
            .bind(profile_id)
            .bind(json_path(k))
            .bind(&kv)
            .bind(event_id)
            .fetch_one(&mut **tx)
            .await?;
            if again {
                return Ok(Err("already_sent"));
            }
        }
    }
    let def = parse_definition(&f.definition).map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let Some(entry) = def.entry.clone() else { return Ok(Err("not_started")) };
    if let Some(pf) = &def.settings.profile_filter {
        let seg = segments::load(&state.db, f.account_id, pf, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        if !segments::profile_matches(&state.db, profile_id, &seg).await.map_err(|e| anyhow::anyhow!(e.public_message()))? {
            return Ok(Err("not_started"));
        }
    }
    let day = obj.pointer(&format!("/{}", field.replace('.', "/"))).cloned().unwrap_or(Value::Null);
    let ctx = json!({"event": {"id": event_id, "metric": metric, "properties": props, "value": value, "occurred_at": at}, "event_date": {"field": field, "date": day}});
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO flow_runs (id, flow_id, account_id, profile_id, current_node, context) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(f.id)
        .bind(f.account_id)
        .bind(profile_id)
        .bind(&entry)
        .bind(&ctx)
        .execute(&mut **tx)
        .await?;
    crate::jobs::enqueue(&mut **tx, "flow_step", json!({"run_id": id}), None).await?;
    Ok(Ok(id))
}

/// Event-date flows: how many fires are waiting, started, skipped (filtered, cancelled, replaced by
/// a newer booking) and missed (due while the scheduler couldn't run, more than a day late).
pub async fn event_date_report(db: &PgPool, flow_id: Uuid) -> AppResult<Value> {
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT outcome, count(*) FROM flow_event_date_fires WHERE flow_id = $1 GROUP BY outcome").bind(flow_id).fetch_all(db).await?;
    let next: Option<chrono::DateTime<Utc>> = sqlx::query_scalar("SELECT min(due_at) FROM flow_event_date_fires WHERE flow_id = $1 AND outcome = 'pending'").bind(flow_id).fetch_one(db).await?;
    let mut m = serde_json::Map::new();
    for (k, n) in rows {
        m.insert(k, json!(n));
    }
    Ok(json!({"counts": m, "next_due_at": next}))
}

#[derive(sqlx::FromRow)]
struct Run {
    id: Uuid,
    flow_id: Uuid,
    profile_id: Uuid,
    current_node: Option<String>,
    status: String,
    context: Value,
    claim_id: Option<Uuid>,
    claim_until: Option<chrono::DateTime<Utc>>,
    waiting_until: Option<chrono::DateTime<Utc>>,
}

/// How long a claim on a run holds while its step reaches outside: longer than any provider
/// time-out or rate-limit wait a step can incur, and than the 15 minutes after which a stuck job is
/// retried, so a slow call can't outlive it. A worker that dies holding one is taken over after.
const CLAIM_MINUTES: i64 = 20;

/// What a worker expects the run to be: written by nobody else since it loaded it or last wrote it.
/// Every write is conditional on it and swaps in a fresh claim id, so one worker moves a run at a
/// time; a worker whose write matches nothing stops.
struct Hold {
    run: Uuid,
    claim: Option<Uuid>,
    node: Option<String>,
}

const HELD: &str = "id = $1 AND status = 'active' AND claim_id IS NOT DISTINCT FROM $2 AND current_node IS NOT DISTINCT FROM $3";

/// Test hook: hold a run's worker right after it loaded the run, so a test can run another worker
/// in that gap. Never set outside tests.
#[doc(hidden)]
pub struct StepPause {
    arrived: tokio::sync::Semaphore,
    go: tokio::sync::Semaphore,
}

static STEP_PAUSES: once_cell::sync::Lazy<std::sync::Mutex<HashMap<Uuid, std::sync::Arc<StepPause>>>> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

impl StepPause {
    /// Hold the next worker that loads this run (once).
    #[doc(hidden)]
    pub fn hold(run_id: Uuid) -> std::sync::Arc<Self> {
        let p = std::sync::Arc::new(Self { arrived: tokio::sync::Semaphore::new(0), go: tokio::sync::Semaphore::new(0) });
        if let Ok(mut m) = STEP_PAUSES.lock() {
            m.insert(run_id, p.clone());
        }
        p
    }

    pub async fn arrived(&self) {
        if let Ok(permit) = self.arrived.acquire().await {
            permit.forget();
        }
    }

    pub fn release(&self) {
        self.go.add_permits(1000);
    }
}

async fn step_pause(run_id: Uuid) {
    let pause = STEP_PAUSES.lock().ok().and_then(|mut m| m.remove(&run_id));
    if let Some(p) = pause {
        p.arrived.add_permits(1);
        if let Ok(permit) = p.go.acquire().await {
            permit.forget();
        }
    }
}

async fn finish(db: &PgPool, h: &Hold, status: &str) -> anyhow::Result<()> {
    // Only by the worker holding the run: one taken out of the flow, or moved on by another worker, stays as it is.
    sqlx::query(&format!(
        "UPDATE flow_runs SET status = $4, finished_at = now(), current_node = NULL, claim_id = NULL, claim_until = NULL, waiting_until = NULL WHERE {HELD}"
    ))
    .bind(h.run)
    .bind(h.claim)
    .bind(&h.node)
    .bind(status)
    .execute(db)
    .await?;
    Ok(())
}

/// Job: advance a run until it hits a delay or finishes. A wake-up job carries the wait it is for
/// (`wake`, the claim id it parked the run with) and does nothing unless the run is still in that
/// wait; a job without one does nothing while the run is waiting for its own wake-up.
pub async fn step_job(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let Some(run_id) = payload["run_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Err(anyhow::anyhow!("job payload missing run_id")) };
    let wake = payload["wake"].as_str().and_then(|s| s.parse::<Uuid>().ok());
    step_with(state, run_id, wake, payload).await
}

/// Advance a run now (as a job without a wake would).
pub async fn step(state: &AppState, run_id: Uuid) -> anyhow::Result<()> {
    step_with(state, run_id, None, &json!({"run_id": run_id})).await
}

async fn step_with(state: &AppState, run_id: Uuid, wake: Option<Uuid>, payload: &Value) -> anyhow::Result<()> {
    let Some(run) = sqlx::query_as::<_, Run>("SELECT id, flow_id, profile_id, current_node, status, context, claim_id, claim_until, waiting_until FROM flow_runs WHERE id = $1")
        .bind(run_id)
        .fetch_optional(&state.db)
        .await?
    else {
        return Ok(());
    };
    if run.status != "active" {
        return Ok(());
    }
    match wake {
        // A wake for a wait the run is no longer in (a duplicate, or one already used): nothing to do.
        Some(w) if run.claim_id != Some(w) || run.waiting_until.is_none() => return Ok(()),
        Some(_) => {}
        // Waiting for its own wake-up (this job is a retry of the one that parked it): leave it.
        None if run.waiting_until.is_some() => return Ok(()),
        None => {}
    }
    // Another worker is reaching out for this run right now: try again when its claim runs out.
    if let Some(until) = run.claim_until.filter(|t| *t > Utc::now()) {
        crate::jobs::enqueue(&state.db, "flow_step", payload.clone(), Some(until + Duration::seconds(1))).await?;
        return Ok(());
    }
    step_pause(run.id).await;
    let mut hold = Hold { run: run.id, claim: run.claim_id, node: run.current_node.clone() };
    let flow: FlowRow = sqlx::query_as("SELECT * FROM flows WHERE id = $1").bind(run.flow_id).fetch_one(&state.db).await?;
    match flow.status.as_str() {
        "live" => {}
        // Paused flows hold people in place and pick up when turned back on (the same wake).
        "paused" => {
            crate::jobs::enqueue(&state.db, "flow_step", payload.clone(), Some(Utc::now() + Duration::minutes(15))).await?;
            return Ok(());
        }
        _ => return finish(&state.db, &hold, "exited").await,
    }
    let def = parse_definition(&flow.definition).map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let account = crate::campaigns::load_account(&state.db, flow.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let Some(person) = profile(&state.db, run.profile_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))? else {
        return finish(&state.db, &hold, "exited").await;
    };
    let exit = match def.settings.exit_condition.as_ref() {
        Some(raw) => Some(segments::load(&state.db, flow.account_id, raw, None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?),
        None => None,
    };
    let mut context = run.context.clone();
    let mut current = run.current_node.clone();
    // Flows keyed to an event (event-date flows, and metric flows with a key): stop when the thing
    // was cancelled or a newer event replaced it (e.g. the booking's dates changed).
    let event_date = match parse_trigger(&flow.trigger) {
        Ok(Trigger::EventDate { key, cancelled_by, .. }) => context["event"]["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()).map(|id| (id, key, cancelled_by)),
        Ok(Trigger::Metric { key: Some(key), cancelled_by, .. }) => context["event"]["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()).map(|id| (id, Some(key), cancelled_by)),
        _ => None,
    };

    for _ in 0..100 {
        let Some(node_id) = current.clone() else {
            return finish(&state.db, &hold, "completed").await;
        };
        let Some(node) = def.node(&node_id).cloned() else {
            // The step was deleted while someone waited on it.
            return finish(&state.db, &hold, "exited").await;
        };
        if let Some(exit) = &exit {
            if segments::profile_matches(&state.db, run.profile_id, exit).await.map_err(|e| anyhow::anyhow!(e.public_message()))? {
                return finish(&state.db, &hold, "exited").await;
            }
        }
        if let Some((event_id, key, cancelled_by)) = &event_date {
            if event_date_stale(&state.db, flow.account_id, *event_id, key.as_deref(), cancelled_by).await?.is_some() {
                return finish(&state.db, &hold, "exited").await;
            }
        }
        let waited = context.get("_waited").and_then(|v| v.as_str()) == Some(node_id.as_str());
        // A step that reaches people outside (a message, a notification, a webhook), decided as it
        // runs, takes an exclusive claim first: a fresh claim id with an expiry, written only if
        // nobody else wrote the run since this worker loaded it. "Remove from flow" leaves a claimed
        // run alone, so a removal and a send can't both happen, and two workers can't both send.
        let reaches_out = !matches!(node.kind.as_str(), "delay" | "condition" | "random_split" | "wait_for_reply" | "update_profile" | "add_to_list" | "remove_from_list");
        if !waited || reaches_out {
            let claim = Uuid::now_v7();
            let claimed = sqlx::query(&format!(
                "UPDATE flow_runs SET visited = CASE WHEN $5 THEN visited ELSE visited || to_jsonb($3::text) END, claim_id = $4,
                    claim_until = CASE WHEN $6 THEN now() + make_interval(mins => {CLAIM_MINUTES}::int) END, waiting_until = NULL
                 WHERE {HELD}"
            ))
            .bind(hold.run)
            .bind(hold.claim)
            .bind(&node_id)
            .bind(claim)
            .bind(waited)
            .bind(reaches_out)
            .execute(&state.db)
            .await?
            .rows_affected();
            if claimed == 0 {
                return Ok(());
            }
            hold.claim = Some(claim);
        }
        let mut next = node.next().map(str::to_string);
        match node.kind.as_str() {
            "delay" if !waited => {
                // A time of day, weekdays or a date (see features::flows_plus::delay); plain delays: None.
                let until = match crate::features::flows_plus::delay::begin(&account, &person, &node, &mut context, Utc::now()) {
                    None => None,
                    Some(crate::features::flows_plus::delay::Wait::Until(at)) => Some(at),
                    Some(crate::features::flows_plus::delay::Wait::Exit) => return finish(&state.db, &hold, "exited").await,
                    Some(crate::features::flows_plus::delay::Wait::Continue) => {
                        current = next;
                        if !advance(&state.db, &mut hold, current.as_deref(), &context).await? {
                            return Ok(());
                        }
                        continue;
                    }
                };
                let amount = node.data.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let secs = amount
                    * match node.str("unit") {
                        Some("minutes") => 60.0,
                        Some("hours") => 3600.0,
                        _ => 86400.0,
                    };
                context["_waited"] = json!(node_id);
                let at = until.unwrap_or_else(|| Utc::now() + Duration::milliseconds((secs * 1000.0) as i64));
                // The wait and its wake-up are saved together, and only while the run is active.
                wait_until(&state.db, &hold, &node_id, &context, at).await?;
                return Ok(());
            }
            "delay" => {
                if let Some(at) = crate::features::flows_plus::delay::wake(&mut context, Utc::now()) {
                    // Woken before its time (early, or retried after a crash): keep waiting for it.
                    wait_until(&state.db, &hold, &node_id, &context, at).await?;
                    return Ok(());
                }
                if let Some(o) = context.as_object_mut() {
                    o.remove("_waited");
                }
            }
            "email" | "sms" => {
                // Smart sending: skip if this person got any marketing message in the last N hours (0 = off).
                // Inbox replies and other conversational texts (HELP, sign-up texts) don't count.
                let hours = def.settings.smart_sending_hours.unwrap_or(0);
                let recent: bool = hours > 0 && crate::features::sendtime::recently_messaged(&state.db, run.profile_id, &node.kind, hours).await?;
                if !recent {
                    let event = context.get("event").cloned();
                    let content = node.data.get("content").cloned().unwrap_or(json!({}));
                    let out = sending::send(
                        state,
                        SendSpec {
                            account: &account,
                            profile: &person,
                            channel: &node.kind,
                            content: &content,
                            campaign_id: None,
                            flow_id: Some(flow.id),
                            flow_node_id: Some(&node_id),
                            event: event.as_ref(),
                            is_test: false,
                            message_id: None,
                            to_override: None,
                        },
                    )
                    .await;
                    match out {
                        Ok(Outcome::Deferred(until)) => {
                            wait_until(&state.db, &hold, &node_id, &context, until).await?;
                            return Ok(());
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(run = %run.id, node = %node_id, error = %e.public_message(), "flow message failed");
                        }
                    }
                }
            }
            "condition" => {
                let cond = segments::load(&state.db, flow.account_id, &node.data["condition"], None).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                // A trigger split also tests the event that started the run (see features::flows_plus::split).
                let yes = crate::features::flows_plus::split::event_matches(&node, &context)
                    && segments::profile_matches(&state.db, run.profile_id, &cond).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                next = node.str(if yes { "yes" } else { "no" }).map(str::to_string);
            }
            // Wait until they text back (yes) or the time runs out (no); see `features::inbox`. The
            // start and deadline are saved with the job in one transaction, and the branch is decided
            // only by replies between them, however late the job runs.
            "wait_for_reply" if !waited => {
                let amount = node.data.get("amount").and_then(|v| v.as_f64()).unwrap_or(1.0);
                let secs = (amount
                    * match node.str("unit") {
                        Some("minutes") => 60.0,
                        Some("hours") => 3600.0,
                        _ => 86400.0,
                    })
                .clamp(1.0, 30.0 * 86400.0);
                let since = Utc::now();
                let until = since + Duration::milliseconds((secs * 1000.0) as i64);
                context["_waited"] = json!(node_id);
                context["_reply_since"] = json!(since);
                context["_reply_until"] = json!(until);
                wait_until(&state.db, &hold, &node_id, &context, until).await?;
                return Ok(());
            }
            "wait_for_reply" => {
                let at = |k: &str| context.get(k).and_then(|v| serde_json::from_value::<chrono::DateTime<Utc>>(v.clone()).ok());
                let since = at("_reply_since").unwrap_or_else(Utc::now);
                // A deadline that can't be read counts nothing after the wait began.
                let deadline = at("_reply_until").unwrap_or(since);
                let now = Utc::now();
                let replied = crate::features::inbox::replied_between(&state.db, run.profile_id, since, deadline.min(now)).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                if !replied && now < deadline {
                    // Woken before the deadline without a reply inside the wait: keep waiting.
                    wait_until(&state.db, &hold, &node_id, &context, deadline).await?;
                    return Ok(());
                }
                if let Some(o) = context.as_object_mut() {
                    o.remove("_waited");
                    o.remove("_reply_since");
                    o.remove("_reply_until");
                }
                next = node.str(if replied { "yes" } else { "no" }).map(str::to_string);
            }
            "random_split" => {
                let percent = split_percent(&node).unwrap_or(50);
                let path_a = rand::Rng::gen_range(&mut rand::thread_rng(), 0..100) < percent;
                next = node.str(if path_a { "yes" } else { "no" }).map(str::to_string);
            }
            "update_profile" => {
                let props = node.data.get("properties").cloned().unwrap_or(json!({}));
                sqlx::query("UPDATE profiles SET properties = properties || $2, updated_at = now() WHERE id = $1")
                    .bind(run.profile_id)
                    .bind(&props)
                    .execute(&state.db)
                    .await?;
            }
            "add_to_list" | "remove_from_list" => {
                let owned = match node.str("list_id").and_then(|s| s.parse::<Uuid>().ok()) {
                    Some(id) => crate::audience::list_belongs(&state.db, flow.account_id, id).await.ok().map(|_| id),
                    None => None,
                };
                if let Some(list_id) = owned {
                    if node.kind == "add_to_list" {
                        crate::audience::add_to_list(&state.db, flow.account_id, list_id, &[run.profile_id]).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                    } else {
                        crate::audience::remove_from_list(&state.db, flow.account_id, list_id, &[run.profile_id]).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                    }
                }
            }
            "notify" => {
                // Past the account's daily allowance (or a busy provider): wait, then send what's left.
                if let Some(until) = crate::features::flows_plus::notify::run(state, &account, &flow, &person, run.id, &node_id, &node, &context).await? {
                    wait_until(&state.db, &hold, &node_id, &context, until).await?;
                    return Ok(());
                }
            }
            "webhook" => {
                let url = node.str("url").unwrap_or_default().to_string();
                let payload = json!({
                    "flow": {"id": flow.id, "name": flow.name}, "node_id": node_id,
                    "profile": person, "event": context.get("event"), "body": node.data.get("body"),
                });
                if let Err(e) = call_webhook(state, flow.account_id, &url, node.str("method") == Some("GET"), &payload).await {
                    tracing::warn!(run = %run.id, url = %url, error = %e, "flow webhook failed");
                }
            }
            _ => {}
        }
        current = next;
        if !advance(&state.db, &mut hold, current.as_deref(), &context).await? {
            return Ok(());
        }
    }
    finish(&state.db, &hold, "failed").await
}

/// Move the run to its next step (ending any claim), if this worker still holds it. False: someone
/// else moved it or it was taken out of the flow, so stop.
async fn advance(db: &PgPool, hold: &mut Hold, node: Option<&str>, context: &Value) -> anyhow::Result<bool> {
    let claim = Uuid::now_v7();
    let n = sqlx::query(&format!("UPDATE flow_runs SET current_node = $4, context = $5, claim_id = $6, claim_until = NULL, waiting_until = NULL WHERE {HELD}"))
        .bind(hold.run)
        .bind(hold.claim)
        .bind(&hold.node)
        .bind(node)
        .bind(context)
        .bind(claim)
        .execute(db)
        .await?
        .rows_affected();
    hold.claim = Some(claim);
    hold.node = node.map(str::to_string);
    Ok(n == 1)
}

/// Park the run at `node` until `at`, if this worker still holds it: its state and its one wake-up
/// job in one transaction. The wake carries the claim id it parks with, the step and the deadline;
/// only that wake can move the run on from here.
async fn wait_until(db: &PgPool, hold: &Hold, node: &str, context: &Value, at: chrono::DateTime<Utc>) -> anyhow::Result<()> {
    let wake = Uuid::now_v7();
    let mut tx = db.begin().await?;
    let n = sqlx::query(&format!("UPDATE flow_runs SET current_node = $4, context = $5, claim_id = $6, claim_until = NULL, waiting_until = $7 WHERE {HELD}"))
        .bind(hold.run)
        .bind(hold.claim)
        .bind(&hold.node)
        .bind(node)
        .bind(context)
        .bind(wake)
        .bind(at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 1 {
        crate::jobs::enqueue(&mut *tx, "flow_step", json!({"run_id": hold.run, "wake": wake, "node": node, "at": at}), Some(at)).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn stats(db: &PgPool, account_id: Uuid, flow_id: Uuid) -> AppResult<(MessageStats, i64, i64)> {
    let q = format!("SELECT {STATS_SELECT} FROM messages m WHERE m.flow_id = $1 AND NOT m.is_test");
    let mut s: MessageStats = sqlx::query_as(&q).bind(flow_id).fetch_one(db).await?;
    let q = format!("SELECT count(*)::bigint, COALESCE(sum(value), 0)::float8 FROM ({} AND e.profile_id IN (SELECT profile_id FROM messages WHERE flow_id = $2)) x WHERE x.flow_id = $2", crate::campaigns::ATTRIBUTION_SQL);
    let (orders, revenue): (i64, f64) = sqlx::query_as(&q).bind(account_id).bind(flow_id).fetch_one(db).await?;
    s.orders = orders;
    s.revenue = revenue;
    let (active, total): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER (WHERE status = 'active'), count(*) FROM flow_runs WHERE flow_id = $1")
        .bind(flow_id)
        .fetch_one(db)
        .await?;
    Ok((s.finish(), active, total))
}

pub async fn node_report(db: &PgPool, account_id: Uuid, flow: &FlowRow) -> AppResult<Vec<Value>> {
    let def = parse_definition(&flow.definition)?;
    let mut out = Vec::new();
    for n in &def.nodes {
        let entered: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_runs WHERE flow_id = $1 AND visited @> jsonb_build_array($2::text)")
            .bind(flow.id)
            .bind(&n.id)
            .fetch_one(db)
            .await?;
        let q = format!("SELECT {STATS_SELECT} FROM messages m WHERE m.flow_id = $1 AND m.flow_node_id = $2 AND NOT m.is_test");
        let mut s: MessageStats = sqlx::query_as(&q).bind(flow.id).bind(&n.id).fetch_one(db).await?;
        let q = format!("SELECT count(*)::bigint, COALESCE(sum(value), 0)::float8 FROM ({}) x WHERE x.flow_id = $2 AND x.flow_node_id = $3", crate::campaigns::ATTRIBUTION_SQL);
        let (orders, revenue): (i64, f64) = sqlx::query_as(&q).bind(account_id).bind(flow.id).bind(&n.id).fetch_one(db).await?;
        s.orders = orders;
        s.revenue = revenue;
        out.push(json!({"node_id": n.id, "entered": entered, "stats": s.finish()}));
    }
    Ok(out)
}

// ---------- Blueprints ----------

fn email(id: &str, name: &str, subject: &str, preheader: &str, heading: &str, body: &str, button: (&str, &str), next: Option<&str>) -> Value {
    json!({
        "id": id, "type": "email", "name": name, "next": next,
        "content": {
            "subject": subject, "preheader": preheader,
            "blocks": [
                {"type": "heading", "text": heading, "align": "left"},
                {"type": "text", "html": body},
                {"type": "button", "text": button.0, "href": button.1}
            ]
        }
    })
}

/// The cart's products, filled from the triggering event (see `features::catalog`).
fn cart_items() -> Value {
    json!({"type": "products", "source": "event_items", "count": 6, "columns": 2, "button_text": "View", "hide_out_of_stock": false})
}

fn delay(id: &str, amount: f64, unit: &str, next: Option<&str>) -> Value {
    json!({"id": id, "type": "delay", "amount": amount, "unit": unit, "next": next})
}

/// Starter flows. `{list}` and `{segment}` placeholders are filled when a blueprint is created.
pub fn blueprints(website: &str) -> Vec<Value> {
    let site = if website.is_empty() { "https://example.com" } else { website };
    let cart = "{{ event.properties.checkout_url|default(organization.website) }}";
    vec![
        json!({
            "key": "welcome_series", "name": "Welcome series",
            "description": "Greet new subscribers with three emails over five days. The highest-earning flow for most brands.",
            "channel_mix": ["email"],
            "trigger": {"type": "list", "list_id": "{list}"},
            "definition": {"entry": "w1", "settings": {"reentry": "never"}, "nodes": [
                email("w1", "Welcome", "Welcome to {{ organization.name }}, {{ first_name|default('friend') }}!", "Thanks for joining us",
                      "Welcome aboard, {{ first_name|default('friend') }}", "<p>Thanks for signing up. Here's what to expect: early access, member-only offers and the occasional behind-the-scenes story.</p>", ("Start shopping", site), Some("d1")),
                delay("d1", 2.0, "days", Some("w2")),
                email("w2", "Our story", "Why we started {{ organization.name }}", "A quick story", "Why we do this", "<p>We started because we thought it could be done better. Here's what makes us different.</p>", ("Read more", site), Some("d2")),
                delay("d2", 3.0, "days", Some("w3")),
                email("w3", "Bestsellers", "Our customers' favorites", "The ones people come back for", "Our bestsellers", "<p>Not sure where to start? These are the picks our customers love most.</p>", ("Shop bestsellers", site), None)
            ]}
        }),
        json!({
            "key": "abandoned_checkout", "name": "Abandoned checkout",
            "description": "Remind shoppers who started checkout but didn't buy: an email after 4 hours, then a text to SMS subscribers the next day.",
            "channel_mix": ["email", "sms"],
            "trigger": {"type": "metric", "metric": "Started Checkout"},
            "definition": {"entry": "a0", "settings": {"reentry": {"after_days": 7}, "exit_condition": {"match": "all", "conditions": [
                {"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1, "within_days": 2}]}}, "nodes": [
                delay("a0", 4.0, "hours", Some("a1")),
                json!({"id": "a1", "type": "email", "name": "Reminder", "next": "a2", "content": {
                    "subject": "You left something behind", "preheader": "Your cart is saved",
                    "blocks": [
                        {"type": "heading", "text": "Still thinking it over?"},
                        {"type": "text", "html": "<p>We saved your cart. Finish checking out whenever you're ready.</p>"},
                        cart_items(),
                        {"type": "button", "text": "Return to my cart", "href": cart}
                    ]}}),
                delay("a2", 1.0, "days", Some("a3")),
                json!({"id": "a3", "type": "condition", "condition": {"match": "all", "conditions": [{"type": "consent", "channel": "sms", "status": "subscribed"}]}, "yes": "a4", "no": "a5"}),
                json!({"id": "a4", "type": "sms", "name": "SMS nudge", "next": null, "content": {"body": "Hi {{ first_name|default('there') }}, your cart is waiting: " .to_string() + cart}}),
                json!({"id": "a5", "type": "email", "name": "Last call", "next": null, "content": {
                    "subject": "Last chance: your cart expires soon", "preheader": "Don't miss out",
                    "blocks": [
                        {"type": "heading", "text": "Your cart is about to expire", "align": "left"},
                        {"type": "text", "html": "<p>Items in your cart are popular. Complete your order before they sell out.</p>"},
                        cart_items(),
                        {"type": "button", "text": "Complete my order", "href": cart}
                    ]}})
            ]}
        }),
        json!({
            "key": "browse_abandonment", "name": "Browse abandonment",
            "description": "Follow up two hours after someone views a product and doesn't buy.",
            "channel_mix": ["email"],
            "trigger": {"type": "metric", "metric": "Viewed Product"},
            "definition": {"entry": "b0", "settings": {"reentry": {"after_days": 14}, "exit_condition": {"match": "any", "conditions": [
                {"type": "event", "metric": "Started Checkout", "op": "at_least", "count": 1, "within_days": 1},
                {"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1, "within_days": 1}]}}, "nodes": [
                delay("b0", 2.0, "hours", Some("b1")),
                json!({"id": "b1", "type": "email", "name": "Still interested?", "next": null, "content": {
                    "subject": "Still thinking about {{ event.properties.name|default('it') }}?", "preheader": "Take another look",
                    "blocks": [
                        {"type": "heading", "text": "Take another look"},
                        {"type": "products", "source": "event_items", "count": 1, "columns": 1, "button_text": "View it again", "hide_out_of_stock": false},
                        {"type": "text", "html": "<p>You might also like:</p>"},
                        {"type": "products", "source": "recommended", "count": 3, "columns": 3, "button_text": "Shop now", "fallback": "best_sellers"}
                    ]}})
            ]}
        }),
        json!({
            "key": "post_purchase", "name": "Post-purchase thank you",
            "description": "Thank first-time and repeat buyers three days after an order and ask for a review.",
            "channel_mix": ["email"],
            "trigger": {"type": "metric", "metric": "Placed Order"},
            "definition": {"entry": "p0", "settings": {"reentry": "always"}, "nodes": [
                delay("p0", 3.0, "days", Some("p1")),
                email("p1", "Thank you", "Thanks for your order, {{ first_name|default('friend') }}", "We hope you love it", "Thank you!", "<p>We hope you're enjoying your order. If you have a minute, we'd love to hear what you think.</p>", ("Leave a review", site), None)
            ]}
        }),
        json!({
            "key": "winback", "name": "Customer winback",
            "description": "Win back customers who bought before but not in the last 90 days.",
            "channel_mix": ["email"],
            "trigger": {"type": "segment", "segment_id": "{segment}"},
            "segment": {"name": "Lapsed customers (90 days)", "definition": {"match": "all", "conditions": [
                {"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1},
                {"type": "event", "metric": "Placed Order", "op": "zero", "within_days": 90}]}},
            "definition": {"entry": "x1", "settings": {"reentry": {"after_days": 180}}, "nodes": [
                email("x1", "We miss you", "We miss you, {{ first_name|default('friend') }}", "Here's what's new", "It's been a while", "<p>A lot has changed since your last visit. Come see what's new.</p>", ("See what's new", site), None)
            ]}
        }),
        json!({
            "key": "back_in_stock", "name": "Back in stock",
            "description": "Email people the moment a product they asked about is back. Needs the back-in-stock widget on your product pages and a product catalog.",
            "channel_mix": ["email"],
            "trigger": {"type": "metric", "metric": "Back in Stock"},
            "definition": {"entry": "k1", "settings": {"reentry": "always"}, "nodes": [
                json!({"id": "k1", "type": "email", "name": "It's back", "next": null, "content": {
                    "subject": "{{ event.properties.name|default('It') }} is back in stock", "preheader": "Get it before it sells out again",
                    "blocks": [
                        {"type": "heading", "text": "It's back!"},
                        {"type": "text", "html": "<p>You asked us to tell you when this was back. It is, but stock is limited.</p>"},
                        {"type": "products", "source": "event_items", "count": 1, "columns": 1, "button_text": "Get it now", "hide_out_of_stock": false}
                    ]}})
            ]}
        }),
        json!({
            "key": "price_drop", "name": "Price drop",
            "description": "Tell people who viewed or carted a product that its price just dropped. Needs a product catalog (a feed, CSV or the API) to see price changes.",
            "channel_mix": ["email"],
            "trigger": {"type": "metric", "metric": "Price Dropped"},
            "definition": {"entry": "r1", "settings": {"reentry": "always", "smart_sending_hours": 16, "exit_condition": {"match": "all", "conditions": [
                {"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1, "within_days": 1}]}}, "nodes": [
                json!({"id": "r1", "type": "email", "name": "Price dropped", "next": null, "content": {
                    "subject": "Price drop: {{ event.properties.name|default('something you liked') }}", "preheader": "Now {{ event.properties.new_price_text }}",
                    "blocks": [
                        {"type": "heading", "text": "Good news: it's cheaper now"},
                        {"type": "text", "html": "<p>{{ event.properties.name }} was {{ event.properties.old_price_text }}. It's now {{ event.properties.new_price_text }}.</p>"},
                        {"type": "products", "source": "event_items", "count": 1, "columns": 1, "button_text": "Take a look"}
                    ]}})
            ]}
        }),
        json!({
            "key": "birthday", "name": "Birthday",
            "description": "Send a birthday note on each profile's `birthday` property.",
            "channel_mix": ["email"],
            "trigger": {"type": "date_property", "field": "birthday", "offset_days": 0},
            "definition": {"entry": "h1", "settings": {"reentry": "always"}, "nodes": [
                email("h1", "Happy birthday", "Happy birthday, {{ first_name|default('friend') }}! 🎉", "A little something for you", "Happy birthday!", "<p>Wishing you a great year ahead from all of us at {{ organization.name }}.</p>", ("Celebrate with us", site), None)
            ]}
        }),
    ]
}

/// Create a draft flow from a blueprint, creating the list/segment it needs.
pub async fn from_blueprint(db: &PgPool, account_id: Uuid, key: &str, name: Option<String>) -> AppResult<FlowRow> {
    let account = crate::campaigns::load_account(db, account_id).await?;
    let bp = blueprints(account.website.as_deref().unwrap_or(""))
        .into_iter()
        .find(|b| b["key"] == key)
        .ok_or_else(|| AppError::bad(format!("unknown blueprint {key:?}")))?;
    let mut trigger = bp["trigger"].clone();
    if trigger["list_id"] == "{list}" {
        // Starter flows get their own sign-up list, so imported contacts don't all get a welcome series.
        let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM lists WHERE account_id = $1 AND name = 'Newsletter' LIMIT 1")
            .bind(account_id)
            .fetch_optional(db)
            .await?;
        let list_id = match existing {
            Some(id) => id,
            None => {
                let id = Uuid::now_v7();
                sqlx::query("INSERT INTO lists (id, account_id, name) VALUES ($1, $2, 'Newsletter')").bind(id).bind(account_id).execute(db).await?;
                id
            }
        };
        trigger["list_id"] = json!(list_id);
    }
    if trigger["segment_id"] == "{segment}" {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO segments (id, account_id, name, definition) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(account_id)
            .bind(bp["segment"]["name"].as_str().unwrap_or("Segment"))
            .bind(&bp["segment"]["definition"])
            .execute(db)
            .await?;
        trigger["segment_id"] = json!(id);
    }
    parse_trigger(&trigger)?;
    parse_definition(&bp["definition"])?;
    let row: FlowRow = sqlx::query_as("INSERT INTO flows (id, account_id, name, trigger, definition) VALUES ($1, $2, $3, $4, $5) RETURNING *")
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(name.unwrap_or_else(|| bp["name"].as_str().unwrap_or("Flow").to_string()))
        .bind(&trigger)
        .bind(&bp["definition"])
        .fetch_one(db)
        .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_public_addresses() {
        let public = |s: &str| super::ip_is_public(s.parse().unwrap());
        for ok in ["8.8.8.8", "2606:4700:4700::1111", "2002:808:808::1", "64:ff9b::808:808"] {
            assert!(public(ok), "{ok}");
        }
        for no in ["10.0.0.1", "127.0.0.1", "169.254.169.254", "::1", "::ffff:10.0.0.1", "::10.0.0.1", "64:ff9b::a00:1", "64:ff9b::a9fe:a9fe",
            "2002:a00:1::1", "2001:0:4136:e378::1", "2001:db8::1", "fec0::1", "fd00::1", "fe80::1", "ff02::1"] {
            assert!(!public(no), "{no}");
        }
    }

    use super::*;

    #[test]
    fn blueprints_are_valid() {
        for bp in blueprints("https://shop.example.com") {
            parse_definition(&bp["definition"]).unwrap_or_else(|e| panic!("{}: {}", bp["key"], e));
        }
    }

    #[test]
    fn rejects_loops_and_dangling_edges() {
        let looped = json!({"entry": "a", "nodes": [
            {"id": "a", "type": "delay", "amount": 1, "unit": "days", "next": "b"},
            {"id": "b", "type": "delay", "amount": 1, "unit": "days", "next": "a"}]});
        assert!(parse_definition(&looped).is_err());
        let dangling = json!({"entry": "a", "nodes": [{"id": "a", "type": "delay", "amount": 1, "unit": "days", "next": "zzz"}]});
        assert!(parse_definition(&dangling).is_err());
        let unknown = json!({"entry": "a", "nodes": [{"id": "a", "type": "teleport"}]});
        assert!(parse_definition(&unknown).is_err());
    }

    #[test]
    fn random_split_needs_a_whole_percent() {
        let split = |percent: Value| json!({"entry": "s", "nodes": [
            {"id": "s", "type": "random_split", "percent": percent, "yes": "a", "no": null},
            {"id": "a", "type": "delay", "amount": 1, "unit": "days", "next": null}]});
        for ok in [json!(1), json!(50), json!(99)] {
            assert!(parse_definition(&split(ok.clone())).is_ok(), "{ok}");
        }
        for bad in [json!(0), json!(100), json!(-1), json!(12.5), json!("50"), Value::Null] {
            assert!(parse_definition(&split(bad.clone())).is_err(), "{bad}");
        }
    }
}
