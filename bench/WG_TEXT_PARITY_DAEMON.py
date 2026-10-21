#!/usr/bin/env python3
"""Daemon-mode parity for `wg`'s text route against rg 15.1.0.

This imports ``WG_TEXT_PARITY.py`` (which imports ``WGREP_PARITY.py``) and
reuses its edge-case fixture, its seeded pattern matrix and its freshness
mutations. Each applicable argv runs twice:

* the reference ``rg`` binary, and
* ``wg`` attached to the fixture with ``wg up``.

For every ``wg`` run the NDJSON file named by ``WGREP_INDEX_EVENTS`` records
whether the daemon candidate path was used (``used == "daemon"``) or the engine
fell back to a full walk (``used == "walk"``), plus a fallback reason. Output
bytes, stderr and exit code are compared exactly, as in ``WG_TEXT_PARITY.py``.

A first query after a freshness mutation is retried for a short settle window.
``stale_immediate`` counts cases whose first query disagreed but converged;
these are reported separately from hard mismatches so a timing race is not
hidden, but also not confused with a wrong answer.
"""
from __future__ import annotations

import argparse
import collections
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

# `WG_TEXT_PARITY.selects_text_route` returns False as soon as it sees `--`,
# but `lexical::classify` treats `--` as a separator and keeps scanning. Every
# matrix case contains `--`, so the imported predicate would skip all of them.
_WG_BOOL = {"--json", "--explain", "-h", "--help", "-V", "--version"}
_WG_VALUE = {"-k", "--top", "--model", "--cache-dir"}


def text_route(args: list[str]) -> bool:
    """Mirror `lexical::classify`: any non-wg flag selects ripgrep."""
    i = 0
    while i < len(args):
        arg = args[i]
        if arg == "--":
            # Everything after this is positional, and every non-wg flag
            # before it already returned True.
            return False
        if arg == "-" or not arg.startswith("-"):
            i += 1
            continue
        name = arg.split("=", 1)[0]
        if name in _WG_BOOL:
            i += 1
            continue
        if name in _WG_VALUE or (len(arg) > 2 and arg[:2] == "-k" and (arg[2] == "=" or arg[2].isdigit())):
            inline = "=" in arg
            i += 1 if inline or (len(arg) > 2 and arg[:2] == "-k" and arg[2].isdigit()) else 2
            continue
        return True
    return False


class Counts:
    def __init__(self) -> None:
        self.applicable = 0
        self.daemon = 0
        self.walk = 0
        self.mismatched = 0
        self.inconclusive = 0
        self.stale_immediate = 0
        self.event_missing = 0
        self.event_count_not_one = 0
        self.errors = 0
        self.reasons: collections.Counter = collections.Counter()
        self.groups: dict[str, dict[str, int]] = {}
        self.failures: list[dict] = []

    def record(self, *, args, cwd, used, reason, comparison, label="", group="", event_count=1):
        self.applicable += 1
        if event_count != 1:
            self.event_count_not_one += 1
        bucket = self.groups.setdefault(
            group or label or "unknown",
            {"daemon": 0, "walk": 0, "missing": 0, "mismatched": 0},
        )
        if used in ("daemon", "walk"):
            bucket[used] += 1
        else:
            bucket["missing"] += 1
        if comparison not in (None, "inconclusive"):
            bucket["mismatched"] += 1
        if used == "daemon":
            self.daemon += 1
        elif used == "walk":
            self.walk += 1
            self.reasons[reason or "unknown"] += 1
        else:
            self.event_missing += 1
        if comparison == "inconclusive":
            self.inconclusive += 1
        elif comparison is not None:
            self.mismatched += 1
            self.failures.append(
                {
                    "label": label,
                    "args": args,
                    "cwd": str(cwd),
                    "used": used,
                    "reason": reason,
                    "comparison": comparison,
                }
            )

    def summary(self) -> dict:
        return {
            "applicable": self.applicable,
            "pruned_daemon": self.daemon,
            "fallback_walk": self.walk,
            "fallback_reasons": dict(self.reasons.most_common()),
            "mismatched": self.mismatched,
            "inconclusive": self.inconclusive,
            "stale_immediate": self.stale_immediate,
            "event_missing": self.event_missing,
            "event_count_not_one": self.event_count_not_one,
            "errors": self.errors,
            "by_group": dict(sorted(self.groups.items())),
            "failures": self.failures[:100],
        }


def run_pair(wg, rg, svc, args, cwd, base_env, tag, timeout, sort_mode):
    events = common.fresh_event_path(svc.work, tag)
    rg_res = wgtp.run(rg, args, cwd, base_env, timeout=timeout)
    wg_res = wgtp.run(wg, args, cwd, svc.env(events), timeout=timeout)
    comparison = wgtp.compare(args, rg_res, wg_res, sort_mode)
    event = common.decision_event(events)
    used = event.get("used") if event else None
    reason = event.get("reason") if event else None
    return comparison, used, reason, event, rg_res, wg_res, events, len(common.search_events(events))


def run_matrix(wg, rg, svc, cases, cwd, base_env, counts, timeout, limit):
    ran = 0
    for inv in cases:
        if not text_route(inv.args):
            continue
        if limit and ran >= limit:
            break
        ran += 1
        comparison, used, reason, _, _, _, _, event_count = run_pair(
            wg,
            rg,
            svc,
            inv.args,
            cwd,
            base_env,
            f"matrix-{ran:05d}",
            timeout,
            inv.sort_mode,
        )
        counts.record(
            args=inv.args,
            cwd=cwd,
            used=used,
            reason=reason,
            comparison=("inconclusive" if comparison == "inconclusive" else comparison),
            label=f"matrix/{inv.flag_name}/{inv.pattern_kind}",
            group=inv.flag_name,
            event_count=event_count,
        )
        if ran % 200 == 0:
            print(
                f"  [matrix] {ran} cases: daemon={counts.daemon} walk={counts.walk} "
                f"bad={counts.mismatched} stale={counts.stale_immediate}",
                flush=True,
            )


def freshness_mutations(wg, rg, svc, root, base_env, counts, timeout, settle_s, mutations):
    patterns = ["forest", "freshness_token_unique_zzq", "granite", "zzqhmarker"]
    token = "freshness_token_unique_zzq"
    seq = 0

    def check(label, pattern, flag_name, flag_args):
        nonlocal seq
        seq += 1
        args = [*flag_args, "--sort", "path", "--color", "never", "--", pattern, "."]
        comparison, used, reason, _, _, _, _, event_count = run_pair(
            wg, rg, svc, args, root, base_env, f"fresh-{seq:05d}", timeout, "sorted"
        )
        if comparison is None:
            counts.record(args=args, cwd=root, used=used, reason=reason, comparison=None,
                          label=f"freshness/{label}/{flag_name}", group="freshness",
                          event_count=event_count)
            return
        # One immediate retry window: a just-delivered watcher event may not
        # have been applied when the first query started. Convergence is
        # reported, not silently accepted.
        deadline = time.monotonic() + settle_s
        settled = None
        while time.monotonic() < deadline:
            time.sleep(0.05)
            comparison2, used2, reason2, _, _, _, _, event_count2 = run_pair(
                wg, rg, svc, args, root, base_env, f"fresh-settle-{seq:05d}", timeout, "sorted"
            )
            if comparison2 is None:
                settled = (comparison2, used2, reason2, event_count2)
                break
            comparison, used, reason, event_count = comparison2, used2, reason2, event_count2
        if settled is not None:
            counts.stale_immediate += 1
            counts.applicable += 1
            if settled[3] != 1:
                counts.event_count_not_one += 1
            bucket = counts.groups.setdefault("freshness", {"daemon": 0, "walk": 0, "missing": 0, "mismatched": 0})
            if settled[1] == "daemon":
                counts.daemon += 1
                bucket["daemon"] += 1
            elif settled[1] == "walk":
                counts.walk += 1
                bucket["walk"] += 1
                counts.reasons[settled[2] or "unknown"] += 1
            else:
                counts.event_missing += 1
                bucket["missing"] += 1
            counts.failures.append(
                {
                    "label": f"freshness/{label}/{flag_name}",
                    "args": args,
                    "cwd": str(root),
                    "used": settled[1],
                    "reason": settled[2],
                    "comparison": "stale on first query, converged after retry",
                }
            )
        else:
            counts.record(args=args, cwd=root, used=used, reason=reason,
                          comparison=comparison, label=f"freshness/{label}/{flag_name}",
                          group="freshness", event_count=event_count)

    def rerun(label):
        for pattern in patterns:
            for flag_name, flag_args in (("default", []), ("-c", ["-c"]), ("-l", ["-l"])):
                check(label, pattern, flag_name, flag_args)

    rerun("baseline")
    if not mutations:
        return

    target_a = root / "notes" / "grp0" / "note_000.txt"
    with target_a.open("a", encoding="utf-8") as stream:
        stream.write(f"\n{token}_a appended line\n")
    rerun("a_append")

    target_b = root / "notes" / "grp1" / "note_001.txt"
    old = target_b.read_bytes()
    target_b.write_bytes((b"X" * len(old))[: len(old)])
    rerun("b_overwrite_samelen")

    target_c = root / "notes" / "grp2" / f"new_{token}.txt"
    target_c.write_text(f"{token}_c new file\n", encoding="utf-8")
    rerun("c_new_file")

    target_d = root / "notes" / "grp3" / "note_003.txt"
    if target_d.exists():
        target_d.unlink()
    rerun("d_delete")

    target_e_src = root / "notes" / "grp4" / "note_004.txt"
    target_e_dst = root / "notes" / "grp4" / "note_004_renamed.txt"
    if target_e_src.exists():
        target_e_src.rename(target_e_dst)
    rerun("e_rename")

    (root / ".gitignore").write_text("ignored_dir/\n", encoding="utf-8")
    rerun("f_gitignore_rewrite")

    target_g = root / "notes" / "grp5" / "note_005.txt"
    with target_g.open("a", encoding="utf-8") as stream:
        stream.write(f"\n{token}_g racy append\n")
    rerun("g_racy_immediate")

    target_h = root / "notes" / "grp5" / "note_007.txt"
    if target_h.exists():
        old = target_h.read_bytes()
        atom = b"zzqhmarker"
        if len(old) >= len(atom):
            st = target_h.stat()
            target_h.write_bytes((b"Q" * (len(old) - len(atom))) + atom)
            os.utime(target_h, ns=(st.st_atime_ns, st.st_mtime_ns))
    rerun("h_samelen_new_match_restored_mtime")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run", action="store_true", help="execute the suite")
    parser.add_argument("--wg", required=False, help="path to the wg binary")
    parser.add_argument("--rg", default=common.DEFAULT_RG, help="reference rg 15.1.0")
    parser.add_argument("--work", type=Path, default=None, help="scratch directory")
    parser.add_argument("--out", type=Path, default=None, help="JSON summary path")
    parser.add_argument("--model", default="code")
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--settle", type=float, default=3.0)
    parser.add_argument("--limit", type=int, default=0, help="matrix case limit (0 = all)")
    parser.add_argument("--skip-freshness", action="store_true")
    parser.add_argument("--skip-mutations", action="store_true")
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

    work = args.work or common.new_work("wg-text-parity-daemon")
    work = Path(work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    fixture = work / "fixture"
    started = time.time()

    print(f"[setup] wg={wg}")
    print(f"[setup] rg={rg}")
    print(f"[setup] work={work}")
    print("[setup] generating edge-case fixture...")
    if fixture.exists():
        shutil.rmtree(fixture)
    parity.build_fixture(fixture)
    common.git_init(fixture)
    time.sleep(3)

    env = dict(os.environ)
    env.pop("RIPGREP_CONFIG_PATH", None)
    env.pop("WGREP_INDEX_EVENTS", None)

    print("[sample] sampling fixture words...")
    rare, mid, common_words, word_files, files = parity.sample_corpus_words(fixture)
    patterns = parity.build_patterns(rare, mid, common_words, "fixture", n_each=6)
    variants = parity.pick_variant_paths(word_files, files, fixture)
    cases = parity.build_invocations(
        "fixture",
        patterns,
        [],
        implicit_ok=True,
        extra_paths_for_variants={
            "explicit_file": variants["explicit_file"],
            "subdir": variants["subdir"] if variants["subdir"] != "." else None,
            "multi_paths": variants["multi_paths"],
            "patternfile": fixture / "patterns.txt",
        },
    )
    print(f"[setup] {len(cases)} fixture matrix cases")

    svc = common.Service(wg, fixture, work, model=args.model, name="daemon")
    counts = Counts()
    startup = {}
    try:
        print("[setup] wg up (semantic + text index)...")
        startup = svc.start()
        root_info = startup.get("root", {})
        print(
            f"[setup] ready: pid={svc.pid} files={root_info.get('files')} "
            f"text={root_info.get('text', {}).get('files') if isinstance(root_info.get('text'), dict) else None}"
        )
        if isinstance(root_info.get("text"), dict):
            startup["text_files"] = root_info["text"].get("files")
        else:
            print(f"[warn] no text index: {root_info.get('text_error') or startup.get('text_error')}")

        print("[run] matrix...")
        run_matrix(wg, rg, svc, cases, fixture, env, counts, args.timeout, args.limit)

        if not args.skip_freshness:
            print("[run] freshness baseline and mutations...")
            freshness_mutations(
                wg,
                rg,
                svc,
                fixture,
                env,
                counts,
                args.timeout,
                args.settle,
                mutations=not args.skip_mutations,
            )
    except Exception as exc:  # noqa: BLE001 - report, do not skip cleanup
        counts.errors += 1
        counts.failures.append({"label": "harness", "error": f"{type(exc).__name__}: {exc}"})
        print(f"[error] {type(exc).__name__}: {exc}", file=sys.stderr)
    finally:
        stopped = svc.stop()
        print(f"[teardown] {stopped}")

    summary = counts.summary()
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
            "source": common.source_hash(),
            "machine": common.machine_info(),
            "startup": startup if isinstance(startup, dict) else {},
            "teardown": stopped,
            "elapsed_s": round(time.time() - started, 1),
        }
    )
    out = args.out or work / "summary.json"
    common.write_json(out, summary)
    print()
    print("=" * 72)
    for key in (
        "applicable",
        "pruned_daemon",
        "fallback_walk",
        "mismatched",
        "inconclusive",
        "stale_immediate",
        "event_missing",
        "errors",
    ):
        print(f"{key + ':':22} {summary[key]}")
    if summary["fallback_reasons"]:
        print(f"{'fallback reasons:':22} {summary['fallback_reasons']}")
    for failure in summary["failures"][:10]:
        print(f"  FAIL {failure}")
    print(f"wrote {out}")
    print("=" * 72)
    return 1 if (summary["mismatched"] or summary["errors"]) else 0


if __name__ == "__main__":
    raise SystemExit(main())

