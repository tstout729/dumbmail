/* DumbMail shopping agent bubble: a chat button on the store's pages. Loaded by the website snippet only when the store turned the bubble on. */
(function () {
  "use strict";
  var C = __CONFIG__;
  if (!C || window.__dumbmailAgent) return;
  window.__dumbmailAgent = true;

  // The chat opens in a panel only on the store's own pages (the chat refuses to be framed
  // anywhere else); elsewhere it opens in a new tab.
  function own() { return (C.origins || []).indexOf(location.origin) >= 0; }

  var panel = null, lastFocus = null, btn = null;
  function onKey(e) { if (e.key === "Escape") { e.preventDefault(); close(); } }
  function close() {
    if (!panel) return;
    panel.parentNode && panel.parentNode.removeChild(panel);
    panel = null;
    document.removeEventListener("keydown", onKey, true);
    if (btn) btn.setAttribute("aria-expanded", "false");
    if (lastFocus && lastFocus.focus) try { lastFocus.focus(); } catch (e) {}
  }
  function open() {
    if (!own()) { window.open(C.url, "_blank", "noopener"); return; }
    if (panel) return;
    lastFocus = document.activeElement;
    panel = document.createElement("div");
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-label", C.label);
    panel.style.cssText = "position:fixed;z-index:2147483646;right:16px;bottom:84px;width:min(400px,calc(100vw - 32px));height:min(620px,calc(100vh - 110px));background:#f6f6f7;border-radius:14px;overflow:hidden;box-shadow:0 12px 40px rgba(0,0,0,.25)";
    var x = document.createElement("button");
    x.type = "button";
    x.setAttribute("aria-label", "Close chat");
    x.textContent = "×";
    x.style.cssText = "position:absolute;top:8px;right:8px;z-index:1;width:34px;height:34px;border-radius:999px;border:1px solid #d1d5db;background:#fff;font:20px/1 sans-serif;cursor:pointer;color:#111827";
    x.addEventListener("click", close);
    var f = document.createElement("iframe");
    f.src = C.url;
    f.title = C.label;
    f.style.cssText = "border:0;width:100%;height:100%;display:block";
    panel.appendChild(x);
    panel.appendChild(f);
    document.body.appendChild(panel);
    document.addEventListener("keydown", onKey, true);
    if (btn) btn.setAttribute("aria-expanded", "true");
    x.focus();
  }

  function draw() {
    btn = document.createElement("button");
    btn.type = "button";
    btn.setAttribute("aria-label", C.label);
    btn.setAttribute("aria-expanded", "false");
    btn.innerHTML = '<svg width="26" height="26" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>';
    btn.style.cssText = "position:fixed;z-index:2147483645;right:16px;bottom:16px;width:56px;height:56px;border-radius:999px;border:0;cursor:pointer;color:#fff;display:flex;align-items:center;justify-content:center;box-shadow:0 6px 20px rgba(0,0,0,.2);background:" + C.color;
    btn.addEventListener("click", function () { if (panel) close(); else open(); });
    document.body.appendChild(btn);
  }

  if (document.body) draw(); else document.addEventListener("DOMContentLoaded", draw);
})();
