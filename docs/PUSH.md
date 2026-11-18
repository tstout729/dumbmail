# Push notifications

DumbMail sends push notifications as a third channel, next to email and texts:

- **Web push** shows notifications in Chrome, Edge, Firefox and Safari (macOS, and iOS 16.4+ when the site is added to the Home Screen). It works with the website snippet you already have, plus one file on your site.
- **Mobile push** goes to your own iOS and Android apps, through your own Apple (APNs) and Firebase (FCM) accounts.

Push campaigns and flow push steps work like email and SMS ones: audiences, A/B tests, send times, smart sending, tests and reports.

## Who gets push

A person can get push when:

1. a device of theirs allowed notifications (the browser's or the phone's own prompt), **and**
2. that device is linked to them (see "Linking devices to people"), **and**
3. they haven't turned push off (`set_push_opt_out`, or `PUT /api/push/profiles/{id}/opt-out`).

Taking back permission in the browser or phone stops push on that device: the push service tells DumbMail the device is gone and it's removed. Turning push off for a person stops it on every device, also for messages already queued: every hand-off checks it right before the push service call.

Segments: `{"type": "can_receive", "channel": "push"}` matches people a push would reach now. Push has no consent status, so `consent` conditions don't take `push`.

Quiet hours: push follows the same hours as texts (Settings → Send times), in each person's time zone. A profile's time zone can only narrow them, together with the account's. A push outside the hours waits until they open.

## Web push setup

Browsers only show notifications from a **service worker on your site's own domain**. That's a small JavaScript file at your site's root. DumbMail can't host it for you on your domain, so you put it there once:

1. In **Settings → Push**, download the service worker file (`GET /push/sw.js?k=YOUR_PUBLIC_KEY`). It already has your site key in it.
2. Put it on your site so it opens at `https://yoursite.com/dumbmail-sw.js` (or the path you set in `sw_path`):
   - **Any site on your own server**: copy the file into the folder your site's root is served from.
   - **Next.js, Vite, Create React App, Astro, Gatsby**: put it in the `public/` folder and deploy.
   - **WordPress**: upload it with your host's file manager or SFTP into the WordPress folder (the one with `wp-config.php`).
   - **Shopify, Wix, Squarespace, Webflow, Framer**: these builders don't let you put a file at your site's root, so web push can't be added there. Email, texts and mobile push still work.
3. Turn web push on. The website snippet then loads the push add-on on every page.

Check it works with **Send me a test** in Settings → Push: it adds the browser you're using to your own test devices (never part of an audience) and sends it a notification.

### Asking for permission

The snippet never asks for permission when a page loads. It shows a small card with your wording first (`prompt`), and only its **Allow** button opens the browser's prompt. A browser that said no is never asked again. Rules (`prompt`):

| Field | What it does | Default |
| --- | --- | --- |
| `enabled` | Show the card automatically. `false`: only your own button asks. | `true` |
| `title`, `body`, `allow_text`, `deny_text` | The card's words. | |
| `page_views` | Pages seen in this visit before it shows. | 2 |
| `delay_seconds` | Seconds on the page first. | 5 |
| `url_contains`, `url_excludes` | Only on, or never on, pages whose address contains these. | |
| `frequency_days` | After "Not now", wait this long. | 7 |
| `position` | `top` or `bottom`. | `bottom` |

Your own button can ask too (it must run from a click):

```html
<button onclick="dumbmail.notifications.prompt()">Get notified</button>
```

`dumbmail.notifications` also has `status()` (`granted`, `denied`, `default` or `unsupported`), `unsubscribe()` and `identify()` (below).

## Linking devices to people

A device is linked to a person only with an identity DumbMail can trust. A browser or app holding just your public site key can't name someone, because anyone can copy that key:

- **From a tracked link**: someone who opens a link in one of your emails, texts or push notifications is recognized by the snippet, and their browser is linked when it subscribes. This needs nothing from you.
- **Verified identity**: your own server signs who is signed in. Make an identity secret in Settings → Push (`rotate_push_identity_secret`; shown once). For each signed-in customer, compute `user_hash = hex(HMAC-SHA256(identity_secret, external_id))` on your server (or use their lowercased email instead of `external_id`) and pass both to the page or app. A verified identity creates the profile when it doesn't exist yet. It never changes email or SMS consent.

```js
// On your site, after sign-in (user_hash computed on your server):
dumbmail.notifications.identify({ external_id: "cust_123", user_hash: "9f2c…" });
```

```js
// Node.js, on your server:
const userHash = require("crypto").createHmac("sha256", process.env.DUMBMAIL_PUSH_IDENTITY_SECRET).update(customer.id).digest("hex");
```

Until one of these arrives, a device is kept but not linked, and no campaign reaches it. Settings → Push shows how many are waiting.

## Mobile push setup

DumbMail doesn't ship a native SDK. Your app registers its device token with one HTTP call and reports opens with another. The code below is the whole integration.

### Credentials

- **Apple**: in your Apple developer account, create an APNs auth key (Certificates, Identifiers & Profiles → Keys, with Apple Push Notifications service). Enter its key ID, your team ID, your app's bundle ID and the `.p8` file. Turn **production** off for development builds (they use the APNs sandbox).
- **Android**: in Firebase, Project settings → Service accounts → Generate new private key. Paste the whole JSON file. DumbMail uses FCM HTTP v1 and keeps the OAuth token cached.

Secrets are stored encrypted and never shown again. A credential problem Apple or Google reports shows in Settings → Push as a short reason.

### Registering a device

`POST {base}/client/push/devices?k=YOUR_PUBLIC_KEY` with JSON:

```json
{"platform": "ios", "token": "<APNs device token as hex, or FCM registration token>", "app_version": "2.4.0",
 "identity": {"external_id": "cust_123", "user_hash": "…"}}
```

Call it on every launch and after sign-in (`identity` is optional before sign-in). On sign-out, `POST {base}/client/push/devices/remove?k=…` with `{"platform", "token"}`.

When someone opens a notification, read `dumbmail.t` (iOS) or `dumbmail_t` (Android data) from it and `POST {base}/client/push/opened?k=…` with `{"t": "<that value>"}`. The tap's link is `dumbmail.url` / `dumbmail_url`: opening it records the tap too.

**Swift (iOS)**

```swift
// AppDelegate: after UNUserNotificationCenter.current().requestAuthorization(...) is granted,
// call UIApplication.shared.registerForRemoteNotifications().
func application(_ app: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
    let token = deviceToken.map { String(format: "%02x", $0) }.joined()
    var body: [String: Any] = ["platform": "ios", "token": token]
    if let id = Session.customerId, let hash = Session.pushUserHash { body["identity"] = ["external_id": id, "user_hash": hash] }
    post("/client/push/devices", body)
}

// UNUserNotificationCenterDelegate
func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse, withCompletionHandler done: @escaping () -> Void) {
    let info = response.notification.request.content.userInfo["dumbmail"] as? [String: Any]
    if let t = info?["t"] as? String { post("/client/push/opened", ["t": t]) }
    if let link = info?["url"] as? String, let url = URL(string: link) { UIApplication.shared.open(url) }
    done()
}

func post(_ path: String, _ body: [String: Any]) {
    var req = URLRequest(url: URL(string: "https://dumbmail.ai\(path)?k=YOUR_PUBLIC_KEY")!)
    req.httpMethod = "POST"
    req.setValue("application/json", forHTTPHeaderField: "Content-Type")
    req.httpBody = try? JSONSerialization.data(withJSONObject: body)
    URLSession.shared.dataTask(with: req).resume()
}
```

**Kotlin (Android, Firebase Messaging)**

```kotlin
class PushService : FirebaseMessagingService() {
    override fun onNewToken(token: String) = register(token)

    fun register(token: String) {
        val body = JSONObject().put("platform", "android").put("token", token)
        Session.customerId?.let { body.put("identity", JSONObject().put("external_id", it).put("user_hash", Session.pushUserHash)) }
        post("/client/push/devices", body)
    }
}

// In the activity the notification opens (FCM puts the data keys in the intent's extras):
intent.extras?.getString("dumbmail_t")?.let { post("/client/push/opened", JSONObject().put("t", it)) }

fun post(path: String, body: JSONObject) = thread {
    val c = URL("https://dumbmail.ai$path?k=YOUR_PUBLIC_KEY").openConnection() as HttpURLConnection
    c.requestMethod = "POST"; c.doOutput = true
    c.setRequestProperty("Content-Type", "application/json")
    c.outputStream.use { it.write(body.toString().toByteArray()) }
    c.responseCode
}
```

**React Native (`@react-native-firebase/messaging`)**

```js
import messaging from "@react-native-firebase/messaging";
import { Platform } from "react-native";

const post = (path, body) => fetch(`https://dumbmail.ai${path}?k=YOUR_PUBLIC_KEY`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });

export async function registerPush(identity) {
  await messaging().requestPermission();
  // iOS sends to APNs directly, so register the APNs token there; Android uses the FCM token.
  const token = Platform.OS === "ios" ? await messaging().getAPNSToken() : await messaging().getToken();
  if (token) await post("/client/push/devices", { platform: Platform.OS, token, identity });
}

messaging().onNotificationOpenedApp((m) => {
  const t = m.data?.dumbmail_t ?? m.data?.dumbmail?.t;
  if (t) post("/client/push/opened", { t });
});
```

## Content

A push campaign's `content` (and a flow `push` step's) is:

| Field | What it does |
| --- | --- |
| `title` (up to 100), `body` (up to 500) | The text. Personalization tags work: `{{ first_name|default('there') }}`, `{{ event.properties.x }}` in flows. |
| `url` | Where a tap goes (https). Default: your website. |
| `icon` | Web only. Default: your logo. |
| `image` | A large picture (Chrome, Edge, Android; iOS apps with a notification service extension). |
| `actions` | Up to two buttons `{title, url}` (Chrome and Edge; apps that register them). |
| `platforms` | `["web", "ios", "android"]`, or empty for all. |
| `ttl_seconds` | How long an offline device's push service keeps it (default 1 day, at most 28 days). |
| `urgency` | `very_low`, `low`, `normal` (default) or `high`. |
| `topic` | A newer push with the same topic replaces an older one that wasn't delivered yet. |
| `ios_sound`, `ios_badge`, `android_channel_id` | Per platform. |

Coupon codes aren't supported in push: a code would show on the lock screen and couldn't be kept per person. Content with a coupon tag is refused when it's saved. Send codes by email or text.

Every link is tracked: a tap goes through DumbMail (recording the click) and on to the link. Links to your own site recognize the person there, as email links do.

## Sending

- Each person gets one message, sent to up to 10 of their most recent devices.
- The person's message and its devices are fixed when it's claimed. A second check runs right before each device's push service is called: the device is still active and the person hasn't opted out.
- A device the push service says is gone (web push 404 or 410, APNs `Unregistered` or `BadDeviceToken`, FCM `UNREGISTERED`) is removed. A device that keeps failing for other reasons is turned off after 5 failures in a row.
- When a push service is busy or down (HTTP 429 or 5xx), that device is retried in the background after 1, 5 and 30 minutes (or the wait the service asks for), then given up.
- A delivery is handed over at most once. If DumbMail stops mid-send, it's marked failed, never sent twice.

## Reports

`get_push_report` (`GET /api/push/report?campaign_id=…` or `flow_id=…`) shows people sent, clicked, failed and skipped, and per platform: devices sent, delivered, clicked, failed, removed and retrying. **Delivered** is known for web push only (the browser reports showing it). Apple and Google don't tell senders, so mobile shows sent and clicked. Attribution counts a push click like a text click (Settings → Attribution's SMS window).

Events: `Received Push` and `Clicked Push` (flows, segments and webhooks use them; browsers can't send them).

## Billing

On the hosted service push is free unless the operator sets `PRICE_PUSH`. A non-zero price is charged per person reached (all their devices), from the balance, like texts.

## API

| | |
| --- | --- |
| `GET /api/push/settings` | Settings, device counts, the service worker URL and the device registration URL. |
| `PATCH /api/push/settings` | `web_enabled`, `sw_path`, `prompt`, `apns` (`{key_id, team_id, bundle_id, production, key_p8}` or `null`), `fcm` (`{service_account_json}` or `null`). |
| `POST /api/push/identity-secret` | A new identity secret (returned once). |
| `POST /api/push/test-devices` | Add this browser's subscription as your own test device. |
| `POST /api/push/test` | `{content?, to?: "me" or an email, profile_id?, device_id?}`. Sending to anyone but your own test devices needs permission to send campaigns. |
| `GET /api/push/profiles/{id}` | A person's devices and whether they opted out. |
| `PUT /api/push/profiles/{id}/opt-out` | `{opted_out: true|false}`. |
| `GET /api/push/report` | See Reports. |

Campaigns take `channel: "push"`; flows take `{"type": "push", "content": {...}, "next": …}` steps. MCP tools: `get_push_settings`, `update_push_settings`, `rotate_push_identity_secret`, `send_test_push`, `get_profile_push`, `set_push_opt_out`, `get_push_report`.

## Limits

- 30 device registrations an hour from one address, per account. 10 linked devices per person (registering an 11th turns off their least recent). 200,000 unlinked devices per account; unlinked devices unused for 180 days are removed.
- 50 test sends an hour per account.
- Web push only goes to the browsers' own push services (Google, Mozilla, Microsoft and Apple).
