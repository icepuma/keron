# keron Zed extension

A Zed dev extension that wires up the `keron-lsp` language server and
ships syntax highlighting via the `tree-sitter-keron` grammar in this
same repo.

## Install (dev mode)

1. Build and install the keron CLI so `keron` is on your `PATH`:

   ```sh
   cargo install --path crates/keron-cli
   ```

2. In Zed, run `zed: install dev extension` (Cmd-Shift-P) and pick this
   directory (`editor/zed/`).

3. Open a `*.keron` file. Zed should pick up syntax highlighting and
   spawn `keron lsp` for diagnostics. If not, run `zed: open log` and
   look for "keron" entries.

## What's wired

- File extension `.keron` → `Keron` language.
- Tree-sitter grammar from `../../tree-sitter-keron`.
- Language server: `keron lsp` (stdio).

## Iterating

When you change the grammar, run `zed: reload extensions` to pick up
the new parser. When you change the LSP, just rebuild the keron CLI;
Zed will respawn the server on the next file open.
