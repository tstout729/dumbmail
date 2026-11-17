# Ad audiences

Sync a list or segment to Meta (Facebook and Instagram), Google Ads or TikTok as a customer-list audience. DumbMail creates the audience on the platform and keeps it current: people who join the list or segment are added, and people who leave or opt out are removed. Use it to advertise to your best customers, to leave recent buyers out of a campaign, or to build lookalike audiences.

None of the three platforms has been checked against a live ad account yet. Each shows "Not yet verified against a live account" until someone confirms a real match and a real removal (see "Not verified yet" below).

## Who is sent

A person is sent to an ad platform only if **all** of these are true, checked by one rule every time DumbMail picks people:

- they're in the list or segment the sync uses;
- their email consent is **subscribed** (not "never subscribed", pending or unsubscribed);
- their email isn't suppressed (bounced or complained);
- they aren't on a marketing hold;
- they haven't opted out of ads: none of the profile properties `ads_opt_out`, `do_not_sell` or `gpc` is set, and `ads_consent` isn't set to a "no".

A property counts as set unless its value is `false`, `"false"`, `"0"`, `"no"`, empty or missing (spaces around it and upper or lower case don't matter). Anything else, such as `true`, `"yes"`, `" TRUE "`, `1` or `"denied"`, counts as an opt-out. `ads_consent` never adds anyone: it can only take people out.

What a visitor's browser reports (the website script) can only take people out. A browser can't change anyone's consent, and a phone number filled in from a browser isn't sent: a phone number is sent only if that person opted in to texts with it. A segment can still use rules about events and properties; the rule above always applies on top.

There is no way to send people who unsubscribed, even for an audience you only use to exclude people from ads: an ad platform can't be made to use a list for exclusion only.

People are rechecked right before each batch is sent, and again at the end of every run. Someone who opts out, is deleted or changes their address while a sync is running isn't sent, and if they were already sent they're removed in the same run.

## What is sent

Only identifiers the platform can match on, normalized the way each platform documents, and **SHA-256 hashed on DumbMail's server before they're sent**. Raw email addresses and phone numbers never leave DumbMail and are never logged.

| Platform | Sent |
| --- | --- |
| Meta | email, and phone (opted in to texts). Optionally ("Email, phone, name and location") also hashed first and last name, city, state, ZIP and country. |
| Google Ads | email and phone (opted in to texts), hashed. Nothing else, so nothing about where people live is sent. |
| TikTok | email only. TikTok audiences take one kind of identifier each. |

Location fields come from the profile's `city`, `region` (or `state`), `zip` (or `postal_code`) and `country` properties.

DumbMail works in identifiers rather than profiles: one hashed email or phone number is one entry on the platform. If two profiles hash to the same entry (for example `a.b+shop@gmail.com` and `ab@gmail.com` on Google), the entry stays while either still qualifies, and is removed as soon as either opts out.

## Google: the consent setting

Google requires advertisers to have consent for using people's data for ads from users in the European Economic Area, the UK and Switzerland. Before anyone can create a Google sync, the account's owner or an admin turns on, in Settings → Ad audiences:

> The marketing consent we collect covers using customer data for advertising, as Google requires for users in the EEA, the UK and Switzerland.

DumbMail records who turned it on and when. With it on, everyone DumbMail sends to Google is sent with both of Google's consent signals (`adUserData` and `adPersonalization`) granted. With it off, Google syncs can't be created and existing ones pause (a paused sync still removes people; see below).

## How a sync runs

1. **Connect a platform** in Settings → Ad audiences. Someone signed in to DumbMail (the owner or an admin) signs in on the platform's own page and allows DumbMail to manage audiences. DumbMail stores the sign-in encrypted and never shows it again.
2. **Create a sync**: pick the platform, the ad account (on Google, type the customer ID, and the manager account ID if you reach it through one; DumbMail checks the sign-in can manage audiences there), and a list or segment. DumbMail creates an audience named after it ("VIPs (DumbMail)" unless you name it) and uploads everyone who qualifies.
3. **It stays in step**: every 6 hours by default (you can pick every hour up to once a week), plus **Sync now**. Each run works out who qualifies now, removes entries that no longer do, and adds new ones.

Runs happen in the background in small steps, 10,000 entries per request on Meta and Google and files of up to 50,000 on TikTok. If a platform asks DumbMail to slow down or has an outage, the run waits and tries again (1 minute, then 2, 4 and so on, up to an hour between tries).

Every batch is written down before it's sent: exactly which hashed entries, whether it adds or removes, and its id. If DumbMail stops in the middle (a crash, a deploy), the next attempt sends the same batch again; nothing is lost and nothing is counted twice. Only one worker runs a sync at a time.

## Status: what "sent" and "confirmed" mean

DumbMail never says something is done unless the platform said so.

- **Confirmed by the platform**: entries the platform confirmed it took. On Meta and TikTok that's the platform's answer to the request (Meta's count of entries received must match what was sent). On Google it's Google's processing result, which DumbMail checks after Google accepts the request.
- **Sent, not confirmed**: entries sent where the platform's answer couldn't be read, or it refused some of them. DumbMail treats them as possibly in the audience, so it removes them if those people stop qualifying, and tries adding them again on a later run.
- **Still pending removal**: removals the platform hasn't confirmed yet. They're retried on every run until it does, and are never shown as removed before that.
- **Size on the platform**: the platform's own estimate of matched people (Meta gives a range; Google gives its largest network size; TikTok its matched count).

Common problems, in plain words on the sync:

- **Sign-in expired or was revoked.** Reconnect the platform; syncs carry on. Meta sign-ins last about 60 days and DumbMail asks you to reconnect in their last week (it doesn't renew them on its own).
- **The ad account doesn't let you manage audiences.** The person who connected needs permission to manage audiences on that ad account. On Meta, also accept the Custom Audience terms for the ad account in Ads Manager.
- **Too small to use yet.** Platforms only run ads to audiences above a minimum: Google needs 100 active people for a new Customer Match list; TikTok needs 1,000.
- **The list or segment was deleted.** Nobody is added; people who opt out are still removed. Delete the sync, or the audience on the platform, if you don't need it.

## Pausing, TikTok's minimum and deleting

- **Pause** stops additions. A paused sync still runs on its schedule to remove people who leave or opt out, so opt-outs always reach the platform.
- **TikTok's minimum.** TikTok refuses any change that would leave an audience with fewer than 1,000 entries, including removals. When removals would do that (or TikTok refuses a removal), DumbMail deletes the whole TikTok audience, so nobody who left or opted out stays in it, and pauses the sync with the reason "below TikTok's minimum of 1,000 people". When 1,000 people qualify again it creates a new audience on its own. Before the first upload, a TikTok sync waits until 1,000 people qualify.
- **Delete** stops the sync for good and needs `confirm=true`. It can also delete the audience on the platform (ads that use it stop). If you keep the audience, DumbMail no longer removes people who opt out from it, so delete it on the platform when you can.

## Audiences DumbMail didn't make

DumbMail only ever touches an audience whose id it saved from its own create call. It never looks for an audience by name or description. If a platform says the saved audience doesn't exist, the sync pauses, keeps its records, and offers to create a new audience (**Create a new audience**, or `recreate_audience: true` with `confirm: true`). If a TikTok create's answer can't be read, TikTok may have made the audience anyway: the sync pauses and names the audience to look for, and never creates a second one on its own.

## API

All under `/api/ad-audiences`, with a session or `Authorization: Bearer <API key>`.

| Method and path | What it does |
| --- | --- |
| `GET /connections` | Each platform: `available` on this server, `status` (`not_connected`, `connected`, `needs_reconnect`), who connected, when the sign-in expires (`expires_soon` in its last week), whether the last disconnect's revoke was confirmed, plus the account's `settings`. |
| `POST /connections/{platform}/start` | Returns `{url}` to open in the browser to connect. Needs someone signed in (owner or admin), not an API key. |
| `DELETE /connections/{platform}` | Deletes DumbMail's sign-in and asks the platform to revoke it; `revoked` says whether the platform confirmed. |
| `GET /connections/{platform}/accounts` | Ad accounts the sign-in can reach (Meta, TikTok). Google returns `manual_entry: true`. |
| `GET /settings`, `PATCH /settings` | `google_consent` (true or false). Changing it needs a signed-in owner or admin; who and when is recorded. |
| `GET /syncs` | All syncs (`?source_type=list&source_id=…` to filter). |
| `POST /syncs` | Create: `platform`, `ad_account_id`, `source_type` (`list` or `segment`), `source_id`, `accept_terms: true`, optional `name`, `match_fields` (`contact`, or `contact_and_location` on Meta), `interval_hours` (1–168, default 6), and for Google `login_account_id` (the manager account ID). |
| `GET /syncs/{id}` | Status, the last 30 runs, and batches still waiting for the platform. |
| `PATCH /syncs/{id}` | `status` (`active` or `paused`), `interval_hours`, `match_fields`, or `recreate_audience: true` with `confirm: true`. |
| `POST /syncs/{id}/run` | Sync now. |
| `DELETE /syncs/{id}?confirm=true&delete_on_platform=true` | Delete the sync, and optionally the audience on the platform. Without `confirm=true` it returns what would happen. |

Sync status fields: `sent`, `confirmed`, `pending_removal` (hashed entries), `last_added` and `last_removed` (confirmed, last run), `platform_size`, `match_rate`, `status`, `pause_reason` (`user`, `google_consent`, `tiktok_minimum`, `audience_missing`, `create_unclear`) with `pause_message`, `error_code` with `error` in plain words, and `verified_live`.

`accept_terms: true` records that the people in the list gave you their contact details directly and that you accept the platform's customer list terms (Meta's Custom Audience terms, Google's Customer Match policy, TikTok's Custom Audience terms).

## MCP tools

`list_ad_connections`, `list_ad_accounts`, `list_audience_syncs`, `get_audience_sync`, `create_audience_sync`, `update_audience_sync`, `run_audience_sync`, `delete_audience_sync`, `disconnect_ad_platform`.

- Connecting needs a browser and a signed-in person: `list_ad_connections` returns `connect_url` (Settings → Ad audiences) to send the user to. The Google consent setting is changed there too.
- `create_audience_sync` sends customer data to an ad platform. Without `confirm: true` it returns a dry run with the number of people who qualify; pass `confirm: true` and `accept_terms: true` only after the user agrees.
- `update_audience_sync` needs `confirm: true` to turn on Meta's name and location matching, or to recreate an audience.
- `delete_audience_sync` and `disconnect_ad_platform` describe what would happen unless `confirm: true`.

## Permissions (for the team-and-roles layer)

Until roles land, signed-in members need to be the owner or an admin to connect, disconnect and change the Google consent setting; API keys can do everything except start a sign-in and change the consent setting. Intended permissions:

| Route or tool | Who |
| --- | --- |
| `POST /connections/{platform}/start`, `DELETE /connections/{platform}`, `disconnect_ad_platform` | Owner, admins |
| `PATCH /settings` (Google consent) | Owner, admins (a signed-in person; never an API key) |
| `POST /syncs`, `PATCH /syncs/{id}`, `POST /syncs/{id}/run`, `DELETE /syncs/{id}`, `create_audience_sync`, `update_audience_sync`, `run_audience_sync`, `delete_audience_sync` | Admins and the owner (they send customer data to third parties) |
| `GET /connections`, `GET /connections/{platform}/accounts`, `GET /settings`, `GET /syncs`, `GET /syncs/{id}`, `list_ad_connections`, `list_ad_accounts`, `list_audience_syncs`, `get_audience_sync` | Anyone who can see the audience |
| `GET /oauth/{platform}/callback` | The signed-in person who started that sign-in |

## Privacy records

Two tables hold what DumbMail sent, and a profile merge or delete should treat them like this:

- `ad_audience_members`: the hashed identifiers that may be in each platform audience, with no profile id. Keep these rows when a profile is deleted or merged: they're how the removal is sent after the profile is gone. Each row is deleted once the platform confirms its removal.
- `ad_audience_desired`: who qualifies, rebuilt at the start of every run (profile id, hashed identifier). Rows for a deleted or merged-away profile can be deleted; a run in progress also ignores them, because every batch rechecks the live profile.

## Setting it up on a server (operators)

Each platform needs an app you register with it. A platform whose settings are missing shows as "not available on this server". Each platform's redirect URL is on `PUBLIC_URL`:

| Platform | Redirect URL |
| --- | --- |
| Meta | `{PUBLIC_URL}/api/ad-audiences/oauth/meta/callback` |
| Google Ads | `{PUBLIC_URL}/api/ad-audiences/oauth/google/callback` |
| TikTok | `{PUBLIC_URL}/api/ad-audiences/oauth/tiktok/callback` |

DumbMail sends requests only to `graph.facebook.com`, `oauth2.googleapis.com`, `datamanager.googleapis.com` and `business-api.tiktok.com`, over HTTPS, without following redirects.

### Meta

1. At developers.facebook.com, create an app of type **Business** and add the **Marketing API** and **Facebook Login for Business** products.
2. Under Facebook Login settings, add the redirect URL above to **Valid OAuth Redirect URIs**. Keep **Use Strict Mode for redirect URIs** on.
3. Request the **`ads_management`** permission. To use it with ad accounts your business doesn't own, Meta's **App Review** must approve Advanced Access to `ads_management` (and the Marketing API's Standard Access tier for its rate limits). Until then only people with a role on the app can connect.
4. Turn on **Require App Secret** (DumbMail sends `appsecret_proof` on every call).
5. Set `META_APP_ID` and `META_APP_SECRET`.

Each ad account must accept Meta's Custom Audience terms once in Ads Manager.

### Google Ads

DumbMail uploads Customer Match lists through Google's **Data Manager API**.

1. In Google Cloud, create a project, enable the **Data Manager API**, and configure the OAuth consent screen with the scopes `https://www.googleapis.com/auth/datamanager`, `openid` and `email`. The Data Manager scope is sensitive, so Google must **verify the app** before people outside your organization can connect.
2. Create an **OAuth client ID** of type **Web application** and add the redirect URL above to its authorized redirect URIs.
3. Set `GOOGLE_ADS_CLIENT_ID` and `GOOGLE_ADS_CLIENT_SECRET`. No developer token is needed.

Google Ads accounts need a good policy history and enough spend to use Customer Match; Google decides this, not DumbMail. Lists are created with Google's longest membership (540 days), and DumbMail sends entries that still qualify again before they'd expire.

### TikTok

1. At business-api.tiktok.com, register as a developer and create an app. Ask for the **Audience Management** scope (and Ad Account Management, to list advertisers).
2. Set the app's **Advertiser redirect URL** to the redirect URL above. TikTok reviews new apps before they go live.
3. Set `TIKTOK_APP_ID` and `TIKTOK_APP_SECRET`.

TikTok's advertiser tokens don't expire until the advertiser revokes them.

## Not verified yet

Built against each platform's public documentation and tested against local stand-ins. Once real app credentials exist, check on a live account:

- **All three:** a real match, a real removal, and a disconnect's revoke; that error codes map to the right words.
- **Meta:** the `/users` answer (`num_received`, `num_invalid_entries` and what `invalid_entry_samples` contains); that one batch per upload session is accepted; DELETE with one identifier per row removes the person.
- **Google:** `requestStatus:retrieve` results for ingest and remove, including partial results and record indexes; the `login-account` header and `Destination.loginAccount` for manager accounts; that the one-item list call is a fair check of access.
- **TikTok:** the header line `email_SHA256` on an `EMAIL_SHA256` file; whether the 1,000 minimum counts uploaded or matched entries; the error code TikTok gives when a removal would go below it; the shape of the create, update and delete answers.
