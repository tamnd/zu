//! The connection: one zu connection, and whatever statements share it.
//!
//! ADBC statements outlive the call that made them and are separate
//! objects, while a zu connection is `&mut` for the length of a query,
//! so the connection lives behind a lock and every statement holds a
//! handle to the same one. That is also what the ADBC specification
//! describes: several statements off one connection, and a driver free
//! to serialise them.
//!
//! Transactions are statements in GQL, so autocommit here is a flag and
//! three words. Turning it off runs `START TRANSACTION`; committing
//! runs `COMMIT` and starts the next one, which is what ADBC means by a
//! connection that stays out of autocommit.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use adbc_core::error::{Result, Status};
use adbc_core::options::{self, IsolationLevel, ObjectDepth, OptionConnection, OptionValue};
use adbc_core::schemas;
use arrow_array::{RecordBatch, RecordBatchIterator, RecordBatchReader, StringArray};
use arrow_schema::Schema;
use zudb::Interrupt;
use zudb::zu1::catalog::{Catalog, ROOT_SCHEMA};

use crate::db::{Database, flag};
use crate::error::{adbc, plain, unbuilt};
use crate::info;
use crate::stmt::Statement;

/// The two kinds of table a zu graph has.
///
/// Not `TABLE` and `VIEW`: a node table and a rel table are different
/// things with different shapes, and calling both of them a table would
/// lose the one fact a caller reading this wants.
const TABLE_TYPES: [&str; 2] = ["node", "rel"];

/// What a statement borrows to run: the connection, and whether an
/// explicit transaction is meant to be running on it.
pub(crate) struct Held {
    pub(crate) conn: zudb::Connection,
    pub(crate) autocommit: bool,
}

/// An ADBC connection over one zu connection.
pub struct Connection {
    pub(crate) held: Arc<Mutex<Held>>,
    /// The handle a cancel pulls, kept out of the lock on purpose: the
    /// thread that wants to stop a statement cannot be the thread
    /// holding the connection, because that one is inside the engine.
    pub(crate) interrupt: Interrupt,
    read_only: bool,
}

impl Connection {
    pub(crate) fn opened(
        db: &Database,
        opts: impl IntoIterator<Item = (OptionConnection, OptionValue)>,
    ) -> Result<Connection> {
        let conn = db.inner().connect().map_err(adbc)?;
        let interrupt = conn.interrupt();
        let read_only = conn.is_read_only();
        let mut connection = Connection {
            held: Arc::new(Mutex::new(Held {
                conn,
                autocommit: true,
            })),
            interrupt,
            read_only,
        };
        for (key, value) in opts {
            adbc_core::Optionable::set_option(&mut connection, key, value)?;
        }
        Ok(connection)
    }

    /// The connection, or the error a poisoned lock is.
    ///
    /// A lock is poisoned when a thread panicked holding it, which for
    /// a database connection means the engine's state is whatever the
    /// panic left. Handing it to the next caller would be worse than
    /// refusing.
    pub(crate) fn locked(held: &Arc<Mutex<Held>>) -> Result<MutexGuard<'_, Held>> {
        held.lock().map_err(|_| {
            plain(
                "this connection is not usable: a thread panicked while it held it, so what the \
                 engine's state is now is not known",
                Status::Internal,
            )
        })
    }

    /// One of the three transaction words, run for its effect.
    fn word(&mut self, word: &str) -> Result<()> {
        let mut held = Connection::locked(&self.held)?;
        held.conn.query(word).map_err(adbc)?;
        Ok(())
    }

    /// Ends the running transaction and starts the next one, which is
    /// what ADBC means: a connection out of autocommit is always in a
    /// transaction, and commit is where one ends and another begins.
    fn end(&mut self, word: &str) -> Result<()> {
        {
            let held = Connection::locked(&self.held)?;
            if held.autocommit {
                return Err(plain(
                    format!(
                        "there is no transaction to {}: this connection is in autocommit, where \
                         every statement is its own transaction. Set {} to false first",
                        word.to_lowercase(),
                        OptionConnection::AutoCommit.as_ref()
                    ),
                    Status::InvalidState,
                ));
            }
        }
        self.word(word)?;
        self.word("START TRANSACTION")
    }

    /// Turns autocommit on or off, which is starting or ending a
    /// transaction and nothing else.
    fn autocommit(&mut self, on: bool) -> Result<()> {
        let already = Connection::locked(&self.held)?.autocommit;
        if already == on {
            return Ok(());
        }
        // Leaving autocommit commits what is staged rather than
        // dropping it, which is what every driver does and what a
        // caller who turned autocommit back on is asking for.
        match on {
            true => self.word("COMMIT")?,
            false => self.word("START TRANSACTION")?,
        }
        Connection::locked(&self.held)?.autocommit = on;
        Ok(())
    }

    fn option(&self, key: &OptionConnection) -> Result<OptionValue> {
        match key {
            OptionConnection::AutoCommit => Ok(OptionValue::String(flag(
                Connection::locked(&self.held)?.autocommit,
            ))),
            OptionConnection::ReadOnly => Ok(OptionValue::String(flag(self.read_only))),
            // Every zu1 file has this schema and a graph nobody wrote a
            // path for belongs to it, so it is the answer and not a
            // placeholder.
            OptionConnection::CurrentSchema => Ok(OptionValue::String(ROOT_SCHEMA.to_string())),
            OptionConnection::IsolationLevel => Ok(OptionValue::String(String::from(
                IsolationLevel::Serializable,
            ))),
            OptionConnection::CurrentCatalog => Err(plain(
                "an embedded database is one file and has nothing above it to be a catalog",
                Status::NotFound,
            )),
            OptionConnection::Other(name) => Err(plain(
                format!("{name} is not an option this driver's connections have"),
                Status::NotFound,
            )),
            _ => Err(plain(
                format!(
                    "{} is not an option this driver's connections have",
                    key.as_ref()
                ),
                Status::NotFound,
            )),
        }
    }
}

/// A batch on its own, as the reader ADBC asks for.
fn reader(batch: RecordBatch) -> Box<dyn RecordBatchReader + Send + 'static> {
    let schema = batch.schema();
    Box::new(RecordBatchIterator::new([Ok(batch)], schema))
}

/// The names of the tables in a result, straight off the catalog.
///
/// A node value carries the id of its table and nothing else, so
/// [`zu_arrow`] asks for the name. Every client has written its own
/// copy of this map; here there is no copy at all, because the catalog
/// the connection already holds is the map, and a lookup by id is a
/// binary search over a few tens of entries rather than an allocation.
pub(crate) struct Named<'a>(pub(crate) &'a Catalog);

impl zu_arrow::Tables for Named<'_> {
    fn node(&self, id: u32) -> Option<&str> {
        self.0.node_by_id(id).map(|table| table.name.as_str())
    }

    fn rel(&self, id: u32) -> Option<&str> {
        self.0.rel_by_id(id).map(|table| table.name.as_str())
    }
}

impl adbc_core::Optionable for Connection {
    type Option = OptionConnection;

    fn set_option(&mut self, key: OptionConnection, value: OptionValue) -> Result<()> {
        match &key {
            OptionConnection::AutoCommit => self.autocommit(truth(&key, value)?),
            OptionConnection::ReadOnly => {
                let wanted = truth(&key, value)?;
                if wanted == self.read_only {
                    return Ok(());
                }
                Err(plain(
                    format!(
                        "whether writes are allowed is decided when the database is opened, not \
                         per connection: set zu.read_only to {} on the database instead",
                        flag(wanted)
                    ),
                    Status::NotImplemented,
                ))
            }
            OptionConnection::IsolationLevel => {
                let level = text(&key, value)?;
                // One writer at a time over an MVCC snapshot is
                // serializable, so that is the level and the only one.
                // Taking a weaker request and giving something stronger
                // would be within the standard's letter and would still
                // be a driver that ignored what it was told.
                match level == String::from(IsolationLevel::Serializable)
                    || level == String::from(IsolationLevel::Default)
                {
                    true => Ok(()),
                    false => Err(plain(
                        format!(
                            "{level:?} is not a level zu runs at: one writer at a time over a \
                             snapshot is serializable, which is the only one there is"
                        ),
                        Status::NotImplemented,
                    )),
                }
            }
            OptionConnection::CurrentCatalog | OptionConnection::CurrentSchema => Err(plain(
                format!(
                    "{} is not something a connection moves between yet: a zu1 file has one \
                     schema, {ROOT_SCHEMA}, and a statement names the graph it wants",
                    key.as_ref()
                ),
                Status::NotImplemented,
            )),
            OptionConnection::Other(name) => Err(plain(
                format!("{name} is not an option this driver's connections have"),
                Status::NotFound,
            )),
            _ => Err(plain(
                format!(
                    "{} is not an option this driver's connections have",
                    key.as_ref()
                ),
                Status::NotFound,
            )),
        }
    }

    fn get_option_string(&self, key: OptionConnection) -> Result<String> {
        match self.option(&key)? {
            OptionValue::String(text) => Ok(text),
            OptionValue::Int(number) => Ok(number.to_string()),
            _ => unreachable!("every connection option reads as a string"),
        }
    }

    fn get_option_bytes(&self, key: OptionConnection) -> Result<Vec<u8>> {
        self.get_option_string(key).map(String::into_bytes)
    }

    fn get_option_int(&self, key: OptionConnection) -> Result<i64> {
        Err(plain(
            format!("{} is not a number", key.as_ref()),
            Status::NotFound,
        ))
    }

    fn get_option_double(&self, key: OptionConnection) -> Result<f64> {
        Err(plain(
            format!("{} is not a number", key.as_ref()),
            Status::NotFound,
        ))
    }
}

impl adbc_core::Connection for Connection {
    type StatementType = Statement;

    fn new_statement(&mut self) -> Result<Statement> {
        Ok(Statement::on(
            Arc::clone(&self.held),
            self.interrupt.clone(),
        ))
    }

    /// Asks whatever is running to stop.
    ///
    /// The handle is outside the lock, so this returns at once and does
    /// not wait for the statement: what it does is set the flag the
    /// executor checks at its next boundary, and the thread inside the
    /// engine is the one that raises.
    fn cancel(&mut self) -> Result<()> {
        self.interrupt.stop();
        Ok(())
    }

    fn get_info(
        &self,
        codes: Option<HashSet<options::InfoCode>>,
    ) -> Result<Box<dyn RecordBatchReader + Send + 'static>> {
        Ok(reader(info::batch(codes)?))
    }

    fn get_objects(
        &self,
        _depth: ObjectDepth,
        _catalog: Option<&str>,
        _db_schema: Option<&str>,
        _table_name: Option<&str>,
        _table_type: Option<Vec<&str>>,
        _column_name: Option<&str>,
    ) -> Result<Box<dyn RecordBatchReader + Send + 'static>> {
        Err(unbuilt(NO_COLUMNS))
    }

    fn get_table_schema(
        &self,
        _catalog: Option<&str>,
        _db_schema: Option<&str>,
        _table_name: &str,
    ) -> Result<Schema> {
        Err(unbuilt(NO_COLUMNS))
    }

    fn get_table_types(&self) -> Result<Box<dyn RecordBatchReader + Send + 'static>> {
        let schema = schemas::GET_TABLE_TYPES_SCHEMA.clone();
        let types = StringArray::from(TABLE_TYPES.to_vec());
        let batch = RecordBatch::try_new(schema, vec![Arc::new(types)]).map_err(arrow)?;
        Ok(reader(batch))
    }

    /// None, which is the whole truth: every statistic this driver
    /// reports is one ADBC already has a number for.
    fn get_statistic_names(&self) -> Result<Box<dyn RecordBatchReader + Send + 'static>> {
        let schema = schemas::GET_STATISTIC_NAMES_SCHEMA.clone();
        let batch = RecordBatch::new_empty(schema);
        Ok(reader(batch))
    }

    fn get_statistics(
        &self,
        _catalog: Option<&str>,
        _db_schema: Option<&str>,
        _table_name: Option<&str>,
        _approximate: bool,
    ) -> Result<Box<dyn RecordBatchReader + Send + 'static>> {
        Err(unbuilt(
            "reporting a table's distribution is something the catalog keeps and this driver does \
             not publish",
        ))
    }

    fn commit(&mut self) -> Result<()> {
        self.end("COMMIT")
    }

    fn rollback(&mut self) -> Result<()> {
        self.end("ROLLBACK")
    }

    fn read_partition(
        &self,
        _partition: impl AsRef<[u8]>,
    ) -> Result<Box<dyn RecordBatchReader + Send + 'static>> {
        Err(unbuilt(PARTITIONS))
    }
}

/// The one reason `get_objects` and `get_table_schema` have no answer,
/// said once so that both of them say the same thing.
const NO_COLUMNS: &str = "listing a table's property columns is something the catalog does not \
     publish yet, so neither call can name them and neither will guess";

pub(crate) const PARTITIONS: &str = "reading a result in partitions is for a database spread over \
     several machines, and an embedded one is a file in this process";

fn arrow(err: arrow_schema::ArrowError) -> adbc_core::error::Error {
    plain(
        format!("arrow could not build the result: {err}"),
        Status::Internal,
    )
}

fn text(key: &OptionConnection, value: OptionValue) -> Result<String> {
    match value {
        OptionValue::String(text) => Ok(text),
        _ => Err(plain(
            format!("{} takes a string", key.as_ref()),
            Status::InvalidArguments,
        )),
    }
}

fn truth(key: &OptionConnection, value: OptionValue) -> Result<bool> {
    match value {
        OptionValue::Int(number) => Ok(number != 0),
        OptionValue::String(text)
            if text == adbc_core::constants::ADBC_OPTION_VALUE_ENABLED
                || text == adbc_core::constants::ADBC_OPTION_VALUE_DISABLED =>
        {
            Ok(text == adbc_core::constants::ADBC_OPTION_VALUE_ENABLED)
        }
        other => Err(plain(
            format!("{} is true or false, and was given {other:?}", key.as_ref()),
            Status::InvalidArguments,
        )),
    }
}

#[cfg(test)]
mod tests {
    use adbc_core::{Connection as _, Database as _, Driver as _, Optionable as _, Statement as _};

    use super::*;
    use crate::Driver;

    fn connected() -> Connection {
        let mut driver = Driver;
        let db = driver.new_database().expect("a database in memory");
        db.new_connection().expect("a connection to it")
    }

    #[test]
    fn a_connection_starts_in_autocommit() {
        let conn = connected();
        assert_eq!(
            conn.get_option_string(OptionConnection::AutoCommit)
                .unwrap(),
            "true"
        );
    }

    #[test]
    fn committing_in_autocommit_says_what_to_do_instead() {
        let mut conn = connected();
        let err = conn.commit().expect_err("nothing to commit");
        assert_eq!(err.status, Status::InvalidState);
        assert!(err.message.contains("autocommit"), "{}", err.message);
    }

    #[test]
    fn autocommit_off_and_on_is_a_transaction() {
        let mut conn = connected();
        conn.set_option(OptionConnection::AutoCommit, "false".into())
            .expect("a transaction starts");
        assert_eq!(
            conn.get_option_string(OptionConnection::AutoCommit)
                .unwrap(),
            "false"
        );
        conn.commit().expect("and commits, and starts the next");
        conn.set_option(OptionConnection::AutoCommit, "true".into())
            .expect("and ends");
    }

    #[test]
    fn a_second_start_is_not_a_second_transaction() {
        let mut conn = connected();
        conn.set_option(OptionConnection::AutoCommit, "false".into())
            .expect("a transaction starts");
        conn.set_option(OptionConnection::AutoCommit, "false".into())
            .expect("saying so again does nothing");
    }

    #[test]
    fn the_table_types_are_the_two_a_graph_has() {
        let conn = connected();
        let batches: Vec<_> = conn
            .get_table_types()
            .expect("a driver knows its own table types")
            .collect::<std::result::Result<_, _>>()
            .expect("and they read");
        let names: Vec<String> = batches
            .iter()
            .flat_map(|batch| {
                batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("utf8")
                    .iter()
                    .map(|name| name.expect("not null").to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(names, TABLE_TYPES);
    }

    #[test]
    fn what_is_not_built_says_what_it_was() {
        let conn = connected();
        let err = conn
            .get_table_schema(None, None, "person")
            .expect_err("the columns are not published");
        assert_eq!(err.status, Status::NotImplemented);
        assert!(err.message.contains("property columns"), "{}", err.message);
    }

    #[test]
    fn a_cancel_stops_the_next_statement() {
        let mut conn = connected();
        conn.cancel().expect("a cancel is always taken");
        let mut stmt = conn.new_statement().expect("a statement");
        stmt.set_sql_query("MATCH (n) RETURN n").expect("set");
        let err = stmt.execute().err().expect("it was cancelled");
        assert_eq!(err.status, Status::Cancelled);
        // And the flag does not stay pulled: the next one runs.
        stmt.set_sql_query("RETURN 1 AS one").expect("set");
        stmt.execute().expect("the cancel was for one statement");
    }
}
