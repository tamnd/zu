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
A recorded workload of an edge property store, two committed
transactions, a checkpoint fold, and two ingest commits runs against
recording files, and the harness rebuilds the image a crash could
leave at every syscall boundary: the full prefix at each cut, the
prefix with each unsynced write dropped, and the prefix with the final
write torn at several lengths. Every image must recover to a committed
prefix of the workload, and once a commit's WAL sync has returned,
every later cut must recover to that commit or newer.

The edge property store is the one step of that workload with no WAL
frame behind it: it frees the old columns, writes the new ones,
republishes the table index and checkpoints, all inside the data file,
so what floors it is the data file's own sync rather than the log's.
The harness reads every edge of the property carrying table back with
its values at every image, through the ordinal lookup a query uses and
with the endpoints in the recorded state, so a fold that kept the
values and renumbered the edges fails here rather than answering a
later query with another edge's property.

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

## zu1: an older on disk format is refused, never misread

Recovery is only as good as the decision to open. A directory written
by an older build has a different header shape, so believing its
fields would hand recovery a plausible but wrong node count rather
than an error. Every container the zu1 file roots is version
prefixed and the gate fires before any field is read.

Proven by `crates/zu-zu1/src/graph.rs::an_older_directory_version_is_refused`,
which hands the decoder a version 8 group directory, one node count
where version 9 carries a from and a to domain, and requires
`Unsupported { what: "group directory version", id: 8 }` rather than
a decode that succeeds on the wrong offsets. The current bump is
version 9, docs/04 §4; the version history lives in the comment above
`DIRECTORY_VERSION`, one line per bump, and a new bump adds a line
there and moves this row's test to the version it retires.

`crates/zu-zu1/src/graph.rs::hostile_group_count_rejected` covers the
neighbouring case, a header of the right version whose group count is
a lie, which must die on the size check rather than in the allocator.

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
