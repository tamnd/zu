//! A plain projection answered as columns, end to end.
//!
//! The unit tests in `zu-exec` check the sink and the ones in
//! `zu-query` check the walk that answers for every other plan. What is
//! only checkable here is that a statement a caller writes reaches the
//! sink at all: that the columns come back filled rather than
//! transposed, that reading the same result as rows gives exactly the
//! rows the old sink pushed, and that a plan with a step above the
//! projection still answers the way it always did.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use zu::dataset::{NodeFile, RelFile, load_dataset};
use zu::query::column::{ColumnData, ColumnType};
use zu::query::{QueryResult, run};
use zu_query::exec::Value;
use zu_zu1::file::Zu1File;

/// Three accounts with a name and a balance, and three people two of
/// whom own one, so the third owns nothing and an OPTIONAL MATCH over
/// the people has a null to carry.
fn write(dir: &Path) -> (Vec<NodeFile>, Vec<RelFile>) {
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
    (
        vec![node("Account", account), node("Person", person)],
        vec![RelFile {
            table: "own".to_string(),
            from: "Person".to_string(),
            to: "Account".to_string(),
            path: own,
            undirected: false,
        }],
    )
}

fn graph(dir: &Path) -> Zu1File {
    let (nodes, rels) = write(dir);
    let out = dir.join("columns.zu1");
    load_dataset(&nodes, &rels, &out).expect("load");
    Zu1File::open(&out).unwrap()
}

// cargo runs this file's tests in one process, in parallel. ZU_EXEC2 is
// process-wide, so a test that pins the old executor would otherwise
// change what its siblings measure (#474).
static EXEC_LOCK: Mutex<()> = Mutex::new(());

fn answer(db: &mut Zu1File, source: &str) -> QueryResult {
    let _guard = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"))
}

fn answer_on_old_executor(db: &mut Zu1File, source: &str) -> QueryResult {
    let _guard = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: EXEC_LOCK makes this the only test in the process that
    // reads or writes ZU_EXEC2 for the duration of the call.
    unsafe { std::env::set_var("ZU_EXEC2", "0") };
    let result = catch_unwind(AssertUnwindSafe(|| {
        run(source, db, &[]).unwrap_or_else(|e| panic!("{source}: {e}"))
    }));
    unsafe { std::env::remove_var("ZU_EXEC2") };
    result.unwrap_or_else(|payload| std::panic::resume_unwind(payload))
}

#[test]
fn a_projection_of_stored_columns_comes_back_as_the_buffers() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let r = answer(
        &mut db,
        "MATCH (a:Account) RETURN a.name AS name, a.balance AS balance",
    );
    assert!(
        r.rows.columns().is_some(),
        "nothing sits above this projection, so the sink kept its vectors"
    );

    let columns = r.columnar().expect("the buffers are already there");
    assert_eq!(columns.rows, 3);
    assert_eq!(columns.columns[0].name, "name");
    assert_eq!(columns.columns[0].ty, ColumnType::Str);
    let ColumnData::Str(names) = &columns.columns[0].data else {
        panic!("a string column, got {:?}", columns.columns[0].data);
    };
    // The bytes are end to end and the offsets say where each row sits,
    // which is the layout Arrow reads without touching a row.
    assert_eq!(names.bytes, b"checkingsavingsbrokerage");
    assert_eq!(names.span(0), (0, 8));
    assert_eq!(names.span(2), (15, 24));
    assert_eq!(
        columns.columns[1].data,
        ColumnData::Float(vec![100.5, 20.25, 0.0])
    );
    assert!(columns.columns[1].validity.is_none(), "no row is missing");

    // Asking twice is allowed and gives the same answer, because the
    // buffers are handed back rather than consumed.
    assert_eq!(r.columnar().expect("again").columns, columns.columns);

    // And the rows are built from those buffers only because this asks
    // for them.
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Str("checking".into()), Value::Float(100.5)],
            vec![Value::Str("savings".into()), Value::Float(20.25)],
            vec![Value::Str("brokerage".into()), Value::Float(0.0)],
        ]
    );
}

/// The rows a caller reads are the rows the old sink pushed, which is
/// what says the columns are the same answer and not a near one.
#[test]
fn the_rows_read_off_the_columns_are_the_rows_the_row_sink_pushed() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for source in [
        "MATCH (a:Account) RETURN a.name AS name",
        "MATCH (a:Account) RETURN a AS a, a.id AS id",
        "MATCH (p:Person)-[:own]->(a:Account) RETURN p.name AS who, a.name AS what",
        "MATCH (a:Account) RETURN a.balance AS b, a.name AS n, a.id AS i",
    ] {
        let held = answer(&mut db, source);
        assert!(
            held.rows.columns().is_some(),
            "{source} has nothing above its projection"
        );
        // The same statement through the old executor, which has no
        // sink but the row one.
        let rows = answer_on_old_executor(&mut db, source);
        assert!(rows.rows.columns().is_none(), "{source} on the old engine");
        assert_eq!(held.columns, rows.columns, "{source} projected the same");
        assert_eq!(held.rows, rows.rows, "{source} answered the same");
    }
}

/// A row with nothing in a column still takes its cell, and the
/// validity is what says the cell means nothing.
#[test]
fn a_missing_row_keeps_its_cell_and_loses_its_bit() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let r = answer(
        &mut db,
        "MATCH (p:Person) OPTIONAL MATCH (p)-[:own]->(a:Account) RETURN p.name AS who, a.name AS what",
    );
    assert!(r.rows.columns().is_some(), "an optional match projects");
    let columns = r.columnar().expect("the buffers");
    assert_eq!(columns.rows, 3);
    let what = &columns.columns[1];
    let validity = what
        .validity
        .as_ref()
        .expect("the third person owns nothing");
    assert_eq!(validity.nulls, 1);
    assert_eq!(validity.len, 3);
    assert!(validity.is_valid(0));
    assert!(validity.is_valid(1));
    assert!(!validity.is_valid(2));
    let ColumnData::Str(names) = &what.data else {
        panic!("a string column, got {:?}", what.data);
    };
    // The missing row is an empty span rather than a hole, so the
    // offsets stay one longer than the rows.
    assert_eq!(names.bytes, b"checkingsavings");
    assert_eq!(names.span(2), (names.bytes.len(), names.bytes.len()));
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Str("ann".into()), Value::Str("checking".into())],
            vec![Value::Str("bo".into()), Value::Str("savings".into())],
            vec![Value::Str("cy".into()), Value::Null],
        ]
    );
}

/// A column that is null the whole way down says so in its type, which
/// is the one thing the buffers answer differently from the walk: the
/// walk sees the values and this knows the projection.
#[test]
fn a_constant_null_is_a_column_of_nulls() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let r = answer(&mut db, "MATCH (a:Account) RETURN a.name AS n, null AS z");
    let columns = r.columnar().expect("the buffers");
    assert_eq!(columns.columns[1].ty, ColumnType::Null);
    assert_eq!(columns.columns[1].data, ColumnData::Null);
    // Everything is null, so a bitmap would say what the type says.
    assert!(columns.columns[1].validity.is_none());
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[2], vec![Value::Str("brokerage".into()), Value::Null]);
}

/// A sort or a limit is a step over rows, so those plans hand rows on
/// and the walk answers for them exactly as it did before.
#[test]
fn a_step_above_the_projection_still_answers_from_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    for source in [
        "MATCH (a:Account) RETURN a.name AS name ORDER BY name",
        "MATCH (a:Account) RETURN a.name AS name LIMIT 2",
        "MATCH (a:Account) RETURN DISTINCT a.name AS name",
        "MATCH (a:Account) RETURN count(a) AS n",
    ] {
        let r = answer(&mut db, source);
        assert!(r.rows.columns().is_none(), "{source} steps over rows");
        assert!(r.columnar().is_ok(), "{source} still reads as columns");
    }
}

/// How many rows is a question the columns answer, and a caller who
/// takes the rows to change them takes the whole answer with them.
#[test]
fn asking_how_many_rows_there_are_does_not_build_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let mut r = answer(&mut db, "MATCH (a:Account) RETURN a.name AS name");
    assert_eq!(r.rows.len(), 3);
    assert!(!r.rows.is_empty());
    assert!(r.rows.columns().is_some(), "counting built no rows");

    // Reading them builds them once and leaves the columns there, so a
    // client that reads rows and then columns is answered twice.
    assert_eq!(r.rows[0][0], Value::Str("checking".into()));
    assert!(r.rows.columns().is_some());
    assert_eq!(r.columnar().expect("still the buffers").rows, 3);

    // Changing them is where the columns go, because a copy of the same
    // answer that does not change with them would be a copy that lies.
    r.rows.push(vec![Value::Str("added".into())]);
    assert!(r.rows.columns().is_none());
    assert_eq!(r.rows.len(), 4);
    assert_eq!(r.columnar().expect("the walk answers now").rows, 4);
}

/// A statement that matched nothing still says what its columns were,
/// which is the one place the buffers answer better than the walk: the
/// walk sees no values and calls every column null, and the plan knows.
#[test]
fn an_empty_answer_keeps_the_types_the_plan_declared() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let r = answer(
        &mut db,
        "MATCH (a:Account) WHERE a.id > 1000 RETURN a.name AS name, a.balance AS balance",
    );
    assert_eq!(r.rows.len(), 0);
    let columns = r.columnar().expect("the buffers");
    assert_eq!(columns.rows, 0);
    assert_eq!(columns.columns[0].ty, ColumnType::Str);
    assert_eq!(columns.columns[1].ty, ColumnType::Float);
}
