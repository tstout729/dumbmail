// Segment → DumbMail (Webhooks destination)
//
// identify calls with traits.email become profiles (matched by the Segment userId first, then
// email; the userId is kept as a key, on one profile only). track calls become events named after the Segment event, only for the event names listed in
// settings.events (and in the integration's metrics), on the person an earlier identify gave that
// userId (waiting for it if needed). An email inside a track call is never used to pick the person.
// Each call's messageId is its unique id. Nobody is subscribed: Segment calls carry no marketing
// opt-in, and names aren't taken from calls.
// Anyone holding a Segment write key can send calls, and browser sources' keys are public. So calls
// from Segment's browser and mobile libraries are refused unless settings.allow_client_sources is
// true; connect only server-side sources you trust.

var CLIENT_LIBRARIES = ["analytics.js", "analytics-next", "analytics-ios", "analytics-android", "analytics-react-native", "analytics-kotlin", "analytics-swift", "analytics-flutter"];
// See README.md for the Segment page behind each assumption.

// Properties of Segment's ecommerce spec (v2) that are copied onto events; free-text ones (review
// bodies, share messages, search queries, recipients) aren't. Add others in
// settings.copy_properties.
var SPEC = ["affiliation", "brand", "cart_id", "category", "checkout_id", "coupon", "coupon_id", "coupon_name", "creative", "currency", "discount", "image_url", "list_id", "name", "order_id", "payment_method", "position", "price", "product_id", "products", "promotion_id", "quantity", "rating", "reason", "revenue", "review_id", "shipping", "shipping_method", "sku", "step", "subtotal", "tax", "total", "url", "value", "variant", "wishlist_id", "wishlist_name"];
var PRODUCT = ["product_id", "sku", "category", "name", "brand", "variant", "price", "quantity", "coupon", "position", "url", "image_url"];

function pick(o, keys) {
  var out = {};
  keys.forEach(function (k) {
    var v = (o || {})[k];
    if (v === null || v === undefined) return;
    if (k === "products" && Array.isArray(v)) v = v.slice(0, 50).map(function (p) { return pick(p, PRODUCT); });
    else if (typeof v === "object") return;
    out[k] = v;
  });
  return out;
}

function handle(input) {
  var c = input.payload || {};
  var settings = input.settings || {};
  var events = settings.events || [];
  var lib = String((c.context && c.context.library && c.context.library.name) || "").toLowerCase();
  if (CLIENT_LIBRARIES.indexOf(lib) >= 0 && settings.allow_client_sources !== true) return [{ type: "log", message: "refused a call from " + lib + " (a browser or app library); see the recipe's setup" }];
  if (c.type === "identify") {
    var t = c.traits || {};
    if (!t.email) return [{ type: "log", message: "identify without traits.email" }];
    // Matched by the Segment userId first (as a namespaced external id), so a new email for the same
    // user changes that profile's address (its consent doesn't carry over) instead of putting the
    // userId on a second profile.
    var up = { type: "upsert_profile", email: t.email };
    if (c.userId) up.external_id = "segment:" + String(c.userId).slice(0, 190);
    if (c.userId) up.keys = { segment_user_id: String(c.userId) };
    return [up];
  }
  if (c.type === "track") {
    if (events.indexOf(c.event) < 0) return [{ type: "log", message: "track " + c.event + " isn't in settings.events (add it there and to the integration's metrics to record it)" }];
    if (!c.userId) return [{ type: "log", message: "track " + c.event + " has no userId" }];
    var who = { property: "segment_user_id", value: String(c.userId), if_missing: "retry" };
    var extra = Array.isArray(settings.copy_properties) ? settings.copy_properties.filter(function (k) { return typeof k === "string" && k !== "email"; }).slice(0, 50) : [];
    var props = pick(c.properties, SPEC.concat(extra));
    var value = parseFloat((c.properties || {}).revenue !== undefined ? c.properties.revenue : (c.properties || {}).value);
    var ev = { type: "track_event", metric: c.event, profile: who, time: c.timestamp || c.sentAt, properties: props };
    if (isFinite(value)) ev.value = value;
    if (c.messageId) ev.unique_id = "segment:" + c.messageId;
    return [ev];
  }
  return [{ type: "log", message: "ignored Segment " + (c.type || "call") }];
}
