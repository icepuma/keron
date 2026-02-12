# Keron Manifest Examples

These examples are meant to be run from the repository root.

## Minimal

```bash
cargo run -- apply examples/simple
cargo run -- apply examples/simple
```

## Dependency Graph

```bash
cargo run -- apply examples/dependency
cargo run -- apply examples/dependency
```

`workstation.lua` depends on `base.lua`, so Keron topologically orders them.

## Template Rendering

```bash
cargo run -- apply examples/template
cargo run -- apply examples/template
```

## Proton Pass Template

```bash
cargo run -- apply examples/proton-pass
```

This renders a template with `secret("pp://Personal/test/username")`.
It requires `pass-cli` to be installed and authenticated.
CI e2e coverage uses a mocked `pass-cli` shim (Bash on Unix, cmd on Windows), so no real Proton Pass account is required.

## Package Lists

```bash
cargo run -- apply examples/packages
```

`install_packages("manager", {...}, opts)` expands into multiple package operations. `apply` depends on
the matching package manager being available on your host.

## Complex Multi-Manifest

```bash
cargo run -- apply examples/complex
cargo run -- apply examples/complex
```

This set combines dependency ordering (`base -> dev -> workstation`) with links,
templates, package resources, commands, and `env("PATH")` template variables.

## Invalid Cycle

```bash
cargo run -- apply examples/invalid-cycle
```

This intentionally fails with a dependency-cycle error.

## Note

These examples use `/tmp/...` destinations for simplicity. On non-Unix systems,
update destination paths to valid absolute paths for your OS.
