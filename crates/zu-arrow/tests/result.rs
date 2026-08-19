//! A statement somebody wrote, exported both ways.
//!
//! The unit tests in the crate build columns by hand, which is how the
//! rare types get covered at all. This is the other half: a real
//! statement, run by the real engine, over buffers the sink filled, out
//! through the C Data Interface and out through the IPC stream, and read
//! back to check that what arrived is what the database holds.

use std::path::{Path, PathBuf};

use arrow::array::{Array, Float64Array, Int64Array, StringArray, StructArray};
use arrow::datatypes::DataType;
#[cfg(feature = "ffi")]
use arrow::ffi_stream::ArrowArrayStreamReader;
#[cfg(feature = "ipc")]
use arrow::ipc::reader::StreamReader;
use arrow::record_batch::{RecordBatch, RecordBatchReader};

use std::collections::HashMap;

use zu::dataset::{NodeFile, RelFile, load_dataset};
use zu::query::{QueryResult, run};
use zu::session::Session;
use zu_arrow::{BATCH, Table, Tables};
use zu_zu1::catalog::Catalog;
use zu_zu1::file::Zu1File;

/// The names a client takes off the catalog when a statement runs, which
/// is where the table ids in the rows get their meaning.
struct Names {
    nodes: HashMap<u32, String>,
    rels: HashMap<u32, String>,
}

impl Names {
    fn of(catalog: &Catalog) -> Names {
        Names {
            nodes: catalog
                .node_tables()
                .iter()
                .map(|table| (table.id, table.name.clone()))
                .collect(),
            rels: catalog
                .rel_tables()
                .iter()
                .map(|table| (table.id, table.name.clone()))
                .collect(),
        }
    }
}

impl Tables for Names {
    fn node(&self, id: u32) -> Option<&str> {
        self.nodes.get(&id).map(String::as_str)
    }

    fn rel(&self, id: u32) -> Option<&str> {
        self.rels.get(&id).map(String::as_str)
    }
}

/// Three accounts and three people, two of whom own one, so a walk has
/// something to be and an OPTIONAL MATCH has a null to carry.
fn graph(dir: &Path) -> Zu1File {
    let at = |name: &str, body: &str| -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    };
    let account = at(
        "Account.csv",
        "id:ID,name:STRING,balance:FLOAT64\n\
         10,checking,100.5\n\
         11,savings,20.25\n\
         12,brokerage,0\n",
    );
    let person = at("Person.csv", "id:ID,name:STRING\n100,ann\n101,bo\n102,cy\n");
    let own = at(
        "own.csv",
        ":START_ID,:END_ID,:TYPE\n100,10,own\n101,11,own\n",
    );
    let node = |table: &str, path: PathBuf| NodeFile {
        table: table.to_string(),
        path,
    };
    let out = dir.join("arrow.zu1");
    load_dataset(
        &[node("Account", account), node("Person", person)],
        &[RelFile {
            table: "own".to_string(),
            from: "Person".to_string(),
            to: "Account".to_string(),
            path: own,
            undirected: false,
        }],
        &out,
    )
    .expect("load");
    Zu1File::open(&out).unwrap()
}

/// The same database's names, read the way a client reads them.
fn names(dir: &Path) -> Names {
    let session = Session::open(&dir.join("arrow.zu1")).expect("open");
    Names::of(session.catalog())
}

fn answer(db: &mut Zu1File, source: &str) -> QueryResult {
    run(source, db, &[]).unwrap_or_else(|err| panic!("{source}: {err}"))
}

/// Every batch a reader gives, in order.
fn drain(reader: impl RecordBatchReader) -> Vec<RecordBatch> {
    reader.map(|batch| batch.expect("batch")).collect()
}

fn strings(batch: &RecordBatch, at: usize) -> Vec<Option<String>> {
    let column = batch
        .column(at)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("strings");
    (0..column.len())
        .map(|row| (!column.is_null(row)).then(|| column.value(row).to_string()))
        .collect()
}

#[test]
fn a_projection_of_stored_columns_is_the_arrow_table_it_looks_like() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let result = answer(
        &mut db,
        "MATCH (a:Account) RETURN a.name AS name, a.balance AS balance",
    );
    let table = Table::of(&result, &names(dir.path())).expect("arrays");
    assert_eq!(table.rows(), 3);
    assert_eq!(table.schema().field(0).name(), "name");
    assert_eq!(table.schema().field(0).data_type(), &DataType::Utf8);
    assert_eq!(table.schema().field(1).data_type(), &DataType::Float64);

    let batches = drain(table.batches(BATCH));
    assert_eq!(batches.len(), 1);
    let mut names = strings(&batches[0], 0);
    names.sort();
    assert_eq!(
        names,
        vec![
            Some("brokerage".to_string()),
            Some("checking".to_string()),
            Some("savings".to_string())
        ]
    );
    let balances = batches[0]
        .column(1)
        .as_any()
        .downcast_ref::<Float64Array>()
        .expect("floats");
    let mut held: Vec<f64> = balances.values().to_vec();
    held.sort_by(f64::total_cmp);
    assert_eq!(held, vec![0.0, 20.25, 100.5]);
}

#[test]
fn a_row_that_matched_nothing_is_a_null_and_not_a_missing_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let result = answer(
        &mut db,
        "MATCH (p:Person) OPTIONAL MATCH (p)-[:own]->(a:Account) RETURN a.name AS owns",
    );
    let batches = drain(
        Table::of(&result, &names(dir.path()))
            .expect("arrays")
            .batches(BATCH),
    );
    let mut owned = strings(&batches[0], 0);
    owned.sort();
    assert_eq!(
        owned,
        vec![
            None,
            Some("checking".to_string()),
            Some("savings".to_string())
        ]
    );
}

#[test]
fn a_node_column_arrives_as_a_struct_naming_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let result = answer(&mut db, "MATCH (p:Person) RETURN p AS who");
    let batches = drain(
        Table::of(&result, &names(dir.path()))
            .expect("arrays")
            .batches(BATCH),
    );
    let held = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<StructArray>()
        .expect("struct");
    let tables = held
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("names");
    assert_eq!(tables.value(0), "Person");
    assert_eq!(held.len(), 3);
}

#[test]
fn a_statement_that_matched_nothing_still_says_what_its_columns_were() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let result = answer(
        &mut db,
        "MATCH (a:Account) WHERE a.name = 'nothing' RETURN a.name AS name, a.balance AS balance",
    );
    let table = Table::of(&result, &names(dir.path())).expect("arrays");
    assert_eq!(table.rows(), 0);
    // The plan declared the types, so an empty answer has the schema a
    // full one would have had rather than two columns of nulls.
    assert_eq!(table.schema().field(0).data_type(), &DataType::Utf8);
    assert_eq!(table.schema().field(1).data_type(), &DataType::Float64);
    let batches = drain(table.batches(BATCH));
    assert_eq!(batches.len(), 1, "a reader has to see the schema somehow");
    assert_eq!(batches[0].num_rows(), 0);
}

#[cfg(feature = "ffi")]
#[test]
fn the_stream_carries_the_same_answer_across_the_c_data_interface() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let result = answer(&mut db, "MATCH (p:Person) RETURN p.name AS name");
    let mut stream = zu_arrow::stream(&result, &names(dir.path()), BATCH).expect("stream");
    // Across the interface and back, which is what a consumer in the
    // same process does with the struct it was handed.
    let reader = unsafe { ArrowArrayStreamReader::from_raw(&mut stream) }.expect("import");
    assert_eq!(reader.schema().field(0).name(), "name");
    let batches = drain(reader);
    let mut names: Vec<Option<String>> = batches.iter().flat_map(|b| strings(b, 0)).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            Some("ann".to_string()),
            Some("bo".to_string()),
            Some("cy".to_string())
        ]
    );
}

#[cfg(feature = "ipc")]
#[test]
fn the_ipc_bytes_read_back_as_the_columns_that_went_in() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let result = answer(
        &mut db,
        "MATCH (a:Account) RETURN a.name AS name, a.balance AS balance",
    );
    let bytes = zu_arrow::ipc(&result, &names(dir.path()), BATCH).expect("ipc");
    let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).expect("reader");
    assert_eq!(reader.schema().field(1).data_type(), &DataType::Float64);
    let batches = drain(reader);
    assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 3);
}

#[cfg(feature = "ipc")]
#[test]
fn a_batch_size_a_caller_asked_for_is_the_batch_size_the_stream_uses() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let result = answer(&mut db, "MATCH (p:Person) RETURN p.name AS name");
    let bytes = zu_arrow::ipc(&result, &names(dir.path()), 2).expect("ipc");
    let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).expect("reader");
    let batches = drain(reader);
    assert_eq!(
        batches
            .iter()
            .map(RecordBatch::num_rows)
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
}

#[test]
fn an_expression_column_and_a_count_are_the_types_they_are() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let result = answer(
        &mut db,
        "MATCH (a:Account) RETURN count(*) AS n, sum(a.balance) AS total",
    );
    let table = Table::of(&result, &names(dir.path())).expect("arrays");
    assert_eq!(table.schema().field(0).data_type(), &DataType::Int64);
    let batches = drain(table.batches(BATCH));
    let counted = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .expect("ints");
    assert_eq!(counted.value(0), 3);
}
