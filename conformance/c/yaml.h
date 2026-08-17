/* The subset of YAML the corpus is written in, read in C.
 *
 * This is the same reader as crates/zu-corpus/src/yaml.rs in the
 * language the C runner is written in, and that one is the reference:
 * where the two disagree about a file, the Rust one is right and this
 * one has a bug. A second implementation exists because the C runner
 * cannot take a Rust dependency, and because a corpus that eight
 * client repositories read is worth proving readable by something
 * other than the reader that produced it.
 *
 * The subset is two space indentation and no tabs, `- ` with exactly
 * one space, plain, single quoted and double quoted scalars on one
 * line, and comments. No flow collections, no block scalars, no
 * anchors, no aliases, no tags, no document markers, no multi document
 * streams. Anything outside it is an error with a line number rather
 * than something reinterpreted, for the reason conformance/README.md
 * gives: these files are hand written and read by people who did not
 * write them, and a construct the reader quietly took to mean
 * something else is a case that says one thing to a reviewer and
 * another to the runner.
 *
 * Whether a scalar was quoted survives parsing, because the value
 * encoding turns on it. An INT64 written bare is a number some reader
 * in some language will round, and refusing it is the point of the
 * encoding.
 *
 * A document owns every byte it hands back. The text passed to
 * zy_parse can be freed the moment it returns, and everything reachable
 * from the root lives until zy_free, which is one call and frees all of
 * it. That is worth the copy: a reader whose nodes borrow the file
 * buffer is a reader whose first caller keeps the buffer alive by
 * accident and whose second one does not.
 */
#ifndef ZU_CONFORMANCE_YAML_H
#define ZU_CONFORMANCE_YAML_H

#include <stddef.h>

/* What a node is. */
typedef enum zy_kind {
    ZY_SCALAR,
    ZY_SEQ,
    ZY_MAP,
    /* A key with nothing under it. It is a node rather than an error
     * because a case that expects no rows back writes `rows:` and
     * stops, and that is a real expectation which needs a spelling.
     * Every accessor says no to it, so a `name:` left blank is still
     * caught by whoever wanted a name. */
    ZY_EMPTY
} zy_kind;

/* A string in a document: pointer and length, and NUL terminated as
 * well so it can be printed. The length is what to pass on, because a
 * scalar can hold a NUL of its own through the \0 escape and the
 * string suite has one that does. */
typedef struct zy_str {
    const char *ptr;
    size_t len;
} zy_str;

typedef struct zy_node zy_node;

/* One node, with the line it started on so that everything downstream
 * can say where. The fields are read directly, since this is data and
 * an accessor per field would be a function call to reach a struct
 * member; the functions below are the operations that are more than
 * that. */
struct zy_node {
    zy_kind kind;
    /* ZY_SCALAR: whether it was written in quotes. */
    int quoted;
    size_t line;
    /* ZY_SCALAR: the text, with escapes resolved. */
    zy_str text;
    /* ZY_MAP: the keys, in the order they were written, one per item. */
    const zy_str *keys;
    /* ZY_SEQ: the items. ZY_MAP: the values, keys[i] to items[i]. */
    const zy_node *items;
    size_t count;
};

typedef struct zy_doc zy_doc;

/* A document, or NULL and a message in err saying which line stopped
 * the reader. The message never carries the file name, because the
 * reader is given bytes and not a path; a caller with a path prints it
 * around this. */
zy_doc *zy_parse(const char *text, size_t len, char *err, size_t err_len);

/* The same, from a file, which is what every caller has. A file that
 * cannot be read is an error in the same place as a file that cannot
 * be parsed, and this one does name the path, since it is the only
 * error here the caller could not have written itself. */
zy_doc *zy_parse_file(const char *path, char *err, size_t err_len);

/* Frees the document and everything reachable from it. A no-op on
 * NULL, so a failed parse can be freed without asking. */
void zy_free(zy_doc *doc);

/* The root node, which is the whole document. NULL only for a NULL
 * document. */
const zy_node *zy_root(const zy_doc *doc);

/* What kind of node this is, for an error that has to say what it
 * found instead of what it wanted. A node that is not there reads as
 * "nothing", which is the same answer a key with nothing under it
 * gives, because to the caller they are the same situation. */
const char *zy_kind_name(const zy_node *n);

/* The value of a key, or NULL for a missing key or a node that is not
 * a mapping. */
const zy_node *zy_get(const zy_node *n, const char *key);

/* The text of a scalar, or a zero length string with a NULL ptr for
 * anything else, which is the case to test for. */
zy_str zy_text(const zy_node *n);

/* Whether a string is a given literal. memcmp rather than strcmp
 * because a scalar can hold a NUL. */
int zy_eq(zy_str s, const char *lit);

/* The items of a sequence, counting a key with nothing under it as the
 * empty one, and returning nonzero for anything else so the caller can
 * refuse it. Only a caller for whom empty is a meaningful answer should
 * reach for this; the rest read n->items after checking the kind, so a
 * list somebody left unfinished is refused rather than read as none. */
int zy_seq_or_empty(const zy_node *n, const zy_node **items, size_t *count);

/* The first key that is not in known, so a caller can refuse a typo
 * rather than drop the field on the floor. The first rather than all of
 * them, because a caller reports one and stops, and a list would be an
 * allocation to say the same thing. A NULL ptr means every key was
 * known. */
zy_str zy_unknown(const zy_node *n, const char *const *known, size_t known_len);

#endif
