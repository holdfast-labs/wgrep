//! Bounded-memory accumulation of trigram postings for the index builder.
//!
//! `build.rs` pushes `(key, id, mask)` triples into a [`SpillBuffer`] as it
//! walks files in id order. Once the buffer's estimated size crosses a
//! budget, it is sorted by `(key, id)` and written out as a "run" file. A run
//! is just the postings for a contiguous slice of build time: for each key,
//! in ascending order, a delta-varint key, a varint count, then per posting a
//! delta-varint id and its mask byte (see `write_run`/`RunReader`). Because
//! file ids are assigned in strictly increasing order as chunks merge (see
//! `build::build`), two runs that both hold postings for the same key always
//! have the earlier run's ids all less than the later run's ids — so a
//! multi-run merge only needs to concatenate per-key groups in run-creation
//! order, never re-sort them.
//!
//! [`reduce_to_fanin`] merges runs down in levels so no merge step opens more
//! than `FAN_IN` files at once, and [`MergeIter`] streams the final merge as
//! an iterator that `format::write` consumes directly, so the merged
//! postings are never all resident in memory at once.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

/// No merge step opens more than this many run files at once, so file-handle
/// and per-run buffering costs stay bounded regardless of corpus size.
pub(crate) const FAN_IN: usize = 64;

struct Posting {
    key: u32,
    id: u32,
    mask: u8,
}

/// Removes a build's temporary directory when dropped, so spill and
/// intermediate-merge runs never outlive the build, including on an error
/// or early return.
pub(crate) struct TmpDirGuard(pub(crate) PathBuf);

impl Drop for TmpDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Accumulates postings in memory up to a byte budget, spilling sorted runs
/// to `dir` when the budget is crossed. Always produces at least one run
/// (via [`SpillBuffer::finish`]) so callers have a single code path whether
/// or not a build was ever large enough to spill mid-way.
pub(crate) struct SpillBuffer {
    dir: PathBuf,
    budget_bytes: u64,
    buffer: Vec<Posting>,
    runs: Vec<PathBuf>,
    next_run: u32,
}

impl SpillBuffer {
    pub(crate) fn new(dir: PathBuf, budget_bytes: u64) -> SpillBuffer {
        SpillBuffer { dir, budget_bytes, buffer: Vec::new(), runs: Vec::new(), next_run: 0 }
    }

    pub(crate) fn push(&mut self, key: u32, id: u32, mask: u8) -> io::Result<()> {
        self.buffer.push(Posting { key, id, mask });
        if (self.buffer.len() * size_of::<Posting>()) as u64 >= self.budget_bytes {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.sort_unstable_by_key(|p| (p.key, p.id));
        let path = self.dir.join(format!("spill-{:06}.wgs", self.next_run));
        self.next_run += 1;
        write_run(&path, &self.buffer)?;
        self.runs.push(path);
        self.buffer.clear();
        Ok(())
    }

    /// Flushes any remaining postings and returns the run files, in
    /// creation order (which is also id order, per the module doc).
    pub(crate) fn finish(mut self) -> io::Result<Vec<PathBuf>> {
        self.flush()?;
        Ok(self.runs)
    }
}

/// Writes `postings` (sorted by `(key, id)`) as one run file: for each key,
/// in ascending order, a delta-varint key, a varint count, a delta-varint id
/// per posting, then that many mask bytes in id order.
fn write_run(path: &Path, postings: &[Posting]) -> io::Result<()> {
    let file = fs::File::create(path)?;
    let mut out = BufWriter::new(file);
    let mut header = Vec::new();
    let mut masks = Vec::new();
    let mut prev_key: Option<u32> = None;
    let mut i = 0;
    while i < postings.len() {
        let key = postings[i].key;
        let j = i + postings[i..].iter().take_while(|p| p.key == key).count();
        header.clear();
        put_varint(&mut header, u64::from(prev_key.map_or(key, |p| key - p)));
        put_varint(&mut header, (j - i) as u64);
        masks.clear();
        let mut prev_id = 0u32;
        for (k, p) in postings[i..j].iter().enumerate() {
            put_varint(&mut header, u64::from(if k == 0 { p.id } else { p.id - prev_id }));
            prev_id = p.id;
            masks.push(p.mask);
        }
        out.write_all(&header)?;
        out.write_all(&masks)?;
        prev_key = Some(key);
        i = j;
    }
    out.flush()
}

/// Reads one run file sequentially, one key group at a time, so a merge
/// never needs a whole run resident in memory.
struct RunReader {
    data: BufReader<fs::File>,
    prev_key: Option<u32>,
}

impl RunReader {
    fn open(path: &Path) -> io::Result<RunReader> {
        Ok(RunReader { data: BufReader::new(fs::File::open(path)?), prev_key: None })
    }

    /// The next key's postings, or `None` at end of file.
    fn next_group(&mut self) -> io::Result<Option<(u32, Vec<u32>, Vec<u8>)>> {
        let Some(delta) = read_varint(&mut self.data)? else { return Ok(None) };
        let key = match self.prev_key {
            None => delta as u32,
            Some(p) => p + delta as u32,
        };
        self.prev_key = Some(key);
        let count = read_varint(&mut self.data)?.ok_or_else(eof)? as usize;
        let mut ids = Vec::with_capacity(count);
        let mut prev_id = 0u32;
        for k in 0..count {
            let delta = read_varint(&mut self.data)?.ok_or_else(eof)?;
            let id = if k == 0 { delta as u32 } else { prev_id + delta as u32 };
            ids.push(id);
            prev_id = id;
        }
        let mut masks = vec![0u8; count];
        self.data.read_exact(&mut masks)?;
        Ok(Some((key, ids, masks)))
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "truncated wgrep spill run")
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Reads one varint, or `None` if the reader is exactly at end of file
/// (no bytes at all before the first byte of the varint).
fn read_varint(r: &mut impl Read) -> io::Result<Option<u64>> {
    let mut v = 0u64;
    let mut shift = 0;
    let mut first = [0u8; 1];
    loop {
        let n = r.read(&mut first)?;
        if n == 0 {
            return if shift == 0 { Ok(None) } else { Err(eof()) };
        }
        let b = first[0];
        v |= u64::from(b & 0x7F) << shift;
        if b < 0x80 {
            return Ok(Some(v));
        }
        shift += 7;
        if shift > 63 {
            return Err(eof());
        }
    }
}

/// One run's cursor within a k-way merge: its reader plus the key group it
/// has read but not yet emitted.
struct Lane {
    reader: RunReader,
    front: Option<(u32, Vec<u32>, Vec<u8>)>,
}

/// Merges several runs (already sorted by key, each key's ids ascending)
/// into single per-key groups, in ascending key order, without holding more
/// than one key's postings in memory at a time.
pub(crate) struct MergeIter {
    lanes: Vec<Lane>,
    heap: BinaryHeap<Reverse<(u32, usize)>>,
}

impl MergeIter {
    pub(crate) fn open(run_paths: &[PathBuf]) -> io::Result<MergeIter> {
        let mut lanes = Vec::with_capacity(run_paths.len());
        let mut heap = BinaryHeap::new();
        for (i, path) in run_paths.iter().enumerate() {
            let mut reader = RunReader::open(path)?;
            let front = reader.next_group()?;
            if let Some((key, _, _)) = &front {
                heap.push(Reverse((*key, i)));
            }
            lanes.push(Lane { reader, front });
        }
        Ok(MergeIter { lanes, heap })
    }

    fn advance(&mut self, i: usize) -> io::Result<()> {
        let next = self.lanes[i].reader.next_group()?;
        if let Some((key, _, _)) = &next {
            self.heap.push(Reverse((*key, i)));
        }
        self.lanes[i].front = next;
        Ok(())
    }

    fn next_result(&mut self) -> io::Result<Option<(u32, Vec<u32>, Vec<u8>)>> {
        let Some(&Reverse((key, _))) = self.heap.peek() else { return Ok(None) };
        let mut ids = Vec::new();
        let mut masks = Vec::new();
        while let Some(&Reverse((k, i))) = self.heap.peek() {
            if k != key {
                break;
            }
            self.heap.pop();
            let (_, lane_ids, lane_masks) = self.lanes[i].front.take().unwrap();
            // Runs are flushed in id order, so a later lane's ids for the
            // same key are always greater than every id already collected.
            debug_assert!(
                ids.last().is_none_or(|&last| lane_ids.first().is_none_or(|&first| first > last)),
                "spill runs out of id order for trigram key {key}"
            );
            ids.extend(lane_ids);
            masks.extend(lane_masks);
            self.advance(i)?;
        }
        Ok(Some((key, ids, masks)))
    }
}

impl Iterator for MergeIter {
    type Item = io::Result<(u32, Vec<u32>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_result().transpose()
    }
}

/// Merges `runs` down in levels, so that at most `fan_in` run files are ever
/// open at once, until at most `fan_in` runs remain; returns that remaining
/// list (unmerged, ready for a final [`MergeIter`]). Intermediate merged
/// runs are written into `dir` with names distinct from `runs`, and the
/// input runs for each level are removed once merged into the next.
pub(crate) fn reduce_to_fanin(mut runs: Vec<PathBuf>, dir: &Path, fan_in: usize) -> io::Result<Vec<PathBuf>> {
    let mut level = 0u32;
    while runs.len() > fan_in {
        let mut next = Vec::with_capacity(runs.len().div_ceil(fan_in));
        for (i, group) in runs.chunks(fan_in).enumerate() {
            let out_path = dir.join(format!("merge-{level:03}-{i:06}.wgs"));
            let mut merged = MergeIter::open(group)?;
            write_run_from_iter(&out_path, &mut merged)?;
            next.push(out_path);
        }
        for path in &runs {
            let _ = fs::remove_file(path);
        }
        runs = next;
        level += 1;
    }
    Ok(runs)
}

/// Drains a merge into a single run file, reusing the run format so
/// intermediate merge levels are indistinguishable from leaf spill runs.
fn write_run_from_iter(path: &Path, iter: &mut MergeIter) -> io::Result<()> {
    let file = fs::File::create(path)?;
    let mut out = BufWriter::new(file);
    let mut buf = Vec::new();
    let mut prev_key: Option<u32> = None;
    while let Some(group) = iter.next_result()? {
        let (key, ids, masks) = group;
        buf.clear();
        put_varint(&mut buf, u64::from(match prev_key {
            None => key,
            Some(p) => key - p,
        }));
        put_varint(&mut buf, ids.len() as u64);
        let mut prev_id = 0u32;
        for (k, &id) in ids.iter().enumerate() {
            put_varint(&mut buf, u64::from(if k == 0 { id } else { id - prev_id }));
            prev_id = id;
        }
        out.write_all(&buf)?;
        out.write_all(&masks)?;
        prev_key = Some(key);
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_round_trips_through_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.wgs");
        let postings = vec![
            Posting { key: 5, id: 0, mask: 0x1 },
            Posting { key: 5, id: 3, mask: 0x2 },
            Posting { key: 5, id: 9, mask: 0x4 },
            Posting { key: 7, id: 1, mask: 0x8 },
            Posting { key: 1000, id: 2, mask: 0x10 },
        ];
        write_run(&path, &postings).unwrap();
        let mut reader = RunReader::open(&path).unwrap();
        assert_eq!(reader.next_group().unwrap(), Some((5, vec![0, 3, 9], vec![0x1, 0x2, 0x4])));
        assert_eq!(reader.next_group().unwrap(), Some((7, vec![1], vec![0x8])));
        assert_eq!(reader.next_group().unwrap(), Some((1000, vec![2], vec![0x10])));
        assert_eq!(reader.next_group().unwrap(), None);
    }

    #[test]
    fn empty_run_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.wgs");
        write_run(&path, &[]).unwrap();
        let mut reader = RunReader::open(&path).unwrap();
        assert_eq!(reader.next_group().unwrap(), None);
    }

    #[test]
    fn merge_iter_combines_runs_in_key_and_id_order() {
        let dir = tempfile::tempdir().unwrap();
        // Ids are assigned in increasing order over time, so a later run's
        // ids for a shared key are always greater than an earlier run's.
        let run_a = vec![
            Posting { key: 1, id: 0, mask: 1 },
            Posting { key: 3, id: 1, mask: 1 },
        ];
        let run_b = vec![
            Posting { key: 1, id: 5, mask: 2 },
            Posting { key: 2, id: 6, mask: 2 },
        ];
        let path_a = dir.path().join("a.wgs");
        let path_b = dir.path().join("b.wgs");
        write_run(&path_a, &run_a).unwrap();
        write_run(&path_b, &run_b).unwrap();
        let merged: Vec<_> = MergeIter::open(&[path_a, path_b]).unwrap().map(Result::unwrap).collect();
        assert_eq!(
            merged,
            vec![
                (1, vec![0, 5], vec![1, 2]),
                (2, vec![6], vec![2]),
                (3, vec![1], vec![1]),
            ]
        );
    }
}
