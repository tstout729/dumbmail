//! The `review` segment condition: has reviewed (a product, a rating range, at least N reviews, in
//! a time window), or has a review link waiting. Compiled to bound SQL like the rest of
//! `segments`. Reviews still waiting for their email confirmation don't count; reviews the author
//! deleted do (they did review it).

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, QueryBuilder};

use crate::error::{AppError, AppResult};
use crate::segments::HasOp;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAction {
    /// Wrote a review.
    #[default]
    Reviewed,
    /// Has a review link that is still open (not used, not expired).
    PendingRequest,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewCondition {
    #[serde(default)]
    pub action: ReviewAction,
    #[serde(default)]
    pub op: HasOp,
    /// The store's product id (or listing or service key).
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub min_rating: Option<i16>,
    #[serde(default)]
    pub max_rating: Option<i16>,
    /// At least this many (default 1).
    #[serde(default)]
    pub count: Option<i64>,
    #[serde(default)]
    pub within_days: Option<i64>,
}

pub fn validate(c: &ReviewCondition) -> AppResult<()> {
    let ok_rating = |r: Option<i16>| r.is_none_or(|r| (1..=5).contains(&r));
    if !ok_rating(c.min_rating) || !ok_rating(c.max_rating) {
        return Err(AppError::bad("a review condition's ratings must be 1 to 5"));
    }
    if let (Some(a), Some(b)) = (c.min_rating, c.max_rating) {
        if a > b {
            return Err(AppError::bad("a review condition's min_rating can't be above its max_rating"));
        }
    }
    if c.count.is_some_and(|n| !(1..=10_000).contains(&n)) {
        return Err(AppError::bad("a review condition's count must be 1 to 10000"));
    }
    if c.within_days.is_some_and(|d| !(1..=3650).contains(&d)) {
        return Err(AppError::bad("a review condition's within_days must be 1 to 3650"));
    }
    if c.product_id.as_deref().is_some_and(|p| p.trim().is_empty() || p.len() > 255) {
        return Err(AppError::bad("a review condition's product_id must be 1 to 255 characters (or left out)"));
    }
    if c.action == ReviewAction::PendingRequest && (c.min_rating.is_some() || c.max_rating.is_some()) {
        return Err(AppError::bad("a pending review request has no rating; leave min_rating and max_rating out"));
    }
    Ok(())
}

pub fn push(qb: &mut QueryBuilder<'_, Postgres>, c: &ReviewCondition) {
    qb.push("(");
    if c.op == HasOp::HasNot {
        qb.push("NOT ");
    }
    match c.action {
        ReviewAction::Reviewed => {
            qb.push("((SELECT count(*) FROM reviews rv WHERE rv.profile_id = p.id AND rv.account_id = p.account_id AND rv.confirmed_at IS NOT NULL");
            if let Some(min) = c.min_rating {
                qb.push(" AND rv.rating >= ").push_bind(min);
            }
            if let Some(max) = c.max_rating {
                qb.push(" AND rv.rating <= ").push_bind(max);
            }
            if let Some(pid) = &c.product_id {
                qb.push(" AND rv.subject_id IN (SELECT s.id FROM review_subjects s WHERE s.account_id = p.account_id AND s.external_id = ").push_bind(pid.trim().to_string()).push(")");
            }
            if let Some(d) = c.within_days {
                qb.push(" AND rv.created_at >= now() - make_interval(days => ").push_bind(d as i32).push(")");
            }
            qb.push(") >= ").push_bind(c.count.unwrap_or(1)).push(")");
        }
        ReviewAction::PendingRequest => {
            qb.push("EXISTS (SELECT 1 FROM review_requests rq WHERE rq.profile_id = p.id AND rq.account_id = p.account_id AND rq.review_id IS NULL AND rq.expires_at > now()");
            if let Some(pid) = &c.product_id {
                qb.push(" AND rq.subject_id IN (SELECT s.id FROM review_subjects s WHERE s.account_id = p.account_id AND s.external_id = ").push_bind(pid.trim().to_string()).push(")");
            }
            if let Some(d) = c.within_days {
                qb.push(" AND rq.created_at >= now() - make_interval(days => ").push_bind(d as i32).push(")");
            }
            qb.push(")");
        }
    }
    qb.push(")");
}
