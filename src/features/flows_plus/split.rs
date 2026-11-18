//! Trigger splits: a conditional split that also tests the event that started the run.
//!
//! A `condition` step may carry `event_condition: {match: all|any, filters: [{field, op, value}]}`.
//! Fields are `properties.<name>` (dots for nested values), `value`, `metric` and `from_browser`
//! (true when the event came from the website script, which anyone can send). The person goes down
//! `yes` only when the event matches and the profile matches the step's `condition` (an empty
//! profile condition matches everyone). A run that no event started (list, segment or date
//! triggers) has no event: `exists` filters are false and `not_exists` filters true.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::flows::Node;
use crate::segments::{self, Filter, Match, PropOp};

pub const MAX_FILTERS: usize = 20;

#[derive(Debug, Deserialize)]
pub struct EventCondition {
    #[serde(rename = "match", default)]
    pub match_: Match,
    #[serde(default)]
    pub filters: Vec<Filter>,
}

fn read(n: &Node) -> AppResult<Option<EventCondition>> {
    match n.data.get("event_condition") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone()).map(Some).map_err(|e| AppError::bad(format!("condition {}: invalid event_condition: {e}", n.id))),
    }
}

/// Check a condition step's `event_condition` when a flow is saved.
pub fn validate(n: &Node) -> AppResult<()> {
    let Some(c) = read(n)? else { return Ok(()) };
    if c.filters.len() > MAX_FILTERS {
        return Err(AppError::bad(format!("condition {}: at most {MAX_FILTERS} event filters", n.id)));
    }
    for f in &c.filters {
        let field_ok = matches!(f.field.as_str(), "value" | "metric" | "from_browser")
            || f.field.strip_prefix("properties.").is_some_and(|p| !p.is_empty() && p.len() <= 100 && !p.split('.').any(str::is_empty));
        if !field_ok {
            return Err(AppError::bad(format!("condition {}: event filter fields are properties.<name>, value, metric or from_browser (got {:?})", n.id, f.field)));
        }
        if matches!(f.op, PropOp::WithinLastDays | PropOp::MoreThanDaysAgo | PropOp::AnniversaryWithinDays) {
            return Err(AppError::bad(format!("condition {}: date operators aren't available for event filters", n.id)));
        }
    }
    Ok(())
}

/// Whether a condition step reads the triggering event.
pub fn reads_event(n: &Node) -> bool {
    matches!(read(n), Ok(Some(c)) if !c.filters.is_empty())
}

/// The event as filters see it.
fn event_view(context: &Value) -> Value {
    match context.get("event") {
        Some(e) if e.is_object() => json!({
            "properties": e.get("properties").cloned().unwrap_or(json!({})),
            "value": e.get("value").cloned().unwrap_or(Value::Null),
            "metric": e.get("metric").cloned().unwrap_or(Value::Null),
            // Unknown counts as from a browser, as everywhere else in flows.
            "from_browser": e.get("from_browser").and_then(Value::as_bool).unwrap_or(true),
        }),
        _ => json!({}),
    }
}

/// Does the run's triggering event pass this condition step's event filters? True when the step has none.
pub fn event_matches(n: &Node, context: &Value) -> bool {
    let Ok(Some(c)) = read(n) else { return true };
    if c.filters.is_empty() {
        return true;
    }
    let ev = event_view(context);
    let hit = |f: &Filter| segments::json_filter_matches(&ev, f);
    match c.match_ {
        Match::All => c.filters.iter().all(hit),
        Match::Any => c.filters.iter().any(hit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(ec: Value) -> Node {
        serde_json::from_value(json!({"id": "c", "type": "condition", "condition": {"conditions": []}, "event_condition": ec, "yes": null, "no": null})).unwrap()
    }

    #[test]
    fn tests_the_triggering_event() {
        let n = node(json!({"match": "all", "filters": [
            {"field": "properties.nights", "op": "gte", "value": 3},
            {"field": "value", "op": "gt", "value": 100},
            {"field": "from_browser", "op": "eq", "value": false}]}));
        validate(&n).unwrap();
        let ctx = |nights: i64, value: f64, browser: bool| json!({"event": {"metric": "Booked", "properties": {"nights": nights}, "value": value, "from_browser": browser}});
        assert!(event_matches(&n, &ctx(4, 250.0, false)));
        assert!(!event_matches(&n, &ctx(2, 250.0, false)));
        assert!(!event_matches(&n, &ctx(4, 50.0, false)));
        assert!(!event_matches(&n, &ctx(4, 250.0, true)), "a browser event fails from_browser = false");
        // No event (list or segment trigger): nothing matches a filter that needs a value.
        assert!(!event_matches(&n, &json!({"list_id": "x"})));
        let any = node(json!({"match": "any", "filters": [{"field": "properties.tier", "op": "eq", "value": "gold"}, {"field": "metric", "op": "eq", "value": "Placed Order"}]}));
        assert!(event_matches(&any, &json!({"event": {"metric": "Placed Order", "properties": {}}})));
        // Unknown from_browser counts as true.
        let trusted = node(json!({"filters": [{"field": "from_browser", "op": "eq", "value": false}]}));
        assert!(!event_matches(&trusted, &json!({"event": {"properties": {}}})));
    }

    #[test]
    fn steps_without_event_filters_match() {
        let n: Node = serde_json::from_value(json!({"id": "c", "type": "condition", "condition": {"conditions": []}, "yes": null, "no": null})).unwrap();
        assert!(event_matches(&n, &json!({})));
        assert!(!reads_event(&n));
    }

    #[test]
    fn rejects_bad_filters() {
        for bad in [
            json!({"filters": [{"field": "nights", "op": "eq", "value": 1}]}),
            json!({"filters": [{"field": "properties.", "op": "eq", "value": 1}]}),
            json!({"filters": [{"field": "properties.at", "op": "within_last_days", "value": 1}]}),
            json!({"filters": "x"}),
        ] {
            assert!(validate(&node(bad.clone())).is_err(), "{bad}");
        }
    }
}
