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

  function reload() {
    resetServiceWorkers().then(function () { location.reload(); });
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
    source.onmessage = function () {
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
