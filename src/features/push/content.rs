//! What a push notification says: its content as stored on a campaign or flow step, checks when it's
//! saved, and rendering for one person.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};

pub const MAX_TITLE: usize = 100;
pub const MAX_BODY: usize = 500;
pub const MAX_URL: usize = 500;
pub const MAX_ACTIONS: usize = 2;
pub const MAX_ACTION_TITLE: usize = 40;
pub const MAX_TOPIC: usize = 32;
/// The encrypted record (web) and the payload (APNs, FCM) must stay under 4 KB; leave room.
pub const MAX_PAYLOAD: usize = 3500;

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct Action {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
}

/// Push content. `title`, `body`, `url` and action titles take personalization tags. `platforms`
/// picks the devices it goes to (default all); the per-platform fields only apply there.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PushContent {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Where a tap goes (http or https). Without it, a tap opens the account's website.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Small icon (web). Defaults to the account's logo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Large picture (web on Chrome, Edge and Android; iOS and Android apps that handle it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Up to two buttons (web on Chrome and Edge; apps that register them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    /// "web", "ios", "android"; empty means all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// How long the push service keeps it for an offline device, in seconds (default 1 day, at most 28).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<i64>,
    /// very_low, low, normal (default) or high.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency: Option<String>,
    /// A newer notification with the same topic replaces an undelivered older one (letters, digits, - and _).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// iOS: the sound to play ("default", or a sound file in the app); none when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ios_sound: Option<String>,
    /// iOS: the app icon's badge number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ios_badge: Option<i64>,
    /// Android: the notification channel id the app created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android_channel_id: Option<String>,
}

pub const PLATFORMS: [&str; 3] = ["web", "ios", "android"];

impl PushContent {
    pub fn platforms(&self) -> Vec<String> {
        if self.platforms.is_empty() { PLATFORMS.iter().map(|s| s.to_string()).collect() } else { self.platforms.clone() }
    }

    pub fn ttl(&self) -> i64 {
        self.ttl_seconds.unwrap_or(86_400).clamp(0, 28 * 86_400)
    }

    pub fn urgency(&self) -> &str {
        match self.urgency.as_deref() {
            Some("very_low") => "very-low",
            Some("low") => "low",
            Some("high") => "high",
            _ => "normal",
        }
    }
}

/// Parse and check stored content (at save time and before sending). Coupon codes aren't
/// supported in push (each code would have to be claimed per device and kept out of the lock
/// screen): they're refused here with a plain message.
pub fn parse(v: &Value) -> AppResult<PushContent> {
    let c: PushContent = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid push content: {e}")))?;
    check(&c)?;
    Ok(c)
}

fn http_url(field: &str, u: &str) -> AppResult<()> {
    let t = u.trim();
    if t.chars().count() > MAX_URL {
        return Err(AppError::bad(format!("{field} can be at most {MAX_URL} characters")));
    }
    // Personalized links are checked after rendering; the start must already be a web address.
    let lower = t.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("{{")) {
        return Err(AppError::bad(format!("{field} must start with https://")));
    }
    Ok(())
}

pub fn check(c: &PushContent) -> AppResult<()> {
    if c.title.chars().count() > MAX_TITLE {
        return Err(AppError::bad(format!("the title can be at most {MAX_TITLE} characters")));
    }
    if c.body.chars().count() > MAX_BODY {
        return Err(AppError::bad(format!("the message can be at most {MAX_BODY} characters")));
    }
    for (name, u) in [("url", &c.url), ("icon", &c.icon), ("image", &c.image)] {
        if let Some(u) = u.as_deref().filter(|u| !u.trim().is_empty()) {
            http_url(name, u)?;
        }
    }
    if c.actions.len() > MAX_ACTIONS {
        return Err(AppError::bad(format!("a notification can have at most {MAX_ACTIONS} buttons")));
    }
    for a in &c.actions {
        if a.title.trim().is_empty() || a.title.chars().count() > MAX_ACTION_TITLE {
            return Err(AppError::bad(format!("each button needs a label of 1 to {MAX_ACTION_TITLE} characters")));
        }
        http_url("a button's link", &a.url)?;
    }
    if c.platforms.len() > PLATFORMS.len() || c.platforms.iter().any(|p| !PLATFORMS.contains(&p.as_str())) {
        return Err(AppError::bad("platforms must be web, ios and/or android"));
    }
    if c.ttl_seconds.is_some_and(|t| !(0..=28 * 86_400).contains(&t)) {
        return Err(AppError::bad("ttl_seconds must be 0 to 2419200 (28 days)"));
    }
    if c.urgency.as_deref().is_some_and(|u| !matches!(u, "very_low" | "low" | "normal" | "high")) {
        return Err(AppError::bad("urgency must be very_low, low, normal or high"));
    }
    if let Some(t) = c.topic.as_deref().filter(|t| !t.is_empty()) {
        if t.len() > MAX_TOPIC || !t.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
            return Err(AppError::bad(format!("topic must be up to {MAX_TOPIC} letters, digits, - or _")));
        }
    }
    if c.ios_badge.is_some_and(|b| !(0..=9999).contains(&b)) {
        return Err(AppError::bad("ios_badge must be 0 to 9999"));
    }
    if c.ios_sound.as_deref().is_some_and(|s| s.len() > 100) || c.android_channel_id.as_deref().is_some_and(|s| s.len() > 100) {
        return Err(AppError::bad("ios_sound and android_channel_id can be at most 100 characters"));
    }
    let v = serde_json::to_value(c)?;
    if crate::features::coupons::template::may_use_coupons(&v) && v.to_string().contains("coupon_") && (v.to_string().contains("{%") || v.to_string().contains("coupon_code")) {
        return Err(AppError::bad("coupon codes aren't supported in push notifications; send them by email or text"));
    }
    Ok(())
}

/// Problems that stop a push from being sent (shown before a campaign or flow goes live).
pub fn problems(v: &Value) -> Vec<String> {
    match parse(v) {
        Err(e) => vec![e.public_message()],
        Ok(c) if c.title.trim().is_empty() && c.body.trim().is_empty() => vec!["Write the notification's title or message".into()],
        Ok(_) => Vec::new(),
    }
}

/// A notification rendered for one person (URLs still untracked).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rendered {
    pub title: String,
    pub body: String,
    pub url: String,
    pub icon: Option<String>,
    pub image: Option<String>,
    pub actions: Vec<Action>,
}

fn cut(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n { t.to_string() } else { t.chars().take(n).collect() }
}

/// A rendered link: http(s) only, else None.
fn link(raw: &str, ctx: &Value) -> AppResult<Option<String>> {
    let r = crate::render::render_str(raw, ctx, false)?;
    let r = r.trim();
    match url::Url::parse(r) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.as_str().len() <= 2 * MAX_URL => Ok(Some(u.to_string())),
        _ => Ok(None),
    }
}

/// Render the content for the person in `ctx` (see `render::context`). `fallback_url` is where a
/// tap goes when the content has no link (the account's website).
pub fn render(c: &PushContent, ctx: &Value, fallback_url: &str, logo: Option<&str>) -> AppResult<Rendered> {
    let title = cut(&crate::render::render_str(&c.title, ctx, false)?, MAX_TITLE);
    let body = cut(&crate::render::render_str(&c.body, ctx, false)?, MAX_BODY);
    if title.is_empty() && body.is_empty() {
        return Err(AppError::bad("the notification has no title or message"));
    }
    let url = match c.url.as_deref().filter(|u| !u.trim().is_empty()) {
        Some(u) => link(u, ctx)?.ok_or_else(|| AppError::bad("the notification's link isn't a web address once filled in"))?,
        None => fallback_url.to_string(),
    };
    let mut actions = Vec::new();
    for a in &c.actions {
        let title = cut(&crate::render::render_str(&a.title, ctx, false)?, MAX_ACTION_TITLE);
        if let (false, Some(url)) = (title.is_empty(), link(&a.url, ctx)?) {
            actions.push(Action { title, url });
        }
    }
    let icon = match c.icon.as_deref().filter(|u| !u.trim().is_empty()) {
        Some(u) => link(u, ctx)?,
        None => logo.map(str::to_string),
    };
    let image = match c.image.as_deref().filter(|u| !u.trim().is_empty()) {
        Some(u) => link(u, ctx)?,
        None => None,
    };
    Ok(Rendered { title, body, url, icon, image, actions })
}

/// What a new push campaign starts with.
pub fn starter() -> Value {
    json!({"title": "", "body": ""})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks() {
        assert!(parse(&json!({"title": "Hi", "body": "There"})).is_ok());
        assert!(parse(&json!({"title": "x".repeat(101)})).is_err());
        assert!(parse(&json!({"title": "Hi", "url": "javascript:alert(1)"})).is_err());
        assert!(parse(&json!({"title": "Hi", "platforms": ["web", "fax"]})).is_err());
        assert!(parse(&json!({"title": "Hi", "actions": [{"title": "a", "url": "https://a.test"}, {"title": "b", "url": "https://b.test"}, {"title": "c", "url": "https://c.test"}]})).is_err());
        assert!(parse(&json!({"title": "Hi", "subject": "x"})).is_err(), "unknown keys are refused");
        assert!(parse(&json!({"title": "Your code {% coupon_code 'WELCOME' %}"})).is_err());
        assert!(parse(&json!({"title": "Hi", "topic": "has space"})).is_err());
    }

    #[test]
    fn renders_and_falls_back() {
        let ctx = json!({"first_name": "Ana"});
        let c = parse(&json!({"title": "Hi {{ first_name }}", "body": "Sale", "actions": [{"title": "Shop", "url": "https://acme.test/s"}]})).unwrap();
        let r = render(&c, &ctx, "https://acme.test", Some("https://acme.test/logo.png")).unwrap();
        assert_eq!(r.title, "Hi Ana");
        assert_eq!(r.url, "https://acme.test");
        assert_eq!(r.icon.as_deref(), Some("https://acme.test/logo.png"));
        assert_eq!(r.actions.len(), 1);
        let bad = parse(&json!({"title": "x", "url": "{{ first_name }}"})).unwrap();
        assert!(render(&bad, &ctx, "https://acme.test", None).is_err());
    }
}
