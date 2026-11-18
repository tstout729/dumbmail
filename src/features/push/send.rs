//! Push as a channel: sending one push message (a `messages` row, channel `push`) to a person's
//! devices, tests, and retries.
//!
//! Order, like email and texts: consent (devices that allowed notifications and no push opt-out),
//! quiet hours in the recipient's zone (the account's texting window), rendering, the balance, the
//! message row, the rate limiter, then the claim. The claim is one conditional write against the
//! current state (still queued, not opted out, the campaign still sending) that also fixes which
//! devices it goes to. Right before each provider call, a second conditional write checks that
//! device is still active and the person still hasn't opted out. A delivery is handed over at most
//! once: a crash after the claim leaves it `sending`, and the sweep marks it failed instead of
//! sending it again. Billing and the "Received Push" event happen once, on the write that records
//! the first device that accepted it.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use super::content::{self, PushContent, Rendered};
use super::providers::{self, Sent};
use super::settings::{self, Row};
use crate::audience::{record_event, NewEvent};
use crate::error::{AppError, AppResult};
use crate::models::{Account, Profile};
use crate::sending::{Outcome, SendSpec};
use crate::state::AppState;

/// Devices per person a message goes to (their most recently seen).
pub const MAX_DEVICES_PER_MESSAGE: i64 = 10;
/// Failed hand-offs in a row before a device is turned off (a gone device is removed at once).
pub const MAX_FAILURES: i32 = 5;
/// Hand-off attempts per delivery: the first, then retries after these waits.
const RETRY_WAITS: [i64; 3] = [60, 300, 1800];

/// SQL true when profile `p` can get push now: a device that allowed notifications, no opt-out.
pub const REACHABLE_SQL: &str = "(NOT EXISTS (SELECT 1 FROM push_opt_outs po WHERE po.profile_id = p.id) AND EXISTS (SELECT 1 FROM push_devices pd WHERE pd.profile_id = p.id AND pd.status = 'active'))";

/// Who a send goes to.
enum Target<'a> {
    /// The person's own devices (campaigns, flows, and tests to a person). For a test to a team
    /// member, also their own test browsers.
    Profile(Option<Uuid>),
    /// A team member's dashboard test devices.
    Member(Uuid),
    /// One device of the account (a test).
    Device(&'a Uuid),
}

/// The channel's entry point from `sending::send` (campaigns and flows).
pub async fn send(state: &AppState, spec: &SendSpec<'_>) -> AppResult<Outcome> {
    run(state, spec, Target::Profile(None)).await
}

async fn run(state: &AppState, spec: &SendSpec<'_>, target: Target<'_>) -> AppResult<Outcome> {
    let account = spec.account;
    let p = spec.profile;
    let c = content::parse(spec.content)?;
    let Some(row) = settings::load(&state.db, account.id).await? else { return skip(state, spec, "push isn't set up").await };
    let ready = row.ready_kinds();
    let kinds: Vec<String> = c.platforms().into_iter().filter(|k| ready.contains(&k.as_str())).collect();
    if kinds.is_empty() {
        return skip(state, spec, "push isn't set up").await;
    }
    let opted_out: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM push_opt_outs WHERE profile_id = $1)").bind(p.id).fetch_one(&state.db).await?;
    if opted_out && matches!(target, Target::Profile(_)) {
        if spec.is_test {
            return Err(AppError::bad("this person turned push notifications off; pick someone else"));
        }
        return skip(state, spec, "push turned off").await;
    }
    let who = target.who(p.id);
    let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM push_devices d WHERE d.account_id = $1 AND d.status = 'active' AND d.kind = ANY($2) AND {WHO}"))
        .bind(account.id)
        .bind(&kinds)
        .bind(who.0)
        .bind(who.1)
        .bind(who.2)
        .fetch_one(&state.db)
        .await?;
    let devices = n.min(MAX_DEVICES_PER_MESSAGE);
    if devices == 0 {
        if spec.is_test {
            return Err(AppError::bad("no device that allowed notifications to send this to"));
        }
        return skip(state, spec, "no push device").await;
    }
    // Quiet hours in their own zone: the account's texting window (see features::sendtime). The
    // profile's zone can come from a browser, so it only ever narrows the hours (with the account's).
    if !spec.is_test {
        let ((start, end), _) = crate::features::sendtime::settings::sms_settings(&state.db, account.id).await?;
        let zones = crate::features::sendtime::settings::sms_zones(&p.properties, None, crate::sending::account_tz(account));
        if let Some(until) = crate::features::sendtime::settings::decide(&zones, Utc::now(), start, end, false).wait {
            return Ok(Outcome::Deferred(until));
        }
    }

    let message_id = spec.message_id.unwrap_or_else(Uuid::now_v7);
    let ctx = crate::render::context(account, Some(p), spec.event, "");
    let fallback = account.website.as_deref().map(|w| if w.contains("://") { w.to_string() } else { format!("https://{w}") }).unwrap_or_else(|| state.cfg.public_url().to_string());
    let rendered = content::render(&c, &ctx, &fallback, account.logo_url.as_deref())?;
    let title = if spec.is_test { format!("[Test] {}", if rendered.title.is_empty() { &rendered.body } else { &rendered.title }) } else { rendered.title.clone() };
    let subject: String = title.chars().take(200).collect();

    let price = crate::billing::quote(state, account.id, "push", 1, false, false).await?;
    if price > 0 && crate::billing::balance(&state.db, account.id).await? < price {
        if spec.is_test {
            return Err(AppError::LimitReached("your balance is empty; add credit in Settings → Billing".into()));
        }
        return skip(state, spec, "insufficient credit").await;
    }

    if spec.message_id.is_none() {
        sqlx::query(
            "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, subject, is_test)
             VALUES ($1, $2, $3, 'push', $4, $5, $6, $7, $8, $9)",
        )
        .bind(message_id)
        .bind(account.id)
        .bind(p.id)
        .bind(spec.campaign_id)
        .bind(spec.flow_id)
        .bind(spec.flow_node_id)
        .bind(format!("{} device{}", devices, if devices == 1 { "" } else { "s" }))
        .bind(&subject)
        .bind(spec.is_test)
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query("UPDATE messages SET subject = $2, to_address = $3 WHERE id = $1").bind(message_id).bind(&subject).bind(format!("{devices} device{}", if devices == 1 { "" } else { "s" })).execute(&state.db).await?;
    }

    crate::sending::throttle("push", f64::from(state.cfg.send_rate_per_sec.max(1))).await;

    let deliveries = match claim(state, spec, message_id, &kinds, &target).await? {
        Ok(d) => d,
        Err(why) => return Ok(Outcome::Skipped(why)),
    };
    let mut first_error: Option<String> = None;
    for (delivery_id, kind) in &deliveries {
        let payload = build_payload(state, account, &c, &rendered, &title, *delivery_id, kind);
        let r = hand_off(state, &row, *delivery_id, kind, &payload, &c, false).await?;
        if first_error.is_none() {
            first_error = r;
        }
    }
    finish(state, account.id, message_id, spec.is_test, first_error.as_deref()).await
}

/// The devices a target covers, with its (profile, member, device) bound as $3, $4 and $5.
const WHO: &str = "(d.profile_id = $3 OR d.test_user_id = $4 OR d.id = $5)";

impl Target<'_> {
    fn who(&self, profile_id: Uuid) -> (Option<Uuid>, Option<Uuid>, Option<Uuid>) {
        match self {
            Target::Profile(member) => (Some(profile_id), *member, None),
            Target::Member(u) => (None, Some(*u), None),
            Target::Device(d) => (None, None, Some(**d)),
        }
    }
}

/// A send that stops before any message row exists: campaigns mark their own row (the batch does it
/// from the outcome); flows and tests keep nothing, like email.
async fn skip(_state: &AppState, _spec: &SendSpec<'_>, why: &'static str) -> AppResult<Outcome> {
    Ok(Outcome::Skipped(why))
}

/// The claim: one transaction that marks the message `sending` only if it may still go (queued,
/// not opted out, the profile exists, the campaign still sending), and fixes its deliveries.
async fn claim(state: &AppState, spec: &SendSpec<'_>, message_id: Uuid, kinds: &[String], target: &Target<'_>) -> AppResult<Result<Vec<(Uuid, String)>, &'static str>> {
    let mut tx = state.db.begin().await?;
    let profile_check = matches!(target, Target::Profile(_));
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE messages m SET status = 'sending', claimed_at = COALESCE(m.claimed_at, now())
         WHERE m.id = $1 AND m.status IN ('queued', 'sending')
           AND EXISTS (SELECT 1 FROM profiles p WHERE p.id = m.profile_id)
           AND (NOT $2 OR NOT EXISTS (SELECT 1 FROM push_opt_outs o WHERE o.profile_id = m.profile_id))
           AND ($3 OR m.campaign_id IS NULL OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = m.campaign_id AND c.status = 'sending'))
         RETURNING m.id",
    )
    .bind(message_id)
    .bind(profile_check)
    .bind(spec.is_test)
    .fetch_optional(&mut *tx)
    .await?;
    if claimed.is_none() {
        let why: Option<(String, bool, Option<String>)> = sqlx::query_as(
            "SELECT m.status, EXISTS (SELECT 1 FROM push_opt_outs o WHERE o.profile_id = m.profile_id), (SELECT c.status FROM campaigns c WHERE c.id = m.campaign_id)
             FROM messages m WHERE m.id = $1",
        )
        .bind(message_id)
        .fetch_optional(&mut *tx)
        .await?;
        let reason = match &why {
            None => "profile deleted",
            Some((s, _, _)) if !matches!(s.as_str(), "queued" | "sending") => "no longer queued",
            Some((_, true, _)) => "push turned off",
            Some((_, _, Some(c))) if c == "cancelled" => "campaign cancelled",
            Some((_, _, Some(_))) => "campaign stopped",
            _ => "no longer queued",
        };
        mark_skipped(&mut tx, state, spec.account.id, message_id, reason, spec.is_test).await?;
        tx.commit().await?;
        return Ok(Err(reason));
    }
    let who = target.who(spec.profile.id);
    let rows: Vec<(Uuid, String)> = sqlx::query_as(&format!(
        "INSERT INTO push_deliveries (id, message_id, account_id, device_id, kind, status, claimed_at)
         SELECT gen_random_uuid(), $6, $1, d.id, d.kind, 'sending', now() FROM (
            SELECT d.id, d.kind FROM push_devices d WHERE d.account_id = $1 AND d.status = 'active' AND d.kind = ANY($2) AND {WHO}
            ORDER BY d.last_seen_at DESC LIMIT $7) d
         ON CONFLICT (message_id, device_id) DO NOTHING
         RETURNING id, kind"
    ))
    .bind(spec.account.id)
    .bind(kinds)
    .bind(who.0)
    .bind(who.1)
    .bind(who.2)
    .bind(message_id)
    .bind(MAX_DEVICES_PER_MESSAGE)
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        mark_skipped(&mut tx, state, spec.account.id, message_id, "no push device", spec.is_test).await?;
        tx.commit().await?;
        return Ok(Err("no push device"));
    }
    tx.commit().await?;
    Ok(Ok(rows))
}

async fn mark_skipped(tx: &mut sqlx::PgConnection, state: &AppState, account_id: Uuid, message_id: Uuid, why: &str, is_test: bool) -> AppResult<()> {
    let n = sqlx::query("UPDATE messages SET status = 'skipped', error = $2 WHERE id = $1 AND status IN ('queued', 'sending')")
        .bind(message_id)
        .bind(why)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n > 0 && !is_test {
        crate::features::messages_unsent_in(tx, &state.db, account_id, &[message_id]).await;
    }
    Ok(())
}

/// A signed token naming one delivery (in the payload, for "delivered" and "opened" reports).
pub fn delivery_token(state: &AppState, delivery_id: Uuid) -> String {
    crate::auth::sign(&state.cfg.secret_key, &format!("pd{}", delivery_id.simple()))
}

pub fn delivery_from_token(state: &AppState, token: &str) -> Option<Uuid> {
    if token.len() > 200 {
        return None;
    }
    let payload = crate::auth::verify(&state.cfg.secret_key, token)?;
    Uuid::parse_str(payload.strip_prefix("pd")?).ok()
}

/// A tracked link for a tap: `/p/c/{token}?u=...&s=...`, signed so it only goes where we sent it.
pub fn tracked_url(state: &AppState, account: &Account, delivery_id: Uuid, url: &str) -> String {
    let base = crate::domains::link_base(state, account);
    let mut u = url::Url::parse(&format!("{base}/p/c/{}", delivery_token(state, delivery_id))).expect("link base is a URL");
    u.query_pairs_mut().append_pair("u", url).append_pair("s", &crate::auth::link_sig(&state.cfg.secret_key, delivery_id, url));
    u.to_string()
}

/// What one device is sent, by kind. Web: the JSON the service worker reads. iOS: the APNs payload.
/// Android: the FCM message without its token.
fn build_payload(state: &AppState, account: &Account, c: &PushContent, r: &Rendered, title: &str, delivery_id: Uuid, kind: &str) -> Value {
    let tok = delivery_token(state, delivery_id);
    let url = tracked_url(state, account, delivery_id, &r.url);
    let actions: Vec<Value> = r.actions.iter().map(|a| json!({"t": a.title, "u": tracked_url(state, account, delivery_id, &a.url)})).collect();
    match kind {
        "web" => {
            let mut v = json!({"t": title, "b": r.body, "u": url, "r": tok});
            if let Some(i) = &r.icon {
                v["i"] = json!(i);
            }
            if let Some(i) = &r.image {
                v["im"] = json!(i);
            }
            if !actions.is_empty() {
                v["a"] = json!(actions);
            }
            if let Some(t) = c.topic.as_deref().filter(|t| !t.is_empty()) {
                v["tag"] = json!(t);
            }
            v
        }
        "ios" => {
            let mut aps = json!({"alert": {"title": title, "body": r.body}});
            match c.ios_sound.as_deref() {
                Some("") => {}
                Some(s) => aps["sound"] = json!(s),
                None => aps["sound"] = json!("default"),
            }
            if let Some(b) = c.ios_badge {
                aps["badge"] = json!(b);
            }
            if r.image.is_some() {
                aps["mutable-content"] = json!(1);
            }
            json!({"aps": aps, "dumbmail": {"t": tok, "url": url, "image": r.image, "actions": actions}})
        }
        _ => {
            let mut android = json!({"ttl": format!("{}s", c.ttl()), "priority": if c.urgency() == "high" { "HIGH" } else { "NORMAL" }});
            if let Some(t) = c.topic.as_deref().filter(|t| !t.is_empty()) {
                android["collapse_key"] = json!(t);
            }
            if let Some(ch) = c.android_channel_id.as_deref().filter(|s| !s.is_empty()) {
                android["notification"] = json!({"channel_id": ch});
            }
            let mut notification = json!({"title": title, "body": r.body});
            if let Some(i) = &r.image {
                notification["image"] = json!(i);
            }
            let mut data = json!({"dumbmail_t": tok, "dumbmail_url": url});
            if !actions.is_empty() {
                data["dumbmail_actions"] = json!(serde_json::to_string(&actions).unwrap_or_default());
            }
            json!({"notification": notification, "data": data, "android": android})
        }
    }
}

/// Hand one claimed delivery to its device's push service and record what happened. `retry`: this
/// is a retry job's attempt (already claimed by it). Returns the error class when it didn't go.
async fn hand_off(state: &AppState, row: &Row, delivery_id: Uuid, kind: &str, payload: &Value, c: &PushContent, retry: bool) -> AppResult<Option<String>> {
    // The last check, as a conditional write right before the provider call: the device is still
    // active and the person hasn't opted out since the claim. A replay of a first attempt finds
    // attempts > 0 and sends nothing.
    let device: Option<(Uuid, String, Option<String>, Option<String>)> = sqlx::query_as(
        "UPDATE push_deliveries pd SET attempts = pd.attempts + 1, claimed_at = now()
         FROM push_devices d, messages m
         WHERE pd.id = $1 AND pd.status = 'sending' AND ($2 OR pd.attempts = 0) AND d.id = pd.device_id AND d.status = 'active' AND m.id = pd.message_id
           AND (m.is_test OR NOT EXISTS (SELECT 1 FROM push_opt_outs o WHERE o.profile_id = m.profile_id))
         RETURNING d.id, d.address, d.p256dh, d.auth_secret",
    )
    .bind(delivery_id)
    .bind(retry)
    .fetch_optional(&state.db)
    .await?;
    let Some((device_id, address, p256dh, auth)) = device else {
        sqlx::query("UPDATE push_deliveries SET status = 'skipped', error = 'device or consent changed', payload = NULL WHERE id = $1 AND status = 'sending'").bind(delivery_id).execute(&state.db).await?;
        return Ok(Some("device or consent changed".into()));
    };
    let result = match kind {
        "web" => {
            let bytes = serde_json::to_vec(payload)?;
            let target = providers::WebTarget { endpoint: &address, p256dh: p256dh.as_deref().unwrap_or(""), auth: auth.as_deref().unwrap_or("") };
            providers::send_web(state, row, &target, &bytes, &providers::Options { ttl: c.ttl(), urgency: c.urgency(), topic: c.topic.as_deref().filter(|t| !t.is_empty()) }).await
        }
        "ios" => {
            let m = providers::ApnsMessage {
                device_token: &address,
                delivery_id,
                payload,
                ttl: c.ttl(),
                high: matches!(c.urgency(), "normal" | "high"),
                collapse_id: c.topic.as_deref().filter(|t| !t.is_empty()),
            };
            providers::send_apns(state, row, &m).await
        }
        _ => {
            let mut msg = payload.clone();
            msg["token"] = json!(address);
            providers::send_fcm(state, row, &msg).await
        }
    };
    // Kept with a waiting retry: the payload and the options that shape the hand-off.
    let mut stored = payload.clone();
    stored["_opts"] = json!({"ttl_seconds": c.ttl_seconds, "urgency": c.urgency, "topic": c.topic});
    record(state, row, delivery_id, device_id, &address, kind, &stored, result).await
}

#[allow(clippy::too_many_arguments)]
async fn record(state: &AppState, row: &Row, delivery_id: Uuid, device_id: Uuid, address: &str, kind: &str, payload: &Value, result: Sent) -> AppResult<Option<String>> {
    match result {
        Sent::Ok(provider_id) => {
            sqlx::query("UPDATE push_deliveries SET status = 'sent', sent_at = now(), provider_id = $2, error = NULL, payload = NULL WHERE id = $1 AND status = 'sending'")
                .bind(delivery_id)
                .bind(provider_id)
                .execute(&state.db)
                .await?;
            sqlx::query("UPDATE push_devices SET last_sent_at = now(), failures = 0, last_error = NULL WHERE id = $1").bind(device_id).execute(&state.db).await?;
            Ok(None)
        }
        Sent::Gone(class) => {
            sqlx::query("UPDATE push_deliveries SET status = 'gone', error = $2, payload = NULL WHERE id = $1 AND status = 'sending'").bind(delivery_id).bind(&class).execute(&state.db).await?;
            // Only the device we sent to: if it re-registered with new keys meanwhile, it stays.
            sqlx::query("DELETE FROM push_devices WHERE id = $1 AND address = $2").bind(device_id).bind(address).execute(&state.db).await?;
            Ok(Some(class))
        }
        Sent::Retry(class, after) => {
            let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM push_deliveries WHERE id = $1").bind(delivery_id).fetch_one(&state.db).await?;
            match RETRY_WAITS.get((attempts - 1).max(0) as usize) {
                Some(wait) => {
                    let wait = (*wait).max(after.map_or(0, |d| d.as_secs() as i64)).min(3600);
                    let at: DateTime<Utc> = Utc::now() + chrono::Duration::seconds(wait);
                    let mut tx = state.db.begin().await?;
                    let n = sqlx::query("UPDATE push_deliveries SET status = 'retry', error = $2, next_attempt_at = $3, payload = $4 WHERE id = $1 AND status = 'sending'")
                        .bind(delivery_id)
                        .bind(&class)
                        .bind(at)
                        .bind(payload)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
                    if n > 0 {
                        crate::jobs::enqueue_background(&mut *tx, "push_retry", json!({"delivery_id": delivery_id}), Some(at)).await?;
                    }
                    tx.commit().await?;
                    Ok(Some(class))
                }
                None => fail_device(state, delivery_id, device_id, &class).await,
            }
        }
        Sent::Failed(class) => fail_device(state, delivery_id, device_id, &class).await,
        Sent::Credentials(class) => {
            sqlx::query("UPDATE push_deliveries SET status = 'failed', error = $2, payload = NULL WHERE id = $1 AND status = 'sending'").bind(delivery_id).bind(&class).execute(&state.db).await?;
            let col = match kind {
                "ios" => Some("apns_error"),
                "android" => Some("fcm_error"),
                _ => None,
            };
            if let Some(col) = col {
                sqlx::query(&format!("UPDATE push_settings SET {col} = $2 WHERE account_id = $1")).bind(row.account_id).bind(&class).execute(&state.db).await?;
            }
            Ok(Some(class))
        }
    }
}

async fn fail_device(state: &AppState, delivery_id: Uuid, device_id: Uuid, class: &str) -> AppResult<Option<String>> {
    sqlx::query("UPDATE push_deliveries SET status = 'failed', error = $2, payload = NULL WHERE id = $1 AND status = 'sending'").bind(delivery_id).bind(class).execute(&state.db).await?;
    sqlx::query(
        "UPDATE push_devices SET failures = failures + 1, last_error = $2,
                status = CASE WHEN failures + 1 >= $3 THEN 'disabled' ELSE status END,
                disabled_at = CASE WHEN failures + 1 >= $3 THEN now() ELSE disabled_at END
         WHERE id = $1",
    )
    .bind(device_id)
    .bind(class)
    .bind(MAX_FAILURES)
    .execute(&state.db)
    .await?;
    Ok(Some(class.to_string()))
}

/// Settle the message from its deliveries. The first device that accepted it marks it sent, and
/// that one write charges it and records "Received Push". With retries still waiting it counts as
/// sent (its campaign goes on); when nothing went out and nothing waits, it failed.
async fn finish(state: &AppState, account_id: Uuid, message_id: Uuid, is_test: bool, error: Option<&str>) -> AppResult<Outcome> {
    let (sent, waiting): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER (WHERE status = 'sent'), count(*) FILTER (WHERE status IN ('retry', 'sending')) FROM push_deliveries WHERE message_id = $1")
        .bind(message_id)
        .fetch_one(&state.db)
        .await?;
    if sent > 0 {
        let first: Option<(Uuid, Option<Uuid>, Option<Uuid>, Option<String>)> = sqlx::query_as(
            "UPDATE messages SET status = CASE WHEN status = 'delivered' THEN status ELSE 'sent' END, sent_at = now(), error = NULL
             WHERE id = $1 AND sent_at IS NULL AND status IN ('sending', 'sent', 'delivered') RETURNING profile_id, campaign_id, flow_id, subject",
        )
        .bind(message_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some((profile_id, campaign_id, flow_id, subject)) = first {
            let cost = crate::billing::charge_message(state, account_id, "push", 1, false, false).await?;
            if cost > 0 {
                sqlx::query("UPDATE messages SET cost_micros = $2 WHERE id = $1").bind(message_id).bind(cost).execute(&state.db).await?;
            }
            if !is_test {
                record_event(
                    &state.db,
                    NewEvent {
                        account_id,
                        profile_id,
                        metric: "Received Push",
                        properties: json!({"subject": subject, "campaign_id": campaign_id, "flow_id": flow_id, "message_id": message_id}),
                        value: None,
                        occurred_at: None,
                        unique_id: Some(message_id.to_string()),
                        message_id: Some(message_id),
                    },
                )
                .await?;
            }
        }
        return Ok(Outcome::Sent(message_id));
    }
    if waiting > 0 {
        sqlx::query("UPDATE messages SET status = 'sent', error = 'waiting to retry' WHERE id = $1 AND status = 'sending'").bind(message_id).execute(&state.db).await?;
        return Ok(Outcome::Sent(message_id));
    }
    let why = error.unwrap_or("not delivered to any device").to_string();
    let failed: Option<Uuid> = sqlx::query_scalar("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1 AND sent_at IS NULL AND status IN ('sending', 'sent') RETURNING id")
        .bind(message_id)
        .bind(&why)
        .fetch_optional(&state.db)
        .await?;
    if failed.is_some() && !is_test {
        crate::features::messages_unsent(&state.db, account_id, &[message_id]).await;
    }
    Ok(Outcome::Failed(why))
}

/// Background job: try a waiting delivery again (see [`RETRY_WAITS`]). Claimed with a conditional
/// write, so a duplicate job does nothing; stops if the campaign was cancelled or the person opted out.
pub async fn retry(state: &AppState, delivery_id: Uuid) -> anyhow::Result<()> {
    let claimed: Option<(Uuid, Uuid, String, Value)> = sqlx::query_as(
        "UPDATE push_deliveries pd SET status = 'sending'
         FROM messages m
         WHERE pd.id = $1 AND pd.status = 'retry' AND m.id = pd.message_id
           AND (m.campaign_id IS NULL OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = m.campaign_id AND c.status IN ('sending', 'testing', 'sent')))
         RETURNING pd.account_id, pd.message_id, pd.kind, pd.payload",
    )
    .bind(delivery_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((account_id, message_id, kind, payload)) = claimed else {
        // Cancelled meanwhile (or already handled): it won't go.
        let skipped: Option<(Uuid, Uuid)> = sqlx::query_as("UPDATE push_deliveries SET status = 'skipped', error = 'campaign stopped', payload = NULL WHERE id = $1 AND status = 'retry' RETURNING account_id, message_id")
            .bind(delivery_id)
            .fetch_optional(&state.db)
            .await?;
        if let Some((account_id, message_id)) = skipped {
            finish(state, account_id, message_id, false, Some("campaign stopped")).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        }
        return Ok(());
    };
    let row = settings::load(&state.db, account_id).await?.ok_or_else(|| anyhow::anyhow!("push settings gone"))?;
    // The options that shape the hand-off (time to live, urgency, topic) travel with the payload.
    let opts: PushContent = serde_json::from_value(payload.get("_opts").cloned().unwrap_or(Value::Null)).unwrap_or_default();
    let mut body = payload.clone();
    if let Some(o) = body.as_object_mut() {
        o.remove("_opts");
    }
    let is_test: bool = sqlx::query_scalar("SELECT is_test FROM messages WHERE id = $1").bind(message_id).fetch_one(&state.db).await?;
    let err = hand_off(state, &row, delivery_id, &kind, &body, &opts, true).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    finish(state, account_id, message_id, is_test, err.as_deref()).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    Ok(())
}

/// Scheduler: deliveries a crashed worker left `sending` are failed, never resent (at most once),
/// and their messages settled; push messages of flows left `sending` the same way.
pub async fn sweep(state: &AppState) -> anyhow::Result<()> {
    let stuck: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE push_deliveries SET status = 'failed', error = 'send interrupted; not retried to avoid a duplicate', payload = NULL
         WHERE status = 'sending' AND claimed_at < now() - interval '10 minutes' RETURNING account_id, message_id",
    )
    .fetch_all(&state.db)
    .await?;
    // A retry whose job was lost (its time long past) is given up the same way.
    let lost: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE push_deliveries SET status = 'failed', error = 'retry never ran', payload = NULL
         WHERE status = 'retry' AND next_attempt_at < now() - interval '1 hour' RETURNING account_id, message_id",
    )
    .fetch_all(&state.db)
    .await?;
    let stuck: Vec<(Uuid, Uuid)> = stuck.into_iter().chain(lost).collect();
    let mut seen = std::collections::HashSet::new();
    for (account_id, message_id) in stuck {
        if seen.insert(message_id) {
            let is_test: bool = sqlx::query_scalar("SELECT is_test FROM messages WHERE id = $1").bind(message_id).fetch_optional(&state.db).await?.unwrap_or(true);
            finish(state, account_id, message_id, is_test, Some("send interrupted")).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        }
    }
    // A flow's push message claimed but with no deliveries (a crash between claim and hand-off).
    let orphans: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE messages m SET status = 'failed', error = 'send interrupted; not retried to avoid a duplicate'
         WHERE m.channel = 'push' AND m.status = 'sending' AND m.campaign_id IS NULL AND m.claimed_at < now() - interval '10 minutes'
           AND NOT EXISTS (SELECT 1 FROM push_deliveries d WHERE d.message_id = m.id AND d.status IN ('sending', 'retry', 'sent'))
         RETURNING m.account_id, m.id",
    )
    .fetch_all(&state.db)
    .await?;
    crate::features::messages_unsent_rows(&state.db, &orphans).await;
    // Bounded upkeep: registration counters older than a day, devices never linked in 180 days.
    sqlx::query("DELETE FROM push_registration_counts WHERE hour < now() - interval '1 day'").execute(&state.db).await?;
    sqlx::query("DELETE FROM push_devices WHERE id IN (SELECT id FROM push_devices WHERE profile_id IS NULL AND test_user_id IS NULL AND last_seen_at < now() - interval '180 days' LIMIT 1000)")
        .execute(&state.db)
        .await?;
    Ok(())
}

/// Send a test of push `content` to a person's devices (`campaigns::send_test` for push campaigns).
/// `member`: the team member with that email, whose own test browsers get it too.
pub async fn test_to_profile(state: &AppState, account: &Account, profile: &Profile, content: &Value, campaign_id: Option<Uuid>, member: Option<Uuid>) -> AppResult<Outcome> {
    let spec = test_spec(account, profile, content, campaign_id);
    run(state, &spec, Target::Profile(member)).await
}

/// Send a test to a team member's own test devices (the dashboard's "Send me a test").
pub async fn test_to_member(state: &AppState, account: &Account, profile: &Profile, user_id: Uuid, content: &Value) -> AppResult<Outcome> {
    let spec = test_spec(account, profile, content, None);
    run(state, &spec, Target::Member(user_id)).await
}

/// Send a test to one device.
pub async fn test_to_device(state: &AppState, account: &Account, profile: &Profile, device_id: Uuid, content: &Value) -> AppResult<Outcome> {
    let spec = test_spec(account, profile, content, None);
    run(state, &spec, Target::Device(&device_id)).await
}

fn test_spec<'a>(account: &'a Account, profile: &'a Profile, content: &'a Value, campaign_id: Option<Uuid>) -> SendSpec<'a> {
    SendSpec {
        account,
        profile,
        channel: "push",
        content,
        campaign_id,
        flow_id: None,
        flow_node_id: None,
        event: None,
        is_test: true,
        message_id: None,
        to_override: None,
    }
}

/// Test sends are limited like campaign tests: 50 an hour per account.
pub async fn check_test_rate(state: &AppState, account_id: Uuid) -> AppResult<()> {
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE account_id = $1 AND is_test AND created_at > now() - interval '1 hour'").bind(account_id).fetch_one(&state.db).await?;
    if recent >= 50 {
        return Err(AppError::Busy("test sends are limited to 50 an hour".into()));
    }
    Ok(())
}
