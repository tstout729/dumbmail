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
    /// Not used yet (a link is used up by its first review for good, even if that review is
    /// deleted later) and not expired.
    pub fn open(&self) -> bool {
        self.review_id.is_none() && self.submitted_at.is_none() && self.expires_at > Utc::now()
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
    /// What proves the purchase or stay (`event:<id>` or `api:<order ref>`): the request is
    /// verified only with one.
    pub verified_ref: Option<String>,
    /// The item's name, image and address came from a trusted source (the catalog, the API, an
    /// integration or a server-recorded event), so the fields it supplies may be shown to others.
    pub fields_trusted: bool,
    pub batch_id: Uuid,
    pub flow_run_id: Option<Uuid>,
    /// The email or text that carries it, when a block or tag made it: it counts as sent once that
    /// message is.
    pub message_id: Option<Uuid>,
}

pub enum Made {
    New(Request),
    Existing(Request),
    /// Why nobody is asked: `reviewed` (in the last 12 months) or `asked` (this order line was asked before).
    Skipped(&'static str),
}

/// What an order line's request has come to, for asking about the line again.
enum LineState {
    /// A message carried it and didn't go (or never got recorded): it doesn't hold the line.
    NeverReached,
    /// Never sent, never used and not carried by any message: ask with it (one send intent per request).
    Unsent,
    /// Sent, used, or on its way.
    Asked,
}

/// See [`LineState`].
async fn line_state(conn: &mut sqlx::PgConnection, id: Uuid) -> sqlx::Result<LineState> {
    let (reached, unsent): (bool, bool) = sqlx::query_as(
        "SELECT NOT (q.sent_at IS NULL AND q.submitted_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM review_sends rs WHERE rs.status = 'pending' AND q.id = ANY(rs.request_ids))
            AND q.message_id IS NOT NULL
            AND (EXISTS (SELECT 1 FROM messages m WHERE m.id = q.message_id AND m.sent_at IS NULL AND m.status IN ('skipped', 'failed'))
                 OR (NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = q.message_id) AND q.created_at < now() - interval '30 minutes'))),
            q.sent_at IS NULL AND q.message_id IS NULL AND q.submitted_at IS NULL AND q.review_id IS NULL
         FROM review_requests q WHERE q.id = $1",
    )
    .bind(id)
    .fetch_one(conn)
    .await?;
    Ok(if !reached {
        LineState::NeverReached
    } else if unsent {
        LineState::Unsent
    } else {
        LineState::Asked
    })
}

/// A request for one person and one item, or the open one they already have. Checked and written
/// under a lock on the person and the item, so two sends at once can't both ask.
///
/// An order line is asked about once, unless nothing ever reached the person (see
/// [`LineState`]); and a request that isn't verified gives way to a verified one for the same
/// line or item (its link stops working).
pub async fn make(state: &AppState, account: &Account, profile_id: Uuid, item: &Item, m: &Make<'_>) -> AppResult<Made> {
    let db = &state.db;
    let verified = m.verified_ref.is_some();
    let hint = Hint { name: item.name.clone(), image_url: item.image_url.clone(), url: item.url.clone(), kind: item.kind.clone(), trusted: m.fields_trusted };
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
    if let Some(k) = &line_key {
        let q = format!("SELECT {REQUEST_COLS}, q.flow_run_id FROM review_requests q WHERE q.account_id = $1 AND q.line_key = $2");
        let asked: Option<(Request, Option<Uuid>)> = sqlx::query_as::<_, RequestRun>(&q).bind(account.id).bind(k).fetch_optional(&mut *tx).await?.map(|r| (r.request, r.flow_run_id));
        if let Some((r, run)) = asked {
            // The same flow run asking again (a retried step) gets its own request back.
            if m.flow_run_id.is_some() && run == m.flow_run_id && r.open() {
                tx.commit().await?;
                return Ok(Made::Existing(r));
            }
            let state = line_state(&mut tx, r.id).await?;
            let upgrade = verified && !r.verified && r.submitted_at.is_none() && r.review_id.is_none();
            if !upgrade && matches!(state, LineState::Unsent) && r.open() {
                // Made but not sent yet (a link, a skipped ask, or another call's ask in flight): this
                // send uses it, and the send intent's unique key lets only one ask go.
                tx.commit().await?;
                return Ok(Made::Existing(r));
            }
            let lapsed = matches!(state, LineState::Unsent) && !r.open();
            if !upgrade && !lapsed && !matches!(state, LineState::NeverReached) {
                return Ok(Made::Skipped("asked"));
            }
            sqlx::query("DELETE FROM review_requests WHERE id = $1").bind(r.id).execute(&mut *tx).await?;
        }
    }
    let q = format!(
        "SELECT {REQUEST_COLS} FROM review_requests q WHERE q.profile_id = $1 AND q.subject_id = $2 AND q.review_id IS NULL AND q.submitted_at IS NULL AND q.expires_at > now()
         ORDER BY q.created_at DESC LIMIT 1"
    );
    let open: Option<Request> = sqlx::query_as(&q).bind(profile_id).bind(subject.id).fetch_optional(&mut *tx).await?;
    if let Some(r) = open {
        if verified && !r.verified {
            // A verified ask takes the place of an unverified one: its link stops working.
            sqlx::query("DELETE FROM review_requests WHERE id = $1").bind(r.id).execute(&mut *tx).await?;
        } else {
            if let Some(k) = &line_key {
                sqlx::query("UPDATE review_requests SET line_key = $2 WHERE id = $1 AND line_key IS NULL").bind(r.id).bind(k).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            return Ok(Made::Existing(r));
        }
    }
    // What this person sees: the trusted name, or the one their own item gave (text only, and only
    // to them); an image only when a trusted source supplied it.
    let fallback = if subject.kind == "listing" || item.kind.as_deref() == Some("listing") { "your stay" } else { "your purchase" };
    let shown = json!({
        "name": subject.trusted_name().map(String::from).or_else(|| item.name.clone()).unwrap_or_else(|| fallback.to_string()),
        "image_url": subject.trusted_image(),
        "product_id": subject.external_id,
    });
    let q = "INSERT INTO review_requests (id, account_id, profile_id, subject_id, batch_id, line_key, source, verified, verified_ref, item, flow_run_id, message_id, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now() + make_interval(days => $13))
         RETURNING id, account_id, profile_id, subject_id, batch_id, source, verified, item, created_at, expires_at, sent_at, reminded_at, review_id, submitted_at";
    let r: Request = sqlx::query_as(q)
        .bind(Uuid::now_v7())
        .bind(account.id)
        .bind(profile_id)
        .bind(subject.id)
        .bind(m.batch_id)
        .bind(&line_key)
        .bind(m.source)
        .bind(verified)
        .bind(&m.verified_ref)
        .bind(&shown)
        .bind(m.flow_run_id)
        .bind(m.message_id)
        .bind(s.request_days)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Made::New(r))
}

#[derive(sqlx::FromRow)]
struct RequestRun {
    #[sqlx(flatten)]
    request: Request,
    flow_run_id: Option<Uuid>,
}

/// "Ready To Review": the person was asked about an item (once per request).
pub async fn ready_event(state: &AppState, r: &Request, subject: &Subject) -> AppResult<()> {
    record_event(
        &state.db,
        NewEvent {
            account_id: r.account_id,
            profile_id: r.profile_id,
            metric: READY_METRIC,
            properties: json!({"product_id": subject.external_id, "product_name": subject.trusted_name(), "request_id": r.id, "verified": r.verified, "source": r.source}),
            value: None,
            occurred_at: None,
            unique_id: Some(format!("rtr:{}", r.id.simple())),
            message_id: None,
        },
    )
    .await?;
    Ok(())
}

/// Metrics that mean a purchase or stay wherever they're recorded by a server. Integrations'
/// server-only metrics (bookings, orders) and the account's own `purchase_metrics` count too.
pub const PURCHASE_METRICS: [&str; 4] = ["Placed Order", "Fulfilled Order", "Delivered", "Delivered Order"];

/// What the event proves, for a verified review: `event:<id>` when a trusted source (the API, an
/// integration, DumbMail) recorded it for this account and it's one of the account's purchase or
/// stay metrics. Browser events and other metrics prove nothing.
pub async fn proof_of_purchase(db: &PgPool, account_id: Uuid, event: Option<&Value>) -> AppResult<Option<String>> {
    let Some(id) = event.and_then(|e| e.get("id")).and_then(|v| v.as_str()).and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(None) };
    let row: Option<(Option<bool>, String, bool, Vec<String>)> = sqlx::query_as(
        "SELECT e.from_browser, m.name, m.server_only, COALESCE((SELECT purchase_metrics FROM review_settings WHERE account_id = $2), '{}')
         FROM events e JOIN metrics m ON m.id = e.metric_id WHERE e.id = $1 AND e.account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    let Some((from_browser, metric, server_only, extra)) = row else { return Ok(None) };
    let purchase = server_only || PURCHASE_METRICS.iter().any(|m| m.eq_ignore_ascii_case(&metric)) || extra.iter().any(|m| m.eq_ignore_ascii_case(&metric));
    Ok((from_browser == Some(false) && purchase).then(|| format!("event:{}", id.simple())))
}

/// Whether the event that started this came from a trusted source (the API, an integration,
/// DumbMail itself), not a browser, so the item details it carries may be shown to others. Read
/// from the event's row; unknown is untrusted.
async fn event_trusted(db: &PgPool, event: Option<&Value>) -> AppResult<bool> {
    let Some(e) = event else { return Ok(false) };
    let Some(id) = e.get("id").and_then(|v| v.as_str()).and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(false) };
    Ok(sqlx::query_scalar::<_, Option<bool>>("SELECT from_browser FROM events WHERE id = $1").bind(id).fetch_optional(db).await?.flatten() == Some(false))
}

/// The reviews module's own check, against the current row, before any ask goes out: a review
/// ask is marketing, so the person must be subscribed on that channel, have an address, not be
/// suppressed and not be held back from marketing. No exception for service-message flows or
/// alerts. The shared sender's own checks run after this, unchanged.
pub async fn may_ask(db: &PgPool, profile_id: Uuid, channel: &str) -> AppResult<bool> {
    let row: Option<(Option<String>, String, bool, Option<String>, String, bool)> = sqlx::query_as(
        "SELECT email, email_consent, email_suppressed_reason IS NOT NULL, phone_number, sms_consent, marketing_hold FROM profiles WHERE id = $1",
    )
    .bind(profile_id)
    .fetch_optional(db)
    .await?;
    let Some((email, email_consent, suppressed, phone, sms_consent, held)) = row else { return Ok(false) };
    Ok(!held
        && match channel {
            "sms" => phone.is_some() && sms_consent == "subscribed",
            _ => email.is_some() && email_consent == "subscribed" && !suppressed,
        })
}

async fn by_ids(db: &PgPool, account_id: Uuid, profile_id: Uuid, ids: &[Uuid]) -> AppResult<Vec<Request>> {
    let q = format!("SELECT {REQUEST_COLS} FROM review_requests q WHERE q.account_id = $1 AND q.profile_id = $2 AND q.id = ANY($3) ORDER BY q.created_at, q.id");
    Ok(sqlx::query_as(&q).bind(account_id).bind(profile_id).bind(ids).fetch_all(db).await?)
}

fn ids_in(v: Option<&Value>) -> Vec<Uuid> {
    v.and_then(|v| v.as_array()).into_iter().flatten().filter_map(|x| x.as_str()?.parse().ok()).take(MAX_ITEMS * 2).collect()
}

// ---------- Send intents ----------

/// How long a worker holds an intent before another may take it over (longer than any send).
const LEASE_SECS: i64 = 600;

/// One ask, reminder or API ask, recorded before anything is sent. Its id is the message's id.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Intent {
    pub id: Uuid,
    pub account_id: Uuid,
    pub profile_id: Uuid,
    pub kind: String,
    pub channel: String,
    pub request_ids: Vec<Uuid>,
    pub created_ids: Vec<Uuid>,
    pub status: String,
}

const INTENT_COLS: &str = "id, account_id, profile_id, kind, channel, request_ids, created_ids, status";

pub struct NewIntent<'a> {
    pub account: &'a Account,
    pub person: &'a Profile,
    pub kind: &'a str,
    pub channel: &'a str,
    pub request_ids: &'a [Uuid],
    pub created_ids: &'a [Uuid],
    pub flow: Option<(Uuid, Uuid, &'a str)>,
}

/// What recording an intent came to.
pub enum Recorded {
    /// A new intent, holding the requests no other intent of the same stage holds.
    New(Intent),
    /// This flow step's own intent, recorded by an earlier attempt.
    Own(Intent),
    /// Another intent already holds every one of these requests for this stage: nothing to send.
    Taken,
}

/// Record a send intent and its queued message row together (the step's own, for a flow). Each
/// request goes to at most one intent per stage (ask, reminder), by a unique key: requests another
/// intent already holds are left out, and if none is left the holder is returned instead.
pub async fn record_intent(conn: &mut sqlx::PgConnection, n: &NewIntent<'_>) -> AppResult<Recorded> {
    let id = Uuid::now_v7();
    let (flow_id, run_id, node_id) = match n.flow {
        Some((f, r, node)) => (Some(f), Some(r), Some(node)),
        None => (None, None, None),
    };
    if let (Some(run), Some(node)) = (run_id, node_id) {
        let q = format!("SELECT {INTENT_COLS} FROM review_sends WHERE flow_run_id = $1 AND node_id = $2");
        if let Some(i) = sqlx::query_as::<_, Intent>(&q).bind(run).bind(node).fetch_optional(&mut *conn).await? {
            return Ok(Recorded::Own(i));
        }
    }
    let stage = if n.kind == "reminder" { "reminder" } else { "ask" };
    let q = format!(
        "INSERT INTO review_sends (id, account_id, profile_id, flow_run_id, node_id, kind, channel, request_ids, created_ids)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT DO NOTHING RETURNING {INTENT_COLS}"
    );
    let inserted: Option<Intent> = sqlx::query_as(&q)
        .bind(id)
        .bind(n.account.id)
        .bind(n.person.id)
        .bind(run_id)
        .bind(node_id)
        .bind(n.kind)
        .bind(n.channel)
        .bind(n.request_ids)
        .bind(n.created_ids)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(mut intent) = inserted else {
        // The same step, recorded at the same moment by another worker.
        let q = format!("SELECT {INTENT_COLS} FROM review_sends WHERE flow_run_id = $1 AND node_id = $2");
        return Ok(Recorded::Own(sqlx::query_as(&q).bind(run_id).bind(node_id).fetch_one(&mut *conn).await?));
    };
    // The unique key decides: a racing send's insert waits here for the first to commit, then
    // takes nothing it already holds.
    let mine: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO review_send_items (request_id, stage, send_id) SELECT r, $2, $3 FROM unnest($1::uuid[]) r ON CONFLICT DO NOTHING RETURNING request_id",
    )
    .bind(n.request_ids)
    .bind(stage)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    if mine.is_empty() {
        sqlx::query("DELETE FROM review_sends WHERE id = $1").bind(id).execute(&mut *conn).await?;
        return Ok(Recorded::Taken);
    }
    if mine.len() != n.request_ids.len() {
        let keep: Vec<Uuid> = n.request_ids.iter().copied().filter(|r| mine.contains(r)).collect();
        sqlx::query("UPDATE review_sends SET request_ids = $2 WHERE id = $1").bind(id).bind(&keep).execute(&mut *conn).await?;
        intent.request_ids = keep;
    }
    let to = if n.channel == "sms" { n.person.phone_number.clone() } else { n.person.email.clone() };
    sqlx::query("INSERT INTO messages (id, account_id, profile_id, channel, flow_id, flow_node_id, to_address, status) VALUES ($1, $2, $3, $4, $5, $6, $7, 'queued')")
        .bind(intent.id)
        .bind(n.account.id)
        .bind(n.person.id)
        .bind(n.channel)
        .bind(flow_id)
        .bind(node_id)
        .bind(to.unwrap_or_default())
        .execute(&mut *conn)
        .await?;
    Ok(Recorded::New(intent))
}

async fn intent_for_step(db: &PgPool, run_id: Uuid, node_id: &str) -> AppResult<Option<Intent>> {
    let q = format!("SELECT {INTENT_COLS} FROM review_sends WHERE flow_run_id = $1 AND node_id = $2");
    Ok(sqlx::query_as(&q).bind(run_id).bind(node_id).fetch_optional(db).await?)
}

pub async fn intent_by_id(db: &PgPool, id: Uuid) -> AppResult<Option<Intent>> {
    let q = format!("SELECT {INTENT_COLS} FROM review_sends WHERE id = $1");
    Ok(sqlx::query_as(&q).bind(id).fetch_optional(db).await?)
}

/// Settle an intent once. `sent`: its asks count as sent (and "Ready To Review" is recorded).
/// Otherwise nothing reached the person: the requests it made are dropped (the order lines may be
/// asked again), a reminder may go again, and its queued message is marked skipped.
async fn finish(state: &AppState, intent: &Intent, status: &str, why: &str) -> AppResult<()> {
    let db = &state.db;
    let mut tx = db.begin().await?;
    let settled = sqlx::query("UPDATE review_sends SET status = $2, finished_at = now(), claimed_at = NULL WHERE id = $1 AND status = 'pending'")
        .bind(intent.id)
        .bind(status)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if !settled {
        return Ok(());
    }
    let mut ready = Vec::new();
    if status == "sent" {
        if intent.kind != "reminder" {
            ready = sqlx::query_scalar("UPDATE review_requests SET sent_at = now() WHERE id = ANY($1) AND sent_at IS NULL RETURNING id").bind(&intent.request_ids).fetch_all(&mut *tx).await?;
        }
    } else {
        // Nothing reached them: the requests are free to be asked (or reminded) again.
        sqlx::query("DELETE FROM review_send_items WHERE send_id = $1").bind(intent.id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM review_requests WHERE id = ANY($1) AND sent_at IS NULL AND submitted_at IS NULL AND review_id IS NULL").bind(&intent.created_ids).execute(&mut *tx).await?;
        if intent.kind == "reminder" {
            sqlx::query("UPDATE review_requests SET reminded_at = NULL WHERE id = ANY($1)").bind(&intent.request_ids).execute(&mut *tx).await?;
        }
        let unsent = sqlx::query("UPDATE messages SET status = 'skipped', error = $2 WHERE id = $1 AND status = 'queued'").bind(intent.id).bind(why).execute(&mut *tx).await?.rows_affected() == 1;
        if unsent {
            crate::features::messages_unsent_in(&mut tx, db, intent.account_id, &[intent.id]).await;
        }
    }
    tx.commit().await?;
    if !ready.is_empty() {
        for r in by_ids(db, intent.account_id, intent.profile_id, &ready).await? {
            if let Ok(s) = store::subject_by_id(db, r.subject_id).await {
                ready_event(state, &r, &s).await?;
            }
        }
    }
    Ok(())
}

/// Why a review reminder was stopped at the last check: one of its requests was answered.
pub const ANSWERED: &str = "already reviewed";

/// For the sender's last check under the strict marketing rules (`sending::send_strict_marketing`):
/// whether the message (its id at `message_param`) is a review reminder one of whose requests has
/// been answered since. A column of that check's own statement, so it adds no query; false for any
/// other send (`strict_param` false, or no review intent with this id).
pub fn answered_sql(strict_param: &str, message_param: &str) -> String {
    format!(
        "({strict_param} AND EXISTS (SELECT 1 FROM review_sends rs JOIN review_requests q ON q.id = ANY(rs.request_ids)
          WHERE rs.id = {message_param} AND rs.kind = 'reminder' AND (q.review_id IS NOT NULL OR q.submitted_at IS NOT NULL)))"
    )
}

/// What a dispatch came to.
pub enum Dispatch {
    /// Settled: sent, skipped or failed.
    Done(&'static str),
    /// Try again then (quiet hours, warm-up, or another worker holds it).
    Later(DateTime<Utc>),
}

/// Hand one intent's message to the sender, at most once: the intent is claimed (with a lease, so
/// a worker that dies is replaced later), a message an earlier attempt already handed over is
/// recognised by its id and not sent again, the reviews module's own marketing check runs against
/// the current row, and requests answered since (a review arrived) are left out; with none left,
/// nothing is sent at all.
pub async fn dispatch(state: &AppState, account: &Account, intent: &Intent, content: &Value, event: Option<&Value>, flow: Option<(Uuid, &str)>) -> AppResult<Dispatch> {
    let db = &state.db;
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE review_sends SET claimed_at = now() WHERE id = $1 AND status = 'pending' AND (claimed_at IS NULL OR claimed_at < now() - make_interval(secs => $2)) RETURNING id",
    )
    .bind(intent.id)
    .bind(LEASE_SECS as f64)
    .fetch_optional(db)
    .await?;
    if claimed.is_none() {
        let held: Option<(String, Option<DateTime<Utc>>)> = sqlx::query_as("SELECT status, claimed_at FROM review_sends WHERE id = $1").bind(intent.id).fetch_optional(db).await?;
        return Ok(match held {
            Some((s, Some(at))) if s == "pending" => Dispatch::Later(at + chrono::Duration::seconds(LEASE_SECS)),
            _ => Dispatch::Done("settled"),
        });
    }
    let handed: Option<bool> = sqlx::query_scalar("SELECT sent_at IS NOT NULL FROM messages WHERE id = $1").bind(intent.id).fetch_optional(db).await?;
    if handed == Some(true) {
        finish(state, intent, "sent", "").await?;
        return Ok(Dispatch::Done("sent"));
    }
    let q = format!("SELECT {} FROM profiles p WHERE p.id = $1", crate::models::PROFILE_COLS);
    let Some(person) = sqlx::query_as::<_, Profile>(&q).bind(intent.profile_id).fetch_optional(db).await? else {
        finish(state, intent, "skipped", "profile deleted").await?;
        return Ok(Dispatch::Done("skipped"));
    };
    if !may_ask(db, person.id, &intent.channel).await? {
        finish(state, intent, "skipped", "no marketing consent").await?;
        return Ok(Dispatch::Done("skipped"));
    }
    let open: Vec<Uuid> = by_ids(db, account.id, person.id, &intent.request_ids).await?.into_iter().filter(Request::open).map(|r| r.id).collect();
    if open.is_empty() {
        finish(state, intent, "skipped", "already reviewed").await?;
        return Ok(Dispatch::Done("skipped"));
    }
    // Tests hold or stop a worker here, between its checks and the hand-off to the sender.
    super::hooks::point("review_send", account.id).await.map_err(AppError::Internal)?;
    let mut ev = event.cloned().unwrap_or(json!({}));
    ev["_review_requests"] = json!(open);
    // The sender's last check applies the strict marketing rules again right before the hand-off
    // (an opt-out, a hold or, for a reminder, a review landing after the checks above).
    let out = sending::send_strict_marketing(
        state,
        SendSpec {
            account,
            profile: &person,
            channel: &intent.channel,
            content,
            campaign_id: None,
            flow_id: flow.map(|f| f.0),
            flow_node_id: flow.map(|f| f.1),
            event: Some(&ev),
            is_test: false,
            message_id: Some(intent.id),
            to_override: None,
        },
    )
    .await;
    match out {
        Ok(Outcome::Deferred(until)) => {
            sqlx::query("UPDATE review_sends SET claimed_at = NULL WHERE id = $1").bind(intent.id).execute(db).await?;
            Ok(Dispatch::Later(until))
        }
        Ok(Outcome::Sent(_)) => {
            finish(state, intent, "sent", "").await?;
            Ok(Dispatch::Done("sent"))
        }
        Ok(Outcome::Skipped(why)) => {
            finish(state, intent, "skipped", why).await?;
            Ok(Dispatch::Done("skipped"))
        }
        Ok(Outcome::Failed(why)) => {
            tracing::warn!(intent = %intent.id, error = %why, "review ask failed");
            finish(state, intent, "failed", "send failed").await?;
            Ok(Dispatch::Done("failed"))
        }
        Err(e) => {
            let why = if matches!(&e, AppError::Conflict(m) if m == ALL_REVIEWED) { "already reviewed" } else { "send failed" };
            if why == "send failed" {
                tracing::warn!(intent = %intent.id, error = %e.public_message(), "review ask failed");
            }
            let status = if why == "already reviewed" { "skipped" } else { "failed" };
            finish(state, intent, status, why).await?;
            Ok(Dispatch::Done(status))
        }
    }
}

/// The error a render gives when every request it was handed was answered in the meantime: the
/// message must not go at all.
const ALL_REVIEWED: &str = "every item in this ask has been reviewed";

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
/// wait (quiet hours, warm-up, another worker). The step's send intent (one per run and step)
/// makes a retried step find what it already did: the same requests, and the message it already
/// handed over, which is never sent twice.
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
    // A resumed step (its worker stopped after recording, or after sending) reads its requests back
    // from its intent, so the reminder later still knows them.
    let remember = |context: &mut Value, intent: &Intent| {
        if !reminder {
            let mut all = ids_in(context.get("review_requests"));
            for i in &intent.request_ids {
                if !all.contains(i) {
                    all.push(*i);
                }
            }
            context["review_requests"] = json!(all);
        }
    };
    let intent = match intent_for_step(&state.db, run_id, node_id).await.map_err(err)? {
        Some(i) => {
            remember(context, &i);
            if i.status != "pending" {
                return Ok(None);
            }
            i
        }
        None => {
            if !may_ask(&state.db, person.id, channel).await.map_err(err)? {
                return Ok(None);
            }
            if smart_hours > 0 && crate::features::sendtime::recently_messaged(&state.db, person.id, channel, smart_hours).await? {
                return Ok(None);
            }
            let mut tx = state.db.begin().await?;
            let (requests, created): (Vec<Uuid>, Vec<Uuid>) = if reminder {
                let earlier = ids_in(context.get("review_requests"));
                // Claimed with the intent: a reminder goes at most once per request.
                let claimed: Vec<Uuid> = sqlx::query_scalar(
                    "UPDATE review_requests SET reminded_at = now() WHERE account_id = $1 AND profile_id = $2 AND id = ANY($3)
                       AND review_id IS NULL AND submitted_at IS NULL AND expires_at > now() AND reminded_at IS NULL AND sent_at IS NOT NULL RETURNING id",
                )
                .bind(account.id)
                .bind(person.id)
                .bind(&earlier)
                .fetch_all(&mut *tx)
                .await?;
                (claimed, Vec::new())
            } else {
                let event = context.get("event");
                let proof = proof_of_purchase(&state.db, account.id, event).await.map_err(err)?;
                let trusted = event_trusted(&state.db, event).await.map_err(err)?;
                let m = Make { source: "flow", verified_ref: proof, fields_trusted: trusted, batch_id: Uuid::now_v7(), flow_run_id: Some(run_id), message_id: None };
                let (mut all, mut new) = (Vec::new(), Vec::new());
                for item in items_from_event(event) {
                    match make(state, account, person.id, &item, &m).await {
                        Ok(Made::New(r)) => {
                            new.push(r.id);
                            all.push(r.id);
                        }
                        Ok(Made::Existing(r)) => all.push(r.id),
                        Ok(Made::Skipped(_)) => {}
                        Err(e) => tracing::warn!(run = %run_id, node = %node_id, error = %e.public_message(), "review request item skipped"),
                    }
                }
                (all, new)
            };
            if requests.is_empty() {
                return Ok(None);
            }
            let recorded = record_intent(
                &mut tx,
                &NewIntent { account, person, kind: if reminder { "reminder" } else { "ask" }, channel, request_ids: &requests, created_ids: &created, flow: Some((flow.id, run_id, node_id)) },
            )
            .await
            .map_err(err)?;
            tx.commit().await?;
            let intent = match recorded {
                Recorded::New(i) | Recorded::Own(i) => i,
                // Another send is already asking about all of these (the reminder may still follow up).
                Recorded::Taken => {
                    if !reminder {
                        let mut all = ids_in(context.get("review_requests"));
                        all.extend(requests.iter().filter(|r| !all.contains(r)).collect::<Vec<_>>());
                        context["review_requests"] = json!(all);
                    }
                    return Ok(None);
                }
            };
            remember(context, &intent);
            if intent.status != "pending" {
                return Ok(None);
            }
            intent
        }
    };
    let content = data.get("content").cloned().unwrap_or(json!({}));
    match dispatch(state, account, &intent, &content, context.get("event"), Some((flow.id, node_id))).await.map_err(err)? {
        Dispatch::Later(at) => Ok(Some(at)),
        Dispatch::Done(_) => Ok(None),
    }
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

/// Requests for one message, made on demand (or handed over by a send intent).
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
    /// The message being rendered: requests made here count as sent once it is.
    message_id: Option<Uuid>,
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
        let proof = proof_of_purchase(&self.state.db, self.account.id, self.event).await?;
        let trusted = event_trusted(&self.state.db, self.event).await?;
        // A product named by the block or tag itself proves no purchase, and supplies no details.
        let (items, proof, trusted) = match product {
            Some(p) => match event_items.iter().find(|i| i.key == p) {
                Some(i) => (vec![i.clone()], proof, trusted),
                None => (vec![Item { key: p.to_string(), ..Default::default() }], None, false),
            },
            None => (event_items, proof, trusted),
        };
        let mut out = Vec::new();
        for item in items {
            if let Some(r) = self.made.iter().find(|r| r.item["product_id"].as_str() == Some(item.key.as_str())) {
                out.push(r.clone());
                continue;
            }
            let m = Make { source: "email", verified_ref: proof.clone(), fields_trusted: trusted, batch_id: self.batch, flow_run_id: None, message_id: self.message_id };
            match make(self.state, self.account, profile.id, &item, &m).await? {
                // Counted as sent (and "Ready To Review" recorded) once its message is handed over.
                Made::New(r) | Made::Existing(r) => {
                    self.made.push(r.clone());
                    out.push(r);
                }
                Made::Skipped(_) => {}
            }
        }
        Ok(out)
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
        let name = subject.trusted_name().map(|n| format!("{} · ", inert(n))).unwrap_or_default();
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

/// Template text with any `{% review_link %}` left in it removed, so a render that had no person
/// to make a link for still works. Text without the tag is returned as it is.
pub fn strip_tags(source: &str) -> Cow<'_, str> {
    if source.contains("review_link") { TAG.replace_all(source, "") } else { Cow::Borrowed(source) }
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
#[allow(clippy::too_many_arguments)]
pub async fn personalize<'c>(
    state: &AppState,
    account: &Account,
    profile: Option<&Profile>,
    event: Option<&Value>,
    content: &'c Value,
    preview: bool,
    message_id: Option<Uuid>,
) -> AppResult<Cow<'c, Value>> {
    let handed = event.map(|e| ids_in(e.get("_review_requests"))).unwrap_or_default();
    // A review ask's own message always gets its links, even a text without the tag.
    if !uses_reviews(content) && handed.is_empty() {
        return Ok(Cow::Borrowed(content));
    }
    let sms = content.get("body").is_some() && content.get("blocks").is_none() && content.get("html").is_none();
    let allowed = match profile {
        Some(p) if !preview => may_ask(&state.db, p.id, if sms { "sms" } else { "email" }).await?,
        _ => false,
    };
    let given = match profile {
        Some(p) if !handed.is_empty() => {
            // Checked again here, as the message is rendered: requests answered since are left out,
            // and when none is left the message doesn't go at all.
            let open: Vec<Request> = by_ids(&state.db, account.id, p.id, &handed).await?.into_iter().filter(Request::open).collect();
            if open.is_empty() && !preview {
                return Err(AppError::Conflict(ALL_REVIEWED.into()));
            }
            Some(open)
        }
        _ => None,
    };
    let mut asker = Asker { state, account, profile, event, allowed, preview, given, made: Vec::new(), batch: Uuid::now_v7(), message_id };
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

/// Send the ask for requests made through the API: a send intent like a flow's (one message, sent
/// at most once), smart sending over `smart_hours` (0: off), and the sender's quiet hours and
/// warm-up: a message that has to wait goes later from a job.
pub async fn send_ask(state: &AppState, account: &Account, person: &Profile, channel: &str, requests: &[Request], smart_hours: i64) -> AppResult<&'static str> {
    if !may_ask(&state.db, person.id, channel).await? {
        return Ok("not subscribed");
    }
    if smart_hours > 0 && crate::features::sendtime::recently_messaged(&state.db, person.id, channel, smart_hours).await? {
        return Ok("smart sending");
    }
    let ids: Vec<Uuid> = requests.iter().map(|r| r.id).collect();
    let mut tx = state.db.begin().await?;
    let intent = match record_intent(&mut tx, &NewIntent { account, person, kind: "api", channel, request_ids: &ids, created_ids: &[], flow: None }).await? {
        Recorded::New(i) => i,
        Recorded::Own(_) | Recorded::Taken => return Ok("already asked"),
    };
    // Its job is queued with it: if this request stops before dispatching, the job still sends it
    // (once: whoever claims the intent first sends, the other finds it settled or claimed).
    crate::jobs::enqueue(&mut *tx, "reviews_send", json!({"intent_id": intent.id}), Some(chrono::Utc::now() + chrono::Duration::seconds(API_SAFETY_SECS))).await?;
    tx.commit().await?;
    super::hooks::point("api_after_intent", account.id).await.map_err(AppError::Internal)?;
    run_api_intent(state, account, &intent).await
}

/// When the job queued with an API ask runs, in case the request that made it never dispatched it.
const API_SAFETY_SECS: i64 = 120;

/// Dispatch an API ask (now, or from its job when it had to wait).
pub async fn run_api_intent(state: &AppState, account: &Account, intent: &Intent) -> AppResult<&'static str> {
    let first = by_ids(&state.db, account.id, intent.profile_id, &intent.request_ids).await?;
    let content = ask_content(&intent.channel, first.first().map(|r| r.name()).unwrap_or("your purchase"));
    match dispatch(state, account, intent, &content, None, None).await? {
        Dispatch::Done(s) => Ok(s),
        Dispatch::Later(at) => {
            crate::jobs::enqueue(&state.db, "reviews_send", json!({"intent_id": intent.id}), Some(at)).await?;
            Ok("waiting")
        }
    }
}

/// Job: an API ask that had to wait.
pub async fn run_send_job(state: &AppState, payload: &Value) -> anyhow::Result<()> {
    let err = |e: AppError| anyhow::anyhow!(e.public_message());
    let Some(id) = payload["intent_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
    let Some(intent) = intent_by_id(&state.db, id).await.map_err(err)? else { return Ok(()) };
    if intent.status != "pending" {
        return Ok(());
    }
    let account = crate::campaigns::load_account(&state.db, intent.account_id).await.map_err(err)?;
    run_api_intent(state, &account, &intent).await.map_err(err)?;
    Ok(())
}

/// Requests a block or tag made count as sent once the message carrying them was handed over:
/// mark them and record "Ready To Review". Requests whose message didn't go are dropped (their
/// order lines may be asked again). Runs from the scheduler, a batch at a time.
pub async fn settle_carried(state: &AppState) -> anyhow::Result<()> {
    let sent: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "UPDATE review_requests q SET sent_at = m.sent_at FROM messages m
         WHERE q.id IN (SELECT q2.id FROM review_requests q2 JOIN messages m2 ON m2.id = q2.message_id
                        WHERE q2.sent_at IS NULL AND q2.message_id IS NOT NULL AND m2.sent_at IS NOT NULL LIMIT 500)
           AND m.id = q.message_id
         RETURNING q.id, q.account_id, q.profile_id",
    )
    .fetch_all(&state.db)
    .await?;
    for (id, account_id, profile_id) in sent {
        let rs = by_ids(&state.db, account_id, profile_id, &[id]).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        for r in rs {
            if let Ok(s) = store::subject_by_id(&state.db, r.subject_id).await {
                ready_event(state, &r, &s).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
            }
        }
    }
    sqlx::query(
        "DELETE FROM review_requests q WHERE q.id IN (SELECT q2.id FROM review_requests q2 JOIN messages m ON m.id = q2.message_id
            WHERE q2.sent_at IS NULL AND q2.submitted_at IS NULL AND q2.message_id IS NOT NULL AND m.sent_at IS NULL AND m.status IN ('skipped', 'failed') LIMIT 500)",
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The starter flow: ask after an order (or a delivery, when the account records one), and remind
/// once a week later if they haven't reviewed. Created as a draft.
/// `stay`: the day after check-out (at 11am), from the Guesty recipe's "Reservation Confirmed"
/// events; a cancelled booking leaves the flow. `order` (default): after an order or delivery.
pub async fn starter_flow(db: &PgPool, account_id: Uuid, kind: &str) -> AppResult<crate::models::FlowRow> {
    let delivered: Option<String> = sqlx::query_scalar("SELECT name FROM metrics WHERE account_id = $1 AND name IN ('Fulfilled Order', 'Delivered', 'Delivered Order') ORDER BY name LIMIT 1")
        .bind(account_id)
        .fetch_optional(db)
        .await?;
    let stay = kind == "stay";
    let (metric, days) = match &delivered {
        Some(m) => (m.as_str(), 5.0),
        None => ("Placed Order", 14.0),
    };
    let ask = |id: &str, name: &str, reminder: bool, next: Option<&str>| {
        json!({"id": id, "type": "review_request", "name": name, "channel": "email", "reminder": reminder, "next": next, "content": {
            "subject": if reminder { "A quick reminder: how was it?" } else { "How did we do, {{ first_name|default('friend') }}?" },
            "preheader": "Your review takes a minute",
            "blocks": [
                {"type": "heading", "text": if reminder { "Still happy to hear from you" } else if stay { "How was your stay?" } else { "How was your order?" }},
                {"type": "text", "html": "<p>Tap the stars to tell us what you think. Your review helps other people choose.</p>"},
                {"type": "review_request", "button_text": "Write a review"}
            ]}})
    };
    let mut nodes = vec![
        json!({"id": "v0", "type": "delay", "amount": days, "unit": "days", "next": "v1"}),
        ask("v1", "Ask for a review", false, Some("v2")),
        json!({"id": "v2", "type": "delay", "amount": 7.0, "unit": "days", "next": "v3"}),
        ask("v3", "Reminder", true, None),
    ];
    let trigger = if stay {
        // The event-date trigger already waits until the day after check-out.
        nodes.remove(0);
        json!({"type": "event_date", "metric": "Reservation Confirmed", "field": "check_out", "offset_days": 1, "hour": 11, "key": "reservation_id", "cancelled_by": ["Reservation Cancelled"]})
    } else {
        json!({"type": "metric", "metric": metric})
    };
    let definition = json!({"entry": if stay { "v1" } else { "v0" }, "settings": {"reentry": "always"}, "nodes": nodes});
    crate::flows::parse_trigger(&trigger)?;
    crate::flows::parse_definition(&definition)?;
    let name = if stay {
        "Ask for a review after check-out"
    } else if delivered.is_some() {
        "Ask for a review after delivery"
    } else {
        "Ask for a review"
    };
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
        assert_eq!(strip_tags("Review: {% review_link 'A' %}!"), "Review: !");
        assert!(matches!(strip_tags("{{ first_name }}"), Cow::Borrowed(_)));
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
