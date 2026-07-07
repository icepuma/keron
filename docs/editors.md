# Editor integration

keron ships a Language Server: `keron lsp` speaks LSP over stdio and
provides diagnostics as you type, hover signatures, completion,
go-to-definition (including across `use` imports), whole-document
formatting (the same engine as `keron format`), outline symbols,
signature help, and semantic-token syntax highlighting.

Any editor with an LSP client can use it — configure the editor to run
`keron lsp` for `.keron` files. The `keron` binary must be installed
and on `PATH`.

Semantic-token highlighting renders in editors that support it
(VS Code, Neovim). Helix and Zed highlight via tree-sitter instead:
the repository ships a grammar in `tree-sitter-keron/`, bundled with
the Zed extension and configurable in Helix (see below).

## Neovim (0.11+)

```lua
vim.filetype.add({ extension = { keron = "keron" } })

vim.lsp.config("keron", {
  cmd = { "keron", "lsp" },
  filetypes = { "keron" },
  root_markers = { ".git" },
})
vim.lsp.enable("keron")
```

Older Neovim (0.9/0.10) can start it manually:

```lua
vim.filetype.add({ extension = { keron = "keron" } })
vim.api.nvim_create_autocmd("FileType", {
  pattern = "keron",
  callback = function()
    vim.lsp.start({ name = "keron", cmd = { "keron", "lsp" } })
  end,
})
```

## Helix

`~/.config/helix/languages.toml`:

```toml
[language-server.keron]
command = "keron"
args = ["lsp"]

[[language]]
name = "keron"
scope = "source.keron"
file-types = ["keron"]
comment-token = "#"
language-servers = ["keron"]

# Prefer pinning `rev` to a commit SHA for reproducible builds.
[[grammar]]
name = "keron"
source = { git = "https://github.com/icepuma/keron", rev = "main", subpath = "tree-sitter-keron" }
```

Fetch and build the grammar:

```sh
hx --grammar fetch
hx --grammar build
```

Helix loads highlight queries from its runtime directory, so copy them
there once (and again after grammar updates):

```sh
mkdir -p ~/.config/helix/runtime/queries/keron
curl -fsSL -o ~/.config/helix/runtime/queries/keron/highlights.scm \
  https://raw.githubusercontent.com/icepuma/keron/main/tree-sitter-keron/queries/highlights.scm
```

(Or copy `tree-sitter-keron/queries/highlights.scm` from a checkout of
this repository.)

## VS Code

Install the bundled extension from `editors/vscode` (not on the
marketplace yet):

```sh
cd editors/vscode
npm install
npm run compile
npx @vscode/vsce package
code --install-extension keron-*.vsix
```

Point `keron.serverPath` at the binary if it is not on VS Code's
`PATH`.

## Zed

Install the bundled dev extension from `editors/zed` (not in the Zed
registry yet): run the `zed: install dev extension` action and pick
the `editors/zed` directory. Zed compiles it to WebAssembly itself and
fetches the bundled tree-sitter grammar for syntax highlighting; see
`editors/zed/README.md` for details.

## Emacs (eglot)

```elisp
;; assuming a keron-mode bound to .keron files
(add-to-list 'eglot-server-programs '(keron-mode "keron" "lsp"))
```

## Kakoune (kakoune-lsp)

```toml
[language_server.keron]
filetypes = ["keron"]
roots = [".git"]
command = "keron"
args = ["lsp"]
```
