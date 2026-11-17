import type { Session } from "../../api/types";

export type Role = "owner" | "admin" | "manager" | "analyst" | "content";

export const roleLabels: Record<Role, string> = {
  owner: "Owner",
  admin: "Admin",
  manager: "Manager",
  analyst: "Analyst",
  content: "Content",
};

export const roleDescriptions: Record<Role, string> = {
  owner: "Everything, including billing and transferring ownership.",
  admin: "Everything except transferring ownership.",
  manager: "Campaigns, flows, forms, audience and content, including sending. No billing, team, API keys or domains.",
  analyst: "Sees everything except billing and team settings, and can export. Changes nothing.",
  content: "Edits templates and campaign drafts. Can't send, schedule or see profiles.",
};

export const roles: Role[] = ["owner", "admin", "manager", "analyst", "content"];

/** What the session still has to do before it can use the account. */
export type Restriction = "two_factor_code" | "two_factor_setup" | "sso_required";

export type AccountChoice = { id: string; name: string; brand_color: string; role: Role; current: boolean };

/** Fields `/api/me` adds for teams and sign-in. */
export type TeamSessionInfo = {
  role: Role;
  permissions: string[];
  restriction: Restriction | null;
  restriction_message: string | null;
  sign_in_method: "sso" | "other" | null;
  two_factor_enabled: boolean;
  accounts: AccountChoice[];
};

export type TeamSession = Session & TeamSessionInfo;

export type Member = {
  user_id: string;
  email: string;
  name: string | null;
  role: Role;
  joined_at: string;
  two_factor: boolean;
  last_active_at: string | null;
  sso_linked: boolean;
};

export type Invite = {
  id: string;
  email: string;
  role: Role;
  invited_by: string | null;
  created_at: string;
  sent_at: string;
  expires_at: string;
  expired: boolean;
};

export type Team = { members: Member[]; invites: Invite[] };

export type InviteResult = { invite: { id: string; email: string; role: Role; expires_at: string }; email_sent: boolean; invite_url?: string };

export type PublicInvite = {
  account_name: string;
  email: string;
  role: Role;
  invited_by: string | null;
  expires_at: string;
  has_login: boolean;
  signed_in_as: string | null;
};

export type TwoFactorStatus = { enabled: boolean; enabled_at: string | null; setup_started: boolean; recovery_codes_left: number; required_by: string[] };

export type TwoFactorSetup = { secret: string; otpauth_url: string; qr_code: string };

export type SessionRow = {
  id: string;
  created_at: string;
  last_seen_at: string;
  ip: string | null;
  device: string;
  user_agent: string | null;
  sign_in_method: string;
  account: string;
  current: boolean;
};

export type SsoSettings = {
  enabled: boolean;
  issuer: string;
  client_id: string;
  client_secret_hint: string;
  domains: string[];
  enforce: boolean;
  auto_join: boolean;
  default_role: Role;
  verified_emails_only: boolean;
  idp_enforces_mfa: boolean;
  redirect_uri: string;
  login_url: string;
};

export type SsoInput = {
  issuer: string;
  client_id: string;
  client_secret?: string;
  domains: string[];
  enforce: boolean;
  auto_join: boolean;
  default_role: Role;
  enabled: boolean;
  verified_emails_only: boolean;
  idp_enforces_mfa: boolean;
};

export type AccountSecurity = { require_two_factor: boolean; members_without_two_factor: number; sso: SsoSettings | null };

export type AuditEntry = {
  id: string;
  created_at: string;
  local_time: string;
  actor: { type: "user" | "api_key" | "system"; user_id: string | null; key_id: string | null; label: string | null };
  action: string;
  target: string | null;
  details: Record<string, unknown>;
  ip: string | null;
  user_agent: string | null;
};

export type AuditFilters = { action?: string; q?: string; from?: string; to?: string; offset?: number };

export type AuditPage = { data: AuditEntry[]; total: number; timezone: string };

export type ScopedApiKey = {
  id: string;
  name: string;
  prefix: string;
  scopes: string[];
  created_at: string;
  last_used_at: string | null;
  expires_at: string | null;
  expired: boolean;
  created_by: string | null;
};

export type CreatedScopedKey = { id: string; name: string; prefix: string; scopes: string[]; expires_at: string | null; key: string };

export type NewKeyInput = { name: string; scopes: string[]; expires_in_days?: number };
