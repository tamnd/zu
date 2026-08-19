//! Running a `MERGE`: what a statement has already merged, so that it
//! does not merge it twice.
//!
//! Most of the clause is [`crate::insert`] and [`crate::set`] already.
//! The elements a merge writes are an insert of its pattern over the
//! slots the walk bound, `ON MATCH SET` is a set over the rows the walk
//! found, and which rows those are is the row itself: the walk runs as
//! an optional match, so a row holding null where the pattern is is a
//! row it found nothing for.
//!
//! What is left is the one question neither of them has to answer.
//! `UNWIND [1, 1] AS x MERGE (n:person {id: x})` is two rows and one
//! element. The walk answers both against the store as it was when the
//! statement started, so it finds nothing for either, and the store
//! cannot say otherwise: nothing this statement writes reaches it until
//! the statement commits. So the rows are kept here as they are merged,
//! under what the pattern was given and what it writes, and a row that
//! asks for something an earlier row already merged takes the element
//! that row made.

use zu_common::{IdMap, Result};

use crate::query::Value;

/// The rows one hash covers: what each of them merged, and the elements
/// that came of it.
type Bucket = Vec<(Vec<Value>, Vec<Value>)>;

/// What one statement has merged so far, by a hash of what it merged.
///
/// The hash only says where to look. Two rows merge the same thing when
/// the values are equal, and equality is a question the values answer
/// themselves, so a hash that puts two different rows in one bucket
/// costs a comparison rather than an answer.
#[derive(Default)]
pub(crate) struct Merged {
    seen: IdMap<u64, Bucket>,
}

impl Merged {
    /// The elements this row merges: the ones an earlier row of the
    /// same statement made for it, or the ones `write` makes now.
    ///
    /// `given` is the endpoints the pattern was handed rather than
    /// wrote, and `props` is every property it writes. Together they
    /// are the whole of what the pattern says, which is why two rows
    /// that agree on them are merging one thing: everything else about
    /// a row is a name the pattern never reads.
    pub(crate) fn made(
        &mut self,
        given: &[Value],
        props: &[Value],
        write: impl FnOnce() -> Result<Vec<Value>>,
    ) -> Result<Vec<Value>> {
        let mut hash = 0u64;
        for value in given.iter().chain(props) {
            stir(&mut hash, value);
        }
        let bucket = self.seen.entry(hash).or_default();
        if let Some((_, made)) = bucket
            .iter()
            .find(|(key, _)| key.iter().eq(given.iter().chain(props)))
        {
            return Ok(made.clone());
        }
        let made = write()?;
        bucket.push((given.iter().chain(props).cloned().collect(), made.clone()));
        Ok(made)
    }
}

/// Stirs one value into a hash.
///
/// A value this has no shape for adds nothing, which is why the bucket
/// holds what it was made of and compares: a value that stirs in
/// nothing lands everything with it in one bucket, which is slower and
/// still right. The kinds that are here are the ones a merge key is
/// made of, which is what a pattern can write in a property and what a
/// slot can hold at an endpoint.
fn stir(hash: &mut u64, value: &Value) {
    match value {
        Value::Null => take(hash, 1),
        Value::Bool(yes) => take(hash, 2 + u64::from(*yes)),
        Value::Int(n) => take(hash, *n as u64),
        // The bits rather than the number, because the question is
        // whether two rows merge the same thing and not whether the two
        // values compare equal.
        Value::Float(f) => take(hash, f.to_bits()),
        Value::Str(text) => {
            take(hash, text.len() as u64);
            for byte in text.as_bytes() {
                take(hash, u64::from(*byte));
            }
        }
        Value::Node { table, offset } => {
            take(hash, u64::from(*table));
            take(hash, *offset);
        }
        Value::Rel {
            table, src, dst, ..
        } => {
            take(hash, u64::from(*table));
            take(hash, *src);
            take(hash, *dst);
        }
        Value::List(items) => {
            for item in items {
                stir(hash, item);
            }
        }
        _ => {}
    }
}

/// One number into the hash: a rotate so that the order the values came
/// in matters, an xor so that the number is in it, and a multiply by
/// the odd constant that spreads it over the whole word.
fn take(hash: &mut u64, n: u64) {
    *hash = (hash.rotate_left(7) ^ n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn made(merged: &mut Merged, props: &[Value], next: &mut i64) -> Vec<Value> {
        merged
            .made(&[], props, || {
                *next += 1;
                Ok(vec![Value::Node {
                    table: 1,
                    offset: *next as u64,
                }])
            })
            .expect("write")
    }

    #[test]
    fn one_statement_merging_the_same_thing_twice_writes_it_once() {
        let mut merged = Merged::default();
        let mut next = 0;
        let first = made(&mut merged, &[Value::Int(7)], &mut next);
        let again = made(&mut merged, &[Value::Int(7)], &mut next);
        assert_eq!(first, again);
        assert_eq!(next, 1, "the second row wrote nothing");
    }

    #[test]
    fn two_keys_are_two_elements() {
        let mut merged = Merged::default();
        let mut next = 0;
        let first = made(&mut merged, &[Value::Int(7)], &mut next);
        let second = made(&mut merged, &[Value::Int(8)], &mut next);
        assert_ne!(first, second);
        assert_eq!(next, 2);
    }

    #[test]
    fn an_integer_and_the_string_of_its_digits_are_two_keys() {
        let mut merged = Merged::default();
        let mut next = 0;
        made(&mut merged, &[Value::Int(7)], &mut next);
        made(&mut merged, &[Value::Str("7".into())], &mut next);
        assert_eq!(next, 2);
    }

    #[test]
    fn the_endpoints_are_part_of_the_key() {
        let mut merged = Merged::default();
        let mut next = 0;
        let ends = |offset| [Value::Node { table: 3, offset }];
        let one = merged
            .made(&ends(1), &[], || Ok(vec![Value::Int(1)]))
            .expect("write");
        let two = merged
            .made(&ends(2), &[], || {
                next += 1;
                Ok(vec![Value::Int(2)])
            })
            .expect("write");
        assert_ne!(one, two, "a different end is a different edge");
        assert_eq!(next, 1);
    }

    #[test]
    fn a_value_the_hash_has_no_shape_for_still_compares() {
        let mut merged = Merged::default();
        let mut next = 0;
        let graph = |name: &str| [Value::Record(vec![(name.into(), Value::Int(1))])];
        made(&mut merged, &graph("a"), &mut next);
        made(&mut merged, &graph("a"), &mut next);
        made(&mut merged, &graph("b"), &mut next);
        assert_eq!(next, 2, "one bucket, two keys");
    }
}
