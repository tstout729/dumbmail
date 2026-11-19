//! Meta's webhooks for one account: `GET` answers Meta's verification with the account's verify
//! token; `POST` is checked against `X-Hub-Signature-256` (HMAC-SHA256 of the raw body with the
//! account's app secret) before anything in it is read.
//!
//! Handled fields: `messages` (inbound messages and message statuses), `message_template_status_update`,
//! `message_template_quality_update`, `phone_number_quality_update` and `user_preferences` (a person
//! stopping marketing messages). Everything else is ignored, as are unknown keys. Meta retries a
//! delivery that doesn't get a 200 for up to 7 days, so each piece is idempotent: inbound messages
//! by their id, statuses only move a message forward, consent changes are idempotent.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{TimeZone, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use uuid::Uuid;

use super::inbox::{self, Inbound};
use super::{consent, templates};
use crate::audience::{record_event, NewEvent};
use crate::error::AppResult;
use crate::state::AppState;

/// Biggest delivery read (Meta batches up to 1,000 updates).
pub const MAX_BODY: usize = 4 * 1024 * 1024;
/// A status for a message id we don't know yet, this recent, may be racing the send that's
/// recording it: answer non-200 so Meta delivers it again.
const RACE_SECONDS: i64 = 120;

/// Whether `header` (`sha256=<hex>`) is the body's HMAC with `secret`.
pub fn signature_ok(secret: &str, body: &[u8], header: Option<&str>) -> bool {
    let Some(hex_sig) = header.and_then(|h| h.trim().strip_prefix("sha256=")) else { return false };
    let Ok(sig) = hex::decode(hex_sig.trim()) else { return false };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else { return false };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `GET /webhooks/whatsapp/{key}`: Meta's one-time check when the webhook is set up.
pub async fn verify(State(state): State<AppState>, Path(key): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    if !valid_key(&key) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let token: Option<String> = sqlx::query_scalar("SELECT verify_token FROM whatsapp_settings WHERE webhook_key = $1").bind(&key).fetch_optional(&state.db).await.ok().flatten();
    let (Some(token), Some("subscribe"), Some(given), Some(challenge)) = (token, q.get("hub.mode").map(String::as_str), q.get("hub.verify_token"), q.get("hub.challenge")) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let same = token.len() == given.len() && subtle::ConstantTimeEq::ct_eq(token.as_bytes(), given.as_bytes()).into();
    if !same || challenge.len() > 200 {
        return StatusCode::FORBIDDEN.into_response();
    }
    (StatusCode::OK, challenge.clone()).into_response()
}

/// `POST /webhooks/whatsapp/{key}`.
pub async fn receive(State(state): State<AppState>, Path(key): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    if !valid_key(&key) || body.len() > MAX_BODY {
        return StatusCode::NOT_FOUND.into_response();
    }
    let row: Option<(Uuid, Option<String>, Option<String>, Option<String>)> =
        match sqlx::query_as("SELECT account_id, app_secret_enc, waba_id, phone_number_id FROM whatsapp_settings WHERE webhook_key = $1").bind(&key).fetch_optional(&state.db).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "WhatsApp webhook: settings lookup failed");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        };
    let Some((account_id, Some(secret_enc), waba_id, phone_number_id)) = row else {
        // No such address, or no app secret to check it with: nothing is read.
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(secret) = crate::crypto::decrypt(&state.cfg.secret_key, &secret_enc) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !signature_ok(&secret, &body, headers.get("x-hub-signature-256").and_then(|v| v.to_str().ok())) {
        tracing::warn!(account = %account_id, "WhatsApp webhook with a bad signature; ignored");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else { return StatusCode::BAD_REQUEST.into_response() };
    let _ = sqlx::query("UPDATE whatsapp_settings SET last_webhook_at = now() WHERE account_id = $1").bind(account_id).execute(&state.db).await;
    if v["object"].as_str() != Some("whatsapp_business_account") {
        return StatusCode::OK.into_response();
    }
    match handle(&state, account_id, waba_id.as_deref(), phone_number_id.as_deref(), &v).await {
        Ok(true) => StatusCode::OK.into_response(),
        // A status that may be racing its own send: Meta tries again shortly.
        Ok(false) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(e) => {
            tracing::error!(account = %account_id, error = %e.public_message(), "WhatsApp webhook not fully handled; Meta will retry");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Handle one verified delivery. Ok(false): something should be delivered again later.
async fn handle(state: &AppState, account_id: Uuid, waba_id: Option<&str>, phone_number_id: Option<&str>, v: &Value) -> AppResult<bool> {
    let mut complete = true;
    for entry in v["entry"].as_array().into_iter().flatten() {
        // Only this account's own WhatsApp Business Account.
        if waba_id.is_none_or(|w| entry["id"].as_str() != Some(w)) {
            continue;
        }
        for change in entry["changes"].as_array().into_iter().flatten() {
            let value = &change["value"];
            match change["field"].as_str() {
                Some("messages") => {
                    if phone_number_id.is_none_or(|p| value["metadata"]["phone_number_id"].as_str() != Some(p)) {
                        continue;
                    }
                    for m in value["messages"].as_array().into_iter().flatten() {
                        match Inbound::parse(m, &value["contacts"]) {
                            Some(rx) => inbox::receive(state, account_id, &rx).await?,
                            None => tracing::info!(account = %account_id, "WhatsApp message without a phone number or id; not stored"),
                        }
                    }
                    for s in value["statuses"].as_array().into_iter().flatten() {
                        if !status(state, account_id, s).await? {
                            complete = false;
                        }
                    }
                }
                Some("message_template_status_update") => {
                    let meta_id = value["message_template_id"].as_i64().map(|i| i.to_string()).or_else(|| value["message_template_id"].as_str().map(str::to_string));
                    let event = value["event"].as_str().map(|s| s.to_ascii_uppercase());
                    if let (Some(meta_id), Some(event)) = (meta_id, event) {
                        if event.len() > 40 {
                            continue;
                        }
                        let reason = value["reason"].as_str().map(|s| s.chars().take(100).collect::<String>());
                        let known = templates::status_update(&state.db, account_id, &meta_id, &event, reason.as_deref()).await?;
                        if !known {
                            crate::jobs::enqueue_background(&state.db, "whatsapp_sync_templates", json!({"account_id": account_id}), None).await?;
                        } else if matches!(event.as_str(), "PAUSED" | "DISABLED") {
                            let name = value["message_template_name"].as_str().unwrap_or("a template");
                            super::alert_template(state, account_id, name, &event).await;
                        }
                    }
                }
                Some("message_template_quality_update") => {
                    let meta_id = value["message_template_id"].as_i64().map(|i| i.to_string());
                    if let (Some(meta_id), Some(score)) = (meta_id, value["new_quality_score"].as_str()) {
                        sqlx::query("UPDATE whatsapp_templates SET quality = $3 WHERE account_id = $1 AND meta_id = $2")
                            .bind(account_id)
                            .bind(meta_id)
                            .bind(score.chars().take(40).collect::<String>())
                            .execute(&state.db)
                            .await?;
                    }
                }
                Some("phone_number_quality_update") => {
                    // `max_daily_conversations_per_business` replaces the deprecated `current_limit`.
                    let limit = value["max_daily_conversations_per_business"].as_str().or(value["current_limit"].as_str()).map(|s| s.chars().take(40).collect::<String>());
                    sqlx::query("UPDATE whatsapp_settings SET messaging_limit = COALESCE($2, messaging_limit), updated_at = now() WHERE account_id = $1")
                        .bind(account_id)
                        .bind(limit)
                        .execute(&state.db)
                        .await?;
                }
                Some("user_preferences") => {
                    for p in value["user_preferences"].as_array().into_iter().flatten() {
                        preference(state, account_id, p).await?;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(complete)
}

/// Meta's "stop marketing messages" (and "resume") choice. Stop unsubscribes the number from
/// WhatsApp marketing at once. Resume is kept in the history but subscribes nobody: only an
/// explicit opt-in does.
async fn preference(state: &AppState, account_id: Uuid, p: &Value) -> AppResult<()> {
    if p["category"].as_str() != Some("marketing_messages") {
        return Ok(());
    }
    let Some(digits) = p["wa_id"].as_str().map(|s| s.chars().filter(char::is_ascii_digit).collect::<String>()) else { return Ok(()) };
    let Some(phone) = crate::models::normalize_phone(&format!("+{digits}")) else { return Ok(()) };
    let at = p["timestamp"].as_i64();
    let mut tx = state.db.begin().await?;
    consent::lock(&mut tx, account_id, &phone).await?;
    let profile: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(&phone).fetch_optional(&mut *tx).await?;
    let changed = match p["value"].as_str() {
        Some("stop") => consent::set(&mut tx, &state.db, account_id, &phone, profile, "unsubscribed", "meta_opt_out", json!({"timestamp": at})).await?,
        Some("resume") => {
            consent::ensure_contact(&mut tx, account_id, &phone, profile).await?;
            sqlx::query("INSERT INTO whatsapp_consent_log (id, account_id, phone_number, profile_id, consent, source, detail) VALUES ($1, $2, $3, $4, 'resume_requested', 'meta_opt_out', $5)")
                .bind(Uuid::now_v7())
                .bind(account_id)
                .bind(&phone)
                .bind(profile)
                .bind(json!({"timestamp": at, "note": "They allowed marketing messages again in WhatsApp; that alone doesn't subscribe them."}))
                .execute(&mut *tx)
                .await?;
            false
        }
        _ => false,
    };
    tx.commit().await?;
    if let (true, Some(pid)) = (changed, profile) {
        record_event(
            &state.db,
            NewEvent {
                account_id,
                profile_id: pid,
                metric: "Unsubscribed from WhatsApp",
                properties: json!({"method": "whatsapp_setting"}),
                value: None,
                occurred_at: None,
                unique_id: Some(format!("wa-pref:{phone}:{}", at.unwrap_or_default())),
                message_id: None,
            },
        )
        .await?;
    }
    Ok(())
}

fn rank(s: &str) -> i32 {
    match s {
        "sent" => 1,
        "delivered" => 2,
        "read" => 3,
        "failed" => 4,
        _ => 0,
    }
}

/// One message status. Statuses only move a message forward (sent → delivered → read; failed only
/// before it was delivered), so a repeated or late one changes nothing. Ok(false): an unknown
/// message id recent enough to be racing its own send.
async fn status(state: &AppState, account_id: Uuid, s: &Value) -> AppResult<bool> {
    let (Some(wamid), Some(st)) = (s["id"].as_str(), s["status"].as_str()) else { return Ok(true) };
    let r = rank(st);
    if r == 0 || wamid.len() > 200 {
        return Ok(true);
    }
    let at = s["timestamp"].as_str().and_then(|t| t.parse::<i64>().ok()).and_then(|t| Utc.timestamp_opt(t, 0).single()).map_or_else(Utc::now, |t| t.min(Utc::now()));
    let code = s["errors"][0]["code"].as_i64();
    let category = s["pricing"]["category"].as_str().map(|c| c.chars().take(40).collect::<String>());
    let moved: Option<(Uuid, String, Uuid, bool)> = sqlx::query_as(
        "UPDATE whatsapp_sent w SET wa_status = $3, wa_status_at = $4, error_code = COALESCE($5, w.error_code), pricing_category = COALESCE($6, w.pricing_category)
         FROM messages m
         WHERE w.wamid = $2 AND w.account_id = $1 AND m.id = w.message_id
           AND (CASE w.wa_status WHEN 'sent' THEN 1 WHEN 'delivered' THEN 2 WHEN 'read' THEN 3 WHEN 'failed' THEN 4 ELSE 0 END) < $7
           AND NOT ($3 = 'failed' AND w.wa_status IN ('delivered', 'read'))
         RETURNING w.message_id, m.to_address, m.profile_id, m.is_test",
    )
    .bind(account_id)
    .bind(wamid)
    .bind(st)
    .bind(at)
    .bind(code.map(|c| c as i32))
    .bind(&category)
    .bind(r)
    .fetch_optional(&state.db)
    .await?;
    let Some((message_id, to, profile_id, is_test)) = moved else {
        let known: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM whatsapp_sent WHERE wamid = $1 AND account_id = $2)").bind(wamid).bind(account_id).fetch_one(&state.db).await?;
        // Known: a repeat or an older status. Unknown: sent outside DumbMail, or racing its own send.
        return Ok(known || (Utc::now() - at).num_seconds() > RACE_SECONDS);
    };
    match st {
        "delivered" => {
            sqlx::query("UPDATE messages SET status = 'delivered', delivered_at = COALESCE(delivered_at, $2) WHERE id = $1 AND status IN ('sent', 'delivered')").bind(message_id).bind(at).execute(&state.db).await?;
        }
        "read" => {
            sqlx::query(
                "UPDATE messages SET status = CASE WHEN status = 'sent' THEN 'delivered' ELSE status END, delivered_at = COALESCE(delivered_at, $2), opened_at = COALESCE(opened_at, $2)
                 WHERE id = $1 AND status IN ('sent', 'delivered')",
            )
            .bind(message_id)
            .bind(at)
            .execute(&state.db)
            .await?;
            if !is_test {
                record_event(
                    &state.db,
                    NewEvent { account_id, profile_id, metric: "Read WhatsApp", properties: json!({"message_id": message_id}), value: None, occurred_at: Some(at), unique_id: Some(format!("wa-read:{message_id}")), message_id: Some(message_id) },
                )
                .await?;
            }
        }
        "failed" => {
            let why = super::graph::GraphError { code: code.unwrap_or(0), subcode: None, status: 200, unclear: false }.describe();
            let n = sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1 AND status = 'sent'").bind(message_id).bind(&why).execute(&state.db).await?.rows_affected();
            if n > 0 && !is_test {
                crate::features::messages_unsent(&state.db, account_id, &[message_id]).await;
            }
            match code {
                Some(131026) => consent::mark_unreachable(&state.db, account_id, &to).await?,
                Some(131050) => {
                    let mut tx = state.db.begin().await?;
                    consent::lock(&mut tx, account_id, &to).await?;
                    consent::set(&mut tx, &state.db, account_id, &to, Some(profile_id), "unsubscribed", "meta_error", json!({"code": 131050, "message_id": message_id})).await?;
                    tx.commit().await?;
                }
                Some(c @ (132015 | 132016)) => {
                    let tid: Option<Uuid> = sqlx::query_scalar("SELECT template_id FROM whatsapp_sent WHERE message_id = $1").bind(message_id).fetch_optional(&state.db).await?.flatten();
                    if let Some(tid) = tid {
                        if templates::mark_paused(&state.db, account_id, tid, c).await? {
                            let name: String = sqlx::query_scalar("SELECT name FROM whatsapp_templates WHERE id = $1").bind(tid).fetch_one(&state.db).await?;
                            super::alert_template(state, account_id, &name, if c == 132016 { "DISABLED" } else { "PAUSED" }).await;
                        }
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let body = br#"{"object":"whatsapp_business_account"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"appsecret").unwrap();
        mac.update(body);
        let good = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(signature_ok("appsecret", body, Some(&good)));
        assert!(!signature_ok("other", body, Some(&good)));
        assert!(!signature_ok("appsecret", b"{}", Some(&good)));
        assert!(!signature_ok("appsecret", body, None));
        assert!(!signature_ok("appsecret", body, Some("sha1=abc")));
    }
}
