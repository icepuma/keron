# keron

Keron is a dotfile manager powered by Lua manifests (`*.lua`).

## Quick Start

From the repo root:

```bash
cargo run -- apply examples/simple
cargo run -- apply examples/simple --execute
```

Use `apply` without `--execute` for a dry run.

## Apply Sources

`keron apply <source>` accepts either:

- a local manifest directory
- a public Git repository source

Examples:

```bash
cargo run -- apply /path/to/manifests
cargo run -- apply https://github.com/org/repo.git//manifests
cargo run -- apply https://github.com/org/repo.git//manifests?ref=main
```

Notes:

- canonical remote format: `<repo-url>//<manifest-subdir>?ref=<branch-or-tag>`
- remote repos are cloned into a temporary directory and cleaned up after the run
- only public network repos are supported (`https://`, `http://`, `git://`)
- `file://` sources are rejected

## Manifest Basics

Keron discovers manifests recursively by `*.lua`.

```lua
depends_on("../base.lua")

link("files/zshrc", "/home/me/.zshrc", {
  mkdirs = true,
  force = false,
})

packages("brew", { "git", "fd", "ripgrep" }, {
  state = "present",
})

template("files/starship.toml.tmpl", "/home/me/.config/starship.toml", {
  mkdirs = true,
  force = true,
  vars = {
    username = "me",
    shell = "/bin/zsh",
  },
})

cmd("echo", { "configured for " .. env("USER") })
```

## DSL Reference

- `depends_on(path)`
  - declares manifest ordering
  - `path` is relative to the current manifest
- `link(src, dest, opts)`
  - links `src` to `dest`
  - `src` is relative to the manifest
  - `dest` must be absolute
  - common opts: `mkdirs`, `force`
- `template(src, dest, opts)`
  - renders a Tera template file to `dest`
  - use `opts.vars` for template variables
  - common opts: `mkdirs`, `force`
- `packages(manager, names, opts)`
  - installs/removes packages through an explicit package manager (for example `"brew"`)
  - `opts.state` is `"present"` by default
  - singular `package(...)` is not supported
- `cmd(program, args)`
  - runs a command in apply order
- `env(name)`
  - reads an environment variable
  - missing variables fail manifest evaluation
- `secret(uri)`
  - reads secret values through configured secret providers
- `is_macos()`, `is_linux()`, `is_windows()`
  - OS guards for conditional resources

## Output Flags

- `--execute`
- `--format text|json`
- `--color auto|always|never`
- `--verbose`
- `--no-hints`

## Safety

- Treat manifests as trusted code.
- `cmd(...)` executes host commands.
- `env(...)` and `secret(...)` may expose sensitive values.
- `force=true` may overwrite existing paths.

## Examples

See `examples/README.md` for runnable manifest sets:

- `examples/simple`
- `examples/dependency`
- `examples/template`
- `examples/packages`
- `examples/complex`
- `examples/invalid-cycle`
