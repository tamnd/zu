//! Frames: tables of a session that live in somebody else's memory.
//!
//! A caller holding a dataframe wants to run a statement over it. The
//! rows are already columns in memory, laid out the way Arrow lays
//! columns out, which for the widths that matter is the way this engine
//! lays them out too: eight-byte words back to back, one bit a row for
//! a boolean, characters end to end with offsets cutting them up. So
//! registering one copies nothing. A frame is a name, the columns as
//! they lie, and a handle that keeps them alive; a scan of it builds
//! vectors that point straight at the caller's buffers, and the
//! executor above cannot tell the difference, because a vector was
//! never more than a pointer, a length and a type.
//!
//! What is not free is what does not match. A frame column of 32-bit
//! integers is read into the eight-byte lane a value at a time, and
//! microseconds become the nanoseconds the temporal types count in the
//! same pass. That is a widening into the morsel arena and not an
//! allocation, and it happens per scanned chunk rather than per
//! registration, so a statement that reads one column of a hundred
//! never touches the other ninety-nine.
//!
//! Everything that can fail is checked once, at registration: the
//! alignment a word read needs, an unsigned value too large for the
//! signed lane, a scale that would overflow. A read of a registered
//! frame cannot fail, which is what lets a scan be a loop.
//!
//! A frame is read only and has no edges. Nothing writes one, nothing
//! deletes from one, and a statement that tries is refused with the
//! reason rather than left to fail somewhere lower down.

use std::any::Any;
use std::ptr::NonNull;
use std::sync::Arc;

use zu_common::{FloatBits, IntBits, LogicalType, Result, Temporal, ZuError};
use zu_vector::{
    Aux, MorselArena, PhysType, RawBuf, SelVector, StrBuffers, StrView, ValueVector, VecEncoding,
};

use crate::exec::Value;
use crate::snapshot::{ColId, ColType, SCAN_ROWS, ScanChunk, TableId, ZonePred};

/// The highest table id there is, the top of the 14-bit field a node
/// reference packs a table into. Frames count down from here while the
/// catalog counts up from zero, so a registration never has to know
/// what the file holds and the two can only meet on a database with
/// sixteen thousand tables in it, which is refused when they do.
pub const TOP_TABLE_ID: TableId = (1 << 14) - 1;

fn refuse(reason: impl Into<String>) -> ZuError {
    ZuError::InvalidArgument(reason.into())
}

/// Where one column's bytes are and how they are laid out.
///
/// This is the caller's description of memory it owns, in the terms
/// Arrow puts it in, and it is deliberately not an Arrow type: the
/// engine never sees a schema, only widths and pointers, and mapping
/// one to the other is the client's business because the client is
/// what knows which Arrow it is holding.
///
/// It clones, because a description is pointers and widths and copying
/// one copies no data. A client that registers the same columns on two
/// connections describes them once.
#[derive(Clone)]
pub enum Layout {
    /// Integers of one width, signed or not, and what one of them has
    /// to be multiplied by to reach the unit its logical type counts
    /// in. That is 1 for an integer and a date, and 1000 for the
    /// microseconds Arrow keeps a time or a timestamp in against the
    /// nanoseconds this engine keeps them in.
    Int {
        ptr: NonNull<u8>,
        bits: IntBits,
        signed: bool,
        scale: i64,
    },
    /// IEEE floats of one width.
    Float { ptr: NonNull<u8>, bits: FloatBits },
    /// One bit a row, packed low bit of the first byte first, which is
    /// Arrow's bitmap and this engine's alike. The first row is the
    /// pointer's low bit: a caller holding a sliced array with a bit
    /// offset of its own owes the shift before it gets here.
    Bool { ptr: NonNull<u8> },
    /// Characters end to end with offsets cutting them up, Arrow's
    /// `Utf8` when the offsets are 32 bits and `LargeUtf8` when they
    /// are 64. There are `rows + 1` of them.
    Str {
        offsets: NonNull<u8>,
        wide: bool,
        data: NonNull<u8>,
        data_len: usize,
    },
    /// Arrow's `Utf8View`: sixteen bytes a row over any number of data
    /// buffers. A short string is already the engine's own view, byte
    /// for byte; a long one names its buffer and offset in a different
    /// order and is rebuilt.
    View {
        views: NonNull<u8>,
        data: Vec<(NonNull<u8>, usize)>,
    },
}

/// One column of a frame as the caller describes it.
#[derive(Clone)]
pub struct Column {
    pub name: String,
    /// What a value read out of it is. The layout says how wide the
    /// bytes are and this says what they mean, which is the whole
    /// difference between a count of days and a number.
    pub ty: LogicalType,
    pub layout: Layout,
}

/// A column as the frame keeps it: the caller's layout with the string
/// buffer ids resolved, so a read is a pointer walk and nothing else.
#[derive(Clone)]
struct Col {
    name: String,
    ty: LogicalType,
    kind: Kind,
}

#[derive(Clone)]
enum Kind {
    Int {
        ptr: NonNull<u8>,
        bits: IntBits,
        signed: bool,
        scale: i64,
    },
    Float {
        ptr: NonNull<u8>,
        bits: FloatBits,
    },
    Bool {
        ptr: NonNull<u8>,
    },
    Str {
        offsets: NonNull<u8>,
        wide: bool,
        data: NonNull<u8>,
        buf: u16,
    },
    View {
        views: NonNull<u8>,
        /// The id the frame's first data buffer went in under. Arrow
        /// numbers a view's buffer from zero within the array and the
        /// engine numbers buffers within the frame, so the two differ
        /// by this and by nothing else.
        base: u16,
    },
}

/// A registered frame: rows of columns the engine reads and never owns.
///
/// Cloning one costs the column names and two handle bumps. It does not
/// copy a value, because there is no value here to copy: both frames
/// point at the caller's bytes and both hold the owner that keeps them
/// alive. That is what lets a label be re-derived when the epoch moves
/// without a statement mid-flight losing the frame it was reading.
#[derive(Clone)]
pub struct Frame {
    name: String,
    id: TableId,
    /// The label of this frame's name in the schema it was merged into.
    label: u16,
    rows: u64,
    cols: Vec<Col>,
    /// The data buffers every string column of this frame resolves a
    /// long view through, built once here and shared by every vector a
    /// scan hands out.
    strs: Arc<StrBuffers>,
    /// What keeps the bytes alive. Held and never read: dropping it is
    /// what releases them, and that happens when the last frame and the
    /// last vector built over it are gone.
    #[allow(dead_code)]
    owner: Arc<dyn Any + Send + Sync>,
}

// The bytes are immutable for as long as the owner lives, the owner is
// itself `Send + Sync`, and nothing here writes. A frame is shared by
// every worker of a parallel scan for exactly that reason.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

/// What a frame is, without what it holds: the values are the caller's
/// and printing them is neither this type's business nor bounded.
impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("name", &self.name)
            .field("id", &self.id)
            .field("rows", &self.rows)
            .field(
                "columns",
                &self.cols.iter().map(|c| &c.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Frame {
    /// Registers `columns` under `name` as `rows` rows.
    ///
    /// Every check a read could have made is made here instead: that a
    /// word column is aligned for the words it will be read as, that
    /// an unsigned value fits the signed lane, that a scaled one does
    /// not overflow, and that a string offset stays inside its buffer.
    /// A frame that comes back from this reads without failing.
    ///
    /// # Safety
    ///
    /// Every pointer in `columns` must address at least the bytes the
    /// row count implies, initialized, and neither written nor freed by
    /// anyone for as long as `owner` lives. `owner` must be what keeps
    /// them alive, so that holding it here holds them.
    pub unsafe fn new(
        name: impl Into<String>,
        rows: u64,
        columns: Vec<Column>,
        owner: Arc<dyn Any + Send + Sync>,
    ) -> Result<Frame> {
        let name = name.into();
        if columns.is_empty() {
            return Err(refuse(format!("frame '{name}' has no columns")));
        }
        for (i, col) in columns.iter().enumerate() {
            if columns[..i].iter().any(|c| c.name == col.name) {
                return Err(refuse(format!(
                    "frame '{name}' names its column '{}' twice",
                    col.name
                )));
            }
        }
        let mut strs = StrBuffers::new();
        let mut cols = Vec::with_capacity(columns.len());
        for column in columns {
            let Column {
                name: col,
                ty,
                layout,
            } = column;
            let kind = match layout {
                Layout::Int {
                    ptr,
                    bits,
                    signed,
                    scale,
                } => {
                    let width = int_width(bits).ok_or_else(|| {
                        refuse(format!(
                            "column '{col}' of frame '{name}' is {} bits wide, and this engine reads 8, 16, 32 and 64",
                            bits.bits()
                        ))
                    })?;
                    aligned(&name, &col, ptr, width)?;
                    let kind = Kind::Int {
                        ptr,
                        bits,
                        signed,
                        scale,
                    };
                    // Only the columns that can fail are walked. A
                    // signed 64-bit count of itself is every value the
                    // lane holds, and there is nothing to find.
                    if scale != 1 || (!signed && width == 8) || matches!(ty, LogicalType::Date) {
                        check_ints(&name, &col, &ty, &kind, rows)?;
                    }
                    kind
                }
                Layout::Float { ptr, bits } => {
                    let width = float_width(bits).ok_or_else(|| {
                        refuse(format!(
                            "column '{col}' of frame '{name}' is a {} bit float, and this engine reads 32 and 64",
                            bits.bits()
                        ))
                    })?;
                    aligned(&name, &col, ptr, width)?;
                    Kind::Float { ptr, bits }
                }
                Layout::Bool { ptr } => Kind::Bool { ptr },
                Layout::Str {
                    offsets,
                    wide,
                    data,
                    data_len,
                } => {
                    aligned(&name, &col, offsets, if wide { 8 } else { 4 })?;
                    if data_len > u32::MAX as usize {
                        return Err(refuse(format!(
                            "column '{col}' of frame '{name}' holds {data_len} bytes of characters, and a string view reaches {}",
                            u32::MAX
                        )));
                    }
                    check_offsets(&name, &col, offsets, wide, data_len, rows)?;
                    let buf =
                        unsafe { strs.push_lent(data.as_ptr(), data_len, Arc::clone(&owner)) };
                    Kind::Str {
                        offsets,
                        wide,
                        data,
                        buf,
                    }
                }
                Layout::View { views, data } => {
                    aligned(&name, &col, views, 8)?;
                    let base = u16::try_from(strs.len())
                        .map_err(|_| refuse(format!("frame '{name}' holds too many buffers")))?;
                    for (ptr, len) in &data {
                        if *len > u32::MAX as usize {
                            return Err(refuse(format!(
                                "column '{col}' of frame '{name}' holds a {len} byte buffer, and a string view reaches {}",
                                u32::MAX
                            )));
                        }
                        unsafe { strs.push_lent(ptr.as_ptr(), *len, Arc::clone(&owner)) };
                    }
                    check_views(&name, &col, views, &data, rows)?;
                    Kind::View { views, base }
                }
            };
            cols.push(Col {
                name: col,
                ty,
                kind,
            });
        }
        Ok(Frame {
            name,
            // The set assigns the real one; a frame on its own is not
            // in any id space yet.
            id: TOP_TABLE_ID,
            label: 0,
            rows,
            cols,
            strs: Arc::new(strs),
            owner,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn id(&self) -> TableId {
        self.id
    }

    fn set_label(&mut self, label: u16) {
        self.label = label;
    }

    /// The label bitset every row of a frame carries, the answer a
    /// stored table gives out of its catalog entry.
    pub fn labels(&self) -> u64 {
        1 << self.label
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// The column positions and names, in the order they were
    /// registered, which is the order the caller's frame held them in.
    pub fn columns(&self) -> impl Iterator<Item = (ColId, &str)> {
        self.cols
            .iter()
            .enumerate()
            .map(|(i, c)| (i as ColId, c.name.as_str()))
    }

    /// The position of a column by name.
    pub fn column(&self, name: &str) -> Option<ColId> {
        self.cols
            .iter()
            .position(|c| c.name == name)
            .map(|i| i as ColId)
    }

    /// What the vector layer would carry this column as, `None` for a
    /// column it has no lane for. The lanes a frame hands over are
    /// integers, doubles and strings; a boolean or a temporal column
    /// resolves to nothing here and the statement reads it a row at a
    /// time. A stored temporal column does have a lane, because it is
    /// written as the count it is and read back as one; a frame holds
    /// whatever a caller lent it, and there is no temporal layout to
    /// lend yet.
    pub fn lane(&self, col: ColId) -> Option<ColType> {
        match &self.cols.get(col as usize)?.ty {
            LogicalType::Int { .. } => Some(ColType::Int),
            LogicalType::Float { .. } => Some(ColType::Float),
            LogicalType::Str { .. } => Some(ColType::Str),
            _ => None,
        }
    }

    /// Resolves a column by name as the vector layer would carry it.
    pub fn resolve(&self, name: &str) -> Option<(ColId, ColType)> {
        let col = self.column(name)?;
        Some((col, self.lane(col)?))
    }

    /// Reads chunk `chunk`, `None` past the last one.
    ///
    /// The vectors of a chunk point at the caller's buffers wherever
    /// the layouts agree, which is what makes this a scan of somebody
    /// else's memory rather than a copy of it. Where they do not, the
    /// widening lands in `arena` and dies with the morsel.
    ///
    /// A frame carries no zone maps, so `pred` cannot skip a chunk
    /// without looking at it. What it can still do is thin one: the
    /// bound is checked a row at a time against the same eight-byte
    /// word a stored column would have decoded to, and a chunk it
    /// empties never reaches an operator at all.
    pub fn scan(
        &self,
        chunk: u64,
        cols: &[ColId],
        pred: Option<&ZonePred>,
        arena: &mut MorselArena,
    ) -> Option<ScanChunk> {
        let row_base = chunk * SCAN_ROWS as u64;
        if row_base >= self.rows {
            return None;
        }
        let rows = (self.rows - row_base).min(SCAN_ROWS as u64) as usize;
        let mut sel = None;
        // A bound on anything but a word column is left to the residual
        // program, which holds the same predicate and is where a frame
        // column with no lane is read from anyway.
        if let Some(p) = pred
            && let Some(col) = self.cols.get(p.col as usize)
            && matches!(col.kind, Kind::Int { .. } | Kind::Bool { .. })
        {
            let pass = |i: usize| {
                let w = self.word(col, row_base + i as u64) as u64;
                w >= p.lo && w <= p.hi
            };
            // Counted first and built only when it is worth carrying,
            // which is the reasoning the stored scan spells out: the
            // predicate is in the residual program either way, so a
            // selection buys the rows the operators above never see and
            // costs a push per row that survives.
            let kept = (0..rows).filter(|&i| pass(i)).count();
            if kept == 0 {
                return None;
            }
            if kept * 2 <= rows {
                let mut s = SelVector::with_capacity(arena, rows);
                for i in (0..rows).filter(|&i| pass(i)) {
                    s.push(i as u16);
                }
                sel = Some(s);
            }
        }
        let columns = cols
            .iter()
            .map(|&c| self.vector(c, row_base, rows, arena))
            .collect();
        Some(ScanChunk {
            row_base,
            rows: rows as u32,
            sel,
            columns,
        })
    }

    /// One column of one run of rows as a vector.
    fn vector(&self, col: ColId, base: u64, rows: usize, arena: &mut MorselArena) -> ValueVector {
        let col = &self.cols[col as usize];
        match &col.kind {
            // The two layouts that are already the lane: the vector is
            // the caller's bytes, offset to the first row of the chunk.
            Kind::Int {
                ptr,
                bits: IntBits::B64,
                signed: true,
                scale: 1,
            } => borrowed(PhysType::Int64, *ptr, base as usize * 8, rows * 8, rows),
            Kind::Float {
                ptr,
                bits: FloatBits::B64,
            } => borrowed(PhysType::Float64, *ptr, base as usize * 8, rows * 8, rows),
            Kind::Int { .. } | Kind::Bool { .. } => {
                let mut vec = ValueVector::flat_uninit(arena, PhysType::Int64, rows);
                let out = vec.values_mut::<i64>();
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = self.word(col, base + i as u64);
                }
                vec
            }
            Kind::Float { ptr, bits } => {
                debug_assert_eq!(*bits, FloatBits::B32);
                let mut vec = ValueVector::flat_uninit(arena, PhysType::Float64, rows);
                let out = vec.values_mut::<f64>();
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = f64::from(unsafe { read::<f32>(*ptr, base as usize + i) });
                }
                vec
            }
            Kind::Str { .. } | Kind::View { .. } => {
                let mut vec = ValueVector::flat_uninit(arena, PhysType::Str, rows);
                {
                    let out = vec.values_mut::<StrView>();
                    for (i, slot) in out.iter_mut().enumerate() {
                        *slot = self.view(col, base + i as u64);
                    }
                }
                vec.aux = Aux::Str(Arc::clone(&self.strs));
                vec
            }
        }
    }

    /// One column of arbitrary rows as a vector, in the caller's row
    /// order. Rows that are not a run cannot be pointed at, so this one
    /// always builds.
    pub fn gather(&self, col: ColId, rows: &[u64], arena: &mut MorselArena) -> ValueVector {
        let col = &self.cols[col as usize];
        match &col.kind {
            Kind::Str { .. } | Kind::View { .. } => {
                let mut vec = ValueVector::flat_uninit(arena, PhysType::Str, rows.len());
                {
                    let out = vec.values_mut::<StrView>();
                    for (slot, &row) in out.iter_mut().zip(rows) {
                        *slot = self.view(col, row);
                    }
                }
                vec.aux = Aux::Str(Arc::clone(&self.strs));
                vec
            }
            Kind::Float { ptr, bits } => {
                let mut vec = ValueVector::flat_uninit(arena, PhysType::Float64, rows.len());
                let out = vec.values_mut::<f64>();
                for (slot, &row) in out.iter_mut().zip(rows) {
                    *slot = match bits {
                        FloatBits::B32 => f64::from(unsafe { read::<f32>(*ptr, row as usize) }),
                        _ => unsafe { read::<f64>(*ptr, row as usize) },
                    };
                }
                vec
            }
            Kind::Int { .. } | Kind::Bool { .. } => {
                let mut vec = ValueVector::flat_uninit(arena, PhysType::Int64, rows.len());
                let out = vec.values_mut::<i64>();
                for (slot, &row) in out.iter_mut().zip(rows) {
                    *slot = self.word(col, row);
                }
                vec
            }
        }
    }

    /// One value of one row, the read the row-at-a-time executor makes.
    /// The row is the caller's and has already been bounds checked by
    /// the scan that produced it.
    pub fn value(&self, col: ColId, row: u64) -> Value {
        let col = &self.cols[col as usize];
        match &col.kind {
            Kind::Str { .. } | Kind::View { .. } => {
                let view = self.view(col, row);
                Value::Str(String::from_utf8_lossy(view.bytes(&self.strs)).into_owned())
            }
            Kind::Float { ptr, bits } => Value::Float(match bits {
                FloatBits::B32 => f64::from(unsafe { read::<f32>(*ptr, row as usize) }),
                _ => unsafe { read::<f64>(*ptr, row as usize) },
            }),
            Kind::Bool { ptr } => Value::Bool(unsafe { bit(*ptr, row) }),
            Kind::Int { .. } => {
                let word = self.word(col, row);
                match &col.ty {
                    LogicalType::Bool => Value::Bool(word != 0),
                    LogicalType::Date => Value::Temporal(Temporal::Date(word as i32)),
                    LogicalType::LocalTime => Value::Temporal(Temporal::LocalTime(word)),
                    LogicalType::LocalDatetime => Value::Temporal(Temporal::LocalDatetime(word)),
                    LogicalType::Duration(kind) => Value::Temporal(Temporal::Duration(*kind, word)),
                    _ => Value::Int(word),
                }
            }
        }
    }

    /// The value of one row of a fixed width column as the eight-byte
    /// lane holds it: widened to the lane's width and scaled to the
    /// unit its type counts in, both of which were checked to fit at
    /// registration.
    fn word(&self, col: &Col, row: u64) -> i64 {
        match &col.kind {
            Kind::Bool { ptr } => i64::from(unsafe { bit(*ptr, row) }),
            Kind::Int {
                ptr,
                bits,
                signed,
                scale,
            } => {
                let row = row as usize;
                let raw = unsafe {
                    match (bits, signed) {
                        (IntBits::B8, true) => i64::from(read::<i8>(*ptr, row)),
                        (IntBits::B8, false) => i64::from(read::<u8>(*ptr, row)),
                        (IntBits::B16, true) => i64::from(read::<i16>(*ptr, row)),
                        (IntBits::B16, false) => i64::from(read::<u16>(*ptr, row)),
                        (IntBits::B32, true) => i64::from(read::<i32>(*ptr, row)),
                        (IntBits::B32, false) => i64::from(read::<u32>(*ptr, row)),
                        _ => read::<i64>(*ptr, row),
                    }
                };
                raw.wrapping_mul(*scale)
            }
            _ => unreachable!("a fixed width column"),
        }
    }

    /// The engine's own sixteen-byte view of one string row.
    fn view(&self, col: &Col, row: u64) -> StrView {
        match &col.kind {
            Kind::Str {
                offsets,
                wide,
                data,
                buf,
            } => {
                let (start, end) = unsafe { span(*offsets, *wide, row) };
                let bytes =
                    unsafe { std::slice::from_raw_parts(data.as_ptr().add(start), end - start) };
                if bytes.len() > zu_vector::INLINE_LEN {
                    StrView::long(bytes, *buf, start as u32)
                } else {
                    StrView::inline(bytes)
                }
            }
            Kind::View { views, base } => {
                let raw = unsafe { read::<[u8; 16]>(*views, row as usize) };
                let len = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
                if len <= zu_vector::INLINE_LEN {
                    // Arrow's short view is this engine's short view,
                    // byte for byte: length, then the characters, then
                    // zeroes. Nothing to rebuild.
                    return StrView::inline(&raw[4..4 + len]);
                }
                let which = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
                let offset = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
                let id = base + which as u16;
                let bytes = self.strs.slice(id, offset, len);
                StrView::long(bytes, id, offset)
            }
            _ => unreachable!("a string column"),
        }
    }
}

/// The frames registered on one session, by the ids the binder and the
/// executor know them under.
///
/// Registering builds a new set rather than changing this one, so a
/// statement that is running holds the set it started with and a frame
/// unregistered under it stays alive and readable until it ends.
#[derive(Default, Clone)]
pub struct FrameSet {
    frames: Vec<Arc<Frame>>,
}

impl FrameSet {
    pub fn new() -> FrameSet {
        FrameSet::default()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// The frame a table id names, `None` for every id of the catalog.
    pub fn get(&self, table: TableId) -> Option<&Arc<Frame>> {
        // Frames are few and the ids are dense at the top, so this is a
        // walk of a handful of u32s that the branch above it usually
        // skips outright.
        self.frames.iter().find(|f| f.id == table)
    }

    pub fn by_name(&self, name: &str) -> Option<&Arc<Frame>> {
        self.frames.iter().find(|f| f.name == name)
    }

    /// The registered names, sorted, which is what a caller listing
    /// them expects and not the order they arrived in.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.frames.iter().map(|f| f.name.clone()).collect();
        names.sort();
        names
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<Frame>> {
        self.frames.iter()
    }

    /// This set with `frame` registered, replacing one of the same name.
    ///
    /// A replaced frame keeps its id, so a statement compiled against
    /// the name binds to the same table it did before. `taken` is what
    /// the database itself uses, which a frame id may not collide with.
    pub fn with(&self, mut frame: Frame, taken: &dyn Fn(TableId) -> bool) -> Result<FrameSet> {
        let mut frames = self.frames.clone();
        match frames.iter().position(|f| f.name == frame.name) {
            Some(at) => {
                frame.id = frames[at].id;
                frames[at] = Arc::new(frame);
            }
            None => {
                let free = (0..=TOP_TABLE_ID)
                    .rev()
                    .find(|&id| !taken(id) && !frames.iter().any(|f| f.id == id))
                    .ok_or_else(|| {
                        refuse("the table id space has no room left for a registered frame")
                    })?;
                frame.id = free;
                frames.push(Arc::new(frame));
            }
        }
        Ok(FrameSet { frames })
    }

    /// Says which label each frame's name took in the schema, in the
    /// order [`FrameSet::iter`] hands them out, which is the order they
    /// were merged into it.
    ///
    /// The label space belongs to the schema and the schema is rebuilt
    /// whenever the epoch moves, so a frame is told its label again
    /// rather than remembering one that was true of an older catalog. A
    /// frame a running statement is still reading is copied instead of
    /// written to, and both copies point at the same bytes.
    pub fn set_labels(&mut self, labels: &[u16]) {
        for (frame, &label) in self.frames.iter_mut().zip(labels) {
            Arc::make_mut(frame).set_label(label);
        }
    }

    /// This set without the frame of that name.
    pub fn without(&self, name: &str) -> Option<FrameSet> {
        let at = self.frames.iter().position(|f| f.name == name)?;
        let mut frames = self.frames.clone();
        frames.remove(at);
        Some(FrameSet { frames })
    }
}

/// A vector over bytes the engine does not own: the caller's column,
/// read where it lies.
fn borrowed(
    phys: PhysType,
    ptr: NonNull<u8>,
    offset: usize,
    len: usize,
    rows: usize,
) -> ValueVector {
    // Safe by the frame's own contract: the bytes are alive while the
    // owner is, the owner is held by the frame, and the run was bounds
    // checked against the row count before this was called.
    let data = unsafe { RawBuf::borrowed(NonNull::new_unchecked(ptr.as_ptr().add(offset)), len) };
    ValueVector {
        phys,
        encoding: VecEncoding::Flat,
        data,
        validity: None,
        aux: Aux::None,
        len: rows as u32,
    }
}

/// One element of a column, read where it lies.
///
/// # Safety
///
/// `row` must be inside the column and `ptr` aligned for `T`, both of
/// which registration checked.
#[inline]
unsafe fn read<T: Copy>(ptr: NonNull<u8>, row: usize) -> T {
    unsafe { ptr.as_ptr().cast::<T>().add(row).read() }
}

/// One bit of a bitmap, low bit of the first byte first.
///
/// # Safety
///
/// `row` must be inside the column.
#[inline]
unsafe fn bit(ptr: NonNull<u8>, row: u64) -> bool {
    let byte = unsafe { ptr.as_ptr().add((row / 8) as usize).read() };
    byte >> (row % 8) & 1 == 1
}

/// The byte range one string row occupies, from a pair of offsets.
///
/// # Safety
///
/// `row` must be inside the column.
#[inline]
unsafe fn span(offsets: NonNull<u8>, wide: bool, row: u64) -> (usize, usize) {
    unsafe {
        if wide {
            (
                read::<i64>(offsets, row as usize) as usize,
                read::<i64>(offsets, row as usize + 1) as usize,
            )
        } else {
            (
                read::<i32>(offsets, row as usize) as usize,
                read::<i32>(offsets, row as usize + 1) as usize,
            )
        }
    }
}

fn int_width(bits: IntBits) -> Option<usize> {
    match bits {
        IntBits::B8 => Some(1),
        IntBits::B16 => Some(2),
        IntBits::B32 => Some(4),
        IntBits::B64 => Some(8),
        _ => None,
    }
}

fn float_width(bits: FloatBits) -> Option<usize> {
    match bits {
        FloatBits::B32 => Some(4),
        FloatBits::B64 => Some(8),
        _ => None,
    }
}

/// Refuses a column whose bytes do not start where a read of that width
/// can start. Arrow allocates its buffers aligned and this is only ever
/// hit by a caller that sliced one by hand.
fn aligned(frame: &str, col: &str, ptr: NonNull<u8>, width: usize) -> Result<()> {
    if (ptr.as_ptr() as usize).is_multiple_of(width) {
        return Ok(());
    }
    Err(refuse(format!(
        "column '{col}' of frame '{frame}' starts at an address {width} byte values cannot be read from"
    )))
}

/// Walks an integer column once, for the columns where a value can fail
/// to be one: an unsigned word too large for the signed lane, a scale
/// that overflows, a day count outside the calendar.
fn check_ints(frame: &str, col: &str, ty: &LogicalType, kind: &Kind, rows: u64) -> Result<()> {
    let Kind::Int {
        ptr,
        bits,
        signed,
        scale,
    } = kind
    else {
        return Ok(());
    };
    for row in 0..rows {
        let row_ix = row as usize;
        let raw = unsafe {
            match (bits, signed) {
                (IntBits::B8, true) => i64::from(read::<i8>(*ptr, row_ix)),
                (IntBits::B8, false) => i64::from(read::<u8>(*ptr, row_ix)),
                (IntBits::B16, true) => i64::from(read::<i16>(*ptr, row_ix)),
                (IntBits::B16, false) => i64::from(read::<u16>(*ptr, row_ix)),
                (IntBits::B32, true) => i64::from(read::<i32>(*ptr, row_ix)),
                (IntBits::B32, false) => i64::from(read::<u32>(*ptr, row_ix)),
                (_, true) => read::<i64>(*ptr, row_ix),
                (_, false) => {
                    let raw = read::<u64>(*ptr, row_ix);
                    i64::try_from(raw).map_err(|_| {
                        refuse(format!(
                            "column '{col}' of frame '{frame}' holds {raw} at row {row}, which is past what a value of this engine counts to"
                        ))
                    })?
                }
            }
        };
        let scaled = raw.checked_mul(*scale).ok_or_else(|| {
            refuse(format!(
                "column '{col}' of frame '{frame}' holds {raw} at row {row}, which does not fit once it is counted in the unit this engine keeps"
            ))
        })?;
        if matches!(ty, LogicalType::Date) && i32::try_from(scaled).is_err() {
            return Err(refuse(format!(
                "column '{col}' of frame '{frame}' holds day {scaled} at row {row}, which is outside the calendar"
            )));
        }
    }
    Ok(())
}

/// Checks that every string of a column lands inside the buffer it cuts
/// from, monotone offsets included, so a read of one is a slice and not
/// a question.
fn check_offsets(
    frame: &str,
    col: &str,
    offsets: NonNull<u8>,
    wide: bool,
    data_len: usize,
    rows: u64,
) -> Result<()> {
    let mut last = 0usize;
    for row in 0..rows {
        let (start, end) = unsafe { span(offsets, wide, row) };
        if start != last || end < start || end > data_len {
            return Err(refuse(format!(
                "column '{col}' of frame '{frame}' cuts {start}..{end} out of {data_len} bytes at row {row}, which is not a string"
            )));
        }
        last = end;
    }
    Ok(())
}

/// The same for the view layout, where a long view names its own buffer
/// and a short one carries its characters.
fn check_views(
    frame: &str,
    col: &str,
    views: NonNull<u8>,
    data: &[(NonNull<u8>, usize)],
    rows: u64,
) -> Result<()> {
    for row in 0..rows {
        let raw = unsafe { read::<[u8; 16]>(views, row as usize) };
        let len = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        if len <= zu_vector::INLINE_LEN {
            continue;
        }
        let which = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as usize;
        let offset = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]) as usize;
        let held = data.get(which).map(|&(_, len)| len).ok_or_else(|| {
            refuse(format!(
                "column '{col}' of frame '{frame}' points row {row} at buffer {which}, which the frame does not hold"
            ))
        })?;
        if offset + len > held {
            return Err(refuse(format!(
                "column '{col}' of frame '{frame}' cuts {offset}..{} out of {held} bytes at row {row}, which is not a string",
                offset + len
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner a test hands over: the vectors the pointers point into,
    /// kept alive by the frame exactly as a caller's arrays would be.
    struct Held {
        #[allow(dead_code)]
        parts: Vec<Box<dyn Any + Send + Sync>>,
    }

    fn own(parts: Vec<Box<dyn Any + Send + Sync>>) -> Arc<dyn Any + Send + Sync> {
        Arc::new(Held { parts })
    }

    fn ptr_of<T>(v: &[T]) -> NonNull<u8> {
        NonNull::new(v.as_ptr() as *mut u8).expect("a real pointer")
    }

    fn int_col(name: &str, values: &[i64]) -> Column {
        Column {
            name: name.into(),
            ty: LogicalType::Int {
                signed: true,
                bits: IntBits::B64,
                precision: None,
            },
            layout: Layout::Int {
                ptr: ptr_of(values),
                bits: IntBits::B64,
                signed: true,
                scale: 1,
            },
        }
    }

    #[test]
    fn an_eight_byte_column_is_scanned_where_it_lies() {
        let values: Vec<i64> = (0..10).collect();
        let frame = unsafe {
            Frame::new("t", 10, vec![int_col("n", &values)], own(vec![])).expect("a frame")
        };
        let mut arena = MorselArena::new();
        let chunk = frame.scan(0, &[0], None, &mut arena).expect("a chunk");
        assert_eq!(chunk.rows, 10);
        let vector = &chunk.columns[0];
        assert_eq!(vector.values::<i64>(), &values[..]);
        // The whole point: the vector is the caller's buffer and not a
        // copy of it.
        assert_eq!(
            vector.values::<i64>().as_ptr() as usize,
            values.as_ptr() as usize
        );
    }

    #[test]
    fn a_narrow_column_widens_into_the_lane() {
        let values: Vec<i32> = vec![1, -2, 3];
        let col = Column {
            name: "n".into(),
            ty: LogicalType::Int {
                signed: true,
                bits: IntBits::B32,
                precision: None,
            },
            layout: Layout::Int {
                ptr: ptr_of(&values),
                bits: IntBits::B32,
                signed: true,
                scale: 1,
            },
        };
        let frame = unsafe { Frame::new("t", 3, vec![col], own(vec![])).expect("a frame") };
        let mut arena = MorselArena::new();
        let chunk = frame.scan(0, &[0], None, &mut arena).expect("a chunk");
        assert_eq!(chunk.columns[0].values::<i64>(), &[1, -2, 3]);
    }

    #[test]
    fn characters_are_read_where_they_lie_and_the_views_are_built() {
        let data = b"adalonger than a view holds".to_vec();
        let offsets: Vec<i32> = vec![0, 3, 27];
        let col = Column {
            name: "name".into(),
            ty: LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            },
            layout: Layout::Str {
                offsets: ptr_of(&offsets),
                wide: false,
                data: ptr_of(&data),
                data_len: data.len(),
            },
        };
        let frame = unsafe { Frame::new("t", 2, vec![col], own(vec![])).expect("a frame") };
        let mut arena = MorselArena::new();
        let chunk = frame.scan(0, &[0], None, &mut arena).expect("a chunk");
        let views = chunk.columns[0].values::<StrView>();
        let bufs = chunk.columns[0].str_buffers().expect("the frame's buffers");
        assert_eq!(views[0].bytes(bufs), b"ada");
        assert_eq!(views[1].bytes(bufs), b"longer than a view holds");
        // The long one resolves into the caller's own bytes.
        assert_eq!(
            views[1].bytes(bufs).as_ptr() as usize,
            data.as_ptr() as usize + 3
        );
    }

    #[test]
    fn a_view_column_keeps_the_short_ones_and_rebuilds_the_long_ones() {
        // Arrow's view layout, which polars hands over: sixteen bytes a
        // row, the short one already this engine's own and the long one
        // naming a buffer and an offset in the other order.
        let data = b"longer than a view holds".to_vec();
        let mut views = vec![[0u8; 16]; 2];
        views[0][..4].copy_from_slice(&3u32.to_le_bytes());
        views[0][4..7].copy_from_slice(b"ada");
        views[1][..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        views[1][4..8].copy_from_slice(&data[..4]);
        views[1][8..12].copy_from_slice(&0u32.to_le_bytes());
        views[1][12..16].copy_from_slice(&0u32.to_le_bytes());
        let col = Column {
            name: "name".into(),
            ty: LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            },
            layout: Layout::View {
                views: ptr_of(&views),
                data: vec![(ptr_of(&data), data.len())],
            },
        };
        let frame = unsafe { Frame::new("t", 2, vec![col], own(vec![])).expect("a frame") };
        let mut arena = MorselArena::new();
        let chunk = frame.scan(0, &[0], None, &mut arena).expect("a chunk");
        let built = chunk.columns[0].values::<StrView>();
        let bufs = chunk.columns[0].str_buffers().expect("the frame's buffers");
        assert_eq!(built[0].bytes(bufs), b"ada");
        assert_eq!(built[1].bytes(bufs), &data[..]);
        assert_eq!(
            built[1].bytes(bufs).as_ptr() as usize,
            data.as_ptr() as usize
        );
    }

    #[test]
    fn microseconds_become_the_nanoseconds_a_value_counts() {
        let values: Vec<i64> = vec![1_700_000_000_000_000];
        let col = Column {
            name: "when".into(),
            ty: LogicalType::LocalDatetime,
            layout: Layout::Int {
                ptr: ptr_of(&values),
                bits: IntBits::B64,
                signed: true,
                scale: 1_000,
            },
        };
        let frame = unsafe { Frame::new("t", 1, vec![col], own(vec![])).expect("a frame") };
        assert_eq!(
            frame.value(0, 0),
            Value::Temporal(Temporal::LocalDatetime(1_700_000_000_000_000_000))
        );
    }

    #[test]
    fn a_bit_a_row_reads_as_a_boolean() {
        let bits: Vec<u8> = vec![0b0000_0101];
        let col = Column {
            name: "yes".into(),
            ty: LogicalType::Bool,
            layout: Layout::Bool { ptr: ptr_of(&bits) },
        };
        let frame = unsafe { Frame::new("t", 3, vec![col], own(vec![])).expect("a frame") };
        assert_eq!(frame.value(0, 0), Value::Bool(true));
        assert_eq!(frame.value(0, 1), Value::Bool(false));
        assert_eq!(frame.value(0, 2), Value::Bool(true));
    }

    #[test]
    fn a_value_too_large_for_the_lane_is_refused_by_row() {
        let values: Vec<u64> = vec![1, u64::MAX];
        let col = Column {
            name: "big".into(),
            ty: LogicalType::Int {
                signed: false,
                bits: IntBits::B64,
                precision: None,
            },
            layout: Layout::Int {
                ptr: ptr_of(&values),
                bits: IntBits::B64,
                signed: false,
                scale: 1,
            },
        };
        let err = unsafe { Frame::new("t", 2, vec![col], own(vec![])) }.expect_err("refused");
        assert!(err.to_string().contains("at row 1"), "{err}");
    }

    #[test]
    fn a_frame_scans_a_chunk_at_a_time() {
        let values: Vec<i64> = (0..SCAN_ROWS as i64 + 5).collect();
        let frame = unsafe {
            Frame::new(
                "t",
                values.len() as u64,
                vec![int_col("n", &values)],
                own(vec![]),
            )
            .expect("a frame")
        };
        let mut arena = MorselArena::new();
        assert_eq!(
            frame.scan(0, &[0], None, &mut arena).expect("a chunk").rows as usize,
            SCAN_ROWS
        );
        assert_eq!(
            frame.scan(1, &[0], None, &mut arena).expect("a chunk").rows,
            5
        );
        assert!(frame.scan(2, &[0], None, &mut arena).is_none());
    }

    #[test]
    fn a_registered_name_keeps_its_id_and_an_unregistered_one_goes() {
        let values: Vec<i64> = vec![1];
        let set = FrameSet::new();
        let one = unsafe {
            Frame::new("people", 1, vec![int_col("n", &values)], own(vec![])).expect("a frame")
        };
        let set = set.with(one, &|_| false).expect("registered");
        let id = set.by_name("people").expect("registered").id();
        let again = unsafe {
            Frame::new("people", 1, vec![int_col("n", &values)], own(vec![])).expect("a frame")
        };
        let set = set.with(again, &|_| false).expect("registered");
        assert_eq!(set.by_name("people").expect("registered").id(), id);
        assert_eq!(set.len(), 1);
        let set = set.without("people").expect("registered");
        assert!(set.is_empty());
        assert!(set.without("people").is_none());
    }

    #[test]
    fn a_bound_thins_a_chunk_and_empties_one() {
        let values: Vec<i64> = (0..100).collect();
        let frame = unsafe {
            Frame::new("t", 100, vec![int_col("n", &values)], own(vec![])).expect("a frame")
        };
        let mut arena = MorselArena::new();
        // A quarter of the rows is worth a selection.
        let pred = ZonePred {
            col: 0,
            lo: 10,
            hi: 34,
        };
        let chunk = frame
            .scan(0, &[0], Some(&pred), &mut arena)
            .expect("a chunk");
        assert_eq!(chunk.rows, 100);
        let sel = chunk.sel.as_ref().expect("a selection");
        assert_eq!(sel.len(), 25);
        assert_eq!(sel.as_slice()[0], 10);
        // Most of them is not: the chunk comes whole and the residual
        // program does the rest.
        let wide = ZonePred {
            col: 0,
            lo: 0,
            hi: 89,
        };
        let chunk = frame
            .scan(0, &[0], Some(&wide), &mut arena)
            .expect("a chunk");
        assert!(chunk.sel.is_none());
        // None of them is no chunk at all.
        let none = ZonePred {
            col: 0,
            lo: 200,
            hi: 300,
        };
        assert!(frame.scan(0, &[0], Some(&none), &mut arena).is_none());
    }

    #[test]
    fn a_frame_takes_an_id_the_database_is_not_using() {
        let values: Vec<i64> = vec![1];
        let frame = unsafe {
            Frame::new("people", 1, vec![int_col("n", &values)], own(vec![])).expect("a frame")
        };
        let set = FrameSet::new()
            .with(frame, &|id| id > TOP_TABLE_ID - 3)
            .expect("registered");
        assert_eq!(
            set.by_name("people").expect("registered").id(),
            TOP_TABLE_ID - 3
        );
    }
}
