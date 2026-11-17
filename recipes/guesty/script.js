// Guesty → DumbMail
//
// Sources (see README.md for the Guesty doc page behind each assumption):
// - v2 reservation webhooks (reservation.created.v2, reservation.updated.v2): what Guesty recommends.
//   They carry the reservation's dates and status but not the guest's email, so they update
//   guests DumbMail already knows (matched by their Guesty guest id) at once, and the pull fills
//   in new guests.
// - Pulls: GET /v1/reservations-v3/search (changed reservations, oldest change first), then
//   GET /v1/guests-crud for those reservations' guests (email, phone, names). Two requests per page.
// - Legacy webhooks (reservation.new, reservation.updated) still work until Guesty retires them
//   on 1 March 2027. They carry the guest and the money, so they are the only source of totals.
//
// Events, each with reservation_id (the key flows use), dates, nights, guests and the channel:
// - Reservation Booked: once per reservation, the first time it is seen confirmed. Carries the
//   total as its value when Guesty sent it (legacy webhooks), so revenue is counted once.
// - Reservation Confirmed: one per version of a confirmed booking, timed by Guesty's own update
//   time. Date-based flows follow the newest version; a late, older version never replaces a newer
//   one, and A → B → A dates make three versions with the last one current.
// - Reservation Cancelled: as soon as a cancellation is seen on any path.
// - Inquiry Received.
// Nobody is subscribed: Guesty has no marketing opt-in. People this integration adds are held back
// from marketing unless the owner says they opted in; booking-site relay addresses are suppressed.

var SEARCH_PATH = "/v1/reservations-v3/search";
var GUESTS_PATH = "/v1/guests-crud";
// 50 a page keeps the reservations waiting for their guests well inside the 64 KB saved state.
var PAGE_SIZE = 50;
var GUEST_COLUMNS = "id firstName lastName fullName guestEmail guestPhone";

function str(v) {
  return v === undefined || v === null || v === "" ? null : String(v);
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

function nightsBetween(a, b) {
  if (!a || !b) return null;
  var n = Math.round((Date.parse(b + "T00:00:00Z") - Date.parse(a + "T00:00:00Z")) / 86400000);
  return isNaN(n) || n < 0 ? null : n;
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

function splitName(full) {
  var parts = String(full || "").trim().split(/\s+/).filter(function (p) { return p; });
  return { first: parts.shift() || null, last: parts.length ? parts.join(" ") : null };
}

// A guest from a legacy reservation (`guest`) or from GET /guests-crud (`guestEmail`, `guestPhone`).
function guestFrom(g, fallbackId) {
  g = g || {};
  var email = cleanEmail(g.guestEmail) || cleanEmail(g.email) || cleanEmail(g.emails && g.emails[0]);
  var phone = cleanPhone(g.guestPhone) || cleanPhone(g.phone) || cleanPhone(g.phones && g.phones[0]);
  var first = str(g.firstName);
  var last = str(g.lastName);
  if (!first && !last && g.fullName) {
    var n = splitName(g.fullName);
    first = n.first;
    last = n.last;
  }
  return { id: str(g.id || g._id || fallbackId), email: email, phone: phone, first: first, last: last };
}

function isOwnerStay(r, settings) {
  if (settings.skip_owner_stays === false) return false;
  var src = String(r.source || "").toLowerCase();
  var info = r.creationInfo || {};
  return src === "owner" || src === "owner-guest" || r.reservationType === "owner" || info.reservationType === "owner" || info.isOwnerReservation === true;
}

// The same reservation from any of the three shapes: legacy (`_id`, `listing`, `money`), v2 webhook
// data (`reservationId`, `unitId`) or v3 search (`reservationId`, `stay[]`, `channel`).
function details(r) {
  var stay = (r.stay && r.stay[0]) || {};
  var listing = r.listing || {};
  var integration = r.integration || {};
  var m = r.money || null;
  var checkIn = day(r.checkInDateLocalized || stay.checkInDateLocalized || r.checkIn || stay.checkIn);
  var checkOut = day(r.checkOutDateLocalized || stay.checkOutDateLocalized || r.checkOut || stay.checkOut);
  var total = null;
  if (m) {
    var paid = num(m.totalPaid);
    var due = num(m.balanceDue);
    total = paid !== null || due !== null ? (paid || 0) + (due || 0) : num(m.hostPayout);
  }
  return {
    reservation_id: str(r.reservationId || r._id || r.id),
    confirmation_code: str(r.confirmationCode),
    status: str(r.status),
    listing_id: str(r.listingId || stay.listingId || r.unitId || listing._id),
    listing_name: str(listing.nickname || listing.title),
    check_in: checkIn,
    check_out: checkOut,
    nights: num(r.nightsCount) !== null ? num(r.nightsCount) : nightsBetween(checkIn, checkOut),
    guests: num(r.guestsCount !== undefined ? r.guestsCount : stay.guestsCount),
    total: total,
    currency: m ? str(m.currency) : null,
    source_channel: str(integration.platform || r.channel || r.platform || r.source),
  };
}

var BOOKED = ["confirmed", "checked_in", "checked_out", "completed", "alteration"];

// Actions for one reservation as Guesty sees it now.
// `who`: how to find the guest's profile; `guest`: details to upsert (null when only the Guesty guest id is known).
// `version`: Guesty's own time for this state of the reservation (meta.publishedAt or lastUpdatedAt).
function reservationActions(r, guest, who, version, input) {
  var settings = input.settings || {};
  var d = details(r);
  if (!d.reservation_id) return [{ type: "log", message: "skipped a reservation with no id" }];
  if (isOwnerStay(r, settings)) return [{ type: "log", message: "skipped owner stay " + d.reservation_id }];
  var out = [];
  if (guest) {
    var profile = { type: "upsert_profile", properties: { guesty_guest_id: guest.id, last_listing: d.listing_name, last_check_in: d.check_in } };
    if (guest.email) profile.email = guest.email;
    if (guest.phone) profile.phone_number = guest.phone;
    if (guest.first) profile.first_name = guest.first;
    if (guest.last) profile.last_name = guest.last;
    out.push(profile);
  }

  // Old bookings seen for the first time (a backfill, or a first pull) are marked historical so
  // "right after booking" flows skip them. Date-based flows still use them for future stays.
  var confirmedAt = str(r.confirmedAt || (r.channelMetadata && r.channelMetadata.confirmedAt) || r.createdAt);
  var ageDays = confirmedAt ? (Date.parse(input.now) - Date.parse(confirmedAt)) / 86400000 : 0;
  var historical = !!input.backfill || ageDays > 3;
  var props = {};
  for (var k in d) if (d[k] !== null && k !== "total" && k !== "currency") props[k] = d[k];
  if (historical) props.historical = true;
  if (version) props.version = version;

  // Without Guesty's own update time, a version is told apart by its content (and dated now).
  var v = version || ("content:" + [d.status, d.check_in, d.check_out, d.listing_id, d.guests].join("|"));
  var when = version || input.now;
  var status = String(r.status || "").toLowerCase();
  if (BOOKED.indexOf(status) >= 0) {
    var booked = {};
    for (var b in props) if (b !== "version") booked[b] = props[b];
    if (d.total !== null) {
      booked.total = d.total;
      if (d.currency) booked.currency = d.currency;
    }
    out.push({
      type: "track_event", metric: "Reservation Booked", profile: who, properties: booked,
      value: d.total === null ? undefined : d.total, time: confirmedAt || when,
      // Once per reservation: a date change or a resend never counts its value again.
      unique_id: "guesty:" + d.reservation_id + ":booked",
    });
    out.push({
      type: "track_event", metric: "Reservation Confirmed", profile: who, properties: props, time: when,
      unique_id: "guesty:" + d.reservation_id + ":confirmed:" + v,
    });
  } else if (status === "canceled" || status === "cancelled") {
    var cancelledAt = str(r.canceledAt || (r.channelMetadata && r.channelMetadata.canceledAt));
    if (cancelledAt) props.canceled_at = cancelledAt;
    out.push({
      type: "track_event", metric: "Reservation Cancelled", profile: who, properties: props,
      // Dated when it is seen, so it is never older than a confirmation already recorded (webhook
      // and pull times come from different clocks): a cancellation always stops the reminders.
      time: version && version > input.now ? version : input.now,
      unique_id: "guesty:" + d.reservation_id + ":cancelled:" + v,
    });
  } else if (status === "inquiry") {
    out.push({
      type: "track_event", metric: "Inquiry Received", profile: who, properties: props,
      time: str(r.createdAt) || when, unique_id: "guesty:" + d.reservation_id + ":inquiry",
    });
  } else {
    out.push({ type: "log", message: "reservation " + d.reservation_id + " is " + (status || "without a status") + "; nothing to record yet" });
  }
  return out;
}

function changedFields(r, before) {
  if (!before) return [];
  var a = details(r);
  var b = details(before);
  return ["status", "check_in", "check_out", "guests", "total", "listing_id"].filter(function (f) { return b[f] !== null && b[f] !== a[f]; });
}

// "Reservation Updated", once per change (webhooks only: they say what changed).
function updated(r, who, changed, subType, version, input) {
  var d = details(r);
  var props = { reservation_id: d.reservation_id, status: d.status, check_in: d.check_in, check_out: d.check_out, listing_id: d.listing_id, changed: changed || [] };
  if (subType) props.change = String(subType);
  return {
    type: "track_event", metric: "Reservation Updated", profile: who, properties: props, time: version || input.now,
    unique_id: "guesty:" + d.reservation_id + ":updated:" + (version || input.now),
  };
}

function whoFor(guest, guestId) {
  if (guest && guest.email) return { email: guest.email };
  if (guest && guest.phone) return { phone_number: guest.phone };
  return guestId ? { property: "guesty_guest_id", value: String(guestId) } : null;
}

function fromWebhook(input) {
  var p = input.payload || {};
  var meta = p.meta || {};
  var data = p.data || {};
  var ev = String(p.event || data.event || "");
  if (ev === "reservation.created.v2" || ev === "reservation.updated.v2") {
    var gid = str(data.guestId);
    var who = whoFor(null, gid);
    if (!data.reservationId) return [{ type: "log", message: ev + " with no reservationId" }];
    if (!who) return [{ type: "log", message: ev + " for " + data.reservationId + " has no guestId; the next pull picks it up" }];
    // v2 events carry no email: this reaches guests DumbMail already has; the pull adds new ones.
    var acts = reservationActions(data, null, who, str(meta.publishedAt), input);
    if (ev === "reservation.updated.v2") acts.push(updated(data, who, meta.changedFields, meta.subType, str(meta.publishedAt), input));
    acts.push({ type: "log", message: ev + " " + (meta.subType || "") + " for " + data.reservationId + " (guest " + gid + "); skipped if the guest isn't known yet, and the next pull fills it in" });
    return acts;
  }
  if (ev === "reservation.new" || ev === "reservation.updated" || ev === "reservation_update_shortlist") {
    var r = p.reservation;
    if (!r) return [{ type: "log", message: ev + " with no reservation object" }];
    var g = guestFrom(r.guest, r.guestId);
    if (!g.email && !g.phone) return [{ type: "log", message: "reservation " + (r._id || r.id) + ": the guest has no usable email or phone, so there's no profile to write" }];
    var version = str(r.lastUpdatedAt) || str(meta.publishedAt);
    var legacy = reservationActions(r, g, whoFor(g), version, input);
    if (ev !== "reservation.new" && !isOwnerStay(r, input.settings || {})) legacy.push(updated(r, whoFor(g), changedFields(r, p.reservationBefore), null, version, input));
    return legacy;
  }
  return [{ type: "log", message: "ignored Guesty event " + (ev || "(none)") }];
}

function requestPath(input) {
  var u = (input.request && input.request.url) || "";
  var m = /^https?:\/\/[^\/]+(\/[^?#]*)/.exec(u);
  return m ? m[1] : "";
}

function compact(r) {
  // What the guests step needs, kept in saved state between the two requests.
  var stay = (r.stay && r.stay[0]) || {};
  return {
    reservationId: r.reservationId || r._id, guestId: r.guestId, status: r.status, confirmationCode: r.confirmationCode,
    checkInDateLocalized: r.checkInDateLocalized || stay.checkInDateLocalized, checkOutDateLocalized: r.checkOutDateLocalized || stay.checkOutDateLocalized,
    checkIn: r.checkIn, checkOut: r.checkOut, listingId: stay.listingId || r.listingId, guestsCount: stay.guestsCount !== undefined ? stay.guestsCount : r.guestsCount,
    source: r.source, channel: r.channel, createdAt: r.createdAt, lastUpdatedAt: r.lastUpdatedAt,
    channelMetadata: r.channelMetadata ? { confirmedAt: r.channelMetadata.confirmedAt, canceledAt: r.channelMetadata.canceledAt, updatedAt: r.channelMetadata.updatedAt } : undefined,
  };
}

function updatedAt(r) {
  return str(r.lastUpdatedAt || (r.channelMetadata && r.channelMetadata.updatedAt));
}

// Step 1: a page of changed reservations. Keep them and ask for their guests.
function fromSearch(input) {
  var page = input.payload || {};
  var results = page.results;
  if (!Array.isArray(results)) return [{ type: "log", message: "unexpected answer from Guesty: no results list" }];
  if (!results.length) return [{ type: "log", message: "page " + input.page + ": no changed reservations" }];
  var st = input.state || {};
  var last = null;
  var first = null;
  var ids = [];
  results.forEach(function (r) {
    var u = updatedAt(r);
    if (u && (!last || u > last)) last = u;
    if (u && (!first || u < first)) first = u;
    if (r.guestId && ids.indexOf(String(r.guestId)) < 0) ids.push(String(r.guestId));
  });
  var more = !!(page.pagination && page.pagination.hasMore) || results.length >= PAGE_SIZE;
  // A full page that all changed at the same moment can't move the cursor: step past it instead.
  var skip = more && first === last ? Number(st.skip || 0) + results.length : 0;
  var out = [{
    type: "set_state",
    values: { pending: results.map(compact), page_last: last, page_more: more, skip: skip },
  }];
  out.push({
    type: "next_request", url: GUESTS_PATH,
    query: { columns: GUEST_COLUMNS, filters: JSON.stringify({ id: { "@in": ids } }), limit: PAGE_SIZE, skip: null, sort: null, "filter[lastUpdatedAt][gte]": null },
  });
  out.push({ type: "log", message: "page " + input.page + ": " + results.length + " changed reservations; fetching their " + ids.length + " guests" });
  return out;
}

// Step 2: the guests for the reservations kept in step 1. Write them, move the cursor on, and go
// back for the next page of reservations if there is one.
function fromGuests(input) {
  var st = input.state || {};
  var pending = st.pending || [];
  var page = input.payload || {};
  var list = page.results || [];
  var byId = {};
  (Array.isArray(list) ? list : []).forEach(function (g) {
    var id = str(g.id || g._id);
    if (id) byId[id] = g;
  });
  var out = [];
  pending.forEach(function (r) {
    var gid = str(r.guestId);
    var g = gid && byId[gid] ? guestFrom(byId[gid], gid) : null;
    var who = whoFor(g && (g.email || g.phone) ? g : null, gid);
    if (!who) {
      out.push({ type: "log", message: "reservation " + r.reservationId + " has no guest; skipped" });
      return;
    }
    reservationActions(r, g && (g.email || g.phone) ? g : null, who, updatedAt(r), input).forEach(function (a) { out.push(a); });
  });
  out.push({ type: "set_state", values: { cursor: st.page_last || st.cursor || null, pending: null, page_last: null } });
  if (st.page_more) {
    var q = { limit: PAGE_SIZE, sort: "lastUpdatedAt", "filter[lastUpdatedAt][gte]": st.page_last, skip: st.skip || 0, columns: null, filters: null };
    out.push({ type: "next_request", url: SEARCH_PATH, query: q });
  } else {
    out.push({ type: "set_state", values: { skip: 0, page_more: null } });
  }
  out.push({ type: "log", message: "page " + input.page + ": wrote " + pending.length + " reservations" });
  return out;
}

function handle(input) {
  if (input.source !== "pull") return fromWebhook(input);
  return requestPath(input).indexOf(GUESTS_PATH) === 0 ? fromGuests(input) : fromSearch(input);
}
