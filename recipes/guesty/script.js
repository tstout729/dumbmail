// Guesty → DumbMail
//
// Handles Guesty reservation webhooks (reservation.new and reservation.updated, which carry the
// full reservation with the guest) and pages of GET /v1/reservations from the Open API (scheduled
// pulls and history backfills).
//
// Events: Reservation Confirmed, Reservation Cancelled, Reservation Updated, Inquiry Received.
// Each carries reservation_id (the key flows use), listing, dates, nights, guests, total and the
// booking channel. Guests become profiles; Airbnb/Booking.com/Vrbo relay addresses are suppressed
// from marketing by DumbMail automatically. Nobody is subscribed: Guesty has no marketing opt-in.

var PAGE_SIZE = 100;

function str(v) {
  return v === undefined || v === null ? null : String(v);
}

function day(v) {
  // "2026-11-20T16:00:00.000Z" → "2026-11-20"; already-local dates pass through.
  var s = str(v);
  return s ? s.slice(0, 10) : null;
}

function num(v) {
  var n = Number(v);
  return v === null || v === undefined || v === "" || isNaN(n) ? null : n;
}

function cleanEmail(v) {
  var s = str(v);
  if (!s) return null;
  s = s.trim();
  return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(s) ? s : null;
}

function cleanPhone(v) {
  var s = str(v && typeof v === "object" ? v.number || v.phone : v);
  if (!s) return null;
  var digits = s.replace(/[^0-9]/g, "");
  if (s.trim().charAt(0) === "+" && digits.length >= 8 && digits.length <= 15) return "+" + digits;
  if (digits.length === 10) return "+1" + digits;
  if (digits.length === 11 && digits.charAt(0) === "1") return "+" + digits;
  return null;
}

function guestOf(r) {
  var g = r.guest || {};
  var email = cleanEmail(g.email) || cleanEmail(g.emails && g.emails[0]);
  var phone = cleanPhone(g.phone) || cleanPhone(g.phones && g.phones[0]);
  var first = str(g.firstName);
  var last = str(g.lastName);
  if (!first && !last && g.fullName) {
    var parts = String(g.fullName).trim().split(/\s+/);
    first = parts.shift() || null;
    last = parts.length ? parts.join(" ") : null;
  }
  return { id: str(g._id || r.guestId), email: email, phone: phone, first: first, last: last };
}

function isOwnerStay(r, settings) {
  if (settings.skip_owner_stays === false) return false;
  var src = String(r.source || "").toLowerCase();
  return src === "owner" || src === "owner-guest" || r.reservationType === "owner";
}

function details(r) {
  var m = r.money || {};
  var paid = num(m.totalPaid);
  var due = num(m.balanceDue);
  var total = paid !== null || due !== null ? (paid || 0) + (due || 0) : num(m.hostPayout);
  var listing = r.listing || {};
  var integration = r.integration || {};
  return {
    reservation_id: str(r._id || r.id),
    confirmation_code: str(r.confirmationCode),
    status: str(r.status),
    listing_id: str(r.listingId || listing._id),
    listing_name: str(listing.nickname || listing.title),
    check_in: day(r.checkInDateLocalized || r.checkIn),
    check_out: day(r.checkOutDateLocalized || r.checkOut),
    nights: num(r.nightsCount),
    guests: num(r.guestsCount),
    total: total,
    currency: str(m.currency),
    host_payout: num(m.hostPayout),
    source_channel: str(integration.platform || r.source),
  };
}

// Events for a reservation as it is now. `before` is the reservation before an update (webhooks only).
function reservationActions(r, before, input, isUpdate) {
  var settings = input.settings || {};
  var out = [];
  var d = details(r);
  if (!d.reservation_id) return [{ type: "log", message: "skipped a reservation with no _id" }];
  if (isOwnerStay(r, settings)) return [{ type: "log", message: "skipped owner stay " + d.reservation_id }];
  var g = guestOf(r);
  if (!g.email && !g.phone) {
    return [{ type: "log", message: "reservation " + d.reservation_id + ": the guest has no usable email or phone, so there's no profile to write" }];
  }
  var who = g.email ? { email: g.email } : { phone_number: g.phone };
  var profile = { type: "upsert_profile", properties: { guesty_guest_id: g.id, last_listing: d.listing_name, last_check_in: d.check_in } };
  if (g.email) profile.email = g.email;
  if (g.phone) profile.phone_number = g.phone;
  if (g.first) profile.first_name = g.first;
  if (g.last) profile.last_name = g.last;
  out.push(profile);

  // Old bookings seen for the first time (a backfill, or a first pull) are marked historical so
  // "right after booking" flows skip them. Date-based flows still use them for future stays.
  var confirmedAt = str(r.confirmedAt || r.createdAt);
  var ageDays = confirmedAt ? (Date.parse(input.now) - Date.parse(confirmedAt)) / 86400000 : 0;
  var historical = !!input.backfill || ageDays > 3;
  var props = {};
  for (var k in d) if (d[k] !== null) props[k] = d[k];
  if (historical) props.historical = true;

  var status = String(r.status || "").toLowerCase();
  if (status === "confirmed") {
    // Already confirmed before this update (new dates or guests): send it again so date-based
    // flows follow the change, but without the value, so revenue isn't counted twice.
    var resend = !!(isUpdate && before && String(before.status || "").toLowerCase() === "confirmed");
    var cprops = props;
    if (resend) {
      cprops = {};
      for (var k3 in props) cprops[k3] = props[k3];
      cprops.updated_booking = true;
    }
    out.push({
      type: "track_event", metric: "Reservation Confirmed", profile: who, properties: cprops,
      value: d.total === null || resend ? undefined : d.total, time: confirmedAt || undefined,
      // A change of dates makes a new event, which replaces the old one in date-based flows.
      unique_id: "guesty:" + d.reservation_id + ":confirmed:" + d.check_in + ":" + d.check_out,
    });
  } else if (status === "canceled" || status === "cancelled") {
    out.push({
      type: "track_event", metric: "Reservation Cancelled", profile: who, properties: props,
      time: str(r.canceledAt || r.lastUpdatedAt) || undefined,
      unique_id: "guesty:" + d.reservation_id + ":cancelled",
    });
  } else if (status === "inquiry") {
    out.push({
      type: "track_event", metric: "Inquiry Received", profile: who, properties: props,
      time: str(r.createdAt) || undefined,
      unique_id: "guesty:" + d.reservation_id + ":inquiry",
    });
  }

  if (isUpdate) {
    var changed = [];
    if (before) {
      var b = details(before);
      ["status", "check_in", "check_out", "guests", "total", "listing_id"].forEach(function (f) {
        if (b[f] !== null && b[f] !== d[f]) changed.push(f);
      });
    }
    var up = {};
    for (var k2 in props) up[k2] = props[k2];
    up.changed = changed;
    out.push({
      type: "track_event", metric: "Reservation Updated", profile: who, properties: up,
      time: str(r.lastUpdatedAt) || undefined,
      unique_id: "guesty:" + d.reservation_id + ":updated:" + (str(r.lastUpdatedAt) || input.now),
    });
  }
  return out;
}

function fromWebhook(input) {
  var p = input.payload || {};
  var ev = String(p.event || "");
  if (ev === "reservation.new" || ev === "reservation.updated" || ev === "reservation_update_shortlist") {
    if (!p.reservation) return [{ type: "log", message: ev + " with no reservation object" }];
    return reservationActions(p.reservation, p.reservationBefore, input, ev !== "reservation.new");
  }
  if (ev === "reservation.created.v2" || ev === "reservation.updated.v2") {
    // v2 events leave out the guest and the money; the scheduled pull fills this reservation in.
    var id = p.data && p.data.reservationId;
    return [{ type: "log", message: ev + " for " + id + ": no guest details in v2 events; the next pull picks it up" }];
  }
  return [{ type: "log", message: "ignored Guesty event " + (ev || "(none)") }];
}

function fromPull(input) {
  var page = input.payload || {};
  var results = page.results || page.data || [];
  if (!Array.isArray(results)) return [{ type: "log", message: "unexpected answer from Guesty: no results list" }];
  var out = [];
  var latest = (input.state && input.state.pending_cursor) || null;
  results.forEach(function (r) {
    reservationActions(r, null, input, false).forEach(function (a) { out.push(a); });
    var u = str(r.lastUpdatedAt);
    if (u && (!latest || u > latest)) latest = u;
  });
  var skip = Number((input.request && input.request.query && findQuery(input.request.query, "skip")) || 0);
  if (results.length >= PAGE_SIZE) {
    // More pages: keep the newest time seen, and commit it as the cursor on the last page, so the
    // filter stays the same while paging.
    out.push({ type: "set_state", values: { pending_cursor: latest } });
    out.push({ type: "next_request", query: { skip: skip + PAGE_SIZE } });
  } else if (latest) {
    out.push({ type: "set_state", values: { cursor: latest, pending_cursor: null } });
  }
  out.push({ type: "log", message: "page " + input.page + ": " + results.length + " reservations" });
  return out;
}

function findQuery(pairs, key) {
  for (var i = 0; i < pairs.length; i++) if (pairs[i][0] === key) return pairs[i][1];
  return null;
}

function handle(input) {
  return input.source === "pull" ? fromPull(input) : fromWebhook(input);
}
