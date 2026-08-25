/* libzu2: the C surface over the zu2 storage engine.
 *
 * zu2 is a storage engine and not a database with a query language, so
 * this is a storage surface: keys and values, nodes and edges, and
 * the traversals a host would otherwise write as a loop over hops. A
 * host that wants GQL wants libzu (crates/zu-capi), which is the other
 * engine and the other header.
 *
 * The object model is the Rust one. A zu2_db is an open file and the
 * structures over it, and it is shareable: every session opened from
 * one db reads the same log and the same adjacency. A zu2_session is
 * the state that cannot be shared, which here is a durability setting
 * and the buffers a call answers out of. A host that traverses from
 * four threads opens one db and opens four sessions on it.
 *
 * A session may move between threads but must not be in two calls at
 * once. A call that finds one already in use answers
 * ZU2_MISUSE_CONCURRENT rather than handing back a buffer another
 * thread is filling. The same rule applies to the fallible db calls
 * (sync, compact, disk_bytes), because they carry a last-error the same
 * way; the accessors that cannot fail do not take that guard and may be
 * called from anywhere.
 *
 * Every fallible call returns a zu2_status and writes what it produced
 * through an out-parameter. The status is the whole control-flow
 * answer, and the out-parameter is written on every path, zeroed or
 * NULL when there is nothing to point at, so a caller who ignores the
 * status is never left holding a pointer from the call before. What a
 * user reads comes from zu2_db_error or zu2_session_error, on the
 * handle the failed call was made on, and stays there until the next
 * call on that handle.
 *
 * The status values match libzu's for the four cases both libraries
 * have, so a host that links both does not need two tables.
 *
 * Buffers this API hands out (a value, a neighbour list, a frontier)
 * belong to the session that produced them and are valid exactly until
 * the next call on that session. Copy before the next call or do not
 * keep them.
 *
 * Those buffers are copies rather than windows into the storage, and
 * that is not an oversight. zu2 holds a neighbour list still by
 * announcing an epoch, and the epoch ends when the call returns, so a
 * pointer that outlived the call would be a pointer into a block a
 * writer is free to replace. This is the reason zu2_khop, zu2_reach and
 * zu2_triangles are here at all: a host that walks a graph one
 * zu2_neighbours call per node pays a copy per hop and measures the
 * copy, and a host that asks for the k-hop frontier pays one copy for
 * the answer and walks the interior at Rust speed inside the epoch.
 *
 * Strings cross the boundary as a pointer and a length. Most source
 * languages have counted strings, and a NUL-terminated parameter makes
 * every one of them measure a string that already knew its own length.
 * A NULL pointer with a zero length is the empty string, not an error.
 *
 * Ownership is the usual C contract: a handle stays valid until the
 * matching close, and each close is a no-op on NULL.
 */
#ifndef ZU2_H
#define ZU2_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* What a call did. The numbers are libzu's, so the two headers agree
 * where they overlap and the gaps are libzu's cases that have no
 * meaning here. */
typedef enum zu2_status {
  /* The call did what it was asked and wrote its out-parameter. */
  ZU2_OK = 0,
  /* The engine refused the work: a full log, a node past the table,
   * a malformed record, an io error. The handle says which. */
  ZU2_ERROR = 3,
  /* The caller broke the contract in this header: a NULL handle, a NULL
   * out-parameter, a path that is not utf8. Nothing was done and
   * nothing is wrong with the database. */
  ZU2_MISUSE = 4,
  /* Two threads used one session at once. Distinct from ZU2_MISUSE
   * because it is the mistake a host makes by accident rather than by
   * typo, and because the fix is a different one: open another session
   * rather than correct the call. */
  ZU2_MISUSE_CONCURRENT = 5,
  /* The db already has as many sessions open as zu2_options.sessions
   * gave it room for. Nothing was done. A session belongs to a thread
   * for the length of a run, so this is a sizing mistake and not back
   * pressure: say how many threads there will be and open one each. */
  ZU2_NO_SESSIONS = 6
} zu2_status;

/* How far a write waits before it is acknowledged.
 *
 * Two settings and not three, and 06-durability-recovery.md section 2
 * is the argument. ZU2_ASYNC returns once the record is in the log's
 * memory tail, which a background thread writes out behind the caller;
 * a crash loses a suffix, never a hole. ZU2_DURABLE returns once the
 * device has acknowledged it, and it is a group commit, so one device
 * write serves every commit queued behind it.
 */
typedef enum zu2_durability {
  ZU2_ASYNC = 0,
  ZU2_DURABLE = 1
} zu2_durability;

/* Which way an edge is followed.
 *
 * ZU2_BOTH is the undirected reading, and it is two loads rather than
 * one: the engine keeps an out list and an in list and has no third
 * list for an edge with no arrow on it. Every call that takes a
 * direction takes this one, and every one of them counts a node
 * reachable both ways round once. */
typedef enum zu2_direction {
  ZU2_OUT = 0,
  ZU2_IN = 1,
  ZU2_BOTH = 2
} zu2_direction;

/* How a database is sized and how durable it is.
 *
 * Every field zero means the engine's own defaults, so a caller that
 * memsets the struct and sets nothing gets a working database.
 * index_buckets and max_nodes are sized once and not grown, so a
 * caller who knows the shape should say so: past the load factor the
 * index's collision chains lengthen, which is graceful and is not free.
 */
typedef struct zu2_options {
  /* Default durability for sessions opened from this db. */
  zu2_durability durability;
  /* Hash index buckets, rounded up to a power of two. Eight entries to
   * a bucket and the table wants to stay under half full, so
   * records / 4 is a reasonable hint. 0 takes the default. */
  uint64_t index_buckets;
  /* The live span the log may reach, in 4 MiB pages, counted from the
   * compaction floor to the tail. A db that compacts hard enough to
   * keep its span under this never reaches it, however long it runs
   * and however much it writes. 0 takes the default. */
  uint64_t max_pages;
  /* Nodes the graph plane is sized for. Only one pointer per 16384
   * nodes per direction is allocated up front, so this is cheap to
   * set high. 0 takes the default. */
  uint64_t max_nodes;
  /* Bytes of log kept per byte of live data, as a percent. 200 settles
   * the file at about twice the live set. 0 takes the default. */
  uint32_t space_target_percent;
  /* Log span below which compaction does not bother, in bytes. A load
   * that will be measured and thrown away wants compaction off, which
   * is what UINT64_MAX does. 0 takes the default. */
  uint64_t compact_below;
  /* Sessions this db can have open at once, which is the number of
   * threads that will use it. One epoch slot and one set of read
   * buffers each, so a cacheline and a few kilobytes per session, and
   * the engine keeps its own slots for flushing and compaction on top
   * of this number. 0 takes the default of 128. */
  uint64_t sessions;
  /* Nonzero pins the index at index_buckets however many keys arrive,
   * which is what measuring the cost of crowding needs, and the only
   * way a caller who knows its key count exactly keeps the migration
   * check off its read path. 0 grows, which is the default.
   *
   * New fields go on the end. Zero the struct before filling it in and
   * an older caller against a newer library gets defaults for what it
   * does not know about rather than a shifted read. */
  uint32_t fixed_index;
  /* Nonzero opens a file with a hole in it anyway, at the prefix below
   * the hole. Off by default, because a hole is records that were
   * acknowledged and are now gone, and an open that says nothing about
   * that is worse than one that fails. zu2_discarded() is how much a
   * salvaged open threw away. */
  uint32_t salvage;
  /* Nonzero keeps the scan plane, which is what zu2_scan runs on. Off
   * by default: the plane is a node per key held in memory for as long
   * as the db is open, and a host that only does point operations
   * should not pay for an order it never asks for. It has to be on for
   * the whole life of the data and not just the run that scans,
   * because the plane is built as the keys arrive. */
  uint32_t ordered;
  /* Nonzero stops a point read of a cold record putting it back in the
   * log. On by default: a record reaches the cold tier for having
   * survived a lap of the log unwritten, which says nothing about how
   * often it is read, and without promotion a record that was quiet for
   * one lap reads from the device for the life of the data. Off is for
   * measuring what promotion is worth, and zu2_promoted() says how much
   * of it happened. */
  uint32_t no_promote_reads;
  /* Pages of log kept in memory, 4 MiB each. 0 never evicts, which is
   * the default, so this field's zero means unbounded rather than
   * nothing kept.
   *
   * This is what decides whether the process's resident set is bounded
   * at all. zu2 reads records out of a mapping, so with no eviction
   * every page a read touches stays resident and a workload that reads
   * uniformly ends up holding the whole database. */
  uint64_t memory_pages;
  /* Nonzero stops the cold tier compressing the values it takes. On by
   * default: a record only reaches the tier by surviving a lap of the
   * log unwritten, and reading it back is a pread of a device, so a few
   * microseconds of decompress sit under a cost that is already there.
   * Off is for measuring what the coder is worth, and for data that is
   * already compressed, though the tier works that out per record
   * anyway and writes the value as it came when the frame is no
   * smaller. zu2_cold_value_bytes() is what it saved.
   *
   * Reading never consults this. A record says for itself whether it is
   * compressed, so turning it off stops new records being compressed
   * and leaves everything already written readable. */
  uint32_t no_cold_compression;
} zu2_options;

typedef struct zu2_db zu2_db;
typedef struct zu2_session zu2_session;

/* Fills an options struct with the engine's defaults. */
zu2_status zu2_options_init(zu2_options *opt);

/* Opens the database at path, creating it when it is not there and
 * replaying its log when it is.
 *
 * opt may be NULL for the defaults. On failure *out is NULL, and since
 * there is no handle to read a message from, the message is written
 * through err when err is not NULL: a pointer valid until the next
 * failed zu2_open on the same thread.
 */
zu2_status zu2_open(const char *path, size_t path_len, const zu2_options *opt,
                    zu2_db **out, const char **err, size_t *err_len);

/* Closes a database, stopping its background thread after one last
 * flush. A no-op on NULL. Sessions hold the engine alive on their own,
 * so closing a db that still has sessions open is safe; it is just not
 * usually what the caller meant. */
void zu2_close(zu2_db *db);

/* What went wrong in the last fallible call on this db, NUL-terminated
 * and empty when that call succeeded. Valid until the next call on this
 * db. */
const char *zu2_db_error(const zu2_db *db, size_t *len);

/* Opens a session. Hold one per thread for the whole run: it owns an
 * epoch slot and the buffers the read path uses, so an operation on a
 * warm database allocates nothing.
 *
 * ZU2_NO_SESSIONS when the db already has zu2_options.sessions of them
 * open. */
zu2_status zu2_session_open(zu2_db *db, zu2_session **out);

/* Closes a session. A no-op on NULL. */
void zu2_session_close(zu2_session *s);

/* What went wrong in the last fallible call on this session,
 * NUL-terminated and empty when that call succeeded. Valid until the
 * next fallible call on this session. */
const char *zu2_session_error(const zu2_session *s, size_t *len);

/* Changes how far this session waits before acknowledging a write.
 * Writes already acknowledged keep the guarantee they were given. The
 * setting belongs to the session and not to the file, which is the same
 * arrangement as sqlite's synchronous pragma. */
zu2_status zu2_set_durability(zu2_session *s, zu2_durability d);

/* ---- records ---- */

/* Writes value under key, whether or not it was there. */
zu2_status zu2_upsert(zu2_session *s, const uint8_t *key, size_t key_len,
                      const uint8_t *value, size_t value_len);

/* One key and one value, so a batch is one array rather than four.
 * NULL is allowed where the length beside it is zero. */
typedef struct zu2_pair {
  const uint8_t *key;
  size_t key_len;
  const uint8_t *value;
  size_t value_len;
} zu2_pair;

/* Writes every pair, waiting for durability once for the batch rather
 * than once per pair.
 *
 * A loader that calls zu2_upsert in a loop pays a foreign call per
 * record, and on a durable session a device write per record for a
 * guarantee it did not ask for. One call and one wait leave the batch
 * exactly as durable as its last record would have been on its own.
 *
 * *written is how many went in, and on ZU2_ERROR that is where it
 * stopped: everything before it is in the log and durable to the
 * session's setting, so a retry starts from there. A ZU2_MISUSE writes
 * zero and changes nothing, because the pointers are all checked before
 * the first record is. written may be NULL. */
zu2_status zu2_upsert_many(zu2_session *s, const zu2_pair *pairs,
                           size_t count, size_t *written);

/* Reads the newest value for key. *found says whether there was one.
 *
 * *value is valid until the next call on this session and it is not
 * always the same kind of memory. A record below the read only boundary
 * whose page is in memory is handed over where it lies, so *value points
 * into the log; anything else is read into the session's buffer first.
 * The caller cannot tell which and does not need to, because the bound
 * is the same either way.
 *
 * What the caller does need to know is that the first case is not free
 * to hold. The session keeps its epoch announced so the page cannot be
 * freed underneath the pointer, which means reclamation stops at that
 * epoch until the next call on this session ends the lease. Reading the
 * value and moving on costs nothing; holding it while doing something
 * slow holds the log. */
zu2_status zu2_read(zu2_session *s, const uint8_t *key, size_t key_len,
                    const uint8_t **value, size_t *value_len, int *found);

/* Reads up to count records in key order from the first key at or
 * after start, and points *pairs at the session's own array of them.
 * The array and the bytes it points into are good until the next call
 * on this session. The keys are always in the session's own buffer. A
 * value is handed over where it lies when its record is below the read
 * only boundary and its page is in memory, and copied into that buffer
 * when it is not, which the caller cannot tell apart and does not need
 * to.
 *
 * A scan that handed over even one value where it lies leaves the
 * session's epoch announced until the next call on it, so the pages
 * under those pointers cannot be freed. That is a lease and it is the
 * caller's to end: reclamation stops at that epoch while it is out, and
 * deferred frees queue behind it the same way they queue behind a long
 * operation. Walking the array and carrying on costs nothing. Walking
 * it slowly, or leaving it and going away, holds the log at the size it
 * was.
 *
 * *returned is how many were filled, which is fewer than count at the
 * end of the key set and is the whole answer: a key whose newest
 * record is a tombstone is walked past and not counted, because it is
 * not there.
 *
 * start may be NULL with a zero length, which is the first key.
 *
 * Fails when the db was opened without zu2_options.ordered, because
 * then there is no key order to walk and answering with an empty scan
 * would be a wrong answer rather than a missing feature. */
zu2_status zu2_scan(zu2_session *s, const uint8_t *start, size_t start_len,
                    size_t count, const zu2_pair **pairs, size_t *returned);

/* Removes key. *existed says whether it was there. */
zu2_status zu2_delete(zu2_session *s, const uint8_t *key, size_t key_len,
                      int *existed);

/* ---- graph ---- */

/* Creates a node under an external key and returns its dense id.
 *
 * The key to id mapping is an ordinary record, so looking a node up
 * by key is a hash probe and nothing more, and a traversal pays it once
 * at the seed rather than once per hop, because a frontier is dense
 * ids. */
zu2_status zu2_add_node(zu2_session *s, const uint8_t *key, size_t key_len,
                          uint32_t *node);

/* The dense id of the node with this key. */
zu2_status zu2_node_of(zu2_session *s, const uint8_t *key, size_t key_len,
                         uint32_t *node, int *found);

/* Links src to dst. Repeating an edge is not an error and does not grow
 * the neighbourhood. */
zu2_status zu2_add_edge(zu2_session *s, uint32_t src, uint32_t dst);

/* Unlinks src from dst. Removing an edge that is not there is not an
 * error. */
zu2_status zu2_remove_edge(zu2_session *s, uint32_t src, uint32_t dst);

/* The degree. One indexed load for ZU2_OUT and ZU2_IN, whatever the
 * degree is. ZU2_BOTH is the number of distinct neighbours either way
 * round, so it is two loads and a merge rather than a sum: a pair of
 * nodes that point at each other are one neighbour, not two. */
zu2_status zu2_degree(zu2_session *s, zu2_direction dir, uint32_t node,
                      uint32_t *degree);

/* A node's neighbours, ascending, copied into the session's buffer
 * and valid until the next call on it. */
zu2_status zu2_neighbours(zu2_session *s, zu2_direction dir, uint32_t node,
                          const uint32_t **out, size_t *len);

/* The distinct nodes exactly k hops from seed.
 *
 * k == 0 is the seed itself. Distinct is per level rather than
 * cumulative, which is what
 * `MATCH (a)-[:E]->()-[:E]->(c) RETURN count(DISTINCT c)` asks for: a
 * node two paths of length k both reach is counted once, and a node
 * that is also reachable in fewer hops is still counted. The frontier
 * is the session's buffer and is valid until the next call on it. */
zu2_status zu2_khop(zu2_session *s, zu2_direction dir, uint32_t seed,
                    uint32_t k, const uint32_t **out, size_t *len);

/* The distinct nodes reachable from seed in one hop or more and at
 * most max_depth hops, breadth first, in the order they were reached.
 *
 * max_depth 0 is no bound and walks the whole reachable set. max_visited
 * 0 is no bound on the answer's size; anything else stops the walk once
 * that many have been found, which bounds a probe on a graph with a
 * giant component.
 *
 * The seed is in the answer only if a path leads back to it, so this is
 * `MATCH (a)-[:E*1..k]->(c) RETURN count(DISTINCT c)` and not a walk of
 * the component a seed sits in. Add one for that.
 *
 * Under ZU2_BOTH a walk may go back along the edge it arrived on, so a
 * seed with any neighbour at all is two hops from itself. That is what
 * reachability over an undirected graph means and it is not what Cypher
 * means by an undirected variable-length pattern, which forbids a path
 * from using one relationship twice. */
zu2_status zu2_reach(zu2_session *s, zu2_direction dir, uint32_t seed,
                     uint32_t max_depth, uint64_t max_visited,
                     const uint32_t **out, size_t *len);

/* The hop count of a shortest path from src to dst.
 *
 * Writes found 0 and leaves hops 0 when there is no such path, which is
 * an answer and not an error. max_depth bounds the search and 0 means no
 * bound; a bounded search that ends without arriving reports not found,
 * so a caller who wants to know whether a path exists at all passes 0.
 * src == dst is nought hops and found. */
zu2_status zu2_shortest(zu2_session *s, zu2_direction dir, uint32_t src,
                        uint32_t dst, uint32_t max_depth, uint32_t *hops,
                        int *found);

/* Closed directed triangles through seed: pairs (b, c) with seed->b,
 * b->c and seed->c. */
zu2_status zu2_triangles(zu2_session *s, uint32_t seed, uint64_t *count);

/* How many nodes the graph holds. */
uint32_t zu2_nodes(const zu2_db *db);

/* ---- administration ---- */

/* Makes everything appended so far durable, whatever the sessions were
 * set to. This is how a loader running async gets its tail onto the
 * device before anything measures the file. */
zu2_status zu2_sync(zu2_db *db);

/* Compacts until another pass would not pay for itself, and reports the
 * bytes the filesystem took back. The background thread does this on
 * its own schedule; this is for a caller who wants the space now. */
zu2_status zu2_compact(zu2_db *db, uint64_t *reclaimed);

/* Bytes the file occupies on the device, which is not its length:
 * compaction punches holes, and a file with holes reports a length that
 * still counts them. This is the honest storage number. */
zu2_status zu2_disk_bytes(zu2_db *db, uint64_t *bytes);

/* The cold tier's half of what zu2_disk_bytes reports, holes excluded,
 * and zero on a db with no tier. Against zu2_disk_bytes this is the
 * migrated share, which belongs beside any number measured with the
 * tier on: the share a run settles at varies from 2 to 35 percent
 * across otherwise identical runs, and two runs that settled
 * differently are two different storage layouts. */
zu2_status zu2_cold_disk_bytes(zu2_db *db, uint64_t *bytes);

/* What the cold tier was given and what it wrote: value is the value
 * bytes of the records it has taken since it was opened, stored is the
 * bytes it wrote for them. Both zero with no tier, equal with
 * compression off, and either pointer may be null.
 *
 * stored over value is what the coder is buying on this data, which is
 * the only honest way to report it since it depends entirely on the
 * data. Records reclaimed since are in both numbers, so the ratio is a
 * property of the workload and not of when it is asked for.
 * zu2_cold_disk_bytes() is the other question, what the tier costs the
 * device right now. */
zu2_status zu2_cold_value_bytes(const zu2_db *db, uint64_t *value,
                                uint64_t *stored);

/* Addresses the cold tier still spans, and zero when there is no tier.
 * What it holds rather than what it costs the device, so this stays put
 * where the disk number falls when a cold pass punches a hole. */
uint64_t zu2_cold_span(const zu2_db *db);

/* Bytes compaction has moved to the cold tier since the db was opened.
 * Not what is down there now: a cold pass can take back everything it
 * was given, so a run that migrated a great deal can end with a small
 * span. */
uint64_t zu2_migrated(const zu2_db *db);

/* Addresses the log has spent, which is what the file would cost had
 * nothing ever been compacted away. */
uint64_t zu2_log_bytes(const zu2_db *db);

/* Addresses the log still spans, tail minus begin. */
uint64_t zu2_log_span(const zu2_db *db);

/* Slots in use in the hash index, for reporting the load factor a run
 * happened at. Slots and not keys: a full bucket displaces, and the
 * displaced key lives on the chain under the slot that took its place,
 * so this comes out below the number of keys written and printing it
 * against a record count reads as data loss. */
uint64_t zu2_index_occupancy(const zu2_db *db);

/* Distinct keys the index has installed, which is what a loader means
 * by rows and what zu2_index_occupancy cannot say. It goes up once per
 * key that was not already present, wherever that key ends up living,
 * so a load of n distinct keys answers n and a client that reported n
 * inserts can be checked against what arrived. Deletes do not take it
 * back down. */
uint64_t zu2_index_keys(const zu2_db *db);

/* Slots naming a chain of more than one key. Every lookup reaching one
 * of these buckets walks the chain whether its tag matched or not, so
 * this is the crowding a read pays for where the load factor is only
 * how full the table is. */
uint64_t zu2_index_foreign(const zu2_db *db);

/* Buckets in the live hash table. Against zu2_index_occupancy this is
 * the load factor, which is what says whether a read walked a chain
 * because the table was crowded or because the keys collided. */
uint64_t zu2_index_buckets(const zu2_db *db);

/* What the scan plane is holding, in bytes, or zero when the db has no
 * scan plane. Memory and not disk: the plane is rebuilt from the log
 * rather than written to it. The arena reserved rather than the bytes
 * used, because reserved is what the process is holding. */
uint64_t zu2_ordered_bytes(const zu2_db *db);

/* Keys the scan plane has ever been told about, or zero when the db has
 * no scan plane. Not the number that are live: a delete leaves the key
 * here and writes a tombstone into the log. */
uint64_t zu2_ordered_keys(const zu2_db *db);

/* Times the index has doubled since the db was opened. Zero means the
 * table was sized right or was never grown, and the load factor tells
 * those apart. */
uint64_t zu2_index_grows(const zu2_db *db);

/* Nonzero while a doubling is still draining the old table, so a phase
 * that ends here ended mid-migration rather than in the steady state. */
uint32_t zu2_index_resizing(const zu2_db *db);

/* Log pages holding memory right now, each 4 MiB. The memory side of
 * the space column: zu2_disk_bytes says what the filesystem holds and
 * this says what the process does. */
uint64_t zu2_resident_pages(const zu2_db *db);

/* Records a read moved out of the cold tier and back into the log,
 * which is the cost side of promote_reads. */
uint64_t zu2_promoted(const zu2_db *db);

/* Bytes a salvaged open threw away, and zero on an open that had nothing
 * to throw away. A salvaged database is a short database and this is how
 * short. */
uint64_t zu2_discarded(const zu2_db *db);

/* The library version, NUL-terminated and valid forever. */
const char *zu2_version(size_t *len);

#ifdef __cplusplus
}
#endif

#endif /* ZU2_H */
