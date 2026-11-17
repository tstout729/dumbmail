//! Conversations: the inbox list, a thread's full two-way history, read state and open/closed.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use super::replies;
use crate::error::{AppError, AppResult};

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    /// open (default) | closed | all
    pub status: Option<String>,
    /// Only conversations with unread texts.
    pub unread: Option<bool>,
    /// Name, phone number, email or words in their texts.
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    profile_id: Uuid,
    status: String,
    unread_count: i32,
    last_message_at: DateTime<Utc>,
    last_inbound_at: Option<DateTime<Utc>>,
    last_direction: String,
    last_preview: String,
    first_name: Option<String>,
    last_name: Option<String>,
    phone_number: Option<String>,
    email: Option<String>,
    sms_consent: String,
}

fn like(q: &str) -> String {
    format!("%{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
}

fn can_reply(r: &Row, now: DateTime<Utc>) -> bool {
    r.phone_number.is_some()
        && match r.sms_consent.as_str() {
            "unsubscribed" => false,
            "subscribed" => true,
            _ => r.last_inbound_at.is_some_and(|at| now - at < chrono::Duration::hours(replies::REPLY_WINDOW_HOURS)),
        }
}

fn row_json(r: &Row, now: DateTime<Utc>) -> Value {
    json!({
        "id": r.id, "status": r.status, "unread_count": r.unread_count, "last_message_at": r.last_message_at,
        "last_inbound_at": r.last_inbound_at, "last_direction": r.last_direction, "last_preview": r.last_preview,
        "can_reply": can_reply(r, now),
        "profile": {"id": r.profile_id, "first_name": r.first_name, "last_name": r.last_name, "phone_number": r.phone_number, "email": r.email, "sms_consent": r.sms_consent},
    })
}

const SELECT: &str = "SELECT c.id, c.profile_id, c.status, c.unread_count, c.last_message_at, c.last_inbound_at, c.last_direction, c.last_preview,
        p.first_name, p.last_name, p.phone_number, p.email, p.sms_consent
    FROM sms_conversations c JOIN profiles p ON p.id = c.profile_id";

pub async fn list(db: &PgPool, account_id: Uuid, q: &ListQuery) -> AppResult<(Vec<Value>, i64)> {
    let status = match q.status.as_deref().unwrap_or("open") {
        "all" => None,
        s @ ("open" | "closed") => Some(s.to_string()),
        _ => return Err(AppError::bad("status must be open, closed or all")),
    };
    let search = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(100).collect::<String>());
    let digits = search.as_deref().map(|s| s.chars().filter(char::is_ascii_digit).collect::<String>()).filter(|d| d.len() >= 3);
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let offset = q.offset.unwrap_or(0).clamp(0, 100_000);
    let filter = "WHERE c.account_id = $1 AND ($2::text IS NULL OR c.status = $2) AND (NOT $3 OR c.unread_count > 0)
        AND ($4::text IS NULL OR p.first_name ILIKE $4 OR p.last_name ILIKE $4 OR (p.first_name || ' ' || p.last_name) ILIKE $4 OR p.email ILIKE $4
             OR ($5::text IS NOT NULL AND p.phone_number LIKE $5)
             OR EXISTS (SELECT 1 FROM sms_inbound i WHERE i.conversation_id = c.id AND i.body ILIKE $4))";
    let pattern = search.as_deref().map(like);
    let digits = digits.map(|d| like(&d));
    let unread = q.unread.unwrap_or(false);
    let sql = format!("{SELECT} {filter} ORDER BY c.last_message_at DESC, c.id DESC LIMIT $6 OFFSET $7");
    let rows: Vec<Row> = sqlx::query_as(&sql)
        .bind(account_id)
        .bind(&status)
        .bind(unread)
        .bind(&pattern)
        .bind(&digits)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
    let count_sql = format!("SELECT count(*) FROM sms_conversations c JOIN profiles p ON p.id = c.profile_id {filter}");
    let total: i64 = sqlx::query_scalar(&count_sql).bind(account_id).bind(&status).bind(unread).bind(&pattern).bind(&digits).fetch_one(db).await?;
    let now = Utc::now();
    Ok((rows.iter().map(|r| row_json(r, now)).collect(), total))
}

pub async fn unread(db: &PgPool, account_id: Uuid) -> AppResult<Value> {
    let (conversations, texts): (i64, i64) =
        sqlx::query_as("SELECT count(*), COALESCE(sum(unread_count), 0)::bigint FROM sms_conversations WHERE account_id = $1 AND unread_count > 0")
            .bind(account_id)
            .fetch_one(db)
            .await?;
    Ok(json!({"conversations": conversations, "texts": texts}))
}

#[derive(sqlx::FromRow)]
struct Item {
    id: Uuid,
    direction: String,
    kind: String,
    keyword: Option<String>,
    body: Option<String>,
    media: Value,
    at: DateTime<Utc>,
    status: Option<String>,
    error: Option<String>,
    campaign_id: Option<Uuid>,
    campaign_name: Option<String>,
    flow_id: Option<Uuid>,
    flow_name: Option<String>,
    sent_by_name: Option<String>,
}

fn media_json(media: &Value) -> Value {
    let items = media.as_array().cloned().unwrap_or_default();
    Value::Array(
        items
            .iter()
            .map(|m| {
                let stored = m["media_id"].as_str().map(|id| format!("/api/inbox/media/{id}"));
                json!({"content_type": m["content_type"], "url": stored.clone().unwrap_or_else(|| m["url"].as_str().unwrap_or_default().to_string()), "stored": stored.is_some()})
            })
            .collect(),
    )
}

/// A conversation with its newest texts (both directions, campaign and flow texts included),
/// newest first. Page back with `before` (the `at` of the oldest text you have).
pub async fn get(db: &PgPool, account_id: Uuid, id: Uuid, before: Option<DateTime<Utc>>, limit: Option<i64>) -> AppResult<Value> {
    let rows: Vec<Row> = sqlx::query_as(&format!("{SELECT} WHERE c.id = $1 AND c.account_id = $2")).bind(id).bind(account_id).fetch_all(db).await?;
    let conv = rows.first().ok_or_else(|| AppError::not_found("conversation"))?;
    let limit = limit.unwrap_or(50).clamp(1, 200);
    let items: Vec<Item> = sqlx::query_as(
        "SELECT * FROM (
            SELECT i.id, 'inbound' AS direction, i.kind, i.keyword, i.body, i.media, i.received_at AS at, NULL::text AS status, NULL::text AS error,
                   NULL::uuid AS campaign_id, NULL::text AS campaign_name, NULL::uuid AS flow_id, NULL::text AS flow_name, NULL::text AS sent_by_name
            FROM sms_inbound i WHERE i.account_id = $1 AND i.profile_id = $2
            UNION ALL
            SELECT m.id, 'outbound', COALESCE(t.kind, 'marketing'), NULL, t.body,
                   CASE WHEN t.media_url IS NULL THEN '[]'::jsonb ELSE jsonb_build_array(jsonb_build_object('url', t.media_url)) END,
                   COALESCE(m.sent_at, m.created_at), m.status, m.error, m.campaign_id, c.name, m.flow_id, f.name, COALESCE(u.name, u.email)
            FROM messages m
            LEFT JOIN sms_sent_texts t ON t.message_id = m.id
            LEFT JOIN campaigns c ON c.id = m.campaign_id
            LEFT JOIN flows f ON f.id = m.flow_id
            LEFT JOIN users u ON u.id = t.sent_by
            WHERE m.account_id = $1 AND m.profile_id = $2 AND m.channel = 'sms' AND NOT m.is_test AND m.status NOT IN ('queued', 'skipped')
         ) x WHERE ($3::timestamptz IS NULL OR x.at < $3) ORDER BY x.at DESC, x.id DESC LIMIT $4",
    )
    .bind(account_id)
    .bind(conv.profile_id)
    .bind(before)
    .bind(limit + 1)
    .fetch_all(db)
    .await?;
    let has_more = items.len() as i64 > limit;
    let messages: Vec<Value> = items
        .iter()
        .take(limit as usize)
        .map(|m| {
            json!({
                "id": m.id, "direction": m.direction, "kind": m.kind, "keyword": m.keyword.as_ref().map(|k| k.to_uppercase()),
                "body": m.body, "media": media_json(&m.media), "at": m.at, "status": m.status, "error": m.error,
                "campaign": m.campaign_id.map(|id| json!({"id": id, "name": m.campaign_name})),
                "flow": m.flow_id.map(|id| json!({"id": id, "name": m.flow_name})),
                "sent_by": m.sent_by_name,
            })
        })
        .collect();
    let profile = replies::load_profile(db, account_id, conv.profile_id).await?;
    let now = Utc::now();
    Ok(json!({
        "conversation": row_json(conv, now),
        "messages": messages,
        "has_more": has_more,
        "reply": {
            "allowed": replies::reply_block(&profile, conv.last_inbound_at, now).is_none(),
            "reason": replies::reply_block(&profile, conv.last_inbound_at, now),
            "window_ends_at": replies::window_ends(&profile, conv.last_inbound_at),
        },
    }))
}

pub async fn mark_read(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<()> {
    let n = sqlx::query("UPDATE sms_conversations SET unread_count = 0 WHERE id = $1 AND account_id = $2").bind(id).bind(account_id).execute(db).await?.rows_affected();
    if n == 0 {
        return Err(AppError::not_found("conversation"));
    }
    Ok(())
}

/// Close (done for now) or reopen. A new text from the person reopens it on its own.
pub async fn set_status(db: &PgPool, account_id: Uuid, id: Uuid, status: &str) -> AppResult<()> {
    if !matches!(status, "open" | "closed") {
        return Err(AppError::bad("status must be open or closed"));
    }
    let n = sqlx::query(
        "UPDATE sms_conversations SET status = $3, closed_at = CASE WHEN $3 = 'closed' THEN now() ELSE NULL END,
                unread_count = CASE WHEN $3 = 'closed' THEN 0 ELSE unread_count END
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .bind(status)
    .execute(db)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::not_found("conversation"));
    }
    Ok(())
}

/// The conversation for a profile, if they've ever texted in.
pub async fn for_profile(db: &PgPool, account_id: Uuid, profile_id: Uuid) -> AppResult<Option<Uuid>> {
    Ok(sqlx::query_scalar("SELECT id FROM sms_conversations WHERE account_id = $1 AND profile_id = $2").bind(account_id).bind(profile_id).fetch_optional(db).await?)
}
