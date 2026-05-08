# tree-sitter-keron

Tree-sitter grammar for the [keron] configuration language.

## Build

```sh
npm install
npm run build   # tree-sitter generate -> src/parser.c
npm test
```

`tree-sitter generate` produces `src/parser.c` plus the binding
shims; the keron repo does not commit the generated artefacts. Each
consumer (Zed, Neovim, Helix) regenerates as part of its grammar
build pipeline.

[keron]: https://github.com/icepuma/keron
