//! Running the statements that change what a file declares: schemas
//! (GC01, GC02), graphs (GC04, GC05) and graph types (GC03), each
//! created or dropped (docs/07 §9).
//!
//! The parser hands over names, because the label dictionary belongs to
//! the catalog and the parser has never seen one. Resolving them is
//! this module's job, and it is the reason a catalog statement is not
//! a plan: there is no binding table, nothing to optimize, and the only
//! output is a new catalog.
//!
//! Applying one is read, change, publish. The change happens on the
//! catalog this call loaded, so a statement that turns out to be
//! impossible halfway through leaves the file exactly as it was, even
//! though building the type interned labels along the way.
//!
//! ISO writes an element type as the pattern an element of it matches,
//! and a pattern does not have to be named. The catalog needs a name
//! per element type, since that is what an edge type's endpoints refer
//! to, so an anonymous type gets one made out of its labels here.

use zu_common::{Result, ZuError};
use zu_query::ast::{
    CatalogStmt, ElementDefKind, ElementTypeDef, Endpoint, GraphName, GraphRef, GraphTypeRef,
    GraphTypeSource,
};
use zu_query::exec::Value;

use crate::zu1::catalog::{Catalog, ElementKind, ElementType, GraphType, GraphTypeOf, ROOT_SCHEMA};
use crate::zu1::file::Zu1File;
use crate::zu1::graph;

/// What running a catalog statement did. `IF EXISTS` and `IF NOT
/// EXISTS` both turn a refusal into a statement that did nothing, and
/// a caller that wants to report which one happened needs to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Created,
    Dropped,
    /// The name was already taken, or already free, and the statement
    /// said that was acceptable.
    Nothing,
}

/// Runs one catalog statement against `db`, publishing a new catalog
/// when it changes anything.
///
/// The parameters are here for `AS COPY OF $g`, which names the graph
/// to copy the way a `USE` names the graph to read.
pub fn apply(db: &mut Zu1File, stmt: &CatalogStmt, params: &[(&str, Value)]) -> Result<Effect> {
    let mut catalog = Catalog::load(db)?;
    match stmt {
        CatalogStmt::CreateGraphType {
            name,
            if_not_exists,
            or_replace,
            source,
        } => {
            if catalog.graph_type(name).is_some() {
                if *if_not_exists {
                    return Ok(Effect::Nothing);
                }
                if !*or_replace {
                    return Err(ZuError::InvalidArgument(format!(
                        "'{name}' is already a graph type"
                    )));
                }
                // The new definition is built before the old one goes,
                // so a replacement that cannot be kept leaves the type
                // that was there.
                let ty = build(&mut catalog, name, source)?;
                catalog.drop_graph_type(name);
                catalog.add_graph_type(ty)?;
                catalog.store(db)?;
                return Ok(Effect::Created);
            }
            let ty = build(&mut catalog, name, source)?;
            catalog.add_graph_type(ty)?;
        }
        CatalogStmt::DropGraphType { name, if_exists } => {
            if !catalog.drop_graph_type(name) {
                if *if_exists {
                    return Ok(Effect::Nothing);
                }
                return Err(ZuError::InvalidArgument(format!(
                    "'{name}' is no graph type here"
                )));
            }
            catalog.store(db)?;
            return Ok(Effect::Dropped);
        }
        CatalogStmt::CreateSchema {
            path,
            if_not_exists,
        } => {
            if catalog.has_schema(path) {
                if *if_not_exists {
                    return Ok(Effect::Nothing);
                }
                return Err(ZuError::InvalidArgument(format!(
                    "'{path}' is already a schema"
                )));
            }
            catalog.add_schema(path)?;
        }
        CatalogStmt::DropSchema { path, if_exists } => {
            if path == ROOT_SCHEMA {
                return Err(ZuError::InvalidArgument(
                    "the root schema is the one every file has, so it is not one to drop".into(),
                ));
            }
            if !catalog.has_schema(path) {
                if *if_exists {
                    return Ok(Effect::Nothing);
                }
                return Err(ZuError::InvalidArgument(format!(
                    "'{path}' is no schema here"
                )));
            }
            // ISO's default is RESTRICT, and it is the right default:
            // dropping a directory takes what is in it, and a statement
            // that would take a graph with it says so.
            if let Some(graph) = catalog.graphs().iter().find(|g| g.schema == *path) {
                return Err(ZuError::InvalidArgument(format!(
                    "'{path}' still holds the graph '{}'",
                    graph.name
                )));
            }
            catalog.drop_schema(path);
            catalog.store(db)?;
            return Ok(Effect::Dropped);
        }
        CatalogStmt::CreateGraph {
            name,
            if_not_exists,
            or_replace,
            of,
            copy_of,
        } => {
            let (schema, name) = split(name);
            if let Some(existing) = catalog.graph(&schema, &name) {
                if *if_not_exists {
                    return Ok(Effect::Nothing);
                }
                if !*or_replace {
                    return Err(ZuError::InvalidArgument(format!(
                        "'{name}' is already a graph in '{schema}'"
                    )));
                }
                // The type is built before the old graph goes, so a
                // replacement that cannot be kept leaves the graph
                // that was there.
                let id = existing.id;
                let graph_type = graph_type_of(&mut catalog, &name, of)?;
                let source = copy_source(&catalog, copy_of.as_ref(), params)?;
                // A replacement frees what the old graph held before it
                // writes the new one, so a graph asked to become a copy
                // of itself is asked for a copy of what is about to be
                // gone.
                if source == Some(id) {
                    return Err(ZuError::InvalidArgument(format!(
                        "'{name}' cannot be replaced by a copy of itself"
                    )));
                }
                // Everything the add below could refuse is asked here,
                // because after the free there is no graph to leave
                // standing.
                catalog.check_graph(&schema, &graph_type)?;
                graph::free_graph_storage(db, &catalog, id)?;
                catalog.drop_graph(id);
                let target = catalog.add_graph(&name, &schema, graph_type)?;
                copy_into(db, &mut catalog, source, target)?;
                catalog.store(db)?;
                return Ok(Effect::Created);
            }
            let graph_type = graph_type_of(&mut catalog, &name, of)?;
            let source = copy_source(&catalog, copy_of.as_ref(), params)?;
            let target = catalog.add_graph(&name, &schema, graph_type)?;
            copy_into(db, &mut catalog, source, target)?;
        }
        CatalogStmt::DropGraph { name, if_exists } => {
            let (schema, name) = split(name);
            let Some(graph) = catalog.graph(&schema, &name) else {
                if *if_exists {
                    return Ok(Effect::Nothing);
                }
                return Err(ZuError::InvalidArgument(format!(
                    "'{name}' is no graph in '{schema}'"
                )));
            };
            let id = graph.id;
            // The blocks come back here and the catalog forgets the
            // tables below; one checkpoint publishes both.
            graph::free_graph_storage(db, &catalog, id)?;
            catalog.drop_graph(id);
            catalog.store(db)?;
            return Ok(Effect::Dropped);
        }
    }
    catalog.store(db)?;
    Ok(Effect::Created)
}

/// A written name as a schema and a name in it. A name with no path in
/// it is a name in the root schema, which is the schema a session works
/// in until there is a statement that says otherwise.
fn split(name: &GraphName) -> (String, String) {
    (
        name.schema
            .clone()
            .unwrap_or_else(|| ROOT_SCHEMA.to_string()),
        name.name.clone(),
    )
}

/// The type a created graph is of (GG01 to GG04).
///
/// A type written inline has no name of its own, so it is held by the
/// graph rather than added to the file's graph types. `LIKE` takes the
/// type of an existing graph, which is the type it was created with, or
/// the type its tables describe when it was created with none.
fn graph_type_of(catalog: &mut Catalog, name: &str, of: &GraphTypeRef) -> Result<GraphTypeOf> {
    match of {
        GraphTypeRef::Any => Ok(GraphTypeOf::Open),
        GraphTypeRef::Named(ty) => Ok(GraphTypeOf::Named(ty.clone())),
        GraphTypeRef::Source(source @ GraphTypeSource::Elements(_)) => {
            Ok(GraphTypeOf::Inline(build(catalog, name, source)?))
        }
        GraphTypeRef::Source(GraphTypeSource::Like(source)) => {
            let graph = graph_named(catalog, source)?;
            match &graph.graph_type {
                GraphTypeOf::Named(ty) => Ok(GraphTypeOf::Named(ty.clone())),
                GraphTypeOf::Inline(ty) => Ok(GraphTypeOf::Inline(GraphType {
                    name: name.to_string(),
                    ..ty.clone()
                })),
                // An open graph that holds tables has the type its
                // tables describe, and one that holds none is open and
                // nothing more.
                GraphTypeOf::Open if catalog.graph_tables(graph.id).is_empty() => {
                    Ok(GraphTypeOf::Open)
                }
                GraphTypeOf::Open => {
                    let id = graph.id;
                    Ok(GraphTypeOf::Inline(catalog.infer_graph_type(name, id)?))
                }
            }
        }
    }
}

/// The graph `AS COPY OF` names (GG05), resolved before anything is
/// created so a statement naming a graph the file does not have leaves
/// the file alone.
fn copy_source(
    catalog: &Catalog,
    source: Option<&GraphRef>,
    params: &[(&str, Value)],
) -> Result<Option<u32>> {
    match source {
        None => Ok(None),
        // The graph the statement is against, which is the home graph:
        // that is the one a query with no `USE` reads and the one a
        // loaded file put its tables in. A catalog statement runs
        // outside any working graph, so the two words are one graph
        // here.
        Some(GraphRef::Current) | Some(GraphRef::Home) => Ok(Some(catalog.home_graph_id())),
        Some(GraphRef::Param(name)) => {
            Ok(Some(crate::query::graph_of_param(catalog, name, params)?))
        }
        Some(GraphRef::Named(name)) => {
            let (schema, name) = split(name);
            let graph = catalog.graph(&schema, &name).ok_or_else(|| {
                ZuError::InvalidArgument(format!("'{name}' is no graph in '{schema}'"))
            })?;
            Ok(Some(graph.id))
        }
    }
}

/// Fills a created graph with a copy of another one (GG05).
///
/// The copy is by value and not by reference: the new graph gets tables
/// of its own holding the same names and blocks of its own holding the
/// same bytes, so a write to either graph is nothing to the other. A
/// graph that holds no tables copies as the empty graph it is, which
/// falls out of there being nothing to walk.
fn copy_into(
    db: &mut Zu1File,
    catalog: &mut Catalog,
    source: Option<u32>,
    target: u32,
) -> Result<()> {
    let Some(source) = source else {
        return Ok(());
    };
    let tables = catalog.copy_graph_tables(source, target)?;
    graph::copy_graph_storage(db, &tables)
}

/// The graph a statement names, which is a graph in the root schema
/// unless the name is a path.
fn graph_named<'a>(catalog: &'a Catalog, name: &str) -> Result<&'a crate::zu1::catalog::GraphDef> {
    let (schema, name) = match name.rsplit_once('/') {
        Some(("", name)) => (ROOT_SCHEMA.to_string(), name.to_string()),
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => (ROOT_SCHEMA.to_string(), name.to_string()),
    };
    catalog
        .graph(&schema, &name)
        .ok_or_else(|| ZuError::InvalidArgument(format!("'{name}' is no graph in '{schema}'")))
}

/// The catalog object a `CREATE GRAPH TYPE` describes.
///
/// `LIKE` reads the tables and not the data, so it costs a catalog walk
/// on a file of any size (GG04). A type written out in braces is closed:
/// somebody listed the element types, and a list nobody qualified is
/// the whole list.
fn build(catalog: &mut Catalog, name: &str, source: &GraphTypeSource) -> Result<GraphType> {
    let defs = match source {
        GraphTypeSource::Like(graph) => {
            let id = graph_named(catalog, graph)?.id;
            return catalog.infer_graph_type(name, id);
        }
        GraphTypeSource::Elements(defs) => defs,
    };
    let mut ty = GraphType::closed(name);
    for def in defs {
        match &def.kind {
            ElementDefKind::Node => {
                let element = element(catalog, &ty, def)?;
                ty.elements.push(element);
            }
            ElementDefKind::Edge {
                from,
                to,
                undirected,
            } => {
                let from = resolve(catalog, &mut ty, from)?;
                let to = resolve(catalog, &mut ty, to)?;
                let mut edge = element(catalog, &ty, def)?;
                edge.kind = ElementKind::Edge;
                edge.from = Some(from);
                edge.to = Some(to);
                edge.undirected = *undirected;
                ty.elements.push(edge);
            }
        }
    }
    Ok(ty)
}

/// One element type as the catalog holds it. The kind here is always a
/// node; an edge type is this with its endpoints filled in, since the
/// two are written the same way inside the brackets.
fn element(catalog: &mut Catalog, ty: &GraphType, def: &ElementTypeDef) -> Result<ElementType> {
    let labels = intern(catalog, &def.labels)?;
    let name = match &def.name {
        Some(name) => name.clone(),
        None => derive_name(ty, &def.labels),
    };
    let mut out = ElementType::node(&name, labels);
    if !def.key_labels.is_empty() {
        out = out.with_key(intern(catalog, &def.key_labels)?);
    }
    for prop in &def.properties {
        out = out.with_property(&prop.name, prop.ty.clone(), prop.optional);
    }
    Ok(out)
}

/// The name an anonymous element type gets: its labels, and a number
/// after them when a type of that name is already in the graph type.
///
/// GG24 is two node types written `(:Person {...})` in one graph type,
/// so a name made of the labels alone is not enough and the number is
/// what keeps the second one from being the first one written twice.
fn derive_name(ty: &GraphType, labels: &[String]) -> String {
    let base = if labels.is_empty() {
        "element".to_string()
    } else {
        labels.join("&")
    };
    if ty.element(&base).is_none() {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}_{n}");
        if ty.element(&candidate).is_none() {
            return candidate;
        }
    }
    unreachable!("a name is free eventually")
}

/// The name of the node type an edge type ends at, declaring it when
/// the edge type pattern wrote it out.
///
/// An endpoint written out is deduplicated against the node types the
/// graph type already has: `(:Person)-[:KNOWS]->(:Person)` names one
/// node type twice rather than two of them. A type that declares
/// properties is never folded into another, because two types over one
/// label set that differ in what they declare are GG24 and the point
/// there is that both survive.
fn resolve(catalog: &mut Catalog, ty: &mut GraphType, end: &Endpoint) -> Result<String> {
    let def = match end {
        Endpoint::Named(name) => match ty.element(name) {
            Some(e) if e.kind == ElementKind::Node => return Ok(name.clone()),
            Some(_) => {
                return Err(ZuError::InvalidArgument(format!(
                    "an edge type ends at '{name}', which is an edge type here"
                )));
            }
            None => {
                return Err(ZuError::InvalidArgument(format!(
                    "an edge type ends at '{name}', which no node type in this graph type is called"
                )));
            }
        },
        Endpoint::Inline(def) => def.as_ref(),
    };
    let element = element(catalog, ty, def)?;
    if def.name.is_none() && def.properties.is_empty() {
        let same = ty.elements.iter().find(|e| {
            e.kind == ElementKind::Node
                && e.properties.is_empty()
                && e.labels == element.labels
                && e.key_labels == element.key_labels
        });
        if let Some(same) = same {
            return Ok(same.name.clone());
        }
    }
    let name = element.name.clone();
    ty.elements.push(element);
    Ok(name)
}

/// Label names to dictionary ids, adding the ones the file has never
/// seen. A graph type may describe labels no row carries yet, which is
/// the ordinary case for a type written before the data is loaded.
fn intern(catalog: &mut Catalog, names: &[String]) -> Result<Vec<u16>> {
    names.iter().map(|n| catalog.intern_label(n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zu1::catalog::KeyLabels;
    use crate::zu1::graph;
    use zu_common::types::{DurationKind, IntBits, LogicalType};
    use zu_query::ast::Statement;
    use zu_query::parser::parse_statement;

    /// Runs a statement against a fresh file and answers the catalog it
    /// left behind.
    fn applied(statements: &[&str]) -> Catalog {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("types.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        graph::bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1), (2, 3)]).expect("load");
        for source in statements {
            let Statement::Catalog(stmt) = parse_statement(source).expect("parse") else {
                panic!("not a catalog statement");
            };
            apply(&mut db, &stmt, &[]).expect("apply");
        }
        Catalog::load(&mut db).expect("catalog")
    }

    fn names(ty: &GraphType) -> Vec<&str> {
        ty.elements.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn an_anonymous_element_type_is_named_after_the_labels_it_carries() {
        // GG24: two node types over one label, which the catalog has to
        // hold under two names because a name is what an endpoint says.
        let catalog = applied(&["CREATE PROPERTY GRAPH TYPE gg24 {
               (:Person {name :: STRING}),
               (:Person {age :: INT})
             }"]);
        let ty = catalog.graph_type("gg24").expect("gg24");
        assert_eq!(names(ty), ["Person", "Person_2"]);
        assert_eq!(ty.elements[0].properties[0].name, "name");
        assert_eq!(ty.elements[1].properties[0].name, "age");
        // GG22: nothing was written before an arrow, so the key label
        // set is the whole label set rather than absent.
        let person = catalog.label_id("Person").expect("Person");
        assert_eq!(ty.elements[0].key_labels, KeyLabels::Inferred(vec![person]));
    }

    /// GG23. A declared property type is written to the file and read
    /// back, so the ones a column cannot hold have to survive the trip
    /// as well: naming a type is not the same promise as storing a
    /// value of one. The spellings here are the ones ISO 18.7 writes
    /// and a column code has no room for, a length bound and an instant
    /// that carries a zone, beside the verbose integer names and the
    /// duration qualifier, which are the same types under other words.
    #[test]
    fn a_declared_property_type_survives_the_file_whatever_it_names() {
        let catalog = applied(&["CREATE PROPERTY GRAPH TYPE gg23 {
               (:Bounded {a :: STRING(1,5), b :: CHAR(3), c :: VARCHAR(10)}),
               (:Instants {a :: ZONED DATETIME, b :: ZONED TIME}),
               (:Verbose {a :: INTEGER8, b :: SMALL INTEGER, c :: BIG INTEGER,
                          d :: UNSIGNED INTEGER32}),
               (:Spans {a :: DURATION(YEAR TO MONTH), b :: DURATION(DAY TO SECOND)})
             }"]);
        let ty = catalog.graph_type("gg23").expect("gg23");
        let declared = |element: &str, prop: &str| {
            ty.element(element)
                .expect(element)
                .properties
                .iter()
                .find(|p| p.name == prop)
                .map(|p| p.ty.clone())
                .expect(prop)
        };
        assert_eq!(
            declared("Bounded", "a"),
            LogicalType::Str {
                min: Some(1),
                max: Some(5),
                fixed: false
            }
        );
        assert_eq!(
            declared("Bounded", "b"),
            LogicalType::Str {
                min: Some(3),
                max: Some(3),
                fixed: true
            }
        );
        assert_eq!(
            declared("Bounded", "c"),
            LogicalType::Str {
                min: None,
                max: Some(10),
                fixed: false
            }
        );
        assert_eq!(declared("Instants", "a"), LogicalType::ZonedDatetime);
        assert_eq!(declared("Instants", "b"), LogicalType::ZonedTime);
        let int = |signed, bits| LogicalType::Int {
            signed,
            bits,
            precision: None,
        };
        assert_eq!(declared("Verbose", "a"), int(true, IntBits::B8));
        assert_eq!(declared("Verbose", "b"), int(true, IntBits::B16));
        assert_eq!(declared("Verbose", "c"), int(true, IntBits::B64));
        assert_eq!(declared("Verbose", "d"), int(false, IntBits::B32));
        assert_eq!(
            declared("Spans", "a"),
            LogicalType::Duration(DurationKind::YearMonth)
        );
        assert_eq!(
            declared("Spans", "b"),
            LogicalType::Duration(DurationKind::DayTime)
        );
    }

    #[test]
    fn an_endpoint_written_out_declares_the_node_type_once() {
        // GG25: two edge types keyed on one label, and three endpoint
        // patterns naming two node types between them.
        let catalog = applied(&["CREATE PROPERTY GRAPH TYPE gg25 {
               (:Person)-[:KNOWS => :Nearby]->(:Person),
               (:Person)-[:KNOWS => :Distant]->(:Company)
             }"]);
        let ty = catalog.graph_type("gg25").expect("gg25");
        assert_eq!(
            names(ty),
            ["Person", "KNOWS&Nearby", "Company", "KNOWS&Distant"]
        );
        let knows = ty.element("KNOWS&Nearby").expect("the first edge type");
        assert_eq!(knows.kind, ElementKind::Edge);
        assert_eq!(knows.from.as_deref(), Some("Person"));
        assert_eq!(knows.to.as_deref(), Some("Person"));
        assert!(!knows.undirected);
        // GG21: the key is what was written before the arrow, and the
        // label after it is one the edge carries and is not keyed on.
        let close = catalog.label_id("Nearby").expect("Nearby");
        assert_eq!(knows.key_labels.ids().len(), 1);
        assert!(knows.labels.contains(&close));
        assert_eq!(
            ty.element("KNOWS&Distant")
                .expect("the second")
                .to
                .as_deref(),
            Some("Company")
        );
    }

    #[test]
    fn a_named_element_type_is_the_one_an_endpoint_points_at() {
        // GG20, and the reason a name is worth writing: the edge type
        // refers to the node type instead of repeating its pattern.
        let catalog = applied(&["CREATE PROPERTY GRAPH TYPE gg20 {
               NODE TYPE PersonType (:Person {name :: STRING}),
               EDGE TYPE Knows (PersonType)-[:KNOWS]->(PersonType)
             }"]);
        let ty = catalog.graph_type("gg20").expect("gg20");
        assert_eq!(names(ty), ["PersonType", "Knows"]);
        let knows = ty.element("Knows").expect("Knows");
        assert_eq!(knows.from.as_deref(), Some("PersonType"));
        assert_eq!(knows.to.as_deref(), Some("PersonType"));
    }

    #[test]
    fn an_endpoint_that_names_nothing_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("refused.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        graph::bulk_load_as(&mut db, "person", "knows", 4, &[(0, 1)]).expect("load");
        let Statement::Catalog(stmt) =
            parse_statement("CREATE GRAPH TYPE t { EDGE TYPE E (Absent)-[:R]->(Absent) }")
                .expect("parse")
        else {
            panic!("not a catalog statement");
        };
        let err = apply(&mut db, &stmt, &[])
            .expect_err("no such node type")
            .to_string();
        assert!(
            err.contains("no node type in this graph type is called"),
            "{err}"
        );
        assert!(
            Catalog::load(&mut db)
                .expect("catalog")
                .graph_types()
                .is_empty()
        );
    }
}
