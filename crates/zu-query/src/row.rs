//! Typed rows: reading a result as Rust types instead of matching on
//! [`Value`] at every call site (`dx/04` §4).
//!
//! A result is columns and rows of [`Value`], which is what the engine
//! produces and the wrong thing for a caller to write against. The
//! caller knows the shape of its own query, so `let (name, n): (&str,
//! i64) = row.get()?;` says what the statement returns once and lets
//! every use of it be an ordinary Rust value. The alternative that this
//! replaces is a `match` per column per call site, where the arm that
//! cannot happen is written anyway and is wrong more often than the
//! arms that can.
//!
//! Nothing here copies a string. A [`Row`] borrows the result it came
//! out of, so `&str` reads the bytes the executor already materialized
//! and `String` is only paid for by a caller who asked for one. That is
//! the reason [`Row`] is a borrowed view rather than an owned struct:
//! an owned row would have to clone every string to exist.
//!
//! Two kinds of failure live here and they are deliberately different
//! conditions. Asking a `STRING` column for an `i64` is `22G03 invalid
//! value type`, a data exception the standard defines, because it is a
//! statement about a value. Asking for a column that the result does
//! not have, by name or by index, is [`ZuError::InvalidArgument`],
//! because no value was involved: the caller and the query disagree
//! about the shape of the answer and that is a bug in the program
//! rather than a condition in the data.

use zu_common::gqlstatus::codes;
use zu_common::{Result, Temporal, ZuError};

use crate::cast::value_type;
use crate::exec::{QueryResult, Value};

/// One row of a result, borrowed from it.
///
/// The column names ride along with the values so that a row read out
/// of an iterator can still be asked for a column by name, which is
/// what a caller with a wide result wants and what a bare `&[Value]`
/// cannot answer.
#[derive(Debug, Clone, Copy)]
pub struct Row<'r> {
    columns: &'r [String],
    values: &'r [Value],
}

impl<'r> Row<'r> {
    /// A row over names and values that belong together. The two are
    /// the same length in every result the executor builds; a caller
    /// that pairs mismatched slices gets a row whose extra values have
    /// no name, which reads as a missing column rather than a panic.
    pub fn new(columns: &'r [String], values: &'r [Value]) -> Row<'r> {
        Row { columns, values }
    }

    /// How many columns this row has.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the row has no columns, which is what a statement with
    /// no projection returns.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The column names, in the order the statement projected them.
    pub fn columns(&self) -> &'r [String] {
        self.columns
    }

    /// The values, untyped, for a caller doing its own matching.
    pub fn values(&self) -> &'r [Value] {
        self.values
    }

    /// The whole row as one type: a tuple for the usual case, or
    /// anything implementing [`FromRow`].
    ///
    /// The arity is checked before the columns are read, so a tuple
    /// that does not match the projection is one error naming both
    /// counts rather than a type error about whichever column happened
    /// to be first.
    pub fn get<T: FromRow<'r>>(&self) -> Result<T> {
        T::from_row(*self)
    }

    /// One column by position, typed.
    pub fn get_at<T: FromValue<'r>>(&self, ix: usize) -> Result<T> {
        match self.values.get(ix) {
            Some(value) => T::from_value(value).map_err(|err| self.blame(err, ix)),
            None => Err(self.no_column(ix)),
        }
    }

    /// One column by name, typed. The name is the one the statement
    /// projected, so `RETURN p.name AS name` is `name` and a projection
    /// with no alias is spelled the way the plan spells it.
    pub fn get_by_name<T: FromValue<'r>>(&self, name: &str) -> Result<T> {
        let ix = self.index_of(name)?;
        self.get_at(ix)
    }

    /// One column by position, untyped.
    pub fn value(&self, ix: usize) -> Result<&'r Value> {
        match self.values.get(ix) {
            Some(value) => Ok(value),
            None => Err(self.no_column(ix)),
        }
    }

    /// The error for a column this row does not have. It is cold and
    /// out of line because every read tests for it and no read on a
    /// correct program takes it, so the branch that formats a message
    /// should not sit in the middle of the loop that reads a million
    /// rows.
    #[cold]
    #[inline(never)]
    fn no_column(&self, ix: usize) -> ZuError {
        ZuError::InvalidArgument(format!(
            "the result has {} columns, so there is no column {ix}",
            self.values.len()
        ))
    }

    /// One column by name, untyped.
    pub fn value_by_name(&self, name: &str) -> Result<&'r Value> {
        self.value(self.index_of(name)?)
    }

    /// The position of a column, or the error that lists what the
    /// result actually returned. Listing them is worth the allocation
    /// on a path that has already failed, because the usual cause is a
    /// name that the statement aliased differently and the list is the
    /// answer.
    fn index_of(&self, name: &str) -> Result<usize> {
        self.columns
            .iter()
            .position(|column| column == name)
            .ok_or_else(|| {
                ZuError::InvalidArgument(format!(
                    "the result has no column '{name}', it has {}",
                    if self.columns.is_empty() {
                        "none".to_string()
                    } else {
                        self.columns.join(", ")
                    }
                ))
            })
    }

    /// Names the column a conversion failed on, keeping the condition
    /// the conversion raised. A caller reading eight columns learns
    /// which one disagreed with it, which the type names alone do not
    /// say when two columns have the same type.
    #[cold]
    #[inline(never)]
    fn blame(&self, err: ZuError, ix: usize) -> ZuError {
        let ZuError::Gql(mut record) = err else {
            return err;
        };
        record.detail = match self.columns.get(ix) {
            Some(name) => format!("column '{name}': {}", record.detail),
            None => format!("column {ix}: {}", record.detail),
        };
        ZuError::Gql(record)
    }
}

/// The rows of a result, borrowed one at a time.
#[derive(Debug, Clone)]
pub struct RowIter<'r> {
    columns: &'r [String],
    rows: std::slice::Iter<'r, Vec<Value>>,
}

impl<'r> Iterator for RowIter<'r> {
    type Item = Row<'r>;

    fn next(&mut self) -> Option<Row<'r>> {
        self.rows
            .next()
            .map(|values| Row::new(self.columns, values))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
}

impl<'r> DoubleEndedIterator for RowIter<'r> {
    fn next_back(&mut self) -> Option<Row<'r>> {
        self.rows
            .next_back()
            .map(|values| Row::new(self.columns, values))
    }
}

impl ExactSizeIterator for RowIter<'_> {}

impl QueryResult {
    /// The rows, as typed views over this result.
    pub fn iter(&self) -> RowIter<'_> {
        RowIter {
            columns: &self.columns,
            rows: self.rows.iter(),
        }
    }

    /// One row by position, for the query that returns exactly one and
    /// whose caller should not have to write a loop to say so.
    pub fn row(&self, ix: usize) -> Result<Row<'_>> {
        match self.rows.get(ix) {
            Some(values) => Ok(Row::new(&self.columns, values)),
            None => Err(ZuError::InvalidArgument(format!(
                "the result has {} rows, so there is no row {ix}",
                self.rows.len()
            ))),
        }
    }

    /// Where a column is, by name, or `None` when the result has no
    /// such column. This is the cheap question a caller asks once
    /// before a loop, so that the loop reads by index and never
    /// compares a string per row.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column == name)
    }
}

impl<'r> IntoIterator for &'r QueryResult {
    type Item = Row<'r>;
    type IntoIter = RowIter<'r>;

    fn into_iter(self) -> RowIter<'r> {
        self.iter()
    }
}

/// A Rust type one column can be read as.
///
/// The lifetime is the result's, which is what lets `&'r str` be an
/// implementation here and is the whole reason this is not a plain
/// `TryFrom<&Value>`: a borrowed string has to name the result it
/// borrows from.
pub trait FromValue<'r>: Sized {
    /// Reads `value`, or raises `22G03` naming both types.
    fn from_value(value: &'r Value) -> Result<Self>;
}

/// `22G03`, the condition a column of the wrong type raises, with the
/// type the result had and the type the caller asked for.
#[cold]
#[inline(never)]
fn mismatch(value: &Value, wanted: &str) -> ZuError {
    ZuError::gql(
        codes::C22G03,
        format!("expected {wanted}, the value is '{}'", value_type(value)),
    )
}

impl<'r> FromValue<'r> for &'r Value {
    fn from_value(value: &'r Value) -> Result<Self> {
        Ok(value)
    }
}

impl FromValue<'_> for Value {
    fn from_value(value: &Value) -> Result<Self> {
        Ok(value.clone())
    }
}

impl FromValue<'_> for bool {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Bool(b) => Ok(*b),
            other => Err(mismatch(other, "BOOL")),
        }
    }
}

impl FromValue<'_> for i64 {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Int(i) => Ok(*i),
            other => Err(mismatch(other, "INT64")),
        }
    }
}

/// The narrower integers, which are the same column read into a
/// smaller Rust type. Only the width check is theirs; a value that
/// does not fit raises `22003 numeric value out of range`, which is
/// the condition for exactly this and is not the same complaint as the
/// column being a string.
macro_rules! from_value_narrow_int {
    ($($ty:ty),+ $(,)?) => {
        $(impl FromValue<'_> for $ty {
            fn from_value(value: &Value) -> Result<Self> {
                match value {
                    Value::Int(i) => <$ty>::try_from(*i).map_err(|_| {
                        ZuError::gql(
                            codes::C22003,
                            format!("{i} does not fit in {}", stringify!($ty)),
                        )
                    }),
                    other => Err(mismatch(other, "INT64")),
                }
            }
        })+
    };
}

from_value_narrow_int!(i8, i16, i32, u8, u16, u32, u64, usize);

impl FromValue<'_> for f64 {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Float(f) => Ok(*f),
            // An integer column read as a float is exact up to 2^53 and
            // is what a caller averaging a count wants, so widening it
            // here saves a cast at every call site. The reverse is not
            // offered: a float read as an integer would have to round,
            // and rounding is a decision the caller makes with a cast
            // in the statement rather than one this layer makes silently.
            Value::Int(i) => Ok(*i as f64),
            other => Err(mismatch(other, "FLOAT64")),
        }
    }
}

impl FromValue<'_> for f32 {
    fn from_value(value: &Value) -> Result<Self> {
        f64::from_value(value).map(|f| f as f32)
    }
}

impl<'r> FromValue<'r> for &'r str {
    fn from_value(value: &'r Value) -> Result<Self> {
        match value {
            Value::Str(s) => Ok(s.as_str()),
            other => Err(mismatch(other, "STRING")),
        }
    }
}

impl FromValue<'_> for String {
    fn from_value(value: &Value) -> Result<Self> {
        <&str>::from_value(value).map(str::to_string)
    }
}

impl FromValue<'_> for Temporal {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Temporal(t) => Ok(*t),
            other => Err(mismatch(other, "a temporal")),
        }
    }
}

impl<'r> FromValue<'r> for &'r [Value] {
    fn from_value(value: &'r Value) -> Result<Self> {
        match value {
            Value::List(items) => Ok(items.as_slice()),
            // A path is a list of alternating nodes and edges, and a
            // caller that asked for the elements of one wants them
            // rather than a type error about a value it can see is a
            // sequence.
            Value::Path(items) => Ok(items.as_slice()),
            other => Err(mismatch(other, "LIST")),
        }
    }
}

impl<'r, T: FromValue<'r>> FromValue<'r> for Vec<T> {
    fn from_value(value: &'r Value) -> Result<Self> {
        <&[Value]>::from_value(value)?
            .iter()
            .map(T::from_value)
            .collect()
    }
}

/// Null is the one value every column can hold, whatever its type, so
/// `Option<T>` is how a caller says it knows that. A column that can be
/// null and is read as `T` raises `22G03` like any other mismatch,
/// which is the honest answer: the caller stated a type the value does
/// not have.
impl<'r, T: FromValue<'r>> FromValue<'r> for Option<T> {
    fn from_value(value: &'r Value) -> Result<Self> {
        match value {
            Value::Null => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

/// A Rust type one whole row can be read as, which is a tuple in every
/// case this crate provides. It is a trait rather than an inherent
/// method so that a caller can implement it for its own struct without
/// waiting for a derive.
pub trait FromRow<'r>: Sized {
    /// Reads the row, or raises what the first column that disagreed
    /// with the type raised.
    fn from_row(row: Row<'r>) -> Result<Self>;
}

/// The arity a tuple wanted against the arity the statement returned.
/// This is misuse rather than a data exception, because it is settled
/// by reading the query and never by looking at a value.
#[cold]
#[inline(never)]
fn wrong_arity(got: usize, wanted: usize) -> ZuError {
    ZuError::InvalidArgument(format!(
        "the row has {got} columns and the type asked for {wanted}"
    ))
}

macro_rules! from_row_tuple {
    ($($ty:ident => $ix:tt),+) => {
        impl<'r, $($ty: FromValue<'r>),+> FromRow<'r> for ($($ty,)+) {
            fn from_row(row: Row<'r>) -> Result<Self> {
                const WANTED: usize = [$(stringify!($ty)),+].len();
                let values = row.values();
                // The arity check is what makes the indexing below
                // valid, so the columns are read straight out of the
                // slice: one length test for the row instead of one
                // bounds test per column, which is the difference
                // between this costing a nanosecond and costing
                // several on a wide projection.
                if values.len() != WANTED {
                    return Err(wrong_arity(values.len(), WANTED));
                }
                Ok(($($ty::from_value(&values[$ix]).map_err(|err| row.blame(err, $ix))?,)+))
            }
        }
    };
}

from_row_tuple!(A => 0);
from_row_tuple!(A => 0, B => 1);
from_row_tuple!(A => 0, B => 1, C => 2);
from_row_tuple!(A => 0, B => 1, C => 2, D => 3);
from_row_tuple!(A => 0, B => 1, C => 2, D => 3, E => 4);
from_row_tuple!(A => 0, B => 1, C => 2, D => 3, E => 4, F => 5);
from_row_tuple!(A => 0, B => 1, C => 2, D => 3, E => 4, F => 5, G => 6);
from_row_tuple!(A => 0, B => 1, C => 2, D => 3, E => 4, F => 5, G => 6, H => 7);
from_row_tuple!(A => 0, B => 1, C => 2, D => 3, E => 4, F => 5, G => 6, H => 7, I => 8);
from_row_tuple!(A => 0, B => 1, C => 2, D => 3, E => 4, F => 5, G => 6, H => 7, I => 8, J => 9);
from_row_tuple!(A => 0, B => 1, C => 2, D => 3, E => 4, F => 5, G => 6, H => 7, I => 8, J => 9, K => 10);
from_row_tuple!(A => 0, B => 1, C => 2, D => 3, E => 4, F => 5, G => 6, H => 7, I => 8, J => 9, K => 10, L => 11);

/// A Rust value written into a parameter, the other direction.
///
/// These are `From` rather than a trait of their own because a
/// parameter is a value and nothing about writing one can fail: a
/// caller that has an `i64` has an `INT64`, and there is no width to
/// check or name to look up. The conversions a caller might expect and
/// will not find are the ones that need a decision, such as an
/// arbitrary string to a date, which the statement makes with
/// `date($text)` instead.
macro_rules! value_from_int {
    ($($ty:ty),+ $(,)?) => {
        $(impl From<$ty> for Value {
            fn from(n: $ty) -> Value {
                Value::Int(i64::from(n))
            }
        })+
    };
}

value_from_int!(i8, i16, i32, i64, u8, u16, u32);

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Value {
        Value::Float(f)
    }
}

impl From<f32> for Value {
    fn from(f: f32) -> Value {
        Value::Float(f.into())
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}

impl From<&String> for Value {
    fn from(s: &String) -> Value {
        Value::Str(s.clone())
    }
}

impl From<Temporal> for Value {
    fn from(t: Temporal) -> Value {
        Value::Temporal(t)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        match v {
            Some(v) => v.into(),
            None => Value::Null,
        }
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(items: Vec<T>) -> Value {
        Value::List(items.into_iter().map(Into::into).collect())
    }
}

impl<T: Into<Value> + Clone, const N: usize> From<[T; N]> for Value {
    fn from(items: [T; N]) -> Value {
        Value::List(items.into_iter().map(Into::into).collect())
    }
}

impl<T: Into<Value> + Clone> From<&[T]> for Value {
    fn from(items: &[T]) -> Value {
        Value::List(items.iter().cloned().map(Into::into).collect())
    }
}

/// The bindings for a statement, written the way the statement names
/// them.
///
/// ```ignore
/// let rows = conn.query_with(
///     "MATCH (p:person {id: $id}) WHERE p.name STARTS WITH $prefix RETURN p.name AS name",
///     &params! { "id" => 42, "prefix" => "A" },
/// )?;
/// ```
///
/// It expands to an array literal and not a `Vec`, so the bindings live
/// on the caller's stack and `&params!{...}` is the slice the query
/// methods take with no allocation at all. The names carry no `$`,
/// which is the same spelling the C ABI and the JSONL protocol use, so
/// one statement's parameter list reads the same in all three.
#[macro_export]
macro_rules! params {
    () => {{
        // Typed rather than bare, since an empty array literal on its
        // own has no element type to infer from.
        let empty: [(&str, $crate::exec::Value); 0] = [];
        empty
    }};
    ($($name:expr => $value:expr),+ $(,)?) => {
        [$(($name, $crate::exec::Value::from($value))),+]
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result() -> QueryResult {
        QueryResult::new(
            vec!["name".to_string(), "n".to_string(), "score".to_string()],
            vec![
                vec![
                    Value::Str("ada".to_string()),
                    Value::Int(3),
                    Value::Float(1.5),
                ],
                vec![Value::Str("bob".to_string()), Value::Int(4), Value::Null],
            ],
        )
    }

    #[test]
    fn a_row_reads_as_a_tuple_of_the_types_the_statement_returns() {
        let result = result();
        let mut seen = Vec::new();
        for row in result.iter() {
            let (name, n, score): (&str, i64, Option<f64>) = row.get().expect("typed");
            seen.push((name.to_string(), n, score));
        }
        assert_eq!(
            seen,
            vec![
                ("ada".to_string(), 3, Some(1.5)),
                ("bob".to_string(), 4, None),
            ]
        );
    }

    #[test]
    fn columns_read_by_name_and_by_position_are_the_same_column() {
        let result = result();
        let row = result.row(0).expect("a row");
        assert_eq!(row.get_by_name::<&str>("name").expect("by name"), "ada");
        assert_eq!(row.get_at::<&str>(0).expect("by index"), "ada");
        assert_eq!(result.column_index("n"), Some(1));
        assert_eq!(result.column_index("missing"), None);
    }

    #[test]
    fn a_column_of_the_wrong_type_is_a_data_exception_naming_the_column() {
        let result = result();
        let row = result.row(0).expect("a row");
        let err = row.get_at::<i64>(0).expect_err("a string is not an int");
        assert_eq!(err.gqlstatus(), Some(codes::C22G03));
        let message = err.to_string();
        assert!(message.contains("column 'name'"), "{message}");
        assert!(message.contains("INT64"), "{message}");
        assert!(message.contains("STRING"), "{message}");
    }

    #[test]
    fn a_column_that_is_not_there_is_misuse_and_lists_the_ones_that_are() {
        let result = result();
        let row = result.row(0).expect("a row");
        let err = row.get_by_name::<&str>("nmae").expect_err("no such column");
        assert!(err.gqlstatus().is_none(), "{err} carries a condition");
        assert!(err.to_string().contains("name, n, score"), "{err}");
        let err = row.get_at::<&str>(9).expect_err("no such column");
        assert!(err.to_string().contains("3 columns"), "{err}");
        let err = result.row(7).expect_err("no such row");
        assert!(err.to_string().contains("2 rows"), "{err}");
    }

    #[test]
    fn a_tuple_of_the_wrong_width_says_both_widths_and_reads_nothing() {
        let result = result();
        let err = result
            .row(0)
            .expect("a row")
            .get::<(&str, i64)>()
            .expect_err("three columns, two slots");
        assert!(err.gqlstatus().is_none(), "{err} carries a condition");
        assert!(err.to_string().contains("3 columns"), "{err}");
        assert!(err.to_string().contains("asked for 2"), "{err}");
    }

    #[test]
    fn a_narrow_integer_checks_its_width_and_says_so_when_it_does_not_fit() {
        let big = Value::Int(i64::from(i32::MAX) + 1);
        assert_eq!(u32::from_value(&Value::Int(7)).expect("fits"), 7);
        let err = i32::from_value(&big).expect_err("does not fit");
        assert_eq!(err.gqlstatus(), Some(codes::C22003));
        let err = u32::from_value(&Value::Int(-1)).expect_err("not unsigned");
        assert_eq!(err.gqlstatus(), Some(codes::C22003));
    }

    #[test]
    fn a_list_column_reads_as_a_vec_of_the_element_type() {
        let value = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert_eq!(
            Vec::<i64>::from_value(&value).expect("a list"),
            vec![1, 2, 3]
        );
        assert_eq!(<&[Value]>::from_value(&value).expect("a list").len(), 3);
        let err = Vec::<&str>::from_value(&value).expect_err("ints are not strings");
        assert_eq!(err.gqlstatus(), Some(codes::C22G03));
    }

    #[test]
    fn an_integer_column_widens_to_a_float_and_a_float_does_not_narrow() {
        assert_eq!(f64::from_value(&Value::Int(3)).expect("widens"), 3.0);
        let err = i64::from_value(&Value::Float(3.0)).expect_err("does not narrow");
        assert_eq!(err.gqlstatus(), Some(codes::C22G03));
    }

    #[test]
    fn parameters_are_written_as_values_without_a_conversion_at_the_call_site() {
        let bound = params! { "id" => 42, "name" => "ada", "on" => true, "ids" => vec![1, 2] };
        assert_eq!(bound[0], ("id", Value::Int(42)));
        assert_eq!(bound[1], ("name", Value::Str("ada".to_string())));
        assert_eq!(bound[2], ("on", Value::Bool(true)));
        assert_eq!(
            bound[3],
            ("ids", Value::List(vec![Value::Int(1), Value::Int(2)]))
        );
        let none: Option<i64> = None;
        let bound = params! { "missing" => none, "score" => 1.5 };
        assert_eq!(bound[0], ("missing", Value::Null));
        assert_eq!(bound[1], ("score", Value::Float(1.5)));
        let empty = params! {};
        assert!(empty.is_empty());
    }

    #[test]
    fn a_result_iterates_by_reference_and_the_rows_borrow_it() {
        let result = result();
        let names: Vec<&str> = (&result)
            .into_iter()
            .map(|row| row.get_at::<&str>(0).expect("a name"))
            .collect();
        assert_eq!(names, vec!["ada", "bob"]);
        assert_eq!(result.iter().len(), 2);
        let last = result.iter().next_back().expect("last");
        assert_eq!(last.get_at::<i64>(1).expect("a count"), 4);
    }
}
