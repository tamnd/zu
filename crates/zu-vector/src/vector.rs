//! Typed value vectors (perf/02 section 1).

use std::sync::Arc;

use zu_common::GROUP_ROWS;

use crate::arena::{MorselArena, Pod, RawBuf};
use crate::bitmap::Bitmap;
use crate::str::{Dictionary, StrBuffers, StrView};

/// Physical types. A cell is one machine word or one 16-byte view, never
/// a tagged enum: the tag lives once per vector, not once per value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PhysType {
    /// One bit per row, packed in u64 words like a bitmap.
    Bool,
    Int64,
    Float64,
    /// Packed node reference: table in the top 16 bits, table row in the
    /// low 48. One word where the old Value::Node was 32 bytes.
    NodeRef,
    /// Rel row id; (table, src, dst) resolve through context, not the cell.
    RelRef,
    /// 16-byte StrView.
    Str,
    /// Offsets into a child vector; entry i spans offsets[i]..offsets[i+1].
    List,
    /// i64 microseconds.
    Interval,
    /// i64 days.
    Date,
    /// i64 microseconds since epoch, UTC.
    Timestamp,
}

impl PhysType {
    /// Bytes per value for word-addressed types; None for Bool, which is
    /// bit-packed.
    pub fn width(self) -> Option<usize> {
        match self {
            PhysType::Bool => None,
            PhysType::Str => Some(16),
            _ => Some(8),
        }
    }
}

/// Pack a node reference. Row is the table row (group * GROUP_ROWS + row
/// within group), which fits 48 bits with 2^48 rows per table to spare.
#[inline]
pub fn node_ref(table: u16, row: u64) -> u64 {
    debug_assert!(row < 1 << 48);
    (u64::from(table) << 48) | row
}

#[inline]
pub fn node_table(r: u64) -> u16 {
    (r >> 48) as u16
}

#[inline]
pub fn node_row(r: u64) -> u64 {
    r & ((1 << 48) - 1)
}

/// The storage group a node reference falls in, for gather radix splits.
#[inline]
pub fn node_group(r: u64) -> u32 {
    (node_row(r) / u64::from(GROUP_ROWS)) as u32
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VecEncoding {
    /// data holds len values of the physical width.
    Flat,
    /// data holds one value standing for len logical copies.
    Constant,
    /// data holds integer codes; the dictionary lives in aux. Strings
    /// flow as codes end to end and materialize only at the result
    /// boundary.
    Dict { codes_width: u8 },
}

/// Side data a vector may carry.
pub enum Aux {
    None,
    /// Buffers backing long string views.
    Str(Arc<StrBuffers>),
    /// Dictionary for code-encoded vectors, plus the buffers its views
    /// need if it is ever materialized.
    Dict(Arc<Dictionary>),
    /// Child vector for List entries.
    List(Box<ValueVector>),
}

pub struct ValueVector {
    pub phys: PhysType,
    pub encoding: VecEncoding,
    pub data: RawBuf,
    /// Absent means all valid, the common case; kernels skip the AND.
    pub validity: Option<Bitmap>,
    pub aux: Aux,
    pub len: u32,
}

impl ValueVector {
    /// Flat vector filled from a slice.
    pub fn flat_from<T: Pod>(arena: &mut MorselArena, phys: PhysType, values: &[T]) -> Self {
        debug_assert_eq!(phys.width(), Some(size_of::<T>()));
        let mut data = arena.alloc_of::<T>(values.len());
        data.as_mut_slice::<T>().copy_from_slice(values);
        Self {
            phys,
            encoding: VecEncoding::Flat,
            data,
            validity: None,
            aux: Aux::None,
            len: values.len() as u32,
        }
    }

    /// Flat vector with room for `len` values, to be filled by a kernel.
    pub fn flat_uninit(arena: &mut MorselArena, phys: PhysType, len: usize) -> Self {
        let width = phys.width().expect("word-addressed type");
        Self {
            phys,
            encoding: VecEncoding::Flat,
            data: arena.alloc(len * width, width.min(8)),
            validity: None,
            aux: Aux::None,
            len: len as u32,
        }
    }

    /// Constant vector: one stored value, `len` logical copies.
    pub fn constant<T: Pod>(arena: &mut MorselArena, phys: PhysType, value: T, len: usize) -> Self {
        debug_assert_eq!(phys.width(), Some(size_of::<T>()));
        let mut data = arena.alloc_of::<T>(1);
        data.as_mut_slice::<T>()[0] = value;
        Self {
            phys,
            encoding: VecEncoding::Constant,
            data,
            validity: None,
            aux: Aux::None,
            len: len as u32,
        }
    }

    /// Dict-encoded string vector: u16 codes into a shared dictionary.
    pub fn dict_str(arena: &mut MorselArena, codes: &[u16], dict: Arc<Dictionary>) -> Self {
        let mut data = arena.alloc_of::<u16>(codes.len());
        data.as_mut_slice::<u16>().copy_from_slice(codes);
        Self {
            phys: PhysType::Str,
            encoding: VecEncoding::Dict { codes_width: 2 },
            data,
            validity: None,
            aux: Aux::Dict(dict),
            len: codes.len() as u32,
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Flat values as a typed slice.
    pub fn values<T: Pod>(&self) -> &[T] {
        debug_assert_eq!(self.encoding, VecEncoding::Flat);
        debug_assert_eq!(self.phys.width(), Some(size_of::<T>()));
        &self.data.as_slice::<T>()[..self.len as usize]
    }

    pub fn values_mut<T: Pod>(&mut self) -> &mut [T] {
        debug_assert_eq!(self.encoding, VecEncoding::Flat);
        debug_assert_eq!(self.phys.width(), Some(size_of::<T>()));
        let len = self.len as usize;
        &mut self.data.as_mut_slice::<T>()[..len]
    }

    /// The single value of a Constant vector.
    pub fn constant_value<T: Pod>(&self) -> T {
        debug_assert_eq!(self.encoding, VecEncoding::Constant);
        self.data.as_slice::<T>()[0]
    }

    /// Codes of a dict vector (u16 width today; u8/u32 when a writer
    /// needs them).
    pub fn codes_u16(&self) -> &[u16] {
        debug_assert!(matches!(
            self.encoding,
            VecEncoding::Dict { codes_width: 2 }
        ));
        &self.data.as_slice::<u16>()[..self.len as usize]
    }

    pub fn dictionary(&self) -> &Dictionary {
        match &self.aux {
            Aux::Dict(d) => d,
            _ => panic!("not a dict vector"),
        }
    }

    pub fn str_buffers(&self) -> Option<&StrBuffers> {
        match &self.aux {
            Aux::Str(b) => Some(b),
            _ => None,
        }
    }

    /// Row validity, honoring the absent-means-valid convention.
    #[inline]
    pub fn is_valid(&self, i: usize) -> bool {
        self.validity.as_ref().is_none_or(|v| v.get(i))
    }
}

/// Convenience constructor for tests and result building: a flat string
/// vector from byte slices, short ones inline, long ones packed into one
/// shared buffer.
pub fn str_vector<B: AsRef<[u8]>>(arena: &mut MorselArena, values: &[B]) -> ValueVector {
    let mut long_bytes = Vec::new();
    let mut spans = Vec::with_capacity(values.len());
    for v in values {
        let v = v.as_ref();
        if v.len() > crate::str::INLINE_LEN {
            let start = long_bytes.len() as u32;
            long_bytes.extend_from_slice(v);
            spans.push(Some(start));
        } else {
            spans.push(None);
        }
    }
    let mut bufs = StrBuffers::new();
    let id = if long_bytes.is_empty() {
        0
    } else {
        bufs.push(Arc::from(long_bytes.into_boxed_slice()))
    };
    let mut vec = ValueVector::flat_uninit(arena, PhysType::Str, values.len());
    {
        let views = vec.values_mut::<StrView>();
        for (i, v) in values.iter().enumerate() {
            let v = v.as_ref();
            views[i] = match spans[i] {
                Some(start) => StrView::long(v, id, start),
                None => StrView::inline(v),
            };
        }
    }
    vec.aux = Aux::Str(Arc::new(bufs));
    vec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ref_packs() {
        let r = node_ref(3, (5 << 17) + 42);
        assert_eq!(node_table(r), 3);
        assert_eq!(node_row(r), (5 << 17) + 42);
        assert_eq!(node_group(r), 5);
    }

    #[test]
    fn flat_and_constant() {
        let mut arena = MorselArena::new();
        let v = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, 2, 3]);
        assert_eq!(v.values::<i64>(), &[1, 2, 3]);
        let c = ValueVector::constant(&mut arena, PhysType::Int64, 9i64, 100);
        assert_eq!(c.constant_value::<i64>(), 9);
        assert_eq!(c.len(), 100);
    }

    #[test]
    fn str_vector_roundtrip() {
        let mut arena = MorselArena::new();
        let vals = [
            "short",
            "a considerably longer string than twelve bytes",
            "",
        ];
        let v = str_vector(&mut arena, &vals);
        let bufs = v.str_buffers().unwrap();
        let views = v.values::<StrView>();
        for (view, want) in views.iter().zip(vals) {
            assert_eq!(view.bytes(bufs), want.as_bytes());
        }
    }
}
