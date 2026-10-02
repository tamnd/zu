# zudb

zu is an embedded, in-process property-graph database written in Rust. It is columnar, vectorized, and factorized, and it has three storage engines behind one query processor: a native single-file format (`zu1`), an ordinary SQLite file, and object storage (`s3`).

This crate is the one to depend on. It is published as `zudb` because the name `zu` was already taken on crates.io. The repository, the binary, and the file extension are still `zu`.

```
cargo add zudb
```

## Example

```rust
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
```

`create` makes a new file and fails if one is already there. Use `open` the second time.

## Status

zu is early. The specification is complete and lives in [docs/](https://github.com/tamnd/zu/tree/main/docs), and the implementation is tracked by milestone issues in the repository. Do not put data you care about in it yet, and expect the API to change between 0.0.x releases.

## The other crates

`zudb` is built out of the `zudb-*` crates (`zudb-query`, `zudb-zu1`, `zudb-storage` and so on). They are published only because `zudb` depends on them. You should not need to add any of them yourself. The command-line tool is [`zudb-cli`](https://crates.io/crates/zudb-cli).

## License

Apache-2.0
