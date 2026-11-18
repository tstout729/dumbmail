//! A message's strings for translators, as XLIFF 1.2 or CSV, and reading their work back.
//!
//! Each string carries its field id and the default text. On import a translation is used only when
//! the default text in the file is still the message's: anything changed since the export is
//! skipped and reported, never applied to different text.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

use super::fields::{self, Field};
use crate::error::{AppError, AppResult};

/// The largest file accepted.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn xml_unesc(s: &str) -> String {
    static ENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"&(#x[0-9a-fA-F]+|#[0-9]+|amp|lt|gt|quot|apos);").unwrap());
    ENT.replace_all(s, |c: &regex::Captures<'_>| {
        let e = &c[1];
        match e {
            "amp" => "&".to_string(),
            "lt" => "<".to_string(),
            "gt" => ">".to_string(),
            "quot" => "\"".to_string(),
            "apos" => "'".to_string(),
            _ => {
                let n = if let Some(h) = e.strip_prefix("#x") { u32::from_str_radix(h, 16).ok() } else { e[1..].parse().ok() };
                n.and_then(char::from_u32).map(String::from).unwrap_or_default()
            }
        }
    })
    .into_owned()
}

/// A glossary line for translators, per term.
pub type GlossaryLine = String;

/// XLIFF 1.2 for `lang`: one trans-unit per string, with the current translation as the target.
pub fn to_xliff(content: &Value, original: &str, default: &str, lang: &str, glossary: &[GlossaryLine]) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xliff version=\"1.2\" xmlns=\"urn:oasis:names:tc:xliff:document:1.2\">\n");
    out.push_str(&format!("  <file original=\"{}\" source-language=\"{}\" target-language=\"{}\" datatype=\"html\">\n", xml_esc(original), xml_esc(default), xml_esc(lang)));
    out.push_str("    <header>\n      <note>Keep template tags like {{ first_name }} and {% coupon_code 'NAME' %} exactly as they are. Links are translated only if you change them.</note>\n");
    for g in glossary {
        out.push_str(&format!("      <note from=\"glossary\">{}</note>\n", xml_esc(g)));
    }
    out.push_str("    </header>\n    <body>\n");
    for f in fields::fields(content) {
        let st = fields::status(content, &f, lang);
        let target = fields::entry(content, &f.id, lang).and_then(|e| e.get("text")).and_then(|t| t.as_str()).filter(|_| st != "stale").unwrap_or("");
        let state = match st {
            "reviewed" => "final",
            "machine" => "needs-review-translation",
            _ => "new",
        };
        out.push_str(&format!("      <trans-unit id=\"{}\" resname=\"{}\">\n", xml_esc(&f.id), fields::hash(&f.source)));
        out.push_str(&format!("        <source xml:space=\"preserve\">{}</source>\n", xml_esc(&f.source)));
        out.push_str(&format!("        <target state=\"{state}\" xml:space=\"preserve\">{}</target>\n", xml_esc(target)));
        out.push_str(&format!("        <note>{} ({})</note>\n", xml_esc(&f.label), f.kind.name()));
        out.push_str("      </trans-unit>\n");
    }
    out.push_str("    </body>\n  </file>\n</xliff>\n");
    out
}

/// One translated string read from a file: (field id, default text, translation).
pub type Unit = (String, String, String);

fn inner_text(raw: &str) -> String {
    static CDATA: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<!\[CDATA\[(.*?)\]\]>").unwrap());
    static INLINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)</?(g|x|bx|ex|ph|mrk)\b[^>]*>").unwrap());
    // Inline markup some tools add around text is dropped; its text stays.
    let s = INLINE.replace_all(raw, "");
    let mut out = String::new();
    let mut last = 0;
    for c in CDATA.captures_iter(&s) {
        let m = c.get(0).expect("match");
        out.push_str(&xml_unesc(&s[last..m.start()]));
        out.push_str(&c[1]);
        last = m.end();
    }
    out.push_str(&xml_unesc(&s[last..]));
    out
}

/// The trans-units of an XLIFF 1.2 (or similar) file.
pub fn from_xliff(xml: &str) -> AppResult<Vec<Unit>> {
    static UNIT: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?s)<trans-unit\b([^>]*)>(.*?)</trans-unit>"#).unwrap());
    static ID: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\bid\s*=\s*"([^"]*)""#).unwrap());
    static SOURCE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?s)<source\b[^>]*>(.*?)</source>"#).unwrap());
    static TARGET: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?s)<target\b[^>]*>(.*?)</target>"#).unwrap());
    if xml.len() > MAX_BYTES {
        return Err(AppError::bad("that file is too big (2 MB at most)"));
    }
    if !xml.contains("<xliff") {
        return Err(AppError::bad("that isn't an XLIFF file"));
    }
    let mut out = Vec::new();
    for c in UNIT.captures_iter(xml).take(fields::MAX_ENTRIES) {
        let Some(id) = ID.captures(&c[1]).map(|m| xml_unesc(&m[1])) else { continue };
        let source = SOURCE.captures(&c[2]).map(|m| inner_text(&m[1])).unwrap_or_default();
        let target = TARGET.captures(&c[2]).map(|m| inner_text(&m[1])).unwrap_or_default();
        out.push((id, source, target));
    }
    Ok(out)
}

/// CSV with columns id, where, source, translation, status.
pub fn to_csv(content: &Value, lang: &str) -> AppResult<String> {
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record(["id", "where", "source", "translation", "status"]).map_err(|e| AppError::Internal(e.into()))?;
    for f in fields::fields(content) {
        let st = fields::status(content, &f, lang);
        let target = fields::entry(content, &f.id, lang).and_then(|e| e.get("text")).and_then(|t| t.as_str()).filter(|_| st != "stale").unwrap_or("");
        w.write_record([f.id.as_str(), f.label.as_str(), f.source.as_str(), target, st]).map_err(|e| AppError::Internal(e.into()))?;
    }
    let bytes = w.into_inner().map_err(|e| AppError::Internal(anyhow::anyhow!("{e}")))?;
    String::from_utf8(bytes).map_err(|e| AppError::Internal(e.into()))
}

/// The rows of a CSV with at least id, source and translation columns (header names as exported).
pub fn from_csv(text: &str) -> AppResult<Vec<Unit>> {
    if text.len() > MAX_BYTES {
        return Err(AppError::bad("that file is too big (2 MB at most)"));
    }
    let mut r = csv::ReaderBuilder::new().flexible(true).from_reader(text.trim_start_matches('\u{feff}').as_bytes());
    let headers = r.headers().map_err(|e| AppError::bad(format!("couldn't read the CSV: {e}")))?.clone();
    let col = |name: &str| headers.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let (Some(id), Some(source), Some(target)) = (col("id"), col("source"), col("translation")) else {
        return Err(AppError::bad("the CSV needs id, source and translation columns (export one to see the layout)"));
    };
    let mut out = Vec::new();
    for rec in r.records().take(fields::MAX_ENTRIES) {
        let rec = rec.map_err(|e| AppError::bad(format!("couldn't read the CSV: {e}")))?;
        out.push((rec.get(id).unwrap_or("").to_string(), rec.get(source).unwrap_or("").to_string(), rec.get(target).unwrap_or("").to_string()));
    }
    Ok(out)
}

/// Apply imported strings to `content` in `lang` as reviewed translations. Returns a summary:
/// how many were applied, left empty, skipped because the default changed, or refused (with why).
pub fn apply(content: &mut Value, lang: &str, units: &[Unit]) -> Value {
    let current: Vec<Field> = fields::fields(content);
    let (mut applied, mut empty, mut changed) = (0, 0, Vec::new());
    let mut refused: Vec<Value> = Vec::new();
    for (id, source, target) in units {
        let Some(f) = current.iter().find(|f| &f.id == id) else {
            changed.push(id.clone());
            continue;
        };
        if f.source != *source {
            changed.push(id.clone());
            continue;
        }
        if target.trim().is_empty() {
            empty += 1;
            continue;
        }
        if f.kind == fields::Kind::Link && target == source {
            continue;
        }
        if let Err(why) = fields::check_text(f.kind, &f.source, target) {
            refused.push(json!({"id": id, "why": why}));
            continue;
        }
        if fields::set_entry(content, id, lang, Some((target, fields::REVIEWED))).is_ok() {
            applied += 1;
        }
    }
    json!({"applied": applied, "empty": empty, "changed_since_export": changed, "refused": refused})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_xliff_and_csv() {
        let mut c = json!({"subject": "Hi & <welcome> {{ first_name }}", "blocks": [{"type": "text", "html": "<p>Shop \"now\"</p>"}]});
        let x = to_xliff(&c, "campaign/1", "en", "fr", &["DumbMail: never translate".into()]);
        assert!(x.contains("<source xml:space=\"preserve\">Hi &amp; &lt;welcome&gt; {{ first_name }}</source>"));
        let mut units = from_xliff(&x).unwrap();
        assert_eq!(units.len(), 2);
        units[0].2 = "Salut & <welcome> {{ first_name }}".into();
        units[1].2 = "<p>Achetez « maintenant »</p>".into();
        let r = apply(&mut c, "fr", &units);
        assert_eq!(r["applied"], 2, "{r}");
        assert_eq!(fields::entry(&c, "subject", "fr").unwrap()["text"], "Salut & <welcome> {{ first_name }}");
        let csv = to_csv(&c, "fr").unwrap();
        let rows = from_csv(&csv).unwrap();
        assert_eq!(rows[1].2, "<p>Achetez « maintenant »</p>");
        // The default changed after the export: that string is skipped.
        c["subject"] = json!("Hello {{ first_name }}");
        let r = apply(&mut c, "fr", &rows);
        assert_eq!(r["changed_since_export"], json!(["subject"]));
        // A translation that drops a template tag is refused.
        let r = apply(&mut c, "fr", &[("subject".into(), "Hello {{ first_name }}".into(), "Bonjour".into())]);
        assert_eq!(r["refused"][0]["id"], "subject");
        assert!(from_xliff("not xml").is_err());
    }

    #[test]
    fn reads_cdata_and_inline_markup() {
        let x = r#"<xliff version="1.2"><file><body><trans-unit id="b0:text"><source>Hello</source><target><![CDATA[Bon<jour]]> <g id="1">toi</g></target></trans-unit></body></file></xliff>"#;
        let u = from_xliff(x).unwrap();
        assert_eq!(u[0], ("b0:text".into(), "Hello".into(), "Bon<jour toi".into()));
    }
}
