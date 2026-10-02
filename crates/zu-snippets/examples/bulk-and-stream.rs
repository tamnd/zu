use zudb::{Database, Flow};

fn main() -> zudb::Result<()> {
    let db = Database::memory()?;
    let mut conn = db.connect()?;
    conn.execute("INSERT (p:person {uid: 0, name: 'first'})")?;

    let mut app = conn.appender("person")?;
    for uid in 1..=10_000i64 {
        app.append_row((uid, "someone"))?;
    }
    let loaded = app.close()?;
    println!("appended {loaded}");

    let mut total = 0i64;
    conn.query_stream("MATCH (p:person) RETURN p.uid AS uid", &[], |batch| {
        for row in batch.iter() {
            total += row.get_at::<i64>(0)?;
        }
        Ok(Flow::More)
    })?;
    println!("sum {total}");

    let plan = conn.explain("MATCH (p:person) WHERE p.uid > 9000 RETURN p.name AS name")?;
    println!("{plan}");
    Ok(())
}
