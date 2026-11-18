//! Asking for reviews: review links, making requests (never twice for an order line, never for
//! something the person reviewed in the last 12 months), the `review_request` flow step, and the
//! "Review request" and "Reviews" email blocks and `{% review_link %}` tag.
//!
//! A review request is marketing: it goes only to people subscribed on that channel, through the
//! normal send path (billing, guardrails, quiet hours, smart sending, the opt-out check).

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::store::{self, Hint, Subject};
use crate::audience::{record_event, NewEvent};
use crate::error::{AppError, AppResult};
use crate::features::content::blocks::{inert, safe_href, safe_src};
use crate::models::{Account, FlowRow, Profile};
use crate::sending::{self, Outcome, SendSpec};
use crate::state::AppState;

pub const READY_METRIC: &str = "Ready To Review";
pub const SUBMITTED_METRIC: &str = "Submitted Review";
/// Most items one ask covers.
pub const MAX_ITEMS: usize = 6;
/// Someone who reviewed a product isn't asked about it again for this long.
const ASKED_REVIEWED_DAYS: i32 = 365;

// ---------- Links ----------

pub fn token(state: &AppState, id: Uuid) -> String {
    crate::auth::sign(&state.cfg.secret_key, &format!("rv{}", id.simple()))
}

pub fn id_from_token(state: &AppState, token: &str) -> Option<Uuid> {
    crate::auth::verify(&state.cfg.secret_key, token)?.strip_prefix("rv")?.parse().ok()
}

pub fn link(state: &AppState, account: &Account, id: Uuid) -> String {
    format!("{}/e/review/{}", crate::domains::link_base(state, account), token(state, id))
}

/// Where previews and test sends point: a sample page that saves nothing.
pub fn preview_link(state: &AppState, account: &Account) -> String {
    format!("{}/e/review/preview", crate::domains::link_base(state, account))
}

// ---------- Requests ----------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Request {
    pub id: Uuid,
    pub account_id: Uuid,
    pub profile_id: Uuid,
    pub subject_id: Uuid,
    pub batch_id: Uuid,
    pub source: String,
    pub verified: bool,
    pub item: Value,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub sent_at: Option<DateTime<Utc>>,
    pub reminded_at: Option<DateTime<Utc>>,
    pub review_id: Option<Uuid>,
    pub submitted_at: Option<DateTime<Utc>>,
}

pub const REQUEST_COLS: &str = "q.id, q.account_id, q.profile_id, q.subject_id, q.batch_id, q.source, q.verified, q.item, q.created_at, q.expires_at, q.sent_at, q.reminded_at, q.review_id, q.submitted_at";

impl Request {
    pub fn open(&self) -> bool {
        self.review_id.is_none() && self.expires_at > Utc::now()
    }
    pub fn name(&self) -> &str {
        self.item["name"].as_str().unwrap_or("your purchase")
    }
}

pub async fn get(db: &PgPool, id: Uuid) -> AppResult<Option<Request>> {
    let q = format!("SELECT {REQUEST_COLS} FROM review_requests q WHERE q.id = $1");
    Ok(sqlx::query_as(&q).bind(id).fetch_optional(db).await?)
}

/// One thing to ask about.
#[derive(Debug, Clone, Default)]
pub struct Item {
    /// The store's product id (or the account's own key for a listing or service).
    pub key: String,
    pub name: Option<String>,
    pub image_url: Option<String>,
    pub url: Option<String>,
    pub kind: Option<String>,
    /// The order it came from, when known (one ask per order line).
    pub order_id: Option<String>,
}

fn sv(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The items an event is about: `properties.items` (the catalog's item shape), or the event's own
/// product or subject (`product_id`, `listing_id`, `subject_id`). At most [`MAX_ITEMS`], each once.
pub fn items_from_event(event: Option<&Value>) -> Vec<Item> {
    let Some(ev) = event else { return Vec::new() };
    let props = ev.get("properties").unwrap_or(ev);
    let order = props.get("order_id").and_then(sv).or_else(|| props.get("reservation_id").and_then(sv)).or_else(|| props.get("booking_id").and_then(sv));
    let one = |o: &Value| -> Option<Item> {
        let key = ["product_id", "listing_id", "subject_id", "id", "sku"].iter().find_map(|k| o.get(*k).and_then(sv))?;
        Some(Item {
            key,
            name: o.get("name").or_else(|| o.get("title")).or_else(|| o.get("listing_name")).and_then(sv),
            image_url: o.get("image_url").or_else(|| o.get("image")).and_then(sv),
            url: o.get("url").and_then(sv),
            kind: o.get("kind").and_then(sv).or_else(|| o.get("listing_id").map(|_| "listing".into())),
            order_id: order.clone(),
        })
    };
    let mut out: Vec<Item> = match props.get("items").and_then(|i| i.as_array()) {
        Some(list) if !list.is_empty() => list.iter().filter_map(one).collect(),
        _ => one(props).into_iter().collect(),
    };
    let mut seen = std::collections::HashSet::new();
    out.retain(|i| seen.insert(i.key.clone()));
    out.truncate(MAX_ITEMS);
    out
}

pub struct Make<'a> {
    pub source: &'a str,
    /// The purchase came from a trusted source (not a browser).
    pub verified: bool,
    pub batch_id: Uuid,
    pub flow_run_id: Option<Uuid>,
}

pub enum Made {
    New(Request),
    Existing(Request),
    /// Why nobody is asked: `reviewed` (in the last 12 months) or `asked` (this order line was asked before).
    Skipped(&'static str),
}

/// A request for one person and one item, or the open one they already have. Checked and written
/// under a lock on the person and the item, so two sends at once can't both ask.
pub async fn make(state: &AppState, account: &Account, profile_id: Uuid, item: &Item, m: &Make<'_>) -> AppResult<Made> {
    let db = &state.db;
    let hint = Hint { name: item.name.clone(), image_url: item.image_url.clone(), url: item.url.clone(), kind: item.kind.clone(), trusted: m.verified };
    let subject = store::subject_for(db, account.id, &item.key, &hint).await?;
    let s = store::settings(db, account.id).await?;
    let line_key = item.order_id.as_deref().map(|o| format!("{}:{}:{}", profile_id.simple(), o.chars().take(200).collect::<String>(), subject.id.simple()));
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 81))").bind(format!("rv:{profile_id}:{}", subject.id)).execute(&mut *tx).await?;
    let reviewed: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM reviews WHERE profile_id = $1 AND subject_id = $2 AND created_at > now() - make_interval(days => $3))")
        .bind(profile_id)
        .bind(subject.id)
        .bind(ASKED_REVIEWED_DAYS)
        .fetch_one(&mut *tx)
        .await?;
    if reviewed {
        return Ok(Made::Skipped("reviewed"));
    }
    let q = format!("SELECT {REQUEST_COLS} FROM review_requests q WHERE q.account_id = $1 AND q.line_key = $2");
    if let Some(k) = &line_key {
        let asked: Option<Request> = sqlx::query_as(&q).bind(account.id).bind(k).fetch_optional(&mut *tx).await?;
        if let Some(r) = asked {
            let same_run: Option<Uuid> = sqlx::query_scalar("SELECT flow_run_id FROM review_requests WHERE id = $1").bind(r.id).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            // The same flow run asking again (a retried step) gets its own request back.
            return Ok(if m.flow_run_id.is_some() && same_run == m.flow_run_id && r.open() { Made::Existing(r) } else { Made::Skipped("asked") });
        }
    }
    let q = format!("SELECT {REQUEST_COLS} FROM review_requests q WHERE q.profile_id = $1 AND q.subject_id = $2 AND q.review_id IS NULL AND q.expires_at > now() ORDER BY q.created_at DESC LIMIT 1");
    let open: Option<Request> = sqlx::query_as(&q).bind(profile_id).bind(subject.id).fetch_optional(&mut *tx).await?;
    if let Some(r) = open {
        if let Some(k) = &line_key {
            sqlx::query("UPDATE review_requests SET line_key = $2 WHERE id = $1 AND line_key IS NULL").bind(r.id).bind(k).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        return Ok(Made::Existing(r));
    }
    // What this person sees: their own item's name and image, or the subject's when trusted.
    let shown = json!({
        "name": if subject.trusted { Some(subject.name.clone()) } else { item.name.clone().or(Some(subject.name.clone())) },
        "image_url": if subject.trusted { subject.image_url.clone() } else { item.image_url.clone().filter(|u| u.starts_with("https://") || u.starts_with("http://")) },
        "product_id": subject.external_id,
    });
    let id = Uuid::now_v7();
    let q = 
        "INSERT INTO review_requests (id, account_id, profile_id, subject_id, batch_id, line_key, source, verified, item, flow_run_id, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now() + make_interval(days => $11))
         RETURNING id, account_id, profile_id, subject_id, batch_id, source, verified, item, created_at, expires_at, sent_at, reminded_at, review_id, submitted_at";
    let r: Request = sqlx::query_as(q)
        .bind(id)
        .bind(account.id)
        .bind(profile_id)
        .bind(subject.id)
        .bind(m.batch_id)
        .bind(&line_key)
        .bind(m.source)
        .bind(m.verified)
        .bind(&shown)
        .bind(m.flow_run_id)
        .bind(s.request_days)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Made::New(r))
}

/// "Ready To Review": the person was asked about an item (once per request).
pub async fn ready_event(state: &AppState, r: &Request, subject: &Subject) -> AppResult<()> {
    record_event(
        &state.db,
        NewEvent {
            account_id: r.account_id,
            profile_id: r.profile_id,
            metric: READY_METRIC,
            properties: json!({"product_id": subject.external_id, "product_name": r.item["name"], "request_id": r.id, "verified": r.verified, "source": r.source}),
            value: None,
            occurred_at: None,
            unique_id: Some(format!("rtr:{}", r.id.simple())),
            message_id: None,
        },
    )
    .await?;
    Ok(())
}

fn may_receive(p: &Profile, channel: &str) -> bool {
    match channel {
        "sms" => p.sms_consent == "subscribed" && p.phone_number.is_some(),
        _ => p.email_consent == "subscribed" && p.email.is_some(),
    }
}

async fn by_ids(db: &PgPool, account_id: Uuid, profile_id: Uuid, ids: &[Uuid]) -> AppResult<Vec<Request>> {
    let q = format!("SELECT {REQUEST_COLS} FROM review_requests q WHERE q.account_id = $1 AND q.profile_id = $2 AND q.id = ANY($3) ORDER BY q.created_at, q.id");
    Ok(sqlx::query_as(&q).bind(account_id).bind(profile_id).bind(ids).fetch_all(db).await?)
}

fn ids_in(v: Option<&Value>) -> Vec<Uuid> {
    v.and_then(|v| v.as_array()).into_iter().flatten().filter_map(|x| x.as_str()?.parse().ok()).take(MAX_ITEMS * 2).collect()
}

// ---------- Flow step ----------

/// Check a `review_request` flow step: `channel` email (default) or sms, `content` like an email or
/// text step, and `reminder` (ask again about this run's earlier requests that are still open).
pub fn check_step(id: &str, data: &Map<String, Value>) -> AppResult<()> {
    let channel = data.get("channel").and_then(|v| v.as_str()).unwrap_or("email");
    if !matches!(channel, "email" | "sms") {
        return Err(AppError::bad(format!("review request {id}: channel must be email or sms")));
    }
    let content = data.get("content").filter(|c| c.is_object()).ok_or_else(|| AppError::bad(format!("review request step {id} needs `content`")))?;
    if channel == "sms" && content.get("body").and_then(|b| b.as_str()).is_none_or(|b| b.trim().is_empty()) {
        return Err(AppError::bad(format!("review request {id}: a text needs a body (the link is added with {{% review_link %}} or at the end)")));
    }
    Ok(())
}

/// Run a `review_request` step for one person. Returns when to try again if the message had to
/// wait (quiet hours, warm-up). Requests made here are kept in the run's context, so a retry asks
/// about the same ones and a later reminder step can find them.
#[allow(clippy::too_many_arguments)]
pub async fn flow_step(
    state: &AppState,
    account: &Account,
    person: &Profile,
    flow: &FlowRow,
    run_id: Uuid,
    node_id: &str,
    data: &Map<String, Value>,
    context: &mut Value,
    smart_hours: i64,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    let err = |e: AppError| anyhow::anyhow!(e.public_message());
    let channel = if data.get("channel").and_then(|v| v.as_str()) == Some("sms") { "sms" } else { "email" };
    let reminder = data.get("reminder").and_then(|v| v.as_bool()) == Some(true);
    let pending_key = format!("_review_pending_{node_id}");
    let pending = ids_in(context.get(&pending_key));
    // A review request is marketing: subscribed people only (checked again on a retry), and smart sending applies.
    if !may_receive(person, channel) {
        if let Some(o) = context.as_object_mut() {
            o.remove(&pending_key);
        }
        return Ok(None);
    }
    if pending.is_empty() && smart_hours > 0 && crate::features::sendtime::recently_messaged(&state.db, person.id, channel, smart_hours).await? {
        return Ok(None);
    }
    let mut created: Vec<Uuid> = Vec::new();
    let requests: Vec<Request> = if !pending.is_empty() {
        by_ids(&state.db, account.id, person.id, &pending).await.map_err(err)?.into_iter().filter(Request::open).collect()
    } else if reminder {
        let earlier = ids_in(context.get("review_requests"));
        // Claimed before sending, so a reminder goes at most once per request.
        let claimed: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE review_requests SET reminded_at = now() WHERE account_id = $1 AND profile_id = $2 AND id = ANY($3)
               AND review_id IS NULL AND expires_at > now() AND reminded_at IS NULL AND sent_at IS NOT NULL RETURNING id",
        )
        .bind(account.id)
        .bind(person.id)
        .bind(&earlier)
        .fetch_all(&state.db)
        .await?;
        by_ids(&state.db, account.id, person.id, &claimed).await.map_err(err)?
    } else {
        let event = context.get("event");
        let verified = event.is_some_and(|e| e.get("from_browser").and_then(|v| v.as_bool()) == Some(false));
        let m = Make { source: "flow", verified, batch_id: Uuid::now_v7(), flow_run_id: Some(run_id) };
        let mut out = Vec::new();
        for item in items_from_event(event) {
            match make(state, account, person.id, &item, &m).await {
                Ok(Made::New(r)) => {
                    created.push(r.id);
                    out.push(r);
                }
                Ok(Made::Existing(r)) => out.push(r),
                Ok(Made::Skipped(_)) => {}
                Err(e) => tracing::warn!(run = %run_id, node = %node_id, error = %e.public_message(), "review request item skipped"),
            }
        }
        out
    };
    if requests.is_empty() {
        if let Some(o) = context.as_object_mut() {
            o.remove(&pending_key);
        }
        return Ok(None);
    }
    let ids: Vec<Uuid> = requests.iter().map(|r| r.id).collect();
    if !reminder {
        let mut all = ids_in(context.get("review_requests"));
        for i in &ids {
            if !all.contains(i) {
                all.push(*i);
            }
        }
        context["review_requests"] = json!(all);
    }
    context[&pending_key] = json!(ids);
    let mut event = context.get("event").cloned().unwrap_or(json!({}));
    event["_review_requests"] = json!(ids);
    let content = data.get("content").cloned().unwrap_or(json!({}));
    let out = sending::send(
        state,
        SendSpec {
            account,
            profile: person,
            channel,
            content: &content,
            campaign_id: None,
            flow_id: Some(flow.id),
            flow_node_id: Some(node_id),
            event: Some(&event),
            is_test: false,
            message_id: None,
            to_override: None,
        },
    )
    .await;
    match out {
        Ok(Outcome::Deferred(until)) => return Ok(Some(until)),
        Ok(Outcome::Sent(_)) => {
            sqlx::query("UPDATE review_requests SET sent_at = COALESCE(sent_at, now()) WHERE id = ANY($1)").bind(&ids).execute(&state.db).await?;
            for r in &requests {
                if let Ok(s) = store::subject_by_id(&state.db, r.subject_id).await {
                    ready_event(state, r, &s).await.map_err(err)?;
                }
            }
        }
        Ok(Outcome::Skipped(_)) | Ok(Outcome::Failed(_)) | Err(_) => {
            if let Err(e) = &out {
                tracing::warn!(run = %run_id, node = %node_id, error = %e.public_message(), "review request message failed");
            }
            // Nothing reached them: the order lines may be asked again later.
            sqlx::query("DELETE FROM review_requests WHERE id = ANY($1) AND sent_at IS NULL AND review_id IS NULL").bind(&created).execute(&state.db).await?;
        }
    }
    if let Some(o) = context.as_object_mut() {
        o.remove(&pending_key);
    }
    Ok(None)
}

// ---------- Email blocks and the review_link tag ----------

static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\{%-?\s*review_link(?:\s+(?:'([^'{}%]{1,255})'|"([^"{}%]{1,255})"))?\s*-?%\}"#).unwrap());

fn block_type(b: &Value) -> &str {
    b.get("type").and_then(|t| t.as_str()).unwrap_or("")
}

fn uses_reviews(content: &Value) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::String(s) => s.contains("review_link"),
            Value::Array(a) => a.iter().any(walk),
            Value::Object(o) => matches!(o.get("type").and_then(|t| t.as_str()), Some("reviews" | "review_request")) || o.values().any(walk),
            _ => false,
        }
    }
    walk(content)
}

/// Requests for one message, made on demand (or handed over by the flow step).
struct Asker<'a> {
    state: &'a AppState,
    account: &'a Account,
    profile: Option<&'a Profile>,
    event: Option<&'a Value>,
    allowed: bool,
    preview: bool,
    given: Option<Vec<Request>>,
    made: Vec<Request>,
    batch: Uuid,
}

impl Asker<'_> {
    /// Requests for `product` (a block's or tag's own product) or the event's items.
    async fn requests(&mut self, product: Option<&str>) -> AppResult<Vec<Request>> {
        if let Some(g) = &self.given {
            let mut list: Vec<Request> = g.clone();
            if let Some(p) = product {
                list.retain(|r| r.item["product_id"].as_str() == Some(p));
            }
            return Ok(list);
        }
        let Some(profile) = self.profile.filter(|_| self.allowed && !self.preview) else { return Ok(Vec::new()) };
        let event_items = items_from_event(self.event);
        let (items, verified) = match product {
            Some(p) => match event_items.iter().find(|i| i.key == p) {
                Some(i) => (vec![i.clone()], self.event_trusted()),
                None => (vec![Item { key: p.to_string(), ..Default::default() }], false),
            },
            None => (event_items, self.event_trusted()),
        };
        let mut out = Vec::new();
        for item in items {
            if let Some(r) = self.made.iter().find(|r| r.item["product_id"].as_str() == Some(item.key.as_str())) {
                out.push(r.clone());
                continue;
            }
            let m = Make { source: "email", verified, batch_id: self.batch, flow_run_id: None };
            match make(self.state, self.account, profile.id, &item, &m).await? {
                Made::New(r) => {
                    if let Ok(s) = store::subject_by_id(&self.state.db, r.subject_id).await {
                        ready_event(self.state, &r, &s).await?;
                    }
                    sqlx::query("UPDATE review_requests SET sent_at = now() WHERE id = $1").bind(r.id).execute(&self.state.db).await?;
                    self.made.push(r.clone());
                    out.push(r);
                }
                Made::Existing(r) => {
                    self.made.push(r.clone());
                    out.push(r);
                }
                Made::Skipped(_) => {}
            }
        }
        Ok(out)
    }

    fn event_trusted(&self) -> bool {
        self.event.is_some_and(|e| e.get("from_browser").and_then(|v| v.as_bool()) == Some(false))
    }
}

fn star_cells(href: Option<&str>, color: &str, size: u32) -> String {
    (1..=5)
        .map(|n| {
            let star = format!(r#"<span style="font-size:{size}px;line-height:1;color:{color};">&#9733;</span>"#);
            match href.and_then(|h| safe_href(&format!("{h}?rating={n}"))) {
                Some(h) => format!(r#"<a href="{h}" title="{n} star{}" style="text-decoration:none;padding:0 2px;">{star}</a>"#, if n == 1 { "" } else { "s" }),
                None => star,
            }
        })
        .collect()
}

fn request_block_html(state: &AppState, account: &Account, b: &Value, requests: &[Request], preview: bool, brand: &str) -> String {
    let font = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
    let button = b.get("button_text").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Write a review");
    let stars_on = b.get("show_stars").and_then(|v| v.as_bool()) != Some(false);
    let star_color = "#f59e0b";
    let rows: Vec<(String, Option<String>, String)> = if preview {
        vec![("Your purchase".to_string(), None, preview_link(state, account))]
    } else {
        requests.iter().map(|r| (r.name().to_string(), r.item["image_url"].as_str().map(String::from), link(state, account, r.id))).collect()
    };
    let mut html = String::new();
    for (name, image, href) in rows {
        let img = image.as_deref().and_then(safe_src).map(|src| format!(r#"<img src="{src}" alt="" width="96" style="display:block;width:96px;height:auto;border-radius:6px;margin:0 auto 10px;">"#)).unwrap_or_default();
        let stars = if stars_on { format!(r#"<div style="margin:6px 0 4px;">{}</div><div style="font-family:{font};font-size:12px;color:#6b7280;">Tap a star to start</div>"#, star_cells(Some(&href), star_color, 28)) } else { String::new() };
        let btn = safe_href(&href).map(|h| format!(r#"<p style="margin:12px 0 0;"><a href="{h}" style="display:inline-block;padding:10px 22px;background:{brand};color:#ffffff;border-radius:6px;font-family:{font};font-weight:600;text-decoration:none;">{}</a></p>"#, inert(button))).unwrap_or_default();
        html.push_str(&format!(
            r#"<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="margin:0 0 16px;"><tr><td align="center" style="padding:16px;border:1px solid #e5e7eb;border-radius:8px;">{img}<p style="margin:0;font-family:{font};font-size:17px;font-weight:600;color:#111827;">{}</p>{stars}{btn}</td></tr></table>"#,
            inert(&name)
        ));
    }
    html
}

/// A published review as an email shows it (escaped and unable to open a template tag).
fn review_html(r: &store::Review, star_color: &str) -> String {
    let font = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
    let stars: String = (1..=5).map(|n| if n <= r.rating { "&#9733;" } else { "&#9734;" }).collect();
    let title = if r.title.is_empty() { String::new() } else { format!(r#"<strong style="color:#111827;">{}</strong><br>"#, inert(&r.title)) };
    let body: String = r.body.chars().take(300).collect();
    let more = if r.body.chars().count() > 300 { "…" } else { "" };
    let label = if r.incentivized { r#"<br><span style="font-size:12px;color:#6b7280;">Received a discount for this review</span>"# } else { "" };
    let verified = if r.verified { " · Verified buyer" } else { "" };
    format!(
        r#"<p style="margin:0 0 14px;font-family:{font};font-size:15px;line-height:1.5;color:#374151;"><span style="color:{star_color};font-size:16px;">{stars}</span><br>{title}{}{more}<br><span style="font-size:13px;color:#6b7280;">{}{verified}</span>{label}</p>"#,
        inert(&body).replace('\n', "<br>"),
        inert(&r.display_name),
    )
}

async fn reviews_block_html(state: &AppState, account: &Account, b: &Value, event: Option<&Value>) -> AppResult<String> {
    let key = b.get("product_id").and_then(sv).or_else(|| items_from_event(event).into_iter().next().map(|i| i.key));
    let Some(key) = key else { return Ok(String::new()) };
    let Some(subject) = store::find_subject(&state.db, account.id, &key).await? else { return Ok(String::new()) };
    if subject.review_count == 0 {
        return Ok(String::new());
    }
    let count = b.get("count").and_then(|v| v.as_i64()).unwrap_or(3).clamp(1, 5);
    let show = b.get("show").and_then(|v| v.as_str()).unwrap_or("both");
    let star_color = "#f59e0b";
    let font = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
    let mut html = String::new();
    if show != "reviews" {
        let avg = subject.average().unwrap_or(0.0);
        let full = avg.round() as i64;
        let stars: String = (1..=5).map(|n| if n <= full { "&#9733;" } else { "&#9734;" }).collect();
        let name = if subject.trusted { format!("{} · ", inert(&subject.name)) } else { String::new() };
        html.push_str(&format!(
            r#"<p style="margin:0 0 14px;font-family:{font};font-size:15px;color:#111827;"><span style="color:{star_color};font-size:20px;">{stars}</span><br>{name}{avg:.1} out of 5 from {} review{}</p>"#,
            subject.review_count,
            if subject.review_count == 1 { "" } else { "s" }
        ));
    }
    if show != "summary" {
        let q = format!("SELECT {} FROM reviews r WHERE r.subject_id = $1 AND r.status = 'published' ORDER BY r.featured DESC, r.rating DESC, r.reviewed_at DESC LIMIT $2", store::REVIEW_COLS);
        let rows: Vec<store::Review> = sqlx::query_as(&q).bind(subject.id).bind(count).fetch_all(&state.db).await?;
        for r in &rows {
            html.push_str(&review_html(r, star_color));
        }
    }
    Ok(html)
}

fn replace_tags(v: &mut Value, links: &[(Option<String>, String)]) {
    match v {
        Value::String(s) if s.contains("review_link") => {
            let out = TAG.replace_all(s, |c: &regex::Captures<'_>| {
                let want = c.get(1).or(c.get(2)).map(|m| m.as_str().to_string());
                links.iter().find(|(k, _)| *k == want).map(|(_, l)| l.clone()).unwrap_or_default()
            });
            *s = out.into_owned();
        }
        Value::Array(a) => a.iter_mut().for_each(|x| replace_tags(x, links)),
        Value::Object(o) => o.values_mut().for_each(|x| replace_tags(x, links)),
        _ => {}
    }
}

fn tag_products(v: &Value, out: &mut Vec<Option<String>>) {
    match v {
        Value::String(s) if s.contains("review_link") => {
            for c in TAG.captures_iter(s) {
                let want = c.get(1).or(c.get(2)).map(|m| m.as_str().to_string());
                if !out.contains(&want) {
                    out.push(want);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| tag_products(x, out)),
        Value::Object(o) => o.values().for_each(|x| tag_products(x, out)),
        _ => {}
    }
}

/// Fill review blocks and `{% review_link %}` tags for one message: makes (or reuses) this
/// person's requests and turns the blocks into plain HTML blocks. Content without either passes
/// through untouched. `preview` (dashboard previews, test sends) shows sample links and makes
/// nothing. People not subscribed on the message's channel get no review ask: the blocks are left
/// out and the tags are empty.
pub async fn personalize<'c>(state: &AppState, account: &Account, profile: Option<&Profile>, event: Option<&Value>, content: &'c Value, preview: bool) -> AppResult<Cow<'c, Value>> {
    if !uses_reviews(content) {
        return Ok(Cow::Borrowed(content));
    }
    let sms = content.get("body").is_some() && content.get("blocks").is_none() && content.get("html").is_none();
    let allowed = profile.is_some_and(|p| may_receive(p, if sms { "sms" } else { "email" }));
    let given = match (event.map(|e| ids_in(e.get("_review_requests"))), profile) {
        (Some(ids), Some(p)) if !ids.is_empty() => Some(by_ids(&state.db, account.id, p.id, &ids).await?.into_iter().filter(Request::open).collect()),
        _ => None,
    };
    let mut asker = Asker { state, account, profile, event, allowed, preview, given, made: Vec::new(), batch: Uuid::now_v7() };
    let mut out = content.clone();
    let brand = crate::render::safe_color(out.pointer("/theme/brand_color").and_then(|v| v.as_str()).unwrap_or(&account.brand_color), "#1f6f5c");
    if let Some(blocks) = out.get_mut("blocks").and_then(|b| b.as_array_mut()) {
        let mut next = Vec::with_capacity(blocks.len());
        for b in blocks.drain(..) {
            match block_type(&b) {
                "review_request" => {
                    let product = b.get("product_id").and_then(sv);
                    let reqs = asker.requests(product.as_deref()).await?;
                    if preview || !reqs.is_empty() {
                        let heading = b.get("heading").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty());
                        let mut html = heading.map(|h| format!(r#"<p style="margin:0 0 12px;font-size:20px;font-weight:700;color:#111827;">{}</p>"#, inert(h))).unwrap_or_default();
                        html.push_str(&request_block_html(state, account, &b, &reqs, preview, &brand));
                        next.push(json!({"type": "html", "html": html}));
                    }
                }
                "reviews" => {
                    let html = reviews_block_html(state, account, &b, event).await?;
                    if !html.is_empty() {
                        next.push(json!({"type": "html", "html": html}));
                    }
                }
                "columns" => {
                    let mut b = b;
                    if let Some(cols) = b.get_mut("columns").and_then(|c| c.as_array_mut()) {
                        for col in cols.iter_mut() {
                            if let Some(list) = col.as_array_mut() {
                                let mut kept = Vec::new();
                                for x in list.drain(..) {
                                    match block_type(&x) {
                                        "reviews" => {
                                            let html = reviews_block_html(state, account, &x, event).await?;
                                            if !html.is_empty() {
                                                kept.push(json!({"type": "html", "html": html}));
                                            }
                                        }
                                        "review_request" => {
                                            let reqs = asker.requests(x.get("product_id").and_then(sv).as_deref()).await?;
                                            if preview || !reqs.is_empty() {
                                                kept.push(json!({"type": "html", "html": request_block_html(state, account, &x, &reqs, preview, &brand)}));
                                            }
                                        }
                                        _ => kept.push(x),
                                    }
                                }
                                *list = kept;
                            }
                        }
                    }
                    next.push(b);
                }
                _ => next.push(b),
            }
        }
        *blocks = next;
    }
    let mut wanted = Vec::new();
    tag_products(&out, &mut wanted);
    if !wanted.is_empty() {
        let mut links = Vec::new();
        for w in wanted {
            let l = if preview {
                preview_link(state, account)
            } else {
                asker.requests(w.as_deref()).await?.first().map(|r| link(state, account, r.id)).unwrap_or_default()
            };
            links.push((w, l));
        }
        replace_tags(&mut out, &links);
    }
    // A text without the tag still carries its link.
    if sms && !preview {
        if let (Some(body), Some(r)) = (out.get("body").and_then(|b| b.as_str()).map(String::from), asker.given.as_ref().and_then(|g| g.first())) {
            let l = link(state, account, r.id);
            if !body.contains(&l) {
                out["body"] = json!(format!("{} {l}", body.trim_end()));
            }
        }
    }
    Ok(Cow::Owned(out))
}

/// An email or text asking about the requests made for an API call (`create_review_request` with `send`).
pub fn ask_content(channel: &str, product_name: &str) -> Value {
    if channel == "sms" {
        json!({"body": format!("How was {}? Tell us in a quick review: {{% review_link %}}", product_name.chars().take(60).collect::<String>().replace(['{', '}', '%'], ""))})
    } else {
        json!({"subject": "How did we do?", "preheader": "It takes a minute", "blocks": [
            {"type": "heading", "text": "How did we do, {{ first_name|default('there') }}?"},
            {"type": "text", "html": "<p>We'd love to know what you think. Your review helps other people choose.</p>"},
            {"type": "review_request", "button_text": "Write a review"}
        ]})
    }
}

/// Send the ask for requests made through the API, through the normal send path.
pub async fn send_ask(state: &AppState, account: &Account, person: &Profile, channel: &str, requests: &[Request]) -> AppResult<Outcome> {
    if !may_receive(person, channel) {
        return Ok(Outcome::Skipped("not subscribed"));
    }
    let ids: Vec<Uuid> = requests.iter().map(|r| r.id).collect();
    let event = json!({"_review_requests": ids, "from_browser": false});
    let content = ask_content(channel, requests.first().map(|r| r.name()).unwrap_or("your purchase"));
    let out = sending::send(
        state,
        SendSpec { account, profile: person, channel, content: &content, campaign_id: None, flow_id: None, flow_node_id: None, event: Some(&event), is_test: false, message_id: None, to_override: None },
    )
    .await?;
    if matches!(out, Outcome::Sent(_)) {
        sqlx::query("UPDATE review_requests SET sent_at = COALESCE(sent_at, now()) WHERE id = ANY($1)").bind(&ids).execute(&state.db).await?;
    }
    Ok(out)
}

/// The starter flow: ask after an order (or a delivery, when the account records one), and remind
/// once a week later if they haven't reviewed. Created as a draft.
pub async fn starter_flow(db: &PgPool, account_id: Uuid) -> AppResult<crate::models::FlowRow> {
    let delivered: Option<String> = sqlx::query_scalar("SELECT name FROM metrics WHERE account_id = $1 AND name IN ('Fulfilled Order', 'Delivered', 'Delivered Order') ORDER BY name LIMIT 1")
        .bind(account_id)
        .fetch_optional(db)
        .await?;
    let (metric, days) = match &delivered {
        Some(m) => (m.as_str(), 5.0),
        None => ("Placed Order", 14.0),
    };
    let ask = |id: &str, name: &str, reminder: bool, next: Option<&str>| {
        json!({"id": id, "type": "review_request", "name": name, "channel": "email", "reminder": reminder, "next": next, "content": {
            "subject": if reminder { "A quick reminder: how was it?" } else { "How did we do, {{ first_name|default('friend') }}?" },
            "preheader": "Your review takes a minute",
            "blocks": [
                {"type": "heading", "text": if reminder { "Still happy to hear from you" } else { "How was your order?" }},
                {"type": "text", "html": "<p>Tap the stars to tell us what you think. Your review helps other people choose.</p>"},
                {"type": "review_request", "button_text": "Write a review"}
            ]}})
    };
    let definition = json!({"entry": "v0", "settings": {"reentry": "always"}, "nodes": [
        {"id": "v0", "type": "delay", "amount": days, "unit": "days", "next": "v1"},
        ask("v1", "Ask for a review", false, Some("v2")),
        {"id": "v2", "type": "delay", "amount": 7.0, "unit": "days", "next": "v3"},
        ask("v3", "Reminder", true, None)
    ]});
    let trigger = json!({"type": "metric", "metric": metric});
    crate::flows::parse_trigger(&trigger)?;
    crate::flows::parse_definition(&definition)?;
    let name = if delivered.is_some() { "Ask for a review after delivery" } else { "Ask for a review" };
    Ok(sqlx::query_as("INSERT INTO flows (id, account_id, name, trigger, definition) VALUES ($1, $2, $3, $4, $5) RETURNING *")
        .bind(Uuid::now_v7())
        .bind(account_id)
        .bind(name)
        .bind(&trigger)
        .bind(&definition)
        .fetch_one(db)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_from_orders_and_stays() {
        let order = json!({"properties": {"order_id": 1001, "items": [{"product_id": "A", "name": "Mug"}, {"product_id": "A"}, {"sku": "B", "title": "Cup"}, {"name": "no id"}]}});
        let items = items_from_event(Some(&order));
        assert_eq!(items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(), vec!["A", "B"]);
        assert_eq!(items[0].order_id.as_deref(), Some("1001"));
        let stay = json!({"properties": {"reservation_id": "R9", "listing_id": "L1", "listing_name": "Beach house"}});
        let s = items_from_event(Some(&stay));
        assert_eq!((s[0].key.as_str(), s[0].name.as_deref(), s[0].kind.as_deref()), ("L1", Some("Beach house"), Some("listing")));
        assert!(items_from_event(Some(&json!({"properties": {}}))).is_empty());
    }

    #[test]
    fn tags_are_found_and_replaced() {
        let mut v = json!({"subject": "x", "blocks": [{"type": "text", "html": "<a href=\"{% review_link %}\">a</a> {% review_link 'SKU-1' %}"}]});
        assert!(uses_reviews(&v));
        let mut w = Vec::new();
        tag_products(&v, &mut w);
        assert_eq!(w, vec![None, Some("SKU-1".to_string())]);
        replace_tags(&mut v, &[(None, "https://l/1".into()), (Some("SKU-1".into()), "https://l/2".into())]);
        assert_eq!(v["blocks"][0]["html"], "<a href=\"https://l/1\">a</a> https://l/2");
        assert!(!uses_reviews(&json!({"subject": "s", "blocks": [{"type": "text", "html": "hi"}]})));
    }

    #[test]
    fn step_checks() {
        let ok: Map<String, Value> = serde_json::from_value(json!({"content": {"subject": "s", "blocks": []}})).unwrap();
        assert!(check_step("n", &ok).is_ok());
        let bad: Map<String, Value> = serde_json::from_value(json!({"channel": "sms", "content": {}})).unwrap();
        assert!(check_step("n", &bad).is_err());
        assert!(check_step("n", &Map::new()).is_err());
    }
}
