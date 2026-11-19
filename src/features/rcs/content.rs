//! The optional rich version of a text: `content.rcs` next to the SMS `body`.
//!
//! ```json
//! {"body": "Acme: our spring sale starts now", "rcs": {
//!    "card": {"image_url": "https://shop.test/spring.jpg", "title": "Spring sale"},
//!    "suggestions": [{"type": "url", "text": "Shop now", "url": "https://shop.test/sale"},
//!                    {"type": "reply", "text": "Not for me"}]}}
//! ```
//!
//! On RCS the card shows the image and the title, with the text (the same text an SMS phone gets,
//! links and opt-out wording included) as its description. Up to four suggestions: replies (a tap
//! sends their text back, like a typed reply) and links (tracked like links in texts). Telnyx's
//! limits: a suggestion's text at most 25 characters, a title 200, a card's description 2,000 and
//! a message's text 3,072. A text that doesn't fit, or a phone that can't show a part of it, gets
//! the plain SMS instead.

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::render::Tracking;

pub const MAX_SUGGESTIONS: usize = 4;
pub const MAX_SUGGESTION_CHARS: usize = 25;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_URL_CHARS: usize = 2048;
/// Telnyx's limits on a message's text and a card's description.
pub const MAX_TEXT_CHARS: usize = 3072;
pub const MAX_CARD_TEXT_CHARS: usize = 2000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rich {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<Suggestion>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Card {
    #[serde(default)]
    pub image_url: String,
    /// May use variables, like the text.
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Suggestion {
    /// `reply` or `url`.
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// JSON schema of `content.rcs` (campaign and flow step content, MCP).
pub fn schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "description": "Optional rich version for phones that get RCS (see dumbmail://docs/rcs). The SMS body stays required: it's what other phones get, and on RCS it's the message text (the card's description when there's a card).",
        "properties": {
            "card": {"type": "object", "additionalProperties": false, "properties": {
                "image_url": {"type": "string", "description": "https link to a JPEG, PNG or GIF."},
                "title": {"type": "string", "maxLength": MAX_TITLE_CHARS, "description": "May use variables."}}},
            "suggestions": {"type": "array", "maxItems": MAX_SUGGESTIONS, "items": {"type": "object", "additionalProperties": false, "required": ["type", "text"], "properties": {
                "type": {"enum": ["reply", "url"], "description": "reply: a tap sends the text back as their reply. url: opens the link (tracked)."},
                "text": {"type": "string", "maxLength": MAX_SUGGESTION_CHARS},
                "url": {"type": "string", "description": "url suggestions: a full https link."}}}}}})
}

fn web_link(u: &str, what: &str) -> AppResult<String> {
    let u = u.trim();
    let ok = u.chars().count() <= MAX_URL_CHARS
        && !u.chars().any(char::is_whitespace)
        && url::Url::parse(u).ok().is_some_and(|p| matches!(p.scheme(), "https" | "http") && p.host_str().is_some_and(|h| !h.is_empty()) && p.username().is_empty() && p.password().is_none());
    if ok { Ok(u.to_string()) } else { Err(AppError::bad(format!("the rich version's {what} must be a full web link (https://…)"))) }
}

/// The rich version in `content`, checked. None when there isn't one (or it's empty).
pub fn parse(content: &Value) -> AppResult<Option<Rich>> {
    let raw = match content.get("rcs") {
        None | Some(Value::Null) => return Ok(None),
        Some(v) => v,
    };
    let rich: Rich = serde_json::from_value(raw.clone()).map_err(|e| AppError::bad(format!("invalid rich version (rcs): {e}")))?;
    if let Some(c) = &rich.card {
        if c.image_url.trim().is_empty() && c.title.trim().is_empty() {
            return Err(AppError::bad("give the rich version's card an image or a title, or remove the card"));
        }
        if !c.image_url.trim().is_empty() {
            let u = web_link(&c.image_url, "image")?;
            if !u.starts_with("https://") {
                return Err(AppError::bad("the rich version's image must be an https link"));
            }
        }
        if c.title.chars().count() > MAX_TITLE_CHARS {
            return Err(AppError::bad(format!("the rich version's title can be at most {MAX_TITLE_CHARS} characters")));
        }
    }
    if rich.suggestions.len() > MAX_SUGGESTIONS {
        return Err(AppError::bad(format!("the rich version can have at most {MAX_SUGGESTIONS} suggestions")));
    }
    let mut seen = Vec::new();
    for s in &rich.suggestions {
        let t = s.text.trim();
        if t.is_empty() || t.chars().count() > MAX_SUGGESTION_CHARS {
            return Err(AppError::bad(format!("each suggestion needs a label of 1 to {MAX_SUGGESTION_CHARS} characters")));
        }
        if seen.contains(&t.to_lowercase()) {
            return Err(AppError::bad(format!("two suggestions are labelled {t:?}")));
        }
        seen.push(t.to_lowercase());
        match s.kind.as_str() {
            "reply" if s.url.as_deref().is_none_or(|u| u.trim().is_empty()) => {}
            "reply" => return Err(AppError::bad("a reply suggestion has no link (use type url for a link)")),
            "url" => {
                web_link(s.url.as_deref().unwrap_or_default(), "link")?;
            }
            _ => return Err(AppError::bad("a suggestion's type is reply or url")),
        }
    }
    Ok((rich.card.is_some() || !rich.suggestions.is_empty()).then_some(rich))
}

/// Problems for the campaign readiness list.
pub fn problems(content: &Value) -> Vec<String> {
    match parse(content) {
        Ok(_) => Vec::new(),
        Err(e) => vec![e.public_message()],
    }
}

/// The RCS features a phone needs to show this rich version (Telnyx's capability names).
pub fn needs(rich: Option<&Rich>) -> Vec<&'static str> {
    let mut out = Vec::new();
    let Some(r) = rich else { return out };
    if r.card.is_some() {
        out.push("RICHCARD_STANDALONE");
    }
    if r.suggestions.iter().any(|s| s.kind == "url") {
        out.push("ACTION_OPEN_URL");
    }
    out
}

/// The rich version for one person: the title personalized, links through click tracking.
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    pub image_url: Option<String>,
    pub title: Option<String>,
    pub suggestions: Vec<Value>,
    pub has_card: bool,
}

fn postback(i: usize, kind: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("dumbmail:{kind}:{i}"))
}

pub fn prepare(rich: Option<&Rich>, ctx: &Value, tracking: Option<&Tracking<'_>>) -> AppResult<Prepared> {
    let Some(r) = rich else { return Ok(Prepared::default()) };
    let mut out = Prepared::default();
    if let Some(c) = &r.card {
        out.has_card = true;
        out.image_url = Some(c.image_url.trim().to_string()).filter(|u| !u.is_empty());
        let title = crate::render::render_str(&c.title, ctx, false)?;
        let title: String = title.trim().chars().take(MAX_TITLE_CHARS).collect();
        out.title = Some(title).filter(|t| !t.is_empty());
    }
    for (i, s) in r.suggestions.iter().enumerate() {
        let text = s.text.trim().to_string();
        out.suggestions.push(match s.kind.as_str() {
            "url" => {
                let dest = s.url.as_deref().unwrap_or_default().trim();
                let link = tracking.map_or_else(|| dest.to_string(), |t| crate::render::tracked_url(t, dest));
                json!({"action": {"text": text, "postback_data": postback(i, "url"), "fallback_url": link,
                    "open_url_action": {"url": link, "application": "BROWSER", "webview_view_mode": "WEBVIEW_VIEW_MODE_UNSPECIFIED"}}})
            }
            _ => json!({"reply": {"text": text, "postback_data": postback(i, "reply")}}),
        });
    }
    Ok(out)
}

/// Telnyx's `agent_message` for `text` (the SMS text as sent) and the prepared rich version.
/// None when RCS can't take it (too long), so it goes as SMS.
pub fn agent_message(text: &str, p: &Prepared) -> Option<Value> {
    let chars = text.chars().count();
    if !p.has_card {
        if chars > MAX_TEXT_CHARS {
            return None;
        }
        let mut m = json!({"text": text});
        if !p.suggestions.is_empty() {
            m["suggestions"] = json!(p.suggestions);
        }
        return Some(json!({"content_message": m}));
    }
    if chars > MAX_CARD_TEXT_CHARS {
        return None;
    }
    let mut card = json!({"description": text});
    if let Some(t) = &p.title {
        card["title"] = json!(t);
    }
    if let Some(u) = &p.image_url {
        card["media"] = json!({"height": "MEDIUM", "content_info": {"file_url": u}});
    }
    if !p.suggestions.is_empty() {
        card["suggestions"] = json!(p.suggestions);
    }
    Some(json!({"content_message": {"rich_card": {"standalone_card": {"card_orientation": "VERTICAL", "thumbnail_image_alignment": "LEFT", "card_content": card}}}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_builds() {
        assert_eq!(parse(&json!({"body": "x"})).unwrap(), None);
        assert_eq!(parse(&json!({"body": "x", "rcs": {}})).unwrap(), None);
        let c = json!({"body": "x", "rcs": {"card": {"image_url": "https://shop.test/a.jpg", "title": "Hi {{ first_name }}"},
            "suggestions": [{"type": "url", "text": "Shop", "url": "https://shop.test/s"}, {"type": "reply", "text": "No thanks"}]}});
        let r = parse(&c).unwrap().unwrap();
        assert_eq!(needs(Some(&r)), vec!["RICHCARD_STANDALONE", "ACTION_OPEN_URL"]);
        let p = prepare(Some(&r), &json!({"first_name": "Ann"}), None).unwrap();
        let m = agent_message("Acme: sale. Reply STOP to opt out.", &p).unwrap();
        let card = &m["content_message"]["rich_card"]["standalone_card"]["card_content"];
        assert_eq!(card["title"], "Hi Ann");
        assert_eq!(card["description"], "Acme: sale. Reply STOP to opt out.");
        assert_eq!(card["media"]["content_info"]["file_url"], "https://shop.test/a.jpg");
        assert_eq!(card["suggestions"][0]["action"]["open_url_action"]["url"], "https://shop.test/s");
        assert_eq!(card["suggestions"][1]["reply"]["text"], "No thanks");
        assert!(agent_message(&"a".repeat(2001), &p).is_none());
        let plain = agent_message("hi", &Prepared::default()).unwrap();
        assert_eq!(plain, json!({"content_message": {"text": "hi"}}));
    }

    #[test]
    fn refuses_bad_versions() {
        let bad = |rcs: Value| parse(&json!({"body": "x", "rcs": rcs})).is_err();
        assert!(bad(json!({"card": {"image_url": "http://shop.test/a.jpg"}})));
        assert!(bad(json!({"card": {}})));
        assert!(bad(json!({"suggestions": [{"type": "reply", "text": "a".repeat(26)}]})));
        assert!(bad(json!({"suggestions": [{"type": "url", "text": "Go", "url": "javascript:alert(1)"}]})));
        assert!(bad(json!({"suggestions": [{"type": "url", "text": "Go", "url": "https://u:p@shop.test"}]})));
        assert!(bad(json!({"suggestions": [{"type": "call", "text": "Go"}]})));
        assert!(bad(json!({"suggestions": [{"type": "reply", "text": "Yes"}, {"type": "reply", "text": "yes"}]})));
        assert!(bad(json!({"suggestions": (0..5).map(|i| json!({"type": "reply", "text": format!("r{i}")})).collect::<Vec<_>>()})));
        assert!(bad(json!({"extra": true})));
    }
}
