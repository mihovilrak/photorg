//! Core configuration. Deliberately free of `clap` so the library never
//! depends on the CLI layer.

use std::path::PathBuf;

use crate::error::TemplateError;
use crate::template::Template;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Granularity {
    Year,
    #[default]
    Month,
    Week,
    Day,
    Adaptive,
}

impl Granularity {
    /// The preset expressed as a template. Adaptive has no single template;
    /// the planner picks one of the fixed ones per node.
    pub fn template_str(self) -> &'static str {
        match self {
            Granularity::Year => "{year}",
            Granularity::Month | Granularity::Adaptive => "{year}/{month:02}-{month_name}",
            Granularity::Week => "{iso_year}/{iso_year}-W{iso_week:02}",
            Granularity::Day => "{year}/{month:02}/{day:02}",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocationDepth {
    Country,
    #[default]
    Region,
    City,
}

impl LocationDepth {
    pub fn template_suffix(self) -> &'static str {
        match self {
            LocationDepth::Country => "{country}",
            LocationDepth::Region => "{country}/{region}",
            LocationDepth::City => "{country}/{region}/{city}",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflict {
    Skip,
    #[default]
    Rename,
    Overwrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DedupMode {
    Off,
    #[default]
    SizeHash,
}

pub const DEFAULT_ADAPTIVE_THRESHOLD: usize = 400;
pub const DEFAULT_COPY_WORKERS: usize = 4;

#[derive(Debug, Clone)]
pub struct Options {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub group: Granularity,
    pub location: Option<LocationDepth>,
    pub template: Option<String>,
    pub mode: Mode,
    pub dry_run: bool,
    pub on_conflict: OnConflict,
    pub force: bool,
    pub dedup: DedupMode,
    pub copy_workers: usize,
    pub adaptive_threshold: usize,
    pub include_sidecars: bool,
    pub include_video: bool,
    pub filename_dates: bool,
    pub follow_symlinks: bool,
    pub resume: Option<PathBuf>,
    pub journal: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            source: PathBuf::new(),
            dest: PathBuf::new(),
            group: Granularity::default(),
            location: None,
            template: None,
            mode: Mode::default(),
            dry_run: false,
            on_conflict: OnConflict::default(),
            force: false,
            dedup: DedupMode::default(),
            copy_workers: DEFAULT_COPY_WORKERS,
            adaptive_threshold: DEFAULT_ADAPTIVE_THRESHOLD,
            include_sidecars: true,
            include_video: false,
            filename_dates: true,
            follow_symlinks: false,
            resume: None,
            journal: None,
        }
    }
}

impl Options {
    /// Expand `--group` / `--location` into the template string, unless an
    /// explicit `--template` overrides both.
    pub fn template_string(&self, group: Granularity) -> String {
        self.template_with_base(group.template_str())
    }

    /// Same expansion for an arbitrary date base, which adaptive grouping needs
    /// because it picks a different base per node.
    pub fn template_with_base(&self, base: &str) -> String {
        if let Some(t) = &self.template {
            return t.clone();
        }
        let mut s = base.to_string();
        if let Some(depth) = self.location {
            s.push('/');
            s.push_str(depth.template_suffix());
        }
        s
    }

    pub fn build_template(&self, group: Granularity) -> Result<Template, TemplateError> {
        Template::parse(&self.template_string(group))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_expand_to_templates() {
        let opts = Options {
            location: Some(LocationDepth::Region),
            ..Default::default()
        };
        assert_eq!(
            opts.template_string(Granularity::Month),
            "{year}/{month:02}-{month_name}/{country}/{region}"
        );
    }

    #[test]
    fn explicit_template_overrides_presets() {
        let opts = Options {
            template: Some("{city}/{year}".into()),
            location: Some(LocationDepth::City),
            ..Default::default()
        };
        assert_eq!(opts.template_string(Granularity::Day), "{city}/{year}");
    }

    #[test]
    fn every_preset_parses() {
        let opts = Options {
            location: Some(LocationDepth::City),
            ..Default::default()
        };
        for g in [
            Granularity::Year,
            Granularity::Month,
            Granularity::Week,
            Granularity::Day,
            Granularity::Adaptive,
        ] {
            assert!(opts.build_template(g).is_ok(), "{g:?} failed to parse");
        }
    }
}
