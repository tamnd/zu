//! Temporal values: the six types of GV39, GV40 and GV41.
//!
//! A temporal value is a number and a meaning, never a struct of named
//! parts. A date is a day count, a time is a nanosecond count, and a
//! duration is a month count or a nanosecond count depending on which
//! of the two kinds it is. The calendar arithmetic happens at the edges
//! where text is read and written, so a comparison is an integer
//! comparison and a column of dates is a column of `i32`.
//!
//! Three decisions are worth writing down because they are the ones a
//! reader will want to argue with.
//!
//! A zone is an offset in minutes and never an IANA name. A name is a
//! rule that changes under a database's feet when the zone database is
//! updated, and a value that means something different tomorrow is not
//! a value. `2024-01-15T10:00+07:00` is what the standard asks for and
//! is what zu stores; the name that produced the offset belongs to the
//! session that wrote it, not to the row.
//!
//! The two duration kinds do not mix. A year-month duration counts
//! months and a day-time duration counts nanoseconds, and no number of
//! days is a month. An engine that stores both in one value has to
//! invent an answer for one month after 31 January, and the standard
//! does not ask for the invention, so zu refuses the arithmetic instead
//! of guessing.
//!
//! Adding months to a date does clamp, and that is a different case
//! from mixing kinds. One month after 31 January is 28 February, which
//! is not an invention but the answer every calendar gives, because the
//! month is named and the day is the largest that month has.

use std::fmt;

use crate::types::{DurationKind, LogicalType};

/// Nanoseconds in one second, minute, hour and day. The minute and the
/// day are public because an offset and a calendar day are the two
/// units a conversion between temporal types is stated in.
const NANOS_PER_SEC: i64 = 1_000_000_000;
pub const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SEC;
const NANOS_PER_HOUR: i64 = 60 * NANOS_PER_MINUTE;
pub const NANOS_PER_DAY: i64 = 24 * NANOS_PER_HOUR;

/// The first and last day the standard's calendar has, as day counts
/// from 1970-01-01.
///
/// ISO dates run from year 1 to year 9999, so an addition that lands
/// outside that is 22008 and not a wider date. An engine with a roomier
/// internal calendar still owes the overflow, because the value it
/// would return has no spelling in the type.
pub const MIN_DAY: i32 = -719_162;
pub const MAX_DAY: i32 = 2_932_896;

/// What a statement answers the datetime value functions with: the
/// instant it is running at and the displacement its session keeps.
///
/// It is one reading and not a call to the operating system per row,
/// because ISO 20.6 asks that every datetime value function in one
/// statement answer the same instant. A statement that read the clock
/// twice could have `CURRENT_DATE` and `CURRENT_TIMESTAMP` land on two
/// days, and a scan of ten million rows would spend ten million system
/// calls finding that out.
///
/// The displacement is minutes east of UTC and never a zone name, for
/// the reason the zoned types carry an offset: a name is a rule that
/// changes when the zone database is updated, and a value that means
/// something different tomorrow is not a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Clock {
    /// Nanoseconds since 1970-01-01T00:00:00Z.
    pub nanos: i64,
    /// Minutes east of UTC.
    pub offset: i16,
}

impl Clock {
    /// The clock read now, in UTC, which is what a statement does once
    /// before its first row.
    ///
    /// The displacement is nought because zu's session time zone is
    /// UTC, which is one of the implementation-defined choices the
    /// standard leaves open and is written down as such. A clock before
    /// the epoch on a machine whose time is set wrong reads as the
    /// epoch rather than refusing, since a statement asking what time
    /// it is has no better answer to give and no condition to raise.
    pub fn read() -> Clock {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| i64::try_from(since.as_nanos()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        Clock { nanos, offset: 0 }
    }

    /// The instant as a zoned datetime, which is the one value the five
    /// datetime value functions are cut out of.
    pub fn instant(self) -> Temporal {
        Temporal::ZonedDatetime {
            nanos: self.nanos,
            offset: self.offset,
        }
    }
}

/// One temporal value.
///
/// The variants carry counts and not fields. A zoned value carries the
/// instant and the offset separately, so two zoned values compare by
/// instant while each still prints in the zone it was written in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Temporal {
    /// Days since 1970-01-01.
    Date(i32),
    /// Nanoseconds since midnight.
    LocalTime(i64),
    /// Nanoseconds since midnight in the offset's own day, and the
    /// offset from UTC in minutes.
    ZonedTime { nanos: i64, offset: i16 },
    /// Nanoseconds since 1970-01-01T00:00:00.
    LocalDatetime(i64),
    /// Nanoseconds since the epoch in UTC, and the offset from UTC in
    /// minutes that the value was written with.
    ZonedDatetime { nanos: i64, offset: i16 },
    /// Months for a year-month duration, nanoseconds for a day-time
    /// one. The kind is carried because the number means nothing on
    /// its own.
    Duration(DurationKind, i64),
}

impl Temporal {
    /// The lattice type of this value.
    pub fn logical_type(&self) -> LogicalType {
        match self {
            Temporal::Date(_) => LogicalType::Date,
            Temporal::LocalTime(_) => LogicalType::LocalTime,
            Temporal::ZonedTime { .. } => LogicalType::ZonedTime,
            Temporal::LocalDatetime(_) => LogicalType::LocalDatetime,
            Temporal::ZonedDatetime { .. } => LogicalType::ZonedDatetime,
            Temporal::Duration(kind, _) => LogicalType::Duration(*kind),
        }
    }

    /// The value `text` spells as a value of type `ty`, or `None` when
    /// it spells none.
    ///
    /// A datetime type accepts a date on its own, at midnight, because
    /// the standard's datetime literal does. Nothing else is widened:
    /// a date is not a datetime and reading one as the other silently
    /// would answer a question nobody asked.
    pub fn parse(ty: &LogicalType, text: &str) -> Option<Temporal> {
        let text = text.trim();
        Some(match ty {
            LogicalType::Date => Temporal::Date(parse_date(text)?),
            LogicalType::LocalTime => {
                let (nanos, offset) = parse_time(text)?;
                if offset.is_some() {
                    return None;
                }
                Temporal::LocalTime(nanos)
            }
            LogicalType::ZonedTime => {
                let (nanos, offset) = parse_time(text)?;
                Temporal::ZonedTime {
                    nanos,
                    offset: offset.unwrap_or(0),
                }
            }
            LogicalType::LocalDatetime => {
                let (nanos, offset) = parse_datetime(text)?;
                if offset.is_some() {
                    return None;
                }
                Temporal::LocalDatetime(nanos)
            }
            LogicalType::ZonedDatetime => {
                let (nanos, offset) = parse_datetime(text)?;
                // The stored instant is UTC, so a written offset is
                // subtracted rather than kept alongside a local time.
                // Two values written in two zones for one instant are
                // then one number and compare equal, which is the
                // whole reason to store the instant.
                let offset = offset.unwrap_or(0);
                Temporal::ZonedDatetime {
                    nanos: nanos.checked_sub(i64::from(offset) * NANOS_PER_MINUTE)?,
                    offset,
                }
            }
            LogicalType::Duration(_) => parse_duration(text)?,
            _ => return None,
        })
    }

    /// The value `text` spells when the type is not written out, which
    /// is what a property in a fixture and a value read back from a
    /// column both need.
    ///
    /// The shape decides: a leading `P` is a duration, a `T` or a space
    /// in the middle is a datetime, a colon is a time, and a dash is a
    /// date.
    pub fn parse_any(text: &str) -> Option<Temporal> {
        let text = text.trim();
        let ty = if text.starts_with('P') || text.starts_with("-P") {
            LogicalType::Duration(DurationKind::DayTime)
        } else if text.contains('T') || text.contains(' ') {
            if has_offset(text) {
                LogicalType::ZonedDatetime
            } else {
                LogicalType::LocalDatetime
            }
        } else if text.contains(':') {
            if has_offset(text) {
                LogicalType::ZonedTime
            } else {
                LogicalType::LocalTime
            }
        } else {
            LogicalType::Date
        };
        Temporal::parse(&ty, text)
    }

    /// The duration `text` spells when it is written the way SQL
    /// writes one, `INTERVAL '3 04:05:06' DAY TO SECOND`, with the
    /// qualifier already read.
    ///
    /// The qualifier is not decoration. It is the whole of what says
    /// how to read the string, because `'1-2'` is a year and two
    /// months under `YEAR TO MONTH` and is nothing at all under `DAY`,
    /// and `'3'` is three of whichever single field was named. So this
    /// reads the fields the qualifier lists, in order, and refuses
    /// anything the qualifier did not ask for.
    ///
    /// The leading field is unbounded and the ones behind it are not:
    /// `'25' HOUR` is twenty five hours, and `'1 25:00:00' DAY TO
    /// SECOND` is not a day and an hour, because a written hour that
    /// follows a written day has 24 of its own and the standard says
    /// so. A number that runs over its field is refused rather than
    /// carried, since carrying it would answer a question the
    /// statement did not ask.
    pub fn parse_sql_interval(text: &str, qualifier: &IntervalQualifier) -> Option<Temporal> {
        if qualifier.start.kind() != qualifier.end.kind()
            || qualifier.start.rank() > qualifier.end.rank()
        {
            return None;
        }
        let (negative, body) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let fields = &FIELD_ORDER[qualifier.start.rank()..=qualifier.end.rank()];
        // The written value is one part a field, and which character
        // stands between two of them depends on the pair: a year and a
        // month are written with a minus sign, a day and the time
        // behind it with a space, and the rest with a colon. That is
        // `'1-2'`, `'3 04:05:06'` and `'04:05'`.
        let mut parts = Vec::with_capacity(fields.len());
        let mut rest = body;
        for (ix, field) in fields.iter().enumerate() {
            if ix + 1 == fields.len() {
                parts.push(rest);
                break;
            }
            let separator = match field {
                IntervalField::Year => '-',
                IntervalField::Day => ' ',
                _ => ':',
            };
            let (head, tail) = rest.split_once(separator)?;
            parts.push(head);
            rest = tail;
        }
        // A written precision is a statement about the leading field
        // alone, and it is the only meaning zu can give it, since the
        // duration types carry one precision each and it is not this
        // one.
        let whole = parts[0].split('.').next()?;
        if qualifier
            .leading
            .is_some_and(|limit| whole.len() > limit as usize)
        {
            return None;
        }
        let mut months = 0i64;
        let mut nanos = 0i64;
        for (ix, (field, text)) in fields.iter().zip(&parts).enumerate() {
            let leading = ix == 0;
            match field {
                IntervalField::Year => {
                    months = months.checked_add(uint(text)?.checked_mul(12)?)?;
                }
                IntervalField::Month => {
                    months = months.checked_add(bounded(text, leading, 11)?)?;
                }
                IntervalField::Day => {
                    nanos = nanos.checked_add(uint(text)?.checked_mul(NANOS_PER_DAY)?)?;
                }
                IntervalField::Hour => {
                    nanos = nanos
                        .checked_add(bounded(text, leading, 23)?.checked_mul(NANOS_PER_HOUR)?)?;
                }
                IntervalField::Minute => {
                    nanos = nanos
                        .checked_add(bounded(text, leading, 59)?.checked_mul(NANOS_PER_MINUTE)?)?;
                }
                IntervalField::Second => {
                    nanos =
                        nanos.checked_add(interval_seconds(text, leading, qualifier.fraction)?)?;
                }
            }
        }
        let sign = if negative { -1 } else { 1 };
        Some(match qualifier.start.kind() {
            DurationKind::YearMonth => Temporal::Duration(DurationKind::YearMonth, sign * months),
            DurationKind::DayTime => Temporal::Duration(DurationKind::DayTime, sign * nanos),
        })
    }
}

/// The six primary datetime fields, smallest last, which is the order
/// a qualifier names them in and the order they are written in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum IntervalField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

/// The fields in the order the standard writes them, so a qualifier is
/// a slice of this and never a list built by hand.
const FIELD_ORDER: [IntervalField; 6] = [
    IntervalField::Year,
    IntervalField::Month,
    IntervalField::Day,
    IntervalField::Hour,
    IntervalField::Minute,
    IntervalField::Second,
];

impl IntervalField {
    /// The field this word names, or `None` when it names none.
    pub fn spelled(word: &str) -> Option<IntervalField> {
        FIELD_ORDER
            .into_iter()
            .find(|field| word.eq_ignore_ascii_case(field.word()))
    }

    /// The word the standard writes this field with.
    pub fn word(self) -> &'static str {
        match self {
            IntervalField::Year => "YEAR",
            IntervalField::Month => "MONTH",
            IntervalField::Day => "DAY",
            IntervalField::Hour => "HOUR",
            IntervalField::Minute => "MINUTE",
            IntervalField::Second => "SECOND",
        }
    }

    /// Where this field stands among the six, which is what says
    /// whether one field may follow another.
    pub fn rank(self) -> usize {
        FIELD_ORDER
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0)
    }

    /// Which of the two duration kinds this field counts in. The kinds
    /// do not mix, so a qualifier naming one field from each is not a
    /// qualifier at all.
    pub fn kind(self) -> DurationKind {
        match self {
            IntervalField::Year | IntervalField::Month => DurationKind::YearMonth,
            _ => DurationKind::DayTime,
        }
    }
}

/// The qualifier of a SQL interval literal: the run of fields the
/// value is written in, and the precisions it was written with.
///
/// A single field is `start` and `end` the same, since one field is
/// the run of length one and nothing about the reading changes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IntervalQualifier {
    pub start: IntervalField,
    pub end: IntervalField,
    /// Digits the leading field may have, when a precision was
    /// written.
    pub leading: Option<u32>,
    /// Digits after the point in the seconds, when one was written.
    pub fraction: Option<u32>,
}

impl fmt::Display for IntervalQualifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.start == self.end {
            true => write!(f, "{}", self.start.word()),
            false => write!(f, "{} TO {}", self.start.word(), self.end.word()),
        }
    }
}

/// A run of digits and nothing else, which is what every field of a
/// SQL interval string is: the sign is written once, in front of them
/// all, and a field carries no sign of its own.
fn uint(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The same, with the bound a field carries when something is written
/// in front of it.
fn bounded(text: &str, leading: bool, most: i64) -> Option<i64> {
    let value = uint(text)?;
    match leading || value <= most {
        true => Some(value),
        false => None,
    }
}

/// The seconds of an interval string, which is the one field that may
/// carry a fraction.
fn interval_seconds(text: &str, leading: bool, digits: Option<u32>) -> Option<i64> {
    let (whole, frac) = match text.split_once('.') {
        Some((whole, frac)) => (whole, Some(frac)),
        None => (text, None),
    };
    let mut nanos = bounded(whole, leading, 59)?.checked_mul(NANOS_PER_SEC)?;
    if let Some(frac) = frac {
        if frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if digits.is_some_and(|limit| frac.len() > limit as usize) {
            return None;
        }
        let scale = 10i64.pow(9 - frac.len() as u32);
        nanos = nanos.checked_add(frac.parse::<i64>().ok()? * scale)?;
    }
    Some(nanos)
}

/// Whether a written time or datetime carries a zone.
///
/// The date is dropped first, because a date's own dashes are not
/// offsets and are the only other sign a written value has.
fn has_offset(text: &str) -> bool {
    let time = match text.split_once(['T', 't', ' ']) {
        Some((_, time)) => time,
        None => text,
    };
    time.ends_with('Z') || time.ends_with('z') || time.contains('+') || time.contains('-')
}

/// `yyyy-mm-dd` as a day count.
fn parse_date(text: &str) -> Option<i32> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i32 = text[0..4].parse().ok()?;
    let month: u32 = text[5..7].parse().ok()?;
    let day: u32 = text[8..10].parse().ok()?;
    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in(year, month)
    {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// `hh:mm[:ss[.fffffffff]][offset]` as nanoseconds since midnight and
/// the offset in minutes when one is written.
fn parse_time(text: &str) -> Option<(i64, Option<i16>)> {
    let (time, offset) = split_offset(text)?;
    let mut parts = time.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let (second, nanos) = match parts.next() {
        Some(rest) => parse_seconds(rest)?,
        None => (0, 0),
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let total = hour * NANOS_PER_HOUR + minute * NANOS_PER_MINUTE + second * NANOS_PER_SEC + nanos;
    Some((total, offset))
}

/// `ss` or `ss.fff` as whole seconds and nanoseconds.
fn parse_seconds(text: &str) -> Option<(i64, i64)> {
    match text.split_once('.') {
        None => Some((text.parse().ok()?, 0)),
        Some((whole, frac)) => {
            if frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let scale = 10i64.pow(9 - frac.len() as u32);
            Some((whole.parse().ok()?, frac.parse::<i64>().ok()? * scale))
        }
    }
}

/// A written time split into the part before the zone and the zone in
/// minutes, `None` in the second place when no zone is written.
///
/// The caller has already taken the date off, so a sign here can only
/// be an offset: a time of day has no other use for one.
fn split_offset(text: &str) -> Option<(&str, Option<i16>)> {
    if let Some(rest) = text.strip_suffix('Z').or_else(|| text.strip_suffix('z')) {
        return Some((rest, Some(0)));
    }
    let Some(sign) = text.find(['+', '-']) else {
        return Some((text, None));
    };
    let (body, zone) = text.split_at(sign);
    let negative = zone.starts_with('-');
    let digits = &zone[1..];
    let (hours, minutes) = match digits.split_once(':') {
        Some((h, m)) => (h, m),
        // `+0700` and `+07` are both written in the wild and both mean
        // the same offset.
        None if digits.len() == 4 => (&digits[0..2], &digits[2..4]),
        None => (digits, "0"),
    };
    let hours: i16 = hours.parse().ok()?;
    let minutes: i16 = minutes.parse().ok()?;
    if hours > 18 || minutes > 59 {
        return None;
    }
    let total = hours * 60 + minutes;
    Some((body, Some(if negative { -total } else { total })))
}

/// `yyyy-mm-ddThh:mm:ss` as nanoseconds since the epoch, plus the
/// offset when one is written.
fn parse_datetime(text: &str) -> Option<(i64, Option<i16>)> {
    let Some((date, time)) = text
        .split_once('T')
        .or_else(|| text.split_once('t'))
        .or_else(|| text.split_once(' '))
    else {
        // A date on its own is midnight, which is what the standard's
        // datetime literal says and what the cast matrix expects of
        // `CAST('2024-01-15' AS DATETIME)`. There is no offset to read
        // out of a date, so the value is local until a zoned type asks
        // for one and gets UTC.
        return Some((i64::from(parse_date(text)?) * NANOS_PER_DAY, None));
    };
    let days = parse_date(date)?;
    let (nanos, offset) = parse_time(time)?;
    Some((i64::from(days) * NANOS_PER_DAY + nanos, offset))
}

/// An ISO 8601 duration, `PnYnMnDTnHnMnS`.
///
/// The two kinds are decided by which fields are written, and a
/// duration that writes both is refused rather than split, because a
/// value that is half months and half nanoseconds is exactly the thing
/// the two kinds exist to prevent.
fn parse_duration(text: &str) -> Option<Temporal> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let rest = rest.strip_prefix('P').or_else(|| rest.strip_prefix('p'))?;
    let (date_part, time_part) = match rest.split_once(['T', 't']) {
        Some((d, t)) => (d, Some(t)),
        None => (rest, None),
    };
    if date_part.is_empty() && time_part.is_none_or(str::is_empty) {
        return None;
    }
    let mut months = 0i64;
    let mut nanos = 0i64;
    for (value, unit) in fields(date_part)? {
        match unit {
            'Y' | 'y' => months = months.checked_add(whole(value)?.checked_mul(12)?)?,
            'M' | 'm' => months = months.checked_add(whole(value)?)?,
            'W' | 'w' => nanos = nanos.checked_add(scaled(value, 7 * NANOS_PER_DAY)?)?,
            'D' | 'd' => nanos = nanos.checked_add(scaled(value, NANOS_PER_DAY)?)?,
            _ => return None,
        }
    }
    for (value, unit) in fields(time_part.unwrap_or(""))? {
        match unit {
            'H' | 'h' => nanos = nanos.checked_add(scaled(value, NANOS_PER_HOUR)?)?,
            'M' | 'm' => nanos = nanos.checked_add(scaled(value, NANOS_PER_MINUTE)?)?,
            'S' | 's' => nanos = nanos.checked_add(scaled(value, NANOS_PER_SEC)?)?,
            _ => return None,
        }
    }
    if months != 0 && nanos != 0 {
        return None;
    }
    let sign = if negative { -1 } else { 1 };
    Some(if nanos != 0 || months == 0 {
        Temporal::Duration(DurationKind::DayTime, sign * nanos)
    } else {
        Temporal::Duration(DurationKind::YearMonth, sign * months)
    })
}

/// A duration part split into its number and unit pairs.
fn fields(text: &str) -> Option<Vec<(&str, char)>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (ix, ch) in text.char_indices() {
        if ch.is_ascii_digit() || ch == '.' || ch == ',' {
            continue;
        }
        if ix == start {
            return None;
        }
        out.push((&text[start..ix], ch));
        start = ix + ch.len_utf8();
    }
    if start != text.len() {
        return None;
    }
    Some(out)
}

/// A field written without a fraction, which is what the month fields
/// need: a third of a month is not a number of anything.
fn whole(text: &str) -> Option<i64> {
    text.parse().ok()
}

/// A field times the size of its unit, with a fraction allowed because
/// `PT0.5S` is half a second and half a second is nanoseconds.
fn scaled(text: &str, unit: i64) -> Option<i64> {
    let text = text.replace(',', ".");
    match text.split_once('.') {
        None => text.parse::<i64>().ok()?.checked_mul(unit),
        Some((whole, frac)) => {
            if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let whole: i64 = if whole.is_empty() {
                0
            } else {
                whole.parse().ok()?
            };
            let digits = frac.len() as u32;
            let numerator: i64 = frac.parse().ok()?;
            let part = unit.checked_mul(numerator)? / 10i64.pow(digits);
            whole.checked_mul(unit)?.checked_add(part)
        }
    }
}

/// Days in `month` of `year`, which is where the leap rule lives.
fn days_in(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap(year) => 29,
        _ => 28,
    }
}

fn leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days from 1970-01-01 to a proleptic Gregorian date, and back.
///
/// This is Howard Hinnant's shift to a calendar whose year starts in
/// March, which makes the leap day the last day of the year and the
/// month lengths a repeating pattern with no table.
pub fn days_from_civil(year: i32, month: u32, day: u32) -> i32 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The year, month and day a day count spells.
pub fn civil_from_days(days: i32) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if month <= 2 { y + 1 } else { y }, month, day)
}

/// A date shifted by whole months, with the day clamped to the length
/// of the month it lands in. `None` when the result leaves the
/// calendar.
pub fn add_months(days: i32, months: i64) -> Option<i32> {
    let (year, month, day) = civil_from_days(days);
    let total = i64::from(year) * 12 + i64::from(month) - 1 + months;
    let new_year = i32::try_from(total.div_euclid(12)).ok()?;
    let new_month = (total.rem_euclid(12) + 1) as u32;
    if !(1..=9999).contains(&new_year) {
        return None;
    }
    let clamped = day.min(days_in(new_year, new_month));
    let out = days_from_civil(new_year, new_month, clamped);
    (MIN_DAY..=MAX_DAY).contains(&out).then_some(out)
}

impl fmt::Display for Temporal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Temporal::Date(days) => {
                let (y, m, d) = civil_from_days(days);
                write!(f, "{y:04}-{m:02}-{d:02}")
            }
            Temporal::LocalTime(nanos) => write_time(f, nanos),
            Temporal::ZonedTime { nanos, offset } => {
                write_time(f, nanos)?;
                write_offset(f, offset)
            }
            Temporal::LocalDatetime(nanos) => write_datetime(f, nanos),
            Temporal::ZonedDatetime { nanos, offset } => {
                write_datetime(f, nanos + i64::from(offset) * NANOS_PER_MINUTE)?;
                write_offset(f, offset)
            }
            Temporal::Duration(kind, count) => write_duration(f, kind, count),
        }
    }
}

fn write_time(f: &mut fmt::Formatter<'_>, nanos: i64) -> fmt::Result {
    let nanos = nanos.rem_euclid(NANOS_PER_DAY);
    let (h, m) = (nanos / NANOS_PER_HOUR, nanos / NANOS_PER_MINUTE % 60);
    let (s, frac) = (nanos / NANOS_PER_SEC % 60, nanos % NANOS_PER_SEC);
    write!(f, "{h:02}:{m:02}:{s:02}")?;
    if frac != 0 {
        write!(f, ".{:09}", frac)?;
    }
    Ok(())
}

fn write_datetime(f: &mut fmt::Formatter<'_>, nanos: i64) -> fmt::Result {
    let days = nanos.div_euclid(NANOS_PER_DAY) as i32;
    let (y, m, d) = civil_from_days(days);
    write!(f, "{y:04}-{m:02}-{d:02}T")?;
    write_time(f, nanos.rem_euclid(NANOS_PER_DAY))
}

fn write_offset(f: &mut fmt::Formatter<'_>, offset: i16) -> fmt::Result {
    if offset == 0 {
        return write!(f, "Z");
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let (h, m) = (offset.abs() / 60, offset.abs() % 60);
    write!(f, "{sign}{h:02}:{m:02}")
}

/// A duration written the way ISO writes it, which is the way it parses
/// back. A zero duration is `PT0S`, because `P` alone is not a value.
fn write_duration(f: &mut fmt::Formatter<'_>, kind: DurationKind, count: i64) -> fmt::Result {
    if count < 0 {
        write!(f, "-")?;
    }
    let count = count.unsigned_abs();
    write!(f, "P")?;
    if kind == DurationKind::YearMonth {
        let (years, months) = (count / 12, count % 12);
        if years != 0 {
            write!(f, "{years}Y")?;
        }
        if months != 0 || years == 0 {
            write!(f, "{months}M")?;
        }
        return Ok(());
    }
    let nanos = count as i128;
    let day = NANOS_PER_DAY as i128;
    let days = nanos / day;
    let rest = (nanos % day) as i64;
    if days != 0 {
        write!(f, "{days}D")?;
    }
    if rest == 0 && days != 0 {
        return Ok(());
    }
    write!(f, "T")?;
    let (h, m) = (rest / NANOS_PER_HOUR, rest / NANOS_PER_MINUTE % 60);
    let (s, frac) = (rest / NANOS_PER_SEC % 60, rest % NANOS_PER_SEC);
    if h != 0 {
        write!(f, "{h}H")?;
    }
    if m != 0 {
        write!(f, "{m}M")?;
    }
    if s != 0 || frac != 0 || (h == 0 && m == 0) {
        write!(f, "{s}")?;
        if frac != 0 {
            write!(f, ".{:09}", frac)?;
        }
        write!(f, "S")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> Temporal {
        Temporal::parse(&LogicalType::Date, text).expect(text)
    }

    fn duration(text: &str) -> Temporal {
        Temporal::parse(&LogicalType::Duration(DurationKind::DayTime), text).expect(text)
    }

    #[test]
    fn a_date_is_a_day_count_and_prints_back_as_it_was_written() {
        assert_eq!(date("1970-01-01"), Temporal::Date(0));
        assert_eq!(date("2024-01-15").to_string(), "2024-01-15");
        assert_eq!(date("0001-01-01"), Temporal::Date(MIN_DAY));
        assert_eq!(date("9999-12-31"), Temporal::Date(MAX_DAY));
        // A day the month does not have is not a date, which a day
        // count cannot say on its own and the parser has to.
        assert_eq!(Temporal::parse(&LogicalType::Date, "2023-02-29"), None);
        assert_eq!(date("2024-02-29").to_string(), "2024-02-29");
        assert_eq!(Temporal::parse(&LogicalType::Date, "2024-13-01"), None);
    }

    #[test]
    fn a_round_trip_holds_across_the_whole_calendar() {
        let mut day = MIN_DAY;
        while day <= MAX_DAY {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day, "{y:04}-{m:02}-{d:02}");
            day += 97;
        }
    }

    #[test]
    fn a_time_takes_its_seconds_and_its_fraction_or_leaves_them_out() {
        let time = |t: &str| Temporal::parse(&LogicalType::LocalTime, t);
        assert_eq!(
            time("10:30"),
            Some(Temporal::LocalTime(37_800 * NANOS_PER_SEC))
        );
        assert_eq!(
            time("10:30:15.5").map(|t| t.to_string()),
            Some("10:30:15.500000000".into())
        );
        assert_eq!(time("24:00"), None);
        // A local type refuses a written zone rather than dropping it.
        assert_eq!(time("10:30+07:00"), None);
    }

    /// Two spellings of one instant are one number, which is the reason
    /// a zoned value stores UTC and the offset apart.
    #[test]
    fn a_zoned_datetime_stores_the_instant_and_remembers_the_offset() {
        let a = Temporal::parse(&LogicalType::ZonedDatetime, "2024-01-15T10:00:00+07:00").unwrap();
        let b = Temporal::parse(&LogicalType::ZonedDatetime, "2024-01-15T03:00:00Z").unwrap();
        let (Temporal::ZonedDatetime { nanos: x, .. }, Temporal::ZonedDatetime { nanos: y, .. }) =
            (a, b)
        else {
            panic!("both are zoned datetimes")
        };
        assert_eq!(x, y);
        assert_eq!(a.to_string(), "2024-01-15T10:00:00+07:00");
        assert_eq!(b.to_string(), "2024-01-15T03:00:00Z");
    }

    #[test]
    fn a_duration_is_one_kind_or_the_other_and_never_both() {
        assert_eq!(
            duration("P2D"),
            Temporal::Duration(DurationKind::DayTime, 2 * NANOS_PER_DAY)
        );
        assert_eq!(
            duration("P1Y2M"),
            Temporal::Duration(DurationKind::YearMonth, 14)
        );
        assert_eq!(
            duration("PT1H30M"),
            Temporal::Duration(DurationKind::DayTime, 90 * NANOS_PER_MINUTE)
        );
        assert_eq!(
            duration("PT0.5S"),
            Temporal::Duration(DurationKind::DayTime, NANOS_PER_SEC / 2)
        );
        assert_eq!(duration("P2D").to_string(), "P2D");
        assert_eq!(duration("P1Y2M").to_string(), "P1Y2M");
        assert_eq!(duration("PT1H30M").to_string(), "PT1H30M");
        // A month and a day in one value is the thing the two kinds
        // exist to prevent.
        assert!(Temporal::parse(&LogicalType::Duration(DurationKind::DayTime), "P1M1D").is_none());
        assert!(Temporal::parse(&LogicalType::Duration(DurationKind::DayTime), "P").is_none());
    }

    #[test]
    fn adding_months_clamps_to_the_length_of_the_month_it_lands_in() {
        let jan31 = days_from_civil(2024, 1, 31);
        assert_eq!(
            add_months(jan31, 1).map(civil_from_days),
            Some((2024, 2, 29))
        );
        assert_eq!(
            add_months(jan31, 13).map(civil_from_days),
            Some((2025, 2, 28))
        );
        assert_eq!(add_months(MAX_DAY, 1), None);
    }

    #[test]
    fn a_written_value_reads_back_without_being_told_its_type() {
        assert_eq!(Temporal::parse_any("2024-01-15"), Some(date("2024-01-15")));
        assert_eq!(Temporal::parse_any("P2D"), Some(duration("P2D")));
        assert!(matches!(
            Temporal::parse_any("2024-01-15T10:00:00Z"),
            Some(Temporal::ZonedDatetime { .. })
        ));
        assert!(matches!(
            Temporal::parse_any("2024-01-15T10:00:00"),
            Some(Temporal::LocalDatetime(_))
        ));
        assert!(matches!(
            Temporal::parse_any("10:00:00"),
            Some(Temporal::LocalTime(_))
        ));
        assert_eq!(Temporal::parse_any("nonsense"), None);
    }

    fn interval(text: &str, start: IntervalField, end: IntervalField) -> Option<Temporal> {
        Temporal::parse_sql_interval(
            text,
            &IntervalQualifier {
                start,
                end,
                leading: None,
                fraction: None,
            },
        )
    }

    #[test]
    fn a_sql_interval_is_read_by_the_fields_its_qualifier_names() {
        use IntervalField::{Day, Hour, Minute, Month, Second, Year};
        assert_eq!(
            interval("1-2", Year, Month),
            Some(Temporal::Duration(DurationKind::YearMonth, 14))
        );
        assert_eq!(
            interval("2", Month, Month),
            Some(Temporal::Duration(DurationKind::YearMonth, 2))
        );
        assert_eq!(
            interval("3 04:05:06", Day, Second),
            duration("P3DT4H5M6S").into()
        );
        assert_eq!(interval("10:30", Hour, Minute), duration("PT10H30M").into());
        assert_eq!(interval("1.5", Second, Second), duration("PT1.5S").into());
        // The same string under two qualifiers is two values, which is
        // the whole reason the qualifier is not optional.
        assert_eq!(interval("1", Day, Day), duration("P1D").into());
        assert_eq!(interval("1", Hour, Hour), duration("PT1H").into());
        // And a string the qualifier did not describe is nothing.
        assert_eq!(interval("1-2", Day, Day), None);
        assert_eq!(interval("3 04:05:06", Hour, Minute), None);
        assert_eq!(interval("", Day, Day), None);
        assert_eq!(interval("1x", Day, Day), None);
    }

    #[test]
    fn only_the_leading_field_of_an_interval_runs_past_its_own_size() {
        use IntervalField::{Day, Hour, Minute, Month, Second, Year};
        assert_eq!(interval("25", Hour, Hour), duration("PT25H").into());
        assert_eq!(interval("90", Minute, Minute), duration("PT1H30M").into());
        assert_eq!(
            interval("13", Month, Month),
            Some(Temporal::Duration(DurationKind::YearMonth, 13))
        );
        // A field with something written in front of it has the size
        // the calendar gives it, so these carry nothing and fail.
        assert_eq!(interval("1 25:00:00", Day, Second), None);
        assert_eq!(interval("1:60", Hour, Minute), None);
        assert_eq!(interval("1:60", Minute, Second), None);
        assert_eq!(interval("1-12", Year, Month), None);
    }

    #[test]
    fn an_interval_precision_bounds_the_digits_that_were_written() {
        use IntervalField::{Day, Second};
        let bound = |text: &str, leading, fraction| {
            Temporal::parse_sql_interval(
                text,
                &IntervalQualifier {
                    start: Day,
                    end: Day,
                    leading,
                    fraction,
                },
            )
            .is_some()
        };
        assert!(bound("100", Some(3), None));
        assert!(!bound("1000", Some(3), None));
        assert!(bound("1000", None, None));
        let seconds = |text: &str, fraction| {
            Temporal::parse_sql_interval(
                text,
                &IntervalQualifier {
                    start: Second,
                    end: Second,
                    leading: Some(2),
                    fraction,
                },
            )
            .is_some()
        };
        // The leading precision counts the digits in front of the
        // point and the fraction counts the ones behind it, so a
        // second written to three places fits `SECOND(2, 3)`.
        assert!(seconds("1.123", Some(3)));
        assert!(!seconds("1.1234", Some(3)));
        assert!(!seconds("100.1", Some(3)));
    }
}
