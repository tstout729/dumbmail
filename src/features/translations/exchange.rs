//! A message's strings for translators, as XLIFF 1.2 or CSV, and reading their work back.
//!
//! Each file says which message and language it's for (XLIFF: the file's `original` and
//! `target-language`; CSV: the `message` and `language` columns), and an import into anything else
//! is refused. Each string carries its field id, the default text and that text's hash; a
//! translation is used only when the field's default text is still exactly the one in the file:
//! anything changed since the export is skipped and reported, never applied to different text.
//! XLIFF is read with a real XML parser (no DTDs, so no entity tricks); files over [`MAX_BYTES`]
//! or with more than `fields::MAX_ENTRIES` strings are refused, never cut short.

use serde_json::{json, Value};

use super::fields::{self, Field};
use crate::error::{AppError, AppResult};

/// The largest file accepted.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// One string read from a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub id: String,
    /// The default text the translator saw.
    pub source: String,
    pub target: String,
    /// The hash of that default text, when the file carries it (XLIFF).
    pub hash: Option<String>,
}

/// What a file is for, and its strings.
#[derive(Debug, Clone)]
pub struct Imported {
    pub message: String,
    pub language: String,
    pub units: Vec<Unit>,
}

impl Imported {
    /// Refuse a file made for another message or language.
    pub fn check(&self, message: &str, lang: &str) -> AppResult<()> {
        if self.message != message {
            return Err(AppError::bad(format!("this file is for another message ({}); export the texts of this one and translate that file", self.message)));
        }
        if super::locale::normalize(&self.language).as_deref() != Some(lang) {
            return Err(AppError::bad(format!("this file is for {}, not {lang}", self.language)));
        }
        Ok(())
    }
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

/// The strings of an XLIFF 1.2 file (as exported: one `file`).
pub fn from_xliff(xml: &str) -> AppResult<Imported> {
    if xml.len() > MAX_BYTES {
        return Err(AppError::bad("that file is too big (2 MB at most)"));
    }
    let opts = roxmltree::ParsingOptions { allow_dtd: false, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(xml, opts).map_err(|e| AppError::bad(format!("that isn't a valid XLIFF file: {e}")))?;
    let root = doc.root_element();
    if root.tag_name().name() != "xliff" {
        return Err(AppError::bad("that isn't an XLIFF file"));
    }
    let files: Vec<roxmltree::Node<'_, '_>> = root.children().filter(|n| n.is_element() && n.tag_name().name() == "file").collect();
    let [file] = files.as_slice() else {
        return Err(AppError::bad("an XLIFF file for DumbMail has exactly one <file>"));
    };
    let message = file.attribute("original").unwrap_or("").to_string();
    let language = file.attribute("target-language").unwrap_or("").to_string();
    // All the text under a node, inline markup included (its text kept, its tags dropped).
    let text = |n: Option<roxmltree::Node<'_, '_>>| n.map(|n| n.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect::<String>()).unwrap_or_default();
    let mut units = Vec::new();
    for u in file.descendants().filter(|n| n.is_element() && n.tag_name().name() == "trans-unit") {
        if units.len() >= fields::MAX_ENTRIES {
            return Err(AppError::bad(format!("that file has more than {} texts", fields::MAX_ENTRIES)));
        }
        let Some(id) = u.attribute("id") else { return Err(AppError::bad("every trans-unit needs an id")) };
        let child = |name: &str| u.children().find(|c| c.is_element() && c.tag_name().name() == name);
        units.push(Unit { id: id.to_string(), source: text(child("source")), target: text(child("target")), hash: u.attribute("resname").map(str::to_string) });
    }
    Ok(Imported { message, language, units })
}

/// Whether a spreadsheet could read a cell as a formula: it starts with `=`, `+`, `-`, `@`, a tab
/// or a carriage return, possibly after apostrophes this guard added before.
fn formula_like(cell: &str) -> bool {
    cell.trim_start_matches('\'').starts_with(['=', '+', '-', '@', '\t', '\r'])
}

/// A cell safe to open in a spreadsheet: formula-like text gets a leading apostrophe.
fn guard(cell: &str) -> String {
    if formula_like(cell) { format!("'{cell}") } else { cell.to_string() }
}

/// The cell as it was before [`guard`].
fn unguard(cell: &str) -> String {
    match cell.strip_prefix('\'') {
        Some(rest) if formula_like(cell) => rest.to_string(),
        _ => cell.to_string(),
    }
}

/// CSV with columns message, language, id, where, source, translation, status. Cells a
/// spreadsheet would run as a formula start with an apostrophe; importing removes exactly that.
pub fn to_csv(content: &Value, lang: &str, message: &str) -> AppResult<String> {
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record(["message", "language", "id", "where", "source", "translation", "status"]).map_err(|e| AppError::Internal(e.into()))?;
    for f in fields::fields(content) {
        let st = fields::status(content, &f, lang);
        let target = fields::entry(content, &f.id, lang).and_then(|e| e.get("text")).and_then(|t| t.as_str()).filter(|_| st != "stale").unwrap_or("");
        w.write_record([guard(message), guard(lang), guard(&f.id), guard(&f.label), guard(&f.source), guard(target), st.to_string()]).map_err(|e| AppError::Internal(e.into()))?;
    }
    let bytes = w.into_inner().map_err(|e| AppError::Internal(anyhow::anyhow!("{e}")))?;
    String::from_utf8(bytes).map_err(|e| AppError::Internal(e.into()))
}

/// The rows of a CSV as exported (message, language, id, source and translation columns; one
/// message and language for the whole file).
pub fn from_csv(text: &str) -> AppResult<Imported> {
    if text.len() > MAX_BYTES {
        return Err(AppError::bad("that file is too big (2 MB at most)"));
    }
    let mut r = csv::ReaderBuilder::new().flexible(true).from_reader(text.trim_start_matches('\u{feff}').as_bytes());
    let headers = r.headers().map_err(|e| AppError::bad(format!("couldn't read the CSV: {e}")))?.clone();
    let col = |name: &str| headers.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let (Some(msg), Some(lang), Some(id), Some(source), Some(target)) = (col("message"), col("language"), col("id"), col("source"), col("translation")) else {
        return Err(AppError::bad("the CSV needs message, language, id, source and translation columns (export one to see the layout)"));
    };
    let mut units = Vec::new();
    let mut seen: Option<(String, String)> = None;
    for rec in r.records() {
        let rec = rec.map_err(|e| AppError::bad(format!("couldn't read the CSV: {e}")))?;
        if units.len() >= fields::MAX_ENTRIES {
            return Err(AppError::bad(format!("that file has more than {} texts", fields::MAX_ENTRIES)));
        }
        let here = (unguard(rec.get(msg).unwrap_or("")), unguard(rec.get(lang).unwrap_or("")));
        match &seen {
            None => seen = Some(here),
            Some(s) if *s != here => return Err(AppError::bad("every row of the CSV must be for the same message and language")),
            Some(_) => {}
        }
        units.push(Unit { id: unguard(rec.get(id).unwrap_or("")), source: unguard(rec.get(source).unwrap_or("")), target: unguard(rec.get(target).unwrap_or("")), hash: None });
    }
    let (message, language) = seen.unwrap_or_default();
    Ok(Imported { message, language, units })
}

/// Apply imported strings to `content` in `lang` as reviewed translations, each matched by its
/// field id and the exact default text (and its hash, when the file has it). Returns a summary:
/// how many were applied, left empty, skipped because the default changed, or refused (with why).
pub fn apply(content: &mut Value, lang: &str, units: &[Unit], own: &[String]) -> Value {
    let current: Vec<Field> = fields::fields(content);
    let (mut applied, mut empty, mut changed) = (0, 0, Vec::new());
    let mut refused: Vec<Value> = Vec::new();
    for u in units {
        let Some(f) = current.iter().find(|f| f.id == u.id) else {
            changed.push(u.id.clone());
            continue;
        };
        if f.source != u.source || u.hash.as_ref().is_some_and(|h| *h != fields::hash(&f.source)) {
            changed.push(u.id.clone());
            continue;
        }
        if u.target.trim().is_empty() {
            empty += 1;
            continue;
        }
        if f.kind == fields::Kind::Link && u.target == u.source {
            continue;
        }
        if let Err(why) = fields::check_text_for(f.kind, &f.source, &u.target, own) {
            refused.push(json!({"id": u.id, "why": why}));
            continue;
        }
        if fields::set_entry(content, &u.id, lang, Some((&u.target, fields::REVIEWED))).is_ok() {
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
        let mut imp = from_xliff(&x).unwrap();
        assert_eq!((imp.message.as_str(), imp.language.as_str(), imp.units.len()), ("campaign/1", "fr", 2));
        imp.check("campaign/1", "fr").unwrap();
        assert!(imp.check("campaign/2", "fr").is_err() && imp.check("campaign/1", "de").is_err());
        imp.units[0].target = "Salut & <welcome> {{ first_name }}".into();
        imp.units[1].target = "<p>Achetez « maintenant »</p>".into();
        let r = apply(&mut c, "fr", &imp.units, &[]);
        assert_eq!(r["applied"], 2, "{r}");
        assert_eq!(fields::entry(&c, "subject", "fr").unwrap()["text"], "Salut & <welcome> {{ first_name }}");
        let csv = to_csv(&c, "fr", "campaign/1").unwrap();
        let rows = from_csv(&csv).unwrap();
        assert_eq!((rows.message.as_str(), rows.language.as_str()), ("campaign/1", "fr"));
        assert_eq!(rows.units[1].target, "<p>Achetez « maintenant »</p>");
        // The default changed after the export: that string is skipped.
        c["subject"] = json!("Hello {{ first_name }}");
        let r = apply(&mut c, "fr", &rows.units, &[]);
        assert_eq!(r["changed_since_export"], json!(["subject"]));
        // A translation that drops a template tag is refused.
        let u = Unit { id: "subject".into(), source: "Hello {{ first_name }}".into(), target: "Bonjour".into(), hash: None };
        let r = apply(&mut c, "fr", &[u], &[]);
        assert_eq!(r["refused"][0]["id"], "subject");
        // The same text under the same id but a hash that doesn't match is skipped.
        let u = Unit { id: "subject".into(), source: "Hello {{ first_name }}".into(), target: "Salut {{ first_name }}".into(), hash: Some("00000000".into()) };
        assert_eq!(apply(&mut c, "fr", &[u], &[])["changed_since_export"], json!(["subject"]));
    }

    #[test]
    fn xliff_is_read_strictly() {
        assert!(from_xliff("not xml").is_err());
        assert!(from_xliff("<xliff><file original='m' target-language='fr'><body><trans-unit id='a'><source>x</source></body></file></xliff>").is_err(), "malformed");
        let dtd = r#"<?xml version="1.0"?><!DOCTYPE x [<!ENTITY e "boom">]><xliff><file original="m" target-language="fr"><body><trans-unit id="a"><source>&e;</source><target>y</target></trans-unit></body></file></xliff>"#;
        assert!(from_xliff(dtd).is_err(), "no DTDs or entities");
        let commented = r#"<xliff><file original="m" target-language="fr"><body><!-- <trans-unit id="a"><source>x</source><target>y</target></trans-unit> --></body></file></xliff>"#;
        assert!(from_xliff(commented).unwrap().units.is_empty(), "a commented-out unit isn't a unit");
        let two = r#"<xliff><file original="a" target-language="fr"/><file original="b" target-language="fr"/></xliff>"#;
        assert!(from_xliff(two).is_err());
        let many: String = (0..=fields::MAX_ENTRIES).map(|i| format!("<trans-unit id=\"u{i}\"><source>x</source><target>y</target></trans-unit>")).collect();
        let big = format!("<xliff><file original=\"m\" target-language=\"fr\"><body>{many}</body></file></xliff>");
        assert!(from_xliff(&big).is_err(), "too many units fails loudly");
    }

    #[test]
    fn csv_cells_cant_run_as_formulas_and_still_round_trip() {
        for cell in ["=SUM(A1)", "+1 503", "-50% today", "@here", "\tx", "'=already", "''+x", "'plain", "plain", ""] {
            let g = guard(cell);
            assert!(!g.starts_with(['=', '+', '-', '@', '\t', '\r']), "{g:?}");
            assert_eq!(unguard(&g), cell, "{cell:?}");
        }
        let c = json!({"subject": "-50% today", "blocks": [{"type": "heading", "text": "=HYPERLINK(\"https://evil.test\")"}]});
        let csv = to_csv(&c, "fr", "template/1").unwrap();
        assert!(csv.contains("'-50% today") && csv.contains("'=HYPERLINK"));
        let rows = from_csv(&csv).unwrap();
        assert_eq!(rows.units[0].source, "-50% today");
        assert_eq!(rows.units[1].source, "=HYPERLINK(\"https://evil.test\")");
        assert!(from_csv("id,source,translation\nsubject,a,b\n").is_err(), "a CSV must say what it's for");
    }

    #[test]
    fn reads_cdata_and_inline_markup() {
        let x = r#"<xliff version="1.2"><file original="m" target-language="fr"><body><trans-unit id="b0:text"><source>Hello</source><target><![CDATA[Bon<jour]]> <g id="1">toi</g></target></trans-unit></body></file></xliff>"#;
        let u = from_xliff(x).unwrap();
        assert_eq!(u.units[0], Unit { id: "b0:text".into(), source: "Hello".into(), target: "Bon<jour toi".into(), hash: None });
    }
}
