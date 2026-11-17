//! Custom reports: a whitelisted JSON spec compiled to parameterized SQL.
//!
//! Nothing in a spec becomes SQL text except names picked from the fixed lists below; every value
//! (dates, ids, metric names, property keys and filter values) is bound. Each run has a statement
//! time limit and a row cap. `docs/REPORTS.md` documents the format.

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::attribution::ATTRIBUTION_SQL;
use super::query::{self, Arg, Sql, Window};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Most groups one query may return before the result is cut (the caller's `limit` is lower).
const SQL_ROW_CAP: usize = 5000;
pub const MAX_LIMIT: i64 = 1000;
const MAX_FILTERS: usize = 10;
const MAX_FILTER_VALUES: usize = 100;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Range {
    /// The last `days` days up to now (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<i64>,
    /// First day, in the account's time zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<NaiveDate>,
    /// Last day, included (default today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<NaiveDate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub field: String,
    pub values: Vec<String>,
    /// Keep everything except these values.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude: bool,
    /// For `property` filters: the event property's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    /// A measure, or `group` for the first group's order.
    pub by: String,
    #[serde(default)]
    pub desc: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub source: String,
    #[serde(default)]
    pub measures: Vec<String>,
    #[serde(default)]
    pub group_by: Vec<String>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    #[serde(default)]
    pub range: Range,
    /// Also total the same length of time just before the range.
    #[serde(default)]
    pub compare: bool,
    /// For `source: metric`: the metric's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    /// For `group_by: property`: the event property to group by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Sort>,
    /// Rows to return, 1-1000 (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    Campaigns,
    Flows,
    FlowMessages,
    Messages,
    Revenue,
    Forms,
    Growth,
    Metric,
    Customers,
    Engagement,
}

#[derive(Clone, Copy)]
enum Formula {
    Raw(&'static str),
    Ratio(&'static str, &'static str),
    /// joined - unsubscribed - bounced
    Net,
    /// This group's share of the total of a raw value.
    Share(&'static str),
}

#[derive(Clone, Copy)]
pub struct Measure {
    pub key: &'static str,
    pub label: &'static str,
    /// count | rate | currency
    pub kind: &'static str,
    formula: Formula,
}

const fn m(key: &'static str, label: &'static str, kind: &'static str, formula: Formula) -> Measure {
    Measure { key, label, kind, formula }
}

use Formula::{Net, Ratio, Raw, Share};

const MESSAGE_MEASURES: &[Measure] = &[
    m("sent", "Sent", "count", Raw("sent")),
    m("delivered", "Delivered", "count", Raw("delivered")),
    m("recipients", "Recipients", "count", Raw("recipients")),
    m("opens", "Unique opens", "count", Raw("opens")),
    m("clicks", "Unique clicks", "count", Raw("clicks")),
    m("unsubscribes", "Unsubscribes", "count", Raw("unsubscribes")),
    m("bounces", "Bounces", "count", Raw("bounces")),
    m("hard_bounces", "Hard bounces", "count", Raw("hard_bounces")),
    m("complaints", "Spam complaints", "count", Raw("complaints")),
    m("conversions", "Conversions", "count", Raw("conversions")),
    m("revenue", "Revenue", "currency", Raw("revenue")),
    m("open_rate", "Open rate", "rate", Ratio("opens", "email_delivered")),
    m("click_rate", "Click rate", "rate", Ratio("clicks", "delivered")),
    m("click_to_open_rate", "Click-to-open rate", "rate", Ratio("email_clicks", "opens")),
    m("unsubscribe_rate", "Unsubscribe rate", "rate", Ratio("unsubscribes", "sent")),
    m("bounce_rate", "Bounce rate", "rate", Ratio("bounces", "email_sent")),
    m("complaint_rate", "Spam complaint rate", "rate", Ratio("complaints", "email_sent")),
    m("conversion_rate", "Conversion rate", "rate", Ratio("conversions", "recipients")),
    m("revenue_per_recipient", "Revenue per recipient", "currency", Ratio("revenue", "recipients")),
    m("average_order_value", "Average order value", "currency", Ratio("revenue", "conversions")),
];
const MESSAGE_RAW: &[&str] = &["sent", "email_sent", "delivered", "email_delivered", "recipients", "opens", "clicks", "email_clicks", "unsubscribes", "bounces", "hard_bounces", "complaints", "conversions", "revenue"];

const REVENUE_MEASURES: &[Measure] = &[
    m("conversions", "Conversions", "count", Raw("conversions")),
    m("revenue", "Revenue", "currency", Raw("revenue")),
    m("attributed_conversions", "Attributed conversions", "count", Raw("attributed_conversions")),
    m("attributed_revenue", "Attributed revenue", "currency", Raw("attributed_revenue")),
    m("attributed_share", "Share of revenue attributed", "rate", Ratio("attributed_revenue", "revenue")),
    m("customers", "Customers", "count", Raw("customers")),
    m("average_order_value", "Average order value", "currency", Ratio("revenue", "conversions")),
];
const REVENUE_RAW: &[&str] = &["conversions", "revenue", "attributed_conversions", "attributed_revenue", "customers"];

const FORM_MEASURES: &[Measure] = &[
    m("views", "Views", "count", Raw("views")),
    m("sign_ups", "Sign-ups", "count", Raw("sign_ups")),
    m("sign_up_rate", "Sign-up rate", "rate", Ratio("sign_ups", "views")),
];
const FORM_RAW: &[&str] = &["views", "sign_ups"];

const GROWTH_MEASURES: &[Measure] = &[
    m("new_profiles", "New profiles", "count", Raw("new_profiles")),
    m("joined", "Joined a list", "count", Raw("joined")),
    m("unsubscribed", "Unsubscribed", "count", Raw("unsubscribed")),
    m("bounced", "Hard bounced", "count", Raw("bounced")),
    m("complaints", "Marked as spam", "count", Raw("complaints")),
    m("net_change", "Net change", "count", Net),
];
const GROWTH_RAW: &[&str] = &["new_profiles", "joined", "unsubscribed", "bounced", "complaints"];

const METRIC_MEASURES: &[Measure] = &[
    m("events", "Events", "count", Raw("events")),
    m("people", "People", "count", Raw("people")),
    m("value", "Total value", "currency", Raw("value")),
    m("average_value", "Average value", "currency", Ratio("value", "events")),
];
const METRIC_RAW: &[&str] = &["events", "people", "value"];

const CUSTOMER_MEASURES: &[Measure] = &[
    m("customers", "Customers", "count", Raw("customers")),
    m("returning_customers", "Returning customers", "count", Raw("returning_customers")),
    m("repeat_purchase_rate", "Repeat purchase rate", "rate", Ratio("returning_customers", "customers")),
    m("orders", "Orders", "count", Raw("orders")),
    m("revenue", "Revenue", "currency", Raw("revenue")),
    m("revenue_per_customer", "Revenue per customer", "currency", Ratio("revenue", "customers")),
    m("orders_per_customer", "Orders per customer", "number", Ratio("orders", "customers")),
];
const CUSTOMER_RAW: &[&str] = &["customers", "returning_customers", "orders", "revenue"];

const ENGAGEMENT_MEASURES: &[Measure] = &[m("profiles", "Profiles", "count", Raw("profiles")), m("share", "Share", "rate", Share("profiles"))];
const ENGAGEMENT_RAW: &[&str] = &["profiles"];

const TIME_GROUPS: &[&str] = &["day", "week", "month", "hour", "weekday"];

pub struct SourceInfo {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub groups: &'static [&'static str],
    pub filters: &'static [&'static str],
    pub measures: &'static [Measure],
    pub default_measures: &'static [&'static str],
    raw: &'static [&'static str],
    source: Source,
}

const MSG_GROUPS: &[&str] = &["day", "week", "month", "hour", "weekday", "channel", "campaign", "flow", "message", "source", "country", "list", "segment"];

pub const SOURCES: &[SourceInfo] = &[
    SourceInfo {
        key: "campaigns",
        label: "Campaigns",
        description: "Campaign messages sent in the range, and the conversions in the range credited to them.",
        groups: &["day", "week", "month", "hour", "weekday", "channel", "campaign", "country", "list", "segment"],
        filters: &["channel", "campaign", "country", "list", "segment"],
        measures: MESSAGE_MEASURES,
        default_measures: &["sent", "open_rate", "click_rate", "conversions", "revenue"],
        raw: MESSAGE_RAW,
        source: Source::Campaigns,
    },
    SourceInfo {
        key: "flows",
        label: "Flows",
        description: "Flow messages sent in the range, and the conversions in the range credited to them.",
        groups: &["day", "week", "month", "hour", "weekday", "channel", "flow", "message", "country"],
        filters: &["channel", "flow", "country"],
        measures: MESSAGE_MEASURES,
        default_measures: &["sent", "open_rate", "click_rate", "conversions", "revenue"],
        raw: MESSAGE_RAW,
        source: Source::Flows,
    },
    SourceInfo {
        key: "flow_messages",
        label: "Flow messages",
        description: "Each email and text in your flows, step by step.",
        groups: &["message", "flow", "channel", "day", "week", "month"],
        filters: &["channel", "flow", "country"],
        measures: MESSAGE_MEASURES,
        default_measures: &["sent", "open_rate", "click_rate", "conversions", "revenue", "revenue_per_recipient"],
        raw: MESSAGE_RAW,
        source: Source::FlowMessages,
    },
    SourceInfo {
        key: "messages",
        label: "All messages",
        description: "Every campaign and flow message sent in the range.",
        groups: MSG_GROUPS,
        filters: &["channel", "campaign", "flow", "country", "list", "segment"],
        measures: MESSAGE_MEASURES,
        default_measures: &["sent", "open_rate", "click_rate", "revenue"],
        raw: MESSAGE_RAW,
        source: Source::Messages,
    },
    SourceInfo {
        key: "revenue",
        label: "Revenue",
        description: "Every conversion in the range, with the message credited for it, if any.",
        groups: &["day", "week", "month", "hour", "weekday", "channel", "source", "campaign", "flow", "message", "country"],
        filters: &["channel", "source", "campaign", "flow", "country"],
        measures: REVENUE_MEASURES,
        default_measures: &["conversions", "revenue", "attributed_revenue", "attributed_share"],
        raw: REVENUE_RAW,
        source: Source::Revenue,
    },
    SourceInfo {
        key: "forms",
        label: "Sign-up forms",
        description: "Form views and sign-ups in the range.",
        groups: &["day", "week", "month", "hour", "weekday", "form"],
        filters: &["form"],
        measures: FORM_MEASURES,
        default_measures: &["views", "sign_ups", "sign_up_rate"],
        raw: FORM_RAW,
        source: Source::Forms,
    },
    SourceInfo {
        key: "list_growth",
        label: "List growth",
        description: "People joining lists, unsubscribing, hard bouncing and marking email as spam.",
        groups: &["day", "week", "month", "list"],
        filters: &["list"],
        measures: GROWTH_MEASURES,
        default_measures: &["joined", "unsubscribed", "bounced", "net_change"],
        raw: GROWTH_RAW,
        source: Source::Growth,
    },
    SourceInfo {
        key: "metric",
        label: "A metric",
        description: "Events of one metric (set `metric`), such as Viewed Product or Placed Order.",
        groups: &["day", "week", "month", "hour", "weekday", "country", "property", "campaign", "flow"],
        filters: &["country", "property"],
        measures: METRIC_MEASURES,
        default_measures: &["events", "people", "value"],
        raw: METRIC_RAW,
        source: Source::Metric,
    },
    SourceInfo {
        key: "customers",
        label: "Customers",
        description: "People who converted in the range, and how many had bought before.",
        groups: &["day", "week", "month"],
        filters: &["country"],
        measures: CUSTOMER_MEASURES,
        default_measures: &["customers", "returning_customers", "repeat_purchase_rate", "revenue_per_customer"],
        raw: CUSTOMER_RAW,
        source: Source::Customers,
    },
    SourceInfo {
        key: "engagement",
        label: "Engagement tiers",
        description: "People you can email, by when they last opened or clicked (as of now; the date range doesn't apply).",
        groups: &["tier"],
        filters: &["list", "segment"],
        measures: ENGAGEMENT_MEASURES,
        default_measures: &["profiles", "share"],
        raw: ENGAGEMENT_RAW,
        source: Source::Engagement,
    },
];

pub const GROUP_LABELS: &[(&str, &str)] = &[
    ("day", "Day"),
    ("week", "Week"),
    ("month", "Month"),
    ("hour", "Hour of day"),
    ("weekday", "Day of week"),
    ("channel", "Channel"),
    ("campaign", "Campaign"),
    ("flow", "Flow"),
    ("message", "Message"),
    ("source", "Source"),
    ("country", "Country"),
    ("list", "List"),
    ("segment", "Segment"),
    ("form", "Form"),
    ("property", "Property"),
    ("tier", "Engagement tier"),
];

pub const ENGAGEMENT_TIERS: &[(&str, &str)] = &[
    ("active_30", "Active in the last 30 days"),
    ("active_90", "Active 31-90 days ago"),
    ("active_180", "Active 91-180 days ago"),
    ("at_risk", "At risk: last active 6-12 months ago"),
    ("unengaged", "Unengaged: emailed this year, never opened or clicked"),
    ("not_emailed", "Not emailed in the last year"),
];

pub fn group_label(g: &str) -> String {
    GROUP_LABELS.iter().find(|(k, _)| *k == g).map(|(_, l)| l.to_string()).unwrap_or_else(|| g.to_string())
}

fn info(source: &str) -> AppResult<&'static SourceInfo> {
    SOURCES.iter().find(|s| s.key == source).ok_or_else(|| {
        AppError::bad(format!("source must be one of: {}", SOURCES.iter().map(|s| s.key).collect::<Vec<_>>().join(", ")))
    })
}

/// The catalog of sources, groups, filters and measures, for builders and MCP clients.
pub fn catalog() -> Value {
    json!({
        "sources": SOURCES.iter().map(|s| json!({
            "key": s.key, "label": s.label, "description": s.description,
            "group_by": s.groups.iter().map(|g| json!({"key": g, "label": group_label(g)})).collect::<Vec<_>>(),
            "filters": s.filters,
            "measures": s.measures.iter().map(|m| json!({"key": m.key, "label": m.label, "kind": m.kind})).collect::<Vec<_>>(),
            "default_measures": s.default_measures,
        })).collect::<Vec<_>>(),
    })
}

/// Check a spec against the whitelists and fill in defaults. Returns the cleaned spec.
pub fn validate(mut spec: Spec) -> AppResult<Spec> {
    let src = info(&spec.source)?;
    if spec.measures.is_empty() {
        spec.measures = src.default_measures.iter().map(|s| s.to_string()).collect();
    }
    if spec.measures.len() > 20 {
        return Err(AppError::bad("pick at most 20 measures"));
    }
    let mut seen = Vec::new();
    for mname in &spec.measures {
        if !src.measures.iter().any(|x| x.key == mname) {
            return Err(AppError::bad(format!(
                "{} reports don't have the measure {mname:?}; use: {}",
                src.key,
                src.measures.iter().map(|x| x.key).collect::<Vec<_>>().join(", ")
            )));
        }
        if !seen.contains(mname) {
            seen.push(mname.clone());
        }
    }
    spec.measures = seen;
    if src.source == Source::Engagement && spec.group_by.is_empty() {
        spec.group_by = vec!["tier".into()];
    }
    if spec.group_by.len() > 2 {
        return Err(AppError::bad("group by at most 2 things"));
    }
    for g in &spec.group_by {
        if !src.groups.contains(&g.as_str()) {
            return Err(AppError::bad(format!("{} reports can't be grouped by {g:?}; use: {}", src.key, src.groups.join(", "))));
        }
    }
    if spec.group_by.len() == 2 && spec.group_by[0] == spec.group_by[1] {
        return Err(AppError::bad("pick two different groups"));
    }
    let time_groups = spec.group_by.iter().filter(|g| ["day", "week", "month"].contains(&g.as_str())).count();
    if time_groups > 1 {
        return Err(AppError::bad("group by one of day, week or month"));
    }
    if spec.group_by.iter().any(|g| g == "property") {
        let key = spec.property.as_deref().map(str::trim).unwrap_or("");
        if key.is_empty() || key.len() > 100 {
            return Err(AppError::bad("set `property` (1-100 characters) to group by an event property"));
        }
        spec.property = Some(key.to_string());
    } else {
        spec.property = None;
    }
    if src.source == Source::Metric {
        let name = spec.metric.as_deref().map(str::trim).unwrap_or("");
        if name.is_empty() || name.len() > 200 {
            return Err(AppError::bad("set `metric` to the metric's name for a metric report"));
        }
        spec.metric = Some(name.to_string());
    } else {
        spec.metric = None;
    }
    if spec.filters.len() > MAX_FILTERS {
        return Err(AppError::bad(format!("use at most {MAX_FILTERS} filters")));
    }
    for f in &mut spec.filters {
        if !src.filters.contains(&f.field.as_str()) {
            return Err(AppError::bad(format!("{} reports can't be filtered by {:?}; use: {}", src.key, f.field, src.filters.join(", "))));
        }
        if f.values.is_empty() || f.values.len() > MAX_FILTER_VALUES {
            return Err(AppError::bad(format!("each filter needs 1-{MAX_FILTER_VALUES} values")));
        }
        for v in &mut f.values {
            *v = v.trim().to_string();
            if v.len() > 200 {
                return Err(AppError::bad("filter values can be at most 200 characters"));
            }
        }
        match f.field.as_str() {
            "campaign" | "flow" | "list" | "segment" | "form" => {
                for v in &f.values {
                    v.parse::<Uuid>().map_err(|_| AppError::bad(format!("{} filter values must be ids", f.field)))?;
                }
            }
            "channel" => {
                if f.values.iter().any(|v| v != "email" && v != "sms") {
                    return Err(AppError::bad("channel filter values are email and sms"));
                }
            }
            "source" => {
                if f.values.iter().any(|v| !["campaign", "flow", "none"].contains(&v.as_str())) {
                    return Err(AppError::bad("source filter values are campaign, flow and none"));
                }
            }
            "property" => {
                let key = f.key.as_deref().map(str::trim).unwrap_or("");
                if key.is_empty() || key.len() > 100 {
                    return Err(AppError::bad("a property filter needs `key`, the property's name"));
                }
                f.key = Some(key.to_string());
            }
            _ => {}
        }
        if f.field != "property" {
            f.key = None;
        }
    }
    if src.source == Source::Growth
        && (spec.group_by.iter().any(|g| g == "list") || spec.filters.iter().any(|f| f.field == "list"))
        && spec.measures.iter().any(|m| m == "new_profiles")
    {
        return Err(AppError::bad("new_profiles isn't counted per list; remove it or the list grouping"));
    }
    if let Some(s) = &spec.sort {
        if s.by != "group" && !spec.measures.contains(&s.by) {
            return Err(AppError::bad("sort.by must be `group` or one of the report's measures"));
        }
    }
    if let Some(l) = spec.limit {
        if !(1..=MAX_LIMIT).contains(&l) {
            return Err(AppError::bad(format!("limit must be 1-{MAX_LIMIT}")));
        }
    }
    if src.source == Source::Engagement {
        spec.compare = false;
    }
    if let (Some(s), Some(e)) = (spec.range.start, spec.range.end) {
        if e < s {
            return Err(AppError::bad("range.end must be on or after range.start"));
        }
    }
    if spec.range.start.is_some() {
        spec.range.days = None;
    }
    Ok(spec)
}

/// One query's raw rows: group keys, labels and raw numbers.
struct RawRow {
    keys: Vec<String>,
    labels: Vec<Option<String>>,
    raw: HashMap<&'static str, f64>,
}

/// Placeholders shared by one query.
struct Ctx<'a> {
    spec: &'a Spec,
    src: &'static SourceInfo,
    tz: String,
    metrics: Vec<String>,
}

fn time_key(group: &str, col: &str, tz: &str) -> Option<String> {
    Some(match group {
        "day" | "week" | "month" => format!("to_char(date_trunc('{group}', {col} AT TIME ZONE {tz}), 'YYYY-MM-DD')"),
        "hour" => format!("to_char({col} AT TIME ZONE {tz}, 'HH24')"),
        "weekday" => format!("extract(isodow FROM {col} AT TIME ZONE {tz})::int::text"),
        _ => return None,
    })
}

/// SQL for a group's label, given the key column `k`. None: the key is its own label.
fn label_sql(group: &str, k: &str) -> Option<String> {
    Some(match group {
        "campaign" => format!("(SELECT c.name FROM campaigns c WHERE c.account_id = $1 AND c.id = NULLIF({k}, '')::uuid)"),
        "flow" => format!("(SELECT f.name FROM flows f WHERE f.account_id = $1 AND f.id = NULLIF({k}, '')::uuid)"),
        "list" => format!("(SELECT l.name FROM lists l WHERE l.account_id = $1 AND l.id = NULLIF({k}, '')::uuid)"),
        "segment" => format!("(SELECT s.name FROM segments s WHERE s.account_id = $1 AND s.id = NULLIF({k}, '')::uuid)"),
        "form" => format!("(SELECT f.name FROM forms f WHERE f.account_id = $1 AND f.id = NULLIF({k}, '')::uuid)"),
        // Flow messages are keyed `<flow id>/<step id>`; campaign messages by the campaign id.
        "message" => format!(
            "CASE WHEN length({k}) > 37 THEN (SELECT f.name || ' · ' || COALESCE((SELECT n->>'name' FROM jsonb_array_elements(f.definition->'nodes') n
                                                     WHERE n->>'id' = substr({k}, 38) LIMIT 1), substr({k}, 38))
                                            FROM flows f WHERE f.account_id = $1 AND f.id = left({k}, 36)::uuid)
                  WHEN {k} <> '' THEN (SELECT c.name FROM campaigns c WHERE c.account_id = $1 AND c.id = {k}::uuid) END"
        ),
        _ => return None,
    })
}

fn static_label(group: &str, key: &str) -> Option<String> {
    let s = match (group, key) {
        (_, "") => "(none)".to_string(),
        ("channel", "email") => "Email".into(),
        ("channel", "sms") => "SMS".into(),
        ("source", "campaign") => "Campaigns".into(),
        ("source", "flow") => "Flows".into(),
        ("source", "other") => "Other messages".into(),
        ("source", "none") => "Not attributed".into(),
        ("hour", h) => format!("{h}:00"),
        ("weekday", d) => ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].get(d.parse::<usize>().ok()?.checked_sub(1)?)?.to_string(),
        ("tier", t) => ENGAGEMENT_TIERS.iter().find(|(k, _)| *k == t)?.1.to_string(),
        _ => return None,
    };
    Some(s)
}

/// Placeholder for a filter's values.
fn arr(q: &mut Sql, f: &Filter) -> String {
    match f.field.as_str() {
        "campaign" | "flow" | "list" | "segment" | "form" => q.bind(Arg::UuidArr(f.values.iter().filter_map(|v| v.parse().ok()).collect())),
        _ => q.bind(Arg::TextArr(f.values.clone())),
    }
}

fn cond(f: &Filter, expr: &str, ph: &str) -> String {
    if f.exclude {
        format!(" AND ({expr} IS NULL OR NOT ({expr} = ANY({ph})))")
    } else {
        format!(" AND {expr} = ANY({ph})")
    }
}

/// Which side of a messages report: the messages themselves (`m`) or their conversions (`x`).
#[derive(Clone, Copy, PartialEq)]
enum Side {
    Msg,
    Rev,
}

fn uses(spec: &Spec, names: &[&str]) -> bool {
    spec.group_by.iter().any(|g| names.contains(&g.as_str())) || spec.filters.iter().any(|f| names.contains(&f.field.as_str()))
}

/// Key expression for a group on one side of a messages report.
fn message_key(group: &str, side: Side, tz: &str) -> String {
    let a = if side == Side::Msg { "m" } else { "x" };
    let time_col = if side == Side::Msg { "m.created_at" } else { "x.occurred_at" };
    if let Some(t) = time_key(group, time_col, tz) {
        return t;
    }
    match group {
        "channel" => format!("{a}.channel"),
        "campaign" => format!("{a}.campaign_id::text"),
        "flow" => format!("{a}.flow_id::text"),
        "message" => format!("COALESCE({a}.flow_id::text || '/' || {a}.flow_node_id, {a}.campaign_id::text)"),
        "source" => format!("CASE WHEN {a}.campaign_id IS NOT NULL THEN 'campaign' WHEN {a}.flow_id IS NOT NULL THEN 'flow' ELSE 'other' END"),
        "country" => "p.properties->>'country'".into(),
        "list" | "segment" => "aud.ref".into(),
        _ => "''".into(),
    }
}

fn message_filter(q: &mut Sql, f: &Filter, side: Side) -> String {
    let a = if side == Side::Msg { "m" } else { "x" };
    let ph = arr(q, f);
    match f.field.as_str() {
        "channel" => cond(f, &format!("{a}.channel"), &ph),
        "campaign" => cond(f, &format!("{a}.campaign_id"), &ph),
        "flow" => cond(f, &format!("{a}.flow_id"), &ph),
        "country" => cond(f, "p.properties->>'country'", &ph),
        "list" | "segment" => {
            let kind = if f.field == "list" { "list" } else { "segment" };
            let has = format!(
                "EXISTS (SELECT 1 FROM jsonb_array_elements(COALESCE(c.audience->'include', '[]'::jsonb)) r WHERE r->>'type' = '{kind}' AND (r->>'id')::uuid = ANY({ph}))"
            );
            if f.exclude { format!(" AND NOT {has}") } else { format!(" AND {has}") }
        }
        _ => String::new(),
    }
}

/// Messages sent in the window, and conversions in the window credited to them.
fn messages_sql(q: &mut Sql, c: &Ctx, groups: &[String], w: Window) -> Vec<String> {
    let (s, e) = (q.bind(Arg::Ts(w.start)), q.bind(Arg::Ts(w.end)));
    let n = groups.len().max(1);
    let keys = |side: Side| -> String {
        if groups.is_empty() {
            return "''::text AS g0".into();
        }
        groups.iter().enumerate().map(|(i, g)| format!("COALESCE({}, '') AS g{i}", message_key(g, side, &c.tz))).collect::<Vec<_>>().join(", ")
    };
    let by = (1..=n).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    let (scope_m, scope_x) = match c.src.source {
        Source::Campaigns => (" AND m.campaign_id IS NOT NULL", " AND x.campaign_id IS NOT NULL"),
        Source::Flows | Source::FlowMessages => (" AND m.flow_id IS NOT NULL", " AND x.flow_id IS NOT NULL"),
        _ => ("", ""),
    };
    let need_campaign = uses(c.spec, &["list", "segment"]);
    let need_profile = uses(c.spec, &["country"]);
    let aud = groups.iter().find(|g| *g == "list" || *g == "segment").map(|g| {
        format!(" CROSS JOIN LATERAL (SELECT r->>'id' AS ref FROM jsonb_array_elements(COALESCE(c.audience->'include', '[]'::jsonb)) r WHERE r->>'type' = '{g}') aud")
    });
    let joins = |side: Side| {
        let a = if side == Side::Msg { "m" } else { "x" };
        let mut j = String::new();
        if need_campaign {
            j.push_str(&format!(" LEFT JOIN campaigns c ON c.id = {a}.campaign_id"));
        }
        if need_profile {
            j.push_str(&format!(" LEFT JOIN profiles p ON p.id = {a}.profile_id"));
        }
        if let Some(a) = &aud {
            j.push_str(a);
        }
        j
    };
    let mut fm = String::new();
    let mut fx = String::new();
    for f in &c.spec.filters {
        fm.push_str(&message_filter(q, f, Side::Msg));
        fx.push_str(&message_filter(q, f, Side::Rev));
    }
    let sent = "m.status NOT IN ('queued','sending','failed','skipped')";
    let delivered = "(m.status IN ('sent','delivered') OR m.opened_at IS NOT NULL)";
    let gcols = (0..n).map(|i| format!("g{i}")).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "WITH msg AS (
            SELECT {km},
                   count(*) FILTER (WHERE {sent}) AS sent,
                   count(*) FILTER (WHERE m.channel = 'email' AND {sent}) AS email_sent,
                   count(*) FILTER (WHERE {delivered}) AS delivered,
                   count(*) FILTER (WHERE m.channel = 'email' AND {delivered}) AS email_delivered,
                   count(DISTINCT m.profile_id) FILTER (WHERE {sent}) AS recipients,
                   count(m.opened_at) AS opens,
                   count(m.clicked_at) AS clicks,
                   count(m.clicked_at) FILTER (WHERE m.channel = 'email') AS email_clicks,
                   count(m.unsubscribed_at) AS unsubscribes,
                   count(*) FILTER (WHERE m.status = 'bounced') AS bounces,
                   count(*) FILTER (WHERE m.status = 'bounced' AND m.bounce_type = 'Permanent') AS hard_bounces,
                   count(*) FILTER (WHERE m.status = 'complained') AS complaints
            FROM messages m{jm}
            WHERE m.account_id = $1 AND NOT m.is_test AND m.status <> 'skipped' AND m.created_at >= {s} AND m.created_at < {e}{scope_m}{fm}
            GROUP BY {by}),
         rev AS (
            SELECT {kx}, count(*) AS conversions, COALESCE(sum(x.value), 0) AS revenue
            FROM ({ATTRIBUTION_SQL} AND e.occurred_at >= {s} AND e.occurred_at < {e}) x{jx}
            WHERE x.message_id IS NOT NULL{scope_x}{fx}
            GROUP BY {by})
         SELECT {gcols}, {labels},
                COALESCE(msg.sent, 0)::float8 AS sent, COALESCE(msg.email_sent, 0)::float8 AS email_sent,
                COALESCE(msg.delivered, 0)::float8 AS delivered, COALESCE(msg.email_delivered, 0)::float8 AS email_delivered,
                COALESCE(msg.recipients, 0)::float8 AS recipients, COALESCE(msg.opens, 0)::float8 AS opens,
                COALESCE(msg.clicks, 0)::float8 AS clicks, COALESCE(msg.email_clicks, 0)::float8 AS email_clicks,
                COALESCE(msg.unsubscribes, 0)::float8 AS unsubscribes, COALESCE(msg.bounces, 0)::float8 AS bounces,
                COALESCE(msg.hard_bounces, 0)::float8 AS hard_bounces, COALESCE(msg.complaints, 0)::float8 AS complaints,
                COALESCE(rev.conversions, 0)::float8 AS conversions, COALESCE(rev.revenue, 0)::float8 AS revenue
         FROM msg FULL JOIN rev USING ({gcols})",
        km = keys(Side::Msg),
        kx = keys(Side::Rev),
        jm = joins(Side::Msg),
        jx = joins(Side::Rev),
        labels = labels(groups),
    );
    q.push(&sql);
    MESSAGE_RAW.iter().map(|s| s.to_string()).collect()
}

fn labels(groups: &[String]) -> String {
    if groups.is_empty() {
        return "NULL::text AS l0".into();
    }
    groups
        .iter()
        .enumerate()
        .map(|(i, g)| format!("{} AS l{i}", label_sql(g, &format!("g{i}")).unwrap_or_else(|| "NULL::text".into())))
        .collect::<Vec<_>>()
        .join(", ")
}

fn keys_simple(groups: &[String], key: impl Fn(&str) -> String) -> (String, String) {
    if groups.is_empty() {
        return ("''::text AS g0".into(), "1".into());
    }
    let k = groups.iter().enumerate().map(|(i, g)| format!("COALESCE({}, '') AS g{i}", key(g))).collect::<Vec<_>>().join(", ");
    let by = (1..=groups.len()).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    (k, by)
}

/// Every conversion in the window, attributed or not.
fn revenue_sql(q: &mut Sql, c: &Ctx, groups: &[String], w: Window) {
    let (s, e) = (q.bind(Arg::Ts(w.start)), q.bind(Arg::Ts(w.end)));
    let (k, by) = keys_simple(groups, |g| {
        if g == "source" {
            "CASE WHEN x.campaign_id IS NOT NULL THEN 'campaign' WHEN x.flow_id IS NOT NULL THEN 'flow' WHEN x.message_id IS NOT NULL THEN 'other' ELSE 'none' END".into()
        } else {
            message_key(g, Side::Rev, &c.tz)
        }
    });
    let join = if uses(c.spec, &["country"]) { " LEFT JOIN profiles p ON p.id = x.profile_id" } else { "" };
    let mut fx = String::new();
    for f in &c.spec.filters {
        if f.field == "source" {
            let ph = arr(q, f);
            let expr = "CASE WHEN x.campaign_id IS NOT NULL THEN 'campaign' WHEN x.flow_id IS NOT NULL THEN 'flow' ELSE 'none' END";
            fx.push_str(&cond(f, expr, &ph));
        } else {
            fx.push_str(&message_filter(q, f, Side::Rev));
        }
    }
    let sql = format!(
        "SELECT {k}, count(*)::float8 AS conversions, COALESCE(sum(x.value), 0)::float8 AS revenue,
                count(*) FILTER (WHERE x.message_id IS NOT NULL)::float8 AS attributed_conversions,
                COALESCE(sum(x.value) FILTER (WHERE x.message_id IS NOT NULL), 0)::float8 AS attributed_revenue,
                count(DISTINCT x.profile_id)::float8 AS customers
         FROM ({ATTRIBUTION_SQL} AND e.occurred_at >= {s} AND e.occurred_at < {e}) x{join}
         WHERE true{fx}
         GROUP BY {by}"
    );
    q.push(&sql);
}

fn forms_sql(q: &mut Sql, c: &Ctx, groups: &[String], w: Window) {
    let (s, e) = (q.bind(Arg::Ts(w.start)), q.bind(Arg::Ts(w.end)));
    let (k, by) = keys_simple(groups, |g| time_key(g, "h.hour", &c.tz).unwrap_or_else(|| if g == "form" { "h.form_id::text".into() } else { "''".into() }));
    let mut f_sql = String::new();
    for f in &c.spec.filters {
        let ph = arr(q, f);
        f_sql.push_str(&cond(f, "h.form_id", &ph));
    }
    q.push(&format!(
        "SELECT {k}, COALESCE(sum(h.views), 0)::float8 AS views, COALESCE(sum(h.submits), 0)::float8 AS sign_ups
         FROM form_stats_hourly h WHERE h.account_id = $1 AND h.hour >= {s} AND h.hour < {e}{f_sql} GROUP BY {by}"
    ));
}

fn growth_sql(q: &mut Sql, c: &Ctx, groups: &[String], w: Window) {
    let (s, e) = (q.bind(Arg::Ts(w.start)), q.bind(Arg::Ts(w.end)));
    let per_list = uses(c.spec, &["list"]);
    let list_filter = c.spec.filters.iter().find(|f| f.field == "list").map(|f| (f.exclude, arr(q, f)));
    let (k, by) = keys_simple(groups, |g| time_key(g, "s.at", &c.tz).unwrap_or_else(|| if g == "list" { "s.list_id::text".into() } else { "''".into() }));
    let list_cond = |col: &str| match &list_filter {
        Some((false, ph)) => format!(" AND {col} = ANY({ph})"),
        Some((true, ph)) => format!(" AND NOT ({col} = ANY({ph}))"),
        None => String::new(),
    };
    let new_profiles = if per_list {
        String::new()
    } else {
        format!("SELECT p.created_at AS at, NULL::uuid AS list_id, p.id AS profile_id, 'new' AS kind FROM profiles p WHERE p.account_id = $1 AND p.created_at >= {s} AND p.created_at < {e} UNION ALL ")
    };
    // Per list, an unsubscribe or bounce counts for every list the person is on, plus the list they
    // left from the preference page. Account-wide, only full unsubscribes count.
    let (metrics, list_expr, list_join) = if per_list {
        (
            "ARRAY['Unsubscribed','Unsubscribed from SMS','Unsubscribed from List','Bounced Email','Marked Email as Spam']",
            "lx.list_id",
            " CROSS JOIN LATERAL (SELECT l.id AS list_id FROM lists l WHERE mt.name = 'Unsubscribed from List' AND l.account_id = $1 AND l.id::text = e.properties->>'list_id'
                                  UNION SELECT lm.list_id FROM list_members lm WHERE mt.name <> 'Unsubscribed from List' AND lm.profile_id = e.profile_id) lx",
        )
    } else {
        ("ARRAY['Unsubscribed','Unsubscribed from SMS','Bounced Email','Marked Email as Spam']", "NULL::uuid", "")
    };
    q.push(&format!(
        "SELECT {k},
                count(DISTINCT s.profile_id) FILTER (WHERE s.kind = 'new')::float8 AS new_profiles,
                count(DISTINCT s.profile_id) FILTER (WHERE s.kind = 'joined')::float8 AS joined,
                count(DISTINCT s.profile_id) FILTER (WHERE s.kind = 'unsubscribed')::float8 AS unsubscribed,
                count(DISTINCT s.profile_id) FILTER (WHERE s.kind = 'bounced')::float8 AS bounced,
                count(DISTINCT s.profile_id) FILTER (WHERE s.kind = 'complained')::float8 AS complaints
         FROM ({new_profiles}
               SELECT lm.added_at AS at, lm.list_id, lm.profile_id, 'joined' AS kind
               FROM list_members lm JOIN lists l ON l.id = lm.list_id
               WHERE l.account_id = $1 AND lm.added_at >= {s} AND lm.added_at < {e}{lc1}
               UNION ALL
               SELECT e.occurred_at, {list_expr}, e.profile_id,
                      CASE mt.name WHEN 'Bounced Email' THEN 'bounced' WHEN 'Marked Email as Spam' THEN 'complained' ELSE 'unsubscribed' END
               FROM events e JOIN metrics mt ON mt.id = e.metric_id{list_join}
               WHERE e.account_id = $1 AND mt.account_id = $1 AND mt.name = ANY({metrics}) AND e.occurred_at >= {s} AND e.occurred_at < {e}
                 AND (mt.name <> 'Bounced Email' OR e.properties->>'bounce_type' = 'Permanent'){lc2}) s
         GROUP BY {by}",
        lc1 = list_cond("lm.list_id"),
        lc2 = if per_list { list_cond("lx.list_id") } else { String::new() },
    ));
}

fn metric_sql(q: &mut Sql, c: &Ctx, groups: &[String], w: Window) {
    let (s, e) = (q.bind(Arg::Ts(w.start)), q.bind(Arg::Ts(w.end)));
    let metric = q.bind(Arg::Text(c.spec.metric.clone().unwrap_or_default()));
    let prop = c.spec.property.clone().map(|p| q.bind(Arg::Text(p)));
    let (k, by) = keys_simple(groups, |g| {
        time_key(g, "e.occurred_at", &c.tz).unwrap_or_else(|| match g {
            "country" => "p.properties->>'country'".into(),
            "property" => format!("e.properties->>{}", prop.clone().unwrap_or_else(|| "''".into())),
            "campaign" => "mm.campaign_id::text".into(),
            "flow" => "mm.flow_id::text".into(),
            _ => "''".into(),
        })
    });
    let mut joins = String::new();
    if uses(c.spec, &["country"]) {
        joins.push_str(" LEFT JOIN profiles p ON p.id = e.profile_id");
    }
    if uses(c.spec, &["campaign", "flow"]) {
        joins.push_str(" LEFT JOIN messages mm ON mm.id = e.message_id");
    }
    let mut fs = String::new();
    for f in &c.spec.filters {
        let ph = arr(q, f);
        match f.field.as_str() {
            "country" => fs.push_str(&cond(f, "p.properties->>'country'", &ph)),
            "property" => {
                let key = q.bind(Arg::Text(f.key.clone().unwrap_or_default()));
                fs.push_str(&cond(f, &format!("e.properties->>{key}"), &ph));
            }
            _ => {}
        }
    }
    q.push(&format!(
        "SELECT {k}, count(*)::float8 AS events, count(DISTINCT e.profile_id)::float8 AS people, COALESCE(sum(e.value), 0)::float8 AS value
         FROM events e{joins}
         WHERE e.account_id = $1 AND e.metric_id = (SELECT id FROM metrics WHERE account_id = $1 AND name = {metric})
           AND e.occurred_at >= {s} AND e.occurred_at < {e}{fs}
         GROUP BY {by}"
    ));
}

fn customers_sql(q: &mut Sql, c: &Ctx, groups: &[String], w: Window) {
    let (s, e) = (q.bind(Arg::Ts(w.start)), q.bind(Arg::Ts(w.end)));
    let metrics = q.bind(Arg::TextArr(c.metrics.clone()));
    let (k, by) = keys_simple(groups, |g| time_key(g, "e.occurred_at", &c.tz).unwrap_or_else(|| "''".into()));
    let mut joins = String::new();
    let mut fs = String::new();
    for f in &c.spec.filters {
        if f.field == "country" {
            joins = " LEFT JOIN profiles p ON p.id = e.profile_id".into();
            let ph = arr(q, f);
            fs.push_str(&cond(f, "p.properties->>'country'", &ph));
        }
    }
    q.push(&format!(
        "WITH mids AS (SELECT array_agg(id) AS ids FROM metrics WHERE account_id = $1 AND name = ANY({metrics}))
         SELECT {k},
                count(DISTINCT e.profile_id)::float8 AS customers,
                count(DISTINCT e.profile_id) FILTER (WHERE EXISTS (
                    SELECT 1 FROM events b WHERE b.profile_id = e.profile_id AND b.metric_id = ANY(mids.ids) AND b.occurred_at < e.occurred_at))::float8 AS returning_customers,
                count(*)::float8 AS orders, COALESCE(sum(e.value), 0)::float8 AS revenue
         FROM mids, events e{joins}
         WHERE e.account_id = $1 AND e.metric_id = ANY(mids.ids) AND e.occurred_at >= {s} AND e.occurred_at < {e}{fs}
         GROUP BY {by}"
    ));
}

fn engagement_sql(q: &mut Sql, c: &Ctx) {
    let mut fs = String::new();
    for f in &c.spec.filters {
        let ph = arr(q, f);
        let table = if f.field == "list" { "list_members x WHERE x.list_id" } else { "segment_members x WHERE x.segment_id" };
        let has = format!("EXISTS (SELECT 1 FROM {table} = ANY({ph}) AND x.profile_id = p.id)");
        fs.push_str(&if f.exclude { format!(" AND NOT {has}") } else { format!(" AND {has}") });
    }
    q.push(&format!(
        "WITH eng AS (
            SELECT m.profile_id, max(GREATEST(m.opened_at, m.clicked_at)) AS last_at
            FROM messages m
            WHERE m.account_id = $1 AND m.channel = 'email' AND NOT m.is_test AND m.status <> 'skipped' AND m.created_at >= now() - interval '365 days'
            GROUP BY 1)
         SELECT t.tier AS g0, count(*)::float8 AS profiles
         FROM (SELECT CASE WHEN eng.profile_id IS NULL THEN 'not_emailed'
                           WHEN eng.last_at >= now() - interval '30 days' THEN 'active_30'
                           WHEN eng.last_at >= now() - interval '90 days' THEN 'active_90'
                           WHEN eng.last_at >= now() - interval '180 days' THEN 'active_180'
                           WHEN eng.last_at IS NOT NULL THEN 'at_risk'
                           ELSE 'unengaged' END AS tier
               FROM profiles p LEFT JOIN eng ON eng.profile_id = p.id
               WHERE p.account_id = $1 AND p.email IS NOT NULL AND p.email_consent = 'subscribed' AND p.email_suppressed_reason IS NULL{fs}) t
         GROUP BY 1"
    ));
}

async fn fetch(tx: &mut sqlx::Transaction<'static, sqlx::Postgres>, c: &Ctx<'_>, account_id: Uuid, groups: &[String], w: Window) -> AppResult<Vec<RawRow>> {
    let mut q = Sql::new(account_id);
    let tz_ph = q.bind(Arg::Text(c.tz.clone()));
    let c = Ctx { spec: c.spec, src: c.src, tz: tz_ph, metrics: c.metrics.clone() };
    match c.src.source {
        Source::Campaigns | Source::Flows | Source::FlowMessages | Source::Messages => {
            messages_sql(&mut q, &c, groups, w);
        }
        Source::Revenue => revenue_sql(&mut q, &c, groups, w),
        Source::Forms => forms_sql(&mut q, &c, groups, w),
        Source::Growth => growth_sql(&mut q, &c, groups, w),
        Source::Metric => metric_sql(&mut q, &c, groups, w),
        Source::Customers => customers_sql(&mut q, &c, groups, w),
        Source::Engagement => engagement_sql(&mut q, &c),
    }
    // Labels for sources whose SELECT groups directly are looked up after the cap.
    let wrapped = if matches!(c.src.source, Source::Campaigns | Source::Flows | Source::FlowMessages | Source::Messages) {
        format!("{} LIMIT {}", q.text, SQL_ROW_CAP + 1)
    } else {
        let n = groups.len().max(1);
        let cols = (0..n).map(|i| format!("r.g{i}")).collect::<Vec<_>>().join(", ");
        let labs = (0..n)
            .map(|i| groups.get(i).and_then(|g| label_sql(g, &format!("r.g{i}"))).map(|l| format!("{l} AS l{i}")).unwrap_or_else(|| format!("NULL::text AS l{i}")))
            .collect::<Vec<_>>()
            .join(", ");
        let raws = c.src.raw.iter().map(|r| format!("r.{r}")).collect::<Vec<_>>().join(", ");
        format!("SELECT {cols}, {labs}, {raws} FROM ({} LIMIT {}) r", q.text, SQL_ROW_CAP + 1)
    };
    q.text = wrapped;
    let rows = q.fetch_all(tx).await?;
    use sqlx::Row;
    let n = groups.len().max(1);
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let mut keys = Vec::with_capacity(n);
        let mut labels = Vec::with_capacity(n);
        for i in 0..n {
            keys.push(r.try_get::<Option<String>, _>(format!("g{i}").as_str()).map_err(|e| AppError::Internal(e.into()))?.unwrap_or_default());
            labels.push(r.try_get::<Option<String>, _>(format!("l{i}").as_str()).map_err(|e| AppError::Internal(e.into()))?);
        }
        let mut raw = HashMap::new();
        for name in c.src.raw {
            raw.insert(*name, r.try_get::<Option<f64>, _>(*name).map_err(|e| AppError::Internal(e.into()))?.unwrap_or(0.0));
        }
        out.push(RawRow { keys, labels, raw });
    }
    Ok(out)
}

fn compute(src: &SourceInfo, measures: &[String], raw: &HashMap<&'static str, f64>, total: Option<&HashMap<&'static str, f64>>) -> Map<String, Value> {
    let get = |k: &str| raw.get(k).copied().unwrap_or(0.0);
    let mut out = Map::new();
    for name in measures {
        let Some(mdef) = src.measures.iter().find(|x| x.key == name) else { continue };
        let v = match mdef.formula {
            Raw(k) => get(k),
            Ratio(n, d) => {
                let d = get(d);
                if d > 0.0 { get(n) / d } else { 0.0 }
            }
            Net => get("joined") - get("unsubscribed") - get("bounced"),
            Share(k) => {
                let t = total.and_then(|t| t.get(k).copied()).unwrap_or(0.0);
                if t > 0.0 { get(k) / t } else { 0.0 }
            }
        };
        let v = if mdef.kind == "count" { json!(v.round() as i64) } else { json!((v * 1e6).round() / 1e6) };
        out.insert(name.clone(), v);
    }
    out
}

fn sum_rows(rows: &[RawRow], src: &SourceInfo) -> HashMap<&'static str, f64> {
    let mut t = HashMap::new();
    for name in src.raw {
        t.insert(*name, rows.iter().map(|r| r.raw.get(name).copied().unwrap_or(0.0)).sum());
    }
    t
}

/// Every bucket key a lone time group should show, so charts have no gaps.
fn time_buckets(group: &str, w: Window, tz: chrono_tz::Tz) -> Option<Vec<String>> {
    match group {
        "hour" => Some((0..24).map(|h| format!("{h:02}")).collect()),
        "weekday" => Some((1..=7).map(|d| d.to_string()).collect()),
        "day" | "week" | "month" => {
            let first = w.start.with_timezone(&tz).date_naive();
            let last = (w.end - chrono::Duration::seconds(1)).with_timezone(&tz).date_naive();
            let trunc = |d: NaiveDate| match group {
                "week" => d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64),
                "month" => d.with_day(1).unwrap_or(d),
                _ => d,
            };
            let mut out = Vec::new();
            let mut d = trunc(first);
            while d <= last && out.len() < 800 {
                out.push(d.format("%Y-%m-%d").to_string());
                d = match group {
                    "week" => d + chrono::Duration::days(7),
                    "month" => {
                        let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
                        NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(d + chrono::Duration::days(31))
                    }
                    _ => d + chrono::Duration::days(1),
                };
            }
            Some(out)
        }
        _ => None,
    }
}

/// Run a validated spec. `limit` statements stop at the given time limit.
pub async fn run(state: &AppState, account_id: Uuid, spec: Spec, timeout: &'static str) -> AppResult<Value> {
    let spec = validate(spec)?;
    let src = info(&spec.source)?;
    let (tz_name, tz) = query::account_tz(state, account_id).await?;
    let settings = super::attribution::load(&state.db, account_id).await?;
    let window = query::resolve_range(tz, spec.range.days, spec.range.start, spec.range.end)?;
    let c = Ctx { spec: &spec, src, tz: tz_name.clone(), metrics: settings.conversion_metrics.clone() };

    let mut tx = query::timed_for(state, timeout).await?;
    let mut rows = fetch(&mut tx, &c, account_id, &spec.group_by, window).await?;
    let totals_raw = if spec.group_by.is_empty() || src.source == Source::Engagement {
        sum_rows(&rows, src)
    } else {
        let t = fetch(&mut tx, &c, account_id, &[], window).await?;
        t.into_iter().next().map(|r| r.raw).unwrap_or_else(|| sum_rows(&[], src))
    };
    let previous_raw = if spec.compare {
        let t = fetch(&mut tx, &c, account_id, &[], window.previous()).await?;
        Some(t.into_iter().next().map(|r| r.raw).unwrap_or_else(|| sum_rows(&[], src)))
    } else {
        None
    };
    tx.commit().await?;

    let truncated_sql = rows.len() > SQL_ROW_CAP;
    rows.truncate(SQL_ROW_CAP);

    // Fill gaps for a lone time group, and keep engagement tiers in their fixed order.
    if spec.group_by.len() == 1 {
        let g = spec.group_by[0].as_str();
        let all: Option<Vec<String>> = if g == "tier" { Some(ENGAGEMENT_TIERS.iter().map(|(k, _)| k.to_string()).collect()) } else { time_buckets(g, window, tz) };
        if let Some(all) = all {
            let mut by_key: HashMap<String, RawRow> = rows.into_iter().map(|r| (r.keys[0].clone(), r)).collect();
            rows = all
                .into_iter()
                .map(|k| by_key.remove(&k).unwrap_or_else(|| RawRow { keys: vec![k], labels: vec![None], raw: src.raw.iter().map(|n| (*n, 0.0)).collect() }))
                .collect();
            // Anything outside the expected buckets (shouldn't happen) is kept at the end.
            rows.extend(by_key.into_values());
        }
    }

    let is_time = |g: &str| TIME_GROUPS.contains(&g);
    let first_group_is_time = spec.group_by.first().is_some_and(|g| is_time(g) || g == "tier");
    let sort = spec.sort.clone().unwrap_or_else(|| {
        if spec.group_by.is_empty() || first_group_is_time {
            Sort { by: "group".into(), desc: false }
        } else {
            Sort { by: spec.measures[0].clone(), desc: true }
        }
    });
    let mut out_rows: Vec<(Vec<String>, Vec<String>, Map<String, Value>)> = rows
        .into_iter()
        .map(|r| {
            let labels = r
                .labels
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let g = spec.group_by.get(i).map(String::as_str).unwrap_or("");
                    l.clone().or_else(|| static_label(g, &r.keys[i])).unwrap_or_else(|| if r.keys[i].is_empty() { "(none)".into() } else { r.keys[i].clone() })
                })
                .collect();
            let vals = compute(src, &spec.measures, &r.raw, Some(&totals_raw));
            (r.keys, labels, vals)
        })
        .collect();
    if sort.by != "group" {
        let key = sort.by.clone();
        out_rows.sort_by(|a, b| {
            let (x, y) = (a.2.get(&key).and_then(Value::as_f64).unwrap_or(0.0), b.2.get(&key).and_then(Value::as_f64).unwrap_or(0.0));
            if sort.desc { y.total_cmp(&x) } else { x.total_cmp(&y) }
        });
    } else {
        // Time buckets sort by key (dates, hours and weekdays all sort as text), tiers in their fixed
        // order, everything else by name.
        let sort_key = |keys: &[String], labels: &[String]| -> Vec<String> {
            spec.group_by
                .iter()
                .enumerate()
                .map(|(i, g)| {
                    if is_time(g) {
                        keys[i].clone()
                    } else if g == "tier" {
                        format!("{:02}", ENGAGEMENT_TIERS.iter().position(|(k, _)| *k == keys[i]).unwrap_or(99))
                    } else {
                        labels[i].to_lowercase()
                    }
                })
                .collect()
        };
        out_rows.sort_by_key(|r| sort_key(&r.0, &r.1));
        if sort.desc {
            out_rows.reverse();
        }
    }
    let default_limit = if spec.group_by.iter().any(|g| is_time(g)) { MAX_LIMIT } else { 100 };
    let limit = spec.limit.unwrap_or(default_limit) as usize;
    let truncated = truncated_sql || out_rows.len() > limit;
    out_rows.truncate(limit);

    let mut columns: Vec<Value> = spec.group_by.iter().map(|g| json!({"key": g, "label": group_label(g), "kind": "group"})).collect();
    columns.extend(spec.measures.iter().filter_map(|name| src.measures.iter().find(|x| x.key == name)).map(|x| json!({"key": x.key, "label": x.label, "kind": x.kind})));
    let range = |w: Window| json!({"start": w.start, "end": w.end});
    let uses_attribution = matches!(src.source, Source::Campaigns | Source::Flows | Source::FlowMessages | Source::Messages | Source::Revenue);
    Ok(json!({
        "spec": spec,
        "timezone": tz_name,
        "range": range(window),
        "previous_range": spec.compare.then(|| range(window.previous())),
        "columns": columns,
        "rows": out_rows.into_iter().map(|(k, l, v)| json!({"keys": k, "labels": l, "values": v})).collect::<Vec<_>>(),
        "totals": compute(src, &spec.measures, &totals_raw, Some(&totals_raw)),
        "previous": previous_raw.as_ref().map(|p| compute(src, &spec.measures, p, Some(p))),
        "truncated": truncated,
        "attribution": uses_attribution.then(|| settings.describe()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(v: Value) -> AppResult<Spec> {
        validate(serde_json::from_value(v).map_err(|e| AppError::bad(e.to_string()))?)
    }

    #[test]
    fn specs_are_whitelisted() {
        let s = spec(json!({"source": "campaigns"})).unwrap();
        assert_eq!(s.measures, vec!["sent", "open_rate", "click_rate", "conversions", "revenue"]);
        assert!(spec(json!({"source": "users; drop table"})).is_err());
        assert!(spec(json!({"source": "campaigns", "measures": ["sent; --"]})).is_err());
        assert!(spec(json!({"source": "campaigns", "group_by": ["flow"]})).is_err(), "campaigns have no flow");
        assert!(spec(json!({"source": "campaigns", "group_by": ["day", "week"]})).is_err());
        assert!(spec(json!({"source": "campaigns", "group_by": ["day", "campaign", "channel"]})).is_err());
        assert!(spec(json!({"source": "campaigns", "filters": [{"field": "campaign", "values": ["x"]}]})).is_err());
        assert!(spec(json!({"source": "campaigns", "filters": [{"field": "channel", "values": ["fax"]}]})).is_err());
        assert!(spec(json!({"source": "metric"})).is_err(), "metric reports need a metric");
        assert!(spec(json!({"source": "metric", "metric": "Viewed Product", "group_by": ["property"]})).is_err(), "needs property");
        assert!(spec(json!({"source": "list_growth", "measures": ["new_profiles"], "group_by": ["list"]})).is_err());
        assert!(spec(json!({"source": "campaigns", "limit": 5000})).is_err());
        assert!(spec(json!({"source": "campaigns", "sort": {"by": "revenue"}, "measures": ["sent"]})).is_err());
        assert!(spec(json!({"source": "campaigns", "unknown": 1})).is_err());
        let e = spec(json!({"source": "engagement", "compare": true})).unwrap();
        assert_eq!(e.group_by, vec!["tier"]);
        assert!(!e.compare);
    }

    #[test]
    fn buckets_cover_the_range() {
        let tz = chrono_tz::America::Los_Angeles;
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let w = query::resolve_range(tz, None, Some(d("2026-10-30")), Some(d("2026-11-02"))).unwrap();
        assert_eq!(time_buckets("day", w, tz).unwrap(), vec!["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"]);
        assert_eq!(time_buckets("week", w, tz).unwrap(), vec!["2026-10-26", "2026-11-02"]);
        assert_eq!(time_buckets("month", w, tz).unwrap(), vec!["2026-10-01", "2026-11-01"]);
        assert_eq!(static_label("weekday", "7").as_deref(), Some("Sun"));
        assert_eq!(static_label("hour", "09").as_deref(), Some("09:00"));
    }
}
