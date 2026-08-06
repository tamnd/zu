.PHONY: build test lint fmt bench gate

bench:
	cargo bench -p zu-encoding --bench decode

gate:
	ZU_GATE=1 cargo bench -p zu-encoding --bench decode

build:
	cargo build --workspace --all-features

test:
	cargo test --workspace --all-features

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings

fmt:
	cargo fmt --all
