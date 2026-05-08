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

## Heads-up: `CARGO_TARGET_DIR`

If your shell exports `CARGO_TARGET_DIR` (common with sccache setups —
e.g. `set -gx CARGO_TARGET_DIR "$HOME/.cargo/target"` in fish), Zed
will fail to install the extension with the generic message **"Failed
to install dev extension: failed to compile Rust extension"**.

The reason: Zed runs `cargo build --target wasm32-wasip1 --release` in
this directory and then looks for the artifact at
`<ext>/target/wasm32-wasip1/release/zed_keron_extension.wasm`. With
`CARGO_TARGET_DIR` set, cargo writes the artefact to the global cache
instead and Zed can't find it. There is no in-`Cargo.toml` workaround
because the env always wins over `[build] target-dir`. A `build.rs`
guard in this crate aborts the build with a precise message if it
detects the bad env, so you'll see the cause in `zed: open log`.

Fixes (any one):

```sh
# macOS GUI launch:
CARGO_TARGET_DIR= open -a Zed

# Terminal launch:
CARGO_TARGET_DIR= zed .

# Permanent: remove the `set -gx CARGO_TARGET_DIR …` from your shell
# rc and reopen Zed.
```

## What's wired

- File extension `.keron` → `Keron` language.
- Tree-sitter grammar from `../../tree-sitter-keron`.
- Language server: `keron lsp` (stdio).

## Iterating

When you change the grammar, run `zed: reload extensions` to pick up
the new parser. When you change the LSP, just rebuild the keron CLI;
Zed will respawn the server on the next file open.
