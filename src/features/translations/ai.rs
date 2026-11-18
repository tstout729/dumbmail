//! Machine-translated drafts through the Anthropic Messages API (`POST /v1/messages`, model
//! [`MODEL`]).
//!
//! A request carries only the strings being translated and the account's glossary, never anything
//! about a person. Template tags (`{{ … }}`, `{% … %}`), HTML tags and never-translate terms are
//! swapped for numbered placeholders before the request and put back after; a result whose
//! placeholders don't come back exactly (each once, HTML ones in their original order) is
//! rejected, and so is one that tries to add a template tag. What's put back is checked like a
//! hand-typed translation. Strings are never logged; errors are reduced to a status and a short
//! reason.

use std::collections::BTreeMap;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

use super::fields::{self, Kind};

pub const MODEL: &str = "claude-sonnet-5-5";
const API_VERSION: &str = "2026-10-21";
/// Server-side fallback when the model declines (routes by the reason it declined).
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// The most source characters in one request.
pub const MAX_REQUEST_CHARS: usize = 12_000;
/// The most strings in one request.
pub const MAX_REQUEST_STRINGS: usize = 60;
/// A field longer than this isn't machine translated (translate it by hand or with a file).
pub const MAX_FIELD_CHARS: usize = 40_000;
const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// A glossary entry as the prompt and the placeholders use it.
#[derive(Debug, Clone)]
pub struct Term {
    pub term: String,
    pub never_translate: bool,
    /// The fixed translation in the target language, when there is one.
    pub translation: Option<String>,
}

/// A string with its tags swapped for placeholders.
#[derive(Debug, Clone)]
pub struct Protected {
    pub text: String,
    /// (placeholder, original), in order of appearance.
    slots: Vec<(String, String)>,
    kind: Kind,
}

static TAGS: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)\{\{.*?\}\}|\{%.*?%\}|\{#.*?#\}").unwrap());
static HTML: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static SLOT: Lazy<Regex> = Lazy::new(|| Regex::new(r"⟦([THK])(\d+)⟧").unwrap());

/// Swap template tags, HTML tags (in HTML fields) and never-translate terms for placeholders.
/// None when the text can't be protected (it already contains placeholder brackets).
pub fn protect(kind: Kind, text: &str, glossary: &[Term]) -> Option<Protected> {
    if text.contains('⟦') || text.contains('⟧') {
        return None;
    }
    let mut slots = Vec::new();
    let mut n = 0;
    let mut swap = |re: &Regex, s: &str, tag: char| -> String {
        re.replace_all(s, |c: &regex::Captures<'_>| {
            n += 1;
            let p = format!("⟦{tag}{n}⟧");
            slots.push((p.clone(), c[0].to_string()));
            p
        })
        .into_owned()
    };
    // Markup first, so a tag inside an attribute (`href="{{ url }}"`) goes back with its element.
    let mut out = if kind == Kind::Html { swap(&HTML, text, 'H') } else { text.to_string() };
    out = swap(&TAGS, &out, 'T');
    for t in glossary.iter().filter(|t| t.never_translate && !t.term.trim().is_empty()) {
        let re = Regex::new(&format!(r"\b{}\b", regex::escape(t.term.trim()))).ok()?;
        out = swap(&re, &out, 'K');
    }
    Some(Protected { text: out, slots, kind })
}

/// Put the originals back into a translated string, or say why it can't be used.
pub fn restore(p: &Protected, translated: &str, source: &str) -> Result<String, &'static str> {
    let found: Vec<String> = SLOT.find_iter(translated).map(|m| m.as_str().to_string()).collect();
    let mut want: Vec<&str> = p.slots.iter().map(|(s, _)| s.as_str()).collect();
    let mut got: Vec<&str> = found.iter().map(String::as_str).collect();
    want.sort();
    got.sort();
    if want != got || translated.matches('⟦').count() != found.len() || translated.matches('⟧').count() != found.len() {
        return Err("placeholders changed");
    }
    let html_order: Vec<&str> = found.iter().map(String::as_str).filter(|s| s.starts_with("⟦H")).collect();
    let html_want: Vec<&str> = p.slots.iter().map(|(s, _)| s.as_str()).filter(|s| s.starts_with("⟦H")).collect();
    if html_order != html_want {
        return Err("HTML tags moved");
    }
    let text_only = SLOT.replace_all(translated, "");
    if ["{{", "{%", "{#", "}}", "%}", "#}"].iter().any(|t| text_only.contains(t)) {
        return Err("added a template tag");
    }
    if text_only.chars().any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r') {
        return Err("control characters");
    }
    // In HTML, the translation's own text can't add markup: only the original tags come back.
    let mut out = if p.kind == Kind::Html {
        let mut s = String::new();
        let mut last = 0;
        for m in SLOT.find_iter(translated) {
            s.push_str(&translated[last..m.start()].replace('<', "&lt;").replace('>', "&gt;"));
            s.push_str(m.as_str());
            last = m.end();
        }
        s.push_str(&translated[last..].replace('<', "&lt;").replace('>', "&gt;"));
        s
    } else {
        translated.to_string()
    };
    for (slot, orig) in &p.slots {
        out = out.replacen(slot, orig, 1);
    }
    fields::check_text(p.kind, source, &out).map_err(|_| "doesn't check out")?;
    Ok(out)
}

/// What a translation request needs, without the key.
pub struct Request<'a> {
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub source_lang: &'a str,
    pub target_lang: &'a str,
    pub glossary: &'a [Term],
    /// id → protected text
    pub strings: &'a BTreeMap<String, String>,
}

fn system_prompt(r: &Request<'_>) -> String {
    let mut s = format!(
        "You translate marketing email and text message copy from {src} to {dst} for a business. \
         You receive a JSON object mapping ids to strings. Reply with only a JSON object with the same ids, each mapped to its translation in {dst}. \
         Write natural, fluent {dst} that keeps the tone and meaning, and keep it about as long as the original. \
         Placeholders like ⟦T1⟧, ⟦H2⟧ and ⟦K3⟧ stand for code, markup and names: copy every placeholder exactly once, unchanged. Keep ⟦H…⟧ placeholders in their original order. \
         Don't add anything that isn't a translation: no notes, no template code, no HTML.",
        src = r.source_lang,
        dst = r.target_lang
    );
    let fixed: Vec<String> = r.glossary.iter().filter_map(|t| t.translation.as_ref().filter(|x| !x.trim().is_empty()).map(|x| format!("\"{}\" → \"{}\"", t.term, x))).collect();
    if !fixed.is_empty() {
        s.push_str(&format!(" Always translate these terms this way: {}.", fixed.join("; ")));
    }
    s
}

/// Why a request failed, short and safe to store (no strings, no key, no response body).
#[derive(Debug)]
pub struct Failure(pub String);

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn client() -> Result<reqwest::Client, Failure> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(Duration::from_secs(10))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Failure("couldn't start the translation client".into()))
}

/// Translate the strings in one request. Returns id → translation (still with placeholders).
pub async fn translate(r: &Request<'_>) -> Result<BTreeMap<String, String>, Failure> {
    let body = json!({
        "model": MODEL,
        "max_tokens": 16000,
        "system": system_prompt(r),
        "messages": [{"role": "user", "content": serde_json::to_string(r.strings).unwrap_or_default()}],
        "output_config": {"effort": "medium"},
        "fallbacks": "default",
    });
    let url = format!("{}/v1/messages", r.base_url.trim_end_matches('/'));
    let res = client()?
        .post(url)
        .header("x-api-key", r.api_key)
        .header("anthropic-version", API_VERSION)
        .header("anthropic-beta", FALLBACK_BETA)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| Failure(if e.is_timeout() { "the translation service didn't answer in time".into() } else { "couldn't reach the translation service".into() }))?;
    let status = res.status();
    if !status.is_success() {
        return Err(Failure(match status.as_u16() {
            401 | 403 => "the translation service refused DumbMail's key".into(),
            429 => "the translation service is busy; try again in a few minutes".into(),
            s if s >= 500 => format!("the translation service had a problem ({s})"),
            s => format!("the translation service refused the request ({s})"),
        }));
    }
    let bytes = read_capped(res).await?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| Failure("the translation service sent something unreadable".into()))?;
    match v.get("stop_reason").and_then(|s| s.as_str()) {
        Some("refusal") => return Err(Failure("the translation model declined this text".into())),
        Some("max_tokens") => return Err(Failure("the translation was cut off; translate fewer strings at a time".into())),
        _ => {}
    }
    let texts: Vec<&str> = v.get("content").and_then(|c| c.as_array()).into_iter().flatten().filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text")).filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect();
    for t in texts.iter().rev() {
        if let Some(m) = parse_object(t) {
            return Ok(m);
        }
    }
    Err(Failure("the translation service's answer wasn't in the expected form".into()))
}

async fn read_capped(mut res: reqwest::Response) -> Result<Vec<u8>, Failure> {
    let mut out = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|_| Failure("the translation service's answer broke off".into()))? {
        if out.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(Failure("the translation service's answer was too big".into()));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// A JSON object of strings in a reply (also inside a ```json fence).
fn parse_object(t: &str) -> Option<BTreeMap<String, String>> {
    let t = t.trim();
    let start = t.find('{')?;
    let end = t.rfind('}')?;
    let v: Value = serde_json::from_str(t.get(start..=end)?).ok()?;
    let o = v.as_object()?;
    Some(o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_protect_tags_and_markup() {
        let src = "<p>Hi <strong>{{ first_name }}</strong>, DumbMail has {% coupon_code 'A' %}</p>";
        let g = vec![Term { term: "DumbMail".into(), never_translate: true, translation: None }];
        let p = protect(Kind::Html, src, &g).unwrap();
        assert_eq!(p.text, "⟦H1⟧Hi ⟦H2⟧⟦T5⟧⟦H3⟧, ⟦K7⟧ has ⟦T6⟧⟦H4⟧");
    }

    #[test]
    fn restores_only_exact_placeholders() {
        let src = "<p>Hi {{ first_name }}</p>";
        let p = protect(Kind::Html, src, &[]).unwrap();
        assert_eq!(p.text, "⟦H1⟧Hi ⟦T3⟧⟦H2⟧");
        assert_eq!(restore(&p, "⟦H1⟧Salut ⟦T3⟧⟦H2⟧", src).unwrap(), "<p>Salut {{ first_name }}</p>");
        assert_eq!(restore(&p, "⟦H1⟧⟦T3⟧, salut ⟦H2⟧", src).unwrap(), "<p>{{ first_name }}, salut </p>", "words and tags may move");
        assert!(restore(&p, "⟦H1⟧Salut⟦H2⟧", src).is_err(), "a tag dropped");
        assert!(restore(&p, "⟦H1⟧Salut ⟦T3⟧ ⟦T3⟧⟦H2⟧", src).is_err(), "a tag twice");
        assert!(restore(&p, "⟦H2⟧Salut ⟦T3⟧⟦H1⟧", src).is_err(), "markup reordered");
        assert!(restore(&p, "⟦H1⟧Salut ⟦T3⟧ {{ person.email }}⟦H2⟧", src).is_err(), "a new template tag");
        assert_eq!(restore(&p, "⟦H1⟧<script>x</script> ⟦T3⟧⟦H2⟧", src).unwrap(), "<p>&lt;script&gt;x&lt;/script&gt; {{ first_name }}</p>", "no new markup");
        let a = "<a href=\"https://x.test/{{ id }}\">Go</a>";
        let p = protect(Kind::Html, a, &[]).unwrap();
        assert_eq!(restore(&p, "⟦H1⟧Allez⟦H2⟧", a).unwrap(), "<a href=\"https://x.test/{{ id }}\">Allez</a>");
        assert!(protect(Kind::Text, "a ⟦T1⟧", &[]).is_none());
    }

    #[test]
    fn parses_fenced_json() {
        let m = parse_object("```json\n{\"subject\": \"Salut\"}\n```").unwrap();
        assert_eq!(m["subject"], "Salut");
        assert!(parse_object("no").is_none());
    }
}
