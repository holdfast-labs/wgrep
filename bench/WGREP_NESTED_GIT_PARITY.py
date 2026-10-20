#!/usr/bin/env python3
"""Reproducible sorted-output parity checks for nested Git ignore context.

Every run creates a private corpus and index. The reference is ripgrep 15.1.0.
For each case both programs receive exactly ``-n --sort path PROBE ROOT``.
The JSON report stores the command, raw output bytes (base64), status, and the
index event emitted by wgrep. No captured output is normalized or sorted.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def write(path: Path, data: str | bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data.encode() if isinstance(data, str) else data)


def invoke(argv: list[str], cwd: Path, env: dict[str, str]) -> dict:
    run = subprocess.run(argv, cwd=cwd, env=env, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, check=False)
    return {"argv": argv, "stdout_b64": base64.b64encode(run.stdout).decode(),
            "stderr_b64": base64.b64encode(run.stderr).decode(), "status": run.returncode}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--wgrep", required=True, type=Path)
    parser.add_argument("--rg", required=True, type=Path)
    parser.add_argument("--work", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    wgrep, rg = args.wgrep.resolve(), args.rg.resolve()
    if args.work:
        args.work.mkdir(parents=True, exist_ok=True)
        base = Path(tempfile.mkdtemp(prefix="nested-git-", dir=args.work.resolve()))
    else:
        base = Path(tempfile.mkdtemp(prefix="wgrep-nested-git-"))
    index_dir = base / "private-index"
    repo = base / "fixture" / "tree"
    cwd_a, cwd_b = base / "fixture" / "cwd-a", base / "fixture" / "cwd-b"
    cwd_a.mkdir(parents=True); cwd_b.mkdir(parents=True)
    nested = repo / "nest"
    gitdir = base / "fixture" / "external-gitdir"
    gitdir2 = base / "fixture" / "external-gitdir2"
    write(repo / "top.txt", "PROBE top\n")
    write(nested / "a.txt", "PROBE nested-a\n")
    write(nested / "z.txt", "PROBE nested-z\n")
    write(gitdir / "commondir", "common\n")
    write(gitdir / "common" / "info" / "exclude", "excluded.txt\n")
    write(gitdir / "info" / "exclude", "excluded.txt\n")
    write(gitdir2 / "commondir", "common\n")
    write(gitdir2 / "common" / "info" / "exclude", "a.txt\n")
    write(nested / ".git", f"gitdir: {os.path.relpath(gitdir, nested)}\n")
    write(nested / "excluded.txt", "PROBE excluded\n")
    write(repo / ".gitignore", "")
    env = os.environ.copy()
    env["WGREP_INDEX_DIR"] = str(index_dir)
    env.pop("WGREP_INDEX_EVENTS", None)
    build = subprocess.run([str(wgrep), "--index-build", str(repo)], cwd=cwd_a,
                           env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if build.returncode:
        print(f"index build failed: {build.stderr.decode(errors='replace')}", file=sys.stderr)
        return 2

    cases: list[tuple[str, Path, callable, str]] = []
    def noop() -> None: pass
    cases.append(("initial_nested", cwd_a, noop, "PROBE"))
    def edit_exclude() -> None: write(gitdir / "common" / "info" / "exclude", "z.txt\n")
    cases.append(("same_cwd_exclude_change", cwd_a, edit_exclude, "PROBE"))
    cases.append(("daemon_cwd_a_client_cwd_b", cwd_b, noop, "PROBE"))
    def rewrite_common() -> None: write(gitdir / "commondir", "common2\n"); write(gitdir / "common2" / "info" / "exclude", "a.txt\n")
    cases.append(("commondir_rewrite", cwd_a, rewrite_common, "PROBE"))
    def pointer_rewrite() -> None: write(nested / ".git", f"gitdir: {os.path.relpath(gitdir2, nested)}\n")
    cases.append(("pointer_rewrite", cwd_a, pointer_rewrite, "PROBE"))
    def delete_exclude() -> None:
        (gitdir / "common2" / "info" / "exclude").unlink(missing_ok=True)
    cases.append(("commondir_exclude_delete", cwd_a, delete_exclude, "PROBE"))
    def create_exclude() -> None: write(gitdir / "common2" / "info" / "exclude", "z.txt\n")
    cases.append(("commondir_exclude_create", cwd_a, create_exclude, "PROBE"))
    def delete_commondir() -> None: (gitdir2 / "commondir").unlink(missing_ok=True)
    cases.append(("commondir_delete", cwd_a, delete_commondir, "PROBE"))
    def create_commondir() -> None: write(gitdir2 / "commondir", "common\n")
    cases.append(("commondir_create", cwd_a, create_commondir, "PROBE"))
    def remove_pointer() -> None: (nested / ".git").unlink(missing_ok=True)
    cases.append(("pointer_delete", cwd_a, remove_pointer, "PROBE"))
    def create_pointer() -> None: write(nested / ".git", f"gitdir: {os.path.relpath(gitdir, nested)}\n")
    cases.append(("pointer_create", cwd_a, create_pointer, "PROBE"))
    cases.append(("ordinary_absolute_safe_layout", cwd_a, remove_pointer, "PROBE"))
    cases.append(("absent_pattern_exit1", cwd_a, noop, "ABSENT_NEEDLE"))

    results = []
    for name, cwd, mutate, pattern in cases:
        mutate()
        argv = ["-n", "--sort", "path", pattern, str(repo)]
        ref = invoke([str(rg), *argv], cwd, env)
        wenvironment = env.copy()
        wenvironment.pop("WGREP_INDEX_EVENTS", None)
        # wgrep writes structured events to the path named by this variable.
        event_file = base / f"events-{name}.jsonl"
        wenvironment["WGREP_INDEX_EVENTS"] = str(event_file)
        got = invoke([str(wgrep), *argv], cwd, wenvironment)
        event_lines = event_file.read_text(errors="replace").splitlines() if event_file.exists() else []
        events = []
        for line in event_lines:
            try: events.append(json.loads(line))
            except json.JSONDecodeError: events.append({"raw": line})
        record = {"case": name, "cwd": str(cwd), "argv": argv, "rg": ref,
                  "wgrep": got, "wgrep_index_events": events,
                  "exact": all(ref[k] == got[k] for k in ("stdout_b64", "stderr_b64", "status")),
                  "daemon_or_walk": next((e.get("used") for e in reversed(events) if e.get("event") == "search"), None)}
        results.append(record)
        expected_status = 1 if name == "absent_pattern_exit1" else 0
        if (not record["exact"] or ref["status"] != expected_status
                or got["status"] != expected_status or not events):
            print(f"FAIL {name}: sorted parity differs", file=sys.stderr)
            print(json.dumps(record, indent=2), file=sys.stderr)
            report = {"cases": results, "count": len(results), "passed": False,
                      "wgrep_sha256": sha256(wgrep), "rg_sha256": sha256(rg)}
            target = args.out.resolve() if args.out else base / "report.json"
            target.write_text(json.dumps(report, indent=2) + "\n")
            return 1
    ordinary = next(r for r in results if r["case"] == "ordinary_absolute_safe_layout")
    if ordinary["daemon_or_walk"] != "daemon":
        print("FAIL ordinary_absolute_safe_layout: expected eligible daemon use", file=sys.stderr)
        return 1
    report = {"cases": results, "count": len(results), "passed": True,
              "wgrep_sha256": sha256(wgrep), "rg_sha256": sha256(rg)}
    target = args.out.resolve() if args.out else base / "report.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(report, indent=2) + "\n")
    print(f"PASS: {len(results)} cases; report={target}")
    print("daemon/walk: " + ", ".join(f"{r['case']}={r['daemon_or_walk']}" for r in results))
    print(f"wgrep sha256={report['wgrep_sha256']}\nrg sha256={report['rg_sha256']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
