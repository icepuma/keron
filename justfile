set shell := ["bash", "-uc"]

default:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo nextest run --workspace --all-features --locked --no-tests=pass

mutants:
    cargo mutants

qualitygate: default mutants
