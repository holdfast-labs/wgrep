#!/usr/bin/env python3
"""Regression: the wgrep daemon must notice ignore-source changes outside its root.

Reproduces and guards the settled-tree omission recorded in
``docs/WGREP_REVIEW_GUIDANCE_2026-10-06.md``. The daemon watches only the root
recursively, but rg's walker also honors:

  * ``.ignore`` / ``.gitignore`` / ``.rgignore`` in ancestor directories,
  * the containing Git repository's ``.git/info/exclude``,
  * the user's global gitignore.

A change to any of these must force a reconcile; otherwise the daemon keeps
serving a stale candidate set and disagrees with an authoritative walk.

Two directions per source:

  * remove the ignore -> a previously ignored file must appear (false negative
    if stale), and
  * add the ignore -> an indexed file must disappear (extra match if stale).

Every search here is expected to be served by the daemon; the harness fails if
the client fell back to the walk, so a pass cannot be vacuous.

Usage:
    python eval/bench/WGREP_ANCESTOR_IGNORE.py --wgrep E:\\wispd-target\\release\\wgrep.exe
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

NEEDLE = "ZZANCESTORNEEDLE"


def run(wgrep: Path, args: list[str], env: dict) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(wgrep), *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        timeout=120,
    )


def events(events_file: Path) -> list[dict]:
    if not events_file.exists():
        return []
    out = []
    for line in events_file.read_text(errors="replace").splitlines():
        line = line.strip()
        if line:
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return out


def last_search_event(events_file: Path) -> dict | None:
    searches = [e for e in events(events_file) if e.get("event") == "search"]
    return searches[-1] if searches else None


def search(wgrep: Path, env: dict, events_file: Path, root: Path) -> tuple[subprocess.CompletedProcess, dict | None]:
    events_file.unlink(missing_ok=True)
    proc = run(wgrep, ["--no-messages", "-l", NEEDLE, str(root)], env)
    return proc, last_search_event(events_file)


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def make_fixture(base: Path, source: str) -> tuple[Path, Path, Path]:
    """Create a root under `parent` with one hidden (matched) and one visible file.

    Returns (parent, root, ignore_file). `source` is one of `.ignore`,
    `.gitignore`, `.rgignore` (each in the parent) or `git-exclude`.
    """
    parent = base / "parent"
    root = parent / "tree"
    root.mkdir(parents=True)
    write(root / "hidden.txt", f"{NEEDLE} present\n")
    write(root / "other.txt", "nothing to see\n")
    if source == "git-exclude":
        write(parent / ".git" / "info" / "exclude", "hidden.txt\n")
        ignore_file = parent / ".git" / "info" / "exclude"
    else:
        if source == ".gitignore":
            # `.gitignore` is only honored inside a Git repository; an empty
            # `.git` directory is enough for rg's walker to enable it.
            (parent / ".git").mkdir()
        ignore_file = parent / source
        write(ignore_file, "hidden.txt\n")
    return parent, root, ignore_file


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--wgrep", required=True, type=Path, help="wgrep.exe under test (built with the fix)")
    ap.add_argument("--work", type=Path, default=None, help="scratch dir off C: (default: a fresh dir under E:)")
    ap.add_argument("--keep", action="store_true", help="keep the scratch dir for inspection")
    args = ap.parse_args()

    if not args.wgrep.is_file():
        print(f"wgrep not found: {args.wgrep}", file=sys.stderr)
        return 2

    scratch = Path("E:/wgrep-worktrees")
    scratch.mkdir(parents=True, exist_ok=True)
    base = args.work or Path(tempfile.mkdtemp(prefix="wgrep-ancestor-ignore-", dir=scratch))
    base.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    env["WGREP_INDEX_DIR"] = str(base / "index")
    env["WGREP_INDEX_EVENTS"] = str(base / "events.ndjson")
    events_file = Path(env["WGREP_INDEX_EVENTS"])
    env.pop("WGREP_NO_DAEMON", None)

    sources = [".ignore", ".gitignore", ".rgignore", "git-exclude"]
    failures: list[str] = []
    stopped: list[Path] = []
    try:
        for source in sources:
            parent, root, ignore_file = make_fixture(base / source.replace(".", "_"), source)
            events_file.unlink(missing_ok=True)

            # Build the index while the ancestor ignore hides hidden.txt.
            build = run(args.wgrep, ["--index-build", str(root)], env)
            if build.returncode != 0:
                failures.append(f"{source}: index build failed: {build.stderr.decode(errors='replace').strip()}")
                continue

            # Direction 1: with the ignore present, the file is absent.
            proc, ev = search(args.wgrep, env, events_file, root)
            if proc.returncode != 1:
                failures.append(f"{source}: expected exit 1 before removing the ignore, got {proc.returncode}")
            if not ev or ev.get("used") != "daemon":
                failures.append(f"{source}: first search not served by the daemon: {ev}")

            # Remove the ancestor ignore; a fresh query must re-walk and find it.
            ignore_file.unlink()
            proc, ev = search(args.wgrep, env, events_file, root)
            text = proc.stdout.decode(errors="replace")
            if proc.returncode != 0 or "hidden.txt" not in text:
                failures.append(
                    f"{source}: expected hidden.txt after removing the ancestor ignore "
                    f"(exit {proc.returncode}, out {text!r})"
                )
            if not ev or ev.get("used") != "daemon":
                failures.append(f"{source}: post-removal search not served by the daemon: {ev}")
            run(args.wgrep, ["--stop", str(root)], env)
            stopped.append(root)

            # Direction 2: an indexed tree that gains an ancestor ignore must lose
            # the file (an extra match would otherwise be reported).
            parent2, root2, ignore_file2 = make_fixture(base / (source.replace(".", "_") + "_add"), source)
            ignore_file2.unlink()
            build = run(args.wgrep, ["--index-build", str(root2)], env)
            if build.returncode != 0:
                failures.append(f"{source} (add): index build failed: {build.stderr.decode(errors='replace').strip()}")
                continue
            proc, ev = search(args.wgrep, env, events_file, root2)
            if proc.returncode != 0 or "hidden.txt" not in proc.stdout.decode(errors="replace"):
                failures.append(f"{source} (add): file not found before the ignore existed (exit {proc.returncode})")
            write(ignore_file2, "hidden.txt\n")
            proc, ev = search(args.wgrep, env, events_file, root2)
            if proc.returncode != 1:
                failures.append(
                    f"{source} (add): expected exit 1 after adding the ancestor ignore, got {proc.returncode}"
                )
            if not ev or ev.get("used") != "daemon":
                failures.append(f"{source} (add): search not served by the daemon: {ev}")
            run(args.wgrep, ["--stop", str(root2)], env)
            stopped.append(root2)
    finally:
        for root in stopped:
            run(args.wgrep, ["--stop", str(root)], env)
        if not args.keep:
            shutil.rmtree(base, ignore_errors=True)

    if failures:
        print(f"FAIL ({len(failures)}):")
        for f in failures:
            print(f"  - {f}")
        return 1
    print(f"PASS: ancestor ignore invalidation holds for {', '.join(sources)} (2 directions each)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
