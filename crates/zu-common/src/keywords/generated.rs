//! Generated from `artifacts/gql-bnf.xml`, the ISO/IEC
//! 39075:2024 grammar artifact. Do not edit by hand: run
//! `ZU_UPDATE_KEYWORDS=1 cargo test -p zu-common --test keyword_table`.
//!
//! Every list is sorted, so the lookups are a binary search.

/// The words `<reserved word>` names directly, which a regular
/// identifier may not be spelled as (ISO 21.3).
#[rustfmt::skip]
pub(super) static RESERVED: &[&str] = &[
    "ABS", "ACOS", "ALL", "ALL_DIFFERENT", "AND", "ANY",
    "ARRAY", "AS", "ASC", "ASCENDING", "ASIN", "AT",
    "ATAN", "AVG", "BIG", "BIGINT", "BINARY", "BOOL",
    "BOOLEAN", "BOTH", "BTRIM", "BY", "BYTES", "BYTE_LENGTH",
    "CALL", "CARDINALITY", "CASE", "CAST", "CEIL", "CEILING",
    "CHAR", "CHARACTERISTICS", "CHARACTER_LENGTH", "CHAR_LENGTH", "CLOSE", "COALESCE",
    "COLLECT_LIST", "COMMIT", "COPY", "COS", "COSH", "COT",
    "COUNT", "CREATE", "CURRENT_DATE", "CURRENT_GRAPH", "CURRENT_PROPERTY_GRAPH", "CURRENT_SCHEMA",
    "CURRENT_TIME", "CURRENT_TIMESTAMP", "DATE", "DATETIME", "DAY", "DEC",
    "DECIMAL", "DEGREES", "DELETE", "DESC", "DESCENDING", "DETACH",
    "DISTINCT", "DOUBLE", "DROP", "DURATION", "DURATION_BETWEEN", "ELEMENT_ID",
    "ELSE", "END", "EXCEPT", "EXISTS", "EXP", "FALSE",
    "FILTER", "FINISH", "FLOAT", "FLOAT128", "FLOAT16", "FLOAT256",
    "FLOAT32", "FLOAT64", "FLOOR", "FOR", "FROM", "GROUP",
    "HAVING", "HOME_GRAPH", "HOME_PROPERTY_GRAPH", "HOME_SCHEMA", "HOUR", "IF",
    "IMPLIES", "IN", "INSERT", "INT", "INT128", "INT16",
    "INT256", "INT32", "INT64", "INT8", "INTEGER", "INTEGER128",
    "INTEGER16", "INTEGER256", "INTEGER32", "INTEGER64", "INTEGER8", "INTERSECT",
    "INTERVAL", "IS", "LEADING", "LEFT", "LET", "LIKE",
    "LIMIT", "LIST", "LN", "LOCAL", "LOCAL_DATETIME", "LOCAL_TIME",
    "LOCAL_TIMESTAMP", "LOG", "LOG10", "LOWER", "LTRIM", "MATCH",
    "MAX", "MIN", "MINUTE", "MOD", "MONTH", "NEXT",
    "NODETACH", "NORMALIZE", "NOT", "NOTHING", "NULL", "NULLIF",
    "NULLS", "OCTET_LENGTH", "OF", "OFFSET", "OPTIONAL", "OR",
    "ORDER", "OTHERWISE", "PARAMETER", "PARAMETERS", "PATH", "PATHS",
    "PATH_LENGTH", "PERCENTILE_CONT", "PERCENTILE_DISC", "POWER", "PRECISION", "PROPERTY_EXISTS",
    "RADIANS", "REAL", "RECORD", "REMOVE", "REPLACE", "RESET",
    "RETURN", "RIGHT", "ROLLBACK", "RTRIM", "SAME", "SCHEMA",
    "SECOND", "SELECT", "SESSION", "SESSION_USER", "SET", "SIGNED",
    "SIN", "SINH", "SIZE", "SKIP", "SMALL", "SMALLINT",
    "SQRT", "START", "STDDEV_POP", "STDDEV_SAMP", "STRING", "SUM",
    "TAN", "TANH", "THEN", "TIME", "TIMESTAMP", "TRAILING",
    "TRIM", "TRUE", "TYPED", "UBIGINT", "UINT", "UINT128",
    "UINT16", "UINT256", "UINT32", "UINT64", "UINT8", "UNION",
    "UNKNOWN", "UNSIGNED", "UPPER", "USE", "USMALLINT", "VALUE",
    "VARBINARY", "VARCHAR", "VARIABLE", "WHEN", "WHERE", "WITH",
    "XOR", "YEAR", "YIELD", "ZONED", "ZONED_DATETIME", "ZONED_TIME",
];

/// The words `<pre-reserved word>` names, an alternative inside
/// `<reserved word>` holding what ISO has taken for a later
/// edition and given no meaning to yet.
#[rustfmt::skip]
pub(super) static PRE_RESERVED: &[&str] = &[
    "ABSTRACT", "AGGREGATE", "AGGREGATES", "ALTER", "CATALOG", "CLEAR",
    "CLONE", "CONSTRAINT", "CURRENT_ROLE", "CURRENT_USER", "DATA", "DIRECTORY",
    "DRYRUN", "EXACT", "EXISTING", "FUNCTION", "GQLSTATUS", "GRANT",
    "INFINITY", "INSTANT", "NUMBER", "NUMERIC", "ON", "OPEN",
    "PARTITION", "PROCEDURE", "PRODUCT", "PROJECT", "QUERY", "RECORDS",
    "REFERENCE", "RENAME", "REVOKE", "SUBSTRING", "SYSTEM_USER", "TEMPORAL",
    "UNIQUE", "UNIT", "VALUES", "WHITESPACE",
];

/// The words `<non-reserved word>` names: spelled like keywords,
/// admitted as identifiers.
#[rustfmt::skip]
pub(super) static NON_RESERVED: &[&str] = &[
    "ACYCLIC", "BINDING", "BINDINGS", "CONNECTING", "DESTINATION", "DIFFERENT",
    "DIRECTED", "EDGE", "EDGES", "ELEMENT", "ELEMENTS", "FIRST",
    "GRAPH", "GROUPS", "KEEP", "LABEL", "LABELED", "LABELS",
    "LAST", "NFC", "NFD", "NFKC", "NFKD", "NO",
    "NODE", "NORMALIZED", "ONLY", "ORDINALITY", "PROPERTY", "READ",
    "RELATIONSHIP", "RELATIONSHIPS", "REPEATABLE", "SHORTEST", "SIMPLE", "SOURCE",
    "TABLE", "TEMP", "TO", "TRAIL", "TRANSACTION", "TYPE",
    "UNDIRECTED", "VERTEX", "WALK", "WITHOUT", "WRITE", "ZONE",
];
