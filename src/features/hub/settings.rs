//! The hub's settings: on or off, the launcher, sections, the custom object types and fields shown,
//! help entries and look. Every save is checked as a whole; a field that is sensitive, or for a
//! booking type looks like access or address details, is refused here and filtered again when the
//! page is drawn (a field can become sensitive after it was chosen).

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::objects::types::{self as object_types, ObjectType};
use crate::models::Account;
use crate::state::AppState;

pub const MAX_OBJECT_TYPES: usize = 10;
pub const MAX_FIELDS_PER_TYPE: usize = 20;
pub const MAX_HELP: usize = 20;
pub const MAX_QUESTION: usize = 200;
pub const MAX_ANSWER: usize = 2000;
pub const MAX_LABEL: usize = 40;
pub const MAX_CONTACT_URL: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Launcher {
    /// No launcher; the hub is reached by its address or `DumbMail.openHub()`.
    #[default]
    Off,
    /// A floating "My account" button on every page with the snippet.
    Button,
    /// Only from the store's own link or button (`data-dumbmail-hub` or `DumbMail.openHub()`).
    Link,
}

impl Launcher {
    pub fn as_str(self) -> &'static str {
        match self {
            Launcher::Off => "off",
            Launcher::Button => "button",
            Launcher::Link => "link",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    /// A stay, appointment or reservation: access and address fields can never show.
    Booking,
    Subscription,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Font {
    #[default]
    System,
    Sans,
    Serif,
}

impl Font {
    pub fn css(self) -> &'static str {
        match self {
            Font::System => "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif",
            Font::Sans => "'Helvetica Neue',Helvetica,Arial,sans-serif",
            Font::Serif => "Georgia,'Times New Roman',serif",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sections {
    pub orders: bool,
    pub bookings: bool,
    pub recommendations: bool,
    pub preferences: bool,
    pub help: bool,
}

impl Default for Sections {
    fn default() -> Self {
        Sections { orders: true, bookings: true, recommendations: true, preferences: true, help: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObjectChoice {
    #[serde(rename = "type")]
    pub type_key: String,
    #[serde(default)]
    pub kind: ObjectKind,
    /// The fields shown, in this order ("show in hub").
    #[serde(default)]
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HelpEntry {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub launcher: Launcher,
    pub button_label: String,
    pub sections: Sections,
    pub object_types: Vec<ObjectChoice>,
    pub help: Vec<HelpEntry>,
    /// Where "Contact us" goes: a web page (https://) or an email address (mailto:).
    pub contact_url: Option<String>,
    pub contact_label: Option<String>,
    /// Buttons and links; the account's brand colour when empty.
    pub accent_color: Option<String>,
    pub font: Font,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            launcher: Launcher::Off,
            button_label: "My account".into(),
            sections: Sections::default(),
            object_types: Vec::new(),
            help: Vec::new(),
            contact_url: None,
            contact_label: None,
            accent_color: None,
            font: Font::System,
        }
    }
}

/// A change: only the fields given are replaced.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub enabled: Option<bool>,
    pub launcher: Option<Launcher>,
    pub button_label: Option<String>,
    pub sections: Option<Sections>,
    pub object_types: Option<Vec<ObjectChoice>>,
    pub help: Option<Vec<HelpEntry>>,
    #[serde(default, deserialize_with = "nullable")]
    pub contact_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub contact_label: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub accent_color: Option<Option<String>>,
    pub font: Option<Font>,
}

/// `null` clears a field; leaving it out keeps it.
fn nullable<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::<String>::deserialize(d)?))
}

pub fn patch_schema() -> Value {
    let section = json!({"type": "boolean"});
    json!({
        "enabled": {"type": "boolean", "description": "Turn the hub on or off."},
        "launcher": {"type": "string", "enum": ["off", "button", "link"], "description": "off: no launcher. button: a floating button on every page with the website snippet. link: only from your own link or button (data-dumbmail-hub or DumbMail.openHub())."},
        "button_label": {"type": "string", "maxLength": MAX_LABEL, "description": "The launcher's text, e.g. \"My account\"."},
        "sections": {"type": "object", "properties": {"orders": section, "bookings": section, "recommendations": section, "preferences": section, "help": section}, "required": ["orders", "bookings", "recommendations", "preferences", "help"], "additionalProperties": false},
        "object_types": {"type": "array", "maxItems": MAX_OBJECT_TYPES, "description": "Custom object types shown under Bookings and subscriptions, with the fields shown for each.", "items": {"type": "object", "properties": {
            "type": {"type": "string", "description": "The object type's key, e.g. reservation."},
            "kind": {"type": "string", "enum": ["booking", "subscription", "other"], "description": "booking: access and address fields can never be shown."},
            "fields": {"type": "array", "maxItems": MAX_FIELDS_PER_TYPE, "items": {"type": "string"}, "description": "Field keys to show, in order. Sensitive fields are refused."}
        }, "required": ["type"], "additionalProperties": false}},
        "help": {"type": "array", "maxItems": MAX_HELP, "items": {"type": "object", "properties": {"question": {"type": "string", "maxLength": MAX_QUESTION}, "answer": {"type": "string", "maxLength": MAX_ANSWER}}, "required": ["question", "answer"], "additionalProperties": false}},
        "contact_url": {"type": ["string", "null"], "maxLength": MAX_CONTACT_URL, "description": "Where \"Contact us\" goes: https://... or mailto:..."},
        "contact_label": {"type": ["string", "null"], "maxLength": MAX_LABEL},
        "accent_color": {"type": ["string", "null"], "description": "Hex colour for buttons and links; your brand colour when null."},
        "font": {"type": "string", "enum": ["system", "sans", "serif"]}
    })
}

/// Words that name how to get into or find a place. A booking type's fields with these in their
/// key or label never show in the hub (whole words of the key or label).
const ACCESS_WORDS: &[&str] = &[
    "address", "street", "city", "zip", "postcode", "postal", "location", "lat", "lng", "latitude", "longitude", "door", "code", "codes", "lock", "lockbox", "key", "keys",
    "keypad", "wifi", "password", "passcode", "pin", "gate", "access", "entry", "directions", "instructions", "parking", "unit", "apartment", "apt", "suite", "floor",
    "building", "map", "coordinates", "checkin_instructions", "arrival",
];

/// Whether a field looks like access or address details (for booking types).
pub fn access_like(key: &str, label: Option<&str>) -> bool {
    let words = |s: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        // Split on anything but letters and digits, and between a lower and an upper case letter.
        let mut prev_lower = false;
        for ch in s.chars() {
            if !ch.is_alphanumeric() || (ch.is_uppercase() && prev_lower) {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            if ch.is_alphanumeric() {
                cur.extend(ch.to_lowercase());
            }
            prev_lower = ch.is_lowercase();
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    };
    let mut all = words(key);
    all.extend(words(label.unwrap_or("")));
    let joined = all.join("");
    all.iter().any(|w| ACCESS_WORDS.contains(&w.as_str())) || ["address", "password", "passcode", "wifi", "lockbox", "doorcode", "accesscode", "gatecode", "keycode"].iter().any(|w| joined.contains(w))
}

/// Whether the hub may ever show this field of this type as the type is now.
pub fn showable(t: &ObjectType, kind: ObjectKind, key: &str) -> bool {
    let Some(def) = t.field(key) else { return false };
    !t.is_sensitive(key) && !(kind == ObjectKind::Booking && access_like(key, def.label.as_deref()))
}

fn clean_text(s: &str, max: usize, what: &str) -> AppResult<String> {
    let t = s.trim();
    if t.chars().count() > max {
        return Err(AppError::bad(format!("{what} can be at most {max} characters")));
    }
    Ok(t.to_string())
}

/// Check and tidy a whole set of settings against the account's object types as they are now.
pub async fn check(db: &PgPool, account_id: Uuid, mut s: Settings) -> AppResult<Settings> {
    s.button_label = clean_text(&s.button_label, MAX_LABEL, "The button label")?;
    if s.button_label.is_empty() {
        s.button_label = "My account".into();
    }
    if s.object_types.len() > MAX_OBJECT_TYPES {
        return Err(AppError::bad(format!("Choose at most {MAX_OBJECT_TYPES} object types")));
    }
    let mut seen = std::collections::HashSet::new();
    for c in &mut s.object_types {
        c.type_key = c.type_key.trim().to_string();
        if !seen.insert(c.type_key.clone()) {
            return Err(AppError::bad(format!("{:?} is listed twice", c.type_key)));
        }
        let t = object_types::by_key(db, account_id, &c.type_key).await?.ok_or_else(|| AppError::bad(format!("There's no object type {:?}", c.type_key)))?;
        if c.fields.len() > MAX_FIELDS_PER_TYPE {
            return Err(AppError::bad(format!("Choose at most {MAX_FIELDS_PER_TYPE} fields for {}", t.name)));
        }
        let mut fields_seen = std::collections::HashSet::new();
        for f in &c.fields {
            if !fields_seen.insert(f.as_str()) {
                return Err(AppError::bad(format!("{f:?} is listed twice for {}", t.name)));
            }
            let Some(def) = t.field(f) else { return Err(AppError::bad(format!("{} has no field {f:?}", t.name))) };
            if t.is_sensitive(f) {
                return Err(AppError::bad(format!("{f:?} is marked sensitive, so it can never be shown in the hub")));
            }
            if c.kind == ObjectKind::Booking && access_like(f, def.label.as_deref()) {
                return Err(AppError::bad(format!("{f:?} looks like access or address details, which a booking never shows in the hub")));
            }
        }
    }
    if s.help.len() > MAX_HELP {
        return Err(AppError::bad(format!("Add at most {MAX_HELP} help entries")));
    }
    for h in &mut s.help {
        h.question = clean_text(&h.question, MAX_QUESTION, "A help question")?;
        h.answer = clean_text(&h.answer, MAX_ANSWER, "A help answer")?;
        if h.question.is_empty() || h.answer.is_empty() {
            return Err(AppError::bad("Each help entry needs a question and an answer"));
        }
    }
    s.contact_url = match s.contact_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        None => None,
        Some(u) => Some(check_contact(u)?),
    };
    s.contact_label = s.contact_label.as_deref().map(|l| clean_text(l, MAX_LABEL, "The contact label")).transpose()?.filter(|l| !l.is_empty());
    s.accent_color = match s.accent_color.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        None => None,
        Some(c) if crate::render::safe_color(c, "") == c => Some(c.to_string()),
        Some(_) => return Err(AppError::bad("The accent colour must be a hex colour like #1f6f5c")),
    };
    Ok(s)
}

fn check_contact(u: &str) -> AppResult<String> {
    if u.len() > MAX_CONTACT_URL {
        return Err(AppError::bad(format!("The contact link can be at most {MAX_CONTACT_URL} characters")));
    }
    if let Some(addr) = u.strip_prefix("mailto:") {
        return crate::models::normalize_email(addr).map(|e| format!("mailto:{e}")).ok_or_else(|| AppError::bad("The contact email isn't a valid address"));
    }
    if u.contains('@') && !u.contains('/') {
        return crate::models::normalize_email(u).map(|e| format!("mailto:{e}")).ok_or_else(|| AppError::bad("The contact email isn't a valid address"));
    }
    let parsed = url::Url::parse(u).map_err(|_| AppError::bad("The contact link must be a web address (https://...) or an email address"))?;
    if !matches!(parsed.scheme(), "https" | "http") || parsed.host_str().is_none() {
        return Err(AppError::bad("The contact link must be a web address (https://...) or an email address"));
    }
    Ok(parsed.to_string())
}

/// The account's settings (defaults when it never saved any).
pub async fn load(db: &PgPool, account_id: Uuid) -> AppResult<Settings> {
    let row: Option<(bool, String, Value)> = sqlx::query_as("SELECT enabled, launcher, config FROM hub_settings WHERE account_id = $1").bind(account_id).fetch_optional(db).await?;
    let Some((enabled, launcher, config)) = row else { return Ok(Settings::default()) };
    let mut s: Settings = serde_json::from_value(config).unwrap_or_default();
    s.enabled = enabled;
    s.launcher = serde_json::from_value(json!(launcher)).unwrap_or_default();
    Ok(s)
}

async fn save(db: &PgPool, account_id: Uuid, s: &Settings) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO hub_settings (account_id, enabled, launcher, config, updated_at) VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (account_id) DO UPDATE SET enabled = $2, launcher = $3, config = $4, updated_at = now()",
    )
    .bind(account_id)
    .bind(s.enabled)
    .bind(s.launcher.as_str())
    .bind(serde_json::to_value(s).map_err(anyhow::Error::from)?)
    .execute(db)
    .await?;
    Ok(())
}

/// Apply a change and save it (checked as a whole), with an audit entry.
pub async fn update(state: &AppState, ctx: &Ctx, body: Value) -> AppResult<Value> {
    let p: Patch = serde_json::from_value(body).map_err(|e| AppError::bad(format!("invalid settings: {e}")))?;
    let mut s = load(&state.db, ctx.account_id).await?;
    let was_enabled = s.enabled;
    if let Some(v) = p.enabled {
        s.enabled = v;
    }
    if let Some(v) = p.launcher {
        s.launcher = v;
    }
    if let Some(v) = p.button_label {
        s.button_label = v;
    }
    if let Some(v) = p.sections {
        s.sections = v;
    }
    if let Some(v) = p.object_types {
        s.object_types = v;
    }
    if let Some(v) = p.help {
        s.help = v;
    }
    if let Some(v) = p.contact_url {
        s.contact_url = v;
    }
    if let Some(v) = p.contact_label {
        s.contact_label = v;
    }
    if let Some(v) = p.accent_color {
        s.accent_color = v;
    }
    if let Some(v) = p.font {
        s.font = v;
    }
    let s = check(&state.db, ctx.account_id, s).await?;
    save(&state.db, ctx.account_id, &s).await?;
    let action = match (was_enabled, s.enabled) {
        (false, true) => "hub.enable",
        (true, false) => "hub.disable",
        _ => "hub.settings_change",
    };
    crate::features::team::audit(
        state,
        ctx,
        action,
        None,
        json!({"launcher": s.launcher.as_str(), "object_types": s.object_types.iter().map(|c| c.type_key.clone()).collect::<Vec<_>>(), "help_entries": s.help.len()}),
    )
    .await;
    settings_json(state, ctx.account_id).await
}

/// Whether the hub can open in a panel on the store's own pages: only on a branded link domain
/// that is the store's own site's subdomain (so the panel is the same site and its sign-in cookie
/// works). Otherwise it opens in a new tab.
pub fn embed_site(account: &Account) -> Option<String> {
    let domain = account.link_domain.as_deref().filter(|_| account.link_domain_status == "active")?;
    let hosts = crate::features::catalog::learn::hosts_of(account.website.as_deref(), &account.verified_domains);
    hosts.into_iter().find(|h| domain.ends_with(&format!(".{h}")))
}

/// Settings plus what the dashboard needs to show them: the hub's address, whether it can open in
/// a panel, and every object type with the fields that could be shown.
pub async fn settings_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let s = load(&state.db, account_id).await?;
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let types: Vec<Value> = object_types::list(&state.db, account_id)
        .await?
        .into_iter()
        .map(|(t, n)| {
            let fields: Vec<Value> = t
                .fields
                .iter()
                .map(|f| json!({"key": f.key, "label": f.label, "type": f.kind, "sensitive": t.is_sensitive(&f.key), "access_like": access_like(&f.key, f.label.as_deref())}))
                .collect();
            json!({"key": t.key, "name": t.name, "plural": t.plural, "count": n, "fields": fields})
        })
        .collect();
    let mut out = serde_json::to_value(&s).map_err(anyhow::Error::from)?;
    out["url"] = json!(super::session::hub_url(state, &account));
    out["embed_site"] = json!(embed_site(&account));
    out["link_domain"] = json!({"domain": account.link_domain, "active": account.link_domain_status == "active"});
    out["available_types"] = json!(types);
    Ok(out)
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/hub/settings", get(get_h).patch(patch_h)).route("/api/hub/preview", get(preview_h))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings_json(&state, ctx.account_id).await?))
}

async fn patch_h(State(state): State<AppState>, ctx: Ctx, Json(b): Json<Value>) -> AppResult<Json<Value>> {
    Ok(Json(update(&state, &ctx, b).await?))
}

/// The hub as a made-up sample shopper would see it with the saved settings.
async fn preview_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let s = load(&state.db, ctx.account_id).await?;
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let types = object_types::list(&state.db, ctx.account_id).await?.into_iter().map(|(t, _)| t).collect::<Vec<_>>();
    let view = super::data::sample(&account, &s, &types);
    let html = super::page::hub_page(&super::page::Frame::preview(&account, &s), &view);
    Ok(Json(json!({"html": html})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_words_are_caught_in_keys_and_labels() {
        for (k, l) in [("door_code", None), ("wifiPassword", None), ("street_address", None), ("notes", Some("Lockbox code")), ("checkin_instructions", None), ("unit", None), ("lat", None)] {
            assert!(access_like(k, l), "{k} {l:?}");
        }
        for (k, l) in [("check_in", None), ("nights", None), ("plan", Some("Plan name")), ("status", None), ("guests", None), ("renews_on", None)] {
            assert!(!access_like(k, l), "{k} {l:?}");
        }
    }

    #[test]
    fn contact_links_are_web_or_email() {
        assert_eq!(check_contact("help@shop.test").unwrap(), "mailto:help@shop.test");
        assert_eq!(check_contact("mailto:Help@Shop.test").unwrap(), "mailto:help@shop.test");
        assert!(check_contact("https://shop.test/help").is_ok());
        assert!(check_contact("javascript:alert(1)").is_err());
        assert!(check_contact("ftp://shop.test").is_err());
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(serde_json::from_value::<Patch>(json!({"enabeld": true})).is_err());
        let p: Patch = serde_json::from_value(json!({"contact_url": null})).unwrap();
        assert_eq!(p.contact_url, Some(None));
        let p: Patch = serde_json::from_value(json!({})).unwrap();
        assert_eq!(p.contact_url, None);
    }
}
