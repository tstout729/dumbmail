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
    /// A `<` that starts a tag but can't be read as one: everything from there on, which must be
    /// exactly the original's (a browser would read it in ways this can't predict).
    Bad(String),
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

// The scanners below work on bytes and only ever slice at ASCII delimiters, so they never split
// a character.

fn at_template(b: &[u8], j: usize) -> bool {
    b[j..].starts_with(b"{{") || b[j..].starts_with(b"{%") || b[j..].starts_with(b"{#")
}

/// Where a template tag that starts at `i` (with `{{`, `{%` or `{#`) ends, quotes respected.
fn tag_end(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    let close: &[u8] = match &b[i..i + 2] {
        b"{{" => b"}}",
        b"{%" => b"%}",
        _ => b"#}",
    };
    let mut j = i + 2;
    let mut quote: Option<u8> = None;
    while j < b.len() {
        match quote {
            Some(q) if b[j] == q => quote = None,
            Some(_) => {}
            None if b[j] == b'\'' || b[j] == b'"' => quote = Some(b[j]),
            None if b[j..].starts_with(close) => return Some(j + 2),
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

fn starts_tag(b: &[u8], i: usize) -> bool {
    b[i] == b'<' && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'/' | b'!' | b'?'))
}

/// Skip a template tag at `j` if there is one (template tags are filled in before a browser
/// reads the markup, and their output is escaped).
fn skip_template(s: &str, j: usize) -> Option<Option<usize>> {
    at_template(s.as_bytes(), j).then(|| tag_end(s, j))
}

/// Read the HTML tag starting at `i` (a `<` followed by a letter, `/`, `!` or `?`) the way an HTML5
/// tokenizer does: the name ends at whitespace, `/` or `>`; a quote starts a value only right after
/// `=`; an unquoted value ends at whitespace or `>`. None when it never ends.
fn parse_tag(s: &str, i: usize) -> Option<(usize, Elem)> {
    let b = s.as_bytes();
    let ws = |c: u8| c.is_ascii_whitespace();
    if b[i..].starts_with(b"<!--") {
        let e = s[i + 4..].find("-->")? + i + 4 + 3;
        return Some((e, Elem { name: "!--".into(), attrs: Vec::new(), self_closing: false, raw: s[i..e].to_string() }));
    }
    if b[i + 1] == b'!' || b[i + 1] == b'?' {
        let e = s[i..].find('>')? + i + 1;
        let raw = s[i..e].split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        return Some((e, Elem { name: "!".into(), attrs: Vec::new(), self_closing: false, raw }));
    }
    let mut j = i + 1;
    let closing = b[j] == b'/';
    if closing {
        j += 1;
    }
    let ns = j;
    while j < b.len() && !ws(b[j]) && b[j] != b'/' && b[j] != b'>' {
        j += 1;
    }
    let name = format!("{}{}", if closing { "/" } else { "" }, s[ns..j].to_lowercase());
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while j < b.len() && ws(b[j]) {
            j += 1;
        }
        if j >= b.len() {
            return None;
        }
        match b[j] {
            b'>' => return Some((j + 1, Elem { name, attrs, self_closing, raw: String::new() })),
            b'/' => {
                j += 1;
                self_closing = b.get(j) == Some(&b'>');
                continue;
            }
            _ => {}
        }
        self_closing = false;
        // Attribute name (a leading `=` belongs to it).
        let an = j;
        j += 1;
        while j < b.len() && !ws(b[j]) && !matches!(b[j], b'/' | b'>' | b'=') {
            match skip_template(s, j) {
                Some(e) => j = e?,
                None => j += 1,
            }
        }
        let aname = s[an..j].to_lowercase();
        while j < b.len() && ws(b[j]) {
            j += 1;
        }
        if j < b.len() && b[j] == b'=' {
            j += 1;
            while j < b.len() && ws(b[j]) {
                j += 1;
            }
            if j >= b.len() {
                return None;
            }
            let value = if b[j] == b'"' || b[j] == b'\'' {
                let q = b[j];
                let vs = j + 1;
                j = vs;
                while j < b.len() && b[j] != q {
                    match skip_template(s, j) {
                        Some(e) => j = e?,
                        None => j += 1,
                    }
                }
                if j >= b.len() {
                    return None;
                }
                let v = s[vs..j].to_string();
                j += 1;
                v
            } else {
                let vs = j;
                while j < b.len() && !ws(b[j]) && b[j] != b'>' {
                    match skip_template(s, j) {
                        Some(e) => j = e?,
                        None => j += 1,
                    }
                }
                s[vs..j].to_string()
            };
            attrs.push((aname, Some(value)));
        } else {
            attrs.push((aname, None));
        }
    }
}

/// Read a string into tokens: text, template tags and HTML elements.
fn tokens(s: &str) -> Vec<Tok> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut ts) = (0, 0);
    let flush = |from: usize, to: usize, out: &mut Vec<Tok>| {
        if to > from {
            out.push(Tok::Text(s[from..to].to_string()));
        }
    };
    while i < b.len() {
        if at_template(b, i) {
            if let Some(e) = tag_end(s, i) {
                flush(ts, i, &mut out);
                out.push(template_tok(&s[i..e]));
                i = e;
                ts = e;
                continue;
            }
        } else if starts_tag(b, i) {
            flush(ts, i, &mut out);
            match parse_tag(s, i) {
                Some((e, el)) => {
                    out.push(Tok::Elem(el));
                    i = e;
                    ts = e;
                    continue;
                }
                None => {
                    out.push(Tok::Bad(s[i..].to_string()));
                    return out;
                }
            }
        }
        i += 1;
    }
    flush(ts, b.len(), &mut out);
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
        Tok::Bad(_) => "an unfinished tag".into(),
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

/// Characters that change how text around them is shown without showing anything themselves:
/// every Unicode format character (General_Category Cf: bidirectional controls, which can
/// reorder an address or a link, and zero-width and other invisible characters, which can make a
/// label look empty) and the blank "filler" letters. Zero-width joiners and non-joiners stay
/// allowed: Persian, Arabic, Indic scripts and emoji need them.
pub fn is_hidden_control(c: char) -> bool {
    if c == '\u{200C}' || c == '\u{200D}' {
        return false;
    }
    matches!(c as u32,
        // General_Category=Cf (Unicode 15).
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB
        | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F
        // Blank letters that render as nothing.
        | 0x115F | 0x1160 | 0x3164 | 0xFFA0 | 0x2800)
}

/// `s` with HTML character references decoded (`&#x202E;`, `&#0008238;`, `&rlm;`,
/// `&ZeroWidthSpace;` …), repeated until nothing changes, for checking what a reader could get.
/// Named references outside the ones that produce invisible characters are left as they are.
pub fn decode_entities(s: &str) -> String {
    static REF: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"&(#[xX][0-9a-fA-F]+|#[0-9]+|[A-Za-z][A-Za-z0-9]{1,31});?").unwrap());
    let mut cur = s.to_string();
    for _ in 0..4 {
        let next = REF
            .replace_all(&cur, |c: &regex::Captures<'_>| {
                let e = &c[1];
                let n: Option<u32> = if let Some(h) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
                    let h = h.trim_start_matches('0');
                    if h.len() > 6 { None } else { u32::from_str_radix(if h.is_empty() { "0" } else { h }, 16).ok() }
                } else if let Some(d) = e.strip_prefix('#') {
                    let d = d.trim_start_matches('0');
                    if d.len() > 7 { None } else { (if d.is_empty() { "0" } else { d }).parse().ok() }
                } else {
                    match e {
                        "lrm" => Some(0x200E),
                        "rlm" => Some(0x200F),
                        "shy" => Some(0xAD),
                        "zwnj" => Some(0x200C),
                        "zwj" => Some(0x200D),
                        "ZeroWidthSpace" | "NegativeVeryThinSpace" | "NegativeThinSpace" | "NegativeMediumSpace" | "NegativeThickSpace" => Some(0x200B),
                        "NoBreak" => Some(0x2060),
                        "ApplyFunction" | "af" => Some(0x2061),
                        "InvisibleTimes" | "it" => Some(0x2062),
                        "InvisibleComma" | "ic" => Some(0x2063),
                        "amp" => Some(0x26),
                        "nbsp" => Some(0xA0),
                        _ => None,
                    }
                };
                n.and_then(char::from_u32).map(String::from).unwrap_or_else(|| c[0].to_string())
            })
            .into_owned();
        if next == cur {
            break;
        }
        cur = next;
    }
    cur
}

/// Why a text can't be shown as written, if it can't: it hides characters (see
/// [`is_hidden_control`]), directly or as an HTML character reference.
pub fn hidden_controls(s: &str) -> Result<(), String> {
    if s.chars().any(is_hidden_control) || decode_entities(s).chars().any(is_hidden_control) {
        return Err("it contains invisible characters (direction marks or zero-width characters) that can hide or reorder text; type it without them".into());
    }
    Ok(())
}

/// `s` with every template tag replaced by `with` (for parsing an address that has tags in it).
pub fn without_tags(s: &str, with: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with("{{") || rest.starts_with("{%") || rest.starts_with("{#") {
            if let Some(e) = tag_end(s, i) {
                out.push_str(with);
                i = e;
                continue;
            }
        }
        let c = rest.chars().next().expect("not empty");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Whether `translation` changes only words of `original` (see the module docs).
pub fn compare(original: &str, translation: &str) -> Result<(), String> {
    let (o, t) = (tokens(original), tokens(translation));
    let structural = |v: &[Tok]| v.iter().filter(|x| matches!(x, Tok::Ctl(_) | Tok::Elem(_) | Tok::Bad(_))).cloned().collect::<Vec<_>>();
    let (os, ts) = (structural(&o), structural(&t));
    for (i, (a, b)) in os.iter().zip(&ts).enumerate() {
        match (a, b) {
            (Tok::Elem(x), Tok::Elem(y)) => same_elem(x, y)?,
            (Tok::Ctl(x), Tok::Ctl(y)) if x == y => {}
            (Tok::Bad(x), Tok::Bad(y)) if x == y => {}
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
    fn invisible_characters_are_caught_however_they_are_written() {
        for bad in ["a\u{202E}b", "a&#x202E;b", "a&#x0000202e;b", "a&#0008238;b", "a&#8203;b", "a&ZeroWidthSpace;b", "a&NegativeThinSpace;b", "a&rlm;b", "a&#xFEFF;b", "a\u{2066}b", "a\u{00AD}b", "a\u{3164}b", "a&amp;#x202E;b", "a\u{E0041}b"] {
            assert!(hidden_controls(bad).is_err(), "{bad:?}");
        }
        for ok in ["می\u{200C}خواهم", "👨\u{200D}👩", "Tom &amp; Jerry", "&lt;b&gt;", "a&#x41;b", "&copy; 2026"] {
            assert!(hidden_controls(ok).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn tags_are_read_the_way_a_browser_reads_them() {
        // A tag that never closes, or one that swallows the next one, isn't text.
        assert!(compare("<p>Hi</p>", "<p>Hi <img src=x onerror=alert(1) <b>x</b></p>").is_err());
        assert!(compare("<p>Hi</p>", "<p>Hi</p><img src=x onerror=alert(1)").is_err());
        assert!(compare("Win <SAVE", "Gagnez <SAVE x").is_err(), "an unreadable tag must match from there on");
        assert!(compare("Win <SAVE", "Gagnez <SAVE").is_ok());
        assert!(compare("Win <SAVE now", "Win <SAVE now").is_ok());
        // A quote inside a words attribute can't open a new attribute.
        assert!(compare("<img src=\"a.png\" alt=\"A bag\">", "<img src=\"a.png\" alt=\"x\" onerror=\"alert(1)\">").is_err());
        assert!(compare("<img src=\"a.png\" alt='A bag'>", "<img src=\"a.png\" alt='Un \"sac\"'>").is_ok());
        // An unquoted value ends at whitespace; a quote in the middle of one doesn't start a string.
        assert!(compare("<a href=https://a.test/x>Go</a>", "<a href=https://a.test/x>Aller</a>").is_ok());
        assert!(compare("<a title=a'b href=https://a.test>Go</a>", "<a title=a'b href=https://a.test>Aller</a>").is_ok());
        // Text in other scripts, in text, attributes and template tags, never splits a character.
        assert!(compare("<p title=\"日本\">こんにちは {{ x|default('名前') }}</p>", "<p title=\"Japon\">Bonjour {{ x|default('名前') }}</p>").is_ok());
        assert!(compare("<a href=\"https://a.test/é\">é</a>", "<a href=\"https://a.test/é\">ê</a>").is_ok());
        for weird in ["<é", "<a é=1>", "{{ 'é", "<a href=\"é", "<!-- é", "<p 日=本>x</p>", "<{{ é }}>", "<a {{ é }}>"] {
            let _ = compare(weird, weird);
            let _ = compare("x", weird);
        }
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
