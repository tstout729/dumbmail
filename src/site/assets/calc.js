// Monthly cost calculator. The server renders the default answer; this only updates it as you type.
// Same rule as billing: free emails first, then a plan's emails, then credit at the pay-as-you-go price.
(function () {
  var form = document.getElementById("calc");
  if (!form) return;
  var p = JSON.parse(form.getAttribute("data-pricing"));
  var out = form.querySelector("output");
  var how = form.querySelector(".how");
  var fmt = function (v) {
    return "$" + v.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  };
  var int = function (v) { return Math.round(v).toLocaleString("en-US"); };
  function update() {
    var contacts = Math.max(0, Number(form.contacts.value) || 0);
    var per = Math.max(0, Number(form.per.value) || 0);
    var emails = Math.round(contacts * per);
    var billable = Math.max(0, emails - p.free);
    var best = { total: (billable * p.per1000) / 1000, tier: null };
    p.tiers.forEach(function (t) {
      var total = t.price + (Math.max(0, billable - t.emails) * p.per1000) / 1000;
      if (total < best.total - 1e-9) best = { total: total, tier: t };
    });
    out.textContent = fmt(best.total);
    var text = int(emails) + " emails a month, ";
    if (!best.tier) {
      text += billable === 0 ? "all inside the free " + int(p.free) + "." : "paid as you go.";
    } else {
      var extra = best.total - best.tier.price;
      text += "on the " + fmt(best.tier.price) + " plan (" + int(best.tier.emails) + " emails)" + (extra > 0.004 ? " plus " + fmt(extra) + " of credit." : ".");
    }
    how.textContent = text;
  }
  form.addEventListener("input", update);
  form.addEventListener("submit", function (e) { e.preventDefault(); update(); });
})();
