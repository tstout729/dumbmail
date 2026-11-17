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
| Content | Edit templates, images and campaign drafts, and send test emails to teammates. Can't send or schedule campaigns, change a scheduled campaign, send test texts, see profiles or export. |

The server checks every request, whatever the dashboard shows. A request a role can't make gets
`403 forbidden` with a message naming the role.

Reports and the product catalog: everyone who sees analytics reads reports; exports (people's data)
need the export permission (analysts and up); saving, scheduling and deleting reports and changing
attribution are for managers and up. Everyone who sees content reads the catalog (who's waiting for
a restock, and picks for a person, also need permission to read profiles); managers and up change
products, feeds and alert settings; admins and up delete products and feeds.

Test sends: owners, admins and managers send tests to any address or number. Content members send
test **emails** only, and only to the email addresses of the account's members that already have
a profile (a manager's first test to an address makes one). API keys need the `messaging` scope.

Rules for owners:

- Only owners can make someone an owner (invite as owner, promote, resend an owner invite), demote
  or remove an owner, or transfer ownership. These only work in the dashboard, from a session
  that re-entered its password or a two-factor code in the last 10 minutes
  (`POST /api/me/reauth` `{password}` or `{code}`). Never through an API key or MCP.
- The last owner can't be removed or demoted. Make someone else an owner first, or transfer
  ownership (`POST /api/team/transfer`), which makes the other member the owner and you an admin.
- An owner invite stops working if the owner who sent it isn't an owner anymore.
- Only owners set up single sign-on.

## Members and invites

- `GET /api/team`: members (role, two-factor status, last active) and open invites.
- `POST /api/team/invites` `{email, role}`: emails an invite link. Links work for 7 days. When the
  server has no `SUPPORT_EMAIL`, the response includes `invite_url` to pass on yourself.
- `POST /api/team/invites/{id}/resend`: a new link (the old one stops working) and 7 more days.
- `DELETE /api/team/invites/{id}`: revoke an invite.
- `PATCH /api/team/members/{user_id}` `{role}`, `DELETE /api/team/members/{user_id}`: change a role,
  or remove someone. Removing someone also revokes the API keys they made in this account, and
  single sign-on won't add them back until an owner or admin invites them again.
- `DELETE /api/team/members/{user_id}/sso` (owners): unlink a member from the account's identity
  provider, for example after the provider gave them a new identity. Their single sign-on
  sessions here end; their next single sign-on links by verified email again.
- `POST /api/team/leave`: take yourself off the account.

Accepting an invite (`/invite/{token}` in the dashboard, `POST /api/invites/{token}/accept`):

- Signed in: the login's email must be the invited email, and the sign-in must be finished (not
  waiting on a two-factor code). A single sign-on session can't accept invites to other accounts.
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

The dashboard sends the account a page was loaded for on every change (`X-DumbMail-Account:
<account id>`). If the session has since opened another account (in another tab), the change is
refused with `409 account_changed` and the page reloads. A browser-session change without the
header is refused the same way when the login belongs to more than one account. API keys belong
to one account and don't send it.

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
someone's Google account uses two-step verification, so they enter our code too.

A single sign-on sign-in counts as two-factor only when the ID token's `amr` claim shows a second
factor (`mfa`, `otp`, `hwk`, `swk`, `sms`, `fpt`, `face` and the other RFC 8176 values that mean
one), or when the owner ticked "Our identity provider enforces two-step verification". Otherwise
the person also enters their DumbMail code. A login that exists only through single sign-on (no
password, no Google) can set its code up in that session; others set it up after signing in with
their password or Google.

Other two-factor routes: `GET /api/me/two-factor`, `POST /api/me/two-factor/disable` `{code}`,
`POST /api/me/two-factor/recovery-codes` `{code}` (replaces them). Two-factor can't be turned off
while an account you belong to requires it. Turning two-factor on or off, or making new recovery
codes, signs out your sessions that were still waiting on a code (and, when turning it on, password
sessions that never entered one).

## Sessions and passwords

- `GET /api/me/sessions`: your signed-in sessions with device, address and last activity.
- `DELETE /api/me/sessions/{id}`, `POST /api/me/sessions/revoke-others`: sign them out.
- `POST /api/me/password` `{current_password, new_password}`: changes it and signs out every other
  session.
- `POST /api/auth/password/forgot` `{email}` emails a link that works for an hour;
  `POST /api/auth/password/reset` `{token, password}` sets the new password and signs out every
  session. The forgot answer is the same whether or not the email has a login.

Sign-in, reset, invite and code attempts are rate limited and answer `429 busy` when
exceeded. For password sign-in only failures count: 30 per address and 10 per email from one
address in 15 minutes. Failures for one email from everywhere only make the next check wait a
little (up to 3 seconds), so strangers guessing can't lock the real person out. A password change
or reset also stops any sign-in that checked the old password from getting a session.

## API keys

Keys (`/api/api-keys`, Settings → Developers) have scopes and an optional expiry:

| Scope | Allows |
| --- | --- |
| `full` | Everything a key can do (the default; keys made before scopes keep it). |
| `read` | Reading every area except the team and the audit log. |
| `audience`, `messaging`, `content`, `analytics`, `account` | Reading and changing that area. |
| `audience:read` (and so on) | Reading that area. `account:read` includes the team and the audit log. |
| `transactional.send` | Sending transactional messages only. |

Areas: audience is profiles, lists, segments and events; messaging is campaigns and flows
(including sending and turning flows live); content is templates, forms and images; analytics is
reports and metrics; account is settings, domains, billing, team, API keys and the audit log.

A key also can't do more than the member who made it: a key made by a manager can't change
billing even with `full`. Keys stop working when that member leaves. Keys made before teams act as
an owner. Keys never change security settings (two-factor requirement, single sign-on), and never
make, demote or remove owners or transfer ownership.

`POST /api/api-keys` `{name, scopes, expires_in_days}` returns the key once. A key can make keys,
but only with scopes it has itself, never expiring after itself, and those keys stop working when
it's revoked or expires (revoking it also revokes them). MCP tools `list_api_keys`,
`create_api_key` and `revoke_api_key` need a key with the `account` (or `full`) scope; `list_team`
and `get_audit_log` need `account` or `account:read`.

MCP uses the same rules: each tool needs a permission, and a read-only key gets an error from any
tool that changes something.

## Audit log

Who did what and when: sign-ins and failed sign-ins, team and role changes, API key changes, sends,
schedule changes and cancellations, flow changes and status changes, imports, exports, deletes,
domain, phone number, billing, SMS provider and settings changes, two-factor and single sign-on
changes. Each entry has the member or API key, the address and browser where known, the action,
what it was done to and a few fixed details for that action. It records what happened, not what
was asked: entries are written where a change is committed (a test that went out is recorded even
when the next one fails), dry runs aren't recorded, and request bodies and MCP arguments are never
copied in. Entries never hold secrets or message content. They're kept 13 months.

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
 "domains": ["acme.com"], "enforce": false, "auto_join": true, "default_role": "analyst",
 "verified_emails_only": false, "idp_enforces_mfa": false}
```

- Register `redirect_uri` from the response (`https://<your DumbMail>/api/auth/sso/callback`) with
  the identity provider.
- Each domain must itself be one of the account's verified sending domains (add `acme.com` in
  Settings → Domains even if you send from `mail.acme.com`), and only one account can use single
  sign-on for a domain.
- An existing DumbMail login is connected to its single sign-on identity only when it's already a
  member of this account and the ID token says the email is verified (`email_verified: true`). A
  missing claim counts as unverified unless the owner turned on `verified_emails_only` ("This
  provider only issues verified emails"). People who have a login but aren't members yet need an
  invite first.
- A login linked to one identity at the provider is never moved to another by an email match; an
  owner resets the link first (`DELETE /api/team/members/{user_id}/sso`).
- `auto_join` adds people from those domains the first time they sign in, as `default_role`
  (manager, analyst or content). Without it, only existing members can sign in this way. Someone
  removed from the account isn't added back until an owner or admin invites them again; signing
  in through the provider then accepts that invite.
- `idp_enforces_mfa` ("Our identity provider enforces two-step verification"): see Two-factor login.
- Turning single sign-on off, removing it, or changing the issuer or client ID signs out everyone
  who signed in through it.
- `enforce` makes members on those domains sign in with single sign-on. Owners can still use
  their password, so a broken identity provider can't lock everyone out.
- The client secret is stored encrypted and never shown again; responses show its last 4
  characters.

People sign in at `/api/auth/sso?email=<their email>` (the login page's "Sign in with SSO"). A
single sign-on session opens only that account, and can't change the login's password, two-factor
or other sessions.

SAML isn't supported.
