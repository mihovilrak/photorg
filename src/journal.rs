//! Append-only JSONL journal. No database: one line per completed
//! operation, flushed immediately so a `kill -9` still leaves a usable file.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::plan::{Action, Operation};

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    source: String,
    destination: String,
}

pub struct Journal {
    writer: Mutex<BufWriter<File>>,
}

impl Journal {
    /// Open for append; an existing journal is extended, never truncated, so
    /// `--resume` against the same file keeps working across several attempts.
    pub fn open(path: &Path) -> std::io::Result<Journal> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Journal {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Record a completed operation. Skips are recorded too: on resume they are
    /// as settled as a copy, and re-checking them costs a stat and a hash.
    pub fn record(&self, op: &Operation) {
        let Some(dest) = &op.dest_rel else { return };
        let entry = Entry {
            source: op.source_rel.to_string_lossy().into_owned(),
            destination: dest.to_string_lossy().into_owned(),
        };
        let Ok(line) = serde_json::to_string(&entry) else {
            return;
        };
        if let Ok(mut w) = self.writer.lock() {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }
    }

    pub fn record_if_done(&self, op: &Operation, ok: bool) {
        if ok && op.action != Action::Skip {
            self.record(op);
        }
    }
}

/// Read a journal into the `done` map the planner consumes. Malformed lines are
/// skipped: a truncated final line is the expected shape of an interrupted run.
pub fn load(path: &Path) -> std::io::Result<HashMap<PathBuf, PathBuf>> {
    let file = File::open(path)?;
    let mut out = HashMap::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(&line) {
            Ok(e) => {
                out.insert(PathBuf::from(e.source), PathBuf::from(e.destination));
            }
            Err(e) => log::debug!("ignoring journal line: {e}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Reason;

    fn op(source: &str, dest: Option<&str>, action: Action) -> Operation {
        Operation {
            source_rel: PathBuf::from(source),
            dest_rel: dest.map(PathBuf::from),
            action,
            reason: Reason::Planned,
            bytes: 1,
            duplicate_of: None,
        }
    }

    #[test]
    fn round_trips_completed_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.jsonl");
        {
            let j = Journal::open(&path).unwrap();
            j.record_if_done(&op("a.jpg", Some("2026/a.jpg"), Action::Copy), true);
            j.record_if_done(&op("b.jpg", Some("2026/b.jpg"), Action::Copy), false);
            j.record_if_done(&op("c.jpg", Some("2026/c.jpg"), Action::Skip), true);
        }
        let done = load(&path).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[&PathBuf::from("a.jpg")], PathBuf::from("2026/a.jpg"));
    }

    #[test]
    fn appends_across_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.jsonl");
        Journal::open(&path)
            .unwrap()
            .record(&op("a.jpg", Some("a.jpg"), Action::Copy));
        Journal::open(&path)
            .unwrap()
            .record(&op("b.jpg", Some("b.jpg"), Action::Copy));
        assert_eq!(load(&path).unwrap().len(), 2);
    }

    #[test]
    fn a_truncated_last_line_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.jsonl");
        std::fs::write(
            &path,
            "{\"source\":\"a.jpg\",\"destination\":\"a.jpg\"}\n{\"source\":\"b.jp",
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().len(), 1);
    }
}
