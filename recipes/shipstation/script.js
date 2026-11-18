// ShipStation → DumbMail
//
// ShipStation's webhooks aren't signed and carry only a pointer: `{"resource_url":
// "https://ssapi.shipstation.com/shipments?storeID=123456&batchId=12345678", "resource_type":
// "SHIP_NOTIFY"}`. The integration's URL is the secret, and nothing in a webhook is trusted: the
// batch's shipments are fetched from ShipStation's API with your API key, then each shipped order
// (a shipment doesn't carry the customer's email), and only those answers are recorded. See
// README.md for the ShipStation page behind each assumption.
//
// Order Shipped: once per shipment (voided labels and return labels aren't shipments), on the
// order's customer, with the carrier, service, tracking number and ship date. Customers are held
// back from marketing: a shipment isn't consent. Addresses, items and labels are never stored.

var PAGE_SIZE = 50;
// Pages of one batch looked up from its first page (ShipStation allows 40 requests a minute).
var MAX_PAGES = 40;
var ID = /^[0-9]{1,20}$/;

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

// The query of a URL, as an object.
function queryOf(url) {
  var q = {};
  var i = String(url).indexOf("?");
  if (i < 0) return q;
  String(url).slice(i + 1).split("&").forEach(function (kv) {
    var p = kv.split("=");
    if (p[0]) q[decodeURIComponent(p[0])] = decodeURIComponent(p.slice(1).join("=") || "");
  });
  return q;
}

// The batch (and store) a shipments request names: only digits pass.
function batchQuery(q) {
  var out = {};
  ["batchId", "storeId", "storeID"].forEach(function (k) {
    if (q[k] !== undefined && ID.test(String(q[k]))) out[k] = String(q[k]);
  });
  return out.batchId ? out : null;
}

// The query of the request a fetch made: its `query` pairs (and any in its URL).
function requestQuery(req) {
  var q = queryOf((req && req.url) || "");
  ((req && Array.isArray(req.query)) ? req.query : []).forEach(function (kv) {
    if (Array.isArray(kv) && kv.length === 2) q[String(kv[0])] = String(kv[1]);
  });
  return q;
}

function shipments(input) {
  var body = input.payload || {};
  var asked = requestQuery(input.request);
  var q = batchQuery(asked);
  var page = parseInt(asked.page, 10) || 1;
  var byOrder = {};
  var order = [];
  (Array.isArray(body.shipments) ? body.shipments : []).forEach(function (s) {
    if (s.voided === true || s.isReturnLabel === true) return;
    var oid = String(s.orderId === undefined || s.orderId === null ? "" : s.orderId);
    var sid = String(s.shipmentId === undefined || s.shipmentId === null ? "" : s.shipmentId);
    if (!ID.test(oid) || !ID.test(sid)) return;
    if (!byOrder[oid]) { byOrder[oid] = []; order.push(oid); }
    if (byOrder[oid].length < 20) {
      byOrder[oid].push(clean({ id: sid, order_number: s.orderNumber, tracking_number: s.trackingNumber, carrier: s.carrierCode, service: s.serviceCode, ship_date: s.shipDate }));
    }
  });
  var out = order.map(function (oid) { return { type: "lookup", url: "/orders/" + oid, context: { shipments: byOrder[oid] } }; });
  // The first page asks for the batch's other pages at once.
  var pages = parseInt(body.pages, 10) || 1;
  if (page === 1 && q && pages > 1) {
    for (var n = 2; n <= Math.min(pages, MAX_PAGES); n++) {
      var more = { page: String(n), pageSize: String(PAGE_SIZE) };
      for (var k in q) more[k] = q[k];
      out.push({ type: "lookup", url: "/shipments", query: more });
    }
    if (pages > MAX_PAGES) out.push({ type: "log", message: "the batch has " + pages + " pages; only the first " + MAX_PAGES + " are read" });
  }
  if (!out.length) out.push({ type: "log", message: "no shipped labels on this page" });
  return out;
}

function orderShipped(input, id) {
  var o = input.payload || {};
  if (String(o.orderId) !== id) return [{ type: "log", message: "the answer isn't order " + id }];
  if (!o.customerEmail) return [{ type: "log", message: "order " + id + " has no customer email" }];
  var list = (input.context && Array.isArray(input.context.shipments)) ? input.context.shipments : [];
  var who = { email: String(o.customerEmail) };
  var out = [{ type: "upsert_profile", email: String(o.customerEmail), keys: { shipstation_order: id } }];
  list.forEach(function (s) {
    if (!ID.test(String(s.id))) return;
    out.push({
      type: "track_event", metric: "Order Shipped", profile: who, unique_id: "shipstation:shipment:" + s.id + ":shipped",
      properties: clean({ order_id: id, order_number: o.orderNumber || s.order_number, shipment_id: String(s.id), tracking_number: s.tracking_number, carrier: s.carrier, service: s.service, ship_date: s.ship_date }),
    });
  });
  return out;
}

function handle(input) {
  if (input.source === "pull") return [{ type: "log", message: "this recipe records ShipStation's webhooks; it doesn't import history" }];
  if (input.source === "fetch") {
    var path = String((input.request && input.request.url) || "").replace(/^https?:\/\/[^\/]+/, "").split("?")[0];
    var m = /^\/orders\/([0-9]{1,20})$/.exec(path);
    if (m) return orderShipped(input, m[1]);
    if (path === "/shipments") return shipments(input);
    return [{ type: "log", message: "unexpected answer from " + path }];
  }
  var e = input.payload || {};
  var type = String(e.resource_type || "");
  if (type !== "SHIP_NOTIFY" && type !== "ITEM_SHIP_NOTIFY") return [{ type: "log", message: "ignored ShipStation " + (type || "webhook") }];
  var url = String(e.resource_url || "");
  if (!/^https:\/\/ssapi[0-9]*\.shipstation\.com\/shipments\?/.test(url)) return [{ type: "log", message: "the webhook doesn't point at ShipStation's shipments" }];
  var q = batchQuery(queryOf(url));
  if (!q) return [{ type: "log", message: "the webhook names no batch" }];
  q.page = "1";
  q.pageSize = String(PAGE_SIZE);
  return [{ type: "next_request", url: "/shipments", query: q }];
}
