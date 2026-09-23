//! `dsopt` — interactive disk-space reclaimer for build artifacts.
//!
//! Running `dsopt` with no arguments opens a TUI. Passing `--list` prints the
//! same candidates as plain text or JSON so it can be used from scripts.

use clap::Parser;
use dsopt::{
    Candidate, THREAD_RANGE,
    app::App,
    cache_path, format_size, scan, should_skip,
    update::{self, UpdateOutcome},
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dsopt",
    version,
    about = "Find and reclaim disk space from node_modules and Rust target directories",
    long_about = None
)]
struct Cli {
    /// Roots to scan. Defaults to the whole filesystem (/).
    #[arg(value_name = "ROOTS")]
    roots: Vec<PathBuf>,

    /// Scanner threads (1-8).
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(usize))]
    threads: usize,

    /// Print candidates instead of opening the interactive TUI.
    #[arg(long)]
    list: bool,

    /// With --list, emit one JSON object per line.
    #[arg(long, requires = "list")]
    json: bool,

    /// Update dsopt to the newest published release.
    #[arg(long)]
    update: bool,

    /// With --update, reinstall even when the running version is already current.
    #[arg(long, requires = "update")]
    force: bool,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if cli.update {
        return run_update(cli.force);
    }
    if !THREAD_RANGE.contains(&cli.threads) {
        eprintln!(
            "error: --threads must be between {} and {}",
            THREAD_RANGE.start(),
            THREAD_RANGE.end()
        );
        return std::process::ExitCode::from(2);
    }
    let roots = if cli.roots.is_empty() {
        vec![PathBuf::from("/")]
    } else {
        cli.roots.clone()
    };
    let roots: Vec<PathBuf> = roots
        .into_iter()
        .filter(|root| {
            let skip = should_skip(root);
            if skip {
                eprintln!("warning: skipping {}, a protected location", root.display());
            }
            !skip
        })
        .collect();
    if roots.is_empty() {
        eprintln!("error: no scannable roots given");
        return std::process::ExitCode::from(2);
    }

    if cli.list {
        return list(&roots, cli.threads, cli.json);
    }

    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("error: cannot start the terminal: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let outcome = App::new(roots, cli.threads).run(&mut terminal);
    ratatui::restore();
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_update(force: bool) -> std::process::ExitCode {
    println!(
        "dsopt {} · checking crates.io for updates...",
        update::current_version()
    );
    match update::run(force) {
        Ok(UpdateOutcome::AlreadyLatest { current, latest }) => {
            println!("dsopt {current} is already the newest release (latest: {latest}).");
            std::process::ExitCode::SUCCESS
        }
        Ok(UpdateOutcome::Updated { from, to }) => {
            println!("Updated dsopt {from} -> {to}. Run `dsopt --version` to confirm.");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn list(roots: &[PathBuf], threads: usize, json: bool) -> std::process::ExitCode {
    let mut all: Vec<Candidate> = Vec::new();
    for root in roots {
        match scan(root, threads) {
            Ok(result) => all.extend(result.candidates),
            Err(error) => {
                eprintln!("error: {error}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    all.sort_by(|left, right| {
        right
            .size_bytes
            .cmp(&left.size_bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    if json {
        for candidate in &all {
            match serde_json::to_string(candidate) {
                Ok(line) => println!("{line}"),
                Err(error) => {
                    eprintln!("error: {error}");
                    return std::process::ExitCode::FAILURE;
                }
            }
        }
    } else {
        for candidate in &all {
            println!(
                "{:>10}  {:<24}  {}",
                format_size(candidate.size_bytes),
                candidate.kind.label(),
                candidate.path.display()
            );
        }
        let total: u64 = all.iter().map(|candidate| candidate.size_bytes).sum();
        println!(
            "\n{} candidates · {} reclaimable · cache {}",
            all.len(),
            format_size(total),
            cache_path().display()
        );
    }
    std::process::ExitCode::SUCCESS
}
