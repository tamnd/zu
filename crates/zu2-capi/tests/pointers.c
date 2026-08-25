/* The pointer lifetimes zu2.h promises, checked from C. tamnd/zu#753.
 *
 * Every read and every scan hands the caller a pointer into memory the
 * library owns, and the header now says exactly how long each one is
 * good for and what ends it. Nothing was checking that the library
 * agrees. The Rust tests hold Rust borrows, so the compiler answers
 * these questions for them before they run and they cannot ask the
 * question a C host actually asks, which is whether a pointer handed
 * back three calls ago still points at what it did.
 *
 * The one that matters is the scan. A scan hands back an array of pairs
 * and says all of them are good at once, and that is only true if the
 * library did not answer two of them out of the same buffer. It did, for
 * a while: a record whose page had been evicted was read into the
 * session's single scratch buffer and handed out as if it were resident,
 * so the next such record in the same walk overwrote the one before it
 * and a caller reading the array afterwards saw the last record fifty
 * times. That is tamnd/zu#751, and this file is the regression test for
 * it in the shape the bug had.
 *
 * Forcing that shape takes a database bigger than the memory it is
 * allowed, which is why this writes 30 MiB against memory_pages of five.
 * A scan from the first key then walks records whose pages are long
 * gone, which is the path that was wrong, and a scan of a small database
 * would walk nothing but resident ones and pass either way.
 *
 * Not run by CI: it writes 30 MiB and takes a second or two. Build it by
 * hand, from the root of this repository, after
 * cargo build --release -p zu2-capi:
 *
 *   cc -O2 -I crates/zu2-capi/include -L target/release -lzu2 \
 *      -Wl,-rpath,$PWD/target/release \
 *      -o /tmp/pointers crates/zu2-capi/tests/pointers.c
 *   /tmp/pointers /tmp/pointers.zu2
 *
 * It is worth a run under the sanitizers, which is where a pointer that
 * outlived its buffer stops being a wrong answer and starts being a
 * diagnostic:
 *
 *   cc -O1 -g -fsanitize=address,undefined ...
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
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

/* 60000 records of 512 bytes is a little under 30 MiB of values, against
 * five 4 MiB pages of memory. Whatever the per record overhead turns out
 * to be, the early keys are not resident by the end of the load. */
enum { RECORDS = 60000, VALUE = 512, SCAN = 200 };

/* A key is k%06d so the byte order and the number order are the same
 * thing, which is what lets the scan check that the keys climb. */
static size_t make_key(char *buf, size_t cap, int i) {
  int n = snprintf(buf, cap, "k%06d", i);
  return n < 0 ? 0 : (size_t)n;
}

/* Every record's value is filled with one byte derived from its own
 * number, so a value tells you which record it belongs to and two
 * records cannot be confused for each other. That is the whole trick:
 * the #751 bug showed up as every value in an array carrying the last
 * record's byte. */
static uint8_t byte_of(int i) { return (uint8_t)(i % 251 + 1); }

static void fill(uint8_t *buf, int i) { memset(buf, byte_of(i), VALUE); }

/* The number a value claims to be, or -1 when it is not one of ours. */
static int number_of(const uint8_t *value, size_t len) {
  if (len != VALUE) {
    return -1;
  }
  uint8_t want = value[0];
  for (size_t j = 1; j < len; j++) {
    if (value[j] != want) {
      return -1;
    }
  }
  return want;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s <database-path>\n", argv[0]);
    return 2;
  }
  const char *path = argv[1];

  zu2_options options;
  CHECK(zu2_options_init(&options) == ZU2_OK);
  options.durability = ZU2_ASYNC;
  options.ordered = 1;
  options.index_buckets = RECORDS;
  /* Five is the smallest setting that means anything, and the point of
   * it here is to make eviction certain rather than likely. */
  options.memory_pages = 5;
  /* Promotion off. A cold record that a read puts back in the log is a
   * resident record by the time the scan reaches it, and a test that
   * wants the evicted path taken should not have the engine quietly
   * moving records off it. */
  options.no_promote_reads = 1;

  zu2_db *db = NULL;
  const char *err = NULL;
  size_t err_len = 0;
  if (zu2_open(path, strlen(path), &options, &db, &err, &err_len) != ZU2_OK) {
    fprintf(stderr, "open %s: %.*s\n", path, (int)err_len, err ? err : "");
    return 1;
  }

  zu2_session *s = NULL;
  CHECK(zu2_session_open(db, &s) == ZU2_OK);

  /* An error buffer that has never held an error. The header says empty
   * and NUL-terminated, and a host that prints it unconditionally is
   * relying on both. */
  size_t msg_len = 0;
  const char *msg = zu2_session_error(s, &msg_len);
  CHECK(msg != NULL);
  CHECK(msg_len == 0);
  CHECK(msg != NULL && msg[0] == '\0');

  uint8_t value[VALUE];
  char key[32];
  for (int i = 0; i < RECORDS; i++) {
    size_t key_len = make_key(key, sizeof key, i);
    fill(value, i);
    if (zu2_upsert(s, (const uint8_t *)key, key_len, value, VALUE) != ZU2_OK) {
      const char *m = zu2_session_error(s, &msg_len);
      fprintf(stderr, "upsert %s: %.*s\n", key, (int)msg_len, m ? m : "");
      zu2_session_close(s);
      zu2_close(db);
      return 1;
    }
  }

  /* The database is bigger than the memory it was given, or the rest of
   * this is checking the resident path twice. */
  CHECK(zu2_log_bytes(db) > (uint64_t)options.memory_pages * 4 * 1024 * 1024);

  /* A read is good until the next call on this session, whichever kind
   * of memory it came out of, and the caller cannot tell which. So the
   * check is that it is right when it is handed over and still right
   * after some work that is not a call on this session. */
  const uint8_t *v = NULL;
  size_t v_len = 0;
  int found = 0;
  size_t key_len = make_key(key, sizeof key, 0);
  CHECK(zu2_read(s, (const uint8_t *)key, key_len, &v, &v_len, &found) ==
        ZU2_OK);
  CHECK(found == 1);
  CHECK(number_of(v, v_len) == byte_of(0));
  /* Calls on the database rather than on the session, which is a
   * different handle and not what the bound is stated in terms of. */
  (void)zu2_log_bytes(db);
  (void)zu2_ordered_keys(db);
  CHECK(number_of(v, v_len) == byte_of(0));

  /* And the same for a record at the other end, which is resident where
   * the first is not. Same promise, and this is the half that was never
   * in doubt. */
  key_len = make_key(key, sizeof key, RECORDS - 1);
  CHECK(zu2_read(s, (const uint8_t *)key, key_len, &v, &v_len, &found) ==
        ZU2_OK);
  CHECK(found == 1);
  CHECK(number_of(v, v_len) == byte_of(RECORDS - 1));

  /* A miss leaves nothing behind to read. */
  CHECK(zu2_read(s, (const uint8_t *)"nope", 4, &v, &v_len, &found) == ZU2_OK);
  CHECK(found == 0);
  CHECK(v == NULL && v_len == 0);

  /* The scan, and the reason this file exists.
   *
   * Every pair in the array is good at once, so the array is walked
   * after the call has returned rather than checked a pair at a time
   * inside a loop that could hide an aliasing bug behind its own
   * ordering. Two records answered out of the same buffer show up here
   * as two pairs whose values carry the same number, and #751 made that
   * every pair in the array. */
  const zu2_pair *pairs = NULL;
  size_t returned = 0;
  key_len = make_key(key, sizeof key, 0);
  CHECK(zu2_scan(s, (const uint8_t *)key, key_len, SCAN, &pairs, &returned) ==
        ZU2_OK);
  CHECK(returned == SCAN);
  CHECK(pairs != NULL);

  if (pairs != NULL) {
    int distinct = 0;
    int wrong = 0;
    int misordered = 0;
    for (size_t i = 0; i < returned; i++) {
      /* The key is the record's number in decimal, and the value is
       * that number in every one of its bytes, so the two have to
       * agree with each other and with the position in the walk. */
      char want[32];
      size_t want_len = make_key(want, sizeof want, (int)i);
      if (pairs[i].key_len != want_len ||
          memcmp(pairs[i].key, want, want_len) != 0) {
        misordered++;
        continue;
      }
      if (number_of(pairs[i].value, pairs[i].value_len) != byte_of((int)i)) {
        wrong++;
      }
      if (i > 0 && pairs[i].value != pairs[i - 1].value) {
        distinct++;
      }
    }
    CHECK(misordered == 0);
    CHECK(wrong == 0);
    /* Not every value has to sit at its own address: consecutive
     * records on a resident page are handed over where they lie and are
     * naturally adjacent, and the library is free to answer any of them
     * out of one buffer as long as it does not answer two. What must not
     * happen is one address for all of them, which is what the bug was,
     * so this asks that the array moved at all. */
    CHECK(distinct > 0);
  }

  /* The scan borrowed, so the session is holding a lease on the log and
   * the next call on it is what ends the lease. A host that does this
   * and then carries on is the ordinary case and it must not hang, and
   * the read has to answer correctly rather than out of a buffer the
   * scan is still using. */
  key_len = make_key(key, sizeof key, RECORDS / 2);
  CHECK(zu2_read(s, (const uint8_t *)key, key_len, &v, &v_len, &found) ==
        ZU2_OK);
  CHECK(found == 1);
  CHECK(number_of(v, v_len) == byte_of(RECORDS / 2));

  /* A scan that walks off the end of the data returns what there was and
   * not an error, and asking past the last key returns nothing at all
   * with the array left alone. */
  key_len = make_key(key, sizeof key, RECORDS - 3);
  CHECK(zu2_scan(s, (const uint8_t *)key, key_len, SCAN, &pairs, &returned) ==
        ZU2_OK);
  CHECK(returned == 3);
  pairs = NULL;
  returned = 12345;
  CHECK(zu2_scan(s, (const uint8_t *)"z", 1, SCAN, &pairs, &returned) ==
        ZU2_OK);
  CHECK(returned == 0);

  /* Nothing has failed on this session, so the error buffer is still
   * empty. A library that leaves the last successful call's message in
   * there would have a host reporting an error on every operation. */
  msg = zu2_session_error(s, &msg_len);
  CHECK(msg != NULL && msg_len == 0 && msg[0] == '\0');

  /* Closing while a lease is held is the case a callback-driven host
   * hits: it borrowed inside a callback and the host tore the session
   * down from in there rather than making another call. The close has to
   * release the lease itself, or reclamation keeps a floor under it for
   * the life of the database. There is nothing to assert from C beyond
   * that this returns, which under the sanitizers is not nothing. */
  const zu2_pair *held = NULL;
  size_t held_count = 0;
  key_len = make_key(key, sizeof key, 0);
  CHECK(zu2_scan(s, (const uint8_t *)key, key_len, SCAN, &held, &held_count) ==
        ZU2_OK);
  CHECK(held_count == SCAN);
  zu2_session_close(s);

  /* And the database still compacts afterwards, which is the thing a
   * leaked lease would have stopped: reclamation cannot pass an epoch a
   * dead session is still announcing. */
  uint64_t reclaimed = 0;
  CHECK(zu2_compact(db, &reclaimed) == ZU2_OK);

  zu2_close(db);

  if (failures > 0) {
    fprintf(stderr, "pointers: %d check(s) failed\n", failures);
    return 1;
  }
  printf("pointers: ok, %d records, %d scanned\n", RECORDS, SCAN);
  return 0;
}
