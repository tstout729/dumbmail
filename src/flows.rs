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
    serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid trigger: {e}")))
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
            "condition" => vec!["yes", "no"],
            "delay" | "email" | "sms" | "update_profile" | "add_to_list" | "remove_from_list" | "webhook" => vec!["next"],
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
            "update_profile" => need("properties")?,
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
            let seg = v6.segments();
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80)
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
    if addrs.is_empty() || addrs.iter().any(|a| !ip_is_public(a.ip())) {
        return Err(AppError::bad("webhook url must point to a public internet address"));
    }
    Ok((host, addrs[0]))
}

/// Fetch a public web page (for checking a customer's site for the snippet): public IPs only,
/// pinned DNS, a few redirects re-checked hop by hop, and at most 3 MB read.
pub async fn fetch_public_page(raw: &str) -> AppResult<String> {
    let mut url = raw.to_string();
    for _ in 0..4 {
        let (host, addr) = check_webhook_url(&url).await.map_err(|_| AppError::bad("enter a public website address"))?;
        let client = reqwest::Client::builder()
            .resolve(&host, addr)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Mozilla/5.0 (compatible; PostlarkSnippetCheck/0.2)")
            .build()
            .map_err(|e| AppError::Internal(e.into()))?;
        let res = client.get(&url).send().await.map_err(|e| AppError::bad(format!("couldn't load that page: {e}")))?;
        if res.status().is_redirection() {
            let loc = res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default();
            url = url::Url::parse(&url).and_then(|b| b.join(loc)).map(|u| u.to_string()).map_err(|_| AppError::bad("the page redirected somewhere invalid"))?;
            continue;
        }
        if !res.status().is_success() {
            return Err(AppError::bad(format!("that page returned HTTP {}", res.status().as_u16())));
        }
        let mut body = Vec::new();
        let mut stream = res;
        while let Some(chunk) = stream.chunk().await.map_err(|e| AppError::bad(format!("couldn't read that page: {e}")))? {
            body.extend_from_slice(&chunk);
            if body.len() > 3 * 1024 * 1024 {
                break;
            }
        }
        return Ok(String::from_utf8_lossy(&body).into_owned());
    }
    Err(AppError::bad("that page redirects too many times"))
}

async fn call_webhook(url: &str, get: bool, payload: &Value) -> anyhow::Result<()> {
    let (host, addr) = check_webhook_url(url).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let client = reqwest::Client::builder()
        .resolve(&host, addr)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("postlark-webhook/0.1")
        .build()?;
    let req = if get { client.get(url) } else { client.post(url).json(payload) };
    req.send().await?.error_for_status()?;
    Ok(())
}

async fn profile(db: &PgPool, id: Uuid) -> AppResult<Option<Profile>> {
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1");
    Ok(sqlx::query_as::<_, Profile>(&q).bind(id).fetch_optional(db).await?)
}

/// Start a run unless re-entry rules or the profile filter say no. Returns the run id.
pub async fn start(state: &AppState, flow: &FlowRow, profile_id: Uuid, context: Value) -> AppResult<Option<Uuid>> {
    let def = parse_definition(&flow.definition)?;
    let Some(entry) = def.entry.clone() else { return Ok(None) };
    let reentry = def.settings.reentry.clone().unwrap_or(json!("never"));
    let blocked: bool = match (&reentry, reentry.get("after_days").and_then(|d| d.as_f64())) {
        (Value::String(s), _) if s == "always" => {
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flow_runs WHERE flow_id = $1 AND profile_id = $2 AND status = 'active')")
                .bind(flow.id)
                .bind(profile_id)
                .fetch_one(&state.db)
                .await?
        }
        (_, Some(days)) => {
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flow_runs WHERE flow_id = $1 AND profile_id = $2 AND (status = 'active' OR started_at > now() - make_interval(secs => $3)))")
                .bind(flow.id)
                .bind(profile_id)
                .bind(days * 86400.0)
                .fetch_one(&state.db)
                .await?
        }
        _ => {
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flow_runs WHERE flow_id = $1 AND profile_id = $2)")
                .bind(flow.id)
                .bind(profile_id)
                .fetch_one(&state.db)
                .await?
        }
    };
    if blocked {
        return Ok(None);
    }
    if let Some(f) = &def.settings.profile_filter {
        if !segments::profile_matches(&state.db, profile_id, &segments::parse(f)?).await? {
            return Ok(None);
        }
    }
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO flow_runs (id, flow_id, account_id, profile_id, current_node, context) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(flow.id)
        .bind(flow.account_id)
        .bind(profile_id)
        .bind(&entry)
        .bind(&context)
        .execute(&state.db)
        .await?;
    crate::jobs::enqueue(&state.db, "flow_step", json!({"run_id": id}), None).await?;
    Ok(Some(id))
}

/// Job: an event happened; start matching metric-triggered flows.
pub async fn on_event(state: &AppState, event_id: Uuid) -> anyhow::Result<()> {
    let row: Option<(Uuid, Uuid, String, Value, Option<f64>, chrono::DateTime<Utc>)> = sqlx::query_as(
        "SELECT e.account_id, e.profile_id, m.name, e.properties, e.value, e.occurred_at FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.id = $1",
    )
    .bind(event_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, profile_id, metric, props, value, at)) = row else { return Ok(()) };
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
                let ctx = json!({"event": {"id": event_id, "metric": metric, "properties": props, "value": value, "occurred_at": at}});
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
    let row: Option<(Uuid, Value, Option<chrono::DateTime<Utc>>)> = sqlx::query_as("SELECT account_id, definition, refreshed_at FROM segments WHERE id = $1")
        .bind(segment_id)
        .fetch_optional(&state.db)
        .await?;
    let Some((account_id, def, refreshed_at)) = row else { return Ok(()) };
    let def = segments::parse(&def).map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let entered = segments::refresh(&state.db, account_id, segment_id, &def).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    // The first refresh only records who is already in the segment; it does not trigger anyone.
    if refreshed_at.is_none() {
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
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Run {
    id: Uuid,
    flow_id: Uuid,
    profile_id: Uuid,
    current_node: Option<String>,
    status: String,
    context: Value,
}

async fn finish(db: &PgPool, run_id: Uuid, status: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE flow_runs SET status = $2, finished_at = now(), current_node = NULL WHERE id = $1")
        .bind(run_id)
        .bind(status)
        .execute(db)
        .await?;
    Ok(())
}

/// Job: advance a run until it hits a delay or finishes.
pub async fn step(state: &AppState, run_id: Uuid) -> anyhow::Result<()> {
    let Some(run) = sqlx::query_as::<_, Run>("SELECT id, flow_id, profile_id, current_node, status, context FROM flow_runs WHERE id = $1")
        .bind(run_id)
        .fetch_optional(&state.db)
        .await?
    else {
        return Ok(());
    };
    if run.status != "active" {
        return Ok(());
    }
    let flow: FlowRow = sqlx::query_as("SELECT * FROM flows WHERE id = $1").bind(run.flow_id).fetch_one(&state.db).await?;
    match flow.status.as_str() {
        "live" => {}
        // Paused flows hold people in place and pick up when turned back on.
        "paused" => {
            crate::jobs::enqueue(&state.db, "flow_step", json!({"run_id": run.id}), Some(Utc::now() + Duration::minutes(15))).await?;
            return Ok(());
        }
        _ => return finish(&state.db, run.id, "exited").await,
    }
    let def = parse_definition(&flow.definition).map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let account = crate::campaigns::load_account(&state.db, flow.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let Some(person) = profile(&state.db, run.profile_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))? else {
        return finish(&state.db, run.id, "exited").await;
    };
    let exit = def.settings.exit_condition.as_ref().map(segments::parse).transpose().map_err(|e| anyhow::anyhow!(e.public_message()))?;
    let mut context = run.context.clone();
    let mut current = run.current_node.clone();

    for _ in 0..100 {
        let Some(node_id) = current.clone() else {
            return finish(&state.db, run.id, "completed").await;
        };
        let Some(node) = def.node(&node_id).cloned() else {
            // The step was deleted while someone waited on it.
            return finish(&state.db, run.id, "exited").await;
        };
        if let Some(exit) = &exit {
            if segments::profile_matches(&state.db, run.profile_id, exit).await.map_err(|e| anyhow::anyhow!(e.public_message()))? {
                return finish(&state.db, run.id, "exited").await;
            }
        }
        let waited = context.get("_waited").and_then(|v| v.as_str()) == Some(node_id.as_str());
        if !waited {
            sqlx::query("UPDATE flow_runs SET visited = visited || to_jsonb($2::text) WHERE id = $1")
                .bind(run.id)
                .bind(&node_id)
                .execute(&state.db)
                .await?;
        }
        let mut next = node.next().map(str::to_string);
        match node.kind.as_str() {
            "delay" if !waited => {
                let amount = node.data.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let secs = amount
                    * match node.str("unit") {
                        Some("minutes") => 60.0,
                        Some("hours") => 3600.0,
                        _ => 86400.0,
                    };
                context["_waited"] = json!(node_id);
                sqlx::query("UPDATE flow_runs SET current_node = $2, context = $3 WHERE id = $1")
                    .bind(run.id)
                    .bind(&node_id)
                    .bind(&context)
                    .execute(&state.db)
                    .await?;
                let at = Utc::now() + Duration::milliseconds((secs * 1000.0) as i64);
                crate::jobs::enqueue(&state.db, "flow_step", json!({"run_id": run.id}), Some(at)).await?;
                return Ok(());
            }
            "delay" => {
                if let Some(o) = context.as_object_mut() {
                    o.remove("_waited");
                }
            }
            "email" | "sms" => {
                // Smart sending: skip if this person got any message in the last N hours (0 = off).
                let hours = def.settings.smart_sending_hours.unwrap_or(0);
                let recent: bool = hours > 0
                    && sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE profile_id = $1 AND channel = $2 AND status IN ('sent','delivered') AND NOT is_test AND sent_at > now() - make_interval(hours => $3))")
                        .bind(run.profile_id)
                        .bind(&node.kind)
                        .bind(hours as i32)
                        .fetch_one(&state.db)
                        .await?;
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
                            sqlx::query("UPDATE flow_runs SET current_node = $2, context = $3 WHERE id = $1")
                                .bind(run.id)
                                .bind(&node_id)
                                .bind(&context)
                                .execute(&state.db)
                                .await?;
                            crate::jobs::enqueue(&state.db, "flow_step", json!({"run_id": run.id}), Some(until)).await?;
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
                let cond = segments::parse(&node.data["condition"]).map_err(|e| anyhow::anyhow!(e.public_message()))?;
                let yes = segments::profile_matches(&state.db, run.profile_id, &cond).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
                next = node.str(if yes { "yes" } else { "no" }).map(str::to_string);
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
                        sqlx::query("DELETE FROM list_members WHERE list_id = $1 AND profile_id = $2").bind(list_id).bind(run.profile_id).execute(&state.db).await?;
                    }
                }
            }
            "webhook" => {
                let url = node.str("url").unwrap_or_default().to_string();
                let payload = json!({
                    "flow": {"id": flow.id, "name": flow.name}, "node_id": node_id,
                    "profile": person, "event": context.get("event"), "body": node.data.get("body"),
                });
                if let Err(e) = call_webhook(&url, node.str("method") == Some("GET"), &payload).await {
                    tracing::warn!(run = %run.id, url = %url, error = %e, "flow webhook failed");
                }
            }
            _ => {}
        }
        current = next;
        sqlx::query("UPDATE flow_runs SET current_node = $2, context = $3 WHERE id = $1")
            .bind(run.id)
            .bind(&current)
            .bind(&context)
            .execute(&state.db)
            .await?;
    }
    finish(&state.db, run.id, "failed").await
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
                        {"type": "button", "text": "Return to my cart", "href": cart}
                    ]}}),
                delay("a2", 1.0, "days", Some("a3")),
                json!({"id": "a3", "type": "condition", "condition": {"match": "all", "conditions": [{"type": "consent", "channel": "sms", "status": "subscribed"}]}, "yes": "a4", "no": "a5"}),
                json!({"id": "a4", "type": "sms", "name": "SMS nudge", "next": null, "content": {"body": "Hi {{ first_name|default('there') }}, your cart is waiting: " .to_string() + cart}}),
                email("a5", "Last call", "Last chance: your cart expires soon", "Don't miss out", "Your cart is about to expire", "<p>Items in your cart are popular. Complete your order before they sell out.</p>", ("Complete my order", cart), None)
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
                        {"type": "product", "title": "{{ event.properties.name }}", "image": "{{ event.properties.image_url }}", "price": "{{ event.properties.price }}", "href": "{{ event.properties.url|default(organization.website) }}", "button_text": "View it again"}
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
}
