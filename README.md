# keron

Keron is a Lua-manifest dotfile tool with one main CLI command: `apply`.

## CLI

Install:

```bash
cargo install --path crates/keron
keron --help
```

Main command:

```bash
keron apply <source> [flags]
```

- No `--execute`: dry run (plan only)
- With `--execute`: perform changes

Common flags:

- `--execute`
- `--format text|json`
- `--verbose`
- `--color auto|always|never`

Examples:

```bash
keron apply examples/simple
keron apply examples/simple --execute
keron apply https://github.com/org/repo.git/manifests
```

`<source>` can be a local manifest folder or a public Git source.

## Exit Codes

`keron apply` (dry-run):

- `0`: plan is clean (no drift)
- `2`: plan contains drift (changes, conflicts, or operation errors)
- `1`: planning/evaluation failed

`keron apply --execute`:

- `0`: apply completed successfully
- `1`: apply failed

## Manifest

Keron loads `*.lua` files recursively.

```lua
depends_on("../base.lua")

link("files/zshrc", "/home/me/.zshrc", {
  mkdirs = true,
})

template("files/gitconfig.tmpl", "/home/me/.gitconfig", {
  mkdirs = true,
  force = true,
  vars = {
    user = env("USER"),
    home = global.HOME,
  },
})

install_packages("brew", { "git", "ripgrep" }, {
  state = "present",
})

cmd("echo", { "setup complete" })
```

Manifest functions:

- `depends_on(path)`: manifest ordering dependency (relative path)
- `link(src, dest, opts)`: symlink resource (`dest` must be absolute)
- `template(src, dest, opts)`: render template to absolute destination
- `install_packages(manager, names, opts)`: ensure package state
- `cmd(program, args)`: run command during apply
- `env(name)`, `secret(uri)`, `global.HOME`: value sources
- `is_macos()`, `is_linux()`, `is_windows()`: OS guards

## Examples

```bash
cargo run -- apply examples/simple
cargo run -- apply examples/template
cargo run -- apply examples/packages
```
