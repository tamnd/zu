//! Packed graph identifiers.
//!
//! `NodeId` packs a table, a node group, and a row offset into one `u64`:
//!
//! ```text
//! bits 63..50  table_id     (14 bits, max 16 384 tables)
//! bits 49..28  node_group   (22 bits, max 4 194 304 groups per table)
//! bits 27..11  row          (17 bits, GROUP_ROWS = 131 072 rows per group)
//! bits 10..0   reserved     (11 bits, must be zero in v1)
//! ```
//!
//! The layout is format-stable: it is written to disk by every engine.

/// Rows per node group, fixed at 2^17.
pub const GROUP_ROWS: u32 = 1 << 17;

const TABLE_BITS: u32 = 14;
const GROUP_BITS: u32 = 22;
const ROW_BITS: u32 = 17;
const RESERVED_BITS: u32 = 11;

const ROW_SHIFT: u32 = RESERVED_BITS;
const GROUP_SHIFT: u32 = ROW_SHIFT + ROW_BITS;
const TABLE_SHIFT: u32 = GROUP_SHIFT + GROUP_BITS;

/// Identifies a node or rel table in the catalog.
pub type TableId = u16;

/// Index of a node group within a table.
pub type NodeGroupId = u32;

/// Row offset within a node group, `0..GROUP_ROWS`.
pub type NodeOffset = u32;

/// Checkpoint or commit epoch, monotonically increasing.
pub type Epoch = u64;

/// Packed node identifier. See the module docs for the bit layout.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

impl NodeId {
    /// Packs the three components. Debug-asserts that each fits its field.
    #[inline]
    pub fn new(table: TableId, group: NodeGroupId, row: NodeOffset) -> Self {
        debug_assert!(u32::from(table) < (1 << TABLE_BITS));
        debug_assert!(group < (1 << GROUP_BITS));
        debug_assert!(row < GROUP_ROWS);
        Self(
            (u64::from(table) << TABLE_SHIFT)
                | (u64::from(group) << GROUP_SHIFT)
                | (u64::from(row) << ROW_SHIFT),
        )
    }

    #[inline]
    pub fn table(self) -> TableId {
        (self.0 >> TABLE_SHIFT) as TableId
    }

    #[inline]
    pub fn group(self) -> NodeGroupId {
        ((self.0 >> GROUP_SHIFT) & ((1 << GROUP_BITS) - 1)) as NodeGroupId
    }

    #[inline]
    pub fn row(self) -> NodeOffset {
        ((self.0 >> ROW_SHIFT) & ((1 << ROW_BITS) - 1)) as NodeOffset
    }

    /// Row index within the table, `group * GROUP_ROWS + row`.
    #[inline]
    pub fn table_row(self) -> u64 {
        u64::from(self.group()) * u64::from(GROUP_ROWS) + u64::from(self.row())
    }
}

impl std::fmt::Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "NodeId({}:{}:{})",
            self.table(),
            self.group(),
            self.row()
        )
    }
}

/// Identifies a relationship: the rel table plus its position in CSR order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RelId {
    pub table: TableId,
    pub group: NodeGroupId,
    pub slot: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip() {
        let id = NodeId::new(3, 7, 42);
        assert_eq!(id.table(), 3);
        assert_eq!(id.group(), 7);
        assert_eq!(id.row(), 42);
        assert_eq!(id.table_row(), 7 * u64::from(GROUP_ROWS) + 42);
    }

    #[test]
    fn pack_extremes() {
        let id = NodeId::new((1 << 14) - 1, (1 << 22) - 1, GROUP_ROWS - 1);
        assert_eq!(id.table(), (1 << 14) - 1);
        assert_eq!(id.group(), (1 << 22) - 1);
        assert_eq!(id.row(), GROUP_ROWS - 1);
        let zero = NodeId::new(0, 0, 0);
        assert_eq!(zero.0, 0);
    }

    #[test]
    fn reserved_bits_are_zero() {
        let id = NodeId::new(123, 456, 789);
        assert_eq!(id.0 & ((1 << 11) - 1), 0);
    }

    #[test]
    fn ordering_follows_physical_layout() {
        let a = NodeId::new(1, 0, 100);
        let b = NodeId::new(1, 1, 0);
        let c = NodeId::new(2, 0, 0);
        assert!(a < b);
        assert!(b < c);
    }
}
