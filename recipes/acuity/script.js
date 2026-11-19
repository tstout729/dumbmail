// Acuity Scheduling → DumbMail
//
// Acuity's webhooks are thin and form-encoded (`action=scheduled&id=13&calendarID=1&
// appointmentTypeID=13`), signed with the account's API key (X-Acuity-Signature, base64 HMAC-SHA256
// of the body). Each one names an appointment; DumbMail fetches it (GET /api/v1/appointments/{id})
// and records what the answer says, so a late or repeated webhook can't bring back an appointment
// that was cancelled since. See README.md for the Acuity page behind each assumption.
//
// Appointment Scheduled: once per appointment. Appointment Rescheduled: once per new time.
// Appointment Cancelled: once (with no_show when Acuity marked it a no-show). Events are dated
// when DumbMail received Acuity's answer: an appointment carries no time of its last change.
// Only the client's name, email and phone and the appointment's time, type, calendar and length are
// kept: intake forms, notes, prices and links are dropped before anything is stored. Booking an
// appointment isn't consent: nobody is subscribed.

function iso(s) {
  if (!s) return undefined;
  // Acuity writes "2013-06-17T10:15:00-0700"; ISO 8601 wants "-07:00".
  var t = String(s).trim().replace(/([+-]\d\d)(\d\d)$/, "$1:$2");
  var p = Date.parse(t);
  return isFinite(p) ? t : undefined;
}

function e164(p) {
  if (!p) return undefined;
  var s = String(p).trim();
  var d = s.replace(/[^0-9]/g, "");
  return s.charAt(0) === "+" && d.length >= 8 && d.length <= 15 ? "+" + d : undefined;
}

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

function fetched(input) {
  var a = input.payload || {};
  var id = String(a.id === undefined || a.id === null ? "" : a.id);
  if (!/^[0-9]{1,20}$/.test(id)) return [{ type: "log", message: "no appointment in the answer" }];
  if (!a.email) return [{ type: "log", message: "appointment " + id + " has no client email" }];
  var action = String((input.context && input.context.action) || "");
  var when = String((input.response && input.response.received_at) || input.now);
  var who = { email: String(a.email) };
  var p = { type: "upsert_profile", email: String(a.email) };
  if (a.firstName) p.first_name = String(a.firstName);
  if (a.lastName) p.last_name = String(a.lastName);
  var ph = e164(a.phone);
  if (ph) p.phone_number = ph;
  var start = iso(a.datetime);
  var props = clean({
    appointment_id: id, datetime: start, type: a.type, calendar: a.calendar, duration_minutes: parseInt(a.duration, 10) || undefined, timezone: a.timezone,
  });
  // Acuity documents that a cancelled appointment comes back with `noShow` (true for a no-show,
  // false for a plain cancellation); `canceled: true` is read too.
  var cancelled = a.canceled === true || a.noShow === true || a.noShow === false;
  if (cancelled) {
    props.no_show = a.noShow === true;
    return [p, { type: "track_event", metric: "Appointment Cancelled", profile: who, time: when, unique_id: "acuity:" + id + ":cancelled", properties: props }];
  }
  if (!start) return [{ type: "log", message: "appointment " + id + " has no time" }];
  var out = [p, { type: "track_event", metric: "Appointment Scheduled", profile: who, time: when, unique_id: "acuity:" + id + ":scheduled", properties: props }];
  if (action === "rescheduled") out.push({ type: "track_event", metric: "Appointment Rescheduled", profile: who, time: when, unique_id: "acuity:" + id + ":rescheduled:" + start, properties: props });
  return out;
}

function handle(input) {
  if (input.source === "fetch") return fetched(input);
  if (input.source === "pull") return [{ type: "log", message: "this recipe records appointments from Acuity's webhooks; it doesn't import history" }];
  var f = input.payload || {};
  // Webhooks set up in Acuity's Integrations page send scheduled, rescheduled, canceled or changed;
  // ones created through the API name the same events appointment.scheduled and so on.
  var action = String(f.action || "").replace(/^appointment\./, "");
  var id = String(f.id === undefined || f.id === null ? "" : f.id);
  if (["scheduled", "rescheduled", "canceled", "changed"].indexOf(action) < 0) return [{ type: "log", message: "ignored Acuity action " + (f.action || "(none)") }];
  if (!/^[0-9]{1,20}$/.test(id)) return [{ type: "log", message: "ignored " + action + " without an appointment id" }];
  return [{ type: "next_request", url: "/api/v1/appointments/" + id, context: { action: action } }];
}
