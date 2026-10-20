#!/usr/bin/env python3
"""Live filesystem-watcher overflow, decline, walk parity, and restart check.

Linux-only. Uses production limits and filesystem create events; no daemon
test hooks. Evidence is retained under --work.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path


def run(argv, env, timeout=15):
    return subprocess.run(argv, env=env, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          timeout=timeout)


def records(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def save_process(work, label, argv, result):
    stem = work / label
    (stem.with_suffix('.stdout')).write_bytes(result.stdout)
    (stem.with_suffix('.stderr')).write_bytes(result.stderr)
    (stem.with_suffix('.status')).write_text(str(result.returncode) + '\n')
    return {'argv': argv, 'stdout': str(stem.with_suffix('.stdout')),
            'stderr': str(stem.with_suffix('.stderr')),
            'status_file': str(stem.with_suffix('.status')),
            'exit': result.returncode,
            'stdout_sha256': hashlib.sha256(result.stdout).hexdigest(),
            'stderr_sha256': hashlib.sha256(result.stderr).hexdigest()}


def exact_parity(a, b):
    return (a.stdout, a.stderr, a.returncode) == (b.stdout, b.stderr, b.returncode)


def daemon_rpc(store, request):
    infos = list(store.rglob('daemon.json'))
    if len(infos) != 1:
        return None
    info = json.loads(infos[0].read_text())
    with socket.create_connection(('127.0.0.1', info['port']), timeout=3) as sock:
        sock.settimeout(5)
        sock.sendall((json.dumps(dict(request, token=info['token'])) + '\n').encode())
        with sock.makefile('rb') as stream:
            line = stream.readline()
            if not line.endswith(b'\n'):
                raise RuntimeError('incomplete RPC reply')
            return json.loads(line)


def daemon_stats(store):
    return daemon_rpc(store, {'stats': True})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--wgrep', required=True)
    ap.add_argument('--rg', required=True)
    ap.add_argument('--work', type=Path, required=True)
    ap.add_argument('--files', type=int, default=12000)
    ap.add_argument('--timeout', type=float, default=15)
    args = ap.parse_args()
    if not sys.platform.startswith('linux'):
        ap.error('Linux is required for this inotify watcher test')
    if args.files < 5000 or args.files > 30000:
        ap.error('--files must be in [5000, 30000]')
    work = args.work.resolve()
    if work.exists() and any(work.iterdir()):
        ap.error(f'work directory must be empty: {work}')
    work.mkdir(parents=True, exist_ok=True)
    corpus, store = work / 'corpus', work / 'index'
    corpus.mkdir()
    (corpus / 'seed.txt').write_text('needle seed\n')
    events = work / 'events.ndjson'
    env = dict(os.environ, WGREP_INDEX_DIR=str(store), WGREP_INDEX_EVENTS=str(events))
    env.pop('WGREP_NO_DAEMON', None)
    env.pop('WGREP_NO_INDEX', None)
    wgrep, rg = str(Path(args.wgrep).resolve()), str(Path(args.rg).resolve())
    version = run([rg, '--version'], env, args.timeout)
    if version.returncode != 0 or b'ripgrep 15.1.0' not in version.stdout:
        raise RuntimeError(f'expected rg 15.1.0 with exit 0; got {version.returncode}: {version.stdout!r}')
    evidence = {'limits': {'watcher_events': 256, 'watcher_paths': 4096,
                           'watcher_payload_bytes': 1048576,
                           'request_read_seconds': 5, 'reply_write_seconds': 300,
                            'handlers': 64}, 'files': args.files,
                'identity': {'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                             'wgrep_sha256': hashlib.sha256(Path(wgrep).read_bytes()).hexdigest(),
                             'rg_sha256': hashlib.sha256(Path(rg).read_bytes()).hexdigest(),
                             'rg_version_stdout': version.stdout.decode(errors='replace').strip()}}
    try:
        p = run([wgrep, '--index-build', str(corpus)], env, args.timeout)
        if p.returncode:
            raise RuntimeError(f'index build failed: {p.stderr.decode(errors="replace")}')
        p = run([wgrep, '--start', str(corpus)], env, args.timeout)
        if p.returncode:
            raise RuntimeError(f'daemon start failed: {p.stderr.decode(errors="replace")}')
        # Generate one real inotify event per file. A single bulk write is
        # intentional: the watcher must fall behind rather than an injected RPC.
        for i in range(args.files):
            (corpus / f'burst-{i:05}.txt').write_text(f'needle event-{i}\n')
        deadline = time.monotonic() + args.timeout
        stats = None
        while time.monotonic() < deadline:
            try:
                stats = daemon_stats(store)
            except (OSError, KeyError, json.JSONDecodeError):
                pass
            if stats and stats.get('watcher', {}).get('untrusted'):
                break
            time.sleep(.05)
        evidence['overflow_stats'] = stats
        if not stats or not stats.get('watcher', {}).get('untrusted'):
            raise RuntimeError('live filesystem burst did not produce watcher.untrusted')
        # Protocol query "*" decodes as Query::All; empty prefix covers the root.
        decline = daemon_rpc(store, {'query': '*', 'prefix': ''})
        evidence['query_rpc_decline'] = decline
        if decline.get('ok') is not False or not isinstance(decline.get('reason'), str) or not decline['reason'].strip():
            raise RuntimeError(f'query RPC did not decline with a reason: {decline!r}')
        if decline.get('candidates') or decline.get('paths'):
            raise RuntimeError(f'declined query returned success candidates: {decline!r}')
        events.unlink(missing_ok=True)
        cli_cmd = [wgrep, '--sort', 'path', '--no-messages', 'needle', str(corpus)]
        rg_cmd = [rg, '--sort', 'path', '--no-messages', 'needle', str(corpus)]
        actual = run(cli_cmd, env, args.timeout)
        expected = run(rg_cmd, env, args.timeout)
        evidence['decline_processes'] = {'wgrep': save_process(work, 'decline-wgrep', cli_cmd, actual),
                                        'rg': save_process(work, 'decline-rg', rg_cmd, expected)}
        ev = records(events)
        evidence['decline_events'] = ev
        if not any(x.get('used') == 'walk' and x.get('reason') == 'query_failed' for x in ev):
            raise RuntimeError(f'CLI did not record used:walk reason:query_failed: {ev!r}')
        if not exact_parity(actual, expected):
            raise RuntimeError('stdout/stderr/exit differs from rg 15.1.0')
        evidence['parity'] = {'exact_stdout_stderr_exit': True,
                              'stdout_sha256': hashlib.sha256(actual.stdout).hexdigest(),
                              'stderr_sha256': hashlib.sha256(actual.stderr).hexdigest(),
                              'exit': actual.returncode}
        # Sticky untrusted state must recover only after a fresh daemon process.
        p = run([wgrep, '--stop', str(corpus)], env, args.timeout)
        if p.returncode:
            raise RuntimeError(f'daemon stop failed: {p.stderr.decode(errors="replace")}')
        # Shutdown replies before the daemon process releases its per-root lock.
        stopped = p.stdout.decode(errors='replace')
        import re
        match = re.search(r'\(pid (\d+)\)', stopped)
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            if match is None:
                break
            try:
                os.kill(int(match.group(1)), 0)
            except ProcessLookupError:
                break
            time.sleep(.05)
        else:
            raise RuntimeError('old daemon remained reachable after shutdown')
        p = run([wgrep, '--start', str(corpus)], env, args.timeout)
        if p.returncode:
            raise RuntimeError(f'restart failed: {p.stderr.decode(errors="replace")}')
        evidence['restart_stats'] = daemon_stats(store)
        if not evidence['restart_stats'] or evidence['restart_stats'].get('watcher', {}).get('untrusted'):
            raise RuntimeError(f'fresh daemon stats missing or untrusted: {evidence["restart_stats"]!r}')
        if evidence['restart_stats'].get('watcher', {}).get('events', -1) != 0 or \
           evidence['restart_stats'].get('watcher', {}).get('paths', -1) != 0 or \
           evidence['restart_stats'].get('watcher', {}).get('payload_bytes', -1) != 0:
            raise RuntimeError(f'fresh daemon watcher counters did not reset: {evidence["restart_stats"]!r}')
        events.unlink(missing_ok=True)
        actual = run(cli_cmd, env, args.timeout)
        expected = run(rg_cmd, env, args.timeout)
        evidence['healthy_processes'] = {'wgrep': save_process(work, 'healthy-wgrep', cli_cmd, actual),
                                         'rg': save_process(work, 'healthy-rg', rg_cmd, expected)}
        ev = records(events)
        evidence['healthy_events'] = ev
        if not exact_parity(actual, expected):
            raise RuntimeError('healthy restarted query differs from rg')
        if not any(x.get('used') == 'daemon' for x in ev):
            raise RuntimeError(f'healthy restarted query did not use daemon: {ev!r}')
        evidence['healthy_parity'] = True
        (work / 'result.json').write_text(json.dumps(evidence, indent=2) + '\n')
        print(json.dumps(evidence, indent=2))
        return 0
    except Exception as exc:
        evidence['error'] = str(exc)
        (work / 'result.json').write_text(json.dumps(evidence, indent=2) + '\n')
        print(f'FAIL: {exc}', file=sys.stderr)
        return 1
    finally:
        # Ensure this harness never leaves its isolated daemon behind.
        try:
            run([wgrep, '--stop', str(corpus)], env, 10)
        except subprocess.TimeoutExpired:
            pass


if __name__ == '__main__':
    raise SystemExit(main())
