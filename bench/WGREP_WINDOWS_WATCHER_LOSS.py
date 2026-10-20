#!/usr/bin/env python3
"""Qualify Windows watcher loss after real removal of its watched root.

This tests the Windows root-loss/error path. It does not test kernel overflow.
All filesystem changes stay inside the required isolated --work directory.
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


def run(argv, env, timeout):
    return subprocess.run(argv, env=env, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          timeout=timeout)


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


def daemon_rpc(store, request):
    infos = list(store.rglob('daemon.json'))
    if len(infos) != 1:
        raise RuntimeError(f'expected one daemon.json, found {len(infos)}')
    info = json.loads(infos[0].read_text())
    with socket.create_connection(('127.0.0.1', info['port']), timeout=3) as sock:
        sock.settimeout(5)
        sock.sendall((json.dumps(dict(request, token=info['token'])) + '\n').encode())
        with sock.makefile('rb') as stream:
            line = stream.readline()
            if not line.endswith(b'\n'):
                raise RuntimeError('incomplete RPC reply')
            return json.loads(line)


def exact(a, b):
    return (a.stdout, a.stderr, a.returncode) == (b.stdout, b.stderr, b.returncode)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--wgrep', required=True)
    ap.add_argument('--rg', required=True)
    ap.add_argument('--work', type=Path, required=True)
    ap.add_argument('--timeout', type=float, default=20)
    args = ap.parse_args()
    if sys.platform != 'win32':
        ap.error('this live watcher-loss test requires native Windows')
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
    env.pop('RIPGREP_CONFIG_PATH', None)
    wgrep, rg = str(Path(args.wgrep).resolve()), str(Path(args.rg).resolve())
    evidence = {'approach': 'remove watched root after removing fixture files, recreate it with a match',
                'claim_scope': 'Windows root-loss/error path only; not kernel overflow',
                'identity': {'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                             'wgrep_path': wgrep, 'wgrep_sha256': hashlib.sha256(Path(wgrep).read_bytes()).hexdigest(),
                             'rg_path': rg, 'rg_sha256': hashlib.sha256(Path(rg).read_bytes()).hexdigest()}}
    started = False
    try:
        version = run([rg, '--version'], env, args.timeout)
        evidence['rg_version'] = save_process(work, 'rg-version', [rg, '--version'], version)
        if version.returncode or b'ripgrep 15.1.0' not in version.stdout:
            raise RuntimeError('reference binary is not ripgrep 15.1.0')
        p = run([wgrep, '--index-build', str(corpus)], env, args.timeout)
        evidence['index_build'] = save_process(work, 'index-build', [wgrep, '--index-build', str(corpus)], p)
        if p.returncode:
            raise RuntimeError('index build failed')
        start_cmd = [wgrep, '--start', str(corpus)]
        p = run(start_cmd, env, args.timeout)
        evidence['start'] = save_process(work, 'start', start_cmd, p)
        if p.returncode:
            raise RuntimeError('daemon start failed')
        started = True
        # Remove fixture entries first, then remove the watched root itself.
        for child in corpus.iterdir():
            child.unlink()
        corpus.rmdir()
        evidence['root_removed'] = True
        deadline = time.monotonic() + args.timeout
        stats = None
        while time.monotonic() < deadline:
            try:
                stats = daemon_rpc(store, {'stats': True})
            except (OSError, RuntimeError, KeyError, json.JSONDecodeError):
                pass
            if stats and stats.get('watcher', {}).get('untrusted'):
                break
            time.sleep(.1)
        evidence['loss_stats'] = stats
        if not stats or not stats.get('watcher', {}).get('untrusted'):
            raise RuntimeError('root removal did not produce watcher.untrusted before deadline')
        corpus.mkdir()
        (corpus / 'fresh.txt').write_text('needle recreated-root-match\n')
        evidence['root_recreated_with_match'] = True
        decline = daemon_rpc(store, {'query': '*', 'prefix': ''})
        evidence['query_rpc_decline'] = decline
        if decline.get('ok') is not False or not isinstance(decline.get('reason'), str) or not decline['reason'].strip():
            raise RuntimeError(f'query RPC did not decline with a reason: {decline!r}')
        if decline.get('candidates') or decline.get('paths'):
            raise RuntimeError(f'declined RPC returned candidates: {decline!r}')
        cli = [wgrep, '--sort', 'path', '--no-messages', 'needle', str(corpus)]
        ref = [rg, '--sort', 'path', '--no-messages', 'needle', str(corpus)]
        actual, expected = run(cli, env, args.timeout), run(ref, env, args.timeout)
        evidence['decline_processes'] = {'wgrep': save_process(work, 'decline-wgrep', cli, actual),
                                         'rg': save_process(work, 'decline-rg', ref, expected)}
        evidence['decline_events'] = [json.loads(x) for x in events.read_text().splitlines() if x.strip()] if events.exists() else []
        if not any(x.get('used') == 'walk' and x.get('reason') == 'query_failed' for x in evidence['decline_events']):
            raise RuntimeError('CLI did not record walk fallback for query_failed')
        if expected.returncode != 0 or not exact(actual, expected):
            raise RuntimeError('fallback stdout/stderr/exit differs from rg 15.1.0')
        evidence['fallback_parity'] = True
        stop_cmd = [wgrep, '--stop', str(corpus)]
        p = run(stop_cmd, env, args.timeout)
        evidence['stop'] = save_process(work, 'stop', stop_cmd, p)
        if p.returncode:
            raise RuntimeError('daemon stop failed')
        # The daemon's successful --stop reply is the shutdown acknowledgement.
        # Windows os.kill(pid, 0) does not provide a reliable liveness probe.
        started = False
        p = run(start_cmd, env, args.timeout)
        evidence['restart'] = save_process(work, 'restart', start_cmd, p)
        if p.returncode:
            raise RuntimeError('fresh daemon restart failed')
        started = True
        healthy = daemon_rpc(store, {'stats': True})
        evidence['healthy_stats'] = healthy
        if healthy.get('watcher', {}).get('untrusted') or healthy.get('watcher', {}).get('events') != 0 or healthy.get('watcher', {}).get('paths') != 0 or healthy.get('watcher', {}).get('payload_bytes') != 0:
            raise RuntimeError('fresh daemon health/counters did not reset')
        events.unlink(missing_ok=True)
        actual, expected = run(cli, env, args.timeout), run(ref, env, args.timeout)
        evidence['healthy_processes'] = {'wgrep': save_process(work, 'healthy-wgrep', cli, actual),
                                         'rg': save_process(work, 'healthy-rg', ref, expected)}
        evidence['healthy_events'] = [json.loads(x) for x in events.read_text().splitlines() if x.strip()] if events.exists() else []
        if expected.returncode != 0 or not exact(actual, expected) or not any(x.get('used') == 'daemon' for x in evidence['healthy_events']):
            raise RuntimeError('healthy restarted query did not use daemon with exact rg parity')
        evidence['healthy_parity'] = True
        evidence['result'] = 'PASS'
        return_code = 0
    except Exception as exc:
        evidence['result'] = 'FAIL'
        evidence['error'] = str(exc)
        return_code = 1
    finally:
        if started:
            try:
                stopped = run([wgrep, '--stop', str(corpus)], env, min(args.timeout, 10))
                evidence['final_stop'] = save_process(work, 'final-stop', [wgrep, '--stop', str(corpus)], stopped)
                if stopped.returncode != 0:
                    evidence['result'] = 'FAIL'
                    evidence['final_stop_error'] = 'daemon stop returned a nonzero exit status'
                    return_code = 1
            except Exception as exc:
                evidence['final_stop_error'] = repr(exc)
                evidence['result'] = 'FAIL'
                return_code = 1
        (work / 'result.json').write_text(json.dumps(evidence, indent=2) + '\n')
    print(json.dumps(evidence, indent=2))
    return return_code


if __name__ == '__main__':
    raise SystemExit(main())
