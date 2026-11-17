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

use crate::audience::{self, NewEvent, ProfileInput};
use crate::error::{AppError, AppResult};
use crate::models::{normalize_email, normalize_phone};
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
}

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
}

pub const ACTION_TYPES: &[&str] = &["upsert_profile", "track_event", "add_to_list", "remove_from_list", "unsubscribe", "suppress", "set_state", "log", "next_request"];

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
                Action::UpsertProfile(a)
            }
            "track_event" => {
                let a: TrackEvent = part(i, k, obj)?;
                if a.metric.trim().is_empty() || a.metric.len() > 128 {
                    return Err(format!("action {i} ({k}): metric must be 1-128 characters"));
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
}

impl Counts {
    pub fn wrote_anything(&self) -> bool {
        self.profiles_created + self.profiles_updated + self.events + self.list_adds + self.list_removes + self.unsubscribes + self.suppressed + self.consents > 0
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

async fn find_by_property(state: &AppState, account_id: Uuid, prop: &str, value: &Value) -> AppResult<Option<Uuid>> {
    let mut m = Map::new();
    m.insert(prop.to_string(), value.clone());
    Ok(sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND properties @> $2 ORDER BY updated_at DESC LIMIT 1")
        .bind(account_id)
        .bind(Value::Object(m))
        .fetch_optional(&state.db)
        .await?)
}

/// Mark a profile the integration just made, and suppress booking-site relay addresses (and any
/// address the script marked `marketing: false`) from email. Never clears an existing suppression.
async fn after_write(state: &AppState, src: &Source, id: Uuid, created: bool, email: Option<&str>, no_marketing: bool) -> AppResult<Option<&'static str>> {
    if created {
        sqlx::query("UPDATE profiles SET source = $2 WHERE id = $1 AND account_id = $3 AND source IS NULL").bind(id).bind(src.tag()).bind(src.account_id).execute(&state.db).await?;
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
        return Ok(find_by_property(state, src.account_id, p, v).await?.map(|id| Found { id, created: false }));
    }
    let input = input_for(w);
    if dry {
        let existing = audience::find_profile(&state.db, src.account_id, &input).await?;
        return Ok(Some(Found { id: existing.as_ref().map(|p| p.id).unwrap_or_else(Uuid::nil), created: existing.is_none() }));
    }
    let up = audience::upsert_profile(&state.db, src.account_id, &input).await?;
    if up.created {
        out.counts.profiles_created += 1;
    }
    after_write(state, src, up.profile.id, up.created, up.profile.email.as_deref(), false).await?;
    Ok(Some(Found { id: up.profile.id, created: up.created }))
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
/// says what would happen. `current_state` is the integration's saved state before this run.
pub async fn apply(state: &AppState, src: &Source, actions: &[Action], current_state: &Map<String, Value>, dry: bool) -> AppResult<Outcome> {
    let mut out = Outcome::default();
    // Lists must exist before anything is written, so a typo can't leave half a delivery behind.
    let mut lists = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        if let Action::AddToList(l) | Action::RemoveFromList(l) = a {
            let found = resolve_list(state, src.account_id, l).await.map_err(|e| AppError::bad(format!("action {i}: {}", e.public_message())))?;
            lists.push((i, found));
        }
    }
    let list_for = |i: usize| lists.iter().find(|(j, _)| *j == i).map(|(_, l)| l.clone());
    let mut new_state = current_state.clone();
    for (i, a) in actions.iter().enumerate() {
        match a {
            Action::UpsertProfile(p) => upsert(state, src, i, p, dry, &mut out).await?,
            Action::TrackEvent(t) => {
                let Some(found) = resolve(state, src, &t.profile, dry, &mut out).await? else {
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
                let recorded = audience::record_event(
                    &state.db,
                    NewEvent {
                        account_id: src.account_id,
                        profile_id: found.id,
                        metric: t.metric.trim(),
                        properties: props,
                        value: t.value,
                        occurred_at: t.time.as_deref().and_then(parse_time),
                        unique_id: t.unique_id.clone(),
                        message_id: None,
                    },
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
                let Some(found) = resolve(state, src, &l.profile, dry, &mut out).await? else {
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
                let Some(found) = resolve(state, src, &u.profile, dry, &mut out).await? else {
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
                let Some(found) = resolve(state, src, &who, dry, &mut out).await? else { continue };
                if s.email.is_some() {
                    after_write(state, src, found.id, false, s.email.as_deref(), true).await?;
                }
                if s.phone_number.is_some() {
                    unsubscribe(state, src, found.id, Channel::Sms).await?;
                }
                out.effect(i, format!("stopped all marketing to {}", who_label(&who)));
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
    }
    if new_state != *current_state {
        out.state = new_state;
    }
    Ok(out)
}

async fn upsert(state: &AppState, src: &Source, i: usize, p: &UpsertProfile, dry: bool, out: &mut Outcome) -> AppResult<()> {
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
        for c in &p.consent {
            out.counts.consents += 1;
            line.push_str(&format!("; {} {} consent (source: {})", if c.subscribed { "record" } else { "remove" }, if c.channel == Channel::Email { "email" } else { "SMS" }, c.source));
        }
        out.effect(i, line);
        return Ok(());
    }
    let up = audience::upsert_profile(&state.db, src.account_id, &input).await?;
    if up.created { out.counts.profiles_created += 1 } else { out.counts.profiles_updated += 1 }
    let mut line = format!("{} profile {label}", if up.created { "created" } else { "updated" });
    if let Some(r) = after_write(state, src, up.profile.id, up.created, up.profile.email.as_deref(), no_marketing).await? {
        out.counts.suppressed += 1;
        line.push_str(if r == REASON_RELAY { " and suppressed it from email (booking-site relay address)" } else { " and suppressed it from marketing email" });
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
