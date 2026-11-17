# Profile tools and privacy

Consent history, the suppression list, merging duplicate profiles, bulk actions, privacy requests (export or delete one person) and the "do not sell or share" flag. Everything here works the same in the dashboard (Audience → Suppression list, Duplicates, Privacy requests, and the Profiles and profile pages), the REST API and the MCP tools.

Nothing here can make anyone easier to reach: there's no bulk subscribe, merges keep the stricter consent, only hand-added suppressions can be lifted, and a deleted person who had unsubscribed stays unsubscribed if they're imported again, until they confirm a sign-up from their own inbox.

## Consent history

Every change to a person's email or SMS consent, their suppression, or their "do not sell or share" flag is recorded with what changed, when and how, in the same transaction as the change, with the profile locked so the before and after values are exact. History starts on the day this feature was installed (`history_started_at`); earlier changes weren't recorded.

Rows are never edited: the database refuses an update. Rows are deleted only with their profile: when someone is erased, and when a profile is merged away (its rows are first copied onto the kept profile with new ids and `detail.merged_from`, then the originals go with the merged profile).

If a history row can't be written (the database refuses it), a change that makes the person less reachable (an unsubscribe, a STOP, a bounce or complaint, a suppression) is still saved and acknowledged: the error is logged and a background job writes the row later, with `detail.recorded_late: true` and a `recorded_at` later than `occurred_at`. A change that makes someone more reachable (a subscribe) fails as a whole instead, so it can be retried; nothing is saved without its history.

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
| `deleted_profile` | The profile's address matched the suppression record kept for a deleted person. |
| `gpc` | The visitor's browser sent Global Privacy Control. |

`GET /api/people/profiles/:id/consent?limit=&offset=` returns `{data, total, history_started_at}`. MCP: `get_consent_history` (by `profile_id`, `email` or `phone_number`), which also returns `reachability`.

### Can they be reached?

`GET /api/people/profiles/:id` returns `reachability`: for `email` and `sms`, `can_send` and every reason not (no address, unsubscribed, waiting to confirm, hard bounce, spam complaint, suppressed by hand, not subscribed to texts, account email paused), with when and how where known. It also returns the "do not sell or share" flag, the first 50 history entries and the profile's notes.

## Suppression list

Everyone who can't get marketing on a channel, and why: `unsubscribed`, `hard_bounce`, `complaint`, `manual` (suppressed by hand), `pending` (email only: a double opt-in sign-up not yet confirmed) or `not_subscribed` (texts only: a phone number that never agreed to texts).

- `GET /api/people/suppressions?q=&channel=&reason=&from=&to=&limit=&offset=`: rows of `{profile_id, channel, address, reason, reason_text, since, how, can_unsuppress, why_not}`, plus `counts` per channel and reason and `deleted_profiles_suppressed`. `since` is when the recorded change happened; it's null for a block from before consent history started (the date filters then use the profile's last change). `from` and `to` are dates (YYYY-MM-DD) in the account's time zone. MCP: `list_suppressions`.
- `GET /api/people/suppressions/export` (same filters): a CSV of up to 100,000 rows.
- `POST /api/people/suppressions` `{addresses: [...], csv?: "...", confirm}`: suppress emails and phone numbers by hand (a CSV column named email or phone is read, or else the first column). Profiles are created when needed and are never subscribed. Without `confirm: true` it returns the count and the entries that aren't addresses. Up to 200 are done at once; more run as a bulk action (`bulk_action` in the response). At most 100,000 per request. MCP: `suppress_profiles`.
- `POST /api/people/suppressions/unsuppress` `{profile_id, channel, confirm}`: lift a suppression added by hand. Bounces, complaints, a person's own unsubscribe and unconfirmed sign-ups can't be lifted (409, with the reason). Consent stays as it is. A manual suppression kept for a deleted person with that address is let go too, so it doesn't come back with the next import. MCP: `unsuppress_profile`.

A suppressed phone number gets no texts at all, test sends included: every text passes the check in `sending::send`. Segments' "can receive SMS" condition leaves it out, and the "suppressed" condition matches a suppression on either channel.

## Duplicates and merging

`GET /api/people/duplicates` lists groups of profiles that look like one person: the same email (case, or a Gmail address with dots or a `+tag`), the same phone number written differently, the same external id in another case, and `linked_identifiers`: an upsert (a form, the API, an integration) gave an email that belongs to one profile and a phone number or external id that belongs to another. Upserts behave as before; the pair is only listed here. Finding duplicates never merges anything. MCP: `find_duplicate_profiles`.

`POST /api/people/merge` `{keep_id, merge_id, choices, confirm}` merges `merge_id` into `keep_id` in one transaction:

- Fields: the kept profile's value wins when both have one, unless `choices` says `{"<field>": "merge"}`. Fields are `email`, `phone_number`, `external_id`, `first_name`, `last_name` and `properties.<key>`. Values only the other profile has are copied.
- Consent on each channel becomes the stricter of the two (unsubscribed, then waiting to confirm, then never subscribed, then subscribed). Suppressions carry over (the stronger one wins), and so does "do not sell or share".
- Events, messages, list and segment memberships, flow runs, double opt-in confirmations, notes, product activity and alerts move to the kept profile; consent history is copied with `merged_from`. A campaign both profiles got keeps one link to the campaign (the other message stays in the history, unlinked). An active run of a flow the kept profile is already in is exited. A price-drop cooldown keeps the later notice, so it never gets shorter.
- Messages waiting to be sent: when both profiles have one for the same campaign or flow step, one is cancelled (`skipped`, "merged into another profile"); the one kept is the one addressed to the merged profile's address. Every queued message keeps the address it was meant for, and the sender skips it ("address changed") if the profile no longer has that address. If a message to either profile is being sent at that moment, the merge is refused (409): try again in a minute.
- Double opt-in confirmations waiting for an address the merged profile won't have are voided: clicking them says the link expired, so a confirmation sent to one address can't subscribe another.
- Old links and provider callbacks act on the kept profile: messages move to it, so an unsubscribe link in an email sent to the merged-away address unsubscribes the kept profile (the safe direction), and a later bounce or complaint for such a message suppresses the kept profile's email.
- An address that doesn't survive the merge keeps a suppression record if it was unsubscribed or suppressed.
- The change is recorded in the kept profile's consent history (`method: merge`). Bulk actions that had the merged-away profile in their saved set skip it.

Without `confirm: true` it returns the merged result (`result`, `fields` with conflicts, `rows_moving`) without changing anything. MCP: `merge_profiles`.

## Bulk actions

`POST /api/people/bulk` `{action, selection, list_id?, key?, value?, channels?, confirm, set_id}` works in two steps, so it acts on exactly the people the user saw counted:

- `action`: `add_to_list` or `remove_from_list` (`list_id`), `set_property` (`key`, `value`) or `clear_property` (`key`), `suppress` (`channels`: `email` and/or `sms`, default email), `delete`, or `export` (a CSV download). There is no bulk subscribe. Property keys can't be reserved fields (see below).
- `selection`: `{profile_ids: [...]}` (up to 10,000), or everyone matching `{q, list_id, segment_id}` (the Profiles page filters). At most 250,000 profiles.
- Preview: without `confirm`, the matching profiles are counted and saved as a set, and it returns `{dry_run, count, set_id, expires_at}`. Nothing changes. The set is kept 15 minutes; a new preview replaces the caller's earlier unconfirmed one.
- Confirm: `confirm: true` with that `set_id` and the same `action` and settings. It runs on exactly the saved set, whoever matches the selection by then. Another action or other settings than the preview's, an expired set or one already started is refused (409). A background job works through the set 500 at a time; each batch's changes and progress are saved together, so a retried batch never repeats. Profiles in the set that were deleted or merged away since are counted as `skipped`. In the dashboard, destructive actions (remove from list, clear property, suppress, delete) need the count typed to confirm.
- One bulk action per account at a time (409 otherwise).
- `GET /api/people/bulk/:id` returns `status` (`queued`, `running`, `done`, `failed`), `total`, `processed`, `succeeded`, `skipped`, `error` and, for a finished export, `file_url`. `GET /api/people/bulk` lists recent ones. MCP: `start_bulk_action` (the dry run returns `set_id`), `get_bulk_action`.

Suppressions record consent history (`method: bulk_action`). Deletes are erasures (below) without a privacy request in the log. An export is written in chunks and can be downloaded only once it's done; if someone in the account is erased while it runs, it stops and its file is thrown away (start it again).

## Privacy requests

- `POST /api/people/privacy/export` `{email | phone_number}`: starts building, in the background, everything held about the person: profile and properties, earlier addresses (from their consent history) and profiles merged into theirs, consent history, events, messages sent with subject and outcome, lists, segments, form sign-ups (including the phone step), double opt-in confirmations and internal notes, as a JSON file and a readable HTML page. It returns `{request_id, status: "building"}`; nothing personal is returned inline. `GET /api/people/privacy/requests/:id` (MCP: `list_privacy_requests` with `id`) gives `status` (`building`, `ready`, `failed`) and, once ready, the download links. Downloads (`GET /api/people/files/:id`) need sign-in or an API key and expire after 7 days. Every row is included up to 250,000 of each kind per profile; past that the file says at the top that it isn't complete (`complete: false`, and which kinds). If the person is erased or merged while the export is built, it's thrown away and marked failed. MCP: `export_person_data`.
- `POST /api/people/privacy/delete` `{email | phone_number, confirm}`: erases every matching profile (see Erasure). Without `confirm: true` it returns what would be deleted. MCP: `delete_person_data`.
- `GET /api/people/privacy/requests`: the log of requests (when, who, export or delete, what was done, export status and links). The person is kept only as a one-way code (below). MCP: `list_privacy_requests`.

### Erasure

Deleting a profile from its page (`DELETE /api/profiles/:id`), a bulk delete and a privacy delete all erase the same way, in one transaction:

- The profile and every row tied to it (`features::people::merge::PROFILE_TABLES`), including its consent history and notes.
- Their personal exports, whichever profile they were made from (exports follow merges).
- **Every stored export file of the account that holds more than one person**: bulk exports here and the CSV exports on the Reports page. They can't be searched cheaply for one person, they expire after 7 days anyway, and they can be made again. Exports still being written stop and throw their file away.
- Saved bulk-action searches that name them, background history writes that carry their details, and their place in pending bulk actions.
- What's kept: if they had unsubscribed or were suppressed, a salted one-way code of each address with the reason. When the address comes back (an import, an API call, a form, an address edit), the block is applied again in the same transaction, on every create and every update, and the stronger block keeps its reason (a hard bounce is never relabelled as a manual suppression). A retained unsubscribe is lifted only by the person's own verified opt-in: confirming a double opt-in sign-up from that inbox, or texting START from that number. An import, an API call or a form field that says "subscribed" isn't enough, since anyone can type an address. Bounces and complaints stay.

The one-way codes (here and in the request log) are SHA-256 of the account's random salt, the channel and the address (emails reduced to the inbox they reach). The salt is stored in the database, so someone holding a copy of the database could test guesses of an address against them. They can't be reversed without guessing.

## Do not sell or share

`profiles.ads_opt_out` is a per-person "do not sell or share my personal information" flag. Features that share data with ad platforms must read this column and leave flagged people out. Set it from the profile page, `PATCH /api/people/profiles/:id/privacy` `{ads_opt_out}`, or MCP `set_do_not_sell`.

Global Privacy Control turns it on (never off): the website snippet sends `navigator.globalPrivacyControl` with events and sign-ups, and a browser's `Sec-GPC: 1` header counts too (so plain hosted form pages are covered). It's taken for the profile the request names, from any visitor, because it only ever restricts: someone who knows a person's email could opt them out of ad audiences. The dashboard (or API) can turn it off again.

## Notes

Internal notes on a profile, never shown to the person: `GET`/`POST /api/people/profiles/:id/notes` `{body}` (up to 5,000 characters), `DELETE /api/people/profiles/:id/notes/:note_id`. Each note keeps who wrote it and when. MCP: `list_profile_notes`, `add_profile_note`, `delete_profile_note` (`confirm: true`).

## Reserved property keys

Custom properties can't use the names of consent, suppression, privacy or identity fields: `email_consent`, `sms_consent`, `email_suppressed_reason`, `sms_suppressed_reason`, `marketing_hold`, `ads_opt_out` (and its `_at`/`_source`), `id`, `email`, `phone_number`, `external_id`, or any key starting with `$`. Profile upserts and bulk property actions refuse them (400); a CSV import leaves such a column out and says so in `errors`.

## How to change someone's consent

For other features (integrations, imports, texting). All of these are in `features::people::consent` and work inside your own transaction:

1. `set_consent_in(&mut tx, account_id, profile_id, Channel::Email | Channel::Sms, "unsubscribed" | "subscribed" | ..., None, &src)` sets one channel's consent. `set_consent(&db, ...)` does the same in a transaction of its own.
2. `suppress_in(&mut tx, account_id, profile_id, channel, "hard_bounce" | "complaint" | "manual", overwrite, unsubscribe, &src)` suppresses a channel.
3. `opt_in_verified_in(&mut tx, account_id, profile_id, channel, &src)` for the person's own verified opt-in (a double opt-in click, a START or keyword opt-in from that number). It's the only call that lifts a deleted person's retained unsubscribe.

Each locks the profile row, writes the change and its history row, and follows the rule above for failed history writes. For anything else, `lock` the row, change a copy of the `ConsentState` and `apply` it. `src` is a `ConsentSource` saying how (`ConsentSource::new("integration", "system").with_detail("integration", json!("shopify"))`); the shared profile upsert reads it from `people::with_source(src, ...)`. Don't write the consent, suppression or opt-out columns with raw SQL: a lint test looks for such writes, but it can't see SQL built at runtime, so it isn't proof.

## Permissions (for the team-and-roles layer)

Until that layer lands, every route and tool here is open to anyone signed in to the account or holding an API key. The intended mapping (REST paths relative to `/api/people`):

- **Audience read**: `GET /profiles/{id}`, `/profiles/{id}/consent`, `/profiles/{id}/notes`, `/suppressions`, `/duplicates`, `/bulk`, `/bulk/{id}`; tools `get_consent_history`, `list_profile_notes`, `list_suppressions`, `find_duplicate_profiles`, `get_bulk_action`. Bulk details are shown only for actions the caller may run.
- **Audience write**: `POST /profiles/{id}/notes`, `DELETE /profiles/{id}/notes/{note_id}`, `POST /suppressions`, `POST /merge`, the permitted actions of `POST /bulk`; tools `add_profile_note`, `delete_profile_note`, `suppress_profiles`, `merge_profiles`, `start_bulk_action`. A bulk add-to-list can start list flows, so it also needs send authority.
- **Send / admin**: `POST /suppressions/unsuppress`; tool `unsuppress_profile`.
- **Export**: `GET /suppressions/export`, bulk exports and their downloads. `GET /files/{id}` must check the permission for the file's kind (`privacy_export` or `bulk_export`).
- **Privacy (admin or owner, or an explicit grant)**: `PATCH /profiles/{id}/privacy`, `POST /privacy/export`, `POST /privacy/delete`, `GET /privacy/requests`, `GET /privacy/requests/{id}`, personal-file downloads and bulk delete; tools `set_do_not_sell`, `export_person_data`, `delete_person_data`, `list_privacy_requests`.
- Content-only roles get none of these.

## For other features

- Change consent only as described in "How to change someone's consent". Profile upserts record their own changes; wrap a request or job in `people::with_source(...)` so they're attributed.
- Add every new table column that holds a profile id to `features::people::merge::PROFILE_TABLES`, saying what a merge and an erasure do with it. Merges and erasures use that list, and a test fails on any `profile_id` column it doesn't cover.
- Store a file that holds more than one person's data only in `people_files` (kind other than `privacy_export`) or the reports `exports` table, so an erasure clears it.
