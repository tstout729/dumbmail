// WooCommerce → DumbMail
//
// Webhooks (order.created, order.updated, customer.created, customer.updated) carry the same
// order or customer object the REST API returns. A pull of /wp-json/wc/v3/orders imports past
// orders. See README.md for the WooCommerce page behind each assumption.
//
// Placed Order: once per order (woocommerce:order:<id>:placed), when WooCommerce records the
//   payment (date_paid_gmt), or, for cash on delivery, when the order is completed. Pending,
//   on-hold, failed and free orders aren't orders.
// Fulfilled Order: once, when the status is completed.
// Cancelled Order: once, when a paid order is cancelled. Unpaid Order Cancelled: once, when an
//   order that was never paid is cancelled (kept apart from orders, which it never was).
// Refunded Order: once per refund, from the refund records themselves (fetched from the store's
//   API when an order lists refunds), with each refund's own amount and date; gateway: false marks
//   a refund the merchant recorded without the payment gateway moving the money.
// People are matched by billing email and never subscribed: WooCommerce has no marketing opt-in
// field, so everyone this adds is held back from marketing.


function num(s) {
  var n = parseFloat(s);
  return isFinite(n) ? n : undefined;
}

// WooCommerce writes GMT times without a zone ("2017-03-22T19:28:08").
function gmt(s) {
  if (!s) return undefined;
  return /([zZ]|[+-]\d\d:?\d\d)$/.test(s) ? s : s + "Z";
}

// Only numbers that are already international (+…) or North American with 10 digits.
function e164(phone, country) {
  if (!phone) return undefined;
  var p = String(phone).trim();
  var digits = p.replace(/[^0-9]/g, "");
  if (p.charAt(0) === "+" && digits.length >= 8 && digits.length <= 15) return "+" + digits;
  if ((country === "US" || country === "CA") && digits.length === 10) return "+1" + digits;
  if ((country === "US" || country === "CA") && digits.length === 11 && digits.charAt(0) === "1") return "+" + digits;
  return undefined;
}

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

function person(email, first, last, phone, country, customerId) {
  var p = { type: "upsert_profile", email: email };
  if (first) p.first_name = first;
  if (last) p.last_name = last;
  var ph = e164(phone, country);
  if (ph) p.phone_number = ph;
  if (customerId) p.keys = { woocommerce_customer: String(customerId) };
  return p;
}

function orderActions(o, historical, canFetch) {
  var b = o.billing || {};
  if (!b.email) return [{ type: "log", message: "order " + o.id + " has no billing email" }];
  var who = { email: b.email };
  var out = [person(b.email, b.first_name, b.last_name, b.phone, b.country, o.customer_id > 0 ? o.customer_id : null)];
  // Cash on delivery: WooCommerce moves the order to processing (and may set date_paid) before the
  // cash is collected, so it's paid only once completed. Other methods: when WooCommerce records
  // the payment (date_paid_gmt).
  var paid = o.payment_method === "cod" ? o.status === "completed" : !!o.date_paid_gmt;
  var total = num(o.total);
  var base = clean({ order_id: o.id, order_number: o.number, currency: o.currency, status: o.status, historical: historical || undefined });
  if (!paid) {
    if (o.status === "cancelled") {
      out.push({ type: "track_event", metric: "Unpaid Order Cancelled", profile: who, time: gmt(o.date_modified_gmt), unique_id: "woocommerce:order:" + o.id + ":unpaid-cancelled", properties: base });
    } else {
      out.push({ type: "log", message: "order " + o.id + " is " + o.status + " and not paid; it's recorded when it's paid" });
    }
    return out;
  }
  if (!(total > 0)) {
    out.push({ type: "log", message: "order " + o.id + " was free, so it isn't recorded as an order" });
    return out;
  }
  var items = (o.line_items || []).slice(0, 20).map(function (li) {
    return clean({ name: li.name, product_id: li.product_id, variation_id: li.variation_id || undefined, quantity: li.quantity, sku: li.sku, total: num(li.total) });
  });
  var placed = JSON.parse(JSON.stringify(base));
  placed.item_count = (o.line_items || []).reduce(function (n, li) { return n + (li.quantity || 0); }, 0);
  placed.items = items;
  out.push({ type: "track_event", metric: "Placed Order", profile: who, value: total, time: gmt(o.date_paid_gmt || o.date_created_gmt), unique_id: "woocommerce:order:" + o.id + ":placed", properties: placed });
  if (o.status === "completed") {
    out.push({ type: "track_event", metric: "Fulfilled Order", profile: who, value: total, time: gmt(o.date_completed_gmt || o.date_modified_gmt), unique_id: "woocommerce:order:" + o.id + ":completed", properties: base });
  }
  if (o.status === "cancelled") {
    out.push({ type: "track_event", metric: "Cancelled Order", profile: who, value: total, time: gmt(o.date_modified_gmt), unique_id: "woocommerce:order:" + o.id + ":cancelled", properties: base });
  }
  // The order's refund list is only a summary: the refund records (their dates, and whether the
  // gateway refunded the money) are fetched next. A history import records refunds as they come.
  if ((o.refunds || []).length && /^[0-9]{1,20}$/.test(String(o.id))) {
    // In a webhook the lookup is the fetch; in a history import each order's refunds are looked up
    // the same way, so they keep their own dates there too.
    var look = { url: "/wp-json/wc/v3/orders/" + o.id + "/refunds", context: { order: clean({ id: o.id, number: o.number, currency: o.currency, email: b.email }) } };
    out.push(canFetch ? Object.assign({ type: "next_request" }, look) : Object.assign({ type: "lookup" }, look));
  }
  return out;
}

function customerActions(c) {
  if (!c.email) return [{ type: "log", message: "customer " + c.id + " has no email" }];
  var b = c.billing || {};
  return [person(c.email, c.first_name || b.first_name, c.last_name || b.last_name, b.phone, b.country, c.id)];
}

// Pulls read orders by modification-time windows, oldest change first: modified_after and
// modified_before bound each request, and nothing is skipped by offset into a list that keeps
// changing. A full page means the window was too wide: it's halved and read again (orders already
// read are recorded once, by their ids). A window of a second or two that's still full is paged
// through. Each next window starts a second before the last one ended, so an order changed at the
// boundary is never missed. Within a page, an order is read once (by id).
var PER_PAGE = 100;

function queryOf(input) {
  var out = {};
  ((input.request && input.request.query) || []).forEach(function (kv) { if (Array.isArray(kv)) out[kv[0]] = kv[1]; });
  return out;
}

function iso(ms) {
  return new Date(ms - (ms % 1000)).toISOString().replace(".000Z", "Z");
}

function pullActions(input) {
  var rows = Array.isArray(input.payload) ? input.payload : [];
  var seen = {};
  var out = [];
  var newest = 0;
  rows.forEach(function (o) {
    if (!o || seen[o.id]) return;
    seen[o.id] = true;
    var m = Date.parse(gmt(o.date_modified_gmt));
    if (m > newest) newest = m;
    out = out.concat(orderActions(o, !!input.backfill, false));
  });
  var q = queryOf(input);
  var after = Date.parse(q.modified_after || input.now);
  var before = q.modified_before ? Date.parse(q.modified_before) : null;
  var now = Date.parse(input.now);
  var page = parseInt(q.page || "1", 10) || 1;
  if (rows.length >= PER_PAGE) {
    var end = before || now;
    if (end - after > 2000) {
      var mid = after + Math.floor((end - after) / 2);
      out.push({ type: "next_request", query: { modified_after: iso(after), modified_before: iso(mid), page: "1" } });
    } else {
      out.push({ type: "next_request", query: { modified_after: iso(after), modified_before: before ? iso(before) : null, page: String(page + 1) } });
    }
    out.push({ type: "set_state", values: { after: iso(after) } });
    return out;
  }
  if (before === null || before >= now) {
    // Caught up: the next run starts a second before the newest change read.
    var next = newest ? Math.max(after, newest - 1000) : after;
    out.push({ type: "set_state", values: { after: iso(next) } });
    if (!rows.length) out.push({ type: "log", message: "no changed orders" });
    return out;
  }
  // This window is done: the next starts a second before it ended, twice as wide (or open-ended).
  var start = before - 1000;
  var span = (before - after) * 2;
  var nextBefore = start + span < now ? iso(start + span) : null;
  out.push({ type: "set_state", values: { after: iso(start) } });
  out.push({ type: "next_request", query: { modified_after: iso(start), modified_before: nextBefore, page: "1" } });
  return out;
}

// The refund records of one order (GET /orders/{id}/refunds), with the order the webhook carried.
function refundActions(input) {
  var order = (input.context && input.context.order) || {};
  var rows = Array.isArray(input.payload) ? input.payload : [];
  if (!order.email) return [{ type: "log", message: "refunds without the order they belong to" }];
  var out = [];
  rows.forEach(function (r) {
    var amt = num(r.amount);
    if (!r.id || !(amt > 0)) return;
    out.push({
      type: "track_event", metric: "Refunded Order", profile: { email: order.email }, value: amt, time: gmt(r.date_created_gmt),
      unique_id: "woocommerce:refund:" + r.id,
      properties: clean({ order_id: order.id, order_number: order.number, refund_id: r.id, currency: order.currency, reason: r.reason, gateway: r.refunded_payment === true }),
    });
  });
  if (!out.length) out.push({ type: "log", message: "no refunds with an amount for order " + order.id });
  return out;
}

function handle(input) {
  if (input.source === "fetch") return refundActions(input);
  if (input.source === "pull") return pullActions(input);
  var p = input.payload || {};
  var topic = String(input.headers["x-wc-webhook-topic"] || "").toLowerCase();
  if (topic === "order.created" || topic === "order.updated" || (!topic && p.order_key)) return orderActions(p, false, true);
  if (topic === "customer.created" || topic === "customer.updated" || (!topic && p.role !== undefined && p.email)) return customerActions(p);
  if (p.webhook_id) return [{ type: "log", message: "WooCommerce's test ping for webhook " + p.webhook_id }];
  return [{ type: "log", message: "ignored WooCommerce topic " + (topic || "(none)") }];
}
