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

use std::sync::Arc;

use zu_common::{FloatBits, IdMap, IntBits, LogicalType, Result, ZuError};
use zu_vector::{MorselArena, PhysType, SelVector, ValueVector, str_vector};

use zu_query::frame::FrameSet;
pub use zu_query::snapshot::{
    ColId, ColType, CsrPin, Dir, FuncCol, GroupId, RelId, SCAN_ROWS, ScanChunk, Snapshot, TableId,
    ZonePred,
};

use crate::deleted::Deleted;
use crate::write::Patches;
use crate::zu1::algo;
use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;
use crate::zu1::graph::{Direction, GraphReader};
use crate::zu1::props::{PropsReader, load_props, load_rel_props};

/// Whether every neighbor list is to be read on its own, whatever the
/// group it sits in costs. Read once: this sits under the walk and the
/// answer cannot change while a process runs.
fn forced_point_reads() -> bool {
    static FORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FORCED.get_or_init(|| std::env::var("ZU_POINT_READS").as_deref() == Ok("1"))
}

fn direction(dir: Dir) -> Direction {
    match dir {
        Dir::Fwd => Direction::Fwd,
        Dir::Bwd => Direction::Bwd,
    }
}

/// The file handle behind a snapshot: borrowed for the common case of
/// reading through an open handle, owned for forks handed to parallel
/// workers. Forked handles share the block cache and decoded pools
/// with their parent, so a fork reads warm.
enum Db<'a> {
    Borrowed(&'a mut Zu1File),
    /// `Some` for the snapshot's whole life; the option only exists so
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

/// The vectorized view of one open zu1 file at its current epoch.
pub struct Zu1Snapshot<'a> {
    db: Db<'a>,
    catalog: Catalog,
    readers: IdMap<u32, GraphReader>,
    props: IdMap<u32, Option<PropsReader>>,
    /// Decoded-chunk scratch shared by scan and gather, with the
    /// column it currently holds so a predicate column requested in
    /// the output decodes once, not twice.
    scratch: Vec<u64>,
    str_bytes: Vec<u8>,
    str_ends: Vec<u64>,
    /// The rows a `DELETE` took away, the file's chains with whatever a
    /// commit has taken away since laid over them, read on the first
    /// scan and kept. `None` is "not read yet" and not "nothing is
    /// deleted": the read costs a table index decode, so a snapshot
    /// that never scans a node table never pays it.
    gone: Option<Deleted>,
    /// The cells committed since the last fold, by table, which the
    /// props readers read through. A fork carries them, so every worker
    /// of a parallel query reads the same database.
    patches: Arc<Patches>,
    /// The frames registered on the session this snapshot was opened
    /// for. They are read where they lie and have nothing to do with
    /// the file, so a fork shares them rather than reopening anything.
    frames: Arc<FrameSet>,
}

/// What a snapshot learns while it reads, kept in a value of its own
/// so a caller that opens a snapshot per query can carry it over.
///
/// A snapshot is a view of one epoch and is built and dropped around a
/// single execution, but the table readers behind it are not per query
/// at all: loading one reads its directory out of the file, and on a
/// small graph that load costs more than the query it was opened for.
/// The buffers are here for the same reason, so a scan does not start
/// by growing a fresh vector every time.
///
/// Everything in here is valid for as long as the epoch it was read at
/// is, which is the caller's business: hand it back to
/// [`Zu1Snapshot::with_cache`] while the epoch holds and drop it when
/// the epoch moves.
#[derive(Default)]
pub struct SnapshotCache {
    pub(crate) readers: IdMap<u32, GraphReader>,
    pub(crate) props: IdMap<u32, Option<PropsReader>>,
    scratch: Vec<u64>,
    str_bytes: Vec<u8>,
    str_ends: Vec<u64>,
    gone: Option<Deleted>,
    patches: Arc<Patches>,
    frames: Arc<FrameSet>,
}

impl SnapshotCache {
    /// Hands over the frames registered on the session, which the
    /// snapshots built from this cache read as tables of the graph.
    pub fn set_frames(&mut self, frames: Arc<FrameSet>) {
        self.frames = frames;
    }

    /// Hands the cached readers the cells a commit has left in the
    /// overlay, and keeps them for the readers a later snapshot loads.
    ///
    /// The readers are kept rather than dropped, which is the point: a
    /// commit that does not fold does not move the columns they
    /// describe, so the only thing that has changed about them is what
    /// they read on top.
    pub fn set_patches(&mut self, patches: Arc<Patches>, catalog: &Catalog) {
        for (table, reader) in &mut self.props {
            if let Some(reader) = reader {
                reader.set_patch(patches.cells.get(table).cloned());
                reader.set_added(patches.rows.get(table).cloned());
                reader.set_marks(patches.marks.get(table).cloned());
            }
        }
        for (rel, reader) in &mut self.readers {
            reader.set_edges(patches.edges.get(rel).cloned());
            let [from, to] = patches.grown(catalog, *rel);
            reader.set_grown(from, to);
        }
        // The chains stay: they are the file's and the file has not
        // moved. What sits over them is the patch, and this is a new
        // one.
        if let Some(gone) = &mut self.gone {
            gone.overlay(&patches.gone);
        }
        self.patches = patches;
    }
}

impl<'a> Zu1Snapshot<'a> {
    pub fn new(db: &'a mut Zu1File, catalog: Catalog) -> Self {
        Self::with_cache(db, catalog, SnapshotCache::default())
    }

    /// A snapshot that starts from what an earlier one already read.
    pub fn with_cache(db: &'a mut Zu1File, catalog: Catalog, cache: SnapshotCache) -> Self {
        Zu1Snapshot {
            db: Db::Borrowed(db),
            catalog,
            readers: cache.readers,
            props: cache.props,
            scratch: cache.scratch,
            str_bytes: cache.str_bytes,
            str_ends: cache.str_ends,
            gone: cache.gone,
            patches: cache.patches,
            frames: cache.frames,
        }
    }

    /// Takes back what this snapshot read, to hand to the next one.
    pub fn into_cache(self) -> SnapshotCache {
        SnapshotCache {
            readers: self.readers,
            props: self.props,
            scratch: self.scratch,
            str_bytes: self.str_bytes,
            str_ends: self.str_ends,
            gone: self.gone,
            patches: self.patches,
            frames: self.frames,
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
        let mut reader = GraphReader::load_table(&mut self.db, &name)?;
        reader.set_edges(self.patches.edges.get(&rel).cloned());
        let [from, to] = self.patches.grown(&self.catalog, rel);
        reader.set_grown(from, to);
        self.readers.insert(rel, reader);
        Ok(())
    }

    fn ensure_props(&mut self, table: u32) -> Result<()> {
        if self.props.contains_key(&table) {
            return Ok(());
        }
        let patch = self.patches.cells.get(&table).cloned();
        let added = self.patches.rows.get(&table).cloned();
        let marks = self.patches.marks.get(&table).cloned();
        let reader = load_props(&mut self.db, table)?.map(|directory| {
            let mut reader = PropsReader::new(directory);
            reader.set_patch(patch);
            reader.set_added(added);
            reader.set_marks(marks);
            reader
        });
        self.props.insert(table, reader);
        Ok(())
    }

    /// The same for a rel table's edge columns, which hang off its
    /// group directory rather than off the table index. They share the
    /// one map because a catalog id names a node table or a rel table
    /// and never both.
    fn ensure_rel_props(&mut self, rel: u32) -> Result<()> {
        if self.props.contains_key(&rel) {
            return Ok(());
        }
        let patch = self.patches.cells.get(&rel).cloned();
        let added = self.patches.rows.get(&rel).cloned();
        let reader = load_rel_props(&mut self.db, rel)?.map(|directory| {
            let mut reader = PropsReader::new(directory);
            reader.set_patch(patch);
            reader.set_added(added);
            reader
        });
        self.props.insert(rel, reader);
        Ok(())
    }

    /// The row a commit added under key `key`, `None` when no unfolded
    /// row holds it. See [`crate::query::Zu1Graph`], which answers a
    /// key the same way.
    fn appended_key(&mut self, table: u32, key: u64) -> Result<Option<u64>> {
        let Some(rows) = self.patches.rows.get(&table).cloned() else {
            return Ok(None);
        };
        self.ensure_props(table)?;
        let Some(col) = self
            .props
            .get(&table)
            .expect("just loaded")
            .as_ref()
            .and_then(|reader| reader.col("id"))
        else {
            return Ok(None);
        };
        Ok(rows.row_with(col, key))
    }

    /// Reads the deleted set once per snapshot, or per epoch when a
    /// caller carries the cache from one snapshot to the next, and lays
    /// whatever a commit has taken away since the last fold over it.
    ///
    /// A file nothing is deferred against answers with the chains
    /// themselves, which is every read of a database that is only
    /// written to in bulk. The patch is laid over rather than merged
    /// in, so it costs a reference count per table it names and
    /// nothing per row, which is what stops a delete getting slower
    /// the more the file has deleted. [`Self::set_patches`] lays down
    /// each new one.
    fn ensure_gone(&mut self) -> Result<&Deleted> {
        if self.gone.is_none() {
            let mut gone = Deleted::load(&mut self.db)?;
            gone.overlay(&self.patches.gone);
            self.gone = Some(gone);
        }
        Ok(self.gone.as_ref().expect("just loaded"))
    }
}

/// Walks a chunk's rows against the rows of it a delete took away.
///
/// Both sides are ascending, so this is a merge and not a lookup per
/// row: the cursor only ever moves forward, whatever the chunk's rows
/// come out of. There are two runs rather than one because the
/// tombstone set is kept as the chains plus what the commits since the
/// last fold took away, and a cursor over each is cheaper than putting
/// the two together for every statement. See [`crate::deleted`].
struct Tombstones<'a> {
    dead: [&'a [u64]; 2],
    at: [usize; 2],
    base: u64,
}

impl Tombstones<'_> {
    fn gone(&mut self, row: u16) -> bool {
        let offset = self.base + u64::from(row);
        let mut gone = false;
        for (run, at) in self.dead.iter().zip(&mut self.at) {
            while *at < run.len() && run[*at] < offset {
                *at += 1;
            }
            gone |= *at < run.len() && run[*at] == offset;
        }
        gone
    }
}

/// The rows of one chunk a delete left behind, as a selection.
///
/// `dead` holds the chunk's deleted rows, one ascending run per half of
/// the tombstone set, and `sel` what the scan had already selected,
/// identity when it has none. An empty answer means the whole chunk is
/// gone, which a scan reports the way it reports a chunk the zone map
/// ruled out.
fn survivors(
    dead: (&[u64], &[u64]),
    row_base: u64,
    rows: usize,
    sel: Option<&SelVector>,
    arena: &mut MorselArena,
) -> SelVector {
    let mut tombs = Tombstones {
        dead: [dead.0, dead.1],
        at: [0, 0],
        base: row_base,
    };
    let mut out = SelVector::with_capacity(arena, sel.map_or(rows, SelVector::len));
    match sel {
        Some(sel) => {
            for &row in sel.as_slice() {
                if !tombs.gone(row) {
                    out.push(row);
                }
            }
        }
        None => {
            for row in 0..rows as u16 {
                if !tombs.gone(row) {
                    out.push(row);
                }
            }
        }
    }
    out
}

/// The props reader of `table`, which must have properties stored.
/// A node table and a rel table never share an id, so this reads
/// either one.
fn props_of(props: &mut IdMap<u32, Option<PropsReader>>, table: u32) -> Result<&mut PropsReader> {
    props
        .get_mut(&table)
        .expect("just loaded")
        .as_mut()
        .ok_or_else(|| ZuError::InvalidArgument(format!("table {table} has no properties stored")))
}

/// The column `name` names, as the vector layer would carry it, or
/// `None` when the reader holds nothing readable under that name.
///
/// The vector layer types a column as one of three things, so a column
/// that is none of them is not resolvable and the compiler declines the
/// plan rather than reading a date as though it were a count. The row
/// at a time executor reads those, and widening this is the vector
/// layer's own change (G1, the `PhysType` move).
///
/// A vector carries a validity mask of its own, but a scan does not
/// fill one from storage yet, so a column that holds a null is not
/// resolvable here either. The compiler leans on that: a value read
/// through one of these is a value and never a null.
fn vector_col(reader: &PropsReader, name: &str) -> Option<(ColId, ColType)> {
    reader.col(name).and_then(|ix| {
        if reader.columns()[ix].validity.is_some() {
            return None;
        }
        Some((ix as ColId, lane_type(&reader.columns()[ix].ty)?))
    })
}

/// How the vector layer types one stored column, `None` for a type it
/// has no lane for. A 32 bit float is not one of them: it is stored in
/// its own width and the layer has one float type, so widening it is a
/// conversion rather than a read.
fn lane_type(ty: &LogicalType) -> Option<ColType> {
    match ty {
        LogicalType::Int {
            signed: true,
            bits: IntBits::B64,
            ..
        } => Some(ColType::Int),
        LogicalType::Float {
            bits: FloatBits::B64,
            ..
        } => Some(ColType::Float),
        LogicalType::Str { .. } => Some(ColType::Str),
        _ => None,
    }
}

/// The physical type a lane column's stored words carry. A float is
/// written as `f64::to_bits`, so the eight bytes the decoder hands back
/// are already the value and the vector only has to be told what they
/// are. Nothing converts.
fn lane_phys(ty: &LogicalType) -> PhysType {
    match lane_type(ty) {
        Some(ColType::Float) => PhysType::Float64,
        _ => PhysType::Int64,
    }
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
        if let Some(frame) = self.frames.get(table) {
            return Ok(frame.rows());
        }
        // The catalog is the file's and a commit that did not fold
        // wrote no catalog, so the rows it added are on top of it.
        Ok(self
            .catalog
            .node_by_id(table)
            .ok_or_else(|| ZuError::InvalidArgument(format!("unknown node table {table}")))?
            .node_count
            + self.patches.added_rows(table))
    }

    fn resolve_col(&mut self, table: TableId, name: &str) -> Result<Option<(ColId, ColType)>> {
        if let Some(frame) = self.frames.get(table) {
            return Ok(frame.resolve(name));
        }
        self.ensure_props(table)?;
        let Some(reader) = self.props.get(&table).expect("just loaded") else {
            return Ok(None);
        };
        Ok(vector_col(reader, name))
    }

    fn resolve_rel_col(&mut self, rel: RelId, name: &str) -> Result<Option<(ColId, ColType)>> {
        self.ensure_rel_props(rel)?;
        let Some(reader) = self.props.get(&rel).expect("just loaded") else {
            return Ok(None);
        };
        Ok(vector_col(reader, name))
    }

    fn scan(
        &mut self,
        table: TableId,
        chunk: u64,
        cols: &[ColId],
        pred: Option<&ZonePred>,
        arena: &mut MorselArena,
    ) -> Result<Option<ScanChunk>> {
        // A frame is somebody else's memory and the whole of what a
        // scan of one does is point at it. Nothing here applies: there
        // are no zones to skip by, no chunks to decode and no
        // tombstones, because nothing has ever written one.
        if let Some(frame) = self.frames.get(table) {
            return Ok(Arc::clone(frame).scan(chunk, cols, pred, arena));
        }
        // A bare row-id scan needs only the catalog extent; tables
        // loaded without properties still scan and expand fine.
        if cols.is_empty() && pred.is_none() {
            let total = self.table_rows(table)?;
            let row_base = chunk * SCAN_ROWS as u64;
            if row_base >= total {
                return Ok(None);
            }
            let rows = (total - row_base).min(SCAN_ROWS as u64) as u32;
            let dead = self.ensure_gone()?.span(table, row_base, u64::from(rows));
            let mut sel = None;
            if !dead.0.is_empty() || !dead.1.is_empty() {
                let alive = survivors(dead, row_base, rows as usize, None, arena);
                if alive.is_empty() {
                    return Ok(None);
                }
                sel = Some(alive);
            }
            return Ok(Some(ScanChunk {
                row_base,
                rows,
                sel,
                columns: Vec::new(),
            }));
        }
        self.ensure_props(table)?;
        self.ensure_gone()?;
        let Self {
            db,
            props,
            scratch,
            str_bytes,
            str_ends,
            gone,
            ..
        } = self;
        let gone = gone.as_ref().expect("just loaded");
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
            // The zone rather than the stored bounds, because a write
            // the column does not hold yet can sit outside them and a
            // skip here is the whole scan.
            if let Some((min, max)) = reader.zone(col)
                && p.skips(min, max)
            {
                return Ok(None);
            }
            if let Some((lo, hi)) = reader.chunk_bounds(db, col, chunk_ix)?
                && p.skips(lo, hi)
            {
                return Ok(None);
            }
            reader.scan_int_chunk(db, col, chunk_ix, scratch)?;
            have = Some(col);
            // The count comes first and the selection only if it is
            // worth carrying. Counting is a branchless pass the
            // compiler vectorizes; building the selection is a push per
            // surviving row, and every operator above a chunk that has
            // one reads its rows through it rather than straight down
            // the vector. The predicate is still in the residual
            // program, so a chunk handed on whole gets the same answer
            // from the kernels; what the selection buys is the rows
            // those kernels never see. Below half the chunk that is
            // worth having and above it the thinning costs more than
            // the rows it removes, which is where a bound like
            // `age > 30` over a table of ages sits.
            let kept = scratch
                .iter()
                .take(rows)
                .filter(|&&v| v >= p.lo && v <= p.hi)
                .count();
            if kept == 0 {
                return Ok(None);
            }
            if kept * 2 <= rows {
                let mut s = SelVector::with_capacity(arena, rows);
                for (i, &v) in scratch.iter().take(rows).enumerate() {
                    if v >= p.lo && v <= p.hi {
                        s.push(i as u16);
                    }
                }
                sel = Some(s);
            }
        }
        // What a delete took away is thinned out here rather than left
        // to a kernel above, because a tombstone is not in the residual
        // program: nothing further up rechecks it, so the selection is
        // the only place the row can go missing. It is built whatever
        // the density, for the same reason.
        let dead = gone.span(table, row_base, rows as u64);
        if !dead.0.is_empty() || !dead.1.is_empty() {
            let alive = survivors(dead, row_base, rows, sel.as_ref(), arena);
            if alive.is_empty() {
                return Ok(None);
            }
            sel = Some(alive);
        }
        let mut columns = Vec::with_capacity(cols.len());
        for &c in cols {
            let col = check_col(reader, c)?;
            if reader.columns()[col].is_lane() {
                if have != Some(col) {
                    reader.scan_int_chunk(db, col, chunk_ix, scratch)?;
                    have = Some(col);
                }
                columns.push(ValueVector::flat_from(
                    arena,
                    lane_phys(&reader.columns()[col].ty),
                    &scratch[..rows],
                ));
            } else {
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
        let (offsets, neighbors) =
            reader.csr_group(&mut self.db, group as usize, direction(dir))?;
        Ok(CsrPin { offsets, neighbors })
    }

    fn list_threshold(&mut self, rel: RelId, group: GroupId, dir: Dir) -> Result<Option<usize>> {
        // The point path only comes up on groups far larger than a test
        // builds, so ZU_POINT_READS=1 asks for it everywhere and the
        // whole query suite runs through it. Nothing outside a test run
        // sets this.
        if forced_point_reads() {
            return Ok(None);
        }
        self.ensure_reader(rel)?;
        let reader = self.readers.get(&rel).expect("just loaded");
        // A group holding edges no fold has sealed is built on every
        // pin rather than decoded once, so while that is true the
        // caller reads one list at a time however many it wants.
        if !reader.pinnable(group as usize, direction(dir)) {
            return Ok(None);
        }
        // Reading one list decodes about four chunks: two on the
        // offsets to find where the list starts and ends, and one or
        // two on the neighbors to cover it. The pin decodes every
        // chunk of the group once, so it starts paying for itself at
        // roughly a quarter as many lists as the group has chunks,
        // and a scan morsel is far past that while a seed is far
        // short of it.
        Ok(Some(reader.list_chunks(group as usize, direction(dir)) / 4))
    }

    fn list_into(&mut self, rel: RelId, node: u64, dir: Dir, out: &mut Vec<u64>) -> Result<()> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        let reader = readers.get(&rel).expect("just loaded");
        reader.neighbors_dir_into(db, node, direction(dir), out)
    }

    fn list_ords_into(
        &mut self,
        rel: RelId,
        node: u64,
        dir: Dir,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        self.ensure_reader(rel)?;
        let Self {
            db,
            readers,
            scratch,
            ..
        } = self;
        let reader = readers.get_mut(&rel).expect("just loaded");
        // The reader writes a list at a time and the caller collects
        // several, so the read lands in the snapshot's own buffer and
        // is copied onto the end of theirs.
        reader.neighbor_ordinals_into(db, node, direction(dir), scratch)?;
        out.extend_from_slice(scratch);
        Ok(())
    }

    fn gather(
        &mut self,
        table: TableId,
        col: ColId,
        rows: &[u64],
        arena: &mut MorselArena,
    ) -> Result<ValueVector> {
        if let Some(frame) = self.frames.get(table) {
            return Ok(Arc::clone(frame).gather(col, rows, arena));
        }
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
        if reader.columns()[ix].is_lane() {
            reader.gather_int(db, ix, rows, scratch)?;
            let phys = lane_phys(&reader.columns()[ix].ty);
            Ok(ValueVector::flat_from(arena, phys, scratch))
        } else {
            reader.gather_str(db, ix, rows, str_bytes, str_ends)?;
            Ok(str_views(arena, str_bytes, str_ends))
        }
    }

    // An edge's row is its ordinal and a node's row is its offset, and
    // the reader underneath does not know the difference, so the two
    // gathers differ only in which directory they load.
    fn gather_rel(
        &mut self,
        rel: RelId,
        col: ColId,
        ords: &[u64],
        arena: &mut MorselArena,
    ) -> Result<ValueVector> {
        self.ensure_rel_props(rel)?;
        let Self {
            db,
            props,
            scratch,
            str_bytes,
            str_ends,
            ..
        } = self;
        let reader = props_of(props, rel)?;
        let ix = check_col(reader, col)?;
        if reader.columns()[ix].is_lane() {
            reader.gather_int(db, ix, ords, scratch)?;
            let phys = lane_phys(&reader.columns()[ix].ty);
            Ok(ValueVector::flat_from(arena, phys, scratch))
        } else {
            reader.gather_str(db, ix, ords, str_bytes, str_ends)?;
            Ok(str_views(arena, str_bytes, str_ends))
        }
    }

    fn seek_key(&mut self, table: TableId, key: u64) -> Result<Option<u64>> {
        // A frame has no key index and no tombstones, so a key is the
        // row, which is the dense contract every table without a keyed
        // rel over it already reads by.
        if let Some(frame) = self.frames.get(table) {
            return Ok((key < frame.rows()).then_some(key));
        }
        // The key index lives in the group directory of a rel table
        // loaded over this node table's rows, so find one and ask it.
        // No keyed rel means the dense contract, where the id is the
        // offset.
        let row = match self
            .catalog
            .rel_tables()
            .iter()
            .find(|r| r.from == table)
            .map(|r| r.id)
        {
            Some(rel) => {
                self.ensure_reader(rel)?;
                let found = {
                    let Self { db, readers, .. } = self;
                    let reader = readers.get_mut(&rel).expect("just loaded");
                    match reader.directory().keys.is_none() {
                        true => Some(key),
                        false => reader.lookup_key(db, key)?,
                    }
                };
                match found {
                    Some(row) => Some(row),
                    // The index is the file's and a deferred commit
                    // rewrites nothing, so a row it added is in no
                    // index and the patch is where its key is.
                    None => self.appended_key(table, key)?,
                }
            }
            None => Some(key),
        };
        let rows = self.table_rows(table)?;
        let Some(row) = row.filter(|&r| r < rows) else {
            return Ok(None);
        };
        // A key still names the row it always named after a delete,
        // because offsets do not move and the index is not rewritten,
        // so the tombstone is what says the row is gone.
        Ok((!self.ensure_gone()?.holds(table, row)).then_some(row))
    }

    fn degree_batch(&mut self, rel: RelId, nodes: &[u64], dir: Dir) -> Result<u64> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        readers
            .get_mut(&rel)
            .expect("just loaded")
            .degree_batch(db, nodes, direction(dir))
    }

    fn degrees(&mut self, rel: RelId, nodes: &[u64], dir: Dir, out: &mut [u64]) -> Result<()> {
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        readers
            .get_mut(&rel)
            .expect("just loaded")
            .degrees_into(db, nodes, direction(dir), out)
    }

    fn table_function(&mut self, name: &str, rel: RelId, args: &[i64]) -> Result<Option<FuncCol>> {
        // pagerank is missing on purpose: its rank is a float and a
        // compiled column carries an integer or a string, so that call
        // stays with the old engine until the column does floats.
        if !matches!(name, "wcc" | "louvain" | "bfs" | "sssp") {
            return Ok(None);
        }
        self.ensure_reader(rel)?;
        let Self { db, readers, .. } = self;
        let reader = readers.get_mut(&rel).expect("just loaded");
        let (values, null) = match name {
            "wcc" => (algo::wcc(db, reader)?, Vec::new()),
            "louvain" => (algo::louvain(db, reader)?, Vec::new()),
            // bfs and sssp are the same frontier over a different view
            // of the rel table, and both mark what they never reached.
            _ => {
                let Some(&source) = args.first() else {
                    return Ok(None);
                };
                let dist = if name == "bfs" {
                    algo::bfs(db, reader, source as u64)?
                } else {
                    algo::sssp(db, reader, source as u64)?
                };
                let null = dist.iter().map(|&d| d == u64::MAX).collect();
                (dist, null)
            }
        };
        Ok(Some(FuncCol {
            // A label and a distance are row ids and hop counts, both
            // small; the cast only ever matters for the unreached
            // sentinel, which is the null beside it.
            values: values.into_iter().map(|v| v as i64).collect(),
            null,
        }))
    }

    fn fork(&self) -> Option<Box<dyn Snapshot + Send>> {
        // A reopened handle carries this handle's in-memory header and
        // shares its block cache and decoded pools, so the fork reads
        // the same epoch and reads it warm.
        let db = self.db.reopen().ok()?;
        Some(Box::new(Zu1Snapshot {
            db: Db::Owned(Some(Box::new(db))),
            catalog: self.catalog.clone(),
            readers: IdMap::default(),
            props: IdMap::default(),
            scratch: Vec::new(),
            str_bytes: Vec::new(),
            str_ends: Vec::new(),
            // Read once and shared out: the set describes the epoch
            // both handles read, and it is the same few words either
            // way, so a worker starts with it rather than reading the
            // table index again.
            gone: self.gone.clone(),
            patches: Arc::clone(&self.patches),
            // Frames are the caller's memory and have nothing to do
            // with the file, so a fork shares them rather than
            // reopening anything.
            frames: Arc::clone(&self.frames),
        }))
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
        assert_eq!(snap.seek_key(table, 5 * 2 + 10).unwrap(), Some(5));
        assert_eq!(snap.seek_key(table, 11).unwrap(), None);
        assert_eq!(snap.degree_batch(rel, &[1, 1, 0], Dir::Fwd).unwrap(), 5);
    }

    #[test]
    fn forks_read_the_same_epoch_through_shared_pools() {
        let dir = tempfile::tempdir().unwrap();
        let (mut db, table, rel) = setup(&dir.path().join("fork.zu1"));
        let pools = db.pools();
        let mut snap = Zu1Snapshot::open(&mut db).unwrap();
        let pin = snap.csr(rel, 0, Dir::Fwd).unwrap();
        assert_eq!(pin.degree(1), 2);
        let cold = pools.adjacency.stats().misses;
        let mut fork = snap.fork().expect("zu1 snapshots fork");
        assert_eq!(fork.epoch(), snap.epoch());
        // The fork's first pin lands on the decode the parent already
        // paid for: the pools are shared, so no new miss.
        let fpin = fork.csr(rel, 0, Dir::Fwd).unwrap();
        assert_eq!(fpin.list(1), &[2, 3]);
        assert_eq!(
            pools.adjacency.stats().misses,
            cold,
            "fork should reuse the parent's decode"
        );
        // The fork answers the whole surface on its own handle.
        let mut arena = MorselArena::new();
        assert_eq!(fork.table_rows(table).unwrap(), N);
        assert_eq!(fork.seek_key(table, 20).unwrap(), Some(5));
        let (a, _) = fork.resolve_col(table, "a").unwrap().unwrap();
        let ints = fork.gather(table, a, &[2999, 0], &mut arena).unwrap();
        assert_eq!(ints.values::<u64>(), &[2999 * 3, 0]);
        assert_eq!(fork.degree_batch(rel, &[0, 1], Dir::Fwd).unwrap(), 3);
    }
}
