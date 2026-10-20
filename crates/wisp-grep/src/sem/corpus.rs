//! Resident corpus: every chunk is rankable, with incremental updates.
//!
//! The corpus is the whole discovered file set — ranking is never
//! constrained to literal trigram hits, so semantic-only candidates are
//! always possible. Each chunk carries its embedding (256 f32), its lexical
//! term counts, and its file's freshness fingerprint.
//!
//! Concurrency discipline (review blockers 6–7):
//! - expensive work (read + chunk + tokenize + embed) happens OFF the
//!   corpus lock in `prepare_path`; `publish` takes the write lock only to
//!   swap row sets atomically. Readers are never blocked by embedding;
//! - publish re-fingerprints and refuses to install rows for a file that
//!   changed mid-prepare (returns `false`; the next reconcile retries), so
//!   a racing write can delay an update but never corrupt one;
//! - embed failures never delete: publish installs new rows only after a
//!   fully successful prepare; on any error the old rows stay;
//! - vocabulary lookup is a hash map, not a scan;
//! - full builds embed in bounded batches (no whole-corpus `texts` + clone
//!   + `Vec<Vec<f32>>` duplication).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::chunks::{self, Chunk, Fingerprint};
use super::lex;
use super::model::ModelCtx;

/// Bounded embed batch: caps transient `texts` + output memory.
const EMBED_BATCH: usize = 512;

pub(crate) struct FileRec {
    pub abs: PathBuf,
    pub fp: Fingerprint,
    /// Chunk indices into `Corpus::chunks`, in file order.
    pub chunk_ids: Vec<usize>,
    pub skipped_large: bool,
}

pub(crate) struct Corpus {
    pub root: PathBuf,
    pub chunks: Vec<Chunk>,
    /// Flat normalized embeddings, `chunks.len() * DIM` f32.
    pub vecs: Vec<f32>,
    /// Per-chunk lexical counts as (vocab_id, count) pairs, id-sorted.
    pub counts: Vec<Vec<(u32, u32)>>,
    pub vocab_terms: Vec<String>,
    pub vocab: HashMap<String, u32>,
    pub files: HashMap<PathBuf, FileRec>,
    pub skipped_large: usize,
    pub skipped_binary: usize,
}

/// Split flat batch vectors back per file: file `i` owns `counts[i]`
/// vectors in order. Pure pairing logic — a skipped file still consumes
/// its slice (possibly empty), so later files can never be assigned the
/// wrong embeddings. Regression-tested below.
fn split_batch_vecs<'a>(vecs: &'a [Vec<f32>], counts: &[usize]) -> Vec<&'a [Vec<f32>]> {
    let mut out = Vec::with_capacity(counts.len());
    let mut cur = 0usize;
    for &n in counts {
        out.push(&vecs[cur..cur + n]);
        cur += n;
    }
    out
}

/// A fully computed per-file update, built without holding any lock.
pub(crate) struct Prepared {
    pub abs: PathBuf,
    pub fp: Fingerprint,
    pub chunks: Vec<Chunk>,
    /// One normalized vector per chunk, same order.
    pub vecs: Vec<Vec<f32>>,
    /// One (term, count) list per chunk, same order. Terms are unresolved
    /// strings; `publish` interns them under the write lock.
    pub term_counts: Vec<Vec<(String, u32)>>,
    /// Set when the file is present but unrankable (too large / binary).
    pub skipped_large: bool,
    /// True when the file is gone (drop rows) — chunks/vecs are empty.
    pub gone: bool,
}

impl Corpus {
    pub(crate) fn empty(root: PathBuf) -> Corpus {
        Corpus {
            root,
            chunks: Vec::new(),
            vecs: Vec::new(),
            counts: Vec::new(),
            vocab_terms: Vec::new(),
            vocab: HashMap::new(),
            files: HashMap::new(),
            skipped_large: 0,
            skipped_binary: 0,
        }
    }

    fn intern(&mut self, term: &str) -> u32 {
        if let Some(&id) = self.vocab.get(term) {
            return id;
        }
        let id = self.vocab_terms.len() as u32;
        self.vocab_terms.push(term.to_owned());
        self.vocab.insert(term.to_owned(), id);
        id
    }

    /// Publish a prepared update atomically. Re-fingerprints first: if the
    /// file changed between prepare and publish, installs nothing and
    /// returns `false` (caller retries / next reconcile heals). Never
    /// deletes old rows on error — install happens only after a fully
    /// successful prepare.
    pub(crate) fn publish(&mut self, prep: Prepared) -> bool {
        if !prep.gone {
            let current = chunks::fingerprint(&prep.abs);
            if current != Some(prep.fp) {
                return false;
            }
        } else if prep.abs.exists() {
            return false;
        }
        self.remove_file(&prep.abs);
        if prep.gone {
            return true;
        }
        if prep.chunks.is_empty() {
            self.files.insert(prep.abs.clone(), FileRec {
                abs: prep.abs,
                fp: prep.fp,
                chunk_ids: Vec::new(),
                skipped_large: prep.skipped_large,
            });
            if prep.skipped_large {
                self.skipped_large += 1;
            } else {
                self.skipped_binary += 1;
            }
            return true;
        }
        let mut ids = Vec::with_capacity(prep.chunks.len());
        for ((c, v), tc) in prep.chunks.into_iter().zip(prep.vecs).zip(prep.term_counts) {
            let mut counts: Vec<(u32, u32)> =
                tc.into_iter().map(|(t, k)| (self.intern(&t), k)).collect();
            counts.sort_unstable();
            ids.push(self.chunks.len());
            self.chunks.push(c);
            self.counts.push(counts);
            self.vecs.extend_from_slice(&v);
        }
        self.files.insert(prep.abs.clone(), FileRec {
            abs: prep.abs,
            fp: prep.fp,
            chunk_ids: ids,
            skipped_large: false,
        });
        true
    }

    fn remove_file(&mut self, abs: &Path) {
        let Some(rec) = self.files.remove(abs) else { return };
        if rec.skipped_large {
            self.skipped_large = self.skipped_large.saturating_sub(1);
        } else if rec.chunk_ids.is_empty() {
            self.skipped_binary = self.skipped_binary.saturating_sub(1);
        }
        // Back-to-front swap-removal keeps earlier indices valid; each step
        // patches the moved tail chunk's owner record.
        let mut ids = rec.chunk_ids;
        ids.sort_unstable_by(|a, b| b.cmp(a));
        for id in ids {
            use super::DIM;
            let last = self.chunks.len() - 1;
            self.chunks.swap_remove(id);
            self.counts.swap_remove(id);
            if id != last {
                self.vecs.copy_within(last * DIM..(last + 1) * DIM, id * DIM);
            }
            self.vecs.truncate(self.vecs.len() - DIM);
            if id != last {
                for f in self.files.values_mut() {
                    if let Some(slot) = f.chunk_ids.iter_mut().find(|x| **x == last) {
                        *slot = id;
                        break;
                    }
                }
            }
        }
    }

    /// Drop everything under `dir` (a removed directory). Exact-file
    /// removal also routes here: the file path is its own prefix.
    pub(crate) fn remove_prefix(&mut self, dir: &Path) {
        let victims: Vec<PathBuf> =
            self.files.keys().filter(|p| p.starts_with(dir)).cloned().collect();
        for v in victims {
            self.remove_file(&v);
        }
    }

    /// Full (re)build: parallel chunking, then bounded-batch embedding
    /// straight into the fresh corpus. Replaces `self` only on success.
    pub(crate) fn build(&mut self, model: &ModelCtx, mode: chunks::Chunking) -> anyhow::Result<usize> {
        let files = chunks::list_files(&self.root);
        let root = self.root.clone();
        let per_thread = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).max(1);
        let batches: Vec<Vec<PathBuf>> = files
            .chunks(files.len().div_ceil(per_thread).max(1))
            .map(|c| c.to_vec())
            .collect();
        // Phase 1 (parallel, no locks): read + chunk every file.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            for batch in batches {
                let tx = tx.clone();
                let root = &root;
                s.spawn(move || {
                    let mut out = Vec::new();
                    for abs in batch {
                        if let Some((fp, fc)) = chunks::chunk_file_mode(root, &abs, mode) {
                            out.push((abs, fp, fc));
                        }
                    }
                    let _ = tx.send(out);
                });
            }
        });
        drop(tx);
        let mut ordered: Vec<(PathBuf, Fingerprint, Vec<Chunk>)> = Vec::new();
        for batch in rx {
            ordered.extend(batch);
        }
        ordered.sort_by(|a, b| a.0.cmp(&b.0));

        // Phase 2 (sequential, bounded batches): embed + intern directly
        // into the fresh corpus. Peak transient memory is one batch.
        // Correctness rule: every embedded vector is CONSUMED exactly once
        // in order, independent of publish. A file that raced away mid-build
        // still advances the cursor past its own vectors — otherwise every
        // later file in the batch would be assigned the wrong embedding.
        let mut fresh = Corpus::empty(self.root.clone());
        let mut batch: Vec<(PathBuf, Fingerprint, Vec<Chunk>)> = Vec::new();
        let mut pending: usize = 0;
        let flush = |fresh: &mut Corpus, batch: &mut Vec<(PathBuf, Fingerprint, Vec<Chunk>)>| -> anyhow::Result<()> {
            if batch.is_empty() {
                return Ok(());
            }
            let texts: Vec<String> = batch
                .iter()
                .flat_map(|(_, _, fc)| fc.iter().map(|c| format!("{}\n{}", c.path, c.content)))
                .collect();
            let vecs = model.embed(&texts)?;
            // Pair vectors back to files positionally: file i owns the
            // next counts[i] vectors, consumed even when the file is
            // skipped below (see split_batch_vecs).
            let counts: Vec<usize> = batch.iter().map(|(_, _, fc)| fc.len()).collect();
            let per_file = split_batch_vecs(&vecs, &counts);
            for ((abs, fp, fchunks), fvecs) in batch.drain(..).zip(per_file) {
                let n = fchunks.len();
                // Files that raced away mid-build re-verify here: their
                // vectors are dropped, but the pairing above already
                // consumed them, so later files keep the right vectors.
                if chunks::fingerprint(&abs) != Some(fp) {
                    continue;
                }
                let mut ids = Vec::with_capacity(n);
                for (k, c) in fchunks.into_iter().enumerate() {
                    let counts = count_terms(&c.path, &c.content, &mut fresh.vocab, &mut fresh.vocab_terms);
                    ids.push(fresh.chunks.len());
                    fresh.chunks.push(c);
                    fresh.counts.push(counts);
                    fresh.vecs.extend_from_slice(&fvecs[k]);
                }
                fresh.files.insert(abs.clone(), FileRec { abs, fp, chunk_ids: ids, skipped_large: false });
            }
            Ok(())
        };
        for (abs, fp, fchunks) in ordered {
            if fchunks.is_empty() {
                let large = fp.len > chunks::MAX_FILE_BYTES;
                fresh.files.insert(abs.clone(), FileRec {
                    abs,
                    fp,
                    chunk_ids: Vec::new(),
                    skipped_large: large,
                });
                if large {
                    fresh.skipped_large += 1;
                } else {
                    fresh.skipped_binary += 1;
                }
                continue;
            }
            pending += fchunks.len();
            batch.push((abs, fp, fchunks));
            if pending >= EMBED_BATCH {
                flush(&mut fresh, &mut batch)?;
                pending = 0;
            }
        }
        flush(&mut fresh, &mut batch)?;
        let n = fresh.chunks.len();
        *self = fresh;
        Ok(n)
    }

    pub(crate) fn file_count(&self) -> usize {
        self.files.len()
    }

    pub(crate) fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

/// Count lexical terms over `path + content` with an explicit vocab map.
/// Free function so both build and prepare share it without a lock.
fn count_terms(
    path: &str,
    content: &str,
    vocab: &mut HashMap<String, u32>,
    terms: &mut Vec<String>,
) -> Vec<(u32, u32)> {
    let mut map: HashMap<u32, u32> = HashMap::new();
    for w in lex::words(content).into_iter().chain(lex::words(path)) {
        let id = if let Some(&id) = vocab.get(&w) {
            id
        } else {
            let id = terms.len() as u32;
            terms.push(w.clone());
            vocab.insert(w, id);
            id
        };
        *map.entry(id).or_insert(0) += 1;
    }
    let mut v: Vec<(u32, u32)> = map.into_iter().collect();
    v.sort_unstable();
    v
}

/// String-form term counts for the lock-free prepare path (interned at
/// publish time, when the final vocab state is known).
fn count_terms_unresolved(path: &str, content: &str) -> Vec<(String, u32)> {
    let mut map: HashMap<String, u32> = HashMap::new();
    for w in lex::words(content).into_iter().chain(lex::words(path)) {
        *map.entry(w).or_insert(0) += 1;
    }
    let mut v: Vec<(String, u32)> = map.into_iter().collect();
    v.sort_unstable();
    v
}

/// Lock-free prepare: read + chunk + embed one file. Returns `None` only
/// when the file vanished mid-prepare (caller treats as drop-candidate).
/// Embed failure is an error and publishes nothing — old rows stay.
pub(crate) fn prepare_path(
    model: &ModelCtx,
    root: &Path,
    abs: &Path,
    mode: chunks::Chunking,
) -> anyhow::Result<Option<Prepared>> {
    let Some((fp, fchunks)) = chunks::chunk_file_mode(root, abs, mode) else {
        // Gone (or unreadable): signal removal. The publisher re-checks
        // existence so a racing create is not deleted.
        if abs.exists() {
            return Ok(None);
        }
        return Ok(Some(Prepared {
            abs: abs.to_path_buf(),
            fp: Fingerprint { len: 0, mtime_ns: 0, token: None },
            chunks: Vec::new(),
            vecs: Vec::new(),
            term_counts: Vec::new(),
            skipped_large: false,
            gone: true,
        }));
    };
    if fchunks.is_empty() {
        return Ok(Some(Prepared {
            abs: abs.to_path_buf(),
            fp,
            chunks: Vec::new(),
            vecs: Vec::new(),
            term_counts: Vec::new(),
            skipped_large: fp.len > chunks::MAX_FILE_BYTES,
            gone: false,
        }));
    }
    let texts: Vec<String> =
        fchunks.iter().map(|c| format!("{}\n{}", c.path, c.content)).collect();
    let vecs = model.embed(&texts)?;
    let term_counts: Vec<Vec<(String, u32)>> =
        fchunks.iter().map(|c| count_terms_unresolved(&c.path, &c.content)).collect();
    Ok(Some(Prepared {
        abs: abs.to_path_buf(),
        fp,
        chunks: fchunks,
        vecs,
        term_counts,
        skipped_large: false,
        gone: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocab_is_hash_not_scan() {
        let mut c = Corpus::empty(PathBuf::from("."));
        for i in 0..5000 {
            let t = format!("term{i:05}");
            c.intern(&t);
        }
        assert_eq!(c.intern("term00042"), 42);
        assert_eq!(c.vocab_terms.len(), 5000);
    }

    #[test]
    fn batch_vector_pairing_survives_skipped_files() {
        // Regression: a file skipped at publish (raced away mid-build)
        // must still consume its own vectors, or every later file in the
        // batch is assigned the wrong embedding.
        let v = |x: f32| vec![x; super::super::DIM];
        let vecs = vec![v(1.0), v(2.0), v(3.0), v(4.0)];
        let counts = [2usize, 0, 2];
        let split = split_batch_vecs(&vecs, &counts);
        assert_eq!(split.len(), 3);
        assert_eq!(split[0].len(), 2);
        assert!(split[1].is_empty());
        assert_eq!(split[2].len(), 2);
        // The third file's vectors are its own (3.0, 4.0), not shifted.
        assert_eq!(split[2][0][0], 3.0);
        assert_eq!(split[2][1][0], 4.0);
        // First file keeps (1.0, 2.0).
        assert_eq!(split[0][0][0], 1.0);
        assert_eq!(split[0][1][0], 2.0);
        // Counts must exactly cover the flat list (flush invariant).
        let total: usize = counts.iter().sum();
        assert_eq!(total, vecs.len());
    }

    #[test]
    fn publish_rejects_raced_prepare() {
        // A prepare whose fingerprint no longer matches installs nothing.
        let dir = std::env::temp_dir().join("wsem-publish-test");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("a.txt");
        std::fs::write(&f, "hello world\n").unwrap();
        let fp1 = chunks::fingerprint(&f).unwrap();
        std::fs::write(&f, "hello world changed\n").unwrap();
        let mut c = Corpus::empty(dir.clone());
        let prep = Prepared {
            abs: f.clone(),
            fp: fp1,
            chunks: vec![chunks::Chunk {
                path: "a.txt".into(),
                start: 1,
                end: 1,
                content: "hello world\n".into(),
            }],
            vecs: vec![vec![0.0f32; super::super::DIM]],
            term_counts: vec![vec![("hello".to_owned(), 1)]],
            skipped_large: false,
            gone: false,
        };
        assert!(!c.publish(prep));
        assert!(c.chunks.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_prefix_drops_only_victims() {
        // Pure index surgery: dropping one file must leave the survivor's
        // rows, vectors, and chunk ids consistent (swap-remove patching).
        let root = PathBuf::from("/root");
        let mut c = Corpus::empty(root.clone());
        let fp = Fingerprint { len: 1, mtime_ns: 1, token: None };
        let mut next_id = 0usize;
        for (name, n) in [("a.txt", 2), ("b.txt", 1)] {
            let mut ids = Vec::new();
            for k in 0..n {
                c.chunks.push(Chunk {
                    path: name.to_owned(),
                    start: k + 1,
                    end: k + 1,
                    content: format!("{name} {k}\n"),
                });
                c.counts.push(vec![(k as u32, 1)]);
                c.vecs.extend(std::iter::repeat_n(next_id as f32, super::super::DIM));
                ids.push(next_id);
                next_id += 1;
            }
            let abs = root.join(name);
            c.files.insert(abs.clone(), FileRec { abs, fp, chunk_ids: ids, skipped_large: false });
        }
        assert_eq!(c.chunk_count(), 3);
        c.remove_prefix(&root.join("a.txt"));
        assert_eq!(c.chunk_count(), 1);
        assert_eq!(c.vecs.len(), super::super::DIM);
        // Survivor kept its content with re-based chunk id.
        assert_eq!(c.chunks[0].path, "b.txt");
        let b = c.files.get(&root.join("b.txt")).unwrap();
        assert_eq!(b.chunk_ids, vec![0]);
        assert_eq!(c.counts[0], vec![(0u32, 1u32)]);
        // Survivor's vector is its own (marker 2.0), not a's leftovers.
        assert!(c.vecs.iter().all(|&x| x == 2.0));
        assert!(!c.files.contains_key(&root.join("a.txt")));
    }
}
