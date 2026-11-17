# DumbMail for Zapier

A Zapier CLI app (`zapier-platform-core` 19) that connects DumbMail to the apps people use with
Zapier.

- **Triggers** (instant, through REST hooks): New Event (optionally one metric, like Placed Order),
  New Profile, Updated Profile, New Subscriber, New Unsubscribe, Profile Added to List, Profile
  Entered Segment, New Form Submission, Email Opened, Link Clicked, Email Bounced, Campaign Sent.
- **Actions**: Create or Update Profile, Track Event, Add Profile to List, Remove Profile From
  List, Unsubscribe Profile.
- **Search**: Find Profile (and Find or Create Profile).

People connect with a DumbMail API key (Settings → Developers) made by an owner or admin, with the
`full` scope (or `account` plus `audience` for the actions). Self-hosted servers enter their
own address; it must be `https://`.

## How it works

When a Zap turns on, Zapier calls `POST /api/hooks` with its hook URL and the trigger's topic;
DumbMail then sends each event to that URL as one signed JSON envelope
(`{id, topic, created_at, account_id, api_version, source, data}`). Turning the Zap off calls
`DELETE /api/hooks/:id`. If Zapier answers a delivery with 410 Gone, DumbMail removes the hook.
A hook belongs to the API key the Zap connected with; revoking the key removes its hooks.

Every delivery is checked before it reaches the Zap: DumbMail signs each request with the hook's
secret (returned when the hook is made and kept in `subscribeData`), and the trigger drops any
request whose signature doesn't verify or is more than 5 minutes old (`verified` in
`triggers/hooks.js`). Still, **treat the Zap's hook URL as a credential**: don't share it, and if it
leaks, turn the Zap off and on to get a new one. Each event also carries `source`; only `system`,
`api` and `integration` events should drive anything that moves money or ships goods, since
`browser` events can be sent by anyone with the site's public key.
Sample data for the Zap editor comes from `GET /api/hooks/samples` (the account's latest real
events when there are any). See `docs/WEBHOOKS.md` in the DumbMail repository.

Actions use the regular DumbMail API: `POST /api/profiles`, `POST /api/events`,
`POST /api/lists/:id/members`, `GET /api/profiles?q=` + `DELETE /api/lists/:id/members`.

## Develop

```sh
npm install
npm test                 # unit tests against a mocked DumbMail API
npm run validate         # Zapier's structural checks (no account needed)
npx zapier-platform validate   # plus Zapier's online style checks
```

## Publish

Publishing needs a Zapier developer account (free) and is done by whoever owns the DumbMail
integration on Zapier.

1. `npx zapier-platform login`
2. `npx zapier-platform register "DumbMail"` (first time only; it writes `.zapierapprc`, which is
   ignored by git) and fill in the app's category, logo and description in the
   [developer platform](https://developer.zapier.com).
3. `npx zapier-platform push` uploads this version. Try it in a private Zap with a real DumbMail
   account: connect, turn on a New Event Zap, track an event, run each action.
4. Invite testers with `npx zapier-platform users:add` or share a link from the developer platform.
5. To list it in the Zapier App Directory, `npx zapier-platform promote 1.0.0` and complete
   Zapier's publishing checklist (it needs live users and a review by Zapier).

Bump `version` in `package.json` for each new push, and use `npx zapier-platform migrate` to move
existing users to a new version.
