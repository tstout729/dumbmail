// Square → DumbMail
//
// Placed Order: once per order. A COMPLETED payment for an order (payment.created or
//   payment.updated) fetches the order (GET /v2/orders/{id}), and the order counts once, by its
//   id, at its total_money, when nothing is due on it any more (net_amount_due_money 0): an order
//   paid in several parts counts once, in full. A payment with no order counts on its own, once.
//   Approved, pending, canceled and failed payments aren't orders.
// Refunded Order: once per refund (square:refund:<id>), when refund.created or refund.updated says
//   it's COMPLETED, with the refund's own amount. A refund names only its payment, so it finds the
//   buyer through the payment id saved on their profile (and waits for the payment if needed).
// Customers (customer.created, customer.updated) become profiles. A customer who unsubscribed from
//   Square's marketing emails is never sent marketing here either. Nobody is subscribed: Square's
//   customer object has no marketing opt-in.
// See README.md for the Square page behind each assumption.

var ZERO_DECIMAL = ["BIF", "CLP", "DJF", "GNF", "ISK", "JPY", "KMF", "KRW", "PYG", "RWF", "UGX", "UYI", "VND", "VUV", "XAF", "XOF", "XPF"];

function money(m) {
  if (!m || typeof m.amount !== "number") return undefined;
  return ZERO_DECIMAL.indexOf(String(m.currency || "").toUpperCase()) >= 0 ? m.amount : m.amount / 100;
}

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

function e164(p) {
  if (!p) return undefined;
  var s = String(p).trim();
  var d = s.replace(/[^0-9]/g, "");
  return s.charAt(0) === "+" && d.length >= 8 && d.length <= 15 ? "+" + d : undefined;
}

function payment(e, p) {
  if (p.status !== "COMPLETED") return [{ type: "log", message: "payment " + p.id + " is " + p.status + "; it's recorded when it's completed" }];
  var amount = money(p.total_money || p.amount_money);
  if (!(amount > 0)) return [{ type: "log", message: "payment " + p.id + " has no amount" }];
  var out = [];
  var who;
  if (p.buyer_email_address) {
    var keys = { square_payment: p.id };
    if (p.customer_id) keys.square_customer = p.customer_id;
    out.push({ type: "upsert_profile", email: p.buyer_email_address, keys: keys });
    who = { email: p.buyer_email_address };
  } else if (p.customer_id) {
    who = { property: "square_customer", value: p.customer_id, if_missing: "retry" };
    // Link the payment to its person, so a refund (which names only the payment) finds them.
    out.push({ type: "add_keys", profile: who, keys: { square_payment: p.id } });
  } else {
    return [{ type: "log", message: "payment " + p.id + " names no buyer email or customer, so there's nobody to match" }];
  }
  if (p.order_id && /^[A-Za-z0-9_-]{1,64}$/.test(p.order_id)) {
    out.push({ type: "next_request", url: "/v2/orders/" + p.order_id, context: { who: who } });
    return out;
  }
  out.push({
    type: "track_event", metric: "Placed Order", profile: who, value: amount,
    time: p.updated_at || p.created_at || e.created_at, unique_id: "square:payment:" + p.id,
    properties: clean({ payment_id: p.id, order_id: p.order_id, location_id: p.location_id, currency: (p.total_money || p.amount_money).currency, source_type: p.source_type, receipt_number: p.receipt_number }),
  });
  return out;
}

function refund(e, r) {
  if (r.status !== "COMPLETED") return [{ type: "log", message: "refund " + r.id + " is " + r.status + "; it's recorded when it's completed" }];
  if (!r.payment_id) return [{ type: "log", message: "refund " + r.id + " names no payment" }];
  return [{
    type: "track_event", metric: "Refunded Order",
    profile: { property: "square_payment", value: r.payment_id, if_missing: "retry" },
    value: money(r.amount_money), time: r.updated_at || r.created_at || e.created_at, unique_id: "square:refund:" + r.id,
    properties: clean({ refund_id: r.id, payment_id: r.payment_id, order_id: r.order_id, currency: r.amount_money && r.amount_money.currency, reason: r.reason }),
  }];
}

function customer(c) {
  if (!c.email_address) return [{ type: "log", message: "customer " + c.id + " has no email" }];
  var p = { type: "upsert_profile", email: c.email_address, keys: { square_customer: c.id } };
  if (c.given_name) p.first_name = c.given_name;
  if (c.family_name) p.last_name = c.family_name;
  var ph = e164(c.phone_number);
  if (ph) p.phone_number = ph;
  if (c.preferences && c.preferences.email_unsubscribed === true) p.marketing = false;
  return [p];
}

// The order a completed payment belongs to: counted once, in full, when nothing is due on it.
function order(input) {
  var o = (input.payload || {}).order;
  var who = input.context && input.context.who;
  if (!o || !o.id || !who) return [{ type: "log", message: "no order in the answer" }];
  var due = o.net_amount_due_money ? o.net_amount_due_money.amount : undefined;
  var total = money(o.total_money);
  if (due !== 0 || !(total > 0)) return [{ type: "log", message: "order " + o.id + " isn't fully paid yet; it's recorded when its last payment completes" }];
  return [{
    type: "track_event", metric: "Placed Order", profile: who, value: total,
    time: o.closed_at || o.updated_at, unique_id: "square:order:" + o.id,
    properties: clean({ order_id: o.id, location_id: o.location_id, currency: o.total_money.currency, payments: (o.tenders || []).length || undefined }),
  }];
}

function handle(input) {
  if (input.source === "fetch") return order(input);
  var e = input.payload || {};
  var obj = (e.data && e.data.object) || {};
  if (e.type === "payment.created" || e.type === "payment.updated") return obj.payment ? payment(e, obj.payment) : [{ type: "log", message: "no payment in " + e.type }];
  if (e.type === "refund.created" || e.type === "refund.updated") return obj.refund ? refund(e, obj.refund) : [{ type: "log", message: "no refund in " + e.type }];
  if (e.type === "customer.created" || e.type === "customer.updated") return obj.customer ? customer(obj.customer) : [{ type: "log", message: "no customer in " + e.type }];
  return [{ type: "log", message: "ignored Square event " + (e.type || "(none)") }];
}
