.PHONY: build test lint fmt

build:
	cargo build --workspace --all-features

test:
	cargo test --workspace --all-features

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings

fmt:
	cargo fmt --all
