//! Link tagging: UTM parameters added to the web links in campaign and flow messages.
//!
//! An account turns tagging on and sets default values (Settings → Tracking); a campaign or flow can
//! use those, turn tagging off, or set its own values. Values may use a fixed set of variables
//! ([`VARIABLES`]); nothing else is a template.
//!
//! How tags reach a message, with no query per link or per recipient: the resolved parameters ride
//! on the message content under [`CONTENT_KEY`]. A campaign's are worked out once, in the
//! transaction that fixes its audience ([`freeze_campaign`]); a flow's are worked out at each step
//! from the defaults loaded with the flow row ([`for_flow_step`]); a test send works them out on the
//! spot ([`for_test`]). The send path reads them back with [`Tagger::from_content`] and the renderer
//! calls it for each link it wraps for click tracking. Content without the key renders exactly as
//! before.

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow, FlowRow};
use crate::state::AppState;

/// Where resolved tags ride on message content. Every path that sends sets or removes it, so a value
/// typed into content by hand never decides what's sent.
pub const CONTENT_KEY: &str = "link_tags";

/// The variables a value may use, written `{name}`.
pub const VARIABLES: &[(&str, &str)] = &[
    ("campaign_name", "The campaign's name (empty in flows)"),
    ("campaign_id", "The campaign's id (empty in flows)"),
    ("flow_name", "The flow's name (empty in campaigns)"),
    ("flow_id", "The flow's id (empty in campaigns)"),
    ("message_name", "The campaign's name, or the flow step's name"),
    ("channel", "email or sms"),
    ("send_date", "The day it's sent, YYYY-MM-DD in the account's time zone"),
    ("version", "The A/B test version (A, B, C or D; empty without a test)"),
];

pub const DEFAULT_SOURCE: &str = "dumbmail";
pub const DEFAULT_MEDIUM: &str = "{channel}";
pub const MAX_CUSTOM: usize = 10;
pub const MAX_VALUE: usize = 200;
pub const MAX_SKIP_DOMAINS: usize = 50;

/// The parameters as an account or campaign sets them. `source`, `medium` and `campaign` fall back to
/// their defaults when empty; `content` and `term` are left out when empty.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub medium: Option<String>,
    #[serde(default)]
    pub campaign: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub term: Option<String>,
    #[serde(default)]
    pub custom: Vec<Custom>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Custom {
    pub name: String,
    pub value: String,
}

/// A campaign's or flow's choice: the account's defaults (no row value), off, or its own values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Override {
    Off,
    Custom { params: Params },
}

/// The account's settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Defaults {
    pub enabled: bool,
    #[serde(default)]
    pub params: Params,
    #[serde(default)]
    pub skip_domains: Vec<String>,
}

static VAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{([a-z_]*)\}").unwrap());
static CUSTOM_NAME: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-z0-9_]{1,40}$").unwrap());
static DOMAIN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$").unwrap());
const STANDARD_NAMES: [&str; 5] = ["utm_source", "utm_medium", "utm_campaign", "utm_content", "utm_term"];

fn check_value(label: &str, v: &str) -> AppResult<()> {
    if v.chars().count() > MAX_VALUE {
        return Err(AppError::bad(format!("{label} is too long (at most {MAX_VALUE} characters)")));
    }
    if v.chars().any(char::is_control) {
        return Err(AppError::bad(format!("{label} can't contain line breaks or control characters")));
    }
    let names: Vec<&str> = VARIABLES.iter().map(|(n, _)| *n).collect();
    for c in VAR.captures_iter(v) {
        if !names.contains(&&c[1]) {
            return Err(AppError::bad(format!("{label}: {{{}}} isn't a variable; use one of {}", &c[1], names.iter().map(|n| format!("{{{n}}}")).collect::<Vec<_>>().join(", "))));
        }
    }
    // Any other brace is a typo for a variable.
    if VAR.replace_all(v, "").contains(['{', '}']) {
        return Err(AppError::bad(format!("{label}: braces are only for variables like {{campaign_name}}")));
    }
    Ok(())
}

/// Trim every value, drop empty ones and check the rest.
pub fn checked_params(mut p: Params) -> AppResult<Params> {
    let tidy = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    p.source = tidy(p.source);
    p.medium = tidy(p.medium);
    p.campaign = tidy(p.campaign);
    p.content = tidy(p.content);
    p.term = tidy(p.term);
    for (label, v) in [("utm_source", &p.source), ("utm_medium", &p.medium), ("utm_campaign", &p.campaign), ("utm_content", &p.content), ("utm_term", &p.term)] {
        if let Some(v) = v {
            check_value(label, v)?;
        }
    }
    if p.custom.len() > MAX_CUSTOM {
        return Err(AppError::bad(format!("at most {MAX_CUSTOM} custom parameters")));
    }
    let mut seen: Vec<String> = Vec::new();
    for c in &mut p.custom {
        c.name = c.name.trim().to_string();
        c.value = c.value.trim().to_string();
        if !CUSTOM_NAME.is_match(&c.name) {
            return Err(AppError::bad(format!("custom parameter name {:?} must be 1-40 lowercase letters, digits or underscores", c.name)));
        }
        if STANDARD_NAMES.contains(&c.name.as_str()) {
            return Err(AppError::bad(format!("{} has its own field; set it there", c.name)));
        }
        if seen.contains(&c.name) {
            return Err(AppError::bad(format!("custom parameter {} is listed twice", c.name)));
        }
        if c.value.is_empty() {
            return Err(AppError::bad(format!("give custom parameter {} a value", c.name)));
        }
        check_value(&c.name, &c.value)?;
        seen.push(c.name.clone());
    }
    Ok(p)
}

/// Lowercase, deduplicated domains; `www.` and a scheme or path someone pasted are dropped.
pub fn checked_domains(domains: Vec<String>) -> AppResult<Vec<String>> {
    if domains.len() > MAX_SKIP_DOMAINS {
        return Err(AppError::bad(format!("at most {MAX_SKIP_DOMAINS} domains that aren't tagged")));
    }
    let mut out: Vec<String> = Vec::new();
    for raw in domains {
        let mut d = raw.trim().to_ascii_lowercase();
        for prefix in ["https://", "http://"] {
            if let Some(rest) = d.strip_prefix(prefix) {
                d = rest.to_string();
            }
        }
        let d = d.split(['/', '?', '#']).next().unwrap_or("").trim_end_matches('.').to_string();
        let d = d.strip_prefix("www.").map(str::to_string).unwrap_or(d);
        if d.is_empty() {
            continue;
        }
        if d.len() > 253 || !DOMAIN.is_match(&d) {
            return Err(AppError::bad(format!("{raw:?} isn't a domain name (like shop.example.com)")));
        }
        if !out.contains(&d) {
            out.push(d);
        }
    }
    Ok(out)
}

/// What one message is about, for the variables.
pub struct Vars<'a> {
    pub campaign: Option<(Uuid, &'a str)>,
    pub flow: Option<(Uuid, &'a str)>,
    pub message_name: &'a str,
    pub channel: &'a str,
    pub send_date: String,
    pub version: Option<&'a str>,
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 60 {
            break;
        }
    }
    out.trim_end_matches('-').to_string()
}

fn short_id(id: Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

/// The default `utm_campaign`: the campaign's or flow's name, slugged, and the first 8 characters of its id.
pub fn default_campaign_value(v: &Vars<'_>) -> String {
    let (id, name) = v.campaign.or(v.flow).unwrap_or((Uuid::nil(), v.message_name));
    let s = slug(name);
    if s.is_empty() { short_id(id) } else { format!("{s}-{}", short_id(id)) }
}

fn fill(template: &str, v: &Vars<'_>) -> String {
    let out = VAR.replace_all(template, |c: &regex::Captures<'_>| match &c[1] {
        "campaign_name" => v.campaign.map(|(_, n)| n.to_string()).unwrap_or_default(),
        "campaign_id" => v.campaign.map(|(id, _)| id.to_string()).unwrap_or_default(),
        "flow_name" => v.flow.map(|(_, n)| n.to_string()).unwrap_or_default(),
        "flow_id" => v.flow.map(|(id, _)| id.to_string()).unwrap_or_default(),
        "message_name" => v.message_name.to_string(),
        "channel" => v.channel.to_string(),
        "send_date" => v.send_date.clone(),
        "version" => v.version.map(str::to_uppercase).unwrap_or_default(),
        _ => String::new(),
    });
    out.trim().chars().take(MAX_VALUE).collect()
}

/// The parameters a message's links get, in order: name and value, variables filled in, empty ones left out.
pub fn resolve(p: &Params, v: &Vars<'_>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut push = |name: &str, value: String| {
        if !value.is_empty() {
            out.push((name.to_string(), value));
        }
    };
    push("utm_source", fill(p.source.as_deref().unwrap_or(DEFAULT_SOURCE), v));
    push("utm_medium", fill(p.medium.as_deref().unwrap_or(DEFAULT_MEDIUM), v));
    push("utm_campaign", p.campaign.as_deref().map(|t| fill(t, v)).unwrap_or_else(|| default_campaign_value(v)));
    push("utm_content", p.content.as_deref().map(|t| fill(t, v)).unwrap_or_default());
    push("utm_term", p.term.as_deref().map(|t| fill(t, v)).unwrap_or_default());
    for c in &p.custom {
        push(&c.name, fill(&c.value, v));
    }
    out
}

/// The parameters in force for a campaign or flow (None: its links aren't tagged), and the domains to skip.
pub fn effective(defaults: Option<&Defaults>, over: Option<&Override>) -> Option<(Params, Vec<String>)> {
    let skip = defaults.map(|d| d.skip_domains.clone()).unwrap_or_default();
    match over {
        Some(Override::Off) => None,
        Some(Override::Custom { params }) => Some((params.clone(), skip)),
        None => defaults.filter(|d| d.enabled).map(|d| (d.params.clone(), skip)),
    }
}

/// A stored override; anything unreadable counts as "use the defaults".
pub fn parse_override(v: Option<&Value>) -> Option<Override> {
    v.filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v.clone()).ok())
}

fn parse_defaults(v: Option<&Value>) -> Option<Defaults> {
    v.filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Resolved tags as they ride on content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frozen {
    pub params: Vec<(String, String)>,
    #[serde(default)]
    pub skip_domains: Vec<String>,
}

/// Put `frozen` on `content` (or take the key off when None). Returns whether it changed.
pub fn set_on(content: &mut Value, frozen: Option<&Frozen>) -> bool {
    let Some(o) = content.as_object_mut() else { return false };
    match frozen {
        Some(f) => {
            let v = json!(f);
            if o.get(CONTENT_KEY) == Some(&v) {
                return false;
            }
            o.insert(CONTENT_KEY.into(), v);
            true
        }
        None => o.remove(CONTENT_KEY).is_some(),
    }
}

/// The day in the account's time zone.
pub fn local_date(account_tz: &str, at: DateTime<Utc>) -> String {
    let tz: chrono_tz::Tz = account_tz.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    at.with_timezone(&tz).format("%Y-%m-%d").to_string()
}

/// Loads a flow with its account's link tag settings in the one statement that reads the flow
/// where its messages are sent (see `flows::run_step`), into `FlowRow::link_tag_defaults`.
pub const FLOW_WITH_DEFAULTS_SQL: &str = "SELECT f.*, (SELECT jsonb_build_object('enabled', s.enabled, 'params', s.params, 'skip_domains', to_jsonb(s.skip_domains))
     FROM link_tag_settings s WHERE s.account_id = f.account_id) AS link_tag_defaults FROM flows f WHERE f.id = $1";

pub async fn load_defaults(db: &mut PgConnection, account_id: Uuid) -> sqlx::Result<Option<Defaults>> {
    let row: Option<(bool, Value, Vec<String>)> = sqlx::query_as("SELECT enabled, params, skip_domains FROM link_tag_settings WHERE account_id = $1").bind(account_id).fetch_optional(&mut *db).await?;
    Ok(row.map(|(enabled, params, skip_domains)| Defaults { enabled, params: serde_json::from_value(params).unwrap_or_default(), skip_domains }))
}

/// A campaign's resolved tags for one version (`{version}`), or None when its links aren't tagged.
fn campaign_frozen(defaults: Option<&Defaults>, c: &CampaignRow, over: Option<&Override>, version: Option<&str>, send_date: &str) -> Option<Frozen> {
    let (params, skip) = effective(defaults, over)?;
    let vars = Vars { campaign: Some((c.id, &c.name)), flow: None, message_name: &c.name, channel: &c.channel, send_date: send_date.to_string(), version };
    Some(Frozen { params: resolve(&params, &vars), skip_domains: skip })
}

/// At send start, on the connection that holds the campaign lock: write the campaign's resolved tags
/// onto its content and each A/B version's (or take stale ones off). Reads the content as it is now,
/// after other steps of the same transaction changed it. No settings and no stale key: no write.
/// Only the first time a send is prepared: a retry, or the winner send after an A/B test, keeps the
/// tags the first recipients got.
pub async fn freeze_campaign(db: &mut PgConnection, campaign_id: Uuid) -> sqlx::Result<()> {
    let c: CampaignRow = sqlx::query_as("SELECT * FROM campaigns WHERE id = $1").bind(campaign_id).fetch_one(&mut *db).await?;
    let started: bool = sqlx::query_scalar("SELECT audience_fixed_at IS NOT NULL OR EXISTS (SELECT 1 FROM messages WHERE campaign_id = $1 AND NOT is_test) FROM campaigns WHERE id = $1")
        .bind(campaign_id)
        .fetch_one(&mut *db)
        .await?;
    if started {
        return Ok(());
    }
    let (over, tz): (Option<Value>, String) = sqlx::query_as("SELECT c.link_tags, a.timezone FROM campaigns c JOIN accounts a ON a.id = c.account_id WHERE c.id = $1").bind(campaign_id).fetch_one(&mut *db).await?;
    let defaults = load_defaults(db, c.account_id).await?;
    let over = parse_override(over.as_ref());
    let send_date = local_date(&tz, c.sent_at.unwrap_or_else(Utc::now));
    let ab = crate::campaigns::ab_test(&c);
    let mut content = c.content.clone();
    let mut changed = set_on(&mut content, campaign_frozen(defaults.as_ref(), &c, over.as_ref(), ab.as_ref().map(|_| "a"), &send_date).as_ref());
    let mut ab_test = c.ab_test.clone();
    if let Some(variants) = ab_test.as_mut().and_then(|t| t.get_mut("variants")).and_then(|v| v.as_array_mut()) {
        for v in variants {
            let id = v["id"].as_str().unwrap_or("").to_string();
            let frozen = campaign_frozen(defaults.as_ref(), &c, over.as_ref(), Some(&id), &send_date);
            if let Some(vc) = v.get_mut("content") {
                changed |= set_on(vc, frozen.as_ref());
            }
        }
    }
    if changed {
        sqlx::query("UPDATE campaigns SET content = $2, ab_test = $3 WHERE id = $1").bind(campaign_id).bind(&content).bind(&ab_test).execute(&mut *db).await?;
    }
    Ok(())
}

/// A test send's content with the tags the real send would use (today's date).
pub async fn for_test(state: &AppState, c: &CampaignRow, version: Option<&str>, content: &Value) -> AppResult<Value> {
    let mut conn = state.db.acquire().await?;
    let over: Option<Value> = sqlx::query_scalar("SELECT link_tags FROM campaigns WHERE id = $1").bind(c.id).fetch_one(&mut *conn).await?;
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(c.account_id).fetch_one(&mut *conn).await?;
    let defaults = load_defaults(&mut conn, c.account_id).await?;
    let version = crate::campaigns::ab_test(c).map(|_| version.unwrap_or("a"));
    let frozen = campaign_frozen(defaults.as_ref(), c, parse_override(over.as_ref()).as_ref(), version, &local_date(&tz, Utc::now()));
    let mut out = content.clone();
    set_on(&mut out, frozen.as_ref());
    Ok(out)
}

/// A flow step's content with its tags, from the settings loaded with the flow row: no query.
pub fn for_flow_step(account: &Account, flow: &FlowRow, node_name: Option<&str>, channel: &str, mut content: Value) -> Value {
    let defaults = parse_defaults(flow.link_tag_defaults.as_ref());
    let frozen = effective(defaults.as_ref(), parse_override(flow.link_tags.as_ref()).as_ref()).map(|(params, skip)| {
        let message_name = node_name.filter(|n| !n.trim().is_empty()).unwrap_or(&flow.name);
        let vars = Vars { campaign: None, flow: Some((flow.id, &flow.name)), message_name, channel, send_date: local_date(&account.timezone, Utc::now()), version: None };
        Frozen { params: resolve(&params, &vars), skip_domains: skip }
    });
    set_on(&mut content, frozen.as_ref());
    content
}

/// Adds tags to the destination of a link that click tracking wraps. Built once per message.
pub struct Tagger {
    params: Vec<(String, String)>,
    skip_domains: Vec<String>,
    own_hosts: Vec<String>,
}

fn host_of(u: &str) -> Option<String> {
    url::Url::parse(u).ok()?.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase())
}

impl Tagger {
    /// The tags riding on `content`, checked again (content can be typed by hand), or None.
    pub fn from_content(state: &AppState, account: &Account, content: &Value) -> Option<Tagger> {
        let f: Frozen = serde_json::from_value(content.get(CONTENT_KEY)?.clone()).ok()?;
        let params: Vec<(String, String)> = f
            .params
            .into_iter()
            .filter(|(n, v)| (STANDARD_NAMES.contains(&n.as_str()) || CUSTOM_NAME.is_match(n)) && !v.is_empty() && v.chars().count() <= MAX_VALUE && !v.chars().any(char::is_control))
            .take(STANDARD_NAMES.len() + MAX_CUSTOM)
            .collect();
        if params.is_empty() {
            return None;
        }
        // DumbMail's own links (tracking, unsubscribe, preferences, view online) are never tagged.
        let mut own_hosts: Vec<String> = [Some(state.cfg.public_url()), Some(state.cfg.link_url()), state.cfg.site_url()].into_iter().flatten().filter_map(host_of).collect();
        own_hosts.extend(account.link_domain.as_deref().map(str::to_ascii_lowercase));
        Some(Tagger { params, skip_domains: f.skip_domains.into_iter().take(MAX_SKIP_DOMAINS).collect(), own_hosts })
    }

    /// A tagger for tests and previews.
    pub fn new(params: Vec<(String, String)>, skip_domains: Vec<String>, own_hosts: Vec<String>) -> Tagger {
        Tagger { params, skip_domains, own_hosts }
    }

    /// `url` with the tags it doesn't already have, or None to leave it as it is: only http(s) links,
    /// not DumbMail's own or a skipped domain's. The existing query and fragment stay exactly as they were.
    pub fn tag(&self, url: &str) -> Option<String> {
        let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
        if !(lower.starts_with("https://") || lower.starts_with("http://")) {
            return None;
        }
        let host = host_of(url)?;
        let matches = |d: &String| host == *d || host.ends_with(&format!(".{d}"));
        if self.own_hosts.iter().any(|h| host == *h) || self.skip_domains.iter().any(matches) {
            return None;
        }
        let (base, fragment) = match url.find('#') {
            Some(i) => (&url[..i], &url[i..]),
            None => (url, ""),
        };
        let query = base.split_once('?').map(|(_, q)| q).unwrap_or("");
        let present: Vec<String> = query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let name = kv.split('=').next().unwrap_or("");
                url::form_urlencoded::parse(format!("{name}=").as_bytes()).next().map(|(k, _)| k.to_ascii_lowercase()).unwrap_or_default()
            })
            .collect();
        let add: Vec<String> = self
            .params
            .iter()
            .filter(|(n, _)| !present.contains(n))
            .map(|(n, v)| format!("{n}={}", url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()))
            .collect();
        if add.is_empty() {
            return None;
        }
        let sep = if !base.contains('?') {
            "?"
        } else if base.ends_with('?') || base.ends_with('&') {
            ""
        } else {
            "&"
        };
        Some(format!("{base}{sep}{}{fragment}", add.join("&")))
    }
}

impl crate::render::LinkHook for Tagger {
    fn destination(&self, url: &str) -> Option<String> {
        self.tag(url)
    }
}

// ---------- API ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultsBody {
    pub enabled: Option<bool>,
    pub params: Option<Params>,
    pub skip_domains: Option<Vec<String>>,
}

pub fn variables_json() -> Value {
    json!(VARIABLES.iter().map(|(n, d)| json!({"name": n, "description": d})).collect::<Vec<_>>())
}

fn defaults_json(d: &Defaults) -> Value {
    json!({
        "enabled": d.enabled,
        "params": d.params,
        "skip_domains": d.skip_domains,
        "fallbacks": {"source": DEFAULT_SOURCE, "medium": DEFAULT_MEDIUM, "campaign": "The campaign or flow name, slugged, and the first 8 characters of its id"},
        "variables": variables_json(),
    })
}

pub async fn get_defaults(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let mut conn = state.db.acquire().await?;
    Ok(defaults_json(&load_defaults(&mut conn, account_id).await?.unwrap_or_default()))
}

/// Save the account's settings (fields left out keep their value). Returns the settings and what changed.
pub async fn put_defaults(state: &AppState, account_id: Uuid, b: DefaultsBody) -> AppResult<(Value, Vec<&'static str>)> {
    let params = b.params.map(checked_params).transpose()?;
    let skip = b.skip_domains.map(checked_domains).transpose()?;
    let mut changed = Vec::new();
    if b.enabled.is_some() {
        changed.push("enabled");
    }
    if params.is_some() {
        changed.push("params");
    }
    if skip.is_some() {
        changed.push("skip_domains");
    }
    let row: (bool, Value, Vec<String>) = sqlx::query_as(
        "INSERT INTO link_tag_settings (account_id, enabled, params, skip_domains) VALUES ($1, COALESCE($2, false), COALESCE($3, '{}'), COALESCE($4, '{}'))
         ON CONFLICT (account_id) DO UPDATE SET enabled = COALESCE($2, link_tag_settings.enabled), params = COALESCE($3, link_tag_settings.params),
            skip_domains = COALESCE($4, link_tag_settings.skip_domains), updated_at = now()
         RETURNING enabled, params, skip_domains",
    )
    .bind(account_id)
    .bind(b.enabled)
    .bind(params.map(|p| json!(p)))
    .bind(skip)
    .fetch_one(&state.db)
    .await?;
    let d = Defaults { enabled: row.0, params: serde_json::from_value(row.1).unwrap_or_default(), skip_domains: row.2 };
    Ok((defaults_json(&d), changed))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverrideBody {
    /// default (use the account's settings), off, or custom (with params).
    pub mode: String,
    pub params: Option<Params>,
}

fn parse_body(b: OverrideBody) -> AppResult<Option<Override>> {
    match (b.mode.as_str(), b.params) {
        ("default", None) => Ok(None),
        ("off", None) => Ok(Some(Override::Off)),
        ("custom", Some(p)) => Ok(Some(Override::Custom { params: checked_params(p)? })),
        ("custom", None) => Err(AppError::bad("custom needs params")),
        ("default" | "off", Some(_)) => Err(AppError::bad("params only go with mode custom")),
        _ => Err(AppError::bad("mode must be default, off or custom")),
    }
}

fn override_json(o: Option<&Override>) -> Value {
    match o {
        None => json!({"mode": "default"}),
        Some(o) => json!(o),
    }
}

/// What a campaign's links get: its choice, the account's, and the parameters with the variables
/// filled in as they'd be if it went out now (`preview`, a readable line too).
pub async fn campaign_tags(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let c = crate::campaigns::load(&state.db, account_id, id).await?;
    let mut conn = state.db.acquire().await?;
    let (over, tz): (Option<Value>, String) = sqlx::query_as("SELECT c.link_tags, a.timezone FROM campaigns c JOIN accounts a ON a.id = c.account_id WHERE c.id = $1").bind(id).fetch_one(&mut *conn).await?;
    let defaults = load_defaults(&mut conn, account_id).await?;
    let over = parse_override(over.as_ref());
    let version = crate::campaigns::ab_test(&c).map(|_| "a");
    let frozen = campaign_frozen(defaults.as_ref(), &c, over.as_ref(), version, &local_date(&tz, c.sent_at.unwrap_or_else(Utc::now)));
    Ok(tags_json(over.as_ref(), defaults.as_ref(), frozen))
}

fn tags_json(over: Option<&Override>, defaults: Option<&Defaults>, frozen: Option<Frozen>) -> Value {
    let line = frozen.as_ref().map(|f| f.params.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("&"));
    json!({
        "override": override_json(over),
        "account_enabled": defaults.is_some_and(|d| d.enabled),
        "tagged": frozen.is_some(),
        "params": frozen.as_ref().map(|f| f.params.iter().map(|(n, v)| json!({"name": n, "value": v})).collect::<Vec<_>>()).unwrap_or_default(),
        "skip_domains": frozen.as_ref().map(|f| f.skip_domains.clone()).unwrap_or_default(),
        "preview": line,
        "variables": variables_json(),
    })
}

/// Set a campaign's choice, while the campaign can still be edited by this caller (checked in the write).
pub async fn set_campaign(state: &AppState, account_id: Uuid, id: Uuid, b: OverrideBody, editable: &[&str]) -> AppResult<Value> {
    let over = parse_body(b)?;
    let allowed: Vec<String> = editable.iter().map(|s| s.to_string()).collect();
    let n = sqlx::query("UPDATE campaigns SET link_tags = $3, updated_at = now() WHERE id = $1 AND account_id = $2 AND status = ANY($4)")
        .bind(id)
        .bind(account_id)
        .bind(over.as_ref().map(|o| json!(o)))
        .bind(&allowed)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        let c = crate::campaigns::load(&state.db, account_id, id).await?;
        return Err(AppError::Conflict(format!("this campaign is {} and its link tags can't be changed now", c.status)));
    }
    campaign_tags(state, account_id, id).await
}

pub async fn flow_tags(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let f = crate::api::messaging::load_flow(state, account_id, id).await?;
    let mut conn = state.db.acquire().await?;
    let defaults = load_defaults(&mut conn, account_id).await?;
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&mut *conn).await?;
    let over = parse_override(f.link_tags.as_ref());
    let frozen = effective(defaults.as_ref(), over.as_ref()).map(|(params, skip)| {
        let vars = Vars { campaign: None, flow: Some((f.id, &f.name)), message_name: &f.name, channel: "email", send_date: local_date(&tz, Utc::now()), version: None };
        Frozen { params: resolve(&params, &vars), skip_domains: skip }
    });
    let mut v = tags_json(over.as_ref(), defaults.as_ref(), frozen);
    v["note"] = json!("Shown for an email step named like the flow; each step uses its own name for {message_name} and its channel for {channel}.");
    Ok(v)
}

pub async fn set_flow(state: &AppState, account_id: Uuid, id: Uuid, b: OverrideBody) -> AppResult<Value> {
    let over = parse_body(b)?;
    let n = sqlx::query("UPDATE flows SET link_tags = $3, updated_at = now() WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .bind(over.as_ref().map(|o| json!(o)))
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("flow"));
    }
    flow_tags(state, account_id, id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagger() -> Tagger {
        Tagger::new(
            vec![("utm_source".into(), "dumbmail".into()), ("utm_medium".into(), "email".into()), ("utm_campaign".into(), "black friday".into())],
            vec!["skip.test".into()],
            vec!["links.test".into()],
        )
    }

    #[test]
    fn tags_web_links_and_keeps_what_is_there() {
        let t = tagger();
        assert_eq!(t.tag("https://shop.test/a").unwrap(), "https://shop.test/a?utm_source=dumbmail&utm_medium=email&utm_campaign=black+friday");
        assert_eq!(t.tag("https://shop.test/a?x=1&y=%20z#top").unwrap(), "https://shop.test/a?x=1&y=%20z&utm_source=dumbmail&utm_medium=email&utm_campaign=black+friday#top");
        assert_eq!(t.tag("https://shop.test/a?utm_source=ig").unwrap(), "https://shop.test/a?utm_source=ig&utm_medium=email&utm_campaign=black+friday", "an existing parameter is kept");
        assert_eq!(t.tag("https://shop.test/a?UTM_SOURCE=ig&utm_medium=x&utm_campaign=y"), None, "nothing to add");
        assert_eq!(t.tag("https://shop.test/?").unwrap(), "https://shop.test/?utm_source=dumbmail&utm_medium=email&utm_campaign=black+friday");
        assert_eq!(t.tag("https://shop.test/#/route?x=1").unwrap(), "https://shop.test/?utm_source=dumbmail&utm_medium=email&utm_campaign=black+friday#/route?x=1", "the fragment stays as is");
        assert_eq!(t.tag("HTTPS://Shop.Test/x").unwrap(), "HTTPS://Shop.Test/x?utm_source=dumbmail&utm_medium=email&utm_campaign=black+friday");
    }

    #[test]
    fn leaves_other_links_alone() {
        let t = tagger();
        for u in ["mailto:a@b.test", "tel:+15035550100", "https://links.test/u/abc", "https://skip.test/x", "https://www.skip.test/x", "ftp://shop.test/x", "/relative", "https://"] {
            assert_eq!(t.tag(u), None, "{u}");
        }
        assert!(t.tag("https://notskip.test/x").is_some(), "only the domain and its subdomains are skipped");
    }

    #[test]
    fn values_are_checked() {
        let ok = checked_params(Params { campaign: Some(" {campaign_name}-{send_date} ".into()), custom: vec![Custom { name: "ref".into(), value: "{channel}".into() }], ..Default::default() }).unwrap();
        assert_eq!(ok.campaign.as_deref(), Some("{campaign_name}-{send_date}"));
        assert!(checked_params(Params { source: Some("{nope}".into()), ..Default::default() }).is_err());
        assert!(checked_params(Params { source: Some("a{b".into()), ..Default::default() }).is_err());
        assert!(checked_params(Params { source: Some("x".repeat(201)), ..Default::default() }).is_err());
        assert!(checked_params(Params { custom: vec![Custom { name: "Bad-Name".into(), value: "x".into() }], ..Default::default() }).is_err());
        assert!(checked_params(Params { custom: vec![Custom { name: "utm_source".into(), value: "x".into() }], ..Default::default() }).is_err());
        let eleven = (0..11).map(|i| Custom { name: format!("p{i}"), value: "x".into() }).collect();
        assert!(checked_params(Params { custom: eleven, ..Default::default() }).is_err());
        assert_eq!(checked_domains(vec!["https://www.Shop.test/path".into(), "shop.test".into(), " ".into()]).unwrap(), vec!["shop.test"]);
        assert!(checked_domains(vec!["not a domain".into()]).is_err());
    }

    #[test]
    fn resolves_variables_and_defaults() {
        let id: Uuid = "0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000".parse().unwrap();
        let v = Vars { campaign: Some((id, "Black Friday: 40% off!")), flow: None, message_name: "Black Friday: 40% off!", channel: "email", send_date: "2026-11-27".into(), version: Some("b") };
        let p = Params { content: Some("{version}-{send_date}".into()), term: Some("{flow_name}".into()), ..Default::default() };
        assert_eq!(
            resolve(&p, &v),
            vec![("utm_source".into(), "dumbmail".into()), ("utm_medium".into(), "email".into()), ("utm_campaign".into(), "black-friday-40-off-0190aaaa".into()), ("utm_content".into(), "B-2026-11-27".into())],
            "the empty term is left out"
        );
    }
}
