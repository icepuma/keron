# Repository Guidelines

## Project Structure & Module Organization
This repository is a Rust workspace. The CLI binary lives in `crates/keron` and delegates to `crates/keron-cli`. Core domain types live in `crates/keron-domain`, orchestration and execution logic live in `crates/keron-engine`, and output rendering/redaction live in `crates/keron-report`. Integration tests are in `crates/keron-e2e/tests/e2e.rs`. Runnable sample manifests are under `examples/` (`simple`, `dependency`, `template`, `packages`, `invalid-cycle`). Keep dependency direction one-way: `keron -> keron-cli -> (keron-engine, keron-report) -> keron-domain`.

## Build, Test, and Development Commands
- `cargo run -p keron -- apply <dir>`: show planned operations (dry-run).
- `cargo run -p keron -- apply <dir> --execute`: execute planned changes.
- `just format`: run `cargo fmt --all`.
- `just lint`: run clippy across workspace targets/features.
- `cargo test --workspace`: run workspace tests.
- `just test`: run test suite via `cargo nextest`.
- `just check`: full quality gate (`format`, `lint`, `test`) and required after every change.

## Development Workflow
After every new feature, refactor, or bug fix, run `just check` and resolve all failures before opening a PR. Treat this as mandatory, not optional. If behavior changes, update or add tests in the same change.

## Coding Style & Naming Conventions
Use Rust 2024 idioms and keep code `cargo fmt` clean. Follow workspace lint policy: avoid `unwrap`, `panic!`, `dbg!`, `todo!`, and `unimplemented!` in committed code. Use `snake_case` for files/modules/functions, `PascalCase` for types/traits, and descriptive crate names with the `keron-` prefix for new workspace crates.

## Testing Guidelines
Prefer unit tests close to crate logic and integration coverage in `crates/keron-e2e/tests/e2e.rs`. Use `tempfile::TempDir` for filesystem isolation. Gate platform-specific tests with `#[cfg(unix)]` / `#[cfg(windows)]`. Validate user-visible CLI behavior (exit codes and key output) for new features.

## Commit & Pull Request Guidelines
Keep commit messages short and prefixed by scope, matching existing history (examples: `build: ...`, `ops: ...`, `chore: ...`, `cleanup: ...`). PRs should include: concise summary, affected crates, test evidence (at minimum `just check`), and sample CLI output when behavior changes.

## Security & Configuration Tips
Do not commit secrets in `.lua` files or examples. Use `env(name)` for runtime configuration; missing variables fail manifest evaluation, so document required env vars in PRs or project docs. Treat manifests as trusted code only: they can execute commands and access secrets/environment values.
