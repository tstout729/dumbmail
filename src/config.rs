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

    /// Serve cookies with the Secure flag (turn on behind HTTPS).
    #[arg(long, env = "SECURE_COOKIES", default_value_t = false)]
    pub secure_cookies: bool,
}

impl Config {
    pub fn public_url(&self) -> &str {
        self.public_url.trim_end_matches('/')
    }
}
