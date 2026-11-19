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
// order's customer, with the carrier, service, tracking number and ship date, only for the stores
// you list (settings.store_ids). Customers are held back from marketing: a shipment isn't
// consent. Addresses, items and labels are never stored.
// Each lookup of an order carries that page's shipments for it, and is its own piece of work (the
// recipe's fetch identity is the record and its context), so a second batch naming the same order
// never replaces a waiting or running lookup's shipments.

var PAGE_SIZE = 50;
// Pages of one batch read from one webhook: its first page asks for the rest (each page's orders
// are a lookup further, and one webhook's lookups chain at most three deep).
var MAX_PAGES = 50;
var ID = /^[0-9]{1,20}$/;

function stores(settings) {
  return Array.isArray(settings.store_ids) ? settings.store_ids.map(String) : [];
}

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
    byOrder[oid].push(clean({ id: sid, order_number: s.orderNumber, tracking_number: s.trackingNumber, carrier: s.carrierCode, service: s.serviceCode, ship_date: s.shipDate }));
  });
  var out = order.map(function (oid) { return { type: "lookup", url: "/orders/" + oid, context: { shipments: byOrder[oid] } }; });
  // The first page asks for the batch's other pages at once.
  var pages = parseInt(body.pages, 10) || 1;
  if (page === 1 && q && pages > 1) {
    // Never a silent cut: a batch bigger than one webhook can read fails, visibly, in the log.
    if (pages > MAX_PAGES) throw new Error("ShipStation batch " + q.batchId + " has " + pages + " pages of shipments; one webhook reads at most " + MAX_PAGES + " (" + MAX_PAGES * PAGE_SIZE + " shipments)");
    for (var n = 2; n <= pages; n++) {
      var more = { page: String(n), pageSize: String(PAGE_SIZE) };
      for (var k in q) more[k] = q[k];
      out.push({ type: "lookup", url: "/shipments", query: more });
    }
  }
  if (!out.length) out.push({ type: "log", message: "no shipped labels on this page" });
  return out;
}

function orderShipped(input, id) {
  var o = input.payload || {};
  if (String(o.orderId) !== id) return [{ type: "log", message: "the answer isn't order " + id }];
  var store = o.advancedOptions && o.advancedOptions.storeId;
  if (stores(input.settings || {}).indexOf(String(store)) < 0) return [{ type: "log", message: "order " + id + " is from a store this integration doesn't record (store_ids)" }];
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
  var mine = stores(input.settings || {});
  if (!mine.length) return [{ type: "log", message: "set store_ids in the integration's settings to the stores to record" }];
  var named = q.storeID || q.storeId;
  if (named && mine.indexOf(named) < 0) return [{ type: "log", message: "ignored a batch for another store (" + named + ")" }];
  q.page = "1";
  q.pageSize = String(PAGE_SIZE);
  return [{ type: "next_request", url: "/shipments", query: q }];
}
