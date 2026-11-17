//! Coupon codes in templates: the `{% coupon_code 'NAME' %}` and `{% coupon_expiry 'NAME' %}` tags
//! and the email builder's coupon block.
//!
//! The tags never reach the template engine. [`resolve`] (called by `render::render_str` on every
//! template) replaces each one with the text it stands for before rendering: the person's own code
//! in a real send, `SAMPLE-CODE` in a preview or test send. Which mode applies is set by the caller
//! under [`CTX_KEY`] in the render context: [`fill_sample`] (previews and test sends only) or the codes
//! a send prepared (`assign::SendCoupons::fill`). With neither, a tag is an error, so a real message can
//! never go out with a sample or blank code. Codes are letters, digits, `-` and `_` (see `store`), and
//! expiry dates are words and digits, so neither can form template syntax when put into the source.
//!
//! Which coupons a message uses is read from the same text the renderer renders ([`message_names`]:
//! the subject, preheader and body sources of an email, the body of a text), with the same pattern
//! [`resolve`] replaces, so a message claims exactly the codes it shows. Only literal names count; a
//! tag whose name isn't a quoted literal, or the old function-call form, is refused when the content
//! is saved ([`check_content`]) and fails to render.

use std::borrow::Cow;
use std::collections::BTreeSet;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::render::EmailContent;

/// Render-context key: `{"sample": true}`, or `{"codes": {lowercased name: {code, expires}}}`.
pub const CTX_KEY: &str = "__dm_coupons";
/// What previews and test sends show instead of a real code.
pub const SAMPLE_CODE: &str = "SAMPLE-CODE";
/// Most coupons one message may use.
pub const MAX_PER_MESSAGE: usize = 5;

static NAME: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9 _.\-]{0,63}$").unwrap());
/// A coupon tag with a literal name: group 1 is `code` or `expiry`, group 2 or 3 the name.
static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\{%-?\s*coupon_(code|expiry)\s+(?:'([^'{}%\n]*)'|"([^"{}%\n]*)")\s*-?%\}"#).unwrap());
/// Anything that starts like a coupon tag (to find ones whose name isn't a literal).
static TAG_START: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\{%-?\s*coupon_(?:code|expiry)\b"#).unwrap());
/// The function-call form an earlier draft of this feature used; no longer supported.
static CALL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\bcoupon_(?:code|expiry)\s*\("#).unwrap());

/// A coupon name: letters, digits, spaces, `.`, `_` and `-`, at most 64, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    NAME.is_match(name)
}

fn tag_name<'a>(c: &regex::Captures<'a>) -> &'a str {
    c.get(2).or_else(|| c.get(3)).map_or("", |m| m.as_str()).trim()
}

/// Mark a render context as a preview or test send: coupon tags show a sample. Never call this for
/// a real send.
pub fn fill_sample(ctx: &mut Value) {
    if let Some(o) = ctx.as_object_mut() {
        o.insert(CTX_KEY.into(), json!({"sample": true}));
    }
}

/// Replace the coupon tags in a template source with their text (see the module docs). Called by
/// `render::render_str`; a source without tags is returned as it is, with no work.
pub fn resolve<'a>(source: &'a str, ctx: &Value) -> AppResult<Cow<'a, str>> {
    if !source.contains("coupon_") || !TAG.is_match(source) {
        return Ok(Cow::Borrowed(source));
    }
    let mode = ctx.get(CTX_KEY);
    let sample = mode.and_then(|m| m.get("sample")).and_then(Value::as_bool) == Some(true);
    let codes = mode.and_then(|m| m.get("codes"));
    let mut err: Option<String> = None;
    let out = TAG.replace_all(source, |c: &regex::Captures<'_>| {
        let name = tag_name(c);
        let expiry = &c[1] == "expiry";
        if sample {
            return if expiry { (chrono::Utc::now() + chrono::Duration::days(30)).format("%B %-d, %Y").to_string() } else { SAMPLE_CODE.to_string() };
        }
        let entry = codes.and_then(|m| m.get(name.to_lowercase()));
        let field = if expiry { "expires" } else { "code" };
        match entry.and_then(|e| e.get(field)).and_then(Value::as_str) {
            Some(text) if text.chars().all(|ch| ch.is_ascii_alphanumeric() || " ,-_".contains(ch)) => text.to_string(),
            _ => {
                err.get_or_insert_with(|| format!("the code from coupon {name:?} wasn't prepared for this message"));
                String::new()
            }
        }
    });
    match err {
        Some(e) => Err(AppError::bad(e)),
        None => Ok(Cow::Owned(out.into_owned())),
    }
}

/// The coupon names in one template source, lowercased.
fn names_in(source: &str, out: &mut BTreeSet<String>) {
    if !source.contains("coupon_") {
        return;
    }
    for c in TAG.captures_iter(source) {
        let n = tag_name(&c);
        if !n.is_empty() {
            out.insert(n.to_lowercase());
        }
    }
}

/// Cheap first look: could this content use a coupon at all? (No allocation; most messages don't.)
pub fn may_use_coupons(content: &Value) -> bool {
    match content {
        Value::String(s) => s.contains("coupon_"),
        Value::Array(a) => a.iter().any(may_use_coupons),
        Value::Object(o) => o.get("type").and_then(Value::as_str) == Some("coupon") || o.values().any(may_use_coupons),
        _ => false,
    }
}

/// What the renderer will render for a message: for email the subject, the preheader, then the body
/// (as HTML and as text); for a text, the body. Returns (subject and preheader, body parts).
fn parts(channel: &str, content: &Value) -> AppResult<(Vec<String>, Vec<String>)> {
    if channel == "sms" {
        return Ok((Vec::new(), vec![content.get("body").and_then(Value::as_str).unwrap_or("").to_string()]));
    }
    let ec: EmailContent = serde_json::from_value(content.clone()).map_err(|e| AppError::bad(format!("invalid email content: {e}")))?;
    let [subject, preheader, html, text] = crate::render::email_template_parts(&ec);
    Ok((vec![subject, preheader], vec![html, text]))
}

/// Every coupon a message will show, lowercased, read from what the renderer renders. Errors when a
/// coupon tag is in the subject or preheader (those are stored with the message and in its events).
pub fn message_names(channel: &str, content: &Value) -> AppResult<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    if !may_use_coupons(content) {
        return Ok(out);
    }
    let (heads, bodies) = parts(channel, content)?;
    for h in &heads {
        names_in(h, &mut out);
    }
    if !out.is_empty() {
        return Err(AppError::bad("coupon codes can't go in the subject or preheader (they're kept with the message): put them in the body"));
    }
    for b in &bodies {
        names_in(b, &mut out);
    }
    if out.len() > MAX_PER_MESSAGE {
        return Err(AppError::bad(format!("a message can use at most {MAX_PER_MESSAGE} coupons")));
    }
    Ok(out)
}

/// Check message content when it's saved: coupon tags only in the body, names as quoted literals,
/// coupon blocks naming a valid coupon, at most [`MAX_PER_MESSAGE`] coupons. `channel` is "email" or
/// "sms"; other content (unknown channel) is checked as email.
pub fn check_content(channel: &str, content: &Value) -> AppResult<()> {
    if !may_use_coupons(content) {
        return Ok(());
    }
    // A text's content is just a body.
    let channel = if channel == "sms" || (content.get("body").is_some() && content.get("blocks").is_none() && content.get("html").is_none()) { "sms" } else { "email" };
    fn walk(v: &Value) -> AppResult<()> {
        match v {
            Value::String(s) if s.contains("coupon_") => {
                if CALL.is_match(s) {
                    return Err(AppError::bad("write coupon codes as {% coupon_code 'NAME' %} (and expiry dates as {% coupon_expiry 'NAME' %})"));
                }
                let literal = TAG.find_iter(s).count();
                if TAG_START.find_iter(s).count() != literal {
                    return Err(AppError::bad("a coupon tag needs the coupon's name in quotes, like {% coupon_code 'WELCOME' %}"));
                }
                for c in TAG.captures_iter(s) {
                    if c.get(3).is_some() {
                        // Double quotes are escaped in some fields (buttons, links), which would break the tag.
                        return Err(AppError::bad("write the coupon's name in single quotes, like {% coupon_code 'WELCOME' %}"));
                    }
                    if !valid_name(tag_name(&c)) {
                        return Err(AppError::bad(format!("{:?} isn't a coupon name (letters, digits, spaces, . _ and -)", tag_name(&c))));
                    }
                }
                Ok(())
            }
            Value::Array(a) => a.iter().try_for_each(walk),
            Value::Object(o) => {
                if o.get("type").and_then(Value::as_str) == Some("coupon") && !o.get("coupon").and_then(Value::as_str).is_some_and(|n| valid_name(n.trim())) {
                    return Err(AppError::bad("a coupon block needs the name of a coupon"));
                }
                o.values().try_for_each(walk)
            }
            _ => Ok(()),
        }
    }
    walk(content)?;
    message_names(channel, content).map(|_| ())
}

/// [`check_content`] for a campaign's content and each of its A/B versions.
pub fn check_campaign(channel: &str, content: &Value, ab_test: Option<&Value>) -> AppResult<()> {
    check_content(channel, content)?;
    for v in ab_test.and_then(|t| t.get("variants")).and_then(Value::as_array).into_iter().flatten() {
        if let Some(c) = v.get("content") {
            check_content(channel, c).map_err(|e| AppError::bad(format!("version {}: {}", v.get("id").and_then(Value::as_str).unwrap_or("?").to_uppercase(), e.public_message())))?;
        }
    }
    Ok(())
}

/// Refuse coupons outright (transactional email, which doesn't support them).
pub fn forbid(content: &Value) -> AppResult<()> {
    if may_use_coupons(content) && (TAG_START.is_match(&content.to_string()) || content.to_string().contains("\"type\":\"coupon\"")) {
        return Err(AppError::bad("coupon codes aren't supported in transactional email"));
    }
    Ok(())
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

/// The expiry line: shown only when the code has an expiry date.
fn expiry_line(name: &str, before: &str, after: &str) -> String {
    format!("{{% set __dm_exp %}}{{% coupon_expiry '{name}' %}}{{% endset %}}{{% if __dm_exp|trim %}}{before}{{{{ __dm_exp|trim }}}}{after}{{% endif %}}")
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
        expiry_line(name, &format!(r#"<div style="font-family:{font};font-size:13px;color:#6b7280;margin:10px 0 0;">Use it by "#), "</div>")
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
        r#"<tr><td style="padding:16px 24px;" align="center"><table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="border:2px dashed {brand};border-radius:8px;"><tr><td align="center" style="padding:20px 16px;"><div style="font-family:{font};font-size:14px;color:#4b5563;margin:0 0 8px;">{title}</div><div style="font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:26px;line-height:1.2;font-weight:700;letter-spacing:2px;color:#111827;word-break:break-all;">{{% coupon_code '{name}' %}}</div>{expiry}</td></tr></table>{button}</td></tr>"#,
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
    out.push_str(&format!("{title}: {{% coupon_code '{name}' %}}\n"));
    if shown_expiry(b) {
        out.push_str(&expiry_line(name, "Use it by ", "\n"));
    }
    if !text(b, "button_text").trim().is_empty() && !text(b, "href").trim().is_empty() {
        out.push_str(&format!("{}: {}\n", text(b, "button_text"), text(b, "href")));
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_rule() {
        assert!(valid_name("WELCOME10"));
        assert!(valid_name("Black Friday 2026"));
        assert!(!valid_name(""));
        assert!(!valid_name(" lead"));
        assert!(!valid_name("a\"b"));
        assert!(!valid_name("a'b"));
        assert!(!valid_name("x{{y"));
        assert!(!valid_name(&"a".repeat(65)));
    }

    #[test]
    fn finds_only_what_the_renderer_renders() {
        let content = json!({
            "subject": "Hello",
            "blocks": [
                {"type": "text", "html": "<p>Use {% coupon_code 'Welcome' %} by {% coupon_expiry \"VIP\" %}. Prose about coupon_code('X') isn't a tag.</p>"},
                {"type": "columns", "columns": [[{"type": "coupon", "coupon": "Spring Sale"}]]}
            ],
            // Not rendered when there are blocks.
            "html": "{% coupon_code 'UNUSED' %}"
        });
        let names: Vec<String> = message_names("email", &content).unwrap().into_iter().collect();
        assert_eq!(names, vec!["spring sale", "vip", "welcome"]);
        assert!(message_names("email", &json!({"subject": "x", "blocks": [{"type": "text", "html": "No coupons here"}]})).unwrap().is_empty());
        assert!(message_names("email", &json!({"subject": "Code {% coupon_code 'A' %}", "blocks": []})).is_err(), "not in the subject");
        assert!(message_names("email", &json!({"subject": "x", "preheader": "{% coupon_code 'A' %}", "blocks": []})).is_err(), "nor the preheader");
        let names: Vec<String> = message_names("sms", &json!({"body": "Code: {% coupon_code 'TXT' %}"})).unwrap().into_iter().collect();
        assert_eq!(names, vec!["txt"]);
        let six = (0..6).map(|i| format!("{{% coupon_code 'C{i}' %}}")).collect::<String>();
        assert!(message_names("sms", &json!({"body": six})).is_err());
    }

    #[test]
    fn saved_content_needs_literal_names() {
        assert!(check_content("email", &json!({"subject": "x", "blocks": [{"type": "text", "html": "{% coupon_code 'OK' %}"}]})).is_ok());
        assert!(check_content("email", &json!({"subject": "x", "blocks": [{"type": "text", "html": "{% coupon_code name %}"}]})).is_err());
        assert!(check_content("email", &json!({"subject": "x", "blocks": [{"type": "text", "html": "{{ coupon_code('A') }}"}]})).is_err());
        assert!(check_content("email", &json!({"subject": "x", "blocks": [{"type": "coupon", "coupon": ""}]})).is_err());
        assert!(check_content("sms", &json!({"body": "{% coupon_expiry first_name %}"})).is_err());
        assert!(check_content("email", &json!({"subject": "{% coupon_code 'A' %}", "blocks": []})).is_err());
        assert!(forbid(&json!({"subject": "x", "blocks": [{"type": "coupon", "coupon": "A"}]})).is_err());
        assert!(forbid(&json!({"subject": "Your receipt", "blocks": [{"type": "text", "html": "Thanks"}]})).is_ok());
    }

    #[test]
    fn tags_resolve_only_when_prepared() {
        let src = "Use {% coupon_code 'WELCOME' %} by {% coupon_expiry 'welcome' %}";
        assert!(resolve(src, &json!({})).is_err(), "a real render without prepared codes fails");
        let mut sample = json!({});
        fill_sample(&mut sample);
        assert!(resolve(src, &sample).unwrap().starts_with("Use SAMPLE-CODE by "));
        let real = json!({CTX_KEY: {"codes": {"welcome": {"code": "W-123", "expires": "May 1, 2027"}}}});
        assert_eq!(resolve(src, &real).unwrap(), "Use W-123 by May 1, 2027");
        assert!(resolve("{% coupon_code 'OTHER' %}", &real).is_err(), "a coupon nobody prepared never renders blank");
        let forged = json!({CTX_KEY: {"codes": {"welcome": {"code": "{{ 7*7 }}"}}}});
        assert!(resolve("{% coupon_code 'WELCOME' %}", &forged).is_err(), "only code characters are put into a template");
        assert!(matches!(resolve("No tags {{ first_name }}", &json!({})).unwrap(), Cow::Borrowed(_)));
    }
}
