//! Mutable, atomically published sgrep-compatible corpus snapshots.
use crate::{
    CODE, Chunk, LoadedModel, ModelChoice, Result, TEXT, bm25_borrowed, cache, identifier_pattern,
    is_prose, make_chunk, rank_impl, words,
};
use rayon::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// Snapshot cache identity and bounds. Bump `SNAPSHOT_VERSION` when the chunk
/// ranges or vector layout change, not merely when unrelated ranking changes.
const SNAPSHOT_MAGIC: &[u8; 8] = b"WGSNAP\0\0";
const SNAPSHOT_VERSION: u32 = 1;
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

/// One source file's immutable chunks and their passage vectors.
///
/// Both lists are `Arc`-shared. An unchanged file therefore survives a refresh
/// as one `Arc` clone, with no content, word-map or embedding copy. The vectors
/// stay valid only for the model revision that produced them.
struct FileData {
    digest: [u8; 32],
    chunks: Vec<Arc<Chunk>>,
    vectors: Vec<Arc<Vec<f32>>>,
}

/// One atomically published corpus state.
///
/// `files` maps each path to its shared [`FileData`]. `chunks` and `vectors`
/// are flat, index-parallel views that share the same rows, so a one-file edit
/// copies only pointers into the new flat views, never content.
struct Snapshot {
    files: HashMap<PathBuf, Arc<FileData>>,
    chunks: Vec<Arc<Chunk>>,
    vectors: Vec<Arc<Vec<f32>>>,
    model: Arc<LoadedModel>,
    generation: u64,
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
    Rebuilt(PathBuf, FileData, Vec<Option<Arc<Vec<f32>>>>),
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

fn read_u32(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_field(reader: &mut impl Read, cap: usize) -> Result<Vec<u8>> {
    let len = read_u32(reader)? as usize;
    if len > cap {
        return Err("snapshot cache field exceeds bound".into());
    }
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
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

/// Load a v1 snapshot cache into a partial [`Snapshot`].
///
/// The cache stores only chunk line ranges, file digests, and passage vectors.
/// Each entry is accepted only when its digest still matches the file. Chunk
/// content and word counts are rebuilt with the same normalization and
/// `make_chunk` call as [`prepare_file`], so a cache hit is identical to a
/// fresh build. Any header, bound, or record problem rejects the whole cache;
/// callers then do a full build.
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

struct CachedEntry {
    rel: PathBuf,
    digest: [u8; 32],
    ranges: Vec<(usize, usize)>,
    vectors: Vec<Arc<Vec<f32>>>,
}

/// Rebuild one cached file from the live bytes. The cached ranges are applied
/// with the same line slicing and `make_chunk` as a fresh build. A stale or
/// malformed entry returns `None`; the caller's reconcile rebuilds that file.
fn rebuild_cached_file(root: &Path, entry: &CachedEntry) -> Option<(PathBuf, Arc<FileData>)> {
    let bytes = std::fs::read(root.join(&entry.rel)).ok()?;
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if actual != entry.digest {
        return None;
    }
    if bytes.contains(&0) {
        if !entry.ranges.is_empty() {
            return None;
        }
        return Some((
            entry.rel.clone(),
            Arc::new(FileData {
                digest: entry.digest,
                chunks: Vec::new(),
                vectors: Vec::new(),
            }),
        ));
    }
    let source = normalize_source(&bytes);
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut rows = Vec::with_capacity(entry.ranges.len());
    for &(first, last) in &entry.ranges {
        if first < 1 || last < first || last > lines.len() {
            return None;
        }
        let content = lines[first - 1..last].concat();
        rows.push(Arc::new(make_chunk(
            entry.rel.to_string_lossy().into_owned(),
            first,
            last,
            content,
        )));
    }
    Some((
        entry.rel.clone(),
        Arc::new(FileData {
            digest: entry.digest,
            chunks: rows,
            vectors: entry.vectors.clone(),
        }),
    ))
}

fn load_snapshot_cache(path: &Path, root: &Path, model: &Arc<LoadedModel>) -> Result<Snapshot> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != SNAPSHOT_MAGIC {
        return Err("snapshot cache magic mismatch".into());
    }
    let version = read_u32(&mut reader)?;
    if version != SNAPSHOT_VERSION {
        return Err(format!("snapshot cache version {version} is unsupported").into());
    }
    let cached_root = read_field(&mut reader, SNAPSHOT_MAX_META)?;
    if cached_root != root.as_os_str().as_encoded_bytes() {
        return Err("snapshot cache root mismatch".into());
    }
    let cached_model = read_field(&mut reader, SNAPSHOT_MAX_META)?;
    if cached_model != model.revision().as_bytes() {
        return Err("snapshot cache model mismatch".into());
    }
    let file_count = read_u32(&mut reader)? as usize;
    let total_chunks = read_u32(&mut reader)? as usize;
    if file_count > SNAPSHOT_MAX_FILES || total_chunks > SNAPSHOT_MAX_CHUNKS {
        return Err("snapshot cache header count exceeds bound".into());
    }
    let mut entries: Vec<CachedEntry> = Vec::with_capacity(file_count.min(4096));
    let mut paths: HashSet<PathBuf> = HashSet::with_capacity(file_count.min(4096));
    let mut seen_chunks = 0usize;
    for _ in 0..file_count {
        let rel = path_from_bytes(read_field(&mut reader, SNAPSHOT_MAX_PATH)?);
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
        reader.read_exact(&mut digest)?;
        let chunk_count = read_u32(&mut reader)? as usize;
        if chunk_count > SNAPSHOT_MAX_CHUNKS
            || seen_chunks
                .checked_add(chunk_count)
                .is_none_or(|n| n > total_chunks)
        {
            return Err("snapshot cache chunk count exceeds header".into());
        }
        let mut ranges = Vec::with_capacity(chunk_count.min(1024));
        for _ in 0..chunk_count {
            let start = read_u32(&mut reader)? as usize;
            let end = read_u32(&mut reader)? as usize;
            ranges.push((start, end));
        }
        let mut vectors = Vec::with_capacity(chunk_count.min(1024));
        for _ in 0..chunk_count {
            let mut values = Vec::with_capacity(256);
            let mut raw = [0u8; 4];
            for _ in 0..256 {
                reader.read_exact(&mut raw)?;
                values.push(f32::from_le_bytes(raw));
            }
            vectors.push(Arc::new(values));
        }
        seen_chunks += chunk_count;
        entries.push(CachedEntry {
            rel,
            digest,
            ranges,
            vectors,
        });
    }
    if seen_chunks != total_chunks {
        return Err("snapshot cache chunk total mismatch".into());
    }
    // Rebuild the per-file rows on the worker pool. A stale or unreadable file
    // is simply absent from the partial state; the following reconcile
    // rebuilds it.
    let rebuilt = parallel_map(&entries, |entry| rebuild_cached_file(root, entry));
    let mut files = HashMap::with_capacity(rebuilt.len());
    for item in rebuilt.into_iter().flatten() {
        files.insert(item.0, item.1);
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
    for rel in ordered {
        let data = &files[&rel];
        chunks.extend(data.chunks.iter().cloned());
        vectors.extend(data.vectors.iter().cloned());
    }
    Ok(Snapshot {
        files,
        chunks,
        vectors,
        model: model.clone(),
        generation: 1,
    })
}

/// Write the v1 snapshot cache atomically. A failure is reported to the caller
/// but never changes the published index.
fn write_snapshot_cache(path: &Path, root: &Path, snapshot: &Snapshot) -> Result<()> {
    let total_chunks: usize = snapshot.files.values().map(|d| d.chunks.len()).sum();
    if snapshot.files.len() > SNAPSHOT_MAX_FILES || total_chunks > SNAPSHOT_MAX_CHUNKS {
        return Err("snapshot exceeds cache bounds".into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("snapshot");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut writer = BufWriter::new(File::create(&tmp)?);
        writer.write_all(SNAPSHOT_MAGIC)?;
        writer.write_all(&SNAPSHOT_VERSION.to_le_bytes())?;
        write_field(
            &mut writer,
            root.as_os_str().as_encoded_bytes(),
            SNAPSHOT_MAX_META,
        )?;
        write_field(
            &mut writer,
            snapshot.model.revision().as_bytes(),
            SNAPSHOT_MAX_META,
        )?;
        writer.write_all(&(snapshot.files.len() as u32).to_le_bytes())?;
        writer.write_all(&(total_chunks as u32).to_le_bytes())?;
        let mut ordered: Vec<&PathBuf> = snapshot.files.keys().collect();
        ordered.sort_unstable_by(|a, b| {
            a.as_os_str()
                .as_encoded_bytes()
                .cmp(b.as_os_str().as_encoded_bytes())
        });
        for rel in ordered {
            let data = &snapshot.files[rel];
            if data.chunks.len() != data.vectors.len() {
                return Err("snapshot cache row count mismatch".into());
            }
            write_field(
                &mut writer,
                rel.as_os_str().as_encoded_bytes(),
                SNAPSHOT_MAX_PATH,
            )?;
            writer.write_all(&data.digest)?;
            writer.write_all(&(data.chunks.len() as u32).to_le_bytes())?;
            for chunk in &data.chunks {
                if chunk.start > u32::MAX as usize || chunk.end > u32::MAX as usize {
                    return Err("snapshot cache range exceeds u32".into());
                }
                writer.write_all(&(chunk.start as u32).to_le_bytes())?;
                writer.write_all(&(chunk.end as u32).to_le_bytes())?;
            }
            for vector in &data.vectors {
                if vector.len() != 256 {
                    return Err("snapshot cache vector width is not 256".into());
                }
                for value in vector.iter() {
                    writer.write_all(&value.to_le_bytes())?;
                }
            }
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&tmp, path)?;
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(error)
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
    let mut chunks = Vec::new();
    let mut reused = Vec::new();
    for (first, last) in ranges {
        let assemble = Instant::now();
        let content = lines[first - 1..last].concat();
        // Reuse the previous immutable chunk when this range kept the same
        // bounds and content; then its vector still applies.
        let hit = previous.and_then(|data| {
            data.chunks
                .iter()
                .position(|c| c.start == first && c.end == last && c.content == content)
        });
        BuildTimings::add(&timings.chunk_ns, assemble);
        match hit {
            Some(i) => {
                let data = previous.unwrap();
                chunks.push(data.chunks[i].clone());
                reused.push(data.vectors.get(i).cloned());
            }
            None => {
                let counts = Instant::now();
                let chunk = make_chunk(rel.to_string_lossy().into_owned(), first, last, content);
                BuildTimings::add(&timings.counts_ns, counts);
                chunks.push(Arc::new(chunk));
                reused.push(None);
            }
        }
    }
    Ok(PreparedFile::Rebuilt(
        rel.clone(),
        FileData {
            digest,
            chunks,
            vectors: Vec::new(),
        },
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
            match load_snapshot_cache(&path, &index.root, &initial_model) {
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
        index.refresh(&[], true)?;
        if std::env::var_os("WG_LIVE_TIMING").is_some() {
            eprintln!("wg live: open_total={}ms", started.elapsed().as_millis());
        }
        index.build_ms = (started.elapsed().as_millis() as u64).saturating_sub(model_ms);
        Ok(index)
    }

    pub fn root(&self) -> &Path {
        &self.root
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
        let mut reused_vectors: HashMap<PathBuf, Vec<Option<Arc<Vec<f32>>>>> = HashMap::new();
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
        let prepared: Vec<Result<PreparedFile>> = selected
            .par_iter()
            .map(|rel| prepare_file(&self.root, rel, &old.files, &timings))
            .collect();
        for item in prepared {
            match item? {
                PreparedFile::Removed(rel) => {
                    if files.remove(&rel).is_some() {
                        changed_paths.insert(rel);
                    }
                }
                PreparedFile::Unchanged => {}
                PreparedFile::Empty(rel, digest) => {
                    changed_paths.insert(rel.clone());
                    files.insert(
                        rel,
                        Arc::new(FileData {
                            digest,
                            chunks: Vec::new(),
                            vectors: Vec::new(),
                        }),
                    );
                }
                PreparedFile::Rebuilt(rel, data, reused) => {
                    changed_paths.insert(rel.clone());
                    reused_vectors.insert(rel.clone(), reused);
                    files.insert(rel, Arc::new(data));
                }
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
            if self.should_write_cache(reconcile, 0) {
                self.write_cache(&old);
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
        // Assemble the flat row views. Unchanged files contribute their existing
        // Arcs; changed files contribute rebuilt chunks and reuse a vector only
        // when the chunk Arc and the model revision both survived.
        let total: usize = files.values().map(|d| d.chunks.len()).sum();
        let mut chunks: Vec<Arc<Chunk>> = Vec::with_capacity(total);
        let mut slots: Vec<Option<Arc<Vec<f32>>>> = Vec::with_capacity(total);
        let mut pending: Vec<String> = Vec::new();
        let mut spans: Vec<(PathBuf, usize, usize)> = Vec::new();
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
                        pending.push(format!("{}\n{}", chunk.path, chunk.content));
                        slots.push(None);
                    }
                }
            }
            if switch || file_changed {
                spans.push((rel.clone(), start, chunks.len() - start));
            }
        }
        mark("assemble_rows", &mut phase);
        let encoded = pending.len();
        // One tokenizer call per passage, distributed over the worker pool.
        // Nested batch parallelism oversubscribed the 4-vCPU host.
        let filled: Vec<Arc<Vec<f32>>> = model
            .embed_parallel(&pending)?
            .into_iter()
            .map(Arc::new)
            .collect();
        mark("embed", &mut phase);
        let mut filled = filled.into_iter();
        let vectors: Vec<Arc<Vec<f32>>> = slots
            .into_iter()
            .map(|slot| slot.unwrap_or_else(|| filled.next().unwrap()))
            .collect();
        debug_assert!(filled.next().is_none());
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
        // Store each rebuilt file's vectors beside its chunks, so the next
        // refresh can share them again without any lookup.
        for (rel, start, len) in spans {
            let data = files.get(&rel).unwrap();
            files.insert(
                rel,
                Arc::new(FileData {
                    digest: data.digest,
                    chunks: data.chunks.clone(),
                    vectors: vectors[start..start + len].to_vec(),
                }),
            );
        }
        let next = Arc::new(Snapshot {
            files,
            chunks,
            vectors,
            model,
            generation: old.generation + 1,
        });
        *self
            .snapshot
            .write()
            .map_err(|_| "live index snapshot poisoned")? = next.clone();
        mark("publish", &mut phase);
        if self.should_write_cache(reconcile, changed) {
            self.write_cache(&next);
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

    /// A failed cache write never fails the index and never changes what a
    /// query sees. It is silent unless `WG_LIVE_TIMING` asks for diagnostics.
    fn write_cache(&self, snapshot: &Snapshot) {
        let Some(path) = self.cache.as_deref() else {
            return;
        };
        let mut phase = Instant::now();
        match write_snapshot_cache(path, &self.root, snapshot) {
            Ok(()) => self.cache_ready.store(true, Ordering::Relaxed),
            Err(error) => {
                if std::env::var_os("WG_LIVE_TIMING").is_some() {
                    eprintln!("wg live: cache_write_failed reason={error}");
                }
            }
        }
        mark("cache_write", &mut phase);
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
        // Score the borrowed `Arc` rows directly: no content or vector clone.
        let lexical = bm25_borrowed(
            snapshot.chunks.iter().map(|chunk| chunk.as_ref()),
            &terms,
            snapshot.chunks.len(),
        );
        let q = snapshot.model.embed(vec![query.to_string()])?.remove(0);
        let similarities: Vec<f64> = snapshot
            .vectors
            .iter()
            .map(|v| v.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>() as f64)
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
                    chunk.path.clone(),
                    chunk.start,
                    chunk.end,
                    chunk.content.clone(),
                    chunk.counts.clone(),
                )
            })
            .collect()
    }

    fn vectors(live: &LiveIndex, rel: &str) -> Vec<Vec<f32>> {
        file_data(live, rel)
            .vectors
            .iter()
            .map(|vector| vector.as_ref().clone())
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
