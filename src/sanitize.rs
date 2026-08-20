//! Path component sanitization.
//!
//! Applied to every rendered path component on every platform, so that a
//! collection organized on Linux transfers to Windows unchanged.

use unicode_normalization::UnicodeNormalization;

/// Max bytes per path component after sanitization.
pub const MAX_COMPONENT_LEN: usize = 100;

/// Windows starts rejecting paths beyond this without the verbatim prefix.
pub const WINDOWS_PATH_LIMIT: usize = 259;

pub const PLACEHOLDER: &str = "unnamed";
pub const UNKNOWN_DATE: &str = "unknown-date";
pub const UNKNOWN_LOCATION: &str = "unknown-location";

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn is_illegal(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20
}

/// Sanitize one path component (a directory name or a file name).
pub fn component(input: &str) -> String {
    // NFC first: macOS hands back NFD, and the two forms are distinct bytes on
    // a case-sensitive filesystem even though they render identically.
    let normalized: String = input.nfc().collect();

    let mut out = String::with_capacity(normalized.len());
    for c in normalized.chars() {
        out.push(if is_illegal(c) { '-' } else { c });
    }

    truncate_chars(&mut out, MAX_COMPONENT_LEN);

    // Windows silently drops these, so two names that differ only by a trailing
    // dot would collapse onto one another after the fact.
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let trimmed_start = out.trim_start_matches([' ', '.']).len();
    if trimmed_start != out.len() {
        out = out[out.len() - trimmed_start..].to_string();
    }

    if out.is_empty() {
        return PLACEHOLDER.to_string();
    }

    if is_reserved(&out) {
        out.insert(0, '_');
    }

    out
}

/// Sanitize a file name, preserving its extension through truncation.
pub fn file_name(input: &str) -> String {
    match input.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 && !ext.contains(' ') => {
            let ext = component(ext);
            let budget = MAX_COMPONENT_LEN.saturating_sub(ext.len() + 1).max(1);
            let mut stem = component(stem);
            truncate_chars(&mut stem, budget);
            while stem.ends_with('.') || stem.ends_with(' ') {
                stem.pop();
            }
            if stem.is_empty() {
                stem.push_str(PLACEHOLDER);
            }
            format!("{stem}.{ext}")
        }
        _ => component(input),
    }
}

fn truncate_chars(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let cut = (0..=max)
        .rev()
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0);
    s.truncate(cut);
}

fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r))
}

/// Prefix over-long Windows paths with the verbatim marker so the 260-char
/// limit does not apply.
#[cfg(windows)]
pub fn extended_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::PathBuf;
    let s = path.to_string_lossy();
    if s.len() < WINDOWS_PATH_LIMIT || s.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    // Only absolute paths can be made verbatim.
    match path.is_absolute() {
        true if s.starts_with(r"\\") => PathBuf::from(format!(r"\\?\UNC\{}", &s[2..])),
        true => PathBuf::from(format!(r"\\?\{s}")),
        false => path.to_path_buf(),
    }
}

#[cfg(not(windows))]
pub fn extended_path(path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_illegal_characters() {
        assert_eq!(component("a/b\\c:d*e?f"), "a-b-c-d-e-f");
        assert_eq!(component("tab\there"), "tab-here");
    }

    #[test]
    fn strips_trailing_dots_and_spaces() {
        assert_eq!(component("folder..."), "folder");
        assert_eq!(component("folder   "), "folder");
        assert_eq!(component("  .hidden"), "hidden");
    }

    #[test]
    fn prefixes_reserved_names() {
        assert_eq!(component("CON"), "_CON");
        assert_eq!(component("com1"), "_com1");
        assert_eq!(component("NUL.txt"), "_NUL.txt");
        assert_eq!(component("CONSOLE"), "CONSOLE");
    }

    #[test]
    fn empty_becomes_placeholder() {
        assert_eq!(component(""), PLACEHOLDER);
        assert_eq!(component("..."), PLACEHOLDER);
        assert_eq!(component("   "), PLACEHOLDER);
    }

    #[test]
    fn normalizes_to_nfc() {
        // "Zürich" spelled NFD (u + combining diaeresis).
        let nfd = "Zu\u{0308}rich";
        assert_eq!(component(nfd), "Zürich");
        assert_eq!(component(nfd), component("Zürich"));
    }

    #[test]
    fn caps_component_length() {
        let long = "x".repeat(500);
        assert_eq!(component(&long).len(), MAX_COMPONENT_LEN);
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let long = "é".repeat(200);
        let out = component(&long);
        assert!(out.len() <= MAX_COMPONENT_LEN);
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn file_name_keeps_extension() {
        let name = format!("{}.jpg", "a".repeat(300));
        let out = file_name(&name);
        assert!(out.ends_with(".jpg"));
        assert!(out.len() <= MAX_COMPONENT_LEN);
    }
}
