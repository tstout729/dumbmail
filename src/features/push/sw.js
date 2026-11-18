/* DumbMail push notifications service worker. Host this file at the root of your website, at the
   path set in DumbMail (Settings → Push), so browsers let your site show notifications. */
var BASE = __BASE__, KEY = __KEY__;

function report(path, token) {
  if (!token) return Promise.resolve();
  return fetch(BASE + path + "?k=" + encodeURIComponent(KEY), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ t: token }),
    credentials: "omit",
    keepalive: true
  }).catch(function () {});
}

self.addEventListener("install", function () { self.skipWaiting(); });
self.addEventListener("activate", function (event) { event.waitUntil(self.clients.claim()); });

self.addEventListener("push", function (event) {
  var d = {};
  try { d = event.data ? event.data.json() : {}; } catch (e) { d = {}; }
  if (!d || typeof d !== "object" || (!d.t && !d.b)) return;
  var options = { body: d.b || "", data: { u: d.u, a: d.a || [], r: d.r } };
  if (d.i) options.icon = d.i;
  if (d.im) options.image = d.im;
  if (d.tag) { options.tag = d.tag; options.renotify = true; }
  if (d.a && d.a.length) options.actions = d.a.map(function (a, i) { return { action: "a" + i, title: a.t }; });
  event.waitUntil(Promise.all([self.registration.showNotification(d.t || "", options), report("/client/push/delivered", d.r)]));
});

self.addEventListener("notificationclick", function (event) {
  var n = event.notification, data = n.data || {};
  n.close();
  var url = data.u, m = /^a(\d)$/.exec(event.action || "");
  if (m && data.a && data.a[+m[1]]) url = data.a[+m[1]].u;
  if (url) event.waitUntil(self.clients.openWindow(url));
});

// The browser replaced the subscription: subscribe again with the same key and tell DumbMail,
// naming the old one so the new one stays linked to the same person.
self.addEventListener("pushsubscriptionchange", function (event) {
  var old = event.oldSubscription, key = old && old.options && old.options.applicationServerKey;
  if (!key) return;
  event.waitUntil(self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key }).then(function (sub) {
    var j = sub.toJSON();
    var oldKeys = old.toJSON ? old.toJSON().keys || {} : {};
    return fetch(BASE + "/client/push/subscribe?k=" + encodeURIComponent(KEY), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ subscription: j, previous: { endpoint: old.endpoint, auth: oldKeys.auth } }),
      credentials: "omit"
    });
  }).catch(function () {}));
});
