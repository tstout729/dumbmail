//! What a translation may change: words, nothing else.
//!
//! The original and the translation are each read into template tokens (`{{ … }}` output tags,
//! `{% … %}` control tags, `{# … #}` comments) and HTML elements, and compared:
//!
//! - **Structure.** The sequence of HTML elements (start and end tags, comments, declarations) and
//!   template control tags must be identical, so tags keep their order and nesting. Every element
//!   keeps the same attributes with the same values, except the attributes that are text for
//!   people (`alt`, `title`, `aria-label`, `placeholder`), whose words may change but whose
//!   template tags may not. No element, link, image, style or attribute can be added, removed or
//!   changed. A plain-text field gets the same treatment, so it can contain no markup beyond what
//!   the original has.
//! - **Output tags in text** (`{{ first_name }}`) must be the same multiset, expression for
//!   expression and filter argument for filter argument (quoted strings exactly), and may move
//!   anywhere within the text, but never into or out of an attribute.
//!
//! Whitespace inside a tag outside quoted strings doesn't matter (`{{first_name}}` is
//! `{{ first_name }}`).

/// Attributes whose values are words for people, which a translation may change.
pub const TEXT_ATTRS: [&str; 4] = ["alt", "title", "aria-label", "placeholder"];

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Text(String),
    /// Normalized expression of `{{ … }}`.
    Out(String),
    /// `{% … %}` or `{# … #}`, normalized, with its opener.
    Ctl(String),
    Elem(Elem),
}

#[derive(Debug, Clone, PartialEq)]
struct Elem {
    /// `tag`, `/tag`, `!--` (comment) or `!` (declaration), lower case.
    name: String,
    /// (name, raw value) in source order; None for a bare attribute.
    attrs: Vec<(String, Option<String>)>,
    self_closing: bool,
    /// The whole raw text for comments and declarations.
    raw: String,
}

/// A template tag's inside with whitespace outside quoted strings normalized.
fn norm_expr(inner: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut space = false;
    for c in inner.trim().chars() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == q {
                    quote = None;
                }
            }
            None if c == '\'' || c == '"' => {
                if space && !out.is_empty() && !out.ends_with(['|', '(', ',', '[', '.', ':']) {
                    out.push(' ');
                }
                space = false;
                out.push(c);
                quote = Some(c);
            }
            None if c.is_whitespace() => space = true,
            None => {
                let tight = matches!(c, '|' | '(' | ')' | ',' | '[' | ']' | '.' | ':');
                if space && !tight && !out.is_empty() && !out.ends_with(['|', '(', ',', '[', '.', ':']) {
                    out.push(' ');
                }
                space = false;
                out.push(c);
            }
        }
    }
    out
}

/// Where a template tag that starts at `i` (with `{{`, `{%` or `{#`) ends, quotes respected.
fn tag_end(s: &str, i: usize) -> Option<usize> {
    let close = match &s[i..i + 2] {
        "{{" => "}}",
        "{%" => "%}",
        _ => "#}",
    };
    let b = s.as_bytes();
    let mut j = i + 2;
    let mut quote: Option<u8> = None;
    while j < b.len() {
        match quote {
            Some(q) if b[j] == q => quote = None,
            Some(_) => {}
            None if b[j] == b'\'' || b[j] == b'"' => quote = Some(b[j]),
            None if s[j..].starts_with(close) => return Some(j + 2),
            None => {}
        }
        j += 1;
    }
    None
}

fn template_tok(t: &str) -> Tok {
    let inner = &t[2..t.len() - 2];
    match &t[..2] {
        "{{" => Tok::Out(norm_expr(inner)),
        open => Tok::Ctl(format!("{open}{}", norm_expr(inner))),
    }
}

/// Where an HTML tag starting at `i` (`<`) ends: the `>` outside quotes and template tags.
fn html_end(s: &str, i: usize) -> Option<usize> {
    if s[i..].starts_with("<!--") {
        return s[i + 4..].find("-->").map(|e| i + 4 + e + 3);
    }
    let b = s.as_bytes();
    let mut j = i + 1;
    let mut quote: Option<u8> = None;
    while j < b.len() {
        if quote.is_none() && (s[j..].starts_with("{{") || s[j..].starts_with("{%") || s[j..].starts_with("{#")) {
            j = tag_end(s, j)?;
            continue;
        }
        match quote {
            Some(q) if b[j] == q => quote = None,
            Some(_) => {}
            None if b[j] == b'"' || b[j] == b'\'' => quote = Some(b[j]),
            None if b[j] == b'>' => return Some(j + 1),
            None if b[j] == b'<' => return None,
            None => {}
        }
        j += 1;
    }
    None
}

fn starts_tag(s: &str, i: usize) -> bool {
    let next = s[i + 1..].chars().next();
    next.is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!')
}

fn parse_elem(raw: &str) -> Elem {
    if raw.starts_with("<!--") {
        return Elem { name: "!--".into(), attrs: Vec::new(), self_closing: false, raw: raw.to_string() };
    }
    if raw.starts_with("<!") || raw.starts_with("<?") {
        return Elem { name: "!".into(), attrs: Vec::new(), self_closing: false, raw: raw.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase() };
    }
    let body = &raw[1..raw.len() - 1];
    let (closing, body) = match body.strip_prefix('/') {
        Some(b) => (true, b),
        None => (false, body),
    };
    let body = body.trim_end();
    let (self_closing, body) = match body.strip_suffix('/') {
        Some(b) => (true, b),
        None => (false, body),
    };
    let name_end = body.find(|c: char| c.is_whitespace()).unwrap_or(body.len());
    let name = format!("{}{}", if closing { "/" } else { "" }, body[..name_end].to_lowercase());
    let mut attrs = Vec::new();
    let rest = &body[name_end..];
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' {
            if rest[i..].starts_with("{{") || rest[i..].starts_with("{%") || rest[i..].starts_with("{#") {
                i = tag_end(rest, i).unwrap_or(b.len());
            } else {
                i += 1;
            }
        }
        let aname = rest[start..i].to_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let value = if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                let mut j = vs;
                while j < b.len() && b[j] != q {
                    if rest[j..].starts_with("{{") || rest[j..].starts_with("{%") || rest[j..].starts_with("{#") {
                        j = tag_end(rest, j).unwrap_or(b.len());
                    } else {
                        j += 1;
                    }
                }
                let v = rest[vs..j.min(b.len())].to_string();
                i = (j + 1).min(b.len());
                v
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                rest[vs..i].to_string()
            };
            attrs.push((aname, Some(value)));
        } else if !aname.is_empty() {
            attrs.push((aname, None));
        }
    }
    Elem { name, attrs, self_closing, raw: String::new() }
}

/// Read a string into tokens: text, template tags and HTML elements.
fn tokens(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    let flush = |text: &mut String, out: &mut Vec<Tok>| {
        if !text.is_empty() {
            out.push(Tok::Text(std::mem::take(text)));
        }
    };
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with("{{") || rest.starts_with("{%") || rest.starts_with("{#") {
            if let Some(e) = tag_end(s, i) {
                flush(&mut text, &mut out);
                out.push(template_tok(&s[i..e]));
                i = e;
                continue;
            }
        } else if rest.starts_with('<') && starts_tag(s, i) {
            if let Some(e) = html_end(s, i) {
                flush(&mut text, &mut out);
                out.push(Tok::Elem(parse_elem(&s[i..e])));
                i = e;
                continue;
            }
        }
        let c = rest.chars().next().expect("not empty");
        text.push(c);
        i += c.len_utf8();
    }
    flush(&mut text, &mut out);
    out
}

/// The template tags inside an attribute value: (output tags sorted, control tags in order), and
/// the value with every tag normalized (for exact comparison).
fn attr_tags(v: &str) -> (Vec<String>, Vec<String>, String) {
    let mut outs = Vec::new();
    let mut ctls = Vec::new();
    let mut norm = String::new();
    let mut i = 0;
    while i < v.len() {
        let rest = &v[i..];
        if rest.starts_with("{{") || rest.starts_with("{%") || rest.starts_with("{#") {
            if let Some(e) = tag_end(v, i) {
                match template_tok(&v[i..e]) {
                    Tok::Out(x) => {
                        norm.push_str(&format!("{{{{{x}}}}}"));
                        outs.push(x);
                    }
                    Tok::Ctl(x) => {
                        norm.push_str(&x);
                        ctls.push(x);
                    }
                    _ => {}
                }
                i = e;
                continue;
            }
        }
        let c = rest.chars().next().expect("not empty");
        norm.push(c);
        i += c.len_utf8();
    }
    outs.sort();
    (outs, ctls, norm)
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Ctl(x) => format!("the template tag {x}…"),
        Tok::Elem(e) if e.name == "!--" => "an HTML comment".into(),
        Tok::Elem(e) => format!("<{}>", e.name),
        _ => String::new(),
    }
}

fn same_elem(a: &Elem, b: &Elem) -> Result<(), String> {
    if a.name != b.name || a.self_closing != b.self_closing || a.raw != b.raw {
        return Err(format!("it must keep the original's markup exactly: <{}> became <{}>", a.name, b.name));
    }
    let mut aa = a.attrs.clone();
    let mut ba = b.attrs.clone();
    aa.sort_by(|x, y| x.0.cmp(&y.0));
    ba.sort_by(|x, y| x.0.cmp(&y.0));
    if aa.len() != ba.len() || aa.iter().zip(&ba).any(|(x, y)| x.0 != y.0) {
        return Err(format!("it must keep the attributes of <{}> exactly as they are", a.name));
    }
    for ((name, av), (_, bv)) in aa.iter().zip(&ba) {
        let (ao, ac, an) = attr_tags(av.as_deref().unwrap_or(""));
        let (bo, bc, bn) = attr_tags(bv.as_deref().unwrap_or(""));
        if TEXT_ATTRS.contains(&name.as_str()) {
            if ao != bo || ac != bc || av.is_some() != bv.is_some() {
                return Err(format!("the {name} of <{}> must keep the original's template tags", a.name));
            }
        } else if an != bn || av.is_some() != bv.is_some() {
            return Err(format!("only words can change: the {name} of <{}> must stay as it is", a.name));
        }
    }
    Ok(())
}

/// Whether `translation` changes only words of `original` (see the module docs).
pub fn compare(original: &str, translation: &str) -> Result<(), String> {
    let (o, t) = (tokens(original), tokens(translation));
    let structural = |v: &[Tok]| v.iter().filter(|x| matches!(x, Tok::Ctl(_) | Tok::Elem(_))).cloned().collect::<Vec<_>>();
    let (os, ts) = (structural(&o), structural(&t));
    for (i, (a, b)) in os.iter().zip(&ts).enumerate() {
        match (a, b) {
            (Tok::Elem(x), Tok::Elem(y)) => same_elem(x, y)?,
            (Tok::Ctl(x), Tok::Ctl(y)) if x == y => {}
            _ => return Err(format!("it must keep the original's tags and markup in the same order: {} at position {} became {}", describe(a), i + 1, describe(b))),
        }
    }
    match os.len().cmp(&ts.len()) {
        std::cmp::Ordering::Less => return Err(format!("it adds {} that the original doesn't have", describe(&ts[os.len()]))),
        std::cmp::Ordering::Greater => return Err(format!("it drops {} that the original has", describe(&os[ts.len()]))),
        std::cmp::Ordering::Equal => {}
    }
    let outs = |v: &[Tok]| {
        let mut x: Vec<String> = v.iter().filter_map(|t| if let Tok::Out(e) = t { Some(e.clone()) } else { None }).collect();
        x.sort();
        x
    };
    if outs(&o) != outs(&t) {
        return Err("it must keep exactly the template tags of the original (like {{ first_name }}), no more and no fewer".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_may_change_and_tags_may_move_within_text() {
        assert!(compare("Hi {{ first_name }}", "{{first_name}}, salut").is_ok());
        assert!(compare("<p>Hi <strong>{{ first_name }}</strong>, look.</p>", "<p>Salut <strong>{{ first_name }}</strong>, regardez.</p>").is_ok());
        assert!(compare("<img src=\"a.png\" alt=\"A bag\">", "<img src=\"a.png\" alt=\"Un sac\">").is_ok(), "alt is words");
        assert!(compare("{% if vip %}<b>Hi</b>{% endif %}", "{%if vip%}<b>Salut</b>{% endif %}").is_ok());
        assert!(compare("{{ x|default('a  b') }}", "{{ x | default('a  b') }}").is_ok());
        assert!(compare("Save 5 < 6", "Économisez 5 < 6").is_ok(), "a lone < is text");
        assert!(compare("<p>{{ a }} and {{ b }}</p>", "<p>{{ b }} et {{ a }}</p>").is_ok());
    }

    #[test]
    fn markup_and_destinations_cant_change() {
        // A new link, image, style or attribute.
        assert!(compare("<p>Hi</p>", "<p>Hi <a href=\"https://evil.test\">here</a></p>").is_err());
        assert!(compare("<p>Hi</p>", "<p style=\"color:red\">Hi</p>").is_err());
        assert!(compare("<a href=\"https://a.test\">Go</a>", "<a href=\"https://b.test\">Aller</a>").is_err(), "a destination");
        assert!(compare("Sale today", "Soldes <b>aujourd'hui</b>").is_err(), "no markup in a plain-text field");
        assert!(compare("<p>a</p><p>b</p>", "<p>a<p>b</p></p>").is_err(), "nesting");
        assert!(compare("<!--[if mso]>x<![endif]-->", "<!--[if !mso]>x<![endif]-->").is_err());
    }

    #[test]
    fn template_tags_stay_where_they_are() {
        // Out of text and into an attribute.
        assert!(compare("<a href=\"https://a.test\">Hi {{ first_name }}</a>", "<a href=\"https://a.test/{{ first_name }}\">Salut</a>").is_err());
        // Control tags reordered.
        assert!(compare("{% if a %}x{% endif %}{% if b %}y{% endif %}", "{% if b %}y{% endif %}{% if a %}x{% endif %}").is_err());
        // A filter argument with spaces in it changed.
        assert!(compare("{{ x|default('a  b') }}", "{{ x|default('a b') }}").is_err());
        assert!(compare("Hi {{ first_name }}", "Salut").is_err());
        assert!(compare("Hi", "Salut {{ person.email }}").is_err());
        assert!(compare("<img alt=\"{{ name }}\">", "<img alt=\"Photo\">").is_err(), "tags in words attributes stay");
        assert!(compare("Hi", "{# note #}Salut").is_err());
    }
}
