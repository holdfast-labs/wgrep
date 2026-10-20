//! Pinned static-embedding assets: download, integrity, load, embed.
//!
//! The model class is small CPU static embeddings (one learned vector per
//! token, 256 dims): query and chunk text are tokenized with the model's
//! own tokenizer, token vectors are mean-pooled (skipping `[UNK]`) and
//! L2-normalized. Cosine similarity is then a dot product. There is no
//! transformer pass at search time and no network call per query.
//!
//! Assets cross the network exactly once (first use, or explicit refresh)
//! and every file is verified against its pinned SHA-256 before use.
//! `WSEM_OFFLINE=1` (like the reference's `HF_HUB_OFFLINE=1`) forbids all
//! downloads; a missing asset is then an error, never a silent fallback.

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::DIM;

pub(crate) struct ModelPin {
    pub repo: &'static str,
    pub revision: &'static str,
    pub rows: usize,
    /// Tokenizer asset (name, size, sha256).
    pub tok_name: &'static str,
    pub tok_size: u64,
    pub tok_sha: &'static str,
    /// Weight asset (name, size, sha256).
    pub w_name: &'static str,
    pub w_size: u64,
    pub w_sha: &'static str,
    pub f16: bool,
}

/// Code-distilled static model (MIT). Pinned exactly as the reference uses.
pub(crate) const CODE: ModelPin = ModelPin {
    repo: "minishlab/potion-code-16M-v2",
    revision: "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
    rows: 63457,
    tok_name: "tokenizer.json",
    tok_size: 1_024_340,
    tok_sha: "107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45",
    w_name: "model.safetensors",
    w_size: 32_490_072,
    w_sha: "75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c",
    f16: true,
};

/// General-English static model (MIT). NOT loaded in the code-only MVP:
/// mixing TEXT query vectors with a CODE corpus (or vice versa) is a
/// silent embedding-space mismatch (review blocker 1). Kept pinned so a
/// future dual-corpus change has the verified pins at hand.
#[allow(dead_code)]
pub(crate) const TEXT: ModelPin = ModelPin {
    repo: "minishlab/potion-base-8M",
    revision: "bf8b056651a2c21b8d2565580b8569da283cab23",
    rows: 29528,
    tok_name: "tokenizer.json",
    tok_size: 683_666,
    tok_sha: "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50",
    w_name: "model.safetensors",
    w_size: 30_236_760,
    w_sha: "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2",
    f16: false,
};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Asset cache root: `WSEM_CACHE_DIR`, else platform cache `wgrep-sem`.
pub(crate) fn cache_dir(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(d) = explicit {
        return Ok(d.to_path_buf());
    }
    if let Some(d) = std::env::var_os("WSEM_CACHE_DIR") {
        return Ok(PathBuf::from(d));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }
    .ok_or_else(|| anyhow::anyhow!("no cache directory; set WSEM_CACHE_DIR"))?;
    Ok(base.join("wgrep-sem"))
}

fn asset_path(cache: &Path, pin: &ModelPin, name: &str) -> PathBuf {
    let repo = pin.repo.replace('/', "--");
    cache.join("models").join(format!("{repo}--{}.{}", pin.revision, "")).join(name)
}

/// Fetch one asset with size + SHA-256 verification and atomic publish.
/// An intact cached file is reused without network. `WSEM_OFFLINE=1`
/// forbids downloads outright.
///
/// Multi-process safety: a `.lock` file (created with `create_new`)
/// serializes concurrent fetchers of the same asset; losers wait (bounded)
/// for the winner's verified publish instead of racing partial `.part`
/// files. Temp files are unique per process (pid + counter). Streaming
/// aborts early past the pinned size, and the HTTP call has a bounded
/// deadline — a hung mirror cannot hang startup forever.
fn fetch(cache: &Path, pin: &ModelPin, name: &str, size: u64, sha: &str) -> anyhow::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let dest = asset_path(cache, pin, name);
    if verified(&dest, size, sha) {
        return Ok(dest);
    }
    if std::env::var_os("WSEM_OFFLINE").is_some() {
        anyhow::bail!("wgrep-sem: {name} for {} not cached and WSEM_OFFLINE=1", pin.repo);
    }
    let dir = dest.parent().unwrap();
    std::fs::create_dir_all(dir)?;
    let lock = dir.join(format!(".{name}.lock"));
    // Bounded wait for a concurrent fetcher (600 s covers the ~32 MB
    // weights on a slow link; then we try ourselves).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        if verified(&dest, size, sha) {
            return Ok(dest);
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // A crashed fetcher leaves its lock behind: treat locks
                // older than the wait budget as stale and take over.
                let stale = std::fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > std::time::Duration::from_secs(600));
                if stale {
                    let _ = std::fs::remove_file(&lock);
                    continue;
                }
                if std::time::Instant::now() > deadline {
                    anyhow::bail!("wgrep-sem: timed out waiting for concurrent {name} fetch");
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(e) => return Err(e.into()),
        }
    }
    let _lock_guard = LockGuard { path: lock };
    if verified(&dest, size, sha) {
        return Ok(dest);
    }
    let id = CTR.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{id}.part", std::process::id()));
    let url = format!(
        "https://huggingface.co/{}/resolve/{}/{}",
        pin.repo, pin.revision, name
    );
    eprintln!("wgrep-sem: downloading {url}");
    let result: anyhow::Result<()> = (|| {
        // ureq 3 carries timeouts on the request config (no per-request
        // `.timeout` builder): the global deadline covers connect +
        // headers + body so a hung mirror cannot hang startup forever.
        let mut resp = ureq::get(&url)
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(300)))
            .build()
            .call()
            .map_err(|e| anyhow::anyhow!("wgrep-sem: download failed for {name}: {e}"))?;
        let mut out = std::fs::File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut buf = vec![0u8; 1 << 20];
        let mut body = resp.body_mut().as_reader();
        loop {
            let n = std::io::Read::read(&mut body, &mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > size {
                anyhow::bail!("wgrep-sem: {name} already exceeds pinned size {size}; aborting");
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        drop(out);
        if total != size {
            anyhow::bail!("wgrep-sem: {name} size {total} != pinned {size}");
        }
        let digest = hex(&hasher.finalize());
        if digest != sha {
            anyhow::bail!("wgrep-sem: {name} sha mismatch (got {digest})");
        }
        std::fs::rename(&tmp, &dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(dest)
}

fn verified(path: &Path, size: u64, sha: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else { return false };
    bytes.len() as u64 == size && hex(&Sha256::digest(&bytes)) == sha
}

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Loaded model: tokenizer plus the full embedding matrix as f32.
/// The matrix (~65 MB for the code model) is loaded once per resident
/// daemon and shared by all clients — never once per query or per agent.
pub(crate) struct ModelCtx {
    pub pin: &'static ModelPin,
    pub tokenizer: tokenizers::Tokenizer,
    pub matrix: Vec<f32>,
}

impl ModelCtx {
    pub(crate) fn load(pin: &'static ModelPin, cache: &Path) -> anyhow::Result<ModelCtx> {
        let tok_path = fetch(cache, pin, pin.tok_name, pin.tok_size, pin.tok_sha)?;
        let w_path = fetch(cache, pin, pin.w_name, pin.w_size, pin.w_sha)?;
        let tok_bytes = std::fs::read(&tok_path)?;
        let mut tokenizer = tokenizers::Tokenizer::from_bytes(&tok_bytes)
            .map_err(|e| anyhow::anyhow!("wgrep-sem: bad tokenizer: {e}"))?;
        tokenizer.with_truncation(None).map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer.with_padding(None);
        let w_bytes = std::fs::read(&w_path)?;
        let tensors = safetensors::SafeTensors::deserialize(&w_bytes)
            .map_err(|e| anyhow::anyhow!("wgrep-sem: bad safetensors: {e}"))?;
        let t = tensors
            .tensor("embeddings")
            .map_err(|e| anyhow::anyhow!("wgrep-sem: no embeddings tensor: {e}"))?;
        let want_dtype = if pin.f16 {
            safetensors::Dtype::F16
        } else {
            safetensors::Dtype::F32
        };
        if t.dtype() != want_dtype || t.shape() != [pin.rows, DIM] {
            anyhow::bail!("wgrep-sem: unexpected embeddings tensor shape/dtype");
        }
        let raw = t.data();
        let width = if pin.f16 { 2 } else { 4 };
        let mut matrix = Vec::with_capacity(pin.rows * DIM);
        for cell in raw.chunks_exact(width) {
            matrix.push(if pin.f16 {
                half::f16::from_le_bytes(cell.try_into().unwrap()).to_f32()
            } else {
                f32::from_le_bytes(cell.try_into().unwrap())
            });
        }
        Ok(ModelCtx { pin, tokenizer, matrix })
    }

    /// Tokenize `texts` to id lists with `[UNK]` dropped, in order.
    /// Shared by `embed` so the fidelity export observes the exact ids
    /// the rank path pools (no duplicated tokenization logic to drift).
    ///
    /// Strictly SEQUENTIAL: one `encode_fast` per text, never the
    /// `encode_batch_fast` rayon pool. The daemon's only configured
    /// compute budget is its rank-worker count; a nested tokenizer pool
    /// would oversubscribe it (4 rank workers × N rayon threads) and break
    /// the measured thread bound. No batch tokenize API may be called
    /// anywhere in this tree (see the resource contract in `daemon.rs`).
    pub(crate) fn tokenize_ids(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<u32>>> {
        let unknown = self.tokenizer.token_to_id("[UNK]");
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let enc = self
                .tokenizer
                .encode_fast(text.as_str(), false)
                .map_err(|e| anyhow::anyhow!("wgrep-sem: tokenize failed: {e}"))?;
            out.push(enc.get_ids().iter().copied().filter(|id| Some(*id) != unknown).collect());
        }
        Ok(out)
    }

    /// Embed `texts` (first is usually the query) into L2-normalized
    /// 256-dim vectors: tokenize, look up, mean-pool over non-`[UNK]`
    /// ids, normalize. Mirrors the reference math exactly.
    pub(crate) fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let id_lists = self.tokenize_ids(texts)?;
        let mut out = Vec::with_capacity(texts.len());
        for ids in &id_lists {
            let mut v = vec![0.0f32; DIM];
            for id in ids {
                let row = (*id as usize)
                    .checked_mul(DIM)
                    .filter(|o| *o + DIM <= self.matrix.len())
                    .ok_or_else(|| anyhow::anyhow!("wgrep-sem: token outside model vocabulary"))?;
                for (x, w) in v.iter_mut().zip(&self.matrix[row..row + DIM]) {
                    *x += *w;
                }
            }
            if !ids.is_empty() {
                let n = ids.len() as f32;
                for x in &mut v {
                    *x /= n;
                }
            }
            let norm = v.iter().map(|x| *x * *x).sum::<f32>().sqrt() + 1e-32;
            for x in &mut v {
                *x /= norm;
            }
            if !v.iter().all(|x| x.is_finite()) {
                anyhow::bail!("wgrep-sem: non-finite embedding");
            }
            out.push(v);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fidelity export for `eval/bench/WGREP_STATIC_FIDELITY.py --verify
    /// --rust`: embeds the fixture's `cases[].text` IN ORDER through the
    /// real `ModelCtx::embed` and writes `{"vectors": [...],
    /// "token_ids": [...]}`. Token ids are asserted EXACT here (same
    /// `tokenize_ids` path the ranker pools: sequential `encode_fast` with
    /// `add_special_tokens=false`, `[UNK]` filtered); the L2 <= 1e-5 leg
    /// runs in the harness (it owns the goldens).
    ///
    /// Test-only hook by design (no production CLI surface): `#[ignore]`
    /// so a normal `cargo test` never needs model assets, network, or any
    /// `WGREP_FIDELITY_*` env (ignored tests don't run by default).
    /// Run on VPS with the verified asset cache (offline-safe):
    /// `WSEM_CACHE_DIR=<cache> WSEM_OFFLINE=1
    ///  WGREP_FIDELITY_FIXTURE=<fixture> WGREP_FIDELITY_OUT=<rust.json>
    ///  cargo test --release --features semantic --bin wgrep-sem
    ///  -- --ignored sem::model::tests::fidelity_export`
    /// (full test path, no `--exact`: a bare `fidelity_export -- --exact`
    /// selects zero tests. Assert `1 passed` and a fresh `rust.json`
    /// mtime — `0 passed` with exit 0 is a silent skip, not a pass.)
    #[test]
    #[ignore]
    fn fidelity_export() {
        // Contract names owned by the fidelity harness (WGREP_*); the
        // older WSEM_* names from the first coordination note still work
        // as fallback.
        let fixture_path = std::env::var("WGREP_FIDELITY_FIXTURE")
            .or_else(|_| std::env::var("WSEM_FIDELITY_FIXTURE"))
            .expect("WGREP_FIDELITY_FIXTURE must point at WGREP_STATIC_FIDELITY_FIXTURE.json");
        let out_path = std::env::var("WGREP_FIDELITY_OUT")
            .or_else(|_| std::env::var("WSEM_RUST_OUT"))
            .unwrap_or_else(|_| "rust.json".to_owned());
        let fixture: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&fixture_path).expect("read fixture"),
        )
        .expect("parse fixture");
        let cases = fixture.get("cases").and_then(|v| v.as_array()).expect("cases array");
        let texts: Vec<String> = cases
            .iter()
            .map(|c| c.get("text").and_then(|v| v.as_str()).unwrap_or("").to_owned())
            .collect();
        let want_ids: Vec<Vec<u64>> = cases
            .iter()
            .map(|c| {
                c.get("token_ids")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().map(|x| x.as_u64().unwrap()).collect())
                    .unwrap_or_default()
            })
            .collect();

        let cache = cache_dir(None).expect("cache dir");
        let model = ModelCtx::load(&CODE, &cache).expect("load CODE model");
        let got_ids = model.tokenize_ids(&texts).expect("tokenize");
        for (i, (got, want)) in got_ids.iter().zip(&want_ids).enumerate() {
            let got64: Vec<u64> = got.iter().map(|&x| u64::from(x)).collect();
            assert_eq!(got64, *want, "case {i}: token ids differ from fixture");
        }
        let vecs = model.embed(&texts).expect("embed");
        assert_eq!(vecs.len(), cases.len());
        let out = serde_json::json!({ "vectors": vecs, "token_ids": got_ids });
        std::fs::write(&out_path, serde_json::to_string(&out).expect("serialize")).expect("write rust.json");
        eprintln!("wgrep-sem: fidelity export wrote {} vectors to {out_path}", vecs.len());
    }
}
