// Recharge → DumbMail
//
// Recharge's webhook body is the resource itself ({"subscription": …}, {"charge": …},
// {"customer": …}), the same as a GET of it, with no topic in it. So each webhook is created with
// its topic in the URL (…?topic=charge/paid) and the script reads input.query.topic.
// See README.md for the Recharge page behind each assumption.
//
// Subscription Started, Subscription Reactivated, Subscription Cancelled, Upcoming Charge, Charge
// Paid, Charge Failed and Charge Skipped. Ids come from Recharge's
// own ids and times, so a resend never records twice. People come from charges and customers
// (they carry the email); subscriptions find them by their Recharge customer id and wait (retry)
// when that customer hasn't arrived yet. Nobody is subscribed to marketing.

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

function num(s) {
  var n = parseFloat(s);
  return isFinite(n) ? n : undefined;
}

function phone(p) {
  if (!p) return undefined;
  var s = String(p).trim();
  var d = s.replace(/[^0-9]/g, "");
  return s.charAt(0) === "+" && d.length >= 8 && d.length <= 15 ? "+" + d : undefined;
}

function byCustomer(id) {
  return { property: "recharge_customer", value: String(id), if_missing: "retry" };
}

function subProps(s) {
  return clean({
    subscription_id: s.id, product_title: s.product_title || s.title, variant_title: s.variant_title, sku: s.sku,
    price: num(s.price), currency: s.presentment_currency || s.currency, quantity: s.quantity,
    interval_unit: s.order_interval_unit, interval_frequency: num(s.order_interval_frequency),
    next_charge_scheduled_at: s.next_charge_scheduled_at, status: s.status,
    cancellation_reason: s.cancellation_reason,
  });
}

function chargeProps(c) {
  return clean({
    charge_id: c.id, status: c.status, type: c.type, currency: c.currency, total_price: num(c.total_price),
    scheduled_at: c.scheduled_at, processed_at: c.processed_at, error_type: c.error_type,
    items: (c.line_items || []).slice(0, 20).map(function (li) {
      return clean({ title: li.title, variant_title: li.variant_title, sku: li.sku, quantity: li.quantity, subscription_id: li.purchase_item_type === "subscription" ? li.purchase_item_id : undefined });
    }),
  });
}

function customerUpsert(c) {
  var p = { type: "upsert_profile", email: c.email, keys: { recharge_customer: String(c.id) } };
  if (c.first_name) p.first_name = c.first_name;
  if (c.last_name) p.last_name = c.last_name;
  var ph = phone(c.phone);
  if (ph) p.phone_number = ph;
  return p;
}

// The topic comes from the URL, which Recharge doesn't sign, so it's only a hint: what happened is
// read from the signed body (the subscription's or charge's status and times). A replayed body
// relabelled with another topic records only what that body itself shows.
var SKIP_HINT = "a skip is recorded from its charge (charge/updated with status skipped)";

function subscriptionEvents(topic, s) {
  var who = byCustomer(s.customer_id);
  var status = String(s.status || "").toLowerCase();
  var base = "recharge:subscription:" + s.id + ":";
  if (status === "cancelled") {
    var when = s.cancelled_at || s.updated_at;
    if (!when) return [{ type: "log", message: "subscription " + s.id + " has no time from Recharge, so it can't be put in order" }];
    return [{ type: "track_event", metric: "Subscription Cancelled", profile: who, time: when, unique_id: base + "cancelled:" + when, properties: subProps(s) }];
  }
  if (status !== "active") return [{ type: "log", message: "subscription " + s.id + " is " + status + "; not recorded" }];
  if (!s.created_at) return [{ type: "log", message: "subscription " + s.id + " has no creation time" }];
  var out = [{ type: "track_event", metric: "Subscription Started", profile: who, time: s.created_at, unique_id: base + "started", properties: subProps(s) }];
  // A reactivation: the topic says so, and the body shows an active subscription changed well after
  // it was created (a new subscription's body can't be passed off as one).
  var changedLater = s.updated_at && Date.parse(s.updated_at) - Date.parse(s.created_at) > 60000;
  if (topic === "subscription/activated" && changedLater && !s.cancelled_at) {
    out.push({ type: "track_event", metric: "Subscription Reactivated", profile: who, time: s.updated_at, unique_id: base + "activated:" + s.updated_at, properties: subProps(s) });
  }
  if (topic === "subscription/skipped") out.push({ type: "log", message: SKIP_HINT });
  return out;
}

function chargeEvents(topic, ch) {
  var status = String(ch.status || "").toLowerCase();
  var out = [];
  if (ch.customer.email) out.push(customerUpsert(ch.customer));
  var who = ch.customer.email ? { email: ch.customer.email } : byCustomer(ch.customer.id);
  var base = "recharge:charge:" + ch.id + ":";
  if (status === "success") {
    // The stop signal (the charge went through, free or not) is apart from the revenue event.
    out.push({ type: "track_event", metric: "Charge Processed", profile: who, time: ch.processed_at || ch.updated_at, unique_id: base + "processed", properties: chargeProps(ch) });
    if (!(num(ch.total_price) > 0)) out.push({ type: "log", message: "charge " + ch.id + " was free, so it isn't recorded as paid" });
    else out.push({ type: "track_event", metric: "Charge Paid", profile: who, value: num(ch.total_price), time: ch.processed_at || ch.updated_at, unique_id: base + "paid", properties: chargeProps(ch) });
  } else if (status === "error") {
    out.push({ type: "track_event", metric: "Charge Failed", profile: who, time: ch.updated_at, unique_id: base + "failed:" + ch.updated_at, properties: chargeProps(ch) });
  } else if (status === "skipped") {
    out.push({ type: "track_event", metric: "Charge Skipped", profile: who, time: ch.updated_at, unique_id: base + "skipped", properties: chargeProps(ch) });
  } else if (status === "queued" && topic === "charge/upcoming") {
    out.push({ type: "track_event", metric: "Upcoming Charge", profile: who, time: ch.updated_at, unique_id: base + "upcoming:" + ch.scheduled_at, properties: chargeProps(ch) });
  } else {
    out.push({ type: "log", message: "charge " + ch.id + " is " + status + "; not recorded for " + topic });
  }
  return out;
}

function handle(input) {
  var topic = String((input.query && input.query.topic) || "").toLowerCase();
  var p = input.payload || {};
  if (!topic) return [{ type: "log", message: "no topic: create each Recharge webhook with ?topic=<its topic> at the end of the URL" }];
  if (topic.indexOf("customer/") === 0) {
    var c = p.customer;
    if (!c || !c.email) return [{ type: "log", message: "customer without an email" }];
    return [customerUpsert(c)];
  }
  if (topic.indexOf("subscription/") === 0) {
    var s = p.subscription;
    if (!s || !s.customer_id) return [{ type: "log", message: topic + " without a subscription or customer" }];
    return subscriptionEvents(topic, s);
  }
  if (topic.indexOf("charge/") === 0) {
    var ch = p.charge;
    if (!ch || !ch.customer) return [{ type: "log", message: topic + " without a charge or customer" }];
    return chargeEvents(topic, ch);
  }
  return [{ type: "log", message: "ignored Recharge topic " + topic }];
}
