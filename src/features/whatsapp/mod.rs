//! WhatsApp as a channel, through Meta's WhatsApp Cloud API with the account's own WhatsApp
//! Business account: the connection and its webhook, message templates (synced and created),
//! WhatsApp consent (separate from texts), campaigns and flow steps that send approved templates,
//! two-way conversations in the inbox, and reports.
//!
//! `sending::send` hands `whatsapp` messages to [`send::send`]; campaigns and flows otherwise treat
//! WhatsApp like texts (audiences use [`consent::REACHABLE_SQL`]). See docs/WHATSAPP.md.
//!
//! RCS isn't here: it's an upgrade to texts (see features::rcs).

pub mod consent;
pub mod content;
pub mod graph;
pub mod inbox;
pub mod send;
pub mod settings;
pub mod templates;
pub mod testers;
mod tools;
pub mod webhook;

use async_trait::async_trait;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
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
use crate::models::{normalize_phone, Account, CampaignRow, Profile, PROFILE_COLS};
use crate::sending::{Outcome, SendSpec};
use crate::state::AppState;

pub struct Module;

/// Operator configuration.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct WhatsappConfig {
    /// Per WhatsApp message sent (Meta bills the business's own account for its conversations, so
    /// DumbMail charges nothing unless this is set). Charged from the balance like texts.
    #[arg(long, env = "PRICE_WHATSAPP", default_value_t = 0.0)]
    pub price_whatsapp: f64,
    /// Testing only: send Graph API requests to this stand-in instead of graph.facebook.com.
    #[arg(long, env = "WHATSAPP_API_BASE", hide = true)]
    pub whatsapp_api_base: Option<String>,
}

/// Settings and templates read like the account's other settings; changing the connection or
/// creating templates is settings write; tests need the test-send permission; consent and
/// conversations are people's data (replying is a send, as for texts).
static ROUTES: &[RouteRule] = &[
    rule("GET", "/api/whatsapp/settings", Perm::AccountRead),
    rule("PATCH", "/api/whatsapp/settings", Perm::SettingsWrite),
    rule("DELETE", "/api/whatsapp/settings", Perm::SettingsWrite),
    rule("POST", "/api/whatsapp/settings/check", Perm::AccountRead),
    rule("POST", "/api/whatsapp/settings/verify-token", Perm::SettingsWrite),
    rule("GET", "/api/whatsapp/templates", Perm::AccountRead),
    rule("POST", "/api/whatsapp/templates", Perm::SettingsWrite),
    rule("POST", "/api/whatsapp/templates/sync", Perm::SettingsWrite),
    rule("GET", "/api/whatsapp/templates/{id}", Perm::AccountRead),
    rule("POST", "/api/whatsapp/test", Perm::TestSend),
    rule("GET", "/api/whatsapp/test-numbers", Perm::AccountRead),
    rule("POST", "/api/whatsapp/test-numbers", Perm::TestSend),
    rule("DELETE", "/api/whatsapp/test-numbers/{id}", Perm::TestSend),
    rule("GET", "/api/whatsapp/profiles/{id}", Perm::ProfilesRead),
    rule("PUT", "/api/whatsapp/profiles/{id}/consent", Perm::AudienceWrite),
    rule("POST", "/api/whatsapp/consent/import", Perm::AudienceWrite),
    rule("GET", "/api/whatsapp/conversations", Perm::ProfilesRead),
    rule("GET", "/api/whatsapp/conversations/{id}", Perm::ProfilesRead),
    rule("POST", "/api/whatsapp/conversations/{id}/read", Perm::ProfilesRead),
    rule("POST", "/api/whatsapp/conversations/{id}/status", Perm::CampaignSend),
    rule("POST", "/api/whatsapp/conversations/{id}/reply", Perm::CampaignSend),
    rule("GET", "/api/whatsapp/report", Perm::MessagingRead),
    public("GET", "/webhooks/whatsapp/{key}"),
    public("POST", "/webhooks/whatsapp/{key}"),
    public("GET", "/wa/{token}"),
];

static TOOLS: &[ToolRule] = &[
    ToolRule { name: "get_whatsapp_settings", perm: Perm::AccountRead },
    ToolRule { name: "update_whatsapp_settings", perm: Perm::SettingsWrite },
    ToolRule { name: "disconnect_whatsapp", perm: Perm::SettingsWrite },
    ToolRule { name: "check_whatsapp_connection", perm: Perm::AccountRead },
    ToolRule { name: "list_whatsapp_templates", perm: Perm::AccountRead },
    ToolRule { name: "sync_whatsapp_templates", perm: Perm::SettingsWrite },
    ToolRule { name: "list_whatsapp_test_numbers", perm: Perm::AccountRead },
    ToolRule { name: "add_whatsapp_test_number", perm: Perm::TestSend },
    ToolRule { name: "create_whatsapp_template", perm: Perm::SettingsWrite },
    ToolRule { name: "send_test_whatsapp", perm: Perm::TestSend },
    ToolRule { name: "get_whatsapp_consent", perm: Perm::ProfilesRead },
    ToolRule { name: "set_whatsapp_consent", perm: Perm::AudienceWrite },
    ToolRule { name: "import_whatsapp_consent", perm: Perm::AudienceWrite },
    ToolRule { name: "list_whatsapp_conversations", perm: Perm::ProfilesRead },
    ToolRule { name: "get_whatsapp_conversation", perm: Perm::ProfilesRead },
    ToolRule { name: "send_whatsapp_reply", perm: Perm::CampaignSend },
    ToolRule { name: "close_whatsapp_conversation", perm: Perm::CampaignSend },
    ToolRule { name: "get_whatsapp_report", perm: Perm::MessagingRead },
];

const DOC: &str = include_str!("../../../docs/WHATSAPP.md");

/// Email the account's owners that Meta paused or disabled a template (sends using it stop).
pub async fn alert_template(state: &AppState, account_id: Uuid, name: &str, event: &str) {
    let name: String = name.chars().take(100).collect();
    let what = if event == "DISABLED" { "disabled" } else { "paused" };
    let text = format!(
        "Meta {what} your WhatsApp template \"{name}\", usually because people reported or blocked messages that used it.\n\n\
         DumbMail has stopped sending it: campaigns and flow steps that use it skip their messages until it's approved again.\n\
         Check its quality in WhatsApp Manager, edit it if needed, then sync templates in Settings → WhatsApp."
    );
    crate::features::webhooks::alert_account(state, account_id, &format!("WhatsApp template {name} was {what}"), &text).await;
}

/// Email the account's owners about a quality problem Meta reported (a restricted number, spam
/// signals), at most once a day.
pub async fn alert_quality(state: &AppState, account_id: Uuid, why: &str) {
    let due: Option<Uuid> = sqlx::query_scalar(
        "UPDATE whatsapp_settings SET quality_alert_at = now() WHERE account_id = $1 AND (quality_alert_at IS NULL OR quality_alert_at < now() - interval '1 day') RETURNING account_id",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    if due.is_none() {
        return;
    }
    let text = format!(
        "Meta limited sending from your WhatsApp number: {why}.\n\n\
         This usually follows people blocking or reporting your messages. DumbMail keeps trying within Meta's limits, but check the number's quality \
         rating in WhatsApp Manager, slow down marketing sends and make sure everyone you message opted in."
    );
    crate::features::webhooks::alert_account(state, account_id, "Meta limited your WhatsApp number", &text).await;
}

// ---------- Channel hooks used by shared code ----------

/// Why a campaign or flow step with this WhatsApp content can't go yet (connection, template,
/// variables). Called when a campaign launches and a flow goes live.
pub async fn setup_problems(state: &AppState, account_id: Uuid, contents: &[&Value]) -> AppResult<Vec<String>> {
    let mut out = Vec::new();
    if !settings::connected(&state.db, account_id).await? {
        out.push("Connect WhatsApp in Settings → WhatsApp".to_string());
        return Ok(out);
    }
    for v in contents {
        let c = content::parse(v)?;
        let Some(tid) = c.template_id else {
            out.push("Pick a WhatsApp template".into());
            continue;
        };
        match templates::get(&state.db, account_id, tid).await {
            Ok(t) => out.extend(content::problems_with(&c, &t)),
            Err(_) => out.push("The WhatsApp template isn't in this account; sync templates and pick it again".into()),
        }
    }
    out.dedup();
    Ok(out)
}

/// Problems the readiness check can see without the database (content shape only).
pub fn content_problems(v: &Value) -> Vec<String> {
    match content::parse(v) {
        Ok(c) if c.template_id.is_none() => vec!["Pick a WhatsApp template".into()],
        Ok(_) => vec![],
        Err(e) => vec![e.public_message()],
    }
}

/// The editors' preview: the template filled for a sample person (or Alex Rivera).
pub async fn preview(state: &AppState, account: &Account, content: &Value, ctx: &Value) -> AppResult<Value> {
    let c = content::parse(content)?;
    let Some(tid) = c.template_id else {
        return Ok(json!({"channel": "whatsapp", "text": "", "problems": ["Pick a WhatsApp template"]}));
    };
    let t = templates::get(&state.db, account.id, tid).await?;
    Ok(content::preview(&t, &c, ctx))
}

/// A campaign's test: only to the team's test numbers (Settings → WhatsApp), never to someone who
/// opted out of WhatsApp.
pub async fn campaign_test(state: &AppState, account: &Account, c: &CampaignRow, content: &Value, to: &[String]) -> AppResult<usize> {
    let mut sent = 0;
    for raw in to {
        let n = test_number(state, account.id, raw).await?;
        let profile = test_profile(state, account.id, &n).await?;
        match test_send(state, account, &profile, &n, content, Some(c.id)).await? {
            Outcome::Sent(_) => sent += 1,
            Outcome::Failed(e) => return Err(AppError::bad(format!("test to {n} failed: {e}"))),
            Outcome::Skipped(why) => return Err(AppError::bad(format!("test to {n} not sent: {why}"))),
            Outcome::Deferred(_) => return Err(AppError::bad(format!("test to {n} not sent: Meta asked to wait; try again shortly"))),
        }
    }
    Ok(sent)
}

async fn test_number(state: &AppState, account_id: Uuid, raw: &str) -> AppResult<String> {
    let n = normalize_phone(raw).ok_or_else(|| AppError::bad(format!("{raw:?} isn't a phone number (use +15035550100)")))?;
    if !testers::is_member_number(&state.db, account_id, &n).await? {
        return Err(AppError::bad(format!("WhatsApp tests go only to team members' verified numbers, and {n} isn't one. Add yours in Settings → WhatsApp.")));
    }
    Ok(n)
}

/// The profile a test to a team number is personalized with (made with just the number if needed;
/// that never subscribes it).
async fn test_profile(state: &AppState, account_id: Uuid, n: &str) -> AppResult<Profile> {
    let input = crate::audience::ProfileInput { phone_number: Some(n.to_string()), ..Default::default() };
    Ok(match crate::audience::find_profile(&state.db, account_id, &input).await? {
        Some(p) => p,
        None => crate::audience::upsert_profile(&state.db, account_id, &input).await?.profile,
    })
}

async fn test_send(state: &AppState, account: &Account, profile: &Profile, to: &str, content: &Value, campaign_id: Option<Uuid>) -> AppResult<Outcome> {
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE account_id = $1 AND is_test AND created_at > now() - interval '1 hour'").bind(account.id).fetch_one(&state.db).await?;
    if recent >= 50 {
        return Err(AppError::LimitReached("test sends are limited to 50 an hour".into()));
    }
    send::send(
        state,
        &SendSpec {
            account,
            profile,
            channel: "whatsapp",
            content,
            campaign_id,
            flow_id: None,
            flow_node_id: None,
            event: None,
            is_test: true,
            message_id: None,
            to_override: Some(to.to_string()),
        },
        false,
    )
    .await
}

// ---------- REST handlers ----------

async fn settings_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings::settings_json(&state, ctx.account_id).await?))
}

pub async fn update_settings(state: &AppState, ctx: &Ctx, b: settings::SettingsBody) -> AppResult<Value> {
    let changed = settings::update(state, ctx.account_id, b).await?;
    if !changed.is_empty() {
        crate::features::team::audit(state, ctx, "whatsapp.settings_update", None, json!({"changed": changed})).await;
    }
    settings::settings_json(state, ctx.account_id).await
}

async fn update_settings_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<settings::SettingsBody>) -> AppResult<Json<Value>> {
    Ok(Json(update_settings(&state, &ctx, b).await?))
}

pub async fn disconnect(state: &AppState, ctx: &Ctx) -> AppResult<Value> {
    settings::disconnect(state, ctx.account_id).await?;
    crate::features::team::audit(state, ctx, "whatsapp.disconnect", None, json!({})).await;
    settings::settings_json(state, ctx.account_id).await
}

async fn disconnect_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(disconnect(&state, &ctx).await?))
}

async fn check_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(settings::check(&state, ctx.account_id).await?))
}

async fn verify_token_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    settings::rotate_verify_token(&state, ctx.account_id).await?;
    crate::features::team::audit(&state, &ctx, "whatsapp.verify_token_rotate", None, json!({})).await;
    Ok(Json(settings::settings_json(&state, ctx.account_id).await?))
}

pub async fn templates_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let list = templates::list(&state.db, account_id, false).await?;
    Ok(json!({"templates": list.iter().map(templates::Template::json).collect::<Vec<_>>()}))
}

async fn templates_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(templates_json(&state, ctx.account_id).await?))
}

async fn template_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(templates::get(&state.db, ctx.account_id, id).await?.json()))
}

pub async fn sync_templates(state: &AppState, ctx: &Ctx) -> AppResult<Value> {
    let n = templates::sync(state, ctx.account_id).await?;
    crate::features::team::audit(state, ctx, "whatsapp.templates_sync", None, json!({"read": n})).await;
    let mut out = templates_json(state, ctx.account_id).await?;
    out["read"] = json!(n);
    Ok(out)
}

async fn sync_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(sync_templates(&state, &ctx).await?))
}

async fn test_numbers_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(json!({"test_numbers": testers::list(&state.db, ctx.account_id).await?})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestNumberBody {
    pub phone_number: String,
}

/// Start verifying the signed-in member's own number.
pub async fn add_test_number(state: &AppState, ctx: &Ctx, phone: &str) -> AppResult<Value> {
    let user = ctx.user_id.ok_or_else(|| AppError::bad("Sign in as the team member whose phone this is: a test number is a member's own phone"))?;
    let out = testers::add(&state.db, ctx.account_id, user, phone).await?;
    crate::features::team::audit(state, ctx, "whatsapp.test_number_add", out["id"].as_str(), json!({})).await;
    Ok(out)
}

async fn add_test_number_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TestNumberBody>) -> AppResult<Json<Value>> {
    Ok(Json(add_test_number(&state, &ctx, &b.phone_number).await?))
}

async fn remove_test_number_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    testers::remove(&state.db, ctx.account_id, ctx.user_id, ctx.can(Perm::SettingsWrite), id).await?;
    crate::features::team::audit(&state, &ctx, "whatsapp.test_number_remove", Some(&id.to_string()), json!({})).await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct CreateBody {
    #[serde(flatten)]
    pub template: templates::NewTemplate,
    #[serde(default)]
    pub confirm: bool,
}

/// Create a template: without `confirm`, what would be submitted to Meta.
pub async fn create_template(state: &AppState, ctx: &Ctx, b: CreateBody) -> AppResult<Value> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let (submit, tracked) = templates::build(&b.template, &crate::domains::link_base(state, &account))?;
    if !b.confirm {
        return Ok(json!({"dry_run": true, "submit": submit, "tracked_buttons": tracked, "note": "Nothing submitted. Call again with confirm=true to send it to Meta for review."}));
    }
    let t = templates::create(state, ctx.account_id, &b.template).await?;
    crate::features::team::audit(state, ctx, "whatsapp.template_create", Some(&t.id.to_string()), json!({"name": t.name, "language": t.language, "category": t.category})).await;
    Ok(t.json())
}

async fn create_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<CreateBody>) -> AppResult<Json<Value>> {
    Ok(Json(create_template(&state, &ctx, b).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestBody {
    /// One of the team's test numbers.
    pub to: String,
    /// WhatsApp content (template and variables).
    pub content: Value,
    /// Personalize as this person (default: the profile with the test number).
    #[serde(default)]
    pub profile_id: Option<Uuid>,
}

pub async fn send_test(state: &AppState, ctx: &Ctx, b: TestBody) -> AppResult<Value> {
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let n = test_number(state, ctx.account_id, &b.to).await?;
    let profile = match b.profile_id {
        Some(id) => {
            let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1 AND p.account_id = $2");
            sqlx::query_as::<_, Profile>(&q).bind(id).bind(ctx.account_id).fetch_optional(&state.db).await?.ok_or_else(|| AppError::not_found("profile"))?
        }
        None => test_profile(state, ctx.account_id, &n).await?,
    };
    let out = match test_send(state, &account, &profile, &n, &b.content, None).await? {
        Outcome::Sent(id) => json!({"sent": true, "message_id": id, "to": n}),
        Outcome::Skipped(why) => return Err(AppError::bad(format!("not sent: {why}"))),
        Outcome::Failed(e) => return Err(AppError::bad(format!("not sent: {e}"))),
        Outcome::Deferred(_) => return Err(AppError::bad("Meta asked to wait; try again shortly")),
    };
    crate::features::team::audit(state, ctx, "whatsapp.test_send", None, json!({"message_id": out["message_id"]})).await;
    Ok(out)
}

async fn test_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TestBody>) -> AppResult<Json<Value>> {
    Ok(Json(send_test(&state, &ctx, b).await?))
}

async fn profile_phone(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Option<String>> {
    let p: Option<Option<String>> = sqlx::query_scalar("SELECT phone_number FROM profiles WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).fetch_optional(&state.db).await?;
    p.ok_or_else(|| AppError::not_found("profile"))
}

pub async fn profile_json(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let Some(phone) = profile_phone(state, account_id, id).await? else {
        return Ok(json!({"profile_id": id, "phone_number": null, "consent": "never_subscribed", "can_receive": false}));
    };
    let mut v = consent::contact_json(&state.db, account_id, &phone).await?;
    let can: bool = sqlx::query_scalar(&format!("SELECT {} FROM profiles p WHERE p.id = $1", consent::REACHABLE_SQL)).bind(id).fetch_one(&state.db).await?;
    v["profile_id"] = json!(id);
    v["can_receive"] = json!(can);
    Ok(v)
}

async fn profile_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(profile_json(&state, ctx.account_id, id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentBody {
    /// subscribed | unsubscribed
    pub consent: String,
    /// For subscribed: how they agreed to WhatsApp marketing (required), e.g. "checkout checkbox
    /// naming WhatsApp". Kept in the consent history.
    #[serde(default)]
    pub evidence: Option<String>,
}

fn evidence(b: &ConsentBody) -> AppResult<Value> {
    let ev = b.evidence.as_deref().map(str::trim).filter(|e| !e.is_empty()).map(|e| e.chars().take(300).collect::<String>());
    if b.consent == "subscribed" && ev.is_none() {
        return Err(AppError::bad("Say how they agreed to WhatsApp marketing (evidence), e.g. \"checkout checkbox naming WhatsApp\". Only an explicit WhatsApp opt-in subscribes someone."));
    }
    Ok(json!({"evidence": ev}))
}

pub async fn set_profile_consent(state: &AppState, ctx: &Ctx, id: Uuid, b: &ConsentBody) -> AppResult<Value> {
    let detail = evidence(b)?;
    let phone = profile_phone(state, ctx.account_id, id).await?.ok_or_else(|| AppError::bad("This person has no phone number"))?;
    let mut tx = state.db.begin().await?;
    consent::lock(&mut tx, ctx.account_id, &phone).await?;
    // The number is read again under the lock, so consent goes only to the number they have now.
    let now_phone: Option<Option<String>> = sqlx::query_scalar("SELECT phone_number FROM profiles WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).fetch_optional(&mut *tx).await?;
    if now_phone.flatten().as_deref() != Some(phone.as_str()) {
        return Err(AppError::Conflict("This person's phone number just changed; try again".into()));
    }
    let changed = consent::set(&mut tx, &state.db, ctx.account_id, &phone, Some(id), &b.consent, "api", detail, chrono::Utc::now()).await?;
    tx.commit().await?;
    if changed {
        let metric = if b.consent == "subscribed" { "Subscribed to WhatsApp" } else { "Unsubscribed from WhatsApp" };
        crate::audience::record_event(
            &state.db,
            crate::audience::NewEvent { account_id: ctx.account_id, profile_id: id, metric, properties: json!({"method": "api"}), value: None, occurred_at: None, unique_id: None, message_id: None },
        )
        .await?;
    }
    crate::features::team::audit(state, ctx, "whatsapp.consent_set", Some(&id.to_string()), json!({"consent": b.consent})).await;
    profile_json(state, ctx.account_id, id).await
}

async fn consent_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<ConsentBody>) -> AppResult<Json<Value>> {
    Ok(Json(set_profile_consent(&state, &ctx, id, &b).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportBody {
    /// E.164 numbers (up to 1,000).
    pub phone_numbers: Vec<String>,
    pub consent: String,
    #[serde(default)]
    pub evidence: Option<String>,
}

/// Record WhatsApp consent for many numbers at once (an import). Numbers without a profile get one
/// (with just the number).
pub async fn import_consent(state: &AppState, ctx: &Ctx, b: &ImportBody) -> AppResult<Value> {
    let detail = evidence(&ConsentBody { consent: b.consent.clone(), evidence: b.evidence.clone() })?;
    if b.phone_numbers.is_empty() || b.phone_numbers.len() > 1000 {
        return Err(AppError::bad("Give 1 to 1,000 phone numbers"));
    }
    let mut numbers: Vec<String> = Vec::new();
    let mut invalid = 0;
    for raw in &b.phone_numbers {
        match normalize_phone(raw) {
            Some(n) if !numbers.contains(&n) => numbers.push(n),
            Some(_) => {}
            None => invalid += 1,
        }
    }
    let mut changed = 0;
    for n in &numbers {
        let input = crate::audience::ProfileInput { phone_number: Some(n.clone()), ..Default::default() };
        let pid = match crate::audience::find_profile(&state.db, ctx.account_id, &input).await? {
            Some(p) => p.id,
            None => crate::audience::upsert_profile(&state.db, ctx.account_id, &input).await?.profile.id,
        };
        let mut tx = state.db.begin().await?;
        consent::lock(&mut tx, ctx.account_id, n).await?;
        if consent::set(&mut tx, &state.db, ctx.account_id, n, Some(pid), &b.consent, "import", detail.clone(), chrono::Utc::now()).await? {
            changed += 1;
        }
        tx.commit().await?;
    }
    crate::features::team::audit(state, ctx, "whatsapp.consent_import", None, json!({"consent": b.consent, "numbers": numbers.len(), "changed": changed})).await;
    Ok(json!({"numbers": numbers.len(), "changed": changed, "invalid": invalid}))
}

async fn import_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<ImportBody>) -> AppResult<Json<Value>> {
    Ok(Json(import_consent(&state, &ctx, &b).await?))
}

async fn conversations_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<inbox::ListQuery>) -> AppResult<Json<Value>> {
    Ok(Json(inbox::list(&state.db, ctx.account_id, &q).await?))
}

async fn conversation_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(inbox::get(&state, ctx.account_id, id).await?))
}

async fn read_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    inbox::mark_read(&state.db, ctx.account_id, id).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

async fn status_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<StatusBody>) -> AppResult<Json<Value>> {
    inbox::set_status(&state.db, ctx.account_id, id, &b.status).await?;
    Ok(Json(json!({"ok": true})))
}

pub async fn reply(state: &AppState, ctx: &Ctx, id: Uuid, b: &inbox::ReplyBody) -> AppResult<Value> {
    let out = inbox::reply(state, ctx.account_id, id, b, ctx.user_id).await?;
    if out["sent"] == true {
        crate::features::team::audit(state, ctx, "whatsapp_reply.send", Some(&id.to_string()), json!({"message_id": out["message_id"]})).await;
    }
    Ok(out)
}

async fn reply_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<inbox::ReplyBody>) -> AppResult<Json<Value>> {
    Ok(Json(reply(&state, &ctx, id, &b).await?))
}

#[derive(Deserialize, Default)]
pub struct ReportQuery {
    #[serde(default)]
    pub campaign_id: Option<Uuid>,
    #[serde(default)]
    pub flow_id: Option<Uuid>,
    #[serde(default)]
    pub days: Option<i64>,
}

/// Sent, delivered, read, clicked and failed WhatsApp messages (a campaign's, a flow's, or the
/// account's over `days`, default 30).
pub async fn report(db: &sqlx::PgPool, account_id: Uuid, q: &ReportQuery) -> AppResult<Value> {
    let days = q.days.unwrap_or(30).clamp(1, 365);
    let row: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE m.status IN ('sent', 'delivered') OR w.wa_status IN ('sent', 'delivered', 'read')),
                count(*) FILTER (WHERE w.wa_status IN ('delivered', 'read')),
                count(*) FILTER (WHERE w.wa_status = 'read'),
                count(*) FILTER (WHERE m.clicked_at IS NOT NULL),
                count(*) FILTER (WHERE m.status = 'failed'),
                count(*) FILTER (WHERE m.status = 'skipped'),
                count(*)
         FROM messages m LEFT JOIN whatsapp_sent w ON w.message_id = m.id
         WHERE m.account_id = $1 AND m.channel = 'whatsapp' AND NOT m.is_test
           AND ($2::uuid IS NULL OR m.campaign_id = $2) AND ($3::uuid IS NULL OR m.flow_id = $3)
           AND ($2::uuid IS NOT NULL OR $3::uuid IS NOT NULL OR m.created_at > now() - make_interval(days => $4::int))",
    )
    .bind(account_id)
    .bind(q.campaign_id)
    .bind(q.flow_id)
    .bind(days as i32)
    .fetch_one(db)
    .await?;
    let failures: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT m.error, count(*) FROM messages m WHERE m.account_id = $1 AND m.channel = 'whatsapp' AND NOT m.is_test AND m.status IN ('failed', 'skipped')
           AND ($2::uuid IS NULL OR m.campaign_id = $2) AND ($3::uuid IS NULL OR m.flow_id = $3)
           AND ($2::uuid IS NOT NULL OR $3::uuid IS NOT NULL OR m.created_at > now() - make_interval(days => $4::int))
         GROUP BY m.error ORDER BY count(*) DESC LIMIT 10",
    )
    .bind(account_id)
    .bind(q.campaign_id)
    .bind(q.flow_id)
    .bind(days as i32)
    .fetch_all(db)
    .await?;
    Ok(json!({
        "sent": row.0, "delivered": row.1, "read": row.2, "clicked": row.3, "failed": row.4, "skipped": row.5, "messages": row.6,
        "reasons": failures.into_iter().map(|(e, n)| json!({"reason": e, "count": n})).collect::<Vec<_>>(),
    }))
}

async fn report_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ReportQuery>) -> AppResult<Json<Value>> {
    Ok(Json(report(&state.db, ctx.account_id, &q).await?))
}

/// A tracked button's click: record it on its message, then go where the button goes.
async fn click_h(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some((message_id, index)) = content::from_button_token(&state.cfg.secret_key, &token) else {
        return (axum::http::StatusCode::BAD_REQUEST, "invalid link").into_response();
    };
    let url: Option<Option<String>> = sqlx::query_scalar(
        "SELECT t.tracked_buttons->>($2::text) FROM whatsapp_sent w JOIN whatsapp_templates t ON t.id = w.template_id WHERE w.message_id = $1",
    )
    .bind(message_id)
    .bind(index.to_string())
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    match url.flatten().filter(|u| u.starts_with("https://")) {
        Some(u) => crate::public::record_click(&state, message_id, &u).await,
        None => Redirect::to(state.cfg.public_url()).into_response(),
    }
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "whatsapp"
    }

    fn access(&self) -> Access {
        Access { routes: ROUTES, tools: TOOLS }
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/whatsapp/settings", get(settings_h).patch(update_settings_h).delete(disconnect_h))
            .route("/api/whatsapp/settings/check", post(check_h))
            .route("/api/whatsapp/settings/verify-token", post(verify_token_h))
            .route("/api/whatsapp/templates", get(templates_h).post(create_h))
            .route("/api/whatsapp/templates/sync", post(sync_h))
            .route("/api/whatsapp/templates/{id}", get(template_h))
            .route("/api/whatsapp/test", post(test_h))
            .route("/api/whatsapp/test-numbers", get(test_numbers_h).post(add_test_number_h))
            .route("/api/whatsapp/test-numbers/{id}", axum::routing::delete(remove_test_number_h))
            .route("/api/whatsapp/profiles/{id}", get(profile_h))
            .route("/api/whatsapp/profiles/{id}/consent", put(consent_h))
            .route("/api/whatsapp/consent/import", post(import_h))
            .route("/api/whatsapp/conversations", get(conversations_h))
            .route("/api/whatsapp/conversations/{id}", get(conversation_h))
            .route("/api/whatsapp/conversations/{id}/read", post(read_h))
            .route("/api/whatsapp/conversations/{id}/status", post(status_h))
            .route("/api/whatsapp/conversations/{id}/reply", post(reply_h))
            .route("/api/whatsapp/report", get(report_h))
            .route("/webhooks/whatsapp/{key}", get(webhook::verify).post(webhook::receive).layer(DefaultBodyLimit::max(webhook::MAX_BODY)))
            .route("/wa/{token}", get(click_h))
    }

    fn mcp_tools(&self) -> Vec<Value> {
        tools::definitions()
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
        tools::call(state, ctx, name, args).await
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "whatsapp",
            name: "WhatsApp",
            description: "WhatsApp through your own WhatsApp Business account: connecting, templates, WhatsApp consent, campaigns and flow steps, conversations, reports, and what's verified only against Meta's docs",
            text: DOC,
        }]
    }

    async fn run_job(&self, state: &AppState, kind: &str, payload: &Value) -> Option<anyhow::Result<()>> {
        match kind {
            "whatsapp_send" => Some(send::run_job(state, payload).await),
            "whatsapp_sync_templates" => Some(async {
                if let Some(id) = payload["account_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
                    if let Err(e) = templates::sync(state, id).await {
                        tracing::warn!(account = %id, error = %e.public_message(), "WhatsApp template sync failed");
                    }
                }
                Ok(())
            }
            .await),
            _ => None,
        }
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        if tick.is_multiple_of(90) {
            send::release_stale_holds(&state.db).await?;
            sqlx::query("DELETE FROM whatsapp_seen WHERE created_at < now() - interval '14 days'").execute(&state.db).await?;
            sqlx::query("DELETE FROM whatsapp_backoff WHERE until < now() - interval '1 day'").execute(&state.db).await?;
            sqlx::query("DELETE FROM whatsapp_test_numbers WHERE verified_at IS NULL AND created_at < now() - interval '1 day'").execute(&state.db).await?;
        }
        Ok(())
    }
}
