# Recovery matrix audit

The docs/08 §7 recovery matrix names one crash recovery path per
engine and one invariant over all of them. This file maps every row
to the test that proves it, so the milestone 3 exit gate is checkable
by reading rather than trusting. When a recovery path changes, the
test named here is the one that must move with it.

## The invariant

Any prefix of physical writes yields either the pre-txn or the
post-txn state, never a hybrid.

Proven by `crates/zu-zu1/tests/crash.rs::every_cut_recovers_to_a_committed_prefix`.
A recorded workload of two committed transactions, a checkpoint fold,
and two ingest commits runs against recording files, and the harness
rebuilds the image a crash could leave at every syscall boundary: the
full prefix at each cut, the prefix with each unsynced write dropped,
and the prefix with the final write torn at several lengths. Every
image must recover to a committed prefix of the workload, and once a
commit's WAL sync has returned, every later cut must recover to that
commit or newer.

## zu1: pick valid max-epoch header, replay WAL tail, open

Proven by `crates/zu-zu1/tests/recovery.rs`:

- `reopen_picks_the_newest_header_and_replays_the_tail` runs the
  matrix row end to end, committed state on one header, newer commits
  in the WAL tail, and checks recovery composes across a checkpoint
  fold.
- `recovery_reads_headers_and_meta_not_data_blocks` pins the cost
  claim, ms-scale with no scan of data blocks, by counting every byte
  recovery reads through a pass-through VFS and asserting data blocks
  are never touched, so the bound stays flat as the database grows.

The epoch accounting recovery trusts is separately verified under
loom in `crates/zu-zu1/tests/loom.rs`, which model-checks the
snapshot pin against advance and horizon.

## sqlite: SQLite WAL recovery (theirs, proven)

The row delegates to SQLite, so the test proves the delegation, not
the algorithm. `crates/zu-sqlite/tests/recovery.rs::a_hot_wal_replays_on_open`
builds a real crash image, the database file plus a hot WAL copied
out from under a live connection that never closes or checkpoints,
with a committed transaction still in the log and an open transaction
in flight. Opening the image replays the log: every committed row is
visible, the open transaction leaves nothing, and a control copy
without the WAL contains none of the data, so the answer demonstrably
came from replay. `crates/zu-sqlite/tests/profile.rs::checkpoint_truncates_the_wal`
covers the other half of the lifecycle, that our checkpoint really
empties the log.

## s3: read CURRENT, manifest, replay WAL objects past the floor

The s3 engine lands with milestone 5 (issue #6); this row is audited
there, with the same standard: a real object-store crash image and a
named test per claim.
