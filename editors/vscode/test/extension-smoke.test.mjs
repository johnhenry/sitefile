// Smoke test of the bundled dist/extension.js with a stub `vscode` module (no
// VS Code download): activation registers the restart command, a missing
// binary gives exactly one non-modal warning whose "Configure path" action
// opens the setting, and `sitefile.server.enabled: false` starts nothing.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import Module, { createRequire } from 'node:module';

const require = createRequire(import.meta.url);

function stubVscode(settings) {
  const calls = { warnings: [], commands: [], registered: [], channels: [] };
  const disposable = { dispose() {} };
  const vscode = {
    window: {
      createOutputChannel: (name) => {
        const ch = { name, lines: [], appendLine: (l) => ch.lines.push(l), dispose() {} };
        calls.channels.push(ch);
        return ch;
      },
      showWarningMessage: (message, ...items) => {
        calls.warnings.push({ message, items });
        return Promise.resolve(items[0]); // the user clicks "Configure path"
      },
    },
    commands: {
      registerCommand: (id, fn) => {
        calls.registered.push([id, fn]);
        return disposable;
      },
      executeCommand: (...args) => {
        calls.commands.push(args);
        return Promise.resolve();
      },
    },
    workspace: {
      workspaceFolders: undefined,
      getConfiguration: () => ({ get: (key, dflt) => (key in settings ? settings[key] : dflt) }),
      onDidChangeConfiguration: () => disposable,
    },
  };
  // vscode-languageclient subclasses vscode types (CompletionItem, …) when it
  // loads; any API not stubbed above is an empty class.
  const classes = new Map();
  const proxy = new Proxy(vscode, {
    get(target, prop) {
      if (prop in target) return target[prop];
      if (!classes.has(prop)) classes.set(prop, class {});
      return classes.get(prop);
    },
  });
  return { vscode: proxy, calls };
}

function loadExtension(vscode) {
  const original = Module._load;
  Module._load = function (request, ...rest) {
    if (request === 'vscode') return vscode;
    return original.call(this, request, ...rest);
  };
  try {
    const file = require.resolve('../dist/extension.js');
    delete require.cache[file];
    return require(file);
  } finally {
    Module._load = original;
  }
}

test('missing binary: one warning with "Configure path", grammar-only mode', async () => {
  const { vscode, calls } = stubVscode({ 'server.path': '/definitely/missing/sitefile' });
  const ext = loadExtension(vscode);
  const context = { subscriptions: [] };
  await ext.activate(context);
  assert.deepEqual(
    calls.registered.map(([id]) => id),
    ['sitefile.restartServer'],
  );
  assert.equal(calls.warnings.length, 1);
  assert.match(calls.warnings[0].message, /not an executable file/);
  assert.deepEqual(calls.warnings[0].items, ['Configure path']);
  await new Promise((r) => setImmediate(r));
  assert.deepEqual(calls.commands, [['workbench.action.openSettings', 'sitefile.server.path']]);

  // A failing restart warns again once (the command resets the warning) ...
  const restart = calls.registered[0][1];
  await restart();
  assert.equal(calls.warnings.length, 2);
  await ext.deactivate();
});

test('not on PATH: the warning says how to install', async () => {
  const saved = process.env.PATH;
  process.env.PATH = '/nonexistent-dir';
  try {
    const { vscode, calls } = stubVscode({});
    const ext = loadExtension(vscode);
    await ext.activate({ subscriptions: [] });
    assert.equal(calls.warnings.length, 1);
    assert.match(calls.warnings[0].message, /'sitefile' was not found on PATH.*cargo install --path rust/);
    await ext.deactivate();
  } finally {
    process.env.PATH = saved;
  }
});

test('sitefile.server.enabled = false starts nothing and warns nothing', async () => {
  const { vscode, calls } = stubVscode({ 'server.enabled': false, 'server.path': '/definitely/missing/sitefile' });
  const ext = loadExtension(vscode);
  await ext.activate({ subscriptions: [] });
  assert.equal(calls.warnings.length, 0);
  assert.match(calls.channels[0].lines.join('\n'), /enabled is false/);
  await ext.deactivate();
});
