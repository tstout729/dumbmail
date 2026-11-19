# Shopping agent

An AI assistant that answers your shoppers' questions ("where's my order?", "do you ship to Canada?", "which jacket is warmer?") from your own information, and hands the conversation to a person when it should.

It can answer in four places. Each is switched on separately in **Settings → Shopping agent**, and all are off until you turn them on:

- **Customer hub** (`hub`): a chat page inside the customer hub, for shoppers signed in there. A "Chat with …" button shows on their account page.
- **Website bubble** (`site`): a chat button on your website, added by the DumbMail snippet. Anyone can use it. It never knows who the shopper is and never shows anything personal; for order questions it points to the customer hub's sign-in (when the hub is on) or hands off.
- **Texts** (`sms`): the first answer to an ordinary text someone sends your number (not STOP, HELP, a keyword or a confirmation). It replaces your default reply while it's on. Its answers are automatic texts: they count toward the inbox's per-person limit and hourly budgets, are billed per text like any reply, and are never sent to someone who opted out.
- **WhatsApp** (`whatsapp`): the first answer to an ordinary WhatsApp text message to your business number (not STOP, START, HELP, a sign-up word or a keyword; HELP and INFO get a fixed answer with your contact details). Answers go out only through the WhatsApp number the message came in on. Answers go out as free-form WhatsApp replies inside the 24-hour service window, count toward the WhatsApp inbox's 5 automatic answers per person per hour, are billed like any WhatsApp reply, and are sent exactly as written.

The agent needs an Anthropic API key on the server (`ANTHROPIC_API_KEY`). Without one, the settings say it isn't available and nothing answers.

## What it answers from

Only these, as tools the server runs:

- `search_answers`: the questions and answers you write in the settings, and passages from the pages of your own website that you list. Pages must be on your website (its domain or a subdomain); they're read as plain text (up to 30,000 characters each, 1 MB fetched), through DumbMail's safe fetcher (public addresses only, no redirects followed), and re-read weekly. A page that redirects asks you to add the address it redirects to.
- `search_products`: your product catalog (published, verified products), by name, description, brand, category and tag, with price and stock as last synced.
- `my_orders`: only in a conversation with a verified shopper, and only that shopper's own orders, read through the customer hub's own functions:
  - in the hub, a signed-in shopper sees their orders (items, totals, parcels and tracking) when the hub's Orders section is on and their email came from your own systems (the hub's rule);
  - by text or WhatsApp, someone writing from the number on their profile sees order status and tracking only, when that number (and any email on the profile) came from your own systems: a profile created or edited through the API, MCP or the dashboard, a CSV import, an integration or Shopify. A number that only ever texted or wrote in on WhatsApp is never counted as yours, and neither is one the website snippet or a form added.
  The tool takes no input: whatever the model asks, it reads the conversation's own person in your account.

  Order access rests on a proof: the hub session that allowed it, or the number the shopper writes from. It's checked again before every order lookup and before an answer that used one goes out (for texts and WhatsApp, right before the provider gets it). When it no longer holds (they signed out everywhere or changed their email, the Orders section was switched off, the number stopped being theirs or yours), earlier order results are dropped from the conversation and it goes on without them.
- `hand_off`: marks the conversation for your team with a reason and a short summary.

Website text and product text are passed to the model as data, never as instructions.

What crosses to the model and back is checked:

- Card numbers, government ID numbers and passwords or one-time codes in a shopper's message are replaced with placeholders before it reaches the model or the transcript.
- Links in an answer are kept only when they point to your website, your link domain or the hub, or are an exact address a tool returned in that conversation (a parcel's tracking link). Every other link is removed, while streaming too.
- An answer that names a price, a percentage or a discount code that no tool returned is replaced with a hand-off. While an answer names one, or after the shopper's orders were read, the rest of it is shown only once these checks pass.
- An answer counts only when the model's response arrived complete and every tool call fits its schema; anything else is withdrawn and handed off.

## Rules it keeps

These are DumbMail's and can't be removed by your settings or by anything a shopper or a page says:

- It never invents prices, discounts, codes, policies, delivery dates or stock; it quotes your own words for policies.
- It never changes an order, a subscription, an address, a payment or anyone's email and text preferences (it can point to the hub's preferences).
- It never reveals anything about another person, never sends marketing and never signs anyone up.
- It hands off for refunds, complaints, anything legal or medical, a frustrated shopper, a request for a person, or when it isn't sure.
- It keeps to your store and replies in the shopper's language.

You set its **name** (default "<store> assistant") and a short **tone note** (style only).

If the model declines, its answer is cut off, or the service fails, the shopper gets a polite hand-off message instead of a made-up answer, and the conversation is marked for your team.

## Hand-offs and transcripts

Conversations are listed under **Inbox → Shopping agent**, newest first, with hand-offs marked (reason and summary). A conversation opens on its newest messages, with earlier ones a page at a time. Once a conversation is handed off, the agent stops answering in it; the shopper's later messages are kept for your team. A handed-off text or WhatsApp conversation is reopened in its inbox, where you reply as usual.

A conversation with a verified shopper is kept with their profile; website-bubble chats are anonymous. Conversations are deleted 90 days after their last message (by a background job, in batches), and with the person when they're deleted. When a Shopify privacy request erases a person's orders but keeps the person, the agent's conversations that could hold those orders go too.

Events: `Chatted with Agent` (a verified shopper started a conversation) and `Agent Handed Off`, with `place`, `conversation_id` and (for hand-offs) `reason`. Only the server records them; the website snippet can't.

## Limits and cost

- 20 shopper messages and 400,000 model tokens per conversation (checked before every model request, with the conversation so far counted in); 5 model steps per message; one message at a time per conversation.
- A monthly conversation cap you set (default 500), counted in your account's time zone. Past it, shoppers are told chat isn't available and given your contact.
- 10 new conversations per visitor (IP address) per hour.
- At most 16 model calls at once per server process.
- On the hosted service, each conversation costs `PRICE_AGENT_CONVERSATION` (shown in the settings). It's held when the conversation starts (your balance must cover it on top of every fee still held) and charged once, when the first answer actually reaches the shopper (in the chat, or when the texting or WhatsApp provider takes it). If nothing goes out (a failure, a busy server, a skipped send), the hold is given back. With no credit, shoppers are told chat isn't available.

## Settings, API and MCP

| What | REST | MCP | Permission |
| --- | --- | --- | --- |
| Read settings | `GET /api/agent/settings` | `get_agent_settings` | settings read |
| Change settings | `PATCH /api/agent/settings` | `update_agent_settings` | settings write |
| List conversations | `GET /api/agent/conversations?handed_off=&limit=&cursor=` | `list_agent_conversations` | people (inbox) read |
| One conversation | `GET /api/agent/conversations/{id}?before=&limit=` (newest 50 first; `older_cursor` for more) | `get_agent_conversation` | people (inbox) read |
| Test chat | `POST /api/agent/test` `{message, conversation?}` | `test_agent_chat` (runs only with `confirm: true`; otherwise a dry run) | settings write |

Settings fields (send only what changes):

- `hub`, `site`, `sms`, `whatsapp` (booleans): where it answers.
- `name` (up to 40 characters), `tone` (up to 300).
- `answers`: `[{question, answer}]`, up to 100 (questions up to 300 characters, answers up to 2,000). Replaces the list.
- `pages`: up to 20 page addresses on your website. Replaces the list; new pages are read right away, then weekly. The settings show each page's title, status and any error.
- `monthly_cap`: 1 to 100,000.

The test chat runs as a made-up shopper with a made-up order, whatever places are on. It uses the model and counts toward the monthly cap, but isn't charged and sends nothing.

Setting changes are recorded in the audit log.

## Public pages

On the customer hub's host (your branded link domain, or DumbMail's shared one):

- `GET /hub/{public_key}/chat`: the hub chat (signed-in shoppers; others go to the hub's sign-in). Messages go to `POST /hub/{public_key}/chat/send` with the session's form token, from the hub's own origin.
- `GET /hub/{public_key}/ask`: the bubble's chat, framed only by your website's origin (or the hub's approved panel origins). Messages go to `POST /hub/{public_key}/ask/send`.
- `GET /client/agent.js?k={public_key}`: the bubble, loaded by the snippet only when the bubble is on.

Messages are JSON `{message, conversation?}`; the answer streams back as server-sent events: `delta` (text), `reset` (discard the text so far), then `done` with `reply`, `conversation` (send it with the next message) and `handed_off`.
