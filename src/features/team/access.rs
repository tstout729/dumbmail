//! Roles, permissions and API key scopes, checked in one place.
//!
//! Every route the app serves is checked by [`super::guard::layer`], which runs for every request
//! that matches a route, keyed on the matched route and method, whether or not the handler reads
//! [`Ctx`]. The route's rule comes from [`PUBLIC_ROUTES`] (no sign-in), [`CORE_ROUTES`] or a
//! feature's [`crate::features::Feature::access`]. A route with no rule is refused. Requests that
//! match no route at all go to the dashboard's static files (`web::serve`), which only serves the
//! built dashboard and answers 404 for anything under `/api/`.
//!
//! Every MCP tool is checked the same way by [`authorize_tool`] before it runs: its rule comes from
//! [`core_tool_perm`] or the feature's `access().tools`, and a tool with no rule is refused.

use axum::http::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    Manager,
    Analyst,
    Content,
}

impl Role {
    pub const ALL: [Role; 5] = [Role::Owner, Role::Admin, Role::Manager, Role::Analyst, Role::Content];

    pub fn parse(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "owner" => Some(Role::Owner),
            "admin" => Some(Role::Admin),
            "manager" => Some(Role::Manager),
            "analyst" => Some(Role::Analyst),
            "content" => Some(Role::Content),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Manager => "manager",
            Role::Analyst => "analyst",
            Role::Content => "content",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Owner => "Owner",
            Role::Admin => "Admin",
            Role::Manager => "Manager",
            Role::Analyst => "Analyst",
            Role::Content => "Content",
        }
    }

    /// Whether this role holds `perm`.
    pub fn has(self, perm: Perm) -> bool {
        use Perm::*;
        match perm {
            Public | Me | Mcp | OwnLogin | TwoFactorStep | SwitchAccount | ConfirmIdentity => true,
            _ => match self {
                Role::Owner => true,
                Role::Admin => perm != Ownership,
                Role::Manager => matches!(
                    perm,
                    AudienceRead
                        | ProfilesRead
                        | AudienceWrite
                        | Export
                        | MessagingRead
                        | CampaignDraft
                        | TestSend
                        | CampaignManage
                        | CampaignSend
                        | FlowEdit
                        | FlowLive
                        | TransactionalSend
                        | ReportsManage
                        | CatalogWrite
                        | ContentRead
                        | ContentWrite
                        | FormsWrite
                        | AnalyticsRead
                        | AccountRead
                        | SettingsWrite
                        | TeamRead
                        | WebhooksRead
                        | IntegrationsRead
                ),
                Role::Analyst => matches!(perm, AudienceRead | ProfilesRead | Export | MessagingRead | ContentRead | AnalyticsRead | AccountRead | TeamRead),
                // Content may send test emails, but only to team members (see `super::sends`).
                Role::Content => matches!(perm, AudienceRead | MessagingRead | CampaignDraft | TestSend | ContentRead | ContentWrite | AnalyticsRead | AccountRead | TeamRead),
            },
        }
    }
}

/// The areas API key scopes are given by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    Audience,
    Messaging,
    Content,
    Analytics,
    Account,
}

impl Area {
    pub const ALL: [Area; 5] = [Area::Audience, Area::Messaging, Area::Content, Area::Analytics, Area::Account];

    pub fn as_str(self) -> &'static str {
        match self {
            Area::Audience => "audience",
            Area::Messaging => "messaging",
            Area::Content => "content",
            Area::Analytics => "analytics",
            Area::Account => "account",
        }
    }
}

/// What a route or MCP tool needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Perm {
    /// No sign-in (public pages, webhooks, sign-in itself). Declare these with [`public`].
    Public,
    /// Anyone signed in, including sessions still waiting on two-factor or SSO, and any API key.
    Me,
    /// The MCP endpoint. Each tool is checked on its own.
    Mcp,
    /// Your own login (sessions, password, two-factor). Browser sessions only.
    OwnLogin,
    /// Finishing sign-in: entering a two-factor code or setting one up. Browser sessions only, and
    /// allowed before two-factor is done.
    TwoFactorStep,
    /// Listing and opening the accounts this login belongs to. Browser sessions only (not single
    /// sign-on ones), allowed before two-factor is done.
    SwitchAccount,
    /// Re-entering your password or a code to confirm it's you. Browser sessions only.
    ConfirmIdentity,
    /// Lists, segments and metric names (no personal data).
    AudienceRead,
    /// Profiles, events and segment samples (personal data).
    ProfilesRead,
    AudienceWrite,
    /// Exports of profiles or reports.
    Export,
    MessagingRead,
    /// Create and edit campaign drafts.
    CampaignDraft,
    /// Send a test of a campaign. People who can't send campaigns may only send test emails to
    /// the account's own members.
    TestSend,
    /// Cancel, unschedule and delete campaigns.
    CampaignManage,
    /// Send or schedule a campaign, or pick an A/B winner.
    CampaignSend,
    FlowEdit,
    /// Turn a flow live (or pause or draft it).
    FlowLive,
    /// Send transactional messages (receipts, password resets) through the API. API keys need the
    /// `transactional.send` scope (or `full`). See [`may_send_transactional`].
    TransactionalSend,
    ContentRead,
    ContentWrite,
    FormsWrite,
    AnalyticsRead,
    /// Save, schedule and delete reports, and change attribution settings.
    ReportsManage,
    /// Add and change products, product feeds and back-in-stock and price-drop settings.
    CatalogWrite,
    /// Delete products and product feeds.
    CatalogDelete,
    AccountRead,
    SettingsWrite,
    /// Sending domains, link domain, phone numbers and SMS providers.
    Domains,
    BillingRead,
    BillingWrite,
    /// Who's on the team: names, emails and roles.
    TeamRead,
    /// The team's security details: open invites, sign-in activity, two-factor and single sign-on
    /// status, and the account's security settings. Admins and owners.
    TeamSecurityRead,
    TeamManage,
    ApiKeys,
    /// The account's two-factor requirement. Browser sessions only.
    Security,
    AuditRead,
    /// Bring in a whole customer list and its consent from another platform (connect it, start, pause,
    /// resume or cancel an import, choose how people who never subscribed are treated). Admins and owners.
    Import,
    /// Owners only, in a browser: single sign-on settings. Ownership changes also need a fresh
    /// password or code (see [`require_fresh_sign_in`]).
    Ownership,
    /// See webhook endpoints and their delivery log, without payloads. Managers and up.
    WebhooksRead,
    /// Create, change and delete webhook endpoints and REST hooks, rotate their secrets, send
    /// tests, read delivery payloads (personal data) and resend. Admins and owners.
    WebhooksManage,
    /// Everything about data export destinations and runs (they copy every profile out). Admins
    /// and owners.
    ExportsManage,
    /// See custom integrations, their status and their delivery log, without payloads or script
    /// output. Managers and up.
    IntegrationsRead,
    /// Create, change and delete custom integrations, their secrets and credentials, run, test,
    /// replay and backfill them, install recipes, and read stored payloads and script output
    /// (personal data). Admins and owners.
    IntegrationsManage,
    /// Connect or disconnect the Shopify store, start, pause or cancel its history import, resync its
    /// products and download privacy-request exports (personal data). Admins and owners.
    ShopifyManage,
}

impl Perm {
    pub const ALL: [Perm; 45] = [
        Perm::Public,
        Perm::Me,
        Perm::Mcp,
        Perm::OwnLogin,
        Perm::TwoFactorStep,
        Perm::SwitchAccount,
        Perm::ConfirmIdentity,
        Perm::AudienceRead,
        Perm::ProfilesRead,
        Perm::AudienceWrite,
        Perm::Export,
        Perm::MessagingRead,
        Perm::CampaignDraft,
        Perm::TestSend,
        Perm::CampaignManage,
        Perm::CampaignSend,
        Perm::FlowEdit,
        Perm::FlowLive,
        Perm::TransactionalSend,
        Perm::ContentRead,
        Perm::ContentWrite,
        Perm::FormsWrite,
        Perm::AnalyticsRead,
        Perm::ReportsManage,
        Perm::CatalogWrite,
        Perm::CatalogDelete,
        Perm::AccountRead,
        Perm::SettingsWrite,
        Perm::Domains,
        Perm::BillingRead,
        Perm::BillingWrite,
        Perm::TeamRead,
        Perm::TeamSecurityRead,
        Perm::TeamManage,
        Perm::ApiKeys,
        Perm::Security,
        Perm::AuditRead,
        Perm::Import,
        Perm::Ownership,
        Perm::WebhooksRead,
        Perm::WebhooksManage,
        Perm::ExportsManage,
        Perm::IntegrationsRead,
        Perm::IntegrationsManage,
        Perm::ShopifyManage,
    ];

    /// Stable name used in the API (`me.permissions`) and docs.
    pub fn name(self) -> &'static str {
        use Perm::*;
        match self {
            Public => "public",
            Me => "me",
            Mcp => "mcp",
            OwnLogin => "own_login",
            TwoFactorStep => "two_factor_step",
            SwitchAccount => "accounts.switch",
            ConfirmIdentity => "confirm_identity",
            AudienceRead => "audience.read",
            ProfilesRead => "profiles.read",
            AudienceWrite => "audience.write",
            Export => "export",
            MessagingRead => "messaging.read",
            CampaignDraft => "campaigns.draft",
            TestSend => "campaigns.test",
            CampaignManage => "campaigns.manage",
            CampaignSend => "campaigns.send",
            FlowEdit => "flows.edit",
            FlowLive => "flows.live",
            TransactionalSend => "transactional.send",
            ContentRead => "content.read",
            ContentWrite => "content.write",
            FormsWrite => "forms.write",
            AnalyticsRead => "analytics.read",
            ReportsManage => "reports.manage",
            CatalogWrite => "catalog.write",
            CatalogDelete => "catalog.delete",
            AccountRead => "account.read",
            SettingsWrite => "settings.write",
            Domains => "domains.manage",
            BillingRead => "billing.read",
            BillingWrite => "billing.write",
            TeamRead => "team.read",
            TeamSecurityRead => "team.security_read",
            TeamManage => "team.manage",
            ApiKeys => "api_keys.manage",
            Security => "security.manage",
            AuditRead => "audit.read",
            Import => "imports.manage",
            Ownership => "ownership.transfer",
            WebhooksRead => "webhooks.read",
            WebhooksManage => "webhooks.manage",
            ExportsManage => "exports.manage",
            IntegrationsRead => "integrations.read",
            IntegrationsManage => "integrations.manage",
            ShopifyManage => "shopify.manage",
        }
    }

    /// The API key area this needs, and whether it writes. `None`: not limited by area.
    pub fn area(self) -> Option<(Area, bool)> {
        use Perm::*;
        Some(match self {
            Public | Me | Mcp | OwnLogin | TwoFactorStep | SwitchAccount | ConfirmIdentity | Security | Ownership | TransactionalSend => return None,
            AudienceRead | ProfilesRead => (Area::Audience, false),
            // An export copies people's data out, so a read-only key can't start or fetch one.
            AudienceWrite | Export | Import => (Area::Audience, true),
            MessagingRead => (Area::Messaging, false),
            CampaignDraft | TestSend | CampaignManage | CampaignSend | FlowEdit | FlowLive => (Area::Messaging, true),
            ContentRead => (Area::Content, false),
            ContentWrite | FormsWrite | CatalogWrite | CatalogDelete => (Area::Content, true),
            AnalyticsRead => (Area::Analytics, false),
            ReportsManage => (Area::Analytics, true),
            AccountRead | BillingRead | TeamRead | TeamSecurityRead | AuditRead | WebhooksRead | IntegrationsRead => (Area::Account, false),
            SettingsWrite | Domains | BillingWrite | TeamManage | ApiKeys | WebhooksManage | ExportsManage | IntegrationsManage | ShopifyManage => (Area::Account, true),
        })
    }

    /// Only a signed-in person in a browser can do this; API keys can't.
    pub fn session_only(self) -> bool {
        matches!(self, Perm::OwnLogin | Perm::TwoFactorStep | Perm::SwitchAccount | Perm::ConfirmIdentity | Perm::Security | Perm::Ownership)
    }

    /// Allowed while a session still has to finish two-factor or single sign-on.
    pub fn before_full_sign_in(self) -> bool {
        matches!(self, Perm::Public | Perm::Me | Perm::TwoFactorStep | Perm::SwitchAccount)
    }
}

/// Refuse a report or metric query that would show people's own values (a raw event or profile
/// property, which can hold an email address) unless the caller may read profiles. `what` names
/// the grouping for the message. Totals and time series never need it.
pub fn require_people_values(ctx: &Ctx, what: &str) -> AppResult<()> {
    if allows(ctx, ProfilesRead) {
        return Ok(());
    }
    let who = if ctx.via_api_key { "this API key" } else { "your role" };
    Err(AppError::Forbidden(format!(
        "grouping by {what} shows people's own values (an email address, say), which needs permission to see profiles; {who} can see totals and time series, or groups like day, campaign or channel"
    )))
}

/// Whether this caller may send transactional messages (for the transactional sending module).
pub fn may_send_transactional(ctx: &Ctx) -> bool {
    allows(ctx, Perm::TransactionalSend)
}

// ---- API key scopes ----

/// The scope that allows only transactional sending.
pub const TRANSACTIONAL_SCOPE: &str = "transactional.send";

/// Every scope a key can be given. `full` is everything (the default); `read` is every area
/// read-only except the team and the audit log; an area name is that area read and write;
/// `<area>:read` is that area read-only; `transactional.send` is transactional sending only.
pub fn valid_scopes() -> Vec<String> {
    let mut out = vec!["full".to_string(), "read".to_string()];
    for a in Area::ALL {
        out.push(a.as_str().to_string());
        out.push(format!("{}:read", a.as_str()));
    }
    out.push(TRANSACTIONAL_SCOPE.to_string());
    out
}

/// Check and tidy a list of scopes (deduplicated, sorted; `full` swallows the rest).
pub fn normalize_scopes(scopes: &[String]) -> AppResult<Vec<String>> {
    if scopes.is_empty() {
        return Err(AppError::bad("give the key at least one scope (full, read, or an area like audience or messaging:read)"));
    }
    if scopes.len() > 20 {
        return Err(AppError::bad("too many scopes"));
    }
    let valid = valid_scopes();
    let mut out: Vec<String> = Vec::new();
    for s in scopes {
        let s = s.trim().to_ascii_lowercase();
        if !valid.contains(&s) {
            return Err(AppError::bad(format!("unknown scope {s:?}; use one of: {}", valid.join(", "))));
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.iter().any(|s| s == "full") {
        return Ok(vec!["full".into()]);
    }
    out.sort();
    Ok(out)
}

/// Whether a key with `scopes` may use `perm` (before the creator's role is applied).
pub fn scopes_allow(scopes: &[String], perm: Perm) -> bool {
    if perm.session_only() {
        return false;
    }
    if scopes.iter().any(|s| s == "full") {
        return true;
    }
    if perm == Perm::TransactionalSend {
        return scopes.iter().any(|s| s == TRANSACTIONAL_SCOPE);
    }
    let Some((area, write)) = perm.area() else { return true };
    // The team and the audit log are about people, not marketing data, and webhook URLs (an
    // integration's inbound URL too) can themselves be credentials: plain `read` doesn't include
    // them; they need the account scope.
    let broad_read_ok = !matches!(perm, Perm::TeamRead | Perm::TeamSecurityRead | Perm::AuditRead | Perm::WebhooksRead | Perm::IntegrationsRead);
    scopes.iter().any(|s| {
        let s = s.as_str();
        s == area.as_str() || (!write && (s.strip_suffix(":read") == Some(area.as_str()) || (broad_read_ok && s == "read")))
    })
}

/// Whether every permission `inner` grants is also granted by `outer`, so a key can't make a key
/// stronger than itself.
pub fn scopes_within(inner: &[String], outer: &[String]) -> bool {
    Perm::ALL.iter().all(|p| !scopes_allow(inner, *p) || scopes_allow(outer, *p))
}

// ---- Sessions that haven't finished signing in ----

/// What a session still has to do before it can use the account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Restriction {
    /// Enter the code from the authenticator app (or a recovery code).
    TwoFactorCode,
    /// The account requires two-factor login and this person hasn't set it up yet.
    TwoFactorSetup,
    /// The account requires single sign-on for this person's email domain.
    SsoRequired,
}

impl Restriction {
    pub fn message(self) -> &'static str {
        match self {
            Restriction::TwoFactorCode => "Enter the code from your authenticator app to finish signing in.",
            Restriction::TwoFactorSetup => "This account requires two-factor login. Set it up to continue.",
            Restriction::SsoRequired => "This account requires single sign-on for your email address. Sign in with SSO to continue.",
        }
    }
}

// ---- Route and tool maps ----

/// One route's permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteRule {
    pub method: &'static str,
    pub path: &'static str,
    pub perm: Perm,
}

/// A route that needs `perm`. `path` is the route exactly as registered (`/api/things/{id}`).
pub const fn rule(method: &'static str, path: &'static str, perm: Perm) -> RouteRule {
    RouteRule { method, path, perm }
}

/// A route anyone may call without signing in (sign-in itself, public pages, webhooks). The
/// handler gets no permission check, so it must not read or change account data on the caller's
/// behalf.
pub const fn public(method: &'static str, path: &'static str) -> RouteRule {
    RouteRule { method, path, perm: Perm::Public }
}

/// An MCP tool's permission.
#[derive(Clone, Copy, Debug)]
pub struct ToolRule {
    pub name: &'static str,
    pub perm: Perm,
}

/// What a feature module's routes and tools need. Routes and tools left out are refused.
#[derive(Clone, Copy, Debug, Default)]
pub struct Access {
    pub routes: &'static [RouteRule],
    pub tools: &'static [ToolRule],
}

use Perm::*;

/// Routes outside the feature modules that anyone may call without signing in.
pub static PUBLIC_ROUTES: &[RouteRule] = &[
    // Signing in (src/api/account.rs, src/api/google.rs)
    public("POST", "/api/auth/signup"),
    public("POST", "/api/auth/login"),
    public("POST", "/api/auth/logout"),
    public("GET", "/api/auth/options"),
    public("GET", "/api/auth/google"),
    public("GET", "/api/auth/google/callback"),
    // The MCP endpoint only takes POST; these answer "use POST" (src/mcp.rs).
    public("GET", "/mcp"),
    public("DELETE", "/mcp"),
    // Tracking, unsubscribe, double opt-in, forms, the onsite script and images (src/public.rs)
    public("GET", "/t/o/{token}"),
    public("GET", "/t/c/{token}"),
    public("GET", "/u/{token}"),
    public("POST", "/u/{token}"),
    public("GET", "/c/{token}"),
    public("POST", "/c/{token}"),
    public("GET", "/onsite.js"),
    public("GET", "/client/dumbmail.js"),
    public("GET", "/client/postlark.js"),
    public("GET", "/client/forms"),
    public("GET", "/client/form-render.js"),
    public("POST", "/client/ping"),
    public("OPTIONS", "/client/ping"),
    public("POST", "/client/events"),
    public("OPTIONS", "/client/events"),
    public("GET", "/client/test"),
    public("POST", "/client/test"),
    public("OPTIONS", "/client/test"),
    public("POST", "/client/subscribe"),
    public("OPTIONS", "/client/subscribe"),
    public("POST", "/client/forms/{id}/view"),
    public("OPTIONS", "/client/forms/{id}/view"),
    public("GET", "/f/{id}"),
    public("GET", "/f/{id}/embed.js"),
    public("GET", "/i/{id}"),
    // Provider webhooks (checked by their own secrets) and the operator's token routes (src/public.rs)
    public("POST", "/webhooks/ses/{secret}"),
    public("POST", "/webhooks/telnyx/{secret}"),
    public("POST", "/webhooks/twilio/{secret}"),
    public("POST", "/webhooks/stripe"),
    public("POST", "/admin/credit"),
    public("POST", "/admin/unpause"),
    public("POST", "/admin/sending-limit"),
    public("GET", "/healthz"),
    // Marketing site (src/site.rs, src/site/landing.rs)
    public("GET", "/about"),
    public("GET", "/privacy"),
    public("GET", "/terms"),
    public("GET", "/"),
    public("GET", "/home"),
    public("GET", "/pricing"),
    public("GET", "/robots.txt"),
    public("GET", "/sitemap.xml"),
    public("GET", "/site/{file}"),
    // Feature modules' public pages and widget endpoints
    // Reports (src/features/reports/recipients.rs): confirming an address for report emails
    public("GET", "/report-emails/{token}"),
    public("POST", "/report-emails/{token}"),
    // Catalog (src/features/catalog/api.rs): the back-in-stock widget, stock lookup and confirmation
    public("GET", "/client/back-in-stock.js"),
    public("GET", "/client/products/stock"),
    public("OPTIONS", "/client/products/stock"),
    public("POST", "/client/back-in-stock"),
    public("OPTIONS", "/client/back-in-stock"),
    public("GET", "/client/back-in-stock/confirm/{token}"),
    public("POST", "/client/back-in-stock/confirm/{token}"),
];

/// Every signed-in route outside the feature modules.
pub static CORE_ROUTES: &[RouteRule] = &[
    // The signed-in person (src/api/account.rs)
    rule("GET", "/api/me", Me),
    // Account settings
    rule("PATCH", "/api/account", SettingsWrite),
    rule("POST", "/api/brand/import", SettingsWrite),
    rule("GET", "/api/account/sending", AccountRead),
    rule("POST", "/api/account/domain", Domains),
    rule("DELETE", "/api/account/domain", Domains),
    rule("POST", "/api/account/domain/verify", Domains),
    rule("POST", "/api/account/domains", Domains),
    rule("DELETE", "/api/account/domains/{domain}", Domains),
    rule("POST", "/api/account/domains/{domain}/verify", Domains),
    rule("POST", "/api/account/link-domain", Domains),
    rule("DELETE", "/api/account/link-domain", Domains),
    rule("POST", "/api/account/link-domain/verify", Domains),
    rule("GET", "/api/billing", BillingRead),
    rule("POST", "/api/billing/checkout", BillingWrite),
    rule("PATCH", "/api/billing/auto-recharge", BillingWrite),
    rule("POST", "/api/billing/plan", BillingWrite),
    rule("POST", "/api/billing/plan/cancel", BillingWrite),
    rule("POST", "/api/billing/plan/resume", BillingWrite),
    rule("GET", "/api/phone-numbers", AccountRead),
    rule("POST", "/api/phone-numbers", Domains),
    rule("GET", "/api/phone-numbers/available", AccountRead),
    rule("DELETE", "/api/phone-numbers/{id}", Domains),
    rule("POST", "/api/phone-numbers/{id}/verification", Domains),
    rule("POST", "/api/sms/connect", Domains),
    rule("DELETE", "/api/sms/connect", Domains),
    rule("GET", "/api/website", AccountRead),
    rule("POST", "/api/website/check", AccountRead),
    rule("POST", "/api/website/tests", SettingsWrite),
    rule("GET", "/api/website/tests/{code}", AccountRead),
    // Audience (src/api/audience.rs)
    rule("GET", "/api/profiles", ProfilesRead),
    rule("POST", "/api/profiles", AudienceWrite),
    rule("POST", "/api/profiles/import", AudienceWrite),
    rule("GET", "/api/profiles/{id}", ProfilesRead),
    rule("PATCH", "/api/profiles/{id}", AudienceWrite),
    rule("DELETE", "/api/profiles/{id}", AudienceWrite),
    rule("GET", "/api/lists", AudienceRead),
    rule("POST", "/api/lists", AudienceWrite),
    rule("GET", "/api/lists/{id}", AudienceRead),
    rule("PATCH", "/api/lists/{id}", AudienceWrite),
    rule("DELETE", "/api/lists/{id}", AudienceWrite),
    rule("POST", "/api/lists/{id}/members", AudienceWrite),
    rule("DELETE", "/api/lists/{id}/members", AudienceWrite),
    rule("GET", "/api/segments", AudienceRead),
    rule("POST", "/api/segments", AudienceWrite),
    rule("POST", "/api/segments/preview", ProfilesRead),
    rule("GET", "/api/segments/{id}", AudienceRead),
    rule("PATCH", "/api/segments/{id}", AudienceWrite),
    rule("DELETE", "/api/segments/{id}", AudienceWrite),
    rule("GET", "/api/events", ProfilesRead),
    rule("POST", "/api/events", AudienceWrite),
    rule("GET", "/api/metrics", AnalyticsRead),
    rule("POST", "/api/metrics/query", AnalyticsRead),
    // Messaging and content (src/api/messaging.rs, src/api/analytics.rs, src/api/images.rs)
    rule("GET", "/api/analytics/overview", AnalyticsRead),
    rule("GET", "/api/analytics/flows", AnalyticsRead),
    rule("GET", "/api/analytics/forms", AnalyticsRead),
    rule("GET", "/api/templates", ContentRead),
    rule("POST", "/api/templates", ContentWrite),
    rule("GET", "/api/templates/{id}", ContentRead),
    rule("PATCH", "/api/templates/{id}", ContentWrite),
    rule("DELETE", "/api/templates/{id}", ContentWrite),
    // Previews with a real profile's data also need profiles.read (checked in the handler).
    rule("POST", "/api/render", ContentRead),
    rule("GET", "/api/campaigns", MessagingRead),
    rule("POST", "/api/campaigns", CampaignDraft),
    rule("POST", "/api/campaigns/estimate", MessagingRead),
    rule("GET", "/api/campaigns/{id}", MessagingRead),
    // Without campaigns.send only drafts change (checked in the UPDATE itself).
    rule("PATCH", "/api/campaigns/{id}", CampaignDraft),
    rule("DELETE", "/api/campaigns/{id}", CampaignManage),
    rule("POST", "/api/campaigns/{id}/test", TestSend),
    rule("POST", "/api/campaigns/{id}/schedule", CampaignSend),
    rule("POST", "/api/campaigns/{id}/send", CampaignSend),
    rule("POST", "/api/campaigns/{id}/cancel", CampaignManage),
    rule("POST", "/api/campaigns/{id}/duplicate", CampaignDraft),
    rule("GET", "/api/campaigns/{id}/report", MessagingRead),
    rule("POST", "/api/campaigns/{id}/winner", CampaignSend),
    rule("GET", "/api/flows", MessagingRead),
    rule("POST", "/api/flows", FlowEdit),
    rule("GET", "/api/flows/blueprints", MessagingRead),
    rule("POST", "/api/flows/from-blueprint", FlowEdit),
    rule("GET", "/api/flows/{id}", MessagingRead),
    rule("PATCH", "/api/flows/{id}", FlowEdit),
    rule("DELETE", "/api/flows/{id}", FlowEdit),
    rule("POST", "/api/flows/{id}/status", FlowLive),
    rule("GET", "/api/flows/{id}/report", MessagingRead),
    rule("GET", "/api/forms", ContentRead),
    rule("POST", "/api/forms", FormsWrite),
    rule("GET", "/api/forms/{id}", ContentRead),
    rule("PATCH", "/api/forms/{id}", FormsWrite),
    rule("DELETE", "/api/forms/{id}", FormsWrite),
    rule("GET", "/api/images", ContentRead),
    rule("POST", "/api/images", ContentWrite),
    rule("DELETE", "/api/images/{id}", ContentWrite),
    // MCP (src/mcp.rs): each tool is checked on its own in `authorize_tool`.
    rule("POST", "/mcp", Mcp),
];

/// The rule for a matched route path, from the core lists or a feature module.
pub fn route_rule(method: &Method, path: &str) -> Option<RouteRule> {
    let m = if method == Method::HEAD { "GET" } else { method.as_str() };
    let found = |rules: &'static [RouteRule]| rules.iter().find(|r| r.method == m && r.path == path).copied();
    found(PUBLIC_ROUTES).or_else(|| found(CORE_ROUTES)).or_else(|| crate::features::ALL.iter().find_map(|f| found(f.access().routes)))
}

/// Every route rule (public, core and features), for tests and docs.
pub fn all_route_rules() -> Vec<RouteRule> {
    let mut out = PUBLIC_ROUTES.to_vec();
    out.extend_from_slice(CORE_ROUTES);
    for f in crate::features::ALL {
        out.extend_from_slice(f.access().routes);
    }
    out
}

/// What a developer sees when a route has no rule.
pub fn missing_route_rule(method: &Method, path: &str) -> String {
    format!(
        "{method} {path} has no permission rule, so it's refused. Add one to the access() of the feature that registers it \
         (src/features/<feature>.rs or src/features/<feature>/mod.rs), or to CORE_ROUTES or PUBLIC_ROUTES in src/features/team/access.rs. See AGENTS.md."
    )
}

/// The permission an MCP tool call needs. `delete` depends on what's being deleted.
pub fn tool_perm(name: &str, args: &Value) -> Option<Perm> {
    if name == "delete" {
        return Some(match args["type"].as_str().unwrap_or("") {
            "campaign" => CampaignManage,
            "flow" => FlowEdit,
            "segment" | "list" | "profile" => AudienceWrite,
            "template" | "image" => ContentWrite,
            "form" => FormsWrite,
            // Unknown types are refused by the tool itself; still require the strongest write.
            _ => SettingsWrite,
        });
    }
    core_tool_perm(name).or_else(|| crate::features::ALL.iter().find_map(|f| f.access().tools.iter().find(|t| t.name == name).map(|t| t.perm)))
}

/// Core MCP tools (src/mcp.rs).
pub fn core_tool_perm(name: &str) -> Option<Perm> {
    Some(match name {
        "get_account" | "list_phone_numbers" | "find_phone_numbers" | "get_website_snippet" => AccountRead,
        "update_account" | "import_brand" => SettingsWrite,
        "set_sending_domain" | "check_sending_domain" | "remove_sending_domain" | "set_link_domain" | "check_link_domain" | "claim_phone_number" => Domains,
        "get_billing" => BillingRead,
        "search_profiles" | "get_profile" | "get_events" | "preview_segment" => ProfilesRead,
        "upsert_profile" | "import_profiles" | "track_event" | "create_list" | "update_list" | "add_to_list" | "remove_from_list" | "create_segment" | "update_segment" => AudienceWrite,
        "list_lists" | "list_segments" => AudienceRead,
        "list_metrics" | "query_metric" | "campaign_performance" | "get_analytics" => AnalyticsRead,
        // render_preview with a profile_id also needs profiles.read (checked in the tool).
        "list_templates" | "render_preview" | "list_forms" | "get_form" | "list_images" => ContentRead,
        "create_template" | "upload_image" => ContentWrite,
        "create_form" | "update_form" => FormsWrite,
        "list_campaigns" | "get_campaign" | "get_campaign_report" | "list_flows" | "get_flow" | "list_flow_blueprints" | "get_flow_report" => MessagingRead,
        "create_campaign" | "update_campaign" => CampaignDraft,
        "send_test" => TestSend,
        "send_campaign" | "pick_campaign_winner" => CampaignSend,
        "cancel_campaign" => CampaignManage,
        "create_flow_from_blueprint" | "create_flow" | "update_flow" => FlowEdit,
        "set_flow_status" => FlowLive,
        "delete" => CampaignManage,
        _ => return None,
    })
}

// ---- The checks ----

fn denied(ctx: &Ctx, perm: Perm) -> AppError {
    if let Some(r) = ctx.restriction {
        return AppError::Forbidden(r.message().into());
    }
    if ctx.via_api_key && perm.session_only() {
        return AppError::Forbidden("API keys can't do this; sign in to the dashboard".into());
    }
    if ctx.via_api_key && !ctx.scopes.as_deref().is_some_and(|s| scopes_allow(s, perm)) {
        let need = match perm {
            TransactionalSend => TRANSACTIONAL_SCOPE.to_string(),
            TeamRead | TeamSecurityRead | AuditRead => "account:read".into(),
            _ => perm.area().map(|(a, w)| if w { a.as_str().to_string() } else { format!("{}:read", a.as_str()) }).unwrap_or_else(|| "full".into()),
        };
        return AppError::Forbidden(format!("this API key's scopes don't allow that (it needs the {need} scope)"));
    }
    if ctx.sso_session && matches!(perm, OwnLogin | SwitchAccount) {
        return AppError::Forbidden("a single sign-on session can only use this account; sign in with your password or Google to change your login or open other accounts".into());
    }
    AppError::Forbidden(format!("your role ({}) can't do this; ask an owner or admin", ctx.role.label()))
}

/// Whether `ctx` holds `perm`: its role, its key's scopes, and how far its session got.
pub fn allows(ctx: &Ctx, perm: Perm) -> bool {
    if perm == Public {
        return true;
    }
    if ctx.restriction.is_some() && !perm.before_full_sign_in() {
        return false;
    }
    if ctx.via_api_key {
        if !ctx.scopes.as_deref().is_some_and(|s| scopes_allow(s, perm)) {
            return false;
        }
    } else if ctx.sso_session && matches!(perm, OwnLogin | SwitchAccount) {
        // A single sign-on session is trusted for its own account only (see `sso`), so it can't
        // change the login itself (password, sessions) or open other accounts.
        return false;
    }
    ctx.role.has(perm)
}

pub fn require(ctx: &Ctx, perm: Perm) -> AppResult<()> {
    if allows(ctx, perm) { Ok(()) } else { Err(denied(ctx, perm)) }
}

/// The permission names `ctx` holds, for the dashboard to show or hide controls.
pub fn granted(ctx: &Ctx) -> Vec<&'static str> {
    Perm::ALL.iter().filter(|p| !matches!(p, Public | Me | Mcp) && allows(ctx, **p)).map(|p| p.name()).collect()
}

/// How recently a session must have re-entered its password or a code to change ownership.
pub const FRESH_SIGN_IN_MINUTES: i64 = 10;

fn within_fresh_window(t: Option<chrono::DateTime<chrono::Utc>>) -> bool {
    t.is_some_and(|t| t > chrono::Utc::now() - chrono::Duration::minutes(FRESH_SIGN_IN_MINUTES))
}

/// Refuse unless this browser session proved who it is in the last [`FRESH_SIGN_IN_MINUTES`]
/// minutes to accept an owner invite: it confirmed it's you (as for [`require_fresh_sign_in`]), or
/// a Google sign-in just started it. Signing in with a password stamps the confirmation itself.
pub fn require_fresh_for_ownership_invite(ctx: &Ctx) -> AppResult<()> {
    if ctx.via_api_key || ctx.session_id.is_none() {
        return Err(AppError::Forbidden("sign in to the dashboard to accept this invite".into()));
    }
    if within_fresh_window(ctx.reauth_at) || within_fresh_window(ctx.google_sign_in_at) {
        return Ok(());
    }
    Err(AppError::Forbidden("this invite makes you an owner, so confirm it's you first: enter your password or a two-factor code (or continue with Google), then accept again".into()))
}

/// Refuse unless this is a browser session that re-entered its password or a two-factor code in
/// the last [`FRESH_SIGN_IN_MINUTES`] minutes. Anything that makes, removes or demotes an owner
/// needs it, so a stolen API key or an unattended browser can't take an account over.
pub fn require_fresh_sign_in(ctx: &Ctx) -> AppResult<()> {
    if ctx.via_api_key || ctx.session_id.is_none() {
        return Err(AppError::Forbidden("only an owner signed in to the dashboard can make, remove or demote owners; API keys and MCP can't".into()));
    }
    if !within_fresh_window(ctx.reauth_at) {
        return Err(AppError::Forbidden("confirm it's you first: enter your password or a two-factor code, then try again".into()));
    }
    Ok(())
}

/// Check an MCP tool call.
pub async fn authorize_tool(_state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> AppResult<Perm> {
    let Some(perm) = tool_perm(name, args) else {
        let owner = crate::features::ALL.iter().find(|f| f.mcp_tools().iter().any(|t| t["name"] == name)).map(|f| f.name());
        let fix = match owner {
            Some(f) => format!("add it to access().tools in the {f} feature (src/features/{f}.rs or src/features/{f}/mod.rs)"),
            None => "add it to core_tool_perm in src/features/team/access.rs, or to access().tools in the feature that defines it".into(),
        };
        tracing::error!(tool = name, "MCP tool has no permission rule; refusing");
        return Err(AppError::Forbidden(format!("the tool {name:?} has no permission rule, so it's refused; {fix}")));
    };
    require(ctx, perm)?;
    Ok(perm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn roles() {
        assert!(Role::Owner.has(Ownership));
        assert!(!Role::Admin.has(Ownership));
        assert!(Role::Admin.has(Security));
        assert!(Role::Admin.has(TeamManage) && Role::Admin.has(BillingWrite));
        assert!(!Role::Manager.has(TeamManage) && !Role::Manager.has(BillingRead) && !Role::Manager.has(ApiKeys) && !Role::Manager.has(Domains));
        assert!(Role::Manager.has(CampaignSend) && Role::Manager.has(FlowLive) && Role::Manager.has(TransactionalSend));
        assert!(Role::Analyst.has(Export) && !Role::Analyst.has(AudienceWrite) && !Role::Analyst.has(CampaignDraft) && !Role::Analyst.has(TestSend));
        assert!(Role::Content.has(CampaignDraft) && Role::Content.has(ContentWrite) && Role::Content.has(TestSend));
        assert!(!Role::Content.has(CampaignSend) && !Role::Content.has(Export) && !Role::Content.has(ProfilesRead) && !Role::Content.has(TransactionalSend));
        assert!(Role::Manager.has(ReportsManage) && Role::Manager.has(CatalogWrite) && !Role::Manager.has(CatalogDelete) && Role::Admin.has(CatalogDelete));
        assert!(!Role::Analyst.has(ReportsManage) && !Role::Content.has(CatalogWrite) && Role::Content.has(ContentRead));
        assert!(Role::Owner.has(TeamSecurityRead) && Role::Admin.has(TeamSecurityRead));
        assert!(Role::Manager.has(TeamRead) && !Role::Manager.has(TeamSecurityRead) && !Role::Analyst.has(TeamSecurityRead) && !Role::Content.has(TeamSecurityRead));
        assert!(Role::Manager.has(IntegrationsRead) && !Role::Manager.has(IntegrationsManage) && Role::Admin.has(IntegrationsManage) && Role::Owner.has(IntegrationsManage));
        assert!(!Role::Analyst.has(IntegrationsRead) && !Role::Content.has(IntegrationsRead));
    }

    #[test]
    fn every_perm_is_listed_once() {
        let distinct: std::collections::HashSet<_> = Perm::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(distinct.len(), Perm::ALL.len(), "each permission once in Perm::ALL");
    }

    #[test]
    fn scopes() {
        assert!(scopes_allow(&s(&["full"]), CampaignSend));
        assert!(scopes_allow(&s(&["read"]), ProfilesRead));
        // Exports copy people's data out: never with a read-only key.
        assert!(!scopes_allow(&s(&["read"]), Export) && !scopes_allow(&s(&["audience:read"]), Export) && !scopes_allow(&s(&["analytics"]), Export));
        assert!(scopes_allow(&s(&["audience"]), Export) && scopes_allow(&s(&["full"]), Export));
        assert!(!scopes_allow(&s(&["read"]), AudienceWrite));
        assert!(scopes_allow(&s(&["audience"]), AudienceWrite));
        assert!(!scopes_allow(&s(&["audience"]), MessagingRead));
        assert!(scopes_allow(&s(&["messaging:read"]), MessagingRead));
        assert!(!scopes_allow(&s(&["messaging:read"]), CampaignSend));
        assert!(!scopes_allow(&s(&["messaging:read"]), TestSend));
        assert!(scopes_allow(&s(&["messaging"]), TestSend));
        assert!(!scopes_allow(&s(&["full"]), Security), "keys never change security settings");
        assert!(scopes_allow(&s(&["read"]), Me));
        // The team and audit log need the account scope; `read` alone isn't enough.
        assert!(!scopes_allow(&s(&["read"]), TeamRead) && !scopes_allow(&s(&["read"]), AuditRead));
        assert!(scopes_allow(&s(&["account:read"]), TeamRead) && scopes_allow(&s(&["account"]), AuditRead));
        assert!(!scopes_allow(&s(&["read"]), IntegrationsRead) && scopes_allow(&s(&["account:read"]), IntegrationsRead));
        assert!(!scopes_allow(&s(&["account:read"]), IntegrationsManage) && !scopes_allow(&s(&["audience"]), IntegrationsManage) && scopes_allow(&s(&["account"]), IntegrationsManage));
        assert!(scopes_allow(&s(&["read"]), AccountRead));
        // Transactional sending has its own scope.
        assert!(scopes_allow(&s(&["transactional.send"]), TransactionalSend) && scopes_allow(&s(&["full"]), TransactionalSend));
        assert!(!scopes_allow(&s(&["messaging"]), TransactionalSend) && !scopes_allow(&s(&["transactional.send"]), MessagingRead));
        assert_eq!(normalize_scopes(&s(&["audience", "full"])).unwrap(), s(&["full"]));
        assert!(normalize_scopes(&s(&["admin"])).is_err());
        assert!(scopes_within(&s(&["audience:read"]), &s(&["audience"])));
        assert!(!scopes_within(&s(&["account"]), &s(&["audience"])));
        assert!(scopes_within(&s(&["read"]), &s(&["full"])));
        assert!(!scopes_within(&s(&["account:read"]), &s(&["read"])), "read can't mint a key that reads the team");
    }

    #[test]
    fn every_core_route_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for r in PUBLIC_ROUTES.iter().chain(CORE_ROUTES) {
            assert!(seen.insert((r.method, r.path)), "duplicate rule {} {}", r.method, r.path);
        }
    }
}
