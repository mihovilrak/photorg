//! One-open metadata extraction: signature check, EXIF and GPS come from the
//! same read.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};
use exif::{In, Rational, Tag, Value};

use crate::dates::{self, DateSource};
use crate::error::FileError;
use crate::formats::{self, FileKind, SIGNATURE_BYTES};

/// Everything pass 1 keeps about a file.
///
/// Field types are chosen for footprint: adaptive grouping forces every record
/// to be resident at once, so memory is the binding constraint.
#[derive(Debug, Clone)]
pub struct PhotoMetadata {
    /// Relative to the source root, which is stored once by the scanner.
    pub rel_path: PathBuf,
    pub size: u64,
    pub kind: FileKind,
    pub taken: Option<NaiveDateTime>,
    pub date_source: DateSource,
    pub gps: Option<(f32, f32)>,
    /// Parsed and recorded, but never applied to the bucketing date.
    pub offset_time: Option<Box<str>>,
    pub camera_make: Option<Box<str>>,
    pub camera_model: Option<Box<str>>,
    pub signature_ok: bool,
}

impl PhotoMetadata {
    pub fn date(&self) -> Option<NaiveDate> {
        self.taken.map(|d| d.date())
    }

    pub fn file_name(&self) -> &str {
        self.rel_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unnamed")
    }

    /// `(directory, lowercased stem)` — the companion grouping key.
    pub fn companion_key(&self) -> (PathBuf, String) {
        let dir = self
            .rel_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let stem = self
            .rel_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        (dir, stem)
    }
}

#[derive(Default)]
struct ExifFacts {
    original: Option<NaiveDateTime>,
    create: Option<NaiveDateTime>,
    modify: Option<NaiveDateTime>,
    gps: Option<(f32, f32)>,
    offset_time: Option<Box<str>>,
    make: Option<Box<str>>,
    model: Option<Box<str>>,
}

/// Read one file and produce its metadata. Never fails on bad EXIF — corrupt
/// metadata falls through the chain.
pub fn extract(
    abs_path: &Path,
    rel_path: PathBuf,
    kind: FileKind,
    filename_dates: bool,
) -> Result<PhotoMetadata, FileError> {
    let file = File::open(abs_path).map_err(io_to_file_error)?;
    let fs_meta = file.metadata().map_err(io_to_file_error)?;
    let size = fs_meta.len();
    let mut reader = BufReader::new(file);

    let mut head = [0u8; SIGNATURE_BYTES];
    let read = read_head(&mut reader, &mut head);
    let signature_ok = formats::signature_matches(kind, &head[..read]);

    let facts = if signature_ok && matches!(kind, FileKind::Image | FileKind::Raw) {
        reader.seek(SeekFrom::Start(0)).map_err(io_to_file_error)?;
        read_exif(&mut reader)
    } else {
        ExifFacts::default()
    };

    let (taken, date_source) = resolve_date(&facts, &rel_path, &fs_meta, filename_dates);

    Ok(PhotoMetadata {
        rel_path,
        size,
        kind,
        taken,
        date_source,
        gps: facts.gps,
        offset_time: facts.offset_time,
        camera_make: facts.make,
        camera_model: facts.model,
        signature_ok,
    })
}

fn read_head<R: Read>(reader: &mut R, buf: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    filled
}

fn io_to_file_error(e: std::io::Error) -> FileError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => FileError::PermissionDenied,
        _ => FileError::Unreadable(e.to_string()),
    }
}

/// Date fallback chain. EXIF ModifyDate sits below the filesystem
/// times: it is extracted, but only used when nothing else resolved.
fn resolve_date(
    facts: &ExifFacts,
    rel_path: &Path,
    fs_meta: &std::fs::Metadata,
    filename_dates: bool,
) -> (Option<NaiveDateTime>, DateSource) {
    if let Some(d) = facts.original {
        return (Some(d), DateSource::ExifDateTimeOriginal);
    }
    if let Some(d) = facts.create {
        return (Some(d), DateSource::ExifCreateDate);
    }
    if filename_dates {
        if let Some(name) = rel_path.file_name().and_then(|n| n.to_str()) {
            if let Some(d) = dates::parse_filename_date(name) {
                return (Some(d), DateSource::Filename);
            }
        }
    }
    if let Some(d) = fs_meta
        .modified()
        .ok()
        .and_then(dates::system_time_to_local)
    {
        return (Some(d), DateSource::Mtime);
    }
    if let Some(d) = fs_meta.created().ok().and_then(dates::system_time_to_local) {
        return (Some(d), DateSource::CreationTime);
    }
    if let Some(d) = facts.modify {
        return (Some(d), DateSource::ExifModifyDate);
    }
    (None, DateSource::Unknown)
}

fn read_exif<R: BufRead + Seek>(reader: &mut R) -> ExifFacts {
    let mut facts = ExifFacts::default();
    let mut exif_reader = exif::Reader::new();
    // Truncated EXIF is common; take whatever parsed rather than dropping it.
    exif_reader.continue_on_error(true);

    let exif = match exif_reader.read_from_container(reader) {
        Ok(e) => e,
        Err(exif::Error::PartialResult(partial)) => {
            let (e, _errors) = partial.into_inner();
            e
        }
        Err(_) => return facts,
    };

    facts.original =
        ascii_field(&exif, Tag::DateTimeOriginal).and_then(|s| dates::parse_exif_datetime(&s));
    facts.create =
        ascii_field(&exif, Tag::DateTimeDigitized).and_then(|s| dates::parse_exif_datetime(&s));
    facts.modify = ascii_field(&exif, Tag::DateTime).and_then(|s| dates::parse_exif_datetime(&s));
    facts.offset_time = ascii_field(&exif, Tag::OffsetTimeOriginal).map(String::into_boxed_str);
    facts.make = ascii_field(&exif, Tag::Make).map(String::into_boxed_str);
    facts.model = ascii_field(&exif, Tag::Model).map(String::into_boxed_str);
    facts.gps = read_gps(&exif);
    facts
}

fn ascii_field(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    let s = match &field.value {
        Value::Ascii(parts) => parts
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect::<Vec<_>>()
            .join(" "),
        other => other.display_as(tag).to_string(),
    };
    let s = s.trim().trim_matches('\0').trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Convert DMS rationals to signed decimal degrees.
///
/// Dropping the ref would put Zagreb in the southern hemisphere.
fn read_gps(exif: &exif::Exif) -> Option<(f32, f32)> {
    let lat = dms_to_degrees(rationals(exif, Tag::GPSLatitude)?)?;
    let lon = dms_to_degrees(rationals(exif, Tag::GPSLongitude)?)?;

    let lat_ref = ascii_field(exif, Tag::GPSLatitudeRef).unwrap_or_default();
    let lon_ref = ascii_field(exif, Tag::GPSLongitudeRef).unwrap_or_default();
    let lat = if lat_ref.eq_ignore_ascii_case("S") {
        -lat
    } else {
        lat
    };
    let lon = if lon_ref.eq_ignore_ascii_case("W") {
        -lon
    } else {
        lon
    };

    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    // Exactly (0, 0) is the null sentinel cameras write, not Null Island.
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    Some((lat as f32, lon as f32))
}

fn rationals(exif: &exif::Exif, tag: Tag) -> Option<Vec<Rational>> {
    match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Rational(v) if v.len() >= 3 => Some(v[..3].to_vec()),
        _ => None,
    }
}

fn dms_to_degrees(parts: Vec<Rational>) -> Option<f64> {
    let mut acc = 0.0;
    for (i, r) in parts.iter().enumerate() {
        if r.denom == 0 {
            // A zero denominator is corruption, not a zero value.
            return None;
        }
        acc += r.to_f64() / 60f64.powi(i as i32);
    }
    acc.is_finite().then_some(acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rat(num: u32, denom: u32) -> Rational {
        Rational { num, denom }
    }

    #[test]
    fn converts_dms_to_decimal() {
        let d = dms_to_degrees(vec![rat(45, 1), rat(48, 1), rat(0, 1)]).unwrap();
        assert!((d - 45.8).abs() < 1e-9);
    }

    #[test]
    fn rejects_zero_denominator() {
        assert_eq!(
            dms_to_degrees(vec![rat(45, 1), rat(48, 0), rat(0, 1)]),
            None
        );
    }

    #[test]
    fn companion_key_is_case_insensitive_on_the_stem() {
        let mk = |p: &str| PhotoMetadata {
            rel_path: PathBuf::from(p),
            size: 0,
            kind: FileKind::Image,
            taken: None,
            date_source: DateSource::Unknown,
            gps: None,
            offset_time: None,
            camera_make: None,
            camera_model: None,
            signature_ok: true,
        };
        assert_eq!(
            mk("a/IMG_0001.HEIC").companion_key(),
            mk("a/img_0001.mov").companion_key()
        );
        assert_ne!(
            mk("a/IMG_0001.HEIC").companion_key(),
            mk("b/IMG_0001.mov").companion_key()
        );
    }
}
