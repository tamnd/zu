//! Running the statements that change what a file declares: `CREATE
//! GRAPH TYPE` and `DROP GRAPH TYPE` (docs/07 §9, GC03).
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
use zu_query::ast::{CatalogStmt, ElementDefKind, ElementTypeDef, Endpoint, GraphTypeSource};

use crate::zu1::catalog::{Catalog, ElementKind, ElementType, GraphType};
use crate::zu1::file::Zu1File;

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
pub fn apply(db: &mut Zu1File, stmt: &CatalogStmt) -> Result<Effect> {
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
    }
    catalog.store(db)?;
    Ok(Effect::Created)
}

/// The catalog object a `CREATE GRAPH TYPE` describes.
///
/// `LIKE` reads the tables and not the data, so it costs a catalog walk
/// on a file of any size (GG04). A type written out in braces is closed:
/// somebody listed the element types, and a list nobody qualified is
/// the whole list.
fn build(catalog: &mut Catalog, name: &str, source: &GraphTypeSource) -> Result<GraphType> {
    let defs = match source {
        GraphTypeSource::Like(_) => return catalog.infer_graph_type(name),
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
            apply(&mut db, &stmt).expect("apply");
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

    #[test]
    fn an_endpoint_written_out_declares_the_node_type_once() {
        // GG25: two edge types keyed on one label, and three endpoint
        // patterns naming two node types between them.
        let catalog = applied(&["CREATE PROPERTY GRAPH TYPE gg25 {
               (:Person)-[:KNOWS => :Close]->(:Person),
               (:Person)-[:KNOWS => :Distant]->(:Company)
             }"]);
        let ty = catalog.graph_type("gg25").expect("gg25");
        assert_eq!(
            names(ty),
            ["Person", "KNOWS&Close", "Company", "KNOWS&Distant"]
        );
        let knows = ty.element("KNOWS&Close").expect("the first edge type");
        assert_eq!(knows.kind, ElementKind::Edge);
        assert_eq!(knows.from.as_deref(), Some("Person"));
        assert_eq!(knows.to.as_deref(), Some("Person"));
        assert!(!knows.undirected);
        // GG21: the key is what was written before the arrow, and the
        // label after it is one the edge carries and is not keyed on.
        let close = catalog.label_id("Close").expect("Close");
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
        let err = apply(&mut db, &stmt)
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
