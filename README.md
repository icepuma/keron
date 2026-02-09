# keron

A [keron](https://stargate.fandom.com/wiki/Keron) an energy particle and part of the individual building blocks of [Replicators](https://stargate.fandom.com/wiki/Replicator) in the Stargate universe.

Keron is an opinionated dotfile manager powered by Lua manifests (`*.lua`).

## Status

This repository is now a Rust workspace with multiple crates:

- `keron` binary (`crates/keron`)
- `keron-cli`
- `keron-domain`
- `keron-engine`
- `keron-report`
- `keron-e2e`

Dependency direction is intentionally one-way:

- `keron` -> `keron-cli`
- `keron-cli` -> `keron-engine` + `keron-report`
- `keron-engine` / `keron-report` -> `keron-domain`

See `ARCHITECTURE.md` for crate-boundary rules.

## Commands

```bash
cargo run -- apply /path/to/manifests
```

The command accepts `--format text|json`.

Text output controls:

- `--color auto|always|never` (default: `auto`)
- `--verbose` (show execution order and per-op manifest details)
- `--no-hints` (suppress hint lines in text output)

## Security Model

- Treat manifests as trusted code.
- `cmd(...)` runs host commands.
- `env(name)` and `secret(uri)` can read sensitive host data.
- `--execute` applies filesystem mutations.
- `force=true` may replace and remove existing paths at destinations.
- Do not run untrusted manifests.

## Manifest DSL (Lua)

Manifests are discovered recursively by file extension: `*.lua`.

```lua
depends_on("../base.lua")
link("files/zshrc", "/home/me/.zshrc", { mkdirs = true, force = false })
package("git", { provider = "brew", state = "present" })
packages({ "fd", "ripgrep" }, { provider = "brew", state = "present" })
template("files/starship.toml.tmpl", "/home/me/.config/starship.toml", {
  mkdirs = true,
  force = true,
  vars = { username = "me", shell = "/bin/zsh" }
})
cmd("echo", { "configured" })
cmd("echo", { "user=" .. env("USER") })
```

Notes:

- `depends_on` paths are relative to the manifest file.
- `link` source paths are relative to the manifest file.
- `link` destination must be absolute.
- `template` uses Tera templating with `vars` as context.
- `package` manages package state (`present` by default).
- `packages` is a list form of `package` with shared options.
- `env(name)` reads from process environment.
- Missing `env(...)` variables fail manifest evaluation.

## Examples

See `examples/README.md` for runnable manifest sets, including:

- a minimal manifest (`examples/simple`)
- dependency-ordered manifests (`examples/dependency`)
- template rendering (`examples/template`)
- package lists (`examples/packages`)
- a complex multi-manifest setup (`examples/complex`)
- an intentionally invalid cycle (`examples/invalid-cycle`)
