//! The actions a translation script returns: parsing, validation and writing them.
//!
//! Every action is validated before anything is written, so a bad action never leaves a delivery
//! half-applied. Writes go through the same functions as the REST API (`audience::upsert_profile`,
//! `record_event`, `add_to_list`), always scoped to the integration's account.

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::audience::{self, EventSource, NewEvent, ProfileInput};
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, normalize_phone, Profile, PROFILE_COLS};
use crate::state::AppState;

/// Most actions one run may return.
pub const MAX_ACTIONS: usize = 500;
/// Largest JSON for one action's properties.
const MAX_PROPS_BYTES: usize = 64 * 1024;
/// Largest saved state.
pub const MAX_STATE_BYTES: usize = 64 * 1024;

/// Email domains booking sites use to relay messages to guests. Their terms forbid marketing to
/// these addresses, so profiles an integration writes with one are suppressed from email. Matches
/// the domain itself and any subdomain. Add more here.
pub const RELAY_DOMAINS: &[&str] = &[
    "guest.airbnb.com",
    "airbnb.com",
    "guest.booking.com",
    "messages.homeaway.com",
    "messages.vrbo.com",
    "reply.homeaway.com",
    "m.expediapartnercentral.com",
    "expediapartnercentral.com",
    "agoda-messaging.com",
    "guest.trip.com",
    "relay.hostaway.com",
    "guest.plumguide.com",
];

/// Suppression reason for a booking-site relay address.
pub const REASON_RELAY: &str = "booking_relay";
/// Suppression reason when an integration says an address is not for marketing.
pub const REASON_NO_MARKETING: &str = "no_marketing";

pub fn is_relay_address(email: &str) -> bool {
    let Some((_, domain)) = email.trim().to_ascii_lowercase().rsplit_once('@').map(|(l, d)| (l.to_string(), d.to_string())) else { return false };
    RELAY_DOMAINS.iter().any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Email,
    Sms,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Consent {
    pub channel: Channel,
    pub subscribed: bool,
    /// Where the opt-in (or opt-out) happened, e.g. "Stripe Checkout marketing checkbox".
    pub source: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertProfile {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, alias = "phone")]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub properties: Option<Map<String, Value>>,
    #[serde(default)]
    pub consent: Vec<Consent>,
    /// `false`: never send marketing email to this address.
    #[serde(default)]
    pub marketing: Option<bool>,
    /// Ids that identify this person in the app and can be many (payment ids, booking ids): each
    /// value is added to the person's list for that key (the newest 100 are kept), so a later
    /// action finds them with `profile: {property: <key>, value: <id>}`. Not shown as properties.
    #[serde(default)]
    pub keys: Option<Map<String, Value>>,
}

/// Most values kept per key in an `upsert_profile`'s `keys`.
pub const MAX_KEY_VALUES: usize = 100;

/// Who an action is about: identifiers (the profile is created if missing) or a lookup by a custom
/// property (never creates; the action is skipped when nobody matches).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Who {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, alias = "phone")]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub property: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
    /// With property + value: `skip` (default) when nobody matches, or `retry` to fail the
    /// delivery so it runs again later (e.g. the order that names this customer hasn't arrived yet).
    #[serde(default)]
    pub if_missing: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrackEvent {
    pub metric: String,
    pub profile: Who,
    #[serde(default)]
    pub properties: Option<Map<String, Value>>,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub unique_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListAction {
    #[serde(default)]
    pub list_id: Option<Uuid>,
    #[serde(default)]
    pub list: Option<String>,
    pub profile: Who,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Unsubscribe {
    pub channel: Channel,
    pub profile: Who,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Suppress {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, alias = "phone")]
    pub phone_number: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetState {
    pub values: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Log {
    pub message: String,
}

/// A person an object belongs to: who (as in other actions) and how (`guest`, `owner` ...).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectWho {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, alias = "phone")]
    pub phone_number: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub property: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub if_missing: Option<String>,
    #[serde(default)]
    pub relationship: Option<String>,
}

impl ObjectWho {
    fn who(&self) -> Who {
        Who {
            email: self.email.clone(),
            phone_number: self.phone_number.clone(),
            external_id: self.external_id.clone(),
            property: self.property.clone(),
            value: self.value.clone(),
            if_missing: self.if_missing.clone(),
        }
    }
}

/// Create or update a custom object (see `features::objects`), linked to the people given.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertObject {
    pub object_type: String,
    /// Its id in the app. Missing: one is made from the delivery (stable across retries).
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub fields: Option<Map<String, Value>>,
    #[serde(default)]
    pub profiles: Vec<ObjectWho>,
    /// The app's own time or version for this state: an older one never overwrites a newer one.
    #[serde(default)]
    pub as_of: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteObject {
    pub object_type: String,
    pub external_id: String,
}

/// Pull runs only: fetch another page. Fields given replace the previous request's; `query` merges.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NextRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

#[derive(Debug, Clone)]
pub enum Action {
    UpsertProfile(UpsertProfile),
    TrackEvent(TrackEvent),
    AddToList(ListAction),
    RemoveFromList(ListAction),
    Unsubscribe(Unsubscribe),
    Suppress(Suppress),
    SetState(SetState),
    Log(Log),
    NextRequest(NextRequest),
    UpsertObject(UpsertObject),
    DeleteObject(DeleteObject),
}

pub const ACTION_TYPES: &[&str] = &["upsert_profile", "track_event", "add_to_list", "remove_from_list", "unsubscribe", "suppress", "set_state", "log", "next_request", "upsert_object", "delete_object"];

fn part<T: DeserializeOwned>(i: usize, kind: &str, v: Map<String, Value>) -> Result<T, String> {
    serde_json::from_value(Value::Object(v)).map_err(|e| format!("action {i} ({kind}): {e}"))
}

fn short(i: usize, kind: &str, field: &str, s: &Option<String>, max: usize) -> Result<(), String> {
    match s {
        Some(v) if v.chars().count() > max => Err(format!("action {i} ({kind}): `{field}` is longer than {max} characters")),
        _ => Ok(()),
    }
}

fn check_email(i: usize, kind: &str, e: &Option<String>) -> Result<(), String> {
    match e.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(s) if normalize_email(s).is_none() => Err(format!("action {i} ({kind}): {s:?} is not a valid email")),
        _ => Ok(()),
    }
}

fn check_phone(i: usize, kind: &str, p: &Option<String>) -> Result<(), String> {
    match p.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(s) if normalize_phone(s).is_none() => Err(format!("action {i} ({kind}): {s:?} is not a valid phone number (use +15035550100)")),
        _ => Ok(()),
    }
}

fn check_props(i: usize, kind: &str, p: &Option<Map<String, Value>>) -> Result<(), String> {
    if let Some(p) = p {
        if p.len() > 200 {
            return Err(format!("action {i} ({kind}): at most 200 properties"));
        }
        if let Some(k) = p.keys().find(|k| k.is_empty() || k.len() > 100) {
            return Err(format!("action {i} ({kind}): property name {k:?} must be 1-100 characters"));
        }
        if serde_json::to_string(p).map(|s| s.len()).unwrap_or(0) > MAX_PROPS_BYTES {
            return Err(format!("action {i} ({kind}): properties are over 64 KB"));
        }
    }
    Ok(())
}

fn check_who(i: usize, kind: &str, w: &Who) -> Result<(), String> {
    check_email(i, kind, &w.email)?;
    check_phone(i, kind, &w.phone_number)?;
    short(i, kind, "external_id", &w.external_id, 200)?;
    let ids = [&w.email, &w.phone_number, &w.external_id].iter().any(|v| v.as_deref().is_some_and(|s| !s.trim().is_empty()));
    if let Some(m) = &w.if_missing {
        if !matches!(m.as_str(), "skip" | "retry") || w.property.is_none() {
            return Err(format!("action {i} ({kind}): profile.if_missing is skip or retry, and goes with property + value"));
        }
    }
    match (&w.property, ids) {
        (Some(p), false) => {
            if p.is_empty() || p.len() > 100 {
                return Err(format!("action {i} ({kind}): profile.property must be 1-100 characters"));
            }
            if w.value.as_ref().is_none_or(|v| v.is_null() || v.is_object() || v.is_array()) {
                return Err(format!("action {i} ({kind}): profile.property needs a plain `value` to match"));
            }
            Ok(())
        }
        (Some(_), true) => Err(format!("action {i} ({kind}): give the profile's email/phone_number/external_id, or property + value, not both")),
        (None, true) if w.value.is_none() => Ok(()),
        (None, true) => Err(format!("action {i} ({kind}): profile.value only goes with profile.property")),
        (None, false) => Err(format!("action {i} ({kind}): `profile` needs an email, phone_number or external_id (or property + value)")),
    }
}

/// Parse and validate a script's result. `pull` allows `next_request`.
pub fn parse(result: &Value, pull: bool) -> Result<Vec<Action>, String> {
    let items = match result {
        Value::Null => return Ok(Vec::new()),
        Value::Array(a) => a,
        _ => return Err("handle() must return a list of actions (an array)".into()),
    };
    if items.len() > MAX_ACTIONS {
        return Err(format!("the script returned {} actions; the limit is {MAX_ACTIONS}", items.len()));
    }
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let Value::Object(obj) = item else { return Err(format!("action {i}: each action must be an object with a `type`")) };
        let mut obj = obj.clone();
        let kind = match obj.remove("type") {
            Some(Value::String(s)) => s,
            _ => return Err(format!("action {i}: missing `type` (one of {})", ACTION_TYPES.join(", "))),
        };
        let k = kind.as_str();
        let a = match k {
            "upsert_profile" => {
                let a: UpsertProfile = part(i, k, obj)?;
                check_email(i, k, &a.email)?;
                check_phone(i, k, &a.phone_number)?;
                for (f, v, max) in [("external_id", &a.external_id, 200), ("first_name", &a.first_name, 200), ("last_name", &a.last_name, 200)] {
                    short(i, k, f, v, max)?;
                }
                check_props(i, k, &a.properties)?;
                if [&a.email, &a.phone_number, &a.external_id].iter().all(|v| v.as_deref().is_none_or(|s| s.trim().is_empty())) {
                    return Err(format!("action {i} ({k}): needs an email, phone_number or external_id"));
                }
                for c in &a.consent {
                    if c.source.trim().is_empty() || c.source.len() > 200 {
                        return Err(format!("action {i} ({k}): each consent needs a `source` (1-200 characters) saying where they opted in"));
                    }
                    if c.subscribed && c.channel == Channel::Email && a.email.is_none() {
                        return Err(format!("action {i} ({k}): email consent needs an email"));
                    }
                    if c.subscribed && c.channel == Channel::Sms && a.phone_number.is_none() {
                        return Err(format!("action {i} ({k}): SMS consent needs a phone_number"));
                    }
                    if c.subscribed && c.channel == Channel::Email && a.marketing == Some(false) {
                        return Err(format!("action {i} ({k}): marketing: false contradicts an email subscription"));
                    }
                }
                if a.consent.len() > 2 || (a.consent.len() == 2 && a.consent[0].channel == a.consent[1].channel) {
                    return Err(format!("action {i} ({k}): at most one consent per channel"));
                }
                if a.marketing == Some(true) {
                    return Err(format!("action {i} ({k}): marketing only takes false; to subscribe someone, add a consent block"));
                }
                if let Some(keys) = &a.keys {
                    if keys.len() > 20 {
                        return Err(format!("action {i} ({k}): at most 20 keys"));
                    }
                    for (name, v) in keys {
                        let ok = v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 200);
                        if name.is_empty() || name.len() > 100 || !ok {
                            return Err(format!("action {i} ({k}): each key needs a name (1-100 characters) and a text value (1-200 characters)"));
                        }
                    }
                }
                Action::UpsertProfile(a)
            }
            "track_event" => {
                let a: TrackEvent = part(i, k, obj)?;
                if a.metric.trim().is_empty() || a.metric.len() > 128 {
                    return Err(format!("action {i} ({k}): metric must be 1-128 characters"));
                }
                // Alerts DumbMail sends from a real stock or price change: nobody else records them.
                if audience::is_server_only_metric(&a.metric) {
                    return Err(format!("action {i} ({k}): {:?} is recorded by DumbMail itself; an integration can't record it", a.metric.trim()));
                }
                check_who(i, k, &a.profile)?;
                check_props(i, k, &a.properties)?;
                short(i, k, "unique_id", &a.unique_id, 200)?;
                if a.value.is_some_and(|v| !v.is_finite()) {
                    return Err(format!("action {i} ({k}): value must be a number"));
                }
                if let Some(t) = &a.time {
                    parse_time(t).ok_or_else(|| format!("action {i} ({k}): time {t:?} isn't an ISO 8601 date-time"))?;
                }
                Action::TrackEvent(a)
            }
            "add_to_list" | "remove_from_list" => {
                let a: ListAction = part(i, k, obj)?;
                match (&a.list_id, &a.list) {
                    (Some(_), None) => {}
                    (None, Some(n)) if !n.trim().is_empty() && n.len() <= 200 => {}
                    _ => return Err(format!("action {i} ({k}): give exactly one of `list_id` or `list` (the list's exact name)")),
                }
                check_who(i, k, &a.profile)?;
                if k == "add_to_list" { Action::AddToList(a) } else { Action::RemoveFromList(a) }
            }
            "unsubscribe" => {
                let a: Unsubscribe = part(i, k, obj)?;
                check_who(i, k, &a.profile)?;
                Action::Unsubscribe(a)
            }
            "suppress" => {
                let a: Suppress = part(i, k, obj)?;
                check_email(i, k, &a.email)?;
                check_phone(i, k, &a.phone_number)?;
                if a.email.is_none() && a.phone_number.is_none() {
                    return Err(format!("action {i} ({k}): needs an email or phone_number"));
                }
                Action::Suppress(a)
            }
            "upsert_object" => {
                let a: UpsertObject = part(i, k, obj)?;
                if !crate::features::objects::types::valid_type_key(a.object_type.trim()) {
                    return Err(format!("action {i} ({k}): object_type is the type's key, 1-40 lower-case letters, digits and _ (e.g. reservation)"));
                }
                if a.external_id.as_deref().is_some_and(|e| e.trim().is_empty() || e.chars().count() > 200) {
                    return Err(format!("action {i} ({k}): external_id must be 1-200 characters"));
                }
                crate::features::objects::store::check_fields_shape(&a.fields.clone().unwrap_or_default()).map_err(|e| format!("action {i} ({k}): {}", e.public_message()))?;
                if a.profiles.len() > crate::features::objects::store::MAX_LINKS {
                    return Err(format!("action {i} ({k}): at most {} profiles", crate::features::objects::store::MAX_LINKS));
                }
                for w in &a.profiles {
                    check_who(i, k, &w.who())?;
                    crate::features::objects::store::relationship(w.relationship.as_deref()).map_err(|e| format!("action {i} ({k}): {}", e.public_message()))?;
                }
                crate::features::objects::store::as_of_text(&a.as_of).map_err(|e| format!("action {i} ({k}): {}", e.public_message()))?;
                Action::UpsertObject(a)
            }
            "delete_object" => {
                let a: DeleteObject = part(i, k, obj)?;
                if !crate::features::objects::types::valid_type_key(a.object_type.trim()) || a.external_id.trim().is_empty() || a.external_id.chars().count() > 200 {
                    return Err(format!("action {i} ({k}): needs object_type (the type's key) and external_id (1-200 characters)"));
                }
                Action::DeleteObject(a)
            }
            "set_state" => {
                let a: SetState = part(i, k, obj)?;
                if serde_json::to_string(&a.values).map(|s| s.len()).unwrap_or(0) > MAX_STATE_BYTES {
                    return Err(format!("action {i} ({k}): state is over 64 KB"));
                }
                Action::SetState(a)
            }
            "log" => {
                let mut a: Log = part(i, k, obj)?;
                a.message = a.message.chars().take(1000).collect();
                Action::Log(a)
            }
            "next_request" => {
                if !pull {
                    return Err(format!("action {i} (next_request): only pulls can fetch another page"));
                }
                let a: NextRequest = part(i, k, obj)?;
                if let Some(m) = &a.method {
                    if !matches!(m.to_ascii_uppercase().as_str(), "GET" | "POST") {
                        return Err(format!("action {i} (next_request): method must be GET or POST"));
                    }
                }
                Action::NextRequest(a)
            }
            other => return Err(format!("action {i}: unknown type {other:?} (use one of {})", ACTION_TYPES.join(", "))),
        };
        out.push(a);
    }
    if out.iter().filter(|a| matches!(a, Action::NextRequest(_))).count() > 1 {
        return Err("return at most one next_request".into());
    }
    Ok(out)
}

pub fn parse_time(t: &str) -> Option<DateTime<Utc>> {
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return Some(d.with_timezone(&Utc));
    }
    if let Ok(d) = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(d.and_utc());
    }
    chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)).map(|d| d.and_utc())
}

/// The integration the writes are for.
#[derive(Debug, Clone)]
pub struct Source {
    pub integration_id: Uuid,
    pub account_id: Uuid,
    pub name: String,
    /// The owner confirmed everyone this integration adds agreed to marketing email.
    pub contacts_opted_in: bool,
    /// The delivery this run belongs to (the original one, for replays): events without a
    /// `unique_id` get one from it, so one delivery never records the same event twice.
    pub delivery: Option<Uuid>,
    /// The metrics this integration declared: the only ones its script may record.
    pub metrics: Vec<String>,
    /// The delivery whose ledger (`applied`) records each action as it is written.
    pub ledger: Option<Uuid>,
    /// The integration's redact list: also removed from object fields before they're written.
    pub redact: Vec<String>,
}

impl Source {
    fn tag(&self) -> String {
        format!("integration:{}", self.integration_id)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Counts {
    pub profiles_created: u32,
    pub profiles_updated: u32,
    pub events: u32,
    pub duplicate_events: u32,
    pub list_adds: u32,
    pub list_removes: u32,
    pub unsubscribes: u32,
    pub suppressed: u32,
    pub consents: u32,
    pub skipped: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub objects_upserted: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub objects_deleted: u32,
    /// New profiles held back from marketing (no opt-in).
    pub held: u32,
    /// Actions skipped on a retry or replay because they were already written.
    pub already_written: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Counts {
    pub fn wrote_anything(&self) -> bool {
        self.profiles_created + self.profiles_updated + self.events + self.list_adds + self.list_removes + self.unsubscribes + self.suppressed + self.consents + self.objects_upserted + self.objects_deleted > 0
    }
}

/// What a run did, or (in a dry run) would do.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Outcome {
    pub counts: Counts,
    /// One line per action, in order.
    pub effects: Vec<String>,
    pub logs: Vec<String>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub state: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_request: Option<NextRequest>,
    /// Actions written, by index, with a hash of each (saved with the delivery).
    #[serde(skip)]
    pub applied: Map<String, Value>,
}

/// A stable fingerprint of an action, leaving out its `time` (scripts often default it to now).
pub fn fingerprint(raw: &Value) -> String {
    let mut v = raw.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("time");
    }
    crate::auth::sha256_hex(&v.to_string())[..16].to_string()
}

const MAX_EFFECT_LINES: usize = 200;

impl Outcome {
    fn effect(&mut self, i: usize, line: String) {
        if self.effects.len() < MAX_EFFECT_LINES {
            self.effects.push(format!("{i}: {line}"));
        } else if self.effects.len() == MAX_EFFECT_LINES {
            self.effects.push("… more not shown".into());
        }
    }
}

fn who_label(w: &Who) -> String {
    w.email.clone().or(w.phone_number.clone()).or(w.external_id.clone().map(|e| format!("external id {e}"))).unwrap_or_else(|| {
        format!("{} = {}", w.property.as_deref().unwrap_or("?"), w.value.as_ref().map(|v| v.to_string()).unwrap_or_default())
    })
}

/// A profile the integration found or made.
struct Found {
    id: Uuid,
    created: bool,
}

fn input_for(w: &Who) -> ProfileInput {
    ProfileInput {
        email: w.email.clone().filter(|s| !s.trim().is_empty()),
        phone_number: w.phone_number.clone().filter(|s| !s.trim().is_empty()),
        external_id: w.external_id.clone().filter(|s| !s.trim().is_empty()),
        ..Default::default()
    }
}

/// Find the one profile an integration gave this property value. Only values integrations wrote
/// count (not ordinary properties, which a browser can set too), and two profiles with the same
/// value match neither.
async fn find_by_property(state: &AppState, account_id: Uuid, prop: &str, value: &Value) -> AppResult<Lookup> {
    let one = json!({prop: key_text(value)});
    let listed = json!({prop: [key_text(value)]});
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND integration_keys <> '{}' AND (integration_keys @> $2 OR integration_keys @> $3) LIMIT 2")
        .bind(account_id)
        .bind(one)
        .bind(listed)
        .fetch_all(&state.db)
        .await?;
    Ok(match ids.as_slice() {
        [one] => Lookup::Found(*one),
        [] => Lookup::Missing,
        _ => Lookup::Ambiguous,
    })
}

enum Lookup {
    Found(Uuid),
    Missing,
    Ambiguous,
}

fn key_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The plain property values an integration writes, kept so later lookups by them can trust them.
fn trusted_keys(props: &Map<String, Value>) -> Map<String, Value> {
    props
        .iter()
        .filter(|(_, v)| matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)))
        .map(|(k, v)| (k.clone(), Value::String(key_text(v))))
        .filter(|(_, v)| v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 200))
        .collect()
}

/// Add an action's `keys` to a profile's integration keys: each key holds a list of values (the
/// newest [`MAX_KEY_VALUES`]); a plain value already there becomes the list's first.
fn merge_keys(current: &Map<String, Value>, add: &Map<String, Value>) -> Map<String, Value> {
    let mut out = current.clone();
    for (k, v) in add {
        let Some(v) = v.as_str() else { continue };
        let mut list: Vec<Value> = match out.get(k) {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::String(s)) => vec![json!(s)],
            _ => Vec::new(),
        };
        list.retain(|x| x.as_str() != Some(v));
        list.push(json!(v));
        let skip = list.len().saturating_sub(MAX_KEY_VALUES);
        out.insert(k.clone(), Value::Array(list.into_iter().skip(skip).collect()));
    }
    out
}

/// Add an action's `keys` to an existing profile (under a row lock, so two deliveries can't lose
/// each other's values).
async fn add_keys(state: &AppState, account_id: Uuid, profile_id: Uuid, add: &Map<String, Value>) -> AppResult<()> {
    if add.is_empty() {
        return Ok(());
    }
    let mut tx = state.db.begin().await?;
    let cur: Value = sqlx::query_scalar("SELECT integration_keys FROM profiles WHERE id = $1 AND account_id = $2 FOR UPDATE").bind(profile_id).bind(account_id).fetch_one(&mut *tx).await?;
    let merged = merge_keys(cur.as_object().unwrap_or(&Map::new()), add);
    sqlx::query("UPDATE profiles SET integration_keys = $3 WHERE id = $1 AND account_id = $2").bind(profile_id).bind(account_id).bind(Value::Object(merged)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Remember the plain property values an integration wrote, so later lookups by them can trust them.
async fn save_keys(state: &AppState, account_id: Uuid, profile_id: Uuid, props: &Map<String, Value>) -> AppResult<()> {
    let keys = trusted_keys(props);
    if keys.is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE profiles SET integration_keys = integration_keys || $3 WHERE id = $1 AND account_id = $2")
        .bind(profile_id)
        .bind(account_id)
        .bind(Value::Object(keys))
        .execute(&state.db)
        .await?;
    Ok(())
}

/// Mark a profile the integration just made, and suppress booking-site relay addresses (and any
/// address the script marked `marketing: false`) from email. Never clears an existing suppression.
async fn after_write(state: &AppState, src: &Source, id: Uuid, created: bool, email: Option<&str>, no_marketing: bool, out: &mut Outcome) -> AppResult<Option<&'static str>> {
    if created {
        sqlx::query("UPDATE profiles SET source = $2 WHERE id = $1 AND account_id = $3 AND source IS NULL").bind(id).bind(src.tag()).bind(src.account_id).execute(&state.db).await?;
        // A person this integration just added, with no opt-in: keep them out of marketing until
        // they agree (an explicit consent block, a form, double opt-in). Never set on someone who existed.
        if !src.contacts_opted_in {
            crate::marketing_hold::hold_new_profile(&state.db, src.account_id, id).await?;
            out.counts.held += 1;
        }
    }
    let reason = match email {
        Some(e) if is_relay_address(e) => Some(REASON_RELAY),
        Some(_) if no_marketing => Some(REASON_NO_MARKETING),
        _ => None,
    };
    if let Some(r) = reason {
        let changed = sqlx::query("UPDATE profiles SET email_suppressed_reason = $2, updated_at = now() WHERE id = $1 AND account_id = $3 AND email_suppressed_reason IS NULL")
            .bind(id)
            .bind(r)
            .bind(src.account_id)
            .execute(&state.db)
            .await?
            .rows_affected();
        if changed > 0 {
            return Ok(Some(r));
        }
    }
    Ok(None)
}

/// Resolve `who` to a profile, creating it from identifiers if needed (a property lookup never creates).
async fn resolve(state: &AppState, src: &Source, w: &Who, dry: bool, out: &mut Outcome) -> AppResult<Option<Found>> {
    if let (Some(p), Some(v)) = (&w.property, &w.value) {
        return match find_by_property(state, src.account_id, p, v).await? {
            Lookup::Found(id) => Ok(Some(Found { id, created: false })),
            Lookup::Ambiguous => Err(AppError::bad(format!("more than one profile has {p} = {v}; not guessing which one"))),
            Lookup::Missing if w.if_missing.as_deref() == Some("retry") && !dry => Err(AppError::bad(format!("no profile has {p} = {v} yet; this delivery will be tried again"))),
            Lookup::Missing => Ok(None),
        };
    }
    let input = input_for(w);
    let existing = audience::find_profile(&state.db, src.account_id, &input).await?;
    if dry {
        return Ok(Some(Found { id: existing.as_ref().map(|p| p.id).unwrap_or_else(Uuid::nil), created: existing.is_none() }));
    }
    // A new profile only ever comes from the single insert that holds it and records its source;
    // an existing match is only updated (if it vanished meanwhile, look again).
    let mut existing = existing;
    for _ in 0..3 {
        if existing.is_none() {
            crate::features::objects::hooks::pause(src.account_id, "before_create").await;
            if let Some(p) = create_profile(state, src, &input, &Map::new(), &Map::new(), false, None).await? {
                out.counts.profiles_created += 1;
                if !src.contacts_opted_in {
                    out.counts.held += 1;
                }
                return Ok(Some(Found { id: p.id, created: true }));
            }
        } else {
            crate::features::objects::hooks::pause(src.account_id, "before_update").await;
            if let Some(up) = audience::update_matching_profile(&state.db, src.account_id, &input).await? {
                after_write(state, src, up.profile.id, false, up.profile.email.as_deref(), false, out).await?;
                mark_sources(state, src, up.profile.id, input.email.as_deref(), input.phone_number.as_deref()).await?;
                return Ok(Some(Found { id: up.profile.id, created: false }));
            }
        }
        existing = audience::find_profile(&state.db, src.account_id, &input).await?;
    }
    Err(AppError::Conflict("that profile kept changing while this ran; this delivery will be tried again".into()))
}

/// Create a profile the integration found nobody for, in one transaction with everything that
/// comes with it: where it came from, the marketing hold (no opt-in), the integration's own keys,
/// a relay-address or `marketing: false` suppression, which source supplied its email and phone,
/// and (`ledger`) the action's entry in its delivery's ledger. A failure part-way leaves nothing,
/// so a retry never finds an unheld profile it made. None when someone else created a matching
/// profile at the same moment (the caller updates that one instead).
async fn create_profile(state: &AppState, src: &Source, input: &ProfileInput, props: &Map<String, Value>, extra_keys: &Map<String, Value>, no_marketing: bool, ledger: Option<(usize, &str)>) -> AppResult<Option<Profile>> {
    let email = input.email.as_deref().and_then(normalize_email);
    let phone = input.phone_number.as_deref().and_then(normalize_phone);
    let suppressed = match &email {
        Some(e) if is_relay_address(e) => Some(REASON_RELAY),
        Some(_) if no_marketing => Some(REASON_NO_MARKETING),
        _ => None,
    };
    let keys = Value::Object(merge_keys(&trusted_keys(props), extra_keys));
    let tag = src.tag();
    let mut tx = state.db.begin().await?;
    let q = format!(
        "INSERT INTO profiles AS p (id, account_id, email, phone_number, external_id, first_name, last_name, properties, email_consent, sms_consent,
                                    source, marketing_hold, integration_keys, email_suppressed_reason, email_source, phone_source)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'never_subscribed', 'never_subscribed', $9, $10, $11, $12,
                 CASE WHEN $3::text IS NULL THEN NULL ELSE $9 END, CASE WHEN $4::text IS NULL THEN NULL ELSE $9 END)
         ON CONFLICT DO NOTHING RETURNING {PROFILE_COLS}"
    );
    let created: Option<Profile> = sqlx::query_as(&q)
        .bind(Uuid::now_v7())
        .bind(src.account_id)
        .bind(&email)
        .bind(&phone)
        .bind(&input.external_id)
        .bind(&input.first_name)
        .bind(&input.last_name)
        .bind(Value::Object(props.clone()))
        .bind(&tag)
        .bind(!src.contacts_opted_in)
        .bind(&keys)
        .bind(suppressed)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(p) = created else { return Ok(None) };
    if let (Some(d), Some((i, print))) = (src.ledger, ledger) {
        mark_applied(&mut *tx, d, i, print).await?;
    }
    tx.commit().await?;
    crate::features::webhooks::profile_saved(&state.db, src.account_id, None, None, &p, EventSource::Integration).await;
    Ok(Some(p))
}

/// Record in the delivery's ledger that action `i` was written.
async fn mark_applied<'e>(db: impl sqlx::PgExecutor<'e>, delivery: Uuid, i: usize, print: &str) -> AppResult<()> {
    sqlx::query("UPDATE integration_deliveries SET applied = applied || jsonb_build_object($2::text, $3::text) WHERE id = $1")
        .bind(delivery)
        .bind(i.to_string())
        .bind(print)
        .execute(db)
        .await?;
    Ok(())
}

/// Note that this integration supplied the profile's current email and phone (only where the
/// profile still has exactly those): flows marked as service messages may use them.
async fn mark_sources(state: &AppState, src: &Source, id: Uuid, email: Option<&str>, phone: Option<&str>) -> AppResult<()> {
    let email = email.and_then(normalize_email);
    let phone = phone.and_then(normalize_phone);
    if email.is_none() && phone.is_none() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE profiles SET email_source = CASE WHEN $3::text IS NOT NULL AND email = $3 THEN $2 ELSE email_source END,
                             phone_source = CASE WHEN $4::text IS NOT NULL AND phone_number = $4 THEN $2 ELSE phone_source END
         WHERE id = $1 AND account_id = $5",
    )
    .bind(id)
    .bind(src.tag())
    .bind(&email)
    .bind(&phone)
    .bind(src.account_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn resolve_list(state: &AppState, account_id: Uuid, a: &ListAction) -> AppResult<(Uuid, String)> {
    let row: Option<(Uuid, String)> = match (&a.list_id, &a.list) {
        (Some(id), _) => sqlx::query_as("SELECT id, name FROM lists WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).fetch_optional(&state.db).await?,
        (None, Some(name)) => sqlx::query_as("SELECT id, name FROM lists WHERE account_id = $1 AND name = $2 ORDER BY created_at LIMIT 1").bind(account_id).bind(name).fetch_optional(&state.db).await?,
        _ => None,
    };
    row.ok_or_else(|| match (&a.list_id, &a.list) {
        (Some(id), _) => AppError::bad(format!("there's no list with id {id}")),
        (_, Some(n)) => AppError::bad(format!("there's no list named {n:?}; create it first or fix the name")),
        _ => AppError::bad("no list given"),
    })
}

/// Apply validated actions for `src`'s account. With `dry`, nothing is written and the outcome
/// says what would happen. `raw` is the script's result (one entry per action) and `skip` the
/// actions a previous attempt already wrote (index → fingerprint): those are skipped. The outcome
/// is returned even when an action fails, so the caller can save what was written.
pub async fn apply(state: &AppState, src: &Source, actions: &[Action], raw: &[Value], skip: &Map<String, Value>, current_state: &Map<String, Value>, dry: bool) -> (Outcome, Option<AppError>) {
    let mut out = Outcome::default();
    let err = apply_into(state, src, actions, raw, skip, current_state, dry, &mut out).await.err();
    (out, err)
}

#[allow(clippy::too_many_arguments)]
async fn apply_into(state: &AppState, src: &Source, actions: &[Action], raw: &[Value], skip: &Map<String, Value>, current_state: &Map<String, Value>, dry: bool, out: &mut Outcome) -> AppResult<()> {
    // Lists must exist before anything is written, so a typo can't leave half a delivery behind.
    let mut lists = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        if let Action::AddToList(l) | Action::RemoveFromList(l) = a {
            let found = resolve_list(state, src.account_id, l).await.map_err(|e| AppError::bad(format!("action {i}: {}", e.public_message())))?;
            lists.push((i, found));
        }
    }
    // The script may record only the metrics the integration declared (they're server-only).
    for (i, a) in actions.iter().enumerate() {
        if let Action::TrackEvent(t) = a {
            if !src.metrics.iter().any(|m| m == t.metric.trim()) {
                return Err(AppError::bad(format!(
                    "action {i} (track_event): {:?} isn't one of this integration's metrics; add it to the integration's `metrics` list first",
                    t.metric.trim()
                )));
            }
        }
    }
    let list_for = |i: usize| lists.iter().find(|(j, _)| *j == i).map(|(_, l)| l.clone());
    let mut new_state = current_state.clone();
    for (i, a) in actions.iter().enumerate() {
        let writes = !matches!(a, Action::SetState(_) | Action::Log(_) | Action::NextRequest(_));
        let print = raw.get(i).map(fingerprint).unwrap_or_default();
        if writes && !dry && skip.get(&i.to_string()).and_then(|v| v.as_str()) == Some(print.as_str()) {
            out.counts.already_written += 1;
            out.effect(i, "already written by an earlier attempt; skipped".into());
            out.applied.insert(i.to_string(), json!(print));
            continue;
        }
        let ledger_before = out.applied.len();
        match a {
            Action::UpsertProfile(p) => upsert(state, src, i, p, &print, dry, out).await?,
            Action::TrackEvent(t) => {
                let Some(found) = resolve(state, src, &t.profile, dry, out).await? else {
                    out.counts.skipped += 1;
                    out.effect(i, format!("skipped {}: no profile has {}", t.metric, who_label(&t.profile)));
                    continue;
                };
                let props = Value::Object(t.properties.clone().unwrap_or_default());
                if dry {
                    let dup: bool = match &t.unique_id {
                        Some(u) => sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.account_id = $1 AND m.name = $2 AND e.unique_id = $3)")
                            .bind(src.account_id)
                            .bind(t.metric.trim())
                            .bind(u)
                            .fetch_one(&state.db)
                            .await?,
                        None => false,
                    };
                    let who = if found.created { format!("new profile {}", who_label(&t.profile)) } else { who_label(&t.profile) };
                    if dup {
                        out.counts.duplicate_events += 1;
                        out.effect(i, format!("would skip {} for {who}: already recorded (unique_id {})", t.metric, t.unique_id.as_deref().unwrap_or("")));
                    } else {
                        out.counts.events += 1;
                        out.effect(i, format!("would record {} for {who}{}", t.metric, t.value.map(|v| format!(" (value {v})")).unwrap_or_default()));
                    }
                    continue;
                }
                // A stable id, so a retry can never record it twice (the stored action list always has one).
                let unique_id = t.unique_id.clone().or_else(|| src.delivery.map(|d| format!("delivery:{d}:{i}:{print}")));
                let recorded = audience::record_event_from(
                    &state.db,
                    NewEvent {
                        account_id: src.account_id,
                        profile_id: found.id,
                        metric: t.metric.trim(),
                        properties: props,
                        value: t.value,
                        occurred_at: t.time.as_deref().and_then(parse_time),
                        unique_id,
                        message_id: None,
                    },
                    EventSource::Integration,
                )
                .await?;
                if recorded.is_some() {
                    out.counts.events += 1;
                    out.effect(i, format!("recorded {} for {}", t.metric, who_label(&t.profile)));
                } else {
                    out.counts.duplicate_events += 1;
                    out.effect(i, format!("skipped {} for {}: already recorded", t.metric, who_label(&t.profile)));
                }
            }
            Action::AddToList(l) | Action::RemoveFromList(l) => {
                let adding = matches!(a, Action::AddToList(_));
                let (list_id, list_name) = list_for(i).expect("resolved above");
                let Some(found) = resolve(state, src, &l.profile, dry, out).await? else {
                    out.counts.skipped += 1;
                    out.effect(i, format!("skipped list change: no profile has {}", who_label(&l.profile)));
                    continue;
                };
                let verb = if adding { "add" } else { "remove" };
                if dry {
                    if adding { out.counts.list_adds += 1 } else { out.counts.list_removes += 1 }
                    out.effect(i, format!("would {verb} {} {} {list_name:?}", who_label(&l.profile), if adding { "to" } else { "from" }));
                    continue;
                }
                if adding {
                    let n = audience::add_to_list(&state.db, src.account_id, list_id, &[found.id]).await?;
                    out.counts.list_adds += n as u32;
                } else {
                    let n = sqlx::query("DELETE FROM list_members lm USING lists l WHERE lm.list_id = $1 AND lm.profile_id = $2 AND l.id = lm.list_id AND l.account_id = $3")
                        .bind(list_id)
                        .bind(found.id)
                        .bind(src.account_id)
                        .execute(&state.db)
                        .await?
                        .rows_affected();
                    out.counts.list_removes += n as u32;
                }
                out.effect(i, format!("{} {} {} {list_name:?}", if adding { "added" } else { "removed" }, who_label(&l.profile), if adding { "to" } else { "from" }));
            }
            Action::Unsubscribe(u) => {
                let Some(found) = resolve(state, src, &u.profile, dry, out).await? else {
                    out.counts.skipped += 1;
                    out.effect(i, format!("skipped unsubscribe: no profile has {}", who_label(&u.profile)));
                    continue;
                };
                out.counts.unsubscribes += 1;
                let ch = if u.channel == Channel::Email { "email" } else { "SMS" };
                if dry {
                    out.effect(i, format!("would unsubscribe {} from {ch}", who_label(&u.profile)));
                    continue;
                }
                unsubscribe(state, src, found.id, u.channel).await?;
                out.effect(i, format!("unsubscribed {} from {ch}", who_label(&u.profile)));
            }
            Action::Suppress(s) => {
                let who = Who { email: s.email.clone(), phone_number: s.phone_number.clone(), ..Default::default() };
                out.counts.suppressed += 1;
                if dry {
                    out.effect(i, format!("would stop all marketing to {}", who_label(&who)));
                    continue;
                }
                let Some(found) = resolve(state, src, &who, dry, out).await? else { continue };
                if s.email.is_some() {
                    after_write(state, src, found.id, false, s.email.as_deref(), true, out).await?;
                }
                if s.phone_number.is_some() {
                    unsubscribe(state, src, found.id, Channel::Sms).await?;
                }
                out.effect(i, format!("stopped all marketing to {}", who_label(&who)));
            }
            Action::UpsertObject(o) => upsert_object(state, src, i, o, dry, out).await?,
            Action::DeleteObject(d) => {
                let id = crate::features::objects::store::ids_for(&state.db, src.account_id, &d.object_type, &[d.external_id.trim().to_string()]).await?;
                if dry {
                    out.effect(i, if id.is_empty() { format!("would skip deleting {} {}: there's no such object", d.object_type, d.external_id) } else { format!("would delete {} {}", d.object_type, d.external_id) });
                    if !id.is_empty() {
                        out.counts.objects_deleted += 1;
                    }
                    continue;
                }
                let ids: Vec<Uuid> = id.iter().map(|(id, _)| *id).collect();
                let n = if ids.is_empty() { 0 } else { crate::features::objects::store::delete(&state.db, src.account_id, &ids).await? };
                out.counts.objects_deleted += n as u32;
                out.effect(i, if n > 0 { format!("deleted {} {}", d.object_type, d.external_id) } else { format!("{} {} was already gone", d.object_type, d.external_id) });
            }
            Action::SetState(s) => {
                for (k, v) in &s.values {
                    if v.is_null() {
                        new_state.remove(k);
                    } else {
                        new_state.insert(k.clone(), v.clone());
                    }
                }
                if serde_json::to_string(&new_state).map(|s| s.len()).unwrap_or(0) > MAX_STATE_BYTES {
                    return Err(AppError::bad(format!("action {i} (set_state): saved state would be over 64 KB")));
                }
                out.effect(i, format!("{} state: {}", if dry { "would save" } else { "saved" }, Value::Object(s.values.clone())));
            }
            Action::Log(l) => {
                out.logs.push(l.message.clone());
                out.effect(i, format!("log: {}", l.message));
            }
            Action::NextRequest(n) => {
                out.next_request = Some(n.clone());
                out.effect(i, "fetch the next page".into());
            }
        }
        if writes && !dry {
            // Written right after the action's effects (in the same transaction where the action
            // wrote one itself, like a new profile); each effect is also keyed so a retry that
            // reruns it changes nothing.
            if out.applied.len() == ledger_before {
                if let Some(d) = src.ledger {
                    mark_applied(&state.db, d, i, &print).await?;
                }
            }
            out.applied.insert(i.to_string(), json!(print));
        }
    }
    if new_state != *current_state {
        out.state = new_state;
    }
    Ok(())
}

/// Write (or, dry, describe) an `upsert_object`: its people are found or created as for other
/// actions (new ones held back from marketing without an opt-in), the integration's redact list is
/// applied to its fields, and it's written with the app's `as_of` so an older state never wins.
async fn upsert_object(state: &AppState, src: &Source, i: usize, o: &UpsertObject, dry: bool, out: &mut Outcome) -> AppResult<()> {
    let key = o.object_type.trim();
    let external_id = match (&o.external_id, dry) {
        (Some(e), _) => e.trim().to_string(),
        (None, true) => "(made from the delivery)".into(),
        (None, false) => return Err(AppError::bad(format!("action {i} (upsert_object): external_id is missing"))),
    };
    let fields = o.fields.clone().unwrap_or_default();
    let redact = super::redact::paths(&src.redact);
    let mut links = Vec::new();
    let mut who = Vec::new();
    for w in &o.profiles {
        let Some(found) = resolve(state, src, &w.who(), dry, out).await? else {
            who.push(format!("{} (nobody yet; not linked)", who_label(&w.who())));
            continue;
        };
        let rel = crate::features::objects::store::relationship(w.relationship.as_deref())?;
        who.push(if rel.is_empty() { who_label(&w.who()) } else { format!("{} ({rel})", who_label(&w.who())) });
        links.push((found.id, rel));
    }
    let people = if who.is_empty() { String::new() } else { format!(" for {}", who.join(", ")) };
    if dry {
        let (what, changed) = crate::features::objects::store::preview(state, src.account_id, key, &external_id, &fields, &redact).await?;
        if what != "unchanged" {
            out.counts.objects_upserted += 1;
        }
        let detail = if changed.is_empty() { String::new() } else { format!(" ({})", changed.join(", ")) };
        out.effect(i, match what {
            "created" => format!("would create {key} {external_id}{detail}{people}"),
            "updated" => format!("would update {key} {external_id}{detail}{people}"),
            _ => format!("{key} {external_id} unchanged{people}"),
        });
        return Ok(());
    }
    let w = crate::features::objects::store::Writer { account_id: src.account_id, source: src.tag(), redact };
    let as_of = crate::features::objects::store::as_of_text(&o.as_of)?;
    let r = crate::features::objects::store::write(state, &w, key, &external_id, &fields, &links, as_of.as_deref()).await?;
    let detail = if r.changed.is_empty() { String::new() } else { format!(" ({})", r.changed.join(", ")) };
    match r.result {
        "created" | "updated" => {
            out.counts.objects_upserted += 1;
            out.effect(i, format!("{} {key} {}{detail}{people}", r.result, r.external_id));
        }
        "older" => out.effect(i, format!("kept {key} {}: this state is older than the one stored", r.external_id)),
        _ => out.effect(i, format!("{key} {} unchanged{people}", r.external_id)),
    }
    Ok(())
}

async fn upsert(state: &AppState, src: &Source, i: usize, p: &UpsertProfile, print: &str, dry: bool, out: &mut Outcome) -> AppResult<()> {
    let input = ProfileInput {
        email: p.email.clone().filter(|s| !s.trim().is_empty()),
        phone_number: p.phone_number.clone().filter(|s| !s.trim().is_empty()),
        external_id: p.external_id.clone().filter(|s| !s.trim().is_empty()),
        first_name: p.first_name.clone(),
        last_name: p.last_name.clone(),
        properties: p.properties.clone(),
        ..Default::default()
    };
    let label = who_label(&Who { email: input.email.clone(), phone_number: input.phone_number.clone(), external_id: input.external_id.clone(), ..Default::default() });
    let relay = input.email.as_deref().is_some_and(is_relay_address);
    let no_marketing = p.marketing == Some(false);
    if dry {
        let existing = audience::find_profile(&state.db, src.account_id, &input).await?;
        let mut line = match &existing {
            Some(_) => {
                out.counts.profiles_updated += 1;
                format!("would update profile {label}")
            }
            None => {
                out.counts.profiles_created += 1;
                format!("would create profile {label}")
            }
        };
        if relay {
            out.counts.suppressed += 1;
            line.push_str(" and suppress it from email (booking-site relay address)");
        } else if no_marketing && input.email.is_some() {
            out.counts.suppressed += 1;
            line.push_str(" and suppress it from marketing email (marketing: false)");
        }
        let opts_in = p.consent.iter().any(|c| c.channel == Channel::Email && c.subscribed);
        if existing.is_none() && !src.contacts_opted_in && !opts_in {
            out.counts.held += 1;
            line.push_str(" held back from marketing (no opt-in)");
        }
        for c in &p.consent {
            out.counts.consents += 1;
            line.push_str(&format!("; {} {} consent (source: {})", if c.subscribed { "record" } else { "remove" }, if c.channel == Channel::Email { "email" } else { "SMS" }, c.source));
        }
        out.effect(i, line);
        return Ok(());
    }
    let opts_in = p.consent.iter().any(|c| c.channel == Channel::Email && c.subscribed);
    // Nobody has these identifiers: create the profile with its hold, source, keys and suppression
    // in one transaction (with the ledger entry, when nothing else follows).
    // A new profile only ever comes from the single insert that holds it; an existing match is only
    // updated (if it vanished meanwhile, look again).
    let mut tries = 0;
    let up = loop {
        tries += 1;
        if tries > 3 {
            return Err(AppError::Conflict("that profile kept changing while this ran; this delivery will be tried again".into()));
        }
        if audience::find_profile(&state.db, src.account_id, &input).await?.is_some() {
            crate::features::objects::hooks::pause(src.account_id, "before_update").await;
            match audience::update_matching_profile(&state.db, src.account_id, &input).await? {
                Some(up) => break up,
                None => continue,
            }
        }
        crate::features::objects::hooks::pause(src.account_id, "before_create").await;
        let last = p.consent.is_empty();
        let props = p.properties.clone().unwrap_or_default();
        let keys = p.keys.clone().unwrap_or_default();
        if let Some(created) = create_profile(state, src, &input, &props, &keys, no_marketing, last.then_some((i, print))).await? {
            if last {
                out.applied.insert(i.to_string(), json!(print));
            }
            out.counts.profiles_created += 1;
            let mut line = format!("created profile {label}");
            if relay {
                out.counts.suppressed += 1;
                line.push_str(" and suppressed it from email (booking-site relay address)");
            } else if no_marketing && created.email.is_some() {
                out.counts.suppressed += 1;
                line.push_str(" and suppressed it from marketing email");
            }
            if !src.contacts_opted_in && !opts_in {
                out.counts.held += 1;
                line.push_str(", held back from marketing (no opt-in)");
            }
            for c in &p.consent {
                line.push_str(&consent(state, src, created.id, c).await?);
                out.counts.consents += 1;
            }
            out.effect(i, line);
            return Ok(());
        }
    };
    if up.created { out.counts.profiles_created += 1 } else { out.counts.profiles_updated += 1 }
    if let Some(props) = &p.properties {
        save_keys(state, src.account_id, up.profile.id, props).await?;
    }
    if let Some(keys) = &p.keys {
        add_keys(state, src.account_id, up.profile.id, keys).await?;
    }
    mark_sources(state, src, up.profile.id, input.email.as_deref(), input.phone_number.as_deref()).await?;
    let mut line = format!("{} profile {label}", if up.created { "created" } else { "updated" });
    let held_before = out.counts.held;
    if let Some(r) = after_write(state, src, up.profile.id, up.created, up.profile.email.as_deref(), no_marketing, out).await? {
        out.counts.suppressed += 1;
        line.push_str(if r == REASON_RELAY { " and suppressed it from email (booking-site relay address)" } else { " and suppressed it from marketing email" });
    }
    if out.counts.held > held_before && !opts_in {
        line.push_str(", held back from marketing (no opt-in)");
    } else if out.counts.held > held_before {
        out.counts.held -= 1;
    }
    for c in &p.consent {
        line.push_str(&consent(state, src, up.profile.id, c).await?);
        out.counts.consents += 1;
    }
    out.effect(i, line);
    Ok(())
}

/// Record a consent change the script vouches for. Subscribing never overrides an earlier unsubscribe.
async fn consent(state: &AppState, src: &Source, profile_id: Uuid, c: &Consent) -> AppResult<String> {
    let (col, label) = if c.channel == Channel::Email { ("email_consent", "email") } else { ("sms_consent", "SMS") };
    if !c.subscribed {
        unsubscribe(state, src, profile_id, c.channel).await?;
        return Ok(format!("; unsubscribed from {label}"));
    }
    // Only people who never subscribed (or are waiting to confirm) become subscribed.
    let q = format!("UPDATE profiles SET {col} = 'subscribed', updated_at = now() WHERE id = $1 AND account_id = $2 AND {col} IN ('never_subscribed', 'pending')");
    let n = sqlx::query(&q).bind(profile_id).bind(src.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        let now: String = sqlx::query_scalar(&format!("SELECT {col} FROM profiles WHERE id = $1")).bind(profile_id).fetch_one(&state.db).await?;
        return Ok(if now == "unsubscribed" { format!("; kept {label} unsubscribed (they unsubscribed before)") } else { format!("; already subscribed to {label}") });
    }
    let metric = if c.channel == Channel::Email { "Subscribed to Email Marketing" } else { "Subscribed to SMS Marketing" };
    audience::record_event(
        &state.db,
        NewEvent {
            account_id: src.account_id,
            profile_id,
            metric,
            properties: json!({"source": c.source, "integration": src.name, "integration_id": src.integration_id}),
            value: None,
            occurred_at: None,
            unique_id: None,
            message_id: None,
        },
    )
    .await?;
    Ok(format!("; subscribed to {label} (source: {})", c.source))
}

async fn unsubscribe(state: &AppState, src: &Source, profile_id: Uuid, channel: Channel) -> AppResult<()> {
    let (col, metric) = if channel == Channel::Email { ("email_consent", "Unsubscribed") } else { ("sms_consent", "Unsubscribed from SMS") };
    let q = format!("UPDATE profiles SET {col} = 'unsubscribed', updated_at = now() WHERE id = $1 AND account_id = $2 AND {col} <> 'unsubscribed'");
    let n = sqlx::query(&q).bind(profile_id).bind(src.account_id).execute(&state.db).await?.rows_affected();
    if n > 0 {
        audience::record_event(
            &state.db,
            NewEvent {
                account_id: src.account_id,
                profile_id,
                metric,
                properties: json!({"source": "integration", "integration": src.name, "integration_id": src.integration_id}),
                value: None,
                occurred_at: None,
                unique_id: None,
                message_id: None,
            },
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_addresses() {
        assert!(is_relay_address("abc123@guest.airbnb.com"));
        assert!(is_relay_address("Jane.X@Guest.Booking.com"));
        assert!(is_relay_address("x@messages.homeaway.com"));
        assert!(is_relay_address("x@m.expediapartnercentral.com"));
        assert!(!is_relay_address("jane@gmail.com"));
        assert!(!is_relay_address("jane@notairbnb.com"));
        assert!(!is_relay_address("not an email"));
    }

    #[test]
    fn parses_and_names_the_bad_action() {
        let ok = json!([
            {"type": "upsert_profile", "email": "a@b.co", "properties": {"x": 1}},
            {"type": "track_event", "metric": "Reservation Confirmed", "profile": {"email": "a@b.co"}, "unique_id": "r1", "time": "2026-11-20"},
            {"type": "add_to_list", "list": "Guests", "profile": {"email": "a@b.co"}},
            {"type": "set_state", "values": {"cursor": "abc"}},
            {"type": "log", "message": "hi"}
        ]);
        assert_eq!(parse(&ok, false).unwrap().len(), 5);
        let unknown = json!([{"type": "log", "message": "x"}, {"type": "track_event", "metric": "M", "profile": {"email": "a@b.co"}, "colour": "red"}]);
        let e = parse(&unknown, false).unwrap_err();
        assert!(e.starts_with("action 1 (track_event): unknown field `colour`"), "{e}");
        assert!(parse(&json!([{"type": "teleport"}]), false).unwrap_err().contains("unknown type"));
        assert!(parse(&json!([{"message": "x"}]), false).unwrap_err().contains("missing `type`"));
        assert!(parse(&json!({"type": "log"}), false).unwrap_err().contains("array"));
        assert!(parse(&json!([{"type": "upsert_profile", "first_name": "A"}]), false).is_err());
        assert!(parse(&json!([{"type": "upsert_profile", "email": "nope"}]), false).unwrap_err().contains("valid email"));
        assert!(parse(&json!([{"type": "track_event", "metric": "M", "profile": {}}]), false).is_err());
        assert!(parse(&json!([{"type": "track_event", "metric": "M", "profile": {"email": "a@b.co"}, "time": "yesterday"}]), false).is_err());
        assert!(parse(&json!([{"type": "next_request"}]), false).unwrap_err().contains("only pulls"));
        assert!(parse(&json!([{"type": "next_request", "query": {"skip": 100}}]), true).is_ok());
        assert!(parse(&json!([{"type": "add_to_list", "profile": {"email": "a@b.co"}}]), false).is_err());
        let many: Vec<Value> = (0..=MAX_ACTIONS).map(|_| json!({"type": "log", "message": "x"})).collect();
        assert!(parse(&Value::Array(many), false).unwrap_err().contains("limit"));
        assert!(parse(&Value::Null, false).unwrap().is_empty());
    }

    #[test]
    fn consent_must_be_explicit() {
        // A consent block needs a channel, subscribed and a source.
        assert!(parse(&json!([{"type": "upsert_profile", "email": "a@b.co", "consent": [{"channel": "email", "subscribed": true}]}]), false).is_err());
        assert!(parse(&json!([{"type": "upsert_profile", "email": "a@b.co", "consent": [{"channel": "email", "subscribed": true, "source": " "}]}]), false).is_err());
        assert!(parse(&json!([{"type": "upsert_profile", "email": "a@b.co", "consent": [{"channel": "email", "subscribed": true, "source": "Checkout box"}]}]), false).is_ok());
        assert!(parse(&json!([{"type": "upsert_profile", "email": "a@b.co", "consent": [{"channel": "sms", "subscribed": true, "source": "x"}]}]), false).unwrap_err().contains("phone"));
        assert!(parse(&json!([{"type": "upsert_profile", "email": "a@b.co", "marketing": true}]), false).is_err());
        assert!(parse(&json!([{"type": "upsert_profile", "email": "a@b.co", "email_consent": "subscribed"}]), false).unwrap_err().contains("unknown field"));
    }
}
