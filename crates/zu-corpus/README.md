# zudb-corpus

The cross-client conformance corpus for [zu](https://github.com/tamnd/zu), and the Rust runner that checks the engine against it.

zu has clients in several languages. Some link the engine and some go through the C ABI, and each decodes values with its own code. The corpus is how we check that a value written through one client and read through another means the same thing. It is a set of YAML files, each one a list of statements and what they must return, versioned with the engine and shipped as a release artifact (`conformance-<version>.tar.zst`) that every client repository runs. This crate reads those files and runs them against the engine in this repository. The runners for the other clients live in [tamnd/zu-kit](https://github.com/tamnd/zu-kit).

## What a case looks like

```yaml
schema: 4
suite: boolean
doc: The truth values and the operators over them.

cases:
  - name: and-with-a-falsehood
    doc: One false makes the conjunction false whichever side it is on.
    query: RETURN false AND true AS n
    columns:
      - n
    rows:
      - values:
          - type: BOOL
            value: false

  - name: a-write-clause-that-is-reserved
    doc: CREATE is not in the v0 core and the parser says so.
    query: CREATE (n:person)
    raises: "42001"
```

A case expects either `columns` and `rows` in order, or `raises` with a five character GQLSTATUS code. It names the code and not the message, because the code is the contract. Every value is written as `{type, value}` with the GQL type spelled out, so no reader has to guess whether `1` is an integer and how wide it is. The value is a plain YAML scalar only where that is exact. INT64 and UINT64, decimals, floats and temporal values are written as strings, because many YAML readers turn every number into a double, and an integer written as a bare number is refused rather than read loosely. A suite can start with a `load:` block, a small table of typed columns plus edges that each runner puts into a fresh database through its own bulk load path before the cases run. A case can also give `setup` statements, run statements on more than one named connection to test transactions, and describe the Arrow schema its result must export as.

The cases are in `conformance/cases/` in the repository.

## Running it

```rust
use std::path::Path;

fn main() -> Result<(), String> {
    let suites = zu_corpus::load(Path::new("conformance/cases"))?;
    let scratch = std::env::temp_dir().join("zu-corpus-run");
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;

    let report = zu_corpus::run(&suites, &scratch);
    for ran in report.failures() {
        println!("{ran}");
    }
    println!("{}", report.summary());
    Ok(())
}
```

From the repository you can also run `cargo test -p zudb-corpus`, or `zu corpus conformance/cases` through the shipped binary. Add `--strict` to fail on unsupported cases too.

Each case gets a fresh database file under the directory you pass, named after the suite and the case, so a failure leaves a file you can open. A case comes back as `Passed`, `Failed`, or `Unsupported`. Unsupported means the engine refused the statement with a syntax error or a feature-not-supported condition (GQLSTATUS class `42` or `0A`). The corpus allows that on purpose: cases can be written ahead of the engine, and the engine catches up. A failed setup step is never counted as a pass.

## How it is organized

| Module | What it does |
| --- | --- |
| `yaml` | reads the small subset of YAML the cases use: block mappings, block sequences, one-line scalars, two space indents. Anything else is an error with a line number. |
| `value` | the `{type, value}` encoding, and comparing a decoded value with what the engine returned |
| `case` | `Suite`, `Case` and `Expect`, and parsing a file into them |
| `load` | the data a suite loads before its cases, and loading it into a database |
| `arrow` | checks the Arrow export of a result: the column types, and refusals such as a time with an offset. Behind the default `arrow` feature, using `zudb-arrow`. |
| `runner` | `run`, which executes every case and returns a `Report` with one `Ran` per case and a one-line `summary()` |

`load(dir)` reads every `.yaml` file in a directory in sorted order, so two machines produce reports that line up line by line. It also checks that the `suite:` name in each file matches the file name.

## Should you depend on this?

Probably not directly. This crate is one part of zu, published so that [`zudb-cli`](https://crates.io/crates/zudb-cli) can depend on it for `zu corpus`. Its API changes whenever zu needs it to, with no deprecation period, and the versions of all the zu crates move together. To use zu from Rust, add [`zudb`](https://crates.io/crates/zudb).

The package is named `zudb-corpus` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_corpus`.

## License

Apache-2.0
