#!/usr/bin/env python3
"""Shared helpers for the wg Phase B2 text-route harnesses.

Only the harnesses in ``bench/WG_TEXT_*`` import this module. It keeps the
common rules in one place:

* every subprocess has a timeout and its exit code is checked (or recorded as
  an explicit error);
* every run gets an isolated ``WG_SERVICE_DIR`` and ``WGREP_INDEX_DIR``;
* a daemon that a harness starts is stopped with ``wg down`` and then killed by
  PID if it is still alive;
* results carry the hash of the tested source tree.

The observable candidate-path signal is the NDJSON file named by
``WGREP_INDEX_EVENTS``. ``wisp_grep::run_rg_text`` writes one ``search`` event
per invocation with ``used`` = ``daemon`` or ``walk`` plus a ``reason``.
"""
from __future__ import annotations

import contextlib
import hashlib
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
DEFAULT_RG = "/srv/workspaces/wispd/tools/rg-15/rg" if os.name != "nt" else "rg"
SEARCH_CODES = (0, 1)


# ---------------------------------------------------------------------------
# Source identity
# ---------------------------------------------------------------------------

def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _tree_digest(root: Path, repo: Path) -> dict:
    files = []
    if root.is_dir():
        for path in sorted(root.rglob("*")):
            if path.is_file():
                files.append((path.relative_to(repo).as_posix(), sha256_file(path)))
    combined = hashlib.sha256()
    for rel, digest in files:
        combined.update(f"{digest}  {rel}\n".encode("utf-8"))
    return {"sha256": combined.hexdigest(), "files": len(files)}


def source_hash(repo: Path = REPO) -> dict:
    """Hash ``src/``, ``crates/wisp-grep/src/`` and ``Cargo.lock``.

    The combined digest is sha256 over sorted ``"<file sha>  <relative path>``
    lines, so it changes with any content, add or delete under those paths.
    """
    components = {
        "src": _tree_digest(repo / "src", repo),
        "crates/wisp-grep/src": _tree_digest(repo / "crates" / "wisp-grep" / "src", repo),
    }
    lock = repo / "Cargo.lock"
    lock_hash = sha256_file(lock) if lock.is_file() else None
    components["Cargo.lock"] = {"sha256": lock_hash, "files": int(lock.is_file())}
    combined = hashlib.sha256(
        json.dumps(components, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()
    return {"sha256": combined, "components": components}


def machine_info() -> dict:
    info = {
        "hostname": os.uname().nodename if hasattr(os, "uname") else os.environ.get("COMPUTERNAME"),
        "platform": sys.platform,
        "cpu_count": os.cpu_count(),
    }
    try:
        with open("/proc/meminfo", encoding="utf-8") as f:
            for line in f:
                if line.startswith("MemTotal:"):
                    info["mem_total_kb"] = int(line.split()[1])
                    break
    except OSError:
        pass
    return info


# ---------------------------------------------------------------------------
# Subprocesses
# ---------------------------------------------------------------------------

class CommandError(RuntimeError):
    def __init__(self, res: dict):
        super().__init__(
            f"{res['argv']} exited {res['code']}"
            + (" (timeout)" if res["timed_out"] else "")
            + f"; stderr={res['stderr'][:300]!r}"
        )
        self.res = res


def run(
    argv,
    *,
    cwd: Path | str | None = None,
    env: dict | None = None,
    timeout: float = 120.0,
    allowed=SEARCH_CODES,
    stdin: bytes | None = None,
    capture: bool = True,
) -> dict:
    """Run one subprocess with a timeout.

    ``allowed`` is the set of exit codes the caller accepts. An unexpected
    code or a timeout is returned in ``unexpected``/``timed_out``; callers that
    must not continue use :func:`require`.
    """
    argv = [str(a) for a in argv]
    started = time.perf_counter()
    try:
        proc = subprocess.run(
            argv,
            cwd=str(cwd) if cwd is not None else None,
            env=env,
            input=stdin,
            stdin=subprocess.DEVNULL if stdin is None else None,
            stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
            stderr=subprocess.PIPE if capture else subprocess.DEVNULL,
            timeout=timeout,
        )
        code = proc.returncode
        out = proc.stdout or b""
        err = proc.stderr or b""
        timed_out = False
    except subprocess.TimeoutExpired as exc:
        code = None
        out = exc.stdout or b""
        err = exc.stderr or b""
        timed_out = True
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    unexpected = timed_out or (allowed is not None and code not in allowed)
    return {
        "argv": argv,
        "code": code,
        "stdout": out,
        "stderr": err,
        "timed_out": timed_out,
        "unexpected": unexpected,
        "elapsed_ms": elapsed_ms,
    }


def require(res: dict) -> dict:
    if res["unexpected"]:
        raise CommandError(res)
    return res


# ---------------------------------------------------------------------------
# Service lifecycle
# ---------------------------------------------------------------------------

class Service:
    """One isolated ``wg`` daemon for one harness run or one root."""

    def __init__(
        self,
        wg: str | Path,
        root: Path,
        work: Path,
        *,
        model: str = "code",
        name: str = "service",
        text_index: bool = True,
        start_timeout: float = 3600.0,
    ):
        self.wg = str(Path(wg).resolve())
        self.root = Path(root).resolve()
        self.work = Path(work)
        self.model = model
        self.dir = self.work / name
        self.index_dir = self.work / f"{name}-text-index"
        self.text_index = text_index
        self.start_timeout = start_timeout
        self.pid: int | None = None
        self.info: dict = {}
        self._started = False

    def env(self, events: Path | None = None, extra: dict | None = None) -> dict:
        env = dict(os.environ)
        env["WG_SERVICE_DIR"] = str(self.dir)
        env["WGREP_INDEX_DIR"] = str(self.index_dir)
        env.pop("WGREP_INDEX_EVENTS", None)
        env.pop("WGREP_NO_DAEMON", None)
        env.pop("WGREP_NO_INDEX", None)
        env.pop("RIPGREP_CONFIG_PATH", None)
        if events is not None:
            env["WGREP_INDEX_EVENTS"] = str(events)
        if extra:
            env.update(extra)
        return env

    def start(self) -> dict:
        self.dir.mkdir(parents=True, exist_ok=True)
        if not self.text_index:
            # A regular file where the index directory should be makes
            # TextIndex::open fail without panicking. The semantic route stays
            # usable; `wg up` reports text_error.
            self.index_dir.parent.mkdir(parents=True, exist_ok=True)
            self.index_dir.write_text("text index disabled for this run\n", encoding="utf-8")
        else:
            self.index_dir.mkdir(parents=True, exist_ok=True)
        res = require(
            run(
                [self.wg, "up", str(self.root), "--model", self.model, "--json"],
                cwd=self.work,
                env=self.env(),
                timeout=self.start_timeout,
                allowed=(0,),
            )
        )
        try:
            self.info = json.loads(res["stdout"].decode("utf-8"))
        except (ValueError, UnicodeDecodeError) as exc:
            raise RuntimeError(f"wg up returned invalid JSON: {exc}: {res['stdout'][:500]!r}") from exc
        self.pid = self.info.get("pid")
        if self.pid is None:
            meta = self.dir / "service.json"
            if meta.is_file():
                self.pid = json.loads(meta.read_text(encoding="utf-8")).get("pid")
        self._started = True
        return self.info

    def stop(self, timeout: float = 30.0) -> dict:
        """Stop the daemon: ``wg down``, then SIGKILL the recorded PID."""
        outcome = {"down": None, "killed": False}
        if not self._started:
            return outcome
        try:
            down = run([self.wg, "down"], cwd=self.work, env=self.env(), timeout=timeout, allowed=(0, 1))
            outcome["down"] = {"code": down["code"], "timed_out": down["timed_out"]}
        except Exception as exc:  # noqa: BLE001 - cleanup must continue
            outcome["down"] = {"error": str(exc)}
        deadline = time.monotonic() + timeout
        while self.pid and time.monotonic() < deadline:
            if not process_alive(self.pid):
                break
            time.sleep(0.05)
        if self.pid and process_alive(self.pid):
            try:
                os.kill(self.pid, signal.SIGKILL)
                outcome["killed"] = True
            except (OSError, AttributeError):
                pass
            for _ in range(100):
                if not process_alive(self.pid):
                    break
                time.sleep(0.02)
        self._started = False
        outcome["alive_after"] = bool(self.pid and process_alive(self.pid))
        return outcome

    def status(self, timeout: float = 60.0) -> dict:
        res = require(
            run([self.wg, "status", "--json"], cwd=self.work, env=self.env(), timeout=timeout, allowed=(0,))
        )
        return json.loads(res["stdout"].decode("utf-8"))


@contextlib.contextmanager
def service(wg, root, work, **kwargs):
    svc = Service(wg, root, work, **kwargs)
    try:
        svc.start()
        yield svc
    finally:
        svc.stop()


if os.name == "nt":
    import ctypes
    from ctypes import wintypes

    _PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    _STILL_ACTIVE = 259
    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    _kernel32.OpenProcess.restype = wintypes.HANDLE
    _kernel32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
    _kernel32.GetExitCodeProcess.restype = wintypes.BOOL
    _kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    _kernel32.CloseHandle.restype = wintypes.BOOL

    def _process_alive_windows(pid: int) -> bool:
        handle = _kernel32.OpenProcess(_PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not handle:
            return False
        try:
            code = wintypes.DWORD()
            if not _kernel32.GetExitCodeProcess(handle, ctypes.byref(code)):
                return False
            return code.value == _STILL_ACTIVE
        finally:
            _kernel32.CloseHandle(handle)


def process_alive(pid: int) -> bool:
    """Return True while ``pid`` is a live process.

    ``os.kill(pid, 0)`` is a reliable probe only on POSIX. On Windows it
    raises ``WinError 87`` for a live process, so query the exit code instead.
    """
    if pid <= 0:
        return False
    if os.name == "nt":
        return _process_alive_windows(pid)
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def rss_kb(pid: int) -> int | None:
    try:
        with open(f"/proc/{pid}/status", encoding="utf-8") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except OSError:
        pass
    try:
        import psutil  # type: ignore

        return int(psutil.Process(pid).memory_info().rss // 1024)
    except Exception:  # noqa: BLE001 - best effort
        return None


def smaps_kb(pid: int, key: str) -> int | None:
    """Read a ``smaps_rollup`` total such as ``Pss`` or ``Private_Dirty``."""
    try:
        with open(f"/proc/{pid}/smaps_rollup", encoding="utf-8") as f:
            for line in f:
                if line.startswith(f"{key}:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return None


# ---------------------------------------------------------------------------
# Events (candidate-path observability)
# ---------------------------------------------------------------------------

def read_events(path: Path | None) -> list[dict]:
    if path is None or not path.is_file():
        return []
    events = []
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except ValueError:
            events.append({"raw": line})
        else:
            events.append(value)
    return events


def search_events(path: Path | None) -> list[dict]:
    return [e for e in read_events(path) if e.get("event") == "search"]


def decision_event(path: Path | None) -> dict | None:
    """The first `search` event for one invocation.

    ``run_rg_text`` records its decision (daemon, or walk plus the real
    fallback reason) before the search runs. When it falls back, the vendored
    engine later calls ``record_walk`` because ``run_rg_text`` sets walk-only;
    that second event says only ``daemon_disabled``. The first event is the
    one that names the actual reason.
    """
    events = search_events(path)
    return events[0] if events else None


def last_search_event(path: Path | None) -> dict | None:
    events = search_events(path)
    return events[-1] if events else None


def fresh_event_path(work: Path, tag: str) -> Path:
    safe = "".join(ch if ch.isalnum() or ch in "-_." else "_" for ch in tag)
    path = work / "events"
    path.mkdir(parents=True, exist_ok=True)
    file = path / f"{safe}.ndjson"
    if file.exists():
        file.unlink(missing_ok=True)
    return file


# ---------------------------------------------------------------------------
# Misc
# ---------------------------------------------------------------------------

def percentile(values, p: float) -> float:
    if not values:
        return float("nan")
    ordered = sorted(values)
    k = min(len(ordered) - 1, int(round((p / 100.0) * (len(ordered) - 1))))
    return ordered[k]


def median(values) -> float:
    if not values:
        return float("nan")
    ordered = sorted(values)
    n = len(ordered)
    mid = n // 2
    if n % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2.0


def wait_for(predicate, timeout_s: float, interval_s: float = 0.05):
    """Poll ``predicate`` until it returns a truthy value or the timeout."""
    started = time.monotonic()
    while True:
        value = predicate()
        if value:
            return value, time.monotonic() - started
        if time.monotonic() - started >= timeout_s:
            return None, time.monotonic() - started
        time.sleep(interval_s)


def write_json(path: Path, value) -> None:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=True) + "\n", encoding="utf-8")


def write_bytes(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def git_init(path: Path) -> None:
    """Create a private Git repository with a bounded, checked subprocess."""
    for argv in (
        ["git", "init", "-q"],
        ["git", "config", "user.email", "wg-text@example.invalid"],
        ["git", "config", "user.name", "wg-text"],
    ):
        require(run(argv, cwd=path, timeout=60, allowed=(0,)))


def new_work(prefix: str, base: str | None = None) -> Path:
    """Create a fresh scratch directory. Never defaults to C: on Windows."""
    if base:
        root = Path(base)
    elif os.name == "nt":
        root = Path("E:/wg-text-scratch")
    else:
        root = Path("/srv/workspaces/wispd/wg-text-scratch")
    root.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    path = root / f"{prefix}-{stamp}-{os.getpid()}"
    path.mkdir(parents=True, exist_ok=False)
    return path
