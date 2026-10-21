#!/usr/bin/env python3
"""Text-route speed: `wg` with an attached trigram index vs direct rg 15.1.0.

Two measurements on a copy of the Rust `src` corpus:

1. One query at a time. Five patterns (absent literal, rare identifier, common
   identifier, broad regex, case-insensitive literal) are timed for both tools.
   Order alternates per sample so drift does not favor one arm. Warmups are
   excluded.
2. Concurrent clients. 1, 4, 16 and 32 independent processes run a mixed query
   sequence; the report has queries/s, p50/p95 latency and errors. `wg` clients
   share one daemon; every `rg` client is its own process.

It also records daemon RSS (``VmRSS`` and ``smaps_rollup`` Pss) with and without
a text index. The no-text arm points ``WGREP_INDEX_DIR`` at a regular file, so
``TextIndex::open`` fails and `wg up` reports ``text_error``; the semantic route
still works. That is the semantic-only baseline.

Every subprocess has a timeout and its exit code is checked. Each arm uses an
isolated ``WG_SERVICE_DIR`` and ``WGREP_INDEX_DIR``. Daemons are stopped with
``wg down`` and killed by PID if still alive.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import statistics
import subprocess
import sys
import threading
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


parity = _load("wgrep_parity", "WGREP_PARITY.py")


# ---------------------------------------------------------------------------
# Corpus and queries
# ---------------------------------------------------------------------------

def tree_stats(root: Path) -> dict:
    files = 0
    size = 0
    for path in root.rglob("*"):
        if path.is_file():
            files += 1
            try:
                size += path.stat().st_size
            except OSError:
                pass
    return {"files": files, "bytes": size}


def doc_frequency(rg: str, corpus: Path, words: list[str], timeout: float) -> dict[str, int]:
    counts = {}
    for word in words:
        res = common.run([rg, "-F", "-l", "--", word, "."], cwd=corpus, timeout=timeout, allowed=(0, 1))
        if res["unexpected"]:
            counts[word] = -1
        else:
            counts[word] = res["stdout"].count(b"\n")
    return counts


def choose_queries(rg: str, corpus: Path, timeout: float) -> tuple[list[dict], dict]:
    rare, mid, common_words, _, sampled = parity.sample_corpus_words(corpus)
    rare_pool = [w for w in rare if len(w) >= 8][:12] or rare[:12] or ["zzraremissing"]
    rare_counts = doc_frequency(rg, corpus, rare_pool, timeout)
    present = [(count, word) for word, count in rare_counts.items() if count > 0]
    if present:
        rare_pick = min(present)[1]
    else:
        rare_pick = rare_pool[0]
    common_pool = common_words[:8] or ["the"]
    common_counts = doc_frequency(rg, corpus, common_pool, timeout)
    common_pick = max(common_pool, key=lambda word: common_counts.get(word, -1))
    absent = "zzqxjwvbnm_wg_text_absent_zzz"
    return [
        {"name": "absent_literal", "args": ["-F"], "pattern": absent},
        {"name": "rare_identifier", "args": ["-F"], "pattern": rare_pick},
        {"name": "common_identifier", "args": ["-F"], "pattern": common_pick},
        # `-n` keeps the invocation on the text route: a bare PATTERN is
        # semantic in `wg`, and `--` alone is not a ripgrep flag.
        {"name": "broad_regex", "args": ["-n"], "pattern": r"\w+\("},
        {"name": "case_insensitive", "args": ["-i", "-F"], "pattern": "error"},
    ], {
        "rare": rare_pick,
        "common": common_pick,
        "absent": absent,
        "rare_candidates": rare_counts,
        "common_candidates": common_counts,
        "sampled_files": len(sampled),
    }


def timed(exe, args, cwd, env, timeout):
    return common.run(
        [exe, *args],
        cwd=cwd,
        env=env,
        timeout=timeout,
        allowed=(0, 1),
        capture=False,
    )


def single_query(wg, rg, svc, corpus, rg_env, query, samples, timeout):
    args = [*query["args"], "--", query["pattern"], "."]
    wg_env = svc.env()
    for _ in range(2):
        timed(rg, args, corpus, rg_env, timeout)
        timed(wg, args, corpus, wg_env, timeout)

    rg_ms = []
    wg_ms = []
    errors = []
    for i in range(samples):
        order = (("rg", rg, rg_env), ("wg", wg, wg_env)) if i % 2 == 0 else (("wg", wg, wg_env), ("rg", rg, rg_env))
        for name, exe, env in order:
            res = timed(exe, args, corpus, env, timeout)
            if res["unexpected"]:
                errors.append({"tool": name, "code": res["code"], "timed_out": res["timed_out"]})
                continue
            (rg_ms if name == "rg" else wg_ms).append(res["elapsed_ms"])
    return {
        "name": query["name"],
        "pattern": query["pattern"],
        "args": args,
        "rg_samples_ms": rg_ms,
        "wg_samples_ms": wg_ms,
        "rg_p50_ms": common.percentile(rg_ms, 50),
        "rg_p95_ms": common.percentile(rg_ms, 95),
        "wg_p50_ms": common.percentile(wg_ms, 50),
        "wg_p95_ms": common.percentile(wg_ms, 95),
        "ratio_p50": (common.percentile(wg_ms, 50) / common.percentile(rg_ms, 50))
        if rg_ms and wg_ms and common.percentile(rg_ms, 50)
        else None,
        "errors": errors,
        "n": {"rg": len(rg_ms), "wg": len(wg_ms)},
    }


def run_to_file(argv, cwd, env, timeout, out_path: Path, err_path: Path):
    started = time.perf_counter()
    try:
        with open(out_path, "wb") as out, open(err_path, "wb") as err:
            proc = subprocess.run(
                [str(a) for a in argv],
                cwd=str(cwd),
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
        "code": code,
        "timed_out": timed_out,
        "unexpected": timed_out or code not in (0, 1),
        "elapsed_ms": (time.perf_counter() - started) * 1000.0,
        "path": str(out_path),
    }


def output_parity(wg, rg, svc, corpus, rg_env, query, work, timeout):
    args = [*query["args"], "--sort", "path", "--color", "never", "--", query["pattern"], "."]
    tag = query["name"]
    rg_out = work / f"parity-{tag}-rg.out"
    wg_out = work / f"parity-{tag}-wg.out"
    rg_err = work / f"parity-{tag}-rg.err"
    wg_err = work / f"parity-{tag}-wg.err"
    events = common.fresh_event_path(work, f"parity-{tag}")
    rg_res = run_to_file([rg, *args], corpus, rg_env, timeout, rg_out, rg_err)
    wg_res = run_to_file([wg, *args], corpus, svc.env(events), timeout, wg_out, wg_err)
    event = common.decision_event(events)
    rg_hash = common.sha256_file(rg_out) if rg_out.is_file() else None
    wg_hash = common.sha256_file(wg_out) if wg_out.is_file() else None
    rg_size = rg_out.stat().st_size if rg_out.is_file() else 0
    wg_size = wg_out.stat().st_size if wg_out.is_file() else 0
    exact = (
        not rg_res["unexpected"]
        and not wg_res["unexpected"]
        and rg_res["code"] == wg_res["code"]
        and rg_hash == wg_hash
        and rg_size == wg_size
    )
    return {
        "name": tag,
        "exact": exact,
        "rg_code": rg_res["code"],
        "wg_code": wg_res["code"],
        "rg_bytes": rg_size,
        "wg_bytes": wg_size,
        "rg_sha256": rg_hash,
        "wg_sha256": wg_hash,
        "used": event.get("used") if event else None,
        "reason": event.get("reason") if event else None,
        "event_count": len(common.search_events(events)),
        "errors": [
            {"tool": tool, "code": res["code"], "timed_out": res["timed_out"]}
            for tool, res in (("rg", rg_res), ("wg", wg_res))
            if res["unexpected"]
        ],
    }


# ---------------------------------------------------------------------------
# Concurrency
# ---------------------------------------------------------------------------

def run_arm(exe, env, corpus, queries, clients, per_client, timeout):
    latencies = []
    errors = []
    lock = threading.Lock()

    def agent(index: int) -> None:
        for step in range(per_client):
            query = queries[(index + step) % len(queries)]
            res = timed(exe, query["args"] + ["--", query["pattern"], "."], corpus, env, timeout)
            with lock:
                if res["unexpected"]:
                    errors.append({"code": res["code"], "timed_out": res["timed_out"]})
                else:
                    latencies.append(res["elapsed_ms"])

    started = time.perf_counter()
    threads = [threading.Thread(target=agent, args=(i,)) for i in range(clients)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    makespan = time.perf_counter() - started
    return latencies, makespan, errors


def concurrency_suite(wg, rg, svc, corpus, rg_env, queries, clients_list, per_client, reps, timeout):
    rows = []
    wg_env = svc.env()
    concurrency_queries = [q for q in queries if q["name"] in ("absent_literal", "rare_identifier", "common_identifier", "broad_regex")]
    for clients in clients_list:
        for tool, exe, env in (("wg", wg, wg_env), ("rg", rg, rg_env)):
            throughputs = []
            p50s = []
            p95s = []
            error_count = 0
            for _ in range(reps):
                latencies, makespan, errors = run_arm(exe, env, corpus, concurrency_queries, clients, per_client, timeout)
                error_count += len(errors)
                if makespan > 0:
                    throughputs.append(len(latencies) / makespan)
                p50s.append(common.percentile(latencies, 50))
                p95s.append(common.percentile(latencies, 95))
            row = {
                "clients": clients,
                "tool": tool,
                "queries_per_s": common.median(throughputs),
                "p50_ms": common.median(p50s),
                "p95_ms": common.median(p95s),
                "errors": error_count,
                "reps": reps,
                "per_client": per_client,
            }
            rows.append(row)
            print(
                f"  {clients:>3} {tool:>3} {row['queries_per_s']:>8.1f} q/s "
                f"{row['p50_ms']:>8.0f} ms p50 {row['p95_ms']:>8.0f} ms p95 errors={error_count}",
                flush=True,
            )
    return rows


# ---------------------------------------------------------------------------
# RSS
# ---------------------------------------------------------------------------

def rss_row(svc, corpus, wg, queries, timeout, label):
    pid = svc.pid
    if not pid:
        return {"label": label, "error": "no daemon pid"}
    time.sleep(3.0)
    idle_rss = common.rss_kb(pid)
    idle_pss = common.smaps_kb(pid, "Pss")
    env = svc.env()
    for _ in range(2):
        for query in queries:
            timed(wg, query["args"] + ["--", query["pattern"], "."], corpus, env, timeout)
    time.sleep(2.0)
    loaded_rss = common.rss_kb(pid)
    loaded_pss = common.smaps_kb(pid, "Pss")
    status = {}
    try:
        status = svc.status()
    except Exception as exc:  # noqa: BLE001 - status is supplementary
        status = {"error": f"{type(exc).__name__}: {exc}"}
    roots = status.get("roots", []) if isinstance(status, dict) else []
    text = None
    text_error = None
    if roots:
        text = roots[0].get("text")
        text_error = roots[0].get("text_error")
    return {
        "label": label,
        "pid": pid,
        "idle_rss_mib": (idle_rss / 1024.0) if idle_rss else None,
        "idle_pss_mib": (idle_pss / 1024.0) if idle_pss else None,
        "loaded_rss_mib": (loaded_rss / 1024.0) if loaded_rss else None,
        "loaded_pss_mib": (loaded_pss / 1024.0) if loaded_pss else None,
        "text_present": text is not None,
        "text_error": text_error,
        "text_files": text.get("files") if isinstance(text, dict) else None,
        "text_resident_mib": (text.get("resident_bytes", 0) / 1e6) if isinstance(text, dict) else None,
        "text_disk_mib": (text.get("disk_bytes", 0) / 1e6) if isinstance(text, dict) else None,
    }


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--wg", required=False)
    parser.add_argument("--rg", default=common.DEFAULT_RG)
    parser.add_argument("--corpus", default="/srv/workspaces/wispd/prof/src")
    parser.add_argument("--work", type=Path, default=None)
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("--model", default="code")
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("--start-timeout", type=float, default=3600.0)
    parser.add_argument("--clients", default="1,4,16,32")
    parser.add_argument("--per-client", type=int, default=4)
    parser.add_argument("--reps", type=int, default=2)
    parser.add_argument("--skip-copy", action="store_true")
    parser.add_argument("--skip-concurrency", action="store_true")
    parser.add_argument("--skip-rss", action="store_true")
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
    source_corpus = Path(args.corpus).resolve()
    if not source_corpus.is_dir():
        parser.error(f"corpus not found: {source_corpus}")

    work = Path(args.work).resolve() if args.work else common.new_work("wg-text-speed")
    work.mkdir(parents=True, exist_ok=True)
    corpus = source_corpus
    if not args.skip_copy:
        corpus = work / "corpus"
        if corpus.exists():
            shutil.rmtree(corpus)
        print(f"[setup] copying {source_corpus} -> {corpus}")
        shutil.copytree(source_corpus, corpus)
    source_stats = tree_stats(source_corpus)
    corpus_stats = tree_stats(corpus)

    queries, selection = choose_queries(rg, corpus, args.timeout)
    print(f"[setup] queries: {[(q['name'], q['pattern']) for q in queries]}")
    print(f"[setup] corpus files={corpus_stats['files']} bytes={corpus_stats['bytes']}")

    rg_env = dict(os.environ)
    rg_env.pop("WG_SERVICE_DIR", None)
    rg_env.pop("WGREP_INDEX_DIR", None)
    rg_env.pop("WGREP_INDEX_EVENTS", None)
    rg_env.pop("RIPGREP_CONFIG_PATH", None)

    started = time.time()
    report = {
        "wg": wg,
        "wg_sha256": common.sha256_file(Path(wg)),
        "rg": rg,
        "rg_version": common.require(common.run([rg, "--version"], timeout=30, allowed=(0,)))["stdout"]
        .decode("utf-8", "replace")
        .splitlines()[0],
        "source_corpus": str(source_corpus),
        "corpus": str(corpus),
        "copied": not args.skip_copy,
        "source_corpus_stats": source_stats,
        "corpus_stats": corpus_stats,
        "query_selection": selection,
        "queries": [],
        "parity": [],
        "concurrency": [],
        "rss": [],
        "source": common.source_hash(),
        "machine": common.machine_info(),
        "model": args.model,
        "samples": args.samples,
    }

    svc = common.Service(
        wg,
        corpus,
        work,
        model=args.model,
        name="with-text",
        text_index=True,
        start_timeout=args.start_timeout,
    )
    stopped = {}
    try:
        print("[setup] wg up (semantic + text index)...")
        info = svc.start()
        root = info.get("root", {})
        text = root.get("text") if isinstance(root, dict) else None
        report["startup"] = {
            "pid": svc.pid,
            "files": root.get("files") if isinstance(root, dict) else None,
            "chunks": root.get("chunks") if isinstance(root, dict) else None,
            "text": text,
            "text_error": root.get("text_error") if isinstance(root, dict) else None,
            "total_ms": info.get("total_ms"),
        }
        print(
            f"[setup] pid={svc.pid} files={report['startup']['files']} "
            f"text_files={text.get('files') if isinstance(text, dict) else None}"
        )
        if not isinstance(text, dict):
            print(f"[warn] text index unavailable: {report['startup']['text_error']}")

        # Prove the candidate path is active at least once, and check output.
        print("[run] output parity (sorted, exact bytes)...")
        for query in queries:
            item = output_parity(wg, rg, svc, corpus, rg_env, query, work, args.timeout)
            report["parity"].append(item)
            print(
                f"  {item['name']:20} exact={item['exact']} used={item['used']} "
                f"rg={item['rg_bytes']}B wg={item['wg_bytes']}B"
            )

        print(f"[run] single-query timing, {args.samples} samples each...")
        for query in queries:
            item = single_query(wg, rg, svc, corpus, rg_env, query, args.samples, args.timeout)
            report["queries"].append(item)
            ratio = item["ratio_p50"]
            ratio_text = f"{ratio:.2f}" if ratio is not None else "n/a"
            print(
                f"  {item['name']:20} rg p50={item['rg_p50_ms']:7.1f} p95={item['rg_p95_ms']:7.1f} "
                f"wg p50={item['wg_p50_ms']:7.1f} p95={item['wg_p95_ms']:7.1f} "
                f"ratio={ratio_text} errors={len(item['errors'])}"
            )

        if not args.skip_concurrency:
            print("[run] concurrency...")
            clients_list = [int(x) for x in args.clients.split(",") if x.strip()]
            report["concurrency"] = concurrency_suite(
                wg,
                rg,
                svc,
                corpus,
                rg_env,
                queries,
                clients_list,
                args.per_client,
                args.reps,
                args.timeout,
            )

        if not args.skip_rss:
            print("[run] daemon RSS with text index...")
            report["rss"].append(rss_row(svc, corpus, wg, queries, args.timeout, "with_text"))
            print(
                f"  with text: idle={report['rss'][-1]['idle_rss_mib']:.1f} MiB "
                f"loaded={report['rss'][-1]['loaded_rss_mib']:.1f} MiB"
            )
    except Exception as exc:  # noqa: BLE001 - report, do not skip cleanup
        report["error"] = f"{type(exc).__name__}: {exc}"
        print(f"[error] {type(exc).__name__}: {exc}", file=sys.stderr)
    finally:
        stopped = svc.stop()
        report["teardown_with_text"] = stopped
        print(f"[teardown] {stopped}")

    if not args.skip_rss:
        print("[run] daemon RSS without text index (semantic-only baseline)...")
        no_text = common.Service(
            wg,
            corpus,
            work,
            model=args.model,
            name="without-text",
            text_index=False,
            start_timeout=args.start_timeout,
        )
        try:
            info = no_text.start()
            root = info.get("root", {})
            text = root.get("text") if isinstance(root, dict) else None
            if isinstance(text, dict):
                report["no_text_startup_error"] = {
                    "error": "text index unexpectedly present",
                    "text": text,
                }
            row = rss_row(no_text, corpus, wg, queries, args.timeout, "without_text")
            report["rss"].append(row)
            report["no_text_startup"] = {
                "text_present": isinstance(text, dict),
                "text_error": root.get("text_error") if isinstance(root, dict) else None,
            }
            print(
                f"  without text: idle={row.get('idle_rss_mib'):.1f} MiB "
                f"loaded={row.get('loaded_rss_mib'):.1f} MiB "
                f"text_error={report['no_text_startup']['text_error']}"
            )
        except Exception as exc:  # noqa: BLE001
            report["no_text_error"] = f"{type(exc).__name__}: {exc}"
            print(f"[error] no-text arm: {type(exc).__name__}: {exc}", file=sys.stderr)
        finally:
            stopped_no_text = no_text.stop()
            report["teardown_without_text"] = stopped_no_text
            print(f"[teardown] {stopped_no_text}")

    report["elapsed_s"] = round(time.time() - started, 1)
    out = args.out or work / "results.json"
    common.write_json(out, report)
    print(f"wrote {out}")

    failed = bool(report.get("error")) or bool(report.get("no_text_error"))
    failed |= any(not item["exact"] or item.get("event_count") != 1 for item in report["parity"])
    failed |= any(len(item["errors"]) for item in report["queries"])
    failed |= any(row["errors"] for row in report["concurrency"])
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
