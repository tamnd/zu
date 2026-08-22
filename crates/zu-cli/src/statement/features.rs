//! Generated from `crates/zu-common/artifacts/gql-features.xml`, the
//! ISO/IEC 39075:2024 artifact that lists every optional language
//! feature the standard defines. Do not edit by hand: run
//! `ZU_UPDATE_STATEMENT=1 cargo test -p zu-cli --test statement`.
//!
//! Codes and descriptions are the standard's, verbatim, with runs of
//! whitespace folded to one space so a description is one line.

use super::Feature;

/// Every optional feature of ISO/IEC 39075:2024, in code order.
pub(super) static FEATURES: &[Feature] = &[
    Feature {
        code: "G002",
        description: "Different-edges match mode",
    },
    Feature {
        code: "G003",
        description: "Explicit REPEATABLE ELEMENTS keyword",
    },
    Feature {
        code: "G004",
        description: "Path variables",
    },
    Feature {
        code: "G005",
        description: "Path search prefix in a path pattern",
    },
    Feature {
        code: "G006",
        description: "Graph pattern KEEP clause: path mode prefix",
    },
    Feature {
        code: "G007",
        description: "Graph pattern KEEP clause: path search prefix",
    },
    Feature {
        code: "G010",
        description: "Explicit WALK keyword",
    },
    Feature {
        code: "G011",
        description: "Advanced path modes: TRAIL",
    },
    Feature {
        code: "G012",
        description: "Advanced path modes: SIMPLE",
    },
    Feature {
        code: "G013",
        description: "Advanced path modes: ACYCLIC",
    },
    Feature {
        code: "G014",
        description: "Explicit PATH/PATHS keywords",
    },
    Feature {
        code: "G015",
        description: "All path search: explicit ALL keyword",
    },
    Feature {
        code: "G016",
        description: "Any path search",
    },
    Feature {
        code: "G017",
        description: "All shortest path search",
    },
    Feature {
        code: "G018",
        description: "Any shortest path search",
    },
    Feature {
        code: "G019",
        description: "Counted shortest path search",
    },
    Feature {
        code: "G020",
        description: "Counted shortest group search",
    },
    Feature {
        code: "G030",
        description: "Path multiset alternation",
    },
    Feature {
        code: "G031",
        description: "Path multiset alternation: variable length path operands",
    },
    Feature {
        code: "G032",
        description: "Path pattern union",
    },
    Feature {
        code: "G033",
        description: "Path pattern union: variable length path operands",
    },
    Feature {
        code: "G035",
        description: "Quantified paths",
    },
    Feature {
        code: "G036",
        description: "Quantified edges",
    },
    Feature {
        code: "G037",
        description: "Questioned paths",
    },
    Feature {
        code: "G038",
        description: "Parenthesized path pattern expression",
    },
    Feature {
        code: "G039",
        description: "Simplified path pattern expression: full defaulting",
    },
    Feature {
        code: "G041",
        description: "Non-local element pattern predicates",
    },
    Feature {
        code: "G043",
        description: "Complete full edge patterns",
    },
    Feature {
        code: "G044",
        description: "Basic abbreviated edge patterns",
    },
    Feature {
        code: "G045",
        description: "Complete abbreviated edge patterns",
    },
    Feature {
        code: "G046",
        description: "Relaxed topological consistency: adjacent vertex patterns",
    },
    Feature {
        code: "G047",
        description: "Relaxed topological consistency: concise edge patterns",
    },
    Feature {
        code: "G048",
        description: "Parenthesized path pattern: subpath variable declaration",
    },
    Feature {
        code: "G049",
        description: "Parenthesized path pattern: path mode prefix",
    },
    Feature {
        code: "G050",
        description: "Parenthesized path pattern: WHERE clause",
    },
    Feature {
        code: "G051",
        description: "Parenthesized path pattern: non-local predicates",
    },
    Feature {
        code: "G060",
        description: "Bounded graph pattern quantifiers",
    },
    Feature {
        code: "G061",
        description: "Unbounded graph pattern quantifiers",
    },
    Feature {
        code: "G074",
        description: "Label expression: wildcard label",
    },
    Feature {
        code: "G080",
        description: "Simplified path pattern expression: basic defaulting",
    },
    Feature {
        code: "G081",
        description: "Simplified path pattern expression: full overrides",
    },
    Feature {
        code: "G082",
        description: "Simplified path pattern expression: basic overrides",
    },
    Feature {
        code: "G100",
        description: "ELEMENT_ID function",
    },
    Feature {
        code: "G110",
        description: "IS DIRECTED predicate",
    },
    Feature {
        code: "G111",
        description: "IS LABELED predicate",
    },
    Feature {
        code: "G112",
        description: "IS SOURCE and IS DESTINATION predicate",
    },
    Feature {
        code: "G113",
        description: "ALL_DIFFERENT predicate",
    },
    Feature {
        code: "G114",
        description: "SAME predicate",
    },
    Feature {
        code: "G115",
        description: "PROPERTY_EXISTS predicate",
    },
    Feature {
        code: "GA01",
        description: "IEEE 754 floating point operations",
    },
    Feature {
        code: "GA03",
        description: "Explicit ordering of nulls",
    },
    Feature {
        code: "GA04",
        description: "Universal comparison",
    },
    Feature {
        code: "GA05",
        description: "Cast specification",
    },
    Feature {
        code: "GA06",
        description: "Value type predicate",
    },
    Feature {
        code: "GA07",
        description: "Ordering by discarded binding variables",
    },
    Feature {
        code: "GA08",
        description: "GQL-status objects with diagnostic records",
    },
    Feature {
        code: "GA09",
        description: "Comparison of paths",
    },
    Feature {
        code: "GB01",
        description: "Long identifiers",
    },
    Feature {
        code: "GB02",
        description: "Double minus sign comments",
    },
    Feature {
        code: "GB03",
        description: "Double solidus comments",
    },
    Feature {
        code: "GC01",
        description: "Graph schema management",
    },
    Feature {
        code: "GC02",
        description: "Graph schema management: IF [ NOT ] EXISTS",
    },
    Feature {
        code: "GC03",
        description: "Graph type: IF [ NOT ] EXISTS",
    },
    Feature {
        code: "GC04",
        description: "Graph management",
    },
    Feature {
        code: "GC05",
        description: "Graph management: IF [ NOT ] EXISTS",
    },
    Feature {
        code: "GD01",
        description: "Updatable graphs",
    },
    Feature {
        code: "GD02",
        description: "Graph label set changes",
    },
    Feature {
        code: "GD03",
        description: "DELETE statement: subquery support",
    },
    Feature {
        code: "GD04",
        description: "DELETE statement: simple expression support",
    },
    Feature {
        code: "GE01",
        description: "Graph reference value expressions",
    },
    Feature {
        code: "GE02",
        description: "Binding table reference value expressions",
    },
    Feature {
        code: "GE03",
        description: "Let-binding of variables in expressions",
    },
    Feature {
        code: "GE04",
        description: "Graph parameters",
    },
    Feature {
        code: "GE05",
        description: "Binding table parameters",
    },
    Feature {
        code: "GE06",
        description: "Path value construction",
    },
    Feature {
        code: "GE07",
        description: "Boolean XOR",
    },
    Feature {
        code: "GE08",
        description: "Reference parameters",
    },
    Feature {
        code: "GE09",
        description: "Horizontal aggregation",
    },
    Feature {
        code: "GF01",
        description: "Enhanced numeric functions",
    },
    Feature {
        code: "GF02",
        description: "Trigonometric functions",
    },
    Feature {
        code: "GF03",
        description: "Logarithmic functions",
    },
    Feature {
        code: "GF04",
        description: "Enhanced path functions",
    },
    Feature {
        code: "GF05",
        description: "Multi-character TRIM function",
    },
    Feature {
        code: "GF06",
        description: "Explicit TRIM function",
    },
    Feature {
        code: "GF07",
        description: "Byte string TRIM function",
    },
    Feature {
        code: "GF10",
        description: "Advanced aggregate functions: general set functions",
    },
    Feature {
        code: "GF11",
        description: "Advanced aggregate functions: binary set functions",
    },
    Feature {
        code: "GF12",
        description: "CARDINALITY function",
    },
    Feature {
        code: "GF13",
        description: "SIZE function",
    },
    Feature {
        code: "GF20",
        description: "Aggregate functions in sort keys",
    },
    Feature {
        code: "GG01",
        description: "Graph with an open graph type",
    },
    Feature {
        code: "GG02",
        description: "Graph with a closed graph type",
    },
    Feature {
        code: "GG03",
        description: "Graph type inline specification",
    },
    Feature {
        code: "GG04",
        description: "Graph type like a graph",
    },
    Feature {
        code: "GG05",
        description: "Graph from a graph source",
    },
    Feature {
        code: "GG20",
        description: "Explicit element type names",
    },
    Feature {
        code: "GG21",
        description: "Explicit element type key label sets",
    },
    Feature {
        code: "GG22",
        description: "Element type key label set inference",
    },
    Feature {
        code: "GG23",
        description: "Optional element type key label sets",
    },
    Feature {
        code: "GG24",
        description: "Relaxed structural consistency",
    },
    Feature {
        code: "GG25",
        description: "Relaxed key label set uniqueness for edge types",
    },
    Feature {
        code: "GG26",
        description: "Relaxed property value type consistency",
    },
    Feature {
        code: "GH01",
        description: "External object references",
    },
    Feature {
        code: "GH02",
        description: "Undirected edge patterns",
    },
    Feature {
        code: "GL01",
        description: "Hexadecimal literals",
    },
    Feature {
        code: "GL02",
        description: "Octal literals",
    },
    Feature {
        code: "GL03",
        description: "Binary literals",
    },
    Feature {
        code: "GL04",
        description: "Exact number in common notation without suffix",
    },
    Feature {
        code: "GL05",
        description: "Exact number in common notation or as decimal integer with suffix",
    },
    Feature {
        code: "GL06",
        description: "Exact number in scientific notation with suffix",
    },
    Feature {
        code: "GL07",
        description: "Approximate number in common notation or as decimal integer with suffix",
    },
    Feature {
        code: "GL08",
        description: "Approximate number in scientific notation with suffix",
    },
    Feature {
        code: "GL09",
        description: "Optional float number suffix",
    },
    Feature {
        code: "GL10",
        description: "Optional double number suffix",
    },
    Feature {
        code: "GL11",
        description: "Opt-out character escaping",
    },
    Feature {
        code: "GL12",
        description: "SQL datetime and interval formats",
    },
    Feature {
        code: "GP01",
        description: "Inline procedure",
    },
    Feature {
        code: "GP02",
        description: "Inline procedure with implicit nested variable scope",
    },
    Feature {
        code: "GP03",
        description: "Inline procedure with explicit nested variable scope",
    },
    Feature {
        code: "GP04",
        description: "Named procedure calls",
    },
    Feature {
        code: "GP05",
        description: "Procedure-local value variable definitions",
    },
    Feature {
        code: "GP06",
        description: "Procedure-local value variable definitions: value variables based on simple expressions",
    },
    Feature {
        code: "GP07",
        description: "Procedure-local value variable definitions: value variable based on subqueries",
    },
    Feature {
        code: "GP08",
        description: "Procedure-local binding table variable definitions",
    },
    Feature {
        code: "GP09",
        description: "Procedure-local binding table variable definitions: binding table variables based on simple expressions or references",
    },
    Feature {
        code: "GP10",
        description: "Procedure-local binding table variable definitions: binding table variables based on subqueries",
    },
    Feature {
        code: "GP11",
        description: "Procedure-local graph variable definitions",
    },
    Feature {
        code: "GP12",
        description: "Procedure-local graph variable definitions: graph variables based on simple expressions or references",
    },
    Feature {
        code: "GP13",
        description: "Procedure-local graph variable definitions: graph variables based on subqueries",
    },
    Feature {
        code: "GP14",
        description: "Binding tables as procedure arguments",
    },
    Feature {
        code: "GP15",
        description: "Graphs as procedure arguments",
    },
    Feature {
        code: "GP16",
        description: "AT schema clause",
    },
    Feature {
        code: "GP17",
        description: "Binding variable definition block",
    },
    Feature {
        code: "GP18",
        description: "Catalog and data statement mixing",
    },
    Feature {
        code: "GQ01",
        description: "USE graph clause",
    },
    Feature {
        code: "GQ02",
        description: "Composite query: OTHERWISE",
    },
    Feature {
        code: "GQ03",
        description: "Composite query: UNION",
    },
    Feature {
        code: "GQ04",
        description: "Composite query: EXCEPT DISTINCT",
    },
    Feature {
        code: "GQ05",
        description: "Composite query: EXCEPT ALL",
    },
    Feature {
        code: "GQ06",
        description: "Composite query: INTERSECT DISTINCT",
    },
    Feature {
        code: "GQ07",
        description: "Composite query: INTERSECT ALL",
    },
    Feature {
        code: "GQ08",
        description: "FILTER statement",
    },
    Feature {
        code: "GQ09",
        description: "LET statement",
    },
    Feature {
        code: "GQ10",
        description: "FOR statement: list value support",
    },
    Feature {
        code: "GQ11",
        description: "FOR statement: WITH ORDINALITY",
    },
    Feature {
        code: "GQ12",
        description: "ORDER BY and page statement: OFFSET clause",
    },
    Feature {
        code: "GQ13",
        description: "ORDER BY and page statement: LIMIT clause",
    },
    Feature {
        code: "GQ14",
        description: "Complex expressions in sort keys",
    },
    Feature {
        code: "GQ15",
        description: "GROUP BY clause",
    },
    Feature {
        code: "GQ16",
        description: "Pre-projection aliases in sort keys",
    },
    Feature {
        code: "GQ17",
        description: "Element-wise group variable operations",
    },
    Feature {
        code: "GQ18",
        description: "Scalar subqueries",
    },
    Feature {
        code: "GQ19",
        description: "Graph pattern YIELD clause",
    },
    Feature {
        code: "GQ20",
        description: "Advanced linear composition with NEXT",
    },
    Feature {
        code: "GQ21",
        description: "OPTIONAL: Multiple MATCH statements",
    },
    Feature {
        code: "GQ22",
        description: "EXISTS predicate: multiple MATCH statements",
    },
    Feature {
        code: "GQ23",
        description: "FOR statement: binding table support",
    },
    Feature {
        code: "GQ24",
        description: "FOR statement: WITH OFFSET",
    },
    Feature {
        code: "GS01",
        description: "SESSION SET command: session-local graph parameters",
    },
    Feature {
        code: "GS02",
        description: "SESSION SET command: session-local binding table parameters",
    },
    Feature {
        code: "GS03",
        description: "SESSION SET command: session-local value parameters",
    },
    Feature {
        code: "GS04",
        description: "SESSION RESET command: reset all characteristics",
    },
    Feature {
        code: "GS05",
        description: "SESSION RESET command: reset session schema",
    },
    Feature {
        code: "GS06",
        description: "SESSION RESET command: reset session graph",
    },
    Feature {
        code: "GS07",
        description: "SESSION RESET command: reset time zone displacement",
    },
    Feature {
        code: "GS08",
        description: "SESSION RESET command: reset all session parameters",
    },
    Feature {
        code: "GS10",
        description: "SESSION SET command: session-local binding table parameters based on subqueries",
    },
    Feature {
        code: "GS11",
        description: "SESSION SET command: session-local value parameters based on subqueries",
    },
    Feature {
        code: "GS12",
        description: "SESSION SET command: session-local graph parameters based on simple expressions or references",
    },
    Feature {
        code: "GS13",
        description: "SESSION SET command: session-local binding table parameters based on simple expressions or references",
    },
    Feature {
        code: "GS14",
        description: "SESSION SET command: session-local value parameters based on simple expressions",
    },
    Feature {
        code: "GS15",
        description: "SESSION SET command: set time zone displacement",
    },
    Feature {
        code: "GS16",
        description: "SESSION RESET command: reset individual session parameters",
    },
    Feature {
        code: "GT01",
        description: "Explicit transaction commands",
    },
    Feature {
        code: "GT02",
        description: "Specified transaction characteristics",
    },
    Feature {
        code: "GT03",
        description: "Use of multiple graphs in a transaction",
    },
    Feature {
        code: "GV01",
        description: "8 bit unsigned integer numbers",
    },
    Feature {
        code: "GV02",
        description: "8 bit signed integer numbers",
    },
    Feature {
        code: "GV03",
        description: "16 bit unsigned integer numbers",
    },
    Feature {
        code: "GV04",
        description: "16 bit signed integer numbers",
    },
    Feature {
        code: "GV05",
        description: "Small unsigned integer numbers",
    },
    Feature {
        code: "GV06",
        description: "32 bit unsigned integer numbers",
    },
    Feature {
        code: "GV07",
        description: "32 bit signed integer numbers",
    },
    Feature {
        code: "GV08",
        description: "Regular unsigned integer numbers",
    },
    Feature {
        code: "GV09",
        description: "Specified integer number precision",
    },
    Feature {
        code: "GV10",
        description: "Big unsigned integer numbers",
    },
    Feature {
        code: "GV11",
        description: "64 bit unsigned integer numbers",
    },
    Feature {
        code: "GV12",
        description: "64 bit signed integer numbers",
    },
    Feature {
        code: "GV13",
        description: "128 bit unsigned integer numbers",
    },
    Feature {
        code: "GV14",
        description: "128 bit signed integer numbers",
    },
    Feature {
        code: "GV15",
        description: "256 bit unsigned integer numbers",
    },
    Feature {
        code: "GV16",
        description: "256 bit signed integer numbers",
    },
    Feature {
        code: "GV17",
        description: "Decimal numbers",
    },
    Feature {
        code: "GV18",
        description: "Small signed integer numbers",
    },
    Feature {
        code: "GV19",
        description: "Big signed integer numbers",
    },
    Feature {
        code: "GV20",
        description: "16 bit floating point numbers",
    },
    Feature {
        code: "GV21",
        description: "32 bit floating point numbers",
    },
    Feature {
        code: "GV22",
        description: "Specified floating point number precision",
    },
    Feature {
        code: "GV23",
        description: "Floating point type name synonyms",
    },
    Feature {
        code: "GV24",
        description: "64 bit floating point numbers",
    },
    Feature {
        code: "GV25",
        description: "128 bit floating point numbers",
    },
    Feature {
        code: "GV26",
        description: "256 bit floating point numbers",
    },
    Feature {
        code: "GV30",
        description: "Specified character string minimum length",
    },
    Feature {
        code: "GV31",
        description: "Specified character string maximum length",
    },
    Feature {
        code: "GV32",
        description: "Specified character string fixed length",
    },
    Feature {
        code: "GV35",
        description: "Byte string types",
    },
    Feature {
        code: "GV36",
        description: "Specified byte string minimum length",
    },
    Feature {
        code: "GV37",
        description: "Specified byte string maximum length",
    },
    Feature {
        code: "GV38",
        description: "Specified byte string fixed length",
    },
    Feature {
        code: "GV39",
        description: "Temporal types: date, local datetime and local time support",
    },
    Feature {
        code: "GV40",
        description: "Temporal types: zoned datetime and zoned time support",
    },
    Feature {
        code: "GV41",
        description: "Temporal types: duration support",
    },
    Feature {
        code: "GV45",
        description: "Record types",
    },
    Feature {
        code: "GV46",
        description: "Closed record types",
    },
    Feature {
        code: "GV47",
        description: "Open record types",
    },
    Feature {
        code: "GV48",
        description: "Nested record types",
    },
    Feature {
        code: "GV50",
        description: "List value types",
    },
    Feature {
        code: "GV55",
        description: "Path value types",
    },
    Feature {
        code: "GV60",
        description: "Graph reference value types",
    },
    Feature {
        code: "GV61",
        description: "Binding table reference value types",
    },
    Feature {
        code: "GV65",
        description: "Dynamic union types",
    },
    Feature {
        code: "GV66",
        description: "Open dynamic union types",
    },
    Feature {
        code: "GV67",
        description: "Closed dynamic union types",
    },
    Feature {
        code: "GV68",
        description: "Dynamic property value types",
    },
    Feature {
        code: "GV70",
        description: "Immaterial value types",
    },
    Feature {
        code: "GV71",
        description: "Immaterial value types: null type support",
    },
    Feature {
        code: "GV72",
        description: "Immaterial value types: empty type support",
    },
    Feature {
        code: "GV90",
        description: "Explicit value type nullability",
    },
];
