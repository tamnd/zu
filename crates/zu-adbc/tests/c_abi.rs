//! The round trip a real caller makes.
//!
//! The unit tests next to the driver call the safe Rust traits, which is
//! the near side of the interface. Nothing there proves the shared
//! object exports an entrypoint a driver manager can find, that the
//! forty function pointers are filled in, or that a result and an error
//! survive the crossing. Python, R, Java and Go all arrive this way
//! and none of them link this crate, so this is the shape their calls
//! have: open the `cdylib` by path, go through the manager, read the
//! stream back.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;

use adbc_core::options::{AdbcVersion, InfoCode, OptionConnection, OptionDatabase, OptionValue};
use adbc_core::{Connection, Database as _, Driver as _, Optionable as _, Statement as _};
use adbc_driver_manager::ManagedDriver;
use arrow_array::{Array, Int64Array, RecordBatch, StringArray, UInt32Array, UnionArray};

/// The shared object, built if this test run has not built it.
///
/// A test binary lives in `target/<profile>/deps`, so the library sits
/// two directories up under whatever the platform calls a shared object.
/// `cargo test` compiles the library as an rlib to link this binary
/// against and stops there, because nothing it knows about consumes a
/// `cdylib`, so the file is often missing on a clean tree. Building it
/// here rather than skipping the test is the point: a driver nobody can
/// `dlopen` is a driver nobody can use, and that has to fail loudly.
///
/// Every test here wants the same file and they run at once, so the
/// build happens behind a [`OnceLock`] rather than eight times against
/// cargo's own lock.
fn cdylib() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(build)
}

fn build() -> PathBuf {
    let mut at = std::env::current_exe().expect("a test binary knows where it is");
    at.pop();
    at.pop();
    let path = at.join(format!(
        "{}zu_adbc{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    if path.exists() {
        return path;
    }

    let profile = at
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the profile is the directory the test binary is in");
    let mut build =
        std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
    build
        .arg("build")
        .arg("--lib")
        .arg("--manifest-path")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
    if profile != "debug" {
        build.arg("--profile").arg(profile);
    }
    let built = build.status().expect("cargo runs");
    assert!(built.success(), "the driver does not build as a cdylib");
    assert!(
        path.exists(),
        "cargo built the library but there is no shared object at {}",
        path.display()
    );
    path
}

/// The manager, holding the driver it just opened by path.
fn loaded() -> ManagedDriver {
    ManagedDriver::load_dynamic_from_filename(
        cdylib().as_path(),
        Some(b"ZuDriverInit"),
        AdbcVersion::V110,
    )
    .expect("the manager finds the entrypoint and initialises the driver")
}

/// A connection to a database at `uri`, through the C ABI.
fn connected(uri: &str) -> impl Connection {
    let db = loaded()
        .new_database_with_opts([(OptionDatabase::Uri, OptionValue::String(uri.into()))])
        .expect("a database opens");
    db.new_connection().expect("a connection opens")
}

/// Everything a statement returns, read to the end.
fn ran(conn: &mut impl Connection, sql: &str) -> Vec<RecordBatch> {
    let mut stmt = conn.new_statement().expect("a statement");
    stmt.set_sql_query(sql).expect("the text is taken");
    stmt.execute()
        .expect("it runs")
        .collect::<Result<_, _>>()
        .expect("and the batches read")
}

#[test]
fn a_result_crosses_the_c_abi_as_arrow() {
    let mut conn = connected(":memory:");
    let batches = ran(&mut conn, "RETURN 1 AS one, 'two' AS two");

    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(batch.schema().field(0).name(), "one");
    assert_eq!(batch.schema().field(1).name(), "two");
    assert_eq!(
        batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("int64")
            .value(0),
        1
    );
    assert_eq!(
        batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("utf8")
            .value(0),
        "two"
    );
}

#[test]
fn every_batch_of_a_long_result_arrives() {
    let mut conn = connected(":memory:");
    let mut stmt = conn.new_statement().expect("a statement");
    stmt.set_option(
        adbc_core::options::OptionStatement::Other("zu.rows_per_batch".into()),
        OptionValue::Int(2),
    )
    .expect("two rows a batch");
    stmt.set_sql_query("UNWIND [1, 2, 3, 4, 5] AS n RETURN n")
        .expect("the text is taken");
    let batches: Vec<RecordBatch> = stmt
        .execute()
        .expect("it runs")
        .collect::<Result<_, _>>()
        .expect("and the batches read");

    assert_eq!(
        batches
            .iter()
            .map(RecordBatch::num_rows)
            .collect::<Vec<_>>(),
        [2, 2, 1],
        "the stream ends where the rows do and not a batch earlier"
    );
}

#[test]
fn a_refusal_crosses_with_its_gqlstatus_and_its_link() {
    let mut conn = connected(":memory:");
    let mut stmt = conn.new_statement().expect("a statement");
    stmt.set_sql_query("RETURN RETURN")
        .expect("the text is taken");
    let err = stmt.execute().err().expect("that is not a statement");

    assert_eq!(
        &err.sqlstate.map(|c| c as u8)[..2],
        b"42",
        "a syntax error is class 42 on both sides of the ABI"
    );
    let details = err.details.as_ref().expect("the detail keys came across");
    let doc = details
        .iter()
        .find(|(key, _)| key == "zu.doc_url")
        .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
        .expect("a caller in another language still gets the page to read");
    assert!(doc.starts_with("http"), "{doc}");
}

#[test]
fn the_driver_names_itself_through_the_abi() {
    let conn = connected(":memory:");
    let batches: Vec<RecordBatch> = conn
        .get_info(Some(HashSet::from([InfoCode::VendorName])))
        .expect("the facts come back")
        .collect::<Result<_, _>>()
        .expect("and read");

    let batch = batches.first().expect("one batch of one row");
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(
        batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("uint32")
            .value(0),
        u32::from(&InfoCode::VendorName)
    );
    let held = batch
        .column(1)
        .as_any()
        .downcast_ref::<UnionArray>()
        .expect("a union")
        .value(0);
    assert_eq!(
        held.as_any()
            .downcast_ref::<StringArray>()
            .expect("utf8")
            .value(0),
        "zu"
    );
}

#[test]
fn the_table_types_are_the_two_zu_has() {
    let conn = connected(":memory:");
    let batches: Vec<RecordBatch> = conn
        .get_table_types()
        .expect("the types come back")
        .collect::<Result<_, _>>()
        .expect("and read");

    let names: Vec<String> = batches
        .iter()
        .flat_map(|batch| {
            let column = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("utf8");
            (0..column.len())
                .map(|row| column.value(row).to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(names, ["node", "rel"]);
}

#[test]
fn a_transaction_opens_and_closes_across_the_abi() {
    let mut conn = connected(":memory:");
    conn.set_option(
        OptionConnection::AutoCommit,
        OptionValue::String("false".into()),
    )
    .expect("the caller takes the commits");
    assert_eq!(ran(&mut conn, "RETURN 1 AS one").len(), 1);
    conn.commit().expect("and gives one");
    assert_eq!(
        ran(&mut conn, "RETURN 2 AS two").len(),
        1,
        "a commit starts the next transaction rather than leaving none"
    );
    conn.rollback().expect("a rollback closes it too");
    conn.set_option(
        OptionConnection::AutoCommit,
        OptionValue::String("true".into()),
    )
    .expect("and back to a commit a statement");
}

#[test]
fn a_file_a_caller_names_is_made_and_reopened() {
    let dir = tempfile::tempdir().expect("a directory to put it in");
    let path = dir.path().join("through-c.zu");
    let uri = path.to_str().expect("a path that is text").to_string();

    let mut conn = connected(&uri);
    assert_eq!(ran(&mut conn, "RETURN 1 AS one").len(), 1);
    drop(conn);
    assert!(path.exists(), "a path that was not there is created");

    let db = loaded()
        .new_database_with_opts([
            (OptionDatabase::Uri, OptionValue::String(uri)),
            (
                OptionDatabase::Other("zu.read_only".into()),
                OptionValue::String("true".into()),
            ),
        ])
        .expect("and opens again with the write side off");
    let mut conn = db.new_connection().expect("a connection opens");
    assert_eq!(ran(&mut conn, "RETURN 1 AS one").len(), 1);
}

#[test]
fn a_refusal_is_a_sentence_and_not_a_crash() {
    let conn = connected(":memory:");
    let err = conn
        .get_table_schema(None, None, "anything")
        .expect_err("the columns of a table are not published yet");
    assert!(
        err.message.contains("property columns"),
        "a caller three languages away is told why: {}",
        err.message
    );
}
