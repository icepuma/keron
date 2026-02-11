default: run

run:
    cargo run -p keron

format:
    cargo fmt --all

format-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic -W clippy::nursery -W clippy::cargo -A clippy::multiple-crate-versions

test:
    cargo test --workspace --all-features

test-nextest:
    cargo nextest run --workspace --all-features --all-targets

check: format-check lint test
