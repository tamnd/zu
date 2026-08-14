//! The `zu` command-line tool.
//!
//! Subcommands (shell, query, copy, convert, verify, stat, bench) are
//! specified in `docs/10-api-and-tooling.md` and land with their layers.
//! Argument parsing is hand-rolled: the surface is small and T7 caps the
//! binary at 15 MiB, so no clap.

use std::fmt::Write as _;
use std::process::ExitCode;

use zu::query::{QueryResult, Value};
use zu::{DiagnosticRecord, Severity};

mod conformance;
mod json;
mod scoreboard;
mod shell;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const QUERY_USAGE: &str = "zu query <file.zu1> -c <zuQL> [--format table|json] [-p name=value ...]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => {
            println!("zu {VERSION}");
            ExitCode::SUCCESS
        }
        Some("--help" | "-h" | "help") | None => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some("stat") => stat_command(&args[1..]),
        Some("analyze") => match args.get(1) {
            Some(path) => analyze(std::path::Path::new(path)),
            None => usage_error("zu analyze <file.zu1>"),
        },
        Some("query") => query_command(&args[1..]),
        Some("shell") => shell::shell_command(&args[1..]),
        Some("conformance") => conformance::conformance_command(&args[1..]),
        Some("verify") => match args.get(1) {
            Some(path) => verify(std::path::Path::new(path)),
            None => usage_error("zu verify <file.zu1>"),
        },
        Some("neighbors") => {
            let mut rest = &args[1..];
            let mut dir = zu::zu1::graph::Direction::Fwd;
            let mut by_key = false;
            while let Some(flag) = rest.first().map(String::as_str) {
                match flag {
                    "--in" => dir = zu::zu1::graph::Direction::Bwd,
                    "--key" => by_key = true,
                    _ => break,
                }
                rest = &rest[1..];
            }
            match (
                rest.first(),
                rest.get(1).and_then(|s| s.parse::<u64>().ok()),
            ) {
                (Some(path), Some(node)) => {
                    neighbors(std::path::Path::new(path), node, dir, by_key)
                }
                _ => usage_error("zu neighbors [--in] [--key] <file.zu1> <node>"),
            }
        }
        Some("lookup") => match (args.get(1), args.get(2).and_then(|s| s.parse::<u64>().ok())) {
            (Some(path), Some(key)) => lookup(std::path::Path::new(path), key),
            _ => usage_error("zu lookup <file.zu1> <key>"),
        },
        Some("edge") => {
            let mut rest = &args[1..];
            let mut dir = zu::zu1::graph::Direction::Fwd;
            if rest.first().map(String::as_str) == Some("--in") {
                dir = zu::zu1::graph::Direction::Bwd;
                rest = &rest[1..];
            }
            match (
                rest.first(),
                rest.get(1).and_then(|s| s.parse::<u64>().ok()),
                rest.get(2).and_then(|s| s.parse::<u64>().ok()),
            ) {
                (Some(path), Some(src), Some(dst)) => {
                    edge(std::path::Path::new(path), src, dst, dir)
                }
                _ => usage_error("zu edge [--in] <file.zu1> <src> <dst>"),
            }
        }
        Some("copy") => {
            let mut reorder = zu::zu1::reorder::Reorder::None;
            let mut rest = &args[1..];
            if rest.first().map(String::as_str) == Some("--reorder") {
                match rest
                    .get(1)
                    .and_then(|s| zu::zu1::reorder::Reorder::parse(s))
                {
                    Some(r) => reorder = r,
                    None => {
                        return usage_error(
                            "zu copy [--reorder degree|bfs|none] <edges.txt> <out.zu1>",
                        );
                    }
                }
                rest = &rest[2..];
            }
            match (rest.first(), rest.get(1)) {
                (Some(edges), Some(out)) => copy(
                    std::path::Path::new(edges),
                    std::path::Path::new(out),
                    reorder,
                ),
                _ => usage_error(
                    "zu copy [--reorder degree|bfs|none] <edges.txt|csv|parquet> <out.zu1>",
                ),
            }
        }
        Some("convert") => match (args.get(1), args.get(2)) {
            (Some(input), Some(output)) => {
                convert(std::path::Path::new(input), std::path::Path::new(output))
            }
            _ => usage_error(
                "zu convert <edges.txt|csv|parquet|db.zu1|db.db> <out.csv|parquet|db.zu1|db.db>",
            ),
        },
        Some(cmd) => {
            eprintln!("zu: unknown command '{cmd}' (commands arrive with their milestones)");
            ExitCode::FAILURE
        }
    }
}

const STAT_USAGE: &str = "zu stat <file.zu1> [--format text|json]";

/// Parses the `stat` argument list.
fn stat_command(args: &[String]) -> ExitCode {
    let mut path: Option<&str> = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--format" | "-f" => match args.get(i + 1).map(String::as_str) {
                Some("text") => i += 2,
                Some("json") => {
                    json = true;
                    i += 2;
                }
                _ => return usage_error(STAT_USAGE),
            },
            arg if arg.starts_with('-') => return usage_error(STAT_USAGE),
            arg if path.is_none() => {
                path = Some(arg);
                i += 1;
            }
            _ => return usage_error(STAT_USAGE),
        }
    }
    let Some(path) = path else {
        return usage_error(STAT_USAGE);
    };
    let path = std::path::Path::new(path);
    if json { stat_json(path) } else { stat(path) }
}

/// The size breakdown, as three lines a person reads in the order they
/// matter: what the file weighs, what the schema costs before any rows
/// exist, and what is left, which is the graph.
///
/// The schema line is the one with a use beyond curiosity. A store's
/// size divided by the graph in it is only an encoding once the schema
/// has come out of the numerator, and zu's schema is four blocks of
/// 256 KiB, which is larger than most of the graphs a conformance
/// suite loads.
fn print_layout(path: &std::path::Path) {
    match zu::zu1::layout(path) {
        Ok(l) => {
            let kib = l.block_size / 1024;
            println!(
                "size:            {} ({} blocks of {kib} KiB)",
                bytes_human(l.bytes()),
                l.blocks
            );
            println!(
                "  schema:        {} ({} blocks: header, catalog, table index, stats)",
                bytes_human(l.schema_bytes()),
                l.schema_blocks
            );
            println!(
                "  free:          {} ({} blocks)",
                bytes_human(l.free_bytes()),
                l.free_blocks
            );
            println!(
                "  data:          {} ({} blocks)",
                bytes_human(l.data_bytes()),
                l.data_blocks
            );
        }
        // A layout that will not read is worth a line rather than an
        // exit code: the rest of stat is still true, and the reader
        // came here to find out what is wrong with the file.
        Err(e) => println!("size:            unreadable: {e}"),
    }
}

/// Bytes with a unit, in the powers of two the format allocates in.
fn bytes_human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[unit])
    }
}

/// The same facts as one JSON object, for a caller that has to do
/// arithmetic with them.
///
/// This exists for the gql-compat harness, which measures what a store
/// weighs after a load and needs the schema figure to subtract before
/// it divides. Everything here is either an exact byte count or a
/// count of rows; the color line the text form prints is prose about a
/// heuristic and has no place in a file something parses.
fn stat_json(path: &std::path::Path) -> ExitCode {
    let mut db = match zu::zu1::file::Zu1File::open(path) {
        Ok(db) => db,
        Err(e) => return command_error("stat", &e),
    };
    let layout = match zu::zu1::layout(path) {
        Ok(l) => l,
        Err(e) => return command_error("stat", &e),
    };
    let catalog = match zu::zu1::catalog::Catalog::load(&mut db) {
        Ok(c) => c,
        Err(e) => return command_error("stat", &e),
    };
    let dh = db.db_header();
    let mut out = String::from("{\"file\":");
    write_json_str(&mut out, &path.display().to_string());
    out.push_str(&format!(
        ",\"format_version\":{},\"epoch\":{},\"block_size\":{},\"blocks\":{}",
        db.file_header().format_version,
        dh.epoch,
        layout.block_size,
        layout.blocks
    ));
    out.push_str(&format!(
        ",\"bytes\":{},\"schema_bytes\":{},\"free_bytes\":{},\"data_bytes\":{}",
        layout.bytes(),
        layout.schema_bytes(),
        layout.free_bytes(),
        layout.data_bytes()
    ));
    out.push_str(",\"node_tables\":[");
    for (i, t) in catalog.node_tables().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        write_json_str(&mut out, &t.name);
        out.push_str(&format!(",\"nodes\":{}}}", t.node_count));
    }
    out.push_str("],\"rel_tables\":[");
    for (i, t) in catalog.rel_tables().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let name = |id| catalog.node_by_id(id).map_or("?", |t| t.name.as_str());
        out.push_str("{\"name\":");
        write_json_str(&mut out, &t.name);
        out.push_str(",\"from\":");
        write_json_str(&mut out, name(t.from));
        out.push_str(",\"to\":");
        write_json_str(&mut out, name(t.to));
        out.push_str(&format!(",\"edges\":{}}}", t.edge_count));
    }
    out.push_str("]}");
    println!("{out}");
    ExitCode::SUCCESS
}

fn stat(path: &std::path::Path) -> ExitCode {
    match zu::zu1::file::Zu1File::open(path) {
        Ok(mut db) => {
            let fh = db.file_header();
            let dh = db.db_header();
            let u = fh.uuid;
            println!("file:            {}", path.display());
            println!(
                "uuid:            {:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                u[0],
                u[1],
                u[2],
                u[3],
                u[4],
                u[5],
                u[6],
                u[7],
                u[8],
                u[9],
                u[10],
                u[11],
                u[12],
                u[13],
                u[14],
                u[15]
            );
            println!(
                "format:          zu1 v{}, min reader v{}",
                fh.format_version, fh.min_reader_version
            );
            println!("block size:      {} KiB", fh.block_size / 1024);
            println!("epoch:           {}", dh.epoch);
            // Named for what it is rather than "blocks", which sat one
            // line above a size in blocks that was larger by one and
            // invited the reader to find the file short a block. This
            // is the high-water mark and block 0 is not in it.
            println!("high water:      block {}", dh.block_count);
            println!("wal seq:         {}", dh.wal_seq);
            println!(
                "roots:           catalog={} tables={} free={} stats={}",
                dh.catalog_root, dh.table_index_root, dh.free_list_root, dh.stats_root
            );
            print_layout(path);
            let stats = zu::zu1::stats::Stats::load(&mut db).unwrap_or_default();
            match zu::zu1::catalog::Catalog::load(&mut db) {
                Ok(catalog) => {
                    for t in catalog.node_tables() {
                        println!("node table:      {} ({} rows)", t.name, t.node_count);
                    }
                    for t in catalog.rel_tables() {
                        let name = |id| {
                            catalog
                                .node_by_id(id)
                                .map_or("?", |t| t.name.as_str())
                                .to_string()
                        };
                        println!(
                            "rel table:       {} ({} edges, {} to {})",
                            t.name,
                            t.edge_count,
                            name(t.from),
                            name(t.to)
                        );
                        println!("  colors:        {}", color_line(&stats, t));
                    }
                    for t in catalog.graph_types() {
                        println!(
                            "graph type:      {} ({}, {} element types)",
                            t.name,
                            if t.closed { "closed" } else { "open" },
                            t.elements.len()
                        );
                    }
                }
                Err(e) => return command_error("stat", &e),
            }
            ExitCode::SUCCESS
        }
        Err(e) => command_error("stat", &e),
    }
}

/// Reads an edge list by extension: `.csv` takes the comma layout with
/// an optional header, `.parquet` needs the `arrow` feature, anything
/// else is whitespace separated SNAP text with `#` comments.
fn read_edges(path: &std::path::Path) -> zu::Result<Vec<(u32, u32)>> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("parquet") => {
            #[cfg(feature = "arrow")]
            {
                zu::zu1::parquet::read_edge_parquet(path)
            }
            #[cfg(not(feature = "arrow"))]
            {
                Err(zu::ZuError::InvalidArgument(
                    "this zu was built without parquet support, rebuild with --features arrow"
                        .into(),
                ))
            }
        }
        Some("csv") => zu::zu1::graph::read_edge_csv(path),
        _ => zu::zu1::graph::read_edge_list(path),
    }
}

/// Bulk-loads an edge list (SNAP text, csv, or parquet, picked by
/// extension) into a fresh zu1 file and prints the ingest numbers.
fn copy(
    edges_path: &std::path::Path,
    out_path: &std::path::Path,
    reorder: zu::zu1::reorder::Reorder,
) -> ExitCode {
    let started = std::time::Instant::now();
    let mut edges = match read_edges(edges_path) {
        Ok(e) => e,
        Err(e) => return command_error("copy", &e),
    };
    let parsed = started.elapsed();
    let node_count = edges
        .iter()
        .map(|&(s, d)| u64::from(s.max(d)) + 1)
        .max()
        .unwrap_or(0);
    let map = match reorder {
        zu::zu1::reorder::Reorder::None => None,
        zu::zu1::reorder::Reorder::Degree => {
            Some(zu::zu1::reorder::degree_order(node_count, &edges))
        }
        zu::zu1::reorder::Reorder::Bfs => Some(zu::zu1::reorder::bfs_order(node_count, &edges)),
    };
    // A relabeled load persists the original ids as the primary-key
    // index, so lookups keep working on the labels the input used.
    let key_by_row = map.map(|map| {
        zu::zu1::reorder::relabel(&mut edges, &map);
        let mut keys = vec![0u64; node_count as usize];
        for (old, &new) in map.iter().enumerate() {
            keys[new as usize] = old as u64;
        }
        keys
    });
    let mut sorted = edges;
    sorted.sort_unstable();
    sorted.dedup();
    let load_started = std::time::Instant::now();
    let result = (|| {
        let mut db = zu::zu1::file::Zu1File::create(out_path)?;
        let d = zu::zu1::graph::bulk_load_keyed(
            &mut db,
            "node",
            "edge",
            node_count,
            &sorted,
            key_by_row.as_deref(),
        )?;
        // A reordered load also stores the original ids as a property
        // column. The key index alone only answers "which row is key k",
        // which is enough for `{id: $k}` and for `neighbors --key`, but
        // `RETURN n.id` reads a property, and with nothing stored the
        // property path falls back to the row offset. That fallback is
        // right for a load that kept its order and wrong for one that
        // did not, so a reordered file that skips this returns the
        // permuted position where the query asked for the id.
        if let Some(keys) = key_by_row.as_deref() {
            zu::zu1::props::store_props(
                &mut db,
                "node",
                &[("id", zu::zu1::props::PropValues::Int(keys))],
            )?;
        }
        Ok(d)
    })();
    match result {
        Ok(d) => {
            let load = load_started.elapsed();
            let total = started.elapsed();
            let file_bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
            // Adjacency density is quoted per direction: the file holds
            // two CSRs, so bwd block bytes (payload plus 256 KiB block
            // padding, the direction's real disk footprint) are carved
            // out of the fwd number to stay comparable with
            // single-direction figures.
            let bwd_bytes: u64 = d
                .groups
                .iter()
                .map(|g| {
                    (g.bwd.offsets.blocks.len() + g.bwd.neighbors.blocks.len()) as u64
                        * u64::from(zu::zu1::BLOCK_SIZE)
                })
                .sum();
            // The key index is a node-count structure, so it is carved
            // out of the adjacency density and quoted per key instead.
            let key_bytes: u64 = d
                .keys
                .as_ref()
                .map(|k| {
                    (k.keys.blocks.len() + k.rows.blocks.len()) as u64
                        * u64::from(zu::zu1::BLOCK_SIZE)
                })
                .unwrap_or(0);
            let per_edge = |bytes: u64| bytes as f64 * 8.0 / d.edge_count as f64;
            println!(
                "copied {} edges, {} nodes, {} groups",
                d.edge_count,
                d.node_count,
                d.groups.len()
            );
            if key_bytes > 0 {
                println!(
                    "key index: {} bytes, {:.1} bits/key",
                    key_bytes,
                    key_bytes as f64 * 8.0 / d.node_count as f64
                );
            }
            println!(
                "parse {:.2}s, encode+write {:.2}s, total {:.2}s",
                parsed.as_secs_f64(),
                load.as_secs_f64(),
                total.as_secs_f64()
            );
            println!(
                "{:.2} M edges/s end to end, {file_bytes} bytes on disk, {:.2} bits/edge fwd, {:.2} bits/edge bwd",
                d.edge_count as f64 / total.as_secs_f64() / 1e6,
                per_edge(file_bytes - bwd_bytes - key_bytes),
                per_edge(bwd_bytes)
            );
            ExitCode::SUCCESS
        }
        Err(e) => command_error("copy", &e),
    }
}

/// Converts an edge list between the formats copy reads (SNAP text or
/// csv or parquet in, csv or parquet out) or a whole database between
/// engines (.zu1 to .db and back), picked by the file extensions.
fn convert(input: &std::path::Path, output: &std::path::Path) -> ExitCode {
    let started = std::time::Instant::now();
    let ext = |p: &std::path::Path| p.extension().and_then(|e| e.to_str()).map(str::to_owned);
    let engine = match (ext(input).as_deref(), ext(output).as_deref()) {
        (Some("zu1"), Some("db")) => Some(zu::convert::zu1_to_sqlite(input, output)),
        (Some("db"), Some("zu1")) => Some(zu::convert::sqlite_to_zu1(input, output)),
        (Some("zu1") | Some("db"), _) | (_, Some("zu1") | Some("db")) => {
            Some(Err(zu::ZuError::InvalidArgument(
                "engine convert pairs .zu1 with .db; edge list convert writes .csv or .parquet"
                    .into(),
            )))
        }
        _ => None,
    };
    if let Some(result) = engine {
        return match result {
            Ok(()) => {
                println!(
                    "converted {} to {} in {:.2}s",
                    input.display(),
                    output.display(),
                    started.elapsed().as_secs_f64()
                );
                ExitCode::SUCCESS
            }
            Err(e) => command_error("convert", &e),
        };
    }
    let edges = match read_edges(input) {
        Ok(e) => e,
        Err(e) => return command_error("convert", &e),
    };
    let result = match output.extension().and_then(|e| e.to_str()) {
        Some("csv") => (|| -> zu::Result<()> {
            use std::io::Write;
            let file = std::fs::File::create(output)?;
            let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
            writeln!(w, "src,dst")?;
            for &(s, d) in &edges {
                writeln!(w, "{s},{d}")?;
            }
            w.flush()?;
            Ok(())
        })(),
        Some("parquet") => {
            #[cfg(feature = "arrow")]
            {
                zu::zu1::parquet::write_edge_parquet(output, &edges)
            }
            #[cfg(not(feature = "arrow"))]
            {
                Err(zu::ZuError::InvalidArgument(
                    "this zu was built without parquet support, rebuild with --features arrow"
                        .into(),
                ))
            }
        }
        _ => Err(zu::ZuError::InvalidArgument(
            "convert writes .csv or .parquet, picked by the output extension".into(),
        )),
    };
    match result {
        Ok(()) => {
            println!(
                "converted {} edges to {} in {:.2}s",
                edges.len(),
                output.display(),
                started.elapsed().as_secs_f64()
            );
            ExitCode::SUCCESS
        }
        Err(e) => command_error("convert", &e),
    }
}

/// Prints one node's sorted neighbor list via the point-read path, which
/// decodes only the chunks holding the node's offsets and its list.
/// `--in` reads the reverse direction: nodes whose edges point here.
/// `--key` resolves the argument through the primary-key index first,
/// so it takes the original id of a reordered load.
fn neighbors(
    path: &std::path::Path,
    node: u64,
    dir: zu::zu1::graph::Direction,
    by_key: bool,
) -> ExitCode {
    let result = (|| {
        let mut db = zu::zu1::file::Zu1File::open(path)?;
        let mut reader = zu::zu1::graph::GraphReader::load(&mut db)?;
        let row = if by_key {
            match reader.lookup_key(&mut db, node)? {
                Some(row) => row,
                None => return Ok(None),
            }
        } else {
            node
        };
        let mut nbrs = Vec::new();
        reader.neighbors_dir_into(&mut db, row, dir, &mut nbrs)?;
        Ok(Some((row, nbrs)))
    })();
    let label = match dir {
        zu::zu1::graph::Direction::Fwd => "degree",
        zu::zu1::graph::Direction::Bwd => "in-degree",
    };
    match result {
        Ok(Some((row, nbrs))) => {
            if by_key {
                println!("key {node} -> node {row}: {label} {}", nbrs.len());
            } else {
                println!("node {node}: {label} {}", nbrs.len());
            }
            for n in nbrs {
                println!("{n}");
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("key {node}: absent");
            ExitCode::FAILURE
        }
        Err(e) => command_error("neighbors", &e),
    }
}

/// Resolves an original id through the primary-key index. Exits 0 when
/// the key exists and 1 when it does not, so scripts can branch on it.
fn lookup(path: &std::path::Path, key: u64) -> ExitCode {
    let result = (|| {
        let mut db = zu::zu1::file::Zu1File::open(path)?;
        let mut reader = zu::zu1::graph::GraphReader::load(&mut db)?;
        reader.lookup_key(&mut db, key)
    })();
    match result {
        Ok(Some(row)) => {
            println!("key {key}: node {row}");
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("key {key}: absent");
            ExitCode::FAILURE
        }
        Err(e) => command_error("lookup", &e),
    }
}

/// Edge probe via the fence path: decodes at most one neighbor chunk
/// however large the endpoint's degree. Exits 0 when the edge exists and
/// 1 when it does not, so scripts can branch on it.
fn edge(path: &std::path::Path, src: u64, dst: u64, dir: zu::zu1::graph::Direction) -> ExitCode {
    let result = (|| {
        let mut db = zu::zu1::file::Zu1File::open(path)?;
        let reader = zu::zu1::graph::GraphReader::load(&mut db)?;
        reader.has_edge_dir(&mut db, src, dst, dir)
    })();
    let arrow = match dir {
        zu::zu1::graph::Direction::Fwd => "->",
        zu::zu1::graph::Direction::Bwd => "<-",
    };
    match result {
        Ok(true) => {
            println!("{src} {arrow} {dst}: exists");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!("{src} {arrow} {dst}: absent");
            ExitCode::FAILURE
        }
        Err(e) => command_error("edge", &e),
    }
}

/// How `zu query` prints its rows: a column-aligned table for someone
/// reading a terminal, or one JSON object for anything parsing it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Table,
    Json,
}

/// Parses the `query` argument list and runs the statement.
///
/// Flags may come before or after the file, because a caller building
/// the command line programmatically should not have to care. The
/// statement is required: without `-c` there is nothing to run, and
/// there is no interactive fallback here (that is `zu shell`).
fn query_command(args: &[String]) -> ExitCode {
    let mut path: Option<&str> = None;
    let mut source: Option<&str> = None;
    let mut format = Format::Table;
    let mut params: Vec<(String, Value)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // Every flag here takes one value, so consuming it and stepping
        // past both happens in one place.
        let next = |i: &mut usize| -> Option<&str> {
            let v = args.get(*i + 1).map(String::as_str);
            *i += 2;
            v
        };
        match arg {
            "-c" | "--command" => match next(&mut i) {
                Some(s) => source = Some(s),
                None => return usage_error(QUERY_USAGE),
            },
            "-p" | "--param" => match next(&mut i).map(parse_param) {
                Some(Some(p)) => params.push(p),
                _ => return usage_error(QUERY_USAGE),
            },
            "--format" | "-f" => match next(&mut i) {
                Some("json") => format = Format::Json,
                Some("table") => format = Format::Table,
                _ => return usage_error(QUERY_USAGE),
            },
            _ if arg.starts_with('-') => return usage_error(QUERY_USAGE),
            _ if path.is_none() => {
                path = Some(arg);
                i += 1;
            }
            _ => return usage_error(QUERY_USAGE),
        }
    }
    match (path, source) {
        (Some(path), Some(source)) => query(std::path::Path::new(path), source, format, &params),
        _ => usage_error(QUERY_USAGE),
    }
}

/// Splits `name=value` and types the value by what it parses as: an
/// integer, then a float, then a string.
///
/// That covers the `{id: $id}` and `$seed` bindings queries actually
/// use and needs no shell quoting for any of them. A value meant to
/// stay a string but spelled like a number cannot be expressed. Fixing
/// that means a cast syntax on the command line, and the CLI should not
/// invent one before the grammar has picked its own.
fn parse_param(arg: &str) -> Option<(String, Value)> {
    let (name, text) = arg.split_once('=')?;
    if name.is_empty() {
        return None;
    }
    let value = if let Ok(i) = text.parse::<i64>() {
        Value::Int(i)
    } else if let Ok(f) = text.parse::<f64>() {
        Value::Float(f)
    } else {
        Value::Str(text.to_owned())
    };
    Some((name.to_owned(), value))
}

/// Opens the file, runs the statement, and prints the result. Errors go
/// to stderr and exit 1, in both formats: a caller that reads stdout as
/// JSON gets either a whole result object or nothing, never a diagnostic
/// it has to tell apart from data.
fn query(
    path: &std::path::Path,
    source: &str,
    format: Format,
    params: &[(String, Value)],
) -> ExitCode {
    let bound: Vec<(&str, Value)> = params
        .iter()
        .map(|(n, v)| (n.as_str(), v.clone()))
        .collect();
    let result = (|| {
        let mut db = zu::zu1::file::Zu1File::open(path)?;
        zu::query::run(source, &mut db, &bound)
    })();
    match result {
        Ok(r) => {
            print!(
                "{}",
                match format {
                    Format::Json => render_json(&r),
                    Format::Table => render_table(&r),
                }
            );
            ExitCode::SUCCESS
        }
        Err(e) => command_error("query", &e),
    }
}

/// Renders a result as one JSON object:
/// `{"gqlstatus":"00000","columns":[...],"rows":[[...]]}`, with a
/// `"notices"` array when the statement raised a condition it survived.
/// Hand-rolled because the CLI carries no JSON crate; T7 caps the binary
/// at 15 MiB and this is the only place that needs one.
///
/// `gqlstatus` is always present, because a statement that succeeded
/// raised a condition too and a reader should not have to infer which one
/// from the shape of the reply. It is the field a conformance harness
/// grades, and leaving it out would mean the harness derives the code
/// from the row count, which grades the harness rather than the engine.
///
/// `notices` is omitted rather than empty on the common path. A reader
/// that wants it can ask for the key; a reader that does not pays no
/// bytes for it, and almost every statement raises nothing.
fn render_json(r: &QueryResult) -> String {
    let mut out = String::from("{\"gqlstatus\":");
    write_json_str(&mut out, r.status().code());
    out.push_str(",\"columns\":[");
    for (i, c) in r.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_str(&mut out, c);
    }
    out.push_str("],\"rows\":[");
    for (i, row) in r.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, v) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            write_json_value(&mut out, v);
        }
        out.push(']');
    }
    out.push(']');
    if !r.notices.is_empty() {
        out.push_str(",\"notices\":[");
        for (i, n) in r.notices.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_diagnostic(&mut out, n);
        }
        out.push(']');
    }
    out.push_str("}\n");
    out
}

/// One diagnostic record on the wire. The standard's code and the
/// standard's own text go in fields of their own, apart from zu's
/// message, so a harness grades the code and never has to parse prose.
pub(crate) fn write_json_diagnostic(out: &mut String, d: &DiagnosticRecord) {
    out.push_str("{\"gqlstatus\":");
    write_json_str(out, d.status.code());
    out.push_str(",\"condition\":");
    write_json_str(out, &d.status.standard_text());
    out.push_str(",\"severity\":");
    write_json_str(out, severity_name(d.severity()));
    out.push_str(",\"message\":");
    write_json_str(out, &d.detail);
    out.push('}');
}

pub(crate) fn severity_name(s: Severity) -> &'static str {
    match s {
        Severity::Success => "success",
        Severity::NoData => "no data",
        Severity::Warning => "warning",
        Severity::Informational => "informational",
        Severity::Exception => "exception",
    }
}

fn write_json_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        // JSON has no NaN and no infinity, and a reader that has to
        // guess what a bare `NaN` token meant is worse off than one
        // that reads null.
        //
        // The debug formatting is deliberate: it keeps the point on a
        // whole number, so 3.0 goes out as `3.0` and not `3`. JSON has
        // one number type and readers recover the distinction from the
        // token, so a float written as `3` arrives as an integer and a
        // caller comparing types sees the wrong one.
        Value::Float(f) if f.is_finite() => {
            let _ = write!(out, "{f:?}");
        }
        Value::Float(_) => out.push_str("null"),
        Value::Str(s) => write_json_str(out, s),
        // A temporal value goes out as the text it was written
        // with. JSON has no date, and a reader that gets a number of
        // days has to know which epoch to count from.
        Value::Temporal(t) => write_json_str(out, &t.to_string()),
        Value::Node { table, offset } => {
            let _ = write!(out, "{{\"table\":{table},\"offset\":{offset}}}");
        }
        Value::Rel { table, src, dst } => {
            let _ = write!(out, "{{\"table\":{table},\"src\":{src},\"dst\":{dst}}}");
        }
        Value::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_value(out, item);
            }
            out.push(']');
        }
        // A record goes out as a JSON object. The fields are held in
        // name order, so the object is too, which makes two records
        // with the same fields one line of output however the query
        // spelled them.
        Value::Record(fields) => {
            out.push('{');
            for (i, (name, value)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_str(out, name);
                out.push(':');
                write_json_value(out, value);
            }
            out.push('}');
        }
        // A path goes out as the array of its elements, which is what
        // it is: nodes and edges alternating, a node at each end.
        Value::Path(elements) => {
            out.push('[');
            for (i, element) in elements.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_value(out, element);
            }
            out.push(']');
        }
        // The executor settles every PMR chain into an edge list before
        // a value leaves the pipeline, so a result never carries one.
        Value::Chain(_) => out.push_str("null"),
    }
}

fn write_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Renders a result as an aligned table with a row count underneath.
/// Column widths come from the widest cell, so a wide list value widens
/// its own column and leaves the rest alone.
fn render_table(r: &QueryResult) -> String {
    let cells: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| row.iter().map(display_value).collect())
        .collect();
    let widths: Vec<usize> = r
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            cells
                .iter()
                .filter_map(|row| row.get(i))
                .map(String::len)
                .chain(std::iter::once(c.len()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    let line = |out: &mut String, row: &[String]| {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let pad = widths
                .get(i)
                .copied()
                .unwrap_or(0)
                .saturating_sub(cell.len());
            out.push_str(cell);
            if i + 1 < row.len() {
                for _ in 0..pad {
                    out.push(' ');
                }
            }
        }
        out.push('\n');
    };
    line(&mut out, &r.columns);
    for row in &cells {
        line(&mut out, row);
    }
    let n = r.rows.len();
    let _ = writeln!(out, "({n} row{})", if n == 1 { "" } else { "s" });
    out
}

fn display_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Str(s) => s.clone(),
        Value::Node { table, offset } => format!("({table}:{offset})"),
        Value::Rel { table, src, dst } => format!("[{table}:{src}->{dst}]"),
        Value::List(items) => {
            let parts: Vec<String> = items.iter().map(display_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Temporal(t) => t.to_string(),
        Value::Record(fields) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(name, value)| format!("{name}: {}", display_value(value)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        // A path reads as the walk it is, so the arrow between two
        // elements is the reader's clue that this is not a list of the
        // same things.
        Value::Path(elements) => {
            let parts: Vec<String> = elements.iter().map(display_value).collect();
            parts.join("-")
        }
        Value::Chain(_) => "path".to_owned(),
    }
}

/// One line describing a rel table's COLOR summary: how many colors it
/// holds, when it was built, and how far the table has moved since.
/// The drift is the number that decides whether the optimizer is still
/// steering by the coloring, so it is the point of the line.
fn color_line(stats: &zu::zu1::stats::Stats, rel: &zu::zu1::catalog::RelTable) -> String {
    let Some(colors) = stats.rels.get(&rel.id).and_then(|r| r.colors.as_ref()) else {
        return "none, run zu analyze".to_string();
    };
    let built = format!(
        "{} colors, built at epoch {} over {} edges",
        colors.counts.len(),
        colors.epoch,
        colors.edges
    );
    if colors.edges == 0 || colors.edges == rel.edge_count {
        return format!("{built}, current");
    }
    let drift = rel.edge_count as f64 / colors.edges as f64;
    format!("{built}, {drift:.2}x off the table, run zu analyze")
}

/// Rebuilds every rel table's COLOR summary and checkpoints. This is
/// the only way to refresh them: writes land under a summary without
/// moving it, and the optimizer scales what it can and then stops
/// trusting the coloring altogether once the table has drifted far
/// enough, so a graph that has taken a lot of writes wants this run.
fn analyze(path: &std::path::Path) -> ExitCode {
    let mut db = match zu::zu1::file::Zu1File::open(path) {
        Ok(db) => db,
        Err(e) => return command_error("analyze", &e),
    };
    let started = std::time::Instant::now();
    if let Err(e) = zu::zu1::colors::analyze(&mut db) {
        return command_error("analyze", &e);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let stats = match zu::zu1::stats::Stats::load(&mut db) {
        Ok(stats) => stats,
        Err(e) => return command_error("analyze", &e),
    };
    let catalog = match zu::zu1::catalog::Catalog::load(&mut db) {
        Ok(catalog) => catalog,
        Err(e) => return command_error("analyze", &e),
    };
    for rel in catalog.rel_tables() {
        let colors = stats.rels.get(&rel.id).and_then(|r| r.colors.as_ref());
        match colors {
            Some(c) => println!(
                "{}: {} colors over {} edges at epoch {}",
                rel.name,
                c.counts.len(),
                c.edges,
                c.epoch
            ),
            None => println!("{}: no summary, the table holds no edges", rel.name),
        }
    }
    println!("analyzed {} in {elapsed:.2}s", path.display());
    ExitCode::SUCCESS
}

fn verify(path: &std::path::Path) -> ExitCode {
    match zu::zu1::verify(path) {
        Ok(bytes) => {
            println!("{}: ok, {bytes} meta bytes verified", path.display());
            ExitCode::SUCCESS
        }
        Err(e) => command_error("verify", &e),
    }
}

fn usage_error(usage: &str) -> ExitCode {
    eprintln!("usage: {usage}");
    ExitCode::FAILURE
}

fn command_error(cmd: &str, err: &zu::ZuError) -> ExitCode {
    eprintln!("zu {cmd}: {err}");
    ExitCode::FAILURE
}

fn print_usage() {
    println!("zu {VERSION}: embedded property-graph database");
    println!();
    println!("usage: zu <command> [args]");
    println!();
    println!(
        "commands: shell, query, copy, convert, verify, stat, analyze, neighbors [--in] [--key], edge [--in], lookup, conformance, bench"
    );
    println!("(implemented milestone by milestone, see the repo issues)");
    println!();
    println!("{QUERY_USAGE}");
    println!("zu shell <file.zu1> [--format jsonl]");
    println!("{STAT_USAGE}");
    println!("zu conformance --declare [--format toml|json] | --verify <report.json>");
    println!("zu conformance --tally <report.json> | --scoreboard <tally.json>...");
    println!("zu conformance --regressed <report.json> <baseline.json>");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reordered load must keep answering in the id space the input
    /// used. The key index already does that for `{id: $k}` lookups;
    /// this pins the property side, which reads a stored column and
    /// used to fall back to the row offset and hand back the permuted
    /// position instead of the id.
    #[test]
    fn a_reordered_copy_keeps_ids_readable_as_properties() {
        let dir = tempfile::tempdir().expect("tempdir");
        let edges_path = dir.path().join("edges.txt");
        let db_path = dir.path().join("out.zu1");
        // Node 10 is the hub, so degree ordering certainly moves it off
        // row 10 and the offset fallback cannot pass by coincidence.
        std::fs::write(&edges_path, "10 11\n10 12\n10 13\n11 13\n12 13\n13 14\n")
            .expect("write edges");
        assert_eq!(
            copy(&edges_path, &db_path, zu::zu1::reorder::Reorder::Degree),
            ExitCode::SUCCESS
        );

        let mut db = zu::zu1::file::Zu1File::open(&db_path).expect("open");
        let r = zu::query::run(
            "MATCH (n:node {id: $id}) RETURN n.id AS id",
            &mut db,
            &[("id", Value::Int(10))],
        )
        .expect("point read");
        assert_eq!(r.rows, [[Value::Int(10)]], "id came back as a row offset");

        // Neighbors read the same way, so a traversal reports the ids
        // the caller can ask about again.
        let r = zu::query::run(
            "MATCH (a:node {id: $id})-[:edge]->(b) RETURN b.id AS id ORDER BY id",
            &mut db,
            &[("id", Value::Int(10))],
        )
        .expect("one hop");
        assert_eq!(
            r.rows,
            [[Value::Int(11)], [Value::Int(12)], [Value::Int(13)],]
        );
    }

    #[test]
    fn params_take_their_type_from_the_text() {
        assert_eq!(parse_param("id=42"), Some(("id".into(), Value::Int(42))));
        assert_eq!(
            parse_param("t=-7"),
            Some(("t".into(), Value::Int(-7))),
            "a negative id is an id"
        );
        assert_eq!(
            parse_param("r=0.5"),
            Some(("r".into(), Value::Float(0.5))),
            "anything with a point is a float"
        );
        assert_eq!(
            parse_param("name=Ada"),
            Some(("name".into(), Value::Str("Ada".into())))
        );
        // Only the first `=` splits, so a value may contain one.
        assert_eq!(
            parse_param("q=a=b"),
            Some(("q".into(), Value::Str("a=b".into())))
        );
        // An empty value is a string, not an error: the query decides
        // whether an empty name matches anything.
        assert_eq!(
            parse_param("s="),
            Some(("s".into(), Value::Str(String::new())))
        );
        assert_eq!(parse_param("nope"), None, "no '=' is not a binding");
        assert_eq!(
            parse_param("=5"),
            None,
            "a nameless parameter binds nothing"
        );
    }

    #[test]
    fn json_output_is_one_object_per_result() {
        let r = QueryResult {
            columns: vec!["n".into(), "name".into()],
            rows: vec![
                vec![Value::Int(3), Value::Str("Ada".into())],
                vec![Value::Null, Value::Bool(true)],
            ],
            notices: Vec::new(),
        };
        assert_eq!(
            render_json(&r),
            "{\"gqlstatus\":\"00000\",\"columns\":[\"n\",\"name\"],\
             \"rows\":[[3,\"Ada\"],[null,true]]}\n"
        );

        // Empty is still a well-formed object, so a caller parsing
        // stdout never has to special-case "no rows". It also still
        // completes with 00000: a query whose binding table came back
        // empty succeeded, it did not raise `02000 no data`.
        let empty = QueryResult {
            columns: vec!["d".into()],
            rows: vec![],
            notices: Vec::new(),
        };
        assert_eq!(
            render_json(&empty),
            "{\"gqlstatus\":\"00000\",\"columns\":[\"d\"],\"rows\":[]}\n"
        );

        // No projection is the one case that reports something else.
        let omitted = QueryResult::new(Vec::new(), Vec::new());
        assert_eq!(
            render_json(&omitted),
            "{\"gqlstatus\":\"00001\",\"columns\":[],\"rows\":[]}\n"
        );
    }

    #[test]
    fn a_surviving_condition_rides_with_the_rows() {
        use zu::gqlstatus::codes;

        let mut r = QueryResult::new(vec!["c".into()], vec![vec![Value::Int(2)]]);
        r.notice(DiagnosticRecord::new(
            codes::C01G11,
            "avg(n.age) skipped 3 nulls",
        ));
        assert_eq!(
            render_json(&r),
            "{\"gqlstatus\":\"00000\",\"columns\":[\"c\"],\"rows\":[[2]],\
             \"notices\":[{\"gqlstatus\":\"01G11\",\
             \"condition\":\"warning, null value eliminated in set function\",\
             \"severity\":\"warning\",\"message\":\"avg(n.age) skipped 3 nulls\"}]}\n"
        );
    }

    #[test]
    fn the_same_warning_from_every_group_is_reported_once() {
        use zu::gqlstatus::codes;

        let mut r = QueryResult::new(vec!["c".into()], vec![]);
        for _ in 0..1000 {
            r.notice(DiagnosticRecord::new(codes::C01G11, "nulls skipped"));
        }
        assert_eq!(r.notices.len(), 1);
    }

    #[test]
    fn json_escapes_what_json_cannot_carry_raw() {
        let r = QueryResult {
            columns: vec!["s".into()],
            rows: vec![
                vec![Value::Str("say \"hi\"\n\tpath\\to".into())],
                vec![Value::Str("bell\u{7}".into())],
                // NaN and infinity have no JSON spelling; null is the
                // one a reader cannot misparse.
                vec![Value::Float(f64::NAN)],
                vec![Value::Float(f64::INFINITY)],
            ],
            notices: Vec::new(),
        };
        assert_eq!(
            render_json(&r),
            "{\"gqlstatus\":\"00000\",\"columns\":[\"s\"],\
             \"rows\":[[\"say \\\"hi\\\"\\n\\tpath\\\\to\"],\
             [\"bell\\u0007\"],[null],[null]]}\n"
        );
    }

    /// JSON has one number type, so a reader tells a float from an
    /// integer by whether the token has a point or an exponent in it.
    /// A whole float written without one arrives as an integer, and a
    /// caller that checks types sees the wrong one.
    #[test]
    fn a_whole_float_keeps_its_point() {
        let r = QueryResult {
            columns: vec!["f".into()],
            rows: vec![
                vec![Value::Float(3.0)],
                vec![Value::Float(-0.25)],
                vec![Value::Float(0.0)],
                vec![Value::Int(3)],
            ],
            notices: Vec::new(),
        };
        assert_eq!(
            render_json(&r),
            "{\"gqlstatus\":\"00000\",\"columns\":[\"f\"],\
             \"rows\":[[3.0],[-0.25],[0.0],[3]]}\n"
        );
    }

    #[test]
    fn json_carries_nodes_rels_and_lists() {
        let r = QueryResult {
            columns: vec!["a".into(), "e".into(), "p".into()],
            rows: vec![vec![
                Value::Node {
                    table: 0,
                    offset: 7,
                },
                Value::Rel {
                    table: 1,
                    src: 7,
                    dst: 9,
                },
                Value::List(vec![Value::Int(1), Value::List(vec![Value::Int(2)])]),
            ]],
            notices: Vec::new(),
        };
        assert_eq!(
            render_json(&r),
            "{\"gqlstatus\":\"00000\",\"columns\":[\"a\",\"e\",\"p\"],\
             \"rows\":[[{\"table\":0,\"offset\":7},\
             {\"table\":1,\"src\":7,\"dst\":9},[1,[2]]]]}\n"
        );
    }

    #[test]
    fn table_output_aligns_on_the_widest_cell() {
        let r = QueryResult {
            columns: vec!["id".into(), "name".into()],
            rows: vec![
                vec![Value::Int(1), Value::Str("Ada".into())],
                vec![Value::Int(1000), Value::Str("Grace".into())],
            ],
            notices: Vec::new(),
        };
        assert_eq!(
            render_table(&r),
            "id    name\n1     Ada\n1000  Grace\n(2 rows)\n"
        );

        let one = QueryResult {
            columns: vec!["n".into()],
            rows: vec![vec![Value::Int(5)]],
            notices: Vec::new(),
        };
        assert_eq!(one.rows.len(), 1);
        assert_eq!(render_table(&one), "n\n5\n(1 row)\n");
    }
}
