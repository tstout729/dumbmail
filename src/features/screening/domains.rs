//! Sending and link domains, checked when they're added, before anything is created with the email
//! provider or the host: a name that impersonates a brand is refused; a new or random-looking one
//! is noted for a person.

use serde_json::json;
use uuid::Uuid;

use super::fetch;
use super::rules;
use super::standing::{self, New, Standing};
use super::Verdict;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Check `domain` (already cleaned) before the account adds it as its `what` ("sending" or
/// "link") domain. Err: refused (the account's standing changes as docs/SCREENING.md says).
pub async fn check_domain(state: &AppState, account_id: Uuid, domain: &str, what: &'static str) -> AppResult<()> {
    if !super::on(state) {
        return Ok(());
    }
    let website: Option<String> = sqlx::query_scalar("SELECT website FROM accounts WHERE id = $1").bind(account_id).fetch_optional(&state.db).await?.flatten();
    let site_host = website.as_deref().and_then(fetch::website_host);
    let mut reasons = Vec::new();
    let mut verdict = Verdict::Pass;
    if let Some(brand) = rules::impersonated_brand(domain, site_host.as_deref()) {
        verdict = Verdict::Block;
        reasons.push(format!("{domain} looks like it impersonates {brand}. Use a domain that belongs to your own business."));
    }
    let mut age_days = None;
    if verdict != Verdict::Block {
        if rules::looks_random(domain) {
            verdict = Verdict::Review;
            reasons.push(format!("{domain} reads like random characters."));
        }
        if let Some(at) = fetch::registered_at(domain).await {
            let days = (chrono::Utc::now() - at).num_days();
            age_days = Some(days);
            if days < super::account::NEW_DOMAIN_DAYS {
                verdict = Verdict::Review;
                reasons.push(format!("{domain} was registered {days} days ago."));
            }
        }
    }
    if verdict == Verdict::Pass {
        return Ok(());
    }
    let signals = json!({"domain": domain, "use": what, "registered_days_ago": age_days, "website": site_host});
    let mut s = New::rules("domain", verdict, &reasons, signals);
    s.subject = Some("domain");
    s.subject_name = Some(domain.to_string());
    standing::record(&state.db, account_id, s).await?;
    if verdict == Verdict::Review {
        // A pending account's check sees it; an approved account keeps sending.
        return Ok(());
    }
    let why = format!("The {what} domain {domain} was refused: {}", reasons.join(" "));
    standing::move_to(state, account_id, &[Standing::Pending], Standing::Blocked, &reasons.join(" "), &why).await.map_err(AppError::Internal)?;
    standing::move_to(state, account_id, &[Standing::Approved], Standing::Review, "A domain you added needs a person's check.", &why).await.map_err(AppError::Internal)?;
    Err(AppError::bad(reasons.join(" ")))
}
