.PHONY: build test lint fmt bench gate check-artifacts grammar extension

# The ISO/IEC 39075:2024 conditions artifact is checked in, and the
# generated status table is derived from it. This verifies the bytes are
# the ones we derived from; the drift test in zu-common verifies the
# table still matches them.
check-artifacts:
	cd crates/zu-common/artifacts && shasum -a 256 -c SHA256SUMS

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

# The word list against the three things that colour a statement, and
# the website's grammar against the conformance corpus. The second half
# wants node, which is why it is a target of its own rather than part of
# lint. The editor's grammar is tamnd/tree-sitter-gql, and the first
# half covers it when a checkout of it is beside this one or named by
# $ZU_TREE_SITTER.
grammar:
	cargo run -p xtask -- grammar --check
	cd grammar && npm ci && npm test

# The VS Code extension. What it decides before it talks to anything is
# a command line, and that is what these tests check, under plain node
# with no editor and no install, which is why they need nothing fetched
# first. Packaging a .vsix is the release train's job and not this one.
extension:
	cd editors/vscode && npm test
