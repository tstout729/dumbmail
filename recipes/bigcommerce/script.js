// BigCommerce → DumbMail
//
// BigCommerce webhooks are thin: they carry only an id ({"scope": "store/order/updated",
// "data": {"type": "order", "id": 250}}). So the script asks DumbMail to fetch the record from the
// store's API (next_request), and the script runs again on the answer (input.source "fetch").
// Refunds are fetched, then their order (for the buyer and the currency). The pull imports past
// orders. See README.md for the BigCommerce page behind each assumption.
//
// Placed Order: once per order, when its payment_status says the money was taken. Fulfilled Order:
// once, when Shipped or Completed. Cancelled Order: once. Refunded Order: once per refund, with the
// sum of its successful (not declined) payments, and only when it belongs to the webhook's order. A shopper who ticked the checkout's email opt-in box
// (is_email_opt_in) is subscribed to email only when settings.checkout_opt_in_is_marketing is true
// (the installer confirms the box is an unticked marketing opt-in); otherwise everyone is held.

// Paid comes from the order's payment_status (GET /v2/orders/{id}), never from fulfilment: these
// mean the money was taken (and possibly refunded later). authorized, pending, declined, void and
// the rest don't.
var PAID = ["captured", "paid", "partially refunded", "refunded"];
// Order status ids (GET /v2/order_statuses).
var FULFILLED = [2, 10];
var CANCELLED = 5;
var MONTHS = { Jan: 0, Feb: 1, Mar: 2, Apr: 3, May: 4, Jun: 5, Jul: 6, Aug: 7, Sep: 8, Oct: 9, Nov: 10, Dec: 11 };

// RFC 2822 ("Tue, 05 Mar 2019 21:40:11 +0000") → ISO 8601 UTC.
function rfc2822(s) {
  var m = /^\w{3}, (\d{1,2}) (\w{3}) (\d{4}) (\d\d):(\d\d):(\d\d) ([+-])(\d\d)(\d\d)$/.exec(String(s || "").trim());
  if (!m || MONTHS[m[2]] === undefined) return undefined;
  var t = Date.UTC(+m[3], MONTHS[m[2]], +m[1], +m[4], +m[5], +m[6]);
  var off = (+m[8] * 60 + +m[9]) * 60000 * (m[7] === "+" ? 1 : -1);
  return new Date(t - off).toISOString();
}

function num(s) {
  var n = parseFloat(s);
  return isFinite(n) ? n : undefined;
}

function clean(o) {
  var out = {};
  for (var k in o) if (o[k] !== null && o[k] !== undefined && o[k] !== "") out[k] = o[k];
  return out;
}

function e164(phone, country) {
  if (!phone) return undefined;
  var p = String(phone).trim();
  var d = p.replace(/[^0-9]/g, "");
  if (p.charAt(0) === "+" && d.length >= 8 && d.length <= 15) return "+" + d;
  if ((country === "US" || country === "CA") && d.length === 10) return "+1" + d;
  return undefined;
}

var input_settings = {};

function orderActions(o, ctx, historical) {
  if (!o || !o.id) return [{ type: "log", message: "no order in the answer" }];
  if (o.status_id === 0 || o.is_deleted) return [{ type: "log", message: "order " + o.id + " is incomplete or archived" }];
  var b = o.billing_address || {};
  if (!b.email) return [{ type: "log", message: "order " + o.id + " has no billing email" }];
  var who = { email: b.email };
  var p = { type: "upsert_profile", email: b.email };
  if (b.first_name) p.first_name = b.first_name;
  if (b.last_name) p.last_name = b.last_name;
  var ph = e164(b.phone, b.country_iso2);
  if (ph) p.phone_number = ph;
  if (o.customer_id > 0) p.keys = { bigcommerce_customer: String(o.customer_id) };
  // Only when the installer has confirmed the store's checkout box is an unticked, affirmative
  // marketing opt-in: BigCommerce describes is_email_opt_in only as agreeing "to receive emails".
  if (o.is_email_opt_in === true && input_settings.checkout_opt_in_is_marketing === true) p.consent = [{ channel: "email", subscribed: true, source: "BigCommerce checkout email opt-in" }];
  var out = [p];
  var currency = o.default_currency_code || o.currency_code;
  var base = clean({ order_id: o.id, status: o.status, currency: currency, channel_id: o.channel_id, historical: historical || undefined });
  var modified = rfc2822(o.date_modified);
  var total = num(o.total_inc_tax);
  var paid = PAID.indexOf(String(o.payment_status || "").toLowerCase()) >= 0;
  if (paid && total > 0) {
    var placed = JSON.parse(JSON.stringify(base));
    placed.item_count = o.items_total;
    out.push({ type: "track_event", metric: "Placed Order", profile: who, value: total, time: rfc2822(o.date_created), unique_id: "bigcommerce:order:" + o.id + ":placed", properties: placed });
  } else if (!paid) {
    out.push({ type: "log", message: "order " + o.id + "'s payment is " + (o.payment_status || "unknown") + "; it's recorded once it's paid" });
  }
  if (FULFILLED.indexOf(o.status_id) >= 0) out.push({ type: "track_event", metric: "Fulfilled Order", profile: who, time: rfc2822(o.date_shipped) || modified, unique_id: "bigcommerce:order:" + o.id + ":fulfilled", properties: base });
  if (o.status_id === CANCELLED) out.push({ type: "track_event", metric: "Cancelled Order", profile: who, time: modified, unique_id: "bigcommerce:order:" + o.id + ":cancelled", properties: base });
  var rf = ctx && ctx.refund;
  if (rf) {
    out.push({
      type: "track_event", metric: "Refunded Order", profile: who, value: num(rf.total_amount), time: rf.created || modified,
      unique_id: "bigcommerce:refund:" + rf.id,
      properties: clean({ order_id: o.id, refund_id: rf.id, currency: currency, reason: rf.reason, historical: historical || undefined }),
    });
  }
  return out;
}

function fetched(input) {
  var url = String((input.request && input.request.url) || "");
  var p = input.payload || {};
  var ctx = input.context || {};
  if (/\/payment_actions\/refunds\/\d+/.test(url)) {
    var r = p.data;
    if (!r || !r.id) return [{ type: "log", message: "no refund in the answer" }];
    // The refund must belong to the order the webhook named (BigCommerce resolves a refund id from
    // another order too), and only its successful payments count.
    if (ctx.webhook_order_id && String(r.order_id) !== String(ctx.webhook_order_id)) return [{ type: "log", message: "refund " + r.id + " belongs to order " + r.order_id + ", not " + ctx.webhook_order_id + "; ignored" }];
    // A part counts only when BigCommerce says it wasn't declined (is_declined: false); a part
    // without the flag isn't proof the money moved.
    var ok = (r.payments || []).filter(function (x) { return x.is_declined === false && num(x.amount) > 0; });
    var amount = ok.reduce(function (t, x) { return t + num(x.amount); }, 0);
    if (!(amount > 0)) return [{ type: "log", message: "refund " + r.id + " has no successful payment" }];
    // The order gives the buyer and the currency.
    if (!idOf(r.order_id)) return [{ type: "log", message: "refund " + r.id + " names no order" }];
    return [{ type: "next_request", url: "/stores/" + input.settings.store_hash + "/v2/orders/" + idOf(r.order_id), context: { refund: clean({ id: r.id, total_amount: Math.round(amount * 100) / 100, created: r.created, reason: r.reason }) } }];
  }
  if (/\/v3\/customers/.test(url)) {
    return (p.data || []).filter(function (c) { return c.email; }).map(function (c) {
      var u = { type: "upsert_profile", email: c.email, keys: { bigcommerce_customer: String(c.id) } };
      if (c.first_name) u.first_name = c.first_name;
      if (c.last_name) u.last_name = c.last_name;
      var ph = e164(c.phone);
      if (ph) u.phone_number = ph;
      return u;
    });
  }
  if (/\/v2\/orders\/\d+/.test(url)) return orderActions(p, ctx, false);
  return [{ type: "log", message: "unexpected answer from " + url.split("?")[0] }];
}

var LIMIT = 50;

// Pages run oldest change first from the cursor (inclusive). A full page asks for the next one and
// remembers its newest change; a short or empty page ends the run, moves the cursor to the newest
// change read, and starts the next run from page 1 (an empty page after a full one never leaves
// the run stuck on a page past the end).
function pulled(input) {
  var rows = Array.isArray(input.payload) ? input.payload : [];
  var st = input.state || {};
  var out = [];
  rows.forEach(function (o) { out = out.concat(orderActions(o, null, !!input.backfill)); });
  var page = parseInt(st.page || "1", 10) || 1;
  var newest = rows.length ? rfc2822(rows[rows.length - 1].date_modified) : null;
  if (rows.length >= LIMIT) {
    out.push({ type: "set_state", values: { page: String(page + 1), pending: newest } });
    out.push({ type: "next_request", query: { page: String(page + 1) } });
    return out;
  }
  var cursor = newest || st.pending || st.cursor || null;
  out.push({ type: "set_state", values: { cursor: cursor, page: "1", pending: null } });
  if (!rows.length) out.push({ type: "log", message: "no changed orders" });
  return out;
}

// Ids that go into a fetch path: digits only (the integration refuses anything else too).
function idOf(v) {
  var s = String(v === undefined || v === null ? "" : v);
  return /^[0-9]{1,20}$/.test(s) ? s : null;
}

function handle(input) {
  input_settings = input.settings || {};
  if (input.source === "fetch") return fetched(input);
  if (input.source === "pull") return pulled(input);
  var e = input.payload || {};
  var d = e.data || {};
  var hash = input.settings && input.settings.store_hash;
  if (!hash) return [{ type: "log", message: "set store_hash in the integration's settings first" }];
  if (e.producer && e.producer !== "stores/" + hash) return [{ type: "log", message: "this webhook is from another store (" + e.producer + ")" }];
  var scope = String(e.scope || "");
  var id = idOf(d.id);
  if (!id) return [{ type: "log", message: "ignored BigCommerce scope " + (scope || "(none)") + " without a numeric id" }];
  if (scope === "store/order/refund/created" && d.refund && idOf(d.refund.refund_id)) {
    return [{ type: "next_request", url: "/stores/" + hash + "/v3/orders/" + id + "/payment_actions/refunds/" + idOf(d.refund.refund_id), context: { webhook_order_id: id } }];
  }
  if (/^store\/order\/(created|updated|statusUpdated)$/.test(scope)) {
    return [{ type: "next_request", url: "/stores/" + hash + "/v2/orders/" + id }];
  }
  if (/^store\/customer\/(created|updated)$/.test(scope)) {
    return [{ type: "next_request", url: "/stores/" + hash + "/v3/customers", query: { "id:in": id } }];
  }
  return [{ type: "log", message: "ignored BigCommerce scope " + (scope || "(none)") }];
}
