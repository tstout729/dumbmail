// Eventbrite → DumbMail
//
// Eventbrite's webhooks aren't signed and carry only a pointer: `{"api_url":
// "https://www.eventbriteapi.com/v3/orders/1234567890/", "config": {"action": "order.placed", ...}}`.
// Eventbrite's docs say to keep the webhook URL private, and it is: the integration's URL is the
// secret. Even so, nothing in a webhook is trusted: the order or attendee it points to is fetched
// from Eventbrite's API with your token, and only that answer is recorded (the action named in the
// webhook only decides what to look up). See README.md for the Eventbrite page behind each
// assumption.
//
// Placed Order: an order Eventbrite reports as placed with a gross above zero, once, in its
// currency. Registered for Event: a free order, once. Refunded Order: once per ticket (attendee)
// Eventbrite marks refunded, its own gross, on the buyer (found by the order); the order's tickets
// are looked up when Eventbrite says it was refunded. Checked In: once per attendee Eventbrite reports checked in, on the attendee's
// profile. Buyers and attendees are held back from marketing: a ticket isn't consent. Barcodes
// (they are the tickets), answers to custom questions and addresses are never stored.

var SITE = "https://www.eventbriteapi.com";

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

function money(c) {
  if (!c || typeof c !== "object") return null;
  var v = parseFloat(c.major_value);
  return isFinite(v) && c.currency ? { value: Math.round(v * 100) / 100, currency: String(c.currency) } : null;
}

function orderActions(o, wantId) {
  if (!o || String(o.id) !== wantId) return [{ type: "log", message: "the answer isn't order " + wantId }];
  if (!o.email) return [{ type: "log", message: "order " + wantId + " has no buyer email" }];
  var who = { email: String(o.email) };
  var p = { type: "upsert_profile", email: String(o.email), keys: { eventbrite_order: wantId } };
  if (o.first_name) p.first_name = String(o.first_name);
  if (o.last_name) p.last_name = String(o.last_name);
  var out = [p];
  var gross = money(o.costs && o.costs.gross);
  var props = clean({ order_id: wantId, event_id: o.event_id ? String(o.event_id) : null });
  if (o.status !== "placed") {
    out.push({ type: "log", message: "order " + wantId + " is " + (o.status || "without a status") + "; it's recorded once it's placed" });
  } else if (gross && gross.value > 0) {
    out.push({ type: "track_event", metric: "Placed Order", profile: who, value: gross.value, time: o.created, unique_id: "eventbrite:order:" + wantId + ":placed", properties: clean(Object.assign({ currency: gross.currency }, props)) });
  } else if (gross && gross.value === 0) {
    out.push({ type: "track_event", metric: "Registered for Event", profile: who, time: o.created, unique_id: "eventbrite:order:" + wantId + ":registered", properties: props });
  } else {
    out.push({ type: "log", message: "order " + wantId + " has no gross amount" });
  }
  return out;
}

// An order's tickets, looked up when Eventbrite says the order was refunded. The refund goes on the
// buyer, found by the order (kept on their profile when the order was recorded); the webhook looked
// the order up too, so a buyer DumbMail hadn't seen yet is added, and the refund waits for them.
function refunds(list, orderId) {
  var out = [];
  (Array.isArray(list.attendees) ? list.attendees : []).forEach(function (a) {
    if (a.refunded !== true || String(a.order_id) !== orderId || !/^[0-9]{1,20}$/.test(String(a.id))) return;
    var gross = money(a.costs && a.costs.gross);
    if (!gross || !(gross.value > 0)) return;
    out.push({
      type: "track_event", metric: "Refunded Order", profile: { property: "eventbrite_order", value: orderId, if_missing: "retry" }, value: gross.value, time: a.changed,
      unique_id: "eventbrite:attendee:" + a.id + ":refunded", properties: clean({ order_id: orderId, attendee_id: String(a.id), event_id: a.event_id ? String(a.event_id) : null, currency: gross.currency }),
    });
  });
  if (list.pagination && list.pagination.has_more_items) out.push({ type: "log", message: "order " + orderId + " has more than one page of tickets; only the first was checked" });
  if (!out.length) out.push({ type: "log", message: "no refunded ticket in order " + orderId });
  return out;
}

function attendee(a, eventId, wantId) {
  if (!a || String(a.id) !== wantId) return [{ type: "log", message: "the answer isn't attendee " + wantId }];
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
  var m;
  if ((m = /^\/v3\/orders\/([0-9]{1,20})\/attendees\/$/.exec(url))) return refunds(p, m[1]);
  if ((m = /^\/v3\/orders\/([0-9]{1,20})\/$/.exec(url))) return orderActions(p, m[1]);
  if ((m = /^\/v3\/events\/([0-9]{1,20})\/attendees\/([0-9]{1,20})\/$/.exec(url))) return attendee(p, m[1], m[2]);
  return [{ type: "log", message: "unexpected answer from " + url }];
}

function handle(input) {
  if (input.source === "fetch") return fetched(input);
  if (input.source === "pull") return [{ type: "log", message: "this recipe records Eventbrite's webhooks; it doesn't import history" }];
  var e = input.payload || {};
  var action = String((e.config && e.config.action) || "");
  var api = String(e.api_url || "");
  if (api.indexOf(SITE + "/v3/") !== 0) return [{ type: "log", message: "the webhook doesn't point at Eventbrite's API" }];
  var path = api.slice(SITE.length).split("?")[0];
  var m;
  if ((m = /^\/v3\/orders\/([0-9]{1,20})\/?$/.exec(path))) {
    if (["order.placed", "order.updated", "order.refunded"].indexOf(action) < 0) return [{ type: "log", message: "ignored Eventbrite action " + (action || "(none)") }];
    // A refund is per ticket: Eventbrite marks each refunded attendee.
    if (action === "order.refunded") return [{ type: "lookup", url: "/v3/orders/" + m[1] + "/" }, { type: "next_request", url: "/v3/orders/" + m[1] + "/attendees/" }];
    return [{ type: "next_request", url: "/v3/orders/" + m[1] + "/" }];
  }
  if ((m = /^\/v3\/events\/([0-9]{1,20})\/attendees\/([0-9]{1,20})\/?$/.exec(path))) {
    if (["attendee.checked_in", "barcode.checked_in"].indexOf(action) < 0) return [{ type: "log", message: "ignored Eventbrite action " + (action || "(none)") }];
    return [{ type: "next_request", url: "/v3/events/" + m[1] + "/attendees/" + m[2] + "/" }];
  }
  return [{ type: "log", message: "ignored Eventbrite webhook for " + path }];
}
