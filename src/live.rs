//! Mutable, atomically published sgrep-compatible corpus snapshots.
use crate::{
    CODE, Chunk, LoadedModel, ModelChoice, Result, TEXT, cache, identifier_pattern, is_prose,
    make_chunk, rank_impl, words,
};
use memmap2::Mmap;
use rayon::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// Snapshot cache identity and bounds. Bump `SNAPSHOT_VERSION` when the chunk
/// ranges or vector layout change, not merely when unrelated ranking changes.
const SNAPSHOT_MAGIC: &[u8; 8] = b"WGSNAP\0\0";
/// Version 3 stores chunk text and then the passage vectors, after all per-file
/// metadata, so a snapshot can be memory-mapped and served without copying
/// either to the heap. Version 2 files are rejected once and rebuilt.
const SNAPSHOT_VERSION: u32 = 3;
/// Fold heap vectors back into a mapped snapshot after this many of them.
const VECTOR_FOLD_HEAP: usize = 65_536;
/// An upper bound on cached chunk rows. A cache above it is ignored; a
/// snapshot above it is not written. Four million rows is 4 GiB of vectors,
/// far above the 13k-file working set and small enough to bound a bad header.
const SNAPSHOT_MAX_CHUNKS: usize = 4_000_000;
const SNAPSHOT_MAX_FILES: usize = 1_000_000;
const SNAPSHOT_MAX_PATH: usize = 64 * 1024;
const SNAPSHOT_MAX_META: usize = 1024 * 1024;
/// Rewrite the cache for a single-file batch only when that many files change
/// at once. This avoids a synchronous 65 MB write on every editor save.
const SNAPSHOT_REWRITE_FILES: usize = 16;
/// Distinguishes concurrent or retried temporary snapshot files. A failed
/// vector append can leave a mapped temp file behind on Windows; a later fold
/// must never truncate that file.
static SNAPSHOT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Return freed worker-arena pages to the OS after a large build.
///
/// Chunking and interning release many short-lived allocations on rayon worker
/// threads. glibc keeps those pages in its arenas, so a resident daemon would
/// otherwise report them as RSS forever. This changes no live data and is a
/// no-op off glibc.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_allocator() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_allocator() {}

/// Phase timing for index builds. Off by default; `WG_LIVE_TIMING=1` prints
/// one line per phase to stderr so a slow repository can be profiled without a
/// special build.
fn mark(label: &str, since: &mut Instant) {
    if std::env::var_os("WG_LIVE_TIMING").is_some() {
        eprintln!("wg live: {label}={}ms", since.elapsed().as_millis());
    }
    *since = Instant::now();
}

pub struct ModelPool {
    cache: std::result::Result<cache::Cache, String>,
    loaded: Mutex<HashMap<&'static str, Arc<LoadedModel>>>,
}

impl ModelPool {
    pub fn new(cache_dir: Option<PathBuf>) -> ModelPool {
        // Cache setup is fallible (tree-sitter configuration); defer its error
        // until a model is requested, keeping the frozen constructor infallible.
        let cache = cache::Cache::new(cache_dir.as_deref(), false).map_err(|e| e.to_string());
        ModelPool {
            cache,
            loaded: Mutex::new(HashMap::new()),
        }
    }

    pub fn loaded_count(&self) -> usize {
        self.loaded.lock().unwrap().len()
    }

    fn get(&self, model: &'static crate::Model) -> Result<Arc<LoadedModel>> {
        let mut loaded = self.loaded.lock().map_err(|_| "model pool poisoned")?;
        if let Some(value) = loaded.get(model.revision) {
            return Ok(value.clone());
        }
        let value = Arc::new(LoadedModel::load_meta(
            model,
            self.cache.as_ref().map_err(|e| e.as_str())?,
        )?);
        loaded.insert(model.revision, value.clone());
        Ok(value)
    }
}

/// Compact per-chunk term counts for one file.
///
/// `terms` is chunk-major: chunk `i` owns `terms[offsets[i]..offsets[i+1]]`,
/// sorted by term id. `lengths[i]` is the exact word count that BM25 used to
/// compute from `counts.values().sum()`.
#[derive(Clone, Default)]
struct FileLexical {
    terms: Vec<(u32, u32)>,
    offsets: Vec<u32>,
    lengths: Vec<u32>,
}

impl FileLexical {
    fn chunk(&self, i: usize) -> (&[(u32, u32)], u32) {
        let start = self.offsets[i] as usize;
        let end = self.offsets[i + 1] as usize;
        (&self.terms[start..end], self.lengths[i])
    }
}

/// Per-chunk input to the term interner. A rebuilt chunk carries strings; a
/// chunk reused from the previous snapshot carries ids that are still valid.
enum RawChunk {
    Raw { terms: Vec<(String, u32)>, length: u32 },
    Known { terms: Vec<(u32, u32)>, length: u32 },
}

/// Maps term strings to stable per-snapshot ids. Ids are never removed, so a
/// carried-forward dictionary stays valid for every unchanged `FileData`.
struct TermInterner {
    lookup: HashMap<Arc<str>, u32>,
}

impl TermInterner {
    fn new() -> Self {
        Self {
            lookup: HashMap::new(),
        }
    }

    fn from_lookup(lookup: &HashMap<Arc<str>, u32>) -> Self {
        Self {
            lookup: lookup.clone(),
        }
    }

    fn intern(&mut self, term: &str) -> u32 {
        if let Some(&id) = self.lookup.get(term) {
            return id;
        }
        let id = self.lookup.len() as u32;
        self.lookup.insert(Arc::from(term), id);
        id
    }

    fn intern_file(&mut self, raw: Vec<RawChunk>) -> FileLexical {
        let mut lexical = FileLexical {
            terms: Vec::new(),
            offsets: Vec::with_capacity(raw.len() + 1),
            lengths: Vec::with_capacity(raw.len()),
        };
        lexical.offsets.push(0);
        for chunk in raw {
            let (mut pairs, length) = match chunk {
                RawChunk::Raw { terms, length } => (
                    terms
                        .into_iter()
                        .map(|(term, count)| (self.intern(&term), count))
                        .collect::<Vec<_>>(),
                    length,
                ),
                RawChunk::Known { terms, length } => (terms, length),
            };
            pairs.sort_unstable_by_key(|pair| pair.0);
            let mut merged: Vec<(u32, u32)> = Vec::with_capacity(pairs.len());
            for (term, count) in pairs {
                if let Some(last) = merged.last_mut()
                    && last.0 == term
                {
                    last.1 += count;
                    continue;
                }
                merged.push((term, count));
            }
            lexical.lengths.push(length);
            lexical.terms.extend_from_slice(&merged);
            lexical.offsets.push(lexical.terms.len() as u32);
        }
        lexical
    }
}

/// Snapshot-wide BM25 state: one term dictionary, a document-frequency table,
/// and an inverted posting list per term.
///
/// Posting inner buffers are `Arc`-shared so an edit copies only the terms that
/// the edited file actually mentions. A posting is `(chunk id, term count)`.
struct LexicalIndex {
    lookup: HashMap<Arc<str>, u32>,
    df: Vec<u32>,
    postings: Vec<Arc<Vec<(u32, u32)>>>,
    lengths: Vec<u32>,
    average: f64,
}

impl LexicalIndex {
    fn empty() -> Self {
        Self {
            lookup: HashMap::new(),
            df: Vec::new(),
            postings: Vec::new(),
            lengths: Vec::new(),
            average: 1.0,
        }
    }

    fn average(lengths: &[u32], corpus_len: usize) -> f64 {
        let total: usize = lengths.iter().map(|&length| length as usize).sum();
        (total as f64 / corpus_len.max(1) as f64).max(1.0)
    }

    /// Bit-identical replacement for `crate::bm25` over the resident rows.
    ///
    /// Terms are visited in the caller's sorted order, and a chunk receives its
    /// contributions in that same order, so every f64 addition has the same
    /// operands and order as the per-chunk `counts` scan it replaces.
    fn bm25(&self, terms: &[String]) -> Vec<f64> {
        // `f64`'s empty `Sum` is negative zero on this toolchain, so a chunk
        // with no matching term must stay negative zero to keep output bytes
        // and `total_cmp` tie-breaks identical to the old map scan.
        let mut scores = vec![-0.0f64; self.lengths.len()];
        let corpus_len = self.lengths.len();
        for term in terms {
            let Some(&id) = self.lookup.get(term.as_str()) else {
                continue;
            };
            let df = self.df[id as usize] as f64;
            let idf = (1.0 + (corpus_len as f64 - df + 0.5) / (df + 0.5)).ln();
            for &(chunk, count) in self.postings[id as usize].iter() {
                let count = count as f64;
                let length = self.lengths[chunk as usize] as f64;
                scores[chunk as usize] +=
                    idf * count * 2.5 / (count + 1.5 * (0.25 + 0.75 * length / self.average));
            }
        }
        scores
    }
}

/// One stored 256-wide f32 passage vector.
///
/// A base snapshot keeps its vectors in a memory-mapped file, so the resident
/// set is file-backed and reclaimable. Edits write only their new vectors to
/// the heap; a later fold writes them back into a fresh mapped snapshot.
#[derive(Clone)]
pub enum VectorRef {
    Heap(Arc<Vec<f32>>),
    Mapped { map: Arc<Mmap>, offset: usize },
}

impl VectorRef {
    pub fn as_slice(&self) -> &[f32] {
        match self {
            VectorRef::Heap(vector) => vector,
            VectorRef::Mapped { map, offset } => {
                debug_assert_eq!(offset % std::mem::align_of::<f32>(), 0);
                // The mapping starts page-aligned and every record is 1024
                // bytes, so the cast is aligned. The format is little-endian on
                // the x86_64 targets this crate supports, like the pinned
                // sgrep index that uses the same cast.
                unsafe { std::slice::from_raw_parts(map[*offset..].as_ptr().cast(), 256) }
            }
        }
    }

    fn is_mapped(&self) -> bool {
        matches!(self, VectorRef::Mapped { .. })
    }

    fn write_bytes(&self, writer: &mut impl Write) -> Result<()> {
        match self {
            // The mapped bytes are already the little-endian on-disk form.
            VectorRef::Mapped { map, offset } => {
                writer.write_all(&map[*offset..*offset + 256 * 4])?;
            }
            VectorRef::Heap(vector) => {
                let mut bytes = [0u8; 256 * 4];
                for (slot, value) in bytes.chunks_exact_mut(4).zip(vector.iter()) {
                    slot.copy_from_slice(&value.to_le_bytes());
                }
                writer.write_all(&bytes)?;
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for VectorRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VectorRef::Heap(_) => f.write_str("VectorRef::Heap"),
            VectorRef::Mapped { offset, .. } => write!(f, "VectorRef::Mapped({offset})"),
        }
    }
}

/// One source file's immutable chunks and their passage vectors.
///
/// Both lists are `Arc`-shared. An unchanged file therefore survives a refresh
/// as one `Arc` clone, with no content, term or embedding copy. The vectors
/// stay valid only for the model revision that produced them.
struct FileData {
    digest: [u8; 32],
    chunks: Vec<Arc<Chunk>>,
    vectors: Vec<VectorRef>,
    lexical: FileLexical,
}

/// One atomically published corpus state.
///
/// `files` maps each path to its shared [`FileData`]. `chunks` and `vectors`
/// are flat, index-parallel views that share the same rows, so a one-file edit
/// copies only pointers into the new flat views, never content.
struct Snapshot {
    files: HashMap<PathBuf, Arc<FileData>>,
    chunks: Vec<Arc<Chunk>>,
    vectors: Vec<VectorRef>,
    lexical: Arc<LexicalIndex>,
    model: Arc<LoadedModel>,
    generation: u64,
}

/// Per-component memory accounting for one published snapshot.
///
/// The values sum allocation sizes where Rust exposes them (capacity for
/// strings and vectors, element counts for hash maps) and add a small structural
/// estimate for hash tables. They are an accounting pass for diagnosis, not an
/// allocator measurement.
#[derive(Serialize, Clone, Default)]
pub struct MemoryReport {
    pub files: usize,
    pub chunks: usize,
    pub terms: usize,
    pub terms_bytes: u64,
    pub postings_entries: u64,
    pub lexical_bytes: u64,
    pub four_byte_words: usize,
    pub chunk_struct_bytes: u64,
    pub chunk_arc_headers: u64,
    pub content_bytes: u64,
    pub content_len: u64,
    pub mapped_text_bytes: u64,
    pub mapped_texts: u64,
    pub heap_texts: u64,
    pub path_bytes: u64,
    pub path_len: u64,
    pub counts_entries: u64,
    pub counts_capacity: u64,
    pub counts_table_bytes: u64,
    pub counts_key_bytes: u64,
    pub rss_kib: u64,
    pub vector_value_bytes: u64,
    pub heap_vector_bytes: u64,
    pub mapped_vectors: usize,
    pub heap_vectors: usize,
    pub vector_struct_bytes: u64,
    pub vector_arc_headers: u64,
    pub flat_view_bytes: u64,
    pub file_map_bytes: u64,
    pub file_data_bytes: u64,
    pub model_table_bytes: u64,
    pub total_estimate_bytes: u64,
}

#[derive(Serialize, Clone)]
pub struct LiveStats {
    pub root: PathBuf,
    pub files: usize,
    pub chunks: usize,
    pub generation: u64,
    pub model_revision: String,
    pub encoded_texts: u64,
    pub build_ms: u64,
    pub model_ms: u64,
}
#[derive(Serialize)]
pub struct UpdateStats {
    pub changed_files: usize,
    pub encoded_passages: usize,
    pub elapsed_ms: u64,
}

/// Build stage numbers shared with `wg status`. They only describe progress;
/// they never change what the index contains.
pub const STAGE_TEXT: u8 = 1;
pub const STAGE_DISCOVER: u8 = 2;
pub const STAGE_READ: u8 = 3;
pub const STAGE_ASSEMBLE: u8 = 4;
pub const STAGE_EMBED: u8 = 5;
pub const STAGE_VERIFY: u8 = 6;
pub const STAGE_LEXICAL: u8 = 7;
pub const STAGE_PUBLISH: u8 = 8;
pub const STAGE_READY: u8 = 9;

fn stage_label(stage: u8) -> &'static str {
    match stage {
        STAGE_TEXT => "text",
        STAGE_DISCOVER => "discover",
        STAGE_READ => "read",
        STAGE_ASSEMBLE => "assemble",
        STAGE_EMBED => "embed",
        STAGE_VERIFY => "verify",
        STAGE_LEXICAL => "lexical",
        STAGE_PUBLISH => "publish",
        STAGE_READY => "ready",
        _ => "starting",
    }
}

/// Cheap, lock-free build progress for `wg up` and `wg status`.
///
/// A large tree can take minutes, so the service returns from `up` at once and
/// the client polls this state. Every value is an atomic counter, so writers
/// never block readers.
#[derive(Default)]
pub struct BuildProgress {
    stage: AtomicU64,
    files_done: AtomicUsize,
    files_total: AtomicUsize,
    encoded_done: AtomicUsize,
    encoded_total: AtomicUsize,
    chunks: AtomicUsize,
}

impl BuildProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_stage(&self, stage: u8) {
        self.stage.store(stage as u64, Ordering::Relaxed);
    }

    pub fn stage(&self) -> u8 {
        self.stage.load(Ordering::Relaxed) as u8
    }

    pub fn set_files(&self, done: usize, total: usize) {
        self.files_done.store(done, Ordering::Relaxed);
        self.files_total.store(total, Ordering::Relaxed);
    }

    pub fn add_files(&self, count: usize) {
        self.files_done.fetch_add(count, Ordering::Relaxed);
    }

    pub fn set_encoded(&self, done: usize, total: usize) {
        self.encoded_done.store(done, Ordering::Relaxed);
        self.encoded_total.store(total, Ordering::Relaxed);
    }

    pub fn add_encoded(&self, count: usize) {
        self.encoded_done.fetch_add(count, Ordering::Relaxed);
    }

    pub fn set_chunks(&self, chunks: usize) {
        self.chunks.store(chunks, Ordering::Relaxed);
    }

    pub fn to_json(&self) -> serde_json::Value {
        let stage = self.stage();
        serde_json::json!({
            "stage": stage_label(stage),
            "stage_code": stage,
            "files_done": self.files_done.load(Ordering::Relaxed),
            "files_total": self.files_total.load(Ordering::Relaxed),
            "chunks": self.chunks.load(Ordering::Relaxed),
            "encoded_done": self.encoded_done.load(Ordering::Relaxed),
            "encoded_total": self.encoded_total.load(Ordering::Relaxed),
        })
    }
}

pub struct LiveIndex {
    root: PathBuf,
    choice: ModelChoice,
    models: Arc<ModelPool>,
    snapshot: RwLock<Arc<Snapshot>>,
    updates: Mutex<()>,
    build_ms: u64,
    model_ms: u64,
    /// Service-supplied snapshot path. `live.rs` never invents one.
    cache: Option<PathBuf>,
    /// True after a cache load or successful write. A missing or rejected
    /// cache makes the first publish write a fresh one.
    cache_ready: AtomicBool,
}

/// One file's result from the parallel read/hash/chunk stage. The serial apply
/// step uses these to keep the ordered maps and change bookkeeping deterministic.
enum PreparedFile {
    Removed(PathBuf),
    Unchanged,
    Empty(PathBuf, [u8; 32]),
    Rebuilt(
        PathBuf,
        [u8; 32],
        Vec<Arc<Chunk>>,
        Vec<RawChunk>,
        Vec<Option<VectorRef>>,
    ),
}

/// Cumulative per-file phase times from the parallel stage. They are additive
/// across workers; `WG_LIVE_TIMING=1` prints them to show where the wall time
/// goes when several files run at once.
#[derive(Default)]
struct BuildTimings {
    read_ns: AtomicU64,
    hash_ns: AtomicU64,
    chunk_ns: AtomicU64,
    counts_ns: AtomicU64,
}

impl BuildTimings {
    fn add(slot: &AtomicU64, start: Instant) {
        slot.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    fn print(&self, label: &str) {
        if std::env::var_os("WG_LIVE_TIMING").is_some() {
            let ms = |slot: &AtomicU64| slot.load(Ordering::Relaxed) / 1_000_000;
            eprintln!(
                "wg live: {label}.read={}ms {label}.hash={}ms {label}.chunk={}ms {label}.counts={}ms",
                ms(&self.read_ns),
                ms(&self.hash_ns),
                ms(&self.chunk_ns),
                ms(&self.counts_ns),
            );
        }
    }
}

/// Bounds-checked slice reader for the mapped snapshot header.
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(len).ok_or("snapshot cursor overflow")?;
        let slice = self
            .data
            .get(self.at..end)
            .ok_or("snapshot cache is truncated")?;
        self.at = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }

    fn field(&mut self, cap: usize) -> Result<Vec<u8>> {
        let len = self.u32()? as usize;
        if len > cap {
            return Err("snapshot cache field exceeds bound".into());
        }
        Ok(self.take(len)?.to_vec())
    }

}

fn read_u32(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_field(writer: &mut impl Write, bytes: &[u8], cap: usize) -> Result<()> {
    if bytes.len() > cap || bytes.len() > u32::MAX as usize {
        return Err("snapshot cache field exceeds bound".into());
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(bytes)?;
    Ok(())
}

fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(&bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Load a v3 snapshot cache into a partial [`Snapshot`].
///
/// The cache stores chunk line ranges, the chunk text itself, file digests,
/// and passage vectors. Each entry is accepted only when its digest still
/// matches the file. Chunk text is served from the mapping, after its offset
/// and length are checked against the file and its bytes are checked as UTF-8
/// and against the live source lines. Word counts are recomputed with the same
/// normalization and `make_chunk` math as [`prepare_file`], so a cache hit is
/// identical to a fresh build. Any header, bound, or record problem rejects the
/// whole cache; callers then do a full build.
/// Normalize line endings once. Most trees are LF-only, so avoid the two full
/// copies that an unconditional `replace` pair makes.
fn normalize_source(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    let text = String::from_utf8_lossy(bytes);
    if text.contains('\r') {
        std::borrow::Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        text
    }
}

fn parallel_map<T, U>(items: &[T], f: impl Fn(&T) -> U + Sync + Send) -> Vec<U>
where
    T: Sync,
    U: Send,
{
    if items.len() <= 1 {
        return items.iter().map(f).collect();
    }
    items.par_iter().map(f).collect()
}

/// One chunk's cached metadata: source line range and its byte range in the
/// snapshot's text section.
struct CachedChunk {
    start: usize,
    end: usize,
    text_offset: usize,
    text_len: usize,
}

struct CachedEntry {
    rel: PathBuf,
    digest: [u8; 32],
    chunks: Vec<CachedChunk>,
    vec_offset: usize,
}

/// Move one chunk's owned word map into the compact interner input and drop
/// the map allocation immediately.
fn raw_from_chunk(chunk: &mut Chunk) -> RawChunk {
    let length = chunk.counts.values().sum::<usize>() as u32;
    let terms = chunk
        .counts
        .drain()
        .map(|(term, count)| (term, count as u32))
        .collect();
    // `drain` keeps the table allocation; reset so the resident chunk holds no
    // word-map memory at all.
    chunk.counts = HashMap::new();
    RawChunk::Raw { terms, length }
}

struct RebuiltCached {
    rel: PathBuf,
    digest: [u8; 32],
    chunks: Vec<Arc<Chunk>>,
    raw: Vec<RawChunk>,
}

/// True when `text` is exactly the concatenation of `lines`. Used to reject a
/// cache whose text section and line ranges disagree, without allocating the
/// concatenation.
fn text_equals_lines(text: &str, lines: &[&str]) -> bool {
    let mut at = 0usize;
    for line in lines {
        let bytes = line.as_bytes();
        let Some(end) = at.checked_add(bytes.len()) else {
            return false;
        };
        if text.as_bytes().get(at..end) != Some(bytes) {
            return false;
        }
        at = end;
    }
    at == text.len()
}

/// Rebuild one cached file from the live bytes, but point its content at the
/// snapshot's mapped text section instead of copying it to the heap. The cached
/// ranges are checked against the live lines, and the mapped bytes must be
/// exactly those lines. A stale or malformed entry returns `None`; the caller's
/// reconcile rebuilds that file.
fn rebuild_cached_file(
    root: &Path,
    entry: &CachedEntry,
    map: &Arc<Mmap>,
    text_base: usize,
) -> Option<RebuiltCached> {
    let bytes = std::fs::read(root.join(&entry.rel)).ok()?;
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if actual != entry.digest {
        return None;
    }
    if bytes.contains(&0) {
        if !entry.chunks.is_empty() {
            return None;
        }
        return Some(RebuiltCached {
            rel: entry.rel.clone(),
            digest: entry.digest,
            chunks: Vec::new(),
            raw: Vec::new(),
        });
    }
    let source = normalize_source(&bytes);
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let shared_path = crate::SharedStr::new(entry.rel.to_string_lossy());
    let mut rows = Vec::with_capacity(entry.chunks.len());
    let mut raw = Vec::with_capacity(entry.chunks.len());
    for cached in &entry.chunks {
        let (first, last) = (cached.start, cached.end);
        if first < 1 || last < first || last > lines.len() {
            return None;
        }
        let offset = text_base.checked_add(cached.text_offset)?;
        let content = crate::ChunkText::mapped(map.clone(), offset, cached.text_len).ok()?;
        if !text_equals_lines(content.as_str(), &lines[first - 1..last]) {
            return None;
        }
        let mut chunk = crate::make_chunk_content(shared_path.clone(), first, last, content);
        raw.push(raw_from_chunk(&mut chunk));
        rows.push(Arc::new(chunk));
    }
    Some(RebuiltCached {
        rel: entry.rel.clone(),
        digest: entry.digest,
        chunks: rows,
        raw,
    })
}

fn load_snapshot_cache(
    base: &Path,
    root: &Path,
    model: &Arc<LoadedModel>,
    progress: Option<&BuildProgress>,
) -> Result<Snapshot> {
    // Choose the newest generation. A fold writes a new versioned file while
    // the previous one may still be mapped by another thread or by Windows.
    let path = snapshot_candidates(base)
        .into_iter()
        .filter_map(|path| snapshot_generation(&path).map(|generation| (generation, path)))
        .max_by_key(|(generation, _)| *generation)
        .map(|(_, path)| path)
        .ok_or("snapshot cache missing")?;
    let file = File::open(&path)?;
    let map = Arc::new(unsafe { Mmap::map(&file)? });
    let mut cursor = Cursor::new(&map[..]);
    if cursor.take(8)? != SNAPSHOT_MAGIC {
        return Err("snapshot cache magic mismatch".into());
    }
    let version = cursor.u32()?;
    if version != SNAPSHOT_VERSION {
        return Err(format!("snapshot cache version {version} is unsupported").into());
    }
    let cached_root = cursor.field(SNAPSHOT_MAX_META)?;
    if cached_root != root.as_os_str().as_encoded_bytes() {
        return Err("snapshot cache root mismatch".into());
    }
    let cached_model = cursor.field(SNAPSHOT_MAX_META)?;
    if cached_model != model.revision().as_bytes() {
        return Err("snapshot cache model mismatch".into());
    }
    let generation = cursor.u64()?;
    let file_count = cursor.u32()? as usize;
    let total_chunks = cursor.u32()? as usize;
    let meta_len = cursor.u64()? as usize;
    let text_base = cursor.u64()? as usize;
    let text_section_len = cursor.u64()? as usize;
    let vector_base = cursor.u64()? as usize;
    if file_count > SNAPSHOT_MAX_FILES || total_chunks > SNAPSHOT_MAX_CHUNKS {
        return Err("snapshot cache header count exceeds bound".into());
    }
    // Layout: header, metadata, text, alignment padding, vectors. Every offset
    // is checked against the file before any slice is made.
    if text_base
        .checked_sub(cursor.at)
        .is_none_or(|len| len != meta_len)
    {
        return Err("snapshot cache metadata length is invalid".into());
    }
    if text_base
        .checked_add(text_section_len)
        .is_none_or(|end| end > vector_base)
    {
        return Err("snapshot cache text section exceeds the file".into());
    }
    let vector_end = vector_base
        .checked_add(total_chunks * 256 * 4)
        .ok_or("snapshot cache vector section overflow")?;
    if vector_end > map.len() {
        return Err("snapshot cache vector section exceeds the file".into());
    }
    // `VectorRef::as_slice` casts mapped bytes to `f32`; an unaligned base
    // from a corrupt header must be an error, never an unaligned read.
    if vector_base % std::mem::align_of::<f32>() != 0 {
        return Err("snapshot cache vector section is misaligned".into());
    }
    // The metadata starts right after the header and ends at `text_base`.
    let mut entries: Vec<CachedEntry> = Vec::with_capacity(file_count.min(4096));
    let mut paths: HashSet<PathBuf> = HashSet::with_capacity(file_count.min(4096));
    let mut seen_chunks = 0usize;
    for _ in 0..file_count {
        let rel = path_from_bytes(cursor.field(SNAPSHOT_MAX_PATH)?);
        if rel.as_os_str().is_empty()
            || rel.is_absolute()
            || rel.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err("snapshot cache path escapes root".into());
        }
        if !paths.insert(rel.clone()) {
            return Err("snapshot cache has a duplicate path".into());
        }
        let mut digest = [0u8; 32];
        digest.copy_from_slice(cursor.take(32)?);
        let chunk_count = cursor.u32()? as usize;
        if chunk_count > SNAPSHOT_MAX_CHUNKS
            || seen_chunks
                .checked_add(chunk_count)
                .is_none_or(|n| n > total_chunks)
        {
            return Err("snapshot cache chunk count exceeds header".into());
        }
        let mut chunks = Vec::with_capacity(chunk_count.min(1024));
        for _ in 0..chunk_count {
            let start = cursor.u32()? as usize;
            let end = cursor.u32()? as usize;
            let text_offset = cursor.u64()? as usize;
            let chunk_text_len = cursor.u32()? as usize;
            if text_offset
                .checked_add(chunk_text_len)
                .is_none_or(|end| end > text_section_len)
            {
                return Err("snapshot cache text range exceeds the text section".into());
            }
            chunks.push(CachedChunk {
                start,
                end,
                text_offset,
                text_len: chunk_text_len,
            });
        }
        let vec_offset = vector_base + seen_chunks * 256 * 4;
        seen_chunks += chunk_count;
        entries.push(CachedEntry {
            rel,
            digest,
            chunks,
            vec_offset,
        });
    }
    if seen_chunks != total_chunks {
        return Err("snapshot cache chunk total mismatch".into());
    }
    if cursor.at != text_base {
        return Err("snapshot cache metadata length mismatch".into());
    }
    // Rebuild the per-file rows on the worker pool, in bounded batches. A
    // stale or unreadable file is simply absent from the partial state; the
    // following reconcile rebuilds it. Batching keeps chunk text and term
    // strings for a large tree from all being live at the same time.
    let mut files = HashMap::with_capacity(entries.len());
    let mut interner = TermInterner::new();
    if let Some(progress) = progress {
        progress.set_stage(STAGE_READ);
        progress.set_files(0, entries.len());
    }
    for batch in entries.chunks(256) {
        let rebuilt = parallel_map(batch, |entry| {
            rebuild_cached_file(root, entry, &map, text_base)
        });
        for (item, entry) in rebuilt.into_iter().zip(batch) {
            let Some(item) = item else { continue };
            let lexical = interner.intern_file(item.raw);
            let vectors = (0..item.chunks.len())
                .map(|i| VectorRef::Mapped {
                    map: map.clone(),
                    offset: entry.vec_offset + i * 256 * 4,
                })
                .collect();
            files.insert(
                item.rel,
                Arc::new(FileData {
                    digest: item.digest,
                    chunks: item.chunks,
                    vectors,
                    lexical,
                }),
            );
        }
        if let Some(progress) = progress {
            progress.add_files(batch.len());
            progress.set_chunks(files.values().map(|data| data.chunks.len()).sum());
        }
    }
    // Publish the same deterministic flat order as `refresh`.
    let mut ordered: Vec<PathBuf> = files.keys().cloned().collect();
    ordered.sort_unstable_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    let mut chunks = Vec::with_capacity(seen_chunks);
    let mut vectors = Vec::with_capacity(seen_chunks);
    for rel in &ordered {
        let data = &files[rel];
        chunks.extend(data.chunks.iter().cloned());
        vectors.extend(data.vectors.iter().cloned());
    }
    let lexical = build_lexical(interner.lookup, &files, &ordered, chunks.len());
    prune_snapshot_versions(base, &path);
    Ok(Snapshot {
        files,
        chunks,
        vectors,
        lexical: Arc::new(lexical),
        model: model.clone(),
        generation,
    })
}

/// Build one complete lexical index from per-file compact term lists.
fn build_lexical(
    lookup: HashMap<Arc<str>, u32>,
    files: &HashMap<PathBuf, Arc<FileData>>,
    ordered: &[PathBuf],
    chunk_count: usize,
) -> LexicalIndex {
    let term_count = lookup.len();
    let mut df = vec![0u32; term_count];
    let mut postings: Vec<Vec<(u32, u32)>> = (0..term_count).map(|_| Vec::new()).collect();
    let mut lengths = Vec::with_capacity(chunk_count);
    let mut id = 0u32;
    for rel in ordered {
        let data = &files[rel];
        for j in 0..data.chunks.len() {
            let (terms, length) = data.lexical.chunk(j);
            lengths.push(length);
            for &(term, count) in terms {
                df[term as usize] += 1;
                postings[term as usize].push((id, count));
            }
            id += 1;
        }
    }
    debug_assert_eq!(id as usize, chunk_count);
    let average = LexicalIndex::average(&lengths, chunk_count);
    let postings = postings.into_iter().map(Arc::new).collect();
    LexicalIndex {
        lookup,
        df,
        postings,
        lengths,
        average,
    }
}

/// One changed file whose flat chunk range is unchanged, so its ids are stable.
struct ChangedSpan {
    start: usize,
    len: usize,
    old: Arc<FileData>,
    new: Arc<FileData>,
}

/// Update only the terms that a set of same-layout file edits touched.
///
/// Unaffected posting buffers stay `Arc`-shared with the previous snapshot.
/// `spans` must be sorted by `start`.
fn update_lexical(
    old: &LexicalIndex,
    lookup: HashMap<Arc<str>, u32>,
    spans: &[ChangedSpan],
    chunk_count: usize,
) -> LexicalIndex {
    let mut df = old.df.clone();
    df.resize(lookup.len(), 0);
    let mut postings = old.postings.clone();
    postings.resize_with(lookup.len(), || Arc::new(Vec::new()));
    let mut lengths = old.lengths.clone();
    lengths.resize(chunk_count, 0);
    // Explicit ids avoid ambiguous ordering at a zero-length span boundary.
    let removed: HashSet<u32> = spans
        .iter()
        .flat_map(|span| span.start as u32..(span.start + span.len) as u32)
        .collect();
    let mut fresh: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
    let mut affected: HashSet<u32> = HashSet::new();
    for span in spans {
        for j in 0..span.old.chunks.len() {
            let (terms, _) = span.old.lexical.chunk(j);
            for &(term, _) in terms {
                affected.insert(term);
            }
        }
        for j in 0..span.new.chunks.len() {
            let (terms, _) = span.new.lexical.chunk(j);
            let id = (span.start + j) as u32;
            for &(term, count) in terms {
                affected.insert(term);
                fresh.entry(term).or_default().push((id, count));
            }
        }
        for j in 0..span.len {
            lengths[span.start + j] = span.new.lexical.lengths[j];
        }
    }
    for term in affected {
        let replaced = fresh.remove(&term);
        let kept: Vec<(u32, u32)> = postings[term as usize]
            .iter()
            .copied()
            .filter(|&(id, _)| !removed.contains(&id))
            .collect();
        if replaced.is_none() && kept.len() == postings[term as usize].len() {
            continue;
        }
        let merged = match replaced {
            Some(add) => {
                let mut merged = Vec::with_capacity(kept.len() + add.len());
                let mut a = kept.into_iter().peekable();
                let mut b = add.into_iter().peekable();
                loop {
                    match (a.peek(), b.peek()) {
                        (Some(x), Some(y)) if x.0 <= y.0 => merged.push(a.next().unwrap()),
                        (Some(_), Some(_)) => merged.push(b.next().unwrap()),
                        (Some(_), None) => merged.push(a.next().unwrap()),
                        (None, Some(_)) => merged.push(b.next().unwrap()),
                        (None, None) => break,
                    }
                }
                merged
            }
            None => kept,
        };
        df[term as usize] = merged.len() as u32;
        postings[term as usize] = Arc::new(merged);
    }
    let average = LexicalIndex::average(&lengths, chunk_count);
    LexicalIndex {
        lookup,
        df,
        postings,
        lengths,
        average,
    }
}

/// Versioned data file for one generation. Generation 1 keeps the base name so
/// a first build and the tests see a stable file; later folds use a new name so
/// an old memory mapping is never replaced in place (a Windows rule).
fn snapshot_version_path(base: &Path, generation: u64) -> PathBuf {
    if generation <= 1 {
        return base.to_path_buf();
    }
    let name = base
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("snapshot");
    let stem = name.strip_suffix(".wgsnap").unwrap_or(name);
    base.with_file_name(format!("{stem}.g{generation}.wgsnap"))
}

/// All data files that belong to `base`, including the base name and any
/// versioned folds.
fn snapshot_candidates(base: &Path) -> Vec<PathBuf> {
    let Some(parent) = base.parent() else {
        return Vec::new();
    };
    let Some(name) = base.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let stem = name.strip_suffix(".wgsnap").unwrap_or(name);
    let prefix = format!("{stem}.g");
    let mut out = Vec::new();
    if base.is_file() {
        out.push(base.to_path_buf());
    }
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let candidate = entry.file_name();
            let Some(candidate) = candidate.to_str() else {
                continue;
            };
            if candidate == name
                || (candidate.starts_with(&prefix) && candidate.ends_with(".wgsnap"))
            {
                let path = entry.path();
                if path.is_file() {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn skip_field(reader: &mut impl Read, cap: usize) -> Result<()> {
    let len = read_u32(reader)? as usize;
    if len > cap {
        return Err("snapshot cache field exceeds bound".into());
    }
    let mut remaining = len;
    let mut buffer = [0u8; 4096];
    while remaining > 0 {
        let take = remaining.min(buffer.len());
        reader.read_exact(&mut buffer[..take])?;
        remaining -= take;
    }
    Ok(())
}

/// Read just the generation from a snapshot header, without mapping the file.
fn snapshot_generation(path: &Path) -> Option<u64> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic).ok()?;
    if &magic != SNAPSHOT_MAGIC {
        return None;
    }
    if read_u32(&mut reader).ok()? != SNAPSHOT_VERSION {
        return None;
    }
    skip_field(&mut reader, SNAPSHOT_MAX_META).ok()?;
    skip_field(&mut reader, SNAPSHOT_MAX_META).ok()?;
    let mut raw = [0u8; 8];
    reader.read_exact(&mut raw).ok()?;
    Some(u64::from_le_bytes(raw))
}

/// Encode the missing vectors and keep every vector on the heap. Used for the
/// no-cache path (tests, one-shot callers) and as the fallback when a fold
/// cannot be written. Batch composition is identical to the fold path.
fn embed_heap_vectors(
    chunks: &[Arc<Chunk>],
    slots: &[Option<VectorRef>],
    missing: &[usize],
    model: &LoadedModel,
    progress: Option<&BuildProgress>,
) -> Result<Vec<VectorRef>> {
    if let Some(progress) = progress {
        progress.set_stage(STAGE_EMBED);
        progress.set_encoded(0, missing.len());
    }
    let mut embedded: Vec<VectorRef> = Vec::with_capacity(missing.len());
    for batch in missing.chunks(512) {
        let texts: Vec<String> = batch
            .iter()
            .map(|&i| format!("{}\n{}", chunks[i].path, chunks[i].content))
            .collect();
        embedded.extend(
            model
                .embed_parallel(&texts)?
                .into_iter()
                .map(|vector| VectorRef::Heap(Arc::new(vector))),
        );
        if let Some(progress) = progress {
            progress.add_encoded(batch.len());
        }
    }
    let mut embedded = embedded.into_iter();
    Ok(slots
        .iter()
        .map(|slot| slot.clone().unwrap_or_else(|| embedded.next().unwrap()))
        .collect())
}

/// A snapshot whose header, metadata and text section are on disk and mapped,
/// but whose vector table is not written yet. The mapping lets the caller point
/// every chunk at its text and drop the heap copy before the (usually long)
/// embedding pass. Vectors are then appended through the same file handle.
struct SnapshotText {
    file: File,
    tmp: PathBuf,
    dest: PathBuf,
    map: Arc<Mmap>,
    text_base: usize,
    text_len: usize,
    vector_base: usize,
    generation: u64,
}

/// The mapped result of a completed fold.
struct FoldedSnapshot {
    map: Arc<Mmap>,
    vector_base: usize,
    path: PathBuf,
    generation: u64,
}

/// Write the header, per-file metadata and text section of a v3 snapshot.
///
/// The file is opened read+write so the text section can be mapped through the
/// same handle before the vectors are appended. Layout: header, metadata, text,
/// alignment padding, vectors. The header carries all three offsets so the
/// reader can validate every section.
fn write_snapshot_text(
    base: &Path,
    root: &Path,
    model: &LoadedModel,
    generation: u64,
    ordered: &[PathBuf],
    files: &HashMap<PathBuf, Arc<FileData>>,
) -> Result<SnapshotText> {
    let total_chunks: usize = ordered.iter().map(|rel| files[rel].chunks.len()).sum();
    if files.len() > SNAPSHOT_MAX_FILES || total_chunks > SNAPSHOT_MAX_CHUNKS {
        return Err("snapshot exceeds cache bounds".into());
    }
    let mut text_len = 0usize;
    for rel in ordered {
        for chunk in &files[rel].chunks {
            text_len = text_len
                .checked_add(chunk.content.as_str().len())
                .ok_or("snapshot text section overflows")?;
        }
    }
    let mut meta = Vec::with_capacity(total_chunks.saturating_mul(20));
    let mut text_offset = 0u64;
    for rel in ordered {
        let data = &files[rel];
        write_field(
            &mut meta,
            rel.as_os_str().as_encoded_bytes(),
            SNAPSHOT_MAX_PATH,
        )?;
        meta.extend_from_slice(&data.digest);
        meta.extend_from_slice(&(data.chunks.len() as u32).to_le_bytes());
        for chunk in &data.chunks {
            if chunk.start > u32::MAX as usize || chunk.end > u32::MAX as usize {
                return Err("snapshot cache range exceeds u32".into());
            }
            let len = chunk.content.as_str().len();
            if len > u32::MAX as usize {
                return Err("snapshot cache chunk text exceeds u32".into());
            }
            meta.extend_from_slice(&(chunk.start as u32).to_le_bytes());
            meta.extend_from_slice(&(chunk.end as u32).to_le_bytes());
            meta.extend_from_slice(&text_offset.to_le_bytes());
            meta.extend_from_slice(&(len as u32).to_le_bytes());
            text_offset = text_offset
                .checked_add(len as u64)
                .ok_or("snapshot text section overflows")?;
        }
    }
    if text_offset as usize != text_len {
        return Err("snapshot text length mismatch".into());
    }
    let mut header = Vec::new();
    header.extend_from_slice(SNAPSHOT_MAGIC);
    header.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    write_field(
        &mut header,
        root.as_os_str().as_encoded_bytes(),
        SNAPSHOT_MAX_META,
    )?;
    write_field(
        &mut header,
        model.revision().as_bytes(),
        SNAPSHOT_MAX_META,
    )?;
    header.extend_from_slice(&generation.to_le_bytes());
    header.extend_from_slice(&(files.len() as u32).to_le_bytes());
    header.extend_from_slice(&(total_chunks as u32).to_le_bytes());
    let fields_end = header.len();
    let text_base = fields_end
        .checked_add(32)
        .and_then(|at| at.checked_add(meta.len()))
        .ok_or("snapshot header overflows")?;
    let pad = (4 - (text_base + text_len) % 4) % 4;
    let vector_base = text_base + text_len + pad;
    header.extend_from_slice(&(meta.len() as u64).to_le_bytes());
    header.extend_from_slice(&(text_base as u64).to_le_bytes());
    header.extend_from_slice(&(text_len as u64).to_le_bytes());
    header.extend_from_slice(&(vector_base as u64).to_le_bytes());
    debug_assert_eq!(header.len(), fields_end + 32);
    debug_assert_eq!(header.len() + meta.len(), text_base);

    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dest = snapshot_version_path(base, generation);
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("snapshot");
    let sequence = SNAPSHOT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dest.with_file_name(format!(
        ".{name}.{}.{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&tmp)?;
        {
            let mut writer = BufWriter::new(&file);
            writer.write_all(&header)?;
            writer.write_all(&meta)?;
            let mut written = 0usize;
            for rel in ordered {
                for chunk in &files[rel].chunks {
                    let bytes = chunk.content.as_str().as_bytes();
                    writer.write_all(bytes)?;
                    written += bytes.len();
                }
            }
            if written != text_len {
                return Err("snapshot text count mismatch".into());
            }
            writer.write_all(&vec![0u8; pad])?;
            writer.flush()?;
        }
        Ok(file)
    })();
    let file = match result {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
    };
    let map = match unsafe { Mmap::map(&file) } {
        Ok(map) => Arc::new(map),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
    };
    Ok(SnapshotText {
        file,
        tmp,
        dest,
        map,
        text_base,
        text_len,
        vector_base,
        generation,
    })
}

/// Read an exact byte range from a file at an absolute offset without mapping
/// the page into this process. Used for the embedding pass so a cold build does
/// not fault the whole just-written text section into resident memory.
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt as _;
        return file.read_exact_at(buffer, offset);
    }
    #[cfg(not(unix))]
    {
        use std::os::windows::fs::FileExt as _;
        let mut buffer = buffer;
        let mut offset = offset;
        while !buffer.is_empty() {
            let n = file.seek_read(buffer, offset)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "short snapshot read",
                ));
            }
            offset += n as u64;
            buffer = &mut buffer[n..];
        }
        Ok(())
    }
}

/// Append the vector table to a mapped text snapshot, then publish it.
///
/// Vectors are written in chunk-id order; a missing (None) slot is embedded in
/// the same 512-row batches the heap path uses, so the embedding inputs and
/// outputs do not change. The caller's chunks already read their text from
/// `text.map`, so the embedding pass never holds the text on the heap.
fn finish_snapshot_folded(
    mut text: SnapshotText,
    model: &LoadedModel,
    chunks: &[Arc<Chunk>],
    slots: &[Option<VectorRef>],
    missing: &[usize],
    progress: Option<&BuildProgress>,
) -> Result<FoldedSnapshot> {
    let total_chunks = chunks.len();
    text.file.seek(std::io::SeekFrom::Start(text.vector_base as u64))?;
    // A separate read handle keeps positioned reads from moving the writer's
    // file pointer (Windows `seek_read` updates the handle position).
    let reader = match File::open(&text.tmp) {
        Ok(reader) => reader,
        Err(error) => {
            let _ = std::fs::remove_file(&text.tmp);
            return Err(error.into());
        }
    };
    let result = (|| -> Result<()> {
        let mut writer = BufWriter::new(&text.file);
        let mut batches = missing.chunks(512);
        let mut batch: Vec<Vec<f32>> = Vec::new();
        let mut batch_at = 0usize;
        let mut embedded = 0usize;
        let mut buffer: Vec<u8> = Vec::new();
        if let Some(progress) = progress {
            progress.set_stage(STAGE_EMBED);
            progress.set_encoded(0, missing.len());
        }
        for id in 0..total_chunks {
            match &slots[id] {
                Some(vector) => vector.write_bytes(&mut writer)?,
                None => {
                    if batch_at >= batch.len() {
                        let ids = batches
                            .next()
                            .ok_or("snapshot vector slot has no embedding batch")?;
                        let mut texts = Vec::with_capacity(ids.len());
                        for &i in ids {
                            let (offset, len) = match &chunks[i].content {
                                crate::ChunkText::Mapped { offset, len, .. } => (*offset, *len),
                                crate::ChunkText::Heap(text) => {
                                    texts.push(format!("{}\n{}", chunks[i].path, text));
                                    continue;
                                }
                            };
                            buffer.resize(len, 0);
                            read_exact_at(&reader, &mut buffer, offset as u64)?;
                            // The fold wrote these bytes from `&str`; they were
                            // never read from an untrusted cache.
                            let content = unsafe { std::str::from_utf8_unchecked(&buffer) };
                            texts.push(format!("{}\n{}", chunks[i].path, content));
                        }
                        batch = model.embed_parallel(&texts)?;
                        batch_at = 0;
                    }
                    let vector = &batch[batch_at];
                    if vector.len() != 256 {
                        return Err("snapshot cache vector width is not 256".into());
                    }
                    for value in vector.iter() {
                        writer.write_all(&value.to_le_bytes())?;
                    }
                    batch_at += 1;
                    embedded += 1;
                    if let Some(progress) = progress {
                        progress.add_encoded(1);
                    }
                }
            }
        }
        if embedded != missing.len() || batches.next().is_some() {
            return Err("snapshot vector count mismatch".into());
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        let expected = text.vector_base as u64 + total_chunks as u64 * 256 * 4;
        let actual = text.file.metadata()?.len();
        if actual != expected {
            return Err(format!(
                "snapshot vector section length {actual} != expected {expected}"
            )
            .into());
        }
        Ok(())
    })();
    if let Err(error) = result {
        // The mapped text is still valid for this process; leave the tmp file
        // for the OS to release and let the caller fall back to heap vectors.
        let _ = std::fs::remove_file(&text.tmp);
        return Err(error);
    }
    if text.dest.exists() {
        let _ = std::fs::remove_file(&text.dest);
    }
    if let Err(error) = std::fs::rename(&text.tmp, &text.dest) {
        let _ = std::fs::remove_file(&text.tmp);
        return Err(error.into());
    }
    let file = File::open(&text.dest)?;
    let map = Arc::new(unsafe { Mmap::map(&file)? });
    let _ = file.set_modified(std::time::SystemTime::now());
    Ok(FoldedSnapshot {
        map,
        vector_base: text.vector_base,
        path: text.dest,
        generation: text.generation,
    })
}

/// Remove superseded snapshot files. Best effort: on Windows an old mapping can
/// delay a delete until the daemon next starts.
fn prune_snapshot_versions(base: &Path, keep: &Path) {
    for candidate in snapshot_candidates(base) {
        if candidate != keep {
            let _ = std::fs::remove_file(candidate);
        }
    }
}

/// Read and rebuild one file off the update lock. Only immutable previous data
/// is read here, so workers cannot see each other's partial results.
fn prepare_file(
    root: &Path,
    rel: &PathBuf,
    old: &HashMap<PathBuf, Arc<FileData>>,
    timings: &BuildTimings,
) -> Result<PreparedFile> {
    let full = root.join(rel);
    let read = Instant::now();
    let bytes = match std::fs::read(&full) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedFile::Removed(rel.clone()));
        }
        Err(error) => return Err(error.into()),
    };
    BuildTimings::add(&timings.read_ns, read);
    let hash = Instant::now();
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    BuildTimings::add(&timings.hash_ns, hash);
    if old.get(rel).is_some_and(|data| data.digest == digest) {
        return Ok(PreparedFile::Unchanged);
    }
    if bytes.contains(&0) {
        return Ok(PreparedFile::Empty(rel.clone(), digest));
    }
    let chunk = Instant::now();
    let source = normalize_source(&bytes);
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let ranges = crate::splitter::ranges(&source, rel)?;
    BuildTimings::add(&timings.chunk_ns, chunk);
    let previous = old.get(rel);
    let shared_path = crate::SharedStr::new(rel.to_string_lossy());
    let mut chunks = Vec::new();
    let mut raw = Vec::new();
    let mut reused = Vec::new();
    for (first, last) in ranges {
        let assemble = Instant::now();
        let content = lines[first - 1..last].concat();
        // Reuse the previous immutable chunk when this range kept the same
        // bounds and content; then its vector and term ids still apply.
        let hit = previous.and_then(|data| {
            data.chunks
                .iter()
                .position(|c| c.start == first && c.end == last && &*c.content == content)
        });
        BuildTimings::add(&timings.chunk_ns, assemble);
        match hit {
            Some(i) => {
                let data = previous.unwrap();
                chunks.push(data.chunks[i].clone());
                let (terms, length) = data.lexical.chunk(i);
                raw.push(RawChunk::Known {
                    terms: terms.to_vec(),
                    length,
                });
                reused.push(data.vectors.get(i).cloned());
            }
            None => {
                let counts = Instant::now();
                let mut chunk = make_chunk(shared_path.clone(), first, last, content);
                raw.push(raw_from_chunk(&mut chunk));
                BuildTimings::add(&timings.counts_ns, counts);
                chunks.push(Arc::new(chunk));
                reused.push(None);
            }
        }
    }
    Ok(PreparedFile::Rebuilt(
        rel.clone(),
        digest,
        chunks,
        raw,
        reused,
    ))
}

impl LiveIndex {
    pub fn open(root: &Path, choice: ModelChoice, models: Arc<ModelPool>) -> Result<LiveIndex> {
        Self::open_with_cache(root, choice, models, None)
    }

    /// Open a live index and optionally reuse a service-supplied snapshot
    /// cache. The path is per root and model; `live.rs` never invents one.
    pub fn open_with_cache(
        root: &Path,
        choice: ModelChoice,
        models: Arc<ModelPool>,
        cache: Option<PathBuf>,
    ) -> Result<LiveIndex> {
        Self::open_with_cache_progress(root, choice, models, cache, None)
    }

    /// The same open path with optional lock-free build progress. The service
    /// uses it so `wg status` can report a long build while it runs.
    pub fn open_with_cache_progress(
        root: &Path,
        choice: ModelChoice,
        models: Arc<ModelPool>,
        cache: Option<PathBuf>,
        progress: Option<Arc<BuildProgress>>,
    ) -> Result<LiveIndex> {
        let started = Instant::now();
        let mut phase = Instant::now();
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err("live index root must be a directory".into());
        }
        // Auto only needs the path extensions, and prose is a per-path property.
        // Use the authoritative rg listing directly: the old full read+chunk
        // pre-scan produced the same decision at a large cost.
        let (_, listed) = crate::source_files(&root)?;
        let prose = listed.iter().all(|path| is_prose(&path.to_string_lossy()));
        mark("prescan_chunk", &mut phase);
        let model_started = Instant::now();
        let initial_model = if choice == ModelChoice::Text || (choice == ModelChoice::Auto && prose)
        {
            models.get(&TEXT)?
        } else {
            models.get(&CODE)?
        };
        let model_ms = model_started.elapsed().as_millis() as u64;
        let mut index = LiveIndex {
            root,
            choice,
            models,
            snapshot: RwLock::new(Arc::new(Snapshot {
                files: HashMap::new(),
                chunks: Vec::new(),
                vectors: Vec::new(),
                lexical: Arc::new(LexicalIndex::empty()),
                model: initial_model.clone(),
                generation: 0,
            })),
            updates: Mutex::new(()),
            build_ms: 0,
            model_ms,
            cache,
            cache_ready: AtomicBool::new(false),
        };
        if let Some(path) = index.cache.clone() {
            if let Some(progress) = progress.as_ref() {
                progress.set_stage(STAGE_DISCOVER);
            }
            match load_snapshot_cache(&path, &index.root, &initial_model, progress.as_deref()) {
                Ok(loaded) => {
                    let rows = loaded.chunks.len();
                    let files = loaded.files.len();
                    *index.snapshot.write().unwrap() = Arc::new(loaded);
                    index.cache_ready.store(true, Ordering::Relaxed);
                    if std::env::var_os("WG_LIVE_TIMING").is_some() {
                        eprintln!("wg live: cache_loaded files={files} chunks={rows}");
                    }
                }
                Err(error) => {
                    if std::env::var_os("WG_LIVE_TIMING").is_some() {
                        eprintln!("wg live: cache_skipped reason={error}");
                    }
                }
            }
            mark("cache_load", &mut phase);
        }
        index.refresh_with_progress(&[], true, progress.as_deref())?;
        if let Some(progress) = progress.as_ref() {
            progress.set_stage(STAGE_READY);
        }
        trim_allocator();
        if std::env::var_os("WG_LIVE_TIMING").is_some() {
            eprintln!("wg live: open_total={}ms", started.elapsed().as_millis());
        }
        index.build_ms = (started.elapsed().as_millis() as u64).saturating_sub(model_ms);
        Ok(index)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sum the allocation capacities of one published snapshot by component.
    pub fn memory_report(&self) -> MemoryReport {
        let s = self.snapshot.read().unwrap().clone();
        let n = s.chunks.len();
        let mut report = MemoryReport {
            files: s.files.len(),
            chunks: n,
            four_byte_words: n * 256,
            chunk_struct_bytes: (n * std::mem::size_of::<Chunk>()) as u64,
            chunk_arc_headers: (n * 2 * std::mem::size_of::<usize>()) as u64,
            vector_value_bytes: (n * 256 * std::mem::size_of::<f32>()) as u64,
            vector_struct_bytes: (n * std::mem::size_of::<VectorRef>()) as u64,
            flat_view_bytes: (s.chunks.capacity() * std::mem::size_of::<Arc<Chunk>>()
                + s.vectors.capacity() * std::mem::size_of::<VectorRef>())
                as u64,
            file_map_bytes: (s.files.capacity()
                * (std::mem::size_of::<PathBuf>()
                    + std::mem::size_of::<Arc<FileData>>()
                    + 1)) as u64,
            model_table_bytes: (s.model.table.len() * std::mem::size_of::<f32>()) as u64,
            ..MemoryReport::default()
        };
        for (path, data) in &s.files {
            report.file_map_bytes += path.as_os_str().len() as u64;
            report.file_data_bytes += std::mem::size_of::<FileData>() as u64;
            report.file_data_bytes += (data.chunks.capacity()
                * std::mem::size_of::<Arc<Chunk>>()
                + data.vectors.capacity() * std::mem::size_of::<VectorRef>())
                as u64;
            report.file_data_bytes += (data.lexical.terms.capacity()
                * std::mem::size_of::<(u32, u32)>()
                + data.lexical.offsets.capacity() * std::mem::size_of::<u32>()
                + data.lexical.lengths.capacity() * std::mem::size_of::<u32>())
                as u64;
            for chunk in &data.chunks {
                match &chunk.content {
                    crate::ChunkText::Heap(text) => {
                        report.heap_texts += 1;
                        report.content_bytes += text.len() as u64;
                    }
                    crate::ChunkText::Mapped { len, .. } => {
                        report.mapped_texts += 1;
                        report.mapped_text_bytes += *len as u64;
                    }
                }
                report.content_len += chunk.content.len() as u64;
                report.path_bytes += chunk.path.len() as u64;
                report.path_len += chunk.path.len() as u64;
                report.counts_entries += chunk.counts.len() as u64;
                report.counts_capacity += chunk.counts.capacity() as u64;
                report.counts_table_bytes += chunk.counts.capacity() as u64
                    * (std::mem::size_of::<String>() + std::mem::size_of::<usize>() + 1) as u64;
                for key in chunk.counts.keys() {
                    report.counts_key_bytes += key.capacity() as u64;
                }
            }
        }
        for vector in &s.vectors {
            match vector {
                VectorRef::Heap(values) => {
                    report.heap_vectors += 1;
                    report.heap_vector_bytes += (values.capacity() * 4) as u64;
                }
                VectorRef::Mapped { .. } => report.mapped_vectors += 1,
            }
        }
        let index = &s.lexical;
        report.terms = index.lookup.len();
        report.terms_bytes = (index.lookup.capacity()
            * (std::mem::size_of::<Arc<str>>() + std::mem::size_of::<u32>() + 1)) as u64
            + index.lookup.keys().map(|key| key.len() as u64).sum::<u64>();
        report.postings_entries = index.postings.iter().map(|row| row.len() as u64).sum();
        let postings_bytes: u64 = index
            .postings
            .iter()
            .map(|row| {
                (row.capacity() * std::mem::size_of::<(u32, u32)>()
                    + std::mem::size_of::<Vec<(u32, u32)>>()
                    + 2 * std::mem::size_of::<usize>()) as u64
            })
            .sum();
        report.lexical_bytes = report.terms_bytes
            + (index.df.len() * std::mem::size_of::<u32>()) as u64
            + (index.lengths.len() * std::mem::size_of::<u32>()) as u64
            + postings_bytes;
        report.rss_kib = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("VmRSS:")
                        .and_then(|value| value.split_whitespace().next())
                        .and_then(|value| value.parse().ok())
                })
            })
            .unwrap_or(0);
        report.total_estimate_bytes = report.chunk_struct_bytes
            + report.chunk_arc_headers
            + report.content_bytes
            + report.path_bytes
            + report.counts_table_bytes
            + report.counts_key_bytes
            + report.heap_vector_bytes
            + report.vector_struct_bytes
            + report.flat_view_bytes
            + report.file_map_bytes
            + report.file_data_bytes
            + report.model_table_bytes
            + report.lexical_bytes;
        report
    }

    pub fn stats(&self) -> LiveStats {
        let s = self.snapshot.read().unwrap().clone();
        LiveStats {
            root: self.root.clone(),
            files: s.files.len(),
            chunks: s.chunks.len(),
            generation: s.generation,
            model_revision: s.model.revision().to_string(),
            encoded_texts: s.model.encoded_texts(),
            build_ms: self.build_ms,
            model_ms: self.model_ms,
        }
    }

    /// Reconcile discovers files with the authoritative rg listing. Otherwise
    /// only supplied known paths are read; newly created paths require reconcile.
    ///
    /// The refresh shares every unchanged row. It clones the file map by pointer,
    /// rebuilds only the changed files' chunks (reusing a chunk `Arc` when its
    /// range and content are unchanged) and reuses an embedding unless the model
    /// revision changed. A refresh that finds no change keeps the published
    /// generation and does not rebuild any row.
    pub fn refresh(&self, paths: &[PathBuf], reconcile: bool) -> Result<UpdateStats> {
        self.refresh_with_progress(paths, reconcile, None)
    }

    fn refresh_with_progress(
        &self,
        paths: &[PathBuf],
        reconcile: bool,
        progress: Option<&BuildProgress>,
    ) -> Result<UpdateStats> {
        // Prepare off the query lock, but serialize writers so a slower update
        // cannot replace a newer snapshot made from a different old generation.
        let _update = self
            .updates
            .lock()
            .map_err(|_| "live index update poisoned")?;
        let started = Instant::now();
        let mut phase = Instant::now();
        let old = self
            .snapshot
            .read()
            .map_err(|_| "live index snapshot poisoned")?
            .clone();
        let target: HashSet<PathBuf> = if reconcile {
            let (_, files) = crate::source_files(&self.root)?;
            files.into_iter().map(|p| normalize(&p)).collect()
        } else {
            paths
                .iter()
                .map(|p| self.relative(p))
                .collect::<Result<HashSet<_>>>()?
        };
        mark("discover", &mut phase);
        // Shallow copy: unchanged entries keep their `Arc<FileData>`, so their
        // chunks and vectors are shared rather than cloned.
        let mut files: HashMap<PathBuf, Arc<FileData>> = if reconcile {
            old.files
                .iter()
                .filter(|(p, _)| target.contains(*p))
                .map(|(p, d)| (p.clone(), d.clone()))
                .collect()
        } else {
            old.files.clone()
        };
        let mut changed_paths: HashSet<PathBuf> = HashSet::new();
        // A changed file can reuse the vector of any chunk `Arc` it kept.
        let mut reused_vectors: HashMap<PathBuf, Vec<Option<VectorRef>>> = HashMap::new();
        // Chunk ids are stable only when no path appears or disappears and no
        // changed file gains or loses a chunk. The common single-file edit does.
        let mut layout_same = !reconcile || old.files.keys().all(|path| target.contains(path));
        let mut interner: Option<TermInterner> = None;
        let mut selected: Vec<PathBuf> = target.iter().cloned().collect();
        // Sort before the parallel read so an error returns for the first path in
        // byte order, not for whichever worker happens to fail first.
        selected.sort_unstable_by(|a, b| {
            a.as_os_str()
                .as_encoded_bytes()
                .cmp(b.as_os_str().as_encoded_bytes())
        });
        for rel in &selected {
            if !reconcile && !old.files.contains_key(rel) {
                return Err("new or unknown path requires reconcile=true".into());
            }
            if !reconcile && self.root.join(rel).is_dir() {
                return Err("directory refresh requires reconcile=true".into());
            }
        }
        let timings = BuildTimings::default();
        if let Some(progress) = progress {
            progress.set_stage(STAGE_READ);
            progress.set_files(0, selected.len());
            progress.set_chunks(files.values().map(|data| data.chunks.len()).sum());
        }
        // Read and intern in bounded batches. The old path collected every
        // file's chunks and term strings before it applied any of them, so a
        // 1.6 GB tree held a second copy of its text plus the term lists all at
        // once. A batch drops both before the next read starts.
        const PREPARE_BATCH: usize = 256;
        let mut live_chunks: usize = files.values().map(|data| data.chunks.len()).sum();
        for batch in selected.chunks(PREPARE_BATCH) {
            let prepared: Vec<Result<PreparedFile>> = batch
                .par_iter()
                .map(|rel| prepare_file(&self.root, rel, &old.files, &timings))
                .collect();
            for item in prepared {
                match item? {
                    PreparedFile::Removed(rel) => {
                        if let Some(removed) = files.remove(&rel) {
                            live_chunks = live_chunks.saturating_sub(removed.chunks.len());
                            changed_paths.insert(rel);
                            layout_same = false;
                        }
                    }
                    PreparedFile::Unchanged => {}
                    PreparedFile::Empty(rel, digest) => {
                        if let Some(previous) = old.files.get(&rel) {
                            if !previous.chunks.is_empty() {
                                layout_same = false;
                            }
                            live_chunks = live_chunks.saturating_sub(previous.chunks.len());
                        }
                        changed_paths.insert(rel.clone());
                        files.insert(
                            rel,
                            Arc::new(FileData {
                                digest,
                                chunks: Vec::new(),
                                vectors: Vec::new(),
                                lexical: FileLexical::default(),
                            }),
                        );
                    }
                    PreparedFile::Rebuilt(rel, digest, chunks, raw, reused) => {
                        if old.files.get(&rel).map(|data| data.chunks.len())
                            != Some(chunks.len())
                        {
                            layout_same = false;
                        }
                        if let Some(previous) = old.files.get(&rel) {
                            live_chunks = live_chunks.saturating_sub(previous.chunks.len());
                        }
                        live_chunks += chunks.len();
                        let intern = interner
                            .get_or_insert_with(|| TermInterner::from_lookup(&old.lexical.lookup));
                        let lexical = intern.intern_file(raw);
                        changed_paths.insert(rel.clone());
                        reused_vectors.insert(rel.clone(), reused);
                        files.insert(
                            rel,
                            Arc::new(FileData {
                                digest,
                                chunks,
                                vectors: Vec::new(),
                                lexical,
                            }),
                        );
                    }
                }
            }
            if let Some(progress) = progress {
                progress.add_files(batch.len());
                progress.set_chunks(live_chunks);
            }
        }
        if reconcile {
            files.retain(|p, _| target.contains(p));
        }
        timings.print("read_chunk_count");
        mark("read_chunk_count", &mut phase);
        let all_paths: HashSet<_> = old.files.keys().chain(files.keys()).collect();
        let changed = all_paths
            .into_iter()
            .filter(|p| match (old.files.get(*p), files.get(*p)) {
                (Some(a), Some(b)) => a.digest != b.digest,
                (None, None) => false,
                _ => true,
            })
            .count();
        let prose = files
            .values()
            .flat_map(|file| file.chunks.iter())
            .all(|chunk| is_prose(&chunk.path));
        let model =
            if self.choice == ModelChoice::Text || (self.choice == ModelChoice::Auto && prose) {
                self.models.get(&TEXT)?
            } else {
                self.models.get(&CODE)?
            };
        let switch = old.model.revision() != model.revision();
        if changed == 0 && !switch {
            // Nothing changed: keep the published generation and every row.
            for rel in &target {
                match (files.get(rel), std::fs::read(self.root.join(rel))) {
                    (Some(data), Ok(bytes)) if Sha256::digest(&bytes).as_slice() == data.digest => {
                    }
                    (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => {
                        return Err(
                            "source changed while preparing live index; retry refresh".into()
                        );
                    }
                }
            }
            if reconcile {
                let (_, current) = crate::source_files(&self.root)?;
                let current: HashSet<_> = current.iter().map(|p| normalize(p)).collect();
                if current != target {
                    return Err(
                        "source membership changed while preparing live index; retry refresh"
                            .into(),
                    );
                }
            }
            return Ok(UpdateStats {
                changed_files: 0,
                encoded_passages: 0,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }
        let mut ordered: Vec<PathBuf> = files.keys().cloned().collect();
        ordered.sort_unstable_by(|a, b| {
            a.as_os_str()
                .as_encoded_bytes()
                .cmp(b.as_os_str().as_encoded_bytes())
        });
        if let Some(progress) = progress {
            progress.set_stage(STAGE_ASSEMBLE);
        }
        // Assemble the flat row views. Unchanged files contribute their existing
        // Arcs; changed files contribute rebuilt chunks and reuse a vector only
        // when the chunk Arc and the model revision both survived.
        let total: usize = files.values().map(|d| d.chunks.len()).sum();
        let mut chunks: Vec<Arc<Chunk>> = Vec::with_capacity(total);
        let mut slots: Vec<Option<VectorRef>> = Vec::with_capacity(total);
        let mut missing: Vec<usize> = Vec::new();
        let mut spans: Vec<(PathBuf, usize, usize)> = Vec::new();
        let mut file_starts: HashMap<PathBuf, (usize, usize)> = HashMap::new();
        for rel in &ordered {
            let data = &files[rel];
            let file_changed = changed_paths.contains(rel);
            let start = chunks.len();
            for (i, chunk) in data.chunks.iter().enumerate() {
                chunks.push(chunk.clone());
                let vector = if switch {
                    None
                } else if file_changed {
                    reused_vectors.get(rel).and_then(|v| v[i].clone())
                } else {
                    data.vectors.get(i).cloned()
                };
                match vector {
                    Some(v) => slots.push(Some(v)),
                    None => {
                        missing.push(slots.len());
                        slots.push(None);
                    }
                }
            }
            let len = chunks.len() - start;
            file_starts.insert(rel.clone(), (start, len));
            if switch || file_changed {
                spans.push((rel.clone(), start, len));
            }
        }
        mark("assemble_rows", &mut phase);
        let encoded = missing.len();
        let heap_vectors = slots
            .iter()
            .filter(|slot| matches!(slot, Some(VectorRef::Heap(_))))
            .count();
        // Fold when there is no mapped base, when ids moved, when too many heap
        // vectors accumulated, or when a cache refresh was already due. A fold
        // writes vectors directly to disk, so a full build never holds the
        // whole vector table in the heap.
        let fold = self.cache.is_some()
            && (!layout_same
                || old.vectors.iter().all(|vector| !vector.is_mapped())
                || heap_vectors + missing.len() > VECTOR_FOLD_HEAP
                || self.should_write_cache(reconcile, changed));
        let mut folded: Option<FoldedSnapshot> = None;
        // True once the pre-vector text snapshot exists and every chunk reads
        // from it, even if appending the vectors later fails.
        let mut text_remapped = false;
        let vectors: Vec<VectorRef> = if fold {
            let base = self.cache.as_deref().unwrap();
            match write_snapshot_text(
                base,
                &self.root,
                &model,
                old.generation + 1,
                &ordered,
                &files,
            ) {
                Ok(text) => {
                    // Point every chunk at the newly written text section, one
                    // file at a time. Replacing both the flat row and the
                    // `FileData` entry frees that file's heap text immediately,
                    // so the embedding pass never holds the text on the heap.
                    // The bytes came from `&str` written by this call, so the
                    // UTF-8 scan is skipped; cache input uses the checked
                    // constructor.
                    let mut offset = text.text_base;
                    for rel in &ordered {
                        let (start, len) = file_starts[rel];
                        let data = files.get(rel).unwrap().clone();
                        let mut fresh = Vec::with_capacity(len);
                        for (i, chunk) in data.chunks.iter().enumerate() {
                            let text_len = chunk.content.as_str().len();
                            let content = crate::ChunkText::mapped_trusted(
                                text.map.clone(),
                                offset,
                                text_len,
                            )?;
                            let replacement = Arc::new(Chunk {
                                path: chunk.path.clone(),
                                start: chunk.start,
                                end: chunk.end,
                                content,
                                counts: HashMap::new(),
                                scores: None,
                            });
                            chunks[start + i] = replacement.clone();
                            fresh.push(replacement);
                            offset += text_len;
                        }
                        files.insert(
                            rel.clone(),
                            Arc::new(FileData {
                                digest: data.digest,
                                chunks: fresh,
                                vectors: Vec::new(),
                                lexical: data.lexical.clone(),
                            }),
                        );
                    }
                    if offset != text.text_base + text.text_len {
                        return Err("folded snapshot text length mismatch".into());
                    }
                    text_remapped = true;
                    // The heap text just became unreachable. Return those pages
                    // before the embedding pass, so the allocator does not keep
                    // them resident for the rest of the build.
                    trim_allocator();
                    match finish_snapshot_folded(
                        text, &model, &chunks, &slots, &missing, progress,
                    ) {
                        Ok(result) => {
                            let vectors = (0..chunks.len())
                                .map(|i| VectorRef::Mapped {
                                    map: result.map.clone(),
                                    offset: result.vector_base + i * 256 * 4,
                                })
                                .collect();
                            folded = Some(result);
                            vectors
                        }
                        Err(error) => {
                            if std::env::var_os("WG_LIVE_TIMING").is_some() {
                                eprintln!(
                                    "wg live: fold_vectors_failed reason={error}; using heap vectors"
                                );
                            }
                            embed_heap_vectors(&chunks, &slots, &missing, &model, progress)?
                        }
                    }
                }
                Err(error) => {
                    if std::env::var_os("WG_LIVE_TIMING").is_some() {
                        eprintln!("wg live: fold_failed reason={error}; using heap vectors");
                    }
                    embed_heap_vectors(&chunks, &slots, &missing, &model, progress)?
                }
            }
        } else {
            embed_heap_vectors(&chunks, &slots, &missing, &model, progress)?
        };
        mark("embed", &mut phase);
        if let Some(progress) = progress {
            progress.set_stage(STAGE_VERIFY);
        }
        // Verify only the input paths. Ordinary file updates must not rescan the
        // unchanged repository; full reconciles additionally recheck membership.
        for rel in &target {
            match (files.get(rel), std::fs::read(self.root.join(rel))) {
                (Some(data), Ok(bytes)) if Sha256::digest(&bytes).as_slice() == data.digest => {}
                (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err("source changed while preparing live index; retry refresh".into()),
            }
        }
        if reconcile {
            let (_, current) = crate::source_files(&self.root)?;
            let current: HashSet<_> = current.iter().map(|p| normalize(p)).collect();
            if current != target {
                return Err(
                    "source membership changed while preparing live index; retry refresh".into(),
                );
            }
        }
        mark("verify", &mut phase);
        // Store each file's vectors beside its chunks, so the next refresh can
        // share them again without any lookup. A fold replaces the whole
        // mapped base, so every file points into the new map; otherwise only
        // the rebuilt spans change.
        let refolded = folded.is_some() || text_remapped;
        let touched: Vec<(PathBuf, usize, usize)> = if refolded {
            ordered
                .iter()
                .map(|rel| {
                    let (start, len) = file_starts[rel];
                    (rel.clone(), start, len)
                })
                .collect()
        } else {
            spans
        };
        for (rel, start, len) in touched {
            let data = files.get(&rel).unwrap().clone();
            files.insert(
                rel,
                Arc::new(FileData {
                    digest: data.digest,
                    chunks: chunks[start..start + len].to_vec(),
                    vectors: vectors[start..start + len].to_vec(),
                    lexical: data.lexical.clone(),
                }),
            );
        }
        if let Some(progress) = progress {
            progress.set_stage(STAGE_LEXICAL);
        }
        let lookup = interner
            .map(|intern| intern.lookup)
            .unwrap_or_else(|| old.lexical.lookup.clone());
        let lexical = if changed == 0 {
            // A model switch rebuilds vectors but leaves the term index alone.
            old.lexical.clone()
        } else if layout_same {
            let mut changed_spans: Vec<ChangedSpan> = changed_paths
                .iter()
                .filter_map(|rel| {
                    let (start, len) = *file_starts.get(rel)?;
                    let old_data = old.files.get(rel)?.clone();
                    let new_data = files.get(rel)?.clone();
                    (old_data.chunks.len() == len).then_some(ChangedSpan {
                        start,
                        len,
                        old: old_data,
                        new: new_data,
                    })
                })
                .collect();
            changed_spans.sort_unstable_by_key(|span| span.start);
            Arc::new(update_lexical(
                &old.lexical,
                lookup,
                &changed_spans,
                chunks.len(),
            ))
        } else {
            Arc::new(build_lexical(lookup, &files, &ordered, chunks.len()))
        };
        if let Some(progress) = progress {
            progress.set_stage(STAGE_PUBLISH);
        }
        let next = Arc::new(Snapshot {
            files,
            chunks,
            vectors,
            lexical,
            model,
            generation: old.generation + 1,
        });
        *self
            .snapshot
            .write()
            .map_err(|_| "live index snapshot poisoned")? = next.clone();
        mark("publish", &mut phase);
        if let Some(folded) = folded {
            self.cache_ready.store(true, Ordering::Relaxed);
            if let Some(base) = self.cache.as_deref() {
                prune_snapshot_versions(base, &folded.path);
                let _ = folded.generation;
            }
        }
        if !layout_same || encoded >= 512 || text_remapped {
            drop(old);
            trim_allocator();
        }
        Ok(UpdateStats {
            changed_files: changed,
            encoded_passages: encoded,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// Rewrite when the cache is missing/rejected, when a reconcile changed a
    /// file, or when one batch changed enough files to amortize the write.
    fn should_write_cache(&self, reconcile: bool, changed: usize) -> bool {
        self.cache.is_some()
            && (!self.cache_ready.load(Ordering::Relaxed)
                || (reconcile && changed > 0)
                || changed >= SNAPSHOT_REWRITE_FILES)
    }

    fn relative(&self, p: &Path) -> Result<PathBuf> {
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        // The stored root is canonical. Callers can pass a non-canonical path
        // (a Windows tempdir, a symlink, `..`) or a path that was just deleted.
        // Canonicalize the input, or its parent plus the final name when the
        // path no longer exists, so the comparison uses one path form.
        let resolved = match full.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = full.parent().ok_or("refresh path outside root")?;
                let name = full.file_name().ok_or("refresh path outside root")?;
                parent.canonicalize()?.join(name)
            }
            Err(error) => return Err(error.into()),
        };
        let rel = resolved
            .strip_prefix(&self.root)
            .map_err(|_| "refresh path outside root")?;
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("refresh path escapes root".into());
        }
        Ok(normalize(rel))
    }

    pub fn query(&self, query: &str, top_n: usize, explain: bool) -> Result<Vec<Chunk>> {
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| "live index snapshot poisoned")?
            .clone();
        let mut terms = words(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Err("query has no searchable words".into());
        }
        if snapshot.chunks.is_empty() {
            return Ok(Vec::new());
        }
        // The compact postings touch only matching chunks. Contributions reach
        // each chunk in query-term order, as the old per-chunk map scan did.
        let lexical = snapshot.lexical.bm25(&terms);
        let q = snapshot.model.embed(vec![query.to_string()])?.remove(0);
        let similarities: Vec<f64> = snapshot
            .vectors
            .iter()
            .map(|v| v.as_slice().iter().zip(&q).map(|(a, b)| a * b).sum::<f32>() as f64)
            .collect();
        let exact: HashSet<_> = identifier_pattern(query)?
            .into_iter()
            .flat_map(|(pattern, _)| {
                snapshot
                    .chunks
                    .iter()
                    .enumerate()
                    .filter_map(move |(i, c)| pattern.is_match(&c.content).then_some(i))
            })
            .collect();
        rank_impl(
            snapshot.chunks.len(),
            &lexical,
            &similarities,
            Vec::new(),
            &exact,
            false,
            explain,
            top_n,
            // Clone only the rows that ranking actually returns.
            |i| Ok((*snapshot.chunks[i]).clone()),
        )
    }
}

fn normalize(p: &Path) -> PathBuf {
    p.strip_prefix(".").unwrap_or(p).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Searcher;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn alpha() {\n    let useful_value = 1;\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.rs"),
            "fn beta() {\n    let other_value = 2;\n}\n",
        )
        .unwrap();
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    fn equal_fresh(root: &Path, live: &LiveIndex, pool: &ModelPool) {
        let fresh = Searcher::open(root, ModelChoice::Code, None).unwrap();
        for q in ["useful value", "other value", "renamed symbol"] {
            assert_eq!(
                serde_json::to_value(fresh.query(q, 10, true).unwrap()).unwrap(),
                serde_json::to_value(live.query(q, 10, true).unwrap()).unwrap()
            );
        }
        assert!(pool.loaded_count() >= 1);
    }

    fn file_data(live: &LiveIndex, rel: &str) -> Arc<FileData> {
        live.snapshot.read().unwrap().files[Path::new(rel)].clone()
    }

    fn file_chunks(live: &LiveIndex, rel: &str) -> Vec<Arc<Chunk>> {
        file_data(live, rel).chunks.clone()
    }

    #[test]
    fn relative_accepts_noncanonical_and_missing_paths_without_escape() {
        let (_dir, root) = fixture();
        let pool = Arc::new(ModelPool::new(None));
        let live = LiveIndex::open(&root, ModelChoice::Code, pool).unwrap();
        // The fixture root is not canonical on Windows. Both the absolute
        // non-canonical spelling and the relative path must map to `a.rs`.
        assert_eq!(
            live.relative(&root.join("a.rs")).unwrap(),
            PathBuf::from("a.rs")
        );
        assert_eq!(
            live.relative(Path::new("a.rs")).unwrap(),
            PathBuf::from("a.rs")
        );
        // A path that no longer exists resolves through its canonical parent.
        assert_eq!(
            live.relative(&root.join("deleted.rs")).unwrap(),
            PathBuf::from("deleted.rs")
        );
        // The traversal guard still rejects a path outside the root, even when
        // the target does not exist.
        assert!(
            live.relative(&root.join("..").join("escape.rs")).is_err(),
            "path outside the root must be rejected"
        );
    }

    /// The pre-change embed math, kept here as a test oracle. It rebuilds a
    /// per-batch unique-token table from raw safetensors bytes.
    fn reference_embed(model: &LoadedModel, raw: &[u8], texts: Vec<String>) -> Vec<Vec<f32>> {
        let tensors = safetensors::SafeTensors::deserialize(raw).unwrap();
        let tensor = tensors.tensor("embeddings").unwrap();
        let unknown = model.tokenizer.token_to_id("[UNK]");
        let encoded = if texts.len() == 1 {
            vec![
                model
                    .tokenizer
                    .encode_fast(texts.into_iter().next().unwrap(), false)
                    .unwrap(),
            ]
        } else {
            model.tokenizer.encode_batch_fast(texts, false).unwrap()
        };
        let width = if crate::CODE.dtype == safetensors::Dtype::F16 {
            2
        } else {
            4
        };
        let mut offsets = vec![usize::MAX; model.rows];
        let mut weights = Vec::new();
        for encoding in &encoded {
            for &id in encoding.get_ids() {
                if Some(id) == unknown {
                    continue;
                }
                let slot = &mut offsets[id as usize];
                if *slot != usize::MAX {
                    continue;
                }
                *slot = weights.len();
                let offset = id as usize * 256 * width;
                for bytes in tensor.data()[offset..offset + 256 * width].chunks_exact(width) {
                    weights.push(if width == 2 {
                        half::f16::from_le_bytes(bytes.try_into().unwrap()).to_f32()
                    } else {
                        f32::from_le_bytes(bytes.try_into().unwrap())
                    });
                }
            }
        }
        encoded
            .iter()
            .map(|encoding| {
                let mut vector = vec![0.0f32; 256];
                let ids: Vec<_> = encoding
                    .get_ids()
                    .iter()
                    .filter(|&&id| Some(id) != unknown)
                    .collect();
                for &&id in &ids {
                    let offset = offsets[id as usize];
                    for (x, weight) in vector.iter_mut().zip(&weights[offset..offset + 256]) {
                        *x += weight;
                    }
                }
                if !ids.is_empty() {
                    for x in &mut vector {
                        *x /= ids.len() as f32;
                    }
                }
                let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt() + 1e-32;
                for x in &mut vector {
                    *x /= norm;
                }
                vector
            })
            .collect()
    }

    #[test]
    fn embedding_table_matches_old_safetensors_math() {
        let cache = crate::cache::Cache::new(None, false).unwrap();
        let model = LoadedModel::load_meta(&crate::CODE, &cache).unwrap();
        let raw = cache
            .model_bytes(&crate::CODE, &crate::CODE.weights)
            .unwrap();
        let cases = [
            vec!["fn alpha() {\n    let useful_value = 1;\n}\n".to_string()],
            vec![
                "fn beta() {\n    let other_value = 2;\n}\n".to_string(),
                "the quick brown fox jumps over the lazy dog".to_string(),
            ],
        ];
        for texts in cases {
            let got = model.embed(texts.clone()).unwrap();
            let want = reference_embed(&model, &raw, texts);
            assert_eq!(
                got, want,
                "table embed differs from the raw-safetensors math"
            );
        }
    }

    #[test]
    fn compact_bm25_matches_the_per_chunk_map_scan_bit_for_bit() {
        let (_dir, root) = fixture();
        let mut body = String::new();
        for i in 0..300 {
            body.push_str(&format!(
                "fn helper_{i}() {{ let alpha = {i}; let beta = alpha + {i}; }}\n"
            ));
        }
        std::fs::write(root.join("notes.txt"), body).unwrap();
        let pool = Arc::new(ModelPool::new(None));
        let live = LiveIndex::open(&root, ModelChoice::Code, pool).unwrap();
        let snapshot = live.snapshot.read().unwrap().clone();
        let chunks = crate::candidates(&root, None).unwrap();
        assert_eq!(chunks.len(), snapshot.chunks.len());
        for query in [
            "useful value",
            "alpha beta helper_17",
            "notes txt filler",
            "missing term words",
        ] {
            let mut terms = words(query);
            terms.sort();
            terms.dedup();
            let expected = crate::bm25(&chunks, &terms, chunks.len());
            let actual = snapshot.lexical.bm25(&terms);
            assert_eq!(expected.len(), actual.len());
            for (i, (want, got)) in expected.iter().zip(&actual).enumerate() {
                assert_eq!(
                    want.to_bits(),
                    got.to_bits(),
                    "query {query:?} chunk {i}: {want:?} != {got:?}"
                );
            }
        }
    }

    #[test]
    fn zero_length_span_edit_matches_a_fresh_searcher() {
        let (_dir, root) = fixture();
        // A binary file sorts first and stays binary, so an edit to it is a
        // zero-length chunk span at the same start as the next changed file.
        std::fs::write(root.join("0.bin"), b"first\0payload").unwrap();
        let pool = Arc::new(ModelPool::new(None));
        let live = LiveIndex::open(&root, ModelChoice::Code, pool.clone()).unwrap();
        std::fs::write(root.join("0.bin"), b"second\0payload").unwrap();
        std::fs::write(root.join("a.rs"), "fn alpha() {\n    let changed_value = 9;\n}\n")
            .unwrap();
        let stats = live
            .refresh(
                &[root.join("0.bin"), root.join("a.rs")],
                false,
            )
            .unwrap();
        assert_eq!(stats.changed_files, 2);
        equal_fresh(&root, &live, &pool);
    }

    #[test]
    fn incremental_updates_match_fresh_searcher_and_reuse_unchanged_vectors() {
        let (_dir, root) = fixture();
        let pool = Arc::new(ModelPool::new(None));
        let live = LiveIndex::open(&root, ModelChoice::Code, pool.clone()).unwrap();
        let before = live.stats().encoded_texts;
        let generation = live.stats().generation;
        let changed = root.join("a.rs");
        std::fs::write(&changed, "fn alpha() {\n    let renamed_symbol = 3;\n}\n").unwrap();
        let stats = live.refresh(&[changed.clone()], false).unwrap();
        assert_eq!(stats.changed_files, 1);
        assert!(stats.encoded_passages > 0);
        assert!(stats.encoded_passages < live.stats().chunks);
        assert_eq!(live.stats().generation, generation + 1);
        assert_eq!(
            live.stats().encoded_texts,
            before + stats.encoded_passages as u64
        );
        let repeat = live.refresh(&[changed.clone()], false).unwrap();
        assert_eq!(repeat.encoded_passages, 0);
        assert_eq!(repeat.changed_files, 0);
        equal_fresh(&root, &live, &pool);
        let deleted = root.join("b.rs");
        std::fs::remove_file(&deleted).unwrap();
        assert_eq!(live.refresh(&[deleted], false).unwrap().changed_files, 1);
        equal_fresh(&root, &live, &pool);
        let added = root.join("c.rs");
        std::fs::write(
            &added,
            "fn renamed_symbol() {\n    let fresh_value = 4;\n}\n",
        )
        .unwrap();
        assert!(live.refresh(std::slice::from_ref(&added), false).is_err());
        live.refresh(&[], true).unwrap();
        equal_fresh(&root, &live, &pool);
        assert_eq!(live.stats().files, 2);
        std::fs::rename(&added, root.join("d.rs")).unwrap();
        live.refresh(&[], true).unwrap();
        equal_fresh(&root, &live, &pool);
        assert_eq!(live.stats().files, 2);
    }

    #[test]
    fn no_op_refresh_keeps_generation_and_embeds_nothing() {
        let (_dir, root) = fixture();
        let pool = Arc::new(ModelPool::new(None));
        let live = LiveIndex::open(&root, ModelChoice::Code, pool.clone()).unwrap();
        let generation = live.stats().generation;
        let encoded = live.stats().encoded_texts;
        for paths in [vec![root.join("a.rs")], vec![root.join("b.rs")]] {
            let stats = live.refresh(&paths, false).unwrap();
            assert_eq!(stats.changed_files, 0);
            assert_eq!(stats.encoded_passages, 0);
        }
        let stats = live.refresh(&[], true).unwrap();
        assert_eq!(stats.changed_files, 0);
        assert_eq!(stats.encoded_passages, 0);
        assert_eq!(live.stats().generation, generation);
        assert_eq!(live.stats().encoded_texts, encoded);
    }

    #[test]
    fn one_file_edit_shares_unchanged_arcs_and_reuses_matching_ranges() {
        let (_dir, root) = fixture();
        // A multi-chunk text file so a same-length single-line edit leaves the
        // earlier chunk ranges byte-identical.
        let long = root.join("notes.txt");
        let mut body = String::new();
        for i in 0..400 {
            body.push_str(&format!("filler line {i:04} keeps the chunk boundary\n"));
        }
        std::fs::write(&long, &body).unwrap();
        let pool = Arc::new(ModelPool::new(None));
        let live = LiveIndex::open(&root, ModelChoice::Code, pool.clone()).unwrap();
        let a_before = file_data(&live, "a.rs");
        let b_before = file_data(&live, "b.rs");
        let b_chunks_before = file_chunks(&live, "b.rs");
        let long_before = file_chunks(&live, "notes.txt");
        assert!(
            long_before.len() >= 2,
            "fixture text must split into several chunks"
        );
        let encoded_before = live.stats().encoded_texts;
        let edited = body.replace("filler line 0399", "filler line 9400");
        assert_ne!(edited, body);
        assert_eq!(edited.len(), body.len());
        std::fs::write(&long, &edited).unwrap();
        let stats = live.refresh(std::slice::from_ref(&long), false).unwrap();
        assert_eq!(stats.changed_files, 1);
        assert!(stats.encoded_passages < long_before.len());
        assert_eq!(
            live.stats().encoded_texts,
            encoded_before + stats.encoded_passages as u64
        );
        // Unchanged files keep the very same `Arc<FileData>` and chunk rows.
        assert!(Arc::ptr_eq(&a_before, &file_data(&live, "a.rs")));
        assert!(Arc::ptr_eq(&b_before, &file_data(&live, "b.rs")));
        let b_chunks_after = file_chunks(&live, "b.rs");
        for (before, after) in b_chunks_before.iter().zip(&b_chunks_after) {
            assert!(Arc::ptr_eq(before, after));
        }
        // The edited file reuses every range whose content did not change.
        let long_after = file_chunks(&live, "notes.txt");
        let shared = long_before
            .iter()
            .zip(&long_after)
            .filter(|(before, after)| Arc::ptr_eq(before, after))
            .count();
        assert!(shared >= 1, "no unchanged range was reused");
        assert!(
            shared < long_after.len(),
            "the edited range must be rebuilt"
        );
        equal_fresh(&root, &live, &pool);
    }

    fn cache_place() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.wgsnap");
        (dir, path)
    }

    fn rows(
        live: &LiveIndex,
        rel: &str,
    ) -> Vec<(String, usize, usize, String, HashMap<String, usize>)> {
        file_data(live, rel)
            .chunks
            .iter()
            .map(|chunk| {
                (
                    chunk.path.to_string(),
                    chunk.start,
                    chunk.end,
                    chunk.content.to_string(),
                    chunk.counts.clone(),
                )
            })
            .collect()
    }

    fn vectors(live: &LiveIndex, rel: &str) -> Vec<Vec<f32>> {
        file_data(live, rel)
            .vectors
            .iter()
            .map(|vector| vector.as_slice().to_vec())
            .collect()
    }

    #[test]
    fn snapshot_cache_roundtrip_preserves_rows_vectors_and_queries() {
        let (_dir, root) = fixture();
        let (_cache_dir, cache) = cache_place();
        let first = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        assert!(first.stats().encoded_texts > 0);
        assert!(cache.is_file(), "a first open must write the cache");
        let rows_before: Vec<_> = ["a.rs", "b.rs"].map(|rel| rows(&first, rel)).to_vec();
        let vectors_before: Vec<_> = ["a.rs", "b.rs"].map(|rel| vectors(&first, rel)).to_vec();
        let queries_before: Vec<_> = ["useful value", "other value", "renamed symbol"]
            .iter()
            .map(|query| serde_json::to_value(first.query(query, 10, true).unwrap()).unwrap())
            .collect();
        drop(first);

        let second = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache),
        )
        .unwrap();
        assert_eq!(second.stats().encoded_texts, 0);
        for (i, rel) in ["a.rs", "b.rs"].iter().enumerate() {
            assert_eq!(rows(&second, rel), rows_before[i]);
            assert_eq!(vectors(&second, rel), vectors_before[i]);
        }
        for (i, query) in ["useful value", "other value", "renamed symbol"]
            .iter()
            .enumerate()
        {
            assert_eq!(
                serde_json::to_value(second.query(query, 10, true).unwrap()).unwrap(),
                queries_before[i]
            );
        }
    }

    fn header_positions(data: &[u8]) -> (usize, usize, usize) {
        let u32_at = |at: &mut usize| {
            let value = u32::from_le_bytes(data[*at..*at + 4].try_into().unwrap()) as usize;
            *at += 4;
            value
        };
        let field = |at: &mut usize| {
            let len = u32_at(at);
            *at += len;
        };
        let mut at = 0;
        at += 8; // magic
        at += 4; // version
        field(&mut at);
        field(&mut at);
        at += 8; // generation
        at += 4; // file count
        at += 4; // chunk count
        at += 8; // meta length
        let text_base = u64::from_le_bytes(data[at..at + 8].try_into().unwrap()) as usize;
        at += 8;
        let text_len_at = at;
        at += 8;
        let vector_base = u64::from_le_bytes(data[at..at + 8].try_into().unwrap()) as usize;
        (text_base, text_len_at, vector_base)
    }

    #[test]
    fn snapshot_cache_serves_text_from_the_mapping() {
        let (_dir, root) = fixture();
        let (_cache_dir, cache) = cache_place();
        let first = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        let first_report = first.memory_report();
        assert_eq!(first_report.heap_texts, 0, "a fresh fold must map all text");
        assert_eq!(first_report.mapped_texts as usize, first_report.chunks);
        assert!(first_report.mapped_text_bytes > 0);
        assert_eq!(first_report.content_bytes, 0);
        let expected: Vec<_> = ["a.rs", "b.rs"].map(|rel| rows(&first, rel)).to_vec();
        drop(first);

        let second = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache),
        )
        .unwrap();
        let report = second.memory_report();
        assert_eq!(second.stats().encoded_texts, 0);
        assert_eq!(report.heap_texts, 0, "a cache hit must not copy text");
        assert_eq!(report.mapped_texts as usize, report.chunks);
        for (i, rel) in ["a.rs", "b.rs"].iter().enumerate() {
            assert_eq!(rows(&second, rel), expected[i]);
        }
    }

    #[test]
    fn a_fold_maps_edited_text_and_a_single_edit_keeps_only_its_text_on_the_heap() {
        let (_dir, root) = fixture();
        let (_cache_dir, cache) = cache_place();
        let live = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache),
        )
        .unwrap();
        assert_eq!(live.memory_report().heap_texts, 0);

        let edited = root.join("a.rs");
        std::fs::write(&edited, "fn alpha() {\n    let changed_value = 9;\n}\n").unwrap();
        let stats = live.refresh(&[edited.clone()], false).unwrap();
        assert_eq!(stats.changed_files, 1);
        let report = live.memory_report();
        assert!(report.heap_texts > 0, "an edit must own its text until a fold");
        assert!(report.mapped_texts > 0);
        assert_eq!(report.heap_texts + report.mapped_texts, report.chunks as u64);

        // A new file changes the layout, so the next reconcile folds.
        std::fs::write(root.join("c.rs"), "fn gamma() {\n    let c_value = 3;\n}\n").unwrap();
        live.refresh(&[], true).unwrap();
        let report = live.memory_report();
        assert_eq!(report.heap_texts, 0, "a fold must move edited text to the map");
        assert_eq!(report.mapped_texts as usize, report.chunks);
    }

    #[test]
    fn corrupt_snapshot_text_is_rejected_and_rebuilt() {
        let (_dir, root) = fixture();
        let (_cache_dir, cache) = cache_place();
        let first = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        assert!(first.stats().encoded_texts > 0);
        drop(first);

        let valid = std::fs::read(&cache).unwrap();
        let (_text_base, text_len_at, _) = header_positions(&valid);

        // An offset outside the text section must reject the whole cache.
        let mut out_of_range = valid.clone();
        out_of_range[text_len_at..text_len_at + 8]
            .copy_from_slice(&(valid.len() as u64 * 2).to_le_bytes());
        std::fs::write(&cache, &out_of_range).unwrap();
        let rebuilt = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        assert!(rebuilt.stats().encoded_texts > 0);
        assert_eq!(rebuilt.stats().files, 2);
        drop(rebuilt);

        // Invalid UTF-8 in the first chunk's mapped bytes must also rebuild.
        let valid = std::fs::read(&cache).unwrap();
        let (text_base, _, _) = header_positions(&valid);
        let mut bad_utf8 = valid.clone();
        bad_utf8[text_base] = 0xFF;
        std::fs::write(&cache, &bad_utf8).unwrap();
        let rebuilt = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        assert!(rebuilt.stats().encoded_texts > 0);
        assert_eq!(rebuilt.stats().files, 2);
        drop(rebuilt);

        let repaired = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache),
        )
        .unwrap();
        assert_eq!(repaired.stats().encoded_texts, 0);
    }

    #[test]
    fn mapped_snapshot_can_be_renamed_on_this_platform() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from.wgsnap");
        let to = dir.path().join("to.wgsnap");
        std::fs::write(&from, b"mapped bytes").unwrap();
        let file = File::open(&from).unwrap();
        let map = unsafe { Mmap::map(&file).unwrap() };
        // A fold maps the text section, then appends the vector table through
        // the same handle and renames the file.
        let mut writer = std::fs::OpenOptions::new().append(true).open(&from).unwrap();
        writer.write_all(b" more").unwrap();
        writer.sync_all().unwrap();
        std::fs::rename(&from, &to).unwrap();
        assert_eq!(&map[..], b"mapped bytes");
        assert_eq!(std::fs::read(&to).unwrap(), b"mapped bytes more");
        assert!(to.is_file());
    }

    #[test]
    fn map_then_append_keeps_the_file_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seq.wgsnap");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        {
            let mut writer = BufWriter::new(&file);
            writer.write_all(&vec![7u8; 364]).unwrap();
            writer.flush().unwrap();
        }
        let _map = unsafe { Mmap::map(&file).unwrap() };
        {
            let mut same = &file;
            same.seek(std::io::SeekFrom::Start(364)).unwrap();
        }
        {
            let mut writer = BufWriter::new(&file);
            writer.write_all(&vec![9u8; 2048]).unwrap();
            writer.flush().unwrap();
            writer.get_ref().sync_all().unwrap();
        }
        assert_eq!(file.metadata().unwrap().len(), 2412);
    }

    #[test]
    fn snapshot_cache_rebuilds_only_a_stale_file_and_then_updates() {
        let (_dir, root) = fixture();
        let (_cache_dir, cache) = cache_place();
        let first = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        assert!(first.stats().encoded_texts > 0);
        drop(first);

        std::fs::write(
            root.join("a.rs"),
            "fn alpha() {\n    let renamed_symbol = 3;\n    let extra_value = 4;\n}\n",
        )
        .unwrap();
        let second = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        let rebuilt = file_data(&second, "a.rs").chunks.len() as u64;
        assert!(rebuilt > 0);
        assert_eq!(second.stats().encoded_texts, rebuilt);
        assert!(second.stats().encoded_texts < second.stats().chunks as u64);
        drop(second);

        // The reconcile that rebuilt `a.rs` rewrote the cache. A third open is
        // therefore a pure hit.
        let third = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache),
        )
        .unwrap();
        assert_eq!(third.stats().encoded_texts, 0);
        assert_eq!(third.stats().files, 2);
    }

    #[test]
    fn corrupt_or_truncated_snapshot_cache_falls_back_to_a_full_build() {
        let (_dir, root) = fixture();
        let (_cache_dir, cache) = cache_place();

        std::fs::write(&cache, b"not a snapshot").unwrap();
        let corrupt = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        assert!(corrupt.stats().encoded_texts > 0);
        assert_eq!(corrupt.stats().files, 2);
        drop(corrupt);

        // The failed load caused one valid rewrite.
        let valid = std::fs::read(&cache).unwrap();
        std::fs::write(&cache, &valid[..valid.len() / 2]).unwrap();
        let truncated = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        assert!(truncated.stats().encoded_texts > 0);
        assert_eq!(truncated.stats().files, 2);
        drop(truncated);

        let repaired = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache),
        )
        .unwrap();
        assert_eq!(repaired.stats().encoded_texts, 0);
    }

    #[test]
    fn snapshot_cache_for_another_model_or_root_is_ignored() {
        let (_dir, root) = fixture();
        let (_cache_dir, cache) = cache_place();
        let first = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        assert!(first.stats().encoded_texts > 0);
        drop(first);

        let wrong_model = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Text,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        assert!(wrong_model.stats().encoded_texts > 0);
        assert_eq!(wrong_model.stats().model_revision, TEXT.revision);
        drop(wrong_model);

        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        let wrong_root = LiveIndex::open_with_cache(
            other.path(),
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache),
        )
        .unwrap();
        assert!(wrong_root.stats().encoded_texts > 0);
        assert_eq!(wrong_root.stats().files, 1);
    }

    #[test]
    fn warm_snapshot_cache_is_materially_faster_than_a_fresh_build() {
        let dir = tempfile::tempdir().unwrap();
        // Enough files and chunks that embedding dominates the fresh build.
        for file in 0..120 {
            let mut body = String::new();
            for line in 0..90 {
                body.push_str(&format!(
                    "fn helper_{file}_{line}() {{ let value_{line} = {line}; }}\n"
                ));
            }
            std::fs::write(dir.path().join(format!("f{file:03}.rs")), body).unwrap();
        }
        let root = dir.path().to_path_buf();
        let (_cache_dir, cache) = cache_place();
        let fresh = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache.clone()),
        )
        .unwrap();
        let fresh_ms = fresh.stats().build_ms;
        assert!(fresh.stats().encoded_texts > 0);
        drop(fresh);

        let warm = LiveIndex::open_with_cache(
            &root,
            ModelChoice::Code,
            Arc::new(ModelPool::new(None)),
            Some(cache),
        )
        .unwrap();
        assert_eq!(warm.stats().encoded_texts, 0);
        assert!(
            warm.stats().build_ms < fresh_ms,
            "cached build {}ms was not faster than fresh {}ms",
            warm.stats().build_ms,
            fresh_ms
        );
        if fresh_ms >= 20 {
            // A warm open still reads and hashes every file and rebuilds word
            // counts, so this small fixture is not the whole win. The VPS
            // receipt records the larger end-to-end gain; here we only require
            // a clear improvement and zero re-encoding.
            assert!(
                warm.stats().build_ms * 10 <= fresh_ms * 9,
                "cached build {}ms was not at least 10% faster than fresh {}ms",
                warm.stats().build_ms,
                fresh_ms
            );
        }
    }

    #[test]
    fn model_pool_shares_models_and_auto_reembeds_on_choice_change() {
        let (_dir, root) = fixture();
        std::fs::rename(root.join("a.rs"), root.join("a.md")).unwrap();
        std::fs::rename(root.join("b.rs"), root.join("b.md")).unwrap();
        let pool = Arc::new(ModelPool::new(None));
        let one = LiveIndex::open(&root, ModelChoice::Auto, pool.clone()).unwrap();
        let generation = one.stats().generation;
        let two = LiveIndex::open(&root, ModelChoice::Auto, pool.clone()).unwrap();
        assert_eq!(pool.loaded_count(), 1);
        assert_eq!(one.stats().model_revision, TEXT.revision);
        std::fs::rename(root.join("a.md"), root.join("a.rs")).unwrap();
        let update = one.refresh(&[], true).unwrap();
        assert_eq!(one.stats().model_revision, CODE.revision);
        assert_eq!(one.stats().generation, generation + 1);
        assert_eq!(update.encoded_passages, one.stats().chunks);
        assert_eq!(two.stats().model_revision, TEXT.revision);
        assert_eq!(pool.loaded_count(), 2);
    }
}
