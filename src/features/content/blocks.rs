//! HTML and plain text for the content module's email blocks: social links, video, countdown
//! timer, table, quote, header, menu bar, footer and image beside text, plus the internal
//! `if_open` / `if_close` markers that conditional display turns into.
//!
//! `render.rs` calls [`html`] and [`text`] for these block types. Everything here is table HTML with
//! inline styles and `role="presentation"`, widths set on tables and cells, and VML for Outlook's
//! buttons. Text fields go through the template engine like the core blocks (so `{{ first_name }}`
//! works); values that come from outside the account (thumbnails, icons) are computed by
//! [`super::prepare`] into `_`-prefixed fields that content can't set.

use serde_json::Value;

use crate::render::safe_color;

pub const NETWORKS: [&str; 10] = ["instagram", "facebook", "tiktok", "x", "youtube", "pinterest", "linkedin", "threads", "website", "email"];
pub const ICON_STYLES: [&str; 4] = ["color", "dark", "light", "white"];

pub fn network_label(n: &str) -> &'static str {
    match n {
        "instagram" => "Instagram",
        "facebook" => "Facebook",
        "tiktok" => "TikTok",
        "x" => "X",
        "youtube" => "YouTube",
        "pinterest" => "Pinterest",
        "linkedin" => "LinkedIn",
        "threads" => "Threads",
        "website" => "Website",
        "email" => "Email",
        _ => "Link",
    }
}

pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn s<'a>(b: &'a Value, k: &str) -> &'a str {
    b.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn num(b: &Value, k: &str, default: u64, lo: u64, hi: u64) -> u64 {
    b.get(k).and_then(|v| v.as_u64()).unwrap_or(default).clamp(lo, hi)
}

fn align(b: &Value) -> &'static str {
    match s(b, "align") {
        "center" => "center",
        "right" => "right",
        _ => "left",
    }
}

/// A link target: web, mail and phone links, an anchor, or a personalized link (`{{ ... }}`).
/// Anything else (javascript:, data:) becomes "#".
pub fn safe_href(raw: &str) -> String {
    let t = raw.trim();
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:") || lower.starts_with("tel:") || t.starts_with('#') || t.starts_with("{{") {
        esc(t)
    } else {
        "#".into()
    }
}

/// An image source: only web addresses (or a personalized one).
pub fn safe_src(raw: &str) -> Option<String> {
    let t = raw.trim();
    let lower = t.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://") || t.starts_with("{{")).then(|| esc(t))
}

/// Text that came from outside the account: escaped, and unable to open a template tag.
pub fn inert(s: &str) -> String {
    esc(s).replace('{', "&#123;").replace('}', "&#125;").replace('%', "&#37;")
}

fn links(b: &Value) -> Vec<(&str, &str)> {
    b.get("links")
        .and_then(|l| l.as_array())
        .map(|a| a.iter().take(8).map(|l| (s(l, "text"), s(l, "href"))).filter(|(t, _)| !t.trim().is_empty()).collect())
        .unwrap_or_default()
}

/// A button that keeps its shape in Outlook (VML) and everywhere else (a padded link).
pub fn button(text: &str, href: &str, color: &str, font: &str, align: &str) -> String {
    let width = (text.chars().count() as u64 * 9 + 56).clamp(120, 520);
    let href = safe_href(href);
    let text = esc(text);
    format!(
        r#"<table role="presentation" cellpadding="0" cellspacing="0" border="0" align="{align}" style="margin:0;"><tr><td align="center" bgcolor="{color}" style="border-radius:6px;background:{color};"><!--[if mso]><v:roundrect xmlns:v="urn:schemas-microsoft-com:vml" xmlns:w="urn:schemas-microsoft-com:office:word" href="{href}" style="height:44px;v-text-anchor:middle;width:{width}px;" arcsize="14%" stroke="f" fillcolor="{color}"><w:anchorlock/><center style="color:#ffffff;font-family:Arial,sans-serif;font-size:16px;font-weight:bold;">{text}</center></v:roundrect><![endif]--><!--[if !mso]><!-- --><a href="{href}" style="display:inline-block;padding:12px 24px;font-family:{font};font-size:16px;font-weight:600;line-height:20px;color:#ffffff;text-decoration:none;border-radius:6px;background:{color};">{text}</a><!--<![endif]--></td></tr></table>"#
    )
}

/// HTML for one of this module's block types (empty for anything else).
pub fn html(b: &Value, brand: &str, font: &str) -> String {
    match s(b, "type") {
        "social" => social_html(b, font),
        "video" => video_html(b, font),
        "countdown" => countdown_html(b),
        "table" => table_html(b, font),
        "quote" => quote_html(b, brand, font),
        "header" => header_html(b, brand, font),
        "nav" => nav_html(b, brand, font),
        "footer" => footer_html(b, font),
        "image_text" => image_text_html(b, brand, font),
        "if_open" => match super::prepare::checked_expr(s(b, "expr")) {
            Some(e) => format!("{{% if {e} %}}"),
            None => "{% if false %}".into(),
        },
        "if_close" => "{% endif %}".into(),
        _ => String::new(),
    }
}

/// The icons [`super::prepare`] worked out: (link, image URL, label).
fn icons(b: &Value) -> Vec<(&str, &str, &str)> {
    b.get("_icons")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|i| (s(i, "href"), s(i, "src"), s(i, "label"))).filter(|(h, src, _)| !h.is_empty() && !src.is_empty()).collect())
        .unwrap_or_default()
}

fn icon_row(b: &Value) -> String {
    let size = match b.get("size").and_then(|v| v.as_u64()) {
        Some(n) if n <= 24 => 24,
        Some(n) if n >= 40 => 40,
        _ => 32,
    };
    let gap = if size >= 40 { 8 } else { 6 };
    let cells: String = icons(b)
        .iter()
        .map(|(href, src, label)| {
            format!(
                r#"<td style="padding:0 {gap}px;"><a href="{href}" style="display:inline-block;text-decoration:none;"><img src="{src}" width="{size}" height="{size}" alt="{label}" style="display:block;width:{size}px;height:{size}px;border:0;"></a></td>"#,
                href = safe_href(href),
                src = esc(src),
                label = esc(label),
            )
        })
        .collect();
    if cells.is_empty() {
        return String::new();
    }
    format!(r#"<table role="presentation" cellpadding="0" cellspacing="0" border="0"><tr>{cells}</tr></table>"#)
}

fn social_html(b: &Value, _font: &str) -> String {
    let row = icon_row(b);
    if row.is_empty() {
        return String::new();
    }
    format!(r#"<tr><td align="{a}" style="padding:12px 24px;">{row}</td></tr>"#, a = align(b))
}

fn video_html(b: &Value, font: &str) -> String {
    let href = safe_href(s(b, "url"));
    let alt = if s(b, "alt").trim().is_empty() { "Play video".to_string() } else { esc(s(b, "alt")) };
    let w = num(b, "width", 552, 100, 552);
    let r = num(b, "radius", 6, 0, 40);
    let media = match safe_src(s(b, "thumbnail")) {
        Some(src) => format!(
            r#"<a href="{href}" style="display:block;text-decoration:none;"><img src="{src}" alt="{alt}" width="{w}" style="display:block;width:100%;max-width:{w}px;height:auto;border:0;border-radius:{r}px;"></a>"#
        ),
        // No thumbnail yet: a dark panel with a play link still works everywhere.
        None => format!(
            r##"<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tr><td align="center" bgcolor="#111827" style="background:#111827;border-radius:{r}px;padding:64px 16px;"><a href="{href}" style="font-family:{font};font-size:18px;font-weight:700;color:#ffffff;text-decoration:none;">&#9654;&nbsp; {alt}</a></td></tr></table>"##
        ),
    };
    let caption = if s(b, "caption").trim().is_empty() {
        String::new()
    } else {
        format!(r#"<p style="margin:8px 0 0;font-family:{font};font-size:14px;line-height:1.5;color:#4b5563;">{}</p>"#, esc(s(b, "caption")))
    };
    format!(r#"<tr><td align="center" style="padding:8px 24px;">{media}{caption}</td></tr>"#)
}

fn countdown_html(b: &Value) -> String {
    let Some(src) = safe_src(s(b, "_src")) else { return String::new() };
    format!(
        r#"<tr><td align="center" style="padding:12px 24px;"><img src="{src}" alt="{alt}" width="552" style="display:block;width:100%;max-width:552px;height:auto;border:0;"></td></tr>"#,
        alt = esc(s(b, "_alt")),
    )
}

/// The event list a table repeats a row for, like `event.properties.items`. Only plain dotted paths.
pub fn loop_source(b: &Value) -> Option<&str> {
    let src = b.get("loop").and_then(|l| l.get("source")).and_then(|v| v.as_str())?.trim();
    let ok = !src.is_empty()
        && src.len() <= 80
        && src.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !p.starts_with(|c: char| c.is_ascii_digit()));
    ok.then_some(src)
}

/// `src` as a loop source that's an empty list (not an error) when any part of the path is missing.
fn guarded(src: &str) -> String {
    let parts: Vec<&str> = src.split('.').collect();
    let checks: Vec<String> = (1..=parts.len()).map(|i| format!("{} is defined", parts[..i].join("."))).collect();
    format!("({src} if ({} and {src} is sequence) else [])", checks.join(" and "))
}

pub const TABLE_MAX_COLUMNS: usize = 6;
pub const TABLE_MAX_ROWS: usize = 50;
/// The most event items a table repeats a row for.
pub const TABLE_MAX_LOOP: usize = 50;

fn strings(v: Option<&Value>, max: usize) -> Vec<&str> {
    v.and_then(|r| r.as_array()).map(|a| a.iter().take(max).map(|c| c.as_str().unwrap_or("")).collect()).unwrap_or_default()
}

fn rows(v: Option<&Value>) -> Vec<Vec<&str>> {
    v.and_then(|r| r.as_array()).map(|a| a.iter().take(TABLE_MAX_ROWS).map(|r| strings(Some(r), TABLE_MAX_COLUMNS)).collect()).unwrap_or_default()
}

fn table_html(b: &Value, font: &str) -> String {
    let cols: Vec<(&str, &str)> = b
        .get("columns")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .take(TABLE_MAX_COLUMNS)
                .map(|c| (s(c, "label"), match s(c, "align") { "right" => "right", "center" => "center", _ => "left" }))
                .collect()
        })
        .unwrap_or_default();
    if cols.is_empty() {
        return String::new();
    }
    let n = cols.len();
    let al = |i: usize| cols.get(i).map(|c| c.1).unwrap_or("left");
    let cell = |i: usize, v: &str, style: &str| format!(r#"<td align="{a}" style="padding:8px 6px;{style}">{v}</td>"#, a = al(i), v = esc(v));
    let row = |cells: &[&str], style: &str| -> String {
        let tds: String = (0..n).map(|i| cell(i, cells.get(i).copied().unwrap_or(""), style)).collect();
        format!("<tr>{tds}</tr>")
    };
    let mut out = String::new();
    if cols.iter().any(|c| !c.0.trim().is_empty()) {
        let labels: Vec<&str> = cols.iter().map(|c| c.0).collect();
        out.push_str(&row(&labels, "border-bottom:2px solid #e5e7eb;font-weight:700;color:#111827;"));
    }
    let body = "border-bottom:1px solid #f3f4f6;";
    for r in rows(b.get("rows")) {
        out.push_str(&row(&r, body));
    }
    if let Some(src) = loop_source(b) {
        let cells = strings(b.get("loop").and_then(|l| l.get("cells")), TABLE_MAX_COLUMNS);
        out.push_str(&format!("{{% for item in {} %}}{{% if loop.index > {TABLE_MAX_LOOP} %}}{{% break %}}{{% endif %}}{}{{% endfor %}}", guarded(src), row(&cells, body)));
    }
    for r in rows(b.get("footer")) {
        out.push_str(&row(&r, "font-weight:700;color:#111827;"));
    }
    format!(
        r#"<tr><td style="padding:8px 24px;"><table width="100%" cellpadding="0" cellspacing="0" border="0" style="width:100%;border-collapse:collapse;font-family:{font};font-size:14px;line-height:1.4;color:#374151;">{out}</table></td></tr>"#
    )
}

fn quote_html(b: &Value, brand: &str, font: &str) -> String {
    if s(b, "text").trim().is_empty() {
        return String::new();
    }
    let card = s(b, "style") == "card";
    let a = if card { "center" } else { align(b) };
    let avatar = safe_src(s(b, "image"))
        .map(|src| format!(r#"<td valign="middle" style="padding-right:12px;"><img src="{src}" alt="{}" width="48" height="48" style="display:block;width:48px;height:48px;border:0;border-radius:24px;"></td>"#, esc(s(b, "author"))))
        .unwrap_or_default();
    let who = match (s(b, "author").trim(), s(b, "role").trim()) {
        ("", _) => String::new(),
        (name, "") => format!(r#"<strong style="color:#111827;">{}</strong>"#, esc(name)),
        (name, role) => format!(r#"<strong style="color:#111827;">{}</strong><br>{}"#, esc(name), esc(role)),
    };
    let byline = if who.is_empty() && avatar.is_empty() {
        String::new()
    } else {
        format!(r#"<table role="presentation" cellpadding="0" cellspacing="0" border="0" align="{a}"><tr>{avatar}<td valign="middle" align="left" style="font-family:{font};font-size:14px;line-height:1.4;color:#4b5563;">{who}</td></tr></table>"#)
    };
    let quote = format!(
        r#"<p style="margin:0 0 12px;font-family:{font};font-size:18px;line-height:1.5;color:#111827;font-style:italic;">&ldquo;{}&rdquo;</p>{byline}"#,
        esc(s(b, "text"))
    );
    let inner = if card {
        format!(r##"<td align="center" bgcolor="#f9fafb" style="background:#f9fafb;border-radius:8px;padding:24px;">{quote}</td>"##)
    } else {
        let bar = safe_color(s(b, "color"), brand);
        format!(r#"<td align="{a}" style="border-left:4px solid {bar};padding:4px 0 4px 16px;">{quote}</td>"#)
    };
    format!(r#"<tr><td style="padding:12px 24px;"><table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tr>{inner}</tr></table></td></tr>"#)
}

fn link_row(b: &Value, color: &str, font: &str, size: u32) -> String {
    let sep = if b.get("separator").and_then(|v| v.as_bool()).unwrap_or(false) { "&nbsp;&nbsp;|&nbsp;&nbsp;" } else { "&nbsp;&nbsp;&nbsp;&nbsp;" };
    links(b)
        .iter()
        .map(|(t, h)| format!(r#"<a href="{}" style="font-family:{font};font-size:{size}px;font-weight:600;color:{color};text-decoration:none;white-space:nowrap;">{}</a>"#, safe_href(h), esc(t)))
        .collect::<Vec<_>>()
        .join(&format!(r#"<span style="color:{color};">{sep}</span>"#))
}

fn header_html(b: &Value, brand: &str, font: &str) -> String {
    let bg = safe_color(s(b, "background"), "#ffffff");
    let color = safe_color(s(b, "link_color"), "#111827");
    let nav = link_row(b, &color, font, 14);
    let logo_w = num(b, "logo_width", 140, 40, 300);
    let logo = safe_src(s(b, "logo")).map(|src| {
        let img = format!(r#"<img src="{src}" alt="{}" width="{logo_w}" style="display:block;width:{logo_w}px;max-width:100%;height:auto;border:0;">"#, esc(s(b, "logo_alt")));
        match s(b, "logo_href") {
            "" => img,
            h => format!(r#"<a href="{}" style="text-decoration:none;">{img}</a>"#, safe_href(h)),
        }
    });
    let _ = brand;
    let inner = match logo {
        Some(img) if !nav.is_empty() => format!(
            r#"<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tr><td class="pl-col" valign="middle" width="40%" style="width:40%;padding:4px 0;">{img}</td><td class="pl-col" valign="middle" align="right" width="60%" style="width:60%;padding:4px 0;line-height:24px;">{nav}</td></tr></table>"#
        ),
        Some(img) => format!(r#"<table role="presentation" cellpadding="0" cellspacing="0" border="0" align="center"><tr><td>{img}</td></tr></table>"#),
        None if !nav.is_empty() => format!(r#"<div style="text-align:center;line-height:24px;">{nav}</div>"#),
        None => return String::new(),
    };
    format!(r#"<tr><td bgcolor="{bg}" style="background:{bg};padding:12px 24px;">{inner}</td></tr>"#)
}

fn nav_html(b: &Value, brand: &str, font: &str) -> String {
    let bg = safe_color(s(b, "background"), brand);
    let color = safe_color(s(b, "color"), "#ffffff");
    let row = link_row(b, &color, font, 14);
    if row.is_empty() {
        return String::new();
    }
    format!(r#"<tr><td align="{a}" bgcolor="{bg}" style="background:{bg};padding:12px 16px;line-height:24px;">{row}</td></tr>"#, a = if s(b, "align").is_empty() { "center" } else { align(b) })
}

fn footer_html(b: &Value, font: &str) -> String {
    let a = if s(b, "align").is_empty() { "center" } else { align(b) };
    let icons = if b.get("show_social").and_then(|v| v.as_bool()).unwrap_or(true) { icon_row(b) } else { String::new() };
    let icons = if icons.is_empty() { icons } else { format!(r#"<div style="padding-bottom:12px;">{icons}</div>"#) };
    let text = if s(b, "text").trim().is_empty() {
        String::new()
    } else {
        format!(r#"<p style="margin:0 0 8px;font-family:{font};font-size:13px;line-height:1.6;color:#6b7280;">{}</p>"#, esc(s(b, "text")).replace('\n', "<br>"))
    };
    let row = link_row(b, "#4b5563", font, 13);
    let row = if row.is_empty() { row } else { format!(r#"<div style="line-height:22px;">{row}</div>"#) };
    if icons.is_empty() && text.is_empty() && row.is_empty() {
        return String::new();
    }
    format!(r#"<tr><td align="{a}" style="padding:16px 24px;border-top:1px solid #f3f4f6;">{icons}{text}{row}</td></tr>"#)
}

fn image_text_html(b: &Value, brand: &str, font: &str) -> String {
    let right = s(b, "image_position") == "right";
    let iw = match b.get("image_width").and_then(|v| v.as_u64()) {
        Some(n) if n <= 35 => 33,
        Some(n) if n >= 45 => 50,
        _ => 40,
    };
    let tw = 100 - iw;
    let r = num(b, "radius", 6, 0, 40);
    let img = match safe_src(s(b, "src")) {
        Some(src) => {
            let img = format!(r#"<img src="{src}" alt="{}" width="260" style="display:block;width:100%;max-width:260px;height:auto;border:0;border-radius:{r}px;">"#, esc(s(b, "alt")));
            match s(b, "href") {
                "" => img,
                h => format!(r#"<a href="{}" style="text-decoration:none;">{img}</a>"#, safe_href(h)),
            }
        }
        None => String::new(),
    };
    let heading = if s(b, "heading").trim().is_empty() {
        String::new()
    } else {
        format!(r#"<h3 style="margin:0 0 8px;font-family:{font};font-size:20px;line-height:1.3;color:#111827;font-weight:700;">{}</h3>"#, esc(s(b, "heading")))
    };
    let body = if s(b, "html").trim().is_empty() { String::new() } else { format!(r#"<div style="font-family:{font};font-size:16px;line-height:1.6;color:#374151;">{}</div>"#, s(b, "html")) };
    let btn = if s(b, "button_text").trim().is_empty() {
        String::new()
    } else {
        format!(r#"<div style="padding-top:12px;">{}</div>"#, button(s(b, "button_text"), s(b, "button_href"), &safe_color(s(b, "button_color"), brand), font, "left"))
    };
    let (img_pad, text_pad) = if right { ("padding:0 0 12px 12px;", "padding:0 12px 12px 0;") } else { ("padding:0 12px 12px 0;", "padding:0 0 12px 12px;") };
    // With the image on the right, `dir="rtl"` swaps the cells on wide screens while the image
    // still comes first when the cells stack on phones.
    let dir = if right { "rtl" } else { "ltr" };
    format!(
        r#"<tr><td style="padding:12px 24px;"><table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" dir="{dir}"><tr><td class="pl-col" dir="ltr" valign="middle" width="{iw}%" style="width:{iw}%;{img_pad}">{img}</td><td class="pl-col" dir="ltr" valign="middle" width="{tw}%" style="width:{tw}%;{text_pad}text-align:left;">{heading}{body}{btn}</td></tr></table></td></tr>"#
    )
}

fn strip_tags(html: &str) -> String {
    static TAGS: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"<[^>]+>").unwrap());
    static BR: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?i)<br\s*/?>|</p>|</li>").unwrap());
    let t = BR.replace_all(html, "\n");
    TAGS.replace_all(&t, "").replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").trim().to_string()
}

fn text_links(b: &Value, out: &mut String) {
    for (t, h) in links(b) {
        out.push_str(&format!("{t}: {h}\n"));
    }
}

fn text_icons(b: &Value, out: &mut String) {
    for (href, _, label) in icons(b) {
        out.push_str(&format!("{label}: {}\n", href.trim_start_matches("mailto:")));
    }
}

/// The plain-text part for one of this module's block types.
pub fn text(b: &Value, out: &mut String) {
    match s(b, "type") {
        "social" => {
            text_icons(b, out);
            out.push('\n');
        }
        "video" => {
            let label = if s(b, "alt").trim().is_empty() { "Watch the video" } else { s(b, "alt") };
            out.push_str(&format!("{label}: {}\n\n", s(b, "url")));
        }
        "countdown" => {
            if !s(b, "_alt").is_empty() {
                out.push_str(&format!("{}\n\n", s(b, "_alt")));
            }
        }
        "table" => {
            let cols: Vec<&str> = b.get("columns").and_then(|c| c.as_array()).map(|a| a.iter().take(TABLE_MAX_COLUMNS).map(|c| s(c, "label")).collect()).unwrap_or_default();
            if cols.is_empty() {
                return;
            }
            if cols.iter().any(|c| !c.trim().is_empty()) {
                out.push_str(&format!("{}\n", cols.join(" | ")));
            }
            for r in rows(b.get("rows")) {
                out.push_str(&format!("{}\n", r.join(" | ")));
            }
            if let Some(src) = loop_source(b) {
                let cells = strings(b.get("loop").and_then(|l| l.get("cells")), TABLE_MAX_COLUMNS);
                out.push_str(&format!("{{% for item in {} %}}{{% if loop.index > {TABLE_MAX_LOOP} %}}{{% break %}}{{% endif %}}{}\n{{% endfor %}}", guarded(src), cells.join(" | ")));
            }
            for r in rows(b.get("footer")) {
                out.push_str(&format!("{}\n", r.join(" | ")));
            }
            out.push('\n');
        }
        "quote" => {
            if s(b, "text").trim().is_empty() {
                return;
            }
            let who = [s(b, "author").trim(), s(b, "role").trim()].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ");
            out.push_str(&format!("\"{}\"{}\n\n", s(b, "text"), if who.is_empty() { String::new() } else { format!(" - {who}") }));
        }
        "header" | "nav" => {
            text_links(b, out);
            out.push('\n');
        }
        "footer" => {
            if !s(b, "text").trim().is_empty() {
                out.push_str(&format!("{}\n", s(b, "text")));
            }
            if b.get("show_social").and_then(|v| v.as_bool()).unwrap_or(true) {
                text_icons(b, out);
            }
            text_links(b, out);
            out.push('\n');
        }
        "image_text" => {
            if !s(b, "heading").trim().is_empty() {
                out.push_str(&format!("{}\n\n", s(b, "heading")));
            }
            let t = strip_tags(s(b, "html"));
            if !t.is_empty() {
                out.push_str(&format!("{t}\n\n"));
            }
            if !s(b, "button_text").trim().is_empty() {
                out.push_str(&format!("{}: {}\n\n", s(b, "button_text"), s(b, "button_href")));
            } else if !s(b, "href").is_empty() {
                out.push_str(&format!("{}\n\n", s(b, "href")));
            }
        }
        "if_open" => match super::prepare::checked_expr(s(b, "expr")) {
            Some(e) => out.push_str(&format!("{{% if {e} %}}")),
            None => out.push_str("{% if false %}"),
        },
        "if_close" => out.push_str("{% endif %}"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hrefs_and_sources_are_limited_to_safe_schemes() {
        assert_eq!(safe_href("javascript:alert(1)"), "#");
        assert_eq!(safe_href(" JavaScript:alert(1)"), "#");
        assert_eq!(safe_href("data:text/html,x"), "#");
        assert_eq!(safe_href("https://a.test/?x=\"1\""), "https://a.test/?x=&quot;1&quot;");
        assert_eq!(safe_href("{{ event.properties.url }}"), "{{ event.properties.url }}");
        assert_eq!(safe_href("mailto:hi@a.test"), "mailto:hi@a.test");
        assert!(safe_src("javascript:x").is_none());
        assert!(safe_src("https://a.test/x.png").is_some());
    }

    #[test]
    fn table_loop_source_must_be_a_plain_path() {
        assert_eq!(loop_source(&json!({"loop": {"source": "event.properties.items"}})), Some("event.properties.items"));
        assert_eq!(loop_source(&json!({"loop": {"source": "event.items %}{{ 7*7 }}"}})), None);
        assert_eq!(loop_source(&json!({"loop": {"source": "a..b"}})), None);
        assert_eq!(loop_source(&json!({"loop": {"source": "x.1y"}})), None);
    }

    #[test]
    fn inert_text_cannot_open_a_tag() {
        assert_eq!(inert("{{ x }} {% if %}"), "&#123;&#123; x &#125;&#125; &#123;&#37; if &#37;&#125;");
    }
}
