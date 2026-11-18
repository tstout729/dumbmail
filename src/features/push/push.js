/* DumbMail web push: loaded by the website snippet when push is on. It never asks for permission
   on page load: a small card with the site's own wording shows by the rules set in DumbMail, and
   only its "Allow" button (or the site's own button calling dumbmail.notifications.prompt()) opens
   the browser's prompt. A browser that said no is never asked again, and one that turned push off
   here is never subscribed again without a new click on Allow. */
(function () {
  "use strict";
  var BASE = __BASE__, KEY = __KEY__, CFG = __CONFIG__;
  if (!CFG || window.__dumbmailPush) return;
  window.__dumbmailPush = true;
  var supported = !!(window.isSecureContext && "serviceWorker" in navigator && "PushManager" in window && "Notification" in window);
  var now = function () { return Date.now(); };
  var store = {
    get: function (k, s) { try { return JSON.parse((s ? sessionStorage : localStorage).getItem(k)); } catch (e) { return null; } },
    set: function (k, v, s) { try { (s ? sessionStorage : localStorage).setItem(k, JSON.stringify(v)); } catch (e) {} },
    del: function (k) { try { localStorage.removeItem(k); } catch (e) {} }
  };
  function post(path, body) {
    return fetch(BASE + path + "?k=" + encodeURIComponent(KEY), {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), credentials: "omit", keepalive: true
    });
  }
  // The enrollment grant from the site's own server (see dumbmail.notifications.identify). Kept in
  // memory for this page only, never stored: the site passes it again on each page.
  var grant = null;
  function identity() { return grant ? { type: grant.type, value: grant.value, grant: grant.grant } : {}; }
  function keyBytes(b64) {
    var s = (b64 + "===".slice((b64.length + 3) % 4)).replace(/-/g, "+").replace(/_/g, "/");
    var raw = atob(s), out = new Uint8Array(raw.length);
    for (var i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
    return out;
  }
  // DumbMail's own registration: the worker at CFG.sw with DumbMail's scope, which controls no pages.
  // Never the site's own (a root registration also "matches" our scope's URL, so look it up exactly).
  var SCOPE_URL = location.origin + CFG.scope;
  function ours() {
    return navigator.serviceWorker.getRegistrations().then(function (regs) {
      for (var i = 0; i < regs.length; i++) if (regs[i].scope === SCOPE_URL) return regs[i];
      return null;
    });
  }
  function registration() {
    return navigator.serviceWorker.register(CFG.sw, { scope: CFG.scope }).then(function (reg) {
      var w = reg.installing || reg.waiting;
      if (reg.active || !w) return reg;
      return new Promise(function (resolve) { w.addEventListener("statechange", function () { if (w.state === "activated") resolve(reg); }); });
    });
  }
  function existing() {
    return ours().then(function (reg) { return reg ? reg.pushManager.getSubscription() : null; });
  }
  function report(sub, force) {
    var j = sub.toJSON(), id = identity();
    var sig = j.endpoint + "|" + (id.type || "") + "|" + (id.value || ""), last = store.get("pl_push_sent");
    // Once a day, or when the subscription or who the site says is signed in changes.
    if (!force && last && last.sig === sig && now() - last.at < 86400000) return Promise.resolve("granted");
    return post("/client/push/subscribe", { subscription: j, identity: id, url: location.href }).then(function (r) {
      if (r && r.ok) { store.set("pl_push_sent", { sig: sig, at: now() }); store.set("pl_push_sub", { endpoint: j.endpoint, auth: j.keys && j.keys.auth }); }
      return "granted";
    });
  }
  function subscribe(force) {
    return registration().then(function (reg) {
      return reg.pushManager.getSubscription().then(function (s) {
        return s || reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(CFG.key) });
      });
    }).then(function (sub) { return report(sub, force); });
  }
  // A click on Allow (the card's, or the site's own button).
  function prompt() {
    if (!supported) return Promise.resolve("unsupported");
    if (Notification.permission === "denied") return Promise.resolve("denied");
    store.del("pl_push_off");
    if (Notification.permission === "granted") return subscribe(true).catch(function () { return "error"; });
    hide();
    return Promise.resolve(Notification.requestPermission()).then(function (p) {
      if (p === "granted") return subscribe(true).catch(function () { return "error"; });
      if (p === "denied") forget();
      return p;
    });
  }
  // Tell DumbMail this browser's registration is gone, keeping the proof (endpoint and its secret)
  // until DumbMail confirms, so a failed request is sent again on the next page.
  function forget(proof) {
    var s = proof || store.get("pl_push_removal") || store.get("pl_push_sub");
    store.del("pl_push_sub"); store.del("pl_push_sent");
    if (!s || !s.endpoint) { store.del("pl_push_removal"); return Promise.resolve(); }
    store.set("pl_push_removal", s);
    return post("/client/push/unsubscribe", { endpoint: s.endpoint, auth: s.auth }).then(function (r) {
      if (r && r.ok) store.del("pl_push_removal");
    }).catch(function () {});
  }
  // Turn push off in this browser: it stays off until the person clicks Allow again.
  function unsubscribe() {
    store.set("pl_push_off", true);
    if (!supported) return forget();
    return existing().then(function (sub) {
      var proof = sub ? { endpoint: sub.endpoint, auth: (sub.toJSON().keys || {}).auth } : null;
      return forget(proof).then(function () { return sub ? sub.unsubscribe() : false; });
    });
  }
  // The site's visitor signed out: this browser goes back to being anonymous. If DumbMail can't
  // confirm that, the browser stops using this subscription (so the person signed out gets nothing
  // more here) and the unlink is sent again on later pages until DumbMail confirms it.
  function signOut() {
    grant = null;
    store.del("pl_push_sent");
    if (!supported) return Promise.resolve();
    return existing().then(function (sub) {
      if (!sub) return;
      var proof = { endpoint: sub.endpoint, auth: (sub.toJSON().keys || {}).auth };
      return post("/client/push/sign-out", proof).then(function (r) {
        if (!r || !r.ok) throw new Error("not confirmed");
      }).catch(function () {
        return Promise.resolve(sub.unsubscribe()).catch(function () {}).then(function () { return forget(proof); });
      });
    }).catch(function () {});
  }

  // ---------- The in-page card ----------
  var card = null;
  function esc(s) { return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]; }); }
  function hide() { if (card) { card.remove(); card = null; } }
  function show() {
    if (card || !document.body) return;
    var P = CFG.prompt, host = document.createElement("div");
    host.setAttribute("data-dumbmail-push", "");
    var root = host.attachShadow ? host.attachShadow({ mode: "open" }) : host;
    var edge = P.position === "top" ? "top:16px" : "bottom:16px";
    root.innerHTML = "<style>.c{position:fixed;" + edge + ";left:50%;transform:translateX(-50%);z-index:2147483646;width:min(380px,calc(100vw - 32px));box-sizing:border-box;background:#fff;color:#111827;border-radius:12px;box-shadow:0 10px 30px rgba(0,0,0,.18);padding:16px;font:14px/1.45 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif}" +
      ".t{font-weight:600;font-size:15px;margin:0 0 4px}.b{margin:0 0 12px;color:#4b5563}.r{display:flex;gap:8px;justify-content:flex-end}" +
      "button{font:inherit;font-weight:600;border-radius:8px;padding:8px 14px;cursor:pointer;border:1px solid #d1d5db;background:#fff;color:#374151}button.y{background:" + esc(CFG.color) + ";border-color:transparent;color:#fff}</style>" +
      "<div class='c' role='dialog' aria-label='" + esc(P.title) + "'><p class='t'>" + esc(P.title) + "</p><p class='b'>" + esc(P.body) + "</p>" +
      "<div class='r'><button type='button' class='n'>" + esc(P.deny_text) + "</button><button type='button' class='y'>" + esc(P.allow_text) + "</button></div></div>";
    root.querySelector(".y").addEventListener("click", function () { prompt(); });
    root.querySelector(".n").addEventListener("click", function () { store.set("pl_push_dismissed", now()); hide(); });
    document.body.appendChild(host);
    card = host;
  }
  function pageOk(P) {
    var u = location.href;
    if (P.url_excludes && P.url_excludes.some(function (x) { return u.indexOf(x) >= 0; })) return false;
    return !P.url_contains || !P.url_contains.length || P.url_contains.some(function (x) { return u.indexOf(x) >= 0; });
  }
  function maybeShow() {
    var P = CFG.prompt;
    if (!P || !P.enabled || Notification.permission !== "default") return;
    var views = (store.get("pl_push_pv", true) || 0) + 1;
    store.set("pl_push_pv", views, true);
    var dismissed = store.get("pl_push_dismissed");
    if (dismissed && now() - dismissed < P.frequency_days * 86400000) return;
    if (views < P.page_views || !pageOk(P)) return;
    setTimeout(function () { if (Notification.permission === "default") show(); }, (P.delay_seconds || 0) * 1000);
  }

  var api = {
    supported: supported,
    /** "granted", "denied", "default" or "unsupported". */
    status: function () { return supported ? Notification.permission : "unsupported"; },
    prompt: prompt,
    subscribe: prompt,
    unsubscribe: unsubscribe,
    /** An enrollment grant from your server for the signed-in customer: { type: "external_id" or "email", value, grant }. Pass it on every page. */
    identify: function (v) {
      if (!v || typeof v !== "object" || !v.grant || !v.type || !v.value) return api;
      grant = { type: String(v.type), value: String(v.value), grant: String(v.grant) };
      if (supported && Notification.permission === "granted" && !store.get("pl_push_off")) existing().then(function (sub) { if (sub) report(sub, false); }).catch(function () {});
      return api;
    },
    signOut: signOut
  };
  if (window.dumbmail && typeof window.dumbmail === "object") window.dumbmail.notifications = api;
  // A removal DumbMail hasn't confirmed yet: send it again.
  if (store.get("pl_push_removal")) forget();
  if (!supported) return;
  if (Notification.permission === "granted") {
    // Only a browser that subscribed here and didn't turn push off since: never recreated without a click.
    if (!store.get("pl_push_off")) existing().then(function (sub) { if (sub) report(sub, false); }).catch(function () {});
  } else if (Notification.permission === "denied") { if (store.get("pl_push_sub")) forget(); }
  else if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", maybeShow);
  else maybeShow();
})();
