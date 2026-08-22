//! The table an `INSERT` names by a label or an edge type nothing
//! declared.
//!
//! An element is created in the node table whose own name is the label
//! the pattern wrote, so `INSERT (x:person {name: 'ada'})` needs a
//! table called person. GQL has no statement that makes one, and a
//! caller who wrote a label the graph has never seen means a table by
//! it the way they mean a table by one it has, so the graph gets one.
//!
//! An edge type is the same thing on the edge side: a rel table runs
//! between two node tables, and the pattern says which two, because
//! either end is written with a label or stands for something an
//! earlier clause matched by one. When neither says, there is no
//! answer to make a table out of and the statement is refused with the
//! ends it could not settle named.
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
use zu_query::ast::{
    Clause, Expr, LabelExpr, Literal, NodePattern, PathPattern, Query, RelDirection, RelPattern,
};

use crate::zu1::catalog::{Catalog, ElementKind};
use crate::zu1::file::Zu1File;
use crate::zu1::graph::create_empty_rel;
use crate::zu1::props::{PropInput, PropValues, store_props_for, store_rel_props_for};

/// A table one statement wants and the graph has not got: the label the
/// pattern wrote, and a column per property it wrote, in written order.
///
/// A column is carried as the values it would hold, none of them, which
/// is what a column of a table with no rows is and what says its type.
pub(crate) struct NewTable {
    pub(crate) name: String,
    pub(crate) columns: Vec<(String, PropValues<'static>)>,
}

/// A rel table one statement wants: the type the step wrote, the node
/// tables its ends are in, whether its edges point, and a column per
/// property the step wrote.
///
/// The ends are names rather than ids because one of them may be a node
/// table this same statement is making, which has no id until it is
/// made.
pub(crate) struct NewRel {
    pub(crate) name: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) undirected: bool,
    pub(crate) columns: Vec<(String, PropValues<'static>)>,
}

/// Everything one statement names that the graph has not got, in the
/// order it names it.
#[derive(Default)]
pub(crate) struct Wanted {
    pub(crate) nodes: Vec<NewTable>,
    pub(crate) rels: Vec<NewRel>,
}

impl Wanted {
    /// Whether the statement wants nothing, which is what says the
    /// failure to compile was about something else.
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.rels.is_empty()
    }
}

/// The tables the statement names and no table of `graph` is named by,
/// in the order it names them.
///
/// Nearly every statement names none. This is asked once, of a
/// statement that did not compile, so the walk costs nothing on the
/// path where the tables are all there.
pub(crate) fn wanted(catalog: &Catalog, graph: u32, parsed: &Query) -> Result<Wanted> {
    let mut wanted = Wanted::default();
    for clause in parsed.clauses() {
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
                if let Some(first) = wanted.nodes.iter().find(|t| t.name == *name) {
                    agree(first, node)?;
                    continue;
                }
                promised(catalog, graph, name)?;
                wanted.nodes.push(NewTable {
                    name: name.clone(),
                    columns: columns_of(name, node)?,
                });
            }
            wanted_rels(catalog, graph, parsed, pattern, &mut wanted)?;
        }
    }
    Ok(wanted)
}

/// The rel tables one written path wants, added to `wanted`.
///
/// A step is one edge, between the node the pattern reached and the one
/// it reaches next, so the ends are settled by the two node patterns
/// around it rather than by the step itself.
fn wanted_rels(
    catalog: &Catalog,
    graph: u32,
    parsed: &Query,
    pattern: &PathPattern,
    wanted: &mut Wanted,
) -> Result<()> {
    let mut left = &pattern.start;
    for (rel, right) in &pattern.steps {
        let [name] = rel.types.as_slice() else {
            // A step naming no type or several is one the binder turns
            // away by name, and it says it better than this could.
            left = right;
            continue;
        };
        if catalog.rel_in(graph, name).is_some() {
            left = right;
            continue;
        }
        let undirected = rel.direction == RelDirection::Undirected;
        let (from, to) = match rel.direction {
            RelDirection::In => (right, left),
            _ => (left, right),
        };
        let (from, to) = (
            end_table(parsed, name, from, "leaves")?,
            end_table(parsed, name, to, "arrives at")?,
        );
        // Two steps naming one new table in one statement are one
        // table, and one written twice with different ends or
        // different properties is two tables of one name.
        if let Some(first) = wanted.rels.iter().find(|t| t.name == *name) {
            agree_rel(first, &from, &to, undirected, rel)?;
        } else {
            promised_rel(catalog, graph, name)?;
            wanted.rels.push(NewRel {
                name: name.clone(),
                from,
                to,
                undirected,
                columns: rel_columns_of(name, rel)?,
            });
        }
        left = right;
    }
    Ok(())
}

/// The node table one end of a written edge is in.
///
/// An end written with a label is in the table that label names,
/// whether the table is there already or is one this statement is
/// making. An end that only stands for a variable is wherever the
/// clause that bound it left it, and the label written on it there is
/// what says so. Anything else leaves the table with an end nobody
/// named, and a rel table has to have both.
fn end_table(parsed: &Query, rel: &str, node: &NodePattern, side: &str) -> Result<String> {
    if let Some(LabelExpr::Label(name)) = &node.label {
        return Ok(name.clone());
    }
    if let Some(var) = &node.var
        && let Some(name) = labelled_elsewhere(parsed, var)
    {
        return Ok(name);
    }
    Err(ZuError::gql(
        codes::C42002,
        format!(
            "no edge table is named '{rel}', and the pattern does not say which table an edge of it {side}: {}",
            match &node.var {
                Some(var) => format!("nothing here gives '{var}' a label"),
                None => "that end is written with no label".to_string(),
            }
        ),
    ))
}

/// The label some other pattern of the same statement writes on `var`,
/// which is where a variable an earlier clause bound got its table.
///
/// The first one wins, and a variable written with two labels is not
/// this module's to settle: the binder narrows a reused variable to the
/// tables both occurrences allow, and it does that against a catalog
/// this has already made the table in.
fn labelled_elsewhere(parsed: &Query, var: &str) -> Option<String> {
    let patterns = parsed
        .clauses()
        .into_iter()
        .flat_map(|clause| match clause {
            Clause::Match { patterns, .. } | Clause::Insert { patterns } => patterns.as_slice(),
            _ => &[],
        });
    for pattern in patterns {
        let nodes = std::iter::once(&pattern.start).chain(pattern.steps.iter().map(|(_, n)| n));
        for node in nodes {
            if node.var.as_deref() == Some(var)
                && let Some(LabelExpr::Label(name)) = &node.label
            {
                return Some(name.clone());
            }
        }
    }
    None
}

/// Creates every table in the list, with its columns and no rows.
///
/// The catalog goes in first and the columns after it, because a column
/// is written against the table's id and the id is what the catalog
/// hands out. The node tables go in before the rel tables for the same
/// reason: a rel table runs between two ids.
pub(crate) fn create(db: &mut Zu1File, graph: u32, wanted: &Wanted) -> Result<()> {
    for table in &wanted.nodes {
        let mut catalog = Catalog::load(db)?;
        let id = catalog.create_node_in(graph, &table.name)?;
        catalog.store(db)?;
        store_props_for(db, id, 0, &inputs(&table.columns))?;
    }
    for rel in &wanted.rels {
        let mut catalog = Catalog::load(db)?;
        let end = |name: &str| -> Result<u32> {
            catalog.node_in(graph, name).map(|t| t.id).ok_or_else(|| {
                ZuError::gql(
                    codes::C42002,
                    format!(
                        "no edge table is named '{}', and an edge of it would run between elements of '{name}', which no node table is named by either",
                        rel.name
                    ),
                )
            })
        };
        let (from, to) = (end(&rel.from)?, end(&rel.to)?);
        let id = create_empty_rel(db, &mut catalog, &rel.name, from, to, rel.undirected)?;
        catalog.store(db)?;
        if !rel.columns.is_empty() {
            store_rel_props_for(db, id, 0, &inputs(&rel.columns))?;
        }
    }
    Ok(())
}

/// The columns of a table being made, as the store wants them: a dense
/// column of no values, which is what a column of a table with no rows
/// is.
fn inputs<'a>(columns: &'a [(String, PropValues<'static>)]) -> Vec<PropInput<'a>> {
    columns
        .iter()
        .map(|(name, values)| PropInput::dense(name, *values))
        .collect()
}

/// Whether the graph's type leaves room for a table of this name.
///
/// A graph created with a closed type has said what its elements look
/// like, and a table made out of a pattern is a shape nobody promised:
/// its rows would carry a label the type either never mentions, which
/// no element of the graph may carry, or mentions with properties of
/// its own that the pattern knows nothing about. Both are refused, and
/// the message says which one it is, because the answer to the second
/// is to make the table the type describes and the answer to the first
/// is that the graph is not for this.
///
/// A graph with no type or an open one promises nothing, which is every
/// graph a zu1 file has held until now, so this answers yes.
fn promised(catalog: &Catalog, graph: u32, name: &str) -> Result<()> {
    let Some(ty) = catalog.closed_type_of(graph) else {
        return Ok(());
    };
    let described = catalog.label_id(name).is_some_and(|id| {
        ty.types_for(ElementKind::Node, 1 << id)
            .iter()
            .any(|e| e.labels.contains(&id))
    });
    let why = if described {
        format!(
            "graph type '{}' describes a node labelled '{name}' and says what it holds, which is not what this pattern says",
            ty.name
        )
    } else {
        format!(
            "no element type of graph type '{}' describes a node labelled '{name}'",
            ty.name
        )
    };
    Err(ZuError::gql(
        codes::CG2000,
        format!("no node table is named '{name}' in this graph, and {why}"),
    ))
}

/// Whether the graph's type leaves room for a rel table of this name,
/// which is [`promised`] on the edge side and refuses for the same two
/// reasons.
fn promised_rel(catalog: &Catalog, graph: u32, name: &str) -> Result<()> {
    let Some(ty) = catalog.closed_type_of(graph) else {
        return Ok(());
    };
    let described = catalog.label_id(name).is_some_and(|id| {
        ty.types_for(ElementKind::Edge, 1 << id)
            .iter()
            .any(|e| e.labels.contains(&id))
    });
    let why = if described {
        format!(
            "graph type '{}' describes an edge labelled '{name}' and says what it holds, which is not what this pattern says",
            ty.name
        )
    } else {
        format!(
            "no element type of graph type '{}' describes an edge labelled '{name}'",
            ty.name
        )
    };
    Err(ZuError::gql(
        codes::CG2000,
        format!("no edge table is named '{name}' in this graph, and {why}"),
    ))
}

/// The columns of a rel table made for one step, on the same terms as a
/// node table's: one per property, in written order, typed by what was
/// written.
///
/// An edge with no properties is not the refusal a node with none is.
/// A row of a node table is a row of its columns and a table with none
/// has nowhere to put one, while an edge is a pair of ends the CSR
/// holds whether or not anything hangs off it, so a bare `-[:KNOWS]->`
/// makes a table that stores the edge and nothing about it.
fn rel_columns_of(name: &str, rel: &RelPattern) -> Result<Vec<(String, PropValues<'static>)>> {
    let mut columns: Vec<(String, PropValues<'static>)> = Vec::with_capacity(rel.props.len());
    for (key, value) in &rel.props {
        if columns.iter().any(|(had, _)| had == key) {
            return Err(ZuError::InvalidArgument(format!(
                "the edge carries '{key}' twice, and a table holds one column of that name"
            )));
        }
        columns.push((
            key.clone(),
            written(&format!("no edge table is named '{name}'"), key, value)?,
        ));
    }
    Ok(columns)
}

/// Whether a second step naming a rel table the first one is making
/// agrees with it about the ends, the direction, and the columns.
fn agree_rel(
    first: &NewRel,
    from: &str,
    to: &str,
    undirected: bool,
    rel: &RelPattern,
) -> Result<()> {
    let disagreement = if first.from != from || first.to != to {
        Some(format!(
            "one step writes it between '{}' and '{}' and another between '{from}' and '{to}'",
            first.from, first.to
        ))
    } else if first.undirected != undirected {
        Some("one step writes it with a direction and another without one".to_string())
    } else {
        let missing = first
            .columns
            .iter()
            .map(|(name, _)| name.as_str())
            .find(|name| !rel.props.iter().any(|(key, _)| key == name))
            .or_else(|| {
                rel.props
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .find(|key| !first.columns.iter().any(|(name, _)| name == key))
            });
        missing.map(|name| format!("one step carries '{name}' and another does not"))
    };
    match disagreement {
        None => Ok(()),
        Some(why) => Err(ZuError::InvalidArgument(format!(
            "'{}' is being made by this statement out of the steps that write it, and {why}",
            first.name
        ))),
    }
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
        columns.push((
            key.clone(),
            written(&format!("no node table is named '{name}'"), key, value)?,
        ));
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
fn written(what: &str, key: &str, value: &Expr) -> Result<PropValues<'static>> {
    let Expr::Literal(literal) = value else {
        return Err(ZuError::gql(
            codes::C42002,
            format!(
                "{what}, and the value of '{key}' is worked out rather than written, so it does not say what the column would hold"
            ),
        ));
    };
    Ok(match literal {
        Literal::Bool(_) => PropValues::Bool(&[]),
        Literal::Int(_) => PropValues::Int(&[]),
        Literal::Float(_) => PropValues::Float(&[]),
        Literal::Str(_) => PropValues::Str(&[]),
        Literal::Bytes(_) => PropValues::Bytes(&[]),
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
                    "{what}, and '{key}' is written as null, which does not say what the column would hold"
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
                    "{what}, and '{key}' is written with an offset from UTC, which is not something a column holds"
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

    /// The same line on the edge side: a type no rel table is named by
    /// is a table, between the tables the two ends are in, and the
    /// properties the step wrote are its columns.
    #[test]
    fn an_edge_type_that_names_no_table_makes_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "edge.zu1");

        let made = session
            .run(
                "MATCH (a:person {name: 'ada'}), (b:person {name: 'kay'})
                 INSERT (a)-[e:employs {since: 2020}]->(b)
                 RETURN e.since AS since",
                &[],
            )
            .expect("insert");
        assert_eq!(made.rows, vec![vec![Value::Int(2020)]]);

        let walked = session
            .run(
                "MATCH (a:person)-[e:employs]->(b:person) RETURN b.name AS name, e.since AS since",
                &[],
            )
            .expect("read");
        assert_eq!(strings(&walked, 0), ["kay"]);
        assert_eq!(walked.rows[0][1], Value::Int(2020));
    }

    /// An edge with no properties makes a table all the same, because
    /// the CSR holds the pair of ends whether or not anything hangs off
    /// it. This is where an edge differs from an element, which needs a
    /// column to be a row of.
    #[test]
    fn an_edge_with_no_properties_makes_a_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "bare-edge.zu1");

        session
            .run(
                "MATCH (a:person {name: 'ada'}), (b:person {name: 'kay'}) INSERT (a)-[:employs]->(b)",
                &[],
            )
            .expect("insert");

        let walked = session
            .run(
                "MATCH (:person)-[:employs]->(b:person) RETURN b.name AS name",
                &[],
            )
            .expect("read");
        assert_eq!(strings(&walked, 0), ["kay"]);
    }

    /// A table this statement is making is an end of an edge this
    /// statement is making, so both come out of the one pattern.
    #[test]
    fn an_edge_reaches_a_table_the_same_statement_makes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "both.zu1");

        session
            .run(
                "MATCH (a:person {name: 'ada'}) INSERT (a)-[:lives_in]->(c:city {name: 'york'})",
                &[],
            )
            .expect("insert");

        let walked = session
            .run(
                "MATCH (a:person)-[:lives_in]->(c:city) RETURN a.name AS who, c.name AS where",
                &[],
            )
            .expect("read");
        assert_eq!(strings(&walked, 0), ["ada"]);
        assert_eq!(strings(&walked, 1), ["york"]);
    }

    /// An end nothing gives a label to says nothing about which table
    /// the edge would run between, and a rel table has to have both
    /// ends, so the statement is refused rather than guessed at.
    #[test]
    fn an_edge_whose_end_has_no_label_makes_no_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "unlabelled.zu1");

        let err = session
            .run("MATCH (a), (b:person) INSERT (a)-[:employs]->(b)", &[])
            .expect_err("nothing says where the edge leaves from");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("42002"));
        assert!(
            err.to_string().contains("nothing here gives 'a' a label"),
            "the refusal names the end: {err}"
        );
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

        let gone = session
            .run("MATCH (c:city) RETURN c.name AS name", &[])
            .expect("the statement runs");
        assert!(
            gone.rows.is_empty(),
            "the table went with the statement that made it"
        );
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

        let gone = session
            .run("MATCH (c:city) RETURN c.name AS name", &[])
            .expect("the statement runs");
        assert!(
            gone.rows.is_empty(),
            "the table went with the statement that made it"
        );
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

        // The read answers a null per row, ISO 20.11, and it answers it
        // off the table that was already there: nothing here declared a
        // column for the name, and a statement that reads a property is
        // not one that writes one.
        let rows = session
            .run("MATCH (p:person) RETURN p.height AS h", &[])
            .expect("a read of a column nobody wrote");
        assert!(!rows.rows.is_empty(), "the person table has rows in it");
        assert!(
            rows.rows.iter().all(|row| row[0] == Value::Null),
            "every row reads null: {:?}",
            rows.rows
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
    /// A graph created with a closed type has said what its elements
    /// look like, so a table made out of a pattern is a shape nobody
    /// promised and the statement is refused rather than answered by
    /// widening the graph behind the type's back.
    #[test]
    fn a_table_is_not_made_in_a_graph_whose_type_says_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "typed.zu1");
        for stmt in [
            "CREATE PROPERTY GRAPH TYPE t { (:person {name :: STRING, age :: INT}) }",
            "CREATE GRAPH g TYPED t AS COPY OF home",
        ] {
            session.run(stmt, &[]).expect("the graph and its type");
        }

        let err = session
            .run("USE g INSERT (c:city {name: 'york'})", &[])
            .expect_err("the type does not describe a city");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("G2000"));
        assert!(
            err.to_string().contains("labelled 'city'"),
            "the refusal names the element: {err}"
        );

        // The graph is as it was: a refusal that had made the table
        // and then taken it back would leave the label dictionary
        // holding a name for it.
        assert!(session.catalog().label_id("city").is_none());
        // And the graph that has no type still takes one.
        session
            .run("INSERT (c:city {name: 'york'})", &[])
            .expect("the home graph promises nothing");
    }
}
