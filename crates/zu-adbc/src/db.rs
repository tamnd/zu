//! The database: where the file is, and how it was opened.
//!
//! ADBC splits a connection in two, and the split is the same one zu
//! makes. A database is the shared, cheap, `Send + Sync` thing that
//! knows a path and a configuration; a connection is the expensive
//! thing with a file handle and a plan cache. So this is a
//! [`zudb::Database`] and the options it was built from, and nothing
//! else.
//!
//! The options are read once, here, and turned into a
//! [`zudb::Config`]. They are kept beside it as well, because ADBC lets
//! a caller read an option back and a `Config` does not say what it was
//! given.

use std::path::PathBuf;

use adbc_core::error::{Result, Status};
use adbc_core::options::{OptionConnection, OptionDatabase, OptionValue};

use crate::conn::Connection;
use crate::error::{adbc, plain};

/// Where a database with no URI at all lives, which is nowhere.
///
/// A caller that loads the driver and connects without saying where is
/// asking for a scratch database, the way every embedded engine reads
/// it, and a fresh one in memory is that. Saying so is better than an
/// error, because the first thing anybody does with a new driver is
/// exactly this.
const MEMORY: &str = ":memory:";

/// The driver-specific option keys, all under one prefix so that a
/// caller reading a config file can tell ours from ADBC's own.
const READ_ONLY: &str = "zu.read_only";
const THREADS: &str = "zu.threads";
const MEMORY_LIMIT: &str = "zu.memory_limit";

/// An open zu database, with the options it was opened from.
pub struct Database {
    db: zudb::Database,
    uri: String,
    read_only: bool,
    threads: Option<i64>,
    memory_limit: Option<i64>,
}

/// Where the URI pointed.
enum Place {
    Memory,
    Path(PathBuf),
}

impl Database {
    /// Reads the options, opens what they name, and keeps both.
    ///
    /// Opening here rather than at the first connection is what makes a
    /// wrong path fail where the caller is looking: ADBC's init is the
    /// call a driver manager reports on, and a database that opened is
    /// one every connection off it will open too.
    pub(crate) fn opened(
        opts: impl IntoIterator<Item = (OptionDatabase, OptionValue)>,
    ) -> Result<Database> {
        let mut db = Database {
            db: zudb::Database::memory().map_err(adbc)?,
            uri: MEMORY.to_string(),
            read_only: false,
            threads: None,
            memory_limit: None,
        };
        // Every option is applied to the fields before anything is
        // opened, because `zu.read_only` decides whether a path that is
        // not there gets created and it may arrive after the URI.
        for (key, value) in opts {
            db.take(key, value)?;
        }
        db.db = open(&db)?;
        Ok(db)
    }

    /// One option onto the fields. Not the opening: that is once, after
    /// all of them.
    fn take(&mut self, key: OptionDatabase, value: OptionValue) -> Result<()> {
        match &key {
            OptionDatabase::Uri => self.uri = text(&key, value)?,
            OptionDatabase::Username | OptionDatabase::Password => {
                return Err(plain(
                    format!(
                        "{} is not a thing an embedded database has: zu runs in this process and \
                         the file's permissions are the only ones there are",
                        key.as_ref()
                    ),
                    Status::NotImplemented,
                ));
            }
            OptionDatabase::Other(name) => match name.as_str() {
                // The spelling every embedded driver takes, for a
                // caller that has a path and no URI scheme to wrap it
                // in.
                "path" => self.uri = text(&key, value)?,
                READ_ONLY => self.read_only = truth(&key, value)?,
                THREADS => self.threads = Some(count(&key, value)?),
                MEMORY_LIMIT => self.memory_limit = Some(count(&key, value)?),
                _ => return Err(unknown(name)),
            },
            // The enum is non-exhaustive, so a key a later ADBC adds
            // lands here rather than being taken and ignored.
            _ => return Err(unknown(key.as_ref())),
        }
        Ok(())
    }

    /// The zu database underneath, for the connections.
    pub(crate) fn inner(&self) -> &zudb::Database {
        &self.db
    }

    fn option(&self, key: &OptionDatabase) -> Result<OptionValue> {
        match key {
            OptionDatabase::Uri => Ok(OptionValue::String(self.uri.clone())),
            OptionDatabase::Other(name) => match name.as_str() {
                "path" => Ok(OptionValue::String(self.uri.clone())),
                READ_ONLY => Ok(OptionValue::String(flag(self.read_only))),
                THREADS => self.set(name, self.threads),
                MEMORY_LIMIT => self.set(name, self.memory_limit),
                _ => Err(unknown(name)),
            },
            _ => Err(unknown(key.as_ref())),
        }
    }

    /// An option that has a value only if somebody set one, since a
    /// default the engine picks is not a number this crate can name.
    fn set(&self, name: &str, value: Option<i64>) -> Result<OptionValue> {
        value.map(OptionValue::Int).ok_or_else(|| {
            plain(
                format!("{name} was not set, so the engine picks it and there is no value to read"),
                Status::NotFound,
            )
        })
    }
}

/// Opens what the URI names, creating it if it is a path that is not
/// there.
///
/// Create-if-missing is what every zu client does and what a caller
/// pointing a tool at a new file means. Read-only is the one case that
/// does not: a database created with nothing allowed to write to it
/// stays empty forever, and `zudb` refuses it for the same reason.
fn open(db: &Database) -> Result<zudb::Database> {
    let mut config = zudb::Config::new().read_only(db.read_only);
    if let Some(threads) = db.threads {
        config = config.threads(size(THREADS, threads)?);
    }
    if let Some(bytes) = db.memory_limit {
        config = config.memory_limit(size(MEMORY_LIMIT, bytes)?);
    }
    match place(&db.uri) {
        Place::Memory if db.read_only => Err(plain(
            format!(
                "a database in memory opened read-only is one nothing could ever put a row in: \
                 drop {READ_ONLY}, or give a path to a database that already has rows"
            ),
            Status::InvalidArguments,
        )),
        Place::Memory => zudb::Database::memory_with(config).map_err(adbc),
        Place::Path(path) if !db.read_only && !path.exists() => {
            zudb::Database::create_with(path, config).map_err(adbc)
        }
        Place::Path(path) => zudb::Database::open_with(path, config).map_err(adbc),
    }
}

/// The URI as a place.
///
/// Three spellings reach the same file, because three kinds of caller
/// write them: `zu:` is this driver's own scheme, `file:` is the one a
/// URI library produces, and a bare path is what somebody typing into a
/// config file writes. Everything after the scheme is the path, which
/// is why there is no parser here: a zu database is a file and not a
/// host, a port and a query string.
fn place(uri: &str) -> Place {
    let rest = uri
        .strip_prefix("zu://")
        .or_else(|| uri.strip_prefix("zu:"))
        .or_else(|| uri.strip_prefix("file://"))
        .or_else(|| uri.strip_prefix("file:"))
        .unwrap_or(uri);
    match rest {
        "" | MEMORY | "memory:" => Place::Memory,
        path => Place::Path(PathBuf::from(path)),
    }
}

/// A count as the engine takes it, which is a `usize`.
fn size(name: &str, value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        plain(
            format!("{name} was {value}, and a count of things is not negative"),
            Status::InvalidArguments,
        )
    })
}

/// A string option, whatever kind of value carried it.
fn text(key: &OptionDatabase, value: OptionValue) -> Result<String> {
    match value {
        OptionValue::String(text) => Ok(text),
        other => Err(kind(key.as_ref(), &other, "a string")),
    }
}

/// A count option. ADBC lets a caller set an integer as an integer or
/// as its digits, and both mean the same number, so both are taken.
fn count(key: &OptionDatabase, value: OptionValue) -> Result<i64> {
    match value {
        OptionValue::Int(number) => Ok(number),
        OptionValue::String(text) => text.parse().map_err(|_| {
            plain(
                format!("{} was {text:?}, which is not a number", key.as_ref()),
                Status::InvalidArguments,
            )
        }),
        other => Err(kind(key.as_ref(), &other, "a number")),
    }
}

/// A truth option, spelled the way ADBC spells one.
fn truth(key: &OptionDatabase, value: OptionValue) -> Result<bool> {
    let text = match value {
        OptionValue::String(text) => text,
        OptionValue::Int(number) => return Ok(number != 0),
        other => return Err(kind(key.as_ref(), &other, "true or false")),
    };
    match text.as_str() {
        adbc_core::constants::ADBC_OPTION_VALUE_ENABLED => Ok(true),
        adbc_core::constants::ADBC_OPTION_VALUE_DISABLED => Ok(false),
        other => Err(plain(
            format!(
                "{} was {other:?}, and the two things it can be are \"true\" and \"false\"",
                key.as_ref()
            ),
            Status::InvalidArguments,
        )),
    }
}

fn kind(key: &str, value: &OptionValue, wanted: &str) -> adbc_core::error::Error {
    let got = match value {
        OptionValue::String(_) => "a string",
        OptionValue::Bytes(_) => "bytes",
        OptionValue::Int(_) => "a number",
        OptionValue::Double(_) => "a float",
        _ => "something this driver has no name for",
    };
    plain(
        format!("{key} takes {wanted} and was given {got}"),
        Status::InvalidArguments,
    )
}

/// An option nobody here has heard of.
///
/// Naming the ones that exist is the point: a caller who wrote
/// `zu.readonly` gets to see `zu.read_only` in the message rather than
/// going to look for it.
fn unknown(name: &str) -> adbc_core::error::Error {
    plain(
        format!(
            "{name} is not an option this driver has: the database takes uri, path, {READ_ONLY}, \
             {THREADS} and {MEMORY_LIMIT}"
        ),
        Status::NotFound,
    )
}

/// A truth as ADBC writes one back.
pub(crate) fn flag(value: bool) -> String {
    match value {
        true => adbc_core::constants::ADBC_OPTION_VALUE_ENABLED.to_string(),
        false => adbc_core::constants::ADBC_OPTION_VALUE_DISABLED.to_string(),
    }
}

impl adbc_core::Optionable for Database {
    type Option = OptionDatabase;

    /// Nothing is settable after init, and the message says which call
    /// takes it instead.
    ///
    /// Every option this database has decides how the file was opened,
    /// and the file is open by the time anything can call this. A
    /// driver that quietly kept the new value and went on using the old
    /// one would be lying about its own state.
    fn set_option(&mut self, key: OptionDatabase, _value: OptionValue) -> Result<()> {
        Err(plain(
            format!(
                "{} is what the database was opened with and cannot change after: set it before \
                 AdbcDatabaseInit, or open a second database",
                key.as_ref()
            ),
            Status::InvalidState,
        ))
    }

    fn get_option_string(&self, key: OptionDatabase) -> Result<String> {
        match self.option(&key)? {
            OptionValue::String(text) => Ok(text),
            OptionValue::Int(number) => Ok(number.to_string()),
            _ => Err(kind(
                key.as_ref(),
                &OptionValue::Bytes(Vec::new()),
                "a string",
            )),
        }
    }

    fn get_option_bytes(&self, key: OptionDatabase) -> Result<Vec<u8>> {
        self.get_option_string(key).map(String::into_bytes)
    }

    fn get_option_int(&self, key: OptionDatabase) -> Result<i64> {
        match self.option(&key)? {
            OptionValue::Int(number) => Ok(number),
            other => Err(kind(key.as_ref(), &other, "a number")),
        }
    }

    fn get_option_double(&self, key: OptionDatabase) -> Result<f64> {
        self.get_option_int(key).map(|number| number as f64)
    }
}

impl adbc_core::Database for Database {
    type ConnectionType = Connection;

    fn new_connection(&self) -> Result<Connection> {
        self.new_connection_with_opts([])
    }

    fn new_connection_with_opts(
        &self,
        opts: impl IntoIterator<Item = (OptionConnection, OptionValue)>,
    ) -> Result<Connection> {
        Connection::opened(self, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adbc_core::Optionable;

    #[test]
    fn nothing_named_is_a_database_in_memory() {
        let db = Database::opened([]).expect("a database with no options opens");
        assert!(db.inner().is_memory());
        assert_eq!(db.get_option_string(OptionDatabase::Uri).unwrap(), MEMORY);
    }

    #[test]
    fn a_path_that_is_not_there_is_created() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("fresh.zu");
        let db = Database::opened([(
            OptionDatabase::Other("path".into()),
            OptionValue::String(path.display().to_string()),
        )])
        .expect("a path that is not there is created");
        assert!(!db.inner().is_memory());
        assert!(path.exists(), "the file is on disk after the open");
    }

    #[test]
    fn a_read_only_open_of_nothing_says_why_not() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("absent.zu");
        let err = Database::opened([
            (
                OptionDatabase::Other("path".into()),
                OptionValue::String(path.display().to_string()),
            ),
            (
                OptionDatabase::Other(READ_ONLY.into()),
                OptionValue::String("true".into()),
            ),
        ])
        .err()
        .expect("there is nothing there to read");
        assert_eq!(err.status, Status::IO, "{}", err.message);
    }

    #[test]
    fn the_three_spellings_reach_the_same_file() {
        for uri in ["zu:/tmp/x.zu", "zu:///tmp/x.zu", "file:///tmp/x.zu"] {
            match place(uri) {
                Place::Path(path) => assert_eq!(path, PathBuf::from("/tmp/x.zu"), "{uri}"),
                Place::Memory => panic!("{uri} is a path"),
            }
        }
        for uri in [":memory:", "zu::memory:", ""] {
            assert!(matches!(place(uri), Place::Memory), "{uri}");
        }
    }

    #[test]
    fn an_option_nobody_has_heard_of_names_the_ones_that_exist() {
        let err = Database::opened([(
            OptionDatabase::Other("zu.readonly".into()),
            OptionValue::String("true".into()),
        )])
        .err()
        .expect("that is not the spelling");
        assert!(err.message.contains(READ_ONLY), "{}", err.message);
    }

    #[test]
    fn an_option_cannot_change_after_the_file_is_open() {
        let mut db = Database::opened([]).expect("a database in memory");
        let err = db
            .set_option(OptionDatabase::Uri, OptionValue::String("other".into()))
            .expect_err("the file is already open");
        assert_eq!(err.status, Status::InvalidState);
    }
}
