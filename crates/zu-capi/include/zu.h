/* libzu: C API for the zu embedded property-graph database.
 *
 * One session per thread; sessions, statements, and results are not
 * thread-safe. A statement must be closed before its session, and a
 * result outlives none of them: every pointer an accessor returns
 * (column names, column buffers, cell strings) stays valid exactly
 * until zu_result_free on the result that produced it. Every *_free
 * and *_close call here is a no-op on NULL.
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

#ifdef __cplusplus
extern "C" {
#endif

typedef struct zu_session zu_session;
typedef struct zu_stmt zu_stmt;
typedef struct zu_result zu_result;
typedef struct zu_error zu_error;

/* What a call answers. The GQLSTATUS condition a user reads is on the
 * error handle, not here, which is what keeps this from growing a
 * value per condition. Values are fixed; new ones are appended, never
 * inserted.
 *
 * The gaps are held for the rest of the set dx/02 §6 names and nothing
 * produces yet: 1 for ZU_ROW, 5 and 6 for the ownership checks
 * (ZU_MISUSE_CONCURRENT, ZU_MISUSE_CLOSED), 7 for ZU_INTERRUPTED, and
 * 12 for ZU_OOM. Reserving the numbers is free, and it is what lets
 * those land beside the misuse value they belong with instead of at
 * the end because the end was where there was room. */
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

/* Session lifecycle. The session keeps the catalog, statistics, plan
 * cache, and block caches resident, so queries after the first run
 * without touching the catalog on disk. */
zu_status zu_open(const char *path, size_t path_len, zu_session **out, zu_error **err);
zu_status zu_open_z(const char *path, zu_session **out, zu_error **err);
void zu_close(zu_session *session);

/* One-shot statement without parameters. */
zu_status zu_query(zu_session *session, const char *q, size_t q_len, zu_result **out,
                   zu_error **err);
zu_status zu_query_z(zu_session *session, const char *q, zu_result **out, zu_error **err);

/* Prepared statements. Bindings live on the statement and survive
 * zu_execute, so a loop rebinds only what changed. Binding a name
 * again replaces its value. The bind calls return ZU_MISUSE for a NULL
 * statement or a name that is not UTF-8, and take no error handle
 * because that is all they can say. */
zu_status zu_prepare(zu_session *session, const char *q, size_t q_len, zu_stmt **out,
                     zu_error **err);
zu_status zu_prepare_z(zu_session *session, const char *q, zu_stmt **out, zu_error **err);
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

/* One string cell, NUL-terminated, with its byte length through len
 * when that is non-NULL. ZU_MISUSE when the cell is out of range or is
 * not a string. */
zu_status zu_result_cell_str(zu_result *result, uint64_t row, uint32_t col, const char **out,
                             size_t *len);

void zu_result_free(zu_result *result);

#ifdef __cplusplus
}
#endif

#endif /* ZU_H */
