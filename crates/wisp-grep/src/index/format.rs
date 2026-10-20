//! On-disk trigram index: one little-endian file, memory-mapped for queries.
//!
//! Layout: `MAGIC | u32 header_len | header JSON | section table | sections`.
//! The section table holds `(offset, len)` as two u64s for each section in
//! `Section` order. Files are sorted by relative path so lookups binary
//! search the path table. Each trigram's posting record starts with a tag:
//! a delta-varint list of file ids, a bitmap over all files, or "dense",
//! meaning the trigram was not stored and every file must be considered.
//! List and bitmap records end with one next-byte mask per file, in file id
//! order (see `fold::next_bit`). The index holds no file content; search
//! always reads the source file.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use memmap2::Mmap;

use super::plan::Query;

pub(crate) const MAGIC: &[u8; 8] = b"WGRPIDX1";
pub(crate) const VERSION: u64 = 5;

const TAG_LIST: u8 = 0;
const TAG_BITMAP: u8 = 1;
const TAG_DENSE: u8 = 2;

#[derive(Clone, Copy)]
enum Section {
    Paths = 0,
    PathOffsets,
    /// Per file: size (`u64`) then mtime (`i64`), interleaved so the
    /// freshness check touches one cache line.
    Meta,
    Keys,
    PostingOffsets,
    Postings,
}
const SECTIONS: usize = 6;

pub(crate) struct FileEntry {
    /// Path relative to the index root, `/`-separated.
    pub(crate) rel: String,
    pub(crate) size: u64,
    pub(crate) mtime_ns: i64,
    /// Platform change token: inode change time (Unix) or file index (Windows).
    pub(crate) change_ns: i64,
}

pub(crate) struct BuildStats {
    pub(crate) bytes: u64,
    pub(crate) trigrams: usize,
    pub(crate) dense_trigrams: usize,
}

/// Writes an index. `files` must be sorted by `rel`. `postings` yields
/// `(key, ids, masks)` triples in ascending key order, one per trigram; each
/// `ids` list must hold increasing file ids with `masks` its parallel list
/// of next-byte masks. A trigram present in at least `dense_percent` of
/// files (when the index has at least 64 files) is stored as dense instead
/// of as a list.
///
/// The postings blob (by far the largest section on a big corpus) is
/// streamed to a temporary file next to `path` as `postings` is consumed,
/// rather than built up in memory, and copied into the final file only once
/// its length is known; every other section is small (proportional to the
/// file and trigram *counts*, not to source size) and kept in memory as
/// before.
pub(crate) fn write(
    path: &Path,
    header: &serde_json::Value,
    files: &[FileEntry],
    postings: impl Iterator<Item = io::Result<(u32, Vec<u32>, Vec<u8>)>>,
    dense_percent: u64,
) -> io::Result<BuildStats> {
    let n = files.len();
    // One entry per section except `Postings`, which is written straight to
    // `postings_tmp_path` below and appended to the output file last.
    let mut sections: Vec<Vec<u8>> = vec![Vec::new(); SECTIONS - 1];

    let mut offset = 0u64;
    for f in files {
        sections[Section::PathOffsets as usize].extend_from_slice(&u64_le(offset));
        sections[Section::Paths as usize].extend_from_slice(f.rel.as_bytes());
        offset += f.rel.len() as u64;
        sections[Section::Meta as usize].extend_from_slice(&f.size.to_le_bytes());
        sections[Section::Meta as usize].extend_from_slice(&f.mtime_ns.to_le_bytes());
        sections[Section::Meta as usize].extend_from_slice(&f.change_ns.to_le_bytes());
    }
    sections[Section::PathOffsets as usize].extend_from_slice(&u64_le(offset));

    let bitmap_len = n.div_ceil(8);
    let postings_tmp_path = path.with_extension("postings.tmp");
    let blob = write_postings_blob(&postings_tmp_path, postings, n, dense_percent, bitmap_len, &mut sections);
    let (postings_len, trigrams, dense) = match blob {
        Ok(v) => v,
        Err(err) => {
            let _ = fs::remove_file(&postings_tmp_path);
            return Err(err);
        }
    };

    let header = serde_json::to_vec(header).map_err(io::Error::other)?;
    let table_start = MAGIC.len() + 4 + header.len();
    let mut data_start = (table_start + SECTIONS * 16) as u64;
    let mut file = fs::File::create(path)?;
    let assembled = write_head_and_sections(&mut file, MAGIC, &header, &sections, postings_len, &mut data_start);
    if let Err(err) = assembled {
        let _ = fs::remove_file(&postings_tmp_path);
        return Err(err);
    }
    let copied = io::copy(&mut fs::File::open(&postings_tmp_path)?, &mut file);
    let _ = fs::remove_file(&postings_tmp_path);
    copied?;
    file.sync_all()?;
    Ok(BuildStats { bytes: data_start, trigrams, dense_trigrams: dense })
}

/// Consumes `postings`, streaming the encoded records to `tmp_path` and
/// filling in the `Keys` and `PostingOffsets` sections (whose final entry is
/// the total blob length, so they have one more entry than there are
/// sections, as `Index::open` expects). Returns the blob length, the number
/// of trigrams, and how many were stored dense.
fn write_postings_blob(
    tmp_path: &Path,
    postings: impl Iterator<Item = io::Result<(u32, Vec<u32>, Vec<u8>)>>,
    n: usize,
    dense_percent: u64,
    bitmap_len: usize,
    sections: &mut [Vec<u8>],
) -> io::Result<(u64, usize, usize)> {
    let mut blob = BufWriter::new(fs::File::create(tmp_path)?);
    let (mut postings_len, mut trigrams, mut dense) = (0u64, 0, 0);
    for item in postings {
        let (key, ids, masks) = item?;
        trigrams += 1;
        sections[Section::Keys as usize].extend_from_slice(&key.to_le_bytes());
        put_offset(sections, postings_len)?;
        if n >= 64 && ids.len() as u64 * 100 >= n as u64 * dense_percent {
            blob.write_all(&[TAG_DENSE])?;
            postings_len += 1;
            dense += 1;
            continue;
        }
        let mut list = Vec::new();
        put_varint(&mut list, ids.len() as u64);
        let mut prev = 0u32;
        for (i, &id) in ids.iter().enumerate() {
            put_varint(&mut list, u64::from(if i == 0 { id } else { id - prev }));
            prev = id;
        }
        if list.len() <= bitmap_len {
            blob.write_all(&[TAG_LIST])?;
            blob.write_all(&list)?;
            postings_len += 1 + list.len() as u64;
        } else {
            let mut bitmap = vec![0u8; bitmap_len];
            for &id in &ids {
                bitmap[id as usize / 8] |= 1 << (id % 8);
            }
            blob.write_all(&[TAG_BITMAP])?;
            blob.write_all(&bitmap)?;
            postings_len += 1 + bitmap.len() as u64;
        }
        blob.write_all(&masks)?;
        postings_len += masks.len() as u64;
    }
    put_offset(sections, postings_len)?;
    blob.flush()?;
    Ok((postings_len, trigrams, dense))
}

/// Appends a u32 posting offset (the format caps the postings blob at 4 GiB so
/// the offset table is half the size of a u64 one).
fn put_offset(sections: &mut [Vec<u8>], v: u64) -> io::Result<()> {
    let v = u32::try_from(v).map_err(|_| io::Error::other("postings exceed 4 GiB"))?;
    sections[Section::PostingOffsets as usize].extend_from_slice(&v.to_le_bytes());
    Ok(())
}

/// Writes everything but the postings blob itself: the magic, header,
/// section table (`sections` in order, then the postings entry using
/// `postings_len`), and `sections`' own bytes. `data_start` is advanced by
/// each section's length so it ends up as the total file length once the
/// caller appends the postings blob of `postings_len` bytes.
fn write_head_and_sections(
    file: &mut fs::File,
    magic: &[u8; 8],
    header: &[u8],
    sections: &[Vec<u8>],
    postings_len: u64,
    data_start: &mut u64,
) -> io::Result<()> {
    let mut out = BufWriter::new(file);
    out.write_all(magic)?;
    out.write_all(&(header.len() as u32).to_le_bytes())?;
    out.write_all(header)?;
    for s in sections {
        out.write_all(&data_start.to_le_bytes())?;
        out.write_all(&(s.len() as u64).to_le_bytes())?;
        *data_start += s.len() as u64;
    }
    out.write_all(&data_start.to_le_bytes())?;
    out.write_all(&postings_len.to_le_bytes())?;
    *data_start += postings_len;
    for s in sections {
        out.write_all(s)?;
    }
    out.flush()
}

fn u64_le(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *data.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7F) << shift;
        if b < 0x80 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// A set of file ids. `None` in APIs below means "every file".
pub(crate) struct Bitset(Vec<u64>);

impl Bitset {
    fn zeros(n: usize) -> Bitset {
        Bitset(vec![0; n.div_ceil(64)])
    }
    pub(crate) fn contains(&self, id: usize) -> bool {
        self.0[id / 64] >> (id % 64) & 1 == 1
    }
    pub(crate) fn count(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }
    fn is_empty(&self) -> bool {
        self.0.iter().all(|&w| w == 0)
    }
    fn and(&mut self, other: &Bitset) {
        self.0.iter_mut().zip(&other.0).for_each(|(a, b)| *a &= b);
    }
    fn or(&mut self, other: &Bitset) {
        self.0.iter_mut().zip(&other.0).for_each(|(a, b)| *a |= b);
    }
}

pub(crate) struct Index {
    map: Mmap,
    n: usize,
    m: usize,
    sections: [(usize, usize); SECTIONS],
    pub(crate) header: serde_json::Value,
}

impl Index {
    pub(crate) fn open(path: &Path) -> io::Result<Index> {
        let file = fs::File::open(path)?;
        // SAFETY: index files are written once under a fresh generation name
        // and never modified in place; a reader only races a deletion, which
        // does not invalidate an existing mapping.
        let map = unsafe { Mmap::map(&file)? };
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "invalid wgrep index");
        if map.len() < 12 || &map[..8] != MAGIC {
            return Err(bad());
        }
        let header_len = u32::from_le_bytes(map[8..12].try_into().unwrap()) as usize;
        let header: serde_json::Value =
            serde_json::from_slice(map.get(12..12 + header_len).ok_or_else(bad)?).map_err(|_| bad())?;
        if header.get("version").and_then(|v| v.as_u64()) != Some(VERSION) {
            return Err(bad());
        }
        let mut sections = [(0, 0); SECTIONS];
        let mut pos = 12 + header_len;
        for s in &mut sections {
            let raw = map.get(pos..pos + 16).ok_or_else(bad)?;
            let off = u64::from_le_bytes(raw[..8].try_into().unwrap()) as usize;
            let len = u64::from_le_bytes(raw[8..].try_into().unwrap()) as usize;
            if off.checked_add(len).is_none_or(|end| end > map.len()) {
                return Err(bad());
            }
            *s = (off, len);
            pos += 16;
        }
        let n = sections[Section::Meta as usize].1 / 24;
        let m = sections[Section::Keys as usize].1 / 4;
        let ok = sections[Section::PathOffsets as usize].1 == (n + 1) * 8
            && sections[Section::Meta as usize].1 == n * 24
            && sections[Section::PostingOffsets as usize].1 == (m + 1) * 4;
        if !ok {
            return Err(bad());
        }
        // Path offsets must be non-decreasing and span the path blob exactly,
        // so `path_in` can slice without panicking on a corrupt index.
        let (off, _) = sections[Section::PathOffsets as usize];
        let offsets = &map[off..off + (n + 1) * 8];
        let paths_len = sections[Section::Paths as usize].1;
        let mut prev = 0u64;
        for i in 0..=n {
            let v = u64::from_le_bytes(offsets[i * 8..i * 8 + 8].try_into().unwrap());
            if v < prev || v as usize > paths_len {
                return Err(bad());
            }
            prev = v;
        }
        if prev as usize != paths_len {
            return Err(bad());
        }
        // Keys must be strictly increasing: `posting_index` binary-searches
        // them, and an unsorted table can return a wrong posting (a missing
        // file) rather than "absent".
        let (keys_off, _) = sections[Section::Keys as usize];
        let keys = &map[keys_off..keys_off + m * 4];
        for i in 0..m {
            let k = u32::from_le_bytes(keys[i * 4..i * 4 + 4].try_into().unwrap());
            if i > 0 && k <= u32::from_le_bytes(keys[(i - 1) * 4..(i - 1) * 4 + 4].try_into().unwrap()) {
                return Err(bad());
            }
        }
        // Posting offsets must be non-decreasing and span the postings blob
        // exactly, so `posting_span` cannot underflow or slice out of bounds.
        let (po_off, _) = sections[Section::PostingOffsets as usize];
        let postings_len = sections[Section::Postings as usize].1;
        let po = &map[po_off..po_off + (m + 1) * 4];
        let mut prev_off = 0u32;
        for i in 0..=m {
            let v = u32::from_le_bytes(po[i * 4..i * 4 + 4].try_into().unwrap());
            if v < prev_off || v as usize > postings_len {
                return Err(bad());
            }
            prev_off = v;
        }
        if prev_off as usize != postings_len {
            return Err(bad());
        }
        Ok(Index { map, n, m, sections, header })
    }

    pub(crate) fn file_count(&self) -> usize {
        self.n
    }

    fn section(&self, s: Section) -> &[u8] {
        let (off, len) = self.sections[s as usize];
        &self.map[off..off + len]
    }

    fn u64_at(&self, s: Section, i: usize) -> u64 {
        u64::from_le_bytes(self.section(s)[i * 8..i * 8 + 8].try_into().unwrap())
    }

    fn u32_at(&self, s: Section, i: usize) -> u32 {
        u32::from_le_bytes(self.section(s)[i * 4..i * 4 + 4].try_into().unwrap())
    }

    pub(crate) fn size(&self, i: usize) -> u64 {
        self.u64_at(Section::Meta, 3 * i)
    }

    pub(crate) fn mtime_ns(&self, i: usize) -> i64 {
        self.u64_at(Section::Meta, 3 * i + 1) as i64
    }

    pub(crate) fn change_ns(&self, i: usize) -> i64 {
        self.u64_at(Section::Meta, 3 * i + 2) as i64
    }

    /// The `i`th path, slicing the hoisted `PathOffsets`/`Paths` sections so a
    /// binary search does not re-derive them per comparison.
    fn path_in<'a>(offsets: &'a [u8], paths: &'a [u8], i: usize) -> &'a [u8] {
        let a = u64::from_le_bytes(offsets[i * 8..i * 8 + 8].try_into().unwrap()) as usize;
        let b = u64::from_le_bytes(offsets[i * 8 + 8..i * 8 + 16].try_into().unwrap()) as usize;
        &paths[a..b]
    }

    /// The `i`th path, relative to the index root, `/`-separated.
    pub(crate) fn path(&self, i: usize) -> &[u8] {
        let (offsets, paths) = (self.section(Section::PathOffsets), self.section(Section::Paths));
        Self::path_in(offsets, paths, i)
    }

    /// The id of the file at `rel`, or `None`. Binary search over the whole
    /// path table.
    pub(crate) fn find(&self, rel: &[u8]) -> Option<usize> {
        let (offsets, paths) = (self.section(Section::PathOffsets), self.section(Section::Paths));
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match Self::path_in(offsets, paths, mid).cmp(rel) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Binary search over `[lo, hi)`, normally a directory's cached id range.
    /// A miss here means "not in the index", so a wrong range can only widen
    /// the search, never skip a file.
    pub(crate) fn find_in(&self, lo: usize, hi: usize, rel: &[u8]) -> Option<usize> {
        let (offsets, paths) = (self.section(Section::PathOffsets), self.section(Section::Paths));
        let (mut lo, mut hi) = (lo, hi);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match Self::path_in(offsets, paths, mid).cmp(rel) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The contiguous id range of every file whose index-relative path starts
    /// with `prefix`, which must end in `/` (or be empty). Paths are sorted,
    /// so a directory's children form one range.
    pub(crate) fn dir_range(&self, prefix: &[u8]) -> (usize, usize) {
        if prefix.is_empty() {
            return (0, self.n);
        }
        let lo = self.lower_bound(prefix);
        // All children of `prefix` sort below `prefix` with its trailing '/'
        // bumped to the next byte ('0'), so the first non-child is the end.
        debug_assert_eq!(prefix.last(), Some(&b'/'));
        let mut upper = prefix.to_vec();
        match upper.last_mut() {
            Some(b) if *b != 0xff => *b += 1,
            // Cannot bump past 0xFF (or empty prefix): run to the end, which
            // can only widen the search.
            _ => return (lo, self.n),
        }
        (lo, self.lower_bound(&upper))
    }

    /// First id whose path is `>= rel`.
    fn lower_bound(&self, rel: &[u8]) -> usize {
        let (offsets, paths) = (self.section(Section::PathOffsets), self.section(Section::Paths));
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if Self::path_in(offsets, paths, mid) < rel { lo = mid + 1 } else { hi = mid }
        }
        lo
    }

    /// Files that may match `query`; `None` means every file.
    pub(crate) fn eval(&self, query: &Query) -> Option<Bitset> {
        match query {
            Query::All => None,
            Query::None => Some(Bitset::zeros(self.n)),
            Query::Tri(key) => self.posting(*key, None),
            Query::TriNext(key, next) => self.posting(*key, Some(super::fold::next_bit(*next))),
            Query::And(parts) => {
                // Evaluate the most selective/cheapest term first, so an absent
                // (empty) or tiny term short-circuits the rest and dense
                // postings are never decoded.
                let mut order: Vec<&Query> = parts.iter().collect();
                order.sort_by_key(|q| self.plan_cost(q));
                let mut acc: Option<Bitset> = None;
                for part in order {
                    if let Some(set) = self.eval(part) {
                        match acc.as_mut() {
                            Some(a) => a.and(&set),
                            None => acc = Some(set),
                        }
                        if acc.as_ref().is_some_and(Bitset::is_empty) {
                            break;
                        }
                    }
                }
                acc
            }
            Query::Or(parts) => {
                let mut acc = Bitset::zeros(self.n);
                for part in parts {
                    acc.or(&self.eval(part)?);
                }
                Some(acc)
            }
        }
    }

    /// The index of trigram `key` in the `Keys` table, if present.
    fn posting_index(&self, key: u32) -> Option<usize> {
        let keys = self.section(Section::Keys);
        let (mut lo, mut hi) = (0, self.m);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let k = u32::from_le_bytes(keys[mid * 4..mid * 4 + 4].try_into().unwrap());
            match k.cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn posting_span(&self, i: usize) -> (usize, usize) {
        (
            self.u32_at(Section::PostingOffsets, i) as usize,
            self.u32_at(Section::PostingOffsets, i + 1) as usize,
        )
    }

    /// A lower number means a more selective (cheaper, more likely empty)
    /// term. An absent trigram is 0 (evaluated first, short-circuits); a term
    /// covering every file is `u64::MAX` (evaluated last).
    fn plan_cost(&self, q: &Query) -> u64 {
        match q {
            Query::None => 0,
            Query::Tri(k) | Query::TriNext(k, _) => self
                .posting_index(*k)
                .map_or(0, |i| {
                    let (a, b) = self.posting_span(i);
                    b.saturating_sub(a) as u64
                }),
            Query::And(parts) => parts.iter().map(|p| self.plan_cost(p)).sum(),
            Query::Or(_) | Query::All => u64::MAX,
        }
    }

    /// Files containing trigram `key`; with `bit`, only files whose mask for
    /// it has that bit.
    fn posting(&self, key: u32, bit: Option<u8>) -> Option<Bitset> {
        let mut set = Bitset::zeros(self.n);
        let Some(i) = self.posting_index(key) else { return Some(set) };
        let (start, end) = self.posting_span(i);
        // Any malformed record means "consider every file" (`None`), never a
        // panic: the slice is bounds-checked and every field is validated.
        let record = self.section(Section::Postings).get(start..end)?;
        match record.first() {
            Some(&TAG_LIST) => {
                let mut pos = 1;
                let count = get_varint(record, &mut pos)? as usize;
                // First pass: validate ids and find where the masks begin.
                let ids_start = pos;
                let mut id = 0u64;
                for j in 0..count {
                    let delta = get_varint(record, &mut pos)?;
                    id = if j == 0 { delta } else { id.checked_add(delta)? };
                    if id as usize >= self.n {
                        return None;
                    }
                }
                // Exactly `count` mask bytes, no trailing data.
                if pos + count != record.len() {
                    return None;
                }
                let masks = &record[pos..pos + count];
                // Second pass: set bits, applying the next-byte mask filter.
                let mut p = ids_start;
                let mut id = 0u64;
                for j in 0..count {
                    let delta = get_varint(record, &mut p)?;
                    id = if j == 0 { delta } else { id.checked_add(delta)? };
                    if bit.is_none_or(|b| masks[j] & b != 0) {
                        set.0[id as usize / 64] |= 1 << (id as usize % 64);
                    }
                }
            }
            Some(&TAG_BITMAP) => {
                let bitmap = record.get(1..1 + self.n.div_ceil(8))?;
                let masks = record.get(1 + bitmap.len()..)?;
                let mut j = 0usize;
                for (byte_index, &byte) in bitmap.iter().enumerate() {
                    let mut b = byte;
                    while b != 0 {
                        let id = byte_index * 8 + b.trailing_zeros() as usize;
                        b &= b - 1;
                        if id >= self.n {
                            return None;
                        }
                        let mask = *masks.get(j)?;
                        j += 1;
                        if bit.is_none_or(|bb| mask & bb != 0) {
                            set.0[id / 64] |= 1 << (id % 64);
                        }
                    }
                }
                // The mask count must equal the bitmap's population count, with
                // no trailing data, or the record is inconsistent.
                if j != masks.len() {
                    return None;
                }
            }
            // Dense or unrecognized: consider every file.
            _ => return None,
        }
        Some(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_index(path: &Path) {
        let files = vec![
            FileEntry { rel: "a.txt".into(), size: 3, mtime_ns: 1, change_ns: 0 },
            FileEntry { rel: "b/c.txt".into(), size: 4, mtime_ns: 2, change_ns: 0 },
        ];
        let postings = vec![Ok((0x616263u32, vec![0u32], vec![0u8])), Ok((0x78797au32, vec![1u32], vec![1u8]))];
        let header = serde_json::json!({ "version": VERSION, "files": 2 });
        write(path, &header, &files, postings.into_iter(), 100).unwrap();
    }

    #[test]
    fn open_finds_and_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.wgi");
        write_index(&path);
        let idx = Index::open(&path).unwrap();
        assert_eq!(idx.file_count(), 2);
        assert_eq!(idx.find_in(0, 2, b"b/c.txt"), Some(1));
        assert_eq!(idx.find_in(0, 2, b"missing"), None);
        assert_eq!(idx.dir_range(b"b/"), (1, 2));
    }

    #[test]
    fn corrupt_path_offset_is_rejected_not_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.wgi");
        write_index(&path);
        let mut bytes = fs::read(&path).unwrap();
        let header_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        // Section table entry for PathOffsets (index 1).
        let entry = 12 + header_len + 16;
        let off = u64::from_le_bytes(bytes[entry..entry + 8].try_into().unwrap()) as usize;
        bytes[off..off + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(Index::open(&path).is_err());
    }

    fn section_table(bytes: &[u8]) -> usize {
        let header_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        12 + header_len
    }

    fn u64_le_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    fn u32_le_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn cleared_bitmap_with_leftover_masks_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.wgi");
        let files = vec![
            FileEntry { rel: "a.txt".into(), size: 3, mtime_ns: 1, change_ns: 0 },
            FileEntry { rel: "b.txt".into(), size: 4, mtime_ns: 2, change_ns: 0 },
        ];
        // The third trigram is present in both files, so it is stored as a
        // bitmap (its delta list would be longer than the one-byte bitmap).
        let postings = vec![
            Ok((0x616263u32, vec![0u32], vec![0u8])),
            Ok((0x78797au32, vec![1u32], vec![1u8])),
            Ok((0x7a7a7au32, vec![0u32, 1u32], vec![0b01u8, 0b10u8])),
        ];
        let header = serde_json::json!({ "version": VERSION, "files": 2 });
        write(&path, &header, &files, postings.into_iter(), 100).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        let table = section_table(&bytes);
        let po_off = u64_le_at(&bytes, table + 4 * 16) as usize; // PostingOffsets
        let postings_off = u64_le_at(&bytes, table + 5 * 16) as usize; // Postings
        let start = u32_le_at(&bytes, po_off + 2 * 4) as usize; // 3rd trigram
        let rec = postings_off + start;
        assert_eq!(bytes[rec], TAG_BITMAP, "fixture should exercise the bitmap path");

        // Clear the bitmap but leave the two mask bytes after it, exactly the
        // inconsistent record the review reproduced.
        bytes[rec + 1] = 0;
        fs::write(&path, &bytes).unwrap();

        let idx = Index::open(&path).unwrap();
        // Must reject the record (None = "consider every file"), not accept it
        // as an empty set that would drop the files.
        assert!(idx.eval(&Query::Tri(0x7a7a7a)).is_none());
    }

    #[test]
    fn corrupt_posting_returns_none_not_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.wgi");
        write_index(&path);
        let mut bytes = fs::read(&path).unwrap();
        let header_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        // Section table entry for Postings (index 5).
        let entry = 12 + header_len + 5 * 16;
        let off = u64::from_le_bytes(bytes[entry..entry + 8].try_into().unwrap()) as usize;
        // An unrecognized record tag must mean "consider every file" (None),
        // not an empty set that would drop the files.
        bytes[off] = 0x7f;
        fs::write(&path, &bytes).unwrap();
        let idx = Index::open(&path).unwrap();
        assert!(idx.eval(&Query::Tri(0x616263)).is_none());
    }

    #[test]
    fn out_of_range_posting_offset_is_rejected_not_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.wgi");
        write_index(&path);
        let mut bytes = fs::read(&path).unwrap();
        let header_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        // Section table entry for PostingOffsets (index 4): make the second
        // offset larger than the postings blob.
        let entry = 12 + header_len + 4 * 16;
        let off = u64::from_le_bytes(bytes[entry..entry + 8].try_into().unwrap()) as usize;
        bytes[off + 4..off + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        // Rejected at open, rather than underflowing a span later.
        assert!(Index::open(&path).is_err());
    }
}
