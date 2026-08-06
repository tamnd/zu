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
        Some(cmd) => {
            eprintln!("zu: unknown command '{cmd}' (commands arrive with their milestones)");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!("zu {VERSION}: embedded property-graph database");
    println!();
    println!("usage: zu <command> [args]");
    println!();
    println!("commands: shell, query, copy, convert, verify, stat, bench");
    println!("(implemented milestone by milestone, see the repo issues)");
}
