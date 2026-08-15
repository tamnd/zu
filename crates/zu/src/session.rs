//! A persistent query session over one zu1 file: the catalog, stats,
//! and schema stay resident, compiled plans are cached by source text,
//! and the graph readers keep their decoded groups warm across
//! queries. The one-shot entry points in [`crate::query`] pay the full
//! open-load-parse-plan cost on every call, which is fine for a CLI
//! invocation and hopeless for a server loop; a session pays it once
//! and then a warm query is a hash lookup, parameter binding, and
//! execution.
//!
//! Staleness is a single u64 compare: every entry point checks the
//! handle's header epoch and reloads the catalog, stats, and plan
//! cache when it moved. Reads through one session are serial (the
//! underlying handle seeks), so a session is Send but queries on it
//! do not overlap; open one session per thread.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use zu_common::{Result, ZuError};
use zu_query::binder::BoundQuery;
use zu_query::exec;
use zu_query::plan::LogicalPlan;

use crate::query::{self, QueryResult, Value, Zu1Graph};
use crate::zu1::file::Zu1File;

/// Distinct query texts held before the cache starts over. Workloads
/// cycle a handful of statements; overflow means every text is unique
/// and caching buys nothing, so wholesale clearing loses nothing.
const PLAN_CAP: usize = 1024;

/// One compiled query: everything that depends on the text and the
/// schema alone, shared between the cache and prepared statements.
struct CachedPlan {
    /// The graph this text is against and the tables of it, which is
    /// the working graph unless a `USE` named another one. It rides
    /// with the plan because the executor needs the same schema the
    /// plan was built against and the next query may name a different
    /// graph.
    schema: Arc<zu_query::binder::Schema>,
    query: BoundQuery,
    plan: LogicalPlan,
    /// What the optimizer wants EXPLAIN to say that the tree does not.
    notes: Vec<String>,
}

pub struct Session {
    graph: Zu1Graph<'static>,
    /// The graph a statement is against when it does not say, which is
    /// the home graph for the life of the session so far: there is no
    /// statement yet that moves it.
    working: u32,
    /// One schema per graph a statement has named, built on the first
    /// naming and dropped when the epoch moves.
    schemas: HashMap<u32, Arc<zu_query::binder::Schema>>,
    epoch: u64,
    /// What the pipeline executor's snapshot read last time. A
    /// snapshot lives for one execution, so without this every query
    /// reopens the table readers it needs, which on a small graph is
    /// most of what the query costs.
    snap: crate::snapshot::SnapshotCache,
    plans: HashMap<String, Arc<CachedPlan>>,
    stmts: HashMap<u64, String>,
    next_stmt: u64,
}

impl Session {
    pub fn open(path: &Path) -> Result<Session> {
        let mut db = Zu1File::open(path)?;
        let (catalog, schema) = query::load_schema(&mut db)?;
        let epoch = db.db_header().epoch;
        let working = catalog.home_graph_id();
        Ok(Session {
            graph: Zu1Graph::owned(db, catalog),
            working,
            schemas: HashMap::from([(working, Arc::new(schema))]),
            epoch,
            snap: crate::snapshot::SnapshotCache::default(),
            plans: HashMap::new(),
            stmts: HashMap::new(),
            next_stmt: 1,
        })
    }

    /// Runs one query, compiling it on the first sighting of this text
    /// and reusing the cached plan afterwards.
    pub fn run(&mut self, source: &str, params: &[(&str, Value)]) -> Result<QueryResult> {
        self.refresh()?;
        // A catalog statement publishes a new epoch, and the plans and
        // readers this session holds describe the old one. Refreshing
        // after it is what drops them, so the next query compiles
        // against the catalog the statement just wrote.
        if let Some(stmt) = query::catalog_statement(source)? {
            crate::catalog_stmt::apply(self.graph.file_mut(), &stmt)?;
            self.refresh()?;
            return Ok(QueryResult::new(Vec::new(), Vec::new()));
        }
        let cached = self.plan_for(source)?;
        let args = query::bind_args(&cached.query.params, params)?;
        let options = query::env_options();
        if query::exec2_enabled() {
            let catalog = self.graph.catalog().clone();
            let warm = std::mem::take(&mut self.snap);
            let mut snap =
                crate::snapshot::Zu1Snapshot::with_cache(self.graph.file_mut(), catalog, warm);
            let out = zu_exec::try_execute(
                &cached.plan,
                &cached.query,
                &cached.schema,
                &mut snap,
                &args,
                &options,
            );
            self.snap = snap.into_cache();
            if let Some(r) = out? {
                return Ok(r);
            }
        }
        exec::execute(
            &cached.plan,
            &cached.query,
            &cached.schema,
            &mut self.graph,
            &args,
            &options,
        )
    }

    /// Compiles a statement and pins it under an id. The id maps back
    /// to the source text, so a catalog change between prepare and
    /// execute recompiles instead of running a stale plan. Returns the
    /// id and the parameter names the statement wants, in binder
    /// order.
    pub fn prepare(&mut self, source: &str) -> Result<(u64, Vec<String>)> {
        self.refresh()?;
        let cached = self.plan_for(source)?;
        let params = cached.query.params.clone();
        let id = self.next_stmt;
        self.next_stmt += 1;
        self.stmts.insert(id, source.to_string());
        Ok((id, params))
    }

    /// Makes sure a statement is compiled and cached, and reports
    /// whether it already was. A server preloads its statement set
    /// with this at startup; the P0 plan-hit gate times the `true`
    /// path, which is exactly the work between receiving a known
    /// query text and starting execution: the epoch check and the
    /// cache lookup.
    pub fn warm(&mut self, source: &str) -> Result<bool> {
        self.refresh()?;
        if self.plans.contains_key(source) {
            return Ok(true);
        }
        self.plan_for(source)?;
        Ok(false)
    }

    pub fn execute(&mut self, stmt: u64, params: &[(&str, Value)]) -> Result<QueryResult> {
        let source = self
            .stmts
            .get(&stmt)
            .cloned()
            .ok_or_else(|| ZuError::InvalidArgument(format!("no prepared statement {stmt}")))?;
        self.run(&source, params)
    }

    pub fn close_stmt(&mut self, stmt: u64) -> bool {
        self.stmts.remove(&stmt).is_some()
    }

    /// The plan this session would run for `source`, rendered, without
    /// running it.
    ///
    /// [`Self::explain_analyze`] is the same listing with the observed
    /// counters in it, and it costs a full execution to produce. A
    /// caller that wants to record how a statement ran beside a latency
    /// it measured somewhere else cannot afford that execution: for a
    /// read it would double the work being timed, and for a write it
    /// would apply the write a second time. This one compiles and
    /// renders, which on a warm session is a hash lookup and a string.
    ///
    /// It takes no parameters because zu plans on the statement text
    /// and the schema alone. Accepting them would say the plan depends
    /// on their values, and then a caller would have some reason to
    /// pass the ones it is about to bind.
    pub fn explain(&mut self, source: &str) -> Result<String> {
        self.refresh()?;
        let cached = self.plan_for(source)?;
        let listing = zu_query::plan::explain(&cached.plan, &cached.query, &cached.schema);
        Ok(query::noted(cached.notes.clone(), listing))
    }

    /// EXPLAIN ANALYZE through the session: same cache, same options,
    /// profiled execution.
    pub fn explain_analyze(&mut self, source: &str, params: &[(&str, Value)]) -> Result<String> {
        let notes = self.plan_for(source)?.notes.clone();
        let listing = self.profile(source, params)?.render();
        let listing = match self.decisions(source, params)? {
            Some(d) => format!("{listing}decisions:\n{}", d.render()),
            None => listing,
        };
        Ok(query::noted(notes, listing))
    }

    /// The record the pipeline executor keeps of what it decided while
    /// it ran, `None` when this query is not one it covers. The
    /// counters above come from the old executor, so reading the record
    /// costs a second run of the query on the other engine; EXPLAIN
    /// ANALYZE already costs one execution and is not on any path that
    /// answers a caller.
    fn decisions(
        &mut self,
        source: &str,
        params: &[(&str, Value)],
    ) -> Result<Option<zu_exec::decide::Decisions>> {
        if !query::exec2_enabled() {
            return Ok(None);
        }
        let cached = self.plan_for(source)?;
        let args = query::bind_args(&cached.query.params, params)?;
        let catalog = self.graph.catalog().clone();
        let warm = std::mem::take(&mut self.snap);
        let mut snap =
            crate::snapshot::Zu1Snapshot::with_cache(self.graph.file_mut(), catalog, warm);
        let out = zu_exec::try_execute_profiled(
            &cached.plan,
            &cached.query,
            &cached.schema,
            &mut snap,
            &args,
            &query::env_options(),
        );
        self.snap = snap.into_cache();
        Ok(out?.map(|(_, d)| d))
    }

    /// The same run, handing back the counters instead of the
    /// rendering, for callers that want the numbers. `zu bench
    /// cardinality` reads q-error off this.
    pub fn profile(&mut self, source: &str, params: &[(&str, Value)]) -> Result<exec::Profile> {
        self.refresh()?;
        let cached = self.plan_for(source)?;
        let args = query::bind_args(&cached.query.params, params)?;
        let (_, profile) = exec::execute_profiled(
            &cached.plan,
            &cached.query,
            &cached.schema,
            &mut self.graph,
            &args,
            &query::env_options(),
        )?;
        Ok(profile)
    }

    fn plan_for(&mut self, source: &str) -> Result<Arc<CachedPlan>> {
        if let Some(cached) = self.plans.get(source) {
            return Ok(cached.clone());
        }
        // The text is parsed before anything is compiled because the
        // `USE` clause in front of it says which graph's tables the
        // names below it are names of.
        let parsed = zu_query::parser::parse(source)?;
        let graph = query::graph_of(self.graph.catalog(), self.working, &parsed)?;
        let schema = self.schema_for(graph)?;
        let (query, plan, notes) = query::compile_parsed(&parsed, &schema)?;
        let cached = Arc::new(CachedPlan {
            schema,
            query,
            plan,
            notes,
        });
        if self.plans.len() >= PLAN_CAP {
            self.plans.clear();
        }
        self.plans.insert(source.to_string(), cached.clone());
        Ok(cached)
    }

    /// The schema of one graph, built on the first statement that names
    /// it. A session that never leaves its working graph builds one.
    fn schema_for(&mut self, graph: u32) -> Result<Arc<zu_query::binder::Schema>> {
        if let Some(schema) = self.schemas.get(&graph) {
            return Ok(schema.clone());
        }
        let catalog = self.graph.catalog().clone();
        let schema = Arc::new(query::schema_with_stats(
            self.graph.file_mut(),
            &catalog,
            graph,
        )?);
        self.schemas.insert(graph, schema.clone());
        Ok(schema)
    }

    fn refresh(&mut self) -> Result<()> {
        let epoch = self.graph.file().db_header().epoch;
        if epoch == self.epoch {
            return Ok(());
        }
        let (catalog, schema) = query::load_schema(self.graph.file_mut())?;
        self.working = catalog.home_graph_id();
        self.graph.set_catalog(catalog);
        self.schemas.clear();
        self.schemas.insert(self.working, Arc::new(schema));
        self.plans.clear();
        // The readers the last epoch's snapshots loaded describe a
        // layout that has moved, so they go with the plans.
        self.snap = crate::snapshot::SnapshotCache::default();
        self.epoch = epoch;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zu1::catalog::GraphTypeOf;
    use crate::zu1::graph;

    fn seeded(path: &Path) -> Vec<(u32, u32)> {
        let mut db = Zu1File::create(path).expect("create");
        let mut edges: Vec<(u32, u32)> = (0..400u32).map(|i| (i % 97, (i * 7 + 3) % 89)).collect();
        edges.sort_unstable();
        edges.dedup();
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load");
        edges
    }

    #[test]
    fn cached_runs_match_the_one_shot_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.zu1");
        let edges = seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let source = "MATCH (a:person {id: $src})-[:follows]->(b) \
                      RETURN b.id AS friend ORDER BY friend";
        for src in [3i64, 10, 42, 3, 3] {
            let got = session
                .run(source, &[("src", Value::Int(src))])
                .expect("session run");
            let mut want: Vec<i64> = edges
                .iter()
                .filter(|(s, _)| i64::from(*s) == src)
                .map(|(_, d)| i64::from(*d))
                .collect();
            want.sort_unstable();
            let rows: Vec<i64> = got
                .rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Int(i) => *i,
                    other => panic!("expected int, got {other:?}"),
                })
                .collect();
            assert_eq!(rows, want, "src {src}");
        }
        // The second and later runs hit the plan cache; same text, one
        // compiled entry.
        assert_eq!(session.plans.len(), 1);
    }

    #[test]
    fn a_use_clause_picks_the_graph_the_query_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("use.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");

        let source = "MATCH (a:person) RETURN count(a) AS n";
        let plain = session.run(source, &[]).expect("plain");
        let used = session
            .run(&format!("USE CURRENT_PROPERTY_GRAPH {source}"), &[])
            .expect("use current");
        assert_eq!(plain.rows, used.rows);

        // The home graph by its name is the same graph again, and a
        // name the catalog does not hold is a reference to nothing.
        let named = session
            .run(&format!("USE home {source}"), &[])
            .expect("use home");
        assert_eq!(plain.rows, named.rows);
        let err = session
            .run(&format!("USE nowhere {source}"), &[])
            .expect_err("no such graph");
        assert!(err.to_string().contains("is no graph in '/'"), "{err}");
    }

    #[test]
    fn a_second_graph_holds_no_tables_of_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("two.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        session
            .run("CREATE PROPERTY GRAPH empty ANY", &[])
            .expect("create");

        // The tables are the home graph's, so a query against the new
        // graph does not find them: a name is a name in a graph.
        let err = session
            .run("USE empty MATCH (a:person) RETURN count(a) AS n", &[])
            .expect_err("no person there");
        assert!(err.to_string().contains("person"), "{err}");
        assert!(
            session
                .run("MATCH (a:person) RETURN count(a) AS n", &[])
                .is_ok()
        );
    }

    #[test]
    fn prepared_statements_bind_and_close() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stmt.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let (id, params) = session
            .prepare("MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n")
            .expect("prepare");
        assert_eq!(params, ["src"]);
        let r = session
            .execute(id, &[("src", Value::Int(3))])
            .expect("execute");
        assert_eq!(r.columns, ["n"]);
        assert!(session.close_stmt(id));
        assert!(!session.close_stmt(id));
        let err = session
            .execute(id, &[("src", Value::Int(3))])
            .expect_err("closed");
        assert!(err.to_string().contains("no prepared statement"));
    }

    #[test]
    fn missing_parameter_is_an_error_not_a_stale_bind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("param.zu1");
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        let err = session
            .run(
                "MATCH (a:person {id: $src}) RETURN a.id AS id",
                &[("wrong", Value::Int(1))],
            )
            .expect_err("missing param");
        assert!(err.to_string().contains("missing parameter $src"));
    }

    #[test]
    fn table_readers_outlive_a_query_but_not_an_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("readers.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        assert!(session.snap.readers.is_empty(), "nothing read yet");
        let source = "MATCH (a:person {id: $src})-[:follows]->(b) RETURN count(b) AS n";
        session
            .run(source, &[("src", Value::Int(3))])
            .expect("warm");
        // The follows reader the seek and the hop needed is still
        // here, so the next query starts on it instead of reading its
        // directory back out of the file.
        assert_eq!(session.snap.readers.len(), 1);
        session
            .run(source, &[("src", Value::Int(10))])
            .expect("second");
        assert_eq!(session.snap.readers.len(), 1);

        // A moved epoch describes a layout those readers were built
        // for and no longer describes, so they go with the plans.
        session.graph.file_mut().db_header_mut().epoch += 1;
        session.refresh().expect("refresh");
        assert!(session.snap.readers.is_empty(), "stale readers dropped");
        assert!(session.snap.props.is_empty(), "stale props dropped");
        session
            .run(source, &[("src", Value::Int(3))])
            .expect("after epoch move");
        assert_eq!(session.snap.readers.len(), 1);
    }

    #[test]
    fn epoch_move_reloads_catalog_and_drops_plans() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("epoch.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("warm");
        assert_eq!(session.plans.len(), 1);
        // Push the header epoch forward behind the session's back, the
        // shape a writer commit through the same handle leaves.
        session.graph.file_mut().db_header_mut().epoch += 1;
        session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("after epoch move");
        assert_eq!(session.plans.len(), 1);
        assert_eq!(session.epoch, session.graph.file().db_header().epoch);
    }

    #[test]
    fn a_catalog_statement_publishes_and_the_session_sees_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("types.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("warm");
        assert_eq!(session.plans.len(), 1);
        let before = session.epoch;

        let result = session
            .run(
                "CREATE PROPERTY GRAPH TYPE social {
                   NODE TYPE PersonType (:person {name :: STRING}),
                   (PersonType)-[:follows]->(PersonType)
                 }",
                &[],
            )
            .expect("create graph type");
        assert!(
            result.columns.is_empty(),
            "a statement that answers no rows"
        );
        assert_eq!(result.status(), zu_common::gqlstatus::codes::C00001);
        assert!(session.epoch > before, "the statement published an epoch");
        assert!(session.plans.is_empty(), "the plans went with the epoch");
        let ty = session
            .graph
            .catalog()
            .graph_type("social")
            .expect("the session reloaded the catalog it just wrote");
        assert!(ty.closed);
        assert_eq!(ty.elements.len(), 2);

        // The graph still answers the query it answered before, now
        // against a catalog that declares a type for it.
        session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("after the statement");

        // GC03: the modifier is what turns a taken name into a
        // statement that did nothing.
        let err = session
            .run("CREATE GRAPH TYPE social { (:person) }", &[])
            .expect_err("the name is taken")
            .to_string();
        assert!(err.contains("already a graph type"), "{err}");
        session
            .run("CREATE GRAPH TYPE IF NOT EXISTS social { (:person) }", &[])
            .expect("if not exists");
        assert_eq!(session.graph.catalog().graph_types().len(), 1);
        // The other answer to a taken name: take it over.
        session
            .run("CREATE OR REPLACE GRAPH TYPE social { (:person) }", &[])
            .expect("or replace");
        assert_eq!(session.graph.catalog().graph_types().len(), 1);
        assert_eq!(
            session
                .graph
                .catalog()
                .graph_type("social")
                .expect("social")
                .elements
                .len(),
            1,
            "the replacement is the type that is there now"
        );

        // GG04, off the tables of the graph named and not off its data.
        session
            .run("CREATE GRAPH TYPE mirror LIKE home", &[])
            .expect("like the graph this file holds");
        let mirror = session
            .graph
            .catalog()
            .graph_type("mirror")
            .expect("mirror");
        assert_eq!(
            mirror
                .elements
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["person", "follows"]
        );

        session.run("DROP GRAPH TYPE mirror", &[]).expect("drop");
        assert!(session.graph.catalog().graph_type("mirror").is_none());
        let err = session
            .run("DROP GRAPH TYPE mirror", &[])
            .expect_err("gone already")
            .to_string();
        assert!(err.contains("is no graph type here"), "{err}");
        session
            .run("DROP GRAPH TYPE IF EXISTS mirror", &[])
            .expect("if exists");
    }

    #[test]
    fn a_schema_is_a_directory_the_file_holds_and_a_graph_lives_in_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schemas.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        // GC01, GC02.
        session.run("CREATE SCHEMA /app", &[]).expect("create");
        let err = session
            .run("CREATE SCHEMA /app", &[])
            .expect_err("the path is taken")
            .to_string();
        assert!(err.contains("already a schema"), "{err}");
        session
            .run("CREATE SCHEMA IF NOT EXISTS /app", &[])
            .expect("if not exists");
        assert!(session.graph.catalog().has_schema("/app"));

        // GC04: a graph in that schema, named by the path it is at.
        session
            .run("CREATE GRAPH /app/social ANY", &[])
            .expect("create graph");
        assert!(session.graph.catalog().graph("/app", "social").is_some());

        // ISO's default is RESTRICT, so the schema holding it stays.
        let err = session
            .run("DROP SCHEMA /app", &[])
            .expect_err("the schema still holds a graph")
            .to_string();
        assert!(err.contains("still holds the graph 'social'"), "{err}");
        session
            .run("DROP GRAPH /app/social", &[])
            .expect("drop graph");
        session.run("DROP SCHEMA /app", &[]).expect("drop schema");
        assert!(!session.graph.catalog().has_schema("/app"));
        let err = session
            .run("DROP SCHEMA /", &[])
            .expect_err("the root schema is not one to drop")
            .to_string();
        assert!(err.contains("every file has"), "{err}");
    }

    #[test]
    fn a_graph_is_created_with_a_type_or_with_none_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graphs.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        // GG01: any graph, which is what a zu1 file has always been.
        session.run("CREATE GRAPH open_one ANY", &[]).expect("any");
        assert_eq!(
            session.graph.catalog().graph("/", "open_one"),
            session
                .graph
                .catalog()
                .graphs()
                .iter()
                .find(|g| g.name == "open_one")
        );
        // GC05.
        let err = session
            .run("CREATE GRAPH open_one ANY", &[])
            .expect_err("the name is taken")
            .to_string();
        assert!(err.contains("already a graph"), "{err}");
        session
            .run("CREATE GRAPH IF NOT EXISTS open_one ANY", &[])
            .expect("if not exists");

        // GG03: a type written where the graph is created has no name
        // of its own, so it is no graph type the file holds.
        session
            .run(
                "CREATE PROPERTY GRAPH typed { (:Person {name :: STRING}) }",
                &[],
            )
            .expect("inline type");
        let typed = session
            .graph
            .catalog()
            .graph("/", "typed")
            .expect("typed")
            .clone();
        let GraphTypeOf::Inline(ty) = &typed.graph_type else {
            panic!("a type written inline");
        };
        assert!(ty.closed);
        assert_eq!(ty.elements.len(), 1);
        assert!(session.graph.catalog().graph_types().is_empty());

        // GG02, GG04: the name of a graph type the file holds, and the
        // type of a graph that already has one.
        session
            .run("CREATE GRAPH TYPE social { (:person) }", &[])
            .expect("graph type");
        session
            .run("CREATE GRAPH of_named :: social", &[])
            .expect("of a named type");
        assert_eq!(
            session
                .graph
                .catalog()
                .graph("/", "of_named")
                .map(|g| &g.graph_type),
            Some(&GraphTypeOf::Named("social".to_string()))
        );
        session
            .run("CREATE PROPERTY GRAPH mirror LIKE typed", &[])
            .expect("like a graph");
        let GraphTypeOf::Inline(ty) = &session
            .graph
            .catalog()
            .graph("/", "mirror")
            .expect("mirror")
            .graph_type
        else {
            panic!("the type of the graph it is like");
        };
        assert_eq!(ty.elements.len(), 1);

        // GG05: an empty graph copies as the empty graph it is, and a
        // graph with tables in it is the copy nobody has written yet.
        session
            .run("CREATE GRAPH copy_of_it ANY AS COPY OF open_one", &[])
            .expect("copy of an empty graph");
        let err = session
            .run("CREATE GRAPH copy_of_home ANY AS COPY OF home", &[])
            .expect_err("a copy of a graph that holds tables")
            .to_string();
        assert!(err.contains("AS COPY OF"), "{err}");

        let err = session
            .run("CREATE GRAPH lost ANY AS COPY OF nowhere", &[])
            .expect_err("no such graph")
            .to_string();
        assert!(err.contains("is no graph in"), "{err}");
        assert!(session.graph.catalog().graph("/", "lost").is_none());

        // A replacement frees what the old graph held, so one that
        // cannot be kept is refused before anything is freed and the
        // graph that was there is still there, tables included.
        let tables = session.graph.catalog().node_tables().len();
        let err = session
            .run("CREATE OR REPLACE GRAPH home :: nowhere", &[])
            .expect_err("no such graph type")
            .to_string();
        assert!(err.contains("is no graph type here"), "{err}");
        assert_eq!(session.graph.catalog().node_tables().len(), tables);
        assert!(session.graph.catalog().graph("/", "home").is_some());
    }

    /// How many blocks the committed free list names, which is what a
    /// drop that reclaims has to grow.
    fn free_blocks(db: &mut Zu1File) -> u64 {
        let root = db.db_header().free_list_root;
        if root == crate::zu1::file::NULL_BLOCK {
            return 0;
        }
        let count = db.db_header().block_count;
        let bytes = crate::zu1::meta::read_chain(db, root).expect("the free list chain");
        crate::zu1::file::decode_free_list(&bytes, count)
            .expect("a free list")
            .len() as u64
    }

    #[test]
    fn dropping_a_graph_hands_the_blocks_its_tables_held_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reclaim.zu1");
        let edges = seeded(&path);

        let first_load = {
            let mut db = Zu1File::open(&path).expect("open");
            assert_eq!(free_blocks(&mut db), 0, "a fresh load frees nothing");
            db.db_header().block_count
        };
        let mut session = Session::open(&path).expect("open");
        session
            .run("DROP GRAPH home", &[])
            .expect("drop the graph this file holds");
        assert!(session.graph.catalog().node_tables().is_empty());
        assert!(session.graph.catalog().rel_tables().is_empty());
        drop(session);

        let mut db = Zu1File::open(&path).expect("reopen");
        let freed = free_blocks(&mut db);
        assert!(
            freed >= first_load / 2,
            "{freed} blocks back of the {first_load} the load took"
        );
        // The blocks are back rather than merely unnamed, so loading
        // the same graph again writes into them: it costs the file a
        // block or two where the first load cost it all of them.
        let before = db.db_header().block_count;
        graph::bulk_load_as(&mut db, "person", "follows", 97, &edges).expect("load again");
        let grew = db.db_header().block_count - before;
        assert!(
            grew < first_load / 2,
            "the reload grew the file by {grew} blocks, the first load by {first_load}"
        );
        let mut session = Session::open(&path).expect("open again");
        let rows = session
            .run("MATCH (a:person) RETURN count(a) AS n", &[])
            .expect("the reloaded graph answers");
        assert_eq!(rows.rows.len(), 1);
    }

    #[test]
    fn a_graph_type_that_cannot_be_kept_leaves_the_file_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("refused.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        let epoch = session.epoch;
        let labels = session.graph.catalog().labels().len();
        // The catalog writes a property type with the codes a column
        // stores, and a list of lists is not one of them. The statement
        // interned `Ghost` on the way to finding that out, and the file
        // still has to come out unchanged.
        let err = session
            .run(
                "CREATE GRAPH TYPE strict { (:Ghost {seen :: LIST<LIST<STRING>>}) }",
                &[],
            )
            .expect_err("a property type no column can hold")
            .to_string();
        assert!(err.contains("a type this file cannot write"), "{err}");
        session.refresh().expect("refresh");
        assert_eq!(session.epoch, epoch, "nothing was published");
        assert_eq!(session.graph.catalog().labels().len(), labels);
        assert!(session.graph.catalog().graph_type("strict").is_none());
    }
}
