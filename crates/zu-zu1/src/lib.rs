//! The `zu1` native single-file storage engine.
//!
//! Byte-level format is specified in `docs/04-storage-zu1-format.md`.
//! This crate starts with the write-once file constants; headers, blocks,
//! node groups, and CSR build land in M1.

/// File magic: UTF-8 図 followed by `ZU1\0\n`.
pub const MAGIC: [u8; 8] = [0xE5, 0x9B, 0xB3, b'Z', b'U', b'1', 0x00, 0x0A];

/// Fixed block size in bytes.
pub const BLOCK_SIZE: u32 = 262_144;

/// Current format version.
pub const FORMAT_VERSION: u16 = 1;

/// Oldest reader version that can open files we write.
pub const MIN_READER_VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_spells_zu() {
        assert_eq!(std::str::from_utf8(&MAGIC[0..3]).unwrap(), "図");
        assert_eq!(&MAGIC[3..6], b"ZU1");
    }

    #[test]
    fn block_size_is_power_of_two() {
        assert!(BLOCK_SIZE.is_power_of_two());
        assert_eq!(BLOCK_SIZE, 256 * 1024);
    }
}
