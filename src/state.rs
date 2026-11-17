use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;
use crate::providers::email::{EmailProvider, LogEmail, SesEmail};
use crate::providers::sms::{LogSms, SmsProvider, TelnyxSms, TwilioSms};

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

pub struct Inner {
    pub db: PgPool,
    pub cfg: Config,
    pub email: Arc<dyn EmailProvider>,
    pub sms: Arc<dyn SmsProvider>,
    pub http: reqwest::Client,
    /// Bounce/complaint rate checks put off by the per-account rate limit (see `deliverability`).
    pub pending_checks: crate::deliverability::PendingChecks,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    pub async fn build(cfg: Config, db: PgPool) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("dumbmail/0.1")
            .build()?;
        let email: Arc<dyn EmailProvider> = match cfg.email_provider.as_str() {
            "ses" => Arc::new(SesEmail::new(cfg.ses_configuration_set.clone()).await),
            "log" => Arc::new(LogEmail::new()),
            other => anyhow::bail!("unknown EMAIL_PROVIDER {other:?} (use ses or log)"),
        };
        let sms: Arc<dyn SmsProvider> = match cfg.sms_provider.as_str() {
            "telnyx" => Arc::new(TelnyxSms::new(
                http.clone(),
                cfg.telnyx_api_key.clone().ok_or_else(|| anyhow::anyhow!("TELNYX_API_KEY is required"))?,
                cfg.telnyx_messaging_profile_id.clone(),
            )),
            "twilio" => Arc::new(TwilioSms::new(
                http.clone(),
                cfg.twilio_account_sid.clone().ok_or_else(|| anyhow::anyhow!("TWILIO_ACCOUNT_SID is required"))?,
                cfg.twilio_auth_token.clone().ok_or_else(|| anyhow::anyhow!("TWILIO_AUTH_TOKEN is required"))?,
            )),
            "log" => Arc::new(LogSms::new()),
            other => anyhow::bail!("unknown SMS_PROVIDER {other:?} (use telnyx, twilio or log)"),
        };
        Ok(AppState(Arc::new(Inner { db, cfg, email, sms, http, pending_checks: Default::default() })))
    }
}
