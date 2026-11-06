// End-to-end protocol test: spawn the real Rust binary the way the extension
// does (command + args from resolveServerCommand, plain stdio, the extension's
// documentSelector language id and initializationOptions) and talk to it with
// the same JSON-RPC stack vscode-languageclient uses.
//
// The binary is ../../rust/target/release/sitefile (built with cargo if it is
// missing), or $SITEFILE_LSP_BIN.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import {
  createProtocolConnection,
  StreamMessageReader,
  StreamMessageWriter,
  InitializeRequest,
  InitializedNotification,
  DidOpenTextDocumentNotification,
  DidChangeTextDocumentNotification,
  DidCloseTextDocumentNotification,
  PublishDiagnosticsNotification,
  CompletionRequest,
  HoverRequest,
  ShutdownRequest,
  ExitNotification,
} from 'vscode-languageserver-protocol/node';
import {
  DOCUMENT_SELECTOR,
  INITIALIZATION_OPTIONS,
  LANGUAGE_ID,
  resolveServerCommand,
} from '../dist/serverCommand.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const rustDir = path.resolve(here, '../../../rust');
const defaultBin = path.join(rustDir, 'target/release', process.platform === 'win32' ? 'sitefile.exe' : 'sitefile');
const bin = process.env.SITEFILE_LSP_BIN || defaultBin;

let child;
let conn;
let exitCode;
const diagnostics = [];
let onDiagnostics = () => {};

function nextDiagnostics(uri, pred = () => true) {
  const found = diagnostics.find((d) => d.uri === uri && pred(d));
  if (found) return Promise.resolve(found);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`no diagnostics for ${uri}`)), 10_000);
    onDiagnostics = (d) => {
      if (d.uri === uri && pred(d)) {
        clearTimeout(timer);
        resolve(d);
      }
    };
  });
}

before(() => {
  if (!process.env.SITEFILE_LSP_BIN && !existsSync(bin)) {
    const r = spawnSync('cargo', ['build', '--release'], { cwd: rustDir, stdio: 'inherit' });
    assert.equal(r.status, 0, 'cargo build --release failed');
  }
  // Resolve exactly as the extension does: a path setting plus the default args.
  const resolved = resolveServerCommand({ path: bin }, { workspaceFolder: process.cwd() });
  assert.ok(resolved.ok, resolved.message);
  assert.deepEqual(resolved.args, ['lsp']);
  child = spawn(resolved.command, resolved.args, { stdio: ['pipe', 'pipe', 'inherit'] });
  child.on('exit', (code) => {
    exitCode = code;
  });
  conn = createProtocolConnection(new StreamMessageReader(child.stdout), new StreamMessageWriter(child.stdin));
  conn.onNotification(PublishDiagnosticsNotification.type, (d) => {
    diagnostics.push(d);
    onDiagnostics(d);
  });
  conn.listen();
});

after(() => {
  conn?.dispose();
  if (child && child.exitCode === null) child.kill();
});

const uri = 'file:///tmp/sitefile-vscode-test/Sitefile';
const untitled = 'untitled:Untitled-1';

test('initialize with the extension options', async () => {
  const result = await conn.sendRequest(InitializeRequest.type, {
    processId: process.pid,
    clientInfo: { name: 'Visual Studio Code', version: '1.82.0' },
    rootUri: null,
    workspaceFolders: null,
    initializationOptions: INITIALIZATION_OPTIONS,
    capabilities: {
      general: { positionEncodings: ['utf-16'] },
      textDocument: {
        synchronization: { dynamicRegistration: true, didSave: true, willSave: true },
        publishDiagnostics: { relatedInformation: true, versionSupport: false },
        completion: { completionItem: { snippetSupport: true, documentationFormat: ['markdown', 'plaintext'] } },
        hover: { contentFormat: ['markdown', 'plaintext'] },
      },
      workspace: { configuration: true, workspaceFolders: true },
    },
    trace: 'off',
  });
  assert.equal(result.capabilities.positionEncoding, 'utf-16');
  assert.ok(result.capabilities.completionProvider);
  assert.ok(result.capabilities.hoverProvider);
  await conn.sendNotification(InitializedNotification.type, {});
});

test('didOpen (languageId from the document selector) publishes diagnostics', async () => {
  assert.deepEqual([...new Set(DOCUMENT_SELECTOR.map((s) => s.language))], [LANGUAGE_ID]);
  await conn.sendNotification(DidOpenTextDocumentNotification.type, {
    textDocument: { uri, languageId: LANGUAGE_ID, version: 1, text: 'FROM scratch\nFROB x\n' },
  });
  const d = await nextDiagnostics(uri, (x) => x.diagnostics.length > 0);
  assert.equal(d.diagnostics[0].range.start.line, 1);
  assert.equal(d.diagnostics[0].severity, 1);
  assert.match(String(d.diagnostics[0].code), /^E_/);

  // Fixing the file clears them.
  await conn.sendNotification(DidChangeTextDocumentNotification.type, {
    textDocument: { uri, version: 2 },
    contentChanges: [{ text: 'FROM scratch\nCOPY <<EOF /index.html\nhi\nEOF\n\nEXPORT / AS directory\n' }],
  });
  await nextDiagnostics(uri, (x) => x.diagnostics.length === 0);
});

test('untitled documents work too', async () => {
  await conn.sendNotification(DidOpenTextDocumentNotification.type, {
    textDocument: { uri: untitled, languageId: LANGUAGE_ID, version: 1, text: 'ARG title\nFROM scratch\nEXPORT / AS directory\n' },
  });
  const d = await nextDiagnostics(untitled);
  assert.ok(d.diagnostics.some((x) => x.code === 'E_MISSING_ARG'));
  await conn.sendNotification(DidCloseTextDocumentNotification.type, { textDocument: { uri: untitled } });
});

test('completion and hover', async () => {
  const completion = await conn.sendRequest(CompletionRequest.type, {
    textDocument: { uri },
    position: { line: 4, character: 0 },
  });
  const items = Array.isArray(completion) ? completion : completion.items;
  const labels = items.map((i) => i.label);
  for (const kw of ['COPY', 'RENDER', 'IMPORT', 'EXPORT']) assert.ok(labels.includes(kw), `missing ${kw}: ${labels}`);

  const hover = await conn.sendRequest(HoverRequest.type, { textDocument: { uri }, position: { line: 0, character: 1 } });
  assert.ok(hover && hover.contents, 'hover on FROM');
});

test('shutdown and exit cleanly', async () => {
  await conn.sendRequest(ShutdownRequest.type);
  const exited = new Promise((resolve) => (exitCode !== undefined ? resolve() : child.once('exit', () => resolve())));
  await conn.sendNotification(ExitNotification.type);
  await exited;
  assert.equal(exitCode, 0);
});
