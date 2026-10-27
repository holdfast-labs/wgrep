#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# ///
"""Measure first-query latency after an idle period, with isolated wg state.

This probe runs one exact-search query after `wg up`, repeats it warm, and runs
one more query after an idle wait. It keeps the service, text index, profile
and event files in a private run directory, so it does not touch another wg
service. Model assets stay in the shared cache and `HF_HUB_OFFLINE=1` blocks
downloads.

It records evidence. It does not claim a cause. The client profile starts at
`args_parsed`, which is after the text candidate request, so the profile does
not split candidate acquisition from wall time. See `limits` in the report.

Run with uv, for example:

    uv run scripts/wg_idle_probe.py --wg /path/to/wg --root /path/to/tree
    uv run scripts/wg_idle_probe.py --wg C:\\path\\wg.exe --mutate

With no `--root`, a small fixture is generated in the run directory.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import datetime
from pathlib import Path

TOOL = "scripts/wg_idle_probe.py"
TOOL_VERSION = "0.1.0"
DEFAULT_PATTERN = "idle_probe_needle"


def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--wg", required=True, help="path to the wg executable")
    ap.add_argument("--root", default=None, help="directory to search (default: generated fixture)")
    ap.add_argument("--work", default=None, help="base directory for the run (default: temp)")
    ap.add_argument("--pattern", default=DEFAULT_PATTERN, help="exact pattern to search")
    ap.add_argument("--regex", action="store_true", help="treat the pattern as a regex (default: fixed string)")
    ap.add_argument("--warm", type=int, default=3, help="immediate warm queries after cold (default: 3)")
    ap.add_argument("--idle", type=float, default=2.0, help="idle wait before the after-idle query (default: 2.0 s)")
    ap.add_argument("--settle", type=float, default=0.2, help="wait for NDJSON writes after each query (default: 0.2 s)")
    ap.add_argument("--timeout", type=float, default=300.0, help="per-command timeout in seconds (default: 300)")
    ap.add_argument("--mutate", action="store_true", help="create one probe file after cold and query it")
    ap.add_argument("--fallback", action="store_true", help="also run one forced full-scan query")
    ap.add_argument("--online", action="store_true", help="allow model downloads (clears HF_HUB_OFFLINE)")
    ap.add_argument("--out", default=None, help="report path (default: <run>/report.json)")
    return ap.parse_args()


def sha256_file(path: Path) -> str | None:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def tail_text(path: Path, limit: int = 2048) -> str:
    try:
        data = path.read_bytes()
    except OSError:
        return ""
    return data[-limit:].decode("utf-8", "replace")


def ndjson_from(path: Path, offset: int) -> list[dict]:
    try:
        with path.open("rb") as stream:
            stream.seek(offset)
            data = stream.read()
    except OSError:
        return []
    records = []
    for line in data.decode("utf-8", "replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            records.append({"torn_line": line[:200]})
            continue
        if isinstance(value, dict):
            records.append(value)
    return records


def file_size(path: Path) -> int:
    try:
        return path.stat().st_size
    except OSError:
        return 0


def invoke(
    argv: list[str],
    env: dict[str, str],
    timeout: float,
    stdout_path: Path,
    stderr_path: Path,
) -> dict:
    started = time.perf_counter_ns()
    with stdout_path.open("wb") as out, stderr_path.open("wb") as err:
        try:
            proc = subprocess.run(
                argv,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=out,
                stderr=err,
                timeout=timeout,
            )
            code = proc.returncode
            timed_out = False
        except subprocess.TimeoutExpired:
            code = None
            timed_out = True
    return {
        "argv": argv,
        "wall_ms": round((time.perf_counter_ns() - started) / 1_000_000.0, 3),
        "exit": code,
        "timed_out": timed_out,
        "stdout_path": str(stdout_path),
        "stderr_path": str(stderr_path),
        "stdout_sha256": sha256_file(stdout_path),
        "stderr_sha256": sha256_file(stderr_path),
        "stderr_tail": tail_text(stderr_path),
    }


def make_fixture(tree: Path, needle: str) -> dict:
    files = 0
    for i in range(24):
        name = f"module_{i:02d}.rs"
        folder = tree / "src" / ("nested" if i % 4 == 0 else "")
        folder.mkdir(parents=True, exist_ok=True)
        lines = [f"// idle probe fixture file {i}"]
        for n in range(36):
            lines.append(f"fn helper_{i}_{n}() -> usize {{ {n} }}")
        if i % 4 == 0:
            lines.append(f'fn {needle}_{i}() -> &\'static str {{ "{needle}" }}')
        (folder / name).write_text("\n".join(lines) + "\n", encoding="utf-8")
        files += 1
    return {"files": files, "needle_files": 6, "settle_s": 2.3}


def base_env(run: Path, service: Path, index: Path, online: bool) -> dict[str, str]:
    env = os.environ.copy()
    env["WG_SERVICE_DIR"] = str(service)
    env["WGREP_INDEX_DIR"] = str(index)
    env["WGREP_PROFILE"] = str(run / "profile.ndjson")
    env["WGREP_INDEX_EVENTS"] = str(run / "index-events.ndjson")
    env["WGREP_INDEX_DEBUG"] = "1"
    for key in (
        "WGREP_NO_INDEX",
        "WGREP_NO_DAEMON",
        "WGREP_INDEX_PROFILE",
        "WG_TEXT_FORCE_UNTRUSTED",
    ):
        env.pop(key, None)
    if online:
        env.pop("HF_HUB_OFFLINE", None)
    else:
        env["HF_HUB_OFFLINE"] = "1"
    return env


def status(wg: str, env: dict[str, str], timeout: float, run: Path, label: str) -> dict:
    result = invoke(
        [wg, "status", "--json"],
        env,
        timeout,
        run / f"{label}.stdout",
        run / f"{label}.stderr",
    )
    parsed = None
    try:
        parsed = json.loads((run / f"{label}.stdout").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        pass
    result["json"] = parsed
    return result


def phase_query(
    label: str,
    argv: list[str],
    env: dict[str, str],
    timeout: float,
    settle: float,
    run: Path,
) -> dict:
    profile = run / "profile.ndjson"
    events = run / "index-events.ndjson"
    profile_offset = file_size(profile)
    events_offset = file_size(events)
    result = invoke(
        argv,
        env,
        timeout,
        run / f"{label}.stdout",
        run / f"{label}.stderr",
    )
    time.sleep(settle)
    result["phase"] = label
    result["profile"] = ndjson_from(profile, profile_offset)
    result["index_events"] = ndjson_from(events, events_offset)
    return result


def search_summary(phase: dict) -> dict:
    events = [e for e in phase.get("index_events", []) if e.get("event") == "search"]
    last = events[-1] if events else {}
    marks = {}
    for rec in phase.get("profile", []):
        if rec.get("proc") == "client" and isinstance(rec.get("marks"), dict):
            marks = rec["marks"]
    total = None
    if "end" in marks:
        first = min(marks.values()) if marks else None
        if first is not None:
            total = round(marks["end"] - first, 3)
    return {
        "used": last.get("used"),
        "reason": last.get("reason"),
        "search_events": len(events),
        "client_profile_total_ms": total,
        "client_marks_ms": marks,
    }


def run_probe(args: argparse.Namespace) -> dict:
    wg = args.wg
    if not Path(wg).is_file() and shutil.which(wg) is None:
        raise SystemExit(f"wg executable not found: {wg}")

    if args.work:
        base = Path(args.work).resolve()
        base.mkdir(parents=True, exist_ok=True)
        run = base / f"run-{datetime.now():%Y%m%d-%H%M%S}-{os.getpid()}"
    else:
        run = Path(tempfile.mkdtemp(prefix="wg-idle-probe-"))
    run.mkdir(parents=True, exist_ok=True)

    service = run / "service"
    index = run / "indexes"
    service.mkdir()
    index.mkdir()

    fixture = None
    if args.root:
        root = Path(args.root).resolve()
        root_kind = "given"
        if not root.is_dir():
            raise SystemExit(f"--root is not a directory: {root}")
    else:
        root = run / "tree"
        root.mkdir()
        fixture = make_fixture(root, args.pattern)
        root_kind = "generated"
        time.sleep(fixture["settle_s"])

    env = base_env(run, service, index, args.online)
    report: dict = {
        "tool": TOOL,
        "tool_version": TOOL_VERSION,
        "generated": datetime.now().isoformat(timespec="seconds"),
        "wg": wg,
        "root": {"path": str(root), "kind": root_kind},
        "fixture": fixture,
        "isolation": {
            "service_dir": str(service),
            "index_dir": str(index),
            "profile": str(run / "profile.ndjson"),
            "index_events": str(run / "index-events.ndjson"),
            "online": args.online,
        },
        "pattern": args.pattern,
        "regex": args.regex,
        "phases": [],
        "limits": [
            "Cold means a fresh isolated service and text index. The OS page cache is not dropped.",
            "The client profile starts at args_parsed, after the text candidate request; it cannot split candidate acquisition from wall time.",
            "The host text route has no service-side answer_ms record. Standalone wgrep daemon records do not apply.",
            "The shared model cache is reused. HF_HUB_OFFLINE=1 blocks downloads unless --online is set.",
            "The probe records evidence. It does not attribute a cause.",
        ],
    }

    argv_query = [wg]
    argv_query.append("-n")
    if not args.regex:
        argv_query.append("-F")
    argv_query.extend(["--", args.pattern, str(root)])

    down_result = None
    mutation_path: Path | None = None
    try:
        version = invoke(
            [wg, "--version"],
            env,
            args.timeout,
            run / "wg-version.stdout",
            run / "wg-version.stderr",
        )
        report["wg_version"] = {
            "exit": version["exit"],
            "stdout": (run / "wg-version.stdout").read_text(encoding="utf-8", errors="replace").strip(),
        }

        report["up"] = invoke(
            [wg, "up", "--json", str(root)],
            env,
            args.timeout,
            run / "up.stdout",
            run / "up.stderr",
        )
        try:
            report["up"]["json"] = json.loads((run / "up.stdout").read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            report["up"]["json"] = None
        report["status_after_up"] = status(wg, env, args.timeout, run, "status-after-up")

        report["phases"].append(
            phase_query("cold", argv_query, env, args.timeout, args.settle, run)
        )

        for n in range(max(0, args.warm)):
            report["phases"].append(
                phase_query(f"warm-{n + 1}", argv_query, env, args.timeout, args.settle, run)
            )

        if args.idle > 0:
            time.sleep(args.idle)
            report["phases"].append(
                phase_query("after-idle", argv_query, env, args.timeout, args.settle, run)
            )

        if args.mutate:
            fd, name = tempfile.mkstemp(prefix="idle_probe_mutation-", suffix=".rs", dir=root)
            mutation_path = Path(name)
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                stream.write(f'fn idle_probe_mutation() -> &\'static str {{ "{args.pattern}" }}\n')
            report["mutation"] = {"path": str(mutation_path), "root_kind": root_kind, "removed": False}
            if root_kind == "given":
                print(
                    f"warning: --mutate writes and removes {mutation_path}",
                    file=sys.stderr,
                )
            time.sleep(args.idle if args.idle > 0 else 0.2)
            report["phases"].append(
                phase_query("after-mutation", argv_query, env, args.timeout, args.settle, run)
            )

        if args.fallback:
            fallback_env = dict(env)
            fallback_env["WGREP_NO_INDEX"] = "1"
            report["phases"].append(
                phase_query("forced-fallback", argv_query, fallback_env, args.timeout, args.settle, run)
            )

        report["status_after"] = status(wg, env, args.timeout, run, "status-after")
    finally:
        down_result = invoke(
            [wg, "down"],
            env,
            min(args.timeout, 60.0),
            run / "down.stdout",
            run / "down.stderr",
        )
        if mutation_path is not None:
            try:
                mutation_path.unlink()
                report["mutation"]["removed"] = True
            except OSError as error:
                report["mutation"]["remove_error"] = str(error)
    report["down"] = down_result

    for phase in report["phases"]:
        phase["summary"] = search_summary(phase)
    warm = [p["wall_ms"] for p in report["phases"] if p["phase"].startswith("warm-")]
    report["timing_ms"] = {
        "cold": report["phases"][0]["wall_ms"] if report["phases"] else None,
        "warm_median": round(statistics.median(warm), 3) if warm else None,
        "warm_min": min(warm) if warm else None,
        "warm_max": max(warm) if warm else None,
        "after_idle": next(
            (p["wall_ms"] for p in report["phases"] if p["phase"] == "after-idle"), None
        ),
    }
    return {"run": run, "report": report}


def print_table(report: dict) -> None:
    print(f"{'phase':<16}{'wall_ms':>10}{'exit':>6}{'used':>9}{'reason':>24}{'profile_ms':>12}")
    for phase in report["phases"]:
        summary = phase.get("summary", {})
        print(
            f"{phase['phase']:<16}{phase['wall_ms']:>10.1f}{str(phase['exit']):>6}"
            f"{str(summary.get('used')):>9}{str(summary.get('reason')):>24}"
            f"{str(summary.get('client_profile_total_ms')):>12}"
        )
    timing = report.get("timing_ms", {})
    print(
        f"cold={timing.get('cold')} ms "
        f"warm_median={timing.get('warm_median')} ms "
        f"after_idle={timing.get('after_idle')} ms"
    )


def main() -> int:
    args = parse_args()
    result = run_probe(args)
    run = result["run"]
    report = result["report"]
    out = Path(args.out).resolve() if args.out else run / "report.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print_table(report)
    print(f"Report: {out}")
    print(f"Evidence: {run}")
    management_ok = all(
        report[key]["exit"] == 0
        for key in ("up", "status_after_up", "status_after", "down")
    )
    return 0 if management_ok and all(p["exit"] in (0, 1) for p in report["phases"]) else 2


if __name__ == "__main__":
    sys.exit(main())
