//! Roles, permissions and API key scopes, checked in one place.
//!
//! Every route says which [`Perm`] it needs in [`CORE_ROUTES`] (or in its feature's
//! [`crate::features::Feature::access`]); every MCP tool does the same in [`core_tool_perm`] or the
//! feature's `access().tools`. The [`crate::auth::Ctx`] extractor calls [`authorize_route`] before
//! any handler runs, and the MCP server calls [`authorize_tool`] before any tool runs. Anything not
//! listed is refused.

use axum::http::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

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
            Public | Me | Mcp | OwnLogin | TwoFactorStep => true,
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
                        | CampaignManage
                        | CampaignSend
                        | FlowEdit
                        | FlowLive
                        | ContentRead
                        | ContentWrite
                        | FormsWrite
                        | AnalyticsRead
                        | AccountRead
                        | SettingsWrite
                        | TeamRead
                ),
                Role::Analyst => matches!(perm, AudienceRead | ProfilesRead | Export | MessagingRead | ContentRead | AnalyticsRead | AccountRead | TeamRead),
                Role::Content => matches!(perm, AudienceRead | MessagingRead | CampaignDraft | ContentRead | ContentWrite | AnalyticsRead | AccountRead | TeamRead),
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
    /// No sign-in (public pages, webhooks, sign-in itself). Handlers here don't read `Ctx`.
    Public,
    /// Anyone signed in, including sessions still waiting on two-factor or SSO, and any API key.
    Me,
    /// The MCP endpoint. Each tool is checked on its own.
    Mcp,
    /// Your own login (sessions, password, accounts). Browser sessions only.
    OwnLogin,
    /// Finishing sign-in: entering a two-factor code, setting one up, picking an account. Browser
    /// sessions only, and allowed before two-factor is done.
    TwoFactorStep,
    /// Lists, segments and metric names (no personal data).
    AudienceRead,
    /// Profiles, events and segment samples (personal data).
    ProfilesRead,
    AudienceWrite,
    /// Exports of profiles or reports.
    Export,
    MessagingRead,
    /// Create and edit campaign drafts and send tests.
    CampaignDraft,
    /// Cancel, unschedule and delete campaigns.
    CampaignManage,
    /// Send or schedule a campaign, or pick an A/B winner.
    CampaignSend,
    FlowEdit,
    /// Turn a flow live (or pause or draft it).
    FlowLive,
    ContentRead,
    ContentWrite,
    FormsWrite,
    AnalyticsRead,
    AccountRead,
    SettingsWrite,
    /// Sending domains, link domain, phone numbers and SMS providers.
    Domains,
    BillingRead,
    BillingWrite,
    TeamRead,
    TeamManage,
    ApiKeys,
    /// Two-factor requirement and single sign-on. Browser sessions only.
    Security,
    AuditRead,
    /// Transfer ownership. Browser sessions only.
    Ownership,
}

impl Perm {
    pub const ALL: [Perm; 30] = [
        Perm::Public,
        Perm::Me,
        Perm::Mcp,
        Perm::OwnLogin,
        Perm::TwoFactorStep,
        Perm::AudienceRead,
        Perm::ProfilesRead,
        Perm::AudienceWrite,
        Perm::Export,
        Perm::MessagingRead,
        Perm::CampaignDraft,
        Perm::CampaignManage,
        Perm::CampaignSend,
        Perm::FlowEdit,
        Perm::FlowLive,
        Perm::ContentRead,
        Perm::ContentWrite,
        Perm::FormsWrite,
        Perm::AnalyticsRead,
        Perm::AccountRead,
        Perm::SettingsWrite,
        Perm::Domains,
        Perm::BillingRead,
        Perm::BillingWrite,
        Perm::TeamRead,
        Perm::TeamManage,
        Perm::ApiKeys,
        Perm::Security,
        Perm::AuditRead,
        Perm::Ownership,
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
            AudienceRead => "audience.read",
            ProfilesRead => "profiles.read",
            AudienceWrite => "audience.write",
            Export => "export",
            MessagingRead => "messaging.read",
            CampaignDraft => "campaigns.draft",
            CampaignManage => "campaigns.manage",
            CampaignSend => "campaigns.send",
            FlowEdit => "flows.edit",
            FlowLive => "flows.live",
            ContentRead => "content.read",
            ContentWrite => "content.write",
            FormsWrite => "forms.write",
            AnalyticsRead => "analytics.read",
            AccountRead => "account.read",
            SettingsWrite => "settings.write",
            Domains => "domains.manage",
            BillingRead => "billing.read",
            BillingWrite => "billing.write",
            TeamRead => "team.read",
            TeamManage => "team.manage",
            ApiKeys => "api_keys.manage",
            Security => "security.manage",
            AuditRead => "audit.read",
            Ownership => "ownership.transfer",
        }
    }

    /// The API key area this needs, and whether it writes. `None`: not limited by area.
    pub fn area(self) -> Option<(Area, bool)> {
        use Perm::*;
        Some(match self {
            Public | Me | Mcp | OwnLogin | TwoFactorStep | Security | Ownership => return None,
            AudienceRead | ProfilesRead | Export => (Area::Audience, false),
            AudienceWrite => (Area::Audience, true),
            MessagingRead => (Area::Messaging, false),
            CampaignDraft | CampaignManage | CampaignSend | FlowEdit | FlowLive => (Area::Messaging, true),
            ContentRead => (Area::Content, false),
            ContentWrite | FormsWrite => (Area::Content, true),
            AnalyticsRead => (Area::Analytics, false),
            AccountRead | BillingRead | TeamRead | AuditRead => (Area::Account, false),
            SettingsWrite | Domains | BillingWrite | TeamManage | ApiKeys => (Area::Account, true),
        })
    }

    /// Only a signed-in person in a browser can do this; API keys can't.
    pub fn session_only(self) -> bool {
        matches!(self, Perm::OwnLogin | Perm::TwoFactorStep | Perm::Security | Perm::Ownership)
    }

    /// Allowed while a session still has to finish two-factor or single sign-on.
    pub fn before_full_sign_in(self) -> bool {
        matches!(self, Perm::Public | Perm::Me | Perm::TwoFactorStep)
    }

    /// Writes change something; they're recorded in the audit log when called through MCP.
    pub fn writes(self) -> bool {
        self.area().is_some_and(|(_, w)| w) || matches!(self, Perm::Security | Perm::Ownership)
    }
}

// ---- API key scopes ----

/// Every scope a key can be given. `full` is everything (the default); `read` is every area
/// read-only; an area name is that area read and write; `<area>:read` is that area read-only.
pub fn valid_scopes() -> Vec<String> {
    let mut out = vec!["full".to_string(), "read".to_string()];
    for a in Area::ALL {
        out.push(a.as_str().to_string());
        out.push(format!("{}:read", a.as_str()));
    }
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
    let Some((area, write)) = perm.area() else { return true };
    scopes.iter().any(|s| {
        let s = s.as_str();
        s == "full" || s == area.as_str() || (!write && (s == "read" || s.strip_suffix(":read") == Some(area.as_str())))
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

/// One route's permission and, for changes worth recording, the audit log action written after a
/// successful call (the path's first parameter becomes the target).
#[derive(Clone, Copy, Debug)]
pub struct RouteRule {
    pub method: &'static str,
    pub path: &'static str,
    pub perm: Perm,
    pub audit: Option<&'static str>,
}

pub const fn rule(method: &'static str, path: &'static str, perm: Perm, audit: Option<&'static str>) -> RouteRule {
    RouteRule { method, path, perm, audit }
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

/// Every route outside the feature modules.
pub static CORE_ROUTES: &[RouteRule] = &[
    // Sign-in and the signed-in person (src/api/account.rs, src/api/google.rs)
    rule("POST", "/api/auth/signup", Public, None),
    rule("POST", "/api/auth/login", Public, None),
    rule("POST", "/api/auth/logout", Public, None),
    rule("GET", "/api/me", Me, None),
    rule("GET", "/api/auth/options", Public, None),
    rule("GET", "/api/auth/google", Public, None),
    rule("GET", "/api/auth/google/callback", Public, None),
    // Account settings
    rule("PATCH", "/api/account", SettingsWrite, Some("account.settings_change")),
    rule("POST", "/api/brand/import", SettingsWrite, Some("account.brand_import")),
    rule("GET", "/api/account/sending", AccountRead, None),
    rule("POST", "/api/account/domain", Domains, Some("domain.add")),
    rule("DELETE", "/api/account/domain", Domains, Some("domain.remove")),
    rule("POST", "/api/account/domain/verify", Domains, None),
    rule("POST", "/api/account/domains", Domains, Some("domain.add")),
    rule("DELETE", "/api/account/domains/{domain}", Domains, Some("domain.remove")),
    rule("POST", "/api/account/domains/{domain}/verify", Domains, None),
    rule("POST", "/api/account/link-domain", Domains, Some("link_domain.set")),
    rule("DELETE", "/api/account/link-domain", Domains, Some("link_domain.remove")),
    rule("POST", "/api/account/link-domain/verify", Domains, None),
    rule("GET", "/api/billing", BillingRead, None),
    rule("POST", "/api/billing/checkout", BillingWrite, Some("billing.checkout")),
    rule("PATCH", "/api/billing/auto-recharge", BillingWrite, Some("billing.auto_recharge_change")),
    rule("POST", "/api/billing/plan", BillingWrite, Some("billing.plan_change")),
    rule("POST", "/api/billing/plan/cancel", BillingWrite, Some("billing.plan_cancel")),
    rule("POST", "/api/billing/plan/resume", BillingWrite, Some("billing.plan_resume")),
    rule("GET", "/api/phone-numbers", AccountRead, None),
    rule("POST", "/api/phone-numbers", Domains, Some("phone_number.claim")),
    rule("GET", "/api/phone-numbers/available", AccountRead, None),
    rule("DELETE", "/api/phone-numbers/{id}", Domains, Some("phone_number.release")),
    rule("POST", "/api/phone-numbers/{id}/verification", Domains, Some("phone_number.verification_submit")),
    rule("POST", "/api/sms/connect", Domains, Some("sms_provider.connect")),
    rule("DELETE", "/api/sms/connect", Domains, Some("sms_provider.disconnect")),
    rule("GET", "/api/website", AccountRead, None),
    rule("POST", "/api/website/check", AccountRead, None),
    rule("POST", "/api/website/tests", SettingsWrite, None),
    rule("GET", "/api/website/tests/{code}", AccountRead, None),
    // Audience (src/api/audience.rs)
    rule("GET", "/api/profiles", ProfilesRead, None),
    rule("POST", "/api/profiles", AudienceWrite, None),
    rule("POST", "/api/profiles/import", AudienceWrite, Some("profiles.import")),
    rule("GET", "/api/profiles/{id}", ProfilesRead, None),
    rule("PATCH", "/api/profiles/{id}", AudienceWrite, None),
    rule("DELETE", "/api/profiles/{id}", AudienceWrite, Some("profile.delete")),
    rule("GET", "/api/lists", AudienceRead, None),
    rule("POST", "/api/lists", AudienceWrite, None),
    rule("GET", "/api/lists/{id}", AudienceRead, None),
    rule("PATCH", "/api/lists/{id}", AudienceWrite, None),
    rule("DELETE", "/api/lists/{id}", AudienceWrite, Some("list.delete")),
    rule("POST", "/api/lists/{id}/members", AudienceWrite, None),
    rule("DELETE", "/api/lists/{id}/members", AudienceWrite, Some("list.members_remove")),
    rule("GET", "/api/segments", AudienceRead, None),
    rule("POST", "/api/segments", AudienceWrite, None),
    rule("POST", "/api/segments/preview", ProfilesRead, None),
    rule("GET", "/api/segments/{id}", AudienceRead, None),
    rule("PATCH", "/api/segments/{id}", AudienceWrite, None),
    rule("DELETE", "/api/segments/{id}", AudienceWrite, Some("segment.delete")),
    rule("GET", "/api/events", ProfilesRead, None),
    rule("POST", "/api/events", AudienceWrite, None),
    rule("GET", "/api/metrics", AnalyticsRead, None),
    rule("POST", "/api/metrics/query", AnalyticsRead, None),
    // Messaging and content (src/api/messaging.rs, src/api/images.rs)
    rule("GET", "/api/analytics/overview", AnalyticsRead, None),
    rule("GET", "/api/analytics/flows", AnalyticsRead, None),
    rule("GET", "/api/analytics/forms", AnalyticsRead, None),
    rule("GET", "/api/templates", ContentRead, None),
    rule("POST", "/api/templates", ContentWrite, None),
    rule("GET", "/api/templates/{id}", ContentRead, None),
    rule("PATCH", "/api/templates/{id}", ContentWrite, None),
    rule("DELETE", "/api/templates/{id}", ContentWrite, Some("template.delete")),
    rule("POST", "/api/render", ContentRead, None),
    rule("GET", "/api/campaigns", MessagingRead, None),
    rule("POST", "/api/campaigns", CampaignDraft, None),
    rule("POST", "/api/campaigns/estimate", MessagingRead, None),
    rule("GET", "/api/campaigns/{id}", MessagingRead, None),
    rule("PATCH", "/api/campaigns/{id}", CampaignDraft, None),
    rule("DELETE", "/api/campaigns/{id}", CampaignManage, Some("campaign.delete")),
    rule("POST", "/api/campaigns/{id}/test", CampaignDraft, Some("campaign.test_send")),
    rule("POST", "/api/campaigns/{id}/schedule", CampaignSend, Some("campaign.schedule")),
    rule("POST", "/api/campaigns/{id}/send", CampaignSend, Some("campaign.send")),
    rule("POST", "/api/campaigns/{id}/cancel", CampaignManage, Some("campaign.cancel")),
    rule("POST", "/api/campaigns/{id}/duplicate", CampaignDraft, None),
    rule("GET", "/api/campaigns/{id}/report", MessagingRead, None),
    rule("POST", "/api/campaigns/{id}/winner", CampaignSend, Some("campaign.pick_winner")),
    rule("GET", "/api/flows", MessagingRead, None),
    rule("POST", "/api/flows", FlowEdit, None),
    rule("GET", "/api/flows/blueprints", MessagingRead, None),
    rule("POST", "/api/flows/from-blueprint", FlowEdit, None),
    rule("GET", "/api/flows/{id}", MessagingRead, None),
    rule("PATCH", "/api/flows/{id}", FlowEdit, Some("flow.update")),
    rule("DELETE", "/api/flows/{id}", FlowEdit, Some("flow.delete")),
    rule("POST", "/api/flows/{id}/status", FlowLive, Some("flow.status_change")),
    rule("GET", "/api/flows/{id}/report", MessagingRead, None),
    rule("GET", "/api/forms", ContentRead, None),
    rule("POST", "/api/forms", FormsWrite, None),
    rule("GET", "/api/forms/{id}", ContentRead, None),
    rule("PATCH", "/api/forms/{id}", FormsWrite, Some("form.update")),
    rule("DELETE", "/api/forms/{id}", FormsWrite, Some("form.delete")),
    rule("GET", "/api/images", ContentRead, None),
    rule("POST", "/api/images", ContentWrite, None),
    rule("DELETE", "/api/images/{id}", ContentWrite, Some("image.delete")),
    // MCP (src/mcp.rs): tools are checked one by one in `authorize_tool`.
    rule("POST", "/mcp", Mcp, None),
    rule("GET", "/mcp", Public, None),
    rule("DELETE", "/mcp", Public, None),
    // Public pages, tracking, forms, webhooks and the operator's admin token routes (src/public.rs)
    rule("GET", "/t/o/{token}", Public, None),
    rule("GET", "/t/c/{token}", Public, None),
    rule("GET", "/u/{token}", Public, None),
    rule("POST", "/u/{token}", Public, None),
    rule("GET", "/c/{token}", Public, None),
    rule("POST", "/c/{token}", Public, None),
    rule("GET", "/onsite.js", Public, None),
    rule("GET", "/client/dumbmail.js", Public, None),
    rule("GET", "/client/postlark.js", Public, None),
    rule("GET", "/client/forms", Public, None),
    rule("GET", "/client/form-render.js", Public, None),
    rule("POST", "/client/ping", Public, None),
    rule("OPTIONS", "/client/ping", Public, None),
    rule("POST", "/client/events", Public, None),
    rule("OPTIONS", "/client/events", Public, None),
    rule("GET", "/client/test", Public, None),
    rule("POST", "/client/test", Public, None),
    rule("OPTIONS", "/client/test", Public, None),
    rule("POST", "/client/subscribe", Public, None),
    rule("OPTIONS", "/client/subscribe", Public, None),
    rule("POST", "/client/forms/{id}/view", Public, None),
    rule("OPTIONS", "/client/forms/{id}/view", Public, None),
    rule("GET", "/f/{id}", Public, None),
    rule("GET", "/f/{id}/embed.js", Public, None),
    rule("GET", "/i/{id}", Public, None),
    rule("POST", "/webhooks/ses/{secret}", Public, None),
    rule("POST", "/webhooks/telnyx/{secret}", Public, None),
    rule("POST", "/webhooks/twilio/{secret}", Public, None),
    rule("POST", "/webhooks/stripe", Public, None),
    rule("POST", "/admin/credit", Public, None),
    rule("POST", "/admin/unpause", Public, None),
    rule("POST", "/admin/sending-limit", Public, None),
    rule("GET", "/healthz", Public, None),
    // Marketing site (src/site.rs, src/site/landing.rs)
    rule("GET", "/about", Public, None),
    rule("GET", "/privacy", Public, None),
    rule("GET", "/terms", Public, None),
    rule("GET", "/", Public, None),
    rule("GET", "/home", Public, None),
    rule("GET", "/pricing", Public, None),
    rule("GET", "/robots.txt", Public, None),
    rule("GET", "/sitemap.xml", Public, None),
    rule("GET", "/site/{file}", Public, None),
];

/// The rule for a matched route path, from the core list or a feature module.
pub fn route_rule(method: &Method, path: &str) -> Option<RouteRule> {
    let m = if method == Method::HEAD { "GET" } else { method.as_str() };
    let found = |rules: &'static [RouteRule]| rules.iter().find(|r| r.method == m && r.path == path).copied();
    found(CORE_ROUTES).or_else(|| crate::features::ALL.iter().find_map(|f| found(f.access().routes)))
}

/// Every route rule (core and features), for tests and docs.
pub fn all_route_rules() -> Vec<RouteRule> {
    let mut out = CORE_ROUTES.to_vec();
    for f in crate::features::ALL {
        out.extend_from_slice(f.access().routes);
    }
    out
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
        "list_templates" | "render_preview" | "list_forms" | "get_form" | "list_images" => ContentRead,
        "create_template" | "upload_image" => ContentWrite,
        "create_form" | "update_form" => FormsWrite,
        "list_campaigns" | "get_campaign" | "get_campaign_report" | "list_flows" | "get_flow" | "list_flow_blueprints" | "get_flow_report" => MessagingRead,
        "create_campaign" | "update_campaign" | "send_test" => CampaignDraft,
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
        let need = perm.area().map(|(a, w)| if w { a.as_str().to_string() } else { format!("{}:read", a.as_str()) }).unwrap_or_else(|| "full".into());
        return AppError::Forbidden(format!("this API key's scopes don't allow that (it needs the {need} scope)"));
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
    } else if ctx.sso_session && matches!(perm, OwnLogin | TwoFactorStep) {
        // A single sign-on session is trusted for its own account only (see `sso`), so it can't
        // change the login itself (password, two-factor, sessions) or open other accounts.
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

/// Check a route call. `campaign_id` is the `{id}` path parameter of campaign routes, used for the
/// one rule that depends on data: people who can't send may only edit drafts, never a scheduled
/// campaign.
pub async fn authorize_route(state: &AppState, ctx: &Ctx, method: &Method, path: Option<&str>, first_param: Option<&str>) -> AppResult<RouteRule> {
    let Some(path) = path else {
        return Err(AppError::Forbidden("this route has no permission rule".into()));
    };
    let Some(r) = route_rule(method, path) else {
        tracing::error!(%method, path, "route has no permission rule; refusing");
        return Err(AppError::Forbidden("this route has no permission rule".into()));
    };
    require(ctx, r.perm)?;
    if r.perm == CampaignDraft && method == Method::PATCH && path == "/api/campaigns/{id}" {
        if let Some(id) = first_param.and_then(|s| s.parse::<Uuid>().ok()) {
            draft_only_check(state, ctx, id).await?;
        }
    }
    Ok(r)
}

/// Check an MCP tool call.
pub async fn authorize_tool(state: &AppState, ctx: &Ctx, name: &str, args: &Value) -> AppResult<Perm> {
    let Some(perm) = tool_perm(name, args) else {
        return Err(AppError::Forbidden(format!("the tool {name:?} has no permission rule")));
    };
    require(ctx, perm)?;
    if name == "update_campaign" {
        if let Some(id) = args["id"].as_str().and_then(|s| s.parse::<Uuid>().ok()) {
            draft_only_check(state, ctx, id).await?;
        }
    }
    Ok(perm)
}

async fn draft_only_check(state: &AppState, ctx: &Ctx, campaign_id: Uuid) -> AppResult<()> {
    if allows(ctx, CampaignSend) {
        return Ok(());
    }
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM campaigns WHERE id = $1 AND account_id = $2").bind(campaign_id).bind(ctx.account_id).fetch_optional(&state.db).await?;
    if status.as_deref() == Some("scheduled") {
        return Err(AppError::Forbidden("this campaign is scheduled to send; only people who can send campaigns can change it".into()));
    }
    Ok(())
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
        assert!(Role::Admin.has(TeamManage) && Role::Admin.has(BillingWrite));
        assert!(!Role::Manager.has(TeamManage) && !Role::Manager.has(BillingRead) && !Role::Manager.has(ApiKeys) && !Role::Manager.has(Domains));
        assert!(Role::Manager.has(CampaignSend) && Role::Manager.has(FlowLive));
        assert!(Role::Analyst.has(Export) && !Role::Analyst.has(AudienceWrite) && !Role::Analyst.has(CampaignDraft));
        assert!(Role::Content.has(CampaignDraft) && Role::Content.has(ContentWrite));
        assert!(!Role::Content.has(CampaignSend) && !Role::Content.has(Export) && !Role::Content.has(ProfilesRead));
    }

    #[test]
    fn scopes() {
        assert!(scopes_allow(&s(&["full"]), CampaignSend));
        assert!(scopes_allow(&s(&["read"]), ProfilesRead));
        assert!(!scopes_allow(&s(&["read"]), AudienceWrite));
        assert!(scopes_allow(&s(&["audience"]), AudienceWrite));
        assert!(!scopes_allow(&s(&["audience"]), MessagingRead));
        assert!(scopes_allow(&s(&["messaging:read"]), MessagingRead));
        assert!(!scopes_allow(&s(&["messaging:read"]), CampaignSend));
        assert!(!scopes_allow(&s(&["full"]), Security), "keys never change security settings");
        assert!(scopes_allow(&s(&["read"]), Me));
        assert_eq!(normalize_scopes(&s(&["audience", "full"])).unwrap(), s(&["full"]));
        assert!(normalize_scopes(&s(&["admin"])).is_err());
        assert!(scopes_within(&s(&["audience:read"]), &s(&["audience"])));
        assert!(!scopes_within(&s(&["account"]), &s(&["audience"])));
        assert!(scopes_within(&s(&["read"]), &s(&["full"])));
    }

    #[test]
    fn every_core_tool_and_route_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for r in CORE_ROUTES {
            assert!(seen.insert((r.method, r.path)), "duplicate rule {} {}", r.method, r.path);
        }
    }
}
