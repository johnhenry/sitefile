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

  function reload() {
    resetServiceWorkers().then(function () { location.reload(); });
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
      if (ev.type === "css" && options.cssInject && swapStylesheets(ev.paths || [])) return;
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
