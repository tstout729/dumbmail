//! Push notifications as a third channel, next to email and texts: web push through the website
//! snippet (no app needed) and mobile push to the customer's own iOS and Android apps through their
//! own Apple (APNs) and Firebase (FCM) credentials.
//!
//! `sending::send` hands `push` messages to [`send::send`]; campaigns and flows otherwise treat push
//! like texts (audiences use [`send::REACHABLE_SQL`]). See docs/PUSH.md.

pub mod client;
pub mod content;
pub mod crypto;
pub mod limit;
pub mod providers;
pub mod send;
pub mod settings;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Feature, McpDoc};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::team::access::{public, rule, Access, Perm, RouteRule, ToolRule};
use crate::mcp::{from, obj, tool};
use crate::models::{Profile, PROFILE_COLS};
use crate::sending::Outcome;
use crate::state::AppState;

pub struct Module;

/// Settings read like the account's other settings; credentials are changed by Admins and the Owner
/// (settings write); tests need the test-send permission (and, to reach someone else's devices, the
/// campaign-send one, as test texts do); a person's devices are personal data.
static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/push/settings", Perm::AccountRead),
    rule("PATCH", "/api/push/settings", Perm::SettingsWrite),
    rule("POST", "/api/push/identity-secret", Perm::SettingsWrite),
    rule("POST", "/api/push/test-devices", Perm::TestSend),
    rule("POST", "/api/push/test", Perm::TestSend),
    rule("GET", "/api/push/profiles/{id}", Perm::ProfilesRead),
    rule("PUT", "/api/push/profiles/{id}/opt-out", Perm::AudienceWrite),
    rule("GET", "/api/push/report", Perm::MessagingRead),
    public("GET", "/push/sw.js"),
    public("POST", "/client/push/sign-out"),
    public("OPTIONS", "/client/push/sign-out"),
    public("POST", "/client/push/devices/sign-out"),
    public("OPTIONS", "/client/push/devices/sign-out"),
    public("GET", "/client/push.js"),
    public("POST", "/client/push/subscribe"),
    public("OPTIONS", "/client/push/subscribe"),
    public("POST", "/client/push/unsubscribe"),
    public("OPTIONS", "/client/push/unsubscribe"),
    public("POST", "/client/push/delivered"),
    public("OPTIONS", "/client/push/delivered"),
    public("POST", "/client/push/opened"),
    public("OPTIONS", "/client/push/opened"),
    public("POST", "/client/push/devices"),
    public("OPTIONS", "/client/push/devices"),
    public("POST", "/client/push/devices/remove"),
    public("OPTIONS", "/client/push/devices/remove"),
    public("GET", "/p/c/{token}"),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "get_push_settings", perm: Perm::AccountRead },
    ToolRule { name: "update_push_settings", perm: Perm::SettingsWrite },
    ToolRule { name: "rotate_push_identity_secret", perm: Perm::SettingsWrite },
    ToolRule { name: "send_test_push", perm: Perm::TestSend },
    ToolRule { name: "get_profile_push", perm: Perm::ProfilesRead },
    ToolRule { name: "set_push_opt_out", perm: Perm::AudienceWrite },
    ToolRule { name: "get_push_report", perm: Perm::MessagingRead },
];

const DOC: &str = include_str!("../../../docs/PUSH.md");

/// JSON Schema for push content (campaign `content` and flow `push` steps).
pub fn content_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "description": "Push content. title (up to 100 characters) and body (up to 500) take personalization tags like {{ first_name }}; coupon codes aren't supported. See dumbmail://docs/push.",
        "properties": {
            "title": {"type": "string", "maxLength": 100},
            "body": {"type": "string", "maxLength": 500},
            "url": {"type": "string", "description": "Where a tap goes (https). Default: the account's website."},
            "icon": {"type": "string", "description": "Web only. Default: the account's logo."},
            "image": {"type": "string", "description": "Large picture (Chrome, Edge, Android; iOS apps with a notification service extension)."},
            "actions": {"type": "array", "maxItems": 2, "items": {"type": "object", "properties": {"title": {"type": "string", "maxLength": 40}, "url": {"type": "string"}}, "required": ["title", "url"], "additionalProperties": false}},
            "platforms": {"type": "array", "items": {"enum": ["web", "ios", "android"]}, "description": "Which devices get it. Empty or missing: all."},
            "ttl_seconds": {"type": "integer", "minimum": 0, "maximum": 2419200, "description": "How long an offline device's push service keeps it. Default 86400."},
            "urgency": {"enum": ["very_low", "low", "normal", "high"]},
            "topic": {"type": "string", "maxLength": 32, "description": "A newer push with the same topic replaces an undelivered older one."},
            "ios_sound": {"type": "string", "description": "Default \"default\"; empty for silent."},
            "ios_badge": {"type": "integer", "minimum": 0, "maximum": 9999},
            "android_channel_id": {"type": "string"}
        }})
}

fn settings_schema() -> Value {
    json!({
        "web_enabled": {"type": "boolean", "description": "Turn web push on for the website snippet. The service worker file must be on the site first (see get_push_settings: web.service_worker_url and docs)."},
        "sw_path": {"type": "string", "description": "Where the service worker is on the site, e.g. /dumbmail-sw.js."},
        "prompt": {"type": "object", "additionalProperties": false, "description": "The in-page card shown before the browser's own prompt. Replaced whole.",
            "properties": {
                "enabled": {"type": "boolean", "description": "Show it automatically by these rules. false: only the site's own button (dumbmail.notifications.prompt()) asks."},
                "title": {"type": "string", "maxLength": 80}, "body": {"type": "string", "maxLength": 300},
                "allow_text": {"type": "string", "maxLength": 30}, "deny_text": {"type": "string", "maxLength": 30},
                "page_views": {"type": "integer", "minimum": 1, "maximum": 50}, "delay_seconds": {"type": "integer", "minimum": 0, "maximum": 600},
                "url_contains": {"type": "array", "maxItems": 20, "items": {"type": "string"}}, "url_excludes": {"type": "array", "maxItems": 20, "items": {"type": "string"}},
                "frequency_days": {"type": "integer", "minimum": 0, "maximum": 365}, "position": {"enum": ["top", "bottom"]}
            }},
        "apns": {"type": ["object", "null"], "additionalProperties": false, "description": "Apple push (token auth). null removes it.",
            "properties": {"key_id": {"type": "string"}, "team_id": {"type": "string"}, "bundle_id": {"type": "string"}, "production": {"type": "boolean", "description": "false for development builds (APNs sandbox). Default true."}, "key_p8": {"type": "string", "description": "The .p8 file's text. Leave out to keep the saved key."}},
            "required": ["key_id", "team_id", "bundle_id"]},
        "fcm": {"type": ["object", "null"], "additionalProperties": false, "description": "Firebase Cloud Messaging HTTP v1. null removes it.",
            "properties": {"service_account_json": {"type": "string", "description": "The service account key file's text."}}, "required": ["service_account_json"]}
    })
}

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings::settings_json(&state, ctx.account_id).await?))
}

async fn update_settings(state: &AppState, ctx: &Ctx, b: settings::SettingsBody) -> AppResult<Value> {
    let changed = settings::update(state, ctx.account_id, b).await?;
    if !changed.is_empty() {
        crate::features::team::audit(state, ctx, "push.settings_update", None, json!({"changed": changed})).await;
    }
    settings::settings_json(state, ctx.account_id).await
}

async fn update_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<settings::SettingsBody>) -> AppResult<Json<Value>> {
    Ok(Json(update_settings(&state, &ctx, b).await?))
}

async fn rotate_identity(state: &AppState, ctx: &Ctx) -> AppResult<Value> {
    let secret = settings::rotate_identity_secret(state, ctx.account_id).await?;
    crate::features::team::audit(state, ctx, "push.identity_secret_rotate", None, json!({})).await;
    Ok(json!({"identity_secret": secret, "note": "Shown once. Keep it on your server: it signs enrollment grants. Grants made with an older secret stop working now."}))
}

async fn identity_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(rotate_identity(&state, &ctx).await?))
}

#[derive(Deserialize)]
struct TestDeviceBody {
    subscription: Value,
}

/// The dashboard's own browser, as a test device for the signed-in member (never in an audience).
async fn test_device_h(State(state): State<AppState>, ctx: Ctx, headers: axum::http::HeaderMap, ApiJson(b): ApiJson<TestDeviceBody>) -> AppResult<Json<Value>> {
    let user = ctx.user_id.ok_or_else(|| AppError::bad("sign in to the dashboard to add this browser"))?;
    let endpoint = b.subscription["endpoint"].as_str().unwrap_or_default();
    let p256dh = b.subscription["keys"]["p256dh"].as_str().unwrap_or_default();
    let auth = b.subscription["keys"]["auth"].as_str().unwrap_or_default();
    if !providers::endpoint_allowed(&state, endpoint) || !crypto::check_subscription_keys(p256dh, auth) {
        return Err(AppError::bad("that isn't a browser push subscription"));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM push_devices WHERE account_id = $1 AND test_user_id = $2").bind(ctx.account_id).bind(user).fetch_one(&state.db).await?;
    if count >= 5 {
        sqlx::query("DELETE FROM push_devices WHERE id IN (SELECT id FROM push_devices WHERE account_id = $1 AND test_user_id = $2 ORDER BY last_seen_at LIMIT 1)").bind(ctx.account_id).bind(user).execute(&state.db).await?;
    }
    let ua = headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|u| u.chars().take(300).collect::<String>());
    // A browser that is someone's linked device stays theirs; this only adds it as a test device.
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO push_devices (id, account_id, test_user_id, kind, address, p256dh, auth_secret, user_agent, permission)
         VALUES ($1, $2, $3, 'web', $4, $5, $6, $7, 'granted')
         ON CONFLICT (account_id, kind, address) DO UPDATE SET p256dh = EXCLUDED.p256dh, auth_secret = EXCLUDED.auth_secret, status = 'active', failures = 0, last_seen_at = now(),
            permission = 'granted', test_user_id = EXCLUDED.test_user_id,
            version = push_devices.version + 1
         WHERE push_devices.profile_id IS NULL
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(ctx.account_id)
    .bind(user)
    .bind(endpoint)
    .bind(p256dh)
    .bind(auth)
    .bind(ua)
    .fetch_optional(&state.db)
    .await?;
    let id = id.ok_or_else(|| AppError::Conflict("this browser is already one of your audience's devices; send it a test from that person's profile".into()))?;
    Ok(Json(json!({"device_id": id})))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TestBody {
    /// Push content (default: a short sample).
    #[serde(default)]
    pub content: Option<Value>,
    /// "me" (default): your own test browsers. Or an email address of a profile with devices.
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub profile_id: Option<Uuid>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
}

async fn member_profile(state: &AppState, ctx: &Ctx) -> AppResult<Profile> {
    let user = ctx.user_id.ok_or_else(|| AppError::bad("with an API key, send the test to a person (to: their email) or a device_id"))?;
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1").bind(user).fetch_one(&state.db).await?;
    let input = crate::audience::ProfileInput { email: Some(email), ..Default::default() };
    // The member's own profile carries the test message (messages need one); it's never subscribed by this.
    Ok(match crate::audience::find_profile(&state.db, ctx.account_id, &input).await? {
        Some(p) => p,
        None => crate::audience::upsert_profile(&state.db, ctx.account_id, &input).await?.profile,
    })
}

async fn load_profile(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Profile> {
    let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1 AND p.account_id = $2");
    sqlx::query_as::<_, Profile>(&q).bind(id).bind(account_id).fetch_optional(&state.db).await?.ok_or_else(|| AppError::not_found("profile"))
}

pub async fn send_test(state: &AppState, ctx: &Ctx, b: TestBody) -> AppResult<Value> {
    ctx.require(Perm::TestSend)?;
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let content = b.content.unwrap_or_else(|| json!({"title": format!("Test from {}", account.name), "body": "Push notifications are working."}));
    content::parse(&content)?;
    send::check_test_rate(state, ctx.account_id).await?;
    let to_me = b.device_id.is_none() && b.profile_id.is_none() && b.to.as_deref().is_none_or(|t| t == "me");
    // Reaching someone else's device is a real send to a real person.
    if !to_me && !ctx.can(Perm::CampaignSend) {
        return Err(AppError::Forbidden("your role can send test notifications to your own browsers only".into()));
    }
    let outcome = if let Some(device) = b.device_id {
        let owner: Option<Option<Uuid>> = sqlx::query_scalar("SELECT profile_id FROM push_devices WHERE id = $1 AND account_id = $2").bind(device).bind(ctx.account_id).fetch_optional(&state.db).await?;
        let owner = owner.ok_or_else(|| AppError::not_found("device"))?;
        let profile = match owner {
            Some(pid) => load_profile(state, ctx.account_id, pid).await?,
            None => member_profile(state, ctx).await?,
        };
        send::test_to_device(state, &account, &profile, device, &content).await?
    } else if to_me {
        let profile = member_profile(state, ctx).await?;
        send::test_to_member(state, &account, &profile, ctx.user_id.expect("checked by member_profile"), &content).await?
    } else {
        let profile = match (b.profile_id, b.to.as_deref()) {
            (Some(id), _) => load_profile(state, ctx.account_id, id).await?,
            (None, Some(email)) => {
                let email = crate::models::normalize_email(email).ok_or_else(|| AppError::bad("to must be \"me\" or an email address"))?;
                let input = crate::audience::ProfileInput { email: Some(email.clone()), ..Default::default() };
                crate::audience::find_profile(&state.db, ctx.account_id, &input).await?.ok_or_else(|| AppError::bad(format!("{email} isn't a profile in this account")))?
            }
            (None, None) => unreachable!("to_me covers it"),
        };
        send::test_to_profile(state, &account, &profile, &content, None, None).await?
    };
    let result = match outcome {
        Outcome::Sent(id) => {
            let r: Vec<(String, String, Option<String>)> = sqlx::query_as("SELECT kind, status, error FROM push_deliveries WHERE message_id = $1 ORDER BY created_at").bind(id).fetch_all(&state.db).await?;
            json!({"message_id": id, "devices": r.iter().map(|(k, s, e)| json!({"kind": k, "status": s, "error": e})).collect::<Vec<_>>()})
        }
        Outcome::Failed(e) => return Err(AppError::bad(format!("the test didn't reach any device: {e}"))),
        Outcome::Skipped(why) => return Err(AppError::bad(format!("not sent: {why}"))),
        Outcome::Deferred(_) => return Err(AppError::bad("not sent right now; try again")),
    };
    crate::features::team::audit(state, ctx, "push.test_send", None, json!({"to": if to_me { "me" } else { "person" }})).await;
    Ok(result)
}

async fn test_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TestBody>) -> AppResult<Json<Value>> {
    Ok(Json(send_test(&state, &ctx, b).await?))
}

pub async fn profile_push(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    load_profile(state, account_id, id).await?;
    type D = (Uuid, String, String, Option<String>, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>, Option<String>, String, bool);
    let rows: Vec<D> = sqlx::query_as(
        "SELECT id, kind, status, user_agent, created_at, last_seen_at, last_sent_at, last_error, permission, anonymous FROM push_devices WHERE profile_id = $1 AND account_id = $2 ORDER BY last_seen_at DESC LIMIT 50",
    )
    .bind(id)
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let opt_out: Option<(chrono::DateTime<chrono::Utc>, String)> = sqlx::query_as("SELECT created_at, source FROM push_opt_outs WHERE profile_id = $1").bind(id).fetch_optional(&state.db).await?;
    let can_receive: bool = sqlx::query_scalar(&format!("SELECT {} FROM profiles p WHERE p.id = $1", send::REACHABLE_SQL)).bind(id).fetch_one(&state.db).await?;
    Ok(json!({
        "profile_id": id,
        "opted_out": opt_out.is_some(),
        "opted_out_at": opt_out.as_ref().map(|o| o.0),
        "can_receive": can_receive,
        "devices": rows.iter().map(|(id, kind, status, ua, created, seen, sent, err, permission, anonymous)| json!({"id": id, "kind": kind, "status": status, "permission": permission, "anonymous": anonymous, "user_agent": ua, "created_at": created, "last_seen_at": seen, "last_sent_at": sent, "last_error": err})).collect::<Vec<_>>(),
    }))
}

async fn profile_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(profile_push(&state, ctx.account_id, id).await?))
}

/// Turn push off (or back on) for a person. Off wins over anything already queued: each hand-off
/// checks it right before the provider call.
pub async fn set_opt_out(state: &AppState, ctx: &Ctx, id: Uuid, opted_out: bool) -> AppResult<Value> {
    load_profile(state, ctx.account_id, id).await?;
    // The person's row is locked first: a hand-off's final claim holds it too, so an opt-out either
    // lands before a claim (and stops it) or waits for it.
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT 1 FROM profiles WHERE id = $1 AND account_id = $2 FOR UPDATE").bind(id).bind(ctx.account_id).execute(&mut *tx).await?;
    if opted_out {
        sqlx::query("INSERT INTO push_opt_outs (profile_id, account_id, source) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(id)
            .bind(ctx.account_id)
            .bind(if ctx.via_api_key { "api" } else { "dashboard" })
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query("DELETE FROM push_opt_outs WHERE profile_id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    crate::features::team::audit(state, ctx, if opted_out { "push.opt_out" } else { "push.opt_in" }, Some(&id.to_string()), json!({})).await;
    profile_push(state, ctx.account_id, id).await
}

#[derive(Deserialize)]
struct OptOutBody {
    opted_out: bool,
}

async fn opt_out_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<OptOutBody>) -> AppResult<Json<Value>> {
    Ok(Json(set_opt_out(&state, &ctx, id, b.opted_out).await?))
}

#[derive(Deserialize)]
pub struct ReportQuery {
    pub campaign_id: Option<Uuid>,
    pub flow_id: Option<Uuid>,
}

/// Sent, delivered (shown by the browser; apps don't report it), clicked and failed, by platform.
pub async fn report(state: &AppState, account_id: Uuid, q: &ReportQuery) -> AppResult<Value> {
    let (col, id) = match (q.campaign_id, q.flow_id) {
        (Some(c), None) => ("campaign_id", c),
        (None, Some(f)) => ("flow_id", f),
        _ => return Err(AppError::bad("give a campaign_id or a flow_id")),
    };
    let rows: Vec<(String, i64, i64, i64, i64, i64, i64, i64)> = sqlx::query_as(&format!(
        "SELECT d.kind, count(*) FILTER (WHERE d.status = 'sent'), count(d.delivered_at), count(d.clicked_at),
                count(*) FILTER (WHERE d.status = 'failed'), count(*) FILTER (WHERE d.status = 'gone'), count(*) FILTER (WHERE d.status = 'retry'),
                count(*) FILTER (WHERE d.status = 'unconfirmed')
         FROM push_deliveries d JOIN messages m ON m.id = d.message_id
         WHERE m.{col} = $1 AND m.account_id = $2 AND NOT m.is_test GROUP BY d.kind ORDER BY d.kind"
    ))
    .bind(id)
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let people: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(&format!(
        "SELECT count(*) FILTER (WHERE status IN ('sent', 'delivered')), count(*) FILTER (WHERE clicked_at IS NOT NULL), count(*) FILTER (WHERE status = 'failed'), count(*) FILTER (WHERE status = 'skipped'),
                count(*) FILTER (WHERE status = 'retrying'), count(*) FILTER (WHERE status = 'unconfirmed')
         FROM messages WHERE {col} = $1 AND account_id = $2 AND channel = 'push' AND NOT is_test"
    ))
    .bind(id)
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    Ok(json!({
        "people": {"sent": people.0, "clicked": people.1, "failed": people.2, "skipped": people.3, "retrying": people.4, "unconfirmed": people.5},
        "platforms": rows.iter().map(|(k, sent, delivered, clicked, failed, gone, retry, unconfirmed)| json!({
            "platform": k, "sent": sent, "delivered": if k == "web" { json!(delivered) } else { Value::Null }, "clicked": clicked, "failed": failed, "removed": gone, "retrying": retry, "unconfirmed": unconfirmed
        })).collect::<Vec<_>>(),
    }))
}

async fn report_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ReportQuery>) -> AppResult<Json<Value>> {
    Ok(Json(report(&state, ctx.account_id, &q).await?))
}

/// Tests of a push campaign (`campaigns::send_test`): each address is a person's email, and the
/// devices linked to them get it (for a team member, also their own test browsers). Callers check
/// who may send tests (`features::team::sends`: push tests need the campaign-send permission).
pub async fn campaign_test(state: &AppState, account: &crate::models::Account, c: &crate::models::CampaignRow, content: &Value, to: &[String], create_profiles: bool) -> AppResult<usize> {
    let mut sent = 0;
    for raw in to {
        let email = crate::models::normalize_email(raw).ok_or_else(|| AppError::bad(format!("{raw:?} isn't an email address: push tests go to the devices of the person with that email")))?;
        let member: Option<Uuid> = sqlx::query_scalar("SELECT u.id FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = $1 AND u.email = $2")
            .bind(account.id)
            .bind(&email)
            .fetch_optional(&state.db)
            .await?;
        let input = crate::audience::ProfileInput { email: Some(email.clone()), ..Default::default() };
        let profile = match crate::audience::find_profile(&state.db, account.id, &input).await? {
            Some(p) => p,
            // Only a team member's own profile is made for a test (it carries their test browsers).
            None if member.is_some() && create_profiles => crate::audience::upsert_profile(&state.db, account.id, &input).await?.profile,
            None => return Err(AppError::bad(format!("{email} isn't a profile in this account"))),
        };
        match send::test_to_profile(state, account, &profile, content, Some(c.id), member).await? {
            Outcome::Sent(_) => sent += 1,
            Outcome::Failed(e) => return Err(AppError::bad(format!("test to {email} failed: {e}"))),
            Outcome::Skipped(why) => return Err(AppError::bad(format!("test to {email} not sent: {why}"))),
            Outcome::Deferred(_) => {}
        }
    }
    Ok(sent)
}

/// Why a push campaign or flow step can't go out yet: push isn't set up for any platform it targets.
pub async fn setup_problems(state: &AppState, account_id: Uuid, contents: &[&Value]) -> AppResult<Vec<String>> {
    let row = settings::load(&state.db, account_id).await?;
    let ready = row.as_ref().map(settings::Row::ready_kinds).unwrap_or_default();
    let mut out = Vec::new();
    for c in contents {
        out.extend(content::problems(c));
        if let Ok(pc) = content::parse(c) {
            if !pc.platforms().iter().any(|k| ready.contains(&k.as_str())) {
                out.push("Set up push in Settings → Push for the devices this notification goes to".to_string());
            }
        }
    }
    out.dedup();
    Ok(out)
}

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("`{k}` must be an id")))
}

/// Render push content for a preview (the `/api/render` endpoint and `render_preview` tool).
pub fn preview(account: &crate::models::Account, content: &Value, ctx: &Value) -> AppResult<Value> {
    let c = content::parse(content)?;
    let fallback = account.website.as_deref().map(|w| if w.contains("://") { w.to_string() } else { format!("https://{w}") }).unwrap_or_default();
    let r = content::render(&c, ctx, &fallback, account.logo_url.as_deref())?;
    Ok(json!({"title": r.title, "body": r.body, "url": r.url, "icon": r.icon, "image": r.image, "actions": r.actions, "platforms": c.platforms()}))
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "push"
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/push/settings", get(settings_h).patch(update_settings_h))
            .route("/api/push/identity-secret", post(identity_h))
            .route("/api/push/test-devices", post(test_device_h))
            .route("/api/push/test", post(test_h))
            .route("/api/push/profiles/{id}", get(profile_h))
            .route("/api/push/profiles/{id}/opt-out", put(opt_out_h))
            .route("/api/push/report", get(report_h))
            .route("/push/sw.js", get(client::service_worker))
            .route("/client/push/sign-out", post(client::sign_out).options(client::preflight))
            .route("/client/push/devices/sign-out", post(client::device_sign_out).options(client::preflight))
            .route("/client/push.js", get(client::push_js))
            .route("/client/push/subscribe", post(client::subscribe).options(client::preflight))
            .route("/client/push/unsubscribe", post(client::unsubscribe).options(client::preflight))
            .route("/client/push/delivered", post(client::delivered).options(client::preflight))
            .route("/client/push/opened", post(client::opened).options(client::preflight))
            .route("/client/push/devices", post(client::register_device).options(client::preflight))
            .route("/client/push/devices/remove", post(client::remove_device).options(client::preflight))
            .route("/p/c/{token}", get(client::click))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        vec![
            tool("get_push_settings", "Get push settings", "Web push (on or off, the service worker file and path, the in-page prompt), Apple (APNs) and Firebase (FCM) credentials (which are set, never the secrets), identity verification, and device counts. Read dumbmail://docs/push for setup.", obj(json!({}), &[]), true, false),
            tool("update_push_settings", "Change push settings", "Turn web push on or off, set the service worker path and the in-page prompt, and add, replace or remove (null) APNs and FCM credentials. Only the fields given change. Secrets are stored encrypted and never shown again.", obj(settings_schema(), &[]), false, false),
            tool("rotate_push_identity_secret", "New enrollment secret", "Make a new secret for enrollment grants: the customer's server signs who is signed in (grant = v1.<expires>.<hex HMAC-SHA256 of v1|push-enroll|<account id>|<external_id or email>|<value>|<expires>>, valid at most 24 hours; see dumbmail://docs/push). Returned once; the old one stops working. Requires confirm=true.", obj(json!({"confirm": {"type": "boolean"}}), &[]), false, true),
            tool("send_test_push", "Send a test push", "Send a test notification. to: \"me\" (your own test browsers, added from Settings → Push) or a person's email; or profile_id or device_id. A person's devices are real people: get the user's approval and pass confirm=true. Without confirm it returns what would happen. content defaults to a short sample.",
                obj(json!({"content": content_schema(), "to": {"type": "string"}, "profile_id": {"type": "string"}, "device_id": {"type": "string"}, "confirm": {"type": "boolean"}}), &[]), false, true),
            tool("get_profile_push", "A person's push devices", "Their devices (web, ios, android) with status and last use, whether they turned push off, and whether a push would reach them now.", obj(json!({"profile_id": {"type": "string"}}), &["profile_id"]), true, false),
            tool("set_push_opt_out", "Turn push off or on for a person", "opted_out true stops every push to them on every device (including ones already queued); false lets push reach them again. Only clear it when they asked to get push again.", obj(json!({"profile_id": {"type": "string"}, "opted_out": {"type": "boolean"}}), &["profile_id", "opted_out"]), false, false),
            tool("get_push_report", "Push results", "For a push campaign or flow: people sent, clicked, failed and skipped, and per platform devices sent, delivered (web only: shown by the browser), clicked, failed, removed (unsubscribed devices) and retrying.", obj(json!({"campaign_id": {"type": "string"}, "flow_id": {"type": "string"}}), &[]), true, false),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        let acct = ctx.account_id;
        let r = match name {
            "get_push_settings" => settings::settings_json(state, acct).await,
            "update_push_settings" => async { update_settings(state, ctx, from(a.clone())?).await }.await,
            "rotate_push_identity_secret" => {
                if a["confirm"].as_bool() != Some(true) {
                    Ok(json!({"dry_run": true, "note": "This replaces the enrollment secret; grants made with the old one stop working. Call again with confirm=true."}))
                } else {
                    rotate_identity(state, ctx).await
                }
            }
            "send_test_push" => async {
                let mut b = a.clone();
                let confirm = b.as_object_mut().and_then(|o| o.remove("confirm")).and_then(|v| v.as_bool()) == Some(true);
                let body: TestBody = from(b)?;
                let to_me = body.device_id.is_none() && body.profile_id.is_none() && body.to.as_deref().is_none_or(|t| t == "me");
                if !to_me && !confirm {
                    return Ok(json!({"dry_run": true, "note": "This sends a real notification to that person's devices. Get the user's approval and call again with confirm=true."}));
                }
                send_test(state, ctx, body).await
            }
            .await,
            "get_profile_push" => async { profile_push(state, acct, uuid_arg(a, "profile_id")?).await }.await,
            "set_push_opt_out" => async { set_opt_out(state, ctx, uuid_arg(a, "profile_id")?, a["opted_out"].as_bool().ok_or_else(|| AppError::bad("opted_out must be true or false"))?).await }.await,
            "get_push_report" => async {
                let q = ReportQuery { campaign_id: a["campaign_id"].as_str().map(|_| uuid_arg(a, "campaign_id")).transpose()?, flow_id: a["flow_id"].as_str().map(|_| uuid_arg(a, "flow_id")).transpose()? };
                report(state, acct, &q).await
            }
            .await,
            _ => return None,
        };
        Some(r)
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc { slug: "push", name: "Push notifications", description: "Web push through the website snippet, mobile push through your own APNs and Firebase credentials, push campaigns and flow steps, consent and reports", text: DOC }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            "push_retry" => Some(async {
                let id: Uuid = payload["message_id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("push_retry needs message_id"))?;
                send::hand_off(state, id, send::Attempt::Later).await.map(|_| ()).map_err(|e| anyhow::anyhow!(e.public_message()))
            }
            .await),
            "push_event" => Some(async {
                let id: Uuid = payload["message_id"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("push_event needs message_id"))?;
                let metric = match payload["metric"].as_str() {
                    Some("Clicked Push") => "Clicked Push",
                    _ => "Received Push",
                };
                send::record_message_event(state, id, metric).await
            }
            .await),
            "push_cleanup" => Some(send::cleanup(state).await),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(15) {
            send::schedule_cleanup(state).await?;
        }
        Ok(())
    }
}
