//! Generated from `crates/zu-common/artifacts/gql-implementation-defined.xml`
//! and `gql-implementation-dependent.xml`, the two ISO/IEC
//! 39075:2024 artifacts that list what the standard leaves to the
//! implementation. Do not edit by hand: run
//! `ZU_UPDATE_IMPDEF=1 cargo test -p zu-cli --test impdef_table`.
//!
//! Codes and descriptions are the standard's, verbatim, with runs
//! of whitespace folded to one space so a description is one line.

use super::{Item, Kind};

/// Every item the standard leaves open, in code order, the
/// implementation-defined ones first.
pub(super) static ITEMS: &[Item] = &[
    Item {
        code: "IA001",
        kind: Kind::Defined,
        description: "Whether the declared type of a regular result of a successful outcome of a GQL-request is exposed to the GQL-client.",
    },
    Item {
        code: "IA002",
        kind: Kind::Defined,
        description: "The extent to which further GQL-status objects are chained.",
    },
    Item {
        code: "IA003",
        kind: Kind::Defined,
        description: "The result of any operation other than a normalize function or a normalized predicate on an unnormalized character string.",
    },
    Item {
        code: "IA004",
        kind: Kind::Defined,
        description: "The rules for determining the actual value of an approximate numeric type from its apparent value.",
    },
    Item {
        code: "IA005",
        kind: Kind::Defined,
        description: "Whether rounding or truncating occurs when least significant digits are lost on assignment.",
    },
    Item {
        code: "IA006",
        kind: Kind::Defined,
        description: "The choice of value selected when there is more than one approximation for a numeric type that conforms to the criteria for each supported type of numeric value.",
    },
    Item {
        code: "IA007",
        kind: Kind::Defined,
        description: "Which supported numeric values other than exact numeric types also have approximations.",
    },
    Item {
        code: "IA010",
        kind: Kind::Defined,
        description: "The boundaries within which the normal rules of arithmetic apply.",
    },
    Item {
        code: "IA011",
        kind: Kind::Defined,
        description: "Whether rounding or truncating is used on division with an approximate mathematical result.",
    },
    Item {
        code: "IA012",
        kind: Kind::Defined,
        description: "Whether a GQL Flagger flags implementation-defined features.",
    },
    Item {
        code: "IA013",
        kind: Kind::Defined,
        description: "Whether the General Rules of Evaluation of a selective path pattern are terminated if an exception condition is raised.",
    },
    Item {
        code: "IA014",
        kind: Kind::Defined,
        description: "Whether an exception condition is raised when the declared type of NULL cannot be determined contextually.",
    },
    Item {
        code: "IA015",
        kind: Kind::Defined,
        description: "Whether to pad character strings for comparison, or not.",
    },
    Item {
        code: "IA016",
        kind: Kind::Defined,
        description: "Whether to treat byte string differing only in right-most X'00' bytes as equal, or not.",
    },
    Item {
        code: "IA017",
        kind: Kind::Defined,
        description: "Whether or not an exception condition is raised or an arbitrary value is chosen when multiple assignments to a graph element property are specified.",
    },
    Item {
        code: "IA019",
        kind: Kind::Defined,
        description: "Whether ‹bidirectional control character›s are permitted in string literals.",
    },
    Item {
        code: "IA020",
        kind: Kind::Defined,
        description: "Whether characters of the Unicode General Category class “Co” are permitted to be contained in the representative form of an identifier.",
    },
    Item {
        code: "IA021",
        kind: Kind::Defined,
        description: "Whether an exception condition is raised, or truncation or rounding occurs, when an assignment of some number would result in a loss of its least significant digits.",
    },
    Item {
        code: "IA023",
        kind: Kind::Defined,
        description: "The character (code) interpreted as newline.",
    },
    Item {
        code: "IA025",
        kind: Kind::Defined,
        description: "The effect that additional values resulting from the support of Feature GA01, “IEEE 754 floating point operations” have on the processing of a GQL-request.",
    },
    Item {
        code: "IA026",
        kind: Kind::Defined,
        description: "Whether a GQL-implementation supports leap seconds or discontinuities in calendars, and the consequences of such support for temporal arithmetic.",
    },
    Item {
        code: "ID001",
        kind: Kind::Defined,
        description: "The object (principal) that represents a user within a GQL-implementation.",
    },
    Item {
        code: "ID002",
        kind: Kind::Defined,
        description: "The association between a principal and its home schema and home graph.",
    },
    Item {
        code: "ID003",
        kind: Kind::Defined,
        description: "The set of privileges identified by an authorization identifier.",
    },
    Item {
        code: "ID004",
        kind: Kind::Defined,
        description: "The value types of inner elements of constructed values when no concrete value is specified.",
    },
    Item {
        code: "ID005",
        kind: Kind::Defined,
        description: "The declared type of an ‹elements function›.",
    },
    Item {
        code: "ID006",
        kind: Kind::Defined,
        description: "The default transaction characteristics.",
    },
    Item {
        code: "ID016",
        kind: Kind::Defined,
        description: "The translations of condition texts.",
    },
    Item {
        code: "ID017",
        kind: Kind::Defined,
        description: "The map of diagnostic information, if provided.",
    },
    Item {
        code: "ID022",
        kind: Kind::Defined,
        description: "The default collation.",
    },
    Item {
        code: "ID023",
        kind: Kind::Defined,
        description: "The preferred name of a string type, for each supported kind of string type.",
    },
    Item {
        code: "ID028",
        kind: Kind::Defined,
        description: "The effective binary precision of each supported integer type.",
    },
    Item {
        code: "ID034",
        kind: Kind::Defined,
        description: "The effective decimal precision of each decimal type.",
    },
    Item {
        code: "ID037",
        kind: Kind::Defined,
        description: "The effective binary precision and scale of each supported approximate numeric type.",
    },
    Item {
        code: "ID048",
        kind: Kind::Defined,
        description: "The default time zone displacement.",
    },
    Item {
        code: "ID049",
        kind: Kind::Defined,
        description: "The default session parameters.",
    },
    Item {
        code: "ID057",
        kind: Kind::Defined,
        description: "The exact numeric type with scale 0 (zero) of list element ordinal positions.",
    },
    Item {
        code: "ID058",
        kind: Kind::Defined,
        description: "The exact numeric type with scale 0 (zero) of list element position offsets.",
    },
    Item {
        code: "ID059",
        kind: Kind::Defined,
        description: "The exact numeric declared type of results of the COUNT function.",
    },
    Item {
        code: "ID061",
        kind: Kind::Defined,
        description: "The declared type of SESSION_USER.",
    },
    Item {
        code: "ID062",
        kind: Kind::Defined,
        description: "The exact numeric declared type of a non-negative integer specification.",
    },
    Item {
        code: "ID063",
        kind: Kind::Defined,
        description: "The numeric declared type of the result of a dyadic arithmetic operator when either operand is approximate numeric.",
    },
    Item {
        code: "ID064",
        kind: Kind::Defined,
        description: "The numeric declared type of the result of a dyadic arithmetic operator when both operands are exact numeric.",
    },
    Item {
        code: "ID065",
        kind: Kind::Defined,
        description: "The precision of the result of addition and subtraction of exact numeric types.",
    },
    Item {
        code: "ID066",
        kind: Kind::Defined,
        description: "The precision of the result of multiplication of exact numeric types.",
    },
    Item {
        code: "ID067",
        kind: Kind::Defined,
        description: "The precision and scale of the result of division of exact numeric types.",
    },
    Item {
        code: "ID068",
        kind: Kind::Defined,
        description: "The exact numeric declared type of results length expressions.",
    },
    Item {
        code: "ID069",
        kind: Kind::Defined,
        description: "The numeric declared types of results of trigonometric functions, general logarithm functions, natural logarithms, exponential functions, and power functions.",
    },
    Item {
        code: "ID070",
        kind: Kind::Defined,
        description: "The declared type of the result of a cardinality expression.",
    },
    Item {
        code: "ID074",
        kind: Kind::Defined,
        description: "The precision of an exact numeric result of a numeric value expression.",
    },
    Item {
        code: "ID075",
        kind: Kind::Defined,
        description: "The precision of an approximate numeric result of a numeric value expression.",
    },
    Item {
        code: "ID076",
        kind: Kind::Defined,
        description: "The declared type of results of the ELEMENT_ID function.",
    },
    Item {
        code: "ID079",
        kind: Kind::Defined,
        description: "The declared type of an approximate numeric literal.",
    },
    Item {
        code: "ID085",
        kind: Kind::Defined,
        description: "The nullable declared type of NULL if its declared type cannot be determined contextually.",
    },
    Item {
        code: "ID086",
        kind: Kind::Defined,
        description: "The default graph pattern match mode.",
    },
    Item {
        code: "ID089",
        kind: Kind::Defined,
        description: "The use of GRAPH or PROPERTY GRAPH in the preferred name of graph types and graph reference value types.",
    },
    Item {
        code: "ID090",
        kind: Kind::Defined,
        description: "The use of NODE or VERTEX in the preferred name of node types, node reference value types, and their base types.",
    },
    Item {
        code: "ID091",
        kind: Kind::Defined,
        description: "The use of EDGE or RELATIONSHIP in the preferred name of edge types, edge reference value types, and their base types.",
    },
    Item {
        code: "ID095",
        kind: Kind::Defined,
        description: "The exact numeric declared types of the results of the SUM function.",
    },
    Item {
        code: "ID096",
        kind: Kind::Defined,
        description: "The exact numeric declared types of the results of the AVG function.",
    },
    Item {
        code: "ID097",
        kind: Kind::Defined,
        description: "The approximate numeric declared types of the results of the SUM and AVG functions.",
    },
    Item {
        code: "ID098",
        kind: Kind::Defined,
        description: "The approximate numeric declared types of the results of the STDDEV_POP and STDDEV_SAMP functions.",
    },
    Item {
        code: "ID099",
        kind: Kind::Defined,
        description: "The approximate numeric declared types of the results of binary set functions.",
    },
    Item {
        code: "IE001",
        kind: Kind::Defined,
        description: "The object, resource, or value identified by a URI or a URL.",
    },
    Item {
        code: "IE002",
        kind: Kind::Defined,
        description: "The levels of transaction isolation, their interactions, their granularity of application and the Format and Syntax Rules for ‹implementation-defined access mode› used to select them.",
    },
    Item {
        code: "IE003",
        kind: Kind::Defined,
        description: "The UAX31-R1-1 profile if used.",
    },
    Item {
        code: "IE004",
        kind: Kind::Defined,
        description: "Relaxations of the assumption of serializable transactional behavior, if any.",
    },
    Item {
        code: "IE005",
        kind: Kind::Defined,
        description: "The treatment of language that does not conform to the Formats and Syntax Rules.",
    },
    Item {
        code: "IE006",
        kind: Kind::Defined,
        description: "Additional restrictions, requirements, and conditions imposed on mixed-mode transactions.",
    },
    Item {
        code: "IE007",
        kind: Kind::Defined,
        description: "The conditions raised when the requirements on mixed-mode transactions are violated.",
    },
    Item {
        code: "IE008",
        kind: Kind::Defined,
        description: "Additional conditions for which a completion condition warning (01000) is raised.",
    },
    Item {
        code: "IE009",
        kind: Kind::Defined,
        description: "Additional informational conditions raised.",
    },
    Item {
        code: "IE010",
        kind: Kind::Defined,
        description: "The subclasses providing information of a non-cautionary nature when the completion condition is successful completion.",
    },
    Item {
        code: "IL001",
        kind: Kind::Defined,
        description: "The minimum and maximum cardinalities of label sets, for each kind of graph element.",
    },
    Item {
        code: "IL002",
        kind: Kind::Defined,
        description: "The maximum cardinalities of property sets, for each kind of graph element.",
    },
    Item {
        code: "IL003",
        kind: Kind::Defined,
        description: "The minimum and maximum cardinalities of key label sets, for each kind of graph element.",
    },
    Item {
        code: "IL009",
        kind: Kind::Defined,
        description: "The minimum length of a string resulting from string concatenation of strings of variable-length string types, for each supported string type.",
    },
    Item {
        code: "IL010",
        kind: Kind::Defined,
        description: "The maximum number of digits permitted in an unsigned integer literal.",
    },
    Item {
        code: "IL011",
        kind: Kind::Defined,
        description: "The maximum precision and scale of numbers of numeric types, for each supported kind of number.",
    },
    Item {
        code: "IL013",
        kind: Kind::Defined,
        description: "The maximum lengths of string values of string types, for each supported string type.",
    },
    Item {
        code: "IL015",
        kind: Kind::Defined,
        description: "The maximum cardinality of constructed values, for each supported constructed value type.",
    },
    Item {
        code: "IL018",
        kind: Kind::Defined,
        description: "The maximum value of the upper bound of a general qualifier.",
    },
    Item {
        code: "IL020",
        kind: Kind::Defined,
        description: "The maximum depth of nesting of GQL-directories.",
    },
    Item {
        code: "IL023",
        kind: Kind::Defined,
        description: "The minimum and maximum values of the exponent for an approximate numeric type.",
    },
    Item {
        code: "IL024",
        kind: Kind::Defined,
        description: "The maximum value of fractional seconds precision for a temporal instant or a temporal duration.",
    },
    Item {
        code: "IS001",
        kind: Kind::Defined,
        description: "The implicit ordering of NULLs.",
    },
    Item {
        code: "IV001",
        kind: Kind::Defined,
        description: "The character repertoire of GQL source text.",
    },
    Item {
        code: "IV002",
        kind: Kind::Defined,
        description: "The result of an inequality comparison between operands that are essentially comparable values when not otherwise specified.",
    },
    Item {
        code: "IV003",
        kind: Kind::Defined,
        description: "The choice of the normal form of each supported kind of GQL-object type with a defined normal form.",
    },
    Item {
        code: "IV008",
        kind: Kind::Defined,
        description: "The choice of the normal form of each supported kind of value type with a defined normal form.",
    },
    Item {
        code: "IV010",
        kind: Kind::Defined,
        description: "The result of a comparison between two operands that are universally comparable values.",
    },
    Item {
        code: "IV011",
        kind: Kind::Defined,
        description: "The dynamic union type chosen as the dynamic property value type.",
    },
    Item {
        code: "IV012",
        kind: Kind::Defined,
        description: "The set of component types of the open dynamic union type.",
    },
    Item {
        code: "IV014",
        kind: Kind::Defined,
        description: "The set of value types that includes at least one supertype of every static value type supported by the GQL-implementation.",
    },
    Item {
        code: "IV015",
        kind: Kind::Defined,
        description: "The valid syntactic representation of an authorization identifier.",
    },
    Item {
        code: "IV016",
        kind: Kind::Defined,
        description: "The description of any additional text provided about conditions.",
    },
    Item {
        code: "IV023",
        kind: Kind::Defined,
        description: "The set of characters included in truncating whitespace.",
    },
    Item {
        code: "IW001",
        kind: Kind::Defined,
        description: "The mechanism for instructing a GQL-client to create and destroy GQL-sessions to GQL-servers, and to submit GQL-requests to them.",
    },
    Item {
        code: "IW002",
        kind: Kind::Defined,
        description: "The mechanism for creating and destroying authorization identifiers, and their mapping to principals.",
    },
    Item {
        code: "IW003",
        kind: Kind::Defined,
        description: "The mechanism for determining when the last request has been received.",
    },
    Item {
        code: "IW004",
        kind: Kind::Defined,
        description: "The alternative mechanism for starting and terminating transactions.",
    },
    Item {
        code: "IW005",
        kind: Kind::Defined,
        description: "The mechanism by which termination success or failure statuses are made available to the GQL-agent or administrator.",
    },
    Item {
        code: "IW006",
        kind: Kind::Defined,
        description: "The mechanism for determining the dictionary of GQL-request parameters.",
    },
    Item {
        code: "IW007",
        kind: Kind::Defined,
        description: "The manner in which GQL-status objects are presented to a GQL-client.",
    },
    Item {
        code: "IW010",
        kind: Kind::Defined,
        description: "The manner in which external procedures are provided.",
    },
    Item {
        code: "IW011",
        kind: Kind::Defined,
        description: "The mechanism for determining the reference value type of an element variable declared by a graph pattern.",
    },
    Item {
        code: "IW012",
        kind: Kind::Defined,
        description: "The mechanism for determining the reference value type of an element variable declared by insert node pattern.",
    },
    Item {
        code: "IW014",
        kind: Kind::Defined,
        description: "The mechanism used to determine if two character strings are visually confusable with each other.",
    },
    Item {
        code: "IW015",
        kind: Kind::Defined,
        description: "The manner, if it so chooses, in which a GQL-implementation automatically creates and populates a GQL-directory.",
    },
    Item {
        code: "IW016",
        kind: Kind::Defined,
        description: "The manner, if it so chooses, in which a GQL-implementation automatically populates a GQL-schema upon its creation.",
    },
    Item {
        code: "IW017",
        kind: Kind::Defined,
        description: "The manner in which the result of the concatenation of non-normalized character strings is determined.",
    },
    Item {
        code: "IW018",
        kind: Kind::Defined,
        description: "The manner in which lax casts (and supporting type tests) are generated and included in the syntax transforms for the dynamic generation of strict casts.",
    },
    Item {
        code: "IW019",
        kind: Kind::Defined,
        description: "The mechanism for determining a common supertype of a set of value types of the same primary static base type.",
    },
    Item {
        code: "IW021",
        kind: Kind::Defined,
        description: "The mechanism for determining a permutation of all value types of a set of value types that adheres to type precedence rules.",
    },
    Item {
        code: "IW022",
        kind: Kind::Defined,
        description: "The mechanism for determining if the null value is not actually going to be assigned to a site.",
    },
    Item {
        code: "IW023",
        kind: Kind::Defined,
        description: "The mechanism for determining the canonical name form of a ‹delimited identifier› or ‹non-delimited identifier›.",
    },
    Item {
        code: "IW025",
        kind: Kind::Defined,
        description: "The mechanism for determining which and how many catalog-modifying procedures are under transaction control, and which catalog-modifying procedures can be contained in a single transaction.",
    },
    Item {
        code: "UA001",
        kind: Kind::Dependent,
        description: "The interaction between multiple GQL-environments within the constraints of GQL-transaction semantics.",
    },
    Item {
        code: "UA002",
        kind: Kind::Dependent,
        description: "Whether or not diagnostic information pertaining to more than one condition is made available.",
    },
    Item {
        code: "UA004",
        kind: Kind::Dependent,
        description: "Whether or not that exception condition is actually raised when the evaluation of an inessential part of an expression or search condition would cause an exception to be raised.",
    },
    Item {
        code: "UA005",
        kind: Kind::Dependent,
        description: "Which path bindings are retained in an any paths search if the number of candidates exceeds the required number.",
    },
    Item {
        code: "UA006",
        kind: Kind::Dependent,
        description: "Which additional path bindings are actually probed to establish whether they might also raise an exception when the GQL-implementation has terminated the evaluation of a selective path pattern.",
    },
    Item {
        code: "UA007",
        kind: Kind::Dependent,
        description: "Whether or not a rollback is forced when a GQL-transaction becomes blocked, cannot complete without causing semantic inconsistency, or the resources required to continue its execution become unavailable.",
    },
    Item {
        code: "US001",
        kind: Kind::Dependent,
        description: "The sequence of records in an unordered binding table.",
    },
    Item {
        code: "US005",
        kind: Kind::Dependent,
        description: "The order of path bindings that have the same number of edges.",
    },
    Item {
        code: "US006",
        kind: Kind::Dependent,
        description: "The relative ordering of peers in a sort.",
    },
    Item {
        code: "US007",
        kind: Kind::Dependent,
        description: "The relative ordering of items in a sort whose comparison is Unknown.",
    },
    Item {
        code: "US008",
        kind: Kind::Dependent,
        description: "The actual order of expression evaluation.",
    },
    Item {
        code: "US009",
        kind: Kind::Dependent,
        description: "The point in time at which the request timestamp is set.",
    },
    Item {
        code: "UV001",
        kind: Kind::Dependent,
        description: "The value of an object identifier.",
    },
    Item {
        code: "UV003",
        kind: Kind::Dependent,
        description: "The ‹value expression› whose evaluation raises the exception condition: “data exception — invalid value type (22G12)”.",
    },
    Item {
        code: "UV004",
        kind: Kind::Dependent,
        description: "The value value returned by an evaluation of the ELEMENT_ID function.",
    },
    Item {
        code: "UV005",
        kind: Kind::Dependent,
        description: "The physical representation of an instance of a data type.",
    },
    Item {
        code: "UV007",
        kind: Kind::Dependent,
        description: "The declared type of a site that contains an intermediate result.",
    },
    Item {
        code: "UV009",
        kind: Kind::Dependent,
        description: "Which arbitrary value is chosen when multiple assignments to a graph element property are specified.",
    },
    Item {
        code: "UV014",
        kind: Kind::Dependent,
        description: "The start datetime used for converting intervals to scalars for subtraction purposes.",
    },
    Item {
        code: "UW001",
        kind: Kind::Dependent,
        description: "The mechanism for determining which exception condition is to be returned as the primary GQL-status object of an execution outcome from a set of raised exception conditions.",
    },
];
