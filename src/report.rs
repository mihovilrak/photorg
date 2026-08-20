//! Output contract. Machine records go to stdout, everything a human
//! watches goes to stderr, so `--json | jq` works while the bar still renders.

use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;

use photorg::execute::{ExecStats, Outcome};
use photorg::plan::{Action, Operation, OperationPlan, PlanStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Human,
    Jsonl,
}

#[derive(Serialize)]
struct Record<'a> {
    source: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<String>,
    action: Action,
    reason: photorg::plan::Reason,
    bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    duplicate_of: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct Summary<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    source_root: String,
    dest_root: String,
    dry_run: bool,
    scanned: usize,
    planned: usize,
    copied: usize,
    moved: usize,
    overwritten: usize,
    skipped: usize,
    duplicates: usize,
    renamed: usize,
    resumed: usize,
    unshippable: usize,
    failed: usize,
    bytes: u64,
    cancelled: bool,
    unattempted: usize,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    failures: &'a [String],
}

pub struct Reporter {
    format: Format,
    quiet: bool,
    out: Mutex<io::Stdout>,
    bar: Option<ProgressBar>,
    failures: Mutex<Vec<String>>,
}

impl Reporter {
    pub fn new(format: Format, quiet: bool, total: u64) -> Reporter {
        Reporter {
            format,
            quiet,
            out: Mutex::new(io::stdout()),
            bar: (!quiet && progress_allowed()).then(|| make_bar(total)),
            failures: Mutex::new(Vec::new()),
        }
    }

    /// One record per file, emitted identically for dry runs and real runs.
    pub fn file(&self, plan: &OperationPlan, op: &Operation, outcome: &Outcome) {
        if let (Some(bar), true) = (&self.bar, op.action.is_pending()) {
            bar.inc(1);
            bar.set_message(short(&op.source_rel.to_string_lossy()));
        }
        if let Err(e) = outcome {
            let msg = format!("{}: {e}", op.source_rel.display());
            if let Ok(mut f) = self.failures.lock() {
                f.push(msg.clone());
            }
            self.warn(&msg);
        }

        match self.format {
            Format::Jsonl => self.jsonl(op, outcome),
            Format::Human if !self.quiet => self.human(plan, op, outcome),
            Format::Human => {}
        }
    }

    fn jsonl(&self, op: &Operation, outcome: &Outcome) {
        let source = slashes(&op.source_rel.to_string_lossy());
        let record = Record {
            source: &source,
            destination: op.dest_rel.as_ref().map(|d| slashes(&d.to_string_lossy())),
            action: op.action,
            reason: op.reason,
            bytes: op.bytes,
            duplicate_of: op
                .duplicate_of
                .as_ref()
                .map(|d| slashes(&d.to_string_lossy())),
            error: outcome.as_ref().err().map(|e| e.to_string()),
        };
        if let Ok(line) = serde_json::to_string(&record) {
            self.line(&line);
        }
    }

    fn human(&self, _plan: &OperationPlan, op: &Operation, outcome: &Outcome) {
        use photorg::plan::Reason::*;
        let source = slashes(&op.source_rel.to_string_lossy());
        let dest = op
            .dest_rel
            .as_ref()
            .map(|d| slashes(&d.to_string_lossy()))
            .unwrap_or_else(|| "-".into());

        let text = match (outcome, op.reason) {
            (Err(e), _) => format!("{source} -> FAILED: {e}"),
            (_, DuplicateOfDestination) | (_, DuplicateOfSource) => {
                let of = op
                    .duplicate_of
                    .as_ref()
                    .map(|d| slashes(&d.to_string_lossy()))
                    .unwrap_or(dest);
                format!("{source} -> SKIP: duplicate of {of}")
            }
            (_, ExistingFile) if op.action == Action::Skip => {
                format!("{source} -> SKIP: exists {dest}")
            }
            (_, AlreadyDone) => format!("{source} -> SKIP: already done"),
            _ => format!("{source} -> {dest}"),
        };
        self.line(&text);
    }

    fn line(&self, text: &str) {
        if let Ok(mut out) = self.out.lock() {
            // A closed pipe (`| head`) is not an error worth reporting.
            let _ = writeln!(out, "{text}");
        }
    }

    pub fn warn(&self, msg: &str) {
        if !self.quiet {
            match &self.bar {
                Some(bar) => bar.suspend(|| eprintln!("warning: {msg}")),
                None => eprintln!("warning: {msg}"),
            }
        }
    }

    /// The final line: a summary object in JSONL, a short block for humans.
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        &self,
        plan: &OperationPlan,
        scanned: usize,
        plan_stats: &PlanStats,
        exec: &ExecStats,
        dry_run: bool,
    ) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
        let failures = self.failures.lock().map(|f| f.clone()).unwrap_or_default();

        match self.format {
            Format::Jsonl => {
                let summary = Summary {
                    kind: "summary",
                    source_root: slashes(&plan.source_root.to_string_lossy()),
                    dest_root: slashes(&plan.dest_root.to_string_lossy()),
                    dry_run,
                    scanned,
                    planned: plan_stats.pending,
                    copied: exec.copied,
                    moved: exec.moved,
                    overwritten: exec.overwritten,
                    skipped: exec.skipped,
                    duplicates: plan_stats.duplicates,
                    renamed: plan_stats.renamed,
                    resumed: plan_stats.resumed,
                    unshippable: plan_stats.unshippable,
                    failed: exec.failed,
                    bytes: exec.bytes,
                    cancelled: exec.cancelled,
                    unattempted: exec.unattempted,
                    failures: &failures,
                };
                if let Ok(line) = serde_json::to_string(&summary) {
                    self.line(&line);
                }
            }
            Format::Human => {
                if self.quiet {
                    return;
                }
                let verb = if dry_run {
                    "would process"
                } else {
                    "processed"
                };
                eprintln!();
                eprintln!(
                    "{verb} {} of {scanned} files ({})",
                    exec.copied + exec.moved + exec.overwritten,
                    human_bytes(exec.bytes)
                );
                eprintln!(
                    "  duplicates {}  renamed {}  existing {}  resumed {}  unshippable {}",
                    plan_stats.duplicates,
                    plan_stats.renamed,
                    plan_stats.skipped_existing,
                    plan_stats.resumed,
                    plan_stats.unshippable
                );
                if exec.failed > 0 {
                    eprintln!("  {} files failed", exec.failed);
                }
                if exec.cancelled {
                    eprintln!(
                        "  interrupted: {} operations were not started",
                        exec.unattempted
                    );
                }
            }
        }
    }
}

/// Auto-disable when stderr is not a TTY, and honor `NO_COLOR`.
fn progress_allowed() -> bool {
    io::stderr().is_terminal()
}

/// Pass 1 has no total worth showing: a spinner with a running count is honest,
/// a bar over an unknown tree is not.
pub fn scan_spinner(quiet: bool) -> Option<ProgressBar> {
    if quiet || !progress_allowed() {
        return None;
    }
    let bar = ProgressBar::new_spinner();
    if let Ok(style) = ProgressStyle::with_template("{spinner} scanning {msg}") {
        bar.set_style(style);
    }
    bar.enable_steady_tick(Duration::from_millis(120));
    Some(bar)
}

fn make_bar(total: u64) -> ProgressBar {
    let bar = ProgressBar::new(total);
    let plain = std::env::var_os("NO_COLOR").is_some();
    // Rate, no ETA: an ETA over mixed file sizes is a guess presented as fact.
    let template = if plain {
        "{pos}/{len} [{bar:30}] {per_sec} {msg}"
    } else {
        "{spinner:.green} {pos}/{len} [{bar:30.cyan/blue}] {per_sec} {msg}"
    };
    if let Ok(style) = ProgressStyle::with_template(template) {
        bar.set_style(style.progress_chars("=> "));
    }
    bar.enable_steady_tick(Duration::from_millis(120));
    bar
}

/// Windows canonicalization returns verbatim paths; the prefix is correct but
/// unreadable, and no consumer of the JSONL needs it either.
fn slashes(s: &str) -> String {
    let s = match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => s.trim_start_matches(r"\\?\").to_string(),
    };
    s.replace('\\', "/")
}

fn short(s: &str) -> String {
    match s.rsplit_once('/').or_else(|| s.rsplit_once('\\')) {
        Some((_, name)) => name.to_string(),
        None => s.to_string(),
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
