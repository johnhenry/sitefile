// Starts `sitefile lsp` for Sitefile documents. Highlighting comes from the
// TextMate grammar and keeps working whether or not the server runs.

import * as os from 'node:os';
import * as vscode from 'vscode';
import {
  CloseAction,
  ErrorAction,
  LanguageClient,
  type LanguageClientOptions,
  type ServerOptions,
} from 'vscode-languageclient/node';
import {
  CLIENT_ID,
  CLIENT_NAME,
  DOCUMENT_SELECTOR,
  INITIALIZATION_OPTIONS,
  RestartBudget,
  resolveServerCommand,
} from './serverCommand';

let client: LanguageClient | undefined;
let outputChannel: vscode.OutputChannel | undefined;
let warned = false;
const budget = new RestartBudget();

const CONFIGURE = 'Configure path';

function log(message: string): void {
  outputChannel?.appendLine(`[sitefile] ${message}`);
}

/** One non-modal warning per (re)configuration; "Configure path" opens the setting. */
function warnOnce(message: string): void {
  log(message);
  if (warned) return;
  warned = true;
  void vscode.window
    .showWarningMessage(`Sitefile language server: ${message} Highlighting still works.`, CONFIGURE)
    .then((choice) => {
      if (choice === CONFIGURE) {
        void vscode.commands.executeCommand('workbench.action.openSettings', 'sitefile.server.path');
      }
    });
}

async function startClient(): Promise<void> {
  const config = vscode.workspace.getConfiguration('sitefile');
  if (!config.get<boolean>('server.enabled', true)) {
    log('sitefile.server.enabled is false; not starting the language server.');
    return;
  }

  const resolved = resolveServerCommand(
    { path: config.get('server.path'), args: config.get('server.args') },
    {
      PATH: process.env.PATH ?? process.env.Path,
      PATHEXT: process.env.PATHEXT,
      home: os.homedir(),
      workspaceFolder: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
    },
  );
  if (!resolved.ok) {
    warnOnce(
      resolved.reason === 'not-found'
        ? `${resolved.message} Install it with \`cargo install --path rust\` or set sitefile.server.path.`
        : resolved.message,
    );
    return;
  }

  const serverOptions: ServerOptions = {
    // No `transport`: plain stdio, and the args are passed exactly as configured.
    command: resolved.command,
    args: resolved.args,
    options: { cwd: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath },
  };
  const clientOptions: LanguageClientOptions = {
    documentSelector: DOCUMENT_SELECTOR,
    initializationOptions: INITIALIZATION_OPTIONS,
    outputChannel,
    errorHandler: {
      error: (_error, _message, count) =>
        count !== undefined && count <= 3 ? { action: ErrorAction.Continue } : { action: ErrorAction.Shutdown },
      closed: () => {
        if (budget.record(Date.now())) return { action: CloseAction.Restart, handled: true };
        warnOnce('the server stopped repeatedly and will not be restarted. Run "Sitefile: Restart Language Server" to try again.');
        return { action: CloseAction.DoNotRestart, handled: true };
      },
    },
  };

  const next = new LanguageClient(CLIENT_ID, CLIENT_NAME, serverOptions, clientOptions);
  client = next;
  log(`starting ${resolved.command} ${resolved.args.join(' ')}`);
  try {
    await next.start();
  } catch (error) {
    if (client === next) client = undefined;
    warnOnce(`couldn't start '${resolved.command}': ${error instanceof Error ? error.message : String(error)}`);
  }
}

async function stopClient(): Promise<void> {
  const current = client;
  client = undefined;
  if (!current) return;
  try {
    await current.stop();
  } catch (error) {
    log(`stopping the server failed: ${error instanceof Error ? error.message : String(error)}`);
  }
}

async function restart(): Promise<void> {
  warned = false;
  budget.reset();
  await stopClient();
  await startClient();
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  outputChannel = vscode.window.createOutputChannel(CLIENT_NAME);
  context.subscriptions.push(
    outputChannel,
    vscode.commands.registerCommand('sitefile.restartServer', restart),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('sitefile.server')) void restart();
    }),
  );
  await startClient();
}

export function deactivate(): Promise<void> {
  return stopClient();
}
