# zudb-cli

The `zu` command-line tool for [zu](https://github.com/tamnd/zu), an embedded property-graph database. It loads data into `.zu1` files, runs queries against them, checks them, and gives you an interactive shell and a language server for editors.

```
cargo install zudb-cli
```

That installs one binary, `zu`. The package is named `zudb-cli` because `zu` was already taken on crates.io. Reading Parquet edge lists needs the `arrow` feature: `cargo install zudb-cli --features arrow`.

zu is early and not ready for data you care about. Flags and output can change between 0.0.x releases.

## A first session

```
zu copy edges.csv graph.zu1
zu stat graph.zu1
zu query graph.zu1 -c 'MATCH (n:node) RETURN count(n) AS n'
zu query graph.zu1 -c 'MATCH (n:node {id: $id}) RETURN n.id' -p id=10 --format json
zu shell graph.zu1
```

`zu copy` builds a new file from an edge list, `zu query` runs one statement and exits, and `zu shell` keeps the file open so the catalog, statistics, plan cache and decoded blocks are paid for once.

## Commands

| Command | What it does |
| --- | --- |
| `zu shell [<file.zu1>]` | open an interactive session on a file, or on nothing |
| `zu query <file.zu1> -c <zuQL>` | run one statement and print the result |
| `zu lsp --stdio [--db <file.zu1>]` | speak the language server protocol, for an editor |
| `zu copy <edges> <out.zu1>` | bulk load an edge list, or a whole dataset, into a new file |
| `zu convert <in> <out>` | rewrite an edge list in another format, or a database in another engine |
| `zu verify <file.zu1>` | walk every checksum and cross-check the structure |
| `zu stat <file.zu1>` | print the size breakdown: schema, free space, and data |
| `zu analyze <file.zu1>` | rebuild every rel table's optimizer summary |
| `zu neighbors [--in] [--key] <file.zu1> <node>` | print one node's neighbor list, in either direction |
| `zu lookup <file.zu1> <key>` | resolve an original id through the primary-key index |
| `zu edge [--in] <file.zu1> <src> <dst>` | ask whether one edge exists, without decoding the list |
| `zu conformance ...` | declare, verify, tally, and score conformance reports |
| `zu corpus <dir>` | run the shared corpus cases against this build |
| `zu version` | print the version, the C ABI revision, and what this build supports |

`zu help` lists them and `zu help <command>` (or `zu <command> --help`) prints the synopsis, examples and related commands for one. All of it is generated from one table in the source, so the usage line you get after a wrong command line is the same one the help shows.

### Loading and converting

`zu copy` takes an edge list as whitespace separated text, CSV, or Parquet, and writes one node table and one rel table. `--nodes nodes.csv` adds node properties, and `--reorder degree|bfs|none` renumbers nodes before writing so that adjacency compresses better. Original ids stay readable. For labelled data, give one file per table: `zu copy --node Account=nodes/Account.csv --rel transfer=Account:Account:rels/transfer.csv fin.zu1`.

`zu convert` moves an edge list between text, CSV and Parquet, or a database between the `zu1` format and SQLite (`graph.zu1` to `graph.db` and back). Both engines number rows the same way, so the converted file answers queries with the same ids.

### The shell

At a terminal, `zu shell` is an editor with history, multi-line statements, syntax highlighting, and tab completion that offers the labels and properties that are actually in the open file. Backslash commands answer questions about the file without going through the query language:

| Command | What it shows |
| --- | --- |
| `\d [NAME]` | the tables in the file, or one table's columns |
| `\l` | the graphs in the file |
| `\session` | the schema, graph, time zone and parameters of this session |
| `\i FILE` | runs the statements in a file |
| `\timing [on\|off]` | how long each statement took |
| `\?` and `\q` | help, and quit |

When standard input is not a terminal, or with `--format jsonl`, the shell speaks a line protocol for programs instead. The first line out is a greeting with the protocol and build versions. Each line in is either a bare statement or a JSON frame such as `{"op":"query","q":"...","params":{...}}`, and the other ops are `prepare`, `execute`, `close_stmt`, `explain`, `explain_analyze`, `hello` and `quit`. Each reply is one JSON object with the GQLSTATUS, columns and rows, or an error. A failed statement does not end the session. The protocol is described in `docs/api/jsonl-protocol.md` in the repository.

### Editors

`zu lsp --stdio` is a language server in the same binary. Completion comes from the catalog of the file passed with `--db`, diagnostics come from the same parser that would reject the statement, and highlighting uses the same scanner as the shell prompt.

## Output and exit codes

Commands that print results take `--format`. `query` takes `table` or `json`. `stat`, `verify`, `convert`, `copy` and `version` take `text` or `json`, and `zu help --format json` prints the whole command table for tools. The flag is spelled and read the same way by every command.

The exit code depends on the kind of error, not on which command hit it:

| Code | Meaning |
| --- | --- |
| 0 | success |
| 1 | the statement failed with a GQL condition |
| 2 | bad usage or an invalid argument |
| 3 | an I/O error |
| 4 | the file is corrupt, or was written by something this build cannot read |
| 5 | lost a write conflict to another writer |
| 130 | interrupted |

## How it is built

The binary is a thin layer over [`zudb`](https://crates.io/crates/zudb). Every query goes through the same `Session` the Rust API uses, so the CLI and an embedding program get the same answers. Argument parsing is hand-written rather than clap, because the command surface is small and the binary has a 15 MiB size budget. JSON reading and writing come from the small `zudb-json` crate for the same reason. The shell is split into a terminal driver (`term`), key decoding (`keys`), a line editor (`line`) and the loop that joins them (`repl`), so everything except the loop is tested without a terminal. The `corpus` and `conformance` commands are the test harnesses the release process runs, and they use `zudb-corpus`.

To use zu from Rust code instead of the shell, depend on [`zudb`](https://crates.io/crates/zudb).

## License

Apache-2.0
