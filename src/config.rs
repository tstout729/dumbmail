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

    /// Hosted service: emails a new account may send in its first 24 hours. The limit then grows with
    /// clean sending (see `deliverability`).
    #[arg(long, env = "WARMUP_START_DAILY", default_value_t = 500)]
    pub warmup_start_daily: i64,

    /// Number of concurrent worker tasks.
    #[arg(long, env = "WORKERS", default_value_t = 4)]
    pub workers: usize,

    /// Stripe secret key for credit top-ups (hosted billing).
    #[arg(long, env = "STRIPE_SECRET_KEY", hide_env_values = true)]
    pub stripe_secret_key: Option<String>,
    /// Signing secret of the Stripe webhook pointed at {PUBLIC_URL}/webhooks/stripe.
    #[arg(long, env = "STRIPE_WEBHOOK_SECRET", hide_env_values = true)]
    pub stripe_webhook_secret: Option<String>,
    /// Stripe API base URL (tests point it at a local stand-in).
    #[arg(long, env = "STRIPE_API_BASE", default_value = "https://api.stripe.com/v1")]
    pub stripe_api_base: String,

    /// Customer prices in USD. Each covers the worst-case provider cost plus Stripe's fee on the
    /// smallest ($25) top-up (4.1%) and still leaves a margin.
    /// Email: SES costs $0.10 per 1,000 on the à la carte plan (the account uses it), plus about
    /// $0.02 for data, so about $0.12.
    #[arg(long, env = "PRICE_EMAIL_PER_1000", default_value_t = 0.40)]
    pub price_email_per_1000: f64,
    /// SMS per segment, carrier fees included. Telnyx toll-free: $0.0055 plus up to $0.0045 in
    /// carrier fees = $0.01 worst case, plus unbilled inbound replies. $0.015 keeps about a third
    /// as margin in that worst case.
    #[arg(long, env = "PRICE_SMS_SEGMENT", default_value_t = 0.015)]
    pub price_sms_segment: f64,
    /// MMS, carrier fees included. Telnyx toll-free: $0.016 plus up to $0.01 = $0.026 worst case.
    #[arg(long, env = "PRICE_MMS", default_value_t = 0.03)]
    pub price_mms: f64,
    /// Phone number per month (Telnyx charges about $1.00–1.10).
    #[arg(long, env = "PRICE_NUMBER_MONTHLY", default_value_t = 2.20)]
    pub price_number_monthly: f64,
    /// Free emails per account each month (costs at most ~$0.18 per account).
    #[arg(long, env = "FREE_EMAILS_PER_MONTH", default_value_t = 1000)]
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

    /// Google OAuth web client for "Continue with Google". Both unset hides the button. The client's
    /// authorized redirect URI must be PUBLIC_URL + /api/auth/google/callback.
    #[arg(long, env = "GOOGLE_CLIENT_ID")]
    pub google_client_id: Option<String>,
    #[arg(long, env = "GOOGLE_CLIENT_SECRET", hide_env_values = true)]
    pub google_client_secret: Option<String>,
    /// Google Search Console HTML-file token (e.g. google0123abcd.html), served at /<token>.
    #[arg(long, env = "GOOGLE_SITE_VERIFICATION")]
    pub google_site_verification: Option<String>,
    /// Contact address shown on the About, Privacy and Terms pages.
    #[arg(long, env = "SUPPORT_EMAIL")]
    pub support_email: Option<String>,
    /// Where operator alerts go (an account's email sending was paused for high bounces/complaints).
    /// Falls back to SUPPORT_EMAIL. The address emails itself, so its domain must be verified with the
    /// email provider. Alerts are always logged too.
    #[arg(long, env = "OPERATOR_ALERT_EMAIL")]
    pub operator_alert_email: Option<String>,
    /// Old hostnames (comma-separated) whose page visits are sent to PUBLIC_URL with a permanent
    /// redirect. Webhooks, the API, MCP, tracking links and the website snippet keep working there.
    #[arg(long, env = "REDIRECT_HOSTS", value_delimiter = ',')]
    pub redirect_hosts: Vec<String>,
    /// Base URL (no trailing slash) for tracking, unsubscribe and confirmation links in email from
    /// accounts without their own branded link domain, e.g. https://dumbmaillinks.com. Keeps
    /// PUBLIC_URL's domain out of customers' email, so one sender's spam can't get it blocklisted.
    /// Point the domain at this service before setting it. Unset uses PUBLIC_URL.
    #[arg(long, env = "LINK_URL")]
    pub link_url: Option<String>,
}

impl Config {
    pub fn public_url(&self) -> &str {
        self.public_url.trim_end_matches('/')
    }

    /// Base URL for links in email when the account has no branded link domain.
    pub fn link_url(&self) -> &str {
        self.link_url.as_deref().map(|u| u.trim().trim_end_matches('/')).filter(|u| !u.is_empty()).unwrap_or(self.public_url())
    }

    /// LINK_URL's hostname when it differs from PUBLIC_URL's.
    pub fn shared_link_host(&self) -> Option<String> {
        let host = |u: &str| url::Url::parse(u).ok().and_then(|u| u.host_str().map(str::to_lowercase));
        host(self.link_url()).filter(|h| Some(h) != host(self.public_url()).as_ref())
    }
}
