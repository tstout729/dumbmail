# Sign-up form reference

A form has a `kind` (`popup`, `flyout`, `embed` or `page`), an optional `list_id` that sign-ups join, a `status` (`draft` or `live`) and a `config` object. Everything in `config` is optional; anything left out uses the default shown here. The dashboard editor, the API (`/api/forms`) and the MCP tools (`create_form`, `update_form`) all use this same config.

**Updating.** `update_form` / `PATCH /api/forms/:id` merges `config` one level deep: each top-level key you send replaces that key, and keys you don't send are kept. Objects (`style`, `targeting`, `teaser`, `phone_step`) and lists (`fields`, `variants`) are replaced whole, so send the complete object. Send `null` to reset a key to its default. Unknown keys are rejected, so typos don't silently do nothing.

The same renderer draws the form on the website, on the hosted page (`/f/:id`) and in the editor preview, so what you design is what visitors see. On websites the form is drawn inside a shadow root, so the site's own CSS can't break it and `style.custom_css` can't leak out.

## Content

These keys can also be set per A/B test version (see `variants`).

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `headline` | text, ≤ 200 | "Get 10% off your first order" | |
| `body` | text, ≤ 1,000 | "Join our list…" | Line breaks are kept. |
| `button_text` | text, ≤ 60 | "Sign up" | |
| `image_url` | http(s) URL | none | Upload with `upload_image` / `POST /api/images` to get a URL. Where it shows depends on `style.layout`. |
| `image_alt` | text, ≤ 200 | "" | Alt text for the image. |
| `no_thanks_text` | text, ≤ 60 | none | A "No thanks" link under the button that closes the form (popup and flyout only). |
| `success_message` | text, ≤ 200 | "Thanks! Check your inbox." | Headline of the thank-you screen. |
| `success_body` | text, ≤ 1,000 | none | Text under it. |
| `discount_code` | text, ≤ 60 | none | Shown on the thank-you screen in a box with a Copy button. It's never sent to the website or the hosted page before someone signs up; the sign-up response carries it. |

Not per version:

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `legal_text` | text, ≤ 1,000 | none | Small print under the button. `[label](https://url)` becomes a link; nothing else is formatted. |
| `success_button_text` | text, ≤ 60 | none | A button on the thank-you screen. |
| `success_button_url` | http(s) URL | none | Where that button goes. With text but no URL, it closes the form. |
| `redirect_url` | http(s) URL | none | Send the visitor here right after a successful sign-up instead of showing the thank-you screen. |
| `auto_close_seconds` | integer 0–60 | 3, or 0 when a discount code or success button is set | Popup/flyout: close the thank-you screen after this many seconds. 0 keeps it open. |
| `confirm_message` | text, ≤ 300 | "Almost done! Check your inbox…" | Shown instead of the thank-you screen when the list uses double opt-in. |
| `sms_consent_text` | text, ≤ 1,000 | the standard carrier disclosure | Shown next to the consent checkbox under any phone field. |
| `brand_color` | hex color | the account's brand color | Default button color (see `style.button_color`). |

## Fields

`fields` is an ordered list (at most 12) of what the form asks for. It must include `email` or `phone_number`.

```json
"fields": [
  {"key": "first_name", "label": "First name"},
  {"key": "email", "required": true},
  {"key": "skin_type", "type": "select", "label": "Skin type", "options": ["Dry", "Oily", "Combination"]},
  {"key": "interests", "type": "checkboxes", "label": "I'm into", "options": ["Skincare", "Makeup"]}
]
```

| Field key | What it saves |
| --- | --- |
| `email` | The profile's email. Required by default. |
| `phone_number` | The profile's phone, plus SMS consent when the visitor ticks the consent checkbox (always shown under a phone field). |
| `first_name`, `last_name` | The profile's name. |
| `birthday` | Profile property `birthday` as `YYYY-MM-DD`. |
| anything else | A custom profile property with that key. Keys are lowercase letters, digits and `_`, start with a letter, at most 40 characters. |

Each field also takes:

| Key | Type | Notes |
| --- | --- | --- |
| `type` | custom fields only: `text` (default), `number`, `date`, `select`, `radio`, `checkboxes`, `checkbox` | `checkbox` saves true/false; `checkboxes` saves a list of the ticked options. |
| `label` | text, ≤ 100 | Shown above the input. Without it, only the placeholder shows. |
| `placeholder` | text, ≤ 100 | Defaults to a sensible hint for standard fields. |
| `required` | boolean | Defaults to true for `email`, false for the rest. Checked on the server too. |
| `options` | list of 1–20 texts, each ≤ 80 | Required for `select`, `radio` and `checkboxes`. |

Answers are kept to 500 characters (names to 100). Form sign-ups only fill in blanks: they never overwrite a name or property the profile already has. Answers to fields the form doesn't have are ignored.

`collect` (a list of `email`, `phone_number`, `first_name`) is the older way to choose fields. It still works when `fields` is absent; setting `fields` keeps `collect` in step automatically.

## Phone step

Ask for a phone number on a second screen after someone signs up with their email (the "email first, then texts" pattern). It's skipped for forms that already have a phone field.

```json
"phone_step": {"enabled": true, "headline": "Want texts too?", "body": "Get early access by text.", "button_text": "Text me", "skip_text": "No thanks"}
```

All keys except `enabled` are optional text (headline ≤ 200, body ≤ 1,000, button and skip ≤ 60). The consent checkbox and `sms_consent_text` always show. The step adds the phone number and SMS consent to the profile that just signed up (using a one-hour `phone_token` from the first screen's response, given only when that sign-up created the profile, so nobody can attach a number to someone else's email; otherwise the number is saved on its own profile); it doesn't count as a second sign-up.

## Style

`style` is one object shared by every A/B version.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `layout` | `stacked`, `image_left`, `image_right`, `background` | `stacked` | Where `image_url` goes: above the text, beside it (stacks on narrow screens), or behind it. |
| `width` | integer 280–900 (px) | popup 420, flyout 360, embed 480, page 440 | Never wider than the screen. |
| `align` | `center`, `left` | `center` | Text alignment. |
| `font` | `system`, `inherit`, `serif`, `Inter`, `Poppins`, `Montserrat`, `Lato`, `DM Sans`, `Space Grotesk`, `Playfair Display`, `Lora`, `Merriweather`, `Oswald` | `system` | `inherit` uses the website's own font. Named fonts load from Google Fonts. |
| `headline_size` | integer 14–56 | 24 | px |
| `headline_weight` | 400, 500, 600, 700, 800 | 700 | |
| `body_size` | integer 12–22 | 15 | px |
| `background_color` | hex | `#ffffff` | |
| `headline_color` | hex | `#111827` | |
| `text_color` | hex | `#4b5563` | Body, labels and small print. |
| `button_color` | hex | `brand_color` | |
| `button_text_color` | hex | white or black, whichever reads better on the button | |
| `button_full_width` | boolean | true | |
| `input_background` | hex | `#ffffff` | |
| `input_border_color` | hex | `#d1d5db` | |
| `input_text_color` | hex | `#111827` | |
| `corner_radius` | integer 0–40 | 14 | The form's corners. |
| `button_radius` | integer 0–40 | 8 | 40 makes a pill. |
| `input_radius` | integer 0–40 | 8 | |
| `padding` | integer 12–64 | 28 | |
| `border_width` | integer 0–8 | 0 (embed and page: 1) | |
| `border_color` | hex | `#e5e7eb` | |
| `shadow` | `none`, `soft`, `strong` | popup/flyout `strong`, embed/page `none` | |
| `overlay_color` | hex | `#111827` | Popup only: the dimmed page behind it. |
| `overlay_opacity` | integer 0–90 | 55 | Percent. |
| `close_on_overlay` | boolean | true | Popup only: clicking outside closes it. |
| `close_color` | hex | `#9ca3af` | The × button. |
| `position` | popup: `center`, `top`, `bottom`; flyout: `bottom_right`, `bottom_left`, `top_right`, `top_left` | `center` / `bottom_right` | |
| `animation` | `fade`, `slide`, `zoom`, `none` | `fade` | Turned off for visitors who prefer reduced motion. |
| `page_background` | hex | a light tint of the button color | Hosted page only. |
| `custom_css` | text, ≤ 10,000 | none | Extra CSS inside the form's shadow root (CSS only: it can't contain `<`). Hooks: `.dm-card`, `.dm-media`, `.dm-content`, `.dm-headline`, `.dm-body`, `.dm-field`, `.dm-label`, `.dm-input`, `.dm-consent`, `.dm-button`, `.dm-no-thanks`, `.dm-legal`, `.dm-error`, `.dm-success`, `.dm-code`, `.dm-close`, `.dm-overlay`, `.dm-teaser`. |

## Teaser

A small tab in a corner of the page that opens the popup or flyout. Popup and flyout only.

```json
"teaser": {"enabled": true, "text": "Get 10% off", "position": "bottom_left", "show": "after_close"}
```

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | boolean | false | |
| `text` | text, ≤ 40 | `headline` | |
| `position` | `bottom_left`, `bottom_right`, `left`, `right` | `bottom_left` | `left`/`right` are vertical tabs at mid-height. |
| `show` | `after_close`, `always` | `after_close` | `after_close`: appears once the visitor closes the form without signing up. `always`: also before the form opens. It has its own small × that hides it for `targeting.frequency_days`. Never shown after the visitor signs up. |

## Targeting

`targeting` decides when a popup or flyout opens. Every key is optional.

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `trigger` | `delay`, `scroll`, `exit_intent`, `manual` | `delay` | `manual` only opens from the teaser, a `data-dumbmail-open` element or `dumbmail.showForm(id)`. |
| `delay_seconds` (top-level key) | integer 0–600 | 5 | For `delay`. |
| `scroll_percent` | integer 1–100 | 40 | For `scroll`. |
| `devices` | `all`, `desktop`, `mobile` | `all` | |
| `url_contains` | list of text | none | Only on pages whose URL contains one of these. |
| `url_excludes` | list of text | none | Never on these. |
| `page_views` | integer 1–50 | 1 | Only after the visitor has seen this many pages in the current visit. |
| `visitors` | `all`, `new`, `returning` | `all` | `returning` means this browser visited before today's visit (more than 30 minutes ago). |
| `frequency_days` | integer 0–365 | 7 | After someone closes it, wait this long before showing it again. |
| `hide_for_subscribers` | boolean | true | Skip visitors the site already knows. |
| `start_at`, `end_at` | ISO 8601 date-time with a time zone, like `2026-12-01T09:00:00-08:00` | none | Only show between these times. Outside them the form doesn't load on the site and the hosted page says it's closed. |

Any element on the site with `data-dumbmail-open="FORM_ID"` opens that form when clicked, whatever its trigger.

## A/B tests

`variants` is a list of up to three extra versions, `[{"id": "b", …}, {"id": "c", …}, {"id": "d", …}]` in that order. The form itself is version A. A version can change only the content keys marked "per version" above: `headline`, `body`, `button_text`, `image_url`, `image_alt`, `no_thanks_text`, `success_message`, `success_body` and `discount_code`. Empty keys fall back to version A. Each visitor keeps one version. `list_forms` and `get_form` report views and sign-ups per version in `variant_stats`. To end a test, copy the winning version's keys into the base config and set `variants` to `[]`.

## Images

`POST /api/images` (MCP `upload_image`) stores a PNG, JPEG, GIF or WebP image up to 5 MB and returns `{id, url, content_type, size, created_at}`. Send the raw file with its `Content-Type`, or JSON `{"url": "https://…"}` to copy an image from the web, or `{"data_base64": "…"}`. The URL is public and permanent; use it in `image_url` or in emails. `GET /api/images` lists them, `DELETE /api/images/:id` removes one (anything still using the URL shows a broken image). Each account can store 500 images.

## Previewing

Every form in the API has a `preview_url`: the hosted page for that form, drafts included, with nothing counted as a view and nothing saved when someone submits it. Share it to review a form before publishing. During an A/B test it shows version A; add `&v=b` (or `c`, `d`) to see another version.
