use std::path::PathBuf;
use thiserror::Error;

/// Errors that abort the whole run.
#[derive(Debug, Error)]
pub enum FatalError {
    #[error("source directory does not exist or is not a directory: {0}")]
    SourceUnavailable(PathBuf),

    #[error("destination is unavailable: {path}: {source}")]
    DestinationUnavailable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("source and destination overlap ({source_root} <-> {dest_root}); refusing to run")]
    Overlap {
        source_root: PathBuf,
        dest_root: PathBuf,
    },

    #[error("invalid template: {0}")]
    Template(#[from] TemplateError),

    #[error(
        "not enough free space on destination: need {needed} bytes, {available} bytes available"
    )]
    DiskFull { needed: u64, available: u64 },

    #[error("--on-conflict overwrite requires --force")]
    OverwriteWithoutForce,

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error("unclosed '{{' at byte {0}")]
    UnclosedBrace(usize),

    #[error("unexpected '}}' at byte {0}")]
    StrayBrace(usize),

    #[error("unknown variable `{0}`")]
    UnknownVariable(String),

    #[error("invalid padding spec `{0}`")]
    BadPadding(String),

    #[error("template must not contain `..` path segments")]
    ParentEscape,

    #[error("template must be relative, not absolute")]
    Absolute,

    #[error("template is empty")]
    Empty,
}

/// Errors attached to a single file. Never abort the run.
#[derive(Debug, Error)]
pub enum FileError {
    #[error("permission denied")]
    PermissionDenied,

    #[error("unreadable: {0}")]
    Unreadable(String),

    #[error("not a recognized image (signature mismatch)")]
    SignatureMismatch,

    #[error("copy failed: {0}")]
    Copy(String),
}

pub type Result<T> = std::result::Result<T, FatalError>;
