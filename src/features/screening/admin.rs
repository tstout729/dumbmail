//! The operator's side (Bearer ADMIN_TOKEN, like `/admin/unpause`): accounts waiting on a person,
//! with their screenings, and the decisions. Every decision is recorded as a screening with source
//! `operator`.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::standing::{self, New, Standing};
use super::Verdict;
use crate::deliverability::plain;
use crate::error::{AppError, AppResult};
use crate::public::check_admin;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/admin/screening", get(list_h)).route("/admin/screening/decide", post(decide_h)).route("/admin/screening/pass", post(pass_h))
}

async fn list_h(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<Value>> {
    check_admin(&state, &headers)?;
    let accounts: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'account_id', a.id, 'name', a.name, 'website', a.website, 'status', a.sending_review, 'since', a.sending_review_at, 'note', a.sending_review_note,
            'answers', a.sending_review_answers, 'created_at', a.created_at, 'sending_paused_reason', a.sending_paused_reason,
            'owner_email', (SELECT u.email FROM memberships m JOIN users u ON u.id = m.user_id WHERE m.account_id = a.id ORDER BY (m.role = 'owner') DESC, m.created_at LIMIT 1),
            'waiting_for', CASE WHEN a.sending_review <> 'pending' THEN NULL
                WHEN a.sending_review_answers IS NULL THEN 'the customer''s answers (asked on their first send)'
                WHEN NOT EXISTS (SELECT 1 FROM sending_domains d WHERE d.account_id = a.id AND d.status = 'verified') THEN 'a verified sending domain'
                ELSE 'its account check' END,
            'screenings', (SELECT COALESCE(jsonb_agg(jsonb_build_object('id', s.id, 'kind', s.kind, 'subject', s.subject, 'subject_id', s.subject_id, 'subject_name', s.subject_name,
                    'verdict', s.verdict, 'reasons', s.reasons, 'signals', s.signals, 'source', s.source, 'model', s.model, 'input_tokens', s.input_tokens,
                    'output_tokens', s.output_tokens, 'created_at', s.created_at) ORDER BY s.created_at DESC), '[]')
                FROM (SELECT * FROM screenings x WHERE x.account_id = a.id ORDER BY x.created_at DESC LIMIT 20) s))
         FROM accounts a
         WHERE a.closed_at IS NULL AND (a.sending_review IN ('pending', 'review', 'blocked')
            OR EXISTS (SELECT 1 FROM screenings h WHERE h.account_id = a.id AND h.kind = 'content' AND h.verdict = 'review' AND h.created_at > now() - interval '30 days'
                AND NOT EXISTS (SELECT 1 FROM screenings p WHERE p.subject_id = h.subject_id AND p.kind = 'content' AND p.verdict = 'pass' AND p.created_at > h.created_at)))
         ORDER BY COALESCE(a.sending_review_at, a.created_at) DESC LIMIT 200",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "accounts": accounts,
        "notes": ["Opens are always tracked on DumbMail campaign email, so no campaign is left out of the open-rate checks."],
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideBody {
    account_id: Uuid,
    decision: String,
    #[serde(default)]
    note: Option<String>,
}

async fn decide_h(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<DecideBody>) -> AppResult<Json<Value>> {
    check_admin(&state, &headers)?;
    let (to, verdict) = match b.decision.as_str() {
        "approve" => (Standing::Approved, Verdict::Pass),
        "review" => (Standing::Review, Verdict::Review),
        "block" => (Standing::Blocked, Verdict::Block),
        _ => return Err(AppError::bad("decision must be approve, review or block")),
    };
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM accounts WHERE id = $1)").bind(b.account_id).fetch_one(&state.db).await?;
    if !exists {
        return Err(AppError::not_found("account"));
    }
    let note = plain(b.note.as_deref().unwrap_or(""), 600);
    let reasons: Vec<String> = if note.is_empty() { Vec::new() } else { vec![note.clone()] };
    let mut s = New::rules("account", verdict, &reasons, json!({"decision": b.decision}));
    s.source = "operator";
    standing::record(&state.db, b.account_id, s).await?;
    let all = [Standing::Pending, Standing::Approved, Standing::Review, Standing::Blocked];
    let changed = standing::move_to(&state, b.account_id, &all, to, &note, &format!("Operator decision: {}. {note}", b.decision)).await.map_err(AppError::Internal)?;
    let mut was_paused = false;
    if to == Standing::Approved {
        // One call lets the account send again: the email pause goes too (as /admin/unpause).
        was_paused = crate::deliverability::unpause(&state.db, b.account_id).await?;
        sqlx::query("UPDATE accounts SET sending_review_note = NULL WHERE id = $1").bind(b.account_id).execute(&state.db).await?;
    }
    tracing::warn!(account = %b.account_id, decision = %b.decision, "operator decided a sending review");
    Ok(Json(json!({"account_id": b.account_id, "status": to.as_str(), "changed": changed.is_some(), "was_paused": was_paused})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PassBody {
    screening_id: Uuid,
}

/// Let held content go: the customer's next launch (or turning the flow on) of the same content passes.
async fn pass_h(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<PassBody>) -> AppResult<Json<Value>> {
    check_admin(&state, &headers)?;
    let row: Option<(Uuid, String, Option<String>, Option<Uuid>, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT account_id, kind, subject, subject_id, subject_name, content_hash FROM screenings WHERE id = $1").bind(b.screening_id).fetch_optional(&state.db).await?;
    let Some((account_id, kind, subject, subject_id, name, hash)) = row else { return Err(AppError::not_found("screening")) };
    if kind != "content" || hash.is_none() {
        return Err(AppError::bad("only a content screening (a held campaign or flow) can be passed; decide about an account with /admin/screening/decide"));
    }
    let subject: Option<&'static str> = match subject.as_deref() {
        Some("campaign") => Some("campaign"),
        Some("flow") => Some("flow"),
        _ => None,
    };
    let mut s = New::rules("content", Verdict::Pass, &[], json!({"passed": b.screening_id}));
    s.source = "operator";
    s.subject = subject;
    s.subject_id = subject_id;
    s.subject_name = name.clone();
    s.content_hash = hash;
    let id = standing::record(&state.db, account_id, s).await?;
    standing::notify_owner(
        &state,
        account_id,
        "Your email was approved",
        &format!(
            "We checked \"{}\" and it's fine to send. Open it in DumbMail and send or schedule it again (for a flow, turn it on or save it again).\n\nThe DumbMail team",
            plain(name.as_deref().unwrap_or("your message"), 120)
        ),
    )
    .await;
    Ok(Json(json!({"screening_id": id, "passed": b.screening_id})))
}
