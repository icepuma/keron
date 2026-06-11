# AGENTS.md

Project-wide rules for any agent (human or AI) working in this repo.

## What this is

`keron` is a dotfile + user-package manager driven by a custom
expression language. It describes user-level system state only — no
services, no root, no kernel.

## Toolchain

- **Rust**: pinned exactly via `rust-toolchain.toml` to a single
  numeric version. No `stable`, no ranges. Bump the file when you bump
  the toolchain.
- **Edition**: `2024` everywhere.
- **Resolver**: `3` at the workspace root.
- **Components**: `rustfmt`, `clippy`. Installed by the toolchain file.

## Dependency policy

- **Pin every dependency exactly** with the `=` prefix — e.g.
  `serde = "=1.0.219"`, never `"1.0"` or `"^1.0"`. This includes dev
  and build deps.
- **Upgrade with `cargo-edit`**: use `cargo upgrade --incompatible`
  (from `cargo-edit`) to bump versions. Never hand-edit version numbers
  to upgrade. After upgrading, rerun `just` to verify.
- **Workspace inheritance**: declare deps in `[workspace.dependencies]`
  and reference them per-crate with `dep.workspace = true`. One source
  of truth per dependency.
- **No git/path deps in committed code** unless explicitly justified
  in the same PR.

## Workspace layout

```
crates/
  keron-lang/         lexer, parser, types, eval, embedded stdlib  (lib)
  keron-apply/        plan, execute, sentinels, providers, Tera     (lib)
  keron-cli/          thin orchestrator binary                      (bin)
```

Add new crates under `crates/`. Keep each crate single-responsibility.

## File size & modularization

- **Soft cap: 1000 lines per `.rs` file.** When a file approaches the
  cap, split it. Prefer a folder module (`foo/mod.rs` + siblings) over
  a single bloated file.
- **Group by concern, not by kind**: `parser/expr.rs`,
  `parser/types.rs`, `parser/error.rs` — not one giant `parser.rs`.
- Public re-exports go in the module root; implementation details stay
  private.

## Lints

- Workspace-wide `[workspace.lints]` apply pedantic + nursery + cargo
  groups at warn level. Each crate opts in via
  `[lints] workspace = true`.
- `unsafe_code = "deny"` — no unsafe in production code. Test-only
  carve-outs and platform-FFI call sites for the elevated-rights flow
  may opt in via `#[allow(unsafe_code)]` with a one-line *why* comment.
- CI promotes warnings to errors with `-D warnings`.
- Don't add `#[allow(...)]` without a one-line comment explaining why.

## Commands

Local check:

- **`just`** (the `default` recipe) — fast gate for every change:
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
  3. `cargo nextest run --workspace --all-features --locked`

  Run this before every commit. If `just` is green, syntax/type/test
  invariants hold and the branch is ship-ready for routine changes.

- **`just qualitygate`** — compatibility alias for `default`. It does
  not run mutation testing locally.

Full mutation testing runs in `.github/workflows/mutants.yml` via
manual dispatch and the weekly schedule. For any large language
addition or change — new syntax form, new parser pass, new typing
rule, evaluator change, IR or AST refactor, error-path rework — use
that workflow to expose test-suite gaps. A surviving mutant means the
tests don't actually pin the behavior. Treat surviving mutants as
merge blockers: add a test that kills each one before landing the
change.

The workflow runs:

```bash
env -u CARGO_TARGET_DIR cargo mutants \
  --workspace \
  --all-features \
  --cargo-arg=--locked \
  --test-tool nextest \
  -j2
```

Don't add local recipes for mutation testing; keep it in the workflow.

## Coding conventions

- **No backwards-compat shims**: this is pre-1.0; rename or delete
  freely. Don't leave `// removed` breadcrumbs.
- **No half-finished implementations**: if it doesn't work end-to-end,
  it doesn't land. `todo!()` is fine for genuinely scoped-out follow-up
  work, gated by a tracking task.
- **Errors**: prefer `thiserror` for typed errors in libraries,
  `anyhow` only at the binary boundary.
- **No `unsafe`.** Forbidden by lint.
- **Stdlib namespace growth**: builtins share the single flat namespace
  that user `fn`s live in, and builtins are unshadowable (a user `fn`
  colliding with one is a hard error). So every new builtin is a
  potential source-breaking change for existing manifests. Prefer a
  domain prefix for new additions (`path_*`, `str_*`, `list_*`,
  `map_*`) over squatting a maximally generic bare name (`get`, `with`,
  `len`); bare names are grandfathered, not a pattern to extend.

### Comments

These rules govern **non-doc comments** — line (`// ...`) and block
(`/* ... */`) comments inside function bodies and module bodies.
**Rust doc comments (`///`, `//!`, `#[doc = "..."]`) are exempt** and
should still document every public item, plus any non-obvious private
helper. Doc comments are the API surface; the rules below are about
inline noise.

Default to writing **no inline comments**. Add one only when the WHY
is non-obvious to a future reader who already has the code: a hidden
invariant, a surprising platform constraint, a workaround for a
specific upstream bug, or behavior that would otherwise look like a
typo. If removing the comment wouldn't confuse a reader, the comment
shouldn't exist.

Specifically, **delete or never write**:

- **Restating WHAT the code does.** `// loop over args` above a `for
  arg in args { ... }` is noise — the reader can see the loop.
- **Naming-the-line comments.** `// canonicalize the path` above
  `let canonical = fs::canonicalize(&p)?;`. The identifier already
  says it.
- **Section headers inside short functions.** `// ---------- parse
  ----------` blocks inside a 30-line function. If a function needs
  internal headers, it's two functions.
- **Caller-list comments.** `// used by foo() and bar()`. Rots
  immediately; `cargo` already knows.
- **Provenance comments.** `// added for issue #123`, `// part of the
  template rewrite`. Belongs in the commit message / PR description.
- **TODO without a tracking task.** Either fix it now or open an
  issue and reference it: `// TODO(#42): ...`.
- **Comments that paraphrase a tool's own error.** `// unwrap is
  safe here because the type checker proved it` — say it in a
  `.expect("...")` instead so the panic message carries the rationale.

Write a comment when:

- A constant looks arbitrary but isn't (`// matches the kernel's
  PATH_MAX on Linux`).
- A workaround papers over an upstream bug (`// chumsky #245: ...`).
- A non-local invariant is being relied on (`// safe to skip the
  check because resolve_managed_path canonicalizes`).
- Platform behavior diverges in a way the `cfg` block alone doesn't
  explain (`// FILE_FLAG_OPEN_REPARSE_POINT so we set ownership on
  the link itself, not its target`).

If you find yourself writing a multi-paragraph essay, move it to a
module-level `//!` doc comment instead — that's the right surface for
prose.

## Tests

- Unit tests live next to the code (`#[cfg(test)] mod tests`).
- Integration tests live in `crates/<name>/tests/`.
- Run via `cargo nextest`; never commit code that fails `just`.
- For `keron-lang`, language fixtures live under
  `crates/keron-lang/tests/corpus/` — one `.keron` file per case.
  Snapshots are sidecar `.snap` files generated by `insta`. Regenerate
  with `INSTA_UPDATE=always cargo nextest run -p keron-lang --test corpus`,
  then `cargo insta review`.

### Coverage discipline

Every language addition lands with **enough tests that the Mutants
workflow reports zero missed mutants on the new code**. In
practice that means each new feature gets:

- **Corpus fixtures** — at least one `.keron` per syntactic form, in
  the right stage subdir (`parse/`, `check/`, `errors/parse/`,
  `errors/check/`). Cover the success path and the obvious failure
  modes.
- **Property tests** (`tests/properties.rs`, `proptest`) — for any
  structural law the feature implies (round-trips, invariants over
  arbitrary inputs, compositional rules over many decls).
- **Fuzz coverage** (`tests/fuzz.rs`, `bolero`) — extend the no-panic
  tests if the feature opens new code paths reachable from arbitrary
  bytes.
- **Unit tests** — for edge cases that fixtures can't conveniently
  express (overflow, span correctness, internal helpers).

If a mutant survives the Mutants workflow, the test suite isn't pinning
the behavior — add a test that kills it before merging. Don't disable
mutants or relax the gate.
