//! Who may get scheduled report emails. A member gets them only after confirming their address:
//! adding them to a schedule emails a one-time link (good for 7 days); opening it shows a button,
//! and only pressing it (a POST) confirms, so link scanners can't. Members who signed in with Google
//! have shown they own their address and count as confirmed. This keeps anyone from using
//! DumbMail's own sender to mail report content to an address they made an account with but don't own.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppResult;
use crate::providers::email::OutgoingEmail;
use crate::state::AppState;

/// How long a confirmation link works.
pub const LINK_DAYS: i32 = crate::optin::LINK_DAYS;
/// A member waiting to confirm isn't emailed another link for this long.
const RESEND_AFTER_HOURS: i32 = 24;

/// Each member's state: `confirmed`, `waiting` (a link was sent) or `not_asked`. Only members of the
/// account are returned.
pub async fn statuses(state: &AppState, account_id: Uuid, user_ids: &[Uuid]) -> AppResult<HashMap<Uuid, (String, Option<String>, &'static str)>> {
    let rows: Vec<(Uuid, String, Option<String>, String)> = sqlx::query_as(
        "SELECT u.id, u.email, u.name,
                CASE WHEN u.google_sub IS NOT NULL OR (r.confirmed_at IS NOT NULL AND r.email = u.email) THEN 'confirmed'
                     WHEN r.email = u.email THEN 'waiting' ELSE 'not_asked' END
         FROM memberships m JOIN users u ON u.id = m.user_id
         LEFT JOIN report_recipients r ON r.account_id = m.account_id AND r.user_id = u.id
         WHERE m.account_id = $1 AND u.id = ANY($2)",
    )
    .bind(account_id)
    .bind(user_ids)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, email, name, st)| {
            let st = match st.as_str() {
                "confirmed" => "confirmed",
                "waiting" => "waiting",
                _ => "not_asked",
            };
            (id, (email, name, st))
        })
        .collect())
}

/// The members among `user_ids` who may get report emails now, with their address.
pub async fn confirmed(state: &AppState, account_id: Uuid, user_ids: &[Uuid]) -> AppResult<Vec<(Uuid, String)>> {
    let mut out: Vec<(Uuid, String)> = statuses(state, account_id, user_ids).await?.into_iter().filter(|(_, v)| v.2 == "confirmed").map(|(id, v)| (id, v.0)).collect();
    out.sort();
    Ok(out)
}

fn token(state: &AppState, id: Uuid) -> String {
    crate::auth::sign(&state.cfg.secret_key, &format!("rr{}", id.simple()))
}

fn id_from_token(state: &AppState, token: &str) -> Option<Uuid> {
    crate::auth::verify(&state.cfg.secret_key, token)?.strip_prefix("rr")?.parse().ok()
}

/// Email a confirmation link to each of `user_ids` who hasn't confirmed, at most once a day each.
/// A failed email is logged; the member stays "waiting".
pub async fn ask(state: &AppState, account_id: Uuid, user_ids: &[Uuid]) -> AppResult<()> {
    let st = statuses(state, account_id, user_ids).await?;
    let Some(from) = state.cfg.support_email.clone().filter(|s| s.contains('@')) else {
        tracing::warn!("no SUPPORT_EMAIL set; report email confirmations not sent");
        return Ok(());
    };
    for (user_id, (email, _, status)) in st {
        if status == "confirmed" {
            continue;
        }
        // A new address gets a new link; the same address keeps its link, re-sent after a day.
        let id: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO report_recipients (account_id, user_id, id, email, asked_at) VALUES ($1, $2, $3, $4, now())
             ON CONFLICT (account_id, user_id) DO UPDATE
                 SET id = CASE WHEN report_recipients.email = EXCLUDED.email THEN report_recipients.id ELSE EXCLUDED.id END,
                     confirmed_at = CASE WHEN report_recipients.email = EXCLUDED.email THEN report_recipients.confirmed_at END,
                     email = EXCLUDED.email, asked_at = now()
                 WHERE report_recipients.email <> EXCLUDED.email OR report_recipients.asked_at < now() - make_interval(hours => $5)
             RETURNING id",
        )
        .bind(account_id)
        .bind(user_id)
        .bind(Uuid::now_v7())
        .bind(&email)
        .bind(RESEND_AFTER_HOURS)
        .fetch_optional(&state.db)
        .await?;
        let Some(id) = id else { continue };
        let url = format!("{}/report-emails/{}", state.cfg.public_url(), token(state, id));
        let text = format!(
            "Someone added this address to a scheduled report in DumbMail.\n\nTo get report emails here, confirm within {LINK_DAYS} days:\n{url}\n\nIf you don't use DumbMail, ignore this email and you won't get any.\n"
        );
        let html = format!(
            "<div style=\"font-family:system-ui,sans-serif;font-size:14px;line-height:1.5;color:#18181b\"><p>Someone added this address to a scheduled report in DumbMail.</p><p><a href=\"{u}\">Confirm to get report emails here</a> (the link works for {LINK_DAYS} days).</p><p style=\"color:#71717a\">If you don't use DumbMail, ignore this email and you won't get any.</p></div>",
            u = url.replace('&', "&amp;").replace('"', "&quot;")
        );
        let msg = OutgoingEmail {
            message_id: Uuid::now_v7(),
            to: email.clone(),
            from_name: "DumbMail reports".into(),
            from_email: from.clone(),
            reply_to: None,
            subject: "Confirm your address for DumbMail report emails".into(),
            html,
            text,
            unsubscribe_url: None,
        };
        if let Err(e) = state.email.send(&msg).await {
            tracing::error!(user = %user_id, error = %format!("{e:#}"), "couldn't send a report email confirmation");
        }
    }
    Ok(())
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/report-emails/{token}", get(page_h).post(confirm_h))
}

enum Outcome {
    Waiting { email: String, account: String },
    Confirmed { email: String, account: String },
    Expired,
}

async fn lookup(state: &AppState, id: Uuid, confirm: bool) -> AppResult<Option<Outcome>> {
    if confirm {
        // Only while the link is fresh, and only for the address the member still has.
        sqlx::query(
            "UPDATE report_recipients r SET confirmed_at = now() FROM users u
             WHERE r.id = $1 AND u.id = r.user_id AND u.email = r.email AND r.confirmed_at IS NULL AND r.asked_at >= now() - make_interval(days => $2)",
        )
        .bind(id)
        .bind(LINK_DAYS)
        .execute(&state.db)
        .await?;
    }
    let row: Option<(String, String, bool, bool)> = sqlx::query_as(
        "SELECT r.email, a.name, r.confirmed_at IS NOT NULL, r.asked_at < now() - make_interval(days => $2) OR u.email <> r.email
         FROM report_recipients r JOIN accounts a ON a.id = r.account_id JOIN users u ON u.id = r.user_id WHERE r.id = $1",
    )
    .bind(id)
    .bind(LINK_DAYS)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|(email, account, done, stale)| match (done, stale) {
        (true, _) => Outcome::Confirmed { email, account },
        (false, true) => Outcome::Expired,
        (false, false) => Outcome::Waiting { email, account },
    }))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn respond(state: &AppState, token: &str, confirm: bool) -> Response {
    let Some(id) = id_from_token(state, token) else { return (StatusCode::NOT_FOUND, "link not found").into_response() };
    let body = match lookup(state, id, confirm).await {
        Ok(None) => return (StatusCode::NOT_FOUND, "link not found").into_response(),
        Ok(Some(Outcome::Waiting { email, account })) => format!(
            r#"<h1>Get report emails?</h1><p>Scheduled reports from {} will be emailed to {}.</p><form method="post"><button type="submit">Yes, send them here</button></form>"#,
            esc(&account),
            esc(&email)
        ),
        Ok(Some(Outcome::Confirmed { email, account })) => format!("<h1>You're set</h1><p>Scheduled reports from {} will be emailed to {}.</p>", esc(&account), esc(&email)),
        Ok(Some(Outcome::Expired)) => "<h1>This link has expired</h1><p>Ask a teammate to save the report's schedule again, and we'll send you a new one.</p>".to_string(),
        Err(e) => {
            tracing::error!(error = %e.public_message(), "report email confirmation failed");
            "<h1>Something went wrong</h1><p>Please try the link again in a minute.</p>".to_string()
        }
    };
    crate::public::page("Report emails", &body, "#1f6f5c").into_response()
}

async fn page_h(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    respond(&state, &token, false).await
}

async fn confirm_h(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    respond(&state, &token, true).await
}

/// The schedule's recipients with their state, for the dashboard and API.
pub fn recipient_json(ids: &[Uuid], st: &HashMap<Uuid, (String, Option<String>, &'static str)>) -> Vec<Value> {
    ids.iter()
        .filter_map(|id| st.get(id).map(|(email, name, status)| json!({"id": id, "email": email, "name": name, "status": status})))
        .collect()
}
