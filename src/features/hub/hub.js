/* DumbMail customer hub launcher: a "My account" button, links marked data-dumbmail-hub, and DumbMail.openHub(). Loaded by the website snippet. */
(function () {
  "use strict";
  var C = __CONFIG__;
  var waiting = window.__dumbmailHubQ || [];
  var api = { ready: !!C, open: function () {} };
  window.__dumbmailHub = api;
  if (!C) { flush(); return; }

  // The panel works only on the pages the store approved (its website by default), which are the
  // same site as the hub's branded domain, so its sign-in cookie is sent; the hub refuses to be
  // framed anywhere else. Elsewhere it opens in a new tab.
  function sameSite() {
    var o = C.panel_origins || [];
    return o.indexOf(location.origin) >= 0;
  }

  var panel = null, lastFocus = null;
  function close() {
    if (!panel) return;
    panel.parentNode && panel.parentNode.removeChild(panel);
    panel = null;
    document.removeEventListener("keydown", onKey, true);
    if (lastFocus && lastFocus.focus) try { lastFocus.focus(); } catch (e) {}
  }
  function onKey(e) { if (e.key === "Escape") { e.preventDefault(); close(); } }
  function openPanel() {
    if (panel) return;
    lastFocus = document.activeElement;
    panel = document.createElement("div");
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");
    panel.setAttribute("aria-label", C.label);
    panel.style.cssText = "position:fixed;inset:0;z-index:2147483646;background:rgba(17,24,39,.45);display:flex;justify-content:flex-end";
    var box = document.createElement("div");
    box.style.cssText = "position:relative;width:min(460px,100vw);height:100%;background:#f6f6f7;box-shadow:-8px 0 24px rgba(0,0,0,.18)";
    var x = document.createElement("button");
    x.type = "button";
    x.setAttribute("aria-label", "Close");
    x.textContent = "×";
    x.style.cssText = "position:absolute;top:10px;right:10px;z-index:1;width:36px;height:36px;border-radius:999px;border:1px solid #d1d5db;background:#fff;font:22px/1 sans-serif;cursor:pointer;color:#111827";
    x.addEventListener("click", close);
    var f = document.createElement("iframe");
    f.src = C.url + "?embed=1";
    f.title = C.label;
    f.style.cssText = "border:0;width:100%;height:100%;display:block";
    box.appendChild(x);
    box.appendChild(f);
    panel.appendChild(box);
    panel.addEventListener("click", function (e) { if (e.target === panel) close(); });
    document.body.appendChild(panel);
    document.addEventListener("keydown", onKey, true);
    x.focus();
  }
  function open() {
    if (sameSite()) openPanel();
    else window.open(C.url, "_blank", "noopener");
  }
  api.open = open;

  // Any link or button marked data-dumbmail-hub opens the hub.
  document.addEventListener("click", function (e) {
    var t = e.target;
    while (t && t !== document) {
      if (t.hasAttribute && t.hasAttribute("data-dumbmail-hub")) { e.preventDefault(); open(); return; }
      t = t.parentNode;
    }
  }, true);

  if (C.launcher === "button") {
    var draw = function () {
      if (document.getElementById("dumbmail-hub-button")) return;
      var b = document.createElement("button");
      b.id = "dumbmail-hub-button";
      b.type = "button";
      b.textContent = C.label;
      b.style.cssText = "position:fixed;left:16px;bottom:16px;z-index:2147483645;background:" + C.color + ";color:#fff;border:0;border-radius:999px;padding:12px 18px;font:600 15px/1 " + C.font + ";box-shadow:0 4px 14px rgba(0,0,0,.18);cursor:pointer";
      b.addEventListener("click", open);
      document.body.appendChild(b);
    };
    if (document.body) draw(); else document.addEventListener("DOMContentLoaded", draw);
  }
  flush();

  function flush() {
    window.__dumbmailHubQ = { push: function (cb) { try { cb(api); } catch (e) {} } };
    for (var i = 0; i < waiting.length; i++) try { waiting[i](api); } catch (e) {}
  }
})();
