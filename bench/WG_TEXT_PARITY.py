#!/usr/bin/env python3
"""Differential parity for `wg`'s in-process text route against rg 15.1.0.

`WGREP_PARITY.py` exercises wgrep's vendored ripgrep. This adapter reuses its
fixture, its seeded pattern matrix and its freshness mutations, but runs each
applicable argv through the `wg` binary instead. A bare `wg PATTERN` is
semantic by design, so cases whose argv does not select the text route are
counted as skipped, never run.

Comparison is byte-exact on stdout and stderr plus the exit code. The only
normalization is the elapsed-time fields in `--json` and `--stats`, exactly as
`WGREP_PARITY.py` does. Unsorted (`multiset`) cases compare line records as a
multiset because ripgrep's parallel traversal order is not deterministic.
"""
from __future__ import annotations

import argparse
import collections
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("wgrep_parity", HERE / "WGREP_PARITY.py")
parity = importlib.util.module_from_spec(spec)
spec.loader.exec_module(parity)

WG_BOOL = {"--json", "--explain", "-h", "--help", "-V", "--version"}
WG_VALUE = {"-k", "--top", "--model", "--cache-dir"}


def selects_text_route(args: list[str]) -> bool:
    """Mirror `lexical::classify`: any non-wg flag selects ripgrep."""
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--":
            return False
        if a == "-" or not a.startswith("-"):
            i += 1
            continue
        name = a.split("=", 1)[0]
        if name in WG_BOOL:
            i += 1
            continue
        if name in WG_VALUE or (len(a) > 2 and a[:2] == "-k" and (a[2] == "=" or a[2].isdigit())):
            inline = "=" in a
            i += 1 if inline or (len(a) > 2 and a[:2] == "-k" and a[2].isdigit()) else 2
            continue
        return True
    return False


def run(exe: str, args: list[str], cwd: Path, env: dict, timeout: float = 60.0):
    try:
        proc = subprocess.run(
            [exe, *args], cwd=str(cwd), env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout,
        )
        return proc.returncode, proc.stdout, proc.stderr, False
    except subprocess.TimeoutExpired as e:
        return None, e.stdout or b"", e.stderr or b"", True


def split_records(data: bytes, null_sep: bool) -> list[bytes]:
    sep = b"\0" if null_sep else b"\n"
    parts = data.split(sep)
    if parts and parts[-1] == b"":
        parts = parts[:-1]
    return parts


def compare(args: list[str], rg_res, wg_res, sort_mode: str):
    rg_code, rg_out, rg_err, rg_timeout = rg_res
    wg_code, wg_out, wg_err, wg_timeout = wg_res
    if rg_timeout and wg_timeout:
        return "inconclusive"
    if rg_timeout or wg_timeout:
        return f"timeout: rg={rg_timeout} wg={wg_timeout}"
    has_timing = "--json" in args or "--stats" in args
    if has_timing:
        rg_out = parity.normalize_timing(rg_out)
        wg_out = parity.normalize_timing(wg_out)
    if rg_code != wg_code:
        return f"exit code: rg={rg_code} wg={wg_code}"
    if rg_err != wg_err:
        return f"stderr: rg={rg_err[:200]!r} wg={wg_err[:200]!r}"
    if sort_mode == "sorted":
        if rg_out != wg_out:
            return "stdout differs (sorted)"
        return None
    null_sep = "--null-data" in args
    a = collections.Counter(split_records(rg_out, null_sep))
    b = collections.Counter(split_records(wg_out, null_sep))
    if a != b:
        return f"stdout differs (multiset): rg_only={list((a - b).elements())[:3]!r} wg_only={list((b - a).elements())[:3]!r}"
    return None


def strip_rg_from_path(env: dict) -> dict:
    """Copy env with every PATH entry that contains an `rg` executable removed."""
    out = dict(env)
    entries = []
    for entry in out.get("PATH", "").split(os.pathsep):
        if not entry:
            continue
        names = ["rg.exe", "rg"] if os.name == "nt" else ["rg"]
        if any((Path(entry) / name).is_file() for name in names):
            continue
        entries.append(entry)
    out["PATH"] = os.pathsep.join(entries)
    return out


class Results:
    def __init__(self):
        self.applicable = 0
        self.matched = 0
        self.mismatched = 0
        self.skipped = 0
        self.inconclusive = 0
        self.failures: list[dict] = []

    def record(self, args, cwd, reason):
        self.applicable += 1
        if reason is None:
            self.matched += 1
        elif reason == "inconclusive":
            self.inconclusive += 1
        else:
            self.mismatched += 1
            self.failures.append({"args": args, "cwd": str(cwd), "reason": reason})


def run_cases(rg: str, wg: str, cases, cwd: Path, env: dict, wg_env: dict, results: Results):
    for inv in cases:
        if not selects_text_route(inv.args):
            results.skipped += 1
            continue
        rg_res = run(rg, inv.args, cwd, env)
        wg_res = run(wg, inv.args, cwd, wg_env)
        results.record(inv.args, cwd, compare(inv.args, rg_res, wg_res, inv.sort_mode))


def freshness_cases(rg: str, wg: str, root: Path, env: dict, wg_env: dict, results: Results,
                    mutations: bool):
    patterns = ["forest", "freshness_token_unique_zzq", "granite", "zzqhmarker"]
    token = "freshness_token_unique_zzq"

    def rerun(label: str):
        for pattern in patterns:
            for flag_name, flag_args in (("default", []), ("-c", ["-c"]), ("-l", ["-l"])):
                args = [*flag_args, "--sort", "path", "--color", "never", "--", pattern, "."]
                rg_res = run(rg, args, root, env)
                wg_res = run(wg, args, root, wg_env)
                reason = compare(args, rg_res, wg_res, "sorted")
                if reason is not None:
                    reason = f"{label}: {reason}"
                results.record(args, root, reason)

    rerun("baseline")
    if not mutations:
        return

    target_a = root / "notes" / "grp0" / "note_000.txt"
    with target_a.open("a", encoding="utf-8") as f:
        f.write(f"\n{token}_a appended line\n")
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
    rerun("f_unignore")

    target_g = root / "notes" / "grp5" / "note_005.txt"
    with target_g.open("a", encoding="utf-8") as f:
        f.write(f"\n{token}_g racy append\n")
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
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--run", action="store_true", help="execute the suite")
    ap.add_argument("--wg", required=False, help="path to the wg binary")
    ap.add_argument("--rg", default="rg", help="reference rg 15.1.0 executable")
    ap.add_argument("--work", type=Path, default=Path(tempfile.gettempdir()) / f"wg-text-parity-{os.getpid()}",
                    help="scratch directory")
    ap.add_argument("--out", type=Path, default=None, help="JSON summary path")
    ap.add_argument("--skip-freshness", action="store_true")
    ap.add_argument("--skip-mutations", action="store_true",
                    help="run the freshness baseline and mutations only if both this and --skip-freshness are absent")
    ap.add_argument("--wg-no-rg", action="store_true",
                    help="run wg with every rg-containing PATH entry removed")
    args = ap.parse_args()
    if not args.run:
        ap.error("inert unless --run is supplied")
    if not args.wg:
        ap.error("--wg is required")

    rg = shutil.which(args.rg) or (str(Path(args.rg).resolve()) if Path(args.rg).is_file() else None)
    if not rg:
        ap.error(f"rg not found: {args.rg}")
    wg = str(Path(args.wg).resolve())
    if not Path(wg).is_file():
        ap.error(f"wg binary not found: {wg}")

    work = args.work.resolve()
    fixture = work / "fixture"
    work.mkdir(parents=True, exist_ok=True)
    print(f"[setup] wg={wg}")
    print(f"[setup] rg={rg}")
    print(f"[setup] work={work}")
    print("[setup] generating edge-case fixture...")
    parity.build_fixture(fixture)
    parity.git_init(fixture)
    time.sleep(3)

    env = dict(os.environ)
    wg_env = strip_rg_from_path(env) if args.wg_no_rg else dict(env)

    print("[sample] sampling fixture words...")
    rare, mid, common, word_files, files = parity.sample_corpus_words(fixture)
    patterns = parity.build_patterns(rare, mid, common, "fixture", n_each=6)
    variants = parity.pick_variant_paths(word_files, files, fixture)
    cases = parity.build_invocations("fixture", patterns, [], implicit_ok=True, extra_paths_for_variants={
        "explicit_file": variants["explicit_file"],
        "subdir": variants["subdir"] if variants["subdir"] != "." else None,
        "multi_paths": variants["multi_paths"],
        "patternfile": fixture / "patterns.txt",
    })
    print(f"[run] {len(cases)} fixture matrix cases")

    results = Results()
    run_cases(rg, wg, cases, fixture, env, wg_env, results)
    if not args.skip_freshness:
        print("[freshness] baseline and mutations...")
        freshness_cases(rg, wg, fixture, env, wg_env, results, mutations=not args.skip_mutations)

    summary = {
        "wg": wg,
        "rg": rg,
        "work": str(work),
        "wg_path_without_rg": args.wg_no_rg,
        "applicable": results.applicable,
        "matched": results.matched,
        "mismatched": results.mismatched,
        "inconclusive_both_timed_out": results.inconclusive,
        "skipped_not_text_route": results.skipped,
        "failures": results.failures,
    }
    out = args.out or work / "summary.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(summary, indent=2, ensure_ascii=True) + "\n", encoding="utf-8")
    print()
    print("=" * 70)
    print(f"applicable: {results.applicable}")
    print(f"matched:    {results.matched}")
    print(f"mismatched: {results.mismatched}")
    print(f"inconclusive (both timed out): {results.inconclusive}")
    print(f"skipped (not text route): {results.skipped}")
    for failure in results.failures[:20]:
        print(f"  MISMATCH {failure['args']}: {failure['reason']}")
    print(f"wrote {out}")
    print("=" * 70)
    return 1 if results.mismatched else 0


if __name__ == "__main__":
    raise SystemExit(main())
