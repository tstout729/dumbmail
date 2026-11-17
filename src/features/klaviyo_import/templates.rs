//! Klaviyo template language (Django-style) → DumbMail's Jinja.
//!
//! Most of Klaviyo's syntax is already valid Jinja. What differs is converted: filter arguments
//! (`|default:"x"` → `|default('x')`), Klaviyo's own tags (`{% unsubscribe %}`, `{% manage_preferences %}`),
//! `person|lookup:'Prop'`, event variables (`event.X` → `event.properties.X`) and organization fields.
//! A tag with no DumbMail equivalent is removed and listed, never left to break the template; the result
//! is always compiled once to prove it renders.

use once_cell::sync::Lazy;
use regex::Regex;

pub struct Converted {
    pub text: String,
    /// Tags removed because DumbMail has nothing like them, as they appeared in the original.
    pub removed: Vec<String>,
    /// Tags changed into their DumbMail form, as they appeared in the original (for the report).
    pub converted: usize,
}

static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)\{\{.*?\}\}|\{%.*?%\}").unwrap());
static FILTER_ARG: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\|\s*([a-z_]+)\s*:\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[\w.$]+)"#).unwrap());
static LOOKUP: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\b(person|event)\s*\|\s*lookup\s*:\s*("[^"]*"|'[^']*')"#).unwrap());
static EVENT_VAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bevent\.([A-Za-z_$][\w$]*)").unwrap());
static PERSON_VAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bperson\.([A-Za-z_$][\w$ ]*?)\b").unwrap());

/// Django/Klaviyo filters that exist in Jinja under the same name and take no argument.
const SAME_FILTERS: &[&str] = &["upper", "lower", "title", "capitalize", "trim", "length", "first", "last", "safe", "escape", "striptags", "abs", "int", "float", "string", "urlencode", "join", "list", "reverse", "sort", "unique", "wordcount", "center"];
/// Profile fields DumbMail fills on `person` directly; anything else is a custom property.
const PERSON_FIELDS: &[&str] = &["first_name", "last_name", "email", "phone_number", "external_id", "id", "properties"];

fn quote(arg: &str) -> String {
    let a = arg.trim();
    if a.starts_with('"') && a.ends_with('"') && a.len() >= 2 {
        let inner = &a[1..a.len() - 1];
        format!("'{}'", inner.replace('\'', "\\'"))
    } else {
        a.to_string()
    }
}

/// Convert the expression part of a tag. Returns None when it uses something DumbMail can't do.
fn expr(src: &str) -> Option<String> {
    let mut e = src.to_string();
    e = LOOKUP
        .replace_all(&e, |c: &regex::Captures| {
            let key = quote(&c[2]);
            if &c[1] == "person" { format!("person.properties[{key}]") } else { format!("event.properties[{key}]") }
        })
        .into_owned();
    let mut bad = false;
    e = FILTER_ARG
        .replace_all(&e, |c: &regex::Captures| {
            let arg = quote(&c[2]);
            match &c[1] {
                "default" | "default_if_none" => format!("|default({arg})"),
                "floatformat" => format!("|round({arg})"),
                "truncatechars" | "truncatewords" | "date" | "time" | "slice" | "cut" | "add" | "divisibleby" | "pluralize" | "yesno" => {
                    bad = true;
                    String::new()
                }
                other => format!("|{other}({arg})"),
            }
        })
        .into_owned();
    if bad {
        return None;
    }
    // Filters without arguments.
    for f in Regex::new(r"\|\s*([a-z_]+)").unwrap().captures_iter(&e.clone()) {
        let name = &f[1];
        if !(SAME_FILTERS.contains(&name) || ["default", "default_if_none", "round", "replace", "truncate"].contains(&name)) {
            match name {
                "floatformat" => e = e.replace("|floatformat", "|round(2)"),
                "default_if_none" => {}
                _ => return None,
            }
        }
    }
    e = e.replace("|default_if_none", "|default");
    // Event variables live under event.properties in DumbMail; `event.value` stays.
    e = EVENT_VAR
        .replace_all(&e, |c: &regex::Captures| {
            let k = &c[1];
            if k == "properties" || k == "value" {
                format!("event.{k}")
            } else if k == "extra" {
                // Klaviyo's `event.extra.x` is just a nested property.
                "event.properties.extra".to_string()
            } else if k.starts_with('$') {
                if k == "$value" { "event.value".to_string() } else { format!("event.properties['{k}']") }
            } else {
                format!("event.properties.{k}")
            }
        })
        .into_owned();
    e = PERSON_VAR
        .replace_all(&e, |c: &regex::Captures| {
            let k = c[1].trim();
            if PERSON_FIELDS.contains(&k) { format!("person.{k}") } else if k.contains(' ') || k.starts_with('$') { format!("person.properties['{k}']") } else { format!("person.properties.{k}") }
        })
        .into_owned();
    e = e
        .replace("organization.full_address", "organization.address")
        .replace("organization.url", "organization.website");
    // `$` isn't valid in a Jinja name.
    if Regex::new(r"(^|[^'\w])\$\w").unwrap().is_match(&e) {
        return None;
    }
    Some(e)
}

fn convert_tag(tag: &str) -> Result<Option<String>, ()> {
    // Ok(None) = unchanged, Ok(Some(x)) = converted, Err = no equivalent.
    if let Some(inner) = tag.strip_prefix("{{").and_then(|t| t.strip_suffix("}}")) {
        let body = inner.trim();
        let out = expr(body).ok_or(())?;
        return Ok(if out == body { None } else { Some(format!("{{{{ {out} }}}}")) });
    }
    let inner = tag.trim_start_matches("{%").trim_end_matches("%}").trim().trim_start_matches('-').trim_end_matches('-').trim();
    let (word, rest) = inner.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((inner, ""));
    match word {
        "unsubscribe_url" | "manage_preferences_url" | "unsubscribe_link" => Ok(Some("{{ unsubscribe_url }}".into())),
        "unsubscribe" | "manage_preferences" => {
            let label = rest.trim_matches(['"', '\'']);
            let label = if label.is_empty() { if word == "unsubscribe" { "Unsubscribe" } else { "Manage preferences" } } else { label };
            Ok(Some(format!(r#"<a href="{{{{ unsubscribe_url }}}}">{}</a>"#, label.replace('<', "&lt;"))))
        }
        "if" | "elif" | "for" => {
            let out = expr(rest).ok_or(())?;
            let out = out.replace(" == None", " is none").replace(" != None", " is not none");
            let changed = format!("{{% {word} {out} %}}");
            Ok(if out == rest { None } else { Some(changed) })
        }
        "else" | "endif" | "endfor" | "empty" => {
            if word == "empty" {
                // Django's for … empty is Jinja's for … else.
                Ok(Some("{% else %}".into()))
            } else {
                Ok(None)
            }
        }
        "currency_format" => {
            let out = expr(rest).ok_or(())?;
            Ok(Some(format!("{{{{ ({out})|round(2) }}}}")))
        }
        "comment" | "endcomment" => Err(()),
        _ => Err(()),
    }
}

pub fn convert(src: &str) -> Converted {
    let mut out = String::with_capacity(src.len());
    let mut removed = Vec::new();
    let mut converted = 0;
    let mut last = 0;
    for m in TAG.find_iter(src) {
        out.push_str(&src[last..m.start()]);
        match convert_tag(m.as_str()) {
            Ok(None) => out.push_str(m.as_str()),
            Ok(Some(x)) => {
                converted += 1;
                out.push_str(&x);
            }
            Err(()) => {
                let t: String = m.as_str().chars().take(200).collect();
                if !removed.contains(&t) {
                    removed.push(t);
                }
            }
        }
        last = m.end();
    }
    out.push_str(&src[last..]);
    // Prove it compiles. If it still doesn't (unbalanced blocks after a removal, odd syntax), keep the
    // design and drop the personalization rather than ship a template that fails at send time.
    if minijinja::Environment::new().template_from_str(&out).is_err() {
        let mut plain = String::with_capacity(src.len());
        let mut last = 0;
        for m in TAG.find_iter(&out) {
            plain.push_str(&out[last..m.start()]);
            if m.as_str() == "{{ unsubscribe_url }}" {
                plain.push_str(m.as_str());
            } else {
                let t: String = m.as_str().chars().take(200).collect();
                if !removed.contains(&t) {
                    removed.push(t);
                }
            }
            last = m.end();
        }
        plain.push_str(&out[last..]);
        // Stray braces left over can't open a tag any more.
        let plain = plain.replace("{%", "&#123;%").replace("{{ unsubscribe_url }}", "\u{1}").replace("{{", "&#123;&#123;").replace('\u{1}', "{{ unsubscribe_url }}");
        return Converted { text: plain, removed, converted };
    }
    Converted { text: out, removed, converted }
}

/// Klaviyo HTML → DumbMail email content: a full document goes in `html` (kept whole, with its own
/// styles), a fragment becomes an HTML block. The untouched original is kept under `imported`.
pub fn email_content(html: &str, subject: &str, preheader: Option<&str>, from_name: Option<&str>, from_email: Option<&str>, reply_to: Option<&str>) -> (serde_json::Value, Vec<String>) {
    let c = convert(html);
    let subj = convert(subject);
    let pre = preheader.map(convert);
    let mut removed = c.removed.clone();
    removed.extend(subj.removed.iter().cloned());
    if let Some(p) = &pre {
        removed.extend(p.removed.iter().cloned());
    }
    removed.dedup();
    let mut content = if c.text.to_lowercase().contains("<body") {
        serde_json::json!({"subject": subj.text, "html": c.text})
    } else {
        serde_json::json!({"subject": subj.text, "blocks": [{"type": "html", "html": c.text}]})
    };
    if let Some(p) = pre.filter(|p| !p.text.is_empty()) {
        content["preheader"] = p.text.into();
    }
    for (k, v) in [("from_name", from_name), ("from_email", from_email), ("reply_to", reply_to)] {
        if let Some(v) = v.filter(|v| !v.trim().is_empty()) {
            content[k] = v.into();
        }
    }
    content["imported"] = serde_json::json!({"from": "klaviyo", "original_html": html, "removed_tags": removed});
    (content, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> String {
        convert(s).text
    }

    #[test]
    fn converts_klaviyo_tags() {
        assert_eq!(c("Hi {{ first_name|default:'there' }}!"), "Hi {{ first_name|default('there') }}!");
        assert_eq!(c(r#"Hi {{ person.first_name|default:"friend" }}"#), "Hi {{ person.first_name|default('friend') }}");
        assert_eq!(c("{% unsubscribe %}"), r#"<a href="{{ unsubscribe_url }}">Unsubscribe</a>"#);
        assert_eq!(c("{% unsubscribe 'Stop these emails' %}"), r#"<a href="{{ unsubscribe_url }}">Stop these emails</a>"#);
        assert_eq!(c(r#"<a href="{% unsubscribe_url %}">x</a>"#), r#"<a href="{{ unsubscribe_url }}">x</a>"#);
        assert_eq!(c("{% manage_preferences %}"), r#"<a href="{{ unsubscribe_url }}">Manage preferences</a>"#);
        assert_eq!(c("{{ organization.name }}"), "{{ organization.name }}");
        assert_eq!(c("{{ organization.full_address }}"), "{{ organization.address }}");
        assert_eq!(c("{{ person|lookup:'Favorite Color' }}"), "{{ person.properties['Favorite Color'] }}");
        assert_eq!(c("{{ person.Birthday }}"), "{{ person.properties.Birthday }}");
        assert_eq!(c("{{ event.ProductName }}"), "{{ event.properties.ProductName }}");
        assert_eq!(c("{{ event|lookup:'$value' }}"), "{{ event.properties['$value'] }}");
        assert_eq!(c("{% for item in event.Items %}{{ item.title }}{% endfor %}"), "{% for item in event.properties.Items %}{{ item.title }}{% endfor %}");
        assert_eq!(c("{% if person.first_name %}Hi{% else %}Hey{% endif %}"), "{% if person.first_name %}Hi{% else %}Hey{% endif %}");
        assert_eq!(c("{{ event.value|floatformat:2 }}"), "{{ event.value|round(2) }}");
        assert_eq!(c("{{ first_name|title }}"), "{{ first_name|title }}");
    }

    #[test]
    fn removes_what_has_no_equivalent_and_still_compiles() {
        let r = convert("<p>{% web_view %}</p><p>Code: {% coupon_code 'WELCOME' %}</p><p>{{ first_name|truncatechars:5 }}</p>");
        assert_eq!(r.text, "<p></p><p>Code: </p><p></p>");
        assert_eq!(r.removed.len(), 3);
        // An unbalanced block after a removal falls back to plain text, never a broken template.
        let r = convert("{% if person|has_tag:'vip' %}VIP{% endif %} {{ first_name }} {% unsubscribe %}");
        assert!(minijinja::Environment::new().template_from_str(&r.text).is_ok(), "{}", r.text);
        assert!(r.text.contains("{{ unsubscribe_url }}"));
        assert!(!r.removed.is_empty());
    }

    #[test]
    fn full_documents_keep_their_styles() {
        let (content, removed) = email_content("<html><head><style>p{color:red}</style></head><body><p>Hi {{ first_name|default:'you' }}</p>{% unsubscribe %}</body></html>", "Hello {{ first_name }}", Some("Preview"), Some("Acme"), None, None);
        assert!(content["html"].as_str().unwrap().contains("<style>"));
        assert!(content["html"].as_str().unwrap().contains("{{ unsubscribe_url }}"));
        assert_eq!(content["preheader"], "Preview");
        assert!(removed.is_empty());
        assert!(content["imported"]["original_html"].as_str().unwrap().contains("{% unsubscribe %}"));
        let (frag, _) = email_content("<p>Hello</p>", "S", None, None, None, None);
        assert_eq!(frag["blocks"][0]["type"], "html");
    }
}
