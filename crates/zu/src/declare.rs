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
//! What the table holds comes from whichever of two things says it. A
//! graph whose type is closed has declared what an element of this
//! label holds, and that declaration is the better answer: it says
//! `BINARY(16)` where a written value can only say that a run of bytes
//! was written, it says it for the properties whose values the plan has
//! yet to work out, and it says it in the order the type wrote them.
//!
//! A graph that promised nothing leaves the pattern as the only thing
//! here that says anything about a type, and then a value written out
//! says its own and a value that has to be worked out first says
//! nothing, since the plan that would work it out is the plan this
//! stands in front of. That is the refusal in this module, and it names
//! the property rather than the statement.
//!
//! This runs before the statement compiles and under the savepoint the
//! statement holds, so a table made for a statement that then raises is
//! a table that was never made.

use zu_common::gqlstatus::{Subject, codes};
use zu_common::{FloatBits, IntBits, LogicalType, Result, Temporal, ZuError};
use zu_query::ast::{
    Clause, Expr, LabelExpr, Literal, NodePattern, PathPattern, Query, RelDirection, RelPattern,
};

use crate::zu1::catalog::{Catalog, ElementKind, ElementType};
use crate::zu1::file::Zu1File;
use crate::zu1::graph::create_empty_rel;
use crate::zu1::props::{PropInput, PropValues, storable, store_props_for, store_rel_props_for};

/// A table one statement wants and the graph has not got: the label the
/// pattern wrote, and a column per property, in the order whatever said
/// what the table holds wrote them.
///
/// A column is carried as its name and its type, which is the whole of
/// what a column of a table with no rows is.
pub(crate) struct NewTable {
    pub(crate) name: String,
    pub(crate) columns: Vec<(String, LogicalType)>,
    /// Whether the columns came out of a graph type rather than out of
    /// the pattern, which is what says a second pattern naming this
    /// table has nothing to agree with the first one about.
    pub(crate) declared: bool,
}

/// A rel table one statement wants: the type the step wrote, the node
/// tables its ends are in, whether its edges point, and a column per
/// property.
///
/// The ends are names rather than ids because one of them may be a node
/// table this same statement is making, which has no id until it is
/// made.
pub(crate) struct NewRel {
    pub(crate) name: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) undirected: bool,
    pub(crate) columns: Vec<(String, LogicalType)>,
    pub(crate) declared: bool,
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
                let shape = promised(catalog, graph, ElementKind::Node, name, "node")?;
                wanted.nodes.push(NewTable {
                    name: name.clone(),
                    columns: columns_of(name, node, &shape)?,
                    declared: matches!(shape, Shape::Declared(_)),
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
        // A step naming the types it does not walk names no table to
        // make, and it is turned away by name as well.
        if rel.negated {
            left = right;
            continue;
        }
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
            let shape = promised(catalog, graph, ElementKind::Edge, name, "edge")?;
            wanted.rels.push(NewRel {
                name: name.clone(),
                from,
                to,
                undirected,
                columns: rel_columns_of(name, rel, &shape)?,
                declared: matches!(shape, Shape::Declared(_)),
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
    )
    .about(Subject::Label(rel.to_string())))
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
                .about(Subject::Label(name.to_string()))
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
/// is, and beside it the type the column is declared.
///
/// The type is passed even where it is exactly what the values imply,
/// because saying it twice costs nothing and leaving it out would make
/// this the one caller whose columns mean something different from
/// what they say. Where it is narrower than the values imply, which is
/// every column a graph type declared, it is the only thing that says
/// the column is `BINARY(16)` and not `BYTES`.
fn inputs<'a>(columns: &'a [(String, LogicalType)]) -> Vec<PropInput<'a>> {
    columns
        .iter()
        .map(|(name, ty)| {
            let values = PropValues::none_of(ty).expect("the column type was checked as storable");
            PropInput::typed(name, values, ty)
        })
        .collect()
}

/// What a table being made is made out of.
enum Shape<'c> {
    /// Nothing promised anything about this label, so the pattern is
    /// the only thing that says what the table holds.
    Written,
    /// The graph's type describes an element of this label and says
    /// what one holds, so the table is that declaration.
    Declared(&'c ElementType),
}

/// What the graph's type says about a table of this name, or the
/// refusal that it says the graph is not for this.
///
/// A graph created with a closed type has said what its elements look
/// like. A label the type never mentions is one no element of the graph
/// may carry, so a table for it is refused: widening the graph behind
/// the type's back is not an answer to give quietly. A label it does
/// mention is the opposite case, and the table is made out of what the
/// type declared, which is where a column gets a type no written value
/// could have given it.
///
/// Two element types over one label are GG24, and which of them a table
/// would hold is not a question the pattern answers, so that is refused
/// too and the message says how many there were.
///
/// A graph with no type or an open one promises nothing, which is every
/// graph a zu1 file has held until now, and then the pattern is the
/// whole of it.
fn promised<'c>(
    catalog: &'c Catalog,
    graph: u32,
    kind: ElementKind,
    name: &str,
    noun: &str,
) -> Result<Shape<'c>> {
    let Some(ty) = catalog.closed_type_of(graph) else {
        return Ok(Shape::Written);
    };
    let described: Vec<&ElementType> = catalog
        .label_id(name)
        .map(|id| {
            ty.types_for(kind, 1 << id)
                .into_iter()
                .filter(|e| e.labels.contains(&id))
                .collect()
        })
        .unwrap_or_default();
    let why = match described.as_slice() {
        [one] => return Ok(Shape::Declared(one)),
        [] => format!(
            "no element type of graph type '{}' describes {} {noun} labelled '{name}'",
            ty.name,
            match noun {
                "edge" => "an",
                _ => "a",
            }
        ),
        many => format!(
            "graph type '{}' describes {} {noun} types labelled '{name}', and nothing here says which of them a table would hold",
            ty.name,
            many.len()
        ),
    };
    Err(ZuError::gql(
        codes::CG2000,
        format!("no {noun} table is named '{name}' in this graph, and {why}"),
    ))
}

/// The columns a graph type's element type says a table holds: one per
/// declared property, in declared order, typed by the declaration.
///
/// An element type that is open permits properties it never declared,
/// so what the pattern wrote and the type did not say is a column too,
/// typed by what was written and standing behind the declared ones. A
/// closed element type declares the whole of what an element holds, and
/// a pattern that writes more than that is refused by the insert with
/// the property named, ISO 24.5.2 item IL002, once the table it is
/// being measured against exists.
fn declared_columns(
    what: &str,
    element: &ElementType,
    props: &[(String, Expr)],
) -> Result<Vec<(String, LogicalType)>> {
    let mut columns = Vec::with_capacity(element.properties.len());
    for prop in &element.properties {
        // A graph type may name a type no column holds, since naming
        // one is a promise about values and holding one is a layout.
        // Refused here, with the property named, rather than left for
        // the store to refuse halfway through making the table.
        if !storable(&prop.ty) {
            return Err(ZuError::gql(
                codes::C42002,
                format!(
                    "{what}, and the graph type declares '{}' as {}, which is not a type a column holds",
                    prop.name, prop.ty
                ),
            )
            .about(Subject::Property(prop.name.clone())));
        }
        columns.push((prop.name.clone(), prop.ty.clone()));
    }
    if element.open {
        for (key, value) in props {
            if element.property(key).is_none() {
                columns.push((key.clone(), written(what, key, value)?));
            }
        }
    }
    Ok(columns)
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
fn rel_columns_of(
    name: &str,
    rel: &RelPattern,
    shape: &Shape<'_>,
) -> Result<Vec<(String, LogicalType)>> {
    let what = format!("no edge table is named '{name}'");
    twice("edge", &rel.props)?;
    if let Shape::Declared(element) = shape {
        return declared_columns(&what, element, &rel.props);
    }
    rel.props
        .iter()
        .map(|(key, value)| Ok((key.clone(), written(&what, key, value)?)))
        .collect()
}

/// Refuses a pattern that writes one property twice, whatever the table
/// is going to be made out of, since a table holds one column of a
/// name and the second write has nowhere to go.
fn twice(noun: &str, props: &[(String, Expr)]) -> Result<()> {
    for (i, (key, _)) in props.iter().enumerate() {
        if props[..i].iter().any(|(had, _)| had == key) {
            return Err(ZuError::InvalidArgument(format!(
                "the {noun} carries '{key}' twice, and a table holds one column of that name"
            )));
        }
    }
    Ok(())
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
    } else if first.declared {
        // A table the graph type declared is not made out of the steps
        // that write it, so two steps writing different properties onto
        // it are two edges of one table and not two tables.
        None
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
/// the order the pattern wrote them, typed by what it wrote, or the
/// columns the graph type declared where one did.
fn columns_of(
    name: &str,
    node: &NodePattern,
    shape: &Shape<'_>,
) -> Result<Vec<(String, LogicalType)>> {
    let what = format!("no node table is named '{name}'");
    twice("element", &node.props)?;
    if let Shape::Declared(element) = shape {
        return declared_columns(&what, element, &node.props);
    }
    if node.props.is_empty() {
        return Err(ZuError::gql(
            codes::C42002,
            format!(
                "{what}, and the pattern that would make one carries no property, so the table would have no column for a row to grow"
            ),
        )
        .about(Subject::Label(name.to_string())));
    }
    node.props
        .iter()
        .map(|(key, value)| Ok((key.clone(), written(&what, key, value)?)))
        .collect()
}

/// The type of the column one written property makes.
///
/// A literal says its own type. Anything else is worked out by the plan
/// that runs the statement, and this runs before there is one, so the
/// column would have to be guessed. A guessed column is worse than a
/// refusal: it is written down, and the next statement is typed against
/// it.
///
/// What comes back is the widest type of the value's family, because a
/// value is one value and a column is every row a table will ever hold:
/// a written `71` says the column holds whole numbers and not that it
/// holds numbers under a hundred. A declaration is where a narrower
/// column comes from, and that is [`declared_columns`].
fn written(what: &str, key: &str, value: &Expr) -> Result<LogicalType> {
    let Expr::Literal(literal) = value else {
        return Err(ZuError::gql(
            codes::C42002,
            format!(
                "{what}, and the value of '{key}' is worked out rather than written, so it does not say what the column would hold"
            ),
        )
        .about(Subject::Property(key.to_string())));
    };
    Ok(match literal {
        Literal::Bool(_) => LogicalType::Bool,
        Literal::Int(_) => LogicalType::Int {
            signed: true,
            bits: IntBits::B64,
            precision: None,
        },
        Literal::Float(_) => LogicalType::Float {
            bits: FloatBits::B64,
            precision: None,
        },
        Literal::Str(_) => LogicalType::string(),
        Literal::Bytes(_) => LogicalType::bytes(),
        Literal::Temporal(Temporal::Date(_)) => LogicalType::Date,
        Literal::Temporal(Temporal::LocalTime(_)) => LogicalType::LocalTime,
        Literal::Temporal(Temporal::LocalDatetime(_)) => LogicalType::LocalDatetime,
        Literal::Temporal(Temporal::Duration(kind, _)) => LogicalType::Duration(*kind),
        // A null says the row holds nothing, which is a fact about the
        // row and not about the column, and a column of nulls is one
        // no INSERT can append to anyway.
        Literal::Null => {
            return Err(ZuError::gql(
                codes::C42002,
                format!(
                    "{what}, and '{key}' is written as null, which does not say what the column would hold"
                ),
            )
            .about(Subject::Property(key.to_string())));
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
            )
            .about(Subject::Property(key.to_string())));
        }
    })
}

/// Whether a second pattern naming a table the first one is making
/// agrees with it about the columns.
///
/// Two patterns that disagree are one statement asking for two tables
/// of one name, and the one that would be made is whichever was written
/// first. That is not an answer to give quietly.
///
/// A table the graph type declared is not made out of the patterns at
/// all, so there is nothing for them to disagree about: both are
/// measured against the declaration by the insert instead.
fn agree(first: &NewTable, node: &NodePattern) -> Result<()> {
    if first.declared {
        return Ok(());
    }
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
    use crate::zu1::catalog::ROOT_SCHEMA;
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

    /// Every column of a table of graph `g`, as its name and the type
    /// the store holds it under, which is what says whether a
    /// declaration reached the file or stopped at the catalog.
    fn declared_columns_of(path: &Path, table: &str) -> Vec<(String, String)> {
        let mut db = Zu1File::open(path).expect("open");
        let catalog = crate::zu1::catalog::Catalog::load(&mut db).expect("catalog");
        let graph = catalog.graph(ROOT_SCHEMA, "g").expect("the graph").id;
        let id = catalog.node_in(graph, table).expect("the table").id;
        let dir = crate::zu1::props::load_props(&mut db, id)
            .expect("props")
            .expect("the table stores columns");
        dir.columns
            .iter()
            .map(|col| (col.name.clone(), col.ty.to_string()))
            .collect()
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
                "MATCH (a:person)-[:lives_in]->(c:city) RETURN a.name AS who, c.name AS place",
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

    /// The other half of the same rule. A graph type that does
    /// describe the label says what an element of it holds, and that
    /// is what the table is made out of: `id` is the declared
    /// `BINARY(16)` and not the `BYTES` a written value can say for
    /// itself, which is a column type no statement could reach before.
    #[test]
    fn a_table_the_graph_type_describes_is_made_out_of_the_type() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("declared.zu1");
        seeded(&path);
        {
            let mut session = Session::open(&path).expect("open");
            for stmt in [
                "CREATE PROPERTY GRAPH TYPE t { (:doc {id :: BINARY(16), title :: STRING}) }",
                "CREATE GRAPH g TYPED t",
            ] {
                session.run(stmt, &[]).expect("the graph and its type");
            }
            session
                .run(
                    "USE g INSERT (d:doc {id: X'000102030405060708090A0B0C0D0E0F', title: 'ada'})",
                    &[],
                )
                .expect("insert");

            let back = session
                .run("USE g MATCH (d:doc) RETURN d.title AS title", &[])
                .expect("read");
            assert_eq!(strings(&back, 0), ["ada"]);
        }

        assert_eq!(
            declared_columns_of(&path, "doc"),
            [
                ("id".to_string(), "BYTES(16) FIXED".to_string()),
                ("title".to_string(), "STRING".to_string()),
            ],
            "the column carries the type the graph type declared"
        );
    }

    /// And the declaration is a check rather than a label on the
    /// column: an id of the wrong octet count is refused, because a
    /// column declared `BINARY(16)` whose rows are not all sixteen
    /// octets is a column whose own type is a lie.
    #[test]
    fn a_value_the_declared_type_does_not_admit_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("narrow.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        for stmt in [
            "CREATE PROPERTY GRAPH TYPE t { (:doc {id :: BINARY(16), title :: STRING}) }",
            "CREATE GRAPH g TYPED t",
        ] {
            session.run(stmt, &[]).expect("the graph and its type");
        }

        let err = session
            .run("USE g INSERT (d:doc {id: X'0001', title: 'ada'})", &[])
            .expect_err("two octets are not sixteen");
        assert!(
            err.to_string().contains("octets"),
            "the refusal says what was wrong with it: {err}"
        );

        // And the table the refused statement would have made is not
        // there, so the next statement is the first one again.
        session
            .run(
                "USE g INSERT (d:doc {id: X'000102030405060708090A0B0C0D0E0F', title: 'ada'})",
                &[],
            )
            .expect("the right width goes in");
    }

    /// A property whose value the plan has yet to work out says
    /// nothing about a column, and where a graph type declared the
    /// table nothing needs it to: the column has a type already.
    #[test]
    fn a_declared_table_takes_a_value_that_is_worked_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("computed.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        for stmt in [
            "CREATE PROPERTY GRAPH TYPE t { (:city {name :: STRING, founded :: INT}) }",
            "CREATE GRAPH g TYPED t",
        ] {
            session.run(stmt, &[]).expect("the graph and its type");
        }

        session
            .run("USE g INSERT (c:city {name: 'york', founded: 70 + 1})", &[])
            .expect("the type says what the column holds");

        let back = session
            .run("USE g MATCH (c:city) RETURN c.founded AS n", &[])
            .expect("read");
        assert_eq!(back.rows[0][0], Value::Int(71));
    }

    /// A graph type may name a type no column holds, since naming one
    /// is a promise about values and holding one is a layout. The
    /// statement that would need the column is where that is refused,
    /// and the refusal names the property.
    #[test]
    fn a_declared_type_no_column_holds_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("unstorable.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        for stmt in [
            "CREATE PROPERTY GRAPH TYPE t { (:doc {tags :: LIST<STRING>[4], title :: STRING}) }",
            "CREATE GRAPH g TYPED t",
        ] {
            session.run(stmt, &[]).expect("the graph and its type");
        }

        let err = session
            .run("USE g INSERT (d:doc {tags: 'a', title: 'ada'})", &[])
            .expect_err("no column holds a bounded list of strings");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("42002"));
        assert!(
            err.to_string().contains("'tags'"),
            "the refusal names the property: {err}"
        );
    }

    /// A type a column holds is not yet a type a statement can write
    /// into one. A bounded list of a fixed width element is the column
    /// the embedding work of #747, #749 and #754 was for, and the store
    /// holds it; what `INSERT` has no way to say is the value. So the
    /// statement is refused and the table it would have made goes with
    /// it, which is where this stops until the write path carries a
    /// list.
    #[test]
    fn a_declared_list_column_is_not_yet_something_a_statement_can_fill() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("embedding.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        for stmt in [
            "CREATE PROPERTY GRAPH TYPE t { (:doc {v :: LIST<FLOAT32 NOT NULL>[3]}) }",
            "CREATE GRAPH g TYPED t",
        ] {
            session.run(stmt, &[]).expect("the graph and its type");
        }

        let err = session
            .run("USE g INSERT (d:doc {v: [1.0, 2.0, 3.0]})", &[])
            .expect_err("the write path carries no list yet");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22G03"));
        // The label is in the dictionary because the graph type named
        // it, and the table is not, because the statement that would
        // have made it raised and took it with it.
        let catalog = session.catalog();
        let graph = catalog.graph(ROOT_SCHEMA, "g").expect("the graph").id;
        assert!(catalog.node_in(graph, "doc").is_none());
    }

    /// A zoned column is a column the store holds and a statement
    /// cannot fill, for the reason a list column is: the cell between
    /// the two is one word, and a zoned value is an instant and the
    /// offset it was written with, so a word would keep the first and
    /// drop the second. That is a wrong answer rather than a missing
    /// one, which is why the refusal is here and not a zero offset.
    #[test]
    fn a_declared_zoned_column_is_not_yet_something_a_statement_can_fill() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("zoned.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        for stmt in [
            "CREATE PROPERTY GRAPH TYPE t { (:event {at :: ZONED DATETIME}) }",
            "CREATE GRAPH g TYPED t",
        ] {
            session.run(stmt, &[]).expect("the graph and its type");
        }

        let err = session
            .run(
                "USE g INSERT (e:event {at: ZONED_DATETIME('2024-01-15T10:00:00+07:00')})",
                &[],
            )
            .expect_err("the write path carries no zone yet");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22G03"));
        assert!(err.to_string().contains("zone"), "{err}");
        let catalog = session.catalog();
        let graph = catalog.graph(ROOT_SCHEMA, "g").expect("the graph").id;
        assert!(catalog.node_in(graph, "event").is_none());
    }

    /// A decimal column is the first one whose declaration is part of
    /// reading it back. The lane holds a whole number of units and the
    /// scale in the declared type says how large a unit is, so a column
    /// of `DECIMAL(12,2)` is a column of pence and the type is what
    /// turns a hundred and twenty of them into 1.20.
    ///
    /// The scale binds on the way in as well as on the way out. A value
    /// the column cannot hold exactly is refused rather than rounded:
    /// rounding a price on the way into a ledger is the mistake this
    /// type exists to stop, and a caller who wants it rounded has
    /// `ROUND` to ask with.
    #[test]
    fn a_declared_decimal_column_keeps_its_scale_in_and_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decimal.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        for stmt in [
            "CREATE PROPERTY GRAPH TYPE t { (:purchase {total :: DECIMAL(12,2)}) }",
            "CREATE GRAPH g TYPED t",
            // One at the column's own scale, one coarser, and a plain
            // integer. All three are exact at two places, so all three
            // are values this column holds.
            "USE g INSERT (p:purchase {total: CAST('1.20' AS DECIMAL(5,2))})",
            "USE g INSERT (p:purchase {total: CAST('0.5' AS DECIMAL(5,1))})",
            "USE g INSERT (p:purchase {total: 7})",
        ] {
            session
                .run(stmt, &[])
                .expect("the graph, its type, its rows");
        }

        // The column stores 120, 50 and 700 units and the declared scale
        // is what turns them back into the numbers written. The second
        // place on 0.50 is the column's rather than the literal's, which
        // is the whole point of keeping the scale in the catalog.
        let rows = session
            .run(
                "USE g MATCH (p:purchase) RETURN p.total AS total ORDER BY total",
                &[],
            )
            .expect("the rows read back");
        let read: Vec<String> = rows
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Decimal(d) => d.to_string(),
                other => panic!("expected an exact decimal, got {other:?}"),
            })
            .collect();
        assert_eq!(read, ["0.50", "1.20", "7.00"]);

        // A third place is not something this column holds, and 22003 is
        // the condition for a value outside a numeric type's range.
        let err = session
            .run(
                "USE g INSERT (p:purchase {total: CAST('1.234' AS DECIMAL(6,3))})",
                &[],
            )
            .expect_err("the column has two places and this has three");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22003"));

        // So is a number wider than the declared precision, which is the
        // other half of what `DECIMAL(12,2)` promised: eleven digits and
        // two places is thirteen, and twelve is the whole of it.
        let err = session
            .run("USE g INSERT (p:purchase {total: 12345678901})", &[])
            .expect_err("thirteen digits do not fit twelve");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22003"));
    }

    /// A decimal wider than a lane word is the same column on the other
    /// plane, and the declaration still says what a unit is.
    ///
    /// This is the first type here stored two ways. `DECIMAL(12,2)`
    /// above is a lane word of unscaled units and `DECIMAL(30,4)` is
    /// sixteen bytes of them, and nothing about the declaration says
    /// which: the precision does, through `props::fixed_octets`, which
    /// is the one place that decides so the writer and the reader cannot
    /// disagree. A caller writing the statement sees one type.
    ///
    /// What the wide plane buys is the range an `i128` has, which is
    /// where a decimal now stops, because that is the carrier a value of
    /// one arrives in. Thirty eight digits is the widest a declaration
    /// may ask for and the widest a value can be, so the two ends meet.
    #[test]
    fn a_declared_wide_decimal_keeps_its_scale_on_the_other_plane() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wide-decimal.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        // Twenty six digits and four places, which is a number of units
        // no lane word holds: the units alone are thirty digits.
        let units = "1234567890123456789012345678.9012";
        for stmt in [
            "CREATE PROPERTY GRAPH TYPE t { (:tally {n :: DECIMAL(32,4)}) }".to_string(),
            "CREATE GRAPH g TYPED t".to_string(),
            format!("USE g INSERT (t:tally {{n: CAST('{units}' AS DECIMAL(32,4))}})"),
            // At the column's scale, coarser than it, and a plain
            // integer. All three are exact at four places.
            "USE g INSERT (t:tally {n: CAST('0.5' AS DECIMAL(5,1))})".to_string(),
            "USE g INSERT (t:tally {n: 7})".to_string(),
        ] {
            session
                .run(&stmt, &[])
                .expect("the graph, its type, its rows");
        }

        // The scale comes from the column and not from the literal, the
        // same way it does on the lane, so 0.5 reads back at four
        // places. That is the declaration doing the work on a plane
        // where the value is sixteen bytes rather than a word.
        let rows = session
            .run("USE g MATCH (t:tally) RETURN t.n AS n ORDER BY n", &[])
            .expect("the rows read back");
        let read: Vec<String> = rows
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Decimal(d) => d.to_string(),
                other => panic!("expected an exact decimal, got {other:?}"),
            })
            .collect();
        assert_eq!(read, ["0.5000", "7.0000", units]);

        // The declared precision still binds, and it binds on the wide
        // plane the way it does on the narrow one: thirty three digits
        // is not a value of a column declared with thirty two.
        let err = session
            .run(
                "USE g INSERT (t:tally {n: CAST('12345678901234567890123456789.0123' AS DECIMAL(33,4))})",
                &[],
            )
            .expect_err("thirty three digits do not fit thirty two");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22003"));

        // And a fifth place is a place this column has no room for,
        // which is the refusal the narrow decimal already made.
        let err = session
            .run(
                "USE g INSERT (t:tally {n: CAST('1.00001' AS DECIMAL(6,5))})",
                &[],
            )
            .expect_err("the column has four places and this has five");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22003"));
    }

    /// An `INT128` column holds numbers no lane word could, and hands
    /// them back whole.
    ///
    /// This is the first column that is a number and is not stored in
    /// the scalar lane. Sixteen little endian bytes a row at a fixed
    /// stride is the layout `BINARY(16)` already had, so the storage
    /// side asked for nothing new; what is new is that the bytes are
    /// read as a number rather than as a run of octets.
    ///
    /// A value comes back as an exact numeric of scale nought, which is
    /// what a whole number is and what the engine has a carrier for. It
    /// prints without a point and compares equal to the integer of the
    /// same size, so a caller who wrote 7 reads 7.
    #[test]
    fn a_declared_int128_column_holds_a_number_wider_than_a_lane() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wide.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        // The largest and smallest values thirty eight digits can spell,
        // which is the widest a declaration may ask a decimal for, and a
        // small one to show the ordinary case still reads plainly.
        let big = "99999999999999999999999999999999999999";
        for stmt in [
            "CREATE PROPERTY GRAPH TYPE t { (:ledger {n :: INT128}) }".to_string(),
            "CREATE GRAPH g TYPED t".to_string(),
            "USE g INSERT (l:ledger {n: 7})".to_string(),
            format!("USE g INSERT (l:ledger {{n: CAST('{big}' AS DECIMAL(38,0))}})"),
            format!("USE g INSERT (l:ledger {{n: CAST('-{big}' AS DECIMAL(38,0))}})"),
        ] {
            session
                .run(&stmt, &[])
                .expect("the graph, its type, its rows");
        }

        let rows = session
            .run("USE g MATCH (l:ledger) RETURN l.n AS n ORDER BY n", &[])
            .expect("the rows read back");
        let read: Vec<String> = rows
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Decimal(d) => d.to_string(),
                other => panic!("expected an exact numeric, got {other:?}"),
            })
            .collect();
        assert_eq!(read, [format!("-{big}"), "7".to_string(), big.to_string()]);

        // A number with a fraction is not a value of an integer column,
        // and it is refused rather than rounded, which is the rule the
        // decimal column above set.
        let err = session
            .run(
                "USE g INSERT (l:ledger {n: CAST('1.5' AS DECIMAL(2,1))})",
                &[],
            )
            .expect_err("an integer column holds no halves");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22003"));
    }

    /// A refusal is asked once whether this module can do anything for
    /// it, and the answer is held with it, so the second send of the
    /// same bad statement does not parse it again to find out. What
    /// that must not change is the refusal itself: the same text raises
    /// the same condition however often it is sent.
    #[test]
    fn a_refusal_that_makes_no_table_stays_the_same_refusal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "again.zu1");

        let query = "MATCH (a:person)-[:knows]->(b) RETURN c.name AS name";
        let first = session.run(query, &[]).expect_err("c is bound by nothing");
        let second = session.run(query, &[]).expect_err("and still is");
        assert_eq!(first.gqlstatus().map(|s| s.code()), Some("42002"));
        assert_eq!(
            first.gqlstatus().map(|s| s.code()),
            second.gqlstatus().map(|s| s.code())
        );
        assert_eq!(first.to_string(), second.to_string());
    }

    /// The other half of the same rule. A statement that does make a
    /// table is refused on the way in, and the refusal must not be the
    /// one the session holds: it made the table, so the second send
    /// inserts into what the first send made.
    #[test]
    fn a_statement_that_makes_a_table_still_makes_it_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "twice.zu1");

        let query = "INSERT (c:city {name: 'york', founded: 71})";
        session.run(query, &[]).expect("the first makes the table");
        session.run(query, &[]).expect("the second uses it");

        let after = session
            .run("MATCH (c:city) RETURN c.name AS name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["york", "york"]);
    }
}
