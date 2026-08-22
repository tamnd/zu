//! `zu conformance`: the engine's own statement of what it can do, and a
//! check that a conformance report does not contradict it
//! (Spec/2064g/gql/plan/07).
//!
//! The declaration used to live in Go, hard-coded in the gql-compat zu
//! adapter. That is the wrong repository for it. A capability is a fact
//! about this engine, it changes in the same commit that changes the
//! engine, and a reviewer of that commit is the person who knows whether
//! it moved. Keeping it here means a PR that teaches the loader to hold
//! floats also flips `float-values`, in the same diff, in front of the
//! same reviewer. Keeping it there meant the two drifted until somebody
//! noticed a skip that should have been a verdict.
//!
//! `--declare` prints the declaration. `conformance.toml` at the repo
//! root is that output, checked in, with a test that regenerates it and
//! fails on drift, the same arrangement as the GQLSTATUS table.
//!
//! `--verify` reads a gql-compat report and fails when the report and
//! the declaration disagree, in either direction. Over-claiming is the
//! obvious failure and it is not the interesting one. Declaring a
//! feature unsupported when every case for it passes is just as wrong
//! and much easier to leave lying around, because it costs nothing at
//! the time and quietly converts real passes into skips.

use std::process::ExitCode;

use zu::zu1::catalog::{MAX_LABELS, MAX_PROPERTIES};
use zu_json::{self as json, Json};

/// One capability zu declares, with the reason attached.
///
/// The reason is not decoration. A `false` with no reason is
/// indistinguishable from a `false` nobody thought about, and the second
/// is a bug that reads exactly like a finding.
struct Declared {
    key: &'static str,
    supported: bool,
    why: &'static str,
}

/// What zu's storage can hold, in the names gql-compat's fixture package
/// uses. Order is that package's `AllCapabilities` order so a diff of
/// this file against the harness reads straight down.
const DATA: &[Declared] = &[
    Declared {
        key: "labels",
        supported: true,
        why: "a node table is a label",
    },
    Declared {
        key: "multi-label",
        supported: true,
        why: "a node row carries a word with a bit per label, and the table it lives in is the label every row of it carries",
    },
    Declared {
        key: "node-properties",
        supported: true,
        why: "node tables carry property columns",
    },
    Declared {
        key: "edge-properties",
        supported: true,
        why: "a rel table carries property columns, addressed by the edge ordinal the load order gives every edge",
    },
    Declared {
        key: "edge-types",
        supported: true,
        why: "a rel table is an edge type",
    },
    Declared {
        key: "multiple-edge-types",
        supported: true,
        why: "a graph holds several rel tables",
    },
    Declared {
        key: "multiple-node-labels",
        supported: true,
        why: "a rel table names the node table at each of its ends, and the two need not be the same one",
    },
    Declared {
        key: "temporal-values",
        supported: true,
        why: "dates, local times and durations ride their own lanes, declared across the sqlite staging hop",
    },
    Declared {
        key: "list-values",
        supported: true,
        why: "a list column holds one element type, staged as a JSON array in a text column",
    },
    Declared {
        key: "null-properties",
        supported: true,
        why: "a column is dense either way and carries validity words saying which of its rows hold a value",
    },
    Declared {
        key: "float-values",
        supported: true,
        why: "float columns ride the fixed width lane as their IEEE bits",
    },
    Declared {
        key: "boolean-values",
        supported: true,
        why: "boolean columns ride the lane, declared BOOLEAN across the sqlite staging hop",
    },
    Declared {
        key: "undirected-edges",
        supported: true,
        why: "a rel table says whether its edges have a direction, and both stored lists answer for one that has none",
    },
    Declared {
        key: "self-loops",
        supported: true,
        why: "the converter reads endpoints through, so an edge to itself survives",
    },
    Declared {
        key: "parallel-edges",
        supported: true,
        why: "a second edge over the same ordered pair survives",
    },
    Declared {
        key: "parallel-edge-properties",
        supported: true,
        why: "an edge property is addressed by the ordinal the match bound, not by searching the forward list for the destination, so two edges over one pair carry their own values",
    },
];

/// What zu's session and wire protocol can do, as opposed to what its
/// storage can hold. These are the harness's non-data capability flags.
const ENGINE: &[Declared] = &[
    Declared {
        key: "gqlstatus",
        supported: true,
        why: "every reply carries a code from the ISO conditions artifact",
    },
    Declared {
        key: "parameters",
        supported: true,
        why: "the shell takes a params object and prepare reports its names",
    },
    Declared {
        key: "transactions",
        supported: true,
        why: "START TRANSACTION, COMMIT and ROLLBACK run across statements on a session",
    },
    Declared {
        key: "multiple-statements",
        supported: true,
        why: "one shell process runs a case's setup list in order",
    },
    Declared {
        key: "isolated",
        supported: true,
        why: "a reset is a statement on the running session, so a case starts on a graph the case before it did not write to",
    },
];

/// One implementation-defined limit zu declares a finite value for.
///
/// Sixteen of the standard's sixty-eight conditions exist only because
/// an implementation declares a limit. An engine with no maximum on
/// node properties can never raise 22G0S, and a harness asking for
/// sixty-four properties is asking a question with two correct
/// answers, so the number has to come from the engine rather than from
/// the test. This is where it comes from: the item's code in ISO
/// 24.5.2, which kind of element it is the limit of, and the largest a
/// statement may ask for.
///
/// A limit that is not in here is one zu declares no finite value for,
/// and a harness reading this must treat the absence as an absence
/// rather than as a zero.
struct Limit {
    /// The item's code in ISO 24.5.2.
    item: &'static str,
    /// Which kind of graph element the value is for, because the items
    /// that matter here are all written "for each kind of graph
    /// element" and one number would be answering half the question.
    kind: &'static str,
    /// The largest a statement may ask for. One more than this raises
    /// the condition the standard gives that limit.
    max: usize,
    why: &'static str,
}

/// The limits zu declares, which are the storage's own numbers rather
/// than a set picked to make conditions reachable. A label set is the
/// bits of one word, an element holds the columns its table declares,
/// and a key label set is one label short of the word for the same
/// reason the label set is the word.
const LIMITS: &[Limit] = &[
    Limit {
        item: "IL001",
        kind: "node",
        max: MAX_LABELS,
        why: "a node's labels are the bits of one word, so the dictionary and the node are the same 64 wide",
    },
    Limit {
        item: "IL001",
        kind: "edge",
        max: 1,
        why: "an edge is stored under the one type its rel table is named by, so its label set holds one label",
    },
    Limit {
        item: "IL002",
        kind: "node",
        max: MAX_PROPERTIES,
        why: "a node holds the property columns its table declares, and a table is 4096 columns wide",
    },
    Limit {
        item: "IL002",
        kind: "edge",
        max: MAX_PROPERTIES,
        why: "an edge holds the property columns its rel table declares, under the same width",
    },
    Limit {
        item: "IL003",
        kind: "node",
        max: 63,
        why: "a key label set has to leave room in the word for whatever the pattern around it adds, so it is one short of the label set that has to contain it",
    },
    Limit {
        item: "IL003",
        kind: "edge",
        max: 63,
        why: "an edge type's key label set is bounded by the same word, for the same reason",
    },
];

/// Notes the report prints verbatim next to zu's numbers. Anything a
/// reader needs in order to interpret a result and could not work out
/// from the numbers goes here.
const NOTES: &[&str] = &[
    "driven through `zu shell --format jsonl`, one long-lived process per session",
    "loaded through `zu convert`, which reads a SQLite database in zu's schema",
    "the evaluator is MATCH WHERE FILTER LET FOR CALL UNWIND WITH RETURN, plus INSERT of node patterns and \
     of edges between the elements a statement has in scope, SET and REMOVE of properties and \
     of labels, and DELETE and DETACH DELETE of elements, so a case that writes anything else \
     is answered with an error rather than a skip",
    "several statements chain with NEXT, and what one hands the next is the result it returned \
     and nothing else it had in hand, so a variable the statement before it matched is out of \
     scope behind the NEXT; the chain runs as one pipeline rather than materialising a table \
     between statements",
    "two result tables meet with UNION, EXCEPT or INTERSECT in both their forms, ALL keeping \
     every copy and DISTINCT keeping one, and leaving the quantifier out means DISTINCT; the \
     conjunctions are all at one level and fold to the left, so there is no precedence between \
     UNION and INTERSECT, and EXCEPT and INTERSECT hold whichever operand the planner estimates \
     fewer rows for and stream the other past it",
    "OTHERWISE answers the left operand, and runs the right one only when the left answered no \
     rows at all",
    "the operands of a conjunction have to have the same columns, in the same order and under \
     the same names, and neither of them may write, because how many times a write ran would \
     otherwise depend on which operand the planner chose to hold",
    "a FILTER keeps the rows its condition holds for and has no pattern under it, so it reads \
     what the statement already has, including what a NEXT handed it, and the WHERE the \
     standard allows after the word is optional and says nothing more",
    "a LET names values and takes no name away, which is what makes it a LET rather than a \
     WITH, and the definitions read left to right so a later one may use a name an earlier one \
     in the same statement gave; the name is a variable, so LET of a property is refused with \
     the statement that does change a property named",
    "a projection holding an aggregate groups by the items that are not aggregates, and GROUP \
     BY says that same grouping out loud, so the keys and the non-aggregate items have to be \
     the same: an item the grouping does not fix has no one value in a group and a key the \
     projection does not carry leaves no column saying which group a row is, and both are \
     refused by name rather than answered with a row of the group picked arbitrarily",
    "a page of a result is written OFFSET and LIMIT, and SKIP is the synonym the standard \
     names for OFFSET, so the two words are one clause and writing both of them is refused \
     rather than skipping twice",
    "a FOR makes a row of every element of a list, which is the same statement UNWIND is and \
     the spelling the standard gives it, and WITH ORDINALITY or WITH OFFSET numbers those rows, \
     from one and from zero; the number counts the elements of the list rather than the rows \
     the statement has answered, so a FOR under a match starts again at each row that reaches \
     it",
    "the graph a statement is against is named in front of it, as the graph the session is \
     working in, the graph it started in, a graph the catalog holds by name or by path, or a \
     graph the caller passed in as a parameter; the last is a graph reference and not a name, \
     so a string spelling a graph's name is refused, and a reference to a graph that has been \
     dropped since it was taken is refused rather than read as whatever holds that place now",
    "a step repeats as many times as one quantity says, written as a range inside the brackets \
     or as the standard's quantifier behind the arrow, `+` for one or more, `{n}` for exactly n \
     and `{n,m}` for a range with either end left out, and a step carrying both forms is refused \
     rather than one of them being read and the other dropped; a repetition that may take no hop \
     at all, a bare `*` or a range starting at zero, is refused with that as the reason rather \
     than answered as though it had been written `+`",
    "a path selector says how many of the paths a pattern matches are kept per pair of \
     endpoints, and all seven are read: ALL keeps every one of them and is what a pattern with \
     no selector keeps, ANY and ANY k keep that many and the standard leaves which ones to the \
     engine, ANY SHORTEST and ALL SHORTEST keep one and every path of the least length, \
     SHORTEST k keeps k paths counted shortest first, and SHORTEST k GROUP keeps every path \
     whose length is one of the k least; a selector needs a repeating step to choose between \
     paths, since a pattern of fixed length matches one path per set of elements, and a count \
     of zero is refused because it keeps no path whatever the graph holds",
    "a KEEP behind a list of patterns says the path mode or the path selector once for every \
     pattern of that list, instead of once in front of each of them, and it says the same thing, \
     so the query written either way answers the same rows; it fills in what a pattern left out \
     rather than overruling what one wrote, so a pattern naming a mode and a KEEP naming another \
     is refused by name, and so is a KEEP with neither a mode nor a selector after it",
    "a repeated step walks under a path mode, and the default is TRAIL, which repeats no edge: \
     WALK repeats anything, ACYCLIC repeats no node, and SIMPLE repeats no node except that a \
     path may end where it began, so a cycle is a simple path and is not an acyclic one and \
     neither mode is a substitute for the other",
    "a match mode says what a whole list of patterns may bind twice where a path mode says what \
     one path of it may, and DIFFERENT EDGES is what a list naming none means: a pattern that \
     named no path mode walks a TRAIL under it, and no one edge answers two of the edge patterns \
     of the list, while REPEATABLE ELEMENTS lifts both; either mode is written with the singular \
     noun and with BINDINGS behind it and all four spellings say one thing, and the mode belongs \
     to the pattern list rather than to the match statement block, so two patterns of one \
     statement may not take the same edge where two patterns of two statements of a block may. \
     The edges kept apart are those whose steps describe the same pair of ends, which is where \
     any graph at all can answer both patterns with one edge; two steps that end somewhere else \
     coincide only on a graph holding a loop, and that pair is not checked yet",
    "brackets around part of a pattern match nothing of their own, and what they are for is that \
     a name, a path mode and a condition can be said about the stretch they hold rather than \
     about the whole pattern: a name inside them binds a path over that stretch, a mode inside \
     them governs the steps they hold with the tightest pair of brackets winning, and a WHERE \
     inside them may read a variable bound outside them, which is the non local predicate. A \
     factor written straight against another meets it at a node, so the two node patterns there \
     describe one node and two names written there are two names for one element; a name that \
     already stands for something else is refused rather than joined to it. A path selector \
     inside the brackets is refused, the standard writing one in front of a whole pattern",
    "a quantifier behind those brackets repeats the stretch they hold, and a fixed count is \
     written out as that many copies of the steps, so a stretch written `{2}` walks two edges \
     and keeps its copies apart the way a repeated step does; a count of several lengths, \
     written as a range or as the question mark that stands for nought or one, is a pattern \
     per length rather than one pattern, and the lengths are written out and run over the fork \
     seam the bar below runs on, so a stretch of nought to two lengths is three walks and the \
     length nought is the one node the ends of the stretch meet at. A count with no ceiling on \
     it is refused, since there is no writing out a list with no end to it. Every repetition \
     binds the names inside the brackets again, so a name there stands for one element per \
     repetition rather than for one element: reading it answers the list of them in the order \
     the walk took them, reading a property of it answers the list of the properties, and an \
     aggregate written around it folds that row's bindings rather than the rows, which is one \
     row in and one row out and no grouping under it. A name inside a stretch of several \
     lengths is out of reach behind it, the lengths holding the elements in different places \
     and the row between the parts holding one column per name, and it is the reading of the \
     name that is refused rather than the walk",
    "a simplified path pattern writes a stretch of edges between one pair of slashes, so \
     `-/ knows knows /->` is the two steps that pattern spells out and every node between them \
     is a node nobody named. The arrow around the slashes says which way the steps go and there \
     are seven of them, the same seven an ordinary arrow writes, so a stretch written between \
     `-/` and `/-` walks either way and one written between `~/` and `/~>` walks an undirected \
     edge or an outgoing one. That direction is the one every step of the stretch takes unless \
     the step overrides it, which a step does by writing `<` in front of the type or `>` behind \
     it. Inside the slashes a type is one step, two types written against each other are two \
     steps with a node between them, a bar between two types is the one step either type \
     answers, brackets group, and a quantifier repeats what stands in front of it: a count with \
     a floor of one or more over a single step is the hop range the arrow already writes, and \
     every other count is written out the way a quantifier behind brackets is. What the standard \
     writes for label expressions this engine has no store for is refused by name: an edge is \
     kept under one type here, so a negated type, a wildcard and a conjunction have no answer, \
     and a bar separating stretches of more than one step is refused as well, that being an \
     alternation of paths rather than of labels and something the bar between whole patterns \
     already says",
    "a condition may be written inside an element pattern, and it is asked of the one element \
     that pattern stands on where the pattern reaches it rather than of the row a whole pattern \
     built: a node writes it after its labels and its property map and an edge writes it inside \
     the brackets, and both may read the names the pattern bound to their left, which is what \
     makes them non local. Being part of the pattern is what tells the condition apart from the \
     same text written behind it: under an OPTIONAL MATCH a node the condition refuses is a \
     match that did not happen rather than a row to drop, so the left side keeps its row. Two \
     conditions where two stretches meet are both asked of the node they meet at; one inside a \
     repeated stretch is refused, since the name inside the brackets stands for the group and \
     not for the repetition's element; and one inside an inserted pattern is refused, an INSERT \
     describing an element to make rather than one to find",
    "six predicates ask about an element the statement already bound rather than about the rows \
     a pattern is walking: IS DIRECTED holds of an edge whose table stores a direction and is \
     refused of a node, which has none to answer with; IS LABELED asks the label expression a \
     pattern would write after a colon, reading the labels a node's row carries and the name of \
     the table an edge is in; IS SOURCE OF and IS DESTINATION OF relate a node to an edge, \
     which is a comparison and not another read of the graph because an edge value carries both \
     of its ends; ALL_DIFFERENT and SAME compare element identity and are written of at least \
     two elements, one element being the same as itself and different from nothing; and \
     PROPERTY_EXISTS asks whether an element carries a property without reading it, so a \
     property that is there and null is there. All six answer null over an element that is \
     null, which is the reading an unmatched optional row gets",
    "a property reference to a property the element does not have is null and not an error, ISO \
     20.11, which is the same answer an element that carries the property and holds nothing in \
     it gives, so a query cannot tell a property nobody wrote from a property nobody stored and \
     PROPERTY_EXISTS is how the two are told apart. A graph where half the people have a \
     nickname is one column half the rows are null in, and a graph where none of them do is the \
     same question asked of no column",
    "a case is written in both of the standard's forms, one asking a condition per branch and \
     one naming a value and comparing each branch with it, and a value before the first WHEN is \
     what tells them apart; the branches are asked in the order they were written, a branch \
     whose condition is null is a branch that did not hold, a null subject of the simple form \
     equals no branch at all, and a case that holds no branch and wrote no ELSE is null. Only \
     the branch that holds is evaluated, so the expression the other rows cannot answer may be \
     written in a branch they never reach. COALESCE and NULLIF are the two abbreviations, read \
     as forms rather than as functions for the same reason: COALESCE stops at the first \
     argument that is not null, which is a decision a function would have made after all of \
     them had been evaluated",
    "a path variable binds the walk as a path value rather than as the list of the same \
     elements, so PATH builds one, PATH_LENGTH counts its edges, ELEMENTS answers its nodes \
     and edges as a list in the order they were walked, and two paths over the same elements \
     compare equal; ELEMENTS is the one way to read what a path holds, since a path is a value \
     of its own and nothing indexes into one, and it is refused on a list, which is already \
     the list of its elements",
    "a YIELD after a match says which of the names it wrote leave it, and under what name, so \
     a name the yield does not carry is out of scope behind it and a name the match did not \
     write is refused; it narrows the columns and leaves the rows alone, so the rows a match \
     answered are the rows the yield answers",
    "a VALUE query is a whole query written where one value belongs, so it may chain and sort \
     and aggregate the way any other query does, and it has to return one column because one \
     value is what stands there; a query answering no row stands for a null and one answering \
     several rows is refused; one reading nothing from the query around it is answered once for \
     the whole statement, and one reading a name from it is answered once for every row and \
     says so in the plan and in a warning beside the rows",
    "a block of match statements stands wherever one does: an EXISTS takes one and asks \
     whether it answered a row, and an OPTIONAL takes one and keeps what it answered. The \
     statements of a block are all required and they share the names they write, so a block is \
     one conjunction and the same thing the commas of a single statement are; under an OPTIONAL \
     it is also one operand, so a block that matches half of itself matches none of it and \
     every name it writes reads null",
    "the existence predicate is written in the three shapes the standard gives it, a graph \
     pattern, a block of match statements and a whole query, and the first two stand in \
     parentheses as well as in braces; the RETURN is what tells a query from a block. What the \
     query returns is never read, only whether it returned a row, so it may return any number \
     of columns and a query answering no row is false rather than the null a query written \
     where a value belongs would stand for; the run stops at the first row, so a predicate \
     over a query that would match a million times costs the first match. A query has a scope \
     of its own, so it reads the row around it through its expressions rather than through its \
     patterns, and a pattern in it writing a name the row already carries is refused by name \
     rather than quietly meaning a second element",
    "the character string expressions of ISO 20.22 to 20.24: two strings are joined with the \
     concatenation operator, which binds tighter than a comparison and looser than an addition, \
     so a join written beside a sum joins the sum; a string is measured by its characters with \
     CHAR_LENGTH, spelled out as CHARACTER_LENGTH for the same function, and by the bytes the \
     store keeps with OCTET_LENGTH, and the two answer the same number only for ASCII; UPPER \
     and LOWER fold it, and TRIM of one argument takes the spaces off both ends. A number is \
     refused rather than measured or joined by its spelling, since a query that meant the \
     digits says so with a CAST, and a null anywhere in one of these is a null out",
    "an INSERT runs once for every row the clauses before it answered, and the clauses after \
     it read the rows it wrote rather than the store, so a MATCH followed by an INSERT writes \
     one element per row the match answered",
    "an edge carries properties the way an element does, so a written edge holds one value for \
     every column its table stores, and an edge written into a table that stores none on its \
     edges is refused by name rather than dropping what it carried",
    "a SET changes what an element an earlier clause found holds, one row at a time, and the \
     clauses after it read the new value; a property of a node, a property of an edge and the \
     whole record of either are all reachable, and the record form empties every property it \
     does not name",
    "SET and REMOVE of a label change the bit the label is in the row's label word, so a \
     pattern naming that label finds the row afterwards; a label the row's table has not \
     declared is declared by the SET that puts it on, published with the rows the statement \
     changed and undone with them, while the name of the table is the label every row of it \
     carries rather than one a statement puts on or takes off",
    "a REMOVE is the assignment of a null the standard says it is, so it and SET of a null are \
     one write and a column holds the absence as a clear validity bit",
    "a DELETE takes away the element an earlier clause found, a DETACH DELETE takes its edges \
     with it, and a plain DELETE of an element that still has edges on it is refused with the \
     code the standard gives that rather than leaving a dangling edge",
    "a delete item is a variable an earlier clause bound or a query answering the element, \
     written VALUE and the query in braces, and the query runs on its own against the same \
     graph, so it reads the store rather than the variables around it and has to answer one \
     row of one column because one item takes away one element",
    "an element is created in the node table whose own name is the label the pattern wrote, \
     and a label no node table is named by makes one, out of the properties the pattern \
     writes and under the savepoint the statement holds, so a property written as a value \
     that has to be worked out first is refused rather than guessed at",
    "an edge type no rel table is named by makes one as well, between the node tables the two \
     ends of the step are in, and an end nothing gives a label to is refused rather than \
     guessed at because a rel table has to have both of its ends",
    "a graph with a closed graph type is checked at the write rather than at the read, so a \
     label change that would leave an element carrying a label set no element type of that \
     graph type describes is refused with the code the standard gives that, naming the row \
     and the set it would have carried, and a label that names no node table makes no table \
     in such a graph because the type already says what the graph holds",
    "the limits a write can reach are declared and finite rather than absent, so a statement \
     that asks for more than one of them is told the standard's answer for hitting that limit \
     rather than a general failure: a node carries between one label and 64 of them, an edge \
     carries the one label its rel table is named by, and an element or an edge carries up to \
     4096 properties",
    "assigning to the same property of one element twice in one SET is refused, because an \
     element holds one value per property and the clause has not said which of the two it \
     wants, while two SET clauses in a row stay last wins",
    "an element an earlier clause found and a DELETE in the same statement took away is gone \
     for the clauses after it, so reading a property off it and writing an edge onto it are \
     both refused by name rather than reading what the row used to hold",
    "a protocol fault, a malformed frame or an unknown op, reports no GQLSTATUS on purpose \
     and is scored on its message",
];

/// Renders the declaration as TOML.
///
/// Hand-rolled, like the JSON in `main.rs` and for the same reason: T7
/// caps the binary at 15 MiB and this is the only place that needs it.
/// Nothing here contains a quote or a backslash, and a test asserts that,
/// so there is no escaping to get wrong.
pub(crate) fn render() -> String {
    let mut out = String::new();
    out.push_str(
        "# What zu declares it can do, for the gql-compat harness.\n\
         #\n\
         # Generated by `zu conformance --declare`. Do not edit by hand: run\n\
         # `ZU_UPDATE_CONFORMANCE=1 cargo test -p zu-cli --test conformance_toml`.\n\
         #\n\
         # Every entry carries a reason, because a `false` with no reason is\n\
         # indistinguishable from a `false` nobody thought about, and the second\n\
         # is a bug that reads exactly like a finding.\n\n",
    );
    out.push_str("[engine]\n");
    out.push_str("name = \"zu\"\n");
    out.push_str(&format!("version = \"{}\"\n\n", crate::VERSION));

    out.push_str("# What the storage can hold.\n[data]\n");
    for d in DATA {
        out.push_str(&format!("{} = {}  # {}\n", d.key, d.supported, d.why));
    }
    out.push_str("\n# What the session and the wire protocol can do.\n[capabilities]\n");
    for d in ENGINE {
        out.push_str(&format!("{} = {}  # {}\n", d.key, d.supported, d.why));
    }
    out.push_str(
        "\n# The implementation-defined limits of ISO 24.5.2 that zu declares a\n\
         # finite value for, as the largest a statement may ask for. An item\n\
         # that is not here is one zu sets no maximum on, and a reader must\n\
         # take the absence for an absence rather than for a zero.\n[limits]\n",
    );
    for l in LIMITS {
        out.push_str(&format!(
            "\"{}/{}\" = {}  # {}\n",
            l.item, l.kind, l.max, l.why
        ));
    }
    out.push_str("\n# Printed verbatim beside zu's numbers in the report.\nnotes = [\n");
    for n in NOTES {
        out.push_str(&format!("  \"{n}\",\n"));
    }
    out.push_str("]\n");
    out
}

/// Everything `--verify` found wrong, as sentences a reader can act on.
///
/// It collects rather than returning on the first problem. A run that
/// drifted usually drifted in several places at once, and reporting one
/// at a time turns a single fix into five CI rounds.
fn verify_report(report: &Json) -> Vec<String> {
    let mut problems = Vec::new();

    let Some(caps) = report.get("engine").and_then(|e| e.get("capabilities")) else {
        return vec!["the report has no engine.capabilities to check against".into()];
    };

    // The declaration and the adapter must agree exactly. This is the
    // check the whole file exists for: the two live in different
    // repositories and nothing else notices when they part company.
    let data = caps.get("Data");
    for d in DATA {
        match data.and_then(|m| m.get(d.key)) {
            Some(Json::Bool(reported)) if *reported == d.supported => {}
            Some(Json::Bool(reported)) => problems.push(format!(
                "data capability {}: zu declares {} but the harness reported {reported}",
                d.key, d.supported
            )),
            // Not declared is not "no". A capability the adapter never
            // mentioned reads as false in the report and is
            // indistinguishable from one it considered and rejected.
            _ => problems.push(format!(
                "data capability {}: zu declares {} and the harness reported nothing at all",
                d.key, d.supported
            )),
        }
    }

    // The harness spells its non-data flags in Go's field case, so the
    // mapping is written out rather than derived from the key.
    for (key, field) in [
        ("gqlstatus", "GQLStatus"),
        ("parameters", "Parameters"),
        ("transactions", "Transactions"),
        ("multiple-statements", "MultipleStatements"),
        ("isolated", "Isolated"),
    ] {
        let declared = ENGINE
            .iter()
            .find(|d| d.key == key)
            .expect("every mapped key is declared")
            .supported;
        match caps.get(field) {
            Some(Json::Bool(reported)) if *reported == declared => {}
            Some(Json::Bool(reported)) => problems.push(format!(
                "capability {key}: zu declares {declared} but the harness reported {reported}"
            )),
            _ => problems.push(format!(
                "capability {key}: zu declares {declared} and the harness reported nothing at all"
            )),
        }
    }

    // A limit drifts the same way a capability does and is worse when
    // it drifts, because a harness sizing a statement from a stale
    // number does not fail: it sends a statement under the real limit
    // and records that the condition was not reachable.
    let limits = caps.get("Limits");
    for l in LIMITS {
        let key = format!("{}/{}", l.item, l.kind);
        match limits
            .and_then(|m| m.get(&key))
            .and_then(Json::as_u64)
            .map(|n| n as usize)
        {
            Some(reported) if reported == l.max => {}
            Some(reported) => problems.push(format!(
                "limit {key}: zu declares {} but the harness reported {reported}",
                l.max
            )),
            None => problems.push(format!(
                "limit {key}: zu declares {} and the harness reported nothing at all",
                l.max
            )),
        }
    }

    problems.extend(verify_claims_are_not_empty(report));
    problems.extend(verify_nothing_was_contradicted(report));
    problems
}

/// Checks a challenging run for a claim of absence the engine did not keep.
///
/// `gql-compat run -challenge` ignores the declaration, runs the cases it
/// would have excluded, and writes one entry per claim into
/// `declarations`. An entry marked `contradicted` is one where every
/// excluded case passed, which is the one outcome an engine that lacks the
/// thing cannot produce. An ordinary run writes no such array and this
/// check has nothing to say.
///
/// This is the half of `--verify` that could not be written before. The
/// comparison above catches a declaration the adapter reports differently,
/// which is drift between two files. This catches a declaration that is
/// simply wrong, and the only evidence for it is cases that ran.
fn verify_nothing_was_contradicted(report: &Json) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(Json::Arr(declarations)) = report.get("declarations") else {
        return problems;
    };
    for d in declarations {
        if d.get("contradicted") != Some(&Json::Bool(true)) {
            continue;
        }
        let claim = d.get("claim").and_then(Json::as_str).unwrap_or("(unnamed)");
        let cases = d.get("cases").and_then(Json::as_u64).unwrap_or(0);
        let ids = match d.get("passing") {
            Some(Json::Arr(items)) => items
                .iter()
                .filter_map(Json::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        problems.push(format!(
            "zu declares {claim} absent, and all {cases} case(s) it excluded passed: {ids}"
        ));
    }
    problems
}

/// Checks that a capability zu claims actually did something in the run.
///
/// This is the direction that rots quietly. Declaring a feature
/// unsupported when it works costs nothing at the time and silently
/// turns real passes into skips; claiming one that never fires costs
/// nothing either, and inflates the declaration table in the report
/// while the case row stays empty. Neither shows up as a failure
/// anywhere else, so it has to be asserted here.
fn verify_claims_are_not_empty(report: &Json) -> Vec<String> {
    let mut problems = Vec::new();
    let claims_gqlstatus = ENGINE
        .iter()
        .find(|d| d.key == "gqlstatus")
        .expect("gqlstatus is declared")
        .supported;
    if !claims_gqlstatus {
        return problems;
    }
    let Some(Json::Arr(cases)) = report.get("cases") else {
        problems.push("the report has no cases, so no claim can be checked".into());
        return problems;
    };
    let graded = cases
        .iter()
        .filter(|c| {
            c.get("got_gqlstatus")
                .and_then(Json::as_str)
                .is_some_and(|s| !s.is_empty())
        })
        .count();
    if graded == 0 {
        problems.push(
            "zu declares gqlstatus but no case in the report was graded on a code, \
             so the claim did nothing"
                .into(),
        );
    }
    problems
}

/// The same declaration as JSON, for the harness rather than for a
/// reader.
///
/// The checked-in artifact is TOML because a person has to read it and
/// the reasons matter more than the flags. The harness is written in Go,
/// which has no TOML parser in its standard library, and adding a
/// dependency to that repository so it can read forty lines of key and
/// bool is a worse trade than emitting the same tables twice. Both come
/// from `DATA` and `ENGINE`, so they cannot disagree, and the drift test
/// pins the TOML.
///
/// The reasons are deliberately not here. They exist for a person
/// reading the declaration, and a harness that could read them would
/// sooner or later match on one.
fn render_json() -> String {
    let mut out = String::from("{\"engine\":{\"name\":\"zu\",\"version\":\"");
    out.push_str(crate::VERSION);
    out.push_str("\"},\"data\":{");
    for (i, d) in DATA.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}\":{}", d.key, d.supported));
    }
    out.push_str("},\"capabilities\":{");
    for (i, d) in ENGINE.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}\":{}", d.key, d.supported));
    }
    out.push_str("},\"limits\":{");
    for (i, l) in LIMITS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}/{}\":{}", l.item, l.kind, l.max));
    }
    out.push_str("},\"notes\":[");
    for (i, n) in NOTES.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{n}\""));
    }
    out.push_str("]}\n");
    out
}

pub(crate) fn conformance_command(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("--declare") => match args.get(1).map(String::as_str) {
            None => {
                print!("{}", render());
                ExitCode::SUCCESS
            }
            Some("--format") => match args.get(2).map(String::as_str) {
                Some("toml") => {
                    print!("{}", render());
                    ExitCode::SUCCESS
                }
                Some("json") => {
                    print!("{}", render_json());
                    ExitCode::SUCCESS
                }
                _ => crate::usage_error("conformance"),
            },
            _ => crate::usage_error("conformance"),
        },
        Some("--verify") => match args.get(1) {
            Some(path) => verify(path),
            None => crate::usage_error("conformance"),
        },
        Some("--tally") => match args.get(1) {
            Some(path) => crate::scoreboard::tally_command(path),
            None => crate::usage_error("conformance"),
        },
        Some("--scoreboard") if args.len() > 1 => crate::scoreboard::scoreboard_command(&args[1..]),
        Some("--regressed") => match (args.get(1), args.get(2)) {
            (Some(report), Some(baseline)) => {
                crate::scoreboard::regressed_command(report, baseline)
            }
            _ => crate::usage_error("conformance"),
        },
        _ => crate::usage_error("conformance"),
    }
}

fn verify(path: &str) -> ExitCode {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("zu conformance: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = match json::parse(&text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zu conformance: {path} is not JSON: {e}");
            return ExitCode::FAILURE;
        }
    };
    let problems = verify_report(&report);
    if problems.is_empty() {
        println!("zu conformance: the report agrees with conformance.toml");
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "zu conformance: the report contradicts conformance.toml in {} place(s):",
        problems.len()
    );
    for p in &problems {
        eprintln!("  {p}");
    }
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declaration_carries_a_reason() {
        for d in DATA.iter().chain(ENGINE) {
            assert!(
                !d.why.trim().is_empty(),
                "{} declares {} with no reason",
                d.key,
                d.supported
            );
        }
        for l in LIMITS {
            assert!(
                !l.why.trim().is_empty(),
                "{}/{} declares {} with no reason",
                l.item,
                l.kind,
                l.max
            );
        }
    }

    /// A limit is only worth declaring if it is the number the engine
    /// actually enforces. The two that live in a constant are read from
    /// it; the key label set is a number the parser holds privately, so
    /// it is written here and asserted against the message the parser
    /// produces in its own tests.
    #[test]
    fn the_declared_limits_are_the_engines_own_numbers() {
        let of = |item: &str, kind: &str| {
            LIMITS
                .iter()
                .find(|l| l.item == item && l.kind == kind)
                .unwrap_or_else(|| panic!("{item}/{kind} is declared"))
                .max
        };
        assert_eq!(of("IL001", "node"), MAX_LABELS);
        assert_eq!(of("IL001", "edge"), 1);
        assert_eq!(of("IL002", "node"), MAX_PROPERTIES);
        assert_eq!(of("IL002", "edge"), MAX_PROPERTIES);
        assert_eq!(of("IL003", "node"), MAX_LABELS - 1);
        assert_eq!(of("IL003", "edge"), MAX_LABELS - 1);
    }

    /// Every item zu declares a value for is one of ISO 24.5.2's, and
    /// no kind is invented either: a harness keys on both halves, and a
    /// typo in one of them reads as an engine that declares nothing.
    #[test]
    fn the_limits_name_items_and_kinds_the_standard_has() {
        for l in LIMITS {
            assert!(
                matches!(l.item, "IL001" | "IL002" | "IL003"),
                "{} is not an implementation-defined item this engine declares",
                l.item
            );
            assert!(
                matches!(l.kind, "node" | "edge"),
                "{} is not a kind of graph element",
                l.kind
            );
        }
    }

    #[test]
    fn nothing_needs_toml_escaping() {
        // The renderer does no escaping, which is fine only as long as
        // this holds. When it stops holding the renderer has to grow an
        // escape, not the string get quietly reworded.
        let all = DATA
            .iter()
            .chain(ENGINE)
            .flat_map(|d| [d.key, d.why])
            .chain(LIMITS.iter().flat_map(|l| [l.item, l.kind, l.why]))
            .chain(NOTES.iter().copied());
        for s in all {
            assert!(
                !s.contains('"') && !s.contains('\\') && !s.contains('\n'),
                "{s:?} needs escaping the renderer does not do"
            );
        }
    }

    #[test]
    fn the_declaration_matches_the_harnesss_capability_list() {
        // gql-compat's fixture.AllCapabilities, in its order. If the
        // harness grows a capability and zu says nothing about it, the
        // report prints "no" for something nobody decided, which is the
        // exact failure `Capabilities.Undeclared` exists to catch. This
        // is that check on our side of the wire.
        let expected = [
            "labels",
            "multi-label",
            "node-properties",
            "edge-properties",
            "edge-types",
            "multiple-edge-types",
            "multiple-node-labels",
            "temporal-values",
            "list-values",
            "null-properties",
            "float-values",
            "boolean-values",
            "undirected-edges",
            "self-loops",
            "parallel-edges",
            "parallel-edge-properties",
        ];
        let declared: Vec<&str> = DATA.iter().map(|d| d.key).collect();
        assert_eq!(
            declared, expected,
            "declaration is out of step with gql-compat"
        );
    }

    #[test]
    fn the_rendered_toml_says_what_the_tables_say() {
        let toml = render();
        assert!(toml.contains("name = \"zu\""));
        assert!(toml.contains("gqlstatus = true"));
        assert!(toml.contains("transactions = true"));
        assert!(toml.contains("float-values = true"));
        assert!(toml.contains("self-loops = true"));
        // The reason rides along on the same line as the value, so a
        // reader of the file never has to go looking for it.
        assert!(toml.contains(
            "float-values = true  # float columns ride the fixed width lane as their IEEE bits"
        ));
        // A limit is a number and a reason on one line, under a key
        // naming the item and the kind of element, because ISO writes
        // these items per kind and one number would answer half.
        assert!(toml.contains("[limits]"));
        assert!(
            toml.contains("\"IL002/node\" = 4096  # a node holds"),
            "{toml}"
        );
        assert!(
            toml.contains("\"IL001/edge\" = 1  # an edge is stored"),
            "{toml}"
        );
    }

    #[test]
    fn the_rendered_json_carries_the_limits_a_harness_sizes_from() {
        let text = render_json();
        let json = json::parse(&text).expect("the declaration is JSON");
        let limits = json.get("limits").expect("a limits object");
        assert_eq!(
            limits.get("IL002/node").and_then(Json::as_u64),
            Some(MAX_PROPERTIES as u64)
        );
        assert_eq!(limits.get("IL001/edge").and_then(Json::as_u64), Some(1));
        // An item with no finite value is absent rather than zero: a
        // harness that read a zero would send an empty statement and
        // call the condition unreachable.
        assert!(limits.get("IL015/path").is_none());
    }

    #[test]
    fn a_claim_the_harness_contradicted_fails_verification() {
        let report = json::parse(
            r#"{"declarations":[
                 {"claim":"float-values","skip_reason":"fixture-capability",
                  "cases":3,"pass":3,"contradicted":true,
                  "passing":["mandatory/return/float","optional/gv01/double"]}]}"#,
        )
        .expect("test report parses");
        let problems = verify_nothing_was_contradicted(&report);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("float-values"), "{}", problems[0]);
        // The case ids travel with the complaint. A contradiction nobody
        // can reproduce is a line in a log and not a bug report.
        assert!(
            problems[0].contains("mandatory/return/float"),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn a_claim_the_harness_confirmed_passes_verification() {
        // Both shapes have to be quiet: a run that challenged the
        // declaration and found it honest, and an ordinary run, which
        // writes no declarations at all and is the common case.
        let challenged = json::parse(
            r#"{"declarations":[
                 {"claim":"GQ13","skip_reason":"required-feature",
                  "cases":2,"pass":0,"fail":2,"contradicted":false}]}"#,
        )
        .expect("test report parses");
        assert!(verify_nothing_was_contradicted(&challenged).is_empty());

        let ordinary = json::parse(r#"{"cases":[]}"#).expect("test report parses");
        assert!(verify_nothing_was_contradicted(&ordinary).is_empty());
    }
}
