/* The `{type, value}` encoding a case writes its values in, in C.
 *
 * This is crates/zu-corpus/src/value.rs in the language the C runner is
 * written in, and that one is the reference in the same way and for the
 * same reason its reader is: where the two disagree about a case, the
 * Rust one is right and this one has a bug. conformance/README.md is
 * where the encoding itself is written down, and the rules this file
 * enforces are that document and not an invention of it.
 *
 * The rule that carries the whole encoding is which types are written
 * in quotes. INT64, UINT64, the two floats and every temporal type are
 * quoted, because a bare 9007199254740993 is a number and some reader
 * of this file, in some language, will hand it to a double and give
 * back a value one less. Everything a YAML scalar carries exactly is
 * written bare, so that a reader cannot take it for a string. A case on
 * the wrong side of that line is refused rather than read leniently,
 * which is the one thing this file exists to do.
 *
 * A value is a small tagged union and never a pointer to one. A row of
 * a case is an array of them, a list is an array of them, and the only
 * things allocated are the strings and the arrays. They come out of an
 * arena the caller owns, so a runner that decodes a case, compares it
 * and moves on frees the whole case with one call and does not have to
 * walk a tree to do it.
 */
#ifndef ZU_CONFORMANCE_VALUE_H
#define ZU_CONFORMANCE_VALUE_H

#include <stddef.h>
#include <stdint.h>

#include "yaml.h"

/* What a value is at runtime, which is narrower than what a case may
 * declare: INT8 and INT64 are both CV_INT here, as they are both an
 * i64 in the engine. The declared type is still the contract, and what
 * a runner can check of it is a question for the runner and not for the
 * value. */
typedef enum cv_kind {
    CV_NULL,
    CV_BOOL,
    CV_INT,
    CV_FLOAT,
    CV_STR,
    /* GV35, a sequence of octets. Its own kind and not a string with a
     * different name, because the two are not comparable in GQL and a
     * byte string that compared equal to the characters its octets
     * happen to encode would be a case passing for the wrong reason. */
    CV_BYTES,
    CV_TEMPORAL,
    CV_LIST,
    /* A node and an edge carry the name of their table rather than its
     * id, because the id is a number the file decided and every client
     * builds its own file. The name is what a case can assert, so the
     * runner asks the connection for it and compares names. */
    CV_NODE,
    CV_EDGE,
    /* A walk: nodes and edges alternating, a node at both ends. Same
     * shape as a list and a different kind, because a path that
     * compared equal to the list of its parts would be a case passing
     * for the wrong reason. */
    CV_PATH
} cv_kind;

/* Which temporal a value is, and therefore what its count means. A
 * temporal is a number and a meaning and never a struct of named parts,
 * which is the shape crates/zu-common/src/temporal.rs keeps them in:
 * the calendar arithmetic happens where text is read and written, so a
 * comparison is an integer comparison. */
typedef enum cv_unit {
    /* days from 1970-01-01 */
    CV_DATE,
    /* nanoseconds from midnight */
    CV_LOCALTIME,
    /* nanoseconds from midnight in the offset's own day */
    CV_ZONEDTIME,
    /* nanoseconds from 1970-01-01T00:00:00 */
    CV_LOCALDATETIME,
    /* nanoseconds from the epoch in UTC, with the offset it was written
     * in kept beside it, so two spellings of one instant are one number
     * and each still prints in its own zone */
    CV_ZONEDDATETIME,
    /* months */
    CV_YEARMONTH,
    /* nanoseconds */
    CV_DAYTIME
} cv_unit;

typedef struct cv cv;

/* One value. The union is named rather than anonymous because an
 * anonymous one is C11 and this is C99, which is what a client
 * repository picking these files up is most likely to have. */
struct cv {
    cv_kind kind;
    union {
        /* CV_BOOL */
        int boolean;
        /* CV_INT */
        int64_t integer;
        /* CV_FLOAT */
        double real;
        /* CV_STR, which may hold a NUL and so is never read with
         * strlen. It is NUL terminated as well, for printing. */
        zy_str str;
        /* CV_BYTES, the octets themselves and not the hexits they are
         * written in. It holds a NUL as readily as any other octet, so
         * like a string it is never read with strlen, and unlike a
         * string it is never printed as itself. */
        zy_str bytes;
        /* CV_TEMPORAL. The offset is in minutes and is zero for every
         * unit but the two zoned ones. */
        struct {
            cv_unit unit;
            int64_t count;
            int offset;
        } temporal;
        /* CV_LIST and CV_PATH, the composite types and therefore the
         * places a decoder has to recurse. */
        struct {
            const cv *items;
            size_t count;
        } list;
        /* CV_NODE */
        struct {
            zy_str table;
            uint64_t offset;
        } node;
        /* CV_EDGE. Which of two parallel edges it is is not carried,
         * because that is the order the loader wrote them in and not
         * anything the case chose. */
        struct {
            zy_str table;
            uint64_t src;
            uint64_t dst;
        } edge;
    } as;
};

/* Where the strings and the arrays of a decoded value live. One arena
 * per case is the shape this is for: decode the rows, compare them,
 * reset, and the next case starts with the chunks already warm. */
typedef struct cv_arena cv_arena;

cv_arena *cv_arena_new(void);

/* Drops every value in the arena and keeps the memory, which is what a
 * runner wants between two cases. */
void cv_arena_reset(cv_arena *a);

/* Frees the arena and everything in it. A no-op on NULL. */
void cv_arena_free(cv_arena *a);

/* Memory from the arena, aligned for anything in a value, or NULL if
 * it could not be had. It is here for a caller that builds values
 * rather than decoding them: the runner turns what the engine gave back
 * into a cv to compare it, and a list of those needs an array
 * somewhere that lives exactly as long as the case does. */
void *cv_alloc(cv_arena *a, size_t bytes);

/* The value a `{type, value}` mapping describes: 0 and a value in out,
 * or -1 and a message in err saying which line is wrong and why. The
 * message reads `line N: ...`, as the reader's do, because a case file
 * is read by whoever wrote the case and a line number is what they
 * have. */
int cv_decode(cv_arena *a, const zy_node *node, cv *out, char *err, size_t err_len);

/* The `type` and `value` of a mapping that carries more than those two,
 * which is a parameter: it is a value with a name, and the name belongs
 * to the case rather than to the encoding. The keys are the caller's to
 * check, since only the caller knows which others it allows. */
int cv_typed(cv_arena *a, const zy_node *node, cv *out, char *err, size_t err_len);

/* The value a bare payload spells under a type read from somewhere
 * else, which is the encoding with the type factored out: a load column
 * names its type once and its values are payloads under it. The quoting
 * rule is the same one, checked the same way. */
int cv_payload(cv_arena *a, const char *ty, const zy_node *value, cv *out, char *err,
               size_t err_len);

/* Whether a name is a type this encoding knows, for a caller that has
 * to decide something from the name before it has a value: a load
 * column picks a loader column type from it. Nonzero for a type. */
int cv_is_type(const char *ty);

/* How a value reads in a failure report, in the encoding's own spelling
 * so that it can be pasted into a case. Returns the length the text
 * would have, as snprintf does, so a caller can size a buffer or notice
 * it was truncated; buf is always NUL terminated when len is nonzero.
 *
 * A float prints as the shortest text that reads back as the same
 * double, which is the property a report needs and printf's default
 * does not have, and it picks between positional and exponent text on
 * the same threshold the Rust one does so that the two agree on shape.
 * They can still disagree on the last digit, when the shortest text
 * sits exactly between two spellings of one double: this one takes the
 * even digit and the Rust one takes the one further from zero, which
 * over half a million random doubles is fifty one of them and over the
 * corpus is none. Both spellings read back as the double they came
 * from, so that is a difference in two reports and not in two values,
 * and cv_same is what decides whether a case passed. */
size_t cv_show(const cv *v, char *buf, size_t len);

/* Whether two values are the same value.
 *
 * Not a memcmp and not an equality operator, for one reason: a float.
 * NaN is not equal to itself and a case asserting NaN has to pass, and
 * 0.0 is equal to -0.0 and a case asserting -0.0 has to fail on 0.0,
 * because the sign of a zero is exactly the sort of thing that survives
 * one binding and not another. Comparing the bits answers the second.
 *
 * It does not answer the first, because a NaN is a family of bit
 * patterns rather than one: inf - inf has the sign bit set on x86-64
 * and clear on aarch64, and neither is the constant a case that writes
 * NaN decodes to. The sign and the payload of a NaN are the hardware's
 * business, so every NaN is the same NaN here. */
int cv_same(const cv *a, const cv *b);

#endif
