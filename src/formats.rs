//! Extension filtering and signature validation.
//!
//! Detection order is cheap-first: extension filter, then a signature check
//! performed on bytes already read for EXIF. At 1M files, sniffing every file
//! separately would cost 1M extra opens.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileKind {
    Image,
    Raw,
    Video,
    Sidecar,
}

const IMAGE_EXTS: [&str; 9] = [
    "jpg", "jpeg", "png", "tif", "tiff", "webp", "heic", "heif", "avif",
];
/// TIFF-derived RAW formats. CR3 is ISOBMFF and deferred.
const RAW_EXTS: [&str; 6] = ["nef", "arw", "dng", "orf", "raf", "cr2"];
const VIDEO_EXTS: [&str; 7] = ["mov", "mp4", "m4v", "avi", "mts", "3gp", "mpg"];
const SIDECAR_EXTS: [&str; 2] = ["xmp", "aae"];

/// Classify by extension alone. `None` means "not our business".
pub fn classify(extension: &str) -> Option<FileKind> {
    let ext = extension.to_ascii_lowercase();
    let ext = ext.as_str();
    if IMAGE_EXTS.contains(&ext) {
        Some(FileKind::Image)
    } else if RAW_EXTS.contains(&ext) {
        Some(FileKind::Raw)
    } else if VIDEO_EXTS.contains(&ext) {
        Some(FileKind::Video)
    } else if SIDECAR_EXTS.contains(&ext) {
        Some(FileKind::Sidecar)
    } else {
        None
    }
}

/// Does `head` look like a container we understand?
///
/// Sidecars are plain text/XML and are not signature-checked. A `false` here
/// downgrades the file rather than failing the run.
pub fn signature_matches(kind: FileKind, head: &[u8]) -> bool {
    if kind == FileKind::Sidecar {
        return true;
    }
    if head.len() < 12 {
        return false;
    }
    let is_isobmff = &head[4..8] == b"ftyp";
    let is_riff = &head[0..4] == b"RIFF";

    match kind {
        FileKind::Image | FileKind::Raw => {
            head.starts_with(&[0xFF, 0xD8, 0xFF])                    // JPEG
                || head.starts_with(b"\x89PNG\r\n\x1a\n")            // PNG
                || head.starts_with(b"II\x2a\x00")                   // TIFF LE (+ most RAW)
                || head.starts_with(b"MM\x00\x2a")                   // TIFF BE
                || head.starts_with(b"II\x2b\x00")                   // BigTIFF LE
                || head.starts_with(b"MM\x00\x2b")                   // BigTIFF BE
                || (is_riff && &head[8..12] == b"WEBP")              // WebP
                || is_isobmff                                        // HEIC/HEIF/AVIF
                || head.starts_with(b"FUJIFILMCCD") // RAF
        }
        FileKind::Video => is_isobmff || is_riff || head.starts_with(&[0x00, 0x00, 0x01]),
        FileKind::Sidecar => true,
    }
}

/// Bytes to read from the head of a file for the signature check.
pub const SIGNATURE_BYTES: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension_case_insensitively() {
        assert_eq!(classify("JPG"), Some(FileKind::Image));
        assert_eq!(classify("heic"), Some(FileKind::Image));
        assert_eq!(classify("NEF"), Some(FileKind::Raw));
        assert_eq!(classify("mov"), Some(FileKind::Video));
        assert_eq!(classify("xmp"), Some(FileKind::Sidecar));
        assert_eq!(classify("txt"), None);
    }

    #[test]
    fn recognizes_signatures() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(signature_matches(FileKind::Image, &jpeg));

        let mut heic = [0u8; 16];
        heic[4..8].copy_from_slice(b"ftyp");
        heic[8..12].copy_from_slice(b"heic");
        assert!(signature_matches(FileKind::Image, &heic));

        let mut webp = [0u8; 16];
        webp[0..4].copy_from_slice(b"RIFF");
        webp[8..12].copy_from_slice(b"WEBP");
        assert!(signature_matches(FileKind::Image, &webp));

        assert!(signature_matches(
            FileKind::Raw,
            b"II\x2a\x00\0\0\0\0\0\0\0\0"
        ));
        assert!(!signature_matches(FileKind::Image, b"not an image"));
        assert!(!signature_matches(FileKind::Image, b"short"));
    }
}
