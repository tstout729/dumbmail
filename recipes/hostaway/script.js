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
// Reservation Unconfirmed: a version that's pending, awaiting payment or otherwise not booked;
//   the pre-arrival flow is cancelled by it, so an earlier "paid" version can't send.
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

// Paid in full only when every signal agrees: a booked status, a total above zero, a remaining
// balance (sent only with includePayments=1) of exactly zero, and isPaid not saying otherwise.
// Missing or conflicting signals mean not paid.
function paidInFull(r) {
  if (BOOKED.indexOf(String(r.status || "")) < 0) return false;
  if (!(typeof r.totalPrice === "number" && r.totalPrice > 0)) return false;
  if (typeof r.remainingBalance !== "number" || Math.abs(r.remainingBalance) >= 0.005) return false;
  if (r.isPaid === false || r.isPaid === 0) return false;
  return true;
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
    // Pending, awaiting payment and the like: a reminder waiting for an earlier version stops.
    out.push({ type: "track_event", metric: "Reservation Unconfirmed", profile: who, time: updated, unique_id: "hostaway:" + id + ":unconfirmed:" + updated, properties: props });
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

// Reservations come newest change first (updatedOn DESC, id DESC), paged with afterId. A run reads
// back to the boundary the last run saved: its newest change's time and the ids read at that time
// (rows changed in the same second count as read only if their id was). While a run pages, it
// saves where it got to (resume_after); if it stops at the page cap, the next run continues from
// there before starting over from the newest. The boundary moves only when a sweep reaches it.
function handle(input) {
  if (input.source !== "pull") return [{ type: "log", message: "this recipe reads Hostaway's API; webhooks aren't used" }];
  var body = input.payload || {};
  if (body.status && body.status !== "success") return [{ type: "log", message: "Hostaway answered status " + body.status }];
  var rows = Array.isArray(body.result) ? body.result : [];
  var st = input.state || {};
  var first = input.page === 1;
  if (first && st.resume_after) {
    return [{ type: "log", message: "continuing after reservation " + st.resume_after }, { type: "next_request", query: { afterId: String(st.resume_after) } }];
  }
  var stop = input.backfill ? iso(input.backfill.since) : st.cursor || new Date(Date.parse(input.now) - 86400000).toISOString();
  var stopIds = input.backfill ? [] : (st.cursor_ids || []).map(String);
  var newest = st.pending || null;
  var newestIds = st.pending_ids || [];
  if (first && rows.length) {
    newest = iso(rows[0].updatedOn) || null;
    newestIds = rows.filter(function (r) { return iso(r.updatedOn) === newest; }).map(function (r) { return String(r.id); });
  }
  var out = [];
  var done = rows.length < LIMIT;
  for (var i = 0; i < rows.length; i++) {
    var u = iso(rows[i].updatedOn);
    if (u && stop && (u < stop || (u === stop && stopIds.indexOf(String(rows[i].id)) >= 0))) { done = true; break; }
    out = out.concat(reservation(rows[i], !!input.backfill, input.settings || {}));
  }
  if (done) {
    var values = { pending: null, pending_ids: null, resume_after: null };
    if (newest && (!st.cursor || newest > st.cursor)) {
      values.cursor = newest;
      values.cursor_ids = newestIds.slice(0, 100);
    } else if (newest && newest === st.cursor) {
      values.cursor_ids = stopIds.concat(newestIds.filter(function (x) { return stopIds.indexOf(x) < 0; })).slice(0, 100);
    }
    out.push({ type: "set_state", values: values });
  } else {
    var last = rows[rows.length - 1].id;
    out.push({ type: "set_state", values: { pending: newest, pending_ids: newestIds.slice(0, 100), resume_after: String(last) } });
    out.push({ type: "next_request", query: { afterId: String(last) } });
  }
  return out;
}
