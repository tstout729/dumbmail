# Moving from Klaviyo

DumbMail can bring your whole Klaviyo account across in one go: profiles with their exact consent, lists, segments, event history, templates, campaigns, flows and forms. When it's done, a report puts Klaviyo's numbers next to DumbMail's so you can see nothing was lost.

Nobody is emailed or texted during the import, no flow starts, nobody becomes easier to reach than they were in Klaviyo, and no restriction already recorded in DumbMail (an unsubscribe, a STOP, a suppression, an unconfirmed sign-up) is lifted.

In the dashboard: **Settings → Move from Klaviyo**. With an AI assistant: the `klaviyo_*` MCP tools below.

## 1. Make a read-only Klaviyo key

In Klaviyo, go to **Settings → Account → API keys → Create Private API Key**. Name it "DumbMail", choose **Custom key**, and give it **Read access** to:

Accounts, Campaigns, Catalogs, Coupons, Events, Flows, Forms, Lists, Metrics, Profiles, Segments and Templates.

Read access is all DumbMail needs. It never changes anything in Klaviyo. Copy the key (it starts with `pk_`).

The key is stored encrypted only while the import runs. It is deleted when the import finishes, when you cancel, or when it stops with an error. DumbMail shows only its last four characters. One DumbMail account takes one Klaviyo account.

## 2. Connect and look before importing

Paste the key and press **Connect** (MCP: `klaviyo_survey` with `api_key`). DumbMail checks the key, then counts what's in the account: profiles, lists, segments, metrics, an estimate of events, templates, campaigns, flows, forms, catalog items, coupons and suppressions, plus how long the import should take. Large counts are shown as "more than"; the import counts them exactly.

If the key is missing a read scope, the survey says which. Profiles are required; without other scopes those parts are left out.

## 3. Start the import

Choose what to bring (profiles and consent always come), how far back to import events (two years by default, because event history can be huge; the day starts at midnight in your account's time zone), and what to do with **people who never subscribed** in Klaviyo:

- **Keep them emailable, as in Klaviyo** (the default, `never_subscribed: "send"`). Klaviyo can email people who never subscribed, and so can DumbMail.
- **Hold them back from marketing until they opt in** (`never_subscribed: "hold"`). They're left out of campaigns and get only service-message flows until they subscribe.

Anyone whose Klaviyo consent is missing or a state DumbMail doesn't recognise is always held back this way.

Then **Start import** (MCP: `klaviyo_start_import` with `confirm: true`; without it you get a dry run).

While event history is being imported, and for each segment's first refresh after it, **flows triggered by segments don't start** for people who enter the segment (imported orders would otherwise start them). People who enter after that start them as usual.

The import runs in the background in this order. You can leave the page, pause and resume, or cancel. If the server restarts, it carries on from the last page it saved.

1. **Metrics**: the list of Klaviyo metrics.
2. **Profiles**: email, phone, external id, names, every custom property, location (country names become ISO codes like `US`), and the Klaviyo id (`klaviyo_id` property). Each profile keeps Klaviyo's consent record in its `klaviyo_consent` property: status, time and method of each opt-in, and every suppression and list suppression with its reason and time.
3. **Lists** and their members, with each list's opt-in setting. Members join quietly: list-triggered flows don't start. Someone who unsubscribed from a list in Klaviyo, or left a list in DumbMail (the preference page, or removed by hand), is never added back to it by the import. A list opt-out only affects that list.
4. **Segments**. Rules that mean the same thing in DumbMail become live segments. If any rule can't be expressed (predictive analytics, region, distance, consent filtered by method, event rules that reach back further than the imported history, and so on), the segment comes across as a list of its current members, named "… (Klaviyo segment snapshot, date)", and the report says why. Running the import again replaces a snapshot's members whole (people no longer in the segment come off). Meaning is never changed silently.
5. **Templates**: the HTML is kept (the original too, under `imported.original_html`). Klaviyo tags become DumbMail tags: `{{ person.first_name|default:"there" }}` → `{{ person.first_name|default('there') }}`, `{% unsubscribe %}` → an unsubscribe link, `{{ person|lookup:'Color' }}` → `{{ person.properties['Color'] }}`, `{{ event.Items }}` → `{{ event.properties.Items }}`, `organization.full_address` → `organization.address`. Tags DumbMail doesn't have (`{% web_view %}`, coupon codes, catalog lookups, some filters) are removed and listed, so a template never breaks at send time. Content inside `{% comment %}` stays commented out. A condition or loop that can't be converted is replaced by a visible "Needs review" marker (never shown with both branches). `safe` is kept only on your own content (text in the template, your organization's details), never on profile or event data. Any of these marks the template **needs review** (see below).
6. **Campaigns**: sent campaigns come across as history (name, subject, send time, audience names and Klaviyo's headline numbers: recipients, delivered, unique opens and clicks, unsubscribes, bounces, spam complaints, orders and revenue). Drafts stay drafts, with each message's own audience. A campaign scheduled in Klaviyo comes across as a draft so it can't go out twice. A draft whose audience, exclusions or send settings (a send to part of the audience, time-zone or throttled sending, an A/B test) didn't map exactly **needs review**: the audience is never made wider. Push and WhatsApp campaigns are listed, not imported.
7. **Flows**, always as drafts: the trigger, delays, emails, texts, yes/no splits, profile updates, list updates and webhooks. Anything else (push, WhatsApp, multi-path splits, A/B variations, custom code) is skipped and listed, and the flow's name ends in "(partial import)". A flow **needs review** if a split's rules didn't translate, a list step might add or remove (Klaviyo's API doesn't say which), a message step was off or manual in Klaviyo, or an email needed guessing. Flows whose trigger has no DumbMail equivalent (price drop, low inventory) are listed for rebuilding.
8. **Forms**: each form's name and the list its sign-up button adds people to (in the live version), as a draft form. A form that signs people up to more than one list, or to one that wasn't imported, comes across without a list for you to choose. Klaviyo doesn't share form designs, so rebuild each design in **Sign-up forms**, publish it, and replace Klaviyo's script on your site with DumbMail's snippet.
9. **Events**: the chosen metrics (by default orders, checkouts, product views, carts and email/SMS engagement) with their original times, values and properties, skipping any already imported. Past emails and texts are tied to the imported campaigns and flows, so segments like "received campaign X" keep working. Imported events never start flows, aren't counted as new activity and aren't billed.
10. **Compare**: Klaviyo's own counts for the report (list and segment sizes, events and revenue per month).

### Needs review

Anything the importer couldn't translate with certainty is marked **needs review**, with notes saying why: a campaign can't send and a flow can't be turned on until a person clears it. The list is on the import page (MCP: `reviews` in `klaviyo_import_status`). Fix what the notes say, then press **Mark reviewed** (MCP: `klaviyo_clear_review` with `kind` and `id`).

Catalogs and coupons are counted in the survey but not imported: DumbMail doesn't store them yet.

### Consent, exactly

Consent belongs to an email address or a phone number, not to a profile. Klaviyo's email consent is applied only when the DumbMail profile has the same email address (or none yet), and its SMS consent only for the same phone number. If the profile matched has a different address or number, that channel is left alone and the report lists the clash with both values (partly hidden) for you to resolve.

A new profile gets Klaviyo's state:

| In Klaviyo | In DumbMail |
| --- | --- |
| Email subscribed | subscribed |
| Email never subscribed | never subscribed (held back too, if you chose that) |
| Email consent missing or unrecognised | never subscribed, held back from marketing |
| Email unsubscribed, or an unsubscribe suppression | unsubscribed |
| Hard bounce | suppressed (`hard_bounce`): never emailed |
| Spam complaint | unsubscribed and suppressed (`complaint`) |
| Invalid email | suppressed (`invalid_email`) |
| Suppressed by hand | suppressed (`manual`) |
| Klaviyo can't email them for another reason | suppressed (`klaviyo_suppressed`) |
| SMS subscribed and textable, with a valid number | subscribed |
| SMS subscribed but Klaviyo can't text them (landline, carrier) | never subscribed: not textable |
| SMS unsubscribed | unsubscribed |
| SMS never subscribed or missing | never subscribed |

When the profile is already in DumbMail (same address or number), the merge happens in one step that reads the profile as it is at that moment, so a STOP or unsubscribe that arrives during the import is never overwritten:

- An opt-out in Klaviyo always applies.
- An opt-in in Klaviyo applies only when DumbMail has "never subscribed" and no suppression. It never lifts an unsubscribe, a suppression or an unconfirmed (pending) sign-up.
- Someone who subscribed through DumbMail directly keeps that consent when Klaviyo has no record or says "never subscribed": they gave it to you through DumbMail, so the import isn't making them easier to reach. The report counts them.
- A Klaviyo suppression is added; one already in DumbMail stays.

## 4. Read the comparison report

When the import finishes, the report (MCP: `klaviyo_comparison_report`; download as CSV) lists, each with Klaviyo's number, DumbMail's number and green or red:

Each line is **match**, **mismatch** or **not verified**. A line matches only when Klaviyo's number comes from Klaviyo itself (its own count, or a scan that read every record) and agrees with DumbMail's data within the stated tolerance. Anything Klaviyo wouldn't count, a scan that didn't finish, or a list beyond the first 500 is "not verified", with the reason. "Nothing was lost" shows only when every line matches.

- profiles: Klaviyo's total from a full scan, with any combined because they shared an email or phone, any Klaviyo couldn't give an email, phone or id for, and email or phone clashes to resolve;
- email, person by person: unsubscribed, suppressed, subscribed, never subscribed and unknown in Klaviyo against the same people here, "more reachable here than in Klaviyo" (must be 0), and anyone who kept a DumbMail opt-in;
- SMS: unsubscribed, subscribed and textable, and "more textable here than in Klaviyo" (must be 0);
- list opt-outs: everyone who unsubscribed from a list in Klaviyo is off it here;
- every list's size, and every segment's size (within 2% or 5 people, since Klaviyo's counts are taken after the scan and both sides keep changing);
- events per metric over the imported range ("everything" starts when Klaviyo created the metric) and revenue per month (in your account's time zone), within 0.1%.

Below it, everything that didn't come across exactly: snapshot segments and why, removed template tags, skipped flow steps and profiles, identity clashes (MCP: `klaviyo_import_issues`). Running the import again (connect the key again) only adds what's new.

## 5. Run it again before you switch

You can run the import again at any time: connect the key again and start. It picks up everything that changed in Klaviyo since the last run (new people, consent changes, unsubscribes and suppressions, list joins and leaves, new events) and duplicates nothing.

**Don't send from both systems at once.** When you're ready to switch:

1. Stop sending from Klaviyo (pause its flows, unschedule its campaigns).
2. Run the import again, so the latest unsubscribes and suppressions come across.
3. Check the comparison report.
4. Then send from DumbMail.

People can still unsubscribe through links in emails Klaviyo sent earlier, so if you wait a while before sending, run it again first. The checklist's "Unsubscribes up to date" item is green only after a run that finished in the last 24 hours and began after the last Klaviyo send it found; otherwise it says how old the last import is.

## 6. Switch over

The checklist on the page (MCP: `klaviyo_checklist`) checks most items from your live data:

- your sending domain is verified (Settings → Domains);
- DumbMail's snippet is on your site, and Klaviyo's is gone (**Check my site** looks at your home page);
- imported flows are reviewed and turned on;
- sign-up forms are live;
- email and SMS opt-outs, suppressions and list opt-outs came across (from the report);
- a test send went through;
- you have a warm-up plan: a new sending domain starts with a small daily limit that grows while people open and don't complain (Settings → Sending). Send to your most engaged people first.

## MCP tools

| Tool | What it does |
| --- | --- |
| `klaviyo_survey` | With `api_key`: check the key and count what's in Klaviyo. Without: the current survey. |
| `klaviyo_start_import` | Start the import (`confirm: true`; options `what`, `events_from`, `metrics`, `never_subscribed`). Dry run without confirm. |
| `klaviyo_import_status` | Status, progress per step, issue counts, items needing review, and the checklist. |
| `klaviyo_pause_import` | Pause, or resume with `resume: true`. |
| `klaviyo_cancel_import` | Stop and delete the key. Imported data stays. |
| `klaviyo_comparison_report` | The side-by-side report. |
| `klaviyo_import_issues` | What didn't come across exactly, and why. |
| `klaviyo_clear_review` | Mark an imported campaign, flow or template reviewed (`kind`, `id`). |
| `klaviyo_checklist` | The switch-over checklist; tick hand-checked items or check the site. |

REST: `GET /api/klaviyo`, `POST /api/klaviyo/connect`, `/start`, `/pause`, `/resume`, `/cancel`, `/review`, `GET /api/klaviyo/report` (and `/report.csv`), `/issues`, `/campaigns`, `POST /api/klaviyo/checklist`, `/checklist/check-site`.
