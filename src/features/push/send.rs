//! Push as a channel: sending one push message (a `messages` row, channel `push`) to a person's
//! devices, tests, retries and upkeep.
//!
//! A send happens in three writes:
//!
//! 1. **The message claim** (one transaction): the message may still go (queued, the profile
//!    exists, the campaign still sending), and its deliveries are fixed: one per device
//!    registration, each remembering the registration's version and owner and carrying the payload
//!    it will send (so a retry sends the same thing).
//! 2. **The final claim**, right before every provider call, first attempts and retries alike (one
//!    transaction holding the message, the devices and their owners): the campaign or flow is still
//!    live, quiet hours and smart sending (not counting this message) still allow it, and, for each
//!    device, the same owner and registration version as when the message was made, the platform
//!    still enabled, notifications still permitted, and the owner not opted out of push. Only what
//!    passes is marked `sending` and handed over; the rest is skipped with the reason.
//! 3. **The results** (one transaction, batched): applied only to the registration version that was
//!    sent to. The first device that accepted marks the message sent, and that same write charges it
//!    and queues the "Received Push" event (recorded once, on the message id).
//!
//! Every provider call goes through [`super::limit`]. A request with no answer is `unconfirmed`, never
//! sent again; only a certain refusal (connection refused, 429, 5xx) is retried, in the background lane.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

use super::content::{self, PushContent, Rendered};
use super::providers::{self, Sent};
use super::settings::{self, Row};
use crate::error::{AppError, AppResult};
use crate::models::{Account, Profile};
use crate::sending::{Outcome, SendSpec};
use crate::state::AppState;

/// Devices per person a message goes to (their most recently seen).
pub const MAX_DEVICES_PER_MESSAGE: i64 = 10;
/// Failed hand-offs in a row before a device is turned off (a gone device is removed at once).
pub const MAX_FAILURES: i32 = 5;
/// Waits before each retry of a delivery the push service didn't take; then it's given up.
const RETRY_WAITS: [i64; 3] = [60, 300, 1800];
/// How long a delivery's report and tap token stays valid (the longest time to live is 28 days).
pub const TOKEN_DAYS: i64 = 35;

macro_rules! sendable {
    () => {
        "d.status = 'active' AND d.permission IN ('granted', 'provisional')
         AND ((d.kind = 'web' AND ps.web_enabled AND ps.vapid_private_enc IS NOT NULL)
           OR (d.kind = 'ios' AND ps.apns_key_enc IS NOT NULL AND ps.apns_bundle_id IS NOT NULL)
           OR (d.kind = 'android' AND ps.fcm_credentials_enc IS NOT NULL))"
    };
}

/// SQL true for a device `d` (with its account's push settings `ps`) that may be sent to now:
/// active, notifications permitted, and its platform enabled.
pub const DEVICE_SENDABLE_SQL: &str = sendable!();

/// SQL true when profile `p` can get push now: a sendable device and no push opt-out.
pub const REACHABLE_SQL: &str = concat!(
    "(NOT EXISTS (SELECT 1 FROM push_opt_outs po WHERE po.profile_id = p.id) AND EXISTS (SELECT 1 FROM push_devices d JOIN push_settings ps ON ps.account_id = d.account_id WHERE d.profile_id = p.id AND ",
    sendable!(),
    "))"
);

/// [`REACHABLE_SQL`], counting only devices on `platforms` (a campaign's targets) when given.
pub fn push_reachable(qb: &mut QueryBuilder<'_, Postgres>, platforms: Option<&[String]>) {
    match platforms {
        None => {
            qb.push(REACHABLE_SQL);
        }
        Some(p) => {
            qb.push("(NOT EXISTS (SELECT 1 FROM push_opt_outs po WHERE po.profile_id = p.id) AND EXISTS (SELECT 1 FROM push_devices d JOIN push_settings ps ON ps.account_id = d.account_id WHERE d.profile_id = p.id AND ");
            qb.push(DEVICE_SENDABLE_SQL).push(" AND d.kind = ANY(").push_bind(p.to_vec()).push(")))");
        }
    }
}

/// People in the account a push would reach now.
pub async fn reachable_count(db: &sqlx::PgPool, account_id: Uuid) -> sqlx::Result<i64> {
    sqlx::query_scalar(&format!("SELECT count(*) FROM profiles p WHERE p.account_id = $1 AND {REACHABLE_SQL}")).bind(account_id).fetch_one(db).await
}

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

/// The channel's entry point from `sending::send` (campaigns and flows). A flow passes the message
/// id it stored with its run, so a re-run of the step finds the message instead of making another.
pub async fn send(state: &AppState, spec: &SendSpec<'_>) -> AppResult<Outcome> {
    run(state, spec, Target::Profile(None)).await
}

async fn run(state: &AppState, spec: &SendSpec<'_>, target: Target<'_>) -> AppResult<Outcome> {
    let account = spec.account;
    let p = spec.profile;
    if account.closed_at.is_some() {
        if spec.is_test {
            return Err(AppError::Conflict("this account is closed, so it can't send".into()));
        }
        return Ok(Outcome::Skipped("account closed"));
    }
    let c = content::parse(spec.content)?;
    let Some(row) = settings::load(&state.db, account.id).await? else { return Ok(Outcome::Skipped("push isn't set up")) };
    let ready = row.ready_kinds();
    let kinds: Vec<String> = c.platforms().into_iter().filter(|k| ready.contains(&k.as_str())).collect();
    if kinds.is_empty() {
        if spec.is_test {
            return Err(AppError::bad("push isn't set up for the devices this notification goes to"));
        }
        return Ok(Outcome::Skipped("push isn't set up"));
    }
    // A message already handled (a flow step run again) is never sent again.
    if let Some(mid) = spec.message_id {
        let status: Option<String> = sqlx::query_scalar("SELECT status FROM messages WHERE id = $1").bind(mid).fetch_optional(&state.db).await?;
        if status.as_deref().is_some_and(|s| !matches!(s, "queued" | "sending")) {
            return Ok(Outcome::Skipped("already handled"));
        }
    }
    let who = target.who(p.id);
    // The devices, as they are now (the final claim checks them again right before the hand-off).
    type Dev = (Uuid, String, i64, Option<Uuid>, Option<Uuid>);
    let devices: Vec<Dev> = sqlx::query_as(&format!(
        "SELECT d.id, d.kind, d.version, d.profile_id, d.test_user_id FROM push_devices d JOIN push_settings ps ON ps.account_id = d.account_id
         WHERE d.account_id = $1 AND d.kind = ANY($2) AND {WHO} AND {DEVICE_SENDABLE_SQL}
           AND (d.profile_id IS NULL OR NOT EXISTS (SELECT 1 FROM push_opt_outs o WHERE o.profile_id = d.profile_id))
         ORDER BY d.last_seen_at DESC LIMIT $6"
    ))
    .bind(account.id)
    .bind(&kinds)
    .bind(who.0)
    .bind(who.1)
    .bind(who.2)
    .bind(MAX_DEVICES_PER_MESSAGE)
    .fetch_all(&state.db)
    .await?;
    if devices.is_empty() {
        let opted_out: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM push_opt_outs WHERE profile_id = $1)").bind(p.id).fetch_one(&state.db).await?;
        if spec.is_test {
            return Err(AppError::bad(if opted_out && matches!(target, Target::Profile(_)) {
                "this person turned push notifications off; pick someone else"
            } else {
                "no device that allowed notifications to send this to"
            }));
        }
        return Ok(Outcome::Skipped(if opted_out { "push turned off" } else { "no push device" }));
    }
    // Quiet hours in their own zone (the account's texting window). Checked again at the hand-off.
    if !spec.is_test {
        if let Some(until) = quiet_until(state, account, &p.properties).await? {
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
        return Ok(Outcome::Skipped("insufficient credit"));
    }

    let to = format!("{} device{}", devices.len(), if devices.len() == 1 { "" } else { "s" });
    sqlx::query(
        "INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, flow_id, flow_node_id, to_address, subject, is_test)
         VALUES ($1, $2, $3, 'push', $4, $5, $6, $7, $8, $9)
         ON CONFLICT (id) DO UPDATE SET subject = EXCLUDED.subject, to_address = EXCLUDED.to_address WHERE messages.status IN ('queued', 'sending')",
    )
    .bind(message_id)
    .bind(account.id)
    .bind(p.id)
    .bind(spec.campaign_id)
    .bind(spec.flow_id)
    .bind(spec.flow_node_id)
    .bind(&to)
    .bind(&subject)
    .bind(spec.is_test)
    .execute(&state.db)
    .await?;

    crate::sending::throttle("push", f64::from(state.cfg.send_rate_per_sec.max(1))).await;

    // The message claim: still allowed, and its deliveries fixed, in one transaction.
    let mut tx = state.db.begin().await?;
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE messages m SET status = 'sending', claimed_at = COALESCE(m.claimed_at, now())
         WHERE m.id = $1 AND m.status IN ('queued', 'sending')
           AND EXISTS (SELECT 1 FROM profiles p WHERE p.id = m.profile_id)
           AND ($2 OR m.campaign_id IS NULL OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = m.campaign_id AND c.status = 'sending'))
         RETURNING m.id",
    )
    .bind(message_id)
    .bind(spec.is_test)
    .fetch_optional(&mut *tx)
    .await?;
    if claimed.is_none() {
        let campaign: Option<String> = sqlx::query_scalar("SELECT c.status FROM messages m JOIN campaigns c ON c.id = m.campaign_id WHERE m.id = $1").bind(message_id).fetch_optional(&mut *tx).await?;
        let why = match campaign.as_deref() {
            Some("cancelled") => "campaign cancelled",
            Some(s) if s != "sending" => "campaign stopped",
            _ => "no longer queued",
        };
        mark_skipped(&mut tx, state, account.id, message_id, why, spec.is_test).await?;
        tx.commit().await?;
        return Ok(Outcome::Skipped(why));
    }
    let opts = json!({"ttl": c.ttl(), "urgency": c.urgency(), "topic": c.topic.as_deref().filter(|t| !t.is_empty()), "high": matches!(c.urgency(), "normal" | "high")});
    let (mut ids, mut device_ids, mut kinds_v, mut versions, mut owners, mut members, mut payloads) = (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
    for (did, kind, version, owner, member) in &devices {
        let id = Uuid::now_v7();
        let mut payload = build_payload(state, account, &c, &rendered, &title, id, kind);
        payload["_opts"] = opts.clone();
        ids.push(id);
        device_ids.push(*did);
        kinds_v.push(kind.clone());
        versions.push(*version);
        owners.push(*owner);
        members.push(*member);
        payloads.push(payload);
    }
    sqlx::query(
        "INSERT INTO push_deliveries (id, message_id, account_id, device_id, kind, device_version, owner_profile_id, owner_test_user_id, payload)
         SELECT x.id, $1, $2, x.device_id, x.kind, x.version, x.owner, x.member, x.payload
         FROM unnest($3::uuid[], $4::uuid[], $5::text[], $6::bigint[], $7::uuid[], $8::uuid[], $9::jsonb[]) AS x(id, device_id, kind, version, owner, member, payload)
         ON CONFLICT (message_id, device_id) DO NOTHING",
    )
    .bind(message_id)
    .bind(account.id)
    .bind(&ids)
    .bind(&device_ids)
    .bind(&kinds_v)
    .bind(&versions)
    .bind(&owners)
    .bind(&members)
    .bind(&payloads)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    hand_off(state, message_id, Attempt::First).await
}

/// When quiet hours end for someone with these profile properties, if they're in them now.
async fn quiet_until(state: &AppState, account: &Account, props: &Value) -> AppResult<Option<DateTime<Utc>>> {
    let ((start, end), _) = crate::features::sendtime::settings::sms_settings(&state.db, account.id).await?;
    let zones = crate::features::sendtime::settings::sms_zones(props, None, crate::sending::account_tz(account));
    Ok(crate::features::sendtime::settings::decide(&zones, Utc::now(), start, end, false).wait)
}

async fn mark_skipped(tx: &mut PgConnection, state: &AppState, account_id: Uuid, message_id: Uuid, why: &str, is_test: bool) -> AppResult<()> {
    let n = sqlx::query("UPDATE messages SET status = 'skipped', error = $2 WHERE id = $1 AND status IN ('queued', 'sending', 'retrying')")
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

/// A signed token naming one delivery until it expires (in the payload, for "delivered" and
/// "opened" reports and the tap link).
pub fn delivery_token(state: &AppState, delivery_id: Uuid) -> String {
    let exp = Utc::now().timestamp() + TOKEN_DAYS * 86_400;
    crate::auth::sign(&state.cfg.secret_key, &format!("pd{}{exp:x}", delivery_id.simple()))
}

/// The delivery a token names, if it's genuine and not expired.
pub fn delivery_from_token(state: &AppState, token: &str) -> Option<Uuid> {
    if token.len() > 200 {
        return None;
    }
    let payload = crate::auth::verify(&state.cfg.secret_key, token)?;
    let rest = payload.strip_prefix("pd")?;
    if rest.len() <= 32 {
        return None;
    }
    let (id, exp) = rest.split_at(32);
    if i64::from_str_radix(exp, 16).ok()? < Utc::now().timestamp() {
        return None;
    }
    Uuid::parse_str(id).ok()
}

/// A tracked link for a tap: `/p/c/{token}?u=...&s=...`, signed so it only goes where we sent it.
pub fn tracked_url(state: &AppState, account: &Account, delivery_id: Uuid, token: &str, url: &str) -> String {
    let base = crate::domains::link_base(state, account);
    let mut u = url::Url::parse(&format!("{base}/p/c/{token}")).expect("link base is a URL");
    u.query_pairs_mut().append_pair("u", url).append_pair("s", &crate::auth::link_sig(&state.cfg.secret_key, delivery_id, url));
    u.to_string()
}

/// What one device is sent, by kind. Web: the JSON DumbMail's service worker reads (marked `dm`).
/// iOS: the APNs payload. Android: the FCM message without its token.
fn build_payload(state: &AppState, account: &Account, c: &PushContent, r: &Rendered, title: &str, delivery_id: Uuid, kind: &str) -> Value {
    let tok = delivery_token(state, delivery_id);
    let url = tracked_url(state, account, delivery_id, &tok, &r.url);
    let actions: Vec<Value> = r.actions.iter().map(|a| json!({"t": a.title, "u": tracked_url(state, account, delivery_id, &tok, &a.url)})).collect();
    match kind {
        "web" => {
            let mut v = json!({"dm": 1, "t": title, "b": r.body, "u": url, "r": tok});
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

/// Which hand-off this is: the send itself (its campaign must still be sending), or a later one
/// (a retry, or upkeep picking up deliveries a crash left queued).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Attempt {
    First,
    Later,
}

/// Test hooks: hold an account's hand-offs at a point, so a test can change the world exactly there.
/// Never set outside tests.
#[doc(hidden)]
pub mod hook {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use once_cell::sync::Lazy;
    use tokio::sync::Semaphore;
    use uuid::Uuid;

    /// Where a hand-off can be held.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub enum Point {
        /// With the provider slot and credentials in hand, right before the final claim.
        BeforeClaim,
        /// Inside the final claim, holding the person, the device, the message and the delivery.
        InClaim,
        /// Applying results, right after the devices are written and before the message.
        InApply,
    }

    pub struct Pause {
        arrived: Semaphore,
        go: Semaphore,
    }

    static PAUSES: Lazy<Mutex<HashMap<(Uuid, Point), Arc<Pause>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

    impl Pause {
        /// Hold this account's hand-offs right before their final claim until [`Pause::release`].
        pub fn hold(account_id: Uuid) -> Arc<Pause> {
            Self::hold_at(account_id, Point::BeforeClaim)
        }
        pub fn hold_at(account_id: Uuid, point: Point) -> Arc<Pause> {
            let p = Arc::new(Pause { arrived: Semaphore::new(0), go: Semaphore::new(0) });
            PAUSES.lock().unwrap_or_else(|e| e.into_inner()).insert((account_id, point), p.clone());
            p
        }
        pub async fn arrived(&self) {
            if let Ok(permit) = self.arrived.acquire().await {
                permit.forget();
            }
        }
        pub fn release(&self, account_id: Uuid) {
            PAUSES.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, p| !std::ptr::eq(Arc::as_ptr(p), self));
            let _ = account_id;
            self.go.add_permits(1000);
        }
    }

    static FAIL_EVENTS: Lazy<Mutex<HashMap<Uuid, usize>>> = Lazy::new(|| Mutex::new(HashMap::new()));

    /// Make an account's next `n` push event recordings fail, as if the database had refused them.
    pub fn fail_events(account_id: Uuid, n: usize) {
        FAIL_EVENTS.lock().unwrap_or_else(|e| e.into_inner()).insert(account_id, n);
    }

    pub(super) fn event_fails(account_id: Uuid) -> bool {
        let mut m = FAIL_EVENTS.lock().unwrap_or_else(|e| e.into_inner());
        match m.get_mut(&account_id) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        }
    }

    pub(super) async fn wait(account_id: Uuid, point: Point) {
        let p = PAUSES.lock().unwrap_or_else(|e| e.into_inner()).get(&(account_id, point)).cloned();
        if let Some(p) = p {
            p.arrived.add_permits(1);
            if let Ok(permit) = p.go.acquire().await {
                permit.forget();
            }
        }
    }
}

/// One delivery claimed for its hand-off.
struct Claimed {
    id: Uuid,
    kind: String,
    payload: Value,
    attempts: i32,
    device_id: Uuid,
    version: i64,
    address: String,
    p256dh: Option<String>,
    auth: Option<String>,
    last_seen_at: DateTime<Utc>,
    /// iOS: the device said which APNs environment it belongs to.
    own_environment: bool,
}

/// What happened to one due delivery.
enum Turn {
    /// Claimed and handed to the provider.
    Sent(Claimed, Sent),
    /// Its credentials couldn't be had (nothing was claimed or sent): (delivery, platform, why).
    NoCredentials(Uuid, String, Sent),
    /// Skipped or not due at its final claim.
    Nothing,
}

/// Deliveries of a message due for a hand-off.
const DUE: &str = "(pd.status = 'queued' OR (pd.status = 'retry' AND pd.next_attempt_at <= now() + interval '5 seconds'))";

/// The hand-off for a message's due deliveries: for each, a provider slot and its credentials
/// first, then the final claim, then the provider call at once; then the results.
pub async fn hand_off(state: &AppState, message_id: Uuid, attempt: Attempt) -> AppResult<Outcome> {
    let meta: Option<(Uuid, bool)> = sqlx::query_as("SELECT account_id, is_test FROM messages WHERE id = $1").bind(message_id).fetch_optional(&state.db).await?;
    let Some((account_id, is_test)) = meta else { return Ok(Outcome::Skipped("message deleted")) };
    let Some(row) = settings::load(&state.db, account_id).await? else { return Ok(Outcome::Skipped("push isn't set up")) };
    let due: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(&format!(
        "SELECT pd.id, pd.kind, d.apns_environment FROM push_deliveries pd LEFT JOIN push_devices d ON d.id = pd.device_id WHERE pd.message_id = $1 AND {DUE}"
    ))
    .bind(message_id)
    .fetch_all(&state.db)
    .await?;
    let row_ref = &row;
    let turns = due.iter().map(|(id, kind, env)| async move {
        let row = row_ref;
        let _slot = super::limit::slot(account_id).await;
        let creds = match providers::prepare(state, row, kind, env.as_deref()).await {
            Ok(c) => c,
            Err(sent) => return Ok(Turn::NoCredentials(*id, kind.clone(), sent)),
        };
        hook::wait(account_id, hook::Point::BeforeClaim).await;
        let Some(c) = final_claim(state, message_id, *id, attempt).await? else { return Ok(Turn::Nothing) };
        let sent = call(state, row, &creds, &c).await;
        Ok::<_, AppError>(Turn::Sent(c, sent))
    });
    let mut results = Vec::with_capacity(due.len());
    let mut failure = None;
    for t in futures_util::future::join_all(turns).await {
        match t {
            Ok(t) => results.push(t),
            Err(e) => failure = Some(e),
        }
    }
    let out = apply(state, &row, message_id, is_test, results).await?;
    match failure {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// The provider call for one claimed delivery.
async fn call(state: &AppState, row: &Row, creds: &providers::Creds, c: &Claimed) -> Sent {
    let mut body = c.payload.clone();
    let opts = body.as_object_mut().and_then(|o| o.remove("_opts")).unwrap_or(Value::Null);
    let ttl = opts["ttl"].as_i64().unwrap_or(86_400);
    let topic = opts["topic"].as_str();
    match c.kind.as_str() {
        "web" => {
            let bytes = serde_json::to_vec(&body).unwrap_or_default();
            let target = providers::WebTarget { endpoint: &c.address, p256dh: c.p256dh.as_deref().unwrap_or(""), auth: c.auth.as_deref().unwrap_or("") };
            providers::send_web(state, row, &target, &bytes, &providers::Options { ttl, urgency: opts["urgency"].as_str().unwrap_or("normal"), topic }).await
        }
        "ios" => {
            let m = providers::ApnsMessage { device_token: &c.address, delivery_id: c.id, payload: &body, ttl, high: opts["high"].as_bool().unwrap_or(true), collapse_id: topic };
            providers::send_apns(creds, &m, c.own_environment).await
        }
        _ => {
            body["token"] = json!(c.address);
            providers::send_fcm(creds, &body).await
        }
    }
}

/// The final claim of one delivery, in one transaction taking its locks in the push module's one
/// order (profile, device, message, delivery, settings): the delivery is still due; the message may
/// still go; the account is open; the device has the same owner and registration version as when
/// the message was made, allows notifications and its platform is on; its owner hasn't opted out;
/// and (not for tests) the campaign or flow is live, it isn't the person's quiet hours, and smart
/// sending allows it. The person's row is held for update, so two sends to one person take turns
/// and can't both pass smart sending. Some: claimed, call the provider now.
async fn final_claim(state: &AppState, message_id: Uuid, delivery_id: Uuid, attempt: Attempt) -> AppResult<Option<Claimed>> {
    let mut tx = state.db.begin().await?;
    let owners: Option<(Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT m.profile_id, pd.device_id, d.profile_id FROM push_deliveries pd JOIN messages m ON m.id = pd.message_id LEFT JOIN push_devices d ON d.id = pd.device_id WHERE pd.id = $1",
    )
    .bind(delivery_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((recipient, device_id, device_owner)) = owners else { return Ok(None) };
    // 1. The people: the recipient and the device's owner, in id order.
    let mut people = vec![recipient];
    people.extend(device_owner.filter(|o| *o != recipient));
    people.sort();
    sqlx::query("SELECT 1 FROM profiles WHERE id = ANY($1) ORDER BY id FOR NO KEY UPDATE").bind(&people).execute(&mut *tx).await?;
    // 2. The device.
    type Dev = (Option<Uuid>, Option<Uuid>, i64, String, String, String, Option<String>, Option<String>, DateTime<Utc>, Option<String>);
    let dev: Option<Dev> = match device_id {
        Some(id) => sqlx::query_as("SELECT profile_id, test_user_id, version, status, permission, address, p256dh, auth_secret, last_seen_at, apns_environment FROM push_devices WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?,
        None => None,
    };
    // 3. The message.
    type M = (Uuid, Option<Uuid>, Option<Uuid>, bool, String);
    let m: Option<M> = sqlx::query_as("SELECT account_id, campaign_id, flow_id, is_test, status FROM messages WHERE id = $1 FOR UPDATE").bind(message_id).fetch_optional(&mut *tx).await?;
    let Some((account_id, campaign_id, flow_id, is_test, status)) = m else { return Ok(None) };
    // 4. The delivery.
    type D = (String, bool, i64, Option<Uuid>, Option<Uuid>, String, Value, i32);
    let d: Option<D> = sqlx::query_as(
        "SELECT status, (status = 'queued' OR (status = 'retry' AND next_attempt_at <= now() + interval '5 seconds')), device_version, owner_profile_id, owner_test_user_id, kind, payload, attempts
         FROM push_deliveries WHERE id = $1 FOR UPDATE",
    )
    .bind(delivery_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((_, due, version, owner_profile, owner_member, kind, payload, attempts)) = d else { return Ok(None) };
    // 5. The settings.
    let row: Option<Row> = sqlx::query_as("SELECT * FROM push_settings WHERE account_id = $1 FOR SHARE").bind(account_id).fetch_optional(&mut *tx).await?;
    hook::wait(account_id, hook::Point::InClaim).await;
    if !due {
        tx.commit().await?;
        return Ok(None);
    }
    let open = match attempt {
        Attempt::First => status == "sending",
        Attempt::Later => matches!(status.as_str(), "sending" | "retrying" | "sent" | "delivered" | "unconfirmed"),
    };
    // With the account's sending review (see features::screening): a blocked account sends no push.
    let (closed, standing): (bool, String) =
        sqlx::query_as(&format!("SELECT a.closed_at IS NOT NULL, {} FROM accounts a WHERE a.id = $1", crate::features::screening::standing_sql(state, "a"))).bind(account_id).fetch_one(&mut *tx).await?;
    let platform_on = row.as_ref().is_some_and(|r| r.ready_kinds().contains(&kind.as_str()));
    let device_ok = match &dev {
        Some((owner, member, v, dstatus, permission, ..)) => {
            *v == version && *owner == owner_profile && *member == owner_member && dstatus == "active" && matches!(permission.as_str(), "granted" | "provisional") && platform_on
        }
        None => false,
    };
    let opted_out: bool = match dev.as_ref().and_then(|d| d.0) {
        Some(owner) => sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM push_opt_outs WHERE profile_id = $1)").bind(owner).fetch_one(&mut *tx).await?,
        None => false,
    };
    let stop: Option<&'static str> = if !open {
        Some("no longer queued")
    } else if closed {
        Some("account closed")
    } else if standing == "blocked" {
        Some(crate::features::screening::BLOCKED)
    } else if !device_ok || opted_out {
        Some("the device changed, or push is off for it")
    } else if is_test {
        None
    } else {
        message_stop(&mut tx, account_id, recipient, message_id, campaign_id, flow_id, attempt).await?
    };
    if let Some(why) = stop {
        sqlx::query("UPDATE push_deliveries SET status = 'skipped', error = $2, next_attempt_at = NULL WHERE id = $1").bind(delivery_id).bind(why).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(None);
    }
    sqlx::query("UPDATE push_deliveries SET status = 'sending', attempts = attempts + 1, claimed_at = now(), next_attempt_at = NULL WHERE id = $1").bind(delivery_id).execute(&mut *tx).await?;
    tx.commit().await?;
    let (_, _, version, _, _, address, p256dh, auth, last_seen_at, env) = dev.expect("checked above");
    Ok(Some(Claimed {
        id: delivery_id,
        kind,
        payload,
        attempts: attempts + 1,
        device_id: device_id.expect("checked above"),
        version,
        address,
        p256dh,
        auth,
        last_seen_at,
        own_environment: env.is_some(),
    }))
}

/// The message-level rules, read inside the final claim (under the person's lock): the campaign or
/// flow still live, quiet hours, and smart sending (not counting this message; counting other push
/// messages already claimed for them, not only those that went out). None: it may go.
async fn message_stop(tx: &mut PgConnection, account_id: Uuid, profile_id: Uuid, message_id: Uuid, campaign_id: Option<Uuid>, flow_id: Option<Uuid>, attempt: Attempt) -> AppResult<Option<&'static str>> {
    let mut smart_hours: Option<i64> = None;
    if let Some(cid) = campaign_id {
        let c: Option<(String, Option<Value>)> = sqlx::query_as("SELECT status, send_options FROM campaigns WHERE id = $1").bind(cid).fetch_optional(&mut *tx).await?;
        let Some((status, options)) = c else { return Ok(Some("campaign cancelled")) };
        let live = match attempt {
            Attempt::First => status == "sending",
            Attempt::Later => matches!(status.as_str(), "sending" | "testing" | "sent"),
        };
        if !live {
            return Ok(Some(if status == "cancelled" { "campaign cancelled" } else { "campaign stopped" }));
        }
        smart_hours = crate::features::sendtime::options::of(options.as_ref()).and_then(|o| o.smart_hours());
    }
    if let Some(fid) = flow_id {
        let f: Option<(String, Option<i64>)> = sqlx::query_as("SELECT status, (definition->'settings'->>'smart_sending_hours')::bigint FROM flows WHERE id = $1").bind(fid).fetch_optional(&mut *tx).await?;
        let Some((status, hours)) = f else { return Ok(Some("flow deleted")) };
        if status != "live" {
            return Ok(Some("flow turned off"));
        }
        smart_hours = hours.filter(|h| *h > 0);
    }
    // Read here, under the person's lock: a settings change either lands before this claim or after it.
    let (tz, props, ws, we): (String, Value, Option<i32>, Option<i32>) = sqlx::query_as(
        "SELECT a.timezone, p.properties, ss.sms_start_minute, ss.sms_end_minute
         FROM accounts a JOIN profiles p ON p.id = $2 LEFT JOIN send_settings ss ON ss.account_id = a.id WHERE a.id = $1",
    )
    .bind(account_id)
    .bind(profile_id)
    .fetch_one(&mut *tx)
    .await?;
    let window = ws.zip(we);
    let account_tz = tz.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    let ((start, end), _) = crate::features::sendtime::settings::window_from(window, None);
    let zones = crate::features::sendtime::settings::sms_zones(&props, None, account_tz);
    if crate::features::sendtime::settings::decide(&zones, Utc::now(), start, end, false).wait.is_some() {
        return Ok(Some("quiet hours"));
    }
    if let Some(h) = smart_hours {
        let recent: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messages m JOIN push_deliveries d ON d.message_id = m.id
                            WHERE m.profile_id = $1 AND m.channel = 'push' AND m.id <> $2 AND NOT m.is_test AND NOT m.transactional
                              AND d.status IN ('sending', 'sent', 'unconfirmed') AND COALESCE(d.sent_at, d.claimed_at) > now() - make_interval(hours => $3))",
        )
        .bind(profile_id)
        .bind(message_id)
        .bind(h as i32)
        .fetch_one(&mut *tx)
        .await?;
        if recent {
            return Ok(Some("smart sending"));
        }
    }
    Ok(None)
}

/// Apply the results in one transaction, in the push module's lock order (people, devices, the
/// message, the deliveries, the settings), each only to the registration version sent to.
async fn apply(state: &AppState, row: &Row, message_id: Uuid, is_test: bool, turns: Vec<Turn>) -> AppResult<Outcome> {
    let mut ok: (Vec<Uuid>, Vec<Option<String>>, Vec<Uuid>, Vec<i64>) = Default::default();
    let mut gone: (Vec<Uuid>, Vec<String>, Vec<Uuid>, Vec<i64>, Vec<Option<DateTime<Utc>>>) = Default::default();
    let mut retry: (Vec<Uuid>, Vec<String>, Vec<DateTime<Utc>>) = Default::default();
    // (delivery, error, final status, device to count a failure against)
    let mut done: Vec<(Uuid, String, &'static str, Option<(Uuid, i64)>)> = Vec::new();
    let mut account_errors: Vec<(String, String)> = Vec::new();
    let (mut ok_ios, mut ok_android) = (false, false);
    let backoff = |attempts: i32, after: &Option<std::time::Duration>| {
        RETRY_WAITS.get((attempts - 1).max(0) as usize).map(|wait| Utc::now() + chrono::Duration::seconds((*wait).max(after.map_or(0, |d| d.as_secs() as i64)).min(3600)))
    };
    for t in &turns {
        match t {
            Turn::Nothing => {}
            Turn::NoCredentials(id, kind, sent) => match sent {
                // Couldn't get a token (Google sign-in busy or down): nothing was sent, try again later.
                Sent::Retry(class, after) => {
                    retry.0.push(*id);
                    retry.1.push(class.clone());
                    retry.2.push(Utc::now() + chrono::Duration::seconds(RETRY_WAITS[0].max(after.map_or(0, |d| d.as_secs() as i64)).min(3600)));
                }
                Sent::Account(class) => {
                    done.push((*id, class.clone(), "failed", None));
                    account_errors.push((kind.clone(), class.clone()));
                }
                _ => done.push((*id, "couldn't sign in to the push service".into(), "failed", None)),
            },
            Turn::Sent(c, sent) => match sent {
                Sent::Ok(pid) => {
                    ok.0.push(c.id);
                    ok.1.push(pid.clone());
                    ok.2.push(c.device_id);
                    ok.3.push(c.version);
                    ok_ios |= c.kind == "ios";
                    ok_android |= c.kind == "android";
                }
                Sent::Gone(class, ts) => {
                    // Apple: a token that stopped being valid before the device last registered stays.
                    let since = ts.and_then(DateTime::from_timestamp_millis).filter(|t| *t <= c.last_seen_at);
                    if since.is_some() {
                        done.push((c.id, class.clone(), "failed", None));
                    } else {
                        gone.0.push(c.id);
                        gone.1.push(class.clone());
                        gone.2.push(c.device_id);
                        gone.3.push(c.version);
                        gone.4.push(ts.and_then(DateTime::from_timestamp_millis));
                    }
                }
                Sent::Retry(class, after) => match backoff(c.attempts, after) {
                    Some(at) => {
                        retry.0.push(c.id);
                        retry.1.push(class.clone());
                        retry.2.push(at);
                    }
                    // The service stayed busy: given up, but not the device's fault.
                    None => done.push((c.id, class.clone(), "failed", None)),
                },
                Sent::Unknown(class) => done.push((c.id, format!("{class}; not sent again in case it arrived"), "unconfirmed", None)),
                Sent::Failed(class) => done.push((c.id, class.clone(), "failed", Some((c.device_id, c.version)))),
                Sent::Rejected(class) => done.push((c.id, class.clone(), "failed", None)),
                Sent::Account(class) => {
                    done.push((c.id, class.clone(), "failed", None));
                    account_errors.push((c.kind.clone(), class.clone()));
                }
            },
        }
    }
    let mut tx = state.db.begin().await?;
    // 0. The people (the recipient and the devices' owners), so a profile deletion either finishes
    // first or waits for this.
    sqlx::query(
        "SELECT 1 FROM profiles WHERE id IN (SELECT profile_id FROM messages WHERE id = $1 UNION SELECT d.profile_id FROM push_devices d JOIN push_deliveries pd ON pd.device_id = d.id WHERE pd.message_id = $1)
         ORDER BY id FOR NO KEY UPDATE",
    )
    .bind(message_id)
    .execute(&mut *tx)
    .await?;
    // 1. Devices.
    if !ok.2.is_empty() {
        sqlx::query("UPDATE push_devices d SET last_sent_at = now(), failures = 0, last_error = NULL FROM unnest($1::uuid[], $2::bigint[]) AS x(id, v) WHERE d.id = x.id AND d.version = x.v")
            .bind(&ok.2)
            .bind(&ok.3)
            .execute(&mut *tx)
            .await?;
    }
    let mut removed_owners: Vec<Uuid> = Vec::new();
    if !gone.2.is_empty() {
        // Only the registration sent to, and (Apple) only if it wasn't renewed after it lapsed.
        removed_owners = sqlx::query_scalar::<_, Option<Uuid>>(
            "DELETE FROM push_devices d USING unnest($1::uuid[], $2::bigint[], $3::timestamptz[]) AS x(id, v, lapsed)
             WHERE d.id = x.id AND d.version = x.v AND (x.lapsed IS NULL OR d.last_seen_at <= x.lapsed) RETURNING CASE WHEN d.anonymous THEN d.profile_id END",
        )
        .bind(&gone.2)
        .bind(&gone.3)
        .bind(&gone.4)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .flatten()
        .collect();
    }
    let failing: Vec<(Uuid, i64, String)> = done.iter().filter_map(|d| d.3.map(|(id, v)| (id, v, d.1.clone()))).collect();
    if !failing.is_empty() {
        sqlx::query(
            "UPDATE push_devices d SET failures = d.failures + 1, last_error = x.err,
                    status = CASE WHEN d.failures + 1 >= $4 THEN 'disabled' ELSE d.status END,
                    disabled_at = CASE WHEN d.failures + 1 >= $4 THEN now() ELSE d.disabled_at END
             FROM unnest($1::uuid[], $2::bigint[], $3::text[]) AS x(id, v, err) WHERE d.id = x.id AND d.version = x.v",
        )
        .bind(failing.iter().map(|f| f.0).collect::<Vec<_>>())
        .bind(failing.iter().map(|f| f.1).collect::<Vec<_>>())
        .bind(failing.iter().map(|f| f.2.clone()).collect::<Vec<_>>())
        .bind(MAX_FAILURES)
        .execute(&mut *tx)
        .await?;
    }
    hook::wait(row.account_id, hook::Point::InApply).await;
    // 2. The message.
    sqlx::query("SELECT 1 FROM messages WHERE id = $1 FOR UPDATE").bind(message_id).execute(&mut *tx).await?;
    // 3. The deliveries.
    if !ok.0.is_empty() {
        sqlx::query(
            "UPDATE push_deliveries pd SET status = 'sent', sent_at = COALESCE(pd.sent_at, now()), provider_id = x.pid, error = NULL
             FROM unnest($1::uuid[], $2::text[]) AS x(id, pid) WHERE pd.id = x.id AND pd.status = 'sending'",
        )
        .bind(&ok.0)
        .bind(&ok.1)
        .execute(&mut *tx)
        .await?;
    }
    if !gone.0.is_empty() {
        sqlx::query("UPDATE push_deliveries pd SET status = 'gone', error = x.err FROM unnest($1::uuid[], $2::text[]) AS x(id, err) WHERE pd.id = x.id AND pd.status = 'sending'")
            .bind(&gone.0)
            .bind(&gone.1)
            .execute(&mut *tx)
            .await?;
    }
    if !retry.0.is_empty() {
        // Claimed deliveries are 'sending'; ones that never got credentials are still due.
        sqlx::query(
            "UPDATE push_deliveries pd SET status = 'retry', error = x.err, next_attempt_at = x.at,
                    attempts = pd.attempts + CASE WHEN pd.status = 'sending' THEN 0 ELSE 1 END
             FROM unnest($1::uuid[], $2::text[], $3::timestamptz[]) AS x(id, err, at) WHERE pd.id = x.id AND pd.status IN ('sending', 'queued', 'retry')",
        )
        .bind(&retry.0)
        .bind(&retry.1)
        .bind(&retry.2)
        .execute(&mut *tx)
        .await?;
    }
    if !done.is_empty() {
        let ids: Vec<Uuid> = done.iter().map(|d| d.0).collect();
        let errs: Vec<String> = done.iter().map(|d| d.1.clone()).collect();
        let sts: Vec<String> = done.iter().map(|d| d.2.to_string()).collect();
        sqlx::query(
            "UPDATE push_deliveries pd SET status = x.st, error = x.err, next_attempt_at = NULL
             FROM unnest($1::uuid[], $2::text[], $3::text[]) AS x(id, err, st) WHERE pd.id = x.id AND pd.status IN ('sending', 'queued', 'retry')",
        )
        .bind(&ids)
        .bind(&errs)
        .bind(&sts)
        .execute(&mut *tx)
        .await?;
    }
    // Retries that aren't due yet keep a job of their own.
    let later: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT min(next_attempt_at) FROM push_deliveries WHERE message_id = $1 AND status = 'retry'").bind(message_id).fetch_one(&mut *tx).await?;
    if let Some(at) = later {
        crate::jobs::enqueue_background(&mut *tx, "push_retry", json!({"message_id": message_id, "account_id": row.account_id}), Some(at)).await?;
    }
    let out = settle(&mut tx, state, row.account_id, message_id, is_test).await?;
    // 4. The settings: a platform that accepted is set up right; one that refused for the account isn't.
    if ok_ios || ok_android {
        sqlx::query(
            "UPDATE push_settings SET apns_error = CASE WHEN $2 THEN NULL ELSE apns_error END, fcm_error = CASE WHEN $3 THEN NULL ELSE fcm_error END
             WHERE account_id = $1 AND credentials_version = $4",
        )
        .bind(row.account_id)
        .bind(ok_ios)
        .bind(ok_android)
        .bind(row.credentials_version)
        .execute(&mut *tx)
        .await?;
    }
    for (kind, class) in &account_errors {
        let col = match kind.as_str() {
            "ios" => "apns_error",
            "android" => "fcm_error",
            _ => continue,
        };
        sqlx::query(&format!("UPDATE push_settings SET {col} = $2 WHERE account_id = $1 AND credentials_version = $3"))
            .bind(row.account_id)
            .bind(class)
            .bind(row.credentials_version)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    // An anonymous device that's gone takes its nameless profile with it, when nothing else is on it.
    if !removed_owners.is_empty() {
        super::client::forget_nameless(&state.db, &removed_owners).await?;
    }
    Ok(out)
}

/// Reasons a whole message can be skipped for, as fixed words.
const SKIP_REASONS: &[&str] = &[
    "account closed", "quiet hours", "smart sending", "campaign cancelled", "campaign stopped", "flow turned off", "flow deleted", "push turned off", "the device changed, or push is off for it",
];

/// Settle a message from its deliveries, inside the caller's transaction. Accepted by a device:
/// sent (or delivered), charged once. Still waiting on a retry: retrying (not sent). Sent with no
/// answer: unconfirmed. All skipped: skipped. Otherwise failed.
async fn settle(tx: &mut PgConnection, state: &AppState, account_id: Uuid, message_id: Uuid, is_test: bool) -> AppResult<Outcome> {
    let (accepted, waiting, unconfirmed, skipped, total, err): (i64, i64, i64, i64, i64, Option<String>) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'sent'), count(*) FILTER (WHERE status IN ('queued', 'sending', 'retry')), count(*) FILTER (WHERE status = 'unconfirmed'),
                count(*) FILTER (WHERE status = 'skipped'), count(*),
                (array_agg(error ORDER BY created_at) FILTER (WHERE status NOT IN ('sent', 'queued', 'sending')))[1]
         FROM push_deliveries WHERE message_id = $1",
    )
    .bind(message_id)
    .fetch_one(&mut *tx)
    .await?;
    if accepted > 0 {
        accept_in(tx, state, message_id).await?;
        return Ok(Outcome::Sent(message_id));
    }
    if waiting > 0 {
        sqlx::query("UPDATE messages SET status = 'retrying', error = 'waiting to retry' WHERE id = $1 AND status IN ('sending', 'retrying')").bind(message_id).execute(&mut *tx).await?;
        return Ok(Outcome::Sent(message_id));
    }
    if unconfirmed > 0 {
        sqlx::query("UPDATE messages SET status = 'unconfirmed', error = 'the push service never answered; not sent again' WHERE id = $1 AND status IN ('sending', 'retrying')")
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        return Ok(Outcome::Sent(message_id));
    }
    if total > 0 && skipped == total {
        let why = SKIP_REASONS.iter().copied().find(|r| err.as_deref() == Some(*r)).unwrap_or("not sent");
        mark_skipped(tx, state, account_id, message_id, why, is_test).await?;
        return Ok(Outcome::Skipped(why));
    }
    let why = err.unwrap_or_else(|| "not delivered to any device".into());
    let failed: Option<Uuid> = sqlx::query_scalar("UPDATE messages SET status = 'failed', error = $2 WHERE id = $1 AND sent_at IS NULL AND status IN ('sending', 'retrying') RETURNING id")
        .bind(message_id)
        .bind(&why)
        .fetch_optional(&mut *tx)
        .await?;
    if failed.is_some() && !is_test {
        crate::features::messages_unsent_in(tx, &state.db, account_id, &[message_id]).await;
    }
    Ok(Outcome::Failed(why))
}

/// A device accepted the message (or showed it, or was tapped): inside the caller's transaction,
/// the first time only, mark it sent, charge it and queue the "Received Push" event; always bring
/// its status up to date (delivered when any device reported showing it).
pub(super) async fn accept_in(tx: &mut PgConnection, state: &AppState, message_id: Uuid) -> AppResult<()> {
    let first: Option<(Uuid, bool)> = sqlx::query_as("UPDATE messages SET sent_at = now(), error = NULL WHERE id = $1 AND sent_at IS NULL RETURNING account_id, is_test")
        .bind(message_id)
        .fetch_optional(&mut *tx)
        .await?;
    let delivered: Option<(Uuid, bool)> = sqlx::query_as(
        "UPDATE messages m SET status = CASE WHEN EXISTS (SELECT 1 FROM push_deliveries d WHERE d.message_id = m.id AND d.delivered_at IS NOT NULL) THEN 'delivered' ELSE 'sent' END,
                delivered_at = COALESCE(m.delivered_at, (SELECT min(d.delivered_at) FROM push_deliveries d WHERE d.message_id = m.id)), error = NULL
         WHERE m.id = $1 AND m.status <> 'delivered' RETURNING m.account_id, m.status = 'delivered'",
    )
    .bind(message_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((account_id, is_test)) = first {
        let price = if state.cfg.hosted { crate::billing::prices(state).push } else { 0 };
        if price > 0 {
            crate::billing::debit_usage_tx(tx, account_id, "push", 1, price).await?;
            sqlx::query("UPDATE messages SET cost_micros = $2 WHERE id = $1").bind(message_id).bind(price).execute(&mut *tx).await?;
        }
        if !is_test {
            crate::jobs::enqueue_background(&mut *tx, "push_event", json!({"message_id": message_id, "account_id": account_id, "metric": "Received Push"}), None).await?;
        }
    }
    let _ = delivered;
    Ok(())
}

/// Background job: record a message's "Received Push" or "Clicked Push" event (once: keyed on the message).
pub async fn record_message_event(state: &AppState, message_id: Uuid, metric: &str) -> anyhow::Result<()> {
    let m: Option<(Uuid, Uuid, Option<Uuid>, Option<Uuid>, Option<String>, bool)> =
        sqlx::query_as("SELECT account_id, profile_id, campaign_id, flow_id, subject, is_test FROM messages WHERE id = $1").bind(message_id).fetch_optional(&state.db).await?;
    let Some((account_id, profile_id, campaign_id, flow_id, subject, is_test)) = m else { return Ok(()) };
    if is_test {
        return Ok(());
    }
    if hook::event_fails(account_id) {
        anyhow::bail!("event recording failed (test)");
    }
    let (unique, props) = match metric {
        "Clicked Push" => {
            let url: Option<String> = sqlx::query_scalar("SELECT url FROM message_clicks WHERE message_id = $1 ORDER BY id LIMIT 1").bind(message_id).fetch_optional(&state.db).await?;
            (format!("click:{message_id}"), json!({"campaign_id": campaign_id, "flow_id": flow_id, "message_id": message_id, "url": url}))
        }
        _ => (message_id.to_string(), json!({"subject": subject, "campaign_id": campaign_id, "flow_id": flow_id, "message_id": message_id})),
    };
    crate::audience::record_event(
        &state.db,
        crate::audience::NewEvent { account_id, profile_id, metric, properties: props, value: None, occurred_at: None, unique_id: Some(unique), message_id: Some(message_id) },
    )
    .await
    .map_err(|e| anyhow::anyhow!(e.public_message()))?;
    Ok(())
}

/// Background job: upkeep, bounded per run.
///
/// - Deliveries a crashed worker left `sending` are `unconfirmed` (they may have gone): never resent.
/// - Deliveries a crash left `queued` (never handed over) are handed off now, through the final claim.
/// - Retries whose job was lost are given up; registration counters older than a day, devices
///   turned off more than 30 days ago and anonymous devices unseen for 180 days are deleted.
pub async fn cleanup(state: &AppState) -> anyhow::Result<()> {
    let stuck: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(
        "WITH s AS (UPDATE push_deliveries SET status = 'unconfirmed', error = 'interrupted while sending; not sent again'
                    WHERE id IN (SELECT id FROM push_deliveries WHERE status = 'sending' AND claimed_at < now() - interval '10 minutes' LIMIT 500)
                    RETURNING message_id)
         SELECT DISTINCT m.account_id, m.id, m.is_test FROM s JOIN messages m ON m.id = s.message_id",
    )
    .fetch_all(&state.db)
    .await?;
    let lost: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(
        "WITH s AS (UPDATE push_deliveries SET status = 'failed', error = 'retry never ran', next_attempt_at = NULL
                    WHERE id IN (SELECT id FROM push_deliveries WHERE status = 'retry' AND next_attempt_at < now() - interval '1 hour' LIMIT 500)
                    RETURNING message_id)
         SELECT DISTINCT m.account_id, m.id, m.is_test FROM s JOIN messages m ON m.id = s.message_id",
    )
    .fetch_all(&state.db)
    .await?;
    for (account_id, message_id, is_test) in stuck.into_iter().chain(lost) {
        let mut tx = state.db.begin().await?;
        settle(&mut tx, state, account_id, message_id, is_test).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
        tx.commit().await?;
    }
    let queued: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT message_id FROM push_deliveries WHERE status = 'queued' AND created_at < now() - interval '10 minutes' LIMIT 100",
    )
    .fetch_all(&state.db)
    .await?;
    for message_id in queued {
        hand_off(state, message_id, Attempt::Later).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    }
    sqlx::query("DELETE FROM push_registration_counts WHERE hour < now() - interval '1 day'").execute(&state.db).await?;
    sqlx::query("DELETE FROM push_devices WHERE id IN (SELECT id FROM push_devices WHERE status = 'disabled' AND disabled_at < now() - interval '30 days' LIMIT 1000)").execute(&state.db).await?;
    let owners: Vec<Option<Uuid>> =
        sqlx::query_scalar("DELETE FROM push_devices WHERE id IN (SELECT id FROM push_devices WHERE anonymous AND last_seen_at < now() - interval '180 days' LIMIT 1000) RETURNING profile_id")
            .fetch_all(&state.db)
            .await?;
    let owners: Vec<Uuid> = owners.into_iter().flatten().collect();
    super::client::forget_nameless(&state.db, &owners).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    Ok(())
}

/// Scheduler: queue upkeep in the background lane, unless a run is already waiting.
pub async fn schedule_cleanup(state: &AppState) -> anyhow::Result<()> {
    let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = 'push_cleanup')").fetch_one(&state.db).await?;
    if !waiting {
        crate::jobs::enqueue_background(&state.db, "push_cleanup", json!({}), None).await?;
    }
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

/// Send a test to one device. Its owner's push opt-out still applies.
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
