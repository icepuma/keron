# keron for VS Code

Language support for [keron](https://github.com/icepuma/keron):
diagnostics as you type, hover signatures, completion,
go-to-definition (including across `use` imports), document formatting
(`keron format`), outline symbols, signature help, and semantic-token
syntax highlighting.

The extension is a thin client — it spawns `keron lsp` and everything
else happens in the keron binary.

## Requirements

The `keron` binary must be installed and on your `PATH` (or point
`keron.serverPath` at it in the settings).

## Install (sideload)

The extension is not on the marketplace yet. Build and install a vsix:

```sh
cd editors/vscode
npm ci
npm run compile
npx @vscode/vsce package
code --install-extension keron-*.vsix
```

## Settings

- `keron.serverPath` — machine-scoped path to the `keron` binary
  (default: `keron` from `PATH`). The extension runs
  `<serverPath> lsp`. Workspace settings cannot override this
  executable path, and the extension stays disabled in untrusted
  workspaces.

## Publishing to the marketplace (maintainer)

Needs a one-time [publisher](https://marketplace.visualstudio.com/manage)
named `icepuma` (matching `publisher` in package.json) and an Azure
DevOps PAT with the *Marketplace → Manage* scope:

```sh
cd editors/vscode
npm ci && npm run compile
npx @vscode/vsce login icepuma   # paste the PAT once
npx @vscode/vsce publish         # publishes the version in package.json
```

Also worth publishing to [open-vsx.org](https://open-vsx.org) for
VSCodium/Cursor users: `npx ovsx publish -p <open-vsx-token>`.
