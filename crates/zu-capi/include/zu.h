/* libzu: C API for the zu embedded property-graph database.
 *
 * One session per thread; sessions, statements, and results are not
 * thread-safe. A statement must be closed before its session, and a
 * result outlives none of them: every pointer an accessor returns
 * (column names, column buffers, cell strings) stays valid exactly
 * until zu_result_free on the result that produced it.
 *
 * Calls that can fail take a `char **err`. On failure they return NULL
 * and, when err is non-NULL, set *err to a message the caller releases
 * with zu_string_free. Passing err as NULL discards the message.
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

/* Cell type tags returned by zu_result_cell_type. */
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

/* Session lifecycle. The session keeps the catalog, statistics, plan
 * cache, and block caches resident, so queries after the first run
 * without touching the catalog on disk. */
zu_session *zu_open(const char *path, char **err);
void zu_close(zu_session *session);

/* One-shot statement without parameters. */
zu_result *zu_query(zu_session *session, const char *q, char **err);

/* Prepared statements. Bindings live on the statement and survive
 * zu_execute, so a loop rebinds only what changed. Binding a name
 * again replaces its value. */
zu_stmt *zu_prepare(zu_session *session, const char *q, char **err);
void zu_bind_i64(zu_stmt *stmt, const char *name, int64_t v);
void zu_bind_f64(zu_stmt *stmt, const char *name, double v);
void zu_bind_str(zu_stmt *stmt, const char *name, const char *v);
void zu_bind_null(zu_stmt *stmt, const char *name);
zu_result *zu_execute(zu_stmt *stmt, char **err);
void zu_stmt_close(zu_stmt *stmt);

/* Result shape. */
uint64_t zu_result_rows(const zu_result *result);
uint32_t zu_result_cols(const zu_result *result);
const char *zu_result_col_name(const zu_result *result, uint32_t col);
int32_t zu_result_cell_type(const zu_result *result, uint64_t row, uint32_t col);

/* Columnar reads: the whole column in one call, contiguous, owned by
 * the result. col_i64 accepts int, bool, null (reads 0), and node
 * cells (read their offset); col_f64 accepts floats, ints, and nulls;
 * both return NULL when the column holds anything else. col_valid is
 * one byte per row, 0 where the cell is null. */
const int64_t *zu_result_col_i64(zu_result *result, uint32_t col);
const double *zu_result_col_f64(zu_result *result, uint32_t col);
const uint8_t *zu_result_col_valid(zu_result *result, uint32_t col);

/* One string cell, NUL-terminated; len (when non-NULL) gets the byte
 * length. NULL when the cell is not a string. */
const char *zu_result_cell_str(zu_result *result, uint64_t row, uint32_t col, size_t *len);

void zu_result_free(zu_result *result);
void zu_string_free(char *s);

#ifdef __cplusplus
}
#endif

#endif /* ZU_H */
