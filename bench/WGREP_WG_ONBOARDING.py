#!/usr/bin/env python3
"""Focused wg onboarding, two-repository isolation, and live-update check.

Latency is measured from a completed filesystem operation to the first correct
CLI answer. The fresh sgrep comparison runs afterwards, outside that interval.
This small functional fixture is not a large-corpus performance benchmark.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading
import time


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--wg', type=Path, required=True)
    ap.add_argument('--sgrep', type=Path, required=True)
    ap.add_argument('--sgrep-sha256', default='715946fa0f17006905b5f0d0bbf1f21c00e355882855ec40c44c14e78a3c6217')
    ap.add_argument('--work', type=Path, required=True)
    ap.add_argument('--store-env', default='WG_SERVICE_DIR')
    ap.add_argument('--timeout', type=float, default=120)
    ap.add_argument('--visibility-timeout', type=float, default=10)
    ap.add_argument('--background-files', type=int, default=128)
    args = ap.parse_args()
    work = args.work.resolve()
    if work.exists() and any(work.iterdir()):
        ap.error('work must be empty')
    work.mkdir(parents=True, exist_ok=True)
    wg, control = args.wg.resolve(), args.sgrep.resolve()
    if sha(control) != args.sgrep_sha256:
        ap.error('sgrep control hash does not match the supplied pin')
    env = dict(os.environ, HF_HUB_OFFLINE='1')
    env[args.store_env] = str(work / 'service')
    env.pop('RIPGREP_CONFIG_PATH', None)
    report = {'wg_sha256': sha(wg), 'sgrep_sha256': sha(control),
              'harness_sha256': sha(__file__), 'store_env': args.store_env,
              'background_files': args.background_files,
              'cases': [], 'latency_scope': 'settled filesystem change to correct CLI response'}
    serial = 0
    serial_lock = threading.Lock()

    def run(argv, cwd=None):
        nonlocal serial
        with serial_lock:
            serial += 1
            case_number = serial
        start = time.monotonic()
        p = subprocess.run([str(x) for x in argv], env=env, cwd=cwd,
                           stdin=subprocess.DEVNULL, capture_output=True,
                           timeout=args.timeout)
        stem = work / f'{case_number:03d}'
        stem.with_suffix('.stdout').write_bytes(p.stdout)
        stem.with_suffix('.stderr').write_bytes(p.stderr)
        row = {'argv': [str(x) for x in argv], 'cwd': str(cwd) if cwd else None,
               'exit': p.returncode, 'elapsed_ms': (time.monotonic() - start) * 1000,
               'stdout_file': str(stem.with_suffix('.stdout')),
               'stderr_file': str(stem.with_suffix('.stderr'))}
        report['cases'].append(row)
        return p

    def checked(argv, cwd=None):
        p = run(argv, cwd)
        if p.returncode:
            raise RuntimeError(f'command failed ({p.returncode}): {argv}: {p.stderr!r}')
        return p

    def status():
        return json.loads(checked([wg, 'status', '--json']).stdout)

    def query(root, text, cwd=None, explicit=True):
        argv = [wg, text]
        if explicit:
            argv.append(root)
        argv += ['--model', 'code', '--json', '--explain', '--top', '5']
        p = run(argv, cwd)
        if p.returncode not in (0, 1):
            return p, None
        return p, json.loads(p.stdout)

    def compare(root, text):
        p, actual = query(root, text)
        expected = run([control, text, root, '--model', 'code', '--json',
                        '--explain', '--no-cache', '-n', '5'])
        if expected.returncode not in (0, 1):
            raise RuntimeError(f'fresh sgrep failed: {expected.stderr!r}')
        if p.returncode != expected.returncode or actual != json.loads(expected.stdout):
            raise RuntimeError(f'fresh sgrep result/score/order mismatch for {text!r}')

    def visible(root, text, predicate, mutate, label):
        mutate()
        start = time.monotonic()
        deadline = start + args.visibility_timeout
        last = None
        while time.monotonic() < deadline:
            p, rows = query(root, text)
            last = {'exit': p.returncode, 'rows': rows}
            if rows is not None and predicate(rows):
                elapsed = (time.monotonic() - start) * 1000
                report.setdefault('visibility_ms', {})[label] = elapsed
                compare(root, text)
                return
            time.sleep(.01)
        raise RuntimeError(f'{label}: not visible before deadline: {last!r}')

    a, b = work / 'repo-a', work / 'repo-b'
    a.mkdir()
    b.mkdir()
    (a / 'sub').mkdir()
    (a / 'sub' / 'alpha.rs').write_text('fn AuroraOldV17() { println!("alpha ledger decoder"); }\n')
    (b / 'beta.rs').write_text('fn BorealisUniqueV29() { println!("beta queue scheduler"); }\n')
    for i in range(args.background_files):
        (a / f'background-{i:04}.rs').write_text(
            f'fn background_{i:04}() {{ println!("ordinary helper number {i}"); }}\n')
    started = False
    try:
        checked([wg, '--version'])
        started = True  # Cleanup also covers a partial startup failure.
        with ThreadPoolExecutor(max_workers=2) as pool:
            ups = [pool.submit(checked, [wg, 'up', a, '--model', 'code']) for _ in range(2)]
            for up in ups:
                up.result()
        first = status()
        pid = first['pid']
        checked([wg, 'up', a, '--model', 'code'])
        if status()['pid'] != pid:
            raise RuntimeError('repeated up changed daemon PID')
        checked([wg, 'up', b, '--model', 'code'])
        two = status()
        report['status_after_two_roots'] = two
        if two['pid'] != pid or len(two['roots']) != 2:
            raise RuntimeError('two repositories did not share one daemon')
        loads = two.get('model_loads', two.get('loaded_models'))
        if loads != 1:
            raise RuntimeError(f'code model was not loaded exactly once: {loads!r}')
        compare(a, 'AuroraOldV17')
        compare(b, 'BorealisUniqueV29')
        _, rows = query(a, 'AuroraOldV17', cwd=a / 'sub', explicit=False)
        if not rows or any('beta.rs' in c['path'] for c in rows):
            raise RuntimeError('subdirectory query did not select its attached repository')
        alpha = a / 'sub' / 'alpha.rs'
        visible(a, 'AuroraNewV18',
                lambda rows: any('AuroraNewV18' in c['content'] for c in rows)
                and all('AuroraOldV17' not in c['content'] for c in rows),
                lambda: alpha.write_text('fn AuroraNewV18() { println!("alpha ledger decoder"); }\n'), 'edit')
        added = a / 'created.rs'
        visible(a, 'CelestialAddedV31',
                lambda rows: any(c['path'] == 'created.rs' and 'CelestialAddedV31' in c['content'] for c in rows),
                lambda: added.write_text('fn CelestialAddedV31() { println!("new packet codec"); }\n'), 'create')
        renamed = a / 'renamed.rs'
        visible(a, 'CelestialAddedV31',
                lambda rows: any(c['path'] == 'renamed.rs' for c in rows)
                and all(c['path'] != 'created.rs' for c in rows),
                lambda: added.rename(renamed), 'rename')
        visible(a, 'CelestialAddedV31',
                lambda rows: all(c['path'] != 'renamed.rs' for c in rows),
                renamed.unlink, 'delete')
        compare(b, 'BorealisUniqueV29')
        if status()['pid'] != pid:
            raise RuntimeError('live updates replaced the shared daemon')
        report['result'] = 'PASS'
    except Exception as exc:
        report['result'] = 'FAIL'
        report['error'] = str(exc)
    finally:
        if started:
            try:
                stopped = run([wg, 'down'])
                if stopped.returncode:
                    report['result'] = 'FAIL'
                    report['cleanup_error'] = stopped.stderr.decode(errors='replace')
            except Exception as exc:
                report['result'] = 'FAIL'
                report['cleanup_error'] = str(exc)
        (work / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))
    return 0 if report['result'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
