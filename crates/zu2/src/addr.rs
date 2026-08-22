//! The log's address space.
//!
//! One flat `u64` over the whole log, memory and disk alike, split into
//! a page number and an offset in that page. Nothing outside
//! [`Log`](crate::log::Log) interprets an address; everyone else hands
//! it back and asks for bytes. The file mirrors the address space
//! one to one, so the file offset of a record is its address, and the
//! few bytes at the end of a page that a record did not fit into are a
//! hole in the file rather than a shift in the mapping. That costs
//! sparse-file slack of under a record per page and buys a recovery
//! scan that needs no translation table.

/// Bits of a page-relative offset, so a page is 4 MiB.
///
/// FASTER uses 32 MiB. 4 MiB is the same design with a finer unit of
/// flush and eviction, which matters more here because the working sets
/// under test are small enough that a 32 MiB page would be the whole
/// database.
pub const PAGE_BITS: u32 = 22;

/// Bytes in a page.
pub const PAGE_SIZE: usize = 1 << PAGE_BITS;

const OFFSET_MASK: u64 = (1 << PAGE_BITS) - 1;

/// Addresses are carried in 48 bits inside an index entry, so the page
/// table is sized to what an entry can name: 2^26 pages of 4 MiB, which
/// is 256 TiB of log.
pub const MAX_PAGES: usize = 1 << (48 - PAGE_BITS);

/// A byte position in the log.
pub type Address = u64;

/// The address no record has. An index entry holds it when empty and a
/// record header holds it when it is the oldest version of its key.
pub const NULL: Address = 0;

/// Where the first record goes. Address 0 has to stay unused for
/// [`NULL`] to mean what it says.
pub const FIRST: Address = 8;

/// The page an address falls in.
#[inline]
pub const fn page_of(address: Address) -> usize {
    (address >> PAGE_BITS) as usize
}

/// The offset of an address within its page.
#[inline]
pub const fn offset_of(address: Address) -> usize {
    (address & OFFSET_MASK) as usize
}

/// The address of the first byte of a page.
#[inline]
pub const fn page_start(page: usize) -> Address {
    (page as u64) << PAGE_BITS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_splits_into_a_page_and_an_offset() {
        let a = page_start(3) + 17;
        assert_eq!(page_of(a), 3);
        assert_eq!(offset_of(a), 17);
        assert_eq!(page_start(page_of(a)) + offset_of(a) as u64, a);
    }

    #[test]
    fn the_page_table_covers_what_an_index_entry_can_name() {
        assert_eq!(page_start(MAX_PAGES), 1 << 48);
    }
}
