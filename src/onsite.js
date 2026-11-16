/* DumbMail onsite script: tracking, identification and sign-up forms. Paste once in <head> or the footer. */
(function () {
  "use strict";
  var BASE = __BASE__, KEY = __KEY__, ONLY_FORM = __FORM__;
  if ((window.__dumbmailLoaded || window.__postlarkLoaded) && !ONLY_FORM) return;
  if (!ONLY_FORM) window.__dumbmailLoaded = true;

  var store = {
    get: function (k, s) { try { return JSON.parse((s ? sessionStorage : localStorage).getItem(k)); } catch (e) { return null; } },
    set: function (k, v, s) { try { (s ? sessionStorage : localStorage).setItem(k, JSON.stringify(v)); } catch (e) {} }
  };
  var PROFILE = "pl_profile";
  function profile() { return store.get(PROFILE) || {}; }
  function identified() { var p = profile(); return !!(p.email || p.phone_number || p.external_id || p.$token); }
  function post(path, body) {
    try {
      return fetch(BASE + path + "?k=" + encodeURIComponent(KEY), {
        method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), keepalive: true, credentials: "omit"
      });
    } catch (e) { return Promise.resolve(); }
  }

  var api = {
    identify: function (props) {
      if (!props || typeof props !== "object") return api;
      var p = profile();
      for (var k in props) if (Object.prototype.hasOwnProperty.call(props, k)) p[k] = props[k];
      store.set(PROFILE, p);
      return api;
    },
    track: function (metric, properties, value) {
      if (!metric || !identified()) return api;
      post("/client/events", { metric: metric, profile: profile(), properties: properties || {}, value: value });
      return api;
    },
    profile: profile,
    showForm: function (id) { loadForms(function (forms) { forms.forEach(function (f) { if (f.id === id) show(f, true); }); }); return api; }
  };

  // Identify visitors arriving from a tracked email or text link (?_pl=token), then tidy the URL.
  try {
    var u = new URL(location.href), t = u.searchParams.get("_pl");
    if (t) {
      api.identify({ $token: t });
      u.searchParams.delete("_pl");
      history.replaceState(history.state, "", u.pathname + (u.search ? u.search : "") + u.hash);
    }
  } catch (e) {}

  if (!ONLY_FORM) {
    // Run calls queued before the script loaded: window.dumbmail = window.dumbmail || []; dumbmail.push(["track", ...]) (window.postlark still works)
    var queued = window.dumbmail || window.postlark;
    window.dumbmail = window.postlark = api;
    var q = (queued && (queued.q || (Array.isArray(queued) ? queued : null))) || window._dumbmailq || window._postlarkq || [];
    for (var i = 0; i < q.length; i++) { var c = q[i]; if (c && api[c[0]]) api[c[0]].apply(api, Array.prototype.slice.call(c, 1)); }
    window._dumbmailq = window._postlarkq = { push: function (c) { if (c && api[c[0]]) api[c[0]].apply(api, Array.prototype.slice.call(c, 1)); } };

    post("/client/ping", { url: location.href });
    if (!store.get("pl_active", true)) { store.set("pl_active", 1, true); api.track("Active on Site", { url: location.href, title: document.title, referrer: document.referrer || null }); }
  }

  // ---------- Forms ----------
  var formsCache = null, showing = false;
  function loadForms(cb) {
    if (formsCache) return cb(formsCache);
    fetch(BASE + "/client/forms?k=" + encodeURIComponent(KEY), { credentials: "omit" })
      .then(function (r) { return r.json(); })
      .then(function (j) { formsCache = (j.data || []).map(withVersion); cb(formsCache); })
      .catch(function () {});
  }
  function esc(s) { return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]; }); }
  function el(tag, attrs, html) { var x = document.createElement(tag); for (var k in attrs) x.setAttribute(k, attrs[k]); if (html != null) x.innerHTML = html; return x; }
  function days(n) { return n * 86400000; }
  function closedKey(f) { return "pl_form_closed_" + f.id; }
  function doneKey(f) { return "pl_form_done_" + f.id; }

  // A/B test: each visitor keeps one version (a = the form itself) and sees its text and image.
  function withVersion(f) {
    var base = f.config || {}, vs = base.variants || [];
    if (!vs.length) return f;
    var ids = ["a"].concat(vs.map(function (x) { return x.id; })), key = "pl_form_var_" + f.id, v = store.get(key);
    if (ids.indexOf(v) < 0) { v = ids[Math.floor(Math.random() * ids.length)]; store.set(key, v); }
    var c = {};
    for (var k in base) c[k] = base[k];
    vs.forEach(function (x) {
      if (x.id === v) ["headline", "body", "button_text", "image_url"].forEach(function (k) { if (x[k] && String(x[k]).trim()) c[k] = x[k]; });
    });
    return { id: f.id, kind: f.kind, config: c, version: v };
  }

  function fieldsHtml(f) {
    var c = f.config || {}, collect = c.collect || ["email"], h = "";
    var input = "width:100%;box-sizing:border-box;padding:12px 14px;border:1px solid #d1d5db;border-radius:8px;font:inherit;font-size:16px;margin:0 0 10px;color:#111827;background:#fff";
    if (collect.indexOf("first_name") >= 0) h += '<input type="text" name="first_name" placeholder="First name" autocomplete="given-name" style="' + input + '">';
    if (collect.indexOf("email") >= 0) h += '<input type="email" name="email" placeholder="Email address" autocomplete="email" required style="' + input + '">';
    if (collect.indexOf("phone_number") >= 0) {
      var consent = c.sms_consent_text || "By checking this box, you agree to receive recurring automated marketing text messages at this number. Consent is not a condition of purchase. Msg & data rates may apply. Reply STOP to opt out, HELP for help.";
      h += '<input type="tel" name="phone_number" placeholder="Mobile number" autocomplete="tel" style="' + input + '">' +
        '<label style="display:flex;gap:8px;align-items:flex-start;margin:0 0 12px;font-size:12px;line-height:1.4;color:#6b7280;text-align:left"><input type="checkbox" name="sms_consent" value="1" style="margin-top:2px;flex:none"><span>' + esc(consent) + "</span></label>";
    }
    return h;
  }

  function render(f) {
    var c = f.config || {}, color = /^#[0-9a-fA-F]{3,8}$/.test(c.brand_color || "") ? c.brand_color : "#1f6f5c";
    var embed = f.kind === "embed";
    var box = el("div", {
      role: embed ? "region" : "dialog", "aria-modal": embed ? "false" : "true", "aria-label": c.headline || "Sign up",
      style: "font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;background:#fff;color:#111827;border-radius:14px;padding:28px;box-sizing:border-box;text-align:center;line-height:1.5;" +
        (embed ? "max-width:480px;margin:0 auto;border:1px solid #e5e7eb;" : "position:relative;width:min(420px,calc(100vw - 32px));max-height:calc(100vh - 32px);overflow:auto;box-shadow:0 24px 60px rgba(0,0,0,.28);")
    },
      (c.image_url ? '<img src="' + esc(c.image_url) + '" alt="" style="width:100%;border-radius:10px;margin:0 0 16px;display:block">' : "") +
      '<h2 style="margin:0 0 8px;font-size:24px;line-height:1.2;font-weight:700;color:#111827">' + esc(c.headline) + "</h2>" +
      '<p style="margin:0 0 18px;color:#4b5563;font-size:15px">' + esc(c.body) + "</p>" +
      '<form novalidate>' + fieldsHtml(f) +
      '<button type="submit" style="width:100%;padding:13px;border:0;border-radius:8px;background:' + color + ';color:#fff;font:inherit;font-size:16px;font-weight:600;cursor:pointer">' + esc(c.button_text || "Sign up") + "</button>" +
      '<p data-pl-err style="color:#b91c1c;font-size:13px;margin:8px 0 0;min-height:1em"></p></form>');
    var wrap = box, onKey;
    function close(remember) {
      if (remember) store.set(closedKey(f), Date.now());
      if (onKey) document.removeEventListener("keydown", onKey);
      if (wrap.parentNode) wrap.parentNode.removeChild(wrap);
      showing = false;
    }
    if (!embed) {
      var x = el("button", { type: "button", "aria-label": "Close", style: "position:absolute;top:8px;right:10px;border:0;background:none;font-size:26px;line-height:1;cursor:pointer;color:#9ca3af;padding:6px" }, "&times;");
      x.onclick = function () { close(true); };
      box.appendChild(x);
      wrap = el("div", {
        style: f.kind === "flyout"
          ? "position:fixed;right:16px;bottom:16px;z-index:2147483000;max-width:calc(100vw - 32px)"
          : "position:fixed;inset:0;background:rgba(17,24,39,.55);display:flex;align-items:center;justify-content:center;z-index:2147483000;padding:16px;box-sizing:border-box"
      });
      wrap.appendChild(box);
      if (f.kind === "popup") wrap.addEventListener("click", function (e) { if (e.target === wrap) close(true); });
      onKey = function (e) { if (e.key === "Escape") close(true); };
      document.addEventListener("keydown", onKey);
    }
    box.querySelector("form").addEventListener("submit", function (e) {
      e.preventDefault();
      var form = e.target, d = { form_id: f.id, variant: f.version || null }, err = form.querySelector("[data-pl-err]"), btn = form.querySelector("button[type=submit]");
      ["email", "phone_number", "first_name"].forEach(function (n) { var i = form.querySelector("[name=" + n + "]"); if (i && i.value) d[n] = i.value.trim(); });
      var cb = form.querySelector("[name=sms_consent]"); d.sms_consent = !!(cb && cb.checked);
      if (form.querySelector("[name=email]") && !d.email) { err.textContent = "Enter your email address."; return; }
      if (d.phone_number && !d.sms_consent) { err.textContent = "Check the box to agree to texts, or leave the number blank."; return; }
      btn.disabled = true; btn.style.opacity = ".7"; err.textContent = "";
      post("/client/subscribe", d).then(function (r) { return r.json(); }).then(function (j) {
        btn.disabled = false; btn.style.opacity = "";
        if (j.error) { err.textContent = j.error.message; return; }
        store.set(doneKey(f), Date.now());
        var who = {}; if (d.email) who.email = d.email; if (d.phone_number) who.phone_number = d.phone_number; if (d.first_name) who.first_name = d.first_name;
        api.identify(who);
        form.outerHTML = '<p style="font-size:18px;font-weight:600;margin:8px 0 0;color:#111827">' + esc(j.message || c.success_message || "Thanks for signing up!") + "</p>";
        if (!embed) setTimeout(function () { close(false); }, 3000);
      }).catch(function () { btn.disabled = false; btn.style.opacity = ""; err.textContent = "Something went wrong. Please try again."; });
    });
    return { wrap: wrap, close: close };
  }

  function view(f) { fetch(BASE + "/client/forms/" + f.id + "/view?k=" + encodeURIComponent(KEY) + (f.version ? "&v=" + encodeURIComponent(f.version) : ""), { method: "POST", credentials: "omit", keepalive: true }).catch(function () {}); }

  function show(f, force) {
    if (f.kind === "embed") {
      var slots = document.querySelectorAll('[data-dumbmail-form="' + f.id + '"], [data-postlark-form="' + f.id + '"]');
      if (!slots.length && ONLY_FORM && document.currentScript) slots = [document.currentScript.parentNode];
      for (var i = 0; i < slots.length; i++) { if (slots[i].getAttribute("data-pl-rendered")) continue; slots[i].setAttribute("data-pl-rendered", "1"); slots[i].appendChild(render(f).wrap); view(f); }
      return;
    }
    if (showing && !force) return;
    showing = true;
    document.body.appendChild(render(f).wrap);
    view(f);
  }

  function eligible(f) {
    var t = (f.config && f.config.targeting) || {};
    if (store.get(doneKey(f))) return false;
    var closed = store.get(closedKey(f)), freq = t.frequency_days == null ? 7 : Number(t.frequency_days);
    if (closed && Date.now() - closed < days(freq)) return false;
    if (t.hide_for_subscribers !== false && identified()) return false;
    var mobile = window.matchMedia && window.matchMedia("(max-width: 768px)").matches;
    if (t.devices === "mobile" && !mobile) return false;
    if (t.devices === "desktop" && mobile) return false;
    var href = location.href.toLowerCase();
    var inc = (t.url_contains || []).filter(Boolean), exc = (t.url_excludes || []).filter(Boolean);
    if (inc.length && !inc.some(function (s) { return href.indexOf(String(s).toLowerCase()) >= 0; })) return false;
    if (exc.some(function (s) { return href.indexOf(String(s).toLowerCase()) >= 0; })) return false;
    return true;
  }

  function schedule(f) {
    if (!eligible(f)) return;
    var t = (f.config && f.config.targeting) || {}, fired = false;
    function fire() { if (!fired) { fired = true; show(f); } }
    if (t.trigger === "scroll") {
      var pct = Number(t.scroll_percent || 40);
      var onScroll = function () {
        var h = document.documentElement, max = (h.scrollHeight - h.clientHeight) || 1;
        if ((h.scrollTop || document.body.scrollTop) / max * 100 >= pct) { window.removeEventListener("scroll", onScroll); fire(); }
      };
      window.addEventListener("scroll", onScroll, { passive: true });
    } else if (t.trigger === "exit_intent") {
      var mobile = window.matchMedia && window.matchMedia("(max-width: 768px)").matches;
      if (mobile) { setTimeout(fire, 15000); return; } // no mouse on phones: fall back to a delay
      document.addEventListener("mouseout", function onOut(e) { if (!e.relatedTarget && e.clientY <= 0) { document.removeEventListener("mouseout", onOut); fire(); } });
    } else {
      setTimeout(fire, Math.max(0, Number((f.config && f.config.delay_seconds) || 5)) * 1000);
    }
  }

  function start() {
    loadForms(function (forms) {
      forms.forEach(function (f) {
        if (ONLY_FORM && f.id !== ONLY_FORM) return;
        if (f.kind === "embed") show(f);
        else if (ONLY_FORM && f.kind === "page") return;
        else schedule(f);
      });
    });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start); else start();
})();
