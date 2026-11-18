/* DumbMail push notifications. Host this file on your site: the snippet registers it with a scope
   of its own (/dumbmail-push/), so it controls no pages and never replaces your own service worker.
   It only handles notifications DumbMail sent, and only opens DumbMail's signed tap links. */
(function () {
  var BASE = __BASE__, KEY = __KEY__, LINKS = __LINKS__, VAPID = __VAPID__;

  function ours(d) {
    return !!(d && typeof d === "object" && d.dm === 1 && typeof d.r === "string" && d.r.length < 200);
  }
  function signedLink(u) {
    return typeof u === "string" && LINKS.some(function (p) { return u.indexOf(p) === 0; });
  }
  function b64u(buf) {
    var bytes = new Uint8Array(buf), s = "";
    for (var i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }
  function report(token) {
    return fetch(BASE + "/client/push/delivered?k=" + encodeURIComponent(KEY), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ t: token }),
      credentials: "omit",
      keepalive: true
    }).catch(function () {});
  }

  self.addEventListener("push", function (event) {
    var d = null;
    try { d = event.data ? event.data.json() : null; } catch (e) { d = null; }
    // Not DumbMail's: leave it to the site's own handlers.
    if (!ours(d)) return;
    var actions = (d.a || []).filter(function (a) { return a && signedLink(a.u); }).slice(0, 2);
    var options = { body: String(d.b || ""), data: { dm: 1, u: signedLink(d.u) ? d.u : null, a: actions } };
    if (d.i) options.icon = String(d.i);
    if (d.im) options.image = String(d.im);
    if (d.tag) { options.tag = String(d.tag); options.renotify = true; }
    if (actions.length) options.actions = actions.map(function (a, i) { return { action: "a" + i, title: String(a.t) }; });
    // Reported only once it's actually shown.
    event.waitUntil(self.registration.showNotification(String(d.t || ""), options).then(function () { return report(d.r); }));
  });

  self.addEventListener("notificationclick", function (event) {
    var n = event.notification, data = n.data || {};
    if (data.dm !== 1) return;
    n.close();
    var url = data.u, m = /^a(\d)$/.exec(event.action || "");
    if (m && data.a && data.a[+m[1]]) url = data.a[+m[1]].u;
    if (signedLink(url)) event.waitUntil(self.clients.openWindow(url));
  });

  // The browser replaced the subscription: subscribe again with the same key and tell DumbMail,
  // naming the old one so the new one keeps the same owner.
  self.addEventListener("pushsubscriptionchange", function (event) {
    var old = event.oldSubscription, key = old && old.options && old.options.applicationServerKey;
    // Only DumbMail's own subscription (the site's other subscriptions are its own business).
    if (!key || !VAPID || b64u(key) !== VAPID) return;
    event.waitUntil(self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key }).then(function (sub) {
      var oldKeys = old.toJSON ? old.toJSON().keys || {} : {};
      return fetch(BASE + "/client/push/subscribe?k=" + encodeURIComponent(KEY), {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ subscription: sub.toJSON(), previous: { endpoint: old.endpoint, auth: oldKeys.auth } }),
        credentials: "omit"
      });
    }).catch(function () {}));
  });
})();
