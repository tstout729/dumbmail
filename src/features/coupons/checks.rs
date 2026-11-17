//! A campaign's pre-send check: every coupon it uses exists, hasn't expired and has enough codes
//! for the people who don't have one yet.

use sqlx::QueryBuilder;
use uuid::Uuid;

use super::store;
use super::template;
use crate::campaigns::{self, Audience};
use crate::error::AppResult;
use crate::models::CampaignRow;
use crate::state::AppState;

/// Problems that would stop or hold this campaign because of its coupons. Empty when it's fine.
pub async fn campaign_problems(state: &AppState, account_id: Uuid, c: &CampaignRow) -> AppResult<Vec<String>> {
    // Every A/B version's coupons too.
    let mut names = template::coupon_names(&c.content);
    for v in campaigns::ab_test(c).map(|t| t.variants).unwrap_or_default() {
        names.extend(template::coupon_names(&v.content));
    }
    problems_for(state, account_id, c, names).await
}

async fn problems_for(state: &AppState, account_id: Uuid, c: &CampaignRow, names: std::collections::BTreeSet<String>) -> AppResult<Vec<String>> {
    let mut out = Vec::new();
    if names.is_empty() {
        return Ok(out);
    }
    if names.len() > template::MAX_PER_MESSAGE {
        out.push(format!("A message can use at most {} coupons", template::MAX_PER_MESSAGE));
        return Ok(out);
    }
    let audience: Audience = serde_json::from_value(c.audience.clone()).unwrap_or_default();
    for name in names {
        let Some(coupon) = store::by_name(&state.db, account_id, &name).await? else {
            out.push(format!("There's no coupon named {name:?}: create it under Content → Coupons or fix the message"));
            continue;
        };
        if coupon.expires_at.is_some_and(|t| t <= chrono::Utc::now()) {
            out.push(format!("Coupon {:?} has expired: change its expiry or use another coupon", coupon.name));
            continue;
        }
        if audience.include.is_empty() {
            continue;
        }
        // People in the audience who'd need a new code: everyone for "new code each time", else those without one.
        let mut qb = QueryBuilder::new("SELECT count(*)");
        campaigns::append_audience(&mut qb, &state.db, account_id, &c.channel, &audience).await?;
        if !coupon.new_code_each_time {
            qb.push(" AND NOT EXISTS (SELECT 1 FROM coupon_codes cc WHERE cc.coupon_id = ").push_bind(coupon.id).push(" AND cc.assign_key = p.id::text)");
        }
        let need: i64 = qb.build_query_scalar().fetch_one(&state.db).await?;
        let available = store::stock(&state.db, &coupon).await?["available"].as_i64().unwrap_or(0);
        if need > available {
            out.push(format!(
                "Coupon {:?} has {available} codes left but this campaign may need {need}. Add codes first, or the campaign pauses when they run out",
                coupon.name
            ));
        }
    }
    Ok(out)
}
