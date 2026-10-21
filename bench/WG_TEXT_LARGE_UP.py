#!/usr/bin/env python3
"""Large-tree `wg up`: does the combined semantic + text build finish?

`wg up` builds both indexes inside one request. The client frame timeout is a
fixed bound (90 s at the time of this run), so a large tree can make the client
return an error while the daemon keeps building. This harness records:

* whether `wg up` returned success, an error, or hit the client timeout;
* the wall time to that return;
* whether the root later became ready (polled with `wg status --json`);
* peak daemon ``VmRSS``/Pss while it builds;
* the final text index fields;
* the daemon log tail on failure.

Every subprocess has a timeout. Each corpus gets isolated `WG_SERVICE_DIR` and
`WGREP_INDEX_DIR`. Daemons are stopped with `wg down` and killed by PID if
needed.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import WG_TEXT_COMMON as common  # noqa: E402

DEFAULT_CORPORA = [
    "/srv/workspaces/wispd/corpora/linux-7.2.9",
    "/srv/workspaces/wispd/prof/src",
]


def tree_stats(root: Path) -> dict:
    files = 0
    total = 0
    for path in root.rglob("*"):
        if path.is_file():
            files += 1
            try:
                total += path.stat().st_size
            except OSError:
                pass
    return {"files": files, "bytes": total}


class Sampler(threading.Thread):
    def __init__(self, service_dir: Path):
        super().__init__(daemon=True)
        self.service_dir = service_dir
        self.stop_event = threading.Event()
        self.peak_rss_kb = 0
        self.peak_pss_kb = 0
        self.pid: int | None = None
        self.samples = 0

    def _pid(self) -> int | None:
        if self.pid and common.process_alive(self.pid):
            return self.pid
        try:
            value = json.loads((self.service_dir / "service.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
        pid = value.get("pid")
        if isinstance(pid, int):
            self.pid = pid
            return pid
        return None

    def run(self) -> None:
        while not self.stop_event.is_set():
            pid = self._pid()
            if pid:
                rss = common.rss_kb(pid)
                pss = common.smaps_kb(pid, "Pss")
                if rss:
                    self.peak_rss_kb = max(self.peak_rss_kb, rss)
                if pss:
                    self.peak_pss_kb = max(self.peak_pss_kb, pss)
                self.samples += 1
            time.sleep(0.1)

    def stop(self) -> None:
        self.stop_event.set()


def daemon_pid(service_dir: Path) -> int | None:
    try:
        value = json.loads((service_dir / "service.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    pid = value.get("pid")
    return int(pid) if isinstance(pid, int) else None


def stop(wg: str, env: dict, service_dir: Path, pid: int | None) -> dict:
    out = {"down": None, "killed": False, "alive_after": False}
    try:
        down = common.run([wg, "down"], env=env, timeout=120, allowed=(0, 1))
        out["down"] = {"code": down["code"], "timed_out": down["timed_out"]}
    except Exception as exc:  # noqa: BLE001
        out["down"] = {"error": str(exc)}
    pid = pid or daemon_pid(service_dir)
    deadline = time.monotonic() + 30
    while pid and time.monotonic() < deadline and common.process_alive(pid):
        time.sleep(0.1)
    if pid and common.process_alive(pid):
        try:
            os.kill(pid, signal.SIGKILL)
            out["killed"] = True
        except OSError:
            pass
    if pid:
        out["alive_after"] = common.process_alive(pid)
    return out


def status_snapshot(wg: str, env: dict, timeout: float) -> dict | None:
    res = common.run([wg, "status", "--json"], env=env, timeout=timeout, allowed=(0,))
    if res["unexpected"]:
        return None
    try:
        return json.loads(res["stdout"].decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        return None


def run_one(wg: str, corpus: Path, work: Path, args) -> dict:
    name = corpus.name
    service_dir = work / f"svc-{name}"
    index_dir = work / f"idx-{name}"
    service_dir.mkdir(parents=True, exist_ok=True)
    index_dir.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    env["WG_SERVICE_DIR"] = str(service_dir)
    env["WGREP_INDEX_DIR"] = str(index_dir)
    env.pop("WGREP_INDEX_EVENTS", None)
    env.pop("RIPGREP_CONFIG_PATH", None)

    record = {
        "corpus": str(corpus),
        "name": name,
        "tree": tree_stats(corpus),
        "up_timeout_s": args.timeout,
        "ready_timeout_s": args.ready_timeout,
        "service_dir": str(service_dir),
        "index_dir": str(index_dir),
    }
    sampler = Sampler(service_dir)
    sampler.start()
    proc = None
    try:
        started = time.perf_counter()
        proc = subprocess.Popen(
            [wg, "up", str(corpus), "--model", args.model, "--json"],
            cwd=str(work),
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            out, err = proc.communicate(timeout=args.timeout)
            record["up_timed_out"] = False
        except subprocess.TimeoutExpired:
            proc.kill()
            out, err = proc.communicate(timeout=60)
            record["up_timed_out"] = True
        record["up_elapsed_s"] = round(time.perf_counter() - started, 3)
        record["up_code"] = proc.returncode
        record["up_stdout"] = out.decode("utf-8", "replace")[:2000]
        record["up_stderr"] = err.decode("utf-8", "replace")[:2000]
        record["daemon_pid"] = daemon_pid(service_dir)
        record["daemon_alive_after_up"] = bool(
            record["daemon_pid"] and common.process_alive(record["daemon_pid"])
        )

        # Poll until the root appears (attach completed) or the deadline.
        poll_started = time.perf_counter()
        last = None
        ready = False
        while time.perf_counter() - poll_started < args.ready_timeout:
            snap = status_snapshot(wg, env, args.status_timeout)
            if snap and snap.get("roots"):
                last = snap
                root = snap["roots"][0]
                text = root.get("text")
                if root.get("ready") and (isinstance(text, dict) or root.get("text_error")):
                    ready = True
                    break
            if record["daemon_pid"] and not common.process_alive(record["daemon_pid"]):
                break
            time.sleep(5.0)
        record["ready"] = ready
        record["ready_elapsed_s"] = round(time.perf_counter() - poll_started, 3)
        record["status"] = last
        if last and last.get("roots"):
            root = last["roots"][0]
            text = root.get("text")
            record["root_files"] = root.get("files")
            record["root_chunks"] = root.get("chunks")
            record["root_ready"] = root.get("ready")
            record["text_files"] = text.get("files") if isinstance(text, dict) else None
            record["text_build_ms"] = root.get("text_build_ms")
            record["text_disk_bytes"] = text.get("disk_bytes") if isinstance(text, dict) else None
            record["text_resident_bytes"] = text.get("resident_bytes") if isinstance(text, dict) else None
            record["text_error"] = root.get("text_error")
    except Exception as exc:  # noqa: BLE001
        record["error"] = f"{type(exc).__name__}: {exc}"
    finally:
        sampler.stop()
        sampler.join(timeout=2)
        record["peak_rss_mib"] = round(sampler.peak_rss_kb / 1024.0, 1) if sampler.peak_rss_kb else None
        record["peak_pss_mib"] = round(sampler.peak_pss_kb / 1024.0, 1) if sampler.peak_pss_kb else None
        record["rss_samples"] = sampler.samples
        record["teardown"] = stop(wg, env, service_dir, record.get("daemon_pid"))
        log = service_dir / "daemon.log"
        if log.is_file():
            try:
                record["daemon_log_tail"] = log.read_text(encoding="utf-8", errors="replace")[-2000:]
            except OSError:
                pass
    return record


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--wg", required=False)
    parser.add_argument("--corpus", action="append", default=None, help="repeatable; defaults to Linux + Rust")
    parser.add_argument("--work", type=Path, default=None)
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("--model", default="code")
    parser.add_argument("--timeout", type=float, default=300.0, help="client wait for wg up")
    parser.add_argument("--ready-timeout", type=float, default=1800.0)
    parser.add_argument("--status-timeout", type=float, default=120.0)
    args = parser.parse_args()
    if not args.run:
        parser.error("inert unless --run is supplied")
    if not args.wg:
        parser.error("--wg is required")
    wg = str(Path(args.wg).resolve())
    if not Path(wg).is_file():
        parser.error(f"wg binary not found: {wg}")
    corpora = [Path(p) for p in (args.corpus or DEFAULT_CORPORA)]
    for corpus in corpora:
        if not corpus.is_dir():
            parser.error(f"corpus not found: {corpus}")

    work = Path(args.work).resolve() if args.work else common.new_work("wg-text-large-up")
    work.mkdir(parents=True, exist_ok=True)
    started = time.time()
    print(f"[setup] wg={wg}")
    print(f"[setup] work={work}")
    rows = []
    for corpus in corpora:
        print(f"[run] {corpus}")
        row = run_one(wg, corpus, work, args)
        rows.append(row)
        print(
            f"  up_code={row.get('up_code')} timed_out={row.get('up_timed_out')} "
            f"elapsed={row.get('up_elapsed_s')}s ready={row.get('ready')} "
            f"ready_after={row.get('ready_elapsed_s')}s "
            f"peak_rss={row.get('peak_rss_mib')}MiB files={row.get('text_files')} "
            f"text_error={row.get('text_error')}"
        )

    summary = {
        "wg": wg,
        "wg_sha256": common.sha256_file(Path(wg)),
        "work": str(work),
        "source": common.source_hash(),
        "machine": common.machine_info(),
        "model": args.model,
        "rows": rows,
        "elapsed_s": round(time.time() - started, 1),
    }
    out = args.out or work / "summary.json"
    common.write_json(out, summary)
    print(f"wrote {out}")
    bad = any(
        row.get("up_timed_out")
        or row.get("up_code") != 0
        or not row.get("ready")
        or row.get("text_error")
        or row.get("error")
        for row in rows
    )
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main())
