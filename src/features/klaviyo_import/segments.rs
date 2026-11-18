//! Klaviyo segment definitions → DumbMail segment rules (see `crate::segments`).
//!
//! Klaviyo ANDs its `condition_groups` and ORs the conditions inside each group. A condition is only
//! translated when DumbMail's rule means the same thing; otherwise the whole segment is reported as
//! untranslatable (with every reason) and imported as a snapshot of its current members instead.
//! Event conditions also need the event history to be there: a rule over all time, or reaching back
//! before the imported date range, would quietly count fewer events, so it isn't translated.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

pub struct Ctx<'a> {
    /// Klaviyo metric id → DumbMail metric name, for metrics whose events are imported.
    pub metrics: &'a HashMap<String, String>,
    /// Klaviyo metric id → name, for every metric (to name the ones not imported in reasons).
    pub all_metrics: &'a HashMap<String, String>,
    /// Klaviyo list id → DumbMail list id.
    pub lists: &'a HashMap<String, Uuid>,
    /// Earliest imported event, or None when the whole history is imported.
    pub events_since: Option<DateTime<Utc>>,
    pub now: DateTime<Utc>,
}

type R<T> = Result<T, String>;

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn date_of(v: &Value) -> Option<NaiveDate> {
    let s = v.as_str()?;
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

/// Days for a relative date ("in the last 3 weeks"); hours only when they make whole days.
fn days(f: &Value) -> R<i64> {
    let q = f["quantity"].as_i64().or_else(|| f["value"].as_i64()).ok_or("a relative date without a number")?;
    match f["unit"].as_str().unwrap_or("day") {
        "day" | "days" => Ok(q),
        "week" | "weeks" => Ok(q * 7),
        "hour" | "hours" if q % 24 == 0 => Ok(q / 24),
        u => Err(format!("a time window in {u}s (DumbMail counts whole days)")),
    }
}

fn cmp_op(op: &str) -> R<&'static str> {
    Ok(match op {
        "equals" => "eq",
        "not-equals" => "neq",
        "greater-than" => "gt",
        "greater-than-or-equal" => "gte",
        "less-than" => "lt",
        "less-than-or-equal" => "lte",
        o => return Err(format!("the comparison \"{o}\"")),
    })
}

/// A Klaviyo property filter → (op, value) in DumbMail's property rule language.
fn filter(f: &Value) -> R<(&'static str, Option<Value>)> {
    let op = f["operator"].as_str().unwrap_or("");
    let value = f.get("value").cloned();
    let t = f["type"].as_str().unwrap_or("");
    Ok(match (t, op) {
        (_, "is-set") => ("exists", None),
        (_, "not-set") => ("not_exists", None),
        ("string", "equals") | ("boolean", "equals") => ("eq", value),
        ("string", "not-equals") => ("neq", value),
        ("string", "contains") | ("list", "contains") => ("contains", value),
        ("string", "not-contains") | ("list", "not-contains") => ("not_contains", value),
        ("string", "starts-with") => ("starts_with", value),
        ("string", "ends-with") => ("ends_with", value),
        ("string", "in") => ("in", value),
        ("string", "not-in") => ("not_in", value),
        ("numeric", o) | ("integer", o) => (cmp_op(o)?, value),
        ("date", "after") => ("gt", f.get("date").or(f.get("value")).cloned()),
        ("date", "before") => ("lt", f.get("date").or(f.get("value")).cloned()),
        ("date", "between-static") => ("between", Some(json!([f["start"], f["end"]]))),
        ("date", "in-the-last") => ("within_last_days", Some(json!(days(f)?))),
        ("date", "at-least") => ("more_than_days_ago", Some(json!(days(f)?))),
        ("date", "anniversary-next") => ("anniversary_within_days", Some(json!(days(f)?))),
        (t, o) => return Err(format!("the {t} filter \"{o}\"")),
    })
}

/// Klaviyo's built-in profile fields → DumbMail fields (location and the rest are stored as properties).
fn field(property: &str) -> R<String> {
    if let Some(rest) = property.strip_prefix("properties[").and_then(|r| r.strip_suffix(']')) {
        return Ok(rest.trim_matches(['\'', '"']).to_string());
    }
    let p = property.trim_start_matches("location.").trim_start_matches('$');
    Ok(match p {
        "email" | "phone_number" | "first_name" | "last_name" | "external_id" => p.to_string(),
        "created" => "created_at".to_string(),
        "organization" | "title" | "locale" | "city" | "region" | "country" | "zip" | "timezone" | "address1" | "address2" => p.to_string(),
        other => return Err(format!("the profile field \"{other}\"")),
    })
}

/// The event history a rule needs must fall inside what was imported.
fn window(ctx: &Ctx, tf: &Value, what: &str) -> R<Value> {
    let op = tf["operator"].as_str().unwrap_or("");
    let covered = |from: DateTime<Utc>| ctx.events_since.is_none_or(|s| from >= s);
    let since_text = || ctx.events_since.map(|s| s.format("%Y-%m-%d").to_string()).unwrap_or_default();
    match op {
        "alltime" | "" if ctx.events_since.is_none() => Ok(json!({})),
        "alltime" | "" => Err(format!("{what} over all time, but events were imported only from {}", since_text())),
        "in-the-last" => {
            let d = days(tf)?;
            if covered(ctx.now - chrono::Duration::days(d)) {
                Ok(json!({"within_days": d}))
            } else {
                Err(format!("{what} in the last {d} days, but events were imported only from {}", since_text()))
            }
        }
        "after" => {
            let d = date_of(tf.get("date").unwrap_or(&tf["value"])).ok_or("a date we couldn't read")?;
            if covered(d.and_hms_opt(0, 0, 0).unwrap().and_utc()) {
                Ok(json!({"since": d.to_string()}))
            } else {
                Err(format!("{what} since {d}, but events were imported only from {}", since_text()))
            }
        }
        "between-static" => {
            let a = date_of(&tf["start"]).ok_or("a date we couldn't read")?;
            let b = date_of(&tf["end"]).ok_or("a date we couldn't read")?;
            if covered(a.and_hms_opt(0, 0, 0).unwrap().and_utc()) {
                Ok(json!({"since": a.to_string(), "until": b.to_string()}))
            } else {
                Err(format!("{what} from {a}, but events were imported only from {}", since_text()))
            }
        }
        "before" if ctx.events_since.is_none() => {
            let d = date_of(tf.get("date").unwrap_or(&tf["value"])).ok_or("a date we couldn't read")?;
            Ok(json!({"until": (d - chrono::Duration::days(1)).to_string()}))
        }
        o => Err(format!("{what} with the time window \"{o}\"")),
    }
}

fn metric_condition(ctx: &Ctx, c: &Value) -> R<Value> {
    let id = c["metric_id"].as_str().unwrap_or("");
    let name = ctx.metrics.get(id).ok_or_else(|| {
        let n = ctx.all_metrics.get(id).cloned().unwrap_or_else(|| id.to_string());
        format!("the metric \"{n}\", whose events weren't imported")
    })?;
    let mut win = window(ctx, &c["timeframe_filter"], &format!("\"{name}\""))?;
    let mut wheres = Vec::new();
    for mf in c["metric_filters"].as_array().into_iter().flatten() {
        let prop = mf["property"].as_str().unwrap_or("");
        let field = match prop {
            "$value" | "value" => "value".to_string(),
            p => p.to_string(),
        };
        let (op, value) = filter(&mf["filter"])?;
        let mut w = json!({"field": field, "op": op});
        if let Some(v) = value {
            w["value"] = v;
        }
        wheres.push(w);
    }
    let mf = &c["measurement_filter"];
    let n = num(&mf["value"]).ok_or("a count without a number")?;
    let op = mf["operator"].as_str().unwrap_or("");
    let mut out = match c["measurement"].as_str().unwrap_or("count") {
        "count" => {
            let n = n.round() as i64;
            let (op, count) = match op {
                "greater-than" => ("at_least", n + 1),
                "greater-than-or-equal" => ("at_least", n),
                "equals" if n == 0 => ("zero", 0),
                "equals" => ("exactly", n),
                "less-than" if n <= 1 => ("zero", 0),
                "less-than" => ("at_most", n - 1),
                "less-than-or-equal" if n == 0 => ("zero", 0),
                "less-than-or-equal" => ("at_most", n),
                o => return Err(format!("a count compared with \"{o}\"")),
            };
            json!({"type": "event", "metric": name, "op": op, "count": count})
        }
        "sum" => json!({"type": "event_value", "metric": name, "aggregate": "sum", "op": cmp_op(op)?, "value": n}),
        m => return Err(format!("the measurement \"{m}\"")),
    };
    if let (Some(o), Some(w)) = (out.as_object_mut(), win.as_object_mut()) {
        o.append(w);
    }
    if !wheres.is_empty() {
        out["where"] = Value::Array(wheres);
    }
    Ok(out)
}

fn consent_condition(c: &Value) -> R<Value> {
    let consent = &c["consent"];
    let channel = consent["channel"].as_str().unwrap_or("");
    let can = consent["can_receive_marketing"].as_bool().unwrap_or(true);
    let status = &consent["consent_status"];
    let sub = status["subscription"].as_str().unwrap_or("any");
    if status["filters"].as_array().is_some_and(|f| !f.is_empty()) {
        return Err(format!("{channel} consent filtered by method, date or suppression"));
    }
    let ch = match channel {
        "email" => "email",
        "sms" => "sms",
        other => return Err(format!("{other} consent")),
    };
    let can_receive = json!({"type": "can_receive", "channel": ch, "value": can});
    Ok(match (can, sub) {
        (_, "any") => can_receive,
        (true, s @ ("subscribed" | "never_subscribed")) => json!({"type": "group", "match": "all", "conditions": [{"type": "consent", "channel": ch, "status": s}, can_receive]}),
        (false, s @ ("unsubscribed" | "never_subscribed")) => json!({"type": "group", "match": "all", "conditions": [{"type": "consent", "channel": ch, "status": s}, can_receive]}),
        (_, s) => return Err(format!("{ch} consent \"{s}\"")),
    })
}

fn condition(ctx: &Ctx, c: &Value) -> R<Value> {
    match c["type"].as_str().unwrap_or("") {
        "profile-property" => {
            let f = field(c["property"].as_str().unwrap_or(""))?;
            let (op, value) = filter(&c["filter"])?;
            let mut out = json!({"type": "property", "field": f, "op": op});
            if let Some(v) = value {
                out["value"] = v;
            }
            Ok(out)
        }
        "profile-metric" => metric_condition(ctx, c),
        "profile-group-membership" => {
            if c.get("timeframe_filter").is_some_and(|t| !t.is_null()) {
                return Err("list membership within a time window".into());
            }
            let member = c["is_member"].as_bool().unwrap_or(true);
            let mut parts = Vec::new();
            for g in c["group_ids"].as_array().into_iter().flatten() {
                let gid = g.as_str().unwrap_or("");
                let list = ctx.lists.get(gid).ok_or_else(|| format!("the list or segment {gid}, which wasn't imported as a list"))?;
                parts.push(json!({"type": "list", "list_id": list, "op": if member { "in" } else { "not_in" }}));
            }
            match parts.len() {
                0 => Err("a membership rule without a list".into()),
                1 => Ok(parts.remove(0)),
                _ => Ok(json!({"type": "group", "match": if member { "any" } else { "all" }, "conditions": parts})),
            }
        }
        "profile-marketing-consent" => consent_condition(c),
        "profile-region" => Err("the region rule (EU / US)".into()),
        "profile-postal-code-distance" => Err("distance from a postal code".into()),
        "profile-predictive-analytics" => Err("Klaviyo's predictive analytics (DumbMail computes its own predictions differently)".into()),
        "profile-random-sample" | "profile-sample" => Err("Klaviyo's random sample".into()),
        "profile-has-custom-object" => Err("custom objects".into()),
        "profile-permissions" => Err("reachability permissions".into()),
        other => Err(format!("the rule type \"{other}\"")),
    }
}

/// Translate a Klaviyo `definition`. On failure, every reason found.
pub fn translate(ctx: &Ctx, definition: &Value) -> Result<Value, Vec<String>> {
    let mut groups = Vec::new();
    let mut reasons = Vec::new();
    for g in definition["condition_groups"].as_array().into_iter().flatten() {
        let mut ors = Vec::new();
        for c in g["conditions"].as_array().into_iter().flatten() {
            match condition(ctx, c) {
                Ok(x) => ors.push(x),
                Err(r) => reasons.push(r),
            }
        }
        match ors.len() {
            0 => {}
            1 => groups.push(ors.remove(0)),
            _ => groups.push(json!({"type": "group", "match": "any", "conditions": ors})),
        }
    }
    if !reasons.is_empty() {
        reasons.dedup();
        return Err(reasons);
    }
    if groups.is_empty() {
        return Err(vec!["no rules we could read".into()]);
    }
    let def = json!({"match": "all", "conditions": groups});
    // DumbMail's own validator must accept it.
    crate::segments::parse(&def).map_err(|e| vec![e.public_message()])?;
    Ok(def)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(m: &'a HashMap<String, String>, l: &'a HashMap<String, Uuid>, since: Option<DateTime<Utc>>) -> Ctx<'a> {
        Ctx { metrics: m, all_metrics: m, lists: l, events_since: since, now: Utc::now() }
    }

    #[test]
    fn translates_what_maps() {
        let m: HashMap<String, String> = [("PO".to_string(), "Placed Order".to_string())].into();
        let lid = Uuid::now_v7();
        let l: HashMap<String, Uuid> = [("L1".to_string(), lid)].into();
        let c = ctx(&m, &l, Some(Utc::now() - chrono::Duration::days(730)));
        let def = json!({"condition_groups": [
            {"conditions": [{"type": "profile-metric", "metric_id": "PO", "measurement": "count",
                "measurement_filter": {"type": "numeric", "operator": "greater-than", "value": 1},
                "timeframe_filter": {"type": "date", "operator": "in-the-last", "quantity": 90, "unit": "day"},
                "metric_filters": [{"property": "$value", "filter": {"type": "numeric", "operator": "greater-than", "value": 20}}]}]},
            {"conditions": [{"type": "profile-property", "property": "properties['Favorite Color']", "filter": {"type": "string", "operator": "equals", "value": "blue"}},
                            {"type": "profile-property", "property": "email", "filter": {"type": "string", "operator": "ends-with", "value": "@hotmail.com"}}]},
            {"conditions": [{"type": "profile-group-membership", "is_member": true, "group_ids": ["L1"]}]},
            {"conditions": [{"type": "profile-marketing-consent", "consent": {"channel": "email", "can_receive_marketing": true, "consent_status": {"subscription": "subscribed"}}}]}
        ]});
        let out = translate(&c, &def).unwrap();
        let conds = out["conditions"].as_array().unwrap();
        assert_eq!(conds[0], json!({"type": "event", "metric": "Placed Order", "op": "at_least", "count": 2, "within_days": 90, "where": [{"field": "value", "op": "gt", "value": 20}]}));
        assert_eq!(conds[1]["match"], "any");
        assert_eq!(conds[1]["conditions"][0], json!({"type": "property", "field": "Favorite Color", "op": "eq", "value": "blue"}));
        assert_eq!(conds[2], json!({"type": "list", "list_id": lid, "op": "in"}));
        assert_eq!(conds[3]["conditions"][0], json!({"type": "consent", "channel": "email", "status": "subscribed"}));
    }

    #[test]
    fn refuses_what_would_change_meaning() {
        let m: HashMap<String, String> = [("PO".to_string(), "Placed Order".to_string())].into();
        let l = HashMap::new();
        let since = Some(Utc::now() - chrono::Duration::days(730));
        let c = ctx(&m, &l, since);
        let all_time = json!({"condition_groups": [{"conditions": [{"type": "profile-metric", "metric_id": "PO", "measurement": "count",
            "measurement_filter": {"type": "numeric", "operator": "greater-than-or-equal", "value": 1}, "timeframe_filter": {"type": "date", "operator": "alltime"}}]}]});
        let e = translate(&c, &all_time).unwrap_err();
        assert!(e[0].contains("over all time"), "{e:?}");
        // With the whole history imported it maps.
        let c_all = ctx(&m, &l, None);
        assert_eq!(translate(&c_all, &all_time).unwrap()["conditions"][0]["op"], "at_least");
        let too_far = json!({"condition_groups": [{"conditions": [{"type": "profile-metric", "metric_id": "PO", "measurement": "count",
            "measurement_filter": {"type": "numeric", "operator": "equals", "value": 0}, "timeframe_filter": {"type": "date", "operator": "in-the-last", "quantity": 1000, "unit": "day"}}]}]});
        assert!(translate(&c, &too_far).is_err());
        let other = json!({"condition_groups": [{"conditions": [
            {"type": "profile-predictive-analytics", "dimension": "churn_probability"},
            {"type": "profile-metric", "metric_id": "XX", "measurement": "count", "measurement_filter": {"type": "numeric", "operator": "greater-than", "value": 0}, "timeframe_filter": {"type": "date", "operator": "in-the-last", "quantity": 3, "unit": "day"}},
            {"type": "profile-property", "property": "email", "filter": {"type": "string", "operator": "regex", "value": ".*"}},
            {"type": "profile-group-membership", "is_member": true, "group_ids": ["NOPE"]}
        ]}]});
        let e = translate(&c, &other).unwrap_err();
        assert_eq!(e.len(), 4, "{e:?}");
        let filtered_consent = json!({"condition_groups": [{"conditions": [{"type": "profile-marketing-consent", "consent": {"channel": "email", "can_receive_marketing": true,
            "consent_status": {"subscription": "subscribed", "filters": [{"field": "method", "method": "form"}]}}}]}]});
        assert!(translate(&c, &filtered_consent).is_err());
    }
}
