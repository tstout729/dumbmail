//! Campaign audiences, preparation and batched sending.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow, MessageStats, Profile, PROFILE_COLS, STATS_SELECT};
use crate::segments::{self, Definition};
use crate::sending::{self, Outcome, SendSpec};
use crate::state::AppState;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Audience {
    #[serde(default)]
    pub include: Vec<AudienceRef>,
    #[serde(default)]
    pub exclude: Vec<AudienceRef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AudienceRef {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Uuid,
}

/// Version ids in order: A is the campaign's own content, B-D live in `AbTest::variants`.
pub const VERSION_IDS: [&str; 4] = ["a", "b", "c", "d"];

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbVariant {
    pub id: String,
    pub content: Value,
}

/// A campaign A/B test: a random `test_percent` of the audience is split evenly across the
/// versions; after `wait_hours` the leader on `metric` goes to everyone else (`pick` = auto),
/// or the sender picks it (`pick` = manual). At 100% everyone is in the test and there's no winner send.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AbTest {
    pub variants: Vec<AbVariant>,
    #[serde(default = "default_test_percent")]
    pub test_percent: i64,
    #[serde(default = "default_wait_hours")]
    pub wait_hours: i64,
    #[serde(default = "default_metric")]
    pub metric: String,
    #[serde(default = "default_pick")]
    pub pick: String,
}

fn default_test_percent() -> i64 {
    20
}
fn default_wait_hours() -> i64 {
    4
}
fn default_metric() -> String {
    "click_rate".into()
}
fn default_pick() -> String {
    "auto".into()
}

impl AbTest {
    pub fn versions(&self) -> Vec<String> {
        VERSION_IDS[..=self.variants.len()].iter().map(|v| v.to_string()).collect()
    }
}

pub fn parse_ab_test(channel: &str, v: &Value) -> AppResult<AbTest> {
    let t: AbTest = serde_json::from_value(v.clone()).map_err(|e| AppError::bad(format!("invalid ab_test: {e}")))?;
    if t.variants.is_empty() || t.variants.len() > 3 {
        return Err(AppError::bad("an A/B test needs 1 to 3 versions besides A"));
    }
    for (i, var) in t.variants.iter().enumerate() {
        if var.id != VERSION_IDS[i + 1] {
            return Err(AppError::bad("A/B test versions must be named b, c and d, in order"));
        }
        if !var.content.is_object() {
            return Err(AppError::bad(format!("version {} needs content", var.id.to_uppercase())));
        }
    }
    if !(5..=100).contains(&t.test_percent) {
        return Err(AppError::bad("test_percent must be between 5 and 100"));
    }
    if !(1..=168).contains(&t.wait_hours) {
        return Err(AppError::bad("wait_hours must be between 1 and 168"));
    }
    match (t.metric.as_str(), channel) {
        ("open_rate", "email") | ("click_rate", _) | ("revenue", _) => {}
        ("open_rate", _) => return Err(AppError::bad("SMS has no opens; pick the winner by click_rate or revenue")),
        _ => return Err(AppError::bad("metric must be open_rate, click_rate or revenue")),
    }
    if !matches!(t.pick.as_str(), "auto" | "manual") {
        return Err(AppError::bad("pick must be auto or manual"));
    }
    Ok(t)
}

/// The campaign's A/B test, if it has one.
pub fn ab_test(c: &CampaignRow) -> Option<AbTest> {
    c.ab_test.as_ref().filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// The content a version sends: A (or no version) is the campaign's own content.
pub fn version_content<'a>(c: &'a CampaignRow, ab: Option<&'a AbTest>, version: Option<&str>) -> Option<&'a Value> {
    match version {
        None | Some("a") => Some(&c.content),
        Some(v) => ab?.variants.iter().find(|x| x.id == v).map(|x| &x.content),
    }
}

enum Resolved {
    List(Uuid),
    Segment(Definition),
}

async fn resolve(db: &PgPool, account_id: Uuid, refs: &[AudienceRef]) -> AppResult<Vec<Resolved>> {
    let mut out = Vec::new();
    for r in refs {
        match r.kind.as_str() {
            "list" => {
                crate::audience::list_belongs(db, account_id, r.id).await?;
                out.push(Resolved::List(r.id));
            }
            "segment" => {
                let def: Option<Value> = sqlx::query_scalar("SELECT definition FROM segments WHERE id = $1 AND account_id = $2")
                    .bind(r.id)
                    .bind(account_id)
                    .fetch_optional(db)
                    .await?;
                let def = def.ok_or_else(|| AppError::not_found("segment"))?;
                out.push(Resolved::Segment(segments::load(db, account_id, &def, Some(r.id)).await?));
            }
            other => return Err(AppError::bad(format!("audience type must be list or segment, got {other:?}"))),
        }
    }
    Ok(out)
}

fn push_any(qb: &mut QueryBuilder<'_, Postgres>, items: &[Resolved]) {
    qb.push("(");
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            qb.push(" OR ");
        }
        match it {
            Resolved::List(id) => {
                qb.push("EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ").push_bind(*id).push(" AND lm.profile_id = p.id)");
            }
            Resolved::Segment(def) => segments::push_definition(qb, def),
        }
    }
    qb.push(")");
}

pub fn push_eligible(qb: &mut QueryBuilder<'_, Postgres>, channel: &str) {
    if channel == "sms" {
        qb.push(" AND p.phone_number IS NOT NULL AND p.sms_consent = 'subscribed'");
    } else {
        qb.push(" AND p.email IS NOT NULL AND p.email_consent NOT IN ('unsubscribed', 'pending') AND p.email_suppressed_reason IS NULL");
    }
}

pub async fn estimate(db: &PgPool, account_id: Uuid, channel: &str, audience: &Audience) -> AppResult<i64> {
    let mut qb = QueryBuilder::new("SELECT count(*)");
    append_audience(&mut qb, db, account_id, channel, audience).await?;
    Ok(qb.build_query_scalar().fetch_one(db).await?)
}

/// Appends `FROM profiles p WHERE ...` selecting a campaign's reachable recipients.
pub async fn append_audience(qb: &mut QueryBuilder<'_, Postgres>, db: &PgPool, account_id: Uuid, channel: &str, audience: &Audience) -> AppResult<()> {
    let r = resolve_audience(db, account_id, channel, audience).await?;
    push_audience(qb, account_id, channel, &r);
    Ok(())
}

/// A campaign audience with its lists checked and segments loaded, ready to put into SQL with
/// [`push_audience`] without another database round trip.
pub struct ResolvedAudience {
    include: Vec<Resolved>,
    exclude: Vec<Resolved>,
    sunset: bool,
}

pub async fn resolve_audience(db: &PgPool, account_id: Uuid, channel: &str, audience: &Audience) -> AppResult<ResolvedAudience> {
    if audience.include.is_empty() {
        return Err(AppError::bad("choose at least one list or segment to send to"));
    }
    let include = resolve(db, account_id, &audience.include).await?;
    let exclude = resolve(db, account_id, &audience.exclude).await?;
    let sunset = channel == "email"
        && sqlx::query_scalar::<_, bool>("SELECT email_sunset_enabled FROM accounts WHERE id = $1").bind(account_id).fetch_optional(db).await?.unwrap_or(true);
    Ok(ResolvedAudience { include, exclude, sunset })
}

/// Appends `FROM profiles p WHERE ...` for a resolved audience (see [`append_audience`]).
pub fn push_audience(qb: &mut QueryBuilder<'_, Postgres>, account_id: Uuid, channel: &str, r: &ResolvedAudience) {
    qb.push(" FROM profiles p WHERE p.account_id = ").push_bind(account_id).push(" AND ");
    push_any(qb, &r.include);
    if !r.exclude.is_empty() {
        qb.push(" AND NOT ");
        push_any(qb, &r.exclude);
    }
    push_eligible(qb, channel);
    if r.sunset {
        crate::deliverability::push_not_unengaged(qb);
    }
}

pub async fn load(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<CampaignRow> {
    sqlx::query_as::<_, CampaignRow>("SELECT * FROM campaigns WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::not_found("campaign"))
}

pub async fn load_account(db: &PgPool, id: Uuid) -> AppResult<Account> {
    sqlx::query_as::<_, Account>(
        "SELECT a.*, ARRAY(SELECT d.domain FROM sending_domains d WHERE d.account_id = a.id AND d.status = 'verified' ORDER BY d.created_at) AS verified_domains
         FROM accounts a WHERE a.id = $1",
    )
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::not_found("account"))
}

/// Check a campaign is complete enough to send; returns a list of problems.
/// `verify_domain`: true when sending through a shared provider (SES) that requires a verified domain.
pub fn readiness(account: &Account, c: &CampaignRow, verify_domain: bool) -> Vec<String> {
    let mut problems = Vec::new();
    let aud: Audience = serde_json::from_value(c.audience.clone()).unwrap_or_default();
    if aud.include.is_empty() {
        problems.push("Pick at least one list or segment".to_string());
    }
    problems.extend(content_problems(account, &c.channel, &c.content, verify_domain));
    if let Some(v) = c.ab_test.as_ref().filter(|v| !v.is_null()) {
        match parse_ab_test(&c.channel, v) {
            Ok(t) => {
                for var in &t.variants {
                    let label = format!("Version {}", var.id.to_uppercase());
                    problems.extend(content_problems(account, &c.channel, &var.content, verify_domain).into_iter().map(|p| format!("{label}: {p}")));
                }
            }
            Err(e) => problems.push(e.public_message()),
        }
    }
    if c.channel == "email" {
        if account.sending_paused_at.is_some() {
            problems.push(crate::deliverability::paused_message(account.sending_paused_reason.as_deref()));
        }
        if account.address.as_deref().unwrap_or("").trim().is_empty() {
            problems.push("Add your mailing address in Settings (required by CAN-SPAM)".into());
        }
    } else if account.sms_from.is_none() {
        problems.push("Set an SMS sending number in Settings → SMS".into());
    }
    problems
}

/// Problems with one version's content (subject, body, sender).
fn content_problems(account: &Account, channel: &str, content: &Value, verify_domain: bool) -> Vec<String> {
    let mut problems = Vec::new();
    if channel == "email" {
        let subject = content.get("subject").and_then(|s| s.as_str()).unwrap_or("");
        if subject.trim().is_empty() {
            problems.push("Add a subject line".into());
        }
        let has_body = content.get("blocks").and_then(|b| b.as_array()).is_some_and(|b| !b.is_empty())
            || content.get("html").and_then(|h| h.as_str()).is_some_and(|h| !h.trim().is_empty());
        if !has_body {
            problems.push("Add email content".into());
        }
        let from = content.get("from_email").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string).or(account.from_email.clone());
        if from.is_none() {
            problems.push("Set a sender email in Settings → Sending".into());
        }
        if let Some(f) = from.as_ref().filter(|_| verify_domain) {
            if let Err(e) = crate::sending::check_sender_domain_name(account, f) {
                problems.push(e);
            }
        }
    } else if content.get("body").and_then(|s| s.as_str()).unwrap_or("").trim().is_empty() {
        problems.push("Write the SMS message".into());
    }
    problems
}

/// Job: create queued message rows for every recipient, then fan out batches.
/// With an A/B test and no winner yet, only the test slice gets rows (each with its version);
/// once a winner is picked this runs again and adds everyone else.
pub async fn prepare_send(state: &AppState, campaign_id: Uuid) -> anyhow::Result<()> {
    let to_anyhow = |e: AppError| anyhow::anyhow!(e.public_message());
    // Look up the audience's lists and segments first: everything below runs on the one connection
    // that holds the campaign lock, so busy workers can't each hold a lock while waiting for another.
    let pre: Option<(Value, String, Uuid, String)> = sqlx::query_as("SELECT audience, status, account_id, channel FROM campaigns WHERE id = $1").bind(campaign_id).fetch_optional(&state.db).await?;
    let Some((pre_audience, pre_status, account_id, channel)) = pre else { return Ok(()) };
    if pre_status != "sending" {
        return Ok(());
    }
    let audience: Audience = serde_json::from_value(pre_audience.clone())?;
    // Only needed when this run adds recipients; a retry of a fixed audience mustn't fail on a list deleted since.
    let resolved = resolve_audience(&state.db, account_id, &channel, &audience).await;

    // Lock the campaign while creating recipients, so two send jobs can't both draw a (random) test slice.
    let mut tx = state.db.begin().await?;
    let c: CampaignRow = sqlx::query_as("SELECT * FROM campaigns WHERE id = $1 FOR UPDATE").bind(campaign_id).fetch_one(&mut *tx).await?;
    if c.status != "sending" {
        return Ok(());
    }
    if c.audience != pre_audience || c.channel != channel {
        anyhow::bail!("the campaign changed while its send was being prepared; retrying");
    }
    // Copy in the saved blocks it uses, in the same transaction that fixes the audience, so every
    // recipient (also those held for their local or best time) gets this copy and editing a saved
    // block later doesn't change what this campaign sends. A copy that can't be sent fails the
    // campaign here, before anyone gets it.
    if let Some(problem) = crate::features::content::prepare::freeze_campaign(state, &mut tx, &c).await? {
        sqlx::query("UPDATE campaigns SET status = 'failed', error = $2, updated_at = now() WHERE id = $1").bind(c.id).bind(&problem).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    if c.channel == "email" {
        if let Some(reason) = crate::deliverability::paused_reason(&mut *tx, c.account_id).await? {
            tx.commit().await?;
            crate::deliverability::stop_campaign(&state.db, c.id, &crate::deliverability::paused_message(reason.as_deref())).await?;
            return Ok(());
        }
    }
    let addr = if c.channel == "sms" { "p.phone_number" } else { "p.email" };
    let ab = ab_test(&c);
    let started: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE campaign_id = $1 AND NOT is_test)").bind(c.id).fetch_one(&mut *tx).await?;
    // Set once everyone who gets the campaign has a row (or a holdout row): a retried send job then adds
    // nobody who joined the audience since.
    let fixed: bool = sqlx::query_scalar("SELECT audience_fixed_at IS NOT NULL FROM campaigns WHERE id = $1").bind(c.id).fetch_one(&mut *tx).await?;
    let mut fix = false;
    match (&ab, &c.ab_winner) {
        (Some(t), None) if !started => {
            // One statement, so one consistent read: the audience, the test slice (a random sample dealt
            // round-robin across the versions) and everyone else, who get the winner later even if they
            // leave the list meanwhile.
            let resolved = resolved.map_err(to_anyhow)?;
            let versions = t.versions();
            let n = versions.len() as i64;
            let mut qb = QueryBuilder::new("WITH aud AS (SELECT p.account_id, p.id, ");
            qb.push(addr).push(" AS addr");
            push_audience(&mut qb, c.account_id, &c.channel, &resolved);
            qb.push("), ranked AS (SELECT a.*, row_number() OVER (ORDER BY random()) AS rn, count(*) OVER () AS total FROM aud a),
                     sized AS (SELECT r.*, CASE WHEN ").push_bind(t.test_percent).push(" >= 100 THEN r.total ELSE LEAST(GREATEST((r.total * ").push_bind(t.test_percent).push(" + 99) / 100, ").push_bind(n).push("), r.total) END AS size FROM ranked r),
                     ins AS (INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, to_address, variant)
                             SELECT gen_random_uuid(), x.account_id, x.id, ");
            qb.push_bind(c.channel.clone()).push(", ").push_bind(c.id).push(", x.addr, (").push_bind(versions).push("::text[])[1 + ((x.rn - 1) % ").push_bind(n).push(")::int]
                             FROM sized x WHERE x.rn <= x.size
                             ON CONFLICT (campaign_id, profile_id) WHERE campaign_id IS NOT NULL AND NOT is_test DO NOTHING)
                 INSERT INTO campaign_holdouts (campaign_id, profile_id) SELECT ");
            qb.push_bind(c.id).push(", x.id FROM sized x WHERE x.rn > x.size ON CONFLICT DO NOTHING");
            qb.build().execute(&mut *tx).await?;
            fix = true;
        }
        // The test slice was already created (this job is a retry): just send what's queued.
        (Some(_), None) => {}
        // The audience was fixed when the send started (or the winner was picked): a retry adds nobody.
        _ if fixed => {}
        // Everyone (no test), or everyone left after a test that started before audiences were fixed.
        _ => {
            insert_rest(&mut tx, &c, resolved).await.map_err(to_anyhow)?;
            fix = true;
        }
    }
    sqlx::query("UPDATE campaigns SET sent_at = COALESCE(sent_at, now()), audience_fixed_at = CASE WHEN $2 THEN COALESCE(audience_fixed_at, now()) ELSE audience_fixed_at END, updated_at = now() WHERE id = $1")
        .bind(c.id)
        .bind(fix)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // Send options (local time, best time, gradual): give each new recipient a release time.
    crate::features::sendtime::plan::plan_campaign(state, &c).await?;
    // One batch job per release time, run when it comes; claims keep a re-run from sending twice.
    let rows: Vec<(Uuid, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as("SELECT id, send_after FROM messages WHERE campaign_id = $1 AND status = 'queued' AND NOT is_test ORDER BY send_after NULLS FIRST, id")
            .bind(c.id)
            .fetch_all(&state.db)
            .await?;
    if rows.is_empty() {
        finish_if_done(&state.db, c.id).await?;
    }
    for group in rows.chunk_by(|a, b| a.1 == b.1) {
        let at = group[0].1;
        let ids: Vec<Uuid> = group.iter().map(|r| r.0).collect();
        for chunk in ids.chunks(100) {
            crate::jobs::enqueue(&state.db, "campaign_batch", json!({"campaign_id": c.id, "message_ids": chunk}), at).await?;
        }
    }
    Ok(())
}

/// Queue a message (with no version: the winner, or the only content) for every audience member who has none yet.
/// After an A/B test whose audience was fixed when it started, that's the holdouts who can still be messaged.
/// Runs entirely on `tx`: the audience is resolved beforehand (and only used, so only has to have
/// resolved, when there are no holdouts).
async fn insert_rest(tx: &mut sqlx::Transaction<'_, Postgres>, c: &CampaignRow, resolved: AppResult<ResolvedAudience>) -> AppResult<()> {
    let addr = if c.channel == "sms" { "p.phone_number" } else { "p.email" };
    let mut qb = QueryBuilder::new("INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, to_address) SELECT gen_random_uuid(), p.account_id, p.id, ");
    qb.push_bind(c.channel.clone()).push(", ").push_bind(c.id).push(", ").push(addr);
    let holdouts: bool = sqlx::query_scalar("SELECT audience_fixed_at IS NOT NULL AND ab_test IS NOT NULL FROM campaigns WHERE id = $1").bind(c.id).fetch_one(&mut **tx).await?;
    if holdouts {
        qb.push(" FROM profiles p JOIN campaign_holdouts h ON h.profile_id = p.id AND h.campaign_id = ").push_bind(c.id);
        qb.push(" WHERE p.account_id = ").push_bind(c.account_id);
        push_eligible(&mut qb, &c.channel);
    } else {
        push_audience(&mut qb, c.account_id, &c.channel, &resolved?);
    }
    qb.push(" ON CONFLICT (campaign_id, profile_id) WHERE campaign_id IS NOT NULL AND NOT is_test DO NOTHING");
    qb.build().execute(&mut **tx).await?;
    Ok(())
}

async fn finish_if_done(db: &PgPool, campaign_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        // A test slice that has gone out waits for its results ('testing') before the winner send;
        // the wait starts when the last test message is handled (sends can be deferred, e.g. SMS quiet hours).
        "UPDATE campaigns SET
           status = CASE WHEN awaiting_winner THEN 'testing' ELSE 'sent' END,
           ab_test_ends_at = CASE WHEN awaiting_winner THEN now() + make_interval(hours => (ab_test->>'wait_hours')::int) ELSE ab_test_ends_at END,
           updated_at = now()
         FROM (SELECT ab_test IS NOT NULL AND ab_winner IS NULL AND (ab_test->>'test_percent')::int < 100 AS awaiting_winner FROM campaigns WHERE id = $1) w
         WHERE id = $1 AND status = 'sending'
         AND NOT EXISTS (SELECT 1 FROM messages WHERE campaign_id = $1 AND status IN ('queued','sending') AND NOT is_test)",
    )
    .bind(campaign_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Job: send a batch of pre-created messages.
pub async fn send_batch(state: &AppState, campaign_id: Uuid, ids: &[Uuid]) -> anyhow::Result<()> {
    let c: CampaignRow = sqlx::query_as("SELECT * FROM campaigns WHERE id = $1").bind(campaign_id).fetch_one(&state.db).await?;
    if c.status != "sending" {
        sqlx::query("UPDATE messages SET status = 'skipped', error = 'campaign cancelled' WHERE id = ANY($1) AND status = 'queued'")
            .bind(ids)
            .execute(&state.db)
            .await?;
        return Ok(());
    }
    let account = load_account(&state.db, c.account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    if c.channel == "email" && account.sending_paused_at.is_some() {
        crate::deliverability::stop_campaign(&state.db, c.id, &crate::deliverability::paused_message(account.sending_paused_reason.as_deref())).await?;
        return Ok(());
    }
    let ab = ab_test(&c);
    let smart_hours = crate::features::sendtime::options::of(c.send_options.as_ref()).and_then(|o| o.smart_hours());
    // One read against one cutoff: each queued message in the batch is either due now (sent below) or
    // waits for a batch of its own at its release time, so none can fall between two reads.
    let cutoff = chrono::Utc::now();
    let queued: Vec<(Uuid, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as("SELECT id, send_after FROM messages WHERE id = ANY($1) AND status = 'queued' ORDER BY id")
        .bind(ids)
        .fetch_all(&state.db)
        .await?;
    // Messages waiting for their release time or (texts) their recipient's quiet hours, by when each may go.
    let mut held: std::collections::BTreeMap<chrono::DateTime<chrono::Utc>, Vec<Uuid>> = Default::default();
    let mut rows: Vec<Uuid> = Vec::with_capacity(queued.len());
    for (id, at) in queued {
        match at {
            Some(t) if t > cutoff => held.entry(t).or_default().push(id),
            _ => rows.push(id),
        }
    }
    for (i, mid) in rows.iter().enumerate() {
        // Claim the message so a duplicate or retried batch can't send it again. A crash mid-send leaves it
        // 'sending'; release_abandoned_claims fails it later instead of retrying: at most once, never twice.
        let Some((pid, variant)) = claim(&state.db, c.id, *mid, &c.channel, cutoff).await? else { continue };
        // Smart sending: skip people who just got a message on this channel.
        if let Some(hours) = smart_hours {
            if crate::features::sendtime::recently_messaged(&state.db, pid, &c.channel, hours).await? {
                sqlx::query("UPDATE messages SET status = 'skipped', error = 'smart sending' WHERE id = $1").bind(mid).execute(&state.db).await?;
                continue;
            }
        }
        let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1");
        let Some(profile) = sqlx::query_as::<_, Profile>(&q).bind(pid).fetch_optional(&state.db).await? else {
            sqlx::query("UPDATE messages SET status = 'skipped', error = 'profile deleted' WHERE id = $1").bind(mid).execute(&state.db).await?;
            continue;
        };
        // Test-slice messages carry their version; everyone after the test gets the winner.
        let content = version_content(&c, ab.as_ref(), variant.as_deref().or(c.ab_winner.as_deref())).unwrap_or(&c.content);
        let outcome = sending::send(
            state,
            SendSpec {
                account: &account,
                profile: &profile,
                channel: &c.channel,
                content,
                campaign_id: Some(c.id),
                flow_id: None,
                flow_node_id: None,
                event: None,
                is_test: false,
                message_id: Some(*mid),
                to_override: None,
            },
        )
        .await;
        match outcome {
            // A text outside its recipient's hours waits alone; everyone else in the batch goes on.
            Ok(Outcome::Deferred(until)) if c.channel == "sms" => {
                if requeue_claimed(&state.db, c.id, *mid, &c.channel, Some(until)).await? {
                    held.entry(until).or_default().push(*mid);
                }
            }
            Ok(Outcome::Deferred(until)) => {
                if !requeue_claimed(&state.db, c.id, *mid, &c.channel, None).await? {
                    // Cancelled or stopped meanwhile: the rest were skipped with it.
                    return Ok(());
                }
                for (at, ids) in &held {
                    crate::jobs::enqueue(&state.db, "campaign_batch", json!({"campaign_id": c.id, "message_ids": ids}), Some(*at)).await?;
                }
                let rest: Vec<Uuid> = rows[i..].to_vec();
                crate::jobs::enqueue(&state.db, "campaign_batch", json!({"campaign_id": c.id, "message_ids": rest}), Some(until)).await?;
                return Ok(());
            }
            Ok(Outcome::Skipped(why)) => {
                sqlx::query("UPDATE messages SET status = 'skipped', error = $2 WHERE id = $1").bind(mid).bind(why).execute(&state.db).await?;
            }
            Ok(_) => {}
            Err(e) => {
                // A content problem affects every recipient: fail the campaign instead of retrying forever.
                sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE campaign_id = $1 AND (status = 'queued' OR id = $3)")
                    .bind(c.id)
                    .bind(e.public_message())
                    .bind(mid)
                    .execute(&state.db)
                    .await?;
                sqlx::query("UPDATE campaigns SET status = 'failed', error = $2 WHERE id = $1").bind(c.id).bind(e.public_message()).execute(&state.db).await?;
                return Ok(());
            }
        }
    }
    for (at, ids) in held {
        crate::jobs::enqueue(&state.db, "campaign_batch", json!({"campaign_id": c.id, "message_ids": ids}), Some(at)).await?;
    }
    finish_if_done(&state.db, c.id).await
}

/// Claim a due campaign message for sending: only while its campaign is still sending (and, for email,
/// the account isn't paused). The campaign row is share-locked for the check, so a cancel or pause that
/// is committing either lands first (no claim) or waits for the claim.
pub async fn claim(db: &PgPool, campaign_id: Uuid, message_id: Uuid, channel: &str, cutoff: chrono::DateTime<chrono::Utc>) -> sqlx::Result<Option<(Uuid, Option<String>)>> {
    sqlx::query_as(
        "UPDATE messages m SET status = 'sending', claimed_at = now()
         WHERE m.id = $1 AND m.status = 'queued' AND (m.send_after IS NULL OR m.send_after <= $2)
           AND EXISTS (SELECT 1 FROM campaigns c JOIN accounts a ON a.id = c.account_id
                       WHERE c.id = $3 AND c.status = 'sending' AND ($4 = 'sms' OR a.sending_paused_at IS NULL) FOR SHARE OF c)
         RETURNING m.profile_id, m.variant",
    )
    .bind(message_id)
    .bind(cutoff)
    .bind(campaign_id)
    .bind(channel)
    .fetch_optional(db)
    .await
}

/// Put a claimed message back in the queue (to go at `send_after`, or when its batch is re-run), unless
/// its campaign was cancelled or stopped, or email was paused, since it was claimed: then it's skipped.
/// Returns true when it's queued again. Locks the campaign row like [`claim`], so a cancel committing at
/// the same moment can't leave it queued behind it.
pub async fn requeue_claimed(db: &PgPool, campaign_id: Uuid, message_id: Uuid, channel: &str, send_after: Option<chrono::DateTime<chrono::Utc>>) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    let st: Option<(String, bool)> = sqlx::query_as(
        "SELECT c.status, a.sending_paused_at IS NOT NULL FROM campaigns c JOIN accounts a ON a.id = c.account_id WHERE c.id = $1 FOR SHARE OF c",
    )
    .bind(campaign_id)
    .fetch_optional(&mut *tx)
    .await?;
    let stop = match st.as_ref().map(|(s, p)| (s.as_str(), *p)) {
        Some(("sending", true)) if channel == "email" => Some("sending paused"),
        Some(("sending", _)) => None,
        Some(("cancelled", _)) | None => Some("campaign cancelled"),
        Some(_) => Some("campaign stopped"),
    };
    match stop {
        None => {
            sqlx::query("UPDATE messages SET status = 'queued', claimed_at = NULL, send_after = COALESCE($2, send_after) WHERE id = $1 AND status = 'sending'")
                .bind(message_id)
                .bind(send_after)
                .execute(&mut *tx)
                .await?;
        }
        Some(why) => {
            sqlx::query("UPDATE messages SET status = 'skipped', error = $2, claimed_at = NULL WHERE id = $1 AND status = 'sending'").bind(message_id).bind(why).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(stop.is_none())
}

/// Attribution: each conversion credited to a message by the account's attribution settings (by
/// default the newest email opened or clicked in the prior 5 days, or SMS clicked in the prior 24
/// hours). Defined with the other reporting code in `features::reports::attribution`. Yields (event
/// id, value, occurred_at, profile_id, message_id, channel, campaign_id, flow_id, flow_node_id, variant).
pub const ATTRIBUTION_SQL: &str = crate::features::reports::attribution::ATTRIBUTION_SQL;

pub async fn stats(db: &PgPool, account_id: Uuid, campaign_id: Uuid) -> AppResult<MessageStats> {
    let q = format!("SELECT {STATS_SELECT} FROM messages m WHERE m.campaign_id = $1 AND NOT m.is_test AND m.status <> 'skipped'");
    let mut s: MessageStats = sqlx::query_as(&q).bind(campaign_id).fetch_one(db).await?;
    let q = format!("SELECT count(*)::bigint, COALESCE(sum(value), 0)::float8 FROM ({ATTRIBUTION_SQL} AND e.profile_id IN (SELECT profile_id FROM messages WHERE campaign_id = $2)) x WHERE x.campaign_id = $2");
    let (orders, revenue): (i64, f64) = sqlx::query_as(&q).bind(account_id).bind(campaign_id).fetch_one(db).await?;
    s.orders = orders;
    s.revenue = revenue;
    Ok(s.finish())
}

/// Stats for each version's test-slice recipients, in version order.
pub async fn version_stats(db: &PgPool, account_id: Uuid, campaign_id: Uuid, versions: &[String]) -> AppResult<Vec<MessageStats>> {
    let mut out = Vec::with_capacity(versions.len());
    for v in versions {
        let q = format!("SELECT {STATS_SELECT} FROM messages m WHERE m.campaign_id = $1 AND m.variant = $2 AND NOT m.is_test AND m.status <> 'skipped'");
        let mut s: MessageStats = sqlx::query_as(&q).bind(campaign_id).bind(v).fetch_one(db).await?;
        let q = format!("SELECT count(*)::bigint, COALESCE(sum(value), 0)::float8 FROM ({ATTRIBUTION_SQL} AND e.profile_id IN (SELECT profile_id FROM messages WHERE campaign_id = $2)) x WHERE x.campaign_id = $2 AND x.variant = $3");
        let (orders, revenue): (i64, f64) = sqlx::query_as(&q).bind(account_id).bind(campaign_id).bind(v).fetch_one(db).await?;
        s.orders = orders;
        s.revenue = revenue;
        out.push(s.finish());
    }
    Ok(out)
}

/// The leading version on `metric` (ties go to the earlier version, so A wins a dead heat) and the
/// confidence, from 0.5 (a coin flip) to 1.0, that it really beats the runner-up. Revenue has no
/// confidence: one large order can swing revenue per recipient, and order counts don't capture that.
pub fn leader(metric: &str, stats: &[MessageStats]) -> (usize, Option<f64>) {
    let score = |s: &MessageStats| match metric {
        "open_rate" => s.open_rate,
        "revenue" => s.revenue / s.delivered.max(1) as f64,
        _ => s.click_rate,
    };
    let mut best = 0;
    for (i, s) in stats.iter().enumerate() {
        if score(s) > score(&stats[best]) {
            best = i;
        }
    }
    if metric == "revenue" {
        return (best, None);
    }
    let runner_up = (0..stats.len()).filter(|&i| i != best).max_by(|&a, &b| score(&stats[a]).total_cmp(&score(&stats[b])));
    let Some(r) = runner_up else { return (best, Some(0.5)) };
    // Two-proportion z-test on the rate.
    let hits = |s: &MessageStats| if metric == "open_rate" { s.unique_opens } else { s.unique_clicks } as f64;
    let (n1, n2) = (stats[best].delivered as f64, stats[r].delivered as f64);
    if n1 == 0.0 || n2 == 0.0 {
        return (best, Some(0.5));
    }
    let (p1, p2) = (hits(&stats[best]) / n1, hits(&stats[r]) / n2);
    let pooled = (hits(&stats[best]) + hits(&stats[r])) / (n1 + n2);
    let se = (pooled * (1.0 - pooled) * (1.0 / n1 + 1.0 / n2)).sqrt();
    // A tie is a coin flip; the CDF approximation is off by ~5e-10 at z = 0.
    if se == 0.0 || p1 <= p2 {
        return (best, Some(0.5));
    }
    (best, Some(normal_cdf((p1 - p2) / se).max(0.5)))
}

/// Standard normal CDF (Abramowitz and Stegun 7.1.26, error < 1.5e-7).
fn normal_cdf(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let erf = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    if z >= 0.0 { 0.5 * (1.0 + erf) } else { 0.5 * (1.0 - erf) }
}

/// End a campaign's test: send `chosen` (or the leader, when None) to everyone not in the test slice.
pub async fn pick_winner(state: &AppState, c: &CampaignRow, chosen: Option<&str>) -> AppResult<CampaignRow> {
    let t = ab_test(c).ok_or_else(|| AppError::bad("this campaign has no A/B test"))?;
    if c.status != "testing" {
        return Err(AppError::Conflict(format!("this campaign is {}, not waiting on test results", c.status)));
    }
    let versions = t.versions();
    let winner = match chosen {
        Some(v) if versions.iter().any(|x| x == v) => v.to_string(),
        Some(_) => return Err(AppError::bad(format!("pick one of: {}", versions.join(", ")))),
        None => {
            let stats = version_stats(&state.db, c.account_id, c.id, &versions).await?;
            versions[leader(&t.metric, &stats).0].clone()
        }
    };
    // Looked up before the transaction, which then holds the campaign row on one connection only.
    let audience: Audience = serde_json::from_value(c.audience.clone()).map_err(|e| AppError::bad(format!("invalid audience: {e}")))?;
    let resolved = resolve_audience(&state.db, c.account_id, &c.channel, &audience).await;
    // One transaction: record the winner, queue everyone else and the send job, so a crash can't leave
    // a campaign 'sending' with nothing queued, and no batch can mark it sent before the rest are queued.
    let mut tx = state.db.begin().await?;
    let row: Option<CampaignRow> = sqlx::query_as("UPDATE campaigns SET ab_winner = $2, ab_winner_at = now(), status = 'sending', updated_at = now() WHERE id = $1 AND status = 'testing' RETURNING *")
        .bind(c.id)
        .bind(&winner)
        .fetch_optional(&mut *tx)
        .await?;
    let row = row.ok_or_else(|| AppError::Conflict("the winner was already picked".into()))?;
    insert_rest(&mut tx, &row, resolved).await?;
    crate::jobs::enqueue(&mut *tx, "campaign_send", json!({"campaign_id": c.id}), None).await?;
    tx.commit().await?;
    Ok(row)
}

/// A failed or cancelled campaign that is edited goes back to draft: forget its last attempt, so the
/// next send is planned afresh for the audience it has then. Messages that never reached the provider
/// (queued, skipped, or failed without being claimed: a content error stops the rest unsent) are
/// removed, with the fixed audience and A/B holdouts. Messages that went out, or may have (claimed and
/// sent, failed or still sending), stay: those people are never sent the campaign again.
pub async fn reset_for_relaunch(tx: &mut sqlx::Transaction<'_, Postgres>, campaign_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "DELETE FROM messages WHERE campaign_id = $1 AND NOT is_test
           AND (status IN ('queued', 'skipped') OR (status = 'failed' AND claimed_at IS NULL))",
    )
    .bind(campaign_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM campaign_holdouts WHERE campaign_id = $1").bind(campaign_id).execute(&mut **tx).await?;
    sqlx::query("UPDATE campaigns SET audience_fixed_at = NULL WHERE id = $1").bind(campaign_id).execute(&mut **tx).await?;
    Ok(())
}

/// How long a claimed campaign message can stay 'sending' before it counts as interrupted (a send takes seconds).
const CLAIM_TIMEOUT_MINUTES: i32 = 10;

/// Scheduler: a worker that crashed mid-send leaves its message 'sending'. Mark those failed (they may or
/// may not have gone out, so they're never resent) and let their campaigns finish.
pub async fn release_abandoned_claims(state: &AppState) -> anyhow::Result<()> {
    let campaigns: Vec<Uuid> = sqlx::query_scalar(
        "WITH r AS (UPDATE messages SET status = 'failed', error = 'send interrupted; not retried to avoid a duplicate'
                    WHERE status = 'sending' AND campaign_id IS NOT NULL AND claimed_at < now() - make_interval(mins => $1)
                    RETURNING campaign_id)
         SELECT DISTINCT campaign_id FROM r",
    )
    .bind(CLAIM_TIMEOUT_MINUTES)
    .fetch_all(&state.db)
    .await?;
    for id in campaigns {
        finish_if_done(&state.db, id).await?;
    }
    Ok(())
}

/// Scheduler: tests whose wait is over and that pick automatically send their leader.
pub async fn finish_due_tests(state: &AppState) -> anyhow::Result<()> {
    let due: Vec<CampaignRow> = sqlx::query_as("SELECT * FROM campaigns WHERE status = 'testing' AND ab_test_ends_at <= now() AND ab_test->>'pick' = 'auto'")
        .fetch_all(&state.db)
        .await?;
    for c in due {
        if let Err(e) = pick_winner(state, &c, None).await {
            tracing::warn!(campaign = %c.id, error = %e.public_message(), "couldn't finish A/B test");
        }
    }
    Ok(())
}

/// Send tests of `c`. `create_profiles`: an address with no profile gets one (for realistic
/// personalization next time); without it such an address is refused. Callers check who may send
/// tests where (`features::team::sends`).
pub async fn send_test(state: &AppState, account: &Account, c: &CampaignRow, to: &[String], version: Option<&str>, create_profiles: bool) -> AppResult<usize> {
    if to.is_empty() || to.len() > 10 {
        return Err(AppError::bad("send tests to 1-10 addresses"));
    }
    let ab = ab_test(c);
    let content = version_content(c, ab.as_ref(), version).ok_or_else(|| AppError::bad("this campaign has no such version"))?;
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE account_id = $1 AND is_test AND created_at > now() - interval '1 hour'")
        .bind(account.id)
        .fetch_one(&state.db)
        .await?;
    if recent + to.len() as i64 > 50 {
        return Err(AppError::LimitReached("test sends are limited to 50 an hour".into()));
    }
    let mut sent = 0;
    for raw in to {
        // A number is put in one form first, so the profile lookup, the opt-out check and the send agree.
        let addr = if c.channel == "sms" {
            &crate::models::normalize_phone(raw).ok_or_else(|| AppError::bad(format!("{raw:?} isn't a phone number (use +15035550100)")))?
        } else {
            raw
        };
        let input = if c.channel == "email" {
            crate::audience::ProfileInput { email: Some(addr.clone()), ..Default::default() }
        } else {
            crate::audience::ProfileInput { phone_number: Some(addr.clone()), ..Default::default() }
        };
        // Use the matching profile for realistic personalization; create one only if absent.
        let profile = match crate::audience::find_profile(&state.db, account.id, &input).await? {
            Some(p) => {
                let suppressed: Option<String> = sqlx::query_scalar("SELECT email_suppressed_reason FROM profiles WHERE id = $1").bind(p.id).fetch_one(&state.db).await?;
                let opted_out = if c.channel == "sms" { p.sms_consent == "unsubscribed" } else { p.email_consent == "unsubscribed" || suppressed.is_some() };
                if opted_out {
                    return Err(AppError::bad(format!("{addr} has unsubscribed or bounced; pick another test address")));
                }
                p
            }
            None if create_profiles => crate::audience::upsert_profile(&state.db, account.id, &input).await?.profile,
            None => return Err(AppError::bad(format!("{addr} isn't a profile in this account yet; ask a manager or admin to send it the first test"))),
        };
        let out = sending::send(
            state,
            SendSpec {
                account,
                profile: &profile,
                channel: &c.channel,
                content,
                campaign_id: Some(c.id),
                flow_id: None,
                flow_node_id: None,
                event: None,
                is_test: true,
                message_id: None,
                to_override: Some(addr.clone()),
            },
        )
        .await?;
        match out {
            Outcome::Sent(_) => sent += 1,
            Outcome::Failed(e) => return Err(AppError::bad(format!("test to {addr} failed: {e}"))),
            _ => {}
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(delivered: i64, opens: i64, clicks: i64) -> MessageStats {
        MessageStats { delivered, unique_opens: opens, unique_clicks: clicks, ..Default::default() }.finish()
    }

    #[test]
    fn leader_and_confidence() {
        // Clear win: 30% vs 10% clicks on 1,000 each.
        let (i, conf) = leader("click_rate", &[stats(1000, 0, 100), stats(1000, 0, 300)]);
        assert_eq!(i, 1);
        assert!(conf.unwrap() > 0.99, "{conf:?}");
        // A dozen opens can't call a winner.
        let (i, conf) = leader("open_rate", &[stats(20, 5, 0), stats(20, 7, 0)]);
        assert_eq!(i, 1);
        assert!(conf.unwrap() < 0.9, "{conf:?}");
        // A dead heat goes to A at coin-flip confidence.
        assert_eq!(leader("click_rate", &[stats(100, 0, 5), stats(100, 0, 5)]), (0, Some(0.5)));
        // Revenue per recipient picks the leader, with no confidence figure.
        let rev = |delivered, revenue| MessageStats { delivered, revenue, ..Default::default() };
        assert_eq!(leader("revenue", &[rev(100, 90.0), rev(100, 120.0)]), (1, None));
        assert!((normal_cdf(1.96) - 0.975).abs() < 1e-4);
    }

    #[test]
    fn ab_test_validation() {
        let ok = json!({"variants": [{"id": "b", "content": {"subject": "B"}}], "test_percent": 20, "wait_hours": 4, "metric": "open_rate"});
        assert_eq!(parse_ab_test("email", &ok).unwrap().versions(), vec!["a", "b"]);
        assert!(parse_ab_test("sms", &ok).is_err(), "SMS can't be judged on opens");
        let skip = json!({"variants": [{"id": "c", "content": {}}]});
        assert!(parse_ab_test("email", &skip).is_err());
        let none = json!({"variants": []});
        assert!(parse_ab_test("email", &none).is_err());
        let big = json!({"variants": [{"id": "b", "content": {}}], "test_percent": 101});
        assert!(parse_ab_test("email", &big).is_err());
    }
}
