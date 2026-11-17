//! Sending while Klaviyo and DumbMail both run.
//!
//! While the ongoing sync is on, a Klaviyo opt-out has to be in DumbMail before DumbMail sends:
//! - **Campaigns** start only when the consent part of a sync finished within the last 15 minutes.
//!   Otherwise starting one triggers a sync and the campaign waits ("Waiting for Klaviyo opt-outs to
//!   sync"); if no sync succeeds within 2 hours it goes back to draft with that reason instead of sending.
//! - **Flows** send on consent up to 2 hours old (the sync runs hourly); past that, flow emails and texts
//!   wait until a sync succeeds.
//!
//! Also here: while event history is being imported, and for each segment's first refresh after it,
//! segment-triggered flows take in new members without starting runs.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::state::AppState;

pub const WAIT_MESSAGE: &str = "Waiting for Klaviyo opt-outs to sync. It starts as soon as a sync from Klaviyo finishes.";
pub const PAUSED_MESSAGE: &str =
    "Not sent: no sync from Klaviyo succeeded in 2 hours, so recent Klaviyo opt-outs might be missing. Check the sync under Settings → Move from Klaviyo, then send it again.";
const CAMPAIGN_FRESH_MINUTES: i64 = 15;
const CAMPAIGN_MAX_WAIT_HOURS: i64 = 2;
const FLOW_MAX_AGE_HOURS: i64 = 2;

/// The account's ongoing sync, if it's on: (import id, status, key stored, consent fresh as of).
async fn active_sync(db: &PgPool, account_id: Uuid) -> sqlx::Result<Option<(Uuid, String, bool, DateTime<Utc>)>> {
    sqlx::query_as(
        "SELECT id, status, api_key_enc IS NOT NULL, COALESCE(consent_synced_at, started_at, created_at) FROM klaviyo_imports
         WHERE account_id = $1 AND sync_enabled ORDER BY created_at DESC LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await
}

async fn kick_sync(db: &PgPool, import_id: Uuid, status: &str, key: bool) -> anyhow::Result<()> {
    if status == "done" && key {
        super::runner::start_sync(db, import_id).await?;
    }
    Ok(())
}

/// Called when a campaign starts sending (`campaigns::prepare_send`). False: not now (it was queued
/// again, or put back to draft with the reason).
pub async fn campaign_may_start(state: &AppState, campaign_id: Uuid) -> anyhow::Result<bool> {
    let db = &state.db;
    let row: Option<(Uuid, String, bool)> = sqlx::query_as("SELECT account_id, status, EXISTS (SELECT 1 FROM messages m WHERE m.campaign_id = c.id AND NOT m.is_test) FROM campaigns c WHERE id = $1")
        .bind(campaign_id)
        .fetch_optional(db)
        .await?;
    let Some((account_id, status, started)) = row else { return Ok(true) };
    if status != "sending" {
        return Ok(true);
    }
    // `started`: recipients are already queued (a retry, or an A/B test's winner send). They're still held
    // until consent is fresh, because each message re-checks consent when it's sent.
    // An imported campaign still waiting for review never goes out.
    if let Some(notes) = super::review::pending(db, "campaign", campaign_id).await?.filter(|_| !started) {
        let msg = format!("Not sent: it was imported from Klaviyo and needs review ({}). Mark it reviewed under Settings → Move from Klaviyo.", notes.first().map(String::as_str).unwrap_or("see the notes"));
        back_to_draft(db, campaign_id, &msg).await?;
        return Ok(false);
    }
    let Some((import_id, istatus, key, fresh_at)) = active_sync(db, account_id).await? else {
        sqlx::query("DELETE FROM klaviyo_campaign_waits WHERE campaign_id = $1").bind(campaign_id).execute(db).await?;
        return Ok(true);
    };
    if fresh_at >= Utc::now() - Duration::minutes(CAMPAIGN_FRESH_MINUTES) {
        sqlx::query("DELETE FROM klaviyo_campaign_waits WHERE campaign_id = $1").bind(campaign_id).execute(db).await?;
        sqlx::query("UPDATE campaigns SET error = NULL WHERE id = $1 AND error = $2").bind(campaign_id).bind(WAIT_MESSAGE).execute(db).await?;
        return Ok(true);
    }
    let since: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO klaviyo_campaign_waits (campaign_id, account_id) VALUES ($1, $2) ON CONFLICT (campaign_id) DO UPDATE SET campaign_id = EXCLUDED.campaign_id RETURNING since",
    )
    .bind(campaign_id)
    .bind(account_id)
    .fetch_one(db)
    .await?;
    if since < Utc::now() - Duration::hours(CAMPAIGN_MAX_WAIT_HOURS) {
        if started {
            // Part of it may have gone out (an A/B test slice): stop the rest, skipping what's queued.
            crate::deliverability::stop_campaign(db, campaign_id, PAUSED_MESSAGE).await?;
        } else {
            back_to_draft(db, campaign_id, PAUSED_MESSAGE).await?;
        }
        sqlx::query("DELETE FROM klaviyo_campaign_waits WHERE campaign_id = $1").bind(campaign_id).execute(db).await?;
        return Ok(false);
    }
    kick_sync(db, import_id, &istatus, key).await?;
    sqlx::query("UPDATE campaigns SET error = $2 WHERE id = $1").bind(campaign_id).bind(WAIT_MESSAGE).execute(db).await?;
    crate::jobs::enqueue(db, "campaign_send", json!({"campaign_id": campaign_id}), Some(Utc::now() + Duration::seconds(60))).await?;
    Ok(false)
}

async fn back_to_draft(db: &PgPool, campaign_id: Uuid, msg: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE campaigns SET status = 'draft', error = $2, send_at = NULL, updated_at = now() WHERE id = $1 AND status = 'sending'").bind(campaign_id).bind(msg).execute(db).await?;
    Ok(())
}

/// Called by the flow engine before an email or text step. True: held (the run waits at this step and
/// is picked up again in 15 minutes).
pub async fn flow_send_held(state: &AppState, run_id: Uuid, account_id: Uuid, node_id: &str, context: &Value) -> anyhow::Result<bool> {
    let db = &state.db;
    let Some((import_id, status, key, fresh_at)) = active_sync(db, account_id).await? else { return Ok(false) };
    if fresh_at >= Utc::now() - Duration::hours(FLOW_MAX_AGE_HOURS) {
        return Ok(false);
    }
    kick_sync(db, import_id, &status, key).await?;
    // Wait at this very step: when the run is picked up again it sends from here, without redoing
    // the steps before it.
    let mut ctx = context.clone();
    if !ctx.is_object() {
        ctx = json!({});
    }
    ctx["_waited"] = json!(node_id);
    sqlx::query("UPDATE flow_runs SET current_node = $2, context = $3 WHERE id = $1").bind(run_id).bind(node_id).bind(&ctx).execute(db).await?;
    crate::jobs::enqueue(db, "flow_step", json!({"run_id": run_id}), Some(Utc::now() + Duration::minutes(15))).await?;
    Ok(true)
}

/// Called by a segment refresh before it starts segment-triggered flows. True: take the new members in
/// without starting runs, because event history is being imported (or was, before this segment's last
/// refresh).
pub async fn absorb_segment_entries(db: &PgPool, account_id: Uuid, previous_refresh: Option<DateTime<Utc>>) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM klaviyo_imports WHERE account_id = $1 AND history_started_at IS NOT NULL
            AND (history_done_at IS NULL OR history_done_at < history_started_at OR COALESCE($2, '-infinity') < history_done_at))",
    )
    .bind(account_id)
    .bind(previous_refresh)
    .fetch_one(db)
    .await?)
}

/// Campaigns waiting on the sync right now, for the import page.
pub async fn waiting(db: &PgPool, account_id: Uuid) -> sqlx::Result<Vec<Value>> {
    let rows: Vec<(Uuid, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT w.campaign_id, c.name, w.since FROM klaviyo_campaign_waits w JOIN campaigns c ON c.id = w.campaign_id AND c.status = 'sending' WHERE w.account_id = $1 ORDER BY w.since",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(id, name, since)| json!({"id": id, "name": name, "since": since})).collect())
}
