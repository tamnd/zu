//! [`Snapshot`] over a zu1 file: the vectorized read path. Scans skip
//! chunks by zone map before touching payload bytes and decode the
//! predicate column first, so a filter that misses a chunk costs a
//! fence comparison and a filter that hits decodes the other columns
//! only for chunks with survivors. CSR groups pin out of the decoded
//! pools as shared slices, and gathers batch arbitrary row sets with
//! one decode per touched chunk.
//!
//! Like [`Zu1Graph`], the readers and their directories load lazily
//! and stay cached, so a snapshot held across queries reads warm.
//!
//! [`Zu1Graph`]: crate::query::Zu1Graph

use std::collections::HashMap;

use zu_common::{Result, ZuError};
use zu_vector::{MorselArena, PhysType, SelVector, ValueVector, str_vector};

pub use zu_query::snapshot::{
    ColId, ColType, CsrPin, Dir, GroupId, RelId, SCAN_ROWS, ScanChunk, Snapshot, TableId, ZonePred,
};

use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;
use crate::zu1::graph::{Direction, GraphReader};
use crate::zu1::props::{PropType, PropsReader, load_props};

fn direction(dir: Dir) -> Direction {
    match dir {
        Dir::Fwd => Direction::Fwd,
        Dir::Bwd => Direction::Bwd,
    }
}

/// The vectorized view of one open zu1 file at its current epoch.
pub struct Zu1Snapshot<'a> {
    db: &'a mut Zu1File,
    catalog: Catalog,
    readers: HashMap<u32, GraphReader>,
    props: HashMap<u32, Option<PropsReader>>,
    /// Decoded-chunk scratch shared by scan and gather, with the
    /// column it currently holds so a predicate column requested in
    /// the output decodes once, not twice.
    scratch: Vec<u64>,
    str_bytes: Vec<u8>,
    str_ends: Vec<u64>,
}

impl<'a> Zu1Snapshot<'a> {
    pub fn new(db: &'a mut Zu1File, catalog: Catalog) -> Self {
        Zu1Snapshot {
            db,
            catalog,
            readers: HashMap::new(),
            props: HashMap::new(),
            scratch: Vec::new(),
            str_bytes: Vec::new(),
            str_ends: Vec::new(),
        }
    }

    /// Loads the catalog from the file itself.
    pub fn open(db: &'a mut Zu1File) -> Result<Self> {
        let catalog = Catalog::load(db)?;
        Ok(Self::new(db, catalog))
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
        let reader = GraphReader::load_table(self.db, &name)?;
        self.readers.insert(rel, reader);
        Ok(())
    }

    fn ensure_props(&mut self, table: u32) -> Result<()> {
        if self.props.contains_key(&table) {
            return Ok(());
        }
        let reader = load_props(self.db, table)?.map(PropsReader::new);
        self.props.insert(table, reader);
        Ok(())
    }
}

/// The props reader of `table`, which must have properties stored.
fn props_of(props: &mut HashMap<u32, Option<PropsReader>>, table: u32) -> Result<&mut PropsReader> {
    props
        .get_mut(&table)
        .expect("just loaded")
        .as_mut()
        .ok_or_else(|| {
            ZuError::InvalidArgument(format!("node table {table} has no properties stored"))
        })
}

fn check_col(reader: &PropsReader, col: ColId) -> Result<usize> {
    let ix = col as usize;
    if ix >= reader.columns().len() {
        return Err(ZuError::InvalidArgument(format!(
            "column {col} out of 0..{}",
            reader.columns().len()
        )));
    }
    Ok(ix)
}

/// Rebuilds `bytes`/`ends` blob output as one arena string vector.
fn str_views(arena: &mut MorselArena, bytes: &[u8], ends: &[u64]) -> ValueVector {
    let mut views: Vec<&[u8]> = Vec::with_capacity(ends.len());
    let mut lo = 0usize;
    for &e in ends {
        views.push(&bytes[lo..e as usize]);
        lo = e as usize;
    }
    str_vector(arena, &views)
}

impl Snapshot for Zu1Snapshot<'_> {
    fn epoch(&self) -> u64 {
        self.db.db_header().epoch
    }

    fn table_rows(&mut self, table: TableId) -> Result<u64> {
        Ok(self
            .catalog
            .node_by_id(table)
            .ok_or_else(|| ZuError::InvalidArgument(format!("unknown node table {table}")))?
            .node_count)
    }

    fn resolve_col(&mut self, table: TableId, name: &str) -> Result<Option<(ColId, ColType)>> {
        self.ensure_props(table)?;
        let Some(reader) = self.props.get(&table).expect("just loaded") else {
            return Ok(None);
        };
        Ok(reader.col(name).map(|ix| {
            let ty = match reader.columns()[ix].ty {
                PropType::Int => ColType::Int,
                PropType::Str => ColType::Str,
            };
            (ix as ColId, ty)
        }))
    }

    fn scan(
        &mut self,
        table: TableId,
        chunk: u64,
        cols: &[ColId],
        pred: Option<&ZonePred>,
        arena: &mut MorselArena,
    ) -> Result<Option<ScanChunk>> {
        self.ensure_props(table)?;
        let Self {
            db,
            props,
            scratch,
            str_bytes,
            str_ends,
            ..
        } = self;
        let reader = props_of(props, table)?;
        let row_base = chunk * SCAN_ROWS as u64;
        if row_base >= reader.rows() {
            return Ok(None);
        }
        let rows = (reader.rows() - row_base).min(SCAN_ROWS as u64) as usize;
        let chunk_ix = chunk as usize;
        // The column the scratch currently holds decoded.
        let mut have = None;
        let mut sel = None;
        if let Some(p) = pred {
            let col = check_col(reader, p.col)?;
            let m = reader.meta(col);
            if m.value_count > 0 && p.skips(m.min, m.max) {
                return Ok(None);
            }
            if let Some((lo, hi)) = reader.chunk_bounds(db, col, chunk_ix)?
                && p.skips(lo, hi)
            {
                return Ok(None);
            }
            reader.scan_int_chunk(db, col, chunk_ix, scratch)?;
            have = Some(col);
            // The selection builds in the unsigned domain, the same
            // ordering the zone maps skip by.
            let mut s = SelVector::with_capacity(arena, rows);
            for (i, &v) in scratch.iter().enumerate() {
                if v >= p.lo && v <= p.hi {
                    s.push(i as u16);
                }
            }
            if s.is_empty() {
                return Ok(None);
            }
            if s.len() < rows {
                sel = Some(s);
            }
        }
        let mut columns = Vec::with_capacity(cols.len());
        for &c in cols {
            let col = check_col(reader, c)?;
            match reader.columns()[col].ty {
                PropType::Int => {
                    if have != Some(col) {
                        reader.scan_int_chunk(db, col, chunk_ix, scratch)?;
                        have = Some(col);
                    }
                    columns.push(ValueVector::flat_from(
                        arena,
                        PhysType::Int64,
                        &scratch[..rows],
                    ));
                }
                PropType::Str => {
                    reader.scan_str_range(
                        db,
                        col,
                        row_base,
                        row_base + rows as u64,
                        str_bytes,
                        str_ends,
                    )?;
                    columns.push(str_views(arena, str_bytes, str_ends));
                }
            }
        }
        Ok(Some(ScanChunk {
            row_base,
            rows: rows as u32,
            sel,
            columns,
        }))
    }

    fn csr(&mut self, rel: RelId, group: GroupId, dir: Dir) -> Result<CsrPin> {
        self.ensure_reader(rel)?;
        let reader = self.readers.get(&rel).expect("just loaded");
        let (offsets, neighbors) = reader.csr_group(self.db, group as usize, direction(dir))?;
        Ok(CsrPin { offsets, neighbors })
    }

    fn gather(
        &mut self,
        table: TableId,
        col: ColId,
        rows: &[u64],
        arena: &mut MorselArena,
    ) -> Result<ValueVector> {
        self.ensure_props(table)?;
        let Self {
            db,
            props,
            scratch,
            str_bytes,
            str_ends,
            ..
        } = self;
        let reader = props_of(props, table)?;
        let ix = check_col(reader, col)?;
        match reader.columns()[ix].ty {
            PropType::Int => {
                reader.gather_int(db, ix, rows, scratch)?;
                Ok(ValueVector::flat_from(arena, PhysType::Int64, scratch))
            }
            PropType::Str => {
                reader.gather_str(db, ix, rows, str_bytes, str_ends)?;
                Ok(str_views(arena, str_bytes, str_ends))
            }
        }
    }

    fn lookup_pk(&mut self, rel: RelId, key: u64) -> Result<Option<u64>> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        readers
            .get_mut(&rel)
            .expect("just loaded")
            .lookup_key(db, key)
    }

    fn degree_batch(&mut self, rel: RelId, nodes: &[u64], dir: Dir) -> Result<u64> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        readers
            .get(&rel)
            .expect("just loaded")
            .degree_batch(db, nodes, direction(dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zu1::graph::bulk_load_keyed;
    use crate::zu1::props::{PropValues, store_props};
    use zu_vector::StrView;

    const N: u64 = 3000;

    /// Three chunks of "person" rows keyed by `row * 2 + 10`, an
    /// ascending int column `a` at `row * 3`, a descending column `d`,
    /// and strings `s` as `v{row}`, plus a few "knows" edges.
    fn setup(path: &std::path::Path) -> (Zu1File, u32, u32) {
        let mut db = Zu1File::create(path).unwrap();
        let keys: Vec<u64> = (0..N).map(|i| i * 2 + 10).collect();
        bulk_load_keyed(
            &mut db,
            "person",
            "knows",
            N,
            &[(0, 1), (1, 2), (1, 3)],
            Some(&keys),
        )
        .unwrap();
        let asc: Vec<u64> = (0..N).map(|i| i * 3).collect();
        let desc: Vec<u64> = (0..N).map(|i| (N - 1 - i) * 3).collect();
        let strs: Vec<Vec<u8>> = (0..N).map(|i| format!("v{i}").into_bytes()).collect();
        let str_refs: Vec<&[u8]> = strs.iter().map(|v| v.as_slice()).collect();
        store_props(
            &mut db,
            "person",
            &[
                ("a", PropValues::Int(&asc)),
                ("d", PropValues::Int(&desc)),
                ("s", PropValues::Str(&str_refs)),
            ],
        )
        .unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let table = catalog.node_by_name("person").unwrap().id;
        let rel = catalog.rel_tables()[0].id;
        (db, table, rel)
    }

    fn str_at(v: &ValueVector, i: usize) -> Vec<u8> {
        let views: &[StrView] = v.values();
        views[i].bytes(v.str_buffers().unwrap()).to_vec()
    }

    #[test]
    fn scan_skips_by_zone_and_selects_survivors() {
        let dir = tempfile::tempdir().unwrap();
        let (mut db, table, _) = setup(&dir.path().join("scan.zu1"));
        let mut snap = Zu1Snapshot::open(&mut db).unwrap();
        let mut arena = MorselArena::new();
        assert_eq!(snap.table_rows(table).unwrap(), N);
        let (a, ty) = snap.resolve_col(table, "a").unwrap().unwrap();
        assert_eq!(ty, ColType::Int);
        let (s, ty) = snap.resolve_col(table, "s").unwrap().unwrap();
        assert_eq!(ty, ColType::Str);
        assert!(snap.resolve_col(table, "nope").unwrap().is_none());
        // Row 2500 lives in chunk 2; its value zones exclude chunks 0
        // and 1, so those come back as skips without a decode.
        let pred = ZonePred {
            col: a,
            lo: 2500 * 3,
            hi: 2500 * 3,
        };
        for chunk in [0u64, 1] {
            assert!(
                snap.scan(table, chunk, &[a, s], Some(&pred), &mut arena)
                    .unwrap()
                    .is_none(),
                "chunk {chunk} not skipped"
            );
        }
        let hit = snap
            .scan(table, 2, &[a, s], Some(&pred), &mut arena)
            .unwrap()
            .unwrap();
        assert_eq!(hit.row_base, 2048);
        assert_eq!(u64::from(hit.rows), N - 2048);
        let sel = hit.sel.as_ref().unwrap();
        assert_eq!(sel.as_slice(), &[(2500 - 2048) as u16]);
        let local = (2500 - 2048) as usize;
        assert_eq!(hit.columns[0].values::<u64>()[local], 2500 * 3);
        assert_eq!(str_at(&hit.columns[1], local), b"v2500");
        // Past the last chunk is the end of the scan, not an error.
        assert!(
            snap.scan(table, 3, &[a], Some(&pred), &mut arena)
                .unwrap()
                .is_none()
        );
        // Without a predicate every row of the tail chunk comes back
        // and sel stays None.
        let full = snap
            .scan(table, 2, &[a], None, &mut arena)
            .unwrap()
            .unwrap();
        assert!(full.sel.is_none());
        assert_eq!(full.columns[0].len(), (N - 2048) as usize);
        // A predicate whose range misses the whole segment skips at
        // the segment zone, and one nothing survives in also skips.
        let out_of_range = ZonePred {
            col: a,
            lo: N * 3 + 1,
            hi: u64::MAX,
        };
        assert!(
            snap.scan(table, 2, &[a], Some(&out_of_range), &mut arena)
                .unwrap()
                .is_none()
        );
        // The descending column has no chunk zones, so the scan
        // decodes and filters; row 2500's value lives at row N-1-2500
        // in chunk 0.
        let (d, _) = snap.resolve_col(table, "d").unwrap().unwrap();
        let dp = ZonePred {
            col: d,
            lo: 2500 * 3,
            hi: 2500 * 3,
        };
        let hit = snap
            .scan(table, 0, &[d], Some(&dp), &mut arena)
            .unwrap()
            .unwrap();
        assert_eq!(
            hit.sel.as_ref().unwrap().as_slice(),
            &[(N - 1 - 2500) as u16]
        );
    }

    #[test]
    fn csr_gather_and_lookups_read_batched() {
        let dir = tempfile::tempdir().unwrap();
        let (mut db, table, rel) = setup(&dir.path().join("batch.zu1"));
        let mut snap = Zu1Snapshot::open(&mut db).unwrap();
        let mut arena = MorselArena::new();
        assert!(snap.epoch() > 0);
        let pin = snap.csr(rel, 0, Dir::Fwd).unwrap();
        assert_eq!(pin.degree(1), 2);
        assert_eq!(pin.list(1), &[2, 3]);
        assert_eq!(pin.list(0), &[1]);
        assert_eq!(pin.offsets().len() as u64, N + 1);
        let bwd = snap.csr(rel, 0, Dir::Bwd).unwrap();
        assert_eq!(bwd.list(2), &[1]);
        // Gathers in caller order across chunks, both types.
        let rows = [2999u64, 0, 1024, 0];
        let (a, _) = snap.resolve_col(table, "a").unwrap().unwrap();
        let (s, _) = snap.resolve_col(table, "s").unwrap().unwrap();
        let ints = snap.gather(table, a, &rows, &mut arena).unwrap();
        let got: Vec<u64> = ints.values::<u64>().to_vec();
        assert_eq!(got, vec![2999 * 3, 0, 1024 * 3, 0]);
        let strs = snap.gather(table, s, &rows, &mut arena).unwrap();
        for (i, &r) in rows.iter().enumerate() {
            assert_eq!(str_at(&strs, i), format!("v{r}").into_bytes());
        }
        // Primary keys resolve to dense rows; a value between keys is
        // absent, not an error.
        assert_eq!(snap.lookup_pk(rel, 5 * 2 + 10).unwrap(), Some(5));
        assert_eq!(snap.lookup_pk(rel, 11).unwrap(), None);
        assert_eq!(snap.degree_batch(rel, &[1, 1, 0], Dir::Fwd).unwrap(), 5);
    }
}
