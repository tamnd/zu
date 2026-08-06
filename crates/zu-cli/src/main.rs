//! The `zu` command-line tool.
//!
//! Subcommands (shell, query, copy, convert, verify, stat, bench) are
//! specified in `docs/10-api-and-tooling.md` and land with their layers.
//! Argument parsing is hand-rolled: the surface is small and G7 caps the
//! binary at 15 MiB, so no clap.

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

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
        Some("stat") => match args.get(1) {
            Some(path) => stat(std::path::Path::new(path)),
            None => usage_error("zu stat <file.zu1>"),
        },
        Some("verify") => match args.get(1) {
            Some(path) => verify(std::path::Path::new(path)),
            None => usage_error("zu verify <file.zu1>"),
        },
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
                _ => usage_error("zu copy [--reorder degree|bfs|none] <edges.txt> <out.zu1>"),
            }
        }
        Some(cmd) => {
            eprintln!("zu: unknown command '{cmd}' (commands arrive with their milestones)");
            ExitCode::FAILURE
        }
    }
}

fn stat(path: &std::path::Path) -> ExitCode {
    match zu::zu1::file::Zu1File::open(path) {
        Ok(db) => {
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
            println!("blocks:          {}", dh.block_count);
            println!("wal seq:         {}", dh.wal_seq);
            println!(
                "roots:           catalog={} tables={} free={} stats={}",
                dh.catalog_root, dh.table_index_root, dh.free_list_root, dh.stats_root
            );
            ExitCode::SUCCESS
        }
        Err(e) => command_error("stat", &e),
    }
}

/// Bulk-loads a whitespace separated `src dst` edge list (SNAP layout,
/// `#` comments) into a fresh zu1 file and prints the ingest numbers.
fn copy(
    edges_path: &std::path::Path,
    out_path: &std::path::Path,
    reorder: zu::zu1::reorder::Reorder,
) -> ExitCode {
    let started = std::time::Instant::now();
    let mut edges = match zu::zu1::graph::read_edge_list(edges_path) {
        Ok(e) => e,
        Err(e) => return command_error("copy", &e),
    };
    let parsed = started.elapsed();
    let node_count = edges
        .iter()
        .map(|&(s, d)| u64::from(s.max(d)) + 1)
        .max()
        .unwrap_or(0);
    match reorder {
        zu::zu1::reorder::Reorder::None => {}
        zu::zu1::reorder::Reorder::Degree => {
            let map = zu::zu1::reorder::degree_order(node_count, &edges);
            zu::zu1::reorder::relabel(&mut edges, &map);
        }
        zu::zu1::reorder::Reorder::Bfs => {
            let map = zu::zu1::reorder::bfs_order(node_count, &edges);
            zu::zu1::reorder::relabel(&mut edges, &map);
        }
    }
    let mut sorted = edges;
    sorted.sort_unstable();
    sorted.dedup();
    let load_started = std::time::Instant::now();
    let result = (|| {
        let mut db = zu::zu1::file::Zu1File::create(out_path)?;
        zu::zu1::graph::bulk_load(&mut db, node_count, &sorted)
    })();
    match result {
        Ok(d) => {
            let load = load_started.elapsed();
            let total = started.elapsed();
            let file_bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
            let bits_per_edge = file_bytes as f64 * 8.0 / d.edge_count as f64;
            println!(
                "copied {} edges, {} nodes, {} groups",
                d.edge_count,
                d.node_count,
                d.groups.len()
            );
            println!(
                "parse {:.2}s, encode+write {:.2}s, total {:.2}s",
                parsed.as_secs_f64(),
                load.as_secs_f64(),
                total.as_secs_f64()
            );
            println!(
                "{:.2} M edges/s end to end, {file_bytes} bytes on disk, {bits_per_edge:.2} bits/edge",
                d.edge_count as f64 / total.as_secs_f64() / 1e6
            );
            ExitCode::SUCCESS
        }
        Err(e) => command_error("copy", &e),
    }
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
    println!("commands: shell, query, copy, convert, verify, stat, bench");
    println!("(implemented milestone by milestone, see the repo issues)");
}
