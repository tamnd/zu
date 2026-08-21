/* The conformance corpus, run against the C ABI.
 *
 * This is crates/zu-corpus/src/runner.rs in the language a client
 * repository writes its own runner in, and that one is the reference in
 * the same way the reader and the decoder beside this file have one:
 * where the two disagree about a case, the Rust one is right and this
 * one has a bug. It exists because the corpus is shipped to eight client
 * repositories that reach the engine through zu.h and nothing else, and
 * a corpus proved runnable only by the runner that produced it has
 * proved the wrong thing. Everything here goes through the header: the
 * database is created through zu_create, the load goes in through
 * zu_loader, and every value comes back through zu_result_cell and the
 * zu_value accessors.
 *
 * Usage: runner --dir DIR [--strict] [--quiet] case.yaml ...
 *
 * The scratch directory and the files are arguments rather than things
 * found, which is how yaml_test and value_test are called and why this
 * needs no mkdtemp, no opendir and no ifdef per platform. A case gets a
 * database of its own under DIR, named after it, for the reason the
 * reference runner gives: a case that leaked a table into the next one
 * would be a failure that moves when the file is reordered. The file is
 * removed once the case passes and left behind when it does not, so a
 * failure leaves something to open.
 *
 * What it prints is what the reference runner prints, line for line,
 * so that two clients disagreeing about a case is a diff rather than a
 * reading exercise, and CI diffs the two over every case and over
 * conformance/c/wrong, which is the failures the corpus cannot show
 * because it passes. Three places are outside that. A load that cannot
 * go in is refused by the ABI and reported in the ABI's words, because
 * the two runners refuse it in different places: the Rust one knows the
 * line the column was written on and the loader knows the column. A
 * cell holding a record is named rather than printed, since the
 * encoding has no spelling for one and a cv has no arm for one. And an
 * export the engine has no Arrow type for is reported in the ABI's
 * words too, which are the same words with the ABI's own "invalid
 * argument" in front of them, since a refusal reaches this side as a
 * zu_error and reaches the other side as the export's own error. None
 * of the three can happen in a case that passes.
 *
 * An outcome is one of three things. Passed and failed are obvious.
 * Unsupported is a statement the engine refuses with a GQLSTATUS in
 * class 42 or 0A, which is a case written ahead of the engine and
 * allowed on purpose; --strict turns it into a failure, which is what a
 * release runs. Exit code 1 on any failure, 2 on a usage error, so a
 * script can tell "you called this wrongly" from "the corpus does not
 * pass".
 */
#include "value.h"
#include "yaml.h"
#include "zu.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* The schema this runner reads. A file from another one says so here
 * rather than failing somewhere in the middle of its cases. */
#define RUNNER_SCHEMA 3

typedef enum outcome { OUT_PASSED, OUT_FAILED, OUT_UNSUPPORTED } outcome;

/* One run, so that the case loop is not a function of eight arguments.
 * The arena is the run's rather than the case's and is reset between
 * cases, which is what it was built for: the chunks stay warm and no
 * value outlives the comparison it was decoded for. */
typedef struct run {
    const char *dir;
    int strict;
    int quiet;
    cv_arena *arena;
    /* The connection the case is running on, which is here because
     * turning a node into a value the case can be compared against
     * needs the name of its table and the connection is what knows
     * one. NULL outside a case. */
    zu_conn *conn;
    unsigned long cases;
    unsigned long passed;
    unsigned long failed;
    unsigned long unsupported;
} run;

/* What a refusal said and the condition it named, copied out of the
 * handle so that the handle can be released here rather than at every
 * place a caller stops looking at it. */
typedef struct refusal {
    char message[512];
    char code[8];
} refusal;

/* Always -1, so a caller writes `return say(...)`. */
static int say(char *detail, size_t len, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(detail, len, fmt, ap);
    va_end(ap);
    return -1;
}

/* Appends to a buffer that may already be full, tracking what a whole
 * report would have taken so that the pieces after a truncation do not
 * write over the ones before it. */
static void append(char *buf, size_t len, size_t *used, const char *fmt, ...) {
    va_list ap;
    int n;
    if (*used >= len) {
        return;
    }
    va_start(ap, fmt);
    n = vsnprintf(buf + *used, len - *used, fmt, ap);
    va_end(ap);
    if (n > 0) {
        *used += (size_t)n;
    }
}

static void take(zu_status status, zu_error *e, refusal *out) {
    const char *message = e != NULL ? zu_error_message(e, NULL) : NULL;
    const char *code = e != NULL ? zu_error_code(e, NULL) : NULL;
    out->code[0] = '\0';
    if (code != NULL) {
        snprintf(out->code, sizeof out->code, "%s", code);
    }
    if (message != NULL) {
        snprintf(out->message, sizeof out->message, "%s", message);
    } else {
        snprintf(out->message, sizeof out->message, "status %d and no message with it",
                 (int)status);
    }
    zu_error_free(e);
}

/* Whether a refusal means the engine does not implement the statement
 * rather than that the statement is wrong. The two GQL classes that say
 * so are 42, syntax error or access rule violation, and 0A, feature not
 * supported. A case landing on either is a case ahead of the engine,
 * which the corpus allows on purpose. */
static int ahead_of_the_engine(const refusal *f) {
    return strncmp(f->code, "42", 2) == 0 || strncmp(f->code, "0A", 2) == 0;
}

/* `["a", "b"]`, which is how Rust writes a list of names with `{:?}`
 * and therefore how the reference runner spells this complaint. A
 * column name is a word, so nothing here escapes one; a report that had
 * to would be a report about a name no case writes. */
static void show_names(char *buf, size_t len, const zy_str *items, size_t count) {
    size_t used = 0, i;
    if (len > 0) {
        buf[0] = '\0';
    }
    append(buf, len, &used, "[");
    for (i = 0; i < count; i++) {
        append(buf, len, &used, "%s\"%s\"", i > 0 ? ", " : "", items[i].ptr);
    }
    append(buf, len, &used, "]");
}

/* An array of n things in the arena, which lives until the case ends. */
static void *slots(run *r, size_t n, size_t size) {
    return cv_alloc(r->arena, n > 0 ? n * size : size);
}

/* ---- what came back ---- */

static const char *type_name(int32_t type) {
    switch (type) {
    case ZU_TYPE_RECORD: return "RECORD";
    default: return "value of a type this header does not name";
    }
}

/* What a table id is called, copied into the arena.
 *
 * Copied rather than borrowed because the connection keeps one name at
 * a time and the next call replaces it, so a path of four nodes that
 * held the pointer would be four pointers at one name.
 *
 * A table the catalog does not name is spelled `#7` after its id, which
 * is what the reference runner does and for the reason it gives: a
 * report saying "the case wants person#1 and this is #7" is more use to
 * whoever has to fix it than a runner that stopped. */
static int table_name(run *r, uint32_t table, zy_str *out) {
    size_t len = 0;
    const char *name = zu_conn_table_name(r->conn, table, &len);
    char spelled[16];
    char *copy;
    if (name == NULL) {
        len = (size_t)snprintf(spelled, sizeof spelled, "#%lu", (unsigned long)table);
        name = spelled;
    }
    copy = (char *)cv_alloc(r->arena, len + 1);
    if (copy == NULL) {
        return -1;
    }
    memcpy(copy, name, len);
    copy[len] = '\0';
    out->ptr = copy;
    out->len = len;
    return 0;
}

/* One cell of a result as the value the encoding compares, so that what
 * the engine handed back and what the case wrote are the same kind of
 * thing.
 *
 * A string borrows the result's own bytes rather than copying them,
 * which is safe for exactly as long as it needs to be: the comparison
 * happens before zu_result_free. A list is the one composite type and
 * therefore the one place this has to allocate, and it allocates in the
 * arena so that its items die with the case the decoded ones do.
 *
 * A node, an edge and a path carry the name of their table rather than
 * its id, because the id is a number the file decided and every client
 * builds its own file, so the name is what a case can assert and the
 * connection is what is asked for it.
 *
 * A record is a failure naming what came back. The encoding has no
 * spelling for one, so a cell holding one is a case whose statement
 * returns something the corpus has no way to assert. */
static int engine_value(run *r, const zu_value *v, const char *where, cv *out, char *detail,
                        size_t len) {
    int32_t type = zu_value_type(v);
    switch (type) {
    case ZU_TYPE_NULL:
        out->kind = CV_NULL;
        return 0;
    case ZU_TYPE_BOOL: {
        int32_t b = 0;
        if (zu_value_bool(v, &b) != ZU_OK) {
            return say(detail, len, "%s says it is a BOOL and does not read as one", where);
        }
        out->kind = CV_BOOL;
        out->as.boolean = b != 0;
        return 0;
    }
    case ZU_TYPE_INT: {
        int64_t n = 0;
        if (zu_value_i64(v, &n) != ZU_OK) {
            return say(detail, len, "%s says it is an INT64 and does not read as one", where);
        }
        out->kind = CV_INT;
        out->as.integer = n;
        return 0;
    }
    case ZU_TYPE_FLOAT: {
        double f = 0;
        if (zu_value_f64(v, &f) != ZU_OK) {
            return say(detail, len, "%s says it is a FLOAT64 and does not read as one", where);
        }
        out->kind = CV_FLOAT;
        out->as.real = f;
        return 0;
    }
    case ZU_TYPE_STR: {
        const char *text = NULL;
        size_t text_len = 0;
        if (zu_value_str(v, &text, &text_len) != ZU_OK || text == NULL) {
            return say(detail, len, "%s says it is a STRING and does not read as one", where);
        }
        out->kind = CV_STR;
        out->as.str.ptr = text;
        out->as.str.len = text_len;
        return 0;
    }
    case ZU_TYPE_TEMPORAL: {
        int32_t kind = -1, offset = 0;
        int64_t count = 0;
        if (zu_value_temporal(v, &kind, &count, &offset) != ZU_OK || kind < ZU_TEMPORAL_DATE ||
            kind > ZU_TEMPORAL_DURATION_DAY_TIME) {
            return say(detail, len, "%s says it is a temporal and does not read as one", where);
        }
        out->kind = CV_TEMPORAL;
        /* The two enumerations are one list in two headers, in the same
         * order and for the same reason: a temporal is a count and the
         * unit it is counted in. */
        out->as.temporal.unit = (cv_unit)kind;
        out->as.temporal.count = count;
        out->as.temporal.offset = offset;
        return 0;
    }
    case ZU_TYPE_NODE: {
        uint32_t table = 0;
        uint64_t offset = 0;
        if (zu_value_node(v, &table, &offset) != ZU_OK) {
            return say(detail, len, "%s says it is a NODE and does not read as one", where);
        }
        if (table_name(r, table, &out->as.node.table) != 0) {
            return say(detail, len, "%s is a node with no memory to name its table", where);
        }
        out->kind = CV_NODE;
        out->as.node.offset = offset;
        return 0;
    }
    case ZU_TYPE_REL: {
        uint32_t table = 0;
        uint64_t src = 0, dst = 0;
        if (zu_value_rel(v, &table, &src, &dst) != ZU_OK) {
            return say(detail, len, "%s says it is an EDGE and does not read as one", where);
        }
        if (table_name(r, table, &out->as.edge.table) != 0) {
            return say(detail, len, "%s is an edge with no memory to name its table", where);
        }
        out->kind = CV_EDGE;
        out->as.edge.src = src;
        out->as.edge.dst = dst;
        return 0;
    }
    /* A path walks with the accessors a list walks with, which is why
     * it is read here rather than in a case of its own. */
    case ZU_TYPE_PATH:
    case ZU_TYPE_LIST: {
        uint64_t count = zu_value_len(v), i;
        cv *items = (cv *)slots(r, (size_t)count, sizeof(cv));
        if (items == NULL) {
            return say(detail, len, "%s is a list of %lu with no memory to hold it", where,
                       (unsigned long)count);
        }
        for (i = 0; i < count; i++) {
            const zu_value *item = NULL;
            if (zu_value_at(v, i, &item) != ZU_OK) {
                return say(detail, len, "%s is a list whose item %lu did not read", where,
                           (unsigned long)(i + 1));
            }
            if (engine_value(r, item, where, &items[i], detail, len) != 0) {
                return -1;
            }
        }
        out->kind = type == ZU_TYPE_PATH ? CV_PATH : CV_LIST;
        out->as.list.items = items;
        out->as.list.count = (size_t)count;
        return 0;
    }
    default:
        return say(detail, len, "%s came back as a %s, which the encoding has no spelling for",
                   where, type_name(type));
    }
}

/* The names the case wrote, and the names the result carries, in the
 * one shape the comparison and the report both want. */
static const zy_str *wanted_names(run *r, const zy_node *columns, size_t *count) {
    const zy_node *items = NULL;
    zy_str *out;
    size_t i;
    /* A `columns:` with nothing under it is a result with no columns,
     * which is what FINISH answers, so it goes through the same
     * accessor `rows:` does rather than reading the fields directly. */
    if (zy_seq_or_empty(columns, &items, count) != 0) {
        return NULL;
    }
    out = (zy_str *)slots(r, *count, sizeof(zy_str));
    for (i = 0; out != NULL && i < *count; i++) {
        if (items[i].kind != ZY_SCALAR) {
            return NULL;
        }
        out[i] = items[i].text;
    }
    return out;
}

static const zy_str *result_names(run *r, const zu_result *result, size_t *count) {
    uint32_t cols = zu_result_cols(result), i;
    zy_str *out = (zy_str *)slots(r, cols, sizeof(zy_str));
    *count = cols;
    for (i = 0; out != NULL && i < cols; i++) {
        if (zu_result_col_name(result, i, &out[i].ptr, &out[i].len) != ZU_OK) {
            return NULL;
        }
    }
    return out;
}

static int same_name(zy_str a, zy_str b) {
    return a.len == b.len && memcmp(a.ptr, b.ptr, a.len) == 0;
}

/* What differs between what a case wants and what came back, or 0 if
 * nothing does.
 *
 * The first difference and not all of them, because the first is nearly
 * always the cause of the rest, and a report that prints a hundred rows
 * is one nobody reads to the end. The order is the reference runner's:
 * the columns, then the values, then how many rows there were, so that
 * a projection that renamed a column is not reported as forty wrong
 * values. */
static int compare(run *r, const zy_node *columns, const zy_node *rows, zu_result *result,
                   char *detail, size_t len) {
    const zy_str *want_names, *got_names;
    const zy_node *want_rows = NULL;
    size_t want_cols = 0, got_cols = 0, want_count = 0, i, j;
    uint64_t got_count = zu_result_rows(result);
    char err[512];

    want_names = wanted_names(r, columns, &want_cols);
    got_names = result_names(r, result, &got_cols);
    if (want_names == NULL || got_names == NULL) {
        return say(detail, len, "the columns did not read");
    }
    if (want_cols != got_cols) {
        j = want_cols;
    } else {
        for (j = 0; j < want_cols && same_name(want_names[j], got_names[j]); j++) {
        }
    }
    if (j != want_cols || want_cols != got_cols) {
        char got_text[512], want_text[512];
        show_names(got_text, sizeof got_text, got_names, got_cols);
        show_names(want_text, sizeof want_text, want_names, want_cols);
        return say(detail, len, "columns %s where the case wants %s", got_text, want_text);
    }

    if (zy_seq_or_empty(rows, &want_rows, &want_count) != 0) {
        return say(detail, len, "`rows:` is a sequence");
    }
    for (i = 0; i < want_count && (uint64_t)i < got_count; i++) {
        const zy_node *cells = zy_get(&want_rows[i], "values");
        if (cells == NULL || cells->kind != ZY_SEQ) {
            return say(detail, len, "row %lu is `values:` and its values",
                       (unsigned long)(i + 1));
        }
        if (cells->count != want_cols) {
            return say(detail, len, "a row of %lu against %lu columns",
                       (unsigned long)cells->count, (unsigned long)want_cols);
        }
        for (j = 0; j < want_cols; j++) {
            char where[128], got_text[512], want_text[512];
            const zu_value *cell = NULL;
            cv want, got;
            snprintf(where, sizeof where, "row %lu column %s", (unsigned long)(i + 1),
                     want_names[j].ptr);
            if (cv_decode(r->arena, &cells->items[j], &want, err, sizeof err) != 0) {
                return say(detail, len, "%s: %s", where, err);
            }
            if (zu_result_cell(result, (uint64_t)i, (uint32_t)j, &cell) != ZU_OK) {
                return say(detail, len, "%s did not read", where);
            }
            if (engine_value(r, cell, where, &got, detail, len) != 0) {
                return -1;
            }
            if (!cv_same(&want, &got)) {
                cv_show(&got, got_text, sizeof got_text);
                cv_show(&want, want_text, sizeof want_text);
                return say(detail, len, "%s is %s where the case wants %s", where, got_text,
                           want_text);
            }
        }
    }
    if ((uint64_t)want_count != got_count) {
        return say(detail, len, "%lu rows where the case wants %lu", (unsigned long)got_count,
                   (unsigned long)want_count);
    }
    return 0;
}

/* ---- the export ----
 *
 * The same result on the way out through Arrow, which a case describes
 * with `arrow:` and which crates/zu-corpus/src/arrow.rs says the whole
 * of the why about. What is checked here is the schema, in the C Data
 * Interface's own format strings, and how many rows came back through
 * the stream: this side reads those strings out of the struct where the
 * reference runner reads them off an FFI_ArrowSchema, and the two are
 * the same bytes, which is the point of asserting the format string
 * rather than a name for the type. */

/* One field of the schema, as the case wrote it. The strings point into
 * the document, which outlives the case, so only the arrays are the
 * arena's. */
typedef struct afield {
    zy_str name;
    zy_str format;
    const struct afield *children;
    size_t count;
} afield;

static int arrow_fields(run *r, const zy_node *node, const afield **out, size_t *count,
                        char *detail, size_t len);

static int arrow_field(run *r, const zy_node *node, afield *out, char *detail, size_t len) {
    static const char *const KEYS[3] = {"name", "format", "children"};
    const zy_node *name, *format, *children;
    zy_str extra;

    if (node->kind != ZY_MAP) {
        return say(detail, len, "an Arrow field is a mapping of `name` and `format`");
    }
    extra = zy_unknown(node, KEYS, 3);
    if (extra.ptr != NULL) {
        return say(detail, len, "an Arrow field has no key \"%s\"", extra.ptr);
    }
    name = zy_get(node, "name");
    format = zy_get(node, "format");
    if (name == NULL || name->kind != ZY_SCALAR) {
        return say(detail, len, "an Arrow field has a `name:`");
    }
    if (format == NULL || format->kind != ZY_SCALAR || format->text.len == 0) {
        return say(detail, len, "an Arrow field has a `format:`");
    }
    out->name = name->text;
    out->format = format->text;
    out->children = NULL;
    out->count = 0;
    children = zy_get(node, "children");
    if (children == NULL) {
        return 0;
    }
    return arrow_fields(r, children, &out->children, &out->count, detail, len);
}

static int arrow_fields(run *r, const zy_node *node, const afield **out, size_t *count,
                        char *detail, size_t len) {
    afield *fields;
    size_t i;

    if (node->kind != ZY_SEQ) {
        return say(detail, len, "`arrow:` is a sequence of fields");
    }
    fields = (afield *)slots(r, node->count, sizeof(afield));
    if (fields == NULL) {
        return say(detail, len, "the fields did not read");
    }
    for (i = 0; i < node->count; i++) {
        if (arrow_field(r, &node->items[i], &fields[i], detail, len) != 0) {
            return -1;
        }
    }
    *out = fields;
    *count = node->count;
    return 0;
}

/* The fields under one place of the schema against the ones the case
 * wrote, where the place is the dotted path of the field they are under
 * and the empty one is the result itself. The first difference and not
 * all of them, for the reason the row comparison gives. */
static int arrow_schema(const char *prefix, struct ArrowSchema **got, size_t found,
                        const afield *want, size_t count, char *detail, size_t len) {
    char place[512];
    size_t i;

    if (prefix[0] == '\0') {
        snprintf(place, sizeof place, "the result");
    } else {
        snprintf(place, sizeof place, "\"%s\"", prefix);
    }
    if (found != count) {
        return say(detail, len, "arrow gives %lu fields in %s where the case wants %lu",
                   (unsigned long)found, place, (unsigned long)count);
    }
    for (i = 0; i < count; i++) {
        /* A field of a schema may carry no name at all, which is not
         * something this export produces and is still not a reason to
         * follow a NULL into a comparison. */
        const char *name = got[i]->name != NULL ? got[i]->name : "";
        const char *format = got[i]->format != NULL ? got[i]->format : "";
        char path[512];
        if (strcmp(name, want[i].name.ptr) != 0) {
            return say(detail, len, "arrow field %lu in %s is named \"%s\" where the case wants \"%s\"",
                       (unsigned long)(i + 1), place, name, want[i].name.ptr);
        }
        if (prefix[0] == '\0') {
            snprintf(path, sizeof path, "%s", want[i].name.ptr);
        } else {
            snprintf(path, sizeof path, "%s.%s", prefix, want[i].name.ptr);
        }
        if (strcmp(format, want[i].format.ptr) != 0) {
            return say(detail, len, "arrow field \"%s\" is \"%s\" where the case wants \"%s\"", path,
                       format, want[i].format.ptr);
        }
        if (arrow_schema(path, got[i]->children, (size_t)got[i]->n_children, want[i].children,
                         want[i].count, detail, len) != 0) {
            return -1;
        }
    }
    return 0;
}

/* What the export gave that the case did not want, or 0 if the two
 * agree.
 *
 * This spends the result, which is why it takes the handle through a
 * pointer to it and why it runs after the rows have been compared:
 * zu_result_arrow writes NULL back on every path and the buffers a cell
 * string was borrowed from belong to the stream afterwards. The caller
 * still frees what it has, which is then NULL and a no-op. */
static int arrow_export(run *r, zu_result **result, const zy_node *arrow, uint64_t rows,
                        char *detail, size_t len) {
    struct ArrowArrayStream stream;
    struct ArrowSchema schema;
    struct ArrowArray batch;
    const afield *want = NULL;
    size_t count = 0;
    int refused = 0, wrong;
    uint64_t given = 0;
    zu_status status;
    zu_error *e = NULL;
    refusal f;

    if (arrow->kind == ZY_SCALAR) {
        if (!zy_eq(arrow->text, "refused")) {
            return say(detail, len, "`arrow:` is the columns the export gives, or `refused`");
        }
        refused = 1;
    } else if (arrow_fields(r, arrow, &want, &count, detail, len) != 0) {
        return -1;
    }

    status = zu_result_arrow(r->conn, result, 0, &stream, &e);
    /* A library built without the export answers this and nothing else,
     * and a runner that graded it would be grading how the library was
     * built rather than what it does. The reference runner skips the
     * same check for the same reason, so the two still agree. */
    if (status == ZU_UNSUPPORTED) {
        zu_error_free(e);
        return 0;
    }
    if (status != ZU_OK) {
        take(status, e, &f);
        if (refused) {
            return 0;
        }
        /* The one report here that is not word for word the reference
         * runner's, for the reason at the top of this file: what the
         * ABI hands back is the export's own account with the ABI's
         * category in front of it. */
        return say(detail, len, "arrow refused the result: %s", f.message);
    }
    if (refused) {
        stream.release(&stream);
        return say(detail, len, "arrow exported the result where the case wants a refusal");
    }
    if (stream.get_schema(&stream, &schema) != 0) {
        const char *why = stream.get_last_error(&stream);
        say(detail, len, "arrow would not describe the schema: %s", why != NULL ? why : "no reason");
        stream.release(&stream);
        return -1;
    }
    /* The stream's schema is the struct of the columns, so what the
     * case wrote is compared against the fields under it. */
    wrong = arrow_schema("", schema.children, (size_t)schema.n_children, want, count, detail, len);
    schema.release(&schema);
    if (wrong != 0) {
        stream.release(&stream);
        return -1;
    }
    for (;;) {
        if (stream.get_next(&stream, &batch) != 0) {
            const char *why = stream.get_last_error(&stream);
            say(detail, len, "arrow would not give the rows: %s", why != NULL ? why : "no reason");
            stream.release(&stream);
            return -1;
        }
        /* A batch that was never filled is the end of the stream, which
         * is how the interface says so rather than through a status. */
        if (batch.release == NULL) {
            break;
        }
        given += (uint64_t)batch.length;
        batch.release(&batch);
    }
    stream.release(&stream);
    if (given != rows) {
        return say(detail, len, "arrow gives %lu rows where the case wants %lu",
                   (unsigned long)given, (unsigned long)rows);
    }
    return 0;
}

/* ---- the load ---- */

/* One column, decoded and handed to the loader through the call its
 * type picks. A column names its type once and its values are bare
 * payloads under it, which is the row encoding with the type factored
 * out, and the quoting rule is the same one.
 *
 * The two zoned temporal types are refused by the loader rather than
 * here, since a stored column has nowhere to keep the offset that makes
 * those two what they are and the ABI is where that fact lives. */
static int load_column(run *r, zu_loader *l, const zy_node *column, uint64_t count, char *detail,
                       size_t len) {
    const zy_node *name = zy_get(column, "name");
    const zy_node *type = zy_get(column, "type");
    const zy_node *values = zy_get(column, "values");
    zu_status status = ZU_OK;
    zu_error *e = NULL;
    refusal f;
    cv *decoded;
    char err[512];
    size_t i;

    if (name == NULL || name->kind != ZY_SCALAR || type == NULL || type->kind != ZY_SCALAR ||
        values == NULL || values->kind != ZY_SEQ) {
        return say(detail, len, "a column is a name, a type and its values");
    }
    if ((uint64_t)values->count != count) {
        return say(detail, len, "column \"%s\" has %lu values against a count of %lu",
                   name->text.ptr, (unsigned long)values->count, (unsigned long)count);
    }
    decoded = (cv *)slots(r, values->count, sizeof(cv));
    if (decoded == NULL) {
        return say(detail, len, "out of memory");
    }
    for (i = 0; i < values->count; i++) {
        if (cv_payload(r->arena, type->text.ptr, &values->items[i], &decoded[i], err, sizeof err) !=
            0) {
            return say(detail, len, "column \"%s\": %s", name->text.ptr, err);
        }
    }
    /* A column holds one kind of value, and a temporal column one unit
     * of it, because that is what a stored column is. A file that mixes
     * them is refused rather than loaded as whichever came first. */
    for (i = 1; i < values->count; i++) {
        if (decoded[i].kind != decoded[0].kind ||
            (decoded[0].kind == CV_TEMPORAL &&
             decoded[i].as.temporal.unit != decoded[0].as.temporal.unit)) {
            return say(detail, len,
                       "column \"%s\" holds more than one kind of value, and a column holds one",
                       name->text.ptr);
        }
    }

    switch (decoded[0].kind) {
    case CV_INT: {
        int64_t *held = (int64_t *)slots(r, values->count, sizeof(int64_t));
        if (held == NULL) {
            return say(detail, len, "out of memory");
        }
        for (i = 0; i < values->count; i++) {
            held[i] = decoded[i].as.integer;
        }
        status = zu_loader_col_i64(l, name->text.ptr, name->text.len, held, count, &e);
        break;
    }
    case CV_FLOAT: {
        double *held = (double *)slots(r, values->count, sizeof(double));
        if (held == NULL) {
            return say(detail, len, "out of memory");
        }
        for (i = 0; i < values->count; i++) {
            held[i] = decoded[i].as.real;
        }
        status = zu_loader_col_f64(l, name->text.ptr, name->text.len, held, count, &e);
        break;
    }
    case CV_BOOL: {
        int32_t *held = (int32_t *)slots(r, values->count, sizeof(int32_t));
        if (held == NULL) {
            return say(detail, len, "out of memory");
        }
        for (i = 0; i < values->count; i++) {
            held[i] = decoded[i].as.boolean;
        }
        status = zu_loader_col_bool(l, name->text.ptr, name->text.len, held, count, &e);
        break;
    }
    case CV_STR: {
        const char **held = (const char **)slots(r, values->count, sizeof(const char *));
        size_t *lens = (size_t *)slots(r, values->count, sizeof(size_t));
        if (held == NULL || lens == NULL) {
            return say(detail, len, "out of memory");
        }
        for (i = 0; i < values->count; i++) {
            held[i] = decoded[i].as.str.ptr;
            lens[i] = decoded[i].as.str.len;
        }
        status = zu_loader_col_str(l, name->text.ptr, name->text.len, held, lens, count, &e);
        break;
    }
    case CV_TEMPORAL: {
        int64_t *held = (int64_t *)slots(r, values->count, sizeof(int64_t));
        if (held == NULL) {
            return say(detail, len, "out of memory");
        }
        for (i = 0; i < values->count; i++) {
            held[i] = decoded[i].as.temporal.count;
        }
        status = zu_loader_col_temporal(l, name->text.ptr, name->text.len,
                                        (int32_t)decoded[0].as.temporal.unit, held, count, &e);
        break;
    }
    default: {
        char shown[256];
        cv_show(&decoded[0], shown, sizeof shown);
        return say(detail, len, "column \"%s\" holds %s, and a column of those cannot be loaded yet",
                   name->text.ptr, shown);
    }
    }
    if (status != ZU_OK) {
        take(status, e, &f);
        return say(detail, len, "column \"%s\": %s", name->text.ptr, f.message);
    }
    return 0;
}

/* The edges of a load, which are the row each one starts at and the row
 * it ends at. The loader sorts and deduplicates them at finish, so they
 * go in as they were written. */
static int load_edges(run *r, zu_loader *l, const zy_node *pairs, uint64_t count, char *detail,
                      size_t len) {
    const zy_node *items = NULL;
    size_t found = 0, i;
    uint32_t *from, *to;
    zu_status status;
    zu_error *e = NULL;
    refusal f;

    if (pairs == NULL) {
        return 0;
    }
    if (zy_seq_or_empty(pairs, &items, &found) != 0) {
        return say(detail, len, "`pairs:` is a sequence of edges");
    }
    if (found == 0) {
        return 0;
    }
    from = (uint32_t *)slots(r, found, sizeof(uint32_t));
    to = (uint32_t *)slots(r, found, sizeof(uint32_t));
    if (from == NULL || to == NULL) {
        return say(detail, len, "out of memory");
    }
    for (i = 0; i < found; i++) {
        static const char *const ends[2] = {"from", "to"};
        uint32_t *into[2];
        size_t k;
        into[0] = from;
        into[1] = to;
        for (k = 0; k < 2; k++) {
            const zy_node *end = zy_get(&items[i], ends[k]);
            char *stop = NULL;
            unsigned long row;
            if (end == NULL || end->kind != ZY_SCALAR) {
                return say(detail, len, "an edge has a `%s:`", ends[k]);
            }
            row = strtoul(end->text.ptr, &stop, 10);
            if (stop == end->text.ptr || *stop != '\0') {
                return say(detail, len, "`%s:` is a row number", ends[k]);
            }
            if ((uint64_t)row >= count) {
                return say(detail, len,
                           "an edge reaches row %lu of a table with %lu rows in it", row,
                           (unsigned long)count);
            }
            into[k][i] = (uint32_t)row;
        }
    }
    status = zu_loader_edges(l, from, to, found, &e);
    if (status != ZU_OK) {
        take(status, e, &f);
        return say(detail, len, "the edges: %s", f.message);
    }
    return 0;
}

/* The suite's data, put into a database that is not there yet through
 * the same bulk load path `zu copy` and the Rust appender are built on.
 *
 * It is bulk load rather than a statement because in v0 that is the
 * only way in, which is what makes a load the strongest question the
 * corpus asks a client: the value crosses the boundary twice and by two
 * different mechanisms. */
static int apply_load(run *r, const char *path, const zy_node *load, char *detail, size_t len) {
    const zy_node *nodes = zy_get(load, "nodes");
    const zy_node *edges = zy_get(load, "edges");
    const zy_node *count_node = zy_get(load, "count");
    const zy_node *columns = zy_get(load, "columns");
    zu_loader *l = NULL;
    zu_status status;
    zu_error *e = NULL;
    refusal f;
    unsigned long count;
    char *stop = NULL;
    size_t i;
    int rc = 0;

    if (nodes == NULL || nodes->kind != ZY_SCALAR || edges == NULL || edges->kind != ZY_SCALAR ||
        count_node == NULL || count_node->kind != ZY_SCALAR || columns == NULL ||
        columns->kind != ZY_SEQ) {
        return say(detail, len, "a load is a node table, a rel table, a count and its columns");
    }
    count = strtoul(count_node->text.ptr, &stop, 10);
    if (stop == count_node->text.ptr || *stop != '\0' || count == 0) {
        return say(detail, len, "`count:` is how many rows the load has");
    }

    status = zu_loader_create_z(path, &l, &e);
    if (status != ZU_OK) {
        take(status, e, &f);
        return say(detail, len, "creating %s: %s", path, f.message);
    }
    status = zu_loader_table_z(l, nodes->text.ptr, edges->text.ptr, count, &e);
    if (status != ZU_OK) {
        take(status, e, &f);
        rc = say(detail, len, "the table: %s", f.message);
    }
    for (i = 0; rc == 0 && i < columns->count; i++) {
        rc = load_column(r, l, &columns->items[i], count, detail, len);
    }
    if (rc == 0) {
        rc = load_edges(r, l, zy_get(load, "pairs"), count, detail, len);
    }
    if (rc == 0) {
        /* Nothing reached the file before this, so a load either
         * happened or did not. */
        status = zu_loader_finish(l, &e);
        if (status != ZU_OK) {
            take(status, e, &f);
            rc = say(detail, len, "finishing: %s", f.message);
        }
    }
    zu_loader_free(l);
    return rc;
}

/* ---- one case ---- */

static void report(run *r, const char *suite, const char *name, size_t line, outcome o,
                   const char *detail) {
    const char *mark = "ok";
    r->cases++;
    switch (o) {
    case OUT_PASSED:
        r->passed++;
        break;
    case OUT_FAILED:
        mark = "FAILED";
        r->failed++;
        break;
    default:
        mark = "unsupported";
        r->unsupported++;
        break;
    }
    if (o == OUT_PASSED || r->quiet) {
        return;
    }
    printf("%s/%s line %lu %s", suite, name, (unsigned long)line, mark);
    if (detail != NULL && detail[0] != '\0') {
        printf(": %s", detail);
    }
    printf("\n");
}

/* Binds one parameter of a case onto a prepared statement.
 *
 * A parameter is the value encoding with a name beside it, so it is
 * decoded by the same decoder every row is and then handed to the bind
 * call its kind picks. A LIST has no bind call, because the ABI has no
 * way to build a list value: that is a gap in the ABI and it is said in
 * those words, since a runner that reported it as a wrong answer would
 * send whoever reads the report looking at the case. */
static int bind_param(run *r, zu_stmt *stmt, const zy_node *param, char *detail, size_t len) {
    static const char *const KEYS[3] = {"name", "type", "value"};
    const zy_node *name;
    zy_str extra;
    zu_status status;
    char err[512];
    cv v;

    if (param->kind != ZY_MAP) {
        return say(detail, len, "a parameter is a mapping of `name`, `type` and `value`");
    }
    extra = zy_unknown(param, KEYS, 3);
    if (extra.ptr != NULL) {
        return say(detail, len, "a parameter has no key \"%s\"", extra.ptr);
    }
    name = zy_get(param, "name");
    if (name == NULL || name->kind != ZY_SCALAR) {
        return say(detail, len, "a parameter names itself");
    }
    if (cv_typed(r->arena, param, &v, err, sizeof err) != 0) {
        return say(detail, len, "parameter \"%s\": %s", name->text.ptr, err);
    }

    switch (v.kind) {
    case CV_NULL:
        status = zu_bind_null(stmt, name->text.ptr, name->text.len);
        break;
    case CV_BOOL:
        status = zu_bind_bool(stmt, name->text.ptr, name->text.len, v.as.boolean);
        break;
    case CV_INT:
        status = zu_bind_i64(stmt, name->text.ptr, name->text.len, v.as.integer);
        break;
    case CV_FLOAT:
        status = zu_bind_f64(stmt, name->text.ptr, name->text.len, v.as.real);
        break;
    case CV_STR:
        status = zu_bind_str(stmt, name->text.ptr, name->text.len, v.as.str.ptr, v.as.str.len);
        break;
    case CV_TEMPORAL:
        status = zu_bind_temporal(stmt, name->text.ptr, name->text.len,
                                  (int32_t)v.as.temporal.unit, v.as.temporal.count,
                                  v.as.temporal.offset);
        break;
    default: {
        char shown[256];
        cv_show(&v, shown, sizeof shown);
        return say(detail, len, "parameter \"%s\" is %s, which this ABI has no bind call for",
                   name->text.ptr, shown);
    }
    }
    if (status != ZU_OK) {
        return say(detail, len, "parameter \"%s\" would not bind", name->text.ptr);
    }
    return 0;
}

/* Runs the statement under test, with the parameters the case binds.
 *
 * A case with none goes through zu_query, which is the call a client
 * makes when there is nothing to bind. A case with parameters goes
 * through prepare, bind and execute, because through this ABI that is
 * the only way a value gets into a statement. Returns -1 for a case
 * this runner cannot read, with the account in detail, and 0 otherwise
 * with the status and whatever came back written out. */
static int statement(run *r, zu_conn *conn, const zy_node *node, const zy_node *query,
                     zu_status *status, zu_result **result, zu_error **e, char *detail,
                     size_t len) {
    const zy_node *params = zy_get(node, "params");
    zu_stmt *stmt = NULL;
    size_t i;

    if (params == NULL) {
        *status = zu_query(conn, query->text.ptr, query->text.len, result, e);
        return 0;
    }
    if (params->kind != ZY_SEQ) {
        return say(detail, len, "`params:` is a sequence");
    }
    *status = zu_prepare(conn, query->text.ptr, query->text.len, &stmt, e);
    if (*status != ZU_OK) {
        /* A statement that will not compile is an answer about the
         * statement and not about the parameters, so it is handed back
         * as it stands and graded where every other refusal is. */
        return 0;
    }
    for (i = 0; i < params->count; i++) {
        if (bind_param(r, stmt, &params->items[i], detail, len) != 0) {
            zu_stmt_close(stmt);
            return -1;
        }
    }
    *status = zu_execute(stmt, result, e);
    /* Closed before the result is read, which the ABI allows: a result
     * owns its values and outlives the statement that made them. */
    zu_stmt_close(stmt);
    return 0;
}

/* The statement under test, and what the case says it has to produce.
 * Returns the outcome and writes the account of why into detail. */
static outcome answer(run *r, zu_conn *conn, const zy_node *node, char *detail, size_t len) {
    const zy_node *query = zy_get(node, "query");
    const zy_node *raises = zy_get(node, "raises");
    const zy_node *columns = zy_get(node, "columns");
    const zy_node *rows = zy_get(node, "rows");
    const zy_node *setup = zy_get(node, "setup");
    zu_result *result = NULL;
    zu_status status;
    zu_error *e = NULL;
    refusal f;
    size_t i;

    if (query == NULL || query->kind != ZY_SCALAR) {
        say(detail, len, "a case names its `query:`");
        return OUT_FAILED;
    }
    if (setup != NULL) {
        const zy_node *items = NULL;
        size_t count = 0;
        if (zy_seq_or_empty(setup, &items, &count) != 0) {
            say(detail, len, "`setup:` is a sequence of statements");
            return OUT_FAILED;
        }
        for (i = 0; i < count; i++) {
            if (items[i].kind != ZY_SCALAR) {
                say(detail, len, "a setup statement is one line");
                return OUT_FAILED;
            }
            status = zu_query(conn, items[i].text.ptr, items[i].text.len, &result, &e);
            zu_result_free(result);
            result = NULL;
            if (status == ZU_OK) {
                continue;
            }
            /* A setup that fails is not a result about the statement
             * under test, so it is never a pass and never a quiet skip. */
            take(status, e, &f);
            if (ahead_of_the_engine(&f)) {
                say(detail, len, "setup %lu: %s", (unsigned long)(i + 1), f.message);
                return OUT_UNSUPPORTED;
            }
            say(detail, len, "setup %lu failed: %s", (unsigned long)(i + 1), f.message);
            return OUT_FAILED;
        }
    }

    if (statement(r, conn, node, query, &status, &result, &e, detail, len) != 0) {
        return OUT_FAILED;
    }
    if (raises != NULL) {
        outcome out;
        if (raises->kind != ZY_SCALAR) {
            zu_result_free(result);
            zu_error_free(e);
            say(detail, len, "`raises:` is a GQLSTATUS code");
            return OUT_FAILED;
        }
        if (status == ZU_OK) {
            zu_result_free(result);
            say(detail, len, "returned rows where the case wants %s", raises->text.ptr);
            return OUT_FAILED;
        }
        take(status, e, &f);
        if (f.code[0] == '\0') {
            say(detail, len, "failed with no GQLSTATUS where the case wants %s: %s",
                raises->text.ptr, f.message);
            return OUT_FAILED;
        }
        out = strcmp(f.code, raises->text.ptr) == 0 ? OUT_PASSED : OUT_FAILED;
        if (out == OUT_FAILED) {
            say(detail, len, "raised %s where the case wants %s: %s", f.code, raises->text.ptr,
                f.message);
        }
        return out;
    }

    if (columns == NULL || (columns->kind != ZY_SEQ && columns->kind != ZY_EMPTY)
        || rows == NULL) {
        zu_result_free(result);
        zu_error_free(e);
        say(detail, len, "a case says what it produces, with `columns:` and `rows:` or `raises:`");
        return OUT_FAILED;
    }
    if (status != ZU_OK) {
        take(status, e, &f);
        say(detail, len, "%s", f.message);
        return ahead_of_the_engine(&f) ? OUT_UNSUPPORTED : OUT_FAILED;
    }
    /* The comparison happens before the result is freed, because a
     * string that came back is borrowed from it. The export happens
     * after it and not before, because the export takes the result and
     * those same strings go with it; what it leaves behind is a NULL
     * the free below is a no-op on. */
    {
        outcome out = compare(r, columns, rows, result, detail, len) == 0 ? OUT_PASSED : OUT_FAILED;
        const zy_node *arrow = zy_get(node, "arrow");
        if (out == OUT_PASSED && arrow != NULL) {
            const zy_node *want = NULL;
            size_t count = 0;
            (void)zy_seq_or_empty(rows, &want, &count);
            if (arrow_export(r, &result, arrow, (uint64_t)count, detail, len) != 0) {
                out = OUT_FAILED;
            }
        }
        zu_result_free(result);
        return out;
    }
}

static void one(run *r, const char *suite, const zy_node *load, const zy_node *node) {
    const zy_node *name = zy_get(node, "name");
    char path[1024];
    char detail[1024];
    zu_conn *conn = NULL;
    zu_status status;
    zu_error *e = NULL;
    refusal f;
    outcome out;

    detail[0] = '\0';
    if (name == NULL || name->kind != ZY_SCALAR) {
        report(r, suite, "?", node->line, OUT_FAILED, "a case has a `name:`");
        return;
    }
    snprintf(path, sizeof path, "%s/%s-%s.zu", r->dir, suite, name->text.ptr);
    /* A run left behind by an earlier one would be opened rather than
     * created, since both create calls refuse a path that is already a
     * database. Removing it here is what makes a second run of the same
     * directory the same run as the first. */
    remove(path);

    if (load != NULL) {
        if (apply_load(r, path, load, detail, sizeof detail) != 0) {
            /* The prefix is eighteen bytes ahead of a detail that can
             * fill its own buffer, and gcc sizes the destination
             * against what the format could write rather than against
             * what it usually does, so a matching 1024 is a
             * format-truncation error where warnings are errors. */
            char whole[sizeof detail + 32];
            snprintf(whole, sizeof whole, "the suite's load: %s", detail);
            report(r, suite, name->text.ptr, node->line, OUT_FAILED, whole);
            cv_arena_reset(r->arena);
            return;
        }
        status = zu_open_z(path, &conn, &e);
    } else {
        status = zu_create_z(path, &conn, &e);
    }
    if (status != ZU_OK) {
        take(status, e, &f);
        say(detail, sizeof detail, "%s %s: %s", load != NULL ? "opening" : "creating", path,
            f.message);
        report(r, suite, name->text.ptr, node->line, OUT_FAILED, detail);
        cv_arena_reset(r->arena);
        return;
    }

    r->conn = conn;
    out = answer(r, conn, node, detail, sizeof detail);
    r->conn = NULL;
    zu_conn_close(conn);
    report(r, suite, name->text.ptr, node->line, out, detail);
    cv_arena_reset(r->arena);
    /* A failure leaves its database behind, which is the one thing
     * somebody looking at the report will want. Everything else goes,
     * because a corpus of five hundred cases is five hundred files. */
    if (out != OUT_FAILED) {
        remove(path);
    }
}

/* ---- one file ---- */

/* Every case in one file. Returns -1 for a file that is not a suite,
 * which stops the run: a corpus that cannot be read is not a corpus
 * that failed. */
static int suite_file(run *r, const char *path) {
    char err[512];
    zy_doc *doc = zy_parse_file(path, err, sizeof err);
    const zy_node *root, *schema, *suite, *cases;
    char *stop = NULL;
    long version;
    size_t i;

    if (doc == NULL) {
        fprintf(stderr, "runner: %s\n", err);
        return -1;
    }
    root = zy_root(doc);
    schema = zy_get(root, "schema");
    suite = zy_get(root, "suite");
    cases = zy_get(root, "cases");
    if (schema == NULL || schema->kind != ZY_SCALAR) {
        fprintf(stderr, "runner: %s does not open with `schema:`\n", path);
        zy_free(doc);
        return -1;
    }
    version = strtol(schema->text.ptr, &stop, 10);
    if (stop == schema->text.ptr || *stop != '\0' || version != RUNNER_SCHEMA) {
        fprintf(stderr, "runner: %s is schema %s and the runner reads schema %d\n", path,
                schema->text.ptr, RUNNER_SCHEMA);
        zy_free(doc);
        return -1;
    }
    if (suite == NULL || suite->kind != ZY_SCALAR || cases == NULL || cases->kind != ZY_SEQ ||
        cases->count == 0) {
        fprintf(stderr, "runner: %s is a suite name and the cases under it\n", path);
        zy_free(doc);
        return -1;
    }
    for (i = 0; i < cases->count; i++) {
        one(r, suite->text.ptr, zy_get(root, "load"), &cases->items[i]);
    }
    zy_free(doc);
    return 0;
}

/* That the directory is there and can be written to, asked once rather
 * than five hundred times: a --dir that is not there is otherwise every
 * case failing to make its database, which is one mistake reported as
 * if it were the corpus. */
static int usable(const char *dir) {
    char path[1024];
    FILE *probe;
    snprintf(path, sizeof path, "%s/runner-probe.tmp", dir);
    probe = fopen(path, "wb");
    if (probe == NULL) {
        return -1;
    }
    fclose(probe);
    remove(path);
    return 0;
}

static int usage(void) {
    fprintf(stderr, "usage: runner --dir DIR [--strict] [--quiet] case.yaml ...\n");
    return 2;
}

int main(int argc, char **argv) {
    run r;
    int i, files = 0;

    memset(&r, 0, sizeof r);
    for (i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--dir") == 0) {
            if (++i == argc) {
                return usage();
            }
            r.dir = argv[i];
        } else if (strcmp(argv[i], "--strict") == 0) {
            r.strict = 1;
        } else if (strcmp(argv[i], "--quiet") == 0 || strcmp(argv[i], "-q") == 0) {
            r.quiet = 1;
        } else if (argv[i][0] == '-') {
            return usage();
        } else {
            files++;
        }
    }
    if (r.dir == NULL || files == 0) {
        return usage();
    }
    if (usable(r.dir) != 0) {
        fprintf(stderr, "runner: nothing can be written under %s\n", r.dir);
        return 2;
    }

    r.arena = cv_arena_new();
    if (r.arena == NULL) {
        fprintf(stderr, "runner: out of memory before the first case\n");
        return 1;
    }
    for (i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--dir") == 0) {
            i++;
        } else if (argv[i][0] != '-' && suite_file(&r, argv[i]) != 0) {
            cv_arena_free(r.arena);
            return 1;
        }
    }
    cv_arena_free(r.arena);

    printf("%lu cases, %lu passed, %lu failed, %lu unsupported\n", r.cases, r.passed, r.failed,
           r.unsupported);
    if (r.failed > 0 || (r.strict && r.unsupported > 0)) {
        return 1;
    }
    return 0;
}
