/* What the corpus YAML reader reads, and what it refuses.
 *
 * Two halves. The first is the shapes the corpus is made of, written
 * out here as literals, which is the same set the Rust reader's unit
 * tests cover and in the same order, so that the two can be read side
 * by side and a rule that exists in one and not the other is visible.
 * The second takes the case files named on the command line, parses
 * every one, and checks the shape a runner will walk. That half is the
 * one that fires when somebody writes a case: 652 cases of hand written
 * YAML are a better adversary than any literal in this file.
 *
 * Usage: yaml_test conformance/cases/one.yaml conformance/cases/two.yaml,
 * or the same directory as a glob, which is how CI passes all of them.
 *
 * The files are named rather than found, because opening a directory
 * needs dirent.h on one platform and a different header on another, and
 * a shell already knows how to expand a glob on both.
 */
#include "yaml.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static int failures;

static void check(int ok, const char *what) {
    if (!ok) {
        printf("FAIL %s\n", what);
        failures++;
    }
}

/* A scalar with this text, which is most of what these tests ask. */
static void check_text(const zy_node *n, const char *want, const char *what) {
    zy_str got = zy_text(n);
    if (got.ptr == NULL) {
        printf("FAIL %s: wanted \"%s\", found %s\n", what, want, zy_kind_name(n));
        failures++;
        return;
    }
    if (!zy_eq(got, want)) {
        printf("FAIL %s: wanted \"%s\", got \"%s\"\n", what, want, got.ptr);
        failures++;
    }
}

/* A document that has to parse. The caller frees it. */
static zy_doc *ok(const char *text) {
    char err[256];
    zy_doc *doc = zy_parse(text, strlen(text), err, sizeof err);
    if (doc == NULL) {
        printf("FAIL %s\nshould parse: %s\n", text, err);
        failures++;
    }
    return doc;
}

static void a_mapping_of_scalars_is_the_shape_everything_else_is_made_of(void) {
    zy_doc *doc = ok("schema: 1\nsuite: int\n");
    const zy_node *root = zy_root(doc);
    check_text(zy_get(root, "schema"), "1", "schema");
    check_text(zy_get(root, "suite"), "int", "suite");
    check(zy_get(root, "nothing") == NULL, "a key that is not there is not there");
    zy_free(doc);
}

static void a_sequence_item_carrying_a_mapping_is_the_same_shape_as_one_written_out(void) {
    zy_doc *doc = ok("cases:\n  - name: a\n    query: RETURN 1\n  - name: b\n    query: RETURN 2\n");
    const zy_node *cases = zy_get(zy_root(doc), "cases");
    check(cases != NULL && cases->kind == ZY_SEQ && cases->count == 2, "two cases");
    if (cases != NULL && cases->count == 2) {
        check_text(zy_get(&cases->items[0], "name"), "a", "the first name");
        check_text(zy_get(&cases->items[0], "query"), "RETURN 1", "the first query");
        check_text(zy_get(&cases->items[1], "name"), "b", "the second name");
    }
    zy_free(doc);
}

static void a_sequence_of_scalars_is_not_read_as_anything_cleverer(void) {
    zy_doc *doc = ok("columns:\n  - n\n  - m\n");
    const zy_node *columns = zy_get(zy_root(doc), "columns");
    check(columns != NULL && columns->kind == ZY_SEQ && columns->count == 2, "two columns");
    if (columns != NULL && columns->count == 2) {
        check_text(&columns->items[0], "n", "the first column");
        check_text(&columns->items[1], "m", "the second column");
    }
    zy_free(doc);
}

static void nesting_goes_as_deep_as_a_list_of_records_needs(void) {
    zy_doc *doc = ok("rows:\n"
                     "  - values:\n"
                     "      - type: LIST\n"
                     "        value:\n"
                     "          - type: INT64\n"
                     "            value: \"1\"\n");
    const zy_node *rows = zy_get(zy_root(doc), "rows");
    const zy_node *values = rows == NULL ? NULL : zy_get(&rows->items[0], "values");
    check(values != NULL && values->kind == ZY_SEQ && values->count == 1, "one value");
    if (values != NULL && values->count == 1) {
        const zy_node *list = zy_get(&values->items[0], "value");
        check_text(zy_get(&values->items[0], "type"), "LIST", "the type");
        check(list != NULL && list->kind == ZY_SEQ && list->count == 1, "one element");
        if (list != NULL && list->count == 1) {
            check_text(zy_get(&list->items[0], "value"), "1", "the element");
        }
    }
    zy_free(doc);
}

static void whether_a_scalar_was_quoted_survives_because_the_encoding_turns_on_it(void) {
    zy_doc *doc = ok("bare: 42\nquoted: \"42\"\nsingle: '42'\n");
    const zy_node *root = zy_root(doc);
    const zy_node *bare = zy_get(root, "bare");
    const zy_node *quoted = zy_get(root, "quoted");
    const zy_node *single = zy_get(root, "single");
    check_text(bare, "42", "the bare one");
    check_text(quoted, "42", "the double quoted one");
    check_text(single, "42", "the single quoted one");
    check(bare != NULL && !bare->quoted, "bare is bare");
    check(quoted != NULL && quoted->quoted, "quoted is quoted");
    check(single != NULL && single->quoted, "single quoted is quoted");
    zy_free(doc);
}

static void a_colon_inside_a_value_is_part_of_it_and_not_another_key(void) {
    zy_doc *doc = ok("query: RETURN datetime('2024-01-01T00:00:00')\n");
    check_text(zy_get(zy_root(doc), "query"), "RETURN datetime('2024-01-01T00:00:00')", "the query");
    zy_free(doc);
}

static void a_hash_is_a_comment_only_where_a_comment_can_start(void) {
    zy_doc *doc = ok("# the whole line\n"
                     "name: a  # and the end of this one\n"
                     "hash: \"a # b\"\n"
                     "word: c#d\n");
    const zy_node *root = zy_root(doc);
    check_text(zy_get(root, "name"), "a", "a comment after a value");
    check_text(zy_get(root, "hash"), "a # b", "a hash inside quotes");
    check_text(zy_get(root, "word"), "c#d", "a hash inside a word");
    zy_free(doc);
}

static void a_quote_that_never_closes_is_an_ordinary_character_inside_a_plain_scalar(void) {
    /* The second quote has a space before it, so it looks like the
     * start of a run, and there is nothing after it to close one. */
    zy_doc *doc = ok("query: RETURN cast('  42  ' AS INT64) AS n  # a note\n");
    check_text(zy_get(zy_root(doc), "query"), "RETURN cast('  42  ' AS INT64) AS n", "the query");
    zy_free(doc);
}

static void an_escape_and_a_doubled_quote_are_the_two_ways_a_quote_gets_in(void) {
    zy_doc *doc = ok("a: \"say \\\"no\\\"\"\nb: 'say ''no'''\n");
    const zy_node *root = zy_root(doc);
    check_text(zy_get(root, "a"), "say \"no\"", "the escaped one");
    check_text(zy_get(root, "b"), "say 'no'", "the doubled one");
    zy_free(doc);
}

static void the_control_characters_a_query_can_hold_all_have_an_escape(void) {
    zy_doc *doc = ok("a: \"one\\ntwo\\rthree\\tfour\\0five\"\n");
    zy_str got = zy_text(zy_get(zy_root(doc), "a"));
    /* The length carries the NUL, which is why every string here is a
     * pointer and a length and not a pointer. A reader that handed back
     * a C string alone would lose the last four characters of this one
     * and pass a test that compared with strcmp. */
    check(got.ptr != NULL && got.len == 23, "an escaped NUL is one character in the middle");
    check(got.ptr != NULL && memcmp(got.ptr, "one\ntwo\rthree\tfour\0five", 23) == 0,
          "every escape resolved");
    zy_free(doc);
}

static void a_negative_number_is_a_scalar_and_not_a_sequence(void) {
    zy_doc *doc = ok("value: -1\n");
    check_text(zy_get(zy_root(doc), "value"), "-1", "a negative number");
    zy_free(doc);
}

static void every_node_says_which_line_it_started_on(void) {
    zy_doc *doc = ok("schema: 1\n\ncases:\n  - name: a\n    query: RETURN 1\n");
    const zy_node *cases = zy_get(zy_root(doc), "cases");
    check(cases != NULL && cases->count == 1 && cases->items[0].line == 4, "the case is on line 4");
    if (cases != NULL && cases->count == 1) {
        const zy_node *query = zy_get(&cases->items[0], "query");
        check(query != NULL && query->line == 5, "the query is on line 5");
    }
    zy_free(doc);
}

static void a_key_with_nothing_under_it_is_a_node_and_every_accessor_says_no_to_it(void) {
    zy_doc *doc = ok("rows:\n");
    const zy_node *rows = zy_get(zy_root(doc), "rows");
    const zy_node *items = NULL;
    size_t count = 1;
    check(rows != NULL && rows->kind == ZY_EMPTY, "a key with nothing under it is a node");
    check(strcmp(zy_kind_name(rows), "nothing") == 0, "which is nothing");
    check(zy_text(rows).ptr == NULL, "and not a scalar");
    /* The one caller for whom empty is an answer, which is a case that
     * expects no rows back. */
    check(zy_seq_or_empty(rows, &items, &count) == 0 && count == 0, "and is the empty sequence");
    zy_free(doc);
}

static void a_key_nobody_reads_can_be_asked_for(void) {
    static const char *const known[] = {"name", "query"};
    zy_doc *doc = ok("name: a\nqeury: RETURN 1\n");
    zy_str unknown = zy_unknown(zy_root(doc), known, 2);
    check(zy_eq(unknown, "qeury"), "the typo is the unknown key");
    zy_free(doc);
    doc = ok("name: a\nquery: RETURN 1\n");
    check(zy_unknown(zy_root(doc), known, 2).ptr == NULL, "and a document without one has none");
    zy_free(doc);
}

/* Every line ending survives a checkout on Windows, which is the one
 * platform where a case file on disk is not the file that was
 * committed. */
static void a_carriage_return_at_the_end_of_a_line_is_not_part_of_the_value(void) {
    zy_doc *doc = ok("schema: 2\r\nsuite: scalar\r\n");
    const zy_node *root = zy_root(doc);
    check_text(zy_get(root, "schema"), "2", "schema through CRLF");
    check_text(zy_get(root, "suite"), "scalar", "suite through CRLF");
    zy_free(doc);
}

/* A file that will not open is an error where a file that will not
 * parse is, and it names itself, since a caller reading a directory has
 * nothing else to go on. */
static void a_file_that_is_not_there_says_so(void) {
    char err[256];
    zy_doc *doc = zy_parse_file("conformance/cases/nothing-is-here.yaml", err, sizeof err);
    check(doc == NULL && strstr(err, "nothing-is-here.yaml") != NULL,
          "a missing file names itself");
    zy_free(doc);
}

/* A C reader can be handed what a Rust one cannot, since that one takes
 * a string the compiler already proved and this one takes bytes off a
 * disk. */
static void a_byte_that_is_not_utf8_is_refused_with_a_line_number(void) {
    char err[256];
    const char text[] = "name: a\ndoc: \xC3\x28\n";
    zy_doc *doc = zy_parse(text, sizeof text - 1, err, sizeof err);
    check(doc == NULL && strstr(err, "line 2") != NULL && strstr(err, "UTF-8") != NULL,
          "a bad byte is refused and says where");
    zy_free(doc);
}

/* The promise the header makes: a document owns its bytes. Under a
 * sanitizer this is the test that fails if a node ever points into the
 * text it was parsed from. */
static void a_document_outlives_the_bytes_it_was_parsed_from(void) {
    const char *source = "name: a\nquery: RETURN 1\ncolumns:\n  - n\n";
    size_t len = strlen(source);
    char *copy = (char *)malloc(len);
    zy_doc *doc;
    const zy_node *root;
    memcpy(copy, source, len);
    doc = zy_parse(copy, len, NULL, 0);
    memset(copy, 'x', len);
    free(copy);
    root = zy_root(doc);
    check_text(zy_get(root, "name"), "a", "the name after the source is gone");
    check_text(zy_get(root, "query"), "RETURN 1", "the query after the source is gone");
    check(zy_get(root, "columns") != NULL, "and the keys are still the keys");
    zy_free(doc);
}

static void what_the_reader_does_not_read_it_refuses_and_says_where(void) {
    static const struct {
        const char *text;
        const char *want;
    } cases[] = {
        {"", "nothing in it"},
        {"  name: a\n", "line 1"},
        {"name:\ta\n", "line 1"},
        {"name: a\n   doc: b\n", "line 2"},
        {"a: 1\n b: 2\n", "line 2"},
        {"name: a\nname: b\n", "line 2"},
        {"cases:\n  -\n", "line 2"},
        {"cases:\n  -  name: a\n", "line 2"},
        {"cases:\n  - - a\n", "line 2"},
        {"columns: [n, m]\n", "line 1"},
        {"doc: >\n  folded\n", "line 1"},
        {"doc: |\n  literal\n", "line 1"},
        {"anchor: &a 1\n", "line 1"},
        {"---\nname: a\n", "line 1"},
        {"a: \"unterminated\n", "line 1"},
        {"a: \"bad \\q escape\"\n", "line 1"},
        {"a: 'unterminated\n", "line 1"},
        {"a: 1\ncases:\n      - b\n", "line 3"},
    };
    size_t i;
    for (i = 0; i < sizeof cases / sizeof cases[0]; i++) {
        char err[256];
        zy_doc *doc = zy_parse(cases[i].text, strlen(cases[i].text), err, sizeof err);
        if (doc != NULL) {
            printf("FAIL %s\nshould be refused\n", cases[i].text);
            failures++;
            zy_free(doc);
            continue;
        }
        if (strstr(err, cases[i].want) == NULL) {
            printf("FAIL %s\ngave \"%s\", which does not mention \"%s\"\n", cases[i].text, err,
                   cases[i].want);
            failures++;
        }
    }
    /* A refusal that says nothing would pass every check above, so the
     * one thing all of them share is asserted once here. */
    {
        char err[256];
        err[0] = 'x';
        check(zy_parse("", 0, err, sizeof err) == NULL && err[0] != '\0' && err[0] != 'x',
              "a refusal writes a message");
    }
}

/* ---- the cases themselves ---- */

/* The keys a case file has, from conformance/README.md. Reading them
 * here is what makes this half more than a parse: a file that parses
 * into a shape no runner can walk fails on the machine of whoever wrote
 * it rather than in nine client repositories. */
static const char *const file_keys[] = {"schema", "suite", "doc", "load", "cases"};
static const char *const case_keys[] = {"name",  "doc",    "setup",  "on",    "params",
                                        "query", "columns", "rows", "raises", "arrow"};

static size_t read_case_file(const char *path) {
    char err[512];
    zy_doc *doc = zy_parse_file(path, err, sizeof err);
    const zy_node *root, *cases;
    zy_str unknown;
    size_t i, count;
    if (doc == NULL) {
        printf("FAIL %s\n%s\n", path, err);
        failures++;
        return 0;
    }
    root = zy_root(doc);
    check_text(zy_get(root, "schema"), "4", path);
    check(zy_text(zy_get(root, "suite")).ptr != NULL, "the file names its suite");
    unknown = zy_unknown(root, file_keys, sizeof file_keys / sizeof file_keys[0]);
    if (unknown.ptr != NULL) {
        printf("FAIL %s: the file has a key called \"%s\"\n", path, unknown.ptr);
        failures++;
    }
    cases = zy_get(root, "cases");
    if (cases == NULL || cases->kind != ZY_SEQ) {
        printf("FAIL %s: cases is %s\n", path, zy_kind_name(cases));
        failures++;
        zy_free(doc);
        return 0;
    }
    for (i = 0; i < cases->count; i++) {
        const zy_node *c = &cases->items[i];
        zy_str name = zy_text(zy_get(c, "name"));
        if (name.ptr == NULL) {
            printf("FAIL %s line %lu: a case with no name\n", path, (unsigned long)c->line);
            failures++;
            continue;
        }
        if (zy_text(zy_get(c, "query")).ptr == NULL) {
            printf("FAIL %s: case %s has no query\n", path, name.ptr);
            failures++;
        }
        unknown = zy_unknown(c, case_keys, sizeof case_keys / sizeof case_keys[0]);
        if (unknown.ptr != NULL) {
            printf("FAIL %s: case %s has a key called \"%s\"\n", path, name.ptr, unknown.ptr);
            failures++;
        }
        /* Either it expects rows or it expects a refusal, and a case
         * that expects neither asserts nothing. */
        if (zy_get(c, "rows") == NULL && zy_get(c, "raises") == NULL) {
            printf("FAIL %s: case %s expects neither rows nor a raise\n", path, name.ptr);
            failures++;
        }
    }
    count = cases->count;
    zy_free(doc);
    return count;
}

int main(int argc, char **argv) {
    size_t cases = 0;
    size_t bytes = 0;
    int i;
    clock_t start;
    double seconds;

    a_mapping_of_scalars_is_the_shape_everything_else_is_made_of();
    a_sequence_item_carrying_a_mapping_is_the_same_shape_as_one_written_out();
    a_sequence_of_scalars_is_not_read_as_anything_cleverer();
    nesting_goes_as_deep_as_a_list_of_records_needs();
    whether_a_scalar_was_quoted_survives_because_the_encoding_turns_on_it();
    a_colon_inside_a_value_is_part_of_it_and_not_another_key();
    a_hash_is_a_comment_only_where_a_comment_can_start();
    a_quote_that_never_closes_is_an_ordinary_character_inside_a_plain_scalar();
    an_escape_and_a_doubled_quote_are_the_two_ways_a_quote_gets_in();
    the_control_characters_a_query_can_hold_all_have_an_escape();
    a_negative_number_is_a_scalar_and_not_a_sequence();
    every_node_says_which_line_it_started_on();
    a_key_with_nothing_under_it_is_a_node_and_every_accessor_says_no_to_it();
    a_key_nobody_reads_can_be_asked_for();
    a_carriage_return_at_the_end_of_a_line_is_not_part_of_the_value();
    a_file_that_is_not_there_says_so();
    a_byte_that_is_not_utf8_is_refused_with_a_line_number();
    a_document_outlives_the_bytes_it_was_parsed_from();
    what_the_reader_does_not_read_it_refuses_and_says_where();

    start = clock();
    for (i = 1; i < argc; i++) {
        FILE *f = fopen(argv[i], "rb");
        if (f != NULL) {
            fseek(f, 0, SEEK_END);
            bytes += (size_t)ftell(f);
            fclose(f);
        }
        cases += read_case_file(argv[i]);
    }
    seconds = (double)(clock() - start) / CLOCKS_PER_SEC;

    /* What the reading cost, printed rather than gated. Nine
     * repositories read this corpus on every CI run of each of them, so
     * the number is worth having in the log; a ceiling in wall clock on
     * a shared runner would be a flake rather than a gate, and the Rust
     * bench next door gates the shape of the curve instead. */
    if (argc > 1) {
        printf("%d files, %lu cases, %.1f KiB in %.0f ms, %.0f MiB/s\n", argc - 1,
               (unsigned long)cases, (double)bytes / 1024.0, seconds * 1e3,
               seconds > 0.0 ? (double)bytes / seconds / (1024.0 * 1024.0) : 0.0);
    }
    if (failures > 0) {
        printf("%d failed\n", failures);
        return 1;
    }
    printf("the reader reads what the corpus is written in\n");
    return 0;
}
