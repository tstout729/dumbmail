//! The customer's side: the account's sending review (standing, what it's waiting for, the
//! reasons it can see, anything held) and the request for a review with the three answers.
//! The operator's notes and signals never leave through here.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use uuid::Uuid;

use super::account::Answers;
use super::standing::{self, Standing};
use crate::api::ApiJson;
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub fn routes() -> axum::Router<AppState> {
    Router::new().route("/api/screening", get(get_h)).route("/api/screening/request", post(request_h))
}

/// The account's sending review, as the customer sees it.
pub async fn review(state: &AppState, account_id: Uuid) -> AppResult<Value> {
    if !super::on(state) {
        return Ok(json!({"enabled": false, "status": "approved", "can_send": true}));
    }
    type Row = (String, Option<chrono::DateTime<chrono::Utc>>, Option<String>, Option<Value>, bool, bool);
    let (status, since, note, answers, checking, has_domain): Row = sqlx::query_as(
        "SELECT sending_review, sending_review_at, sending_review_note, sending_review_answers,
                sending_review_checking_at IS NOT NULL AND sending_review_checking_at > now() - interval '5 minutes',
                EXISTS (SELECT 1 FROM sending_domains d WHERE d.account_id = a.id AND d.status = 'verified')
         FROM accounts a WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let st = Standing::parse(Some(&status));
    let waiting_for = match st {
        Standing::Pending if answers.is_none() => Some("answers"),
        Standing::Pending if !has_domain => Some("sending_domain"),
        Standing::Pending => Some("check"),
        Standing::Review => Some("person"),
        _ => None,
    };
    // Content held for a person (and not let go since).
    let held: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('screening_id', s.id, 'subject', s.subject, 'subject_id', s.subject_id, 'name', s.subject_name, 'verdict', s.verdict, 'reasons', s.reasons, 'created_at', s.created_at)
         FROM screenings s WHERE s.account_id = $1 AND s.kind = 'content' AND s.verdict IN ('review', 'block') AND s.created_at > now() - interval '30 days'
           AND NOT EXISTS (SELECT 1 FROM screenings p WHERE p.account_id = s.account_id AND p.subject_id = s.subject_id AND p.kind = 'content' AND p.created_at > s.created_at AND p.verdict = 'pass')
         ORDER BY s.created_at DESC LIMIT 20",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let recent: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('kind', kind, 'subject', subject, 'subject_id', subject_id, 'name', subject_name, 'verdict', verdict, 'reasons', reasons, 'by', CASE source WHEN 'operator' THEN 'person' ELSE 'automatic' END, 'created_at', created_at)
         FROM screenings WHERE account_id = $1 ORDER BY created_at DESC LIMIT 10",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    Ok(json!({
        "enabled": true,
        "status": st.as_str(),
        "can_send": st == Standing::Approved,
        "since": since,
        "note": note,
        "message": standing::hold_message(st),
        "waiting_for": waiting_for,
        "checking": checking,
        "answers": answers,
        "held": held,
        "recent": recent,
    }))
}

async fn get_h(State(state): State<AppState>, ctx: Ctx) -> AppResult<Json<Value>> {
    Ok(Json(review(&state, ctx.account_id).await?))
}

/// Save the three answers and, for a pending account, run its check now (within the launch deadline).
pub async fn request(state: &AppState, ctx: &Ctx, a: Answers) -> AppResult<Value> {
    if !super::on(state) {
        return review(state, ctx.account_id).await;
    }
    let a = a.checked().map_err(AppError::bad)?;
    let before = standing::read(state, ctx.account_id).await?;
    sqlx::query("UPDATE accounts SET sending_review_answers = $2 WHERE id = $1").bind(ctx.account_id).bind(json!(a)).execute(&state.db).await?;
    crate::features::team::audit(state, ctx, "sending_review.request", None, json!({"status": before.as_str()})).await;
    if before == Standing::Pending {
        super::account::run(state, ctx.account_id, None, super::content::deadline(ctx.account_id)).await.map_err(AppError::Internal)?;
    }
    review(state, ctx.account_id).await
}

async fn request_h(State(state): State<AppState>, ctx: Ctx, ApiJson(a): ApiJson<Answers>) -> AppResult<Json<Value>> {
    Ok(Json(request(&state, &ctx, a).await?))
}

pub fn mcp_tools() -> Vec<Value> {
    vec![
        tool(
            "get_sending_review",
            "Get the account's sending review",
            "Whether this account may send yet: status pending (new, checked before its first send), approved, review (a person is checking it) or blocked; what it's waiting for (answers, sending_domain, check, person); the reasons it can see; and campaigns or flows held for a person's check. See dumbmail://docs/screening.",
            obj(json!({}), &[]),
            true,
            false,
        ),
        tool(
            "request_sending_review",
            "Request a sending review",
            "Answer the three questions DumbMail asks before an account's first send, and for a new account run its check now (takes up to a minute). Ask the user for each answer; don't make them up.",
            obj(
                json!({
                    "business": {"type": "string", "maxLength": 1000, "description": "What the business does."},
                    "list_source": {"type": "string", "maxLength": 1000, "description": "How the people on the list signed up (a sign-up form on the website, at checkout, in the shop...)."},
                    "monthly_volume": {"type": "integer", "minimum": 0, "maximum": 100000000, "description": "About how many emails a month the account will send."}
                }),
                &["business", "list_source", "monthly_volume"],
            ),
            false,
            false,
        ),
    ]
}

pub async fn mcp_call(state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> Option<AppResult<Value>> {
    match name {
        "get_sending_review" => Some(review(state, ctx.account_id).await),
        "request_sending_review" => Some(async { request(state, ctx, from(args.clone())?).await }.await),
        _ => None,
    }
}
