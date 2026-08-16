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

/* Prints what an error carries and releases it, which is also the check
 * that all four accessors are exported and agree with the header. */
static int report(const char *what, zu_status status, zu_error *err) {
  const char *message = zu_error_message(err, NULL);
  const char *code = zu_error_code(err, NULL);
  fprintf(stderr, "smoke: %s: status %d, code %s, severity %d: %s\n", what, (int)status,
          code ? code : "(none)", zu_error_severity(err), message ? message : "(no message)");
  zu_error_free(err);
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

  zu_error *err = NULL;
  zu_session *session = NULL;
  zu_status status = zu_open_z(argv[1], &session, &err);
  if (status != ZU_OK) {
    return report("open failed", status, err);
  }

  /* The graph is the four edges the workflow copied in, so the count is
   * a number this knows rather than one it prints and accepts. */
  zu_result *result = NULL;
  status = zu_query_z(session, "MATCH (a) RETURN count(a) AS n", &result, &err);
  if (status != ZU_OK) {
    report("query failed", status, err);
    zu_close(session);
    return 1;
  }
  if (zu_result_rows(result) != 1 || zu_result_cols(result) != 1) {
    zu_result_free(result);
    zu_close(session);
    return fail("a count is one row of one column");
  }

  const char *name = NULL;
  size_t name_len = 0;
  if (zu_result_col_name(result, 0, &name, &name_len) != ZU_OK || strcmp(name, "n") != 0 ||
      name_len != 1) {
    zu_result_free(result);
    zu_close(session);
    return fail("the column is not named n");
  }

  int32_t type = -1;
  if (zu_result_cell_type(result, 0, 0, &type) != ZU_OK || type != ZU_TYPE_INT) {
    zu_result_free(result);
    zu_close(session);
    return fail("a count is an integer");
  }

  const int64_t *counts = NULL;
  if (zu_result_col_i64(result, 0, &counts) != ZU_OK || counts == NULL || counts[0] != 4) {
    zu_result_free(result);
    zu_close(session);
    return fail("the graph has four nodes and this says otherwise");
  }
  zu_result_free(result);

  /* A well formed query with nothing to return answers ZU_DONE rather
   * than an error, which is the whole reason a status crosses this
   * boundary alongside the pointer. */
  result = NULL;
  status = zu_query_z(session, "MATCH (a) WHERE false RETURN a AS a", &result, &err);
  if (status != ZU_OK) {
    report("empty query failed", status, err);
    zu_close(session);
    return 1;
  }
  const int64_t *nothing = (const int64_t *)1;
  if (zu_result_col_i64(result, 0, &nothing) != ZU_DONE || nothing != NULL) {
    zu_result_free(result);
    zu_close(session);
    return fail("an empty column is ZU_DONE and a NULL pointer");
  }
  zu_result_free(result);

  /* The failure path crosses the boundary the other way: an error
   * allocated in Rust, read here through its accessors, and released
   * with zu_error_free. A platform where that is wrong is a platform
   * where every error a user ever sees leaks or crashes. */
  err = NULL;
  result = NULL;
  status = zu_query_z(session, "NOT A QUERY", &result, &err);
  if (status != ZU_ERROR) {
    zu_result_free(result);
    zu_error_free(err);
    zu_close(session);
    return fail("nonsense was not refused as ZU_ERROR");
  }
  if (result != NULL) {
    zu_result_free(result);
    zu_error_free(err);
    zu_close(session);
    return fail("a refusal still handed back a result");
  }
  size_t len = 0;
  const char *message = zu_error_message(err, &len);
  if (err == NULL || message == NULL || len == 0 || message[len] != '\0') {
    zu_error_free(err);
    zu_close(session);
    return fail("a refusal with no message");
  }
  if (zu_error_status(err) != ZU_ERROR || zu_error_severity(err) != ZU_SEVERITY_EXCEPTION) {
    zu_error_free(err);
    zu_close(session);
    return fail("a refusal that does not know what it was");
  }
  if (zu_error_code(err, NULL) == NULL) {
    zu_error_free(err);
    zu_close(session);
    return fail("a syntax error with no GQLSTATUS code");
  }
  zu_error_free(err);

  zu_close(session);
  printf("smoke: libzu %s on this platform, four nodes, one refusal\n", version);
  return 0;
}
