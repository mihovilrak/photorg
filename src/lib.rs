//! Core library for the photo organizer.
//!
//! Nothing here knows about `clap`, stdout, or the terminal. The
//! binary owns argument parsing and rendering; this crate owns the work.

pub mod adaptive;
#[cfg(feature = "geocoding")]
mod cities;
pub mod companion;
pub mod config;
pub mod dates;
pub mod dedup;
pub mod error;
pub mod execute;
pub mod formats;
pub mod geocode;
pub mod journal;
pub mod metadata;
pub mod plan;
pub mod safety;
pub mod sanitize;
pub mod scan;
pub mod spill;
pub mod template;

pub use config::Options;
pub use error::{FatalError, FileError, Result, TemplateError};
