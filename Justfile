default: run

run:
    cargo run -p keron

format:
    cargo fmt --all

lint:
    cargo clippy --workspace --tests --all-features --all-targets

test:
    cargo nextest run --workspace --all-features --all-targets

check: format lint test
