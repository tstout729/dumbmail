//! Which numbers can get RCS from an agent, as Telnyx's capability check says, kept for a bounded
//! time (Telnyx says the lookups can take seconds and recommends caching them).
//!
//! A text that finds no fresh answer asks Telnyx about its one number (bounded wait; no answer
//! means SMS this time). A campaign's numbers are also looked up in bulk, 100 at a time, by a
//! background job that starts when the campaign starts sending, so most campaign texts find their
//! answer ready.

use std::time::{Duration, Instant};

use serde_json::json;
use uuid::Uuid;

use super::settings::Active;
use super::telnyx;
use crate::error::AppResult;
use crate::state::AppState;

/// How long an answer is kept: a phone that can get RCS is asked again after three days, one that
/// can't after a day (people turn RCS on and change phones). A failed RCS falls back to SMS anyway.
pub const CAPABLE_HOURS: i32 = 72;
pub const NOT_CAPABLE_HOURS: i32 = 24;
/// Longest a single text waits on a capability lookup.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(4);
/// A campaign's bulk lookup: most numbers and longest it runs.
pub const PREFETCH_MAX_NUMBERS: i64 = 200_000;
pub const PREFETCH_DEADLINE: Duration = Duration::from_secs(15 * 60);

const FRESH: &str = "checked_at > now() - make_interval(hours => CASE WHEN capable THEN 72 ELSE 24 END)";

async fn cached(state: &AppState, account_id: Uuid, agent_id: &str, phone: &str) -> AppResult<Option<Vec<String>>> {
    let row: Option<(bool, Vec<String>)> = sqlx::query_as(&format!(
        "SELECT capable, features FROM rcs_capabilities WHERE account_id = $1 AND agent_id = $2 AND phone_number = $3 AND {FRESH}"
    ))
    .bind(account_id)
    .bind(agent_id)
    .bind(phone)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|(capable, f)| if capable { f } else { Vec::new() }))
}

async fn store(state: &AppState, account_id: Uuid, agent_id: &str, answers: &[(String, Vec<String>)]) -> AppResult<()> {
    if answers.is_empty() {
        return Ok(());
    }
    let phones: Vec<&str> = answers.iter().map(|(p, _)| p.as_str()).collect();
    let capable: Vec<bool> = answers.iter().map(|(_, f)| !f.is_empty()).collect();
    // Postgres arrays of arrays must be rectangular: features travel as one comma-joined string each.
    let features: Vec<String> = answers.iter().map(|(_, f)| f.join(",")).collect();
    sqlx::query(
        "INSERT INTO rcs_capabilities (account_id, agent_id, phone_number, capable, features, checked_at)
         SELECT $1, $2, p, c, CASE WHEN f = '' THEN '{}'::text[] ELSE string_to_array(f, ',') END, now() FROM unnest($3::text[], $4::bool[], $5::text[]) AS x(p, c, f)
         ON CONFLICT (account_id, agent_id, phone_number) DO UPDATE SET capable = EXCLUDED.capable, features = EXCLUDED.features, checked_at = now()",
    )
    .bind(account_id)
    .bind(agent_id)
    .bind(&phones)
    .bind(&capable)
    .bind(&features)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The RCS features `phone` supports from the active agent; empty when it can't get RCS or Telnyx
/// didn't answer in time (then the text goes as SMS). Never holds a database connection while
/// waiting on Telnyx.
pub async fn features(state: &AppState, account_id: Uuid, active: &Active, phone: &str) -> AppResult<Vec<String>> {
    if let Some(f) = cached(state, account_id, &active.agent_id, phone).await? {
        return Ok(f);
    }
    match telnyx::capabilities(&state.cfg, &active.creds.api_key, &active.agent_id, phone, LOOKUP_TIMEOUT).await {
        Ok(f) => {
            store(state, account_id, &active.agent_id, &[(phone.to_string(), f.clone())]).await?;
            Ok(f)
        }
        // A definite "no such number" or similar: not RCS. No answer: SMS this time, ask again next time.
        Err(e) if (400..500).contains(&e.status) && e.status != 429 && e.status != 401 && e.status != 403 => {
            store(state, account_id, &active.agent_id, &[(phone.to_string(), Vec::new())]).await?;
            Ok(Vec::new())
        }
        Err(e) => {
            tracing::warn!(account = %account_id, error = %e.describe(), "RCS capability lookup failed; sending as SMS");
            Ok(Vec::new())
        }
    }
}

/// Scheduler: start a bulk lookup for each SMS campaign that began sending from an account with
/// RCS on (once per campaign; the claim and the job commit together).
pub async fn start_prefetches(state: &AppState) -> anyhow::Result<()> {
    let mut tx = state.db.begin().await?;
    let started: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO rcs_prefetches (campaign_id, account_id)
         SELECT c.id, c.account_id FROM campaigns c JOIN rcs_settings s ON s.account_id = c.account_id AND s.enabled AND s.checked_agent_id IS NOT NULL
         WHERE c.status = 'sending' AND c.channel = 'sms' AND NOT EXISTS (SELECT 1 FROM rcs_prefetches p WHERE p.campaign_id = c.id)
         LIMIT 20
         ON CONFLICT DO NOTHING RETURNING campaign_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for id in &started {
        crate::jobs::enqueue_background(&mut *tx, "rcs_prefetch", json!({"campaign_id": id}), None).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Job: look up the campaign's queued numbers that have no fresh answer, 100 per request.
pub async fn prefetch(state: &AppState, campaign_id: Uuid) -> anyhow::Result<()> {
    let started = Instant::now();
    let Some(account_id): Option<Uuid> = sqlx::query_scalar("SELECT account_id FROM rcs_prefetches WHERE campaign_id = $1 AND done_at IS NULL").bind(campaign_id).fetch_optional(&state.db).await? else {
        return Ok(());
    };
    let Some(active) = super::settings::active(state, account_id).await.map_err(|e| anyhow::anyhow!(e.public_message()))? else {
        sqlx::query("UPDATE rcs_prefetches SET done_at = now() WHERE campaign_id = $1").bind(campaign_id).execute(&state.db).await?;
        return Ok(());
    };
    let numbers: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT m.to_address FROM messages m WHERE m.campaign_id = $1 AND m.account_id = $2 AND m.channel = 'sms' AND m.status = 'queued' AND m.to_address IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM rcs_capabilities c WHERE c.account_id = $2 AND c.agent_id = $3 AND c.phone_number = m.to_address AND {FRESH})
         LIMIT $4"
    ))
    .bind(campaign_id)
    .bind(account_id)
    .bind(&active.agent_id)
    .bind(PREFETCH_MAX_NUMBERS)
    .fetch_all(&state.db)
    .await?;
    let numbers: Vec<String> = numbers.into_iter().filter_map(|n| crate::models::normalize_phone(&n)).collect();
    let mut failures = 0;
    for chunk in numbers.chunks(telnyx::BULK_MAX) {
        if started.elapsed() > PREFETCH_DEADLINE || failures >= 3 {
            break;
        }
        match telnyx::bulk_capabilities(&state.cfg, &active.creds.api_key, &active.agent_id, chunk, Duration::from_secs(30)).await {
            Ok(answers) => {
                failures = 0;
                store(state, account_id, &active.agent_id, &answers).await.map_err(|e| anyhow::anyhow!(e.public_message()))?;
            }
            Err(e) => {
                failures += 1;
                tracing::warn!(campaign = %campaign_id, error = %e.describe(), "RCS bulk capability lookup failed");
            }
        }
    }
    sqlx::query("UPDATE rcs_prefetches SET done_at = now() WHERE campaign_id = $1").bind(campaign_id).execute(&state.db).await?;
    Ok(())
}

/// Scheduler: forget old answers, a bounded batch at a time.
pub async fn forget_old(state: &AppState) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM rcs_capabilities WHERE ctid IN (SELECT ctid FROM rcs_capabilities WHERE checked_at < now() - interval '7 days' LIMIT 5000)",
    )
    .execute(&state.db)
    .await?;
    Ok(())
}
