//! Removing fields from a payload before it is stored or shown (door codes, access notes).
//!
//! An integration (or its recipe) lists paths; each is dot-separated segments:
//!
//! - `name`: that key of an object;
//! - `name[]`: that key, then each item of the array it holds;
//! - `[]`: each item of an array;
//! - `*`: every key of an object;
//! - `**`: any depth (zero or more levels of objects and arrays).
//!
//! The last segment must be a key name (`name` or `name[]`), which is removed. For example
//! `results[].notes`, `reservation.notes`, `**.keyCode`.
//!
//! **Keep-only mode.** Paths starting with `keep:` turn the list into an allow-list: only what
//! they name is kept (then the other paths are removed from that). A key segment may filter the
//! items of an array, `name[sub/key=a|b]` (items whose `sub.key` is `a` or `b`), and
//! `{setting:KEY}` stands for the integration's setting `KEY` (a text, or each item of a list), so
//! what's kept can follow what the installer mapped: `keep:payload.data.{setting:email_field}`,
//! `keep:form_response.answers[field/ref={setting:copy_fields}]`. A path whose setting is empty
//! keeps nothing. `**` can't be used in a keep path.
//!
//! What's kept is all that's stored, and a webhook's script runs on what was stored, so a list
//! must keep everything the script reads.

use serde_json::{Map, Value};

/// Items of an array whose value at `at` (a path of keys) is one of `values`.
#[derive(Debug, Clone, PartialEq)]
struct Filter {
    at: Vec<String>,
    values: Vec<String>,
}

impl Filter {
    fn matches(&self, item: &Value) -> bool {
        let mut cur = item;
        for k in &self.at {
            match cur.get(k) {
                Some(c) => cur = c,
                None => return false,
            }
        }
        let text = match cur {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => return false,
        };
        self.values.iter().any(|v| *v == text)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key { name: String, each: bool, filter: Option<Filter> },
    Each,
    Any,
    Deep,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    segs: Vec<Seg>,
    keep: bool,
}

impl Path {
    /// A `keep:` path (part of an allow-list).
    pub fn is_keep(&self) -> bool {
        self.keep
    }

    pub fn parse(raw: &str) -> Result<Path, String> {
        let raw = raw.trim();
        if raw.is_empty() || raw.len() > 200 {
            return Err("each redact path must be 1-200 characters".into());
        }
        let (keep, body) = match raw.strip_prefix("keep:") {
            Some(b) => (true, b),
            None => (false, raw),
        };
        let mut segs = Vec::new();
        for part in body.split('.') {
            let seg = match part {
                "[]" => Seg::Each,
                "*" => Seg::Any,
                "**" if keep => return Err(format!("keep path {raw:?} can't use **")),
                "**" => Seg::Deep,
                p => {
                    let (name, each, filter) = if let Some(n) = p.strip_suffix("[]") {
                        (n, true, None)
                    } else if let (Some(open), true) = (p.find('['), p.ends_with(']')) {
                        let inner = &p[open + 1..p.len() - 1];
                        let (at, values) = inner.split_once('=').ok_or_else(|| format!("redact path {raw:?}: a filter looks like name[key=value]"))?;
                        let at: Vec<String> = at.split('/').map(str::to_string).collect();
                        if at.iter().any(|k| k.is_empty() || k.len() > 100) || values.is_empty() {
                            return Err(format!("redact path {raw:?}: a filter looks like name[key=value] or name[sub/key=a|b]"));
                        }
                        (&p[..open], true, Some(Filter { at, values: values.split('|').map(str::to_string).collect() }))
                    } else {
                        (p, false, None)
                    };
                    if name.is_empty() || name.len() > 100 || name.contains(['[', ']', '*']) {
                        return Err(format!("redact path {raw:?}: {part:?} isn't a field name, name[], name[key=value], [], * or **"));
                    }
                    Seg::Key { name: name.to_string(), each, filter }
                }
            };
            segs.push(seg);
        }
        if !matches!(segs.last(), Some(Seg::Key { .. })) {
            return Err(format!("redact path {raw:?} must end with a field name (the field to remove or keep)"));
        }
        Ok(Path { segs, keep })
    }
}

/// Parse a list of paths, skipping (never failing on) any that don't parse: they were checked
/// when saved. `{setting:KEY}` placeholders are left as they are (see [`paths_with`]).
pub fn paths(list: &[String]) -> Vec<Path> {
    list.iter().filter_map(|p| Path::parse(p).ok()).collect()
}

/// Like [`paths`], with `{setting:KEY}` filled in from the integration's settings: inside a
/// filter, every value of the setting (`a|b`); elsewhere, one path per value. A value that can't
/// be part of a path (it holds `.`, `[`, `]`, `|`, `=` or `/`) is skipped, and a path whose setting
/// has no usable value is dropped.
pub fn paths_with(list: &[String], settings: &Map<String, Value>) -> Vec<Path> {
    static PH: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"\{setting:([A-Za-z0-9_-]{1,64})\}").unwrap());
    let values = |k: &str| -> Vec<String> {
        // A value can't hold another placeholder (no braces), so each pass removes one for good.
        let ok = |s: &str| !s.is_empty() && s.len() <= 100 && !s.contains(['.', '[', ']', '|', '=', '/', '{', '}']);
        match settings.get(k) {
            Some(Value::String(s)) if ok(s) => vec![s.clone()],
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).filter(|s| ok(s)).map(String::from).take(50).collect(),
            _ => Vec::new(),
        }
    };
    let mut out = Vec::new();
    'raw: for raw in list {
        let mut expanded = vec![raw.clone()];
        let mut passes = 0;
        while passes < 4 && expanded.len() <= 200 {
            passes += 1;
            let Some(cap) = expanded.first().and_then(|r| PH.captures(r).map(|c| (c.get(0).unwrap().range(), c[1].to_string()))) else { break };
            let (range, key) = cap;
            let vals = values(&key);
            if vals.is_empty() {
                continue 'raw;
            }
            let inside_filter = expanded[0][..range.start].rfind('[').is_some_and(|o| expanded[0][..range.start].rfind(']').is_none_or(|c| c < o));
            let mut next = Vec::new();
            for r in &expanded {
                let (start, end) = PH.find(r).map(|m| (m.start(), m.end())).unwrap_or((range.start, range.end));
                if inside_filter {
                    next.push(format!("{}{}{}", &r[..start], vals.join("|"), &r[end..]));
                } else {
                    next.extend(vals.iter().map(|v| format!("{}{v}{}", &r[..start], &r[end..])));
                }
            }
            expanded = next;
        }
        if expanded.iter().any(|p| PH.is_match(p)) || expanded.len() > 200 {
            continue;
        }
        out.extend(expanded.iter().filter_map(|p| Path::parse(p).ok()));
        if out.len() > 500 {
            break;
        }
    }
    out
}

/// Keep what the keep paths name (when there are any), then remove what the other paths name.
pub fn apply(v: &mut Value, paths: &[Path]) {
    let keeps: Vec<&Path> = paths.iter().filter(|p| p.keep).collect();
    if !keeps.is_empty() {
        let mut kept = match v {
            Value::Array(_) => Value::Array(Vec::new()),
            _ => Value::Object(Map::new()),
        };
        for p in &keeps {
            keep_into(v, &p.segs, &mut kept, 0);
        }
        prune(&mut kept);
        *v = kept;
    }
    for p in paths.iter().filter(|p| !p.keep) {
        walk(v, &p.segs, 0);
    }
}

/// Copy what `segs` names from `src` into `dst`, building the containers on the way.
fn keep_into(src: &Value, segs: &[Seg], dst: &mut Value, depth: usize) {
    if depth > 64 {
        return;
    }
    let Some((first, rest)) = segs.split_first() else { return };
    let items = |src: &Value, dst: &mut Value, pick: &dyn Fn(&Value) -> bool| {
        let (Value::Array(s), Value::Array(d)) = (src, &mut *dst) else { return };
        while d.len() < s.len() {
            d.push(Value::Null);
        }
        for (i, item) in s.iter().enumerate() {
            if !pick(item) {
                continue;
            }
            if rest.is_empty() {
                d[i] = item.clone();
            } else {
                if d[i].is_null() {
                    d[i] = if item.is_array() { Value::Array(Vec::new()) } else { Value::Object(Map::new()) };
                }
                keep_into(item, rest, &mut d[i], depth + 1);
            }
        }
    };
    match first {
        Seg::Deep => {}
        Seg::Each => items(src, dst, &|_| true),
        Seg::Any => {
            let (Value::Object(s), Value::Object(d)) = (src, &mut *dst) else { return };
            for (k, c) in s {
                if rest.is_empty() {
                    d.insert(k.clone(), c.clone());
                } else {
                    let slot = d.entry(k.clone()).or_insert_with(|| if c.is_array() { Value::Array(Vec::new()) } else { Value::Object(Map::new()) });
                    keep_into(c, rest, slot, depth + 1);
                }
            }
        }
        Seg::Key { name, each, filter } => {
            let (Value::Object(s), Value::Object(d)) = (src, &mut *dst) else { return };
            let Some(child) = s.get(name) else { return };
            if *each {
                if !child.is_array() {
                    return;
                }
                let slot = d.entry(name.clone()).or_insert_with(|| Value::Array(Vec::new()));
                items(child, slot, &|it| filter.as_ref().is_none_or(|f| f.matches(it)));
            } else if rest.is_empty() {
                d.insert(name.clone(), child.clone());
            } else {
                let slot = d.entry(name.clone()).or_insert_with(|| if child.is_array() { Value::Array(Vec::new()) } else { Value::Object(Map::new()) });
                keep_into(child, rest, slot, depth + 1);
            }
        }
    }
}

/// Drop the gaps keep left in arrays (items nothing was kept from).
fn prune(v: &mut Value) {
    match v {
        Value::Array(a) => {
            a.retain(|x| !x.is_null());
            a.iter_mut().for_each(prune);
        }
        Value::Object(m) => m.values_mut().for_each(prune),
        _ => {}
    }
}

fn walk(v: &mut Value, segs: &[Seg], depth: usize) {
    // Payloads are capped at 1 MB; this bounds pathological nesting all the same.
    if depth > 64 {
        return;
    }
    let Some((first, rest)) = segs.split_first() else { return };
    match first {
        Seg::Deep => {
            walk(v, rest, depth + 1);
            match v {
                Value::Object(m) => m.values_mut().for_each(|c| walk(c, segs, depth + 1)),
                Value::Array(a) => a.iter_mut().for_each(|c| walk(c, segs, depth + 1)),
                _ => {}
            }
        }
        Seg::Each => {
            if let Value::Array(a) = v {
                a.iter_mut().for_each(|c| walk(c, rest, depth + 1));
            }
        }
        Seg::Any => {
            if let Value::Object(m) = v {
                m.values_mut().for_each(|c| walk(c, rest, depth + 1));
            }
        }
        Seg::Key { name, each, filter } => {
            let Value::Object(m) = v else { return };
            if rest.is_empty() {
                match filter {
                    // A filtered last segment removes the matching items.
                    Some(f) => {
                        if let Some(Value::Array(a)) = m.get_mut(name) {
                            a.retain(|it| !f.matches(it));
                        }
                    }
                    None => {
                        m.remove(name);
                    }
                }
                return;
            }
            let Some(child) = m.get_mut(name) else { return };
            if *each {
                if let Value::Array(a) = child {
                    a.iter_mut().filter(|c| filter.as_ref().is_none_or(|f| f.matches(c))).for_each(|c| walk(c, rest, depth + 1));
                }
            } else {
                walk(child, rest, depth + 1);
            }
        }
    }
}

/// What to keep of a body under a redact list: JSON and form bodies with the fields removed. A
/// body of another kind can't be checked, so nothing of it is kept. With no paths the body is kept
/// as it is.
pub fn body(content_type: Option<&str>, raw: &str, paths: &[Path]) -> Option<String> {
    if paths.is_empty() {
        return Some(raw.to_string());
    }
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if ct.contains("application/x-www-form-urlencoded") {
        let mut m = Map::new();
        for (k, v) in url::form_urlencoded::parse(raw.as_bytes()) {
            m.insert(k.into_owned(), Value::String(v.into_owned()));
        }
        let mut v = Value::Object(m);
        apply(&mut v, paths);
        let mut out = url::form_urlencoded::Serializer::new(String::new());
        if let Value::Object(m) = v {
            for (k, v) in m {
                out.append_pair(&k, v.as_str().unwrap_or(""));
            }
        }
        return Some(out.finish());
    }
    if raw.trim().is_empty() {
        return Some(String::new());
    }
    let mut v: Value = serde_json::from_str(raw).ok()?;
    apply(&mut v, paths);
    Some(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(v: Value, list: &[&str]) -> Value {
        let mut v = v;
        let ps: Vec<Path> = list.iter().map(|p| Path::parse(p).unwrap()).collect();
        apply(&mut v, &ps);
        v
    }

    #[test]
    fn removes_what_the_paths_name() {
        let v = json!({"results": [{"id": 1, "notes": {"keyCode": "1234"}}, {"id": 2, "notes": "x"}], "notes": "top"});
        assert_eq!(run(v.clone(), &["results[].notes"]), json!({"results": [{"id": 1}, {"id": 2}], "notes": "top"}));
        assert_eq!(run(v.clone(), &["**.notes"]), json!({"results": [{"id": 1}, {"id": 2}]}));
        assert_eq!(run(v.clone(), &["**.keyCode"]), json!({"results": [{"id": 1, "notes": {}}, {"id": 2, "notes": "x"}], "notes": "top"}));
        assert_eq!(run(json!([{"a": 1, "b": 2}]), &["[].a"]), json!([{"b": 2}]));
        assert_eq!(run(json!({"x": {"p": {"s": 1, "t": 2}, "q": {"s": 3}}}), &["x.*.s"]), json!({"x": {"p": {"t": 2}, "q": {}}}));
        // Nothing to remove: unchanged.
        assert_eq!(run(json!({"a": [1, 2]}), &["a[].b", "c.d"]), json!({"a": [1, 2]}));
    }

    #[test]
    fn paths_must_end_with_a_field() {
        assert!(Path::parse("results[]").is_ok(), "removes the results key");
        assert!(Path::parse("results.[]").is_err());
        assert!(Path::parse("**").is_err());
        assert!(Path::parse("a..b").is_err());
        assert!(Path::parse("a[b]").is_err());
        assert!(Path::parse("").is_err());
    }

    #[test]
    fn keep_paths_keep_only_what_they_name() {
        let v = json!({"event_id": "e1", "form_response": {"token": "t", "hidden": {"user_id": "u"}, "answers": [
            {"type": "email", "email": "a@example.com", "field": {"ref": "em"}},
            {"type": "text", "text": "free text", "field": {"ref": "note"}},
            {"type": "choice", "choice": {"label": "London", "other": "typed"}, "field": {"ref": "city"}}]}});
        let settings = json!({"email_field": "em", "copy_fields": ["city"]}).as_object().unwrap().clone();
        let list: Vec<String> = ["keep:event_id", "keep:form_response.token", "keep:form_response.answers[field/ref={setting:email_field}]", "keep:form_response.answers[field/ref={setting:copy_fields}]", "keep:form_response.answers[field/ref={setting:consent_field}]", "form_response.answers[].choice.other"]
            .iter().map(|s| s.to_string()).collect();
        let mut out = v.clone();
        apply(&mut out, &paths_with(&list, &settings));
        assert_eq!(out, json!({"event_id": "e1", "form_response": {"token": "t", "answers": [
            {"type": "email", "email": "a@example.com", "field": {"ref": "em"}},
            {"type": "choice", "choice": {"label": "London"}, "field": {"ref": "city"}}]}}));
        // Object keys from a setting, one path per value; an unset setting keeps nothing.
        let d = json!({"payload": {"id": "s1", "data": {"Email": "a@example.com", "Message": "call me", "Interest": "x"}}});
        let settings = json!({"email_field": "Email", "copy_fields": ["Interest", "bad.name"]}).as_object().unwrap().clone();
        let list: Vec<String> = ["keep:payload.id", "keep:payload.data.{setting:email_field}", "keep:payload.data.{setting:copy_fields}", "keep:payload.data.{setting:consent_field}"].iter().map(|s| s.to_string()).collect();
        let mut out = d.clone();
        apply(&mut out, &paths_with(&list, &settings));
        assert_eq!(out, json!({"payload": {"id": "s1", "data": {"Email": "a@example.com", "Interest": "x"}}}));
        // A setting holding a placeholder is skipped, never expanded again.
        let loopy = json!({"copy_fields": ["{setting:copy_fields}", "ok"]}).as_object().unwrap().clone();
        let ps = paths_with(&["keep:a.{setting:copy_fields}".to_string()], &loopy);
        assert_eq!(ps.len(), 1);
        assert!(Path::parse("keep:**.a").is_err());
        assert!(Path::parse("a[b]").is_err());
        assert!(Path::parse("answers[field/ref=a|b]").is_ok());
    }

    #[test]
    fn bodies_that_cant_be_checked_are_not_kept() {
        let ps = vec![Path::parse("code").unwrap()];
        assert_eq!(body(Some("application/json"), r#"{"code":"1","a":2}"#, &ps).as_deref(), Some(r#"{"a":2}"#));
        assert_eq!(body(Some("application/x-www-form-urlencoded"), "code=1&a=2", &ps).as_deref(), Some("a=2"));
        assert_eq!(body(Some("text/plain"), "code is 1234", &ps), None);
        assert_eq!(body(Some("text/plain"), "code is 1234", &[]).as_deref(), Some("code is 1234"));
    }
}
