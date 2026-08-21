//! The statement: the text to run, and the result as Arrow.
//!
//! This is where a caller's query meets [`zu_arrow`], and the meeting
//! is short. The engine's sink fills columns, `zu_arrow` puts a type
//! and a validity bitmap around the buffers it filled, and the batches
//! that come out are slices of those arrays. Nothing here builds a row,
//! and on a plain projection nothing here copies a value.
//!
//! A statement holds the connection, so two statements off one
//! connection take turns. That is what the ADBC specification allows
//! and what an embedded engine wants anyway: a second thread inside one
//! connection's caches would cost more than it bought.

use std::sync::{Arc, Mutex};

use adbc_core::PartitionedResult;
use adbc_core::error::{Result, Status};
use adbc_core::options::{OptionStatement, OptionValue};
use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_schema::{DataType, Field, Schema};
use zudb::Interrupt;

use crate::conn::{Connection, Held, Named, PARTITIONS};
use crate::error::{adbc, plain, unbuilt};

/// How many rows go in a batch, which is [`zu_arrow::BATCH`] unless a
/// caller says otherwise through `zu.rows_per_batch`.
const ROWS_PER_BATCH: &str = "zu.rows_per_batch";

/// One statement: its text, its prepared handle if it has one, and the
/// connection it runs on.
pub struct Statement {
    held: Arc<Mutex<Held>>,
    interrupt: Interrupt,
    sql: Option<String>,
    /// The handle the engine gave for this text, and the parameter
    /// names it found in it. Present only after [`Statement::prepare`].
    prepared: Option<(u64, Vec<String>)>,
    rows_per_batch: usize,
}

impl Statement {
    pub(crate) fn on(held: Arc<Mutex<Held>>, interrupt: Interrupt) -> Statement {
        Statement {
            held,
            interrupt,
            sql: None,
            prepared: None,
            rows_per_batch: zu_arrow::BATCH,
        }
    }

    /// The text, or the error a statement nobody gave one is.
    fn text(&self) -> Result<&str> {
        self.sql.as_deref().ok_or_else(|| {
            plain(
                "this statement has nothing to run: set a query on it first",
                Status::InvalidState,
            )
        })
    }

    /// Whether somebody cancelled, and spending the cancel by asking.
    ///
    /// A cancel that arrived with nothing running lands on the next
    /// statement rather than on nothing at all, so that a caller who
    /// cancelled is told and does not read a result it no longer
    /// wanted. Clearing it here is what keeps that to one statement:
    /// the flag is a message about one call and not a state the
    /// connection stays in.
    fn cancelled(&self) -> Result<()> {
        if self.interrupt.stopped() {
            self.interrupt.clear();
            return Err(adbc(zudb::ZuError::Interrupted));
        }
        Ok(())
    }

    /// Runs it and turns the result into Arrow, whole.
    ///
    /// The arrays are built here rather than as the reader pulls,
    /// because a column of the wrong type has to be refused while there
    /// is still a call to refuse it in. Cutting them into batches
    /// afterwards is a slice.
    ///
    /// The result is taken and not borrowed, which is what makes the
    /// arrays the engine's own buffers rather than a copy of them:
    /// nothing here is going to read the result again, and a driver
    /// whose whole job is to hand the answer to somebody else should not
    /// memcpy it on the way past.
    fn table(&mut self) -> Result<zu_arrow::Table> {
        self.cancelled()?;
        let sql = self.text()?.to_string();
        let prepared = self.prepared.as_ref().map(|(handle, _)| *handle);
        let mut held = Connection::locked(&self.held)?;
        let ran = match prepared {
            Some(handle) => held.conn.execute_prepared(handle, &[]),
            None => held.conn.query(&sql),
        };
        // Cleared either way: the flag is about the statement that just
        // ran and the next one starts with nobody having cancelled it.
        self.interrupt.clear();
        let result = ran.map_err(adbc)?;
        let catalog = held.conn.session_mut().catalog();
        zu_arrow::Table::taken(result, &Named(catalog)).map_err(translation)
    }
}

/// Whatever the translation into Arrow refused, as ADBC says it.
///
/// A column of two types is the caller's query and not the engine's
/// fault, which is why the first two are [`Status::InvalidData`] rather
/// than [`Status::Internal`].
fn translation(err: zu_arrow::Error) -> adbc_core::error::Error {
    let status = match err {
        zu_arrow::Error::Type(_) | zu_arrow::Error::Value(_) => Status::InvalidData,
        zu_arrow::Error::Arrow(_) => Status::Internal,
    };
    plain(err.to_string(), status)
}

impl adbc_core::Optionable for Statement {
    type Option = OptionStatement;

    fn set_option(&mut self, key: OptionStatement, value: OptionValue) -> Result<()> {
        match &key {
            OptionStatement::Other(name) if name == ROWS_PER_BATCH => {
                let rows = match value {
                    OptionValue::Int(rows) => rows,
                    OptionValue::String(text) => text.parse().map_err(|_| {
                        plain(
                            format!("{ROWS_PER_BATCH} was {text:?}, which is not a number"),
                            Status::InvalidArguments,
                        )
                    })?,
                    _ => {
                        return Err(plain(
                            format!("{ROWS_PER_BATCH} is a number"),
                            Status::InvalidArguments,
                        ));
                    }
                };
                if rows < 1 {
                    return Err(plain(
                        format!(
                            "{ROWS_PER_BATCH} has to be at least one, because a reader cutting a \
                             result into batches of no rows never reaches the end of it"
                        ),
                        Status::InvalidArguments,
                    ));
                }
                self.rows_per_batch = rows as usize;
                Ok(())
            }
            OptionStatement::IngestMode
            | OptionStatement::TargetTable
            | OptionStatement::TargetCatalog
            | OptionStatement::TargetDbSchema
            | OptionStatement::Temporary => Err(unbuilt(INGEST)),
            OptionStatement::Incremental => Err(unbuilt(PARTITIONS)),
            _ => Err(plain(
                format!(
                    "{} is not an option this driver's statements have: the one they take is \
                     {ROWS_PER_BATCH}",
                    key.as_ref()
                ),
                Status::NotFound,
            )),
        }
    }

    fn get_option_string(&self, key: OptionStatement) -> Result<String> {
        self.get_option_int(key).map(|rows| rows.to_string())
    }

    fn get_option_bytes(&self, key: OptionStatement) -> Result<Vec<u8>> {
        self.get_option_string(key).map(String::into_bytes)
    }

    fn get_option_int(&self, key: OptionStatement) -> Result<i64> {
        match &key {
            OptionStatement::Other(name) if name == ROWS_PER_BATCH => {
                Ok(self.rows_per_batch as i64)
            }
            _ => Err(plain(
                format!(
                    "{} is not an option this driver's statements have",
                    key.as_ref()
                ),
                Status::NotFound,
            )),
        }
    }

    fn get_option_double(&self, key: OptionStatement) -> Result<f64> {
        self.get_option_int(key).map(|rows| rows as f64)
    }
}

impl adbc_core::Statement for Statement {
    fn bind(&mut self, _batch: RecordBatch) -> Result<()> {
        Err(unbuilt(BIND))
    }

    fn bind_stream(&mut self, _reader: Box<dyn RecordBatchReader + Send>) -> Result<()> {
        Err(unbuilt(BIND))
    }

    fn execute(&mut self) -> Result<Box<dyn RecordBatchReader + Send + 'static>> {
        let rows = self.rows_per_batch;
        Ok(Box::new(self.table()?.batches(rows)))
    }

    /// The rows the statement changed.
    ///
    /// A statement that returns rows instead answers `None` rather than
    /// zero, which is what ADBC reserves for "the driver cannot say":
    /// zero would read as a write that changed nothing.
    fn execute_update(&mut self) -> Result<Option<i64>> {
        self.cancelled()?;
        let sql = self.text()?.to_string();
        let mut held = Connection::locked(&self.held)?;
        let ran = held.conn.execute(&sql);
        self.interrupt.clear();
        Ok(Some(ran.map_err(adbc)? as i64))
    }

    /// The columns the statement would return, learned by running it
    /// and unmaking whatever it wrote.
    ///
    /// zu does not publish a statement's output schema without running
    /// it, so this runs it. What keeps that honest is the transaction
    /// around it: a statement that only reads costs one extra read, and
    /// a statement that writes has its writes rolled back, so calling
    /// this and then executing is one insert and not two.
    ///
    /// Inside a transaction the caller already started there is nothing
    /// to roll back to, so this refuses rather than ending that
    /// transaction on the caller's behalf.
    fn execute_schema(&mut self) -> Result<Schema> {
        if !Connection::locked(&self.held)?.autocommit {
            return Err(plain(
                "asking what a statement returns runs it and rolls it back, and there is no \
                 savepoint inside the transaction you started to roll back to: ask outside one",
                Status::InvalidState,
            ));
        }
        {
            let mut held = Connection::locked(&self.held)?;
            held.conn.query("START TRANSACTION").map_err(adbc)?;
        }
        let table = self.table();
        {
            let mut held = Connection::locked(&self.held)?;
            held.conn.query("ROLLBACK").map_err(adbc)?;
        }
        Ok(Schema::clone(&table?.schema()))
    }

    fn execute_partitions(&mut self) -> Result<PartitionedResult> {
        Err(unbuilt(PARTITIONS))
    }

    /// The parameters the prepared text has, by name and in the order
    /// the engine found them.
    ///
    /// Every type is null, which the ADBC specification asks for when
    /// the driver cannot say: a zu parameter takes its type from the
    /// value bound to it and the text does not declare one.
    fn get_parameter_schema(&self) -> Result<Schema> {
        let (_, names) = self.prepared.as_ref().ok_or_else(|| {
            plain(
                "a statement has parameters only once it is prepared: prepare it first",
                Status::InvalidState,
            )
        })?;
        Ok(Schema::new(
            names
                .iter()
                .map(|name| Field::new(name, DataType::Null, true))
                .collect::<Vec<_>>(),
        ))
    }

    fn prepare(&mut self) -> Result<()> {
        let sql = self.text()?.to_string();
        let mut held = Connection::locked(&self.held)?;
        self.prepared = Some(held.conn.prepare(&sql).map_err(adbc)?);
        Ok(())
    }

    fn set_sql_query(&mut self, query: impl AsRef<str>) -> Result<()> {
        // The handle belongs to the old text, so it goes with it. The
        // engine keeps its own plan cache, so preparing the same text
        // again is not a recompile.
        if let Some((handle, _)) = self.prepared.take() {
            Connection::locked(&self.held)?.conn.close_prepared(handle);
        }
        self.sql = Some(query.as_ref().to_string());
        Ok(())
    }

    fn set_substrait_plan(&mut self, _plan: impl AsRef<[u8]>) -> Result<()> {
        Err(unbuilt(
            "Substrait is a relational plan and zu runs a graph one, so there is nothing to \
             translate it into yet",
        ))
    }

    fn cancel(&mut self) -> Result<()> {
        self.interrupt.stop();
        Ok(())
    }
}

impl Drop for Statement {
    fn drop(&mut self) {
        // The engine keeps a compiled plan per handle, so a statement
        // that is finished with hands it back. A poisoned lock means
        // the connection is going away anyway.
        if let Some((handle, _)) = self.prepared.take()
            && let Ok(mut held) = self.held.lock()
        {
            held.conn.close_prepared(handle);
        }
    }
}

const BIND: &str = "binding Arrow data to a statement is what a bulk insert and a parameterised \
     query both use, and neither is wired through this driver";

const INGEST: &str = "bulk ingest, which takes an Arrow stream and makes a table out of it, is \
     not wired through this driver";

#[cfg(test)]
mod tests {
    use adbc_core::{Connection as _, Database as _, Driver as _, Optionable as _, Statement as _};
    use arrow_array::{Array, Int64Array};

    use super::*;
    use crate::Driver;

    fn statement() -> Statement {
        let mut driver = Driver;
        let db = driver.new_database().expect("a database in memory");
        let mut conn = db.new_connection().expect("a connection");
        conn.new_statement().expect("a statement")
    }

    fn ran(sql: &str) -> Vec<RecordBatch> {
        let mut stmt = statement();
        stmt.set_sql_query(sql).expect("set");
        stmt.execute()
            .expect("it runs")
            .collect::<std::result::Result<_, _>>()
            .expect("and reads")
    }

    #[test]
    fn a_statement_comes_back_as_arrow() {
        let batches = ran("RETURN 1 AS one, 'two' AS two");
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.schema().field(0).name(), "one");
        assert_eq!(batch.schema().field(1).name(), "two");
        let one = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64");
        assert_eq!(one.value(0), 1);
    }

    #[test]
    fn a_result_with_no_rows_still_has_its_columns() {
        let batches = ran("MATCH (n:nothing) RETURN n.uid AS uid");
        assert_eq!(batches.len(), 1, "an empty result is one empty batch");
        assert_eq!(batches[0].num_rows(), 0);
        assert_eq!(batches[0].schema().field(0).name(), "uid");
    }

    #[test]
    fn a_statement_with_nothing_in_it_says_so() {
        let mut stmt = statement();
        let err = stmt.execute().err().expect("there is no query");
        assert_eq!(err.status, Status::InvalidState);
    }

    #[test]
    fn a_syntax_error_keeps_its_gqlstatus() {
        let mut stmt = statement();
        stmt.set_sql_query("RETURN RETURN").expect("set");
        let err = stmt.execute().err().expect("that is not a statement");
        assert_eq!(err.status, Status::InvalidArguments);
        assert_eq!(
            &err.sqlstate.map(|c| c as u8)[..2],
            b"42",
            "a syntax error is class 42"
        );
    }

    #[test]
    fn a_caller_can_say_how_big_a_batch_is() {
        let mut stmt = statement();
        stmt.set_option(
            OptionStatement::Other(ROWS_PER_BATCH.into()),
            OptionValue::Int(1),
        )
        .expect("one row a batch");
        assert_eq!(
            stmt.get_option_int(OptionStatement::Other(ROWS_PER_BATCH.into()))
                .unwrap(),
            1
        );
        stmt.set_sql_query("UNWIND [1, 2, 3] AS n RETURN n")
            .expect("set");
        let batches: Vec<RecordBatch> = stmt
            .execute()
            .expect("it runs")
            .collect::<std::result::Result<_, _>>()
            .expect("and reads");
        assert_eq!(
            batches
                .iter()
                .map(RecordBatch::num_rows)
                .collect::<Vec<_>>(),
            [1, 1, 1]
        );
    }

    #[test]
    fn a_batch_of_no_rows_is_refused() {
        let mut stmt = statement();
        let err = stmt
            .set_option(
                OptionStatement::Other(ROWS_PER_BATCH.into()),
                OptionValue::Int(0),
            )
            .expect_err("that reader never ends");
        assert!(err.message.contains("at least one"), "{}", err.message);
    }

    #[test]
    fn the_schema_comes_without_keeping_the_rows() {
        let mut stmt = statement();
        stmt.set_sql_query("RETURN 1 AS one").expect("set");
        let schema = stmt.execute_schema().expect("the columns");
        assert_eq!(schema.field(0).name(), "one");
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
    }

    #[test]
    fn what_is_not_built_says_which_one_it_was() {
        let mut stmt = statement();
        for err in [
            stmt.execute_partitions().expect_err("a refusal"),
            stmt.set_substrait_plan([]).expect_err("a refusal"),
            stmt.bind_stream(Box::new(arrow_array::RecordBatchIterator::new(
                [],
                Arc::new(Schema::empty()),
            )))
            .expect_err("a refusal"),
        ] {
            assert_eq!(err.status, Status::NotImplemented);
            assert!(err.message.contains("does not do yet"), "{}", err.message);
        }
    }
}
