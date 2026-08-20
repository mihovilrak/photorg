//! Companion-file grouping.
//!
//! Live Photos, RAW+JPEG pairs and sidecars must travel together: treating
//! files in isolation orphans `.XMP` edits and separates `IMG_0001.HEIC` from
//! `IMG_0001.MOV`.

use std::collections::BTreeMap;

use crate::formats::FileKind;
use crate::metadata::PhotoMetadata;

#[derive(Debug, Clone)]
pub struct CompanionGroup {
    /// Indices into the photo slice, sorted.
    pub members: Vec<usize>,
    /// The member whose date and GPS the whole group inherits.
    pub primary: usize,
}

/// Group by `(directory, file stem)`.
///
/// Input must be sorted by relative path; output order is deterministic.
pub fn group(photos: &[PhotoMetadata]) -> Vec<CompanionGroup> {
    let mut buckets: BTreeMap<(std::path::PathBuf, String), Vec<usize>> = BTreeMap::new();
    for (i, p) in photos.iter().enumerate() {
        buckets.entry(p.companion_key()).or_default().push(i);
    }

    buckets
        .into_values()
        .map(|members| {
            let primary = pick_primary(photos, &members);
            CompanionGroup { members, primary }
        })
        .collect()
}

/// The best-dated member wins: a RAW+JPEG pair takes the date from whichever
/// file still carries real EXIF.
fn pick_primary(photos: &[PhotoMetadata], members: &[usize]) -> usize {
    *members
        .iter()
        .min_by_key(|&&i| {
            let p = &photos[i];
            (
                p.date_source,      // earlier in the chain is better
                kind_rank(p.kind),  // prefer a real image over a sidecar
                p.rel_path.clone(), // stable tie-break
            )
        })
        .expect("groups are never empty")
}

fn kind_rank(kind: FileKind) -> u8 {
    match kind {
        FileKind::Image => 0,
        FileKind::Raw => 1,
        FileKind::Video => 2,
        FileKind::Sidecar => 3,
    }
}

impl CompanionGroup {
    /// A group is worth moving only if it carries actual media. A lone `.XMP`
    /// or (without `--include-video`) a lone `.MOV` is not.
    pub fn is_shippable(&self, photos: &[PhotoMetadata], include_video: bool) -> bool {
        self.members.iter().any(|&i| match photos[i].kind {
            FileKind::Image | FileKind::Raw => true,
            FileKind::Video => include_video,
            FileKind::Sidecar => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dates::DateSource;
    use chrono::NaiveDate;
    use std::path::PathBuf;

    fn photo(path: &str, kind: FileKind, src: DateSource) -> PhotoMetadata {
        PhotoMetadata {
            rel_path: PathBuf::from(path),
            size: 1,
            kind,
            taken: NaiveDate::from_ymd_opt(2026, 8, 19).map(|d| d.and_hms_opt(0, 0, 0).unwrap()),
            date_source: src,
            gps: None,
            offset_time: None,
            camera_make: None,
            camera_model: None,
            signature_ok: true,
        }
    }

    #[test]
    fn groups_live_photo_pairs() {
        let mut photos = vec![
            photo(
                "a/IMG_0001.HEIC",
                FileKind::Image,
                DateSource::ExifDateTimeOriginal,
            ),
            photo("a/IMG_0001.MOV", FileKind::Video, DateSource::Mtime),
            photo("a/IMG_0002.JPG", FileKind::Image, DateSource::Filename),
        ];
        photos.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        let groups = group(&photos);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].members.len(), 2);
        assert_eq!(photos[groups[0].primary].kind, FileKind::Image);
    }

    #[test]
    fn raw_jpeg_pair_takes_the_best_date() {
        let photos = vec![
            photo("a/DSC01.JPG", FileKind::Image, DateSource::Mtime),
            photo(
                "a/DSC01.NEF",
                FileKind::Raw,
                DateSource::ExifDateTimeOriginal,
            ),
        ];
        let groups = group(&photos);
        assert_eq!(groups.len(), 1);
        assert_eq!(photos[groups[0].primary].kind, FileKind::Raw);
    }

    #[test]
    fn orphan_sidecars_are_not_shippable() {
        let photos = vec![photo("a/edit.xmp", FileKind::Sidecar, DateSource::Mtime)];
        let groups = group(&photos);
        assert!(!groups[0].is_shippable(&photos, true));
    }

    #[test]
    fn standalone_video_needs_the_flag() {
        let photos = vec![photo("a/clip.mp4", FileKind::Video, DateSource::Mtime)];
        let groups = group(&photos);
        assert!(!groups[0].is_shippable(&photos, false));
        assert!(groups[0].is_shippable(&photos, true));
    }
}
