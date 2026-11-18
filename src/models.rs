use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Account {
    pub id: Uuid,
    pub public_key: String,
    pub name: String,
    pub website: Option<String>,
    pub from_name: Option<String>,
    pub from_email: Option<String>,
    pub reply_to: Option<String>,
    pub sms_from: Option<String>,
    pub timezone: String,
    pub address: Option<String>,
    pub brand_color: String,
    pub logo_url: Option<String>,
    /// Social profile links by network (instagram, facebook, tiktok, x, youtube, pinterest, linkedin, threads).
    pub social_links: Value,
    pub plan: String,
    pub onboarding: Value,
    pub conversion_metric: String,
    pub created_at: DateTime<Utc>,
    /// Set when email sending was paused automatically for high bounces or spam complaints (see
    /// `deliverability`); campaigns, flows and tests send no email until an operator lifts it.
    pub sending_paused_at: Option<DateTime<Utc>>,
    pub sending_paused_reason: Option<String>,
    /// Operator-set 24-hour email limit in place of the automatic warm-up (see `deliverability`).
    #[serde(skip)]
    pub email_daily_limit_override: Option<i32>,
    /// Campaign email skips people who haven't engaged in months (see `deliverability::SUNSET_DAYS`).
    pub email_sunset_enabled: bool,
    #[serde(skip)]
    pub link_domain: Option<String>,
    #[serde(skip)]
    pub link_domain_status: String,
    #[serde(skip)]
    pub link_domain_records: Value,
    /// Sending domains this account has verified (filled in by `load_account`; empty elsewhere).
    #[serde(skip)]
    #[sqlx(default)]
    pub verified_domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Profile {
    pub id: Uuid,
    #[serde(skip_serializing)]
    #[serde(default)]
    pub account_id: Uuid,
    pub email: Option<String>,
    pub phone_number: Option<String>,
    pub external_id: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub properties: Value,
    pub email_consent: String,
    pub sms_consent: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_event_at: Option<DateTime<Utc>>,
}

pub const PROFILE_COLS: &str = "p.id, p.account_id, p.email, p.phone_number, p.external_id, p.first_name, p.last_name, p.properties, p.email_consent, p.sms_consent, p.created_at, p.updated_at, p.last_event_at";

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct List {
    pub id: Uuid,
    pub name: String,
    pub double_opt_in: bool,
    /// Listed on the preference page people reach from the unsubscribe link.
    pub show_on_preferences: bool,
    /// Shown under the list's name on the preference page.
    pub description: String,
    pub member_count: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Segment {
    pub id: Uuid,
    pub name: String,
    pub definition: Value,
    pub member_count: i32,
    pub refreshed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Template {
    pub id: Uuid,
    pub name: String,
    pub channel: String,
    pub content: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct CampaignRow {
    pub id: Uuid,
    pub account_id: Uuid,
    pub name: String,
    pub channel: String,
    pub status: String,
    pub audience: Value,
    pub content: Value,
    pub send_at: Option<DateTime<Utc>>,
    pub sent_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// A/B test settings and the extra versions (version A is `content`); see `campaigns::AbTest`.
    pub ab_test: Option<Value>,
    /// When the test slice's results are read and the winner goes to everyone else.
    pub ab_test_ends_at: Option<DateTime<Utc>>,
    pub ab_winner: Option<String>,
    /// When each recipient's message goes out and smart sending; see `features::sendtime::SendOptions`.
    #[sqlx(default)]
    pub send_options: Option<Value>,
    /// Imported (from Klaviyo) and needing a person's review before it can be sent; see `features::klaviyo_import::review`.
    #[sqlx(default)]
    pub import_review: Option<Value>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct FlowRow {
    pub id: Uuid,
    pub account_id: Uuid,
    pub name: String,
    pub status: String,
    pub trigger: Value,
    pub definition: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Imported and needing review before it can go live; see `features::klaviyo_import::review`.
    #[sqlx(default)]
    pub import_review: Option<Value>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct FormRow {
    pub id: Uuid,
    #[serde(skip)]
    pub account_id: Uuid,
    pub name: String,
    pub status: String,
    pub kind: String,
    pub list_id: Option<Uuid>,
    pub config: Value,
    pub views: i64,
    pub submits: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Imported without a sign-up list it could be sure of; can't be published until a list is chosen.
    #[sqlx(default)]
    pub import_review: Option<Value>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct EventRow {
    pub id: Uuid,
    pub metric: String,
    pub profile_id: Uuid,
    pub properties: Value,
    pub value: Option<f64>,
    pub occurred_at: DateTime<Utc>,
    /// Email, phone or name of the person, where the query joins profiles (the events list).
    #[sqlx(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_label: Option<String>,
}

/// Aggregated delivery and engagement numbers for a campaign, flow or flow node.
#[derive(Debug, Clone, Default, Serialize, FromRow)]
pub struct MessageStats {
    pub recipients: i64,
    pub delivered: i64,
    pub bounced: i64,
    pub failed: i64,
    pub unique_opens: i64,
    pub unique_clicks: i64,
    pub unsubscribes: i64,
    pub complaints: i64,
    #[sqlx(skip)]
    pub open_rate: f64,
    #[sqlx(skip)]
    pub click_rate: f64,
    #[sqlx(default)]
    pub orders: i64,
    #[sqlx(default)]
    pub revenue: f64,
}

impl MessageStats {
    pub fn finish(mut self) -> Self {
        let base = self.delivered.max(1) as f64;
        if self.delivered > 0 {
            self.open_rate = self.unique_opens as f64 / base;
            self.click_rate = self.unique_clicks as f64 / base;
        }
        self
    }
}

/// SQL aggregate producing the MessageStats columns over a `messages m` relation.
pub const STATS_SELECT: &str = "count(*)::bigint AS recipients,
    count(*) FILTER (WHERE m.status IN ('sent','delivered') OR m.opened_at IS NOT NULL)::bigint AS delivered,
    count(*) FILTER (WHERE m.status = 'bounced')::bigint AS bounced,
    count(*) FILTER (WHERE m.status = 'failed')::bigint AS failed,
    count(m.opened_at)::bigint AS unique_opens,
    count(m.clicked_at)::bigint AS unique_clicks,
    count(m.unsubscribed_at)::bigint AS unsubscribes,
    count(*) FILTER (WHERE m.status = 'complained')::bigint AS complaints";

pub fn normalize_email(e: &str) -> Option<String> {
    let e = e.trim().to_lowercase();
    let (local, domain) = e.split_once('@')?;
    if local.is_empty() || !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') || e.contains(' ') {
        return None;
    }
    Some(e)
}

/// Normalize to E.164. Ten-digit numbers are treated as US/Canada.
pub fn normalize_phone(p: &str) -> Option<String> {
    let trimmed = p.trim();
    let plus = trimmed.starts_with('+');
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    let e164 = if plus {
        format!("+{digits}")
    } else if digits.len() == 10 {
        format!("+1{digits}")
    } else if digits.len() == 11 && digits.starts_with('1') {
        format!("+{digits}")
    } else {
        return None;
    };
    if (8..=16).contains(&e164.len()) {
        Some(e164)
    } else {
        None
    }
}

/// `pending`: signed up through a double opt-in form and hasn't confirmed yet; gets no email but the confirmation.
pub fn valid_consent(s: &str) -> bool {
    matches!(s, "subscribed" | "unsubscribed" | "never_subscribed" | "pending")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize_email(" Ann@Example.COM ").as_deref(), Some("ann@example.com"));
        assert!(normalize_email("nope").is_none());
        assert!(normalize_email("a@b").is_none());
        assert_eq!(normalize_phone("(503) 555-0100").as_deref(), Some("+15035550100"));
        assert_eq!(normalize_phone("+44 20 7946 0958").as_deref(), Some("+442079460958"));
        assert!(normalize_phone("123").is_none());
    }
}
