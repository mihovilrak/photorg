//! Adaptive granularity.
//!
//! The decision is made **per node**, recursively — a library with one huge
//! trip year and one sparse year must not get uniform granularity.
//!
//! Caveat, documented in `--help` and the README: adaptive layout is *not*
//! stable across runs. Adding photos can push a node over the threshold and
//! reshuffle it. Use a fixed `--group` for incremental workflows.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Year,
    Month,
    Week,
}

impl Level {
    /// Weeks nest inside their month so the tree stays a tree.
    pub fn template_str(self) -> &'static str {
        match self {
            Level::Year => "{year}",
            Level::Month => "{year}/{month:02}-{month_name}",
            Level::Week => "{year}/{month:02}-{month_name}/W{iso_week:02}",
        }
    }
}

/// Decide a level for every input date.
///
/// `dates[i] == None` (unknown date) never influences a threshold: those files
/// go to `unknown-date/` regardless of level.
pub fn decide(dates: &[Option<NaiveDate>], threshold: usize) -> Vec<Level> {
    let mut levels = vec![Level::Year; dates.len()];

    let mut by_year: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    for (i, d) in dates.iter().enumerate() {
        if let Some(d) = d {
            by_year.entry(d.year()).or_default().push(i);
        }
    }

    for (_, year_members) in by_year {
        // Tie-break is defined: split when strictly greater than N.
        if year_members.len() <= threshold {
            continue;
        }
        let mut by_month: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for i in year_members {
            let month = dates[i].expect("only dated members are bucketed").month();
            by_month.entry(month).or_default().push(i);
        }
        for (_, month_members) in by_month {
            let level = if month_members.len() > threshold {
                Level::Week
            } else {
                Level::Month
            };
            for i in month_members {
                levels[i] = level;
            }
        }
    }

    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(y, m, d)
    }

    #[test]
    fn sparse_year_stays_at_year_level() {
        let dates: Vec<_> = (1..=10).map(|d| day(2020, 1, d)).collect();
        assert!(decide(&dates, 400).iter().all(|l| *l == Level::Year));
    }

    #[test]
    fn busy_year_splits_into_months() {
        let mut dates = Vec::new();
        for m in 1..=12u32 {
            for _ in 0..10 {
                dates.push(day(2021, m, 1));
            }
        }
        assert!(decide(&dates, 100).iter().all(|l| *l == Level::Month));
    }

    #[test]
    fn busy_month_splits_into_weeks_independently() {
        let mut dates = Vec::new();
        // January: 30 photos -> weeks. February: 3 photos -> month.
        for _ in 0..30 {
            dates.push(day(2022, 1, 10));
        }
        for _ in 0..3 {
            dates.push(day(2022, 2, 10));
        }
        let levels = decide(&dates, 20);
        assert!(levels[..30].iter().all(|l| *l == Level::Week));
        assert!(levels[30..].iter().all(|l| *l == Level::Month));
    }

    #[test]
    fn each_year_is_decided_on_its_own() {
        let mut dates = Vec::new();
        for _ in 0..50 {
            dates.push(day(2023, 6, 1));
        }
        for _ in 0..2 {
            dates.push(day(2024, 6, 1));
        }
        let levels = decide(&dates, 10);
        assert_eq!(levels[0], Level::Week);
        assert_eq!(levels[51], Level::Year);
    }

    #[test]
    fn threshold_is_strictly_greater() {
        let dates: Vec<_> = (0..10).map(|_| day(2020, 1, 1)).collect();
        assert!(decide(&dates, 10).iter().all(|l| *l == Level::Year));
        assert!(decide(&dates, 9).iter().all(|l| *l != Level::Year));
    }

    #[test]
    fn unknown_dates_do_not_count_toward_a_split() {
        let mut dates = vec![None; 1000];
        dates.push(day(2020, 1, 1));
        let levels = decide(&dates, 10);
        assert!(levels.iter().all(|l| *l == Level::Year));
    }
}
