//! Coupon codes in templates: the `{% coupon_code 'NAME' %}` tag, the `coupon_code()` and
//! `coupon_expiry()` functions it becomes, and the email builder's coupon block.
//!
//! Codes are never handed out while rendering. A real send assigns them first (see `assign`) and
//! puts them in the render context under [`CTX_KEY`]; the functions only read that. Without it
//! (previews, test sends) they show a sample, so a preview never uses up a code.

use std::collections::BTreeSet;

use minijinja::value::Value as JValue;
use minijinja::{Environment, Error, ErrorKind, State};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

/// Render-context key holding this message's codes: `{lowercased name: {code, expires}}`.
pub const CTX_KEY: &str = "__dm_coupons";
/// What previews and test sends show instead of a real code.
pub const SAMPLE_CODE: &str = "SAMPLE-CODE";
/// Most coupons one message may use.
pub const MAX_PER_MESSAGE: usize = 5;

static NAME: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9 _.\-]{0,63}$").unwrap());
static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\{%-?\s*coupon_code\s+(?:'([^'{}%\n]*)'|"([^"{}%\n]*)")\s*-?%\}"#).unwrap());
static CALL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"coupon_(?:code|expiry)\s*\(\s*(?:'([^'{}%\n]*)'|"([^"{}%\n]*)")\s*\)"#).unwrap());

/// A coupon name: letters, digits, spaces, `.`, `_` and `-`, at most 64, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    NAME.is_match(name)
}

/// The tag becomes a function call, which the template engine understands.
pub fn rewrite_tags(source: &str) -> std::borrow::Cow<'_, str> {
    if !source.contains("coupon_code") {
        return std::borrow::Cow::Borrowed(source);
    }
    TAG.replace_all(source, |c: &regex::Captures<'_>| {
        let name = c.get(1).or_else(|| c.get(2)).map_or("", |m| m.as_str()).trim();
        // Names that aren't valid can't break out of the string; the function then refuses them.
        let name = if valid_name(name) { name } else { "" };
        format!("{{{{ coupon_code(\"{name}\") }}}}")
    })
}

/// Every coupon a message's content uses, lowercased (tags, function calls and coupon blocks).
pub fn coupon_names(content: &Value) -> BTreeSet<String> {
    fn walk(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::String(s) if s.contains("coupon_") => {
                for c in TAG.captures_iter(s).chain(CALL.captures_iter(s)) {
                    if let Some(m) = c.get(1).or_else(|| c.get(2)) {
                        out.insert(m.as_str().trim().to_lowercase());
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => {
                if o.get("type").and_then(Value::as_str) == Some("coupon") {
                    if let Some(n) = o.get("coupon").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()) {
                        out.insert(n.to_lowercase());
                    }
                }
                o.values().for_each(|x| walk(x, out));
            }
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    walk(content, &mut out);
    out
}

fn entry(state: &State<'_, '_>, name: &str) -> Result<Option<JValue>, Error> {
    if !valid_name(name) {
        return Err(Error::new(ErrorKind::InvalidOperation, format!("{name:?} isn't a coupon name")));
    }
    let Some(map) = state.lookup(CTX_KEY).filter(|m| !m.is_undefined() && !m.is_none()) else { return Ok(None) };
    let e = map.get_item(&JValue::from(name.to_lowercase()))?;
    if e.is_undefined() {
        // Never send a blank code: a send prepares every coupon its content names first.
        return Err(Error::new(ErrorKind::InvalidOperation, format!("the code from coupon {name:?} wasn't prepared for this message")));
    }
    Ok(Some(e))
}

fn coupon_code(state: &State<'_, '_>, name: String) -> Result<JValue, Error> {
    Ok(match entry(state, &name)? {
        Some(e) => e.get_attr("code")?,
        None => JValue::from(SAMPLE_CODE),
    })
}

fn coupon_expiry(state: &State<'_, '_>, name: String) -> Result<JValue, Error> {
    Ok(match entry(state, &name)? {
        Some(e) => e.get_attr("expires")?,
        // A believable sample date for previews.
        None => JValue::from((chrono::Utc::now() + chrono::Duration::days(30)).format("%B %-d, %Y").to_string()),
    })
}

/// Add `coupon_code(name)` and `coupon_expiry(name)` to a template environment.
pub fn register(env: &mut Environment<'static>) {
    env.add_function("coupon_code", coupon_code);
    env.add_function("coupon_expiry", coupon_expiry);
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn text<'a>(b: &'a Value, k: &str) -> &'a str {
    b.get(k).and_then(Value::as_str).unwrap_or("")
}

fn shown_expiry(b: &Value) -> bool {
    b.get("show_expiry").and_then(Value::as_bool).unwrap_or(true)
}

/// The coupon block: the code in a dashed box, an optional expiry line and an optional button.
/// `{"type": "coupon", "coupon": "WELCOME", "title": "Your code", "show_expiry": true, "button_text": "Shop now", "href": "https://..."}`
pub fn block_html(b: &Value, brand: &str, font: &str) -> String {
    let name = text(b, "coupon").trim();
    if !valid_name(name) {
        return String::new();
    }
    let title = Some(text(b, "title")).filter(|t| !t.trim().is_empty()).unwrap_or("Your code");
    let expiry = if shown_expiry(b) {
        format!(
            r#"{{% set __dm_exp = coupon_expiry("{name}") %}}{{% if __dm_exp %}}<div style="font-family:{font};font-size:13px;color:#6b7280;margin:10px 0 0;">Use it by {{{{ __dm_exp }}}}</div>{{% endif %}}"#
        )
    } else {
        String::new()
    };
    let button = if text(b, "button_text").trim().is_empty() || text(b, "href").trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"<table role="presentation" cellpadding="0" cellspacing="0" style="margin:16px auto 0;"><tr><td style="border-radius:6px;background:{brand};"><a href="{href}" style="display:inline-block;padding:12px 26px;font-family:{font};font-size:16px;font-weight:600;color:#ffffff;text-decoration:none;border-radius:6px;">{t}</a></td></tr></table>"#,
            href = esc(text(b, "href")),
            t = esc(text(b, "button_text"))
        )
    };
    format!(
        r#"<tr><td style="padding:16px 24px;" align="center"><table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="border:2px dashed {brand};border-radius:8px;"><tr><td align="center" style="padding:20px 16px;"><div style="font-family:{font};font-size:14px;color:#4b5563;margin:0 0 8px;">{title}</div><div style="font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:26px;line-height:1.2;font-weight:700;letter-spacing:2px;color:#111827;word-break:break-all;">{{{{ coupon_code("{name}") }}}}</div>{expiry}</td></tr></table>{button}</td></tr>"#,
        title = esc(title)
    )
}

/// The coupon block in the plain-text part.
pub fn block_text(b: &Value, out: &mut String) {
    let name = text(b, "coupon").trim();
    if !valid_name(name) {
        return;
    }
    let title = Some(text(b, "title")).filter(|t| !t.trim().is_empty()).unwrap_or("Your code");
    out.push_str(&format!("{title}: {{{{ coupon_code(\"{name}\") }}}}\n"));
    if shown_expiry(b) {
        out.push_str(&format!("{{% set __dm_exp = coupon_expiry(\"{name}\") %}}{{% if __dm_exp %}}Use it by {{{{ __dm_exp }}}}\n{{% endif %}}"));
    }
    if !text(b, "button_text").trim().is_empty() && !text(b, "href").trim().is_empty() {
        out.push_str(&format!("{}: {}\n", text(b, "button_text"), text(b, "href")));
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_follow_the_rule() {
        assert!(valid_name("WELCOME10"));
        assert!(valid_name("Black Friday 2026"));
        assert!(!valid_name(""));
        assert!(!valid_name(" lead"));
        assert!(!valid_name("a\"b"));
        assert!(!valid_name("x{{y"));
        assert!(!valid_name(&"a".repeat(65)));
    }

    #[test]
    fn finds_every_coupon_a_message_uses() {
        let content = json!({
            "subject": "Your code {% coupon_code 'Welcome' %}",
            "blocks": [
                {"type": "text", "html": "<p>Also {{ coupon_code(\"VIP\") }} until {{ coupon_expiry('vip') }}</p>"},
                {"type": "columns", "columns": [[{"type": "coupon", "coupon": "Spring Sale"}]]}
            ]
        });
        let names: Vec<String> = coupon_names(&content).into_iter().collect();
        assert_eq!(names, vec!["spring sale", "vip", "welcome"]);
        assert!(coupon_names(&json!({"body": "No coupons here"})).is_empty());
    }

    #[test]
    fn tags_become_calls_and_bad_names_cant_escape() {
        assert_eq!(rewrite_tags("Use {% coupon_code 'WELCOME' %} today"), "Use {{ coupon_code(\"WELCOME\") }} today");
        assert_eq!(rewrite_tags("{%- coupon_code \"Spring Sale\" -%}"), "{{ coupon_code(\"Spring Sale\") }}");
        assert_eq!(rewrite_tags("{% coupon_code 'a\") }}{{ 7*7 }}' %}"), "{% coupon_code 'a\") }}{{ 7*7 }}' %}", "braces never match");
        assert_eq!(rewrite_tags("{% coupon_code 'bad\"name' %}"), "{{ coupon_code(\"\") }}");
    }
}
