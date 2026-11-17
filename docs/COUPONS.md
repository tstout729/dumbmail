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
- `pattern`: `{"prefix": "WELCOME-", "length": 8, "alphabet": "letters_digits"}`. `prefix` is up to 20 letters, digits, `-` and `_`; `length` is 4 to 24 random characters; `alphabet` is `letters_digits` (default, without the look-alikes 0, O, 1, I and L), `letters` or `digits`.
- Expiry, either `expires_at` (a date and time: every code stops on that date) or `expires_after_days` (1–3650: each code expires that many days after it's given out). Leave both out for codes that don't expire. After a fixed `expires_at` has passed, sends that use the coupon fail with a clear reason instead of handing out dead codes.
- `new_code_each_time`: `false` (default) gives a person the same code in every message, so a reminder or resend shows the code they already have. `true` gives a new code in every message.
- `low_stock_threshold`: when fewer codes than this are left, the account owner gets one email and the Coupons page shows a warning.
- `form_codes_per_hour`: the most codes sign-up forms may hand out per hour (1–10,000, default 200). See "Sign-up forms".

`PATCH /api/coupons/{id}` (MCP `update_coupon`) changes everything except the name. `GET /api/coupons` (MCP `list_coupons`) is paged (`limit` up to 100, `offset`) and shows each coupon's `stock` (`total`, `available`, `assigned`, `redeemed`, `redeemed_unverified`), `alerts` (`out_of_codes`, `low_stock`, and `waiting`: the campaigns and flows held because it ran out) and its template tag.

## Adding codes

- **Codes made in your store:** `POST /api/coupons/{id}/codes` (MCP `add_coupon_codes`) with `{"codes": ["SAVE-1", "SAVE-2"]}` or `{"text": "<pasted codes or a CSV>"}`. Text is read one code per line, from the first column; a header row such as `code` is skipped. A code is 1–100 characters with no spaces. Codes the coupon already has, in any letter case, are skipped; codes are compared ignoring letter case. The reply says how many were `added`, `duplicates` and `refused`.
- **Generated codes:** `POST /api/coupons/{id}/generate` (MCP `generate_coupon_codes`) with `{"count": 5000}` (and optionally a `pattern`, which is saved). Your store must accept these codes before you send them: export them and import them into the store.
- Up to 200,000 codes per upload or generate, and 2,000,000 per coupon.
- **Export:** `GET /api/coupons/{id}/export?status=available|assigned|redeemed|all` downloads a CSV (`code,status,email,given_at,expires_at,redeemed_at,order_value,order_verified`). Spreadsheet formulas in a code are neutralized with a leading `'`.
- **Search:** `GET /api/coupons/{id}/codes?q=SAVE&status=assigned&limit=50&offset=0` lists codes, newest given first, with who has each. `q` is the start of a code, or an email address to find that person's code. Pages go up to 200 rows and offset 10,000; `total` stops counting at 10,000 (`total_capped`).

Codes are money: full code lists never go into logs, and expired or unredeemed codes are never put back into the pool for someone else.

## Giving codes out in messages

In any email subject, text, button or HTML, and in SMS bodies:

```
Here's your code: {% coupon_code 'WELCOME' %}
It works until {{ coupon_expiry('WELCOME') }}.
```

`{{ coupon_code('WELCOME') }}` works the same as the tag. In the email builder, the **Coupon** block shows the code in a dashed box, with an optional "Use it by" line and button:

```json
{"type": "coupon", "coupon": "WELCOME", "title": "Your code", "show_expiry": true, "button_text": "Shop now", "href": "https://shop.example.com"}
```

How codes are handed out:

- **One per person per coupon.** The first message that needs a code gives the person one; every later message (a reminder, a resend, another flow) shows the same code, unless the coupon gives a new code each time. A message can use up to 5 coupons.
- **Exactly once.** The code is taken from the pool in the database before the message is built, and the database refuses two codes for the same person, so two sends at the same moment, or a job that runs twice, still give one code. Two people never share a code.
- **Only for messages that go out.** Consent, unsubscribes, suppression and the account's sending pause are checked before a code is touched. If a message still doesn't go out after that (no credit left, the daily sending limit, the provider is busy, the person opted out at the last moment), its code is held for that person for 15 minutes and then put back in the pool, unless the message was sent. A message the email or SMS provider might have taken (a failed or interrupted send) keeps its code; the person gets the same code next time.
- **Previews and test sends** show `SAMPLE-CODE` (and a sample date) and never use a code.
- **When a coupon runs out**, nothing goes out with a blank code. A campaign pauses: its status stays "sending", the campaign shows "Coupon WELCOME has no codes left" as the reason it's paused, and it tries again every 10 minutes. Flow messages wait and retry the same way. The account owner gets one email (from `SUPPORT_EMAIL`), and the Coupons page lists what's waiting. Adding codes clears the alert and everything carries on within 10 minutes.
- **Before a campaign sends**, its check (and MCP `get_campaign`'s `problems`) names any coupon that doesn't exist, has expired, or has fewer codes available than the people in the audience who don't have one yet. Sending is refused until that's fixed.

## Sign-up forms

Set a form's `config.coupon` to a coupon's name and each person who signs up gets their own code on the thank-you screen (the `discount_code` in the sign-up response) instead of the form's fixed `discount_code`.

- The code is only handed over after a real sign-up, and only to a person this sign-up created. Anyone can type someone else's email address into a form, so a person already in your audience never sees a code on the form (their code must not go to a stranger). Give them one in your welcome email with `{% coupon_code %}` instead.
- One code per person. Signing up again doesn't give another code or show the first one again.
- On a double opt-in list, the code is shown on the confirmation page after the person confirms by email, not on the form.
- So a bot can't drain the pool with made-up addresses, forms hand out at most `form_codes_per_hour` codes per coupon per hour, and at most 3 per visitor (IP address) per hour across all accounts. Past a limit, or when the coupon is out of codes, the sign-up still works but shows no code.

## Redemptions

When a `Placed Order` event carries a discount code (`discount_codes`, a list of codes or of objects with `code`, as the website script and Shopify send them, or a single `discount_code`), and that code was given to the same person, the code is marked redeemed and linked to the order (its value counts as the coupon's revenue). A code that belongs to someone else, or to no one, is ignored, so a made-up order can't mark another person's code used.

Orders recorded through the website script (`/client/events`) could be made up by a visitor. They mark a code **redeemed (unverified)**: it shows as redeemed in the code list, but it's counted separately (`redeemed_unverified`), adds no revenue and doesn't match the "redeemed" segment rule. An order with the same code recorded by the API, MCP or a store integration verifies it. Redemption numbers are only as reliable as where your orders come from.

`GET /api/coupons/{id}/report?days=30` (MCP `coupon_report` with `id`) gives codes `given` (that reached someone), `redeemed`, `redeemed_unverified`, `redemption_rate`, `revenue` and a day-by-day breakdown in the account's time zone. `GET /api/coupons/report` (MCP `coupon_report` without `id`) gives the totals for every coupon.

## Segments

```json
{"type": "coupon", "coupon_id": "...", "action": "given" | "redeemed", "op": "has" | "has_not", "within_days": 30}
```

`given`: a code from the coupon reached them (a message carrying it was sent, or a form showed it). `redeemed`: they used it in a verified order. `within_days` (1–3650) counts from when it was given or redeemed.

## Deleting

`DELETE /api/coupons/{id}` (MCP `delete_coupon`, which needs `confirm: true`) removes the coupon, its codes, who got which code and its report. It's refused while a scheduled or sending campaign, a live or paused flow, or a live form uses the coupon.

## For store integrations

`features::coupons::give_code` is the one way to give someone a code. It takes an optional `CreateInStore` callback, called (with no database lock held) when the pool is empty, so an integration can create a real discount code in the store for that person. The code it returns is saved as that person's code.
