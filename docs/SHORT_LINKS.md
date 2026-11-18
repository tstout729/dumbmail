# Short links in text messages

A link in a text is billed by its length. Without short links, a tracked link carries its whole
destination (`https://<link domain>/t/c/<token>?u=<the whole URL>&s=<signature>`), so one link can
push a text into a second or third segment. With short links, every link the sender wrote becomes

```
https://<link domain>/s/Ab12Cd34Ef56Gh78Jk90Lm12
```

the same length whatever it points to: `/s/` and 24 letters and digits. On dumbmail.ai's shared
link domain that's 46 characters. Short links are on for every account, new and existing; turn
them off in Settings → Texting (`PATCH /api/short-links/settings`, MCP
`update_short_link_settings`).

## What gets shortened

- Links the sender typed into the text: `http` and `https` links written literally in the message,
  outside any template tag. Sentence punctuation right after a link (`.` `,` `!` `?` `;` `:`, quotes,
  and closing brackets that aren't balanced inside the link) isn't part of it: it stays in the text
  after the short link. The destination is exactly the characters the short link replaced. So
  "Shop the sale: https://shop.com/sale." shows the short link followed by a full stop and goes to
  `https://shop.com/sale`. Links that keep the long tracked form end in the same place.
- Not shortened (they keep the long tracked form, as before): anything a variable or template tag
  produces, even when it's a link written elsewhere in the message (like
  `https://shop.com/?ref={{ email }}` or a product link); links whose address or path contains
  `unsubscribe`, `optout`, `opt-out`, `opt_out` or `stop`, so where they go stays visible;
  DumbMail's own links (unsubscribe, forms, tracking); links longer than 2,000 characters; and links
  past the tenth in one text.
- The brand name, the opt-out line ("Reply STOP to opt out.") and STOP handling are unchanged.
- Campaign texts, flow texts, replies typed in the inbox, keyword answers and automatic replies all
  use short links. Email links are unchanged.

## Link tags (UTM)

With short links on, the link tags from Settings → Tracking (or the campaign's or flow's own) are
added to the destination of each short link, exactly as for email. They cost nothing: the short
link stays 24 characters after `/s/`. Links that keep the long form aren't tagged, since tags would
make them longer.

## Previews and segment counts

The text editor's preview, `POST /api/render` and the inbox's reply preview show each link that
would be shortened as a sample short link of the real length, so the character and segment counts
match what's sent. Billing counts segments from the final text, with its short links.

## How a short link works

The 24 characters are three parts:

| Part | Length | What it is |
| --- | --- | --- |
| Target | 6 | A random code for the destination. One row per destination per account (`short_link_targets`), reused by every text that links there, never one per recipient. |
| Message | 6 | A random reference written on the text's message row (`messages.short_ref`). |
| Tag | 12 | HMAC-SHA256 under the server's secret key over the target code, a hash of the destination and the message's full id, in base 62. |

A click on `/s/...` looks up the target and the messages in its account with that reference, keeps
the one the tag was made for, records the click exactly as a `/t/c/` click is recorded (the
`Clicked SMS` event, the message's `clicked_at`, the click history, attribution and webhooks) and
redirects to the stored destination. The destination is only ever one the send path stored for
that account, so a short link can't be pointed anywhere else. A link of the wrong shape, an unknown
or changed link, a deleted target or a closed account's link gets the plain "link not found" page.
Old `/t/c/` links keep working.

### Why 12 characters of tag is enough

The tag ties one destination to one message: changing any character fails, a valid link for one
message can't record a click for another (the tag covers the message's full id, not only its
6-character reference), and a link always goes where it went when it was sent: if cleanup frees a
code and it's later given to another destination, the old link's tag no longer matches.

Target codes and message references aren't secret; the tag is what can't be guessed. 12 base-62
characters is 62^12, about 3.2 × 10^21 (71 bits). The click endpoint limits each address to 30
lookups a minute that end in "not found" (then 429 for the rest of the minute) and 32 lookups at
once per server (then 503), but don't count on that: assume an attacker spread over many addresses
sends 100,000 guesses a second for ten years (3 × 10^13 guesses). Each guess matches with
probability 1 in 3.2 × 10^21, so the chance of forging a single click in all that time is about 1 in
100 million.

Message references are 6 characters (5.7 × 10^10 values per account) and unique in their account:
the statement that writes a text's row takes the first of 8 candidates not already used, and the
text carries the one it got.

## Cost

- A text without a link: nothing extra.
- A flow text, reply, keyword answer or test with links: one statement for all of its links (it
  creates or reuses their targets and checks the setting).
- A campaign text: none per recipient. A text campaign's targets are created when its audience is
  fixed, at every launch (a failed or cancelled campaign that's edited and relaunched gets them
  afresh), and kept apart from its editable content (`short_link_campaigns`); the sender reads
  them once per batch of recipients. A campaign keeps the setting it was launched with.
- The message's reference is written by the statement that already writes the message row.

## Housekeeping

- Targets not put in a text for 400 days are deleted by a background job, in batches. A click on a
  deleted target shows the "link not found" page.
- A closed account's short links stop redirecting at once, and the same background job deletes its
  targets and campaign link maps.
- Request logs show short links as `/s/…`, without their 24 characters.
- Lookups use an index on `messages (account_id, short_ref)`, built in the background after the
  server starts. Each lookup checks in the same statement that the index is there and reads
  `messages` only if it is; if it's gone, or the lookup fails, the click gets "link not found" and
  the server stops issuing short links until a check finds the index again.
- A server puts short links in texts only once that index is in place and it has been accepting
  requests for 2 minutes. During a deploy the old server keeps getting some clicks for a short while
  and can't resolve `/s/` links, so texts sent in a new server's first 2 minutes use the long form.
  The setting's `ready` says whether short links are being issued now.

## API and MCP

- `GET /api/short-links/settings` (MCP `get_short_link_settings`): `enabled`, `sample` (a short link
  of the real length on your link domain), `link_length` and `ready`.
- `PATCH /api/short-links/settings` with `{"enabled": true | false}` (MCP
  `update_short_link_settings`). Needs permission to change settings. Recorded in the activity log.
- `GET /s/{path}`: the click (public).
