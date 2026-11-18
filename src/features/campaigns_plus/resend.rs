//! Resend to people who didn't open (or didn't click).
//!
//! A resend is an ordinary email campaign with a row in `campaign_resends` naming its original. That
//! row is the authority: a resend's audience and channel can't be changed through any path, and when
//! it starts sending (`campaigns::prepare_send`) its audience is always the resend audience, and its
//! limits are checked again in the transaction that fixes the audience ([`check_at_send`]).
//!
//! The audience: the original's recipients whose email went out (sent or delivered, not bounced,
//! failed or skipped), at the same address they have now, who haven't opened (or clicked) the
//! original or an earlier resend of it, and who are subscribed to email now. Like every audience it's
//! turned into recipients once, when the send starts, and a retry adds nobody. Right before each email
//! goes to the provider the address and consent are checked again ([`handoff_stop`]). Suppressions,
//! unsubscribes, sunsetting, smart sending and billing apply as for any campaign. Lists and segments
//! don't: who got the original decides.
//!
//! DumbMail doesn't record which opens came from mail privacy proxies (Apple Mail), so every recorded
//! open counts as an open, and people whose opens we can't see may get the email again.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::campaigns::{Audience, AudienceRef};
use crate::error::{AppError, AppResult};
use crate::models::CampaignRow;
use crate::state::AppState;

/// How long after the original finished sending a resend can be made: opens need time to come in.
pub const MIN_HOURS_AFTER: i64 = 4;
/// How long after the original a resend can still be made or start sending.
pub const MAX_DAYS_AFTER: i64 = 30;
/// Resends per original.
pub const MAX_RESENDS: i64 = 2;

pub const UNDERCOUNT_NOTE: &str = "Opens are undercounted: some email apps (Apple Mail, for one) hide or fake opens, so some people who read the original will get it again. DumbMail can't tell those opens apart, so every recorded open counts.";

/// When campaign `o` finished sending. Campaigns that finished before DumbMail recorded it fall back
/// to their last message.
const FINISHED: &str = "COALESCE(o.finished_at, GREATEST(o.sent_at, (SELECT max(fm.sent_at) FROM messages fm WHERE fm.campaign_id = o.id AND NOT fm.is_test)))";

/// Slots that count against an original's limit: a resend that exists, or one that started sending.
const HELD: &str = "(r.campaign_id IS NOT NULL OR r.started_at IS NOT NULL)";

/// The conditions campaign `o` must meet to be resent, checked in the statements that create a
/// resend and that start one. `min_hours`: how long since it finished.
fn eligible_sql(min_hours: i64) -> String {
    format!(
        "o.status = 'sent' AND o.channel = 'email' AND NOT EXISTS (SELECT 1 FROM campaign_resends x WHERE x.campaign_id = o.id)
         AND {FINISHED} <= now() - make_interval(hours => {min_hours}) AND {FINISHED} >= now() - make_interval(days => {MAX_DAYS_AFTER})"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    NoOpen,
    NoClick,
}

impl Target {
    pub fn parse(s: &str) -> AppResult<Target> {
        match s {
            "no_open" => Ok(Target::NoOpen),
            "no_click" => Ok(Target::NoClick),
            _ => Err(AppError::bad("target must be no_open (didn't open) or no_click (didn't click)")),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Target::NoOpen => "no_open",
            Target::NoClick => "no_click",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Target::NoOpen => "didn't open",
            Target::NoClick => "didn't click",
        }
    }
}

/// A resend's audience, ready to put into SQL.
pub struct ResendAudience {
    original_id: Uuid,
    /// The original and all its resends: an open of any of them counts.
    family: Vec<Uuid>,
    target: Target,
}

impl ResendAudience {
    /// A condition on `p` (a profile of the campaign's account). Driven from the original's messages
    /// through `messages_campaign_profile_uq (campaign_id, profile_id)`; only a person whose email
    /// now is the address the original went to.
    pub fn push(&self, qb: &mut QueryBuilder<'_, Postgres>) {
        qb.push("(p.email_consent = 'subscribed' AND (p.id, lower(p.email)) IN (SELECT m.profile_id, lower(m.to_address) FROM messages m WHERE m.campaign_id = ")
            .push_bind(self.original_id)
            .push(" AND NOT m.is_test AND m.status IN ('sent', 'delivered')) AND NOT EXISTS (SELECT 1 FROM messages o WHERE o.campaign_id = ANY(")
            .push_bind(self.family.clone())
            .push(") AND o.profile_id = p.id AND NOT o.is_test AND ");
        qb.push(match self.target {
            Target::NoOpen => "(o.opened_at IS NOT NULL OR o.clicked_at IS NOT NULL)",
            Target::NoClick => "o.clicked_at IS NOT NULL",
        });
        qb.push("))");
    }
}

async fn family(db: &mut PgConnection, original_id: Uuid) -> sqlx::Result<Vec<Uuid>> {
    let mut f: Vec<Uuid> = sqlx::query_scalar("SELECT campaign_id FROM campaign_resends WHERE original_id = $1 AND campaign_id IS NOT NULL").bind(original_id).fetch_all(&mut *db).await?;
    f.push(original_id);
    Ok(f)
}

/// The audience of resend `campaign_id`. Refused once the original is more than 30 days old (unless
/// the resend already started: then its audience is fixed and only reports read it).
pub async fn resolve(db: &PgPool, account_id: Uuid, campaign_id: Uuid) -> AppResult<ResendAudience> {
    let row: Option<(Uuid, String, bool)> = sqlx::query_as("SELECT original_id, target, started_at IS NOT NULL FROM campaign_resends WHERE campaign_id = $1 AND account_id = $2")
        .bind(campaign_id)
        .bind(account_id)
        .fetch_optional(db)
        .await?;
    let (original_id, target, started) = row.ok_or_else(|| AppError::bad("this resend audience belongs to a resend campaign; use a list or segment"))?;
    if !started {
        let fresh: bool = sqlx::query_scalar(&format!("SELECT {FINISHED} >= now() - make_interval(days => {MAX_DAYS_AFTER}) FROM campaigns o WHERE o.id = $1")).bind(original_id).fetch_one(db).await?;
        if !fresh {
            return Err(AppError::bad(format!("the original went out more than {MAX_DAYS_AFTER} days ago; a resend must go out within {MAX_DAYS_AFTER} days of it")));
        }
    }
    let mut conn = db.acquire().await?;
    Ok(ResendAudience { original_id, family: family(&mut conn, original_id).await?, target: Target::parse(&target)? })
}

/// For sending: a resend's own audience in place of whatever its editable audience says; any other
/// campaign's audience as it is.
/// `is_resend` comes with the campaign row the send already reads (`campaigns_plus::PREPARE_COLUMNS`).
pub fn send_audience(campaign_id: Uuid, is_resend: bool, audience: Audience) -> Audience {
    if is_resend {
        Audience { include: vec![AudienceRef { kind: "resend".into(), id: campaign_id }], exclude: vec![] }
    } else {
        audience
    }
}

/// In the transaction that fixes a campaign's audience, on the connection holding its lock: for a
/// resend whose audience isn't fixed yet, check its original is still fully sent and within 30 days and that
/// it holds one of the two slots, then mark the slot started (kept for good). Returns why it can't
/// go, or None (also for any campaign that isn't a resend).
pub async fn check_at_send(db: &mut PgConnection, c: &CampaignRow, is_resend: bool) -> sqlx::Result<Option<String>> {
    if !is_resend {
        return Ok(None);
    }
    let slot: Option<Uuid> = sqlx::query_scalar("SELECT original_id FROM campaign_resends WHERE campaign_id = $1 FOR UPDATE").bind(c.id).fetch_optional(&mut *db).await?;
    let Some(original_id) = slot else { return Ok(None) };
    // Already fixed (a retried job, or the winner send after an A/B test): nobody is added.
    let fixed: bool = sqlx::query_scalar("SELECT audience_fixed_at IS NOT NULL FROM campaigns WHERE id = $1").bind(c.id).fetch_one(&mut *db).await?;
    if fixed {
        return Ok(None);
    }
    if c.channel != "email" {
        return Ok(Some("Resends are email only".into()));
    }
    let ok: bool = sqlx::query_scalar(&format!("SELECT EXISTS (SELECT 1 FROM campaigns o WHERE o.id = $1 AND {})", eligible_sql(MIN_HOURS_AFTER))).bind(original_id).fetch_one(&mut *db).await?;
    if !ok {
        return Ok(Some(format!(
            "The original must have finished sending at least {MIN_HOURS_AFTER} hours and at most {MAX_DAYS_AFTER} days ago when a resend starts"
        )));
    }
    let held: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM campaign_resends r WHERE r.original_id = $1 AND {HELD}")).bind(original_id).fetch_one(&mut *db).await?;
    if held > MAX_RESENDS {
        return Ok(Some(format!("The original already has {MAX_RESENDS} resends")));
    }
    sqlx::query("UPDATE campaign_resends SET started_at = COALESCE(started_at, now()) WHERE campaign_id = $1").bind(c.id).execute(&mut *db).await?;
    Ok(None)
}

/// Columns added to the send path's last check before the hand-off (`sending::last_check`, where
/// `$2` is the campaign and `p` the profile): whether it's a resend, and the address the original
/// went to for this person.
pub const HANDOFF_COLUMNS: &str = "EXISTS (SELECT 1 FROM campaign_resends hr WHERE hr.campaign_id = $2),
    (SELECT ho.to_address FROM campaign_resends hr JOIN messages ho ON ho.campaign_id = hr.original_id AND ho.profile_id = p.id AND NOT ho.is_test WHERE hr.campaign_id = $2 LIMIT 1)";

/// For a resend, why it mustn't go now: the person's address isn't the one the original went to (or
/// isn't the one about to be used), or they aren't subscribed any more.
pub fn handoff_stop(resend: bool, consent: &str, email: Option<&str>, original_to: Option<&str>, to: &str) -> Option<&'static str> {
    if !resend {
        return None;
    }
    if consent != "subscribed" {
        return Some("not subscribed (resend)");
    }
    let same = |a: Option<&str>| a.is_some_and(|a| a.eq_ignore_ascii_case(to));
    if !same(email) || !same(original_to) {
        return Some("address changed since the original (resend)");
    }
    None
}

/// A resend's audience and channel stay as they were made. Refuses a change to either; the same
/// value again (as the dashboard saves it) is fine.
pub async fn refuse_audience_change(db: &PgPool, c: &CampaignRow, audience: Option<&Value>, channel: Option<&str>) -> AppResult<()> {
    if audience.is_none() && channel.is_none() {
        return Ok(());
    }
    let resend: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM campaign_resends WHERE campaign_id = $1)").bind(c.id).fetch_one(db).await?;
    if !resend {
        return Ok(());
    }
    let norm = |v: &Value| serde_json::from_value::<Audience>(v.clone()).ok().map(|a| serde_json::to_value(a).unwrap_or_default());
    let audience_changed = audience.is_some_and(|a| norm(a) != norm(&c.audience));
    if audience_changed || channel.is_some_and(|ch| ch != c.channel) {
        return Err(AppError::Conflict("a resend goes to the people who didn't engage with its original; its audience and channel can't be changed (duplicate it to send to a list instead)".into()));
    }
    Ok(())
}

/// Readiness: a resend audience only on its own campaign, alone, on email.
pub fn audience_problems(c: &CampaignRow, aud: &Audience) -> Vec<String> {
    let is_resend = |r: &AudienceRef| r.kind == "resend";
    let mut problems = Vec::new();
    if aud.exclude.iter().any(is_resend) {
        problems.push("A resend audience can't be excluded; use a list or segment".to_string());
    }
    if aud.include.iter().any(is_resend) {
        if aud.include.len() != 1 || aud.include[0].id != c.id {
            problems.push("A resend's audience can only be used by that resend, on its own".to_string());
        }
        if c.channel != "email" {
            problems.push("Resends are email only".to_string());
        }
    }
    problems
}

/// Whether `c` can be resent now, and why not (for the page; the insert checks again).
async fn eligibility(db: &PgPool, account_id: Uuid, c: &CampaignRow) -> AppResult<Value> {
    let is_resend: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM campaign_resends WHERE campaign_id = $1)").bind(c.id).fetch_one(db).await?;
    let used: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM campaign_resends r WHERE r.original_id = $1 AND {HELD}")).bind(c.id).fetch_one(db).await?;
    let finished: Option<DateTime<Utc>> = sqlx::query_scalar(&format!("SELECT {FINISHED} FROM campaigns o WHERE o.id = $1 AND o.account_id = $2 AND o.status = 'sent'"))
        .bind(c.id)
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .flatten();
    let earliest = finished.map(|f| f + Duration::hours(MIN_HOURS_AFTER));
    let latest = finished.map(|f| f + Duration::days(MAX_DAYS_AFTER));
    let now = Utc::now();
    let reason = if c.channel != "email" {
        Some("Only email campaigns can be resent.".to_string())
    } else if is_resend {
        Some("This is already a resend; resends can't be resent.".to_string())
    } else if c.status != "sent" {
        Some("A campaign can be resent once it has finished sending.".to_string())
    } else if used >= MAX_RESENDS {
        Some(format!("This campaign already has {MAX_RESENDS} resends, the most it can have."))
    } else if earliest.is_some_and(|e| now < e) {
        Some(format!("Wait until {MIN_HOURS_AFTER} hours after it finished sending, so opens have time to come in."))
    } else if latest.is_some_and(|l| now > l) {
        Some(format!("It went out more than {MAX_DAYS_AFTER} days ago."))
    } else {
        None
    };
    Ok(json!({"ok": reason.is_none(), "reason": reason, "finished_at": finished, "earliest_at": earliest, "latest_at": latest, "resends_used": used, "resends_allowed": MAX_RESENDS}))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    /// no_open (default) or no_click.
    pub target: Option<String>,
    pub subject: Option<String>,
    pub preheader: Option<String>,
    pub name: Option<String>,
}

fn text(v: Option<String>, label: &str, max: usize) -> AppResult<Option<String>> {
    let v = v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if v.as_ref().is_some_and(|s| s.chars().count() > max) {
        return Err(AppError::bad(format!("{label} is too long (at most {max} characters)")));
    }
    Ok(v)
}

/// Make a draft resend of `original_id`. Every limit (original fully sent at least 4 hours and at
/// most 30 days ago, not itself a resend, two per original) is a condition of the insert that takes
/// the slot, so two people clicking at once can't make a third and nothing read earlier decides it.
pub async fn create(state: &AppState, account_id: Uuid, original_id: Uuid, b: CreateBody) -> AppResult<CampaignRow> {
    let target = Target::parse(b.target.as_deref().unwrap_or("no_open"))?;
    let subject = text(b.subject, "subject", 300)?;
    let preheader = text(b.preheader, "preview text", 300)?;
    let name = text(b.name, "name", 200)?;
    let o = crate::campaigns::load(&state.db, account_id, original_id).await?;
    let e = eligibility(&state.db, account_id, &o).await?;
    if let Some(reason) = e["reason"].as_str() {
        return Err(AppError::Conflict(reason.to_string()));
    }
    // The content the original's audience got: the A/B winner if there was a test, else version A.
    let ab = crate::campaigns::ab_test(&o);
    let mut content = crate::campaigns::version_content(&o, ab.as_ref(), o.ab_winner.as_deref()).unwrap_or(&o.content).clone();
    super::utm::set_on(&mut content, None);
    if let Some(obj) = content.as_object_mut() {
        if let Some(s) = subject {
            obj.insert("subject".into(), json!(s));
        }
        if let Some(p) = preheader {
            obj.insert("preheader".into(), json!(p));
        }
    }
    crate::features::coupons::template::check_campaign("email", &content, None)?;
    let name = name.unwrap_or_else(|| format!("{} (resend: {})", o.name.chars().take(150).collect::<String>(), target.label()));
    let id = Uuid::now_v7();
    let audience = json!({"include": [{"type": "resend", "id": id}], "exclude": []});
    let over: Option<Value> = sqlx::query_scalar("SELECT link_tags FROM campaigns WHERE id = $1").bind(o.id).fetch_one(&state.db).await?;

    let mut tx = state.db.begin().await?;
    let row: CampaignRow = sqlx::query_as("INSERT INTO campaigns (id, account_id, name, channel, audience, content, link_tags) VALUES ($1, $2, $3, 'email', $4, $5, $6) RETURNING *")
        .bind(id)
        .bind(account_id)
        .bind(&name)
        .bind(&audience)
        .bind(&content)
        .bind(&over)
        .fetch_one(&mut *tx)
        .await?;
    // Slots left by deleted drafts that never sent are free again.
    sqlx::query("DELETE FROM campaign_resends WHERE original_id = $1 AND campaign_id IS NULL AND started_at IS NULL").bind(o.id).execute(&mut *tx).await?;
    // Take the first free slot. Two requests at once may both try slot 1: the loser's insert does
    // nothing and it tries again with a fresh view, so it gets slot 2 or finds none left.
    let insert = format!(
        "INSERT INTO campaign_resends (id, campaign_id, account_id, original_id, target, number)
         SELECT $1, $2, $3, o.id, $5, v.n FROM campaigns o CROSS JOIN (VALUES (1::smallint), (2::smallint)) v(n)
         WHERE o.id = $4 AND o.account_id = $3 AND {}
           AND NOT EXISTS (SELECT 1 FROM campaign_resends r WHERE r.original_id = o.id AND r.number = v.n)
         ORDER BY v.n LIMIT 1
         ON CONFLICT (original_id, number) DO NOTHING RETURNING number",
        eligible_sql(MIN_HOURS_AFTER)
    );
    let mut slot: Option<i16> = None;
    for _ in 0..3 {
        slot = sqlx::query_scalar(&insert).bind(Uuid::now_v7()).bind(id).bind(account_id).bind(o.id).bind(target.as_str()).fetch_optional(&mut *tx).await?;
        if slot.is_some() {
            break;
        }
        let used: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM campaign_resends r WHERE r.original_id = $1 AND {HELD}")).bind(o.id).fetch_one(&mut *tx).await?;
        if used >= MAX_RESENDS {
            break;
        }
    }
    if slot.is_none() {
        drop(tx);
        let e = eligibility(&state.db, account_id, &o).await?;
        let reason = e["reason"].as_str().map(str::to_string).unwrap_or_else(|| format!("This campaign already has {MAX_RESENDS} resends, the most it can have."));
        return Err(AppError::Conflict(reason));
    }
    tx.commit().await?;
    Ok(row)
}

#[derive(sqlx::FromRow)]
struct ResendRow {
    campaign_id: Uuid,
    target: String,
    number: i16,
}

/// For an original: its resends, totals across them without counting a person twice, and whether it
/// can be resent. For a resend: what it resends.
pub async fn summary(state: &AppState, account_id: Uuid, id: Uuid) -> AppResult<Value> {
    let c = crate::campaigns::load(&state.db, account_id, id).await?;
    let of: Option<(Uuid, String, i16, String)> = sqlx::query_as(
        "SELECT r.original_id, r.target, r.number, o.name FROM campaign_resends r JOIN campaigns o ON o.id = r.original_id WHERE r.campaign_id = $1 AND r.account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    if let Some((original_id, target, number, original_name)) = of {
        return Ok(json!({
            "resend_of": {"id": original_id, "name": original_name, "target": target, "number": number},
            "audience_note": audience_note(Target::parse(&target)?),
            "undercount_note": UNDERCOUNT_NOTE,
        }));
    }
    let rows: Vec<ResendRow> = sqlx::query_as("SELECT campaign_id, target, number FROM campaign_resends WHERE original_id = $1 AND account_id = $2 AND campaign_id IS NOT NULL ORDER BY number")
        .bind(id)
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    let mut resends = Vec::with_capacity(rows.len());
    let mut family = vec![id];
    for r in &rows {
        let rc = crate::campaigns::load(&state.db, account_id, r.campaign_id).await?;
        let stats = crate::campaigns::stats(&state.db, account_id, rc.id).await?;
        resends.push(json!({"id": rc.id, "name": rc.name, "status": rc.status, "target": r.target, "number": r.number, "send_at": rc.send_at, "sent_at": rc.sent_at, "stats": stats}));
        family.push(rc.id);
    }
    // One population: the people the original or a resend reached. Opens and clicks count only for
    // them, so a rate can't pass 100%.
    let (reached, opened, clicked, delivered): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE reached), count(*) FILTER (WHERE reached AND opened), count(*) FILTER (WHERE reached AND clicked), COALESCE(sum(emails), 0)::bigint
         FROM (SELECT bool_or(status IN ('sent', 'delivered', 'complained')) AS reached,
                      bool_or(opened_at IS NOT NULL OR clicked_at IS NOT NULL) AS opened,
                      bool_or(clicked_at IS NOT NULL) AS clicked,
                      count(*) FILTER (WHERE status IN ('sent', 'delivered', 'complained')) AS emails
               FROM messages WHERE campaign_id = ANY($1) AND account_id = $2 AND NOT is_test GROUP BY profile_id) per_person",
    )
    .bind(&family)
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let rate = |n: i64| if reached > 0 { n as f64 / reached as f64 } else { 0.0 };
    Ok(json!({
        "resend_of": null,
        "resends": resends,
        "combined": if rows.is_empty() { Value::Null } else { json!({"people_reached": reached, "people_opened": opened, "people_clicked": clicked, "emails_delivered": delivered, "open_rate": rate(opened), "click_rate": rate(clicked)}) },
        "can_resend": eligibility(&state.db, account_id, &c).await?,
        "undercount_note": UNDERCOUNT_NOTE,
    }))
}

pub fn audience_note(t: Target) -> String {
    format!(
        "Everyone the original went to who {} it (or an earlier resend) by the time this starts sending, at the same email address, and who is still subscribed. Lists and segments don't apply; unsubscribes and suppressions do.",
        t.label()
    )
}
