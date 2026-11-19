/* DumbMail form renderer: draws sign-up forms (popup, flyout, embed, hosted page) and teaser tabs inside a shadow root.
   Shared by the onsite script, the hosted page and the dashboard preview, so all three look the same. ES5 syntax only.
   The whole file is one expression: { mount, teaser, fonts, version }. See docs/FORMS.md for every config key. */
(function () {
  "use strict";

  var FONTS = ["system", "inherit", "serif", "Inter", "Poppins", "Montserrat", "Lato", "DM Sans", "Space Grotesk", "Playfair Display", "Lora", "Merriweather", "Oswald"];
  var GOOGLE = {
    "Inter": "Inter:wght@400;500;600;700;800",
    "Poppins": "Poppins:wght@400;500;600;700;800",
    "Montserrat": "Montserrat:wght@400;500;600;700;800",
    "Lato": "Lato:wght@400;700;900",
    "DM Sans": "DM+Sans:wght@400;500;600;700;800",
    "Space Grotesk": "Space+Grotesk:wght@400;500;600;700",
    "Playfair Display": "Playfair+Display:wght@400;500;600;700;800",
    "Lora": "Lora:wght@400;500;600;700",
    "Merriweather": "Merriweather:wght@400;700;900",
    "Oswald": "Oswald:wght@400;500;600;700"
  };
  var SANS = "-apple-system,BlinkMacSystemFont,\"Segoe UI\",Roboto,\"Helvetica Neue\",Helvetica,Arial,sans-serif";
  var SERIF = "Georgia,Cambria,\"Times New Roman\",Times,serif";
  var SERIF_FONTS = { "Playfair Display": 1, "Lora": 1, "Merriweather": 1 };
  var MONO = "ui-monospace,SFMono-Regular,Menlo,Consolas,\"Liberation Mono\",monospace";
  var Z = 2147483000;
  var GENERIC = "Something went wrong. Please try again.";
  var CONSENT = "By checking this box, you agree to receive recurring automated marketing text messages at this number. Consent is not a condition of purchase. Msg & data rates may apply. Reply STOP to opt out, HELP for help.";
  var EMAIL_RE = /^[^\s@<>()[\]\\,;:"]+@[^\s@<>()[\]\\,;:"]+\.[a-z]{2,}$/i;
  var DEFAULTS = {
    headline: "Get 10% off your first order",
    body: "Join our list for early access to new arrivals and member-only offers.",
    button_text: "Sign up",
    success_message: "Thanks! Check your inbox.",
    phone_headline: "Want texts too?",
    phone_body: "Get early access by text.",
    phone_button: "Text me",
    phone_skip: "No thanks"
  };
  var STD = {
    email: { type: "email", ph: "Email address", ac: "email", im: "email", req: true, need: "Enter your email address." },
    phone_number: { type: "tel", ph: "Mobile number", ac: "tel", im: "tel", need: "Enter your mobile number." },
    first_name: { type: "text", ph: "First name", ac: "given-name", need: "Enter your first name." },
    last_name: { type: "text", ph: "Last name", ac: "family-name", need: "Enter your last name." },
    birthday: { type: "date", ph: "Birthday", ac: "bday", need: "Enter your birthday." }
  };
  var TOP = { email: 1, phone_number: 1, first_name: 1, last_name: 1 };
  // Own keys only: a custom field named "constructor" or "toString" isn't a standard one.
  function own(o, k) { return Object.prototype.hasOwnProperty.call(o, k); }
  var TYPES = ["text", "number", "date", "select", "radio", "checkboxes", "checkbox"];

  // ---------- Small helpers ----------
  function has(o, k) { return Object.prototype.hasOwnProperty.call(o, k); }
  function isObj(x) { return !!x && typeof x === "object" && !Array.isArray(x); }
  function str(v) { return v == null ? "" : String(v); }
  function trim(v) { return str(v).replace(/^\s+|\s+$/g, ""); }
  // The config value, or the default when the key is missing (an empty string stays empty).
  function pick(v, d) { return v == null ? d : str(v); }
  // The config value, or the default when it's missing or blank.
  function filled(v, d) { return trim(v) ? str(v) : d; }
  function oneOf(v, list, d) { return list.indexOf(v) >= 0 ? v : d; }
  function int(v, min, max, d) {
    var n = typeof v === "number" ? v : typeof v === "string" && trim(v) ? Number(v) : NaN;
    if (!isFinite(n)) return d;
    n = Math.round(n);
    return n < min ? min : n > max ? max : n;
  }
  function hex(v, d) { v = trim(v); return /^#(?:[0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(v) ? v : d; }
  function rgb(h) {
    h = h.slice(1);
    if (h.length <= 4) h = h.replace(/./g, function (c) { return c + c; });
    return [parseInt(h.slice(0, 2), 16), parseInt(h.slice(2, 4), 16), parseInt(h.slice(4, 6), 16)];
  }
  function rgba(h, a) { var c = rgb(h); return "rgba(" + c[0] + "," + c[1] + "," + c[2] + "," + a + ")"; }
  function lum(h) {
    var c = rgb(h).map(function (v) { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); });
    return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
  }
  // White or black, whichever reads better on this color.
  function contrast(h) { var l = lum(h); return 1.05 / (l + 0.05) >= (l + 0.05) / 0.05 ? "#ffffff" : "#000000"; }
  // Mix with white: amount 0 = the color, 1 = white.
  function tint(h, amount) {
    return "#" + rgb(h).map(function (v) { var x = Math.round(v + (255 - v) * amount).toString(16); return x.length < 2 ? "0" + x : x; }).join("");
  }
  function safeUrl(v) {
    v = trim(v);
    if (!/^https?:\/\/[^\s]/i.test(v)) return null;
    try { var u = new URL(v); return u.protocol === "http:" || u.protocol === "https:" ? u.href : null; } catch (e) { return null; }
  }
  function humanize(k) { k = str(k).replace(/_+/g, " ").replace(/^\s+|\s+$/g, ""); return k.charAt(0).toUpperCase() + k.slice(1); }
  function clip(s, n) { s = str(s); return s.length > n ? s.slice(0, n - 1) + "…" : s; }

  // Build an element. props: text, cls, on<event> (function), anything else becomes an attribute.
  function h(tag, props, kids) {
    var e = document.createElement(tag);
    if (props) {
      for (var k in props) {
        if (!has(props, k)) continue;
        var v = props[k];
        if (v == null || v === false) continue;
        if (k === "text") e.textContent = str(v);
        else if (k === "cls") e.className = v;
        else if (k.slice(0, 2) === "on" && typeof v === "function") e.addEventListener(k.slice(2), v);
        else e.setAttribute(k, v === true ? "" : str(v));
      }
    }
    add(e, kids);
    return e;
  }
  function add(e, kids) {
    if (kids == null) return e;
    if (!Array.isArray(kids)) kids = [kids];
    for (var i = 0; i < kids.length; i++) {
      var k = kids[i];
      if (k == null || k === false) continue;
      e.appendChild(typeof k === "string" ? document.createTextNode(k) : k);
    }
    return e;
  }
  function clear(e) { while (e && e.firstChild) e.removeChild(e.firstChild); }
  var SVGNS = "http://www.w3.org/2000/svg";
  function icon(d, cls, width) {
    var s = document.createElementNS(SVGNS, "svg");
    s.setAttribute("viewBox", "0 0 24 24");
    s.setAttribute("aria-hidden", "true");
    s.setAttribute("focusable", "false");
    if (cls) s.setAttribute("class", cls);
    var p = document.createElementNS(SVGNS, "path");
    p.setAttribute("d", d);
    p.setAttribute("fill", "none");
    p.setAttribute("stroke", "currentColor");
    p.setAttribute("stroke-width", String(width || 2));
    p.setAttribute("stroke-linecap", "round");
    p.setAttribute("stroke-linejoin", "round");
    s.appendChild(p);
    return s;
  }
  var X_PATH = "M6 6l12 12M18 6L6 18", CHECK_PATH = "M5 12.5l4.5 4.5L19 7.5";

  // legal_text: plain text where [label](https://url) becomes a link. Nothing else is formatted.
  function legal(s) {
    var out = [], re = /\[([^\[\]\n]{1,200})\]\(([^()\s]{1,2000})\)/g, last = 0, m;
    while ((m = re.exec(s))) {
      var u = safeUrl(m[2]);
      if (!u) continue;
      if (m.index > last) out.push(s.slice(last, m.index));
      out.push(h("a", { href: u, target: "_blank", rel: "noopener noreferrer", text: m[1] }));
      last = re.lastIndex;
    }
    if (last < s.length) out.push(s.slice(last));
    return out;
  }

  // Fonts don't load from inside a shadow root, so named fonts get one stylesheet link in the page's <head>.
  function loadFont(name) {
    if (!GOOGLE[name] || !document.head) return;
    var id = "dm-font-" + name.replace(/\s+/g, "-").toLowerCase();
    if (document.getElementById(id)) return;
    var l = document.createElement("link");
    l.id = id;
    l.rel = "stylesheet";
    l.href = "https://fonts.googleapis.com/css2?family=" + GOOGLE[name] + "&display=swap";
    l.setAttribute("data-pl-own", "1");
    document.head.appendChild(l);
  }
  function fontStack(name) {
    if (name === "inherit") return "inherit";
    if (name === "serif") return SERIF;
    if (GOOGLE[name]) return "\"" + name + "\"," + (SERIF_FONTS[name] ? SERIF : SANS);
    return SANS;
  }

  // ---------- Config ----------
  function kindOf(k) { return oneOf(k, ["popup", "flyout", "embed", "page"], "popup"); }
  function styleOf(kind, c) {
    var s = isObj(c.style) ? c.style : {}, inline = kind === "embed" || kind === "page";
    var brand = hex(c.brand_color, "#1f6f5c"), btn = hex(s.button_color, brand);
    var weight = Number(s.headline_weight);
    var font = FONTS.indexOf(s.font) >= 0 ? s.font : "system";
    return {
      layout: oneOf(s.layout, ["stacked", "image_left", "image_right", "background"], "stacked"),
      width: int(s.width, 280, 900, { popup: 420, flyout: 360, embed: 480, page: 440 }[kind]),
      align: oneOf(s.align, ["center", "left"], "center"),
      font: font,
      hs: int(s.headline_size, 14, 56, 24),
      hw: [400, 500, 600, 700, 800].indexOf(weight) >= 0 ? weight : 700,
      bs: int(s.body_size, 12, 22, 15),
      bg: hex(s.background_color, "#ffffff"),
      hc: hex(s.headline_color, "#111827"),
      tc: hex(s.text_color, "#4b5563"),
      btn: btn,
      btnText: hex(s.button_text_color, contrast(btn)),
      full: s.button_full_width !== false,
      ib: hex(s.input_background, "#ffffff"),
      ibc: hex(s.input_border_color, "#d1d5db"),
      itc: hex(s.input_text_color, "#111827"),
      radius: int(s.corner_radius, 0, 40, 14),
      br: int(s.button_radius, 0, 40, 8),
      ir: int(s.input_radius, 0, 40, 8),
      pad: int(s.padding, 12, 64, 28),
      bw: int(s.border_width, 0, 8, inline ? 1 : 0),
      bc: hex(s.border_color, "#e5e7eb"),
      shadow: oneOf(s.shadow, ["none", "soft", "strong"], inline ? "none" : "strong"),
      oc: hex(s.overlay_color, "#111827"),
      oo: int(s.overlay_opacity, 0, 90, 55),
      overlayClose: s.close_on_overlay !== false,
      cc: hex(s.close_color, "#9ca3af"),
      pos: kind === "flyout" ? oneOf(s.position, ["bottom_right", "bottom_left", "top_right", "top_left"], "bottom_right") : oneOf(s.position, ["center", "top", "bottom"], "center"),
      anim: oneOf(s.animation, ["fade", "slide", "zoom", "none"], "fade"),
      pageBg: hex(s.page_background, tint(btn, 0.9)),
      css: typeof s.custom_css === "string" ? s.custom_css.slice(0, 10000) : ""
    };
  }

  function fieldsOf(c) {
    var list = Array.isArray(c.fields) && c.fields.length ? c.fields : null;
    if (!list) {
      var col = Array.isArray(c.collect) && c.collect.length ? c.collect : ["email"];
      list = ["first_name", "email", "phone_number"].filter(function (k) { return col.indexOf(k) >= 0; }).map(function (k) { return { key: k }; });
      if (!list.length) list = [{ key: "email" }];
    }
    var out = [], seen = Object.create(null);
    list.forEach(function (f) {
      if (typeof f === "string") f = { key: f };
      if (!isObj(f)) return;
      var key = str(f.key);
      if (!/^[a-z][a-z0-9_]{0,39}$/.test(key) || seen[key]) return;
      seen[key] = 1;
      var std = own(STD, key) ? STD[key] : null;
      var type = std ? std.type : oneOf(f.type, TYPES, "text");
      var options = (Array.isArray(f.options) ? f.options : []).filter(function (o) { return trim(o) !== ""; }).map(function (o) { return trim(o); }).slice(0, 20);
      if ((type === "select" || type === "radio" || type === "checkboxes") && !options.length) type = "text";
      out.push({
        key: key, type: type, std: std,
        label: clip(trim(f.label), 100),
        placeholder: clip(trim(f.placeholder), 100),
        hint: clip(trim(f.placeholder), 100) || (std ? std.ph : humanize(key)),
        required: typeof f.required === "boolean" ? f.required : !!(std && std.req),
        options: options
      });
    });
    if (!out.length) out.push({ key: "email", type: "email", std: STD.email, label: "", placeholder: "", hint: STD.email.ph, required: true, options: [] });
    return out.slice(0, 12);
  }

  // ---------- CSS ----------
  function vars(st) {
    var shadow = { none: "none", soft: "0 10px 30px rgba(17,24,39,.12),0 2px 6px rgba(17,24,39,.06)", strong: "0 24px 64px rgba(17,24,39,.28),0 4px 14px rgba(17,24,39,.12)" }[st.shadow];
    return ".dm-root{" +
      "--dm-w:" + st.width + "px;--dm-pad:" + st.pad + "px;--dm-r:" + st.radius + "px;--dm-br:" + (st.br >= 40 ? 999 : st.br) + "px;--dm-ir:" + st.ir + "px;" +
      "--dm-hs:" + st.hs + "px;--dm-hw:" + st.hw + ";--dm-bs:" + st.bs + "px;--dm-align:" + st.align + ";--dm-self:" + (st.align === "left" ? "flex-start" : "center") + ";" +
      "--dm-bg:" + st.bg + ";--dm-hc:" + st.hc + ";--dm-tc:" + st.tc + ";--dm-btn:" + st.btn + ";--dm-btn-text:" + st.btnText + ";" +
      "--dm-ring:" + rgba(st.btn, 0.25) + ";--dm-soft:" + rgba(st.btn, 0.1) + ";" +
      "--dm-ib:" + st.ib + ";--dm-ibc:" + st.ibc + ";--dm-itc:" + st.itc + ";--dm-ph:" + rgba(st.itc, 0.45) + ";" +
      "--dm-bw:" + st.bw + "px;--dm-bc:" + st.bc + ";--dm-shadow:" + shadow + ";" +
      "--dm-overlay:" + rgba(st.oc, st.oo / 100) + ";--dm-cc:" + st.cc + ";--dm-page:" + st.pageBg + ";" +
      "font-family:" + fontStack(st.font) + ";}";
  }
  var ARROW = "url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 12 8'%3E%3Cpath d='M1 1.5l5 5 5-5' fill='none' stroke='%236b7280' stroke-width='1.6' stroke-linecap='round' stroke-linejoin='round'/%3E%3C/svg%3E\")";
  var NARROW = ".dm-side{flex-direction:column}.dm-side .dm-media{flex:none;width:100%;height:180px;min-height:0;order:0}.dm-side .dm-content{align-self:stretch}";
  var BASE_CSS = [
    // all:initial keeps the page's inherited styles (letter-spacing, text-transform, colors…) out; font-family comes back in vars().
    ".dm-root{all:initial;display:block;box-sizing:border-box;font-size:var(--dm-bs);line-height:1.5;color:var(--dm-tc);text-align:left;-webkit-font-smoothing:antialiased;-moz-osx-font-smoothing:grayscale;-webkit-text-size-adjust:100%;text-size-adjust:100%}",
    ".dm-root *,.dm-root *::before,.dm-root *::after{box-sizing:border-box}",
    ".dm-root [hidden]{display:none!important}",
    ".dm-floating{position:absolute;top:0;right:0;bottom:0;left:0}",
    ".dm-flyout,.dm-flyout .dm-frame{pointer-events:none}",
    ".dm-overlay{position:absolute;top:0;right:0;bottom:0;left:0;background:var(--dm-overlay)}",
    ".dm-frame{position:absolute;top:0;right:0;bottom:0;left:0;display:flex;align-items:center;justify-content:center;padding:16px}",
    ".dm-pos-top .dm-frame{align-items:flex-start;padding-top:max(16px,6vh)}.dm-pos-bottom .dm-frame{align-items:flex-end;padding-bottom:max(16px,6vh)}",
    ".dm-pos-bottom_right .dm-frame{align-items:flex-end;justify-content:flex-end}.dm-pos-bottom_left .dm-frame{align-items:flex-end;justify-content:flex-start}",
    ".dm-pos-top_right .dm-frame{align-items:flex-start;justify-content:flex-end}.dm-pos-top_left .dm-frame{align-items:flex-start;justify-content:flex-start}",
    ".dm-page{display:flex;align-items:center;justify-content:center;min-height:100vh;padding:40px 16px;background:var(--dm-page)}",
    ".dm-preview.dm-page{min-height:100%}",
    ".dm-card{position:relative;display:flex;flex-direction:column;width:min(var(--dm-w),100%);max-height:100%;margin:0;background:var(--dm-bg);border:var(--dm-bw) solid var(--dm-bc);border-radius:var(--dm-r);box-shadow:var(--dm-shadow);overflow:hidden;pointer-events:auto;text-align:var(--dm-align);container-type:inline-size;outline:none}",
    ".dm-embed .dm-card{width:100%;max-width:var(--dm-w);margin:0 auto;max-height:none}",
    ".dm-page .dm-card{max-height:none}",
    ".dm-inner{position:relative;z-index:1;display:flex;flex-direction:column;flex:1 1 auto;min-height:0;overflow-y:auto;overscroll-behavior:contain;-webkit-overflow-scrolling:touch}",
    ".dm-embed .dm-inner,.dm-page .dm-inner{overflow:visible}",
    ".dm-side{flex-direction:row;align-items:stretch}",
    ".dm-media{position:relative;flex:none;overflow:hidden;margin:0;background:" + "rgba(17,24,39,.04)}",
    ".dm-media img{display:block;width:100%;height:auto;max-height:280px;object-fit:cover;border:0;margin:0}",
    ".dm-side .dm-media{flex:0 0 44%;min-height:220px}",
    ".dm-side .dm-media img,.dm-bglayout .dm-media img{position:absolute;top:0;left:0;width:100%;height:100%;max-height:none}",
    ".dm-image_right .dm-media{order:2}",
    ".dm-bglayout .dm-media{position:absolute;top:0;right:0;bottom:0;left:0;z-index:0}",
    ".dm-content{position:relative;flex:1 1 auto;min-width:0;padding:var(--dm-pad)}",
    ".dm-side .dm-content{margin-top:auto;margin-bottom:auto}", // auto margins center without clipping the top when it overflows
    "@container (max-width: 559px){" + NARROW + "}",
    ".dm-narrow " + NARROW.replace(/\}\./g, "}.dm-narrow ."),
    "@container (max-width: 380px){.dm-content{padding:min(var(--dm-pad),24px)}}",
    ".dm-headline{display:block;margin:0 0 8px;font-family:inherit;font-size:var(--dm-hs);font-weight:var(--dm-hw);font-style:normal;line-height:1.2;letter-spacing:-0.01em;color:var(--dm-hc);white-space:pre-line;overflow-wrap:break-word;text-transform:none;text-wrap:balance}",
    ".dm-x-clear .dm-headline{padding-right:20px}.dm-x-clear.dm-align-center .dm-headline{padding-left:20px}",
    ".dm-body{display:block;margin:0 0 20px;font-size:var(--dm-bs);line-height:1.55;color:var(--dm-tc);white-space:pre-line;overflow-wrap:break-word;text-wrap:pretty}",
    ".dm-headline+.dm-form{margin-top:16px}",
    ".dm-form{display:flex;flex-direction:column;gap:12px;margin:0;padding:0;text-align:left}",
    ".dm-field{display:flex;flex-direction:column;gap:6px;min-width:0;margin:0;padding:0;border:0}",
    ".dm-label{display:block;margin:0;padding:0;font-size:13px;font-weight:500;line-height:1.35;color:var(--dm-tc)}",
    "fieldset.dm-field{display:block}legend.dm-label{margin:0 0 8px}fieldset.dm-field .dm-error{margin-top:6px}",
    ".dm-sr{position:absolute!important;width:1px!important;height:1px!important;padding:0!important;margin:-1px!important;overflow:hidden!important;clip:rect(0,0,0,0)!important;white-space:nowrap!important;border:0!important}",
    ".dm-input{display:block;width:100%;min-height:48px;margin:0;padding:12px 14px;font-family:inherit;font-size:16px;font-weight:400;line-height:1.25;color:var(--dm-itc);background-color:var(--dm-ib);border:1px solid var(--dm-ibc);border-radius:var(--dm-ir);outline:none;box-shadow:none;-webkit-appearance:none;-moz-appearance:none;appearance:none;transition:border-color .15s,box-shadow .15s;text-align:left}",
    ".dm-input::placeholder{color:var(--dm-ph);opacity:1}",
    ".dm-input:focus{border-color:var(--dm-btn);box-shadow:0 0 0 3px var(--dm-ring)}",
    ".dm-input[aria-invalid=true]{border-color:#dc2626}.dm-input[aria-invalid=true]:focus{box-shadow:0 0 0 3px rgba(220,38,38,.2)}",
    "select.dm-input{padding-right:40px;background-image:" + ARROW + ";background-repeat:no-repeat;background-position:right 15px center;background-size:12px 8px;cursor:pointer}",
    "select.dm-input.dm-empty{color:var(--dm-ph)}select.dm-input option{color:#111827}",
    "input[type=date].dm-input{min-height:48px}input[type=date].dm-input::-webkit-date-and-time-value{text-align:left}",
    ".dm-choices{display:flex;flex-direction:column;gap:8px}",
    ".dm-choice{display:flex;align-items:flex-start;gap:10px;margin:0;font-size:15px;line-height:1.4;color:var(--dm-tc);cursor:pointer}",
    ".dm-choice input,.dm-consent input{flex:none;width:18px;height:18px;margin:1px 0 0;accent-color:var(--dm-btn);cursor:pointer}",
    ".dm-consent{display:flex;align-items:flex-start;gap:10px;margin:2px 0 0;font-size:12px;line-height:1.45;color:var(--dm-tc);text-align:left;cursor:pointer}",
    ".dm-consent input{width:16px;height:16px}",
    ".dm-root input[type=checkbox]:focus-visible,.dm-root input[type=radio]:focus-visible{outline:2px solid var(--dm-btn);outline-offset:2px}",
    ".dm-invalid-box input{outline:2px solid #dc2626;outline-offset:1px}",
    ".dm-button{position:relative;display:inline-flex;align-items:center;justify-content:center;gap:8px;align-self:var(--dm-self);min-width:160px;min-height:48px;margin:4px 0 0;padding:13px 24px;font-family:inherit;font-size:16px;font-weight:600;line-height:1.2;letter-spacing:0;text-align:center;text-decoration:none;text-transform:none;color:var(--dm-btn-text);background:var(--dm-btn);border:0;border-radius:var(--dm-br);cursor:pointer;-webkit-appearance:none;appearance:none;transition:filter .15s,transform .05s}",
    ".dm-full .dm-button{align-self:stretch;width:100%}",
    ".dm-button:hover{filter:brightness(.93)}.dm-button:active{transform:translateY(1px)}",
    ".dm-button:focus-visible,.dm-copy:focus-visible{outline:none;box-shadow:0 0 0 2px var(--dm-bg),0 0 0 4px var(--dm-btn)}",
    ".dm-button[disabled]{opacity:.75;cursor:default;filter:none;transform:none}",
    ".dm-spin{width:16px;height:16px;border:2px solid currentColor;border-right-color:transparent;border-radius:50%;animation:dm-spin .7s linear infinite}",
    "@keyframes dm-spin{to{transform:rotate(360deg)}}",
    ".dm-no-thanks{display:block;align-self:var(--dm-self);margin:4px 0 0;padding:4px 6px;font-family:inherit;font-size:14px;line-height:1.4;color:var(--dm-tc);background:none;border:0;border-radius:4px;text-decoration:underline;text-underline-offset:3px;cursor:pointer;opacity:.85}",
    ".dm-no-thanks:hover{opacity:1}.dm-no-thanks:focus-visible{outline:2px solid var(--dm-btn);outline-offset:1px}",
    ".dm-legal{display:block;margin:16px 0 0;font-size:12px;line-height:1.5;color:var(--dm-tc);opacity:.85;white-space:pre-line;overflow-wrap:break-word}",
    ".dm-legal a{color:inherit;text-decoration:underline;text-underline-offset:2px}.dm-legal a:focus-visible{outline:2px solid var(--dm-btn);outline-offset:1px}",
    ".dm-error{display:block;margin:0;font-size:13px;line-height:1.4;color:#b91c1c;text-align:left}.dm-error:empty{display:none}",
    ".dm-form-error{text-align:center}.dm-align-left .dm-form-error{text-align:left}",
    ".dm-close{position:absolute;top:10px;right:10px;z-index:3;display:flex;align-items:center;justify-content:center;width:32px;height:32px;margin:0;padding:0;color:var(--dm-cc);background:transparent;border:0;border-radius:50%;cursor:pointer;transition:background-color .15s}",
    ".dm-close:hover{background-color:rgba(17,24,39,.07)}.dm-close:focus-visible{outline:2px solid var(--dm-btn);outline-offset:1px}",
    ".dm-close svg{display:block;width:18px;height:18px}",
    ".dm-has-media .dm-close{background-color:rgba(255,255,255,.92);box-shadow:0 1px 4px rgba(17,24,39,.18)}.dm-has-media .dm-close:hover{background-color:#fff}",
    ".dm-check{display:flex;align-items:center;justify-content:center;width:48px;height:48px;margin:0 auto 16px;color:var(--dm-btn);background:var(--dm-soft);border-radius:50%}",
    ".dm-align-left .dm-check{margin-left:0}.dm-check svg{width:26px;height:26px}",
    ".dm-success .dm-body{margin-bottom:0}.dm-success .dm-headline{margin-bottom:8px}",
    ".dm-code{display:flex;align-items:center;justify-content:space-between;gap:12px;margin:20px 0 0;padding:8px 8px 8px 16px;background:var(--dm-soft);border:1.5px dashed var(--dm-btn);border-radius:var(--dm-ir);text-align:left}",
    ".dm-code-value{min-width:0;font-family:" + MONO + ";font-size:18px;font-weight:700;letter-spacing:.06em;color:var(--dm-hc);overflow-wrap:anywhere;user-select:all;-webkit-user-select:all}",
    ".dm-copy{flex:none;min-width:76px;margin:0;padding:9px 14px;font-family:inherit;font-size:14px;font-weight:600;line-height:1.2;color:var(--dm-btn-text);background:var(--dm-btn);border:0;border-radius:max(4px,calc(var(--dm-br) * .75));cursor:pointer}",
    ".dm-success .dm-button{margin-top:20px}.dm-success.dm-full-btn .dm-button{width:100%}",
    ".dm-phone .dm-headline{margin-bottom:8px}",
    "@keyframes dm-fade{from{opacity:0}to{opacity:1}}",
    "@keyframes dm-up{from{opacity:0;transform:translateY(24px)}to{opacity:1;transform:none}}",
    "@keyframes dm-down{from{opacity:0;transform:translateY(-24px)}to{opacity:1;transform:none}}",
    "@keyframes dm-zoom{from{opacity:0;transform:scale(.94)}to{opacity:1;transform:none}}",
    ".dm-anim-fade .dm-card{animation:dm-fade .25s ease-out both}",
    ".dm-anim-slide .dm-card{animation:dm-up .35s cubic-bezier(.2,.8,.2,1) both}",
    ".dm-anim-slide.dm-pos-top .dm-card,.dm-anim-slide.dm-pos-top_left .dm-card,.dm-anim-slide.dm-pos-top_right .dm-card{animation-name:dm-down}",
    ".dm-anim-zoom .dm-card{animation:dm-zoom .25s cubic-bezier(.2,.8,.2,1) both}",
    ".dm-animated .dm-overlay,.dm-animated.dm-teaser-root .dm-teaser-wrap{animation:dm-fade .25s ease-out both}",
    ".dm-root.dm-leaving{opacity:0;transition:opacity .16s ease-in}",
    // Teaser
    ".dm-teaser-root{position:absolute;top:0;right:0;bottom:0;left:0;pointer-events:none}",
    ".dm-teaser-wrap{position:absolute;pointer-events:auto}",
    ".dm-tpos-bottom_left .dm-teaser-wrap{left:16px;bottom:16px}.dm-tpos-bottom_right .dm-teaser-wrap{right:16px;bottom:16px}",
    ".dm-tpos-left .dm-teaser-wrap{left:0;top:50%;transform:translateY(-50%)}.dm-tpos-right .dm-teaser-wrap{right:0;top:50%;transform:translateY(-50%)}",
    ".dm-teaser{display:block;max-width:min(280px,calc(100vw - 48px));margin:0;padding:12px 20px;font-family:inherit;font-size:15px;font-weight:600;line-height:1.2;color:var(--dm-btn-text);background:var(--dm-btn);border:0;border-radius:var(--dm-br);box-shadow:0 8px 24px rgba(17,24,39,.22);cursor:pointer;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;transition:filter .15s}",
    ".dm-teaser:hover{filter:brightness(.93)}.dm-teaser:focus-visible{outline:none;box-shadow:0 0 0 2px #fff,0 0 0 4px var(--dm-btn)}",
    ".dm-tpos-left .dm-teaser,.dm-tpos-right .dm-teaser{max-width:none;max-height:min(260px,calc(100vh - 48px));padding:18px 12px;writing-mode:vertical-rl;border-radius:max(6px,min(var(--dm-br),14px)) 0 0 max(6px,min(var(--dm-br),14px))}",
    ".dm-tpos-left .dm-teaser{transform:rotate(180deg)}",
    ".dm-teaser-x{position:absolute;top:-9px;right:-9px;display:flex;align-items:center;justify-content:center;width:22px;height:22px;margin:0;padding:0;color:#374151;background:#fff;border:0;border-radius:50%;box-shadow:0 1px 4px rgba(17,24,39,.3);cursor:pointer}",
    ".dm-tpos-right .dm-teaser-x{right:auto;left:-9px}.dm-tpos-left .dm-teaser-x{right:-9px}",
    ".dm-teaser-x svg{width:12px;height:12px}.dm-teaser-x:focus-visible{outline:2px solid var(--dm-btn);outline-offset:1px}",
    "@media (prefers-reduced-motion: reduce){.dm-root .dm-card,.dm-root .dm-overlay,.dm-root .dm-teaser-wrap{animation:none!important}.dm-root.dm-leaving{transition:none}.dm-button,.dm-input{transition:none}}"
  ].join("\n");

  function styleEl(st) {
    // One <style>: the base CSS, the variables from config.style (after the base's all:initial reset), then custom_css last so it wins.
    var s = document.createElement("style");
    s.textContent = BASE_CSS + "\n" + vars(st) + (st.css ? "\n" + st.css : "");
    return s;
  }

  // The host is a plain element in the page; important inline styles keep page CSS from hiding or moving it.
  function hostStyle(host, mode, passThrough, z) {
    var s = { display: "block", margin: "0", padding: "0", border: "0", "box-sizing": "border-box", visibility: "visible", opacity: "1", transform: "none", filter: "none", float: "none", "min-width": "0", "max-width": "none", "clip-path": "none", background: "transparent", "box-shadow": "none" };
    if (mode === "fixed" || mode === "absolute") {
      s.position = mode; s.top = "0"; s.right = "0"; s.bottom = "0"; s.left = "0"; s.width = "auto"; s.height = "auto";
      s["min-height"] = "0"; s["max-height"] = "none"; s["z-index"] = String(z); s["pointer-events"] = passThrough ? "none" : "auto"; s.overflow = "visible";
    } else {
      s.position = "relative"; s.width = "auto"; s["max-width"] = "100%"; s.height = "auto"; s["pointer-events"] = "auto";
    }
    for (var k in s) if (has(s, k)) host.style.setProperty(k, s[k], "important");
  }
  function makeHost(container, mode, passThrough, z) {
    var host = document.createElement("div");
    host.setAttribute("data-pl-own", "1");
    if (mode === "absolute" && container) {
      try { if (window.getComputedStyle(container).position === "static") container.style.position = "relative"; } catch (e) {}
    }
    hostStyle(host, mode, passThrough, z);
    var root = host.attachShadow ? host.attachShadow({ mode: "open" }) : host;
    return { host: host, root: root };
  }
  function reducedMotion() { try { return !!(window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches); } catch (e) { return false; } }
  var CQ = (function () { try { return !!(window.CSS && CSS.supports && CSS.supports("container-type", "inline-size")); } catch (e) { return false; } })();

  // ---------- mount ----------
  function mount(form, opts) {
    form = isObj(form) ? form : {};
    opts = opts || {};
    var kind = kindOf(form.kind), c = isObj(form.config) ? form.config : {}, st = styleOf(kind, c);
    var floating = kind === "popup" || kind === "flyout", preview = !!opts.preview;
    var container = opts.container && opts.container.nodeType === 1 ? opts.container : null;
    var animate = floating && !preview && st.anim !== "none" && !reducedMotion();
    var fields = fieldsOf(c), hasPhoneField = fields.some(function (f) { return f.key === "phone_number"; });
    var ps = isObj(c.phone_step) ? c.phone_step : {};
    var image = safeUrl(c.image_url);
    var alive = true, closed = false, busy = false, timers = [], docListeners = [], ro = null;
    var screen = null, lastResult = null, prevFocus = null;

    loadFont(st.font);
    var made = makeHost(container, floating ? (container ? "absolute" : "fixed") : "inline", kind === "flyout", Z);
    var host = made.host, shadow = made.root;
    if (kind === "page") {
      host.style.setProperty("min-height", preview ? "100%" : "100vh", "important");
      if (preview) host.style.setProperty("height", "100%", "important");
      if (container && !preview) {
        container.style.minHeight = "100vh";
        container.style.background = st.pageBg;
        if (container === document.body) container.style.margin = "0";
      }
    }

    function later(fn, ms) { var t = setTimeout(function () { if (alive) fn(); }, ms); timers.push(t); return t; }
    function listen(target, ev, fn, cap) { target.addEventListener(ev, fn, cap); docListeners.push([target, ev, fn, cap]); }
    function navigate(url) {
      if (typeof opts.navigate === "function") { try { opts.navigate(url); } catch (e) {} return; }
      if (preview) return;
      try { window.location.href = url; } catch (e) {}
    }

    // Structure: root > [overlay] > frame > card > close + [media] + inner > [media] + content
    var rootCls = ["dm-root", "dm-" + kind, "dm-layout-" + st.layout, "dm-" + st.layout, "dm-align-" + st.align];
    if (floating) rootCls.push("dm-floating", "dm-pos-" + st.pos);
    if (st.full) rootCls.push("dm-full");
    if (preview) rootCls.push("dm-preview");
    if (animate) rootCls.push("dm-animated", "dm-anim-" + st.anim);
    if (st.layout === "background") rootCls.push("dm-bglayout");
    if (image) rootCls.push("dm-has-media");
    if (floating && (!image || st.layout === "image_left")) rootCls.push("dm-x-clear");
    var rootEl = h("div", { cls: rootCls.join(" ") });
    var card = h("div", { cls: "dm-card", tabindex: "-1" });
    var role = kind === "popup" || kind === "flyout" ? "dialog" : "region";
    card.setAttribute("role", role);
    if (kind === "popup") card.setAttribute("aria-modal", "true");
    if (kind === "flyout") card.setAttribute("aria-modal", "false");
    var inner = h("div", { cls: "dm-inner" + (image && (st.layout === "image_left" || st.layout === "image_right") ? " dm-side" : "") });
    var content = h("div", { cls: "dm-content" });
    var media = null;
    if (image) {
      var img = h("img", { src: image, alt: str(c.image_alt).slice(0, 200), loading: "eager", decoding: "async" });
      media = h("div", { cls: "dm-media" }, img);
      img.addEventListener("error", function () {
        if (media && media.parentNode) media.parentNode.removeChild(media);
        rootEl.classList.remove("dm-has-media");
        inner.classList.remove("dm-side");
        if (floating) rootEl.classList.add("dm-x-clear");
        if (st.layout === "background") rootEl.classList.remove("dm-bglayout");
      });
    }
    if (media && st.layout === "background") card.appendChild(media);
    else if (media) inner.appendChild(media);
    inner.appendChild(content);
    if (floating) {
      card.appendChild(h("button", { type: "button", cls: "dm-close", "aria-label": "Close", onclick: function () { close("dismiss"); } }, icon(X_PATH)));
    }
    card.appendChild(inner);

    if (kind === "popup") {
      var overlay = h("div", { cls: "dm-overlay" });
      var frame = h("div", { cls: "dm-frame" }, card);
      if (st.overlayClose) {
        frame.addEventListener("click", function (e) { if (e.target === frame) close("dismiss"); });
      }
      add(rootEl, [overlay, frame]);
    } else if (kind === "flyout") {
      add(rootEl, h("div", { cls: "dm-frame" }, card));
    } else if (kind === "page") {
      rootEl.classList.add("dm-page");
      rootEl.appendChild(card);
    } else {
      rootEl.appendChild(card);
    }
    shadow.appendChild(styleEl(st));
    shadow.appendChild(rootEl);

    // Side layouts stack when the card is narrow; browsers without container queries get a class instead.
    if (!CQ && media && st.layout !== "stacked" && st.layout !== "background") {
      var measure = function () { if (alive) rootEl.classList.toggle("dm-narrow", card.getBoundingClientRect().width < 560); };
      if (window.ResizeObserver) { ro = new ResizeObserver(measure); ro.observe(card); }
      else listen(window, "resize", measure);
      later(measure, 0);
    }

    // ----- Screens -----
    function headline(textValue) {
      if (!trim(textValue)) { card.removeAttribute("aria-labelledby"); card.setAttribute("aria-label", "Sign up"); return null; }
      card.removeAttribute("aria-label");
      card.setAttribute("aria-labelledby", "dm-h");
      return h("h2", { cls: "dm-headline", id: "dm-h", text: textValue });
    }
    function para(cls, v) { return trim(v) ? h("p", { cls: cls, text: str(v) }) : null; }
    function button(label, type) {
      return h("button", { type: type || "submit", cls: "dm-button" }, h("span", { cls: "dm-button-text", text: label }));
    }
    function setBusy(btn, on) {
      busy = on;
      if (!btn) return;
      btn.disabled = on;
      if (on) { btn.setAttribute("aria-busy", "true"); if (!btn.querySelector(".dm-spin")) btn.insertBefore(h("span", { cls: "dm-spin", "aria-hidden": "true" }), btn.firstChild); }
      else { btn.removeAttribute("aria-busy"); var sp = btn.querySelector(".dm-spin"); if (sp) btn.removeChild(sp); }
    }

    function fieldEl(f) {
      var id = "dm-f-" + f.key, eid = id + "-err", input = null, wrap, label = f.label;
      var err = h("p", { cls: "dm-error dm-field-error", id: eid, "aria-live": "polite" });
      var cls = "dm-field dm-field-" + f.key + " dm-type-" + f.type;
      if (f.type === "radio" || f.type === "checkboxes") {
        var box = h("div", { cls: "dm-choices" });
        f.options.forEach(function (o, i) {
          box.appendChild(h("label", { cls: "dm-choice" }, [
            h("input", { type: f.type === "radio" ? "radio" : "checkbox", name: f.key, value: o, id: i ? null : id }),
            h("span", { text: o })
          ]));
        });
        wrap = h("fieldset", { cls: cls, "aria-describedby": eid, "aria-required": f.required ? "true" : null }, [h("legend", { cls: "dm-label", text: label || f.hint }), box, err]);
      } else if (f.type === "checkbox") {
        input = h("input", { type: "checkbox", name: f.key, id: id, value: "1", "aria-describedby": eid, "aria-required": f.required ? "true" : null });
        wrap = h("div", { cls: cls }, [h("label", { cls: "dm-choice" }, [input, h("span", { text: label || f.hint })]), err]);
      } else {
        var showLabel = !!label || f.type === "date";
        var lab = h("label", { cls: "dm-label" + (showLabel ? "" : " dm-sr"), "for": id, text: label || f.hint });
        var common = { cls: "dm-input", id: id, name: f.key, "aria-describedby": eid, "aria-required": f.required ? "true" : null };
        if (f.type === "select") {
          common.cls += " dm-empty";
          input = h("select", common);
          input.appendChild(h("option", { value: "", disabled: true, selected: true, text: f.placeholder || (label ? "Choose…" : f.hint) }));
          f.options.forEach(function (o) { input.appendChild(h("option", { value: o, text: o })); });
          input.addEventListener("change", function () { input.classList.toggle("dm-empty", !input.value); });
        } else {
          common.type = f.type === "number" ? "number" : f.type;
          // With a visible label, the placeholder is only a hint; skip one that just repeats the label.
          var ph = label ? f.placeholder || (f.std && f.std.ph.toLowerCase() !== label.toLowerCase() ? f.std.ph : "") : f.hint;
          if (f.type !== "date" && ph) common.placeholder = ph;
          if (f.std) { common.autocomplete = f.std.ac; if (f.std.im) common.inputmode = f.std.im; }
          if (f.type === "number") common.inputmode = "decimal";
          if (f.key === "email") { common.autocapitalize = "off"; common.spellcheck = "false"; }
          if (f.type === "date" && f.key === "birthday") common.max = new Date().toISOString().slice(0, 10);
          input = h("input", common);
        }
        wrap = h("div", { cls: cls }, [lab, input, err]);
      }
      return { f: f, wrap: wrap, input: input, err: err };
    }
    function consentEl() {
      var cb = h("input", { type: "checkbox", name: "sms_consent", value: "1" });
      return { cb: cb, el: h("label", { cls: "dm-consent" }, [cb, h("span", { text: filled(c.sms_consent_text, CONSENT) })]) };
    }

    function value(r) {
      var f = r.f;
      if (f.type === "radio" || f.type === "checkboxes") {
        var vals = [];
        Array.prototype.forEach.call(r.wrap.querySelectorAll("input"), function (i) { if (i.checked) vals.push(i.value); });
        return f.type === "radio" ? (vals[0] || "") : vals;
      }
      if (f.type === "checkbox") return !!r.input.checked;
      return trim(r.input.value);
    }
    function setErr(r, msg) {
      r.err.textContent = msg || "";
      var targets = r.input ? [r.input] : Array.prototype.slice.call(r.wrap.querySelectorAll("input"));
      targets.forEach(function (t) { if (msg) t.setAttribute("aria-invalid", "true"); else t.removeAttribute("aria-invalid"); });
    }
    function focusEl(el) { if (!el) return; try { el.focus({ preventScroll: preview }); } catch (e) { try { el.focus(); } catch (x) {} } }
    function firstInput(r) { return r.input || r.wrap.querySelector("input"); }

    // Validate and collect. Returns the data to send, or null after showing what's wrong.
    function gather(rows, consent, formErr) {
      var data = {}, extra = {}, bad = null, phoneRow = null;
      formErr.textContent = "";
      rows.forEach(function (r) {
        var f = r.f, v = value(r), msg = "";
        var empty = f.type === "checkbox" ? !v : Array.isArray(v) ? !v.length : v === "";
        if (f.key === "phone_number") phoneRow = r;
        if (empty && f.required) {
          msg = f.std ? f.std.need : f.type === "checkbox" ? "Check this box to continue." : f.type === "checkboxes" ? "Choose at least one option." : (f.type === "select" || f.type === "radio") ? "Choose an option." : "This is required.";
        } else if (!empty && f.key === "email" && !EMAIL_RE.test(v)) msg = "Enter a valid email address.";
        else if (!empty && f.key === "phone_number" && str(v).replace(/\D/g, "").length < 7) msg = "Enter a valid mobile number.";
        else if (!empty && f.type === "number" && !isFinite(Number(v))) msg = "Enter a number.";
        setErr(r, msg);
        if (msg) { if (!bad) bad = r; return; }
        if (f.type !== "checkbox" && empty) return;
        if (own(TOP, f.key)) data[f.key] = v;
        else extra[f.key] = f.type === "number" ? Number(v) : v;
      });
      var agreed = !!(consent && consent.cb.checked);
      if (consent) consent.el.classList.remove("dm-invalid-box");
      if (!bad && phoneRow && data.phone_number && !agreed) {
        setErr(phoneRow, phoneRow.f.required ? "Check the box to agree to texts." : "Check the box to agree to texts, or leave the number blank.");
        phoneRow.input.removeAttribute("aria-invalid");
        consent.el.classList.add("dm-invalid-box");
        focusEl(consent.cb);
        return null;
      }
      if (!bad && !data.email && !data.phone_number) {
        // The server needs one way to reach them.
        var emailRow = rows.filter(function (r) { return r.f.key === "email"; })[0];
        if (emailRow && phoneRow) { formErr.textContent = "Enter your email address or mobile number."; focusEl(firstInput(emailRow)); return null; }
        if (emailRow || phoneRow) { bad = emailRow || phoneRow; setErr(bad, bad.f.std.need); }
      }
      if (bad) { focusEl(firstInput(bad)); return null; }
      data.sms_consent = agreed && !!data.phone_number;
      var any = false;
      for (var k in extra) if (has(extra, k)) any = true;
      if (any) data.fields = extra;
      return data;
    }

    function send(data, btn, formErr, then) {
      if (busy) return;
      setBusy(btn, true);
      formErr.textContent = "";
      var p;
      try {
        p = typeof opts.submit === "function" ? opts.submit(data) : new Promise(function (res) { setTimeout(function () { res({ ok: true }); }, 300); });
      } catch (e) { p = Promise.reject(e); }
      Promise.resolve(p).then(function (r) {
        if (!alive) return;
        setBusy(btn, false);
        if (r && r.ok) { then(r); return; }
        formErr.textContent = r && r.error && trim(r.error.message) ? str(r.error.message) : GENERIC;
      }, function () {
        if (!alive) return;
        setBusy(btn, false);
        formErr.textContent = GENERIC;
      });
    }
    function clearOnInput(formEl, rows, consent) {
      formEl.addEventListener("input", function (e) {
        rows.forEach(function (r) { if (r.wrap.contains(e.target) && r.err.textContent) setErr(r, ""); });
      });
      formEl.addEventListener("change", function (e) {
        rows.forEach(function (r) { if (r.wrap.contains(e.target) && r.err.textContent) setErr(r, ""); });
        if (consent && e.target === consent.cb) consent.el.classList.remove("dm-invalid-box");
      });
    }

    // An optional box naming WhatsApp, when the form has WhatsApp consent wording.
    function whatsappEl() {
      var cb = h("input", { type: "checkbox", name: "whatsapp_consent", value: "1" });
      return { cb: cb, el: h("label", { cls: "dm-consent" }, [cb, h("span", { text: str(c.whatsapp_consent_text) })]) };
    }

    function formScreen() {
      var rows = fields.map(fieldEl), consent = null, wa = null;
      var formEl = h("form", { cls: "dm-form", novalidate: true });
      rows.forEach(function (r) {
        formEl.appendChild(r.wrap);
        if (r.f.key === "phone_number") {
          consent = consentEl();
          r.wrap.insertBefore(consent.el, r.err);
          if (trim(c.whatsapp_consent_text)) { wa = whatsappEl(); r.wrap.insertBefore(wa.el, r.err); }
        }
      });
      var btn = button(filled(c.button_text, DEFAULTS.button_text));
      var formErr = h("p", { cls: "dm-error dm-form-error", role: "alert" });
      add(formEl, [btn, formErr]);
      if (floating && trim(c.no_thanks_text)) {
        formEl.appendChild(h("button", { type: "button", cls: "dm-no-thanks", text: str(c.no_thanks_text), onclick: function () { close("dismiss"); } }));
      }
      clearOnInput(formEl, rows, consent);
      formEl.addEventListener("submit", function (e) {
        e.preventDefault();
        if (busy) return;
        var d = gather(rows, consent, formErr);
        if (!d) return;
        var data = { form_id: form.id, variant: form.version || null };
        for (var k in d) if (has(d, k)) data[k] = d[k];
        if (wa && wa.cb.checked && data.phone_number) data.whatsapp_consent = true;
        // The visitor's time zone lets the server guess their location for new profiles.
        try { data.timezone = Intl.DateTimeFormat().resolvedOptions().timeZone; } catch (e2) {}
        // The browser's language: a hint for which language version of a message they get.
        try { if (navigator.language) data.locale = navigator.language; } catch (e3) {}
        send(data, btn, formErr, function (r) { mainDone(data, r); });
      });
      var legalText = trim(c.legal_text) ? h("p", { cls: "dm-legal" }, legal(str(c.legal_text))) : null;
      return [headline(pick(c.headline, DEFAULTS.headline)), para("dm-body", pick(c.body, DEFAULTS.body)), formEl, legalText];
    }

    function phoneScreen() {
      var f = { key: "phone_number", type: "tel", std: STD.phone_number, label: "", placeholder: "", hint: STD.phone_number.ph, required: true, options: [] };
      var row = fieldEl(f), consent = consentEl();
      row.wrap.insertBefore(consent.el, row.err);
      var formEl = h("form", { cls: "dm-form", novalidate: true }, row.wrap);
      var btn = button(filled(ps.button_text, DEFAULTS.phone_button));
      var formErr = h("p", { cls: "dm-error dm-form-error", role: "alert" });
      add(formEl, [btn, formErr, h("button", { type: "button", cls: "dm-no-thanks dm-skip", text: filled(ps.skip_text, DEFAULTS.phone_skip), onclick: function () { finish(); } })]);
      clearOnInput(formEl, [row], consent);
      formEl.addEventListener("submit", function (e) {
        e.preventDefault();
        if (busy) return;
        var d = gather([row], consent, formErr);
        if (!d) return;
        var data = { form_id: form.id, variant: form.version || null, step: "phone", phone_number: d.phone_number, sms_consent: d.sms_consent };
        // The first screen's token ties the number to the person who just signed up.
        if (lastResult && lastResult.phone_token) data.phone_token = String(lastResult.phone_token);
        send(data, btn, formErr, function () { finish(); });
      });
      return [headline(pick(ps.headline, DEFAULTS.phone_headline)), para("dm-body", pick(ps.body, DEFAULTS.phone_body)), formEl];
    }

    function successScreen() {
      var r = lastResult || { ok: true };
      var confirm = r.confirm === true && trim(r.message);
      var head = confirm ? str(r.message) : filled(c.success_message, trim(r.message) ? str(r.message) : DEFAULTS.success_message);
      if (opts.test) head += " (test)";
      var out = [h("div", { cls: "dm-check", "aria-hidden": "true" }, icon(CHECK_PATH, null, 2.4)), headline(head)];
      if (confirm) return out;
      out.push(para("dm-body", c.success_body));
      // Live forms get the code only in the sign-up response; previews have it in the config.
      var code = trim(r.discount_code) || trim(c.discount_code);
      if (code) {
        var copy = h("button", { type: "button", cls: "dm-copy", text: "Copy", "aria-live": "polite" });
        copy.addEventListener("click", function () {
          copyText(code, function (ok) {
            if (!alive) return;
            copy.textContent = ok ? "Copied" : "Copy";
            if (ok) later(function () { copy.textContent = "Copy"; }, 2000);
          });
        });
        out.push(h("div", { cls: "dm-code" }, [h("span", { cls: "dm-code-value", text: code }), copy]));
      }
      var btnUrl = safeUrl(c.success_button_url);
      if (trim(c.success_button_text) && (btnUrl || floating)) {
        var b = button(str(c.success_button_text), "button");
        b.addEventListener("click", function () { if (btnUrl) navigate(btnUrl); else close("done"); });
        out.push(b);
      }
      return out;
    }
    function shownInteractive() {
      var r = lastResult || {};
      if (r.confirm === true && trim(r.message)) return false;
      return !!(trim(r.discount_code) || trim(c.discount_code) || (trim(c.success_button_text) && (safeUrl(c.success_button_url) || floating)));
    }

    function render(name) {
      if (!alive) return;
      screen = name;
      clear(content);
      var kids = name === "phone_step" ? phoneScreen() : name === "success" ? successScreen() : formScreen();
      add(content, kids);
      content.className = "dm-content dm-screen-" + name + (name === "success" ? " dm-success" : name === "phone_step" ? " dm-phone" : "");
      inner.scrollTop = 0;
    }
    function phoneStepOn() { return ps.enabled === true && !hasPhoneField; }
    function mainDone(data, r) {
      lastResult = r;
      if (typeof opts.onSuccess === "function") { try { opts.onSuccess(data, r); } catch (e) {} }
      if (!alive) return;
      if (phoneStepOn()) { render("phone_step"); focusIn(); }
      else finish();
    }
    function focusIn() {
      if (preview || kind !== "popup") return;
      var first = content.querySelector("input,select,button");
      focusEl(first || card);
    }
    function finish() {
      var redirect = safeUrl(c.redirect_url);
      // A double opt-in sign-up must see "check your inbox", so it never redirects.
      if (lastResult && lastResult.confirm === true) redirect = null;
      if (redirect && (!preview || typeof opts.navigate === "function")) { navigate(redirect); return; }
      render("success");
      if (kind === "popup" && !preview) focusEl(content.querySelector("button") || card);
      var secs = int(c.auto_close_seconds, 0, 60, shownInteractive() ? 0 : 3);
      if (floating && !preview && secs > 0) later(function () { close("done"); }, secs * 1000);
    }

    // ----- Closing -----
    function teardown() {
      alive = false;
      timers.forEach(clearTimeout);
      timers = [];
      docListeners.forEach(function (l) { l[0].removeEventListener(l[1], l[2], l[3]); });
      docListeners = [];
      if (ro) { try { ro.disconnect(); } catch (e) {} ro = null; }
    }
    function removeHost() { if (host.parentNode) host.parentNode.removeChild(host); }
    function restoreFocus() {
      if (prevFocus && prevFocus.focus && document.contains && document.contains(prevFocus)) { try { prevFocus.focus({ preventScroll: true }); } catch (e) {} }
      prevFocus = null;
    }
    function close(reason) {
      if (closed) return;
      closed = true;
      teardown();
      if (kind === "popup") restoreFocus();
      if (animate) { rootEl.classList.add("dm-leaving"); setTimeout(removeHost, 170); }
      else removeHost();
      if (reason && typeof opts.onClose === "function") { try { opts.onClose(reason); } catch (e) {} }
    }
    function destroy() { closed = true; teardown(); removeHost(); }

    // ----- Keys and focus -----
    if (floating) {
      var onKey = function (e) {
        if (e.key === "Escape" || e.key === "Esc") {
          // Inside a field with an open native picker, Escape belongs to the picker.
          close("dismiss");
        }
      };
      if (preview) card.addEventListener("keydown", onKey);
      else listen(document, "keydown", onKey);
    }
    if (kind === "popup" && !preview) {
      // Keep Tab inside the dialog.
      card.addEventListener("keydown", function (e) {
        if (e.key !== "Tab") return;
        var list = Array.prototype.filter.call(card.querySelectorAll("button,[href],input,select,textarea,[tabindex]:not([tabindex='-1'])"), function (x) { return !x.disabled && x.getClientRects().length; });
        if (!list.length) return;
        var active = shadow.activeElement || document.activeElement, firstEl = list[0], lastEl = list[list.length - 1];
        if (e.shiftKey && (active === firstEl || active === card)) { e.preventDefault(); focusEl(lastEl); }
        else if (!e.shiftKey && active === lastEl) { e.preventDefault(); focusEl(firstEl); }
      });
    }

    var startScreen = oneOf(opts.screen, ["form", "phone_step", "success"], "form");
    render(startScreen);
    (container || document.body || document.documentElement).appendChild(host);
    if (kind === "popup" && !preview) {
      prevFocus = document.activeElement;
      later(focusIn, 40);
    }

    return {
      host: host,
      close: function () { close(null); },
      destroy: destroy,
      setScreen: function (name) { if (alive && ["form", "phone_step", "success"].indexOf(name) >= 0) render(name); }
    };
  }

  function copyText(s, done) {
    function fallback() {
      var ok = false;
      try {
        var t = document.createElement("textarea");
        t.value = s;
        t.setAttribute("readonly", "");
        t.style.cssText = "position:fixed;top:-1000px;left:-1000px;opacity:0";
        document.body.appendChild(t);
        t.select();
        ok = document.execCommand("copy");
        document.body.removeChild(t);
      } catch (e) {}
      done(ok);
    }
    try {
      if (navigator.clipboard && navigator.clipboard.writeText) { navigator.clipboard.writeText(s).then(function () { done(true); }, fallback); return; }
    } catch (e) {}
    fallback();
  }

  // ---------- teaser ----------
  function teaser(form, opts) {
    form = isObj(form) ? form : {};
    opts = opts || {};
    var kind = kindOf(form.kind), c = isObj(form.config) ? form.config : {}, st = styleOf(kind, c);
    var t = isObj(c.teaser) ? c.teaser : {};
    var container = opts.container && opts.container.nodeType === 1 ? opts.container : null;
    var preview = !!opts.preview;
    var pos = oneOf(t.position, ["bottom_left", "bottom_right", "left", "right"], "bottom_left");
    var label = clip(trim(t.text) || trim(c.headline) || DEFAULTS.headline, 40);
    loadFont(st.font);
    var made = makeHost(container, container ? "absolute" : "fixed", true, Z - 1);
    var alive = true;
    var cls = "dm-root dm-teaser-root dm-tpos-" + pos + (!preview && !reducedMotion() ? " dm-animated" : "");
    var tab = h("button", { type: "button", cls: "dm-teaser", "aria-haspopup": "dialog", title: label.length >= 40 ? trim(t.text) || trim(c.headline) : null, text: label });
    tab.addEventListener("click", function () { if (alive && typeof opts.onOpen === "function") opts.onOpen(); });
    var x = h("button", { type: "button", cls: "dm-teaser-x", "aria-label": "Dismiss" }, icon(X_PATH, null, 2.6));
    x.addEventListener("click", function (e) { e.stopPropagation(); if (alive && typeof opts.onDismiss === "function") opts.onDismiss(); });
    made.root.appendChild(styleEl(st));
    made.root.appendChild(h("div", { cls: cls }, h("div", { cls: "dm-teaser-wrap" }, [tab, x])));
    (container || document.body || document.documentElement).appendChild(made.host);
    return {
      host: made.host,
      destroy: function () { alive = false; if (made.host.parentNode) made.host.parentNode.removeChild(made.host); }
    };
  }

  return { mount: mount, teaser: teaser, fonts: FONTS.slice(), version: 1 };
})()
