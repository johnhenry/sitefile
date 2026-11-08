// The browser side of live reload: served at /__sitefile/reload.js while `serve --watch` runs
// (src/server.ts), and loaded by the tag injected into navigated HTML.
//
// This file is the single source of the script: the Rust server (rust/src/server.rs) compiles it
// in with include_str! and serves the text between the String.raw backticks, so both servers send
// the same bytes. Keep it plain ES2017 with no backticks and no dollar-brace inside.

/** The live-reload client script (JavaScript). */
export const RELOAD_CLIENT = String.raw`(function () {
  "use strict";
  var me = document.currentScript;
  // Relative to this script, so the page can be served under another origin or a base path.
  var base = (me && me.src) || location.href;
  var source = new EventSource(new URL("reload", base).href);
  source.onmessage = function () {
    location.reload();
  };
})();
`;
