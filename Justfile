default: run

run:
    cargo run

format:
    cargo fmt --all

lint:
    cargo clippy --tests --all-features --all-targets

test:
    cargo nextest run --all-features --all-targets

check: format lint test