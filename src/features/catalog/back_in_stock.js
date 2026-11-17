/* DumbMail back-in-stock button: on a sold-out product page it shows "Notify me when it's back" and an email box. */
(function () {
  "use strict";
  var BASE = __BASE__, KEY = __KEY__;
  if (window.__dumbmailBackInStock) return;
  window.__dumbmailBackInStock = true;
  var CFG = window.dumbmailBackInStock || {};
  var root = null, lastKey = null;

  function q(path, params) {
    var s = "?k=" + encodeURIComponent(KEY);
    for (var k in params) if (params[k]) s += "&" + k + "=" + encodeURIComponent(params[k]);
    return BASE + path + s;
  }
  function esc(s) { return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]; }); }
  function placeholder() { return document.querySelector("[data-dumbmail-back-in-stock]"); }
  // The option the shopper picked in the store's own add-to-cart form (Shopify and most themes keep it in name="id").
  function selectedVariant() {
    var el = document.querySelector('form[action*="/cart/add"] [name="id"]');
    if (!el) return null;
    if (el.type === "radio") { var on = document.querySelector('form[action*="/cart/add"] [name="id"]:checked'); return on ? on.value : null; }
    return el.value || null;
  }

  // Which product is this page? Settings first, then the placeholder's attributes, the store, the page's structured data, the URL.
  function detect(cb) {
    var el = placeholder();
    var pid = CFG.productId || (el && el.getAttribute("data-product-id"));
    var vid = CFG.variantId || (el && el.getAttribute("data-variant-id"));
    var url = location.href.split("#")[0];
    if (pid || vid) return cb({ product_id: pid, variant_id: vid || selectedVariant(), url: url });
    var m = /\/products\/([^\/?#]+)/.exec(location.pathname);
    if (window.Shopify && m) {
      var r = (window.Shopify.routes && window.Shopify.routes.root) || "/";
      fetch(r + "products/" + m[1] + ".js", { credentials: "same-origin" }).then(function (x) { return x.ok ? x.json() : null; }).then(function (p) {
        if (!p) return cb({ url: url });
        var sel = new URLSearchParams(location.search).get("variant") || selectedVariant();
        cb({ product_id: String(p.id), variant_id: sel || null, url: url });
      }).catch(function () { cb({ url: url }); });
      return;
    }
    var id = null;
    var lds = document.querySelectorAll('script[type="application/ld+json"]');
    for (var i = 0; i < lds.length && !id; i++) {
      try {
        var d = JSON.parse(lds[i].textContent);
        [].concat(d["@graph"] || d).forEach(function (n) { if (!id && n && /Product/.test([].concat(n["@type"] || []).join(" "))) id = n.productID || n.sku || null; });
      } catch (e) {}
    }
    cb({ product_id: id, url: url });
  }

  function style(w) {
    if (document.getElementById("dm-bis-style")) return;
    var s = document.createElement("style");
    s.id = "dm-bis-style";
    s.textContent =
      ".dm-bis{--dm-bis-color:" + (w.color || "#111827") + ";--dm-bis-text:" + (w.text_color || "#ffffff") + ";--dm-bis-radius:6px;font:inherit;margin:12px 0;max-width:420px}" +
      ".dm-bis-float{position:fixed;right:16px;bottom:16px;z-index:2147483000;background:#fff;padding:12px;border:1px solid #e5e7eb;border-radius:10px;box-shadow:0 8px 30px rgba(0,0,0,.15);margin:0;width:min(340px,calc(100vw - 32px))}" +
      ".dm-bis p{margin:0 0 8px}.dm-bis form{display:flex;flex-wrap:wrap;gap:8px}" +
      ".dm-bis input,.dm-bis select{flex:1 1 180px;min-width:0;padding:10px 12px;border:1px solid #d1d5db;border-radius:var(--dm-bis-radius);font:inherit}" +
      ".dm-bis button{padding:10px 16px;border:0;border-radius:var(--dm-bis-radius);background:var(--dm-bis-color);color:var(--dm-bis-text);font:inherit;font-weight:600;cursor:pointer}" +
      ".dm-bis button[disabled]{opacity:.6;cursor:default}.dm-bis-msg{font-size:.9em;margin-top:6px}.dm-bis-err{color:#b91c1c}";
    document.head.appendChild(s);
  }

  function mount() {
    var el = placeholder();
    var box = document.createElement("div");
    box.className = "dm-bis";
    if (el) { el.innerHTML = ""; el.appendChild(box); return box; }
    var anchor = document.querySelector('form[action*="/cart/add"]') || document.querySelector("form.cart") || document.querySelector('[name="add"],[name="add-to-cart"]');
    if (anchor && anchor.parentNode) { anchor.parentNode.insertBefore(box, anchor.nextSibling); return box; }
    box.className += " dm-bis-float";
    document.body.appendChild(box);
    return box;
  }

  function render(info, page) {
    if (root && root.parentNode) root.parentNode.removeChild(root);
    root = null;
    if (!info || !info.found || !info.enabled) return;
    var out = (info.variants || []).filter(function (v) { return !v.in_stock; });
    var chosen = info.variant;
    // Show only when what the shopper is looking at is sold out.
    if (chosen ? chosen.in_stock : info.in_stock) return;
    var w = info.widget || {};
    style(w);
    root = mount();
    var pick = !chosen && out.length > 1 ? "<select name='variant' aria-label='Option'>" + out.map(function (v) { return "<option value='" + esc(v.id) + "'>" + esc(v.title || v.id) + "</option>"; }).join("") + "</select>" : "";
    root.innerHTML = "<p><strong>" + esc(w.heading || "Sold out") + "</strong></p>" +
      "<button type='button' data-open>" + esc(w.button_text || "Notify me when it's back") + "</button>";
    root.querySelector("[data-open]").addEventListener("click", function () {
      root.innerHTML = "<p><strong>" + esc(w.heading || "Sold out") + "</strong></p><form novalidate>" + pick +
        "<input type='email' name='email' placeholder='Your email' autocomplete='email' required>" +
        "<button type='submit'>" + esc(w.button_text || "Notify me") + "</button></form><div class='dm-bis-msg' aria-live='polite'></div>";
      var known = window.dumbmail && window.dumbmail.profile && window.dumbmail.profile().email;
      if (known) root.querySelector("input").value = known;
      root.querySelector("form").addEventListener("submit", function (e) {
        e.preventDefault();
        var email = root.querySelector("input").value.trim(), sel = root.querySelector("select"), btn = root.querySelector("button"), msg = root.querySelector(".dm-bis-msg");
        if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) { msg.className = "dm-bis-msg dm-bis-err"; msg.textContent = "Enter a valid email."; return; }
        btn.disabled = true;
        // A visitor who came from one of the store's emails carries its link token: they're armed without a confirmation email.
        var token = (window.dumbmail && window.dumbmail.profile && window.dumbmail.profile().$token) || null;
        fetch(q("/client/back-in-stock", {}), {
          method: "POST", headers: { "content-type": "application/json" }, credentials: "omit",
          body: JSON.stringify({ email: email, token: token, product_id: info.product_id, variant_id: sel ? sel.value : chosen ? chosen.id : null })
        }).then(function (r) { return r.json().then(function (j) { return { ok: r.ok, j: j }; }); }).then(function (r) {
          btn.disabled = false;
          if (!r.ok) { msg.className = "dm-bis-msg dm-bis-err"; msg.textContent = (r.j && r.j.error && r.j.error.message) || "Something went wrong. Try again."; return; }
          if (window.dumbmail && window.dumbmail.identify) window.dumbmail.identify({ email: email }, "form");
          root.innerHTML = "<p>" + esc(r.j.message || "Thanks! We'll email you when it's back.") + "</p>";
        }).catch(function () { btn.disabled = false; msg.className = "dm-bis-msg dm-bis-err"; msg.textContent = "Couldn't reach the server. Try again."; });
      });
    });
  }

  function check() {
    detect(function (page) {
      var key = [page.product_id, page.variant_id, page.url].join("|");
      if (key === lastKey) return;
      lastKey = key;
      fetch(q("/client/products/stock", page), { credentials: "omit" }).then(function (r) { return r.ok ? r.json() : null; }).then(function (info) { render(info, page); }).catch(function () {});
    });
  }

  function start() {
    check();
    // Stores switch variants by changing the URL (?variant=…) without reloading, or only in their add-to-cart form.
    var href = location.href;
    setInterval(function () { if (location.href !== href) { href = location.href; check(); } }, 1500);
    document.addEventListener("change", function (e) {
      var t = e.target;
      if (t && t.closest && t.closest('form[action*="/cart/add"]')) setTimeout(check, 0);
    });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start); else start();
})();
