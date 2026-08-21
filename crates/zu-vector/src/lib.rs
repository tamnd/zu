//! Typed columnar vectors and expression kernels (Spec/2064g/perf/02).
//!
//! The executor's tuple representation today is a Vec<Vec<Value>> with a
//! 32-byte tagged enum per cell. This crate replaces the cell type with
//! typed vectors: one contiguous buffer per column, validity as a bitmap,
//! filters as selection vectors, strings as 16-byte views into shared
//! buffers, and dictionary columns flowing as integer codes end to end.
//! All vector memory comes from a per-morsel bump arena, so steady-state
//! operator paths allocate nothing.
//!
//! The factorized execution model is unchanged; only the bones are new.
//! The old executor keeps running while this crate lands standalone, then
//! the perf/03 executor consumes it natively.

mod arena;
mod bitmap;
mod chunk;
mod expr;
pub mod kernels;
mod sel;
mod str;
mod vector;

pub use arena::{BLOCK_BYTES, MorselArena, Pod, RawBuf};
pub use bitmap::Bitmap;
pub use chunk::{ChunkSet, DataChunk};
pub use expr::{ExprOp, OwnedValue, Program, Reg, const_str};
pub use kernels::{
    BinOp, CmpOp, MathOp, MathPair, StrCut, StrFold, StrLen, StrNorm, StrTrim, TrimSet,
};
pub use sel::SelVector;
pub use str::{Dictionary, INLINE_LEN, NO_BUFFERS, StrBuffers, StrBuilder, StrView};
pub use vector::{
    Aux, PhysType, ValueVector, VecEncoding, node_group, node_ref, node_row, node_table, str_vector,
};

/// Rows per vector, unchanged from the executor's chunk fill width.
/// A u16 selection index covers it with room to spare, and one vector
/// spans exactly two of the storage layer's 1024-row decode chunks.
pub const VECTOR_SIZE: usize = 2048;
