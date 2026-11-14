use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{page, ApiJson};
use crate::auth::Ctx;
use crate::campaigns::{self, load_account, Audience};
use crate::error::{AppError, AppResult};
use crate::flows;
use crate::models::{CampaignRow, FlowRow, FormRow, Template};
use crate::render;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/templates", get(templates_h).post(create_template_h))
        .route("/api/templates/{id}", get(get_template_h).patch(patch_template_h).delete(delete_template_h))
        .route("/api/render", post(render_h))
        .route("/api/campaigns", get(campaigns_h).post(create_campaign_h))
        .route("/api/campaigns/estimate", post(estimate_h))
        .route("/api/campaigns/{id}", get(get_campaign_h).patch(patch_campaign_h).delete(delete_campaign_h))
        .route("/api/campaigns/{id}/test", post(test_h))
        .route("/api/campaigns/{id}/schedule", post(schedule_h))
        .route("/api/campaigns/{id}/send", post(send_now_h))
        .route("/api/campaigns/{id}/cancel", post(cancel_h))
        .route("/api/campaigns/{id}/duplicate", post(duplicate_h))
        .route("/api/campaigns/{id}/report", get(report_h))
        .route("/api/flows", get(flows_h).post(create_flow_h))
        .route("/api/flows/blueprints", get(blueprints_h))
        .route("/api/flows/from-blueprint", post(from_blueprint_h))
        .route("/api/flows/{id}", get(get_flow_h).patch(patch_flow_h).delete(delete_flow_h))
        .route("/api/flows/{id}/status", post(flow_status_h))
        .route("/api/flows/{id}/report", get(flow_report_h))
        .route("/api/forms", get(forms_h).post(create_form_h))
        .route("/api/forms/{id}", get(get_form_h).patch(patch_form_h).delete(delete_form_h))
}

fn check_channel(c: &str) -> AppResult<()> {
    if c == "email" || c == "sms" { Ok(()) } else { Err(AppError::bad("channel must be email or sms")) }
}

// ---------- Templates ----------

#[derive(Deserialize)]
pub struct ChannelQuery {
    pub channel: Option<String>,
    pub status: Option<String>,
}

async fn templates_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ChannelQuery>) -> AppResult<Json<Value>> {
    let rows: Vec<Template> = sqlx::query_as("SELECT id, name, channel, content, created_at, updated_at FROM templates WHERE account_id = $1 AND ($2::text IS NULL OR channel = $2) ORDER BY updated_at DESC")
        .bind(ctx.account_id)
        .bind(&q.channel)
        .fetch_all(&state.db)
        .await?;
    let n = rows.len() as i64;
    Ok(page(rows, n))
}

#[derive(Deserialize)]
pub struct TemplateBody {
    pub name: Option<String>,
    pub channel: Option<String>,
    pub content: Option<Value>,
}

pub async fn create_template(state: &AppState, account_id: Uuid, b: TemplateBody) -> AppResult<Template> {
    let channel = b.channel.unwrap_or_else(|| "email".into());
    check_channel(&channel)?;
    let name = b.name.filter(|n| !n.trim().is_empty()).ok_or_else(|| AppError::bad("give the template a name"))?;
    let content = b.content.unwrap_or(json!({}));
    Ok(sqlx::query_as("INSERT INTO templates (id, account_id, name, channel, content) VALUES ($1, $2, $3, $4, $5) RETURNING id, name, channel, content, created_at, updated_at")
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(name)
        .bind(channel)
        .bind(content)
        .fetch_one(&state.db)
        .await?)
}

async fn create_template_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<TemplateBody>) -> AppResult<Json<Template>> {
    Ok(Json(create_template(&state, ctx.account_id, b).await?))
}

async fn get_template_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Template>> {
    let t: Option<Template> = sqlx::query_as("SELECT id, name, channel, content, created_at, updated_at FROM templates WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(ctx.account_id)
        .fetch_optional(&state.db)
        .await?;
    Ok(Json(t.ok_or_else(|| AppError::not_found("template"))?))
}

async fn patch_template_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<TemplateBody>) -> AppResult<Json<Template>> {
    let t: Option<Template> = sqlx::query_as(
        "UPDATE templates SET name = COALESCE($3, name), content = COALESCE($4, content), updated_at = now()
         WHERE id = $1 AND account_id = $2 RETURNING id, name, channel, content, created_at, updated_at",
    )
    .bind(id)
    .bind(ctx.account_id)
    .bind(b.name.filter(|n| !n.trim().is_empty()))
    .bind(b.content)
    .fetch_optional(&state.db)
    .await?;
    Ok(Json(t.ok_or_else(|| AppError::not_found("template"))?))
}

async fn delete_template_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let n = sqlx::query("DELETE FROM templates WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("template"));
    }
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct RenderBody {
    pub channel: String,
    pub content: Value,
    pub profile_id: Option<Uuid>,
    #[serde(default)]
    pub event: Option<Value>,
}

/// Preview a message as a given profile (or a sample person) without tracking.
pub async fn render_preview(state: &AppState, account_id: Uuid, b: RenderBody) -> AppResult<Value> {
    check_channel(&b.channel)?;
    let account = load_account(&state.db, account_id).await?;
    let profile = match b.profile_id {
        Some(id) => Some(super::audience::load_profile(state, account_id, id).await?),
        None => None,
    };
    let mut ctx = render::context(&account, profile.as_ref(), b.event.as_ref(), "#unsubscribe");
    if profile.is_none() {
        ctx["first_name"] = json!("Alex");
        ctx["last_name"] = json!("Rivera");
        ctx["email"] = json!("alex@example.com");
        ctx["person"] = json!({"first_name": "Alex", "last_name": "Rivera", "email": "alex@example.com", "properties": {}});
    }
    if b.channel == "email" {
        let content: render::EmailContent = serde_json::from_value(b.content)?;
        let r = render::render_email(&account, &content, &ctx, None)?;
        Ok(json!({"subject": r.subject, "preheader": r.preheader, "html": r.html, "text": r.text}))
    } else {
        let content: render::SmsContent = serde_json::from_value(b.content)?;
        let body = render::render_sms(&account, &content, &ctx, None, false)?;
        let (chars, segs) = crate::providers::sms::segment_count(&body);
        Ok(json!({"body": body, "characters": chars, "segments": segs}))
    }
}

async fn render_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<RenderBody>) -> AppResult<Json<Value>> {
    Ok(Json(render_preview(&state, ctx.account_id, b).await?))
}

// ---------- Campaigns ----------

pub async fn campaign_json(state: &AppState, c: &CampaignRow) -> AppResult<Value> {
    let stats = campaigns::stats(&state.db, c.account_id, c.id).await?;
    let mut v = serde_json::to_value(c)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("account_id");
        o.insert("stats".into(), serde_json::to_value(stats)?);
    }
    Ok(v)
}

pub async fn list_campaigns(state: &AppState, account_id: Uuid, q: &ChannelQuery) -> AppResult<Vec<Value>> {
    let rows: Vec<CampaignRow> = sqlx::query_as("SELECT * FROM campaigns WHERE account_id = $1 AND ($2::text IS NULL OR channel = $2) AND ($3::text IS NULL OR status = $3) ORDER BY COALESCE(sent_at, send_at, created_at) DESC LIMIT 500")
        .bind(account_id)
        .bind(&q.channel)
        .bind(&q.status)
        .fetch_all(&state.db)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for c in &rows {
        out.push(campaign_json(state, c).await?);
    }
    Ok(out)
}

async fn campaigns_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ChannelQuery>) -> AppResult<Json<Value>> {
    let rows = list_campaigns(&state, ctx.account_id, &q).await?;
    let n = rows.len() as i64;
    Ok(page(rows, n))
}

#[derive(Deserialize, Default)]
pub struct CampaignBody {
    pub name: Option<String>,
    pub channel: Option<String>,
    pub audience: Option<Value>,
    pub content: Option<Value>,
    pub send_at: Option<DateTime<Utc>>,
}

fn validate_audience(v: &Value) -> AppResult<()> {
    serde_json::from_value::<Audience>(v.clone()).map(|_| ()).map_err(|e| AppError::bad(format!("invalid audience: {e}")))
}

pub async fn create_campaign(state: &AppState, account_id: Uuid, b: CampaignBody) -> AppResult<CampaignRow> {
    let channel = b.channel.unwrap_or_else(|| "email".into());
    check_channel(&channel)?;
    let name = b.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("{} campaign {}", if channel == "sms" { "SMS" } else { "Email" }, Utc::now().format("%b %-d")));
    let audience = b.audience.unwrap_or(json!({"include": [], "exclude": []}));
    validate_audience(&audience)?;
    let content = b.content.unwrap_or_else(|| if channel == "sms" { json!({"body": ""}) } else { json!({"subject": "", "blocks": []}) });
    Ok(sqlx::query_as("INSERT INTO campaigns (id, account_id, name, channel, audience, content) VALUES ($1, $2, $3, $4, $5, $6) RETURNING *")
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(name.trim())
        .bind(channel)
        .bind(audience)
        .bind(content)
        .fetch_one(&state.db)
        .await?)
}

async fn create_campaign_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<CampaignBody>) -> AppResult<Json<Value>> {
    let c = create_campaign(&state, ctx.account_id, b).await?;
    Ok(Json(campaign_json(&state, &c).await?))
}

async fn get_campaign_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let c = campaigns::load(&state.db, ctx.account_id, id).await?;
    Ok(Json(campaign_json(&state, &c).await?))
}

pub async fn update_campaign(state: &AppState, account_id: Uuid, id: Uuid, b: CampaignBody) -> AppResult<CampaignRow> {
    let c = campaigns::load(&state.db, account_id, id).await?;
    if !matches!(c.status.as_str(), "draft" | "scheduled" | "failed" | "cancelled") {
        return Err(AppError::Conflict(format!("this campaign is {} and can't be edited; duplicate it instead", c.status)));
    }
    if let Some(a) = &b.audience {
        validate_audience(a)?;
    }
    if let Some(ch) = &b.channel {
        check_channel(ch)?;
    }
    Ok(sqlx::query_as(
        "UPDATE campaigns SET name = COALESCE($3, name), channel = COALESCE($4, channel), audience = COALESCE($5, audience),
            content = COALESCE($6, content), send_at = COALESCE($7, send_at), updated_at = now(),
            status = CASE WHEN status IN ('failed','cancelled') THEN 'draft' ELSE status END, error = NULL
         WHERE id = $1 AND account_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(account_id)
    .bind(b.name.filter(|n| !n.trim().is_empty()))
    .bind(b.channel)
    .bind(b.audience)
    .bind(b.content)
    .bind(b.send_at)
    .fetch_one(&state.db)
    .await?)
}

async fn patch_campaign_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<CampaignBody>) -> AppResult<Json<Value>> {
    let c = update_campaign(&state, ctx.account_id, id, b).await?;
    Ok(Json(campaign_json(&state, &c).await?))
}

async fn delete_campaign_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let c = campaigns::load(&state.db, ctx.account_id, id).await?;
    if matches!(c.status.as_str(), "sending" | "sent") {
        return Err(AppError::Conflict("sent campaigns are kept for reporting and can't be deleted".into()));
    }
    sqlx::query("DELETE FROM campaigns WHERE id = $1").bind(id).execute(&state.db).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct EstimateBody {
    pub channel: String,
    pub audience: Value,
}

async fn estimate_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<EstimateBody>) -> AppResult<Json<Value>> {
    check_channel(&b.channel)?;
    let a: Audience = serde_json::from_value(b.audience).map_err(|e| AppError::bad(format!("invalid audience: {e}")))?;
    if a.include.is_empty() {
        return Ok(Json(json!({"count": 0})));
    }
    Ok(Json(json!({"count": campaigns::estimate(&state.db, ctx.account_id, &b.channel, &a).await?})))
}

#[derive(Deserialize)]
struct TestBody {
    to: Vec<String>,
}

async fn test_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<TestBody>) -> AppResult<Json<Value>> {
    let c = campaigns::load(&state.db, ctx.account_id, id).await?;
    let account = load_account(&state.db, ctx.account_id).await?;
    let sent = campaigns::send_test(&state, &account, &c, &b.to).await?;
    Ok(Json(json!({"sent": sent})))
}

/// Move a draft to scheduled (future time) or sending (now), after readiness and plan checks.
pub async fn launch(state: &AppState, account_id: Uuid, id: Uuid, send_at: Option<DateTime<Utc>>) -> AppResult<CampaignRow> {
    let c = campaigns::load(&state.db, account_id, id).await?;
    if !matches!(c.status.as_str(), "draft" | "scheduled") {
        return Err(AppError::Conflict(format!("this campaign is already {}", c.status)));
    }
    let account = load_account(&state.db, account_id).await?;
    let problems = campaigns::readiness(&account, &c);
    if !problems.is_empty() {
        return Err(AppError::bad(format!("Not ready to send: {}", problems.join("; "))));
    }
    let audience: Audience = serde_json::from_value(c.audience.clone()).unwrap_or_default();
    let count = campaigns::estimate(&state.db, account_id, &c.channel, &audience).await?;
    if count == 0 {
        return Err(AppError::bad("nobody in this audience can receive this campaign (check consent and that profiles have an address)"));
    }
    crate::plans::check_send(&state.db, state.cfg.hosted, account_id, &account.plan, &c.channel, count).await?;
    // Validate the template renders before committing to a send.
    render_preview(state, account_id, RenderBody { channel: c.channel.clone(), content: c.content.clone(), profile_id: None, event: None }).await?;
    let now = Utc::now();
    let row: CampaignRow = match send_at.filter(|t| *t > now + chrono::Duration::seconds(30)) {
        Some(t) => sqlx::query_as("UPDATE campaigns SET status = 'scheduled', send_at = $2, updated_at = now() WHERE id = $1 RETURNING *").bind(id).bind(t).fetch_one(&state.db).await?,
        None => {
            let r: CampaignRow = sqlx::query_as("UPDATE campaigns SET status = 'sending', send_at = now(), updated_at = now() WHERE id = $1 RETURNING *").bind(id).fetch_one(&state.db).await?;
            crate::jobs::enqueue(&state.db, "campaign_send", json!({"campaign_id": id}), None).await?;
            r
        }
    };
    Ok(row)
}

#[derive(Deserialize)]
struct ScheduleBody {
    send_at: DateTime<Utc>,
}

async fn schedule_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<ScheduleBody>) -> AppResult<Json<Value>> {
    if b.send_at < Utc::now() {
        return Err(AppError::bad("pick a time in the future, or send now"));
    }
    let c = launch(&state, ctx.account_id, id, Some(b.send_at)).await?;
    Ok(Json(campaign_json(&state, &c).await?))
}

async fn send_now_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let c = launch(&state, ctx.account_id, id, None).await?;
    Ok(Json(campaign_json(&state, &c).await?))
}

pub async fn cancel(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<CampaignRow> {
    let c = campaigns::load(&state.db, account_id, id).await?;
    let new_status = match c.status.as_str() {
        "scheduled" => "draft",
        "sending" => "cancelled",
        other => return Err(AppError::Conflict(format!("a {other} campaign can't be cancelled"))),
    };
    sqlx::query("UPDATE messages SET status = 'skipped', error = 'campaign cancelled' WHERE campaign_id = $1 AND status = 'queued'").bind(id).execute(&state.db).await?;
    Ok(sqlx::query_as("UPDATE campaigns SET status = $2, updated_at = now() WHERE id = $1 RETURNING *").bind(id).bind(new_status).fetch_one(&state.db).await?)
}

async fn cancel_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let c = cancel(&state, ctx.account_id, id).await?;
    Ok(Json(campaign_json(&state, &c).await?))
}

async fn duplicate_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let c = campaigns::load(&state.db, ctx.account_id, id).await?;
    let copy = create_campaign(&state, ctx.account_id, CampaignBody { name: Some(format!("{} (copy)", c.name)), channel: Some(c.channel), audience: Some(c.audience), content: Some(c.content), send_at: None }).await?;
    Ok(Json(campaign_json(&state, &copy).await?))
}

pub async fn report(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let c = campaigns::load(&state.db, account_id, id).await?;
    let stats = campaigns::stats(&state.db, account_id, id).await?;
    let links: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT mc.url, count(*), count(DISTINCT mc.message_id) FROM message_clicks mc JOIN messages m ON m.id = mc.message_id
         WHERE m.campaign_id = $1 AND NOT m.is_test GROUP BY mc.url ORDER BY 2 DESC LIMIT 50",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let tz: String = sqlx::query_scalar("SELECT timezone FROM accounts WHERE id = $1").bind(account_id).fetch_one(&state.db).await?;
    let series: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT to_char(h, 'YYYY-MM-DD\"T\"HH24:00'), opens, clicks FROM (
           SELECT date_trunc('hour', t AT TIME ZONE $2) AS h, count(*) FILTER (WHERE k = 'o') AS opens, count(*) FILTER (WHERE k = 'c') AS clicks
           FROM (SELECT opened_at AS t, 'o' AS k FROM messages WHERE campaign_id = $1 AND opened_at IS NOT NULL AND NOT is_test
                 UNION ALL SELECT clicked_at, 'c' FROM messages WHERE campaign_id = $1 AND clicked_at IS NOT NULL AND NOT is_test) x
           GROUP BY 1) y ORDER BY 1 LIMIT 720",
    )
    .bind(id)
    .bind(&tz)
    .fetch_all(&state.db)
    .await?;
    Ok(json!({
        "campaign": {"id": c.id, "name": c.name, "channel": c.channel, "status": c.status, "sent_at": c.sent_at},
        "stats": stats,
        "links": links.into_iter().map(|(url, clicks, unique)| json!({"url": url, "clicks": clicks, "unique_clicks": unique})).collect::<Vec<_>>(),
        "series": series.into_iter().map(|(hour, opens, clicks)| json!({"hour": hour, "opens": opens, "clicks": clicks})).collect::<Vec<_>>(),
    }))
}

async fn report_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(report(&state, ctx.account_id, id).await?))
}

// ---------- Flows ----------

pub async fn flow_json(state: &AppState, f: &FlowRow) -> AppResult<Value> {
    let (stats, active, total) = flows::stats(&state.db, f.account_id, f.id).await?;
    let mut v = serde_json::to_value(f)?;
    let mut s = serde_json::to_value(stats)?;
    s["active_runs"] = json!(active);
    s["total_runs"] = json!(total);
    if let Some(o) = v.as_object_mut() {
        o.remove("account_id");
        o.insert("stats".into(), s);
    }
    Ok(v)
}

pub async fn load_flow(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<FlowRow> {
    sqlx::query_as("SELECT * FROM flows WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::not_found("flow"))
}

pub async fn list_flows(state: &AppState, account_id: Uuid) -> AppResult<Vec<Value>> {
    let rows: Vec<FlowRow> = sqlx::query_as("SELECT * FROM flows WHERE account_id = $1 ORDER BY (status = 'live') DESC, updated_at DESC").bind(account_id).fetch_all(&state.db).await?;
    let mut out = Vec::new();
    for f in &rows {
        out.push(flow_json(state, f).await?);
    }
    Ok(out)
}

async fn flows_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let rows = list_flows(&state, ctx.account_id).await?;
    let n = rows.len() as i64;
    Ok(page(rows, n))
}

#[derive(Deserialize, Default)]
pub struct FlowBody {
    pub name: Option<String>,
    pub trigger: Option<Value>,
    pub definition: Option<Value>,
}

async fn check_trigger_refs(state: &AppState, account_id: Uuid, t: &flows::Trigger) -> AppResult<()> {
    match t {
        flows::Trigger::List { list_id } => crate::audience::list_belongs(&state.db, account_id, *list_id).await,
        flows::Trigger::Segment { segment_id } => super::audience::get_segment(state, account_id, *segment_id).await.map(|_| ()),
        _ => Ok(()),
    }
}

pub async fn create_flow(state: &AppState, account_id: Uuid, b: FlowBody) -> AppResult<FlowRow> {
    let name = b.name.filter(|n| !n.trim().is_empty()).ok_or_else(|| AppError::bad("give the flow a name"))?;
    let trigger = b.trigger.ok_or_else(|| AppError::bad("a flow needs a trigger"))?;
    let t = flows::parse_trigger(&trigger)?;
    check_trigger_refs(state, account_id, &t).await?;
    let def = b.definition.unwrap_or(json!({"entry": null, "nodes": []}));
    flows::parse_definition(&def)?;
    Ok(sqlx::query_as("INSERT INTO flows (id, account_id, name, trigger, definition) VALUES ($1, $2, $3, $4, $5) RETURNING *")
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(name.trim())
        .bind(serde_json::to_value(&t)?)
        .bind(def)
        .fetch_one(&state.db)
        .await?)
}

async fn create_flow_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<FlowBody>) -> AppResult<Json<Value>> {
    let f = create_flow(&state, ctx.account_id, b).await?;
    Ok(Json(flow_json(&state, &f).await?))
}

async fn get_flow_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let f = load_flow(&state, ctx.account_id, id).await?;
    Ok(Json(flow_json(&state, &f).await?))
}

pub async fn update_flow(state: &AppState, account_id: Uuid, id: Uuid, b: FlowBody) -> AppResult<FlowRow> {
    load_flow(state, account_id, id).await?;
    let trigger = match &b.trigger {
        Some(t) => {
            let t = flows::parse_trigger(t)?;
            check_trigger_refs(state, account_id, &t).await?;
            Some(serde_json::to_value(&t)?)
        }
        None => None,
    };
    if let Some(d) = &b.definition {
        flows::parse_definition(d)?;
    }
    Ok(sqlx::query_as("UPDATE flows SET name = COALESCE($3, name), trigger = COALESCE($4, trigger), definition = COALESCE($5, definition), updated_at = now() WHERE id = $1 AND account_id = $2 RETURNING *")
        .bind(id)
        .bind(account_id)
        .bind(b.name.filter(|n| !n.trim().is_empty()))
        .bind(trigger)
        .bind(b.definition)
        .fetch_one(&state.db)
        .await?)
}

async fn patch_flow_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<FlowBody>) -> AppResult<Json<Value>> {
    let f = update_flow(&state, ctx.account_id, id, b).await?;
    Ok(Json(flow_json(&state, &f).await?))
}

async fn delete_flow_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let f = load_flow(&state, ctx.account_id, id).await?;
    if f.status == "live" {
        return Err(AppError::Conflict("turn the flow off before deleting it".into()));
    }
    sqlx::query("DELETE FROM flows WHERE id = $1").bind(id).execute(&state.db).await?;
    Ok(Json(json!({"ok": true})))
}

pub async fn set_flow_status(state: &AppState, account_id: Uuid, id: Uuid, status: &str) -> AppResult<FlowRow> {
    if !matches!(status, "live" | "paused" | "draft") {
        return Err(AppError::bad("status must be live, paused or draft"));
    }
    let f = load_flow(state, account_id, id).await?;
    if status == "live" {
        let def = flows::parse_definition(&f.definition)?;
        if def.entry.is_none() {
            return Err(AppError::bad("add at least one step before turning the flow on"));
        }
        let account = load_account(&state.db, account_id).await?;
        let has_email = def.nodes.iter().any(|n| n.kind == "email");
        let has_sms = def.nodes.iter().any(|n| n.kind == "sms");
        if has_email && account.from_email.is_none() {
            return Err(AppError::bad("set a sender email in Settings → Sending before turning on a flow with emails"));
        }
        if has_email && account.address.as_deref().unwrap_or("").is_empty() {
            return Err(AppError::bad("add your mailing address in Settings (required by CAN-SPAM) before turning on email flows"));
        }
        if has_sms && account.sms_from.is_none() {
            return Err(AppError::bad("set an SMS sending number in Settings → SMS before turning on a flow with texts"));
        }
        for n in &def.nodes {
            if n.kind == "email" || n.kind == "sms" {
                let content = n.data.get("content").cloned().unwrap_or(json!({}));
                render_preview(state, account_id, RenderBody { channel: n.kind.clone(), content, profile_id: None, event: Some(json!({"properties": {}})) })
                    .await
                    .map_err(|e| AppError::bad(format!("step {}: {}", n.id, e.public_message())))?;
            }
        }
        // Segment triggers: snapshot current members so only new entrants start the flow.
        if let Ok(flows::Trigger::Segment { segment_id }) = flows::parse_trigger(&f.trigger) {
            let seg = super::audience::get_segment(state, account_id, segment_id).await?;
            if seg.refreshed_at.is_none() {
                let d = crate::segments::parse(&seg.definition)?;
                crate::segments::refresh(&state.db, account_id, segment_id, &d).await?;
            }
        }
    }
    let row: FlowRow = sqlx::query_as("UPDATE flows SET status = $2, updated_at = now() WHERE id = $1 RETURNING *").bind(id).bind(status).fetch_one(&state.db).await?;
    if status == "live" {
        sqlx::query("UPDATE accounts SET onboarding = onboarding || '{\"first_flow\": true}' WHERE id = $1").bind(account_id).execute(&state.db).await?;
    }
    if status == "draft" {
        sqlx::query("UPDATE flow_runs SET status = 'exited', finished_at = now() WHERE flow_id = $1 AND status = 'active'").bind(id).execute(&state.db).await?;
    }
    Ok(row)
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

async fn flow_status_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<StatusBody>) -> AppResult<Json<Value>> {
    let f = set_flow_status(&state, ctx.account_id, id, &b.status).await?;
    Ok(Json(flow_json(&state, &f).await?))
}

pub async fn flow_report(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let f = load_flow(state, account_id, id).await?;
    let (stats, active, total) = flows::stats(&state.db, account_id, id).await?;
    let nodes = flows::node_report(&state.db, account_id, &f).await?;
    Ok(json!({"flow": {"id": f.id, "name": f.name, "status": f.status}, "stats": stats, "active_runs": active, "total_runs": total, "nodes": nodes}))
}

async fn flow_report_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(flow_report(&state, ctx.account_id, id).await?))
}

pub async fn blueprints_json(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    let account = load_account(&state.db, account_id).await?;
    let data: Vec<Value> = flows::blueprints(account.website.as_deref().unwrap_or(""))
        .into_iter()
        .map(|mut b| {
            if let Some(o) = b.as_object_mut() {
                o.remove("segment");
            }
            b
        })
        .collect();
    Ok(json!({"data": data}))
}

async fn blueprints_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(blueprints_json(&state, ctx.account_id).await?))
}

#[derive(Deserialize)]
struct BlueprintBody {
    key: String,
    name: Option<String>,
}

async fn from_blueprint_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<BlueprintBody>) -> AppResult<Json<Value>> {
    let f = flows::from_blueprint(&state.db, ctx.account_id, &b.key, b.name).await?;
    Ok(Json(flow_json(&state, &f).await?))
}

// ---------- Forms ----------

fn form_json(f: &FormRow) -> Value {
    let mut v = serde_json::to_value(f).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("stats".into(), json!({"views": f.views, "submits": f.submits}));
    }
    v
}

async fn forms_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    let rows: Vec<FormRow> = sqlx::query_as("SELECT * FROM forms WHERE account_id = $1 ORDER BY updated_at DESC").bind(ctx.account_id).fetch_all(&state.db).await?;
    let n = rows.len() as i64;
    Ok(page(rows.iter().map(form_json).collect(), n))
}

#[derive(Deserialize, Default)]
pub struct FormBody {
    pub name: Option<String>,
    pub kind: Option<String>,
    pub list_id: Option<Uuid>,
    pub config: Option<Value>,
    pub status: Option<String>,
}

fn default_form_config() -> Value {
    json!({
        "headline": "Get 10% off your first order",
        "body": "Join our list for early access to new arrivals and member-only offers.",
        "button_text": "Sign up",
        "success_message": "Thanks! Check your inbox.",
        "collect": ["email"],
        "delay_seconds": 5
    })
}

pub async fn create_form(state: &AppState, account_id: Uuid, b: FormBody) -> AppResult<FormRow> {
    let kind = b.kind.unwrap_or_else(|| "popup".into());
    if !matches!(kind.as_str(), "popup" | "embed" | "flyout" | "page") {
        return Err(AppError::bad("kind must be popup, embed, flyout or page"));
    }
    if let Some(l) = b.list_id {
        crate::audience::list_belongs(&state.db, account_id, l).await?;
    }
    let mut config = default_form_config();
    if let (Some(o), Some(Value::Object(extra))) = (config.as_object_mut(), b.config) {
        o.extend(extra);
    }
    Ok(sqlx::query_as("INSERT INTO forms (id, account_id, name, kind, list_id, config) VALUES ($1, $2, $3, $4, $5, $6) RETURNING *")
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(b.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Sign-up form".into()))
        .bind(kind)
        .bind(b.list_id)
        .bind(config)
        .fetch_one(&state.db)
        .await?)
}

async fn create_form_h(State(state): State<AppState>, ctx: Ctx, ApiJson(b): ApiJson<FormBody>) -> AppResult<Json<Value>> {
    Ok(Json(form_json(&create_form(&state, ctx.account_id, b).await?)))
}

async fn get_form_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let f: Option<FormRow> = sqlx::query_as("SELECT * FROM forms WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).fetch_optional(&state.db).await?;
    Ok(Json(form_json(&f.ok_or_else(|| AppError::not_found("form"))?)))
}

pub async fn update_form(state: &AppState, account_id: Uuid, id: Uuid, b: FormBody) -> AppResult<FormRow> {
    if let Some(k) = &b.kind {
        if !matches!(k.as_str(), "popup" | "embed" | "flyout" | "page") {
            return Err(AppError::bad("kind must be popup, embed, flyout or page"));
        }
    }
    if let Some(s) = &b.status {
        if !matches!(s.as_str(), "draft" | "live") {
            return Err(AppError::bad("status must be draft or live"));
        }
    }
    if let Some(l) = b.list_id {
        crate::audience::list_belongs(&state.db, account_id, l).await?;
    }
    let f: Option<FormRow> = sqlx::query_as(
        "UPDATE forms SET name = COALESCE($3, name), kind = COALESCE($4, kind), list_id = COALESCE($5, list_id),
           config = CASE WHEN $6::jsonb IS NULL THEN config ELSE config || $6 END, status = COALESCE($7, status), updated_at = now()
         WHERE id = $1 AND account_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(account_id)
    .bind(b.name.filter(|n| !n.trim().is_empty()))
    .bind(b.kind)
    .bind(b.list_id)
    .bind(b.config)
    .bind(b.status)
    .fetch_optional(&state.db)
    .await?;
    f.ok_or_else(|| AppError::not_found("form"))
}

async fn patch_form_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>, ApiJson(b): ApiJson<FormBody>) -> AppResult<Json<Value>> {
    Ok(Json(form_json(&update_form(&state, ctx.account_id, id, b).await?)))
}

async fn delete_form_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    let n = sqlx::query("DELETE FROM forms WHERE id = $1 AND account_id = $2").bind(id).bind(ctx.account_id).execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("form"));
    }
    Ok(Json(json!({"ok": true})))
}
