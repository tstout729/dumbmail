# Unique coupon codes

A coupon is a pool of single-use discount codes. Each person gets their own code, so codes can't be shared on coupon sites and every redemption is tied to a person. Find coupons in the dashboard under **Content → Coupons**.

## Making a coupon

`POST /api/coupons` (MCP `create_coupon`):

```json
{"name": "WELCOME", "description": "10% off a first order", "expires_after_days": 30, "low_stock_threshold": 500}
```

- `name`: what templates use. Letters, digits, spaces, `.`, `_` and `-`, up to 64, starting with a letter or digit. Names are unique per account (ignoring letter case) and can't change later, because messages refer to them.
- `description`: up to 500 characters, for you.
- `source`: `uploaded` (default; codes made in your store) or `generated` (DumbMail makes them; needs `pattern`).
- `pattern`: `{"prefix": "WELCOME-", "length": 8, "alphabet": "letters_digits"}`. `prefix` is up to 20 letters, digits, `-` and `_`; `length` is 6 to 24 random characters, and prefix and random part together are at least 8; `alphabet` is `letters_digits` (default, without the look-alikes 0, O, 1, I and L), `letters` or `digits`.
- Expiry, either `expires_at` (a date and time: every code stops on that date) or `expires_after_days` (1–3650: each code expires that many days after a message carrying it is first sent, or a form shows it). Leave both out for codes that don't expire. After a fixed `expires_at` has passed, sends that use the coupon fail with a clear reason instead of handing out dead codes.
- `new_code_each_time`: `false` (default) gives a person the same code in every message, so a reminder or resend shows the code they already have. `true` gives a new code in every send. A retry of the same send (a campaign message, or one step of one flow run, tried again later) keeps its code.
- `low_stock_threshold`: when fewer codes than this are left, the account owner gets one email and the Coupons page shows a warning.
- `form_codes_per_hour`: the most codes sign-up forms may hand out per hour (1–10,000, default 200). See "Sign-up forms".

`PATCH /api/coupons/{id}` (MCP `update_coupon`) changes everything except the name. `GET /api/coupons` (MCP `list_coupons`) is paged (`limit` up to 100, `offset`) and shows each coupon's `stock` (`total`, `available`, `assigned`, `redeemed`, `redeemed_unverified`, `generating`), `alerts` (`out_of_codes`, `low_stock`, and `waiting`: the campaigns and flows held because it ran out) and its template tags.

## Adding codes

- **Codes made in your store:** `POST /api/coupons/{id}/codes` (MCP `add_coupon_codes`). Send the file itself (`Content-Type: text/csv` or `text/plain`, up to 16 MB), which is read and added a batch at a time as it arrives, or JSON (up to 2 MB) with `{"codes": ["SAVE-1", "SAVE-2"]}` or `{"text": "<pasted codes or a CSV>"}`. Text is read one code per line, from the first column; a header row such as `code` is skipped.
- A code is 4 to 64 ASCII letters, digits, `-` and `_`. Other lines are refused and the reply lists the first 20 by line number (`refused_lines`). Codes are compared ignoring letter case and kept as you uploaded them; codes the coupon already has are skipped. The reply says how many were `added`, `duplicates` and `refused`.
- Up to 200,000 codes per upload and 2,000,000 per coupon. An upload that reaches a limit stops there and says so (`stopped`); the codes before that point stay added. One upload or generation runs at a time per account; another one meanwhile gets a 409 and can try again when it finishes.
- **Generated codes:** `POST /api/coupons/{id}/generate` (MCP `generate_coupon_codes`) with `{"count": 5000}` (and optionally a `pattern`, which is saved). Up to 5,000 are made at once (`added`); more are made in the background in batches (`queued`, and `stock.generating` counts down). Your store must accept these codes before you send them: export them and import them into the store.
- **Export:** `GET /api/coupons/{id}/export?status=available|assigned|redeemed|all&format=codes|csv`. `format=codes` is one code per line, exactly as stored, so it uploads back unchanged (use it to import generated codes into your store). `format=csv` (the default) is `code,status,email,given_at,expires_at,redeemed_at,order_value,unverified_order_at`, safe to open in a spreadsheet: a value that starts with `=`, `+`, `-` or `@` gets a leading `'`, so a code like `-WELCOME` reads `'-WELCOME` there. MCP `export_coupon_codes` returns the same, up to 10,000 codes a call (`after` and `next_after` page through).
- **Search:** `GET /api/coupons/{id}/codes?q=SAVE&status=assigned&limit=50` lists codes, newest added first, with who has each. `q` is the start of a code, or an email address to find that person's codes. Pass `next_cursor` back as `cursor` for the next page; paging goes through every code. With a search, `total` stops counting at 10,000 (`total_capped`).

Codes are money: full code lists never go into logs, and a code given to a person is never put back into the pool for someone else.

## Giving codes out in messages

In the body of an email (a text or HTML block, a button or a link) and in SMS bodies:

```
Here's your code: {% coupon_code 'WELCOME' %}
It works until {% coupon_expiry 'WELCOME' %}.
```

Write the coupon's name in single quotes. A tag whose name isn't written out like that, the `{{ coupon_code(...) }}` form, and coupon codes in the **subject or preheader** are refused when you save the campaign, flow or template: the subject and preheader are kept with every message and in its events, so a code there would be stored outside the coupon. Transactional email doesn't support coupons at all (transactional templates and sends that use them are refused).

In the email builder, the **Coupon** block shows the code in a dashed box, with an optional "Use it by" line and button:

```json
{"type": "coupon", "coupon": "WELCOME", "title": "Your code", "show_expiry": true, "button_text": "Shop now", "href": "https://shop.example.com"}
```

Which coupons a message uses is read from exactly what it renders (the body DumbMail builds from its blocks, or its HTML), per A/B version. A message can use up to 5 coupons.

How codes are handed out:

- **One per person per coupon**, unless the coupon gives a new code each time. The first message that needs a code gives the person one; every later message (a reminder, a resend, another flow) shows the same code.
- **A code given to a person is theirs for good.** It's taken from the pool when the message is about to be built: after consent, unsubscribes, suppression, quiet hours and the sending pause have been checked, so a message that is only planned or waiting (send at local time, gradual sending, a delay) holds no code. If the message still doesn't go out (no credit, the daily sending limit, a last-moment opt-out, the provider is busy or fails), the person keeps the code and gets the same one next time. Codes never go back to the pool, so two people never share a code. An occasional code given to someone who never received it is the price of that.
- **All or nothing.** Every code a message needs is taken together; if any of its coupons has run out, it takes none and waits.
- **Exactly once.** The database refuses two codes for the same person (or the same send), so two sends at the same moment, or a job that runs twice, still give one code.
- **Previews and test sends** show `SAMPLE-CODE` (and a sample date) and never use a code. A real message never shows the sample: if its code can't be given, it waits or fails with a reason.
- **When a coupon runs out**, nothing goes out with a blank code. A campaign pauses: its status stays "sending", the campaign shows "Coupon WELCOME has no codes left" as the reason, and it tries again every 10 minutes. Flow messages wait and retry the same way. The account owner gets one email (from `SUPPORT_EMAIL`) for each time it runs out, and the Coupons page lists what's waiting. Adding codes clears the alert (the next time it runs out alerts again) and everything carries on within 10 minutes.
- **Before a campaign sends**, the launch check (and MCP `get_campaign`'s `problems`) refuses a coupon that doesn't exist or has expired, and content that misuses coupons. Whether the pool covers the audience is a warning, with the numbers (`GET /api/coupons/campaign-check/{campaign_id}`, MCP `get_campaign`'s `warnings`, and the dashboard's send dialog): the audience and the stock can still change, and running out only makes messages wait. Each A/B version is checked against its own share of the audience.
- **Clicks:** a link carrying the person's code works as written, but the copy of the URL kept with the click (and the click event) has the code replaced by `{coupon_code}`.

## Sign-up forms

Set a form's `config.coupon` to a coupon's name and each person who signs up gets their own code on the thank-you screen (the `discount_code` in the sign-up response) instead of the form's fixed `discount_code`.

- The code is only handed over after a real sign-up, and only to a person this sign-up created. Anyone can type someone else's email address into a form, so a person already in your audience never sees a code on the form (their code must not go to a stranger). Give them one in your welcome email with `{% coupon_code %}` instead.
- One code per person. Signing up again doesn't give another code or show the first one again.
- On a double opt-in list, the sign-up records the coupon the form had at that moment (if the sign-up created the person). Confirming by email gives that code once, on the confirmation page, which shows it for 24 hours after confirming. Older links, and links whose sign-up didn't earn a code (an existing person, or a form that had no coupon then), show the normal "You're subscribed" page.
- So a bot can't drain the pool with made-up addresses, forms hand out at most `form_codes_per_hour` codes per coupon per hour, and at most 3 per visitor (IP address) per hour across all accounts; sign-ups whose address can't be read share one such allowance. Past a limit, or when the coupon is out of codes, the sign-up still works but shows no code.

## Redemptions

Every minute or so DumbMail reads new `Placed Order` events (for accounts with coupons) and matches their discount codes: `discount_codes` (a list of codes or of objects with `code`, as the website script and Shopify send them) or a single `discount_code`. An order's code redeems exactly one code: the same person's, with the same code (ignoring letter case), given before the order was placed and not expired then; the most recently given if more than one matches. Its value counts as the coupon's revenue, once per order. A code that belongs to someone else, or to no one, is ignored, so a made-up order can't mark another person's code used. Each order is matched once, however often it's read.

Orders recorded through the website script (`/client/events`) could be made up by a visitor. They're kept as unverified evidence (`unverified_order_at`, counted in `redeemed_unverified`): they add no revenue, don't match the "redeemed" segment rule, and never stop an order from the API, MCP or a store integration redeeming the same code. Orders never change whether a code counts as given. Redemption numbers are only as reliable as where your orders come from.

`GET /api/coupons/{id}/report?days=30` (MCP `coupon_report` with `id`) gives codes `given` (that reached someone), `redeemed`, `redeemed_unverified`, `redemption_rate`, `revenue` and a day-by-day breakdown in the account's time zone. `GET /api/coupons/report` (MCP `coupon_report` without `id`) gives the totals for every coupon.

## Segments

```json
{"type": "coupon", "coupon_id": "...", "action": "given" | "redeemed", "op": "has" | "has_not", "within_days": 30}
```

`given`: a code from the coupon reached them (a message carrying it was sent, or a form showed it). `redeemed`: they used it in a verified order. `within_days` (1–3650) counts from when it was given or redeemed. A rule naming a coupon that isn't in the account (deleted, or a wrong id) matches nobody, with `has` or `has_not`.

## Deleting

`DELETE /api/coupons/{id}?confirm=true` (MCP `delete_coupon` with `confirm: true`; without it both only describe what would happen) removes the coupon, its codes, who got which code and its report. It's refused while anything uses the coupon, and the error says what: a segment or flow rule that names it, a campaign that isn't finished (draft, scheduled, sending or testing) or a flow whose content uses it, or a form set to hand out its codes.

When a person is deleted, their codes stay used (they never go back to the pool) but keep no link to them.

## Permissions

| What | Permission (roles) | Routes and MCP tools |
| --- | --- | --- |
| See coupons, stock, alerts and reports | `content.read` (every role) | `GET /api/coupons`, `GET /api/coupons/{id}`, `GET /api/coupons/report`, `GET /api/coupons/{id}/report`; `list_coupons`, `coupon_report` |
| A campaign's coupon check | `messaging.read` (every role) | `GET /api/coupons/campaign-check/{campaign_id}` |
| See raw codes and who holds them, and export | `catalog.write` and `profiles.read` (Managers, Admins, the Owner) | `GET /api/coupons/{id}/codes`, `GET /api/coupons/{id}/export`; `get_coupon`, `export_coupon_codes` |
| Create and edit coupons, add and generate codes | `catalog.write` (Managers, Admins, the Owner) | `POST /api/coupons`, `PATCH /api/coupons/{id}`, `POST /api/coupons/{id}/codes`, `POST /api/coupons/{id}/generate`; `create_coupon`, `update_coupon`, `add_coupon_codes`, `generate_coupon_codes` |
| Delete a coupon | `catalog.delete` (Admins, the Owner), with `confirm=true` | `DELETE /api/coupons/{id}?confirm=true`; `delete_coupon` with `confirm: true` |

API keys follow the same rules through their scopes (`content` for writes, `content:read` for reads, plus `audience:read` for raw codes). Creating, changing, adding or generating codes, exporting and deleting are recorded in the audit log with counts, never codes. Sign-up form codes are handed out by the public form routes.

## For store integrations

`features::coupons::give_code` is the one way to give someone a code outside the pool (sends use it too for a coupon linked to the Shopify store, see docs/SHOPIFY.md). It takes an optional `CreateInStore` callback, called (with no database lock held) when the pool is empty, so an integration can create a real discount code in the store for that person. The code it returns must follow the code rule above, and is saved as that person's code.
