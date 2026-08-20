//! Pre-flight checks. Everything here runs before a single byte is
//! written, and every failure is fatal by design.

use std::path::{Path, PathBuf};

use crate::config::{Mode, OnConflict, Options};
use crate::error::FatalError;

/// Validate options and return the canonical source and destination roots.
/// Canonicalization happens **after** symlink resolution so that a symlinked
/// destination inside the source is still caught.
pub fn prepare(opts: &Options) -> Result<(PathBuf, PathBuf), FatalError> {
    if opts.on_conflict == OnConflict::Overwrite && !opts.force {
        return Err(FatalError::OverwriteWithoutForce);
    }
    if !opts.source.is_dir() {
        return Err(FatalError::SourceUnavailable(opts.source.clone()));
    }

    let source = canonical(&opts.source)?;
    // Overlap is decided before the destination is created: refusing the run
    // and still leaving a new folder inside the source tree is not acceptable.
    overlap(&source, &absolute(&opts.dest))?;

    if !opts.dry_run {
        std::fs::create_dir_all(&opts.dest).map_err(|e| FatalError::DestinationUnavailable {
            path: opts.dest.clone(),
            source: e,
        })?;
    }

    // A dry run against a not-yet-created destination is legitimate; fall back
    // to the lexically absolute path.
    let dest = match canonical(&opts.dest) {
        Ok(d) => d,
        Err(e) if opts.dry_run => {
            log::debug!("destination not canonicalizable yet: {e}");
            absolute(&opts.dest)
        }
        Err(e) => return Err(e),
    };
    // Again once both roots are real: a symlinked destination only reveals
    // itself after canonicalization.
    overlap(&source, &dest)?;

    if !opts.dry_run {
        writable(&dest)?;
    }
    Ok((source, dest))
}

fn overlap(source: &Path, dest: &Path) -> Result<(), FatalError> {
    if source == dest || dest.starts_with(source) || source.starts_with(dest) {
        return Err(FatalError::Overlap {
            source_root: source.to_path_buf(),
            dest_root: dest.to_path_buf(),
        });
    }
    Ok(())
}

/// Lexical absolutization for a path that does not exist yet. `std::path::absolute`
/// would do this, but it landed after the MSRV.
fn absolute(path: &Path) -> PathBuf {
    use std::path::Component;

    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            // `..` is resolved lexically; without an existing path there is no
            // link to follow, so this matches what the OS would do here.
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    plain(&out)
}

fn canonical(path: &Path) -> Result<PathBuf, FatalError> {
    path.canonicalize()
        .map(|p| plain(&p))
        .map_err(|e| FatalError::DestinationUnavailable {
            path: path.to_path_buf(),
            source: e,
        })
}

/// Windows `canonicalize` always returns a verbatim path. Strip the prefix so
/// every message and record stays readable; `sanitize::extended_path` puts it
/// back on the few paths that actually need it.
fn plain(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// Probe with a real file: a read-only mount, a full quota, or a Windows ACL
/// that `metadata()` reports nothing about all fail here instead of mid-run.
fn writable(dir: &Path) -> Result<(), FatalError> {
    let probe = dir.join(format!("{}probe", crate::scan::TMP_PREFIX));
    std::fs::write(&probe, b"").map_err(|e| FatalError::DestinationUnavailable {
        path: dir.to_path_buf(),
        source: e,
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Refuse to start a run that cannot possibly fit. Unknown free
/// space is not an error — an exotic filesystem should not block the tool.
pub fn check_space(dir: &Path, needed: u64, mode: Mode) -> Result<(), FatalError> {
    if mode == Mode::Move {
        // A same-filesystem move consumes no space, and a cross-device one is
        // bounded by the largest single file rather than the total.
        return Ok(());
    }
    match free_space(dir) {
        Some(available) if available < needed => Err(FatalError::DiskFull { needed, available }),
        _ => Ok(()),
    }
}

#[cfg(windows)]
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

#[cfg(unix)]
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    // `f_bavail` is what a non-root process may actually use.
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(any(unix, windows)))]
pub fn free_space(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn opts(source: PathBuf, dest: PathBuf) -> Options {
        Options {
            source,
            dest,
            ..Default::default()
        }
    }

    #[test]
    fn accepts_disjoint_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().join("s");
        fs::create_dir_all(&s).unwrap();
        let o = opts(s, tmp.path().join("d"));
        assert!(prepare(&o).is_ok());
        assert!(o.dest.is_dir(), "destination is created");
    }

    #[test]
    fn rejects_destination_inside_source() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().join("s");
        fs::create_dir_all(&s).unwrap();
        let o = opts(s.clone(), s.join("out"));
        assert!(matches!(prepare(&o), Err(FatalError::Overlap { .. })));
    }

    #[test]
    fn rejects_source_inside_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("d");
        let s = d.join("inner");
        fs::create_dir_all(&s).unwrap();
        assert!(matches!(
            prepare(&opts(s, d)),
            Err(FatalError::Overlap { .. })
        ));
    }

    #[test]
    fn rejects_identical_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().join("s");
        fs::create_dir_all(&s).unwrap();
        assert!(matches!(
            prepare(&opts(s.clone(), s)),
            Err(FatalError::Overlap { .. })
        ));
    }

    #[test]
    fn rejects_a_missing_source() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            prepare(&opts(tmp.path().join("nope"), tmp.path().join("d"))),
            Err(FatalError::SourceUnavailable(_))
        ));
    }

    #[test]
    fn overwrite_demands_force() {
        let tmp = tempfile::tempdir().unwrap();
        let s = tmp.path().join("s");
        fs::create_dir_all(&s).unwrap();
        let mut o = opts(s, tmp.path().join("d"));
        o.on_conflict = OnConflict::Overwrite;
        assert!(matches!(
            prepare(&o),
            Err(FatalError::OverwriteWithoutForce)
        ));
        o.force = true;
        assert!(prepare(&o).is_ok());
    }

    #[test]
    fn reports_free_space_for_a_real_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let free = free_space(tmp.path()).expect("a temp dir has a filesystem");
        assert!(free > 0);
        assert!(check_space(tmp.path(), 1, Mode::Copy).is_ok());
        assert!(matches!(
            check_space(tmp.path(), u64::MAX, Mode::Copy),
            Err(FatalError::DiskFull { .. })
        ));
        assert!(check_space(tmp.path(), u64::MAX, Mode::Move).is_ok());
    }
}
