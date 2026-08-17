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
 * header and the library disagree about a type or a struct layout, or
 * when the artifact for this platform was built against the wrong libc.
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

  /* The one struct that crosses by value, so its layout is a thing the
   * artifact and the header have to agree about rather than assume.
   * Setting a key by name is how a binding forwards a user's options,
   * and it is checked here because a binding that gets it wrong gets it
   * wrong on every platform at once. */
  zu_config cfg;
  zu_error *err = NULL;
  zu_status status = ZU_OK;
  if (zu_config_init(&cfg) != ZU_OK || cfg.struct_size != sizeof(zu_config)) {
    return fail("the header and the library disagree about sizeof(zu_config)");
  }
  status = zu_config_set_z(&cfg, "threads", "1", &err);
  if (status != ZU_OK) {
    return report("config set failed", status, err);
  }
  if (cfg.threads != 1) {
    return fail("a configuration key was accepted and not written");
  }
  if (zu_config_set_z(&cfg, "no_such_key", "1", NULL) != ZU_MISUSE) {
    return fail("an unknown configuration key was accepted");
  }

  zu_database *db = NULL;
  status = zu_database_open_z(argv[1], &cfg, &db, &err);
  if (status != ZU_OK) {
    return report("open failed", status, err);
  }

  const char *path = NULL;
  size_t path_len = 0;
  if (zu_database_path(db, &path, &path_len) != ZU_OK || path == NULL ||
      strcmp(path, argv[1]) != 0 || path_len != strlen(argv[1])) {
    zu_database_close(db);
    return fail("the database does not know where it lives");
  }

  /* Two connections on one database, which is the whole reason the two
   * are separate handles: a pool opens once and connects per worker. */
  zu_conn *first = NULL;
  zu_conn *second = NULL;
  status = zu_connect(db, &first, &err);
  if (status != ZU_OK) {
    zu_database_close(db);
    return report("connect failed", status, err);
  }
  status = zu_connect(db, &second, &err);
  if (status != ZU_OK) {
    zu_conn_close(first);
    zu_database_close(db);
    return report("second connect failed", status, err);
  }

  /* The graph is the four edges the workflow copied in, so the count is
   * a number this knows rather than one it prints and accepts. Both
   * connections answer it, each out of its own caches. */
  int which;
  for (which = 0; which < 2; which++) {
    zu_conn *conn = which == 0 ? first : second;
    zu_result *result = NULL;
    status = zu_query_z(conn, "MATCH (a) RETURN count(a) AS n", &result, &err);
    if (status != ZU_OK) {
      zu_conn_close(first);
      zu_conn_close(second);
      zu_database_close(db);
      return report("query failed", status, err);
    }
    if (zu_result_rows(result) != 1 || zu_result_cols(result) != 1) {
      zu_result_free(result);
      zu_conn_close(first);
      zu_conn_close(second);
      zu_database_close(db);
      return fail("a count is one row of one column");
    }

    const char *name = NULL;
    size_t name_len = 0;
    int32_t type = -1;
    const int64_t *counts = NULL;
    if (zu_result_col_name(result, 0, &name, &name_len) != ZU_OK || strcmp(name, "n") != 0 ||
        name_len != 1 || zu_result_cell_type(result, 0, 0, &type) != ZU_OK || type != ZU_TYPE_INT ||
        zu_result_col_i64(result, 0, &counts) != ZU_OK || counts == NULL || counts[0] != 4) {
      zu_result_free(result);
      zu_conn_close(first);
      zu_conn_close(second);
      zu_database_close(db);
      return fail("the graph has four nodes and this says otherwise");
    }
    zu_result_free(result);
  }
  zu_conn_close(second);

  /* Closing the database leaves the connection working, because the
   * connection holds its own file handle. A platform where that is
   * wrong is a platform where every pooled host crashes on shutdown. */
  zu_database_close(db);

  /* The chunked read, which is the loop every binding writes over a
   * result it does not want to convert whole. Four nodes fit one chunk,
   * so what this checks is that the loop closes: the count bounds it,
   * each chunk reports its own size, and the values are the ones the
   * whole-column call gives. */
  {
    zu_result *chunked = NULL;
    status = zu_query_z(first, "MATCH (a) RETURN a AS n", &chunked, &err);
    if (status != ZU_OK) {
      zu_conn_close(first);
      return report("chunked query failed", status, err);
    }
    uint64_t chunks = zu_result_chunk_count(chunked);
    uint64_t seen = 0;
    uint64_t sum = 0;
    uint64_t i;
    if (chunks != 1) {
      zu_result_free(chunked);
      zu_conn_close(first);
      return fail("four rows are one chunk");
    }
    for (i = 0; i < chunks; i++) {
      uint64_t offset = 0;
      uint64_t rows = 0;
      const uint64_t *offsets = NULL;
      const uint8_t *valid = NULL;
      uint64_t r;
      if (zu_result_chunk(chunked, i, &offset, &rows) != ZU_OK || offset != seen || rows == 0) {
        zu_result_free(chunked);
        zu_conn_close(first);
        return fail("a chunk that does not know where it starts");
      }
      if (zu_result_chunk_col_node_offset(chunked, i, 0, &offsets) != ZU_OK || offsets == NULL ||
          zu_result_chunk_col_valid(chunked, i, 0, &valid) != ZU_OK || valid == NULL) {
        zu_result_free(chunked);
        zu_conn_close(first);
        return fail("a chunk of a node column did not read");
      }
      for (r = 0; r < rows; r++) {
        if (valid[r] != 1) {
          zu_result_free(chunked);
          zu_conn_close(first);
          return fail("a matched node read as null");
        }
        sum += offsets[r];
      }
      seen += rows;
    }
    /* Four nodes, offsets 0 through 3, so the sum is 6. */
    if (seen != zu_result_rows(chunked) || sum != 6) {
      zu_result_free(chunked);
      zu_conn_close(first);
      return fail("the chunks are not the column");
    }
    if (zu_result_chunk(chunked, chunks, NULL, NULL) != ZU_MISUSE) {
      zu_result_free(chunked);
      zu_conn_close(first);
      return fail("a chunk past the end was accepted");
    }
    zu_result_free(chunked);
  }

  /* The cell reader, which is how the values that have no column reach
   * a C host. A pointer into the result rather than a handle, so what
   * this checks on top of the answers is that nothing here needs
   * freeing and that a borrowed pointer is still good after its
   * neighbours have been read: a sanitizer running this is the reason
   * it is in the C test and not only in the Rust one. */
  {
    zu_result *values = NULL;
    const zu_value *cell = NULL;
    const zu_value *inner = NULL;
    const char *text = NULL;
    size_t text_len = 0;
    int32_t kind = -1;
    int64_t count = 0;
    int32_t offset = -1;
    int64_t n = 0;
    status = zu_query_z(first, "RETURN DATE '2024-02-29' AS d, [[1, 2], 'hi'] AS l", &values, &err);
    if (status != ZU_OK) {
      zu_conn_close(first);
      return report("cell query failed", status, err);
    }
    if (zu_result_cell(values, 0, 0, &cell) != ZU_OK || zu_value_type(cell) != ZU_TYPE_TEMPORAL ||
        zu_value_temporal(cell, &kind, &count, &offset) != ZU_OK || kind != ZU_TEMPORAL_DATE ||
        count != 19782 || offset != 0) {
      zu_result_free(values);
      zu_conn_close(first);
      return fail("a date did not read as days from the epoch");
    }
    /* The list, then the list inside it, then a string beside it, all
     * borrowed from the one result at the same time. */
    if (zu_result_cell(values, 0, 1, &cell) != ZU_OK || zu_value_type(cell) != ZU_TYPE_LIST ||
        zu_value_len(cell) != 2 || zu_value_at(cell, 0, &inner) != ZU_OK ||
        zu_value_len(inner) != 2) {
      zu_result_free(values);
      zu_conn_close(first);
      return fail("a list did not read as its elements");
    }
    if (zu_value_at(inner, 1, &inner) != ZU_OK || zu_value_i64(inner, &n) != ZU_OK || n != 2) {
      zu_result_free(values);
      zu_conn_close(first);
      return fail("a nested element did not read");
    }
    if (zu_value_at(cell, 1, &inner) != ZU_OK || zu_value_str(inner, &text, &text_len) != ZU_OK ||
        text_len != 2 || memcmp(text, "hi", 2) != 0) {
      zu_result_free(values);
      zu_conn_close(first);
      return fail("a borrowed string is a pointer and a length");
    }
    /* Past the end and the wrong accessor, which is where a host that
     * walks a list by hand goes wrong. */
    if (zu_value_at(cell, 2, &inner) != ZU_MISUSE || inner != NULL ||
        zu_value_i64(cell, &n) != ZU_MISUSE || n != 0) {
      zu_result_free(values);
      zu_conn_close(first);
      return fail("a list was read past its end or as an integer");
    }
    zu_result_free(values);
  }

  /* The loader, which is how values get in and so the other half of
   * what a binding does. It builds a database of its own beside the one
   * this was given, then opens it and reads back what it wrote: a
   * column that arrives wrong is a column that reads back wrong, and no
   * amount of checking the write alone would say so. */
  {
    const char *suffix = ".load";
    char *loaded = (char *)malloc(strlen(argv[1]) + strlen(suffix) + 1);
    zu_loader *loader = NULL;
    zu_conn *reader = NULL;
    zu_result *back = NULL;
    const int64_t ages[3] = {30, 40, 50};
    const double scores[3] = {1.5, 2.5, -0.5};
    const int32_t flags[3] = {1, 0, 7};
    const char *names[3] = {"ann", "bo", "cy"};
    /* 2024-02-29, 2024-03-01, and the day before the epoch. */
    const int64_t days[3] = {19782, 19783, -1};
    const uint32_t from[2] = {0, 1};
    const uint32_t to[2] = {1, 2};
    const int64_t *read = NULL;
    if (loaded == NULL) {
      zu_conn_close(first);
      return fail("out of memory");
    }
    strcpy(loaded, argv[1]);
    strcat(loaded, suffix);
    /* The smoke test runs several times over one working directory, and
     * a loader will not clobber a database, so the leftover from the
     * last run goes first. */
    remove(loaded);

    status = zu_loader_create_z(loaded, &loader, &err);
    if (status != ZU_OK) {
      free(loaded);
      zu_conn_close(first);
      return report("loader create failed", status, err);
    }
    status = zu_loader_table_z(loader, "person", "knows", 3, &err);
    if (status == ZU_OK) {
      status = zu_loader_col_i64(loader, "age", 3, ages, 3, &err);
    }
    if (status == ZU_OK) {
      status = zu_loader_col_f64(loader, "score", 5, scores, 3, &err);
    }
    if (status == ZU_OK) {
      status = zu_loader_col_bool(loader, "ok", 2, flags, 3, &err);
    }
    if (status == ZU_OK) {
      status = zu_loader_col_str_z(loader, "name", names, 3, &err);
    }
    if (status == ZU_OK) {
      status = zu_loader_col_temporal(loader, "born", 4, ZU_TEMPORAL_DATE, days, 3, &err);
    }
    if (status == ZU_OK) {
      status = zu_loader_edges(loader, from, to, 2, &err);
    }
    if (status == ZU_OK) {
      status = zu_loader_finish(loader, &err);
    }
    if (status != ZU_OK) {
      zu_loader_free(loader);
      remove(loaded);
      free(loaded);
      zu_conn_close(first);
      return report("load failed", status, err);
    }
    /* Spent, and saying so rather than writing a second table through a
     * handle the host forgot to drop. */
    if (zu_loader_finish(loader, NULL) != ZU_MISUSE_CLOSED) {
      zu_loader_free(loader);
      remove(loaded);
      free(loaded);
      zu_conn_close(first);
      return fail("a finished loader took another call");
    }
    zu_loader_free(loader);

    status = zu_open_z(loaded, &reader, &err);
    if (status != ZU_OK) {
      remove(loaded);
      free(loaded);
      zu_conn_close(first);
      return report("the loaded database did not open", status, err);
    }
    status = zu_query_z(reader, "MATCH (p:person) RETURN p.age AS a ORDER BY a", &back, &err);
    if (status != ZU_OK) {
      zu_conn_close(reader);
      remove(loaded);
      free(loaded);
      zu_conn_close(first);
      return report("the loaded database did not read", status, err);
    }
    if (zu_result_rows(back) != 3 || zu_result_col_i64(back, 0, &read) != ZU_OK || read == NULL ||
        read[0] != 30 || read[1] != 40 || read[2] != 50) {
      zu_result_free(back);
      zu_conn_close(reader);
      remove(loaded);
      free(loaded);
      zu_conn_close(first);
      return fail("a column went in and came back as something else");
    }
    zu_result_free(back);

    /* The date, read through the cell reader in the unit it was written
     * in, which is the round trip a corpus runner is actually made of.
     */
    back = NULL;
    status = zu_query_z(reader, "MATCH (p:person) WHERE p.name = 'ann' RETURN p.born AS b", &back,
                        &err);
    if (status != ZU_OK) {
      zu_conn_close(reader);
      remove(loaded);
      free(loaded);
      zu_conn_close(first);
      return report("the loaded date did not read", status, err);
    }
    {
      const zu_value *born = NULL;
      int32_t kind = -1;
      int64_t count = 0;
      if (zu_result_rows(back) != 1 || zu_result_cell(back, 0, 0, &born) != ZU_OK ||
          zu_value_temporal(born, &kind, &count, NULL) != ZU_OK || kind != ZU_TEMPORAL_DATE ||
          count != 19782) {
        zu_result_free(back);
        zu_conn_close(reader);
        remove(loaded);
        free(loaded);
        zu_conn_close(first);
        return fail("a date went in as days and came back as something else");
      }
    }
    zu_result_free(back);
    zu_conn_close(reader);
    remove(loaded);
    free(loaded);
  }

  /* A well formed query with nothing to return answers ZU_DONE rather
   * than an error, which is the whole reason a status crosses this
   * boundary alongside the pointer. */
  zu_result *result = NULL;
  status = zu_query_z(first, "MATCH (a) WHERE false RETURN a AS a", &result, &err);
  if (status != ZU_OK) {
    zu_conn_close(first);
    return report("empty query failed", status, err);
  }
  const int64_t *nothing = (const int64_t *)1;
  if (zu_result_col_i64(result, 0, &nothing) != ZU_DONE || nothing != NULL) {
    zu_result_free(result);
    zu_conn_close(first);
    return fail("an empty column is ZU_DONE and a NULL pointer");
  }
  /* The chunked path says the same thing by having nothing to say: no
   * rows means no chunks, so the loop above would run zero times. */
  if (zu_result_chunk_count(result) != 0) {
    zu_result_free(result);
    zu_conn_close(first);
    return fail("an empty result has no chunks");
  }
  zu_result_free(result);

  /* The failure path crosses the boundary the other way: an error
   * allocated in Rust, read here through its accessors, and released
   * with zu_error_free. A platform where that is wrong is a platform
   * where every error a user ever sees leaks or crashes. */
  err = NULL;
  result = NULL;
  status = zu_query_z(first, "NOT A QUERY", &result, &err);
  if (status != ZU_ERROR) {
    zu_result_free(result);
    zu_error_free(err);
    zu_conn_close(first);
    return fail("nonsense was not refused as ZU_ERROR");
  }
  if (result != NULL) {
    zu_result_free(result);
    zu_error_free(err);
    zu_conn_close(first);
    return fail("a refusal still handed back a result");
  }
  size_t len = 0;
  const char *message = zu_error_message(err, &len);
  if (err == NULL || message == NULL || len == 0 || message[len] != '\0') {
    zu_error_free(err);
    zu_conn_close(first);
    return fail("a refusal with no message");
  }
  if (zu_error_status(err) != ZU_ERROR || zu_error_severity(err) != ZU_SEVERITY_EXCEPTION) {
    zu_error_free(err);
    zu_conn_close(first);
    return fail("a refusal that does not know what it was");
  }
  if (zu_error_code(err, NULL) == NULL) {
    zu_error_free(err);
    zu_conn_close(first);
    return fail("a syntax error with no GQLSTATUS code");
  }
  /* The place, as numbers. A binding that wants to underline the token
   * reads it here rather than out of the message, which also says it. */
  uint32_t line = 0;
  uint32_t column = 0;
  if (zu_error_position(err, &line, &column) != ZU_OK || line != 1 || column != 1) {
    zu_error_free(err);
    zu_conn_close(first);
    return fail("a syntax error that will not say where");
  }
  zu_error_free(err);

  /* A condition raised while the statement runs happened at no token,
   * and says it has no place rather than pointing at one it guessed. */
  err = NULL;
  result = NULL;
  status = zu_query_z(first, "RETURN 1 / 0", &result, &err);
  if (status != ZU_ERROR || err == NULL) {
    zu_result_free(result);
    zu_error_free(err);
    zu_conn_close(first);
    return fail("dividing by zero was not refused");
  }
  line = 7;
  column = 9;
  if (zu_error_position(err, &line, &column) != ZU_DONE || line != 7 || column != 9) {
    zu_error_free(err);
    zu_conn_close(first);
    return fail("an error with no place wrote one anyway");
  }
  zu_error_free(err);

  /* A statement that outlives its connection answers rather than
   * following the pointer it still holds, and is still safe to close. */
  zu_stmt *stmt = NULL;
  err = NULL;
  status = zu_prepare_z(first, "MATCH (a) RETURN count(a) AS n", &stmt, &err);
  if (status != ZU_OK) {
    zu_conn_close(first);
    return report("prepare failed", status, err);
  }
  zu_conn_close(first);
  result = NULL;
  if (zu_execute(stmt, &result, NULL) != ZU_MISUSE_CLOSED || result != NULL) {
    zu_stmt_close(stmt);
    return fail("a statement outlived its connection and was let through");
  }
  zu_stmt_close(stmt);

  printf(
      "smoke: libzu %s on this platform, two connections, four nodes, one chunk, one date, one "
      "nested list, one load, one refusal with a place and one without\n",
      version);
  return 0;
}
