// WooCommerce → DumbMail
//
// Webhooks (order.created, order.updated, customer.created, customer.updated) carry the same
// order or customer object the REST API returns. A pull of /wp-json/wc/v3/orders imports past
// orders. See README.md for the WooCommerce page behind each assumption.
//
// Placed Order: once per order (woocommerce:order:<id>:placed), when WooCommerce says it's paid
//   (date_paid_gmt set, or status processing or completed). Pending, on-hold, failed and free orders
//   aren't orders.
// Fulfilled Order: once, when the status is completed.
// Cancelled Order: once, when a paid order is cancelled.
// Refunded Order: once per refund id, with that refund's own amount.
// People are matched by billing email and never subscribed: WooCommerce has no marketing opt-in
// field, so everyone this adds is held back from marketing.

var PAID = ["processing", "completed"];

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

function orderActions(o, historical) {
  var b = o.billing || {};
  if (!b.email) return [{ type: "log", message: "order " + o.id + " has no billing email" }];
  var who = { email: b.email };
  var out = [person(b.email, b.first_name, b.last_name, b.phone, b.country, o.customer_id > 0 ? o.customer_id : null)];
  var paid = !!o.date_paid_gmt || PAID.indexOf(o.status) >= 0;
  var total = num(o.total);
  var base = clean({ order_id: o.id, order_number: o.number, currency: o.currency, status: o.status, historical: historical || undefined });
  if (!paid) {
    out.push({ type: "log", message: "order " + o.id + " is " + o.status + " and not paid; it's recorded when it's paid" });
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
  (o.refunds || []).forEach(function (r) {
    var amt = num(r.total);
    if (amt === undefined || !r.id) return;
    out.push({
      type: "track_event", metric: "Refunded Order", profile: who, value: Math.abs(amt), time: gmt(o.date_modified_gmt),
      unique_id: "woocommerce:refund:" + r.id,
      properties: clean({ order_id: o.id, order_number: o.number, refund_id: r.id, currency: o.currency, reason: r.reason || r.refund, historical: historical || undefined }),
    });
  });
  return out;
}

function customerActions(c) {
  if (!c.email) return [{ type: "log", message: "customer " + c.id + " has no email" }];
  var b = c.billing || {};
  return [person(c.email, c.first_name || b.first_name, c.last_name || b.last_name, b.phone, b.country, c.id)];
}

// Pulls page through orders oldest change first. Each page starts after a moment just before the
// newest change read (`after`) and skips the orders it already read there (`skip`), so orders
// changed in the same second are never missed or read twice.
var PER_PAGE = 50;

function pullActions(input) {
  var rows = Array.isArray(input.payload) ? input.payload : [];
  var out = [];
  rows.forEach(function (o) {
    out = out.concat(orderActions(o, !!input.backfill));
  });
  if (!rows.length) return out.concat([{ type: "log", message: "no changed orders" }]);
  var last = gmt(rows[rows.length - 1].date_modified_gmt);
  var lastSecond = new Date(Date.parse(last) - (Date.parse(last) % 1000));
  var after = new Date(lastSecond.getTime() - 1000).toISOString().replace(".000Z", "Z");
  var atLast = rows.filter(function (o) { return Date.parse(gmt(o.date_modified_gmt)) - lastSecond.getTime() < 1000 && Date.parse(gmt(o.date_modified_gmt)) >= lastSecond.getTime(); }).length;
  var prevAfter = input.state.after;
  var prevSkip = parseInt(input.state.skip || "0", 10) || 0;
  var skip = prevAfter === after ? prevSkip + rows.length : atLast;
  out.push({ type: "set_state", values: { after: after, skip: String(skip) } });
  if (rows.length >= PER_PAGE) out.push({ type: "next_request", query: { modified_after: after, offset: String(skip) } });
  return out;
}

function handle(input) {
  if (input.source === "pull") return pullActions(input);
  var p = input.payload || {};
  var topic = String(input.headers["x-wc-webhook-topic"] || "").toLowerCase();
  if (topic === "order.created" || topic === "order.updated" || (!topic && p.order_key)) return orderActions(p, false);
  if (topic === "customer.created" || topic === "customer.updated" || (!topic && p.role !== undefined && p.email)) return customerActions(p);
  if (p.webhook_id) return [{ type: "log", message: "WooCommerce's test ping for webhook " + p.webhook_id }];
  return [{ type: "log", message: "ignored WooCommerce topic " + (topic || "(none)") }];
}
