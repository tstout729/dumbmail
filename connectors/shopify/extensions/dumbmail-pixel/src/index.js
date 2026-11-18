// DumbMail's checkout pixel as a Shopify app web pixel. Installed by DumbMail when a store connects,
// with `base` (the DumbMail server) and `key` (the store's public key) as settings.
//
// It reports the same events, properties and ids as the custom pixel DumbMail offers to paste
// (src/shopify_pixel.js in the DumbMail repo): Started Checkout (`checkout:<token>`) and Placed Order
// (`order:<id>`). With the app connected, Shopify's own webhooks report the same orders and
// checkouts with the same ids, so each is recorded once; the pixel's part is telling the storefront
// script who checked out.
import { register } from "@shopify/web-pixels-extension";

register(({ analytics, browser, settings, init, customerPrivacy }) => {
  const BASE = String(settings.base || "").replace(/\/+$/, "");
  const KEY = String(settings.key || "");
  if (!/^https:\/\//.test(BASE) || !KEY) return;
  // The visitor's consent (Shopify's Customer Privacy API), kept current. Without analytics and
  // marketing consent the pixel sends nothing and passes no identity or held browsing activity to
  // the storefront script.
  let consent = (init && init.customerPrivacy) || {};
  if (customerPrivacy && typeof customerPrivacy.subscribe === "function") {
    customerPrivacy.subscribe("visitorConsentCollected", (event) => { consent = (event && event.customerPrivacy) || {}; });
  }
  const allowed = () => consent.analyticsProcessingAllowed === true && consent.marketingAllowed === true;

  async function read(store, key) {
    try { return JSON.parse(await browser[store].getItem(key)); } catch (e) { return null; }
  }
  // Shopify ids look like gid://shopify/Order/123; keep the number, as the storefront uses it.
  const id = (g) => (g == null ? undefined : String(g).split("/").pop());
  const money = (m) => (m && typeof m.amount === "number" ? m.amount : m && m.amount != null ? Number(m.amount) : undefined);
  function items(checkout, origin) {
    return (checkout.lineItems || []).map((li) => {
      const v = li.variant || {}, p = v.product || {};
      return {
        name: p.title || li.title, product_id: id(p.id), variant_id: id(v.id), variant: v.title && v.title !== "Default Title" ? v.title : undefined,
        sku: v.sku || undefined, price: money(v.price), quantity: li.quantity,
        image_url: v.image && v.image.src ? (v.image.src.startsWith("//") ? "https:" + v.image.src : v.image.src) : undefined,
        url: p.url && origin ? new URL(p.url, origin).href : undefined, brand: p.vendor || undefined,
      };
    });
  }
  function post(path, body) {
    return fetch(BASE + path + "?k=" + encodeURIComponent(KEY), {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), keepalive: true, credentials: "omit",
    }).catch(() => {});
  }
  function buyer(checkout) {
    const email = checkout.email && checkout.email.trim();
    const phone = checkout.phone || (checkout.shippingAddress && checkout.shippingAddress.phone) || null;
    return email || phone ? { email: email || undefined, phone_number: email ? undefined : phone } : null;
  }

  async function report(metric, event, uniqueId) {
    // Without an order id or checkout token the event can't be matched to Shopify's own copy, so it
    // would be counted twice: send nothing.
    if (!uniqueId) return false;
    if (!allowed()) return false;
    const checkout = event.data.checkout || {};
    const origin = event.context && event.context.document && event.context.document.location ? event.context.document.location.origin : undefined;
    const list = items(checkout, origin);
    const total = money(checkout.totalPrice);
    const props = {
      tracked_by: "shopify_pixel", items: list, item_names: list.map((i) => i.name).filter(Boolean), item_count: list.reduce((s, i) => s + (i.quantity || 1), 0),
      total, currency: (checkout.totalPrice && checkout.totalPrice.currencyCode) || checkout.currencyCode,
      discount_codes: (checkout.discountApplications || []).map((d) => d.title).filter(Boolean),
    };
    if (metric === "Started Checkout" && event.context && event.context.document) props.checkout_url = event.context.document.location.href;
    if (metric === "Placed Order") props.order_id = id(checkout.order && checkout.order.id);
    await browser.localStorage.setItem("pl_pixel", JSON.stringify(Date.now()));
    const who = buyer(checkout);
    if (!who) return false;
    // Tell the storefront script who this is, keeping what it knew only if it's the same person.
    const stored = (await read("localStorage", "pl_profile")) || {};
    const same = who.email ? stored.email && stored.email.toLowerCase() === who.email.toLowerCase() : stored.phone_number === who.phone_number;
    const profile = same ? Object.assign({}, stored, who) : who;
    await browser.localStorage.setItem("pl_profile", JSON.stringify(profile));
    post("/client/events", { metric, profile, properties: props, value: total, unique_id: uniqueId, time: Date.now() });
    // Activity the storefront held while it didn't know this visitor (product views, adds) belongs to them too.
    const held = (await read("localStorage", "pl_buffer")) || [];
    if (held.length) {
      await browser.localStorage.removeItem("pl_buffer");
      for (const e of held) {
        if (e && Date.now() - e.time < 3 * 86400000) post("/client/events", { metric: e.metric, profile, properties: e.properties, value: e.value, unique_id: e.unique_id, time: e.time });
      }
    }
    return true;
  }

  async function started(event) {
    const checkout = event.data.checkout || {};
    const key = "pl_px_sc_" + (checkout.token || "");
    if (await read("sessionStorage", key)) return;
    if (!buyer(checkout)) return;
    if (await report("Started Checkout", event, checkout.token ? "checkout:" + checkout.token : undefined)) await browser.sessionStorage.setItem(key, "1");
  }

  analytics.subscribe("checkout_started", started);
  analytics.subscribe("checkout_contact_info_submitted", started);
  analytics.subscribe("checkout_completed", (event) => {
    const order = event.data.checkout && event.data.checkout.order;
    report("Placed Order", event, order && order.id ? "order:" + id(order.id) : undefined);
  });
});
