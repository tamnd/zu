//! The differential corpus: one seeded workload driven through zu1
//! and sqlite side by side, with every observable compared along the
//! way. The sqlite engine is the oracle, plain B-tree rows with
//! nothing clever, so wherever zu1's overlays, folds, tombstone
//! chains, or CSR rebuilds disagree with it, one of them is wrong and
//! it is almost never the oracle. Comparisons run mid-stream, after
//! folds, at the end, and again after a cold zu1 reopen, and the
//! sqlite side reads adjacency through its lazy CSR cache so the
//! cache earns differential coverage too.

use zu_sqlite::{ColumnType, SqliteStore, Value};
use zu_zu1::catalog::Catalog;
use zu_zu1::file::Zu1File;
use zu_zu1::fold::{checkpoint_fold, recover};
use zu_zu1::graph::{Direction, GraphReader, bulk_load_as};
use zu_zu1::props::{PropValues, PropsReader, load_props, store_props};
use zu_zu1::txn::{Cell, Mvcc};
use zu_zu1::wal::Wal;

/// splitmix64: deterministic, seedable, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const BASE_ROWS: u64 = 8;
const BASE_EDGES: [(u32, u32); 6] = [(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (5, 0)];

struct Zu1Side {
    db: Zu1File,
    wal: Wal,
    mvcc: Mvcc,
    person: u32,
    knows: u32,
}

/// One live comparison of everything a reader can see in both stores.
/// zu1 rows are zero-based, sqlite `zrow` starts at one, hence the +1.
fn compare(zu: &mut Zu1Side, sq: &SqliteStore, total: u64, what: &str) {
    let epoch = zu.mvcc.epoch();
    let base = Catalog::load(&mut zu.db)
        .unwrap()
        .node_by_id(zu.person)
        .unwrap()
        .node_count;
    assert_eq!(
        base + zu.mvcc.appended_rows(zu.person, epoch),
        total,
        "{what}: row domain"
    );
    let props = load_props(&mut zu.db, zu.person).unwrap().unwrap();
    let mut reader = PropsReader::new(props);
    let mut graph = GraphReader::load_table(&mut zu.db, "knows").unwrap();
    let graph_rows = graph.directory().node_count;
    for row in 0..total {
        let sq_row: Option<(i64, String)> = sq
            .raw()
            .query_row(
                "SELECT p_age, p_name FROM n_person WHERE zrow = ?",
                [row as i64 + 1],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .unwrap();
        let deleted = zu.mvcc.is_deleted(zu.person, row, epoch);
        assert_eq!(deleted, sq_row.is_none(), "{what}: liveness of row {row}");
        if let Some((sq_age, sq_name)) = sq_row {
            let age = match zu.mvcc.cell(zu.person, base, row, 0, epoch) {
                Some(Cell::Int(x)) => x,
                Some(Cell::Str(_)) => unreachable!("age is an int column"),
                None => reader.read_int(&mut zu.db, 0, row).unwrap(),
            };
            let name = match zu.mvcc.cell(zu.person, base, row, 1, epoch) {
                Some(Cell::Str(s)) => s,
                Some(Cell::Int(_)) => unreachable!("name is a str column"),
                None => {
                    let mut buf = Vec::new();
                    reader.read_str(&mut zu.db, 1, row, &mut buf).unwrap();
                    buf
                }
            };
            assert_eq!(age as i64, sq_age, "{what}: age of row {row}");
            assert_eq!(name, sq_name.into_bytes(), "{what}: name of row {row}");
        }
        for (reversed, dir, sq_dir) in [
            (false, Direction::Fwd, zu_sqlite::Direction::Fwd),
            (true, Direction::Bwd, zu_sqlite::Direction::Bwd),
        ] {
            let mut zu_nbrs = if row < graph_rows {
                graph.neighbors_dir(&mut zu.db, row, dir).unwrap().to_vec()
            } else {
                Vec::new()
            };
            zu.mvcc
                .neighbors(zu.knows, row, reversed, epoch, &mut zu_nbrs);
            zu_nbrs.sort_unstable();
            let group = (row + 1) / u64::from(zu_sqlite::GROUP_ROWS);
            let csr = sq.csr("knows", group, sq_dir).unwrap();
            let sq_nbrs: Vec<u64> = csr
                .neighbors(row as i64 + 1)
                .iter()
                .map(|&n| (n - 1) as u64)
                .collect();
            assert_eq!(zu_nbrs, sq_nbrs, "{what}: {dir:?} neighbors of row {row}");
        }
    }
}

fn run_seed(seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join(format!("diff-{seed}.zu1"));
    let wal_path = dir.path().join(format!("diff-{seed}.wal"));
    let sq_path = dir.path().join(format!("diff-{seed}.db"));

    let mut db = Zu1File::create(&db_path).unwrap();
    bulk_load_as(&mut db, "person", "knows", BASE_ROWS, &BASE_EDGES).unwrap();
    let ages: Vec<u64> = (0..BASE_ROWS).map(|i| 20 + i).collect();
    let names: Vec<Vec<u8>> = (0..BASE_ROWS)
        .map(|i| format!("p{i}").into_bytes())
        .collect();
    let name_refs: Vec<&[u8]> = names.iter().map(|n| n.as_slice()).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("age", PropValues::Int(&ages)),
            ("name", PropValues::Str(&name_refs)),
        ],
    )
    .unwrap();
    let wal = Wal::open(&wal_path).unwrap();
    let mvcc = recover(&mut db, &wal).unwrap();
    let catalog = Catalog::load(&mut db).unwrap();
    let person = catalog.node_by_name("person").unwrap().id;
    let knows = catalog.rel_by_name("knows").unwrap().id;
    let mut zu = Zu1Side {
        db,
        wal,
        mvcc,
        person,
        knows,
    };

    let mut sq = SqliteStore::open(&sq_path).unwrap();
    sq.create_node_table(
        "person",
        &[("age", ColumnType::Integer), ("name", ColumnType::Text)],
    )
    .unwrap();
    sq.create_rel_table("knows", "person", "person", &[])
        .unwrap();
    for i in 0..BASE_ROWS {
        sq.insert_node(
            "person",
            &[
                Value::Int(ages[i as usize] as i64),
                Value::Text(format!("p{i}")),
            ],
        )
        .unwrap();
    }
    for (a, b) in BASE_EDGES {
        sq.insert_rel("knows", i64::from(a) + 1, i64::from(b) + 1, &[])
            .unwrap();
    }

    let mut rng = Rng(seed);
    let mut live: Vec<bool> = vec![true; BASE_ROWS as usize];
    let pick_live = |rng: &mut Rng, live: &[bool]| -> Option<u64> {
        let alive: Vec<u64> = (0..live.len() as u64)
            .filter(|&r| live[r as usize])
            .collect();
        if alive.is_empty() {
            None
        } else {
            Some(alive[rng.below(alive.len() as u64) as usize])
        }
    };
    for step in 0..300u32 {
        let mut txn = zu.mvcc.begin();
        sq.begin().unwrap();
        match rng.below(10) {
            0..=2 => {
                let age = rng.below(1000);
                let name = format!("n{}", rng.below(100_000));
                txn.insert_nodes(
                    person,
                    vec![
                        (0, vec![Cell::Int(age)]),
                        (1, vec![Cell::Str(name.clone().into_bytes())]),
                    ],
                )
                .unwrap();
                // Explicit zrow: bare rowid allocation would reuse a
                // deleted tail row's id and fork from zu1's
                // append-only domain.
                sq.insert_node_at(
                    "person",
                    live.len() as i64 + 1,
                    &[Value::Int(age as i64), Value::Text(name)],
                )
                .unwrap();
                live.push(true);
            }
            3 | 4 => {
                if let Some(row) = pick_live(&mut rng, &live) {
                    if rng.below(2) == 0 {
                        let age = rng.below(1000);
                        txn.update(person, row, 0, Cell::Int(age));
                        sq.update_node("person", row as i64 + 1, "age", &Value::Int(age as i64))
                            .unwrap();
                    } else {
                        let name = format!("u{}", rng.below(100_000));
                        txn.update(person, row, 1, Cell::Str(name.clone().into_bytes()));
                        sq.update_node("person", row as i64 + 1, "name", &Value::Text(name))
                            .unwrap();
                    }
                }
            }
            5 => {
                if let Some(row) = pick_live(&mut rng, &live) {
                    txn.delete(person, row);
                    sq.delete_node("person", row as i64 + 1).unwrap();
                    live[row as usize] = false;
                }
            }
            _ => {
                if let (Some(a), Some(b)) = (pick_live(&mut rng, &live), pick_live(&mut rng, &live))
                {
                    txn.insert_rel(knows, a, b);
                    sq.insert_rel("knows", a as i64 + 1, b as i64 + 1, &[])
                        .unwrap();
                }
            }
        }
        txn.commit(&mut zu.wal).unwrap();
        sq.commit().unwrap();
        if step % 37 == 36 {
            checkpoint_fold(&mut zu.db, &mut zu.mvcc, &mut zu.wal).unwrap();
            sq.checkpoint().unwrap();
            compare(
                &mut zu,
                &sq,
                live.len() as u64,
                &format!("seed {seed} fold at {step}"),
            );
        } else if step % 19 == 18 {
            compare(
                &mut zu,
                &sq,
                live.len() as u64,
                &format!("seed {seed} step {step}"),
            );
        }
    }
    compare(
        &mut zu,
        &sq,
        live.len() as u64,
        &format!("seed {seed} final"),
    );
    // A cold zu1 reopen must land on the same answers the oracle holds.
    drop(zu.mvcc);
    let mut db = Zu1File::open(&db_path).unwrap();
    let wal = Wal::open(&wal_path).unwrap();
    let mvcc = recover(&mut db, &wal).unwrap();
    let mut zu = Zu1Side {
        db,
        wal,
        mvcc,
        person,
        knows,
    };
    compare(
        &mut zu,
        &sq,
        live.len() as u64,
        &format!("seed {seed} reopened"),
    );
}

#[test]
fn seeded_workloads_agree_across_engines() {
    for seed in [7, 1913, 58_141] {
        run_seed(seed);
    }
}
