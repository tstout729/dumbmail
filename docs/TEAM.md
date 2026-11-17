# Team, roles and account security

An account can have many members, each with a role. One login can belong to several accounts
(an agency running several brands, or one company with several stores) and switch between them.
Two-factor login, single sign-on, session controls, limited API keys and an audit log keep the
account safe.

Everything here works in the dashboard (Settings → Team and Settings → Security), over the REST
API and, for members, invites, API keys and the audit log, over MCP.

## Roles

| Role | Can |
| --- | --- |
| Owner | Everything, including billing and transferring ownership. |
| Admin | Everything except transferring ownership and setting up single sign-on. |
| Manager | Campaigns (including sending), flows (including turning them live), forms, audience, content and account settings. No billing, team, API keys, domains or phone numbers. |
| Analyst | Read everything except billing, team management and the audit log, and export. Changes nothing. |
| Content | Edit templates, images and campaign drafts, and send tests. Can't send or schedule campaigns, change a scheduled campaign, see profiles or export. |

The server checks every request, whatever the dashboard shows. A request a role can't make gets
`403 forbidden` with a message naming the role.

Rules for owners:

- Only owners can make someone an owner, change an owner's role or remove an owner.
- The last owner can't be removed or demoted. Make someone else an owner first, or transfer
  ownership (`POST /api/team/transfer`), which makes the other member the owner and you an admin.
- Only owners set up single sign-on.

## Members and invites

- `GET /api/team`: members (role, two-factor status, last active) and open invites.
- `POST /api/team/invites` `{email, role}`: emails an invite link. Links work for 7 days. When the
  server has no `SUPPORT_EMAIL`, the response includes `invite_url` to pass on yourself.
- `POST /api/team/invites/{id}/resend`: a new link (the old one stops working) and 7 more days.
- `DELETE /api/team/invites/{id}`: revoke an invite.
- `PATCH /api/team/members/{user_id}` `{role}`, `DELETE /api/team/members/{user_id}`: change a role,
  or remove someone. Removing someone also revokes the API keys they made in this account.
- `POST /api/team/leave`: take yourself off the account.

Accepting an invite (`/invite/{token}` in the dashboard, `POST /api/invites/{token}/accept`):

- Signed in: the login's email must be the invited email.
- Not signed in: give a name and a password to create a login for the invited email, or use
  Continue with Google with that address. If the email already has a login, sign in first.

MCP tools: `list_team`, `invite_member`, `resend_invite`, `revoke_invite`, `change_member_role`,
`remove_member`. Inviting, resending and removing need `confirm: true`; without it they only
describe what would happen.

## Several accounts per login

- `GET /api/me` lists the login's accounts in `accounts`.
- `POST /api/accounts/switch` `{account_id}` opens another account in this session.
- `POST /api/accounts` `{name}` starts a new account with you as its owner and opens it.

Each session has one active account, and every request only sees that account's data. A login
can belong to up to 50 accounts.

## Two-factor login

Members can turn on two-factor login with an authenticator app (TOTP, RFC 6238):

1. `POST /api/me/two-factor/setup` returns a QR code, the `otpauth://` link and the secret.
2. `POST /api/me/two-factor/enable` `{code}` confirms it and returns 10 one-time recovery codes.
   They're shown once; only their hashes are kept.

After that, password sign-ins need a code: the session starts restricted (`restriction:
"two_factor_code"` in `/api/me`) until `POST /api/auth/two-factor` `{code}` succeeds. A recovery
code works in place of the app's code, once. Each app code works once, and attempts are limited
(6 per 10 minutes).

Owners and admins can require two-factor login for every member (`PATCH /api/team/security`
`{require_two_factor: true}`). Members without it must set it up at their next request
(`restriction: "two_factor_setup"`). This also applies to Google sign-ins: we can't tell whether
someone's Google account uses two-step verification, so they enter our code too. Single sign-on
sign-ins are exempt; the identity provider handles that.

Other two-factor routes: `GET /api/me/two-factor`, `POST /api/me/two-factor/disable` `{code}`,
`POST /api/me/two-factor/recovery-codes` `{code}` (replaces them). Two-factor can't be turned off
while an account you belong to requires it.

## Sessions and passwords

- `GET /api/me/sessions`: your signed-in sessions with device, address and last activity.
- `DELETE /api/me/sessions/{id}`, `POST /api/me/sessions/revoke-others`: sign them out.
- `POST /api/me/password` `{current_password, new_password}`: changes it and signs out every other
  session.
- `POST /api/auth/password/forgot` `{email}` emails a link that works for an hour;
  `POST /api/auth/password/reset` `{token, password}` sets the new password and signs out every
  session. The forgot answer is the same whether or not the email has a login.

Sign-in, reset, invite and code attempts are rate limited per address and per email, and answer
`429 rate_limited` when exceeded.

## API keys

Keys (`/api/api-keys`, Settings → Developers) have scopes and an optional expiry:

| Scope | Allows |
| --- | --- |
| `full` | Everything a key can do (the default; keys made before scopes keep it). |
| `read` | Reading every area. |
| `audience`, `messaging`, `content`, `analytics`, `account` | Reading and changing that area. |
| `audience:read` (and so on) | Reading that area. |

Areas: audience is profiles, lists, segments and events; messaging is campaigns and flows
(including sending and turning flows live); content is templates, forms and images; analytics is
reports and metrics; account is settings, domains, billing, team, API keys and the audit log.

A key also can't do more than the member who made it: a key made by a manager can't change
billing even with `full`. Keys stop working when that member leaves. Keys made before teams act as
an owner. Keys never change security settings (two-factor requirement, single sign-on) or transfer
ownership.

`POST /api/api-keys` `{name, scopes, expires_in_days}` returns the key once. A key can make keys,
but only with scopes it has itself. MCP tools `list_api_keys`, `create_api_key` and
`revoke_api_key` need a key with the `account` (or `full`) scope.

MCP uses the same rules: each tool needs a permission, and a read-only key gets an error from any
tool that changes something.

## Audit log

Who did what and when: sign-ins and failed sign-ins, team and role changes, API key changes, sends,
schedule changes and cancellations, flow changes and status changes, imports, exports, deletes,
domain, phone number, billing, SMS provider and settings changes, two-factor and single sign-on
changes. Each entry has the member or API key, the address and browser where known, the action,
what it was done to and a few details. Entries never hold secrets or message content. They're kept
13 months.

- `GET /api/audit?action=&user_id=&key_id=&q=&from=&to=&limit=&offset=` (owners and admins).
  `action` is exact (`campaign.send`) or a prefix ending in a dot (`team.`). `from` and `to` are
  days (YYYY-MM-DD) in the account's time zone.
- `GET /api/audit/export` with the same filters downloads a CSV (up to 50,000 rows).
- MCP: `get_audit_log`.

Other modules record their own actions with `features::team::audit(state, ctx, action, target,
details)`.

## Single sign-on (OpenID Connect)

Owners connect an OpenID Connect identity provider (Okta, Microsoft Entra ID, Google
Workspace, Auth0 and others) in Settings → Security, or with `PUT /api/team/sso`:

```json
{"issuer": "https://acme.okta.com", "client_id": "…", "client_secret": "…",
 "domains": ["acme.com"], "enforce": false, "auto_join": true, "default_role": "analyst"}
```

- Register `redirect_uri` from the response (`https://<your DumbMail>/api/auth/sso/callback`) with
  the identity provider.
- Each domain must itself be one of the account's verified sending domains (add `acme.com` in
  Settings → Domains even if you send from `mail.acme.com`), and only one account can use single
  sign-on for a domain.
- An existing DumbMail login is connected to its single sign-on identity only when it's already a
  member of this account. People who have a login but aren't members yet need an invite first.
- `auto_join` adds people from those domains the first time they sign in, as `default_role`
  (manager, analyst or content). Without it, only existing members can sign in this way.
- `enforce` makes members on those domains sign in with single sign-on. Owners can still use
  their password, so a broken identity provider can't lock everyone out.
- The client secret is stored encrypted and never shown again; responses show its last 4
  characters.

People sign in at `/api/auth/sso?email=<their email>` (the login page's "Sign in with SSO"). A
single sign-on session opens only that account, and can't change the login's password, two-factor
or other sessions.

SAML isn't supported.
