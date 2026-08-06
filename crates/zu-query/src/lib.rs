//! Query engine: hand-written zuQL parser, binder, cost-based optimizer,
//! and the factorized vectorized executor with a morsel scheduler.
//! Language and operator set are specified in `docs/07-query-engine.md`.
//! Implementation lands in M2 (core) and M4 (recursion, WCOJ).
//!
//! What exists today is the in-memory graph kernel slice: the CSR
//! adjacency in `csr` and the table-function kernels in `kernels`.

pub mod csr;
pub mod kernels;
