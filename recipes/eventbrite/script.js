// Eventbrite → DumbMail
//
// Eventbrite's webhooks aren't signed and carry only a pointer: `{"api_url":
// "https://www.eventbriteapi.com/v3/orders/1234567890/", "config": {"action": "order.placed", ...}}`.
// Eventbrite's docs say to keep the webhook URL private, and it is: the integration's URL is the
// secret. Even so, nothing in a webhook is trusted: the order or attendee it points to is fetched
// from Eventbrite's API with your token, only for the events you list (settings.event_ids), and
// only that answer is recorded. See README.md for the Eventbrite page behind each assumption.
//
// Money only from Eventbrite's own records, in minor units at the currency's real precision:
// - Placed Order: a placed order with a gross above zero and a payment processor fee (Eventbrite's
//   `costs.payment_fee`: the order was paid through a payment processor), once, in its currency.
//   Any other placed order (free, or paid some other way) is Registered for Event, with no value.
// - Refunded Order: once per processed refund item in the order's refund request (status
//   `processed`, with `amount_processed` and `processed_date`), for its own processed amount.
// Checked In: once per attendee Eventbrite reports checked in, on the attendee's profile. Buyers
// and attendees are held back from marketing: a ticket isn't consent (Eventbrite's
// contact_list_preferences isn't read). Barcodes, answers and addresses are never stored.

var SITE = "https://www.eventbriteapi.com";
var ID = /^[0-9]{1,20}$/;
// ISO 4217 minor units that aren't 2.
var EXPONENT = { BIF: 0, CLP: 0, DJF: 0, GNF: 0, ISK: 0, JPY: 0, KMF: 0, KRW: 0, PYG: 0, RWF: 0, UGX: 0, UYI: 0, VND: 0, VUV: 0, XAF: 0, XOF: 0, XPF: 0, BHD: 3, IQD: 3, JOD: 3, KWD: 3, LYD: 3, OMR: 3, TND: 3, CLF: 4, UYW: 4 };

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

// An Eventbrite currency object: `value` is a whole number of the currency's minor units.
function money(c) {
  if (!c || typeof c !== "object" || !/^[A-Z]{3}$/.test(String(c.currency || ""))) return null;
  if (typeof c.value !== "number" || !Number.isSafeInteger(c.value)) return null;
  var exp = EXPONENT[c.currency] === undefined ? 2 : EXPONENT[c.currency];
  return { minor: c.value, value: c.value / Math.pow(10, exp), currency: String(c.currency) };
}

function inScope(settings, eventId) {
  var ids = Array.isArray(settings.event_ids) ? settings.event_ids.map(String) : [];
  return ids.length > 0 && ids.indexOf(String(eventId)) >= 0;
}

// Refund items Eventbrite has processed, from the order's refund request.
function processedRefunds(o) {
  var rr = o.refund_requests !== undefined ? o.refund_requests : o.refund_request;
  var list = Array.isArray(rr) ? rr : rr && typeof rr === "object" ? [rr] : [];
  var items = [];
  list.forEach(function (r) {
    (Array.isArray(r.items) ? r.items : []).forEach(function (it) {
      if (it.status !== "processed" || !it.processed_date) return;
      if (it.order_id !== null && it.order_id !== undefined && String(it.order_id) !== String(o.id)) return;
      var amount = money(it.amount_processed);
      if (amount && amount.minor > 0) items.push({ item: it, amount: amount });
    });
  });
  return items;
}

function orderActions(o, wantId, settings) {
  if (!o || String(o.id) !== wantId) return [{ type: "log", message: "the answer isn't order " + wantId }];
  if (!inScope(settings, o.event_id)) return [{ type: "log", message: "order " + wantId + " is for an event this integration doesn't record (event_ids)" }];
  if (!o.email) return [{ type: "log", message: "order " + wantId + " has no buyer email" }];
  var who = { email: String(o.email) };
  var p = { type: "upsert_profile", email: String(o.email) };
  if (o.first_name) p.first_name = String(o.first_name);
  if (o.last_name) p.last_name = String(o.last_name);
  var out = [p];
  var props = clean({ order_id: wantId, event_id: String(o.event_id) });
  var costs = o.costs || {};
  var gross = money(costs.gross);
  var fee = money(costs.payment_fee);
  if (o.status !== "placed") {
    out.push({ type: "log", message: "order " + wantId + " is " + (o.status || "without a status") + "; it's recorded once it's placed" });
  } else if (gross && gross.minor > 0 && fee && fee.minor > 0 && fee.currency === gross.currency) {
    out.push({ type: "track_event", metric: "Placed Order", profile: who, value: gross.value, time: o.created, unique_id: "eventbrite:order:" + wantId + ":placed", properties: clean(Object.assign({ currency: gross.currency }, props)) });
  } else {
    out.push({ type: "track_event", metric: "Registered for Event", profile: who, time: o.created, unique_id: "eventbrite:order:" + wantId + ":registered", properties: props });
  }
  processedRefunds(o).forEach(function (r) {
    var it = r.item;
    out.push({
      type: "track_event", metric: "Refunded Order", profile: who, value: r.amount.value, time: it.processed_date,
      // Eventbrite gives a refund item no id of its own: its order, kind, processing time and amount identify it.
      unique_id: "eventbrite:refund:" + wantId + ":" + String(it.item_type || "") + ":" + String(it.processed_date) + ":" + r.amount.minor + r.amount.currency,
      properties: clean(Object.assign({ currency: r.amount.currency, refund_type: it.item_type, quantity: it.quantity_processed }, props)),
    });
  });
  return out;
}

function attendee(a, eventId, wantId, settings) {
  if (!a || String(a.id) !== wantId || String(a.event_id) !== eventId) return [{ type: "log", message: "the answer isn't attendee " + wantId + " of event " + eventId }];
  if (!inScope(settings, eventId)) return [{ type: "log", message: "attendee " + wantId + " is for an event this integration doesn't record (event_ids)" }];
  var email = a.profile && a.profile.email;
  if (!email) return [{ type: "log", message: "attendee " + wantId + " has no email" }];
  if (a.checked_in !== true) return [{ type: "log", message: "attendee " + wantId + " isn't checked in" }];
  return [
    { type: "upsert_profile", email: String(email) },
    {
      type: "track_event", metric: "Checked In", profile: { email: String(email) }, time: a.changed, unique_id: "eventbrite:attendee:" + wantId + ":checked_in",
      properties: clean({ event_id: eventId, order_id: a.order_id ? String(a.order_id) : null, attendee_id: wantId, ticket_class: a.ticket_class_name }),
    },
  ];
}

function fetched(input) {
  var url = String((input.request && input.request.url) || "").replace(/^https?:\/\/[^\/]+/, "").split("?")[0];
  var p = input.payload || {};
  var s = input.settings || {};
  var m;
  if ((m = /^\/v3\/orders\/([0-9]{1,20})\/$/.exec(url))) return orderActions(p, m[1], s);
  if ((m = /^\/v3\/events\/([0-9]{1,20})\/attendees\/([0-9]{1,20})\/$/.exec(url))) return attendee(p, m[1], m[2], s);
  return [{ type: "log", message: "unexpected answer from " + url }];
}

function handle(input) {
  if (input.source === "fetch") return fetched(input);
  if (input.source === "pull") return [{ type: "log", message: "this recipe records Eventbrite's webhooks; it doesn't import history" }];
  var s = input.settings || {};
  if (!Array.isArray(s.event_ids) || !s.event_ids.length) return [{ type: "log", message: "set event_ids in the integration's settings to the events to record" }];
  var e = input.payload || {};
  var action = String((e.config && e.config.action) || "");
  var api = String(e.api_url || "");
  if (api.indexOf(SITE + "/v3/") !== 0) return [{ type: "log", message: "the webhook doesn't point at Eventbrite's API" }];
  var path = api.slice(SITE.length).split("?")[0];
  var m;
  if ((m = /^\/v3\/orders\/([0-9]{1,20})\/?$/.exec(path))) {
    if (["order.placed", "order.updated", "order.refunded"].indexOf(action) < 0) return [{ type: "log", message: "ignored Eventbrite action " + (action || "(none)") }];
    // The order with its refund request (refunds are read from it, whichever action this was).
    return [{ type: "next_request", url: "/v3/orders/" + m[1] + "/", query: { expand: "refund_requests" } }];
  }
  if ((m = /^\/v3\/events\/([0-9]{1,20})\/attendees\/([0-9]{1,20})\/?$/.exec(path))) {
    if (["attendee.checked_in", "barcode.checked_in"].indexOf(action) < 0) return [{ type: "log", message: "ignored Eventbrite action " + (action || "(none)") }];
    if (!inScope(s, m[1])) return [{ type: "log", message: "ignored a check-in for an event this integration doesn't record" }];
    return [{ type: "next_request", url: "/v3/events/" + m[1] + "/attendees/" + m[2] + "/" }];
  }
  return [{ type: "log", message: "ignored Eventbrite webhook for " + path }];
}
