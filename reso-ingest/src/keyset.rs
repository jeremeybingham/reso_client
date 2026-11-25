//! Key sets at millions of keys: an external sort of the pulled keys and a streaming diff (#87)
//!
//! A key is handled as its JSON text (`"K1"`, `42`), one per line; JSON text never holds a raw
//! newline. Key sets are sorted by those bytes and hold no repeats, so two of them diff by merging
//! two sorted streams. Pulled keys are sorted in chunks of [`CHUNK_KEYS`] that spill to run files,
//! and the runs are merged; memory stays at about one chunk whatever the number of keys.

use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Lines, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::error::IngestError;

/// Keys held in memory before they are sorted and spilled to a run file
pub(crate) const CHUNK_KEYS: usize = 100_000;

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn io_error(path: &Path, e: impl std::fmt::Display) -> IngestError {
    IngestError::Output {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

/// A fresh directory for one pass's temporary files, removed with everything in it on drop
pub(crate) struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// A new directory under `base` (created if missing)
    pub(crate) fn new(base: &Path) -> Result<Self, IngestError> {
        let n = SCRATCH_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let path = base.join(format!("keys-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).map_err(|e| io_error(&path, e))?;
        Ok(Self { path })
    }

    pub(crate) fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Sorts any number of keys with bounded memory: chunks are sorted, deduplicated and spilled to
/// run files in a [`ScratchDir`], then merged by [`finish`](Self::finish)
pub(crate) struct ExternalSorter<'a> {
    scratch: &'a ScratchDir,
    chunk: Vec<String>,
    chunk_keys: usize,
    runs: Vec<PathBuf>,
}

impl<'a> ExternalSorter<'a> {
    pub(crate) fn new(scratch: &'a ScratchDir, chunk_keys: usize) -> Self {
        Self {
            scratch,
            chunk: Vec::new(),
            chunk_keys: chunk_keys.max(1),
            runs: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, key: String) -> Result<(), IngestError> {
        self.chunk.push(key);
        if self.chunk.len() >= self.chunk_keys {
            self.spill()?;
        }
        Ok(())
    }

    fn spill(&mut self) -> Result<(), IngestError> {
        self.chunk.sort_unstable();
        self.chunk.dedup();
        let n = SCRATCH_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let path = self.scratch.file(&format!("run-{n}"));
        let mut out = BufWriter::new(File::create(&path).map_err(|e| io_error(&path, e))?);
        for key in self.chunk.drain(..) {
            writeln!(out, "{key}").map_err(|e| io_error(&path, e))?;
        }
        out.flush().map_err(|e| io_error(&path, e))?;
        self.runs.push(path);
        Ok(())
    }

    /// The keys in byte order without repeats
    pub(crate) fn finish(mut self) -> Result<SortedKeys, IngestError> {
        if !self.chunk.is_empty() {
            self.spill()?;
        }
        let mut heads = Vec::with_capacity(self.runs.len());
        for path in self.runs {
            let mut lines = KeyLines::open(&path)?;
            let first = lines.next_key()?;
            heads.push((first, lines));
        }
        Ok(SortedKeys { heads, last: None })
    }
}

/// Lines of a key file, as keys
pub(crate) struct KeyLines {
    path: PathBuf,
    lines: Lines<BufReader<File>>,
}

impl KeyLines {
    pub(crate) fn open(path: &Path) -> Result<Self, IngestError> {
        let file = File::open(path).map_err(|e| io_error(path, e))?;
        Ok(Self {
            path: path.to_path_buf(),
            lines: BufReader::new(file).lines(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn next_key(&mut self) -> Result<Option<String>, IngestError> {
        self.lines
            .next()
            .transpose()
            .map_err(|e| io_error(&self.path, e))
    }
}

/// The merge of sorted run files: every key once, in byte order
pub(crate) struct SortedKeys {
    heads: Vec<(Option<String>, KeyLines)>,
    last: Option<String>,
}

impl SortedKeys {
    pub(crate) fn next_key(&mut self) -> Result<Option<String>, IngestError> {
        loop {
            // Few runs (one per CHUNK_KEYS keys), so a linear scan for the smallest head is enough
            let Some(i) = self
                .heads
                .iter()
                .enumerate()
                .filter_map(|(i, (head, _))| head.as_ref().map(|h| (i, h)))
                .min_by(|a, b| a.1.cmp(b.1))
                .map(|(i, _)| i)
            else {
                return Ok(None);
            };
            let (head, lines) = &mut self.heads[i];
            let next = lines.next_key()?;
            let key = std::mem::replace(head, next).unwrap_or_default();
            if self.last.as_ref() != Some(&key) {
                self.last = Some(key.clone());
                return Ok(Some(key));
            }
        }
    }
}

/// What a diff of the saved key set against the pulled one found
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DiffCounts {
    pub(crate) previous: u64,
    pub(crate) current: u64,
    pub(crate) deleted: u64,
}

/// Merge `previous` (sorted, as saved) with `current`, writing every current key to `next` and
/// every previous key missing from `current` to `deleted`
///
/// # Errors
///
/// [`IngestError::Output`] when a file cannot be read or written, or `previous` is out of order
/// (a damaged key set; diffing it would report wrong deletes).
pub(crate) fn diff(
    mut previous: Option<KeyLines>,
    current: &mut SortedKeys,
    next: &mut dyn Write,
    next_path: &Path,
    deleted: &mut dyn Write,
    deleted_path: &Path,
) -> Result<DiffCounts, IngestError> {
    let mut counts = DiffCounts::default();
    let mut prev_last: Option<String> = None;
    let mut read_previous =
        |previous: &mut Option<KeyLines>| -> Result<Option<String>, IngestError> {
            let Some(lines) = previous.as_mut() else {
                return Ok(None);
            };
            let Some(key) = lines.next_key()? else {
                return Ok(None);
            };
            if prev_last.as_ref().is_some_and(|last| *last >= key) {
                return Err(io_error(
                    lines.path(),
                    "the saved key set is not sorted; delete it to start a new baseline",
                ));
            }
            prev_last = Some(key.clone());
            Ok(Some(key))
        };
    let mut p = read_previous(&mut previous)?;
    let mut c = current.next_key()?;
    loop {
        let order = match (&p, &c) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(p), Some(c)) => p.cmp(c),
        };
        if order != Ordering::Greater {
            if let Some(key) = p.take() {
                counts.previous += 1;
                if order == Ordering::Less {
                    counts.deleted += 1;
                    writeln!(deleted, "{key}").map_err(|e| io_error(deleted_path, e))?;
                }
            }
            p = read_previous(&mut previous)?;
        }
        if order != Ordering::Less {
            if let Some(key) = c.take() {
                counts.current += 1;
                writeln!(next, "{key}").map_err(|e| io_error(next_path, e))?;
            }
            c = current.next_key()?;
        }
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(scratch: &ScratchDir, keys: &[&str], chunk: usize) -> SortedKeys {
        let mut sorter = ExternalSorter::new(scratch, chunk);
        for k in keys {
            sorter.push(k.to_string()).unwrap();
        }
        sorter.finish().unwrap()
    }

    fn drain(mut keys: SortedKeys) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(k) = keys.next_key().unwrap() {
            out.push(k);
        }
        out
    }

    fn base(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("reso-keyset-{name}-{}", std::process::id()))
    }

    #[test]
    fn sorts_across_runs_and_drops_repeats() {
        let scratch = ScratchDir::new(&base("sort")).unwrap();
        let keys = ["\"b\"", "\"a\"", "3", "\"b\"", "\"c\"", "\"a\"", "1"];
        for chunk in [1, 2, 3, 100] {
            assert_eq!(
                drain(sorted(&scratch, &keys, chunk)),
                ["\"a\"", "\"b\"", "\"c\"", "1", "3"],
                "chunk {chunk}"
            );
        }
        assert!(drain(sorted(&scratch, &[], 2)).is_empty());
    }

    #[test]
    fn scratch_dir_is_removed_on_drop() {
        let scratch = ScratchDir::new(&base("drop")).unwrap();
        let path = scratch.path.clone();
        fs::write(scratch.file("x"), "1").unwrap();
        drop(scratch);
        assert!(!path.exists());
    }

    fn run_diff(previous: Option<&str>, current: &[&str]) -> (DiffCounts, String, String) {
        let scratch = ScratchDir::new(&base("diff")).unwrap();
        let prev = previous.map(|text| {
            let path = scratch.file("prev");
            fs::write(&path, text).unwrap();
            KeyLines::open(&path).unwrap()
        });
        let mut cur = sorted(&scratch, current, 2);
        let (mut next, mut deleted) = (Vec::new(), Vec::new());
        let counts = diff(
            prev,
            &mut cur,
            &mut next,
            Path::new("next"),
            &mut deleted,
            Path::new("deleted"),
        )
        .unwrap();
        (
            counts,
            String::from_utf8(next).unwrap(),
            String::from_utf8(deleted).unwrap(),
        )
    }

    #[test]
    fn diff_reports_keys_that_left() {
        let (counts, next, deleted) = run_diff(
            Some("\"K1\"\n\"K2\"\n\"K3\"\n\"K5\"\n"),
            &["\"K4\"", "\"K1\"", "\"K3\""],
        );
        assert_eq!(
            counts,
            DiffCounts {
                previous: 4,
                current: 3,
                deleted: 2
            }
        );
        assert_eq!(next, "\"K1\"\n\"K3\"\n\"K4\"\n");
        assert_eq!(deleted, "\"K2\"\n\"K5\"\n");
    }

    #[test]
    fn diff_without_a_previous_set_deletes_nothing() {
        let (counts, next, deleted) = run_diff(None, &["2", "1"]);
        assert_eq!((counts.previous, counts.current, counts.deleted), (0, 2, 0));
        assert_eq!(next, "1\n2\n");
        assert_eq!(deleted, "");
    }

    #[test]
    fn diff_refuses_an_unsorted_previous_set() {
        let scratch = ScratchDir::new(&base("unsorted")).unwrap();
        let path = scratch.file("prev");
        fs::write(&path, "\"b\"\n\"a\"\n").unwrap();
        let mut cur = sorted(&scratch, &[], 2);
        let e = diff(
            Some(KeyLines::open(&path).unwrap()),
            &mut cur,
            &mut Vec::new(),
            Path::new("n"),
            &mut Vec::new(),
            Path::new("d"),
        )
        .unwrap_err();
        assert!(e.to_string().contains("not sorted"), "{e}");
    }
}
