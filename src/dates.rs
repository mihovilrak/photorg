//! Date resolution chain.
//!
//! Timezone policy: EXIF timestamps are naive local wall-clock and are **never**
//! converted. Filesystem timestamps are absolute UTC and are converted to local
//! time before bucketing. `OffsetTimeOriginal` is recorded but never shifts the
//! bucketing date. Without this rule, two copies of one photo land in different
//! months depending on which source won.

use std::time::SystemTime;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};

pub const MIN_YEAR: i32 = 1990;

/// Which link of the fallback chain produced the date.
///
/// Declaration order is precedence order, and `Ord` is derived from it:
/// `companion::pick_primary` sorts by it, so it must stay in step with
/// `metadata::resolve_date`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DateSource {
    ExifDateTimeOriginal,
    ExifCreateDate,
    Filename,
    Mtime,
    CreationTime,
    ExifModifyDate,
    Unknown,
}

impl DateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DateSource::ExifDateTimeOriginal => "exif-datetime-original",
            DateSource::ExifCreateDate => "exif-create-date",
            DateSource::Filename => "filename",
            DateSource::Mtime => "mtime",
            DateSource::CreationTime => "creation-time",
            DateSource::ExifModifyDate => "exif-modify-date",
            DateSource::Unknown => "unknown",
        }
    }
}

/// Reject dates outside 1990..=now+1; a 1904 mtime or a 2099 filename is noise.
pub fn in_sane_range(dt: &NaiveDateTime) -> bool {
    let year = dt.year();
    year >= MIN_YEAR && year <= Local::now().year() + 1
}

/// Parse an EXIF `YYYY:MM:DD HH:MM:SS` string.
///
/// `0000:00:00 00:00:00` is very common and means "absent", not "year zero".
pub fn parse_exif_datetime(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    if s.starts_with("0000") || s.is_empty() {
        return None;
    }
    let (date_part, time_part) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (d, t),
        None => (s, "00:00:00"),
    };
    let nums: Vec<u32> = date_part
        .split([':', '-', '/'])
        .map(|p| p.trim().parse::<u32>().ok())
        .collect::<Option<Vec<_>>>()?;
    if nums.len() != 3 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(nums[0] as i32, nums[1], nums[2])?;

    let time = parse_time(time_part).unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    let dt = date.and_time(time);
    in_sane_range(&dt).then_some(dt)
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    let digits: Vec<u32> = s
        .split([':', '.', '-'])
        .take(3)
        .map(|p| p.trim().parse::<u32>().ok())
        .collect::<Option<Vec<_>>>()?;
    match digits.len() {
        3 => NaiveTime::from_hms_opt(digits[0], digits[1], digits[2]),
        2 => NaiveTime::from_hms_opt(digits[0], digits[1], 0),
        _ => None,
    }
}

/// Extract a date embedded in a file name.
///
/// Filename patterns recover far more files than filesystem times: mtime is
/// destroyed by copying, syncing and editing, and Linux birthtime is often
/// unavailable. Recognizes `IMG_20230815_123456`, `20230815_123456`,
/// `PXL_20220103_180000000`, `IMG-20200101-WA0001`,
/// `Screenshot 2024-01-05 at 10.11.12`.
pub fn parse_filename_date(name: &str) -> Option<NaiveDateTime> {
    let b = name.as_bytes();
    let mut i = 0;
    while i + 8 <= b.len() {
        // Never start mid-digit-run: that is how `180000000` would be misread.
        if i > 0 && b[i - 1].is_ascii_digit() {
            i += 1;
            continue;
        }
        if let Some((dt, end)) = try_date_at(b, i) {
            if in_sane_range(&dt) {
                return Some(dt);
            }
            i = end;
            continue;
        }
        i += 1;
    }
    None
}

fn is_sep(c: u8) -> bool {
    matches!(c, b'-' | b'_' | b'.' | b' ')
}

fn digits_at(b: &[u8], i: usize, n: usize) -> Option<u32> {
    if i + n > b.len() || !b[i..i + n].iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(&b[i..i + n]).ok()?.parse().ok()
}

fn try_date_at(b: &[u8], start: usize) -> Option<(NaiveDateTime, usize)> {
    let year = digits_at(b, start, 4)?;
    let mut i = start + 4;

    let separated = i < b.len() && is_sep(b[i]);
    let sep = if separated {
        let s = b[i];
        i += 1;
        Some(s)
    } else {
        None
    };
    let month = digits_at(b, i, 2)?;
    i += 2;
    if let Some(s) = sep {
        if i >= b.len() || b[i] != s {
            return None;
        }
        i += 1;
    }
    let day = digits_at(b, i, 2)?;
    i += 2;

    let date = NaiveDate::from_ymd_opt(year as i32, month, day)?;

    // A compact date must not be a slice out of a longer digit run, unless what
    // follows is exactly a HHMMSS(mmm) timestamp.
    if sep.is_none() && i < b.len() && b[i].is_ascii_digit() {
        let run = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if run != 6 && run != 9 {
            return None;
        }
        let time = digits_at(b, i, 2)
            .zip(digits_at(b, i + 2, 2))
            .zip(digits_at(b, i + 4, 2))
            .and_then(|((h, m), s)| NaiveTime::from_hms_opt(h, m, s))
            .unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        return Some((date.and_time(time), i + run));
    }

    let (time, end) = parse_trailing_time(b, i);
    Some((date.and_time(time), end))
}

/// Best-effort time after a date: `_123456`, ` at 10.11.12`, `-104512`.
fn parse_trailing_time(b: &[u8], mut i: usize) -> (NaiveTime, usize) {
    let midnight = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
    let start = i;
    while i < b.len() && (is_sep(b[i]) || b[i] == b'T' || b[i] == b'a' || b[i] == b't') {
        i += 1;
    }
    if i == start && i < b.len() {
        return (midnight, start);
    }
    let Some(h) = digits_at(b, i, 2) else {
        return (midnight, start);
    };
    let mut j = i + 2;
    if j < b.len() && is_sep(b[j]) {
        j += 1;
    }
    let Some(m) = digits_at(b, j, 2) else {
        return (midnight, start);
    };
    j += 2;
    if j < b.len() && is_sep(b[j]) {
        j += 1;
    }
    let s = digits_at(b, j, 2).unwrap_or(0);
    match NaiveTime::from_hms_opt(h, m, s) {
        Some(t) => (t, j + 2),
        None => (midnight, start),
    }
}

/// Convert an absolute filesystem timestamp to naive **local** wall-clock.
pub fn system_time_to_local(st: SystemTime) -> Option<NaiveDateTime> {
    let dt: DateTime<Local> = st.into();
    let naive = dt.naive_local();
    in_sane_range(&naive).then_some(naive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, s)
            .unwrap()
    }

    #[test]
    fn parses_exif_datetimes() {
        assert_eq!(
            parse_exif_datetime("2023:08:15 12:34:56"),
            Some(dt(2023, 8, 15, 12, 34, 56))
        );
        assert_eq!(parse_exif_datetime("0000:00:00 00:00:00"), None);
        assert_eq!(parse_exif_datetime(""), None);
        assert_eq!(parse_exif_datetime("garbage"), None);
        assert_eq!(parse_exif_datetime("1899:01:01 00:00:00"), None);
    }

    #[test]
    fn parses_filename_patterns() {
        let cases = [
            ("IMG_20230815_123456.jpg", dt(2023, 8, 15, 12, 34, 56)),
            ("20230815_123456.jpg", dt(2023, 8, 15, 12, 34, 56)),
            ("PXL_20220103_180000000.jpg", dt(2022, 1, 3, 18, 0, 0)),
            ("IMG-20200101-WA0001.jpg", dt(2020, 1, 1, 0, 0, 0)),
            (
                "Screenshot 2024-01-05 at 10.11.12.png",
                dt(2024, 1, 5, 10, 11, 12),
            ),
            ("VID_20211231.mp4", dt(2021, 12, 31, 0, 0, 0)),
        ];
        for (name, want) in cases {
            assert_eq!(parse_filename_date(name), Some(want), "{name}");
        }
    }

    #[test]
    fn rejects_non_dates_and_out_of_range() {
        assert_eq!(parse_filename_date("IMG_1234.jpg"), None);
        assert_eq!(parse_filename_date("DSC00001.jpg"), None);
        assert_eq!(parse_filename_date("19000101_000000.jpg"), None);
        assert_eq!(parse_filename_date("20231345_000000.jpg"), None);
        // A 17-digit run is an id, not a timestamp.
        assert_eq!(parse_filename_date("12345678901234567.jpg"), None);
    }

    #[test]
    fn sanity_range_rejects_the_far_future() {
        let far = dt(Local::now().year() + 5, 1, 1, 0, 0, 0);
        assert!(!in_sane_range(&far));
    }
}
