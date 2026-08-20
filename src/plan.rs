//! The plan phase: serial, deterministic, and the only place where
//! non-determinism — collision suffixes, ordering, duplicate resolution — is
//! allowed to be resolved. Pass 2 executes what this produced and decides
//! nothing on its own.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::adaptive::{self, Level};
use crate::companion;
use crate::config::{DedupMode, Granularity, Mode, OnConflict, Options};
use crate::dedup::{Digest, HashCache, SizeIndex};
use crate::error::FatalError;
use crate::geocode::Locator;
use crate::metadata::PhotoMetadata;
use crate::sanitize;
use crate::spill::{self, Spill};
use crate::template::{Location, RenderVars, Template};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Copy,
    Move,
    Skip,
    Overwrite,
}

impl Action {
    /// Does this operation touch the filesystem in pass 2?
    pub fn is_pending(self) -> bool {
        !matches!(self, Action::Skip)
    }
}

/// Why an operation looks the way it does. Duplicates are a distinct outcome,
/// never a generic skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    Planned,
    /// Identical bytes already at the destination.
    DuplicateOfDestination,
    /// Identical bytes to an earlier file in this same source set.
    DuplicateOfSource,
    /// A different file already occupies the destination.
    Renamed,
    ExistingFile,
    /// Recorded in the `--resume` journal.
    AlreadyDone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    /// Relative to the source root, which the plan stores once.
    pub source_rel: PathBuf,
    /// Relative to the destination root. `None` only for skips with no target.
    pub dest_rel: Option<PathBuf>,
    pub action: Action,
    pub reason: Reason,
    pub bytes: u64,
    /// The file this one duplicates, for duplicate reasons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PlanStats {
    pub groups: usize,
    pub pending: usize,
    pub duplicates: usize,
    pub skipped_existing: usize,
    pub renamed: usize,
    pub resumed: usize,
    /// Sidecars with no media and, without `--include-video`, standalone video.
    pub unshippable: usize,
    pub pending_bytes: u64,
}

/// Where the built operations live. Small plans stay resident; large ones go
/// to a temp file so peak RSS does not track the file count.
#[derive(Debug)]
pub enum Ops {
    Memory(Vec<Operation>),
    Spilled(Spill),
}

impl Ops {
    pub fn len(&self) -> usize {
        match self {
            Ops::Memory(ops) => ops.len(),
            Ops::Spilled(spill) => spill.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The operations as a slice, or `None` once the plan has been spilled —
    /// which only happens above `spill::THRESHOLD` files, where the caller has
    /// to stream the plan back instead of borrowing it.
    pub fn resident(&self) -> Option<&[Operation]> {
        match self {
            Ops::Memory(ops) => Some(ops),
            Ops::Spilled(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct OperationPlan {
    pub source_root: PathBuf,
    pub dest_root: PathBuf,
    pub mode: Mode,
    pub ops: Ops,
    pub stats: PlanStats,
}

impl OperationPlan {
    /// Operations that will touch the filesystem, or `None` for a spilled plan.
    pub fn pending(&self) -> Option<impl Iterator<Item = &Operation>> {
        Some(
            self.ops
                .resident()?
                .iter()
                .filter(|o| o.action.is_pending()),
        )
    }

    pub fn source_abs(&self, op: &Operation) -> PathBuf {
        self.source_root.join(&op.source_rel)
    }

    pub fn dest_abs(&self, op: &Operation) -> Option<PathBuf> {
        op.dest_rel.as_ref().map(|d| self.dest_root.join(d))
    }
}

/// Build the plan. `done` maps source paths already executed to where they
/// landed, so `--resume` neither re-copies nor re-reserves them incorrectly.
pub fn build(
    photos: &[PhotoMetadata],
    opts: &Options,
    mut locator: Option<&mut Locator>,
    done: &HashMap<PathBuf, PathBuf>,
) -> Result<OperationPlan, FatalError> {
    let groups = companion::group(photos);
    let templates = Templates::build(opts)?;

    let levels = adaptive_levels(photos, &groups, opts);

    let dest_index = match opts.dedup {
        DedupMode::SizeHash => SizeIndex::build(&opts.dest),
        DedupMode::Off => SizeIndex::default(),
    };
    let mut hashes = HashCache::default();
    // Lowercased destination strings: a case-insensitive filesystem must not
    // be handed two names that differ only in case.
    let mut reserved: HashSet<String> = HashSet::new();
    let mut twins = Twins::default();

    let mut stats = PlanStats {
        groups: groups.len(),
        ..Default::default()
    };

    // Phase 1: resolve each group's destination directory. Every member takes
    // the primary's date and place, which is what keeps a Live Photo pair
    // together. Directories are interned — even a million files land
    // in a few thousand of them — so the whole grouping collapses to one u32
    // per photo and the groups themselves can be dropped before planning.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut dir_ids: HashMap<PathBuf, u32> = HashMap::new();
    let mut dir_of: Vec<u32> = vec![UNSHIPPABLE; photos.len()];

    for (gi, group) in groups.iter().enumerate() {
        if !group.is_shippable(photos, opts.include_video) {
            stats.unshippable += group.members.len();
            continue;
        }
        let primary = &photos[group.primary];
        let location = resolve_location(primary, &templates, locator.as_deref_mut());
        let dir = templates
            .for_level(levels.get(&gi).copied())
            .render(&RenderVars {
                date: primary.date(),
                location: location.as_ref(),
                camera_make: primary.camera_make.as_deref(),
                camera_model: primary.camera_model.as_deref(),
            });
        let id = *dir_ids.entry(dir.clone()).or_insert_with(|| {
            dirs.push(dir);
            (dirs.len() - 1) as u32
        });
        for &mi in &group.members {
            dir_of[mi] = id;
        }
    }
    drop((groups, levels, dir_ids));

    // Phase 2: plan in source order, which is the order the records already
    // arrive in. Planning in that order is what lets the operations
    // be written straight out — a plan on disk cannot be sorted afterwards.
    let mut sink = Sink::new(photos.len());
    for (i, photo) in photos.iter().enumerate() {
        let id = dir_of[i];
        if id == UNSHIPPABLE {
            continue;
        }
        let op = plan_one(
            photo,
            &dirs[id as usize],
            opts,
            done,
            &dest_index,
            &mut hashes,
            &mut reserved,
            &mut twins,
        );
        match op.reason {
            Reason::DuplicateOfDestination | Reason::DuplicateOfSource => stats.duplicates += 1,
            Reason::ExistingFile => stats.skipped_existing += 1,
            Reason::Renamed => stats.renamed += 1,
            Reason::AlreadyDone => stats.resumed += 1,
            Reason::Planned => {}
        }
        if op.action.is_pending() {
            stats.pending += 1;
            stats.pending_bytes += op.bytes;
        }
        sink.push(op)?;
    }

    Ok(OperationPlan {
        source_root: opts.source.clone(),
        dest_root: opts.dest.clone(),
        mode: opts.mode,
        ops: sink.finish()?,
        stats,
    })
}

/// A photo whose group produced no destination, so it is never planned.
const UNSHIPPABLE: u32 = u32::MAX;

/// What dedup has to remember about an already-planned file. It cannot be an
/// index into the operations: by now they may be on disk.
struct Twin {
    source_rel: PathBuf,
    dest_rel: Option<PathBuf>,
}

/// Source-side duplicate memory.
///
/// A file is parked under its size until a second file of that size turns up;
/// only then is anything hashed, so the usual case of a unique size costs no
/// I/O. Once a size repeats, its twins move into the digest index and every
/// later comparison is a hash lookup rather than a scan of the whole bucket —
/// without that, a folder of same-sized files is quadratic in *file opens*.
#[derive(Default)]
struct Twins {
    unhashed: HashMap<u64, Vec<Twin>>,
    by_digest: HashMap<Digest, Vec<Twin>>,
}

impl Twins {
    fn remember(&mut self, size: u64, twin: Twin) {
        self.unhashed.entry(size).or_default().push(twin);
    }

    /// The first remembered twin holding identical bytes.
    ///
    /// "First" is insertion order: `unhashed` drains in order and digest lists
    /// only ever grow at the end, so which twin wins does not depend on when
    /// the bucket happened to be indexed.
    fn find(
        &mut self,
        hashes: &mut HashCache,
        source_root: &Path,
        abs: &Path,
        size: u64,
    ) -> Option<&Twin> {
        // An absent entry means this size has never been seen. An empty one
        // means it has, and its twins are already indexed.
        let pending = self.unhashed.get_mut(&size)?;
        let digest = hashes.partial(abs, size).ok()?;
        for twin in std::mem::take(pending) {
            match hashes.partial(&source_root.join(&twin.source_rel), size) {
                Ok(d) => self.by_digest.entry(d).or_default().push(twin),
                // Unreadable now and unreadable later: it can never be shown to
                // be a duplicate, so stop carrying it.
                Err(e) => log::debug!("dedup hash failed for {}: {e}", twin.source_rel.display()),
            }
        }
        // Everything in this bucket already agrees on the edge hash, so only
        // the full comparison is left.
        self.by_digest.get(&digest)?.iter().find(|t| {
            let other = source_root.join(&t.source_rel);
            settle(hashes.same_bytes_after_partial(abs, &other, size), abs)
        })
    }
}

/// Collects operations either in memory or straight into a spill file.
enum Sink {
    Memory(Vec<Operation>),
    Disk(spill::Writer),
}

impl Sink {
    fn new(photos: usize) -> Sink {
        if photos >= spill::THRESHOLD {
            match spill::Writer::create() {
                Some(writer) => {
                    log::info!("spilling the plan to {}", writer.path().display());
                    return Sink::Disk(writer);
                }
                None => log::warn!("no temp file available; keeping the plan in memory"),
            }
        }
        Sink::Memory(Vec::with_capacity(photos))
    }

    fn push(&mut self, op: Operation) -> Result<(), FatalError> {
        match self {
            Sink::Memory(ops) => ops.push(op),
            Sink::Disk(writer) => writer.push(&op)?,
        }
        Ok(())
    }

    fn finish(self) -> Result<Ops, FatalError> {
        Ok(match self {
            Sink::Memory(ops) => Ops::Memory(ops),
            Sink::Disk(writer) => Ops::Spilled(writer.finish()?),
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_one(
    photo: &PhotoMetadata,
    dir: &Path,
    opts: &Options,
    done: &HashMap<PathBuf, PathBuf>,
    dest_index: &SizeIndex,
    hashes: &mut HashCache,
    reserved: &mut HashSet<String>,
    twins: &mut Twins,
) -> Operation {
    let source_abs = opts.source.join(&photo.rel_path);

    if let Some(dest) = done.get(&photo.rel_path) {
        reserved.insert(key(dest));
        // A resumed file still counts as content already shipped, so later
        // twins of it are duplicates rather than a second copy under a new name.
        if opts.dedup == DedupMode::SizeHash {
            twins.remember(
                photo.size,
                Twin {
                    source_rel: photo.rel_path.clone(),
                    dest_rel: Some(dest.clone()),
                },
            );
        }
        return Operation {
            source_rel: photo.rel_path.clone(),
            dest_rel: Some(dest.clone()),
            action: Action::Skip,
            reason: Reason::AlreadyDone,
            bytes: photo.size,
            duplicate_of: None,
        };
    }

    if opts.dedup == DedupMode::SizeHash {
        // Source set first: it needs no disk index and catches re-imported
        // cards, where the same bytes arrive under two names.
        if let Some(other) = twins.find(hashes, &opts.source, &source_abs, photo.size) {
            return Operation {
                source_rel: photo.rel_path.clone(),
                dest_rel: other.dest_rel.clone(),
                action: Action::Skip,
                reason: Reason::DuplicateOfSource,
                bytes: photo.size,
                duplicate_of: Some(other.source_rel.clone()),
            };
        }
        for candidate in dest_index.candidates(photo.size) {
            if same_bytes(hashes, &source_abs, candidate, photo.size) {
                let rel = candidate
                    .strip_prefix(&opts.dest)
                    .unwrap_or(candidate)
                    .to_path_buf();
                return Operation {
                    source_rel: photo.rel_path.clone(),
                    dest_rel: Some(rel.clone()),
                    action: Action::Skip,
                    reason: Reason::DuplicateOfDestination,
                    bytes: photo.size,
                    duplicate_of: Some(rel),
                };
            }
        }
    }

    let name = sanitize::file_name(photo.file_name());
    let wanted = dir.join(&name);
    let (dest_rel, action, reason) = resolve_conflict(wanted, opts, reserved);

    if let Some(d) = &dest_rel {
        reserved.insert(key(d));
    }
    if action.is_pending() && opts.dedup == DedupMode::SizeHash {
        twins.remember(
            photo.size,
            Twin {
                source_rel: photo.rel_path.clone(),
                dest_rel: dest_rel.clone(),
            },
        );
    }

    Operation {
        source_rel: photo.rel_path.clone(),
        dest_rel,
        action,
        reason,
        bytes: photo.size,
        duplicate_of: None,
    }
}

fn same_bytes(hashes: &mut HashCache, a: &Path, b: &Path, size: u64) -> bool {
    settle(hashes.same_bytes(a, b, size), a)
}

/// An unreadable candidate is not a duplicate; the copy will report the real
/// error if the source itself is the broken one.
fn settle(result: std::io::Result<bool>, a: &Path) -> bool {
    result.unwrap_or_else(|e| {
        log::debug!("dedup comparison failed for {}: {e}", a.display());
        false
    })
}

/// Assign a free destination. Two sources colliding *within one run* always
/// rename — `--on-conflict overwrite` is about pre-existing files, not about
/// silently dropping half of this run's input.
fn resolve_conflict(
    wanted: PathBuf,
    opts: &Options,
    reserved: &HashSet<String>,
) -> (Option<PathBuf>, Action, Reason) {
    let copy_or_move = match opts.mode {
        Mode::Copy => Action::Copy,
        Mode::Move => Action::Move,
    };

    let in_run = reserved.contains(&key(&wanted));
    let on_disk = opts.dest.join(&wanted).exists();

    if !in_run && !on_disk {
        return (Some(wanted), copy_or_move, Reason::Planned);
    }
    if !in_run && on_disk {
        match opts.on_conflict {
            OnConflict::Skip => {
                return (Some(wanted), Action::Skip, Reason::ExistingFile);
            }
            OnConflict::Overwrite => {
                return (Some(wanted), Action::Overwrite, Reason::ExistingFile);
            }
            OnConflict::Rename => {}
        }
    }

    let dir = wanted.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = wanted
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(sanitize::PLACEHOLDER);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, Some(e)),
        _ => (name, None),
    };

    for n in 1u32.. {
        let candidate = match ext {
            Some(ext) => dir.join(format!("{stem}_{n}.{ext}")),
            None => dir.join(format!("{stem}_{n}")),
        };
        if !reserved.contains(&key(&candidate)) && !opts.dest.join(&candidate).exists() {
            return (Some(candidate), copy_or_move, Reason::Renamed);
        }
    }
    unreachable!("the suffix search is unbounded")
}

/// Case- and separator-insensitive form of a destination path, for the
/// reservation set. One pass into one allocation: this runs once per planned
/// file, and `to_string_lossy().replace().to_lowercase()` costs three.
fn key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\\' => out.push('/'),
            c if c.is_ascii_uppercase() => out.push(c.to_ascii_lowercase()),
            c if c.is_uppercase() => out.extend(c.to_lowercase()),
            c => out.push(c),
        }
    }
    out
}

fn resolve_location(
    photo: &PhotoMetadata,
    templates: &Templates,
    locator: Option<&mut Locator>,
) -> Option<Location> {
    if !templates.uses_location {
        return None;
    }
    let (lat, lon) = photo.gps?;
    locator?.lookup(lat, lon)
}

/// One template for fixed grouping, one per level for adaptive.
struct Templates {
    fixed: Option<Template>,
    year: Option<Template>,
    month: Option<Template>,
    week: Option<Template>,
    uses_location: bool,
}

impl Templates {
    fn build(opts: &Options) -> Result<Templates, FatalError> {
        if opts.group == Granularity::Adaptive && opts.template.is_none() {
            let year = Template::parse(&opts.template_with_base(Level::Year.template_str()))?;
            let uses_location = year.uses_location();
            return Ok(Templates {
                fixed: None,
                year: Some(year),
                month: Some(Template::parse(
                    &opts.template_with_base(Level::Month.template_str()),
                )?),
                week: Some(Template::parse(
                    &opts.template_with_base(Level::Week.template_str()),
                )?),
                uses_location,
            });
        }
        let fixed = opts.build_template(opts.group)?;
        let uses_location = fixed.uses_location();
        Ok(Templates {
            fixed: Some(fixed),
            year: None,
            month: None,
            week: None,
            uses_location,
        })
    }

    fn for_level(&self, level: Option<Level>) -> &Template {
        match (level, &self.fixed) {
            (_, Some(fixed)) => fixed,
            (Some(Level::Year), _) => self.year.as_ref().expect("adaptive templates built"),
            (Some(Level::Month), _) => self.month.as_ref().expect("adaptive templates built"),
            (Some(Level::Week), _) => self.week.as_ref().expect("adaptive templates built"),
            (None, _) => self.year.as_ref().expect("adaptive templates built"),
        }
    }
}

/// Adaptive counts *files*, not groups: a year of Live Photo pairs is twice as
/// crowded as the group count suggests.
fn adaptive_levels(
    photos: &[PhotoMetadata],
    groups: &[companion::CompanionGroup],
    opts: &Options,
) -> HashMap<usize, Level> {
    if opts.group != Granularity::Adaptive || opts.template.is_some() {
        return HashMap::new();
    }
    let mut dates = Vec::with_capacity(photos.len());
    let mut owner = Vec::with_capacity(photos.len());
    for (gi, g) in groups.iter().enumerate() {
        let date = photos[g.primary].date();
        for _ in &g.members {
            dates.push(date);
            owner.push(gi);
        }
    }
    let levels = adaptive::decide(&dates, opts.adaptive_threshold);
    let mut out = HashMap::new();
    for (i, level) in levels.into_iter().enumerate() {
        out.insert(owner[i], level);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dates::DateSource;
    use crate::formats::FileKind;
    use chrono::NaiveDate;
    use std::fs;

    fn photo(path: &str, size: u64, y: i32, m: u32, d: u32) -> PhotoMetadata {
        PhotoMetadata {
            rel_path: PathBuf::from(path),
            size,
            kind: FileKind::Image,
            taken: NaiveDate::from_ymd_opt(y, m, d).map(|d| d.and_hms_opt(12, 0, 0).unwrap()),
            date_source: DateSource::ExifDateTimeOriginal,
            gps: None,
            offset_time: None,
            camera_make: None,
            camera_model: None,
            signature_ok: true,
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        opts: Options,
    }

    fn fixture(files: &[(&str, &[u8])]) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src");
        let dest = tmp.path().join("dst");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dest).unwrap();
        for (name, bytes) in files {
            let p = source.join(name);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, bytes).unwrap();
        }
        Fixture {
            _tmp: tmp,
            opts: Options {
                source,
                dest,
                ..Default::default()
            },
        }
    }

    fn rel(op: &Operation) -> String {
        op.dest_rel
            .as_ref()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default()
    }

    #[test]
    fn renders_the_month_preset() {
        let f = fixture(&[("a.jpg", b"a")]);
        let photos = vec![photo("a.jpg", 1, 2026, 8, 19)];
        let plan = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        assert_eq!(
            rel(&plan.ops.resident().unwrap()[0]),
            "2026/08-August/a.jpg"
        );
        assert_eq!(plan.ops.resident().unwrap()[0].action, Action::Copy);
    }

    #[test]
    fn two_sources_with_one_destination_are_renamed_not_lost() {
        let f = fixture(&[("one/a.jpg", b"first"), ("two/a.jpg", b"second")]);
        let photos = vec![
            photo("one/a.jpg", 5, 2026, 8, 19),
            photo("two/a.jpg", 6, 2026, 8, 19),
        ];
        let plan = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        let dests: Vec<_> = plan.ops.resident().unwrap().iter().map(rel).collect();
        assert_eq!(dests[0], "2026/08-August/a.jpg");
        assert_eq!(dests[1], "2026/08-August/a_1.jpg");
        assert_eq!(plan.stats.renamed, 1);
    }

    #[test]
    fn identical_bytes_under_different_names_are_one_copy() {
        let f = fixture(&[("a.jpg", b"same"), ("a (1).jpg", b"same")]);
        let photos = vec![
            photo("a (1).jpg", 4, 2026, 8, 19),
            photo("a.jpg", 4, 2026, 8, 19),
        ];
        let plan = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        assert_eq!(plan.stats.pending, 1);
        assert_eq!(plan.stats.duplicates, 1);
        let dup = plan
            .ops
            .resident()
            .unwrap()
            .iter()
            .find(|o| o.reason == Reason::DuplicateOfSource)
            .unwrap();
        assert!(dup.duplicate_of.is_some());
    }

    #[test]
    fn a_rerun_finds_its_own_output_instead_of_renaming() {
        let f = fixture(&[("a.jpg", b"payload")]);
        let photos = vec![photo("a.jpg", 7, 2026, 8, 19)];
        let first = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        // Simulate the executed copy.
        let dest = f
            .opts
            .dest
            .join(first.ops.resident().unwrap()[0].dest_rel.as_ref().unwrap());
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"payload").unwrap();

        let second = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        assert_eq!(
            second.ops.resident().unwrap()[0].reason,
            Reason::DuplicateOfDestination
        );
        assert_eq!(second.stats.pending, 0);
    }

    #[test]
    fn different_bytes_at_the_destination_get_a_suffix() {
        let f = fixture(&[("a.jpg", b"new")]);
        let photos = vec![photo("a.jpg", 3, 2026, 8, 19)];
        let dest = f.opts.dest.join("2026/08-August");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("a.jpg"), b"old").unwrap();

        let plan = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        assert_eq!(
            rel(&plan.ops.resident().unwrap()[0]),
            "2026/08-August/a_1.jpg"
        );
    }

    #[test]
    fn on_conflict_skip_leaves_the_existing_file_alone() {
        let f = fixture(&[("a.jpg", b"new")]);
        let mut opts = f.opts.clone();
        opts.on_conflict = OnConflict::Skip;
        let photos = vec![photo("a.jpg", 3, 2026, 8, 19)];
        let dest = opts.dest.join("2026/08-August");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("a.jpg"), b"old").unwrap();

        let plan = build(&photos, &opts, None, &HashMap::new()).unwrap();
        assert_eq!(plan.ops.resident().unwrap()[0].action, Action::Skip);
        assert_eq!(plan.ops.resident().unwrap()[0].reason, Reason::ExistingFile);
    }

    #[test]
    fn companions_land_in_one_folder() {
        let f = fixture(&[("IMG_1.jpg", b"j"), ("IMG_1.xmp", b"x")]);
        let mut photos = vec![
            photo("IMG_1.jpg", 1, 2026, 8, 19),
            photo("IMG_1.xmp", 1, 2021, 1, 1),
        ];
        photos[1].kind = FileKind::Sidecar;
        photos[1].date_source = DateSource::Mtime;
        let plan = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        assert_eq!(
            rel(&plan.ops.resident().unwrap()[0]),
            "2026/08-August/IMG_1.jpg"
        );
        assert_eq!(
            rel(&plan.ops.resident().unwrap()[1]),
            "2026/08-August/IMG_1.xmp"
        );
    }

    #[test]
    fn orphan_sidecars_are_not_planned() {
        let f = fixture(&[("edit.xmp", b"x")]);
        let mut p = photo("edit.xmp", 1, 2026, 8, 19);
        p.kind = FileKind::Sidecar;
        let plan = build(&[p], &f.opts, None, &HashMap::new()).unwrap();
        assert!(plan.ops.is_empty());
        assert_eq!(plan.stats.unshippable, 1);
    }

    #[test]
    fn unknown_dates_go_to_their_own_folder() {
        let f = fixture(&[("a.jpg", b"a")]);
        let mut p = photo("a.jpg", 1, 2026, 8, 19);
        p.taken = None;
        p.date_source = DateSource::Unknown;
        let plan = build(&[p], &f.opts, None, &HashMap::new()).unwrap();
        assert_eq!(rel(&plan.ops.resident().unwrap()[0]), "unknown-date/a.jpg");
    }

    #[test]
    fn resume_skips_recorded_operations() {
        let f = fixture(&[("a.jpg", b"a")]);
        let mut done = HashMap::new();
        done.insert(
            PathBuf::from("a.jpg"),
            PathBuf::from("2026/08-August/a.jpg"),
        );
        let plan = build(&[photo("a.jpg", 1, 2026, 8, 19)], &f.opts, None, &done).unwrap();
        assert_eq!(plan.ops.resident().unwrap()[0].reason, Reason::AlreadyDone);
        assert_eq!(plan.stats.pending, 0);
    }

    #[test]
    fn applying_a_plan_twice_is_a_no_op() {
        let f = fixture(&[("a.jpg", b"aaa"), ("b/a.jpg", b"bbb")]);
        let photos = vec![
            photo("a.jpg", 3, 2026, 8, 19),
            photo("b/a.jpg", 3, 2026, 8, 19),
        ];
        let first = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        for op in first.pending().unwrap() {
            let dest = f.opts.dest.join(op.dest_rel.as_ref().unwrap());
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::copy(f.opts.source.join(&op.source_rel), dest).unwrap();
        }
        let second = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        assert_eq!(second.stats.pending, 0);
        assert_eq!(second.stats.duplicates, 2);
    }

    #[test]
    fn every_source_appears_exactly_once_with_a_unique_destination() {
        let files: Vec<(String, Vec<u8>)> = (0..25)
            .map(|i| {
                (
                    format!("d{}/img{i}.jpg", i % 5),
                    format!("body-{i}").into_bytes(),
                )
            })
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let f = fixture(&refs);
        let photos: Vec<_> = files
            .iter()
            .map(|(n, b)| photo(n, b.len() as u64, 2026, 8, 19))
            .collect();
        let plan = build(&photos, &f.opts, None, &HashMap::new()).unwrap();
        assert_eq!(plan.ops.len(), photos.len());
        let mut dests: Vec<_> = plan.pending().unwrap().map(rel).collect();
        let total = dests.len();
        dests.sort();
        dests.dedup();
        assert_eq!(dests.len(), total, "destinations must be unique");
    }
}
