# zudb-cli

The `zu` command-line tool for the zu embedded graph database.

```
cargo install zudb-cli
```

That installs a binary called `zu`. The package is named `zudb-cli` because `zu` was already taken on crates.io.

## Commands

```
zu shell [<file.zu1>]                 interactive session
zu query <file.zu1> -c <zuQL>         run one statement and print the result
zu copy <edges.csv> <out.zu1>         bulk load an edge list or a dataset into a new file
zu convert <in.zu1> <out.db>          move a database to another engine, or an edge list to another format
zu verify <file.zu1>                  walk every checksum and check the structure
zu stat <file.zu1>                    print the size breakdown: schema, free space, and data
zu lsp --stdio                        language server for an editor
```

There are a few more (`analyze`, `neighbors`, `lookup`, `edge`, `conformance`, `corpus`, `version`). Run `zu help` for the list and `zu help <command>` for the details and examples of one command.

Most commands take `--format`, and the exit codes are the same across commands, so the tool is easy to script.

## Status

zu is early and not ready for real data yet. See the [repository](https://github.com/tamnd/zu) for the status and the specification. To use zu from Rust code, depend on [`zudb`](https://crates.io/crates/zudb).

## License

Apache-2.0
