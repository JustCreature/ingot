setup-local:
	brew install jpeg-turbo cmake nasm

ci:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo build --release --locked
	cargo test --workspace --locked

lint-check:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace -- --show-output

build:
	cargo build --release

run:
	cargo run --release

run_bench_enrich:
	cargo run --release -p ingot_core --example bench_enrich

run_bench_preview:
	cargo run --release -p ingot_core --example bench_preview
