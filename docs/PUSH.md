# Push notifications

DumbMail sends push notifications as a third channel, next to email and texts:

- **Web push** shows notifications in Chrome, Edge, Firefox and Safari (macOS, and iOS 16.4+ when the site is added to the Home Screen). It works with the website snippet you already have, plus one file on your site.
- **Mobile push** goes to your own iOS and Android apps, through your own Apple (APNs) and Firebase (FCM) accounts.

Push campaigns and flow push steps work like email and SMS ones: audiences, A/B tests, send times, smart sending, tests and reports.

## Who gets push

A push goes to a device when:

1. the device allowed notifications (the browser's or the phone's own prompt; for apps, the permission the app reports: granted, or provisional on iOS), **and**
2. its platform is set up and on in Settings → Push, **and**
3. the person it belongs to hasn't turned push off (`set_push_opt_out`, or `PUT /api/push/profiles/{id}/opt-out`).

Every device belongs to a person. A device whose site or app said who's signed in (see "Linking devices to people") belongs to that person; any other device gets an anonymous profile of its own. Anonymous profiles can be messaged by segments like any other (`{"type": "can_receive", "channel": "push"}`), and they never take over someone else's profile.

All of these are checked again right before every hand-off to a push service, retries included: the device still belongs to the same person and registration it did when the message was made, it still allows notifications, its platform is still on, the person hasn't opted out, the campaign or flow is still live, it isn't their quiet hours, and smart sending (not counting this message) still allows it. Anything that no longer passes is skipped with the reason.

Taking back permission in the browser or phone stops push there: the browser or app tells DumbMail, or the push service says the device is gone and it's removed.

Quiet hours: push follows the same hours as texts (Settings → Send times), in each person's time zone, and the send-time preview shows that. A profile's time zone can only narrow them, together with the account's.

The marketing hold (people imported without a marketing opt-in) is about email. It doesn't stop push: a push only reaches a device whose owner allowed notifications there.

## Web push setup

Browsers only show notifications from a **service worker on your site's own domain**. DumbMail can't put a file on your domain, so you do it once:

1. In **Settings → Push**, download the service worker (`GET /push/sw.js?k=YOUR_SITE_KEY`). It already has your site key in it.
2. Put it on your site so it opens at `https://yoursite.com/dumbmail-sw.js` (or the path you set as `sw_path`):
   - **Any site on your own server**: copy the file into the folder your site's root is served from.
   - **Next.js, Vite, Create React App, Astro, Gatsby**: put it in the `public/` folder and deploy.
   - **WordPress**: upload it with your host's file manager or SFTP into the WordPress folder (the one with `wp-config.php`).
   - **Shopify, Wix, Squarespace, Webflow, Framer**: these builders don't let you put a file at your site's root, so web push can't be added there. Email, texts and app push still work.
3. Turn web push on. The website snippet then loads the push add-on on every page.

The snippet registers the worker with DumbMail's own scope, `/dumbmail-push/`, and only ever looks up that registration. It controls none of your pages, so it never replaces or touches a service worker your site already has, and it only handles notifications DumbMail sent and only opens DumbMail's signed tap links. Sharing one worker with your site's own push notifications isn't supported yet.

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

`dumbmail.notifications` also has `status()` (`granted`, `denied`, `default` or `unsupported`), `unsubscribe()`, `identify()` and `signOut()` (below). After `unsubscribe()` the browser stays off until the person clicks Allow again; it's never subscribed again on a later page by itself. If the removal can't reach DumbMail (offline), the snippet keeps proof of it and sends it again on the next page until DumbMail confirms.

## Linking devices to people

A device is linked to a person only with an **enrollment grant** that your own server makes for the person signed in. Anyone can copy your site key, so a browser or app can't just say who it belongs to, and a link from one of your emails (which can be forwarded) doesn't link a device either.

1. Make an enrollment secret in Settings → Push (`rotate_push_identity_secret`; shown once). Keep it on your server only.
2. For a signed-in customer, your server makes a grant valid for at most 24 hours (DumbMail allows five minutes of clock difference on top, so `expires` may be up to 24 hours and 5 minutes past DumbMail's clock):

   ```
   grant = "v1." + expires + "." + hex(HMAC-SHA256(secret, "v1|push-enroll|" + ACCOUNT_ID + "|" + type + "|" + value + "|" + expires))
   ```

   `ACCOUNT_ID` is your DumbMail account id (shown in Settings → Push, and as `account_id` in `get_push_settings`). `type` is `external_id` (your customer id) or `email`. Sign `value` exactly as DumbMail reads it: an `external_id` with leading and trailing spaces removed (case kept), an `email` with leading and trailing spaces removed and then lowercased (`" Ana@Shop.TEST "` is signed as `ana@shop.test`). Send the same `value` you signed. `expires` is a Unix time in seconds, at most a day ahead. A grant for an email never names a profile whose external id is the same text.
3. Pass `{type, value, grant}` to the page or app. The snippet keeps it in memory only, never in the browser's storage; pass it again on each page.

```js
// On your site, after sign-in (grant made on your server):
dumbmail.notifications.identify({ type: "external_id", value: "cust_123", grant: "v1.1795000000.9f2c…" });
// On sign-out: this browser goes back to an anonymous profile of its own. If DumbMail can't be
// reached, the browser drops its subscription at once and the snippet sends the unlink again on
// later pages until DumbMail confirms it.
dumbmail.notifications.signOut();
```

**Node.js**

```js
const crypto = require("crypto");
function pushGrant(type, value) {
  const expires = Math.floor(Date.now() / 1000) + 3600;
  const mac = crypto.createHmac("sha256", process.env.DUMBMAIL_PUSH_SECRET)
    .update(`v1|push-enroll|${process.env.DUMBMAIL_ACCOUNT_ID}|${type}|${value}|${expires}`).digest("hex");
  return { type, value, grant: `v1.${expires}.${mac}` };
}
```

**Python**

```python
import hashlib, hmac, os, time
def push_grant(type_, value):
    expires = int(time.time()) + 3600
    msg = f"v1|push-enroll|{os.environ['DUMBMAIL_ACCOUNT_ID']}|{type_}|{value}|{expires}"
    mac = hmac.new(os.environ["DUMBMAIL_PUSH_SECRET"].encode(), msg.encode(), hashlib.sha256).hexdigest()
    return {"type": type_, "value": value, "grant": f"v1.{expires}.{mac}"}
```

**PHP**

```php
function push_grant(string $type, string $value): array {
    $expires = time() + 3600;
    $msg = "v1|push-enroll|" . getenv("DUMBMAIL_ACCOUNT_ID") . "|$type|$value|$expires";
    $mac = hash_hmac("sha256", $msg, getenv("DUMBMAIL_PUSH_SECRET"));
    return ["type" => $type, "value" => $value, "grant" => "v1.$expires.$mac"];
}
```

**Ruby**

```ruby
require "openssl"
def push_grant(type, value)
  expires = Time.now.to_i + 3600
  msg = "v1|push-enroll|#{ENV['DUMBMAIL_ACCOUNT_ID']}|#{type}|#{value}|#{expires}"
  mac = OpenSSL::HMAC.hexdigest("SHA256", ENV["DUMBMAIL_PUSH_SECRET"], msg)
  { type: type, value: value, grant: "v1.#{expires}.#{mac}" }
end
```

A verified grant creates the profile when the account has nobody with that identifier yet. It never changes email or SMS consent. When a device that belonged to someone else is enrolled for a new person (a shared computer), anything still waiting for the old owner on that device is dropped.

## Mobile push setup

DumbMail doesn't ship a native SDK. Your app asks for permission, registers its device token (with the permission it got) in one HTTP call, and reports opens in another. The code below is the whole integration.

### Credentials

- **Apple**: in your Apple developer account, create an APNs auth key (Certificates, Identifiers & Profiles → Keys, with Apple Push Notifications service). Enter its key ID, your team ID, your app's bundle ID and the `.p8` file. **Production** sets the APNs environment for devices whose app doesn't say which one it uses (see `environment` below); turn it off if all your builds are development builds.
- **Android**: in Firebase, Project settings → Service accounts → Generate new private key. Paste the whole JSON file. DumbMail uses FCM HTTP v1 and keeps the OAuth token cached.

Secrets are stored encrypted and never shown again. When Apple or Google says the account's setup is wrong (credentials, project, topic, or a sandbox token sent to production), Settings → Push shows the reason and no device is deleted. A device is removed only when Apple or Google says that device is gone (APNs `Unregistered` or `ExpiredToken`; APNs `BadDeviceToken` only for a device whose app said its environment, since otherwise it may just belong to the other one; FCM `UNREGISTERED`).

### Registering a device

`POST {base}/client/push/devices?k=YOUR_SITE_KEY` with JSON:

```json
{"platform": "ios", "token": "<APNs device token as hex, or FCM registration token>", "permission": "granted",
 "environment": "production", "app_version": "2.4.0", "identity": {"type": "external_id", "value": "cust_123", "grant": "v1.…"}}
```

`permission` is required: `granted`, `provisional` (iOS), `denied` or `not_determined`. On iOS, send `environment`: `sandbox` for development builds (run from Xcode) and `production` for TestFlight and App Store builds (for example from a `#if DEBUG` check, or the `aps-environment` entitlement). Without it the account's default is used, and a token Apple calls bad is kept, since it may only belong to the other environment. Only `granted` and `provisional` are sent to; provisional devices are marked as such. Call it on every launch, after asking for permission, after sign-in (with a grant), and whenever the permission changes. On sign-out, `POST {base}/client/push/devices/sign-out?k=…` with `{"platform", "token", "install_secret"}` (the device goes back to an anonymous profile); to forget a device entirely, `POST {base}/client/push/devices/remove?k=…` with the same.

**The install secret.** A device token alone doesn't prove the app holds it (a token can leak), so the first registration of a token answers with `install_secret`: a random secret for this install, shown only that once (DumbMail keeps only a hash of it). Keep it in the Keychain (iOS) or encrypted storage (Android) and send it as `install_secret` on every later call for that device. Without it, DumbMail refuses (409) to sign the device out, remove it, give it a new owner, or refresh it anonymously; the one exception is a registration with a valid grant for the person the device already belongs to, which may refresh it. When the token changes (APNs or FCM gives the app a new one), register the new token with `"previous_token": "<the old one>"` and the install secret: the device keeps its owner and its secret, and the old token is forgotten. A new token registered without `previous_token` is a new device with a new secret.

Devices registered before install secrets existed have none. Their first registration that keeps the owner they already have (the same token, with a grant for the same person, or without a grant for an anonymous device) adopts one and answers with it; until then their owner can't change, and they can't be signed out or removed from the app.

Browsers don't need this: a subscription's own auth secret proves it. A subscribe that brings new keys or a new owner for an endpoint DumbMail already has must carry that endpoint's current auth secret (the subscription's own keys, or `"proof": {"auth": "<the old auth secret>"}` when the keys changed), or come from the service worker's `previous` subscription; otherwise it's refused with 409. The snippet does this itself.

When someone opens a notification, read `dumbmail.t` (iOS) or `dumbmail_t` (Android data) and `POST {base}/client/push/opened?k=…` with `{"t": "<that value>"}`. The tap's link is `dumbmail.url` / `dumbmail_url`: opening it records the tap too.

**Swift (iOS)**

```swift
func registerForPush() {
    UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .badge, .sound]) { _, _ in
        DispatchQueue.main.async { UIApplication.shared.registerForRemoteNotifications() }
    }
}

func application(_ app: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
    Push.token = deviceToken.map { String(format: "%02x", $0) }.joined()
    Push.report()
}

// Call on launch and whenever the app comes to the foreground (the person may have changed it in Settings).
enum Push {
    static var token: String?
    static func report() {
        guard let token else { return }
        UNUserNotificationCenter.current().getNotificationSettings { s in
            let permission: String
            switch s.authorizationStatus {
            case .authorized: permission = "granted"
            case .provisional, .ephemeral: permission = "provisional"
            case .denied: permission = "denied"
            default: permission = "not_determined"
            }
            var body: [String: Any] = ["platform": "ios", "token": token, "permission": permission]
            if let secret = Keychain.get("dumbmail_install_secret") { body["install_secret"] = secret }
            // A new token for this install: say which one it replaces.
            if let old = UserDefaults.standard.string(forKey: "dumbmail_token"), old != token { body["previous_token"] = old }
            #if DEBUG
            body["environment"] = "sandbox"      // builds run from Xcode use the APNs sandbox
            #else
            body["environment"] = "production"   // TestFlight and the App Store
            #endif
            if let grant = Session.pushGrant { body["identity"] = grant }   // {type, value, grant} from your server
            post("/client/push/devices", body) { answer in
                guard let answer, answer["ok"] as? Bool == true else { return }
                // Shown once, at the first registration: keep it (Keychain is your own small wrapper).
                if let secret = answer["install_secret"] as? String { Keychain.set("dumbmail_install_secret", secret) }
                UserDefaults.standard.set(token, forKey: "dumbmail_token")
            }
        }
    }

    // On sign-out (or to forget the device): the install secret proves it's this app.
    static func signOut() {
        guard let token else { return }
        post("/client/push/devices/sign-out", ["platform": "ios", "token": token, "install_secret": Keychain.get("dumbmail_install_secret") ?? ""])
    }
}

// UNUserNotificationCenterDelegate
func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse, withCompletionHandler done: @escaping () -> Void) {
    let info = response.notification.request.content.userInfo["dumbmail"] as? [String: Any]
    if let t = info?["t"] as? String { post("/client/push/opened", ["t": t]) }
    if let link = info?["url"] as? String, let url = URL(string: link) { UIApplication.shared.open(url) }
    done()
}

func post(_ path: String, _ body: [String: Any], done: (([String: Any]?) -> Void)? = nil) {
    var req = URLRequest(url: URL(string: "https://dumbmail.ai\(path)?k=YOUR_SITE_KEY")!)
    req.httpMethod = "POST"
    req.setValue("application/json", forHTTPHeaderField: "Content-Type")
    req.httpBody = try? JSONSerialization.data(withJSONObject: body)
    URLSession.shared.dataTask(with: req) { data, _, _ in
        done?(data.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] })
    }.resume()
}
```

**Kotlin (Android, Firebase Messaging)**

```kotlin
// Android 13+: ask first (from an Activity).
val ask = registerForActivityResult(ActivityResultContracts.RequestPermission()) { Push.report(this) }
if (Build.VERSION.SDK_INT >= 33) ask.launch(Manifest.permission.POST_NOTIFICATIONS) else Push.report(this)

object Push {
    // The install secret DumbMail gives at the first registration, and the last token registered.
    // Keep them in encrypted storage (EncryptedSharedPreferences, or your own).
    private fun prefs(context: Context) = Secure.prefs(context)

    // Call after asking, on every start (onResume), and from onNewToken.
    fun report(context: Context) {
        FirebaseMessaging.getInstance().token.addOnSuccessListener { token ->
            val permission = if (NotificationManagerCompat.from(context).areNotificationsEnabled()) "granted" else "denied"
            val body = JSONObject().put("platform", "android").put("token", token).put("permission", permission)
            prefs(context).getString("dumbmail_install_secret", null)?.let { body.put("install_secret", it) }
            prefs(context).getString("dumbmail_token", null)?.takeIf { it != token }?.let { body.put("previous_token", it) }
            Session.pushGrant?.let { body.put("identity", it) }   // {type, value, grant} from your server
            post("/client/push/devices", body) { answer ->
                if (answer?.optBoolean("ok") != true) return@post
                val edit = prefs(context).edit().putString("dumbmail_token", token)
                answer.optString("install_secret").takeIf { it.isNotEmpty() }?.let { edit.putString("dumbmail_install_secret", it) }
                edit.apply()
            }
        }
    }

    // On sign-out (or to forget the device): the install secret proves it's this app.
    fun signOut(context: Context) {
        val p = prefs(context)
        val body = JSONObject().put("platform", "android").put("token", p.getString("dumbmail_token", "")).put("install_secret", p.getString("dumbmail_install_secret", ""))
        post("/client/push/devices/sign-out", body)
    }
}

class PushService : FirebaseMessagingService() {
    override fun onNewToken(token: String) = Push.report(this)
}

// In the activity the notification opens (FCM puts the data keys in the intent's extras):
intent.extras?.getString("dumbmail_t")?.let { post("/client/push/opened", JSONObject().put("t", it)) }

fun post(path: String, body: JSONObject, done: ((JSONObject?) -> Unit)? = null) = thread {
    val c = URL("https://dumbmail.ai$path?k=YOUR_SITE_KEY").openConnection() as HttpURLConnection
    c.requestMethod = "POST"; c.doOutput = true
    c.setRequestProperty("Content-Type", "application/json")
    c.outputStream.use { it.write(body.toString().toByteArray()) }
    val ok = c.responseCode in 200..299
    done?.invoke(if (ok) runCatching { JSONObject(c.inputStream.bufferedReader().readText()) }.getOrNull() else null)
}
```

**React Native (`@react-native-firebase/messaging`)**

```js
import messaging, { AuthorizationStatus } from "@react-native-firebase/messaging";
import { AppState, PermissionsAndroid, Platform } from "react-native";
import * as Keychain from "react-native-keychain"; // or any secure storage

const post = (path, body) => fetch(`https://dumbmail.ai${path}?k=YOUR_SITE_KEY`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });

// The install secret DumbMail gives at the first registration, with the token it was for.
async function saved() {
  const c = await Keychain.getGenericPassword({ service: "dumbmail-push" });
  return c ? { token: c.username, secret: c.password } : { token: null, secret: null };
}

// Android 13 (API 33) and later show notifications only after POST_NOTIFICATIONS is granted, and
// Firebase's requestPermission() doesn't ask for it there. Declare it in AndroidManifest.xml too:
// <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />
async function androidPermission() {
  if (Platform.Version < 33) return "granted";
  const r = await PermissionsAndroid.request(PermissionsAndroid.PERMISSIONS.POST_NOTIFICATIONS);
  return r === PermissionsAndroid.RESULTS.GRANTED ? "granted" : "denied";
}

function permissionOf(status) {
  if (status === AuthorizationStatus.AUTHORIZED) return "granted";
  if (status === AuthorizationStatus.PROVISIONAL || status === AuthorizationStatus.EPHEMERAL) return "provisional";
  if (status === AuthorizationStatus.DENIED) return "denied";
  return "not_determined";
}

// Call after sign-in (identity = {type, value, grant} from your server) and on every start.
export async function registerPush(identity) {
  const permission = Platform.OS === "android" ? await androidPermission() : permissionOf(await messaging().requestPermission());
  await messaging().registerDeviceForRemoteMessages();
  // iOS sends to APNs directly, so register the APNs token there (and say which APNs environment
  // the build uses); Android uses the FCM token.
  const token = Platform.OS === "ios" ? await messaging().getAPNSToken() : await messaging().getToken();
  const environment = Platform.OS === "ios" ? (__DEV__ ? "sandbox" : "production") : undefined;
  if (!token) return;
  const s = await saved();
  const body = { platform: Platform.OS, token, permission, environment, identity, install_secret: s.secret ?? undefined };
  if (s.token && s.token !== token) body.previous_token = s.token;   // a new token for this install
  const r = await post("/client/push/devices", body);
  if (!r.ok) return;
  const answer = await r.json();
  // Shown once, at the first registration: keep it.
  const secret = answer.install_secret ?? s.secret;
  if (secret) await Keychain.setGenericPassword(token, secret, { service: "dumbmail-push" });
}

// On sign-out (or to forget the device): the install secret proves it's this app.
export async function signOutPush() {
  const s = await saved();
  if (s.token) await post("/client/push/devices/sign-out", { platform: Platform.OS, token: s.token, install_secret: s.secret });
}

// Report changes made in the phone's settings when the app comes back.
AppState.addEventListener("change", (s) => { if (s === "active") registerPush(currentGrant()); });
messaging().onTokenRefresh(() => registerPush(currentGrant()));

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
| `platforms` | `["web", "ios", "android"]`, or empty for all. Audience counts include only devices on these. |
| `ttl_seconds` | How long an offline device's push service keeps it (default 1 day, at most 28 days). |
| `urgency` | `very_low`, `low`, `normal` (default) or `high`. |
| `topic` | A newer push with the same topic replaces an older one that wasn't delivered yet. |
| `ios_sound`, `ios_badge`, `android_channel_id` | Per platform. |

Coupon codes aren't supported in push: a code would show on the lock screen and couldn't be kept per person. Content with a coupon tag is refused when it's saved. Send codes by email or text.

Every link is tracked: a tap goes through DumbMail (recording the click) and on to the link. Links to your own site recognize the person there, as email links do. A tap link stops working 35 days after the notification was made.

## Sending

- Each person gets one message, sent to up to 10 of their most recent devices. A flow's push step keeps its message id with the person's run, so a step that runs again never sends a second message.
- Each device is handed over at most once. If a push service doesn't answer after the request went out (a timeout, a dropped connection), that device is **unconfirmed**: it may have arrived, so it's never sent again. If DumbMail stops mid-send, the same happens.
- Only when a push service certainly didn't take it (it refused the connection, or said 429 or 5xx) is that device retried, in the background, after 1, 5 and 30 minutes (or the wait the service asks for), then given up. Until a device accepts it, the message is **retrying**, not sent.
- A device that keeps failing for its own reasons is turned off after 5 failures in a row; turned-off devices are deleted after 30 days.
- The first device that accepts a message marks it sent; that same write charges it and records "Received Push", once.
- Every call to a push service goes through one limiter: at most 8 at once per account and 64 overall per server.

## Reports

`get_push_report` (`GET /api/push/report?campaign_id=…` or `flow_id=…`) shows people sent, clicked, failed, skipped, retrying and unconfirmed, and per platform: devices sent, delivered, clicked, failed, removed, retrying and unconfirmed. **Delivered** is known for web push only (the browser reports showing it; a report that arrives before the push service's answer is saved still counts). Apple and Google don't tell senders, so mobile shows sent and clicked. A person who taps on two devices counts as one click. Attribution counts a push click like a text click (Settings → Attribution's SMS window) and shows push revenue on its own line.

Events: `Received Push` and `Clicked Push` (flows, segments and webhooks use them; browsers can't send them).

## Billing

On the hosted service push is free unless the operator sets `PRICE_PUSH`. A non-zero price is charged per person reached (all their devices), from the balance, like texts.

## API

| | |
| --- | --- |
| `GET /api/push/settings` | Settings, device counts (including anonymous ones), the service worker URL and the device registration URL. |
| `PATCH /api/push/settings` | `web_enabled`, `sw_path`, `prompt`, `apns` (`{key_id, team_id, bundle_id, production, key_p8}` or `null`), `fcm` (`{service_account_json}` or `null`). |
| `POST /api/push/identity-secret` | A new enrollment secret (returned once). |
| `POST /api/push/test-devices` | Add this browser's subscription as your own test device. |
| `POST /api/push/test` | `{content?, to?: "me" or an email, profile_id?, device_id?}`. Sending to anyone but your own test devices needs permission to send campaigns; a person's push opt-out still applies. |
| `GET /api/push/profiles/{id}` | A person's devices (with their permission) and whether they opted out. |
| `PUT /api/push/profiles/{id}/opt-out` | `{opted_out: true|false}`. |
| `GET /api/push/report` | See Reports. |

Public (site key): `/client/push/subscribe`, `/client/push/unsubscribe`, `/client/push/sign-out`, `/client/push/devices`, `/client/push/devices/sign-out`, `/client/push/devices/remove`, `/client/push/delivered`, `/client/push/opened`.

Campaigns take `channel: "push"`; flows take `{"type": "push", "content": {...}, "next": …}` steps. MCP tools: `get_push_settings`, `update_push_settings`, `rotate_push_identity_secret`, `send_test_push`, `get_profile_push`, `set_push_opt_out`, `get_push_report`.

## Limits

- 30 device registrations an hour from one address, per account. 10 active devices per person (registering an 11th turns off their least recent). 1,000,000 devices per account, of which at most 200,000 anonymous; all are checked before a new device is stored. Anonymous devices unused for 180 days are removed. When an anonymous device is removed (unsubscribed, gone, or unused), its anonymous profile is deleted too unless something else is on it (another device, a list, or activity other than push's own).
- 50 test sends an hour per account.
- Web push only goes to the browsers' own push services (Google, Mozilla, Microsoft and Apple).
