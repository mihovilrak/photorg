//! Spilling the plan to disk.
//!
//! Adaptive grouping cannot decide anything until every record has been seen,
//! so the whole plan exists before pass 2 starts. At a million files that costs
//! ~280 MB on top of the scan records, putting peak RSS over the memory budget.
//! Above [`THRESHOLD`] operations the planner streams them to a temp JSONL file
//! instead and pass 2 reads it back a chunk at a time.
//!
//! Paths are stored as JSON strings, so a path that is not valid Unicode fails
//! the spill rather than being silently mangled. That aborts the run with the
//! serializer's error, which is the honest outcome: the alternative is copying
//! to a destination nobody asked for.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::plan::Operation;

/// Operation count above which the plan goes to disk. 250k stays around 70 MB
/// resident, which leaves the budget intact whatever the scan records cost.
pub const THRESHOLD: usize = 250_000;

/// Operations read back a chunk at a time: big enough to keep a copy pool fed,
/// small enough that the chunk never shows up in the memory figures.
const CHUNK: usize = 4096;

/// Removes the temp file whenever the plan goes away, run finished or not.
#[derive(Debug)]
struct TempPath(PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A plan being written out, one JSON object per line.
#[derive(Debug)]
pub struct Writer {
    inner: BufWriter<File>,
    path: TempPath,
    len: usize,
}

impl Writer {
    /// `None` when no temp file can be created: the caller keeps the plan in
    /// memory rather than failing a run over an optimization.
    pub fn create() -> Option<Writer> {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        for attempt in 0..8u32 {
            let path = dir.join(format!("po-plan-{pid}-{attempt}.jsonl"));
            if let Ok(file) = File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                return Some(Writer {
                    inner: BufWriter::new(file),
                    path: TempPath(path),
                    len: 0,
                });
            }
        }
        None
    }

    pub fn path(&self) -> &Path {
        &self.path.0
    }

    pub fn push(&mut self, op: &Operation) -> io::Result<()> {
        serde_json::to_writer(&mut self.inner, op)?;
        self.inner.write_all(b"\n")?;
        self.len += 1;
        Ok(())
    }

    pub fn finish(self) -> io::Result<Spill> {
        let file = self.inner.into_inner().map_err(io::Error::from)?;
        Ok(Spill {
            file,
            path: self.path,
            len: self.len,
        })
    }
}

/// A finished plan on disk. The file handle is kept open, so removing the path
/// from under a running organize cannot break it.
#[derive(Debug)]
pub struct Spill {
    file: File,
    path: TempPath,
    len: usize,
}

impl Spill {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn path(&self) -> &Path {
        &self.path.0
    }

    /// The operations in planned order, [`CHUNK`] at a time.
    pub fn chunks(&self) -> io::Result<Chunks> {
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        Ok(Chunks {
            reader: BufReader::new(file),
            line: String::new(),
            read: 0,
            len: self.len,
            failed: false,
        })
    }
}

pub struct Chunks {
    reader: BufReader<File>,
    line: String,
    read: usize,
    len: usize,
    failed: bool,
}

impl Chunks {
    /// Operations not yet handed out, which is what an interrupted stream has
    /// to be charged for.
    pub fn remaining(&self) -> usize {
        self.len - self.read
    }
}

impl Iterator for Chunks {
    type Item = io::Result<Vec<Operation>>;

    fn next(&mut self) -> Option<io::Result<Vec<Operation>>> {
        if self.failed || self.read >= self.len {
            return None;
        }
        let mut out = Vec::with_capacity(CHUNK.min(self.len - self.read));
        while out.len() < CHUNK && self.read + out.len() < self.len {
            self.line.clear();
            let read = self.reader.read_line(&mut self.line);
            let parsed = match read {
                Ok(0) => break,
                Ok(_) => serde_json::from_str(self.line.trim_end())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
                Err(e) => Err(e),
            };
            match parsed {
                Ok(op) => out.push(op),
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            }
        }
        if out.is_empty() {
            // Fewer lines than the writer counted: treat the shortfall as a
            // failure rather than quietly organizing part of the plan.
            self.failed = true;
            return Some(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the spilled plan is shorter than it should be",
            )));
        }
        self.read += out.len();
        Some(Ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Action, Reason};

    fn op(n: usize) -> Operation {
        Operation {
            source_rel: PathBuf::from(format!("in/{n}.jpg")),
            dest_rel: Some(PathBuf::from(format!("2026/{n}.jpg"))),
            action: Action::Copy,
            reason: Reason::Planned,
            bytes: n as u64,
            duplicate_of: None,
        }
    }

    fn spill(count: usize) -> Spill {
        let mut w = Writer::create().unwrap();
        for i in 0..count {
            w.push(&op(i)).unwrap();
        }
        w.finish().unwrap()
    }

    #[test]
    fn round_trips_in_order() {
        let s = spill(10);
        let back: Vec<Operation> = s.chunks().unwrap().flat_map(|c| c.unwrap()).collect();
        assert_eq!(back.len(), 10);
        assert_eq!(back[7].source_rel, PathBuf::from("in/7.jpg"));
        assert_eq!(back[7].bytes, 7);
        assert_eq!(back[7].dest_rel, Some(PathBuf::from("2026/7.jpg")));
    }

    #[test]
    fn splits_into_chunks() {
        let s = spill(CHUNK + 5);
        let sizes: Vec<usize> = s.chunks().unwrap().map(|c| c.unwrap().len()).collect();
        assert_eq!(sizes, vec![CHUNK, 5]);
    }

    #[test]
    fn empty_plan_yields_nothing() {
        let s = spill(0);
        assert!(s.is_empty());
        assert_eq!(s.chunks().unwrap().count(), 0);
    }

    #[test]
    fn truncation_is_an_error_not_a_short_read() {
        let s = spill(4);
        s.file.set_len(0).unwrap();
        let mut chunks = s.chunks().unwrap();
        assert!(chunks.next().unwrap().is_err());
        assert_eq!(chunks.remaining(), 4);
        assert!(chunks.next().is_none());
    }

    #[test]
    fn the_file_goes_away_with_the_spill() {
        let s = spill(1);
        let path = s.path().to_path_buf();
        assert!(path.exists());
        drop(s);
        assert!(!path.exists());
    }
}
