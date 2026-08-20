//! The binary: parse, wire the two passes together, render, exit.
//! All policy lives in the library; this file only sequences it.

mod cli;
mod report;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

use photorg::config::{LocationDepth, Options};
use photorg::geocode::Locator;
use photorg::journal::{self, Journal};
use photorg::{execute, plan, safety, scan};

use cli::Cli;
use report::{Format, Reporter};

/// 0 clean, 1 finished with per-file failures, 2 never started.
const EXIT_OK: u8 = 0;
const EXIT_PARTIAL: u8 = 1;
const EXIT_FATAL: u8 = 2;

fn main() -> ExitCode {
    let cli = Cli::parse();
    env_logger::Builder::new()
        .filter_level(cli.log_level())
        .format_timestamp(None)
        .format_target(false)
        .init();

    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(EXIT_FATAL)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    let format = if cli.json {
        Format::Jsonl
    } else {
        Format::Human
    };
    let quiet = cli.quiet;
    let mut opts = cli.into_options();

    let (source, dest) = safety::prepare(&opts)?;
    opts.source = source;
    opts.dest = dest;

    let cancel = install_cancel_handler()?;

    let scanned = scan_source(&opts, quiet);
    let done = resume_map(&opts)?;

    let mut locator = build_locator(&opts)?;
    let plan = plan::build(&scanned.photos, &opts, locator.as_mut(), &done)?;
    safety::check_space(&opts.dest, plan.stats.pending_bytes, opts.mode)?;

    let journal = match (&opts.journal, opts.dry_run) {
        (Some(path), false) => Some(
            Journal::open(path)
                .with_context(|| format!("cannot write journal {}", path.display()))?,
        ),
        _ => None,
    };

    let reporter = Reporter::new(format, quiet, plan.stats.pending as u64);
    for (path, err) in &scanned.failures {
        reporter.warn(&format!("{}: {err}", path.display()));
    }

    let stats = execute::run(&plan, &opts, &cancel, |op, outcome| {
        reporter.file(&plan, op, outcome);
        if let Some(j) = &journal {
            j.record_if_done(op, outcome.is_ok());
        }
    });
    reporter.finish(
        &plan,
        scanned.photos.len() + scanned.failures.len(),
        &plan.stats,
        &stats,
        opts.dry_run,
    );

    let clean = stats.failed == 0 && scanned.failures.is_empty() && !stats.cancelled;
    Ok(if clean { EXIT_OK } else { EXIT_PARTIAL })
}

fn scan_source(opts: &Options, quiet: bool) -> scan::ScanResult {
    let spinner = report::scan_spinner(quiet);
    let seen = AtomicUsize::new(0);
    let result = scan::scan(opts, || {
        let n = seen.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(bar) = &spinner {
            if n % 64 == 0 {
                bar.set_message(format!("{n} files read"));
            }
        }
    });
    if let Some(bar) = spinner {
        bar.finish_and_clear();
    }
    log::info!(
        "scanned {} files, {} unreadable, {} ignored",
        result.photos.len(),
        result.failures.len(),
        result.ignored
    );
    result
}

fn resume_map(opts: &Options) -> Result<HashMap<PathBuf, PathBuf>> {
    let Some(path) = &opts.resume else {
        return Ok(HashMap::new());
    };
    // A missing journal is an empty one: resuming a run that died before its
    // first write should not itself be an error.
    if !path.exists() {
        log::warn!("resume journal {} does not exist yet", path.display());
        return Ok(HashMap::new());
    }
    let done = journal::load(path)
        .with_context(|| format!("cannot read resume journal {}", path.display()))?;
    log::info!("resuming: {} operations already recorded", done.len());
    Ok(done)
}

/// Only build the geocoder when the rendered paths actually contain a place.
fn build_locator(opts: &Options) -> Result<Option<Locator>> {
    let template = opts.build_template(opts.group)?;
    if !template.uses_location() {
        return Ok(None);
    }
    let depth = opts.location.unwrap_or(LocationDepth::City);
    let locator = Locator::new(depth);
    if locator.is_none() {
        log::warn!("built without the `geocoding` feature: location variables stay empty");
    }
    Ok(locator)
}

/// First Ctrl-C stops issuing new work and lets in-flight copies finish;
/// a second one is an emergency exit.
fn install_cancel_handler() -> Result<Arc<AtomicBool>> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::SeqCst) {
            eprintln!("\ninterrupted twice: exiting now");
            // Nothing unwinds past `exit`, so partial temp files have to go now
            // or they stay in the destination forever.
            let swept = execute::cleanup_temps();
            if swept > 0 {
                eprintln!("removed {swept} partial files");
            }
            std::process::exit(EXIT_PARTIAL as i32);
        }
        eprintln!("\ninterrupted: finishing files already in flight...");
    })
    .context("cannot install the Ctrl-C handler")?;
    Ok(cancel)
}
