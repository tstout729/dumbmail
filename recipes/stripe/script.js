// Stripe → DumbMail
//
// checkout.session.completed (one-time payments) and invoice.paid (subscription invoices) →
// Placed Order; charge.refunded → Refunded Order; customer.subscription.created/deleted (and
// first subscription invoices) → Subscription Started / Subscription Cancelled.
//
// Profiles are matched by email. The Stripe customer id is saved as the stripe_customer_id
// property, so subscription events (which carry no email) find the same person. People are
// subscribed to email only when they ticked Stripe Checkout's promotional-emails box
// (consent_collection.promotions); otherwise consent is left alone.

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

function profile(email, name, phone, customer, consent) {
  var p = { type: "upsert_profile", email: email };
  var n = names(name);
  if (n.first_name) p.first_name = n.first_name;
  if (n.last_name) p.last_name = n.last_name;
  if (phone && /^\+[0-9]{8,15}$/.test(phone)) p.phone_number = phone;
  if (customer) p.properties = { stripe_customer_id: customer };
  if (consent) p.consent = [consent];
  return p;
}

function customerId(c) {
  return c && typeof c === "object" ? c.id : c || null;
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

function handle(input) {
  var e = input.payload || {};
  if (!e.type || !e.data || !e.data.object) return [{ type: "log", message: "not a Stripe event" }];
  var o = e.data.object;
  var when = iso(e.created);

  if (e.type === "checkout.session.completed") {
    if (o.payment_status !== "paid" && o.payment_status !== "no_payment_required") {
      return [{ type: "log", message: "checkout " + o.id + " isn't paid yet (" + o.payment_status + ")" }];
    }
    var cd = o.customer_details || {};
    var email = cd.email || o.customer_email;
    if (!email) return [{ type: "log", message: "checkout " + o.id + " has no email" }];
    var consent = o.consent && o.consent.promotions === "opt_in" ? { channel: "email", subscribed: true, source: "Stripe Checkout promotional emails checkbox" } : null;
    var out = [profile(email, cd.name, cd.phone, customerId(o.customer), consent)];
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
    var acts = [profile(o.customer_email, o.customer_name, o.customer_phone, customerId(o.customer), null)];
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

  if (e.type === "charge.refunded") {
    var bd = o.billing_details || {};
    var to = bd.email || o.receipt_email;
    if (!to) return [{ type: "log", message: "refunded charge " + o.id + " has no email" }];
    return [
      profile(to, bd.name, bd.phone, customerId(o.customer), null),
      {
        type: "track_event", metric: "Refunded Order", profile: { email: to },
        value: amount(o.amount_refunded, o.currency), time: when,
        // Each further partial refund is a new event.
        unique_id: "stripe:refund:" + o.id + ":" + o.amount_refunded,
        properties: clean({ charge_id: o.id, payment_intent: customerId(o.payment_intent), currency: o.currency, fully_refunded: !!o.refunded, amount_charged: amount(o.amount, o.currency) }),
      },
    ];
  }

  if (e.type === "customer.subscription.created" || e.type === "customer.subscription.deleted") {
    var started = e.type === "customer.subscription.created";
    var cust = customerId(o.customer);
    if (!cust) return [{ type: "log", message: "subscription " + o.id + " has no customer" }];
    var props = plan(o);
    props.subscription_id = o.id;
    props.status = o.status;
    if (!started && o.cancellation_details) props.reason = o.cancellation_details.reason || o.cancellation_details.feedback || null;
    return [{
      type: "track_event", metric: started ? "Subscription Started" : "Subscription Cancelled",
      // No email on subscriptions: find the person by the customer id saved from checkout or invoices.
      profile: { property: "stripe_customer_id", value: cust },
      time: when, unique_id: "stripe:subscription:" + o.id + (started ? ":started" : ":cancelled"),
      properties: clean(props),
    }];
  }

  return [{ type: "log", message: "ignored Stripe event " + e.type }];
}
