# Messages in each person's language

One campaign, flow email, text, template or saved block can carry versions in several languages. Each person is sent the version for their language; anyone else gets the default. Machine translation gives you a first draft that a person reviews. DumbMail's own pages and footer lines appear in the person's language too.

Nothing changes for an account that never adds a second language: the same email, byte for byte.

## A person's language

Profiles have a `locale`: a language tag such as `fr`, `fr-CA` or `pt-BR` (BCP 47). Tags are checked and tidied (`pt_br` becomes `pt-BR`); at most 35 characters.

- **API, MCP and imports:** send `locale` (or `language`) with the profile, e.g. `{"email": "ana@example.com", "locale": "pt-BR"}`. An import column named `locale` or `language` works the same way. A `locale` that isn't a tag is refused; `locale: null` or `""` clears it. A `language` that isn't a tag (like "French") stays an ordinary custom property.
- **Klaviyo import:** Klaviyo's `locale` field, tidied the same way.
- **By hand:** on the profile page.
- **A browser's language** (a sign-up form's `locale` or `language` field, the browser's language when the form is sent, the website script) is kept apart, as a hint (`locale_hint`). It's only used to choose which language version someone is sent when they have no language set another way; it fills in only when empty, never shows up in segments, templates or custom properties, and never changes `locale`.
- Personalization: `{{ person.locale }}` (the trusted language only).

### Segments

```json
{"type": "language", "op": "is", "value": "fr"}
{"type": "language", "op": "is_one_of", "values": ["fr", "es"]}
{"type": "language", "op": "is_not", "value": "fr-CA"}
```

A tag without a region matches every region of that language (`fr` matches `fr` and `fr-CA`); a tag with a region matches only that region. `is_not` includes people whose language isn't known.

## Account languages

Settings → Languages: the **default language** (what your content is written in, `en` unless you change it) and the **languages you send in** (up to 20 more).

- `GET /api/translations/settings`, `PUT /api/translations/settings` `{"default_language": "en", "languages": ["fr", "es"]}` (settings permission). MCP: `get_languages`, `set_languages`.

## Language versions of a message

A translation is not a second copy of the message. Each text field carries its translations next to it, so layout, images, display rules, products, coupons, links (unless you localize one) and the footer stay shared and can't drift. Translatable text: the subject, preview text, custom HTML, a text message's body, and in blocks every heading, text, button label, alt text, caption, quote, table cell, menu link label, footer text and coupon label. Links can be localized too; until someone does, the default link is used.

Each translated field has a status:

| Status | Meaning |
| --- | --- |
| `missing` | Not translated. The default text is sent. |
| `machine` | A machine draft nobody has reviewed yet. It is sent as drafted. |
| `check` | A machine draft whose numbers, links or email addresses differ from the original. It is **not** sent (the original text is) until a person checks it and marks it reviewed. |
| `reviewed` | Written, imported or reviewed by a person. |
| `stale` | The default text changed after it was translated. The default text is sent until someone updates it. |
| `default` | A link nobody localized (the default link is used). |

### Which version someone gets

Decided once per recipient (their `locale`, else their browser's hint) when the message is prepared, with no extra lookup: the version with their exact tag, else their language without the region (`fr` for `fr-CA`), else the default. Another region's version is never used (someone in `pt-PT` doesn't get `pt-BR`). A campaign's versions are frozen with the campaign when its send starts, like its saved blocks. A/B tests and languages combine: each A/B version has its own translations. Saved blocks carry their own translations and follow the language the email is sent in.

The language each message went out in is kept on the message: `GET /api/translations/report?campaign_id=...` (or `flow_id`) gives sent, opened, clicked and unsubscribed by language. MCP: `translation_report`.

### Reading and changing translations

Every endpoint exists for each kind of message:

| Message | Path | Read | Change |
| --- | --- | --- | --- |
| Campaign | `/api/translations/campaigns/{id}` (`version=b` for an A/B version) | messaging.read | campaigns.draft (drafts; scheduled ones need campaigns.send) |
| Flow step | `/api/translations/flows/{id}/steps/{step}` | messaging.read | flows.edit |
| Template | `/api/translations/templates/{id}` | content.read | content.write |
| Saved block | `/api/translations/saved-blocks/{id}` | content.read | content.write |

- `GET {path}?language=fr`: every text with its id, where it is, the default text, the translation and its status, plus a count of statuses per language.
- `PUT {path}` `{"language": "fr", "strings": [{"id": "subject", "text": "Bonjour {{ first_name }}"}]}`: save translations (as `reviewed` unless `"status": "machine"`). An empty `text` removes one. `"remove_language": true` takes a language off the message.
- `POST {path}/review` `{"language": "fr"}`: mark machine drafts reviewed (all, or `ids`).
- `GET {path}/export?language=fr&format=xliff` (or `csv`): the texts for a translator. XLIFF 1.2 has one `file` whose `original` is the message (`campaign/<id>`, `campaign/<id>/b`, `flow/<id>/<step>`, `template/<id>`, `saved_block/<id>`) and whose `target-language` is the language; each text is a trans-unit with its id, the default text, the hash of that text (`resname`) and the current translation; the glossary is in the header. CSV columns: `message, language, id, where, source, translation, status`; cells a spreadsheet would run as a formula (starting with `=`, `+`, `-`, `@`) start with an apostrophe, which importing removes.
- `POST {path}/import` `{"language": "fr", "format": "xliff", "data": "<file text>"}`: read a translator's file back (2 MB and 5,000 texts at most; DTDs aren't accepted). A file made for another message or language is refused. Translations are saved as reviewed. A text whose default changed since the export (by text or hash) is skipped and listed (`changed_since_export`); one that breaks the rules below is refused and listed.

MCP: `get_translations`, `set_translations`, `review_translations`, `export_translations`, `import_translations`, with `target: {"kind": "campaign" | "flow" | "template" | "saved_block", "id": "...", "version": "b", "step": "..."}`.

Field ids: `subject`, `preheader`, `html`, `body`, and for blocks `b<n>:<field>` (the n-th block from 0, e.g. `b0:text`, `b3:links.1.text`) or `b<n>.c<column>.<k>:<field>` for blocks inside columns.

### Rules a translation must follow

- Only words change. Output tags (`{{ first_name }}`) are the same set, expression for expression (quoted text exactly), and may move within the text but not into or out of an attribute. Control tags (`{% if %}`, `{% coupon_code 'WELCOME' %}`) stay identical, in the same order and nesting. HTML keeps the same elements in the same order and nesting, with the same attributes and values; only text and the attributes that are words for people (`alt`, `title`, `aria-label`, `placeholder`) may differ. Plain-text fields (subject, preview text, button labels, texts) can't gain markup. It must parse as a template. All of this is checked when a translation is saved (by hand, the API, MCP, an import or a machine draft), not at send time.
- No direction marks or invisible characters (bidirectional controls, zero-width spaces and other format characters, typed or as HTML references). Zero-width joiners and non-joiners are fine.
- A localized link is an `https://` address (or `http://` where the original was), with its host written out and no user name or password; a `mailto:` or `tel:` link only replaces one of the same kind. DumbMail's own links (unsubscribe, preferences, tracking) can't be localized, and no localized link can point at DumbMail. It's tracked and tagged (UTM) like the original.
- Translated HTML is inserted the way the default HTML is; plain text is escaped.
- A translation can never change who gets a message, its tracking, its unsubscribe link or its footer: only words.

Translations live inside the message's content (a block's `i18n`, and the content's `translations`); the endpoints above fill in the bookkeeping. Content saved through the regular campaign, flow, template and saved-block APIs is checked the same way.

### Before sending

The check before sending (`check_email`, the Check panel) warns, per language, about texts that are missing, out of date or unreviewed machine drafts, and checks each language's subject (length, capitals, `!!`), preview text, rendering and localized links. None of these stops a send.

### Preview and test in a language

- `POST /api/translations/preview` `{"channel": "email", "content": {...}, "language": "fr"}`: the email as it goes out in French (sample person unless `profile_id`). MCP: `preview_in_language`.
- `POST /api/translations/campaigns/{id}/test` `{"to": ["you@example.com"], "language": "fr", "version": "b"}`: a test send in that language, with the usual test limits (campaigns.test). MCP: `send_test_in_language` (needs `confirm: true`).

## Draft with AI

When the server has `ANTHROPIC_API_KEY`, "Draft with AI" machine-translates a message's missing and out-of-date texts (or chosen ones) with Claude (`claude-sonnet-5-5`). Without the key the button is hidden and import and export still work.

- `POST {path}/draft` `{"language": "fr"}` (or `ids`). MCP: `draft_translations` (without `confirm: true` it says how many texts and characters it would translate).
- It runs in the background: `GET /api/translations/jobs/{id}` (MCP `get_translation_run`) shows `queued`, `running`, `done` or `failed`, with how many texts were translated or skipped.
- Machine drafts are drafts: nobody has checked that they mean what the original means. Read them before marking them reviewed.
- Only the texts and the glossary terms that occur in them are sent to the model (as data, apart from the instructions), never anything about people.
- After the model answers, every number (prices, dates, percentages, phone numbers), web address and email address in the original must appear unchanged in the draft, and no new ones may appear. A draft that fails this is kept with the status `check`, the check before sending lists it as a problem that blocks sending, and the original text is sent in its place until a person marks it reviewed. Template tags, HTML tags and never-translate terms are replaced by placeholders before the request and put back after; a result that loses, repeats or reorders one is thrown away, and so is one that adds a template tag. What comes back is checked like a hand-typed translation and saved as a `machine` draft.
- A draft is written only where the default text is still the one that was translated and nobody has saved a reviewed translation in the meantime.
- Requests go only to `https://api.anthropic.com` and are at most 64 KB each (text, glossary and instructions together); a text too big for a request is skipped.
- Limits: one run per account at a time, a daily character allowance per account (`TRANSLATION_DAILY_CHARS`, 200,000 by default; days in the account's time zone), at most 12,000 characters per request and 40,000 per text. Characters count once they're sent, whether or not what came back was usable. A run has 10 minutes in all; one worker holds it at a time. Failures that may pass (busy, a server problem, no answer) are tried again within that time; otherwise the run ends `failed`, saying how many texts weren't drafted (what was drafted so far is kept).
- When drafting costs money (hosted, `PRICE_TRANSLATION_PER_1000_CHARS` above 0), REST and MCP both answer with a quote unless `confirm: true`. Each batch's drafts and its charge are saved together. Starting a run needs content.write and the permission that changes that message; it's recorded in the audit log (who, which message, how many texts and characters, never the text).
- Price: free unless the hosted service sets `PRICE_TRANSLATION_PER_1000_CHARS`.

## Glossary

Terms with a fixed translation per language, and terms never translated (like your brand name). Machine drafts follow it; exports show it to translators.

- `GET /api/translations/glossary`, `POST` `{"term": "DumbMail", "never_translate": true}` or `{"term": "free shipping", "translations": {"fr": "livraison gratuite"}}`, `PATCH /api/translations/glossary/{id}`, `DELETE /api/translations/glossary/{id}` (settings permission; up to 500 terms). MCP: `list_glossary`, `save_glossary_term`, `delete_glossary_term`.

## DumbMail's own pages and footer lines

The unsubscribe page, the email preference page, the double opt-in email and confirmation page, and the footer lines DumbMail adds to email ("You're receiving this because you subscribed.", "Unsubscribe") come in English, Spanish, French, German, Italian, Portuguese and Dutch. They're shown in the person's language: their profile's, else their browser's hint, else your default, among the languages you send in. You can reword any of them for any of your languages (other languages start in English until you do).

Right-to-left languages (Arabic, Hebrew, Persian, Urdu ...) get `dir="rtl"` on pages, on the root of full HTML emails, and on the text of block emails (not on their layout, so columns keep their order); left and right alignment swap sides, and the footer's address and links keep their own direction.

- `GET /api/translations/system-texts?language=fr`: each text's key, where it appears, the English, the shipped translation and your wording.
- `PUT /api/translations/system-texts` `{"language": "fr", "texts": {"unsub.button": "Je me désabonne"}}` (null goes back to the shipped text; keep `{name}` and `{days}`; no invisible characters; the unsubscribe and preference labels must show words). MCP: `get_system_texts`, `set_system_texts`.
- A campaign's footer wording is fixed when its send starts: changing it later doesn't change what the rest of that campaign's recipients get.

Not included yet: a "view in browser" page (DumbMail has none), sign-up forms' own messages, and the "Reply STOP to opt out." line added to texts, which stays in English.
