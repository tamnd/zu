//! The register of what ISO/IEC 39075:2024 leaves to the implementation,
//! with zu's answer to every item (Spec/2064g/gql/plan/08).
//!
//! Clause 24.5.2 asks a conforming implementation to state its answer to
//! two lists. The first is 117 implementation-defined items, which are
//! the questions an implementation must answer the same way every time
//! and must publish. The second is 20 implementation-dependent items,
//! which are the ones it need not publish and need not keep stable, and
//! zu publishes those too, as observed behaviour rather than as a
//! promise, because a reader who knows what an engine does today can
//! write a query that does not depend on it.
//!
//! The codes and the wording of the items are the standard's and come
//! out of the two published artifacts mechanically, which is what
//! `generated.rs` holds and what `tests/impdef_table.rs` checks. What is
//! written by hand here is the answer column and only the answer column,
//! so an item cannot be quietly reworded to suit an answer and an item
//! the standard adds arrives with no answer and fails the test that says
//! every item has one.
//!
//! Two answers are absences and they are stated as absences rather than
//! left out: IL015 and IW014. An absence is a conformant answer, it is
//! the reason two GQLSTATUS conditions are unreachable here, and a
//! reader who found neither the item nor a note about it would have to
//! guess which of the two had happened.

mod generated;

use generated::ITEMS;

/// Which of the standard's two lists an item is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Clause 24.5.2's implementation-defined list: an answer that must
    /// be the same every time and must be published.
    Defined,
    /// Clause 24.5.2's implementation-dependent list: an answer that
    /// need be neither.
    Dependent,
}

impl Kind {
    fn heading(self) -> &'static str {
        match self {
            Kind::Defined => "Implementation-defined",
            Kind::Dependent => "Implementation-dependent",
        }
    }
}

/// One item of the standard's two lists, verbatim.
pub struct Item {
    /// The item's code, two letters and three digits.
    pub code: &'static str,
    /// Which list it is on.
    pub kind: Kind,
    /// What the standard says is left open, in the standard's words.
    pub description: &'static str,
}

/// zu's answer to one item.
///
/// Every item has one. An item zu sets no value for is answered by
/// saying so, since a missing row and a declared absence read the same
/// on the page and mean different things.
struct Answer {
    /// The item's code, which has to be one the artifacts define.
    item: &'static str,
    /// What zu does, and where it is a choice, why.
    answer: &'static str,
}

/// The 117 implementation-defined answers, in code order.
const DEFINED: &[Answer] = &[
    Answer {
        item: "IA001",
        answer: "No. A reply carries the column names and the rows, and every value on the wire carries its own kind, so a client reading one knows what it is holding without being told what the column was declared to hold. The shell's `prepare` op reports the parameter names a statement will read, which is the one place a declared type would be useful and the one place it is not yet published.",
    },
    Answer {
        item: "IA002",
        answer: "Nothing is chained. A reply carries one GQL-status object, and a statement that could have raised two reports the first one that stopped it.",
    },
    Answer {
        item: "IA003",
        answer: "The operation reads the code points that were written. zu normalizes nothing on its own, so a string that arrived unnormalized compares, joins, folds and is measured as the sequence it is, and NORMALIZE is the only thing in the language that changes it.",
    },
    Answer {
        item: "IA004",
        answer: "IEEE 754 binary64, round to nearest with ties to even, which is what the hardware underneath does.",
    },
    Answer {
        item: "IA005",
        answer: "Truncation toward zero, and no condition is raised for it. `CAST(1.9 AS INTEGER)` is 1 and `CAST(-1.9 AS INTEGER)` is -1.",
    },
    Answer {
        item: "IA006",
        answer: "There is never a choice to make. zu has one approximate type, binary64, so a value has one approximation and not several.",
    },
    Answer {
        item: "IA007",
        answer: "None. The numeric types are a 64 bit signed integer and a binary64 float, and the float is the approximation rather than a type that has one.",
    },
    Answer {
        item: "IA010",
        answer: "Integer arithmetic is exact from -9223372036854775808 to 9223372036854775807 and an answer outside that raises `22003` rather than wrapping. Float arithmetic is IEEE 754 binary64 throughout, and an answer that left the range a double holds raises `22003` as well rather than travelling as an infinity, since a value that travels is one a later comparison answers false about and a statement that got one would have been told nothing.",
    },
    Answer {
        item: "IA011",
        answer: "Rounding, to nearest with ties to even, a float division being the hardware's. An integer division truncates toward zero instead, `7/2` being 3, because two exact operands keep the answer exact.",
    },
    Answer {
        item: "IA012",
        answer: "zu has no GQL Flagger, so nothing is flagged.",
    },
    Answer {
        item: "IA013",
        answer: "Yes. The first condition raised ends the statement, and the paths the selector had not reached are not walked.",
    },
    Answer {
        item: "IA014",
        answer: "No. A null whose type nothing around it settles is a null of the open dynamic union type and travels as one.",
    },
    Answer {
        item: "IA015",
        answer: "No padding. `'ab '` and `'ab'` are two strings and compare unequal.",
    },
    Answer {
        item: "IA016",
        answer: "No. A byte string is compared by its bytes, so two that differ in a trailing X'00' differ.",
    },
    Answer {
        item: "IA017",
        answer: "An exception condition is raised. Assigning twice to the same property of one element in one SET is refused by name, because the element holds one value and the clause has not said which of the two it meant. Two SET clauses in a row are not that case and stay last wins.",
    },
    Answer {
        item: "IA019",
        answer: "Yes. A string literal holds whatever code points were written between its quotes, and zu reads none of them as controls.",
    },
    Answer {
        item: "IA020",
        answer: "No. An identifier is built from the Unicode general category classes ISO 21.3 names, and Co is not one of them, so a private use character neither starts nor continues a name.",
    },
    Answer {
        item: "IA021",
        answer: "Truncation, toward zero, with no condition raised. The one assignment that raises instead is a string too long for the type it is going into, which raises `22001` when what would be cut off is not spaces and reports `01004` when it is.",
    },
    Answer {
        item: "IA023",
        answer: "U+000A. A U+000D immediately before one is part of the same break rather than a break of its own, so source text written on either kind of machine reports the same line numbers.",
    },
    Answer {
        item: "IA025",
        answer: "An infinity or a NaN that arrives as data travels through arithmetic the way IEEE 754 says it should. What the numeric library will not do is answer one where the standard names a condition, so a logarithm of nought raises `2201E` rather than answering minus infinity, and an argument that was already infinite is left alone, the conditions being about a statement that asked for a number nobody has.",
    },
    Answer {
        item: "IA026",
        answer: "Neither is supported. A day is 86400 seconds and the calendar is proleptic Gregorian throughout, so temporal arithmetic is exact and has no discontinuity in it to reason about.",
    },
    Answer {
        item: "ID001",
        answer: "zu has no principal. A session is a process holding a file open, and who may open it is the file system's answer rather than the engine's.",
    },
    Answer {
        item: "ID002",
        answer: "There is no principal, so there is no association. A session's home schema is the root, `/`, and its home graph is the graph in the file it opened.",
    },
    Answer {
        item: "ID003",
        answer: "There is no authorization identifier and there are no privileges.",
    },
    Answer {
        item: "ID004",
        answer: "The open dynamic union type. An empty list is a list of it, which is what lets it be joined to a list of anything.",
    },
    Answer {
        item: "ID005",
        answer: "A list of the union of node and edge. ELEMENTS answers a path's nodes and edges in the order the walk took them, so both kinds are in the one list.",
    },
    Answer {
        item: "ID006",
        answer: "Read write. A statement outside a transaction runs in one of its own, which commits when it completes and rolls back when it raises.",
    },
    Answer {
        item: "ID016",
        answer: "There are none. A condition's text is the standard's English wording out of the conditions artifact, and the message beside it is zu's own English.",
    },
    Answer {
        item: "ID017",
        answer: "A failure carries `gqlstatus`, `condition`, `severity` and `message` always, the graph and schema it was raised in, the subject kind and subject where a name was at fault, and the line, column, byte offset and an excerpt of the source text where the position is known.",
    },
    Answer {
        item: "ID022",
        answer: "Code point order over the string's Unicode scalar values, with no locale in it, so `'B'` sorts before `'a'`. An engine whose order changed with the machine's locale would answer one ORDER BY two ways on two machines holding one file.",
    },
    Answer {
        item: "ID023",
        answer: "STRING for the character string type and BYTES for the byte string type. There is no fixed length or maximum length string type to name.",
    },
    Answer {
        item: "ID028",
        answer: "64 bits, signed, two's complement. It is the one integer type, and the narrower spellings the standard names are read as it.",
    },
    Answer {
        item: "ID034",
        answer: "zu declares none. DECIMAL is accepted as a type name and a value under it is held as binary64, so there is no decimal type with a precision of its own to publish.",
    },
    Answer {
        item: "ID037",
        answer: "53 bits of significand and an exponent from -1022 to 1023, which is IEEE 754 binary64 and is the one approximate type.",
    },
    Answer {
        item: "ID048",
        answer: "UTC, `+00:00`.",
    },
    Answer {
        item: "ID049",
        answer: "None. A session starts with no parameters and the ones a statement reads are the ones the caller passed with it.",
    },
    Answer {
        item: "ID057",
        answer: "The 64 bit signed integer, which is the one exact numeric type.",
    },
    Answer {
        item: "ID058",
        answer: "The 64 bit signed integer, for the same reason.",
    },
    Answer {
        item: "ID059",
        answer: "The 64 bit signed integer. A count that reached the top of it would raise `22003` rather than wrap, which no file this engine can open is large enough to do.",
    },
    Answer {
        item: "ID061",
        answer: "zu has no SESSION_USER, having no principal for it to name.",
    },
    Answer {
        item: "ID062",
        answer: "The 64 bit signed integer.",
    },
    Answer {
        item: "ID063",
        answer: "The binary64 float. An exact operand beside an approximate one is widened before the kernel sees them, so `2.0 * 3` is 6.0.",
    },
    Answer {
        item: "ID064",
        answer: "The 64 bit signed integer, unwidened.",
    },
    Answer {
        item: "ID065",
        answer: "64 bits, the same as the operands. An answer that does not fit raises `22003` rather than growing the type.",
    },
    Answer {
        item: "ID066",
        answer: "64 bits, on the same terms.",
    },
    Answer {
        item: "ID067",
        answer: "64 bits and scale 0, the answer truncated toward zero. A divisor of nought raises `22012`.",
    },
    Answer {
        item: "ID068",
        answer: "The 64 bit signed integer. CHAR_LENGTH and OCTET_LENGTH both answer it.",
    },
    Answer {
        item: "ID069",
        answer: "The binary64 float, whatever arrived. The answer of a root, a logarithm or an angle is irrational for all but a handful of arguments, and a type that changed with the value would be a type nothing could be planned against.",
    },
    Answer {
        item: "ID070",
        answer: "The 64 bit signed integer.",
    },
    Answer {
        item: "ID074",
        answer: "64 bits.",
    },
    Answer {
        item: "ID075",
        answer: "53 bits of significand, binary64 throughout.",
    },
    Answer {
        item: "ID076",
        answer: "STRING.",
    },
    Answer {
        item: "ID079",
        answer: "The binary64 float, under any of the spellings the standard gives one: an exponent, a `D` suffix, an `F` suffix or an `M` suffix all read as it.",
    },
    Answer {
        item: "ID085",
        answer: "The open dynamic union type, nullable.",
    },
    Answer {
        item: "ID086",
        answer: "DIFFERENT EDGES. Under it a pattern that named no path mode walks a TRAIL, and no one edge answers two of the edge patterns of the list.",
    },
    Answer {
        item: "ID089",
        answer: "GRAPH.",
    },
    Answer {
        item: "ID090",
        answer: "NODE.",
    },
    Answer {
        item: "ID091",
        answer: "EDGE.",
    },
    Answer {
        item: "ID095",
        answer: "The 64 bit signed integer, and a total that does not fit it raises `22003` rather than wrapping.",
    },
    Answer {
        item: "ID096",
        answer: "zu declares none. An average of exact numbers is a binary64 float here, since the average of two integers is not in general an integer and an exact answer would have to be rounded to be given.",
    },
    Answer {
        item: "ID097",
        answer: "The binary64 float.",
    },
    Answer {
        item: "ID098",
        answer: "The binary64 float.",
    },
    Answer {
        item: "ID099",
        answer: "The binary64 float.",
    },
    Answer {
        item: "IE001",
        answer: "zu resolves none. A graph is named by a path in the catalog or is the graph in the file the session opened, and there is no URI anywhere in the language it reads.",
    },
    Answer {
        item: "IE002",
        answer: "One level, and no implementation-defined access mode to select it with. A transaction reads the snapshot it opened on and a write publishes at commit, so a read never sees half a statement and two sessions on one file each see their own snapshot.",
    },
    Answer {
        item: "IE003",
        answer: "None. The identifier syntax is ISO 21.3's general category classes rather than a UAX31 profile.",
    },
    Answer {
        item: "IE004",
        answer: "None declared.",
    },
    Answer {
        item: "IE005",
        answer: "It is refused with `42001` and a position: the line, the column, the byte offset and an excerpt of the source text. Nothing outside the grammar is guessed at, extended or read as something near it.",
    },
    Answer {
        item: "IE006",
        answer: "None. A catalog statement and a data statement mix in one transaction, which is GP18, and two graphs mix in one, which is GT03, so `25G02` and `25G04` are unreachable here.",
    },
    Answer {
        item: "IE007",
        answer: "None, there being no requirement to violate.",
    },
    Answer {
        item: "IE008",
        answer: "Two. `01000` reports a VALUE or EXISTS query that reads a name from the query around it, since that one is answered once per row rather than once and the warning names what to lift out. `01004` reports a cast to a string type that cut trailing spaces off, the cut being lossless in that case and a refusal in every other.",
    },
    Answer {
        item: "IE009",
        answer: "None.",
    },
    Answer {
        item: "IE010",
        answer: "`00001 omitted result`, which a statement that completed without returning a table reports.",
    },
    Answer {
        item: "IL001",
        answer: "A node carries at least one label and at most 64, its labels being the bits of one word. An edge carries exactly one, the type its rel table is named by.",
    },
    Answer {
        item: "IL002",
        answer: "4096 for a node and 4096 for an edge, a table being that many columns wide.",
    },
    Answer {
        item: "IL003",
        answer: "None to 63, for both kinds. A key label set is one short of the label set that has to contain it, so the pattern around it has room in the word.",
    },
    Answer {
        item: "IL009",
        answer: "The sum of the two operands' lengths. A join is the code points of the left followed by the code points of the right and nothing is dropped, so there is no length below which the answer is guaranteed and above which it is not.",
    },
    Answer {
        item: "IL010",
        answer: "19, the digits of 9223372036854775807. A literal with more raises `42001` at the position of the literal rather than being read modulo anything.",
    },
    Answer {
        item: "IL011",
        answer: "64 bits for the integer type and 53 bits of significand for the float. There is no decimal type with a scale of its own, so there is no maximum scale to publish.",
    },
    Answer {
        item: "IL013",
        answer: "zu declares none. A string is bounded by what the store can hold rather than by a number the language publishes, so `22001` is raised by a cast to a narrower string type and not by a limit on the type itself.",
    },
    Answer {
        item: "IL015",
        answer: "zu declares none, which is why `22G10` is unreachable here. A path or a list a query builds is bounded by the row it is built for rather than by a number worth publishing, and a limit invented so a condition could be provoked would be a limit nobody wanted.",
    },
    Answer {
        item: "IL018",
        answer: "zu declares none. `{n,m}` takes any m that fits the integer type, and a quantifier with no ceiling at all is refused with that as the reason rather than being bounded silently.",
    },
    Answer {
        item: "IL020",
        answer: "zu declares none. A catalog path nests as deep as it is written.",
    },
    Answer {
        item: "IL023",
        answer: "-1022 and 1023, IEEE 754 binary64's.",
    },
    Answer {
        item: "IL024",
        answer: "9 digits, a nanosecond, which is what the temporal lanes hold.",
    },
    Answer {
        item: "IS001",
        answer: "A null sorts last under an ascending key and first under a descending one, so it behaves as though it were larger than every value. NULLS FIRST and NULLS LAST are read where they are written and they move the nulls to that end of the result rather than to an end of the order, so a descending key with NULLS FIRST still leads with its nulls.",
    },
    Answer {
        item: "IV001",
        answer: "Unicode, read as UTF-8. Source text that is not valid UTF-8 is refused before it is lexed.",
    },
    Answer {
        item: "IV002",
        answer: "The order zu answers with is total and it is one order for the whole engine, read by `<`, by ORDER BY, by DISTINCT and by GROUP BY alike. Within a type it is the type's own order: numbers by value across the exact and the approximate types, strings by code point, byte strings by byte, temporals of one kind by instant, booleans false before true.",
    },
    Answer {
        item: "IV003",
        answer: "There is one form for each and it is the one the type printer writes, so there is no second spelling for a normal form to be chosen from. A graph type prints as its node types and its edge types in catalog order.",
    },
    Answer {
        item: "IV008",
        answer: "The form the type printer writes: `INT64`, `FLOAT64`, `STRING`, `BOOL`, `LIST<T>` and the temporal names, with `NOT NULL` where a type is not nullable.",
    },
    Answer {
        item: "IV010",
        answer: "Two values of different types compare by a fixed order over the types themselves: null, boolean, number, string, byte string, temporal, node, edge, list, record, path, graph, binding table. It is stated rather than left to whatever order the enum happened to be declared in, because a documented choice can be relied on and a declaration order cannot, and it is why `22G04` is unreachable here.",
    },
    Answer {
        item: "IV011",
        answer: "The union of what a property column can hold: boolean, the 64 bit integer, the binary64 float, string, byte string, date, local time, local datetime, zoned time, zoned datetime, duration, and a list of any one of those.",
    },
    Answer {
        item: "IV012",
        answer: "Everything IV011 names, and node, edge, path, record, graph and binding table besides, which is every type a value can have here.",
    },
    Answer {
        item: "IV014",
        answer: "`ANY`, the single type every value type is under.",
    },
    Answer {
        item: "IV015",
        answer: "There is none, zu having no authorization identifier.",
    },
    Answer {
        item: "IV016",
        answer: "The message beside a condition is written for the person reading it, in English, and says what was asked and what was wrong with it. Where the fault is in source text it carries the line, the column, the byte offset and an excerpt, and where it is a name it carries the kind of thing the name was and the name. The condition text beside the message is the standard's and never zu's.",
    },
    Answer {
        item: "IV023",
        answer: "The space, U+0020, and nothing else. TRIM with no character named takes spaces off both ends, and a wider set is written out with BTRIM, LTRIM or RTRIM.",
    },
    Answer {
        item: "IW001",
        answer: "A session is a process. `zu shell` opens one on a file and reads a request per line as a JSON frame, and the session ends when the input does. The C ABI is the same session opened in process rather than over a pipe, and there is no server and no network protocol in either.",
    },
    Answer {
        item: "IW002",
        answer: "There is none.",
    },
    Answer {
        item: "IW003",
        answer: "End of input on the session's pipe, or the caller closing the connection through the C ABI.",
    },
    Answer {
        item: "IW004",
        answer: "There is none beyond START TRANSACTION, COMMIT and ROLLBACK. A statement written outside one runs in a transaction of its own.",
    },
    Answer {
        item: "IW005",
        answer: "The status object on the last reply, and the process's exit status.",
    },
    Answer {
        item: "IW006",
        answer: "The `params` object of the query frame. The shell's `prepare` op reports the names a statement will read, so a caller can tell what to fill without running it.",
    },
    Answer {
        item: "IW007",
        answer: "As a `gqlstatus` field on every reply, and on a failure as a `failure` object beside it holding the code, the standard's condition text, the severity, zu's message and the diagnostics ID017 lists.",
    },
    Answer {
        item: "IW010",
        answer: "zu provides none.",
    },
    Answer {
        item: "IW011",
        answer: "It is the union of the element types the pattern could bind, worked out from the labels the pattern wrote and the tables of the graph that carry them. A pattern that named no label binds the union of every table the graph holds.",
    },
    Answer {
        item: "IW012",
        answer: "It is the node type of the table the written label names, which the INSERT creates when no table is named by that label and the graph type is open.",
    },
    Answer {
        item: "IW014",
        answer: "zu applies none, which is why `42004` is unreachable here. Two identifiers that render alike are two identifiers: the Latin a and the Cyrillic a are different characters in every encoding and this engine reads them as different names.",
    },
    Answer {
        item: "IW015",
        answer: "zu creates the root directory and nothing else automatically.",
    },
    Answer {
        item: "IW016",
        answer: "A new schema holds nothing until a statement puts something in it.",
    },
    Answer {
        item: "IW017",
        answer: "The code points of the left operand followed by the code points of the right, with no normalization before the join or after it. A join of two strings that were each normalized is not itself guaranteed normalized, and NORMALIZE is how a query that needs it says so.",
    },
    Answer {
        item: "IW018",
        answer: "zu generates none. A cast is written or it is not, and a value that will not convert raises rather than being coerced quietly. The one widening that happens without being written is an exact operand beside an approximate one, which is ID063 rather than a lax cast.",
    },
    Answer {
        item: "IW019",
        answer: "The narrowest type of the set that every other is under, and where there is none, the base type they share. The answer is nullable when any member of the set is.",
    },
    Answer {
        item: "IW021",
        answer: "The order of the type table, which is IV010's order over the primary types and is fixed rather than derived per query.",
    },
    Answer {
        item: "IW022",
        answer: "zu makes no such determination ahead of the assignment. A site holds a validity bit and an assignment of null clears it, which is the same write a REMOVE is.",
    },
    Answer {
        item: "IW023",
        answer: "A delimited identifier is the characters between its quotes, a doubled quote standing for one. A non-delimited identifier is the characters as written. Neither is case folded, so `n` and `N` are two names, and a name written one way in one statement has to be written the same way in the next.",
    },
    Answer {
        item: "IW025",
        answer: "All of them, and any number of them in one transaction. A catalog change is written under the same savepoint a data change is, so the two roll back together and neither can be left behind by the other.",
    },
];

/// The 20 implementation-dependent answers, in code order.
///
/// These are behaviour rather than promises. The standard does not ask
/// for them to be published and does not ask for them to be stable, so a
/// query that depends on one of these is a query that may stop working
/// in a release that changed nothing it was allowed to rely on.
const DEPENDENT: &[Answer] = &[
    Answer {
        item: "UA001",
        answer: "There is one environment per process and two do not interact. Two sessions on one file each read their own snapshot, and what one commits the other sees on its next transaction rather than inside the one it is in.",
    },
    Answer {
        item: "UA002",
        answer: "No. One reply carries one status object, which is IA002's answer read from the other side.",
    },
    Answer {
        item: "UA004",
        answer: "It is not raised. A part of an expression that was never evaluated raises nothing: the second operand of an OR the first operand settled is not evaluated, nor is a CASE branch whose condition did not hold, nor is an argument of COALESCE past the first that was not null. This is deliberate rather than incidental, and the compiler asks which functions can raise when it decides what to put behind a guard.",
    },
    Answer {
        item: "UA005",
        answer: "The ones the walk reached first, which follows the order of the storage's adjacency lists.",
    },
    Answer {
        item: "UA006",
        answer: "None. The walk stops when it has what the selector asked for and probes nothing further.",
    },
    Answer {
        item: "UA007",
        answer: "A statement that is interrupted or runs out of resources rolls back what it had written, the savepoint being the unit that is undone.",
    },
    Answer {
        item: "US001",
        answer: "The order the plan produced them in, which is scan order through the storage for a match and insertion order for a table a statement wrote. It is the same order twice for one plan on one file, and it is not a promise: a plan change moves it and nothing warns.",
    },
    Answer {
        item: "US005",
        answer: "The order the walk reached them, which follows the adjacency lists.",
    },
    Answer {
        item: "US006",
        answer: "The order they arrived in. The sort is stable, ties being broken on the row's position, so a second ORDER BY over an already sorted table keeps what the first one decided.",
    },
    Answer {
        item: "US007",
        answer: "There is no such pair. The order is total, which is what supporting GA04 means, so an ORDER BY never has an Unknown to place.",
    },
    Answer {
        item: "US008",
        answer: "Left to right, with the two exceptions UA004 names: a conjunction stops at the operand that settles it, and only the branch of a CASE that holds is evaluated.",
    },
    Answer {
        item: "US009",
        answer: "When the statement begins, before it is planned. Every reading of the clock inside one statement answers the same instant, so two columns of one row cannot disagree about what time it is.",
    },
    Answer {
        item: "UV001",
        answer: "A node's is its table and its row within that table. An edge's is its rel table and the ordinal the load order gave it. Both are stable for as long as the file is not rewritten.",
    },
    Answer {
        item: "UV003",
        answer: "The first one the evaluator reaches that holds a value of the wrong type, evaluation being left to right.",
    },
    Answer {
        item: "UV004",
        answer: "A string naming the table and the row within it. It is unique within the file and it says nothing about a graph in another file, so a client that stored one and reopened a rewritten file has a string and not a handle.",
    },
    Answer {
        item: "UV005",
        answer: "The physical types the columnar store holds: a boolean is a bit in a word, an integer is two's complement of its width, a float is its IEEE 754 bits, a string is bytes with offsets into a buffer, a date is days from the epoch, a time is nanoseconds, a duration is months and nanoseconds held apart, and a list is offsets over its element column.",
    },
    Answer {
        item: "UV007",
        answer: "The type the compiler worked out for the expression that fills it, which is the narrowest type every value it can hold is under.",
    },
    Answer {
        item: "UV009",
        answer: "None is chosen. zu raises instead, which is the other answer IA017 allows, so this item has no observable value here.",
    },
    Answer {
        item: "UV014",
        answer: "There is none. A duration holds months and nanoseconds apart rather than as one scalar, and a subtraction of two dates answers a duration in days, so no interval is ever converted against an anchor datetime.",
    },
    Answer {
        item: "UW001",
        answer: "The first exception raised, since a statement stops at it. There is never a set of raised conditions to choose a primary from.",
    },
];

/// zu's answer to one item, or None when nothing has been written for
/// it, which the completeness test is what stops from ever happening.
fn answer(code: &str) -> Option<&'static str> {
    DEFINED
        .iter()
        .chain(DEPENDENT)
        .find(|a| a.item == code)
        .map(|a| a.answer)
}

/// The register as markdown, which is what `docs/gql-implementation-defined.md`
/// holds and what a reader of the conformance statement is pointed at.
///
/// Every item the standard lists appears, in the standard's order, with
/// the standard's wording above zu's answer. An item is never left out
/// for having a dull answer, because a reader looking for one and not
/// finding it cannot tell a dull answer from an unanswered question.
pub fn render() -> String {
    let mut out = String::new();
    // The item texts are the standard's, verbatim out of the artifact,
    // and ID090 asks which of NODE or VERTEX zu prefers. The question
    // cannot be asked without the word in it, so the page names the
    // exemption instead of rewording an item, which is the one thing
    // this file is built not to do.
    out.push_str(
        "# What zu leaves to itself\n\
         \n\
         ISO/IEC 39075:2024 Clause 24.5.2 asks a conforming implementation to \
         state its answer to two lists of items, and this file is zu's answer to \
         both of them. Generated by `zu conformance --implementation-defined`. \
         Do not edit by hand: the answers live in `crates/zu-cli/src/impdef.rs` \
         and the items come out of the two published artifacts in \
         `crates/zu-common/artifacts`, mechanically, so an item cannot be \
         reworded here to suit an answer.\n\
         \n\
         The first list is implementation-defined: an answer that must be the \
         same every time and must be published, which is what this file does. \
         The second is implementation-dependent: an answer the standard asks \
         neither to be stable nor to be published. zu publishes those too, as behaviour observed today and \
         not as a promise, because a reader who knows what an engine does can \
         write a query that does not depend on it.\n\
         \n\
         <!-- terms: allow vertex -->\n",
    );
    let mut kind = None;
    for item in ITEMS {
        if kind != Some(item.kind) {
            kind = Some(item.kind);
            let n = ITEMS.iter().filter(|i| i.kind == item.kind).count();
            out.push_str(&format!("\n## {} ({n} items)\n", item.kind.heading()));
        }
        let answer = answer(item.code).expect("every item has an answer; the test says so");
        out.push_str(&format!(
            "\n### {}\n\n{}\n\n{}\n",
            item.code, item.description, answer
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every item the standard lists has exactly one answer, and no
    /// answer names an item the standard does not list.
    ///
    /// This is the test the whole arrangement is for. An item added by an
    /// amendment arrives in the generated table with nothing written for
    /// it and fails here, which is the only moment anybody would notice.
    #[test]
    fn every_item_has_one_answer() {
        for item in ITEMS {
            let n = DEFINED
                .iter()
                .chain(DEPENDENT)
                .filter(|a| a.item == item.code)
                .count();
            assert_eq!(n, 1, "item {} has {n} answers, wanted one", item.code);
        }
        for a in DEFINED.iter().chain(DEPENDENT) {
            assert!(
                ITEMS.iter().any(|i| i.code == a.item),
                "answer names {}, which the artifacts do not define",
                a.item
            );
        }
    }

    /// The two lists are answered in their own tables, so an answer
    /// filed under the wrong one would render under the wrong heading.
    #[test]
    fn each_answer_is_on_the_list_it_was_filed_under() {
        for (answers, kind) in [(DEFINED, Kind::Defined), (DEPENDENT, Kind::Dependent)] {
            for a in answers {
                let item = ITEMS.iter().find(|i| i.code == a.item).expect("known item");
                assert_eq!(item.kind, kind, "{} is filed under the wrong list", a.item);
            }
        }
    }

    /// An answer of no words reads on the page exactly like an answer,
    /// so the shortest one still has to be a finished sentence. Some of
    /// them are one word and that is the whole of the answer: ID090 is
    /// NODE and there is nothing else true to say about it.
    #[test]
    fn every_answer_is_a_finished_sentence() {
        for a in DEFINED.iter().chain(DEPENDENT) {
            assert!(!a.answer.trim().is_empty(), "{} has no answer", a.item);
            assert!(
                a.answer.ends_with('.'),
                "the answer for {} does not finish: {:?}",
                a.item,
                a.answer
            );
        }
    }

    /// The two items zu declares no value for say so in the words the
    /// declaration uses, because those two are the reason two of the
    /// standard's conditions are unreachable here and a reader chasing
    /// that has to land somewhere.
    #[test]
    fn the_two_absences_are_stated_as_absences() {
        for code in ["IL015", "IW014"] {
            let a = answer(code).expect("answered");
            assert!(
                a.contains("declares none") || a.contains("applies none"),
                "{code} does not state its absence"
            );
        }
    }

    #[test]
    fn the_render_holds_every_item() {
        let doc = render();
        for item in ITEMS {
            assert!(
                doc.contains(item.code),
                "{} is not in the render",
                item.code
            );
        }
        assert!(doc.contains("## Implementation-defined (117 items)"));
        assert!(doc.contains("## Implementation-dependent (20 items)"));
    }
}
