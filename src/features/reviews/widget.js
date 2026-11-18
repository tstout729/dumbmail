/* DumbMail review widgets. The website script loads this when a page has a review widget:
   <div data-dumbmail-reviews="PRODUCT_ID"></div>           product reviews (summary, list, photos)
   <span data-dumbmail-stars="PRODUCT_ID"></span>          star badge for product cards
   <div data-dumbmail-reviews-carousel></div>              featured reviews
   <div data-dumbmail-reviews-all></div>                   every product's reviews
   Each renders in its own shadow root, so the store's styles and these never mix. */
(function () {
  "use strict";
  if (window.__dumbmailReviews) { window.__dumbmailReviews.scan(); return; }
  var BASE = __BASE__, KEY = __KEY__;
  var HEX = /^#[0-9a-fA-F]{3,8}$/;
  var SLOTS = "[data-dumbmail-reviews],[data-dumbmail-stars],[data-dumbmail-reviews-carousel],[data-dumbmail-reviews-all]";
  var HEIGHTS = { reviews: 360, stars: 20, carousel: 220, all: 480 };
  var ldDone = {};

  function esc(s) { return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]; }); }
  function color(c, d) { return HEX.test(c || "") ? c : d; }
  function get(path, q) {
    var u = BASE + path + "?k=" + encodeURIComponent(KEY);
    for (var k in q) if (q[k] != null && q[k] !== "") u += "&" + k + "=" + encodeURIComponent(q[k]);
    return fetch(u, { credentials: "omit" }).then(function (r) { if (!r.ok) throw new Error(String(r.status)); return r.json(); });
  }
  function kindOf(el) {
    if (el.hasAttribute("data-dumbmail-stars")) return "stars";
    if (el.hasAttribute("data-dumbmail-reviews-carousel")) return "carousel";
    if (el.hasAttribute("data-dumbmail-reviews-all")) return "all";
    return "reviews";
  }
  function productOf(el, attr) { return el.getAttribute(attr) || el.getAttribute("data-product-id") || ""; }

  function css(w) {
    var star = color(w && w.star_color, "#f59e0b"), text = color(w && w.text_color, "#111827"), accent = color(w && w.accent_color, "#1f6f5c");
    return "<style>:host{all:initial;display:block;font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:" + text + "}" +
      ":host([data-kind=stars]){display:inline-block}*{box-sizing:border-box}button,select,input,textarea{font:inherit;color:inherit}" +
      ".st{position:relative;display:inline-block;color:#d1d5db;letter-spacing:1px;line-height:1;white-space:nowrap}.st span{position:absolute;left:0;top:0;overflow:hidden;color:" + star + "}" +
      "h2{font-size:20px;margin:0 0 12px}.sum{display:flex;flex-wrap:wrap;gap:24px;align-items:center;margin:0 0 16px}.avg{font-size:32px;font-weight:700;line-height:1}" +
      ".hist{flex:1;min-width:200px;max-width:340px}.hist button{display:flex;gap:8px;align-items:center;width:100%;background:none;border:0;padding:2px 0;cursor:pointer;text-align:left}" +
      ".bar{flex:1;height:8px;border-radius:4px;background:#e5e7eb;overflow:hidden}.bar i{display:block;height:100%;background:" + star + "}.hist button[aria-pressed=true]{font-weight:700}" +
      ".tools{display:flex;flex-wrap:wrap;gap:8px;align-items:center;margin:0 0 8px}.tools select,.btn{border:1px solid #d1d5db;background:#fff;border-radius:6px;padding:6px 10px;cursor:pointer}" +
      ".btn.p{background:" + accent + ";border-color:" + accent + ";color:#fff}.rv{border-top:1px solid #e5e7eb;padding:14px 0}.rv h3{font-size:15px;margin:4px 0}.meta{font-size:13px;color:#6b7280}" +
      ".badge{display:inline-block;font-size:12px;border-radius:999px;padding:1px 8px;background:#ecfdf5;color:#065f46;margin-left:6px}.inc{font-size:12px;color:#6b7280;margin-top:4px}" +
      ".body{white-space:pre-line;margin:6px 0}.ans{font-size:13px;color:#4b5563}.ph{display:flex;flex-wrap:wrap;gap:6px;margin:8px 0}.ph button{border:0;padding:0;background:none;cursor:zoom-in}" +
      ".ph img{width:64px;height:64px;object-fit:cover;border-radius:6px;display:block}.reply{border-left:3px solid " + accent + ";padding:6px 10px;margin:8px 0 0;background:#f9fafb;font-size:13px}" +
      ".pg{display:flex;gap:8px;align-items:center;justify-content:center;margin:12px 0 0}.empty{color:#6b7280;padding:8px 0}" +
      ".lb{position:fixed;inset:0;background:rgba(0,0,0,.85);display:flex;align-items:center;justify-content:center;z-index:2147483647;padding:16px}.lb img{max-width:100%;max-height:90vh;border-radius:6px}" +
      ".lb button{position:absolute;top:12px;right:12px;background:#fff;border:0;border-radius:50%;width:40px;height:40px;font-size:22px;cursor:pointer}" +
      ".car{display:flex;gap:12px;overflow-x:auto;scroll-snap-type:x mandatory;padding:4px 2px 8px}.card{flex:0 0 min(280px,85%);scroll-snap-align:start;border:1px solid #e5e7eb;border-radius:10px;padding:14px;background:#fff}" +
      ".card .body{display:-webkit-box;-webkit-line-clamp:5;-webkit-box-orient:vertical;overflow:hidden}.nav{display:flex;gap:8px;justify-content:flex-end}" +
      "form.w{border:1px solid #e5e7eb;border-radius:10px;padding:14px;margin:0 0 12px}form.w label{display:block;font-weight:600;margin:10px 0 4px}form.w input[type=text],form.w input[type=email],form.w textarea{width:100%;padding:8px;border:1px solid #d1d5db;border-radius:6px}" +
      "fieldset{border:0;padding:0;margin:0}.pick input{position:absolute;opacity:0;width:1px;height:1px}.pick label{display:inline;font-size:28px;color:#d1d5db;cursor:pointer;margin:0}.pick label.on{color:" + star + "}.pick input:focus-visible+label{outline:2px solid " + accent + "}" +
      ".msg{margin:8px 0;font-size:13px}.sr{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)}:focus-visible{outline:2px solid " + accent + ";outline-offset:2px}</style>";
  }
  function stars(v, size) {
    var pct = Math.max(0, Math.min(100, (Number(v) || 0) / 5 * 100));
    return '<span class="st" style="font-size:' + (size || 16) + 'px" aria-hidden="true">★★★★★<span style="width:' + pct + '%">★★★★★</span></span>';
  }
  function plural(n, w) { return n + " " + w + (n === 1 ? "" : "s"); }

  function reviewHtml(r, withProduct) {
    var h = '<article class="rv">' + stars(r.rating, 15) + '<span class="sr">' + r.rating + ' out of 5 stars</span>';
    if (r.title) h += "<h3>" + esc(r.title) + "</h3>";
    h += '<div class="meta">' + esc(r.name) + " · " + esc(r.date) + (r.verified ? '<span class="badge">Verified buyer</span>' : "") + "</div>";
    if (withProduct && r.product && r.product.name) h += '<div class="meta">' + (r.product.url && /^https?:/.test(r.product.url) ? '<a href="' + esc(r.product.url) + '">' + esc(r.product.name) + "</a>" : esc(r.product.name)) + "</div>";
    if (r.body) h += '<p class="body">' + esc(r.body) + "</p>";
    (r.answers || []).forEach(function (a) { h += '<div class="ans">' + esc(a.label) + ": " + esc(a.answer) + "</div>"; });
    if (r.recommend === true) h += '<div class="ans">Recommends this</div>';
    if (r.photos && r.photos.length) {
      h += '<div class="ph">';
      r.photos.forEach(function (p, i) { h += '<button type="button" data-photo="' + esc(p.url) + '" aria-label="Open photo ' + (i + 1) + ' from ' + esc(r.name) + '"><img loading="lazy" src="' + esc(p.url) + '" alt="Photo from ' + esc(r.name) + '" width="64" height="64"></button>'; });
      h += "</div>";
    }
    if (r.incentivized) h += '<div class="inc">Received a discount for this review</div>';
    if (r.reply) h += '<div class="reply"><strong>Reply from the store</strong> · ' + esc(r.reply.date || "") + "<br>" + esc(r.reply.text) + "</div>";
    return h + "</article>";
  }

  function lightbox(root, url, back) {
    var box = document.createElement("div");
    box.className = "lb"; box.setAttribute("role", "dialog"); box.setAttribute("aria-modal", "true"); box.setAttribute("aria-label", "Review photo");
    box.innerHTML = '<img alt="Review photo" src="' + esc(url) + '"><button type="button" aria-label="Close">×</button>';
    function close() { box.remove(); document.removeEventListener("keydown", key, true); if (back) back.focus(); }
    function key(e) { if (e.key === "Escape") close(); if (e.key === "Tab") { e.preventDefault(); box.querySelector("button").focus(); } }
    box.addEventListener("click", function (e) { if (e.target === box || e.target.tagName === "BUTTON") close(); });
    document.addEventListener("keydown", key, true);
    root.appendChild(box);
    box.querySelector("button").focus();
  }

  // The structured data comes with the first page and describes exactly the reviews shown there.
  function injectLd(product, ld) {
    if (!ld || ldDone[product]) return;
    ldDone[product] = 1;
    var s = document.createElement("script");
    s.type = "application/ld+json"; s.setAttribute("data-dumbmail-reviews-ld", product);
    s.textContent = JSON.stringify(ld).replace(/</g, "\\u003c");
    document.head.appendChild(s);
  }

  function productWidget(host, root) {
    var product = productOf(host, "data-dumbmail-reviews"), state = { sort: "", rating: "", photos: false }, writing = false, last = null, shown = [];
    if (!product) return;
    function load() {
      shown = [];
      more(null);
    }
    function more(after) {
      get("/client/reviews", { product_id: product, after: after, sort: state.sort, rating: state.rating, photos: state.photos ? "true" : "" }).then(function (d) {
        shown = shown.concat(d.reviews); d.reviews = shown; last = d; draw(d);
        if (host.getAttribute("data-structured-data") !== "false") injectLd(product, d.structured_data);
      })
        .catch(function () { root.innerHTML = ""; host.style.minHeight = ""; });
    }
    function draw(d) {
      var s = d.summary, h = css(d.widget) + '<section aria-label="' + esc(d.widget.heading) + '"><h2>' + esc(d.widget.heading) + "</h2>";
      if (s.count > 0) {
        h += '<div class="sum"><div><div class="avg">' + Number(s.average).toFixed(1) + '</div>' + stars(s.average, 18) + '<div class="meta">' + plural(s.count, "review") + '</div><span class="sr">Rated ' + Number(s.average).toFixed(1) + " out of 5 from " + plural(s.count, "review") + '</span></div><div class="hist" role="group" aria-label="Filter by rating">';
        for (var n = 5; n >= 1; n--) {
          var c = s.histogram[String(n)] || 0, pct = s.count ? Math.round(c / s.count * 100) : 0;
          h += '<button type="button" data-rating="' + n + '" aria-pressed="' + (String(state.rating) === String(n)) + '" aria-label="' + n + " stars: " + plural(c, "review") + '"><span>' + n + '★</span><span class="bar"><i style="width:' + pct + '%"></i></span><span>' + c + "</span></button>";
        }
        h += "</div></div>";
        h += '<div class="tools"><label class="sr" for="sort">Sort reviews</label><select id="sort"><option value="">Most relevant</option><option value="recent"' + (state.sort === "recent" ? " selected" : "") + '>Newest</option><option value="highest"' + (state.sort === "highest" ? " selected" : "") + '>Highest rated</option><option value="lowest"' + (state.sort === "lowest" ? " selected" : "") + '>Lowest rated</option></select>' +
          '<button type="button" class="btn" data-a="photos" aria-pressed="' + state.photos + '">With photos</button>' + (state.rating ? '<button type="button" class="btn" data-a="clear">Show all ratings</button>' : "") + (d.can_write ? '<button type="button" class="btn p" data-a="write">Write a review</button>' : "") + "</div>";
      } else if (d.can_write) {
        h += '<p class="empty">No reviews yet.</p><div class="tools"><button type="button" class="btn p" data-a="write">Be the first to write one</button></div>';
      } else {
        h += '<p class="empty">No reviews yet.</p>';
      }
      if (writing) h += writeForm(d);
      h += '<div aria-live="polite">';
      if (s.count > 0 && !d.reviews.length) h += '<p class="empty">No reviews match.</p>';
      d.reviews.forEach(function (r) { h += reviewHtml(r, false); });
      h += "</div>";
      if (d.next) h += '<nav class="pg"><button type="button" class="btn" data-a="more" data-after="' + esc(d.next) + '">Show more reviews</button></nav>';
      root.innerHTML = h + "</section>";
      host.style.minHeight = "";
      if (writing) wireForm(d);
    }
    function writeForm(d) {
      var h = '<form class="w" novalidate><fieldset class="pick"><legend style="font-weight:600">Your rating</legend>';
      for (var i = 1; i <= 5; i++) h += '<input type="radio" name="r" id="w' + i + '" value="' + i + '"><label for="w' + i + '" aria-label="' + i + ' star' + (i > 1 ? "s" : "") + '">★</label>';
      h += '</fieldset><label for="wt">Title</label><input type="text" id="wt" maxlength="150"><label for="wb">Your review</label><textarea id="wb" rows="4" maxlength="5000"></textarea>' +
        '<label for="wn">Name shown with your review</label><input type="text" id="wn" maxlength="60" autocomplete="given-name"><label for="we">Your email (to confirm your review, never shown)</label><input type="email" id="we" autocomplete="email" required>' +
        '<div class="msg" role="alert"></div><button type="submit" class="btn p">Send review</button> <button type="button" class="btn" data-a="cancel">Cancel</button></form>';
      return h;
    }
    function wireForm() {
      var f = root.querySelector("form.w"), pick = f.querySelector(".pick"), rating = 0;
      pick.addEventListener("change", function (e) { rating = Number(e.target.value); [].forEach.call(pick.querySelectorAll("label"), function (l, i) { l.className = i < rating ? "on" : ""; }); });
      f.addEventListener("submit", function (e) {
        e.preventDefault();
        var msg = f.querySelector(".msg");
        if (!rating) { msg.textContent = "Choose a star rating."; return; }
        var body = { product_id: product, rating: rating, title: f.querySelector("#wt").value, body: f.querySelector("#wb").value, name: f.querySelector("#wn").value, email: f.querySelector("#we").value };
        fetch(BASE + "/client/reviews/write?k=" + encodeURIComponent(KEY), { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), credentials: "omit" })
          .then(function (r) { return r.json().then(function (j) { return { ok: r.ok, j: j }; }); })
          .then(function (x) { msg.textContent = x.ok ? x.j.message : (x.j.error && x.j.error.message) || "Your review couldn't be sent."; if (x.ok) f.querySelector("button[type=submit]").disabled = true; })
          .catch(function () { msg.textContent = "Your review couldn't be sent. Check your connection."; });
      });
      f.querySelector("#w1").focus();
    }
    root.addEventListener("click", function (e) {
      var t = e.target.closest ? e.target.closest("button") : null;
      if (!t) return;
      var ph = t.getAttribute("data-photo"), r = t.getAttribute("data-rating"), a = t.getAttribute("data-a");
      if (ph) return lightbox(root, ph, t);
      if (r) { state.rating = String(state.rating) === r ? "" : r; return load(); }
      if (a === "photos") { state.photos = !state.photos; return load(); }
      if (a === "clear") { state.rating = ""; return load(); }
      if (a === "more") return more(t.getAttribute("data-after"));
      if (a === "write") { writing = true; return draw(last); }
      if (a === "cancel") { writing = false; return draw(last); }
    });
    root.addEventListener("change", function (e) { if (e.target.id === "sort") { state.sort = e.target.value; load(); } });
    load();
  }

  var badgeQueue = [], badgeTimer = 0;
  function badge(host, root) {
    var id = productOf(host, "data-dumbmail-stars");
    if (!id) return;
    badgeQueue.push({ id: id, host: host, root: root });
    clearTimeout(badgeTimer);
    badgeTimer = setTimeout(flushBadges, 50);
  }
  function flushBadges() {
    var q = badgeQueue.splice(0, badgeQueue.length);
    for (var i = 0; i < q.length; i += 50) (function (part) {
      var ids = part.map(function (b) { return b.id; }).filter(function (v, j, a) { return a.indexOf(v) === j; });
      get("/client/reviews/summary", { ids: ids.join(",") }).then(function (d) {
        part.forEach(function (b) {
          var s = d.data[b.id];
          if (!s || !s.count) { b.root.innerHTML = ""; b.host.style.minHeight = ""; return; }
          b.root.innerHTML = css(d.widget) + '<span role="img" aria-label="Rated ' + Number(s.average).toFixed(1) + " out of 5, " + plural(s.count, "review") + '">' + stars(s.average, 14) + ' <span class="meta">(' + s.count + ")</span></span>";
        });
      }).catch(function () {});
    })(q.slice(i, i + 50));
  }

  function carousel(host, root) {
    get("/client/reviews/all", { featured: "true", limit: host.getAttribute("data-limit") || 10 }).then(function (d) {
      if (!d.reviews.length) { root.innerHTML = ""; host.style.minHeight = ""; return; }
      var h = css(d.widget) + '<section aria-label="' + esc(d.widget.heading) + '"><div class="nav"><button type="button" class="btn" data-a="l" aria-label="Previous reviews">‹</button><button type="button" class="btn" data-a="r" aria-label="Next reviews">›</button></div><div class="car" tabindex="0">';
      d.reviews.forEach(function (r) {
        h += '<div class="card">' + stars(r.rating, 15) + (r.title ? "<h3 style=\"font-size:15px;margin:6px 0\">" + esc(r.title) + "</h3>" : "") + (r.body ? '<p class="body">' + esc(r.body) + "</p>" : "") +
          '<div class="meta">' + esc(r.name) + (r.verified ? '<span class="badge">Verified buyer</span>' : "") + "</div>" + (r.product && r.product.name ? '<div class="meta">' + esc(r.product.name) + "</div>" : "") +
          (r.incentivized ? '<div class="inc">Received a discount for this review</div>' : "") + "</div>";
      });
      root.innerHTML = h + "</div></section>";
      host.style.minHeight = "";
      var car = root.querySelector(".car");
      root.addEventListener("click", function (e) {
        var a = e.target.getAttribute && e.target.getAttribute("data-a");
        if (a) car.scrollBy({ left: (a === "l" ? -1 : 1) * car.clientWidth * 0.8, behavior: "smooth" });
      });
    }).catch(function () { root.innerHTML = ""; });
  }

  function allReviews(host, root) {
    var shown = [];
    function load(after) {
      get("/client/reviews/all", { after: after }).then(function (d) {
        shown = shown.concat(d.reviews); d.reviews = shown;
        var h = css(d.widget) + '<section aria-label="' + esc(d.widget.heading) + '"><h2>' + esc(d.widget.heading) + "</h2>";
        if (d.summary.count) h += '<div class="sum"><div class="avg">' + Number(d.summary.average).toFixed(1) + "</div><div>" + stars(d.summary.average, 18) + '<div class="meta">' + plural(d.summary.count, "review") + "</div></div></div>";
        else h += '<p class="empty">No reviews yet.</p>';
        h += '<div aria-live="polite">';
        d.reviews.forEach(function (r) { h += reviewHtml(r, true); });
        h += "</div>";
        if (d.next) h += '<nav class="pg"><button type="button" class="btn" data-a="more" data-after="' + esc(d.next) + '">Show more reviews</button></nav>';
        root.innerHTML = h + "</section>";
        host.style.minHeight = "";
      }).catch(function () { root.innerHTML = ""; });
    }
    root.addEventListener("click", function (e) {
      var t = e.target.closest ? e.target.closest("button") : null;
      if (!t) return;
      var ph = t.getAttribute("data-photo"), a = t.getAttribute("data-a");
      if (ph) return lightbox(root, ph, t);
      if (a === "more") load(t.getAttribute("data-after"));
    });
    load();
  }

  function mount(host) {
    if (host.getAttribute("data-dm-rv")) return;
    host.setAttribute("data-dm-rv", "1");
    var kind = kindOf(host);
    host.setAttribute("data-kind", kind);
    // Hold the space until the widget draws, so the page doesn't jump.
    if (!host.style.minHeight) host.style.minHeight = HEIGHTS[kind] + "px";
    var root = host.attachShadow ? host.attachShadow({ mode: "open" }) : host;
    var run = function () { if (kind === "stars") badge(host, root); else if (kind === "carousel") carousel(host, root); else if (kind === "all") allReviews(host, root); else productWidget(host, root); };
    if (kind !== "stars" && window.IntersectionObserver) {
      var io = new IntersectionObserver(function (es) { if (es.some(function (e) { return e.isIntersecting; })) { io.disconnect(); run(); } }, { rootMargin: "400px" });
      io.observe(host);
    } else run();
  }
  function scan() { [].forEach.call(document.querySelectorAll(SLOTS), mount); }
  window.__dumbmailReviews = { scan: scan };
  scan();
  if (window.MutationObserver) {
    var t = 0;
    new MutationObserver(function () { if (t) return; t = setTimeout(function () { t = 0; scan(); }, 200); }).observe(document.documentElement, { childList: true, subtree: true });
  }
})();
