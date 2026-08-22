//! The length of time between two instants, GF of ISO 20.28.
//!
//! Both spellings are here, the one that counts in days and smaller and
//! the one that counts in months and larger, and they are not the same
//! kind of work. A day-time duration is a subtraction: every operand is
//! a count of nanoseconds once a date has been multiplied out to one,
//! so the answer is the difference of two words and the loop is the
//! loop a subtraction of two numbers gets. A year-month duration is a
//! walk over the calendar, because the count it wants is a number of
//! months and months are of different lengths, so there is no scaling
//! that turns one into the other and the answer has to be read off the
//! civil date and then corrected by a step.
//!
//! What both have in common is that the type is settled before the loop
//! starts. The op carries the type both operands hold, so the choice
//! between a scale of one and a scale of a day's worth of nanoseconds
//! is made once a chunk rather than once a row, and no value in the
//! loop carries a tag for the loop to read.
//!
//! The conditions are the row engine's, in the row engine's words. A
//! pair with no duration between them raises `22G03`, and that is not
//! an edge case reached only by absurd input: a date is days and a
//! day-time duration is nanoseconds, so any date more than about two
//! hundred and ninety years from the epoch has no day-time duration to
//! anything at all. The compute loop uses checked arithmetic and notes
//! that some row had no answer; only then does a second walk go over
//! the selection to find which row it was, since a row the selection
//! dropped is a row the row engine never evaluated and has no condition
//! to raise.

use zu_common::types::LogicalType;
use zu_common::{DurationKind, Result, Temporal, ZuError, gqlstatus::codes};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::sel::SelVector;
use crate::vector::{PhysType, ValueVector, VecEncoding};

/// The words of an operand, however it is encoded.
///
/// A constant is one word standing for the whole chunk, which is what a
/// literal instant arrives as, and it is worth reading through the same
/// hole as a stored column rather than writing the loop twice for it:
/// the branch is on a value that does not change across the chunk, so
/// it costs a prediction and nothing else, and this loop is doing
/// checked arithmetic rather than something a branch would spoil.
#[derive(Clone, Copy)]
enum Words<'a> {
    Flat(&'a [i64]),
    Const(i64),
}

impl Words<'_> {
    #[inline(always)]
    fn at(self, row: usize) -> i64 {
        match self {
            Words::Flat(v) => v[row],
            Words::Const(c) => c,
        }
    }
}

fn words<'a>(v: &'a ValueVector, len: usize) -> Result<Words<'a>> {
    match v.encoding {
        VecEncoding::Flat => Ok(Words::Flat(&v.values::<i64>()[..len])),
        VecEncoding::Constant => Ok(Words::Const(v.constant_value::<i64>())),
        VecEncoding::Dict { .. } => Err(ZuError::InvalidArgument(
            "a dictionary holds strings and an instant is not one".into(),
        )),
    }
}

/// How many nanoseconds one of this type's words is worth, for the
/// types whose words are a count of something a day-time duration also
/// counts.
///
/// A date is days and everything else here is nanoseconds already, so
/// this is a day or it is one. It is the whole of the difference
/// between the three day-time loops, which is why there is one loop
/// rather than three.
fn scale(of: &LogicalType) -> Option<i64> {
    match of {
        LogicalType::Date => Some(zu_common::temporal::NANOS_PER_DAY),
        LogicalType::LocalTime | LogicalType::LocalDatetime => Some(1),
        _ => None,
    }
}

/// The value one word stands for, at the type the operands hold.
fn maker(of: &LogicalType) -> Option<fn(i64) -> Temporal> {
    match of {
        LogicalType::Date => Some(|w| Temporal::Date(w as i32)),
        LogicalType::LocalTime => Some(Temporal::LocalTime),
        LogicalType::LocalDatetime => Some(Temporal::LocalDatetime),
        _ => None,
    }
}

/// The duration that carries `from` to `to`, over two vectors of the
/// same temporal type.
///
/// `of` is that type, which the compiler read off the columns and both
/// operands share. `sel` is the chunk's selection, which is what the
/// conditions are raised over and nothing else reads.
pub fn duration_between(
    arena: &mut MorselArena,
    of: &LogicalType,
    kind: DurationKind,
    from: &ValueVector,
    to: &ValueVector,
    sel: Option<&SelVector>,
) -> Result<ValueVector> {
    debug_assert_eq!(from.len, to.len);
    if from.phys != to.phys {
        return Err(ZuError::InvalidArgument(format!(
            "no duration between {:?} and {:?}",
            from.phys, to.phys
        )));
    }
    let len = from.len as usize;
    let (a, b) = (words(from, len)?, words(to, len)?);
    let mut out = ValueVector::flat_uninit(arena, PhysType::Interval, len);
    let mut short = false;
    {
        let dst = out.values_mut::<i64>();
        match kind {
            DurationKind::DayTime => {
                let Some(scale) = scale(of) else {
                    return Err(ZuError::InvalidArgument(format!(
                        "no day-time duration kernel over {of:?}"
                    )));
                };
                for (row, out) in dst.iter_mut().enumerate().take(len) {
                    match count_nanos(a.at(row), b.at(row), scale) {
                        Some(n) => *out = n,
                        None => {
                            *out = 0;
                            short = true;
                        }
                    }
                }
            }
            DurationKind::YearMonth => {
                let Some(make) = maker(of) else {
                    return Err(ZuError::InvalidArgument(format!(
                        "no year-month duration kernel over {of:?}"
                    )));
                };
                for (row, out) in dst.iter_mut().enumerate().take(len) {
                    match count_months(make, a.at(row), b.at(row)) {
                        Some(n) => *out = n,
                        None => {
                            *out = 0;
                            short = true;
                        }
                    }
                }
            }
        }
    }
    out.validity = merged(arena, from, to, len);
    if short {
        raise(of, kind, a, b, &out, sel, len)?;
    }
    Ok(out)
}

/// The nanoseconds from one word to another, both counts of `scale`
/// nanoseconds apiece.
///
/// Each side is scaled before the subtraction rather than the
/// difference being scaled after it, which matters and is not a
/// rearrangement: a date in the year nine thousand is not a number of
/// nanoseconds at all, so the pair has no day-time duration between
/// them even where the gap between the two would have fitted. That is
/// what the row engine answers, and scaling the difference would have
/// answered a number instead.
#[inline(always)]
fn count_nanos(from: i64, to: i64, scale: i64) -> Option<i64> {
    to.checked_mul(scale)?.checked_sub(from.checked_mul(scale)?)
}

/// The months from one word to another, off the calendar.
#[inline(always)]
fn count_months(make: fn(i64) -> Temporal, from: i64, to: i64) -> Option<i64> {
    match Temporal::between(make(from), make(to), DurationKind::YearMonth) {
        Some(Temporal::Duration(_, months)) => Some(months),
        _ => None,
    }
}

/// The condition, once the compute loop has said that some row had no
/// answer.
///
/// Which row it was is found by walking the selection, because a row
/// outside it was computed for the loop's sake and is none of the
/// query's business. Where every row that had no answer is a row nobody
/// selected, or is null, there is no condition and this returns.
fn raise(
    of: &LogicalType,
    kind: DurationKind,
    a: Words<'_>,
    b: Words<'_>,
    out: &ValueVector,
    sel: Option<&SelVector>,
    len: usize,
) -> Result<()> {
    let make = maker(of).ok_or_else(|| {
        ZuError::InvalidArgument(format!("no duration between two values of {of:?}"))
    })?;
    let scale = scale(of);
    let check = |row: usize| -> Result<()> {
        if out.validity.as_ref().is_some_and(|v| !v.get(row)) {
            return Ok(());
        }
        let (from, to) = (a.at(row), b.at(row));
        let answered = match kind {
            DurationKind::DayTime => count_nanos(from, to, scale.unwrap_or(1)).is_some(),
            DurationKind::YearMonth => count_months(make, from, to).is_some(),
        };
        if answered {
            return Ok(());
        }
        let (from, to) = (make(from), make(to));
        Err(ZuError::gql(
            codes::C22G03,
            format!(
                "there is no {} from {from} to {to}",
                LogicalType::Duration(kind)
            ),
        ))
    };
    match sel {
        Some(sel) => {
            for &row in sel.as_slice() {
                check(row as usize)?;
            }
        }
        None => {
            for row in 0..len {
                check(row)?;
            }
        }
    }
    Ok(())
}

/// Valid where both operands are, which is where a null answers a null.
fn merged(arena: &mut MorselArena, l: &ValueVector, r: &ValueVector, len: usize) -> Option<Bitmap> {
    match (&l.validity, &r.validity) {
        (None, None) => None,
        (a, b) => {
            let mut v = Bitmap::new_in(arena, len, true);
            if let Some(a) = a {
                v.and_with(a);
            }
            if let Some(b) = b {
                v.and_with(b);
            }
            Some(v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000_000_000;

    fn between(of: &LogicalType, kind: DurationKind, from: &[i64], to: &[i64]) -> Result<Vec<i64>> {
        let mut arena = MorselArena::new();
        let a = ValueVector::flat_from(&mut arena, PhysType::Date, from);
        let b = ValueVector::flat_from(&mut arena, PhysType::Date, to);
        let out = duration_between(&mut arena, of, kind, &a, &b, None)?;
        Ok(out.values::<i64>()[..from.len()].to_vec())
    }

    #[test]
    fn a_day_time_duration_between_two_dates_is_the_days_in_nanoseconds() {
        let got = between(
            &LogicalType::Date,
            DurationKind::DayTime,
            &[0, 10, -5],
            &[1, 3, 5],
        )
        .unwrap();
        assert_eq!(got, vec![DAY, -7 * DAY, 10 * DAY]);
    }

    #[test]
    fn a_day_time_duration_between_two_datetimes_is_the_difference() {
        let got = between(
            &LogicalType::LocalDatetime,
            DurationKind::DayTime,
            &[0, 1_000],
            &[DAY, 400],
        )
        .unwrap();
        assert_eq!(got, vec![DAY, -600]);
    }

    /// A date the calendar has and a day-time duration cannot reach.
    /// The gap between the two below is one day, and the answer is
    /// still a condition, because neither of them is a number of
    /// nanoseconds on its own.
    #[test]
    fn a_date_too_far_out_has_no_day_time_duration_to_anything() {
        let err = between(
            &LogicalType::Date,
            DurationKind::DayTime,
            &[2_000_000],
            &[2_000_001],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("there is no"),
            "expected a 22G03, got {err}"
        );
    }

    #[test]
    fn a_year_month_duration_counts_whole_months() {
        // 1970-01-01 to 1971-03-01 is fourteen months, and one day
        // short of it is thirteen, since the count is of whole months.
        let got = between(
            &LogicalType::Date,
            DurationKind::YearMonth,
            &[0, 0],
            &[424, 423],
        )
        .unwrap();
        assert_eq!(got, vec![14, 13]);
    }

    /// A row the selection dropped was computed and is not the query's,
    /// so a pair with no answer there raises nothing.
    #[test]
    fn a_row_outside_the_selection_raises_nothing() {
        let mut arena = MorselArena::new();
        let a = ValueVector::flat_from(&mut arena, PhysType::Date, &[0i64, 2_000_000]);
        let b = ValueVector::flat_from(&mut arena, PhysType::Date, &[1i64, 2_000_001]);
        let mut sel = SelVector::with_capacity(&mut arena, 1);
        sel.push(0);
        let out = duration_between(
            &mut arena,
            &LogicalType::Date,
            DurationKind::DayTime,
            &a,
            &b,
            Some(&sel),
        )
        .expect("the selected row has an answer");
        assert_eq!(out.values::<i64>()[0], DAY);
    }
}
