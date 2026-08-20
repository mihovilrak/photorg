//! Filesystem behaviour that differs per platform. These run on
//! every native CI runner; the assertions are the same everywhere, because the
//! sanitizer and the planner deliberately apply the strictest rules on all three.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()))
}

fn place(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Same photo, different bytes — otherwise the second copy is deduplicated and
/// the collision under test never happens.
fn variant(name: &str, tag: u8) -> Vec<u8> {
    let mut bytes = fixture(name);
    bytes.push(tag);
    bytes
}

fn organize(source: &Path, dest: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec![
        source.to_str().unwrap().to_string(),
        dest.to_str().unwrap().to_string(),
        "--json".into(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::new(env!("CARGO_BIN_EXE_photorg"))
        .args(&args)
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG")
        .output()
        .expect("the binary under test failed to start")
}

fn tree(root: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(root, root, &mut out);
    out
}

struct Dirs {
    _tmp: tempfile::TempDir,
    source: PathBuf,
    dest: PathBuf,
}

fn dirs() -> Dirs {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("src");
    let dest = tmp.path().join("out");
    std::fs::create_dir_all(&source).unwrap();
    Dirs {
        _tmp: tmp,
        source,
        dest,
    }
}

/// Windows and APFS fold case, so `IMG_1.JPG` and `img_1.jpg` are one name.
/// The planner reserves names case-insensitively on every platform, so neither
/// file is ever clobbered — and the result is identical on Linux.
#[test]
fn names_differing_only_in_case_never_clobber() {
    let d = dirs();
    place(&d.source, "a/IMG_0001.JPG", &variant("dated.jpg", 1));
    place(&d.source, "b/img_0001.jpg", &variant("dated.jpg", 2));

    let out = organize(&d.source, &d.dest, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let files = tree(&d.dest);
    assert_eq!(
        files.len(),
        2,
        "a case-only collision lost a file: {files:?}"
    );
    assert!(files.contains("2021/06-June/IMG_0001.JPG"));
    assert!(files.contains("2021/06-June/img_0001_1.jpg"));
}

/// macOS hands back decomposed names. The sanitizer composes them, so the same
/// source name produces the same destination on all three platforms.
#[test]
fn decomposed_names_are_composed() {
    let d = dirs();
    // "Ce\u{301}sar" — NFD.
    place(&d.source, "Ce\u{301}sar.jpg", &fixture("dated.jpg"));

    let out = organize(&d.source, &d.dest, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let files = tree(&d.dest);
    let name = files.iter().next().expect("one file was organized");
    assert!(
        name.ends_with("C\u{e9}sar.jpg"),
        "destination kept a decomposed name: {name:?}"
    );
}

/// Well past the Windows 260-character limit; `sanitize::extended_path` is what
/// makes this work, and it must not disturb the other platforms.
#[test]
fn destinations_longer_than_260_characters_are_written() {
    let d = dirs();
    place(&d.source, "deep.jpg", &fixture("dated.jpg"));
    let long = "w".repeat(80);
    let template = format!("{{year}}/{long}/{long}/{long}");

    let out = organize(&d.source, &d.dest, &["--template", &template]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let written = d
        .dest
        .join("2021")
        .join(&long)
        .join(&long)
        .join(&long)
        .join("deep.jpg");
    assert!(
        written.to_string_lossy().len() > 260,
        "the test path is not long enough to exercise the limit"
    );
    assert_eq!(tree(&d.dest).len(), 1, "the long path was not written");
}

/// Characters that are legal on Linux and fatal on Windows are replaced on
/// every platform, so a tree copied on Linux stays portable.
#[cfg(unix)]
#[test]
fn characters_windows_rejects_are_replaced_everywhere() {
    let d = dirs();
    place(&d.source, "wh:at?*.jpg", &fixture("dated.jpg"));

    let out = organize(&d.source, &d.dest, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tree(&d.dest).contains("2021/06-June/wh-at--.jpg"));
}

/// A device name is still a device name with an extension attached.
#[cfg(unix)]
#[test]
fn windows_reserved_names_are_prefixed_everywhere() {
    let d = dirs();
    place(&d.source, "con.jpg", &fixture("dated.jpg"));

    let out = organize(&d.source, &d.dest, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tree(&d.dest).contains("2021/06-June/_con.jpg"));
}

/// Symlinks are not followed by default, so a directory pointing at its own
/// ancestor must not turn the scan into an infinite walk.
#[cfg(unix)]
#[test]
fn a_symlink_loop_in_the_source_terminates() {
    let d = dirs();
    place(&d.source, "real/a.jpg", &fixture("dated.jpg"));
    std::os::unix::fs::symlink(&d.source, d.source.join("real/loop")).unwrap();

    let out = organize(&d.source, &d.dest, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(tree(&d.dest).len(), 1, "the loop was walked into");
}

/// Linux has no birth time; a file with no EXIF and no date in its name has to
/// fall back to the modification time rather than landing in `unknown-date`.
#[test]
fn a_file_without_exif_falls_back_to_its_modification_time() {
    let d = dirs();
    place(&d.source, "plain.jpg", &fixture("no_exif.jpg"));

    // 2014-02-13T12:00:00Z, comfortably inside the sanity range.
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_392_292_800);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(d.source.join("plain.jpg"))
        .unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .unwrap();
    drop(file);

    let out = organize(&d.source, &d.dest, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let files = tree(&d.dest);
    let path = files.iter().next().expect("one file was organized");
    assert!(
        path.starts_with("2014/02-February/"),
        "modification time was ignored: {path:?}"
    );
}
