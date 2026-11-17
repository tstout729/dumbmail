# Ad audiences

Sync a list or segment to Meta (Facebook and Instagram), Google Ads or TikTok as a customer-list audience. DumbMail creates the audience on the platform and keeps it current: people who join the list or segment are added, and people who leave are removed. Use it to advertise to your best customers, to leave recent buyers out of a campaign, or to build lookalike audiences.

## How it works

1. **Connect a platform** in Settings → Ad audiences. You sign in on the platform's own page and allow DumbMail to manage audiences. DumbMail stores the sign-in encrypted and never shows it again (only its last four characters).
2. **Create a sync**: pick the platform, the ad account, and a list or segment. DumbMail creates an audience named after it ("VIPs (DumbMail)" unless you name it) and uploads everyone who qualifies.
3. **It stays in step**: every 6 hours by default (you can pick every hour up to once a week), plus **Sync now**. Each run compares who is in the list or segment now with what DumbMail last sent, then removes people who left and adds people who joined. Nothing is sent twice, and removals use exactly the values that were sent.

A run happens in the background in small batches (10,000 people per request on Meta and Google, files of up to 50,000 on TikTok). If a platform asks DumbMail to slow down or has an outage, the run waits and tries again (1 minute, then 2, 4 and so on, up to an hour between tries).

## What is sent

Only identifiers the platform can match on, normalized the way each platform documents and **SHA-256 hashed on DumbMail's server before they're sent**. Raw email addresses and phone numbers never leave DumbMail and are never logged.

| Platform | Contact | Contact and location (optional) |
| --- | --- | --- |
| Meta | email, phone | adds first and last name, city, state, ZIP, country |
| Google Ads | email, phone | adds name, country and postal code as an address |
| TikTok | email | (TikTok's customer files take one kind of identifier; DumbMail uses email) |

Location fields come from the profile's `city`, `region` (or `state`), `zip` (or `postal_code`) and `country` properties. Google's country and postal code are sent as plain text, as Google requires; everything else is hashed.

## Who is never sent

These rules are checked on every run, for every platform:

- **Unsubscribed or suppressed email.** People who unsubscribed from email, or whose email bounced or complained, are left out. A sync for an **exclusion audience** (one you use to stop showing ads to people) can turn on "Include people who unsubscribed", because leaving them out of your ads is the point. Even then, the next rule still applies.
- **Opted out of ads.** A profile with `ads_opt_out`, `do_not_sell` or `gpc` set to true (a "do not sell or share my information" request or a Global Privacy Control signal), or `ads_consent` set to false, is removed from every ad audience on the next sync.
- **Deleted profiles** are removed from every synced audience on the next sync.
- **Google, Europe:** for people in the European Economic Area, the UK or Switzerland, Google needs proof they agreed to their data being used for ads. DumbMail sends `adUserData` and `adPersonalization` as granted only for profiles whose `ads_consent` property is true, and leaves everyone else in those countries out. A `country` DumbMail can't read is treated the same way.

The run history shows how many people were left out because they had nothing the platform could match on or no consent.

## Status

Each sync shows: when it last ran, how many people DumbMail has sent (and are in the audience now), how many were added and removed last run, the size the platform reports (Meta gives a range; Google gives its Search and Display sizes; TikTok gives its matched count), Google's match rate, and any problem in plain words. Common ones:

- **Sign-in expired or was revoked.** Reconnect the platform; syncs carry on from where they were.
- **The ad account doesn't let you manage audiences.** The person who connected needs permission to manage audiences on that ad account. On Meta, also accept the Custom Audience terms for the ad account in Ads Manager.
- **Too small to use yet.** Platforms only run ads to audiences above a minimum (Google: 100 matched people for Display and YouTube, 1,000 for Search; TikTok: 1,000). The sync keeps going; the audience becomes usable once enough people match.
- **The list or segment was deleted.** Delete the sync, and remove the audience on the platform if you don't need it.
- **The audience was deleted on the platform.** DumbMail creates a new one on the next run.

Pause stops a sync (a run in progress stops at its next step). Delete stops it for good. Deleting can also delete the audience on the platform; ads that use it stop, so it's off unless you choose it.

## API

All under `/api/ad-audiences`, with a session or `Authorization: Bearer <API key>`.

| Method and path | What it does |
| --- | --- |
| `GET /connections` | Each platform: `available` on this server, `status` (`not_connected`, `connected`, `needs_reconnect`), who connected, the token hint. |
| `POST /connections/{platform}/start` | Returns `{url}` to open in the browser to connect. The person must be signed in to DumbMail in that browser. |
| `DELETE /connections/{platform}` | Revokes DumbMail's access on the platform and deletes the tokens. |
| `GET /connections/{platform}/accounts` | Ad accounts the sign-in can reach. Google without a developer token returns `manual_entry: true`: enter the customer ID. |
| `GET /syncs` | All syncs (`?source_type=list&source_id=…` to filter). |
| `POST /syncs` | Create: `platform`, `ad_account_id`, `source_type` (`list` or `segment`), `source_id`, `accept_terms: true`, optional `name`, `include_unsubscribed`, `match_fields` (`contact` or `contact_and_location`), `interval_hours` (1–168, default 6), and for Google `login_account_id` (the manager account ID, when the ad account is reached through one). |
| `GET /syncs/{id}` | Status and the last 30 runs. |
| `PATCH /syncs/{id}` | `status` (`active` or `paused`), `interval_hours`, `include_unsubscribed`, `match_fields`. |
| `POST /syncs/{id}/run` | Sync now. |
| `DELETE /syncs/{id}?delete_on_platform=true` | Delete the sync, and optionally the audience on the platform. |

`accept_terms: true` records that the people in the list gave you their contact details directly and that you accept the platform's customer list terms (Meta's Custom Audience terms, Google's Customer Match policy, TikTok's Custom Audience terms).

## MCP tools

`list_ad_connections`, `list_ad_accounts`, `list_audience_syncs`, `get_audience_sync`, `create_audience_sync`, `update_audience_sync`, `run_audience_sync`, `delete_audience_sync`, `disconnect_ad_platform`.

- Connecting needs a browser: `list_ad_connections` returns `connect_url` (Settings → Ad audiences) to send the user to.
- `create_audience_sync` sends customer data to an ad platform. Without `confirm: true` it returns a dry run with the number of people; pass `confirm: true` and `accept_terms: true` only after the user agrees.
- `update_audience_sync` needs `confirm: true` to turn on `include_unsubscribed` or `contact_and_location`, since both send more.
- `delete_audience_sync` and `disconnect_ad_platform` describe what would happen unless `confirm: true`.

## Setting it up on a server (operators)

Each platform needs an app you register with it. A platform whose settings are missing shows as "not available on this server". Each platform's redirect URL is on `PUBLIC_URL`:

| Platform | Redirect URL |
| --- | --- |
| Meta | `{PUBLIC_URL}/api/ad-audiences/oauth/meta/callback` |
| Google Ads | `{PUBLIC_URL}/api/ad-audiences/oauth/google/callback` |
| TikTok | `{PUBLIC_URL}/api/ad-audiences/oauth/tiktok/callback` |

### Meta

1. At developers.facebook.com, create an app of type **Business** and add the **Marketing API** and **Facebook Login for Business** products.
2. Under Facebook Login settings, add the redirect URL above to **Valid OAuth Redirect URIs**. Keep **Use Strict Mode for redirect URIs** on.
3. Request the **`ads_management`** permission. To use it with ad accounts your business doesn't own, Meta's **App Review** must approve Advanced Access to `ads_management` (and the Marketing API's Standard Access tier for its rate limits). Until then only people with a role on the app can connect.
4. Turn on **Require App Secret** (DumbMail sends `appsecret_proof` on every call).
5. Set `META_APP_ID` and `META_APP_SECRET`.

Sign-ins last about 60 days. DumbMail trades them for new ones during their last week; if that fails, the connection asks to be reconnected. Each ad account must accept Meta's Custom Audience terms once in Ads Manager.

### Google Ads

DumbMail uploads Customer Match lists through Google's **Data Manager API**, which Google requires for Customer Match uploads since April 2026.

1. In Google Cloud, create a project, enable the **Data Manager API** (and the **Google Ads API** to list ad accounts), and configure the OAuth consent screen. The scopes are `https://www.googleapis.com/auth/datamanager`, `https://www.googleapis.com/auth/adwords`, `openid` and `email`. Both Google scopes are sensitive, so Google must **verify the app** before people outside your organization can connect.
2. Create an **OAuth client ID** of type **Web application** and add the redirect URL above to its authorized redirect URIs.
3. Set `GOOGLE_ADS_CLIENT_ID` and `GOOGLE_ADS_CLIENT_SECRET`.
4. Optional: set `GOOGLE_ADS_DEVELOPER_TOKEN` (from a Google Ads manager account's API Center, with at least Basic access) so people can pick their ad account from a list. Without it they type their 10-digit customer ID. Uploads don't need it.

Google Ads accounts need a good policy history and enough spend to use Customer Match; Google decides this, not DumbMail. A person who reaches the ad account through a manager account enters the manager account's ID too.

### TikTok

1. At business-api.tiktok.com, register as a developer and create an app. Ask for the **Audience Management** scope (and Ad Account Management, to list advertisers).
2. Set the app's **Advertiser redirect URL** to the redirect URL above. TikTok reviews new apps before they go live.
3. Set `TIKTOK_APP_ID` and `TIKTOK_APP_SECRET`.

TikTok's advertiser tokens don't expire until the advertiser revokes them. TikTok creates the audience with its first upload, so a sync whose list is empty waits until someone joins. TikTok only runs ads to customer-file audiences of at least 1,000 people.
