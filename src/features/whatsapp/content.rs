//! WhatsApp content for campaigns and flow steps: an approved template and what fills its
//! variables.
//!
//! ```json
//! {"template_id": "<uuid>", "header": "{{ first_name }}", "header_media_url": "https://...",
//!  "body": {"1": "{{ first_name|default('there') }}", "2": "{{ event.properties.order_id }}"},
//!  "buttons": {"1": "{{ event.properties.order_id }}"}}
//! ```
//!
//! `body` is keyed by the template's variable names ("1", "2" ... or named ones). `buttons` fills
//! link buttons whose URL ends in a variable, by button position; buttons DumbMail tracks are
//! filled automatically. Variables take `{{ }}` personalization only: coupon codes and review links
//! aren't supported in WhatsApp messages, and saving them is refused.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::templates::Template;
use crate::error::{AppError, AppResult};

pub const MAX_VAR: usize = 1000;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WaContent {
    #[serde(default)]
    pub template_id: Option<Uuid>,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub header_media_url: Option<String>,
    #[serde(default)]
    pub body: BTreeMap<String, String>,
    #[serde(default)]
    pub buttons: BTreeMap<String, String>,
}

pub fn schema() -> Value {
    json!({"type": "object", "description": "WhatsApp content: an approved template (list_whatsapp_templates) and what fills its variables. See dumbmail://docs/whatsapp.",
        "properties": {
            "template_id": {"type": "string", "description": "A template's DumbMail id from list_whatsapp_templates."},
            "header": {"type": "string", "description": "Fills the text header's variable, with personalization like {{ first_name }}."},
            "header_media_url": {"type": "string", "description": "https link to the image, video or document for a media header."},
            "body": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Each body variable by name (\"1\", \"2\" ... or the template's named variables) to its value, e.g. {\"1\": \"{{ first_name|default('there') }}\"}."},
            "buttons": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Link buttons whose URL ends in a variable, by button position (\"0\", \"1\"). Buttons DumbMail tracks fill themselves."}
        }})
}

pub fn starter() -> Value {
    json!({"template_id": null, "body": {}})
}

/// Parse content, checking what can be checked without the template.
pub fn parse(v: &Value) -> AppResult<WaContent> {
    if !v.is_object() {
        return Err(AppError::bad("WhatsApp content must be an object with template_id"));
    }
    let c: WaContent = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("WhatsApp content: {e}")))?;
    let vals = c.header.iter().chain(c.body.values()).chain(c.buttons.values());
    for s in vals {
        if s.chars().count() > MAX_VAR {
            return Err(AppError::bad(format!("WhatsApp variables are up to {MAX_VAR} characters")));
        }
        if s.contains("{%") {
            return Err(AppError::bad("WhatsApp variables take {{ }} personalization only: coupon codes, review links and other {% %} tags aren't supported in WhatsApp messages"));
        }
    }
    if c.body.len() > 60 || c.buttons.len() > 10 {
        return Err(AppError::bad("too many WhatsApp variables"));
    }
    if let Some(u) = c.header_media_url.as_deref().filter(|u| !u.trim().is_empty()) {
        media_url(u)?;
    }
    Ok(c)
}

fn media_url(u: &str) -> AppResult<String> {
    let u = u.trim();
    let ok = url::Url::parse(u).is_ok_and(|p| p.scheme() == "https" && p.username().is_empty() && p.host_str().is_some()) && u.len() <= 2000;
    if ok { Ok(u.to_string()) } else { Err(AppError::bad("The header's media link must be a full https:// link")) }
}

/// What's missing before this content can be sent with `t`.
pub fn problems_with(c: &WaContent, t: &Template) -> Vec<String> {
    let mut p = Vec::new();
    if !t.approved() {
        p.push(format!("The WhatsApp template {} is {} at Meta, not approved", t.name, t.status.to_lowercase()));
    }
    let v = t.vars();
    if let Some(f) = &v.header_unsupported {
        p.push(format!("DumbMail can't fill this template's {f} header"));
    }
    if !v.unsupported_buttons.is_empty() {
        p.push(format!("DumbMail can't fill this template's {} buttons", v.unsupported_buttons.join(", ")));
    }
    if v.header.is_some() && c.header.as_deref().is_none_or(|h| h.trim().is_empty()) {
        p.push("Fill the header's variable".into());
    }
    if let Some(kind) = &v.header_media && c.header_media_url.as_deref().is_none_or(|u| u.trim().is_empty()) {
        p.push(format!("Add a link to the header's {}", kind.to_lowercase()));
    }
    for name in &v.body {
        if c.body.get(name).is_none_or(|s| s.trim().is_empty()) {
            p.push(format!("Fill the variable {{{{{name}}}}}"));
        }
    }
    for (i, tracked) in &v.url_buttons {
        if !tracked && c.buttons.get(&i.to_string()).is_none_or(|s| s.trim().is_empty()) {
            p.push(format!("Fill the link of button {}", i + 1));
        }
    }
    p
}

/// A parameter value as Meta accepts it: no new lines or tabs, no runs of more than four spaces.
fn param(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c }).collect();
    let mut out = String::with_capacity(flat.len());
    let mut spaces = 0;
    for c in flat.trim().chars() {
        spaces = if c == ' ' { spaces + 1 } else { 0 };
        if spaces <= 4 {
            out.push(c);
        }
    }
    out.chars().take(max).collect()
}

/// The token in a tracked button's link for one message: signed, so it only ever names it.
pub fn button_token(secret: &str, message_id: Uuid, index: usize) -> String {
    crate::auth::sign(secret, &format!("wa{}{index}", message_id.simple()))
}

/// The message and button a tracked link's token names.
pub fn from_button_token(secret: &str, token: &str) -> Option<(Uuid, usize)> {
    if token.len() > 200 {
        return None;
    }
    let p = crate::auth::verify(secret, token)?;
    let rest = p.strip_prefix("wa")?;
    if rest.len() < 33 {
        return None;
    }
    let (id, idx) = rest.split_at(32);
    Some((Uuid::parse_str(id).ok()?, idx.parse().ok()?))
}

/// A template message ready for the Cloud API, and what the conversation view shows.
pub struct Rendered {
    pub payload: Value,
    pub text: String,
}

/// Fill the template for one person. `message_id` and `secret` make tracked buttons' tokens
/// (None for a preview, which shows where they go instead). Errors say what's wrong in plain words.
pub fn render(t: &Template, c: &WaContent, ctx: &Value, tracking: Option<(&str, Uuid)>) -> AppResult<Rendered> {
    let v = t.vars();
    let named = t.parameter_format == "NAMED";
    let fill = |src: Option<&String>, name: &str, max: usize| -> AppResult<String> {
        let raw = src.map(String::as_str).unwrap_or_default();
        let out = param(&crate::render::render_str(raw, ctx, false)?, max);
        if out.is_empty() {
            return Err(AppError::bad(format!("the variable {{{{{name}}}}} came out empty for this person (add a default, like {{{{ first_name|default('there') }}}})")));
        }
        Ok(out)
    };
    let mut components = Vec::new();
    let mut shown: BTreeMap<String, String> = BTreeMap::new();
    let mut header_text = t.component("HEADER").and_then(|h| h["text"].as_str()).map(str::to_string);
    if let Some(name) = &v.header {
        let val = fill(c.header.as_ref(), name, 60)?;
        let mut p = json!({"type": "text", "text": val});
        if named {
            p["parameter_name"] = json!(name);
        }
        header_text = header_text.map(|h| replace_var(&h, name, &val));
        components.push(json!({"type": "header", "parameters": [p]}));
    }
    if let Some(kind) = &v.header_media {
        let link = media_url(c.header_media_url.as_deref().unwrap_or_default())?;
        let k = kind.to_lowercase();
        components.push(json!({"type": "header", "parameters": [{"type": k, k.clone(): {"link": link}}]}));
    }
    if !v.body.is_empty() {
        let mut params = Vec::new();
        for name in &v.body {
            let val = fill(c.body.get(name), name, 1024)?;
            let mut p = json!({"type": "text", "text": val});
            if named {
                p["parameter_name"] = json!(name);
            }
            shown.insert(name.clone(), val);
            params.push(p);
        }
        components.push(json!({"type": "body", "parameters": params}));
    }
    for (i, is_tracked) in &v.url_buttons {
        let val = if *is_tracked {
            match tracking {
                Some((secret, mid)) => button_token(secret, mid, *i),
                None => "preview".into(),
            }
        } else {
            let raw = c.buttons.get(&i.to_string());
            let out = param(&crate::render::render_str(raw.map(String::as_str).unwrap_or_default(), ctx, false)?, 2000);
            if out.is_empty() {
                return Err(AppError::bad(format!("the link of button {} came out empty for this person", i + 1)));
            }
            out
        };
        components.push(json!({"type": "button", "sub_type": "url", "index": i.to_string(), "parameters": [{"type": "text", "text": val}]}));
    }
    let mut body_text = t.component("BODY").and_then(|b| b["text"].as_str()).unwrap_or_default().to_string();
    for (name, val) in &shown {
        body_text = replace_var(&body_text, name, val);
    }
    let footer = t.component("FOOTER").and_then(|f| f["text"].as_str()).map(str::to_string);
    let text = [header_text, Some(body_text), footer].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
    let mut tpl = json!({"name": t.name, "language": {"code": t.language}});
    if !components.is_empty() {
        tpl["components"] = json!(components);
    }
    Ok(Rendered { payload: json!({"type": "template", "template": tpl}), text })
}

fn replace_var(text: &str, name: &str, val: &str) -> String {
    let mut out = text.replace(&format!("{{{{{name}}}}}"), val);
    out = out.replace(&format!("{{{{ {name} }}}}"), val);
    out
}

/// A free-form text inside the customer service window.
pub fn text_payload(body: &str) -> Value {
    json!({"type": "text", "text": {"preview_url": true, "body": body}})
}

/// The phone-style preview the editors show: the filled template, its buttons and anything missing.
pub fn preview(t: &Template, c: &WaContent, ctx: &Value) -> Value {
    let problems = problems_with(c, t);
    let rendered = if problems.is_empty() { render(t, c, ctx, None).map_err(|e| e.public_message()) } else { Err(String::new()) };
    let buttons: Vec<Value> = t
        .component("BUTTONS")
        .and_then(|b| b["buttons"].as_array())
        .map(|bs| {
            bs.iter()
                .enumerate()
                .map(|(i, b)| {
                    let url = t.tracked_buttons.get(i.to_string()).cloned().or_else(|| b.get("url").cloned());
                    json!({"type": b["type"], "text": b["text"], "url": url, "tracked": t.tracked_buttons.get(i.to_string()).is_some()})
                })
                .collect()
        })
        .unwrap_or_default();
    let mut problems = problems;
    let text = match rendered {
        Ok(r) => r.text,
        Err(e) => {
            if !e.is_empty() {
                problems.push(e);
            }
            t.text()
        }
    };
    json!({
        "channel": "whatsapp", "template": {"id": t.id, "name": t.name, "language": t.language, "category": t.category, "status": t.status},
        "header_media": t.vars().header_media, "header_media_url": c.header_media_url, "text": text, "buttons": buttons, "problems": problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_and_tokens() {
        assert_eq!(param("a\nb\t c      d", 100), "a b  c    d");
        let id = Uuid::now_v7();
        let tok = button_token("k", id, 1);
        assert_eq!(from_button_token("k", &tok), Some((id, 1)));
        assert_eq!(from_button_token("other", &tok), None);
        assert!(parse(&json!({"template_id": null, "body": {"1": "{% coupon_code 'X' %}"}})).is_err());
        assert!(parse(&json!({"header_media_url": "http://x.test/a.png"})).is_err());
    }
}
