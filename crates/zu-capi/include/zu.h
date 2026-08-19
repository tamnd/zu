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
 * system tests when it has to compile one way against 0.11 and another
 * against what comes next. The two numbers are counts and not decimals,
 * so 0.11 is the revision after 0.10 and a caller comparing them
 * compares each on its own. `cargo xtask package` holds it to the
 * constant the rest of the workspace reports, `zu version` included,
 * so a header and a binary that disagree is a failed check rather than
 * a caller's afternoon. */
#define ZU_ABI_VERSION "0.11"

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
/* A database being built. See the bulk load section at the end. */
typedef struct zu_loader zu_loader;
/* Rows on their way into a table that already exists. See the appending
 * section at the end. */
typedef struct zu_appender zu_appender;
/* Columns of the host's own memory, named as a table of a connection
 * and read where they lie. See the frames section at the end. */
typedef struct zu_frame zu_frame;

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
 * and nothing produces yet: 1 for ZU_ROW and 12 for ZU_OOM. Reserving
 * the numbers is free, and it is what lets those land beside the value
 * they belong with instead of at the end because the end was where
 * there was room. */
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
  /* The caller stopped the statement while it was running. Nothing is
   * wrong with the connection and the next call on it runs normally. */
  ZU_INTERRUPTED = 7,
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
/* GV60 and GV61, the two reference values. Neither reads through an
   accessor: a handle has no contents to hand over, so the tag is the
   whole of what a binding can say about the cell. */
#define ZU_TYPE_GRAPH 11
#define ZU_TYPE_BINDING_TABLE 12

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

/* What zu_frame_col_int is told when a column of integers is a column
 * of integers and nothing else. Negative so that the kinds above, which
 * count up from nought and are the rest of what that parameter takes,
 * keep their numbering. */
#define ZU_FRAME_PLAIN (-1)

/* Static version string; do not free. */
const char *zu_version(void);

/* Errors. An error carries the status its call returned, the GQLSTATUS
 * code, the standard's name for it, the severity, whether it is worth
 * retrying, where in the statement it happened, the line that place is
 * on, and the message, as fields rather than as one string to parse:
 * the code picks which exception class a binding raises, the severity
 * decides whether it raises at all, and neither survives being
 * formatted into prose and parsed back out.
 *
 * The strings live until zu_error_free, and each len out-parameter may
 * be NULL. A string a failure does not carry is NULL rather than
 * empty, since no condition and an empty condition are different
 * facts: zu_error_code, zu_error_standard_text and zu_error_doc_url
 * are all NULL for an engine-internal failure, which has no code
 * rather than one that would be a guess.
 *
 * zu_error_message is zu's own account, naming the table, the token or
 * the value. zu_error_standard_text is the standard's words for the
 * condition class and subclass, which is what a conformance harness
 * grades. zu_error_doc_url is where that condition is written up, so a
 * binding hands a reader a page rather than five characters to search
 * for.
 *
 * zu_error_retryable answers 1 when running the same statement again
 * could succeed, 0 when it could not, -1 for a NULL error. A write
 * that lost to a concurrent one is the 1: nothing of it was applied.
 * Text that will not parse is the 0, and so is a statement the caller
 * interrupted, which did not fail so much as stop. A retry loop reads
 * this rather than carrying a list of codes, which is the sort of list
 * that is right in one binding and stale in the other five.
 *
 * zu_error_position writes the line and column the condition was
 * raised at, both 1-based, the column counted in characters so a line
 * of multi-byte text does not read as wider than it looks, and
 * zu_error_offset writes the same place as a 0-based byte index into
 * the statement, for a caller that slices the text rather than
 * printing it. Both answer ZU_OK and write when there is a position,
 * ZU_DONE and write nothing when there is not, and ZU_MISUSE for a
 * NULL error. Not every failure has one: a division by zero happens
 * while the statement runs and has no token to point at, and an io
 * error has no statement at all. Every out-parameter may be NULL. The
 * offset is always on a character boundary, so slicing at it cannot
 * split a character in half.
 *
 * zu_error_excerpt is the line that position is on, without its
 * newline, which the column counts characters into: a caller has both
 * halves of a caret without having kept the statement text. It is NULL
 * when there is no position, when the line is empty, and when the line
 * is longer than anyone would read under a caret, since a line cut to
 * fit would put the column somewhere it is not.
 *
 * The message says all of this in words and keeps saying it, so
 * printing it alone is still a complete report. The fields are for the
 * caller that would rather underline the token than read the numbers
 * back out of the sentence. */
zu_status zu_error_status(const zu_error *e);
const char *zu_error_message(const zu_error *e, size_t *len);
const char *zu_error_code(const zu_error *e, size_t *len);
const char *zu_error_standard_text(const zu_error *e, size_t *len);
const char *zu_error_doc_url(const zu_error *e, size_t *len);
int32_t zu_error_severity(const zu_error *e);  /* -1 for a NULL error */
int32_t zu_error_retryable(const zu_error *e); /* -1 for a NULL error */
zu_status zu_error_position(const zu_error *e, uint32_t *line, uint32_t *column);
zu_status zu_error_offset(const zu_error *e, uint32_t *offset);
const char *zu_error_excerpt(const zu_error *e, size_t *len);
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
/* Creates a database and opens it. The path must not exist: a create
 * that opened what it found there would be the call that quietly writes
 * into somebody else's data, and a host that wants either one has
 * zu_database_open to fall back to and a decision to make about which.
 *
 * What it makes is a valid database with nothing in it, which is what a
 * host has to start from to run any statement at all. Bulk load below
 * makes a database with a table in it, and until this call there was no
 * other way for a C host to have one. */
zu_status zu_database_create(const char *path, size_t path_len, const zu_config *cfg,
                             zu_database **out, zu_error **err);
zu_status zu_database_create_z(const char *path, const zu_config *cfg, zu_database **out,
                               zu_error **err);
/* Creates a database that never touches the filesystem. The blocks a
 * file would hold are held in memory instead, and the log beside it
 * too, so everything above this point runs unchanged and nothing
 * survives the process.
 *
 * Every call makes a database of its own. Two connections on one handle
 * are two views of one graph; two handles share nothing.
 *
 * zu_database_path still answers, with a name that is not a path: it is
 * what this process calls the database, which is what an error message
 * needs and not something to open. zu_database_is_memory is the way to
 * ask rather than to parse that name, and returns ZU_OK for a database
 * in memory and ZU_DONE for one on disk. */
zu_status zu_database_memory(const zu_config *cfg, zu_database **out, zu_error **err);
zu_status zu_database_is_memory(const zu_database *db);
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
/* The same convenience over a database that is not there yet:
 * zu_database_create and one connection on it. */
zu_status zu_create(const char *path, size_t path_len, zu_conn **out, zu_error **err);
zu_status zu_create_z(const char *path, zu_conn **out, zu_error **err);
/* And the same over a database that is nowhere: one scratch graph and
 * one connection on it, which go together when the connection closes. */
zu_status zu_memory(zu_conn **out, zu_error **err);
/* A second connection on the database a connection is already on, made
 * without a path. This is what a pool calls once it has handed the
 * database handle back, and it is the only way to a second connection
 * on a database in memory, which has no path to reopen. The switches
 * and the read-only setting come across; the plan cache, the block
 * caches, the interrupt and the transaction do not, because those are
 * what makes it a connection of its own. */
zu_status zu_conn_duplicate(zu_conn *conn, zu_conn **out, zu_error **err);
void zu_conn_close(zu_conn *conn);
void zu_close(zu_conn *conn); /* the old name; goes at the freeze */

/* Cancellation and progress.
 *
 * zu_conn_interrupt is the one call here meant to be made from another
 * thread while a connection is in use, and it does not answer
 * ZU_MISUSE_CONCURRENT for it: a cancellation that had to wait for the
 * connection to be free could only arrive after the statement it was
 * meant to stop. The statement stops at the next boundary the executor
 * checks, which is a chunk of rows rather than the end of the query,
 * and answers ZU_INTERRUPTED. Nothing failed: the connection keeps its
 * plans and its warm caches and runs the next statement normally, which
 * is the difference between this and closing it.
 *
 * An ask raised while nothing is running is dropped when the next
 * statement starts, so a Ctrl-C at a prompt cannot end whatever the
 * user types next.
 *
 * zu_conn_rows_read is the same watch by polling: how many rows the
 * statement has read out of storage, counted from zero at each
 * statement and left at its final value once one ends. Rows read rather
 * than rows answered, because the statement a user is waiting on is
 * exactly the one reading a hundred million rows to answer one.
 *
 * zu_conn_set_progress asks to be called back every interval_ms while a
 * statement runs, with the rows read and the milliseconds since it
 * started; returning 0 from the callback stops the statement exactly as
 * zu_conn_interrupt would. A NULL callback takes the arrangement back
 * and ignores interval_ms; a callback with an interval of zero is
 * ZU_MISUSE, since a period of nothing is not a period. The
 * arrangement belongs to the connection and covers every statement
 * after it, and a statement already running keeps the one it started
 * with.
 *
 * The callback runs on a thread of this library's, one per statement,
 * never two at once and never after the call it belongs to has
 * returned. It is not called on the thread that asked for the
 * statement, because that thread is inside the executor; what follows
 * from that is that user_data has to be usable from another thread, and
 * that a callback must not call back into this library on the
 * connection it is reporting on. */
zu_status zu_conn_interrupt(zu_conn *conn);
zu_status zu_conn_rows_read(zu_conn *conn, uint64_t *out);
typedef int (*zu_progress_fn)(void *user_data, uint64_t rows, uint64_t ms);
zu_status zu_conn_set_progress(zu_conn *conn, zu_progress_fn cb, void *user_data,
                               uint64_t interval_ms);

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
/* A boolean, as an int: nought is false and anything else is true. */
zu_status zu_bind_bool(zu_stmt *stmt, const char *name, size_t name_len, int v);
zu_status zu_bind_bool_z(zu_stmt *stmt, const char *name, int v);
zu_status zu_bind_str(zu_stmt *stmt, const char *name, size_t name_len, const char *v,
                      size_t v_len);
zu_status zu_bind_str_z(zu_stmt *stmt, const char *name, const char *v);
/* A temporal parameter, as one ZU_TEMPORAL_ kind and the count in the
 * unit that kind implies, which is zu_value_temporal read backwards.
 * The offset is minutes east of UTC and is ignored by every kind but
 * the two zoned ones. A kind that is not one of the seven, or an
 * offset or a count the kind cannot hold, is ZU_MISUSE. */
zu_status zu_bind_temporal(zu_stmt *stmt, const char *name, size_t name_len, int32_t kind,
                           int64_t count, int32_t offset);
zu_status zu_bind_temporal_z(zu_stmt *stmt, const char *name, int32_t kind, int64_t count,
                             int32_t offset);
zu_status zu_bind_null(zu_stmt *stmt, const char *name, size_t name_len);
zu_status zu_bind_null_z(zu_stmt *stmt, const char *name);
zu_status zu_execute(zu_stmt *stmt, zu_result **out, zu_error **err);
void zu_stmt_close(zu_stmt *stmt);

/* Transactions.
 *
 * Every statement outside one is already a transaction of its own, so
 * these do not turn transactions on. What they do is make several
 * statements one: what they wrote is kept by zu_commit or unmade by
 * zu_rollback, and nothing between the two is visible to another
 * connection until the commit publishes it.
 *
 * They are the statements START TRANSACTION, COMMIT and ROLLBACK,
 * called rather than written, which is deliberate on both counts. A
 * host that sends the text gets the same behaviour, because these run
 * that text; and a host that would rather call gets a signature instead
 * of a string to build, which is what a binding wrapping this in a
 * block or a decorator actually needs.
 *
 * zu_begin with a nonzero read_only starts a READ ONLY transaction,
 * which is enforced rather than advisory: a write inside one is 25G03
 * at the statement that wrote, not at the commit. Beginning inside a
 * transaction is 25G01 rather than a nested one, and committing or
 * rolling back with nothing running is 2D000 rather than a call that
 * quietly did nothing, since a host that rolls back in an error path
 * wants to hear that the transaction it meant to undo was not the one
 * it thought.
 *
 * A commit that answers ZU_OK is durable: the log frame is on the disk
 * before the call returns. Closing a connection with a transaction
 * still running rolls it back, which is what a host that failed
 * halfway and dropped everything wants and the only answer that does
 * not depend on a destructor running.
 *
 * zu_conn_in_transaction is the one thing about a transaction that no
 * statement answers, and every host offering a block, a `using` or a
 * `defer` needs it: the cleanup path has to know whether the body
 * already ended the transaction before it tries to. */
zu_status zu_begin(zu_conn *conn, int32_t read_only, zu_error **err);
zu_status zu_commit(zu_conn *conn, zu_error **err);
zu_status zu_rollback(zu_conn *conn, zu_error **err);
zu_status zu_conn_in_transaction(zu_conn *conn, int32_t *out);

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

/* ---- diagnostics ----
 *
 * What a statement that worked has to say for itself, which is the half
 * of the GQLSTATUS envelope a host reading rows and errors could not
 * see. The status a call returns says whether it worked; this says
 * which way, in the standard's own terms.
 *
 * zu_result_gqlstatus is the completion condition: "00000" for a
 * statement that answered with columns, and "00001", successful
 * completion with the result omitted, for one that had none to give
 * back. It is the value the JSON Lines protocol already writes into
 * every record and the value a conformance harness grades. It is never
 * NULL for a result that is not NULL, and it belongs to the result
 * rather than to the caller, so it is good until zu_result_free and is
 * not freed on its own.
 *
 * The notices are the conditions the statement raised and carried on
 * through. An exception replaces a result and arrives as an error; a
 * warning rides along with one, because a statement that dropped a null
 * out of an aggregate still has rows to give you and the standard still
 * wants you told. Almost every statement raises none, so a host that
 * asks and finds nought has paid for one call.
 *
 * A notice comes back as a zu_error, which is one shape rather than
 * two: a diagnostic record is a diagnostic record, the code, its
 * standard text, the severity, the place, the line and the doc page are
 * the same accessors either way, and a binding that already turns one
 * of these into an exception gets its warning class for the cost of
 * reading zu_error_severity. That is what tells them apart, along with
 * zu_error_status, which is ZU_OK here because that is what the call
 * that produced it returned. It is a copy rather than a borrow, so the
 * rule for every zu_error a host is handed stays the one rule: free it.
 * The result keeps its own and can be asked again. */
/* len may be NULL. */
const char *zu_result_gqlstatus(zu_result *result, size_t *len);
uint32_t zu_result_notices(zu_result *result);
/* ZU_DONE with *out NULL past the end, which is what a host walking
 * them gets at the end of the walk rather than a failure. */
zu_status zu_result_notice(zu_result *result, uint32_t ix, zu_error **out);

/* ---- bulk load ----
 *
 * How values get into a database that does not exist yet. CREATE and
 * INSERT need a table and no statement makes one, so a host holding
 * data and an empty file has nowhere else to go. A database that does
 * exist is what the appending section below is for. This is also the
 * entry point the Rust appender and `zu copy` are built on, not a
 * second mechanism beside them.
 *
 * A loader is columnar for the same reason a result is. One call per
 * column, not one per cell.
 *
 * The order is fixed: create, then table, then columns and edges in any
 * order and as many calls as you like, then finish. A column call
 * before zu_loader_table is ZU_MISUSE, and so is a column whose count
 * disagrees with the row count the table was given, checked at the call
 * that passed it rather than at finish so the error names the column
 * while you still know which one you were building.
 *
 * Nothing reaches the file until zu_loader_finish, so a load either
 * happened or did not. The loader copies every array it is given, which
 * means a caller may free or reuse its own buffers as soon as a call
 * returns; the alternative is a lifetime rule this comment could not
 * state safely.
 *
 * A loader is used from one thread, like a connection, and the same
 * check applies: a second thread in a call answers ZU_MISUSE_CONCURRENT
 * rather than corrupting the columns. After finish, and after a finish
 * that failed, every call answers ZU_MISUSE_CLOSED and only
 * zu_loader_free is left.
 *
 * zu_loader_create fails if the path exists, which is what `zu copy`
 * does: a bulk load builds a database rather than adding to one. A
 * loader freed before finish wrote nothing and leaves the empty file it
 * created for the caller to remove. */
zu_status zu_loader_create(const char *path, size_t path_len, zu_loader **out, zu_error **err);
zu_status zu_loader_create_z(const char *path, zu_loader **out, zu_error **err);
/* rows is given rather than counted from the first column, so a column
 * with a value missing is an error and not a shorter table. One table
 * per loader. */
zu_status zu_loader_table(zu_loader *l, const char *nodes, size_t nodes_len, const char *edges,
                          size_t edges_len, uint64_t rows, zu_error **err);
zu_status zu_loader_table_z(zu_loader *l, const char *nodes, const char *edges, uint64_t rows,
                            zu_error **err);
/* Edges as the row each starts at and the row it ends at, two arrays so
 * a host that has them in columns passes what it has. Appends, so call
 * it as often as you like; the loader sorts and deduplicates at finish.
 */
zu_status zu_loader_edges(zu_loader *l, const uint32_t *from, const uint32_t *to, uint64_t count,
                          zu_error **err);
zu_status zu_loader_col_i64(zu_loader *l, const char *name, size_t name_len, const int64_t *values,
                            uint64_t count, zu_error **err);
zu_status zu_loader_col_f64(zu_loader *l, const char *name, size_t name_len, const double *values,
                            uint64_t count, zu_error **err);
/* Any nonzero value is true. int32_t rather than _Bool, because this
 * header is C89-safe and because zu_value_bool writes one out. */
zu_status zu_loader_col_bool(zu_loader *l, const char *name, size_t name_len, const int32_t *values,
                             uint64_t count, zu_error **err);
/* Lengths are a separate array so a caller whose strings are not
 * NUL-terminated passes what it has. Every string is checked for UTF-8
 * here rather than read back later as something no query could return.
 */
zu_status zu_loader_col_str(zu_loader *l, const char *name, size_t name_len,
                            const char *const *values, const size_t *lens, uint64_t count,
                            zu_error **err);
zu_status zu_loader_col_str_z(zu_loader *l, const char *name, const char *const *values,
                              uint64_t count, zu_error **err);
/* zu_value_temporal read backwards: one ZU_TEMPORAL_ kind and the count
 * each row holds in the unit that kind implies, so a value read out as
 * 19782 days goes back in as 19782 days. ZU_TEMPORAL_ZONED_TIME and
 * ZU_TEMPORAL_ZONED_DATETIME answer ZU_UNSUPPORTED: a stored column has
 * nowhere to keep the offset that makes those two what they are. */
zu_status zu_loader_col_temporal(zu_loader *l, const char *name, size_t name_len, int32_t kind,
                                 const int64_t *values, uint64_t count, zu_error **err);
/* Writes it all. The database is on disk when this returns ZU_OK, and
 * zu_open on the same path reads it. */
zu_status zu_loader_finish(zu_loader *l, zu_error **err);
void zu_loader_free(zu_loader *l);

/* ---- appending ----
 *
 * How values get into a table that already exists. A statement is the
 * wrong shape for it: every row is parsed, bound, planned and
 * committed, and the commit is the expensive part, so a million rows is
 * a million commits and the load is dominated by durability work nobody
 * asked for. An appender buffers rows and pays one commit per flush.
 *
 * A row is written a value at a time, in the order the table declares
 * its columns, and ended by zu_append_end_row. A column is a position
 * rather than a name, because naming one per value would cost a lookup
 * on the one path where per-value cost is the whole story and a loader
 * knows its own column order. The columns are there to be read back:
 * zu_appender_cols is how many values a row carries and
 * zu_appender_col_name is what each one is called.
 *
 * A refused value ends the row it was in, whether the column would not
 * take it or it was no value at all. The values that row had already
 * written come back off, the error names the column and says what it
 * holds, and the next value starts a new row. A row of the wrong width
 * is refused the same way at zu_append_end_row. Nothing of a refused row
 * is kept, so an appender is still usable once the loop is fixed. A row
 * that was never ended is not a row: a flush takes it back off rather
 * than writing a short one.
 *
 * A rel table has no property columns. A row of one is the two ends of
 * an edge, as offsets into the tables it runs between, so those are its
 * two columns and they are named for what they are. A negative offset
 * is refused where it was appended, since it is no row of anything. An
 * edge to a row that is not there is refused at the flush, before
 * anything is written, and the file is left as it was.
 *
 * A flush is one commit. When it returns the rows are durable and every
 * later statement sees them, and before it returns nothing sees
 * anything. A flush with nothing buffered touches no file, so a host
 * can flush on a timer without writing empty commits, and a flush that
 * failed keeps its rows so what did not go in is still there to look
 * at. Opening the appender is where a table nothing declares, a table
 * that stores no properties, a column that holds a null and a read-only
 * connection are refused, rather than at the first flush a million rows
 * later.
 *
 * An appender is used from one thread, like the connection it writes
 * through, and it takes that connection's claim for every call: a
 * second thread in a call, or a statement running on the same
 * connection, answers ZU_MISUSE_CONCURRENT rather than tearing a
 * buffer. After close, every call but close answers ZU_MISUSE_CLOSED
 * and only zu_appender_free is left. Closing twice writes nothing the
 * second time, so a cleanup path may close what the load already did.
 *
 * zu_appender_free writes what is still buffered, because rows that
 * were appended are rows the host meant to write; what it cannot do is
 * say that the write failed, which is what close is for. A host that
 * wants the rows gone calls zu_appender_discard and gets exactly
 * that. */
zu_status zu_appender_open(zu_conn *conn, const char *table, size_t table_len, zu_appender **out,
                           zu_error **err);
zu_status zu_appender_open_z(zu_conn *conn, const char *table, zu_appender **out, zu_error **err);
/* Any nonzero value is true, as everywhere else in this header. */
zu_status zu_append_bool(zu_appender *app, int32_t v, zu_error **err);
zu_status zu_append_i64(zu_appender *app, int64_t v, zu_error **err);
zu_status zu_append_f64(zu_appender *app, double v, zu_error **err);
zu_status zu_append_str(zu_appender *app, const char *v, size_t v_len, zu_error **err);
zu_status zu_append_str_z(zu_appender *app, const char *v, zu_error **err);
zu_status zu_append_bytes(zu_appender *app, const uint8_t *v, size_t v_len, zu_error **err);
/* One ZU_TEMPORAL_ kind and the count in the unit that kind implies,
 * which is zu_value_temporal read backwards. ZU_TEMPORAL_ZONED_TIME and
 * ZU_TEMPORAL_ZONED_DATETIME answer ZU_UNSUPPORTED for the reason
 * zu_loader_col_temporal gives. */
zu_status zu_append_temporal(zu_appender *app, int32_t kind, int64_t count, zu_error **err);
/* Ends the row being written, which is what makes it a row. */
zu_status zu_append_end_row(zu_appender *app, zu_error **err);
zu_status zu_appender_flush(zu_appender *app, zu_error **err);
/* Rows ended and not yet written, and rows committed across every
 * flush. Both write nought before anything can fail. */
zu_status zu_appender_buffered(zu_appender *app, uint64_t *out);
zu_status zu_appender_committed(zu_appender *app, uint64_t *out);
zu_status zu_appender_cols(zu_appender *app, uint32_t *out);
/* The column's name, borrowed from the appender and valid until it is
 * freed, or NULL out of range. len may be NULL. */
const char *zu_appender_col_name(zu_appender *app, uint32_t col, size_t *len);
/* Throws away what is buffered and says how many rows that was. Rows an
 * earlier flush committed are committed and this does not reach them.
 * out may be NULL. */
zu_status zu_appender_discard(zu_appender *app, uint64_t *out);
/* Flushes what is left and spends the appender, writing the rows it
 * committed in all through out, which may be NULL. */
zu_status zu_appender_close(zu_appender *app, uint64_t *out, zu_error **err);
void zu_appender_free(zu_appender *app);

/* ---- frames ----
 *
 * How values get queried without getting in at all. A host holding
 * columns in memory names them as a table of one connection and runs
 * statements over them where they lie. Nothing is copied, at
 * registration or at read: a scan builds vectors that point straight at
 * the host's buffers wherever the layouts agree, and widens a value at
 * a time into a scratch arena where they do not, so a host with a
 * hundred columns pays for the one the statement named and a frame of
 * ten million rows registers in the time it takes to walk its columns.
 *
 * The layouts that agree are the ones Arrow and this engine both keep:
 * 64-bit signed integers, doubles, one bit a row for a boolean, and
 * characters end to end with offsets cutting them up. A narrower
 * integer, an unsigned one, a single-precision float and a count of
 * microseconds against the nanoseconds this engine keeps time in are
 * all widened as the statement reaches them. Strings never copy their
 * characters either way: what a scan builds is the sixteen-byte view a
 * row of the string lane is, and that view points back into the host's
 * data buffer.
 *
 * The order is create, then a column call per column, then register.
 * Every column call takes the count of values it is passing and a
 * mismatch with the frame's row count is refused at that call, where
 * the caller still knows which column it was describing. Everything
 * else that can fail is settled at zu_conn_register: alignment, an
 * unsigned value too large for the signed lane, a scale that would
 * overflow, an offset that leaves its buffer. A read of a registered
 * frame cannot fail, which is what lets a scan be a loop.
 *
 * The buffers stay the host's and this library never writes one. What
 * it asks for is that each stays where it is, unwritten and unfreed,
 * until the release callback runs. That callback is how a host learns
 * the engine is finished: it runs once, on a thread of this library's,
 * after the last statement reading the frame ends, which is not the
 * unregister that preceded it and not necessarily the free either. A
 * host that has to take a lock, or a runtime's interpreter lock, to let
 * go of what it passed takes it inside the callback. Both owner and
 * release may be NULL for a host whose buffers outlive the process.
 *
 * A frame is described once and registered as often as you like. The
 * handle stays the caller's on every path, so registering it on two
 * connections registers the same memory twice, and zu_frame_free is
 * what ends it either way.
 *
 * A frame is read only and has no edges. A statement that would insert
 * into, set on or delete from a registered name is refused with 25G03
 * and the reason. A name a stored table already holds is refused;
 * a name another frame holds replaces that frame. Registering inside a
 * transaction is 25G01, since a table appearing halfway through one is
 * not a thing the transaction could then be rolled back over.
 *
 * A frame is used from one thread, like the connection it registers on,
 * and a second thread in a call answers ZU_MISUSE_CONCURRENT. */
zu_status zu_frame_new(const char *name, size_t name_len, uint64_t rows, void *owner,
                       void (*release)(void *), zu_frame **out, zu_error **err);
zu_status zu_frame_new_z(const char *name, uint64_t rows, void *owner, void (*release)(void *),
                         zu_frame **out, zu_error **err);
/* bits is 8, 16, 32 or 64. scale is what one value is multiplied by to
 * reach the unit its meaning counts in: 1 for an integer and a date,
 * 1000 for the microseconds Arrow keeps a time or a timestamp in.
 * temporal is that meaning, ZU_FRAME_PLAIN for a column of numbers or
 * one of the ZU_TEMPORAL_ kinds otherwise, with the two zoned kinds
 * answering ZU_UNSUPPORTED for the reason zu_loader_col_temporal gives.
 * Sixty-four signed bits at scale 1 is the lane this engine reads
 * natively and is the column that costs nothing at all. */
zu_status zu_frame_col_int(zu_frame *f, const char *name, size_t name_len, const void *values,
                           uint64_t count, int32_t bits, int32_t is_signed, int64_t scale,
                           int32_t temporal, zu_error **err);
/* bits is 32 or 64, and 64 is the lane. */
zu_status zu_frame_col_float(zu_frame *f, const char *name, size_t name_len, const void *values,
                             uint64_t count, int32_t bits, zu_error **err);
/* One bit a row, low bit of the first byte first, which is Arrow's
 * bitmap and this engine's alike. A host holding a slice with a bit
 * offset of its own owes the shift before it gets here: a bitmap that
 * starts partway into a byte is not a thing a pointer can say. */
zu_status zu_frame_col_bool(zu_frame *f, const char *name, size_t name_len, const void *bitmap,
                            uint64_t count, zu_error **err);
/* Arrow's Utf8 when wide is nought and its LargeUtf8 when it is not,
 * which is 32-bit and 64-bit offsets. There are count + 1 of them and
 * the last is how much of data is used. */
zu_status zu_frame_col_str(zu_frame *f, const char *name, size_t name_len, const void *offsets,
                           int32_t wide, const void *data, size_t data_len, uint64_t count,
                           zu_error **err);
/* Arrow's Utf8View: sixteen bytes a row at views, over buffers data
 * buffers named by the two arrays data and data_lens. A short string in
 * that layout is already this engine's own view, byte for byte. */
zu_status zu_frame_col_view(zu_frame *f, const char *name, size_t name_len, const void *views,
                            const void *const *data, const size_t *data_lens, size_t buffers,
                            uint64_t count, zu_error **err);
void zu_frame_free(zu_frame *f);
/* Registers the frame as a table of this connection under the name it
 * carries. Does not spend the handle. */
zu_status zu_conn_register(zu_conn *conn, zu_frame *f, zu_error **err);
/* Drops one, writing through out, which may be NULL, whether there was
 * one under that name. A statement already running keeps the frame it
 * started with, and the release callback waits for it. */
zu_status zu_conn_unregister(zu_conn *conn, const char *name, size_t name_len, int32_t *out,
                             zu_error **err);
zu_status zu_conn_unregister_z(zu_conn *conn, const char *name, int32_t *out, zu_error **err);
/* How many are registered, and the call that refreshes the names the
 * accessor below hands out. The two are separate so that every pointer
 * a host took while walking the list is still good when it reaches the
 * end. */
zu_status zu_conn_registered_count(zu_conn *conn, uint64_t *out);
/* One name, in the sorted order the count call last read them in, or
 * NULL out of range. Borrowed from the connection and valid until the
 * next zu_conn_registered_count on it or until it closes. Not
 * NUL-terminated, which is what len is for; len may be NULL. */
const char *zu_conn_registered_name(zu_conn *conn, uint64_t index, size_t *len);

#ifdef __cplusplus
}
#endif

#endif /* ZU_H */
