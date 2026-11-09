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

  // --sync: this page reports where it is (its path and fragment, its title and, with
  // --sync=scroll, how far down it is) and follows the navigation (and scrolling) of the other open
  // pages, unless a form field has focus. It never sends query strings, form contents, keys or cookies.
  var sync = options.sync === "nav" || options.sync === "scroll";
  var syncScroll = options.sync === "scroll";
  function stored(k) {
    try {
      return sessionStorage.getItem(k);
    } catch (_) {
      return null;
    }
  }
  function store(k, v) {
    try {
      sessionStorage.setItem(k, v);
    } catch (_) {}
  }
  // One ID per tab, kept across its page loads.
  var clientId = sync ? stored("sitefile:client") || "" : "";
  if (sync && !/^[0-9a-f]{16}$/.test(clientId)) {
    var bytes = new Uint8Array(8);
    if (window.crypto && crypto.getRandomValues) crypto.getRandomValues(bytes);
    else for (var b = 0; b < 8; b++) bytes[b] = Math.floor(Math.random() * 256);
    clientId = Array.prototype.map.call(bytes, function (x) { return (x < 16 ? "0" : "") + x.toString(16); }).join("");
    store("sitefile:client", clientId);
  }
  var atKey = "sitefile:at";
  function where() {
    return (location.pathname + location.hash).slice(0, 2048);
  }
  function scrolled() {
    var el = document.scrollingElement || document.documentElement;
    var w = el.scrollWidth - window.innerWidth;
    var h = el.scrollHeight - window.innerHeight;
    var part = function (at, size) { return size > 0 ? Math.min(10000, Math.max(0, Math.round((at / size) * 10000))) : 0; };
    return [part(window.scrollX, w), part(window.scrollY, h)];
  }
  var clientUrl = new URL("client", base).href;
  function report(nav, withScroll) {
    try {
      var body = { id: clientId, url: where(), title: String(document.title || "").slice(0, 200), nav: nav };
      if (withScroll) body.scroll = scrolled();
      fetch(clientUrl, { method: "POST", body: JSON.stringify(body), keepalive: true }).catch(function () {});
    } catch (_) {
      // never let reporting break the page
    }
  }
  // Once its stream is open, a page reports itself: as a navigation only when this tab showed
  // another page before (not on a reload, a reconnect, a new tab or a page another one led here).
  function arrived() {
    var at = where();
    var before = stored(atKey);
    store(atKey, at);
    report(before !== null && before !== at, syncScroll);
  }
  function moved() {
    var at = where();
    if (stored(atKey) === at) return;
    store(atKey, at);
    report(true, false);
  }
  if (sync) {
    ["pushState", "replaceState"].forEach(function (name) {
      var original = history[name];
      if (typeof original !== "function") return;
      history[name] = function () {
        var r = original.apply(this, arguments);
        moved();
        return r;
      };
    });
    window.addEventListener("popstate", moved);
    window.addEventListener("hashchange", moved);
  }
  function editing() {
    var a = document.activeElement;
    if (!a) return false;
    var t = a.localName;
    return t === "input" || t === "textarea" || t === "select" || a.isContentEditable === true;
  }
  var quietUntil = 0;
  function follow(ev) {
    if (!sync || typeof ev.url !== "string" || ev.url.charAt(0) !== "/" || ev.url.charAt(1) === "/") return;
    var hash = ev.url.indexOf("#");
    var path = hash === -1 ? ev.url : ev.url.slice(0, hash);
    if (ev.type === "navigate") {
      if (ev.url === where() || editing()) return;
      store(atKey, ev.url); // the page this leads to is not a navigation of its own
      if (hash !== -1 && path === location.pathname) location.hash = ev.url.slice(hash);
      else location.assign(ev.url);
      return;
    }
    if (!syncScroll || path !== location.pathname || editing()) return;
    if (typeof ev.x !== "number" || typeof ev.y !== "number") return;
    var el = document.scrollingElement || document.documentElement;
    quietUntil = Date.now() + 500; // the scroll this causes is not reported back
    window.scrollTo((ev.x / 10000) * Math.max(0, el.scrollWidth - window.innerWidth), (ev.y / 10000) * Math.max(0, el.scrollHeight - window.innerHeight));
  }
  if (syncScroll) {
    var scrollTimer = null;
    window.addEventListener(
      "scroll",
      function () {
        if (Date.now() < quietUntil || scrollTimer !== null) return;
        scrollTimer = setTimeout(function () {
          scrollTimer = null;
          if (Date.now() >= quietUntil) report(false, true);
        }, 250);
      },
      { passive: true }
    );
  }

  // EventSource reconnects by itself after a dropped connection (the server sends retry: 1000),
  // but gives up for good when an answer is not an event stream (a proxy or tunnel error page):
  // then try again, waiting twice as long each time, up to 30 s.
  var delay = 1000;
  function connect() {
    var source = new EventSource(new URL(sync ? "reload?client=" + clientId : "reload", base).href);
    source.onopen = function () {
      delay = 1000;
      if (sync) arrived();
    };
    source.onmessage = function (e) {
      var ev;
      try {
        ev = JSON.parse(e.data);
      } catch (_) {
        ev = { type: "reload" };
      }
      if (ev.type === "navigate" || ev.type === "scroll") return follow(ev);
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
