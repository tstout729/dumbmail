//! A campaign's pre-send check for coupons. It blocks only what can't work: a coupon that doesn't
//! exist or has expired, or content that uses coupons wrongly. Whether the pools cover the audience is
//! a warning with the numbers: the audience and the stock can still change before the send, and a
//! send that runs out waits for codes instead of going out without them.
//!
//! Each A/B version is checked against its own share: a version sends to its part of the test slice,
//! and could win the rest of the audience.

use std::collections::BTreeMap;

use sqlx::QueryBuilder;
use uuid::Uuid;

use super::store;
use super::template;
use crate::campaigns::{self, Audience};
use crate::error::AppResult;
use crate::models::CampaignRow;
use crate::state::AppState;

#[derive(Debug, Default)]
pub struct Check {
    /// Stop the send.
    pub problems: Vec<String>,
    /// Worth knowing before sending.
    pub warnings: Vec<String>,
}

/// Problems that stop this campaign because of its coupons (for the launch check).
pub async fn campaign_problems(state: &AppState, account_id: Uuid, c: &CampaignRow) -> AppResult<Vec<String>> {
    Ok(campaign_check(state, account_id, c).await?.problems)
}

/// The share of the audience each version may reach, as (version content, fraction of the audience).
fn version_shares(c: &CampaignRow, total: i64) -> Vec<(serde_json::Value, f64)> {
    let Some(t) = campaigns::ab_test(c) else { return vec![(c.content.clone(), 1.0)] };
    let versions = t.versions();
    let k = versions.len().max(1) as i64;
    let test = if t.test_percent >= 100 { total } else { ((total * t.test_percent + 99) / 100).max(k).min(total) };
    let per_version = (test + k - 1) / k;
    // After the test, the winner (any version) sends to everyone else.
    let rest = (total - test).max(0);
    let share = if total > 0 { (per_version + rest) as f64 / total as f64 } else { 0.0 };
    versions
        .iter()
        .filter_map(|v| campaigns::version_content(c, Some(&t), Some(v.as_str())).cloned())
        .map(|content| (content, share.min(1.0)))
        .collect()
}

/// Every coupon problem and warning for a campaign.
pub async fn campaign_check(state: &AppState, account_id: Uuid, c: &CampaignRow) -> AppResult<Check> {
    let mut out = Check::default();
    let audience: Audience = serde_json::from_value(c.audience.clone()).unwrap_or_default();
    let total = if audience.include.is_empty() { 0 } else { campaigns::estimate(&state.db, account_id, &c.channel, &audience).await.unwrap_or(0) };
    // Coupon name -> the share of the audience that may need its codes (summed over the versions using it,
    // counting the winner's remainder once).
    let mut needs: BTreeMap<String, f64> = BTreeMap::new();
    let shares = version_shares(c, total);
    let winner_rest = if shares.len() > 1 {
        let t = campaigns::ab_test(c);
        let k = shares.len() as i64;
        let test = t.map(|t| if t.test_percent >= 100 { total } else { ((total * t.test_percent + 99) / 100).max(k).min(total) }).unwrap_or(total);
        if total > 0 { (total - test).max(0) as f64 / total as f64 } else { 0.0 }
    } else {
        0.0
    };
    for (content, share) in &shares {
        if !template::may_use_coupons(content) {
            continue;
        }
        let names = match template::check_content(&c.channel, content).and_then(|_| template::message_names(&c.channel, content)) {
            Ok(n) => n,
            Err(e) => {
                out.problems.push(e.public_message());
                continue;
            }
        };
        for n in names {
            let e = needs.entry(n).or_insert(0.0);
            // The remainder goes to one winner only, so it's counted once per coupon.
            *e = if *e == 0.0 { *share } else { (*e + share - winner_rest).min(1.0) };
        }
    }
    for (name, share) in needs {
        let Some(coupon) = store::by_name(&state.db, account_id, &name).await? else {
            out.problems.push(format!("There's no coupon named {name:?}: create it under Content → Coupons or fix the message"));
            continue;
        };
        if coupon.expires_at.is_some_and(|t| t <= chrono::Utc::now()) {
            out.problems.push(format!("Coupon {:?} has expired: change its expiry or use another coupon", coupon.name));
            continue;
        }
        if total == 0 {
            continue;
        }
        // People in the audience who'd need a new code: everyone for "new code each time", else those without one.
        let without = if coupon.new_code_each_time {
            total
        } else {
            let mut qb = QueryBuilder::new("SELECT count(*)");
            campaigns::append_audience(&mut qb, &state.db, account_id, &c.channel, &audience).await?;
            qb.push(" AND NOT EXISTS (SELECT 1 FROM coupon_codes cc WHERE cc.coupon_id = ").push_bind(coupon.id).push(" AND cc.profile_id = p.id AND cc.send_key IS NULL)");
            qb.build_query_scalar().fetch_one(&state.db).await?
        };
        let need = (without as f64 * share).ceil() as i64;
        let available = store::stock(&state.db, &coupon).await?["available"].as_i64().unwrap_or(0);
        if need > available {
            out.warnings.push(format!(
                "Coupon {:?} has {available} codes left and this campaign may need up to {need}. Add codes before sending, or messages wait when it runs out",
                coupon.name
            ));
        }
    }
    Ok(out)
}
