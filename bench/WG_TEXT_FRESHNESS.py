#!/usr/bin/env python3
"""Freshness of the `wg` daemon text route.

Every check compares ``wg`` with rg 15.1.0 on the same tree with
``-n --sort path --color never``. A change is fresh when the first text query
after the change returns exactly rg's bytes.

Covered changes:

* append, create, same-length overwrite, same-length overwrite with the
  original mtime restored (an index that trusts size+mtime would miss it),
  delete and rename in a watched root;
* add and remove a root ``.gitignore``;
* add and remove an ancestor ``.ignore``;
* add and remove an ancestor ``.gitignore`` in a parent Git repository;
* add and remove a Git ``info/exclude`` entry;
* create, remove and re-point a nested ``.git`` pointer file whose external
  gitdir carries ``info/exclude``.

A first query that disagrees is retried for ``--settle`` seconds. It counts as
``stale_first`` when it converges; a case that never converges is a hard
mismatch. The observable signal (``WGREP_INDEX_EVENTS``) records whether the
query used the daemon candidate path or a fallback walk.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import WG_TEXT_COMMON as common  # noqa: E402


def _load(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


wgtp = _load("wg_text_parity", "WG_TEXT_PARITY.py")
parity = wgtp.parity

NEEDLE = "zzfreshtoken_needle_9182"
EDIT_A = "zzfreshtoken_edit_a_4411"
EDIT_B = "zzfreshtoken_edit_b_5522"
EDIT_C = "zzfreshtoken_edit_c_6633"


class Results:
    def __init__(self) -> None:
        self.checks = 0
        self.matched = 0
        self.stale_first = 0
        self.mismatched = 0
        self.daemon = 0
        self.walk = 0
        self.event_missing = 0
        self.event_count_not_one = 0
        self.reasons: dict[str, int] = {}
        self.records: list[dict] = []
        self.failures: list[dict] = []

    def add(self, record: dict) -> None:
        self.checks += 1
        self.records.append(record)
        if record["matched"]:
            self.matched += 1
        if record.get("stale_first"):
            self.stale_first += 1
        if record.get("hard_mismatch"):
            self.mismatched += 1
            self.failures.append(record)
        used = record.get("used")
        if used == "daemon":
            self.daemon += 1
        elif used == "walk":
            self.walk += 1
            reason = record.get("reason") or "unknown"
            self.reasons[reason] = self.reasons.get(reason, 0) + 1
        else:
            self.event_missing += 1
        if record.get("event_count", 1) != 1:
            self.event_count_not_one += 1

    def summary(self) -> dict:
        return {
            "checks": self.checks,
            "matched": self.matched,
            "stale_first": self.stale_first,
            "mismatched": self.mismatched,
            "daemon": self.daemon,
            "fallback_walk": self.walk,
            "event_missing": self.event_missing,
            "event_count_not_one": self.event_count_not_one,
            "fallback_reasons": self.reasons,
            "failures": self.failures[:50],
        }


def query(wg, rg, svc, root, pattern, base_env, tag, timeout, settle_s):
    args = ["-n", "--sort", "path", "--color", "never", "--", pattern, "."]
    events = common.fresh_event_path(svc.work, tag)
    rg_res = wgtp.run(rg, args, root, base_env, timeout=timeout)
    wg_res = wgtp.run(wg, args, root, svc.env(events), timeout=timeout)
    comparison = wgtp.compare(args, rg_res, wg_res, "sorted")
    event = common.decision_event(events)
    used = event.get("used") if event else None
    reason = event.get("reason") if event else None
    first = {
        "args": args,
        "matched": comparison is None,
        "stale_first": False,
        "hard_mismatch": False,
        "comparison": comparison,
        "used": used,
        "reason": reason,
        "event_count": len(common.search_events(events)),
    }
    if comparison is None:
        return first
    deadline = time.monotonic() + settle_s
    attempts = 1
    last = first
    while time.monotonic() < deadline:
        time.sleep(0.05)
        attempts += 1
        events2 = common.fresh_event_path(svc.work, tag + f"-r{attempts}")
        rg_res = wgtp.run(rg, args, root, base_env, timeout=timeout)
        wg_res = wgtp.run(wg, args, root, svc.env(events2), timeout=timeout)
        comparison2 = wgtp.compare(args, rg_res, wg_res, "sorted")
        event2 = common.decision_event(events2)
        last = {
            "args": args,
            "matched": comparison2 is None,
            "stale_first": False,
            "hard_mismatch": False,
            "comparison": comparison2,
            "used": event2.get("used") if event2 else None,
            "reason": event2.get("reason") if event2 else None,
            "event_count": len(common.search_events(events2)),
        }
        if comparison2 is None:
            last["stale_first"] = True
            last["settle_ms"] = round((time.monotonic() - (deadline - settle_s)) * 1000.0, 1)
            last["attempts"] = attempts
            return last
    last["hard_mismatch"] = True
    last["attempts"] = attempts
    return last


def check(results, wg, rg, svc, root, base_env, label, pattern, timeout, settle_s):
    record = query(wg, rg, svc, root, pattern, base_env, f"{label}-{results.checks}", timeout, settle_s)
    record["label"] = label
    record["root"] = str(root)
    record["pattern"] = pattern
    results.add(record)
    status = "ok" if record["matched"] else ("stale" if record["stale_first"] else "FAIL")
    print(
        f"  {status:5} {label:44} used={record.get('used')} "
        f"reason={record.get('reason')} {record.get('comparison') or ''}",
        flush=True,
    )
    return record


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def attach(svc, root, model, timeout):
    res = common.require(
        common.run(
            [svc.wg, "up", str(root), "--model", model, "--json"],
            cwd=svc.work,
            env=svc.env(),
            timeout=timeout,
            allowed=(0,),
        )
    )
    return json.loads(res["stdout"].decode("utf-8"))


def file_cases(results, wg, rg, svc, root, base_env, timeout, settle_s):
    print("[file mutations]")
    probe_dir = root / "fresh"
    probe_dir.mkdir(parents=True, exist_ok=True)
    check(results, wg, rg, svc, root, base_env, "file/baseline_absent", NEEDLE, timeout, settle_s)

    target = probe_dir / "created.txt"
    write_text(target, f"{NEEDLE} created\n")
    check(results, wg, rg, svc, root, base_env, "file/create", NEEDLE, timeout, settle_s)

    with target.open("a", encoding="utf-8") as stream:
        stream.write(f"{EDIT_A} appended\n")
    check(results, wg, rg, svc, root, base_env, "file/append", EDIT_A, timeout, settle_s)

    old = target.read_bytes()
    same = (b"Q" * len(old))
    atom = EDIT_B.encode()
    if len(same) >= len(atom):
        st = target.stat()
        target.write_bytes(same[: len(same) - len(atom)] + atom)
        os.utime(target, ns=(st.st_atime_ns, st.st_mtime_ns))
    check(results, wg, rg, svc, root, base_env, "file/samelen_restored_mtime", EDIT_B, timeout, settle_s)

    # Remove a word from a file with a same-length rewrite and restore mtime.
    notes = sorted((root / "notes").rglob("*.txt"))
    forest_file = None
    for path in notes:
        if "forest" in path.read_text(encoding="utf-8", errors="replace"):
            forest_file = path
            break
    if forest_file:
        old = forest_file.read_bytes()
        st = forest_file.stat()
        forest_file.write_bytes(b"Z" * len(old))
        os.utime(forest_file, ns=(st.st_atime_ns, st.st_mtime_ns))
        check(results, wg, rg, svc, root, base_env, "file/samelen_remove_match", "forest", timeout, settle_s)

    rename_src = probe_dir / "created.txt"
    rename_dst = probe_dir / "renamed.txt"
    rename_src.rename(rename_dst)
    check(results, wg, rg, svc, root, base_env, "file/rename", EDIT_A, timeout, settle_s)

    rename_dst.unlink()
    check(results, wg, rg, svc, root, base_env, "file/delete", EDIT_A, timeout, settle_s)

    # A racy append immediately followed by a query.
    racy = probe_dir / "racy.txt"
    write_text(racy, "seed line\n")
    check(results, wg, rg, svc, root, base_env, "file/racy_baseline", EDIT_C, timeout, settle_s)
    with racy.open("a", encoding="utf-8") as stream:
        stream.write(f"{EDIT_C} racy\n")
    check(results, wg, rg, svc, root, base_env, "file/racy_append", EDIT_C, timeout, settle_s)


def ignore_case(results, wg, rg, svc, work, base_env, name, timeout, settle_s, model):
    """Add/remove one ignore source outside (or at) a small watched root."""
    parent = work / "ignore" / name / "parent"
    root = parent / "tree"
    if root.exists():
        shutil.rmtree(root.parent)
    root.mkdir(parents=True)
    write_text(root / "keep.txt", f"{NEEDLE} keep\n")
    hidden = root / "hidden.txt"
    write_text(hidden, f"{NEEDLE} hidden\n")
    external = None

    if name == "root_gitignore":
        (root / ".git").mkdir()
        ignore = root / ".gitignore"
        write_text(ignore, "hidden.txt\n")
    elif name == "ancestor_ignore":
        ignore = parent / ".ignore"
        write_text(ignore, "hidden.txt\n")
    elif name == "ancestor_gitignore":
        (parent / ".git").mkdir(exist_ok=True)
        ignore = parent / ".gitignore"
        write_text(ignore, "hidden.txt\n")
    elif name == "git_info_exclude":
        (root / ".git" / "info").mkdir(parents=True)
        ignore = root / ".git" / "info" / "exclude"
        write_text(ignore, "hidden.txt\n")
    elif name == "nested_git_pointer":
        external = work / "ignore" / name / "external-gitdir"
        write_text(external / "info" / "exclude", "hidden.txt\n")
        nested = root / "sub"
        write_text(nested / "hidden.txt", f"{NEEDLE} nested hidden\n")
        write_text(nested / "keep.txt", f"{NEEDLE} nested keep\n")
        ignore = external / "info" / "exclude"
        pointer = nested / ".git"
        write_text(pointer, f"gitdir: {os.path.relpath(external, nested)}\n")
        root = root
    else:
        raise ValueError(name)

    attach(svc, root, model, timeout)
    # Direction 1: the ignore exists, so the hidden file is out of the set.
    check(results, wg, rg, svc, root, base_env, f"ignore/{name}/with_ignore", NEEDLE, timeout, settle_s)
    # Remove it: the hidden file must appear on the next query.
    ignore.unlink()
    check(results, wg, rg, svc, root, base_env, f"ignore/{name}/remove", NEEDLE, timeout, settle_s)
    # Add it back: the file must disappear on the next query.
    write_text(ignore, "hidden.txt\n")
    check(results, wg, rg, svc, root, base_env, f"ignore/{name}/add_back", NEEDLE, timeout, settle_s)

    if name == "nested_git_pointer":
        pointer.unlink()
        check(results, wg, rg, svc, root, base_env, "ignore/nested_git_pointer/pointer_removed", NEEDLE, timeout, settle_s)
        write_text(pointer, f"gitdir: {os.path.relpath(external, root / 'sub')}\n")
        check(results, wg, rg, svc, root, base_env, "ignore/nested_git_pointer/pointer_created", NEEDLE, timeout, settle_s)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--wg", required=False)
    parser.add_argument("--rg", default=common.DEFAULT_RG)
    parser.add_argument("--work", type=Path, default=None)
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("--model", default="code")
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("--settle", type=float, default=3.0)
    parser.add_argument("--skip-ignore", action="store_true")
    args = parser.parse_args()
    if not args.run:
        parser.error("inert unless --run is supplied")
    if not args.wg:
        parser.error("--wg is required")

    rg = shutil.which(args.rg) or (str(Path(args.rg).resolve()) if Path(args.rg).is_file() else None)
    if not rg:
        parser.error(f"rg not found: {args.rg}")
    wg = str(Path(args.wg).resolve())
    if not Path(wg).is_file():
        parser.error(f"wg binary not found: {wg}")

    work = Path(args.work).resolve() if args.work else common.new_work("wg-text-freshness")
    work.mkdir(parents=True, exist_ok=True)
    fixture = work / "fixture"
    if fixture.exists():
        shutil.rmtree(fixture)
    parity.build_fixture(fixture)
    common.git_init(fixture)
    time.sleep(3)

    env = dict(os.environ)
    env.pop("RIPGREP_CONFIG_PATH", None)
    env.pop("WGREP_INDEX_EVENTS", None)

    results = Results()
    svc = common.Service(wg, fixture, work, model=args.model, name="daemon")
    started = time.time()
    stopped = {}
    try:
        print("[setup] wg up on fixture...")
        info = svc.start()
        print(f"[setup] pid={svc.pid} text={info.get('root', {}).get('text') is not None}")

        file_cases(results, wg, rg, svc, fixture, env, args.timeout, args.settle)

        if not args.skip_ignore:
            for name in (
                "root_gitignore",
                "ancestor_ignore",
                "ancestor_gitignore",
                "git_info_exclude",
                "nested_git_pointer",
            ):
                ignore_case(results, wg, rg, svc, work, env, name, args.timeout, args.settle, args.model)
    except Exception as exc:  # noqa: BLE001 - report, do not skip cleanup
        results.mismatched += 1
        results.failures.append({"label": "harness", "error": f"{type(exc).__name__}: {exc}"})
        print(f"[error] {type(exc).__name__}: {exc}", file=sys.stderr)
    finally:
        stopped = svc.stop()
        print(f"[teardown] {stopped}")

    summary = results.summary()
    summary.update(
        {
            "wg": wg,
            "wg_sha256": common.sha256_file(Path(wg)),
            "rg": rg,
            "rg_version": common.require(common.run([rg, "--version"], timeout=30, allowed=(0,)))["stdout"]
            .decode("utf-8", "replace")
            .splitlines()[0],
            "work": str(work),
            "model": args.model,
            "settle_s": args.settle,
            "source": common.source_hash(),
            "machine": common.machine_info(),
            "teardown": stopped,
            "elapsed_s": round(time.time() - started, 1),
            "records": results.records,
        }
    )
    out = args.out or work / "summary.json"
    common.write_json(out, summary)
    print()
    print("=" * 72)
    for key in ("checks", "matched", "stale_first", "mismatched", "daemon", "fallback_walk", "event_missing", "event_count_not_one"):
        print(f"{key + ':':18} {summary[key]}")
    if summary["fallback_reasons"]:
        print(f"{'fallback reasons:':18} {summary['fallback_reasons']}")
    for failure in summary["failures"][:10]:
        print(f"  FAIL {failure}")
    print(f"wrote {out}")
    print("=" * 72)
    return 1 if summary["mismatched"] else 0


if __name__ == "__main__":
    raise SystemExit(main())

