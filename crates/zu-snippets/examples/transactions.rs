use zudb::{Database, params};

fn main() -> zudb::Result<()> {
    let db = Database::memory()?;
    let mut conn = db.connect()?;

    conn.execute("START TRANSACTION")?;
    conn.execute("INSERT (a:person {uid: 1, name: 'ada'})-[:knows {since: 1843}]->(b:person {uid: 2, name: 'charles'})")?;
    conn.execute("COMMIT")?;

    conn.execute("START TRANSACTION")?;
    conn.execute("INSERT (p:person {uid: 3, name: 'nobody'})")?;
    conn.execute("ROLLBACK")?;

    let (stmt, names) = conn.prepare(
        "MATCH (a:person)-[k:knows]->(b:person) WHERE a.uid = $uid RETURN b.name AS name, k.since AS since",
    )?;
    assert_eq!(names, ["uid"]);
    let rows = conn.execute_prepared(stmt, &params! { "uid" => 1 })?;
    for row in rows.iter() {
        let name: &str = row.get_by_name("name")?;
        let since: i64 = row.get_by_name("since")?;
        println!("ada knows {name} since {since}");
    }
    conn.close_prepared(stmt);

    let people = conn.query("MATCH (p:person) RETURN count(p) AS n")?;
    let n: i64 = people.row(0)?.get_at(0)?;
    println!("{n} people");
    Ok(())
}
