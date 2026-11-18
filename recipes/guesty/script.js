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
// - Reservation Booked: once per reservation, the first time it is seen confirmed (no value).
// - Reservation Total: once per reservation, with the total as its value, whenever Guesty first
//   sends it (legacy webhooks), so revenue counts once whichever message arrives first.
// - Reservation Confirmed: a new version only when the booking's dates, listing, status or guest
//   change (a webhook that says only the notes changed makes none), timed by Guesty's own update
//   time. (The pre-arrival flow also never sends the same booking's reminder twice for one date.) Date-based flows follow the newest
//   version; a late, older version never replaces a newer one, and A → B → A dates make three
//   versions with the last one current.
// - Reservation Cancelled: timed by Guesty's own update time, so a late, older cancellation never
//   undoes a newer reinstatement.
// - Inquiry Received.
// Each reservation is also kept as a `reservation` object (see dumbmail://docs/objects) with its
// current status, dates, listing, nights, guests and channel, written with Guesty's own time as
// `as_of` so a late, older message never undoes a newer one. Its `guest` link is replaced by each
// message: when Guesty moves the booking to another guest (or it has none), the old guest is
// unlinked at once, and the new one is linked as soon as DumbMail knows them. Object-date flows
// (the pre-arrival flow) follow it when its dates move or it's cancelled.
// A webhook without any Guesty time can't be ordered, so it writes no events; the next pull (every
// 15 minutes) records the change with Guesty's time. A v2 event for a guest DumbMail doesn't know
// yet is retried until the pull has added them.
// Door codes and access notes (`notes`, `keyCode`, `channelMetadata.guestContact`) are never put in
// events or profiles, and the recipe's redact list keeps them out of the delivery log.
// Nobody is subscribed: Guesty has no marketing opt-in. People this integration adds are held back
// from marketing unless the owner says they opted in; booking-site relay addresses are suppressed.

var SEARCH_PATH = "/v1/reservations-v3/search";
var GUESTS_PATH = "/v1/guests-crud";
// 50 a page keeps the reservations waiting for their guests well inside the 64 KB saved state.
var PAGE_SIZE = 50;
// Guesty answers these columns with `_id`, `email` and `phone` (checked against the live API).
var GUEST_COLUMNS = "id firstName lastName fullName guestEmail guestPhone";
// What makes a new booking version: a change to any of these (Guesty's field names in
// meta.changedFields, and ours when comparing a legacy webhook's before and after).
var VERSION_CHANGES = ["status", "checkIn", "checkOut", "checkInDateLocalized", "checkOutDateLocalized", "listingId", "unitId", "listing", "guestId", "guest"];
var VERSION_FIELDS = ["status", "check_in", "check_out", "listing_id", "guest_id", "paid_in_full"];

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

// `withCountry`: the number is digits with the country code but no "+" (GET /guests-crud).
function cleanPhone(v, withCountry) {
  var s = str(v && typeof v === "object" ? v.number || v.phone : v);
  if (!s) return null;
  var digits = s.replace(/[^0-9]/g, "");
  if ((withCountry || s.trim().charAt(0) === "+") && digits.length >= 8 && digits.length <= 15) return "+" + digits;
  if (digits.length === 10) return "+1" + digits;
  if (digits.length === 11 && digits.charAt(0) === "1") return "+" + digits;
  return null;
}

function splitName(full) {
  var parts = String(full || "").trim().split(/\s+/).filter(function (p) { return p; });
  return { first: parts.shift() || null, last: parts.length ? parts.join(" ") : null };
}

// A guest from a legacy reservation (`guest`) or from GET /guests-crud (`_id`, `email`, `phone`
// with the country code and no "+"; any of them can be missing).
function guestFrom(g, fallbackId, crud) {
  g = g || {};
  var email = cleanEmail(g.email) || cleanEmail(g.guestEmail) || cleanEmail(g.emails && g.emails[0]);
  var phone = cleanPhone(g.phone, crud) || cleanPhone(g.guestPhone, crud) || cleanPhone(g.phones && g.phones[0]);
  var first = str(g.firstName);
  var last = str(g.lastName);
  if (!first && !last && g.fullName) {
    var n = splitName(g.fullName);
    first = n.first;
    last = n.last;
  }
  return { id: str(g._id || g.id || fallbackId), email: email, phone: phone, first: first, last: last };
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
  // Paid in full, as Guesty reports it: nothing left to pay on a booking worth something. Unknown
  // (null) when the message carries no money (v2 webhooks and the current reservation API don't).
  var paidInFull = null;
  if (m) {
    var paid = num(m.totalPaid);
    var due = num(m.balanceDue);
    total = paid !== null || due !== null ? (paid || 0) + (due || 0) : num(m.hostPayout);
    if (due !== null && total !== null) paidInFull = due === 0 && total > 0;
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
    paid_in_full: paidInFull,
    source_channel: str(integration.platform || r.channel || r.platform || r.source),
    guest_id: str(r.guestId || (r.guest && (r.guest._id || r.guest.id))),
  };
}

var BOOKED = ["confirmed", "checked_in", "checked_out", "completed", "alteration"];

// Actions for one reservation as Guesty sees it now.
// `who`: how to find the guest's profile; `guest`: details to upsert (null when only the Guesty guest id is known).
// `version`: Guesty's own time for this state of the reservation (meta.publishedAt or lastUpdatedAt).
// `versioned`: false when the message says nothing that matters for a version changed (only
// notes, say): then no new Reservation Confirmed version is recorded.
function reservationActions(r, guest, who, version, input, versioned) {
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

  // Without Guesty's own time a message can't be put in order: nothing time-dependent is written
  // (the next pull records the change with Guesty's time).
  if (!version) {
    out.push({ type: "log", message: "reservation " + d.reservation_id + ": the message has no Guesty time, so it can't be ordered; the next pull records it" });
    return out;
  }

  // Old bookings seen for the first time (a backfill, or a first pull) are marked historical so
  // "right after booking" flows skip them. Date-based flows still use them for future stays.
  var confirmedAt = str(r.confirmedAt || (r.channelMetadata && r.channelMetadata.confirmedAt) || r.createdAt);
  var ageDays = confirmedAt ? (Date.parse(input.now) - Date.parse(confirmedAt)) / 86400000 : 0;
  var historical = !!input.backfill || ageDays > 3;
  var props = {};
  for (var k in d) if (d[k] !== null && k !== "total" && k !== "currency") props[k] = d[k];
  if (historical) props.historical = true;
  props.version = version;

  // Payment is part of the versioned write: an older message never overrides a newer state. If a
  // newer message without money got there first, payment stays as it was (unknown means no
  // pre-arrival message) until Guesty's next message that carries money.
  out.push(reservationObject(d, who, version));

  var status = String(r.status || "").toLowerCase();
  if (BOOKED.indexOf(status) >= 0) {
    var booked = {};
    for (var b in props) if (b !== "version") booked[b] = props[b];
    out.push({
      type: "track_event", metric: "Reservation Booked", profile: who, properties: booked, time: confirmedAt || version,
      // Once per reservation, whichever message arrives first.
      unique_id: "guesty:" + d.reservation_id + ":booked",
    });
    if (d.total !== null) {
      // The value, once per reservation, from the first message that carries it.
      out.push({
        type: "track_event", metric: "Reservation Total", profile: who, value: d.total, time: confirmedAt || version,
        properties: clean({ reservation_id: d.reservation_id, total: d.total, currency: d.currency, listing_id: d.listing_id, check_in: d.check_in, check_out: d.check_out, historical: historical || null }),
        unique_id: "guesty:" + d.reservation_id + ":total",
      });
    }
    if (versioned === false) {
      out.push({ type: "log", message: "reservation " + d.reservation_id + ": only details that don't change the booking's dates, listing, status or guest changed; no new version" });
    } else {
      out.push({
        type: "track_event", metric: "Reservation Confirmed", profile: who, properties: props, time: version,
        unique_id: "guesty:" + d.reservation_id + ":confirmed:" + version,
      });
    }
  } else if (status === "canceled" || status === "cancelled") {
    var cancelledAt = str(r.canceledAt || (r.channelMetadata && r.channelMetadata.canceledAt));
    if (cancelledAt) props.canceled_at = cancelledAt;
    out.push({
      type: "track_event", metric: "Reservation Cancelled", profile: who, properties: props,
      // Guesty's own time: a cancellation that arrives late, after the booking was reinstated,
      // is older than the reinstatement and doesn't stop its reminders.
      time: version,
      unique_id: "guesty:" + d.reservation_id + ":cancelled:" + version,
    });
  } else if (status === "inquiry") {
    out.push({
      type: "track_event", metric: "Inquiry Received", profile: who, properties: props,
      time: str(r.createdAt) || version, unique_id: "guesty:" + d.reservation_id + ":inquiry",
    });
  } else {
    out.push({ type: "log", message: "reservation " + d.reservation_id + " is " + (status || "without a status") + "; nothing to record yet" });
  }
  return out;
}

// The reservation as it is now. Only what this message says is written (a v2 webhook without the
// listing's name leaves the stored name alone); door codes and notes are never among the fields.
// `who` may be null (no guest, or one DumbMail can't find yet): the old guest is unlinked anyway.
// Payment: a version that carries money records `paid_in_full` with the time of that evidence
// (`paid_checked_at`, Guesty's own time for the version); a version without money makes payment
// unknown (both cleared), so evidence is never older than the reservation's last change.
function reservationObject(d, who, version) {
  var fields = clean({
    status: d.status ? d.status.toLowerCase() : null, confirmation_code: d.confirmation_code, listing_id: d.listing_id, listing_name: d.listing_name,
    check_in: d.check_in, check_out: d.check_out, nights: d.nights, guests: d.guests, total: d.total, currency: d.currency,
    source_channel: d.source_channel, guest_id: d.guest_id,
  });
  if (d.paid_in_full !== null) {
    fields.paid_in_full = d.paid_in_full;
    fields.paid_checked_at = version;
  } else {
    fields.paid_in_full = null;
    fields.paid_checked_at = null;
  }
  var profiles = [];
  if (who) {
    var link = {};
    for (var k in who) if (k !== "if_missing") link[k] = who[k];
    link.relationship = "guest";
    profiles.push(link);
  }
  return { type: "upsert_object", object_type: "reservation", external_id: d.reservation_id, fields: fields, profiles: profiles, replace_links: ["guest"], as_of: version };
}

// A reservation with no guest DumbMail can find: only its object (which unlinks any old guest).
function objectOnly(r, version, input) {
  var d = details(r);
  if (!d.reservation_id || isOwnerStay(r, input.settings || {})) return [];
  if (!version) return [];
  return [reservationObject(d, null, version)];
}

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined) out[k] = o[k];
  return out;
}

function changedFields(r, before) {
  if (!before) return [];
  var a = details(r);
  var b = details(before);
  return ["status", "check_in", "check_out", "guests", "total", "listing_id", "guest_id", "paid_in_full"].filter(function (f) { return b[f] !== null && b[f] !== a[f]; });
}

// "Reservation Updated", once per change (webhooks only: they say what changed).
function updated(r, who, changed, subType, version, input) {
  if (!version) return { type: "log", message: "a change without a Guesty time isn't recorded as Reservation Updated" };
  var d = details(r);
  var props = { reservation_id: d.reservation_id, status: d.status, check_in: d.check_in, check_out: d.check_out, listing_id: d.listing_id, changed: changed || [] };
  if (subType) props.change = String(subType);
  return {
    type: "track_event", metric: "Reservation Updated", profile: who, properties: props, time: version,
    unique_id: "guesty:" + d.reservation_id + ":updated:" + version,
  };
}

// `retry`: a guest DumbMail doesn't know yet makes the delivery wait (retried until the pull adds them).
function whoFor(guest, guestId, retry) {
  if (guest && guest.email) return { email: guest.email };
  if (guest && guest.phone) return { phone_number: guest.phone };
  if (!guestId) return null;
  var w = { property: "guesty_guest_id", value: String(guestId) };
  if (retry) w.if_missing = "retry";
  return w;
}

function fromWebhook(input) {
  var p = input.payload || {};
  var meta = p.meta || {};
  var data = p.data || {};
  var ev = String(p.event || data.event || "");
  if (ev === "reservation.created.v2" || ev === "reservation.updated.v2") {
    var gid = str(data.guestId);
    var who = whoFor(null, gid, true);
    if (!data.reservationId) return [{ type: "log", message: ev + " with no reservationId" }];
    if (!who) return objectOnly(data, str(meta.publishedAt), input).concat([{ type: "log", message: ev + " for " + data.reservationId + " has no guestId; the reservation is kept without a guest" }]);
    // v2 events carry no email: this reaches guests DumbMail already has; the pull adds new ones.
    var changed = Array.isArray(meta.changedFields) ? meta.changedFields : null;
    var versioned = ev === "reservation.created.v2" || !changed || changed.some(function (f) { return VERSION_CHANGES.indexOf(String(f)) >= 0; });
    var acts = reservationActions(data, null, who, str(meta.publishedAt), input, versioned);
    if (ev === "reservation.updated.v2") acts.push(updated(data, who, meta.changedFields, meta.subType, str(meta.publishedAt), input));
    acts.push({ type: "log", message: ev + " " + (meta.subType || "") + " for " + data.reservationId + " (guest " + gid + "); retried until the guest is known" });
    return acts;
  }
  if (ev === "reservation.new" || ev === "reservation.updated" || ev === "reservation_update_shortlist") {
    var r = p.reservation;
    if (!r) return [{ type: "log", message: ev + " with no reservation object" }];
    var g = guestFrom(r.guest, r.guestId);
    if (!g.email && !g.phone) return [{ type: "log", message: "reservation " + (r._id || r.id) + ": the guest has no usable email or phone, so there's no profile to write" }];
    var version = str(r.lastUpdatedAt) || str(meta.publishedAt);
    var before = p.reservationBefore;
    var moved = before ? changedFields(r, before).some(function (f) { return VERSION_FIELDS.indexOf(f) >= 0; }) : true;
    var legacy = reservationActions(r, g, whoFor(g), version, input, ev === "reservation.new" || moved);
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

// The value of query parameter `name` in the request this page answered.
function requestQuery(input, name) {
  var q = (input.request && input.request.query) || [];
  for (var i = 0; i < q.length; i++) if (q[i][0] === name) return q[i][1];
  return null;
}

// Step 1: a page of changed reservations. Keep them and ask for their guests.
// Continuation: the next page starts at the newest change seen (`gte` that time) and skips the
// reservations at exactly that time already read, so equal timestamps never make it reread the same
// rows, even across runs (the cursor and the skip are both saved).
function fromSearch(input) {
  var page = input.payload || {};
  var results = page.results;
  if (!Array.isArray(results)) return [{ type: "log", message: "unexpected answer from Guesty: no results list" }];
  var st = input.state || {};
  var usedFrom = requestQuery(input, "filter[lastUpdatedAt][gte]") || st.cursor || null;
  var usedSkip = Number(requestQuery(input, "skip") || 0) || 0;
  if (!results.length) return [{ type: "log", message: "page " + input.page + ": no changed reservations" }];
  var last = null;
  var ids = [];
  results.forEach(function (r) {
    var u = updatedAt(r);
    if (u && (!last || u > last)) last = u;
    if (r.guestId && ids.indexOf(String(r.guestId)) < 0) ids.push(String(r.guestId));
  });
  var atLast = results.filter(function (r) { return updatedAt(r) === last; }).length;
  var skip = (last === usedFrom ? usedSkip : 0) + atLast;
  var more = !!(page.pagination && page.pagination.hasMore) || results.length >= PAGE_SIZE;
  var out = [{
    type: "set_state",
    values: { pending: results.map(compact), page_last: last || usedFrom, page_skip: last ? skip : usedSkip + results.length, page_more: more },
  }];
  out.push({
    type: "next_request", url: GUESTS_PATH,
    query: { columns: GUEST_COLUMNS, filters: JSON.stringify({ _id: { "@in": ids } }), limit: PAGE_SIZE, skip: null, sort: null, "filter[lastUpdatedAt][gte]": null },
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
    var id = str(g._id || g.id);
    if (id) byId[id] = g;
  });
  var out = [];
  pending.forEach(function (r) {
    var gid = str(r.guestId);
    var g = gid && byId[gid] ? guestFrom(byId[gid], gid, true) : null;
    var who = whoFor(g && (g.email || g.phone) ? g : null, gid);
    if (!who) {
      objectOnly(r, updatedAt(r), input).forEach(function (a) { out.push(a); });
      out.push({ type: "log", message: "reservation " + r.reservationId + " has no guest; kept without one" });
      return;
    }
    reservationActions(r, g && (g.email || g.phone) ? g : null, who, updatedAt(r), input).forEach(function (a) { out.push(a); });
  });
  var cursor = st.page_last || st.cursor || null;
  var skip = st.page_last ? st.page_skip || 0 : st.skip || 0;
  out.push({ type: "set_state", values: { cursor: cursor, skip: skip, pending: null, page_last: null, page_skip: null, page_more: null } });
  if (st.page_more) {
    var q = { limit: PAGE_SIZE, sort: "lastUpdatedAt", "filter[lastUpdatedAt][gte]": cursor, skip: skip, columns: null, filters: null };
    out.push({ type: "next_request", url: SEARCH_PATH, query: q });
  }
  out.push({ type: "log", message: "page " + input.page + ": wrote " + pending.length + " reservations" });
  return out;
}

function handle(input) {
  if (input.source !== "pull") return fromWebhook(input);
  return requestPath(input).indexOf(GUESTS_PATH) === 0 ? fromGuests(input) : fromSearch(input);
}
