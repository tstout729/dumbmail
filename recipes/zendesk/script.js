// Zendesk → DumbMail
//
// Zendesk triggers notify a webhook with a JSON body you paste in (see the setup steps), built from
// Zendesk's placeholders: the ticket id, subject, status, the requester's email and name, the update
// time and which trigger fired (event). Anyone can open a ticket, so only the email is used. Ticket Created, Ticket Solved and Satisfaction Rated go on
// the requester's profile. Only the subject and status are kept, never the ticket's comments, and a
// satisfaction comment isn't recorded. Support tickets aren't consent: nobody is subscribed.
// See README.md for the Zendesk page behind each assumption.

// Zendesk's *_with_timestamp placeholders look like 2013-12-12T05:35Z (no seconds).
function iso(t) {
  if (!t) return undefined;
  var s = String(t).trim();
  return /T\d\d:\d\dZ$/.test(s) ? s.replace(/Z$/, ":00Z") : s;
}

function handle(input) {
  var p = input.payload || {};
  if (typeof p !== "object" || !p.ticket_id) return [{ type: "log", message: "not the DumbMail trigger body (see the recipe's setup steps)" }];
  if (!p.requester_email) return [{ type: "log", message: "ticket " + p.ticket_id + " has no requester email" }];
  var when = iso(p.updated_at || p.created_at);
  if (!when) return [{ type: "log", message: "ticket " + p.ticket_id + " has no update time, so it can't be put in order" }];
  var who = { email: p.requester_email };
  var up = { type: "upsert_profile", email: p.requester_email, keys: { zendesk_ticket: String(p.ticket_id) } };
  // Anyone can email support under any name, so the requester's name never changes a profile.
  var props = { ticket_id: String(p.ticket_id), subject: p.subject, status: p.status, via: p.via };
  for (var k in props) if (props[k] === undefined || props[k] === null || props[k] === "") delete props[k];
  var ev = String(p.event || "");
  if (ev === "created") return [up, { type: "track_event", metric: "Ticket Created", profile: who, time: iso(p.created_at) || when, unique_id: "zendesk:" + p.ticket_id + ":created", properties: props }];
  if (ev === "solved") return [up, { type: "track_event", metric: "Ticket Solved", profile: who, time: when, unique_id: "zendesk:" + p.ticket_id + ":solved:" + when, properties: props }];
  if (ev === "satisfaction") {
    if (!p.rating) return [{ type: "log", message: "ticket " + p.ticket_id + " has no rating" }];
    props.rating = p.rating;
    return [up, { type: "track_event", metric: "Satisfaction Rated", profile: who, time: when, unique_id: "zendesk:" + p.ticket_id + ":rating:" + when, properties: props }];
  }
  return [{ type: "log", message: "unknown event " + (ev || "(none)") + " in the trigger body" }];
}
