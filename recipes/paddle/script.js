// Paddle Billing → DumbMail
//
// Placed Order: transaction.completed, once per transaction, with details.totals.grand_total.
// Refunded Order: an adjustment whose action is refund, once it's approved (adjustment.created or
//   adjustment.updated), once per adjustment id, with its own total.
// Subscription Started: once per subscription, from subscription.created when it's active or
//   trialing, or subscription.activated. Subscription Cancelled: subscription.canceled.
// Customers (customer.created, customer.updated) become profiles keyed by the Paddle customer id;
// transactions, adjustments and subscriptions find them by it and wait (retry) when the customer
// hasn't arrived yet. A customer whose marketing_consent is true ticked Paddle Checkout's marketing
// box, so they're subscribed to email; nobody else is. A later change to false unsubscribes a
// subscription that came from Paddle (and leaves consent from anywhere else alone).
// See README.md for the Paddle page behind each assumption.

var ZERO_DECIMAL = ["JPY", "KRW"];

function money(s, currency) {
  var n = parseInt(s, 10);
  if (!isFinite(n)) return undefined;
  return ZERO_DECIMAL.indexOf(String(currency || "").toUpperCase()) >= 0 ? n : n / 100;
}

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

function byCustomer(id) {
  return { property: "paddle_customer", value: id, if_missing: "retry" };
}

function plan(sub) {
  var item = (sub.items || [])[0] || {};
  var price = item.price || {};
  return clean({
    subscription_id: sub.id, status: sub.status, currency: sub.currency_code,
    plan: price.name || price.description || price.id, price_id: price.id,
    amount: price.unit_price ? money(price.unit_price.amount, price.unit_price.currency_code) : undefined,
    interval: price.billing_cycle && price.billing_cycle.interval, frequency: price.billing_cycle && price.billing_cycle.frequency,
    quantity: item.quantity, next_billed_at: sub.next_billed_at,
  });
}

function started(sub) {
  return { type: "track_event", metric: "Subscription Started", profile: byCustomer(sub.customer_id), time: sub.started_at || sub.updated_at, unique_id: "paddle:subscription:" + sub.id + ":started", properties: plan(sub) };
}

function handle(input) {
  var e = input.payload || {};
  var d = e.data || {};
  var t = e.event_type;
  if (!t) return [{ type: "log", message: "not a Paddle event" }];

  if (t === "customer.created" || t === "customer.updated") {
    if (!d.email) return [{ type: "log", message: "customer " + d.id + " has no email" }];
    var p = { type: "upsert_profile", email: d.email, keys: { paddle_customer: d.id } };
    if (d.name) {
      var parts = String(d.name).trim().split(/\s+/);
      p.first_name = parts.shift();
      if (parts.length) p.last_name = parts.join(" ");
    }
    // Preferences apply in Paddle's order (its updated_at); turning marketing off withdraws only a
    // subscription that came from Paddle, never consent the person gave somewhere else.
    var asOf = d.updated_at || e.occurred_at;
    if (d.marketing_consent === true) p.consent = [{ channel: "email", subscribed: true, source: "Paddle Checkout marketing consent box", as_of: asOf }];
    else if (d.marketing_consent === false) p.consent = [{ channel: "email", subscribed: false, source: "Paddle marketing consent turned off", as_of: asOf, only_own: true }];
    return [p];
  }

  if (t === "transaction.completed") {
    if (!d.customer_id) return [{ type: "log", message: "transaction " + d.id + " has no customer" }];
    var totals = (d.details && d.details.totals) || {};
    var value = money(totals.grand_total, totals.currency_code || d.currency_code);
    if (!(value > 0)) return [{ type: "log", message: "transaction " + d.id + " was free, so it isn't recorded as an order" }];
    return [{
      type: "track_event", metric: "Placed Order", profile: byCustomer(d.customer_id), value: value,
      time: d.billed_at || d.updated_at || e.occurred_at, unique_id: "paddle:transaction:" + d.id,
      properties: clean({ transaction_id: d.id, invoice_number: d.invoice_number, subscription_id: d.subscription_id, origin: d.origin, currency: d.currency_code }),
    }];
  }

  if (t === "adjustment.created" || t === "adjustment.updated") {
    if (d.action !== "refund") return [{ type: "log", message: "adjustment " + d.id + " is a " + d.action + ", not a refund" }];
    if (d.status !== "approved") return [{ type: "log", message: "refund " + d.id + " is " + d.status + "; it's recorded when Paddle approves it" }];
    var tt = d.totals || {};
    return [{
      type: "track_event", metric: "Refunded Order", profile: byCustomer(d.customer_id),
      value: money(tt.total, tt.currency_code || d.currency_code), time: d.updated_at || e.occurred_at, unique_id: "paddle:refund:" + d.id,
      properties: clean({ refund_id: d.id, transaction_id: d.transaction_id, subscription_id: d.subscription_id, reason: d.reason, currency: d.currency_code }),
    }];
  }

  if (t === "subscription.created") {
    if (d.status !== "active" && d.status !== "trialing") return [{ type: "log", message: "subscription " + d.id + " is " + d.status + "; it's recorded as started when it becomes active" }];
    return [started(d)];
  }
  if (t === "subscription.activated") return [started(d)];
  if (t === "subscription.canceled") {
    var props = plan(d);
    return [{ type: "track_event", metric: "Subscription Cancelled", profile: byCustomer(d.customer_id), time: d.canceled_at || d.updated_at, unique_id: "paddle:subscription:" + d.id + ":canceled", properties: props }];
  }
  return [{ type: "log", message: "ignored Paddle event " + t }];
}
