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
    hosted_only(&state)?;
    check_admin(&state, &headers)?;
    let accounts: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'account_id', a.id, 'name', a.name, 'website', a.website, 'status', a.sending_review, 'since', a.sending_review_at, 'note', a.sending_review_note,
            'answers', a.sending_review_answers, 'created_at', a.created_at, 'sending_paused_at', a.sending_paused_at, 'sending_paused_reason', a.sending_paused_reason,
            'affiliated_brands', a.sending_review_brands, 'checked_at', now(),
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
    let mut notes = vec!["Opens are always tracked on DumbMail campaign email, so no campaign is left out of the open-rate checks."];
    if state.cfg.anthropic_api_key.as_deref().is_none_or(|k| k.trim().is_empty()) {
        notes.push("No ANTHROPIC_API_KEY is set: approved accounts' content is judged by the rules alone (a rules review or block still holds it), and every new account's first check comes here for a person.");
    }
    Ok(Json(json!({"accounts": accounts, "notes": notes})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideBody {
    account_id: Uuid,
    decision: String,
    #[serde(default)]
    note: Option<String>,
    /// When the operator looked (the listing's `checked_at`): refused if the account's standing or
    /// pause changed after that, so a decision never erases something newer.
    #[serde(default)]
    seen_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Brands this account really belongs to (exempt from the lookalike rule for them).
    #[serde(default)]
    affiliated_brands: Option<Vec<String>>,
}

async fn decide_h(State(state): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> AppResult<Json<Value>> {
    hosted_only(&state)?;
    check_admin(&state, &headers)?;
    let b: DecideBody = body_json(&body)?;
    let (to, verdict) = match b.decision.as_str() {
        "approve" => (Standing::Approved, Verdict::Pass),
        "review" => (Standing::Review, Verdict::Review),
        "block" => (Standing::Blocked, Verdict::Block),
        _ => return Err(AppError::bad("decision must be approve, review or block")),
    };
    let brands: Option<Vec<String>> = b.affiliated_brands.map(|v| v.iter().map(|x| plain(x, 40).to_ascii_lowercase()).filter(|x| !x.is_empty()).take(20).collect());
    type Read = (String, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, String);
    let read: Option<Read> = sqlx::query_as("SELECT sending_review, sending_review_at, sending_paused_at, name FROM accounts WHERE id = $1").bind(b.account_id).fetch_optional(&state.db).await?;
    let Some((old, old_at, paused_at, name)) = read else { return Err(AppError::not_found("account")) };
    let changed = || AppError::Conflict("This account changed after you looked (a new check, a pause or another decision). Look again, then decide.".into());
    if let Some(seen) = b.seen_at {
        if old_at.is_some_and(|t| t > seen) || paused_at.is_some_and(|t| t > seen) {
            return Err(changed());
        }
    }
    let note = plain(b.note.as_deref().unwrap_or(""), 600);
    let approve = to == Standing::Approved;
    // One statement: the standing, the note and (approving) the email pause, only if nothing changed
    // since the read above.
    let n = sqlx::query(
        "UPDATE accounts SET sending_review = $2, sending_review_at = now(), sending_review_note = NULLIF($3, ''),
            sending_review_brands = COALESCE($4, sending_review_brands),
            sending_paused_at = CASE WHEN $5 THEN NULL ELSE sending_paused_at END,
            sending_paused_reason = CASE WHEN $5 THEN NULL ELSE sending_paused_reason END,
            sending_window_start = CASE WHEN $5 AND sending_paused_at IS NOT NULL THEN now() ELSE sending_window_start END
         WHERE id = $1 AND sending_review = $6 AND sending_review_at IS NOT DISTINCT FROM $7 AND sending_paused_at IS NOT DISTINCT FROM $8",
    )
    .bind(b.account_id)
    .bind(to.as_str())
    .bind(&note)
    .bind(&brands)
    .bind(approve)
    .bind(&old)
    .bind(old_at)
    .bind(paused_at)
    .execute(&state.db)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(changed());
    }
    let was_paused = approve && paused_at.is_some();
    if was_paused {
        crate::deliverability::forget_daily_limit(&state, b.account_id);
    }
    let reasons: Vec<String> = if note.is_empty() { Vec::new() } else { vec![note.clone()] };
    let mut s = New::rules("account", verdict, &reasons, json!({"decision": b.decision, "affiliated_brands": brands}));
    s.source = "operator";
    standing::record(&state.db, b.account_id, s).await?;
    let old = Standing::parse(Some(&old));
    if old != to {
        standing::after_move(&state, b.account_id, &name, old, to, &note, &format!("Operator decision: {}. {note}", b.decision)).await.map_err(AppError::Internal)?;
    }
    tracing::warn!(account = %b.account_id, decision = %b.decision, "operator decided a sending review");
    Ok(Json(json!({"account_id": b.account_id, "status": to.as_str(), "changed": old != to, "was_paused": was_paused})))
}

fn body_json<T: serde::de::DeserializeOwned>(b: &[u8]) -> AppResult<T> {
    serde_json::from_slice(b).map_err(|e| AppError::bad(format!("invalid body: {e}")))
}

/// The operator routes exist only on the hosted service.
fn hosted_only(state: &AppState) -> AppResult<()> {
    if super::on(state) { Ok(()) } else { Err(AppError::NotFound("not found".into())) }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PassBody {
    screening_id: Uuid,
}

/// Let held content go: the customer's next launch (or turning the flow on) of the same content passes.
async fn pass_h(State(state): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> AppResult<Json<Value>> {
    hosted_only(&state)?;
    check_admin(&state, &headers)?;
    let b: PassBody = body_json(&body)?;
    type Row = (Uuid, String, Option<String>, Option<Uuid>, Option<String>, Option<String>, Vec<String>);
    let row: Option<Row> =
        sqlx::query_as("SELECT account_id, kind, subject, subject_id, subject_name, content_hash, part_hashes FROM screenings WHERE id = $1").bind(b.screening_id).fetch_optional(&state.db).await?;
    let Some((account_id, kind, subject, subject_id, name, hash, parts)) = row else { return Err(AppError::not_found("screening")) };
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
    s.part_hashes = parts;
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
