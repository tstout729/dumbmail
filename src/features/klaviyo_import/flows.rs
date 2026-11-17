//! Klaviyo flow definitions → DumbMail flow drafts.
//!
//! The trigger, delays, emails, texts, conditional splits, profile updates, list updates and webhooks
//! are recreated. A step with no DumbMail equivalent is skipped (its path continues to the next step)
//! and listed; a split whose rules don't translate sends everyone down "Yes" until someone fixes it.
//! Any of those makes the flow "partial", and its name says so. Imported flows are always drafts.

use std::collections::HashMap;

use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::segments as seg;

pub struct Ctx<'a> {
    /// Every Klaviyo metric id → name (a flow can trigger on any metric).
    pub metric_names: &'a HashMap<String, String>,
    pub seg: seg::Ctx<'a>,
    /// Klaviyo segment id → DumbMail segment id (only segments translated as live rules).
    pub segments: &'a HashMap<String, Uuid>,
    /// Klaviyo template id → converted DumbMail email content.
    pub templates: &'a HashMap<String, Value>,
}

#[derive(Debug)]
pub struct Translated {
    pub trigger: Value,
    pub definition: Value,
    pub issues: Vec<String>,
}

impl Translated {
    pub fn partial(&self) -> bool {
        !self.issues.is_empty()
    }
}

/// Template ids a flow's emails use, so the caller can load them first.
pub fn template_ids(def: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for a in def["actions"].as_array().into_iter().flatten() {
        for m in [&a["data"]["message"], &a["data"]["main_action"]["data"]["message"]] {
            if let Some(t) = m["template_id"].as_str() {
                out.push(t.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn days_of(value: i64, unit: &str) -> Option<i64> {
    match unit {
        "day" | "days" => Some(value),
        "week" | "weeks" => Some(value * 7),
        "month" | "months" => Some(value * 30),
        _ => None,
    }
}

fn trigger(ctx: &Ctx, def: &Value, issues: &mut Vec<String>) -> Result<Value, String> {
    let t = def["triggers"].get(0).ok_or("it has no trigger")?;
    match t["type"].as_str().unwrap_or("") {
        "metric" => {
            let id = t["id"].as_str().unwrap_or("");
            let name = ctx.metric_names.get(id).ok_or_else(|| format!("its trigger metric {id} wasn't found"))?;
            let mut filters = Vec::new();
            for g in t["trigger_filter"]["condition_groups"].as_array().into_iter().flatten() {
                let conds = g["conditions"].as_array().cloned().unwrap_or_default();
                // DumbMail trigger filters are ANDed; a Klaviyo group with several ORed rules doesn't fit.
                if conds.len() != 1 {
                    issues.push("A trigger filter with \"or\" rules was left out, so more people may enter. Add it back by hand.".into());
                    continue;
                }
                let c = &conds[0];
                let field = c["field"].as_str().or(c["property"].as_str()).unwrap_or("").trim_start_matches('$');
                let field = if field == "value" { "value".to_string() } else { field.to_string() };
                match translate_filter(&c["filter"]) {
                    Some((op, value)) if !field.is_empty() => {
                        let mut f = json!({"field": field, "op": op});
                        if let Some(v) = value {
                            f["value"] = v;
                        }
                        filters.push(f);
                    }
                    _ => issues.push(format!("The trigger filter on \"{field}\" couldn't be translated and was left out, so more people may enter.")),
                }
            }
            Ok(json!({"type": "metric", "metric": name, "filters": filters}))
        }
        "list" => {
            let id = t["id"].as_str().unwrap_or("");
            let list = ctx.seg.lists.get(id).ok_or_else(|| format!("its trigger list {id} wasn't imported"))?;
            Ok(json!({"type": "list", "list_id": list}))
        }
        "segment" => {
            let id = t["id"].as_str().unwrap_or("");
            let s = ctx.segments.get(id).ok_or_else(|| "it starts when people join a segment that was imported as a snapshot list (a snapshot never changes, so the flow would never start)".to_string())?;
            Ok(json!({"type": "segment", "segment_id": s}))
        }
        "date" if t["date_field_type"] == "profile-property" => {
            let prop = t["date_profile_property"].as_str().unwrap_or("");
            let prop = prop.strip_prefix("properties[").and_then(|p| p.strip_suffix(']')).unwrap_or(prop).trim_matches(['\'', '"']).trim_start_matches('$');
            let unit = t["timedelta_unit_before_date"].as_str().unwrap_or("weeks");
            let v = t["timedelta_value_before_date"].as_i64().unwrap_or(0);
            let before = days_of(v, unit).ok_or_else(|| format!("its date trigger counts in {unit}"))?;
            if unit.starts_with("month") && v != 0 {
                issues.push(format!("The date trigger was {v} month(s) before the date; it's now {before} days before."));
            }
            if t["recurrence_frequency"].as_str().is_some_and(|r| r != "annually") {
                issues.push("The date trigger repeated other than yearly; DumbMail date flows run on the date each year.".into());
            }
            Ok(json!({"type": "date_property", "field": prop, "offset_days": -before}))
        }
        other => Err(format!("its trigger type \"{other}\" has no DumbMail equivalent")),
    }
}

fn translate_filter(f: &Value) -> Option<(&'static str, Option<Value>)> {
    let op = f["operator"].as_str()?;
    let v = f.get("value").cloned();
    Some(match op {
        "equals" => ("eq", v),
        "not-equals" => ("neq", v),
        "contains" => ("contains", v),
        "not-contains" => ("not_contains", v),
        "greater-than" => ("gt", v),
        "greater-than-or-equal" => ("gte", v),
        "less-than" => ("lt", v),
        "less-than-or-equal" => ("lte", v),
        "is-set" => ("exists", None),
        "not-set" => ("not_exists", None),
        "in" => ("in", v),
        "not-in" => ("not_in", v),
        _ => return None,
    })
}

fn email_content(ctx: &Ctx, m: &Value, issues: &mut Vec<String>, step: &str) -> Value {
    let mut content = m["template_id"].as_str().and_then(|t| ctx.templates.get(t)).cloned().unwrap_or_else(|| {
        issues.push(format!("{step}: its email design wasn't found in Klaviyo, so the email is empty."));
        json!({"blocks": []})
    });
    let conv = |s: &str| super::templates::convert(s).text;
    if let Some(s) = m["subject_line"].as_str() {
        content["subject"] = conv(s).into();
    }
    if let Some(s) = m["preview_text"].as_str().filter(|s| !s.is_empty()) {
        content["preheader"] = conv(s).into();
    }
    for (k, d) in [("from_label", "from_name"), ("from_email", "from_email"), ("reply_to_email", "reply_to")] {
        if let Some(v) = m[k].as_str().filter(|v| !v.is_empty()) {
            content[d] = v.into();
        }
    }
    content
}

pub fn translate(ctx: &Ctx, def: &Value) -> Result<Translated, String> {
    let mut issues = Vec::new();
    let trigger = trigger(ctx, def, &mut issues)?;
    let actions = def["actions"].as_array().cloned().unwrap_or_default();
    if actions.len() > 200 {
        return Err("it has more than 200 steps".into());
    }
    let nid = |id: &Value| id.as_str().map(|s| format!("k{s}")).or_else(|| id.as_i64().map(|n| format!("k{n}")));
    let mut nodes: Vec<Map<String, Value>> = Vec::new();
    // Skipped steps forward to where their path went next.
    let mut forward: HashMap<String, Option<String>> = HashMap::new();
    let mut smart = false;
    for a in &actions {
        let id = nid(&a["id"]).unwrap_or_else(|| format!("k{}", nodes.len() + forward.len()));
        let links = &a["links"];
        let next = nid(&links["next"]);
        let data = &a["data"];
        let kind = a["type"].as_str().unwrap_or("");
        let step = format!("Step \"{}\"", data["name"].as_str().or(data["message"]["name"].as_str()).unwrap_or(kind));
        let mut n = Map::new();
        n.insert("id".into(), json!(id));
        let mut add_message = |n: &mut Map<String, Value>, k: &str, d: &Value, issues: &mut Vec<String>| -> bool {
            let m = &d["message"];
            if m["smart_sending_enabled"].as_bool() == Some(true) {
                smart = true;
            }
            if matches!(d["status"].as_str(), Some("disabled") | Some("manual")) {
                issues.push(format!("{step} was {} in Klaviyo.", d["status"].as_str().unwrap_or("")));
            }
            match k {
                "send-email" => {
                    n.insert("type".into(), json!("email"));
                    if let Some(name) = m["name"].as_str() {
                        n.insert("name".into(), json!(name));
                    }
                    n.insert("content".into(), email_content(ctx, m, issues, &step));
                    true
                }
                "send-sms" => {
                    n.insert("type".into(), json!("sms"));
                    let conv = super::templates::convert(m["body"].as_str().unwrap_or(""));
                    if !conv.removed.is_empty() {
                        issues.push(format!("{step}: removed {} from the text.", conv.removed.join(", ")));
                    }
                    n.insert("content".into(), json!({"body": conv.text}));
                    true
                }
                _ => false,
            }
        };
        let kept = match kind {
            "time-delay" => {
                let unit = data["unit"].as_str().unwrap_or("days");
                let unit = if matches!(unit, "days" | "hours" | "minutes") { unit } else { "days" };
                n.insert("type".into(), json!("delay"));
                n.insert("amount".into(), json!(data["value"].as_i64().unwrap_or(0).max(0)));
                n.insert("unit".into(), json!(unit));
                if data["delay_until_time"].is_string() || data["delay_until_weekdays"].as_array().is_some_and(|w| !w.is_empty()) {
                    issues.push(format!("{step} also waited for a time of day or a weekday; it's now a plain delay."));
                }
                true
            }
            "send-email" | "send-sms" => add_message(&mut n, kind, data, &mut issues),
            "ab-test" => {
                let main = &data["main_action"];
                let k = main["type"].as_str().unwrap_or("");
                let ok = add_message(&mut n, k, &main["data"], &mut issues);
                if ok {
                    issues.push(format!("{step} was an A/B test; only its main version was imported."));
                }
                ok
            }
            "conditional-split" | "trigger-split" => {
                n.insert("type".into(), json!("condition"));
                let rules = if kind == "conditional-split" { seg::translate(&ctx.seg, &data["profile_filter"]) } else { Err(vec!["splits on the triggering event's details".into()]) };
                let cond = rules.unwrap_or_else(|why| {
                    issues.push(format!("{step}: its rules couldn't be translated ({}). Everyone goes down \"Yes\" until you set the rules.", why.join("; ")));
                    json!({"match": "all", "conditions": []})
                });
                n.insert("condition".into(), cond);
                n.insert("yes".into(), json!(nid(&links["next_if_true"])));
                n.insert("no".into(), json!(nid(&links["next_if_false"])));
                true
            }
            "update-profile" => {
                let mut props = Map::new();
                for op in data["profile_operations"].as_array().into_iter().flatten() {
                    let key = op["property_key"].as_str().unwrap_or("");
                    match (op["operator"].as_str(), op["property_type"].as_str()) {
                        (Some("create" | "update"), Some(t)) if t != "list" && !key.is_empty() => {
                            props.insert(key.to_string(), op["property_value"].clone());
                        }
                        _ => issues.push(format!("{step}: the change to \"{key}\" couldn't be translated.")),
                    }
                }
                n.insert("type".into(), json!("update_profile"));
                n.insert("properties".into(), Value::Object(props));
                true
            }
            "list-update" => match data["list_id"].as_str().and_then(|l| ctx.seg.lists.get(l)) {
                Some(list) => {
                    let add = data["on_execution"].as_bool().unwrap_or(true) || data["on_execution"] == "add";
                    n.insert("type".into(), json!(if add { "add_to_list" } else { "remove_from_list" }));
                    n.insert("list_id".into(), json!(list));
                    true
                }
                None => {
                    issues.push(format!("{step}: its list wasn't imported, so the step was skipped."));
                    false
                }
            },
            "send-webhook" => {
                let m = &data["message"];
                match m["url"].as_str().filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
                    Some(url) => {
                        n.insert("type".into(), json!("webhook"));
                        n.insert("url".into(), json!(url));
                        n.insert("method".into(), json!("POST"));
                        if let Some(b) = m.get("body").filter(|b| !b.is_null()) {
                            n.insert("body".into(), b.clone());
                        }
                        if m["headers"].as_array().is_some_and(|h| !h.is_empty()) || m["headers"].as_object().is_some_and(|h| !h.is_empty()) {
                            issues.push(format!("{step}: its custom headers weren't imported (they often hold secrets). Add them back by hand."));
                        }
                        true
                    }
                    None => {
                        issues.push(format!("{step}: a webhook without a web address was skipped."));
                        false
                    }
                }
            }
            other => {
                let what = match other {
                    "send-mobile-push" => "a push notification",
                    "send-whatsapp" | "send-whatsapp-with-ac" => "a WhatsApp message",
                    "send-internal-alert" => "an internal alert email",
                    "multi-branch-split" => "a split with more than two paths (only the first path was kept)",
                    "action-output-split" => "a split on another step's output",
                    "target-date" | "countdown-delay" | "back-in-stock-delay" => "a wait for a date or back-in-stock",
                    "code" => "custom code",
                    "internal-service" => "an internal Klaviyo action",
                    "content-experiment" => "a content experiment",
                    _ => "a step type DumbMail doesn't have",
                };
                issues.push(format!("{step} was {what} and was skipped."));
                false
            }
        };
        if kept {
            if !n.contains_key("yes") {
                n.insert("next".into(), json!(next));
            }
            nodes.push(n);
        } else {
            let next = if kind == "multi-branch-split" {
                data["branches"].as_array().and_then(|b| b.first()).and_then(|b| nid(&b["links"]["next"]))
            } else {
                next.or_else(|| nid(&links["next_if_true"]))
            };
            forward.insert(id, next);
        }
    }
    let resolve = |mut t: Option<String>| -> Option<String> {
        for _ in 0..250 {
            match t.as_ref().and_then(|x| forward.get(x)) {
                Some(n) => t = n.clone(),
                None => return t,
            }
        }
        None
    };
    for n in &mut nodes {
        for k in ["next", "yes", "no"] {
            if let Some(v) = n.get(k).cloned() {
                n.insert(k.into(), json!(resolve(v.as_str().map(String::from))));
            }
        }
    }
    let entry = resolve(nid(&def["entry_action_id"]).or_else(|| nodes.first().and_then(|n| n["id"].as_str().map(String::from))));
    let mut settings = json!({});
    if smart {
        settings["smart_sending_hours"] = json!(16);
    }
    if let Some(r) = def["reentry_criteria"].as_object() {
        let d = r.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
        match r.get("unit").and_then(|u| u.as_str()).and_then(|u| days_of(d, u).or(if u.starts_with("hour") { Some((d + 23) / 24) } else { None })) {
            Some(days) if days > 0 => settings["reentry"] = json!({"after_days": days}),
            _ => settings["reentry"] = json!("always"),
        }
    }
    let pf = &def["profile_filter"];
    if pf["condition_groups"].as_array().is_some_and(|g| !g.is_empty()) {
        match seg::translate(&ctx.seg, pf) {
            Ok(rules) => settings["profile_filter"] = rules,
            Err(why) => issues.push(format!("The flow filter couldn't be translated ({}) and was left out, so more people may enter.", why.join("; "))),
        }
    }
    let definition = json!({"entry": entry, "nodes": nodes, "settings": settings});
    if let Err(e) = crate::flows::parse_definition(&definition) {
        return Err(format!("its steps couldn't be put back together ({})", e.public_message()));
    }
    Ok(Translated { trigger, definition, issues })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilds_a_welcome_series() {
        let metrics: HashMap<String, String> = [("M1".to_string(), "Placed Order".to_string())].into();
        let lid = Uuid::now_v7();
        let lists: HashMap<String, Uuid> = [("L1".to_string(), lid)].into();
        let segs = HashMap::new();
        let templates: HashMap<String, Value> = [("T1".to_string(), json!({"subject": "", "html": "<html><body>Hi</body></html>"}))].into();
        let ctx = Ctx { metric_names: &metrics, seg: seg::Ctx { metrics: &metrics, all_metrics: &metrics, lists: &lists, events_since: None, now: chrono::Utc::now() }, segments: &segs, templates: &templates };
        let def = json!({
            "triggers": [{"type": "list", "id": "L1"}],
            "entry_action_id": "1",
            "actions": [
                {"id": "1", "type": "send-email", "links": {"next": "2"}, "data": {"status": "live", "message": {"name": "Welcome", "subject_line": "Hi {{ first_name|default:'there' }}", "template_id": "T1", "smart_sending_enabled": true}}},
                {"id": "2", "type": "time-delay", "links": {"next": "3"}, "data": {"unit": "days", "value": 2}},
                {"id": "3", "type": "send-mobile-push", "links": {"next": "4"}, "data": {"message": {"body": "x"}}},
                {"id": "4", "type": "conditional-split", "links": {"next_if_true": "5", "next_if_false": null},
                 "data": {"profile_filter": {"condition_groups": [{"conditions": [{"type": "profile-metric", "metric_id": "M1", "measurement": "count",
                    "measurement_filter": {"type": "numeric", "operator": "equals", "value": 0}, "timeframe_filter": {"type": "date", "operator": "alltime"}}]}]}}},
                {"id": "5", "type": "send-sms", "links": {"next": null}, "data": {"message": {"body": "Hi {{ first_name }}! {% web_view %}"}}}
            ]
        });
        let t = translate(&ctx, &def).unwrap();
        assert_eq!(t.trigger, json!({"type": "list", "list_id": lid}));
        let nodes = t.definition["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 4);
        assert_eq!(nodes[0]["content"]["subject"], "Hi {{ first_name|default('there') }}");
        // The push step was skipped; the delay now goes straight to the split.
        assert_eq!(nodes[1]["next"], "k4");
        assert_eq!(nodes[2]["condition"]["conditions"][0]["op"], "zero");
        assert_eq!(nodes[3]["content"]["body"], "Hi {{ first_name }}! ");
        assert_eq!(t.definition["settings"]["smart_sending_hours"], 16);
        assert!(t.partial());
        assert_eq!(t.issues.len(), 2, "{:?}", t.issues);
    }

    #[test]
    fn refuses_triggers_without_an_equivalent() {
        let m = HashMap::new();
        let l = HashMap::new();
        let s = HashMap::new();
        let tp = HashMap::new();
        let ctx = Ctx { metric_names: &m, seg: seg::Ctx { metrics: &m, all_metrics: &m, lists: &l, events_since: None, now: chrono::Utc::now() }, segments: &s, templates: &tp };
        assert!(translate(&ctx, &json!({"triggers": [{"type": "price-drop"}], "actions": []})).is_err());
        assert!(translate(&ctx, &json!({"triggers": [{"type": "segment", "id": "S"}], "actions": []})).unwrap_err().contains("snapshot"));
    }
}
