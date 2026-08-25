//! The two layer type model (gql/plan/02 sections 2 and 3).
//!
//! zu has had one notion of type and it is physical: `PropType { Str,
//! Int }` in storage and a ten arm `PhysType` in the vector layer. GQL
//! needs two, and conflating them is what makes an engine either slow
//! or wrong. A logical type is what the language talks about, with its
//! nullability and its declared width and its element types. A physical
//! type is what a vector holds, and it is a small closed set with no
//! nullability in it, because validity is a bitmap, and no precision in
//! it, because precision is a check and not a layout.
//!
//! The GV family is 52 of the standard's 228 optional features and
//! nothing else in the standard can be evaluated without it, which is
//! why it comes first. Nineteen of the 52 are the integer tower, and
//! they are one implementation with a width on it, so the family is
//! 23% of the conformance surface and closer to 6% of the work.
//!
//! This module is the lattice and the mapping. It holds no values and
//! reads no storage: a `LogicalType` is what a column or an expression
//! is declared to be, and [`LogicalType::physical`] is the only bridge
//! to the layer that holds bytes.

use std::fmt;

/// Width of an integer type, in bits.
///
/// The tower stops at 256 because the standard does, and every width is
/// a fixed stride, so the FOR and delta cascades in `zu-encoding` need
/// the width registered and nothing else.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum IntBits {
    B8,
    B16,
    B32,
    B64,
    B128,
    B256,
}

impl IntBits {
    /// The width in bits.
    pub fn bits(self) -> u16 {
        match self {
            IntBits::B8 => 8,
            IntBits::B16 => 16,
            IntBits::B32 => 32,
            IntBits::B64 => 64,
            IntBits::B128 => 128,
            IntBits::B256 => 256,
        }
    }

    /// Whether a 64 bit lane word is a value of an integer type this
    /// wide.
    ///
    /// The word is two's complement for a signed type and plain for an
    /// unsigned one, which is the reading the lane gives it, so this
    /// asks about the value rather than about the bits. A type 64 bits
    /// or wider holds every word there is, since a word is all a lane
    /// carries and the wide towers have no lane of their own.
    pub fn holds(self, word: u64, signed: bool) -> bool {
        let width = u32::from(self.bits());
        if width >= 64 {
            return true;
        }
        match signed {
            true => {
                let bound = 1i64 << (width - 1);
                (-bound..bound).contains(&(word as i64))
            }
            false => word < 1u64 << width,
        }
    }

    /// The narrowest width that holds `digits` decimal digits, which is
    /// what `INT(p)` (GV09) asks for. `None` when no width in the tower
    /// is wide enough.
    ///
    /// The bound is the count of decimal digits a width holds in full:
    /// an i64 reaches 9_223_372_036_854_775_807, so it holds every 18
    /// digit number and only some 19 digit ones, and a type that
    /// promised 19 would be promising a range it does not have.
    pub fn for_digits(digits: u16) -> Option<IntBits> {
        Some(match digits {
            0..=2 => IntBits::B8,
            3..=4 => IntBits::B16,
            5..=9 => IntBits::B32,
            10..=18 => IntBits::B64,
            19..=38 => IntBits::B128,
            39..=76 => IntBits::B256,
            _ => return None,
        })
    }
}

/// Width of a floating point type, in bits.
///
/// 16, 128 and 256 have no hardware anywhere zu runs. They are here
/// because the standard has them; they compute in software and the
/// benchmark matrix never uses them, which gql/plan/09 section 6 says
/// out loud so nobody reads a soft float number as an engine number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum FloatBits {
    B16,
    B32,
    B64,
    B128,
    B256,
}

impl FloatBits {
    /// The width in bits.
    pub fn bits(self) -> u16 {
        match self {
            FloatBits::B16 => 16,
            FloatBits::B32 => 32,
            FloatBits::B64 => 64,
            FloatBits::B128 => 128,
            FloatBits::B256 => 256,
        }
    }

    /// The narrowest binary width whose significand holds `digits`
    /// decimal digits, which is what `FLOAT(p)` (GV22) asks for.
    pub fn for_digits(digits: u16) -> Option<FloatBits> {
        Some(match digits {
            0..=3 => FloatBits::B16,
            4..=6 => FloatBits::B32,
            7..=15 => FloatBits::B64,
            16..=33 => FloatBits::B128,
            34..=71 => FloatBits::B256,
            _ => return None,
        })
    }
}

/// The two duration kinds, which do not mix.
///
/// A year-month duration plus a day-time duration is a type error and
/// not a normalisation, because a month is not a number of days. An
/// engine that stores months, days and nanoseconds in one value has to
/// invent an answer for "one month after 31 January" and the standard
/// does not ask it to. zu stores two types and refuses the arithmetic
/// that would need the invention.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum DurationKind {
    YearMonth,
    DayTime,
}

/// One field of a record type.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct Field {
    pub name: String,
    pub ty: LogicalType,
}

/// A record type: named fields, and whether fields outside the list are
/// allowed.
///
/// `open` is GV47 and closed is GV46, one boolean between them, and
/// GV48 (nested records) falls out of the type being recursive.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct RecordType {
    pub fields: Vec<Field>,
    pub open: bool,
}

impl RecordType {
    /// A closed record, the common case.
    pub fn closed(fields: Vec<Field>) -> Self {
        RecordType {
            fields,
            open: false,
        }
    }

    /// An open record, which may carry fields the type does not name.
    /// This is the one place in the type system where a per value map
    /// exists, and that map is the honest cost of GV47.
    pub fn open(fields: Vec<Field>) -> Self {
        RecordType { fields, open: true }
    }

    /// The declared field of this name, if the type names one.
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// What GQL talks about: the language facing type lattice.
///
/// Never present in a vector. A vector holds a [`PhysicalType`] and a
/// validity bitmap, and the logical type is what the binder knows about
/// the column, which is a different and larger thing: `INT(4) NOT NULL`
/// and `BIGINT` are two logical types over one physical layout.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub enum LogicalType {
    /// GV71. The type of the null literal, which every nullable type
    /// accepts and which carries no other value.
    Null,
    /// GV72. The empty type: no value has it. A column can never be
    /// declared with it, an expression can have it, and the binder
    /// refuses anything that would have to materialise one.
    Nothing,

    Bool,
    /// GV01 to GV19. `signed` and the width are the tower; `precision`
    /// is the declared decimal digit count of `INT(p)`, kept because it
    /// is a check the engine owes the user and not a layout.
    Int {
        signed: bool,
        bits: IntBits,
        precision: Option<u16>,
    },
    /// GV17. Exact decimal, stored in the narrowest integer width that
    /// holds `precision` digits and scaled by `scale`.
    Decimal {
        precision: u16,
        scale: u16,
    },
    /// GV20 to GV26.
    Float {
        bits: FloatBits,
        precision: Option<u16>,
    },
    /// GV30 to GV32. Lengths are in characters, and a character is a
    /// Unicode scalar value. `fixed` is GV32, the one length constraint
    /// that changes the layout rather than adding a check.
    Str {
        min: Option<u32>,
        max: Option<u32>,
        fixed: bool,
    },
    /// GV35 to GV38. Lengths are in octets.
    Bytes {
        min: Option<u32>,
        max: Option<u32>,
        fixed: bool,
    },

    /// GV39. Proleptic Gregorian, no zone.
    Date,
    /// GV39. Time of day with no zone and therefore no DST.
    LocalTime,
    /// GV39. An instant with no zone.
    LocalDatetime,
    /// GV40. Time of day with an offset from UTC in minutes.
    ZonedTime,
    /// GV40. An instant with an offset from UTC in minutes.
    ZonedDatetime,
    /// GV41.
    Duration(DurationKind),

    /// A node reference, optionally of a named node type.
    Node(Option<String>),
    /// An edge reference, optionally of a named edge type.
    Edge(Option<String>),
    /// GV60. A graph handle: a catalog id and a snapshot epoch, not
    /// data. Making it a value is what turns `USE` from a statement
    /// prefix into something composable.
    Graph(Option<String>),
    /// GV61. A binding table handle, with the record type of its rows
    /// when that is known.
    BindingTable(Option<RecordType>),

    /// GV55. A path, whose type describes the alternating node and edge
    /// types it walks.
    Path(Option<Box<PathType>>),
    /// GV50.
    List {
        elem: Box<LogicalType>,
        max: Option<u32>,
    },
    /// GV45 to GV48.
    Record(RecordType),

    /// GV65 and GV66. The open dynamic union: any type at all.
    Any,
    /// GV67. A closed dynamic union, `ANY<INT|STRING>`.
    Union(Vec<LogicalType>),
    /// GV68. Any type a property may hold, which is `Any` minus the
    /// reference and constructed types.
    AnyProperty,

    /// GV90, orthogonal to everything above. Nullability is a wrapper
    /// and not a flag on each variant, so no variant can forget it.
    Nullable(Box<LogicalType>),
}

/// The alternating node and edge types a path walks.
///
/// `nodes` is one longer than `edges` for a path type that describes a
/// fixed walk. A path type that describes a repeated segment is left to
/// the language surface in G6, which is where the syntax for it lands.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct PathType {
    pub nodes: Vec<Option<String>>,
    pub edges: Vec<Option<String>>,
}

/// What a vector holds. Fifteen layouts, each one either a fixed stride
/// or an offsets buffer plus a child, so every kernel in perf/02 and
/// every SIMD tier in perf/11 can address them, and so Parquet and
/// Arrow ingest map onto them without a transcode.
///
/// No nullability: validity is a separate bitmap and null is never a
/// value in a vector. No precision: that is a check the logical type
/// carries.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum PhysicalType {
    /// One bit per row, packed in words.
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    /// 16 byte stride. Serves the 128 bit integers and decimals up to
    /// 38 digits.
    I128,
    /// 32 byte stride. Serves the 256 bit integers and decimals up to
    /// 76 digits.
    I256,
    F32,
    F64,
    /// 16 byte stride, computed in software.
    F128,
    /// Offsets plus an arena.
    Str,
    /// Offsets plus an arena, compared by octet and never dictionary
    /// encoded.
    Bytes,
    /// i32 stride: days since 1970-01-01.
    Days32,
    /// i64 stride: nanoseconds, since midnight for a time and since the
    /// epoch for a datetime, and the whole of a day-time duration.
    Nanos64,
    /// Two buffers: i64 nanoseconds and an i16 offset in minutes.
    Zoned,
    /// i32 stride: whole months, and nothing else, which is what makes
    /// the two duration kinds refuse to mix.
    Months32,
    /// Offsets plus a child vector.
    ListOffsets,
    /// Child vectors and no bytes of its own.
    Struct,
}

impl PhysicalType {
    /// Bytes per value for the fixed stride layouts.
    ///
    /// `None` for `Bool`, which is bit packed, and for the four layouts
    /// that are offsets or children rather than a stride. A caller that
    /// wants "how much space does a row take" cannot get an answer from
    /// those without the data, and returning a number would be a lie
    /// rather than an approximation.
    pub fn stride(self) -> Option<usize> {
        Some(match self {
            PhysicalType::Bool => return None,
            PhysicalType::I8 | PhysicalType::U8 => 1,
            PhysicalType::I16 | PhysicalType::U16 | PhysicalType::Months32 => 2,
            PhysicalType::I32 | PhysicalType::U32 | PhysicalType::F32 | PhysicalType::Days32 => 4,
            PhysicalType::I64 | PhysicalType::U64 | PhysicalType::F64 | PhysicalType::Nanos64 => 8,
            PhysicalType::I128 | PhysicalType::F128 => 16,
            PhysicalType::I256 => 32,
            PhysicalType::Str
            | PhysicalType::Bytes
            | PhysicalType::Zoned
            | PhysicalType::ListOffsets
            | PhysicalType::Struct => return None,
        })
    }
}

impl LogicalType {
    /// A nullable version of this type, without stacking wrappers.
    pub fn nullable(self) -> LogicalType {
        match self {
            LogicalType::Nullable(_) => self,
            other => LogicalType::Nullable(Box::new(other)),
        }
    }

    /// Whether a value of this type may be null. `NULL` itself is
    /// nullable and nothing else is unless it says so, which is the
    /// GV90 reading: a declared type is not null until wrapped.
    pub fn is_nullable(&self) -> bool {
        matches!(self, LogicalType::Nullable(_) | LogicalType::Null)
    }

    /// This type with any nullability wrapper removed, which is the
    /// form every layout question is asked about.
    pub fn base(&self) -> &LogicalType {
        match self {
            LogicalType::Nullable(inner) => inner.base(),
            other => other,
        }
    }

    /// Whether this type may hold a value at all. The immaterial types
    /// (GV71, GV72) cannot: `NOTHING` has no values and `NULL` has one
    /// that is never stored in a vector.
    pub fn is_material(&self) -> bool {
        !matches!(self.base(), LogicalType::Null | LogicalType::Nothing)
    }

    /// The layout a vector of this type uses, or `None` when there is
    /// nothing to lay out.
    ///
    /// The dynamic unions map to `Struct`, which holds a type code
    /// child and one child per member. That is the one construct in the
    /// system that puts a tag back next to each value, and it is
    /// confined to columns declared as unions, which in practice is the
    /// schemaless ingest path and nothing else.
    pub fn physical(&self) -> Option<PhysicalType> {
        Some(match self.base() {
            LogicalType::Null | LogicalType::Nothing => return None,
            LogicalType::Bool => PhysicalType::Bool,
            LogicalType::Int { signed, bits, .. } => match (signed, bits) {
                (true, IntBits::B8) => PhysicalType::I8,
                (true, IntBits::B16) => PhysicalType::I16,
                (true, IntBits::B32) => PhysicalType::I32,
                (true, IntBits::B64) => PhysicalType::I64,
                (false, IntBits::B8) => PhysicalType::U8,
                (false, IntBits::B16) => PhysicalType::U16,
                (false, IntBits::B32) => PhysicalType::U32,
                (false, IntBits::B64) => PhysicalType::U64,
                // 128 and 256 have one stride each and carry their sign
                // in the value, because there is no hardware difference
                // to gain from two layouts at these widths.
                (_, IntBits::B128) => PhysicalType::I128,
                (_, IntBits::B256) => PhysicalType::I256,
            },
            LogicalType::Decimal { precision, .. } => {
                match IntBits::for_digits(*precision).unwrap_or(IntBits::B256) {
                    IntBits::B8 | IntBits::B16 | IntBits::B32 => PhysicalType::I32,
                    IntBits::B64 => PhysicalType::I64,
                    IntBits::B128 => PhysicalType::I128,
                    IntBits::B256 => PhysicalType::I256,
                }
            }
            LogicalType::Float { bits, .. } => match bits {
                // A 16 bit float stores as f32 and computes as f32,
                // which is what every SIMD unit does with one anyway.
                FloatBits::B16 | FloatBits::B32 => PhysicalType::F32,
                FloatBits::B64 => PhysicalType::F64,
                FloatBits::B128 | FloatBits::B256 => PhysicalType::F128,
            },
            LogicalType::Str { .. } => PhysicalType::Str,
            LogicalType::Bytes { .. } => PhysicalType::Bytes,
            LogicalType::Date => PhysicalType::Days32,
            LogicalType::LocalTime | LogicalType::LocalDatetime => PhysicalType::Nanos64,
            LogicalType::ZonedTime | LogicalType::ZonedDatetime => PhysicalType::Zoned,
            LogicalType::Duration(DurationKind::YearMonth) => PhysicalType::Months32,
            LogicalType::Duration(DurationKind::DayTime) => PhysicalType::Nanos64,
            // References are one word each: a node or edge row id, and
            // a handle for the two catalog references.
            LogicalType::Node(_)
            | LogicalType::Edge(_)
            | LogicalType::Graph(_)
            | LogicalType::BindingTable(_) => PhysicalType::U64,
            LogicalType::Path(_) | LogicalType::Record(_) => PhysicalType::Struct,
            LogicalType::List { .. } => PhysicalType::ListOffsets,
            LogicalType::Any | LogicalType::Union(_) | LogicalType::AnyProperty => {
                PhysicalType::Struct
            }
            LogicalType::Nullable(_) => unreachable!("base() removed the wrapper"),
        })
    }

    /// The common integer spellings, so a caller does not write the
    /// struct out to say `BIGINT`.
    pub fn int(bits: IntBits) -> LogicalType {
        LogicalType::Int {
            signed: true,
            bits,
            precision: None,
        }
    }

    pub fn uint(bits: IntBits) -> LogicalType {
        LogicalType::Int {
            signed: false,
            bits,
            precision: None,
        }
    }

    pub fn float(bits: FloatBits) -> LogicalType {
        LogicalType::Float {
            bits,
            precision: None,
        }
    }

    /// A string with no length constraint.
    pub fn string() -> LogicalType {
        LogicalType::Str {
            min: None,
            max: None,
            fixed: false,
        }
    }

    /// A byte string with no length constraint.
    pub fn bytes() -> LogicalType {
        LogicalType::Bytes {
            min: None,
            max: None,
            fixed: false,
        }
    }
}

/// Resolves a type name, with its synonyms and its parenthesised
/// arguments, to a logical type.
///
/// Synonym resolution is a table and not a parser branch, because the
/// standard's spellings are a list and not a grammar: `SMALLINT`,
/// `INT`, `BIGINT`, `SIGNED`, `UNSIGNED`, `REAL`, `DOUBLE PRECISION`
/// and the rest name types the tower already has. The parenthesised
/// argument is the only structure here: `INT(p)` and `FLOAT(p)` are
/// decimal digit counts, the string and byte lengths are counts of
/// characters and octets, and `DECIMAL(p,s)` takes both.
///
/// The name is matched case insensitively and internal whitespace is
/// collapsed, so `double   precision` and `DOUBLE PRECISION` are one
/// name. A trailing `NOT NULL` is not handled here: nullability is a
/// wrapper the binder applies, and letting it into the name table would
/// make every entry two entries.
pub fn type_by_name(name: &str) -> Option<LogicalType> {
    let (head, args) = split_args(name)?;
    let head = head.split_whitespace().collect::<Vec<_>>().join(" ");
    let head = head.to_ascii_uppercase();
    let arg = |i: usize| args.get(i).copied();
    Some(match (head.as_str(), args.len()) {
        ("BOOL" | "BOOLEAN", 0) => LogicalType::Bool,

        ("INT8" | "TINYINT", 0) => LogicalType::int(IntBits::B8),
        ("INT16" | "SMALLINT", 0) => LogicalType::int(IntBits::B16),
        ("INT32", 0) => LogicalType::int(IntBits::B32),
        ("INT64" | "BIGINT" | "SIGNED", 0) => LogicalType::int(IntBits::B64),
        ("INT128", 0) => LogicalType::int(IntBits::B128),
        ("INT256", 0) => LogicalType::int(IntBits::B256),
        // The standard's bare INT is the implementation's regular
        // width, and zu's row ids, degrees and counts are all 64 bit,
        // so a narrower default would surprise every user of it.
        ("INT" | "INTEGER", 0) => LogicalType::int(IntBits::B64),

        ("UINT8", 0) => LogicalType::uint(IntBits::B8),
        ("UINT16" | "USMALLINT", 0) => LogicalType::uint(IntBits::B16),
        ("UINT32", 0) => LogicalType::uint(IntBits::B32),
        ("UINT64" | "UBIGINT" | "UNSIGNED", 0) => LogicalType::uint(IntBits::B64),
        ("UINT128", 0) => LogicalType::uint(IntBits::B128),
        ("UINT256", 0) => LogicalType::uint(IntBits::B256),
        ("UINT" | "UINTEGER", 0) => LogicalType::uint(IntBits::B64),

        // GV09 and GV22: a declared decimal digit count, kept on the
        // type as a check and widened to the narrowest layout holding
        // it. A count no width holds is not a type.
        ("INT" | "INTEGER", 1) => LogicalType::Int {
            signed: true,
            bits: IntBits::for_digits(arg(0)?)?,
            precision: Some(arg(0)?),
        },
        ("UINT" | "UINTEGER", 1) => LogicalType::Int {
            signed: false,
            bits: IntBits::for_digits(arg(0)?)?,
            precision: Some(arg(0)?),
        },
        ("FLOAT", 1) => LogicalType::Float {
            bits: FloatBits::for_digits(arg(0)?)?,
            precision: Some(arg(0)?),
        },

        ("FLOAT16" | "HALF", 0) => LogicalType::float(FloatBits::B16),
        ("FLOAT32" | "REAL", 0) => LogicalType::float(FloatBits::B32),
        ("FLOAT64" | "DOUBLE" | "DOUBLE PRECISION" | "FLOAT", 0) => {
            LogicalType::float(FloatBits::B64)
        }
        ("FLOAT128", 0) => LogicalType::float(FloatBits::B128),
        ("FLOAT256", 0) => LogicalType::float(FloatBits::B256),

        ("DECIMAL" | "DEC", 1) => LogicalType::Decimal {
            precision: arg(0)?,
            scale: 0,
        },
        ("DECIMAL" | "DEC", 2) => LogicalType::Decimal {
            precision: arg(0)?,
            scale: arg(1)?,
        },

        ("STRING" | "VARCHAR", 0) => LogicalType::string(),
        ("STRING" | "VARCHAR", 1) => LogicalType::Str {
            min: None,
            max: Some(u32::from(arg(0)?)),
            fixed: false,
        },
        ("STRING" | "VARCHAR", 2) => LogicalType::Str {
            min: Some(u32::from(arg(0)?)),
            max: Some(u32::from(arg(1)?)),
            fixed: false,
        },
        ("CHAR", 1) => LogicalType::Str {
            min: Some(u32::from(arg(0)?)),
            max: Some(u32::from(arg(0)?)),
            fixed: true,
        },
        ("BYTES" | "VARBINARY", 0) => LogicalType::bytes(),
        ("BYTES" | "VARBINARY", 1) => LogicalType::Bytes {
            min: None,
            max: Some(u32::from(arg(0)?)),
            fixed: false,
        },
        ("BINARY", 1) => LogicalType::Bytes {
            min: Some(u32::from(arg(0)?)),
            max: Some(u32::from(arg(0)?)),
            fixed: true,
        },

        ("DATE", 0) => LogicalType::Date,
        ("LOCAL TIME" | "TIME WITHOUT TIME ZONE", 0) => LogicalType::LocalTime,
        ("LOCAL DATETIME" | "TIMESTAMP WITHOUT TIME ZONE", 0) => LogicalType::LocalDatetime,
        ("ZONED TIME" | "TIME WITH TIME ZONE" | "TIME", 0) => LogicalType::ZonedTime,
        ("ZONED DATETIME" | "TIMESTAMP WITH TIME ZONE" | "TIMESTAMP", 0) => {
            LogicalType::ZonedDatetime
        }
        ("DURATION" | "INTERVAL", 0) => LogicalType::Duration(DurationKind::DayTime),
        ("YEAR MONTH DURATION", 0) => LogicalType::Duration(DurationKind::YearMonth),
        ("DAY TIME DURATION", 0) => LogicalType::Duration(DurationKind::DayTime),

        ("NULL", 0) => LogicalType::Null,
        ("NOTHING", 0) => LogicalType::Nothing,
        ("ANY", 0) => LogicalType::Any,
        ("ANY PROPERTY VALUE" | "PROPERTY VALUE", 0) => LogicalType::AnyProperty,

        ("NODE" | "VERTEX", 0) => LogicalType::Node(None),
        ("EDGE" | "RELATIONSHIP", 0) => LogicalType::Edge(None),
        ("GRAPH", 0) => LogicalType::Graph(None),
        ("BINDING TABLE" | "TABLE", 0) => LogicalType::BindingTable(None),
        ("PATH", 0) => LogicalType::Path(None),

        _ => return None,
    })
}

/// Splits `INT(4)` into `("INT", [4])` and `INT` into `("INT", [])`.
/// Returns `None` when the parentheses do not close or an argument is
/// not a number, so a caller gets one answer for "not a type name" and
/// not two.
fn split_args(name: &str) -> Option<(&str, Vec<u16>)> {
    let name = name.trim();
    let Some(open) = name.find('(') else {
        return Some((name, Vec::new()));
    };
    let rest = name.strip_suffix(')')?;
    let (head, args) = rest.split_at(open);
    let args = args
        .trim_start_matches('(')
        .split(',')
        .map(|a| a.trim().parse().ok())
        .collect::<Option<Vec<u16>>>()?;
    if args.is_empty() {
        return None;
    }
    Some((head, args))
}

impl fmt::Display for LogicalType {
    /// The spelling that goes in a diagnostic record, so the two type
    /// names in a `22G0x` say what the user wrote rather than what the
    /// enum looks like.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogicalType::Null => write!(f, "NULL"),
            LogicalType::Nothing => write!(f, "NOTHING"),
            LogicalType::Bool => write!(f, "BOOL"),
            LogicalType::Int {
                signed,
                bits,
                precision,
            } => {
                let u = if *signed { "" } else { "U" };
                match precision {
                    Some(p) => write!(f, "{u}INT({p})"),
                    None => write!(f, "{u}INT{}", bits.bits()),
                }
            }
            LogicalType::Decimal { precision, scale } => write!(f, "DECIMAL({precision},{scale})"),
            LogicalType::Float { bits, precision } => match precision {
                Some(p) => write!(f, "FLOAT({p})"),
                None => write!(f, "FLOAT{}", bits.bits()),
            },
            LogicalType::Str { min, max, fixed } => write_len(f, "STRING", *min, *max, *fixed),
            LogicalType::Bytes { min, max, fixed } => write_len(f, "BYTES", *min, *max, *fixed),
            LogicalType::Date => write!(f, "DATE"),
            LogicalType::LocalTime => write!(f, "LOCAL TIME"),
            LogicalType::LocalDatetime => write!(f, "LOCAL DATETIME"),
            LogicalType::ZonedTime => write!(f, "ZONED TIME"),
            LogicalType::ZonedDatetime => write!(f, "ZONED DATETIME"),
            LogicalType::Duration(DurationKind::YearMonth) => write!(f, "YEAR MONTH DURATION"),
            LogicalType::Duration(DurationKind::DayTime) => write!(f, "DAY TIME DURATION"),
            LogicalType::Node(t) => write_ref(f, "NODE", t.as_deref()),
            LogicalType::Edge(t) => write_ref(f, "EDGE", t.as_deref()),
            LogicalType::Graph(t) => write_ref(f, "GRAPH", t.as_deref()),
            LogicalType::BindingTable(_) => write!(f, "BINDING TABLE"),
            LogicalType::Path(_) => write!(f, "PATH"),
            LogicalType::List { elem, max } => match max {
                Some(n) => write!(f, "LIST<{elem}>[{n}]"),
                None => write!(f, "LIST<{elem}>"),
            },
            LogicalType::Record(r) => {
                write!(f, "RECORD<")?;
                for (i, field) in r.fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", field.name, field.ty)?;
                }
                if r.open {
                    write!(f, "{}..", if r.fields.is_empty() { "" } else { ", " })?;
                }
                write!(f, ">")
            }
            LogicalType::Any => write!(f, "ANY"),
            LogicalType::Union(members) => {
                write!(f, "ANY<")?;
                for (i, m) in members.iter().enumerate() {
                    if i > 0 {
                        write!(f, "|")?;
                    }
                    write!(f, "{m}")?;
                }
                write!(f, ">")
            }
            LogicalType::AnyProperty => write!(f, "ANY PROPERTY VALUE"),
            LogicalType::Nullable(inner) => write!(f, "{inner}?"),
        }
    }
}

fn write_len(
    f: &mut fmt::Formatter<'_>,
    name: &str,
    min: Option<u32>,
    max: Option<u32>,
    fixed: bool,
) -> fmt::Result {
    match (min, max, fixed) {
        (Some(n), _, true) => write!(f, "{name}({n}) FIXED"),
        (Some(lo), Some(hi), false) => write!(f, "{name}({lo}..{hi})"),
        (None, Some(hi), false) => write!(f, "{name}({hi})"),
        _ => write!(f, "{name}"),
    }
}

fn write_ref(f: &mut fmt::Formatter<'_>, name: &str, ty: Option<&str>) -> fmt::Result {
    match ty {
        Some(t) => write!(f, "{name}<{t}>"),
        None => write!(f, "{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_integer_tower_is_one_implementation_with_a_width_on_it() {
        for (bits, signed, want) in [
            (IntBits::B8, true, PhysicalType::I8),
            (IntBits::B16, true, PhysicalType::I16),
            (IntBits::B32, true, PhysicalType::I32),
            (IntBits::B64, true, PhysicalType::I64),
            (IntBits::B8, false, PhysicalType::U8),
            (IntBits::B64, false, PhysicalType::U64),
            (IntBits::B128, true, PhysicalType::I128),
            (IntBits::B256, false, PhysicalType::I256),
        ] {
            let ty = LogicalType::Int {
                signed,
                bits,
                precision: None,
            };
            assert_eq!(ty.physical(), Some(want), "{ty}");
        }
    }

    #[test]
    fn a_declared_digit_count_picks_the_narrowest_width_that_holds_it() {
        assert_eq!(IntBits::for_digits(1), Some(IntBits::B8));
        assert_eq!(IntBits::for_digits(4), Some(IntBits::B16));
        assert_eq!(IntBits::for_digits(9), Some(IntBits::B32));
        // 19 digits do not fit an i64 in full, so INT(19) is a 128.
        assert_eq!(IntBits::for_digits(18), Some(IntBits::B64));
        assert_eq!(IntBits::for_digits(19), Some(IntBits::B128));
        assert_eq!(IntBits::for_digits(76), Some(IntBits::B256));
        assert_eq!(IntBits::for_digits(77), None);
    }

    #[test]
    fn the_two_duration_kinds_are_two_types_and_two_layouts() {
        let ym = LogicalType::Duration(DurationKind::YearMonth);
        let dt = LogicalType::Duration(DurationKind::DayTime);
        assert_ne!(ym, dt);
        assert_eq!(ym.physical(), Some(PhysicalType::Months32));
        assert_eq!(dt.physical(), Some(PhysicalType::Nanos64));
    }

    #[test]
    fn the_immaterial_types_have_no_layout() {
        assert_eq!(LogicalType::Null.physical(), None);
        assert_eq!(LogicalType::Nothing.physical(), None);
        assert!(!LogicalType::Nothing.is_material());
        assert!(!LogicalType::Null.clone().nullable().is_material());
        assert!(LogicalType::Bool.is_material());
    }

    #[test]
    fn nullability_is_a_wrapper_that_does_not_stack_or_change_the_layout() {
        let ty = LogicalType::int(IntBits::B32);
        let once = ty.clone().nullable();
        let twice = once.clone().nullable();
        assert_eq!(once, twice);
        assert!(once.is_nullable());
        assert!(!ty.is_nullable());
        assert_eq!(once.base(), &ty);
        assert_eq!(once.physical(), ty.physical());
    }

    #[test]
    fn every_temporal_type_has_the_layout_the_plan_names() {
        for (ty, want) in [
            (LogicalType::Date, PhysicalType::Days32),
            (LogicalType::LocalTime, PhysicalType::Nanos64),
            (LogicalType::LocalDatetime, PhysicalType::Nanos64),
            (LogicalType::ZonedTime, PhysicalType::Zoned),
            (LogicalType::ZonedDatetime, PhysicalType::Zoned),
        ] {
            assert_eq!(ty.physical(), Some(want), "{ty}");
        }
    }

    #[test]
    fn a_fixed_stride_knows_its_width_and_the_others_admit_they_do_not() {
        assert_eq!(PhysicalType::I8.stride(), Some(1));
        assert_eq!(PhysicalType::Months32.stride(), Some(2));
        assert_eq!(PhysicalType::Days32.stride(), Some(4));
        assert_eq!(PhysicalType::Nanos64.stride(), Some(8));
        assert_eq!(PhysicalType::I128.stride(), Some(16));
        assert_eq!(PhysicalType::I256.stride(), Some(32));
        assert_eq!(PhysicalType::Bool.stride(), None);
        assert_eq!(PhysicalType::Str.stride(), None);
        assert_eq!(PhysicalType::Zoned.stride(), None);
        assert_eq!(PhysicalType::Struct.stride(), None);
    }

    #[test]
    fn the_spellings_resolve_to_the_types_they_name() {
        for (name, want) in [
            ("BOOL", LogicalType::Bool),
            ("boolean", LogicalType::Bool),
            ("SMALLINT", LogicalType::int(IntBits::B16)),
            ("BIGINT", LogicalType::int(IntBits::B64)),
            ("int", LogicalType::int(IntBits::B64)),
            ("UNSIGNED", LogicalType::uint(IntBits::B64)),
            ("REAL", LogicalType::float(FloatBits::B32)),
            ("double precision", LogicalType::float(FloatBits::B64)),
            ("DOUBLE   PRECISION", LogicalType::float(FloatBits::B64)),
            ("FLOAT", LogicalType::float(FloatBits::B64)),
            ("STRING", LogicalType::string()),
            ("DATE", LogicalType::Date),
            ("zoned datetime", LogicalType::ZonedDatetime),
            (
                "year month duration",
                LogicalType::Duration(DurationKind::YearMonth),
            ),
            ("ANY", LogicalType::Any),
            ("any property value", LogicalType::AnyProperty),
        ] {
            assert_eq!(type_by_name(name), Some(want), "{name}");
        }
        assert_eq!(type_by_name("QUUX"), None);
        assert_eq!(type_by_name("INT("), None);
        assert_eq!(type_by_name("INT()"), None);
        assert_eq!(type_by_name("INT(x)"), None);
    }

    #[test]
    fn a_parenthesised_argument_is_a_count_and_not_a_width() {
        assert_eq!(
            type_by_name("INT(4)"),
            Some(LogicalType::Int {
                signed: true,
                bits: IntBits::B16,
                precision: Some(4),
            })
        );
        // The declared digit count survives on the type, because the
        // range check it asks for is narrower than the layout.
        assert_eq!(
            type_by_name("INT(4)").unwrap().physical(),
            type_by_name("INT16").unwrap().physical()
        );
        assert_ne!(type_by_name("INT(4)"), type_by_name("INT16"));
        assert_eq!(type_by_name("INT(100)"), None);
        assert_eq!(
            type_by_name("DECIMAL(20,4)"),
            Some(LogicalType::Decimal {
                precision: 20,
                scale: 4,
            })
        );
        assert_eq!(
            type_by_name("DECIMAL(20,4)").unwrap().physical(),
            Some(PhysicalType::I128)
        );
        assert_eq!(
            type_by_name("STRING(1,5)"),
            Some(LogicalType::Str {
                min: Some(1),
                max: Some(5),
                fixed: false,
            })
        );
        assert_eq!(
            type_by_name("CHAR(3)"),
            Some(LogicalType::Str {
                min: Some(3),
                max: Some(3),
                fixed: true,
            })
        );
    }

    #[test]
    fn a_record_carries_open_closed_and_nesting_in_one_type() {
        let inner = RecordType::closed(vec![Field {
            name: "since".into(),
            ty: LogicalType::Date,
        }]);
        let outer = RecordType::open(vec![
            Field {
                name: "name".into(),
                ty: LogicalType::string(),
            },
            Field {
                name: "meta".into(),
                ty: LogicalType::Record(inner),
            },
        ]);
        assert!(outer.open);
        assert_eq!(
            outer.field("name").map(|f| &f.ty),
            Some(&LogicalType::string())
        );
        assert_eq!(outer.field("nope"), None);
        assert_eq!(
            LogicalType::Record(outer).physical(),
            Some(PhysicalType::Struct)
        );
    }

    #[test]
    fn a_type_prints_the_way_a_diagnostic_needs_to_name_it() {
        assert_eq!(LogicalType::int(IntBits::B64).to_string(), "INT64");
        assert_eq!(type_by_name("INT(4)").unwrap().to_string(), "INT(4)");
        assert_eq!(type_by_name("UNSIGNED").unwrap().to_string(), "UINT64");
        assert_eq!(
            type_by_name("CHAR(3)").unwrap().to_string(),
            "STRING(3) FIXED"
        );
        assert_eq!(
            type_by_name("STRING(1,5)").unwrap().to_string(),
            "STRING(1..5)"
        );
        assert_eq!(LogicalType::string().nullable().to_string(), "STRING?");
        assert_eq!(
            LogicalType::List {
                elem: Box::new(LogicalType::Date),
                max: Some(4),
            }
            .to_string(),
            "LIST<DATE>[4]"
        );
        assert_eq!(
            LogicalType::Union(vec![LogicalType::int(IntBits::B64), LogicalType::string()])
                .to_string(),
            "ANY<INT64|STRING>"
        );
        assert_eq!(
            LogicalType::Record(RecordType::open(vec![Field {
                name: "n".into(),
                ty: LogicalType::Bool,
            }]))
            .to_string(),
            "RECORD<n: BOOL, ..>"
        );
        assert_eq!(
            LogicalType::Node(Some("person".into())).to_string(),
            "NODE<person>"
        );
    }

    #[test]
    fn every_name_the_table_resolves_prints_back_to_a_name_it_resolves() {
        // Not that the spelling round trips, since synonyms collapse,
        // but that what a diagnostic prints is a name the engine
        // accepts. A type whose printed form is not a type name is a
        // message the user cannot act on.
        for name in [
            "BOOL",
            "SMALLINT",
            "BIGINT",
            "UNSIGNED",
            "REAL",
            "DOUBLE PRECISION",
            "STRING",
            "BYTES",
            "DATE",
            "LOCAL TIME",
            "LOCAL DATETIME",
            "ZONED TIME",
            "ZONED DATETIME",
            "DURATION",
            "YEAR MONTH DURATION",
            "NULL",
            "NOTHING",
            "ANY",
            "ANY PROPERTY VALUE",
            "NODE",
            "EDGE",
            "GRAPH",
            "BINDING TABLE",
            "PATH",
            "INT(4)",
            "FLOAT(3)",
        ] {
            let ty = type_by_name(name).unwrap_or_else(|| panic!("{name} is not a type"));
            let printed = ty.to_string();
            assert_eq!(
                type_by_name(&printed),
                Some(ty.clone()),
                "{name} prints as {printed}, which does not resolve back"
            );
        }
    }
}
