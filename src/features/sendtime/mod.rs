//! Send-time rules: each person's time zone, campaigns sent at each person's local time or best
//! hour, gradual sends, smart sending for campaigns, and SMS quiet hours in the recipient's zone.
//!
//! The send path calls in here: `campaigns::prepare_send` plans release times with
//! [`plan::plan_campaign`], `campaigns::send_batch` applies smart sending with
//! [`plan::recently_messaged`], `sending::send` asks [`settings::sms_quiet_until`] before a text,
//! and `messaging::launch` asks [`plan::launch_start`] when a local-time campaign starts.

pub mod clock;
pub mod options;
pub mod plan;
pub mod settings;
pub mod zone;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::campaigns::Audience;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::models::{Profile, PROFILE_COLS};
use crate::state::AppState;

pub use options::SendOptions;
pub use plan::recently_messaged;
pub use settings::{sms_hours, sms_quiet_until, SmsHours};

pub struct Module;

const DOC: &str = include_str!("../../../docs/SENDTIME.md");

/// JSON Schema for a campaign's `send_options`, shared by the campaign tools.
pub fn send_options_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "description": "How the campaign goes out. timing: all_at_once (default) | local_time (at local_time, HH:MM, in each person's time zone on the send day; late: next_day (default) or immediately for people whose time had already passed more than 15 minutes before the send was asked for; anyone whose time passed less than 15 minutes before gets it right away) | best_time (each person's best hour from their own past opens and clicks, between window_start and window_end their time, within within_hours of the send time; people without enough history get the account's best hour, then 10:00; anyone whose window doesn't open within within_hours gets it when it next opens, after that) | gradual (spread over spread_hours, one equal batch an hour). smart_sending: true skips people who got a message on this channel in the last smart_sending_hours (default 16). null clears all options. See dumbmail://docs/sendtime.",
        "properties": {
            "timing": {"enum": ["all_at_once", "local_time", "best_time", "gradual"]},
            "local_time": {"type": "string", "pattern": "^\\d{1,2}:\\d{2}$"},
            "late": {"enum": ["next_day", "immediately"]},
            "window_start": {"type": "string", "pattern": "^\\d{1,2}:\\d{2}$"},
            "window_end": {"type": "string", "pattern": "^\\d{1,2}:\\d{2}$"},
            "within_hours": {"type": "integer", "minimum": 6, "maximum": 72},
            "spread_hours": {"type": "integer", "minimum": 2, "maximum": 72},
            "smart_sending": {"type": "boolean"},
            "smart_sending_hours": {"type": "integer", "minimum": 1, "maximum": 168}
        },
        "additionalProperties": false
    })
}

/// Validate send options for storing on a campaign (None/null clears them).
pub fn checked(v: Option<&Value>) -> AppResult<Option<Value>> {
    let Some(v) = v else { return Ok(None) };
    let Some(o) = options::parse(v)? else { return Ok(None) };
    Ok(Some(serde_json::to_value(o)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewBody {
    campaign_id: Option<Uuid>,
    channel: Option<String>,
    audience: Option<Value>,
    /// Missing: the campaign's. `null`: everyone at once.
    #[serde(default, deserialize_with = "present")]
    send_options: Option<Value>,
    send_at: Option<DateTime<Utc>>,
}

/// Deserializes a present field (even `null`) as Some, so a missing field stays None.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// Estimate when a send goes out. Missing fields come from the campaign when `campaign_id` is given.
async fn preview(state: &AppState, account_id: Uuid, b: PreviewBody) -> AppResult<Value> {
    let c = match b.campaign_id {
        Some(id) => Some(crate::campaigns::load(&state.db, account_id, id).await?),
        None => None,
    };
    let channel = b.channel.or_else(|| c.as_ref().map(|c| c.channel.clone())).unwrap_or_else(|| "email".into());
    if channel != "email" && channel != "sms" {
        return Err(AppError::bad("channel must be email or sms"));
    }
    let audience_v = b.audience.or_else(|| c.as_ref().map(|c| c.audience.clone())).ok_or_else(|| AppError::bad("give an audience or a campaign_id"))?;
    let audience: Audience = serde_json::from_value(audience_v).map_err(|e| AppError::bad(format!("invalid audience: {e}")))?;
    if audience.include.is_empty() {
        return Ok(json!({"total": 0, "buckets": [], "description": "Choose who receives it first."}));
    }
    let mut opts = match b.send_options {
        Some(v) => options::parse(&v)?,
        None => options::of(c.as_ref().and_then(|c| c.send_options.as_ref())),
    };
    // A send time given here picks the day afresh.
    if let (Some(o), Some(_)) = (opts.as_mut(), b.send_at) {
        o.local_date = None;
    }
    let send_at = b.send_at.or_else(|| c.as_ref().filter(|c| c.status == "scheduled").and_then(|c| c.send_at));
    plan::preview(state, account_id, &channel, &audience, opts, send_at).await
}

async fn preview_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<PreviewBody>) -> AppResult<Json<Value>> {
    Ok(Json(preview(&state, ctx.account_id, b).await?))
}

async fn progress_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(plan::progress(&state.db, ctx.account_id, id).await?))
}

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings::settings_json(&state.db, ctx.account_id).await?))
}

async fn update_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<settings::SettingsBody>) -> AppResult<Json<Value>> {
    Ok(Json(settings::update(&state.db, ctx.account_id, b).await?))
}

/// A profile's time zone and where it came from.
pub async fn profile_zone(state: &AppState, account_id: Uuid, profile_id: Uuid) -> AppResult<Value> {
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1 AND p.account_id = $2");
    let p: Profile = sqlx::query_as(&q).bind(profile_id).bind(account_id).fetch_optional(&state.db).await?.ok_or_else(|| AppError::not_found("profile"))?;
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let (tz, source) = zone::resolve(&p, crate::sending::account_tz(&account));
    let local = Utc::now().with_timezone(&tz);
    Ok(json!({"profile_id": p.id, "timezone": tz.name(), "source": source.as_str(), "local_time": local.format("%Y-%m-%d %H:%M").to_string()}))
}

async fn profile_zone_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(profile_zone(&state, ctx.account_id, id).await?))
}

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("`{k}` must be an id")))
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "sendtime"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/sendtime/preview", post(preview_h))
            .route("/api/sendtime/settings", get(settings_h).patch(update_settings_h))
            .route("/api/campaigns/{id}/send-progress", get(progress_h))
            .route("/api/profiles/{id}/timezone", get(profile_zone_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let audience = json!({"type": "object", "description": "{include: [{type: list|segment, id}], exclude: [...]}"});
        vec![
            tool("preview_send_plan", "Preview when a campaign goes out", "Estimate how many recipients get the campaign when, for its send options (or the send_options given here): 15-minute buckets with their main time zones, from a random sample of up to 2,000 recipients scaled to the audience. Also counts who smart sending would skip right now. Nothing is sent or saved.",
                obj(json!({"campaign_id": {"type": "string"}, "send_options": send_options_schema(), "send_at": {"type": "string", "format": "date-time", "description": "When sending starts (for local_time: the day). Defaults to the campaign's schedule, or now."}, "channel": {"enum": ["email", "sms"]}, "audience": audience}), &[]), true, false),
            tool("get_send_progress", "Campaign send progress by time zone", "For a campaign that is sending or sent: messages per release time (with their time zones) waiting, sent, skipped by smart sending, skipped for other reasons and failed, plus the next release and skip reasons.",
                obj(json!({"campaign_id": {"type": "string"}}), &["campaign_id"]), true, false),
            tool("get_send_settings", "Get SMS quiet hours", "The hours texts may go out in each recipient's own time zone (default 09:00-20:00; never wider than 08:00-21:00), using their phone number's zone (a profile time zone can only narrow it). Texts outside the window wait for it to open. sms_zone_conflict: what happens when the phone's and profile's zones never share texting hours.", obj(json!({}), &[]), true, false),
            tool("update_send_settings", "Set SMS quiet hours", "Narrow the hours texts may go out in each recipient's time zone (must stay within 08:00-21:00 and be at least an hour long), and choose what happens when someone's profile time zone and phone number never share texting hours: phone (default) uses the phone number's hours, skip doesn't text them. Either way the message notes it.",
                obj(json!({"sms_window_start": {"type": "string", "pattern": "^\\d{1,2}:\\d{2}$"}, "sms_window_end": {"type": "string", "pattern": "^\\d{1,2}:\\d{2}$"}, "sms_zone_conflict": {"enum": ["phone", "skip"]}}), &[]), false, false),
            tool("get_profile_timezone", "Get a person's time zone", "The time zone DumbMail uses for a profile and where it came from: profile (their timezone property), region, zip, country, phone (country or area code) or account (no other clue).",
                obj(json!({"profile_id": {"type": "string"}}), &["profile_id"]), true, false),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        let acct = ctx.account_id;
        let r = match name {
            "preview_send_plan" => async {
                let send_at = a["send_at"].as_str().map(|s| s.parse::<DateTime<Utc>>().map_err(|_| AppError::bad("send_at must be an ISO 8601 date-time"))).transpose()?;
                let campaign_id = match a["campaign_id"].as_str() {
                    Some(_) => Some(uuid_arg(a, "campaign_id")?),
                    None => None,
                };
                preview(state, acct, PreviewBody { campaign_id, channel: a["channel"].as_str().map(String::from), audience: a.get("audience").cloned(), send_options: a.get("send_options").cloned(), send_at }).await
            }
            .await,
            "get_send_progress" => async { plan::progress(&state.db, acct, uuid_arg(a, "campaign_id")?).await }.await,
            "get_send_settings" => settings::settings_json(&state.db, acct).await.map_err(AppError::from),
            "update_send_settings" => async { settings::update(&state.db, acct, from(a.clone())?).await }.await,
            "get_profile_timezone" => async { profile_zone(state, acct, uuid_arg(a, "profile_id")?).await }.await,
            _ => return None,
        };
        Some(r)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc { slug: "sendtime", name: "Send times", description: "Local-time and best-time campaign sends, gradual sends, smart sending, time zones and SMS quiet hours", text: DOC }]
    }
}
