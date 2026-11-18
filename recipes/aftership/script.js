// AfterShip → DumbMail
//
// tracking_update webhooks become events on the shipment's customer: Shipment In Transit, Out For
// Delivery, Delivered and Shipment Exception (Exception or a failed attempt), once per shipment and
// status. The customer is msg.customers[] (not the sender). Shipments aren't consent: nobody is
// subscribed. See README.md for the AfterShip page behind each assumption.

var METRICS = { InTransit: "Shipment In Transit", OutForDelivery: "Out For Delivery", Delivered: "Delivered", Exception: "Shipment Exception", AttemptFail: "Shipment Exception" };

function e164(p) {
  if (!p) return undefined;
  var s = String(p).trim();
  var d = s.replace(/[^0-9]/g, "");
  return s.charAt(0) === "+" && d.length >= 8 && d.length <= 15 ? "+" + d : undefined;
}

function handle(input) {
  var p = input.payload || {};
  if (p.event !== "tracking_update" || !p.msg) return [{ type: "log", message: "not an AfterShip tracking_update" }];
  var m = p.msg;
  var metric = METRICS[m.tag];
  if (!metric) return [{ type: "log", message: "shipment " + m.tracking_number + " is " + m.tag + "; not recorded" }];
  var c = (m.customers || []).filter(function (x) { return x && x.email && x.role !== "sender"; })[0];
  if (!c) return [{ type: "log", message: "shipment " + m.tracking_number + " has no customer email" }];
  var up = { type: "upsert_profile", email: c.email };
  var ph = e164(c.phone_number);
  if (ph) up.phone_number = ph;
  var eta = (m.latest_estimated_delivery && (m.latest_estimated_delivery.datetime || m.latest_estimated_delivery.datetime_max)) || (m.aftership_estimated_delivery_date && m.aftership_estimated_delivery_date.estimated_delivery_date);
  var props = { tracking_number: m.tracking_number, courier: m.slug, status: m.tag, status_detail: m.subtag_message, order_number: m.order_number, order_id: m.order_id, title: m.title, expected_delivery: eta, tracking_url: m.aftership_tracking_url, delivered_at: m.shipment_delivery_date };
  for (var k in props) if (props[k] === undefined || props[k] === null || props[k] === "") delete props[k];
  return [up, { type: "track_event", metric: metric, profile: { email: c.email }, time: m.updated_at, unique_id: "aftership:" + (m.id || m.tracking_number) + ":" + m.tag, properties: props }];
}
