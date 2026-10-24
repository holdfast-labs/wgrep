#!/usr/bin/env python3
"""wg release validation harness: unified front-door contract, semantic
mutation parity against a pinned fresh sgrep, text/rg parity through the same
daemon, and bounded warm/visibility timing.

Scope and honesty boundaries
----------------------------
This harness validates the frozen release contract recorded in
``docs/release-protocol.md``:

* ``wg QUERY [ROOT] --json --explain --model code`` is the sgrep-compatible
  hybrid semantic search path.
* Semantic result count is ``--top N`` / ``-k N``. ``-n`` is reserved for
  rg line numbers.
* rg-shaped requests (``-F -e -n -l -g --files ...``) select precise
  filesystem text/regex behavior and must preserve the authoritative rg
  argv, cwd, stdout bytes, stderr bytes and exit status.
* ``wg up ROOT --model code``, ``wg status --json`` and ``wg down`` are the
  lifecycle surface, isolated by ``WG_SERVICE_DIR``.

Every comparison is exact: ordered semantic JSON with exact finite numeric
scores (no float tolerance) and byte-exact text output. A single-side timeout
is a mismatch; a timeout is never discarded. ``--self-test`` exercises all
comparison and statistics logic without a service binary; ``--run`` is only
meaningful once the unified ``wg`` CLI exists.

Latency is measured from a completed (settled) filesystem operation to the
first correct CLI response, BEFORE the slow fresh-sgrep baseline is run.
Warm semantic and warm text workloads are reported separately; no cross-kind
"speedup" ratio is invented.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import shutil
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
QUERIES_PATH = HERE / 'WGREP_WG_RELEASE_QUERIES.json'
DEFAULT_SGREP = '/srv/projects/wispd/sgrep-control-0fbe8e5/target/release/sgrep'
DEFAULT_RG = '/srv/workspaces/wispd/tools/rg-15/rg'
CONTROL_SHA256 = '715946fa0f17006905b5f0d0bbf1f21c00e355882855ec40c44c14e78a3c6217'
CONTROL_REVISION = '0fbe8e5725f71667789be4a3abcea4e02f33afa4'
RG_VERSION = '15.1.0'
SIXLANG_SCHEMA = 'wgrep-sgrep-small-parity-v1'
SIXLANG_LANGUAGES = {'cpp', 'go', 'java', 'python', 'rust', 'typescript'}
CODE_MODEL = {'repo': 'minishlab/potion-code-16M-v2',
              'revision': 'e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b'}
PHASES = ('lifecycle', 'isolation', 'semantic', 'text', 'bench')


class HarnessError(RuntimeError):
    """A structural failure that stops the run."""


class Proc:
    __slots__ = ('argv', 'cwd', 'exit', 'timed_out', 'elapsed_ms',
                 'stdout_path', 'stderr_path', 'stdout', 'stderr')

    def __init__(self, argv, cwd, exit_code, timed_out, elapsed_ms,
                 stdout_path, stderr_path, stdout, stderr):
        self.argv = argv
        self.cwd = cwd
        self.exit = exit_code
        self.timed_out = timed_out
        self.elapsed_ms = elapsed_ms
        self.stdout_path = str(stdout_path)
        self.stderr_path = str(stderr_path)
        self.stdout = stdout
        self.stderr = stderr


# --------------------------------------------------------------------------
# Pure helpers (unit-tested by --self-test)
# --------------------------------------------------------------------------

def sha256_file(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def _reject_constant(token):
    raise ValueError(f'non-finite JSON constant {token!r} is not allowed')


def _assert_finite(obj, where):
    if isinstance(obj, bool):
        return
    if isinstance(obj, float):
        if not math.isfinite(obj):
            raise ValueError(f'{where}: non-finite number {obj!r}')
        return
    if isinstance(obj, dict):
        for key, value in obj.items():
            _assert_finite(value, f'{where}.{key}')
        return
    if isinstance(obj, list):
        for i, value in enumerate(obj):
            _assert_finite(value, f'{where}[{i}]')


def load_json_strict(data, where):
    """Parse UTF-8 JSON, rejecting NaN/Infinity and any non-finite number."""
    if isinstance(data, (bytes, bytearray)):
        data = bytes(data).decode('utf-8')
    try:
        obj = json.loads(data, parse_constant=_reject_constant)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f'{where}: invalid JSON: {exc}') from exc
    _assert_finite(obj, where)
    return obj


def norm_path(path):
    return str(path).replace('\\', '/')


def validate_rows(rows, where):
    """Validate the explained semantic result shape. Returns nothing on success."""
    if not isinstance(rows, list):
        raise ValueError(f'{where}: result JSON must be an array')
    for i, row in enumerate(rows):
        if not isinstance(row, dict):
            raise ValueError(f'{where}: result {i} is not an object')
        if not isinstance(row.get('path'), str):
            raise ValueError(f'{where}: result {i} has no string path')
        if type(row.get('start')) is not int or type(row.get('end')) is not int:
            raise ValueError(f'{where}: result {i} has no integer line range')
        if not 1 <= row['start'] <= row['end']:
            raise ValueError(f'{where}: result {i} has invalid line range')
        if not isinstance(row.get('content'), str):
            raise ValueError(f'{where}: result {i} has no string content')
        scores = row.get('scores')
        if not isinstance(scores, dict) or not scores:
            raise ValueError(f'{where}: result {i} has no explained scores object')
        for key, value in scores.items():
            if not isinstance(key, str):
                raise ValueError(f'{where}: result {i} has a non-string score name')
            if isinstance(value, bool) or value is None or isinstance(value, str):
                continue
            if isinstance(value, (int, float)):
                if not math.isfinite(float(value)):
                    raise ValueError(f'{where}: result {i}.scores.{key} is non-finite')
                continue
            raise ValueError(f'{where}: result {i}.scores.{key} has unsupported type')


def compare_scores(a, b, where):
    issues = []
    if set(a) != set(b):
        return [f'{where}: score field mismatch: control={sorted(a)} candidate={sorted(b)}']
    for key in sorted(a):
        x, y = a[key], b[key]
        if isinstance(x, bool) or isinstance(y, bool) or x is None or y is None \
                or isinstance(x, str) or isinstance(y, str):
            if type(x) is not type(y) or x != y:
                issues.append(f'{where}.{key}: {x!r} != {y!r}')
        elif isinstance(x, (int, float)) and isinstance(y, (int, float)):
            if not math.isfinite(float(x)) or not math.isfinite(float(y)):
                issues.append(f'{where}.{key}: non-finite score')
            elif x != y:
                issues.append(f'{where}.{key}: exact numeric mismatch {x!r} != {y!r}')
        else:
            issues.append(f'{where}.{key}: unsupported score type')
    return issues


def compare_semantic(control, candidate, control_rc, candidate_rc, where):
    """Exact, ordered comparison of explained semantic rows. Returns issues."""
    issues = []
    if control_rc != candidate_rc:
        issues.append(f'{where}: exit code control={control_rc} candidate={candidate_rc}')
    if len(control) != len(candidate):
        issues.append(f'{where}: result count control={len(control)} candidate={len(candidate)}')
    for i, (a, b) in enumerate(zip(control, candidate)):
        if set(a) != set(b):
            issues.append(f'{where}: row {i} field set differs')
        for key in ('path', 'start', 'end', 'content'):
            if a[key] != b[key]:
                issues.append(f'{where}: row {i} {key} differs')
        issues.extend(compare_scores(a['scores'], b['scores'], f'{where}: row {i}'))
    return issues


def compare_text(candidate, control, where):
    """Byte-exact stdout/stderr plus exact exit status for an rg-shaped call."""
    issues = []
    if candidate.timed_out or control.timed_out:
        issues.append(f'{where}: timeout candidate={candidate.timed_out} control={control.timed_out}')
        return issues
    if candidate.exit != control.exit:
        issues.append(f'{where}: exit status {candidate.exit} != {control.exit}')
    json_mode = '--json' in getattr(candidate, 'argv', [])
    def stable_json_lines(data):
        rows = [load_json_strict(line, where) for line in data.splitlines() if line]
        for row in rows:
            if row.get('type') == 'summary':
                info = row.get('data', {})
                info.pop('elapsed_total', None)
                if isinstance(info.get('stats'), dict):
                    info['stats'].pop('elapsed', None)
            elif row.get('type') == 'end':
                stats = row.get('data', {}).get('stats')
                if isinstance(stats, dict):
                    stats.pop('elapsed', None)
        return rows
    try:
        same_stdout = (stable_json_lines(candidate.stdout) == stable_json_lines(control.stdout)
                       if json_mode else candidate.stdout == control.stdout)
    except (ValueError, AttributeError):
        same_stdout = False
    if not same_stdout:
        issues.append(f'{where}: stdout bytes differ '
                      f'(candidate {len(candidate.stdout)}B / control {len(control.stdout)}B)')
    if candidate.stderr != control.stderr:
        issues.append(f'{where}: stderr bytes differ '
                      f'(candidate {len(candidate.stderr)}B / control {len(control.stderr)}B)')
    return issues


def predicate_holds(rows, expect):
    """Evaluate a declarative expectation against semantic rows."""
    if expect.get('empty'):
        return not rows
    ok = True
    if 'contains' in expect:
        ok = ok and any(expect['contains'] in r['content'] for r in rows)
    if 'path' in expect:
        ok = ok and any(norm_path(r['path']) == norm_path(expect['path']) for r in rows)
    if 'forbid_substring' in expect:
        ok = ok and not any(expect['forbid_substring'] in r['content'] for r in rows)
    if 'forbid_path' in expect:
        ok = ok and not any(norm_path(r['path']) == norm_path(expect['forbid_path']) for r in rows)
    return ok


def percentile(values, p):
    if not values:
        return None
    xs = sorted(float(v) for v in values)
    if len(xs) == 1:
        return xs[0]
    k = (len(xs) - 1) * (p / 100.0)
    lo = int(math.floor(k))
    hi = int(math.ceil(k))
    if lo == hi:
        return xs[lo]
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def distribution(values):
    values = [float(v) for v in values]
    if not values:
        return {'count': 0, 'min': None, 'max': None, 'mean': None,
                'p50': None, 'p95': None, 'p99': None}
    return {'count': len(values), 'min': min(values), 'max': max(values),
            'mean': sum(values) / len(values), 'p50': percentile(values, 50),
            'p95': percentile(values, 95), 'p99': percentile(values, 99)}


# --------------------------------------------------------------------------
# Query-set loading / validation
# --------------------------------------------------------------------------

def _require(ok, message):
    if not ok:
        raise ValueError(message)


def load_queries(path):
    data = load_json_strict(Path(path).read_bytes(), str(path))
    _require(data.get('schema') == 'wgrep-wg-release-v1', 'unknown query-set schema')
    tokens = data.get('tokens')
    _require(isinstance(tokens, dict) and tokens, 'query set needs a tokens object')
    for key in ('alpha_old', 'alpha_new', 'restored_old', 'restored_new', 'added',
                'ignored', 'ignored_keep', 'beta', 'background_prefix', 'bench_a', 'bench_b'):
        _require(isinstance(tokens.get(key), str) and tokens[key], f'missing token {key}')
    _require(len(tokens['alpha_old']) == len(tokens['alpha_new']),
             'alpha_old and alpha_new must be the same length for the same-length edit')
    _require(len(tokens['restored_old']) == len(tokens['restored_new']),
             'restored_old and restored_new must be the same length for the restored-mtime rewrite')
    _require(len(tokens['bench_a']) == len(tokens['bench_b']),
             'bench_a and bench_b must be the same length for the visibility toggle')

    known = set(MUTATION_NAMES)
    for step in data.get('mutations', []):
        _require(step.get('id'), 'mutation step needs an id')
        _require(step.get('mutation') is None or step['mutation'] in known,
                 f"unknown mutation {step.get('mutation')!r}")
        _require(isinstance(step.get('query'), str) and step['query'], 'mutation step needs a query')
        _require(isinstance(step.get('expect'), dict) and step['expect'], 'mutation step needs an expect')
    _require(len(data.get('mutations', [])) >= 6, 'need at least six mutation steps')
    seen_ids = set()
    for cell in data.get('text_cells', []):
        _require(cell.get('id') and cell['id'] not in seen_ids, f'duplicate text cell {cell.get("id")!r}')
        seen_ids.add(cell['id'])
        _require(isinstance(cell.get('args'), list) and cell['args'], f'text cell {cell["id"]} needs argv')
        _require(all(isinstance(a, str) for a in cell['args']), f'text cell {cell["id"]} argv must be strings')
    _require(len(data.get('text_cells', [])) >= 12, 'need at least twelve text cells')
    _require(any(c.get('expect_error') for c in data['text_cells']), 'text matrix needs an error cell')
    _require(isinstance(data.get('warm_semantic'), list) and data['warm_semantic'],
             'warm_semantic query set is required')
    _require(all(isinstance(q, str) and q for q in data['warm_semantic']), 'warm_semantic entries must be strings')
    _require(isinstance(data.get('warm_text'), list) and data['warm_text'], 'warm_text query set is required')
    for cell in data['warm_text']:
        _require(cell.get('id') and isinstance(cell.get('args'), list) and cell['args'],
                 'warm_text cell needs id and argv')
    vq = data.get('visibility_query')
    _require(isinstance(vq, dict), 'visibility_query is required')
    _require(vq.get('query') == tokens['bench_a'] and vq.get('alternate') == tokens['bench_b'],
             'visibility_query must match the bench_a/bench_b tokens')
    return data


def load_sixlang_fixture(root):
    root = Path(root)
    data = load_json_strict((root / 'queries.json').read_bytes(), str(root / 'queries.json'))
    _require(data.get('schema') == SIXLANG_SCHEMA, 'unknown six-language fixture schema')
    _require(set(data.get('languages', [])) == SIXLANG_LANGUAGES,
             'six-language fixture must cover exactly C++, Go, Java, Python, Rust, TypeScript')
    _require(len(data.get('queries', [])) >= 12, 'six-language fixture needs at least twelve queries')
    for row in data['queries']:
        source = root / 'corpus' / row['path']
        _require(source.is_file(), f"missing six-language source: {row['path']}")
        _require(sha256_file(source) == row['sha256'], f"six-language source hash mismatch: {row['path']}")
    return data


# --------------------------------------------------------------------------
# Harness
# --------------------------------------------------------------------------

class Runner:
    def __init__(self, args, data):
        self.args = args
        self.data = data
        self.tokens = data['tokens']
        self.work = Path(args.work).resolve()
        self.a = self.work / 'repo-a'
        self.b = self.work / 'repo-b'
        self.raw = self.work / 'raw'
        self.wg = Path(args.wg).resolve()
        self.sgrep = Path(args.sgrep).resolve()
        self.rg = Path(args.rg).resolve()
        self.env = dict(os.environ, HF_HUB_OFFLINE='1')
        self.env[self.args.store_env] = str(self.work / 'service')
        self.env.pop('RIPGREP_CONFIG_PATH', None)
        for key in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE'):
            self.env.pop(key, None)
        self.report = {
            'protocol': 'wgrep-wg-release-v1',
            'harness_sha256': sha256_file(__file__),
            'queries_sha256': sha256_file(QUERIES_PATH),
            'identities': {},
            'phases': {},
            'commands': [],
            'failures': [],
            'cleanup': None,
            'result': None,
        }
        self.failures = []
        self.started = False
        self.cleanup_ok = False
        self._serial = 0
        self._lock = threading.Lock()
        self._deadline = time.monotonic() + args.deadline
        self._events_offset = 0

    # -- low-level process execution -------------------------------------
    def run(self, argv, cwd=None, tag='cmd', timeout=None, cleanup=False, input_bytes=None):
        argv = [str(x) for x in argv]
        with self._lock:
            self._serial += 1
            n = self._serial
        remaining = self._deadline - time.monotonic()
        if not cleanup and remaining <= 0:
            raise HarnessError(f'overall deadline reached before {tag}: {argv}')
        budget = min(self.args.timeout, timeout if timeout is not None else self.args.timeout)
        if not cleanup:
            budget = min(budget, remaining)
        stem = self.raw / f'{n:04d}-{tag}'
        stem.parent.mkdir(parents=True, exist_ok=True)
        start = time.monotonic()
        p = subprocess.Popen(argv, cwd=str(cwd) if cwd else None, env=self.env,
                             stdin=subprocess.PIPE if input_bytes is not None else subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE)
        timed_out = False
        try:
            out, err = p.communicate(input=input_bytes, timeout=max(.001, budget))
            code = p.returncode
        except subprocess.TimeoutExpired:
            p.kill()
            out, err = p.communicate()
            code = None
            timed_out = True
        elapsed = (time.monotonic() - start) * 1000.0
        out_path = stem.with_suffix('.stdout')
        err_path = stem.with_suffix('.stderr')
        out_path.write_bytes(out)
        err_path.write_bytes(err)
        if input_bytes is not None:
            stem.with_suffix('.stdin').write_bytes(input_bytes)
        proc = Proc(argv, str(cwd) if cwd else None, code, timed_out, elapsed,
                    out_path, err_path, out, err)
        self.report['commands'].append({
            'tag': tag, 'argv': argv, 'cwd': proc.cwd, 'exit': code,
            'timed_out': timed_out, 'elapsed_ms': elapsed,
            'stdout_file': str(out_path), 'stderr_file': str(err_path),
            'stdin_bytes': len(input_bytes) if input_bytes is not None else None})
        return proc

    def checked(self, argv, cwd=None, tag='cmd', allow=(0, 1)):
        p = self.run(argv, cwd, tag)
        if p.timed_out:
            self.fail(f'{tag}: timeout after {self.args.timeout}s: {argv}')
            raise HarnessError(f'{tag}: timeout: {argv}')
        if p.exit not in allow:
            self.fail(f'{tag}: exit {p.exit}: {argv}: {p.stderr[:400]!r}')
            raise HarnessError(f'{tag}: exit {p.exit}: {argv}')
        return p

    def require(self, ok, message):
        if not ok:
            self.fail(message)
            raise HarnessError(message)

    def fail(self, message):
        self.failures.append(message)
        print(f'FAIL {message}', file=sys.stderr, flush=True)

    def parse_rows(self, proc, where):
        try:
            rows = load_json_strict(proc.stdout, where)
            validate_rows(rows, where)
            return rows, None
        except ValueError as exc:
            return None, str(exc)

    # -- semantic / text /
    def semantic_argv(self, query, root, top, flag='--top'):
        # The sgrep parity contract applies to the classic ranker only.
        return [self.wg, *self.args.wg_global_args, query, str(root),
                '--model', self.args.model, '--rank', 'classic', '--json',
                '--explain', flag, str(top)]

    def control_argv(self, query, root, top):
        return [self.sgrep, query, str(root), '--model', self.args.model,
                '--json', '--explain', '--no-cache', '-n', str(top)]

    def text_argv(self, args):
        base = []
        if self.args.text_sort:
            base += ['--sort', self.args.text_sort]
        base += ['--color', self.args.text_color]
        return [self.wg, *self.args.wg_global_args, *base, *self.args.wg_text_args, *args]

    def text_control_argv(self, args):
        base = []
        if self.args.text_sort:
            base += ['--sort', self.args.text_sort]
        base += ['--color', self.args.text_color]
        return [self.rg, *base, *args]

    def status(self):
        p = self.checked([self.wg, 'status', '--json'], tag='status')
        return load_json_strict(p.stdout, 'status --json')

    def model_loads(self, status):
        for key in ('model_loads', 'loaded_models'):
            if key in status:
                return status[key]
        return None

    def roots(self, status):
        return status.get('roots') or []

    def capture_events(self, tag):
        name = self.args.events_env
        path = self.env.get(name)
        record = {'tag': tag, 'env': name, 'path': path, 'present': False}
        if path and Path(path).is_file():
            target = self.raw / f'events-{tag}.ndjson'
            target.write_bytes(Path(path).read_bytes())
            record.update(present=True, copied_to=str(target),
                          size=target.stat().st_size)
        self.report.setdefault('events', []).append(record)

    # -- fixture ----------------------------------------------------------
    def build_fixture(self):
        t = self.tokens
        (self.a / 'sub').mkdir(parents=True)
        (self.a / 'bench').mkdir(parents=True)
        self.b.mkdir(parents=True)
        (self.a / 'sub' / 'alpha.rs').write_text(
            f'fn {t["alpha_old"]}() {{ println!("alpha ledger decoder"); }}\n', encoding='utf-8')
        (self.a / 'sub' / 'restored.rs').write_text(
            f'fn {t["restored_old"]}() {{ println!("restored mtime fixture"); }}\n', encoding='utf-8')
        (self.a / 'bench' / 'bench_edit.rs').write_text(
            f'fn {t["bench_a"]}() {{ println!("bench toggle line"); }}\n', encoding='utf-8')
        (self.a / '.gitignore').write_text('ignored_marker.txt\n', encoding='utf-8')
        (self.a / 'ignored_marker.txt').write_text(
            f'{t["ignored"]} content here\n', encoding='utf-8')
        (self.a / 'sub' / '.gitignore').write_text('ignored_keep.txt\n', encoding='utf-8')
        (self.a / 'sub' / 'ignored_keep.txt').write_text(
            f'{t["ignored_keep"]} keep content\n', encoding='utf-8')
        (self.a / 'binary_nul.bin').write_bytes(
            b'needle_before_nul_forest\x00\x01\x02needle_after_nul_harbor\x00\xff\xfe\x00tail\n')
        for i in range(self.args.background_files):
            (self.a / f'background-{i:04}.rs').write_text(
                f'fn background_{i:04}() {{ println!("ordinary helper number {i}"); }}\n',
                encoding='utf-8')
        (self.b / 'beta.rs').write_text(
            f'fn {t["beta"]}() {{ println!("beta queue scheduler"); }}\n', encoding='utf-8')
        # A `.git` marker makes rg apply `.gitignore` (rg requires a repository
        # unless --no-require-git). Without it the ignore cells would silently
        # stop testing ignore behavior.
        for root in (self.a, self.b):
            self.checked(['git', 'init', '-q'], cwd=root, tag='fixture-git-init', allow=(0,))

    # -- mutations --------------------------------------------------------
    def mutate(self, name):
        t = self.tokens
        if name is None:
            return
        if name == 'edit_alpha':
            (self.a / 'sub' / 'alpha.rs').write_text(
                f'fn {t["alpha_new"]}() {{ println!("alpha ledger decoder"); }}\n', encoding='utf-8')
        elif name == 'create_added':
            (self.a / 'created.rs').write_text(
                f'fn {t["added"]}() {{ println!("new packet codec"); }}\n', encoding='utf-8')
        elif name == 'rename_added':
            (self.a / 'created.rs').rename(self.a / 'renamed.rs')
        elif name == 'delete_added':
            (self.a / 'renamed.rs').unlink(missing_ok=True)
        elif name == 'restored_mtime':
            target = self.a / 'sub' / 'restored.rs'
            old = target.read_bytes()
            new = old.replace(t['restored_old'].encode(), t['restored_new'].encode())
            if new == old or len(new) != len(old):
                raise HarnessError('restored_mtime must produce a same-length byte change')
            st = target.stat()
            target.write_bytes(new)
            os.utime(target, ns=(st.st_atime_ns, st.st_mtime_ns))
        elif name == 'unignore_ignored':
            (self.a / '.gitignore').write_text('', encoding='utf-8')
        else:
            raise HarnessError(f'unknown mutation {name!r}')

    # -- phases -----------------------------------------------------------
    def preflight(self):
        idents = self.report['identities']
        self.require(self.wg.is_file(), f'candidate wg binary missing: {self.wg}')
        if self.args.wg_sha256:
            actual = sha256_file(self.wg)
            self.require(actual == self.args.wg_sha256,
                         f'candidate wg SHA-256 mismatch: {actual}')
        self.require(self.sgrep.is_file(), f'pinned control sgrep missing: {self.sgrep}')
        control_sha = sha256_file(self.sgrep)
        self.require(control_sha == self.args.sgrep_sha256,
                     f'control sgrep SHA-256 mismatch: {control_sha}')
        self.require(self.rg.is_file(), f'rg control missing: {self.rg}')
        rg_version = self.run([self.rg, '--version'], tag='rg-version')
        first = rg_version.stdout.splitlines()[0].decode(errors='replace') if rg_version.stdout else ''
        self.require(rg_version.exit == 0 and first.split()[:2] == ['ripgrep', self.args.rg_version],
                     f'rg version mismatch: {first!r} (want {self.args.rg_version})')
        wg_version = self.run([self.wg, '--version'], tag='wg-version')
        idents.update({
            'wg': {'path': str(self.wg), 'sha256': sha256_file(self.wg),
                   'source_revision': self.args.wg_source_revision,
                   'version_stdout': wg_version.stdout.decode(errors='replace').strip()},
            'sgrep_control': {'path': str(self.sgrep), 'sha256': control_sha,
                              'source_revision': CONTROL_REVISION},
            'rg_control': {'path': str(self.rg), 'sha256': sha256_file(self.rg),
                           'version_stdout': first},
            'model': {'choice': self.args.model, **CODE_MODEL},
            'config': {'store_env': self.args.store_env,
                       'store_dir': self.env[self.args.store_env],
                       'events_env': self.args.events_env,
                       'text_sort': self.args.text_sort, 'text_color': self.args.text_color,
                       'top': self.args.top,
                       'wg_global_args': self.args.wg_global_args,
                       'wg_text_args': self.args.wg_text_args},
        })
        self.build_fixture()

    def phase_lifecycle(self):
        # Mark ownership before launch: a partial concurrent up can still have
        # started the service, so cleanup must run even if one call fails.
        self.started = True
        with ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(self.checked, [self.wg, 'up', self.a, '--model', self.args.model],
                                   None, 'up-concurrent', (0,)) for _ in range(2)]
            for future in futures:
                future.result()
        fresh = self.status()
        pid = fresh.get('pid')
        self.require(pid is not None, 'status --json did not report a pid')
        ups = []
        ups.append(load_json_strict(self.run(
            [self.wg, 'up', self.a, '--model', self.args.model, '--json'],
            tag='up-repeat-json').stdout, 'up --json'))
        after_repeat = self.status()
        self.require(after_repeat.get('pid') == pid, 'repeated up changed daemon PID')
        ups.append(load_json_strict(self.run(
            [self.wg, 'up', self.b, '--model', self.args.model, '--json'],
            tag='up-second-json').stdout, 'up --json'))
        two = self.status()
        self.require(two.get('pid') == pid, 'second root did not share the daemon PID')
        self.require(len(self.roots(two)) == 2, 'two repositories did not attach to one daemon')
        loads = self.model_loads(two)
        self.require(loads == 1, f'code model was not loaded exactly once: {loads!r}')
        self.report['phases']['lifecycle'] = {
            'pid': pid, 'roots': len(self.roots(two)), 'model_loads': loads,
            'up_json': ups, 'status_after_two_roots': two}

    def phase_isolation(self):
        t = self.tokens
        a_rows = self._semantic_rows(self.a, t['alpha_old'], 'isolation-a')
        b_rows = self._semantic_rows(self.b, t['beta'], 'isolation-b')
        self.require(any(norm_path(r['path']).endswith('alpha.rs') for r in a_rows),
                     'root A query did not return its own file')
        self.require(not any(t['beta'] in r['content'] for r in a_rows),
                     'root A result leaked root B content')
        self.require(any(norm_path(r['path']).endswith('beta.rs') for r in b_rows),
                     'root B query did not return its own file')
        self.require(not any(t['alpha_old'] in r['content'] for r in b_rows),
                     'root B result leaked root A content')
        implicit = self.run([self.wg, *self.args.wg_global_args, t['alpha_old'],
                              '--model', self.args.model, '--json', '--explain', '--top', str(self.args.top)],
                            cwd=self.a / 'sub', tag='isolation-implicit')
        rows, problem = self.parse_rows(implicit, 'isolation-implicit')
        self.require(problem is None, f'implicit subdir query failed: {problem}')
        self.require(not any('beta.rs' in norm_path(r['path']) for r in rows),
                     'implicit subdir query selected a different repository')
        self.report['phases']['isolation'] = {
            'a_rows': len(a_rows), 'b_rows': len(b_rows),
            'implicit_rows': len(rows), 'implicit_cwd': str(self.a / 'sub')}

    def _semantic_rows(self, root, query, tag):
        p = self.checked(self.semantic_argv(query, root, self.args.top), tag=tag)
        rows, problem = self.parse_rows(p, tag)
        self.require(problem is None, f'{tag}: {problem}')
        return rows

    def phase_semantic(self):
        steps = []
        for step in self.data['mutations']:
            label = step['id']
            if step['mutation']:
                self.mutate(step['mutation'])
            start = time.monotonic()
            if self.args.settle_ms > 0:
                time.sleep(self.args.settle_ms / 1000.0)
            attempt_log = []
            visible_ms = None
            deadline = start + self.args.visibility_timeout
            while time.monotonic() < deadline:
                p = self.run(self.semantic_argv(step['query'], self.a, self.args.top),
                              tag=f'visibility-{label}', timeout=deadline-time.monotonic())
                rows, problem = self.parse_rows(p, f'visibility-{label}')
                ok = not p.timed_out and p.exit in (0, 1) and problem is None and predicate_holds(rows, step['expect'])
                attempt_log.append({
                    'elapsed_ms': p.elapsed_ms, 'exit': p.exit, 'timed_out': p.timed_out,
                    'json_ok': problem is None, 'parse_error': problem,
                    'rows': len(rows) if rows is not None else None, 'predicate_ok': bool(ok),
                    'stdout_file': p.stdout_path, 'stderr_file': p.stderr_path})
                if ok:
                    visible_ms = (time.monotonic() - start) * 1000.0
                    break
                if time.monotonic() >= deadline:
                    break
                time.sleep(self.args.poll_ms / 1000.0)
            if visible_ms is None:
                self.fail(f'semantic {label}: expectation not visible before deadline: '
                           f'{step["query"]!r}')
            self.capture_events(f'after-{label}')
            # Fresh exact parity against the pinned control, after visibility.
            control = self.run(self.control_argv(step['query'], self.a, self.args.top),
                               tag=f'control-{label}')
            candidate = self.run(self.semantic_argv(step['query'], self.a, self.args.top),
                                 tag=f'compare-{label}')
            c_rows, n_rows, issues = [], [], []
            try:
                c_rows = load_json_strict(control.stdout, f'control-{label}')
                validate_rows(c_rows, f'control-{label}')
                n_rows = load_json_strict(candidate.stdout, f'compare-{label}')
                validate_rows(n_rows, f'compare-{label}')
                issues = compare_semantic(c_rows, n_rows, control.exit, candidate.exit,
                                          f'semantic[{label}]')
            except ValueError as exc:
                issues = [f'semantic[{label}]: output validation failed: {exc}']
            for issue in issues:
                self.fail(issue)
            steps.append({'id': label, 'mutation': step['mutation'], 'query': step['query'],
                          'visible_ms': visible_ms, 'attempts': attempt_log,
                          'control_exit': control.exit, 'candidate_exit': candidate.exit,
                          'control_rows': len(c_rows), 'candidate_rows': len(n_rows),
                          'issues': issues})
        self.report['phases']['semantic'] = {'steps': steps}
        self._count_contract()

    def _count_contract(self):
        query = next((q for q in self.data['warm_semantic'] if 'helper' in q),
                     self.data['warm_semantic'][0])
        top = self.args.top
        control = self.checked(self.control_argv(query, self.a, top), tag='count-control')
        cand_top = self.checked(self.semantic_argv(query, self.a, top, '--top'), tag='count-top')
        cand_alias = self.checked(self.semantic_argv(query, self.a, top, '-k'), tag='count-k')
        rows = {}
        issues = []
        for name, p in (('control', control), ('top', cand_top), ('alias', cand_alias)):
            try:
                parsed = load_json_strict(p.stdout, f'count-{name}')
                validate_rows(parsed, f'count-{name}')
                rows[name] = parsed
            except ValueError as exc:
                issues.append(f'count[{name}]: {exc}')
        if not issues:
            issues += compare_semantic(rows['control'], rows['top'], control.exit, cand_top.exit,
                                       'count[top]')
            issues += compare_semantic(rows['top'], rows['alias'], cand_top.exit, cand_alias.exit,
                                       'count[-k]')
            for name in rows:
                if len(rows[name]) > top:
                    issues.append(f'count[{name}]: returned {len(rows[name])} rows for top={top}')
        for issue in issues:
            self.fail(issue)
        self.report['phases']['count'] = {
            'query': query, 'top': top, 'control_flag': '-n',
            'candidate_flags': ['--top', '-k'], 'issues': issues,
            'control_rows': len(rows.get('control', [])),
            'top_rows': len(rows.get('top', [])), 'alias_rows': len(rows.get('alias', []))}

    def phase_text(self):
        cells = []
        for cell in self.data['text_cells']:
            cand = self.run(self.text_argv(cell['args']), cwd=self.a, tag=f'text-{cell["id"]}')
            ctrl = self.run(self.text_control_argv(cell['args']), cwd=self.a, tag=f'rg-{cell["id"]}')
            issues = compare_text(cand, ctrl, f'text[{cell["id"]}]')
            if cell.get('expect_error') and not issues:
                if cand.exit == 0:
                    issues.append(f'text[{cell["id"]}]: expected a non-zero exit')
            for issue in issues:
                self.fail(issue)
            cells.append({'id': cell['id'], 'args': cell['args'],
                          'candidate': {'argv': cand.argv, 'exit': cand.exit, 'timed_out': cand.timed_out},
                          'control': {'argv': ctrl.argv, 'exit': ctrl.exit, 'timed_out': ctrl.timed_out},
                           'issues': issues})
        # A one-file root makes bare rg defaults deterministic. Do not prepend
        # --sort/--color here: these calls must test flag-based route selection
        # and distinguish DEVNULL from a real input pipe.
        bare = [
            ('bare-literal-cwd', ['-F', self.tokens['beta']], None),
            ('bare-line-numbers', ['-n', '-e', self.tokens['beta']], None),
            ('bare-regex', ['-e', 'BorealisUniqueV[0-9]+'], None),
            ('bare-json', ['--json', '-F', self.tokens['beta']], None),
            ('pipe-literal', ['-F', 'needle', '-'], b'ignore\nneedle match\n'),
            ('pipe-json', ['--json', '-F', 'needle', '-'], 'ignore\nneedle \u03c0 match\n'.encode()),
            ('empty-pipe-implicit-cwd', ['-F', self.tokens['beta']], b''),
            ('pattern-like-global-option', ['-F', '-e', '--cache-dir', 'beta.rs'], None),
        ]
        for label, argv, data in bare:
            cand = self.run([self.wg, *self.args.wg_global_args, *argv], cwd=self.b,
                            tag=label, input_bytes=data)
            ctrl = self.run([self.rg, *argv], cwd=self.b, tag='rg-'+label, input_bytes=data)
            issues = compare_text(cand, ctrl, label)
            for issue in issues:
                self.fail(issue)
            cells.append({'id': label, 'args': argv, 'stdin_bytes': len(data) if data is not None else None,
                          'candidate': {'exit': cand.exit}, 'control': {'exit': ctrl.exit}, 'issues': issues})
        self.report['phases']['text'] = {'cells': cells}

    def phase_bench(self):
        top = self.args.top
        queries = self.data['warm_semantic']
        bench_root = Path(self.args.bench_root).resolve() if self.args.bench_root else self.a
        if self.args.bench_root:
            self.checked([self.wg, 'up', bench_root, '--model', self.args.model, '--json'],
                         tag='bench-root-up', allow=(0,))
            queries = self.data.get('corpus_semantic', queries)
        for _ in range(self.args.warmup):
            self.run(self.semantic_argv(queries[0], bench_root, top), tag='warmup-sem')
        sem_samples = []
        for i in range(self.args.samples):
            q = queries[i % len(queries)]
            p = self.run(self.semantic_argv(q, bench_root, top), tag=f'warm-sem-{i}')
            rows, problem = self.parse_rows(p, f'warm-sem-{i}')
            valid = not p.timed_out and p.exit == 0 and problem is None and bool(rows)
            sem_samples.append({'i': i, 'query': q, 'ms': p.elapsed_ms, 'exit': p.exit,
                                 'timed_out': p.timed_out, 'json_ok': problem is None,
                                'valid': valid,
                                 'rows': len(rows) if rows is not None else None})
            if not valid:
                self.fail(f'warm-sem-{i}: failed/empty semantic response, exit={p.exit}, parse={problem}')
        text_samples = []
        text_cells = self.data['warm_text']
        if self.args.bench_root:
            text_cells = self.data.get('corpus_text', text_cells)
        for _ in range(self.args.warmup):
            self.run(self.text_argv(text_cells[0]['args']), cwd=bench_root, tag='warmup-text')
        for i in range(self.args.samples):
            cell = text_cells[i % len(text_cells)]
            p = self.run(self.text_argv(cell['args']), cwd=bench_root, tag=f'warm-text-{i}')
            text_samples.append({'i': i, 'cell': cell['id'], 'ms': p.elapsed_ms, 'exit': p.exit,
                                 'timed_out': p.timed_out,
                                 'valid': not p.timed_out and p.exit in (0, 1)})
            if p.timed_out or p.exit not in (0, 1):
                self.fail(f'warm-text-{i}: failed request, exit={p.exit}, timeout={p.timed_out}')
        bench = {'samples_requested': self.args.samples, 'warmup': self.args.warmup,
                 'bench_root': str(bench_root),
                 'semantic': {'queries': queries,
                              'distribution': distribution([s['ms'] for s in sem_samples
                                                            if s['valid']]),
                               'errors': sum(1 for s in sem_samples if not s['valid']),
                              'samples': sem_samples},
                 'text': {'cells': [c['id'] for c in text_cells],
                          'distribution': distribution([s['ms'] for s in text_samples
                                                        if s['valid']]),
                           'errors': sum(1 for s in text_samples if not s['valid']),
                          'samples': text_samples}}
        if self.args.bench_control:
            ctrl = []
            for i in range(self.args.samples):
                q = queries[i % len(queries)]
                p = self.run(self.control_argv(q, bench_root, top), tag=f'warm-sem-control-{i}')
                rows, problem = self.parse_rows(p, f'warm-sem-control-{i}')
                if p.timed_out or p.exit != 0 or problem is not None or not rows:
                    self.fail(f'warm-sem-control-{i}: failed control response')
                ctrl.append({'i': i, 'query': q, 'ms': p.elapsed_ms, 'exit': p.exit,
                             'timed_out': p.timed_out})
            sem_ok = distribution([s['ms'] for s in sem_samples if s['valid']])['p50']
            ctrl_ok = distribution([s['ms'] for s in ctrl if not s['timed_out']])['p50']
            bench['semantic_control'] = {'distribution': distribution(
                [s['ms'] for s in ctrl if not s['timed_out']]), 'samples': ctrl}
            if sem_ok and ctrl_ok and not self.failures:
                bench['semantic_control']['ratio_p50_candidate_over_control'] = sem_ok / ctrl_ok
        bench['visibility'] = self._visibility_bench()
        self.report['phases']['bench'] = bench

    def _visibility_bench(self):
        t = self.tokens
        root = self.a
        corpus_setup = None
        if self.args.visibility_corpus:
            source = Path(self.args.visibility_corpus).resolve()
            root = self.work / 'visibility-corpus'
            root.mkdir()
            listing = self.checked([self.rg, '--no-config', '--files', '-0', '.'],
                                   cwd=source, tag='visibility-corpus-files', allow=(0,))
            names = [Path(os.fsdecode(n)) for n in listing.stdout.split(b'\0') if n]
            self.require(len(names) <= 100000, 'visibility corpus exceeds copy file-count bound')
            for rel in names:
                self.require(not rel.is_absolute() and '..' not in rel.parts,
                             'visibility corpus contains an escaping relative path')
                src = source / rel
                self.require(not src.is_symlink(), 'visibility corpus input unexpectedly contains a symlink')
                dst = root / rel
                dst.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(src, dst)
            target = root / 'wg_release_visibility_probe.rs'
            self.require(not target.exists(), 'visibility probe collides with an existing source file')
            target.write_text(f'fn {t["bench_a"]}() {{ println!("bench toggle line"); }}\n', encoding='utf-8')
            prepared = self.checked([self.wg, 'up', root, '--model', self.args.model, '--json'],
                                    tag='visibility-corpus-up', allow=(0,))
            corpus_setup = {'source': str(source), 'copied_files': len(names), 'root': str(root),
                            'up': load_json_strict(prepared.stdout, 'visibility corpus up')}
        else:
            target = self.a / 'bench' / 'bench_edit.rs'
        cur = t['bench_a']
        alt = t['bench_b']
        samples = []
        for i in range(self.args.visibility_samples):
            cur = alt if cur == t['bench_a'] else t['bench_a']
            target.write_text(f'fn {cur}() {{ println!("bench toggle line"); }}\n', encoding='utf-8')
            start = time.monotonic()
            deadline = start + self.args.visibility_timeout
            attempts = 0
            measured = None
            attempt_rows = []
            while time.monotonic() < deadline:
                attempts += 1
                p = self.run(self.semantic_argv(cur, root, self.args.top), tag=f'visbench-{i}',
                             timeout=deadline-time.monotonic())
                rows, problem = self.parse_rows(p, f'visbench-{i}')
                correct = not p.timed_out and p.exit == 0 and problem is None and any(cur in r['content'] for r in rows)
                attempt_rows.append({'exit': p.exit, 'timeout': p.timed_out, 'json_error': problem,
                                     'correct': correct, 'elapsed_ms': p.elapsed_ms,
                                     'stdout_file': p.stdout_path, 'stderr_file': p.stderr_path})
                if correct:
                    measured = (time.monotonic() - start) * 1000.0
                    break
                time.sleep(self.args.poll_ms / 1000.0)
            samples.append({'i': i, 'token': cur, 'ms': measured, 'attempts': attempts,
                            'attempt_rows': attempt_rows})
            if measured is None:
                self.fail(f'visibility bench {i}: token {cur} not visible before deadline')
        final_issues = []
        if corpus_setup and all(s['ms'] is not None for s in samples):
            control = self.run(self.control_argv(cur, root, self.args.top), tag='visibility-final-control')
            candidate = self.run(self.semantic_argv(cur, root, self.args.top), tag='visibility-final-candidate')
            try:
                crows = load_json_strict(control.stdout, 'visibility final control')
                nrows = load_json_strict(candidate.stdout, 'visibility final candidate')
                validate_rows(crows, 'visibility final control')
                validate_rows(nrows, 'visibility final candidate')
                final_issues = compare_semantic(crows, nrows, control.exit, candidate.exit, 'visibility final')
            except ValueError as exc:
                final_issues = [str(exc)]
            for issue in final_issues:
                self.fail('large visibility final fresh parity: '+issue)
        return {'samples_requested': self.args.visibility_samples, 'corpus_setup': corpus_setup,
                'root': str(root),
                'final_fresh_parity_issues': final_issues,
                'distribution': distribution([s['ms'] for s in samples if s['ms'] is not None]),
                'misses': sum(1 for s in samples if s['ms'] is None), 'samples': samples}

    def phase_sixlang(self):
        self.require(self.args.sixlang_fixture is not None,
                     '--sixlang-fixture is required for the sixlang phase')
        fixture = load_sixlang_fixture(self.args.sixlang_fixture)
        root = Path(self.args.sixlang_fixture) / 'corpus'
        self.checked([self.wg, 'up', root, '--model', self.args.model, '--json'],
                     tag='sixlang-up', allow=(0,))
        record = {'fixture': str(self.args.sixlang_fixture), 'rows': []}
        for q in fixture['queries']:
            control = self.run(self.control_argv(q['query'], root, self.args.top), tag=f'six-{q["id"]}-control')
            candidate = self.run(self.semantic_argv(q['query'], root, self.args.top), tag=f'six-{q["id"]}')
            issues = []
            try:
                c_rows = load_json_strict(control.stdout, f'six-{q["id"]}-control')
                validate_rows(c_rows, f'six-{q["id"]}-control')
                n_rows = load_json_strict(candidate.stdout, f'six-{q["id"]}')
                validate_rows(n_rows, f'six-{q["id"]}')
                issues = compare_semantic(c_rows, n_rows, control.exit, candidate.exit, f'six[{q["id"]}]')
            except ValueError as exc:
                issues = [f'six[{q["id"]}]: {exc}']
            for issue in issues:
                self.fail(issue)
            record['rows'].append({'id': q['id'], 'issues': issues})
        self.report['phases']['sixlang'] = record

    # -- cleanup / finalize ----------------------------------------------
    def cleanup(self):
        record = {'attempted': False, 'down_exit': None, 'timed_out': False,
                  'status_exit': None, 'daemon_gone': None, 'ok': False}
        if not self.started:
            record['ok'] = True
            record['note'] = 'no owned daemon was started'
            self.cleanup_ok = True
            self.report['cleanup'] = record
            return
        record['attempted'] = True
        try:
            down = self.run([self.wg, 'down'], tag='cleanup-down', timeout=30, cleanup=True)
            record['down_exit'] = down.exit
            record['timed_out'] = down.timed_out
            until = time.monotonic() + 5
            gone = False
            while time.monotonic() < until:
                status = self.run([self.wg, 'status', '--json'], tag='cleanup-status',
                                  timeout=max(.001, until-time.monotonic()), cleanup=True)
                record['status_exit'] = status.exit
                gone = (not status.timed_out and status.exit != 0
                        and not (Path(self.env[self.args.store_env]) / 'service.json').exists())
                if gone:
                    break
                time.sleep(.01)
            record['daemon_gone'] = gone
            record['ok'] = (down.exit == 0 and not down.timed_out and gone)
        except Exception as exc:  # noqa: BLE001 - cleanup must never mask PASS
            record['error'] = str(exc)
            record['ok'] = False
        self.cleanup_ok = bool(record['ok'])
        self.report['cleanup'] = record
        if not self.cleanup_ok:
            self.fail('cleanup failed: owned daemon may still be running')

    def finalize(self, ran_phases):
        self.report['failures'] = list(self.failures)
        self.report['phases_ran'] = list(ran_phases)
        self.report['result'] = 'PASS' if (not self.failures and self.cleanup_ok) else 'FAIL'
        out = self.work / 'result.json'
        out.write_text(json.dumps(self.report, indent=2, ensure_ascii=False) + '\n', encoding='utf-8')
        print(json.dumps({'result': self.report['result'], 'failures': self.failures,
                          'cleanup': self.report['cleanup'], 'report': str(out)}, indent=2))
        return 0 if self.report['result'] == 'PASS' else 1


MUTATION_NAMES = ('edit_alpha', 'create_added', 'rename_added', 'delete_added',
                  'restored_mtime', 'unignore_ignored')


# --------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------

def self_test():
    base = {'path': 'sub/alpha.rs', 'start': 2, 'end': 3, 'content': 'x\ny\n',
            'scores': {'bm25': 0.25, 'semantic': 0.5, 'fused': 0.4375, 'exact_identifier': True}}
    _require(not compare_semantic([base], [dict(base)], 0, 0, 't'), 'identical rows must pass')
    _require(any('content differs' in i for i in compare_semantic(
        [base], [dict(base, content='wrong')], 0, 0, 't')), 'content diff not detected')
    _require(any('path differs' in i for i in compare_semantic(
        [base], [dict(base, path='go/main.go')], 0, 0, 't')), 'path diff not detected')
    _require(any('exit code' in i for i in compare_semantic([base], [base], 1, 0, 't')),
             'exit diff not detected')
    swapped = [dict(base, start=3, end=4), dict(base, start=2, end=3)]
    _require(any('start differs' in i for i in compare_semantic([base, base], swapped, 0, 0, 't')),
             'order change not detected')
    _require(any('exact numeric mismatch' in i for i in compare_semantic(
        [base], [dict(base, scores=dict(base['scores'], bm25=0.2500001))], 0, 0, 't')),
        'exact numeric mismatch not detected')
    _require(any('score field mismatch' in i for i in compare_semantic(
        [base], [dict(base, scores={'bm25': 0.25})], 0, 0, 't')),
        'score field-set change not detected')
    _require(any('non-finite' in i for i in compare_scores({'bm25': float('nan')}, {'bm25': 0.0}, 't')),
             'non-finite score not rejected')
    try:
        validate_rows([dict(base, scores={'bm25': float('inf')})], 't')
    except ValueError:
        pass
    else:
        raise ValueError('validate_rows accepted a non-finite score')
    try:
        validate_rows([dict(base, start=0)], 't')
    except ValueError:
        pass
    else:
        raise ValueError('validate_rows accepted a bad line range')

    try:
        load_json_strict(b'{"x": NaN}', 't')
    except ValueError:
        pass
    else:
        raise ValueError('load_json_strict accepted NaN')
    try:
        load_json_strict(b'{not json', 't')
    except ValueError:
        pass
    else:
        raise ValueError('load_json_strict accepted invalid JSON')
    _require(load_json_strict(b'[{"a": 1.5}]', 't') == [{'a': 1.5}], 'strict JSON lost data')

    class _P:
        def __init__(self, exit, out, err, timed=False):
            self.exit, self.stdout, self.stderr, self.timed_out = exit, out, err, timed
    _require(not compare_text(_P(0, b'a\n', b''), _P(0, b'a\n', b''), 't'), 'identical text must pass')
    _require(any('stdout' in i for i in compare_text(_P(0, b'b\n', b''), _P(0, b'a\n', b''), 't')),
             'stdout diff not detected')
    _require(any('stderr' in i for i in compare_text(_P(0, b'a\n', b'x'), _P(0, b'a\n', b''), 't')),
             'stderr diff not detected')
    _require(any('exit' in i for i in compare_text(_P(2, b'', b''), _P(0, b'', b''), 't')),
             'exit diff not detected')
    _require(any('timeout' in i for i in compare_text(_P(None, b'', b'', True), _P(0, b'', b''), 't')),
             'one-sided timeout not reported as a mismatch')
    timed_a = {'type': 'summary', 'data': {'elapsed_total': {'nanos': 1},
                'stats': {'elapsed': {'nanos': 1}, 'matches': 3}}}
    timed_b = json.loads(json.dumps(timed_a))
    timed_b['data']['elapsed_total']['nanos'] = 999
    timed_b['data']['stats']['elapsed']['nanos'] = 999
    ja = _P(0, (json.dumps(timed_a)+'\n').encode(), b'')
    jb = _P(0, (json.dumps(timed_b)+'\n').encode(), b'')
    ja.argv = ['wg', '--json']
    _require(not compare_text(ja, jb, 'json-timing'), 'JSON runtime timing alone must not fail parity')
    timed_b['data']['stats']['matches'] = 4
    jb.stdout = (json.dumps(timed_b)+'\n').encode()
    _require(compare_text(ja, jb, 'json-content'), 'JSON non-timing differences must fail parity')

    rows = [{'path': 'sub/alpha.rs', 'start': 1, 'end': 1, 'content': 'AuroraOldV17', 'scores': {'a': 1}},
            {'path': 'created.rs', 'start': 1, 'end': 1, 'content': 'CelestialAddedV31', 'scores': {'a': 1}}]
    _require(predicate_holds(rows, {'contains': 'AuroraOldV17', 'path': 'sub/alpha.rs'}),
             'contains+path predicate failed')
    _require(not predicate_holds(rows, {'contains': 'AuroraOldV17', 'forbid_substring': 'Aurora'}),
             'forbid_substring predicate failed')
    _require(not predicate_holds(rows, {'path': 'renamed.rs', 'forbid_path': 'created.rs'}),
             'path+forbid_path predicate failed')
    _require(predicate_holds([], {'empty': True}), 'empty predicate failed')
    _require(not predicate_holds(rows, {'empty': True}), 'empty predicate accepted rows')

    _require(percentile([1, 2, 3, 4], 50) == 2.5, 'p50 interpolation wrong')
    _require(percentile([1, 2, 3, 4], 0) == 1 and percentile([1, 2, 3, 4], 100) == 4,
             'percentile endpoints wrong')
    _require(percentile([10], 95) == 10, 'single-sample percentile wrong')
    d = distribution([1, 2, 3, 4])
    _require(d['count'] == 4 and d['min'] == 1 and d['max'] == 4 and d['p50'] == 2.5,
             'distribution wrong')
    _require(distribution([])['p95'] is None, 'empty distribution should be None')

    data = load_queries(QUERIES_PATH)
    _require(data['schema'] == 'wgrep-wg-release-v1', 'query-set schema wrong')
    _require(len(data['mutations']) >= 6 and len(data['text_cells']) >= 12, 'query set too small')
    _require(any(c.get('expect_error') for c in data['text_cells']), 'query set lost its error cell')
    _require(data['warm_semantic'] and data['warm_text'], 'warm query sets missing')

    class _Args:
        wg_global_args = []
        wg_text_args = []
        text_sort = 'path'
        text_color = 'never'
        model = 'code'
        top = 10
    r = Runner.__new__(Runner)
    r.args = _Args()
    r.wg = Path('wg')
    r.sgrep = Path('sgrep')
    r.rg = Path('rg')
    sem = r.semantic_argv('q', Path('root'), 7)
    _require('--top' in sem and '7' in sem and '-n' not in sem, 'candidate semantic argv must use --top')
    ctrl = r.control_argv('q', Path('root'), 7)
    _require('-n' in ctrl and '7' in ctrl, 'control semantic argv must use -n')
    alias = r.semantic_argv('q', Path('root'), 7, '-k')
    _require('-k' in alias and '--top' not in alias, 'alias semantic argv must use -k')
    txt = [str(x) for x in r.text_argv(['-e', 'x', '.'])]
    _require(txt[0] == 'wg' and '--sort' in txt and 'path' in txt and '--color' in txt,
             'text argv must carry sort/color base flags')
    rg_argv = [str(x) for x in r.text_control_argv(['-e', 'x', '.'])]
    _require(rg_argv[0] == 'rg', 'text control argv must run rg')

    def _bad_mut(data):
        bad = json.loads(json.dumps(data))
        bad['mutations'][1]['mutation'] = 'does_not_exist'
        return bad
    tmp = Path(os.environ.get('TEMP', '/tmp')) / 'wgrep_wg_release_selftest.json'
    tmp.write_text(json.dumps(_bad_mut(data)), encoding='utf-8')
    try:
        load_queries(tmp)
    except ValueError:
        pass
    else:
        raise ValueError('load_queries accepted an unknown mutation')
    finally:
        tmp.unlink(missing_ok=True)

    print('self-test: PASS (semantic exact/order/scores/finite, text bytes/exit/timeout, '
          'predicates, percentile/distribution, argv contract, query-set validation)')
    return 0


# --------------------------------------------------------------------------

def build_parser():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--wg', type=Path, help='unified wg candidate (required with --run)')
    ap.add_argument('--wg-sha256', help='candidate wg SHA-256 receipt (required with --run)')
    ap.add_argument('--wg-source-revision', help='candidate source identity (required with --run)')
    ap.add_argument('--sgrep', type=Path, default=DEFAULT_SGREP)
    ap.add_argument('--sgrep-sha256', default=CONTROL_SHA256)
    ap.add_argument('--rg', type=Path, default=DEFAULT_RG)
    ap.add_argument('--rg-version', default=RG_VERSION)
    ap.add_argument('--work', type=Path, help='empty scratch/run directory (required with --run)')
    ap.add_argument('--queries', type=Path, default=QUERIES_PATH)
    ap.add_argument('--store-env', default='WG_SERVICE_DIR')
    ap.add_argument('--events-env', default='WG_SERVICE_EVENTS')
    ap.add_argument('--model', choices=['code'], default='code')
    ap.add_argument('--top', type=int, default=10)
    ap.add_argument('--timeout', type=float, default=120)
    ap.add_argument('--deadline', type=float, default=3600)
    ap.add_argument('--visibility-timeout', type=float, default=20)
    ap.add_argument('--poll-ms', type=float, default=10)
    ap.add_argument('--settle-ms', type=float, default=0)
    ap.add_argument('--background-files', type=int, default=64)
    ap.add_argument('--samples', type=int, default=120,
                    help='warm-query samples per workload (p95 cells need >=100)')
    ap.add_argument('--visibility-samples', type=int, default=100,
                    help='edit-visibility samples (p95 cell needs >=100)')
    ap.add_argument('--visibility-corpus', type=Path,
                    help='copy rg-visible files from this read-only corpus into scratch for large-index edit timing')
    ap.add_argument('--bench-root', type=Path,
                    help='read-only root for the warm benchmarks only; default is the scratch repo-a')
    ap.add_argument('--warmup', type=int, default=5)
    ap.add_argument('--bench-control', action='store_true',
                    help='also time the pinned sgrep control on the same semantic workload')
    ap.add_argument('--text-sort', default='path', help="rg --sort value; empty string disables")
    ap.add_argument('--text-color', default='never')
    ap.add_argument('--wg-global-args', nargs='*', default=[],
                    help='extra args inserted after the wg executable for every wg call')
    ap.add_argument('--wg-text-args', nargs='*', default=[],
                    help='extra args inserted only for rg-shaped text calls')
    ap.add_argument('--sixlang-fixture', type=Path, help='optional pinned six-language fixture root')
    ap.add_argument('--phase', action='append', choices=[*PHASES, 'sixlang'],
                    help='run only these phases (repeatable); default all applicable')
    ap.add_argument('--run', action='store_true', help='execute the harness against real binaries')
    ap.add_argument('--self-test', action='store_true')
    ap.add_argument('--verify-queries', action='store_true')
    return ap


def main(argv=None):
    args = build_parser().parse_args(argv)
    if args.self_test:
        return self_test()
    if args.verify_queries:
        data = load_queries(args.queries)
        print(f"queries: PASS ({len(data['mutations'])} mutations, "
              f"{len(data['text_cells'])} text cells, "
              f"{len(data['warm_semantic'])} semantic / {len(data['warm_text'])} text warm queries)")
        return 0
    if not args.run:
        build_parser().print_help()
        return 0

    for name in ('wg', 'work'):
        if getattr(args, name) in (None, ''):
            print(f'error: --{name} is required with --run', file=sys.stderr)
            return 2
    if not args.wg_sha256 or not args.wg_source_revision:
        print('error: --wg-sha256 and --wg-source-revision are required with --run',
              file=sys.stderr)
        return 2
    work = Path(args.work).resolve()
    if work.exists() and any(work.iterdir()):
        print(f'error: --work must be empty: {work}', file=sys.stderr)
        return 2
    work.mkdir(parents=True, exist_ok=True)

    global QUERIES_PATH
    QUERIES_PATH = Path(args.queries)
    data = load_queries(args.queries)

    runner = Runner(args, data)
    requested = set(args.phase or PHASES)
    requested.add('lifecycle')
    selected = [p for p in (*PHASES, 'sixlang') if p in requested]
    if args.sixlang_fixture and 'sixlang' not in selected:
        selected = [*selected, 'sixlang']
    ran = []
    try:
        runner.preflight()
        for phase in selected:
            getattr(runner, f'phase_{phase}')()
            ran.append(phase)
    except HarnessError as exc:
        message = f'aborted: {exc}'
        if message not in runner.failures and str(exc) not in runner.failures:
            runner.fail(message)
    except Exception as exc:  # noqa: BLE001 - any crash must still clean up and fail
        runner.fail(f'unhandled error: {exc!r}')
    finally:
        runner.cleanup()
    return runner.finalize(ran)


if __name__ == '__main__':
    raise SystemExit(main())
