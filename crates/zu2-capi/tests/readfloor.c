// What a point read costs through the C API with no language runtime
// between the loop and the library. tamnd/zu#645.
//
// A read through go-ycsb costs several times what the engine costs, and
// the question this file settles is which side of the boundary the rest
// of it is on. The same 200000 record database and the same key stream
// as the Go benchmark in tamnd/go-ycsb db/zu2/floor_test.go, driven from
// C, so the difference between the two is everything cgo and the Go
// runtime add and nothing else.
//
// Not a test and not run by CI: it writes 200 MiB and takes a couple of
// seconds a run. Build it by hand, from the root of this repository,
// after cargo build --release -p zu2-capi:
//
//   cc -O2 -I crates/zu2-capi/include -L target/release -lzu2 \
//      -Wl,-rpath,$PWD/target/release \
//      -o /tmp/readfloor crates/zu2-capi/tests/readfloor.c

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "zu2.h"

#define RECORDS 200000
#define OPS 2000000
#define VALUE 1000

static double now_ns(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (double)t.tv_sec * 1e9 + (double)t.tv_nsec;
}

int main(void) {
  char path[] = "/tmp/readfloor.zu2";
  char cmd[256];
  snprintf(cmd, sizeof cmd, "rm -rf %s %s.*", path, path);
  if (system(cmd) != 0) {
    fprintf(stderr, "could not clear %s\n", path);
  }

  zu2_options opt;
  if (zu2_options_init(&opt) != ZU2_OK) {
    return fprintf(stderr, "options\n");
  }
  opt.durability = ZU2_ASYNC;
  opt.index_buckets = RECORDS / 4 + 1;
  opt.sessions = 4;

  zu2_db *db = NULL;
  const char *err = NULL;
  size_t err_len = 0;
  if (zu2_open(path, strlen(path), &opt, &db, &err, &err_len) != ZU2_OK) {
    return fprintf(stderr, "open: %.*s\n", (int)err_len, err ? err : "");
  }

  zu2_session *s = NULL;
  if (zu2_session_open(db, &s) != ZU2_OK) {
    return fprintf(stderr, "session\n");
  }

  unsigned char value[VALUE];
  memset(value, 'v', sizeof value);
  char key[64];
  for (int i = 0; i < RECORDS; i++) {
    int n = snprintf(key, sizeof key, "usertable:user%019d", i);
    if (zu2_upsert(s, (const unsigned char *)key, (size_t)n, value, sizeof value) != ZU2_OK) {
      return fprintf(stderr, "upsert %d\n", i);
    }
  }
  zu2_sync(db);

  // The same stride the Go benchmark walks, so the two are asking for
  // the same records in the same order.
  double t0 = now_ns();
  unsigned long long bytes = 0;
  for (int i = 0; i < OPS; i++) {
    int at = (int)(((long long)i * 7919) % RECORDS);
    int n = snprintf(key, sizeof key, "usertable:user%019d", at);
    const unsigned char *val = NULL;
    size_t val_len = 0;
    int found = 0;
    if (zu2_read(s, (const unsigned char *)key, (size_t)n, &val, &val_len, &found) != ZU2_OK) {
      return fprintf(stderr, "read %d\n", at);
    }
    if (!found) {
      return fprintf(stderr, "key %d is not there\n", at);
    }
    bytes += val_len;
  }
  double t1 = now_ns();

  // snprintf is not free and the Go side builds its key with an append,
  // so the key is timed on its own and subtracted rather than left in
  // the read.
  double k0 = now_ns();
  unsigned long long sink = 0;
  for (int i = 0; i < OPS; i++) {
    int at = (int)(((long long)i * 7919) % RECORDS);
    sink += (unsigned long long)snprintf(key, sizeof key, "usertable:user%019d", at);
  }
  double k1 = now_ns();

  printf("read %.1f ns/op including the key, key %.1f ns/op, read alone %.1f ns/op\n",
         (t1 - t0) / OPS, (k1 - k0) / OPS, ((t1 - t0) - (k1 - k0)) / OPS);
  printf("# %llu bytes read, %llu from the key loop\n", bytes, sink);

  zu2_session_close(s);
  zu2_close(db);
  return 0;
}
