#!/usr/bin/env python3
"""Differential fuzz for the wgrep daemon's incremental file set (Phase 1).

Random sequences of file operations on a fixture; after each step, the
daemon's candidate set for Query::All (via the daemon protocol, not search
output) must match `rg --files` as a multiset. Unsorted-output order may
legitimately differ for incrementally added files, hence multiset compare.

Also asserts the no-fallback counter: structural ops (create/delete/rename,
including hidden and rule-ignored paths) must cause ZERO full walks; ignore
-rule ops still walk (Phase 2) and are reported, not failed.

Exits nonzero on any candidate mismatch, stuck pending set, or structural
op that caused a full walk.
"""
from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path


def daemon_info(index_dir: Path) -> dict:
    found = list(index_dir.glob("*/daemon.json"))
    assert len(found) == 1, f"expected one daemon.json under {index_dir}, found {found}"
    return json.loads(found[0].read_text())


def daemon_request(info: dict, payload: dict, n_paths: int = 0) -> tuple[dict, list[str]]:
    s = socket.create_connection(("127.0.0.1", info["port"]), timeout=30)
    try:
        s.sendall((json.dumps({"token": info["token"], **payload}) + "\n").encode())
        f = s.makefile("r", encoding="utf-8")
        header = json.loads(f.readline())
        paths = []
        if header.get("ok") and "candidates" in header:
            for _ in range(header["candidates"]):
                line = f.readline()
                if not line:
                    break
                paths.append(line.rstrip("\n"))
        return header, paths
    finally:
        s.close()


def daemon_stats(info: dict) -> dict:
    header, _ = daemon_request(info, {"stats": True})
    assert header.get("ok"), f"stats failed: {header}"
    return header


def daemon_all_candidates(info: dict) -> list[str]:
    header, paths = daemon_request(info, {"query": "*", "prefix": ""})
    assert header.get("ok"), f"query failed: {header}"
    return paths


def rg_files(rg: str, corpus: Path) -> list[str]:
    out = subprocess.run([rg, "--files", str(corpus)], stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, check=True).stdout.decode("utf-8")
    prefix = str(corpus) + os.sep
    rels = []
    for line in out.splitlines():
        if line.startswith(prefix):
            rels.append(line[len(prefix):].replace(os.sep, "/"))
        else:
            rels.append(line)
    return rels


IGNORE_NAMES = {".gitignore", ".ignore", ".rgignore"}


def touches_ignore(path: Path) -> bool:
    """Whether an op on `path` can change ignore rules: the path itself is an
    ignore file, or (for trees) contains one. Such ops still need a full walk
    until Phase 2, so the fuzz reports them instead of failing."""
    if path.name in IGNORE_NAMES:
        return True
    if path.is_dir():
        return any(p.name in IGNORE_NAMES for p in path.rglob("*") if p.is_file())
    return False


def write_file(path: Path, rng: random.Random, token: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    words = ["alpha", "bravo", token, "delta", "echo", "foxtrot"]
    path.write_text("\n".join(rng.choice(words) for _ in range(rng.randint(1, 8))) + "\n")


def build_fixture(corpus: Path, rng: random.Random, token: str) -> None:
    shutil.rmtree(corpus, ignore_errors=True)
    (corpus / ".git").mkdir(parents=True)
    (corpus / ".gitignore").write_text("ignored_dir/\n")
    for d in ["grp0", "grp1/sub", "grp2", "ignored_dir", ".hdir"]:
        (corpus / d).mkdir(parents=True, exist_ok=True)
    for i in range(6):
        write_file(corpus / f"grp0/f{i}.txt", rng, token)
    for i in range(3):
        write_file(corpus / f"grp1/sub/g{i}.txt", rng, token)
    write_file(corpus / "grp2/keep.txt", rng, token)
    write_file(corpus / "ignored_dir/skip.txt", rng, token)
    write_file(corpus / ".hdir/h.txt", rng, token)


def existing_files(corpus: Path) -> list[Path]:
    return [p for p in corpus.rglob("*") if p.is_file() and ".git" not in p.parts]


def existing_dirs(corpus: Path) -> list[Path]:
    return [p for p in corpus.rglob("*") if p.is_dir() and ".git" not in p.parts]


def do_op(op: str, corpus: Path, rng: random.Random, token: str, n: int) -> str:
    """Performs one random op. Returns 'structural' (must not walk) or 'ignore'."""
    files = existing_files(corpus)
    dirs = existing_dirs(corpus)
    if op == "create_file":
        d = rng.choice(dirs + [corpus])
        # Sometimes nest a new directory level.
        if rng.random() < 0.3:
            d = d / f"newdir{n}"
        write_file(d / f"new{n}.txt", rng, token)
        return "structural"
    if op == "create_tree":
        d = corpus / f"tree{n}" / "a" / "b"
        for i in range(rng.randint(1, 4)):
            write_file(d / f"t{i}.txt", rng, token)
        return "structural"
    if op == "delete_file":
        if not files:
            return do_op("create_file", corpus, rng, token, n)
        victim = rng.choice(files)
        kind = "ignore" if touches_ignore(victim) else "structural"
        victim.unlink()
        return kind
    if op == "delete_tree":
        cands = [d for d in dirs if d != corpus and ".git" not in d.parts]
        if not cands:
            return do_op("create_file", corpus, rng, token, n)
        victim = rng.choice(cands)
        kind = "ignore" if touches_ignore(victim) else "structural"
        shutil.rmtree(victim)
        return kind
    if op == "rename_file":
        if not files:
            return do_op("create_file", corpus, rng, token, n)
        src = rng.choice(files)
        dst = src.parent / f"renamed{n}.txt"
        kind = "ignore" if touches_ignore(src) else "structural"
        src.rename(dst)
        return kind
    if op == "rename_dir":
        cands = [d for d in dirs if d != corpus and ".git" not in d.parts]
        if not cands:
            return do_op("create_tree", corpus, rng, token, n)
        src = rng.choice(cands)
        kind = "ignore" if touches_ignore(src) else "structural"
        src.rename(src.parent / f"moved{n}")
        return kind
    if op == "create_and_delete":
        p = corpus / f"tmp{n}.txt"
        write_file(p, rng, token)
        p.unlink()
        return "structural"
    if op == "create_ignored":
        write_file(corpus / "ignored_dir" / f"ign{n}.txt", rng, token)
        return "structural"
    if op == "create_hidden":
        d = corpus / f".hidden{n}"
        d.mkdir(exist_ok=True)
        write_file(d / "h.txt", rng, token)
        return "structural"
    if op == "toggle_gitignore":
        gi = corpus / ".gitignore"
        cur = gi.read_text() if gi.exists() else ""
        if "ignored2/" in cur:
            gi.write_text(cur.replace("ignored2/\n", ""))
        else:
            gi.write_text(cur + "ignored2/\n")
        return "ignore"
    if op == "sub_gitignore":
        gi = corpus / "grp2" / ".gitignore"
        if gi.exists():
            gi.unlink()
        else:
            gi.parent.mkdir(parents=True, exist_ok=True)
            gi.write_text("*.txt\n")
        return "ignore"
    raise AssertionError(op)


OPS = ["create_file"] * 3 + ["create_tree", "delete_file", "delete_tree",
       "rename_file", "rename_dir", "create_and_delete", "create_ignored",
       "create_hidden"]
IGNORE_OPS = ["toggle_gitignore", "sub_gitignore"]


def walks_total(stats: dict) -> int:
    return sum(stats["walks"].values())


def walks_by_cause(stats: dict) -> dict:
    return dict(stats["walks"])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--wgrep", required=True)
    ap.add_argument("--rg", required=True)
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--steps", type=int, default=100)
    ap.add_argument("--seed", type=int, default=20261009)
    ap.add_argument("--settle", type=float, default=0.2,
                    help="seconds to wait after each op for event delivery")
    ap.add_argument("--no-ignore-ops", action="store_true",
                    help="skip ignore-rule ops: isolates structural-op walk "
                         "behavior from rescan cascades under rapid ops")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    token = f"fzztok{rng.randint(0, 1 << 30)}"
    work = args.work
    corpus = work / "corpus"
    index_dir = work / "index"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)
    env = dict(os.environ, WGREP_INDEX_DIR=str(index_dir))

    build_fixture(corpus, rng, token)
    subprocess.run([args.wgrep, "--index-build", str(corpus)], env=env, check=True,
                   stdout=subprocess.DEVNULL)
    subprocess.run([args.wgrep, "--start", str(corpus)], env=env, check=True,
                   stdout=subprocess.DEVNULL)
    failures: list[str] = []
    walk_violations: list[str] = []
    ignore_walks: list[str] = []
    try:
        info = daemon_info(index_dir)

        def check(step: str) -> None:
            got = sorted(daemon_all_candidates(info))
            want = sorted(rg_files(args.rg, corpus))
            if got != want:
                missing = [x for x in want if x not in got][:20]
                extra = [x for x in got if x not in want][:20]
                failures.append(f"{step}: missing={missing} extra={extra}")

        def settled_stats() -> dict:
            for _ in range(100):
                st = daemon_stats(info)
                if st["pending"] == 0:
                    return st
                time.sleep(0.1)
            failures.append("pending set stuck non-empty")
            return daemon_stats(info)

        check("baseline")
        ops = OPS if args.no_ignore_ops else OPS + IGNORE_OPS
        for n in range(args.steps):
            op = rng.choice(ops)
            kind = do_op(op, corpus, rng, token, n)
            time.sleep(args.settle)  # let inotify deliver + the daemon apply
            before = walks_by_cause(settled_stats())
            check(f"step{n}:{op}")
            after = walks_by_cause(daemon_stats(info))
            delta = {k: after[k] - before[k] for k in after}
            total = sum(delta.values())
            if kind == "structural":
                if total > 0:
                    walk_violations.append(f"step{n}:{op} caused {delta}")
            elif total > 0:
                ignore_walks.append(f"step{n}:{op} caused {delta}")
    finally:
        subprocess.run([args.wgrep, "--stop", str(corpus)], env=env, stdout=subprocess.DEVNULL)

    report = {"seed": args.seed, "steps": args.steps, "token": token,
              "mismatches": failures, "walk_violations": walk_violations,
              "ignore_walks": ignore_walks}
    if args.out:
        Path(args.out).write_text(json.dumps(report, indent=1))
    print(f"steps={args.steps} mismatches={len(failures)} "
          f"walk_violations={len(walk_violations)} ignore_walks={len(ignore_walks)}")
    for f in failures[:10]:
        print("MISMATCH", f)
    for f in walk_violations[:10]:
        print("WALK", f)
    for f in ignore_walks[:10]:
        print("IGNORE-WALK", f)
    return 1 if (failures or walk_violations) else 0


if __name__ == "__main__":
    sys.exit(main())
