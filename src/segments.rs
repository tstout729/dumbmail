//! Segment definitions: a small JSON rule language compiled to parameterized SQL over `profiles p`.
//!
//! Campaign audiences, segment membership, flow condition splits, flow filters and exit conditions
//! all compile through here, so every rule type works everywhere. Values are always bound, never
//! formatted into the SQL. Calendar dates (`since`, `until`, `between` on dates, anniversaries) use
//! the account's time zone.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    #[default]
    All,
    Any,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Definition {
    #[serde(rename = "match", default)]
    pub match_: Match,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Condition {
    /// A profile field or custom property.
    Property {
        field: String,
        op: PropOp,
        #[serde(default)]
        value: Option<Value>,
    },
    /// How many times someone did something.
    Event {
        metric: String,
        op: EventOp,
        #[serde(default)]
        count: Option<i64>,
        #[serde(default)]
        within_days: Option<i64>,
        #[serde(default)]
        since: Option<String>,
        #[serde(default)]
        until: Option<String>,
        #[serde(rename = "where", default)]
        where_: Vec<Filter>,
    },
    /// A total, average, minimum or maximum over the events' value or a numeric event property,
    /// e.g. lifetime spend over $500.
    EventValue {
        metric: String,
        aggregate: Aggregate,
        /// `value` (the event's revenue) or a numeric event property. Default `value`.
        #[serde(default)]
        field: Option<String>,
        op: CmpOp,
        value: Value,
        #[serde(default)]
        within_days: Option<i64>,
        #[serde(default)]
        since: Option<String>,
        #[serde(default)]
        until: Option<String>,
        #[serde(rename = "where", default)]
        where_: Vec<Filter>,
    },
    /// When someone first or last did something. People who never did it don't match.
    EventTime {
        metric: String,
        which: FirstLast,
        op: TimeOp,
        #[serde(default)]
        value: Option<Value>,
        #[serde(rename = "where", default)]
        where_: Vec<Filter>,
    },
    List {
        list_id: Uuid,
        #[serde(default)]
        op: ListOp,
    },
    /// Membership in another segment, evaluated live from that segment's rules.
    Segment {
        segment_id: Uuid,
        #[serde(default)]
        op: ListOp,
        /// Filled in by `resolve`; never read from or written to JSON.
        #[serde(skip)]
        resolved: Option<Box<Definition>>,
    },
    Consent {
        channel: Channel,
        status: String,
    },
    /// Whether a send on this channel would reach them right now: an address, the right consent and,
    /// for email, no bounce or spam-complaint suppression.
    CanReceive {
        channel: Channel,
        #[serde(default = "yes")]
        value: bool,
    },
    /// Email suppressed after a hard bounce or spam complaint.
    Suppressed {
        #[serde(default = "yes")]
        value: bool,
    },
    /// Signed up through a sign-up form (any form, or one form), optionally in a time window.
    Form {
        #[serde(default)]
        form_id: Option<Uuid>,
        #[serde(default)]
        op: HasOp,
        #[serde(default)]
        within_days: Option<i64>,
        #[serde(default)]
        since: Option<String>,
        #[serde(default)]
        until: Option<String>,
    },
    /// Opened or clicked email, clicked or replied to a text, or (channel `any`) was also active on
    /// the site, viewed a product, added to cart, checked out, ordered or signed up.
    Engagement {
        channel: EngagementChannel,
        #[serde(default = "yes")]
        engaged: bool,
        within_days: i64,
    },
    /// Received, opened or clicked a message from one campaign or flow.
    Message {
        action: MessageAction,
        #[serde(default)]
        campaign_id: Option<Uuid>,
        #[serde(default)]
        flow_id: Option<Uuid>,
        #[serde(default)]
        op: HasOp,
        #[serde(default)]
        within_days: Option<i64>,
    },
    /// Predictions from someone's order history (see `Prediction`).
    Prediction {
        measure: Prediction,
        op: CmpOp,
        value: Value,
        /// The order metric. Default `Placed Order`.
        #[serde(default)]
        metric: Option<String>,
    },
    /// Viewed, carted, checked out or ordered a product: one product (the store's id), or any
    /// product in a category, with a tag or of a brand (all optional). Counts events, read from the
    /// product catalog's activity (see `features::catalog`).
    Product {
        action: ProductAction,
        #[serde(default)]
        product_id: Option<String>,
        #[serde(default)]
        category: Option<String>,
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        brand: Option<String>,
        #[serde(default = "at_least")]
        op: EventOp,
        #[serde(default)]
        count: Option<i64>,
        #[serde(default)]
        within_days: Option<i64>,
        #[serde(default)]
        since: Option<String>,
        #[serde(default)]
        until: Option<String>,
    },
    /// A stable random share of profiles, e.g. a 10% holdout. The same seed always picks the same people.
    Random {
        percent: f64,
        #[serde(default)]
        seed: Option<String>,
    },
    Group {
        #[serde(rename = "match", default)]
        match_: Match,
        conditions: Vec<Condition>,
    },
}

fn yes() -> bool {
    true
}

fn at_least() -> EventOp {
    EventOp::AtLeast
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProductAction {
    Viewed,
    Carted,
    Checkout,
    Ordered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    pub field: String,
    pub op: PropOp,
    #[serde(default)]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PropOp {
    Eq,
    Neq,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Exists,
    NotExists,
    Gt,
    Gte,
    Lt,
    Lte,
    /// `[low, high]`, inclusive. Numbers, or dates (`YYYY-MM-DD` dates are whole days in the account's time zone).
    Between,
    In,
    NotIn,
    WithinLastDays,
    MoreThanDaysAgo,
    /// The date's month and day fall within the next N days (0 = today), e.g. birthdays.
    AnniversaryWithinDays,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EventOp {
    AtLeast,
    AtMost,
    Exactly,
    Zero,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    Sum,
    Average,
    Min,
    Max,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    /// `[low, high]`, inclusive.
    Between,
    /// For `churn_risk`: an array of levels.
    In,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FirstLast {
    First,
    Last,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TimeOp {
    WithinLastDays,
    MoreThanDaysAgo,
    /// Before a date (`YYYY-MM-DD`, start of that day in the account's time zone).
    Before,
    /// On or after a date.
    After,
    /// `["YYYY-MM-DD", "YYYY-MM-DD"]`, both days included.
    Between,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ListOp {
    #[default]
    In,
    NotIn,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HasOp {
    #[default]
    Has,
    HasNot,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Email,
    Sms,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EngagementChannel {
    Email,
    Sms,
    Any,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MessageAction {
    Received,
    Opened,
    Clicked,
}

/// Figures from someone's order history (orders = the order metric's events, revenue = their value).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Prediction {
    /// Total revenue so far.
    HistoricClv,
    /// Revenue so far plus the expected next 365 days: average order value × expected orders a year
    /// (365 ÷ average days between orders, at most 52). Nothing extra for one-time buyers or
    /// customers at high churn risk.
    PredictedClv,
    OrderCount,
    AverageOrderValue,
    DaysSinceLastOrder,
    /// Needs 2+ orders.
    AverageDaysBetweenOrders,
    /// Days until the next order is due (last order + average gap); negative when overdue. Needs 2+ orders.
    ExpectedNextOrderInDays,
    /// `low`, `medium` or `high`: days since the last order compared with the usual gap (90 days for
    /// one-time buyers). Up to 1× is low, up to 2× medium, more is high.
    ChurnRisk,
}

pub fn parse(v: &Value) -> AppResult<Definition> {
    let def: Definition = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid segment definition: {e}")))?;
    validate(&def.conditions, 0)?;
    Ok(def)
}

/// Parse and load every referenced segment's rules, so membership in other segments is evaluated
/// live. Fails on unknown segments and on segments that (directly or not) refer to themselves.
/// `self_id` is the segment being saved, if any.
pub async fn load(db: &sqlx::PgPool, account_id: Uuid, v: &Value, self_id: Option<Uuid>) -> AppResult<Definition> {
    let mut def = parse(v)?;
    let mut stack: Vec<Uuid> = self_id.into_iter().collect();
    let mut budget = Budget { refs: MAX_SEGMENT_REFS, conditions: MAX_EXPANDED_CONDITIONS };
    resolve(db, account_id, &mut def.conditions, &mut stack, &mut budget).await?;
    Ok(def)
}

/// Limits on inlining other segments, so a chain of segments that each include another many times
/// can't expand into a huge query.
const MAX_SEGMENT_REFS: usize = 50;
const MAX_EXPANDED_CONDITIONS: usize = 1000;

struct Budget {
    refs: usize,
    conditions: usize,
}

fn count_conditions(conds: &[Condition]) -> usize {
    conds.iter().map(|c| if let Condition::Group { conditions, .. } = c { 1 + count_conditions(conditions) } else { 1 }).sum()
}

fn resolve<'a>(
    db: &'a sqlx::PgPool,
    account_id: Uuid,
    conds: &'a mut [Condition],
    stack: &'a mut Vec<Uuid>,
    budget: &'a mut Budget,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<()>> + Send + 'a>> {
    Box::pin(async move {
        for c in conds.iter_mut() {
            match c {
                Condition::Group { conditions, .. } => resolve(db, account_id, conditions, stack, budget).await?,
                Condition::Segment { segment_id, resolved, .. } => {
                    if stack.contains(segment_id) {
                        return Err(AppError::bad("a segment can't include itself, directly or through another segment"));
                    }
                    if stack.len() > 5 {
                        return Err(AppError::bad("segments can refer to other segments at most 5 levels deep"));
                    }
                    let raw: Value = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2")
                        .bind(*segment_id)
                        .bind(account_id)
                        .fetch_optional(db)
                        .await?
                        .ok_or_else(|| AppError::not_found("segment"))?;
                    let mut inner = parse(&raw)?;
                    budget.refs = budget.refs.checked_sub(1).ok_or_else(too_big)?;
                    budget.conditions = budget.conditions.checked_sub(count_conditions(&inner.conditions)).ok_or_else(too_big)?;
                    stack.push(*segment_id);
                    resolve(db, account_id, &mut inner.conditions, stack, budget).await?;
                    stack.pop();
                    *resolved = Some(Box::new(inner));
                }
                _ => {}
            }
        }
        Ok(())
    })
}

fn too_big() -> AppError {
    AppError::bad(format!(
        "these rules include other segments too many times (at most {MAX_SEGMENT_REFS} segment uses and {MAX_EXPANDED_CONDITIONS} rules in total, counting the rules inside them)"
    ))
}

/// Segment ids a definition refers to (not following them).
pub fn referenced_segments(v: &Value) -> HashSet<Uuid> {
    fn walk(c: &[Condition], out: &mut HashSet<Uuid>) {
        for c in c {
            match c {
                Condition::Segment { segment_id, .. } => {
                    out.insert(*segment_id);
                }
                Condition::Group { conditions, .. } => walk(conditions, out),
                _ => {}
            }
        }
    }
    let mut out = HashSet::new();
    if let Ok(d) = serde_json::from_value::<Definition>(v.clone()) {
        walk(&d.conditions, &mut out);
    }
    out
}

fn validate(conds: &[Condition], depth: usize) -> AppResult<()> {
    if depth > 4 {
        return Err(AppError::bad("segment groups can nest at most 4 levels"));
    }
    if conds.len() > 50 {
        return Err(AppError::bad("a segment can have at most 50 conditions per group"));
    }
    for c in conds {
        match c {
            Condition::Property { field, op, value } => check_filter(field, *op, value)?,
            Condition::Event { metric, count, within_days, since, until, where_, .. } => {
                check_metric(metric)?;
                if count.is_some_and(|c| c < 0) {
                    return Err(AppError::bad("count must be 0 or more"));
                }
                check_window(*within_days, since, until)?;
                check_filters(where_)?;
            }
            Condition::EventValue { metric, field, op, value, within_days, since, until, where_, .. } => {
                check_metric(metric)?;
                if field.as_deref().is_some_and(|f| f.trim().is_empty()) {
                    return Err(AppError::bad("event total needs a field (or leave it out for the event's value)"));
                }
                check_cmp(*op, value, false)?;
                check_window(*within_days, since, until)?;
                check_filters(where_)?;
            }
            Condition::EventTime { metric, op, value, where_, .. } => {
                check_metric(metric)?;
                check_time(*op, value)?;
                check_filters(where_)?;
            }
            Condition::Consent { status, .. } => {
                if !crate::models::valid_consent(status) {
                    return Err(AppError::bad("consent status must be subscribed, unsubscribed, never_subscribed or pending"));
                }
            }
            Condition::Form { within_days, since, until, .. } => check_window(*within_days, since, until)?,
            Condition::Engagement { within_days, .. } => {
                if !(1..=3650).contains(within_days) {
                    return Err(AppError::bad("engagement needs a number of days from 1 to 3650"));
                }
            }
            Condition::Message { campaign_id, flow_id, within_days, .. } => {
                if campaign_id.is_some() == flow_id.is_some() {
                    return Err(AppError::bad("a message condition needs either a campaign_id or a flow_id"));
                }
                check_window(*within_days, &None, &None)?;
            }
            Condition::Prediction { measure, op, value, metric } => {
                if metric.as_deref().is_some_and(|m| m.trim().is_empty()) {
                    return Err(AppError::bad("prediction metric can't be empty (leave it out for Placed Order)"));
                }
                check_cmp(*op, value, *measure == Prediction::ChurnRisk)?;
            }
            Condition::Product { product_id, category, tag, brand, count, within_days, since, until, .. } => {
                for v in [product_id, category, tag, brand].into_iter().flatten() {
                    if v.trim().is_empty() || v.chars().count() > 255 {
                        return Err(AppError::bad("product, category, tag and brand must be 1 to 255 characters (or left out)"));
                    }
                }
                if count.is_some_and(|c| c < 0) {
                    return Err(AppError::bad("count must be 0 or more"));
                }
                check_window(*within_days, since, until)?;
            }
            Condition::Random { percent, .. } => {
                if !(percent.is_finite() && *percent > 0.0 && *percent <= 100.0) {
                    return Err(AppError::bad("random sample percent must be more than 0 and at most 100"));
                }
            }
            Condition::Group { conditions, .. } => validate(conditions, depth + 1)?,
            Condition::List { .. } | Condition::Segment { .. } | Condition::CanReceive { .. } | Condition::Suppressed { .. } => {}
        }
    }
    Ok(())
}

fn check_metric(metric: &str) -> AppResult<()> {
    if metric.trim().is_empty() {
        return Err(AppError::bad("event condition needs a metric name"));
    }
    Ok(())
}

fn check_filters(fs: &[Filter]) -> AppResult<()> {
    for f in fs {
        check_filter(&f.field, f.op, &f.value)?;
    }
    Ok(())
}

fn parse_date(s: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

fn check_window(within_days: Option<i64>, since: &Option<String>, until: &Option<String>) -> AppResult<()> {
    if within_days.is_some_and(|d| d <= 0) {
        return Err(AppError::bad("within_days must be more than 0"));
    }
    for d in [since, until].into_iter().flatten() {
        if parse_date(d).is_none() {
            return Err(AppError::bad(format!("{d:?} isn't a date; use YYYY-MM-DD")));
        }
    }
    if let (Some(a), Some(b)) = (since.as_deref().and_then(parse_date), until.as_deref().and_then(parse_date)) {
        if a > b {
            return Err(AppError::bad("since must be on or before until"));
        }
    }
    Ok(())
}

fn pair(v: &Value) -> Option<(&Value, &Value)> {
    match v.as_array().map(Vec::as_slice) {
        Some([a, b]) => Some((a, b)),
        _ => None,
    }
}

fn check_cmp(op: CmpOp, value: &Value, levels: bool) -> AppResult<()> {
    if levels {
        let ok_level = |v: &Value| matches!(v.as_str(), Some("low" | "medium" | "high"));
        let ok = match op {
            CmpOp::Eq | CmpOp::Neq => ok_level(value),
            CmpOp::In => value.as_array().is_some_and(|a| !a.is_empty() && a.iter().all(ok_level)),
            _ => false,
        };
        if !ok {
            return Err(AppError::bad("churn_risk compares with eq, neq or in, against low, medium or high"));
        }
        return Ok(());
    }
    let ok = match op {
        CmpOp::Between => pair(value).is_some_and(|(a, b)| as_f64(a).is_some() && as_f64(b).is_some()),
        CmpOp::In => false,
        _ => as_f64(value).is_some(),
    };
    if !ok {
        return Err(AppError::bad(if op == CmpOp::Between { "between needs [low, high] numbers" } else { "this comparison needs a number (in isn't allowed here)" }));
    }
    Ok(())
}

fn check_time(op: TimeOp, value: &Option<Value>) -> AppResult<()> {
    let v = value.as_ref().unwrap_or(&Value::Null);
    let ok = match op {
        TimeOp::WithinLastDays | TimeOp::MoreThanDaysAgo => as_f64(v).is_some_and(|d| d >= 0.0),
        TimeOp::Before | TimeOp::After => v.as_str().and_then(parse_date).is_some(),
        TimeOp::Between => pair(v).is_some_and(|(a, b)| a.as_str().and_then(parse_date).is_some() && b.as_str().and_then(parse_date).is_some()),
    };
    if !ok {
        return Err(AppError::bad("first/last time needs a number of days, a YYYY-MM-DD date, or [from, to] dates"));
    }
    Ok(())
}

fn check_filter(field: &str, op: PropOp, value: &Option<Value>) -> AppResult<()> {
    if field.trim().is_empty() {
        return Err(AppError::bad("condition needs a field"));
    }
    let needs_value = !matches!(op, PropOp::Exists | PropOp::NotExists);
    if needs_value && value.as_ref().is_none_or(|v| v.is_null()) {
        return Err(AppError::bad(format!("condition on {field:?} needs a value")));
    }
    if matches!(op, PropOp::WithinLastDays | PropOp::MoreThanDaysAgo) && value.as_ref().and_then(as_f64).is_none() {
        return Err(AppError::bad("day-based conditions need a number of days"));
    }
    if op == PropOp::AnniversaryWithinDays && !value.as_ref().and_then(as_f64).is_some_and(|d| (0.0..=366.0).contains(&d)) {
        return Err(AppError::bad("anniversary_within_days needs a number of days from 0 to 366"));
    }
    if matches!(op, PropOp::In | PropOp::NotIn) && !value.as_ref().is_some_and(|v| v.is_array()) {
        return Err(AppError::bad("`in` and `not_in` need an array value"));
    }
    if op == PropOp::Between && !value.as_ref().and_then(pair).is_some_and(|(a, b)| !a.is_null() && !b.is_null()) {
        return Err(AppError::bad("`between` needs [low, high]"));
    }
    Ok(())
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

const TEXT_COLUMNS: &[&str] = &["email", "phone_number", "first_name", "last_name", "external_id", "email_consent", "sms_consent", "email_suppressed_reason"];
const TIME_COLUMNS: &[&str] = &["created_at", "updated_at", "last_event_at"];

/// The account's time zone, for calendar dates.
const ACCOUNT_TZ: &str = "(SELECT a.timezone FROM accounts a WHERE a.id = p.account_id)";

/// Where a filter reads its field from.
#[derive(Clone, Copy)]
enum Source {
    Profile,
    Event,
}

fn push_text_field(qb: &mut QueryBuilder<'_, Postgres>, src: Source, field: &str) {
    match src {
        Source::Profile if TEXT_COLUMNS.contains(&field) => {
            qb.push(format!("p.{field}"));
        }
        Source::Profile if TIME_COLUMNS.contains(&field) => {
            qb.push(format!("p.{field}::text"));
        }
        Source::Event if field == "value" => {
            qb.push("e.value::text");
        }
        Source::Profile => {
            qb.push("(p.properties->>").push_bind(field.to_string()).push(")");
        }
        Source::Event => {
            qb.push("(e.properties->>").push_bind(field.to_string()).push(")");
        }
    }
}

fn push_json_field(qb: &mut QueryBuilder<'_, Postgres>, src: Source, field: &str) -> bool {
    match src {
        Source::Profile if !TEXT_COLUMNS.contains(&field) && !TIME_COLUMNS.contains(&field) => {
            qb.push("(p.properties->").push_bind(field.to_string()).push(")");
            true
        }
        Source::Event if field != "value" => {
            qb.push("(e.properties->").push_bind(field.to_string()).push(")");
            true
        }
        _ => false,
    }
}

fn push_time_field(qb: &mut QueryBuilder<'_, Postgres>, src: Source, field: &str) {
    if matches!(src, Source::Profile) && TIME_COLUMNS.contains(&field) {
        qb.push(format!("p.{field}"));
    } else {
        qb.push("pl_ts(");
        push_text_field(qb, src, field);
        qb.push(")");
    }
}

fn push_num_field(qb: &mut QueryBuilder<'_, Postgres>, src: Source, field: &str) {
    if matches!(src, Source::Event) && field == "value" {
        qb.push("e.value::numeric");
    } else {
        qb.push("pl_num(");
        push_text_field(qb, src, field);
        qb.push(")");
    }
}

/// A point in time to compare a timestamp with: a `YYYY-MM-DD` date is the start of that day in the
/// account's time zone (`next_day` moves to the start of the following day); anything else is parsed
/// as a timestamp.
fn push_moment(qb: &mut QueryBuilder<'_, Postgres>, s: &str, next_day: bool) {
    match parse_date(s) {
        Some(d) => {
            let d = if next_day { d.succ_opt().unwrap_or(d) } else { d };
            qb.push("(").push_bind(d).push(format!("::timestamp AT TIME ZONE {ACCOUNT_TZ})"));
        }
        None => {
            qb.push("pl_ts(").push_bind(s.to_string()).push(")");
        }
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn push_filter(qb: &mut QueryBuilder<'_, Postgres>, src: Source, field: &str, op: PropOp, value: &Option<Value>) {
    let v = value.clone().unwrap_or(Value::Null);
    match op {
        PropOp::Exists | PropOp::NotExists => {
            if op == PropOp::NotExists {
                qb.push("NOT ");
            }
            qb.push("(coalesce(");
            push_text_field(qb, src, field);
            qb.push(", '') <> '')");
        }
        PropOp::Eq | PropOp::Neq => {
            if op == PropOp::Neq {
                qb.push("coalesce(NOT ");
            }
            qb.push("(");
            match &v {
                Value::Number(n) => {
                    push_num_field(qb, src, field);
                    qb.push(" = ").push_bind(n.as_f64().unwrap_or_default()).push("::numeric");
                }
                Value::Bool(b) => {
                    qb.push("lower(");
                    push_text_field(qb, src, field);
                    qb.push(") = ").push_bind(b.to_string());
                }
                _ => {
                    qb.push("lower(");
                    push_text_field(qb, src, field);
                    qb.push(") = lower(").push_bind(value_text(&v)).push(")");
                }
            }
            qb.push(")");
            if op == PropOp::Neq {
                qb.push(", true)");
            }
        }
        PropOp::Contains | PropOp::NotContains => {
            if op == PropOp::NotContains {
                qb.push("coalesce(NOT ");
            }
            let needle = value_text(&v);
            qb.push("(");
            push_text_field(qb, src, field);
            qb.push(" ILIKE ").push_bind(format!("%{}%", like_escape(&needle)));
            // Array properties, e.g. tags: ["vip", "wholesale"]
            qb.push(" OR (");
            if push_json_field(qb, src, field) {
                qb.push(" ? ").push_bind(needle);
            } else {
                qb.push("false");
            }
            qb.push("))");
            if op == PropOp::NotContains {
                qb.push(", true)");
            }
        }
        PropOp::StartsWith => {
            push_text_field(qb, src, field);
            qb.push(" ILIKE ").push_bind(format!("{}%", like_escape(&value_text(&v))));
        }
        PropOp::EndsWith => {
            push_text_field(qb, src, field);
            qb.push(" ILIKE ").push_bind(format!("%{}", like_escape(&value_text(&v))));
        }
        PropOp::Gt | PropOp::Gte | PropOp::Lt | PropOp::Lte => {
            let sym = match op {
                PropOp::Gt => " > ",
                PropOp::Gte => " >= ",
                PropOp::Lt => " < ",
                _ => " <= ",
            };
            let is_time_col = matches!(src, Source::Profile) && TIME_COLUMNS.contains(&field);
            match (&v, is_time_col) {
                (Value::Number(n), false) => {
                    push_num_field(qb, src, field);
                    qb.push(sym).push_bind(n.as_f64().unwrap_or_default()).push("::numeric");
                }
                (other, _) => {
                    let s = value_text(other);
                    if !is_time_col && s.parse::<f64>().is_ok() {
                        push_num_field(qb, src, field);
                        qb.push(sym).push_bind(s.parse::<f64>().unwrap_or_default()).push("::numeric");
                    } else {
                        push_time_field(qb, src, field);
                        qb.push(sym).push("pl_ts(").push_bind(s).push(")");
                    }
                }
            }
        }
        PropOp::Between => {
            let Some((a, b)) = pair(&v) else {
                qb.push("false");
                return;
            };
            let is_time_col = matches!(src, Source::Profile) && TIME_COLUMNS.contains(&field);
            match (as_f64(a), as_f64(b)) {
                (Some(lo), Some(hi)) if !is_time_col => {
                    qb.push("(");
                    push_num_field(qb, src, field);
                    qb.push(" BETWEEN ").push_bind(lo).push("::numeric AND ").push_bind(hi).push("::numeric)");
                }
                _ => {
                    // Dates: from the start of the first day to the end of the last.
                    qb.push("(");
                    push_time_field(qb, src, field);
                    qb.push(" >= ");
                    push_moment(qb, &value_text(a), false);
                    qb.push(" AND ");
                    push_time_field(qb, src, field);
                    qb.push(" < ");
                    push_moment(qb, &value_text(b), true);
                    qb.push(")");
                }
            }
        }
        PropOp::In | PropOp::NotIn => {
            let items: Vec<String> = v.as_array().map(|a| a.iter().map(|x| value_text(x).to_lowercase()).collect()).unwrap_or_default();
            if op == PropOp::NotIn {
                qb.push("coalesce(NOT ");
            }
            qb.push("(lower(");
            push_text_field(qb, src, field);
            qb.push(") = ANY(").push_bind(items).push("))");
            if op == PropOp::NotIn {
                qb.push(", true)");
            }
        }
        PropOp::WithinLastDays | PropOp::MoreThanDaysAgo => {
            let days = as_f64(&v).unwrap_or(0.0);
            push_time_field(qb, src, field);
            qb.push(if op == PropOp::WithinLastDays { " >= " } else { " < " });
            qb.push("now() - make_interval(secs => ").push_bind(days * 86400.0).push(")");
        }
        PropOp::AnniversaryWithinDays => {
            // Month and day of the date, compared with today and the next N days in the account's time zone.
            let days = as_f64(&v).unwrap_or(0.0).clamp(0.0, 366.0) as i32;
            qb.push("EXISTS (SELECT 1 FROM generate_series(0, ").push_bind(days).push(") g(d) WHERE to_char((now() AT TIME ZONE ");
            qb.push(ACCOUNT_TZ);
            qb.push(")::date + g.d, 'MM-DD') = to_char(");
            if matches!(src, Source::Profile) && TIME_COLUMNS.contains(&field) {
                qb.push(format!("(p.{field} AT TIME ZONE {ACCOUNT_TZ})"));
            } else {
                // Dates stored as text ("1990-05-12") keep their own month and day; anything that isn't a date never matches.
                qb.push("pl_ts(");
                push_text_field(qb, src, field);
                qb.push(")::date");
            }
            qb.push(", 'MM-DD'))");
        }
    }
}

/// `e.metric_id` is one of these metrics in the profile's account.
fn push_metrics(qb: &mut QueryBuilder<'_, Postgres>, names: Vec<String>) {
    qb.push("e.metric_id IN (SELECT mt.id FROM metrics mt WHERE mt.account_id = p.account_id AND mt.name = ANY(").push_bind(names).push("))");
}

/// ` AND <time window>` on `e.occurred_at`.
fn push_window(qb: &mut QueryBuilder<'_, Postgres>, within_days: Option<i64>, since: &Option<String>, until: &Option<String>) {
    if let Some(d) = within_days {
        qb.push(" AND e.occurred_at >= now() - make_interval(days => ").push_bind(d as i32).push(")");
    }
    if let Some(s) = since {
        qb.push(" AND e.occurred_at >= ");
        push_moment(qb, s, false);
    }
    if let Some(u) = until {
        qb.push(" AND e.occurred_at < ");
        push_moment(qb, u, true);
    }
}

fn push_where(qb: &mut QueryBuilder<'_, Postgres>, where_: &[Filter]) {
    for f in where_ {
        qb.push(" AND (");
        push_filter(qb, Source::Event, &f.field, f.op, &f.value);
        qb.push(")");
    }
}

/// `<expr> <op> <value>` for numeric comparisons. NULL never matches.
fn push_cmp(qb: &mut QueryBuilder<'_, Postgres>, expr: impl Fn(&mut QueryBuilder<'_, Postgres>), op: CmpOp, value: &Value) {
    match op {
        CmpOp::Between => {
            let (lo, hi) = pair(value).map(|(a, b)| (as_f64(a).unwrap_or_default(), as_f64(b).unwrap_or_default())).unwrap_or_default();
            qb.push("(");
            expr(qb);
            qb.push(" BETWEEN ").push_bind(lo).push("::numeric AND ").push_bind(hi).push("::numeric)");
        }
        CmpOp::In => {
            let items: Vec<String> = value.as_array().map(|a| a.iter().map(value_text).collect()).unwrap_or_default();
            qb.push("(");
            expr(qb);
            qb.push("::text = ANY(").push_bind(items).push("))");
        }
        _ => {
            let sym = match op {
                CmpOp::Eq => " = ",
                CmpOp::Neq => " <> ",
                CmpOp::Gt => " > ",
                CmpOp::Gte => " >= ",
                CmpOp::Lt => " < ",
                _ => " <= ",
            };
            if let Some(s) = value.as_str().filter(|s| as_f64(&Value::String(s.to_string())).is_none()) {
                // churn_risk levels
                qb.push("(");
                expr(qb);
                qb.push(sym).push_bind(s.to_string()).push(")");
            } else {
                qb.push("(");
                expr(qb);
                qb.push(sym).push_bind(as_f64(value).unwrap_or_default()).push("::numeric)");
            }
        }
    }
}

const ENGAGED_EMAIL: &[&str] = &["Opened Email", "Clicked Email"];
const ENGAGED_SMS: &[&str] = &["Clicked SMS", "Received SMS Reply"];
const ENGAGED_SITE: &[&str] = &["Active on Site", "Viewed Product", "Added to Cart", "Started Checkout", "Placed Order", "Subscribed to List"];

fn push_condition(qb: &mut QueryBuilder<'_, Postgres>, c: &Condition) {
    match c {
        Condition::Property { field, op, value } => {
            qb.push("(");
            push_filter(qb, Source::Profile, field, *op, value);
            qb.push(")");
        }
        Condition::Event { metric, op, count, within_days, since, until, where_ } => {
            qb.push("((SELECT count(*) FROM events e WHERE e.profile_id = p.id AND ");
            push_metrics(qb, vec![metric.clone()]);
            push_window(qb, *within_days, since, until);
            push_where(qb, where_);
            let n = count.unwrap_or(1);
            match op {
                EventOp::AtLeast => qb.push(") >= ").push_bind(n),
                EventOp::AtMost => qb.push(") <= ").push_bind(n),
                EventOp::Exactly => qb.push(") = ").push_bind(n),
                EventOp::Zero => qb.push(") = 0"),
            };
            qb.push(")");
        }
        Condition::EventValue { metric, aggregate, field, op, value, within_days, since, until, where_ } => {
            let field = field.clone().unwrap_or_else(|| "value".into());
            let agg = match aggregate {
                Aggregate::Sum => "coalesce(sum(",
                Aggregate::Average => "(avg(",
                Aggregate::Min => "(min(",
                Aggregate::Max => "(max(",
            };
            let expr = |qb: &mut QueryBuilder<'_, Postgres>| {
                qb.push("(SELECT ").push(agg);
                push_num_field(qb, Source::Event, &field);
                qb.push(if *aggregate == Aggregate::Sum { "), 0)" } else { "))" });
                qb.push(" FROM events e WHERE e.profile_id = p.id AND ");
                push_metrics(qb, vec![metric.clone()]);
                push_window(qb, *within_days, since, until);
                push_where(qb, where_);
                qb.push(")");
            };
            push_cmp(qb, expr, *op, value);
        }
        Condition::EventTime { metric, which, op, value, where_ } => {
            let v = value.clone().unwrap_or(Value::Null);
            qb.push("((SELECT ").push(if *which == FirstLast::First { "min" } else { "max" });
            qb.push("(e.occurred_at) FROM events e WHERE e.profile_id = p.id AND ");
            push_metrics(qb, vec![metric.clone()]);
            push_where(qb, where_);
            qb.push(")");
            match op {
                TimeOp::WithinLastDays | TimeOp::MoreThanDaysAgo => {
                    qb.push(if *op == TimeOp::WithinLastDays { " >= " } else { " < " });
                    qb.push("now() - make_interval(secs => ").push_bind(as_f64(&v).unwrap_or(0.0) * 86400.0).push(")");
                }
                TimeOp::Before => {
                    qb.push(" < ");
                    push_moment(qb, &value_text(&v), false);
                }
                TimeOp::After => {
                    qb.push(" >= ");
                    push_moment(qb, &value_text(&v), false);
                }
                TimeOp::Between => {
                    let (a, b) = pair(&v).map(|(a, b)| (value_text(a), value_text(b))).unwrap_or_default();
                    qb.push(" BETWEEN ");
                    push_moment(qb, &a, false);
                    qb.push(" AND ");
                    push_moment(qb, &b, true);
                    qb.push(" - interval '1 microsecond'");
                }
            }
            qb.push(")");
        }
        Condition::List { list_id, op } => {
            if *op == ListOp::NotIn {
                qb.push("NOT ");
            }
            qb.push("EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ").push_bind(*list_id).push(" AND lm.profile_id = p.id)");
        }
        Condition::Segment { segment_id, op, resolved } => {
            if *op == ListOp::NotIn {
                qb.push("NOT ");
            }
            match resolved {
                Some(def) => push_conditions(qb, def.match_, &def.conditions),
                // Not resolved (shouldn't happen on the main paths): use the last refreshed membership.
                None => {
                    qb.push("EXISTS (SELECT 1 FROM segment_members sm WHERE sm.segment_id = ").push_bind(*segment_id).push(" AND sm.profile_id = p.id)");
                }
            }
        }
        Condition::Consent { channel, status } => {
            let col = match channel {
                Channel::Email => "p.email_consent",
                Channel::Sms => "p.sms_consent",
            };
            qb.push(format!("({col} = ")).push_bind(status.clone()).push(")");
        }
        Condition::CanReceive { channel, value } => {
            if !value {
                qb.push("NOT ");
            }
            qb.push(match channel {
                Channel::Email => "(p.email IS NOT NULL AND p.email_consent NOT IN ('unsubscribed', 'pending') AND p.email_suppressed_reason IS NULL)",
                Channel::Sms => "(p.phone_number IS NOT NULL AND p.sms_consent = 'subscribed')",
            });
        }
        Condition::Suppressed { value } => {
            qb.push(if *value { "(p.email_suppressed_reason IS NOT NULL)" } else { "(p.email_suppressed_reason IS NULL)" });
        }
        Condition::Form { form_id, op, within_days, since, until } => {
            if *op == HasOp::HasNot {
                qb.push("NOT ");
            }
            qb.push("EXISTS (SELECT 1 FROM events e WHERE e.profile_id = p.id AND ");
            push_metrics(qb, vec!["Subscribed to List".into()]);
            qb.push(" AND e.properties ? 'form_id'");
            if let Some(f) = form_id {
                qb.push(" AND e.properties->>'form_id' = ").push_bind(f.to_string());
            }
            push_window(qb, *within_days, since, until);
            qb.push(")");
        }
        Condition::Engagement { channel, engaged, within_days } => {
            let mut names: Vec<&str> = match channel {
                EngagementChannel::Email => ENGAGED_EMAIL.to_vec(),
                EngagementChannel::Sms => ENGAGED_SMS.to_vec(),
                EngagementChannel::Any => [ENGAGED_EMAIL, ENGAGED_SMS, ENGAGED_SITE].concat(),
            };
            names.dedup();
            if !engaged {
                qb.push("NOT ");
            }
            qb.push("EXISTS (SELECT 1 FROM events e WHERE e.profile_id = p.id AND ");
            push_metrics(qb, names.into_iter().map(String::from).collect());
            push_window(qb, Some(*within_days), &None, &None);
            qb.push(")");
        }
        Condition::Message { action, campaign_id, flow_id, op, within_days } => {
            let names: Vec<String> = match action {
                MessageAction::Received => vec!["Received Email".into(), "Received SMS".into()],
                MessageAction::Opened => vec!["Opened Email".into()],
                MessageAction::Clicked => vec!["Clicked Email".into(), "Clicked SMS".into()],
            };
            if *op == HasOp::HasNot {
                qb.push("NOT ");
            }
            qb.push("EXISTS (SELECT 1 FROM events e WHERE e.profile_id = p.id AND ");
            push_metrics(qb, names);
            if let Some(c) = campaign_id {
                qb.push(" AND e.properties->>'campaign_id' = ").push_bind(c.to_string());
            }
            if let Some(f) = flow_id {
                qb.push(" AND e.properties->>'flow_id' = ").push_bind(f.to_string());
            }
            push_window(qb, *within_days, &None, &None);
            qb.push(")");
        }
        Condition::Prediction { measure, op, value, metric } => {
            let metric = metric.clone().unwrap_or_else(|| "Placed Order".into());
            let measure = *measure;
            let expr = move |qb: &mut QueryBuilder<'_, Postgres>| {
                // n = orders, gap = average days between orders (2+ orders), since = days since the last order.
                const N: &str = "count(*)";
                const SINCE: &str = "(extract(epoch FROM now() - max(e.occurred_at)) / 86400)";
                const GAP: &str = "(extract(epoch FROM max(e.occurred_at) - min(e.occurred_at)) / 86400 / nullif(count(*) - 1, 0))";
                // The usual gap for churn: the average gap with 2+ orders (at least a day), else 90 days.
                // (greatest() skips NULLs, so the order count has to decide.)
                const USUAL: &str = "(CASE WHEN count(*) > 1 THEN greatest(extract(epoch FROM max(e.occurred_at) - min(e.occurred_at)) / 86400 / (count(*) - 1), 1) ELSE 90 END)";
                let sql = match measure {
                    Prediction::HistoricClv => "coalesce(sum(e.value), 0)".to_string(),
                    Prediction::OrderCount => N.to_string(),
                    Prediction::AverageOrderValue => "avg(e.value)".to_string(),
                    Prediction::DaysSinceLastOrder => SINCE.to_string(),
                    Prediction::AverageDaysBetweenOrders => GAP.to_string(),
                    Prediction::ExpectedNextOrderInDays => format!("({GAP} - {SINCE})"),
                    Prediction::ChurnRisk => format!(
                        "CASE WHEN {N} = 0 THEN NULL WHEN {SINCE} <= {USUAL} THEN 'low' \
                         WHEN {SINCE} <= 2 * {USUAL} THEN 'medium' ELSE 'high' END"
                    ),
                    Prediction::PredictedClv => format!(
                        "coalesce(sum(e.value), 0) + CASE WHEN {N} > 1 AND {SINCE} <= 2 * greatest({GAP}, 1) \
                         THEN coalesce(avg(e.value), 0) * least(365 / greatest({GAP}, 1), 52) ELSE 0 END"
                    ),
                };
                qb.push("(SELECT ").push(sql).push(" FROM events e WHERE e.profile_id = p.id AND ");
                push_metrics(qb, vec![metric.clone()]);
                qb.push(")");
            };
            push_cmp(qb, expr, *op, value);
        }
        Condition::Product { action, product_id, category, tag, brand, op, count, within_days, since, until } => {
            let kind = match action {
                ProductAction::Viewed => "viewed",
                ProductAction::Carted => "carted",
                ProductAction::Checkout => "checkout",
                ProductAction::Ordered => "ordered",
            };
            qb.push("((SELECT count(DISTINCT e.event_id) FROM catalog_interactions e JOIN catalog_products cp ON cp.id = e.product_id WHERE e.profile_id = p.id AND e.kind = ");
            qb.push_bind(kind);
            if let Some(id) = product_id {
                qb.push(" AND cp.external_id = ").push_bind(id.trim().to_string());
            }
            if let Some(c) = category {
                qb.push(" AND cp.categories @> ARRAY[").push_bind(c.trim().to_string()).push("]::text[]");
            }
            if let Some(t) = tag {
                qb.push(" AND cp.tags @> ARRAY[").push_bind(t.trim().to_string()).push("]::text[]");
            }
            if let Some(b) = brand {
                qb.push(" AND lower(cp.brand) = lower(").push_bind(b.trim().to_string()).push(")");
            }
            push_window(qb, *within_days, since, until);
            let n = count.unwrap_or(1);
            match op {
                EventOp::AtLeast => qb.push(") >= ").push_bind(n),
                EventOp::AtMost => qb.push(") <= ").push_bind(n),
                EventOp::Exactly => qb.push(") = ").push_bind(n),
                EventOp::Zero => qb.push(") = 0"),
            };
            qb.push(")");
        }
        Condition::Random { percent, seed } => {
            // Stable per profile and seed: hash into 0..9999 and keep the lowest `percent` share.
            qb.push("(((hashtextextended(p.id::text || ").push_bind(seed.clone().unwrap_or_default()).push(", 0) % 10000) + 10000) % 10000 < ");
            qb.push_bind((percent * 100.0).round() as i64).push(")");
        }
        Condition::Group { match_, conditions } => push_conditions(qb, *match_, conditions),
    }
}

fn push_conditions(qb: &mut QueryBuilder<'_, Postgres>, m: Match, conds: &[Condition]) {
    if conds.is_empty() {
        qb.push("TRUE");
        return;
    }
    let joiner = if m == Match::All { " AND " } else { " OR " };
    qb.push("(");
    for (i, c) in conds.iter().enumerate() {
        if i > 0 {
            qb.push(joiner);
        }
        // coalesce: a rule with no answer (e.g. NULL from a missing value) counts as false, so NOT
        // around a group or a "not in segment" still behaves.
        qb.push("coalesce(");
        push_condition(qb, c);
        qb.push(", false)");
    }
    qb.push(")");
}

/// Push a boolean SQL expression for the definition, referencing profile alias `p`.
pub fn push_definition(qb: &mut QueryBuilder<'_, Postgres>, def: &Definition) {
    push_conditions(qb, def.match_, &def.conditions);
}

/// Count profiles in an account matching a definition, plus a sample.
pub async fn preview(db: &sqlx::PgPool, account_id: Uuid, def: &Definition, sample: i64) -> AppResult<(i64, Vec<crate::models::Profile>)> {
    let mut qb = QueryBuilder::new("SELECT count(*) FROM profiles p WHERE p.account_id = ");
    qb.push_bind(account_id).push(" AND ");
    push_definition(&mut qb, def);
    let count: i64 = qb.build_query_scalar().fetch_one(db).await?;

    let mut qb = QueryBuilder::new(format!("SELECT {} FROM profiles p WHERE p.account_id = ", crate::models::PROFILE_COLS));
    qb.push_bind(account_id).push(" AND ");
    push_definition(&mut qb, def);
    qb.push(" ORDER BY p.created_at DESC LIMIT ").push_bind(sample);
    let rows = qb.build_query_as::<crate::models::Profile>().fetch_all(db).await?;
    Ok((count, rows))
}

/// Does one profile match? Used by flow condition splits, filters and exit conditions.
pub async fn profile_matches(db: &sqlx::PgPool, profile_id: Uuid, def: &Definition) -> AppResult<bool> {
    let mut qb = QueryBuilder::new("SELECT EXISTS (SELECT 1 FROM profiles p WHERE p.id = ");
    qb.push_bind(profile_id).push(" AND ");
    push_definition(&mut qb, def);
    qb.push(")");
    Ok(qb.build_query_scalar().fetch_one(db).await?)
}

/// Recompute materialized membership. Returns profile ids that newly entered.
pub async fn refresh(db: &sqlx::PgPool, account_id: Uuid, segment_id: Uuid, def: &Definition) -> AppResult<Vec<Uuid>> {
    let mut tx = db.begin().await?;
    let first_refresh: bool = sqlx::query_scalar("SELECT refreshed_at IS NULL FROM segments WHERE id = $1").bind(segment_id).fetch_optional(&mut *tx).await?.unwrap_or(true);
    let mut qb = QueryBuilder::new("CREATE TEMP TABLE seg_now ON COMMIT DROP AS SELECT p.id FROM profiles p WHERE p.account_id = ");
    qb.push_bind(account_id).push(" AND ");
    push_definition(&mut qb, def);
    qb.build().execute(&mut *tx).await?;
    let left: Vec<Uuid> = sqlx::query_scalar("DELETE FROM segment_members sm WHERE sm.segment_id = $1 AND NOT EXISTS (SELECT 1 FROM seg_now s WHERE s.id = sm.profile_id) RETURNING profile_id")
        .bind(segment_id)
        .fetch_all(&mut *tx)
        .await?;
    let entered: Vec<Uuid> = sqlx::query_scalar("INSERT INTO segment_members (segment_id, profile_id) SELECT $1, id FROM seg_now ON CONFLICT DO NOTHING RETURNING profile_id")
        .bind(segment_id)
        .fetch_all(&mut *tx)
        .await?;
    sqlx::query("UPDATE segments SET member_count = (SELECT count(*) FROM segment_members WHERE segment_id = $1), refreshed_at = now() WHERE id = $1")
        .bind(segment_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // The first refresh only records who is already in the segment.
    if !first_refresh {
        crate::features::webhooks::segment_changed(db, account_id, segment_id, &entered, &left).await;
    }
    Ok(entered)
}

/// Evaluate a simple filter against a JSON object (flow trigger filters on event properties).
pub fn json_filter_matches(obj: &Value, f: &Filter) -> bool {
    let got = if f.field == "value" { obj.get("$value").or_else(|| obj.get("value")) } else { obj.pointer(&format!("/{}", f.field.replace('.', "/"))) };
    let want = f.value.clone().unwrap_or(Value::Null);
    let got_s = got.map(value_text).unwrap_or_default();
    let got_n = got.and_then(as_f64);
    let want_n = as_f64(&want);
    let in_list = |list: &Value| list.as_array().is_some_and(|a| a.iter().any(|x| value_text(x).eq_ignore_ascii_case(&got_s)));
    match f.op {
        PropOp::Exists => got.is_some_and(|g| !g.is_null() && !got_s.is_empty()),
        PropOp::NotExists => got.is_none_or(|g| g.is_null() || got_s.is_empty()),
        PropOp::Eq => match (got_n, want_n) {
            (Some(a), Some(b)) if want.is_number() => a == b,
            _ => got_s.eq_ignore_ascii_case(&value_text(&want)),
        },
        PropOp::Neq => !got_s.eq_ignore_ascii_case(&value_text(&want)),
        PropOp::Contains => match got {
            Some(Value::Array(a)) => a.iter().any(|x| value_text(x).eq_ignore_ascii_case(&value_text(&want))),
            _ => got_s.to_lowercase().contains(&value_text(&want).to_lowercase()),
        },
        PropOp::NotContains => !got_s.to_lowercase().contains(&value_text(&want).to_lowercase()),
        PropOp::StartsWith => got_s.to_lowercase().starts_with(&value_text(&want).to_lowercase()),
        PropOp::EndsWith => got_s.to_lowercase().ends_with(&value_text(&want).to_lowercase()),
        PropOp::Gt => matches!((got_n, want_n), (Some(a), Some(b)) if a > b),
        PropOp::Gte => matches!((got_n, want_n), (Some(a), Some(b)) if a >= b),
        PropOp::Lt => matches!((got_n, want_n), (Some(a), Some(b)) if a < b),
        PropOp::Lte => matches!((got_n, want_n), (Some(a), Some(b)) if a <= b),
        PropOp::Between => match (got_n, pair(&want).and_then(|(a, b)| Some((as_f64(a)?, as_f64(b)?)))) {
            (Some(x), Some((lo, hi))) => x >= lo && x <= hi,
            _ => false,
        },
        PropOp::In => in_list(&want),
        PropOp::NotIn => !in_list(&want),
        PropOp::WithinLastDays | PropOp::MoreThanDaysAgo | PropOp::AnniversaryWithinDays => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_compiles() {
        let def = parse(&json!({
            "match": "all",
            "conditions": [
                {"type": "property", "field": "city", "op": "eq", "value": "Portland"},
                {"type": "event", "metric": "Placed Order", "op": "at_least", "count": 2, "within_days": 90,
                 "where": [{"field": "value", "op": "gt", "value": 50}]},
                {"type": "consent", "channel": "email", "status": "subscribed"},
                {"type": "group", "match": "any", "conditions": [
                    {"type": "property", "field": "created_at", "op": "within_last_days", "value": 30},
                    {"type": "property", "field": "tags", "op": "contains", "value": "vip"}
                ]}
            ]
        }))
        .unwrap();
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM profiles p WHERE ");
        push_definition(&mut qb, &def);
        let sql = qb.sql();
        assert!(sql.contains("p.properties->>$1"));
        assert!(sql.contains("FROM events e"));
        assert!(!sql.contains("Portland"), "values must be bound, not inlined");
    }

    #[test]
    fn every_new_rule_compiles_with_bound_values() {
        let def = parse(&json!({"match": "any", "conditions": [
            {"type": "event", "metric": "Placed Order", "op": "at_least", "count": 1, "since": "2026-11-01", "until": "2026-11-30"},
            {"type": "event_value", "metric": "Placed Order", "aggregate": "sum", "op": "gt", "value": 500, "within_days": 365},
            {"type": "event_value", "metric": "Placed Order", "aggregate": "average", "field": "items_count", "op": "between", "value": [2, 5]},
            {"type": "event_time", "metric": "Placed Order", "which": "last", "op": "more_than_days_ago", "value": 60},
            {"type": "event_time", "metric": "Placed Order", "which": "first", "op": "between", "value": ["2026-01-01", "2026-03-31"]},
            {"type": "segment", "segment_id": "0190a8a2-0000-7000-8000-000000000001", "op": "not_in"},
            {"type": "can_receive", "channel": "sms"},
            {"type": "suppressed", "value": false},
            {"type": "form", "form_id": "0190a8a2-0000-7000-8000-000000000002", "within_days": 30},
            {"type": "engagement", "channel": "any", "engaged": false, "within_days": 90},
            {"type": "message", "action": "clicked", "campaign_id": "0190a8a2-0000-7000-8000-000000000003"},
            {"type": "prediction", "measure": "churn_risk", "op": "in", "value": ["medium", "high"]},
            {"type": "prediction", "measure": "predicted_clv", "op": "gte", "value": 1000},
            {"type": "random", "percent": 10, "seed": "holdout-nov"},
            {"type": "property", "field": "birthday", "op": "anniversary_within_days", "value": 7},
            {"type": "property", "field": "country", "op": "not_in", "value": ["US", "CA"]},
            {"type": "property", "field": "zip", "op": "between", "value": [97201, 97299]},
            {"type": "property", "field": "email", "op": "ends_with", "value": "@gmail.com"}
        ]}))
        .unwrap();
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM profiles p WHERE ");
        push_definition(&mut qb, &def);
        let sql = qb.sql();
        for leaked in ["holdout-nov", "gmail", "2026-11-01", "Placed Order", "0190a8a2", "97201"] {
            assert!(!sql.contains(leaked), "{leaked} must be bound, not inlined");
        }
        assert!(sql.contains("hashtextextended"));
        assert!(sql.contains("segment_members"), "an unresolved segment falls back to its last membership");
    }

    #[test]
    fn product_rules_compile_with_bound_values() {
        let def = parse(&json!({"match": "all", "conditions": [
            {"type": "product", "action": "ordered", "product_id": "TEE-1", "count": 2, "within_days": 90},
            {"type": "product", "action": "viewed", "category": "Shirts", "tag": "organic", "brand": "Acme", "since": "2026-10-01"},
            {"type": "product", "action": "carted", "op": "zero"}
        ]}))
        .unwrap();
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM profiles p WHERE ");
        push_definition(&mut qb, &def);
        let sql = qb.sql();
        assert!(sql.contains("FROM catalog_interactions e JOIN catalog_products cp"));
        assert!(sql.contains("cp.categories @> ARRAY["));
        for leaked in ["TEE-1", "Shirts", "organic", "Acme", "ordered", "2026-10-01"] {
            assert!(!sql.contains(leaked), "{leaked} must be bound, not inlined");
        }
        assert!(parse(&json!({"conditions": [{"type": "product", "action": "bought"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "product", "action": "viewed", "category": " "}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "product", "action": "viewed", "within_days": 0}]})).is_err());
    }

    #[test]
    fn rejects_bad_definitions() {
        assert!(parse(&json!({"conditions": [{"type": "property", "field": "x", "op": "eq"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "nope"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "consent", "channel": "email", "status": "maybe"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "event", "metric": "x", "op": "zero", "since": "11/01/2026"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "event", "metric": "x", "op": "zero", "since": "2026-12-01", "until": "2026-11-01"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "event_value", "metric": "x", "aggregate": "sum", "op": "gt", "value": "lots"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "prediction", "measure": "churn_risk", "op": "gt", "value": 1}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "prediction", "measure": "order_count", "op": "in", "value": [1]}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "random", "percent": 0}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "random", "percent": 101}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "message", "action": "opened"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "engagement", "channel": "email", "within_days": 0}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "property", "field": "zip", "op": "between", "value": [1]}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "event_time", "metric": "x", "which": "last", "op": "before", "value": "soon"}]})).is_err());
    }

    #[test]
    fn finds_referenced_segments() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let refs = referenced_segments(&json!({"conditions": [
            {"type": "segment", "segment_id": a},
            {"type": "group", "match": "any", "conditions": [{"type": "segment", "segment_id": b, "op": "not_in"}]}
        ]}));
        assert_eq!(refs, HashSet::from([a, b]));
    }

    #[test]
    fn json_filters() {
        let ev = json!({"value": 120.5, "items": ["Shoes"], "source": "web"});
        let f = |field: &str, op: PropOp, value: Value| Filter { field: field.into(), op, value: Some(value) };
        assert!(json_filter_matches(&ev, &f("value", PropOp::Gt, json!(100))));
        assert!(json_filter_matches(&ev, &f("items", PropOp::Contains, json!("shoes"))));
        assert!(json_filter_matches(&ev, &f("source", PropOp::Eq, json!("WEB"))));
        assert!(!json_filter_matches(&ev, &f("source", PropOp::Eq, json!("pos"))));
        assert!(json_filter_matches(&ev, &f("value", PropOp::Between, json!([100, 150]))));
        assert!(json_filter_matches(&ev, &f("source", PropOp::NotIn, json!(["pos", "app"]))));
        assert!(json_filter_matches(&ev, &f("source", PropOp::EndsWith, json!("EB"))));
    }
}
