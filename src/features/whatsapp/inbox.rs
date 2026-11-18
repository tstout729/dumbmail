//! Two-way WhatsApp: inbound messages become conversations in the inbox, keywords work as they do
//! for texts, STOP and START change WhatsApp consent (never SMS), and the team replies: free-form
//! text inside the 24-hour customer service window, approved templates outside it.
//!
//! An inbound message is stored with its consent effect, its conversation and any automatic answer
//! (queued with its job) in one transaction under the number's lock; a retried webhook finds it
//! stored and does nothing twice. Events (flows, segments) follow, each recorded once.

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::content::{self, WaContent};
use super::send::{self, WINDOW_HOURS};
use super::{consent, templates};
use crate::audience::{self, NewEvent, ProfileInput};
use crate::error::{AppError, AppResult};
use crate::features::inbox::keywords::{self, Keyword};
use crate::features::inbox::optout::{self, Class};
use crate::state::AppState;

pub const MAX_BODY_CHARS: usize = 4096;
/// Automatic answers (keyword replies, confirmations) per person per hour.
const AUTO_PER_HOUR: i32 = 5;

/// One inbound message as Meta's webhook described it.
#[derive(Debug, Clone)]
pub struct Inbound {
    pub wamid: String,
    pub from: String,
    pub name: Option<String>,
    pub msg_type: String,
    pub text: String,
    pub context_wamid: Option<String>,
    pub received_at: DateTime<Utc>,
}

impl Inbound {
    /// A `messages[]` item (with the change's `contacts[]` for the name). None when it can't be
    /// stored: no id, or no phone number (someone using a WhatsApp username only).
    pub fn parse(m: &Value, contacts: &Value) -> Option<Self> {
        let wamid = m["id"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.len() <= 200)?.to_string();
        let digits: String = m["from"].as_str()?.chars().filter(char::is_ascii_digit).collect();
        let from = crate::models::normalize_phone(&format!("+{digits}"))?;
        let msg_type = m["type"].as_str().unwrap_or("unknown").chars().take(30).collect::<String>();
        let text = match msg_type.as_str() {
            "text" => m["text"]["body"].as_str().unwrap_or_default().to_string(),
            "button" => m["button"]["text"].as_str().or(m["button"]["payload"].as_str()).unwrap_or_default().to_string(),
            "interactive" => {
                let i = &m["interactive"];
                i["button_reply"]["title"].as_str().or(i["list_reply"]["title"].as_str()).unwrap_or_default().to_string()
            }
            "image" | "video" | "document" | "audio" | "sticker" => m[msg_type.as_str()]["caption"].as_str().unwrap_or_default().to_string(),
            _ => String::new(),
        };
        let name = contacts
            .as_array()
            .and_then(|cs| cs.iter().find(|c| c["wa_id"].as_str() == m["from"].as_str()).or(cs.first()))
            .and_then(|c| c["profile"]["name"].as_str())
            .map(|n| n.chars().take(100).collect());
        let now = Utc::now();
        let received_at = m["timestamp"].as_str().and_then(|t| t.parse::<i64>().ok()).and_then(|t| Utc.timestamp_opt(t, 0).single()).map_or(now, |t| t.min(now));
        Some(Self {
            wamid,
            from,
            name,
            msg_type,
            text: text.chars().take(MAX_BODY_CHARS).collect(),
            context_wamid: m["context"]["id"].as_str().map(|s| s.chars().take(200).collect()),
            received_at,
        })
    }
}

fn shown(rx: &Inbound) -> String {
    if !rx.text.trim().is_empty() {
        return rx.text.clone();
    }
    match rx.msg_type.as_str() {
        "image" => "Sent a photo".into(),
        "video" => "Sent a video".into(),
        "document" => "Sent a document".into(),
        "audio" => "Sent a voice message".into(),
        "sticker" => "Sent a sticker".into(),
        "location" => "Shared a location".into(),
        "contacts" => "Shared a contact".into(),
        "reaction" => "Reacted to a message".into(),
        other => format!("Sent a {other} message"),
    }
}

/// What was stored, for what follows.
struct Stored {
    id: Uuid,
    profile_id: Uuid,
    kind: String,
    keyword: Option<Keyword>,
    subscribed_now: bool,
}

/// Store one inbound message for an account and apply its effects. Idempotent on Meta's id.
pub async fn receive(state: &AppState, account_id: Uuid, rx: &Inbound) -> AppResult<()> {
    let db = &state.db;
    let class = optout::classify(&rx.text, None);
    let word = optout::word(&rx.text);
    let strict = optout::strict_word(&rx.text);
    let keyword = if class == Class::Other && !word.is_empty() { keywords::matching(db, account_id, &word, &strict).await? } else { None };
    let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(&rx.from).fetch_optional(db).await?;
    let profile_id = match existing {
        Some(id) => id,
        // A number we haven't seen: a profile with just the number. Writing in isn't consent.
        None => audience::upsert_profile(db, account_id, &ProfileInput { phone_number: Some(rx.from.clone()), ..Default::default() }).await?.profile.id,
    };
    let account_name: String = sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1").bind(account_id).fetch_one(db).await?;
    let text = shown(rx);

    let mut tx = db.begin().await?;
    consent::lock(&mut tx, account_id, &rx.from).await?;
    consent::ensure_contact(&mut tx, account_id, &rx.from, Some(profile_id)).await?;
    let current: String = sqlx::query_scalar("SELECT consent FROM whatsapp_contacts WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(&rx.from).fetch_one(&mut *tx).await?;
    let kind = match class {
        Class::Stop => "stop",
        _ if optout::START_WORDS.contains(&strict.as_str()) && current == "unsubscribed" => "start",
        _ if keyword.is_some() => "keyword",
        _ => "message",
    };
    let conversation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO whatsapp_conversations (id, account_id, profile_id, phone_number, name) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (account_id, phone_number) DO UPDATE SET profile_id = EXCLUDED.profile_id, name = COALESCE(EXCLUDED.name, whatsapp_conversations.name)
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(profile_id)
    .bind(&rx.from)
    .bind(&rx.name)
    .fetch_one(&mut *tx)
    .await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO whatsapp_inbound (id, account_id, conversation_id, profile_id, wamid, from_number, msg_type, body, kind, keyword, context_wamid, received_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         ON CONFLICT (account_id, wamid) DO NOTHING RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(conversation_id)
    .bind(profile_id)
    .bind(&rx.wamid)
    .bind(&rx.from)
    .bind(&rx.msg_type)
    .bind(&rx.text)
    .bind(kind)
    .bind(keyword.as_ref().map(|k| k.keyword.clone()))
    .bind(&rx.context_wamid)
    .bind(rx.received_at)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        // Meta delivered it before; its effects committed with it. Finish anything after storing.
        tx.rollback().await?;
        return finish_stored(state, account_id, &rx.wamid).await;
    };
    // They wrote, so they're on WhatsApp, and the service window starts again.
    sqlx::query(
        "UPDATE whatsapp_contacts SET last_inbound_at = GREATEST(COALESCE(last_inbound_at, $3), $3), unreachable_at = NULL, profile_id = $4, updated_at = now()
         WHERE account_id = $1 AND phone_number = $2",
    )
    .bind(account_id)
    .bind(&rx.from)
    .bind(rx.received_at)
    .bind(profile_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE whatsapp_conversations SET unread_count = unread_count + 1, last_message_at = now(), last_inbound_at = GREATEST(COALESCE(last_inbound_at, $3), $3),
                last_direction = 'inbound', last_preview = $2, status = 'open', closed_at = NULL
         WHERE id = $1",
    )
    .bind(conversation_id)
    .bind(crate::features::inbox::store::preview(&text, 0))
    .bind(rx.received_at)
    .execute(&mut *tx)
    .await?;
    let detail = json!({"wamid": rx.wamid, "inbound_id": id});
    let mut subscribed_now = false;
    let mut answer: Option<(&str, String)> = None;
    match kind {
        "stop" => {
            consent::set(&mut tx, db, account_id, &rx.from, Some(profile_id), "unsubscribed", "stop", detail).await?;
            answer = Some(("optout_confirmation", format!("You won't get marketing messages from {account_name} on WhatsApp anymore. Reply START to get them again.")));
        }
        "start" => {
            subscribed_now = consent::set(&mut tx, db, account_id, &rx.from, Some(profile_id), "subscribed", "start", detail).await?;
            answer = Some(("confirmation", format!("You're subscribed to WhatsApp messages from {account_name} again. Reply STOP to unsubscribe.")));
        }
        "keyword" => {
            let k = keyword.as_ref().expect("matched");
            if k.subscribe {
                // A sign-up keyword sent on WhatsApp is an explicit WhatsApp opt-in (texts are untouched).
                subscribed_now = consent::set(&mut tx, db, account_id, &rx.from, Some(profile_id), "subscribed", "keyword", json!({"wamid": rx.wamid, "inbound_id": id, "keyword": k.keyword})).await?;
                let lead = if k.reply.trim().is_empty() { format!("Thanks! You're subscribed to WhatsApp messages from {account_name}.") } else { k.reply.clone() };
                answer = Some(("confirmation", format!("{lead} Reply STOP to unsubscribe.")));
            } else if !k.reply.trim().is_empty() {
                answer = Some(("keyword_reply", k.reply.clone()));
            }
        }
        _ => {}
    }
    if let Some((answer_kind, raw)) = answer {
        // A bounded number of automatic answers per person per hour, counted in this transaction.
        let allowed: bool = sqlx::query_scalar(
            "UPDATE whatsapp_conversations SET
                auto_window_count = CASE WHEN auto_window_start IS NULL OR auto_window_start < now() - interval '1 hour' THEN 1 ELSE auto_window_count + 1 END,
                auto_window_start = CASE WHEN auto_window_start IS NULL OR auto_window_start < now() - interval '1 hour' THEN now() ELSE auto_window_start END
             WHERE id = $1 RETURNING auto_window_count <= $2",
        )
        .bind(conversation_id)
        .bind(AUTO_PER_HOUR)
        .fetch_one(&mut *tx)
        .await?;
        if allowed {
            let profile = crate::audience::find_profile(db, account_id, &ProfileInput { phone_number: Some(rx.from.clone()), ..Default::default() }).await?;
            let account = crate::campaigns::load_account(db, account_id).await?;
            let ctx = crate::render::context(&account, profile.as_ref(), None, "");
            let body: String = crate::render::render_str(&raw, &ctx, false).unwrap_or(raw).chars().take(MAX_BODY_CHARS).collect();
            if !body.trim().is_empty() {
                let mid = send::prepare_conversational(&mut tx, account_id, profile_id, &rx.from, answer_kind, None, &body, None).await?;
                crate::jobs::enqueue(&mut *tx, "whatsapp_send", json!({"message_id": mid}), None).await?;
            }
        }
    }
    tx.commit().await?;
    let stored = Stored { id, profile_id, kind: kind.to_string(), keyword, subscribed_now };
    after(state, account_id, rx, &stored).await
}

/// A message stored by an earlier delivery whose follow-up didn't finish: run it again (each step is
/// recorded once).
async fn finish_stored(state: &AppState, account_id: Uuid, wamid: &str) -> AppResult<()> {
    let row: Option<(Uuid, Uuid, String, Option<String>, String, String, Option<String>, DateTime<Utc>, Option<DateTime<Utc>>, String)> = sqlx::query_as(
        "SELECT id, profile_id, kind, keyword, from_number, body, context_wamid, received_at, processed_at, msg_type FROM whatsapp_inbound WHERE account_id = $1 AND wamid = $2",
    )
    .bind(account_id)
    .bind(wamid)
    .fetch_optional(&state.db)
    .await?;
    let Some((id, profile_id, kind, keyword, from, body, context_wamid, received_at, processed, msg_type)) = row else { return Ok(()) };
    if processed.is_some() {
        return Ok(());
    }
    let k = match keyword.as_deref() {
        Some(w) => keywords::by_word(&state.db, account_id, w).await?,
        None => None,
    };
    let subscribed_now = matches!(kind.as_str(), "start") || k.as_ref().is_some_and(|k| k.subscribe);
    let rx = Inbound { wamid: wamid.to_string(), from, name: None, msg_type, text: body, context_wamid, received_at };
    after(state, account_id, &rx, &Stored { id, profile_id, kind, keyword: k, subscribed_now }).await
}

/// Events and list sign-ups after a message is stored.
async fn after(state: &AppState, account_id: Uuid, rx: &Inbound, s: &Stored) -> AppResult<()> {
    let event = |metric: &'static str, unique: String, properties: Value| NewEvent {
        account_id,
        profile_id: s.profile_id,
        metric,
        properties,
        value: None,
        occurred_at: Some(rx.received_at),
        unique_id: Some(unique),
        message_id: None,
    };
    match s.kind.as_str() {
        "stop" => {
            audience::record_event(&state.db, event("Unsubscribed from WhatsApp", format!("wa-out:{}", s.id), json!({"method": "stop", "inbound_message_id": s.id}))).await?;
        }
        _ => {
            if s.subscribed_now {
                let method = if s.kind == "start" { "start" } else { "keyword" };
                audience::record_event(&state.db, event("Subscribed to WhatsApp", format!("wa-sub:{}", s.id), json!({"method": method, "keyword": s.keyword.as_ref().map(|k| &k.keyword), "inbound_message_id": s.id}))).await?;
            }
            if let Some(k) = &s.keyword {
                if let Some(list_id) = k.list_id {
                    audience::add_to_list(&state.db, account_id, list_id, &[s.profile_id]).await?;
                }
            }
            audience::record_event(
                &state.db,
                event(
                    "Received WhatsApp Reply",
                    format!("wa-in:{}", s.id),
                    json!({"text": rx.text, "type": rx.msg_type, "keyword": s.keyword.as_ref().map(|k| k.keyword.to_uppercase()), "inbound_message_id": s.id, "replied_to": rx.context_wamid}),
                ),
            )
            .await?;
        }
    }
    sqlx::query("UPDATE whatsapp_inbound SET processed_at = now() WHERE id = $1").bind(s.id).execute(&state.db).await?;
    // A flow waiting for this person's reply takes the "replied" path now.
    crate::features::inbox::store::wake_waiting_flows(&state.db, s.profile_id, rx.received_at).await?;
    Ok(())
}

// ---------- Conversations ----------

#[derive(Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub before: Option<DateTime<Utc>>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn list(db: &sqlx::PgPool, account_id: Uuid, q: &ListQuery) -> AppResult<Value> {
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let status = q.status.as_deref().filter(|s| matches!(*s, "open" | "closed"));
    let rows: Vec<(Uuid, Uuid, String, Option<String>, String, i32, DateTime<Utc>, Option<DateTime<Utc>>, String, String, Option<String>, Option<String>, String)> = sqlx::query_as(&format!(
        "SELECT c.id, c.profile_id, c.phone_number, c.name, c.status, c.unread_count, c.last_message_at, c.last_inbound_at, c.last_direction, c.last_preview,
                p.first_name, p.last_name, {}
         FROM whatsapp_conversations c JOIN profiles p ON p.id = c.profile_id
         WHERE c.account_id = $1 AND ($2::text IS NULL OR c.status = $2) AND ($3::timestamptz IS NULL OR c.last_message_at < $3)
         ORDER BY c.last_message_at DESC, c.id DESC LIMIT $4",
        consent::STATUS_SQL
    ))
    .bind(account_id)
    .bind(status)
    .bind(q.before)
    .bind(limit)
    .fetch_all(db)
    .await?;
    let now = Utc::now();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(id, pid, phone, name, status, unread, last, inbound, dir, preview, first, last_name, consent)| {
            let person = [first, last_name].into_iter().flatten().collect::<Vec<_>>().join(" ");
            json!({
                "id": id, "channel": "whatsapp", "profile_id": pid, "phone_number": phone, "name": if person.trim().is_empty() { name } else { Some(person) },
                "status": status, "unread_count": unread, "last_message_at": last, "last_direction": dir, "last_preview": preview,
                "window_open": inbound.is_some_and(|t| now - t < chrono::Duration::hours(WINDOW_HOURS)), "whatsapp_consent": consent,
            })
        })
        .collect();
    Ok(json!({"conversations": items}))
}

async fn conversation(db: &sqlx::PgPool, account_id: Uuid, id: Uuid) -> AppResult<(Uuid, String, Option<String>, String)> {
    sqlx::query_as("SELECT profile_id, phone_number, name, status FROM whatsapp_conversations WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::not_found("WhatsApp conversation"))
}

pub async fn get(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let (profile_id, phone, name, status) = conversation(&state.db, account_id, id).await?;
    let inbound: Vec<(Uuid, String, String, String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, msg_type, body, kind, keyword, received_at FROM whatsapp_inbound WHERE conversation_id = $1 AND account_id = $2 ORDER BY received_at DESC LIMIT 200",
    )
    .bind(id)
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let outbound: Vec<(Uuid, String, String, Option<String>, String, Option<String>, Option<String>, DateTime<Utc>, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT m.id, w.kind, w.body, w.template_name, m.status, w.wa_status, m.error, COALESCE(m.sent_at, m.created_at), m.campaign_id, m.flow_id
         FROM messages m JOIN whatsapp_sent w ON w.message_id = m.id
         WHERE m.account_id = $1 AND m.channel = 'whatsapp' AND m.to_address = $2 AND NOT m.is_test AND m.status NOT IN ('queued', 'skipped')
         ORDER BY m.created_at DESC LIMIT 200",
    )
    .bind(account_id)
    .bind(&phone)
    .fetch_all(&state.db)
    .await?;
    let mut items: Vec<(DateTime<Utc>, Value)> = inbound
        .into_iter()
        .map(|(mid, t, body, kind, kw, at)| (at, json!({"id": mid, "direction": "inbound", "type": t, "body": body, "kind": kind, "keyword": kw, "at": at})))
        .chain(outbound.into_iter().map(|(mid, kind, body, tpl, status, wa, err, at, camp, flow)| {
            (at, json!({"id": mid, "direction": "outbound", "kind": kind, "body": body, "template": tpl, "status": wa.unwrap_or(status), "error": err, "at": at, "campaign_id": camp, "flow_id": flow}))
        }))
        .collect();
    items.sort_by_key(|(at, _)| *at);
    let contact = consent::contact_json(&state.db, account_id, &phone).await?;
    let approved: Vec<Value> = templates::list(&state.db, account_id, false).await?.into_iter().filter(|t| t.approved() && t.vars().supported()).map(|t| t.json()).collect();
    let window_open = contact["window_open_until"].is_string();
    Ok(json!({
        "id": id, "channel": "whatsapp", "profile_id": profile_id, "phone_number": phone, "name": name, "status": status,
        "messages": items.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "contact": contact,
        "reply": {
            "free_form": window_open,
            "why": if window_open { Value::Null } else { json!("It's been more than 24 hours since they last wrote, so WhatsApp only allows an approved template.") },
            "templates": approved,
        },
    }))
}

pub async fn mark_read(db: &sqlx::PgPool, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("UPDATE whatsapp_conversations SET unread_count = 0 WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("WhatsApp conversation"));
    }
    Ok(())
}

pub async fn set_status(db: &sqlx::PgPool, account_id: Uuid, id: Uuid, status: &str) -> AppResult<()> {
    if !matches!(status, "open" | "closed") {
        return Err(AppError::bad("status must be open or closed"));
    }
    let n = sqlx::query("UPDATE whatsapp_conversations SET status = $3, closed_at = CASE WHEN $3 = 'closed' THEN now() END WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .bind(status)
        .execute(db)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("WhatsApp conversation"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyBody {
    /// Free-form text (inside the 24-hour window).
    #[serde(default)]
    pub text: Option<String>,
    /// Or an approved template with its variables (see `content`).
    #[serde(default)]
    pub template: Option<Value>,
    #[serde(default)]
    pub confirm: bool,
}

/// Reply in a conversation. Without `confirm`, returns what would go and whether it may.
pub async fn reply(state: &AppState, account_id: Uuid, id: Uuid, b: &ReplyBody, user: Option<Uuid>) -> AppResult<Value> {
    let (profile_id, phone, _, _) = conversation(&state.db, account_id, id).await?;
    if !super::settings::connected(&state.db, account_id).await? {
        return Err(AppError::bad("Connect WhatsApp in Settings → WhatsApp first"));
    }
    let account = crate::campaigns::load_account(&state.db, account_id).await?;
    let profile = crate::audience::find_profile(&state.db, account_id, &ProfileInput { phone_number: Some(phone.clone()), ..Default::default() })
        .await?
        .filter(|p| p.id == profile_id);
    let ctx = crate::render::context(&account, profile.as_ref(), None, "");
    let contact = consent::contact_json(&state.db, account_id, &phone).await?;
    let window_open = contact["window_open_until"].is_string();
    let (text, tpl) = match (&b.text, &b.template) {
        (Some(t), None) => {
            let t = t.trim();
            if t.is_empty() || t.chars().count() > MAX_BODY_CHARS {
                return Err(AppError::bad(format!("Write a reply (up to {MAX_BODY_CHARS} characters)")));
            }
            if !window_open {
                return Err(AppError::bad("It's been more than 24 hours since they last wrote, so WhatsApp only allows an approved template. Pick one instead."));
            }
            (crate::render::render_str(t, &ctx, false)?, None)
        }
        (None, Some(v)) => {
            let c: WaContent = content::parse(v)?;
            let tid = c.template_id.ok_or_else(|| AppError::bad("Pick a template"))?;
            let t = templates::get(&state.db, account_id, tid).await?;
            if let Some(p) = content::problems_with(&c, &t).into_iter().next() {
                return Err(AppError::bad(p));
            }
            if t.is_marketing() && contact["consent"] != "subscribed" {
                return Err(AppError::bad("This is a marketing template, and they haven't opted in to WhatsApp marketing. Use a utility template or a free-form reply."));
            }
            let r = content::render(&t, &c, &ctx, None)?;
            (r.text.clone(), Some((t, c)))
        }
        _ => return Err(AppError::bad("Give either text or template")),
    };
    if contact["consent"] == "unsubscribed" && tpl.is_some() {
        return Err(AppError::bad("They opted out of WhatsApp messages from you; only a free-form reply inside the 24-hour window can go"));
    }
    if !b.confirm {
        return Ok(json!({"dry_run": true, "sent": false, "text": text, "free_form": tpl.is_none(), "note": "Nothing sent. Call again with confirm=true to send."}));
    }
    let mut tx = state.db.begin().await?;
    let mid = match &tpl {
        Some((t, c)) => {
            let message_id = send::prepare_conversational(&mut tx, account_id, profile_id, &phone, "reply", user, &text, None).await?;
            // The template's tracked buttons carry this message's own token.
            let r = content::render(t, c, &ctx, Some((&state.cfg.secret_key, message_id)))?;
            sqlx::query("UPDATE whatsapp_sent SET payload = $2, template_id = $3, template_name = $4, category = $5, body = $6 WHERE message_id = $1")
                .bind(message_id)
                .bind(&r.payload)
                .bind(t.id)
                .bind(&t.name)
                .bind(&t.category)
                .bind(&r.text)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE messages SET subject = $2 WHERE id = $1").bind(message_id).bind(&t.name).execute(&mut *tx).await?;
            message_id
        }
        None => send::prepare_conversational(&mut tx, account_id, profile_id, &phone, "reply", user, &text, None).await?,
    };
    tx.commit().await?;
    let out = send::hand_off(state, mid, false).await?;
    Ok(match out {
        crate::sending::Outcome::Sent(id) => json!({"sent": true, "message_id": id, "text": text}),
        crate::sending::Outcome::Skipped(why) => json!({"sent": false, "message_id": mid, "reason": why}),
        crate::sending::Outcome::Deferred(at) => json!({"sent": false, "message_id": mid, "reason": "Meta asked to wait; it will be tried again", "retry_at": at}),
        crate::sending::Outcome::Failed(why) => json!({"sent": false, "message_id": mid, "reason": why}),
    })
}
