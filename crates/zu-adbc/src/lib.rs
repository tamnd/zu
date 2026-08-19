//! zu as an ADBC driver: a statement in, an Arrow stream out.
//!
//! ADBC is the one database interface that hands back Arrow rather than
//! rows, which is the same thing [`zu_arrow`] already builds for the
//! Python and JavaScript clients. So this crate is thin on purpose: it
//! is the ADBC vocabulary spelled over [`zudb::Database`] and
//! [`zudb::Connection`], and the columns it returns are the columns the
//! engine's sink already filled.
//!
//! What that buys is everything that speaks ADBC and nothing else that
//! had to be written. A driver manager loads the `cdylib` this crate
//! builds, and Python, R, Java, Go and C++ read a zu result
//! without a client library per language and without a row ever being
//! built.
//!
//! ## The C ABI is not written here either
//!
//! The forty function pointers of `AdbcDriver` are laid out by
//! `adbc_ffi`, out of Apache's own tree, and this crate implements the
//! four safe traits behind them: [`Driver`], [`Database`],
//! [`Connection`] and [`Statement`]. That division is deliberate. A
//! field written into the wrong slot of that struct is a jump through a
//! wrong pointer, which is a segfault in the caller's process and not a
//! failed test here, and it is a layout the ADBC project maintains
//! anyway.
//!
//! ## What v0 does and what it refuses
//!
//! Executing a statement, in a transaction or out of one, and reading
//! the result: that is the whole of what a caller does and it is all
//! here. The metadata calls are split. `get_info` and
//! `get_table_types` answer for real. `get_objects` and
//! `get_table_schema` refuse, because they have to name a table's
//! property columns and the engine does not publish those through the
//! catalog yet; a GUI's schema tree is what that will buy and it is the
//! first thing v1 owes.
//!
//! Bulk ingest, bound parameters, Substrait plans and partitioned reads
//! refuse as well, each saying which one of them it was. A refusal here
//! is [`adbc_core::error::Status::NotImplemented`] with a sentence, not
//! a null pointer.
//!
//! ## Which options it takes
//!
//! On the database, before init:
//!
//! | Key | What |
//! | --- | --- |
//! | `uri` | `zu:`, `file:` or a bare path. `:memory:` for one that never touches the filesystem |
//! | `path` | the same, spelled as a path, for a caller with no URI to give |
//! | `zu.read_only` | `true` to open without the write side |
//! | `zu.threads` | how many threads a statement may use |
//! | `zu.memory_limit` | bytes |
//!
//! A path that is not there is created, which is what every zu client
//! does and what a caller pointing an ADBC tool at a new file means.
//! Read-only is the exception: nothing creates a database it may not
//! write to.
//!
//! On the connection, `adbc.connection.autocommit` and
//! `adbc.connection.readonly`, both as ADBC spells them.

use adbc_core::error::Result;
use adbc_core::options::{OptionDatabase, OptionValue};

mod conn;
mod db;
pub mod error;
mod info;
mod stmt;

pub use conn::Connection;
pub use db::Database;
pub use stmt::Statement;

/// The driver itself, which holds nothing.
///
/// Everything a zu database needs is in the options a caller sets
/// before init, so there is no per-driver state to keep and no reason
/// for two of these to differ.
#[derive(Debug, Default, Clone, Copy)]
pub struct Driver;

impl adbc_core::Driver for Driver {
    type DatabaseType = Database;

    fn new_database(&mut self) -> Result<Database> {
        self.new_database_with_opts([])
    }

    fn new_database_with_opts(
        &mut self,
        opts: impl IntoIterator<Item = (OptionDatabase, OptionValue)>,
    ) -> Result<Database> {
        Database::opened(opts)
    }
}

// The name a driver manager looks for first, and `AdbcDriverInit` after
// it, which the macro emits too.
adbc_ffi::export_driver!(ZuDriverInit, Driver);
