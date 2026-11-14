use clap::Args;

/// Runtime configuration, read from flags or environment variables.
#[derive(Args, Debug, Clone)]
pub struct Config {
    /// Postgres connection string.
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,

    /// Address to listen on.
    #[arg(long, env = "BIND", default_value = "0.0.0.0:8080")]
    pub bind: String,

    /// Public base URL used for tracking links, unsubscribe links and forms (no trailing slash).
    #[arg(long, env = "PUBLIC_URL", default_value = "http://localhost:8080")]
    pub public_url: String,

    /// Secret used to sign tracking and unsubscribe tokens. Use 32+ random bytes in production.
    #[arg(long, env = "SECRET_KEY", default_value = "dev-secret-change-me")]
    pub secret_key: String,

    /// Email provider: `ses` (Amazon SES v2) or `log` (prints instead of sending).
    #[arg(long, env = "EMAIL_PROVIDER", default_value = "log")]
    pub email_provider: String,

    /// SES configuration set that publishes bounce/complaint/delivery events to SNS.
    #[arg(long, env = "SES_CONFIGURATION_SET")]
    pub ses_configuration_set: Option<String>,

    /// SMS provider: `telnyx`, `twilio` or `log`.
    #[arg(long, env = "SMS_PROVIDER", default_value = "log")]
    pub sms_provider: String,

    #[arg(long, env = "TELNYX_API_KEY", hide_env_values = true)]
    pub telnyx_api_key: Option<String>,
    #[arg(long, env = "TELNYX_MESSAGING_PROFILE_ID")]
    pub telnyx_messaging_profile_id: Option<String>,
    #[arg(long, env = "TWILIO_ACCOUNT_SID")]
    pub twilio_account_sid: Option<String>,
    #[arg(long, env = "TWILIO_AUTH_TOKEN", hide_env_values = true)]
    pub twilio_auth_token: Option<String>,

    /// Secret path segment for provider webhooks: /webhooks/{provider}/{secret}.
    #[arg(long, env = "WEBHOOK_SECRET", default_value = "dev-webhook-secret")]
    pub webhook_secret: String,

    /// Enforce plan limits (hosted service). Self-hosted installs leave this off.
    #[arg(long, env = "HOSTED", default_value_t = false)]
    pub hosted: bool,

    /// Allow anyone to sign up. Self-hosters usually turn this off after creating their account.
    #[arg(long, env = "ALLOW_SIGNUP", default_value_t = true, action = clap::ArgAction::Set)]
    pub allow_signup: bool,

    /// Maximum messages per second per channel (SES starts new accounts at 14/s).
    #[arg(long, env = "SEND_RATE_PER_SEC", default_value_t = 14)]
    pub send_rate_per_sec: u32,

    /// Number of concurrent worker tasks.
    #[arg(long, env = "WORKERS", default_value_t = 4)]
    pub workers: usize,

    /// Stripe secret key for credit top-ups (hosted billing).
    #[arg(long, env = "STRIPE_SECRET_KEY", hide_env_values = true)]
    pub stripe_secret_key: Option<String>,
    /// Signing secret of the Stripe webhook pointed at {PUBLIC_URL}/webhooks/stripe.
    #[arg(long, env = "STRIPE_WEBHOOK_SECRET", hide_env_values = true)]
    pub stripe_webhook_secret: Option<String>,

    /// Customer prices in USD (cost plus markup). Email: SES costs $0.10 per 1,000.
    #[arg(long, env = "PRICE_EMAIL_PER_1000", default_value_t = 0.20)]
    pub price_email_per_1000: f64,
    /// SMS per segment, carrier fees included (Telnyx costs about $0.0083 all-in).
    #[arg(long, env = "PRICE_SMS_SEGMENT", default_value_t = 0.0099)]
    pub price_sms_segment: f64,
    #[arg(long, env = "PRICE_MMS", default_value_t = 0.025)]
    pub price_mms: f64,
    /// Phone number per month (Telnyx charges $1.10).
    #[arg(long, env = "PRICE_NUMBER_MONTHLY", default_value_t = 2.20)]
    pub price_number_monthly: f64,
    #[arg(long, env = "FREE_EMAILS_PER_MONTH", default_value_t = 3000)]
    pub free_emails_per_month: i64,

    /// AWS region used for SES (also determines the custom MAIL FROM MX host).
    #[arg(long, env = "AWS_REGION", default_value = "us-east-1")]
    pub aws_region: String,

    /// Railway API token: lets customers add branded link domains with automatic TLS.
    #[arg(long, env = "RAILWAY_API_TOKEN", hide_env_values = true)]
    pub railway_api_token: Option<String>,
    #[arg(long, env = "RAILWAY_PROJECT_ID")]
    pub railway_project_id: Option<String>,
    #[arg(long, env = "RAILWAY_ENVIRONMENT_ID")]
    pub railway_environment_id: Option<String>,
    #[arg(long, env = "RAILWAY_SERVICE_ID")]
    pub railway_service_id: Option<String>,

    /// Operator token for /admin endpoints (32+ characters). Unset disables them.
    #[arg(long, env = "ADMIN_TOKEN", hide_env_values = true)]
    pub admin_token: Option<String>,

    /// Serve cookies with the Secure flag (turn on behind HTTPS).
    #[arg(long, env = "SECURE_COOKIES", default_value_t = false)]
    pub secure_cookies: bool,
}

impl Config {
    pub fn public_url(&self) -> &str {
        self.public_url.trim_end_matches('/')
    }
}
