/* What the value encoding accepts, and what it refuses.
 *
 * Two halves, the same two the reader's tests have. The first is the
 * rules written out as literals, which is the set crates/zu-corpus's
 * value.rs covers and in the same order, so the two can be read side by
 * side and a rule that exists in one and not the other is visible. The
 * second decodes every value in every case file named on the command
 * line, which is where the rules meet several thousand values somebody
 * wrote by hand.
 *
 * Usage: value_test conformance/cases/scalar.yaml conformance/cases/list.yaml,
 * or the same directory as a glob, which is how CI passes all of them.
 */
#include "value.h"

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static int failures;
static cv_arena *arena;
static char err[512];

static void check(int ok, const char *what) {
    if (!ok) {
        printf("FAIL %s\n", what);
        failures++;
    }
}

/* The value a document spells, or -1 with the message in err. The
 * document is freed before this returns, which is a claim about the
 * value as much as a tidying up: a decoded value owns its strings, so a
 * runner may drop the file it read them out of. */
static int decode(const char *text, cv *out) {
    zy_doc *doc = zy_parse(text, strlen(text), err, sizeof err);
    int rc;
    if (doc == NULL) {
        printf("FAIL %s\nshould parse: %s\n", text, err);
        failures++;
        return -1;
    }
    rc = cv_decode(arena, zy_root(doc), out, err, sizeof err);
    zy_free(doc);
    return rc;
}

/* A value that has to decode. A failure prints and the returned value
 * is a NULL, so the rest of the test runs and reports what else is
 * wrong rather than stopping at the first thing. */
static cv ok(const char *text) {
    cv v;
    v.kind = CV_NULL;
    if (decode(text, &v) != 0) {
        printf("FAIL %s\nshould decode: %s\n", text, err);
        failures++;
        v.kind = CV_NULL;
    }
    return v;
}

/* A value that has to be refused, with the message saying what for. */
static void refused(const char *text, const char *want) {
    cv v;
    if (decode(text, &v) == 0) {
        printf("FAIL %s\nshould be refused, wanted %s\n", text, want);
        failures++;
        return;
    }
    if (strstr(err, want) == NULL) {
        printf("FAIL %s\nwanted %s, said %s\n", text, want, err);
        failures++;
    }
}

static void check_show(const cv *v, const char *want, const char *what) {
    char buf[512];
    size_t need = cv_show(v, buf, sizeof buf);
    if (need >= sizeof buf || strcmp(buf, want) != 0) {
        printf("FAIL %s: wanted %s, got %s\n", what, want, buf);
        failures++;
    }
}

static cv an_int(int64_t n) {
    cv v;
    v.kind = CV_INT;
    v.as.integer = n;
    return v;
}

static cv a_float(double f) {
    cv v;
    v.kind = CV_FLOAT;
    v.as.real = f;
    return v;
}

static void the_widths_a_yaml_number_carries_are_written_as_numbers(void) {
    cv v = ok("type: INT8\nvalue: -128\n");
    check(v.kind == CV_INT && v.as.integer == -128, "INT8 -128");
    v = ok("type: INT32\nvalue: 2147483647\n");
    check(v.kind == CV_INT && v.as.integer == 2147483647, "INT32 max");
    v = ok("type: BOOL\nvalue: true\n");
    check(v.kind == CV_BOOL && v.as.boolean, "BOOL true");
    v = ok("type: NULL\n");
    check(v.kind == CV_NULL, "NULL");
}

static void the_widths_it_does_not_carry_are_written_as_strings(void) {
    cv v = ok("type: INT64\nvalue: \"9223372036854775807\"\n");
    check(v.kind == CV_INT && v.as.integer == INT64_MAX, "INT64 max");
    v = ok("type: INT64\nvalue: \"-9223372036854775808\"\n");
    check(v.kind == CV_INT && v.as.integer == INT64_MIN, "INT64 min");
    v = ok("type: UINT64\nvalue: \"9223372036854775807\"\n");
    check(v.kind == CV_INT && v.as.integer == INT64_MAX, "UINT64 up to where the engine holds it");
}

static void an_int64_written_bare_is_the_defect_the_encoding_exists_to_stop(void) {
    refused("type: INT64\nvalue: 9223372036854775807\n", "written in quotes");
    refused("type: INT64\nvalue: 9223372036854775807\n", "will round it");
}

static void a_number_written_in_quotes_is_refused_the_other_way_round(void) {
    cv v;
    refused("type: INT8\nvalue: \"42\"\n", "without quotes");
    /* A string is the one type whose payload is quoted or not as YAML
     * pleases, because either way it is the same text. */
    v = ok("type: STRING\nvalue: \"42\"\n");
    check(v.kind == CV_STR && zy_eq(v.as.str, "42"), "a quoted STRING");
    v = ok("type: STRING\nvalue: 42\n");
    check(v.kind == CV_STR && zy_eq(v.as.str, "42"), "a bare STRING");
}

static void a_value_too_wide_for_the_type_it_claims_is_not_quietly_widened(void) {
    refused("type: INT8\nvalue: 128\n", "is not a INT8");
    refused("type: UINT8\nvalue: -1\n", "is not a UINT8");
    refused("type: INT32\nvalue: 2147483648\n", "is not a INT32");
    refused("type: UINT64\nvalue: \"18446744073709551615\"\n", "is not a UINT64");
    refused("type: INT64\nvalue: \"9223372036854775808\"\n", "is not a INT64");
}

static void a_float_says_which_float_because_the_three_awkward_ones_have_no_yaml_spelling(void) {
    cv minus, zero, negative_zero;
    cv v = ok("type: FLOAT64\nvalue: \"1.5\"\n");
    check(v.kind == CV_FLOAT && v.as.real == 1.5, "1.5");
    v = ok("type: FLOAT64\nvalue: \"NaN\"\n");
    check(v.kind == CV_FLOAT && isnan(v.as.real), "NaN");
    v = ok("type: FLOAT64\nvalue: \"inf\"\n");
    check(v.kind == CV_FLOAT && isinf(v.as.real) && v.as.real > 0, "inf");
    v = ok("type: FLOAT64\nvalue: \"-inf\"\n");
    check(v.kind == CV_FLOAT && isinf(v.as.real) && v.as.real < 0, "-inf");
    /* A negative zero is a different value from a zero, and saying so is
     * why cv_same compares bits. */
    minus = ok("type: FLOAT64\nvalue: \"-0.0\"\n");
    zero = a_float(0.0);
    negative_zero = a_float(-0.0);
    check(!cv_same(&minus, &zero), "-0.0 is not 0.0");
    check(cv_same(&minus, &negative_zero), "-0.0 is -0.0");
    /* strtod reads these and this encoding does not, because a case that
     * wrote one meant something a client in another language would read
     * differently or not at all. */
    refused("type: FLOAT64\nvalue: \"0x1.8p3\"\n", "is not a FLOAT64");
    refused("type: FLOAT64\nvalue: \" 1.0\"\n", "is not a FLOAT64");
    refused("type: FLOAT64\nvalue: \"Infinity\"\n", "is not a FLOAT64");
    refused("type: FLOAT64\nvalue: \"1e400\"\n", "is not a FLOAT64");
    refused("type: FLOAT64\nvalue: \"1.0.0\"\n", "is not a FLOAT64");
}

static void a_nan_matches_a_nan_whatever_the_hardware_put_in_its_sign_and_payload(void) {
    cv want = ok("type: FLOAT64\nvalue: \"NaN\"\n");
    cv plain = a_float((double)NAN);
    cv negative = a_float(-(double)NAN);
    cv computed = a_float((double)INFINITY - (double)INFINITY);
    cv zero = a_float(0.0);
    check(cv_same(&want, &plain), "a NaN is a NaN");
    check(cv_same(&want, &negative), "a NaN with its sign bit set is a NaN");
    check(cv_same(&want, &computed), "a NaN the hardware made is a NaN");
    check(!cv_same(&want, &zero), "a NaN is not a zero");
}

static void a_float32_is_narrowed_so_a_case_asserts_what_the_narrower_type_can_hold(void) {
    cv narrow = ok("type: FLOAT32\nvalue: \"0.1\"\n");
    cv wide = ok("type: FLOAT64\nvalue: \"0.1\"\n");
    cv want = a_float((double)0.1f);
    check(cv_same(&narrow, &want), "a FLOAT32 is the double the float rounds to");
    check(!cv_same(&narrow, &wide), "and that is not the double the digits spell");
}

static void an_integer_written_as_a_float_is_refused_rather_than_promoted(void) {
    refused("type: FLOAT64\nvalue: \"1\"\n", "is not a FLOAT64");
}

static void a_temporal_is_written_the_way_the_engine_prints_it(void) {
    static const char *const round_trips[] = {
        "DATE", "2024-02-29",
        "DATE", "0001-01-01",
        "DATE", "9999-12-31",
        "LOCALTIME", "12:34:56",
        "LOCALTIME", "00:00:00",
        "LOCALTIME", "10:30:15.500000000",
        "ZONEDTIME", "12:34:56+07:00",
        "ZONEDTIME", "12:34:56Z",
        "ZONEDTIME", "12:34:56-05:30",
        "LOCALDATETIME", "2024-01-15T10:00:00",
        "ZONEDDATETIME", "2024-01-01T00:00:00+07:00",
        "ZONEDDATETIME", "2024-01-01T00:00:00Z",
        "DURATION", "P1Y2M",
        "DURATION", "PT0S",
        "DURATION", "P1D",
        "DURATION", "PT1H30M",
        "DURATION", "PT0.500000000S",
        "DURATION", "-PT1H",
        "DURATION", "P7DT1H"};
    size_t i;
    cv zero;
    for (i = 0; i < sizeof round_trips / sizeof round_trips[0]; i += 2) {
        char text[128], want[128];
        cv v;
        snprintf(text, sizeof text, "type: %s\nvalue: \"%s\"\n", round_trips[i],
                 round_trips[i + 1]);
        snprintf(want, sizeof want, "%s \"%s\"", round_trips[i], round_trips[i + 1]);
        v = ok(text);
        check_show(&v, want, round_trips[i + 1]);
    }
    /* A duration is a count of one unit or the other, so a zero written
     * in months comes back in the kind a zero belongs to. Both readers
     * do this, and a case that wants the year-month kind has to write a
     * number that is not zero. */
    zero = ok("type: DURATION\nvalue: \"P0M\"\n");
    check_show(&zero, "DURATION \"PT0S\"", "a zero duration");
    refused("type: DATE\nvalue: \"2023-02-29\"\n", "is not a DATE");
    refused("type: DATE\nvalue: \"2024-13-01\"\n", "is not a DATE");
    refused("type: LOCALTIME\nvalue: \"24:00\"\n", "is not a LOCALTIME");
    /* A local type refuses a written zone rather than dropping it. */
    refused("type: LOCALTIME\nvalue: \"10:30+07:00\"\n", "is not a LOCALTIME");
    refused("type: LOCALDATETIME\nvalue: \"2024-01-15T10:00:00Z\"\n", "is not a LOCALDATETIME");
    /* A duration that is half months and half nanoseconds is the thing
     * the two kinds exist to prevent. */
    refused("type: DURATION\nvalue: \"P1M1D\"\n", "is not a DURATION");
    refused("type: DURATION\nvalue: \"P\"\n", "is not a DURATION");
    refused("type: DURATION\nvalue: \"PT1\"\n", "is not a DURATION");
}

/* Two spellings of one instant are one number, which is the reason a
 * zoned value stores UTC and the offset apart. */
static void a_zoned_datetime_stores_the_instant_and_remembers_the_offset(void) {
    cv east = ok("type: ZONEDDATETIME\nvalue: \"2024-01-15T10:00:00+07:00\"\n");
    cv utc = ok("type: ZONEDDATETIME\nvalue: \"2024-01-15T03:00:00Z\"\n");
    check(east.as.temporal.count == utc.as.temporal.count, "one instant is one number");
    check(east.as.temporal.offset == 420 && utc.as.temporal.offset == 0, "each keeps its zone");
    /* And they are still two values, because a case that wrote the
     * offset asked for it back. */
    check(!cv_same(&east, &utc), "the offset is part of the value");
    check_show(&east, "ZONEDDATETIME \"2024-01-15T10:00:00+07:00\"", "the zone it was written in");
}

/* The same shift as zu-common's, walked across the whole calendar so
 * that a date the engine holds and a date a case writes are one number
 * on every day of it. */
static void the_calendar_holds_from_the_first_day_to_the_last(void) {
    cv first = ok("type: DATE\nvalue: \"0001-01-01\"\n");
    cv last = ok("type: DATE\nvalue: \"9999-12-31\"\n");
    int64_t day;
    check(first.as.temporal.count == -719162, "the first day is where zu-common puts it");
    check(last.as.temporal.count == 2932896, "and so is the last");
    for (day = first.as.temporal.count; day <= last.as.temporal.count; day += 97) {
        /* The show of a date is thirty one bytes into the format
         * below, but gcc sizes the destination against what the
         * format could write rather than against what a date writes,
         * and a hand-picked sixty four is a format-truncation error
         * on the runner where warnings are errors. */
        char want[64], text[sizeof want + 32];
        cv v;
        cv made;
        made.kind = CV_TEMPORAL;
        made.as.temporal.unit = CV_DATE;
        made.as.temporal.count = day;
        made.as.temporal.offset = 0;
        /* The show of a DATE is `DATE "yyyy-mm-dd"`, and what follows
         * the name is the payload a case writes, quotes and all. */
        cv_show(&made, want, sizeof want);
        snprintf(text, sizeof text, "type: DATE\nvalue: %s\n", want + strlen("DATE "));
        v = ok(text);
        if (v.as.temporal.count != day) {
            printf("FAIL %s is day %ld and reads back as %ld\n", want, (long)day,
                   (long)v.as.temporal.count);
            failures++;
            return;
        }
    }
}

/* A nanosecond count holds about 292 years either side of 1970 and the
 * calendar runs to 9999, so a datetime past that has nowhere to go and
 * is refused rather than wrapped into a date in the past. */
static void a_datetime_wider_than_a_nanosecond_count_is_refused(void) {
    refused("type: LOCALDATETIME\nvalue: \"9999-12-31T00:00:00\"\n", "is not a LOCALDATETIME");
    refused("type: ZONEDDATETIME\nvalue: \"2300-01-01T00:00:00Z\"\n", "is not a ZONEDDATETIME");
    check(ok("type: LOCALDATETIME\nvalue: \"2262-04-11T00:00:00\"\n").kind == CV_TEMPORAL,
          "and the last year that fits still parses");
}

static void a_list_holds_encoded_values_and_not_bare_ones(void) {
    cv list = ok("type: LIST\nvalue:\n  - type: INT8\n    value: 1\n  - type: STRING\n"
                 "    value: two\n");
    check(list.kind == CV_LIST && list.as.list.count == 2, "two items");
    if (list.kind == CV_LIST && list.as.list.count == 2) {
        cv one = an_int(1);
        check(cv_same(&list.as.list.items[0], &one), "the first item");
        check(list.as.list.items[1].kind == CV_STR &&
                  zy_eq(list.as.list.items[1].as.str, "two"),
              "the second item");
    }
    refused("type: LIST\nvalue:\n  - 1\n", "a value is a mapping");
    /* The empty list is a value, and a `value:` with nothing under it is
     * how it is written. */
    list = ok("type: LIST\nvalue:\n");
    check(list.kind == CV_LIST && list.as.list.count == 0, "the empty list");
    /* Nesting, which is the one place a decoder has to recurse. */
    list = ok("type: LIST\nvalue:\n  - type: LIST\n    value:\n      - type: BOOL\n"
              "        value: true\n");
    check_show(&list, "LIST [LIST [BOOL true]]", "a list of a list");
}

/* A byte string is the octets and not the hexits they are written in,
 * and not the characters those octets happen to encode either. */
static void a_byte_string_is_the_octets_and_not_the_text_of_them(void) {
    cv b = ok("type: BYTES\nvalue: \"00AB00\"\n");
    cv other;
    check(b.kind == CV_BYTES && b.as.bytes.len == 3, "three octets");
    if (b.kind == CV_BYTES && b.as.bytes.len == 3) {
        check((unsigned char)b.as.bytes.ptr[0] == 0x00 &&
                  (unsigned char)b.as.bytes.ptr[1] == 0xAB &&
                  (unsigned char)b.as.bytes.ptr[2] == 0x00,
              "the octets they name");
    }
    /* A NUL in the middle is an octet like any other, which is why the
     * length and never strlen decides how long one is. */
    check_show(&b, "BYTES \"00AB00\"", "a byte string prints as its hexits");
    /* Both cases of a hexit spell one octet, so two spellings of one
     * value compare equal and print the one way. */
    other = ok("type: BYTES\nvalue: \"00ab00\"\n");
    check(cv_same(&b, &other), "either case of a hexit is the same value");
    check_show(&other, "BYTES \"00AB00\"", "and prints upper case");
    /* Space anywhere, which is what the standard's production allows so
     * that a long literal can be written in groups. */
    other = ok("type: BYTES\nvalue: \"00 AB 00\"\n");
    check(cv_same(&b, &other), "space between the groups changes nothing");
    /* No octets at all is a value the way the empty string is one. */
    other = ok("type: BYTES\nvalue: \"\"\n");
    check(other.kind == CV_BYTES && other.as.bytes.len == 0, "the empty byte string");
    check_show(&other, "BYTES \"\"", "which prints as no hexits");
    check(!cv_same(&b, &other), "and is not every other byte string");
    /* A byte string is never the character string its octets encode. */
    other = ok("type: STRING\nvalue: \"00AB00\"\n");
    check(!cv_same(&b, &other), "a byte string is not the text of its hexits");
    refused("type: BYTES\nvalue: \"0\"\n", "is not a BYTES");
    refused("type: BYTES\nvalue: \"00AG\"\n", "is not a BYTES");
    refused("type: BYTES\nvalue: 00\n", "in quotes");
}

static void a_type_the_engine_cannot_hold_yet_says_so_rather_than_looking_like_a_typo(void) {
    refused("type: DECIMAL\nvalue: \"1.00\"\n", "reserves");
    refused("type: INT65\nvalue: \"1\"\n", "not a type this encoding knows");
    check(cv_is_type("INT64") && cv_is_type("LIST"), "the names a load column may write");
    check(!cv_is_type("DECIMAL") && !cv_is_type("int64"), "and the ones it may not");
}

/* A node and an edge carry a name and the rows they are, which is what
 * a case can assert about them: the id in the value is a number the
 * file decided and every client builds its own file. */
/* A byte string is the octets its hexits name and not the text of the
 * hexits, which is the misread the cases are written to catch. */
static void a_byte_string_is_decoded_into_octets_and_not_kept_as_its_hexits(void) {
    cv b = ok("type: BYTES\nvalue: \"00AB00\"\n");
    check(b.kind == CV_BYTES && b.as.bytes.len == 3, "three octets");
    check(b.as.bytes.ptr[0] == 0 && (unsigned char)b.as.bytes.ptr[1] == 0xAB &&
              b.as.bytes.ptr[2] == 0,
          "the octets the hexits name, zeros and all");
    check_show(&b, "BYTES \"00AB00\"", "printed back as upper case hexits");
    /* Both cases of hexit spell the same octets, so the two decode to
     * one value. A reader that kept the source text would answer that
     * they are two, which is the whole of the case above at line 966 of
     * conformance/cases/string.yaml. */
    {
        cv lower = ok("type: BYTES\nvalue: \"00ab\"\n");
        cv upper = ok("type: BYTES\nvalue: \"00AB\"\n");
        check(cv_same(&lower, &upper), "either case of hexit is one value");
        check(!cv_same(&lower, &b), "and different octets are not");
    }
    /* The empty byte string is a value the way the empty string is. */
    b = ok("type: BYTES\nvalue: \"\"\n");
    check(b.kind == CV_BYTES && b.as.bytes.len == 0, "no octets is still a byte string");
    /* Half an octet is not an octet, and a character outside the hexits
     * names nothing at all. */
    refused("type: BYTES\nvalue: \"0\"\n", "is not a BYTES");
    refused("type: BYTES\nvalue: \"00A\"\n", "is not a BYTES");
    refused("type: BYTES\nvalue: \"00GG\"\n", "is not a BYTES");
    /* Written bare it is a number in some reader of this file, and the
     * empty one is nothing at all, so the quotes are the rule here for
     * the same reason they are the rule for an INT64. */
    refused("type: BYTES\nvalue: 00AB\n", "written in quotes");
}

static void a_node_and_an_edge_are_written_as_a_table_and_the_rows_they_are(void) {
    static const char *const wrong[] = {"type: NODE\nvalue: \"person\"\n",
                                        "type: NODE\nvalue: \"#1\"\n",
                                        "type: NODE\nvalue: \"person#-1\"\n",
                                        "type: EDGE\nvalue: \"knows#0\"\n",
                                        "type: EDGE\nvalue: \"knows#0->\"\n"};
    size_t i;
    cv v = ok("type: NODE\nvalue: \"person#1\"\n");
    check(v.kind == CV_NODE && zy_eq(v.as.node.table, "person") && v.as.node.offset == 1,
          "a node is its table and its row");
    v = ok("type: EDGE\nvalue: \"knows#0->1\"\n");
    check(v.kind == CV_EDGE && zy_eq(v.as.edge.table, "knows") && v.as.edge.src == 0 &&
              v.as.edge.dst == 1,
          "an edge is its table and the rows it runs between");
    for (i = 0; i < sizeof wrong / sizeof wrong[0]; i++) {
        refused(wrong[i], "is not a");
    }
    /* Bare rather than quoted, which is the rule the whole encoding
     * exists for, told with the reason that applies to these two. */
    refused("type: NODE\nvalue: person#1\n", "no reader has a scalar for that");
}

static void a_path_is_a_walk_and_a_sequence_that_is_not_one_is_refused(void) {
    static const char hop[] = "type: PATH\nvalue:\n  - type: NODE\n    value: \"person#0\"\n"
                              "  - type: EDGE\n    value: \"knows#0->1\"\n"
                              "  - type: NODE\n    value: \"person#1\"\n";
    cv v = ok(hop);
    check(v.kind == CV_PATH && v.as.list.count == 3, "a node, an edge and a node");
    check_show(&v, "PATH [NODE \"person#0\", EDGE \"knows#0->1\", NODE \"person#1\"]",
               "what a path prints as");
    /* A path of one node is the shortest there is, and it is a path: a
     * walk starts somewhere. */
    v = ok("type: PATH\nvalue:\n  - type: NODE\n    value: \"person#0\"\n");
    check(v.kind == CV_PATH && v.as.list.count == 1, "the shortest walk");
    refused("type: PATH\nvalue:\n  - type: NODE\n    value: \"person#0\"\n"
            "  - type: EDGE\n    value: \"knows#0->1\"\n",
            "odd number of values");
    refused("type: PATH\nvalue:\n  - type: NODE\n    value: \"person#0\"\n"
            "  - type: NODE\n    value: \"person#1\"\n  - type: NODE\n    value: \"person#2\"\n",
            "value 2 is a NODE");
    /* A path and the list of its parts print differently and are
     * different values, which is what keeps a case from passing because
     * the shapes happened to line up. */
    {
        cv path = ok(hop);
        cv list;
        list.kind = CV_LIST;
        list.as.list = path.as.list;
        check(!cv_same(&path, &list), "a path is not the list of its parts");
    }
}

static void a_mapping_that_is_not_a_value_is_refused_with_its_line(void) {
    refused("type: INT8\n", "with no `value`");
    refused("value: 1\n", "with no `type`");
    refused("type: NULL\nvalue: 1\n", "carries no `value`");
    refused("type: INT8\nvalue: 1\nnote: hi\n", "no key \"note\"");
    refused("type: INT8\nvalue:\n  - 1\n", "holds one scalar");
    refused("- 1\n", "a value is a mapping");
    /* The line is the offending value's own and not the line the value
     * it sits inside started on, which is the whole point of carrying
     * one: a list of forty values names the one that is wrong. */
    refused("type: LIST\nvalue:\n  - type: INT8\n    value: 1\n  - type: INT8\n"
            "    value: \"2\"\n",
            "line 6");
}

static void what_a_report_prints_is_what_a_case_would_be_written_as(void) {
    cv v = an_int(7);
    cv list;
    cv items[2];
    check_show(&v, "INT64 \"7\"", "an integer");
    v = a_float(1.0);
    check_show(&v, "FLOAT64 \"1.0\"", "a whole float keeps its point");
    v = a_float(0.1);
    check_show(&v, "FLOAT64 \"0.1\"", "and a fraction its shortest digits");
    v = a_float((double)NAN);
    check_show(&v, "FLOAT64 \"NaN\"", "NaN");
    v = a_float(-(double)INFINITY);
    check_show(&v, "FLOAT64 \"-inf\"", "-inf");
    v.kind = CV_NULL;
    check_show(&v, "NULL", "a null");
    items[0].kind = CV_BOOL;
    items[0].as.boolean = 1;
    items[1] = ok("type: STRING\nvalue: \"a\"\n");
    list.kind = CV_LIST;
    list.as.list.items = items;
    list.as.list.count = 2;
    check_show(&list, "LIST [BOOL true, STRING \"a\"]", "a list");
    /* A string prints as a case would write it, so that a report can be
     * pasted back into one. */
    v = ok("type: STRING\nvalue: \"a\\tb\\nc\"\n");
    check_show(&v, "STRING \"a\\tb\\nc\"", "the escapes a case writes");
}

/* A report longer than the buffer says how long it needed to be, which
 * is what a caller sizing one has to go on and what stops a truncated
 * one from being read as the whole answer. */
static void a_report_that_does_not_fit_says_how_much_it_needed(void) {
    cv v = an_int(1234567);
    char small[8];
    size_t need = cv_show(&v, small, sizeof small);
    check(need == strlen("INT64 \"1234567\""), "the length it wanted");
    check(strlen(small) == sizeof small - 1, "and what it wrote fits with room for the NUL");
    check(cv_show(&v, NULL, 0) == need, "a buffer of nothing is a way to ask the length");
}

/* A parameter is the same encoding with a name beside it, which is a
 * key cv_decode refuses and cv_typed lets through for the caller to
 * read. Nothing else about the value changes, quoting included. */
static void a_parameter_is_the_same_encoding_with_a_name_beside_it(void) {
    static const char text[] = "name: n\ntype: INT64\nvalue: \"9007199254740993\"\n";
    zy_doc *doc = zy_parse(text, sizeof text - 1, err, sizeof err);
    cv v;
    if (doc == NULL) {
        printf("FAIL a parameter should parse: %s\n", err);
        failures++;
        return;
    }
    check(cv_typed(arena, zy_root(doc), &v, err, sizeof err) == 0, "the name is not in the way");
    check(v.kind == CV_INT && v.as.integer == 9007199254740993LL, "and the value is the value");
    check(cv_decode(arena, zy_root(doc), &v, err, sizeof err) != 0,
          "while the door a row comes through still refuses it");
    zy_free(doc);
    /* What cv_typed does not stop refusing, since a name is the only
     * thing it was opened for. */
    doc = zy_parse("name: n\ntype: INT64\nvalue: 1\n", 28, err, sizeof err);
    if (doc == NULL) {
        printf("FAIL a bare int64 should parse: %s\n", err);
        failures++;
        return;
    }
    check(cv_typed(arena, zy_root(doc), &v, err, sizeof err) != 0,
          "a bare INT64 is the same mistake with a name on it");
    zy_free(doc);
}

/* A payload is the same encoding with the type factored out, which is
 * what a load column is written in: the type once at the top and bare
 * values under it. */
static void a_payload_is_the_same_encoding_with_the_type_named_once(void) {
    static const char text[] = "values:\n  - \"30\"\n  - \"40\"\n";
    zy_doc *doc = zy_parse(text, sizeof text - 1, err, sizeof err);
    const zy_node *values;
    cv v;
    if (doc == NULL) {
        printf("FAIL the fixture parses: %s\n", err);
        failures++;
        return;
    }
    values = zy_get(zy_root(doc), "values");
    check(values != NULL && values->kind == ZY_SEQ && values->count == 2, "two payloads");
    if (values != NULL && values->count == 2) {
        check(cv_payload(arena, "INT64", &values->items[0], &v, err, sizeof err) == 0, "the first");
        check(v.kind == CV_INT && v.as.integer == 30, "reads as an INT64");
        /* The quoting rule is the same rule and refuses the same way. */
        check(cv_payload(arena, "INT8", &values->items[0], &v, err, sizeof err) != 0,
              "and an INT8 in quotes is still refused");
        check(strstr(err, "without quotes") != NULL, "for the reason it always is");
    }
    zy_free(doc);
}

/* An arena reset keeps the memory and drops the values, which is what a
 * runner does between two cases. What it must not do is hand the next
 * case a string the last one is still holding. */
static void an_arena_reset_hands_the_memory_back_without_handing_a_value_over(void) {
    cv first, second;
    size_t i;
    for (i = 0; i < 4096; i++) {
        cv v = ok("type: STRING\nvalue: filler\n");
        check(v.kind == CV_STR, "the filler decodes");
    }
    first = ok("type: STRING\nvalue: before\n");
    check(zy_eq(first.as.str, "before"), "the string before the reset");
    cv_arena_reset(arena);
    second = ok("type: STRING\nvalue: after\n");
    check(zy_eq(second.as.str, "after"), "and the one after it");
}

/* ---- the corpus ---- */

/* A value and everything inside it, which is how many values a case
 * file holds rather than how many rows it writes. A list of three is
 * four values and every one of them was decoded. */
static size_t counted(const cv *v) {
    size_t n = 1, i;
    if (v->kind == CV_LIST || v->kind == CV_PATH) {
        for (i = 0; i < v->as.list.count; i++) {
            n += counted(&v->as.list.items[i]);
        }
    }
    return n;
}

/* Every value in one case file, decoded. The keys are the ones
 * conformance/README.md documents; a file with a key nobody reads is
 * the reader's test to catch, and this one only walks what it knows. */
static size_t read_case_file(const char *path) {
    zy_doc *doc = zy_parse_file(path, err, sizeof err);
    const zy_node *root, *cases, *load;
    size_t values = 0, i, j, k;
    if (doc == NULL) {
        printf("FAIL %s: %s\n", path, err);
        failures++;
        return 0;
    }
    root = zy_root(doc);
    load = zy_get(root, "load");
    if (load != NULL) {
        const zy_node *columns = zy_get(load, "columns");
        for (i = 0; columns != NULL && columns->kind == ZY_SEQ && i < columns->count; i++) {
            const zy_node *column = &columns->items[i];
            const zy_node *type = zy_get(column, "type");
            const zy_node *items = zy_get(column, "values");
            if (type == NULL || type->kind != ZY_SCALAR || items == NULL ||
                items->kind != ZY_SEQ) {
                printf("FAIL %s: a load column is a type and its values\n", path);
                failures++;
                continue;
            }
            for (j = 0; j < items->count; j++) {
                cv v;
                if (cv_payload(arena, type->text.ptr, &items->items[j], &v, err, sizeof err) != 0) {
                    printf("FAIL %s: %s\n", path, err);
                    failures++;
                    continue;
                }
                check(cv_same(&v, &v), "a loaded value is itself");
                values += counted(&v);
            }
        }
    }
    cases = zy_get(root, "cases");
    for (i = 0; cases != NULL && cases->kind == ZY_SEQ && i < cases->count; i++) {
        const zy_node *params = zy_get(&cases->items[i], "params");
        const zy_node *rows = zy_get(&cases->items[i], "rows");
        const zy_node *items;
        size_t count;
        /* A parameter is a value with a name beside it, so it goes
         * through the door that allows the name rather than the one
         * that refuses every key it does not know. */
        for (j = 0; params != NULL && params->kind == ZY_SEQ && j < params->count; j++) {
            cv v;
            if (cv_typed(arena, &params->items[j], &v, err, sizeof err) != 0) {
                printf("FAIL %s: %s\n", path, err);
                failures++;
                continue;
            }
            check(cv_same(&v, &v), "a bound value is itself");
            values += counted(&v);
        }
        if (rows == NULL || zy_seq_or_empty(rows, &items, &count) != 0) {
            continue;
        }
        for (j = 0; j < count; j++) {
            const zy_node *cells = zy_get(&items[j], "values");
            if (cells == NULL || cells->kind != ZY_SEQ) {
                printf("FAIL %s: a row is a sequence of values\n", path);
                failures++;
                continue;
            }
            for (k = 0; k < cells->count; k++) {
                char shown[512];
                cv v;
                if (cv_decode(arena, &cells->items[k], &v, err, sizeof err) != 0) {
                    printf("FAIL %s: %s\n", path, err);
                    failures++;
                    continue;
                }
                /* Two properties every decoded value owes, checked here
                 * because the corpus is a larger set of values than any
                 * literal in this file: it equals itself, and it has a
                 * spelling a report can print. */
                check(cv_same(&v, &v), "a decoded value is itself");
                if (cv_show(&v, shown, sizeof shown) == 0) {
                    printf("FAIL %s: a value with nothing to print\n", path);
                    failures++;
                }
                values += counted(&v);
            }
        }
        /* One case is one arena's worth, which is how a runner will use
         * it and therefore the way it should be exercised. */
        cv_arena_reset(arena);
    }
    zy_free(doc);
    return values;
}

int main(int argc, char **argv) {
    size_t values = 0;
    int i;
    clock_t start;
    double seconds;

    arena = cv_arena_new();
    if (arena == NULL) {
        printf("FAIL out of memory before the first test\n");
        return 1;
    }

    the_widths_a_yaml_number_carries_are_written_as_numbers();
    the_widths_it_does_not_carry_are_written_as_strings();
    an_int64_written_bare_is_the_defect_the_encoding_exists_to_stop();
    a_number_written_in_quotes_is_refused_the_other_way_round();
    a_value_too_wide_for_the_type_it_claims_is_not_quietly_widened();
    a_float_says_which_float_because_the_three_awkward_ones_have_no_yaml_spelling();
    a_nan_matches_a_nan_whatever_the_hardware_put_in_its_sign_and_payload();
    a_float32_is_narrowed_so_a_case_asserts_what_the_narrower_type_can_hold();
    an_integer_written_as_a_float_is_refused_rather_than_promoted();
    a_temporal_is_written_the_way_the_engine_prints_it();
    a_zoned_datetime_stores_the_instant_and_remembers_the_offset();
    the_calendar_holds_from_the_first_day_to_the_last();
    a_datetime_wider_than_a_nanosecond_count_is_refused();
    a_list_holds_encoded_values_and_not_bare_ones();
    a_byte_string_is_the_octets_and_not_the_text_of_them();
    a_type_the_engine_cannot_hold_yet_says_so_rather_than_looking_like_a_typo();
    a_byte_string_is_decoded_into_octets_and_not_kept_as_its_hexits();
    a_node_and_an_edge_are_written_as_a_table_and_the_rows_they_are();
    a_path_is_a_walk_and_a_sequence_that_is_not_one_is_refused();
    a_mapping_that_is_not_a_value_is_refused_with_its_line();
    what_a_report_prints_is_what_a_case_would_be_written_as();
    a_report_that_does_not_fit_says_how_much_it_needed();
    a_parameter_is_the_same_encoding_with_a_name_beside_it();
    a_payload_is_the_same_encoding_with_the_type_named_once();
    an_arena_reset_hands_the_memory_back_without_handing_a_value_over();
    cv_arena_reset(arena);

    start = clock();
    for (i = 1; i < argc; i++) {
        values += read_case_file(argv[i]);
    }
    seconds = (double)(clock() - start) / CLOCKS_PER_SEC;

    /* What the decoding cost, printed rather than gated, for the reason
     * the reader's test gives: a ceiling in wall clock on a shared
     * runner is a flake rather than a gate. */
    if (argc > 1) {
        printf("%d files, %lu values in %.0f ms\n", argc - 1, (unsigned long)values, seconds * 1e3);
    }
    cv_arena_free(arena);
    if (failures > 0) {
        printf("%d failed\n", failures);
        return 1;
    }
    printf("the encoding decodes what the corpus is written in\n");
    return 0;
}
