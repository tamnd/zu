/* The C host every platform's libzu has to satisfy.
 *
 * The Rust test beside this one calls the same functions through the
 * same extern "C" declarations, which proves the code is right and
 * proves nothing about the artifact: it links the rlib, so the symbols
 * it resolves are the ones the compiler just built, in a process the
 * compiler laid out. This links the shared library the release ships,
 * with a compiler that is not rustc, from a translation unit whose only
 * knowledge of zu is the header. That is the thing a user does, and it
 * is the thing that fails when a symbol is not exported, when the
 * header and the library disagree about a type, or when the artifact
 * for this platform was built against the wrong libc.
 *
 * Takes the path to a database, prints one line, and returns nonzero on
 * anything unexpected. Build it against the header in ../include.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "zu.h"

static int fail(const char *what) {
  fprintf(stderr, "smoke: %s\n", what);
  return 1;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    return fail("usage: smoke <database>");
  }

  const char *version = zu_version();
  if (version == NULL || version[0] == '\0') {
    return fail("no version string");
  }

  char *err = NULL;
  zu_session *session = zu_open(argv[1], &err);
  if (session == NULL) {
    fprintf(stderr, "smoke: open failed: %s\n", err ? err : "(no message)");
    zu_string_free(err);
    return 1;
  }

  /* The graph is the four edges the workflow copied in, so the count is
   * a number this knows rather than one it prints and accepts. */
  zu_result *result = zu_query(session, "MATCH (a) RETURN count(a) AS n", &err);
  if (result == NULL) {
    fprintf(stderr, "smoke: query failed: %s\n", err ? err : "(no message)");
    zu_string_free(err);
    zu_close(session);
    return 1;
  }
  if (zu_result_rows(result) != 1 || zu_result_cols(result) != 1) {
    zu_result_free(result);
    zu_close(session);
    return fail("a count is one row of one column");
  }
  if (strcmp(zu_result_col_name(result, 0), "n") != 0) {
    zu_result_free(result);
    zu_close(session);
    return fail("the column is not named n");
  }
  if (zu_result_cell_type(result, 0, 0) != ZU_TYPE_INT) {
    zu_result_free(result);
    zu_close(session);
    return fail("a count is an integer");
  }
  const int64_t *counts = zu_result_col_i64(result, 0);
  if (counts == NULL || counts[0] != 4) {
    zu_result_free(result);
    zu_close(session);
    return fail("the graph has four nodes and this says otherwise");
  }
  zu_result_free(result);

  /* The failure path crosses the boundary the other way: a message
   * allocated in Rust, read here, and released through zu_string_free.
   * A platform where that is wrong is a platform where every error a
   * user ever sees leaks or crashes. */
  err = NULL;
  zu_result *refused = zu_query(session, "NOT A QUERY", &err);
  if (refused != NULL) {
    zu_result_free(refused);
    zu_close(session);
    return fail("nonsense was accepted");
  }
  if (err == NULL || err[0] == '\0') {
    zu_close(session);
    return fail("a refusal with no message");
  }
  zu_string_free(err);

  zu_close(session);
  printf("smoke: libzu %s on this platform, four nodes, one refusal\n", version);
  return 0;
}
