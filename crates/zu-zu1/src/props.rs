//! Node property columns, the slice of the column catalog the M2 LDBC
//! subset needs. A node table's entry in the table index points at a
//! props directory chain: string columns stored as FullZip blob
//! segments, integer columns as cascade-encoded u64 segments, every
//! column row-aligned with the table's row domain. The full typed
//! column catalog is milestone 3 (docs/04 section 5, docs/12); the
//! encoding here is version-prefixed so that catalog replaces this
//! with a version bump, not a migration.
//!
//! Directory layout: `version: u16`, `node_count: u64`,
//! `column_count: u32`, then per column `name_len: u16` + UTF-8 bytes,
//! `type: u8` (0 string, 1 integer), and the column's `SegmentMeta`.

use std::collections::BTreeMap;
use std::sync::Arc;

use zu_common::{Result, ZuError};

use crate::catalog::{Catalog, TableIndex};
use crate::file::{BlockPtr, Zu1File};
use crate::fullzip::{read_blob_range, write_blob_segment};
use crate::meta;
use crate::segment::{
    CHUNK_ROWS, ChunkCache, ChunkDirectory, SegmentMeta, decode_chunk, load_chunk_directory_pooled,
    read_one_cached, write_segment,
};

const PROPS_VERSION: u16 = 1;
const MAX_NAME_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropType {
    Str,
    Int,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropColumn {
    pub name: String,
    pub ty: PropType,
    pub meta: SegmentMeta,
}

/// The property columns of one node table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropsDirectory {
    pub node_count: u64,
    pub columns: Vec<PropColumn>,
}

/// Row-ordered values for one column at store time.
#[derive(Debug, Clone, Copy)]
pub enum PropValues<'a> {
    Str(&'a [&'a [u8]]),
    Int(&'a [u64]),
}

impl PropValues<'_> {
    /// Rows this column carries.
    pub fn len(&self) -> usize {
        match self {
            PropValues::Str(v) => v.len(),
            PropValues::Int(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn corrupt(detail: String) -> ZuError {
    ZuError::Corrupt {
        what: "props directory",
        detail,
    }
}

impl PropsDirectory {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PROPS_VERSION.to_le_bytes());
        out.extend_from_slice(&self.node_count.to_le_bytes());
        out.extend_from_slice(&(self.columns.len() as u32).to_le_bytes());
        for col in &self.columns {
            out.extend_from_slice(&(col.name.len() as u16).to_le_bytes());
            out.extend_from_slice(col.name.as_bytes());
            out.push(match col.ty {
                PropType::Str => 0,
                PropType::Int => 1,
            });
            col.meta.encode(&mut out);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let head = bytes
            .get(..14)
            .ok_or_else(|| corrupt("truncated header".into()))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        if version != PROPS_VERSION {
            return Err(ZuError::Unsupported {
                what: "props directory version",
                id: u32::from(version),
            });
        }
        let node_count = u64::from_le_bytes(head[2..10].try_into().unwrap());
        let column_count = u32::from_le_bytes(head[10..14].try_into().unwrap()) as usize;
        // A column entry is at least 3 bytes of name and type ahead of
        // its segment meta, so a count the payload cannot hold is
        // rejected before it sizes an allocation.
        if column_count > bytes.len().saturating_sub(14) / 3 {
            return Err(corrupt("truncated column entry".into()));
        }
        let mut pos = 14usize;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            let raw = bytes
                .get(pos..pos + 2)
                .ok_or_else(|| corrupt("truncated name length".into()))?;
            let len = u16::from_le_bytes(raw.try_into().unwrap()) as usize;
            pos += 2;
            if len == 0 || len > MAX_NAME_LEN {
                return Err(corrupt(format!(
                    "name length {len} out of 1..{MAX_NAME_LEN}"
                )));
            }
            let raw = bytes
                .get(pos..pos + len)
                .ok_or_else(|| corrupt("truncated name".into()))?;
            let name =
                String::from_utf8(raw.to_vec()).map_err(|_| corrupt("name is not UTF-8".into()))?;
            pos += len;
            let ty = match bytes.get(pos) {
                Some(0) => PropType::Str,
                Some(1) => PropType::Int,
                Some(t) => return Err(corrupt(format!("unknown column type {t}"))),
                None => return Err(corrupt("truncated column type".into())),
            };
            pos += 1;
            let (meta, next) = SegmentMeta::decode(bytes, pos)?;
            pos = next;
            columns.push(PropColumn { name, ty, meta });
        }
        if pos != bytes.len() {
            return Err(corrupt("trailing bytes".into()));
        }
        Ok(Self {
            node_count,
            columns,
        })
    }
}

pub(crate) fn free_props(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    let directory = PropsDirectory::decode(&meta::read_chain(db, root)?)?;
    for col in &directory.columns {
        for &ptr in &col.meta.blocks {
            db.free_block(ptr)?;
        }
    }
    for ptr in meta::chain_blocks(db, root)? {
        db.free_block(ptr)?;
    }
    Ok(())
}

/// Stores the property columns of `node_table`, replacing any earlier
/// set whole; every column must hold exactly one value per row of the
/// table's domain. The table must already exist, so a load always
/// precedes its properties.
pub fn store_props(
    db: &mut Zu1File,
    node_table: &str,
    columns: &[(&str, PropValues)],
) -> Result<PropsDirectory> {
    let catalog = Catalog::load(db)?;
    let table = catalog
        .node_by_name(node_table)
        .ok_or_else(|| ZuError::InvalidArgument(format!("no node table '{node_table}'")))?;
    let (table_id, node_count) = (table.id, table.node_count);
    for (name, values) in columns {
        if values.len() as u64 != node_count {
            return Err(ZuError::InvalidArgument(format!(
                "column '{name}' holds {} values over {node_count} rows",
                values.len()
            )));
        }
    }
    let mut index = TableIndex::load(db)?;
    if let Some(root) = index.get(table_id) {
        free_props(db, root)?;
        index.remove(table_id);
    }
    let mut cols = Vec::with_capacity(columns.len());
    for (name, values) in columns {
        let (ty, meta) = match values {
            PropValues::Str(v) => (PropType::Str, write_blob_segment(db, v)?),
            PropValues::Int(v) => (PropType::Int, write_segment(db, v)?),
        };
        cols.push(PropColumn {
            name: (*name).to_string(),
            ty,
            meta,
        });
    }
    let directory = PropsDirectory {
        node_count,
        columns: cols,
    };
    let root = meta::write_chain(db, &directory.encode())?;
    index.set(table_id, root);
    crate::graph::free_chain(db, db.db_header().table_index_root)?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().table_index_root = index_root;
    db.checkpoint()?;
    Ok(directory)
}

/// Loads the props directory of a node table, `None` when the table
/// has no properties stored.
pub fn load_props(db: &mut Zu1File, table_id: u32) -> Result<Option<PropsDirectory>> {
    let index = TableIndex::load(db)?;
    let Some(root) = index.get(table_id) else {
        return Ok(None);
    };
    Ok(Some(PropsDirectory::decode(&meta::read_chain(db, root)?)?))
}

/// One decoded FullZip chunk of a string column: the values of rows
/// `chunk * CHUNK_ROWS` onward concatenated in `bytes`, row `i` of the
/// chunk ending at `ends[i]`.
#[derive(Debug)]
struct StrChunk {
    chunk: u64,
    bytes: Vec<u8>,
    ends: Vec<u64>,
}

/// Point reads over one table's property columns, keeping decoded
/// chunks between reads the way the key reader does: integer columns
/// through the segment chunk cache, string columns as the last decoded
/// FullZip chunk per column, so a scan or a hot row set stays on slice
/// copies instead of a chunk decode per read.
#[derive(Debug)]
pub struct PropsReader {
    directory: PropsDirectory,
    int_state: BTreeMap<usize, (Arc<ChunkDirectory>, ChunkCache)>,
    str_state: BTreeMap<usize, StrChunk>,
    /// Reused by [`Self::gather_int`] so a warm gather decodes into the
    /// same buffer every call.
    gather_scratch: Vec<u64>,
    /// Row order scratch for the gathers, reused the same way.
    order_scratch: Vec<u32>,
}

impl PropsReader {
    pub fn new(directory: PropsDirectory) -> Self {
        Self {
            directory,
            int_state: BTreeMap::new(),
            str_state: BTreeMap::new(),
            gather_scratch: Vec::new(),
            order_scratch: Vec::new(),
        }
    }

    pub fn columns(&self) -> &[PropColumn] {
        &self.directory.columns
    }

    pub fn col(&self, name: &str) -> Option<usize> {
        self.directory.columns.iter().position(|c| c.name == name)
    }

    pub fn read_int(&mut self, db: &mut Zu1File, col: usize, row: u64) -> Result<u64> {
        let meta = &self.directory.columns[col].meta;
        if let std::collections::btree_map::Entry::Vacant(slot) = self.int_state.entry(col) {
            let pools = db.pools();
            let dir = load_chunk_directory_pooled(db, &pools.fences, meta)?;
            slot.insert((dir, ChunkCache::default()));
        }
        let (dir, cache) = self.int_state.get_mut(&col).expect("just inserted");
        read_one_cached(db, meta, dir, cache, row)
    }

    /// Gathers `col` for arbitrary `rows`, writing `out[i]` for
    /// `rows[i]`, the batched read of perf/04 section 5: rows sort by
    /// position, runs sharing a chunk decode it once into a reused
    /// scratch, and values scatter back to the caller's order. However
    /// many rows land in one chunk, it decodes once per call.
    pub fn gather_int(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        rows: &[u64],
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let meta = &self.directory.columns[col].meta;
        if self.directory.columns[col].ty != PropType::Int {
            return Err(ZuError::InvalidArgument(format!(
                "column '{}' is not an integer column",
                self.directory.columns[col].name
            )));
        }
        let pools = db.pools();
        let dir = load_chunk_directory_pooled(db, &pools.fences, meta)?;
        let order = &mut self.order_scratch;
        order.clear();
        order.extend(0..rows.len() as u32);
        order.sort_unstable_by_key(|&i| rows[i as usize]);
        out.clear();
        out.resize(rows.len(), 0);
        let scratch = &mut self.gather_scratch;
        let mut i = 0;
        while i < order.len() {
            let row = rows[order[i] as usize];
            if row >= meta.value_count {
                return Err(ZuError::InvalidArgument(format!(
                    "row {row} out of 0..{}",
                    meta.value_count
                )));
            }
            let chunk = (row / CHUNK_ROWS as u64) as usize;
            decode_chunk(db, meta, &dir, chunk, scratch)?;
            while i < order.len() {
                let r = rows[order[i] as usize];
                if r / CHUNK_ROWS as u64 != chunk as u64 {
                    break;
                }
                out[order[i] as usize] = scratch[(r % CHUNK_ROWS as u64) as usize];
                i += 1;
            }
        }
        Ok(())
    }

    /// Gathers a string column for arbitrary `rows`: the values land
    /// concatenated in `out_bytes` in row-argument order, value `i`
    /// ending at `out_ends[i]`. Chunk runs decode once, like
    /// [`Self::gather_int`]; the scatter goes through a span table so
    /// the output order is the caller's even though decoding walks the
    /// rows sorted.
    pub fn gather_str(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        rows: &[u64],
        out_bytes: &mut Vec<u8>,
        out_ends: &mut Vec<u64>,
    ) -> Result<()> {
        let meta = &self.directory.columns[col].meta;
        if self.directory.columns[col].ty != PropType::Str {
            return Err(ZuError::InvalidArgument(format!(
                "column '{}' is not a string column",
                self.directory.columns[col].name
            )));
        }
        let order = &mut self.order_scratch;
        order.clear();
        order.extend(0..rows.len() as u32);
        order.sort_unstable_by_key(|&i| rows[i as usize]);
        let mut staged = Vec::new();
        let mut spans = vec![(0usize, 0usize); rows.len()];
        let mut chunk_bytes = Vec::new();
        let mut chunk_ends: Vec<u64> = Vec::new();
        let mut cur_chunk = u64::MAX;
        let mut chunk_start = 0u64;
        for &ix in order.iter() {
            let row = rows[ix as usize];
            if row >= meta.value_count {
                return Err(ZuError::InvalidArgument(format!(
                    "row {row} out of 0..{}",
                    meta.value_count
                )));
            }
            let chunk = row / CHUNK_ROWS as u64;
            if chunk != cur_chunk {
                chunk_start = chunk * CHUNK_ROWS as u64;
                let end = meta.value_count.min(chunk_start + CHUNK_ROWS as u64);
                chunk_bytes.clear();
                chunk_ends.clear();
                read_blob_range(
                    db,
                    meta,
                    chunk_start,
                    end,
                    &mut chunk_bytes,
                    &mut chunk_ends,
                )?;
                cur_chunk = chunk;
            }
            let local = (row - chunk_start) as usize;
            let lo = if local == 0 {
                0
            } else {
                chunk_ends[local - 1] as usize
            };
            let hi = chunk_ends[local] as usize;
            spans[ix as usize] = (staged.len(), staged.len() + (hi - lo));
            staged.extend_from_slice(&chunk_bytes[lo..hi]);
        }
        out_bytes.clear();
        out_ends.clear();
        for &(lo, hi) in &spans {
            out_bytes.extend_from_slice(&staged[lo..hi]);
            out_ends.push(out_bytes.len() as u64);
        }
        Ok(())
    }

    pub fn read_str(
        &mut self,
        db: &mut Zu1File,
        col: usize,
        row: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let meta = &self.directory.columns[col].meta;
        if row >= meta.value_count {
            return Err(ZuError::InvalidArgument(format!(
                "row {row} out of 0..{}",
                meta.value_count
            )));
        }
        let chunk = row / CHUNK_ROWS as u64;
        if !self.str_state.get(&col).is_some_and(|c| c.chunk == chunk) {
            let start = chunk * CHUNK_ROWS as u64;
            let end = meta.value_count.min(start + CHUNK_ROWS as u64);
            let mut fresh = StrChunk {
                chunk,
                bytes: Vec::new(),
                ends: Vec::new(),
            };
            read_blob_range(db, meta, start, end, &mut fresh.bytes, &mut fresh.ends)?;
            self.str_state.insert(col, fresh);
        }
        let c = self.str_state.get(&col).expect("just decoded");
        let i = (row % CHUNK_ROWS as u64) as usize;
        let start = if i == 0 { 0 } else { c.ends[i - 1] as usize };
        out.extend_from_slice(&c.bytes[start..c.ends[i] as usize]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::bulk_load_keyed;

    fn setup(dir: &std::path::Path) -> Zu1File {
        let mut db = Zu1File::create(&dir.join("props.zu1")).unwrap();
        bulk_load_keyed(&mut db, "person", "knows", 4, &[(0, 1), (2, 3)], None).unwrap();
        db
    }

    #[test]
    fn roundtrip_and_point_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let names: Vec<&[u8]> = vec![b"Ada", b"Grace", b"Edsger", b"Barbara"];
        let ids = [14u64, 16, 32, 4398046517420];
        let stored = store_props(
            &mut db,
            "person",
            &[
                ("firstName", PropValues::Str(&names)),
                ("id", PropValues::Int(&ids)),
            ],
        )
        .unwrap();
        let table = Catalog::load(&mut db)
            .unwrap()
            .node_by_name("person")
            .unwrap()
            .id;
        let loaded = load_props(&mut db, table).unwrap().unwrap();
        assert_eq!(loaded, stored);
        let mut reader = PropsReader::new(loaded);
        let mut out = Vec::new();
        for (row, name) in names.iter().enumerate() {
            out.clear();
            let col = reader.col("firstName").unwrap();
            reader.read_str(&mut db, col, row as u64, &mut out).unwrap();
            assert_eq!(&out, name);
            let col = reader.col("id").unwrap();
            assert_eq!(reader.read_int(&mut db, col, row as u64).unwrap(), ids[row]);
        }
        assert!(reader.col("lastName").is_none());
    }

    #[test]
    fn replacing_props_frees_the_old_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let a: Vec<&[u8]> = vec![b"a"; 4];
        store_props(&mut db, "person", &[("x", PropValues::Str(&a))]).unwrap();
        let before = db.db_header().block_count;
        for _ in 0..8 {
            store_props(&mut db, "person", &[("x", PropValues::Str(&a))]).unwrap();
        }
        // Freed blocks recycle instead of growing the file without
        // bound; one round of slack covers checkpoint staging.
        assert!(
            db.db_header().block_count <= before + 4,
            "blocks grew {} -> {}",
            before,
            db.db_header().block_count
        );
    }

    #[test]
    fn store_rejects_misaligned_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let short: Vec<&[u8]> = vec![b"a"; 3];
        let err = store_props(&mut db, "person", &[("x", PropValues::Str(&short))]).unwrap_err();
        assert!(err.to_string().contains("3 values over 4 rows"), "{err}");
        let four: Vec<&[u8]> = vec![b"a"; 4];
        let err = store_props(&mut db, "nobody", &[("x", PropValues::Str(&four))]).unwrap_err();
        assert!(err.to_string().contains("no node table"), "{err}");
    }

    #[test]
    fn decode_rejects_hostile_input() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = setup(dir.path());
        let vals: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let good = store_props(&mut db, "person", &[("x", PropValues::Str(&vals))])
            .unwrap()
            .encode();
        assert!(PropsDirectory::decode(&good).is_ok());
        for len in 0..good.len() {
            assert!(
                PropsDirectory::decode(&good[..len]).is_err(),
                "prefix {len}"
            );
        }
        let mut bad = good.clone();
        bad[0] = 99;
        assert!(PropsDirectory::decode(&bad).is_err());
        // A hostile column count must not size an allocation.
        let mut hostile = good.clone();
        hostile[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(PropsDirectory::decode(&hostile).is_err());
        let mut trailing = good;
        trailing.push(0);
        assert!(PropsDirectory::decode(&trailing).is_err());
    }
}
