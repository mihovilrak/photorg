//! Stage-by-stage benchmark against the memory NFR.
//!
//!     cargo run --release --example bench -- <count> [work-dir] [--scan-only] [--keep]
//!
//! Generates `<count>` small JPEGs with real EXIF, then times scan, EXIF,
//! plan, and copy separately and reports peak RSS after each. Pointing
//! `work-dir` at an HDD or a mounted share is how the storage comparison in
//! `BENCHMARKS.md` is produced; it defaults to the system temp directory.
//!
//! `--scan-only` stops after the plan, which is what the memory NFR is about
//! and the only way a million files finish in reasonable time. `--keep` leaves
//! the generated tree behind, and a later run reuses it instead of spending
//! half an hour writing it again.
//!
//! Synthetic files are ~1 KB, so the copy stage measures per-file overhead
//! rather than bulk throughput — that is the part that scales with count.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use rayon::prelude::*;

use photorg::config::{DedupMode, Mode, Options};
use photorg::{execute, plan, scan};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let mut positional = args.iter().filter(|a| !a.starts_with("--"));
    let count: usize = positional
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or_else(|| exit("usage: bench <count> [work-dir] [--scan-only] [--keep]"));
    let (scan_only, keep) = (flag("--scan-only"), flag("--keep"));
    let base = positional
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("po-bench-{count}"));

    let source = base.join("src");
    let dest = base.join("out");
    let _ = std::fs::remove_dir_all(&dest);
    let reused = std::fs::metadata(source.join(".complete")).is_ok();
    if !reused {
        let _ = std::fs::remove_dir_all(&source);
        std::fs::create_dir_all(&source).expect("cannot create the bench source tree");
    }

    let t = Instant::now();
    if reused {
        println!("generate  reusing the tree in {}", source.display());
    } else {
        generate(&source, count);
        std::fs::write(source.join(".complete"), []).expect("cannot mark the tree complete");
        println!(
            "generate  {count} files in {:.2}s",
            t.elapsed().as_secs_f64()
        );
    }
    println!(
        "{:<10} {:>10} {:>12} {:>12}",
        "stage", "seconds", "files/s", "peak RSS"
    );

    let opts = Options {
        source: source.clone(),
        dest: dest.clone(),
        mode: Mode::Copy,
        // Hashing every file would measure blake3, not the pipeline.
        dedup: DedupMode::Off,
        ..Default::default()
    };

    let t = Instant::now();
    let candidates = scan::collect_candidates(&opts.source, &opts, &Default::default());
    stage("scan", t, candidates.len());

    let t = Instant::now();
    let scanned = scan::extract_all(candidates, &opts, || {});
    stage("exif", t, scanned.photos.len());

    let t = Instant::now();
    let built = plan::build(&scanned.photos, &opts, None, &Default::default())
        .unwrap_or_else(|e| exit(&format!("plan failed: {e}")));
    stage("plan", t, built.ops.len());

    if !scan_only {
        let t = Instant::now();
        let stats = execute::run(&built, &opts, &AtomicBool::new(false), |_, _| {});
        stage("copy", t, stats.copied);
    }

    let _ = std::fs::remove_dir_all(&dest);
    if !keep {
        let _ = std::fs::remove_dir_all(&base);
    }
}

fn stage(name: &str, t: Instant, items: usize) {
    let secs = t.elapsed().as_secs_f64();
    println!(
        "{name:<10} {secs:>10.2} {:>12.0} {:>12}",
        items as f64 / secs.max(f64::EPSILON),
        peak_rss().map_or("n/a".into(), |b| format!("{:.0} MB", b as f64 / 1e6))
    );
}

/// One JPEG with EXIF, rewritten per file so dates spread across ~12 years —
/// a single date would collapse every group and flatter the planner.
fn generate(source: &Path, count: usize) {
    let template =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dated.jpg"))
            .expect("missing tests/fixtures/dated.jpg");
    let at = find_datetime(&template).expect("the fixture has no DateTimeOriginal");

    // 500 files per directory: deep enough that traversal is not one flat read.
    // Written in parallel — at a million files the setup otherwise costs more
    // than everything being measured.
    (0..count.div_ceil(500)).into_par_iter().for_each(|chunk| {
        let dir = source.join(format!("d{chunk:04}"));
        std::fs::create_dir_all(&dir).expect("cannot create a bench directory");
        for i in chunk * 500..(chunk * 500 + 500).min(count) {
            let mut bytes = template.clone();
            let stamp = format!(
                "{:04}:{:02}:{:02} {:02}:{:02}:{:02}",
                2014 + (i % 12),
                1 + (i % 12),
                1 + (i % 28),
                i % 24,
                i % 60,
                i % 60
            );
            bytes[at..at + 19].copy_from_slice(stamp.as_bytes());
            // Unique tail bytes so dedup and conflict handling see distinct files.
            bytes.extend_from_slice(&i.to_le_bytes());
            std::fs::write(dir.join(format!("img_{i:07}.jpg")), &bytes)
                .expect("cannot write a file");
        }
    });
}

/// Locate the ASCII `YYYY:MM:DD HH:MM:SS` the fixture stores, so the stamp can
/// be patched in place without re-encoding EXIF.
fn find_datetime(bytes: &[u8]) -> Option<usize> {
    bytes.windows(19).position(|w| {
        w[4] == b':'
            && w[7] == b':'
            && w[10] == b' '
            && w[13] == b':'
            && w[16] == b':'
            && w.iter()
                .enumerate()
                .all(|(i, &c)| matches!(i, 4 | 7 | 10 | 13 | 16) || c.is_ascii_digit())
    })
}

#[cfg(target_os = "linux")]
fn peak_rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn peak_rss() -> Option<u64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // ru_maxrss is bytes on Darwin, kilobytes on Linux.
    (unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0).then(|| usage.ru_maxrss as u64)
}

#[cfg(windows)]
#[repr(C)]
struct MemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set: usize,
    working_set: usize,
    quota_peak_paged_pool: usize,
    quota_paged_pool: usize,
    quota_peak_nonpaged_pool: usize,
    quota_nonpaged_pool: usize,
    pagefile: usize,
    peak_pagefile: usize,
}

// K32GetProcessMemoryInfo lives in kernel32, so this needs no import library
// and no windows-sys dependency for what is only a benchmark.
#[cfg(windows)]
extern "system" {
    fn GetCurrentProcess() -> isize;
    fn K32GetProcessMemoryInfo(process: isize, counters: *mut MemoryCounters, cb: u32) -> i32;
}

#[cfg(windows)]
fn peak_rss() -> Option<u64> {
    let mut counters: MemoryCounters = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<MemoryCounters>() as u32;
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.peak_working_set as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
fn peak_rss() -> Option<u64> {
    None
}

fn exit(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2);
}
