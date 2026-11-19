//! Meta's webhooks for one account: `GET` answers Meta's verification with the account's verify
//! token; `POST` is checked against `X-Hub-Signature-256` (HMAC-SHA256 of the raw body with the
//! account's app secret) before anything in it is read.
//!
//! Only the connected sender's events have effects: entries for the account's WhatsApp Business
//! Account, and for message and preference events, the account's own phone number id
//! (`metadata.phone_number_id`). Handled fields: `messages` (inbound messages and statuses),
//! `user_preferences` (a person stopping marketing messages), `message_template_status_update`,
//! `template_category_update`, `message_template_quality_update` and `phone_number_quality_update`.
//! Unknown fields and keys are ignored. Meta retries a delivery that doesn't get a 200 for up to 7
//! days, so each piece applies once: inbound messages by their id, statuses only move a message
//! forward, preferences by who, when and what, template events by Meta's time for them.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeZone, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use uuid::Uuid;

use super::graph::{Class, GraphError};
use super::inbox::{self, Inbound};
use super::send::{self, UNDELIVERABLE_LIMIT};
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

/// A Unix time from Meta (a number or a string of digits), never in the future.
pub fn meta_time(v: &Value) -> Option<DateTime<Utc>> {
    let secs = v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))?;
    Utc.timestamp_opt(secs, 0).single().map(|t| t.min(Utc::now()))
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
        let entry_at = meta_time(&entry["time"]);
        for change in entry["changes"].as_array().into_iter().flatten() {
            let value = &change["value"];
            // Message and preference events name the phone number they're for: only ours count.
            let ours = phone_number_id.is_some_and(|p| value["metadata"]["phone_number_id"].as_str() == Some(p));
            match change["field"].as_str() {
                Some("messages") if ours => {
                    for m in value["messages"].as_array().into_iter().flatten() {
                        match Inbound::parse(m, &value["contacts"]) {
                            Some(rx) => inbox::receive(state, account_id, &rx).await?,
                            None => tracing::info!(account = %account_id, "WhatsApp message without an id or sender; not stored"),
                        }
                    }
                    for s in value["statuses"].as_array().into_iter().flatten() {
                        if !status(state, account_id, s).await? {
                            complete = false;
                        }
                    }
                }
                Some("user_preferences") if ours => {
                    for p in value["user_preferences"].as_array().into_iter().flatten() {
                        preference(state, account_id, p, &value["contacts"]).await?;
                    }
                }
                Some("message_template_status_update") => {
                    let meta_id = value["message_template_id"].as_i64().map(|i| i.to_string()).or_else(|| value["message_template_id"].as_str().map(str::to_string));
                    let event = value["event"].as_str().map(|s| s.to_ascii_uppercase()).filter(|e| e.len() <= 40);
                    if let (Some(meta_id), Some(event)) = (meta_id, event) {
                        let reason = value["reason"].as_str().map(|s| s.chars().take(100).collect::<String>());
                        match templates::status_update(&state.db, account_id, &meta_id, &event, reason.as_deref(), entry_at.unwrap_or_else(Utc::now)).await? {
                            templates::StatusChange::Unknown => {
                                crate::jobs::enqueue_background(&state.db, "whatsapp_sync_templates", json!({"account_id": account_id}), None).await?;
                            }
                            templates::StatusChange::Applied { paused_now: true } => {
                                let name = value["message_template_name"].as_str().unwrap_or("a template");
                                super::alert_template(state, account_id, name, &event).await;
                            }
                            _ => {}
                        }
                    }
                }
                Some("template_category_update") => {
                    // A completed change carries `previous_category`; an upcoming one (`correct_category`)
                    // changes nothing yet.
                    let meta_id = value["message_template_id"].as_i64().map(|i| i.to_string()).or_else(|| value["message_template_id"].as_str().map(str::to_string));
                    let category = value["new_category"].as_str().map(str::to_ascii_uppercase).filter(|c| matches!(c.as_str(), "MARKETING" | "UTILITY" | "AUTHENTICATION"));
                    if let (Some(meta_id), Some(category), true) = (meta_id, category, value.get("previous_category").is_some()) {
                        if !templates::category_update(&state.db, account_id, &meta_id, &category, entry_at.unwrap_or_else(Utc::now)).await? {
                            crate::jobs::enqueue_background(&state.db, "whatsapp_sync_templates", json!({"account_id": account_id}), None).await?;
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
                _ => {}
            }
        }
    }
    Ok(complete)
}

/// Remember a webhook event once. Returns false when it was already seen.
async fn first_time(db: &sqlx::PgPool, account_id: Uuid, key: &str) -> sqlx::Result<bool> {
    Ok(sqlx::query("INSERT INTO whatsapp_seen (account_id, key) VALUES ($1, $2) ON CONFLICT DO NOTHING").bind(account_id).bind(key).execute(db).await?.rows_affected() == 1)
}

/// Meta's "stop marketing messages" (and "resume") choice, applied once and in the order it was
/// made. Stop unsubscribes the number from WhatsApp marketing; a person known only by Meta's
/// business-scoped id has the stop kept until their number is known. Resume is kept in the history
/// but subscribes nobody: only an explicit opt-in does.
async fn preference(state: &AppState, account_id: Uuid, p: &Value, contacts: &Value) -> AppResult<()> {
    if p["category"].as_str() != Some("marketing_messages") {
        return Ok(());
    }
    let value = p["value"].as_str().unwrap_or_default();
    let phone = p["wa_id"].as_str().and_then(|w| crate::models::normalize_phone(&format!("+{}", w.chars().filter(char::is_ascii_digit).collect::<String>())));
    let user_id = p["user_id"].as_str().or_else(|| contacts[0]["user_id"].as_str()).map(|s| s.chars().take(128).collect::<String>());
    let Some(who) = phone.clone().or(user_id.clone()) else { return Ok(()) };
    let at = meta_time(&p["timestamp"]);
    let key = format!("pref:{who}:{}:{value}", p["timestamp"]);
    if !first_time(&state.db, account_id, &key).await? {
        return Ok(());
    }
    // Without Meta's time, a stop still applies now; a resume needs nothing.
    let at = at.unwrap_or_else(Utc::now);
    let mut tx = state.db.begin().await?;
    let mut changed_for: Option<(String, Option<Uuid>)> = None;
    match (value, phone) {
        ("stop", Some(phone)) => {
            consent::lock(&mut tx, account_id, &phone).await?;
            let profile: Option<Uuid> = sqlx::query_scalar("SELECT id FROM profiles WHERE account_id = $1 AND phone_number = $2").bind(account_id).bind(&phone).fetch_optional(&mut *tx).await?;
            if let Some(uid) = &user_id {
                consent::learn_user_id(&mut tx, &state.db, account_id, &phone, uid, profile).await?;
            }
            if consent::set(&mut tx, &state.db, account_id, &phone, profile, "unsubscribed", "meta_opt_out", json!({"timestamp": p["timestamp"]}), at).await? {
                changed_for = Some((phone, profile));
            }
        }
        ("stop", None) => {
            if let Some(uid) = &user_id {
                consent::opt_out_user_id(&mut tx, &state.db, account_id, uid, "meta_opt_out", at).await?;
            }
        }
        ("resume", Some(phone)) => {
            consent::lock(&mut tx, account_id, &phone).await?;
            consent::ensure_contact(&mut tx, account_id, &phone, None).await?;
            sqlx::query("INSERT INTO whatsapp_consent_log (id, account_id, phone_number, consent, source, detail) VALUES ($1, $2, $3, 'resume_requested', 'meta_opt_out', $4)")
                .bind(Uuid::now_v7())
                .bind(account_id)
                .bind(&phone)
                .bind(json!({"timestamp": p["timestamp"], "note": "They allowed marketing messages again in WhatsApp; that alone doesn't subscribe them."}))
                .execute(&mut *tx)
                .await?;
        }
        _ => {}
    }
    tx.commit().await?;
    if let Some((phone, Some(pid))) = changed_for {
        record_event(
            &state.db,
            NewEvent {
                account_id,
                profile_id: pid,
                metric: "Unsubscribed from WhatsApp",
                properties: json!({"method": "whatsapp_setting"}),
                value: None,
                occurred_at: Some(at),
                unique_id: Some(format!("wa-pref:{phone}:{}", at.timestamp())),
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

/// One message status, in one transaction under the number's lock with everything it causes.
/// Statuses only move a message forward (sent → delivered → read; failed only before it was
/// delivered), so a repeated or late one changes nothing. Sent (or later) charges the held credit
/// once; failed releases it, reschedules Meta's rate-limit and undeliverable failures (a bounded
/// number of times), and records an opt-out for 131050. Ok(false): an unknown message id recent
/// enough to be racing its own send.
async fn status(state: &AppState, account_id: Uuid, s: &Value) -> AppResult<bool> {
    let (Some(wamid), Some(st)) = (s["id"].as_str(), s["status"].as_str()) else { return Ok(true) };
    let r = rank(st);
    if r == 0 || wamid.len() > 200 {
        return Ok(true);
    }
    let at = meta_time(&s["timestamp"]).unwrap_or_else(Utc::now);
    let code = s["errors"][0]["code"].as_i64();
    let category = s["pricing"]["category"].as_str().map(|c| c.chars().take(40).collect::<String>());
    // The message and its number first (the opt-out lock), then the change.
    let target: Option<(Uuid, String)> = sqlx::query_as("SELECT message_id, COALESCE(sent_to, '') FROM whatsapp_sent WHERE wamid = $1 AND account_id = $2").bind(wamid).bind(account_id).fetch_optional(&state.db).await?;
    let Some((message_id, to)) = target else {
        // Sent outside DumbMail, or racing its own send.
        return Ok((Utc::now() - at).num_seconds() > RACE_SECONDS);
    };
    let mut tx = state.db.begin().await?;
    consent::lock(&mut tx, account_id, &to).await?;
    type Moved = (Uuid, bool, Option<Uuid>, Option<Uuid>, i32, Option<Uuid>);
    let moved: Option<Moved> = sqlx::query_as(
        "UPDATE whatsapp_sent w SET wa_status = $3, wa_status_at = $4, error_code = COALESCE($5, w.error_code), pricing_category = COALESCE($6, w.pricing_category)
         FROM messages m
         WHERE w.message_id = $7 AND w.wamid = $2 AND w.account_id = $1 AND m.id = w.message_id
           AND (CASE w.wa_status WHEN 'sent' THEN 1 WHEN 'delivered' THEN 2 WHEN 'read' THEN 3 WHEN 'failed' THEN 4 ELSE 0 END) < $8
           AND NOT ($3 = 'failed' AND COALESCE(w.wa_status, '') IN ('delivered', 'read'))
         RETURNING m.profile_id, m.is_test, m.campaign_id, m.flow_id, w.attempts, w.template_id",
    )
    .bind(account_id)
    .bind(wamid)
    .bind(st)
    .bind(at)
    .bind(code.map(|c| c as i32))
    .bind(&category)
    .bind(message_id)
    .bind(r)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((profile_id, is_test, _, _, attempts, template_id)) = moved else {
        // A repeat or an older status.
        return Ok(true);
    };
    let mut alert_template: Option<(String, &'static str)> = None;
    let mut alert_quality: Option<String> = None;
    let mut reschedule: Option<(DateTime<Utc>, String)> = None;
    match st {
        "sent" | "delivered" | "read" => {
            send::settle(&mut tx, account_id, message_id).await?;
            if st != "sent" {
                sqlx::query("UPDATE messages SET status = 'delivered', delivered_at = COALESCE(delivered_at, $2) WHERE id = $1 AND status IN ('sent', 'delivered')").bind(message_id).bind(at).execute(&mut *tx).await?;
            }
            if st == "read" {
                sqlx::query("UPDATE messages SET opened_at = COALESCE(opened_at, $2) WHERE id = $1").bind(message_id).bind(at).execute(&mut *tx).await?;
            }
        }
        _ => {
            let e = GraphError { code: code.unwrap_or(0), subcode: None, status: 200, unclear: false };
            let why = e.describe();
            send::release(&mut tx, account_id, message_id).await?;
            let next = match e.class() {
                Class::RateLimited => {
                    let wait = [60i64, 300, 1800, 7200].get(attempts.max(1) as usize - 1).copied();
                    wait.map(|w| Utc::now() + chrono::Duration::seconds(w))
                }
                Class::NotOnWhatsApp => [6 * 3600i64, 24 * 3600].get(attempts.max(1) as usize - 1).map(|w| Utc::now() + chrono::Duration::seconds(*w)),
                _ => None,
            };
            match (next, is_test) {
                (Some(when), false) => reschedule = Some((when, why.clone())),
                _ => {
                    let n = sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1 AND status = 'sent'").bind(message_id).bind(&why).execute(&mut *tx).await?.rows_affected();
                    if n > 0 && !is_test {
                        crate::features::messages_unsent_in(&mut tx, &state.db, account_id, &[message_id]).await;
                    }
                }
            }
            match (e.class(), code) {
                (Class::MarketingOptOut, _) => {
                    // The opt-out commits with the status that carried it.
                    consent::set(&mut tx, &state.db, account_id, &to, Some(profile_id), "unsubscribed", "meta_error", json!({"code": 131050, "message_id": message_id}), at).await?;
                }
                (Class::TemplatePaused, Some(c)) => {
                    if let Some(tid) = template_id {
                        let status = if c == 132016 { "DISABLED" } else { "PAUSED" };
                        let name: Option<String> = sqlx::query_scalar(
                            "UPDATE whatsapp_templates SET status = $3, paused_at = $4, status_at = GREATEST(status_at, $4), updated_at = now()
                             WHERE id = $1 AND account_id = $2 AND status NOT IN ('PAUSED', 'DISABLED') RETURNING name",
                        )
                        .bind(tid)
                        .bind(account_id)
                        .bind(status)
                        .bind(at)
                        .fetch_optional(&mut *tx)
                        .await?;
                        alert_template = name.map(|n| (n, status));
                    }
                }
                (Class::Restricted, _) => alert_quality = Some(why.clone()),
                (Class::RateLimited, Some(c)) => {
                    let scope = if c == 131056 { format!("pair:{to}") } else { "sender".into() };
                    sqlx::query("INSERT INTO whatsapp_backoff (account_id, scope, until) VALUES ($1, $2, $3) ON CONFLICT (account_id, scope) DO UPDATE SET until = GREATEST(whatsapp_backoff.until, EXCLUDED.until)")
                        .bind(account_id)
                        .bind(scope)
                        .bind(reschedule.as_ref().map(|r| r.0).unwrap_or_else(|| Utc::now() + chrono::Duration::hours(2)))
                        .execute(&mut *tx)
                        .await?;
                }
                _ => {}
            }
        }
    }
    tx.commit().await?;
    if code == Some(131026) {
        consent::undeliverable(&state.db, account_id, &to, UNDELIVERABLE_LIMIT).await?;
    }
    if let Some((when, why)) = reschedule {
        send::reschedule(state, account_id, message_id, when, &why, None).await?;
    }
    if let Some((name, status)) = alert_template {
        super::alert_template(state, account_id, &name, status).await;
    }
    if let Some(why) = alert_quality {
        super::alert_quality(state, account_id, &why).await;
    }
    if st == "read" && !is_test {
        record_event(
            &state.db,
            NewEvent { account_id, profile_id, metric: "Read WhatsApp", properties: json!({"message_id": message_id}), value: None, occurred_at: Some(at), unique_id: Some(format!("wa-read:{message_id}")), message_id: Some(message_id) },
        )
        .await?;
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
        assert_eq!(meta_time(&json!("1751247548")).unwrap().timestamp(), 1751247548);
        assert!(meta_time(&json!("soon")).is_none());
    }
}
