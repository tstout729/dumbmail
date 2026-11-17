# Moving from Klaviyo

DumbMail can bring your whole Klaviyo account across in one go: profiles with their exact consent, lists, segments, event history, templates, campaigns, flows and forms. When it's done, a report puts Klaviyo's numbers next to DumbMail's so you can see nothing was lost.

Nobody is emailed or texted during the import, no flow starts, and nobody becomes easier to reach than they were in Klaviyo.

In the dashboard: **Settings → Move from Klaviyo**. With an AI assistant: the `klaviyo_*` MCP tools below.

## 1. Make a read-only Klaviyo key

In Klaviyo, go to **Settings → Account → API keys → Create Private API Key**. Name it "DumbMail", choose **Custom key**, and give it **Read access** to:

Accounts, Campaigns, Catalogs, Coupons, Events, Flows, Forms, Lists, Metrics, Profiles, Segments and Templates.

Read access is all DumbMail needs. It never changes anything in Klaviyo. Copy the key (it starts with `pk_`).

The key is stored encrypted only while the import runs, and while the ongoing sync (step 5) is on. It is deleted when the import finishes, when you cancel, or when you turn the sync off. DumbMail shows only its last four characters.

## 2. Connect and look before importing

Paste the key and press **Connect** (MCP: `klaviyo_survey` with `api_key`). DumbMail checks the key, then counts what's in the account: profiles, lists, segments, metrics, an estimate of events, templates, campaigns, flows, forms, catalog items, coupons and suppressions, plus how long the import should take. Large counts are shown as "more than"; the import counts them exactly.

If the key is missing a read scope, the survey says which. Profiles are required; without other scopes those parts are left out.

## 3. Start the import

Choose what to bring (profiles and consent always come) and how far back to import events (two years by default, because event history can be huge), then **Start import** (MCP: `klaviyo_start_import` with `confirm: true`; without it you get a dry run).

The import runs in the background in this order. You can leave the page, pause and resume, or cancel. If the server restarts, it carries on from the last page it saved.

1. **Metrics**: the list of Klaviyo metrics.
2. **Profiles**: email, phone, external id, names, every custom property, location (country names become ISO codes like `US`), and the Klaviyo id (`klaviyo_id` property). Each profile keeps Klaviyo's consent record (status, time, method) in its `klaviyo_consent` property.
3. **Lists** and their members, with each list's opt-in setting. Members join quietly: list-triggered flows don't start. People who unsubscribed from a list in Klaviyo aren't added back to it.
4. **Segments**. Rules that mean the same thing in DumbMail become live segments. If any rule can't be expressed (predictive analytics, region, distance, consent filtered by method, event rules that reach back further than the imported history, and so on), the segment comes across as a list of its current members, named "… (Klaviyo segment snapshot, date)", and the report says why. Meaning is never changed silently.
5. **Templates**: the HTML is kept (the original too, under `imported.original_html`). Klaviyo tags become DumbMail tags: `{{ person.first_name|default:"there" }}` → `{{ person.first_name|default('there') }}`, `{% unsubscribe %}` → an unsubscribe link, `{{ person|lookup:'Color' }}` → `{{ person.properties['Color'] }}`, `{{ event.Items }}` → `{{ event.properties.Items }}`, `organization.full_address` → `organization.address`. Tags DumbMail doesn't have (`{% web_view %}`, coupon codes, catalog lookups, some filters) are removed and listed, so a template never breaks at send time.
6. **Campaigns**: sent campaigns come across as history (name, subject, send time, audience names and Klaviyo's headline numbers: recipients, delivered, unique opens and clicks, unsubscribes, bounces, spam complaints, orders and revenue). Drafts stay drafts. A campaign scheduled in Klaviyo comes across as a draft so it can't go out twice. Push and WhatsApp campaigns are listed, not imported.
7. **Flows**, always as drafts: the trigger, delays, emails, texts, yes/no splits, profile updates, list updates and webhooks. Anything else (push, WhatsApp, multi-path splits, A/B variations, custom code) is skipped and listed, and the flow's name ends in "(partial import)". A split whose rules don't translate sends everyone down "Yes" until you set its rules. Flows whose trigger has no DumbMail equivalent (price drop, low inventory) are listed for rebuilding.
8. **Forms**: each form's name and the list it signs people up to, as a draft form. Klaviyo doesn't share form designs, so rebuild each design in **Sign-up forms**, publish it, and replace Klaviyo's script on your site with DumbMail's snippet.
9. **Events**: the chosen metrics (by default orders, checkouts, product views, carts and email/SMS engagement) with their original times, values and properties, skipping any already imported. Past emails and texts are tied to the imported campaigns and flows, so segments like "received campaign X" keep working. Imported events never start flows, aren't counted as new activity and aren't billed.
10. **Compare**: Klaviyo's own counts for the report (list and segment sizes, events and revenue per month).

Catalogs and coupons are counted in the survey but not imported: DumbMail doesn't store them yet.

### Consent, exactly

| In Klaviyo | In DumbMail |
| --- | --- |
| Email subscribed | subscribed |
| Email never subscribed | never subscribed (stays never subscribed) |
| Email unsubscribed | unsubscribed |
| Hard bounce | suppressed (`hard_bounce`): never emailed |
| Spam complaint | unsubscribed and suppressed (`complaint`) |
| Invalid email | suppressed (`invalid_email`) |
| Suppressed by hand | suppressed (`manual`) |
| Klaviyo can't email them for another reason | suppressed (`klaviyo_suppressed`) |
| SMS subscribed and textable | subscribed |
| SMS subscribed but Klaviyo can't text them (landline, carrier) | unsubscribed |
| SMS unsubscribed | unsubscribed |
| SMS never subscribed | never subscribed |

When a profile already exists in DumbMail, an unsubscribe or suppression on either side wins, and a real opt-in on either side beats "never subscribed". A DumbMail unsubscribe is never undone by Klaviyo data; someone who resubscribes in Klaviyo can sign up again through a DumbMail form.

## 4. Read the comparison report

When the import finishes, the report (MCP: `klaviyo_comparison_report`; download as CSV) lists, each with Klaviyo's number, DumbMail's number and green or red:

- profiles (with any that were combined because they shared an email or phone, and any Klaviyo couldn't give an email, phone or id for);
- email and SMS: subscribed, unsubscribed, never subscribed, suppressed, reachable, and "more reachable here than in Klaviyo", which must be 0;
- every list's size;
- every segment's size (live segments match within 2% or 5 people, since both sides keep changing; snapshots match their size on import day);
- events per metric over the imported range;
- revenue per month (in your account's time zone).

Below it, everything that didn't come across exactly: snapshot segments and why, removed template tags, skipped flow steps and profiles (MCP: `klaviyo_import_issues`). Running the import again (connect the key again) only adds what's new.

## 5. Run both for a while (optional)

Turn on **Keep syncing from Klaviyo** (MCP: `klaviyo_set_sync`). Every hour DumbMail pulls new and changed profiles (with their consent), new list members and new events. When someone unsubscribes, complains or bounces in Klaviyo, their profile is re-read in the same pass.

Worst case, a Klaviyo unsubscribe reaches DumbMail within about an hour (plus the time one sync takes). A sync also runs within 30 minutes before any scheduled campaign. Before sending anything by hand, press **Sync now** (MCP: `klaviyo_sync_now`).

While both run, an order your site reports to both Klaviyo and DumbMail is counted in DumbMail twice (once from its snippet, once from the sync). Turn the sync off once DumbMail's snippet is on your site. Turning the sync off deletes the stored key.

## 6. Switch over

The checklist on the page (MCP: `klaviyo_checklist`) checks most items from your live data:

- your sending domain is verified (Settings → Domains);
- DumbMail's snippet is on your site, and Klaviyo's is gone (**Check my site** looks at your home page);
- imported flows are reviewed and turned on;
- sign-up forms are live;
- unsubscribes and suppressions came across (from the report);
- a test send went through;
- you have a warm-up plan: a new sending domain starts with a small daily limit that grows while people open and don't complain (Settings → Sending). Send to your most engaged people first.

## MCP tools

| Tool | What it does |
| --- | --- |
| `klaviyo_survey` | With `api_key`: check the key and count what's in Klaviyo. Without: the current survey. |
| `klaviyo_start_import` | Start the import (`confirm: true`; options `what`, `events_from`, `metrics`, `sync`). Dry run without confirm. |
| `klaviyo_import_status` | Status, progress per step, issue counts, sync and checklist. |
| `klaviyo_pause_import` | Pause, or resume with `resume: true`. |
| `klaviyo_cancel_import` | Stop and delete the key. Imported data stays. |
| `klaviyo_comparison_report` | The side-by-side report. |
| `klaviyo_import_issues` | What didn't come across exactly, and why. |
| `klaviyo_set_sync` | Ongoing hourly sync on or off. |
| `klaviyo_sync_now` | Sync right away. |
| `klaviyo_checklist` | The switch-over checklist; tick hand-checked items or check the site. |

REST: `GET /api/klaviyo`, `POST /api/klaviyo/connect`, `/start`, `/pause`, `/resume`, `/cancel`, `/sync`, `/sync-now`, `GET /api/klaviyo/report` (and `/report.csv`), `/issues`, `/campaigns`, `POST /api/klaviyo/checklist`, `/checklist/check-site`.
