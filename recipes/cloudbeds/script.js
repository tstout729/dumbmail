// Cloudbeds → DumbMail
//
// Cloudbeds' reservation webhooks are thin (`{"event": "reservation/created", "reservationID":
// "31415926", "propertyID": 12345, "timestamp": ...}`) and unsigned: they're checked by the secret
// header given when the webhook was created, and their `timestamp` (unsigned) isn't used. Each one
// only names a reservation; DumbMail fetches it from Cloudbeds' API (GET /api/v1.3/getReservation)
// and everything recorded comes from that answer. An hourly check (the pull) lists confirmed
// reservations arriving in the next few days and fetches each one again, so the payment evidence
// stays fresh. See README.md for the Cloudbeds page behind each assumption.
//
// Each fetched reservation is kept as a `cloudbeds_reservation` object (its own type, so flows
// built for other booking systems never act on it), versioned by when DumbMail sent the request
// for it: an answer to an earlier request never overwrites a later one, and a deletion beats every
// answer requested before it was handled. Every field is written each time, a missing one cleared.
// Paid in full only on exact evidence, compared exactly in decimal (no floating point): a booked
// status, a total above zero, a grand total equal to it, a balance of exactly zero and payments
// equal to the total. Anything missing or inexact is not paid.
// Events: Cloudbeds Reservation Confirmed (once per reservation), Cloudbeds Reservation Cancelled
// (once), Cloudbeds Checked Out (once). They carry no money: revenue would need Cloudbeds' posted
// transactions, which this recipe doesn't read. Guests are held back from marketing: Cloudbeds has
// no marketing opt-in. Documents, addresses, cards and notes are never kept.

var BOOKED = ["confirmed", "checked_in", "checked_out"];
var ID = /^[A-Za-z0-9]{1,40}$/;
var PATH = "/api/v1.3/getReservation";
// The pull lists 100 a page (Cloudbeds' maximum); a lookup per row.
var PAGE = 100;
var TYPE = "cloudbeds_reservation";

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
  var m = /^(\d{4}-\d\d-\d\d)$/.exec(String(s || "").slice(0, 10));
  return m ? m[1] : null;
}

function e164(p) {
  if (!p) return undefined;
  var s = String(p).trim();
  var d = s.replace(/[^0-9]/g, "");
  return s.charAt(0) === "+" && d.length >= 8 && d.length <= 15 ? "+" + d : undefined;
}

// An amount as an exact whole number of millionths (a BigInt), from a JSON number or a plain
// decimal string; anything else (exponents, more than six decimals, text) is null.
function exact(v) {
  var s = typeof v === "number" && isFinite(v) ? String(v) : typeof v === "string" ? v.trim() : null;
  var m = s === null ? null : /^(-?)(\d{1,15})(?:\.(\d{1,6}))?$/.exec(s);
  if (!m) return null;
  var n = BigInt(m[2]) * 1000000n + BigInt(((m[3] || "") + "000000").slice(0, 6));
  return m[1] === "-" ? -n : n;
}

// Exact evidence only: a booked status, a total above zero, balanceDetailed's grand total equal to
// it, a balance of exactly zero and payments equal to the total.
function paidInFull(d) {
  if (BOOKED.indexOf(String(d.status || "")) < 0) return false;
  var total = exact(d.total);
  if (total === null || total <= 0n) return false;
  var balance = exact(d.balance);
  if (balance === null || balance !== 0n) return false;
  var b = d.balanceDetailed;
  if (!b || typeof b !== "object" || Array.isArray(b)) return false;
  var grand = exact(b.grandTotal);
  var paid = exact(b.paid);
  return grand !== null && paid !== null && grand === total && paid === total;
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
  return seen ? n : null;
}

function nights(a, b) {
  var x = Date.parse(a + "T00:00:00Z");
  var y = Date.parse(b + "T00:00:00Z");
  return isFinite(x) && isFinite(y) && y > x ? Math.round((y - x) / 86400000) : null;
}

// The query of the request a fetch made: its `query` pairs (and any in its URL).
function requestQuery(req) {
  var q = {};
  var url = String((req && req.url) || "");
  var i = url.indexOf("?");
  if (i >= 0) url.slice(i + 1).split("&").forEach(function (kv) { var p = kv.split("="); if (p[0]) q[decodeURIComponent(p[0])] = decodeURIComponent(p.slice(1).join("=")); });
  ((req && Array.isArray(req.query)) ? req.query : []).forEach(function (kv) { if (Array.isArray(kv) && kv.length === 2) q[String(kv[0])] = String(kv[1]); });
  return q;
}

// The reservation as Cloudbeds answered a request DumbMail sent at `asked`.
function reservation(d, asked, settings) {
  var id = str(d.reservationID);
  if (settings.property_id && str(d.propertyID) !== String(settings.property_id)) return [{ type: "log", message: "reservation " + id + " is for another property" }];
  var status = str(d.status);
  var start = day(d.startDate);
  var end = day(d.endDate);
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
  // Every field, every time: what this answer doesn't say is cleared, never left from before.
  var fields = {
    reservation_id: id, status: status, property_id: str(d.propertyID), check_in: start, check_out: end, nights: start && end ? nights(start, end) : null,
    guests: guests(d), total: typeof d.total === "number" ? d.total : null, balance: typeof d.balance === "number" ? d.balance : null,
    source_channel: str(d.source), paid_in_full: paidInFull(d), paid_checked_at: asked,
  };
  out.push({
    type: "upsert_object", object_type: TYPE, external_id: "cloudbeds:" + id, fields: fields,
    profiles: email ? [{ email: email, relationship: "guest" }] : [], replace_links: ["guest"], as_of: asked,
  });
  if (!email) {
    out.push({ type: "log", message: "reservation " + id + " has no guest email; only its object is kept" });
    return out;
  }
  var who = { email: email };
  var props = clean({ reservation_id: id, status: status, property_id: str(d.propertyID), check_in: start, check_out: end, nights: fields.nights, guests: fields.guests, source_channel: str(d.source) });
  var created = iso(d.dateCreated) || iso(d.dateModified);
  if (BOOKED.indexOf(status) >= 0) {
    out.push({ type: "track_event", metric: "Cloudbeds Reservation Confirmed", profile: who, time: created, unique_id: "cloudbeds:" + id + ":confirmed", properties: props });
    if (status === "checked_out") out.push({ type: "track_event", metric: "Cloudbeds Checked Out", profile: who, time: iso(d.dateModified) || asked, unique_id: "cloudbeds:" + id + ":checked_out", properties: props });
  } else if (status === "canceled") {
    out.push({ type: "track_event", metric: "Cloudbeds Reservation Cancelled", profile: who, time: iso(d.dateCancelledUTC) || iso(d.dateModified) || asked, unique_id: "cloudbeds:" + id + ":cancelled", properties: props });
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

function fetched(input) {
  var body = input.payload || {};
  var want = requestQuery(input.request).reservationID;
  var asked = iso(input.request && input.request.sent_at);
  if (!want || !asked) return [{ type: "log", message: "not a lookup DumbMail made (no reservation id or request time)" }];
  // A deleted (or unknown) reservation isn't returned: nothing is written, so it stays as it was.
  if (body.success !== true || !body.data) return [{ type: "log", message: "Cloudbeds didn't return reservation " + want }];
  if (str(body.data.reservationID) !== want) return [{ type: "log", message: "the answer isn't reservation " + want }];
  return reservation(body.data, asked, input.settings || {});
}

// Webhooks that change what's recorded; the reservation is fetched for each.
var FETCH_ON = ["reservation/created", "reservation/status_changed", "reservation/dates_changed", "reservation/accommodation_changed", "reservation/accommodation_type_changed"];

function handle(input) {
  var s = input.settings || {};
  if (input.source === "pull") return swept(input);
  if (input.source === "fetch") return fetched(input);
  var e = input.payload || {};
  var ev = String(e.event || "");
  var id = str(e.reservationID !== undefined ? e.reservationID : e.reservationId);
  var prop = str(e.propertyID !== undefined ? e.propertyID : e.propertyId);
  if (ev.indexOf("reservation/") !== 0) return [{ type: "log", message: "ignored Cloudbeds event " + (ev || "(none)") }];
  if (!id || !ID.test(id)) return [{ type: "log", message: "ignored " + ev + " without a reservation id" }];
  if (s.property_id && prop !== String(s.property_id)) return [{ type: "log", message: "ignored " + ev + " for another property (" + prop + ")" }];
  // A deleted reservation can't be fetched any more. It's marked deleted (not paid) as of when this
  // webhook is handled, which is after every request for it sent before then: none of their
  // answers can bring it back. Only a later answer in which Cloudbeds returns it again does.
  if (ev === "reservation/deleted") {
    var at = iso(input.now);
    return [{
      type: "upsert_object", object_type: TYPE, external_id: "cloudbeds:" + id,
      fields: { reservation_id: id, status: "deleted", check_in: null, check_out: null, nights: null, paid_in_full: false, paid_checked_at: at }, as_of: at,
    }];
  }
  if (FETCH_ON.indexOf(ev) < 0) return [{ type: "log", message: "ignored Cloudbeds event " + ev }];
  return [{ type: "next_request", url: PATH, query: lookup(id, prop) }];
}
