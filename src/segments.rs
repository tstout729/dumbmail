//! Segment definitions: a small JSON rule language compiled to parameterized SQL over `profiles p`.

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
    Property {
        field: String,
        op: PropOp,
        #[serde(default)]
        value: Option<Value>,
    },
    Event {
        metric: String,
        op: EventOp,
        #[serde(default)]
        count: Option<i64>,
        #[serde(default)]
        within_days: Option<i64>,
        #[serde(rename = "where", default)]
        where_: Vec<Filter>,
    },
    List {
        list_id: Uuid,
        #[serde(default)]
        op: ListOp,
    },
    Consent {
        channel: Channel,
        status: String,
    },
    Group {
        #[serde(rename = "match", default)]
        match_: Match,
        conditions: Vec<Condition>,
    },
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
    Exists,
    NotExists,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    WithinLastDays,
    MoreThanDaysAgo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EventOp {
    AtLeast,
    AtMost,
    Exactly,
    Zero,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ListOp {
    #[default]
    In,
    NotIn,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Email,
    Sms,
}

pub fn parse(v: &Value) -> AppResult<Definition> {
    let def: Definition = serde_json::from_value(v.clone())
        .map_err(|e| AppError::bad(format!("invalid segment definition: {e}")))?;
    validate(&def.conditions, 0)?;
    Ok(def)
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
            Condition::Event { metric, count, within_days, where_, .. } => {
                if metric.trim().is_empty() {
                    return Err(AppError::bad("event condition needs a metric name"));
                }
                if count.is_some_and(|c| c < 0) || within_days.is_some_and(|d| d <= 0) {
                    return Err(AppError::bad("count must be >= 0 and within_days > 0"));
                }
                for f in where_ {
                    check_filter(&f.field, f.op, &f.value)?;
                }
            }
            Condition::Consent { status, .. } => {
                if !crate::models::valid_consent(status) {
                    return Err(AppError::bad("consent status must be subscribed, unsubscribed or never_subscribed"));
                }
            }
            Condition::Group { conditions, .. } => validate(conditions, depth + 1)?,
            Condition::List { .. } => {}
        }
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
    if op == PropOp::In && !value.as_ref().is_some_and(|v| v.is_array()) {
        return Err(AppError::bad("`in` needs an array value"));
    }
    Ok(())
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

const TEXT_COLUMNS: &[&str] = &["email", "phone_number", "first_name", "last_name", "external_id", "email_consent", "sms_consent"];
const TIME_COLUMNS: &[&str] = &["created_at", "updated_at", "last_event_at"];

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
        PropOp::In => {
            let items: Vec<String> = v
                .as_array()
                .map(|a| a.iter().map(|x| value_text(x).to_lowercase()).collect())
                .unwrap_or_default();
            qb.push("lower(");
            push_text_field(qb, src, field);
            qb.push(") = ANY(").push_bind(items).push(")");
        }
        PropOp::WithinLastDays | PropOp::MoreThanDaysAgo => {
            let days = as_f64(&v).unwrap_or(0.0);
            push_time_field(qb, src, field);
            qb.push(if op == PropOp::WithinLastDays { " >= " } else { " < " });
            qb.push("now() - make_interval(secs => ").push_bind(days * 86400.0).push(")");
        }
    }
}

fn push_condition(qb: &mut QueryBuilder<'_, Postgres>, c: &Condition) {
    match c {
        Condition::Property { field, op, value } => {
            qb.push("(");
            push_filter(qb, Source::Profile, field, *op, value);
            qb.push(")");
        }
        Condition::Event { metric, op, count, within_days, where_ } => {
            qb.push("((SELECT count(*) FROM events e WHERE e.profile_id = p.id AND e.metric_id = (SELECT id FROM metrics mt WHERE mt.account_id = p.account_id AND mt.name = ")
                .push_bind(metric.clone())
                .push(")");
            if let Some(d) = within_days {
                qb.push(" AND e.occurred_at >= now() - make_interval(days => ").push_bind(*d as i32).push(")");
            }
            for f in where_ {
                qb.push(" AND (");
                push_filter(qb, Source::Event, &f.field, f.op, &f.value);
                qb.push(")");
            }
            let n = count.unwrap_or(1);
            match op {
                EventOp::AtLeast => qb.push(") >= ").push_bind(n),
                EventOp::AtMost => qb.push(") <= ").push_bind(n),
                EventOp::Exactly => qb.push(") = ").push_bind(n),
                EventOp::Zero => qb.push(") = 0"),
            };
            qb.push(")");
        }
        Condition::List { list_id, op } => {
            if *op == ListOp::NotIn {
                qb.push("NOT ");
            }
            qb.push("EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ")
                .push_bind(*list_id)
                .push(" AND lm.profile_id = p.id)");
        }
        Condition::Consent { channel, status } => {
            let col = match channel {
                Channel::Email => "p.email_consent",
                Channel::Sms => "p.sms_consent",
            };
            qb.push(format!("({col} = ")).push_bind(status.clone()).push(")");
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
        push_condition(qb, c);
    }
    qb.push(")");
}

/// Push a boolean SQL expression for the definition, referencing profile alias `p`.
pub fn push_definition(qb: &mut QueryBuilder<'_, Postgres>, def: &Definition) {
    push_conditions(qb, def.match_, &def.conditions);
}

/// Count profiles in an account matching a definition, plus a sample.
pub async fn preview(
    db: &sqlx::PgPool,
    account_id: Uuid,
    def: &Definition,
    sample: i64,
) -> AppResult<(i64, Vec<crate::models::Profile>)> {
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
    let mut qb = QueryBuilder::new("CREATE TEMP TABLE seg_now ON COMMIT DROP AS SELECT p.id FROM profiles p WHERE p.account_id = ");
    qb.push_bind(account_id).push(" AND ");
    push_definition(&mut qb, def);
    qb.build().execute(&mut *tx).await?;
    sqlx::query("DELETE FROM segment_members sm WHERE sm.segment_id = $1 AND NOT EXISTS (SELECT 1 FROM seg_now s WHERE s.id = sm.profile_id)")
        .bind(segment_id)
        .execute(&mut *tx)
        .await?;
    let entered: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO segment_members (segment_id, profile_id) SELECT $1, id FROM seg_now ON CONFLICT DO NOTHING RETURNING profile_id",
    )
    .bind(segment_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("UPDATE segments SET member_count = (SELECT count(*) FROM segment_members WHERE segment_id = $1), refreshed_at = now() WHERE id = $1")
        .bind(segment_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(entered)
}

/// Evaluate a simple filter against a JSON object (flow trigger filters on event properties).
pub fn json_filter_matches(obj: &Value, f: &Filter) -> bool {
    let got = if f.field == "value" { obj.get("$value").or_else(|| obj.get("value")) } else { obj.pointer(&format!("/{}", f.field.replace('.', "/"))) };
    let want = f.value.clone().unwrap_or(Value::Null);
    let got_s = got.map(value_text).unwrap_or_default();
    let got_n = got.and_then(as_f64);
    let want_n = as_f64(&want);
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
        PropOp::Gt => matches!((got_n, want_n), (Some(a), Some(b)) if a > b),
        PropOp::Gte => matches!((got_n, want_n), (Some(a), Some(b)) if a >= b),
        PropOp::Lt => matches!((got_n, want_n), (Some(a), Some(b)) if a < b),
        PropOp::Lte => matches!((got_n, want_n), (Some(a), Some(b)) if a <= b),
        PropOp::In => want.as_array().is_some_and(|a| a.iter().any(|x| value_text(x).eq_ignore_ascii_case(&got_s))),
        PropOp::WithinLastDays | PropOp::MoreThanDaysAgo => false,
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
    fn rejects_bad_definitions() {
        assert!(parse(&json!({"conditions": [{"type": "property", "field": "x", "op": "eq"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "nope"}]})).is_err());
        assert!(parse(&json!({"conditions": [{"type": "consent", "channel": "email", "status": "maybe"}]})).is_err());
    }

    #[test]
    fn json_filters() {
        let ev = json!({"value": 120.5, "items": ["Shoes"], "source": "web"});
        let f = |field: &str, op: PropOp, value: Value| Filter { field: field.into(), op, value: Some(value) };
        assert!(json_filter_matches(&ev, &f("value", PropOp::Gt, json!(100))));
        assert!(json_filter_matches(&ev, &f("items", PropOp::Contains, json!("shoes"))));
        assert!(json_filter_matches(&ev, &f("source", PropOp::Eq, json!("WEB"))));
        assert!(!json_filter_matches(&ev, &f("source", PropOp::Eq, json!("pos"))));
    }
}
