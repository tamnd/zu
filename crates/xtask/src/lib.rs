//! Repository automation. The binary is a thin argument parser over
//! these modules; they are a library so the tests and the bench can
//! drive the normalizer directly, without nightly and without cargo.

pub mod apimap;
pub mod corpus;
pub mod fixture;
pub mod model;
pub mod rustdoc;
pub mod toml;
