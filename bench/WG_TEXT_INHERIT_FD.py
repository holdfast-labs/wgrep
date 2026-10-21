#!/usr/bin/env python3
"""Check that `wg up` does not leak an inherited file descriptor to the daemon.

The Windows regression was a daemon that inherited every inheritable handle of
the caller, so a piped caller waited forever for EOF. This is the Unix version:

1. create a pipe and mark its write end inheritable;
2. start `wg up` with `pass_fds=(write_end,)` and with stdout/stderr captured
   through pipes;
3. wait for `wg up` to exit (bounded);
4. close the parent's write end;
5. require EOF on the read end within 5 s while the daemon is still running;
6. repeat with plain captured stdout/stderr and no extra descriptor.

Evidence includes `ls -l /proc/<daemon pid>/fd` and whether any descriptor
points at the pipe inode. Every subprocess has a timeout, and every daemon is
stopped with `wg down` and then killed by PID if needed.
"""
from __future__ import annotations

import argparse
import json
import os
import select
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import WG_TEXT_COMMON as common  # noqa: E402


def write_fixture(root: Path) -> None:
    (root / "sub").mkdir(parents=True, exist_ok=True)
    (root / "alpha.txt").write_text("alpha needle\n", encoding="utf-8")
    (root / "sub" / "beta.txt").write_text("beta needle\n", encoding="utf-8")
    (root / "sub" / "gamma.txt").write_text("gamma other\n", encoding="utf-8")


def daemon_pid(service_dir: Path) -> int | None:
    meta = service_dir / "service.json"
    try:
        value = json.loads(meta.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    pid = value.get("pid")
    return int(pid) if isinstance(pid, int) else None


def fd_listing(pid: int | None) -> str:
    if not pid:
        return ""
    res = common.run(["ls", "-l", f"/proc/{pid}/fd"], timeout=15, allowed=(0, 2))
    return res["stdout"].decode("utf-8", "replace") + res["stderr"].decode("utf-8", "replace")


def pipe_inode(fd: int) -> int | None:
    try:
        return os.fstat(fd).st_ino
    except OSError:
        return None


def stop(wg: str, env: dict, service_dir: Path, pid: int | None) -> dict:
    out = {"down": None, "killed": False, "alive_after": False}
    try:
        down = common.run([wg, "down"], env=env, timeout=60, allowed=(0, 1))
        out["down"] = {"code": down["code"], "timed_out": down["timed_out"]}
    except Exception as exc:  # noqa: BLE001 - cleanup must continue
        out["down"] = {"error": str(exc)}
    pid = pid or daemon_pid(service_dir)
    deadline = time.monotonic() + 20
    while pid and time.monotonic() < deadline and common.process_alive(pid):
        time.sleep(0.05)
    if pid and common.process_alive(pid):
        try:
            os.kill(pid, signal.SIGKILL)
            out["killed"] = True
        except OSError:
            pass
    if pid:
        out["alive_after"] = common.process_alive(pid)
    return out


def inherited_fd_case(wg: str, work: Path, model: str, timeout: float, eof_wait: float) -> dict:
    root = work / "tree-inherit"
    write_fixture(root)
    service_dir = work / "svc-inherit"
    index_dir = work / "idx-inherit"
    env = dict(os.environ)
    env["WG_SERVICE_DIR"] = str(service_dir)
    env["WGREP_INDEX_DIR"] = str(index_dir)
    env.pop("WGREP_INDEX_EVENTS", None)
    env.pop("RIPGREP_CONFIG_PATH", None)

    read_fd, write_fd = os.pipe()
    os.set_inheritable(write_fd, True)
    inode = pipe_inode(read_fd)
    record = {
        "case": "inherited_fd",
        "pipe_inode": inode,
        "up_timeout_s": timeout,
        "eof_wait_s": eof_wait,
    }
    proc = None
    try:
        started = time.perf_counter()
        proc = subprocess.Popen(
            [wg, "up", str(root), "--model", model, "--json"],
            cwd=str(work),
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            pass_fds=(write_fd,),
        )
        try:
            out, err = proc.communicate(timeout=timeout)
            record["up_exited"] = True
            record["up_timed_out"] = False
        except subprocess.TimeoutExpired:
            proc.kill()
            out, err = proc.communicate(timeout=30)
            record["up_exited"] = False
            record["up_timed_out"] = True
        record["up_elapsed_s"] = round(time.perf_counter() - started, 3)
        record["up_code"] = proc.returncode
        record["up_stdout"] = out.decode("utf-8", "replace")[:800]
        record["up_stderr"] = err.decode("utf-8", "replace")[:800]
        pid = daemon_pid(service_dir)
        record["daemon_pid"] = pid
        record["daemon_alive_before_close"] = bool(pid and common.process_alive(pid))

        os.close(write_fd)
        write_fd = -1
        eof = False
        leaked = b""
        deadline = time.monotonic() + eof_wait
        while time.monotonic() < deadline:
            remaining = max(0.0, deadline - time.monotonic())
            ready, _, _ = select.select([read_fd], [], [], remaining)
            if not ready:
                break
            chunk = os.read(read_fd, 4096)
            if chunk == b"":
                eof = True
                break
            leaked += chunk
        record["eof_within_wait"] = eof
        record["bytes_before_eof"] = len(leaked)
        record["daemon_alive_after_close"] = bool(pid and common.process_alive(pid))
        record["fd_listing"] = fd_listing(pid)
        record["pipe_fd_in_daemon"] = bool(
            inode is not None and f"pipe:[{inode}]" in record["fd_listing"]
        )
        record["pass"] = bool(
            record["up_exited"]
            and not record["up_timed_out"]
            and record.get("up_code") == 0
            and record["daemon_alive_before_close"]
            and eof
            and not record["pipe_fd_in_daemon"]
        )
    except Exception as exc:  # noqa: BLE001 - report, do not skip cleanup
        record["error"] = f"{type(exc).__name__}: {exc}"
        record["pass"] = False
    finally:
        if write_fd >= 0:
            os.close(write_fd)
        os.close(read_fd)
        record["teardown"] = stop(wg, env, service_dir, record.get("daemon_pid"))
    return record


def plain_stdio_case(wg: str, work: Path, model: str, timeout: float) -> dict:
    root = work / "tree-plain"
    write_fixture(root)
    service_dir = work / "svc-plain"
    index_dir = work / "idx-plain"
    env = dict(os.environ)
    env["WG_SERVICE_DIR"] = str(service_dir)
    env["WGREP_INDEX_DIR"] = str(index_dir)
    env.pop("WGREP_INDEX_EVENTS", None)
    env.pop("RIPGREP_CONFIG_PATH", None)
    record = {"case": "plain_stdio", "timeout_s": timeout}
    try:
        started = time.perf_counter()
        proc = subprocess.Popen(
            [wg, "up", str(root), "--model", model, "--json"],
            cwd=str(work),
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            out, err = proc.communicate(timeout=timeout)
            record["up_timed_out"] = False
        except subprocess.TimeoutExpired:
            proc.kill()
            out, err = proc.communicate(timeout=30)
            record["up_timed_out"] = True
        record["up_elapsed_s"] = round(time.perf_counter() - started, 3)
        record["up_code"] = proc.returncode
        record["stdout_eof"] = not record["up_timed_out"]
        record["up_stdout"] = out.decode("utf-8", "replace")[:800]
        record["up_stderr"] = err.decode("utf-8", "replace")[:800]
        pid = daemon_pid(service_dir)
        record["daemon_pid"] = pid
        record["daemon_alive"] = bool(pid and common.process_alive(pid))
        record["fd_listing"] = fd_listing(pid)
        record["pass"] = bool(
            not record["up_timed_out"] and record["up_code"] == 0 and record["daemon_alive"]
        )
    except Exception as exc:  # noqa: BLE001
        record["error"] = f"{type(exc).__name__}: {exc}"
        record["pass"] = False
    finally:
        record["teardown"] = stop(wg, env, service_dir, record.get("daemon_pid"))
    return record


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--wg", required=False)
    parser.add_argument("--work", type=Path, default=None)
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("--model", default="text")
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--eof-wait", type=float, default=5.0)
    args = parser.parse_args()
    if not args.run:
        parser.error("inert unless --run is supplied")
    if not args.wg:
        parser.error("--wg is required")
    wg = str(Path(args.wg).resolve())
    if not Path(wg).is_file():
        parser.error(f"wg binary not found: {wg}")
    if os.name == "nt" or not hasattr(os, "set_inheritable"):
        print("SKIP: this check needs POSIX pass_fds and os.set_inheritable")
        return 0

    work = Path(args.work).resolve() if args.work else common.new_work("wg-text-inherit-fd")
    work.mkdir(parents=True, exist_ok=True)
    started = time.time()
    print(f"[setup] wg={wg}")
    print(f"[setup] work={work}")

    cases = [
        inherited_fd_case(wg, work, args.model, args.timeout, args.eof_wait),
        plain_stdio_case(wg, work, args.model, args.timeout),
    ]
    for case in cases:
        print(
            f"  {case['case']:14} pass={case['pass']} "
            f"up={case.get('up_code')} eof={case.get('eof_within_wait', case.get('stdout_eof'))} "
            f"pipe_fd_in_daemon={case.get('pipe_fd_in_daemon')}"
        )
        if case.get("fd_listing"):
            print("    daemon fds:")
            for line in case["fd_listing"].splitlines()[:20]:
                print(f"      {line}")

    summary = {
        "wg": wg,
        "wg_sha256": common.sha256_file(Path(wg)),
        "work": str(work),
        "source": common.source_hash(),
        "machine": common.machine_info(),
        "model": args.model,
        "cases": cases,
        "pass": all(case.get("pass") for case in cases),
        "elapsed_s": round(time.time() - started, 1),
    }
    out = args.out or work / "summary.json"
    common.write_json(out, summary)
    print(f"wrote {out}")
    return 0 if summary["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
