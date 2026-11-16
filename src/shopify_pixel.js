// DumbMail checkout tracking for Shopify. Shopify admin → Settings → Customer events → Add custom pixel.
// Your theme can't run scripts on checkout pages, so this pixel reports Started Checkout and Placed Order.
const BASE = __BASE__, KEY = __KEY__;

async function read(store, key) {
  try { return JSON.parse(await browser[store].getItem(key)); } catch (e) { return null; }
}
// Shopify ids look like gid://shopify/Order/123; keep the number, as the storefront uses it.
function id(g) { return g == null ? undefined : String(g).split("/").pop(); }
function money(m) { return m && typeof m.amount === "number" ? m.amount : m && m.amount != null ? Number(m.amount) : undefined; }
function items(checkout) {
  return (checkout.lineItems || []).map((li) => {
    const v = li.variant || {}, p = v.product || {};
    return {
      name: p.title || li.title, product_id: id(p.id), variant_id: id(v.id), variant: v.title && v.title !== "Default Title" ? v.title : undefined,
      sku: v.sku || undefined, price: money(v.price), quantity: li.quantity, image_url: v.image && v.image.src ? (v.image.src.startsWith("//") ? "https:" + v.image.src : v.image.src) : undefined,
      url: p.url ? new URL(p.url, init.context.document.location.origin).href : undefined, brand: p.vendor || undefined,
    };
  });
}
function post(path, body) {
  return fetch(BASE + path + "?k=" + encodeURIComponent(KEY), {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), keepalive: true, credentials: "omit",
  }).catch(() => {});
}

// Shopify knows who is checking out from the email (or phone) on the checkout itself; that decides who it's credited to.
function buyer(checkout) {
  const email = checkout.email && checkout.email.trim();
  const phone = checkout.phone || (checkout.shippingAddress && checkout.shippingAddress.phone) || null;
  return email || phone ? { email: email || undefined, phone_number: email ? undefined : phone } : null;
}

async function report(metric, event, uniqueId) {
  const checkout = event.data.checkout || {};
  const list = items(checkout);
  const total = money(checkout.totalPrice);
  const props = {
    tracked_by: "shopify_pixel", items: list, item_names: list.map((i) => i.name).filter(Boolean), item_count: list.reduce((s, i) => s + (i.quantity || 1), 0),
    total, currency: (checkout.totalPrice && checkout.totalPrice.currencyCode) || checkout.currencyCode,
    discount_codes: (checkout.discountApplications || []).map((d) => d.title).filter(Boolean),
  };
  if (metric === "Started Checkout") props.checkout_url = event.context.document.location.href;
  if (metric === "Placed Order") props.order_id = id(checkout.order && checkout.order.id);
  await browser.localStorage.setItem("pl_pixel", JSON.stringify(Date.now()));
  const who = buyer(checkout);

  const test = await read("sessionStorage", "pl_test");
  if (test && test.code && test.until > Date.now()) {
    const check = metric === "Placed Order" ? "order" : "checkout";
    const amount = `${props.currency || ""} ${total != null ? total.toFixed(2) : ""}`.trim();
    const summary = metric === "Placed Order" ? `Order ${props.order_id || ""} · ${amount}` : `${props.item_count} item(s) · ${amount}`;
    post("/client/test", { code: test.code, check: "pixel", detail: { summary: "Checkout pixel is running" }, url: props.checkout_url });
    post("/client/test", { code: test.code, check, detail: { metric, summary, tracked_by: "shopify_pixel", value: total, email: (who && who.email) || null }, url: props.checkout_url });
    return false;
  }
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

// Started Checkout once per checkout, once Shopify knows the buyer's email (often only after they type it).
async function started(event) {
  const checkout = event.data.checkout || {};
  const key = "pl_px_sc_" + (checkout.token || "");
  if (await read("sessionStorage", key)) return;
  const test = await read("sessionStorage", "pl_test");
  if (!buyer(checkout) && !(test && test.until > Date.now())) return;
  if (await report("Started Checkout", event, checkout.token ? "checkout:" + checkout.token : undefined)) await browser.sessionStorage.setItem(key, "1");
}

analytics.subscribe("checkout_started", started);
analytics.subscribe("checkout_contact_info_submitted", started);
analytics.subscribe("checkout_completed", (event) => {
  const order = event.data.checkout && event.data.checkout.order;
  report("Placed Order", event, order && order.id ? "order:" + id(order.id) : undefined);
});
