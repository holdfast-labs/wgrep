//! The one case fold shared by the index builder and the query planner.
//!
//! It lowercases ASCII and maps the only two non-ASCII code points whose
//! Unicode simple case fold is ASCII: U+017F LATIN SMALL LETTER LONG S to `s`
//! and U+212A KELVIN SIGN to `k`. Every other byte passes through unchanged,
//! so non-ASCII case variants stay distinct in the index; the planner parses
//! case-insensitive patterns with case folding on, which makes regex-syntax
//! enumerate those variants as classes.

use std::cell::RefCell;

/// Bytes that occur in the two folded multi-byte sequences. A pattern that
/// contains one of these outside valid UTF-8 could match part of a sequence
/// that the fold rewrites, so the planner treats such literals as unknown.
const SEQUENCE_BYTES: [u8; 5] = [0xC5, 0xBF, 0xE2, 0x84, 0xAA];

pub(crate) fn fold_into(src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(src.len());
    let mut i = 0;
    while i < src.len() {
        let b = src[i];
        if b < 0x80 {
            // ASCII run: lowercase in place without testing the multi-byte
            // fold cases per byte.
            let start = i;
            i += 1;
            while i < src.len() && src[i] < 0x80 {
                i += 1;
            }
            out.extend(src[start..i].iter().map(u8::to_ascii_lowercase));
        } else if b == 0xC5 && src.get(i + 1) == Some(&0xBF) {
            out.push(b's');
            i += 2;
        } else if b == 0xE2 && src.get(i + 1) == Some(&0x84) && src.get(i + 2) == Some(&0xAA) {
            out.push(b'k');
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
}

pub(crate) fn fold(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    fold_into(src, &mut out);
    out
}

/// True when folding `bytes` on its own gives the same result as folding it
/// in the middle of any surrounding text.
pub(crate) fn fold_is_context_free(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok() || !bytes.iter().any(|b| SEQUENCE_BYTES.contains(b))
}

pub(crate) fn key(a: u8, b: u8, c: u8) -> u32 {
    (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c)
}

/// The bit that records `next` in a trigram's next-byte mask. The multiply
/// spreads nearby letters across the eight bits.
pub(crate) fn next_bit(next: u8) -> u8 {
    1 << ((next.wrapping_mul(0x9D) >> 5) & 7)
}

/// Reusable per-thread tables for `trigram_masks`, indexed by the 24-bit
/// trigram key, so a file's working memory grows with its distinct trigrams
/// rather than its length. About 18 MB; allocate one per thread.
pub(crate) struct MaskScratch {
    masks: Vec<u8>,
    seen: Vec<u64>,
    touched: Vec<u32>,
}

impl MaskScratch {
    pub(crate) fn new() -> MaskScratch {
        MaskScratch { masks: vec![0; 1 << 24], seen: vec![0; (1 << 24) / 64], touched: Vec::new() }
    }
}

/// Files at most this long use the sort-based path, which needs no scratch
/// table; longer files use the 16 MB `MaskScratch` so their memory stays
/// proportional to their distinct trigrams, not their length.
const SMALL_FILE: usize = 1 << 16;

/// Folded trigram keys of `folded`, sorted and deduplicated, each with a mask
/// of the bytes that follow it anywhere in the text. A trigram at the very
/// end contributes no bit. The scratch is allocated lazily, so a tree of small
/// files never pays the 16 MB table.
pub(crate) fn trigram_masks(folded: &[u8], scratch: &mut Option<MaskScratch>, out: &mut Vec<(u32, u8)>) {
    if folded.len() <= SMALL_FILE {
        trigram_masks_small(folded, out);
    } else {
        let scratch = scratch.get_or_insert_with(MaskScratch::new);
        trigram_masks_table(folded, scratch, out);
    }
}

thread_local! {
    /// Ping-pong buffers for the small-file radix sort, reused per thread so a
    /// burst of small files does not allocate two vectors each time.
    static RADIX: RefCell<RadixBuffers> =
        const { RefCell::new(RadixBuffers { a: Vec::new(), b: Vec::new() }) };
}

struct RadixBuffers {
    a: Vec<u32>,
    b: Vec<u32>,
}

/// Below this many trigrams a comparison sort beats the fixed radix overhead.
const RADIX_MIN: usize = 64;

/// Path for small files: collect one `(key, mask-bit)` per position, order by
/// key, and merge equal keys. Above `RADIX_MIN` the ordering is a linear radix
/// sort over the 24-bit keys, which avoids the comparison sort's
/// `O(n log n)`; tiny inputs keep the simpler sort.
fn trigram_masks_small(folded: &[u8], out: &mut Vec<(u32, u8)>) {
    let n = folded.len().saturating_sub(2);
    out.clear();
    if n == 0 {
        return;
    }
    if n < RADIX_MIN {
        out.reserve(n);
        for i in 0..n {
            let k = key(folded[i], folded[i + 1], folded[i + 2]);
            let m = folded.get(i + 3).map_or(0, |&b| next_bit(b));
            out.push((k, m));
        }
        out.sort_unstable();
        merge_masks(out);
        return;
    }
    RADIX.with(|cell| {
        let mut r = cell.borrow_mut();
        let RadixBuffers { a, b } = &mut *r;
        a.clear();
        a.reserve(n);
        for i in 0..n {
            let k = key(folded[i], folded[i + 1], folded[i + 2]);
            let m = folded.get(i + 3).map_or(0, |&b| next_bit(b));
            // Pack key and mask so the radix sort orders by key alone; the
            // mask rides in the low byte.
            a.push((k << 8) | u32::from(m));
        }
        b.clear();
        b.resize(n, 0);
        radix_sort_keys(a, b);
        out.reserve(n);
        let mut i = 0;
        while i < n {
            let k = a[i] >> 8;
            let mut m = (a[i] & 0xff) as u8;
            i += 1;
            while i < n && (a[i] >> 8) == k {
                m |= (a[i] & 0xff) as u8;
                i += 1;
            }
            out.push((k, m));
        }
    });
}

/// In-place (ping-pong) LSD radix sort of `a` by all 32 bits, so packed
/// `(key << 8) | mask` values come out ordered by key. Leaves the sorted
/// result in `a` and clobbers `b` as scratch.
fn radix_sort_keys(a: &mut Vec<u32>, b: &mut Vec<u32>) {
    // Only the key (bits 8..31) needs ordering; the mask in the low byte is
    // OR-folded afterwards, so its byte pass is unnecessary.
    for shift in [8u32, 16, 24] {
        let mut count = [0u32; 256];
        for &v in a.iter() {
            count[((v >> shift) & 0xff) as usize] += 1;
        }
        let mut sum = 0u32;
        for c in count.iter_mut() {
            let t = *c;
            *c = sum;
            sum += t;
        }
        for &v in a.iter() {
            let idx = ((v >> shift) & 0xff) as usize;
            b[count[idx] as usize] = v;
            count[idx] += 1;
        }
        std::mem::swap(a, b);
    }
}

/// Collapse a key-sorted `out` to one entry per key, OR-ing the masks.
fn merge_masks(out: &mut Vec<(u32, u8)>) {
    let mut w = 0;
    let mut r = 0;
    while r < out.len() {
        let k = out[r].0;
        let mut m = out[r].1;
        r += 1;
        while r < out.len() && out[r].0 == k {
            m |= out[r].1;
            r += 1;
        }
        out[w] = (k, m);
        w += 1;
    }
    out.truncate(w);
}

/// Table path for large files. The reusable per-thread tables are indexed by
/// the 24-bit trigram key, so a file's working memory grows with its distinct
/// trigrams rather than its length.
fn trigram_masks_table(folded: &[u8], scratch: &mut MaskScratch, out: &mut Vec<(u32, u8)>) {
    out.clear();
    for (i, w) in folded.windows(3).enumerate() {
        let k = key(w[0], w[1], w[2]) as usize;
        if scratch.seen[k / 64] >> (k % 64) & 1 == 0 {
            scratch.seen[k / 64] |= 1 << (k % 64);
            scratch.touched.push(k as u32);
        }
        scratch.masks[k] |= folded.get(i + 3).map_or(0, |&b| next_bit(b));
    }
    scratch.touched.sort_unstable();
    for k in scratch.touched.drain(..) {
        let k_usize = k as usize;
        out.push((k, scratch.masks[k_usize]));
        // Leave the tables clean for the next file.
        scratch.masks[k_usize] = 0;
        scratch.seen[k_usize / 64] &= !(1 << (k_usize % 64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_ascii_and_the_two_ascii_simple_folds() {
        assert_eq!(fold("KſK é É".as_bytes()), "ksk é É".as_bytes());
    }

    #[test]
    fn masks_record_every_following_byte() {
        let mut out = Vec::new();
        let mut scratch = None;
        trigram_masks(b"abcdabce", &mut scratch, &mut out);
        // A second run must produce the same result (small path has no shared
        // state to leak; the table path cleans itself).
        let mut again = Vec::new();
        trigram_masks(b"abcdabce", &mut scratch, &mut again);
        assert_eq!(out, again);
        let abc = out.iter().find(|(k, _)| *k == key(b'a', b'b', b'c')).unwrap().1;
        assert_eq!(abc, next_bit(b'd') | next_bit(b'e'));
        let bce = out.iter().find(|(k, _)| *k == key(b'b', b'c', b'e')).unwrap().1;
        assert_eq!(bce, 0);
    }

    #[test]
    fn small_and_table_paths_agree() {
        // Deterministic bytes spanning many trigrams, longer than SMALL_FILE so
        // it takes the table path; the small path is then run on the same bytes
        // by pretending it is small.
        let mut data = Vec::with_capacity(SMALL_FILE + 1000);
        let mut x = 0x243f_6a88u32;
        while data.len() < SMALL_FILE + 1000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            data.push((x >> 16) as u8);
        }
        let mut table = Vec::new();
        let mut scratch = None;
        trigram_masks(&data, &mut scratch, &mut table);
        assert!(scratch.is_some(), "long input should allocate the table");
        let mut small = Vec::new();
        trigram_masks_small(&data, &mut small);
        assert_eq!(table, small);
        assert!(!table.is_empty());
        // Sorted and deduplicated.
        assert!(table.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn raw_sequence_bytes_are_not_context_free() {
        assert!(fold_is_context_free("ſ".as_bytes()));
        assert!(fold_is_context_free(b"\xFF\x01"));
        assert!(!fold_is_context_free(b"\xC5"));
    }
}
