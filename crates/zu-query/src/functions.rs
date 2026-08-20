//! The builtin function registry (ISO 20, Spec/2064g/gql plan/05 §1.1).
//!
//! One table says what each builtin function is called, what it takes,
//! what it answers, whether it folds a group or answers a row, and
//! whether two calls on the same arguments answer the same thing. The
//! binder resolves a written name against that table and checks the
//! call against the signature it found, so a function is added by
//! writing a row here rather than by writing its name into a match in
//! the binder, another in the printer and a third in the evaluator.
//!
//! A scalar function carries its kernel too, which is the code that
//! answers it. The binder settles which row answers a call while it
//! binds and writes that row's number onto the call, so the evaluator
//! reads the kernel at a number and calls through the pointer rather
//! than spending a row deciding which function it is looking at or
//! walking the table to find out. A call whose
//! arguments are all literals does not reach a row at all: a
//! deterministic kernel answers the same thing every time, so the
//! binder runs it once and keeps the answer.

use zu_common::gqlstatus::codes;
use zu_common::unicode::NormalForm;
use zu_common::{Result, ZuError, unicode};

use crate::ast::{DatetimeFn, Literal};
use crate::binder::{BoundExpr, Cut, Func, Math, Trim, Type};
use crate::cast;
use crate::exec::{Value, settle};

/// The code behind a scalar function: the arguments already evaluated,
/// one answer out. It takes the function it was resolved for because
/// two of them carry a normal form that the name alone does not say.
pub type Kernel = fn(func: Func, args: &[Value]) -> Result<Value>;

/// What a signature accepts in an argument position. It is a class of
/// types rather than one type, because a function that takes a number
/// takes either of the two, and the ones over elements take a node or
/// an edge alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Anything at all, which is what the comparing aggregates take.
    Any,
    /// An integer or a float.
    Number,
    /// A character string.
    Str,
    /// A list, and not a string that could be read as one.
    List,
    /// A list, a string or a path: the three things that have a count
    /// of elements, which is what `SIZE` answers.
    Sized,
    /// A path.
    Path,
    /// A node or an edge.
    Element,
    /// A string first and a count of characters after it, which is what
    /// the substring function takes and the one kind here whose
    /// positions are not alike.
    Counted,
}

impl Kind {
    /// Whether an argument of this type is one this kind accepts,
    /// wherever it was written. `ANY` is accepted everywhere, because it
    /// is the type of a value nobody has narrowed yet and refusing it
    /// would refuse a property read.
    pub fn accepts(self, ty: &Type) -> bool {
        match self {
            Kind::Any => true,
            Kind::Number => matches!(ty, Type::Any | Type::Int | Type::Float),
            Kind::Str => matches!(ty, Type::Any | Type::Str),
            Kind::List => matches!(ty, Type::Any | Type::List(_)),
            Kind::Sized => matches!(ty, Type::Any | Type::List(_) | Type::Str | Type::Path),
            Kind::Path => matches!(ty, Type::Any | Type::Path),
            Kind::Element => matches!(ty, Type::Any | Type::Node | Type::Rel),
            Kind::Counted => Kind::Str.accepts(ty) || Kind::Number.accepts(ty),
        }
    }

    /// Whether an argument of this type is one this kind accepts in the
    /// position it was written in. Only [`Kind::Counted`] reads the
    /// position: every other kind takes the same thing everywhere, so
    /// for those this is [`Kind::accepts`] and nothing more.
    pub fn accepts_at(self, at: usize, ty: &Type) -> bool {
        match (self, at) {
            (Kind::Counted, 0) => Kind::Str.accepts(ty),
            (Kind::Counted, _) => Kind::Number.accepts(ty),
            _ => self.accepts(ty),
        }
    }
}

/// How many arguments a call may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// Exactly this many.
    Exactly(usize),
    /// This many or more, which is what the element predicates take
    /// since they compare a list of elements against each other.
    AtLeast(usize),
    /// This many at the fewest and that many at the most, which is what
    /// `ROUND` takes: the digits to round to are a second argument and
    /// nought when none is written.
    Between(usize, usize),
}

impl Arity {
    /// The fewest arguments a call may write, which is the count the
    /// tests hand a kernel when they ask what it does with a null.
    pub fn least(self) -> usize {
        match self {
            Arity::Exactly(n) | Arity::AtLeast(n) | Arity::Between(n, _) => n,
        }
    }
}

/// What the answer's type is. It is written as a rule rather than as a
/// type because half of these answer whatever they were handed: `MIN`
/// of integers is an integer and `MIN` of floats is a float.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ret {
    Int,
    Float,
    Str,
    Bool,
    /// A list of whatever the elements turn out to be, which is what a
    /// path's element list is: nodes and edges together, and no
    /// narrower type covers both.
    ListOfAny,
    /// The type of the first argument.
    Same,
    /// A list of the type of the first argument.
    ListOf,
    /// The wider of the arguments: an integer where every argument is
    /// one and a float where any of them is, which is the rule the
    /// arithmetic operators follow and so is the rule `MOD` follows.
    Wider,
}

impl Ret {
    /// The answer's type, given the types of the arguments. Most of
    /// these answer one type whatever arrived; the rest read the first
    /// argument, or all of them.
    pub fn of(self, args: &[Type]) -> Type {
        let first = || args.first().cloned().unwrap_or(Type::Any);
        match self {
            Ret::Int => Type::Int,
            Ret::Float => Type::Float,
            Ret::Str => Type::Str,
            Ret::Bool => Type::Bool,
            Ret::ListOfAny => Type::List(Box::new(Type::Any)),
            Ret::Same => first(),
            Ret::ListOf => Type::List(Box::new(first())),
            Ret::Wider => {
                if args.iter().any(|ty| matches!(ty, Type::Any)) {
                    Type::Any
                } else if args.iter().any(|ty| matches!(ty, Type::Float)) {
                    Type::Float
                } else {
                    Type::Int
                }
            }
        }
    }
}

/// One builtin function: everything the binder, the printer and the
/// evaluator need to know about it.
#[derive(Debug, Clone, Copy)]
pub struct Signature {
    /// The name the plan is printed with, and the one a statement
    /// writes unless it writes an alias.
    pub name: &'static str,
    /// Other spellings of the same function, since ISO gives two of
    /// these a long name and a short one.
    pub aliases: &'static [&'static str],
    /// Which function this is to the rest of the engine.
    pub func: Func,
    pub arity: Arity,
    /// What the arguments may be. One kind covers every position,
    /// because no builtin here mixes kinds across its arguments yet.
    pub arg: Kind,
    /// How the refusal reads when an argument is of the wrong type,
    /// after the name and before the type that arrived. Written out
    /// rather than derived from the kind, because a message is for a
    /// reader and each of these says the thing its function is about.
    pub needs: &'static str,
    pub ret: Ret,
    /// Whether the same arguments always give the same answer. A
    /// deterministic call over literals is answered while binding, and
    /// one that is not deterministic is answered per row however
    /// little its arguments vary.
    pub deterministic: bool,
    /// Whether it folds a group of rows rather than answering one.
    pub aggregate: bool,
    /// Whether `f(*)` is a spelling it has. `COUNT` is the one.
    pub star: bool,
    /// Whether a statement may reach it by writing its name. The two
    /// normalization functions are read by the parser, because both
    /// write a normal form where an expression cannot stand, so they
    /// have a row here for their name and their kernel and no name
    /// resolution.
    pub by_name: bool,
    /// The code that answers it, for the scalars. An aggregate has
    /// none: what answers it is the accumulator the grouping keeps.
    pub kernel: Option<Kernel>,
}

/// Every builtin, in the order the family reads: the set functions, the
/// element functions, the list and path functions, then the strings.
pub static REGISTRY: &[Signature] = &[
    Signature {
        name: "count",
        aliases: &[],
        func: Func::Count,
        arity: Arity::Exactly(1),
        arg: Kind::Any,
        needs: "needs a value",
        ret: Ret::Int,
        deterministic: true,
        aggregate: true,
        star: true,
        by_name: true,
        kernel: None,
    },
    Signature {
        name: "sum",
        aliases: &[],
        func: Func::Sum,
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Same,
        deterministic: true,
        aggregate: true,
        star: false,
        by_name: true,
        kernel: None,
    },
    Signature {
        name: "avg",
        aliases: &[],
        func: Func::Avg,
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: true,
        star: false,
        by_name: true,
        kernel: None,
    },
    Signature {
        name: "min",
        aliases: &[],
        func: Func::Min,
        arity: Arity::Exactly(1),
        arg: Kind::Any,
        needs: "needs a value",
        ret: Ret::Same,
        deterministic: true,
        aggregate: true,
        star: false,
        by_name: true,
        kernel: None,
    },
    Signature {
        name: "max",
        aliases: &[],
        func: Func::Max,
        arity: Arity::Exactly(1),
        arg: Kind::Any,
        needs: "needs a value",
        ret: Ret::Same,
        deterministic: true,
        aggregate: true,
        star: false,
        by_name: true,
        kernel: None,
    },
    Signature {
        name: "collect",
        aliases: &[],
        func: Func::Collect,
        arity: Arity::Exactly(1),
        arg: Kind::Any,
        needs: "needs a value",
        ret: Ret::ListOf,
        deterministic: true,
        aggregate: true,
        star: false,
        by_name: true,
        kernel: None,
    },
    Signature {
        name: "id",
        aliases: &[],
        func: Func::Id,
        arity: Arity::Exactly(1),
        arg: Kind::Element,
        needs: "needs a node or rel",
        ret: Ret::Int,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(element_kernel),
    },
    Signature {
        name: "element_id",
        aliases: &[],
        func: Func::ElementId,
        arity: Arity::Exactly(1),
        arg: Kind::Element,
        needs: "needs a node or an edge",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(element_kernel),
    },
    Signature {
        name: "size",
        aliases: &[],
        func: Func::Size,
        arity: Arity::Exactly(1),
        arg: Kind::Sized,
        needs: "needs a list or string",
        ret: Ret::Int,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(count_kernel),
    },
    Signature {
        name: "cardinality",
        aliases: &[],
        func: Func::Cardinality,
        arity: Arity::Exactly(1),
        arg: Kind::List,
        needs: "needs a list",
        ret: Ret::Int,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(count_kernel),
    },
    Signature {
        name: "path_length",
        aliases: &[],
        func: Func::PathLength,
        arity: Arity::Exactly(1),
        arg: Kind::Path,
        needs: "needs a path",
        ret: Ret::Int,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(count_kernel),
    },
    Signature {
        name: "elements",
        aliases: &[],
        func: Func::Elements,
        arity: Arity::Exactly(1),
        arg: Kind::Path,
        needs: "needs a path",
        ret: Ret::ListOfAny,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(count_kernel),
    },
    Signature {
        name: "all_different",
        aliases: &[],
        func: Func::AllDifferent,
        arity: Arity::AtLeast(2),
        arg: Kind::Element,
        needs: "compares nodes and edges",
        ret: Ret::Bool,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(identity_kernel),
    },
    Signature {
        name: "same",
        aliases: &[],
        func: Func::Same,
        arity: Arity::AtLeast(2),
        arg: Kind::Element,
        needs: "compares nodes and edges",
        ret: Ret::Bool,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(identity_kernel),
    },
    Signature {
        name: "char_length",
        aliases: &["character_length"],
        func: Func::CharLength,
        arity: Arity::Exactly(1),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Int,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(string_kernel),
    },
    Signature {
        name: "octet_length",
        aliases: &[],
        func: Func::OctetLength,
        arity: Arity::Exactly(1),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Int,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(string_kernel),
    },
    Signature {
        name: "upper",
        aliases: &[],
        func: Func::Upper,
        arity: Arity::Exactly(1),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(string_kernel),
    },
    Signature {
        name: "lower",
        aliases: &[],
        func: Func::Lower,
        arity: Arity::Exactly(1),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(string_kernel),
    },
    Signature {
        name: "trim",
        aliases: &[],
        func: Func::Trim(Trim::Both),
        arity: Arity::Between(1, 2),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(trim_kernel),
    },
    // The two ends, which a statement reaches by writing LEADING or
    // TRAILING in the explicit form and cannot reach by name: there is
    // no function called trim_leading in ISO, and inventing one here
    // would be inventing a spelling for a thing the standard already
    // spells.
    Signature {
        name: "trim_leading",
        aliases: &[],
        func: Func::Trim(Trim::Leading),
        arity: Arity::Between(1, 2),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: false,
        kernel: Some(trim_kernel),
    },
    Signature {
        name: "trim_trailing",
        aliases: &[],
        func: Func::Trim(Trim::Trailing),
        arity: Arity::Between(1, 2),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: false,
        kernel: Some(trim_kernel),
    },
    Signature {
        name: "btrim",
        aliases: &[],
        func: Func::Trim(Trim::Btrim),
        arity: Arity::Between(1, 2),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(trim_kernel),
    },
    Signature {
        name: "ltrim",
        aliases: &[],
        func: Func::Trim(Trim::Ltrim),
        arity: Arity::Between(1, 2),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(trim_kernel),
    },
    Signature {
        name: "rtrim",
        aliases: &[],
        func: Func::Trim(Trim::Rtrim),
        arity: Arity::Between(1, 2),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(trim_kernel),
    },
    // The substring function, which in GQL is these two and nothing
    // else: SUBSTRING is a word the standard has reserved and given no
    // meaning yet, so a query that wants the middle of a string writes
    // one of these inside the other.
    Signature {
        name: "left",
        aliases: &[],
        func: Func::Cut(Cut::Left),
        arity: Arity::Exactly(2),
        arg: Kind::Counted,
        needs: "needs a string and a count of characters",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(cut_kernel),
    },
    Signature {
        name: "right",
        aliases: &[],
        func: Func::Cut(Cut::Right),
        arity: Arity::Exactly(2),
        arg: Kind::Counted,
        needs: "needs a string and a count of characters",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(cut_kernel),
    },
    // ISO 20.6, the datetime value functions. None of them is reachable
    // by name: the grammar writes them as bare words with no
    // parentheses, so a query saying CURRENT_DATE() is asking for a
    // function nobody defined and gets told so. The argument is the
    // instant the statement is running at, which the parser never
    // writes and the binder always does.
    Signature {
        name: "current_date",
        aliases: &[],
        func: Func::Datetime(DatetimeFn::CurrentDate),
        arity: Arity::Exactly(1),
        arg: Kind::Any,
        needs: "needs the instant the statement is running at",
        ret: Ret::Same,
        deterministic: false,
        aggregate: false,
        star: false,
        by_name: false,
        kernel: Some(datetime_kernel),
    },
    Signature {
        name: "current_time",
        aliases: &[],
        func: Func::Datetime(DatetimeFn::CurrentTime),
        arity: Arity::Exactly(1),
        arg: Kind::Any,
        needs: "needs the instant the statement is running at",
        ret: Ret::Same,
        deterministic: false,
        aggregate: false,
        star: false,
        by_name: false,
        kernel: Some(datetime_kernel),
    },
    Signature {
        name: "current_timestamp",
        aliases: &[],
        func: Func::Datetime(DatetimeFn::CurrentTimestamp),
        arity: Arity::Exactly(1),
        arg: Kind::Any,
        needs: "needs the instant the statement is running at",
        ret: Ret::Same,
        deterministic: false,
        aggregate: false,
        star: false,
        by_name: false,
        kernel: Some(datetime_kernel),
    },
    Signature {
        name: "local_time",
        aliases: &[],
        func: Func::Datetime(DatetimeFn::LocalTime),
        arity: Arity::Exactly(1),
        arg: Kind::Any,
        needs: "needs the instant the statement is running at",
        ret: Ret::Same,
        deterministic: false,
        aggregate: false,
        star: false,
        by_name: false,
        kernel: Some(datetime_kernel),
    },
    Signature {
        name: "local_timestamp",
        aliases: &[],
        func: Func::Datetime(DatetimeFn::LocalTimestamp),
        arity: Arity::Exactly(1),
        arg: Kind::Any,
        needs: "needs the instant the statement is running at",
        ret: Ret::Same,
        deterministic: false,
        aggregate: false,
        star: false,
        by_name: false,
        kernel: Some(datetime_kernel),
    },
    Signature {
        name: "normalize",
        aliases: &[],
        func: Func::Normalize(NormalForm::Nfc),
        arity: Arity::Exactly(1),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Str,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: false,
        kernel: Some(string_kernel),
    },
    Signature {
        name: "is_normalized",
        aliases: &[],
        func: Func::IsNormalized(NormalForm::Nfc),
        arity: Arity::Exactly(1),
        arg: Kind::Str,
        needs: "needs a string",
        ret: Ret::Bool,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: false,
        kernel: Some(string_kernel),
    },
    Signature {
        name: "abs",
        aliases: &[],
        func: Func::Math(Math::Abs),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Same,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(exact_kernel),
    },
    Signature {
        name: "ceil",
        aliases: &["ceiling"],
        func: Func::Math(Math::Ceil),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Same,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(exact_kernel),
    },
    Signature {
        name: "floor",
        aliases: &[],
        func: Func::Math(Math::Floor),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Same,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(exact_kernel),
    },
    Signature {
        name: "round",
        aliases: &[],
        func: Func::Math(Math::Round),
        arity: Arity::Between(1, 2),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Same,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(exact_kernel),
    },
    Signature {
        name: "sign",
        aliases: &[],
        func: Func::Math(Math::Sign),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Int,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(exact_kernel),
    },
    Signature {
        name: "mod",
        aliases: &[],
        func: Func::Math(Math::Mod),
        arity: Arity::Exactly(2),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Wider,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(pair_kernel),
    },
    Signature {
        name: "sqrt",
        aliases: &[],
        func: Func::Math(Math::Sqrt),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "power",
        aliases: &[],
        func: Func::Math(Math::Power),
        arity: Arity::Exactly(2),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(pair_kernel),
    },
    Signature {
        name: "exp",
        aliases: &[],
        func: Func::Math(Math::Exp),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "ln",
        aliases: &[],
        func: Func::Math(Math::Ln),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "log",
        aliases: &[],
        func: Func::Math(Math::Log),
        arity: Arity::Exactly(2),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(pair_kernel),
    },
    Signature {
        name: "log10",
        aliases: &[],
        func: Func::Math(Math::Log10),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "sin",
        aliases: &[],
        func: Func::Math(Math::Sin),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "cos",
        aliases: &[],
        func: Func::Math(Math::Cos),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "tan",
        aliases: &[],
        func: Func::Math(Math::Tan),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "cot",
        aliases: &[],
        func: Func::Math(Math::Cot),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "asin",
        aliases: &[],
        func: Func::Math(Math::Asin),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "acos",
        aliases: &[],
        func: Func::Math(Math::Acos),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "atan",
        aliases: &[],
        func: Func::Math(Math::Atan),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "degrees",
        aliases: &[],
        func: Func::Math(Math::Degrees),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
    Signature {
        name: "radians",
        aliases: &[],
        func: Func::Math(Math::Radians),
        arity: Arity::Exactly(1),
        arg: Kind::Number,
        needs: "needs a number",
        ret: Ret::Float,
        deterministic: true,
        aggregate: false,
        star: false,
        by_name: true,
        kernel: Some(real_kernel),
    },
];

/// The row number a written name resolves to, or nothing when no
/// builtin has that name. The comparison ignores case, since GQL folds
/// the names of its functions and a statement may shout them.
pub fn lookup(name: &str) -> Option<u16> {
    let at = REGISTRY.iter().position(|sig| {
        sig.by_name
            && (sig.name.eq_ignore_ascii_case(name)
                || sig
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(name)))
    })?;
    Some(at as u16)
}

/// The row at that number. The number comes from [`lookup`] or from
/// [`row_of`], both of which read this same table, so a number that is
/// past its end is a bug rather than something a statement can write.
pub fn row(at: u16) -> Option<&'static Signature> {
    REGISTRY.get(at as usize)
}

/// Which row answers a function the engine already holds. The two
/// normalization functions carry a form, and the form is not part of
/// which function it is, so what is compared is the function with the
/// form set aside.
pub fn row_of(func: Func) -> Option<u16> {
    let key = keyed(func);
    let at = REGISTRY.iter().position(|sig| keyed(sig.func) == key)?;
    Some(at as u16)
}

/// A function with the part that is not its identity taken off, which
/// is the normal form the two normalization functions carry.
fn keyed(func: Func) -> Func {
    match func {
        Func::Normalize(_) => Func::Normalize(NormalForm::Nfc),
        Func::IsNormalized(_) => Func::IsNormalized(NormalForm::Nfc),
        other => other,
    }
}

/// The signature of a function the engine already holds, for the places
/// that have a function and no row number: the printer and the binder's
/// horizontal aggregates. A call that is evaluated per row reads its
/// row number instead and never walks the table.
pub fn signature(func: Func) -> Option<&'static Signature> {
    row_of(func).and_then(row)
}

/// The name a function is printed with. Every function has a row, so
/// the fallback is unreachable and says so rather than inventing one.
pub fn name_of(func: Func) -> &'static str {
    signature(func).map(|sig| sig.name).unwrap_or("?")
}

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// The answer of a call every one of whose arguments is a literal, or
/// nothing when the call is not one that can be answered early.
///
/// A deterministic kernel over values the statement wrote answers the
/// same thing on every row, so it is answered once here and the plan
/// carries the answer instead of the call. A kernel that refuses its
/// arguments is left alone rather than refused early, because the
/// condition it raises belongs to the row that asked for it and a
/// statement that never reaches that row never raises it.
pub fn fold(sig: &Signature, args: &[BoundExpr]) -> Option<Literal> {
    let kernel = sig.kernel?;
    if !sig.deterministic {
        return None;
    }
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            BoundExpr::Literal(lit) => values.push(value_of(lit)),
            _ => return None,
        }
    }
    literal_of(kernel(sig.func, &values).ok()?)
}

/// The value a literal stands for.
pub fn value_of(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Int(i) => Value::Int(*i),
        Literal::Float(f) => Value::Float(*f),
        Literal::Str(s) => Value::Str(s.clone()),
        Literal::Temporal(t) => Value::Temporal(*t),
    }
}

/// The literal a value is, for the values a statement can write. A list
/// and an element are not among them, so a call answering one of those
/// stays a call.
pub fn literal_of(value: Value) -> Option<Literal> {
    Some(match value {
        Value::Null => Literal::Null,
        Value::Bool(b) => Literal::Bool(b),
        Value::Int(i) => Literal::Int(i),
        Value::Float(f) => Literal::Float(f),
        Value::Str(s) => Literal::Str(s),
        Value::Temporal(t) => Literal::Temporal(t),
        _ => return None,
    })
}

/// The one argument a scalar kernel was handed, or the failure that
/// says the binder let a call through with the wrong count. The binder
/// checks arity against the signature, so this is a bug in zu rather
/// than something a statement can write.
fn one(func: Func, args: &[Value]) -> Result<&Value> {
    match args {
        [value] => Ok(value),
        _ => Err(invalid(format!(
            "{}() takes exactly one argument",
            name_of(func)
        ))),
    }
}

/// ID and ELEMENT_ID: what names the element the argument holds. ID
/// answers a node's offset and null for an edge, which is zu's own
/// function and older than the standard's; ELEMENT_ID answers a string
/// that says which kind of element it names, so a node and an edge that
/// sit at the same number are two identifiers and not one. An edge is
/// its table, the pair it runs between and which copy of that pair it
/// is, which is what tells parallel edges apart.
fn element_kernel(func: Func, args: &[Value]) -> Result<Value> {
    let value = one(func, args)?;
    match (func, value) {
        (Func::Id, Value::Node { offset, .. }) => Ok(Value::Int(*offset as i64)),
        (Func::Id, Value::Rel { .. }) => Ok(Value::Null),
        (Func::ElementId, Value::Node { table, offset }) => {
            Ok(Value::Str(format!("n:{table}:{offset}")))
        }
        (
            Func::ElementId,
            Value::Rel {
                table,
                src,
                dst,
                ord,
            },
        ) => Ok(Value::Str(format!("e:{table}:{src}:{dst}:{ord}"))),
        (_, Value::Null) => Ok(Value::Null),
        (Func::Id, other) => Err(invalid(format!("id() expects a node, got {other:?}"))),
        (_, other) => Err(invalid(format!(
            "element_id() expects a node or an edge, got {other:?}"
        ))),
    }
}

/// SIZE, CARDINALITY, PATH_LENGTH and ELEMENTS: the four questions
/// about how much a value holds and what it holds.
///
/// SIZE counts the elements of a list, the characters of a string and
/// the elements of a path, and reads a chain's stored length rather
/// than materializing the list behind it. CARDINALITY is the same count
/// over lists only, since a string has a length and not a cardinality
/// and answering anyway would let a query that meant CHAR_LENGTH pass.
/// PATH_LENGTH counts edges, so a path of two nodes has three elements
/// and a length of one. ELEMENTS hands back the walk in the order it
/// was taken, which is the shape the path already holds, so the list is
/// the same values under another type rather than a copy of anything.
fn count_kernel(func: Func, args: &[Value]) -> Result<Value> {
    let value = one(func, args)?;
    match (func, value) {
        (_, Value::Null) => Ok(Value::Null),
        (Func::Size, Value::Chain(link)) => Ok(Value::Int(link.hops as i64)),
        (Func::Size, Value::List(items) | Value::Path(items)) => Ok(Value::Int(items.len() as i64)),
        (Func::Size, Value::Str(s)) => Ok(Value::Int(s.chars().count() as i64)),
        (Func::Cardinality, Value::List(items)) => Ok(Value::Int(items.len() as i64)),
        (Func::PathLength, Value::Path(elements)) => Ok(Value::Int((elements.len() / 2) as i64)),
        (Func::Elements, Value::Path(elements)) => Ok(Value::List(elements.clone())),
        (Func::Size, other) => Err(invalid(format!(
            "size() expects a list or string, got {other:?}"
        ))),
        (Func::Cardinality, other) => Err(invalid(format!(
            "cardinality() expects a list, got {other:?}"
        ))),
        (func, other) => Err(invalid(format!(
            "{}() expects a path, got {other:?}",
            name_of(func)
        ))),
    }
}

/// G113 and G114, ALL_DIFFERENT and SAME: element identity, which is
/// the table and the row for a node and the table, the ends and the
/// ordinal for an edge, so comparing the values is comparing the
/// elements. A null argument leaves the answer unknown, the way it does
/// in the comparison this is a shorthand for, and a list this short is
/// walked pairwise rather than sorted.
fn identity_kernel(func: Func, args: &[Value]) -> Result<Value> {
    for arg in args {
        match arg {
            Value::Null => return Ok(Value::Null),
            Value::Node { .. } | Value::Rel { .. } => {}
            other => {
                return Err(invalid(format!(
                    "{}() compares nodes and edges, got {other:?}",
                    name_of(func)
                )));
            }
        }
    }
    let same = func == Func::Same;
    let held = args
        .iter()
        .enumerate()
        .all(|(at, left)| args[at + 1..].iter().all(|right| (left == right) == same));
    Ok(Value::Bool(held))
}

/// ISO 20.22 and 20.24, the questions about one string: its two
/// lengths, its two folds and the two about a normal form. A null in
/// answers null, which is the rule every scalar here shares with the
/// operators around them.
fn string_kernel(func: Func, args: &[Value]) -> Result<Value> {
    let s = match settle(one(func, args)?.clone()) {
        Value::Str(s) => s,
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(invalid(format!(
                "{}() expects a string, got {other:?}",
                name_of(func)
            )));
        }
    };
    Ok(match func {
        // Characters, not bytes: what a reader counts is the scalar
        // values, and what the store keeps is the UTF-8 they encode
        // to. The two agree on ASCII and nowhere else.
        Func::CharLength => Value::Int(s.chars().count() as i64),
        Func::OctetLength => Value::Int(s.len() as i64),
        // ASCII is the fold this does, which is the fold the standard's
        // default collation asks for and the one that needs no table. A
        // character outside it is left as it stands rather than folded
        // by a rule this engine has not written down yet.
        Func::Upper => Value::Str(s.to_ascii_uppercase()),
        Func::Lower => Value::Str(s.to_ascii_lowercase()),
        // ISO 20.24 and 19.7, both answered by UAX 15 and the Unicode
        // Character Database, which is where the tables under these two
        // come from.
        Func::Normalize(form) => Value::Str(unicode::normalize(&s, form)),
        Func::IsNormalized(form) => Value::Bool(unicode::is_normalized(&s, form)),
        other => {
            return Err(invalid(format!(
                "{}() is not a question about a string",
                name_of(other)
            )));
        }
    })
}

/// ISO 20.24, the trim family: characters off the front of a string,
/// off the back, or off both.
///
/// Six functions and one piece of code, because the whole of what
/// tells them apart is which ends are trimmed and how many characters
/// may be trimmed. `TRIM` takes one character and raises `22027` when
/// it is handed a longer string, which is the condition the standard
/// names and the reason `BTRIM`, `LTRIM` and `RTRIM` exist at all:
/// those three take a set and trim any character of it. A trim
/// character nobody wrote is a space, and trimming an empty set
/// answers the string it was given.
fn trim_kernel(func: Func, args: &[Value]) -> Result<Value> {
    let trim = match func {
        Func::Trim(trim) => trim,
        other => {
            return Err(invalid(format!("{}() is not a trim", name_of(other))));
        }
    };
    let text = match str_arg(func, args.first())? {
        Some(text) => text,
        None => return Ok(Value::Null),
    };
    let chars = match args.get(1) {
        None => Some(" ".to_string()),
        Some(_) => str_arg(func, args.get(1))?,
    };
    let Some(chars) = chars else {
        return Ok(Value::Null);
    };
    let one_character = matches!(trim, Trim::Both | Trim::Leading | Trim::Trailing);
    if one_character && chars.chars().count() != 1 {
        return Err(gql(
            codes::C22027,
            format!(
                "{}() trims one character and was given {} of them, which is what btrim, ltrim and rtrim are for",
                name_of(func),
                chars.chars().count()
            ),
        ));
    }
    let set: Vec<char> = chars.chars().collect();
    let front = matches!(trim, Trim::Both | Trim::Leading | Trim::Btrim | Trim::Ltrim);
    let back = matches!(
        trim,
        Trim::Both | Trim::Trailing | Trim::Btrim | Trim::Rtrim
    );
    let mut out = text.as_str();
    if front {
        out = out.trim_start_matches(|c| set.contains(&c));
    }
    if back {
        out = out.trim_end_matches(|c| set.contains(&c));
    }
    Ok(Value::Str(out.to_string()))
}

/// ISO 20.24, the substring function: the first characters of a string
/// or the last of them.
///
/// The count is characters and not bytes, the way every length here is,
/// so `LEFT(s, 2)` answers two characters however many bytes they took
/// to write. A count above the length of the string answers the whole
/// string, since there is nothing else to hand back and asking for more
/// than there is is not an error the standard names. A negative count
/// is `22011`, which is the one condition it does name for these, and a
/// count that is not a whole number is the same condition, since the
/// grammar takes any numeric expression there and half a character is
/// not a thing a string has.
fn cut_kernel(func: Func, args: &[Value]) -> Result<Value> {
    let cut = match func {
        Func::Cut(cut) => cut,
        other => {
            return Err(invalid(format!(
                "{}() is not a substring function",
                name_of(other)
            )));
        }
    };
    let Some(text) = str_arg(func, args.first())? else {
        return Ok(Value::Null);
    };
    let given = args
        .get(1)
        .ok_or_else(|| invalid(format!("{}() was given no count", name_of(func))))?;
    let count = match settle(given.clone()) {
        Value::Null => return Ok(Value::Null),
        Value::Int(i) => i,
        Value::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e18 => f as i64,
        Value::Float(f) => {
            return Err(gql(
                codes::C22011,
                format!(
                    "{}() counts whole characters and was asked for {f} of them",
                    name_of(func)
                ),
            ));
        }
        other => {
            return Err(invalid(format!(
                "{}() expects a number, got {other:?}",
                name_of(func)
            )));
        }
    };
    if count < 0 {
        return Err(gql(
            codes::C22011,
            format!(
                "{}() was asked for {count} characters, and a string has no negative number of them",
                name_of(func)
            ),
        ));
    }
    let count = count as usize;
    let held = text.chars().count();
    let taken: String = match cut {
        Cut::Left => text.chars().take(count).collect(),
        Cut::Right => text.chars().skip(held.saturating_sub(count)).collect(),
    };
    Ok(Value::Str(taken))
}

/// ISO 20.6, the datetime value functions: one instant cut five ways.
///
/// The instant arrives as the argument, so this kernel reads no clock
/// and is as pure as every other one here. What makes the five differ
/// is only which type the instant is read as, and reading one temporal
/// type as another is what the cast rules already say, so the cut is a
/// conversion and not arithmetic written twice.
///
/// A null instant answers null, which is the rule every kernel here
/// keeps rather than an answer a query can reach: the binder plants the
/// argument itself and the run always has a clock, so nothing a
/// statement writes gets here with a null in hand.
fn datetime_kernel(func: Func, args: &[Value]) -> Result<Value> {
    let Func::Datetime(which) = func else {
        return Err(invalid(format!(
            "{}() is not a datetime value function",
            name_of(func)
        )));
    };
    let instant = match args.first().map(|value| settle(value.clone())) {
        Some(Value::Temporal(instant)) => instant,
        Some(Value::Null) => return Ok(Value::Null),
        Some(other) => {
            return Err(invalid(format!(
                "{}() expects an instant, got {other:?}",
                name_of(func)
            )));
        }
        None => {
            return Err(invalid(format!("{}() was given no instant", name_of(func))));
        }
    };
    let cut = cast::convert(instant, &which.target()).ok_or_else(|| {
        invalid(format!(
            "{}() cannot read the instant as {}",
            name_of(func),
            which.target()
        ))
    })?;
    Ok(Value::Temporal(cut))
}

/// The string an argument holds, or nothing where it holds a null.
/// Every kernel over strings reads its arguments through this, so a
/// null answers null in one place rather than in each of them.
fn str_arg(func: Func, value: Option<&Value>) -> Result<Option<String>> {
    let value = value.ok_or_else(|| invalid(format!("{}() was given no string", name_of(func))))?;
    match settle(value.clone()) {
        Value::Str(s) => Ok(Some(s)),
        Value::Null => Ok(None),
        other => Err(invalid(format!(
            "{}() expects a string, got {other:?}",
            name_of(func)
        ))),
    }
}

/// A GQL condition raised by a kernel. These are conditions the
/// standard names, unlike the failures above, so they carry the code a
/// client checks rather than a message it would have to read.
fn gql(status: zu_common::GqlStatus, detail: String) -> ZuError {
    ZuError::gql(status, detail)
}

/// `22003 numeric value out of range`, for an answer that is a number
/// the type cannot hold: an absolute value one past the top of the
/// integers, an exponential that ran off the end of the doubles.
fn out_of_range(func: Func, detail: String) -> ZuError {
    gql(codes::C22003, format!("{}() {detail}", name_of(func)))
}

/// Which numeric function this is, for the three kernels the numeric
/// library shares. A function that is not one of them cannot reach
/// them, since the row that carries the kernel carries the function.
fn math_of(func: Func) -> Result<Math> {
    match func {
        Func::Math(math) => Ok(math),
        other => Err(invalid(format!(
            "{}() is not a numeric function",
            name_of(other)
        ))),
    }
}

/// The number an argument holds, as a float. Every kernel here that
/// answers an approximate number reads its arguments through this, so
/// an integer argument is widened once and in one place.
fn real(func: Func, value: &Value) -> Result<f64> {
    match value {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(invalid(format!(
            "{}() expects a number, got {other:?}",
            name_of(func)
        ))),
    }
}

/// The answer of a kernel over the reals, refused when it left the
/// range the doubles hold. An argument that was already infinite is
/// let through, because an engine that raises there is answering a
/// question about IEEE arithmetic with a condition the statement did
/// not cause (GA01).
fn finite(func: Func, arg: f64, answer: f64) -> Result<Value> {
    if answer.is_finite() || !arg.is_finite() {
        Ok(Value::Float(answer))
    } else {
        Err(out_of_range(
            func,
            format!("of {arg} is outside the range of a float"),
        ))
    }
}

/// GF01, the numeric functions that keep an exact argument exact: ABS,
/// CEIL, FLOOR, ROUND and SIGN.
///
/// An integer in gives an integer out, because the answer of every one
/// of these over a whole number is a whole number, and widening it to a
/// float would lose a digit above two to the fifty third for no reason
/// the statement asked for. ROUND takes the digits to round to as a
/// second argument: a positive count rounds inside the fraction, a
/// negative one rounds tens, hundreds and upwards, and halves go away
/// from nought either way, which is the rule SQL rounds by and the one
/// a reader expects when they write it out by hand.
fn exact_kernel(func: Func, args: &[Value]) -> Result<Value> {
    let math = math_of(func)?;
    let (value, digits) = match args {
        [value] => (settle(value.clone()), Value::Int(0)),
        [value, digits] if math == Math::Round => (settle(value.clone()), settle(digits.clone())),
        _ => {
            return Err(invalid(format!(
                "{}() was given {} arguments",
                name_of(func),
                args.len()
            )));
        }
    };
    if matches!(value, Value::Null) || matches!(digits, Value::Null) {
        return Ok(Value::Null);
    }
    let digits = match digits {
        Value::Int(n) => n,
        other => {
            return Err(gql(
                codes::C22G03,
                format!("round() rounds to a whole number of digits, got {other:?}"),
            ));
        }
    };
    match (math, &value) {
        (Math::Abs, Value::Int(i)) => i
            .checked_abs()
            .map(Value::Int)
            .ok_or_else(|| out_of_range(func, format!("of {i} is one past the top of an integer"))),
        (Math::Abs, Value::Float(f)) => Ok(Value::Float(f.abs())),
        (Math::Sign, Value::Int(i)) => Ok(Value::Int(i.signum())),
        (Math::Sign, Value::Float(f)) => Ok(Value::Int(if *f > 0.0 {
            1
        } else if *f < 0.0 {
            -1
        } else {
            0
        })),
        // A whole number is already at its own ceiling and its own
        // floor, so these two answer what they were handed rather than
        // going through a float that could not hold it.
        (Math::Ceil | Math::Floor | Math::Round, Value::Int(i)) if digits >= 0 => {
            Ok(Value::Int(*i))
        }
        (Math::Round, Value::Int(i)) => rounded_int(*i, digits)
            .map(Value::Int)
            .ok_or_else(|| out_of_range(func, format!("of {i} to {digits} digits does not fit"))),
        (Math::Ceil, Value::Float(f)) => Ok(Value::Float(f.ceil())),
        (Math::Floor, Value::Float(f)) => Ok(Value::Float(f.floor())),
        (Math::Round, Value::Float(f)) => {
            let scale = 10f64.powi(digits.clamp(-308, 308) as i32);
            let answer = if digits == 0 {
                f.round()
            } else {
                (f * scale).round() / scale
            };
            finite(func, *f, answer)
        }
        (_, other) => Err(invalid(format!(
            "{}() expects a number, got {other:?}",
            name_of(func)
        ))),
    }
}

/// An integer rounded to a place left of the decimal point, kept in the
/// integers the whole way: a hundred and fifty rounded to minus one
/// digit is two hundred, and nothing here goes through a float, so a
/// number wider than a double holds is rounded exactly.
fn rounded_int(value: i64, digits: i64) -> Option<i64> {
    let places = u32::try_from(-digits).ok()?;
    // Past nineteen digits every integer rounds to nought, and the
    // power below would overflow rather than say so.
    if places > 19 {
        return Some(0);
    }
    let factor = 10i128.checked_pow(places)?;
    let value = value as i128;
    let half = factor / 2;
    let carried = if value >= 0 {
        (value + half) / factor
    } else {
        (value - half) / factor
    };
    i64::try_from(carried.checked_mul(factor)?).ok()
}

/// GF02 and GF03, the functions over one number whose answer is
/// approximate: the roots, the exponential, the logarithms and the
/// trigonometric set.
///
/// Every one of these answers a float, because the answer of all but a
/// handful of arguments is irrational and a type that changed with the
/// value would be a type nothing could be planned against. Where the
/// function has no answer at all the kernel raises the condition the
/// standard names for it and never hands back a NaN: a NaN travels
/// through every comparison below it as false and a statement that got
/// one has been told nothing about what went wrong.
fn real_kernel(func: Func, args: &[Value]) -> Result<Value> {
    let math = math_of(func)?;
    let value = settle(one(func, args)?.clone());
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let x = real(func, &value)?;
    let answer = match math {
        // ISO 20.20 defines the square root as the power of one half,
        // so a negative argument is the power function's condition
        // rather than a condition of its own.
        Math::Sqrt => {
            if x < 0.0 {
                return Err(gql(
                    codes::C2201F,
                    format!("sqrt() has no answer for {x}, which is below nought"),
                ));
            }
            x.sqrt()
        }
        Math::Exp => x.exp(),
        Math::Ln => {
            if x <= 0.0 {
                return Err(gql(
                    codes::C2201E,
                    format!("ln() has no answer for {x}, which is not above nought"),
                ));
            }
            x.ln()
        }
        Math::Log10 => {
            if x <= 0.0 {
                return Err(gql(
                    codes::C2201E,
                    format!("log10() has no answer for {x}, which is not above nought"),
                ));
            }
            x.log10()
        }
        Math::Sin => x.sin(),
        Math::Cos => x.cos(),
        Math::Tan => x.tan(),
        // The cotangent is the cosine over the sine, so where the sine
        // is nought it is a division by nought and is raised as one.
        Math::Cot => {
            let sin = x.sin();
            if sin == 0.0 {
                return Err(gql(
                    codes::C22012,
                    format!("cot() has no answer for {x}, where the sine is nought"),
                ));
            }
            x.cos() / sin
        }
        Math::Asin | Math::Acos => {
            if !(-1.0..=1.0).contains(&x) && x.is_finite() {
                return Err(out_of_range(
                    func,
                    format!("has no answer for {x}, which is outside minus one to one"),
                ));
            }
            if math == Math::Asin {
                x.asin()
            } else {
                x.acos()
            }
        }
        Math::Atan => x.atan(),
        Math::Degrees => x.to_degrees(),
        Math::Radians => x.to_radians(),
        _ => {
            return Err(invalid(format!(
                "{}() takes more than one number",
                name_of(func)
            )));
        }
    };
    finite(func, x, answer)
}

/// The numeric functions over two numbers: MOD, POWER and LOG.
///
/// MOD is the operator under another name, so it keeps the operator's
/// arithmetic: an integer remainder where both sides are integers, the
/// sign of the dividend, and `22012` for a nought divisor rather than
/// the infinity the hardware would answer. POWER and LOG answer floats
/// for the reason the kernel above does, and each raises where it has
/// no answer: nought to a negative power and a negative number to a
/// fractional power are `2201F`, and a logarithm of a number that is
/// not above nought, or in a base that is not above nought or is one,
/// is `2201E`.
fn pair_kernel(func: Func, args: &[Value]) -> Result<Value> {
    let math = math_of(func)?;
    let [left, right] = args else {
        return Err(invalid(format!(
            "{}() takes two arguments, got {}",
            name_of(func),
            args.len()
        )));
    };
    let (left, right) = (settle(left.clone()), settle(right.clone()));
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    if let (Math::Mod, Value::Int(x), Value::Int(y)) = (math, &left, &right) {
        if *y == 0 {
            return Err(gql(codes::C22012, "division by zero".into()));
        }
        return x
            .checked_rem(*y)
            .map(Value::Int)
            .ok_or_else(|| out_of_range(func, format!("of {x} and {y} does not fit an integer")));
    }
    let x = real(func, &left)?;
    let y = real(func, &right)?;
    let answer = match math {
        Math::Mod => {
            if y == 0.0 {
                return Err(gql(codes::C22012, "division by zero".into()));
            }
            x % y
        }
        Math::Power => {
            if x == 0.0 && y < 0.0 {
                return Err(gql(
                    codes::C2201F,
                    "power() has no answer for nought raised to a negative power".into(),
                ));
            }
            if x < 0.0 && y.fract() != 0.0 && y.is_finite() {
                return Err(gql(
                    codes::C2201F,
                    format!("power() has no answer for {x} raised to {y}, which is not whole"),
                ));
            }
            x.powf(y)
        }
        // LOG takes the base first and the number second, which is the
        // order ISO 20.21 writes it in.
        Math::Log => {
            if x <= 0.0 || x == 1.0 {
                return Err(gql(
                    codes::C2201E,
                    format!("log() has no answer in base {x}"),
                ));
            }
            if y <= 0.0 {
                return Err(gql(
                    codes::C2201E,
                    format!("log() has no answer for {y}, which is not above nought"),
                ));
            }
            y.log(x)
        }
        _ => {
            return Err(invalid(format!(
                "{}() takes one number and not two",
                name_of(func)
            )));
        }
    };
    if answer.is_finite() || !x.is_finite() || !y.is_finite() {
        Ok(Value::Float(answer))
    } else {
        Err(out_of_range(
            func,
            format!("of {x} and {y} is outside the range of a float"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row is reachable both ways: a name resolves to the row
    /// that holds it, and the function on that row resolves back to the
    /// same name. A row nobody can reach from a function is a printer
    /// that would print the wrong name.
    #[test]
    fn a_row_is_reached_from_both_sides() {
        let named = |name: &str| lookup(name).and_then(row).map(|sig| sig.name);
        for sig in REGISTRY {
            assert_eq!(signature(sig.func).map(|s| s.name), Some(sig.name));
            if sig.by_name {
                assert_eq!(named(sig.name), Some(sig.name));
                assert_eq!(named(&sig.name.to_uppercase()), Some(sig.name));
                for alias in sig.aliases {
                    assert_eq!(named(alias), Some(sig.name));
                }
            } else {
                assert_eq!(named(sig.name), None);
            }
        }
    }

    /// A name is spelled once. Two rows under one name would resolve to
    /// whichever came first, which is a table that answers by accident.
    #[test]
    fn no_name_is_written_twice() {
        let mut names: Vec<&str> = REGISTRY
            .iter()
            .flat_map(|sig| std::iter::once(sig.name).chain(sig.aliases.iter().copied()))
            .collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "a function name is written twice");
    }

    /// An aggregate is answered by the accumulator the grouping keeps
    /// and a scalar by its kernel, so exactly one of the two is on
    /// every row.
    #[test]
    fn every_scalar_has_a_kernel_and_no_aggregate_has_one() {
        for sig in REGISTRY {
            assert_eq!(
                sig.kernel.is_none(),
                sig.aggregate,
                "{} has the wrong half filled in",
                sig.name
            );
        }
    }

    /// The kernels answer null for a null argument, which is what every
    /// scalar in GQL does and what a row with a missing property hands
    /// them.
    #[test]
    fn a_null_argument_answers_null() {
        for sig in REGISTRY {
            let Some(kernel) = sig.kernel else { continue };
            let args = vec![Value::Null; 2];
            let at = sig.arity.least();
            assert_eq!(
                kernel(sig.func, &args[..at]).unwrap(),
                Value::Null,
                "{} answered a null argument with something else",
                sig.name
            );
        }
    }

    /// The answer of a call, for the tests below, by the name a
    /// statement would write.
    fn call(name: &str, args: &[Value]) -> Result<Value> {
        let sig = lookup(name).and_then(row).expect("a builtin of that name");
        sig.kernel.expect("a scalar")(sig.func, args)
    }

    /// GF01. An exact argument keeps its type through the exact half of
    /// the numeric library, because the whole number a rounding answers
    /// is a whole number and a float would lose the digits above two to
    /// the fifty third that an integer holds.
    #[test]
    fn a_whole_number_stays_whole() {
        for (name, arg, want) in [
            ("abs", -7, 7),
            ("ceil", 7, 7),
            ("floor", 7, 7),
            ("round", 7, 7),
            ("sign", -7, -1),
        ] {
            assert_eq!(call(name, &[Value::Int(arg)]).unwrap(), Value::Int(want));
        }
        assert_eq!(
            call("mod", &[Value::Int(7), Value::Int(3)]).unwrap(),
            Value::Int(1)
        );
        // Rounding to a place left of the point stays in the integers
        // too, and rounds a half away from nought.
        assert_eq!(
            call("round", &[Value::Int(155), Value::Int(-1)]).unwrap(),
            Value::Int(160)
        );
        assert_eq!(
            call("round", &[Value::Int(-155), Value::Int(-1)]).unwrap(),
            Value::Int(-160)
        );
        // And the approximate half answers a float from an integer, so
        // a plan is typed against one answer rather than two.
        assert_eq!(call("sqrt", &[Value::Int(16)]).unwrap(), Value::Float(4.0));
        assert_eq!(
            call("power", &[Value::Int(2), Value::Int(3)]).unwrap(),
            Value::Float(8.0)
        );
    }

    /// GF02 and GF03. Where a function has no answer it raises the
    /// condition the standard names for it. A NaN would travel through
    /// every comparison below as false, so a statement that got one
    /// would have been told nothing about what went wrong.
    #[test]
    fn a_domain_error_is_a_condition_and_not_a_nan() {
        for (name, args, want) in [
            ("ln", vec![Value::Int(0)], codes::C2201E),
            ("log10", vec![Value::Float(-1.0)], codes::C2201E),
            ("log", vec![Value::Int(1), Value::Int(8)], codes::C2201E),
            ("sqrt", vec![Value::Float(-1.0)], codes::C2201F),
            ("power", vec![Value::Int(0), Value::Int(-1)], codes::C2201F),
            (
                "power",
                vec![Value::Float(-2.0), Value::Float(0.5)],
                codes::C2201F,
            ),
            ("mod", vec![Value::Int(1), Value::Int(0)], codes::C22012),
            ("mod", vec![Value::Float(1.0), Value::Int(0)], codes::C22012),
            ("cot", vec![Value::Int(0)], codes::C22012),
            ("asin", vec![Value::Int(2)], codes::C22003),
            ("acos", vec![Value::Float(-1.5)], codes::C22003),
            ("exp", vec![Value::Int(1000)], codes::C22003),
        ] {
            let err = call(name, &args).expect_err("a condition");
            assert_eq!(err.gqlstatus(), Some(want), "{name} raised {err}");
        }
    }

    /// An argument that is already infinite is IEEE 754's business and
    /// not the standard's, so it travels rather than raising: what the
    /// conditions above are about is a statement that asked for a
    /// number nobody has, and this one has already been answered.
    #[test]
    fn an_infinite_argument_is_left_to_ieee() {
        assert_eq!(
            call("exp", &[Value::Float(f64::INFINITY)]).unwrap(),
            Value::Float(f64::INFINITY)
        );
    }

    /// The answer of a trim, by the function rather than by a name,
    /// since the two ends of the explicit form have no name a statement
    /// can write.
    fn trimmed(trim: Trim, args: &[Value]) -> Result<Value> {
        trim_kernel(Func::Trim(trim), args)
    }

    /// GF05 and GF06. Which end is trimmed is the function, and every
    /// one of the six trims the characters it was given and stops at
    /// the first character it was not.
    #[test]
    fn a_trim_takes_characters_off_the_ends_it_is_asked_for() {
        let text = Value::Str("xxayx".into());
        let x = Value::Str("x".into());
        for (trim, want) in [
            (Trim::Both, "ay"),
            (Trim::Leading, "ayx"),
            (Trim::Trailing, "xxay"),
            (Trim::Btrim, "ay"),
            (Trim::Ltrim, "ayx"),
            (Trim::Rtrim, "xxay"),
        ] {
            let got = trimmed(trim, &[text.clone(), x.clone()]).unwrap();
            assert_eq!(got, Value::Str(want.into()), "{trim:?}");
        }
        // A set of characters is a set: any of them is trimmed, in
        // whatever order they stand and however often.
        assert_eq!(
            trimmed(
                Trim::Btrim,
                &[Value::Str("xyyxaxy".into()), Value::Str("xy".into())]
            )
            .unwrap(),
            Value::Str("a".into())
        );
        // An empty set trims nothing, and neither does a character the
        // string does not begin with.
        assert_eq!(
            trimmed(
                Trim::Btrim,
                &[Value::Str("ab".into()), Value::Str("".into())]
            )
            .unwrap(),
            Value::Str("ab".into())
        );
        assert_eq!(
            trimmed(
                Trim::Ltrim,
                &[Value::Str("ab".into()), Value::Str("b".into())]
            )
            .unwrap(),
            Value::Str("ab".into())
        );
        // What nobody wrote is a space, for all six.
        assert_eq!(
            trimmed(Trim::Both, &[Value::Str("  a  ".into())]).unwrap(),
            Value::Str("a".into())
        );
    }

    /// GF05 is a feature of its own because TRIM trims one character
    /// and says so: a longer trim character raises the condition the
    /// standard names rather than being read as the set the three
    /// multi-character functions take.
    #[test]
    fn trimming_more_than_one_character_is_a_condition() {
        for trim in [Trim::Both, Trim::Leading, Trim::Trailing] {
            let err = trimmed(trim, &[Value::Str("abx".into()), Value::Str("ab".into())])
                .expect_err("a trim error");
            assert_eq!(err.gqlstatus(), Some(codes::C22027), "{trim:?}");
        }
        for (trim, want) in [
            (Trim::Btrim, "x"),
            (Trim::Ltrim, "x"),
            // Nothing off the back, the string ending in a character
            // the set does not hold.
            (Trim::Rtrim, "abx"),
        ] {
            assert_eq!(
                trimmed(trim, &[Value::Str("abx".into()), Value::Str("ab".into())]).unwrap(),
                Value::Str(want.into()),
                "{trim:?}"
            );
        }
    }

    /// ISO 20.24. LEFT counts from the front and RIGHT from the back,
    /// both in characters, and a count past the end of the string
    /// answers the string, there being nothing else to answer with.
    #[test]
    fn a_substring_is_counted_in_characters_from_the_end_it_names() {
        let text = Value::Str("héllo".into());
        for (name, count, want) in [
            ("left", 2, "hé"),
            ("right", 2, "lo"),
            ("left", 0, ""),
            ("right", 0, ""),
            ("left", 5, "héllo"),
            ("right", 5, "héllo"),
            // More than the string holds is the string. The count is
            // characters, so the accented one counts once here and takes
            // two bytes in the store.
            ("left", 40, "héllo"),
            ("right", 40, "héllo"),
        ] {
            let got = call(name, &[text.clone(), Value::Int(count)]).unwrap();
            assert_eq!(got, Value::Str(want.into()), "{name} of {count}");
        }
    }

    /// A count no string has is `22011`, which is the one condition the
    /// standard names for the substring function. A count written as a
    /// whole float is a count, since the grammar takes any numeric
    /// expression there, and one with a fraction on it is not.
    #[test]
    fn a_count_of_characters_no_string_has_is_a_condition() {
        let text = Value::Str("abc".into());
        for name in ["left", "right"] {
            let err = call(name, &[text.clone(), Value::Int(-1)]).expect_err("a substring error");
            assert_eq!(err.gqlstatus(), Some(codes::C22011), "{name}");
            let err =
                call(name, &[text.clone(), Value::Float(1.5)]).expect_err("a substring error");
            assert_eq!(err.gqlstatus(), Some(codes::C22011), "{name}");
            assert_eq!(
                call(name, &[text.clone(), Value::Float(2.0)]).unwrap(),
                Value::Str(if name == "left" { "ab" } else { "bc" }.into()),
                "{name}"
            );
        }
    }
}
