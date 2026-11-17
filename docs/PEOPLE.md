# Profile tools and privacy

Consent history, the suppression list, merging duplicate profiles, bulk actions, privacy requests (export or delete one person) and the "do not sell or share" flag. Everything here works the same in the dashboard (Audience → Suppression list, Duplicates, Privacy requests, and the Profiles and profile pages), the REST API and the MCP tools.

Nothing here can make anyone easier to reach: there's no bulk subscribe, merges keep the stricter consent, only hand-added suppressions can be lifted, and a deleted person who had unsubscribed stays unsubscribed if they're imported again.

## Consent history

Every change to a person's email or SMS consent, their suppression, or their "do not sell or share" flag is recorded with what changed, when and how. Rows can't be edited or deleted; they go only when the profile is deleted. History starts on the day this feature was installed (`history_started_at`); earlier changes weren't recorded.

Each entry has `channel` (`email`, `sms`, `ads`), `field` (`consent`, `suppression`, `opt_out`), `old_value`, `new_value`, `address` (the email or phone number then), `occurred_at`, the IP address and browser where there was one (forms, unsubscribe links), and `method`, plus readable `summary` and `how` sentences:

| `method` | How |
| --- | --- |
| `form` | A sign-up form (which form, the visitor's IP and browser). |
| `import` | A CSV import (dashboard, API or MCP: see `via`). |
| `api` / `mcp` / `dashboard` | An API key, the MCP server, or a team member in the dashboard (`actor_email`). |
| `unsubscribe_link` / `preference_page` / `one_click` | The unsubscribe page, "Unsubscribe from all" on the preference page, or a mail app's one-click unsubscribe; each names the message. |
| `sms_keyword` | A STOP or START reply (the keyword). |
| `double_opt_in` | The person confirmed a double opt-in sign-up. |
| `hard_bounce` / `spam_complaint` | Amazon SES reported a hard bounce or a spam complaint for a message. |
| `integration` / `klaviyo_import` | Another feature's sync or import (named in `detail`). |
| `merge` / `bulk_action` / `suppression_list` / `privacy_request` | Changes made by the tools on this page. |
| `deleted_profile` | A new address matched the suppression record kept for a deleted person. |
| `gpc` | The visitor's browser sent Global Privacy Control. |

`GET /api/people/profiles/:id/consent?limit=&offset=` returns `{data, total, history_started_at}`. MCP: `get_consent_history` (by `profile_id`, `email` or `phone_number`), which also returns `reachability`.

### Can they be reached?

`GET /api/people/profiles/:id` returns `reachability`: for `email` and `sms`, `can_send` and every reason not (no address, unsubscribed, waiting to confirm, hard bounce, spam complaint, suppressed by hand, not subscribed to texts, account email paused), with when and how where known. It also returns the "do not sell or share" flag, the first 50 history entries and the profile's notes.

## Suppression list

Everyone who can't get marketing on a channel, and why: `unsubscribed`, `hard_bounce`, `complaint`, `manual` (suppressed by hand) or `pending` (email only: a double opt-in sign-up not yet confirmed).

- `GET /api/people/suppressions?q=&channel=&reason=&from=&to=&limit=&offset=`: rows of `{profile_id, channel, address, reason, reason_text, since, how, can_unsuppress, why_not}`, plus `counts` per channel and reason and `deleted_profiles_suppressed`. `from` and `to` are dates (YYYY-MM-DD) in the account's time zone. MCP: `list_suppressions`.
- `GET /api/people/suppressions/export` (same filters): a CSV of up to 100,000 rows.
- `POST /api/people/suppressions` `{addresses: [...], csv?: "...", confirm}`: suppress emails and phone numbers by hand (a CSV column named email or phone is read, or else the first column). Profiles are created when needed and are never subscribed. Without `confirm: true` it returns the count and the entries that aren't addresses. Up to 200 are done at once; more run as a bulk action (`bulk_action` in the response). At most 100,000 per request. MCP: `suppress_profiles`.
- `POST /api/people/suppressions/unsuppress` `{profile_id, channel, confirm}`: lift a suppression added by hand. Bounces, complaints, a person's own unsubscribe and unconfirmed sign-ups can't be lifted (409, with the reason). Consent stays as it is. MCP: `unsuppress_profile`.

A suppressed phone number gets no texts: campaigns and flows skip it, and segments' "can receive SMS" condition leaves it out.

## Duplicates and merging

`GET /api/people/duplicates` lists groups of profiles that look like one person: the same email (case, or a Gmail address with dots or a `+tag`), the same phone number written differently, the same external id in another case, and `linked_identifiers`: an upsert (a form, the API, an integration) gave an email that belongs to one profile and a phone number or external id that belongs to another. Upserts behave as before; the pair is only listed here. Finding duplicates never merges anything. MCP: `find_duplicate_profiles`.

`POST /api/people/merge` `{keep_id, merge_id, choices, confirm}` merges `merge_id` into `keep_id` in one transaction:

- Fields: the kept profile's value wins when both have one, unless `choices` says `{"<field>": "merge"}`. Fields are `email`, `phone_number`, `external_id`, `first_name`, `last_name` and `properties.<key>`. Values only the other profile has are copied.
- Consent on each channel becomes the stricter of the two (unsubscribed, then waiting to confirm, then never subscribed, then subscribed). Suppressions carry over (the stronger one wins), and so does "do not sell or share".
- Events, messages, list and segment memberships, flow runs, double opt-in confirmations and notes move to the kept profile; consent history is copied with `merged_from`. A campaign both profiles got keeps one link to the campaign (the other message stays in the history, unlinked). An active run of a flow the kept profile is already in is exited.
- An address that doesn't survive the merge keeps a suppression record if it was unsubscribed or suppressed.
- The change is recorded in the kept profile's consent history (`method: merge`).

Without `confirm: true` it returns the merged result (`result`, `fields` with conflicts, `rows_moving`) without changing anything. MCP: `merge_profiles`.

## Bulk actions

`POST /api/people/bulk` `{action, selection, list_id?, key?, value?, channels?, confirm, expected_count}`:

- `action`: `add_to_list` or `remove_from_list` (`list_id`), `set_property` (`key`, `value`) or `clear_property` (`key`), `suppress` (`channels`: `email` and/or `sms`, default email), `delete`, or `export` (a CSV download). There is no bulk subscribe.
- `selection`: `{profile_ids: [...]}` (up to 10,000), or everyone matching `{q, list_id, segment_id}` (the Profiles page filters). At most 250,000 profiles.
- Without `confirm` (or without `expected_count`) it returns `{dry_run, count}`. With `confirm: true` and `expected_count` set to that count, the matching profiles are saved and a background job works through them 500 at a time. If the count changed in between, nothing runs (409). In the dashboard, destructive actions (remove from list, clear property, suppress, delete) need the count typed to confirm.
- One bulk action per account at a time (409 otherwise).
- `GET /api/people/bulk/:id` returns `status` (`queued`, `running`, `done`, `failed`), `total`, `processed`, `succeeded`, `skipped`, `error` and, for exports, `file_url`. `GET /api/people/bulk` lists recent ones. MCP: `start_bulk_action`, `get_bulk_action`.

Suppressions record consent history (`method: bulk_action`). Deletes work like a privacy delete (below) without logging a privacy request.

## Privacy requests

- `POST /api/people/privacy/export` `{email | phone_number}`: everything held about the person (profile and properties, consent history, events, messages sent with subject and outcome, lists, segments, form sign-ups, double opt-in confirmations, internal notes) as a JSON file and a readable HTML page. Downloads (`GET /api/people/files/:id`) need sign-in or an API key and expire after 7 days. MCP: `export_person_data` (also returns the data).
- `POST /api/people/privacy/delete` `{email | phone_number, confirm}`: deletes every matching profile with every row tied to it, plus earlier exports and bulk exports that include them. If they had unsubscribed or were suppressed, a salted one-way code of the address and the reason is kept, so they stay suppressed if they're imported or added again (their own form sign-up can still subscribe them; bounces and complaints always stay). Without `confirm: true` it returns what would be deleted. MCP: `delete_person_data`.
- `GET /api/people/privacy/requests`: the log of requests (when, who, export or delete, what was done). The person is kept only as a one-way code. MCP: `list_privacy_requests`.

Deleting a profile from its page (`DELETE /api/profiles/:id`) removes the same rows and keeps the same suppression record.

## Do not sell or share

`profiles.ads_opt_out` is a per-person "do not sell or share my personal information" flag. Features that share data with ad platforms must leave flagged people out. Set it from the profile page, `PATCH /api/people/profiles/:id/privacy` `{ads_opt_out}`, or MCP `set_do_not_sell`. The website snippet turns it on (never off) when the visitor's browser sends Global Privacy Control (`navigator.globalPrivacyControl`).

## Notes

Internal notes on a profile, never shown to the person: `GET`/`POST /api/people/profiles/:id/notes` `{body}` (up to 5,000 characters), `DELETE /api/people/profiles/:id/notes/:note_id`. Each note keeps who wrote it and when. MCP: `list_profile_notes`, `add_profile_note`, `delete_profile_note` (`confirm: true`).

## For other features

- Record any change to `email_consent`, `sms_consent`, `email_suppressed_reason`, `sms_suppressed_reason` or `ads_opt_out` through `features::people::consent` (`set_consent`, `suppress`, `set_ads_opt_out`, or `record_consent_change`), with a `ConsentSource` saying how. Profile upserts record their own changes; wrap a request or job in `people::with_source(ConsentSource::new("integration", "system").with_detail("integration", json!("shopify")), ...)` so they're attributed. A test fails on raw SQL writes to these columns elsewhere.
- Add every new table column that holds a profile id to `features::people::merge::PROFILE_TABLES`, saying what a merge does with it. Merges and deletions use that list, and a test fails on any `profile_id` column it doesn't cover.
