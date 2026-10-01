# Releasing

A release is a `v*` tag on `main`. The tag runs `.github/workflows/release.yml`,
and nothing a person does by hand after pushing it is part of the release.

## Cutting one

1. Move `version` in `[workspace.package]` and every `version = "=x.y.z"`
   pin in `[workspace.dependencies]` together. The pins are exact, so a
   pin left behind is a build that fails to resolve rather than a crate
   published against the previous release.
2. `cargo check --workspace` to move `Cargo.lock`, which is committed:
   the upload runs with `--locked`.
3. Add a `## x.y.z` section to `CHANGELOG.md`. It is the release notes,
   and the workflow refuses a tag without one.
4. Merge that, then tag the merge commit and push the tag:

        git tag -a vX.Y.Z -m vX.Y.Z && git push origin vX.Y.Z

## What the tag does

- `verify` checks the tag against the workspace version and the
  changelog, then runs `cargo publish --workspace --locked --dry-run`,
  which builds every crate from its own tarball. Nothing is uploaded
  until this passes.
- `crates-io` runs `scripts/publish-crates.sh`. It publishes every crate
  without `publish = false` in dependency order, skips what the index
  already has, and waits out the rate limits. A crate crates.io has never
  seen goes up at one every ten minutes after a burst of five, so a
  release that adds crates is slow once. A version of a crate it has seen
  goes up at one a minute after a burst of thirty, and a crate can take
  twenty versions a day.
- `publish` creates the GitHub release with the changelog section as its
  notes and attaches the libzu archives with build provenance. It waits
  for crates.io, so a release that exists is one `cargo add zudb` can get.

A rehearsal (`workflow_dispatch`) runs everything but the two uploads.

## When it stops halfway

Re-run the failed jobs. The publish script reads the index on every
attempt and carries on from the first crate that is not up, so a re-run
is how a partial release is finished. If crates.io's daily version limit
is the reason it stopped, the job says so: wait for a slot to age out
before re-running.

## The token

`CARGO_REGISTRY_TOKEN` is a secret of the `crates-io` environment and not
of the repository. Only a `v*` tag can deploy to that environment, so
a branch, a pull request and a rehearsal never see it, and it sits in the
environment of one step. Use a crates.io token scoped to
`publish-new` and `publish-update` on `zudb` and `zudb-*`, with an expiry.
To replace it:

    gh secret set CARGO_REGISTRY_TOKEN --env crates-io -R tamnd/zu < token-file

Once every crate exists on crates.io, trusted publishing (GitHub OIDC)
can replace the token for versions of them. A crate that is new to the
registry still needs a token for its first upload.
