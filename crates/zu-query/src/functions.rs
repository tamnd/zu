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
//! answers it. The binder settles which kernel a call runs while it
//! binds, and the evaluator calls through that pointer, so no row is
//! spent deciding which function it is looking at. A call whose
//! arguments are all literals does not reach a row at all: a
//! deterministic kernel answers the same thing every time, so the
//! binder runs it once and keeps the answer.

use zu_common::unicode::NormalForm;
use zu_common::{Result, ZuError, unicode};

use crate::ast::Literal;
use crate::binder::{BoundExpr, Func, Type};
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
}

impl Kind {
    /// Whether an argument of this type is one this kind accepts.
    /// `ANY` is accepted everywhere, because it is the type of a value
    /// nobody has narrowed yet and refusing it would refuse a property
    /// read.
    pub fn accepts(self, ty: &Type) -> bool {
        match self {
            Kind::Any => true,
            Kind::Number => matches!(ty, Type::Any | Type::Int | Type::Float),
            Kind::Str => matches!(ty, Type::Any | Type::Str),
            Kind::List => matches!(ty, Type::Any | Type::List(_)),
            Kind::Sized => matches!(ty, Type::Any | Type::List(_) | Type::Str | Type::Path),
            Kind::Path => matches!(ty, Type::Any | Type::Path),
            Kind::Element => matches!(ty, Type::Any | Type::Node | Type::Rel),
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
}

impl Ret {
    /// The answer's type, given the type of the first argument.
    pub fn of(self, arg: &Type) -> Type {
        match self {
            Ret::Int => Type::Int,
            Ret::Float => Type::Float,
            Ret::Str => Type::Str,
            Ret::Bool => Type::Bool,
            Ret::ListOfAny => Type::List(Box::new(Type::Any)),
            Ret::Same => arg.clone(),
            Ret::ListOf => Type::List(Box::new(arg.clone())),
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
        func: Func::Trim,
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
];

/// The signature a written name resolves to, or nothing when no builtin
/// has that name. The comparison ignores case, since GQL folds the
/// names of its functions and a statement may shout them.
pub fn lookup(name: &str) -> Option<&'static Signature> {
    REGISTRY.iter().find(|sig| {
        sig.by_name
            && (sig.name.eq_ignore_ascii_case(name)
                || sig
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(name)))
    })
}

/// The signature of a function the engine already holds. The two
/// normalization functions carry a form, and the form is not part of
/// which function it is, so the arms are compared and not the values.
pub fn signature(func: Func) -> Option<&'static Signature> {
    REGISTRY
        .iter()
        .find(|sig| std::mem::discriminant(&sig.func) == std::mem::discriminant(&func))
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
/// lengths, its two folds, the spaces off its ends and the two about a
/// normal form. A null in answers null, which is the rule every scalar
/// here shares with the operators around them.
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
        // The one argument form trims spaces, which is what ISO 20.24
        // says it trims: the trim character defaults to a space and the
        // trim specification to BOTH. Naming either is GF06 and a
        // spelling this does not read.
        Func::Trim => Value::Str(s.trim_matches(' ').to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row is reachable both ways: a name resolves to the row
    /// that holds it, and the function on that row resolves back to the
    /// same name. A row nobody can reach from a function is a printer
    /// that would print the wrong name.
    #[test]
    fn a_row_is_reached_from_both_sides() {
        for sig in REGISTRY {
            assert_eq!(signature(sig.func).map(|s| s.name), Some(sig.name));
            if sig.by_name {
                assert_eq!(lookup(sig.name).map(|s| s.name), Some(sig.name));
                assert_eq!(
                    lookup(&sig.name.to_uppercase()).map(|s| s.name),
                    Some(sig.name)
                );
                for alias in sig.aliases {
                    assert_eq!(lookup(alias).map(|s| s.name), Some(sig.name));
                }
            } else {
                assert_eq!(lookup(sig.name).map(|s| s.name), None);
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
            let at = match sig.arity {
                Arity::Exactly(n) => n,
                Arity::AtLeast(n) => n,
            };
            assert_eq!(
                kernel(sig.func, &args[..at]).unwrap(),
                Value::Null,
                "{} answered a null argument with something else",
                sig.name
            );
        }
    }
}
