//! Email and SMS rendering: blocks → responsive table-based HTML, Jinja personalization,
//! click tracking, open pixel and the CAN-SPAM footer.

use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::{Account, Profile};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EmailContent {
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub preheader: Option<String>,
    #[serde(default)]
    pub from_name: Option<String>,
    #[serde(default)]
    pub from_email: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub blocks: Option<Vec<Value>>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub theme: Option<Theme>,
    /// The language version being sent, set by the content prepare step (see
    /// `features::translations`); None renders exactly as before.
    #[serde(default, rename = "_lang")]
    pub lang: Option<String>,
    /// The footer lines in that language, set with `lang`.
    #[serde(default, rename = "_footer")]
    pub footer_texts: Option<FooterTexts>,
}

/// The words of the marketing footer in the email's language.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FooterTexts {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub unsubscribe: String,
}

/// `lang` (and `dir="rtl"`) attributes for a language version: only well-formed tags.
fn lang_attrs(lang: Option<&str>) -> Option<(String, bool)> {
    let l = crate::features::translations::locale::normalize(lang?)?;
    let rtl = crate::features::translations::locale::is_rtl(&l);
    Some((l, rtl))
}

/// Right-to-left email: the blocks' left and right alignment swap sides.
fn mirror(html: &str) -> String {
    html.replace("text-align:left;", "text-align:\u{1}l;").replace("text-align:right;", "text-align:left;").replace("text-align:\u{1}l;", "text-align:right;")
        .replace("align=\"left\"", "align=\"\u{1}l\"").replace("align=\"right\"", "align=\"left\"").replace("align=\"\u{1}l\"", "align=\"right\"")
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Theme {
    pub brand_color: Option<String>,
    pub background: Option<String>,
    pub font: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SmsContent {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub media_url: Option<String>,
}

pub struct RenderedEmail {
    pub subject: String,
    pub preheader: String,
    pub html: String,
    pub text: String,
}

/// Tracking options; None renders a clean preview.
pub struct Tracking<'a> {
    pub public_url: &'a str,
    pub secret: &'a str,
    pub message_id: Uuid,
    pub unsubscribe_url: String,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn s<'a>(b: &'a Value, k: &str) -> &'a str {
    b.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn align(b: &Value) -> &str {
    match s(b, "align") {
        "center" => "center",
        "right" => "right",
        _ => "left",
    }
}

/// Only hex colors are allowed anywhere a color lands in HTML or CSS.
pub fn safe_color(c: &str, fallback: &str) -> String {
    static HEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"^#[0-9a-fA-F]{3,8}$").unwrap());
    if HEX.is_match(c) { c.to_string() } else { fallback.to_string() }
}

fn block_html(b: &Value, brand: &str, font: &str, depth: usize) -> String {
    let pad = "padding:8px 24px;";
    match s(b, "type") {
        "heading" => {
            let level = b.get("level").and_then(|v| v.as_u64()).unwrap_or(1).clamp(1, 3);
            let size = [0, 28, 22, 18][level as usize];
            format!(
                r#"<tr><td style="{pad}text-align:{a};"><h{level} style="margin:0;font-family:{font};font-size:{size}px;line-height:1.25;color:#111827;font-weight:700;">{t}</h{level}></td></tr>"#,
                a = align(b),
                t = esc(s(b, "text"))
            )
        }
        "text" => format!(
            r#"<tr><td style="{pad}text-align:{a};font-family:{font};font-size:16px;line-height:1.6;color:#374151;">{h}</td></tr>"#,
            a = align(b),
            h = s(b, "html")
        ),
        "image" => {
            let w = b.get("width").and_then(|v| v.as_u64()).unwrap_or(552).min(552);
            let img = format!(
                r#"<img src="{src}" alt="{alt}" width="{w}" style="display:block;width:100%;max-width:{w}px;height:auto;border:0;border-radius:{r}px;">"#,
                src = esc(s(b, "src")),
                alt = esc(s(b, "alt")),
                r = b.get("radius").and_then(|v| v.as_u64()).unwrap_or(6).min(40)
            );
            let inner = if s(b, "href").is_empty() { img } else { format!(r#"<a href="{}">{img}</a>"#, esc(s(b, "href"))) };
            format!(r#"<tr><td style="{pad}" align="center">{inner}</td></tr>"#)
        }
        "button" => {
            let color = safe_color(s(b, "color"), brand);
            format!(
                r#"<tr><td style="padding:16px 24px;" align="{a}"><table role="presentation" cellpadding="0" cellspacing="0"><tr><td style="border-radius:6px;background:{color};"><a href="{href}" style="display:inline-block;padding:14px 28px;font-family:{font};font-size:16px;font-weight:600;color:#ffffff;text-decoration:none;border-radius:6px;">{t}</a></td></tr></table></td></tr>"#,
                a = align(b),
                href = esc(s(b, "href")),
                t = esc(s(b, "text"))
            )
        }
        "divider" => r#"<tr><td style="padding:16px 24px;"><div style="border-top:1px solid #e5e7eb;font-size:0;line-height:0;">&nbsp;</div></td></tr>"#.to_string(),
        "spacer" => {
            let h = b.get("height").and_then(|v| v.as_u64()).unwrap_or(24).min(200);
            format!(r#"<tr><td style="height:{h}px;font-size:0;line-height:0;">&nbsp;</td></tr>"#)
        }
        "columns" if depth == 0 => {
            let cols: Vec<Vec<Value>> = b
                .get("columns")
                .and_then(|c| serde_json::from_value(c.clone()).ok())
                .unwrap_or_default();
            if cols.is_empty() {
                return String::new();
            }
            let width = 100 / cols.len();
            let tds: String = cols
                .iter()
                .map(|col| {
                    let inner: String = col.iter().map(|x| block_html(x, brand, font, depth + 1)).collect();
                    format!(r#"<td class="pl-col" valign="top" width="{width}%" style="width:{width}%;"><table role="presentation" width="100%" cellpadding="0" cellspacing="0">{inner}</table></td>"#)
                })
                .collect();
            format!(r#"<tr><td><table role="presentation" width="100%" cellpadding="0" cellspacing="0"><tr>{tds}</tr></table></td></tr>"#)
        }
        "product" => {
            let img = if s(b, "image").is_empty() {
                String::new()
            } else {
                format!(r#"<img src="{}" alt="{}" width="260" style="display:block;width:100%;max-width:260px;height:auto;border-radius:6px;margin:0 auto 12px;">"#, esc(s(b, "image")), esc(s(b, "title")))
            };
            let btn = if s(b, "button_text").is_empty() { "Shop now" } else { s(b, "button_text") };
            format!(
                r#"<tr><td style="{pad}" align="center">{img}<div style="font-family:{font};font-size:18px;font-weight:600;color:#111827;">{t}</div><div style="font-family:{font};font-size:16px;color:#4b5563;margin:4px 0 12px;">{p}</div><a href="{href}" style="display:inline-block;padding:10px 22px;background:{brand};color:#fff;border-radius:6px;font-family:{font};font-weight:600;text-decoration:none;">{btn}</a></td></tr>"#,
                t = esc(s(b, "title")),
                p = esc(s(b, "price")),
                href = esc(s(b, "href")),
                btn = esc(btn)
            )
        }
        "products" if depth == 0 => products_html(b, brand, font),
        "coupon" => crate::features::coupons::template::block_html(b, brand, font),
        "html" => format!(r#"<tr><td style="{pad}">{}</td></tr>"#, s(b, "html")),
        // A saved block that wasn't filled in before rendering shows the copy stored with it.
        "saved" => b.get("blocks").and_then(|v| v.as_array()).map(|bs| bs.iter().filter(|x| s(x, "type") != "saved").map(|x| block_html(x, brand, font, depth)).collect()).unwrap_or_default(),
        "social" | "video" | "countdown" | "table" | "quote" | "header" | "nav" | "footer" | "image_text" => crate::features::content::blocks::html(b, brand, font),
        _ => String::new(),
    }
}

/// A web link for a product, or "#". Braces are percent-encoded so a link can't open a template tag.
fn product_href(v: &Value) -> String {
    match v.as_str().filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
        Some(u) => esc(&u.replace('{', "%7B").replace('}', "%7D")),
        None => "#".into(),
    }
}

/// Product text from the catalog: escaped, and unable to open a template tag.
fn product_text(v: &Value) -> String {
    no_tpl(&esc(v.as_str().unwrap_or("")))
}

/// A grid of products (1-4 across, stacking on phones), filled per recipient into `items` by the
/// catalog before rendering. No items renders nothing.
fn products_html(b: &Value, brand: &str, font: &str) -> String {
    let items: Vec<&Value> = b.get("items").and_then(|i| i.as_array()).map(|a| a.iter().take(12).collect()).unwrap_or_default();
    if items.is_empty() {
        return String::new();
    }
    let cols = b.get("columns").and_then(|c| c.as_u64()).unwrap_or(2).clamp(1, 4) as usize;
    let flag = |k: &str| b.get(k).and_then(|v| v.as_bool()).unwrap_or(true);
    let (show_price, show_compare, show_button) = (flag("show_price"), flag("show_compare_at"), flag("show_button"));
    let btn = no_tpl(&esc(Some(s(b, "button_text")).filter(|t| !t.is_empty()).unwrap_or("Shop now")));
    let width = 100 / cols;
    let img_w = [0, 552, 260, 168, 122][cols];
    let cell = |it: &Value| {
        let href = product_href(&it["url"]);
        let title = product_text(&it["title"]);
        // Without a web link the product shows unlinked and without a button.
        let link = |inner: String| if href == "#" { inner } else { format!(r#"<a href="{href}" style="text-decoration:none;color:#111827;">{inner}</a>"#) };
        let img = match it["image_url"].as_str().filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
            Some(u) => link(format!(
                r#"<img src="{}" alt="{title}" width="{img_w}" style="display:block;width:100%;max-width:{img_w}px;height:auto;border:0;border-radius:6px;margin:0 auto 10px;">"#,
                esc(&u.replace('{', "%7B").replace('}', "%7D"))
            )),
            None => String::new(),
        };
        let mut price = String::new();
        if show_price {
            if let Some(p) = it["price_text"].as_str() {
                let was = match it["compare_at_text"].as_str() {
                    Some(c) if show_compare => format!(r#" <span style="color:#9ca3af;text-decoration:line-through;">{}</span>"#, product_text(&json!(c))),
                    _ => String::new(),
                };
                price = format!(r#"<div style="font-family:{font};font-size:15px;color:#4b5563;margin:2px 0 10px;">{}{was}</div>"#, product_text(&json!(p)));
            }
        }
        let button = if show_button && href != "#" {
            format!(r#"<a href="{href}" style="display:inline-block;padding:9px 18px;background:{brand};color:#fff;border-radius:6px;font-family:{font};font-size:14px;font-weight:600;text-decoration:none;">{btn}</a>"#)
        } else {
            String::new()
        };
        format!(
            r#"<td class="pl-col" valign="top" width="{width}%" style="width:{width}%;padding:8px;text-align:center;">{img}<div style="font-family:{font};font-size:16px;font-weight:600;color:#111827;">{}</div>{price}{button}</td>"#,
            link(title.clone())
        )
    };
    let rows: String = items
        .chunks(cols)
        .map(|row| {
            let mut tds: String = row.iter().map(|it| cell(it)).collect();
            for _ in row.len()..cols {
                tds.push_str(&format!(r#"<td class="pl-col" width="{width}%" style="width:{width}%;"></td>"#));
            }
            format!("<tr>{tds}</tr>")
        })
        .collect();
    format!(r#"<tr><td style="padding:8px 16px;"><table role="presentation" width="100%" cellpadding="0" cellspacing="0">{rows}</table></td></tr>"#)
}

fn block_text(b: &Value, out: &mut String) {
    static TAGS: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());
    static BR: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)<br\s*/?>|</p>|</li>").unwrap());
    match s(b, "type") {
        "heading" => out.push_str(&format!("{}\n\n", s(b, "text"))),
        "text" | "html" => {
            let t = BR.replace_all(s(b, "html"), "\n");
            let t = TAGS.replace_all(&t, "");
            let t = t.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">");
            out.push_str(t.trim());
            out.push_str("\n\n");
        }
        "button" => out.push_str(&format!("{}: {}\n\n", s(b, "text"), s(b, "href"))),
        "product" => out.push_str(&format!("{} {} {}\n\n", s(b, "title"), s(b, "price"), s(b, "href"))),
        "coupon" => crate::features::coupons::template::block_text(b, out),
        "products" => {
            // Catalog text can't open a template tag in the plain-text part either.
            // A zero-width space after every brace: no sequence of them can open a tag.
            let inert = |v: &Value| v.as_str().unwrap_or("").replace('{', "{\u{200B}");
            for it in b.get("items").and_then(|i| i.as_array()).into_iter().flatten().take(12) {
                let line = [inert(&it["title"]), inert(&it["price_text"]), inert(&it["url"])].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" ");
                out.push_str(&line);
                out.push('\n');
            }
            out.push('\n');
        }
        "image" if !s(b, "href").is_empty() => out.push_str(&format!("{}\n\n", s(b, "href"))),
        "divider" => out.push_str("----\n\n"),
        "columns" => {
            if let Some(cols) = b.get("columns").and_then(|c| c.as_array()) {
                for col in cols {
                    for x in col.as_array().into_iter().flatten() {
                        block_text(x, out);
                    }
                }
            }
        }
        "saved" => {
            for x in b.get("blocks").and_then(|v| v.as_array()).into_iter().flatten().filter(|x| s(x, "type") != "saved") {
                block_text(x, out);
            }
        }
        "social" | "video" | "countdown" | "table" | "quote" | "header" | "nav" | "footer" | "image_text" => crate::features::content::blocks::text(b, out),
        _ => {}
    }
}

fn no_tpl(s: &str) -> String {
    s.replace('{', "&#123;").replace('}', "&#125;")
}

/// The account's social profile links as (label, https URL), in a fixed order.
fn social_links(account: &Account) -> Vec<(&'static str, String)> {
    let labels = [("instagram", "Instagram"), ("facebook", "Facebook"), ("tiktok", "TikTok"), ("x", "X"), ("youtube", "YouTube"), ("pinterest", "Pinterest"), ("linkedin", "LinkedIn"), ("threads", "Threads")];
    labels
        .iter()
        .filter_map(|(k, label)| {
            let u = account.social_links.get(*k)?.as_str()?;
            u.starts_with("https://").then(|| (*label, u.to_string()))
        })
        .collect()
}

fn layout(inner: &str, preheader: &str, account: &Account, theme: &Theme, footer: &str, lang: Option<&(String, bool)>) -> String {
    let doc = layout_en(inner, preheader, account, theme, footer);
    match lang {
        None => doc,
        Some((l, rtl)) => {
            let dir = if *rtl { r#" dir="rtl""# } else { "" };
            doc.replacen(r#"<html lang="en">"#, &format!(r#"<html lang="{l}"{dir}>"#), 1).replacen("<body style=", &format!("<body{dir} style="), 1)
        }
    }
}

fn layout_en(inner: &str, preheader: &str, account: &Account, theme: &Theme, footer: &str) -> String {
    let bg = safe_color(theme.background.as_deref().unwrap_or(""), "#f4f4f5");
    let font = theme.font.clone().unwrap_or_else(|| "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif".into());
    let font = font.replace('"', "'");
    let logo = account
        .logo_url
        .as_deref()
        .filter(|u| !u.is_empty())
        .map(|u| format!(r#"<tr><td align="center" style="padding:24px 24px 8px;"><img src="{}" alt="{}" height="40" style="height:40px;width:auto;border:0;"></td></tr>"#, esc(u), esc(&account.name)))
        .unwrap_or_default();
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light"><title></title><style>@media (max-width:620px){{.pl-card{{width:100%!important;border-radius:0!important}}.pl-col{{display:block!important;width:100%!important}}.pl-foot{{width:100%!important}}}}a{{color:inherit}}</style></head><body style="margin:0;padding:0;background:{bg};"><div style="display:none;max-height:0;overflow:hidden;opacity:0;">{pre}&#8199;&#847;&#8199;&#847;&#8199;&#847;</div><table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:{bg};"><tr><td align="center" style="padding:24px 8px;"><table role="presentation" class="pl-card" width="600" cellpadding="0" cellspacing="0" style="width:600px;max-width:600px;background:#ffffff;border-radius:10px;overflow:hidden;">{logo}<tr><td style="height:8px;"></td></tr>{inner}<tr><td style="height:16px;"></td></tr></table><table role="presentation" class="pl-foot" width="600" cellpadding="0" cellspacing="0" style="width:600px;max-width:600px;"><tr><td style="padding:16px 24px;text-align:center;font-family:{font};font-size:12px;line-height:1.5;color:#6b7280;">{footer}</td></tr></table></td></tr></table></body></html>"#,
        pre = esc(preheader),
    )
}

fn env(auto_escape: bool) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Lenient);
    if auto_escape {
        env.set_auto_escape_callback(|_| AutoEscape::Html);
    } else {
        env.set_auto_escape_callback(|_| AutoEscape::None);
    }
    env
}

pub fn render_str(source: &str, ctx: &Value, html: bool) -> AppResult<String> {
    if !source.contains("{{") && !source.contains("{%") {
        return Ok(source.to_string());
    }
    // `{% coupon_code 'NAME' %}` becomes the code prepared for this message (see features::coupons).
    let source = crate::features::coupons::template::resolve(source, ctx)?;
    env(html)
        .render_str(&source, ctx)
        .map_err(|e| AppError::bad(format!("template error: {e}")))
}

/// Personalization variables available to every message.
pub fn context(account: &Account, profile: Option<&Profile>, event: Option<&Value>, unsubscribe_url: &str) -> Value {
    let mut root = Map::new();
    if let Some(p) = profile {
        if let Some(props) = p.properties.as_object() {
            for (k, v) in props {
                root.insert(k.clone(), v.clone());
            }
        }
        // Leave empty fields out entirely so `{{ first_name|default('there') }}` falls back.
        let mut person = Map::new();
        person.insert("id".into(), json!(p.id));
        person.insert("properties".into(), p.properties.clone());
        if let Some(l) = p.locale.as_deref().filter(|l| !l.is_empty()) {
            person.insert("locale".into(), json!(l));
        }
        for (k, v) in [("first_name", &p.first_name), ("last_name", &p.last_name), ("email", &p.email), ("phone_number", &p.phone_number), ("external_id", &p.external_id)] {
            match v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(val) => {
                    person.insert(k.into(), json!(val));
                    if k != "external_id" {
                        root.insert(k.into(), json!(val));
                    }
                }
                None => {
                    root.remove(k);
                }
            }
        }
        root.insert("person".into(), Value::Object(person));
    } else {
        root.insert("person".into(), json!({"properties": {}}));
    }
    root.insert("event".into(), event.cloned().unwrap_or(json!({})));
    root.insert(
        "organization".into(),
        json!({"name": account.name, "address": account.address, "website": account.website}),
    );
    root.insert("unsubscribe_url".into(), json!(unsubscribe_url));
    Value::Object(root)
}

/// What an email's footer carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Footer {
    /// Brand, postal address and the unsubscribe link (every marketing email).
    Marketing,
    /// Brand and postal address only (transactional mail that wants it).
    Address,
    /// Nothing (transactional mail by default).
    None,
}

/// How an email is rendered. Marketing email always uses the defaults.
#[derive(Debug, Clone, Copy)]
pub struct EmailOptions {
    pub footer: Footer,
    pub track_clicks: bool,
    pub track_opens: bool,
}

impl Default for EmailOptions {
    fn default() -> Self {
        Self { footer: Footer::Marketing, track_clicks: true, track_opens: true }
    }
}

pub fn render_email(
    account: &Account,
    content: &EmailContent,
    ctx: &Value,
    tracking: Option<&Tracking<'_>>,
) -> AppResult<RenderedEmail> {
    render_email_with(account, content, ctx, tracking, EmailOptions::default())
}

/// The template text an email renders: subject, preheader, then the body as HTML and as text
/// (without the layout and footer, which never carry template tags). Features that must know what a
/// message will show (coupon codes) read this, so they see what [`render_email_with`] renders.
pub fn email_template_parts(content: &EmailContent) -> [String; 4] {
    let (html, text) = match (&content.blocks, &content.html) {
        (Some(blocks), _) if !blocks.is_empty() => {
            let mut text = String::new();
            blocks.iter().for_each(|b| block_text(b, &mut text));
            (blocks.iter().map(|b| block_html(b, "#000000", "", 0)).collect(), text)
        }
        (_, Some(html)) if !html.trim().is_empty() => {
            let mut text = String::new();
            block_text(&json!({"type": "html", "html": html}), &mut text);
            (html.clone(), text)
        }
        _ => (String::new(), String::new()),
    };
    [content.subject.clone(), content.preheader.clone().unwrap_or_default(), html, text]
}

pub fn render_email_with(
    account: &Account,
    content: &EmailContent,
    ctx: &Value,
    tracking: Option<&Tracking<'_>>,
    opts: EmailOptions,
) -> AppResult<RenderedEmail> {
    let theme = content.theme.clone().unwrap_or_default();
    let brand = safe_color(theme.brand_color.as_deref().unwrap_or(&account.brand_color), "#1f6f5c");
    let font = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
    let unsub = tracking.map(|t| t.unsubscribe_url.clone()).unwrap_or_else(|| "#".into());
    let address = account.address.clone().unwrap_or_default();
    let socials = social_links(account);
    let lang = lang_attrs(content.lang.as_deref());
    let ft = content.footer_texts.as_ref().filter(|_| lang.is_some());
    let reason = ft.map(|f| f.reason.as_str()).filter(|t| !t.trim().is_empty()).unwrap_or("You're receiving this because you subscribed.");
    let unsub_label = ft.map(|f| f.unsubscribe.as_str()).filter(|t| !t.trim().is_empty()).unwrap_or("Unsubscribe");
    let social_html = if socials.is_empty() {
        String::new()
    } else {
        let links: Vec<String> = socials
            .iter()
            .map(|(label, url)| format!(r#"<a href="{}" style="color:#6b7280;text-decoration:underline;">{label}</a>"#, esc(url)))
            .collect();
        format!("{}<br>", links.join(" · "))
    };
    let footer = match opts.footer {
        Footer::Marketing => format!(
            r#"{social_html}{name}{addr}<br>{reason} <a href="{unsub}" data-pl-skip="1" style="color:#6b7280;text-decoration:underline;">{unsub_label}</a>"#,
            // The footer goes through the template engine with the body, so brand text (often read
            // from a website) must not be able to open a template tag.
            social_html = no_tpl(&social_html),
            name = no_tpl(&esc(&account.name)),
            addr = if address.is_empty() { String::new() } else { format!(" · {}", no_tpl(&esc(&address))) },
            unsub = esc(&unsub),
            reason = no_tpl(&esc(reason)),
            unsub_label = no_tpl(&esc(unsub_label)),
        ),
        Footer::Address => format!(
            "{}{}",
            no_tpl(&esc(&account.name)),
            if address.is_empty() { String::new() } else { format!(" · {}", no_tpl(&esc(&address))) }
        ),
        Footer::None => String::new(),
    };

    let (html_source, text_source) = match (&content.blocks, &content.html) {
        (Some(blocks), _) if !blocks.is_empty() => {
            let mut inner: String = blocks.iter().map(|b| block_html(b, &brand, font, 0)).collect();
            if lang.as_ref().is_some_and(|(_, rtl)| *rtl) {
                inner = mirror(&inner);
            }
            let mut text = String::new();
            blocks.iter().for_each(|b| block_text(b, &mut text));
            (layout(&inner, content.preheader.as_deref().unwrap_or(""), account, &theme, &footer, lang.as_ref()), text)
        }
        (_, Some(html)) if !html.trim().is_empty() => {
            let mut text = String::new();
            block_text(&json!({"type": "html", "html": html}), &mut text);
            let doc = if html.to_lowercase().contains("<body") {
                // Custom full HTML: make sure an unsubscribe link exists (and any footer is added).
                if html.contains("unsubscribe_url") || footer.is_empty() {
                    html.clone()
                } else {
                    html.replacen("</body>", &format!(r#"<div style="text-align:center;font-size:12px;color:#6b7280;padding:16px;">{footer}</div></body>"#), 1)
                }
            } else {
                layout(&format!("<tr><td style=\"padding:8px 24px;\">{html}</td></tr>"), content.preheader.as_deref().unwrap_or(""), account, &theme, &footer, lang.as_ref())
            };
            (doc, text)
        }
        _ => return Err(AppError::bad("email content needs blocks or html")),
    };

    let subject = render_str(&content.subject, ctx, false)?;
    let preheader = render_str(content.preheader.as_deref().unwrap_or(""), ctx, false)?;
    let mut html = render_str(&html_source, ctx, true)?;
    let mut text = render_str(&text_source, ctx, false)?;
    let signature = format!("{}{}", account.name, if address.is_empty() { String::new() } else { format!(", {address}") });
    match opts.footer {
        Footer::Marketing => {
            for (label, url) in &socials {
                text.push_str(&format!("\n{label}: {url}"));
            }
            text.push_str(&format!("\n--\n{signature}\n{unsub_label}: {unsub}\n"));
        }
        Footer::Address => text.push_str(&format!("\n--\n{signature}\n")),
        Footer::None => {}
    }

    if let Some(t) = tracking.filter(|_| opts.track_clicks) {
        html = rewrite_links(&html, t);
    }
    if let Some(t) = tracking.filter(|_| opts.track_opens) {
        let pixel = format!(
            r#"<img src="{}/t/o/{}" width="1" height="1" alt="" style="display:block;width:1px;height:1px;border:0;">"#,
            t.public_url,
            crate::auth::sign(t.secret, &t.message_id.simple().to_string())
        );
        html = match html.rfind("</body>") {
            Some(i) => format!("{}{}{}", &html[..i], pixel, &html[i..]),
            None => format!("{html}{pixel}"),
        };
    }
    Ok(RenderedEmail { subject, preheader, html, text: text.trim().to_string() + "\n" })
}

pub fn tracked_url(t: &Tracking<'_>, url: &str) -> String {
    let token = crate::auth::sign(t.secret, &t.message_id.simple().to_string());
    let sig = crate::auth::link_sig(t.secret, t.message_id, url);
    format!("{}/t/c/{}?u={}&s={}", t.public_url, token, urlencode(url), sig)
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn decode_entities(s: &str) -> String {
    s.replace("&#x2f;", "/").replace("&#x2F;", "/").replace("&#47;", "/").replace("&#x3d;", "=").replace("&#61;", "=")
        .replace("&quot;", "\"").replace("&#x27;", "'").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

fn rewrite_links(html: &str, t: &Tracking<'_>) -> String {
    static HREF: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<a\s([^>]*?)href="([^"]+)"([^>]*)>"#).unwrap());
    HREF.replace_all(html, |caps: &regex::Captures<'_>| {
        let whole = &caps[0];
        // Personalized links arrive HTML-escaped (`/` as `&#x2f;`); decode before matching.
        let raw = decode_entities(&caps[2]);
        let is_web = raw.starts_with("https://") || raw.starts_with("http://");
        if !is_web || whole.contains("data-pl-skip") || raw == t.unsubscribe_url || raw.starts_with(t.public_url) {
            return whole.to_string();
        }
        format!(r#"<a {}href="{}"{}>"#, &caps[1], esc(&tracked_url(t, &raw)), &caps[3])
    })
    .into_owned()
}

/// SMS: render the body, shorten links through click tracking and add opt-out language when needed.
pub fn render_sms(account: &Account, content: &SmsContent, ctx: &Value, tracking: Option<&Tracking<'_>>, first_message: bool) -> AppResult<String> {
    static URL: Lazy<Regex> = Lazy::new(|| Regex::new(r#"https?://[^\s]+"#).unwrap());
    let mut body = render_str(&content.body, ctx, false)?;
    if let Some(t) = tracking {
        body = URL
            .replace_all(&body, |c: &regex::Captures<'_>| tracked_url(t, &c[0]))
            .into_owned();
    }
    let lower = body.to_lowercase();
    if first_message || !(lower.contains("stop") || lower.contains("opt out")) {
        // Carriers require the brand name and opt-out instructions.
        if !lower.starts_with(&account.name.to_lowercase()) {
            body = format!("{}: {body}", account.name);
        }
        if !lower.contains("stop") {
            body.push_str(" Reply STOP to opt out.");
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account {
            id: Uuid::nil(),
            public_key: "pk".into(),
            name: "Acme".into(),
            website: None,
            from_name: None,
            from_email: None,
            reply_to: None,
            sms_from: None,
            timezone: "America/Los_Angeles".into(),
            address: Some("1 Main St, Portland OR".into()),
            brand_color: "#123456".into(),
            logo_url: None,
            social_links: serde_json::json!({}),
            plan: "free".into(),
            onboarding: json!({}),
            conversion_metric: "Placed Order".into(),
            created_at: chrono::Utc::now(),
            sending_paused_at: None,
            sending_paused_reason: None,
            email_daily_limit_override: None,
            email_sunset_enabled: true,
            link_domain: None,
            link_domain_status: "not_started".into(),
            link_domain_records: json!([]),
            verified_domains: Vec::new(),
        }
    }

    #[test]
    fn renders_blocks_with_tracking() {
        let acct = account();
        let content: EmailContent = serde_json::from_value(json!({
            "subject": "Hi {{ first_name|default('there') }}",
            "preheader": "New stuff",
            "blocks": [
                {"type": "heading", "text": "Hello {{ first_name }}"},
                {"type": "text", "html": "<p>Your city: {{ person.properties.city }}</p>"},
                {"type": "button", "text": "Shop", "href": "https://shop.example.com/?a=1&b=2"}
            ]
        }))
        .unwrap();
        let ctx = context(&acct, None, None, "https://pl.test/u/x");
        let t = Tracking { public_url: "https://pl.test", secret: "s", message_id: Uuid::nil(), unsubscribe_url: "https://pl.test/u/x".into() };
        let out = render_email(&acct, &content, &ctx, Some(&t)).unwrap();
        assert_eq!(out.subject, "Hi there");
        assert!(out.html.contains("https://pl.test/t/c/"));
        assert!(out.html.contains("https://pl.test/t/o/"));
        assert!(out.html.contains("https://pl.test/u/x"), "unsubscribe link stays untracked");
        assert!(out.html.contains("1 Main St"));
        assert!(out.text.contains("Shop: https://shop.example.com/?a=1&b=2"));
    }

    #[test]
    fn missing_names_fall_back_to_default() {
        let acct = account();
        let profile = Profile {
            id: Uuid::nil(), account_id: Uuid::nil(), email: Some("a@b.co".into()), phone_number: None, external_id: None,
            first_name: None, last_name: Some("".into()), properties: json!({}), email_consent: "subscribed".into(), sms_consent: "never_subscribed".into(),
            created_at: chrono::Utc::now(), updated_at: chrono::Utc::now(), last_event_at: None, locale: None,
        };
        let ctx = context(&acct, Some(&profile), None, "#");
        assert_eq!(render_str("Hi {{ first_name|default('there') }} {{ person.last_name|default('') }}", &ctx, false).unwrap().trim(), "Hi there");
    }

    #[test]
    fn escapes_profile_values() {
        let acct = account();
        let ctx = json!({"first_name": "<script>x</script>"});
        let content: EmailContent = serde_json::from_value(json!({"subject": "s", "blocks": [{"type": "text", "html": "<p>{{ first_name }}</p>"}]})).unwrap();
        let out = render_email(&acct, &content, &ctx, None).unwrap();
        assert!(!out.html.contains("<script>"));
    }

    #[test]
    fn fixed_width_tables_shrink_on_narrow_screens() {
        // Outlook keeps the 600px width attribute; everything else follows the media query, so no
        // table forces a narrow screen (or the dashboard preview) to scroll sideways.
        let content: EmailContent = serde_json::from_value(json!({"subject": "s", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]})).unwrap();
        let html = render_email(&account(), &content, &json!({}), None).unwrap().html;
        let tables: Vec<&str> = html.split("<table").skip(1).filter(|t| t.split('>').next().unwrap_or("").contains("width=\"600\"")).collect();
        assert_eq!(tables.len(), 2, "the card and the footer");
        for (t, class) in tables.iter().zip(["pl-card", "pl-foot"]) {
            assert!(t.split('>').next().unwrap().contains(&format!("class=\"{class}\"")), "{class}");
            assert!(html.contains(&format!(".{class}{{width:100%!important")), "{class} goes full width under 620px");
        }
    }

    #[test]
    fn footer_shows_social_links_and_brand_text_is_inert() {
        let mut acct = account();
        acct.name = "Acme {{ oops".into();
        acct.social_links = json!({"tiktok": "https://tiktok.com/@acme", "instagram": "https://instagram.com/acme", "evil": "javascript:alert(1)"});
        let content: EmailContent = serde_json::from_value(json!({"subject": "s", "blocks": [{"type": "text", "html": "<p>Hi</p>"}]})).unwrap();
        let out = render_email(&acct, &content, &json!({}), None).unwrap();
        let ig = out.html.find("https://instagram.com/acme").expect("instagram link");
        let tt = out.html.find("https://tiktok.com/@acme").expect("tiktok link");
        assert!(ig < tt, "fixed network order");
        assert!(!out.html.contains("javascript:"));
        assert!(out.html.contains("Acme &#123;&#123; oops"));
        assert!(out.text.contains("Instagram: https://instagram.com/acme"));
    }

    #[test]
    fn products_block_renders_a_grid_and_keeps_catalog_text_inert() {
        let content: EmailContent = serde_json::from_value(json!({"subject": "s", "blocks": [
            {"type": "products", "columns": 3, "button_text": "Buy", "items": [
                {"title": "Mug {{ oops }}", "url": "https://shop.test/mug", "image_url": "https://shop.test/mug.jpg", "price_text": "$12.00", "compare_at_text": "$15.00"},
                {"title": "Cap", "url": "javascript:alert(1)", "price_text": "$9.00"}
            ]},
            {"type": "products", "items": []}
        ]})).unwrap();
        let out = render_email(&account(), &content, &json!({}), None).unwrap();
        assert!(out.html.contains("Mug &#123;&#123; oops &#125;&#125;"), "titles can't run template code");
        assert!(out.html.contains("line-through;\">$15.00"));
        assert!(out.html.contains("width=\"33%\""));
        assert!(!out.html.contains("javascript:"));
        assert_eq!(out.html.matches(">Buy</a>").count(), 1, "no button without a web link");
        assert!(out.text.contains("Mug {\u{200B}{\u{200B} oops }} $12.00 https://shop.test/mug"));
        let tricky: EmailContent = serde_json::from_value(json!({"subject": "s", "blocks": [{"type": "products", "items": [{"title": "{{{ 7*7 }}}{% if 1 %}x{% endif %}"}]}]})).unwrap();
        let out = render_email(&account(), &tricky, &json!({}), None).unwrap();
        assert!(!out.text.contains("49") && out.text.contains("7*7"), "{}", out.text);
    }

    #[test]
    fn sms_adds_compliance() {
        let acct = account();
        let body = render_sms(&acct, &SmsContent { body: "Sale today! https://x.co/s".into(), media_url: None }, &json!({}), None, true).unwrap();
        assert!(body.starts_with("Acme: "));
        assert!(body.ends_with("Reply STOP to opt out."));
    }
}
