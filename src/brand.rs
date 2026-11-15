//! Brand import: read a customer's website and pull out their name, logo, colors, mailing
//! address, contact email and social links, so setting up a brand is "paste your URL".
//!
//! [`analyze`] is a pure function over one HTML page (easy to test); [`import`] fetches the page,
//! its stylesheets and logo through the SSRF-safe fetcher and combines everything.

use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use regex::Regex;
use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::error::{AppError, AppResult};
use crate::flows::fetch_public;
use crate::models::normalize_email;

/// Social networks we recognize, in the order emails show them.
pub const SOCIAL_NETWORKS: [&str; 8] = ["instagram", "facebook", "tiktok", "x", "youtube", "pinterest", "linkedin", "threads"];

/// What we found on the site. Every field is a suggestion; nothing is saved until the user (or an
/// MCP caller with `apply`) says so.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct BrandKit {
    /// The site's home page after redirects.
    pub website: String,
    pub name: Option<String>,
    pub logo_url: Option<String>,
    /// The best guess for the brand color, also `colors[0]`.
    pub brand_color: Option<String>,
    /// Up to five candidate brand colors, best first.
    pub colors: Vec<String>,
    pub address: Option<String>,
    /// A contact address on the site's own domain if one is listed, else `hello@<domain>`.
    pub from_email: Option<String>,
    /// True when `from_email` was listed on the site rather than guessed.
    pub from_email_found: bool,
    pub social_links: BTreeMap<String, String>,
}

/// Everything [`analyze`] can learn from one HTML page.
#[derive(Debug, Default)]
pub struct PageFindings {
    pub name: Option<String>,
    /// Logo URLs, best first. Raster images come before SVGs because most email apps can't show SVG.
    pub logo_candidates: Vec<String>,
    pub stylesheets: Vec<String>,
    pub colors: Vec<(Rgb, f32)>,
    pub address: Option<String>,
    pub emails: Vec<String>,
    pub social_links: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// (saturation, lightness) in HSL, each 0..1.
    fn sl(self) -> (f32, f32) {
        let (r, g, b) = (self.0 as f32 / 255.0, self.1 as f32 / 255.0, self.2 as f32 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.0;
        let d = max - min;
        let s = if d == 0.0 { 0.0 } else { d / (1.0 - (2.0 * l - 1.0).abs()) };
        (s, l)
    }

    /// Grays, near-white and near-black: never picked as a brand color while a real color exists.
    fn is_neutral(self) -> bool {
        let (s, l) = self.sl();
        s < 0.25 || l > 0.93 || l < 0.07
    }

    fn dist(self, o: Rgb) -> f32 {
        let d = |a: u8, b: u8| (a as f32 - b as f32).powi(2);
        (d(self.0, o.0) + d(self.1, o.1) + d(self.2, o.2)).sqrt()
    }
}

/// Parse a CSS color literal: `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, `rgb()`/`rgba()`.
/// Mostly transparent colors are ignored.
pub fn parse_color(s: &str) -> Option<Rgb> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix('#') {
        let h = h.trim();
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let x = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).ok();
        return match h.len() {
            3 | 4 => {
                if h.len() == 4 && x(3, 1)? < 8 {
                    return None;
                }
                Some(Rgb(x(0, 1)? * 17, x(1, 1)? * 17, x(2, 1)? * 17))
            }
            6 | 8 => {
                if h.len() == 8 && x(6, 2)? < 128 {
                    return None;
                }
                Some(Rgb(x(0, 2)?, x(2, 2)?, x(4, 2)?))
            }
            _ => None,
        };
    }
    let lower = s.to_ascii_lowercase();
    let inner = lower.strip_prefix("rgba(").or_else(|| lower.strip_prefix("rgb("))?.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(|c: char| c == ',' || c == '/' || c.is_whitespace()).filter(|p| !p.is_empty()).collect();
    if parts.len() < 3 {
        return None;
    }
    let ch = |p: &str| -> Option<u8> {
        if let Some(pct) = p.strip_suffix('%') {
            Some((pct.parse::<f32>().ok()?.clamp(0.0, 100.0) * 2.55).round() as u8)
        } else {
            Some(p.parse::<f32>().ok()?.clamp(0.0, 255.0).round() as u8)
        }
    };
    if let Some(a) = parts.get(3) {
        let alpha = if let Some(pct) = a.strip_suffix('%') { pct.parse::<f32>().ok()? / 100.0 } else { a.parse::<f32>().ok()? };
        if alpha < 0.5 {
            return None;
        }
    }
    Some(Rgb(ch(parts[0])?, ch(parts[1])?, ch(parts[2])?))
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}

static COLOR_LITERAL: &str = r"#[0-9a-fA-F]{3,8}\b|rgba?\([^)]{5,40}\)";
static CSS_VAR: Lazy<Regex> = Lazy::new(|| Regex::new(&format!(r"--([A-Za-z0-9_-]{{1,60}})\s*:\s*({COLOR_LITERAL})")).unwrap());
static CSS_RULE: Lazy<Regex> = Lazy::new(|| Regex::new(r"([^{}]{1,400})\{([^{}]{1,3000})\}").unwrap());
static CSS_BG: Lazy<Regex> = Lazy::new(|| Regex::new(&format!(r"(?i)background(?:-color)?\s*:\s*({COLOR_LITERAL})")).unwrap());
static CSS_ANY: Lazy<Regex> = Lazy::new(|| Regex::new(&format!(r"(?i)(?:^|[;{{\s])(?:color|background|background-color|border-color|fill)\s*:\s*({COLOR_LITERAL})")).unwrap());
static EMAIL_TEXT: Lazy<Regex> = Lazy::new(|| Regex::new(r"[A-Za-z0-9._%+-]{1,64}@[A-Za-z0-9.-]{1,200}\.[A-Za-z]{2,24}").unwrap());
static TITLE_SPLIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+[|\-–—·•:]\s+|\s*[|•·]\s*").unwrap());
static STREET_ADDRESS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(\d{1,6}[A-Za-z]?\s+(?:[A-Za-z0-9.'#-]+\s+){0,5}?",
        r"(?i:St|Street|Ave|Avenue|Blvd|Boulevard|Rd|Road|Dr|Drive|Ln|Lane|Way|Ct|Court|Pl|Place|Pkwy|Parkway|Hwy|Highway|Ter|Terrace|Cir|Circle|Sq|Square|Loop|Trail|Trl|Broadway)\.?",
        r"(?:\s+(?:N|S|E|W|NE|NW|SE|SW)\.?)?",
        r"(?:[,\s]+(?i:Suite|Ste\.?|Unit|Apt\.?|Floor|Fl\.?|#)\s*[\w-]+)?",
        r"|(?i:P\.?\s?O\.?\s+Box)\s+\d+)",
        r"[,\s]+([A-Z][A-Za-z.'-]+(?:[ ][A-Z][A-Za-z.'-]+){0,3}),?\s+([A-Z]{2})\.?,?\s+(\d{5}(?:-\d{4})?)\b",
    ))
    .unwrap()
});

const US_STATES: [&str; 54] = [
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV",
    "NH", "NJ", "NM", "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY", "DC", "PR", "GU", "VI",
];

/// Parsed names of colors that usually hold the brand color in a site's CSS variables.
fn brandish_var(name: &str) -> f32 {
    let n = name.to_ascii_lowercase();
    if n.contains("brand") || n.contains("primary") {
        5.0
    } else if n.contains("accent") || n.contains("button") || n.contains("btn") || n.contains("cta") || n.contains("theme") || n.contains("highlight") {
        3.5
    } else if n.contains("secondary") || n.contains("link") {
        1.5
    } else {
        0.4
    }
}

/// Score the colors in a stylesheet (or `<style>` block) and add them to `out`.
pub fn scan_css(css: &str, out: &mut Vec<(Rgb, f32)>) {
    let mut budget = 6000usize;
    for c in CSS_VAR.captures_iter(css).take(1500) {
        if let Some(rgb) = parse_color(&c[2]) {
            out.push((rgb, brandish_var(&c[1])));
        }
    }
    for rule in CSS_RULE.captures_iter(css).take(4000) {
        let selector = rule[1].to_ascii_lowercase();
        let body = &rule[2];
        let buttonish = ["btn", "button", "cta", "shopify-payment", "wp-block-button", "sqs-block-button"].iter().any(|k| selector.contains(k));
        if buttonish {
            for c in CSS_BG.captures_iter(body) {
                if let Some(rgb) = parse_color(&c[1]) {
                    out.push((rgb, 3.0));
                }
            }
        }
        for c in CSS_ANY.captures_iter(body) {
            if budget == 0 {
                return;
            }
            budget -= 1;
            if let Some(rgb) = parse_color(&c[1]) {
                out.push((rgb, 0.25));
            }
        }
    }
}

/// Merge near-identical colors, then rank: real colors first, pale ones (bad behind white
/// button text) pushed down. Returns up to five hex colors.
pub fn rank_colors(weighted: &[(Rgb, f32)]) -> Vec<String> {
    let mut clusters: Vec<(Rgb, f32, f32)> = Vec::new(); // (representative, total weight, best single weight)
    for &(c, w) in weighted {
        if c.is_neutral() || w <= 0.0 {
            continue;
        }
        match clusters.iter_mut().find(|(r, _, _)| r.dist(c) < 30.0) {
            Some(cl) => {
                cl.1 += w;
                if w > cl.2 {
                    cl.0 = c;
                    cl.2 = w;
                }
            }
            None => clusters.push((c, w, w)),
        }
    }
    for cl in clusters.iter_mut() {
        let (_, l) = cl.0.sl();
        if l > 0.72 {
            cl.1 *= 0.35;
        }
        // Framework defaults (Bootstrap, Tailwind, WordPress presets, iOS blue) are in the CSS of
        // sites that never chose them, so they only win when nothing else is there.
        if FRAMEWORK_DEFAULTS.contains(&cl.0.hex().as_str()) {
            cl.1 *= 0.05;
        }
    }
    clusters.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut ranked: Vec<Rgb> = clusters.iter().take(4).map(|c| c.0).collect();
    // A strongly signalled dark neutral (a black logo, a near-black theme color) is offered too.
    let mut dark: Vec<&(Rgb, f32)> = weighted.iter().filter(|(c, w)| *w >= 4.0 && c.is_neutral() && c.sl().1 < 0.2).collect();
    dark.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((c, _)) = dark.first() {
        ranked.push(*c);
    }
    // The brand color fills buttons with white text, so the first color must be readable under it.
    if let Some(i) = ranked.iter().position(|c| contrast_with_white(*c) >= 3.0) {
        let c = ranked.remove(i);
        ranked.insert(0, c);
    }
    ranked.into_iter().take(5).map(Rgb::hex).collect()
}

/// WCAG contrast ratio between a color and white.
fn contrast_with_white(c: Rgb) -> f32 {
    let lin = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    let l = 0.2126 * lin(c.0) + 0.7152 * lin(c.1) + 0.0722 * lin(c.2);
    1.05 / (l + 0.05)
}

const FRAMEWORK_DEFAULTS: [&str; 40] = [
    // Bootstrap 4/5
    "#0d6efd", "#007bff", "#6610f2", "#6f42c1", "#d63384", "#e83e8c", "#dc3545", "#fd7e14", "#ffc107", "#198754", "#28a745", "#20c997", "#0dcaf0", "#17a2b8",
    // Tailwind's most common 500/600 shades
    "#3b82f6", "#2563eb", "#ef4444", "#dc2626", "#10b981", "#059669", "#6366f1", "#4f46e5", "#f59e0b", "#22c55e", "#8b5cf6",
    // WordPress block presets
    "#cf2e2e", "#ff6900", "#fcb900", "#7bdcb5", "#00d084", "#8ed1fc", "#0693e3", "#eb144c", "#f78da7", "#9b51e0",
    // iOS / browser link blues, Shopify checkout defaults
    "#007aff", "#0000ee", "#0000ff", "#1773b0", "#2c6ecb",
];

fn clean_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn host_of(base: &Url) -> String {
    base.host_str().unwrap_or_default().trim_start_matches("www.").to_ascii_lowercase()
}

/// "stayportland" for www.stayportland.com.
fn domain_core(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    let core = if parts.len() >= 3 && matches!(parts[parts.len() - 2], "co" | "com" | "org" | "net") && parts[parts.len() - 1].len() == 2 {
        parts[parts.len() - 3]
    } else if parts.len() >= 2 {
        parts[parts.len() - 2]
    } else {
        host
    };
    core.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

fn alnum_lower(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// Pick the brand out of a page title like "Home | Stay Portland" or "Stay Portland – Vacation Rentals".
fn name_from_title(title: &str, host: &str) -> Option<String> {
    let core = domain_core(host);
    let segs: Vec<String> = TITLE_SPLIT.split(title).map(clean_text).filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    if let Some(s) = segs.iter().find(|s| {
        let n = alnum_lower(s);
        !n.is_empty() && (n == core || (n.len() >= 4 && core.contains(&n)) || (core.len() >= 4 && n.contains(&core) && s.len() <= 40))
    }) {
        return Some(s.clone());
    }
    let generic = ["home", "homepage", "home page", "welcome", "official site", "official website", "index"];
    segs.into_iter().find(|s| !generic.contains(&s.to_lowercase().as_str()) && s.len() <= 60)
}

/// Absolute http(s) URL for an attribute value, or None for data: URIs, templates and junk.
fn absolutize(base: &Url, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with("data:") || raw.starts_with("javascript:") || raw.contains("{{") {
        return None;
    }
    let raw = raw.replace("{width}", "600");
    let u = base.join(&raw).ok()?;
    matches!(u.scheme(), "http" | "https").then(|| u.to_string())
}

/// The best source for an `<img>`: lazy-load attributes, then the widest `srcset` entry, then `src`.
fn img_src(el: &ElementRef, base: &Url) -> Option<String> {
    for attr in ["data-src", "data-lazy-src", "data-original", "data-image"] {
        if let Some(u) = el.attr(attr).and_then(|v| absolutize(base, v)) {
            return Some(u);
        }
    }
    for attr in ["srcset", "data-srcset"] {
        if let Some(set) = el.attr(attr) {
            let best = set
                .split(',')
                .filter_map(|part| {
                    let mut it = part.split_whitespace();
                    let url = it.next()?;
                    let w = it.next().and_then(|d| d.trim_end_matches(['w', 'x']).parse::<f32>().ok()).unwrap_or(1.0);
                    Some((url, w))
                })
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            if let Some(u) = best.and_then(|(u, _)| absolutize(base, u)) {
                return Some(u);
            }
        }
    }
    el.attr("src").and_then(|v| absolutize(base, v))
}

fn is_svg_url(u: &str) -> bool {
    let path = u.split(['?', '#']).next().unwrap_or(u).to_ascii_lowercase();
    path.ends_with(".svg") || path.ends_with(".svgz")
}

/// Visible text with line breaks at block boundaries (scripts, styles and SVG skipped).
fn visible_text(el: ElementRef, out: &mut String) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                let n = e.name();
                if matches!(n, "script" | "style" | "noscript" | "template" | "svg" | "head") {
                    continue;
                }
                let block = matches!(
                    n,
                    "br" | "p" | "div" | "li" | "ul" | "ol" | "address" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "section" | "footer" | "header" | "td" | "tr" | "table" | "article" | "nav"
                );
                if block {
                    out.push('\n');
                }
                if let Some(er) = ElementRef::wrap(child) {
                    visible_text(er, out);
                }
                if block {
                    out.push('\n');
                }
            }
            _ => {}
        }
    }
}

fn lines(text: &str) -> String {
    text.lines().map(clean_text).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

/// The first US street or PO Box address in some text, formatted "street, City, ST 12345".
pub fn find_address(text: &str) -> Option<String> {
    for c in STREET_ADDRESS.captures_iter(text) {
        if !US_STATES.contains(&&c[3]) {
            continue;
        }
        let street = clean_text(&c[1].replace('\n', " ")).trim_end_matches(',').to_string();
        return Some(format!("{street}, {}, {} {}", clean_text(&c[2]), &c[3], &c[4]));
    }
    None
}

/// Social profile links (not share buttons) keyed by network.
pub fn social_network(raw: &str) -> Option<(&'static str, String)> {
    let u = Url::parse(raw.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?.to_ascii_lowercase();
    let host = host.trim_start_matches("www.").trim_start_matches("m.").trim_start_matches("mobile.");
    let path = u.path().trim_end_matches('/').to_string();
    let first = path.trim_start_matches('/').split('/').next().unwrap_or("").to_ascii_lowercase();
    if first.is_empty() {
        return None;
    }
    let share = ["share", "sharer", "sharer.php", "intent", "dialog", "plugins", "tr", "home", "p", "reel", "explore", "watch", "embed", "pin", "shareArticle", "sharing", "search", "hashtag", "login", "privacy", "legal", "policies"];
    if share.iter().any(|s| first.eq_ignore_ascii_case(s)) {
        return None;
    }
    let net = match host {
        "instagram.com" => "instagram",
        "facebook.com" | "fb.com" | "fb.me" => "facebook",
        "tiktok.com" if first.starts_with('@') => "tiktok",
        "x.com" | "twitter.com" => "x",
        "youtube.com" if first.starts_with('@') || matches!(first.as_str(), "channel" | "c" | "user") => "youtube",
        "pinterest.com" => "pinterest",
        "linkedin.com" if matches!(first.as_str(), "company" | "in" | "school") => "linkedin",
        "threads.net" | "threads.com" if first.starts_with('@') => "threads",
        _ => return None,
    };
    Some((net, format!("https://{host}{path}")))
}

fn ld_objects(v: &Value, depth: usize, out: &mut Vec<serde_json::Map<String, Value>>) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Array(a) => a.iter().for_each(|x| ld_objects(x, depth + 1, out)),
        Value::Object(o) => {
            out.push(o.clone());
            for (k, x) in o {
                if k != "@context" && (x.is_object() || x.is_array()) {
                    ld_objects(x, depth + 1, out);
                }
            }
        }
        _ => {}
    }
}

fn ld_types(o: &serde_json::Map<String, Value>) -> Vec<String> {
    match o.get("@type") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    }
}

fn is_org(o: &serde_json::Map<String, Value>) -> bool {
    ld_types(o).iter().any(|t| ["Organization", "Business", "Store", "Corporation", "Hotel", "Restaurant", "Lodging", "Brand", "Shop", "Agent"].iter().any(|k| t.contains(k)))
}

fn ld_url(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("url").or_else(|| o.get("contentUrl")).and_then(|x| x.as_str()).map(String::from),
        Value::Array(a) => a.iter().find_map(ld_url),
        _ => None,
    }
}

fn ld_address(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if s.chars().any(|c| c.is_ascii_digit()) => Some(clean_text(s)),
        Value::Array(a) => a.iter().find_map(ld_address),
        Value::Object(o) => {
            let g = |k: &str| o.get(k).and_then(|x| x.as_str()).map(clean_text).filter(|s| !s.is_empty());
            let street = g("streetAddress")?;
            let locality = g("addressLocality");
            let region = g("addressRegion");
            let postal = g("postalCode");
            let country = match o.get("addressCountry") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Object(c)) => c.get("name").and_then(|x| x.as_str()).map(String::from),
                _ => None,
            }
            .filter(|c| !matches!(c.to_ascii_uppercase().as_str(), "US" | "USA" | "UNITED STATES" | "UNITED STATES OF AMERICA"));
            let region_postal = [region, postal].into_iter().flatten().collect::<Vec<_>>().join(" ");
            let parts: Vec<String> = [Some(street), locality, (!region_postal.is_empty()).then_some(region_postal), country].into_iter().flatten().collect();
            Some(parts.join(", "))
        }
        _ => None,
    }
}

const LOGO_BAD_CONTEXT: [&str; 14] = ["partner", "client", "sponsor", "press", "featured", "as-seen", "asseen", "payment", "badge", "award", "trust", "review", "testimonial", "certif"];

/// Read one HTML page. `base` is the page's final URL (for resolving relative links).
pub fn analyze(html: &str, base: &Url) -> PageFindings {
    let doc = Html::parse_document(html);
    let host = host_of(base);
    let mut f = PageFindings::default();
    let meta = |selector: &str| -> Option<String> {
        doc.select(&sel(selector)).find_map(|e| e.attr("content").map(clean_text).filter(|s| !s.is_empty()))
    };

    // Structured data (JSON-LD) is the most reliable source when a site has it.
    let mut ld = Vec::new();
    for s in doc.select(&sel(r#"script[type="application/ld+json"]"#)) {
        let raw: String = s.text().collect();
        if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
            ld_objects(&v, 0, &mut ld);
        }
    }
    let orgs: Vec<&serde_json::Map<String, Value>> = ld.iter().filter(|o| is_org(o)).collect();
    let ld_name = orgs.iter().find_map(|o| o.get("name").and_then(|n| n.as_str()).map(clean_text)).filter(|s| !s.is_empty() && s.len() <= 80);
    let ld_logo = orgs.iter().find_map(|o| o.get("logo").and_then(ld_url)).and_then(|u| absolutize(base, &u));
    let ld_addr = orgs.iter().find_map(|o| o.get("address").and_then(ld_address)).or_else(|| {
        ld.iter().find(|o| ld_types(o).iter().any(|t| t == "PostalAddress")).and_then(|o| ld_address(&Value::Object((*o).clone())))
    });

    // Name.
    let title: Option<String> = doc.select(&sel("title")).next().map(|t| clean_text(&t.text().collect::<String>()));
    f.name = meta(r#"meta[property="og:site_name"]"#)
        .filter(|s| s.len() <= 80)
        .or(ld_name)
        .or_else(|| meta(r#"meta[name="application-name"]"#).filter(|s| s.len() <= 60))
        .or_else(|| meta(r#"meta[name="apple-mobile-web-app-title"]"#).filter(|s| s.len() <= 60))
        .or_else(|| title.as_deref().and_then(|t| name_from_title(t, &host)));
    let name_norm = f.name.as_deref().map(alnum_lower).unwrap_or_default();

    // Logo: score every <img> by how logo-like it is and where it sits.
    let mut logos: Vec<(String, i32)> = Vec::new();
    for img in doc.select(&sel("img")) {
        let Some(src) = img_src(&img, base) else { continue };
        let own = format!(
            "{} {} {} {}",
            img.attr("class").unwrap_or(""),
            img.attr("id").unwrap_or(""),
            img.attr("alt").unwrap_or(""),
            src.rsplit('/').next().unwrap_or("")
        )
        .to_ascii_lowercase();
        let own_logo = own.contains("logo");
        let (mut anc_logo, mut in_header, mut home_link, mut in_footer, mut bad) = (false, false, false, false, LOGO_BAD_CONTEXT.iter().any(|b| own.contains(b)));
        for (depth, anc) in img.ancestors().filter_map(ElementRef::wrap).take(8).enumerate() {
            let e = anc.value();
            let ctx = format!("{} {}", e.attr("class").unwrap_or(""), e.attr("id").unwrap_or("")).to_ascii_lowercase();
            if depth < 4 && (ctx.contains("logo") || (ctx.contains("brand") && !ctx.contains("brands"))) {
                anc_logo = true;
            }
            if matches!(e.name(), "header" | "nav") || ctx.contains("header") || ctx.contains("navbar") || ctx.contains("masthead") {
                in_header = true;
            }
            if e.name() == "footer" || ctx.contains("footer") {
                in_footer = true;
            }
            if e.name() == "a" && depth < 3 {
                if let Some(href) = e.attr("href").and_then(|h| base.join(h).ok()) {
                    if href.host_str().map(|h| h.trim_start_matches("www.")) == Some(host.as_str()) && matches!(href.path(), "/" | "" | "/index.html") {
                        home_link = true;
                    }
                }
            }
            if LOGO_BAD_CONTEXT.iter().any(|b| ctx.contains(b)) || ctx.contains("brands") {
                bad = true;
            }
        }
        let alt_name = !name_norm.is_empty() && alnum_lower(img.attr("alt").unwrap_or("")).contains(&name_norm);
        // Being in the header isn't enough (seasonal banners live there too): it has to look like a logo.
        if bad || !(own_logo || anc_logo || alt_name) {
            continue;
        }
        let score = 4 * own_logo as i32 + 3 * anc_logo as i32 + 3 * in_header as i32 + 2 * home_link as i32 + 2 * alt_name as i32 - in_footer as i32;
        if score >= 5 {
            logos.push((src, score + 2));
        }
    }
    if let Some(u) = ld_logo {
        logos.push((u, 9));
    }
    if let Some(u) = meta(r#"meta[property="og:logo"]"#).and_then(|u| absolutize(base, &u)) {
        logos.push((u, 8));
    }
    if let Some(u) = meta(r#"meta[property="og:image"]"#).and_then(|u| absolutize(base, &u)) {
        if u.rsplit('/').next().unwrap_or("").to_ascii_lowercase().contains("logo") {
            logos.push((u, 5));
        }
    }
    // Square app icons are a decent fallback (and usually PNG, which every inbox shows).
    for link in doc.select(&sel("link[rel][href]")) {
        let rel = link.attr("rel").unwrap_or("").to_ascii_lowercase();
        let size = link.attr("sizes").and_then(|s| s.split('x').next()).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
        let Some(u) = link.attr("href").and_then(|h| absolutize(base, h)) else { continue };
        if rel.contains("apple-touch-icon") {
            logos.push((u, 3));
        } else if rel.split_whitespace().any(|r| r == "icon") && size >= 96 {
            logos.push((u, 2));
        } else if rel.contains("stylesheet") && f.stylesheets.len() < 6 {
            let skip = ["fonts.googleapis", "use.typekit", "fontawesome", "font-awesome", "bootstrap", "cdnjs.cloudflare", "jsdelivr", "unpkg.com", "googleapis.com/css"];
            if !skip.iter().any(|s| u.contains(s)) {
                f.stylesheets.push(u);
            }
        }
    }
    logos.sort_by_key(|a| (is_svg_url(&a.0), -a.1));
    for (u, _) in logos {
        if !f.logo_candidates.contains(&u) {
            f.logo_candidates.push(u);
        }
    }

    // Colors declared in the page itself.
    for (selector, weight) in [(r#"meta[name="theme-color"]"#, 10.0), (r#"meta[name="msapplication-TileColor"]"#, 6.0)] {
        for m in doc.select(&sel(selector)) {
            if let Some(c) = m.attr("content").and_then(parse_color) {
                f.colors.push((c, weight));
            }
        }
    }
    for m in doc.select(&sel(r#"link[rel="mask-icon"][color]"#)) {
        if let Some(c) = m.attr("color").and_then(parse_color) {
            f.colors.push((c, 6.0));
        }
    }
    for s in doc.select(&sel("style")) {
        scan_css(&s.text().collect::<String>(), &mut f.colors);
    }
    let mut inline_styles = String::new();
    for e in doc.select(&sel("[style]")).take(3000) {
        inline_styles.push_str("x{");
        inline_styles.push_str(e.attr("style").unwrap_or(""));
        inline_styles.push_str("}\n");
    }
    scan_css(&inline_styles, &mut f.colors);

    // Address: structured data, then the footer, then the whole page.
    let body_text = {
        let mut t = String::new();
        visible_text(doc.root_element(), &mut t);
        lines(&t)
    };
    let footer_text = doc
        .select(&sel(r#"footer, [class*="footer"], [id*="footer"], address"#))
        .map(|e| {
            let mut t = String::new();
            visible_text(e, &mut t);
            lines(&t)
        })
        .collect::<Vec<_>>()
        .join("\n");
    f.address = ld_addr.or_else(|| find_address(&footer_text)).or_else(|| find_address(&body_text));

    // Contact emails and social profiles.
    let mut emails = Vec::new();
    for a in doc.select(&sel(r#"a[href^="mailto:"], a[href^="MAILTO:"]"#)) {
        let raw = a.attr("href").unwrap_or("")[7..].split('?').next().unwrap_or("").to_string();
        if let Some(e) = normalize_email(&raw) {
            emails.push(e);
        }
    }
    for m in EMAIL_TEXT.find_iter(&body_text).take(50) {
        if let Some(e) = normalize_email(m.as_str()) {
            emails.push(e);
        }
    }
    for e in emails {
        if !f.emails.contains(&e) {
            f.emails.push(e);
        }
    }
    let mut socials: Vec<String> = doc.select(&sel("a[href]")).filter_map(|a| a.attr("href").map(String::from)).collect();
    for o in &orgs {
        if let Some(Value::Array(same)) = o.get("sameAs") {
            socials.extend(same.iter().filter_map(|x| x.as_str().map(String::from)));
        }
    }
    for s in socials {
        if let Some((net, url)) = social_network(&s) {
            f.social_links.entry(net.to_string()).or_insert(url);
        }
    }
    f
}

/// The best sender address: one listed on the site's own domain, else `hello@<domain>`.
fn pick_from_email(emails: &[String], host: &str) -> (Option<String>, bool) {
    let host = host.trim_start_matches("www.");
    let same_domain = |e: &&String| {
        let d = e.rsplit('@').next().unwrap_or("");
        d == host || host.ends_with(&format!(".{d}")) || d.ends_with(&format!(".{host}"))
    };
    let bad = ["noreply", "no-reply", "donotreply", "privacy", "abuse", "legal", "dmca", "unsubscribe", "postmaster", "example"];
    let usable: Vec<&String> = emails.iter().filter(same_domain).filter(|e| !bad.iter().any(|b| e.contains(b))).collect();
    let preferred = ["hello", "hi", "info", "contact", "team", "support", "stay", "book", "reservations", "shop", "store"];
    for p in preferred {
        if let Some(e) = usable.iter().find(|e| e.split('@').next() == Some(p)) {
            return (Some((*e).clone()), true);
        }
    }
    if let Some(e) = usable.first() {
        return (Some((*e).clone()), true);
    }
    if host.contains('.') {
        return (normalize_email(&format!("hello@{host}")), false);
    }
    (None, false)
}

/// Decode an image with hard limits, so a small file that claims to be gigapixels can't eat memory.
fn decode_limited(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    reader.limits(limits_for_decode());
    let (w, h) = reader.into_dimensions().ok()?;
    if w > 4096 || h > 4096 {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    reader.limits(limits_for_decode());
    reader.decode().ok()
}

fn limits_for_decode() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits
}

/// Check a logo is a real, reasonably sized image and find its main color. Decodes once.
fn inspect_logo(bytes: &[u8]) -> Option<Option<(Rgb, f32)>> {
    let img = decode_limited(bytes)?;
    if img.width() < 24 || img.height() < 16 {
        return None;
    }
    Some(logo_color(&img))
}

/// Main color of a logo image, if it has a clear one.
fn logo_color(img: &image::DynamicImage) -> Option<(Rgb, f32)> {
    let thumb = image::imageops::thumbnail(img, 64, 64);
    let mut buckets: std::collections::HashMap<(u8, u8, u8), (u32, u32, u32, u32)> = std::collections::HashMap::new();
    let mut opaque = 0u32;
    for p in thumb.pixels() {
        if p[3] < 200 {
            continue;
        }
        opaque += 1;
        let c = Rgb(p[0], p[1], p[2]);
        if c.is_neutral() {
            continue;
        }
        let e = buckets.entry((p[0] >> 4, p[1] >> 4, p[2] >> 4)).or_default();
        e.0 += p[0] as u32;
        e.1 += p[1] as u32;
        e.2 += p[2] as u32;
        e.3 += 1;
    }
    if let Some((_, (r, g, b, n))) = buckets.into_iter().max_by_key(|(_, v)| v.3) {
        if (n as f32) >= opaque as f32 * 0.04 {
            return Some((Rgb((r / n) as u8, (g / n) as u8, (b / n) as u8), 8.0));
        }
    }
    // A one-color dark logo (black wordmark): offer that dark color.
    let dark: Vec<_> = thumb.pixels().filter(|p| p[3] >= 200 && Rgb(p[0], p[1], p[2]).sl().1 < 0.3).collect();
    if opaque > 0 && dark.len() as f32 >= opaque as f32 * 0.3 {
        let n = dark.len() as u32;
        let avg = |i: usize| (dark.iter().map(|p| p[i] as u32).sum::<u32>() / n) as u8;
        return Some((Rgb(avg(0), avg(1), avg(2)), 4.0));
    }
    None
}


/// "acme.com", "https://acme.com/shop" → a full https URL, or a friendly error.
pub fn normalize_site_url(raw: &str) -> AppResult<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AppError::bad("enter your website, like acme.com"));
    }
    let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") { raw.to_string() } else { format!("https://{raw}") };
    let u = Url::parse(&with_scheme).map_err(|_| AppError::bad("that doesn't look like a website address"))?;
    let host = u.host_str().unwrap_or("");
    if !host.contains('.') || host.ends_with('.') {
        return Err(AppError::bad("that doesn't look like a website address"));
    }
    Ok(u.to_string())
}

/// Fetch a site and build its brand kit. Takes at most ~30 seconds.
pub async fn import(raw: &str) -> AppResult<BrandKit> {
    let url = normalize_site_url(raw)?;
    match tokio::time::timeout(std::time::Duration::from_secs(30), import_inner(&url)).await {
        Ok(r) => r,
        Err(_) => Err(AppError::bad("that site took too long to answer; try again or fill in your brand by hand")),
    }
}

async fn import_inner(url: &str) -> AppResult<BrandKit> {
    let page = match fetch_public(url, 3 * 1024 * 1024).await {
        Ok(p) => p,
        // Plenty of small sites only answer on http or without "www."; try once more.
        Err(e) if url.starts_with("https://") => fetch_public(&url.replacen("https://", "http://", 1), 3 * 1024 * 1024).await.map_err(|_| e)?,
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&page.body).into_owned();
    if !page.content_type.contains("html") && !text.trim_start().starts_with('<') {
        return Err(AppError::bad("that address isn't a web page"));
    }
    let base = Url::parse(&page.url).map_err(|_| AppError::bad("that site redirected somewhere invalid"))?;
    let mut f = analyze(&text, &base);

    // Stylesheets and the logo are fetched in parallel.
    let mut sheets = tokio::task::JoinSet::new();
    for s in f.stylesheets.iter().take(4).cloned().collect::<Vec<String>>() {
        sheets.spawn(async move { fetch_public(&s, 1536 * 1024).await.ok().map(|p| String::from_utf8_lossy(&p.body).into_owned()) });
    }
    let candidates = f.logo_candidates.clone();
    let logo = async move {
        for u in candidates.into_iter().take(4) {
            let Ok(img) = fetch_public(&u, 2 * 1024 * 1024).await else { continue };
            if img.content_type.contains("svg") || is_svg_url(&img.url) {
                return Some((u, None));
            }
            if img.content_type.starts_with("text/") {
                continue;
            }
            // Decoding is CPU work; keep it off the async workers.
            let body = img.body;
            let Ok(Some(color)) = tokio::task::spawn_blocking(move || inspect_logo(&body)).await else { continue };
            return Some((u, color));
        }
        None
    };
    let (logo, css) = tokio::join!(logo, async {
        let mut out = Vec::new();
        while let Some(r) = sheets.join_next().await {
            if let Ok(Some(css)) = r {
                out.push(css);
            }
        }
        out
    });
    for css in css {
        scan_css(&css, &mut f.colors);
    }
    let mut kit = BrandKit { website: format!("{}://{}/", base.scheme(), base.host_str().unwrap_or_default()), ..Default::default() };
    if let Some((u, color)) = logo {
        // Inboxes warn about or block insecure images; every real host serves its logo over https too.
        kit.logo_url = Some(if base.scheme() == "https" { u.replacen("http://", "https://", 1) } else { u });
        if let Some(c) = color {
            f.colors.push(c);
        }
    }
    kit.colors = rank_colors(&f.colors);
    kit.brand_color = kit.colors.first().cloned();
    kit.name = f.name;
    kit.address = f.address;
    let (from, found) = pick_from_email(&f.emails, &host_of(&base));
    kit.from_email = from;
    kit.from_email_found = found;
    kit.social_links = f.social_links;
    Ok(kit)
}

/// Validate a `social_links` patch: known networks only, https profile URLs, blank removes.
pub fn clean_social_links(current: &Value, patch: &serde_json::Map<String, Value>) -> AppResult<Value> {
    let mut out = current.as_object().cloned().unwrap_or_default();
    for (k, v) in patch {
        let net = k.to_ascii_lowercase();
        if !SOCIAL_NETWORKS.contains(&net.as_str()) {
            return Err(AppError::bad(format!("unknown social network {k:?} (use {})", SOCIAL_NETWORKS.join(", "))));
        }
        let s = v.as_str().unwrap_or("").trim();
        if s.is_empty() {
            out.remove(&net);
            continue;
        }
        let full = if s.starts_with("http") { s.to_string() } else { format!("https://{s}") };
        let u = Url::parse(&full).map_err(|_| AppError::bad(format!("{net} link is not a valid URL")))?;
        if u.scheme() != "https" || u.host_str().is_none() || full.len() > 300 {
            return Err(AppError::bad(format!("{net} link must be an https:// address")));
        }
        out.insert(net, Value::String(u.to_string()));
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"<!doctype html><html><head>
        <title>Home | Juniper &amp; Oak</title>
        <meta name="theme-color" content="#ffffff">
        <meta property="og:image" content="/hero.jpg">
        <link rel="apple-touch-icon" href="/apple-touch-icon.png">
        <link rel="stylesheet" href="/assets/site.css">
        <link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter">
        <style>:root{--color-primary:#c2410c;--text:#111827;--bg:#fafafa}.btn-primary{background-color:#c2410c;color:#fff}</style>
      </head><body>
        <header class="site-header"><a href="/" class="site-logo"><img src="/img/logo.svg" alt="Juniper & Oak"></a>
          <a href="/"><img class="header__logo" srcset="//cdn.example.com/logo-200.png 200w, //cdn.example.com/logo-400.png 400w" src="//cdn.example.com/logo-200.png" alt="Juniper and Oak logo"></a></header>
        <main><section class="partners"><img src="/press/vogue-logo.png" alt="Vogue logo"></section>
          <p>Write to us at <a href="mailto:Hello@JuniperOak.com?subject=hi">hello</a> or press@other.com</p></main>
        <footer>Juniper &amp; Oak<br>1234 SE Division St, Suite 5<br>Portland, OR 97202
          <a href="https://www.instagram.com/juniperoak/">IG</a>
          <a href="https://www.facebook.com/sharer/sharer.php?u=x">share</a>
          <a href="https://facebook.com/juniperoak">FB</a>
          <a href="https://www.tiktok.com/@juniperoak?lang=en">TT</a></footer>
      </body></html>"##;

    #[test]
    fn reads_a_typical_store_page() {
        let base = Url::parse("https://www.juniperoak.com/").unwrap();
        let f = analyze(PAGE, &base);
        assert_eq!(f.name.as_deref(), Some("Juniper & Oak"));
        // Raster header logo (largest srcset entry) beats the SVG and the app icon; press logos are skipped.
        assert_eq!(f.logo_candidates[0], "https://cdn.example.com/logo-400.png");
        assert!(f.logo_candidates.iter().position(|u| u.ends_with("apple-touch-icon.png")) < f.logo_candidates.iter().position(|u| u.ends_with("logo.svg")));
        assert!(!f.logo_candidates.iter().any(|u| u.contains("vogue")));
        assert_eq!(f.stylesheets, vec!["https://www.juniperoak.com/assets/site.css".to_string()]);
        assert_eq!(rank_colors(&f.colors).first().map(String::as_str), Some("#c2410c"));
        assert_eq!(f.address.as_deref(), Some("1234 SE Division St, Suite 5, Portland, OR 97202"));
        assert_eq!(pick_from_email(&f.emails, "juniperoak.com"), (Some("hello@juniperoak.com".into()), true));
        assert_eq!(f.social_links.get("instagram").map(String::as_str), Some("https://instagram.com/juniperoak"));
        assert_eq!(f.social_links.get("facebook").map(String::as_str), Some("https://facebook.com/juniperoak"));
        assert_eq!(f.social_links.get("tiktok").map(String::as_str), Some("https://tiktok.com/@juniperoak"));
    }

    #[test]
    fn structured_data_wins() {
        let html = r#"<html><head><title>Welcome</title><script type="application/ld+json">
            {"@context":"https://schema.org","@graph":[{"@type":["LodgingBusiness"],"name":"Stay Portland",
             "logo":{"@type":"ImageObject","url":"/brand/logo.png"},
             "address":{"@type":"PostalAddress","streetAddress":"500 NW 23rd Ave","addressLocality":"Portland","addressRegion":"OR","postalCode":"97210","addressCountry":"US"},
             "sameAs":["https://www.linkedin.com/company/stay-portland","https://x.com/intent/tweet"]}]}
            </script></head><body><p>Contact: reservations@stayportland.com</p></body></html>"#;
        let base = Url::parse("https://stayportland.com/").unwrap();
        let f = analyze(html, &base);
        assert_eq!(f.name.as_deref(), Some("Stay Portland"));
        assert_eq!(f.logo_candidates.first().map(String::as_str), Some("https://stayportland.com/brand/logo.png"));
        assert_eq!(f.address.as_deref(), Some("500 NW 23rd Ave, Portland, OR 97210"));
        assert_eq!(f.social_links.len(), 1);
        assert_eq!(pick_from_email(&f.emails, "stayportland.com"), (Some("reservations@stayportland.com".into()), true));
    }

    #[test]
    fn titles_and_fallbacks() {
        assert_eq!(name_from_title("Stay Portland - Vacation Rentals in Portland, OR", "stayportland.com").as_deref(), Some("Stay Portland"));
        assert_eq!(name_from_title("Home – Acme Goods", "shop.example.com").as_deref(), Some("Acme Goods"));
        assert_eq!(pick_from_email(&["owner@gmail.com".into()], "acme.com"), (Some("hello@acme.com".into()), false));
        assert_eq!(find_address("Visit 99 Main Street\nSpringfield, ZZ 12345"), None);
        assert_eq!(find_address("PO Box 42, Bend, OR 97701").as_deref(), Some("PO Box 42, Bend, OR 97701"));
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#ABC"), Some(Rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_color("rgb(31 111 92 / 0.9)"), Some(Rgb(31, 111, 92)));
        assert_eq!(parse_color("rgba(0,0,0,.1)"), None);
        assert_eq!(parse_color("#12345680"), Some(Rgb(0x12, 0x34, 0x56)));
        // Pale colors lose to strong ones even with more votes; a black-and-white site falls back to its dark color.
        assert_eq!(rank_colors(&[(Rgb(0xfd, 0xe6, 0x8a), 9.0), (Rgb(0x1d, 0x4e, 0xd8), 5.0)])[0], "#1d4ed8");
        assert_eq!(rank_colors(&[(Rgb(0x11, 0x11, 0x11), 10.0), (Rgb(0xff, 0xff, 0xff), 10.0)]), vec!["#111111".to_string()]);
        // Tailwind's blue-gray text color is not a brand color.
        assert!(rank_colors(&[(Rgb(0x36, 0x41, 0x53), 3.0)]).is_empty());
    }

    #[test]
    fn huge_images_are_refused_without_decoding() {
        // A valid PNG header claiming 100,000 x 100,000 pixels.
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR");
        png.extend_from_slice(&100_000u32.to_be_bytes());
        png.extend_from_slice(&100_000u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        assert!(decode_limited(&png).is_none());
        let mut ok = Vec::new();
        image::RgbaImage::from_pixel(40, 20, image::Rgba([200, 30, 30, 255])).write_to(&mut std::io::Cursor::new(&mut ok), image::ImageFormat::Png).unwrap();
        let (c, _) = inspect_logo(&ok).unwrap().unwrap();
        assert_eq!(c, Rgb(200, 30, 30));
    }

    #[test]
    fn social_links_patch() {
        let cur = serde_json::json!({"instagram": "https://instagram.com/a"});
        let mut p = serde_json::Map::new();
        p.insert("instagram".into(), Value::String("".into()));
        p.insert("TikTok".into(), Value::String("tiktok.com/@a".into()));
        assert_eq!(clean_social_links(&cur, &p).unwrap(), serde_json::json!({"tiktok": "https://tiktok.com/@a"}));
        let mut bad = serde_json::Map::new();
        bad.insert("myspace".into(), Value::String("https://myspace.com/a".into()));
        assert!(clean_social_links(&cur, &bad).is_err());
    }
}
