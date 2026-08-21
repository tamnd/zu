//! The procedure catalog (ISO 13.1 and 13.4, feature GP04).
//!
//! A named procedure call is a call to something the catalog holds, so
//! the name in the statement is a catalog object reference and not a
//! word the binder knows: it is looked up in a schema, it is refused
//! with the schema named when it is not there, and it may be written
//! out in full, `/pagerank`, or left to the schema the call is at.
//!
//! Every procedure here is a built-in and they are all in the root
//! schema, which is the schema every file has. There is no statement
//! that writes a procedure, GQL having none, so this table is the whole
//! catalog and the lookup is against a slice rather than against the
//! file. What matters is that the lookup exists at all: a call resolves
//! by schema and name the way a graph reference does, so the day a
//! procedure comes from somewhere else, the resolving is already the
//! shape it has to be.
//!
//! A descriptor says everything about a procedure that the binder needs
//! and nothing about how it runs. The arity, the argument types and the
//! columns are read off it, so adding a procedure is a row in a table
//! rather than an arm in three matches, and the refusals a caller reads
//! are built from the same words the table holds.

use crate::binder::TableFunc;

/// The schema the built-in procedures are in, which is the root, the
/// one schema every file has.
pub const ROOT: &str = "/";

/// What a parameter accepts, which is a narrow list because every
/// procedure here is a graph algorithm and the things one takes are a
/// node, a count, a column name and a list of nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamTy {
    /// An integer, which is what a node id and a round count both are.
    Int,
    /// A list, of node ids in every case there is so far.
    List,
    /// A string that has to be written as a literal rather than worked
    /// out, because what it names is settled while the statement is
    /// being bound and not while it is running.
    StrLiteral,
}

/// One parameter of a procedure, past the rel table every one of them
/// takes first.
///
/// The three pieces of prose are here rather than at the refusal
/// because a caller who got the call wrong is reading about this
/// parameter and not about parameters in general: `phrase` is how the
/// parameter is named in a list of what the procedure takes, `noun` is
/// how it is named on its own, and `expect` is what it should have
/// been.
#[derive(Debug, Clone, Copy)]
pub struct Param {
    pub ty: ParamTy,
    /// Whether the call may leave it out, which only a defaulted
    /// parameter may be.
    pub optional: bool,
    /// How the parameter reads in the list of what the procedure takes,
    /// as in "takes the rel table and a source node id".
    pub phrase: &'static str,
    /// How the parameter reads on its own, as in "sssp's source".
    pub noun: &'static str,
    /// What the argument should have been, as in "must be a node id".
    pub expect: &'static str,
}

/// The type of the column a procedure yields after `node`. It is a
/// word here rather than the binder's own type because a descriptor
/// says what a procedure is and not how the binder spells things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColTy {
    Int,
    Float,
}

/// One procedure in the catalog.
#[derive(Debug, Clone, Copy)]
pub struct Procedure {
    /// The schema it is in, which is the root for every one of these.
    pub schema: &'static str,
    pub name: &'static str,
    /// The kernel that runs it, which is what the plan carries.
    pub func: TableFunc,
    /// The parameters past the rel table, in written order.
    pub params: &'static [Param],
    /// The column yielded after `node`, and its type.
    pub column: (&'static str, ColTy),
}

const SOURCE: Param = Param {
    ty: ParamTy::Int,
    optional: false,
    phrase: "a source node id",
    noun: "source",
    expect: "a node id",
};

/// Every procedure there is, in the order a listing should read them.
pub const CATALOG: &[Procedure] = &[
    Procedure {
        schema: ROOT,
        name: "pagerank",
        func: TableFunc::Pagerank,
        params: &[],
        column: ("rank", ColTy::Float),
    },
    Procedure {
        schema: ROOT,
        name: "wcc",
        func: TableFunc::Wcc,
        params: &[],
        column: ("component", ColTy::Int),
    },
    Procedure {
        schema: ROOT,
        name: "bfs",
        func: TableFunc::Bfs,
        params: &[SOURCE],
        column: ("level", ColTy::Int),
    },
    Procedure {
        schema: ROOT,
        name: "sssp",
        func: TableFunc::Sssp,
        params: &[SOURCE],
        column: ("distance", ColTy::Int),
    },
    Procedure {
        schema: ROOT,
        name: "sssp_weighted",
        func: TableFunc::SsspWeighted,
        // The weight column is named rather than assumed: a rel table
        // can carry several numeric columns and which one is a distance
        // is the caller's to say.
        params: &[
            SOURCE,
            Param {
                ty: ParamTy::StrLiteral,
                optional: false,
                phrase: "the name of the weight column",
                noun: "weight column",
                expect: "a string literal",
            },
        ],
        column: ("distance", ColTy::Int),
    },
    Procedure {
        schema: ROOT,
        name: "cdlp",
        func: TableFunc::Cdlp,
        // The round count is what makes label propagation reproducible,
        // so it is spellable, and the default is the one Graphalytics
        // fixed.
        params: &[Param {
            ty: ParamTy::Int,
            optional: true,
            phrase: "an optional round count",
            noun: "round count",
            expect: "an integer",
        }],
        column: ("community", ColTy::Int),
    },
    Procedure {
        schema: ROOT,
        name: "lcc",
        func: TableFunc::Lcc,
        params: &[],
        column: ("coefficient", ColTy::Float),
    },
    Procedure {
        schema: ROOT,
        name: "triangle_count",
        func: TableFunc::TriangleCount,
        params: &[],
        column: ("triangles", ColTy::Int),
    },
    Procedure {
        schema: ROOT,
        name: "betweenness",
        func: TableFunc::Betweenness,
        // The sources are a list and not a single node, because the
        // score a node gets is a sum over the sample and running the
        // sample one source at a time would be one pass of the graph
        // per source with the adding left to the caller.
        params: &[Param {
            ty: ParamTy::List,
            optional: false,
            phrase: "a list of source node ids",
            noun: "sources",
            expect: "a list of node ids",
        }],
        column: ("centrality", ColTy::Float),
    },
    Procedure {
        schema: ROOT,
        name: "louvain",
        func: TableFunc::Louvain,
        params: &[],
        column: ("community", ColTy::Int),
    },
];

/// The procedure of that name in that schema, if the catalog holds one.
///
/// The name is folded, since a procedure reference is a name and names
/// are matched the way the rest of the language matches them, and the
/// schema is not, a path being a path.
pub fn resolve(schema: &str, name: &str) -> Option<&'static Procedure> {
    let folded = name.to_ascii_lowercase();
    CATALOG
        .iter()
        .find(|p| p.schema == schema && p.name == folded)
}

/// The names in a schema, for a refusal to list. Empty for a schema
/// that holds no procedures, which is every schema but the root.
pub fn in_schema(schema: &str) -> Vec<&'static str> {
    CATALOG
        .iter()
        .filter(|p| p.schema == schema)
        .map(|p| p.name)
        .collect()
}

impl Procedure {
    /// How many arguments the call has to carry past the rel table, as
    /// the smallest and the largest it may be.
    pub fn arity(&self) -> (usize, usize) {
        let least = self.params.iter().filter(|p| !p.optional).count();
        (least, self.params.len())
    }

    /// What the procedure takes, written the way a sentence writes it,
    /// which is what a refusal about the wrong number of arguments
    /// says.
    pub fn takes(&self) -> String {
        match self.params.len() {
            0 => "only the rel table".to_string(),
            1 => format!("the rel table and {}", self.params[0].phrase),
            n => {
                let front: Vec<&str> = self.params[..n - 1].iter().map(|p| p.phrase).collect();
                format!(
                    "the rel table, {}, and {}",
                    front.join(", "),
                    self.params[n - 1].phrase
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog is looked up by schema and name, and a name in a
    /// schema that does not hold it is not found rather than found
    /// somewhere else, which is the whole point of resolving against a
    /// catalog instead of against a list of words.
    #[test]
    fn a_procedure_is_found_by_its_schema_and_its_name() {
        assert_eq!(resolve(ROOT, "pagerank").map(|p| p.name), Some("pagerank"));
        assert_eq!(resolve(ROOT, "PageRank").map(|p| p.name), Some("pagerank"));
        assert!(resolve("/app", "pagerank").is_none());
        assert!(resolve(ROOT, "nonsense").is_none());
        assert_eq!(in_schema(ROOT).len(), CATALOG.len());
        assert!(in_schema("/app").is_empty());
    }

    /// What a procedure takes is a sentence built from the parameters,
    /// so a refusal reads the way somebody would have written it and
    /// there is one place to change when a parameter is added.
    #[test]
    fn what_a_procedure_takes_reads_as_a_sentence() {
        let takes = |name: &str| resolve(ROOT, name).expect(name).takes();
        assert_eq!(takes("pagerank"), "only the rel table");
        assert_eq!(takes("sssp"), "the rel table and a source node id");
        assert_eq!(
            takes("sssp_weighted"),
            "the rel table, a source node id, and the name of the weight column"
        );
        assert_eq!(takes("cdlp"), "the rel table and an optional round count");
        assert_eq!(
            resolve(ROOT, "cdlp").expect("cdlp").arity(),
            (0, 1),
            "an optional parameter is a range and not a count"
        );
        assert_eq!(resolve(ROOT, "sssp_weighted").expect("it").arity(), (2, 2));
    }
}
