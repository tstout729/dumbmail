// Stripe → DumbMail
//
// Placed Order: checkout.session.completed when the session is paid, otherwise
//   checkout.session.async_payment_succeeded (delayed payment methods); both keyed by the Checkout
//   Session id, so an order is recorded once. A session that needed no payment (a free or fully
//   discounted checkout) isn't an order. invoice.paid for subscription invoices.
// Refunded Order: once per refund (by refund id) with that refund's own amount, and only once it
//   has succeeded: refund.created when it's already succeeded, refund.updated when it gets there.
//   charge.refunded is used only when it lists its refunds (older API versions), with the same ids.
// Subscription Started / Subscription Cancelled: customer.subscription.created when the
//   subscription is active (or trialing), customer.subscription.updated when an incomplete one
//   becomes active, first subscription invoices, and customer.subscription.deleted. Each is keyed
//   by the subscription, so a start is recorded once.
//
// Profiles are matched by email. The Stripe customer id is saved as the stripe_customer_id
// property, and each payment's PaymentIntent and charge ids as keys, so events without an email
// find the same person: subscriptions by customer, refunds by their PaymentIntent or charge (a
// Refund has no customer). When that person isn't known yet (their checkout or invoice hasn't
// arrived), the delivery is retried later. People
// are subscribed to email only when they ticked Stripe Checkout's promotional-emails box
// (consent_collection.promotions); otherwise consent is left alone. See README.md for the doc
// page behind each assumption.

// Stripe amounts are in the smallest unit; these currencies have none.
var ZERO_DECIMAL = ["bif", "clp", "djf", "gnf", "jpy", "kmf", "krw", "mga", "pyg", "rwf", "ugx", "vnd", "vuv", "xaf", "xof", "xpf"];

function amount(cents, currency) {
  if (cents === null || cents === undefined) return undefined;
  var c = String(currency || "").toLowerCase();
  return ZERO_DECIMAL.indexOf(c) >= 0 ? cents : Math.round(cents) / 100;
}

function iso(unix) {
  return unix ? new Date(unix * 1000).toISOString() : undefined;
}

function names(full) {
  if (!full) return {};
  var parts = String(full).trim().split(/\s+/);
  var out = { first_name: parts.shift() };
  if (parts.length) out.last_name = parts.join(" ");
  return out;
}

function customerId(c) {
  return c && typeof c === "object" ? c.id : c || null;
}

// `payments`: the PaymentIntent and charge ids of this person's payment, kept as keys so a
// refund (which names only those) finds them.
function profile(email, name, phone, customer, consent, payments) {
  var p = { type: "upsert_profile", email: email };
  var n = names(name);
  if (n.first_name) p.first_name = n.first_name;
  if (n.last_name) p.last_name = n.last_name;
  if (phone && /^\+[0-9]{8,15}$/.test(phone)) p.phone_number = phone;
  if (customer) p.properties = { stripe_customer_id: customer };
  if (consent) p.consent = [consent];
  var keys = {};
  var pi = payments && customerId(payments.payment_intent);
  var ch = payments && customerId(payments.charge);
  if (pi) keys.stripe_payment_intent = pi;
  if (ch) keys.stripe_charge = ch;
  if (pi || ch) p.keys = keys;
  return p;
}

// Who a refund is about: its PaymentIntent, else its charge (both saved from the payment).
function refundWho(rf) {
  var pi = customerId(rf.payment_intent);
  if (pi) return { property: "stripe_payment_intent", value: pi, if_missing: "retry" };
  var ch = customerId(rf.charge);
  if (ch) return { property: "stripe_charge", value: ch, if_missing: "retry" };
  // `customer` is nullable on a Refund (and usually empty): only a last resort.
  var cu = customerId(rf.customer);
  if (cu) return { property: "stripe_customer_id", value: cu, if_missing: "retry" };
  return null;
}

// An invoice's PaymentIntent: `payment_intent` in older API versions, `payments.data[].payment` in newer ones.
function invoicePayment(inv) {
  if (inv.payment_intent) return customerId(inv.payment_intent);
  var list = (inv.payments && inv.payments.data) || [];
  for (var i = 0; i < list.length; i++) {
    var pay = list[i].payment || {};
    if (pay.payment_intent) return customerId(pay.payment_intent);
  }
  return null;
}

var ACTIVE = ["active", "trialing"];

function subscriptionStarted(o, when) {
  var props = plan(o);
  props.subscription_id = o.id;
  props.status = o.status;
  return {
    type: "track_event", metric: "Subscription Started",
    // No email on subscriptions: find the person by the customer id saved from checkout or invoices.
    // Retried later if that customer's first invoice or checkout hasn't arrived yet.
    profile: { property: "stripe_customer_id", value: customerId(o.customer), if_missing: "retry" },
    time: when, unique_id: "stripe:subscription:" + o.id + ":started",
    properties: clean(props),
  };
}

function subscriptionOf(invoice) {
  if (invoice.subscription) return customerId(invoice.subscription);
  var parent = invoice.parent || {};
  return (parent.subscription_details && customerId(parent.subscription_details.subscription)) || null;
}

function plan(sub) {
  var item = sub.items && sub.items.data && sub.items.data[0];
  var price = (item && (item.price || item.plan)) || {};
  return {
    plan: price.nickname || price.lookup_key || price.id || null,
    amount: amount(price.unit_amount !== undefined ? price.unit_amount : price.amount, price.currency),
    currency: price.currency || null,
    interval: (price.recurring && price.recurring.interval) || price.interval || null,
  };
}

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined) out[k] = o[k];
  return out;
}

function refunded(rf, who, when) {
  return {
    type: "track_event", metric: "Refunded Order", profile: who,
    value: amount(rf.amount, rf.currency), time: when,
    unique_id: "stripe:refund:" + rf.id,
    properties: clean({ refund_id: rf.id, charge_id: customerId(rf.charge), payment_intent: customerId(rf.payment_intent), currency: rf.currency, reason: rf.reason || null }),
  };
}

function handle(input) {
  var e = input.payload || {};
  if (!e.type || !e.data || !e.data.object) return [{ type: "log", message: "not a Stripe event" }];
  var o = e.data.object;
  var when = iso(e.created);

  if (e.type === "checkout.session.completed" || e.type === "checkout.session.async_payment_succeeded") {
    // A delayed payment method completes the session unpaid; the order counts when it succeeds.
    if (o.payment_status !== "paid" && o.payment_status !== "no_payment_required") {
      return [{ type: "log", message: "checkout " + o.id + " isn't paid yet (" + o.payment_status + "); it's recorded when checkout.session.async_payment_succeeded arrives" }];
    }
    var cd = o.customer_details || {};
    var email = cd.email || o.customer_email;
    if (!email) return [{ type: "log", message: "checkout " + o.id + " has no email" }];
    var consent = o.consent && o.consent.promotions === "opt_in" ? { channel: "email", subscribed: true, source: "Stripe Checkout promotional emails checkbox" } : null;
    var out = [profile(email, cd.name, cd.phone, customerId(o.customer), consent, { payment_intent: o.payment_intent })];
    if (o.payment_status === "no_payment_required") {
      out.push({ type: "log", message: "checkout " + o.id + " needed no payment, so it isn't recorded as an order" });
      return out;
    }
    // Subscription checkouts are counted from their invoices instead.
    if (o.mode === "payment") {
      out.push({
        type: "track_event", metric: "Placed Order", profile: { email: email },
        value: amount(o.amount_total, o.currency), time: when, unique_id: "stripe:checkout:" + o.id,
        properties: clean({ order_id: o.id, payment_intent: customerId(o.payment_intent), currency: o.currency, source: "stripe_checkout", metadata: o.metadata }),
      });
    }
    return out;
  }

  if (e.type === "invoice.paid") {
    var sub = subscriptionOf(o);
    if (!sub) return [{ type: "log", message: "invoice " + o.id + " isn't for a subscription; one-time payments come from checkout" }];
    if (!o.customer_email) return [{ type: "log", message: "invoice " + o.id + " has no customer email" }];
    var acts = [profile(o.customer_email, o.customer_name, o.customer_phone, customerId(o.customer), null, { payment_intent: invoicePayment(o), charge: o.charge })];
    if (o.amount_paid > 0) {
      acts.push({
        type: "track_event", metric: "Placed Order", profile: { email: o.customer_email },
        value: amount(o.amount_paid, o.currency), time: when, unique_id: "stripe:invoice:" + o.id,
        properties: clean({ order_id: o.id, invoice_number: o.number, subscription_id: sub, billing_reason: o.billing_reason, currency: o.currency, source: "stripe_invoice" }),
      });
    }
    if (o.billing_reason === "subscription_create") {
      acts.push({
        type: "track_event", metric: "Subscription Started", profile: { email: o.customer_email },
        time: when, unique_id: "stripe:subscription:" + sub + ":started",
        properties: { subscription_id: sub, currency: o.currency },
      });
    }
    return acts;
  }

  if (e.type === "refund.created" || e.type === "refund.updated") {
    // Counted once it has succeeded (pending refunds can still fail), and once per refund id.
    if (o.status !== "succeeded") return [{ type: "log", message: "refund " + o.id + " is " + (o.status || "without a status") + "; it's recorded when it succeeds" }];
    var rw = refundWho(o);
    if (!rw) return [{ type: "log", message: "refund " + o.id + " names no payment, so there's nobody to match" }];
    return [refunded(o, rw, when)];
  }

  if (e.type === "charge.refunded") {
    var bd = o.billing_details || {};
    var to = bd.email || o.receipt_email;
    if (!to) return [{ type: "log", message: "refunded charge " + o.id + " has no email" }];
    var acts2 = [profile(to, bd.name, bd.phone, customerId(o.customer), null, { payment_intent: o.payment_intent, charge: o.id })];
    // The charge's amount_refunded is a running total, so it's never the refund's amount. Each
    // refund is recorded once, from refund.created/updated or from the charge's own list of refunds.
    var list = (o.refunds && o.refunds.data) || [];
    list.forEach(function (rf) {
      if (rf.status === "succeeded") acts2.push(refunded(rf, { email: to }, iso(rf.created) || when));
    });
    if (!list.length) acts2.push({ type: "log", message: "charge " + o.id + " doesn't list its refunds; each refund is recorded from refund.created or refund.updated" });
    return acts2;
  }

  if (e.type === "customer.subscription.created" || e.type === "customer.subscription.updated") {
    if (!customerId(o.customer)) return [{ type: "log", message: "subscription " + o.id + " has no customer" }];
    // An incomplete subscription (its first payment hasn't gone through) hasn't started; it starts
    // when it becomes active. The start is keyed by the subscription, so it's recorded once.
    if (ACTIVE.indexOf(o.status) < 0) return [{ type: "log", message: "subscription " + o.id + " is " + o.status + "; it's recorded as started when it becomes active" }];
    if (e.type === "customer.subscription.updated") {
      var prev = (e.data.previous_attributes || {}).status;
      if (prev === undefined || ACTIVE.indexOf(prev) >= 0) return [{ type: "log", message: "subscription " + o.id + " changed; still " + o.status }];
    }
    return [subscriptionStarted(o, when)];
  }

  if (e.type === "customer.subscription.deleted") {
    var cust = customerId(o.customer);
    if (!cust) return [{ type: "log", message: "subscription " + o.id + " has no customer" }];
    var props = plan(o);
    props.subscription_id = o.id;
    props.status = o.status;
    if (o.cancellation_details) props.reason = o.cancellation_details.reason || o.cancellation_details.feedback || null;
    return [{
      type: "track_event", metric: "Subscription Cancelled",
      profile: { property: "stripe_customer_id", value: cust, if_missing: "retry" },
      time: when, unique_id: "stripe:subscription:" + o.id + ":cancelled",
      properties: clean(props),
    }];
  }

  return [{ type: "log", message: "ignored Stripe event " + e.type }];
}
