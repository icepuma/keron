// VS Code client for the keron language server. All language smarts
// live in `keron lsp` (the Rust binary); this extension only spawns it
// and wires the LSP channel. Syntax highlighting comes from the
// server's semantic tokens, so no TextMate grammar is bundled.

import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export async function activate(): Promise<void> {
  const command =
    vscode.workspace.getConfiguration("keron").get<string>("serverPath") ??
    "keron";
  const serverOptions: ServerOptions = { command, args: ["lsp"] };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "keron" }],
  };
  client = new LanguageClient(
    "keron",
    "keron language server",
    serverOptions,
    clientOptions,
  );
  await client.start();
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
