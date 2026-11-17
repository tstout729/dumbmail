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
    /// What a person should check before this is sent: anything the conversion had to guess.
    pub review: Vec<String>,
}

/// Jinja comments are passed through untouched; tags inside them aren't tags.
static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)\{#.*?#\}|\{\{.*?\}\}|\{%.*?%\}").unwrap());
static COMMENT_BLOCK: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)\{%-?\s*comment\b[^%]*-?%\}(.*?)\{%-?\s*endcomment\s*-?%\}").unwrap());
static FILTER_ARG: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\|\s*([a-z_]+)\s*:\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[\w.$]+)"#).unwrap());
static LOOKUP: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\b(person|event)\s*\|\s*lookup\s*:\s*("[^"]*"|'[^']*')"#).unwrap());
static EVENT_VAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bevent\.([A-Za-z_$][\w$]*)").unwrap());
static PERSON_VAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bperson\.([A-Za-z_$][\w$ ]*?)\b").unwrap());
static AUTHORED: Lazy<Regex> = Lazy::new(|| Regex::new(r#"^\s*(organization(\.[A-Za-z_]\w*)+|'[^'{}]*'|"[^"{}]*")\s*(\|\s*[a-z_]+\s*(\(\s*('[^']*'|"[^"]*"|\d+)?\s*\))?\s*)*$"#).unwrap());
static SAFE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\|\s*safe\b").unwrap());

/// Shown in the email where Klaviyo content couldn't be converted, so it can't go out unnoticed.
pub const REVIEW_MARKER: &str = "[Needs review: Klaviyo content here couldn't be converted]";

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

/// `safe` turns off HTML escaping. Keep it only on the template author's own content (a quoted literal or
/// the organization's details), never on profile or event data a customer could have typed.
fn strip_unsafe(e: &str) -> (String, bool) {
    if !SAFE.is_match(e) {
        return (e.to_string(), false);
    }
    // The whole expression must be one literal or one organization field, then filters: anything that
    // also mentions a person, an event or any other value loses `safe`.
    let authored = AUTHORED.is_match(e);
    if authored {
        (e.to_string(), false)
    } else {
        (SAFE.replace_all(e, "").into_owned(), true)
    }
}

fn convert_tag(tag: &str, notes: &mut Vec<String>) -> Result<Option<String>, ()> {
    // Ok(None) = unchanged, Ok(Some(x)) = converted, Err = no equivalent.
    if tag.starts_with("{#") {
        return Ok(None);
    }
    if let Some(inner) = tag.strip_prefix("{{").and_then(|t| t.strip_suffix("}}")) {
        let body = inner.trim();
        let out = expr(body).ok_or(())?;
        let (out, stripped) = strip_unsafe(&out);
        if stripped {
            notes.push("HTML in a profile or event value is now shown as text, not as HTML (Klaviyo's `safe` was removed). Check how it looks.".into());
        }
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

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Open,
    Mid,
    Close,
    Plain,
}

fn role(tag: &str) -> Role {
    if !tag.starts_with("{%") {
        return Role::Plain;
    }
    let inner = tag.trim_start_matches("{%").trim_end_matches("%}").trim().trim_start_matches('-').trim_end_matches('-').trim();
    match inner.split_whitespace().next().unwrap_or("") {
        "if" | "for" => Role::Open,
        "elif" | "else" | "empty" => Role::Mid,
        "endif" | "endfor" => Role::Close,
        _ => Role::Plain,
    }
}

/// Neutralize anything that could open a tag, keeping DumbMail's unsubscribe link.
fn defang(s: &str) -> String {
    s.replace("{{ unsubscribe_url }}", "\u{1}").replace("{%", "&#123;%").replace("{{", "&#123;&#123;").replace("{#", "&#123;#").replace('\u{1}', "{{ unsubscribe_url }}")
}

pub fn convert(src: &str) -> Converted {
    let mut removed = Vec::new();
    let mut converted = 0;
    let mut notes: Vec<String> = Vec::new();
    // Klaviyo's {% comment %} blocks stay comments (Jinja's {# #}), so what's inside never shows.
    let src = COMMENT_BLOCK.replace_all(src, |c: &regex::Captures| format!("{{#{}#}}", c[1].replace("#}", "# }"))).into_owned();
    // Tokens: (start, end, role, conversion).
    let tags: Vec<(usize, usize, Role, Result<Option<String>, ()>)> = TAG.find_iter(&src).map(|m| (m.start(), m.end(), role(m.as_str()), convert_tag(m.as_str(), &mut notes))).collect();
    // Pair blocks; a block whose `if`/`for` (or `elif`) couldn't be converted is replaced whole by a marker,
    // never shown with both branches.
    let mut bad_span: Vec<(usize, usize)> = Vec::new();
    let mut stack: Vec<(usize, bool)> = Vec::new();
    let mut balanced = true;
    for (i, (_, _, r, c)) in tags.iter().enumerate() {
        match r {
            Role::Open => stack.push((i, c.is_err())),
            Role::Mid => match stack.last_mut() {
                Some(top) => top.1 |= c.is_err(),
                None => balanced = false,
            },
            Role::Close => match stack.pop() {
                Some((open, bad)) => {
                    if bad {
                        bad_span.push((open, i));
                    }
                }
                None => balanced = false,
            },
            Role::Plain => {}
        }
    }
    balanced &= stack.is_empty();
    let in_bad = |i: usize| bad_span.iter().any(|(a, b)| i >= *a && i <= *b);
    let mut out = String::with_capacity(src.len());
    let mut last = 0;
    let mut i = 0;
    while i < tags.len() {
        let (start, end, _, c) = &tags[i];
        out.push_str(&src[last..*start]);
        if balanced && in_bad(i) {
            // Skip the whole outermost bad block.
            let (_, b) = bad_span.iter().filter(|(a, b)| i >= *a && i <= *b).max_by_key(|(a, b)| b - a).copied().unwrap();
            let t: String = src[*start..*end].chars().take(200).collect();
            if !removed.contains(&t) {
                removed.push(t);
            }
            out.push_str(REVIEW_MARKER);
            notes.push("A Klaviyo condition or loop couldn't be converted; its content was replaced by a \"Needs review\" marker. Rebuild that part.".into());
            last = tags[b].1;
            i = b + 1;
            continue;
        }
        match c {
            Ok(None) => out.push_str(&src[*start..*end]),
            Ok(Some(x)) => {
                converted += 1;
                out.push_str(x);
            }
            Err(()) => {
                let t: String = src[*start..*end].chars().take(200).collect();
                if !removed.contains(&t) {
                    removed.push(t);
                }
            }
        }
        last = *end;
        i += 1;
    }
    out.push_str(&src[last..]);
    if !removed.is_empty() {
        notes.push(format!("Klaviyo tags DumbMail doesn't have were taken out: {}. Check the content still reads right.", removed.iter().map(|r| r.chars().take(60).collect::<String>()).collect::<Vec<_>>().join(", ")));
    }
    // Prove it compiles. If it still doesn't (unbalanced blocks, odd syntax), keep the design, show the
    // tags as text and mark it for review, rather than ship a template that fails at send time.
    if !balanced || minijinja::Environment::new().template_from_str(&out).is_err() {
        let plain = defang(&out);
        notes.push("Its template tags didn't fit together (an \"if\" or \"for\" without its end, or similar), so they're shown as plain text. Fix them before sending.".into());
        notes.dedup();
        return Converted { text: plain, removed, converted, review: notes };
    }
    notes.dedup();
    Converted { text: out, removed, converted, review: notes }
}

/// Klaviyo HTML → DumbMail email content: a full document goes in `html` (kept whole, with its own
/// styles), a fragment becomes an HTML block. The untouched original is kept under `imported`, with the
/// list of what a person must review (`imported.review`). Returns (content, removed tags, review notes).
pub fn email_content(html: &str, subject: &str, preheader: Option<&str>, from_name: Option<&str>, from_email: Option<&str>, reply_to: Option<&str>) -> (serde_json::Value, Vec<String>, Vec<String>) {
    let c = convert(html);
    let subj = convert(subject);
    let pre = preheader.map(convert);
    let mut removed = c.removed.clone();
    removed.extend(subj.removed.iter().cloned());
    let mut notes = c.review.clone();
    notes.extend(subj.review.iter().map(|n| format!("Subject: {n}")));
    if let Some(p) = &pre {
        removed.extend(p.removed.iter().cloned());
        notes.extend(p.review.iter().map(|n| format!("Preview text: {n}")));
    }
    removed.dedup();
    notes.dedup();
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
    content["imported"] = serde_json::json!({"from": "klaviyo", "original_html": html, "removed_tags": removed, "review": notes});
    (content, removed, notes)
}

/// The review notes carried in imported content (empty once a person cleared them).
pub fn review_notes(content: &serde_json::Value) -> Vec<String> {
    content["imported"]["review"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert!(!r.review.is_empty(), "removing content needs a person to look");
        let r = convert("Hi {{ first_name }}");
        assert!(r.review.is_empty(), "a clean conversion needs no review");
    }

    #[test]
    fn comments_stay_commented() {
        let r = convert("<p>Hi</p>{% comment %}Internal: {{ person.email }} gets 50% off{% endcomment %}<p>Bye</p>");
        let out = crate::render::render_str(&r.text, &json!({"person": {"email": "a@b.c"}}), true).unwrap();
        assert_eq!(out, "<p>Hi</p><p>Bye</p>", "{}", r.text);
        // Already-Jinja comments pass through too.
        let r = convert("A{# note {{ x }} #}B");
        assert_eq!(crate::render::render_str(&r.text, &json!({}), true).unwrap(), "AB");
    }

    #[test]
    fn an_unconvertible_condition_is_a_visible_marker_not_both_branches() {
        let src = "{% if person|has_tag:'vip' %}VIP price{% else %}Full price{% endif %} {{ first_name }} {% unsubscribe %}";
        let r = convert(src);
        assert!(!r.text.contains("VIP price") && !r.text.contains("Full price"), "{}", r.text);
        assert!(r.text.contains(REVIEW_MARKER));
        assert!(!r.review.is_empty());
        let out = crate::render::render_str(&r.text, &json!({"first_name": "Ann", "unsubscribe_url": "https://u"}), true).unwrap();
        assert!(out.contains(REVIEW_MARKER) && out.contains("Ann") && out.contains("Unsubscribe</a>"), "{out}");
        // A good block around a bad one keeps its own content.
        let r = convert("{% if first_name %}Hi {% if person|has_tag:'x' %}secret{% endif %}{% endif %}");
        let out = crate::render::render_str(&r.text, &json!({"first_name": "Ann"}), true).unwrap();
        assert_eq!(out, format!("Hi {REVIEW_MARKER}"));
        // Unbalanced tags: shown as text and flagged, never a template that fails at send time.
        let r = convert("{% if first_name %}Hi {{ first_name }}");
        assert!(crate::render::render_str(&r.text, &json!({}), true).is_ok(), "{}", r.text);
        assert!(r.review.iter().any(|n| n.contains("plain text")));
    }

    #[test]
    fn safe_only_on_authored_content() {
        let r = convert("{{ organization.name ~ person.first_name|safe }}");
        assert!(!r.text.contains("safe"), "a mixed expression loses safe: {}", r.text);
        let r = convert("{{ person.bio|safe }} {{ event.html|safe }} {{ organization.name|safe }}");
        assert!(r.text.contains("{{ person.properties.bio }}") && r.text.contains("{{ event.properties.html }}"), "{}", r.text);
        assert!(r.text.contains("{{ organization.name|safe }}"));
        assert!(!r.review.is_empty());
        let out = crate::render::render_str(&r.text, &json!({"person": {"properties": {"bio": "<script>x</script>"}}, "event": {"properties": {}}, "organization": {"name": "<b>Acme</b>"}}), true).unwrap();
        assert!(out.contains("&lt;script&gt;") && out.contains("<b>Acme</b>"), "{out}");
    }

    #[test]
    fn renders_with_full_and_missing_data() {
        let (content, _, notes) = email_content(
            "<p>Hi {{ person.first_name|default:'there' }}, your {{ event.ProductName }} for {{ event|lookup:'$value' }}.</p>{% if person.vip %}<p>VIP</p>{% endif %}{% unsubscribe %}",
            "For {{ first_name|default:'you' }}", None, None, None, None);
        assert!(notes.is_empty(), "{notes:?}");
        let html = content["blocks"][0]["html"].as_str().unwrap();
        let full = json!({"person": {"first_name": "Ann", "vip": true, "properties": {"vip": true}}, "event": {"properties": {"ProductName": "Mug", "$value": 12}}, "unsubscribe_url": "https://u"});
        let out = crate::render::render_str(html, &full, true).unwrap();
        assert!(out.contains("Hi Ann, your Mug for 12."), "{out}");
        // A profile with no name or properties, and an event without the properties the email uses.
        let empty = crate::render::render_str(html, &json!({"person": {"properties": {}}, "event": {"properties": {}}, "unsubscribe_url": "https://u"}), true).unwrap();
        assert!(empty.contains("Hi there, your  for .") && !empty.contains("VIP") && empty.contains("Unsubscribe</a>"), "{empty}");
        assert_eq!(crate::render::render_str(content["subject"].as_str().unwrap(), &json!({}), false).unwrap(), "For you");
    }

    #[test]
    fn full_documents_keep_their_styles() {
        let (content, removed, notes) = email_content("<html><head><style>p{color:red}</style></head><body><p>Hi {{ first_name|default:'you' }}</p>{% unsubscribe %}</body></html>", "Hello {{ first_name }}", Some("Preview"), Some("Acme"), None, None);
        assert!(content["html"].as_str().unwrap().contains("<style>"));
        assert!(content["html"].as_str().unwrap().contains("{{ unsubscribe_url }}"));
        assert_eq!(content["preheader"], "Preview");
        assert!(removed.is_empty() && notes.is_empty());
        assert!(content["imported"]["original_html"].as_str().unwrap().contains("{% unsubscribe %}"));
        let (frag, _, _) = email_content("<p>Hello</p>", "S", None, None, None, None);
        assert_eq!(frag["blocks"][0]["type"], "html");
        let (_, _, notes) = email_content("<p>Hi</p>", "Sale {% coupon_code 'X' %}", None, None, None, None);
        assert!(notes.iter().any(|n| n.starts_with("Subject:")), "subject warnings aren't dropped: {notes:?}");
    }
}
