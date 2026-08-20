//! Pass 2: apply an `OperationPlan`. This phase decides
//! nothing — every destination and suffix was already fixed by the planner.

use std::collections::HashSet;
use std::fs::{self, File, FileTimes};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use crate::config::Options;
use crate::error::FileError;
use crate::plan::{Action, Operation, OperationPlan, Ops};
use crate::scan::TMP_PREFIX;

#[derive(Debug, Default, Clone)]
pub struct ExecStats {
    pub copied: usize,
    pub moved: usize,
    pub overwritten: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes: u64,
    /// Ctrl-C arrived and the remaining operations were never issued.
    pub cancelled: bool,
    /// How many those were. A cancelled run's record stream stops early, so
    /// this is the only place the difference is visible.
    pub unattempted: usize,
}

/// What happened to one operation. Skips are reported too, so `--dry-run` and a
/// real run emit the same record stream.
pub type Outcome = Result<(), FileError>;

/// Apply the plan. `cancel` is polled before every operation; in-flight copies
/// are allowed to finish so nothing is left half-written.
pub fn run<F>(plan: &OperationPlan, opts: &Options, cancel: &AtomicBool, on_done: F) -> ExecStats
where
    F: Fn(&Operation, &Outcome) + Sync,
{
    let dirs = RwLock::new(HashSet::new());
    let stats = Counters::default();

    let apply = |op: &Operation| {
        if !op.action.is_pending() {
            stats.skipped.fetch_add(1, Ordering::Relaxed);
            on_done(op, &Ok(()));
            return;
        }
        if cancel.load(Ordering::Relaxed) {
            stats.cancelled.store(true, Ordering::Relaxed);
            stats.unattempted.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let outcome = if opts.dry_run {
            Ok(())
        } else {
            perform(plan, op, &dirs)
        };

        match &outcome {
            Ok(()) => {
                match op.action {
                    Action::Copy => stats.copied.fetch_add(1, Ordering::Relaxed),
                    Action::Move => stats.moved.fetch_add(1, Ordering::Relaxed),
                    Action::Overwrite => stats.overwritten.fetch_add(1, Ordering::Relaxed),
                    Action::Skip => stats.skipped.fetch_add(1, Ordering::Relaxed),
                };
                stats.bytes.fetch_add(op.bytes, Ordering::Relaxed);
            }
            Err(_) => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
        on_done(op, &outcome);
    };

    // Copies get their own small pool: concurrent large sequential writes thrash
    // seeks on HDDs and network shares.
    let workers = opts.copy_workers.max(1);
    let pool = if workers == 1 || opts.dry_run {
        None
    } else {
        match rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|i| format!("po-copy-{i}"))
            .build()
        {
            Ok(pool) => Some(pool),
            Err(e) => {
                log::warn!("copy pool unavailable ({e}); running serially");
                None
            }
        }
    };
    let run_chunk = |ops: &[Operation]| match &pool {
        Some(pool) => pool.install(|| {
            use rayon::prelude::*;
            ops.par_iter().for_each(apply);
        }),
        None => ops.iter().for_each(&apply),
    };

    match &plan.ops {
        Ops::Memory(ops) => run_chunk(ops),
        // A spilled plan is read back a chunk at a time, so pass 2 costs the
        // chunk rather than the whole plan.
        Ops::Spilled(spill) => match spill.chunks() {
            Ok(mut chunks) => loop {
                if cancel.load(Ordering::Relaxed) {
                    stats.cancelled.store(true, Ordering::Relaxed);
                    break;
                }
                match chunks.next() {
                    Some(Ok(ops)) => run_chunk(&ops),
                    Some(Err(e)) => {
                        log::error!("cannot read back the spilled plan: {e}");
                        stats
                            .failed
                            .fetch_add(chunks.remaining() as u64, Ordering::Relaxed);
                        break;
                    }
                    None => break,
                }
            },
            Err(e) => {
                log::error!("cannot reopen the spilled plan: {e}");
                stats
                    .failed
                    .fetch_add(spill.len() as u64, Ordering::Relaxed);
            }
        },
    }

    stats.finish()
}

fn perform(plan: &OperationPlan, op: &Operation, dirs: &RwLock<HashSet<PathBuf>>) -> Outcome {
    let dest = match plan.dest_abs(op) {
        Some(d) => d,
        None => return Err(FileError::Copy("operation has no destination".into())),
    };
    let source = plan.source_abs(op);
    let parent = dest.parent().unwrap_or(&plan.dest_root).to_path_buf();
    // Long-path prefixing happens here, at the last moment, so everything
    // upstream keeps working with paths a human can read.
    let source = crate::sanitize::extended_path(&source);
    let dest = crate::sanitize::extended_path(&dest);
    let parent = crate::sanitize::extended_path(&parent);
    ensure_dir(&parent, dirs)?;

    match op.action {
        Action::Move => move_file(&source, &dest, &parent),
        Action::Copy | Action::Overwrite => copy_file(&source, &dest, &parent),
        Action::Skip => Ok(()),
    }
}

/// Parent directories are created once and remembered; at 100k files the
/// repeated `create_dir_all` syscalls are otherwise measurable. Photos arrive
/// grouped by destination, so nearly every call is a hit and takes only a
/// read lock.
fn ensure_dir(dir: &Path, cache: &RwLock<HashSet<PathBuf>>) -> Result<(), FileError> {
    if cache.read().map(|c| c.contains(dir)).unwrap_or(false) {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|e| classify(&e, dir))?;
    if let Ok(mut c) = cache.write() {
        c.insert(dir.to_path_buf());
    }
    Ok(())
}

/// Copy through a temp file in the *destination* directory so it shares the
/// filesystem and the final rename is atomic.
fn copy_file(source: &Path, dest: &Path, parent: &Path) -> Outcome {
    let tmp = temp_path(parent);
    track(&tmp);
    let result = (|| -> io::Result<()> {
        let mut reader = File::open(source)?;
        let mut writer = File::create(&tmp)?;
        io::copy(&mut reader, &mut writer)?;
        writer.sync_all()?;

        let meta = reader.metadata()?;
        if let Ok(mtime) = meta.modified() {
            let _ = writer.set_times(FileTimes::new().set_modified(mtime));
        }
        drop(writer);
        let _ = fs::set_permissions(&tmp, meta.permissions());

        rename_over(&tmp, dest)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    untrack(&tmp);
    result.map_err(|e| classify(&e, source))
}

/// Temp files that exist right now. An emergency exit (a second Ctrl-C) runs
/// no destructors, so the paths have to be reachable from the signal handler
/// for `cleanup_temps` to unlink them.
fn live_temps() -> &'static Mutex<HashSet<PathBuf>> {
    static LIVE: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

fn track(tmp: &Path) {
    if let Ok(mut set) = live_temps().lock() {
        set.insert(tmp.to_path_buf());
    }
}

fn untrack(tmp: &Path) {
    if let Ok(mut set) = live_temps().lock() {
        set.remove(tmp);
    }
}

/// Unlink every temp file still in flight. Safe to call from the Ctrl-C
/// handler; a poisoned lock is stepped over rather than panicked on.
pub fn cleanup_temps() -> usize {
    let paths: Vec<PathBuf> = match live_temps().lock() {
        Ok(mut set) => set.drain().collect(),
        Err(poisoned) => poisoned.into_inner().drain().collect(),
    };
    paths.iter().filter(|p| fs::remove_file(p).is_ok()).count()
}

/// `rename` first — instant and atomic on one filesystem. Cross-device falls
/// back to copy, then unlinks the source *last*.
fn move_file(source: &Path, dest: &Path, parent: &Path) -> Outcome {
    match rename_over(source, dest) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            copy_file(source, dest, parent)?;
            fs::remove_file(source).map_err(|e| classify(&e, source))
        }
        Err(e) => Err(classify(&e, source)),
    }
}

/// Windows `rename` refuses to clobber; the planner already decided that
/// overwriting here is correct, so remove the target and retry.
fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if cfg!(windows) && to.exists() && !is_cross_device(&e) => {
            fs::remove_file(to)?;
            fs::rename(from, to)
        }
        Err(e) => Err(e),
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    // `ErrorKind::CrossesDevices` is not stable on our MSRV.
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(18)
    }
    #[cfg(windows)]
    {
        // ERROR_NOT_SAME_DEVICE
        e.raw_os_error() == Some(17)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = e;
        false
    }
}

fn temp_path(dir: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    dir.join(format!("{TMP_PREFIX}{:x}{:x}", nanos, n))
}

fn classify(e: &io::Error, path: &Path) -> FileError {
    match e.kind() {
        io::ErrorKind::PermissionDenied => FileError::PermissionDenied,
        _ => FileError::Copy(format!("{}: {e}", path.display())),
    }
}

#[derive(Default)]
struct Counters {
    copied: AtomicU64,
    moved: AtomicU64,
    overwritten: AtomicU64,
    skipped: AtomicU64,
    failed: AtomicU64,
    bytes: AtomicU64,
    cancelled: AtomicBool,
    unattempted: AtomicU64,
}

impl Counters {
    fn finish(self) -> ExecStats {
        ExecStats {
            copied: self.copied.load(Ordering::Relaxed) as usize,
            moved: self.moved.load(Ordering::Relaxed) as usize,
            overwritten: self.overwritten.load(Ordering::Relaxed) as usize,
            skipped: self.skipped.load(Ordering::Relaxed) as usize,
            failed: self.failed.load(Ordering::Relaxed) as usize,
            bytes: self.bytes.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
            unattempted: self.unattempted.load(Ordering::Relaxed) as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Reason;

    fn op(source: &str, dest: &str, action: Action, bytes: u64) -> Operation {
        Operation {
            source_rel: PathBuf::from(source),
            dest_rel: Some(PathBuf::from(dest)),
            action,
            reason: Reason::Planned,
            bytes,
            duplicate_of: None,
        }
    }

    struct Env {
        _tmp: tempfile::TempDir,
        plan: OperationPlan,
        opts: Options,
    }

    fn env(files: &[(&str, &[u8])], ops: Vec<Operation>, mode: crate::config::Mode) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src");
        let dest = tmp.path().join("dst");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dest).unwrap();
        for (name, bytes) in files {
            let p = source.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        }
        Env {
            plan: OperationPlan {
                source_root: source.clone(),
                dest_root: dest.clone(),
                mode,
                ops: Ops::Memory(ops),
                stats: Default::default(),
            },
            opts: Options {
                source,
                dest,
                mode,
                copy_workers: 2,
                ..Default::default()
            },
            _tmp: tmp,
        }
    }

    fn go(e: &Env) -> ExecStats {
        run(&e.plan, &e.opts, &AtomicBool::new(false), |_, _| {})
    }

    /// Pass 2 must produce the same result whether the plan is resident or
    /// streamed back off disk, which is the only difference above 250k files.
    #[test]
    fn a_spilled_plan_executes_like_a_resident_one() {
        let files: Vec<(String, Vec<u8>)> = (0..12)
            .map(|i| (format!("a{i}.jpg"), format!("body {i}").into_bytes()))
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let ops: Vec<Operation> = (0..12)
            .map(|i| {
                op(
                    &format!("a{i}.jpg"),
                    &format!("out/a{i}.jpg"),
                    Action::Copy,
                    6,
                )
            })
            .collect();

        let mut e = env(&refs, ops.clone(), crate::config::Mode::Copy);
        let mut writer = crate::spill::Writer::create().expect("no temp file for the spill");
        for o in &ops {
            writer.push(o).unwrap();
        }
        e.plan.ops = Ops::Spilled(writer.finish().unwrap());

        let stats = go(&e);
        assert_eq!(stats.copied, 12);
        assert_eq!(stats.failed, 0);
        for i in 0..12 {
            let out = e.opts.dest.join(format!("out/a{i}.jpg"));
            assert_eq!(fs::read(&out).unwrap(), format!("body {i}").into_bytes());
        }
    }

    #[test]
    fn copies_creating_parent_directories() {
        let e = env(
            &[("a.jpg", b"hello")],
            vec![op("a.jpg", "2026/08-August/a.jpg", Action::Copy, 5)],
            crate::config::Mode::Copy,
        );
        let stats = go(&e);
        assert_eq!(stats.copied, 1);
        assert_eq!(stats.failed, 0);
        let out = e.opts.dest.join("2026/08-August/a.jpg");
        assert_eq!(fs::read(&out).unwrap(), b"hello");
        assert!(
            e.opts.source.join("a.jpg").exists(),
            "copy keeps the source"
        );
    }

    #[test]
    fn preserves_the_modification_time() {
        let e = env(
            &[("a.jpg", b"hello")],
            vec![op("a.jpg", "a.jpg", Action::Copy, 5)],
            crate::config::Mode::Copy,
        );
        go(&e);
        let src = fs::metadata(e.opts.source.join("a.jpg")).unwrap();
        let dst = fs::metadata(e.opts.dest.join("a.jpg")).unwrap();
        assert_eq!(src.modified().unwrap(), dst.modified().unwrap());
    }

    #[test]
    fn move_removes_the_source() {
        let e = env(
            &[("a.jpg", b"bytes")],
            vec![op("a.jpg", "y/a.jpg", Action::Move, 5)],
            crate::config::Mode::Move,
        );
        let stats = go(&e);
        assert_eq!(stats.moved, 1);
        assert!(!e.opts.source.join("a.jpg").exists());
        assert_eq!(fs::read(e.opts.dest.join("y/a.jpg")).unwrap(), b"bytes");
    }

    #[test]
    fn overwrite_replaces_the_existing_file() {
        let e = env(
            &[("a.jpg", b"new")],
            vec![op("a.jpg", "a.jpg", Action::Overwrite, 3)],
            crate::config::Mode::Copy,
        );
        fs::write(e.opts.dest.join("a.jpg"), b"old").unwrap();
        let stats = go(&e);
        assert_eq!(stats.overwritten, 1);
        assert_eq!(fs::read(e.opts.dest.join("a.jpg")).unwrap(), b"new");
    }

    #[test]
    fn dry_run_writes_nothing_but_still_reports() {
        let e = env(
            &[("a.jpg", b"hello")],
            vec![op("a.jpg", "z/a.jpg", Action::Copy, 5)],
            crate::config::Mode::Copy,
        );
        let mut opts = e.opts.clone();
        opts.dry_run = true;
        let seen = Mutex::new(Vec::new());
        let stats = run(&e.plan, &opts, &AtomicBool::new(false), |op, _| {
            seen.lock().unwrap().push(op.source_rel.clone());
        });
        assert_eq!(stats.copied, 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(!e.opts.dest.join("z/a.jpg").exists());
    }

    #[test]
    fn a_missing_source_fails_only_that_file() {
        let e = env(
            &[("a.jpg", b"ok")],
            vec![
                op("a.jpg", "a.jpg", Action::Copy, 2),
                op("gone.jpg", "gone.jpg", Action::Copy, 2),
            ],
            crate::config::Mode::Copy,
        );
        let stats = go(&e);
        assert_eq!(stats.copied, 1);
        assert_eq!(stats.failed, 1);
        assert!(e.opts.dest.join("a.jpg").exists());
    }

    #[test]
    fn a_failed_copy_leaves_no_temp_file() {
        let e = env(
            &[],
            vec![op("gone.jpg", "d/gone.jpg", Action::Copy, 2)],
            crate::config::Mode::Copy,
        );
        assert_eq!(go(&e).failed, 1);
        let leftovers: Vec<_> = fs::read_dir(e.opts.dest.join("d"))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect();
        assert!(leftovers.is_empty(), "temp file survived a failure");
    }

    #[test]
    fn tracks_temp_files_until_they_are_renamed_away() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(format!("{TMP_PREFIX}probe"));
        track(&path);
        assert!(live_temps().lock().unwrap().contains(&path));
        untrack(&path);
        assert!(!live_temps().lock().unwrap().contains(&path));
    }

    #[test]
    fn a_finished_run_leaves_nothing_for_the_sweeper() {
        let e = env(
            &[("a.jpg", b"xx")],
            vec![op("a.jpg", "d/a.jpg", Action::Copy, 2)],
            crate::config::Mode::Copy,
        );
        assert_eq!(go(&e).copied, 1);
        // A global drain would race other tests, so only this run's tree is checked.
        let live = live_temps().lock().unwrap();
        assert!(live.iter().all(|p| !p.starts_with(&e.opts.dest)));
    }

    #[test]
    fn cancellation_stops_issuing_work() {
        let e = env(
            &[("a.jpg", b"x")],
            vec![op("a.jpg", "a.jpg", Action::Copy, 1)],
            crate::config::Mode::Copy,
        );
        let stats = run(&e.plan, &e.opts, &AtomicBool::new(true), |_, _| {});
        assert!(stats.cancelled);
        assert_eq!(stats.copied, 0);
        assert!(!e.opts.dest.join("a.jpg").exists());
    }

    #[test]
    fn skips_are_reported_without_touching_disk() {
        let mut o = op("a.jpg", "a.jpg", Action::Skip, 1);
        o.reason = Reason::DuplicateOfDestination;
        let e = env(&[("a.jpg", b"x")], vec![o], crate::config::Mode::Copy);
        let stats = go(&e);
        assert_eq!(stats.skipped, 1);
        assert!(!e.opts.dest.join("a.jpg").exists());
    }
}
