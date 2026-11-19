// Gorgias → DumbMail
//
// Gorgias's HTTP integrations aren't signed: each request carries the secret header you set up
// (X-Webhook-Token), and its body is the template from the setup steps, `{"ticket_id":
// "{{ticket.id}}"}`. The ticket is then fetched from Gorgias's API (GET /api/tickets/{id}), and only
// that answer is recorded, so a resent or late request can't record a state the ticket has left.
// See README.md for the Gorgias page behind each assumption.
//
// Ticket Created: once per ticket. Ticket Closed: once each time it's closed (Gorgias's
// closed_datetime). Satisfaction Rated: once per survey, with its score (1 to 5), once the
// customer has answered. Only the ticket's subject, status and channel are kept: its messages and
// the survey's comment are dropped before anything is stored, and customers' names aren't taken.
// Spam and trashed tickets are skipped. A support ticket isn't consent: nobody is subscribed.

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

function fetched(input, id) {
  var t = input.payload || {};
  if (String(t.id) !== id) return [{ type: "log", message: "the answer isn't ticket " + id }];
  if (t.spam === true || t.trashed_datetime) return [{ type: "log", message: "ticket " + id + " is spam or in the trash" }];
  var email = t.customer && t.customer.email;
  if (!email) return [{ type: "log", message: "ticket " + id + " has no customer email" }];
  var who = { email: String(email) };
  var props = clean({ ticket_id: id, subject: t.subject, status: t.status, channel: t.channel });
  var out = [{ type: "upsert_profile", email: String(email), keys: { gorgias_ticket: id } }];
  if (t.created_datetime) out.push({ type: "track_event", metric: "Ticket Created", profile: who, time: t.created_datetime, unique_id: "gorgias:" + id + ":created", properties: props });
  if (t.status === "closed" && t.closed_datetime) out.push({ type: "track_event", metric: "Ticket Closed", profile: who, time: t.closed_datetime, unique_id: "gorgias:" + id + ":closed:" + t.closed_datetime, properties: props });
  var s = t.satisfaction_survey;
  if (s && typeof s.score === "number" && s.score >= 1 && s.score <= 5 && s.scored_datetime && /^[0-9]{1,20}$/.test(String(s.id))) {
    out.push({ type: "track_event", metric: "Satisfaction Rated", profile: who, time: s.scored_datetime, unique_id: "gorgias:survey:" + s.id, properties: clean(Object.assign({ score: s.score }, props)) });
  }
  return out;
}

function handle(input) {
  if (input.source === "pull") return [{ type: "log", message: "this recipe records Gorgias's HTTP integration requests; it doesn't import history" }];
  if (input.source === "fetch") {
    var path = String((input.request && input.request.url) || "").replace(/^https?:\/\/[^\/]+/, "").split("?")[0];
    var m = /^\/api\/tickets\/([0-9]{1,20})$/.exec(path);
    return m ? fetched(input, m[1]) : [{ type: "log", message: "unexpected answer from " + path }];
  }
  var p = input.payload || {};
  var id = String(p.ticket_id === undefined || p.ticket_id === null ? "" : p.ticket_id).trim();
  if (!/^[0-9]{1,20}$/.test(id)) return [{ type: "log", message: "not the DumbMail body (see the recipe's setup steps): no ticket_id" }];
  return [{ type: "next_request", url: "/api/tickets/" + id }];
}
