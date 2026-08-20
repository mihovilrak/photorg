//! Pass 1: parallel walk, extension filter, and parallel metadata extraction.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use jwalk::WalkDir;
use rayon::prelude::*;

use crate::config::Options;
use crate::formats::{self, FileKind};
use crate::metadata::{self, PhotoMetadata};

/// Temp files this tool writes; never re-ingest our own output.
pub const TMP_PREFIX: &str = ".po-tmp-";

#[derive(Debug, Default)]
pub struct ScanResult {
    pub photos: Vec<PhotoMetadata>,
    pub failures: Vec<(PathBuf, String)>,
    /// Files whose extension is not ours.
    pub ignored: u64,
}

/// A file worth opening. Only the path relative to the scan root is stored:
/// at a million files the absolute copy is tens of megabytes of duplicate
/// prefix, and `root.join(&rel)` reconstructs it for free.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub rel: PathBuf,
    pub kind: FileKind,
}

/// Walk the source tree in parallel and return everything worth opening.
pub fn collect_candidates(root: &Path, opts: &Options, ignored: &AtomicU64) -> Vec<Candidate> {
    let walker = WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(opts.follow_symlinks)
        .parallelism(jwalk::Parallelism::RayonDefaultPool {
            busy_timeout: std::time::Duration::from_secs(1),
        });

    let mut out = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::warn!("walk error: {e}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with(TMP_PREFIX) {
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            ignored.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let Some(kind) = formats::classify(ext) else {
            ignored.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if kind == FileKind::Sidecar && !opts.include_sidecars {
            ignored.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        out.push(Candidate { rel, kind });
    }
    out
}

/// Run metadata extraction across the rayon pool.
///
/// Sorting by relative path afterwards is what makes the whole run
/// reproducible.
pub fn extract_all(
    candidates: Vec<Candidate>,
    opts: &Options,
    on_file: impl Fn() + Sync,
) -> ScanResult {
    let root = &opts.source;
    // Folding straight into the two output vectors, rather than collecting
    // `Vec<Result<..>>` and partitioning it, keeps one copy of the results in
    // memory instead of two -- worth ~100 MB at a million files.
    let (photos, failures) = candidates
        .into_par_iter()
        .fold(
            || (Vec::new(), Vec::new()),
            |(mut photos, mut failures), c| {
                let abs = root.join(&c.rel);
                match metadata::extract(&abs, c.rel, c.kind, opts.filename_dates) {
                    Ok(m) => photos.push(m),
                    Err(e) => failures.push((abs, e.to_string())),
                }
                on_file();
                (photos, failures)
            },
        )
        .reduce(|| (Vec::new(), Vec::new()), merge);

    let mut scan = ScanResult {
        photos,
        failures,
        ignored: 0,
    };
    scan.photos.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    // Failures are reported too, and the fold merges them out of order.
    scan.failures.sort_by(|a, b| a.0.cmp(&b.0));
    scan
}

/// Append the shorter side onto the longer one, so merging a fold tree copies
/// each element as few times as possible.
fn merge<A, B>(
    (mut photos, mut failures): (Vec<A>, Vec<B>),
    (mut other_photos, mut other_failures): (Vec<A>, Vec<B>),
) -> (Vec<A>, Vec<B>) {
    if photos.len() < other_photos.len() {
        std::mem::swap(&mut photos, &mut other_photos);
    }
    if failures.len() < other_failures.len() {
        std::mem::swap(&mut failures, &mut other_failures);
    }
    photos.append(&mut other_photos);
    failures.append(&mut other_failures);
    (photos, failures)
}

/// Full pass 1.
pub fn scan(opts: &Options, on_file: impl Fn() + Sync) -> ScanResult {
    let ignored = AtomicU64::new(0);
    let candidates = collect_candidates(&opts.source, opts, &ignored);
    let mut result = extract_all(candidates, opts, on_file);
    result.ignored = ignored.load(Ordering::Relaxed);
    result
}
