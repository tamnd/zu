//! Query engine: hand-written zuQL parser, binder, cost-based optimizer,
//! and the factorized vectorized executor with a morsel scheduler.
//! Language and operator set are specified in `docs/07-query-engine.md`.
//! Implementation lands in M2 (core) and M4 (recursion, WCOJ).
//!
//! What exists today: the zuQL frontend (`lexer`, `ast`, `parser`,
//! `binder`) for the MATCH, WHERE, UNWIND, WITH, RETURN core per
//! `docs/grammar.ebnf`, the CSR adjacency in `csr`, and the
//! table-function kernels in `kernels`.

pub mod ast;
pub mod binder;
pub mod csr;
pub mod kernels;
pub mod lexer;
pub mod parser;
