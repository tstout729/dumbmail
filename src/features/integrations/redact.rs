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
//! `results[].notes`, `reservation.notes`, `**.keyCode`. The script always sees the whole payload
//! in memory; only what is kept (the delivery log) is reduced.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key { name: String, each: bool },
    Each,
    Any,
    Deep,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Path(Vec<Seg>);

impl Path {
    pub fn parse(raw: &str) -> Result<Path, String> {
        let raw = raw.trim();
        if raw.is_empty() || raw.len() > 200 {
            return Err("each redact path must be 1-200 characters".into());
        }
        let mut segs = Vec::new();
        for part in raw.split('.') {
            let seg = match part {
                "[]" => Seg::Each,
                "*" => Seg::Any,
                "**" => Seg::Deep,
                p => {
                    let (name, each) = match p.strip_suffix("[]") {
                        Some(n) => (n, true),
                        None => (p, false),
                    };
                    if name.is_empty() || name.len() > 100 || name.contains(['[', ']', '*']) {
                        return Err(format!("redact path {raw:?}: {part:?} isn't a field name, name[], [], * or **"));
                    }
                    Seg::Key { name: name.to_string(), each }
                }
            };
            segs.push(seg);
        }
        if !matches!(segs.last(), Some(Seg::Key { .. })) {
            return Err(format!("redact path {raw:?} must end with a field name (the field to remove)"));
        }
        Ok(Path(segs))
    }
}

/// Parse a list of paths, skipping (never failing on) any that don't parse: they were checked
/// when saved.
pub fn paths(list: &[String]) -> Vec<Path> {
    list.iter().filter_map(|p| Path::parse(p).ok()).collect()
}

/// Remove every field the paths name.
pub fn apply(v: &mut Value, paths: &[Path]) {
    for p in paths {
        walk(v, &p.0, 0);
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
        Seg::Key { name, each } => {
            let Value::Object(m) = v else { return };
            if rest.is_empty() {
                m.remove(name);
                return;
            }
            let Some(child) = m.get_mut(name) else { return };
            if *each {
                if let Value::Array(a) = child {
                    a.iter_mut().for_each(|c| walk(c, rest, depth + 1));
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
    fn bodies_that_cant_be_checked_are_not_kept() {
        let ps = vec![Path::parse("code").unwrap()];
        assert_eq!(body(Some("application/json"), r#"{"code":"1","a":2}"#, &ps).as_deref(), Some(r#"{"a":2}"#));
        assert_eq!(body(Some("application/x-www-form-urlencoded"), "code=1&a=2", &ps).as_deref(), Some("a=2"));
        assert_eq!(body(Some("text/plain"), "code is 1234", &ps), None);
        assert_eq!(body(Some("text/plain"), "code is 1234", &[]).as_deref(), Some("code is 1234"));
    }
}
