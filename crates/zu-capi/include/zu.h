/* libzu: C API for the zu embedded property-graph database.
 *
 * A zu_database is a path and a configuration that have been checked
 * against a real file. It holds no descriptor and no cache, so it is
 * thread-safe and shareable. A zu_conn is the state that cannot be
 * shared: a file handle, the caches, and the plans compiled against a
 * catalog. A host that queries from four threads opens one database and
 * connects four times.
 *
 * A connection may move between threads but must not be used from two
 * at once; a call that finds one already in use answers
 * ZU_MISUSE_CONCURRENT rather than corrupting a cache. Statements
 * belong to the connection they were prepared on, and using one after
 * that connection closes answers ZU_MISUSE_CLOSED rather than following
 * a dangling pointer. Results own their rows outright, so a result
 * stays readable after its connection has gone back to a pool.
 *
 * Every pointer an accessor returns (column names, column buffers, cell
 * strings) stays valid exactly until zu_result_free on the result that
 * produced it. Every *_free and *_close call here is a no-op on NULL.
 *
 * Every fallible call returns a zu_status and writes what it produced
 * through an out-parameter, because one returned pointer cannot say
 * both "this failed" and "this succeeded and there is nothing here".
 * The out-parameter is written on every path, NULL when there is
 * nothing to point at, so a caller who ignores the status is never
 * left holding a pointer from the call before.
 *
 * What a user reads comes back separately. The calls that can fail for
 * a reason the engine has something to say about take a zu_error ** as
 * their last parameter; on anything but ZU_OK they write a handle
 * there, which the caller reads through the zu_error_* accessors and
 * releases with zu_error_free. Passing NULL for that parameter
 * discards the error and keeps the status. The accessors below take no
 * error handle: their failures are structural, and the status names
 * each one exactly.
 *
 * Strings cross this boundary as a pointer and a length, since most
 * source languages have counted strings and a NUL-terminated parameter
 * makes every one of them copy a string that already knew how long it
 * was. Each of those calls has a _z variant for a caller who genuinely
 * has a C string. A NULL pointer with a zero length is the empty
 * string, not an error.
 */
#ifndef ZU_H
#define ZU_H

#include <stddef.h>
#include <stdint.h>

/* The revision of this ABI (dx/02 section 8), which is what a build
 * system tests when it has to compile one way against 0.5 and another
 * against what comes next. `cargo xtask package` holds it to the
 * constant the rest of the workspace reports, `zu version` included,
 * so a header and a binary that disagree is a failed check rather than
 * a caller's afternoon. */
#define ZU_ABI_VERSION "0.5"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct zu_database zu_database;
typedef struct zu_conn zu_conn;
typedef struct zu_stmt zu_stmt;
typedef struct zu_result zu_result;
typedef struct zu_error zu_error;
/* One cell of a result, borrowed from it. Not a handle: nothing to
 * free, and it lives exactly as long as the result does. */
typedef struct zu_value zu_value;

/* The name a connection had before the database was split out of it.
 * Kept for one release, along with zu_close below, so that code written
 * against v0 still compiles; both go at the freeze. */
typedef zu_conn zu_session;

/* What a call answers. The GQLSTATUS condition a user reads is on the
 * error handle, not here, which is what keeps this from growing a
 * value per condition. Values are fixed; new ones are appended, never
 * inserted.
 *
 * The gaps that remain are held for the rest of the set dx/02 §6 names
 * and nothing produces yet: 1 for ZU_ROW, 7 for ZU_INTERRUPTED, and 12
 * for ZU_OOM. Reserving the numbers is free, and it is what lets those
 * land beside the value they belong with instead of at the end because
 * the end was where there was room. */
typedef enum zu_status {
  /* The call did what it was asked and wrote its out-parameter. */
  ZU_OK = 0,
  /* Well formed, and there is nothing to read: a column of a result
   * with no rows. The out-parameter is NULL. This is the case a
   * returned NULL could not tell apart from failure. */
  ZU_DONE = 2,
  /* The engine refused the work; the error handle says why. */
  ZU_ERROR = 3,
  /* The caller broke the contract in this header: a NULL handle, an
   * index out of range, an accessor asked for a column that does not
   * hold what it reads, or a string that is not UTF-8. Nothing was
   * done, and nothing is wrong with the database. */
  ZU_MISUSE = 4,
  /* Two threads used one connection at once. Nothing was done. Connect
   * again rather than share. */
  ZU_MISUSE_CONCURRENT = 5,
  /* A statement was used after its connection closed. Nothing was
   * done; the statement handle is still safe to close. */
  ZU_MISUSE_CLOSED = 6,
  /* A write lost to a concurrent one. */
  ZU_CONFLICT = 8,
  /* The file says something that cannot be true. */
  ZU_CORRUPT = 9,
  /* Not implemented in this build, as against declined. */
  ZU_UNSUPPORTED = 10,
  /* The operating system refused a read or a write. */
  ZU_IO = 11
} zu_status;

/* Severity, from zu_error_severity. */
#define ZU_SEVERITY_SUCCESS 0
#define ZU_SEVERITY_NO_DATA 1
#define ZU_SEVERITY_WARNING 2
#define ZU_SEVERITY_INFORMATIONAL 3
#define ZU_SEVERITY_EXCEPTION 4

/* Cell type tags from zu_result_cell_type. */
#define ZU_TYPE_NULL 0
#define ZU_TYPE_BOOL 1
#define ZU_TYPE_INT 2
#define ZU_TYPE_FLOAT 3
#define ZU_TYPE_STR 4
#define ZU_TYPE_NODE 5
#define ZU_TYPE_REL 6
#define ZU_TYPE_LIST 7
#define ZU_TYPE_PATH 8
#define ZU_TYPE_TEMPORAL 9
#define ZU_TYPE_RECORD 10

/* Which temporal a temporal cell is, from zu_value_temporal. The unit
 * follows the kind: days for a date, months for a year-month duration,
 * nanoseconds for the other five. One tag rather than a type per arm,
 * because a host that reads temporals reads all of them and a switch
 * over seven is the shape it wants. */
#define ZU_TEMPORAL_DATE 0
#define ZU_TEMPORAL_LOCAL_TIME 1
#define ZU_TEMPORAL_ZONED_TIME 2
#define ZU_TEMPORAL_LOCAL_DATETIME 3
#define ZU_TEMPORAL_ZONED_DATETIME 4
#define ZU_TEMPORAL_DURATION_YEAR_MONTH 5
#define ZU_TEMPORAL_DURATION_DAY_TIME 6

/* Static version string; do not free. */
const char *zu_version(void);

/* Errors. An error carries the status its call returned, the GQLSTATUS
 * code, the severity, and the message, as fields rather than as one
 * string to parse: the code picks which exception class a binding
 * raises and the severity decides whether it raises at all. The
 * strings live until zu_error_free, and each len out-parameter may be
 * NULL. zu_error_code is NULL for a failure that carries no condition,
 * which a binding mapping codes to classes has to tell from a code it
 * does not know. */
zu_status zu_error_status(const zu_error *e);
const char *zu_error_message(const zu_error *e, size_t *len);
const char *zu_error_code(const zu_error *e, size_t *len);
int32_t zu_error_severity(const zu_error *e); /* -1 for a NULL error */
void zu_error_free(zu_error *e);

/* How a database is opened. The only struct that crosses this boundary
 * by value, and it does so because it is versioned: struct_size comes
 * first, the caller sets it with zu_config_init, and every field after
 * it is read only when that size says the caller's struct is long
 * enough to hold it. A field appended later is therefore invisible to a
 * binding compiled against this header, rather than fatal to it.
 *
 * Zero means the default in every field, so a zeroed struct with
 * struct_size set opens the same database as a NULL config. */
typedef struct zu_config {
  size_t struct_size;   /* sizeof(zu_config); set this */
  size_t memory_limit;  /* bytes the caches may hold; 0 for the default */
  size_t threads;       /* query workers; 0 to let the executor pick, 1 for sequential */
  int32_t read_only;    /* nonzero opens a descriptor this process cannot write through */
} zu_config;

zu_status zu_config_init(zu_config *cfg);

/* Sets one option by name, so a binding can forward a user's option map
 * without this ABI growing a setter per option and without the binding
 * hard-coding a layout it would have to keep in step. Keys are
 * memory_limit, threads, and read_only. The first two take a decimal
 * count; a suffix such as MB is deliberately not parsed here, because
 * its two readings differ by 4.9% and the language the user typed it in
 * is a better place to decide which they meant. read_only takes true,
 * false, 1, or 0. An unrecognized key is refused and named. */
zu_status zu_config_set(zu_config *cfg, const char *key, size_t key_len, const char *value,
                        size_t value_len, zu_error **err);
zu_status zu_config_set_z(zu_config *cfg, const char *key, const char *value, zu_error **err);

/* Database lifecycle. cfg may be NULL for the defaults. The file is
 * opened once here and closed again, so a path that is not a zu1 file
 * fails now rather than on the first connection. Closing a database
 * does not close the connections opened from it: each holds its own
 * file handle, and this releases only the path and the configuration. */
zu_status zu_database_open(const char *path, size_t path_len, const zu_config *cfg,
                           zu_database **out, zu_error **err);
zu_status zu_database_open_z(const char *path, const zu_config *cfg, zu_database **out,
                             zu_error **err);
zu_status zu_database_path(const zu_database *db, const char **out, size_t *len);
void zu_database_close(zu_database *db);

/* Connection lifecycle. A connection keeps the catalog, statistics,
 * plan cache, and block caches resident, so queries after the first run
 * without touching the catalog on disk. That is also why it is per
 * connection rather than per database, and why a pool calls zu_connect
 * once per worker instead of sharing one.
 *
 * zu_open is the convenience for a host that wants exactly one: it
 * opens a database with the default configuration, connects once, and
 * returns the connection. Nothing outlives the database it discards,
 * since the connection carries its own file handle.
 *
 * Closing is itself a use of the connection and obeys the same rule as
 * every other one. */
zu_status zu_connect(zu_database *db, zu_conn **out, zu_error **err);
zu_status zu_open(const char *path, size_t path_len, zu_conn **out, zu_error **err);
zu_status zu_open_z(const char *path, zu_conn **out, zu_error **err);
void zu_conn_close(zu_conn *conn);
void zu_close(zu_conn *conn); /* the old name; goes at the freeze */

/* One-shot statement without parameters. */
zu_status zu_query(zu_conn *conn, const char *q, size_t q_len, zu_result **out, zu_error **err);
zu_status zu_query_z(zu_conn *conn, const char *q, zu_result **out, zu_error **err);

/* Prepared statements. Bindings live on the statement and survive
 * zu_execute, so a loop rebinds only what changed. Binding a name
 * again replaces its value. The bind calls return ZU_MISUSE for a NULL
 * statement or a name that is not UTF-8, ZU_MISUSE_CLOSED once the
 * connection has closed, and take no error handle because that is all
 * they can say. */
zu_status zu_prepare(zu_conn *conn, const char *q, size_t q_len, zu_stmt **out, zu_error **err);
zu_status zu_prepare_z(zu_conn *conn, const char *q, zu_stmt **out, zu_error **err);
zu_status zu_bind_i64(zu_stmt *stmt, const char *name, size_t name_len, int64_t v);
zu_status zu_bind_i64_z(zu_stmt *stmt, const char *name, int64_t v);
zu_status zu_bind_f64(zu_stmt *stmt, const char *name, size_t name_len, double v);
zu_status zu_bind_f64_z(zu_stmt *stmt, const char *name, double v);
zu_status zu_bind_str(zu_stmt *stmt, const char *name, size_t name_len, const char *v,
                      size_t v_len);
zu_status zu_bind_str_z(zu_stmt *stmt, const char *name, const char *v);
zu_status zu_bind_null(zu_stmt *stmt, const char *name, size_t name_len);
zu_status zu_bind_null_z(zu_stmt *stmt, const char *name);
zu_status zu_execute(zu_stmt *stmt, zu_result **out, zu_error **err);
void zu_stmt_close(zu_stmt *stmt);

/* Result shape. The two counts are 0 for a NULL result, which is the
 * same answer as an empty one and needs no status. */
uint64_t zu_result_rows(const zu_result *result);
uint32_t zu_result_cols(const zu_result *result);
zu_status zu_result_col_name(const zu_result *result, uint32_t col, const char **out,
                             size_t *len);
/* The ZU_TYPE_* tag of one cell, or -1 out of range: every tag is a
 * type a cell can hold, so the failure has to be a value that is not
 * one of them. */
zu_status zu_result_cell_type(const zu_result *result, uint64_t row, uint32_t col, int32_t *out);

/* Columnar reads: the whole column in one call, contiguous, owned by
 * the result and valid until zu_result_free. ZU_DONE with *out NULL
 * when the result has no rows, ZU_MISUSE when the column is out of
 * range or holds something the accessor does not read.
 *
 * col_i64 reads ints and bools, col_f64 reads floats and ints, and
 * col_node_offset reads the row offset that identifies a node. Nulls
 * read 0 in all three, which col_valid tells apart. A node is not an
 * integer here: reading one as its offset is what col_node_offset is
 * for, and doing it quietly through col_i64 is how a binding ends up
 * handing an internal row number to a user who asked for an identity. */
zu_status zu_result_col_i64(zu_result *result, uint32_t col, const int64_t **out);
zu_status zu_result_col_f64(zu_result *result, uint32_t col, const double **out);
zu_status zu_result_col_node_offset(zu_result *result, uint32_t col, const uint64_t **out);
zu_status zu_result_col_valid(zu_result *result, uint32_t col, const uint8_t **out);

/* Chunked reads: the same columns, a chunk of rows at a time.
 *
 * Which one to use is a question of size. A point read wants the whole
 * column, because the answer is small and one call beats a loop. Every
 * large answer wants chunks, because the whole-column call converts all
 * of it before returning any of it, and keeps the conversion until the
 * result is freed: a million-row int column is eight megabytes of
 * buffer beyond the rows, and reading the first hundred rows and
 * stopping pays for the other 999,900. A chunked read converts the
 * chunk asked for, into a buffer of a fixed size that the next chunk
 * reuses.
 *
 * That is the trade: a chunk pointer is valid until the next call for
 * the same column and the same accessor, which replaces its contents,
 * or until zu_result_free. A host that needs one chunk to outlive the
 * next copies it, which is the copy it was making anyway on the way
 * into a host array. Columns are independent of each other, so reading
 * a chunk's values and its validity together costs no reconversion.
 *
 * zu_result_chunk_count is the loop bound, and it is 0 for a result
 * with no rows, which is why nothing here answers ZU_DONE. Ask each
 * chunk its size rather than multiplying: chunks are the same size
 * today except the last, and will stop being once a chunk is what the
 * executor produced rather than a slice of what it materialized. The
 * offset turns a chunk row back into the row number the cell accessors
 * take, which is how a string column is read beside a chunked one.
 *
 * ZU_MISUSE when the chunk or the column is out of range, or the
 * column holds something the accessor does not read. */
uint64_t zu_result_chunk_count(const zu_result *result);
zu_status zu_result_chunk(const zu_result *result, uint64_t chunk, uint64_t *offset,
                          uint64_t *rows);
zu_status zu_result_chunk_col_i64(zu_result *result, uint64_t chunk, uint32_t col,
                                  const int64_t **out);
zu_status zu_result_chunk_col_f64(zu_result *result, uint64_t chunk, uint32_t col,
                                  const double **out);
zu_status zu_result_chunk_col_node_offset(zu_result *result, uint64_t chunk, uint32_t col,
                                          const uint64_t **out);
zu_status zu_result_chunk_col_valid(zu_result *result, uint64_t chunk, uint32_t col,
                                    const uint8_t **out);

/* One string cell, NUL-terminated, with its byte length through len
 * when that is non-NULL. ZU_MISUSE when the cell is out of range or is
 * not a string. */
zu_status zu_result_cell_str(zu_result *result, uint64_t row, uint32_t col, const char **out,
                             size_t *len);

/* Cells one at a time, for the values that have no column to be read
 * into. A temporal is a count and a unit, a list recurses, a node is a
 * table and an offset, and none of the three fits an int64_t *. The
 * columnar accessors above stay the path a bulk read takes; this is the
 * path a value takes that they cannot express.
 *
 * zu_result_cell hands back a pointer into the result's own rows, so it
 * allocates nothing and stays valid until zu_result_free, exactly like
 * every other pointer here. There is no zu_value_free.
 *
 * These read a value as the type it is and nothing else, which is where
 * they differ from the columns: zu_result_col_i64 reads bools and nulls
 * too, because a column is one host array and something has to go in
 * every slot, while zu_value_i64 on a bool answers ZU_MISUSE. Each
 * writes its out-parameters on every path, so a caller that ignores the
 * status reads a zero rather than the call before.
 *
 * zu_value_type returns the tag directly, and -1 for a NULL pointer:
 * every tag is a type a cell can hold, so the failure has to be a value
 * that is not one of them. zu_value_len is 0 for anything that is not a
 * list, a path or a record, an empty list included, which is the same
 * answer zu_result_rows gives and needs no status for the same reason.
 *
 * zu_value_str and zu_value_field point into the result's bytes and are
 * NOT NUL-terminated; the length is the whole of the answer, and their
 * len parameter may not be NULL. That is the price of not copying, and
 * a string inside a list has no row and column to be cached under.
 * zu_result_cell_str above is the NUL-terminated form, for a top-level
 * cell, and it keeps the copy it makes. */
zu_status zu_result_cell(const zu_result *result, uint64_t row, uint32_t col,
                         const zu_value **out);
int32_t zu_value_type(const zu_value *v);
zu_status zu_value_bool(const zu_value *v, int32_t *out);
zu_status zu_value_i64(const zu_value *v, int64_t *out);
zu_status zu_value_f64(const zu_value *v, double *out);
zu_status zu_value_str(const zu_value *v, const char **out, size_t *len);
/* kind and count are required; offset may be NULL for a host with no
 * zoned type, and is minutes east of UTC, 0 for the five kinds that
 * carry none. */
zu_status zu_value_temporal(const zu_value *v, int32_t *kind, int64_t *count, int32_t *offset);
/* Both parts, because neither identifies a node on its own: two tables
 * number their rows from zero. Either out-parameter may be NULL. */
zu_status zu_value_node(const zu_value *v, uint32_t *table, uint64_t *offset);
zu_status zu_value_rel(const zu_value *v, uint32_t *table, uint64_t *src, uint64_t *dst);
uint64_t zu_value_len(const zu_value *v);
zu_status zu_value_at(const zu_value *v, uint64_t i, const zu_value **out);
/* A record's fields are in name order and a name appears once, which is
 * what makes two records written in different orders one value. */
zu_status zu_value_field(const zu_value *v, uint64_t i, const char **out, size_t *len);

void zu_result_free(zu_result *result);

#ifdef __cplusplus
}
#endif

#endif /* ZU_H */
