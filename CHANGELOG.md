# Changelog

Every release has a section here, and the section is the release notes:
the release workflow refuses a tag without one and copies it onto the
GitHub release. The crates are versioned together, so one number is the
version of all of them.

## 0.0.2

Every crate now has its own README, so its page on crates.io says what it is. In 0.0.1 only `zudb` and `zudb-cli` had one, and both showed the repository README. There are no code changes in this release.

- Each `zudb-*` crate has a README covering what it does, how it is put together, and a short example against its real API. Each also says the crate is published for `zudb` to depend on, and that the import keeps the workspace name (`use zu_query` for `zudb-query`).
- `zudb` has a README of its own with the quickstart, databases and connections, statements, transactions, bulk loading, errors, and the storage engines. Its Rust examples are programs in `crates/zu-snippets`, and the snippet test compiles and runs them against the README.
- `zudb-cli` has a README covering the commands of the `zu` binary, the shell, output formats, and exit codes.

## 0.0.1

The first release, and the first time zu is on crates.io. It is early:
the specification in `docs/` is complete and the engine behind it is
not, so this is for trying the API rather than for keeping data in.

- `zudb` on crates.io is the embedded API, `cargo add zudb` and
  `use zudb::Database`. The crate name `zu` is taken there, so every
  published crate is `zudb` or `zudb-*`. The repository, the binary and
  the file extension are still `zu`.
- `zudb-cli` installs the `zu` binary with `cargo install zudb-cli`.
- The engine crates under them are published as `zudb-common`,
  `zudb-encoding`, `zudb-storage`, `zudb-vector`, `zudb-zu1`,
  `zudb-sqlite`, `zudb-s3`, `zudb-query`, `zudb-exec`, `zudb-arrow`,
  `zudb-json` and `zudb-corpus`. They are versioned with `zudb` and pinned
  to it exactly. Depend on `zudb` rather than on them.
- `libzu`, the C library, is attached to the GitHub release for each
  tier 1 platform, with a `SHA256SUMS` and build provenance.
