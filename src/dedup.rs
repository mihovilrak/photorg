//! Duplicate detection.
//!
//! Definition for v1: **duplicate = identical bytes.** It is the only
//! definition implementable without false positives.
//!
//! The key is size alone, never `filename + size` — the most common real
//! duplicate is the same bytes under a different name (`IMG_1234 (1).jpg`,
//! re-imported cards).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use jwalk::WalkDir;

/// One seek at each end kills nearly all false pairs before a full read.
const EDGE_BYTES: u64 = 64 * 1024;

pub type Digest = [u8; 32];

/// BLAKE3 over the first and last 64 KB. There is no adversary here, and
/// BLAKE3 is several times faster than SHA-256 and internally parallel.
pub fn partial_hash(path: &Path, size: u64) -> std::io::Result<Digest> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());

    let head_len = size.min(EDGE_BYTES);
    let mut buf = vec![0u8; head_len as usize];
    file.read_exact(&mut buf)?;
    hasher.update(&buf);

    if size > EDGE_BYTES * 2 {
        file.seek(SeekFrom::End(-(EDGE_BYTES as i64)))?;
        let mut tail = vec![0u8; EDGE_BYTES as usize];
        file.read_exact(&mut tail)?;
        hasher.update(&tail);
    }
    Ok(*hasher.finalize().as_bytes())
}

pub fn full_hash(path: &Path) -> std::io::Result<Digest> {
    let mut hasher = blake3::Hasher::new();
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// How many files either map memoizes before it is thrown away and rebuilt.
/// Unbounded, a million-file run holds a path and a digest per file; a cache
/// miss only costs a re-read, so the ceiling is the cheaper end of the trade.
const CACHE_LIMIT: usize = 50_000;

/// Memoized hashes, so a file colliding with several candidates is read once.
#[derive(Default)]
pub struct HashCache {
    partial: HashMap<PathBuf, Digest>,
    full: HashMap<PathBuf, Digest>,
}

impl HashCache {
    pub fn partial(&mut self, path: &Path, size: u64) -> std::io::Result<Digest> {
        if let Some(d) = self.partial.get(path) {
            return Ok(*d);
        }
        let d = partial_hash(path, size)?;
        remember(&mut self.partial, path, d);
        Ok(d)
    }

    pub fn full(&mut self, path: &Path) -> std::io::Result<Digest> {
        if let Some(d) = self.full.get(path) {
            return Ok(*d);
        }
        let d = full_hash(path)?;
        remember(&mut self.full, path, d);
        Ok(d)
    }

    /// Identical bytes? size -> edge hash -> full hash.
    pub fn same_bytes(&mut self, a: &Path, b: &Path, size: u64) -> std::io::Result<bool> {
        if self.partial(a, size)? != self.partial(b, size)? {
            return Ok(false);
        }
        self.same_bytes_after_partial(a, b, size)
    }

    /// The tail of `same_bytes`, for callers that already know the edge hashes
    /// agree — an index keyed by the edge hash, say.
    pub fn same_bytes_after_partial(
        &mut self,
        a: &Path,
        b: &Path,
        size: u64,
    ) -> std::io::Result<bool> {
        // Files at or below the sampled window are fully covered already.
        if size <= EDGE_BYTES {
            return Ok(true);
        }
        Ok(self.full(a)? == self.full(b)?)
    }
}

/// Reassigning rather than clearing hands the table itself back to the
/// allocator; `HashMap::clear` would keep the capacity for the whole run.
fn remember(map: &mut HashMap<PathBuf, Digest>, path: &Path, digest: Digest) {
    if map.len() >= CACHE_LIMIT {
        *map = HashMap::new();
    }
    map.insert(path.to_path_buf(), digest);
}

/// `size -> paths` index of the destination subtree.
///
/// Without it, every rerun produces `IMG_1234_1.jpg`.
#[derive(Debug, Default)]
pub struct SizeIndex {
    by_size: HashMap<u64, Vec<PathBuf>>,
}

impl SizeIndex {
    pub fn build(root: &Path) -> SizeIndex {
        let mut index = SizeIndex::default();
        if !root.exists() {
            return index;
        }
        for entry in WalkDir::new(root).skip_hidden(false).follow_links(false) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            index.insert(meta.len(), entry.path());
        }
        index
    }

    pub fn insert(&mut self, size: u64, path: PathBuf) {
        self.by_size.entry(size).or_default().push(path);
    }

    pub fn candidates(&self, size: u64) -> &[PathBuf] {
        self.by_size.get(&size).map_or(&[], Vec::as_slice)
    }

    pub fn len(&self) -> usize {
        self.by_size.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.by_size.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }

    #[test]
    fn identical_bytes_under_different_names_match() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(tmp.path(), "IMG_1234.jpg", b"hello world");
        let b = write(tmp.path(), "IMG_1234 (1).jpg", b"hello world");
        let mut cache = HashCache::default();
        assert!(cache.same_bytes(&a, &b, 11).unwrap());
    }

    #[test]
    fn different_bytes_do_not_match() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(tmp.path(), "a.jpg", b"hello world");
        let b = write(tmp.path(), "b.jpg", b"hello w0rld");
        let mut cache = HashCache::default();
        assert!(!cache.same_bytes(&a, &b, 11).unwrap());
    }

    #[test]
    fn large_files_fall_through_to_a_full_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let mut a_bytes = vec![7u8; 200 * 1024];
        let mut b_bytes = a_bytes.clone();
        // Differ only in the middle, which the edge hash cannot see.
        a_bytes[100 * 1024] = 1;
        b_bytes[100 * 1024] = 2;
        let a = write(tmp.path(), "a.jpg", &a_bytes);
        let b = write(tmp.path(), "b.jpg", &b_bytes);
        let mut cache = HashCache::default();
        assert!(!cache.same_bytes(&a, &b, a_bytes.len() as u64).unwrap());
    }

    #[test]
    fn size_index_finds_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.jpg", b"12345");
        write(tmp.path(), "b.jpg", b"67890");
        write(tmp.path(), "c.jpg", b"123");
        let index = SizeIndex::build(tmp.path());
        assert_eq!(index.candidates(5).len(), 2);
        assert_eq!(index.candidates(3).len(), 1);
        assert!(index.candidates(99).is_empty());
    }
}
