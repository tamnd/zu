//! The `sqlite` storage engine.
//!
//! Maps the graph onto an ordinary SQLite database file for interop and durability, and doubles as the differential-testing oracle for zu1.
//! Schema mapping is specified in `docs/05-storage-sqlite.md`.
//! Covers open/create with the full docs/05 §3 pragma profile, the `zu_catalog` table, node and rel tables with their adjacency indexes, row inserts, neighbor queries, counts, IMMEDIATE transactions with `wal_checkpoint(TRUNCATE)` as the checkpoint, and the lazy per-group CSR cache from §4.
//! The `GraphStore` trait implementation lands once the trait itself grows out of its skeleton.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use rusqlite::types::{ToSqlOutput, ValueRef};
pub use zu_common::GROUP_ROWS;
use zu_common::{Result, ZuError};
pub use zu_storage::Direction;

/// SQLite `application_id` claimed by zu, the ASCII bytes `ZU1`.
pub const APPLICATION_ID: i32 = 0x005A_5531;

/// Schema version written to `user_version`.
pub const SCHEMA_VERSION: i32 = 1;

/// SQLite column affinity for a property column.
/// The zu type mapping is specified in `docs/05-storage-sqlite.md` §2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnType {
    Integer,
    Real,
    Text,
    Blob,
}

impl ColumnType {
    fn sql(self) -> &'static str {
        match self {
            Self::Integer => "INTEGER",
            Self::Real => "REAL",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
        }
    }
}

/// A property value bound into an insert.
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl rusqlite::ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            Self::Null => ValueRef::Null,
            Self::Int(v) => ValueRef::Integer(*v),
            Self::Real(v) => ValueRef::Real(*v),
            Self::Text(v) => ValueRef::Text(v.as_bytes()),
            Self::Blob(v) => ValueRef::Blob(v),
        }))
    }
}

/// One row of `zu_catalog`: a table this store created and its DDL.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CatalogEntry {
    pub kind: String,
    pub name: String,
    pub sql: String,
}

/// One node group's adjacency in CSR form, built lazily from a rel
/// table's index and served at native speed once cached.
/// Covers rows `group * GROUP_ROWS ..` for one group; rows outside the
/// recorded span read as empty.
#[derive(Debug)]
pub struct CsrGroup {
    lo: i64,
    offsets: Vec<u32>,
    neighbors: Vec<i64>,
}

impl CsrGroup {
    /// Neighbors of `row`, sorted ascending; empty when the row is
    /// outside this group or has no edges in this direction.
    pub fn neighbors(&self, row: i64) -> &[i64] {
        let idx = row - self.lo;
        if idx < 0 || idx as usize + 1 >= self.offsets.len() {
            return &[];
        }
        let (a, b) = (self.offsets[idx as usize], self.offsets[idx as usize + 1]);
        &self.neighbors[a as usize..b as usize]
    }

    /// Edges this group holds in its direction.
    pub fn edge_count(&self) -> usize {
        self.neighbors.len()
    }
}

type CsrKey = (String, u64, Direction);

/// The lazy CSR cache with its per-(table, group, direction) write
/// versions. A cached group is served only while its version matches;
/// writes bump exactly the versions their endpoints fall into, so
/// invalidation is targeted rather than table-wide. A rollback leaves
/// a spurious bump behind, which costs one rebuild, never staleness.
#[derive(Default, Debug)]
struct CsrCache {
    versions: HashMap<CsrKey, u64>,
    built: HashMap<CsrKey, (u64, Arc<CsrGroup>)>,
}

/// SQLite-backed store: one connection over one database file.
#[derive(Debug)]
pub struct SqliteStore {
    conn: Connection,
    cache: Mutex<CsrCache>,
}

impl SqliteStore {
    /// Opens or creates the database file at `path`.
    ///
    /// Applies the full docs/05 §3 profile: `journal_mode=WAL`, `synchronous=NORMAL`, `page_size=8192`, `cache_size=-16384`, `mmap_size=0`, `foreign_keys=OFF`, `busy_timeout=5000`, `wal_autocheckpoint=2000`.
    /// Claims fresh files with [`APPLICATION_ID`] and [`SCHEMA_VERSION`]; files carrying a different nonzero `application_id` are rejected with [`ZuError::Corrupt`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).map_err(sql_err)?;
        // Page size must land before the first write initializes a
        // fresh file; on an existing file this is a no-op until VACUUM.
        conn.pragma_update(None, "page_size", 8192)
            .map_err(sql_err)?;
        let app_id: i32 = conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .map_err(open_err)?;
        match app_id {
            0 => {
                conn.pragma_update(None, "application_id", APPLICATION_ID)
                    .map_err(sql_err)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                    .map_err(sql_err)?;
            }
            APPLICATION_ID => {
                let version: i32 = conn
                    .query_row("PRAGMA user_version", [], |row| row.get(0))
                    .map_err(sql_err)?;
                if version != SCHEMA_VERSION {
                    return Err(ZuError::Unsupported {
                        what: "sqlite schema version",
                        id: version.cast_unsigned(),
                    });
                }
            }
            other => {
                return Err(ZuError::Corrupt {
                    what: "sqlite application_id",
                    detail: format!("expected {APPLICATION_ID:#x}, found {other:#x}"),
                });
            }
        }
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(sql_err)?;
        if mode != "wal" {
            return Err(ZuError::Corrupt {
                what: "sqlite journal mode",
                detail: format!("expected wal, got {mode}"),
            });
        }
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_err)?;
        // Buffer management is the zu layer's job, so mmap stays off,
        // and integrity rules are enforced above the engine, so
        // foreign keys stay off too, matching zu1.
        for (pragma, value) in [
            ("cache_size", -16384i64),
            ("mmap_size", 0),
            ("foreign_keys", 0),
            ("busy_timeout", 5000),
            ("wal_autocheckpoint", 2000),
        ] {
            conn.pragma_update(None, pragma, value).map_err(sql_err)?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS zu_catalog (\
               kind TEXT NOT NULL, \
               name TEXT NOT NULL, \
               sql TEXT NOT NULL, \
               PRIMARY KEY (kind, name))",
        )
        .map_err(sql_err)?;
        Ok(Self {
            conn,
            cache: Mutex::new(CsrCache::default()),
        })
    }

    /// Creates node table `n_<name>` and records it in the catalog.
    /// The rowid alias `zrow` is the node id; property columns get a `p_` prefix.
    pub fn create_node_table(&mut self, name: &str, columns: &[(&str, ColumnType)]) -> Result<()> {
        let name = ident(name)?;
        let mut sql = format!("CREATE TABLE n_{name} (zrow INTEGER PRIMARY KEY");
        for (col, ty) in columns {
            sql.push_str(&format!(", p_{} {}", ident(col)?, ty.sql()));
        }
        sql.push_str(");");
        self.create("node", name, &sql)
    }

    /// Creates rel table `r_<name>` with `src`/`dst` endpoint columns, the forward and backward adjacency indexes, and a catalog entry.
    pub fn create_rel_table(&mut self, name: &str, columns: &[(&str, ColumnType)]) -> Result<()> {
        let name = ident(name)?;
        let mut sql = format!(
            "CREATE TABLE r_{name} (zrel INTEGER PRIMARY KEY, \
             src INTEGER NOT NULL, dst INTEGER NOT NULL"
        );
        for (col, ty) in columns {
            sql.push_str(&format!(", p_{} {}", ident(col)?, ty.sql()));
        }
        sql.push_str(&format!(
            ");\nCREATE INDEX r_{name}_fwd ON r_{name} (src, dst);\
             \nCREATE INDEX r_{name}_bwd ON r_{name} (dst, src);"
        ));
        self.create("rel", name, &sql)
    }

    /// Inserts one node row; `values` bind the property columns in declared order.
    /// Returns the assigned node id.
    pub fn insert_node(&self, table: &str, values: &[Value]) -> Result<i64> {
        let sql = format!(
            "INSERT INTO n_{} VALUES (NULL{})",
            ident(table)?,
            placeholders(values.len())
        );
        self.conn
            .execute(&sql, rusqlite::params_from_iter(values))
            .map_err(sql_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Inserts one relationship from `src` to `dst`; `values` bind the property columns in declared order.
    /// Returns the assigned rel id.
    pub fn insert_rel(&self, table: &str, src: i64, dst: i64, values: &[Value]) -> Result<i64> {
        let table = ident(table)?;
        let sql = format!(
            "INSERT INTO r_{table} VALUES (NULL, ?, ?{})",
            placeholders(values.len())
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(values.len() + 2);
        params.push(&src);
        params.push(&dst);
        params.extend(values.iter().map(|v| v as &dyn rusqlite::ToSql));
        self.conn
            .execute(&sql, params.as_slice())
            .map_err(sql_err)?;
        let mut cache = self.cache.lock().unwrap();
        for (row, dir) in [(src, Direction::Fwd), (dst, Direction::Bwd)] {
            if row >= 0 {
                let key = (table.to_string(), row as u64 / u64::from(GROUP_ROWS), dir);
                *cache.versions.entry(key).or_insert(0) += 1;
            }
        }
        Ok(self.conn.last_insert_rowid())
    }

    /// The CSR adjacency of one node group in one direction, built on
    /// miss from a single index range query and cached until a write
    /// touches the group. Hot traversals read the cached group at
    /// native speed; cold ones pay the B-tree walk once.
    pub fn csr(&self, table: &str, group: u64, dir: Direction) -> Result<Arc<CsrGroup>> {
        let table = ident(table)?;
        let key = (table.to_string(), group, dir);
        let version = {
            let cache = self.cache.lock().unwrap();
            let version = cache.versions.get(&key).copied().unwrap_or(0);
            if let Some((v, csr)) = cache.built.get(&key)
                && *v == version
            {
                return Ok(csr.clone());
            }
            version
        };
        // The lock drops across the build; a write that lands meanwhile
        // has already bumped past `version`, so the entry we insert is
        // born stale and the next call rebuilds.
        let lo = group as i64 * i64::from(GROUP_ROWS);
        let hi = lo + i64::from(GROUP_ROWS) - 1;
        let sql = match dir {
            Direction::Fwd => format!(
                "SELECT src, dst FROM r_{table} INDEXED BY r_{table}_fwd \
                 WHERE src BETWEEN ? AND ? ORDER BY src, dst"
            ),
            Direction::Bwd => format!(
                "SELECT dst, src FROM r_{table} INDEXED BY r_{table}_bwd \
                 WHERE dst BETWEEN ? AND ? ORDER BY dst, src"
            ),
        };
        let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
        let mut offsets = vec![0u32; GROUP_ROWS as usize + 1];
        let mut neighbors = Vec::new();
        let rows = stmt
            .query_map([lo, hi], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(sql_err)?;
        for row in rows {
            let (from, to) = row.map_err(sql_err)?;
            offsets[(from - lo) as usize + 1] += 1;
            neighbors.push(to);
        }
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        let csr = Arc::new(CsrGroup {
            lo,
            offsets,
            neighbors,
        });
        let mut cache = self.cache.lock().unwrap();
        cache.built.insert(key, (version, csr.clone()));
        Ok(csr)
    }

    /// Starts the single-writer transaction as a SQLite IMMEDIATE
    /// transaction, taking the write lock up front.
    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE").map_err(sql_err)
    }

    /// Commits the open transaction; durability follows WAL semantics
    /// under `synchronous=NORMAL`.
    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT").map_err(sql_err)
    }

    /// Rolls the open transaction back.
    pub fn rollback(&self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK").map_err(sql_err)
    }

    /// A monotonic commit marker for MVCC epochs: `data_version` moves
    /// when another connection commits, `total_changes` when this one
    /// writes, so the sum advances on every mutation from anywhere.
    pub fn epoch(&self) -> Result<u64> {
        let data_version: u64 = self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .map_err(sql_err)?;
        let own: u64 = self
            .conn
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .map_err(sql_err)?;
        Ok(data_version + own)
    }

    /// Checkpoints and truncates the WAL, the whole of this engine's
    /// checkpoint machinery per docs/05 section 5.
    pub fn checkpoint(&self) -> Result<()> {
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .map_err(sql_err)?;
        if busy != 0 {
            return Err(ZuError::Io(std::io::Error::other(
                "wal_checkpoint(TRUNCATE) blocked by a concurrent reader",
            )));
        }
        Ok(())
    }

    /// Node ids adjacent to `src` in `r_<table>`, sorted ascending.
    /// `Fwd` follows edges leaving `src`; `Bwd` follows edges entering it.
    pub fn neighbors(&self, table: &str, src: i64, dir: Direction) -> Result<Vec<i64>> {
        let table = ident(table)?;
        let sql = match dir {
            Direction::Fwd => format!(
                "SELECT dst FROM r_{table} INDEXED BY r_{table}_fwd \
                 WHERE src = ? ORDER BY dst"
            ),
            Direction::Bwd => format!(
                "SELECT src FROM r_{table} INDEXED BY r_{table}_bwd \
                 WHERE dst = ? ORDER BY src"
            ),
        };
        let mut stmt = self.conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt.query_map([src], |row| row.get(0)).map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)
    }

    /// Number of rows in node table `n_<table>`.
    pub fn node_count(&self, table: &str) -> Result<i64> {
        self.count("n", table)
    }

    /// Number of rows in rel table `r_<table>`.
    pub fn rel_count(&self, table: &str) -> Result<i64> {
        self.count("r", table)
    }

    /// All tables this store created, ordered by kind then name.
    pub fn catalog_entries(&self) -> Result<Vec<CatalogEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind, name, sql FROM zu_catalog ORDER BY kind, name")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CatalogEntry {
                    kind: row.get(0)?,
                    name: row.get(1)?,
                    sql: row.get(2)?,
                })
            })
            .map_err(sql_err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(sql_err)
    }

    /// The underlying connection, the escape hatch for inspection and
    /// for the differential runner's raw SQL. Writes made through it
    /// bypass CSR invalidation, so treat it as read-only.
    pub fn raw(&self) -> &Connection {
        &self.conn
    }

    /// Runs DDL and its catalog insert in one transaction.
    fn create(&mut self, kind: &str, name: &str, sql: &str) -> Result<()> {
        let tx = self.conn.transaction().map_err(sql_err)?;
        tx.execute_batch(sql).map_err(sql_err)?;
        tx.execute(
            "INSERT INTO zu_catalog (kind, name, sql) VALUES (?1, ?2, ?3)",
            (kind, name, sql),
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)
    }

    fn count(&self, prefix: &str, table: &str) -> Result<i64> {
        let sql = format!("SELECT count(*) FROM {prefix}_{}", ident(table)?);
        self.conn
            .query_row(&sql, [], |row| row.get(0))
            .map_err(sql_err)
    }
}

/// Validates a table or column identifier and passes it through.
/// Names are interpolated into SQL text, so only `[A-Za-z_][A-Za-z0-9_]*` is allowed.
fn ident(name: &str) -> Result<&str> {
    let mut chars = name.chars();
    let head_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if head_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(name)
    } else {
        Err(ZuError::InvalidArgument(format!(
            "invalid identifier {name:?}"
        )))
    }
}

fn placeholders(n: usize) -> String {
    ", ?".repeat(n)
}

fn sql_err(e: rusqlite::Error) -> ZuError {
    ZuError::Io(std::io::Error::other(e))
}

/// Like [`sql_err`], but surfaces a non-SQLite file as corruption.
fn open_err(e: rusqlite::Error) -> ZuError {
    if let rusqlite::Error::SqliteFailure(f, _) = &e
        && f.code == rusqlite::ErrorCode::NotADatabase
    {
        return ZuError::Corrupt {
            what: "sqlite file",
            detail: e.to_string(),
        };
    }
    sql_err(e)
}
