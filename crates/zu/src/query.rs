//! zuQL against an engine catalog: the facade that turns the storage
//! catalog into the binder's `Schema` and runs text through the
//! frontend. The binder itself is engine-agnostic; this is where zu1
//! table definitions become labels and relationship types.

use std::collections::HashMap;

use zu_common::gqlstatus::codes;
use zu_common::{Result, ZuError};
use zu_query::binder::{self, BoundQuery, NodeDef, RelDef, Schema};
use zu_query::exec::{self, DeletedRows, Graph};
use zu_query::{optimizer, parser, plan};

use crate::deleted::Deleted;
use crate::zu1::algo;
use crate::zu1::catalog::Catalog;
use crate::zu1::file::{NULL_BLOCK, Zu1File};
use crate::zu1::graph::{Direction, GraphReader};
use crate::zu1::props::{ListElement, PropsReader, list_elements, load_props, load_props_at};
use zu_common::{FloatBits, LogicalType, Temporal};

/// The types [`run`] speaks, re-exported here so a caller that depends
/// on `zu` alone can bind a parameter and read a row back without also
/// depending on `zu-query`.
pub use zu_query::exec::{QueryResult, Value};
/// The typed view over those rows, from the same crate and for the same
/// reason.
pub use zu_query::row::{Batch, Flow, FromRow, FromValue, Row, RowIter};

/// Builds the binder schema for the home graph, which is the graph a
/// statement is against when nothing said otherwise.
pub fn schema_of(catalog: &Catalog) -> Result<Schema> {
    schema_of_graph(catalog, catalog.home_graph_id())
}

/// Builds the binder schema from one graph of a zu1 catalog. A query
/// sees the tables of the graph it runs against and no others, which
/// is what lets two graphs in one file both hold a `person`.
pub fn schema_of_graph(catalog: &Catalog, graph: u32) -> Result<Schema> {
    let nodes = catalog
        .node_tables()
        .iter()
        .filter(|n| n.graph == graph)
        .map(|n| NodeDef {
            id: n.id,
            name: n.name.clone(),
            node_count: n.node_count,
            labels: n.labels.clone(),
        })
        .collect();
    let rels = catalog
        .rel_tables()
        .iter()
        .filter(|r| r.graph == graph)
        .map(|r| RelDef {
            id: r.id,
            name: r.name.clone(),
            from: r.from,
            to: r.to,
            edge_count: r.edge_count,
            undirected: r.undirected,
        })
        .collect();
    let mut schema = Schema::with_labels(nodes, rels, catalog.labels().to_vec())?;
    // perf/12 §2.4 wants the dual run threshold tunable. Default is
    // 100x; lower it to reach for the robust join order sooner on data
    // whose estimates cannot be trusted.
    if let Some(factor) = std::env::var("ZU_BOUND_DISAGREEMENT")
        .ok()
        .and_then(|f| f.parse().ok())
    {
        schema.set_bound_disagreement(factor);
    }
    Ok(schema)
}

/// Parses and binds one query against a zu1 catalog.
pub fn bind(source: &str, catalog: &Catalog) -> Result<BoundQuery> {
    let parsed = parser::parse(source)?;
    let graph = graph_of(catalog, catalog.home_graph_id(), &parsed)?;
    binder::bind(&parsed, &schema_of_graph(catalog, graph)?)
}

/// Parses, binds, plans, and optimizes one query, returning the
/// EXPLAIN listing of the plan that would execute.
pub fn explain(source: &str, catalog: &Catalog) -> Result<String> {
    let parsed = parser::parse(source)?;
    let graph = graph_of(catalog, catalog.home_graph_id(), &parsed)?;
    let schema = schema_of_graph(catalog, graph)?;
    let query = binder::bind(&parsed, &schema)?;
    let built = plan::build(&query)?;
    let (optimized, notes) = optimizer::optimize_noted(built, &query, &schema)?;
    Ok(noted(notes, plan::explain(&optimized, &query, &schema)))
}

/// Puts the optimizer's notes above a listing, one per line. They go
/// on top because they are about the whole plan and not about any one
/// operator in it.
pub(crate) fn noted(notes: Vec<String>, listing: String) -> String {
    notes
        .into_iter()
        .map(|n| format!("note: {n}\n"))
        .chain([listing])
        .collect()
}

/// The file handle behind a [`Zu1Graph`]: the caller's borrowed handle
/// on the main path, an owned reopen on the fork a morsel worker
/// drives. Both deref to the same [`Zu1File`] surface.
enum Db<'a> {
    Borrowed(&'a mut Zu1File),
    /// `Some` for the graph's whole life; the option only exists so
    /// the drop impl can move the handle out and recycle it into the
    /// file's fork pool instead of paying an OS open per query.
    Owned(Option<Box<Zu1File>>),
}

impl std::ops::Deref for Db<'_> {
    type Target = Zu1File;
    fn deref(&self) -> &Zu1File {
        match self {
            Db::Borrowed(db) => db,
            Db::Owned(db) => db.as_ref().expect("present until drop"),
        }
    }
}

impl std::ops::DerefMut for Db<'_> {
    fn deref_mut(&mut self) -> &mut Zu1File {
        match self {
            Db::Borrowed(db) => db,
            Db::Owned(db) => db.as_mut().expect("present until drop"),
        }
    }
}

impl Drop for Db<'_> {
    fn drop(&mut self) {
        if let Db::Owned(db) = self
            && let Some(db) = db.take()
        {
            db.recycle();
        }
    }
}

/// Turns one word out of a fixed width property column into the value
/// its column's type says it holds. The lane stores 64 bit words and
/// nothing else, so this is the only place that knows a word out of a
/// boolean column is a truth value and a word out of a float column is
/// an IEEE bit pattern.
fn word_value(ty: &LogicalType, word: u64, key: &str) -> Result<Value> {
    Ok(match ty {
        LogicalType::Bool => Value::Bool(word != 0),
        LogicalType::Int { .. } => Value::Int(word as i64),
        LogicalType::Float { bits, .. } => match bits {
            FloatBits::B32 => Value::Float(f64::from(f32::from_bits(word as u32))),
            _ => Value::Float(f64::from_bits(word)),
        },
        // The temporal lanes are counts with a meaning: days since the
        // epoch, nanoseconds since midnight or since the epoch, and
        // months or nanoseconds for the two duration kinds. The lane
        // does not carry a zone, so the zoned types are not among them
        // and say so rather than reading as though they were local.
        LogicalType::Date => Value::Temporal(Temporal::Date(
            i32::try_from(word as i64).map_err(|_| unreadable(ty, key))?,
        )),
        LogicalType::LocalTime => Value::Temporal(Temporal::LocalTime(word as i64)),
        LogicalType::LocalDatetime => Value::Temporal(Temporal::LocalDatetime(word as i64)),
        LogicalType::Duration(kind) => Value::Temporal(Temporal::Duration(*kind, word as i64)),
        other => return Err(unreadable(other, key)),
    })
}

/// A column zu can store but the runtime has no value for yet. The
/// zoned temporal types are stored here before the lane can carry the
/// offset that makes them zoned, so a read of one says what it met
/// rather than handing back a word dressed as an integer.
fn unreadable(ty: &LogicalType, key: &str) -> ZuError {
    ZuError::InvalidArgument(format!(
        "property '{key}' holds {ty}, which this engine cannot yet read into a value"
    ))
}

/// The executor's view of one open zu1 file: readers load lazily per
/// rel table and cache their directories across calls, and props
/// readers load lazily per node table the same way.
pub struct Zu1Graph<'a> {
    db: Db<'a>,
    catalog: Catalog,
    readers: HashMap<u32, GraphReader>,
    props: HashMap<u32, Option<PropsReader>>,
    /// The rows a `DELETE` took away, read on the first query that
    /// asks and kept for the epoch. `None` is "not read yet", so a
    /// graph that is only ever written through never pays the read.
    gone: Option<Deleted>,
}

impl<'a> Zu1Graph<'a> {
    pub fn new(db: &'a mut Zu1File, catalog: Catalog) -> Self {
        Zu1Graph {
            db: Db::Borrowed(db),
            catalog,
            readers: HashMap::new(),
            props: HashMap::new(),
            gone: None,
        }
    }

    /// A graph that owns its handle outright, the shape a [`Session`]
    /// keeps alive across queries so decoded groups and directories
    /// stay warm instead of dying with each call.
    ///
    /// [`Session`]: crate::session::Session
    pub fn owned(db: Zu1File, catalog: Catalog) -> Zu1Graph<'static> {
        Zu1Graph {
            db: Db::Owned(Some(Box::new(db))),
            catalog,
            readers: HashMap::new(),
            props: HashMap::new(),
            gone: None,
        }
    }

    pub fn file(&self) -> &Zu1File {
        &self.db
    }

    pub fn file_mut(&mut self) -> &mut Zu1File {
        &mut self.db
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Swaps in a fresh catalog and drops every cached reader; the
    /// session calls this when the header epoch moves, because the
    /// cached directories and decoded groups describe the old epoch.
    pub fn set_catalog(&mut self, catalog: Catalog) {
        self.catalog = catalog;
        self.readers.clear();
        self.props.clear();
        // A write is what moves the epoch and a delete is a write, so
        // the deleted set belongs to the epoch as much as the
        // directories do.
        self.gone = None;
    }

    fn ensure_reader(&mut self, rel: u32) -> Result<()> {
        if self.readers.contains_key(&rel) {
            return Ok(());
        }
        let name = self
            .catalog
            .rel_by_id(rel)
            .ok_or_else(|| ZuError::InvalidArgument(format!("unknown rel table {rel}")))?
            .name
            .clone();
        let reader = GraphReader::load_table(&mut self.db, &name)?;
        self.readers.insert(rel, reader);
        Ok(())
    }

    fn ensure_props(&mut self, table: u32) -> Result<()> {
        if self.props.contains_key(&table) {
            return Ok(());
        }
        let reader = load_props(&mut self.db, table)?.map(PropsReader::new);
        self.props.insert(table, reader);
        Ok(())
    }

    /// The same for a rel table's edge columns, which hang off its
    /// group directory rather than off the table index. They share the
    /// one map because a catalog id names a node table or a rel table
    /// and never both.
    /// The weighted shortest-path kernel, which needs the rel's CSR and
    /// the rel's property columns at once and so does not fit the
    /// borrow the other table functions share.
    fn sssp_weighted(&mut self, rel: u32, args: &[Value]) -> Result<Vec<Vec<Value>>> {
        let (Some(Value::Int(source)), Some(Value::Str(column))) = (args.first(), args.get(1))
        else {
            return Err(ZuError::InvalidArgument(
                "sssp_weighted needs a source node offset and a weight column name".into(),
            ));
        };
        self.ensure_rel_props(rel)?;
        let Self {
            db, readers, props, ..
        } = self;
        let Some(reader) = props.get_mut(&rel).expect("just loaded") else {
            return Err(ZuError::InvalidArgument(format!(
                "rel table {rel} stores no edge properties, so it has no column '{column}'"
            )));
        };
        let Some(col) = reader.col(column) else {
            return Err(ZuError::InvalidArgument(format!(
                "no edge property column '{column}'"
            )));
        };
        let mut weights = Vec::new();
        reader.read_int_column(db, col, &mut weights)?;
        // Dijkstra settles a node once and never looks at it again,
        // which is only right when no edge can shorten a path that
        // already reached it. A negative weight is that edge, so it is
        // refused rather than answered with a distance that is not the
        // shortest one.
        if let Some(bad) = weights.iter().position(|&w| (w as i64) < 0) {
            return Err(ZuError::InvalidArgument(format!(
                "edge {bad} weighs {}, and a shortest path is not defined over a negative weight",
                weights[bad] as i64
            )));
        }
        let graph = readers
            .get_mut(&rel)
            .expect("the props load read the reader in");
        Ok(algo::sssp_weighted(db, graph, *source as u64, &weights)?
            .into_iter()
            .map(|dist| {
                vec![if dist == u64::MAX {
                    Value::Null
                } else {
                    Value::Int(dist as i64)
                }]
            })
            .collect())
    }

    fn ensure_rel_props(&mut self, rel: u32) -> Result<()> {
        if self.props.contains_key(&rel) {
            return Ok(());
        }
        self.ensure_reader(rel)?;
        let root = self.readers[&rel].directory().props;
        let reader = match root {
            NULL_BLOCK => None,
            root => Some(PropsReader::new(load_props_at(&mut self.db, root)?)),
        };
        self.props.insert(rel, reader);
        Ok(())
    }
}

/// One value out of one row of one column, whatever the column holds.
///
/// Nodes and edges store their properties the same way and differ only
/// in what a row is, so they read them the same way too: this is the
/// whole of it, and the two callers are left with finding the row.
fn column_value(
    db: &mut Zu1File,
    reader: &mut PropsReader,
    col: usize,
    row: u64,
    key: &str,
) -> Result<Value> {
    // A column that holds a null holds a placeholder in the row
    // that is null, so the mask is asked before the value is
    // read and the placeholder never leaves storage.
    if reader.is_nullable(col) && !reader.is_valid(db, col, row)? {
        return Ok(Value::Null);
    }
    let ty = reader.columns()[col].ty.clone();
    if reader.columns()[col].is_lane() {
        let word = reader.read_int(db, col, row)?;
        return word_value(&ty, word, key);
    }
    match ty {
        LogicalType::Str { .. } => {
            let mut bytes = Vec::new();
            reader.read_str(db, col, row, &mut bytes)?;
            let text = String::from_utf8(bytes).map_err(|_| ZuError::Corrupt {
                what: "props column",
                detail: format!("'{key}' row {row} is not UTF-8"),
            })?;
            Ok(Value::Str(text))
        }
        // A stored list comes back as the list value the rest of the
        // engine already has, element by element through the same
        // reading a scalar column of that type gets, so `b.xs` and a
        // list literal are the same value and CARDINALITY cannot tell
        // them apart.
        LogicalType::List { ref elem, .. } => {
            let mut bytes = Vec::new();
            reader.read_str(db, col, row, &mut bytes)?;
            let items = list_elements(elem, &bytes)?;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(match item {
                    ListElement::Word(word) => word_value(elem, word, key)?,
                    ListElement::Blob(bytes) => Value::Str(
                        std::str::from_utf8(bytes)
                            .map_err(|_| ZuError::Corrupt {
                                what: "props column",
                                detail: format!("'{key}' row {row} is not UTF-8"),
                            })?
                            .to_string(),
                    ),
                });
            }
            Ok(Value::List(out))
        }
        other => Err(unreadable(&other, key)),
    }
}

impl Graph for Zu1Graph<'_> {
    fn neighbors(&mut self, rel: u32, node: u64, reversed: bool, out: &mut Vec<u64>) -> Result<()> {
        self.ensure_reader(rel)?;
        out.clear();
        let dir = if reversed {
            Direction::Bwd
        } else {
            Direction::Fwd
        };
        let Self { db, readers, .. } = self;
        // The cached-group path, not the point path: scans and expands
        // revisit the same groups constantly, and B4 lives on the second
        // hop being a slice copy instead of a chunk decode per row. The
        // reader holds one decoded group per direction of use; a smarter
        // policy is the buffer manager's job (docs/09, M3).
        let nbrs = readers
            .get_mut(&rel)
            .expect("just loaded")
            .neighbors_dir(db, node, dir)?;
        out.extend_from_slice(nbrs);
        Ok(())
    }

    fn degree(&mut self, rel: u32, node: u64, reversed: bool) -> Result<u64> {
        self.ensure_reader(rel)?;
        let dir = if reversed {
            Direction::Bwd
        } else {
            Direction::Fwd
        };
        let Self { db, readers, .. } = self;
        // A degree is the difference of two pooled offsets; neither the
        // neighbor values nor the offset array get copied or re-decoded
        // when the group is warm.
        readers
            .get(&rel)
            .expect("just loaded")
            .degree_of(db, node, dir)
    }

    fn degree_sum(&mut self, rel: u32, nodes: &[u64], reversed: bool) -> Result<u64> {
        self.ensure_reader(rel)?;
        let dir = if reversed {
            Direction::Bwd
        } else {
            Direction::Fwd
        };
        let Self { db, readers, .. } = self;
        // One reader lookup for the whole vector, then the offsets-only
        // batch: a counting expand touches the offsets pool and never
        // decodes a neighbor value.
        readers
            .get_mut(&rel)
            .expect("just loaded")
            .degree_batch(db, nodes, dir)
    }

    fn has_edge(&mut self, rel: u32, src: u64, dst: u64) -> Result<bool> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        readers
            .get(&rel)
            .expect("just loaded")
            .has_edge(db, src, dst)
    }

    fn property(&mut self, table: u32, offset: u64, key: &str) -> Result<Value> {
        self.ensure_props(table)?;
        let Self { db, props, .. } = self;
        if let Some(reader) = props.get_mut(&table).expect("just loaded")
            && let Some(col) = reader.col(key)
        {
            return column_value(db, reader, col, offset, key);
        }
        // Without a stored `id` column the id is the offset, the dense
        // contract every load without REORDER keeps.
        match key {
            "id" => Ok(Value::Int(offset as i64)),
            other => Err(ZuError::InvalidArgument(format!(
                "unknown property '{other}' on table {table}"
            ))),
        }
    }

    fn labels(&mut self, table: u32, offset: u64) -> Result<u64> {
        self.ensure_props(table)?;
        // A table whose rows all carry its own label and nothing else
        // stores no bitset, and the catalog is then the whole answer.
        let primary = self
            .catalog
            .node_by_id(table)
            .map_or(0, |t| 1 << t.primary_label());
        let Self { db, props, .. } = self;
        let word = match props.get_mut(&table).expect("just loaded") {
            Some(reader) => reader.label_word(db, offset)?,
            None => None,
        };
        Ok(word.unwrap_or(primary))
    }

    fn rel_property(&mut self, rel: u32, ord: u64, key: &str) -> Result<Value> {
        self.ensure_rel_props(rel)?;
        let Self { db, props, .. } = self;
        let Some(reader) = props.get_mut(&rel).expect("just loaded") else {
            return Ok(Value::Null);
        };
        let Some(col) = reader.col(key) else {
            return Ok(Value::Null);
        };
        // The row arrived with the value: the operator that matched the
        // edge counted it out of the adjacency list, so a pair that
        // runs more than once reads each copy's own column entry rather
        // than the first one's for all of them.
        column_value(db, reader, col, ord, key)
    }

    fn edge_ordinal(&mut self, rel: u32, src: u64, dst: u64) -> Result<Option<u64>> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        readers
            .get_mut(&rel)
            .expect("just loaded")
            .edge_ordinal(db, src, dst)
    }

    fn edge_run(&mut self, rel: u32, src: u64, dst: u64) -> Result<Option<(u64, u64)>> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        readers
            .get_mut(&rel)
            .expect("just loaded")
            .edge_run(db, src, dst)
    }

    fn neighbor_ordinals(
        &mut self,
        rel: u32,
        node: u64,
        reversed: bool,
        len: usize,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let _ = len;
        self.ensure_reader(rel)?;
        let dir = if reversed {
            Direction::Bwd
        } else {
            Direction::Fwd
        };
        let Self { db, readers, .. } = self;
        readers
            .get_mut(&rel)
            .expect("just loaded")
            .neighbor_ordinals_into(db, node, dir, out)
    }

    fn lookup_key(&mut self, table: u32, key: u64) -> Result<Option<u64>> {
        // The primary-key index lives in the group directory of a rel
        // table loaded over this node table's rows, so find one and ask
        // it. A table with no keyed rel keeps the dense contract where
        // the id is the offset.
        let Some(rel) = self
            .catalog
            .rel_tables()
            .iter()
            .find(|r| r.from == table)
            .map(|r| r.id)
        else {
            return Ok(Some(key));
        };
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        let reader = readers.get_mut(&rel).expect("just loaded");
        if reader.directory().keys.is_none() {
            return Ok(Some(key));
        }
        reader.lookup_key(db, key)
    }

    fn deleted(&mut self) -> Result<DeletedRows> {
        if self.gone.is_none() {
            self.gone = Some(Deleted::load(&mut self.db)?);
        }
        Ok(self.gone.as_ref().expect("just loaded").tables().clone())
    }

    fn fork(&self) -> Option<Box<dyn Graph + Send>> {
        // Data blocks hit the file as they are staged and only the
        // header flip waits for the checkpoint, so a reopen carrying
        // this handle's in-memory header reads exactly what this
        // handle reads. The fork starts with cold reader caches and
        // warms its own; a worker sweeps its own morsels' groups, so
        // sharing decoded state would only add contention.
        let db = self.db.reopen().ok()?;
        Some(Box::new(Zu1Graph {
            db: Db::Owned(Some(Box::new(db))),
            catalog: self.catalog.clone(),
            readers: HashMap::new(),
            props: HashMap::new(),
            gone: self.gone.clone(),
        }))
    }

    fn table_function(&mut self, name: &str, rel: u32, args: &[Value]) -> Result<Vec<Vec<Value>>> {
        if name == "sssp_weighted" {
            return self.sssp_weighted(rel, args);
        }
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        let reader = readers.get_mut(&rel).expect("just loaded");
        match name {
            "pagerank" => Ok(algo::pagerank_converged(db, reader)?
                .into_iter()
                .map(|rank| vec![Value::Float(rank)])
                .collect()),
            "wcc" => Ok(algo::wcc(db, reader)?
                .into_iter()
                .map(|label| vec![Value::Int(label as i64)])
                .collect()),
            "bfs" => {
                let Some(Value::Int(source)) = args.first() else {
                    return Err(ZuError::InvalidArgument(
                        "bfs needs a source node offset".into(),
                    ));
                };
                Ok(algo::bfs(db, reader, *source as u64)?
                    .into_iter()
                    .map(|level| {
                        vec![if level == u64::MAX {
                            Value::Null
                        } else {
                            Value::Int(level as i64)
                        }]
                    })
                    .collect())
            }
            "sssp" => {
                let Some(Value::Int(source)) = args.first() else {
                    return Err(ZuError::InvalidArgument(
                        "sssp needs a source node offset".into(),
                    ));
                };
                Ok(algo::sssp(db, reader, *source as u64)?
                    .into_iter()
                    .map(|dist| {
                        vec![if dist == u64::MAX {
                            Value::Null
                        } else {
                            Value::Int(dist as i64)
                        }]
                    })
                    .collect())
            }
            "cdlp" => {
                let rounds = match args.first() {
                    Some(Value::Int(rounds)) if *rounds >= 0 => *rounds as usize,
                    Some(other) => {
                        return Err(ZuError::InvalidArgument(format!(
                            "cdlp's round count must be a non-negative integer, got {other:?}"
                        )));
                    }
                    None => algo::CDLP_ROUNDS,
                };
                Ok(algo::cdlp(db, reader, rounds)?
                    .into_iter()
                    .map(|label| vec![Value::Int(label as i64)])
                    .collect())
            }
            "lcc" => Ok(algo::lcc(db, reader)?
                .into_iter()
                .map(|coeff| vec![Value::Float(coeff)])
                .collect()),
            "betweenness" => {
                let Some(Value::List(sources)) = args.first() else {
                    return Err(ZuError::InvalidArgument(
                        "betweenness needs a list of source node offsets".into(),
                    ));
                };
                let mut offsets = Vec::with_capacity(sources.len());
                for source in sources {
                    let Value::Int(offset) = source else {
                        return Err(ZuError::InvalidArgument(format!(
                            "betweenness's sources must be node offsets, got {source:?}"
                        )));
                    };
                    offsets.push(*offset as u64);
                }
                Ok(algo::betweenness(db, reader, &offsets)?
                    .into_iter()
                    .map(|score| vec![Value::Float(score)])
                    .collect())
            }
            "triangle_count" => Ok(algo::triangle_count(db, reader)?
                .into_iter()
                .map(|corners| vec![Value::Int(corners as i64)])
                .collect()),
            "louvain" => Ok(algo::louvain(db, reader)?
                .into_iter()
                .map(|label| vec![Value::Int(label as i64)])
                .collect()),
            other => Err(ZuError::InvalidArgument(format!(
                "zu1 has no table function '{other}'"
            ))),
        }
    }
}

/// Everything a query needs before touching graph data: the optimized
/// plan, the bound query, and the parameter values in binder order.
struct Prepared {
    catalog: Catalog,
    schema: Schema,
    query: BoundQuery,
    plan: plan::LogicalPlan,
    args: Vec<Value>,
    /// What the optimizer wants EXPLAIN to say that the tree does not.
    notes: Vec<String>,
}

/// Loads the catalog and stats chains from disk and builds the binder
/// schema with the optimizer's statistics attached. This is the whole
/// per-query disk cost of the one-shot entry points; a [`Session`]
/// calls it once and then only when the header epoch moves.
///
/// [`Session`]: crate::session::Session
pub(crate) fn load_schema(db: &mut Zu1File) -> Result<(Catalog, Schema)> {
    let catalog = Catalog::load(db)?;
    let schema = schema_with_stats(db, &catalog, catalog.home_graph_id())?;
    Ok((catalog, schema))
}

/// The same schema for one graph of an already loaded catalog, which
/// is what a `USE` of a second graph needs. The statistics are keyed
/// by table id and a graph's tables are a subset of the file's, so the
/// ones this schema has no table for are simply never asked for.
pub(crate) fn schema_with_stats(db: &mut Zu1File, catalog: &Catalog, graph: u32) -> Result<Schema> {
    let mut schema = schema_of_graph(catalog, graph)?;
    // The stats chain feeds the optimizer's degree histograms; a file
    // written before stats existed simply attaches nothing and every
    // estimate falls back to the count ratios.
    let stats = crate::zu1::stats::Stats::load(db)?;
    schema.set_color_summaries(
        stats
            .rels
            .iter()
            .filter_map(|(id, r)| {
                let c = r.colors.as_ref()?;
                Some((
                    *id,
                    binder::ColorSummary {
                        counts: c.counts.clone(),
                        triples: c.triples.clone(),
                        epoch: c.epoch,
                        edges: c.edges,
                    },
                ))
            })
            .collect(),
    );
    schema.set_col_stats(
        stats
            .cols
            .iter()
            .map(|(id, cols)| {
                let cols = cols
                    .iter()
                    .map(|(name, c)| {
                        (
                            name.clone(),
                            binder::ColStats {
                                rows: c.rows,
                                ndv: c.ndv,
                                top: c.top.clone(),
                                bounds: c.bounds.clone(),
                            },
                        )
                    })
                    .collect();
                (*id, cols)
            })
            .collect(),
    );
    let norm = |n: crate::zu1::stats::DegreeNorms| binder::DegreeNorms {
        l1: n.l1,
        l2: n.l2,
        l3: n.l3,
        linf: n.linf,
    };
    schema.set_degree_norms(
        stats
            .rels
            .iter()
            .map(|(id, r)| {
                let s = binder::DegreeStats {
                    out: norm(r.norms.out),
                    inn: norm(r.norms.inn),
                    cross: r.norms.cross,
                };
                (*id, s)
            })
            .collect(),
    );
    schema.set_degree_hists(
        stats
            .rels
            .into_iter()
            .map(|(id, r)| (id, [r.out_hist, r.in_hist]))
            .collect(),
    );
    Ok(schema)
}

/// The graph a parsed query is against, given the graph the caller is
/// working in. A `USE` naming a graph the catalog does not hold is a
/// reference that resolves to nothing, which is what `42002` says.
pub(crate) fn graph_of(
    catalog: &Catalog,
    working: u32,
    query: &zu_query::ast::Query,
) -> Result<u32> {
    use zu_query::ast::GraphRef;
    let Some(GraphRef::Named(name)) = &query.use_graph else {
        return Ok(working);
    };
    let schema = name.schema.as_deref().unwrap_or("/");
    catalog
        .graph(schema, &name.name)
        .map(|g| g.id)
        .ok_or_else(|| {
            ZuError::gql(
                codes::C42002,
                format!("USE names '{}', which is no graph in '{schema}'", name.name),
            )
        })
}

/// Binds, plans, and optimizes one parsed query against a schema.
/// Everything here depends only on the query and the schema, so the
/// result is what a plan cache stores. It takes the parse rather than
/// the text because the caller had to read the `USE` clause to know
/// which graph's schema to compile against.
pub(crate) fn compile_parsed(
    parsed: &zu_query::ast::Query,
    schema: &Schema,
) -> Result<(BoundQuery, plan::LogicalPlan, Vec<String>)> {
    let query = binder::bind(parsed, schema)?;
    let built = plan::build(&query)?;
    let (plan, notes) = optimizer::optimize_noted(built, &query, schema)?;
    Ok((query, plan, notes))
}

/// Resolves caller parameters against the binder's parameter order.
///
/// A parameter the caller did not supply is a reference in the
/// statement that resolves to nothing, which is what `42002 invalid
/// reference` is for. It used to come back as an engine-internal
/// invalid argument with no condition at all, which left a client
/// unable to tell a statement it got wrong from an engine that broke.
pub(crate) fn bind_args(names: &[String], params: &[(&str, Value)]) -> Result<Vec<Value>> {
    let mut args = Vec::with_capacity(names.len());
    for name in names {
        match params.iter().find(|(n, _)| n == name) {
            Some((_, v)) => args.push(v.clone()),
            None => {
                return Err(ZuError::gql(
                    codes::C42002,
                    format!("missing parameter ${name}"),
                ));
            }
        }
    }
    Ok(args)
}

/// A statement that is not a query, which is a statement with no
/// binding table and no plan.
pub(crate) enum NotAQuery {
    /// One that changes what the file declares.
    Catalog(zu_query::ast::CatalogStmt),
    /// One that says where a transaction begins or ends.
    Transaction(zu_query::ast::TxnStmt),
}

/// What this source is when it is not a query, `None` when it is one.
///
/// Every entry point that takes statement text checks this first: these
/// statements have no binding table and no plan, so a caller that sent
/// one and got "expected MATCH" back would be told the wrong thing.
pub(crate) fn not_a_query(source: &str) -> Result<Option<NotAQuery>> {
    match zu_query::parser::parse_statement(source)? {
        zu_query::ast::Statement::Catalog(stmt) => Ok(Some(NotAQuery::Catalog(stmt))),
        zu_query::ast::Statement::Transaction(stmt) => Ok(Some(NotAQuery::Transaction(stmt))),
        zu_query::ast::Statement::Query(_) => Ok(None),
    }
}

fn prepare(source: &str, db: &mut Zu1File, params: &[(&str, Value)]) -> Result<Prepared> {
    let catalog = Catalog::load(db)?;
    let parsed = parser::parse(source)?;
    // A one-shot call has no session, so the graph it works in is the
    // home graph and a `USE` is the only way to name another one.
    let graph = graph_of(&catalog, catalog.home_graph_id(), &parsed)?;
    let schema = schema_with_stats(db, &catalog, graph)?;
    let (query, plan, notes) = compile_parsed(&parsed, &schema)?;
    // A write needs the log and the overlay a session owns, and this
    // entry point has neither: it was given a file handle and it hands
    // it back. Saying so here is better than compiling a plan whose
    // written elements nobody made.
    if query.clauses.iter().any(|c| {
        matches!(
            c,
            zu_query::binder::BoundClause::Insert { .. }
                | zu_query::binder::BoundClause::Set { .. }
                | zu_query::binder::BoundClause::Delete { .. }
        )
    }) {
        return Err(ZuError::InvalidArgument(
            "a statement that writes needs a session, which owns the log a write goes through: open one with zu::db::Database or zu::session::Session".into(),
        ));
    }
    let args = bind_args(&query.params, params)?;
    Ok(Prepared {
        catalog,
        schema,
        query,
        plan,
        args,
        notes,
    })
}

/// Parses, plans, optimizes, and executes one query against an open
/// zu1 file, returning the result rows. Scan-driven stages run on the
/// morsel scheduler with `min(cores, 8)` workers; `ZU_THREADS` in the
/// environment overrides the count, `ZU_THREADS=1` forces sequential
/// execution.
pub fn run(source: &str, db: &mut Zu1File, params: &[(&str, Value)]) -> Result<QueryResult> {
    match not_a_query(source)? {
        Some(NotAQuery::Catalog(stmt)) => {
            crate::catalog_stmt::apply(db, &stmt)?;
            return Ok(QueryResult::new(Vec::new(), Vec::new()));
        }
        // A transaction is several statements held together, and this
        // entry point runs one statement against a file handle it hands
        // straight back, so there is nothing here for the next
        // statement to be held together with.
        Some(NotAQuery::Transaction(_)) => {
            return Err(ZuError::InvalidArgument(
                "a transaction runs across statements, which needs a session: open one with zu::db::Database or zu::session::Session".into(),
            ));
        }
        None => {}
    }
    let p = prepare(source, db, params)?;
    let options = env_options();
    if exec2_enabled() {
        let mut snap = crate::snapshot::Zu1Snapshot::new(db, p.catalog.clone());
        if let Some(r) =
            zu_exec::try_execute(&p.plan, &p.query, &p.schema, &mut snap, &p.args, &options)?
        {
            return Ok(r);
        }
    }
    let mut graph = Zu1Graph::new(db, p.catalog);
    exec::execute(&p.plan, &p.query, &p.schema, &mut graph, &p.args, &options)
}

/// Whether plans the pipeline executor covers run there. On by
/// default; `ZU_EXEC2=0` pins every query to the old executor, which
/// is how the differential tests get their oracle rows.
pub(crate) fn exec2_enabled() -> bool {
    std::env::var("ZU_EXEC2").as_deref() != Ok("0")
}

/// The execution options both entry points honor, so a profile always
/// describes the plan `run` would execute under the same environment.
pub(crate) fn env_options() -> exec::Options {
    let mut options = exec::Options::default();
    if let Some(threads) = std::env::var("ZU_THREADS")
        .ok()
        .and_then(|t| t.parse().ok())
    {
        options.threads = threads;
    }
    // The optimizer marks cyclic closes on its own; ZU_WCOJ stays as
    // a manual override, 1 forcing the fusion wherever it fits and 0
    // pinning the binary join for baseline comparisons.
    options.wcoj = match std::env::var("ZU_WCOJ").as_deref() {
        Ok("1") => exec::Wcoj::Force,
        Ok("0") => exec::Wcoj::Off,
        _ => exec::Wcoj::Auto,
    };
    // A join publishes a filter over its build keys unless ZU_SIP=0
    // says otherwise, which is how a run is measured against the same
    // plan without one.
    options.sip = match std::env::var("ZU_SIP").as_deref() {
        Ok("0") => exec::Sip::Off,
        _ => exec::Sip::On,
    };
    options
}

/// Executes one query with per-operator counters and returns the
/// rendered EXPLAIN ANALYZE listing: pulls, rows, the estimate and its
/// q-error, the average vector length, and self time per operator, per
/// stage. The grammar has no EXPLAIN keyword yet, so this is the API
/// entry point.
pub fn explain_analyze(source: &str, db: &mut Zu1File, params: &[(&str, Value)]) -> Result<String> {
    let (profile, notes) = profile_noted(source, db, params)?;
    let listing = match decisions(source, db, params)? {
        Some(d) => format!("{}decisions:\n{}", profile.render(), d.render()),
        None => profile.render(),
    };
    Ok(noted(notes, listing))
}

/// The decisions the pipeline executor made on this query, `None` when
/// it is not the engine that would run it. This is a second run of the
/// query, on the other engine, because the counters above come from the
/// old executor and the record below is only the new one's to keep.
/// EXPLAIN ANALYZE is a debugging tool and can afford the second run;
/// nothing on the answering path pays for it.
fn decisions(
    source: &str,
    db: &mut Zu1File,
    params: &[(&str, Value)],
) -> Result<Option<zu_exec::decide::Decisions>> {
    if !exec2_enabled() {
        return Ok(None);
    }
    let p = prepare(source, db, params)?;
    let mut snap = crate::snapshot::Zu1Snapshot::new(db, p.catalog.clone());
    let run = zu_exec::try_execute_profiled(
        &p.plan,
        &p.query,
        &p.schema,
        &mut snap,
        &p.args,
        &env_options(),
    )?;
    Ok(run.map(|(_, d)| d))
}

/// The same profiled run handing back the counters instead of the
/// rendering. The cardinality phase of the LDBC bench reads q-error
/// off this (perf/12 §4).
pub fn profile(source: &str, db: &mut Zu1File, params: &[(&str, Value)]) -> Result<exec::Profile> {
    Ok(profile_noted(source, db, params)?.0)
}

/// The profiled run plus the optimizer's notes, which the rendering
/// wants and the bench does not.
fn profile_noted(
    source: &str,
    db: &mut Zu1File,
    params: &[(&str, Value)],
) -> Result<(exec::Profile, Vec<String>)> {
    let p = prepare(source, db, params)?;
    let mut graph = Zu1Graph::new(db, p.catalog);
    let (_, profile) = exec::execute_profiled(
        &p.plan,
        &p.query,
        &p.schema,
        &mut graph,
        &p.args,
        &env_options(),
    )?;
    Ok((profile, p.notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zu1::file::Zu1File;
    use crate::zu1::graph;

    #[test]
    fn binds_against_a_real_zu1_catalog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bind.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let catalog = Catalog::load(&mut db).expect("catalog");
        let q = bind(
            "MATCH (a:person {id: $src})-[:follows]->(b) \
             RETURN b.id AS friend ORDER BY friend LIMIT 10",
            &catalog,
        )
        .expect("bind");
        assert_eq!(q.params, ["src"]);
        assert_eq!(q.columns, ["friend"]);
        let a = q.variables.iter().find(|v| v.name == "a").expect("a");
        let person = catalog.node_by_name("person").expect("person").id;
        assert_eq!(a.node_tables, [person]);

        let err = bind("MATCH (a:nope) RETURN a", &catalog).expect_err("unknown label");
        assert!(err.to_string().contains("unknown label"), "got: {err}");

        let text = explain(
            "MATCH (a:person {id: $src})-[:follows]->(b) RETURN b.id AS friend",
            &catalog,
        )
        .expect("explain");
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        assert_eq!(
            lines,
            [
                "Project b.id AS friend",
                "Expand (a)-[#1:follows]->(b)",
                "Filter a.id = $src",
                "ScanNodes a: person",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn runs_queries_on_a_real_zu1_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("run.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let src = 3u32;

        let mut friends: Vec<i64> = edges
            .iter()
            .filter(|(s, _)| *s == src)
            .map(|(_, d)| i64::from(*d))
            .collect();
        friends.sort_unstable();
        let r = run(
            "MATCH (a:person {id: $src})-[:follows]->(b) \
             RETURN b.id AS friend ORDER BY friend",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("one hop");
        assert_eq!(r.columns, ["friend"]);
        let got: Vec<i64> = r
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Int(i) => *i,
                other => panic!("expected an int, got {other:?}"),
            })
            .collect();
        assert_eq!(got, friends);

        let two_hop: i64 = edges
            .iter()
            .filter(|(s, _)| *s == src)
            .map(|(_, mid)| edges.iter().filter(|(s, _)| s == mid).count() as i64)
            .sum();
        let r = run(
            "MATCH (a:person {id: $src})-[:follows]->(b)-[:follows]->(c) \
             RETURN count(c) AS paths",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("two hop count");
        assert_eq!(r.rows, [[Value::Int(two_hop)]]);

        let undirected = edges.iter().filter(|(s, d)| *s == src || *d == src).count() as i64;
        let r = run(
            "MATCH (a:person {id: $src})-[:follows]-(b) RETURN count(b) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("undirected count");
        assert_eq!(r.rows, [[Value::Int(undirected)]]);

        // Trails of one or two hops: a second edge may repeat a node
        // but never the first edge.
        let mut trails = 0i64;
        for &(s1, d1) in &edges {
            if s1 != src {
                continue;
            }
            trails += 1;
            for &(s2, d2) in &edges {
                if s2 == d1 && (s2, d2) != (s1, d1) {
                    trails += 1;
                }
            }
        }
        let r = run(
            "MATCH (a:person {id: $src})-[:follows*1..2]->(b) RETURN count(b) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("var-length count");
        assert_eq!(r.rows, [[Value::Int(trails)]]);

        // ANY SHORTEST against a BFS levels oracle: one row per
        // reached node, hops equal to its level.
        let mut level = vec![u32::MAX; 97];
        level[src as usize] = 0;
        let mut frontier = vec![src];
        let mut depth = 0u32;
        while !frontier.is_empty() {
            depth += 1;
            let mut next = Vec::new();
            for &(s, t) in &edges {
                if frontier.contains(&s) && level[t as usize] == u32::MAX {
                    level[t as usize] = depth;
                    next.push(t);
                }
            }
            frontier = next;
        }
        let reached: Vec<u32> = (0..97u32)
            .filter(|&v| v != src && level[v as usize] != u32::MAX)
            .collect();
        let hop_sum: i64 = reached.iter().map(|&v| i64::from(level[v as usize])).sum();
        let r = run(
            "MATCH ANY SHORTEST (a:person {id: $src})-[r:follows*]->(b) \
             RETURN count(b) AS n, sum(size(r)) AS hops",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("any shortest");
        assert_eq!(
            r.rows,
            [[Value::Int(reached.len() as i64), Value::Int(hop_sum)]]
        );

        // ALL SHORTEST against a path-counting oracle: dynamic
        // programming over the shortest-path DAG the levels induce.
        let mut ways = vec![0i64; 97];
        ways[src as usize] = 1;
        let mut order = reached.clone();
        order.sort_unstable_by_key(|&v| level[v as usize]);
        for &v in &order {
            ways[v as usize] = edges
                .iter()
                .filter(|(s, t)| {
                    *t == v
                        && level[*s as usize] != u32::MAX
                        && level[*s as usize] + 1 == level[v as usize]
                })
                .map(|(s, _)| ways[*s as usize])
                .sum();
        }
        let shortest_paths: i64 = reached.iter().map(|&v| ways[v as usize]).sum();
        let r = run(
            "MATCH ALL SHORTEST (a:person {id: $src})-[r:follows*]->(b) \
             RETURN count(b) AS paths",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("all shortest");
        assert_eq!(r.rows, [[Value::Int(shortest_paths)]]);

        // WALK against an adjacency power oracle: walks of exactly
        // three hops, edge and node repeats allowed.
        let mut at = vec![0i64; 97];
        at[src as usize] = 1;
        for _ in 0..3 {
            let mut next = vec![0i64; 97];
            for &(s, t) in &edges {
                next[t as usize] += at[s as usize];
            }
            at = next;
        }
        let walks: i64 = at.iter().sum();
        let r = run(
            "MATCH WALK (a:person {id: $src})-[:follows*3..3]->(b) RETURN count(b) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("walk count");
        assert_eq!(r.rows, [[Value::Int(walks)]]);

        // ACYCLIC against a brute-force node-distinct DFS oracle over
        // one to three hops.
        fn acyclic(edges: &[(u32, u32)], path: &mut Vec<u32>, total: &mut i64) {
            let cur = *path.last().expect("nonempty");
            for &(s, t) in edges {
                if s == cur && !path.contains(&t) {
                    *total += 1;
                    if path.len() < 3 {
                        path.push(t);
                        acyclic(edges, path, total);
                        path.pop();
                    }
                }
            }
        }
        let mut acyclic_total = 0i64;
        acyclic(&edges, &mut vec![src], &mut acyclic_total);
        let r = run(
            "MATCH ACYCLIC (a:person {id: $src})-[:follows*1..3]->(b) RETURN count(b) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("acyclic count");
        assert_eq!(r.rows, [[Value::Int(acyclic_total)]]);

        // A variable-length step whose paths the stage throws away
        // walks each node once instead of once per path, so it has to
        // reach the same endpoints the enumeration reaches. The oracle
        // is a brute-force walk of the same mode over the edge list.
        // rule is (acyclic, min hops, max hops).
        fn ends(
            edges: &[(u32, u32)],
            at: u32,
            used: &mut (Vec<(u32, u32)>, Vec<u32>),
            rule: (bool, usize, usize),
            out: &mut std::collections::BTreeSet<u32>,
        ) {
            let (acyclic, lo, hi) = rule;
            let d = used.0.len();
            if d >= lo {
                out.insert(at);
            }
            if d == hi {
                return;
            }
            for &e in edges {
                if e.0 != at {
                    continue;
                }
                if acyclic {
                    if used.1.contains(&e.1) {
                        continue;
                    }
                } else if used.0.contains(&e) {
                    continue;
                }
                used.0.push(e);
                used.1.push(e.1);
                ends(edges, e.1, used, rule, out);
                used.0.pop();
                used.1.pop();
            }
        }
        let reach_set = |acyclic: bool, lo: usize, hi: usize| {
            let mut out = std::collections::BTreeSet::new();
            let mut used = (Vec::new(), vec![src]);
            ends(&edges, src, &mut used, (acyclic, lo, hi), &mut out);
            out
        };
        let reach_len =
            |acyclic: bool, lo: usize, hi: usize| reach_set(acyclic, lo, hi).len() as i64;
        // The start node comes back through a cycle under TRAIL and
        // never under ACYCLIC, which is the one endpoint the two modes
        // disagree about and the reason the walk emits it separately.
        assert!(reach_set(false, 1, 3).contains(&src));
        assert!(!reach_set(true, 1, 3).contains(&src));

        // DISTINCT over the projected rows, which is the shape a WITH
        // writes, and a DISTINCT aggregate, which is the shape a
        // RETURN writes. Both throw the duplicates away, so both take
        // the walk.
        for text in [
            "MATCH (a:person {id: $src})-[:follows*1..3]->(b) \
             WITH DISTINCT a, b RETURN count(*) AS n",
            "MATCH (a:person {id: $src})-[:follows*1..3]->(b) \
             RETURN count(DISTINCT b) AS n",
        ] {
            let plan = explain_analyze(text, &mut db, &[("src", Value::Int(i64::from(src)))])
                .expect("plan");
            assert!(plan.contains("reach"), "{text}\n{plan}");
            let r = run(text, &mut db, &[("src", Value::Int(i64::from(src)))]).expect("reach");
            assert_eq!(r.rows, [[Value::Int(reach_len(false, 1, 3))]], "{text}");
        }

        // ACYCLIC cannot end where it started, so the start node is
        // the one the walk has to leave out.
        let r = run(
            "MATCH ACYCLIC (a:person {id: $src})-[:follows*1..3]->(b) \
             WITH DISTINCT a, b RETURN count(*) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("acyclic reach");
        assert_eq!(r.rows, [[Value::Int(reach_len(true, 1, 3))]]);

        // A minimum above one hop keeps the enumeration: the walk
        // reaches a node at the fewest hops that reach it and never
        // again, so a node this wants only further out would go
        // missing.
        let r = run(
            "MATCH (a:person {id: $src})-[:follows*2..3]->(b) \
             WITH DISTINCT a, b RETURN count(*) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("min two reach");
        assert_eq!(r.rows, [[Value::Int(reach_len(false, 2, 3))]]);
        let plan = explain_analyze(
            "MATCH (a:person {id: $src})-[:follows*2..3]->(b) \
             WITH DISTINCT a, b RETURN count(*) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("plan");
        assert!(!plan.contains("reach"), "{plan}");

        // A stage that keeps the duplicates still enumerates: this
        // counts paths and not endpoints.
        fn count_trails(edges: &[(u32, u32)], at: u32, used: &mut Vec<(u32, u32)>, n: &mut i64) {
            if !used.is_empty() {
                *n += 1;
            }
            if used.len() == 3 {
                return;
            }
            for &e in edges {
                if e.0 != at || used.contains(&e) {
                    continue;
                }
                used.push(e);
                count_trails(edges, e.1, used, n);
                used.pop();
            }
        }
        let mut paths = 0i64;
        count_trails(&edges, src, &mut Vec::new(), &mut paths);
        let r = run(
            "MATCH (a:person {id: $src})-[:follows*1..3]->(b) RETURN count(b) AS n",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("paths kept");
        assert_eq!(r.rows, [[Value::Int(paths)]]);

        // Path returns on real storage, checked structurally against
        // the BFS levels: every path alternates node, rel, node, its
        // length is twice the endpoint's level plus one, its rels are
        // exactly the r list, and each hop's endpoints chain.
        let r = run(
            "MATCH p = ANY SHORTEST (a:person {id: $src})-[r:follows*]->(b) \
             RETURN p, r, b.id AS id ORDER BY id",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("path return");
        assert_eq!(r.rows.len(), reached.len());
        for row in &r.rows {
            let (Value::Path(p), Value::List(rels), Value::Int(b)) = (&row[0], &row[1], &row[2])
            else {
                panic!("expected a path, a rel list, and an id, got {row:?}");
            };
            assert_eq!(p.len(), 2 * level[*b as usize] as usize + 1);
            assert_eq!(p.len(), 2 * rels.len() + 1);
            assert_eq!(
                p[0],
                Value::Node {
                    table: 0,
                    offset: u64::from(src)
                }
            );
            assert_eq!(
                p[p.len() - 1],
                Value::Node {
                    table: 0,
                    offset: *b as u64
                }
            );
            for (hop, rel) in rels.iter().enumerate() {
                assert_eq!(&p[2 * hop + 1], rel, "rel {hop} diverges from r");
                let Value::Rel {
                    src: rs, dst: rd, ..
                } = rel
                else {
                    panic!("expected a rel at hop {hop}, got {rel:?}");
                };
                assert_eq!(
                    p[2 * hop],
                    Value::Node {
                        table: 0,
                        offset: *rs
                    }
                );
                assert_eq!(
                    p[2 * hop + 2],
                    Value::Node {
                        table: 0,
                        offset: *rd
                    }
                );
            }
        }

        // Left-outer semantics on real storage: people with no edge
        // into the high ids keep one row with a null friend, so
        // count(a) sees every row and count(b) only the matches.
        let t = 80i64;
        let mut people = 0i64;
        let mut matched = 0i64;
        let mut misses = 0i64;
        for s in 0..97u32 {
            let n = edges
                .iter()
                .filter(|(a, b)| *a == s && i64::from(*b) >= t)
                .count() as i64;
            people += n.max(1);
            matched += n;
            misses += i64::from(n == 0);
        }
        assert!(misses > 0, "threshold too low to exercise the null path");
        let r = run(
            "MATCH (a:person) OPTIONAL MATCH (a)-[:follows]->(b) WHERE b.id >= $t \
             RETURN count(a) AS people, count(b) AS friends",
            &mut db,
            &[("t", Value::Int(t))],
        )
        .expect("optional count");
        assert_eq!(r.rows, [[Value::Int(people), Value::Int(matched)]]);
    }

    /// A property read off a rel variable finds the edge's own row,
    /// which is the same row whichever direction the pattern walked it
    /// in.
    #[test]
    fn reads_an_edge_property_walking_either_way() {
        use crate::zu1::props::{PropValues, store_rel_props};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edgeprops.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let edges = [(0u32, 1u32), (0, 2), (1, 2), (2, 3), (3, 0)];
        graph::bulk_load_as(&mut db, "person", "knows", 4, &edges).expect("load");
        let since: Vec<u64> = (0..edges.len() as u64).map(|i| 2001 + i).collect();
        store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&since))]).expect("store");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let expected: Vec<Vec<Value>> = edges
            .iter()
            .zip(&since)
            .map(|(&(s, d), &v)| {
                vec![
                    Value::Int(i64::from(s)),
                    Value::Int(i64::from(d)),
                    Value::Int(v as i64),
                ]
            })
            .collect();
        let r = run(
            "MATCH (a:person)-[e:knows]->(b) \
             RETURN a.id AS a, b.id AS b, e.since AS since ORDER BY a, b",
            &mut db,
            &[],
        )
        .expect("forward");
        assert_eq!(r.columns, ["a", "b", "since"]);
        assert_eq!(r.rows, expected);

        // Reached backward the edge is the same edge, so it answers
        // the same value: the row a walk counts out of the in-list is
        // the row the out-list would have given it.
        let r = run(
            "MATCH (b:person)<-[e:knows]-(a) \
             RETURN a.id AS a, b.id AS b, e.since AS since ORDER BY a, b",
            &mut db,
            &[],
        )
        .expect("backward");
        assert_eq!(r.rows, expected);

        // An undirected walk reaches every edge once from each end and
        // reads the same value both times.
        let r = run(
            "MATCH (a:person {id: 2})-[e:knows]-(b) RETURN e.since AS since ORDER BY since",
            &mut db,
            &[],
        )
        .expect("undirected");
        assert_eq!(
            r.rows,
            [[Value::Int(2002)], [Value::Int(2003)], [Value::Int(2004)],]
        );

        // A property the table does not store is null, not an error:
        // an edge carries whatever its table wrote down and nothing
        // says every rel table writes the same keys.
        let r = run(
            "MATCH (a:person {id: 0})-[e:knows]->(b) RETURN e.weight AS w",
            &mut db,
            &[],
        )
        .expect("missing key");
        assert_eq!(r.rows, [[Value::Null], [Value::Null]]);
    }

    /// A pair that runs more than once is that many edges, each with
    /// its own row of the property columns, and a walk that reaches
    /// them reads each one's own value rather than the first one's for
    /// all of them.
    ///
    /// This is the shape the generated finance graphs have and the
    /// LDBC ones do not: an account there sends three hundred
    /// transfers to a hundred and fifty counterparties, so every sum
    /// over a transfer amount is wrong by whatever the copies differ
    /// by until each copy answers for itself.
    #[test]
    fn reads_each_copy_of_a_pair_that_runs_more_than_once() {
        use crate::zu1::props::{PropValues, store_rel_props};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("copies.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // 0 -> 1 three times, 2 -> 1 twice, and single edges either
        // side of them, so a run is neither the whole list nor the
        // start of one in either direction.
        let edges = [(0u32, 1u32), (0, 1), (0, 1), (0, 3), (2, 1), (2, 1), (3, 1)];
        graph::bulk_load_as(&mut db, "person", "knows", 4, &edges).expect("load");
        let since: Vec<u64> = (0..edges.len() as u64).map(|i| 2001 + i).collect();
        store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&since))]).expect("store");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let ints =
            |vs: &[i64]| -> Vec<Vec<Value>> { vs.iter().map(|&v| vec![Value::Int(v)]).collect() };

        // Out of 0: the three copies of 0 -> 1 carry rows 0, 1 and 2,
        // and each answers with its own.
        let r = run(
            "MATCH (a:person {id: 0})-[e:knows]->(b) RETURN e.since AS since ORDER BY since",
            &mut db,
            &[],
        )
        .expect("out");
        assert_eq!(r.rows, ints(&[2001, 2002, 2003, 2004]));

        // Into 1: the same three copies plus the two of 2 -> 1 and the
        // single 3 -> 1, every one of them read backward and every one
        // of them its own row.
        let r = run(
            "MATCH (a:person)-[e:knows]->(b:person {id: 1}) RETURN e.since AS since ORDER BY since",
            &mut db,
            &[],
        )
        .expect("in");
        assert_eq!(r.rows, ints(&[2001, 2002, 2003, 2005, 2006, 2007]));

        // The sum over the whole table is the sum of what was stored,
        // which is the check a benchmark makes and the one that fails
        // when a run of copies reads the first copy's value.
        let total: i64 = since.iter().map(|&v| v as i64).sum();
        let r = run(
            "MATCH (:person)-[e:knows]->(:person) RETURN count(e) AS n, sum(e.since) AS total",
            &mut db,
            &[],
        )
        .expect("total");
        assert_eq!(
            r.rows,
            [[Value::Int(edges.len() as i64), Value::Int(total)]]
        );
    }

    /// A pattern with both endpoints pinned matches once per edge and
    /// not once per pair, so a close onto a bound node reports the whole
    /// run: three rows for a pair joined three times, each carrying its
    /// own property, and the same rows in the same order as the walk
    /// that reaches the far node instead of pinning it.
    #[test]
    fn a_close_on_a_bound_pair_reports_every_edge_of_it() {
        use crate::zu1::props::{PropValues, store_rel_props};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("close.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let edges = [(0u32, 1u32), (0, 1), (0, 1), (0, 3), (2, 1), (2, 1), (3, 1)];
        graph::bulk_load_as(&mut db, "person", "knows", 4, &edges).expect("load");
        let since: Vec<u64> = (0..edges.len() as u64).map(|i| 2001 + i).collect();
        store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&since))]).expect("store");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let ints =
            |vs: &[i64]| -> Vec<Vec<Value>> { vs.iter().map(|&v| vec![Value::Int(v)]).collect() };

        let closed = run(
            "MATCH (a:person {id: 0})-[e:knows]->(b:person {id: 1}) \
             RETURN e.since AS since ORDER BY since",
            &mut db,
            &[],
        )
        .expect("closed");
        assert_eq!(closed.rows, ints(&[2001, 2002, 2003]));

        // The pair that runs once still runs once, and a pair with no
        // edge still matches nothing.
        let r = run(
            "MATCH (a:person {id: 3})-[e:knows]->(b:person {id: 1}) \
             RETURN e.since AS since ORDER BY since",
            &mut db,
            &[],
        )
        .expect("single");
        assert_eq!(r.rows, ints(&[2007]));
        let r = run(
            "MATCH (a:person {id: 1})-[e:knows]->(b:person {id: 0}) RETURN e.since AS since",
            &mut db,
            &[],
        )
        .expect("none");
        assert!(r.rows.is_empty());

        // Reached rather than pinned, which is the expand the close
        // stands in for, and the two have to agree edge for edge.
        let walked = run(
            "MATCH (a:person {id: 0})-[e:knows]->(b:person) WHERE b.id = 1 \
             RETURN e.since AS since ORDER BY since",
            &mut db,
            &[],
        )
        .expect("walked");
        assert_eq!(closed.rows, walked.rows);
    }

    /// A secondary label is a bit on the row, so a pattern naming one
    /// reads the same rows the table holds and keeps the ones whose
    /// word has the bit. The table's own label costs nothing.
    #[test]
    fn matches_a_node_by_a_label_its_table_declares() {
        use crate::zu1::props::store_labels;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("labels.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let edges = [(0u32, 1u32), (1, 2), (2, 3), (3, 0)];
        graph::bulk_load_as(&mut db, "person", "knows", 4, &edges).expect("load");
        store_labels(
            &mut db,
            "person",
            &[
                vec!["Employee"],
                vec![],
                vec!["Employee", "Manager"],
                vec!["Manager"],
            ],
        )
        .expect("labels");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let mut ids = |source: &str| {
            run(source, &mut db, &[])
                .expect("run")
                .rows
                .into_iter()
                .map(|r| r[0].clone())
                .collect::<Vec<Value>>()
        };
        assert_eq!(
            ids("MATCH (n:person) RETURN n.id AS id ORDER BY id"),
            [Value::Int(0), Value::Int(1), Value::Int(2), Value::Int(3)]
        );
        assert_eq!(
            ids("MATCH (n:Employee) RETURN n.id AS id ORDER BY id"),
            [Value::Int(0), Value::Int(2)]
        );
        assert_eq!(
            ids("MATCH (n:person:Manager) RETURN n.id AS id ORDER BY id"),
            [Value::Int(2), Value::Int(3)]
        );
        assert_eq!(
            ids("MATCH (n:Employee:Manager) RETURN n.id AS id"),
            [Value::Int(2)]
        );
        // The bit travels with the node however it was reached, so a
        // label on the far end of an expand reads the same way.
        assert_eq!(
            ids("MATCH (a:person {id: 1})-[:knows]->(b:Manager) RETURN b.id AS id"),
            [Value::Int(2)]
        );
        // A label the graph never declared names no rows, and saying
        // so is the binder's job rather than the executor's.
        let err = run("MATCH (n:Ghost) RETURN n", &mut db, &[]).expect_err("unknown label");
        assert!(err.to_string().contains("unknown label 'Ghost'"), "{err}");
    }

    #[test]
    fn matches_a_node_by_a_label_expression() {
        use crate::zu1::props::store_labels;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("label-exprs.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let edges = [(0u32, 1u32), (1, 2), (2, 3), (3, 0)];
        graph::bulk_load_as(&mut db, "person", "knows", 4, &edges).expect("load");
        store_labels(
            &mut db,
            "person",
            &[
                vec!["Employee"],
                vec![],
                vec!["Employee", "Manager"],
                vec!["Manager"],
            ],
        )
        .expect("labels");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let mut ids = |source: &str| {
            run(source, &mut db, &[])
                .expect("run")
                .rows
                .into_iter()
                .map(|r| r[0].clone())
                .collect::<Vec<Value>>()
        };
        // The conjunction the colon writes, written the other way.
        assert_eq!(
            ids("MATCH (n:Employee&Manager) RETURN n.id AS id"),
            [Value::Int(2)]
        );
        assert_eq!(
            ids("MATCH (n:Employee|Manager) RETURN n.id AS id ORDER BY id"),
            [Value::Int(0), Value::Int(2), Value::Int(3)]
        );
        assert_eq!(
            ids("MATCH (n:!Employee) RETURN n.id AS id ORDER BY id"),
            [Value::Int(1), Value::Int(3)]
        );
        // Precedence: this is Employee or (Manager and not Employee),
        // which is everything with either.
        assert_eq!(
            ids("MATCH (n:Employee|Manager&!Employee) RETURN n.id AS id ORDER BY id"),
            [Value::Int(0), Value::Int(2), Value::Int(3)]
        );
        // And the parentheses say the other thing.
        assert_eq!(
            ids("MATCH (n:(Employee|Manager)&!Employee) RETURN n.id AS id"),
            [Value::Int(3)]
        );
        // Every node carries its table's label, so `%` holds of all
        // four and its negation of none.
        assert_eq!(
            ids("MATCH (n:%) RETURN n.id AS id ORDER BY id"),
            [Value::Int(0), Value::Int(1), Value::Int(2), Value::Int(3)]
        );
        // The row with no secondary label at all is the one node that
        // is a person and nothing else.
        assert_eq!(
            ids("MATCH (n:person&!Employee&!Manager) RETURN n.id AS id"),
            [Value::Int(1)]
        );
    }

    #[test]
    fn explain_analyze_profiles_a_real_zu1_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("analyze.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let src = 3u32;
        let friends = edges.iter().filter(|(s, _)| *s == src).count();
        let text = explain_analyze(
            "MATCH (a:person {id: $src})-[:follows]->(b) RETURN b.id AS friend",
            &mut db,
            &[("src", Value::Int(i64::from(src)))],
        )
        .expect("explain analyze");
        assert!(
            text.contains(&format!("stage 1: Project [{friends} rows,")),
            "got:\n{text}"
        );
        assert!(
            text.contains("IndexLookup a: person [id = $src]"),
            "got:\n{text}"
        );
        assert!(text.contains("Expand (a)-[:follows]->(b)"), "got:\n{text}");
        assert!(text.contains("pulls"), "got:\n{text}");

        // The unfiltered 2-hop count runs on degrees, not lists, all
        // the way through real storage.
        let text = explain_analyze(
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c) RETURN count(c) AS paths",
            &mut db,
            &[],
        )
        .expect("explain analyze count");
        assert!(
            text.contains("ExpandCount (b)-[:follows]->(c)"),
            "got:\n{text}"
        );
    }

    #[test]
    fn the_listing_ends_with_the_decisions_the_pipeline_made() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("decide.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let text = explain_analyze(
            "MATCH (a:person)-[:follows]->(b) RETURN count(b) AS n",
            &mut db,
            &[],
        )
        .expect("explain analyze");
        // The split is the one decision every covered run makes, so it
        // is the one that is always there to read.
        assert!(text.contains("decisions:"), "got:\n{text}");
        assert!(text.contains("split scan into"), "got:\n{text}");
        // The count above is answered off degrees and never reads a
        // neighbor, so it decodes no group and the line stays away. A
        // walk that has to look at the neighbors themselves reads the
        // group whole, since a scan wants every list in it.
        assert!(!text.contains("whole group(s)"), "got:\n{text}");
        let walked = explain_analyze(
            "MATCH (a:person)-[:follows]->(b) RETURN b.id AS b",
            &mut db,
            &[],
        )
        .expect("explain analyze");
        assert!(
            walked.contains("decoded 1 whole group(s) and read around 0 more"),
            "got:\n{walked}"
        );

        // Pinned to the old engine there is no pipeline to report on,
        // and the listing says nothing rather than saying zero.
        // SAFETY: single-threaded test, no other thread reads the
        // environment while this is set.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let text = explain_analyze(
            "MATCH (a:person)-[:follows]->(b) RETURN count(b) AS n",
            &mut db,
            &[],
        )
        .expect("explain analyze");
        // SAFETY: as above.
        unsafe { std::env::remove_var("ZU_EXEC2") };
        assert!(!text.contains("decisions:"), "got:\n{text}");
    }

    #[test]
    fn the_profile_carries_the_estimate_beside_the_measured_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("qerror.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let text = explain_analyze(
            "MATCH (a:person)-[:follows]->(b) RETURN a.id, b.id",
            &mut db,
            &[],
        )
        .expect("explain analyze");
        // The scan knows the table count exactly, so its estimate is
        // the row count and its q-error is one. Degree histograms make
        // the expand nearly exact on this shape too.
        assert!(
            text.contains("Scan a: person")
                && text.contains("flat        97  est        97  q    1.0"),
            "got:\n{text}"
        );
        // The source and the flattens report configurations per pull,
        // not rows, so they must not claim an error.
        for line in text
            .lines()
            .filter(|l| l.contains("Source") || l.contains("Flatten"))
        {
            assert!(line.contains("est         -  q      -"), "got:\n{text}");
        }
    }

    #[test]
    fn the_flat_count_is_the_path_count_not_the_vector_width() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("star.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        drop(db);

        // Two-hop paths through the middle node, in and out degree
        // multiplied per middle and summed, which is what the plan is
        // counting.
        let mut indeg = [0u64; 97];
        let mut outdeg = [0u64; 97];
        for &(s, d) in &edges {
            outdeg[s as usize] += 1;
            indeg[d as usize] += 1;
        }
        let paths: u64 = (0..97).map(|v| indeg[v] * outdeg[v]).sum();
        assert!(paths > 0);

        let mut db = Zu1File::open(&path).expect("open");
        let text = explain_analyze(
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c) RETURN count(c) AS n",
            &mut db,
            &[],
        )
        .expect("explain analyze two hop");
        // The optimizer starts at the middle and expands both ways, so
        // the first hop's vector is still unflat while the second runs.
        // The second hop's own row count is neighbours summed over the
        // middles; the paths are that times the first hop's width, and
        // it is the paths the estimate has to be held against.
        let last = text
            .lines()
            .find(|l| l.contains("(b)-[:follows]->(c)") || l.contains("(b)<-[:follows]-(c)"))
            .unwrap_or_else(|| panic!("no closing hop:\n{text}"));
        assert!(
            last.contains(&format!("flat {paths:>9}")),
            "want flat {paths}, got:\n{text}"
        );
    }

    #[test]
    fn stored_statistics_bound_the_hop_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hubs.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // Ten hub sources hold two hundred edges each while every
        // target holds one, so the stored histograms say fan-out 200
        // forward and 1 backward. The count ratio alone says 0.67
        // either way and cannot tell the directions apart, and taking
        // the forward fan-out at face value puts 600000 rows on a hop
        // that produces 2000.
        let edges: Vec<(u32, u32)> = (0..10u32)
            .flat_map(|h| (0..200u32).map(move |k| (h, 1000 + h * 200 + k)))
            .collect();
        graph::bulk_load_as(&mut db, "person", "follows", 3000, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let text = explain_analyze(
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c) RETURN count(c) AS paths",
            &mut db,
            &[],
        )
        .expect("explain analyze");
        // Three thousand distinct scanned nodes cannot expand into more
        // rows than the table holds edges, whichever way they walk, so
        // the stored norms cap the hop at 2000 and it comes out exact.
        // The direction is a genuine tie once both sides cap there, so
        // there is nothing left to assert about which way it goes.
        assert!(
            text.contains("rows     2000  flat      2000  est      2000  q    1.0"),
            "got:\n{text}"
        );
        assert!(!text.contains("600000"), "got:\n{text}");
    }

    #[test]
    fn analyze_built_colors_steer_the_plan_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("colors.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // Two ranks of ten knows hubs each, feeding ten sinks that hold
        // no further knows edges. Two hops really do run a thousand
        // ways, so the norms are right to say so, but the third hop
        // lands on the sinks and dies. Nothing but the coloring knows
        // that, so the means send a three hop walk five thousand rows
        // wide into the close and it upgrades to the hash join for an
        // accumulate sweep over nothing.
        let knows: Vec<(u32, u32)> = (0..20u32)
            .flat_map(|h| (0..10u32).map(move |t| (h, 10 + (h / 10) * 10 + t)))
            .collect();
        graph::bulk_load_as(&mut db, "person", "knows", 2010, &knows).expect("load knows");
        // A second empty rel table so the close can name two of them.
        // The intersection reads one sorted list per end, so a close
        // spanning two rel tables keeps the WCOJ fusion out and the asp
        // mark alone decides between the probe and the hash join.
        graph::bulk_load_as(&mut db, "person", "likes", 2010, &[]).expect("load likes");
        let source = "MATCH (a:person)-[:knows]->(b)-[:knows]->(c)-[:knows]->(d), \
                      (a)-[:knows|likes]-(d) RETURN count(*) AS n";
        let before = explain_analyze(source, &mut db, &[]).expect("explain before");
        assert!(before.contains("AspJoin"), "got:\n{before}");
        crate::zu1::colors::analyze(&mut db).expect("analyze");
        let after = explain_analyze(source, &mut db, &[]).expect("explain after");
        assert!(!after.contains("AspJoin"), "got:\n{after}");
    }

    #[test]
    fn column_statistics_tell_the_frequent_value_from_the_rare_one() {
        use crate::zu1::props::{PropValues, store_props};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("skew.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // One thousand people whose tag column is badly skewed: 700
        // hubs, then 200, 60 and 40. Uniformity would call every one of
        // the four 250 rows and be wrong three times.
        let counts = [(&b"hub"[..], 700usize), (b"a", 200), (b"b", 60), (b"c", 40)];
        let tags: Vec<&[u8]> = counts
            .iter()
            .flat_map(|&(tag, n)| std::iter::repeat_n(tag, n))
            .collect();
        graph::bulk_load_as(&mut db, "person", "knows", 1000, &[(0u32, 1u32)]).expect("load");
        store_props(&mut db, "person", &[("tag", PropValues::Str(&tags))]).expect("props");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let est_of = |source: &str, args: &[(&str, Value)], db: &mut Zu1File| -> String {
            let text = explain_analyze(source, db, args).expect("explain");
            text.lines()
                .find(|l| l.contains("Filter"))
                .unwrap_or_else(|| panic!("no filter line in:\n{text}"))
                .to_string()
        };

        // A literal the top list holds is estimated at its own count,
        // and the actual comes back beside it, so both halves of the
        // assertion are the same run.
        for (tag, n) in counts {
            let tag = std::str::from_utf8(tag).expect("utf8");
            let line = est_of(
                &format!("MATCH (a:person) WHERE a.tag = '{tag}' RETURN count(*) AS c"),
                &[],
                &mut db,
            );
            assert!(line.contains(&format!("est {n:>9}")), "got: {line}");
            assert!(line.contains(&format!("flat {n:>9}")), "got: {line}");
        }

        // A parameter is not known when the plan is built, so the only
        // honest answer is the average value's share, 1000 over 4.
        let line = est_of(
            "MATCH (a:person) WHERE a.tag = $t RETURN count(*) AS c",
            &[("t", Value::Str("hub".into()))],
            &mut db,
        );
        assert!(line.contains("est       250"), "got: {line}");
    }

    #[test]
    fn keyed_ids_and_stored_props_flow_through_run() {
        use crate::zu1::props::{PropValues, store_props};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("props.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // Row r holds original id keys[r], the shape a REORDER load
        // leaves behind: sparse keys in no particular order.
        let keys: [u64; 5] = [9000, 17, 4025, 333, 12_884_901_888];
        let edges: [(u32, u32); 4] = [(0, 1), (0, 3), (2, 4), (3, 4)];
        graph::bulk_load_keyed(&mut db, "person", "knows", 5, &edges, Some(&keys)).expect("load");
        let names: [&[u8]; 5] = [b"Ada", b"Grace", b"Edsger", b"Barbara", b"Tony"];
        let cities: [u64; 5] = [608, 707, 608, 411, 500];
        store_props(
            &mut db,
            "person",
            &[
                ("id", PropValues::Int(&keys)),
                ("firstName", PropValues::Str(&names)),
                ("cityId", PropValues::Int(&cities)),
            ],
        )
        .expect("store props");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        // The `{id: ...}` predicate resolves through the primary-key
        // index and the id property reads the stored column, so both
        // ends of the query stay in the original key space.
        let r = run(
            "MATCH (a:person {id: $src})-[:knows]->(b) \
             RETURN b.firstName AS name, b.id AS id ORDER BY id",
            &mut db,
            &[("src", Value::Int(9000))],
        )
        .expect("one hop");
        assert_eq!(
            r.rows,
            [
                [Value::Str("Grace".into()), Value::Int(17)],
                [Value::Str("Barbara".into()), Value::Int(333)],
            ]
        );

        // A key naming no row matches nothing instead of erroring or
        // treating the key as an offset.
        let r = run(
            "MATCH (a:person {id: $src}) RETURN a.firstName AS name",
            &mut db,
            &[("src", Value::Int(2))],
        )
        .expect("miss");
        assert!(r.rows.is_empty(), "got: {:?}", r.rows);

        // An integer column other than id, addressed by original key.
        let r = run(
            "MATCH (a:person {id: $src}) RETURN a.cityId AS city",
            &mut db,
            &[("src", Value::Int(4025))],
        )
        .expect("city");
        assert_eq!(r.rows, [[Value::Int(608)]]);

        // Property filters scan in key space too: both people in city
        // 608 come back under their original ids.
        let r = run(
            "MATCH (a:person) WHERE a.cityId = $c RETURN a.id AS id ORDER BY id",
            &mut db,
            &[("c", Value::Int(608))],
        )
        .expect("filter");
        assert_eq!(r.rows, [[Value::Int(4025)], [Value::Int(9000)]]);

        let err = run("MATCH (a:person) RETURN a.nope AS x", &mut db, &[]).expect_err("unknown");
        assert!(err.to_string().contains("unknown property"), "got: {err}");

        // A table function source in key space: 9000 is row 0, and the
        // undirected hop distances come back under stored ids.
        let r = run(
            "CALL sssp('knows', 9000) YIELD node, distance \
             RETURN node.id AS id, distance ORDER BY distance, id",
            &mut db,
            &[],
        )
        .expect("sssp");
        assert_eq!(
            r.rows,
            [
                [Value::Int(9000), Value::Int(0)],
                [Value::Int(17), Value::Int(1)],
                [Value::Int(333), Value::Int(1)],
                [Value::Int(12_884_901_888), Value::Int(2)],
                [Value::Int(4025), Value::Int(3)],
            ]
        );
    }

    #[test]
    fn table_functions_run_through_call_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("call.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // Two components: a chain 0 -> 1 -> 2 and a pair 3 -> 4.
        let edges: [(u32, u32); 3] = [(0, 1), (1, 2), (3, 4)];
        graph::bulk_load_as(&mut db, "person", "follows", 5, &edges).expect("load");
        drop(db);
        let mut db = Zu1File::open(&path).expect("open");

        let r = run(
            "CALL wcc('follows') YIELD node, component \
             RETURN node.id AS id, component ORDER BY id",
            &mut db,
            &[],
        )
        .expect("wcc");
        let ints = |r: &QueryResult| -> Vec<Vec<Value>> { r.rows.clone() };
        assert_eq!(
            ints(&r),
            [
                [Value::Int(0), Value::Int(0)],
                [Value::Int(1), Value::Int(0)],
                [Value::Int(2), Value::Int(0)],
                [Value::Int(3), Value::Int(3)],
                [Value::Int(4), Value::Int(3)],
            ]
        );

        // Distances from row 1 walk both directions; the other
        // component stays null.
        let r = run(
            "CALL sssp('follows', 1) YIELD node, distance \
             RETURN node.id AS id, distance ORDER BY id",
            &mut db,
            &[],
        )
        .expect("sssp");
        assert_eq!(
            ints(&r),
            [
                [Value::Int(0), Value::Int(1)],
                [Value::Int(1), Value::Int(0)],
                [Value::Int(2), Value::Int(1)],
                [Value::Int(3), Value::Null],
                [Value::Int(4), Value::Null],
            ]
        );

        // The directed kernel over the same chain: from row 1 the
        // arrows only reach 2, so 0 joins the other component in the
        // nulls. That is the whole difference between bfs and sssp.
        let r = run(
            "CALL bfs('follows', 1) YIELD node, level \
             RETURN node.id AS id, level ORDER BY id",
            &mut db,
            &[],
        )
        .expect("bfs");
        assert_eq!(
            ints(&r),
            [
                [Value::Int(0), Value::Null],
                [Value::Int(1), Value::Int(0)],
                [Value::Int(2), Value::Int(1)],
                [Value::Int(3), Value::Null],
                [Value::Int(4), Value::Null],
            ]
        );

        let r = run(
            "CALL pagerank('follows') YIELD node, rank \
             RETURN count(node) AS n, sum(rank) AS total",
            &mut db,
            &[],
        )
        .expect("pagerank");
        assert_eq!(r.rows[0][0], Value::Int(5));
        let Value::Float(total) = r.rows[0][1] else {
            panic!("expected a float, got {:?}", r.rows[0][1]);
        };
        assert!((total - 1.0).abs() < 1e-9, "ranks sum to {total}");

        let r = run(
            "CALL louvain('follows') YIELD node, community \
             RETURN count(DISTINCT community) AS communities",
            &mut db,
            &[],
        )
        .expect("louvain");
        assert_eq!(r.rows, [[Value::Int(2)]]);

        // The yielded nodes are real rows: expanding from the small
        // component finds exactly its one edge.
        let r = run(
            "CALL wcc('follows') YIELD node, component \
             WITH node, component WHERE component = 3 \
             MATCH (node)-[:follows]->(m) \
             RETURN count(*) AS inside",
            &mut db,
            &[],
        )
        .expect("compose");
        assert_eq!(r.rows, [[Value::Int(1)]]);
    }

    #[test]
    fn triangle_count_runs_through_call_and_sums_to_the_gap_figure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("triangles.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // A closed triple 0 1 2, a fourth node joined to two of them so
        // it closes a second triangle, and a fifth hanging off the end
        // closing nothing.
        let edges: [(u32, u32); 6] = [(0, 1), (0, 2), (1, 2), (1, 3), (2, 3), (3, 4)];
        graph::bulk_load_as(&mut db, "person", "follows", 5, &edges).expect("load");
        drop(db);
        let mut db = Zu1File::open(&path).expect("open");

        let r = run(
            "CALL triangle_count('follows') YIELD node, triangles \
             RETURN node.id AS id, triangles ORDER BY id",
            &mut db,
            &[],
        )
        .expect("triangle_count");
        assert_eq!(
            r.rows,
            [
                [Value::Int(0), Value::Int(1)],
                [Value::Int(1), Value::Int(2)],
                [Value::Int(2), Value::Int(2)],
                [Value::Int(3), Value::Int(1)],
                [Value::Int(4), Value::Int(0)],
            ]
        );

        // The whole-graph figure, which is the sum over corners with
        // each triangle's three of them divided back out.
        let r = run(
            "CALL triangle_count('follows') YIELD node, triangles \
             WITH sum(triangles) AS corners RETURN corners / 3 AS triangles",
            &mut db,
            &[],
        )
        .expect("total");
        assert_eq!(r.rows, [[Value::Int(2)]]);
    }

    #[test]
    fn sssp_weighted_runs_through_call_over_a_stored_weight_column() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("weighted.zu1");
        use crate::zu1::props::{PropValues, store_rel_props};

        let mut db = Zu1File::create(&path).expect("create");
        // 0 -> 1 direct and expensive, 0 -> 2 -> 1 round the back and
        // cheap, and 0 -> 1 a second time cheaper than the first. Node
        // 3 is only reachable against the arrows, which a weighted run
        // does not follow.
        let edges: [(u32, u32); 5] = [(0, 1), (0, 1), (0, 2), (2, 1), (3, 0)];
        graph::bulk_load_as(&mut db, "person", "follows", 4, &edges).expect("load");
        store_rel_props(
            &mut db,
            "follows",
            &[("w", PropValues::Int(&[10, 6, 1, 2, 1]))],
        )
        .expect("weights");
        drop(db);
        let mut db = Zu1File::open(&path).expect("open");

        let r = run(
            "CALL sssp_weighted('follows', 0, 'w') YIELD node, distance \
             RETURN node.id AS id, distance ORDER BY id",
            &mut db,
            &[],
        )
        .expect("sssp_weighted");
        assert_eq!(
            r.rows,
            [
                [Value::Int(0), Value::Int(0)],
                [Value::Int(1), Value::Int(3)],
                [Value::Int(2), Value::Int(1)],
                [Value::Int(3), Value::Null],
            ]
        );

        // Hop counting over the same table is a different answer: it
        // walks both ways and every edge costs one.
        let r = run(
            "CALL sssp('follows', 0) YIELD node, distance \
             RETURN node.id AS id, distance ORDER BY id",
            &mut db,
            &[],
        )
        .expect("sssp");
        assert_eq!(
            r.rows,
            [
                [Value::Int(0), Value::Int(0)],
                [Value::Int(1), Value::Int(1)],
                [Value::Int(2), Value::Int(1)],
                [Value::Int(3), Value::Int(1)],
            ]
        );

        let err = run(
            "CALL sssp_weighted('follows', 0, 'nope') YIELD node, distance RETURN distance",
            &mut db,
            &[],
        )
        .expect_err("unknown column");
        assert!(
            err.to_string().contains("no edge property column 'nope'"),
            "got: {err}"
        );
    }

    #[test]
    fn cdlp_and_lcc_run_through_call_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cdlp.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        // A triangle 0-1-2 with a pendant 3 hanging off 2.
        let edges: [(u32, u32); 4] = [(0, 1), (0, 2), (1, 2), (2, 3)];
        graph::bulk_load_as(&mut db, "person", "follows", 4, &edges).expect("load");
        drop(db);
        let mut db = Zu1File::open(&path).expect("open");

        // Two rounds carry label 0 across the triangle and out to the
        // pendant, and the remaining eight leave it there.
        let r = run(
            "CALL cdlp('follows') YIELD node, community \
             RETURN node.id AS id, community ORDER BY id",
            &mut db,
            &[],
        )
        .expect("cdlp");
        assert_eq!(
            r.rows,
            [
                [Value::Int(0), Value::Int(0)],
                [Value::Int(1), Value::Int(0)],
                [Value::Int(2), Value::Int(0)],
                [Value::Int(3), Value::Int(0)],
            ]
        );

        // One round stops earlier, which is what makes the round count
        // worth spelling.
        let r = run(
            "CALL cdlp('follows', 1) YIELD node, community \
             RETURN count(DISTINCT community) AS communities",
            &mut db,
            &[],
        )
        .expect("cdlp rounds");
        assert_eq!(r.rows, [[Value::Int(3)]]);

        // 0 and 1 each have two neighbors closed by one directed edge,
        // 2 has three neighbors and the same single edge among them,
        // and 3 has nobody to pair with.
        let r = run(
            "CALL lcc('follows') YIELD node, coefficient \
             RETURN node.id AS id, coefficient ORDER BY id",
            &mut db,
            &[],
        )
        .expect("lcc");
        let got: Vec<f64> = r
            .rows
            .iter()
            .map(|row| match row[1] {
                Value::Float(v) => v,
                ref other => panic!("expected a float, got {other:?}"),
            })
            .collect();
        for (got, want) in got.iter().zip([0.5, 0.5, 1.0 / 6.0, 0.0]) {
            assert!((got - want).abs() < 1e-12, "{got} against {want}");
        }
    }

    /// The morsel scheduler over a real zu1 file: workers fork their
    /// own reopened handles, and 64-row morsels split the 500-person
    /// scan across them. Parallel results must equal the sequential
    /// run exactly, rows and order both.
    #[test]
    fn parallel_scan_matches_sequential_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("par.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..3000u32)
            .map(|i| (i % 499, (i * 13 + 7) % 500))
            .collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 500, &edges).expect("load");
        drop(db);

        let mut db = Zu1File::open(&path).expect("open");
        let sources = [
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c) RETURN count(c) AS paths",
            "MATCH (a:person)-[:follows]->(b) RETURN a.id AS a, b.id AS b",
            "MATCH (a:person)-[:follows]->(b) \
             RETURN a.id AS a, count(*) AS deg ORDER BY deg DESC, a LIMIT 10",
        ];
        for source in sources {
            let p = prepare(source, &mut db, &[]).expect("prepare");
            let mut graph = Zu1Graph::new(&mut db, p.catalog);
            let sequential = exec::execute(
                &p.plan,
                &p.query,
                &p.schema,
                &mut graph,
                &p.args,
                &exec::Options {
                    threads: 1,
                    ..exec::Options::default()
                },
            )
            .expect("sequential");
            let parallel = exec::execute(
                &p.plan,
                &p.query,
                &p.schema,
                &mut graph,
                &p.args,
                &exec::Options {
                    threads: 4,
                    morsel_rows: 64,
                    ..exec::Options::default()
                },
            )
            .expect("parallel");
            assert_eq!(
                sequential.rows, parallel.rows,
                "parallel diverged from sequential on: {source}"
            );
        }
    }

    /// The WCOJ intersection over a real zu1 file: triangle queries
    /// run once through the binary-join baseline pinned off and once
    /// through the default path, where the optimizer's mark routes
    /// the close through MultiwayIntersect, and both must equal each
    /// other and a reference count computed straight from the edge
    /// list.
    #[test]
    fn wcoj_matches_the_binary_join_on_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wcoj.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..4000u32)
            .map(|i| (i % 397, (i * 31 + 11) % 400))
            .collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 400, &edges).expect("load");

        let set: std::collections::HashSet<(u32, u32)> = edges.iter().copied().collect();
        let mut by_src: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
        for &(src, dst) in &edges {
            by_src.entry(src).or_default().push(dst);
        }
        let mut reference = 0i64;
        for &(a, b) in &edges {
            for &c in by_src.get(&b).map_or(&[][..], |v| v) {
                if set.contains(&(a, c)) {
                    reference += 1;
                }
            }
        }
        assert!(reference > 0, "seed produced no triangles, no coverage");

        let sources = [
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c), (a)-[:follows]->(c) \
             RETURN count(*) AS triangles",
            "MATCH (a:person)-[:follows]->(b)-[:follows]->(c), (a)-[:follows]->(c) \
             RETURN a.id AS a, b.id AS b, c.id AS c ORDER BY a, b, c LIMIT 50",
        ];
        for (ix, source) in sources.into_iter().enumerate() {
            let p = prepare(source, &mut db, &[]).expect("prepare");
            let mut graph = Zu1Graph::new(&mut db, p.catalog);
            let baseline = exec::execute(
                &p.plan,
                &p.query,
                &p.schema,
                &mut graph,
                &p.args,
                &exec::Options {
                    wcoj: exec::Wcoj::Off,
                    ..exec::Options::default()
                },
            )
            .expect("baseline");
            let fused = exec::execute(
                &p.plan,
                &p.query,
                &p.schema,
                &mut graph,
                &p.args,
                &exec::Options::default(),
            )
            .expect("wcoj");
            assert_eq!(
                baseline.rows, fused.rows,
                "wcoj diverged from the binary join on: {source}"
            );
            if ix == 0 {
                assert_eq!(fused.rows, vec![vec![Value::Int(reference)]]);
            }
        }
    }
}
