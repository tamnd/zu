//! What a case is, and how a file of them is read.
//!
//! A case is a statement and what running it must produce. That is
//! deliberately the whole of it. Every client in every language can
//! run a statement and look at the rows that come back, so a corpus
//! written in those terms is one every client can run, and a corpus
//! written in terms of a client's own API would be nine corpora.
//!
//! The expectation is either rows or a condition. A case expecting a
//! condition names the GQLSTATUS code, not the message, because the
//! code is the contract and the message is prose that will improve.
//!
//! A statement may take parameters, which is the other direction the
//! same values travel: a case with `params:` writes a value in the
//! encoding, hands it to the client's own binding call, and asserts
//! what came back. A client that decodes a date correctly and encodes
//! it a day early passes every case that has no parameters in it.

use crate::load::Load;
use crate::value;
use crate::yaml::{self, Node};

use zu::query::Value;

/// The schema version a file declares. It exists so that a corpus
/// unpacked from an old release tells a new runner what it is instead
/// of failing in the middle.
pub const SCHEMA: i64 = 3;

/// What running a case's statement has to produce.
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    /// The column names and the rows, in order. Order is part of the
    /// expectation because a case that did not care would say so with
    /// an ORDER BY.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// The five character GQLSTATUS code the statement has to raise.
    Raises(String),
}

#[derive(Debug, Clone)]
pub struct Case {
    pub name: String,
    /// Why the case is here, in one sentence. Required, because a case
    /// nobody can explain is one nobody can decide the fate of when it
    /// starts failing.
    pub doc: String,
    /// Statements run before the one under test, against a database
    /// that is this case's alone.
    pub setup: Vec<String>,
    /// The parameters the statement under test is run with, in the
    /// order they were written. The setup statements take none: a
    /// parameter belongs to the case's one assertion, and a fixture
    /// that needed one would be a second statement under test wearing
    /// another name.
    pub params: Vec<(String, Value)>,
    pub query: String,
    pub expect: Expect,
    pub line: usize,
}

/// One file of cases.
#[derive(Debug, Clone)]
pub struct Suite {
    /// The file's own name, which every case name is prefixed with in
    /// a report so that two suites may both have a `max` case.
    pub name: String,
    pub doc: String,
    /// The data every case in the file reads back, put in through the
    /// runner's own bulk load path before the case runs. A suite of
    /// expressions has none, which is most of them.
    pub load: Option<Load>,
    pub cases: Vec<Case>,
}

impl Suite {
    /// A suite, or the first thing in the file that is not one.
    pub fn parse(text: &str) -> Result<Suite, String> {
        let doc = yaml::parse(text)?;
        if let Some(key) = doc
            .unknown(&["schema", "suite", "doc", "load", "cases"])
            .first()
        {
            return Err(format!("line {}: a suite has no key {key:?}", doc.line()));
        }
        let schema = doc
            .get("schema")
            .and_then(Node::str)
            .ok_or("the file does not open with `schema:`")?;
        let schema: i64 = schema
            .parse()
            .map_err(|_| format!("{schema:?} is not a schema version"))?;
        if schema != SCHEMA {
            return Err(format!(
                "this is schema {schema} and the runner reads schema {SCHEMA}"
            ));
        }
        let name = field(&doc, "suite")?;
        let doc_text = field(&doc, "doc")?;
        let load = doc.get("load").map(Load::parse).transpose()?;
        let cases = doc
            .get("cases")
            .ok_or("a suite with no `cases:`")?
            .seq()
            .ok_or("`cases:` is a sequence")?;
        if cases.is_empty() {
            return Err("a suite with no cases in it".to_string());
        }

        let mut out = Vec::with_capacity(cases.len());
        for node in cases {
            out.push(case(node)?);
        }
        // Names are what a report cites and what a binding's skip list
        // names, so two cases sharing one is a report that says less
        // than it looks like it does.
        let mut sorted: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        sorted.sort_unstable();
        if let Some(dup) = sorted.windows(2).find(|w| w[0] == w[1]) {
            return Err(format!("two cases are called {:?}", dup[0]));
        }
        Ok(Suite {
            name,
            doc: doc_text,
            load,
            cases: out,
        })
    }
}

fn field(node: &Node, key: &str) -> Result<String, String> {
    node.get(key)
        .ok_or(format!("line {}: no `{key}:`", node.line()))?
        .str()
        .map(str::to_string)
        .ok_or(format!(
            "line {}: `{key}:` is one line of text",
            node.line()
        ))
}

fn case(node: &Node) -> Result<Case, String> {
    let line = node.line();
    let at = |msg: String| format!("line {line}: {msg}");
    if node.map().is_none() {
        return Err(at(format!(
            "a case is a mapping, and this is {}",
            node.kind()
        )));
    }
    if let Some(key) = node
        .unknown(&[
            "name", "doc", "setup", "params", "query", "columns", "rows", "raises",
        ])
        .first()
    {
        return Err(at(format!("a case has no key {key:?}")));
    }
    let name = field(node, "name")?;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(at(format!(
            "{name:?} is a case name, which is lower case words joined by dashes"
        )));
    }
    let doc = field(node, "doc")?;
    let query = field(node, "query")?;
    let setup = match node.get("setup") {
        None => Vec::new(),
        Some(list) => list
            .seq()
            .ok_or(at("`setup:` is a sequence of statements".to_string()))?
            .iter()
            .map(|s| {
                s.str()
                    .map(str::to_string)
                    .ok_or(format!("line {}: a setup statement is one line", s.line()))
            })
            .collect::<Result<_, _>>()?,
    };

    let params = params(node)?;

    let expect = match (node.get("raises"), node.get("columns")) {
        (Some(_), Some(_)) => {
            return Err(at(
                "a case that raises has no rows, and one that returns rows does not raise"
                    .to_string(),
            ));
        }
        (Some(code), None) => {
            let code = code
                .str()
                .ok_or(at("`raises:` is a GQLSTATUS code".to_string()))?;
            if zu_common::GqlStatus::from_code(code).is_none() {
                return Err(format!(
                    "line {}: {code:?} is not a GQLSTATUS the standard defines",
                    node.get("raises").expect("just matched").line()
                ));
            }
            Expect::Raises(code.to_string())
        }
        (None, Some(columns)) => Expect::Rows {
            // A `columns:` with nothing under it is a result with no
            // columns, which is what `FINISH` answers and what a write
            // answers. A list somebody left unfinished reads as that
            // too and fails the case, since a statement that does
            // answer columns is not one that answers none.
            columns: columns
                .seq_or_empty()
                .ok_or(at("`columns:` is a sequence of names".to_string()))?
                .iter()
                .map(|c| {
                    c.str()
                        .map(str::to_string)
                        .ok_or(format!("line {}: a column name is one word", c.line()))
                })
                .collect::<Result<_, _>>()?,
            rows: rows(node)?,
        },
        (None, None) => {
            return Err(at(
                "a case says what it produces, with `columns:` and `rows:` or with `raises:`"
                    .to_string(),
            ));
        }
    };

    if let Expect::Rows { columns, rows } = &expect
        && let Some(row) = rows.iter().find(|r| r.len() != columns.len())
    {
        return Err(at(format!(
            "a row of {} against {} columns",
            row.len(),
            columns.len()
        )));
    }

    Ok(Case {
        name,
        doc,
        setup,
        params,
        query,
        expect,
        line,
    })
}

/// The parameters a case binds, which is the value encoding with a name
/// beside it.
///
/// A name is what the statement spells after the `$`, so it is checked
/// against what a statement may spell: a case whose name is `n one` is
/// one no client can bind, and finding that out here says so with a
/// line number rather than nine clients each failing their own way.
fn params(node: &Node) -> Result<Vec<(String, Value)>, String> {
    let Some(list) = node.get("params") else {
        return Ok(Vec::new());
    };
    let items = list
        .seq()
        .ok_or(format!("line {}: `params:` is a sequence", list.line()))?;
    let mut out: Vec<(String, Value)> = Vec::with_capacity(items.len());
    for item in items {
        let line = item.line();
        if item.map().is_none() {
            return Err(format!(
                "line {line}: a parameter is a mapping of `name`, `type` and `value`, and this is \
                 {}",
                item.kind()
            ));
        }
        if let Some(key) = item.unknown(&["name", "type", "value"]).first() {
            return Err(format!("line {line}: a parameter has no key {key:?}"));
        }
        let name = field(item, "name")?;
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "line {line}: {name:?} is a parameter name, which is what a statement writes after \
                 the `$`"
            ));
        }
        if out.iter().any(|(n, _)| *n == name) {
            return Err(format!("line {line}: two parameters are called {name:?}"));
        }
        out.push((name, value::typed(item)?));
    }
    Ok(out)
}

fn rows(node: &Node) -> Result<Vec<Vec<Value>>, String> {
    let Some(rows) = node.get("rows") else {
        // A statement that returns no rows is a case worth having, and
        // writing it as an absent `rows:` would make it the same shape
        // as one somebody forgot to finish.
        return Err(format!(
            "line {}: `columns:` with no `rows:`. A case expecting nothing back writes `rows:` \
             with an empty sequence under it.",
            node.line()
        ));
    };
    rows.seq_or_empty()
        .ok_or(format!("line {}: `rows:` is a sequence", rows.line()))?
        .iter()
        .map(|row| {
            row.get("values")
                .ok_or(format!(
                    "line {}: a row is `values:` and its values",
                    row.line()
                ))?
                .seq()
                .ok_or(format!("line {}: `values:` is a sequence", row.line()))?
                .iter()
                .map(value::decode)
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "schema: 3\nsuite: int\ndoc: the integer tower\n";

    fn suite(cases: &str) -> Result<Suite, String> {
        Suite::parse(&format!("{HEAD}\ncases:\n{cases}"))
    }

    fn one(cases: &str) -> Case {
        let suite = suite(cases).unwrap_or_else(|e| panic!("{cases:?} should parse: {e}"));
        suite.cases.into_iter().next().expect("one case")
    }

    #[test]
    fn a_case_is_a_statement_and_the_rows_it_owes() {
        let case = one(
            "  - name: int64-max\n    doc: the largest INT64\n    query: RETURN 9223372036854775807 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT64\n            value: \"9223372036854775807\"\n",
        );
        assert_eq!(case.name, "int64-max");
        assert_eq!(case.doc, "the largest INT64");
        assert_eq!(case.query, "RETURN 9223372036854775807 AS n");
        assert!(case.setup.is_empty());
        assert_eq!(
            case.expect,
            Expect::Rows {
                columns: vec!["n".to_string()],
                rows: vec![vec![Value::Int(i64::MAX)]],
            }
        );
    }

    #[test]
    fn a_case_may_load_its_own_data_first() {
        let case = one(
            "  - name: with-setup\n    doc: a case that needs a graph\n    setup:\n      - CREATE NODE TABLE Person(name STRING)\n      - INSERT (:Person {name: 'a'})\n    query: MATCH (p:Person) RETURN p.name AS name\n    columns:\n      - name\n    rows:\n      - values:\n          - type: STRING\n            value: a\n",
        );
        assert_eq!(case.setup.len(), 2);
        assert!(case.setup[0].starts_with("CREATE NODE TABLE"));
    }

    #[test]
    fn a_case_may_bind_parameters_and_they_keep_the_order_they_were_written_in() {
        let case = one(
            "  - name: bound\n    doc: a statement with two parameters in it\n    params:\n      - name: n\n        type: INT64\n        value: \"42\"\n      - name: s\n        type: STRING\n        value: ada\n    query: RETURN $n AS n, $s AS s\n    columns:\n      - n\n      - s\n    rows:\n      - values:\n          - type: INT64\n            value: \"42\"\n          - type: STRING\n            value: ada\n",
        );
        assert_eq!(
            case.params,
            vec![
                ("n".to_string(), Value::Int(42)),
                ("s".to_string(), Value::Str("ada".to_string())),
            ]
        );
    }

    #[test]
    fn a_parameter_is_a_value_of_the_same_encoding_and_is_read_the_same_way() {
        // A NULL parameter carries no `value`, the quoting rule is the
        // one every other value follows, and a list is a list. The
        // whole point of `params:` is that it is the row encoding with
        // a name added, so what is checked here is that it did not
        // become a second encoding.
        let case = one(
            "  - name: null-param\n    doc: a parameter that is nothing\n    params:\n      - name: n\n        type: NULL\n    query: RETURN $n AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: NULL\n",
        );
        assert_eq!(case.params, vec![("n".to_string(), Value::Null)]);
        let case = one(
            "  - name: list-param\n    doc: a parameter holding a list of two\n    params:\n      - name: xs\n        type: LIST\n        value:\n          - type: INT8\n            value: 1\n          - type: INT8\n            value: 2\n    query: RETURN size($xs) AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT64\n            value: \"2\"\n",
        );
        assert_eq!(
            case.params,
            vec![(
                "xs".to_string(),
                Value::List(vec![Value::Int(1), Value::Int(2)])
            )]
        );
        let err = suite(
            "  - name: a\n    doc: d\n    params:\n      - name: n\n        type: INT64\n        value: 42\n    query: RETURN $n\n    raises: 22012\n",
        )
        .expect_err("refused");
        assert!(err.contains("written in quotes"), "{err}");
    }

    #[test]
    fn a_parameter_a_statement_could_not_name_is_refused_where_it_is_written() {
        for (text, want) in [
            (
                "  - name: a\n    doc: d\n    params:\n      - type: INT8\n        value: 1\n    query: RETURN $n\n    raises: 22012\n",
                "no `name:`",
            ),
            (
                "  - name: a\n    doc: d\n    params:\n      - name: n one\n        type: INT8\n        value: 1\n    query: RETURN $n\n    raises: 22012\n",
                "is a parameter name",
            ),
            (
                "  - name: a\n    doc: d\n    params:\n      - name: n\n        type: INT8\n        value: 1\n      - name: n\n        type: INT8\n        value: 2\n    query: RETURN $n\n    raises: 22012\n",
                "two parameters are called \"n\"",
            ),
            (
                "  - name: a\n    doc: d\n    params:\n      - name: n\n        type: INT8\n        value: 1\n        note: hi\n    query: RETURN $n\n    raises: 22012\n",
                "a parameter has no key \"note\"",
            ),
            (
                "  - name: a\n    doc: d\n    params:\n      - $n\n    query: RETURN $n\n    raises: 22012\n",
                "a parameter is a mapping",
            ),
            (
                "  - name: a\n    doc: d\n    params: n\n    query: RETURN $n\n    raises: 22012\n",
                "`params:` is a sequence",
            ),
        ] {
            let err = suite(text).expect_err(&format!("{text:?} is refused"));
            assert!(err.contains(want), "{text:?} gave {err:?}");
        }
    }

    #[test]
    fn a_suite_may_load_a_table_every_case_in_it_reads_back() {
        let suite = Suite::parse(
            "schema: 3\nsuite: int\ndoc: d\nload:\n  nodes: person\n  edges: knows\n  count: 1\n  columns:\n    - name: age\n      type: INT64\n      values:\n        - \"30\"\ncases:\n  - name: a\n    doc: d\n    query: MATCH (p:person) RETURN p.age AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT64\n            value: \"30\"\n",
        )
        .expect("parses");
        let load = suite.load.expect("a load");
        assert_eq!(load.nodes, "person");
        assert_eq!(load.columns.len(), 1);
    }

    #[test]
    fn a_suite_of_expressions_has_no_load() {
        let suite = suite("  - name: a\n    doc: d\n    query: RETURN 1\n    raises: 22012\n")
            .expect("parses");
        assert!(suite.load.is_none());
    }

    #[test]
    fn a_case_may_expect_a_condition_instead_of_rows() {
        let case = one(
            "  - name: divide-by-zero\n    doc: division by zero raises rather than returning inf\n    query: RETURN 1 / 0\n    raises: 22012\n",
        );
        assert_eq!(case.expect, Expect::Raises("22012".to_string()));
    }

    #[test]
    fn a_case_expecting_nothing_back_says_so_out_loud() {
        let case = one(
            "  - name: empty\n    doc: a filter nothing satisfies gives no rows\n    query: UNWIND [1] AS n WHERE false RETURN n\n    columns:\n      - n\n    rows:\n",
        );
        assert!(matches!(case.expect, Expect::Rows { ref rows, .. } if rows.is_empty()));
    }

    #[test]
    fn a_suite_carries_the_line_of_every_case_so_a_failure_can_be_opened() {
        let suite = suite(
            "  - name: a\n    doc: the first\n    query: RETURN 1 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n  - name: b\n    doc: the second\n    query: RETURN 2 AS n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: INT8\n            value: 2\n",
        )
        .expect("parses");
        assert_eq!(suite.cases[0].line, 6);
        assert_eq!(suite.cases[1].line, 15);
    }

    #[test]
    fn a_file_the_runner_cannot_read_says_which_line_and_why() {
        for (text, want) in [
            ("  - name: a\n", "no `doc:`"),
            ("  - doc: a\n    query: RETURN 1\n", "no `name:`"),
            (
                "  - name: A\n    doc: d\n    query: RETURN 1\n    raises: 22012\n",
                "is a case name",
            ),
            (
                "  - name: a\n    doc: d\n    query: RETURN 1\n",
                "says what it produces",
            ),
            (
                "  - name: a\n    doc: d\n    query: RETURN 1\n    raises: 22012\n    columns:\n      - n\n    rows:\n",
                "has no rows",
            ),
            (
                "  - name: a\n    doc: d\n    query: RETURN 1\n    raises: 99999\n",
                "not a GQLSTATUS",
            ),
            (
                "  - name: a\n    doc: d\n    query: RETURN 1\n    columns:\n      - n\n",
                "with no `rows:`",
            ),
            (
                "  - name: a\n    doc: d\n    query: RETURN 1 AS n, 2 AS m\n    columns:\n      - n\n      - m\n    rows:\n      - values:\n          - type: INT8\n            value: 1\n",
                "a row of 1 against 2 columns",
            ),
            (
                "  - name: a\n    doc: d\n    qeury: RETURN 1\n    raises: 22012\n",
                "no key \"qeury\"",
            ),
        ] {
            let err = suite(text).expect_err(&format!("{text:?} is refused"));
            assert!(err.contains(want), "{text:?} gave {err:?}");
        }
    }

    #[test]
    fn the_same_case_name_twice_is_refused_because_a_report_cites_names() {
        let err = suite(
            "  - name: a\n    doc: d\n    query: RETURN 1\n    raises: 22012\n  - name: a\n    doc: e\n    query: RETURN 2\n    raises: 22012\n",
        )
        .expect_err("refused");
        assert!(err.contains("two cases are called \"a\""), "{err}");
    }

    #[test]
    fn a_file_from_another_schema_says_so_rather_than_failing_in_the_middle() {
        let err = Suite::parse("schema: 1\nsuite: int\ndoc: d\ncases:\n  - name: a\n")
            .expect_err("refused");
        assert!(
            err.contains("schema 1 and the runner reads schema 3"),
            "{err}"
        );
    }
}
