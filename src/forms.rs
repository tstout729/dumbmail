//! Sign-up form config: validation, update merging, the schedule, preview links and checking a
//! visitor's answers against the form's fields. docs/FORMS.md is the reference for every key.

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

const SEE_DOCS: &str = "See docs/FORMS.md (MCP resource dumbmail://docs/forms).";

/// The content keys an A/B test version can change; everything else comes from the form itself (version A).
pub const VARIANT_FIELDS: [&str; 9] = ["headline", "body", "button_text", "image_url", "image_alt", "no_thanks_text", "success_message", "success_body", "discount_code"];

/// Field keys with a built-in meaning (everything else is a custom profile property).
const STANDARD_FIELDS: [&str; 5] = ["email", "phone_number", "first_name", "last_name", "birthday"];
/// Keys that can't be custom fields: profile columns and the sign-up request's own keys.
const RESERVED_FIELDS: [&str; 14] = [
    "id", "external_id", "properties", "email_consent", "sms_consent", "whatsapp_consent", "created_at", "updated_at", "last_event_at", "list_ids", "form_id", "variant", "step", "fields",
];
const FIELD_TYPES: [&str; 7] = ["text", "number", "date", "select", "radio", "checkboxes", "checkbox"];
const MAX_FIELDS: usize = 12;
/// Longest answer saved for a text field.
pub const MAX_ANSWER: usize = 500;

#[derive(Clone, Copy)]
enum Rule {
    Text(usize),
    Url,
    Int(i64, i64),
    IntOneOf(&'static [i64]),
    Color,
    Bool,
    OneOf(&'static [&'static str]),
    TextList(usize, usize),
    DateTime,
    Css,
    /// Checked by its own function.
    Nested,
}

use Rule::*;

const TOP: &[(&str, Rule)] = &[
    ("headline", Text(200)),
    ("body", Text(1000)),
    ("button_text", Text(60)),
    ("image_url", Url),
    ("image_alt", Text(200)),
    ("no_thanks_text", Text(60)),
    ("success_message", Text(200)),
    ("success_body", Text(1000)),
    ("discount_code", Text(60)),
    // A coupon's name: each new sign-up gets its own code from it instead of `discount_code`.
    ("coupon", Text(64)),
    ("legal_text", Text(1000)),
    ("success_button_text", Text(60)),
    ("success_button_url", Url),
    ("redirect_url", Url),
    ("auto_close_seconds", Int(0, 60)),
    ("confirm_message", Text(300)),
    ("sms_consent_text", Text(1000)),
    // Wording for an optional box naming WhatsApp under the phone field; ticking it subscribes the
    // number to WhatsApp marketing (see features::whatsapp::consent). No wording, no box.
    ("whatsapp_consent_text", Text(1000)),
    ("brand_color", Color),
    ("delay_seconds", Int(0, 600)),
    ("collect", Nested),
    ("fields", Nested),
    ("phone_step", Nested),
    ("style", Nested),
    ("teaser", Nested),
    ("targeting", Nested),
    ("variants", Nested),
];

const FONTS: &[&str] = &["system", "inherit", "serif", "Inter", "Poppins", "Montserrat", "Lato", "DM Sans", "Space Grotesk", "Playfair Display", "Lora", "Merriweather", "Oswald"];

const STYLE: &[(&str, Rule)] = &[
    ("layout", OneOf(&["stacked", "image_left", "image_right", "background"])),
    ("width", Int(280, 900)),
    ("align", OneOf(&["center", "left"])),
    ("font", OneOf(FONTS)),
    ("headline_size", Int(14, 56)),
    ("headline_weight", IntOneOf(&[400, 500, 600, 700, 800])),
    ("body_size", Int(12, 22)),
    ("background_color", Color),
    ("headline_color", Color),
    ("text_color", Color),
    ("button_color", Color),
    ("button_text_color", Color),
    ("button_full_width", Bool),
    ("input_background", Color),
    ("input_border_color", Color),
    ("input_text_color", Color),
    ("corner_radius", Int(0, 40)),
    ("button_radius", Int(0, 40)),
    ("input_radius", Int(0, 40)),
    ("padding", Int(12, 64)),
    ("border_width", Int(0, 8)),
    ("border_color", Color),
    ("shadow", OneOf(&["none", "soft", "strong"])),
    ("overlay_color", Color),
    ("overlay_opacity", Int(0, 90)),
    ("close_on_overlay", Bool),
    ("close_color", Color),
    // Popup positions and flyout corners; the renderer uses the ones that fit the form's kind.
    ("position", OneOf(&["center", "top", "bottom", "bottom_right", "bottom_left", "top_right", "top_left"])),
    ("animation", OneOf(&["fade", "slide", "zoom", "none"])),
    ("page_background", Color),
    ("custom_css", Css),
];

const TEASER: &[(&str, Rule)] =
    &[("enabled", Bool), ("text", Text(40)), ("position", OneOf(&["bottom_left", "bottom_right", "left", "right"])), ("show", OneOf(&["after_close", "always"]))];

const TARGETING: &[(&str, Rule)] = &[
    ("trigger", OneOf(&["delay", "scroll", "exit_intent", "manual"])),
    ("scroll_percent", Int(1, 100)),
    ("devices", OneOf(&["all", "desktop", "mobile"])),
    ("url_contains", TextList(50, 500)),
    ("url_excludes", TextList(50, 500)),
    ("page_views", Int(1, 50)),
    ("visitors", OneOf(&["all", "new", "returning"])),
    ("frequency_days", Int(0, 365)),
    ("hide_for_subscribers", Bool),
    ("start_at", DateTime),
    ("end_at", DateTime),
];

const PHONE_STEP: &[(&str, Rule)] = &[("enabled", Bool), ("headline", Text(200)), ("body", Text(1000)), ("button_text", Text(60)), ("skip_text", Text(60))];

const FIELD: &[(&str, Rule)] = &[("key", Nested), ("type", Nested), ("label", Text(100)), ("placeholder", Text(100)), ("required", Bool), ("options", Nested)];

fn bad(msg: String) -> AppError {
    AppError::bad(msg)
}

fn names(rules: &[(&str, Rule)]) -> String {
    rules.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
}

/// Edit distance, for "did you mean" hints on misspelled keys.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

fn unknown_key(path: &str, key: &str, rules: &[(&str, Rule)]) -> AppError {
    let hint = if path == "config.targeting" && key == "delay_seconds" {
        " (delay_seconds is a top-level config key, not part of targeting)".to_string()
    } else if path == "config" && rules.iter().all(|(k, _)| *k != key) && (STYLE.iter().any(|(k, _)| *k == key) || TARGETING.iter().any(|(k, _)| *k == key)) {
        let parent = if STYLE.iter().any(|(k, _)| *k == key) { "style" } else { "targeting" };
        format!(" (it goes inside config.{parent})")
    } else {
        rules.iter().map(|(k, _)| (*k, distance(key, k))).filter(|(_, d)| *d <= 2).min_by_key(|(_, d)| *d).map(|(k, _)| format!(" (did you mean {k:?}?)")).unwrap_or_default()
    };
    bad(format!("{path} has no key {key:?}{hint}. Allowed keys: {}. {SEE_DOCS}", names(rules)))
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 40 { format!("{}…", s.chars().take(40).collect::<String>()) } else { s }
}

fn int_of(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 1e15).map(|f| f as i64))
}

fn is_http_url(s: &str) -> bool {
    url::Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
}

fn check_value(path: &str, v: &Value, rule: Rule) -> AppResult<()> {
    let text = |what: &str| -> AppResult<&str> { v.as_str().ok_or_else(|| bad(format!("{path} must be {what} (got {})", short(v)))) };
    match rule {
        Text(max) => {
            let n = text("text")?.chars().count();
            if n > max {
                return Err(bad(format!("{path} is too long: at most {max} characters (got {n})")));
            }
        }
        Url => {
            let s = text("an http(s) URL")?;
            if !s.is_empty() && (!is_http_url(s) || s.len() > 2000) {
                return Err(bad(format!("{path} must be an http(s) URL like https://example.com/page (got {})", short(v))));
            }
        }
        Int(lo, hi) => match int_of(v) {
            Some(n) if (lo..=hi).contains(&n) => {}
            _ => return Err(bad(format!("{path} must be a whole number from {lo} to {hi} (got {})", short(v)))),
        },
        IntOneOf(opts) => {
            if !int_of(v).is_some_and(|n| opts.contains(&n)) {
                return Err(bad(format!("{path} must be one of {} (got {})", opts.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", "), short(v))));
            }
        }
        Color => {
            let s = text("a hex color like #1f6f5c")?;
            if !s.is_empty() && crate::render::safe_color(s, "") != s {
                return Err(bad(format!("{path} must be a hex color like #1f6f5c (got {})", short(v))));
            }
        }
        Bool => {
            if !v.is_boolean() {
                return Err(bad(format!("{path} must be true or false (got {})", short(v))));
            }
        }
        OneOf(opts) => {
            if !v.as_str().is_some_and(|s| opts.contains(&s)) {
                return Err(bad(format!("{path} must be one of: {} (got {})", opts.join(", "), short(v))));
            }
        }
        TextList(max_items, max_len) => {
            let items = v.as_array().ok_or_else(|| bad(format!("{path} must be a list of text (got {})", short(v))))?;
            if items.len() > max_items {
                return Err(bad(format!("{path} can have at most {max_items} entries (got {})", items.len())));
            }
            for (i, it) in items.iter().enumerate() {
                check_value(&format!("{path}[{i}]"), it, Text(max_len))?;
            }
        }
        DateTime => {
            let s = text("an ISO 8601 date-time")?;
            if !s.is_empty() && parse_time(s).is_none() {
                return Err(bad(format!("{path} must be an ISO 8601 date-time with a time zone, like 2026-12-01T09:00:00-08:00 (got {})", short(v))));
            }
        }
        Css => {
            let s = text("text")?;
            let n = s.chars().count();
            if n > 10_000 {
                return Err(bad(format!("{path} is too long: at most 10000 characters (got {n})")));
            }
            if s.contains('<') {
                return Err(bad(format!("{path} can't contain '<' (it's CSS only, no HTML tags)")));
            }
        }
        Nested => {}
    }
    Ok(())
}

/// An object whose keys all follow `rules`. `null` values mean "use the default" and are allowed.
fn check_object(path: &str, v: &Value, rules: &[(&str, Rule)]) -> AppResult<()> {
    let o = v.as_object().ok_or_else(|| bad(format!("{path} must be an object (got {}). {SEE_DOCS}", short(v))))?;
    for (k, val) in o {
        let Some((_, rule)) = rules.iter().find(|(r, _)| r == k) else { return Err(unknown_key(path, k, rules)) };
        if !val.is_null() {
            check_value(&format!("{path}.{k}"), val, *rule)?;
        }
    }
    Ok(())
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim()).ok().map(|t| t.with_timezone(&Utc))
}

/// Check a form config (a whole config on create, or the keys being changed on update).
/// `null` for a top-level key means "reset it to the default".
pub fn check_config(c: &Value) -> AppResult<()> {
    let o = c.as_object().ok_or_else(|| bad(format!("config must be an object. {SEE_DOCS}")))?;
    for (k, v) in o {
        let Some((_, rule)) = TOP.iter().find(|(r, _)| r == k) else { return Err(unknown_key("config", k, TOP)) };
        if v.is_null() {
            continue;
        }
        let path = format!("config.{k}");
        match k.as_str() {
            "collect" => check_collect(v)?,
            "fields" => check_fields(v)?,
            "phone_step" => check_object(&path, v, PHONE_STEP)?,
            "style" => check_object(&path, v, STYLE)?,
            "teaser" => check_object(&path, v, TEASER)?,
            "targeting" => {
                check_object(&path, v, TARGETING)?;
                let at = |k: &str| v.get(k).and_then(|t| t.as_str()).and_then(parse_time);
                if let (Some(s), Some(e)) = (at("start_at"), at("end_at")) {
                    if s >= e {
                        return Err(bad("config.targeting.end_at must be after start_at".into()));
                    }
                }
            }
            "variants" => check_variants(v)?,
            _ => check_value(&path, v, *rule)?,
        }
    }
    Ok(())
}

fn check_collect(v: &Value) -> AppResult<()> {
    const OK: [&str; 3] = ["email", "phone_number", "first_name"];
    let items = v.as_array().ok_or_else(|| bad("config.collect must be a list like [\"email\", \"first_name\"]".into()))?;
    for it in items {
        if !it.as_str().is_some_and(|s| OK.contains(&s)) {
            return Err(bad(format!("config.collect can only hold email, phone_number and first_name (got {}); use config.fields for anything else", short(it))));
        }
    }
    if !items.iter().any(|i| i == "email" || i == "phone_number") {
        return Err(bad("config.collect must include email or phone_number".into()));
    }
    Ok(())
}

fn valid_field_key(k: &str) -> bool {
    let mut cs = k.chars();
    cs.next().is_some_and(|c| c.is_ascii_lowercase()) && k.len() <= 40 && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn check_fields(v: &Value) -> AppResult<()> {
    let items = v.as_array().ok_or_else(|| bad(format!("config.fields must be a list of field objects like [{{\"key\": \"email\"}}]. {SEE_DOCS}")))?;
    if items.len() > MAX_FIELDS {
        return Err(bad(format!("config.fields can have at most {MAX_FIELDS} fields (got {})", items.len())));
    }
    let mut seen: Vec<&str> = Vec::new();
    for (i, f) in items.iter().enumerate() {
        let path = format!("config.fields[{i}]");
        check_object(&path, f, FIELD)?;
        let key = f.get("key").and_then(|k| k.as_str()).ok_or_else(|| bad(format!("{path}.key is required: email, phone_number, first_name, last_name, birthday or a custom key like skin_type")))?;
        if !valid_field_key(key) {
            return Err(bad(format!("{path}.key {key:?} must be lowercase letters, digits and _, start with a letter and be at most 40 characters")));
        }
        if RESERVED_FIELDS.contains(&key) {
            return Err(bad(format!("{path}.key {key:?} is reserved; pick another name for this field")));
        }
        if seen.contains(&key) {
            return Err(bad(format!("config.fields has {key:?} twice; each field key can appear once")));
        }
        seen.push(key);
        let standard = STANDARD_FIELDS.contains(&key);
        let kind = match f.get("type").filter(|t| !t.is_null()) {
            None => "text",
            Some(_) if standard => return Err(bad(format!("{path}.type can only be set on custom fields; {key} has a fixed input type"))),
            Some(t) => match t.as_str().filter(|t| FIELD_TYPES.contains(t)) {
                Some(t) => t,
                None => return Err(bad(format!("{path}.type must be one of: {} (got {})", FIELD_TYPES.join(", "), short(t)))),
            },
        };
        let choice = matches!(kind, "select" | "radio" | "checkboxes");
        match f.get("options").filter(|o| !o.is_null()) {
            None if choice => return Err(bad(format!("{path}.options is required for a {kind} field: a list of 1-20 choices like [\"Dry\", \"Oily\"]"))),
            None => {}
            Some(_) if !choice => return Err(bad(format!("{path}.options only applies to select, radio and checkboxes fields"))),
            Some(o) => {
                let opts = o.as_array().filter(|a| (1..=20).contains(&a.len())).ok_or_else(|| bad(format!("{path}.options must be a list of 1-20 choices")))?;
                let mut seen_opts: Vec<&str> = Vec::new();
                for (j, opt) in opts.iter().enumerate() {
                    let s = opt.as_str().filter(|s| !s.trim().is_empty()).ok_or_else(|| bad(format!("{path}.options[{j}] must be non-empty text")))?;
                    if s.chars().count() > 80 {
                        return Err(bad(format!("{path}.options[{j}] is too long: at most 80 characters")));
                    }
                    if seen_opts.contains(&s) {
                        return Err(bad(format!("{path}.options has {s:?} twice")));
                    }
                    seen_opts.push(s);
                }
            }
        }
    }
    if !seen.iter().any(|k| *k == "email" || *k == "phone_number") {
        return Err(bad("config.fields must include an email or phone_number field, so sign-ups have a way to be reached".into()));
    }
    Ok(())
}

fn check_variants(v: &Value) -> AppResult<()> {
    let vs = v.as_array().ok_or_else(|| bad("config.variants must be a list like [{\"id\": \"b\", \"headline\": \"…\"}]".into()))?;
    if vs.len() > 3 {
        return Err(bad("a form A/B test can have at most 3 extra versions (B, C and D)".into()));
    }
    for (i, var) in vs.iter().enumerate() {
        let want = crate::campaigns::VERSION_IDS[i + 1];
        let o = var.as_object().filter(|o| o.get("id").and_then(|x| x.as_str()) == Some(want));
        let Some(o) = o else { return Err(bad(format!("form variant {} must be an object with id {want:?} (versions go b, c, d in order)", i + 1))) };
        for (k, val) in o {
            if k == "id" {
                continue;
            }
            if !VARIANT_FIELDS.contains(&k.as_str()) {
                return Err(bad(format!(
                    "form variants can change only {}, not {k:?} (it's shared by every version; set it on the form's own config)",
                    VARIANT_FIELDS.join(", ")
                )));
            }
            if !(val.is_string() || val.is_null()) {
                return Err(bad(format!("form variant {want} {k} must be text")));
            }
            if let Some((_, rule)) = TOP.iter().find(|(r, _)| r == k).filter(|_| val.is_string()) {
                check_value(&format!("config.variants[{i}].{k}"), val, *rule)?;
            }
        }
    }
    Ok(())
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, strip_nulls(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

/// Merge a (checked) config patch one level deep: each key replaces the stored one, `null` removes it.
/// With `fields` set, `collect` follows it so older readers see the same standard fields.
pub fn apply(config: &mut Value, patch: Value) {
    if !config.is_object() {
        *config = json!({});
    }
    let Some(o) = config.as_object_mut() else { return };
    if let Value::Object(p) = patch {
        for (k, v) in p {
            if v.is_null() {
                o.remove(&k);
            } else {
                o.insert(k, strip_nulls(v));
            }
        }
    }
    let synced: Option<Vec<Value>> = o.get("fields").and_then(|f| f.as_array()).map(|fs| {
        fs.iter().filter_map(|f| f["key"].as_str()).filter(|k| matches!(*k, "email" | "phone_number" | "first_name")).map(|k| json!(k)).collect()
    });
    if let Some(c) = synced {
        o.insert("collect".into(), Value::Array(c));
    }
}

/// Whether the form's schedule (`targeting.start_at` / `end_at`) has it open at `now`.
/// A form's config as visitors may see it before signing up: the discount code (base and every
/// A/B version) is left out, so it's only handed over in a successful sign-up's response.
pub fn public_config(config: &Value) -> Value {
    let mut c = config.clone();
    if let Some(o) = c.as_object_mut() {
        o.remove("discount_code");
        if let Some(Value::Array(vs)) = o.get_mut("variants") {
            for v in vs.iter_mut().filter_map(|v| v.as_object_mut()) {
                v.remove("discount_code");
            }
        }
    }
    c
}

/// The discount code a visitor who saw `version` gets after signing up, if the form has one.
pub fn discount_code(config: &Value, version: Option<&str>) -> Option<String> {
    let c = match version {
        Some(v) => crate::api::messaging::form_version_config(config, v),
        None => config.clone(),
    };
    c["discount_code"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

pub fn is_open(config: &Value, now: DateTime<Utc>) -> bool {
    let at = |k: &str| config["targeting"][k].as_str().and_then(parse_time);
    !(at("start_at").is_some_and(|s| now < s) || at("end_at").is_some_and(|e| now >= e))
}

fn preview_payload(form_id: Uuid) -> String {
    format!("fpreview:{}", form_id.simple())
}

/// The token in a form's `preview_url`: shows drafts and counts nothing.
pub fn preview_token(secret: &str, form_id: Uuid) -> String {
    crate::auth::sign(secret, &preview_payload(form_id))
}

pub fn preview_ok(secret: &str, form_id: Uuid, token: &str) -> bool {
    crate::auth::verify(secret, token).is_some_and(|p| p == preview_payload(form_id))
}

/// One question on the form, as the server checks it.
#[derive(Debug, Clone)]
pub struct FieldDef {
    pub key: String,
    pub kind: String,
    pub label: Option<String>,
    pub placeholder: Option<String>,
    pub required: bool,
    pub options: Vec<String>,
}

impl FieldDef {
    fn name(&self) -> String {
        if let Some(l) = self.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            return l.to_string();
        }
        let k = self.key.replace('_', " ");
        let mut cs = k.chars();
        cs.next().map(|c| c.to_uppercase().collect::<String>() + cs.as_str()).unwrap_or_default()
    }
}

/// The form's fields: `fields` when set, otherwise the older `collect` list (email only by default).
pub fn field_defs(config: &Value) -> Vec<FieldDef> {
    if let Some(fs) = config["fields"].as_array().filter(|a| !a.is_empty()) {
        return fs
            .iter()
            .filter_map(|f| {
                let key = f["key"].as_str()?.to_string();
                let kind = if key == "birthday" { "date".to_string() } else { f["type"].as_str().unwrap_or("text").to_string() };
                Some(FieldDef {
                    required: f["required"].as_bool().unwrap_or(key == "email"),
                    label: f["label"].as_str().map(String::from),
                    placeholder: f["placeholder"].as_str().map(String::from),
                    // Trimmed, like the answers they are compared with.
                    options: f["options"].as_array().map(|o| o.iter().filter_map(|s| s.as_str().map(|s| s.trim().to_string())).collect()).unwrap_or_default(),
                    key,
                    kind,
                })
            })
            .collect();
    }
    let collect: Vec<String> = config["collect"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_else(|| vec!["email".into()]);
    // The older forms always showed first name, then email, then phone.
    ["first_name", "email", "phone_number"]
        .iter()
        .filter(|k| collect.iter().any(|c| c == *k))
        .map(|k| FieldDef { key: k.to_string(), kind: "text".into(), label: None, placeholder: None, required: *k == "email", options: Vec::new() })
        .collect()
}

/// What a visitor's sign-up saves, after checking it against the form's fields.
#[derive(Debug, Default)]
pub struct Answers {
    pub email: Option<String>,
    pub phone_number: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    /// Birthday and custom fields, saved as profile properties.
    pub properties: Map<String, Value>,
}

fn blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn as_date(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| d.format("%Y-%m-%d").to_string())
}

/// Check answers against the form's fields. `given` holds every submitted value by field key; answers to
/// fields the form doesn't have are ignored. `required` is enforced here, not only in the browser.
pub fn answers(defs: &[FieldDef], given: &Map<String, Value>) -> AppResult<Answers> {
    let mut out = Answers::default();
    for d in defs {
        let v = given.get(&d.key).cloned().unwrap_or(Value::Null);
        let name = d.name();
        let unticked = d.kind == "checkbox" && !v.as_bool().unwrap_or(false) && !v.as_str().is_some_and(|s| matches!(s, "true" | "on" | "1" | "yes"));
        if blank(&v) || (d.required && unticked) {
            if d.required {
                return Err(match d.key.as_str() {
                    "email" => AppError::bad("enter your email"),
                    "phone_number" => AppError::bad("enter your mobile number"),
                    _ if d.kind == "checkbox" => AppError::bad(format!("tick \"{name}\" to continue")),
                    _ if matches!(d.kind.as_str(), "select" | "radio" | "checkboxes") => AppError::bad(format!("choose an answer for \"{name}\"")),
                    _ => AppError::bad(format!("{name} is required")),
                });
            }
            continue;
        }
        let text = || as_text(&v).ok_or_else(|| AppError::bad(format!("{name} must be text")));
        let too_long = |s: &str, max: usize| -> AppResult<()> {
            if s.chars().count() > max { Err(AppError::bad(format!("{name} is too long (at most {max} characters)"))) } else { Ok(()) }
        };
        match d.key.as_str() {
            "email" => {
                let e = text()?;
                crate::models::normalize_email(&e).ok_or_else(|| AppError::bad("enter a valid email address"))?;
                out.email = Some(e);
            }
            "phone_number" => {
                let p = text()?;
                crate::models::normalize_phone(&p).ok_or_else(|| AppError::bad("enter a valid mobile number"))?;
                out.phone_number = Some(p);
            }
            "first_name" | "last_name" => {
                let n = text()?;
                too_long(&n, 100)?;
                if d.key == "first_name" { out.first_name = Some(n) } else { out.last_name = Some(n) }
            }
            key => {
                let saved = match d.kind.as_str() {
                    "number" => {
                        let n = v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok())).filter(|n| n.is_finite());
                        let n = n.ok_or_else(|| AppError::bad(format!("{name} must be a number")))?;
                        if n.fract() == 0.0 && n.abs() < 9e15 { json!(n as i64) } else { json!(n) }
                    }
                    "date" => json!(as_date(&v).ok_or_else(|| AppError::bad(format!("{name} must be a date like 1990-04-21")))?),
                    "select" | "radio" => {
                        let s = text()?;
                        if !d.options.contains(&s) {
                            return Err(AppError::bad(format!("{name} must be one of: {}", d.options.join(", "))));
                        }
                        json!(s)
                    }
                    "checkboxes" => {
                        let picked: Vec<String> = match &v {
                            Value::Array(a) => a.iter().map(|x| as_text(x).ok_or_else(|| AppError::bad(format!("{name} must be a list of choices")))).collect::<AppResult<_>>()?,
                            other => vec![as_text(other).ok_or_else(|| AppError::bad(format!("{name} must be a list of choices")))?],
                        };
                        let picked: Vec<String> = picked.iter().map(|p| p.trim().to_string()).collect();
                        if let Some(x) = picked.iter().find(|p| !d.options.contains(p)) {
                            return Err(AppError::bad(format!("{x:?} isn't a choice for {name}; pick from: {}", d.options.join(", "))));
                        }
                        // Saved in the form's own order, each choice once.
                        json!(d.options.iter().filter(|o| picked.contains(o)).collect::<Vec<_>>())
                    }
                    "checkbox" => json!(!unticked),
                    _ => {
                        let s = text()?;
                        too_long(&s, MAX_ANSWER)?;
                        json!(s)
                    }
                };
                out.properties.insert(key.to_string(), saved);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_config() {
        assert!(check_config(&json!({"headline": "Hi", "style": {"width": 500, "font": "Lora", "button_color": "#000"}, "teaser": {"enabled": true}})).is_ok());
        let err = |c: Value| check_config(&c).unwrap_err().public_message();
        assert!(err(json!({"headlin": "x"})).contains("did you mean \"headline\""));
        assert!(err(json!({"width": 400})).contains("config.style"));
        assert!(err(json!({"targeting": {"delay_seconds": 3}})).contains("top-level"));
        assert!(err(json!({"style": {"width": 1200}})).contains("280 to 900"));
        assert!(err(json!({"style": {"custom_css": "</style><script>"}})).contains("'<'"));
        assert!(err(json!({"targeting": {"start_at": "2026-12-02T00:00:00Z", "end_at": "2026-12-01T00:00:00Z"}})).contains("after"));
        assert!(err(json!({"fields": [{"key": "first_name"}]})).contains("email or phone_number"));
        assert!(err(json!({"fields": [{"key": "email", "type": "text"}]})).contains("custom fields"));
        assert!(err(json!({"fields": [{"key": "email"}, {"key": "sms_consent"}]})).contains("reserved"));
        assert!(err(json!({"fields": [{"key": "email"}, {"key": "Bad Key"}]})).contains("lowercase"));
    }

    #[test]
    fn merges_and_syncs_collect() {
        let mut c = json!({"headline": "A", "collect": ["email"], "style": {"width": 400}});
        apply(&mut c, json!({"headline": null, "style": {"align": "left", "font": null}, "fields": [{"key": "first_name"}, {"key": "email"}, {"key": "skin", "label": null}]}));
        assert_eq!(c, json!({"collect": ["first_name", "email"], "style": {"align": "left"}, "fields": [{"key": "first_name"}, {"key": "email"}, {"key": "skin"}]}));
    }

    #[test]
    fn schedule() {
        let now = Utc::now();
        let iso = |d: chrono::Duration| (now + d).to_rfc3339();
        assert!(is_open(&json!({}), now));
        assert!(!is_open(&json!({"targeting": {"start_at": iso(chrono::Duration::hours(1))}}), now));
        assert!(!is_open(&json!({"targeting": {"end_at": iso(chrono::Duration::hours(-1))}}), now));
        assert!(is_open(&json!({"targeting": {"start_at": iso(chrono::Duration::hours(-1)), "end_at": iso(chrono::Duration::hours(1))}}), now));
    }
}
