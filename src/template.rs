//! Path template parsing and rendering.
//!
//! `--group` and `--location` are sugar that expand into templates, so grouping
//! and `--template` share exactly one renderer.

use std::fmt::Write as _;
use std::path::PathBuf;

use chrono::{Datelike, NaiveDate};

use crate::error::TemplateError;
use crate::sanitize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Var {
    Year,
    Month,
    MonthName,
    Day,
    IsoYear,
    IsoWeek,
    Country,
    Region,
    City,
    CameraMake,
    CameraModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Date,
    Location,
    Camera,
}

impl Var {
    fn parse(name: &str) -> Option<Var> {
        Some(match name {
            "year" => Var::Year,
            "month" => Var::Month,
            "month_name" => Var::MonthName,
            "day" => Var::Day,
            "iso_year" => Var::IsoYear,
            "iso_week" => Var::IsoWeek,
            "country" => Var::Country,
            "region" => Var::Region,
            "city" => Var::City,
            "camera_make" => Var::CameraMake,
            "camera_model" => Var::CameraModel,
            _ => return None,
        })
    }

    pub fn class(self) -> Class {
        match self {
            Var::Year | Var::Month | Var::MonthName | Var::Day | Var::IsoYear | Var::IsoWeek => {
                Class::Date
            }
            Var::Country | Var::Region | Var::City => Class::Location,
            Var::CameraMake | Var::CameraModel => Class::Camera,
        }
    }

    fn is_numeric(self) -> bool {
        matches!(
            self,
            Var::Year | Var::Month | Var::Day | Var::IsoYear | Var::IsoWeek
        )
    }
}

#[derive(Debug, Clone)]
enum Piece {
    Literal(String),
    Var { var: Var, pad: usize },
}

/// One path component's worth of template.
#[derive(Debug, Clone)]
struct Segment {
    pieces: Vec<Piece>,
}

impl Segment {
    fn vars(&self) -> impl Iterator<Item = Var> + '_ {
        self.pieces.iter().filter_map(|p| match p {
            Piece::Var { var, .. } => Some(*var),
            Piece::Literal(_) => None,
        })
    }

    fn classes(&self) -> Vec<Class> {
        let mut cs: Vec<Class> = self.vars().map(Var::class).collect();
        cs.dedup();
        cs
    }
}

#[derive(Debug, Clone)]
pub struct Template {
    segments: Vec<Segment>,
    source: String,
}

/// Everything a template can interpolate for one photo.
#[derive(Debug, Default, Clone)]
pub struct RenderVars<'a> {
    pub date: Option<NaiveDate>,
    pub location: Option<&'a Location>,
    pub camera_make: Option<&'a str>,
    pub camera_model: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Location {
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

impl Template {
    pub fn parse(input: &str) -> Result<Template, TemplateError> {
        if input.trim().is_empty() {
            return Err(TemplateError::Empty);
        }
        if input.starts_with('/') || input.starts_with('\\') {
            return Err(TemplateError::Absolute);
        }
        if input.len() >= 2 && input.as_bytes()[1] == b':' {
            return Err(TemplateError::Absolute);
        }

        let mut segments = Vec::new();
        let mut pieces: Vec<Piece> = Vec::new();
        let mut literal = String::new();
        let bytes = input.as_bytes();
        let mut i = 0;

        while i < bytes.len() {
            match bytes[i] {
                b'{' => {
                    if !literal.is_empty() {
                        pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                    }
                    let end = input[i..]
                        .find('}')
                        .map(|off| i + off)
                        .ok_or(TemplateError::UnclosedBrace(i))?;
                    let body = &input[i + 1..end];
                    let (name, pad) = match body.split_once(':') {
                        Some((name, spec)) => {
                            let width = spec
                                .strip_prefix('0')
                                .and_then(|w| w.parse::<usize>().ok())
                                .filter(|w| *w <= 9)
                                .ok_or_else(|| TemplateError::BadPadding(spec.to_string()))?;
                            (name, width)
                        }
                        None => (body, 0),
                    };
                    let var = Var::parse(name.trim())
                        .ok_or_else(|| TemplateError::UnknownVariable(name.trim().to_string()))?;
                    if pad > 0 && !var.is_numeric() {
                        return Err(TemplateError::BadPadding(format!("{name} is not numeric")));
                    }
                    pieces.push(Piece::Var { var, pad });
                    i = end + 1;
                }
                b'}' => return Err(TemplateError::StrayBrace(i)),
                b'/' | b'\\' => {
                    if !literal.is_empty() {
                        pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                    }
                    if !pieces.is_empty() {
                        segments.push(Segment {
                            pieces: std::mem::take(&mut pieces),
                        });
                    }
                    i += 1;
                }
                _ => {
                    let ch = input[i..].chars().next().expect("byte index on boundary");
                    literal.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        if !literal.is_empty() {
            pieces.push(Piece::Literal(literal));
        }
        if !pieces.is_empty() {
            segments.push(Segment { pieces });
        }
        if segments.is_empty() {
            return Err(TemplateError::Empty);
        }

        // `..` is rejected on raw literal text; a rendered value can never
        // produce it, because the sanitizer strips leading dots.
        for seg in &segments {
            let all_literal: String = seg
                .pieces
                .iter()
                .map(|p| match p {
                    Piece::Literal(s) => s.as_str(),
                    Piece::Var { .. } => "",
                })
                .collect();
            if all_literal.trim() == ".." {
                return Err(TemplateError::ParentEscape);
            }
        }

        Ok(Template {
            segments,
            source: input.to_string(),
        })
    }

    /// Does rendering this template need a reverse-geocode lookup?
    pub fn uses_location(&self) -> bool {
        self.segments
            .iter()
            .flat_map(Segment::vars)
            .any(|v| v.class() == Class::Location)
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Render to a relative directory path, sanitizing every component.
    ///
    /// A segment whose variables are all unresolved location fields is dropped;
    /// unresolved dates collapse to a single `unknown-date` component.
    pub fn render(&self, vars: &RenderVars<'_>) -> PathBuf {
        let mut out = PathBuf::new();
        let mut last_was_unknown_date = false;

        for seg in &self.segments {
            let classes = seg.classes();

            if !classes.is_empty()
                && classes.iter().all(|c| *c == Class::Location)
                && seg
                    .vars()
                    .all(|v| location_value(vars.location, v).is_none())
            {
                continue;
            }

            if classes.contains(&Class::Date) && vars.date.is_none() {
                if !last_was_unknown_date {
                    out.push(sanitize::UNKNOWN_DATE);
                    last_was_unknown_date = true;
                }
                continue;
            }
            last_was_unknown_date = false;

            let mut rendered = String::new();
            for piece in &seg.pieces {
                match piece {
                    Piece::Literal(s) => rendered.push_str(s),
                    Piece::Var { var, pad } => match value_of(*var, vars) {
                        Some(v) if *pad > 0 => {
                            let _ = write!(rendered, "{:0>width$}", v, width = *pad);
                        }
                        Some(v) => rendered.push_str(&v),
                        None => rendered.push_str(placeholder_for(*var)),
                    },
                }
            }
            out.push(sanitize::component(&rendered));
        }

        if out.as_os_str().is_empty() {
            out.push(sanitize::UNKNOWN_DATE);
        }
        out
    }
}

fn placeholder_for(var: Var) -> &'static str {
    match var.class() {
        Class::Date => sanitize::UNKNOWN_DATE,
        Class::Location => sanitize::UNKNOWN_LOCATION,
        Class::Camera => "unknown-camera",
    }
}

fn location_value(loc: Option<&Location>, var: Var) -> Option<String> {
    let loc = loc?;
    match var {
        Var::Country => loc.country.clone(),
        Var::Region => loc.region.clone(),
        Var::City => loc.city.clone(),
        _ => None,
    }
}

fn value_of(var: Var, vars: &RenderVars<'_>) -> Option<String> {
    match var {
        Var::Year => vars.date.map(|d| d.year().to_string()),
        Var::Month => vars.date.map(|d| d.month().to_string()),
        Var::MonthName => vars
            .date
            .map(|d| MONTH_NAMES[(d.month() - 1) as usize].to_string()),
        Var::Day => vars.date.map(|d| d.day().to_string()),
        // ISO year and week must come from the same call: 2026-12-31 is week 1
        // of ISO year 2027.
        Var::IsoYear => vars.date.map(|d| d.iso_week().year().to_string()),
        Var::IsoWeek => vars.date.map(|d| d.iso_week().week().to_string()),
        Var::Country | Var::Region | Var::City => location_value(vars.location, var),
        Var::CameraMake => vars.camera_make.map(str::to_string),
        Var::CameraModel => vars.camera_model.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(y, m, d)
    }

    fn render(t: &str, vars: &RenderVars<'_>) -> String {
        Template::parse(t)
            .unwrap()
            .render(vars)
            .to_string_lossy()
            .replace('\\', "/")
    }

    #[test]
    fn renders_month_preset() {
        let vars = RenderVars {
            date: date(2026, 8, 19),
            ..Default::default()
        };
        assert_eq!(
            render("{year}/{month:02}-{month_name}", &vars),
            "2026/08-August"
        );
    }

    #[test]
    fn iso_week_uses_iso_year() {
        // 2024-12-30 is a Monday and belongs to ISO week 1 of 2025.
        let vars = RenderVars {
            date: date(2024, 12, 30),
            ..Default::default()
        };
        assert_eq!(
            render("{iso_year}/{iso_year}-W{iso_week:02}", &vars),
            "2025/2025-W01"
        );
        // The calendar year must not leak into an ISO path.
        assert_eq!(render("{year}", &vars), "2024");

        // A year that starts on a Thursday has 53 ISO weeks, and its last day
        // stays in its own ISO year.
        let vars = RenderVars {
            date: date(2026, 12, 31),
            ..Default::default()
        };
        assert_eq!(render("{iso_year}-W{iso_week:02}", &vars), "2026-W53");
    }

    #[test]
    fn missing_date_collapses_to_one_component() {
        let vars = RenderVars::default();
        assert_eq!(render("{year}/{month:02}/{day:02}", &vars), "unknown-date");
    }

    #[test]
    fn missing_location_segments_are_dropped() {
        let vars = RenderVars {
            date: date(2026, 8, 19),
            ..Default::default()
        };
        assert_eq!(render("{year}/{country}/{region}", &vars), "2026");
    }

    #[test]
    fn location_components_are_sanitized() {
        let loc = Location {
            country: Some("Croatia".into()),
            region: Some("A/B: north".into()),
            city: None,
        };
        let vars = RenderVars {
            date: date(2026, 8, 19),
            location: Some(&loc),
            ..Default::default()
        };
        assert_eq!(
            render("{year}/{country}/{region}", &vars),
            "2026/Croatia/A-B- north"
        );
    }

    #[test]
    fn rejects_bad_templates() {
        assert_eq!(
            Template::parse("{year").unwrap_err(),
            TemplateError::UnclosedBrace(0)
        );
        assert!(matches!(
            Template::parse("{nope}").unwrap_err(),
            TemplateError::UnknownVariable(_)
        ));
        assert_eq!(
            Template::parse("/abs/{year}").unwrap_err(),
            TemplateError::Absolute
        );
        assert_eq!(
            Template::parse("C:/{year}").unwrap_err(),
            TemplateError::Absolute
        );
        assert_eq!(
            Template::parse("../{year}").unwrap_err(),
            TemplateError::ParentEscape
        );
        assert_eq!(Template::parse("").unwrap_err(), TemplateError::Empty);
        assert!(matches!(
            Template::parse("{country:02}").unwrap_err(),
            TemplateError::BadPadding(_)
        ));
    }

    #[test]
    fn literal_only_template_is_allowed() {
        assert_eq!(render("photos/all", &RenderVars::default()), "photos/all");
    }
}
