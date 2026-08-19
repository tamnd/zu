//! What the driver says about itself.
//!
//! ADBC's `get_info` is one row per fact and a dense union for the
//! value, because the facts are of different types. Building one by
//! hand is a little fiddly and entirely mechanical: a type id and an
//! offset per row, and one child array per member of the union whether
//! anything went into it or not.
//!
//! Only the facts that are true are here. The Arrow library versions
//! are absent, because a crate cannot read its dependency's version at
//! compile time without a build script and a version string that is
//! wrong is worse than one that is missing.

use std::collections::HashSet;
use std::sync::Arc;

use adbc_core::error::{Result, Status};
use adbc_core::options::InfoCode;
use adbc_core::schemas::GET_INFO_SCHEMA;
use arrow_array::builder::{ArrayBuilder, BooleanBuilder, Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch, UInt32Array, UnionArray, new_empty_array};
use arrow_schema::{DataType, UnionFields};

use crate::error::plain;

/// The union member each kind of value goes into, as the ADBC schema
/// numbers them.
const STRING: i8 = 0;
const BOOL: i8 = 1;
const INT64: i8 = 2;

/// One fact, in whichever member of the union holds it.
enum Fact {
    Str(&'static str),
    Bool(bool),
    Int(i64),
}

/// Everything this driver will answer, in the order the rows come out.
fn facts() -> Vec<(InfoCode, Fact)> {
    vec![
        (InfoCode::VendorName, Fact::Str("zu")),
        (
            InfoCode::VendorVersion,
            Fact::Str(env!("CARGO_PKG_VERSION")),
        ),
        // zu speaks GQL, the ISO/IEC 39075 language, and not SQL. A
        // caller that switches on this is asking which dialect to send,
        // and "SQL" would be the wrong answer to that question.
        (InfoCode::VendorSql, Fact::Bool(false)),
        (InfoCode::VendorSubstrait, Fact::Bool(false)),
        (InfoCode::DriverName, Fact::Str("zu-adbc")),
        (
            InfoCode::DriverVersion,
            Fact::Str(env!("CARGO_PKG_VERSION")),
        ),
        (
            InfoCode::DriverAdbcVersion,
            Fact::Int(adbc_core::constants::ADBC_VERSION_1_1_0 as i64),
        ),
    ]
}

/// The rows a caller asked for, or all of them.
///
/// A code this driver has no answer for is left out rather than
/// answered with a null, which is what the specification asks for: the
/// row is omitted.
pub(crate) fn batch(codes: Option<HashSet<InfoCode>>) -> Result<RecordBatch> {
    let wanted: Vec<(InfoCode, Fact)> = facts()
        .into_iter()
        .filter(|(code, _)| codes.as_ref().is_none_or(|asked| asked.contains(code)))
        .collect();

    let mut names = Vec::with_capacity(wanted.len());
    let mut ids = Vec::with_capacity(wanted.len());
    let mut offsets = Vec::with_capacity(wanted.len());
    let mut strings = StringBuilder::new();
    let mut bools = BooleanBuilder::new();
    let mut ints = Int64Builder::new();

    for (code, fact) in &wanted {
        names.push(u32::from(code));
        let (id, offset) = match fact {
            Fact::Str(text) => {
                strings.append_value(text);
                (STRING, strings.len() - 1)
            }
            Fact::Bool(truth) => {
                bools.append_value(*truth);
                (BOOL, bools.len() - 1)
            }
            Fact::Int(number) => {
                ints.append_value(*number);
                (INT64, ints.len() - 1)
            }
        };
        ids.push(id);
        offsets.push(offset as i32);
    }

    let fields = members()?;
    let built: Vec<ArrayRef> = fields
        .iter()
        .map(|(id, field)| match id {
            STRING => Arc::new(strings.finish()) as ArrayRef,
            BOOL => Arc::new(bools.finish()) as ArrayRef,
            INT64 => Arc::new(ints.finish()) as ArrayRef,
            // The members nothing here ever fills. They are still part
            // of the type, so a reader that walks the union finds an
            // array of the right type with nothing in it.
            _ => new_empty_array(field.data_type()),
        })
        .collect();

    let values = UnionArray::try_new(fields, ids.into(), Some(offsets.into()), built)
        .map_err(|err| plain(err.to_string(), Status::Internal))?;

    RecordBatch::try_new(
        GET_INFO_SCHEMA.clone(),
        vec![Arc::new(UInt32Array::from(names)), Arc::new(values)],
    )
    .map_err(|err| plain(err.to_string(), Status::Internal))
}

/// The union's members, taken off the schema ADBC publishes rather than
/// written out again here. Two copies of a type is two chances to get
/// it wrong.
fn members() -> Result<UnionFields> {
    match GET_INFO_SCHEMA.field(1).data_type() {
        DataType::Union(fields, _) => Ok(fields.clone()),
        other => Err(plain(
            format!("adbc's info schema has changed under this driver: {other}"),
            Status::Internal,
        )),
    }
}

#[cfg(test)]
mod tests {
    use arrow_array::{Array, StringArray};

    use super::*;

    #[test]
    fn the_driver_says_who_it_is() {
        let batch = batch(None).expect("the facts build");
        assert_eq!(batch.num_rows(), facts().len());
        let names = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("uint32");
        let values = batch
            .column(1)
            .as_any()
            .downcast_ref::<UnionArray>()
            .expect("a union");
        let vendor = u32::from(&InfoCode::VendorName);
        let at = (0..batch.num_rows())
            .find(|&row| names.value(row) == vendor)
            .expect("the vendor name is in there");
        let held = values.value(at);
        assert_eq!(
            held.as_any()
                .downcast_ref::<StringArray>()
                .expect("utf8")
                .value(0),
            "zu"
        );
    }

    #[test]
    fn asking_for_one_gets_one() {
        let batch = batch(Some(HashSet::from([InfoCode::DriverName])))
            .expect("one fact builds as well as seven");
        assert_eq!(batch.num_rows(), 1);
    }

    #[test]
    fn a_code_this_driver_has_no_answer_for_is_left_out() {
        let batch = batch(Some(HashSet::from([InfoCode::Other(99_999)])))
            .expect("a driver ignores what it does not know");
        assert_eq!(batch.num_rows(), 0);
    }
}
