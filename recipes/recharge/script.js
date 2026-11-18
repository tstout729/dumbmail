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

// What signed bodies have shown about each subscription, kept in the integration's state: the
// newest cancellation (x:<id>) and the newest time it was seen active (a:<id>). A reactivation is
// recorded only when they show one: active at a time after a cancellation, whichever arrives first.
var KEEP = 300;

function remember(state, key, when) {
  var values = {};
  var old = state[key];
  if (old && Date.parse(old) >= Date.parse(when)) return values;
  values[key] = when;
  var kind = key.slice(0, 2);
  var keys = Object.keys(state).filter(function (k) { return k.slice(0, 2) === kind && k !== key; });
  if (keys.length >= KEEP) {
    keys.sort(function (a, b) { return Date.parse(state[a]) - Date.parse(state[b]); });
    for (var i = 0; i <= keys.length - KEEP; i++) values[keys[i]] = null;
  }
  return values;
}

function subscriptionEvents(topic, s, state) {
  var who = byCustomer(s.customer_id);
  var status = String(s.status || "").toLowerCase();
  var base = "recharge:subscription:" + s.id + ":";
  var reactivated = function (when) {
    var props = subProps(s);
    props.status = "active";
    delete props.cancellation_reason;
    return { type: "track_event", metric: "Subscription Reactivated", profile: who, time: when, unique_id: base + "activated:" + when, properties: props };
  };
  if (status === "cancelled") {
    var when = s.cancelled_at || s.updated_at;
    if (!when || isNaN(Date.parse(when))) return [{ type: "log", message: "subscription " + s.id + " has no time from Recharge, so it can't be put in order" }];
    var out = [{ type: "track_event", metric: "Subscription Cancelled", profile: who, time: when, unique_id: base + "cancelled:" + when, properties: subProps(s) }];
    // Seen active after this cancellation already (the bodies arrived out of order): that was a reactivation.
    var active = state["a:" + s.id];
    if (active && Date.parse(active) > Date.parse(when)) out.push(reactivated(active));
    out.push({ type: "set_state", values: remember(state, "x:" + s.id, when) });
    return out;
  }
  if (status !== "active") return [{ type: "log", message: "subscription " + s.id + " is " + status + "; not recorded" }];
  if (!s.created_at) return [{ type: "log", message: "subscription " + s.id + " has no creation time" }];
  var out = [{ type: "track_event", metric: "Subscription Started", profile: who, time: s.created_at, unique_id: base + "started", properties: subProps(s) }];
  var seen = s.updated_at && !isNaN(Date.parse(s.updated_at)) ? s.updated_at : null;
  if (seen && !s.cancelled_at) {
    var cancelled = state["x:" + s.id];
    // A cancellation from before this body's creation time isn't this subscription's to undo.
    if (cancelled && Date.parse(seen) > Date.parse(cancelled) && Date.parse(cancelled) > Date.parse(s.created_at)) out.push(reactivated(seen));
    else if (topic === "subscription/activated") out.push({ type: "log", message: "no cancellation of subscription " + s.id + " has been seen before this, so it's recorded as started, not reactivated" });
    out.push({ type: "set_state", values: remember(state, "a:" + s.id, seen) });
  }
  if (topic === "subscription/skipped") out.push({ type: "log", message: SKIP_HINT });
  return out;
}

function chargeEvents(topic, ch, now) {
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
    // Only a charge the body shows still scheduled (today or later) is coming up.
    var day = String(ch.scheduled_at || "").slice(0, 10);
    if (!/^\d{4}-\d{2}-\d{2}$/.test(day) || day < String(now || "").slice(0, 10)) {
      out.push({ type: "log", message: "charge " + ch.id + " isn't scheduled for today or later (" + (day || "no date") + "); not an upcoming charge" });
    } else {
      out.push({ type: "track_event", metric: "Upcoming Charge", profile: who, time: ch.updated_at, unique_id: base + "upcoming:" + day, properties: chargeProps(ch) });
    }
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
    return subscriptionEvents(topic, s, input.state || {});
  }
  if (topic.indexOf("charge/") === 0) {
    var ch = p.charge;
    if (!ch || !ch.customer) return [{ type: "log", message: topic + " without a charge or customer" }];
    return chargeEvents(topic, ch, input.now);
  }
  return [{ type: "log", message: "ignored Recharge topic " + topic }];
}
