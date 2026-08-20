//! End-to-end tests driving the real binary over synthetic trees.
//! Everything here asserts on the two contracts a user actually depends on:
//! the tree that ends up on disk, and the JSONL on stdout.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture(name: &str) -> Vec<u8> {
    let path = fixtures().join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("missing fixture {}: {e}", path.display()))
}

/// Writes `rel` under `root`, creating parents. Content comes from a committed
/// fixture unless `bytes` overrides it.
fn place(root: &Path, rel: &str, fixture_name: &str) {
    place_bytes(root, rel, &fixture(fixture_name));
}

fn place_bytes(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

struct Run {
    output: Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn code(&self) -> i32 {
        self.output
            .status
            .code()
            .expect("the process was not signalled")
    }

    /// Every JSONL line except the trailing summary.
    fn records(&self) -> Vec<Value> {
        self.lines()
            .into_iter()
            .filter(|v| v.get("type").is_none())
            .collect()
    }

    fn summary(&self) -> Value {
        self.lines()
            .into_iter()
            .find(|v| v["type"] == "summary")
            .expect("every JSONL run ends with a summary line")
    }

    fn lines(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSONL {l:?}: {e}")))
            .collect()
    }

    fn destinations(&self) -> Vec<String> {
        self.records()
            .iter()
            .filter_map(|r| r["destination"].as_str().map(str::to_owned))
            .collect()
    }
}

fn run(args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_photorg"))
        .args(args)
        // Progress and color are TTY-gated anyway; pin them off so a test host
        // with an unusual environment cannot change what is asserted.
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG")
        .output()
        .expect("the binary under test failed to start");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        output,
    }
}

fn organize(source: &Path, dest: &Path, extra: &[&str]) -> Run {
    let mut args = vec![source.to_str().unwrap(), dest.to_str().unwrap(), "--json"];
    args.extend_from_slice(extra);
    run(&args)
}

/// Relative paths of every file below `root`, slash-normalized and sorted.
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
                let rel = path.strip_prefix(root).unwrap().to_string_lossy();
                out.insert(rel.replace('\\', "/"));
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(root, root, &mut out);
    out
}

/// A tree covering the cases the planner branches on: two dated photos in
/// different years, a byte-identical duplicate, a photo with a sidecar, and a
/// photo with no usable EXIF.
fn sample_tree(root: &Path) {
    place(root, "cam/dated.jpg", "dated.jpg");
    place(root, "cam/dated_copy.jpg", "dated.jpg");
    place(root, "cam/week.jpg", "iso_week_boundary.jpg");
    place(root, "raw/gps_zagreb.jpg", "gps_zagreb.jpg");
    place_bytes(root, "raw/gps_zagreb.xmp", b"<x:xmpmeta/>");
    place(root, "misc/no_exif.jpg", "no_exif.jpg");
}

struct Case {
    _tmp: tempfile::TempDir,
    source: PathBuf,
    dest: PathBuf,
}

fn case() -> Case {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("src");
    let dest = tmp.path().join("out");
    std::fs::create_dir_all(&source).unwrap();
    sample_tree(&source);
    Case {
        _tmp: tmp,
        source,
        dest,
    }
}

#[test]
fn copies_into_year_and_month_folders() {
    let c = case();
    let r = organize(&c.source, &c.dest, &[]);

    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);
    assert_eq!(
        tree(&c.dest),
        BTreeSet::from_iter([
            "2021/06-June/dated.jpg".into(),
            "2026/05-May/gps_zagreb.jpg".into(),
            "2026/05-May/gps_zagreb.xmp".into(),
            "2026/12-December/week.jpg".into(),
            // No EXIF and no filename date: the file falls back to its mtime,
            // which the harness just created, so only its presence is fixed.
            no_exif_destination(&c.dest),
        ]),
    );
    // The byte-identical copy is dropped, and its sidecar travels with the
    // photo it belongs to rather than being planned on its own.
    let s = r.summary();
    assert_eq!(s["duplicates"], 1);
    assert_eq!(s["failed"], 0);
    assert_eq!(s["cancelled"], false);
    assert!(
        c.source.join("cam/dated.jpg").exists(),
        "copy keeps the source"
    );
}

/// The mtime-derived folder for `misc/no_exif.jpg`, discovered rather than
/// predicted: asserting the current month would break at every month boundary.
fn no_exif_destination(dest: &Path) -> String {
    tree(dest)
        .into_iter()
        .find(|p| p.ends_with("no_exif.jpg"))
        .expect("the EXIF-less photo was still organized")
}

#[test]
fn dry_run_touches_nothing() {
    let c = case();
    let r = organize(&c.source, &c.dest, &["--dry-run"]);

    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);
    assert!(r.summary()["dry_run"].as_bool().unwrap());
    assert!(tree(&c.dest).is_empty(), "a dry run created files");
    assert_eq!(r.records().len(), 6, "every scanned file is still reported");
    // Plans are built serially, so a dry run is deterministic and sortable.
    let mut sorted = r.destinations();
    sorted.sort();
    let mut as_emitted = r.destinations();
    as_emitted.sort();
    assert_eq!(sorted, as_emitted);
}

#[test]
fn move_mode_empties_the_source() {
    let c = case();
    let r = organize(&c.source, &c.dest, &["--mode", "move"]);

    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);
    assert!(
        !c.source.join("cam/dated.jpg").exists(),
        "the original stayed put"
    );
    // A duplicate is never moved: dropping it would destroy the only copy the
    // user has of a file the tool decided not to organize.
    assert!(c.source.join("cam/dated_copy.jpg").exists());
    assert_eq!(r.summary()["moved"], 5);
}

#[test]
fn adaptive_grouping_collapses_thin_years() {
    let c = case();
    let r = organize(&c.source, &c.dest, &["--group", "adaptive"]);

    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);
    // Under the default threshold every year here is thin, so months vanish.
    assert!(
        r.destinations().iter().all(|d| d.matches('/').count() == 1),
        "adaptive kept a month level: {:?}",
        r.destinations()
    );
}

/// The dataset only exists with the feature on; without it the location
/// variables render empty and the test has nothing to assert.
#[cfg(feature = "geocoding")]
#[test]
fn location_grouping_uses_gps_and_applies_hemisphere_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("src");
    let dest = tmp.path().join("out");
    place(&source, "zagreb.jpg", "gps_zagreb.jpg");
    place(&source, "south_west.jpg", "gps_south_west.jpg");
    place(&source, "null_island.jpg", "gps_null_island.jpg");

    let r = organize(&source, &dest, &["--location", "city", "--dry-run"]);
    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);

    let by_name = |name: &str| {
        r.records()
            .into_iter()
            .find(|rec| rec["source"] == name)
            .unwrap_or_else(|| panic!("no record for {name}"))["destination"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    assert!(
        by_name("zagreb.jpg").contains("Croatia"),
        "{}",
        by_name("zagreb.jpg")
    );
    // 33.92 S, 18.42 W is the South Atlantic. Ignoring the refs would place it
    // in South Africa instead, so this pins the sign handling.
    let sw = by_name("south_west.jpg");
    assert!(
        sw.contains("Saint Helena") || sw.contains("Tristan"),
        "hemisphere refs were dropped: {sw}"
    );
    // (0, 0) is the default a broken writer emits, not a place anyone shot at.
    assert_eq!(by_name("null_island.jpg"), "2026/05-May/null_island.jpg");
}

#[test]
fn jsonl_records_carry_the_documented_fields() {
    let c = case();
    let r = organize(&c.source, &c.dest, &["--dry-run"]);

    let rec = r
        .records()
        .into_iter()
        .find(|rec| rec["source"] == "cam/dated.jpg")
        .expect("the dated photo is reported");
    assert_eq!(rec["destination"], "2021/06-June/dated.jpg");
    assert_eq!(rec["action"], "copy");
    assert_eq!(rec["reason"], "planned");
    assert!(rec["bytes"].as_u64().unwrap() > 0);
    assert!(
        rec.get("error").is_none(),
        "a clean record carries no error"
    );

    let dup = r
        .records()
        .into_iter()
        .find(|rec| rec["source"] == "cam/dated_copy.jpg")
        .unwrap();
    assert_eq!(dup["action"], "skip");
    assert_eq!(dup["duplicate_of"], "cam/dated.jpg");

    let s = r.summary();
    for key in [
        "source_root",
        "dest_root",
        "scanned",
        "planned",
        "copied",
        "moved",
        "overwritten",
        "skipped",
        "duplicates",
        "renamed",
        "resumed",
        "unshippable",
        "failed",
        "bytes",
        "cancelled",
    ] {
        assert!(s.get(key).is_some(), "summary is missing {key}");
    }
    assert!(
        !s["source_root"].as_str().unwrap().contains('\\'),
        "paths are slash-normalized even on Windows"
    );
}

#[test]
fn resume_skips_what_the_journal_already_recorded() {
    let c = case();
    let journal = c._tmp.path().join("run.jsonl");
    let j = journal.to_str().unwrap();

    let first = organize(&c.source, &c.dest, &["--journal", j]);
    assert_eq!(first.code(), 0, "stderr: {}", first.stderr);
    let done = first.summary()["copied"].as_u64().unwrap();
    assert!(done > 0);

    // The destination is wiped so only the journal can prevent the re-copy.
    std::fs::remove_dir_all(&c.dest).unwrap();
    let second = organize(&c.source, &c.dest, &["--resume", j]);
    assert_eq!(second.code(), 0, "stderr: {}", second.stderr);
    let s = second.summary();
    assert_eq!(s["planned"], 0, "resume re-planned finished work");
    assert_eq!(s["resumed"].as_u64().unwrap(), done);
    assert!(tree(&c.dest).is_empty());
}

#[test]
fn resume_without_a_journal_flag_keeps_writing_to_the_same_file() {
    let c = case();
    let journal = c._tmp.path().join("run.jsonl");
    let j = journal.to_str().unwrap();

    organize(&c.source, &c.dest, &["--journal", j, "--dry-run"]);
    assert!(!journal.exists(), "a dry run wrote a journal");

    organize(&c.source, &c.dest, &["--resume", j]);
    assert!(journal.exists(), "--resume did not double as --journal");
    let entries = std::fs::read_to_string(&journal).unwrap().lines().count();
    assert!(entries > 0);
}

#[test]
fn conflict_modes_decide_what_happens_to_an_occupied_name() {
    let existing = |dest: &Path| {
        place_bytes(dest, "2021/06-June/dated.jpg", b"not the same bytes at all");
    };

    // rename: the incoming file gets a numeric suffix, the squatter survives.
    let c = case();
    existing(&c.dest);
    let r = organize(&c.source, &c.dest, &["--on-conflict", "rename"]);
    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);
    assert!(c.dest.join("2021/06-June/dated_1.jpg").exists());
    assert_eq!(
        std::fs::read(c.dest.join("2021/06-June/dated.jpg")).unwrap(),
        b"not the same bytes at all"
    );
    assert_eq!(r.summary()["renamed"], 1);

    // skip: nothing is written and the file is reported, not silently dropped.
    let c = case();
    existing(&c.dest);
    let r = organize(&c.source, &c.dest, &["--on-conflict", "skip"]);
    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);
    assert!(!c.dest.join("2021/06-June/dated_1.jpg").exists());
    assert_eq!(
        std::fs::read(c.dest.join("2021/06-June/dated.jpg")).unwrap(),
        b"not the same bytes at all"
    );

    // overwrite: destructive, so it is refused without --force.
    let c = case();
    existing(&c.dest);
    let refused = organize(&c.source, &c.dest, &["--on-conflict", "overwrite"]);
    assert_eq!(refused.code(), 2, "overwrite ran without --force");
    assert!(
        refused.stderr.contains("force"),
        "stderr: {}",
        refused.stderr
    );

    let r = organize(
        &c.source,
        &c.dest,
        &["--on-conflict", "overwrite", "--force"],
    );
    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);
    assert_eq!(
        std::fs::read(c.dest.join("2021/06-June/dated.jpg")).unwrap(),
        fixture("dated.jpg")
    );
    assert_eq!(r.summary()["overwritten"], 1);
}

#[test]
fn identical_bytes_already_in_the_destination_are_not_recopied() {
    let c = case();
    assert_eq!(organize(&c.source, &c.dest, &[]).code(), 0);
    let before = tree(&c.dest);

    let again = organize(&c.source, &c.dest, &[]);
    assert_eq!(again.code(), 0, "stderr: {}", again.stderr);
    assert_eq!(tree(&c.dest), before, "a second run duplicated the tree");
    assert_eq!(again.summary()["copied"], 0);
}

#[test]
fn overlapping_roots_are_refused_before_anything_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("src");
    sample_tree(&source);

    let inside = source.join("out");
    let r = organize(&source, &inside, &[]);
    assert_eq!(r.code(), 2);
    assert!(
        !inside.exists(),
        "the refused run still created the destination"
    );

    let r = organize(&source, &source, &[]);
    assert_eq!(r.code(), 2);
}

#[test]
fn a_missing_source_is_fatal_not_an_empty_run() {
    let tmp = tempfile::tempdir().unwrap();
    let r = organize(&tmp.path().join("nope"), &tmp.path().join("out"), &[]);
    assert_eq!(r.code(), 2);
    assert!(r.stdout.trim().is_empty(), "a fatal run emitted records");
}

#[test]
fn sidecars_can_be_left_behind() {
    let c = case();
    let r = organize(&c.source, &c.dest, &["--include-sidecars", "false"]);
    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);
    assert!(
        !tree(&c.dest).iter().any(|p| p.ends_with(".xmp")),
        "the sidecar was copied despite --include-sidecars false"
    );
}

#[test]
fn quiet_keeps_stdout_intact_and_stderr_empty() {
    let c = case();
    let r = organize(&c.source, &c.dest, &["--dry-run", "--quiet"]);
    assert_eq!(r.code(), 0);
    assert!(!r.summary()["scanned"].is_null());
    assert!(r.stderr.trim().is_empty(), "stderr: {}", r.stderr);
}

#[test]
fn a_custom_template_controls_the_whole_layout() {
    let c = case();
    let r = organize(
        &c.source,
        &c.dest,
        &["--template", "{year}/{year}-{month:02}", "--dry-run"],
    );
    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);
    // `{month}` is unpadded on its own; `:02` is what zero-pads it.
    assert!(r
        .destinations()
        .contains(&"2021/2021-06/dated.jpg".to_string()));
}

/// EXIF in HEIC/AVIF lives in ISOBMFF `meta` boxes, and crate support
/// for that path varies, so these fixtures carry the same `meta` -> `iinf` /
/// `iloc` -> `mdat` chain a phone writes. A container whose EXIF is not
/// reachable must still land somewhere, via the mtime, rather than fail.
#[test]
fn isobmff_containers_are_dated_from_their_meta_boxes() {
    let tmp = tempfile::tempdir().unwrap();
    let (source, dest) = (tmp.path().join("src"), tmp.path().join("out"));
    place(&source, "a.heic", "heic_dated.heic");
    place(&source, "b.avif", "avif_dated.avif");
    place(&source, "c.heic", "heic_minimal.heic");

    let r = organize(&source, &dest, &[]);
    assert_eq!(r.code(), 0, "stderr: {}", r.stderr);

    let found = tree(&dest);
    assert!(
        found.contains("2020/07-July/a.heic"),
        "HEIC EXIF was not read: {found:?}"
    );
    assert!(
        found.contains("2022/09-September/b.avif"),
        "AVIF EXIF was not read: {found:?}"
    );
    assert!(
        found.iter().any(|p| p.ends_with("/c.heic")),
        "the unreachable-EXIF container went missing: {found:?}"
    );
    assert!(
        !found.contains("2018/11-November/c.heic"),
        "a container without a meta box must not yield an EXIF date: {found:?}"
    );
}
