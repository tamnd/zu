//! Repository automation. The binary is a thin argument parser over
//! these modules; they are a library so the tests and the bench can
//! drive the normalizer directly, without nightly and without cargo.

pub mod apimap;
pub mod artifacts;
pub mod corpus;
pub mod fixture;
pub mod matrix;
pub mod model;
pub mod package;
pub mod pins;
pub mod platforms;
pub mod repos;
pub mod rustdoc;
pub mod tarball;
pub mod terms;
pub mod toml;
