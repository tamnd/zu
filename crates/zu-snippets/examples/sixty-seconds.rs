use zudb::{Database, params};

fn main() -> zudb::Result<()> {
    let db = Database::create("social.zu1")?;
    let mut conn = db.connect()?;

    conn.execute("INSERT (p:person {uid: 1, name: 'ada'})")?;
    conn.execute("INSERT (p:person {uid: 2, name: 'grace'})")?;

    let rows = conn.query_with(
        "MATCH (p:person) WHERE p.uid >= $uid RETURN p.name AS name, p.uid AS uid",
        &params! { "uid" => 1 },
    )?;
    for row in rows.iter() {
        let (name, uid): (&str, i64) = row.get()?;
        println!("{name} {uid}");
    }
    Ok(())
}
