# keron for Zed

Registers the keron language in [Zed](https://zed.dev) and connects it
to `keron lsp`: diagnostics, hover, completion, go-to-definition,
formatting, outline symbols, and signature help.

Note: Zed highlights via tree-sitter grammars and does not render LSP
semantic tokens, so `.keron` files stay uncolored until keron grows a
tree-sitter grammar. Every other language feature works.

## Requirements

The `keron` binary must be on your `PATH`.

## Install (dev extension)

The extension is not in the Zed registry yet. Install it locally:

1. Open Zed.
2. Run the `zed: install dev extension` action (command palette).
3. Pick this directory (`editors/zed`).

Zed compiles the extension to WebAssembly itself (it will fetch the
`wasm32-wasip1` toolchain if needed) and starts `keron lsp` the next
time you open a `.keron` file.

## Files

- `extension.toml` — extension + language-server registration.
- `languages/keron/config.toml` — the keron language definition.
- `src/lib.rs` — spawns `keron lsp` from PATH.
