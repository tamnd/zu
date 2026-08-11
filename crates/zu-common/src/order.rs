//! Order-preserving byte keys for property values.
//!
//! Column statistics compare values that came from different types
//! against each other by byte order alone: a histogram boundary, a top
//! value, and the literal a query asks about all meet as `&[u8]`, and
//! the comparison has to mean the same thing the query means. Strings
//! already do, since zuQL compares them lexicographically. Integers do
//! not, so they get mapped here, and both the writer that builds the
//! statistics and the optimizer that reads them go through this one
//! function.

/// The order-preserving key of a signed integer: big endian so the
/// bytes compare most significant first, with the sign bit flipped so
/// negatives sort below non-negatives instead of above them.
pub fn int_key(v: i64) -> [u8; 8] {
    ((v as u64) ^ (1u64 << 63)).to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_keys_sort_the_way_the_integers_do() {
        let mut v = [3i64, -1, i64::MIN, 0, i64::MAX, -7, 1 << 40];
        let mut keyed: Vec<[u8; 8]> = v.iter().copied().map(int_key).collect();
        v.sort_unstable();
        keyed.sort_unstable();
        let want: Vec<[u8; 8]> = v.iter().copied().map(int_key).collect();
        assert_eq!(keyed, want);
    }
}
