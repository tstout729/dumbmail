//! Hosted plan limits. Self-hosted installs (HOSTED=false) are unlimited.
//! Pricing is not final; these limits are placeholders until the owner sets them.

use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Limits {
    pub profiles: i64,
    pub emails_per_month: i64,
    pub sms_per_month: i64,
}

pub fn limits(plan: &str) -> Limits {
    match plan {
        "free" => Limits { profiles: 1_000, emails_per_month: 5_000, sms_per_month: 0 },
        "pro" => Limits { profiles: 25_000, emails_per_month: 150_000, sms_per_month: 2_500 },
        "scale" => Limits { profiles: 250_000, emails_per_month: 1_500_000, sms_per_month: 25_000 },
        _ => Limits { profiles: i64::MAX, emails_per_month: i64::MAX, sms_per_month: i64::MAX },
    }
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub profiles: i64,
    pub emails_this_month: i64,
    pub sms_this_month: i64,
}

/// Usage in the current calendar month in the account's time zone.
pub async fn usage(db: &PgPool, account_id: Uuid) -> AppResult<Usage> {
    let (profiles, emails, sms): (i64, i64, i64) = sqlx::query_as(
        "WITH a AS (SELECT timezone FROM accounts WHERE id = $1),
              start AS (SELECT (date_trunc('month', now() AT TIME ZONE a.timezone) AT TIME ZONE a.timezone) AS t FROM a)
         SELECT (SELECT count(*) FROM profiles WHERE account_id = $1),
                (SELECT count(*) FROM messages, start WHERE account_id = $1 AND channel = 'email' AND NOT is_test AND status IN ('sent','delivered','bounced','complained') AND created_at >= start.t),
                (SELECT count(*) FROM messages, start WHERE account_id = $1 AND channel = 'sms' AND NOT is_test AND status IN ('sent','delivered') AND created_at >= start.t)",
    )
    .bind(account_id)
    .fetch_one(db)
    .await?;
    Ok(Usage { profiles, emails_this_month: emails, sms_this_month: sms })
}

pub async fn check_send(db: &PgPool, hosted: bool, account_id: Uuid, plan: &str, channel: &str, recipients: i64) -> AppResult<()> {
    if !hosted {
        return Ok(());
    }
    let l = limits(plan);
    let u = usage(db, account_id).await?;
    let (used, cap, what) = if channel == "sms" {
        (u.sms_this_month, l.sms_per_month, "SMS messages")
    } else {
        (u.emails_this_month, l.emails_per_month, "emails")
    };
    if used + recipients > cap {
        return Err(AppError::LimitReached(format!(
            "This send needs {recipients} {what} but your plan has {} left this month. Upgrade in Settings → Billing.",
            (cap - used).max(0)
        )));
    }
    Ok(())
}
