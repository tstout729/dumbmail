// Cloudbeds → DumbMail
//
// Cloudbeds' reservation webhooks are thin (`{"event": "reservation/created", "reservationID":
// "31415926", "propertyID": 12345, ...}`) and unsigned: they're checked by the secret header given
// when the webhook was created. So each one only names a reservation, and DumbMail fetches it from
// Cloudbeds' API (GET /api/v1.3/getReservation): everything recorded comes from that answer, never
// from the webhook. Cloudbeds sends no webhook when a payment is taken, so an hourly check (the
// pull) lists confirmed reservations arriving in the next few days and fetches each one again.
// See README.md for the Cloudbeds page behind each assumption.
//
// Each fetched reservation is kept as a `reservation` object (status, dates, nights, guests, total,
// balance, and whether it's paid in full) linked to its main guest, versioned by when DumbMail
// received the answer, so an older answer never overwrites a newer one. Paid in full only on exact
// evidence: a total above zero, a balance of exactly zero, and payments of at least the total;
// anything missing or contradictory is not paid. The pre-arrival flow also wants that evidence to
// be at most three hours old.
// Events: Reservation Booked (once), Reservation Confirmed (once per version of its status and
// dates), Reservation Cancelled (once), Checked Out (once). Guests are held back from marketing:
// Cloudbeds has no marketing opt-in. Documents, addresses, cards and notes are never kept.

var BOOKED = ["confirmed", "checked_in", "checked_out"];
var ID = /^[A-Za-z0-9]{1,40}$/;
var PATH = "/api/v1.3/getReservation";
// Webhooks that change what's recorded; the reservation is fetched for each.
var FETCH_ON = ["reservation/created", "reservation/status_changed", "reservation/dates_changed", "reservation/accommodation_changed", "reservation/accommodation_type_changed"];
// The pull lists 100 a page (Cloudbeds' maximum); a lookup per row.
var PAGE = 100;

function str(v) {
  return v === undefined || v === null || v === "" ? null : String(v);
}

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

// Cloudbeds writes ISO 8601 or "2020-05-19 12:34:56"; a time without a zone is read as UTC.
function iso(s) {
  if (!s) return undefined;
  var t = String(s).trim();
  var p = Date.parse(t.replace(" ", "T") + (/[zZ]|[+-]\d\d:?\d\d$/.test(t.slice(10)) ? "" : "Z"));
  return isFinite(p) ? new Date(p).toISOString() : undefined;
}

function day(s) {
  var m = /^(\d{4}-\d\d-\d\d)/.exec(String(s || ""));
  return m ? m[1] : undefined;
}

function e164(p) {
  if (!p) return undefined;
  var s = String(p).trim();
  var d = s.replace(/[^0-9]/g, "");
  return s.charAt(0) === "+" && d.length >= 8 && d.length <= 15 ? "+" + d : undefined;
}

function cents(n) {
  return typeof n === "number" && isFinite(n) ? Math.round(n * 100) : null;
}

// Exact evidence only: a booked status, a total above zero, a balance of exactly zero, and
// (balanceDetailed) payments of at least the total, with its grand total, when given, equal to it.
function paidInFull(d) {
  if (BOOKED.indexOf(String(d.status || "")) < 0) return false;
  var total = cents(d.total);
  if (!(total > 0)) return false;
  if (typeof d.balance !== "number" || d.balance !== 0) return false;
  var b = d.balanceDetailed;
  if (!b || typeof b !== "object" || Array.isArray(b)) return false;
  var paid = cents(b.paid);
  if (paid === null || paid < total) return false;
  if (b.grandTotal !== undefined && b.grandTotal !== null && cents(b.grandTotal) !== total) return false;
  return true;
}

function mainGuest(d) {
  var list = d.guestList && typeof d.guestList === "object" ? d.guestList : {};
  for (var k in list) if (list[k] && list[k].isMainGuest === true) return list[k];
  return {};
}

function guests(d) {
  var n = 0;
  var seen = false;
  [d.assigned, d.unassigned].forEach(function (rooms) {
    (Array.isArray(rooms) ? rooms : []).forEach(function (r) {
      var a = parseInt(r.adults, 10);
      var c = parseInt(r.children, 10);
      if (isFinite(a)) { n += a; seen = true; }
      if (isFinite(c)) n += c;
    });
  });
  return seen ? n : undefined;
}

function nights(a, b) {
  var x = Date.parse(a + "T00:00:00Z");
  var y = Date.parse(b + "T00:00:00Z");
  return isFinite(x) && isFinite(y) && y > x ? Math.round((y - x) / 86400000) : undefined;
}

// The reservation as fetched at `observed` (when DumbMail received Cloudbeds' answer).
function reservation(d, observed, settings) {
  var id = str(d.reservationID);
  if (!id || !ID.test(id)) return [{ type: "log", message: "no reservation in the answer" }];
  if (settings.property_id && str(d.propertyID) !== String(settings.property_id)) return [{ type: "log", message: "reservation " + id + " is for another property" }];
  var status = String(d.status || "");
  var start = day(d.startDate);
  var end = day(d.endDate);
  var modified = iso(d.dateModified) || iso(d.dateCreated);
  var g = mainGuest(d);
  var email = d.isAnonymized === true ? null : str(d.guestEmail);
  var out = [];
  if (email) {
    var p = { type: "upsert_profile", email: email, keys: { cloudbeds_reservation: id } };
    if (g.guestFirstName) p.first_name = String(g.guestFirstName);
    if (g.guestLastName) p.last_name = String(g.guestLastName);
    var ph = e164(g.guestCellPhone) || e164(g.guestPhone);
    if (ph) p.phone_number = ph;
    out.push(p);
  } else if (d.isAnonymized === true) {
    out.push({ type: "log", message: "reservation " + id + "'s guest was anonymized; no profile is written" });
  }
  var paid = paidInFull(d);
  var fields = clean({
    reservation_id: id, status: status, property_id: str(d.propertyID), check_in: start, check_out: end, nights: nights(start, end),
    guests: guests(d), total: typeof d.total === "number" ? d.total : null, balance: typeof d.balance === "number" ? d.balance : null,
    source_channel: str(d.source),
  });
  fields.paid_in_full = paid;
  fields.paid_checked_at = observed;
  out.push({
    type: "upsert_object", object_type: "reservation", external_id: "cloudbeds:" + id, fields: fields,
    profiles: email ? [{ email: email, relationship: "guest" }] : [], replace_links: ["guest"], as_of: observed,
  });
  if (!email) {
    out.push({ type: "log", message: "reservation " + id + " has no guest email; only its object is kept" });
    return out;
  }
  var who = { email: email };
  var props = clean({ reservation_id: id, status: status, property_id: str(d.propertyID), check_in: start, check_out: end, nights: fields.nights, guests: fields.guests, source_channel: str(d.source) });
  if (BOOKED.indexOf(status) >= 0) {
    var booked = { type: "track_event", metric: "Reservation Booked", profile: who, time: iso(d.dateCreated) || modified, unique_id: "cloudbeds:" + id + ":booked", properties: props };
    // Revenue only in a currency: getReservation doesn't say which, so it comes from the settings.
    if (settings.currency && typeof d.total === "number" && d.total > 0) {
      booked.value = d.total;
      booked.properties = clean(Object.assign({ currency: String(settings.currency) }, props));
    }
    out.push(booked);
    if (status !== "checked_out" && modified && start && end) {
      out.push({ type: "track_event", metric: "Reservation Confirmed", profile: who, time: modified, unique_id: "cloudbeds:" + id + ":confirmed:" + [modified, status, start, end].join(":"), properties: props });
    }
    if (status === "checked_out") out.push({ type: "track_event", metric: "Checked Out", profile: who, time: modified, unique_id: "cloudbeds:" + id + ":checked_out", properties: props });
  } else if (status === "canceled") {
    var when = iso(d.dateCancelledUTC) || modified;
    out.push({ type: "track_event", metric: "Reservation Cancelled", profile: who, time: when, unique_id: "cloudbeds:" + id + ":cancelled", properties: props });
  } else {
    out.push({ type: "log", message: "reservation " + id + " is " + (status || "without a status") + "; only its object is updated" });
  }
  return out;
}

function lookup(id, property) {
  var q = { reservationID: id };
  if (property && ID.test(property)) q.propertyID = property;
  return q;
}

function dateOf(ms) {
  return new Date(ms).toISOString().slice(0, 10);
}

// The hourly check: confirmed reservations arriving from yesterday to payment_check_days ahead
// (dates in UTC, so a day's margin each side). The window is kept in the state, so the request
// uses it; when the date has moved on, the first page asks again with the new window.
function swept(input) {
  var st = input.state || {};
  var s = input.settings || {};
  var days = Math.min(Math.max(parseInt(s.payment_check_days, 10) || 7, 1), 30);
  var now = Date.parse(input.now);
  var from = dateOf(now - 86400000);
  var to = dateOf(now + days * 86400000);
  var page = parseInt(st.page || "1", 10) || 1;
  if (st.from !== from || st.to !== to) {
    return [
      { type: "set_state", values: { from: from, to: to, page: "1" } },
      { type: "next_request", query: { checkInFrom: from, checkInTo: to, pageNumber: "1" } },
    ];
  }
  var body = input.payload || {};
  if (body.success !== true) return [{ type: "set_state", values: { page: "1" } }, { type: "log", message: "Cloudbeds didn't answer with reservations" }];
  var rows = Array.isArray(body.data) ? body.data : [];
  var out = [];
  rows.forEach(function (r) {
    var id = str(r.reservationID);
    if (!id || !ID.test(id)) return;
    if (s.property_id && str(r.propertyID) !== String(s.property_id)) return;
    out.push({ type: "lookup", url: PATH, query: lookup(id, str(r.propertyID)) });
  });
  if (rows.length >= PAGE && page < 10) {
    out.push({ type: "set_state", values: { page: String(page + 1) } });
    out.push({ type: "next_request", query: { pageNumber: String(page + 1) } });
  } else {
    out.push({ type: "set_state", values: { page: "1" } });
    if (!rows.length) out.push({ type: "log", message: "no confirmed arrivals between " + from + " and " + to });
  }
  return out;
}

function handle(input) {
  var s = input.settings || {};
  if (input.source === "pull") return swept(input);
  if (input.source === "fetch") {
    var body = input.payload || {};
    if (body.success !== true || !body.data) return [{ type: "log", message: "Cloudbeds didn't return the reservation" }];
    var observed = iso(input.response && input.response.received_at) || iso(input.now);
    return reservation(body.data, observed, s);
  }
  var e = input.payload || {};
  var ev = String(e.event || "");
  var id = str(e.reservationID !== undefined ? e.reservationID : e.reservationId);
  var prop = str(e.propertyID !== undefined ? e.propertyID : e.propertyId);
  if (ev.indexOf("reservation/") !== 0) return [{ type: "log", message: "ignored Cloudbeds event " + (ev || "(none)") }];
  if (!id || !ID.test(id)) return [{ type: "log", message: "ignored " + ev + " without a reservation id" }];
  if (s.property_id && prop !== String(s.property_id)) return [{ type: "log", message: "ignored " + ev + " for another property (" + prop + ")" }];
  // A deleted reservation can't be fetched any more. It's marked deleted (never paid) as of when
  // this webhook is handled: later than any answer fetched before the deletion (versions are
  // DumbMail's receive times, so Cloudbeds' own clock isn't mixed in), so none can bring it back.
  if (ev === "reservation/deleted") {
    var at = iso(input.now);
    return [{ type: "upsert_object", object_type: "reservation", external_id: "cloudbeds:" + id, fields: { reservation_id: id, status: "deleted", paid_in_full: false, paid_checked_at: at }, as_of: at }];
  }
  if (FETCH_ON.indexOf(ev) < 0) return [{ type: "log", message: "ignored Cloudbeds event " + ev }];
  return [{ type: "next_request", url: PATH, query: lookup(id, prop) }];
}
