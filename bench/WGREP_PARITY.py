#!/usr/bin/env python3
"""Differential correctness harness: wgrep vs rg 15.1.0 byte-identical parity.

wgrep is a fork of ripgrep 15.1.0 that adds a trigram index to skip files
proven not to match. Its output must be byte-identical to plain `rg` for
every flag combination; the index may only remove files from the search, it
may never change how a file is searched. This harness drives both binaries
over several corpora with a large, reproducible (seeded) pattern x flag
matrix, and diffs stdout/stderr/exit code exactly (modulo the small set of
elapsed-time fields in --stats/--json output, which are normalized away).

Everything except this script lives under --work (default a fresh directory
on E:\\wgrep-parity, since C: is nearly full). Run with --run to execute the
suite (a plain --help/no-args run just prints usage).
"""
from __future__ import annotations

import argparse
import collections
import hashlib
import heapq
import itertools
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]  # C:\git\wispd
DP013_PARENT = ROOT / "eval" / "datasets" / "xafs" / "xAFS" / "dp_013"
DP013_DATA = DP013_PARENT / "data"

SEED = 0x77677265705F706172697479  # "wgrep_parity" as an int seed
TS = time.strftime("%Y%m%d_%H%M%S")

WORD_RE = re.compile(r"[A-Za-z][A-Za-z']{2,20}[A-Za-z]|[A-Za-z]{3}")
TIMING_STATS_RE = re.compile(rb"(?m)^\d+\.\d+ seconds")
# rg's JSON emits a `{secs,nanos,human}` Duration under two different key
# names ("elapsed" per-file, "elapsed_total" for the run) and, depending on
# the message type, in different field orders (observed both
# human/nanos/secs and secs/nanos/human). Match the whole object body
# non-greedily instead of a fixed key order; Duration objects never nest
# braces and the human string never contains one.
TIMING_JSON_RE = re.compile(rb'"(elapsed(?:_total)?)":\{[^{}]*\}')

# --------------------------------------------------------------------------
# Small deterministic PRNG helpers (stable across runs given the fixed SEED)
# --------------------------------------------------------------------------

def seeded_rng(*parts: str) -> random.Random:
    h = hashlib.sha256(f"{SEED}:{':'.join(parts)}".encode()).digest()
    return random.Random(int.from_bytes(h[:8], "big"))


def stable_sort_key(seed_tag: str):
    def key(word: str) -> str:
        return hashlib.sha256(f"{SEED}:{seed_tag}:{word}".encode()).hexdigest()
    return key


# --------------------------------------------------------------------------
# Corpus construction
# --------------------------------------------------------------------------

LOREM_BANK = [
    "forest", "harbor", "signal", "quartz", "meadow", "copper", "lantern", "gravel",
    "thicket", "ember", "willow", "granite", "cinder", "hollow", "ledger", "compass",
    "rivulet", "boulder", "timber", "frost", "canyon", "beacon", "drift", "marsh",
    "orchard", "pebble", "summit", "tunnel", "vapor", "wharf", "yonder", "zephyr",
    "anchor", "bramble", "cobalt", "dapple", "echo", "fable", "glimmer", "husk",
    "ivory", "jetty", "kernel", "lattice", "mosaic", "nectar", "opal", "prairie",
]


def lorem_sentence(rng: random.Random, n: int) -> str:
    weights = [1.0 / (i + 1) for i in range(len(LOREM_BANK))]
    words = rng.choices(LOREM_BANK, weights=weights, k=n)
    return " ".join(words)


def build_fixture(root: Path) -> None:
    """Generates the edge-case corpus described in the task: binary/NUL,
    UTF-16 with BOM, UTF-8 BOM, Kelvin sign / long s, non-ASCII case
    variants, CRLF, a >1MB line, empty file, hidden dirs, .gitignore /
    .ignore / .rgignore, deep nesting, spaces/non-ASCII names, invalid UTF-8.
    Also lays down ~50 plain lorem-ipsum files so there is a real word
    population to sample rare/mid/common patterns from.
    """
    root.mkdir(parents=True, exist_ok=True)
    rng = seeded_rng("fixture")

    # Plain text population for word sampling.
    for i in range(60):
        sub = root / "notes" / f"grp{i % 6}"
        sub.mkdir(parents=True, exist_ok=True)
        lines = [lorem_sentence(rng, rng.randint(4, 14)) for _ in range(rng.randint(10, 40))]
        (sub / f"note_{i:03d}.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")

    # Binary file with NUL bytes; match tokens before and after the NUL.
    (root / "binary_nul.bin").write_bytes(
        b"needle_before_nul_forest\x00\x01\x02needle_after_nul_harbor\x00\xff\xfe\x00tail\n"
    )

    # UTF-16LE / UTF-16BE with BOM, containing a searchable ASCII word.
    (root / "utf16le.txt").write_bytes("bmp_signal_word\r\nsecond line quartz\r\n".encode("utf-16-le"))
    (root / "utf16be.txt").write_bytes("bmp_signal_word\r\nsecond line quartz\r\n".encode("utf-16-be"))

    # UTF-8 BOM file.
    (root / "utf8bom.txt").write_bytes(b"\xef\xbb\xbf" + "bom_marker_meadow line two copper\n".encode("utf-8"))

    # Kelvin sign / long s inside words.
    (root / "unicode_fold.txt").write_text(
        "\u212Aelvin device reading and cla\u017F\u017F attendance sheet\n"
        "plain kelvin and class also appear in lowercase form\n",
        encoding="utf-8",
    )

    # Non-ASCII case variants: E-acute, sharp s / capital sharp s, sigma
    # forms, Turkish dotted/dotless I.
    (root / "non_ascii_case.txt").write_text(
        "caf\u00e9 vs CAF\u00c9 vs r\u00e9sum\u00e9\n"
        "stra\u00dfe vs STRA\u1e9e vs strasse\n"
        "\u03a3igma vs \u03c3igma vs \u03c2 final sigma\n"
        "\u0130stanbul vs istanbul vs \u0049stanbul vs \u0131cerik\n",
        encoding="utf-8",
    )

    # CRLF line endings.
    (root / "crlf.txt").write_bytes(b"first crlf line copper\r\nsecond crlf line drift\r\nthird line\r\n")

    # One very long line (>1MB).
    long_line = ("longline_marker_granite " + "filler_word_ " * 90000).strip()
    assert len(long_line) > 1_000_000
    (root / "long_line.txt").write_text(long_line + "\n", encoding="utf-8")

    # Empty file.
    (root / "empty.txt").write_bytes(b"")

    # Hidden dirs / hidden files.
    hidden_dir = root / ".hidden_dir"
    hidden_dir.mkdir(parents=True, exist_ok=True)
    (hidden_dir / "inside.txt").write_text("hidden_dir_marker_ember content\n", encoding="utf-8")
    (root / ".hidden_file.txt").write_text("hidden_file_marker_willow content\n", encoding="utf-8")

    # .gitignore'd files (root-level), plus a nested .gitignore.
    (root / ".gitignore").write_text("ignored_top.txt\nignored_dir/\n", encoding="utf-8")
    (root / "ignored_top.txt").write_text("gitignored_marker_timber content\n", encoding="utf-8")
    ignored_dir = root / "ignored_dir"
    ignored_dir.mkdir(parents=True, exist_ok=True)
    (ignored_dir / "inside.txt").write_text("gitignored_dir_marker_vapor content\n", encoding="utf-8")

    nested = root / "nested_gitignore"
    nested.mkdir(parents=True, exist_ok=True)
    (nested / ".gitignore").write_text("nested_ignored.txt\n", encoding="utf-8")
    (nested / "nested_ignored.txt").write_text("nested_gitignored_marker_wharf content\n", encoding="utf-8")
    (nested / "nested_kept.txt").write_text("nested_kept_marker_yonder content\n", encoding="utf-8")

    # .ignore and .rgignore files.
    (root / ".ignore").write_text("dot_ignore_excluded.txt\n", encoding="utf-8")
    (root / "dot_ignore_excluded.txt").write_text("dot_ignore_marker_zephyr content\n", encoding="utf-8")
    (root / ".rgignore").write_text("rgignore_excluded.txt\n", encoding="utf-8")
    (root / "rgignore_excluded.txt").write_text("rgignore_marker_anchor content\n", encoding="utf-8")

    # Deep nesting.
    deep = root
    for part in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]:
        deep = deep / part
    deep.mkdir(parents=True, exist_ok=True)
    (deep / "deep_file.txt").write_text("deep_nesting_marker_bramble content\n", encoding="utf-8")

    # File names with spaces and non-ASCII names.
    (root / "file with spaces.txt").write_text("spaced_name_marker_cobalt content\n", encoding="utf-8")
    (root / "caf\u00e9_\u65e5\u672c\u8a9e.txt").write_text("nonascii_name_marker_dapple content\n", encoding="utf-8")

    # Invalid UTF-8 bytes: 0xff 0xfe mid-file (not a BOM prefix), a lone
    # 0xC5, and an incomplete 2-of-3-byte sequence (0xE2 0x84 without 0xAA).
    (root / "invalid_utf8.txt").write_bytes(
        b"before_invalid_marker_echo\n"
        b"line with lone byte \xc5 stray\n"
        b"line with pair \xff\xfe stray\n"
        b"line with truncated \xe2\x84 stray\n"
        b"after_invalid_marker_fable\n"
    )

    # A patternfile fixture consumers can point -f at.
    (root / "patterns.txt").write_text("forest\nharbor\nquartz\n", encoding="utf-8")


def copy_repo_source(dst: Path) -> None:
    dst.mkdir(parents=True, exist_ok=True)
    for name in ("crates", "docs", "eval/bench", "README.md", "Cargo.toml", ".gitignore"):
        src = ROOT / name
        target = dst / Path(name).name if "/" not in name else dst / name
        if "/" in name:
            target.parent.mkdir(parents=True, exist_ok=True)
        if src.is_dir():
            shutil.copytree(src, target, dirs_exist_ok=True)
        elif src.is_file():
            shutil.copy2(src, target)


def git_init(path: Path) -> None:
    subprocess.run(
        ["git", "init", "-q"], cwd=path, check=True,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    subprocess.run(
        ["git", "config", "user.email", "wgrep-parity@example.invalid"],
        cwd=path, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    subprocess.run(
        ["git", "config", "user.name", "wgrep-parity"],
        cwd=path, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


# --------------------------------------------------------------------------
# Word / pattern sampling ("gold-blind": derived from real corpus content
# with a fixed seed, never hand-picked to make wgrep look good or bad)
# --------------------------------------------------------------------------

TEXT_EXTS = {
    ".txt", ".md", ".rs", ".py", ".toml", ".json", ".yml", ".yaml", ".rst", ".cfg", ".ini", "",
    # Code-corpus extensions (e.g. linux kernel C sources, rust compiler sources).
    ".c", ".h", ".cc", ".cpp", ".hpp", ".hh", ".S", ".s", ".lds", ".dts", ".dtsi",
}


def sample_corpus_words(walk_root: Path, max_files: int = 400, max_bytes_per_file: int = 200_000):
    """Deterministically samples files under walk_root and tokenizes them to
    build doc-frequency buckets. Returns (rare, mid, common, word->relpaths).
    """
    files = []
    for p in sorted(walk_root.rglob("*")):
        if not p.is_file():
            continue
        parts = p.relative_to(walk_root).parts
        if any(part in (".git", "target", "__pycache__") or part.startswith(".") for part in parts[:-1]):
            continue
        if p.suffix.lower() not in TEXT_EXTS:
            continue
        files.append(p)
    files.sort(key=stable_sort_key("file_sample"))
    files = files[:max_files]

    doc_count: collections.Counter[str] = collections.Counter()
    word_files: dict[str, list[str]] = collections.defaultdict(list)
    for p in files:
        try:
            data = p.read_bytes()[:max_bytes_per_file]
            text = data.decode("utf-8", "ignore")
        except OSError:
            continue
        seen = set()
        for m in WORD_RE.finditer(text):
            w = m.group(0).lower()
            if 3 <= len(w) <= 20 and w.isascii():
                seen.add(w)
        rel = p.relative_to(walk_root).as_posix()
        for w in seen:
            doc_count[w] += 1
            if len(word_files[w]) < 3:
                word_files[w].append(rel)

    words = sorted(doc_count)
    rare = [w for w in words if 1 <= doc_count[w] <= 3]
    mid = [w for w in words if 4 <= doc_count[w] <= max(5, len(files) // 8)]
    common_sorted = sorted(words, key=lambda w: (-doc_count[w], w))
    common = common_sorted[:30]

    rare.sort(key=stable_sort_key("rare"))
    mid.sort(key=stable_sort_key("mid"))
    common.sort(key=stable_sort_key("common"))
    return rare, mid, common, word_files, files


def pick_variant_paths(word_files, files, root):
    """Picks an explicit-file, a subdirectory with >=2 sample files, and a
    two-file multi-path set, all relative to `root`, for path-variant
    invocations (path_explicit_*/path_subdir_*/path_multi_*/path_implicit_*).
    """
    explicit_file = None
    subdir = None
    multi = None
    for w, rels in word_files.items():
        if rels:
            explicit_file = rels[0]
            break
    candidates_by_dir = collections.defaultdict(list)
    for f in files:
        rel = f.relative_to(root).as_posix()
        candidates_by_dir[str(Path(rel).parent)].append(rel)
    for d, rels in sorted(candidates_by_dir.items(), key=lambda kv: -len(kv[1])):
        if d != "." and len(rels) >= 2:
            subdir = d
            break
    two = [f.relative_to(root).as_posix() for f in files[:2]]
    if len(two) == 2:
        multi = two
    return {"explicit_file": explicit_file, "subdir": subdir, "multi_paths": multi}


REGEX_BATTERY_TEMPLATES = [
    ("alternation", lambda w: f"{w[0]}{w[1:]}|{w[::-1]}"),
    ("char_class", lambda w: f"[{w[0]}{w[0].upper()}]{re.escape(w[1:])}"),
    ("word_boundary", lambda w: rf"\b{re.escape(w)}\b"),
    ("anchor_start", lambda w: f"^{re.escape(w[:3])}"),
    ("anchor_end", lambda w: f"{re.escape(w[-3:])}$"),
    ("dotstar_span", lambda w: f"{re.escape(w[:2])}.*{re.escape(w[-2:])}"),
    ("bounded_repeat", lambda w: f"{re.escape(w[0])}.{{1,3}}{re.escape(w[-1])}"),
    ("unbounded_repeat", lambda w: f"{re.escape(w[0])}.+{re.escape(w[-1])}"),
    ("inline_ci", lambda w: f"(?i){w.upper()}"),
    ("escaped_meta", lambda w: re.escape(w) + r"\."),
    ("word_ing", lambda w: r"\w+ing"),
    ("short_pattern", lambda w: w[:2]),
    ("empty_anchor", lambda w: "^"),
    ("byte_escape_ff", lambda w: r"(?-u:\xff)"),
    ("byte_escape_c5", lambda w: r"(?-u:\xC5)"),
]

UNICODE_CLASS_PATTERNS = [
    ("unicode_greek", r"\p{Greek}"),
    ("unicode_letter_run", r"\p{L}{4,}"),
]

MULTILINE_PATTERNS = [
    ("multiline_two_words", "signal\nquartz"),
    ("multiline_note_seam", "content\nnested"),
]

PCRE2_LOOKAROUND_PATTERNS = [
    ("lookahead", r"forest(?=\s)"),
    ("lookbehind", r"(?<=\s)harbor"),
    ("neg_lookahead", r"copper(?!bad_suffix_never)"),
]

UNICODE_LITERAL_PATTERNS = [
    ("kelvin_literal", "\u212Aelvin"),
    ("long_s_literal", "cla\u017F\u017F"),
    ("eacute_literal", "caf\u00e9"),
    ("eacute_ci", "(?i)CAF\u00c9"),
]


def build_patterns(rare, mid, common, tag: str, n_each: int = 5, absent_n: int = 6, regex_cap: int | None = None,
                    exclude_kinds: set | None = None):
    rng = seeded_rng("patterns", tag)
    picks = {
        "rare": rare[:n_each] or ["zzz_no_rare_zzz"],
        "mid": mid[:n_each] or (common[:n_each] if common else ["the"]),
        "common": common[:n_each] or ["the"],
    }
    patterns = []
    for kind, words in picks.items():
        for w in words:
            patterns.append({"pattern": w, "kind": kind})
    for i in range(absent_n):
        patterns.append({"pattern": f"wgrep_parity_absent_{SEED:x}_{i:03d}", "kind": "absent"})

    seed_word = (rare + mid + common + ["forest"])[0]
    regex_patterns = []
    for name, fn in REGEX_BATTERY_TEMPLATES:
        try:
            regex_patterns.append({"pattern": fn(seed_word), "kind": f"regex_{name}"})
        except Exception:
            continue
    for name, pat in UNICODE_CLASS_PATTERNS:
        regex_patterns.append({"pattern": pat, "kind": f"regex_{name}"})
    for name, pat in UNICODE_LITERAL_PATTERNS:
        regex_patterns.append({"pattern": pat, "kind": f"regex_{name}"})
    if exclude_kinds:
        regex_patterns = [p for p in regex_patterns if p["kind"] not in exclude_kinds]
    if regex_cap is not None and len(regex_patterns) > regex_cap:
        # Deterministic diverse subsample rather than a plain prefix, so a
        # cap doesn't silently always drop the same tail entries.
        regex_patterns = sorted(regex_patterns, key=lambda p: stable_sort_key("regex_cap:" + tag)(p["pattern"]))[:regex_cap]
    patterns.extend(regex_patterns)
    # dedupe, preserve order
    seen = set()
    out = []
    for p in patterns:
        if p["pattern"] not in seen:
            seen.add(p["pattern"])
            out.append(p)
    return out


# --------------------------------------------------------------------------
# Execution + comparison
# --------------------------------------------------------------------------

class Invocation:
    __slots__ = ("corpus", "flag_name", "pattern_kind", "args", "cwd", "sort_mode", "extra_env")

    def __init__(self, corpus, flag_name, pattern_kind, args, cwd, sort_mode, extra_env=None):
        self.corpus = corpus
        self.flag_name = flag_name
        self.pattern_kind = pattern_kind
        self.args = args
        self.cwd = cwd
        self.sort_mode = sort_mode  # "sorted" or "multiset"
        self.extra_env = extra_env or {}


def run_one(exe: str, args: list[str], cwd: Path, env: dict, tmp_dir: Path, prefix: str, timeout: float = 60.0):
    """Runs `exe` with stdout/stderr redirected to files under `tmp_dir`
    (never PIPE -- some invocations against the 1.6GB Linux tree print
    gigabytes, which blows up in-memory PIPE capture). Returns
    (returncode, stdout_path, stderr_path, elapsed, timeout_or_None). On
    timeout the temp files still exist (possibly partial) but their content
    is never read: `compare()` short-circuits on a timeout before touching
    stdout/stderr.
    """
    full_env = dict(os.environ)
    full_env.update(env)
    out_path = tmp_dir / f"{prefix}.out"
    err_path = tmp_dir / f"{prefix}.err"
    started = time.time()
    try:
        with open(out_path, "wb") as f_out, open(err_path, "wb") as f_err:
            proc = subprocess.run(
                [exe, *args], cwd=str(cwd), env=full_env, stdin=subprocess.DEVNULL,
                stdout=f_out, stderr=f_err, timeout=timeout,
            )
        return proc.returncode, out_path, err_path, time.time() - started, None
    except subprocess.TimeoutExpired:
        return None, out_path, err_path, time.time() - started, "timeout"


def events_offset(path):
    """Current size of the NDJSON events file, or 0 if absent."""
    if path is None:
        return 0
    try:
        return path.stat().st_size
    except OSError:
        return 0


def events_delta(path, start):
    """(new_offset, last_event_or_None) for the events a wgrep run appended."""
    if path is None:
        return start, None
    try:
        size = path.stat().st_size
    except OSError:
        return start, None
    if size < start:
        start = 0
    try:
        with open(path, "rb") as f:
            f.seek(start)
            data = f.read()
    except OSError:
        return start, None
    ev = None
    for line in data.split(b"\n"):
        line = line.strip()
        if line:
            try:
                ev = json.loads(line)
            except ValueError:
                pass
    return size, ev


def normalize_timing(data: bytes) -> bytes:
    data = TIMING_STATS_RE.sub(b"<ELAPSED> seconds", data)
    data = TIMING_JSON_RE.sub(rb'"\1":{"human":"<T>","nanos":0,"secs":0}', data)
    return data


def split_lines(data: bytes, null_sep: bool) -> list[bytes]:
    sep = b"\0" if null_sep else b"\n"
    parts = data.split(sep)
    if parts and parts[-1] == b"":
        parts = parts[:-1]
    return parts


CHUNK_SIZE = 1 << 20  # 1 MiB
TIMING_NORMALIZE_INMEMORY_THRESHOLD = 64 * 1024 * 1024  # 64 MiB
MULTISET_INMEMORY_THRESHOLD = 256 * 1024 * 1024  # 256 MiB


def files_equal_stream(path_a: Path, path_b: Path, chunk_size: int = CHUNK_SIZE) -> bool:
    """Equal iff same size and all chunks equal, streamed in `chunk_size`
    reads so neither file is ever loaded whole."""
    size_a = path_a.stat().st_size
    size_b = path_b.stat().st_size
    if size_a != size_b:
        return False
    if size_a == 0:
        return True
    with open(path_a, "rb") as fa, open(path_b, "rb") as fb:
        while True:
            ca = fa.read(chunk_size)
            cb = fb.read(chunk_size)
            if ca != cb:
                return False
            if not ca:
                return True


def _read_prefix(path: Path, n: int) -> bytes:
    with open(path, "rb") as f:
        return f.read(n)


def iter_records(path: Path, sep: bytes, chunk_size: int = CHUNK_SIZE):
    """Streams records from `path` split on the single-byte `sep`, matching
    `bytes.split(sep)` semantics with a trailing empty element dropped (a
    trailing separator does not produce an empty final record). Bounded
    memory: at most one buffered partial chunk plus the largest single
    record, never the whole file.
    """
    buf = b""
    with open(path, "rb") as f:
        while True:
            chunk = f.read(chunk_size)
            if not chunk:
                break
            buf += chunk
            parts = buf.split(sep)
            buf = parts.pop()
            for p in parts:
                yield p
    if buf:
        yield buf


def count_records(path: Path, sep: bytes) -> int:
    return sum(1 for _ in iter_records(path, sep))


def stream_first_diff_and_counts(path_a: Path, path_b: Path, sep: bytes):
    """Streaming equivalent of the old in-memory `zip(rl, wl)` scan: finds
    the first differing record (stopping at the shorter side, like `zip`)
    and separately streams full record counts for both sides (the message
    always reports both totals, even when the diff is found early).
    """
    count_a = count_records(path_a, sep)
    count_b = count_records(path_b, sep)
    first = None
    for i, (a, b) in enumerate(zip(iter_records(path_a, sep), iter_records(path_b, sep))):
        if a != b:
            first = (i, a, b)
            break
    if first is None:
        first = (min(count_a, count_b), b"<eof>", b"<eof>")
    return first, count_a, count_b


def normalize_timing_file(path: Path, tmp_dir: Path, prefix: str,
                           size_threshold: int = TIMING_NORMALIZE_INMEMORY_THRESHOLD) -> Path:
    """Applies `normalize_timing` and writes the result to a new temp file.
    Small files (<64MiB, the common case for --json/--stats output) are
    loaded whole; larger ones are normalized line by line (both timing
    regexes are line-local: TIMING_STATS_RE anchors ^ per line, and
    TIMING_JSON_RE's Duration objects never span a `rg --json` line).
    """
    size = path.stat().st_size
    out_path = tmp_dir / f"{prefix}.norm"
    if size < size_threshold:
        out_path.write_bytes(normalize_timing(path.read_bytes()))
        return out_path
    with open(path, "rb") as fin, open(out_path, "wb") as fout:
        for line in fin:
            fout.write(normalize_timing(line))
    return out_path


def external_sort_records(path: Path, sep: bytes, tmp_dir: Path, prefix: str,
                           chunk_bytes: int = 64 * 1024 * 1024) -> Path:
    """External merge sort over `path`'s records, bounded to ~chunk_bytes of
    records buffered at a time: split into sorted runs, then k-way-merge
    the runs (heapq.merge holds only one record per run in memory at once).
    Used for the multiset comparison fallback when outputs are too large to
    Counter() in memory.
    """
    runs: list[Path] = []
    buf: list[bytes] = []
    buf_bytes = 0
    run_idx = 0

    def flush():
        nonlocal buf, buf_bytes, run_idx
        if not buf:
            return
        buf.sort()
        run_path = tmp_dir / f"{prefix}_run_{run_idx}.tmp"
        with open(run_path, "wb") as f:
            f.write(sep.join(buf))
            f.write(sep)
        runs.append(run_path)
        run_idx += 1
        buf = []
        buf_bytes = 0

    for rec in iter_records(path, sep):
        buf.append(rec)
        buf_bytes += len(rec) + len(sep)
        if buf_bytes >= chunk_bytes:
            flush()
    flush()

    if not runs:
        empty = tmp_dir / f"{prefix}_empty.tmp"
        empty.write_bytes(b"")
        return empty
    if len(runs) == 1:
        return runs[0]

    merged_path = tmp_dir / f"{prefix}_merged.tmp"
    with open(merged_path, "wb") as out:
        first = True
        for rec in heapq.merge(*(iter_records(r, sep) for r in runs)):
            if not first:
                out.write(sep)
            out.write(rec)
            first = False
        if not first:
            out.write(sep)
    for r in runs:
        try:
            r.unlink()
        except OSError:
            pass
    return merged_path


def _groupby_counts(records_iter):
    for k, g in itertools.groupby(records_iter):
        yield k, sum(1 for _ in g)


def compare_sorted_grouped(path_a: Path, path_b: Path, sep: bytes, cap: int = 5):
    """Merge-compares two already-externally-sorted record streams as
    multisets (run-length grouped), without ever materializing a Counter.
    Returns (equal, rg_only_examples, wg_only_examples) with examples capped
    at `cap` elements each, flattened like Counter.elements() would be.
    """
    ga = _groupby_counts(iter_records(path_a, sep))
    gb = _groupby_counts(iter_records(path_b, sep))
    a = next(ga, None)
    b = next(gb, None)
    rg_only: list[bytes] = []
    wg_only: list[bytes] = []
    equal = True
    while a is not None or b is not None:
        if b is None or (a is not None and a[0] < b[0]):
            equal = False
            if len(rg_only) < cap:
                rg_only.extend([a[0]] * min(a[1], cap - len(rg_only)))
            a = next(ga, None)
        elif a is None or b[0] < a[0]:
            equal = False
            if len(wg_only) < cap:
                wg_only.extend([b[0]] * min(b[1], cap - len(wg_only)))
            b = next(gb, None)
        else:
            if a[1] != b[1]:
                equal = False
                diff = a[1] - b[1]
                if diff > 0 and len(rg_only) < cap:
                    rg_only.extend([a[0]] * min(diff, cap - len(rg_only)))
                elif diff < 0 and len(wg_only) < cap:
                    wg_only.extend([b[0]] * min(-diff, cap - len(wg_only)))
            a = next(ga, None)
            b = next(gb, None)
    return equal, rg_only[:cap], wg_only[:cap]


def compare_multiset(path_a: Path, path_b: Path, sep: bytes, tmp_dir: Path,
                      inmemory_threshold: int = MULTISET_INMEMORY_THRESHOLD):
    """Multiset (unsorted) comparison. Small outputs (<256MiB each, the
    common case -- ~10% of invocations use multiset mode) use the original
    in-memory Counter approach. Larger ones fall back to an external
    sort-merge so memory stays bounded regardless of output size.
    """
    size_a = path_a.stat().st_size
    size_b = path_b.stat().st_size
    null_sep = sep == b"\0"
    if size_a < inmemory_threshold and size_b < inmemory_threshold:
        rl = collections.Counter(split_lines(path_a.read_bytes(), null_sep))
        wl = collections.Counter(split_lines(path_b.read_bytes(), null_sep))
        if rl != wl:
            rg_only = list((rl - wl).elements())[:5]
            wg_only = list((wl - rl).elements())[:5]
            return False, f"stdout differs (multiset): rg_only={rg_only!r} wgrep_only={wg_only!r}"
        return True, None

    sorted_a = external_sort_records(path_a, sep, tmp_dir, "rg_ms")
    sorted_b = external_sort_records(path_b, sep, tmp_dir, "wg_ms")
    equal, rg_only, wg_only = compare_sorted_grouped(sorted_a, sorted_b, sep)
    if not equal:
        return False, f"stdout differs (multiset, external-sort): rg_only={rg_only!r} wgrep_only={wg_only!r}"
    return True, None


def compare(inv: Invocation, rg_res, wg_res, has_timing: bool, null_sep: bool, tmp_dir: Path):
    """Returns (status, reason) where status is "ok", "mismatch" or
    "inconclusive". A timeout on only one side is a mismatch (one binary
    failed to do what the other did); a timeout on *both* sides is
    inconclusive -- neither side produced anything to compare, so it is
    neither confirmed correct nor confirmed wrong.

    rg_res/wg_res carry stdout/stderr as *file paths* (run_one redirects to
    temp files under tmp_dir instead of capturing PIPE output in memory), so
    every comparison here streams rather than loading whole files.
    """
    rg_code, rg_out_path, rg_err_path, _, rg_to = rg_res
    wg_code, wg_out_path, wg_err_path, _, wg_to = wg_res

    if rg_to and wg_to:
        return "inconclusive", "timeout: both rg and wgrep timed out"
    if rg_to or wg_to:
        return "mismatch", f"timeout: rg_timeout={bool(rg_to)} wgrep_timeout={bool(wg_to)}"

    sep = b"\0" if null_sep else b"\n"

    if has_timing:
        rg_out_cmp = normalize_timing_file(rg_out_path, tmp_dir, "rg_out_norm")
        wg_out_cmp = normalize_timing_file(wg_out_path, tmp_dir, "wg_out_norm")
        rg_err_cmp = normalize_timing_file(rg_err_path, tmp_dir, "rg_err_norm")
        wg_err_cmp = normalize_timing_file(wg_err_path, tmp_dir, "wg_err_norm")
    else:
        rg_out_cmp, wg_out_cmp = rg_out_path, wg_out_path
        rg_err_cmp, wg_err_cmp = rg_err_path, wg_err_path

    if rg_code != wg_code:
        return "mismatch", f"exit code differs: rg={rg_code} wgrep={wg_code}"

    if not files_equal_stream(rg_err_cmp, wg_err_cmp):
        rg_snip, wg_snip = _read_prefix(rg_err_cmp, 300), _read_prefix(wg_err_cmp, 300)
        return "mismatch", f"stderr differs: rg={rg_snip!r} wgrep={wg_snip!r}"

    if inv.sort_mode == "sorted":
        if not files_equal_stream(rg_out_cmp, wg_out_cmp):
            first, count_a, count_b = stream_first_diff_and_counts(rg_out_cmp, wg_out_cmp, sep)
            return "mismatch", (f"stdout differs (sorted) at line {first[0]}: rg={first[1][:200]!r} "
                                 f"wgrep={first[2][:200]!r} (rg_lines={count_a} wgrep_lines={count_b})")
        return "ok", None
    else:
        ok, reason = compare_multiset(rg_out_cmp, wg_out_cmp, sep, tmp_dir)
        if not ok:
            return "mismatch", reason
        return "ok", None


# --------------------------------------------------------------------------
# Flag matrix
# --------------------------------------------------------------------------

# Flags whose output is at most one line per file regardless of match count
# (safe to pair with "common"/"mid" patterns on a large corpus).
OUTPUT_BOUNDED_FLAGS = {"-c", "--count-matches", "-l", "-L", "--include-zero-c"}

# Regex-battery kinds whose match count is structurally unbounded on real
# English prose (near-every-line hits): too slow to run in default/-o/-A/-B/
# -C/--json/etc. mode against a 486MB corpus. Still exercised under the
# OUTPUT_BOUNDED_FLAGS above.
BROAD_KINDS = {
    "regex_short_pattern", "regex_word_ing", "regex_dotstar_span",
    "regex_unbounded_repeat", "regex_unicode_letter_run",
}

# --------------------------------------------------------------------------
# Code-shaped hand patterns for --extra-corpus (real code repos: identifiers,
# camelCase, quoted paths, TODO/FIXME, hex constants). These are fixed,
# hand-written patterns (not sampled), applied on top of the usual gold-blind
# sampled patterns. Some are structurally broad on a large code corpus (e.g.
# TODO|FIXME, hex constants, camelCase can match thousands of lines), so
# they are treated like BROAD_KINDS/"mid"/"common": excluded from unbounded
# -output flag groups in large_corpus mode, and only run under the bounded
# flags (-c/--count-matches/-l/-L/--include-zero-c) plus the regex-battery
# style groups that already cap output via -m/-A/-B/-C etc.
GENERIC_CODE_PATTERNS = [
    ("hand_camel", r"\b[a-z]+[A-Z][a-zA-Z0-9]*\b"),
    ("hand_path_string", r'"[A-Za-z0-9_./-]+/[A-Za-z0-9_./-]+"'),
    ("hand_todo_fixme", r"TODO|FIXME"),
    ("hand_hex_const", r"0x[0-9a-f]{8}"),
]
LINUX_CODE_PATTERNS = [
    ("hand_identifier", "kmalloc"),
    ("hand_identifier", "spin_lock_irqsave"),
    ("hand_identifier", "EXPORT_SYMBOL_GPL"),
    ("hand_include_linux", "#include <linux/"),
]
RUST_CODE_PATTERNS = [
    ("hand_rust_impl_for", r"impl.*for"),
    ("hand_rust_fn_mut", r"fn \w+_mut\b"),
]
HAND_KINDS = ({k for k, _ in GENERIC_CODE_PATTERNS} | {k for k, _ in LINUX_CODE_PATTERNS}
              | {k for k, _ in RUST_CODE_PATTERNS})


def code_hand_patterns(corpus_name: str) -> list[dict]:
    """Fixed code-shaped patterns for a named --extra-corpus. Corpus-specific
    patterns are selected by substring match on the corpus name ("linux",
    "rust"), so `--extra-corpus linux=E:\\wgrep-code\\linux` gets the kernel
    identifiers and `--extra-corpus rust=E:\\wgrep-code\\rust` gets the Rust
    ones; every extra corpus gets the generic set regardless of name.
    """
    out = [{"pattern": pat, "kind": kind} for kind, pat in GENERIC_CODE_PATTERNS]
    name_l = corpus_name.lower()
    if "linux" in name_l:
        out += [{"pattern": pat, "kind": kind} for kind, pat in LINUX_CODE_PATTERNS]
    if "rust" in name_l:
        out += [{"pattern": pat, "kind": kind} for kind, pat in RUST_CODE_PATTERNS]
    return out


def flag_groups(trimmed: bool = False):
    g = []
    g.append(("default", []))
    g.append(("-i", ["-i"]))
    g.append(("-S", ["-S"]))
    g.append(("-s", ["-s"]))
    g.append(("-w", ["-w"]))
    g.append(("-x", ["-x"]))
    g.append(("-F", ["-F"]))
    g.append(("-v", ["-v"]))
    g.append(("-c", ["-c"]))
    g.append(("--count-matches", ["--count-matches"]))
    g.append(("-l", ["-l"]))
    g.append(("-L", ["-L"]))
    g.append(("-o", ["-o"]))
    g.append(("-n", ["-n"]))
    g.append(("-N", ["-N"]))
    g.append(("--column", ["--column"]))
    g.append(("-A2", ["-A", "2"]))
    g.append(("-B1", ["-B", "1"]))
    g.append(("-C1", ["-C", "1"]))
    g.append(("-m3", ["-m", "3"]))
    g.append(("--json", ["--json"]))
    g.append(("--stats", ["--stats"]))
    g.append(("--passthru", ["--passthru"]))
    g.append(("-U", ["-U"]))
    g.append(("-U-dotall", ["-U", "--multiline-dotall"]))
    g.append(("-P", ["-P"]))
    g.append(("--hidden", ["--hidden"]))
    g.append(("--no-ignore", ["--no-ignore"]))
    g.append(("-uu", ["-uu"]))
    g.append(("-g_md", ["-g", "*.md"]))
    g.append(("-g_not_rs", ["-g", "!*.rs"]))
    g.append(("-t_rust", ["-t", "rust"]))
    g.append(("-t_md", ["-t", "md"]))
    g.append(("--max-depth2", ["--max-depth", "2"]))
    g.append(("-a", ["-a"]))
    g.append(("--binary", ["--binary"]))
    g.append(("--no-unicode", ["--no-unicode"]))
    g.append(("--include-zero-c", ["--include-zero", "-c"]))
    g.append(("--null-data", ["--null-data"]))
    g.append(("--crlf", ["--crlf"]))
    g.append(("-E_utf16le", ["-E", "utf-16le"]))
    if trimmed:
        # Drop groups that are redundant with another group already covering
        # the same code path, or meaningless on this corpus (no CRLF/UTF-16/
        # Rust files) -- keeps the huge dp013 corpus within its time budget
        # while still covering every *kind* of flag interaction.
        drop = {"-S", "-s", "-N", "--column", "-B1", "-C1", "--binary", "-uu",
                "-g_not_rs", "-t_rust", "--crlf", "-E_utf16le",
                "-x", "--passthru", "-o", "--stats", "--max-depth2", "--null-data"}
        g = [(n, a) for (n, a) in g if n not in drop]
    return g


def build_invocations(corpus_name: str, patterns: list[dict], base_paths: list[str], implicit_ok: bool,
                       extra_paths_for_variants: dict, large_corpus: bool = False) -> list[Invocation]:
    invs: list[Invocation] = []
    groups = flag_groups(trimmed=large_corpus)
    for gi, (gname, gargs) in enumerate(groups):
        if gname == "-v":
            # Inverting a pattern that matches almost nothing dumps almost
            # the whole (486MB) corpus; invert only patterns that already
            # match broadly, so the *complement* stays small.
            pool = [p for p in patterns if p["kind"] == "common"] or patterns[:2]
        elif large_corpus and gname not in OUTPUT_BOUNDED_FLAGS:
            # Every other unbounded-output flag: keep the pool to patterns
            # with naturally few hits (rare/absent/tame regex). "mid",
            # "common", structurally-broad regex kinds and hand-written
            # code-shaped patterns are still exercised, just only under the
            # bounded (-c/-l/...) flags below.
            pool = [p for p in patterns if p["kind"] not in ("mid", "common")
                    and p["kind"] not in BROAD_KINDS and p["kind"] not in HAND_KINDS]
        else:
            pool = patterns
        for pi, p in enumerate(pool):
            idx = gi * 1000 + pi
            sort_mode = "multiset" if (idx % 10 == 9) else "sorted"
            sort_args = [] if sort_mode == "multiset" else ["--sort", "path"]
            args = [*gargs, *sort_args, "--color", "never", "--", p["pattern"], *base_paths]
            invs.append(Invocation(corpus_name, gname, p["kind"], args, None, sort_mode))

    # -e multiple patterns.
    for i in range(0, min(6, len(patterns) - 1), 2):
        a, b = patterns[i]["pattern"], patterns[i + 1]["pattern"]
        for sort_mode, sort_args in (("sorted", ["--sort", "path"]), ("multiset", [])):
            args = ["-e", a, "-e", b, *sort_args, "--color", "never", *base_paths]
            invs.append(Invocation(corpus_name, "-e_multi", "combo", args, None, sort_mode))
            break  # one sorted variant per pair keeps this bounded

    # -f patternfile (needs the fixture's patterns.txt; caller supplies path).
    pf = extra_paths_for_variants.get("patternfile")
    if pf:
        args = ["-f", str(pf), "--sort", "path", "--color", "never", *base_paths]
        invs.append(Invocation(corpus_name, "-f_patternfile", "combo", args, None, "sorted"))

    # Path-variant invocations: explicit file, subdirectory, multiple paths, implicit.
    sample_words = [p["pattern"] for p in patterns if p["kind"] in ("rare", "mid", "absent")][:4]
    explicit_file = extra_paths_for_variants.get("explicit_file")
    subdir = extra_paths_for_variants.get("subdir")
    multi_paths = extra_paths_for_variants.get("multi_paths")
    for w in sample_words:
        for flag_name, flag_args in (("default", []), ("-i", ["-i"]), ("-c", ["-c"]), ("-l", ["-l"])):
            if explicit_file:
                args = [*flag_args, "--sort", "path", "--color", "never", "--", w, str(explicit_file)]
                invs.append(Invocation(corpus_name, f"path_explicit_{flag_name}", "path_variant", args, None, "sorted"))
            if subdir:
                args = [*flag_args, "--sort", "path", "--color", "never", "--", w, str(subdir)]
                invs.append(Invocation(corpus_name, f"path_subdir_{flag_name}", "path_variant", args, None, "sorted"))
            if multi_paths:
                args = [*flag_args, "--sort", "path", "--color", "never", "--", w, *[str(x) for x in multi_paths]]
                invs.append(Invocation(corpus_name, f"path_multi_{flag_name}", "path_variant", args, None, "sorted"))
            if implicit_ok:
                args = [*flag_args, "--sort", "path", "--color", "never", "--", w]
                invs.append(Invocation(corpus_name, f"path_implicit_{flag_name}", "path_variant", args, None, "sorted"))
    return invs


TIMING_FLAG_NAMES = {"--json", "--stats"}


RERUN_TIMEOUT = 300.0


def run_invocations(rg_exe, wgrep_exe, invocations: list[Invocation], cwd: Path, env: dict, results: list, mismatches: list, inconclusives: list, timings: dict, tmp_root: Path, reruns: list | None = None, progress_every: int = 50, mismatch_artifact_cap: int = 20):
    n = len(invocations)
    t_group_start = time.time()
    kept_mismatch_artifacts = 0
    ev_path = Path(env["WGREP_INDEX_EVENTS"]) if env.get("WGREP_INDEX_EVENTS") else None
    for i, inv in enumerate(invocations):
        null_sep = "--null-data" in inv.args
        has_timing = inv.flag_name in TIMING_FLAG_NAMES
        t0 = time.time()
        # Per-invocation temp dir for rg/wgrep stdout+stderr (and any
        # normalized/externally-sorted intermediates `compare()` needs) --
        # always under tmp_root (itself under --work, never the searched
        # corpus). Deleted after comparing unless this invocation mismatches
        # and we're still under the kept-artifact cap.
        inv_tmp = Path(tempfile.mkdtemp(dir=tmp_root, prefix="inv_"))
        keep_dir = False
        try:
            ev_off = events_offset(ev_path)
            rg_res = run_one(rg_exe, inv.args, cwd, env, inv_tmp, "rg")
            wg_res = run_one(wgrep_exe, inv.args, cwd, env, inv_tmp, "wg")

            rg_to_first, wg_to_first = rg_res[4], wg_res[4]
            if bool(rg_to_first) != bool(wg_to_first):
                # Exactly one side timed out at the default (60s) timeout. On
                # large corpora with the stdin-fix in place, implicit-path
                # invocations walk the whole repo and can legitimately take
                # >60s for reasons unrelated to correctness (cold disk cache,
                # scheduler jitter, antivirus scanning, etc.) -- especially
                # asymmetric between rg and wgrep since wgrep has extra index
                # bookkeeping. Rerun BOTH sides at a much longer timeout and let
                # `compare` classify the rerun outcome normally (both timeout =>
                # inconclusive, one timeout => real mismatch, neither => normal
                # diff) instead of immediately declaring a mismatch on the first,
                # short-timeout result.
                print(f"    RERUN (one-sided timeout @60s) [{inv.corpus}/{inv.flag_name}/{inv.pattern_kind}]: "
                      f"rg_timeout={bool(rg_to_first)} wgrep_timeout={bool(wg_to_first)}, "
                      f"retrying both at {RERUN_TIMEOUT:.0f}s", flush=True)
                rerun_tmp = Path(tempfile.mkdtemp(dir=tmp_root, prefix="inv_rerun_"))
                rg_res2 = run_one(rg_exe, inv.args, cwd, env, rerun_tmp, "rg", timeout=RERUN_TIMEOUT)
                wg_res2 = run_one(wgrep_exe, inv.args, cwd, env, rerun_tmp, "wg", timeout=RERUN_TIMEOUT)
                rerun_entry = {
                    "corpus": inv.corpus, "flag": inv.flag_name, "pattern_kind": inv.pattern_kind,
                    "args": inv.args, "cwd": str(cwd),
                    "first_rg_timeout": bool(rg_to_first), "first_wgrep_timeout": bool(wg_to_first),
                    "rerun_rg_timeout": bool(rg_res2[4]), "rerun_wgrep_timeout": bool(wg_res2[4]),
                    "rerun_timeout_seconds": RERUN_TIMEOUT,
                }
                if reruns is not None:
                    reruns.append(rerun_entry)
                print(f"    RERUN result [{inv.corpus}/{inv.flag_name}/{inv.pattern_kind}]: "
                      f"rg_timeout={bool(rg_res2[4])} wgrep_timeout={bool(wg_res2[4])}", flush=True)
                shutil.rmtree(inv_tmp, ignore_errors=True)
                inv_tmp = rerun_tmp
                rg_res, wg_res = rg_res2, wg_res2

            elapsed = time.time() - t0
            _, ev = events_delta(ev_path, ev_off)
            ev = ev or {}
            status, reason = compare(inv, rg_res, wg_res, has_timing, null_sep, inv_tmp)
            results.append({"corpus": inv.corpus, "flag": inv.flag_name, "pattern_kind": inv.pattern_kind,
                             "sort_mode": inv.sort_mode, "status": status, "ok": status == "ok",
                             "used": ev.get("used"), "fallback_reason": ev.get("reason"),
                             "candidates": ev.get("candidates"), "files_seen": ev.get("files")})
            timings.setdefault(inv.corpus, []).append(elapsed)
            if status == "mismatch":
                keep_dir = kept_mismatch_artifacts < mismatch_artifact_cap
                mismatches.append({
                    "corpus": inv.corpus, "flag": inv.flag_name, "pattern_kind": inv.pattern_kind,
                    "args": inv.args, "cwd": str(cwd), "sort_mode": inv.sort_mode, "reason": reason,
                    "artifact_dir": str(inv_tmp) if keep_dir else None,
                })
                if keep_dir:
                    kept_mismatch_artifacts += 1
                print(f"    MISMATCH [{inv.corpus}/{inv.flag_name}/{inv.pattern_kind}]: {reason}", flush=True)
            elif status == "inconclusive":
                inconclusives.append({
                    "corpus": inv.corpus, "flag": inv.flag_name, "pattern_kind": inv.pattern_kind,
                    "args": inv.args, "cwd": str(cwd), "sort_mode": inv.sort_mode, "reason": reason,
                })
                print(f"    INCONCLUSIVE [{inv.corpus}/{inv.flag_name}/{inv.pattern_kind}]: {reason}", flush=True)
        finally:
            if not keep_dir:
                shutil.rmtree(inv_tmp, ignore_errors=True)
        if progress_every and (i + 1) % progress_every == 0:
            rate = (i + 1) / max(time.time() - t_group_start, 0.001)
            print(f"    ... {i + 1}/{n} invocations ({rate:.2f}/s, {sum(1 for r in results[-(i+1):] if not r['ok'])} mism so far in this group)", flush=True)


# --------------------------------------------------------------------------
# Prune effectiveness: verify the index actually skips files (one-shot).
# --------------------------------------------------------------------------

SKIP_RE = re.compile(rb"wgrep index: skipped (\d+) of (\d+) walked files")


def prune_effectiveness_check(wgrep_exe: str, rare_words: list[str], base_paths: list[str], cwd: Path,
                               env: dict, corpus_name: str, n: int = 10) -> dict:
    """For ~n rare-word (selective) patterns, records the WGREP_INDEX_DEBUG
    prune numbers: "skipped N of M walked files".
    """
    picks = rare_words[:n]
    samples = []
    base_env = dict(os.environ)
    base_env.update(env)
    base_env["WGREP_INDEX_DEBUG"] = "1"
    for w in picks:
        args = ["--color", "never", "--", w, *base_paths]
        proc = subprocess.run([wgrep_exe, *args], cwd=str(cwd), env=base_env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=60)
        m = SKIP_RE.search(proc.stderr)
        skipped, walked = (int(m.group(1)), int(m.group(2))) if m else (None, None)
        samples.append({"pattern": w, "one_shot_skipped": skipped, "one_shot_walked": walked})

    return {"corpus": corpus_name, "n_requested": n, "n_words_available": len(rare_words), "samples": samples}


# --------------------------------------------------------------------------
# Freshness phase
# --------------------------------------------------------------------------

def freshness_phase(wgrep_exe, rg_exe, fixture_root: Path, index_dir: Path, results, mismatches, inconclusives, timings, reruns, tmp_root: Path, force_one_shot: bool = False):
    env = {"WGREP_INDEX_DIR": str(index_dir)}
    if force_one_shot:
        # Default mode is a deterministic one-shot gate; the freshness phase
        # must use the same path or it silently exercises the daemon (and
        # leaves one serving the fixture).
        env["WGREP_NO_DAEMON"] = "1"
    # Ensure files are >2s old before building (racy-window avoidance).
    time.sleep(3)
    subprocess.run([wgrep_exe, "--index-build", "."], cwd=fixture_root, env={**os.environ, **env},
                    check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

    relevant_patterns = [
        {"pattern": "forest", "kind": "rare"},
        {"pattern": "freshness_token_unique_zzq", "kind": "absent"},
        {"pattern": "granite", "kind": "rare"},
        # Used by the timestamp-preserving rewrite below: absent from every
        # fixture file until that mutation introduces it.
        {"pattern": "zzqhmarker", "kind": "absent"},
    ]

    def rerun(label: str):
        invs = []
        for p in relevant_patterns:
            for flag_name, flag_args in (("default", []), ("-c", ["-c"]), ("-l", ["-l"])):
                args = [*flag_args, "--sort", "path", "--color", "never", "--", p["pattern"], "."]
                invs.append(Invocation("fixture_freshness_" + label, flag_name, p["kind"], args, None, "sorted"))
        run_invocations(rg_exe, wgrep_exe, invs, fixture_root, env, results, mismatches, inconclusives, timings, tmp_root, reruns)

    rerun("baseline")

    token = "freshness_token_unique_zzq"

    # (a) append a unique token to a file that previously didn't contain it.
    target_a = fixture_root / "notes" / "grp0" / "note_000.txt"
    with target_a.open("a", encoding="utf-8") as f:
        f.write(f"\n{token}_a appended line\n")
    rerun("a_append")

    # (b) overwrite a file in place with same-length different content.
    target_b = fixture_root / "notes" / "grp1" / "note_001.txt"
    old = target_b.read_bytes()
    new = (b"X" * len(old))[: len(old)]
    target_b.write_bytes(new)
    rerun("b_overwrite_samelen")

    # (c) create a new file with the token.
    target_c = fixture_root / "notes" / "grp2" / f"new_{token}.txt"
    target_c.write_text(f"{token}_c new file\n", encoding="utf-8")
    rerun("c_new_file")

    # (d) delete a file.
    target_d = fixture_root / "notes" / "grp3" / "note_003.txt"
    if target_d.exists():
        target_d.unlink()
    rerun("d_delete")

    # (e) rename a file.
    target_e_src = fixture_root / "notes" / "grp4" / "note_004.txt"
    target_e_dst = fixture_root / "notes" / "grp4" / "note_004_renamed.txt"
    if target_e_src.exists():
        target_e_src.rename(target_e_dst)
    rerun("e_rename")

    # (f) edit .gitignore to un-ignore a file.
    (fixture_root / ".gitignore").write_text("ignored_dir/\n", encoding="utf-8")  # drop ignored_top.txt rule
    rerun("f_unignore")

    # (g) modify a file then immediately (sleep 0) query.
    target_g = fixture_root / "notes" / "grp5" / "note_005.txt"
    with target_g.open("a", encoding="utf-8") as f:
        f.write(f"\n{token}_g racy append\n")
    time.sleep(0)
    rerun("g_racy_immediate")

    # (h) same-length rewrite that introduces a match and RESTORES the mtime.
    # Size+mtime cannot detect this, so it exercises the change token (ctime on
    # Unix, NTFS change time on Windows): wgrep must search the file and find
    # `zzqhmarker`, exactly as rg does, rather than skipping a stale entry.
    target_h = fixture_root / "notes" / "grp5" / "note_007.txt"
    if target_h.exists():
        old = target_h.read_bytes()
        atom = b"zzqhmarker"
        if len(old) >= len(atom):
            st = target_h.stat()
            new = (b"Q" * (len(old) - len(atom))) + atom
            target_h.write_bytes(new)
            os.utime(target_h, ns=(st.st_atime_ns, st.st_mtime_ns))
    rerun("h_samelen_new_match_restored_mtime")


# --------------------------------------------------------------------------
# Self-test: proves the streaming comparer works correctly and with bounded
# memory on outputs far larger than anything a PIPE-based capture could
# safely hold (the original MemoryError trigger on the Linux kernel tree).
# --------------------------------------------------------------------------

def _peak_rss_kb():
    """Best-effort peak RSS in KB for this process. Real numbers on
    POSIX/WSL via `resource`; on Windows falls back to `psutil` if
    installed, else None (selftest still verifies correctness, just skips
    the numeric bounded-memory assertion)."""
    try:
        import resource
        return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    except ImportError:
        try:
            import psutil
            return psutil.Process().memory_info().rss // 1024
        except Exception:
            return None
    except Exception:
        return None


def run_selftest() -> int:
    print("[selftest] fabricating two ~300MiB outputs differing at exactly one line...")
    tmp_root = Path(tempfile.mkdtemp(prefix="wgrep_parity_selftest_"))
    try:
        line = b"x" * 199 + b"\n"  # 200 bytes/line
        n_lines = (300 * 1024 * 1024) // len(line)
        diff_index = n_lines // 2
        path_a = tmp_root / "a.out"
        path_b = tmp_root / "b.out"
        with open(path_a, "wb") as fa, open(path_b, "wb") as fb:
            for i in range(n_lines):
                if i == diff_index:
                    fa.write(b"AAA_DIFFERENT_LINE_" + b"a" * 179 + b"\n")
                    fb.write(b"BBB_DIFFERENT_LINE_" + b"b" * 179 + b"\n")
                else:
                    fa.write(line)
                    fb.write(line)
        size_a, size_b = path_a.stat().st_size, path_b.stat().st_size
        print(f"[selftest] fixtures written: a={size_a / 1e6:.1f}MB b={size_b / 1e6:.1f}MB, "
              f"{n_lines} lines each, diff at line {diff_index}")

        # 1) files_equal_stream must say "not equal" without loading either file.
        equal = files_equal_stream(path_a, path_b)
        assert not equal, "selftest FAILED: files_equal_stream reported equal for differing files"

        # 2) stream_first_diff_and_counts must find the exact line and totals.
        first, count_a, count_b = stream_first_diff_and_counts(path_a, path_b, b"\n")
        print(f"[selftest] first diff at record {first[0]} (expected {diff_index}); "
              f"rg_lines={count_a} wgrep_lines={count_b} (expected {n_lines} each)")
        assert first[0] == diff_index, f"selftest FAILED: expected diff at {diff_index}, got {first[0]}"
        assert count_a == n_lines and count_b == n_lines, "selftest FAILED: record counts wrong"
        assert first[1].startswith(b"AAA_DIFFERENT_LINE_") and first[2].startswith(b"BBB_DIFFERENT_LINE_"), \
            "selftest FAILED: reported diff lines don't match the fabricated content"

        mem_after_sorted_check = _peak_rss_kb()

        # 3) the external-sort multiset fallback must also catch the same
        #    single-record difference (exercises external_sort_records +
        #    compare_sorted_grouped, forced past the 256MiB in-memory
        #    threshold since each fixture is ~300MiB).
        ms_tmp = Path(tempfile.mkdtemp(dir=tmp_root, prefix="ms_"))
        ok, reason = compare_multiset(path_a, path_b, b"\n", ms_tmp,
                                       inmemory_threshold=1)  # force external-sort path
        assert not ok, "selftest FAILED: external-sort multiset compare did not detect the one-line difference"
        print(f"[selftest] external-sort multiset compare correctly detected mismatch: {reason[:150]!r}")

        mem_final = _peak_rss_kb()

        if mem_final is not None:
            combined_mb = (size_a + size_b) / 1e6
            peak_mb = mem_final / 1024
            print(f"[selftest] peak RSS: {peak_mb:.1f} MiB (combined fixture size: {combined_mb:.1f} MB)")
            if mem_final * 1024 > 0.5 * (size_a + size_b):
                print("[selftest] WARNING: peak RSS is not clearly bounded relative to fixture size "
                      "(expected well under 50% of the combined 600MB if truly streaming)")
            else:
                print("[selftest] OK: peak RSS well under combined fixture size -- streaming confirmed")
        else:
            print("[selftest] (peak-RSS measurement unavailable on this platform; "
                  "correctness checks above still passed)")

        print("[selftest] PASSED")
        return 0
    except AssertionError as e:
        print(str(e))
        return 1
    finally:
        shutil.rmtree(tmp_root, ignore_errors=True)


# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--selftest", action="store_true",
                     help="fabricate two ~300MiB outputs differing at one line and verify the streaming "
                          "comparer (files_equal_stream/stream_first_diff_and_counts/compare_multiset's "
                          "external-sort path) finds it with bounded memory, then exit. Ignores all other "
                          "arguments except this one.")
    ap.add_argument("--wgrep", required=False, help="path to the frozen wgrep.exe under test")
    ap.add_argument("--rg", default="rg", help="rg executable (default: rg on PATH)")
    ap.add_argument("--work", type=Path, default=Path(f"E:/wgrep-parity/run-{TS}"), help="scratch dir (must be off C:)")
    ap.add_argument("--out", type=Path, default=Path(f"E:/wgrep-parity/parity-{TS}.json"))
    ap.add_argument("--run", action="store_true", help="actually execute the suite")
    ap.add_argument("--dp013-max-files", type=int, default=200)
    ap.add_argument("--dp013-n-each", type=int, default=3)
    ap.add_argument("--fixture-n-each", type=int, default=6)
    ap.add_argument("--repo-n-each", type=int, default=6)
    ap.add_argument("--skip-freshness", action="store_true")
    ap.add_argument("--flag-limit", type=int, default=None, help="use only the first N flag groups (smoke testing)")
    ap.add_argument("--daemon", action="store_true",
                     help="exercise the index daemon (the default production path). Without it, runs are "
                          "forced one-shot with WGREP_NO_DAEMON=1 for a deterministic gate; with it, wgrep's "
                          "auto-start daemon is used and stopped at the end.")
    ap.add_argument("--allow-inconclusive", action="store_true",
                     help="do not fail the run when an invocation is inconclusive (both sides timed out). "
                          "Off by default: an unallowlisted inconclusive fails the run (exit 4), because a hung "
                          "or vacuous run must not read as a pass.")
    ap.add_argument("--extra-corpus", action="append", default=[], metavar="NAME=PATH",
                     help="repeatable; runs the same sampling + trimmed/bounded flag matrix (dp013-style) against "
                          "an existing directory read-only, in place (cwd=PATH, both implicit-path and explicit- "
                          "subdir invocations), instead of copying it into --work. Adds code-shaped hand patterns "
                          "(identifiers, camelCase, quoted paths, TODO|FIXME, hex constants) on top of the usual "
                          "gold-blind sampled ones. Reuses the same WGREP_INDEX_DIR as the rest of this run and "
                          "calls `wgrep --index-build .` in PATH before sampling/running.")
    ap.add_argument("--skip-builtin", action="store_true",
                     help="skip the built-in dp013/repo_copy/fixture corpora (and the freshness phase, which "
                          "depends on the fixture) entirely; useful to run only --extra-corpus corpora within a "
                          "bounded time budget")
    ap.add_argument("--extra-max-files", type=int, default=150,
                     help="max files sampled for word-frequency buckets per --extra-corpus (default 150)")
    ap.add_argument("--extra-n-each", type=int, default=3,
                     help="rare/mid/common patterns per kind sampled per --extra-corpus (default 3)")
    ap.add_argument("--extra-regex-cap", type=int, default=6,
                     help="max regex-battery patterns kept per --extra-corpus (default 6)")
    args = ap.parse_args()

    if args.selftest:
        return run_selftest()

    if not args.run:
        ap.error("inert unless --run is supplied")
    if not args.wgrep:
        ap.error("--wgrep is required unless --selftest is supplied")
    if args.skip_builtin:
        # The freshness phase mutates and re-queries the fixture corpus,
        # which does not exist when the built-in corpora are skipped.
        args.skip_freshness = True

    try:
        sys.stdout.reconfigure(line_buffering=True)
    except Exception:
        pass

    if args.flag_limit is not None:
        _orig_flag_groups = flag_groups
        globals()["flag_groups"] = lambda trimmed=False: _orig_flag_groups(trimmed=trimmed)[: args.flag_limit]

    wgrep_exe = str(Path(args.wgrep).resolve())
    if not Path(wgrep_exe).exists():
        ap.error(f"wgrep binary not found: {wgrep_exe}")
    rg_exe = shutil.which(args.rg) or (str(Path(args.rg).resolve()) if Path(args.rg).is_file() else None)
    if not rg_exe:
        ap.error(f"rg not found: {args.rg}")

    work = args.work.resolve()
    # On this Windows box C: is nearly full, so scratch must live on E:. On
    # other systems (e.g. CI) any writable path is fine.
    if os.name == "nt" and str(work)[:2].upper() != "E:":
        ap.error(f"--work must be on the E: drive on Windows (C: is nearly full); got {work}")
    work.mkdir(parents=True, exist_ok=True)

    index_dir = work / "index"
    index_dir.mkdir(parents=True, exist_ok=True)
    env = {"WGREP_INDEX_DIR": str(index_dir)}
    # Structured per-search mode events (NDJSON) so the run can prove the daemon
    # was actually used (anti-vacuity) instead of silently falling back.
    events_file = work / "index_events.ndjson"
    env["WGREP_INDEX_EVENTS"] = str(events_file)
    if not args.daemon:
        # Deterministic gate: force one-shot. The daemon is covered by --daemon.
        env["WGREP_NO_DAEMON"] = "1"
    daemon_roots: list[str] = []

    # Per-invocation stdout/stderr temp files (see run_one/run_invocations)
    # live here -- always under --work, never under the searched corpus.
    tmp_root = work / "tmp_invocations"
    tmp_root.mkdir(parents=True, exist_ok=True)

    print(f"[setup] wgrep={wgrep_exe}")
    print(f"[setup] rg={rg_exe}")
    print(f"[setup] work={work}")
    print(f"[setup] tmp_root={tmp_root}")
    print(f"[setup] index_dir={index_dir}")

    daemon_procs: list[tuple[str, subprocess.Popen]] = []

    def _stop_daemons():
        for label, proc in daemon_procs:
            try:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=10)
                print(f"[daemon] stopped {label} (pid={proc.pid})")
            except Exception as e:
                print(f"[daemon] failed to stop {label} (pid={proc.pid}): {e}")

    try:
        t_start = time.time()
        results: list[dict] = []
        mismatches: list[dict] = []
        inconclusives: list[dict] = []
        reruns: list[dict] = []
        timings: dict[str, list[float]] = {}
        corpus_meta = {}
        prune_effectiveness: dict[str, dict] = {}

        if not args.skip_builtin:
            # ---- Corpus (b): repo source copy ----
            repo_copy = work / "corpus_repo"
            print("[setup] copying repo source corpus...")
            copy_repo_source(repo_copy)
            git_init(repo_copy)
            corpus_meta["repo_copy"] = str(repo_copy)

            # ---- Corpus (c): edge-case fixture ----
            fixture = work / "corpus_fixture"
            print("[setup] generating edge-case fixture...")
            build_fixture(fixture)
            git_init(fixture)
            corpus_meta["fixture"] = str(fixture)

            # Files must be >2s old before the FIRST index build to avoid the racy
            # window (freshness phase re-sleeps before its own build).
            print("[setup] waiting for fixture/repo mtimes to clear the 2s racy window...")
            time.sleep(3)

            # ---- Index builds ----
            # The dp013 dataset is not checked into every checkout (e.g. CI),
            # so it is optional; the repo-copy and fixture corpora always run.
            dp013_available = DP013_DATA.is_dir()
            if dp013_available:
                print("[index] building index for dp013...")
                subprocess.run([wgrep_exe, "--index-build", "data"], cwd=DP013_PARENT, env={**os.environ, **env},
                                check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                daemon_roots.append(str(DP013_DATA))
            else:
                print("[index] dp013 dataset absent; skipping the dp013 corpus")

            print("[index] building index for repo copy...")
            subprocess.run([wgrep_exe, "--index-build", "."], cwd=repo_copy, env={**os.environ, **env},
                            check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            daemon_roots.append(str(repo_copy))

            print("[index] building index for fixture...")
            subprocess.run([wgrep_exe, "--index-build", "."], cwd=fixture, env={**os.environ, **env},
                            check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            daemon_roots.append(str(fixture))

            # ---- Word sampling ----
            if dp013_available:
                print("[sample] sampling dp013 words...")
                dp013_rare, dp013_mid, dp013_common, dp013_wf, dp013_files = sample_corpus_words(DP013_DATA, max_files=args.dp013_max_files)
                print(f"[sample] dp013: {len(dp013_files)} files sampled, rare={len(dp013_rare)} mid={len(dp013_mid)} common={len(dp013_common)}")
            else:
                dp013_rare, dp013_mid, dp013_common, dp013_files = [], [], [], []
                dp013_wf = {}

            print("[sample] sampling repo copy words...")
            repo_rare, repo_mid, repo_common, repo_wf, repo_files = sample_corpus_words(repo_copy)
            print(f"[sample] repo: {len(repo_files)} files sampled, rare={len(repo_rare)} mid={len(repo_mid)} common={len(repo_common)}")

            print("[sample] sampling fixture words...")
            fix_rare, fix_mid, fix_common, fix_wf, fix_files = sample_corpus_words(fixture)
            print(f"[sample] fixture: {len(fix_files)} files sampled, rare={len(fix_rare)} mid={len(fix_mid)} common={len(fix_common)}")

            # ---- Prune-effectiveness check ----
            # Fail fast (before the expensive full sweep) if the index is
            # provably not being used on dp013.
            if dp013_available:
                print("[prune] checking dp013 prune effectiveness on rare-word queries...")
                dp013_prune = prune_effectiveness_check(wgrep_exe, dp013_rare, ["data"], DP013_PARENT, env, "dp013")
                prune_effectiveness["dp013"] = dp013_prune
                if dp013_prune["samples"] and all(s["one_shot_skipped"] == 0 for s in dp013_prune["samples"]):
                    msg = ("[prune] FATAL: one-shot mode skipped 0 files for EVERY rare-word query on dp013 -- "
                           "the index is not being used, so parity would be vacuous")
                    print(msg)
                    try:
                        args.out.parent.mkdir(parents=True, exist_ok=True)
                        args.out.write_text(json.dumps(
                            {"error": msg, "prune_effectiveness": prune_effectiveness}, indent=2, ensure_ascii=True) + "\n",
                            encoding="utf-8")
                    except Exception:
                        pass
                    return 3
                print(f"[prune] dp013: {[s['one_shot_skipped'] for s in dp013_prune['samples']]} files skipped per rare query "
                      f"(of {[s['one_shot_walked'] for s in dp013_prune['samples']]} walked)")

            print("[prune] checking repo_copy prune effectiveness on rare-word queries...")
            prune_effectiveness["repo_copy"] = prune_effectiveness_check(wgrep_exe, repo_rare, [], repo_copy, env,
                                                                           "repo_copy")
            print("[prune] checking fixture prune effectiveness on rare-word queries...")
            prune_effectiveness["fixture"] = prune_effectiveness_check(wgrep_exe, fix_rare, [], fixture, env,
                                                                         "fixture")

            dp013_patterns = (
                build_patterns(dp013_rare, dp013_mid, dp013_common, "dp013", n_each=args.dp013_n_each,
                               absent_n=3, regex_cap=10, exclude_kinds=BROAD_KINDS)
                if dp013_available else []
            )
            repo_patterns = build_patterns(repo_rare, repo_mid, repo_common, "repo", n_each=args.repo_n_each)
            fix_patterns = build_patterns(fix_rare, fix_mid, fix_common, "fixture", n_each=args.fixture_n_each)

            print(f"[patterns] dp013={len(dp013_patterns)} repo={len(repo_patterns)} fixture={len(fix_patterns)}")

            # ---- Build invocation lists ----
            if dp013_available:
                print("[run] dp013 invocations...")
                dp013_variants = pick_variant_paths(dp013_wf, dp013_files, DP013_DATA)
                dp013_invs = build_invocations("dp013", dp013_patterns, ["data"], implicit_ok=False, extra_paths_for_variants={
                    "explicit_file": ("data/" + dp013_variants["explicit_file"]) if dp013_variants["explicit_file"] else None,
                    "subdir": ("data/" + dp013_variants["subdir"]) if dp013_variants["subdir"] and dp013_variants["subdir"] != "." else None,
                    "multi_paths": [("data/" + x) for x in dp013_variants["multi_paths"]] if dp013_variants["multi_paths"] else None,
                }, large_corpus=True)
                run_invocations(rg_exe, wgrep_exe, dp013_invs, DP013_PARENT, env, results, mismatches, inconclusives, timings, tmp_root, reruns)
                print(f"[run] dp013 done: {len(dp013_invs)} invocations, {sum(1 for r in results if r['corpus']=='dp013' and not r['ok'])} mismatches so far")

            print("[run] repo copy invocations...")
            repo_variants = pick_variant_paths(repo_wf, repo_files, repo_copy)
            repo_invs = build_invocations("repo_copy", repo_patterns, [], implicit_ok=True, extra_paths_for_variants={
                "explicit_file": repo_variants["explicit_file"],
                "subdir": repo_variants["subdir"] if repo_variants["subdir"] != "." else None,
                "multi_paths": repo_variants["multi_paths"],
            })
            run_invocations(rg_exe, wgrep_exe, repo_invs, repo_copy, env, results, mismatches, inconclusives, timings, tmp_root, reruns)
            print(f"[run] repo done: {len(repo_invs)} invocations, {sum(1 for r in results if r['corpus']=='repo_copy' and not r['ok'])} mismatches so far")

            print("[run] fixture invocations...")
            fix_variants = pick_variant_paths(fix_wf, fix_files, fixture)
            fix_invs = build_invocations("fixture", fix_patterns, [], implicit_ok=True, extra_paths_for_variants={
                "explicit_file": fix_variants["explicit_file"],
                "subdir": fix_variants["subdir"] if fix_variants["subdir"] != "." else None,
                "multi_paths": fix_variants["multi_paths"],
                "patternfile": fixture / "patterns.txt",
            })
            run_invocations(rg_exe, wgrep_exe, fix_invs, fixture, env, results, mismatches, inconclusives, timings, tmp_root, reruns)
            print(f"[run] fixture done: {len(fix_invs)} invocations, {sum(1 for r in results if r['corpus']=='fixture' and not r['ok'])} mismatches so far")


        print(f"[extra] {len(args.extra_corpus)} --extra-corpus repo(s) requested")
        for spec in args.extra_corpus:
            if "=" not in spec:
                ap.error(f"--extra-corpus must be NAME=PATH, got: {spec!r}")
            ename, epath_str = spec.split("=", 1)
            epath = Path(epath_str).resolve()
            if not epath.is_dir():
                ap.error(f"--extra-corpus path does not exist or is not a directory: {epath}")
            print(f"[extra] {ename}: {epath}")
            corpus_meta[ename] = str(epath)

            print(f"[extra] {ename}: building index in place (read-only corpus, index-only write)...")
            t_eb = time.time()
            subprocess.run([wgrep_exe, "--index-build", "."], cwd=epath, env={**os.environ, **env},
                            check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            daemon_roots.append(str(epath))
            print(f"[extra] {ename}: index build took {time.time() - t_eb:.1f}s")

            print(f"[extra] {ename}: sampling words (max_files={args.extra_max_files})...")
            e_rare, e_mid, e_common, e_wf, e_files = sample_corpus_words(epath, max_files=args.extra_max_files)
            print(f"[extra] {ename}: {len(e_files)} files sampled, rare={len(e_rare)} mid={len(e_mid)} common={len(e_common)}")

            print(f"[extra] {ename}: checking prune effectiveness on rare-word queries...")
            prune_effectiveness[ename] = prune_effectiveness_check(wgrep_exe, e_rare, [], epath, env, ename)

            e_patterns = build_patterns(e_rare, e_mid, e_common, ename, n_each=args.extra_n_each, absent_n=2,
                                         regex_cap=args.extra_regex_cap, exclude_kinds=BROAD_KINDS)
            e_patterns += code_hand_patterns(ename)
            print(f"[extra] {ename}: {len(e_patterns)} total patterns (incl. hand-written code patterns)")

            e_variants = pick_variant_paths(e_wf, e_files, epath)
            e_invs = build_invocations(ename, e_patterns, [], implicit_ok=True, extra_paths_for_variants={
                "explicit_file": None,
                "subdir": e_variants["subdir"] if e_variants["subdir"] != "." else None,
                "multi_paths": None,
            }, large_corpus=True)
            print(f"[extra] {ename}: {len(e_invs)} invocations queued")
            t_er = time.time()
            run_invocations(rg_exe, wgrep_exe, e_invs, epath, env, results, mismatches, inconclusives, timings, tmp_root, reruns)
            print(f"[extra] {ename}: done in {time.time() - t_er:.1f}s, "
                  f"{sum(1 for r in results if r['corpus'] == ename and not r['ok'])} mismatches so far")

        total_before_freshness = len(results)

        if not args.skip_freshness:
            print("[freshness] running freshness phase on fixture corpus...")
            daemon_roots.append(str(fixture))
            freshness_phase(wgrep_exe, rg_exe, fixture, index_dir, results, mismatches, inconclusives, timings, reruns,
                             tmp_root, force_one_shot=not args.daemon)
            print(f"[freshness] done: {len(results) - total_before_freshness} invocations")

        elapsed_total = time.time() - t_start

        per_corpus_counts = collections.Counter(r["corpus"] for r in results)
        per_corpus_mismatches = collections.Counter(r["corpus"] for r in results if r["status"] == "mismatch")
        per_corpus_inconclusive = collections.Counter(r["corpus"] for r in results if r["status"] == "inconclusive")
        per_flag_mismatches = collections.Counter((r["corpus"], r["flag"]) for r in results if r["status"] == "mismatch")

        # ---- Anti-vacuity: prove the search actually used the mode under test,
        # rather than silently falling back (which would make 0 mismatches
        # meaningless).
        used_counts = collections.Counter(r.get("used") for r in results)
        hard_fallbacks = collections.Counter(
            r.get("fallback_reason") for r in results
            if r.get("fallback_reason") in ("daemon_unavailable", "query_failed", "no_index_for_path")
        )
        vacuity_errors: list[str] = []
        if args.daemon:
            if not any(r.get("used") for r in results):
                vacuity_errors.append("no mode events recorded (WGREP_INDEX_EVENTS not observed)")
            elif used_counts.get("daemon", 0) == 0:
                vacuity_errors.append("--daemon run used the daemon for 0 invocations (parity would be vacuous)")
            if hard_fallbacks:
                vacuity_errors.append(f"daemon fallbacks that should have served: {dict(hard_fallbacks)}")
        elif used_counts.get("daemon", 0):
            vacuity_errors.append("non-daemon run used the daemon (WGREP_NO_DAEMON ignored)")

        summary = {
            "seed": SEED,
            "wgrep_exe": wgrep_exe,
            "rg_exe": rg_exe,
            "work": str(work),
            "index_dir": str(index_dir),
            "corpora": corpus_meta,
            "total_invocations": len(results),
            "total_mismatches": len(mismatches),
            "total_inconclusive": len(inconclusives),
            "total_reruns": len(reruns),
            "per_corpus_invocation_counts": dict(per_corpus_counts),
            "per_corpus_mismatch_counts": dict(per_corpus_mismatches),
            "per_corpus_inconclusive_counts": dict(per_corpus_inconclusive),
            "per_flag_mismatch_counts": {f"{c}:{f}": n for (c, f), n in per_flag_mismatches.items()},
            "prune_effectiveness": prune_effectiveness,
            "timing_seconds_by_corpus": {c: {"count": len(v), "total": sum(v), "mean": sum(v) / len(v) if v else 0.0}
                                          for c, v in timings.items()},
            "elapsed_total_seconds": elapsed_total,
            "mode_used_counts": {str(k): v for k, v in used_counts.items()},
            "hard_fallbacks": {str(k): v for k, v in hard_fallbacks.items()},
            "vacuity_errors": vacuity_errors,
            "mismatches": mismatches,
            "inconclusive": inconclusives,
            "reruns": reruns,
        }

        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(summary, indent=2, ensure_ascii=True) + "\n", encoding="utf-8")

        print()
        print("=" * 70)
        print(f"total invocations: {summary['total_invocations']}")
        print(f"total mismatches:  {summary['total_mismatches']}")
        print(f"total inconclusive (both sides timed out): {summary['total_inconclusive']}")
        print(f"total reruns (one-sided timeout, retried at {RERUN_TIMEOUT:.0f}s): {summary['total_reruns']}")
        print(f"elapsed:           {elapsed_total:.1f}s")
        for c, n in per_corpus_counts.items():
            print(f"  {c}: {n} invocations, {per_corpus_mismatches.get(c, 0)} mismatches, "
                  f"{per_corpus_inconclusive.get(c, 0)} inconclusive")
        if mismatches:
            print("\nfirst mismatches:")
            for m in mismatches[:20]:
                print(f"  [{m['corpus']}/{m['flag']}/{m['pattern_kind']}] {m['reason']}")
                print(f"      args={m['args']}")
        if inconclusives:
            print("\nfirst inconclusive:")
            for m in inconclusives[:20]:
                print(f"  [{m['corpus']}/{m['flag']}/{m['pattern_kind']}] {m['reason']}")
                print(f"      args={m['args']}")
        if reruns:
            print("\nreruns (one-sided timeout at 60s, retried at 300s):")
            for r in reruns[:20]:
                print(f"  [{r['corpus']}/{r['flag']}/{r['pattern_kind']}] "
                      f"first: rg_timeout={r['first_rg_timeout']} wgrep_timeout={r['first_wgrep_timeout']} -> "
                      f"rerun: rg_timeout={r['rerun_rg_timeout']} wgrep_timeout={r['rerun_wgrep_timeout']}")
                print(f"      args={r['args']}")
        print(f"\nmode usage: {summary['mode_used_counts']}")
        if vacuity_errors:
            print("\nANTI-VACUITY FAILURES:")
            for e in vacuity_errors:
                print(f"  - {e}")
        print(f"\nwrote {args.out}")
        print("=" * 70)

        # Exit policy: a mismatch always fails; an anti-vacuity failure fails
        # (3) so a silently-walking run cannot read as a pass; an unallowlisted
        # inconclusive fails (4) so a hang cannot read as a pass.
        if mismatches:
            return 1
        if vacuity_errors:
            return 3
        if inconclusives and not args.allow_inconclusive:
            return 4
        return 0
    finally:
        _stop_daemons()
        # Always sweep daemons left serving the corpora. Auto-start can leave
        # one behind even in default mode if a phase forgot WGREP_NO_DAEMON;
        # this is the safety net (a no-op when none is running).
        for root in daemon_roots:
            subprocess.run([wgrep_exe, "--stop", root], env={**os.environ, **env},
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except BaseException:
        import traceback
        crash_path = Path(f"E:/wgrep-parity/crash-{TS}.log")
        try:
            crash_path.write_text(traceback.format_exc(), encoding="utf-8")
        except OSError:
            pass
        traceback.print_exc()
        raise
