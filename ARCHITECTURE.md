# Architecture

## Crate Boundaries

- `crates/keron`: thin binary entrypoint.
- `crates/keron-cli`: CLI argument parsing, command dispatch, pager/output emission.
- `crates/keron-engine`: discovery, manifest evaluation, dependency graphing, planning, apply logic, provider integration.
- `crates/keron-report`: text/json rendering and sensitive value redaction.
- `crates/keron-domain`: shared data model used across engine/report/CLI boundaries.
- `crates/keron-e2e`: black-box integration tests that execute the `keron` binary.

## Dependency Direction

Allowed dependency edges:

- `keron` -> `keron-cli`
- `keron-cli` -> `keron-engine`, `keron-report`
- `keron-engine` -> `keron-domain`
- `keron-report` -> `keron-domain`
- `keron-e2e` -> no workspace runtime crates (invokes compiled binary)

Disallowed:

- `keron-domain` depending on engine/report/cli crates
- `keron-engine` depending on `keron-report` or `keron-cli`
- `keron-report` depending on `keron-engine` or `keron-cli`

## Security Model

- Manifests are treated as trusted input.
- `cmd` resources execute host commands.
- `env` and `secret` functions can access sensitive host values.
- `apply --execute` performs filesystem mutations.
- `force=true` may overwrite/remove existing paths.
- Running untrusted manifests is out of scope and not supported.
