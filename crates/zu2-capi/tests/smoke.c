/* A C host, in the shape a cgo adapter has: open once, session per
 * worker, load, traverse, read the answer out of the session's buffer
 * before the next call, close in order.
 *
 * The Rust tests next to this one drive the same functions, so what
 * this adds is the header. A hand-written header can disagree with the
 * library about a struct's layout or an enum's value and nothing in
 * Rust would notice, so this compiles against zu2.h and checks the two
 * agree where it would hurt: the options struct is filled by the
 * library and read back here, and every status is compared against the
 * name rather than the number.
 *
 * Usage: smoke <path-to-a-database-that-does-not-exist-yet>
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "zu2.h"

static int failures = 0;

#define CHECK(cond)                                                            \
  do {                                                                         \
    if (!(cond)) {                                                             \
      fprintf(stderr, "%s:%d: %s\n", __FILE__, __LINE__, #cond);               \
      failures++;                                                              \
    }                                                                          \
  } while (0)

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s <database-path>\n", argv[0]);
    return 2;
  }

  zu2_options options;
  CHECK(zu2_options_init(&options) == ZU2_OK);
  /* Every field zero is the defaults, and this is the check that the
   * library and this header agree on which field is which: setting one
   * by name here and having the database come up is worth more than
   * comparing sizeof against a number. */
  options.durability = ZU2_ASYNC;
  options.max_nodes = 1 << 16;
  options.compact_below = UINT64_MAX;

  zu2_db *db = NULL;
  const char *err = NULL;
  size_t err_len = 0;
  const char *path = argv[1];
  zu2_status status =
      zu2_open(path, strlen(path), &options, &db, &err, &err_len);
  if (status != ZU2_OK) {
    fprintf(stderr, "open %s: %.*s\n", path, (int)err_len, err ? err : "");
    return 1;
  }

  zu2_session *s = NULL;
  CHECK(zu2_session_open(db, &s) == ZU2_OK);

  /* Records. */
  CHECK(zu2_upsert(s, (const uint8_t *)"user1", 5, (const uint8_t *)"a", 1) ==
        ZU2_OK);
  const uint8_t *value = NULL;
  size_t value_len = 0;
  int found = 0;
  CHECK(zu2_read(s, (const uint8_t *)"user1", 5, &value, &value_len, &found) ==
        ZU2_OK);
  CHECK(found == 1);
  CHECK(value_len == 1 && value[0] == 'a');
  CHECK(zu2_read(s, (const uint8_t *)"nope", 4, &value, &value_len, &found) ==
        ZU2_OK);
  CHECK(found == 0);
  CHECK(value == NULL);

  /* A ring of a thousand nodes with a chord across it, so a two hop
   * frontier has two members and a one hop has one, and neither is the
   * whole graph. */
  enum { N = 1000 };
  uint32_t ids[N];
  for (int i = 0; i < N; i++) {
    char key[32];
    int len = snprintf(key, sizeof key, "v%d", i);
    CHECK(zu2_add_node(s, (const uint8_t *)key, (size_t)len, &ids[i]) ==
          ZU2_OK);
  }
  for (int i = 0; i < N; i++) {
    CHECK(zu2_add_edge(s, ids[i], ids[(i + 1) % N]) == ZU2_OK);
  }
  CHECK(zu2_add_edge(s, ids[0], ids[500]) == ZU2_OK);

  uint32_t degree = 0;
  CHECK(zu2_degree(s, ZU2_OUT, ids[0], &degree) == ZU2_OK);
  CHECK(degree == 2);
  CHECK(zu2_degree(s, ZU2_IN, ids[1], &degree) == ZU2_OK);
  CHECK(degree == 1);

  const uint32_t *out = NULL;
  size_t len = 0;
  CHECK(zu2_neighbours(s, ZU2_OUT, ids[0], &out, &len) == ZU2_OK);
  CHECK(len == 2);
  CHECK(zu2_khop(s, ZU2_OUT, ids[0], 1, &out, &len) == ZU2_OK);
  CHECK(len == 2);
  CHECK(zu2_khop(s, ZU2_OUT, ids[0], 2, &out, &len) == ZU2_OK);
  CHECK(len == 2);
  /* Round the ring and back to the start, so the seed is in its own
   * answer and the count is every node. */
  CHECK(zu2_reach(s, ZU2_OUT, ids[0], 0, 0, &out, &len) == ZU2_OK);
  CHECK(len == N);
  CHECK(zu2_reach(s, ZU2_OUT, ids[0], 1, 0, &out, &len) == ZU2_OK);
  CHECK(len == 2);
  CHECK(zu2_reach(s, ZU2_OUT, ids[0], 0, 10, &out, &len) == ZU2_OK);
  CHECK(len == 10);
  CHECK(out[0] == ids[1]);

  /* The chord is the short way to the far side of the ring, and the
   * long way round is what the ring itself is. */
  uint32_t hops = 0;
  CHECK(zu2_shortest(s, ZU2_OUT, ids[0], ids[500], 0, &hops, &found) == ZU2_OK);
  CHECK(found == 1 && hops == 1);
  CHECK(zu2_shortest(s, ZU2_OUT, ids[1], ids[0], 0, &hops, &found) == ZU2_OK);
  CHECK(found == 1 && hops == N - 1);
  CHECK(zu2_shortest(s, ZU2_BOTH, ids[1], ids[0], 0, &hops, &found) == ZU2_OK);
  CHECK(found == 1 && hops == 1);
  CHECK(zu2_shortest(s, ZU2_OUT, ids[1], ids[0], 3, &hops, &found) == ZU2_OK);
  CHECK(found == 0 && hops == 0);

  /* Undirected degree is a merge and not a sum: node 500 is pointed
   * at by 499 and by the chord and points at 501. */
  CHECK(zu2_degree(s, ZU2_BOTH, ids[500], &degree) == ZU2_OK);
  CHECK(degree == 3);

  uint64_t triangles = 0;
  CHECK(zu2_triangles(s, ids[0], &triangles) == ZU2_OK);
  CHECK(triangles == 0);
  CHECK(zu2_nodes(db) == N);

  /* A node found by key is the id it was created with, which is what
   * a loader's second pass over an edge list does. */
  uint32_t id = 0;
  CHECK(zu2_node_of(s, (const uint8_t *)"v500", 4, &id, &found) == ZU2_OK);
  CHECK(found == 1 && id == ids[500]);

  /* Misuse is caught rather than crashed on. */
  CHECK(zu2_degree(s, 42, ids[0], &degree) == ZU2_MISUSE);
  CHECK(zu2_read(NULL, (const uint8_t *)"k", 1, &value, &value_len, &found) ==
        ZU2_MISUSE);
  CHECK(value == NULL && value_len == 0 && found == 0);

  /* Administration. */
  CHECK(zu2_sync(db) == ZU2_OK);
  uint64_t bytes = 0;
  CHECK(zu2_disk_bytes(db, &bytes) == ZU2_OK);
  CHECK(bytes > 0);
  CHECK(zu2_log_bytes(db) > 0);
  CHECK(zu2_log_span(db) > 0);
  CHECK(zu2_index_occupancy(db) == N + 1);
  uint64_t reclaimed = 0;
  CHECK(zu2_compact(db, &reclaimed) == ZU2_OK);

  size_t version_len = 0;
  const char *version = zu2_version(&version_len);
  CHECK(version != NULL && version_len == strlen(version));

  const char *message = zu2_session_error(s, &len);
  CHECK(message != NULL);

  zu2_session_close(s);
  zu2_close(db);
  /* Both closes are no-ops on NULL, which is the contract a host with a
   * cleanup path on an early return relies on. */
  zu2_session_close(NULL);
  zu2_close(NULL);

  if (failures > 0) {
    fprintf(stderr, "smoke: %d check(s) failed\n", failures);
    return 1;
  }
  printf("smoke: ok, libzu2 %s\n", version);
  return 0;
}
