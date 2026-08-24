//! The catalog and the table index, the two meta chains that make a zu1
//! file self-describing.
//!
//! The catalog behind `catalog_root` holds table definitions: node tables
//! with their row domain, rel tables with their FROM and TO node tables
//! and edge count. The table index behind `table_index_root` maps a rel
//! table id to the root of its group directory chain, so DDL rewrites
//! only the catalog while a bulk load rewrites only the index entry it
//! touches. Both encodings are version-prefixed and little-endian per
//! `docs/04-storage-zu1-format.md` §1.
//!
//! Catalog layout: `version: u16`, `node_table_count: u32`,
//! `rel_table_count: u32`, `label_count: u32`, then per node table
//! `id: u32`, `name_len: u16` + UTF-8 bytes, `node_count: u64`,
//! `declared_label_count: u16` and that many `label: u16`, then per rel
//! table `id: u32`, name, `from: u32`, `to: u32`, `edge_count: u64`,
//! then the label dictionary as `label_count` names, then
//! `graph_type_count: u32` and that many graph types.
//!
//! A graph type is a name, a closed flag, and its element types: each a
//! name, a kind byte, a flag byte, a label set, a key label set behind
//! the byte that says which of the three kinds it is, the two endpoint
//! names for an edge type, and the properties it declares as a name, a
//! type code, and an optionality byte. Properties hang off the element
//! type rather than off the graph, which is what pays for the three
//! relaxed consistency features at once (GG24 to GG26): two element
//! types may share a key label set and disagree about what a property
//! of the same name holds.
//!
//! The dictionary is per file and a label is its position in it, which
//! is what makes a label set a bitset: a node's labels are a word with
//! one bit per dictionary entry, and a pattern's labels are a mask over
//! the same word. A node table declares which labels its rows may
//! carry, the first of which is the table's own name and the one every
//! row carries, so a label a table never declares prunes that table at
//! plan time rather than being tested a row at a time.
//!
//! Table index layout: `version: u16`, `entry_count: u32`, then per
//! entry `table_id: u32`, `directory_root: u64`.

use zu_common::gqlstatus::{GqlStatus, codes};
use zu_common::{LogicalType, Result, ZuError};

use crate::file::{BlockPtr, NULL_BLOCK, Zu1File};
use crate::{meta, props};

/// Version 1 had no labels: a node's only label was the name of its
/// table. Version 2 adds the label dictionary and the set each node
/// table declares. A version 1 catalog still reads, and reads as the
/// graph it always was, one label per table carrying the table's name.
/// Version 3 adds graph types, and a version 2 catalog reads as a file
/// that declares none, which is the open graph it has always been.
/// Version 4 adds schemas and named graphs, and says which graph each
/// table belongs to. A version 3 catalog reads as the file it always
/// was: one schema, one graph called `home`, and every table in it.
/// Version 5 says whether a rel table's edges have a direction, and a
/// version 4 catalog reads as the file it was, every edge directed.
const CATALOG_VERSION: u16 = 5;
const TABLE_INDEX_VERSION: u16 = 1;

/// The label dictionary is bounded by the width of the bitset a node
/// carries. One word per node is the fast path the whole design is
/// sized for (LDBC SNB declares 13 labels, LinkBench and Graph500 one),
/// and a wider set waits for the spill representation.
pub const MAX_LABELS: usize = 64;

/// Table ids live in the 14-bit field of `NodeId`.
pub const MAX_TABLE_ID: u32 = (1 << 14) - 1;

/// The most properties one element holds. Generous and finite rather
/// than unlimited, for the reason the label dictionary is bounded: a
/// declared limit is one the encoding can be built on, and an engine
/// that declares none pays for the possibility on every element of
/// every graph. The standard has a condition for hitting a declared
/// limit, and an engine with no limit can never raise it, so declaring
/// one is also the honest answer to the question the code asks.
pub const MAX_PROPERTIES: usize = 4096;

const MAX_NAME_LEN: usize = 256;

/// The schema every zu1 file has, and the parent of a graph nobody
/// wrote a path for. ISO names catalog objects by an absolute directory
/// path, and a file that never created a schema still has this one.
pub const ROOT_SCHEMA: &str = "/";

/// The graph a zu1 file has always held, and the one a load with no
/// graph named writes into. ISO calls the graph a session works on
/// without saying so its home graph, which is where the name comes from.
pub const HOME_GRAPH: &str = "home";

/// The id of that graph. Ids are handed out in order and never reused,
/// so the first graph a file has is always this one.
pub const HOME_GRAPH_ID: u32 = 0;

/// A node table: the row domain `0..node_count` that rel tables index
/// into. Property columns land with the column catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeTable {
    pub id: u32,
    pub name: String,
    /// The graph this table belongs to (GC04). Dropping that graph is
    /// what hands its blocks back.
    pub graph: u32,
    pub node_count: u64,
    /// The labels rows of this table may carry, as dictionary ids.
    /// `labels[0]` is the table's own name, which every row carries;
    /// the rest are optional per row and the bitset column says which
    /// rows have them.
    pub labels: Vec<u16>,
}

impl NodeTable {
    /// The label every row of this table carries: the table's name.
    pub fn primary_label(&self) -> u16 {
        self.labels[0]
    }

    /// The declared set as a mask, which is what a plan time prune
    /// tests a pattern's mask against.
    pub fn label_mask(&self) -> u64 {
        self.labels.iter().fold(0u64, |m, &l| m | 1 << l)
    }
}

/// A rel table: a bulk-loaded CSR pair over `from` and `to` node tables,
/// with its group directory reachable through the table index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelTable {
    pub id: u32,
    pub name: String,
    /// The graph this table belongs to, as on a node table.
    pub graph: u32,
    pub from: u32,
    pub to: u32,
    pub edge_count: u64,
    /// Whether the edges here have no direction (GH02). An undirected
    /// edge is stored once, the way it was written, and read as
    /// standing for both ways round: it is the pattern that asks which
    /// of the two it wants, not the store.
    pub undirected: bool,
}

/// Whether an element type describes nodes or edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementKind {
    Node,
    Edge,
}

/// One property an element type declares (GG26). The declaration hangs
/// off the element type rather than off a name shared by the graph, so
/// two element types may declare `since` with different types and both
/// are right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyType {
    pub name: String,
    pub ty: LogicalType,
    /// Whether an element of the type may leave the property out.
    pub optional: bool,
}

/// The label subset that picks an element type out of a graph type
/// (GG21 to GG23).
///
/// ISO lets a type declare one, lets it be inferred from the whole
/// label set when it is not declared, and lets an open graph type hold
/// a type with none at all. The three cases select differently at
/// insert time, so they are three cases here and not one `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyLabels {
    /// GG21: written out in the type definition.
    Declared(Vec<u16>),
    /// GG22: not written, so it is the whole label set.
    Inferred(Vec<u16>),
    /// GG23: the type has none. Open graph types only, and selection
    /// falls back to matching the whole label set.
    None,
}

impl KeyLabels {
    /// The label ids the key names, empty when there is no key.
    pub fn ids(&self) -> &[u16] {
        match self {
            KeyLabels::Declared(ids) | KeyLabels::Inferred(ids) => ids,
            KeyLabels::None => &[],
        }
    }

    fn code(&self) -> u8 {
        match self {
            KeyLabels::Declared(_) => 0,
            KeyLabels::Inferred(_) => 1,
            KeyLabels::None => 2,
        }
    }
}

/// One element type of a graph type (GG20 to GG26).
///
/// Two element types may share a key label set and differ in their
/// properties (GG24) or, for edges, in their endpoints (GG25). Nothing
/// here forbids that, on purpose: the shape that would have forbidden
/// it is a per graph property table, and the whole point of hanging
/// properties off the element type is that the relaxed features cost
/// the strict case nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementType {
    pub name: String,
    pub kind: ElementKind,
    /// The labels an element of this type carries, as dictionary ids.
    pub labels: Vec<u16>,
    pub key_labels: KeyLabels,
    pub properties: Vec<PropertyType>,
    /// GG01 at the element level: an open type permits properties it
    /// never declared.
    pub open: bool,
    /// Edge types only: the element type names of the two endpoints.
    pub from: Option<String>,
    pub to: Option<String>,
    /// GH02: whether an edge of this type has a direction. It is a
    /// property of the type rather than of the pattern, which is why
    /// it lives here and not in the query.
    pub undirected: bool,
}

impl ElementType {
    /// A node type carrying `labels`, with the key label set inferred
    /// from them, which is what a definition that names no key means.
    pub fn node(name: &str, labels: Vec<u16>) -> Self {
        ElementType {
            name: name.to_string(),
            kind: ElementKind::Node,
            key_labels: KeyLabels::Inferred(labels.clone()),
            labels,
            properties: Vec::new(),
            open: false,
            from: None,
            to: None,
            undirected: false,
        }
    }

    /// An edge type between two node types, key label set inferred.
    pub fn edge(name: &str, labels: Vec<u16>, from: &str, to: &str) -> Self {
        ElementType {
            kind: ElementKind::Edge,
            from: Some(from.to_string()),
            to: Some(to.to_string()),
            ..Self::node(name, labels)
        }
    }

    /// The same type with the key label set written out (GG21).
    pub fn with_key(mut self, key: Vec<u16>) -> Self {
        self.key_labels = KeyLabels::Declared(key);
        self
    }

    /// The same type with no key label set at all (GG23).
    pub fn without_key(mut self) -> Self {
        self.key_labels = KeyLabels::None;
        self
    }

    pub fn with_property(mut self, name: &str, ty: LogicalType, optional: bool) -> Self {
        self.properties.push(PropertyType {
            name: name.to_string(),
            ty,
            optional,
        });
        self
    }

    pub fn label_mask(&self) -> u64 {
        self.labels.iter().fold(0u64, |m, &l| m | 1 << l)
    }

    /// The mask an element's label set must contain for this type to
    /// describe it. A type with no key label set is selected by its
    /// whole label set, which is the fallback GG23 asks for.
    pub fn selection_mask(&self) -> u64 {
        match &self.key_labels {
            KeyLabels::None => self.label_mask(),
            key => key.ids().iter().fold(0u64, |m, &l| m | 1 << l),
        }
    }

    /// The type a property of this name has here, `None` when the type
    /// does not declare one. An open type may still carry it.
    pub fn property(&self, name: &str) -> Option<&PropertyType> {
        self.properties.iter().find(|p| p.name == name)
    }
}

/// A graph type: the element types a graph's elements may have, and
/// whether that list is the whole of it (GG01, GG02).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphType {
    pub name: String,
    /// GG02. A closed type is a guarantee rather than a description:
    /// every element matches a declared element type, so the optimizer
    /// may resolve a property to a column without asking the row.
    pub closed: bool,
    pub elements: Vec<ElementType>,
}

impl GraphType {
    /// An open graph type, which is what a graph with no type declared
    /// for it has.
    pub fn open(name: &str) -> Self {
        GraphType {
            name: name.to_string(),
            closed: false,
            elements: Vec::new(),
        }
    }

    pub fn closed(name: &str) -> Self {
        GraphType {
            name: name.to_string(),
            closed: true,
            elements: Vec::new(),
        }
    }

    pub fn with(mut self, element: ElementType) -> Self {
        self.elements.push(element);
        self
    }

    pub fn element(&self, name: &str) -> Option<&ElementType> {
        self.elements.iter().find(|e| e.name == name)
    }

    /// Every element type an element carrying `labels` could have.
    ///
    /// More than one is not an error: GG24 and GG25 are exactly the
    /// case where two types share a key label set and differ in what
    /// hangs off it, and the caller picks by what it is doing. An
    /// empty key label set is contained in every label set and so
    /// selects every element of its kind, which is what an edge type
    /// inferred from a rel table has and why the caller narrows those
    /// by their endpoints.
    pub fn types_for(&self, kind: ElementKind, labels: u64) -> Vec<&ElementType> {
        self.elements
            .iter()
            .filter(|e| e.kind == kind)
            .filter(|e| labels & e.selection_mask() == e.selection_mask())
            .collect()
    }

    /// The element type an element carrying `labels` belongs to, if one
    /// of them describes it. This is what a closed graph type promises
    /// of every element it holds (GG02).
    ///
    /// Belonging is two things at once: the type's key label set picks
    /// the element out, and every label the element carries is one the
    /// type declares. Selection alone would let an element grow a label
    /// no type mentions and still count as described, which is the
    /// promise a closed type is there to make.
    ///
    /// More than one type may describe it, and the first is answered,
    /// because what the caller asks is whether any does.
    pub fn holder(&self, kind: ElementKind, labels: u64) -> Option<&ElementType> {
        self.elements
            .iter()
            .filter(|e| e.kind == kind)
            .filter(|e| labels & e.selection_mask() == e.selection_mask())
            .find(|e| labels & !e.label_mask() == 0)
    }

    /// Checks the type against the graph's label dictionary and its own
    /// rules. A closed type has to be self-contained, which is what
    /// makes it worth anything to the optimizer.
    ///
    /// Every check below is the same question whichever way the type
    /// arrived, and the answer means two different things, so the
    /// caller says which with an [`Origin`].
    fn validate(&self, labels: usize, origin: Origin) -> Result<()> {
        let mut names: Vec<&str> = self.elements.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        // The duplicate is named rather than counted. These refusals
        // carry no position, because the defs they are about have no
        // spans, so the name is the whole of what a reader has to find
        // the two declarations by.
        if let Some(pair) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(origin.refuse(
                codes::C42000,
                format!(
                    "graph type '{}' declares an element type twice, both named '{}'",
                    self.name, pair[0]
                ),
            ));
        }
        for element in &self.elements {
            let where_ = || format!("element type '{}.{}'", self.name, element.name);
            for &label in element.labels.iter().chain(element.key_labels.ids()) {
                if usize::from(label) >= labels {
                    return Err(origin.refuse(
                        codes::C42002,
                        format!("{} names label {label} of {labels}", where_()),
                    ));
                }
            }
            for &key in element.key_labels.ids() {
                if !element.labels.contains(&key) {
                    return Err(origin.refuse(
                        codes::C42000,
                        format!("{} keys on label {key}, which it does not carry", where_()),
                    ));
                }
            }
            let mut props: Vec<&str> = element.properties.iter().map(|p| p.name.as_str()).collect();
            props.sort_unstable();
            if let Some(pair) = props.windows(2).find(|w| w[0] == w[1]) {
                return Err(origin.refuse(
                    codes::C42000,
                    format!(
                        "{} declares a property twice, both named '{}'",
                        where_(),
                        pair[0]
                    ),
                ));
            }
            // The catalog writes a declared type with the codes a
            // column stores, so a type no column can hold is a type no
            // element type can name. This is where that is refused; the
            // encoder past this point may assume it.
            for prop in &element.properties {
                if props::declared_type_bytes(&prop.ty).is_none() {
                    return Err(origin.refuse(
                        codes::C42000,
                        format!(
                            "{} declares '{}' with a type this file cannot write",
                            where_(),
                            prop.name
                        ),
                    ));
                }
            }
            match element.kind {
                ElementKind::Node => {
                    if element.from.is_some() || element.to.is_some() {
                        return Err(origin.refuse(
                            codes::C42000,
                            format!("{} is a node type with endpoints", where_()),
                        ));
                    }
                }
                ElementKind::Edge => {
                    for end in [&element.from, &element.to] {
                        let end = end.as_deref().ok_or_else(|| {
                            origin.refuse(
                                codes::C42000,
                                format!("{} is an edge type with no endpoint", where_()),
                            )
                        })?;
                        match self.element(end) {
                            Some(e) if e.kind == ElementKind::Node => {}
                            _ => {
                                return Err(origin.refuse(
                                    codes::C42002,
                                    format!(
                                        "{} ends at '{end}', which is no node type here",
                                        where_()
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
            // GG23 says a type may have no key label set; a closed
            // graph type is the promise that every element matches a
            // declared type, and a type nothing selects cannot keep it.
            if self.closed && matches!(element.key_labels, KeyLabels::None) {
                // ISO names this one twice, once per kind: 42012 for a
                // node type and 42014 for an edge type, both of them
                // key labels below the supported minimum.
                let below = match element.kind {
                    ElementKind::Node => codes::C42012,
                    ElementKind::Edge => codes::C42014,
                };
                return Err(origin.refuse(
                    below,
                    format!("{} has no key label set in a closed graph type", where_()),
                ));
            }
            if self.closed && element.open {
                return Err(origin.refuse(
                    codes::C42000,
                    format!("{} is open in a closed graph type", where_()),
                ));
            }
        }
        Ok(())
    }
}

/// Where a graph type came from, which is what decides what a failed
/// check means.
///
/// The checks in [`GraphType::validate`] are the same either way, and
/// the sentence they produce is not. A type a user just declared that
/// fails one of them is a statement zu will not perform, and the user
/// can fix it: it gets a GQLSTATUS. The same type read back out of a
/// file that once held it means the bytes on disk are not the bytes zu
/// wrote, and the user cannot fix that: it gets `corrupt`, which is
/// the word for damage and is a lie about anything else.
///
/// This is why the lint in `crates/zu/tests/refusals.rs` runs
/// statements rather than counting call sites. There is one copy of
/// each check and it is right in one caller and wrong in the other,
/// and only the caller knows which.
///
/// Four of the checks below only ever run as [`Origin::Stored`],
/// because no statement can build the structure they refuse. A key
/// label a type does not carry cannot be declared, since the key label
/// set joins the label set rather than being checked against it. A
/// label outside the dictionary cannot, since a declaration interns
/// every label it names. A node type with endpoints and an edge type
/// whose endpoint is no node type cannot, since an arc's own node
/// patterns are the node types it ends at. Those four keep the word
/// corrupt in practice and have a condition anyway, so that the day one
/// of them becomes reachable it arrives already answered.
#[derive(Clone, Copy)]
enum Origin {
    /// A user declared this type in a statement just now.
    Declared,
    /// This type was read out of a file, or built from tables that
    /// were.
    Stored,
}

impl Origin {
    fn refuse(self, status: GqlStatus, detail: String) -> ZuError {
        match self {
            Origin::Declared => ZuError::gql(status, detail),
            Origin::Stored => corrupt("catalog", detail),
        }
    }
}

/// The type a graph is of (GG01 to GG04).
///
/// ISO lets a graph be created with no type at all, with the name of a
/// graph type the catalog holds, or with a type written inline in the
/// statement. A type written inline has no name of its own and is no
/// catalog object, so it lives here rather than in the file's list of
/// graph types: `CREATE GRAPH g { (:Person) }` declares no graph type
/// called anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphTypeOf {
    /// GG01: any graph. Every element type is allowed, which is what a
    /// zu1 file has always been.
    Open,
    /// The name of a graph type this file holds.
    Named(String),
    /// GG03: written inline, so it is anonymous and belongs to the one
    /// graph created with it.
    Inline(GraphType),
}

impl GraphTypeOf {
    fn code(&self) -> u8 {
        match self {
            GraphTypeOf::Open => 0,
            GraphTypeOf::Named(_) => 1,
            GraphTypeOf::Inline(_) => 2,
        }
    }
}

/// A graph the catalog holds (GC04): a name in a schema, the type it is
/// of, and the tables that are its contents.
///
/// The tables are not listed here. A table names the graph it belongs
/// to instead, which makes the contents of a graph a filter over the
/// tables and keeps one table from being in two graphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDef {
    pub id: u32,
    pub name: String,
    /// The schema this graph lives in, an absolute directory path.
    pub schema: String,
    pub graph_type: GraphTypeOf,
}

/// The table definitions of one zu1 file. Names share a single
/// namespace across both kinds, so a rel table cannot shadow a node
/// table.
///
/// A file always has the root schema and the home graph, whatever else
/// it holds: they are what a load with nothing said about where writes
/// into, and dropping the home graph empties it rather than leaving the
/// file with nowhere to put the next table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    nodes: Vec<NodeTable>,
    rels: Vec<RelTable>,
    labels: Vec<String>,
    graph_types: Vec<GraphType>,
    schemas: Vec<String>,
    graphs: Vec<GraphDef>,
}

impl Default for Catalog {
    fn default() -> Self {
        Catalog {
            nodes: Vec::new(),
            rels: Vec::new(),
            labels: Vec::new(),
            graph_types: Vec::new(),
            schemas: vec![ROOT_SCHEMA.to_string()],
            graphs: vec![GraphDef {
                id: HOME_GRAPH_ID,
                name: HOME_GRAPH.to_string(),
                schema: ROOT_SCHEMA.to_string(),
                graph_type: GraphTypeOf::Open,
            }],
        }
    }
}

fn corrupt(what: &'static str, detail: String) -> ZuError {
    ZuError::Corrupt { what, detail }
}

fn encode_name(out: &mut Vec<u8>, name: &str) {
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
}

fn decode_name(bytes: &[u8], pos: &mut usize, what: &'static str) -> Result<String> {
    let len = bytes
        .get(*pos..*pos + 2)
        .ok_or_else(|| corrupt(what, "truncated name length".into()))?;
    let len = u16::from_le_bytes(len.try_into().unwrap()) as usize;
    *pos += 2;
    if len == 0 || len > MAX_NAME_LEN {
        return Err(corrupt(
            what,
            format!("name length {len} out of 1..{MAX_NAME_LEN}"),
        ));
    }
    let raw = bytes
        .get(*pos..*pos + len)
        .ok_or_else(|| corrupt(what, "truncated name".into()))?;
    *pos += len;
    String::from_utf8(raw.to_vec()).map_err(|_| corrupt(what, "name is not UTF-8".into()))
}

fn encode_labels(out: &mut Vec<u8>, labels: &[u16]) {
    out.extend_from_slice(&(labels.len() as u16).to_le_bytes());
    for &label in labels {
        out.extend_from_slice(&label.to_le_bytes());
    }
}

fn decode_labels(bytes: &[u8], pos: &mut usize) -> Result<Vec<u16>> {
    const WHAT: &str = "catalog";
    let count = read_u16(bytes, pos, WHAT)? as usize;
    if count > MAX_LABELS {
        return Err(corrupt(
            WHAT,
            format!("a label set of {count} above {MAX_LABELS}"),
        ));
    }
    let mut labels = Vec::with_capacity(count);
    for _ in 0..count {
        labels.push(read_u16(bytes, pos, WHAT)?);
    }
    Ok(labels)
}

fn encode_graph_type(out: &mut Vec<u8>, ty: &GraphType) {
    encode_name(out, &ty.name);
    out.push(u8::from(ty.closed));
    out.extend_from_slice(&(ty.elements.len() as u32).to_le_bytes());
    for element in &ty.elements {
        encode_name(out, &element.name);
        out.push(match element.kind {
            ElementKind::Node => 0,
            ElementKind::Edge => 1,
        });
        let flags = u8::from(element.open) | u8::from(element.undirected) << 1;
        out.push(flags);
        encode_labels(out, &element.labels);
        out.push(element.key_labels.code());
        encode_labels(out, element.key_labels.ids());
        if element.kind == ElementKind::Edge {
            encode_name(out, element.from.as_deref().unwrap_or_default());
            encode_name(out, element.to.as_deref().unwrap_or_default());
        }
        out.extend_from_slice(&(element.properties.len() as u16).to_le_bytes());
        for prop in &element.properties {
            encode_name(out, &prop.name);
            // A type nothing can be declared with never reaches the
            // catalog: `GraphType::validate` refuses it when the type
            // is added.
            out.extend_from_slice(
                &props::declared_type_bytes(&prop.ty).expect("property type is declarable"),
            );
            out.push(u8::from(prop.optional));
        }
    }
}

fn decode_graph_type(bytes: &[u8], pos: &mut usize) -> Result<GraphType> {
    const WHAT: &str = "catalog";
    let name = decode_name(bytes, pos, WHAT)?;
    let closed = read_flag(bytes, pos, "graph type closedness")?;
    let count = read_u32(bytes, pos, WHAT)? as usize;
    let mut elements = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let name = decode_name(bytes, pos, WHAT)?;
        let kind = match bytes.get(*pos) {
            Some(0) => ElementKind::Node,
            Some(1) => ElementKind::Edge,
            Some(&other) => {
                return Err(corrupt(WHAT, format!("element kind {other}")));
            }
            None => return Err(corrupt(WHAT, "truncated element kind".into())),
        };
        *pos += 1;
        let flags = match bytes.get(*pos) {
            Some(&flags) if flags & !0b11 == 0 => flags,
            Some(&other) => return Err(corrupt(WHAT, format!("element type flags {other:#x}"))),
            None => return Err(corrupt(WHAT, "truncated element type flags".into())),
        };
        *pos += 1;
        let labels = decode_labels(bytes, pos)?;
        let key_code = match bytes.get(*pos) {
            Some(&code @ 0..=2) => code,
            Some(&other) => return Err(corrupt(WHAT, format!("key label set kind {other}"))),
            None => return Err(corrupt(WHAT, "truncated key label set kind".into())),
        };
        *pos += 1;
        let key_ids = decode_labels(bytes, pos)?;
        let key_labels = match key_code {
            0 => KeyLabels::Declared(key_ids),
            1 => KeyLabels::Inferred(key_ids),
            _ if key_ids.is_empty() => KeyLabels::None,
            _ => return Err(corrupt(WHAT, "a key label set that is not one".into())),
        };
        let (from, to) = match kind {
            ElementKind::Node => (None, None),
            ElementKind::Edge => (
                Some(decode_name(bytes, pos, WHAT)?),
                Some(decode_name(bytes, pos, WHAT)?),
            ),
        };
        let prop_count = read_u16(bytes, pos, WHAT)? as usize;
        let mut properties = Vec::with_capacity(prop_count.min(64));
        for _ in 0..prop_count {
            let name = decode_name(bytes, pos, WHAT)?;
            let ty = props::decode_declared_type(bytes, pos)?;
            let optional = read_flag(bytes, pos, "a property's optionality")?;
            properties.push(PropertyType { name, ty, optional });
        }
        elements.push(ElementType {
            name,
            kind,
            labels,
            key_labels,
            properties,
            open: flags & 1 != 0,
            from,
            to,
            undirected: flags & 0b10 != 0,
        });
    }
    Ok(GraphType {
        name,
        closed,
        elements,
    })
}

/// A byte that stands for a boolean, and is one of the two bytes that
/// do. A file saying anything else about a flag is a file saying
/// something nobody wrote.
fn read_flag(bytes: &[u8], pos: &mut usize, what: &str) -> Result<bool> {
    let flag = match bytes.get(*pos) {
        Some(0) => false,
        Some(1) => true,
        Some(&other) => {
            return Err(corrupt("catalog", format!("{what} is {other}, not 0 or 1")));
        }
        None => return Err(corrupt("catalog", format!("truncated {what}"))),
    };
    *pos += 1;
    Ok(flag)
}

fn read_u32(bytes: &[u8], pos: &mut usize, what: &'static str) -> Result<u32> {
    let raw = bytes
        .get(*pos..*pos + 4)
        .ok_or_else(|| corrupt(what, "truncated entry".into()))?;
    *pos += 4;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn read_u16(bytes: &[u8], pos: &mut usize, what: &'static str) -> Result<u16> {
    let raw = bytes
        .get(*pos..*pos + 2)
        .ok_or_else(|| corrupt(what, "truncated entry".into()))?;
    *pos += 2;
    Ok(u16::from_le_bytes(raw.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], pos: &mut usize, what: &'static str) -> Result<u64> {
    let raw = bytes
        .get(*pos..*pos + 8)
        .ok_or_else(|| corrupt(what, "truncated entry".into()))?;
    *pos += 8;
    Ok(u64::from_le_bytes(raw.try_into().unwrap()))
}

impl Catalog {
    /// Loads the committed catalog; a `NULL_BLOCK` root reads as empty.
    pub fn load(db: &mut Zu1File) -> Result<Self> {
        let root = db.db_header().catalog_root;
        if root == NULL_BLOCK {
            return Ok(Self::default());
        }
        Self::decode(&meta::read_chain(db, root)?)
    }

    /// Publishes the catalog: the old chain goes back to the free list,
    /// the new one is written into free space, and the header flip
    /// makes it the committed one. Nothing else in the file moves,
    /// which is what makes a catalog change cheap however large the
    /// graph under it is.
    pub fn store(&self, db: &mut Zu1File) -> Result<()> {
        self.validate()?;
        let old = db.db_header().catalog_root;
        if old != NULL_BLOCK {
            crate::graph::free_chain(db, old)?;
        }
        let root = meta::write_chain(db, &self.encode())?;
        db.db_header_mut().catalog_root = root;
        db.checkpoint()
    }

    pub fn node_tables(&self) -> &[NodeTable] {
        &self.nodes
    }

    pub fn rel_tables(&self) -> &[RelTable] {
        &self.rels
    }

    /// The node table of that name in the home graph, which is the
    /// graph every load writes into and the one a statement means when
    /// nothing says otherwise. A name is a name in a graph and not in a
    /// file, so a name two graphs both use is two tables and this
    /// answers the home one.
    pub fn node_by_name(&self, name: &str) -> Option<&NodeTable> {
        self.node_in(self.home_graph_id(), name)
    }

    pub fn rel_by_name(&self, name: &str) -> Option<&RelTable> {
        self.rel_in(self.home_graph_id(), name)
    }

    /// The node table a graph calls that name.
    pub fn node_in(&self, graph: u32, name: &str) -> Option<&NodeTable> {
        self.nodes
            .iter()
            .find(|t| t.graph == graph && t.name == name)
    }

    pub fn rel_in(&self, graph: u32, name: &str) -> Option<&RelTable> {
        self.rels
            .iter()
            .find(|t| t.graph == graph && t.name == name)
    }

    /// The id of the home graph, which is the first graph a file has
    /// and keeps its id for the file's life. A file whose home graph
    /// was dropped and not yet put back reads as the id the first one
    /// had, which is the id `ensure_home` gives it back.
    pub fn home_graph_id(&self) -> u32 {
        self.graph(ROOT_SCHEMA, HOME_GRAPH)
            .map(|g| g.id)
            .unwrap_or(HOME_GRAPH_ID)
    }

    pub fn node_by_id(&self, id: u32) -> Option<&NodeTable> {
        self.nodes.iter().find(|t| t.id == id)
    }

    pub fn rel_by_id(&self, id: u32) -> Option<&RelTable> {
        self.rels.iter().find(|t| t.id == id)
    }

    /// The label dictionary, a label's id being its position.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    pub fn label_id(&self, name: &str) -> Option<u16> {
        self.labels.iter().position(|l| l == name).map(|i| i as u16)
    }

    pub fn label_name(&self, id: u16) -> Option<&str> {
        self.labels.get(id as usize).map(String::as_str)
    }

    /// Interns a label name and returns its id, which is stable for the
    /// life of the file because ids are positions and nothing is ever
    /// removed from the dictionary.
    pub fn intern_label(&mut self, name: &str) -> Result<u16> {
        if let Some(id) = self.label_id(name) {
            return Ok(id);
        }
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(ZuError::InvalidArgument(format!(
                "label name length {} out of 1..{MAX_NAME_LEN}",
                name.len()
            )));
        }
        if self.labels.len() == MAX_LABELS {
            return Err(ZuError::Unsupported {
                what: "a label dictionary wider than one bitset word",
                id: MAX_LABELS as u32,
            });
        }
        self.labels.push(name.to_string());
        Ok((self.labels.len() - 1) as u16)
    }

    /// Declares that rows of `table` may carry `label`, interning the
    /// name if the graph has not seen it, and returns its id. Declaring
    /// is what a plan time prune reads: a table that never declared a
    /// label cannot hold a row with it.
    pub fn declare_label(&mut self, table: u32, label: &str) -> Result<u16> {
        let id = self.intern_label(label)?;
        let table = self
            .nodes
            .iter_mut()
            .find(|t| t.id == table)
            .ok_or_else(|| ZuError::InvalidArgument(format!("no node table with id {table}")))?;
        if !table.labels.contains(&id) {
            table.labels.push(id);
        }
        Ok(id)
    }

    /// The graph types this file holds.
    pub fn graph_types(&self) -> &[GraphType] {
        &self.graph_types
    }

    pub fn graph_type(&self, name: &str) -> Option<&GraphType> {
        self.graph_types.iter().find(|t| t.name == name)
    }

    /// The closed graph type one graph is of, which is what a write has
    /// to keep (GG02).
    ///
    /// `None` for a graph created with no type and for one whose type
    /// is open, because both of those describe rather than promise: an
    /// open type admits elements it says nothing about, so there is
    /// nothing a write can break. A named type the file no longer holds
    /// is `None` too, since a promise nothing states is not one a write
    /// can be measured against, and the statement that dropped it is
    /// where that would be caught.
    pub fn closed_type_of(&self, graph: u32) -> Option<&GraphType> {
        let of = &self.graph_by_id(graph)?.graph_type;
        let ty = match of {
            GraphTypeOf::Open => return None,
            GraphTypeOf::Named(name) => self.graph_type(name)?,
            GraphTypeOf::Inline(ty) => ty,
        };
        ty.closed.then_some(ty)
    }

    /// Adds a graph type, refusing a name the file already holds and a
    /// type its own rules reject. The labels it names must already be
    /// in the dictionary, since a type describes the graph it is for
    /// rather than adding to it.
    pub fn add_graph_type(&mut self, ty: GraphType) -> Result<()> {
        if self.graph_type(&ty.name).is_some() {
            return Err(ZuError::InvalidArgument(format!(
                "'{}' is already a graph type",
                ty.name
            )));
        }
        ty.validate(self.labels.len(), Origin::Declared)?;
        self.graph_types.push(ty);
        Ok(())
    }

    /// Drops a graph type, answering whether there was one.
    pub fn drop_graph_type(&mut self, name: &str) -> bool {
        let before = self.graph_types.len();
        self.graph_types.retain(|t| t.name != name);
        self.graph_types.len() != before
    }

    /// The schemas this file holds, the root schema always among them
    /// unless it was dropped.
    pub fn schemas(&self) -> &[String] {
        &self.schemas
    }

    pub fn has_schema(&self, path: &str) -> bool {
        self.schemas.iter().any(|s| s == path)
    }

    /// Adds a schema (GC01), refusing a path that is not absolute and
    /// one the file already holds.
    pub fn add_schema(&mut self, path: &str) -> Result<()> {
        if !path.starts_with('/') || path.len() > MAX_NAME_LEN {
            return Err(ZuError::InvalidArgument(format!(
                "'{path}' is no absolute directory path"
            )));
        }
        if self.has_schema(path) {
            return Err(ZuError::InvalidArgument(format!(
                "'{path}' is already a schema"
            )));
        }
        self.schemas.push(path.to_string());
        Ok(())
    }

    /// Drops a schema, answering whether there was one. A schema that
    /// still holds graphs is refused by the caller: this is the
    /// primitive and the statement is where RESTRICT lives.
    pub fn drop_schema(&mut self, path: &str) -> bool {
        let before = self.schemas.len();
        self.schemas.retain(|s| s != path);
        self.schemas.len() != before
    }

    /// The graphs this file holds.
    pub fn graphs(&self) -> &[GraphDef] {
        &self.graphs
    }

    pub fn graph(&self, schema: &str, name: &str) -> Option<&GraphDef> {
        self.graphs
            .iter()
            .find(|g| g.schema == schema && g.name == name)
    }

    pub fn graph_by_id(&self, id: u32) -> Option<&GraphDef> {
        self.graphs.iter().find(|g| g.id == id)
    }

    /// Everything `add_graph` refuses a graph for other than a name its
    /// schema already holds.
    ///
    /// `CREATE OR REPLACE GRAPH` frees the blocks the old graph held
    /// before the new one is added, and a refusal after that point would
    /// leave a file holding neither, so the caller asks first and frees
    /// nothing when the answer is no.
    pub fn check_graph(&self, schema: &str, graph_type: &GraphTypeOf) -> Result<()> {
        if !self.has_schema(schema) {
            return Err(ZuError::InvalidArgument(format!(
                "'{schema}' is no schema here"
            )));
        }
        if let GraphTypeOf::Named(ty) = graph_type
            && self.graph_type(ty).is_none()
        {
            return Err(ZuError::InvalidArgument(format!(
                "'{ty}' is no graph type here"
            )));
        }
        if let GraphTypeOf::Inline(ty) = graph_type {
            ty.validate(self.labels.len(), Origin::Declared)?;
        }
        self.next_graph_id()?;
        Ok(())
    }

    /// Adds a graph (GC04) and answers its id, refusing a name its
    /// schema already holds and a schema the file does not.
    pub fn add_graph(&mut self, name: &str, schema: &str, graph_type: GraphTypeOf) -> Result<u32> {
        if self.graph(schema, name).is_some() {
            return Err(ZuError::InvalidArgument(format!(
                "'{name}' is already a graph in '{schema}'"
            )));
        }
        self.check_graph(schema, &graph_type)?;
        let id = self.next_graph_id()?;
        self.graphs.push(GraphDef {
            id,
            name: name.to_string(),
            schema: schema.to_string(),
            graph_type,
        });
        Ok(id)
    }

    /// The tables a graph holds, node tables first. Dropping the graph
    /// hands their blocks back, so the caller reads this before the
    /// catalog forgets them.
    pub fn graph_tables(&self, graph: u32) -> Vec<(u32, ElementKind)> {
        let nodes = self
            .nodes
            .iter()
            .filter(|t| t.graph == graph)
            .map(|t| (t.id, ElementKind::Node));
        let rels = self
            .rels
            .iter()
            .filter(|t| t.graph == graph)
            .map(|t| (t.id, ElementKind::Edge));
        nodes.chain(rels).collect()
    }

    /// Copies the table definitions of one graph into another, which is
    /// the catalog half of `CREATE GRAPH ... AS COPY OF` (GC04).
    ///
    /// Each copy keeps the name it had, which is what makes the copy a
    /// copy: a query written against the source runs against the copy
    /// once `USE` names it. A name belongs to a graph rather than to
    /// the file, so both graphs holding a `person` is two tables and
    /// not a clash. What the copies do not keep is their ids, because
    /// an id is what the table index addresses storage by and the copy
    /// gets storage of its own.
    ///
    /// Answers the source id, the copy's id and the kind of each table,
    /// which is what [`crate::graph::copy_graph_storage`] needs to copy
    /// the blocks those ids stand for.
    pub fn copy_graph_tables(
        &mut self,
        source: u32,
        target: u32,
    ) -> Result<Vec<(u32, u32, ElementKind)>> {
        let mut copied = Vec::new();
        // Node tables first, because a rel table names the two node
        // tables it runs between and wants the copies' ids, not the
        // source's, or its edges would land back in the source graph.
        for table in self.nodes.clone() {
            if table.graph != source {
                continue;
            }
            let id = self.next_id()?;
            self.nodes.push(NodeTable {
                id,
                graph: target,
                ..table.clone()
            });
            copied.push((table.id, id, ElementKind::Node));
        }
        let nodes: Vec<(u32, u32)> = copied.iter().map(|&(from, to, _)| (from, to)).collect();
        let mapped = |id: u32| {
            nodes
                .iter()
                .find(|&&(from, _)| from == id)
                .map(|&(_, to)| to)
        };
        for table in self.rels.clone() {
            if table.graph != source {
                continue;
            }
            // Both ends are in the graph being copied, which `validate`
            // holds every catalog to, so a missing one is a file that
            // should never have opened.
            let (Some(from), Some(to)) = (mapped(table.from), mapped(table.to)) else {
                return Err(corrupt(
                    "catalog",
                    format!("rel table '{}' ends outside graph {source}", table.name),
                ));
            };
            let id = self.next_id()?;
            self.rels.push(RelTable {
                id,
                graph: target,
                from,
                to,
                ..table.clone()
            });
            copied.push((table.id, id, ElementKind::Edge));
        }
        Ok(copied)
    }

    /// Drops a graph and every table in it, answering whether there was
    /// one. The storage those tables held is the caller's to free; this
    /// is the catalog half of `DROP GRAPH`.
    pub fn drop_graph(&mut self, id: u32) -> bool {
        let before = self.graphs.len();
        self.graphs.retain(|g| g.id != id);
        if self.graphs.len() == before {
            return false;
        }
        self.nodes.retain(|t| t.graph != id);
        self.rels.retain(|t| t.graph != id);
        true
    }

    /// Graph ids are positions in the order graphs were created and are
    /// never reused, so the id of a dropped graph names nothing rather
    /// than naming the next graph created.
    fn next_graph_id(&self) -> Result<u32> {
        match self.graphs.iter().map(|g| g.id).max() {
            None => Ok(HOME_GRAPH_ID),
            Some(id) if id < u32::MAX => Ok(id + 1),
            Some(_) => Err(ZuError::InvalidArgument(
                "graph id space exhausted".to_string(),
            )),
        }
    }

    /// Puts the home graph back when a `DROP GRAPH home` took it away,
    /// so a load always has somewhere to write. It keeps the id it had:
    /// the tables that named it are gone with it.
    fn ensure_home(&mut self) {
        if self.graph(ROOT_SCHEMA, HOME_GRAPH).is_some() {
            return;
        }
        if !self.has_schema(ROOT_SCHEMA) {
            self.schemas.push(ROOT_SCHEMA.to_string());
        }
        let id = self.next_graph_id().unwrap_or(HOME_GRAPH_ID);
        self.graphs.push(GraphDef {
            id,
            name: HOME_GRAPH.to_string(),
            schema: ROOT_SCHEMA.to_string(),
            graph_type: GraphTypeOf::Open,
        });
    }

    /// The id a table gets when nothing said which graph it is for.
    fn home_id(&mut self) -> u32 {
        self.ensure_home();
        self.home_graph_id()
    }

    /// A closed graph type inferred from the tables a graph holds
    /// (GG04): one node element type per node table keyed on the
    /// table's own label, one edge element type per rel table between
    /// them. It reads the catalog and never the data, so it costs
    /// nothing on a large graph.
    pub fn infer_graph_type(&self, name: &str, graph: u32) -> Result<GraphType> {
        let mut ty = GraphType::closed(name);
        for table in self.nodes.iter().filter(|t| t.graph == graph) {
            ty.elements.push(
                ElementType::node(&table.name, table.labels.clone())
                    .with_key(vec![table.primary_label()]),
            );
        }
        for rel in self.rels.iter().filter(|t| t.graph == graph) {
            let from = self.node_by_id(rel.from).ok_or_else(|| {
                ZuError::InvalidArgument(format!("rel table '{}' has no FROM node table", rel.name))
            })?;
            let to = self.node_by_id(rel.to).ok_or_else(|| {
                ZuError::InvalidArgument(format!("rel table '{}' has no TO node table", rel.name))
            })?;
            // A rel table's name is not a label, so an edge type has an
            // empty label set and is selected by its endpoints.
            ty.elements.push(ElementType::edge(
                &rel.name,
                Vec::new(),
                &from.name,
                &to.name,
            ));
        }
        ty.validate(self.labels.len(), Origin::Stored)?;
        Ok(ty)
    }

    /// Every node table that declares `label`, which is the scan set a
    /// pattern naming that label alone runs over.
    pub fn tables_with_label(&self, label: u16) -> Vec<u32> {
        self.nodes
            .iter()
            .filter(|t| t.labels.contains(&label))
            .map(|t| t.id)
            .collect()
    }

    fn name_taken_by_other_kind(&self, name: &str, node: bool) -> bool {
        if node {
            self.rel_by_name(name).is_some()
        } else {
            self.node_by_name(name).is_some()
        }
    }

    fn next_id(&self) -> Result<u32> {
        let used = self
            .nodes
            .iter()
            .map(|t| t.id)
            .chain(self.rels.iter().map(|t| t.id))
            .max();
        match used {
            None => Ok(0),
            Some(id) if id < MAX_TABLE_ID => Ok(id + 1),
            Some(_) => Err(ZuError::InvalidArgument(format!(
                "table id space exhausted at {MAX_TABLE_ID}"
            ))),
        }
    }

    /// Grows the row domain of the table with this id.
    ///
    /// A fold knows which table it appended to, and the name is not
    /// enough to say which one that is: two graphs in one file each
    /// hold their own `person`, so growing by name would grow the home
    /// graph's table however far from home the rows landed.
    pub fn grow_node(&mut self, id: u32, node_count: u64) -> Result<()> {
        let table = self
            .nodes
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| ZuError::InvalidArgument(format!("no node table has id {id}")))?;
        table.node_count = table.node_count.max(node_count);
        Ok(())
    }

    /// Records how many edges the table with this id holds, which a
    /// fold knows because it just rebuilt it. By id for the same reason
    /// as [`Self::grow_node`].
    pub fn set_edge_count(&mut self, id: u32, edge_count: u64) -> Result<()> {
        let table = self
            .rels
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| ZuError::InvalidArgument(format!("no rel table has id {id}")))?;
        table.edge_count = edge_count;
        Ok(())
    }

    /// Creates or updates a node table and returns its id. The row
    /// domain only grows: a load declaring fewer nodes than an earlier
    /// one leaves the count alone, so rel tables already built over the
    /// larger domain stay valid.
    pub fn upsert_node(&mut self, name: &str, node_count: u64) -> Result<u32> {
        if self.name_taken_by_other_kind(name, true) {
            return Err(ZuError::InvalidArgument(format!(
                "'{name}' is already a rel table"
            )));
        }
        // A load writes into the home graph, so the table it means by a
        // name is the home graph's, and a graph copied from another one
        // holding the same name is a different table.
        let home = self.home_id();
        if let Some(t) = self
            .nodes
            .iter_mut()
            .find(|t| t.graph == home && t.name == name)
        {
            t.node_count = t.node_count.max(node_count);
            return Ok(t.id);
        }
        let id = self.next_id()?;
        // The table's name is a label like any other, and the one every
        // row carries, so it is interned before the table exists and
        // heads the declared set.
        let primary = self.intern_label(name)?;
        let graph = home;
        self.nodes.push(NodeTable {
            id,
            name: name.to_string(),
            graph,
            node_count,
            labels: vec![primary],
        });
        Ok(id)
    }

    /// Creates an empty node table in one graph and returns its id.
    ///
    /// This is `upsert_node` for a caller that says which graph rather
    /// than meaning the home one, and it creates rather than updating:
    /// a name the graph already holds is a table the caller meant to
    /// find, not one to make twice. What it is for is the table an
    /// `INSERT` names by a label nothing declared, which lands in the
    /// graph the statement runs against.
    pub fn create_node_in(&mut self, graph: u32, name: &str) -> Result<u32> {
        if self.graphs.iter().all(|g| g.id != graph) {
            return Err(ZuError::InvalidArgument(format!(
                "no graph with id {graph}"
            )));
        }
        if self.rel_in(graph, name).is_some() {
            return Err(ZuError::InvalidArgument(format!(
                "'{name}' is already a rel table"
            )));
        }
        if self.node_in(graph, name).is_some() {
            return Err(ZuError::InvalidArgument(format!(
                "'{name}' is already a node table"
            )));
        }
        let id = self.next_id()?;
        // The table's name is a label like any other, and the one every
        // row carries, so it is interned before the table exists and
        // heads the declared set.
        let primary = self.intern_label(name)?;
        self.nodes.push(NodeTable {
            id,
            name: name.to_string(),
            graph,
            node_count: 0,
            labels: vec![primary],
        });
        Ok(id)
    }

    /// Creates or updates a rel table and returns its id. The edges are
    /// directed, which is what a rel table has always held; the
    /// undirected form is `upsert_rel_as`.
    pub fn upsert_rel(&mut self, name: &str, from: u32, to: u32, edge_count: u64) -> Result<u32> {
        self.upsert_rel_as(name, from, to, edge_count, false)
    }

    /// Creates or updates a rel table, saying whether its edges have a
    /// direction (GH02).
    ///
    /// Whether the edges are directed belongs to the table rather than
    /// to each edge, because it is a statement about the element type:
    /// a graph type says `(:Peer)-[:FRIEND]-(:Peer)` once and every
    /// edge of that type is the same way round, or is not one way round
    /// at all.
    pub fn upsert_rel_as(
        &mut self,
        name: &str,
        from: u32,
        to: u32,
        edge_count: u64,
        undirected: bool,
    ) -> Result<u32> {
        if self.name_taken_by_other_kind(name, false) {
            return Err(ZuError::InvalidArgument(format!(
                "'{name}' is already a node table"
            )));
        }
        // A rel table is in the graph its FROM table is in, which is
        // the only answer that keeps an edge and the nodes it joins in
        // one graph, and that graph is where the name is looked up.
        let graph = match self.node_by_id(from) {
            Some(table) => table.graph,
            None => self.home_id(),
        };
        if let Some(t) = self
            .rels
            .iter_mut()
            .find(|t| t.graph == graph && t.name == name)
        {
            t.from = from;
            t.to = to;
            t.edge_count = edge_count;
            t.undirected = undirected;
            return Ok(t.id);
        }
        let id = self.next_id()?;
        self.rels.push(RelTable {
            id,
            name: name.to_string(),
            graph,
            from,
            to,
            edge_count,
            undirected,
        });
        Ok(id)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&CATALOG_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.rels.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.labels.len() as u32).to_le_bytes());
        for t in &self.nodes {
            out.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut out, &t.name);
            out.extend_from_slice(&t.graph.to_le_bytes());
            out.extend_from_slice(&t.node_count.to_le_bytes());
            out.extend_from_slice(&(t.labels.len() as u16).to_le_bytes());
            for &label in &t.labels {
                out.extend_from_slice(&label.to_le_bytes());
            }
        }
        for t in &self.rels {
            out.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut out, &t.name);
            out.extend_from_slice(&t.graph.to_le_bytes());
            out.extend_from_slice(&t.from.to_le_bytes());
            out.extend_from_slice(&t.to.to_le_bytes());
            out.extend_from_slice(&t.edge_count.to_le_bytes());
            out.push(u8::from(t.undirected));
        }
        for label in &self.labels {
            encode_name(&mut out, label);
        }
        out.extend_from_slice(&(self.graph_types.len() as u32).to_le_bytes());
        for ty in &self.graph_types {
            encode_graph_type(&mut out, ty);
        }
        out.extend_from_slice(&(self.schemas.len() as u32).to_le_bytes());
        for schema in &self.schemas {
            encode_name(&mut out, schema);
        }
        out.extend_from_slice(&(self.graphs.len() as u32).to_le_bytes());
        for graph in &self.graphs {
            out.extend_from_slice(&graph.id.to_le_bytes());
            encode_name(&mut out, &graph.name);
            encode_name(&mut out, &graph.schema);
            out.push(graph.graph_type.code());
            match &graph.graph_type {
                GraphTypeOf::Open => {}
                GraphTypeOf::Named(name) => encode_name(&mut out, name),
                GraphTypeOf::Inline(ty) => encode_graph_type(&mut out, ty),
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "catalog";
        let head = bytes
            .get(..10)
            .ok_or_else(|| corrupt(WHAT, "truncated header".into()))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        if version == 0 || version > CATALOG_VERSION {
            return Err(ZuError::Unsupported {
                what: "catalog version",
                id: u32::from(version),
            });
        }
        let node_count = u32::from_le_bytes(head[2..6].try_into().unwrap()) as usize;
        let rel_count = u32::from_le_bytes(head[6..10].try_into().unwrap()) as usize;
        let mut pos = 10;
        let label_count = if version >= 2 {
            read_u32(bytes, &mut pos, WHAT)? as usize
        } else {
            0
        };
        let mut catalog = Self::default();
        for _ in 0..node_count {
            let id = read_u32(bytes, &mut pos, WHAT)?;
            let name = decode_name(bytes, &mut pos, WHAT)?;
            let graph = if version >= 4 {
                read_u32(bytes, &mut pos, WHAT)?
            } else {
                HOME_GRAPH_ID
            };
            let node_count = read_u64(bytes, &mut pos, WHAT)?;
            let mut labels = Vec::new();
            if version >= 2 {
                let count = read_u16(bytes, &mut pos, WHAT)? as usize;
                if count > label_count {
                    return Err(corrupt(
                        WHAT,
                        format!("table '{name}' declares {count} of {label_count} labels"),
                    ));
                }
                for _ in 0..count {
                    labels.push(read_u16(bytes, &mut pos, WHAT)?);
                }
            }
            catalog.nodes.push(NodeTable {
                id,
                name,
                graph,
                node_count,
                labels,
            });
        }
        for _ in 0..rel_count {
            let id = read_u32(bytes, &mut pos, WHAT)?;
            let name = decode_name(bytes, &mut pos, WHAT)?;
            let graph = if version >= 4 {
                read_u32(bytes, &mut pos, WHAT)?
            } else {
                HOME_GRAPH_ID
            };
            let from = read_u32(bytes, &mut pos, WHAT)?;
            let to = read_u32(bytes, &mut pos, WHAT)?;
            let edge_count = read_u64(bytes, &mut pos, WHAT)?;
            let undirected = if version >= 5 {
                let byte = bytes
                    .get(pos)
                    .copied()
                    .ok_or_else(|| corrupt(WHAT, "truncated edge direction".into()))?;
                pos += 1;
                match byte {
                    0 => false,
                    1 => true,
                    other => {
                        return Err(corrupt(WHAT, format!("edge direction byte {other}")));
                    }
                }
            } else {
                false
            };
            catalog.rels.push(RelTable {
                id,
                name,
                graph,
                from,
                to,
                edge_count,
                undirected,
            });
        }
        for _ in 0..label_count {
            catalog.labels.push(decode_name(bytes, &mut pos, WHAT)?);
        }
        if version >= 3 {
            let count = read_u32(bytes, &mut pos, WHAT)? as usize;
            for _ in 0..count {
                catalog
                    .graph_types
                    .push(decode_graph_type(bytes, &mut pos)?);
            }
        }
        if version >= 4 {
            // A version 4 file says what its schemas and graphs are, so
            // the ones a default catalog starts with are replaced and
            // not added to. Everything a file wrote comes back as it
            // was written, including a file whose home graph was
            // dropped and which holds no graph at all.
            let count = read_u32(bytes, &mut pos, WHAT)? as usize;
            catalog.schemas = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                catalog.schemas.push(decode_name(bytes, &mut pos, WHAT)?);
            }
            let count = read_u32(bytes, &mut pos, WHAT)? as usize;
            catalog.graphs = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                let id = read_u32(bytes, &mut pos, WHAT)?;
                let name = decode_name(bytes, &mut pos, WHAT)?;
                let schema = decode_name(bytes, &mut pos, WHAT)?;
                let code = bytes
                    .get(pos)
                    .copied()
                    .ok_or_else(|| corrupt(WHAT, "truncated graph type kind".into()))?;
                pos += 1;
                let graph_type = match code {
                    0 => GraphTypeOf::Open,
                    1 => GraphTypeOf::Named(decode_name(bytes, &mut pos, WHAT)?),
                    2 => GraphTypeOf::Inline(decode_graph_type(bytes, &mut pos)?),
                    other => return Err(corrupt(WHAT, format!("graph type kind {other}"))),
                };
                catalog.graphs.push(GraphDef {
                    id,
                    name,
                    schema,
                    graph_type,
                });
            }
        }
        if pos != bytes.len() {
            return Err(corrupt(WHAT, "trailing bytes".into()));
        }
        if version < 2 {
            // A version 1 file is the graph it always was: one label
            // per node table, the table's name, carried by every row.
            // Interning them in table order is what makes reading such
            // a file twice give the same ids.
            for i in 0..catalog.nodes.len() {
                let name = catalog.nodes[i].name.clone();
                let id = catalog.intern_label(&name)?;
                catalog.nodes[i].labels.push(id);
            }
        }
        catalog.validate()?;
        Ok(catalog)
    }

    fn validate(&self) -> Result<()> {
        let mut ids: Vec<u32> = self
            .nodes
            .iter()
            .map(|t| t.id)
            .chain(self.rels.iter().map(|t| t.id))
            .collect();
        for &id in &ids {
            if id > MAX_TABLE_ID {
                return Err(corrupt(
                    "catalog",
                    format!("table id {id} above {MAX_TABLE_ID}"),
                ));
            }
        }
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != total {
            return Err(corrupt("catalog", "duplicate table id".into()));
        }
        // A table's name is a name in its graph, so the pair is what
        // has to be unique. Two graphs may both hold a `person`, which
        // is what a graph copied from another one is made of.
        let mut names: Vec<(u32, &str)> = self
            .nodes
            .iter()
            .map(|t| (t.graph, t.name.as_str()))
            .chain(self.rels.iter().map(|t| (t.graph, t.name.as_str())))
            .collect();
        names.sort_unstable();
        if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(corrupt(
                "catalog",
                format!("graph {} holds two tables called '{}'", w[0].0, w[0].1),
            ));
        }
        if self.labels.len() > MAX_LABELS {
            return Err(corrupt(
                "catalog",
                format!("{} labels above {MAX_LABELS}", self.labels.len()),
            ));
        }
        let mut label_names: Vec<&str> = self.labels.iter().map(String::as_str).collect();
        label_names.sort_unstable();
        if label_names.windows(2).any(|w| w[0] == w[1]) {
            return Err(corrupt("catalog", "duplicate label name".into()));
        }
        for table in &self.nodes {
            // The declared set heads with the table's own name, which is
            // the label every row carries and the one a pattern naming
            // the table is asking for.
            match table.labels.first() {
                Some(&first) if self.label_name(first) == Some(table.name.as_str()) => {}
                _ => {
                    return Err(corrupt(
                        "catalog",
                        format!("node table '{}' does not declare its own name", table.name),
                    ));
                }
            }
            let mut declared = table.labels.clone();
            declared.sort_unstable();
            let total = declared.len();
            declared.dedup();
            if declared.len() != total {
                return Err(corrupt(
                    "catalog",
                    format!("node table '{}' declares a label twice", table.name),
                ));
            }
            if let Some(&worst) = declared.last()
                && worst as usize >= self.labels.len()
            {
                return Err(corrupt(
                    "catalog",
                    format!(
                        "node table '{}' declares label {worst} of {}",
                        table.name,
                        self.labels.len()
                    ),
                ));
            }
        }
        for rel in &self.rels {
            for end in [rel.from, rel.to] {
                let Some(node) = self.node_by_id(end) else {
                    return Err(corrupt(
                        "catalog",
                        format!(
                            "rel table '{}' references missing node table id {end}",
                            rel.name
                        ),
                    ));
                };
                // An edge joins two nodes of the graph it is in. A rel
                // table reaching out of its graph would put rows of
                // another graph in reach of a pattern that never
                // named it.
                if node.graph != rel.graph {
                    return Err(corrupt(
                        "catalog",
                        format!(
                            "rel table '{}' is in graph {} and ends on '{}' in graph {}",
                            rel.name, rel.graph, node.name, node.graph
                        ),
                    ));
                }
            }
        }
        let mut type_names: Vec<&str> = self.graph_types.iter().map(|t| t.name.as_str()).collect();
        type_names.sort_unstable();
        if type_names.windows(2).any(|w| w[0] == w[1]) {
            return Err(corrupt("catalog", "duplicate graph type name".into()));
        }
        for ty in &self.graph_types {
            ty.validate(self.labels.len(), Origin::Stored)?;
        }
        let mut schemas: Vec<&str> = self.schemas.iter().map(String::as_str).collect();
        schemas.sort_unstable();
        if schemas.windows(2).any(|w| w[0] == w[1]) {
            return Err(corrupt("catalog", "duplicate schema path".into()));
        }
        let mut graph_ids: Vec<u32> = self.graphs.iter().map(|g| g.id).collect();
        let total = graph_ids.len();
        graph_ids.sort_unstable();
        graph_ids.dedup();
        if graph_ids.len() != total {
            return Err(corrupt("catalog", "duplicate graph id".into()));
        }
        let mut graph_names: Vec<(&str, &str)> = self
            .graphs
            .iter()
            .map(|g| (g.schema.as_str(), g.name.as_str()))
            .collect();
        graph_names.sort_unstable();
        if graph_names.windows(2).any(|w| w[0] == w[1]) {
            return Err(corrupt("catalog", "duplicate graph name".into()));
        }
        for graph in &self.graphs {
            if !self.has_schema(&graph.schema) {
                return Err(corrupt(
                    "catalog",
                    format!(
                        "graph '{}' is in schema '{}', which this file does not hold",
                        graph.name, graph.schema
                    ),
                ));
            }
            match &graph.graph_type {
                GraphTypeOf::Open => {}
                GraphTypeOf::Named(name) if self.graph_type(name).is_some() => {}
                GraphTypeOf::Named(name) => {
                    return Err(corrupt(
                        "catalog",
                        format!(
                            "graph '{}' is of graph type '{name}', which this file does not hold",
                            graph.name
                        ),
                    ));
                }
                GraphTypeOf::Inline(ty) => ty.validate(self.labels.len(), Origin::Stored)?,
            }
        }
        // A table in no graph is a table nothing can drop, so it is a
        // catalog nobody wrote rather than one to read past.
        for (name, graph) in self
            .nodes
            .iter()
            .map(|t| (&t.name, t.graph))
            .chain(self.rels.iter().map(|t| (&t.name, t.graph)))
        {
            if self.graph_by_id(graph).is_none() {
                return Err(corrupt(
                    "catalog",
                    format!("table '{name}' is in graph {graph}, which this file does not hold"),
                ));
            }
        }
        Ok(())
    }
}

/// The table index: rel table id to group directory root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableIndex {
    entries: Vec<(u32, BlockPtr)>,
}

impl TableIndex {
    /// Loads the committed index; a `NULL_BLOCK` root reads as empty.
    pub fn load(db: &mut Zu1File) -> Result<Self> {
        let root = db.db_header().table_index_root;
        if root == NULL_BLOCK {
            return Ok(Self::default());
        }
        Self::decode(&meta::read_chain(db, root)?)
    }

    pub fn entries(&self) -> &[(u32, BlockPtr)] {
        &self.entries
    }

    pub fn get(&self, table_id: u32) -> Option<BlockPtr> {
        self.entries
            .iter()
            .find(|(id, _)| *id == table_id)
            .map(|&(_, root)| root)
    }

    pub fn set(&mut self, table_id: u32, root: BlockPtr) {
        match self.entries.iter_mut().find(|(id, _)| *id == table_id) {
            Some(entry) => entry.1 = root,
            None => self.entries.push((table_id, root)),
        }
    }

    pub fn remove(&mut self, table_id: u32) {
        self.entries.retain(|(id, _)| *id != table_id);
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&TABLE_INDEX_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for &(id, root) in &self.entries {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&root.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "table index";
        let head = bytes
            .get(..6)
            .ok_or_else(|| corrupt(WHAT, "truncated header".into()))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        if version != TABLE_INDEX_VERSION {
            return Err(ZuError::Unsupported {
                what: "table index version",
                id: u32::from(version),
            });
        }
        let count = u32::from_le_bytes(head[2..6].try_into().unwrap()) as usize;
        // Twelve bytes per entry; a count the payload cannot hold is
        // rejected before it sizes an allocation.
        if count > bytes.len().saturating_sub(6) / 12 {
            return Err(corrupt(WHAT, "truncated entry".into()));
        }
        let mut pos = 6;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let id = read_u32(bytes, &mut pos, WHAT)?;
            let root = read_u64(bytes, &mut pos, WHAT)?;
            if root == NULL_BLOCK {
                return Err(corrupt(
                    WHAT,
                    format!("table {id} has a null directory root"),
                ));
            }
            entries.push((id, root));
        }
        if pos != bytes.len() {
            return Err(corrupt(WHAT, "trailing bytes".into()));
        }
        let mut ids: Vec<u32> = entries.iter().map(|&(id, _)| id).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != entries.len() {
            return Err(corrupt(WHAT, "duplicate table id".into()));
        }
        Ok(Self { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Catalog {
        let mut c = Catalog::default();
        let person = c.upsert_node("person", 500).unwrap();
        let org = c.upsert_node("org", 40).unwrap();
        c.upsert_rel("follows", person, person, 4000).unwrap();
        c.upsert_rel("works_at", person, org, 450).unwrap();
        c
    }

    /// The string type, which is the one every property test here uses
    /// except where the point is that two types differ.
    fn text() -> LogicalType {
        LogicalType::Str {
            min: None,
            max: None,
            fixed: false,
        }
    }

    fn list_of(elem: LogicalType) -> LogicalType {
        LogicalType::List {
            elem: Box::new(elem),
            max: None,
        }
    }

    fn int() -> LogicalType {
        LogicalType::Int {
            signed: true,
            bits: zu_common::IntBits::B64,
            precision: None,
        }
    }

    #[test]
    fn a_graph_type_names_the_elements_a_graph_may_hold() {
        let mut c = sample();
        let person = c.label_id("person").unwrap();
        let employee = c.intern_label("Employee").unwrap();
        let ty = GraphType::closed("company")
            .with(
                ElementType::node("PersonType", vec![person, employee])
                    .with_key(vec![person])
                    .with_property("name", text(), false)
                    .with_property("badge", int(), true),
            )
            .with(ElementType::edge(
                "KnowsType",
                Vec::new(),
                "PersonType",
                "PersonType",
            ));
        c.add_graph_type(ty).unwrap();
        // A name is taken once.
        assert!(
            c.add_graph_type(GraphType::open("company")).is_err(),
            "a graph type name is taken once"
        );
        let ty = c.graph_type("company").unwrap();
        assert!(ty.closed);
        let person_type = ty.element("PersonType").unwrap();
        assert_eq!(person_type.key_labels, KeyLabels::Declared(vec![person]));
        assert!(person_type.property("badge").unwrap().optional);
        assert!(person_type.property("nickname").is_none());
        // The key label set is what selects, so an element carrying
        // both labels is a Person and so is one carrying only the key.
        let mask = |ids: &[u16]| ids.iter().fold(0u64, |m, &l| m | 1 << l);
        assert_eq!(
            ty.types_for(ElementKind::Node, mask(&[person, employee]))
                .len(),
            1
        );
        assert_eq!(ty.types_for(ElementKind::Node, mask(&[person])).len(), 1);
        assert_eq!(ty.types_for(ElementKind::Node, mask(&[employee])).len(), 0);
        // The whole thing survives the round trip, and dropping it
        // leaves the file with the tables it always had.
        assert_eq!(Catalog::decode(&c.encode()).unwrap(), c);
        assert!(c.drop_graph_type("company"));
        assert!(!c.drop_graph_type("company"));
        assert!(c.graph_types().is_empty());
    }

    #[test]
    fn a_key_label_set_is_declared_inferred_or_absent() {
        let mut c = sample();
        let person = c.label_id("person").unwrap();
        let employee = c.intern_label("Employee").unwrap();
        // Not written out, so it is the whole label set.
        let inferred = ElementType::node("A", vec![person, employee]);
        assert_eq!(
            inferred.key_labels,
            KeyLabels::Inferred(vec![person, employee])
        );
        assert_eq!(
            inferred.selection_mask(),
            1 << person | 1 << employee,
            "an inferred key selects on the whole set"
        );
        // Written out, so it is what was written.
        let declared = ElementType::node("B", vec![person, employee]).with_key(vec![employee]);
        assert_eq!(declared.selection_mask(), 1 << employee);
        // Absent, so selection falls back to the whole label set, and
        // a closed graph type will not have it.
        let none = ElementType::node("C", vec![person, employee]).without_key();
        assert_eq!(none.key_labels, KeyLabels::None);
        assert_eq!(none.selection_mask(), 1 << person | 1 << employee);
        c.add_graph_type(GraphType::open("loose").with(none.clone()))
            .unwrap();
        let err = c
            .add_graph_type(GraphType::closed("strict").with(none))
            .expect_err("a closed type needs a key on every element type")
            .to_string();
        assert!(
            err.contains("no key label set in a closed graph type"),
            "{err}"
        );
        // A key has to be part of the label set it keys.
        let err = c
            .add_graph_type(
                GraphType::open("wrong")
                    .with(ElementType::node("D", vec![person]).with_key(vec![employee])),
            )
            .expect_err("a key outside the label set")
            .to_string();
        assert!(err.contains("which it does not carry"), "{err}");
    }

    #[test]
    fn two_element_types_may_share_a_key_and_disagree_about_the_rest() {
        let mut c = sample();
        let person = c.label_id("person").unwrap();
        let org = c.label_id("org").unwrap();
        // GG24 and GG26: the same key label set, different properties,
        // and a property of the same name holding a different type.
        let ty = GraphType::open("relaxed")
            .with(
                ElementType::node("Staff", vec![person])
                    .with_key(vec![person])
                    .with_property("badge", int(), false),
            )
            .with(
                ElementType::node("Guest", vec![person])
                    .with_key(vec![person])
                    .with_property("badge", text(), true)
                    .with_property("tags", list_of(text()), true),
            )
            // GG25: two edge types on one key label set with different
            // endpoints.
            .with(ElementType::node("Org", vec![org]).with_key(vec![org]))
            .with(ElementType::edge("At", Vec::new(), "Staff", "Org"))
            .with(ElementType::edge("With", Vec::new(), "Staff", "Guest"));
        c.add_graph_type(ty).unwrap();
        let ty = c.graph_type("relaxed").unwrap();
        let both = ty.types_for(ElementKind::Node, 1 << person);
        assert_eq!(both.len(), 2, "one key label set, two element types");
        assert_eq!(both[0].property("badge").unwrap().ty, int());
        assert_eq!(both[1].property("badge").unwrap().ty, text());
        // The two edge types differ in where they end, and both stay.
        let edges = ty.types_for(ElementKind::Edge, 0);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].to.as_deref(), Some("Org"));
        assert_eq!(edges[1].to.as_deref(), Some("Guest"));
        let raw = c.encode();
        assert_eq!(Catalog::decode(&raw).unwrap(), c);
        // A list of lists is declarable, since the declared form writes
        // the element with the same function that wrote the list. No
        // column holds one yet, which is a different promise and S2's.
        c.add_graph_type(GraphType::open("deep").with(
            ElementType::node("Nested", vec![person]).with_property(
                "tree",
                list_of(list_of(text())),
                true,
            ),
        ))
        .expect("a list of lists is declarable");
        // A type the declared form has no shape for at all is still
        // refused where it is written and not where it is encoded.
        let err = c
            .add_graph_type(GraphType::open("money").with(
                ElementType::node("Purchase", vec![person]).with_property(
                    "total",
                    LogicalType::Decimal {
                        precision: 12,
                        scale: 2,
                    },
                    true,
                ),
            ))
            .expect_err("a decimal has no declared form")
            .to_string();
        assert!(err.contains("a type this file cannot write"), "{err}");
    }

    #[test]
    fn a_graph_type_is_inferred_from_the_tables_a_file_holds() {
        let c = sample();
        let ty = c.infer_graph_type("like_this", HOME_GRAPH_ID).unwrap();
        assert!(ty.closed, "what the catalog says is all there is");
        let names: Vec<&str> = ty.elements.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["person", "org", "follows", "works_at"]);
        let person = ty.element("person").unwrap();
        assert_eq!(person.kind, ElementKind::Node);
        assert_eq!(
            person.key_labels,
            KeyLabels::Declared(vec![c.label_id("person").unwrap()])
        );
        let works_at = ty.element("works_at").unwrap();
        assert_eq!(works_at.kind, ElementKind::Edge);
        assert_eq!(works_at.from.as_deref(), Some("person"));
        assert_eq!(works_at.to.as_deref(), Some("org"));
    }

    #[test]
    fn a_graph_type_refuses_what_it_cannot_describe() {
        let mut c = sample();
        let person = c.label_id("person").unwrap();
        let cases: Vec<(&str, GraphType)> = vec![
            (
                "declares an element type twice",
                GraphType::open("a")
                    .with(ElementType::node("N", vec![person]))
                    .with(ElementType::node("N", vec![person])),
            ),
            (
                "names label 9",
                GraphType::open("b").with(ElementType::node("N", vec![9])),
            ),
            (
                "declares a property twice",
                GraphType::open("c").with(
                    ElementType::node("N", vec![person])
                        .with_property("p", text(), false)
                        .with_property("p", int(), false),
                ),
            ),
            (
                "which is no node type here",
                GraphType::open("d").with(ElementType::edge("E", Vec::new(), "N", "N")),
            ),
            (
                "is a node type with endpoints",
                GraphType::open("e").with(ElementType {
                    from: Some("N".into()),
                    ..ElementType::node("N", vec![person])
                }),
            ),
        ];
        for (expected, ty) in cases {
            let err = c.add_graph_type(ty).expect_err(expected).to_string();
            assert!(err.contains(expected), "expected {expected}, got {err}");
        }
        assert!(c.graph_types().is_empty(), "nothing broken was kept");
    }

    #[test]
    fn a_graph_type_survives_a_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("types.zu1");
        let mut db = Zu1File::create(&path).unwrap();
        crate::graph::bulk_load_keyed(&mut db, "person", "knows", 4, &[(0, 1), (2, 3)], None)
            .unwrap();
        let mut catalog = Catalog::load(&mut db).unwrap();
        let person = catalog.label_id("person").unwrap();
        catalog
            .add_graph_type(
                GraphType::closed("company").with(
                    ElementType::node("PersonType", vec![person])
                        .with_key(vec![person])
                        .with_property("name", text(), false),
                ),
            )
            .unwrap();
        catalog.store(&mut db).unwrap();
        drop(db);

        crate::verify(&path).unwrap();
        let mut db = Zu1File::open(&path).unwrap();
        let read = Catalog::load(&mut db).unwrap();
        assert_eq!(read, catalog);
        // Storing again gives the blocks the old catalog held back to the
        // free list and takes them straight out again, so a file that is
        // written over and over settles at a size and stays there.
        read.store(&mut db).unwrap();
        read.store(&mut db).unwrap();
        let settled = db.db_header().block_count;
        read.store(&mut db).unwrap();
        read.store(&mut db).unwrap();
        assert_eq!(db.db_header().block_count, settled);
    }

    /// The catalog written the way an older version of the file wrote
    /// it. Each version added a section to the end and version 4 added
    /// a field to every table, so writing one out is the current
    /// encoding with the later parts left off.
    fn encode_at_version(c: &Catalog, version: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&(c.nodes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(c.rels.len() as u32).to_le_bytes());
        out.extend_from_slice(&(c.labels.len() as u32).to_le_bytes());
        for t in &c.nodes {
            out.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut out, &t.name);
            if version >= 4 {
                out.extend_from_slice(&t.graph.to_le_bytes());
            }
            out.extend_from_slice(&t.node_count.to_le_bytes());
            out.extend_from_slice(&(t.labels.len() as u16).to_le_bytes());
            for &label in &t.labels {
                out.extend_from_slice(&label.to_le_bytes());
            }
        }
        for t in &c.rels {
            out.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut out, &t.name);
            if version >= 4 {
                out.extend_from_slice(&t.graph.to_le_bytes());
            }
            out.extend_from_slice(&t.from.to_le_bytes());
            out.extend_from_slice(&t.to.to_le_bytes());
            out.extend_from_slice(&t.edge_count.to_le_bytes());
        }
        for label in &c.labels {
            encode_name(&mut out, label);
        }
        if version >= 3 {
            out.extend_from_slice(&(c.graph_types.len() as u32).to_le_bytes());
            for ty in &c.graph_types {
                encode_graph_type(&mut out, ty);
            }
        }
        if version >= 4 {
            out.extend_from_slice(&(c.schemas.len() as u32).to_le_bytes());
            for schema in &c.schemas {
                encode_name(&mut out, schema);
            }
            out.extend_from_slice(&(c.graphs.len() as u32).to_le_bytes());
            for graph in &c.graphs {
                out.extend_from_slice(&graph.id.to_le_bytes());
                encode_name(&mut out, &graph.name);
                encode_name(&mut out, &graph.schema);
                out.push(graph.graph_type.code());
                match &graph.graph_type {
                    GraphTypeOf::Open => {}
                    GraphTypeOf::Named(name) => encode_name(&mut out, name),
                    GraphTypeOf::Inline(ty) => encode_graph_type(&mut out, ty),
                }
            }
        }
        out
    }

    #[test]
    fn a_version_two_catalog_reads_as_a_file_with_no_graph_types() {
        let mut c = sample();
        c.add_graph_type(GraphType::open("t")).unwrap();
        let read = Catalog::decode(&encode_at_version(&c, 2)).unwrap();
        assert!(read.graph_types().is_empty());
        assert_eq!(read.node_tables(), c.node_tables());
        assert_eq!(read.labels(), c.labels());
    }

    /// A file written before the direction byte existed holds edges
    /// that all point, which is what every rel table was until GH02.
    #[test]
    fn a_version_four_catalog_reads_as_a_file_of_directed_edges() {
        let mut c = sample();
        c.upsert_rel_as("mixed", 0, 0, 3, true).unwrap();
        let read = Catalog::decode(&encode_at_version(&c, 4)).unwrap();
        assert_eq!(read.rel_tables().len(), c.rel_tables().len());
        assert!(read.rel_tables().iter().all(|t| !t.undirected));
        // And the flag survives a round trip at the current version.
        let now = Catalog::decode(&c.encode()).unwrap();
        assert_eq!(now, c);
        assert!(now.rel_by_name("mixed").expect("the table").undirected);
    }

    #[test]
    fn a_version_three_catalog_reads_as_one_schema_and_one_graph() {
        let mut c = sample();
        c.add_graph_type(GraphType::open("t")).unwrap();
        let read = Catalog::decode(&encode_at_version(&c, 3)).unwrap();
        // The file it always was: the root schema, a graph called home
        // with no type on it, and every table in that graph.
        assert_eq!(read.schemas(), [ROOT_SCHEMA]);
        assert_eq!(read.graphs().len(), 1);
        let home = read.graph(ROOT_SCHEMA, HOME_GRAPH).expect("the home graph");
        assert_eq!(home.id, HOME_GRAPH_ID);
        assert_eq!(home.graph_type, GraphTypeOf::Open);
        assert_eq!(read.graph_tables(HOME_GRAPH_ID).len(), 4);
        assert!(read.node_tables().iter().all(|t| t.graph == HOME_GRAPH_ID));
        assert!(read.rel_tables().iter().all(|t| t.graph == HOME_GRAPH_ID));
        assert_eq!(read.graph_types().len(), 1);
        // What a version 3 file holds is what the current encoding
        // holds, so writing it back out and reading it again is the
        // same catalog.
        assert_eq!(Catalog::decode(&read.encode()).unwrap(), read);
    }

    #[test]
    fn roundtrip() {
        let c = sample();
        assert_eq!(Catalog::decode(&c.encode()).unwrap(), c);
        let follows = c.rel_by_name("follows").unwrap();
        assert_eq!(follows.from, c.node_by_name("person").unwrap().id);
        assert_eq!(follows.edge_count, 4000);
    }

    #[test]
    fn upsert_updates_in_place() {
        let mut c = sample();
        let before = c.node_by_name("person").unwrap().id;
        // The row domain only grows.
        assert_eq!(c.upsert_node("person", 100).unwrap(), before);
        assert_eq!(c.node_by_name("person").unwrap().node_count, 500);
        assert_eq!(c.upsert_node("person", 900).unwrap(), before);
        assert_eq!(c.node_by_name("person").unwrap().node_count, 900);
        let rel = c.rel_by_name("follows").unwrap().id;
        assert_eq!(c.upsert_rel("follows", before, before, 9).unwrap(), rel);
        assert_eq!(c.rel_by_name("follows").unwrap().edge_count, 9);
        assert_eq!(c.node_tables().len(), 2);
        assert_eq!(c.rel_tables().len(), 2);
    }

    #[test]
    fn one_namespace_across_kinds() {
        let mut c = sample();
        assert!(c.upsert_node("follows", 1).is_err());
        assert!(c.upsert_rel("person", 0, 0, 1).is_err());
    }

    #[test]
    fn decode_rejects_bad_payloads() {
        let good = sample().encode();
        // Wrong version.
        let mut bad = good.clone();
        bad[0] = 99;
        assert!(matches!(
            Catalog::decode(&bad),
            Err(ZuError::Unsupported { .. })
        ));
        // Truncation at every prefix length must error, never panic.
        for len in 0..good.len() {
            assert!(Catalog::decode(&good[..len]).is_err(), "prefix {len}");
        }
        // Trailing bytes.
        let mut bad = good.clone();
        bad.push(0);
        assert!(Catalog::decode(&bad).is_err());
        // A rel endpoint pointing at a missing node table.
        let mut c = sample();
        c.rels[0].from = 77;
        assert!(Catalog::decode(&c.encode()).is_err());
        // Duplicate ids and names.
        let mut c = sample();
        c.nodes[1].id = c.nodes[0].id;
        assert!(Catalog::decode(&c.encode()).is_err());
        let mut c = sample();
        c.nodes[1].name = c.nodes[0].name.clone();
        assert!(Catalog::decode(&c.encode()).is_err());
        // Id above the 14-bit NodeId field.
        let mut c = sample();
        c.nodes[0].id = MAX_TABLE_ID + 1;
        c.rels.clear();
        assert!(Catalog::decode(&c.encode()).is_err());
    }

    #[test]
    fn a_label_is_declared_once_and_named_by_id() {
        let mut c = sample();
        let person = c.node_by_name("person").unwrap().id;
        let org = c.node_by_name("org").unwrap().id;
        // Every table starts with its own name, which is why the ids
        // here begin at 2.
        assert_eq!(c.labels(), ["person", "org"]);
        assert_eq!(c.declare_label(person, "Employee").unwrap(), 2);
        assert_eq!(c.declare_label(org, "Employee").unwrap(), 2);
        assert_eq!(c.declare_label(person, "Employee").unwrap(), 2);
        assert_eq!(c.node_by_id(person).unwrap().labels, [0, 2]);
        assert_eq!(c.label_id("Employee"), Some(2));
        assert_eq!(c.label_id("Manager"), None);
        assert_eq!(c.label_name(1), Some("org"));
        assert_eq!(c.label_name(9), None);
        assert_eq!(c.tables_with_label(2), [person, org]);
        assert_eq!(c.node_by_id(person).unwrap().label_mask(), 0b101);
        assert!(c.declare_label(person, "").is_err());
        assert!(c.declare_label(4242, "Employee").is_err());
        assert_eq!(Catalog::decode(&c.encode()).unwrap(), c);
        // One word per row is the whole budget, so the dictionary ends
        // at 64 names and says which limit it hit.
        for i in c.labels().len()..MAX_LABELS {
            c.declare_label(person, &format!("L{i}")).unwrap();
        }
        assert!(matches!(
            c.declare_label(person, "one_too_many"),
            Err(ZuError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_version_one_catalog_reads_as_one_label_per_table() {
        // Version 1 had no dictionary, so the bytes end after the rel
        // tables and every table carries its own name and nothing else.
        let c = sample();
        let mut old = 1u16.to_le_bytes().to_vec();
        old.extend_from_slice(&(c.nodes.len() as u32).to_le_bytes());
        old.extend_from_slice(&(c.rels.len() as u32).to_le_bytes());
        for t in &c.nodes {
            old.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut old, &t.name);
            old.extend_from_slice(&t.node_count.to_le_bytes());
        }
        for t in &c.rels {
            old.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut old, &t.name);
            old.extend_from_slice(&t.from.to_le_bytes());
            old.extend_from_slice(&t.to.to_le_bytes());
            old.extend_from_slice(&t.edge_count.to_le_bytes());
        }
        assert_eq!(Catalog::decode(&old).unwrap(), c);
    }

    #[test]
    fn decode_rejects_bad_label_sets() {
        // A table that does not carry its own name first.
        let mut c = sample();
        c.nodes[0].labels[0] = 1;
        assert!(Catalog::decode(&c.encode()).is_err());
        // A label id with no name behind it.
        let mut c = sample();
        c.nodes[0].labels.push(7);
        assert!(Catalog::decode(&c.encode()).is_err());
        // The same label declared twice on one table.
        let mut c = sample();
        c.nodes[0].labels.push(0);
        assert!(Catalog::decode(&c.encode()).is_err());
        // Two names for one id.
        let mut c = sample();
        c.labels[1] = c.labels[0].clone();
        assert!(Catalog::decode(&c.encode()).is_err());
        // A count that outruns the dictionary dies before it allocates.
        let mut c = sample();
        c.nodes[0].labels = vec![0; 300];
        assert!(Catalog::decode(&c.encode()).is_err());
    }

    #[test]
    fn table_index_roundtrip_and_rejects() {
        let mut ix = TableIndex::default();
        ix.set(3, 12);
        ix.set(5, 40);
        ix.set(3, 19);
        assert_eq!(TableIndex::decode(&ix.encode()).unwrap(), ix);
        assert_eq!(ix.get(3), Some(19));
        assert_eq!(ix.get(4), None);
        ix.remove(3);
        assert_eq!(ix.get(3), None);
        assert_eq!(ix.entries().len(), 1);
        // Null root, duplicate id, truncation, trailing bytes.
        let mut bad = TableIndex::default();
        bad.set(1, NULL_BLOCK);
        assert!(TableIndex::decode(&bad.encode()).is_err());
        let raw = {
            let mut ix = TableIndex::default();
            ix.set(1, 7);
            ix.entries.push((1, 8));
            ix.encode()
        };
        assert!(TableIndex::decode(&raw).is_err());
        let good = ix.encode();
        for len in 0..good.len() {
            assert!(TableIndex::decode(&good[..len]).is_err(), "prefix {len}");
        }
        let mut bad = good.clone();
        bad.push(0);
        assert!(TableIndex::decode(&bad).is_err());
        // A six byte header claiming u32::MAX entries must die on the
        // size check, not in the allocator.
        let mut hostile = TABLE_INDEX_VERSION.to_le_bytes().to_vec();
        hostile.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(TableIndex::decode(&hostile).is_err());
    }

    /// A table's name is a name in its graph, so the same name in two
    /// graphs is two tables and the file holds both.
    #[test]
    fn two_graphs_may_both_call_a_table_person() {
        let mut c = sample();
        let other = c
            .add_graph("mirror", ROOT_SCHEMA, GraphTypeOf::Open)
            .unwrap();
        let primary = c.intern_label("person").unwrap();
        let id = c.next_id().unwrap();
        c.nodes.push(NodeTable {
            id,
            name: "person".to_string(),
            graph: other,
            node_count: 3,
            labels: vec![primary],
        });

        let read = Catalog::decode(&c.encode()).unwrap();
        assert_eq!(read.node_in(other, "person").map(|t| t.id), Some(id));
        // The home graph still answers with its own, which is the one
        // every load has been writing into.
        assert_eq!(read.node_by_name("person").map(|t| t.node_count), Some(500));
        assert_eq!(read.graph_tables(other).len(), 1);
    }

    /// The same name twice in one graph is the one thing the pair rule
    /// still refuses.
    #[test]
    fn one_graph_may_not_hold_a_table_name_twice() {
        let mut c = sample();
        let home = c.home_graph_id();
        let primary = c.intern_label("person").unwrap();
        let id = c.next_id().unwrap();
        c.nodes.push(NodeTable {
            id,
            name: "person".to_string(),
            graph: home,
            node_count: 3,
            labels: vec![primary],
        });
        let err = Catalog::decode(&c.encode()).expect_err("two of one name");
        assert!(
            err.to_string().contains("two tables called 'person'"),
            "{err}"
        );
    }

    /// An edge joins two nodes of the graph it is in, and a rel table
    /// reaching out of its graph is a catalog nobody can have written.
    #[test]
    fn a_rel_table_may_not_end_outside_its_graph() {
        let mut c = sample();
        let other = c
            .add_graph("mirror", ROOT_SCHEMA, GraphTypeOf::Open)
            .unwrap();
        let id = c.next_id().unwrap();
        c.rels.push(RelTable {
            id,
            name: "reaches".to_string(),
            graph: other,
            from: 0,
            to: 0,
            edge_count: 1,
            undirected: false,
        });
        let err = Catalog::decode(&c.encode()).expect_err("out of its graph");
        assert!(err.to_string().contains("in graph"), "{err}");
    }
}
