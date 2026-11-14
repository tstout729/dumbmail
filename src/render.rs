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
                r#"<img src="{src}" alt="{alt}" width="{w}" style="display:block;width:100%;max-width:{w}px;height:auto;border:0;border-radius:6px;">"#,
                src = esc(s(b, "src")),
                alt = esc(s(b, "alt"))
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
        "html" => format!(r#"<tr><td style="{pad}">{}</td></tr>"#, s(b, "html")),
        _ => String::new(),
    }
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
        _ => {}
    }
}

fn layout(inner: &str, preheader: &str, account: &Account, theme: &Theme, footer: &str) -> String {
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
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light"><title></title><style>@media (max-width:620px){{.pl-card{{width:100%!important;border-radius:0!important}}.pl-col{{display:block!important;width:100%!important}}}}a{{color:inherit}}</style></head><body style="margin:0;padding:0;background:{bg};"><div style="display:none;max-height:0;overflow:hidden;opacity:0;">{pre}&#8199;&#847;&#8199;&#847;&#8199;&#847;</div><table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:{bg};"><tr><td align="center" style="padding:24px 8px;"><table role="presentation" class="pl-card" width="600" cellpadding="0" cellspacing="0" style="width:600px;max-width:600px;background:#ffffff;border-radius:10px;overflow:hidden;">{logo}<tr><td style="height:8px;"></td></tr>{inner}<tr><td style="height:16px;"></td></tr></table><table role="presentation" width="600" cellpadding="0" cellspacing="0" style="max-width:600px;"><tr><td style="padding:16px 24px;text-align:center;font-family:{font};font-size:12px;line-height:1.5;color:#6b7280;">{footer}</td></tr></table></td></tr></table></body></html>"#,
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
    env(html)
        .render_str(source, ctx)
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
        root.insert("first_name".into(), json!(p.first_name));
        root.insert("last_name".into(), json!(p.last_name));
        root.insert("email".into(), json!(p.email));
        root.insert("phone_number".into(), json!(p.phone_number));
        root.insert(
            "person".into(),
            json!({
                "id": p.id, "email": p.email, "phone_number": p.phone_number, "external_id": p.external_id,
                "first_name": p.first_name, "last_name": p.last_name, "properties": p.properties,
            }),
        );
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

pub fn render_email(
    account: &Account,
    content: &EmailContent,
    ctx: &Value,
    tracking: Option<&Tracking<'_>>,
) -> AppResult<RenderedEmail> {
    let theme = content.theme.clone().unwrap_or_default();
    let brand = safe_color(theme.brand_color.as_deref().unwrap_or(&account.brand_color), "#1f6f5c");
    let font = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
    let unsub = tracking.map(|t| t.unsubscribe_url.clone()).unwrap_or_else(|| "#".into());
    let address = account.address.clone().unwrap_or_default();
    let footer = format!(
        r#"{name}{addr}<br>You're receiving this because you subscribed. <a href="{unsub}" data-pl-skip="1" style="color:#6b7280;text-decoration:underline;">Unsubscribe</a>"#,
        name = esc(&account.name),
        addr = if address.is_empty() { String::new() } else { format!(" · {}", esc(&address)) },
        unsub = esc(&unsub),
    );

    let (html_source, text_source) = match (&content.blocks, &content.html) {
        (Some(blocks), _) if !blocks.is_empty() => {
            let inner: String = blocks.iter().map(|b| block_html(b, &brand, font, 0)).collect();
            let mut text = String::new();
            blocks.iter().for_each(|b| block_text(b, &mut text));
            (layout(&inner, content.preheader.as_deref().unwrap_or(""), account, &theme, &footer), text)
        }
        (_, Some(html)) if !html.trim().is_empty() => {
            let mut text = String::new();
            block_text(&json!({"type": "html", "html": html}), &mut text);
            let doc = if html.to_lowercase().contains("<body") {
                // Custom full HTML: make sure an unsubscribe link exists.
                if html.contains("unsubscribe_url") {
                    html.clone()
                } else {
                    html.replacen("</body>", &format!(r#"<div style="text-align:center;font-size:12px;color:#6b7280;padding:16px;">{footer}</div></body>"#), 1)
                }
            } else {
                layout(&format!("<tr><td style=\"padding:8px 24px;\">{html}</td></tr>"), content.preheader.as_deref().unwrap_or(""), account, &theme, &footer)
            };
            (doc, text)
        }
        _ => return Err(AppError::bad("email content needs blocks or html")),
    };

    let subject = render_str(&content.subject, ctx, false)?;
    let preheader = render_str(content.preheader.as_deref().unwrap_or(""), ctx, false)?;
    let mut html = render_str(&html_source, ctx, true)?;
    let mut text = render_str(&text_source, ctx, false)?;
    text.push_str(&format!("\n--\n{}{}\nUnsubscribe: {unsub}\n", account.name, if address.is_empty() { String::new() } else { format!(", {address}") }));

    if let Some(t) = tracking {
        html = rewrite_links(&html, t);
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
            plan: "free".into(),
            onboarding: json!({}),
            sending_domain: None,
            sending_domain_status: "not_started".into(),
            sending_domain_records: json!([]),
            conversion_metric: "Placed Order".into(),
            created_at: chrono::Utc::now(),
            mail_from_domain: None,
            link_domain: None,
            link_domain_status: "not_started".into(),
            link_domain_records: json!([]),
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
    fn escapes_profile_values() {
        let acct = account();
        let ctx = json!({"first_name": "<script>x</script>"});
        let content: EmailContent = serde_json::from_value(json!({"subject": "s", "blocks": [{"type": "text", "html": "<p>{{ first_name }}</p>"}]})).unwrap();
        let out = render_email(&acct, &content, &ctx, None).unwrap();
        assert!(!out.html.contains("<script>"));
    }

    #[test]
    fn sms_adds_compliance() {
        let acct = account();
        let body = render_sms(&acct, &SmsContent { body: "Sale today! https://x.co/s".into(), media_url: None }, &json!({}), None, true).unwrap();
        assert!(body.starts_with("Acme: "));
        assert!(body.ends_with("Reply STOP to opt out."));
    }
}
