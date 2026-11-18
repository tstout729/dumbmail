// Calendly → DumbMail
//
// invitee.created → Meeting Scheduled, invitee.canceled → Meeting Cancelled, on the invitee's
// profile. Events carry invitee_uri (the key reminder flows use), the meeting's name, start and end
// time, location, the cancel and reschedule links and the invitee's answers. A reschedule arrives as
// a cancel of the old booking plus a new booking, so reminders follow the new time.
// Calendly's text-reminder number is saved on the profile but is not an SMS opt-in.

function answers(qs) {
  var out = {};
  (qs || []).forEach(function (q) { if (q && q.question) out[String(q.question).slice(0, 100)] = q.answer; });
  return out;
}

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

function phone(v) {
  if (!v) return null;
  var d = String(v).replace(/[^0-9]/g, "");
  return String(v).trim().charAt(0) === "+" && d.length >= 8 && d.length <= 15 ? "+" + d : null;
}

function handle(input) {
  var body = input.payload || {};
  var p = body.payload || {};
  var ev = body.event;
  if (ev !== "invitee.created" && ev !== "invitee.canceled") {
    return [{ type: "log", message: "ignored Calendly event " + (ev || "(none)") }];
  }
  if (!p.email) return [{ type: "log", message: ev + " with no invitee email" }];
  var se = p.scheduled_event || {};
  var loc = se.location || {};
  var props = clean({
    invitee_uri: p.uri,
    event_uri: p.event || se.uri,
    event_name: se.name,
    start_time: se.start_time,
    end_time: se.end_time,
    location: loc.location || loc.join_url || loc.type,
    join_url: loc.join_url,
    cancel_url: p.cancel_url,
    reschedule_url: p.reschedule_url,
    timezone: p.timezone,
    rescheduled_from: p.old_invitee,
    answers: (p.questions_and_answers || []).length ? answers(p.questions_and_answers) : null,
    utm_source: p.tracking && p.tracking.utm_source,
    utm_campaign: p.tracking && p.tracking.utm_campaign,
  });
  var prof = { type: "upsert_profile", email: p.email };
  if (p.first_name) prof.first_name = p.first_name;
  if (p.last_name) prof.last_name = p.last_name;
  if (!p.first_name && !p.last_name && p.name) {
    var parts = String(p.name).trim().split(/\s+/);
    prof.first_name = parts.shift();
    if (parts.length) prof.last_name = parts.join(" ");
  }
  var extra = clean({ timezone: p.timezone, calendly_text_reminder_number: phone(p.text_reminder_number) });
  if (Object.keys(extra).length) prof.properties = extra;

  if (ev === "invitee.created") {
    return [prof, {
      type: "track_event", metric: "Meeting Scheduled", profile: { email: p.email }, properties: props,
      time: p.created_at, unique_id: "calendly:" + p.uri + ":created",
    }];
  }
  var c = p.cancellation || {};
  props.reason = c.reason || undefined;
  props.canceled_by = c.canceled_by || undefined;
  props.canceler_type = c.canceler_type || undefined;
  props.rescheduled = !!p.rescheduled;
  return [prof, {
    type: "track_event", metric: "Meeting Cancelled", profile: { email: p.email }, properties: clean(props),
    time: c.created_at || p.updated_at, unique_id: "calendly:" + p.uri + ":canceled",
  }];
}
