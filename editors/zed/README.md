# keron for Zed

Registers the keron language in [Zed](https://zed.dev) with tree-sitter
syntax highlighting (grammar bundled from `tree-sitter-keron/` in this
repository) and connects it to `keron lsp`: diagnostics, hover,
completion, go-to-definition, formatting, outline symbols, and
signature help.

## Requirements

The `keron` binary must be on your `PATH`.

## Install (dev extension)

The extension is not in the Zed registry yet. Install it locally:

1. Open Zed.
2. Run the `zed: install dev extension` action (command palette).
3. Pick this directory (`editors/zed`).

Zed compiles the extension to WebAssembly itself (it will fetch the
`wasm32-wasip1` toolchain if needed), fetches and builds the grammar
declared in `extension.toml`, and starts `keron lsp` the next time you
open a `.keron` file.

## Files

- `extension.toml` — extension, grammar, and language-server
  registration. The `[grammars.keron]` entry points at the
  `tree-sitter-keron` subdirectory of this repository via the `path`
  field; pin its `rev` to a commit SHA when cutting a release.
- `languages/keron/config.toml` — the keron language definition.
- `languages/keron/highlights.scm` — highlight queries (a copy of
  `tree-sitter-keron/queries/highlights.scm`; keep them in sync).
- `src/lib.rs` — spawns `keron lsp` from PATH.
