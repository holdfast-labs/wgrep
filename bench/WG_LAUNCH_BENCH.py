#!/usr/bin/env python3
"""wg launch benchmark: honest comparison-style measurements on one VPS.

Phase D campaign (2026-10-21) for the final single-binary ``wg``: semantic
ranking plus the built-in ripgrep 15.1 text route and its trigram index in one
daemon.

Measurements (all raw per-sample records retained):

1. Freshness: one-file edit -> first query that returns the new symbol.
   wg live daemon vs pinned sgrep CLI (its real end-to-end rebuild).
2. Warm semantic query latency: wg vs pinned sgrep CLI on the same corpus,
   same queries, warm caches; top-k rows compared exactly.
3. Agent fan-out: 1, 4, 16 concurrent clients, fixed duration, mixed queries.
   Throughput, p50/p95, errors/timeouts, daemon RSS vs summed sgrep RSS.
4. Multi-repo memory: 1, 3, 5 attached repos on one wg daemon vs one sgrep
   process per repo.
5. Text route: absent, rare, common, broad regex and case-insensitive cases
   against direct rg 15.1.0, with byte-exact sorted output parity and the
   index-mode event. Then 1, 4, 16 and 32 concurrent clients.
6. Setup: ``wg up`` wall time, peak RSS and steady RSS on the Rust corpus and
   (when enabled) the Linux kernel tree.

The optional ``demo`` phase keeps the earlier natural-language check; quality
numbers for the launch are in ``bench/WG_QUALITY_REPORT.md``.

Only this file writes outputs. It never edits the source corpus: it copies it.
Every subprocess has a timeout and a return-code check. Daemons and text
indexes are isolated by WG_SERVICE_DIR and WGREP_INDEX_DIR. Only task-owned
services are stopped.

Results JSON is flat: one key per headline metric, units in key names.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

DEFAULT_WG = os.environ.get(
    'WG_BIN', '/srv/projects/wispd/wg-d-20261021/target/release/wg')
DEFAULT_SGREP = '/srv/projects/wispd/sgrep-control-0fbe8e5/target/release/sgrep'
DEFAULT_RG = '/srv/workspaces/wispd/tools/rg-15/rg'
DEFAULT_CORPUS = '/srv/workspaces/wispd/prof/src'
DEFAULT_LINUX = '/srv/workspaces/wispd/corpora/linux-7.2.9'
DEFAULT_HF_HUB = '/home/agent/.cache/huggingface/hub'
DEFAULT_TS_PACK = '/home/agent/.cache/tree-sitter-language-pack'
CONTROL_SHA256 = '715946fa0f17006905b5f0d0bbf1f21c00e355882855ec40c44c14e78a3c6217'
RG_VERSION = '15.1.0'
MODEL = 'code'
SETTLE_TIMEOUT_S = 120.0

TEXT_ABSENT = 'zzqxjwvbnm_wg_text_absent_zzz'
TEXT_RARE_POOL = [
    'testable', 'latebound', 'associtem', 'genericargs', 'traitref',
    'implitem', 'tokenstream', 'diagnosticitem', 'unreachablepatterns',
    'hirplaceholder',
]
TEXT_COMMON_POOL = ['not', 'parse', 'error', 'self', 'impl', 'let']

# A ``-n`` (line-number) flag keeps the invocation on the text route: a bare
# PATTERN is semantic in ``wg``. ``-F`` also selects ripgrep.
TEXT_QUERY_SPECS = [
    {'name': 'absent_literal', 'args': ['-F'], 'pattern': TEXT_ABSENT},
    {'name': 'rare_identifier', 'args': ['-F'], 'pattern': None},
    {'name': 'common_identifier', 'args': ['-F'], 'pattern': None},
    {'name': 'broad_regex', 'args': ['-n'], 'pattern': r'\w+\('},
    {'name': 'case_insensitive', 'args': ['-i', '-F'], 'pattern': 'error'},
]

SEMANTIC_QUERIES = [
    'parse Rust expressions',
    'emit compiler error diagnostics',
    'validate command line options',
    'resolve module imports',
    'incremental compilation query dependency tracking',
]

# Answer locations were verified by hand on the copied corpus. ``line`` is the
# first line of the passage that answers the question.
DEMO_QUESTIONS = [
    {'id': 'q1', 'question': 'How does the parser handle unary expressions in Rust code?',
     'answer_path': 'tools/rust-analyzer/crates/parser/src/grammar/expressions.rs',
     'answer_line': 412},
    {'id': 'q2', 'question': 'Where does rustfmt decide to lay out a method chain vertically?',
     'answer_path': 'tools/rustfmt/src/chains.rs', 'answer_line': 709},
    {'id': 'q3', 'question': 'How does clippy find unnecessary clone calls on Copy types?',
     'answer_path': 'tools/clippy/clippy_lints/src/methods/clone_on_copy.rs',
     'answer_line': 61},
    {'id': 'q4', 'question': 'How does miri implement the Stacked Borrows aliasing model?',
     'answer_path': 'tools/miri/src/borrow_tracker/stacked_borrows/mod.rs',
     'answer_line': 467},
    {'id': 'q5', 'question': 'Where does rustdoc build the search index for documentation pages?',
     'answer_path': 'librustdoc/html/render/search_index.rs', 'answer_line': 1267},
    {'id': 'q6', 'question': 'How does bootstrap download a prebuilt LLVM from CI?',
     'answer_path': 'bootstrap/src/core/download.rs', 'answer_line': 356},
    {'id': 'q7', 'question': 'Where does compiletest compare the compiler output against expected files?',
     'answer_path': 'tools/compiletest/src/runtest.rs', 'answer_line': 2488},
    {'id': 'q8', 'question': 'How does rust-analyzer resolve a module import during name resolution?',
     'answer_path': 'tools/rust-analyzer/crates/hir-def/src/nameres/path_resolution.rs',
     'answer_line': 154},
]

STOPWORDS = {
    'a', 'an', 'and', 'are', 'as', 'at', 'be', 'by', 'can', 'code', 'does', 'do',
    'for', 'from', 'how', 'in', 'into', 'is', 'it', 'its', 'of', 'on', 'or',
    'the', 'their', 'them', 'this', 'to', 'use', 'used', 'using', 'what',
    'when', 'where', 'which', 'why', 'with', 'against', 'during',
}


class BenchError(RuntimeError):
    pass


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def percentile(values, p):
    if not values:
        return None
    xs = sorted(float(v) for v in values)
    if len(xs) == 1:
        return xs[0]
    k = (len(xs) - 1) * (p / 100.0)
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi:
        return xs[lo]
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def distribution(values):
    values = [float(v) for v in values if v is not None]
    if not values:
        return {'n': 0, 'p50_ms': None, 'p95_ms': None, 'min_ms': None, 'max_ms': None}
    return {
        'n': len(values),
        'p50_ms': percentile(values, 50),
        'p95_ms': percentile(values, 95),
        'min_ms': min(values),
        'max_ms': max(values),
    }


def read_rss_bytes(pid):
    try:
        with open(f'/proc/{int(pid)}/status', 'r', encoding='utf-8', errors='replace') as f:
            for line in f:
                if line.startswith('VmRSS:'):
                    return int(line.split()[1]) * 1024
    except (OSError, ValueError):
        return 0
    return 0


def child_pids(pid):
    try:
        text = Path(f'/proc/{int(pid)}/task/{int(pid)}/children').read_text()
    except OSError:
        return []
    return [int(x) for x in text.split() if x.isdigit()]


def tree_rss_bytes(pid):
    seen = set()
    stack = [int(pid)]
    total = 0
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        total += read_rss_bytes(current)
        stack.extend(child_pids(current))
    return total


def mib(nbytes):
    return nbytes / (1024.0 * 1024.0)


def parse_json_bytes(data, where):
    if isinstance(data, (bytes, bytearray)):
        data = bytes(data).decode('utf-8', errors='replace')
    try:
        return json.loads(data)
    except json.JSONDecodeError as exc:
        raise BenchError(f'{where}: invalid JSON ({exc})') from exc


def load_pid(path):
    return int(Path(path).read_text().strip())


def rows_have_answer_path(rows, path):
    want = path.replace('\\', '/').lstrip('./')
    return any(
        str(row.get('path', '')).replace('\\', '/').lstrip('./') == want
        for row in rows)


def rows_have_answer(rows, path, line):
    want = path.replace('\\', '/').lstrip('./')
    for row in rows:
        got = str(row.get('path', '')).replace('\\', '/').lstrip('./')
        if got != want:
            continue
        start = row.get('start')
        end = row.get('end')
        if isinstance(start, int) and isinstance(end, int) and start <= line <= end:
            return True
    return False


def rows_have_token(rows, token):
    return any(token in str(row.get('content', '')) for row in rows)


def normalize_row(row):
    return {
        'path': str(row.get('path', '')).replace('\\', '/').lstrip('./'),
        'start': row.get('start'),
        'end': row.get('end'),
        'content': row.get('content'),
    }


def compare_rows(control, candidate):
    issues = []
    if len(control) != len(candidate):
        issues.append(f'row count {len(control)} != {len(candidate)}')
    score_ok = True
    for i, (a, b) in enumerate(zip(control, candidate)):
        if normalize_row(a) != normalize_row(b):
            issues.append(f'row {i} identity differs')
            break
        if a.get('scores') != b.get('scores'):
            score_ok = False
    return issues, score_ok


def keyword_tokens(question):
    words = re.findall(r"[A-Za-z][A-Za-z0-9_'-]+", question.lower())
    out = []
    for word in words:
        word = word.strip("'-")
        if len(word) < 3 or word in STOPWORDS:
            continue
        if word not in out:
            out.append(word)
    return out


class ActiveRss:
    """Sample current RSS of a changing set of PIDs; keep peaks."""

    def __init__(self, interval=0.05, include_children=False):
        self.interval = interval
        self.include_children = include_children
        self.lock = threading.Lock()
        self.pids = set()
        self.peaks = {}
        self.current = 0
        self.sum_peak = 0
        self.samples = []
        self._stop = threading.Event()
        self._thread = None

    def register(self, pid):
        with self.lock:
            self.pids.add(int(pid))

    def unregister(self, pid):
        with self.lock:
            self.pids.discard(int(pid))

    def start(self):
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=2)

    def _loop(self):
        while not self._stop.is_set():
            with self.lock:
                pids = list(self.pids)
            total = 0
            for pid in pids:
                value = tree_rss_bytes(pid) if self.include_children else read_rss_bytes(pid)
                with self.lock:
                    if pid in self.pids:
                        self.peaks[pid] = max(self.peaks.get(pid, 0), value)
                total += value
            with self.lock:
                self.current = total
                self.sum_peak = max(self.sum_peak, total)
                self.samples.append(total)
            time.sleep(self.interval)

    def summary(self):
        with self.lock:
            return {
                'sum_peak_mib': mib(self.sum_peak),
                'process_peak_sum_mib': mib(sum(self.peaks.values())),
                'sampled_current_max_mib': mib(max(self.samples) if self.samples else 0),
                'processes': len(self.peaks),
            }


class SetupSampler(threading.Thread):
    """Sample the daemon RSS while ``wg up`` builds an index."""

    def __init__(self, service_dir, interval=0.05):
        super().__init__(daemon=True)
        self.service_dir = Path(service_dir)
        self.interval = interval
        self.stop_event = threading.Event()
        self.peak_bytes = 0
        self.samples = 0
        self.pid = None

    def _pid(self):
        if self.pid:
            return self.pid
        try:
            value = json.loads((self.service_dir / 'service.json').read_text(
                encoding='utf-8'))
        except (OSError, ValueError):
            return None
        pid = value.get('pid')
        if isinstance(pid, int):
            self.pid = pid
        return self.pid

    def run(self):
        while not self.stop_event.is_set():
            pid = self._pid()
            if pid:
                value = read_rss_bytes(pid)
                if value:
                    self.peak_bytes = max(self.peak_bytes, value)
                    self.samples += 1
            time.sleep(self.interval)

    def stop(self):
        self.stop_event.set()
        self.join(timeout=2)

    @property
    def peak_mib(self):
        return mib(self.peak_bytes)


class Bench:
    def __init__(self, args):
        self.args = args
        self.work = Path(args.work).resolve()
        self.corpus = self.work / 'corpus'
        self.fresh_corpus = self.work / 'freshness-corpus'
        self.repos = self.work / 'repos'
        self.sgrep_cache = self.work / 'sgrep-cache'
        self.raw = self.work / 'raw'
        self.raw.mkdir(parents=True, exist_ok=True)
        self.base_env = dict(os.environ)
        self.base_env['HF_HUB_OFFLINE'] = '1'
        self.base_env.pop('RIPGREP_CONFIG_PATH', None)
        self.base_env.pop('WG_SERVICE_EVENTS', None)
        self.base_env.pop('WG_SERVICE_DIR', None)
        self.base_env.pop('WGREP_INDEX_DIR', None)
        self.base_env.pop('WGREP_INDEX_EVENTS', None)
        self.base_env.pop('WGREP_NO_DAEMON', None)
        self.base_env.pop('WGREP_NO_INDEX', None)
        self.base_env['SGREP_CACHE_MAX_BYTES'] = str(4 * 1024 * 1024 * 1024)
        for key in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE'):
            self.base_env.pop(key, None)
        self.command_log = self.raw / 'commands.jsonl'
        self._log_lock = threading.Lock()
        self._command_count = 0
        self.started_services = []
        self.flags = {}

    # -- process helpers -------------------------------------------------
    def index_dir_for(self, service_dir):
        service_dir = Path(service_dir)
        return self.work / 'index' / service_dir.name

    def env_for(self, service_dir=None, events=None):
        env = dict(self.base_env)
        if service_dir is not None:
            service_dir = Path(service_dir)
            env['WG_SERVICE_DIR'] = str(service_dir)
            env['WGREP_INDEX_DIR'] = str(self.index_dir_for(service_dir))
        if events is not None:
            env['WGREP_INDEX_EVENTS'] = str(events)
        return env

    def _log_command(self, record):
        with self._log_lock:
            self._command_count += 1
            record['n'] = self._command_count
            with open(self.command_log, 'a', encoding='utf-8') as f:
                f.write(json.dumps(record, ensure_ascii=False) + '\n')

    def run(self, argv, cwd=None, timeout=60, tag='cmd', service_dir=None, events=None):
        argv = [str(x) for x in argv]
        start = time.perf_counter()
        timed_out = False
        try:
            proc = subprocess.run(
                argv,
                cwd=str(cwd) if cwd is not None else None,
                env=self.env_for(service_dir, events),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
            )
            rc, out, err = proc.returncode, proc.stdout, proc.stderr
        except subprocess.TimeoutExpired as exc:
            timed_out = True
            rc = None
            out = exc.stdout or b''
            err = exc.stderr or b''
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        self._log_command({
            'tag': tag, 'argv': argv, 'cwd': str(cwd) if cwd is not None else None,
            'exit': rc, 'timed_out': timed_out, 'elapsed_ms': elapsed_ms,
            'stdout_bytes': len(out), 'stderr_bytes': len(err),
            'stderr_head': err[:300].decode('utf-8', errors='replace'),
        })
        return {'argv': argv, 'cwd': str(cwd) if cwd is not None else None,
                'exit': rc, 'timed_out': timed_out, 'elapsed_ms': elapsed_ms,
                'stdout': out, 'stderr': err}

    def require(self, condition, message):
        if not condition:
            raise BenchError(message)

    def checked(self, argv, cwd=None, timeout=120, tag='cmd', allow=(0,), service_dir=None):
        proc = self.run(argv, cwd=cwd, timeout=timeout, tag=tag, service_dir=service_dir)
        if proc['timed_out']:
            raise BenchError(f'{tag}: timeout after {timeout}s: {argv}')
        if proc['exit'] not in allow:
            raise BenchError(f'{tag}: exit {proc["exit"]}: {argv}: '
                             f'{proc["stderr"][:300]!r}')
        return proc

    def append_raw(self, name, record):
        path = self.raw / f'{name}.jsonl'
        with self._log_lock:
            with open(path, 'a', encoding='utf-8') as f:
                f.write(json.dumps(record, ensure_ascii=False) + '\n')

    # -- setup -----------------------------------------------------------
    def setup(self):
        self.raw.mkdir(parents=True, exist_ok=True)
        for path in (self.corpus, self.fresh_corpus):
            if not path.exists():
                print(f'setup: copying corpus -> {path}', flush=True)
                self.checked(['cp', '-a', self.args.corpus_source, str(path)],
                             tag='copy-corpus', allow=(0,), timeout=600)
        self.prepare_sgrep_cache()
        self.flags['corpus_files'] = sum(1 for _ in self.corpus.rglob('*') if _.is_file())
        self.flags['freshness_corpus_files'] = sum(1 for _ in self.fresh_corpus.rglob('*') if _.is_file())
        print(f"setup: corpus files={self.flags['corpus_files']}", flush=True)

    def prepare_sgrep_cache(self):
        marker = self.sgrep_cache / '.prepared'
        if marker.exists():
            return
        self.sgrep_cache.mkdir(parents=True, exist_ok=True)
        model_dir = 'models--minishlab--potion-code-16M-v2'
        hub_src = Path(self.args.hf_hub) / model_dir
        self.require(hub_src.is_dir(), f'model cache missing: {hub_src}')
        hub_dst = self.sgrep_cache / 'hub' / model_dir
        hub_dst.parent.mkdir(parents=True, exist_ok=True)
        if not hub_dst.exists():
            self.checked(['cp', '-a', str(hub_src), str(hub_dst)], tag='copy-model-cache',
                         allow=(0,), timeout=300)
        version = 'v1.21.0'
        ts_src = Path(self.args.ts_pack) / version / 'libs'
        self.require(ts_src.is_dir(), f'tree-sitter parser cache missing: {ts_src}')
        ts_dst = self.sgrep_cache / 'tree-sitter-language-pack' / version / 'libs'
        ts_dst.parent.mkdir(parents=True, exist_ok=True)
        if not ts_dst.exists():
            self.checked(['cp', '-a', str(ts_src), str(ts_dst)], tag='copy-parser-cache',
                         allow=(0,), timeout=300)
        marker.write_text('ok\n')

    # -- services --------------------------------------------------------
    def start_service(self, root, sid):
        service_dir = self.work / 'service' / sid
        service_dir.mkdir(parents=True, exist_ok=True)
        self.index_dir_for(service_dir).mkdir(parents=True, exist_ok=True)
        if service_dir in self.started_services:
            raise BenchError(f'service already started: {service_dir}')
        self.started_services.append(service_dir)
        proc = self.checked([self.args.wg, 'up', str(root), '--model', MODEL, '--json'],
                            timeout=self.args.up_timeout, tag=f'up-{sid}',
                            allow=(0,), service_dir=service_dir)
        status = self.status(service_dir)
        self.require(status.get('ok') is True, f'up-{sid}: status not ok: {status}')
        self.require(status.get('pid'), f'up-{sid}: no daemon pid')
        settle = self.settle_text_index(service_dir, sid)
        self.flags.setdefault('settles', {})[sid] = settle
        return service_dir, status

    def settle_text_index(self, service_dir, sid):
        started = time.monotonic()
        deadline = started + SETTLE_TIMEOUT_S
        text = {}
        while True:
            status = self.status(service_dir)
            roots = status.get('roots') or []
            text = (roots[0].get('text') if roots else None) or {}
            if 'building' not in text and 'pending' not in text:
                break
            building = text.get('building')
            pending = text.get('pending')
            if not building and pending in (0, None):
                break
            if time.monotonic() >= deadline:
                raise BenchError(
                    f'up-{sid}: text index did not settle within '
                    f'{SETTLE_TIMEOUT_S:.0f} s (building={building!r}, '
                    f'pending={pending!r}, indexed={text.get("indexed")!r}, '
                    f'unindexed={text.get("unindexed")!r})')
            time.sleep(0.1)
        elapsed = time.monotonic() - started
        print(f'up-{sid}: text index settled in {elapsed:.3f}s '
              f'indexed={text.get("indexed")} unindexed={text.get("unindexed")}',
              flush=True)
        return {'seconds': elapsed, 'indexed': text.get('indexed'),
                'unindexed': text.get('unindexed')}

    def status(self, service_dir):
        proc = self.run([self.args.wg, 'status', '--json'], timeout=30, tag='status',
                        service_dir=service_dir)
        if proc['timed_out'] or proc['exit'] != 0:
            raise BenchError(f'status failed: exit={proc["exit"]} timeout={proc["timed_out"]} '
                             f'{proc["stderr"][:200]!r}')
        return parse_json_bytes(proc['stdout'], 'status')

    def stop_service(self, service_dir, sid):
        down = self.run([self.args.wg, 'down'], timeout=30, tag=f'down-{sid}',
                        service_dir=service_dir)
        time.sleep(0.1)
        gone = False
        pid = None
        try:
            status = self.status(service_dir)
            pid = status.get('pid')
        except BenchError:
            gone = True
        if not gone and pid:
            try:
                os.kill(int(pid), signal.SIGKILL)
            except OSError:
                pass
            deadline = time.monotonic() + 5.0
            while time.monotonic() < deadline:
                try:
                    self.status(service_dir)
                except BenchError:
                    gone = True
                    break
                time.sleep(0.05)
        if service_dir in self.started_services:
            self.started_services.remove(service_dir)
        return {'sid': sid, 'down_exit': down['exit'],
                'down_timed_out': down['timed_out'], 'daemon_gone': gone,
                'killed': bool(pid and not gone)}

    def stop_all(self):
        results = []
        for service_dir in reversed(self.started_services):
            sid = service_dir.name
            results.append(self.stop_service(service_dir, sid))
        self.started_services.clear()
        return results

    # -- argv builders ---------------------------------------------------
    def wg_semantic(self, query, root, top=None, service_dir=None):
        return ([self.args.wg, query, str(root), '--model', MODEL, '--json',
                 '--explain', '--top', str(top or self.args.top)], service_dir)

    def sgrep_semantic(self, query, root, top=None):
        return [self.args.sgrep, query, str(root), '--model', MODEL, '--json',
                '--explain', '--cache-dir', str(self.sgrep_cache),
                '-n', str(top or self.args.top)]

    def sgrep_prime(self, root):
        proc = self.checked(self.sgrep_semantic(SEMANTIC_QUERIES[0], root, 1),
                            timeout=self.args.sgrep_build_timeout, tag='sgrep-prime')
        rows = parse_json_bytes(proc['stdout'], 'sgrep-prime')
        self.require(isinstance(rows, list) and rows, 'sgrep-prime returned no rows')

    # -- phase: setup ----------------------------------------------------
    def _sample_up(self, name, root, keep):
        sid = f'setup-{name}'
        service_dir = self.work / 'service' / sid
        service_dir.mkdir(parents=True, exist_ok=True)
        self.index_dir_for(service_dir).mkdir(parents=True, exist_ok=True)
        self.started_services.append(service_dir)
        sampler = SetupSampler(service_dir)
        sampler.start()
        record = {'corpus': name, 'root': str(root), 'keep': keep}
        try:
            started = time.perf_counter()
            proc = self.checked(
                [self.args.wg, 'up', str(root), '--model', MODEL, '--json'],
                timeout=self.args.large_up_timeout, tag=f'setup-up-{name}',
                allow=(0,), service_dir=service_dir)
            record['up_ms'] = (time.perf_counter() - started) * 1000.0
            record['up_exit'] = proc['exit']
            try:
                info = parse_json_bytes(proc['stdout'], f'setup-up-{name}')
            except BenchError as exc:
                info = {}
                record['error'] = str(exc)
            root_info = info.get('root') or {}
            record['pid'] = info.get('pid')
            record['files'] = root_info.get('files')
            record['chunks'] = root_info.get('chunks')
            record['build_ms'] = info.get('build_ms')
            record['model_ms'] = info.get('model_ms')
            record['total_ms'] = info.get('total_ms')
            text = root_info.get('text') if isinstance(root_info, dict) else None
            record['text_files'] = text.get('files') if isinstance(text, dict) else None
            record['text_build_ms'] = text.get('build_ms') if isinstance(text, dict) else None
            record['text_disk_bytes'] = text.get('disk_bytes') if isinstance(text, dict) else None
            record['text_resident_bytes'] = (text.get('resident_bytes')
                                              if isinstance(text, dict) else None)
            record['text_error'] = root_info.get('text_error')
            time.sleep(2.0)
            if record.get('pid'):
                record['ready_rss_mib'] = mib(tree_rss_bytes(record['pid']))
            status = self.status(service_dir)
            record['ready'] = bool(
                (status.get('roots') or [{}])[0].get('ready'))
        finally:
            sampler.stop()
            record['peak_rss_mib'] = sampler.peak_mib
            record['rss_samples'] = sampler.samples
            record['cleanup'] = self.stop_service(service_dir, sid)
            if not keep:
                shutil.rmtree(self.index_dir_for(service_dir), ignore_errors=True)
                shutil.rmtree(service_dir, ignore_errors=True)
        self.append_raw('setup', record)
        self.flags.setdefault('setup_records', []).append(record)
        print(f"setup {name}: up={record.get('up_ms', 0) / 1000.0:.1f}s "
              f"peak={record.get('peak_rss_mib', 0):.0f} MiB "
              f"ready={record.get('ready')} files={record.get('files')}", flush=True)
        return record

    def phase_setup(self):
        self._sample_up('rust', self.corpus, keep=False)
        linux = self.args.linux_corpus
        if linux and not self.args.skip_linux_setup:
            linux_path = Path(linux)
            if linux_path.is_dir():
                self._sample_up('linux', linux_path, keep=False)
            else:
                self.flags['setup_linux_skipped'] = f'missing: {linux}'
        else:
            self.flags['setup_linux_skipped'] = 'disabled'

    # -- phase: warm semantic -------------------------------------------
    def phase_warm(self):
        service_dir, status = self.start_service(self.corpus, 'warm')
        pid = int(status['pid'])
        roots = status.get('roots') or []
        if roots:
            self.flags['wg_files'] = roots[0].get('files')
            self.flags['wg_chunks'] = roots[0].get('chunks')
        records = []
        try:
            self.sgrep_prime(self.corpus)
            for q in SEMANTIC_QUERIES:
                for _ in range(self.args.warmup):
                    self.run(self.wg_semantic(q, self.corpus, service_dir=service_dir)[0],
                             timeout=60, tag='warmup-wg', service_dir=service_dir)
                    self.run(self.sgrep_semantic(q, self.corpus), timeout=60, tag='warmup-sgrep')
            parity = []
            for q in SEMANTIC_QUERIES:
                wg_proc = self.run(self.wg_semantic(q, self.corpus, service_dir=service_dir)[0],
                                   timeout=60, tag='parity-wg', service_dir=service_dir)
                sg_proc = self.run(self.sgrep_semantic(q, self.corpus), timeout=60,
                                   tag='parity-sgrep')
                try:
                    wg_rows = parse_json_bytes(wg_proc['stdout'], 'parity-wg')
                    sg_rows = parse_json_bytes(sg_proc['stdout'], 'parity-sgrep')
                    issues, scores_ok = compare_rows(sg_rows, wg_rows)
                except BenchError as exc:
                    issues, scores_ok = [str(exc)], False
                    wg_rows, sg_rows = [], []
                parity.append({'query': q, 'issues': issues, 'scores_exact': scores_ok,
                               'wg_rows': len(wg_rows), 'sgrep_rows': len(sg_rows)})
            (self.raw / 'rank_parity.json').write_text(
                json.dumps(parity, indent=2, ensure_ascii=False) + '\n', encoding='utf-8')

            for sample in range(self.args.semantic_samples):
                q = SEMANTIC_QUERIES[sample % len(SEMANTIC_QUERIES)]
                for tool in ('wg', 'sgrep'):
                    if tool == 'wg':
                        argv = self.wg_semantic(q, self.corpus, service_dir=service_dir)[0]
                        proc = self.run(argv, timeout=60, tag='warm-wg',
                                        service_dir=service_dir)
                    else:
                        argv = self.sgrep_semantic(q, self.corpus)
                        proc = self.run(argv, timeout=60, tag='warm-sgrep')
                    error = None
                    rows = None
                    if proc['timed_out']:
                        error = 'timeout'
                    elif proc['exit'] != 0:
                        error = f'exit {proc["exit"]}'
                    else:
                        try:
                            rows = parse_json_bytes(proc['stdout'], f'warm-{tool}')
                            if not isinstance(rows, list) or not rows:
                                error = 'empty rows'
                        except BenchError as exc:
                            error = str(exc)
                    record = {'sample': sample, 'query': q, 'tool': tool,
                              'ms': None if error else proc['elapsed_ms'],
                              'error': error, 'rows': len(rows) if rows is not None else None}
                    records.append(record)
                    self.append_raw('warm_semantic', record)
        finally:
            self.flags['warm_cleanup'] = self.stop_service(service_dir, 'warm')
        self.flags['warm_records'] = records
        self.flags['warm_parity'] = parity
        self.flags['warm_pid'] = pid

    # -- phase: fan-out --------------------------------------------------
    def _wg_fanout_worker(self, worker_id, service_dir, queries, deadline, records, lock):
        i = worker_id
        while time.monotonic() < deadline:
            q = queries[i % len(queries)]
            i += 1
            proc = self.run(self.wg_semantic(q, self.corpus, service_dir=service_dir)[0],
                            timeout=self.args.request_timeout, tag='fanout-wg',
                            service_dir=service_dir)
            error = None
            rows = None
            if proc['timed_out']:
                error = 'timeout'
            elif proc['exit'] != 0:
                error = f'exit {proc["exit"]}'
            else:
                try:
                    rows = parse_json_bytes(proc['stdout'], 'fanout-wg')
                    if not isinstance(rows, list) or not rows:
                        error = 'empty rows'
                except BenchError as exc:
                    error = str(exc)
            with lock:
                records.append({'worker': worker_id, 'query': q,
                                'ms': None if error else proc['elapsed_ms'],
                                'error': error})

    def _sgrep_fanout_worker(self, worker_id, queries, deadline, records, lock, active):
        i = worker_id
        while time.monotonic() < deadline:
            q = queries[i % len(queries)]
            i += 1
            argv = self.sgrep_semantic(q, self.corpus)
            start = time.perf_counter()
            error = None
            rows = None
            timed_out = False
            try:
                proc = subprocess.Popen(argv, cwd=None, env=self.env_for(),
                                        stdin=subprocess.DEVNULL,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                active.register(proc.pid)
                try:
                    out, err = proc.communicate(timeout=self.args.request_timeout)
                    rc = proc.returncode
                except subprocess.TimeoutExpired:
                    proc.kill()
                    out, err = proc.communicate()
                    rc = None
                    timed_out = True
                active.unregister(proc.pid)
                elapsed_ms = (time.perf_counter() - start) * 1000.0
                self._log_command({'tag': 'fanout-sgrep', 'argv': argv, 'exit': rc,
                                   'timed_out': timed_out, 'elapsed_ms': elapsed_ms,
                                   'stdout_bytes': len(out), 'stderr_bytes': len(err),
                                   'stderr_head': err[:300].decode('utf-8', errors='replace')})
                if timed_out:
                    error = 'timeout'
                elif rc != 0:
                    error = f'exit {rc}'
                else:
                    try:
                        rows = parse_json_bytes(out, 'fanout-sgrep')
                        if not isinstance(rows, list) or not rows:
                            error = 'empty rows'
                    except BenchError as exc:
                        error = str(exc)
            except OSError as exc:
                elapsed_ms = (time.perf_counter() - start) * 1000.0
                error = str(exc)
            with lock:
                records.append({'worker': worker_id, 'query': q,
                                'ms': None if error else elapsed_ms, 'error': error})

    def phase_fanout(self):
        service_dir, status = self.start_service(self.corpus, 'fanout')
        pid = int(status['pid'])
        self.sgrep_prime(self.corpus)
        all_levels = []
        try:
            for clients in self.args.fanout_clients:
                for _ in range(self.args.warmup):
                    self.run(self.wg_semantic(SEMANTIC_QUERIES[0], self.corpus,
                                              service_dir=service_dir)[0],
                             timeout=60, tag='fanout-warmup-wg', service_dir=service_dir)
                    self.run(self.sgrep_semantic(SEMANTIC_QUERIES[0], self.corpus),
                             timeout=60, tag='fanout-warmup-sgrep')
                level = {}
                for tool in ('wg', 'sgrep'):
                    records = []
                    lock = threading.Lock()
                    barrier = threading.Barrier(clients + 1)
                    deadline_holder = {'end': 0.0}

                    def worker(w, tool=tool, records=records, lock=lock,
                               barrier=barrier, deadline_holder=deadline_holder):
                        barrier.wait()
                        if tool == 'wg':
                            self._wg_fanout_worker(w, service_dir, SEMANTIC_QUERIES,
                                                   deadline_holder['end'], records, lock)
                        else:
                            self._sgrep_fanout_worker(w, SEMANTIC_QUERIES,
                                                      deadline_holder['end'], records, lock,
                                                      active)

                    active = ActiveRss(interval=0.05) if tool == 'sgrep' else None
                    if tool == 'sgrep':
                        active.start()
                    monitor = ActiveRss(interval=0.1, include_children=True) if tool == 'wg' else None
                    if monitor:
                        monitor.register(pid)
                        monitor.start()
                    threads = [threading.Thread(target=worker, args=(w,), daemon=True)
                               for w in range(clients)]
                    window_start = time.monotonic()
                    deadline_holder['end'] = window_start + self.args.fanout_seconds
                    for thread in threads:
                        thread.start()
                    barrier.wait()
                    for thread in threads:
                        thread.join()
                    window_end = time.monotonic()
                    if monitor:
                        monitor.stop()
                        rss = monitor.summary()
                    else:
                        active.stop()
                        rss = active.summary()
                    wall_s = max(1e-9, window_end - window_start)
                    ok = [r for r in records if r['error'] is None]
                    latencies = [r['ms'] for r in ok]
                    errors = [r['error'] for r in records if r['error'] is not None]
                    timeout_errors = sum(1 for e in errors if e == 'timeout')
                    dist = distribution(latencies)
                    summary = {
                        'clients': clients, 'tool': tool, 'window_s': wall_s,
                        'requests': len(records), 'successes': len(ok),
                        'throughput_qps': len(ok) / wall_s,
                        'p50_ms': dist['p50_ms'], 'p95_ms': dist['p95_ms'],
                        'errors': len(errors), 'timeouts': timeout_errors,
                        'rss_peak_mib': rss['sum_peak_mib'],
                        'rss_processes': rss['processes'],
                    }
                    level[tool] = summary
                    self.append_raw('fanout', summary)
                    for record in records:
                        record.update({'clients': clients, 'tool': tool})
                        self.append_raw('fanout_requests', record)
                all_levels.append(level)
                print(f"fanout: clients={clients} "
                      f"wg={level['wg']['throughput_qps']:.2f} qps "
                      f"sgrep={level['sgrep']['throughput_qps']:.2f} qps", flush=True)
        finally:
            self.flags['fanout_cleanup'] = self.stop_service(service_dir, 'fanout')
        self.flags['fanout_levels'] = all_levels
        self.flags['fanout_pid'] = pid

    # -- phase: multi-repo ----------------------------------------------
    def ensure_repo_copies(self, count):
        self.repos.mkdir(parents=True, exist_ok=True)
        for i in range(1, count + 1):
            dst = self.repos / f'repo{i}'
            if not dst.exists():
                print(f'setup: copying repo{i}', flush=True)
                self.checked(['cp', '-a', str(self.corpus), str(dst)], tag=f'copy-repo{i}',
                             allow=(0,), timeout=600)
        return [self.repos / f'repo{i}' for i in range(1, count + 1)]

    def phase_multirepo(self):
        max_repos = max(self.args.repo_counts)
        repos = self.ensure_repo_copies(max_repos)
        service_dir, status = self.start_service(repos[0], 'multirepo')
        pid = int(status['pid'])
        levels = []
        try:
            attached = 1
            for count in self.args.repo_counts:
                while attached < count:
                    repo = repos[attached]
                    self.checked([self.args.wg, 'up', str(repo), '--model', MODEL, '--json'],
                                 timeout=self.args.up_timeout, tag=f'up-repo{attached + 1}',
                                 allow=(0,), service_dir=service_dir)
                    attached += 1
                current = self.status(service_dir)
                self.require(len(current.get('roots') or []) == count,
                             f'multirepo: expected {count} roots, got {len(current.get("roots") or [])}')
                monitor = ActiveRss(interval=0.1, include_children=True)
                monitor.register(pid)
                monitor.start()
                for i in range(5):
                    repo = repos[i % count]
                    self.run(self.wg_semantic(SEMANTIC_QUERIES[i % len(SEMANTIC_QUERIES)],
                                              repo, service_dir=service_dir)[0],
                             timeout=60, tag='multirepo-wg', service_dir=service_dir)
                time.sleep(0.5)
                monitor.stop()
                rss = monitor.summary()
                idle = mib(tree_rss_bytes(pid))
                level = {'count': count, 'wg_rss_idle_mib': idle,
                         'wg_rss_query_peak_mib': rss['sum_peak_mib'],
                         'wg_rss_process_peak_sum_mib': rss['process_peak_sum_mib'],
                         'wg_roots': len(current.get('roots') or []),
                         'wg_pid': pid}
                levels.append(level)
                self.append_raw('multirepo', level)
                print(f"multirepo: {count} root(s) wg idle={idle:.1f} MiB", flush=True)
        finally:
            self.flags['multirepo_cleanup'] = self.stop_service(service_dir, 'multirepo')

        sgrep_levels = []
        for repo in repos:
            print(f'multirepo: priming sgrep {repo.name}', flush=True)
            self.sgrep_prime(repo)
        for count in self.args.repo_counts:
            active = ActiveRss(interval=0.05)
            active.start()
            popens = []
            for i in range(count):
                argv = self.sgrep_semantic(SEMANTIC_QUERIES[i % len(SEMANTIC_QUERIES)], repos[i])
                p = subprocess.Popen(argv, env=self.env_for(), stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                active.register(p.pid)
                popens.append((i, p))
            errors = 0
            for i, p in popens:
                try:
                    out, err = p.communicate(timeout=self.args.request_timeout)
                    rc = p.returncode
                except subprocess.TimeoutExpired:
                    p.kill()
                    out, err = p.communicate()
                    rc = None
                    errors += 1
                active.unregister(p.pid)
                if rc != 0:
                    errors += 1
                try:
                    rows = parse_json_bytes(out, f'multirepo-sgrep-{i}')
                    if not isinstance(rows, list) or not rows:
                        errors += 1
                except BenchError:
                    errors += 1
            active.stop()
            rss = active.summary()
            level = {'count': count,
                     'sgrep_rss_sum_peak_mib': rss['sum_peak_mib'],
                     'sgrep_rss_process_peak_sum_mib': rss['process_peak_sum_mib'],
                     'sgrep_processes': count, 'sgrep_errors': errors}
            sgrep_levels.append(level)
            self.append_raw('multirepo', level)
            print(f"multirepo: {count} sgrep process(es) "
                  f"sum_peak={rss['sum_peak_mib']:.1f} MiB errors={errors}", flush=True)
        self.flags['multirepo_levels'] = levels
        self.flags['multirepo_sgrep_levels'] = sgrep_levels

    # -- phase: text route ----------------------------------------------
    def _rg_file_count(self, corpus, pattern, extra=()):
        proc = self.run([self.args.rg, *extra, '-F', '-l', '--', pattern, '.'],
                        cwd=corpus, timeout=120, tag='text-select-rg')
        if proc['timed_out'] or proc['exit'] not in (0, 1):
            return -1
        return len(proc['stdout'].splitlines())

    def choose_text_queries(self, corpus):
        rare_counts = {w: self._rg_file_count(corpus, w) for w in TEXT_RARE_POOL}
        present = [(n, w) for w, n in rare_counts.items() if 0 < n <= 60]
        rare = min(present)[1] if present else TEXT_RARE_POOL[0]
        common_counts = {w: self._rg_file_count(corpus, w) for w in TEXT_COMMON_POOL}
        common = max(common_counts, key=lambda w: common_counts[w])
        queries = []
        for spec in TEXT_QUERY_SPECS:
            item = dict(spec)
            if item['name'] == 'rare_identifier':
                item['pattern'] = rare
            elif item['name'] == 'common_identifier':
                item['pattern'] = common
            item['argv'] = [*item['args'], '--', item['pattern'], '.']
            queries.append(item)
        self.flags['text_selection'] = {
            'rare': rare, 'rare_counts': rare_counts,
            'common': common, 'common_counts': common_counts,
        }
        return queries

    def _text_parity(self, queries, service_dir):
        parity = []
        for query in queries:
            args = [*query['args'], '--sort', 'path', '--color', 'never',
                    '--', query['pattern'], '.']
            events = self.raw / 'events' / f"parity-{query['name']}.ndjson"
            events.parent.mkdir(parents=True, exist_ok=True)
            events.unlink(missing_ok=True)
            rg_res = self.run([self.args.rg, *args], cwd=self.corpus,
                              timeout=180, tag=f"text-parity-rg-{query['name']}")
            wg_res = self.run([self.args.wg, *args], cwd=self.corpus,
                              timeout=180, tag=f"text-parity-wg-{query['name']}",
                              service_dir=service_dir, events=events)
            event = None
            try:
                lines = [json.loads(line) for line in
                         events.read_text(encoding='utf-8').splitlines() if line.strip()]
                event = next((e for e in lines if e.get('event') == 'search'), None)
                event_count = sum(1 for e in lines if e.get('event') == 'search')
            except (OSError, ValueError):
                event_count = 0
            ok_codes = (rg_res['exit'] in (0, 1) and wg_res['exit'] in (0, 1)
                        and not rg_res['timed_out'] and not wg_res['timed_out'])
            exact = (ok_codes and rg_res['exit'] == wg_res['exit']
                     and rg_res['stdout'] == wg_res['stdout'])
            row = {
                'pattern': query['name'], 'text_pattern': query['pattern'],
                'args': args, 'exact': exact,
                'rg_exit': rg_res['exit'], 'wg_exit': wg_res['exit'],
                'rg_bytes': len(rg_res['stdout']), 'wg_bytes': len(wg_res['stdout']),
                'rg_lines': len(rg_res['stdout'].splitlines()),
                'wg_lines': len(wg_res['stdout'].splitlines()),
                'used': event.get('used') if event else None,
                'reason': event.get('reason') if event else None,
                'event_count': event_count,
                'errors': [name for name, res in (('rg', rg_res), ('wg', wg_res))
                           if res['timed_out'] or res['exit'] not in (0, 1)],
            }
            parity.append(row)
            print(f"text parity {query['name']}: exact={exact} "
                  f"used={row['used']} bytes={row['wg_bytes']}", flush=True)
        (self.raw / 'text_parity.json').write_text(
            json.dumps(parity, indent=2, ensure_ascii=False) + '\n', encoding='utf-8')
        self.flags['text_parity'] = parity

    def _text_timed(self, tool, argv, service_dir, timeout):
        started = time.perf_counter()
        env = self.env_for(service_dir if tool == 'wg' else None)
        try:
            proc = subprocess.run(
                [str(a) for a in argv], cwd=str(self.corpus), env=env,
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE, timeout=timeout)
            code, timed_out = proc.returncode, False
        except subprocess.TimeoutExpired:
            code, timed_out = None, True
        return {'ms': (time.perf_counter() - started) * 1000.0,
                'exit': code, 'timed_out': timed_out}

    def _text_arm(self, tool, queries, clients, per_client, timeout):
        records = []
        lock = threading.Lock()

        def agent(index):
            for step in range(per_client):
                query = queries[(index + step) % len(queries)]
                res = self._text_timed(tool, [self.args.wg if tool == 'wg' else self.args.rg,
                                              *query['argv']],
                                       self._text_service, timeout)
                with lock:
                    records.append({'tool': tool, 'clients': clients,
                                    'query': query['name'], 'ms': res['ms'],
                                    'exit': res['exit'], 'timed_out': res['timed_out']})

        started = time.perf_counter()
        threads = [threading.Thread(target=agent, args=(i,)) for i in range(clients)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        makespan = time.perf_counter() - started
        ok = [r for r in records if not r['timed_out'] and r['exit'] in (0, 1)]
        dist = distribution([r['ms'] for r in ok])
        return {
            'clients': clients, 'tool': tool,
            'requests': len(records), 'successes': len(ok),
            'errors': len(records) - len(ok),
            'throughput_qps': len(ok) / max(1e-9, makespan),
            'p50_ms': dist['p50_ms'], 'p95_ms': dist['p95_ms'],
        }, records

    def phase_text(self):
        service_dir, status = self.start_service(self.corpus, 'text')
        self._text_service = service_dir
        pid = int(status['pid'])
        records = []
        concurrency = []
        try:
            queries = self.choose_text_queries(self.corpus)
            for query in queries:
                for _ in range(self.args.warmup):
                    self._text_timed('wg', [self.args.wg, *query['argv']],
                                     service_dir, 180)
                    self._text_timed('rg', [self.args.rg, *query['argv']],
                                     None, 180)
            self._text_parity(queries, service_dir)
            for sample in range(self.args.text_samples):
                for qi, query in enumerate(queries):
                    order = (('rg', self.args.rg), ('wg', self.args.wg))
                    if (sample + qi) % 2:
                        order = tuple(reversed(order))
                    for tool, exe in order:
                        proc = self._text_timed(
                            tool, [exe, *query['argv']],
                            service_dir if tool == 'wg' else None, 180)
                        error = None
                        if proc['timed_out']:
                            error = 'timeout'
                        elif proc['exit'] not in (0, 1):
                            error = f"exit {proc['exit']}"
                        record = {'sample': sample, 'pattern': query['name'],
                                  'text_pattern': query['pattern'], 'tool': tool,
                                  'ms': None if error else proc['ms'],
                                  'error': error, 'exit': proc['exit']}
                        records.append(record)
                        self.append_raw('text_route', record)
            time.sleep(1.0)
            self.flags['text_daemon_rss_mib'] = mib(tree_rss_bytes(pid))

            mix = [q for q in queries
                   if q['name'] in ('absent_literal', 'rare_identifier',
                                    'common_identifier', 'broad_regex')]
            for clients in self.args.text_clients:
                for tool in ('wg', 'rg'):
                    row, raw = self._text_arm(tool, mix, clients,
                                              self.args.text_per_client,
                                              self.args.request_timeout)
                    row['reps'] = 1
                    concurrency.append(row)
                    self.append_raw('text_concurrency', row)
                    for item in raw:
                        self.append_raw('text_concurrency_requests', item)
                    print(f"text fanout {clients} {tool}: "
                          f"{row['throughput_qps']:.1f} q/s "
                          f"p50={row['p50_ms']:.0f} ms errors={row['errors']}",
                          flush=True)
        finally:
            self.flags['text_cleanup'] = self.stop_service(service_dir, 'text')
        self.flags['text_records'] = records
        self.flags['text_concurrency'] = concurrency
        self.flags['text_pid'] = pid

    # -- phase: demo questions ------------------------------------------
    def phase_demo(self):
        service_dir, status = self.start_service(self.corpus, 'demo')
        results = []
        demo_raw = self.raw / 'demo'
        demo_raw.mkdir(exist_ok=True)
        try:
            for item in DEMO_QUESTIONS:
                qid = item['id']
                question = item['question']
                wg_argv, _ = self.wg_semantic(question, self.corpus, service_dir=service_dir)
                wg_proc = self.checked(wg_argv, timeout=60, tag=f'demo-wg-{qid}',
                                       service_dir=service_dir)
                rows = parse_json_bytes(wg_proc['stdout'], f'demo-wg-{qid}')
                (demo_raw / f'{qid}-wg.json').write_bytes(wg_proc['stdout'])
                top1_path = rows_have_answer_path(rows[:1], item['answer_path'])
                top5_path = rows_have_answer_path(rows[:5], item['answer_path'])
                top1_line = rows_have_answer(rows[:1], item['answer_path'], item['answer_line'])
                top5_line = rows_have_answer(rows[:5], item['answer_path'], item['answer_line'])
                keywords = keyword_tokens(question)
                rg_argv = [self.args.rg, '-i', '-F']
                for word in keywords:
                    rg_argv += ['-e', word]
                rg_argv += ['--count-matches', '.']
                rg_proc = self.run(rg_argv, cwd=self.corpus, timeout=120, tag=f'demo-rg-{qid}')
                if rg_proc['timed_out'] or rg_proc['exit'] not in (0, 1):
                    raise BenchError(f'demo-rg-{qid}: exit={rg_proc["exit"]} '
                                     f'timeout={rg_proc["timed_out"]}: '
                                     f'{rg_proc["stderr"][:200]!r}')
                (demo_raw / f'{qid}-rg.txt').write_bytes(rg_proc['stdout'])
                (demo_raw / f'{qid}-rg.argv.json').write_text(
                    json.dumps(rg_argv, indent=2) + '\n', encoding='utf-8')
                match_files = {}
                for line in rg_proc['stdout'].decode('utf-8', errors='replace').splitlines():
                    if not line:
                        continue
                    path, _, count = line.rpartition(':')
                    if not path:
                        path, count = line, '0'
                    try:
                        match_files[path.replace('\\', '/').lstrip('./')] = int(count)
                    except ValueError:
                        continue
                answer_file = item['answer_path'].replace('\\', '/').lstrip('./')
                record = {
                    'id': qid, 'question': question,
                    'answer_path': item['answer_path'], 'answer_line': item['answer_line'],
                    'wg_top1_path_contains_answer': top1_path,
                    'wg_top5_path_contains_answer': top5_path,
                    'wg_top1_line_contains_answer': top1_line,
                    'wg_top5_line_contains_answer': top5_line,
                    'wg_top1_path': rows[0].get('path') if rows else None,
                    'wg_top1_start': rows[0].get('start') if rows else None,
                    'wg_top1_end': rows[0].get('end') if rows else None,
                    'rg_keywords': keywords,
                    'rg_exit': rg_proc['exit'],
                    'rg_total_matches': sum(match_files.values()),
                    'rg_files_with_matches': len(match_files),
                    'rg_answer_file_has_match': answer_file in match_files,
                    'rg_answer_file_match_count': match_files.get(answer_file, 0),
                }
                results.append(record)
                self.append_raw('demo', record)
                print(f"demo {qid}: wg path top1={top1_path} top5={top5_path} "
                      f"line top1={top1_line} top5={top5_line} "
                      f"rg matches={record['rg_total_matches']} "
                      f"answer_file={record['rg_answer_file_has_match']}", flush=True)
        finally:
            self.flags['demo_cleanup'] = self.stop_service(service_dir, 'demo')
        self.flags['demo_results'] = results

    # -- phase: freshness ----------------------------------------------
    def phase_freshness(self):
        probe = self.fresh_corpus / 'wg_launch_bench_probe.rs'
        probe.write_text('fn launch_probe_seed() { let value = 1; }\n', encoding='utf-8')
        service_dir, status = self.start_service(self.fresh_corpus, 'freshness')
        pid = int(status['pid'])
        records = []
        try:
            self.sgrep_prime(self.fresh_corpus)

            def write_token(token):
                probe.write_text(
                    f'fn {token}() {{ let value = 1; }}\n', encoding='utf-8')

            # Warmups are excluded from every distribution.
            for i in range(self.args.fresh_warmup):
                token = f'launch_probe_warm_{i:04d}'
                write_token(token)
                self.run(self.wg_semantic(token, self.fresh_corpus,
                                          service_dir=service_dir)[0],
                         timeout=60, tag='fresh-warmup-wg', service_dir=service_dir)
                self.run(self.sgrep_semantic(token, self.fresh_corpus),
                         timeout=self.args.sgrep_build_timeout, tag='fresh-warmup-sgrep')

            for i in range(self.args.fresh_wg_samples):
                token = f'launch_probe_wg_{i:04d}'
                write_token(token)
                start = time.perf_counter()
                attempts = 0
                measured = None
                errors = []
                while time.perf_counter() - start < self.args.fresh_timeout:
                    attempts += 1
                    proc = self.run(self.wg_semantic(token, self.fresh_corpus,
                                                     service_dir=service_dir)[0],
                                    timeout=self.args.fresh_timeout, tag='fresh-wg',
                                    service_dir=service_dir)
                    if proc['timed_out']:
                        errors.append('timeout')
                        break
                    if proc['exit'] != 0:
                        errors.append(f'exit {proc["exit"]}')
                        continue
                    try:
                        rows = parse_json_bytes(proc['stdout'], 'fresh-wg')
                    except BenchError as exc:
                        errors.append(str(exc))
                        continue
                    if rows_have_token(rows, token):
                        measured = (time.perf_counter() - start) * 1000.0
                        break
                    time.sleep(0.01)
                record = {'sample': i, 'tool': 'wg', 'token': token,
                          'ms': measured, 'attempts': attempts, 'errors': errors,
                          'error': None if measured is not None else 'not visible before timeout'}
                records.append(record)
                self.append_raw('freshness', record)

            for i in range(self.args.fresh_sgrep_samples):
                token = f'launch_probe_sg_{i:04d}'
                write_token(token)
                start = time.perf_counter()
                proc = self.run(self.sgrep_semantic(token, self.fresh_corpus),
                                timeout=self.args.sgrep_build_timeout, tag='fresh-sgrep')
                method = 'auto-rebuild'
                measured = None
                errors = []
                rows = []
                if proc['timed_out']:
                    errors.append('timeout')
                elif proc['exit'] != 0:
                    errors.append(f'exit {proc["exit"]}')
                else:
                    try:
                        rows = parse_json_bytes(proc['stdout'], 'fresh-sgrep')
                    except BenchError as exc:
                        errors.append(str(exc))
                    if rows_have_token(rows, token):
                        measured = (time.perf_counter() - start) * 1000.0
                if measured is None:
                    # Fallback: the cached query did not pick up the edit, so
                    # force a refresh and count the whole wait.
                    method = 'refresh-cache-fallback'
                    refresh = self.run(
                        [self.args.sgrep, token, str(self.fresh_corpus), '--model', MODEL,
                         '--json', '--explain', '--cache-dir', str(self.sgrep_cache),
                         '--refresh-cache', '-n', str(self.args.top)],
                        timeout=self.args.sgrep_build_timeout, tag='fresh-sgrep-refresh')
                    if refresh['timed_out']:
                        errors.append('refresh timeout')
                    elif refresh['exit'] != 0:
                        errors.append(f'refresh exit {refresh["exit"]}')
                    else:
                        try:
                            rows = parse_json_bytes(refresh['stdout'], 'fresh-sgrep-refresh')
                            if rows_have_token(rows, token):
                                measured = (time.perf_counter() - start) * 1000.0
                        except BenchError as exc:
                            errors.append(str(exc))
                record = {'sample': i, 'tool': 'sgrep', 'token': token, 'method': method,
                          'ms': measured, 'errors': errors,
                          'error': None if measured is not None else 'not visible before timeout'}
                records.append(record)
                self.append_raw('freshness', record)
                print(f'freshness: sgrep sample {i + 1}/'
                      f'{self.args.fresh_sgrep_samples} '
                      f"{'ok' if measured else 'FAILED'} "
                      f"{'' if measured is None else f'{measured:.0f} ms'}", flush=True)
        finally:
            self.flags['freshness_cleanup'] = self.stop_service(service_dir, 'freshness')
        self.flags['freshness_records'] = records
        self.flags['freshness_pid'] = pid

    # -- results ---------------------------------------------------------
    def build_results(self):
        flat = {}
        flat['campaign_host'] = os.uname().nodename
        flat['campaign_cpu_count'] = os.cpu_count()
        flat['campaign_date_utc'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
        flat['campaign_corpus'] = str(self.args.corpus_source)
        flat['campaign_corpus_files'] = self.flags.get('corpus_files')
        flat['campaign_wg_commit'] = self.args.commit or None
        flat['campaign_source_archive_sha256'] = self.args.source_sha256 or None
        flat['campaign_linux_corpus'] = str(self.args.linux_corpus or '') or None
        flat['campaign_wg_files'] = self.flags.get('wg_files')
        flat['campaign_wg_chunks'] = self.flags.get('wg_chunks')
        flat['campaign_single_campaign_note'] = (
            'one final campaign on a quiet shared 4 vCPU VPS; the setup phase '
            'runs first, then warm, fan-out, multi-repo, text and freshness')
        flat['sgrep_cache_max_bytes'] = int(self.base_env['SGREP_CACHE_MAX_BYTES'])
        flat['wg_path'] = str(self.args.wg)
        flat['wg_sha256'] = sha256_file(self.args.wg)
        flat['sgrep_path'] = str(self.args.sgrep)
        flat['sgrep_sha256'] = sha256_file(self.args.sgrep)
        flat['rg_path'] = str(self.args.rg)
        flat['rg_sha256'] = sha256_file(self.args.rg)
        flat['rg_version'] = RG_VERSION
        flat['model_choice'] = MODEL
        flat['top_k_default'] = self.args.top
        for sid, settle in sorted(self.flags.get('settles', {}).items()):
            flat[f'settle_{sid}_seconds'] = settle['seconds']
            flat[f'settle_{sid}_text_indexed'] = settle['indexed']
            flat[f'settle_{sid}_text_unindexed'] = settle['unindexed']

        # Freshness
        records = self.flags.get('freshness_records', [])
        for tool in ('wg', 'sgrep'):
            ok = [r['ms'] for r in records if r['tool'] == tool and r['ms'] is not None]
            dist = distribution(ok)
            prefix = f'freshness_{tool}'
            flat[f'{prefix}_n'] = dist['n']
            flat[f'{prefix}_p50_ms'] = dist['p50_ms']
            flat[f'{prefix}_p95_ms'] = dist['p95_ms']
            flat[f'{prefix}_min_ms'] = dist['min_ms']
            flat[f'{prefix}_max_ms'] = dist['max_ms']
            flat[f'{prefix}_errors'] = sum(
                1 for r in records if r['tool'] == tool and r['ms'] is None)
            flat[f'{prefix}_requested_n'] = (
                self.args.fresh_wg_samples if tool == 'wg' else self.args.fresh_sgrep_samples)
        wg_fresh = flat.get('freshness_wg_p50_ms')
        sg_fresh = flat.get('freshness_sgrep_p50_ms')
        flat['freshness_speedup_sgrep_over_wg_p50_x'] = (
            sg_fresh / wg_fresh if wg_fresh and sg_fresh else None)
        auto = sum(1 for r in records if r['tool'] == 'sgrep' and r.get('method') == 'auto-rebuild')
        flat['freshness_sgrep_auto_rebuild_samples'] = auto

        # Warm semantic
        records = self.flags.get('warm_records', [])
        for tool in ('wg', 'sgrep'):
            ok = [r['ms'] for r in records if r['tool'] == tool and r['ms'] is not None]
            dist = distribution(ok)
            prefix = f'warm_semantic_{tool}'
            flat[f'{prefix}_n'] = dist['n']
            flat[f'{prefix}_p50_ms'] = dist['p50_ms']
            flat[f'{prefix}_p95_ms'] = dist['p95_ms']
            flat[f'{prefix}_min_ms'] = dist['min_ms']
            flat[f'{prefix}_max_ms'] = dist['max_ms']
            flat[f'{prefix}_errors'] = sum(
                1 for r in records if r['tool'] == tool and r['ms'] is None)
        wg_warm = flat.get('warm_semantic_wg_p50_ms')
        sg_warm = flat.get('warm_semantic_sgrep_p50_ms')
        flat['warm_semantic_ratio_wg_over_sgrep_p50_x'] = (
            wg_warm / sg_warm if wg_warm and sg_warm else None)
        parity = self.flags.get('warm_parity', [])
        flat['warm_topk_queries_total'] = len(parity)
        flat['warm_topk_queries_identical'] = sum(1 for p in parity if not p['issues'])
        flat['warm_topk_scores_exact_queries'] = sum(1 for p in parity if p['scores_exact'])
        flat['warm_semantic_queries'] = '; '.join(SEMANTIC_QUERIES)

        # Fan-out
        for level in self.flags.get('fanout_levels', []):
            clients = level['wg']['clients']
            for tool in ('wg', 'sgrep'):
                summary = level[tool]
                prefix = f'fanout_{clients}_{tool}'
                flat[f'{prefix}_throughput_qps'] = summary['throughput_qps']
                flat[f'{prefix}_p50_ms'] = summary['p50_ms']
                flat[f'{prefix}_p95_ms'] = summary['p95_ms']
                flat[f'{prefix}_requests'] = summary['requests']
                flat[f'{prefix}_successes'] = summary['successes']
                flat[f'{prefix}_errors'] = summary['errors']
                flat[f'{prefix}_timeouts'] = summary['timeouts']
                flat[f'{prefix}_rss_peak_mib'] = summary['rss_peak_mib']

        # Multi-repo
        wg_levels = {row['count']: row for row in self.flags.get('multirepo_levels', [])}
        sg_levels = {row['count']: row for row in self.flags.get('multirepo_sgrep_levels', [])}
        for count in self.args.repo_counts:
            if count in wg_levels:
                flat[f'multirepo_{count}_wg_rss_idle_mib'] = wg_levels[count]['wg_rss_idle_mib']
                flat[f'multirepo_{count}_wg_rss_query_peak_mib'] = wg_levels[count]['wg_rss_query_peak_mib']
                flat[f'multirepo_{count}_wg_roots'] = wg_levels[count]['wg_roots']
            if count in sg_levels:
                flat[f'multirepo_{count}_sgrep_rss_sum_peak_mib'] = sg_levels[count]['sgrep_rss_sum_peak_mib']
                flat[f'multirepo_{count}_sgrep_rss_process_peak_sum_mib'] = sg_levels[count]['sgrep_rss_process_peak_sum_mib']
                flat[f'multirepo_{count}_sgrep_errors'] = sg_levels[count].get('sgrep_errors', 0)

        # Setup
        for row in self.flags.get('setup_records', []):
            name = row['corpus']
            prefix = f'setup_{name}'
            flat[f'{prefix}_up_ms'] = row.get('up_ms')
            flat[f'{prefix}_peak_rss_mib'] = row.get('peak_rss_mib')
            flat[f'{prefix}_steady_rss_mib'] = row.get('ready_rss_mib')
            flat[f'{prefix}_files'] = row.get('files')
            flat[f'{prefix}_chunks'] = row.get('chunks')
            flat[f'{prefix}_text_files'] = row.get('text_files')
            flat[f'{prefix}_text_build_ms'] = row.get('text_build_ms')
            flat[f'{prefix}_text_disk_bytes'] = row.get('text_disk_bytes')
            flat[f'{prefix}_text_error'] = row.get('text_error')
            flat[f'{prefix}_ready'] = row.get('ready')
            flat[f'{prefix}_root'] = row.get('root')
        skipped = self.flags.get('setup_linux_skipped')
        if skipped:
            flat['setup_linux_skipped'] = skipped

        # Text route, one query at a time
        records = self.flags.get('text_records', [])
        for spec in TEXT_QUERY_SPECS:
            name = spec['name']
            for tool in ('wg', 'rg'):
                ok = [r['ms'] for r in records
                      if r['pattern'] == name and r['tool'] == tool and r['ms'] is not None]
                dist = distribution(ok)
                prefix = f'text_{name}_{tool}'
                flat[f'{prefix}_n'] = dist['n']
                flat[f'{prefix}_p50_ms'] = dist['p50_ms']
                flat[f'{prefix}_p95_ms'] = dist['p95_ms']
                flat[f'{prefix}_errors'] = sum(
                    1 for r in records
                    if r['pattern'] == name and r['tool'] == tool and r['ms'] is None)
            wg_p50 = flat.get(f'text_{name}_wg_p50_ms')
            rg_p50 = flat.get(f'text_{name}_rg_p50_ms')
            flat[f'text_{name}_ratio_wg_over_rg_p50_x'] = (
                wg_p50 / rg_p50 if wg_p50 and rg_p50 else None)
        for row in self.flags.get('text_parity', []):
            name = row['pattern']
            flat[f'text_{name}_text_pattern'] = row['text_pattern']
            flat[f'text_{name}_stdout_exact_identical'] = bool(row['exact'])
            flat[f'text_{name}_wg_bytes'] = row['wg_bytes']
            flat[f'text_{name}_rg_bytes'] = row['rg_bytes']
            flat[f'text_{name}_wg_lines'] = row['wg_lines']
            flat[f'text_{name}_rg_lines'] = row['rg_lines']
            flat[f'text_{name}_index_used'] = row.get('used')
            flat[f'text_{name}_index_reason'] = row.get('reason')
            flat[f'text_{name}_event_count'] = row.get('event_count')
        flat['text_daemon_rss_mib'] = self.flags.get('text_daemon_rss_mib')

        # Text route, concurrent clients
        for row in self.flags.get('text_concurrency', []):
            prefix = f"text_fanout_{row['clients']}_{row['tool']}"
            flat[f'{prefix}_throughput_qps'] = row['throughput_qps']
            flat[f'{prefix}_p50_ms'] = row['p50_ms']
            flat[f'{prefix}_p95_ms'] = row['p95_ms']
            flat[f'{prefix}_requests'] = row['requests']
            flat[f'{prefix}_successes'] = row['successes']
            flat[f'{prefix}_errors'] = row['errors']

        # Demo (optional phase; omitted when it did not run)
        demo = self.flags.get('demo_results', [])
        if demo:
            flat['demo_questions_total'] = len(demo)
            flat['demo_wg_top1_path_hits'] = sum(
                1 for d in demo if d['wg_top1_path_contains_answer'])
            flat['demo_wg_top5_path_hits'] = sum(
                1 for d in demo if d['wg_top5_path_contains_answer'])
            flat['demo_wg_top1_line_hits'] = sum(
                1 for d in demo if d['wg_top1_line_contains_answer'])
            flat['demo_wg_top5_line_hits'] = sum(
                1 for d in demo if d['wg_top5_line_contains_answer'])
            flat['demo_rg_answer_file_hits'] = sum(
                1 for d in demo if d['rg_answer_file_has_match'])
            flat['demo_rg_total_matches'] = sum(d['rg_total_matches'] for d in demo)
            for d in demo:
                prefix = f"demo_{d['id']}"
                flat[f'{prefix}_wg_top1_path_contains_answer'] = bool(
                    d['wg_top1_path_contains_answer'])
                flat[f'{prefix}_wg_top5_path_contains_answer'] = bool(
                    d['wg_top5_path_contains_answer'])
                flat[f'{prefix}_wg_top1_line_contains_answer'] = bool(
                    d['wg_top1_line_contains_answer'])
                flat[f'{prefix}_wg_top5_line_contains_answer'] = bool(
                    d['wg_top5_line_contains_answer'])
                flat[f'{prefix}_rg_total_matches'] = d['rg_total_matches']
                flat[f'{prefix}_rg_files_with_matches'] = d['rg_files_with_matches']
                flat[f'{prefix}_rg_answer_file_has_match'] = bool(
                    d['rg_answer_file_has_match'])

        # Cleanup outcomes
        for key, value in self.flags.items():
            if key.endswith('_cleanup'):
                flat[f'{key}_ok'] = value.get('down_exit') == 0
        return flat


def build_parser():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--wg', default=DEFAULT_WG)
    ap.add_argument('--sgrep', default=DEFAULT_SGREP)
    ap.add_argument('--rg', default=DEFAULT_RG)
    ap.add_argument('--corpus-source', default=DEFAULT_CORPUS)
    ap.add_argument('--linux-corpus', default=DEFAULT_LINUX,
                    help='Linux kernel tree for the setup phase; empty to disable')
    ap.add_argument('--skip-linux-setup', action='store_true')
    ap.add_argument('--commit', default=None, help='wg source commit for the record')
    ap.add_argument('--source-sha256', default=None,
                    help='sha256 of the git archive that produced the binary')
    ap.add_argument('--hf-hub', default=DEFAULT_HF_HUB)
    ap.add_argument('--ts-pack', default=DEFAULT_TS_PACK)
    ap.add_argument('--work')
    ap.add_argument('--out')
    ap.add_argument('--top', type=int, default=10)
    ap.add_argument('--semantic-samples', type=int, default=120,
                    help='total interleaved warm semantic samples per tool')
    ap.add_argument('--warmup', type=int, default=3)
    ap.add_argument('--up-timeout', type=float, default=900)
    ap.add_argument('--large-up-timeout', type=float, default=2400)
    ap.add_argument('--sgrep-build-timeout', type=float, default=300)
    ap.add_argument('--request-timeout', type=float, default=60)
    ap.add_argument('--fresh-wg-samples', type=int, default=30)
    ap.add_argument('--fresh-sgrep-samples', type=int, default=6)
    ap.add_argument('--fresh-warmup', type=int, default=1)
    ap.add_argument('--fresh-timeout', type=float, default=30)
    ap.add_argument('--fanout-clients', type=int, nargs='+', default=[1, 4, 16])
    ap.add_argument('--fanout-seconds', type=float, default=10.0)
    ap.add_argument('--repo-counts', type=int, nargs='+', default=[1, 3, 5])
    ap.add_argument('--text-samples', type=int, default=30)
    ap.add_argument('--text-clients', type=int, nargs='+', default=[1, 4, 16, 32])
    ap.add_argument('--text-per-client', type=int, default=4)
    ap.add_argument('--phase', action='append', default=None,
                    choices=['setup', 'warm', 'fanout', 'multirepo', 'text', 'demo',
                             'freshness'],
                    help='run only these phases (repeatable); setup always runs')
    ap.add_argument('--self-test', action='store_true')
    return ap


def preflight(args):
    for name in ('wg', 'sgrep', 'rg'):
        path = Path(getattr(args, name))
        if not path.is_file():
            raise BenchError(f'{name} binary missing: {path}')
    if sha256_file(args.sgrep) != CONTROL_SHA256:
        raise BenchError('pinned sgrep control SHA-256 mismatch')
    rg_version = subprocess.run([args.rg, '--version'], stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=30).stdout.decode(errors='replace').splitlines()
    if not rg_version or rg_version[0].split()[:2] != ['ripgrep', RG_VERSION]:
        raise BenchError(f'rg version mismatch: {rg_version[:1]}')
    wg_help = subprocess.run([args.wg, '--help'], stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             timeout=30).stdout.decode(errors='replace')
    if '--top' not in wg_help:
        raise BenchError('wg binary does not advertise --top; wrong build')
    if not Path(args.corpus_source).is_dir():
        raise BenchError(f'corpus missing: {args.corpus_source}')


def self_test():
    if percentile([1, 2, 3, 4], 50) != 2.5:
        raise BenchError('percentile p50 interpolation failed')
    dist = distribution([1, 2, 3, 4])
    if dist['n'] != 4 or dist['p50_ms'] != 2.5:
        raise BenchError('distribution failed')
    rows = [{'path': 'a/b.rs', 'start': 10, 'end': 12, 'content': 'x'}]
    if not rows_have_answer(rows, 'a/b.rs', 11):
        raise BenchError('answer row detection failed')
    if rows_have_answer(rows, 'a/b.rs', 13):
        raise BenchError('answer row line range failed')
    if not rows_have_token(rows, 'x'):
        raise BenchError('token detection failed')
    if keyword_tokens('How does the parser handle unary expressions?')[:2] != ['parser', 'handle']:
        raise BenchError('keyword tokenizer failed')
    print('self-test: PASS')
    return 0


def main(argv=None):
    args = build_parser().parse_args(argv)
    if args.self_test:
        return self_test()
    if not args.work or not args.out:
        print('error: --work and --out are required', file=sys.stderr)
        return 2
    try:
        preflight(args)
    except (BenchError, subprocess.SubprocessError) as exc:
        print(f'preflight failed: {exc}', file=sys.stderr)
        return 2
    work = Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    bench = Bench(args)
    selected = args.phase or ['setup', 'warm', 'fanout', 'multirepo', 'text',
                              'freshness']
    if 'setup' not in selected:
        selected = ['prepare'] + selected
    ran = []
    failed = []
    try:
        for phase in selected:
            if phase == 'prepare':
                bench.setup()
                ran.append(phase)
                continue
            if phase == 'setup':
                bench.setup()
                print('phase: setup', flush=True)
                try:
                    bench.phase_setup()
                    ran.append(phase)
                except BenchError as exc:
                    failed.append({'phase': phase, 'error': str(exc)})
                    bench.flags[f'phase_{phase}_error'] = str(exc)
                    print(f'phase {phase} FAILED: {exc}', file=sys.stderr, flush=True)
                except Exception as exc:  # noqa: BLE001 - keep the campaign alive
                    failed.append({'phase': phase, 'error': repr(exc)})
                    bench.flags[f'phase_{phase}_error'] = repr(exc)
                    print(f'phase {phase} CRASHED: {exc!r}', file=sys.stderr, flush=True)
                continue
            print(f'phase: {phase}', flush=True)
            try:
                getattr(bench, f'phase_{phase}')()
                ran.append(phase)
            except BenchError as exc:
                failed.append({'phase': phase, 'error': str(exc)})
                bench.flags[f'phase_{phase}_error'] = str(exc)
                print(f'phase {phase} FAILED: {exc}', file=sys.stderr, flush=True)
            except Exception as exc:  # noqa: BLE001 - keep the campaign alive
                failed.append({'phase': phase, 'error': repr(exc)})
                bench.flags[f'phase_{phase}_error'] = repr(exc)
                print(f'phase {phase} CRASHED: {exc!r}', file=sys.stderr, flush=True)
    finally:
        cleanup = bench.stop_all()
    results = bench.build_results()
    results['campaign_phases_ran'] = ','.join(ran)
    results['campaign_phases_failed'] = ','.join(f['phase'] for f in failed) or 'none'
    results['campaign_phase_errors'] = '; '.join(
        f"{f['phase']}: {f['error']}" for f in failed) or 'none'
    cleanup_flags = [v for k, v in bench.flags.items() if k.endswith('_cleanup')]
    results['cleanup_all_ok'] = all(c.get('down_exit') == 0 for c in cleanup_flags)
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(results, indent=2, ensure_ascii=False) + '\n',
                   encoding='utf-8')
    print(json.dumps({'result': 'OK' if not failed else 'PARTIAL',
                      'phases_ran': ran, 'phases_failed': failed,
                      'out': str(out)}, indent=2))
    return 0 if not failed else 1


if __name__ == '__main__':
    raise SystemExit(main())
