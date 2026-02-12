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
    cargo nextest run --workspace --all-features --all-targets -E 'not binary(harness_cli)'

test-harness:
    cargo nextest run -p keron-e2e --test harness_cli --success-output final --failure-output immediate-final

check: format-check lint test-nextest
