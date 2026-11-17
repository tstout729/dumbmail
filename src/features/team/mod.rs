//! Team members and roles, several accounts per login, two-factor login, sessions and passwords,
//! scoped API keys, the audit log and single sign-on (OpenID Connect).
//!
//! Permissions for every route and MCP tool in the app are checked in one place: see [`access`].

pub mod access;
pub mod audit;
pub mod guard;
pub mod keys;
pub mod login;
pub mod mail;
pub mod members;
pub mod ratelimit;
pub mod sends;
pub mod sso;

use async_trait::async_trait;
use axum::routing::{delete, get, patch, post};
use axum::Router;
use serde_json::{json, Value};
use uuid::Uuid;

pub use access::{Perm, Role};
pub use audit::audit;

use self::access::{public, rule, Access, RouteRule, ToolRule};
use super::{Feature, McpDoc};
use crate::auth::Ctx;
use crate::error::{AppError, AppResult};
use crate::mcp::{from, obj, tool};
use crate::state::AppState;

pub struct Module;

use Perm::*;

static ROUTE_RULES: &[RouteRule] = &[
    rule("GET", "/api/team", TeamRead),
    rule("POST", "/api/team/invites", TeamManage),
    rule("POST", "/api/team/invites/{id}/resend", TeamManage),
    rule("DELETE", "/api/team/invites/{id}", TeamManage),
    rule("PATCH", "/api/team/members/{user_id}", TeamManage),
    rule("DELETE", "/api/team/members/{user_id}", TeamManage),
    rule("DELETE", "/api/team/members/{user_id}/sso", Ownership),
    rule("POST", "/api/team/leave", OwnLogin),
    rule("POST", "/api/team/transfer", Ownership),
    rule("GET", "/api/team/security", TeamSecurityRead),
    rule("PATCH", "/api/team/security", Security),
    rule("GET", "/api/team/sso", Ownership),
    rule("PUT", "/api/team/sso", Ownership),
    rule("DELETE", "/api/team/sso", Ownership),
    public("GET", "/api/invites/{token}"),
    public("POST", "/api/invites/{token}/accept"),
    rule("GET", "/api/accounts", SwitchAccount),
    rule("POST", "/api/accounts", OwnLogin),
    rule("POST", "/api/accounts/switch", SwitchAccount),
    rule("POST", "/api/auth/two-factor", TwoFactorStep),
    rule("GET", "/api/me/two-factor", TwoFactorStep),
    rule("POST", "/api/me/two-factor/setup", TwoFactorStep),
    rule("POST", "/api/me/two-factor/enable", TwoFactorStep),
    rule("POST", "/api/me/two-factor/disable", OwnLogin),
    rule("POST", "/api/me/two-factor/recovery-codes", OwnLogin),
    rule("GET", "/api/me/sessions", OwnLogin),
    rule("DELETE", "/api/me/sessions/{id}", OwnLogin),
    rule("POST", "/api/me/sessions/revoke-others", OwnLogin),
    rule("POST", "/api/me/password", OwnLogin),
    rule("POST", "/api/me/reauth", ConfirmIdentity),
    public("POST", "/api/auth/password/forgot"),
    public("POST", "/api/auth/password/reset"),
    public("GET", "/api/auth/sso"),
    public("GET", "/api/auth/sso/callback"),
    rule("GET", "/api/api-keys", ApiKeys),
    rule("POST", "/api/api-keys", ApiKeys),
    rule("DELETE", "/api/api-keys/{id}", ApiKeys),
    rule("GET", "/api/audit", AuditRead),
    rule("GET", "/api/audit/export", AuditRead),
];

pub(crate) static TOOL_RULES: &[ToolRule] = &[
    ToolRule { name: "list_team", perm: TeamRead },
    ToolRule { name: "invite_member", perm: TeamManage },
    ToolRule { name: "resend_invite", perm: TeamManage },
    ToolRule { name: "revoke_invite", perm: TeamManage },
    ToolRule { name: "change_member_role", perm: TeamManage },
    ToolRule { name: "remove_member", perm: TeamManage },
    ToolRule { name: "get_audit_log", perm: AuditRead },
    ToolRule { name: "list_api_keys", perm: ApiKeys },
    ToolRule { name: "create_api_key", perm: ApiKeys },
    ToolRule { name: "revoke_api_key", perm: ApiKeys },
];

fn uuid_arg(a: &Value, k: &str) -> AppResult<Uuid> {
    a[k].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| AppError::bad(format!("`{k}` must be an id")))
}

fn confirmed(a: &Value) -> bool {
    a["confirm"].as_bool() == Some(true)
}

#[async_trait]
impl Feature for Module {
    fn name(&self) -> &'static str {
        "team"
    }

    fn routes(&self) -> Router<AppState> {
        Router::new()
            .route("/api/team", get(members::team_h))
            .route("/api/team/invites", post(members::invite_h))
            .route("/api/team/invites/{id}/resend", post(members::resend_h))
            .route("/api/team/invites/{id}", delete(members::revoke_invite_h))
            .route("/api/team/members/{user_id}", patch(members::change_role_h).delete(members::remove_h))
            .route("/api/team/members/{user_id}/sso", delete(members::reset_sso_h))
            .route("/api/team/leave", post(members::leave_h))
            .route("/api/team/transfer", post(members::transfer_h))
            .route("/api/team/security", get(members::security_h).patch(members::update_security_h))
            .route("/api/team/sso", get(sso::get_h).put(sso::put_h).delete(sso::delete_h))
            .route("/api/invites/{token}", get(members::show_invite_h))
            .route("/api/invites/{token}/accept", post(members::accept_h))
            .route("/api/accounts", get(members::accounts_h).post(members::create_account_h))
            .route("/api/accounts/switch", post(members::switch_h))
            .route("/api/auth/two-factor", post(login::verify_session_h))
            .route("/api/me/two-factor", get(login::status_h))
            .route("/api/me/two-factor/setup", post(login::setup_h))
            .route("/api/me/two-factor/enable", post(login::enable_h))
            .route("/api/me/two-factor/disable", post(login::disable_h))
            .route("/api/me/two-factor/recovery-codes", post(login::recovery_codes_h))
            .route("/api/me/sessions", get(login::sessions_h))
            .route("/api/me/sessions/{id}", delete(login::revoke_session_h))
            .route("/api/me/sessions/revoke-others", post(login::revoke_others_h))
            .route("/api/me/password", post(login::change_password_h))
            .route("/api/me/reauth", post(login::reauth_h))
            .route("/api/auth/password/forgot", post(login::forgot_h))
            .route("/api/auth/password/reset", post(login::reset_h))
            .route("/api/auth/sso", get(sso::start_h))
            .route("/api/auth/sso/callback", get(sso::callback_h))
            .route("/api/api-keys", get(keys::list_h).post(keys::create_h))
            .route("/api/api-keys/{id}", delete(keys::revoke_h))
            .route("/api/audit", get(audit::list_h))
            .route("/api/audit/export", get(audit::export_h))
    }

    fn access(&self) -> Access {
        Access { routes: ROUTE_RULES, tools: TOOL_RULES }
    }

    fn mcp_tools(&self) -> Vec<Value> {
        let roles = json!({"enum": ["owner", "admin", "manager", "analyst", "content"], "description": "owner: everything; admin: everything but ownership; manager: campaigns, flows, forms, audience and content; analyst: read-only plus exports; content: templates and campaign drafts, no sending."});
        let scopes = json!({"type": "array", "maxItems": 20, "items": {"enum": access::valid_scopes()}, "description": "full (default), read (everything read-only except the team and audit log), an area (audience, messaging, content, analytics, account) for read and write, <area>:read, or transactional.send."});
        vec![
            tool("list_team", "List team members", "Members of this account with their roles. For admins and owners also their two-factor and single sign-on status, last activity, and open invites.", obj(json!({}), &[]), true, false),
            tool("invite_member", "Invite a team member", "Email someone an invite to join this account with a role. Without confirm=true it only checks the invite and describes it. Owner invites can only be sent from the dashboard by an owner who just confirmed their password or code, never through MCP.",
                obj(json!({"email": {"type": "string", "maxLength": 254}, "role": roles.clone(), "confirm": {"type": "boolean"}}), &["email", "role"]), false, true),
            tool("resend_invite", "Resend an invite", "Send an open invite again with a new link (the old link stops working) and another 7 days. Requires confirm=true.",
                obj(json!({"invite_id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["invite_id"]), false, true),
            tool("revoke_invite", "Revoke an invite", "Cancel an open invite so its link stops working.", obj(json!({"invite_id": {"type": "string"}}), &["invite_id"]), false, true),
            tool("change_member_role", "Change a member's role", "Give a member another role. Making, demoting or removing owners only works in the dashboard (an owner who just confirmed their password or code), never through MCP; the last owner can't be demoted.",
                obj(json!({"user_id": {"type": "string"}, "role": roles}), &["user_id", "role"]), false, false),
            tool("remove_member", "Remove a team member", "Take someone off this account and revoke the API keys they made here. The last owner can't be removed. Without confirm=true it only describes what would happen.",
                obj(json!({"user_id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["user_id"]), false, true),
            tool("get_audit_log", "Read the audit log", "Who did what and when in this account, newest first: sign-ins, team and role changes, API keys, sends, schedule and flow status changes, imports, exports, deletes and settings changes. Filter by action (exact, or a prefix ending in a dot like team.), user_id, key_id, words (q) and dates (from/to, YYYY-MM-DD in the account's time zone).",
                obj(json!({"action": {"type": "string"}, "user_id": {"type": "string"}, "key_id": {"type": "string"}, "q": {"type": "string"}, "from": {"type": "string"}, "to": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200}, "offset": {"type": "integer", "minimum": 0}}), &[]), true, false),
            tool("list_api_keys", "List API keys", "The account's API keys: name, prefix, scopes, expiry, who made it and when it was last used. Needs a key with the account scope.", obj(json!({}), &[]), true, false),
            tool("create_api_key", "Create an API key", "Make an API key with scopes and an optional expiry. The key is shown once. A key can only make keys with scopes it has itself. Needs a key with the account scope.",
                obj(json!({"name": {"type": "string", "maxLength": 100}, "scopes": scopes, "expires_in_days": {"type": "integer", "minimum": 1, "maximum": 3650}}), &["name"]), false, false),
            tool("revoke_api_key", "Revoke an API key", "Stop an API key from working. Anything using it fails right away. Requires confirm=true. Needs a key with the account scope.",
                obj(json!({"id": {"type": "string"}, "confirm": {"type": "boolean"}}), &["id"]), false, true),
        ]
    }

    async fn mcp_call(&self, state: &AppState, ctx: &Ctx, name: &str, a: &Value) -> Option<AppResult<Value>> {
        let a = a.clone();
        Some(match name {
            "list_team" => members::team_json(state, ctx).await,
            "invite_member" => match from::<members::InviteBody>(json!({"email": a["email"], "role": a["role"]})) {
                Ok(b) => members::invite(state, ctx, b, !confirmed(&a)).await,
                Err(e) => Err(e),
            },
            "resend_invite" => match uuid_arg(&a, "invite_id") {
                Ok(id) if confirmed(&a) => members::resend(state, ctx, id).await,
                Ok(id) => Ok(json!({"dry_run": true, "invite_id": id, "message": "Sends the invite email again with a new link. Call again with confirm=true."})),
                Err(e) => Err(e),
            },
            "revoke_invite" => match uuid_arg(&a, "invite_id") {
                Ok(id) => members::revoke_invite(state, ctx, id).await,
                Err(e) => Err(e),
            },
            "change_member_role" => match uuid_arg(&a, "user_id") {
                Ok(id) => members::change_role(state, ctx, id, a["role"].as_str().unwrap_or("")).await,
                Err(e) => Err(e),
            },
            "remove_member" => match uuid_arg(&a, "user_id") {
                Ok(id) => members::remove(state, ctx, id, !confirmed(&a)).await,
                Err(e) => Err(e),
            },
            "get_audit_log" => match from::<audit::AuditQuery>(a) {
                Ok(q) => audit::list(state, ctx.account_id, &q).await,
                Err(e) => Err(e),
            },
            "list_api_keys" => keys::list(state, ctx.account_id).await,
            "create_api_key" => match from::<keys::KeyBody>(a) {
                Ok(b) => keys::create(state, ctx, b).await,
                Err(e) => Err(e),
            },
            "revoke_api_key" => match uuid_arg(&a, "id") {
                Ok(id) if confirmed(&a) => keys::revoke(state, ctx, id).await,
                Ok(id) => Ok(json!({"dry_run": true, "id": id, "message": "Revoking stops this key right away; anything using it fails. Call again with confirm=true."})),
                Err(e) => Err(e),
            },
            _ => return None,
        })
    }

    fn mcp_docs(&self) -> Vec<McpDoc> {
        vec![McpDoc {
            slug: "team",
            name: "Team, roles and security",
            description: "Roles and what each can do, API key scopes, two-factor login, single sign-on and the audit log",
            text: include_str!("../../../docs/TEAM.md"),
        }]
    }

    async fn tick(&self, state: &AppState, tick: u64) -> anyhow::Result<()> {
        // About every 20 minutes.
        if tick.is_multiple_of(60) {
            audit::prune(state).await?;
            ratelimit::prune(state).await?;
            sqlx::query("DELETE FROM sessions WHERE expires_at < now() - interval '1 day'").execute(&state.db).await?;
            sqlx::query("DELETE FROM password_resets WHERE expires_at < now() - interval '1 day'").execute(&state.db).await?;
        }
        Ok(())
    }
}
