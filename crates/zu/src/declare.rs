//! The table an `INSERT` names by a label nothing declared.
//!
//! An element is created in the node table whose own name is the label
//! the pattern wrote, so `INSERT (x:person {name: 'ada'})` needs a
//! table called person. GQL has no statement that makes one, and a
//! caller who wrote a label the graph has never seen means a table by
//! it the way they mean a table by one it has, so the graph gets one.
//!
//! What the table holds comes from the pattern, because there is
//! nowhere else for it to come from: a column has a type, and the
//! statement is the only thing here that says anything about one. A
//! value written out says its own type, and a value that has to be
//! worked out first says nothing, since the plan that would work it out
//! is the plan this stands in front of. That is the refusal in this
//! module, and it names the property rather than the statement.
//!
//! This runs before the statement compiles and under the savepoint the
//! statement holds, so a table made for a statement that then raises is
//! a table that was never made.

use zu_common::gqlstatus::codes;
use zu_common::{Result, Temporal, ZuError};
use zu_query::ast::{Clause, Expr, LabelExpr, Literal, NodePattern, Query};

use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;
use crate::zu1::props::{PropInput, PropValues, store_props_for};

/// A table one statement wants and the graph has not got: the label the
/// pattern wrote, and a column per property it wrote, in written order.
///
/// A column is carried as the values it would hold, none of them, which
/// is what a column of a table with no rows is and what says its type.
pub(crate) struct NewTable {
    pub(crate) name: String,
    pub(crate) columns: Vec<(String, PropValues<'static>)>,
}

/// The tables the statement names by a label no node table of `graph`
/// has, in the order it names them.
///
/// Nearly every statement names none, and an empty answer is what says
/// the failure to compile was about something else. This is asked once,
/// of a statement that did not compile, so the walk costs nothing on
/// the path where the tables are all there.
pub(crate) fn wanted(catalog: &Catalog, graph: u32, parsed: &Query) -> Result<Vec<NewTable>> {
    let mut wanted: Vec<NewTable> = Vec::new();
    for clause in &parsed.clauses {
        let Clause::Insert { patterns } = clause else {
            continue;
        };
        for pattern in patterns {
            let nodes = std::iter::once(&pattern.start).chain(pattern.steps.iter().map(|(_, n)| n));
            for node in nodes {
                // A pattern that names no label, or names several, is
                // one the binder turns away by name, and what it should
                // say about that is not this module's to say.
                let Some(LabelExpr::Label(name)) = &node.label else {
                    continue;
                };
                if catalog.node_in(graph, name).is_some() {
                    continue;
                }
                // Two patterns naming one new table in one statement
                // are one table, so the second is checked against the
                // first rather than against a catalog neither is in.
                if let Some(first) = wanted.iter().find(|t| t.name == *name) {
                    agree(first, node)?;
                    continue;
                }
                wanted.push(NewTable {
                    name: name.clone(),
                    columns: columns_of(name, node)?,
                });
            }
        }
    }
    Ok(wanted)
}

/// Creates every table in the list, with its columns and no rows.
///
/// The catalog goes in first and the columns after it, because a column
/// is written against the table's id and the id is what the catalog
/// hands out.
pub(crate) fn create(db: &mut Zu1File, graph: u32, tables: &[NewTable]) -> Result<()> {
    for table in tables {
        let mut catalog = Catalog::load(db)?;
        let id = catalog.create_node_in(graph, &table.name)?;
        catalog.store(db)?;
        let columns: Vec<PropInput> = table
            .columns
            .iter()
            .map(|(name, values)| PropInput::dense(name, *values))
            .collect();
        store_props_for(db, id, 0, &columns)?;
    }
    Ok(())
}

/// The columns of a table made for one pattern: one per property, in
/// the order the pattern wrote them, typed by what it wrote.
fn columns_of(name: &str, node: &NodePattern) -> Result<Vec<(String, PropValues<'static>)>> {
    if node.props.is_empty() {
        return Err(ZuError::gql(
            codes::C42002,
            format!(
                "no node table is named '{name}', and the pattern that would make one carries no property, so the table would have no column for a row to grow"
            ),
        ));
    }
    let mut columns: Vec<(String, PropValues<'static>)> = Vec::with_capacity(node.props.len());
    for (key, value) in &node.props {
        if columns.iter().any(|(had, _)| had == key) {
            return Err(ZuError::InvalidArgument(format!(
                "the element carries '{key}' twice, and a table holds one column of that name"
            )));
        }
        columns.push((key.clone(), written(name, key, value)?));
    }
    Ok(columns)
}

/// The column one written property makes, as the values it would hold.
///
/// A literal says its own type. Anything else is worked out by the plan
/// that runs the statement, and this runs before there is one, so the
/// column would have to be guessed. A guessed column is worse than a
/// refusal: it is written down, and the next statement is typed against
/// it.
fn written(table: &str, key: &str, value: &Expr) -> Result<PropValues<'static>> {
    let Expr::Literal(literal) = value else {
        return Err(ZuError::gql(
            codes::C42002,
            format!(
                "no node table is named '{table}', and the value of '{key}' is worked out rather than written, so it does not say what the column would hold"
            ),
        ));
    };
    Ok(match literal {
        Literal::Bool(_) => PropValues::Bool(&[]),
        Literal::Int(_) => PropValues::Int(&[]),
        Literal::Float(_) => PropValues::Float(&[]),
        Literal::Str(_) => PropValues::Str(&[]),
        Literal::Temporal(Temporal::Date(_)) => PropValues::Date(&[]),
        Literal::Temporal(Temporal::LocalTime(_)) => PropValues::LocalTime(&[]),
        Literal::Temporal(Temporal::LocalDatetime(_)) => PropValues::LocalDatetime(&[]),
        Literal::Temporal(Temporal::Duration(kind, _)) => PropValues::Duration(*kind, &[]),
        // A null says the row holds nothing, which is a fact about the
        // row and not about the column, and a column of nulls is one
        // no INSERT can append to anyway.
        Literal::Null => {
            return Err(ZuError::gql(
                codes::C42002,
                format!(
                    "no node table is named '{table}', and '{key}' is written as null, which does not say what the column would hold"
                ),
            ));
        }
        // A value carrying an offset from UTC is not a column type
        // here, so a table made to hold one would be a table nothing
        // could then be inserted into.
        Literal::Temporal(Temporal::ZonedTime { .. } | Temporal::ZonedDatetime { .. }) => {
            return Err(ZuError::gql(
                codes::C42002,
                format!(
                    "no node table is named '{table}', and '{key}' is written with an offset from UTC, which is not something a column holds"
                ),
            ));
        }
    })
}

/// Whether a second pattern naming a table the first one is making
/// agrees with it about the columns.
///
/// Two patterns that disagree are one statement asking for two tables
/// of one name, and the one that would be made is whichever was written
/// first. That is not an answer to give quietly.
fn agree(first: &NewTable, node: &NodePattern) -> Result<()> {
    let missing = first
        .columns
        .iter()
        .map(|(name, _)| name.as_str())
        .find(|name| !node.props.iter().any(|(key, _)| key == name))
        .or_else(|| {
            node.props
                .iter()
                .map(|(key, _)| key.as_str())
                .find(|key| !first.columns.iter().any(|(name, _)| name == key))
        });
    match missing {
        None => Ok(()),
        Some(name) => Err(ZuError::InvalidArgument(format!(
            "'{}' is being made by this statement out of the properties written on it, and one pattern carries '{name}' while another does not",
            first.name
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::query::Value;
    use crate::session::Session;
    use crate::zu1::file::Zu1File;
    use crate::zu1::graph::bulk_load_as;
    use crate::zu1::props::{PropValues, store_props};

    /// Two people, the fixture the other write tests use, so a table
    /// this module makes lands beside one that was loaded.
    fn open(dir: &tempfile::TempDir, name: &str) -> Session {
        let path: std::path::PathBuf = dir.path().join(name);
        seeded(&path);
        Session::open(&path).expect("open")
    }

    fn seeded(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        bulk_load_as(&mut db, "person", "knows", 2, &[(0, 1)]).expect("load");
        let names: Vec<&[u8]> = vec![b"ada", b"kay"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .expect("props");
    }

    fn strings(result: &crate::query::QueryResult, col: usize) -> Vec<String> {
        result
            .rows
            .iter()
            .map(|row| match &row[col] {
                Value::Str(s) => s.clone(),
                other => panic!("expected a string, got {other:?}"),
            })
            .collect()
    }

    /// The line this module is about: a label no table carries is a
    /// table, made out of what the pattern wrote on it, and the next
    /// statement reads the row back out of it.
    #[test]
    fn a_label_that_names_no_table_makes_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "made.zu1");

        session
            .run("INSERT (c:city {name: 'york', founded: 71})", &[])
            .expect("insert");

        let after = session
            .run(
                "MATCH (c:city) RETURN c.name AS name, c.founded AS founded",
                &[],
            )
            .expect("read");
        assert_eq!(after.columns, ["name", "founded"]);
        assert_eq!(strings(&after, 0), ["york"]);
        assert_eq!(after.rows[0][1], Value::Int(71));
    }

    /// The table is in the file rather than in the session, so a
    /// second open finds it and can add to it.
    #[test]
    fn a_made_table_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reopen.zu1");
        seeded(&path);
        {
            let mut session = Session::open(&path).expect("open");
            session
                .run("INSERT (c:city {name: 'york', founded: 71})", &[])
                .expect("insert");
        }

        let mut session = Session::open(&path).expect("reopen");
        session
            .run("INSERT (c:city {name: 'bath', founded: 60})", &[])
            .expect("second insert");
        let after = session
            .run("MATCH (c:city) RETURN c.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["bath", "york"]);
    }

    /// The table is a write of the statement that wanted it, so a
    /// statement that raises after it leaves no table behind. The
    /// division is what raises, after the insert has run.
    #[test]
    fn a_table_is_undone_with_the_statement_that_made_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "undone.zu1");

        let err = session
            .run(
                "INSERT (c:city {name: 'york', founded: 71}) WITH c RETURN c.founded / 0 AS n",
                &[],
            )
            .expect_err("the division raises");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("22012"),
            "the statement failed where the test meant it to: {err}"
        );

        let err = session
            .run("MATCH (c:city) RETURN c.name AS name", &[])
            .expect_err("there is no such table");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("42002"));
    }

    /// A rollback takes the table the same way, because the savepoint
    /// the transaction holds is the one the creation happened under.
    #[test]
    fn a_rollback_takes_the_table_with_the_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "rollback.zu1");

        session.run("START TRANSACTION", &[]).expect("begin");
        session
            .run("INSERT (c:city {name: 'york', founded: 71})", &[])
            .expect("insert");
        session.run("ROLLBACK", &[]).expect("rollback");

        let err = session
            .run("MATCH (c:city) RETURN c.name AS name", &[])
            .expect_err("there is no such table");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("42002"));
    }

    /// A pattern with no properties says nothing about what the table
    /// would hold, and a table with no column is one no row can grow
    /// in, so it is refused rather than made.
    #[test]
    fn a_pattern_with_no_properties_makes_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "bare.zu1");

        let err = session
            .run("INSERT (c:city)", &[])
            .expect_err("nothing to make a column out of");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("42002"));
        assert!(
            err.to_string().contains("carries no property"),
            "the refusal says why: {err}"
        );
    }

    /// A value the plan would work out says nothing about the column
    /// either, and the plan that would work it out is the one this
    /// stands in front of.
    #[test]
    fn a_value_that_is_not_written_out_makes_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "computed.zu1");

        let err = session
            .run("INSERT (c:city {name: 'york', founded: 70 + 1})", &[])
            .expect_err("the value has to be worked out");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("42002"));
        assert!(
            err.to_string().contains("worked out rather than written"),
            "the refusal names the property: {err}"
        );
    }

    /// A null is a fact about the row rather than about the column.
    #[test]
    fn a_null_makes_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "null.zu1");

        let err = session
            .run("INSERT (c:city {name: 'york', founded: NULL})", &[])
            .expect_err("a null says no type");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("42002"));
        assert!(err.to_string().contains("written as null"), "{err}");
    }

    /// A label on a table that is already there is not this module's
    /// business, and the statement that names one is answered by the
    /// binder the way it always was.
    #[test]
    fn a_label_a_table_already_has_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "existing.zu1");

        let err = session
            .run("MATCH (p:person) RETURN p.height AS h", &[])
            .expect_err("no such column");
        assert!(
            err.to_string().contains("unknown property 'height'"),
            "the binder answered rather than this module: {err}"
        );
    }

    /// Two patterns naming one new table make one table, and both rows
    /// land in it.
    #[test]
    fn two_patterns_naming_one_new_table_make_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "twice.zu1");

        session
            .run(
                "INSERT (a:city {name: 'york', founded: 71}), (b:city {name: 'bath', founded: 60})",
                &[],
            )
            .expect("insert");

        let after = session
            .run("MATCH (c:city) RETURN c.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["bath", "york"]);
    }
}
