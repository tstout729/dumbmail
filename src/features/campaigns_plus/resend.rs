//! Resend to people who didn't open (or didn't click).
//!
//! A resend is an ordinary email campaign whose audience is `{"type": "resend", "id": <its own id>}`.
//! That audience is the original's recipients whose email went out (sent or delivered, not bounced or
//! failed), who haven't opened (or clicked) the original or an earlier resend of it, and who are
//! subscribed to email now. Like every audience it's turned into recipients once, in the
//! transaction that fixes the audience when the send starts (`campaigns::prepare_send`), and a retry
//! adds nobody. Suppressions, unsubscribes, sunsetting, smart sending, billing and the last check
//! before the hand-off apply as for any campaign. Lists and segments don't: who got the original decides.
//!
//! DumbMail doesn't record which opens came from mail privacy proxies (Apple Mail), so every recorded
//! open counts as an open, and people whose opens we can't see may get the email again.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::campaigns::{Audience, AudienceRef};
use crate::error::{AppError, AppResult};
use crate::models::CampaignRow;
use crate::state::AppState;

/// How long after the original finished sending a resend can be made: opens need time to come in.
pub const MIN_HOURS_AFTER: i64 = 4;
/// How long after the original a resend can still be made or sent.
pub const MAX_DAYS_AFTER: i64 = 30;
/// Resends per original.
pub const MAX_RESENDS: i64 = 2;

pub const UNDERCOUNT_NOTE: &str = "Opens are undercounted: some email apps (Apple Mail, for one) hide or fake opens, so some people who read the original will get it again. DumbMail can't tell those opens apart, so every recorded open counts.";

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
    /// through `messages_campaign_profile_uq (campaign_id, profile_id)`.
    pub fn push(&self, qb: &mut QueryBuilder<'_, Postgres>) {
        qb.push("(p.email_consent = 'subscribed' AND p.id IN (SELECT m.profile_id FROM messages m WHERE m.campaign_id = ")
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

/// When the original finished sending: its last message, or when it started.
async fn finished_at(db: &PgPool, account_id: Uuid, id: Uuid) -> AppResult<Option<DateTime<Utc>>> {
    let row: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "SELECT GREATEST(c.sent_at, (SELECT max(m.sent_at) FROM messages m WHERE m.campaign_id = c.id AND NOT m.is_test)) FROM campaigns c WHERE c.id = $1 AND c.account_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    row.ok_or_else(|| AppError::not_found("campaign"))
}

fn too_late(finished: DateTime<Utc>) -> bool {
    Utc::now() > finished + Duration::days(MAX_DAYS_AFTER)
}

/// The audience of resend `campaign_id`. Refused once the original is more than 30 days old.
pub async fn resolve(db: &PgPool, account_id: Uuid, campaign_id: Uuid) -> AppResult<ResendAudience> {
    let row: Option<(Uuid, String)> = sqlx::query_as("SELECT original_id, target FROM campaign_resends WHERE campaign_id = $1 AND account_id = $2").bind(campaign_id).bind(account_id).fetch_optional(db).await?;
    let (original_id, target) = row.ok_or_else(|| AppError::bad("this resend audience belongs to a resend campaign; use a list or segment"))?;
    let finished = finished_at(db, account_id, original_id).await?;
    if finished.is_some_and(too_late) {
        return Err(AppError::bad(format!("the original went out more than {MAX_DAYS_AFTER} days ago; a resend must go out within {MAX_DAYS_AFTER} days of it")));
    }
    let mut family: Vec<Uuid> = sqlx::query_scalar("SELECT campaign_id FROM campaign_resends WHERE original_id = $1").bind(original_id).fetch_all(db).await?;
    family.push(original_id);
    Ok(ResendAudience { original_id, family, target: Target::parse(&target)? })
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

/// Whether `c` can be resent now, and why not.
async fn eligibility(db: &PgPool, account_id: Uuid, c: &CampaignRow) -> AppResult<Value> {
    let is_resend: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM campaign_resends WHERE campaign_id = $1)").bind(c.id).fetch_one(db).await?;
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM campaign_resends WHERE original_id = $1").bind(c.id).fetch_one(db).await?;
    let finished = finished_at(db, account_id, c.id).await?;
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
    Ok(json!({"ok": reason.is_none(), "reason": reason, "earliest_at": earliest, "latest_at": latest, "resends_used": used, "resends_allowed": MAX_RESENDS}))
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

/// Make a draft resend of `original_id`. At most [`MAX_RESENDS`] per original, enforced by the
/// unique slot in the insert, so two people clicking at once can't make a third.
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
    if let Some(o) = content.as_object_mut() {
        if let Some(s) = subject {
            o.insert("subject".into(), json!(s));
        }
        if let Some(p) = preheader {
            o.insert("preheader".into(), json!(p));
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
    // Take the first free slot. Two requests at once may both try slot 1: the loser's insert does
    // nothing and it tries again with a fresh view, so it gets slot 2 or finds none left.
    let mut slot: Option<i16> = None;
    for _ in 0..3 {
        slot = sqlx::query_scalar(
            "INSERT INTO campaign_resends (campaign_id, account_id, original_id, target, number)
             SELECT $1, $2, $3, $4, v.n FROM (VALUES (1::smallint), (2::smallint)) v(n)
             WHERE NOT EXISTS (SELECT 1 FROM campaign_resends r WHERE r.original_id = $3 AND r.number = v.n)
             ORDER BY v.n LIMIT 1
             ON CONFLICT (original_id, number) DO NOTHING RETURNING number",
        )
        .bind(id)
        .bind(account_id)
        .bind(o.id)
        .bind(target.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        if slot.is_some() {
            break;
        }
        let used: i64 = sqlx::query_scalar("SELECT count(*) FROM campaign_resends WHERE original_id = $1").bind(o.id).fetch_one(&mut *tx).await?;
        if used >= MAX_RESENDS {
            break;
        }
    }
    if slot.is_none() {
        return Err(AppError::Conflict(format!("This campaign already has {MAX_RESENDS} resends, the most it can have.")));
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
    let rows: Vec<ResendRow> = sqlx::query_as("SELECT campaign_id, target, number FROM campaign_resends WHERE original_id = $1 AND account_id = $2 ORDER BY number").bind(id).bind(account_id).fetch_all(&state.db).await?;
    let mut resends = Vec::with_capacity(rows.len());
    let mut family = vec![id];
    for r in &rows {
        let rc = crate::campaigns::load(&state.db, account_id, r.campaign_id).await?;
        let stats = crate::campaigns::stats(&state.db, account_id, rc.id).await?;
        resends.push(json!({"id": rc.id, "name": rc.name, "status": rc.status, "target": r.target, "number": r.number, "send_at": rc.send_at, "sent_at": rc.sent_at, "stats": stats}));
        family.push(rc.id);
    }
    let (reached, opened, clicked, delivered): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(DISTINCT profile_id) FILTER (WHERE status IN ('sent', 'delivered', 'complained')),
                count(DISTINCT profile_id) FILTER (WHERE opened_at IS NOT NULL OR clicked_at IS NOT NULL),
                count(DISTINCT profile_id) FILTER (WHERE clicked_at IS NOT NULL),
                count(*) FILTER (WHERE status IN ('sent', 'delivered', 'complained'))
         FROM messages WHERE campaign_id = ANY($1) AND account_id = $2 AND NOT is_test",
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
        "Everyone the original went to who {} it (or an earlier resend) by the time this starts sending, and who is still subscribed. Lists and segments don't apply; unsubscribes and suppressions do.",
        t.label()
    )
}
