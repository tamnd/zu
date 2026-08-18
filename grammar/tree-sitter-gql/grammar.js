/// <reference types="tree-sitter-cli/dsl" />

// The grammar of zuQL, as a parser that runs where the engine cannot.
//
// The engine's parser is the definition of the language and this is not
// a second one: nothing here decides what a statement means, and a
// statement this grammar and `crates/zu-query/src/parser.rs` disagree
// about is a bug here. What it is for is the three places a statement
// is looked at rather than run. The documentation site colours code
// blocks with it, an editor uses it for highlighting, folding, and
// selection by syntax, and both of those need an answer for text that
// is being typed and is not a statement yet, which is the one thing a
// parser that reports errors cannot give.
//
// It is written from `docs/grammar.ebnf`, production by production, and
// checked against the engine by parsing every query in the conformance
// corpus: 926 statements the engine accepts, which must not hold an
// error node here. That is the whole gate. A grammar checked only by
// its own test corpus is a grammar that agrees with its author.
//
// Two things about the words. Keywords are case-insensitive and are
// ordinary identifiers to the lexer, exactly as in the engine, so `kw`
// builds a regex rather than a string and a word is a keyword only
// where the grammar expects one. And the words that are a keyword in
// one position and a variable in another are not keywords here at all:
// `DATE` before a string is a temporal type and `RETURN date` is a
// variable read, so the temporal literal is written as an identifier
// followed by a string, which is the rule the engine's parser applies.

/** A keyword, matched whatever case it is written in. */
function kw(word) {
  const pattern = word
    .split("")
    .map((c) => (/[a-z]/i.test(c) ? `[${c.toLowerCase()}${c.toUpperCase()}]` : c))
    .join("");
  return alias(token(prec(1, new RegExp(pattern))), word);
}

/**
 * What every path selector may carry behind its words: the path mode,
 * then `PATH` or `PATHS`. Both are optional, so this is spread into a
 * `seq` rather than being a rule of its own, which tree-sitter would
 * refuse for matching the empty string.
 */
function pathTail($) {
  return [optional($.path_mode), optional($._path_or_paths)];
}

/** One or more of `rule`, separated by commas. */
function commaSep1(rule) {
  return seq(rule, repeat(seq(",", rule)));
}

module.exports = grammar({
  name: "gql",

  extras: ($) => [/[\s\uFEFF]/, $.comment],

  rules: {
    source_file: ($) =>
      optional(seq(commaSepSemi($), optional(";"))),

    _statement: ($) =>
      choice(
        $.query,
        $.create_schema,
        $.drop_schema,
        $.create_graph,
        $.drop_graph,
        $.create_graph_type,
        $.drop_graph_type,
      ),

    // Queries.

    // A write may end without projecting anything and a read may not,
    // which is the engine's rule and the reason RETURN is not simply
    // optional here. Which clauses are writes is a question about
    // meaning rather than shape, so this asks the weaker question a
    // grammar can ask: a statement is clauses, and it either ends in a
    // RETURN or ends.
    query: ($) =>
      seq(
        optional($.use_clause),
        choice(seq(repeat($._reading_clause), $.return_clause), repeat1($._reading_clause)),
      ),

    use_clause: ($) => seq(kw("USE"), $.graph_ref),

    graph_ref: ($) =>
      choice(
        kw("CURRENT_PROPERTY_GRAPH"),
        kw("CURRENT_GRAPH"),
        seq(optional(seq(optional(kw("PROPERTY")), kw("GRAPH"))), $.graph_name),
      ),

    _reading_clause: ($) =>
      choice(
        $.match_clause,
        $.insert_clause,
        $.set_clause,
        $.remove_clause,
        $.delete_clause,
        $.call_clause,
        $.unwind_clause,
        $.with_clause,
      ),

    match_clause: ($) =>
      seq(
        optional(kw("OPTIONAL")),
        kw("MATCH"),
        optional($.match_mode),
        $.pattern,
        optional($.keep_clause),
        optional($.where_clause),
      ),

    // ISO 16.9, features G002 and G003. A match mode speaks about the
    // whole list of patterns where a path mode speaks about one path of
    // it: DIFFERENT EDGES, which is what a list naming none means, says
    // that no one edge answers two of the edge patterns of the list, and
    // REPEATABLE ELEMENTS lifts that. The singular noun and the BINDINGS
    // behind it are spellings of the same mode.
    match_mode: ($) =>
      choice(
        seq(kw("REPEATABLE"), choice(kw("ELEMENT"), kw("ELEMENTS")), optional($._binding_s)),
        seq(kw("DIFFERENT"), choice(kw("EDGE"), kw("EDGES")), optional($._binding_s)),
      ),

    _binding_s: ($) => choice(kw("BINDING"), kw("BINDINGS")),

    // ISO 16.9. A KEEP says a prefix once for a whole list of patterns
    // rather than once in front of each of them, and it belongs to a
    // graph pattern, which is why it is here and not in `pattern`: an
    // INSERT writes paths rather than choosing between them.
    keep_clause: ($) => seq(kw("KEEP"), $.path_prefix),

    insert_clause: ($) => seq(kw("INSERT"), $.pattern),

    set_clause: ($) => seq(kw("SET"), commaSep1($.set_item)),

    // Three things one item may set, told apart by what follows the
    // name: labels after a colon, the whole record after an equals, one
    // property after a dot.
    set_item: ($) =>
      seq(
        field("name", $._name),
        choice(
          seq(choice(":", kw("IS")), $.label_set),
          seq("=", $.property_map),
          seq(".", field("property", $._name), "=", $._expression),
        ),
      ),

    remove_clause: ($) => seq(kw("REMOVE"), commaSep1($.remove_item)),

    remove_item: ($) =>
      seq(
        field("name", $._name),
        choice(seq(choice(":", kw("IS")), $.label_set), seq(".", field("property", $._name))),
      ),

    // NODETACH is the explicit spelling of the default, which is why
    // both words stand where one word would do.
    delete_clause: ($) =>
      seq(
        optional(choice(kw("DETACH"), kw("NODETACH"))),
        kw("DELETE"),
        commaSep1($.delete_target),
      ),

    delete_target: ($) => choice($.value_block, $.variable),

    // `VALUE { ... }` is a query that answers the elements to delete.
    // VALUE is a name until the brace follows it, the same as `exists`,
    // so this is written as a name and not as a keyword.
    value_block: ($) => prec(2, seq(field("name", $.identifier), "{", $.query, "}")),

    call_clause: ($) =>
      seq(
        kw("CALL"),
        field("name", $._name),
        "(",
        optional(commaSep1($._expression)),
        ")",
        kw("YIELD"),
        commaSep1($.yield_item),
      ),

    yield_item: ($) =>
      seq(field("name", $._name), optional(seq(kw("AS"), field("alias", $._name)))),

    unwind_clause: ($) => seq(kw("UNWIND"), $._expression, kw("AS"), field("name", $._name)),

    with_clause: ($) => seq(kw("WITH"), $.projection, optional($.where_clause)),

    return_clause: ($) => seq(kw("RETURN"), $.projection),

    where_clause: ($) => seq(kw("WHERE"), $._expression),

    projection: ($) =>
      seq(
        optional(kw("DISTINCT")),
        choice("*", $.projection_item),
        repeat(seq(",", $.projection_item)),
        optional($.order_by),
        optional(seq(kw("SKIP"), $._expression)),
        optional(seq(kw("LIMIT"), $._expression)),
      ),

    projection_item: ($) => seq($._expression, optional(seq(kw("AS"), field("alias", $._name)))),

    order_by: ($) => seq(kw("ORDER"), kw("BY"), commaSep1($.sort_item)),

    sort_item: ($) =>
      seq(
        $._expression,
        optional(choice(kw("ASC"), kw("ASCENDING"), kw("DESC"), kw("DESCENDING"))),
        optional($.null_ordering),
      ),

    null_ordering: ($) => seq(kw("NULLS"), choice(kw("FIRST"), kw("LAST"))),

    // Patterns.

    pattern: ($) => commaSep1($.path),

    path: ($) =>
      seq(
        optional(seq(field("name", $._name), "=")),
        optional($.path_prefix),
        $._path_body,
      ),

    _path_body: ($) =>
      seq($._factor, repeat(choice(seq($.relationship, $._factor), $._factor))),

    _factor: ($) => choice($.node, seq($.subpath, optional($.quantifier))),

    // The parenthesized path pattern of ISO 16.11. The brackets match
    // nothing of their own: what they carry is a name for the stretch
    // they hold, a path mode for it, and a condition that has to hold of
    // it. A factor written straight against another is juxtaposition,
    // and the two node patterns where they meet describe one node. A
    // quantifier behind the brackets repeats the stretch they hold, and
    // every repetition binds the names inside them again, which is what
    // makes such a name stand for a list.
    subpath: ($) =>
      seq(
        "(",
        optional(seq(field("name", $._name), "=")),
        optional($.path_mode),
        $._path_body,
        optional($.where_clause),
        ")",
      ),

    // The seven path selectors of ISO 16.6, and the mode, which the
    // standard puts inside the prefix rather than behind it: the words
    // come in one order, the selector, then the mode, then PATH or
    // PATHS, then GROUP last of all. PATH and PATHS are noise the
    // standard allows, while GROUP is what tells k lengths from k paths
    // and is the one form whose count may be left out.
    path_prefix: ($) =>
      choice(
        seq(kw("ALL"), optional(kw("SHORTEST")), ...pathTail($)),
        seq(kw("ANY"), choice(kw("SHORTEST"), optional($.integer)), ...pathTail($)),
        seq(
          kw("SHORTEST"),
          choice(
            seq($.integer, ...pathTail($), optional($._group_or_groups)),
            seq(...pathTail($), $._group_or_groups),
          ),
        ),
        seq($.path_mode, optional($._path_or_paths)),
      ),

    _path_or_paths: ($) => choice(kw("PATH"), kw("PATHS")),

    _group_or_groups: ($) => choice(kw("GROUP"), kw("GROUPS")),

    path_mode: ($) => choice(kw("WALK"), kw("TRAIL"), kw("SIMPLE"), kw("ACYCLIC")),

    node: ($) =>
      seq(
        "(",
        optional(field("name", $._name)),
        repeat(seq(":", $.label_expr)),
        optional($.property_map),
        ")",
      ),

    label_expr: ($) => seq($._label_term, repeat(seq("|", $._label_term))),

    _label_term: ($) => seq($._label_factor, repeat(seq("&", $._label_factor))),

    _label_factor: ($) => seq(repeat("!"), $._label_atom),

    _label_atom: ($) => choice($.label, "%", seq("(", $.label_expr, ")")),

    label: ($) => $._name,

    // The seven edge patterns of ISO 18.9 as one production. Both bars
    // are the same character and a tilde takes no arrowhead, which are
    // rules the engine applies after parsing rather than in it.
    relationship: ($) =>
      seq(
        optional("<"),
        $._bar,
        optional(seq(optional($.rel_detail), $._bar)),
        optional(">"),
        optional($.quantifier),
      ),

    // The graph pattern quantifier of ISO 16.10, which says what a hop
    // range inside the brackets says and is the form the standard's own
    // examples are written in.
    quantifier: ($) =>
      choice(
        "+",
        "*",
        seq("{", optional($.integer), optional(seq(",", optional($.integer))), "}"),
      ),

    _bar: ($) => choice("-", "~"),

    rel_detail: ($) =>
      seq(
        "[",
        optional(field("name", $._name)),
        optional(seq(":", $.label, repeat(seq("|", $.label)))),
        optional(seq("*", optional($.hop_range))),
        optional($.property_map),
        "]",
      ),

    hop_range: ($) =>
      choice(seq($.integer, optional(seq("..", optional($.integer)))), seq("..", $.integer)),

    property_map: ($) =>
      seq("{", optional(commaSep1(seq(field("key", $._name), ":", $._expression))), "}"),

    // Expressions, loosest binding first.

    _expression: ($) =>
      choice(
        $.binary_expression,
        $.unary_expression,
        $.is_expression,
        $.property_access,
        $._primary,
      ),

    binary_expression: ($) =>
      choice(
        prec.left(1, seq($._expression, kw("OR"), $._expression)),
        prec.left(2, seq($._expression, kw("XOR"), $._expression)),
        prec.left(3, seq($._expression, kw("AND"), $._expression)),
        prec.left(
          5,
          seq(
            $._expression,
            choice(
              "=",
              "<>",
              "<",
              "<=",
              ">",
              ">=",
              kw("IN"),
              seq(kw("STARTS"), kw("WITH")),
              seq(kw("ENDS"), kw("WITH")),
              kw("CONTAINS"),
            ),
            $._expression,
          ),
        ),
        prec.left(6, seq($._expression, choice("+", "-"), $._expression)),
        prec.left(7, seq($._expression, choice("*", "/", "%"), $._expression)),
      ),

    unary_expression: ($) =>
      choice(
        prec.right(4, seq(kw("NOT"), $._expression)),
        prec.right(8, seq(choice("-", "+"), $._expression)),
      ),

    is_expression: ($) =>
      prec.left(
        5,
        seq(
          $._expression,
          kw("IS"),
          optional(kw("NOT")),
          choice(kw("NULL"), seq(kw("TYPED"), $.value_type)),
        ),
      ),

    property_access: ($) => prec.left(9, seq($._expression, ".", field("property", $._name))),

    _primary: ($) =>
      choice(
        $.literal,
        $.temporal_literal,
        $.parameter,
        $.function_call,
        $.cast,
        $.path_constructor,
        $.exists_block,
        $.parenthesized_expression,
        $.list,
        $.record,
        $.variable,
      ),

    parenthesized_expression: ($) => seq("(", $._expression, ")"),

    variable: ($) => $._name,

    // A name with a bracket after it. `PATH` is a name until a bracket
    // follows it, the same way `DATE` is a name until a string does, so
    // which name may stand here is a question for the binder and not
    // for the shape of the text.
    path_constructor: ($) =>
      prec(2, seq(field("name", $.identifier), "[", optional(commaSep1($._expression)), "]")),

    // A match written where a predicate goes. The brace is what tells
    // it apart from a variable called exists.
    exists_block: ($) =>
      prec(
        2,
        seq(
          field("name", $.identifier),
          "{",
          optional(kw("MATCH")),
          optional($.match_mode),
          $.pattern,
          optional($.keep_clause),
          optional($.where_clause),
          "}",
        ),
      ),

    function_call: ($) =>
      prec(
        2,
        seq(
          field("name", $._name),
          "(",
          optional(kw("DISTINCT")),
          optional(choice("*", commaSep1($._expression))),
          ")",
        ),
      ),

    cast: ($) => seq(kw("CAST"), "(", $._expression, kw("AS"), $.value_type, ")"),

    list: ($) => seq("[", optional(commaSep1($._expression)), "]"),

    record: ($) => $.property_map,

    // Value types.

    value_type: ($) =>
      seq(
        $._type_component,
        repeat(seq("|", $._type_component)),
        optional(seq(kw("NOT"), kw("NULL"))),
      ),

    _type_component: ($) =>
      choice(
        seq(
          kw("ANY"),
          optional(
            choice(kw("VALUE"), kw("RECORD"), kw("GRAPH"), seq(kw("PROPERTY"), kw("VALUE"))),
          ),
        ),
        seq(kw("RECORD"), optional($.record_fields)),
        $.list_type,
        seq(
          $.type_name,
          optional(seq("(", commaSep1($.integer), ")")),
          optional(seq($.list_name, optional(seq("(", $.integer, ")")))),
        ),
      ),

    // Right associative because `IS TYPED LIST < INT >` and a list type
    // followed by a comparison are the same three tokens, and the
    // engine reads the angle bracket as the element type it opens.
    list_type: ($) =>
      prec.right(
        seq(
          $.list_name,
          optional(seq("<", $.value_type, ">")),
          optional(seq("(", $.integer, ")")),
        ),
      ),

    list_name: ($) => seq(optional(kw("GROUP")), choice(kw("LIST"), kw("ARRAY"))),

    record_fields: ($) =>
      seq("{", optional(commaSep1(seq(field("name", $._name), "::", $.value_type))), "}"),

    // A type is one word or two, and which words name a type lives in
    // `crates/zu-query/src/value_type.rs`. Spelling the table out here
    // would refuse `CAST(1 AS nosuchtype)` as a shape rather than as
    // the error it is, and the corpus asserts that error.
    type_name: ($) => prec.right(seq($.identifier, optional($.identifier))),

    // Catalog statements.

    create_schema: ($) =>
      seq(
        kw("CREATE"),
        kw("SCHEMA"),
        optional(seq(kw("IF"), kw("NOT"), kw("EXISTS"))),
        $.schema_path,
      ),

    drop_schema: ($) =>
      seq(kw("DROP"), kw("SCHEMA"), optional(seq(kw("IF"), kw("EXISTS"))), $.schema_path),

    create_graph: ($) =>
      seq(
        kw("CREATE"),
        optional(seq(kw("OR"), kw("REPLACE"))),
        optional(kw("PROPERTY")),
        kw("GRAPH"),
        optional(seq(kw("IF"), kw("NOT"), kw("EXISTS"))),
        $.graph_name,
        optional($.graph_type_ref),
        optional(seq(kw("AS"), kw("COPY"), kw("OF"), $.graph_ref)),
      ),

    drop_graph: ($) =>
      seq(
        kw("DROP"),
        optional(kw("PROPERTY")),
        kw("GRAPH"),
        optional(seq(kw("IF"), kw("EXISTS"))),
        $.graph_name,
      ),

    create_graph_type: ($) =>
      seq(
        kw("CREATE"),
        optional(seq(kw("OR"), kw("REPLACE"))),
        optional(kw("PROPERTY")),
        kw("GRAPH"),
        kw("TYPE"),
        optional(seq(kw("IF"), kw("NOT"), kw("EXISTS"))),
        field("name", $._name),
        optional(kw("AS")),
        $.graph_type_source,
      ),

    drop_graph_type: ($) =>
      seq(
        kw("DROP"),
        optional(kw("PROPERTY")),
        kw("GRAPH"),
        kw("TYPE"),
        optional(seq(kw("IF"), kw("EXISTS"))),
        field("name", $._name),
      ),

    schema_path: ($) => seq("/", optional(seq($._name, repeat(seq("/", $._name))))),

    graph_name: ($) => choice($._name, seq("/", repeat(seq($._name, "/")), $._name)),

    graph_type_ref: ($) =>
      choice(
        seq(kw("ANY"), optional(kw("PROPERTY")), optional(kw("GRAPH"))),
        seq(choice("::", kw("TYPED")), field("name", $._name)),
        $.graph_type_source,
      ),

    graph_type_source: ($) => choice($.element_type_list, seq(kw("LIKE"), field("name", $._name))),

    element_type_list: ($) => seq("{", optional(commaSep1($.element_type)), "}"),

    element_type: ($) =>
      seq(
        optional(
          seq(
            choice(kw("NODE"), kw("EDGE"), kw("RELATIONSHIP")),
            kw("TYPE"),
            field("name", $._name),
          ),
        ),
        $.node_type_pattern,
        optional(seq($.arc, $.node_type_pattern)),
      ),

    node_type_pattern: ($) => seq("(", optional($.type_body), ")"),

    arc: ($) =>
      choice(
        seq("-", "[", optional($.type_body), "]", "-", optional(">")),
        seq("<", "-", "[", optional($.type_body), "]", "-"),
        seq("~", "[", optional($.type_body), "]", "~"),
      ),

    // Every part of a body is optional and a body of nothing is `()`,
    // which is a pattern that holds of every element. Written as a
    // choice rather than four optionals because a rule that matches
    // the empty string is one an LR parser cannot place.
    type_body: ($) =>
      choice(
        seq(
          field("name", $._name),
          optional(seq(":", $.label_set)),
          optional(seq("=>", ":", $.label_set)),
          optional($.property_types),
        ),
        seq(
          ":",
          $.label_set,
          optional(seq("=>", ":", $.label_set)),
          optional($.property_types),
        ),
        seq("=>", ":", $.label_set, optional($.property_types)),
        $.property_types,
      ),

    label_set: ($) => seq($.label, repeat(seq("&", $.label))),

    property_types: ($) =>
      choice(
        seq(kw("NO"), kw("PROPERTIES")),
        seq(
          optional(kw("PROPERTIES")),
          "{",
          optional(commaSep1($.property_type)),
          "}",
        ),
      ),

    property_type: ($) =>
      seq(field("name", $._name), choice("::", kw("TYPED")), $.value_type),

    // Lexical shapes.

    _name: ($) => choice($.identifier, $.quoted_identifier),

    identifier: ($) => /[A-Za-z_][A-Za-z0-9_]*/,

    quoted_identifier: ($) => seq("`", /[^`]*/, "`"),

    parameter: ($) => /\$[A-Za-z0-9_]+/,

    literal: ($) =>
      choice(kw("NULL"), kw("TRUE"), kw("FALSE"), $.integer, $.float, $.string),

    // A name and a string is a temporal literal, which is the rule the
    // engine reads by: the name is a type only when a string follows.
    // The name may be two words, because LOCAL and ZONED are halves of
    // a name rather than names of their own.
    temporal_literal: ($) => prec(2, seq(field("type", $.type_name), $.string)),

    integer: ($) => token(choice(/[0-9]+[Mm]?/, /0[xX][0-9a-fA-F]+/, /0[oO][0-7]+/, /0[bB][01]+/)),

    float: ($) =>
      token(
        choice(
          /[0-9]+\.[0-9]+([eE][+-]?[0-9]+)?[MmFfDd]?/,
          /[0-9]+[eE][+-]?[0-9]+[MmFfDd]?/,
          /[0-9]+[FfDd]/,
        ),
      ),

    // An `@` before the quote turns escape processing off, and a
    // doubled quote is then the only way to write one.
    string: ($) =>
      token(
        choice(
          seq("'", repeat(choice(/[^'\\]/, /\\./)), "'"),
          seq('"', repeat(choice(/[^"\\]/, /\\./)), '"'),
          seq("@'", repeat(choice(/[^']/, "''")), "'"),
          seq('@"', repeat(choice(/[^"]/, '""')), '"'),
        ),
      ),

    comment: ($) => token(choice(seq("//", /[^\n]*/), seq("/*", /[^*]*\*+([^/*][^*]*\*+)*/, "/"))),
  },
});

/** The statements of a file, separated by semicolons. */
function commaSepSemi($) {
  return seq($._statement, repeat(seq(";", $._statement)));
}
