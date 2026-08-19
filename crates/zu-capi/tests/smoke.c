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

/* A host's progress callback with nothing to report to: counts the
 * calls and lets the statement carry on. Returning 0 here is how a host
 * stops one, which the Rust test beside this file does. */
static int ticked(void *user_data, uint64_t rows, uint64_t ms) {
  (void)rows;
  (void)ms;
  *(unsigned long *)user_data += 1;
  return 1;
}

/* Columns a host keeps in its own memory, which a frame names as a
 * table without copying any of it. The offsets are Arrow's Utf8: one
 * more than there are rows, the last being how much of names is used.
 */
struct lent {
  int64_t ns[3];
  double scores[3];
  int32_t ends[4];
  char names[9];
};

/* How many times the library said it was finished with the arrays
 * above. A count rather than a flag, because once is the contract and
 * twice would be a double free the sanitizer would have to catch on
 * its own. */
static int handed_back = 0;

static void give_back(void *owner) {
  handed_back += 1;
  free(owner);
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

  /* The appender, which is the other way values get in: not a database
   * that does not exist yet but a table that does. It builds one of its
   * own beside the database this was handed, for the reason the loader
   * block above builds one, writes rows a value at a time, and reads
   * them back, because a value that went into the wrong column is a
   * value that reads back wrong and nothing about the write alone would
   * say so. */
  {
    const char *suffix = ".append";
    char *appended = (char *)malloc(strlen(argv[1]) + strlen(suffix) + 1);
    zu_conn *writer = NULL;
    zu_appender *app = NULL;
    zu_result *back = NULL;
    const int64_t *read = NULL;
    const char *named = NULL;
    const char *names[2] = {"bo", "cy"};
    const int64_t ages[2] = {40, 50};
    uint32_t cols = 0;
    uint64_t buffered = 0;
    uint64_t committed = 0;
    size_t named_len = 0;
    int i = 0;
    if (appended == NULL) {
      zu_conn_close(first);
      return fail("out of memory");
    }
    strcpy(appended, argv[1]);
    strcat(appended, suffix);
    remove(appended);

    err = NULL;
    status = zu_create_z(appended, &writer, &err);
    if (status != ZU_OK) {
      free(appended);
      zu_conn_close(first);
      return report("a database could not be created to append to", status, err);
    }
    /* The one way a C host has of declaring a table is writing a row of
     * it, and the columns an appender takes are the ones that row
     * made. */
    status = zu_query_z(writer, "INSERT (p:person {age: 30, name: 'ann'})", &back, &err);
    if (status != ZU_OK) {
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return report("a table could not be declared to append to", status, err);
    }
    /* A write has nothing to project, and the completion condition for
     * that is not the one a query gets: 00001 is successful completion
     * with the result omitted, which is neither a failure nor an empty
     * answer. */
    if (zu_result_cols(back) != 0 || zu_result_gqlstatus(back, NULL) == NULL ||
        strcmp(zu_result_gqlstatus(back, NULL), "00001") != 0 || zu_result_notices(back) != 0) {
      zu_result_free(back);
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return fail("a statement with nothing to project said otherwise");
    }
    zu_result_free(back);
    back = NULL;

    status = zu_appender_open_z(writer, "person", &app, &err);
    if (status != ZU_OK) {
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return report("an appender would not open", status, err);
    }
    named = zu_appender_col_name(app, 0, &named_len);
    if (zu_appender_cols(app, &cols) != ZU_OK || cols != 2 || named == NULL ||
        strcmp(named, "age") != 0 || named_len != 3) {
      zu_appender_free(app);
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return fail("an appender named its columns something else");
    }
    /* A value the column will not take is refused where it was
     * appended, with the row it was in taken back off, so what the loop
     * writes next is a row and not the tail of a broken one. */
    err = NULL;
    if (zu_append_str_z(app, "thirty", &err) != ZU_MISUSE || err == NULL) {
      zu_appender_free(app);
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return fail("a string went into a column of integers");
    }
    zu_error_free(err);
    err = NULL;
    for (i = 0; i < 2; i++) {
      if (status == ZU_OK) {
        status = zu_append_i64(app, ages[i], &err);
      }
      if (status == ZU_OK) {
        status = zu_append_str_z(app, names[i], &err);
      }
      if (status == ZU_OK) {
        status = zu_append_end_row(app, &err);
      }
    }
    if (status != ZU_OK) {
      zu_appender_free(app);
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return report("a row would not append", status, err);
    }
    /* Buffered until they are asked for: nothing is in the table yet,
     * and the close is the one commit that puts both rows there. */
    if (zu_appender_buffered(app, &buffered) != ZU_OK || buffered != 2) {
      zu_appender_free(app);
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return fail("rows were appended and not buffered");
    }
    status = zu_appender_close(app, &committed, &err);
    if (status != ZU_OK || committed != 2) {
      zu_appender_free(app);
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return report("an appender would not close", status, err);
    }
    /* Spent, and saying so rather than buffering into a handle the host
     * forgot to free. */
    if (zu_append_i64(app, 60, NULL) != ZU_MISUSE_CLOSED) {
      zu_appender_free(app);
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return fail("a closed appender took another value");
    }
    zu_appender_free(app);
    zu_conn_close(writer);

    status = zu_open_z(appended, &writer, &err);
    if (status != ZU_OK) {
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return report("the appended database did not open", status, err);
    }
    status = zu_query_z(writer, "MATCH (p:person) RETURN p.age AS a ORDER BY a", &back, &err);
    if (status != ZU_OK) {
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return report("the appended database did not read", status, err);
    }
    if (zu_result_rows(back) != 3 || zu_result_col_i64(back, 0, &read) != ZU_OK || read == NULL ||
        read[0] != 30 || read[1] != 40 || read[2] != 50) {
      zu_result_free(back);
      zu_conn_close(writer);
      remove(appended);
      free(appended);
      zu_conn_close(first);
      return fail("a row was appended and came back as something else");
    }
    zu_result_free(back);
    zu_conn_close(writer);
    remove(appended);
    free(appended);
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
  /* The same place as an index into the text, and the line that place
   * is on, which is what makes the column usable by a caller that no
   * longer holds the statement. */
  uint32_t offset = 99;
  if (zu_error_offset(err, &offset) != ZU_OK || offset != 0) {
    zu_error_free(err);
    zu_conn_close(first);
    return fail("a syntax error that will not say where in bytes");
  }
  size_t excerpt_len = 0;
  const char *excerpt = zu_error_excerpt(err, &excerpt_len);
  if (excerpt == NULL || strcmp(excerpt, "NOT A QUERY") != 0 ||
      excerpt_len != strlen(excerpt)) {
    zu_error_free(err);
    zu_conn_close(first);
    return fail("a syntax error that will not quote its line");
  }
  /* The standard's words, the page they are written up on, and the one
   * question a retry loop asks. */
  if (zu_error_standard_text(err, NULL) == NULL ||
      zu_error_doc_url(err, NULL) == NULL || zu_error_retryable(err) != 0) {
    zu_error_free(err);
    zu_conn_close(first);
    return fail("a syntax error with no standard name, page, or verdict");
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
  offset = 11;
  if (zu_error_offset(err, &offset) != ZU_DONE || offset != 11 ||
      zu_error_excerpt(err, NULL) != NULL) {
    zu_error_free(err);
    zu_conn_close(first);
    return fail("an error with no place quoted a line anyway");
  }
  zu_error_free(err);

  /* The other half of the GQLSTATUS envelope, which is the half a host
   * reading rows and errors could not see: what a statement that worked
   * completed with, and what it raised on the way and carried on
   * through. The warning is the one an optional group that misses
   * raises, because avg() then has a null argument on those rows and
   * ignores it, and the point of checking it here is that the answer
   * survived: three is three whether or not anything was said about
   * it. */
  err = NULL;
  result = NULL;
  status = zu_query_z(first,
                      "MATCH (a) OPTIONAL MATCH (a)->(b) WHERE b.id > 2 RETURN avg(b.id) AS x",
                      &result, &err);
  if (status != ZU_OK) {
    zu_conn_close(first);
    return report("an aggregate over an optional group failed", status, err);
  }
  {
    size_t code_len = 0;
    const char *completion = zu_result_gqlstatus(result, &code_len);
    zu_error *notice = NULL;
    if (completion == NULL || strcmp(completion, "00000") != 0 || code_len != 5) {
      zu_result_free(result);
      zu_conn_close(first);
      return fail("a statement that answered with columns said otherwise");
    }
    if (zu_result_rows(result) != 1 || zu_result_notices(result) != 1) {
      zu_result_free(result);
      zu_conn_close(first);
      return fail("a warning is not an exception and the rows are still rows");
    }
    if (zu_result_notice(result, 0, &notice) != ZU_OK || notice == NULL ||
        zu_error_code(notice, NULL) == NULL ||
        strcmp(zu_error_code(notice, NULL), "01G11") != 0 ||
        zu_error_severity(notice) != ZU_SEVERITY_WARNING ||
        zu_error_status(notice) != ZU_OK) {
      zu_error_free(notice);
      zu_result_free(result);
      zu_conn_close(first);
      return fail("a notice that is not the condition that was raised");
    }
    /* A copy rather than a borrow, so it is freed here on the same
     * terms as a failure and the result still has its own. */
    zu_error_free(notice);
    notice = (zu_error *)1;
    if (zu_result_notice(result, 1, &notice) != ZU_DONE || notice != NULL) {
      zu_result_free(result);
      zu_conn_close(first);
      return fail("a notice past the end was handed out");
    }
    zu_result_free(result);
  }
  /* Cancellation and progress, as far as one thread and four nodes can
   * take them: the symbols are exported, the header's function-pointer
   * type is the one the library was built with, a period of nothing is
   * refused, and a statement watched by a callback answers exactly what
   * the same statement answered unwatched. Stopping one mid-run needs a
   * second thread and a graph worth the wait, and lives in the Rust
   * test beside this file. */
  uint64_t rows_read = 0;
  unsigned long ticks = 0;
  const int64_t *watched = NULL;
  if (zu_conn_set_progress(first, ticked, &ticks, 1) != ZU_OK ||
      zu_conn_set_progress(first, ticked, &ticks, 0) != ZU_MISUSE ||
      zu_conn_interrupt(NULL) != ZU_MISUSE) {
    zu_conn_close(first);
    return fail("the progress arrangement was made wrongly and taken");
  }
  /* An ask raised while nothing is running is dropped when the next
   * statement starts, which is what keeps a Ctrl-C at a prompt from
   * ending whatever the user types next. */
  if (zu_conn_interrupt(first) != ZU_OK) {
    zu_conn_close(first);
    return fail("a connection with nothing running refused to be asked");
  }
  err = NULL;
  result = NULL;
  status = zu_query_z(first, "MATCH (a) RETURN count(a) AS n", &result, &err);
  if (status != ZU_OK) {
    zu_conn_close(first);
    return report("a watched statement failed", status, err);
  }
  if (zu_result_col_i64(result, 0, &watched) != ZU_OK || watched == NULL || watched[0] != 4) {
    zu_result_free(result);
    zu_conn_close(first);
    return fail("a watched statement answered something else");
  }
  zu_result_free(result);
  if (zu_conn_rows_read(first, &rows_read) != ZU_OK || rows_read == 0) {
    zu_conn_close(first);
    return fail("nothing was read by a statement that read the whole graph");
  }
  if (zu_conn_set_progress(first, NULL, NULL, 0) != ZU_OK) {
    zu_conn_close(first);
    return fail("a progress arrangement could not be taken back");
  }

  /* Transactions, as far as a database this test was handed can take
   * them: the four symbols are exported, the flag says what the
   * boundaries did to it, and the two conditions a host meets by
   * accident come back as conditions. What a transaction keeps and
   * what a rollback unmakes is the Rust test beside this file, because
   * this one is given somebody else's database and writing into it is
   * not this test's business. */
  int running = -1;
  if (zu_conn_in_transaction(first, &running) != ZU_OK || running != 0) {
    zu_conn_close(first);
    return fail("a connection with nothing running said it was in a transaction");
  }
  err = NULL;
  status = zu_begin(first, 1, &err);
  if (status != ZU_OK) {
    zu_conn_close(first);
    return report("a read only transaction would not begin", status, err);
  }
  if (zu_conn_in_transaction(first, &running) != ZU_OK || running != 1) {
    zu_conn_close(first);
    return fail("a transaction began and the flag did not say so");
  }
  /* Beginning inside one is a condition rather than a nesting, and the
   * transaction that is running is left alone by the refusal. */
  err = NULL;
  if (zu_begin(first, 0, &err) != ZU_ERROR || err == NULL) {
    zu_conn_close(first);
    return fail("a transaction nested");
  }
  zu_error_free(err);
  err = NULL;
  if (zu_commit(first, &err) != ZU_OK) {
    zu_conn_close(first);
    return report("a read only transaction would not commit", status, err);
  }
  /* Ending nothing is 2D000 rather than a call that quietly did
   * nothing, on both words. */
  err = NULL;
  if (zu_commit(first, &err) != ZU_ERROR || err == NULL) {
    zu_conn_close(first);
    return fail("a commit with no transaction running was let through");
  }
  zu_error_free(err);
  err = NULL;
  if (zu_rollback(first, &err) != ZU_ERROR || err == NULL) {
    zu_conn_close(first);
    return fail("a rollback with no transaction running was let through");
  }
  zu_error_free(err);
  if (zu_conn_in_transaction(first, &running) != ZU_OK || running != 0) {
    zu_conn_close(first);
    return fail("a committed transaction is still running");
  }

  /* Columns of the host's own memory, named as a table of this
   * connection and read where they lie. Nothing here reaches the
   * database the test was handed: a frame is a table of the connection
   * and of nothing on disk. */
  {
    struct lent *mine = (struct lent *)malloc(sizeof(struct lent));
    if (mine == NULL) {
      zu_conn_close(first);
      return fail("no memory for a frame to point at");
    }
    mine->ns[0] = 10;
    mine->ns[1] = 20;
    mine->ns[2] = 30;
    mine->scores[0] = 1.5;
    mine->scores[1] = 2.5;
    mine->scores[2] = 3.5;
    mine->ends[0] = 0;
    mine->ends[1] = 3;
    mine->ends[2] = 6;
    mine->ends[3] = 9;
    memcpy(mine->names, "annbobcat", 9);

    zu_frame *frame = NULL;
    err = NULL;
    status = zu_frame_new_z("people", 3, mine, give_back, &frame, &err);
    if (status != ZU_OK || frame == NULL) {
      free(mine);
      zu_conn_close(first);
      return report("a frame would not begin", status, err);
    }
    err = NULL;
    status = zu_frame_col_int(frame, "n", 1, mine->ns, 3, 64, 1, 1, ZU_FRAME_PLAIN, &err);
    if (status == ZU_OK) {
      status = zu_frame_col_float(frame, "score", 5, mine->scores, 3, 64, &err);
    }
    if (status == ZU_OK) {
      status = zu_frame_col_str(frame, "name", 4, mine->ends, 0, mine->names, sizeof mine->names, 3,
                                &err);
    }
    if (status != ZU_OK) {
      zu_frame_free(frame);
      zu_conn_close(first);
      return report("a column the host holds was refused", status, err);
    }
    err = NULL;
    status = zu_conn_register(first, frame, &err);
    if (status != ZU_OK) {
      zu_frame_free(frame);
      zu_conn_close(first);
      return report("a frame would not register", status, err);
    }
    /* The description has been read and the registration holds the
     * arrays now, so the handle goes here and the memory stays. */
    zu_frame_free(frame);

    const int64_t *totals = NULL;
    result = NULL;
    err = NULL;
    status = zu_query_z(first, "MATCH (p:people) RETURN sum(p.n) AS n", &result, &err);
    if (status != ZU_OK) {
      zu_conn_close(first);
      return report("a registered frame would not answer a statement", status, err);
    }
    if (zu_result_rows(result) != 1 || zu_result_col_i64(result, 0, &totals) != ZU_OK ||
        totals == NULL || totals[0] != 60) {
      zu_result_free(result);
      zu_conn_close(first);
      return fail("a frame answered with something other than what the host holds");
    }
    zu_result_free(result);

    /* Written through the host's own pointer between two statements.
     * The second answers with the new value, which nothing that took a
     * copy at registration could do. */
    mine->ns[0] = 1000;
    totals = NULL;
    result = NULL;
    err = NULL;
    status = zu_query_z(first, "MATCH (p:people) RETURN sum(p.n) AS n", &result, &err);
    if (status != ZU_OK) {
      zu_conn_close(first);
      return report("a frame stopped answering after the host wrote into it", status, err);
    }
    if (zu_result_rows(result) != 1 || zu_result_col_i64(result, 0, &totals) != ZU_OK ||
        totals == NULL || totals[0] != 1050) {
      zu_result_free(result);
      zu_conn_close(first);
      return fail("a frame was read from a copy rather than where it lies");
    }
    zu_result_free(result);

    /* The strings are the host's bytes too: only the offsets and the
     * lengths crossed. */
    result = NULL;
    err = NULL;
    status = zu_query_z(first, "MATCH (p:people) WHERE p.n = 30 RETURN p.name AS name", &result,
                        &err);
    if (status != ZU_OK) {
      zu_conn_close(first);
      return report("a frame's strings would not read", status, err);
    }
    {
      const zu_value *held = NULL;
      const char *text = NULL;
      size_t text_len = 0;
      if (zu_result_rows(result) != 1 || zu_result_cell(result, 0, 0, &held) != ZU_OK ||
          zu_value_str(held, &text, &text_len) != ZU_OK || text_len != 3 ||
          memcmp(text, "cat", 3) != 0) {
        zu_result_free(result);
        zu_conn_close(first);
        return fail("a frame's strings came back as something else");
      }
    }
    zu_result_free(result);

    /* What is registered, counted and then read, which is the order
     * that keeps every borrowed name good until the walk ends. */
    uint64_t registered = 0;
    size_t listed_len = 0;
    const char *listed = NULL;
    if (zu_conn_registered_count(first, &registered) != ZU_OK || registered != 1) {
      zu_conn_close(first);
      return fail("one registered frame was counted as something else");
    }
    listed = zu_conn_registered_name(first, 0, &listed_len);
    if (listed == NULL || listed_len != 6 || memcmp(listed, "people", 6) != 0) {
      zu_conn_close(first);
      return fail("a registered frame was listed under another name");
    }
    if (zu_conn_registered_name(first, 1, NULL) != NULL) {
      zu_conn_close(first);
      return fail("a frame was listed past the end of the list");
    }

    if (handed_back != 0) {
      zu_conn_close(first);
      return fail("a frame's memory was handed back while a table still named it");
    }
    int32_t dropped = -1;
    err = NULL;
    status = zu_conn_unregister_z(first, "people", &dropped, &err);
    if (status != ZU_OK || dropped != 1) {
      zu_conn_close(first);
      return report("a registered frame would not drop", status, err);
    }
    /* The last table naming those arrays has gone, so the host has
     * them back, once, and mine is not to be touched again. */
    if (handed_back != 1) {
      zu_conn_close(first);
      return fail("a frame's memory was not handed back when the last table naming it went");
    }
    if (zu_conn_registered_count(first, &registered) != ZU_OK || registered != 0) {
      zu_conn_close(first);
      return fail("a dropped frame is still registered");
    }
    /* Dropping one that is not there is a no rather than a failure. */
    dropped = -1;
    err = NULL;
    status = zu_conn_unregister_z(first, "people", &dropped, &err);
    if (status != ZU_OK || dropped != 0) {
      zu_conn_close(first);
      return report("dropping a frame that was not there was not a plain no", status, err);
    }
  }

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
      "nested list, one load, one append, one warning carried alongside its rows, one watched "
      "statement, one transaction, one frame read where it lies and handed back once, one refusal "
      "with a place and one without\n",
      version);
  return 0;
}
