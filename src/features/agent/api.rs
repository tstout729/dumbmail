//! The team's side: recent conversations with hand-offs marked, one conversation's transcript, and
//! the settings page's test chat (a made-up shopper with a made-up order).

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::chat::{self, Orders, Start, Who};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::features::inbox::conversations::{decode_cursor, encode_cursor};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/agent/conversations", get(list_h))
        .route("/api/agent/conversations/{id}", get(get_h))
        .route("/api/agent/test", post(test_h))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    #[serde(default)]
    pub handed_off: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub cursor: Option<String>,
}

type Row = (Uuid, String, String, Option<String>, Option<String>, i32, DateTime<Utc>, DateTime<Utc>, Option<Uuid>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);

const ROW_SQL: &str = "SELECT c.id, c.place, c.status, c.handoff_reason, c.handoff_summary, c.turns, c.created_at, c.last_message_at, c.profile_id,
        p.email, p.phone_number, NULLIF(trim(concat_ws(' ', p.first_name, p.last_name)), ''),
        (SELECT m.body FROM agent_messages m WHERE m.conversation_id = c.id AND m.role <> 'note' ORDER BY m.created_at DESC, m.id DESC LIMIT 1),
        c.sms_conversation_id::text, c.whatsapp_conversation_id::text
     FROM agent_conversations c LEFT JOIN profiles p ON p.id = c.profile_id AND p.account_id = c.account_id";

fn row_json(r: Row) -> Value {
    let (id, place, status, reason, summary, turns, created_at, last_message_at, profile_id, email, phone, name, last, sms, wa) = r;
    json!({
        "id": id, "place": place, "status": status, "handed_off": status == "handed_off", "handoff_reason": reason, "handoff_summary": summary,
        "turns": turns, "created_at": created_at, "last_message_at": last_message_at,
        "person": profile_id.map(|pid| json!({"id": pid, "email": email, "phone_number": phone, "name": name})),
        "last_message": last.map(|l| l.chars().take(160).collect::<String>()),
        "sms_conversation_id": sms,
        "whatsapp_conversation_id": wa,
    })
}

pub async fn list(state: &AppState, account_id: Uuid, q: &ListQuery) -> AppResult<Value> {
    let limit = q.limit.unwrap_or(25).clamp(1, 100);
    let cursor = match q.cursor.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => Some(decode_cursor(c)?),
        None => None,
    };
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "{ROW_SQL} WHERE c.account_id = $1 AND ($2::boolean IS NULL OR (c.status = 'handed_off') = $2)
           AND ($3::timestamptz IS NULL OR (c.last_message_at, c.id) < ($3, $4))
         ORDER BY c.last_message_at DESC, c.id DESC LIMIT $5"
    ))
    .bind(account_id)
    .bind(q.handed_off)
    .bind(cursor.map(|c| c.0))
    .bind(cursor.map(|c| c.1))
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    let more = rows.len() as i64 > limit;
    let rows: Vec<Row> = rows.into_iter().take(limit as usize).collect();
    let next = if more { rows.last().map(|r| encode_cursor(r.7, r.0)) } else { None };
    let handed_off: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_conversations WHERE account_id = $1 AND status = 'handed_off'").bind(account_id).fetch_one(&state.db).await?;
    Ok(json!({"data": rows.into_iter().map(row_json).collect::<Vec<_>>(), "next_cursor": next, "handed_off_count": handed_off}))
}

pub async fn one(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let row: Option<Row> = sqlx::query_as(&format!("{ROW_SQL} WHERE c.account_id = $1 AND c.id = $2")).bind(account_id).bind(id).fetch_optional(&state.db).await?;
    let row = row.ok_or_else(|| AppError::not_found("conversation"))?;
    let msgs: Vec<(String, String, DateTime<Utc>)> =
        sqlx::query_as("SELECT role, body, created_at FROM agent_messages WHERE conversation_id = $1 AND account_id = $2 ORDER BY created_at, id LIMIT 200").bind(id).bind(account_id).fetch_all(&state.db).await?;
    let mut out = row_json(row);
    out["messages"] = json!(msgs.into_iter().map(|(role, body, at)| json!({"role": role, "body": body, "at": at})).collect::<Vec<_>>());
    Ok(out)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestBody {
    pub message: String,
    #[serde(default)]
    pub conversation: Option<String>,
}

/// The settings page's test chat: a made-up shopper with a made-up order, whatever places are on.
pub async fn test(state: &AppState, ctx: &Ctx, b: TestBody) -> AppResult<Value> {
    if b.message.chars().count() > chat::MAX_MESSAGE_CHARS {
        return Err(AppError::bad(format!("A message can be at most {} characters", chat::MAX_MESSAGE_CHARS)));
    }
    if state.cfg.anthropic_api_key.as_deref().is_none_or(str::is_empty) {
        return Err(AppError::bad("The shopping agent isn't available on this server (no Anthropic API key is set)"));
    }
    let account = crate::campaigns::load_account(&state.db, ctx.account_id).await?;
    let st = Start { account: &account, place: "test", who: Who { profile_id: None, orders: Orders::Sample }, token: b.conversation.as_deref(), sms_conversation_id: None, whatsapp_conversation_id: None, ip: None };
    let o = chat::say(state, st, &b.message, |_| {}).await?;
    Ok(json!({"reply": o.reply, "conversation": o.token.or(b.conversation), "conversation_id": o.conversation_id, "handed_off": o.handed_off, "refused": o.refused}))
}

async fn list_h(State(state): State<AppState>, ctx: Ctx, Query(q): Query<ListQuery>) -> AppResult<Json<Value>> {
    Ok(Json(list(&state, ctx.account_id, &q).await?))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx, Path(id): Path<Uuid>) -> AppResult<Json<Value>> {
    Ok(Json(one(&state, ctx.account_id, id).await?))
}

async fn test_h(State(state): State<AppState>, ctx: Ctx, Json(b): Json<TestBody>) -> AppResult<Json<Value>> {
    Ok(Json(test(&state, &ctx, b).await?))
}
