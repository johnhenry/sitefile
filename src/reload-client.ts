// The browser side of live reload: served at /__sitefile/reload.js while `serve --watch` runs
// (src/server.ts), and loaded by the tag injected into navigated HTML.
//
// This file is the single source of the script: the Rust server (rust/src/server.rs) compiles it
// in with include_str! and serves the text between the String.raw backticks, so both servers send
// the same bytes. Keep it plain ES2017 with no backticks and no dollar-brace inside.
//
// The literal is a function expression; the server calls it with the watch session's options
// (clientScript in src/server.ts): `RELOAD_CLIENT + "(" + options JSON + ");\n"`.

/** The live-reload client script (JavaScript), a function of the options object. */
export const RELOAD_CLIENT = String.raw`(function (options) {
  "use strict";
  var me = document.currentScript;
  // Relative to this script, so the page can be served under another origin or a base path.
  var base = (me && me.src) || location.href;

  // A service worker that caches the site would keep serving the old build after a reload:
  // unregister this origin's workers and empty its Cache Storage first (--keep-service-workers
  // turns this off).
  function resetServiceWorkers() {
    if (!options.resetServiceWorkers) return Promise.resolve();
    var jobs = [];
    if (navigator.serviceWorker && navigator.serviceWorker.getRegistrations) {
      jobs.push(navigator.serviceWorker.getRegistrations().then(function (regs) {
        return Promise.all(regs.map(function (r) { return r.unregister(); }));
      }));
    }
    if (window.caches && caches.keys) {
      jobs.push(caches.keys().then(function (keys) {
        return Promise.all(keys.map(function (k) { return caches.delete(k); }));
      }));
    }
    return Promise.all(jobs).catch(function () {});
  }

  // Only stylesheets changed: point each <link rel=stylesheet> whose path changed at a fresh URL
  // (the old sheet stays until the new one loads, so nothing flashes). False when no link matched,
  // e.g. a sheet that is only @import-ed: then the page reloads.
  function swapStylesheets(paths) {
    var swapped = false;
    var links = document.querySelectorAll("link[rel~=stylesheet][href]");
    for (var i = 0; i < links.length; i++) {
      var link = links[i];
      var url = new URL(link.getAttribute("href"), location.href);
      if (url.origin !== location.origin) continue;
      var p = decodeURIComponent(url.pathname);
      if (paths.indexOf(p) === -1) continue;
      url.searchParams.set("__sitefile", String(Date.now()));
      var fresh = link.cloneNode(false);
      fresh.href = url.href;
      fresh.onload = fresh.onerror = (function (old) {
        return function () { if (old.parentNode) old.parentNode.removeChild(old); };
      })(link);
      link.parentNode.insertBefore(fresh, link.nextSibling);
      swapped = true;
    }
    return swapped;
  }

  // A full reload keeps the scroll position: it is saved per path in sessionStorage and restored
  // when the reloaded page has loaded.
  var scrollKey = "sitefile:scroll:" + location.pathname;
  function saveScroll() {
    try {
      sessionStorage.setItem(scrollKey, JSON.stringify([window.scrollX, window.scrollY]));
    } catch (_) {}
  }
  function restoreScroll() {
    var saved = null;
    try {
      saved = JSON.parse(sessionStorage.getItem(scrollKey) || "null");
      sessionStorage.removeItem(scrollKey);
    } catch (_) {}
    if (saved && saved.length === 2) window.scrollTo(saved[0], saved[1]);
  }
  if (document.readyState === "complete") restoreScroll();
  else window.addEventListener("load", restoreScroll);

  // --navigate-to-changed: a rebuild that changed exactly one page names it; go there unless
  // this is already that page.
  function reload(path) {
    resetServiceWorkers().then(function () {
      if (path && options.navigateToChanged && path !== location.pathname) return location.assign(path);
      saveScroll();
      location.reload();
    });
  }

  // --reload morph (the default): when only HTML changed, fetch this page again and update the
  // document in place (scroll position, focus and form input stay), unless its scripts changed;
  // anything unexpected falls back to a full reload.
  function isOurs(n) {
    return n.nodeType === 1 && (n.localName === "sitefile-overlay" || n === me);
  }
  function scriptsOf(doc) {
    var out = [];
    var list = doc.querySelectorAll("script");
    for (var i = 0; i < list.length; i++) {
      var s = list[i];
      if (s === me || (s.getAttribute("src") || "").indexOf("/__sitefile/reload.js") !== -1) continue;
      out.push((s.getAttribute("src") || "") + "|" + (s.getAttribute("type") || "") + "|" + s.textContent);
    }
    return out.join("\n");
  }
  function sameKind(a, b) {
    return a.nodeType === b.nodeType && a.nodeName === b.nodeName && (a.nodeType !== 1 || (a.id || "") === (b.id || ""));
  }
  function morphChildren(from, to) {
    var have = Array.prototype.filter.call(from.childNodes, function (n) { return !isOurs(n); });
    var want = Array.prototype.slice.call(to.childNodes);
    var anchor = null;
    for (var k = 0; k < from.childNodes.length; k++) {
      if (isOurs(from.childNodes[k])) { anchor = from.childNodes[k]; break; }
    }
    for (var i = 0; i < want.length; i++) {
      var h = have[i];
      if (!h) from.insertBefore(document.importNode(want[i], true), anchor);
      else if (sameKind(h, want[i])) morphNode(h, want[i]);
      else from.replaceChild(document.importNode(want[i], true), h);
    }
    for (var j = want.length; j < have.length; j++) from.removeChild(have[j]);
  }
  function morphNode(from, to) {
    if (from.nodeType === 3 || from.nodeType === 8) {
      if (from.nodeValue !== to.nodeValue) from.nodeValue = to.nodeValue;
      return;
    }
    if (from.nodeType !== 1 || from.localName === "script") return;
    var i;
    for (i = 0; i < to.attributes.length; i++) {
      var a = to.attributes[i];
      if (from.getAttribute(a.name) !== a.value) from.setAttribute(a.name, a.value);
    }
    for (i = from.attributes.length - 1; i >= 0; i--) {
      if (!to.hasAttribute(from.attributes[i].name)) from.removeAttribute(from.attributes[i].name);
    }
    var tag = from.localName;
    if (from === document.activeElement && (tag === "input" || tag === "textarea" || tag === "select")) return;
    if (tag === "textarea") return; // its text is its value, which the person may have edited
    morphChildren(from, to);
  }
  function morph() {
    return fetch(location.href, { headers: { accept: "text/html" }, cache: "no-store" })
      .then(function (r) {
        if (!r.ok) throw new Error("status " + r.status);
        return r.text();
      })
      .then(function (html) {
        var doc = new DOMParser().parseFromString(html, "text/html");
        if (scriptsOf(doc) !== scriptsOf(document)) throw new Error("scripts changed");
        morphChildren(document.head, doc.head);
        morphChildren(document.body, doc.body);
        if (document.title !== doc.title) document.title = doc.title;
      });
  }
  function update(ev) {
    var going = ev.path && options.navigateToChanged && ev.path !== location.pathname;
    if (!ev.morph || going || options.reload !== "morph") return reload(ev.path);
    resetServiceWorkers()
      .then(morph)
      .catch(function () {
        saveScroll();
        location.reload();
      });
  }

  // Forward errors and warnings (and, with --console, log and info) to the server log, for pages
  // open on a phone or in a browser without devtools.
  var consoleUrl = new URL("console", base).href;
  var sending = false;
  function text(x) {
    if (typeof x === "string") return x;
    if (x instanceof Error) return x.name + ": " + x.message;
    try {
      var j = JSON.stringify(x);
      return j === undefined ? String(x) : j;
    } catch (_) {
      return String(x);
    }
  }
  function forward(level, message, stack, url, line, col) {
    if (sending) return;
    sending = true;
    try {
      var body = { level: level, message: String(message).slice(0, 4000) };
      if (stack) body.stack = String(stack).slice(0, 4000);
      if (url) body.url = String(url);
      if (typeof line === "number") body.line = line;
      if (typeof col === "number") body.col = col;
      fetch(consoleUrl, { method: "POST", body: JSON.stringify(body), keepalive: true }).catch(function () {});
    } catch (_) {
      // never let forwarding break the page
    } finally {
      sending = false;
    }
  }
  window.addEventListener("error", function (e) {
    if (e.error === undefined && !e.message) return; // a resource that failed to load
    forward("error", "Uncaught " + (e.error ? text(e.error) : e.message), e.error && e.error.stack, e.filename, e.lineno, e.colno);
  });
  window.addEventListener("unhandledrejection", function (e) {
    var r = e.reason;
    forward("error", "Unhandled rejection: " + text(r), r && r.stack, location.href);
  });
  var levels = options.console ? ["error", "warn", "log", "info"] : ["error", "warn"];
  levels.forEach(function (level) {
    var original = console[level];
    if (typeof original !== "function") return;
    console[level] = function () {
      var args = Array.prototype.slice.call(arguments);
      var err = args.filter(function (a) { return a instanceof Error; })[0];
      forward(level, args.map(text).join(" "), err && err.stack, location.href);
      return original.apply(this, arguments);
    };
  });

  // The Rebuilding badge and the build-error overlay live in shadow roots, so the site's CSS
  // cannot reach them and theirs cannot leak into the site (--no-overlay turns both off).
  var badge = null;
  var overlay = null;
  function host(css, html) {
    var el = document.createElement("sitefile-overlay");
    var root = el.attachShadow ? el.attachShadow({ mode: "open" }) : el;
    root.innerHTML = "<style>" + css + "</style>" + html;
    (document.body || document.documentElement).appendChild(el);
    return { el: el, root: root };
  }
  function showBadge() {
    if (!options.overlay || badge) return;
    badge = host(
      ":host{all:initial}div{position:fixed;right:12px;bottom:12px;z-index:2147483647;padding:6px 10px;" +
        "border-radius:6px;background:#1f2937;color:#f9fafb;font:12px/1.4 system-ui,sans-serif;opacity:.9}",
      "<div role=\"status\">Rebuilding\u2026</div>"
    );
  }
  function hideBadge() {
    if (badge) badge.el.remove();
    badge = null;
  }
  function showError(ev) {
    hideBadge();
    hideError();
    if (!options.overlay) return;
    overlay = host(
      ":host{all:initial}.back{position:fixed;inset:0;z-index:2147483647;background:rgba(0,0,0,.55);" +
        "display:flex;align-items:flex-start;justify-content:center;padding:8vh 16px;box-sizing:border-box}" +
        ".box{max-width:860px;width:100%;max-height:80vh;overflow:auto;background:#fff;color:#111;" +
        "border-top:6px solid #dc2626;border-radius:8px;padding:20px 24px;font:14px/1.5 system-ui,sans-serif;" +
        "box-shadow:0 10px 40px rgba(0,0,0,.4)}h2{margin:0 0 8px;font-size:16px;color:#b91c1c}" +
        "pre{white-space:pre-wrap;word-break:break-word;font:13px/1.5 ui-monospace,monospace;margin:8px 0}" +
        ".where,.note{color:#555;font-size:13px;overflow-wrap:anywhere}button{float:right;font:inherit;border:0;background:none;" +
        "font-size:20px;line-height:1;cursor:pointer;color:#555}",
      "<div class=\"back\"><div class=\"box\" role=\"alertdialog\" aria-label=\"Build failed\">" +
        "<button type=\"button\" aria-label=\"Dismiss\">\u00d7</button><h2></h2><pre></pre>" +
        "<div class=\"where\"></div><div class=\"note\"></div></div></div>"
    );
    var r = overlay.root;
    r.querySelector("h2").textContent = "Build failed: " + (ev.code || "error");
    r.querySelector("pre").textContent = ev.message || "";
    r.querySelector(".where").textContent = ev.file ? ev.file + (ev.line ? ":" + ev.line + (ev.column ? ":" + ev.column : "") : "") : "";
    r.querySelector(".note").textContent =
      "The page shows the last good build. This clears on the next good build." +
      (navigator.serviceWorker && navigator.serviceWorker.controller ? " A service worker controls this page." : "");
    r.querySelector("button").onclick = hideError;
  }
  function hideError() {
    if (overlay) overlay.el.remove();
    overlay = null;
  }

  // EventSource reconnects by itself after a dropped connection (the server sends retry: 1000),
  // but gives up for good when an answer is not an event stream (a proxy or tunnel error page):
  // then try again, waiting twice as long each time, up to 30 s.
  var delay = 1000;
  function connect() {
    var source = new EventSource(new URL("reload", base).href);
    source.onopen = function () {
      delay = 1000;
    };
    source.onmessage = function (e) {
      var ev;
      try {
        ev = JSON.parse(e.data);
      } catch (_) {
        ev = { type: "reload" };
      }
      if (ev.type === "building") return showBadge();
      if (ev.type === "error") return showError(ev);
      hideBadge();
      hideError();
      if (ev.type === "ok") return;
      if (ev.type === "css" && options.cssInject && swapStylesheets(ev.paths || [])) return;
      if (ev.type === "reload") return update(ev);
      reload();
    };
    source.onerror = function () {
      if (source.readyState !== EventSource.CLOSED) return;
      setTimeout(connect, delay);
      delay = Math.min(delay * 2, 30000);
    };
  }
  connect();
})`;
