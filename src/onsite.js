/* DumbMail onsite script: visitor recognition, shopping events, sign-up forms and a setup test mode. Paste once in <head> or the footer. */
(function () {
  "use strict";
  var BASE = __BASE__, KEY = __KEY__, ONLY_FORM = __FORM__, VERSION = 4;
  var ME = document.currentScript;
  if ((window.__dumbmailLoaded || window.__postlarkLoaded) && !ONLY_FORM) {
    // A second copy of the snippet: tell the first one (test mode shows it) and stop.
    if (typeof window.__dumbmailDuplicate === "function") window.__dumbmailDuplicate(KEY);
    return;
  }
  if (!ONLY_FORM) window.__dumbmailLoaded = true;

  var CFG = window.dumbmailConfig || {};
  function attr(n) { try { return ME && ME.getAttribute(n); } catch (e) { return null; } }
  var AUTO = !ONLY_FORM && CFG.autoTrack !== false && attr("data-auto-track") !== "false";
  var CAPTURE = !ONLY_FORM && CFG.captureEmails !== false && attr("data-capture-emails") !== "false";
  var noop = function () {};
  var now = function () { return Date.now(); };

  var store = {
    get: function (k, s) { try { return JSON.parse((s ? sessionStorage : localStorage).getItem(k)); } catch (e) { return null; } },
    set: function (k, v, s) { try { (s ? sessionStorage : localStorage).setItem(k, JSON.stringify(v)); } catch (e) {} },
    del: function (k, s) { try { (s ? sessionStorage : localStorage).removeItem(k); } catch (e) {} }
  };
  var PROFILE = "pl_profile";
  function profile() { return store.get(PROFILE) || {}; }
  function identified(p) { p = p || profile(); return !!(p.email || p.phone_number || p.external_id || p.$token); }
  function post(path, body) {
    try {
      return fetch(BASE + path + "?k=" + encodeURIComponent(KEY), {
        method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), keepalive: true, credentials: "omit"
      });
    } catch (e) { return Promise.resolve(); }
  }

  // ---------- Helpers ----------
  function text(x) { return x == null ? "" : String(x).replace(/\s+/g, " ").trim(); }
  function cut(s, n) { s = text(s); return s.length > n ? s.slice(0, n) : s; }
  function abs(u) {
    if (!u || typeof u !== "string") return undefined;
    try { return new URL(u.indexOf("//") === 0 ? location.protocol + u : u, location.href).href; } catch (e) { return undefined; }
  }
  // "$1,299.00", "1.299,00 €", 1299, "18" → number
  function num(x) {
    if (typeof x === "number") return isFinite(x) ? x : undefined;
    var m = /-?\d(?:[\d.,]*\d)?/.exec(text(x));
    if (!m) return undefined;
    var s = m[0];
    var dot = s.lastIndexOf("."), comma = s.lastIndexOf(",");
    if (dot >= 0 && comma >= 0) s = dot > comma ? s.replace(/,/g, "") : s.replace(/\./g, "").replace(",", ".");
    else if (comma >= 0) s = /,\d{1,2}$/.test(s) ? s.replace(/\./g, "").replace(",", ".") : s.replace(/,/g, "");
    var n = parseFloat(s);
    return isFinite(n) ? Math.round(n * 100) / 100 : undefined;
  }
  function clean(o) {
    var r = {};
    for (var k in o) {
      if (!Object.prototype.hasOwnProperty.call(o, k)) continue;
      var v = o[k];
      if (v == null || v === "" || (typeof v === "number" && !isFinite(v))) continue;
      if (typeof v === "string") v = cut(v, 500);
      if (Array.isArray(v) && !v.length) continue;
      r[k] = v;
    }
    return r;
  }
  // Fill gaps in `a` from `b`.
  function fill(a, b) { for (var k in b) if (a[k] == null && b[k] != null) a[k] = b[k]; return a; }
  function first(x) { return Array.isArray(x) ? x[0] : x; }
  function meta(name) {
    var m = document.querySelector('meta[property="' + name + '"], meta[name="' + name + '"]');
    return m ? m.getAttribute("content") : null;
  }
  function hasClass(n) { return !!(document.body && document.body.classList.contains(n)); }
  function each(list, fn) { for (var i = 0; list && i < list.length; i++) fn(list[i], i); }

  // ---------- Profile ----------
  var EMAIL_RE = /^[^\s@<>()[\]\\,;:"]+@[^\s@<>()[\]\\,;:"]+\.[a-z]{2,}$/i;
  var api = {
    identify: function (props, how) {
      if (!props || typeof props !== "object") return api;
      var p = profile(), before = identified(p);
      var em = props.email && String(props.email).trim().toLowerCase();
      // A different email is a different person: start a fresh profile instead of merging into theirs.
      if (em && p.email && String(p.email).toLowerCase() !== em) p = {};
      for (var k in props) if (Object.prototype.hasOwnProperty.call(props, k) && props[k] != null && props[k] !== "") p[k] = props[k];
      if (TEST) {
        var who = p.email || p.phone_number || p.external_id;
        if (who) test.check("identify", { email: p.email || null, how: how || "identify", summary: "Recognized " + who + (how === "email_field" || how === "form" ? " from a form on your site" : how === "order" ? " from the order" : "") });
        return api;
      }
      store.set(PROFILE, p);
      if (identified(p) && (!before || em)) flushBuffer();
      return api;
    },
    track: function (metric, properties, value) { if (metric) send(String(metric), properties || {}, value); return api; },
    profile: profile,
    showForm: function (id) { loadForms(function (forms) { forms.forEach(function (f) { if (f.id === id) show(f, true); }); }); return api; },
    // Shopping helpers for custom sites. Each takes plain objects; see the Website page in DumbMail.
    viewedProduct: function (p) { offerProduct(item(p), "manual", 0); flushProduct(); return api; },
    addedToCart: function (p, cart) { addToCart(item(p), "manual", cart ? cartFrom(cart) : null); flushAdd(); return api; },
    startedCheckout: function (cart) { startCheckout(cartFrom(cart || {}), "manual", true); return api; },
    placedOrder: function (order) { placeOrder(order || {}, "manual"); return api; },
    version: VERSION
  };

  // Events from visitors the browser doesn't know yet wait here (for up to 3 days) and are sent once they're recognized.
  var BUFFER = "pl_buffer";
  function buffer(ev) {
    var b = (store.get(BUFFER) || []).filter(function (e) { return e && now() - e.time < 3 * 86400000; });
    b.push(ev);
    store.set(BUFFER, b.slice(-25));
  }
  function flushBuffer() {
    var b = store.get(BUFFER) || [];
    store.del(BUFFER);
    var p = profile();
    b.forEach(function (e) {
      if (now() - e.time < 3 * 86400000) post("/client/events", { metric: e.metric, profile: p, properties: e.properties, value: e.value, unique_id: e.unique_id, time: e.time });
    });
  }
  function send(metric, properties, value, uniqueId) {
    if (TEST) { test.event(metric, properties, value); return; }
    var ev = { metric: metric, properties: properties || {}, value: typeof value === "number" && isFinite(value) ? value : undefined, unique_id: uniqueId, time: now() };
    if (!identified()) { buffer(ev); return; }
    post("/client/events", { metric: ev.metric, profile: profile(), properties: ev.properties, value: ev.value, unique_id: ev.unique_id, time: ev.time });
  }

  // ---------- Test mode ----------
  // ?dumbmail_test=CODE (from the Website page in DumbMail) turns it on for this browser tab for 2 hours.
  // A test code from a link only counts once DumbMail confirms it's a live test for this site's account,
  // so a made-up link can't put a shopper's browser into test mode.
  var TEST = null, NEW_TEST = null;
  (function () {
    try {
      var u = new URL(location.href), changed = false;
      var t = u.searchParams.get("_pl");
      // A tracked link names the person who clicked it, so it replaces whoever this browser knew before.
      if (t) { store.set(PROFILE, { $token: t }); flushBuffer(); u.searchParams.delete("_pl"); changed = true; }
      var code = u.searchParams.get("dumbmail_test");
      if (code) { u.searchParams.delete("dumbmail_test"); changed = true; }
      if (!code) { var m = /[#&]dumbmail_test=([A-Za-z0-9_-]+)/.exec(location.hash); if (m) code = m[1]; }
      if (code === "off") { store.del("pl_test", true); store.del("pl_test_state", true); }
      else if (code && /^[A-Za-z0-9_-]{6,40}$/.test(code) && !ONLY_FORM) NEW_TEST = code;
      if (changed) history.replaceState(history.state, "", u.pathname + (u.search ? u.search : "") + u.hash);
    } catch (e) {}
    if (ONLY_FORM) return;
    var s = store.get("pl_test", true);
    if (s && s.code && s.until > now()) TEST = s;
  })();

  var test = (function () {
    var state = store.get("pl_test_state", true) || { checks: {} }, root = null, list = null, min = false;
    var STEPS = [
      ["loaded", "Script running", "Loading…"],
      ["forms", "Sign-up forms", "Loading your forms…"],
      ["product", "Product views", "Open a product page."],
      ["cart", "Add to cart", "Add a product to your cart."],
      ["checkout", "Checkout", "Go to checkout."],
      ["identify", "Visitor recognition", "Enter your email in the checkout email box, or submit a sign-up or contact form."],
      ["order", "Orders", "Optional: place a test order."]
    ];
    function save() { store.set("pl_test_state", state, true); }
    function check(name, detail, quiet) {
      if (!TEST) return;
      detail = detail || {};
      state.checks[name] = { ok: detail.ok !== false, detail: detail, at: now() };
      save(); draw();
      if (!quiet) post("/client/test", { code: TEST.code, check: name, detail: detail, url: location.href }).catch(noop);
    }
    function warn(kind, message) {
      state.warnings = state.warnings || {};
      if (state.warnings[kind]) return;
      state.warnings[kind] = message; save(); draw();
      post("/client/test", { code: TEST.code, check: "warn_" + kind, detail: { message: message }, url: location.href }).catch(noop);
    }
    function money(v, cur) { return typeof v === "number" ? (cur && cur.length === 3 ? cur + " " : "$") + v.toFixed(2) : ""; }
    function summary(metric, p, v) {
      if (metric === "Viewed Product") return text(p.name) + (p.price != null ? " · " + money(p.price, p.currency) : "");
      if (metric === "Added to Cart") return text(p.name || "an item") + (p.cart_total != null ? " · cart " + money(p.cart_total, p.currency) : "");
      if (metric === "Started Checkout") return (p.item_count || (p.items || []).length || 0) + " item(s)" + (v != null ? " · " + money(v, p.currency) : "");
      if (metric === "Placed Order") return "Order " + text(p.order_id || "") + (v != null ? " · " + money(v, p.currency) : "");
      return metric;
    }
    var CHECK_FOR = { "Viewed Product": "product", "Added to Cart": "cart", "Started Checkout": "checkout", "Placed Order": "order" };
    function event(metric, props, value) {
      var c = CHECK_FOR[metric];
      props = props || {};
      var d = { metric: metric, summary: summary(metric, props, value), tracked_by: props.tracked_by, value: value, name: props.name, price: props.price, image_url: props.image_url, url: props.url, item_count: props.item_count, order_id: props.order_id };
      if (c) check(c, d); else check("custom", d);
    }
    function draw() {
      if (!TEST || !document.body) return;
      if (!root) {
        var host = document.createElement("div");
        host.setAttribute("data-pl-own", "1");
        host.style.cssText = "position:fixed;left:16px;bottom:16px;z-index:2147483646;";
        root = host.attachShadow ? host.attachShadow({ mode: "open" }) : host;
        document.body.appendChild(host);
        root.innerHTML = "<style>" +
          ".p{font:13px/1.4 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#111827;background:#fff;border:1px solid #e5e7eb;border-radius:12px;box-shadow:0 12px 40px rgba(0,0,0,.18);width:min(320px,calc(100vw - 32px));overflow:hidden}" +
          ".h{display:flex;align-items:center;gap:8px;padding:10px 12px;background:#111827;color:#fff;font-weight:600}.h span{flex:1}" +
          ".h button{border:0;background:none;color:#9ca3af;font:inherit;font-size:16px;cursor:pointer;padding:0 4px}" +
          "ul{list-style:none;margin:0;padding:6px 12px 2px}li{display:flex;gap:8px;padding:5px 0;border-bottom:1px solid #f3f4f6}li:last-child{border:0}" +
          ".i{flex:none;width:16px;text-align:center;font-weight:700}.ok{color:#059669}.no{color:#d1d5db}.w{color:#b45309}" +
          ".t{font-weight:600}.d{color:#6b7280;font-size:12px;word-break:break-word}" +
          ".f{padding:8px 12px 12px;color:#6b7280;font-size:12px}.f button{margin-top:8px;margin-right:6px;border:1px solid #d1d5db;background:#fff;border-radius:6px;padding:4px 8px;font:inherit;cursor:pointer;color:#111827}" +
          ".warn{margin:8px 12px 0;padding:8px;border-radius:8px;background:#fffbeb;color:#92400e;font-size:12px}" +
          "</style><div class='p'><div class='h'><span>DumbMail test mode</span><button data-a='min' aria-label='Minimize'>–</button><button data-a='end' aria-label='End test'>×</button></div><div data-body></div></div>";
        root.addEventListener("click", function (e) {
          var a = e.target && e.target.getAttribute && e.target.getAttribute("data-a");
          if (a === "min") { min = !min; draw(); }
          if (a === "end") { store.del("pl_test", true); store.del("pl_test_state", true); TEST = null; host.parentNode && host.parentNode.removeChild(host); }
          if (a === "forms") loadForms(function (forms) { forms.forEach(function (f) { if (f.kind === "popup" || f.kind === "flyout") show(f, true); }); });
        });
        list = root.querySelector("[data-body]");
      }
      if (min) { list.innerHTML = ""; return; }
      var h = "";
      each(Object.keys(state.warnings || {}), function (k) { h += "<div class='warn'>" + esc(state.warnings[k]) + "</div>"; });
      h += "<ul>";
      STEPS.forEach(function (s) {
        var c = state.checks[s[0]];
        var d = c ? (c.detail.summary || c.detail.message || "") : s[2];
        if (!c && PLATFORM === "shopify" && (s[0] === "checkout" || s[0] === "order")) d = (s[0] === "order" ? "Optional: place a test order. " : "Go to checkout. ") + "Shopify's checkout pixel reports this; watch DumbMail for it.";
        h += "<li><span class='i " + (c ? (c.ok ? "ok" : "w") : "no") + "'>" + (c ? (c.ok ? "✓" : "!") : "○") + "</span><div><div class='t'>" + esc(s[1]) + "</div><div class='d'>" + esc(d) + "</div></div></li>";
      });
      h += "</ul><div class='f'>Test events aren't saved, so you can click around and even place a test order. Results also show in DumbMail." +
        "<br><button data-a='forms'>Show my popups</button><button data-a='end'>End test</button></div>";
      list.innerHTML = h;
    }
    function reset() { state = { checks: {} }; save(); }
    return { check: check, warn: warn, event: event, draw: draw, reset: reset };
  })();

  function verifyTest(code, done) {
    var called = false;
    function finish() { if (!called) { called = true; done(); } }
    fetch(BASE + "/client/test?k=" + encodeURIComponent(KEY) + "&code=" + encodeURIComponent(code), { credentials: "omit" })
      .then(function (r) { return r.json(); })
      .then(function (j) {
        if (j && j.active === true) {
          TEST = { code: code, until: now() + 2 * 3600000 };
          store.set("pl_test", TEST, true);
          test.reset();
        }
        finish();
      })
      .catch(finish);
    setTimeout(finish, 5000);
  }

  function testHooks() {
    if (!TEST) return;
    window.__dumbmailDuplicate = function (k) {
      test.warn("duplicate", k === KEY ? "The DumbMail snippet is on this page twice. Remove one copy." : "This page has two DumbMail snippets with different keys. Keep only the one from your Website page.");
    };
    document.addEventListener("securitypolicyviolation", function (e) {
      if (e.blockedURI && e.blockedURI.indexOf(BASE) === 0) test.warn("csp", "Your site's Content-Security-Policy blocks " + BASE + ". Add it to connect-src, script-src and img-src.");
    });
  }

  // ---------- Platform ----------
  function platform() {
    var w = window;
    if (w.Shopify && (w.Shopify.shop || w.Shopify.theme)) return "shopify";
    if (hasClass("woocommerce") || hasClass("woocommerce-page") || w.wc_add_to_cart_params || w.woocommerce_params || w.wcSettings) return "woocommerce";
    if (w.BCData || document.querySelector('script[src*="bigcommerce.com"]')) return "bigcommerce";
    if (w.Static && w.Static.SQUARESPACE_CONTEXT) return "squarespace";
    if (w.wixBiSession || /wix\.com/i.test(meta("generator") || "")) return "wix";
    if (document.documentElement.getAttribute("data-wf-site")) return "webflow";
    if (w.__framer_importFromPackage || document.querySelector('meta[name="generator"][content*="Framer"]')) return "framer";
    if (/wordpress/i.test(meta("generator") || "") || document.querySelector('link[href*="/wp-content/"],script[src*="/wp-content/"]')) return "wordpress";
    if (w.__NEXT_DATA__ || document.getElementById("__next")) return "nextjs";
    return "custom";
  }
  var PLATFORM = "custom";
  function shopifyRoot() { var r = window.Shopify && window.Shopify.routes && window.Shopify.routes.root; return r || "/"; }
  function shopCurrency() { var c = window.Shopify && window.Shopify.currency; return (c && c.active) || undefined; }

  // ---------- Products ----------
  // An item is { name, product_id, variant_id, sku, price, quantity, image_url, url, brand, categories, currency }.
  function item(o) {
    o = o || {};
    return clean({
      name: o.name || o.title || o.product_name || o.item_name, product_id: o.product_id != null ? String(o.product_id) : o.id != null ? String(o.id) : undefined,
      variant_id: o.variant_id != null ? String(o.variant_id) : undefined, variant: o.variant, sku: o.sku, price: num(o.price), quantity: num(o.quantity) || undefined,
      image_url: abs(o.image_url || o.image), url: abs(o.url), brand: o.brand, categories: o.categories, currency: o.currency
    });
  }
  function ldProducts() {
    var out = [];
    function walk(n, d) {
      if (!n || typeof n !== "object" || d > 6) return;
      if (Array.isArray(n)) { each(n, function (x) { walk(x, d + 1); }); return; }
      var t = [].concat(n["@type"] || []).join(" ");
      if (/\b(Product|ProductGroup|IndividualProduct|ProductModel)\b/.test(t)) out.push(n);
      else { walk(n["@graph"], d + 1); walk(n.mainEntity, d + 1); walk(n.itemListElement, d + 1); }
    }
    each(document.querySelectorAll('script[type="application/ld+json"]'), function (s) { try { walk(JSON.parse(s.textContent), 0); } catch (e) {} });
    return out;
  }
  function fromLd(o) {
    var offers = first(o.offers);
    if (!offers && o.hasVariant) { var v = first(o.hasVariant); offers = v && first(v.offers); }
    offers = offers || {};
    var price = offers.price != null ? offers.price : offers.lowPrice != null ? offers.lowPrice : offers.priceSpecification && first(offers.priceSpecification).price;
    var img = first(o.image); if (img && typeof img === "object") img = img.url || img.contentUrl;
    var brand = first(o.brand); if (brand && typeof brand === "object") brand = brand.name;
    return item({ name: o.name, product_id: o.productID || o.productGroupID || o.sku, sku: o.sku, price: price, currency: offers.priceCurrency, image_url: img, url: o.url || offers.url, brand: brand, categories: o.category ? [].concat(o.category).map(text) : undefined });
  }
  function microProduct() {
    var r = document.querySelector('[itemscope][itemtype*="schema.org/Product"]');
    if (!r) return null;
    function prop(n) {
      var e = r.querySelector('[itemprop="' + n + '"]');
      return e ? (e.getAttribute("content") || e.getAttribute("src") || e.getAttribute("href") || e.textContent) : null;
    }
    return item({ name: prop("name"), sku: prop("sku"), product_id: prop("productID") || prop("sku"), price: prop("price") || prop("lowPrice"), currency: prop("priceCurrency"), image_url: prop("image"), brand: prop("brand") });
  }
  function ogProduct() {
    var t = meta("og:type") || "";
    if (!/product/i.test(t)) return null;
    return item({ name: meta("og:title"), price: meta("product:price:amount") || meta("og:price:amount"), currency: meta("product:price:currency") || meta("og:price:currency"), image_url: meta("og:image"), url: meta("og:url") });
  }
  var ADD_TEXT = /^\s*(\+\s*)?(add to (cart|bag|basket)|add-to-cart|buy now|in den warenkorb|ajouter au panier|añadir al carrito)\b/i;
  function isAddButton(el) {
    if (!el || el.nodeType !== 1) return false;
    var label = text(el.getAttribute("aria-label") || el.value || el.textContent || "").slice(0, 60);
    var name = (el.getAttribute("name") || "") + " " + (typeof el.className === "string" ? el.className : "") + " " + (el.id || "");
    return ADD_TEXT.test(label) || /\badd[-_]?to[-_]?(cart|bag|basket)\b/i.test(name) || el.getAttribute("name") === "add";
  }
  // Last resort for sites with no product data: a page with one heading and an add-to-cart button.
  function pageProduct() {
    var btns = document.querySelectorAll("button, input[type=submit], a[role=button]"), has = false;
    each(btns, function (b) { if (!has && isAddButton(b)) has = true; });
    var h1s = document.querySelectorAll("h1");
    if (!has || h1s.length !== 1) return null;
    var priceEl = document.querySelector('[itemprop=price], [class*="price"]:not([class*="compare"]):not([class*="was"])');
    return item({ name: h1s[0].textContent, price: priceEl ? (priceEl.getAttribute("content") || priceEl.textContent) : undefined, image_url: meta("og:image") });
  }
  function shopifyProduct(cb) {
    var m = /\/products\/([^\/?#]+)/.exec(location.pathname), at = location.pathname;
    if (!m) return;
    fetch(shopifyRoot() + "products/" + m[1] + ".js", { credentials: "same-origin" }).then(function (r) { return r.ok ? r.json() : null; }).then(function (p) {
      // The visitor may have moved to another page (single-page apps) while this loaded.
      if (!p || !p.title || location.pathname !== at) return;
      var vid = new URLSearchParams(location.search).get("variant") || (window.ShopifyAnalytics && window.ShopifyAnalytics.meta && window.ShopifyAnalytics.meta.selectedVariantId);
      var v = null;
      each(p.variants, function (x) { if (String(x.id) === String(vid)) v = x; });
      v = v || (p.variants || [])[0] || {};
      cb(item({
        name: p.title, product_id: p.id, variant_id: v.id, variant: v.title !== "Default Title" ? v.title : undefined, sku: v.sku, price: (v.price != null ? v.price : p.price) / 100,
        image_url: (v.featured_image && v.featured_image.src) || p.featured_image, url: location.origin + (p.url || location.pathname), brand: p.vendor,
        categories: p.type ? [p.type] : undefined, currency: shopCurrency()
      }));
    }).catch(noop);
  }

  // Viewed Product: gather what every source says about this page for a moment, then send one merged event.
  var pv = { path: null, cands: [], timer: 0, sent: false };
  var lastProduct = store.get("pl_last_product", true);
  function offerProduct(p, source, rank) {
    if (!AUTO && source !== "manual") return;
    if (!p || !p.name) return;
    if (pv.path !== location.pathname) pv = { path: location.pathname, cands: [], timer: 0, sent: false };
    if (pv.sent) return;
    pv.cands.push({ p: p, rank: rank, source: source });
    clearTimeout(pv.timer);
    pv.timer = setTimeout(flushProduct, 900);
  }
  function flushProduct() {
    if (pv.sent || !pv.cands.length) return;
    pv.sent = true;
    clearTimeout(pv.timer);
    pv.cands.sort(function (a, b) { return a.rank - b.rank; });
    var p = {};
    pv.cands.forEach(function (c) { fill(p, c.p); });
    if (!p.url) p.url = abs(meta("og:url")) || location.href.split("#")[0];
    if (!p.currency) p.currency = shopCurrency();
    if (pv.wooId) p.product_id = String(pv.wooId);
    lastProduct = p;
    store.set("pl_last_product", p, true);
    var props = fill({ tracked_by: pv.cands[0].source }, p);
    send("Viewed Product", props, p.price);
  }
  function scanProduct() {
    if (!AUTO) return;
    if (PLATFORM === "shopify") shopifyProduct(function (p) { offerProduct(p, "shopify", 1); });
    var lds = ldProducts();
    if (lds.length === 1 || (lds.length > 1 && !/\/(collections?|categor|shop|search)\b/i.test(location.pathname))) offerProduct(fromLd(lds[0]), "structured_data", 2);
    offerProduct(microProduct(), "microdata", 3);
    offerProduct(ogProduct(), "open_graph", 4);
    if (!pv.cands.length && PLATFORM !== "shopify") offerProduct(pageProduct(), "page", 6);
    if (PLATFORM === "woocommerce" && hasClass("single-product") && pv.path === location.pathname) {
      // WooCommerce's cart uses the product's post id; use it so adds match this product.
      var field = document.querySelector('form.cart [name="add-to-cart"]'), m = /\bpostid-(\d+)\b/.exec(document.body.className);
      pv.wooId = (field && field.value) || (m && m[1]) || null;
    }
  }

  // ---------- Cart ----------
  // A cart is { items, total, currency, item_count, checkout_url }.
  var CART = "pl_cart";
  function cartFrom(c) {
    var items = (c.items || []).map(item).filter(function (i) { return i.name || i.product_id; });
    var total = num(c.total != null ? c.total : c.value);
    if (total == null && items.length) total = Math.round(items.reduce(function (s, i) { return s + (i.price || 0) * (i.quantity || 1); }, 0) * 100) / 100;
    return clean({ items: items, total: total, currency: c.currency || shopCurrency(), item_count: c.item_count || items.reduce(function (s, i) { return s + (i.quantity || 1); }, 0), checkout_url: abs(c.checkout_url) || defaultCartUrl(items) });
  }
  function defaultCartUrl(items) {
    if (PLATFORM === "shopify") {
      // A cart permalink rebuilds the same cart on any device, so reminder emails can link to it.
      var parts = (items || []).filter(function (i) { return i.variant_id; }).map(function (i) { return i.variant_id + ":" + (i.quantity || 1); });
      return location.origin + shopifyRoot() + "cart" + (parts.length ? "/" + parts.join(",") : "");
    }
    var wc = window.wc_add_to_cart_params;
    if (wc && wc.cart_url) return abs(wc.cart_url);
    return location.origin + "/cart";
  }
  function saveCart(c) { if (c && c.items && c.items.length) store.set(CART, fill({ at: now() }, c), true); return c; }
  function savedCart() { var c = store.get(CART, true); return c && now() - c.at < 86400000 ? c : null; }
  function shopifyCart(cb) {
    fetch(shopifyRoot() + "cart.js", { credentials: "same-origin" }).then(function (r) { return r.ok ? r.json() : null; }).then(function (c) {
      if (!c || !c.items) return cb(null);
      cb(saveCart(cartFrom({
        items: c.items.map(function (i) { return { name: i.product_title || i.title, product_id: i.product_id, variant_id: i.variant_id || i.id, variant: i.variant_title, sku: i.sku, price: (i.final_price != null ? i.final_price : i.price) / 100, quantity: i.quantity, image_url: i.image, url: i.url, brand: i.vendor }; }),
        total: c.total_price / 100, currency: c.currency, item_count: c.item_count
      })));
    }).catch(function () { cb(null); });
  }
  function wooStoreCart(cb) {
    var root = (window.wpApiSettings && window.wpApiSettings.root) || location.origin + "/wp-json/";
    fetch(root + "wc/store/v1/cart", { credentials: "same-origin" }).then(function (r) { return r.ok ? r.json() : null; }).then(function (c) {
      cb(c && c.items ? saveCart(wooCart(c)) : null);
    }).catch(function () { cb(null); });
  }
  function wooCart(c) {
    var minor = function (v, p) { var u = p && p.currency_minor_unit != null ? Number(p.currency_minor_unit) : 2; return v == null ? undefined : Number(v) / Math.pow(10, u); };
    return cartFrom({
      items: c.items.map(function (i) { return { name: i.name, product_id: i.id, sku: i.sku, price: minor(i.prices && i.prices.price, i.prices), quantity: i.quantity, image_url: i.images && i.images[0] && i.images[0].src, url: i.permalink }; }),
      total: c.totals ? minor(c.totals.total_price, c.totals) : undefined, currency: c.totals && c.totals.currency_code, item_count: c.items_count
    });
  }
  function refreshCart(cb) {
    cb = cb || noop;
    if (PLATFORM === "shopify") shopifyCart(cb);
    else if (PLATFORM === "woocommerce") wooStoreCart(cb);
    else cb(null);
  }

  // Added to Cart: the network request, the form, the store's own event and Google Analytics can all report one add.
  // Signals that describe the same add are merged; different products added quickly stay separate events.
  var ADD_RANK = { manual: 0, network: 1, woocommerce: 2, data_layer: 3, form: 4, click: 5 };
  var pendingAdds = [], recentAdds = [];
  function ids(x) {
    var r = [];
    if (x) each([x.variant_id, x.product_id, x.sku], function (v) { if (v != null && v !== "") r.push(String(v)); });
    return r;
  }
  function overlap(a, b) { return a.some(function (v) { return b.indexOf(v) >= 0; }); }
  function sameName(a, b) { return !!(a && b && text(a).toLowerCase() === text(b).toLowerCase()); }
  function addKey(it) { return ids(it)[0] || text(it.name).toLowerCase(); }
  // refresh: the store has finished adding, so its cart can be read now.
  function addToCart(it, source, cart, refresh) {
    if (!AUTO && source !== "manual") return;
    it = it || {};
    var here = (lastProduct && pv.path === location.pathname) || pv.cands.length;
    var mine = ids(it);
    if (!mine.length && !it.name && !here) return;
    var bucket = null;
    for (var i = pendingAdds.length - 1; i >= 0 && !bucket; i--) {
      var b = pendingAdds[i];
      if (overlap(mine, b.ids) || sameName(it.name, b.name)) bucket = b;
    }
    // Nothing identifies it, or another kind of signal for an add we're already holding: same add.
    if (!bucket && pendingAdds.length) {
      var last = pendingAdds[pendingAdds.length - 1];
      if ((!mine.length && !it.name) || last.sources.indexOf(source) < 0) bucket = last;
    }
    if (!bucket) { bucket = { parts: [], sources: [], ids: [], name: null, cart: null, timer: 0 }; pendingAdds.push(bucket); }
    bucket.parts.push({ it: it, rank: ADD_RANK[source] != null ? ADD_RANK[source] : 9, source: source });
    bucket.sources.push(source);
    bucket.ids = bucket.ids.concat(mine);
    bucket.name = bucket.name || it.name || null;
    if (cart) bucket.cart = cart;
    else if (refresh && (PLATFORM === "shopify" || PLATFORM === "woocommerce")) refreshCart(function (c) { if (c && pendingAdds.indexOf(bucket) >= 0) bucket.cart = c; });
    clearTimeout(bucket.timer);
    bucket.timer = setTimeout(function () { flushAdd(bucket); }, 1200);
  }
  function flushAdd(only) {
    pendingAdds.slice().forEach(function (a) { if (!only || a === only) sendAdd(a); });
  }
  function sendAdd(a) {
    pendingAdds = pendingAdds.filter(function (x) { return x !== a; });
    clearTimeout(a.timer);
    a.parts.sort(function (x, y) { return x.rank - y.rank; });
    var it = {};
    a.parts.forEach(function (x) { fill(it, x.it); });
    // On a product page, the product we already read fills what the add didn't say, unless the add is clearly another product.
    if (lastProduct) {
      var mine = ids(it), theirs = ids(lastProduct), here = pv.path === location.pathname;
      var conflict = (mine.length && theirs.length && !overlap(mine, theirs)) || (it.name && !sameName(it.name, lastProduct.name));
      if (overlap(mine, theirs) || (here && !conflict)) { var q = it.quantity; fill(it, lastProduct); if (q) it.quantity = q; }
    }
    if (!it.name && !it.product_id) return;
    // The click fallback and a slow cart request can report the same add seconds apart.
    var key = addKey(it), t = now();
    recentAdds = recentAdds.filter(function (r) { return t - r.at < 10000; });
    if (recentAdds.some(function (r) { return r.key === key && (r.click || a.sources.indexOf("click") >= 0); })) return;
    recentAdds.push({ key: key, at: t, click: a.sources.indexOf("click") >= 0 });
    var cart = a.cart || savedCart() || cartFrom({ items: [] });
    if (!a.cart) {
      // No cart data from the store: keep a running cart from the adds we've seen.
      var items = (cart.items || []).slice();
      items.push(fill({ quantity: it.quantity || 1 }, it));
      cart = saveCart(cartFrom({ items: items, currency: it.currency }));
    }
    var props = fill({ tracked_by: a.parts[0].source, quantity: it.quantity || 1 }, it);
    props.items = cart.items; props.item_names = (cart.items || []).map(function (i) { return i.name; }).filter(Boolean);
    props.cart_total = cart.total; props.item_count = cart.item_count; props.checkout_url = cart.checkout_url;
    if (!props.currency) props.currency = cart.currency;
    send("Added to Cart", clean(props), cart.total != null ? cart.total : (it.price || 0) * (it.quantity || 1));
  }

  var ADD_URL = /\/cart\/add(\.js)?(\?|$)|\/wc\/store(\/v\d)?\/cart\/add-item|[?&]wc-ajax=add_to_cart|\/remote\/v1\/cart\/add|\/api\/storefront\/carts|\/api\/commerce\/shopping-cart\/entries|\/cart\/(add|items|lines)(\.json)?(\?|$)|add[-_]?to[-_]?cart/i;
  function isAddUrl(u, method, body) {
    if (method !== "POST" || String(u || "").indexOf(BASE) === 0) return false;
    // WooCommerce block carts batch several cart changes into one request; only count batches that add.
    if (/\/wc\/store(\/v\d)?\/batch/.test(u)) return typeof body === "string" && body.indexOf("add-item") >= 0;
    return ADD_URL.test(u || "");
  }
  function readBody(b) {
    var o = {};
    try {
      if (!b) return o;
      if (typeof b === "string") { try { return JSON.parse(b) || o; } catch (e) { new URLSearchParams(b).forEach(function (v, k) { o[k] = v; }); return o; } }
      if (typeof FormData !== "undefined" && b instanceof FormData) { b.forEach(function (v, k) { if (typeof v === "string") o[k] = v; }); return o; }
      if (typeof URLSearchParams !== "undefined" && b instanceof URLSearchParams) { b.forEach(function (v, k) { o[k] = v; }); return o; }
    } catch (e) {}
    return o;
  }
  function netAdded(url, res, body) {
    var b = readBody(body), qty = num(b.quantity || b.qty || (b.items && b.items[0] && b.items[0].quantity)) || 1;
    var it = { quantity: qty }, cart = null;
    if (PLATFORM === "shopify" && res) {
      var li = res.items ? res.items[0] : res;
      if (li && (li.product_id || li.variant_id)) it = { name: li.product_title || li.title, product_id: li.product_id, variant_id: li.variant_id || li.id, variant: li.variant_title, sku: li.sku, price: (li.final_price != null ? li.final_price : li.price) / 100, quantity: li.quantity || qty, image_url: li.image, url: li.url, brand: li.vendor };
    } else if (res && res.items && res.totals) {
      // WooCommerce Store API returns the whole cart.
      cart = saveCart(wooCart(res));
      var pid = String(b.id || b.product_id || "");
      each(cart.items, function (i) { if (i.product_id === pid) it = fill({ quantity: qty }, i); });
    } else {
      var id = b.id || b.product_id || b["add-to-cart"] || b.productId || b.variantId;
      if (id) it.product_id = String(id);
    }
    addToCart(item(it), "network", cart, true);
  }
  (function hookNetwork() {
    if (!AUTO) return;
    var f = window.fetch;
    if (typeof f === "function") {
      window.fetch = function (input, init) {
        var p = f.apply(window, arguments);
        try {
          var url = typeof input === "string" ? input : input && (input.url || String(input));
          var method = String((init && init.method) || (input && input.method) || "GET").toUpperCase();
          var body = init && init.body;
          if (isAddUrl(url, method, body)) {
            p.then(function (r) {
              if (!r || !r.ok) return;
              r.clone().json().then(function (j) { netAdded(url, j, body); }, function () { netAdded(url, null, body); });
            }, noop);
          }
        } catch (e) {}
        return p;
      };
    }
    var X = window.XMLHttpRequest;
    if (X && X.prototype) {
      var open = X.prototype.open, sendX = X.prototype.send;
      X.prototype.open = function (m, u) { try { this.__pl = { m: String(m).toUpperCase(), u: String(u) }; } catch (e) {} return open.apply(this, arguments); };
      X.prototype.send = function (body) {
        try {
          var x = this, info = x.__pl;
          if (info && isAddUrl(info.u, info.m, body)) {
            // One listener per request: if the page reuses this XHR, a later request must not count as an add.
            var done = function () {
              x.removeEventListener("loadend", done);
              if (x.__pl !== info) return;
              x.__pl = null;
              if (x.status < 200 || x.status >= 300) return;
              var j = null; try { j = x.responseType === "json" ? x.response : JSON.parse(x.responseText); } catch (e) {}
              netAdded(info.u, j, body);
            };
            x.addEventListener("loadend", done);
          }
        } catch (e) {}
        return sendX.apply(this, arguments);
      };
    }
  })();

  // ---------- Checkout and orders ----------
  // Sources name products differently (post id, SKU, GA item id), so one checkout is one cart size per half hour.
  function cartSig(c) { return String(c.item_count || (c.items || []).reduce(function (s, i) { return s + (i.quantity || 1); }, 0)); }
  // On Shopify the checkout pixel is the one source for checkouts and orders: it sees the email typed at checkout.
  function pixelOwns(source) { return PLATFORM === "shopify" && source !== "manual"; }
  function startCheckout(cart, source, force) {
    if ((!AUTO && source !== "manual") || pixelOwns(source)) return;
    if (!cart || !cart.items || !cart.items.length) cart = savedCart() || cart;
    if (!cart) return;
    var sig = cartSig(cart), last = store.get("pl_sc", true);
    if (!force && last && last.sig === sig && now() - last.at < 30 * 60000) return;
    store.set("pl_sc", { sig: sig, at: now() }, true);
    var props = fill({ tracked_by: source, item_names: (cart.items || []).map(function (i) { return i.name; }).filter(Boolean) }, cart);
    delete props.at;
    send("Started Checkout", clean(props), cart.total);
  }
  function placeOrder(o, source) {
    if ((!AUTO && source !== "manual") || pixelOwns(source)) return;
    var cart = cartFrom({ items: o.items || [], total: o.total != null ? o.total : o.value, currency: o.currency });
    var id = o.order_id || o.id || o.transaction_id;
    id = id != null ? String(id) : "";
    var done = store.get("pl_orders") || [];
    if (id && done.indexOf(id) >= 0) return;
    if (id) store.set("pl_orders", done.concat([id]).slice(-20));
    if (o.email && EMAIL_RE.test(String(o.email))) api.identify({ email: String(o.email).trim() }, "order");
    var props = clean(fill({ tracked_by: source, order_id: id || undefined, item_names: cart.items.map(function (i) { return i.name; }).filter(Boolean), discount_codes: o.discount_codes, shipping: num(o.shipping), tax: num(o.tax) }, cart));
    delete props.checkout_url;
    send("Placed Order", props, cart.total, id ? "order:" + id : undefined);
    store.del(CART, true); store.del("pl_sc", true);
  }
  function startCheckoutNow(source) {
    if (pixelOwns(source)) return;
    var c = savedCart();
    if (c) startCheckout(c, source); else refreshCart(function (rc) { if (rc) startCheckout(rc, source); });
  }
  function scanCheckout() {
    if (!AUTO) return;
    var path = location.pathname;
    if (PLATFORM === "woocommerce") {
      if (hasClass("woocommerce-order-received") || /\/order-received\//.test(path)) return wooOrder();
      if (hasClass("woocommerce-checkout") || document.querySelector(".wp-block-woocommerce-checkout")) {
        wooStoreCart(function (c) { startCheckout(c || wooDomCart(), "woocommerce"); });
      }
      return;
    }
    if (PLATFORM === "shopify") return;
    if (/\/checkout(s)?(\/|$)/i.test(path) && !/thank|order-received|confirm|success|complete|receipt/i.test(path)) startCheckoutNow("page");
  }
  function wooDomCart() {
    var items = [];
    each(document.querySelectorAll(".woocommerce-checkout-review-order-table .cart_item"), function (r) {
      var n = r.querySelector(".product-name"), q = r.querySelector(".product-quantity"), t = r.querySelector(".product-total .amount");
      var qty = num(q && q.textContent) || 1, tot = num(t && t.textContent);
      items.push({ name: n ? text(n.childNodes[0] && n.childNodes[0].textContent) || text(n.textContent) : "", quantity: qty, price: tot != null ? tot / qty : undefined });
    });
    var total = document.querySelector(".order-total .amount");
    return cartFrom({ items: items, total: total ? num(total.textContent) : undefined });
  }
  function wooOrder() {
    var m = /order-received\/(\d+)/.exec(location.pathname);
    var idEl = document.querySelector(".woocommerce-order-overview__order strong, .wc-block-order-confirmation-summary-list-item__value");
    var id = m ? m[1] : idEl ? text(idEl.textContent).replace(/^#/, "") : "";
    var emailEl = document.querySelector(".woocommerce-order-overview__email strong, .woocommerce-customer-details--email");
    var totalEl = document.querySelector(".woocommerce-order-overview__total .amount, .woocommerce-table--order-details tfoot tr:last-child .amount");
    var items = [];
    each(document.querySelectorAll(".woocommerce-table--order-details tbody tr.order_item, .wc-block-order-confirmation-totals__table tbody tr"), function (r) {
      var n = r.querySelector(".product-name a, .wc-block-order-confirmation-totals__product a") || r.querySelector(".product-name"), q = r.querySelector(".product-quantity"), t = r.querySelector(".product-total .amount, .wc-block-order-confirmation-totals__total");
      var qty = num(q && q.textContent.replace(/[×x]/g, "")) || 1, tot = num(t && t.textContent);
      if (n) items.push({ name: text(n.textContent).replace(/\s*[×x]\s*\d+\s*$/, ""), url: n.getAttribute("href") || undefined, quantity: qty, price: tot != null ? tot / qty : undefined });
    });
    if (!id && !items.length) return;
    placeOrder({ order_id: id, email: emailEl ? text(emailEl.textContent) : undefined, total: totalEl ? num(totalEl.textContent) : undefined, items: items }, "woocommerce");
  }

  // Google Analytics ecommerce events in the data layer (GA4 and Universal Analytics), which many stores already send.
  function gaItem(i, cur) { i = i || {}; return item({ name: i.item_name || i.name, product_id: i.item_id || i.id, sku: i.item_id || i.id, variant: i.item_variant || i.variant, price: i.price, quantity: i.quantity, brand: i.item_brand || i.brand, categories: i.item_category || i.category ? [i.item_category || i.category] : undefined, currency: cur }); }
  function onLayer(e) {
    if (!e || typeof e !== "object") return;
    var name, p;
    if (e.length != null && e[0] === "event") { name = e[1]; p = e[2] || {}; }
    else if (e.event) { name = e.event; p = e.ecommerce || e; }
    var ec = e.ecommerce;
    if (!name && !ec) return;
    if (ec && !p) p = ec;
    var cur = p && (p.currency || p.currencyCode);
    var items = (p && p.items) || [];
    if (name === "view_item" && items[0]) offerProduct(gaItem(items[0], cur), "data_layer", 5);
    else if (name === "add_to_cart" && items[0]) addToCart(gaItem(items[0], cur), "data_layer");
    else if (name === "begin_checkout" && items.length) startCheckout(cartFrom({ items: items.map(function (i) { return gaItem(i, cur); }), total: p.value, currency: cur }), "data_layer");
    else if (name === "purchase" && (p.transaction_id || items.length)) placeOrder({ order_id: p.transaction_id, total: p.value, currency: cur, tax: p.tax, shipping: p.shipping, items: items.map(function (i) { return gaItem(i, cur); }), discount_codes: p.coupon ? [p.coupon] : undefined }, "data_layer");
    else if (ec) {
      var ua = ec.detail || ec.add || ec.checkout || ec.purchase, prods = (ua && ua.products) || [];
      cur = ec.currencyCode;
      if (ec.detail && prods[0]) offerProduct(gaItem(prods[0], cur), "data_layer", 5);
      else if (ec.add && prods[0]) addToCart(gaItem(prods[0], cur), "data_layer");
      else if (ec.checkout && prods.length) startCheckout(cartFrom({ items: prods.map(function (i) { return gaItem(i, cur); }), currency: cur }), "data_layer");
      else if (ec.purchase && ec.purchase.actionField) placeOrder({ order_id: ec.purchase.actionField.id, total: ec.purchase.actionField.revenue, currency: cur, items: prods.map(function (i) { return gaItem(i, cur); }) }, "data_layer");
    }
  }
  function hookLayer() {
    var dl = window.dataLayer = window.dataLayer || [];
    if (!Array.isArray(dl) || dl.__pl) return;
    dl.__pl = true;
    each(dl.slice(), function (e) { try { onLayer(e); } catch (x) {} });
    var push = dl.push;
    dl.push = function () {
      for (var i = 0; i < arguments.length; i++) { try { onLayer(arguments[i]); } catch (x) {} }
      return push.apply(dl, arguments);
    };
  }

  // ---------- Page signals ----------
  // Fields for someone else's address (gift recipients, referrals) aren't the visitor's own email.
  var OTHER_PERSON = /recipient|friend|gift|refer|invite|colleague|someone|to[-_ ]?email|send[-_ ]?to/i;
  function isEmailInput(t) {
    if (!t || t.tagName !== "INPUT" || t.type === "password" || t.type === "hidden") return false;
    var desc = (t.name || "") + " " + (t.id || "") + " " + (t.getAttribute("autocomplete") || "") + " " + (t.getAttribute("placeholder") || "") + " " + (t.getAttribute("aria-label") || "");
    if (t.labels) each(t.labels, function (l) { desc += " " + l.textContent; });
    // data-dumbmail-ignore on a field or form opts it out.
    if (OTHER_PERSON.test(desc) || (t.closest && t.closest("[data-dumbmail-ignore]"))) return false;
    return t.type === "email" || /e-?mail/i.test((t.name || "") + " " + (t.id || "") + " " + (t.getAttribute("autocomplete") || ""));
  }
  function inOwnUi(el) { return !!(el && el.closest && el.closest("[data-pl-own]")); }
  // Only forms where people give their own email: sign-ups, logins, accounts, checkout, contact. Never staff pages.
  var OWN_FORM = /subscri|newsletter|sign[ -]?up|register|registration|join|log[ -]?in|sign[ -]?in|account|checkout|contact|notify|wait[ -]?list|back in stock|restock|quote|book|reserv/i;
  var STAFF_PAGE = /(^|\/)(wp-admin|admin|dashboard|staff|manage|backend)(\/|$)/i;
  function optedIn(el) { return !!(el && el.closest && el.closest("[data-dumbmail-identify]")); }
  function formContext(f) {
    if (!f) return "";
    var c = [f.getAttribute("action"), f.id, typeof f.className === "string" ? f.className : "", f.getAttribute("name"), f.getAttribute("aria-label")].join(" ");
    each(f.querySelectorAll("button, input[type=submit], h1, h2, h3, h4, legend"), function (x) { c += " " + text(x.value || x.textContent).slice(0, 60); });
    return c;
  }
  function emailFields(f) { var r = []; each(f.querySelectorAll("input"), function (i) { if (isEmailInput(i)) r.push(i); }); return r; }
  // While typing: only checkout email boxes, so an abandoned checkout still knows who it was.
  function checkoutField(input) {
    if (optedIn(input)) return true;
    var f = input.form || (input.closest && input.closest("form"));
    if (f && OTHER_PERSON.test(formContext(f))) return false;
    return /billing|checkout/i.test((input.name || "") + " " + (input.id || "")) || /checkout/i.test(formContext(f) + " " + location.pathname);
  }
  // On submit: a form with one email box that's clearly the visitor's own.
  function ownForm(f) {
    if (optedIn(f)) return true;
    var ctx = formContext(f);
    return emailFields(f).length === 1 && OWN_FORM.test(ctx) && !OTHER_PERSON.test(ctx);
  }
  function capture(input, how) {
    if (!CAPTURE || inOwnUi(input) || STAFF_PAGE.test(location.pathname)) return;
    var v = text(input.value);
    if (!EMAIL_RE.test(v)) return;
    var p = profile();
    if (!TEST && p.email && String(p.email).toLowerCase() === v.toLowerCase()) return;
    api.identify({ email: v }, how);
  }
  function hookPage() {
    document.addEventListener("change", function (e) { if (isEmailInput(e.target) && checkoutField(e.target)) capture(e.target, "email_field"); }, true);
    document.addEventListener("submit", function (e) {
      var f = e.target;
      if (!f || !f.querySelectorAll || inOwnUi(f)) return;
      if (ownForm(f)) each(emailFields(f), function (i) { capture(i, "form"); });
      if (!AUTO) return;
      var action = f.getAttribute("action") || "";
      // Classic (non-AJAX) add-to-cart forms reload the page, so record the add before they do.
      if (ADD_URL.test(action) || f.querySelector('[name="add-to-cart"]')) {
        var fd = {}; try { new FormData(f).forEach(function (v, k) { if (typeof v === "string") fd[k] = v; }); } catch (x) {}
        addToCart(item({ variant_id: PLATFORM === "shopify" ? fd.id : undefined, product_id: fd["add-to-cart"], quantity: fd.quantity }), "form");
      }
      var sub = e.submitter;
      if ((sub && sub.getAttribute("name") === "checkout") || /\/checkout/.test(action)) startCheckoutNow("checkout_button");
    }, true);
    document.addEventListener("click", function (e) {
      if (!AUTO) return;
      var el = e.target && e.target.closest && e.target.closest("button, a, input[type=submit], input[type=button], [role=button]");
      if (!el || inOwnUi(el)) return;
      var href = el.getAttribute("href") || "", label = text(el.getAttribute("aria-label") || el.value || el.textContent).slice(0, 40);
      if (el.getAttribute("name") === "checkout" || /\/checkout(s)?(\/|$|\?)/.test(href) || /^(secure )?(proceed to |go to |continue to )?check ?out( now| securely)?$/i.test(label)) { startCheckoutNow("checkout_button"); return; }
      if (isAddButton(el) && lastProduct && pv.path === location.pathname && PLATFORM !== "shopify" && PLATFORM !== "woocommerce" && !el.disabled) {
        // Fallback for stores whose cart requests we don't recognize: count the click if nothing else reports the add.
        var at = now(), form = el.form || (el.closest && el.closest("form"));
        if (form && form.checkValidity && !form.checkValidity()) return;
        setTimeout(function () {
          if (pendingAdds.length || recentAdds.some(function (r) { return r.at >= at; })) return;
          addToCart(item({}), "click");
        }, 1500);
      }
    }, true);
    window.addEventListener("pagehide", function () { flushProduct(); flushAdd(); });
    document.addEventListener("visibilitychange", function () { if (document.visibilityState === "hidden") { flushProduct(); flushAdd(); } });
    // WooCommerce's own add-to-cart event (archive pages and AJAX buttons).
    function wooHook() {
      var $ = window.jQuery;
      if (!$ || !$.fn || wooHook.done) return;
      wooHook.done = true;
      $(document.body).on("added_to_cart", function (ev, frags, hash, $b) {
        var b = $b && $b[0], name;
        if (b) {
          var m = /[“"](.+?)[”"]/.exec(b.getAttribute("aria-label") || "");
          var card = b.closest && b.closest(".product, li");
          var t = card && card.querySelector(".woocommerce-loop-product__title, h2, h3");
          name = m ? m[1] : t ? t.textContent : undefined;
        }
        addToCart(item({ name: name, product_id: b && b.getAttribute("data-product_id"), sku: b && b.getAttribute("data-product_sku"), quantity: b && b.getAttribute("data-quantity") }), "woocommerce", null, true);
      });
    }
    wooHook();
    window.addEventListener("load", wooHook);
  }

  // Single-page apps change the URL without reloading: treat each new path as a new page.
  var lastPath = location.pathname;
  function onRoute() {
    if (location.pathname === lastPath) return;
    lastPath = location.pathname;
    // offerProduct starts a fresh page view on its own; the new page may already have reported its product.
    if (pv.path !== location.pathname) pv = { path: location.pathname, cands: [], timer: 0, sent: false };
    setTimeout(function () { scanProduct(); scanCheckout(); }, 700);
    setTimeout(function () { if (!pv.cands.length) scanProduct(); }, 2500);
    countPageView();
    if (formsCache) runForms(formsCache);
    setTimeout(loadReviews, 700);
  }
  function hookRoutes() {
    each(["pushState", "replaceState"], function (m) {
      var orig = history[m];
      if (typeof orig !== "function") return;
      history[m] = function () { var r = orig.apply(this, arguments); setTimeout(onRoute, 0); return r; };
    });
    window.addEventListener("popstate", function () { setTimeout(onRoute, 0); });
  }

  // Run calls queued before the script loaded: window.dumbmail = window.dumbmail || []; dumbmail.push(["track", ...]) (window.postlark still works)
  var queued = ONLY_FORM ? null : window.dumbmail || window.postlark;
  if (!ONLY_FORM) window.dumbmail = window.postlark = api;
  function runQueued() {
    if (ONLY_FORM) return;
    var q = (queued && (queued.q || (Array.isArray(queued) ? queued : null))) || window._dumbmailq || window._postlarkq || [];
    for (var i = 0; i < q.length; i++) { var c = q[i]; if (c && api[c[0]]) api[c[0]].apply(api, Array.prototype.slice.call(c, 1)); }
    window._dumbmailq = window._postlarkq = { push: function (c) { if (c && api[c[0]]) api[c[0]].apply(api, Array.prototype.slice.call(c, 1)); } };
    if (!TEST) post("/client/ping", { url: location.href });
    if (!store.get("pl_active", true)) { store.set("pl_active", 1, true); if (!TEST) send("Active on Site", { url: location.href, title: document.title, referrer: document.referrer || null }); }
  }

  // ---------- Forms ----------
  // The shared form renderer (src/form_render.js); the hosted page and the dashboard preview draw forms with it too.
  var FORMS_UI = __RENDERER__;
  var formsCache = null, showing = null, open = {}, scheduled = {}, teasers = {};
  function loadForms(cb) {
    if (formsCache) return cb(formsCache);
    fetch(BASE + "/client/forms?k=" + encodeURIComponent(KEY), { credentials: "omit" })
      .then(function (r) { return r.json(); })
      .then(function (j) {
        if (j.error) { if (TEST) test.warn("key", "DumbMail didn't accept this snippet's key. Copy the snippet from your Website page again."); return; }
        formsCache = (j.data || []).map(withVersion); cb(formsCache);
      })
      .catch(function () { if (TEST) test.warn("network", "This page couldn't reach " + BASE + ". A Content-Security-Policy, firewall or ad blocker may be blocking it."); });
  }
  function esc(s) { return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]; }); }
  function days(n) { return n * 86400000; }
  function closedKey(f) { return "pl_form_closed_" + f.id; }
  function doneKey(f) { return "pl_form_done_" + f.id; }
  // The teaser after the visitor closed the form, and when they hid it with its own ×.
  function teaserKey(f) { return "pl_form_teaser_" + f.id; }
  function teaserHiddenKey(f) { return "pl_form_teaser_hidden_" + f.id; }
  function targeting(f) { return (f.config && f.config.targeting) || {}; }
  function freqDays(f) { var t = targeting(f); return t.frequency_days == null ? 7 : Number(t.frequency_days) || 0; }
  function floating(f) { return f.kind === "popup" || f.kind === "flyout"; }

  // Pages seen in this visit (single-page app route changes count too), and when this browser first ran the script.
  var PAGE_VIEWS = "pl_page_views", FIRST_SEEN = "pl_first_seen";
  function countPageView() { store.set(PAGE_VIEWS, (Number(store.get(PAGE_VIEWS, true)) || 0) + 1, true); }
  function pageViews() { return Number(store.get(PAGE_VIEWS, true)) || 1; }
  function firstSeen() { var t = Number(store.get(FIRST_SEEN)); if (!t) { t = now(); store.set(FIRST_SEEN, t); } return t; }

  // A/B test: each visitor keeps one version (a = the form itself) and sees its content.
  var VERSION_KEYS = ["headline", "body", "button_text", "image_url", "image_alt", "no_thanks_text", "success_message", "success_body", "discount_code"];
  function withVersion(f) {
    var base = f.config || {}, vs = base.variants || [];
    if (!vs.length) return { id: f.id, kind: f.kind, config: base, version: null };
    var ids = ["a"].concat(vs.map(function (x) { return x.id; })), key = "pl_form_var_" + f.id, v = store.get(key);
    if (ids.indexOf(v) < 0) { v = ids[Math.floor(Math.random() * ids.length)]; store.set(key, v); }
    var c = {};
    for (var k in base) c[k] = base[k];
    vs.forEach(function (x) {
      if (x.id === v) VERSION_KEYS.forEach(function (k) { if (x[k] != null && String(x[k]).trim()) c[k] = x[k]; });
    });
    return { id: f.id, kind: f.kind, config: c, version: v };
  }

  function mountForm(f, container) {
    var opts = {
      test: !!TEST,
      submit: function (d) {
        if (TEST) {
          // Test mode: show the thank-you without subscribing anyone or changing who this browser is.
          if (d.step !== "phone") test.check("identify", { email: d.email || null, how: "signup_form", summary: "Sign-up form works (" + (d.email || d.phone_number) + " was not subscribed in test mode)" });
          return Promise.resolve({ ok: true });
        }
        return post("/client/subscribe", d).then(function (r) { return r.json(); });
      },
      onSuccess: function (d) {
        if (TEST) return;
        store.set(doneKey(f), now());
        store.del(teaserKey(f));
        hideTeaser(f);
        var who = {};
        each(["email", "phone_number", "first_name", "last_name"], function (k) { if (d[k]) who[k] = d[k]; });
        api.identify(who, "signup_form");
      },
      onClose: function (reason) {
        delete open[f.id];
        if (showing === f.id) showing = null;
        if (reason === "dismiss") {
          store.set(closedKey(f), now());
          if (teaserOf(f) && !store.get(doneKey(f))) store.set(teaserKey(f), now());
        }
        syncTeaser(f);
      }
    };
    if (container) opts.container = container;
    return FORMS_UI.mount(f, opts);
  }

  // Test-mode views aren't counted, so testing doesn't skew A/B results.
  function view(f) { if (TEST) return; fetch(BASE + "/client/forms/" + f.id + "/view?k=" + encodeURIComponent(KEY) + (f.version ? "&v=" + encodeURIComponent(f.version) : ""), { method: "POST", credentials: "omit", keepalive: true }).catch(function () {}); }

  function renderEmbeds(f) {
    var slots = document.querySelectorAll('[data-dumbmail-form="' + f.id + '"], [data-postlark-form="' + f.id + '"]');
    if (!slots.length && ONLY_FORM && ME && ME.parentNode) slots = [ME.parentNode];
    for (var i = 0; i < slots.length; i++) { if (slots[i].getAttribute("data-pl-rendered")) continue; slots[i].setAttribute("data-pl-rendered", "1"); mountForm(f, slots[i]); view(f); }
  }
  // force: open now whatever the targeting says (teaser, data-dumbmail-open, showForm, test panel), unless it's already open.
  function show(f, force) {
    if (f.kind === "embed") return renderEmbeds(f);
    if (!floating(f) || open[f.id] || !document.body) return;
    if (showing && !force) return;
    hideTeaser(f);
    showing = f.id;
    open[f.id] = mountForm(f);
    view(f);
  }

  // Targeting that depends on the page and the visitor, not on whether they've seen the form.
  function pageOk(f) {
    var t = targeting(f);
    if (store.get(doneKey(f))) return false;
    if (t.hide_for_subscribers !== false && identified()) return false;
    var mobile = window.matchMedia && window.matchMedia("(max-width: 768px)").matches;
    if (t.devices === "mobile" && !mobile) return false;
    if (t.devices === "desktop" && mobile) return false;
    var href = location.href.toLowerCase();
    var inc = (t.url_contains || []).filter(Boolean), exc = (t.url_excludes || []).filter(Boolean);
    if (inc.length && !inc.some(function (s) { return href.indexOf(String(s).toLowerCase()) >= 0; })) return false;
    if (exc.some(function (s) { return href.indexOf(String(s).toLowerCase()) >= 0; })) return false;
    var start = t.start_at && Date.parse(t.start_at), end = t.end_at && Date.parse(t.end_at);
    if ((start && now() < start) || (end && now() > end)) return false;
    return true;
  }
  function eligible(f) {
    var t = targeting(f);
    if (!pageOk(f)) return false;
    var closed = store.get(closedKey(f));
    if (closed && now() - closed < days(freqDays(f))) return false;
    if (pageViews() < (Number(t.page_views) || 1)) return false;
    if (t.visitors === "new" || t.visitors === "returning") {
      var returning = now() - firstSeen() > 30 * 60000;
      if (returning !== (t.visitors === "returning")) return false;
    }
    return true;
  }

  function schedule(f) {
    var t = targeting(f);
    if (t.trigger === "manual" || scheduled[f.id] || !eligible(f)) return;
    scheduled[f.id] = true;
    var fired = false;
    // Single-page apps may have moved to a page the form doesn't target by the time it fires; try again on a later page.
    function fire() { if (fired) return; fired = true; if (eligible(f)) show(f); else scheduled[f.id] = false; }
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
      var c = f.config || {}, delay = c.delay_seconds == null || c.delay_seconds === "" ? 5 : Number(c.delay_seconds);
      setTimeout(fire, (isFinite(delay) && delay >= 0 ? delay : 5) * 1000);
    }
  }

  // Teaser tab: "always" shows it on eligible pages until the form opens; after the visitor closes the form
  // without signing up it stays on every page until they sign up or hide it with its ×.
  function teaserOf(f) { var t = f.config && f.config.teaser; return floating(f) && t && typeof t === "object" && t.enabled === true ? t : null; }
  function teaserWanted(f) {
    var t = teaserOf(f);
    if (!t || ONLY_FORM || open[f.id] || store.get(doneKey(f))) return false;
    var hidden = store.get(teaserHiddenKey(f));
    if (hidden && now() - hidden < days(freqDays(f))) return false;
    if (store.get(teaserKey(f))) return pageOk(f);
    return t.show === "always" && eligible(f);
  }
  function hideTeaser(f) { if (teasers[f.id]) { teasers[f.id].destroy(); delete teasers[f.id]; } }
  function syncTeaser(f) {
    var want = teaserWanted(f);
    if (want && !teasers[f.id] && document.body) {
      teasers[f.id] = FORMS_UI.teaser(f, {
        onOpen: function () { show(f, true); },
        onDismiss: function () { store.set(teaserHiddenKey(f), now()); store.del(teaserKey(f)); hideTeaser(f); }
      });
    } else if (!want) hideTeaser(f);
  }

  function runForms(forms) {
    forms.forEach(function (f) {
      if (ONLY_FORM && f.id !== ONLY_FORM) return;
      if (f.kind === "embed") show(f);
      else if (floating(f)) { schedule(f); syncTeaser(f); }
    });
  }
  // Any element with data-dumbmail-open="FORM_ID" opens that popup or flyout, including ones added later.
  function hookOpeners() {
    document.addEventListener("click", function (e) {
      var t = e.target, el = t && t.closest ? t.closest("[data-dumbmail-open]") : null;
      if (!el || inOwnUi(el)) return;
      var id = el.getAttribute("data-dumbmail-open");
      if (!id) return;
      e.preventDefault();
      loadForms(function (forms) { forms.forEach(function (f) { if (f.id === id && floating(f)) show(f, true); }); });
    }, true);
  }
  // Embed slots that appear later (single-page apps, lazy sections) get their form too.
  function watchEmbeds() {
    if (!window.MutationObserver || !formsCache || !formsCache.some(function (f) { return f.kind === "embed"; })) return;
    var t = 0;
    new MutationObserver(function () {
      if (t) return;
      t = setTimeout(function () { t = 0; formsCache.forEach(function (f) { if (f.kind === "embed") renderEmbeds(f); }); }, 200);
    }).observe(document.documentElement, { childList: true, subtree: true });
  }

  // Review widgets load their own code, and only on pages that have one (see the Reviews page in DumbMail).
  var reviewsLoading = false;
  function loadReviews() {
    if (ONLY_FORM) return;
    if (window.__dumbmailReviews) { window.__dumbmailReviews.scan(); return; }
    if (reviewsLoading || !document.querySelector("[data-dumbmail-reviews],[data-dumbmail-stars],[data-dumbmail-reviews-carousel],[data-dumbmail-reviews-all]")) return;
    reviewsLoading = true;
    var s = document.createElement("script");
    s.async = true;
    s.src = BASE + "/client/reviews.js?k=" + encodeURIComponent(KEY);
    (document.head || document.documentElement).appendChild(s);
  }

  function start() {
    if (!ONLY_FORM) {
      PLATFORM = platform();
      if (TEST) { test.draw(); test.check("loaded", { platform: PLATFORM, version: VERSION, auto_track: AUTO, capture_emails: CAPTURE, identified: identified(), summary: "On " + location.host + (PLATFORM !== "custom" ? " · " + PLATFORM : "") }); }
      hookPage();
      hookRoutes();
      hookOpeners();
      countPageView();
      if (AUTO) { hookLayer(); scanProduct(); scanCheckout(); }
      loadReviews();
    }
    firstSeen();
    loadForms(function (forms) {
      if (TEST) {
        var live = forms.filter(function (f) { return f.kind !== "page"; });
        test.check("forms", { count: live.length, ok: true, summary: live.length ? live.length + " live form" + (live.length === 1 ? "" : "s") + " ready" : "No live forms yet (that's fine)" });
      }
      runForms(forms);
      if (!ONLY_FORM) watchEmbeds();
    });
  }
  function begin() {
    testHooks();
    runQueued();
    if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start); else start();
  }
  if (NEW_TEST) verifyTest(NEW_TEST, begin); else begin();
})();
