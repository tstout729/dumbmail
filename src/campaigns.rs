//! Campaign audiences, preparation and batched sending.

use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::{Account, CampaignRow, MessageStats, Profile, PROFILE_COLS, STATS_SELECT};
use crate::segments::{self, Definition};
use crate::sending::{self, Outcome, SendSpec};
use crate::state::AppState;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Audience {
    #[serde(default)]
    pub include: Vec<AudienceRef>,
    #[serde(default)]
    pub exclude: Vec<AudienceRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AudienceRef {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Uuid,
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
                out.push(Resolved::Segment(segments::parse(&def)?));
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
        qb.push(" AND p.email IS NOT NULL AND p.email_consent <> 'unsubscribed' AND p.email_suppressed_reason IS NULL");
    }
}

pub async fn estimate(db: &PgPool, account_id: Uuid, channel: &str, audience: &Audience) -> AppResult<i64> {
    let mut qb = QueryBuilder::new("SELECT count(*)");
    append_audience(&mut qb, db, account_id, channel, audience).await?;
    Ok(qb.build_query_scalar().fetch_one(db).await?)
}

/// Appends `FROM profiles p WHERE ...` selecting a campaign's reachable recipients.
async fn append_audience(qb: &mut QueryBuilder<'_, Postgres>, db: &PgPool, account_id: Uuid, channel: &str, audience: &Audience) -> AppResult<()> {
    if audience.include.is_empty() {
        return Err(AppError::bad("choose at least one list or segment to send to"));
    }
    let inc = resolve(db, account_id, &audience.include).await?;
    let exc = resolve(db, account_id, &audience.exclude).await?;
    qb.push(" FROM profiles p WHERE p.account_id = ").push_bind(account_id).push(" AND ");
    push_any(qb, &inc);
    if !exc.is_empty() {
        qb.push(" AND NOT ");
        push_any(qb, &exc);
    }
    push_eligible(qb, channel);
    Ok(())
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
    sqlx::query_as::<_, Account>("SELECT * FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| AppError::not_found("account"))
}

/// Check a campaign is complete enough to send; returns a list of problems.
pub fn readiness(account: &Account, c: &CampaignRow) -> Vec<String> {
    let mut problems = Vec::new();
    let aud: Audience = serde_json::from_value(c.audience.clone()).unwrap_or_default();
    if aud.include.is_empty() {
        problems.push("Pick at least one list or segment".to_string());
    }
    if c.channel == "email" {
        let subject = c.content.get("subject").and_then(|s| s.as_str()).unwrap_or("");
        if subject.trim().is_empty() {
            problems.push("Add a subject line".into());
        }
        let has_body = c.content.get("blocks").and_then(|b| b.as_array()).is_some_and(|b| !b.is_empty())
            || c.content.get("html").and_then(|h| h.as_str()).is_some_and(|h| !h.trim().is_empty());
        if !has_body {
            problems.push("Add email content".into());
        }
        let from = c.content.get("from_email").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string).or(account.from_email.clone());
        if from.is_none() {
            problems.push("Set a sender email in Settings → Sending".into());
        }
        if account.address.as_deref().unwrap_or("").trim().is_empty() {
            problems.push("Add your mailing address in Settings (required by CAN-SPAM)".into());
        }
    } else {
        if c.content.get("body").and_then(|s| s.as_str()).unwrap_or("").trim().is_empty() {
            problems.push("Write the SMS message".into());
        }
        if account.sms_from.is_none() {
            problems.push("Set an SMS sending number in Settings → SMS".into());
        }
    }
    problems
}

/// Job: create queued message rows for every recipient, then fan out batches.
pub async fn prepare_send(state: &AppState, campaign_id: Uuid) -> anyhow::Result<()> {
    let c: CampaignRow = sqlx::query_as("SELECT * FROM campaigns WHERE id = $1").bind(campaign_id).fetch_one(&state.db).await?;
    if c.status != "sending" {
        return Ok(());
    }
    let audience: Audience = serde_json::from_value(c.audience.clone())?;
    let mut qb = QueryBuilder::new("INSERT INTO messages (id, account_id, profile_id, channel, campaign_id, to_address) SELECT gen_random_uuid(), p.account_id, p.id, ");
    qb.push_bind(c.channel.clone()).push(", ").push_bind(c.id).push(", ");
    qb.push(if c.channel == "sms" { "p.phone_number" } else { "p.email" });
    append_audience(&mut qb, &state.db, c.account_id, &c.channel, &audience).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
    qb.push(" ON CONFLICT (campaign_id, profile_id) WHERE campaign_id IS NOT NULL AND NOT is_test DO NOTHING");
    qb.build().execute(&state.db).await?;
    sqlx::query("UPDATE campaigns SET sent_at = COALESCE(sent_at, now()), updated_at = now() WHERE id = $1").bind(c.id).execute(&state.db).await?;

    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM messages WHERE campaign_id = $1 AND status = 'queued' AND NOT is_test ORDER BY id")
        .bind(c.id)
        .fetch_all(&state.db)
        .await?;
    if ids.is_empty() {
        finish_if_done(&state.db, c.id).await?;
    }
    for chunk in ids.chunks(100) {
        crate::jobs::enqueue(&state.db, "campaign_batch", json!({"campaign_id": c.id, "message_ids": chunk}), None).await?;
    }
    Ok(())
}

async fn finish_if_done(db: &PgPool, campaign_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE campaigns SET status = 'sent', updated_at = now() WHERE id = $1 AND status = 'sending'
         AND NOT EXISTS (SELECT 1 FROM messages WHERE campaign_id = $1 AND status = 'queued' AND NOT is_test)",
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
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT id, profile_id FROM messages WHERE id = ANY($1) AND status = 'queued' ORDER BY id")
        .bind(ids)
        .fetch_all(&state.db)
        .await?;
    for (i, (mid, pid)) in rows.iter().enumerate() {
        let q = format!("SELECT {PROFILE_COLS} FROM profiles p WHERE p.id = $1");
        let Some(profile) = sqlx::query_as::<_, Profile>(&q).bind(pid).fetch_optional(&state.db).await? else { continue };
        let outcome = sending::send(
            state,
            SendSpec {
                account: &account,
                profile: &profile,
                channel: &c.channel,
                content: &c.content,
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
            Ok(Outcome::Deferred(until)) => {
                let rest: Vec<Uuid> = rows[i..].iter().map(|r| r.0).collect();
                crate::jobs::enqueue(&state.db, "campaign_batch", json!({"campaign_id": c.id, "message_ids": rest}), Some(until)).await?;
                return Ok(());
            }
            Ok(Outcome::Skipped(why)) => {
                sqlx::query("UPDATE messages SET status = 'skipped', error = $2 WHERE id = $1").bind(mid).bind(why).execute(&state.db).await?;
            }
            Ok(_) => {}
            Err(e) => {
                // A content problem affects every recipient: fail the campaign instead of retrying forever.
                sqlx::query("UPDATE messages SET status = 'failed', error = $2 WHERE campaign_id = $1 AND status = 'queued'")
                    .bind(c.id)
                    .bind(e.public_message())
                    .execute(&state.db)
                    .await?;
                sqlx::query("UPDATE campaigns SET status = 'failed', error = $2 WHERE id = $1").bind(c.id).bind(e.public_message()).execute(&state.db).await?;
                return Ok(());
            }
        }
    }
    finish_if_done(&state.db, c.id).await
}

/// Attribution: each conversion credited to the newest email opened/clicked in the prior 5 days,
/// or SMS clicked in the prior 24 hours. Yields (event id, value, occurred_at, campaign_id, flow_id, flow_node_id).
pub const ATTRIBUTION_SQL: &str = "
    SELECT e.id AS event_id, COALESCE(e.value, 0) AS value, e.occurred_at, att.campaign_id, att.flow_id, att.flow_node_id
    FROM events e
    JOIN metrics mt ON mt.id = e.metric_id
    JOIN accounts a ON a.id = e.account_id AND mt.name = a.conversion_metric
    LEFT JOIN LATERAL (
        SELECT m.campaign_id, m.flow_id, m.flow_node_id FROM messages m
        WHERE m.profile_id = e.profile_id AND NOT m.is_test AND (
            (m.channel = 'email' AND GREATEST(m.opened_at, m.clicked_at) BETWEEN e.occurred_at - interval '5 days' AND e.occurred_at)
            OR (m.channel = 'sms' AND m.clicked_at BETWEEN e.occurred_at - interval '1 day' AND e.occurred_at))
        ORDER BY GREATEST(m.opened_at, m.clicked_at) DESC LIMIT 1
    ) att ON true
    WHERE e.account_id = $1";

pub async fn stats(db: &PgPool, account_id: Uuid, campaign_id: Uuid) -> AppResult<MessageStats> {
    let q = format!("SELECT {STATS_SELECT} FROM messages m WHERE m.campaign_id = $1 AND NOT m.is_test AND m.status <> 'skipped'");
    let mut s: MessageStats = sqlx::query_as(&q).bind(campaign_id).fetch_one(db).await?;
    let q = format!("SELECT count(*)::bigint, COALESCE(sum(value), 0)::float8 FROM ({ATTRIBUTION_SQL}) x WHERE x.campaign_id = $2");
    let (orders, revenue): (i64, f64) = sqlx::query_as(&q).bind(account_id).bind(campaign_id).fetch_one(db).await?;
    s.orders = orders;
    s.revenue = revenue;
    Ok(s.finish())
}

pub async fn send_test(state: &AppState, account: &Account, c: &CampaignRow, to: &[String]) -> AppResult<usize> {
    if to.is_empty() || to.len() > 10 {
        return Err(AppError::bad("send tests to 1-10 addresses"));
    }
    let mut sent = 0;
    for addr in to {
        let input = if c.channel == "email" {
            crate::audience::ProfileInput { email: Some(addr.clone()), ..Default::default() }
        } else {
            crate::audience::ProfileInput { phone_number: Some(addr.clone()), ..Default::default() }
        };
        // Use the matching profile for realistic personalization; create one only if absent.
        let profile = match crate::audience::find_profile(&state.db, account.id, &input).await? {
            Some(p) => p,
            None => crate::audience::upsert_profile(&state.db, account.id, &input).await?.profile,
        };
        let out = sending::send(
            state,
            SendSpec {
                account,
                profile: &profile,
                channel: &c.channel,
                content: &c.content,
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
