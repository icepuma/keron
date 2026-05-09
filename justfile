set shell := ["bash", "-uc"]

default:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo nextest run --workspace --all-features --locked --no-tests=pass

mutants base="origin/main":
    #!/usr/bin/env bash
    set -euo pipefail
    # `env -u CARGO_TARGET_DIR` is load-bearing. cargo-mutants spawns
    # `-j` parallel cargo builds in per-scratch source copies and
    # relies on each scratch's default `./target/` for isolation.
    # If a global `CARGO_TARGET_DIR` is exported (common when using
    # sccache or a shared build cache), every parallel mutant build
    # — and the host's regular builds — collapse into one target
    # directory, corrupting the incremental cache. Symptom: tests
    # that previously passed start failing after `just mutants`
    # because `cargo nextest` reuses a stale rmeta produced from
    # mutated source.
    #
    # `--in-diff` scopes mutation testing to lines changed between
    # the merge base of {{base}} and HEAD — "what this branch added".
    # Pass a different base to compare against another ref, e.g.
    # `just mutants HEAD~1`. Running on a clean main with no branch
    # changes is a no-op (cargo-mutants generates zero mutants).
    diff=$(mktemp -t keron-mutants.XXXXXX.diff)
    trap 'rm -f "$diff"' EXIT
    git diff {{base}}...HEAD > "$diff"
    env -u CARGO_TARGET_DIR cargo mutants --in-diff "$diff" -j4

qualitygate: default mutants
