//! Argument parsing. This is the only module that knows `clap`
//! exists; it produces an `Options` and gets out of the way.

use std::path::PathBuf;

use clap::{ArgAction, Parser, ValueEnum};

use photorg::config::{
    DedupMode, Granularity, LocationDepth, Mode, OnConflict, Options, DEFAULT_ADAPTIVE_THRESHOLD,
    DEFAULT_COPY_WORKERS,
};

const AFTER_HELP: &str = "\
Duplicate means identical bytes, nothing else: same size, then BLAKE3.

EXIF timestamps are naive local time and are never converted to another zone.

Adaptive grouping is not stable across runs -- adding photos can move older
ones into a different folder. Use a fixed --group for incremental imports.

Reverse geocoding is offline and resolves to the nearest populated place, so
country and region are dependable while 'city' may name a town some distance
away.

EXAMPLES:
  photorg ~/Pictures/unsorted ~/Pictures/organized
  photorg A B --group adaptive --location region
  photorg A B --mode move --dry-run
  photorg A B --json > plan.jsonl
";

/// Shown by `--version`. Binary installs (winget, a tap, a bare tarball) do not
/// necessarily carry README or THIRD-PARTY-NOTICES.md, and CC-BY requires the
/// attribution to travel with the data it covers.
#[cfg(feature = "geocoding")]
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nGeocoding data (c) GeoNames, CC BY 4.0 <https://www.geonames.org/>"
);
#[cfg(not(feature = "geocoding"))]
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "photorg",
    version = VERSION,
    about = "Organize photos into dated folders, offline and in place-safe passes.",
    after_help = AFTER_HELP
)]
pub struct Cli {
    /// Directory to read photos from.
    pub source: PathBuf,

    /// Directory to organize them into.
    pub dest: PathBuf,

    /// Folder granularity.
    #[arg(long, value_enum, default_value_t = GroupArg::Month)]
    pub group: GroupArg,

    /// Append location folders inside the date path; bare flag means `region`.
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "region")]
    pub location: Option<DepthArg>,

    /// Path template; overrides --group and --location.
    #[arg(long, value_name = "STR")]
    pub template: Option<String>,

    /// Copy (default) or move.
    #[arg(long, value_enum, default_value_t = ModeArg::Copy)]
    pub mode: ModeArg,

    /// Plan everything and write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// What to do when a different file already occupies the destination.
    #[arg(long, value_enum, default_value_t = ConflictArg::Rename)]
    pub on_conflict: ConflictArg,

    /// Required by `--on-conflict overwrite`.
    #[arg(long)]
    pub force: bool,

    /// Duplicate detection strategy.
    #[arg(long, value_enum, default_value_t = DedupArg::SizeHash)]
    pub dedup: DedupArg,

    /// Concurrent copies. Use 1 for HDDs and network shares.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_COPY_WORKERS)]
    pub workers: usize,

    /// Files per folder before adaptive grouping splits a node.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_ADAPTIVE_THRESHOLD)]
    pub adaptive_threshold: usize,

    /// Emit JSONL on stdout instead of human-readable lines.
    #[arg(long)]
    pub json: bool,

    /// Skip operations recorded in a journal from an earlier run.
    #[arg(long, value_name = "FILE")]
    pub resume: Option<PathBuf>,

    /// Append each completed operation to this journal.
    #[arg(long, value_name = "FILE")]
    pub journal: Option<PathBuf>,

    /// Carry .xmp/.aae sidecars and Live Photo movies with their stills.
    #[arg(long, default_value_t = true, action = ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    pub include_sidecars: bool,

    /// Also organize standalone videos.
    #[arg(long)]
    pub include_video: bool,

    /// Read dates out of filenames when EXIF has none.
    #[arg(long, default_value_t = true, action = ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    pub filename_dates: bool,

    /// Follow symlinks while scanning. Off by default: a loop never ends.
    #[arg(long)]
    pub follow_symlinks: bool,

    /// Only errors.
    #[arg(short, long, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Repeat for more detail.
    #[arg(short, long, action = ArgAction::Count)]
    pub verbose: u8,
}

macro_rules! value_enum {
    ($name:ident => $target:ty { $($variant:ident => $to:expr),+ $(,)? }) => {
        #[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
        #[clap(rename_all = "kebab-case")]
        pub enum $name { $($variant),+ }

        impl From<$name> for $target {
            fn from(v: $name) -> $target {
                match v { $($name::$variant => $to),+ }
            }
        }
    };
}

value_enum!(GroupArg => Granularity {
    Year => Granularity::Year,
    Month => Granularity::Month,
    Week => Granularity::Week,
    Day => Granularity::Day,
    Adaptive => Granularity::Adaptive,
});

value_enum!(DepthArg => LocationDepth {
    Country => LocationDepth::Country,
    Region => LocationDepth::Region,
    City => LocationDepth::City,
});

value_enum!(ModeArg => Mode {
    Copy => Mode::Copy,
    Move => Mode::Move,
});

value_enum!(ConflictArg => OnConflict {
    Skip => OnConflict::Skip,
    Rename => OnConflict::Rename,
    Overwrite => OnConflict::Overwrite,
});

value_enum!(DedupArg => DedupMode {
    Off => DedupMode::Off,
    SizeHash => DedupMode::SizeHash,
});

impl Cli {
    pub fn into_options(self) -> Options {
        // Resuming without naming a journal keeps writing to the same file, so
        // a run interrupted twice still makes progress.
        let journal = self.journal.or_else(|| self.resume.clone());
        Options {
            source: self.source,
            dest: self.dest,
            group: self.group.into(),
            location: self.location.map(Into::into),
            template: self.template,
            mode: self.mode.into(),
            dry_run: self.dry_run,
            on_conflict: self.on_conflict.into(),
            force: self.force,
            dedup: self.dedup.into(),
            copy_workers: self.workers.max(1),
            adaptive_threshold: self.adaptive_threshold.max(1),
            include_sidecars: self.include_sidecars,
            include_video: self.include_video,
            filename_dates: self.filename_dates,
            follow_symlinks: self.follow_symlinks,
            resume: self.resume,
            journal,
        }
    }

    pub fn log_level(&self) -> log::LevelFilter {
        match (self.quiet, self.verbose) {
            (true, _) => log::LevelFilter::Error,
            (_, 0) => log::LevelFilter::Warn,
            (_, 1) => log::LevelFilter::Info,
            (_, 2) => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn help_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_match_the_documented_surface() {
        let opts = Cli::parse_from(["photorg", "a", "b"]).into_options();
        assert_eq!(opts.group, Granularity::Month);
        assert_eq!(opts.mode, Mode::Copy);
        assert_eq!(opts.on_conflict, OnConflict::Rename);
        assert_eq!(opts.dedup, DedupMode::SizeHash);
        assert_eq!(opts.copy_workers, DEFAULT_COPY_WORKERS);
        assert!(opts.location.is_none());
        assert!(opts.include_sidecars);
        assert!(opts.filename_dates);
        assert!(!opts.include_video);
        assert!(!opts.follow_symlinks);
    }

    #[test]
    fn bare_location_means_region() {
        let opts = Cli::parse_from(["photorg", "a", "b", "--location"]).into_options();
        assert_eq!(opts.location, Some(LocationDepth::Region));
        let opts = Cli::parse_from(["photorg", "a", "b", "--location", "city"]).into_options();
        assert_eq!(opts.location, Some(LocationDepth::City));
    }

    #[test]
    fn defaulted_booleans_can_be_switched_off() {
        let opts =
            Cli::parse_from(["photorg", "a", "b", "--filename-dates", "false"]).into_options();
        assert!(!opts.filename_dates);
    }

    #[test]
    fn resume_doubles_as_the_journal_path() {
        let opts = Cli::parse_from(["photorg", "a", "b", "--resume", "j.jsonl"]).into_options();
        assert_eq!(opts.journal, opts.resume);
    }

    #[test]
    fn workers_never_reach_zero() {
        let opts = Cli::parse_from(["photorg", "a", "b", "--workers", "0"]).into_options();
        assert_eq!(opts.copy_workers, 1);
    }
}
