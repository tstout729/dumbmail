// Hostaway → DumbMail (pull)
//
// Hostaway's unified webhooks don't document their body, so this recipe reads reservations from
// the API every 15 minutes instead: GET /v1/reservations, newest change first (updatedOn DESC, the
// default), paged with afterId, stopping at the newest change the last run read.
// See README.md for the Hostaway page behind each assumption.
//
// Reservation Booked: once per reservation. Reservation Total: its value, once.
// Reservation Confirmed: one per version (Hostaway's updatedOn), with the dates, listing, guests and
//   whether it's paid in full (as Hostaway reports it). Pre-arrival flows key on reservation_id and
//   follow the newest version.
// Reservation Cancelled: cancelled, declined or expired, dated by updatedOn.
// Inquiry Received: inquiries. Owner stays are skipped (setting skip_owner_stays).
// Door codes, access notes and wifi details are never put in events or profiles.

var BOOKED = ["new", "modified", "confirmed"];
var CANCELLED = ["cancelled", "declined", "expired"];

// Hostaway writes "2019-12-02 17:16:28"; read as UTC.
function iso(s) {
  if (!s) return undefined;
  var t = Date.parse(String(s).replace(" ", "T") + (/[zZ+]/.test(String(s).slice(10)) ? "" : "Z"));
  return isFinite(t) ? new Date(t).toISOString() : undefined;
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

function paidInFull(r) {
  if (r.isPaid === true || r.isPaid === 1) return true;
  return typeof r.remainingBalance === "number" && r.remainingBalance <= 0 && r.totalPrice > 0;
}

function reservation(r, historical, settings) {
  var id = String(r.id);
  var status = String(r.status || "");
  if (status === "ownerStay" && settings.skip_owner_stays !== false) return [{ type: "log", message: "skipped owner stay " + id }];
  var updated = iso(r.updatedOn) || iso(r.reservationDate);
  if (!updated) return [{ type: "log", message: "reservation " + id + " has no update time, so it can't be put in order" }];
  if (!r.guestEmail) return [{ type: "log", message: "reservation " + id + " has no guest email" }];
  var who = { email: r.guestEmail };
  var p = { type: "upsert_profile", email: r.guestEmail, keys: { hostaway_reservation: id } };
  if (r.guestFirstName) p.first_name = r.guestFirstName;
  if (r.guestLastName) p.last_name = r.guestLastName;
  var ph = e164(r.phone);
  if (ph) p.phone_number = ph;
  var props = clean({
    reservation_id: id, confirmation_code: r.confirmationCode, listing_id: r.listingMapId, channel: r.channelName,
    status: status, check_in: r.arrivalDate, check_out: r.departureDate, nights: r.nights, guests: r.numberOfGuests,
    paid_in_full: paidInFull(r), historical: historical || undefined,
  });
  var out = [p];
  if (status.indexOf("inquiry") === 0) {
    out.push({ type: "track_event", metric: "Inquiry Received", profile: who, time: iso(r.reservationDate) || updated, unique_id: "hostaway:" + id + ":inquiry", properties: props });
    return out;
  }
  if (CANCELLED.indexOf(status) >= 0) {
    out.push({ type: "track_event", metric: "Reservation Cancelled", profile: who, time: updated, unique_id: "hostaway:" + id + ":cancelled:" + updated, properties: props });
    return out;
  }
  if (BOOKED.indexOf(status) < 0) {
    out.push({ type: "log", message: "reservation " + id + " is " + status + "; it's recorded once it's booked" });
    return out;
  }
  out.push({ type: "track_event", metric: "Reservation Booked", profile: who, time: iso(r.reservationDate) || updated, unique_id: "hostaway:" + id + ":booked", properties: props });
  if (r.totalPrice > 0) {
    out.push({ type: "track_event", metric: "Reservation Total", profile: who, value: r.totalPrice, time: iso(r.reservationDate) || updated, unique_id: "hostaway:" + id + ":total", properties: clean({ reservation_id: id, currency: r.currency, check_in: r.arrivalDate, check_out: r.departureDate, historical: historical || undefined }) });
  }
  out.push({ type: "track_event", metric: "Reservation Confirmed", profile: who, time: updated, unique_id: "hostaway:" + id + ":confirmed:" + updated, properties: props });
  return out;
}

var LIMIT = 100;

function handle(input) {
  if (input.source !== "pull") return [{ type: "log", message: "this recipe reads Hostaway's API; webhooks aren't used" }];
  var body = input.payload || {};
  if (body.status && body.status !== "success") return [{ type: "log", message: "Hostaway answered status " + body.status }];
  var rows = Array.isArray(body.result) ? body.result : [];
  var st = input.state || {};
  var first = input.page === 1;
  // Read back to the newest change the last run saw (or the backfill's start date).
  var stop = input.backfill ? iso(input.backfill.since) : st.cursor || new Date(Date.parse(input.now) - 86400000).toISOString();
  var newest = first ? (rows.length ? iso(rows[0].updatedOn) : undefined) : st.pending;
  var out = [];
  var done = rows.length < LIMIT;
  for (var i = 0; i < rows.length; i++) {
    var u = iso(rows[i].updatedOn);
    if (u && stop && u <= stop) { done = true; break; }
    out = out.concat(reservation(rows[i], !!input.backfill, input.settings || {}));
  }
  if (done) {
    var values = { pending: null };
    if (newest && (!st.cursor || newest > st.cursor)) values.cursor = newest;
    out.push({ type: "set_state", values: values });
  } else {
    out.push({ type: "set_state", values: { pending: newest || null } });
    out.push({ type: "next_request", query: { afterId: String(rows[rows.length - 1].id) } });
  }
  return out;
}
