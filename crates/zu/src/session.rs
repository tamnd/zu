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
    query: BoundQuery,
    plan: LogicalPlan,
    /// What the optimizer wants EXPLAIN to say that the tree does not.
    notes: Vec<String>,
}

pub struct Session {
    graph: Zu1Graph<'static>,
    schema: zu_query::binder::Schema,
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
        Ok(Session {
            graph: Zu1Graph::owned(db, catalog),
            schema,
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
                &self.schema,
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
            &self.schema,
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
        let listing = zu_query::plan::explain(&cached.plan, &cached.query, &self.schema);
        Ok(query::noted(cached.notes.clone(), listing))
    }

    /// EXPLAIN ANALYZE through the session: same cache, same options,
    /// profiled execution.
    pub fn explain_analyze(&mut self, source: &str, params: &[(&str, Value)]) -> Result<String> {
        let notes = self.plan_for(source)?.notes.clone();
        Ok(query::noted(notes, self.profile(source, params)?.render()))
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
            &self.schema,
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
        let (query, plan, notes) = query::compile(source, &self.schema)?;
        let cached = Arc::new(CachedPlan { query, plan, notes });
        if self.plans.len() >= PLAN_CAP {
            self.plans.clear();
        }
        self.plans.insert(source.to_string(), cached.clone());
        Ok(cached)
    }

    fn refresh(&mut self) -> Result<()> {
        let epoch = self.graph.file().db_header().epoch;
        if epoch == self.epoch {
            return Ok(());
        }
        let (catalog, schema) = query::load_schema(self.graph.file_mut())?;
        self.graph.set_catalog(catalog);
        self.schema = schema;
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
}
