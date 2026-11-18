//! The translatable text of a message and its language versions.
//!
//! A language version is not a second copy of the message. Each translatable field carries its
//! translations next to it, keyed by language:
//!
//! - a block's fields in the block's `i18n`: `{"fr": {"text": {"text": "Bonjour", "status": "reviewed", "source": "1a2b3c4d"}}}`,
//!   keyed by the field's path inside the block (`text`, `links.0.text`, `rows.1.2`);
//! - the subject, preview text, custom HTML and a text's body in the content's `translations`:
//!   `{"default": "en", "languages": ["fr"], "fields": {"fr": {"subject": {...}}}}`.
//!
//! Layout, images, display rules, products and coupons are shared, so a translation can't change
//! what an email is made of, only its words. `source` is a short hash of the default text the
//! translation was made from: when the default changes, the translation is stale and the default
//! is sent for that field until someone updates it. Links are translated only when someone chooses
//! to localize one; otherwise the default link is used.
//!
//! Field ids (for the API, XLIFF and CSV): `subject`, `preheader`, `html`, `body`, and for blocks
//! `b<n>:<path>` (the n-th block, from 0) or `b<n>.c<col>.<k>:<path>` (a block inside columns).

use std::collections::BTreeSet;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};

use super::locale;
use crate::error::{AppError, AppResult};

/// The most languages one message (or block) may have besides its default.
pub const MAX_LANGUAGES: usize = 20;
/// The longest translation of one field.
pub const MAX_TEXT: usize = 100_000;
/// The most translated fields one message may carry, across languages.
pub const MAX_ENTRIES: usize = 5_000;

pub const MACHINE: &str = "machine";
pub const REVIEWED: &str = "reviewed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Plain text (escaped where it lands).
    Text,
    /// HTML (text blocks, custom HTML), inserted as typed, like the default.
    Html,
    /// A link, translated only when someone localizes it.
    Link,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Html => "html",
            Kind::Link => "link",
        }
    }
}

/// A short hash of a field's default text (FNV-1a, 32 bits, as 8 hex digits). The dashboard
/// computes the same one.
pub fn hash(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

fn s<'a>(b: &'a Value, k: &str) -> &'a str {
    b.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

/// The translatable fields of one block, as (path inside the block, kind), for the paths present.
pub fn block_fields(b: &Value) -> Vec<(String, Kind)> {
    let plain = |keys: &[(&str, Kind)]| keys.iter().map(|(k, kind)| (k.to_string(), *kind)).collect::<Vec<_>>();
    let mut out = match s(b, "type") {
        "heading" => plain(&[("text", Kind::Text)]),
        "text" | "html" => plain(&[("html", Kind::Html)]),
        "image" => plain(&[("alt", Kind::Text), ("href", Kind::Link)]),
        "button" => plain(&[("text", Kind::Text), ("href", Kind::Link)]),
        "product" => plain(&[("title", Kind::Text), ("price", Kind::Text), ("button_text", Kind::Text), ("href", Kind::Link)]),
        "products" => plain(&[("button_text", Kind::Text)]),
        "coupon" => plain(&[("title", Kind::Text), ("button_text", Kind::Text), ("href", Kind::Link)]),
        "video" => plain(&[("alt", Kind::Text), ("caption", Kind::Text), ("url", Kind::Link)]),
        "quote" => plain(&[("text", Kind::Text), ("author", Kind::Text), ("role", Kind::Text)]),
        "header" => plain(&[("logo_alt", Kind::Text), ("logo_href", Kind::Link)]),
        "footer" => plain(&[("text", Kind::Text)]),
        "image_text" => plain(&[("alt", Kind::Text), ("heading", Kind::Text), ("html", Kind::Html), ("button_text", Kind::Text), ("href", Kind::Link), ("button_href", Kind::Link)]),
        "table" => {
            let mut v = Vec::new();
            for (i, c) in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten().enumerate().take(10) {
                if c.get("label").is_some() {
                    v.push((format!("columns.{i}.label"), Kind::Text));
                }
            }
            for key in ["rows", "footer"] {
                for (r, row) in b.get(key).and_then(|r| r.as_array()).into_iter().flatten().enumerate().take(50) {
                    for (c, _) in row.as_array().into_iter().flatten().enumerate().take(10) {
                        v.push((format!("{key}.{r}.{c}"), Kind::Text));
                    }
                }
            }
            v
        }
        _ => Vec::new(),
    };
    if matches!(s(b, "type"), "header" | "nav" | "footer") {
        for (i, _) in b.get("links").and_then(|l| l.as_array()).into_iter().flatten().enumerate().take(8) {
            out.push((format!("links.{i}.text"), Kind::Text));
            out.push((format!("links.{i}.href"), Kind::Link));
        }
    }
    out
}

/// The kind of a block field path, when it's one this block translates.
pub fn block_field_kind(b: &Value, path: &str) -> Option<Kind> {
    block_fields(b).into_iter().find(|(p, _)| p == path).map(|(_, k)| k)
}

/// The text at `path` inside `v` (`links.0.text`).
pub fn get_path<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    path.split('.')
        .try_fold(v, |v, k| match v {
            Value::Object(o) => o.get(k),
            Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
            _ => None,
        })?
        .as_str()
}

fn set_path(v: &mut Value, path: &str, text: &str) -> bool {
    let mut cur = v;
    for k in path.split('.') {
        cur = match cur {
            Value::Object(o) => match o.get_mut(k) {
                Some(x) => x,
                None => return false,
            },
            Value::Array(a) => match k.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                Some(x) => x,
                None => return false,
            },
            _ => return false,
        };
    }
    if cur.is_string() {
        *cur = json!(text);
        true
    } else {
        false
    }
}

/// Top-level fields: (key, kind) for the channel's content.
fn top_fields(content: &Value) -> Vec<(&'static str, Kind)> {
    if content.get("body").is_some() && content.get("blocks").is_none() && content.get("subject").is_none() {
        return vec![("body", Kind::Text)];
    }
    let mut v = vec![("subject", Kind::Text), ("preheader", Kind::Text)];
    let has_blocks = content.get("blocks").and_then(|b| b.as_array()).is_some_and(|b| !b.is_empty());
    if !has_blocks && content.get("html").is_some() {
        v.push(("html", Kind::Html));
    }
    v
}

/// One translatable field of a message.
#[derive(Debug, Clone)]
pub struct Field {
    pub id: String,
    pub kind: Kind,
    pub source: String,
    /// Where it is, for people ("Subject", "Block 3 (button)").
    pub label: String,
}

fn block_label(b: &Value, loc: &str) -> String {
    let t = s(b, "type");
    let mut parts = loc.split('.');
    let top = parts.next().and_then(|p| p.strip_prefix('b')).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0) + 1;
    match (parts.next(), parts.next()) {
        (Some(c), Some(k)) => format!("Block {top}, column {}, block {} ({t})", c.trim_start_matches('c').parse::<usize>().unwrap_or(0) + 1, k.parse::<usize>().unwrap_or(0) + 1),
        _ => format!("Block {top} ({t})"),
    }
}

/// The blocks of a message with their location prefix (`b3`, `b2.c0.1`): top-level blocks and
/// blocks inside columns. Saved blocks are translated on the saved block itself.
fn located_blocks(content: &Value) -> Vec<(String, &Value)> {
    let mut out = Vec::new();
    for (i, b) in content.get("blocks").and_then(|b| b.as_array()).into_iter().flatten().enumerate() {
        if s(b, "type") == "columns" {
            for (c, col) in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten().enumerate() {
                for (k, x) in col.as_array().into_iter().flatten().enumerate() {
                    out.push((format!("b{i}.c{c}.{k}"), x));
                }
            }
        } else {
            out.push((format!("b{i}"), b));
        }
    }
    out
}

fn located_block_mut<'a>(content: &'a mut Value, loc: &str) -> Option<&'a mut Value> {
    let mut parts = loc.split('.');
    let i: usize = parts.next()?.strip_prefix('b')?.parse().ok()?;
    let b = content.get_mut("blocks")?.as_array_mut()?.get_mut(i)?;
    match (parts.next(), parts.next()) {
        (None, _) => Some(b),
        (Some(c), Some(k)) => {
            let c: usize = c.strip_prefix('c')?.parse().ok()?;
            let k: usize = k.parse().ok()?;
            b.get_mut("columns")?.as_array_mut()?.get_mut(c)?.as_array_mut()?.get_mut(k)
        }
        _ => None,
    }
}

/// Every translatable field with text in it, in reading order.
pub fn fields(content: &Value) -> Vec<Field> {
    let mut out = Vec::new();
    for (k, kind) in top_fields(content) {
        let src = s(content, k);
        if !src.trim().is_empty() {
            let label = match k {
                "subject" => "Subject",
                "preheader" => "Preview text",
                "html" => "Custom HTML",
                _ => "Text message",
            };
            out.push(Field { id: k.to_string(), kind, source: src.to_string(), label: label.into() });
        }
    }
    for (loc, b) in located_blocks(content) {
        for (path, kind) in block_fields(b) {
            if let Some(src) = get_path(b, &path).filter(|t| !t.trim().is_empty()) {
                out.push(Field { id: format!("{loc}:{path}"), kind, source: src.to_string(), label: block_label(b, &loc) });
            }
        }
    }
    out
}

/// The default language and the other languages a message has (from its `translations`; for a
/// saved block, the languages its blocks carry).
pub fn languages(content: &Value) -> (Option<String>, Vec<String>) {
    let tr = content.get("translations");
    let default = tr.and_then(|t| t.get("default")).and_then(|d| d.as_str()).map(str::to_string);
    let mut langs: Vec<String> = tr.and_then(|t| t.get("languages")).and_then(|l| l.as_array()).into_iter().flatten().filter_map(|l| l.as_str().map(str::to_string)).collect();
    if tr.is_none() {
        let mut seen = BTreeSet::new();
        for (_, b) in located_blocks(content) {
            if let Some(m) = b.get("i18n").and_then(|m| m.as_object()) {
                seen.extend(m.keys().cloned());
            }
        }
        langs = seen.into_iter().collect();
    }
    (default, langs)
}

/// The stored translation of field `id` in `lang`.
pub fn entry<'a>(content: &'a Value, id: &str, lang: &str) -> Option<&'a Value> {
    match id.split_once(':') {
        None => content.get("translations")?.get("fields")?.get(lang)?.get(id),
        Some((loc, path)) => {
            let b = located_blocks(content).into_iter().find(|(l, _)| l == loc)?.1;
            b.get("i18n")?.get(lang)?.get(path)
        }
    }
}

/// missing, machine, reviewed or stale (links nobody localized: `default`).
pub fn status(content: &Value, f: &Field, lang: &str) -> &'static str {
    match entry(content, &f.id, lang) {
        Some(e) if !s(e, "text").is_empty() => {
            if s(e, "source") != hash(&f.source) {
                "stale"
            } else if s(e, "status") == MACHINE {
                MACHINE
            } else {
                REVIEWED
            }
        }
        _ if f.kind == Kind::Link => "default",
        _ => "missing",
    }
}

/// Set (Some) or clear (None) field `id`'s translation in `lang`, made from its current default
/// text. Err when there's no such field.
pub fn set_entry(content: &mut Value, id: &str, lang: &str, value: Option<(&str, &str)>) -> AppResult<()> {
    let f = fields(content).into_iter().find(|f| f.id == id).ok_or_else(|| AppError::bad(format!("this message has no translatable text {id:?}")))?;
    let e = value.filter(|(t, _)| !t.is_empty()).map(|(text, status)| json!({"text": text, "status": status, "source": hash(&f.source)}));
    let (holder, key): (&mut Value, &str) = match id.split_once(':') {
        None => {
            let o = content.as_object_mut().ok_or_else(|| AppError::bad("content must be an object"))?;
            let tr = o.entry("translations").or_insert_with(|| json!({}));
            if !tr.is_object() {
                *tr = json!({});
            }
            let fields = tr.as_object_mut().expect("object").entry("fields").or_insert_with(|| json!({}));
            if !fields.is_object() {
                *fields = json!({});
            }
            (fields, id)
        }
        Some((loc, path)) => {
            let b = located_block_mut(content, loc).ok_or_else(|| AppError::bad(format!("no block at {loc}")))?;
            let o = b.as_object_mut().ok_or_else(|| AppError::bad("blocks must be objects"))?;
            let i = o.entry("i18n").or_insert_with(|| json!({}));
            if !i.is_object() {
                *i = json!({});
            }
            (i, path)
        }
    };
    let langs = holder.as_object_mut().expect("object");
    match e {
        Some(e) => {
            let m = langs.entry(lang.to_string()).or_insert_with(|| json!({}));
            if !m.is_object() {
                *m = json!({});
            }
            m.as_object_mut().expect("object").insert(key.to_string(), e);
        }
        None => {
            if let Some(m) = langs.get_mut(lang).and_then(|m| m.as_object_mut()) {
                m.remove(key);
                if m.is_empty() {
                    langs.remove(lang);
                }
            }
        }
    }
    Ok(())
}

/// Mark every current translation in `lang` (or only `ids`) reviewed. Returns how many changed.
pub fn mark_reviewed(content: &mut Value, lang: &str, ids: Option<&[String]>) -> AppResult<usize> {
    let mut n = 0;
    for f in fields(content) {
        if ids.is_some_and(|ids| !ids.contains(&f.id)) || status(content, &f, lang) != MACHINE {
            continue;
        }
        let text = entry(content, &f.id, lang).map(|e| s(e, "text").to_string()).unwrap_or_default();
        set_entry(content, &f.id, lang, Some((&text, REVIEWED)))?;
        n += 1;
    }
    Ok(n)
}

/// Make sure `lang` is listed in the message's languages (and the default is recorded).
pub fn add_language(content: &mut Value, default: &str, lang: &str) {
    let Some(o) = content.as_object_mut() else { return };
    let tr = o.entry("translations").or_insert_with(|| json!({}));
    if !tr.is_object() {
        *tr = json!({});
    }
    let t = tr.as_object_mut().expect("object");
    t.entry("default").or_insert_with(|| json!(default));
    let l = t.entry("languages").or_insert_with(|| json!([]));
    if !l.is_array() {
        *l = json!([]);
    }
    let a = l.as_array_mut().expect("array");
    if !a.iter().any(|x| x.as_str() == Some(lang)) {
        a.push(json!(lang));
    }
}

/// Take `lang` off the message: its translations everywhere, and from the list.
pub fn remove_language(content: &mut Value, lang: &str) {
    fn walk(list: &mut [Value], lang: &str) {
        for b in list {
            if let Some(m) = b.get_mut("i18n").and_then(|m| m.as_object_mut()) {
                m.remove(lang);
            }
            if s(b, "type") == "columns" {
                for col in b.get_mut("columns").and_then(|c| c.as_array_mut()).into_iter().flatten() {
                    if let Some(a) = col.as_array_mut() {
                        walk(a, lang);
                    }
                }
            }
        }
    }
    if let Some(blocks) = content.get_mut("blocks").and_then(|b| b.as_array_mut()) {
        walk(blocks, lang);
    }
    if let Some(t) = content.get_mut("translations").and_then(|t| t.as_object_mut()) {
        if let Some(f) = t.get_mut("fields").and_then(|f| f.as_object_mut()) {
            f.remove(lang);
        }
        if let Some(l) = t.get_mut("languages").and_then(|l| l.as_array_mut()) {
            l.retain(|x| x.as_str() != Some(lang));
        }
    }
}

// ---------- Template tags ----------

static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)\{\{.*?\}\}|\{%.*?%\}|\{#.*?#\}").unwrap());

/// The template tags in a text, whitespace-normalized and sorted (a multiset).
pub fn tags(text: &str) -> Vec<String> {
    // `{{first_name}}` and `{{ first_name }}` are the same tag.
    let mut v: Vec<String> = TAG
        .find_iter(text)
        .map(|m| {
            let t = m.as_str();
            format!("{} {} {}", &t[..2], t[2..t.len() - 2].split_whitespace().collect::<Vec<_>>().join(" "), &t[t.len() - 2..])
        })
        .collect();
    v.sort();
    v
}

/// Why a translation can't be used for a field with this default text, if it can't: its template
/// tags must be exactly the default's, and it must parse as a template.
pub fn check_text(kind: Kind, source: &str, text: &str) -> Result<(), String> {
    if text.len() > MAX_TEXT {
        return Err(format!("a translation can be at most {MAX_TEXT} characters"));
    }
    if text.chars().any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t') {
        return Err("a translation can't contain control characters".into());
    }
    if tags(text) != tags(source) {
        return Err("it must keep exactly the template tags of the original (like {{ first_name }}), no more and no fewer".into());
    }
    if text.contains("{{") || text.contains("{%") {
        let (hidden, _) = crate::features::coupons::template::hide_tags(text);
        if let Err(e) = minijinja::Environment::new().template_from_str(&hidden) {
            return Err(format!("its template tags don't work: {e}"));
        }
    }
    if kind == Kind::Link {
        let lower = text.trim().to_ascii_lowercase();
        if !(lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:") || lower.starts_with("tel:")) || text.len() > 2048 || text.trim().contains(char::is_whitespace) {
            return Err("a localized link must start with https://, http://, mailto: or tel:".into());
        }
    }
    Ok(())
}

fn check_entry(kind: Kind, source: Option<&str>, e: &Value, label: &str) -> AppResult<()> {
    let o = e.as_object().ok_or_else(|| AppError::bad(format!("{label}: a translation is {{\"text\", \"status\", \"source\"}}")))?;
    if o.keys().any(|k| !matches!(k.as_str(), "text" | "status" | "source")) {
        return Err(AppError::bad(format!("{label}: a translation has only text, status and source")));
    }
    let text = o.get("text").and_then(|t| t.as_str()).ok_or_else(|| AppError::bad(format!("{label}: text must be text")))?;
    if !matches!(o.get("status").and_then(|t| t.as_str()), Some(MACHINE | REVIEWED)) {
        return Err(AppError::bad(format!("{label}: status must be machine or reviewed")));
    }
    let src = o.get("source").and_then(|t| t.as_str()).unwrap_or("");
    if src.len() != 8 || !src.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::bad(format!("{label}: source must be the 8-digit hash of the original text (the translations API fills it in)")));
    }
    if text.len() > MAX_TEXT {
        return Err(AppError::bad(format!("{label}: a translation can be at most {MAX_TEXT} characters")));
    }
    // Only a translation of the current text can be sent; a stale one falls back to the default, so
    // its tags may differ from the default's new ones.
    if let Some(source) = source.filter(|s| hash(s) == src) {
        if !text.is_empty() {
            check_text(kind, source, text).map_err(|e| AppError::bad(format!("{label}: {e}")))?;
        }
    }
    Ok(())
}

fn check_lang_map(m: &Value, label: &str) -> AppResult<()> {
    let o = m.as_object().ok_or_else(|| AppError::bad(format!("{label}: translations are keyed by language")))?;
    if o.len() > MAX_LANGUAGES {
        return Err(AppError::bad(format!("{label}: at most {MAX_LANGUAGES} languages")));
    }
    for k in o.keys() {
        if locale::normalize(k).as_deref() != Some(k.as_str()) {
            return Err(AppError::bad(format!("{label}: {k:?} isn't a language tag like fr or pt-BR")));
        }
    }
    Ok(())
}

fn check_blocks(list: &[Value], loc: &str, count: &mut usize) -> AppResult<()> {
    for (i, b) in list.iter().enumerate() {
        let here = if loc.is_empty() { format!("block {}", i + 1) } else { format!("{loc}, block {}", i + 1) };
        if let Some(m) = b.get("i18n").filter(|m| !m.is_null()) {
            check_lang_map(m, &here)?;
            for (lang, fields) in m.as_object().into_iter().flatten() {
                let fo = fields.as_object().ok_or_else(|| AppError::bad(format!("{here}: {lang} translations are keyed by field")))?;
                for (path, e) in fo {
                    *count += 1;
                    let kind = block_field_kind(b, path).ok_or_else(|| AppError::bad(format!("{here}: {path:?} isn't translatable text of a {} block", s(b, "type"))))?;
                    check_entry(kind, get_path(b, path), e, &format!("{here}, {lang} {path}"))?;
                }
            }
        }
        match s(b, "type") {
            "columns" => {
                for (c, col) in b.get("columns").and_then(|c| c.as_array()).into_iter().flatten().enumerate() {
                    check_blocks(col.as_array().map(|a| a.as_slice()).unwrap_or(&[]), &format!("{here}, column {}", c + 1), count)?;
                }
            }
            "saved" => check_blocks(b.get("blocks").and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]), &here, count)?,
            _ => {}
        }
    }
    Ok(())
}

fn uses(content: &Value) -> bool {
    fn walk(list: &[Value]) -> bool {
        list.iter().any(|b| {
            b.get("i18n").is_some()
                || (s(b, "type") == "columns" && b.get("columns").and_then(|c| c.as_array()).is_some_and(|cols| cols.iter().any(|c| c.as_array().is_some_and(|a| walk(a)))))
                || (s(b, "type") == "saved" && b.get("blocks").and_then(|v| v.as_array()).is_some_and(|a| walk(a)))
        })
    }
    content.get("translations").is_some() || content.get("blocks").and_then(|b| b.as_array()).is_some_and(|b| walk(b))
}

/// Check a message's language versions when it's saved (any save path: campaigns and their A/B
/// versions, flow steps, templates, saved blocks). Content without any is not looked at further.
pub fn check_saved(content: &Value) -> AppResult<()> {
    if !content.is_object() || !uses(content) {
        return Ok(());
    }
    if let Some(tr) = content.get("translations").filter(|t| !t.is_null()) {
        let o = tr.as_object().ok_or_else(|| AppError::bad("translations must be an object"))?;
        if o.keys().any(|k| !matches!(k.as_str(), "default" | "languages" | "fields")) {
            return Err(AppError::bad("translations has only default, languages and fields"));
        }
        if let Some(d) = o.get("default") {
            let d = d.as_str().unwrap_or("");
            if locale::normalize(d).as_deref() != Some(d) {
                return Err(AppError::bad(format!("translations.default {d:?} isn't a language tag like en")));
            }
        }
        let langs = o.get("languages").map(|l| l.as_array().ok_or_else(|| AppError::bad("translations.languages must be a list"))).transpose()?;
        let langs: Vec<&str> = langs.into_iter().flatten().map(|l| l.as_str().unwrap_or("")).collect();
        if langs.len() > MAX_LANGUAGES {
            return Err(AppError::bad(format!("a message can have at most {MAX_LANGUAGES} languages")));
        }
        for (i, l) in langs.iter().enumerate() {
            if locale::normalize(l).as_deref() != Some(*l) {
                return Err(AppError::bad(format!("{l:?} isn't a language tag like fr or pt-BR")));
            }
            if langs[..i].contains(l) {
                return Err(AppError::bad(format!("{l} is listed twice in translations.languages")));
            }
        }
        if let Some(f) = o.get("fields").filter(|f| !f.is_null()) {
            check_lang_map(f, "translations.fields")?;
            let tops = top_fields(content);
            for (lang, m) in f.as_object().into_iter().flatten() {
                for (k, e) in m.as_object().ok_or_else(|| AppError::bad("translations.fields are keyed by field"))? {
                    let kind = tops.iter().find(|(t, _)| t == k).map(|(_, kind)| *kind).ok_or_else(|| AppError::bad(format!("{k:?} isn't a translatable field of this message")))?;
                    check_entry(kind, content.get(k.as_str()).and_then(|v| v.as_str()), e, &format!("{lang} {k}"))?;
                }
            }
        }
    }
    let mut count = 0;
    check_blocks(content.get("blocks").and_then(|b| b.as_array()).map(|a| a.as_slice()).unwrap_or(&[]), "", &mut count)?;
    let tops: usize = content.pointer("/translations/fields").and_then(|f| f.as_object()).map(|o| o.values().filter_map(|m| m.as_object()).map(|m| m.len()).sum()).unwrap_or(0);
    if count + tops > MAX_ENTRIES {
        return Err(AppError::bad(format!("a message can carry at most {MAX_ENTRIES} translated fields")));
    }
    Ok(())
}

// ---------- Choosing and applying a version ----------

/// Keys the content prepare step computes (see `features::content::prepare`); content can't set them.
pub const COMPUTED: [&str; 2] = ["_lang", "_footer"];

/// Whether the content prepare step has language work to do for `content` (see [`choose`]).
pub fn has_versions(content: &Value) -> bool {
    content.get("translations").is_some_and(|t| t.is_object()) || COMPUTED.iter().any(|k| content.get(*k).is_some())
}

/// Which language `content` goes out in for someone whose language is `person_locale`: the
/// version with the same tag, else the same language, else the message's default. None when the
/// message has no language versions (nothing to do). `pinned` (a preview or test in a chosen
/// language) wins over the person's.
pub fn choose(content: &Value, person_locale: Option<&str>, pinned: Option<&str>) -> Option<String> {
    let Some(tr) = content.get("translations").filter(|t| t.is_object()) else {
        // Content can't set the computed keys itself; strip them (empty: no language).
        return COMPUTED.iter().any(|k| content.get(*k).is_some()).then(String::new);
    };
    let default = tr.get("default").and_then(|d| d.as_str()).and_then(locale::normalize).unwrap_or_else(|| "en".into());
    let mut all = vec![default.clone()];
    all.extend(tr.get("languages").and_then(|l| l.as_array()).into_iter().flatten().filter_map(|l| l.as_str().map(str::to_string)));
    let pick = |want: &str| locale::best_match(want, &all).cloned();
    Some(pinned.and_then(pick).or_else(|| person_locale.and_then(pick)).unwrap_or(default))
}

/// One block in `lang`: each translated field that's current replaces the default text, then the
/// translations are taken off (also from blocks inside columns and saved blocks).
pub fn localize_block(b: &mut Value, lang: &str) {
    if let Some(o) = b.as_object_mut() {
        if let Some(i18n) = o.remove("i18n") {
            if let Some(m) = i18n.get(lang).and_then(|m| m.as_object()) {
                for (path, e) in m {
                    let Some(kind) = block_field_kind(b, path) else { continue };
                    let Some(source) = get_path(b, path) else { continue };
                    let text = s(e, "text");
                    if text.is_empty() || s(e, "source") != hash(source) || check_text(kind, source, text).is_err() {
                        continue;
                    }
                    let text = text.to_string();
                    set_path(b, path, &text);
                }
            }
        }
    }
    match s(b, "type") {
        "columns" => {
            for col in b.get_mut("columns").and_then(|c| c.as_array_mut()).into_iter().flatten() {
                for x in col.as_array_mut().into_iter().flatten() {
                    localize_block(x, lang);
                }
            }
        }
        "saved" => {
            for x in b.get_mut("blocks").and_then(|v| v.as_array_mut()).into_iter().flatten() {
                localize_block(x, lang);
            }
        }
        _ => {}
    }
}

/// `content` in `lang` (see [`localize_block`]), without its translations, with `_lang` set (unless
/// `lang` is empty: then the computed keys are only removed).
pub fn localize(content: &Value, lang: &str) -> Value {
    let mut c = content.clone();
    let Some(o) = c.as_object_mut() else { return c };
    for k in COMPUTED {
        o.remove(k);
    }
    let tr = o.remove("translations");
    if let Some(m) = tr.as_ref().and_then(|t| t.pointer(&format!("/fields/{}", lang.replace('~', "~0").replace('/', "~1")))).and_then(|m| m.as_object()) {
        let tops = top_fields(&Value::Object(o.clone()));
        for (k, kind) in tops {
            let (Some(e), Some(source)) = (m.get(k), o.get(k).and_then(|v| v.as_str())) else { continue };
            let text = s(e, "text");
            if !text.is_empty() && s(e, "source") == hash(source) && check_text(kind, source, text).is_ok() {
                o.insert(k.into(), json!(text));
            }
        }
    }
    if let Some(blocks) = o.get_mut("blocks").and_then(|b| b.as_array_mut()) {
        for b in blocks {
            localize_block(b, lang);
        }
    }
    if !lang.is_empty() {
        o.insert("_lang".into(), json!(lang));
    }
    c
}

/// Every localized link in `content`, in every language (for the link checks before sending).
pub fn localized_links(content: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let (_, langs) = languages(content);
    for f in fields(content).into_iter().filter(|f| f.kind == Kind::Link) {
        for l in &langs {
            if status(content, &f, l) != "default" && status(content, &f, l) != "stale" {
                if let Some(t) = entry(content, &f.id, l).map(|e| s(e, "text")).filter(|t| !t.is_empty()) {
                    out.push((t.to_string(), String::new()));
                }
            }
        }
    }
    out
}

/// A map of status counts for `lang`.
pub fn summary(content: &Value, lang: &str) -> Map<String, Value> {
    let mut m = Map::new();
    for k in ["missing", MACHINE, REVIEWED, "stale"] {
        m.insert(k.into(), json!(0));
    }
    for f in fields(content) {
        let st = status(content, &f, lang);
        if st == "default" {
            continue;
        }
        let n = m.get(st).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        m.insert(st.into(), json!(n));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email() -> Value {
        json!({
            "subject": "Hi {{ first_name }}", "preheader": "New stuff",
            "blocks": [
                {"type": "heading", "text": "Hello"},
                {"type": "columns", "columns": [[{"type": "button", "text": "Shop", "href": "https://a.test/en"}], []]},
                {"type": "image", "src": "https://a.test/x.png", "alt": ""}
            ]
        })
    }

    #[test]
    fn lists_fields_with_stable_ids() {
        let ids: Vec<String> = fields(&email()).into_iter().map(|f| f.id).collect();
        assert_eq!(ids, vec!["subject", "preheader", "b0:text", "b1.c0.0:text", "b1.c0.0:href"]);
        assert_eq!(fields(&json!({"body": "Sale {{ x }}"}))[0].id, "body");
    }

    #[test]
    fn applies_only_current_translations() {
        let mut c = email();
        add_language(&mut c, "en", "fr");
        set_entry(&mut c, "subject", "fr", Some(("Salut {{ first_name }}", REVIEWED))).unwrap();
        set_entry(&mut c, "b0:text", "fr", Some(("Bonjour", MACHINE))).unwrap();
        set_entry(&mut c, "b1.c0.0:href", "fr", Some(("https://a.test/fr", REVIEWED))).unwrap();
        check_saved(&c).unwrap();
        // The default heading changes: its French is stale and the default is sent for it.
        c["blocks"][0]["text"] = json!("Hello there");
        check_saved(&c).unwrap();
        let fr = localize(&c, "fr");
        assert_eq!(fr["subject"], "Salut {{ first_name }}");
        assert_eq!(fr["blocks"][0]["text"], "Hello there");
        assert_eq!(fr["blocks"][1]["columns"][0][0]["href"], "https://a.test/fr");
        assert_eq!(fr["blocks"][1]["columns"][0][0]["text"], "Shop", "no translation: the default");
        assert_eq!(fr["_lang"], "fr");
        assert!(!fr.to_string().contains("i18n") && fr.get("translations").is_none());
        let f = fields(&c).into_iter().find(|f| f.id == "b0:text").unwrap();
        assert_eq!(status(&c, &f, "fr"), "stale");
        assert_eq!(summary(&c, "fr")["stale"], 1);
    }

    #[test]
    fn tags_must_match_and_parse() {
        assert!(check_text(Kind::Text, "Hi {{ first_name }}", "Salut {{first_name}}").is_ok());
        assert!(check_text(Kind::Text, "Hi {{ first_name }}", "Salut").is_err());
        assert!(check_text(Kind::Text, "Hi", "Salut {{ person.properties.secret }}").is_err());
        assert!(check_text(Kind::Html, "{% if x %}a{% endif %}", "{% endif %}b{% if x %}").is_err(), "tags in the wrong order don't parse");
        assert!(check_text(Kind::Link, "https://a.test", "javascript:alert(1)").is_err());
        let mut c = email();
        add_language(&mut c, "en", "fr");
        c["translations"]["fields"] = json!({"fr": {"subject": {"text": "Salut", "status": "reviewed", "source": hash("Hi {{ first_name }}")}}});
        assert!(check_saved(&c).is_err(), "a current translation that drops a tag is refused");
        c["translations"]["fields"]["fr"]["subject"]["source"] = json!("00000000");
        assert!(check_saved(&c).is_ok(), "a stale one is never sent, so it can be saved");
        c["blocks"][0]["i18n"] = json!({"fr": {"level": {"text": "1", "status": "reviewed", "source": "00000000"}}});
        assert!(check_saved(&c).is_err(), "only text fields");
        c["blocks"][0]["i18n"] = json!({"French": {"text": {"text": "x", "status": "reviewed", "source": "00000000"}}});
        assert!(check_saved(&c).is_err(), "language tags only");
    }

    #[test]
    fn choosing_a_version() {
        let mut c = email();
        add_language(&mut c, "en", "fr");
        add_language(&mut c, "en", "pt-BR");
        assert_eq!(choose(&c, Some("fr-CA"), None).as_deref(), Some("fr"));
        assert_eq!(choose(&c, Some("pt-PT"), None).as_deref(), Some("pt-BR"));
        assert_eq!(choose(&c, Some("en-GB"), None).as_deref(), Some("en"));
        assert_eq!(choose(&c, Some("de"), None).as_deref(), Some("en"));
        assert_eq!(choose(&c, None, None).as_deref(), Some("en"));
        assert_eq!(choose(&c, Some("de"), Some("fr")).as_deref(), Some("fr"));
        assert_eq!(choose(&email(), Some("fr"), None), None, "no versions: nothing to do");
        assert_eq!(choose(&json!({"subject": "x", "_lang": "fr"}), None, None).as_deref(), Some(""), "content can't set _lang");
    }

    #[test]
    fn hash_matches_the_dashboard() {
        assert_eq!(hash(""), "811c9dc5");
        assert_eq!(hash("Hello"), "f55c314b");
    }
}
