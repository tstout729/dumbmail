// Segment → DumbMail (Webhooks destination)
//
// identify calls with traits.email become profiles (matched by email; the Segment userId is kept as a
// key). track calls become events named after the Segment event, only for the event names listed in
// settings.events (and in the integration's metrics): track calls find the person by the email in
// context.traits or properties, else by the userId an earlier identify saved. Each call's messageId
// is its unique id. Nobody is subscribed: Segment calls carry no marketing opt-in.
// Anyone holding a Segment write key can send calls, so connect only sources you trust.
// See README.md for the Segment page behind each assumption.

function e164(p) {
  if (!p) return undefined;
  var s = String(p).trim();
  var d = s.replace(/[^0-9]/g, "");
  return s.charAt(0) === "+" && d.length >= 8 && d.length <= 15 ? "+" + d : undefined;
}

function plain(o) {
  var out = {};
  var n = 0;
  for (var k in o || {}) {
    var v = o[k];
    if (n >= 100 || v === null || v === undefined) continue;
    if (typeof v === "object") v = JSON.stringify(v).slice(0, 2000);
    out[k] = v;
    n++;
  }
  return out;
}

function handle(input) {
  var c = input.payload || {};
  var events = (input.settings && input.settings.events) || [];
  if (c.type === "identify") {
    var t = c.traits || {};
    if (!t.email) return [{ type: "log", message: "identify without traits.email" }];
    var up = { type: "upsert_profile", email: t.email };
    if (t.firstName || t.first_name) up.first_name = t.firstName || t.first_name;
    if (t.lastName || t.last_name) up.last_name = t.lastName || t.last_name;
    if (!up.first_name && t.name) {
      var parts = String(t.name).trim().split(/\s+/);
      up.first_name = parts.shift();
      if (parts.length) up.last_name = parts.join(" ");
    }
    var ph = e164(t.phone);
    if (ph) up.phone_number = ph;
    if (c.userId) up.keys = { segment_user_id: String(c.userId) };
    return [up];
  }
  if (c.type === "track") {
    if (events.indexOf(c.event) < 0) return [{ type: "log", message: "track " + c.event + " isn't in settings.events (add it there and to the integration's metrics to record it)" }];
    var email = (c.context && c.context.traits && c.context.traits.email) || (c.properties && c.properties.email);
    var who = email ? { email: email } : c.userId ? { property: "segment_user_id", value: String(c.userId), if_missing: "retry" } : null;
    if (!who) return [{ type: "log", message: "track " + c.event + " names nobody (no email and no userId)" }];
    var props = plain(c.properties);
    delete props.email;
    var value = parseFloat((c.properties || {}).revenue !== undefined ? c.properties.revenue : (c.properties || {}).value);
    var ev = { type: "track_event", metric: c.event, profile: who, time: c.timestamp || c.sentAt, properties: props };
    if (isFinite(value)) ev.value = value;
    if (c.messageId) ev.unique_id = "segment:" + c.messageId;
    var out = [];
    if (email) out.push({ type: "upsert_profile", email: email });
    out.push(ev);
    return out;
  }
  return [{ type: "log", message: "ignored Segment " + (c.type || "call") }];
}
