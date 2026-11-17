# DumbMail for Make

A Make custom app: an instant trigger fed by DumbMail webhooks, actions for profiles, events and
lists, and a profile search. Each file here is one tab of one component in Make's app editor
(Make → Custom apps, or the Make Apps Editor extension for VS Code), written in Make's IML JSON.

## Components

**Base** (`base.imljson`): every request goes to the connection's DumbMail address with
`Authorization: Bearer <API key>`, shows DumbMail's error message, and hides the key in logs.

**Connection** `dumbmail` (type: API key / basic custom connection). The key must be made by an
owner or admin, with the `full` scope (or `account` plus `audience` for the actions): REST hooks
need the `webhooks.manage` permission.

| Tab | File |
| --- | --- |
| Communication | `connections/dumbmail/communication.imljson` |
| Parameters | `connections/dumbmail/parameters.imljson` |

It checks the key with `GET /api/hooks/me` and labels the connection with the account's name.
Self-hosted servers set "DumbMail address" (an advanced field).

**Webhook** `dumbmail` (type: dedicated, attached; connection: `dumbmail`)

| Tab | File |
| --- | --- |
| Communication | `webhooks/dumbmail/communication.imljson` |
| Parameters | `webhooks/dumbmail/parameters.imljson` |
| Attach | `webhooks/dumbmail/attach.imljson` |
| Detach | `webhooks/dumbmail/detach.imljson` |

Attaching calls `POST /api/hooks {url, topic, metric, source: "make"}`, which subscribes the
Make webhook URL to one DumbMail topic and returns its id; detaching calls
`DELETE /api/hooks/{id}`. A hook belongs to the API key that made it: that key can list and delete
only its own hooks, and revoking the key removes them. DumbMail sends each event as one signed JSON
envelope (`{id, topic, created_at, account_id, api_version, source, data}`); if Make ever answers
410 Gone, DumbMail removes the hook.

**The webhook URL is the credential.** Make's custom-app webhooks accept whatever is posted to
their URL; the app can't check DumbMail's `DumbMail-Webhook-Signature` there (it would need the
endpoint's signing secret, which the attach step receives but Make's webhook communication can't
use to verify a request). So anyone who learns a scenario's webhook URL can feed it fake events.
Tell users, in the app's description and help: treat the webhook URL like a password, don't share
it, and if it leaks, delete the webhook in Make and create a new one (DumbMail drops the old hook
when Make answers 410). Check `source` in the envelope: only `system`, `api` and `integration`
events should drive anything that moves money or ships goods; `browser` events can be sent by
anyone with the site's public key.

**RPC** `listLists`: `rpcs/listLists/communication.imljson` fills the List dropdowns.

**Modules**

| Name | Type | Label | Description | Files |
| --- | --- | --- | --- | --- |
| `watchEvents` | Instant trigger (webhook `dumbmail`) | Watch events | Triggers when something happens in DumbMail: an event is tracked, a profile changes, someone subscribes or unsubscribes, a message is opened, a form is submitted and more. | `communication`, `interface`, `samples` |
| `createUpdateProfile` | Action | Create or update a profile | Adds a profile, or updates the one with the same email, phone number or external ID. | `communication`, `mappable-parameters`, `interface` |
| `trackEvent` | Action | Track an event | Records an event, like Placed Order, for a person. | same |
| `addToList` | Action | Add a profile to a list | Adds a person to a list by email, creating the profile if it's new. | same |
| `removeFromList` | Action | Remove a profile from a list | Removes a profile from a list (find its ID with Search profiles). | same |
| `unsubscribeEmail` | Action | Unsubscribe from email | Unsubscribes an email address from marketing email. | same |
| `unsubscribeSms` | Action | Unsubscribe from texts | Unsubscribes a phone number from marketing texts. | same |
| `searchProfiles` | Search | Search profiles | Finds profiles by email, phone number, name or external ID. Pages through results 100 at a time up to the limit. | same |
| `makeApiCall` | Universal (REST) | Make an API call | Calls any DumbMail API route with the connection's key. The path is always under `/api/` on the connection's DumbMail address, so the key is never sent anywhere else. | `communication`, `mappable-parameters`, `interface` |

Every module uses the `dumbmail` connection. Files map to tabs by name: `communication.imljson` →
Communication, `mappable-parameters.imljson` → Mappable parameters, `interface.imljson` →
Interface, `samples.imljson` → Samples. Static parameters are empty everywhere (the trigger's
choices live on the webhook).

The actions call the regular DumbMail API: `POST /api/profiles`, `POST /api/events`,
`POST|DELETE /api/lists/{id}/members`, `GET /api/profiles?q=`, `GET /api/lists`.

## Make's review requirements

- A universal module (`makeApiCall`), using a relative path only.
- Searches paginate (`searchProfiles`, with `pagination` and a stop condition).
- The key is hidden in logs (`base.imljson` sanitizes the Authorization header).
- Every module has a label, a description and an interface.

## Publish

This can't be published from the repository; someone with a Make account does it once:

1. In Make, open **Custom apps** → **Create a new app** (name `dumbmail`, label "DumbMail",
   the DumbMail logo, theme `#1f6f5c`).
2. Paste `base.imljson` into Base, then create the connection, webhook, RPC and modules above,
   pasting each file into its tab. (With the Make Apps Editor for VS Code, create the same
   components and paste the files in the same way, then deploy.)
3. Test in a scenario with a real DumbMail account: connect with an API key, add **Watch events**
   for "Event tracked", track an event (`POST /api/events`), and run each action.
4. To let others use it, share the app's invite link (Custom apps → Publish → Share). To list it
   for everyone, request a review from Make.

These files follow Make's custom app documentation; they haven't been run inside Make itself.
