#!/usr/bin/env python3
"""wg preview packaging helper.

Builds reproducible Windows/Linux PREVIEW archives for the `wg` crate at the
repository root from an explicit source export, with checksums, source
identity, license/provenance copies, and a clean-install smoke test.

This tool never commits, tags, uploads, or creates a release. It never builds
Rust. Binaries are read-only inputs given explicitly with ``--wg`` and
``--rg``. Model assets are downloaded at runtime by ``wg`` and are never
bundled.

Subcommands:
  export     select the root crate source into a clean export tree
  archive    assemble a PREVIEW archive from an export and explicit binaries
  smoke      run a tiny isolated-fixture smoke test against an installed wg
  self-test  exercise export/archive/smoke safety rules without a new binary
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as _dt
import hashlib
import json
import os
import re
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import zipfile
from pathlib import Path

TOOL = "scripts/wg_release.py"
TOOL_VERSION = "0.1.0"
LABEL = "PREVIEW"
NOT_OFFICIAL = (
    "PREVIEW packaging artifact. This is not an official published release. "
    "Commit, tag, upload and release creation remain separate explicit user actions."
)

REPO_ROOT = Path(__file__).resolve().parent.parent

EXPORT_PATHS = [
    "Cargo.toml",
    "Cargo.lock",
    "LICENSE",
    "README.md",
    "llms.txt",
    "BASELINE_PROVENANCE.md",
    "src",
    "tests",
    "vendor/notify",
    "third_party",
    "docs",
]
EXPORT_EXCLUDED_PATHS = ["docs/launch"]
THIRD_PARTY_LICENSES = ["LICENSE-MIT", "UNLICENSE", "COPYING"]

EXCLUDED_SEGMENTS = {
    "target",
    ".git",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
}
EXCLUDED_SUFFIXES = {
    ".pdb",
    ".rlib",
    ".rmeta",
    ".d",
    ".o",
    ".obj",
    ".exp",
    ".lib",
    ".ilk",
    ".pyc",
    ".log",
}
EXCLUDED_NAMES = {".cargo-ok", ".cargo-lock"}
RUNTIME_ROOTS = {".cache", "cache", "models", "cargo-home", "tmp", "temp", "logs", "out", "dist", "staging"}

SENSITIVE_NAMES = {
    ".env",
    ".netrc",
    ".npmrc",
    ".git-credentials",
    "credentials",
    "credentials.json",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
}
SENSITIVE_SUFFIXES = {".pem", ".key", ".pfx", ".p12", ".keystore", ".jks"}

LICENSE_NAME_RE = re.compile(
    r"^(license|licence|copying|copyright|unlicense|notice)([-._].*)?$", re.IGNORECASE
)

DEFAULT_WORK = Path("E:/wgrep-worktrees/wg-release-20261018")
DEFAULT_REGISTRY_CANDIDATES = [
    Path("E:/wgrep-worktrees/wg-up-20261018/cargo-home/registry/src"),
]

FILE_ATTRIBUTE_REPARSE_POINT = 0x400


class ReleaseError(Exception):
    def __init__(self, message, code=2):
        super().__init__(message)
        self.code = code


def utc_now():
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def is_reparse(path: Path) -> bool:
    try:
        st = os.lstat(path)
    except OSError:
        return False
    if stat.S_ISLNK(st.st_mode):
        return True
    attrs = getattr(st, "st_file_attributes", 0)
    return bool(attrs & FILE_ATTRIBUTE_REPARSE_POINT)


def rel_excluded(rel: Path) -> str | None:
    parts = set(rel.parts)
    if parts & EXCLUDED_SEGMENTS:
        return "excluded segment"
    if len(rel.parts) > 1 and rel.parts[0] in RUNTIME_ROOTS:
        return "runtime output root"
    if rel.suffix.lower() in EXCLUDED_SUFFIXES:
        return "excluded suffix"
    if rel.name in EXCLUDED_NAMES:
        return "excluded name"
    return None


def explicitly_excluded(rel: Path) -> bool:
    for value in EXPORT_EXCLUDED_PATHS:
        excluded = Path(value)
        if rel == excluded or rel.is_relative_to(excluded):
            return True
    return False


def sensitive_reason(rel: Path) -> str | None:
    name = rel.name.lower()
    if name in SENSITIVE_NAMES or name.startswith(".env"):
        return "sensitive name"
    if rel.suffix.lower() in SENSITIVE_SUFFIXES:
        return "sensitive suffix"
    return None


def iter_selection(repo: Path):
    selected = []
    for value in EXPORT_PATHS:
        root_rel = Path(value)
        root = repo / root_rel
        if root.is_file():
            if not explicitly_excluded(root_rel):
                selected.append((root, rel_norm(root_rel)))
            continue
        if not root.is_dir():
            raise ReleaseError(f"export path missing: {value}")
        for cur_dir, dir_names, file_names in os.walk(root, followlinks=False):
            cur = Path(cur_dir)
            kept = []
            for d in dir_names:
                dpath = cur / d
                drel = dpath.relative_to(repo)
                if explicitly_excluded(drel) or rel_excluded(drel):
                    continue
                if is_reparse(dpath):
                    raise ReleaseError(f"refusing to export symlink/reparse directory: {drel}")
                kept.append(d)
            dir_names[:] = kept
            for f in file_names:
                fpath = cur / f
                frel = fpath.relative_to(repo)
                if explicitly_excluded(frel) or rel_excluded(frel):
                    continue
                selected.append((fpath, rel_norm(frel)))
    selected.sort(key=lambda pair: pair[1])
    return selected


def rel_norm(p: Path) -> str:
    return str(p).replace(os.sep, "/")


def check_safe_sources(selected):
    for src, dest in selected:
        if is_reparse(src):
            raise ReleaseError(f"refusing to export symlink/reparse point: {dest}")
        reason = sensitive_reason(Path(dest))
        if reason:
            raise ReleaseError(f"refusing to export sensitive file ({reason}): {dest}")
        if not src.is_file():
            raise ReleaseError(f"not a regular file: {src}")


@dataclasses.dataclass
class ExportResult:
    out_dir: Path
    manifest_path: Path
    notice_path: Path
    export_id: str
    file_count: int
    manifest_map: dict


def copy_one(src: Path, dest: Path, mutate_hook=None):
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, dest)
    if mutate_hook is not None:
        mutate_hook(src, dest)


def export_tree(repo: Path, out: Path, force=False, mutate_hook=None) -> ExportResult:
    repo = repo.resolve()
    out = out.resolve()
    if not (repo / "Cargo.toml").is_file():
        raise ReleaseError(f"not a wg checkout (missing Cargo.toml): {repo}")
    if out.is_relative_to(repo):
        raise ReleaseError("source exports must be outside the checkout")
    if out.exists() and any(out.iterdir()) and not force:
        raise ReleaseError(f"output directory is not empty (use --force): {out}")

    selected = iter_selection(repo)
    check_safe_sources(selected)
    if force and out.exists():
        allowed = {dest for _src, dest in selected} | {"SOURCE_MANIFEST.sha256", "EXPORT_NOTICE.txt"}
        for existing in out.rglob("*"):
            if is_reparse(existing):
                raise ReleaseError("refusing to overwrite an export containing a reparse point")
            if existing.is_file() and rel_norm(existing.relative_to(out)) not in allowed:
                raise ReleaseError("forced export contains stale/unowned files; use a fresh output directory")

    before = {}
    for src, _dest in selected:
        st = src.stat()
        before[str(src)] = (st.st_size, st.st_mtime_ns, sha256_file(src))

    out.mkdir(parents=True, exist_ok=True)
    manifest_lines = []
    manifest_map = {}
    for src, dest_rel in selected:
        dest = out / dest_rel
        copy_one(src, dest, mutate_hook)
        digest = sha256_file(dest)
        if digest != before[str(src)][2]:
            raise ReleaseError(f"source changed during export (copy hash mismatch): {src}")
        manifest_map[dest_rel] = digest
        manifest_lines.append(f"{digest}  {dest_rel}")

    for src, _dest in selected:
        st = src.stat()
        if (st.st_size, st.st_mtime_ns, sha256_file(src)) != before[str(src)]:
            raise ReleaseError(
                f"source changed during export (mutated): {src}"
            )

    manifest_text = "\n".join(manifest_lines) + "\n"
    export_id = sha256_bytes(manifest_text.encode("utf-8"))
    manifest_path = out / "SOURCE_MANIFEST.sha256"
    manifest_path.write_text(manifest_text, encoding="utf-8", newline="\n")

    git_info = git_identity(repo)
    notice_path = out / "EXPORT_NOTICE.txt"
    notice_path.write_text(
        export_notice_text(repo, git_info, export_id, len(selected)),
        encoding="utf-8",
        newline="\n",
    )
    return ExportResult(
        out_dir=out,
        manifest_path=manifest_path,
        notice_path=notice_path,
        export_id=export_id,
        file_count=len(selected),
        manifest_map=manifest_map,
    )


def git_identity(repo: Path):
    info = {"toplevel": None, "head": None, "branch": None, "dirty": None,
            "changed_entries": None}
    try:
        top = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, timeout=30,
        )
        if top.returncode == 0:
            info["toplevel"] = top.stdout.strip()
        head = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "HEAD"],
            capture_output=True, text=True, timeout=30,
        )
        if head.returncode == 0:
            info["head"] = head.stdout.strip()
        branch = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True, text=True, timeout=30,
        )
        if branch.returncode == 0:
            info["branch"] = branch.stdout.strip()
        status = subprocess.run(
            ["git", "-C", str(repo), "status", "--porcelain", "--untracked-files=normal"],
            capture_output=True, text=True, timeout=60,
        )
        if status.returncode == 0:
            lines = [ln for ln in status.stdout.splitlines() if ln.strip()]
            info["changed_entries"] = len(lines)
            info["dirty"] = len(lines) > 0
    except (OSError, subprocess.SubprocessError):
        pass
    return info


def export_notice_text(repo, git_info, export_id, file_count):
    lines = [
        "wg source export notice",
        "=======================",
        "",
        f"created_utc: {utc_now()}",
        f"tool: {TOOL} {TOOL_VERSION}",
        f"repo: {repo}",
        f"selected_files: {file_count}",
        f"export_id: {export_id}",
        "",
        "SELECTION",
        "  included roots:",
    ]
    lines.extend(f"    - {value}" for value in EXPORT_PATHS)
    lines.extend(
        [
            "  explicitly excluded paths:",
        ]
    )
    lines.extend(f"    - {value}" for value in EXPORT_EXCLUDED_PATHS)
    lines.extend(
        [
            "  excluded: target/, .git/, node_modules/, __pycache__/, build",
            "    artifacts (*.pdb, *.rlib, *.rmeta, *.d, *.o, *.obj, *.exp,",
            "    *.lib, *.ilk, *.pyc, *.log), .cargo-ok, .cargo-lock, and any",
            "    symlink/reparse point or sensitive file name.",
            "",
            "SOURCE IDENTITY",
            f"  git_toplevel: {git_info.get('toplevel')}",
            f"  git_head: {git_info.get('head')}",
            f"  git_branch: {git_info.get('branch')}",
            f"  git_dirty: {git_info.get('dirty')}",
            f"  git_changed_entries: {git_info.get('changed_entries')}",
            "",
        ]
    )
    if git_info.get("dirty"):
        lines.append(
            "  NOTICE: this export is from a DIRTY working tree. It is NOT a"
        )
        lines.append(
            "  clean-commit release. Do not label it as an official release."
        )
    elif git_info.get("dirty") is False:
        lines.append(
            "  NOTICE: git reported a clean working tree at export time."
        )
    else:
        lines.append("  NOTICE: git status was unavailable; no clean-commit claim is made.")
    lines.append("")
    return "\n".join(lines) + "\n"


def detect_format(path: Path):
    with open(path, "rb") as f:
        head = f.read(4)
    if head[:2] == b"MZ":
        return "PE"
    if head[:4] == b"\x7fELF":
        return "ELF"
    return "unknown"


def pe_arch(path: Path):
    try:
        with open(path, "rb") as f:
            if f.read(2) != b"MZ":
                return None
            f.seek(0x3C)
            e_lfanew = struct.unpack("<I", f.read(4))[0]
            f.seek(e_lfanew)
            if f.read(4) != b"PE\x00\x00":
                return None
            machine = struct.unpack("<H", f.read(2))[0]
    except (OSError, struct.error):
        return None
    return {0x8664: "x64", 0xAA64: "arm64", 0x14C: "x86"}.get(machine)


def elf_arch(path: Path):
    try:
        with open(path, "rb") as f:
            if f.read(4) != b"\x7fELF":
                return None
            f.seek(18)
            machine = struct.unpack("<H", f.read(2))[0]
    except (OSError, struct.error):
        return None
    return {0x3E: "x64", 0xB7: "arm64", 0x03: "x86"}.get(machine)


def platform_of(path: Path):
    fmt = detect_format(path)
    if fmt == "PE":
        return "windows-" + (pe_arch(path) or "unknown")
    if fmt == "ELF":
        return "linux-" + (elf_arch(path) or "unknown")
    return "unknown"


def run_capture(argv, cwd=None, env=None, timeout=30):
    try:
        if os.name == "nt" and Path(argv[0]).suffix.lower() in (".cmd", ".bat"):
            argv = [os.environ.get("COMSPEC", r"C:\Windows\System32\cmd.exe"), "/d", "/c", "call", *argv]
        proc = subprocess.run(
            argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout
        )
        return proc.returncode, proc.stdout, proc.stderr
    except (OSError, subprocess.SubprocessError) as exc:
        return None, b"", str(exc).encode()


def binary_version(path: Path):
    code, out, err = run_capture([str(path), "--version"], timeout=15)
    if code != 0:
        return None, "", (err or out).decode("utf-8", "replace").strip()
    line = out.decode("utf-8", "replace").strip().splitlines()[0] if out else ""
    return 0, line, ""


def parse_wg_version(version_line: str):
    m = re.search(r"\b(\d+\.\d+\.\d+[^\s]*)", version_line or "")
    return m.group(1) if m else None


def resolve_registry(explicit):
    if explicit:
        return Path(explicit)
    for cand in DEFAULT_REGISTRY_CANDIDATES:
        if cand.is_dir():
            return cand
    cargo_home = os.environ.get("CARGO_HOME")
    if cargo_home:
        cand = Path(cargo_home) / "registry" / "src"
        if cand.is_dir():
            return cand
    return None


def cargo_lock_packages(lock_path: Path):
    packages = []
    name = version = None
    section = False
    for line in lock_path.read_text(encoding="utf-8", errors="replace").splitlines():
        s = line.strip()
        if s.startswith("[[package]]"):
            if name and version:
                packages.append((name, version))
            name = version = None
            section = True
            continue
        if s.startswith("[") and not s.startswith("[[package]]"):
            if name and version:
                packages.append((name, version))
            name = version = None
            section = False
            continue
        if section and s.startswith("name = "):
            name = s.split("=", 1)[1].strip().strip('"')
        elif section and s.startswith("version = "):
            version = s.split("=", 1)[1].strip().strip('"')
    if name and version:
        packages.append((name, version))
    return packages


def collect_dependency_licenses(lock_path: Path, registry_src, dest_dir: Path):
    dest_dir.mkdir(parents=True, exist_ok=True)
    packages = cargo_lock_packages(lock_path)
    index = [
        "Dependency license notices",
        "===========================",
        "",
        f"created_utc: {utc_now()}",
        f"cargo_lock: {lock_path}",
        f"registry_src: {registry_src}",
        f"packages_in_lock: {len(packages)}",
        "",
        "These files are copied verbatim from the local Cargo registry source",
        "cache that resolved this lockfile. No license text is invented. A",
        "package is listed as 'unavailable' when its source copy is missing.",
        "",
    ]
    copied = 0
    unavailable = []
    for name, version in sorted(set(packages)):
        src_dir = None
        if registry_src:
            base = Path(registry_src)
            for index_dir in sorted(base.iterdir()) if base.is_dir() else []:
                cand = index_dir / f"{name}-{version}"
                if cand.is_dir():
                    src_dir = cand
                    break
        if src_dir is None:
            unavailable.append(f"{name}-{version}")
            continue
        found = [
            p for p in sorted(src_dir.iterdir())
            if p.is_file() and LICENSE_NAME_RE.match(p.name)
        ]
        if not found:
            unavailable.append(f"{name}-{version}")
            continue
        crate_dest = dest_dir / f"{name}-{version}"
        crate_dest.mkdir(parents=True, exist_ok=True)
        for p in found:
            shutil.copyfile(p, crate_dest / p.name)
        index.append(f"  {name}-{version}: " + ", ".join(p.name for p in found))
        copied += 1
    index.append("")
    index.append(f"packages_with_license_copied: {copied}")
    index.append(f"packages_unavailable: {len(unavailable)}")
    if unavailable:
        index.append("")
        index.append("unavailable (no local registry source copy; not fabricated):")
        for item in unavailable:
            index.append(f"  {item}")
    index.append("")
    (dest_dir.parent / "DEPENDENCY_LICENSES.txt").write_text(
        "\n".join(index), encoding="utf-8", newline="\n"
    )
    return copied, len(unavailable)


WINDOWS_LAUNCHER = (
    "@echo off\r\n"
    "setlocal\r\n"
    "set \"WG_ROOT=%~dp0\"\r\n"
    "set \"WG_RG=%WG_ROOT%bin\\rg.exe\"\r\n"
    "set \"PATH=%WG_ROOT%bin;%PATH%\"\r\n"
    "\"%WG_ROOT%bin\\wg.exe\" %*\r\n"
    "exit /b %ERRORLEVEL%\r\n"
)

LINUX_LAUNCHER = (
    "#!/bin/sh\n"
    "case $0 in */*) base=${0%/*} ;; *) base=. ;; esac\n"
    "here=$(CDPATH= cd -- \"$base\" && pwd)\n"
    "WG_RG=\"$here/bin/rg\" PATH=\"$here/bin:$PATH\" exec \"$here/bin/wg\" \"$@\"\n"
)


def tree_add(tf, path: Path, arcname: str, mode=0o644):
    info = tf.gettarinfo(str(path), arcname=arcname)
    info.mode = mode
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    info.mtime = int(path.stat().st_mtime)
    with open(path, "rb") as f:
        tf.addfile(info, f)


def zip_add(zf, path: Path, arcname: str, mode=0o644):
    zi = zipfile.ZipInfo(arcname)
    zi.external_attr = (mode & 0xFFFF) << 16
    zi.compress_type = zipfile.ZIP_DEFLATED
    zi.date_time = _dt.datetime.fromtimestamp(path.stat().st_mtime).timetuple()[:6]
    with open(path, "rb") as f:
        zf.writestr(zi, f.read())


def _source_copy_ignore(export_dir: Path):
    root = export_dir.resolve()

    def ignore(directory, names):
        if Path(directory).resolve() != root:
            return []
        return [n for n in names if n in ("SOURCE_MANIFEST.sha256", "EXPORT_NOTICE.txt")]

    return ignore


def stage_archive(binary_wg: Path, binary_rg: Path, export, platform, version,
                  build_id, binary_status, work: Path, cargo_registry=None,
                  label=LABEL):
    stage_root = Path(tempfile.mkdtemp(prefix="wg-stage-", dir=str(work)))
    root_name = f"wg-{version}-{platform}-{label}-{build_id}"
    root = stage_root / root_name
    (root / "bin").mkdir(parents=True)
    (root / "docs").mkdir()
    (root / "licenses").mkdir()
    (root / "PROVENANCE").mkdir()

    wg_name = "wg.exe" if platform.startswith("windows") else "wg"
    rg_name = "rg.exe" if platform.startswith("windows") else "rg"
    shutil.copyfile(binary_wg, root / "bin" / wg_name)
    shutil.copyfile(binary_rg, root / "bin" / rg_name)

    core = export.out_dir
    # The README is written for a package root: it links `docs/...`. Keep it at
    # the archive root so those links resolve, and ship the media it embeds.
    shutil.copyfile(core / "README.md", root / "README.md")
    for media_dir in (core / "docs" / "media", core / "docs" / "demos"):
        if media_dir.is_dir():
            shutil.copytree(media_dir, root / "docs" / media_dir.name)
    agent_guide = core / "docs" / "agent-guide.md"
    if not agent_guide.is_file():
        agent_guide = core / "docs" / "agents.md"
    if not agent_guide.is_file():
        raise ReleaseError("core agent guide missing (docs/agent-guide.md)")
    shutil.copyfile(agent_guide, root / "docs" / agent_guide.name)
    shutil.copyfile(core / "docs" / "benchmarks.md", root / "docs" / "benchmarks.md")
    shutil.copyfile(
        core / "BASELINE_PROVENANCE.md", root / "docs" / "BASELINE_PROVENANCE.md"
    )
    shutil.copyfile(core / "llms.txt", root / "docs" / "llms.txt")
    shutil.copyfile(core / "LICENSE", root / "licenses" / "sgrep-LICENSE")
    shutil.copyfile(
        core / "vendor" / "notify" / "LICENSE-CC0",
        root / "licenses" / "notify-LICENSE-CC0",
    )
    for name in THIRD_PARTY_LICENSES:
        shutil.copyfile(
            core / "third_party" / "ripgrep" / name,
            root / "licenses" / f"ripgrep-{name}",
        )

    registry = resolve_registry(cargo_registry)
    copied, unavailable = collect_dependency_licenses(
        core / "Cargo.lock", registry, root / "licenses" / "deps"
    )

    shutil.copytree(core, root / "source", ignore=_source_copy_ignore(core))
    shutil.copyfile(
        export.manifest_path, root / "PROVENANCE" / "SOURCE_MANIFEST.sha256"
    )
    shutil.copyfile(
        export.notice_path, root / "PROVENANCE" / "EXPORT_NOTICE.txt"
    )

    if platform.startswith("windows"):
        (root / "wg.cmd").write_text(WINDOWS_LAUNCHER, encoding="utf-8", newline="")
    else:
        lp = root / "wg"
        lp.write_text(LINUX_LAUNCHER, encoding="utf-8", newline="\n")
        lp.chmod(0o755)

    wg_sha = sha256_file(binary_wg)
    rg_sha = sha256_file(binary_rg)
    _, wg_vline, _ = binary_version(binary_wg)
    rg_code, rg_vline, _ = binary_version(binary_rg)

    build_info = {
        "label": label,
        "not_official_release": label != "RELEASE",
        "note": NOT_OFFICIAL if label != "RELEASE" else (
            "Release packaging artifact. The draft release must pass review "
            "before it is published."
        ),
        "package": "wg",
        "version": version,
        "build_id": build_id,
        "platform": platform,
        "created_utc": utc_now(),
        "tool": TOOL,
        "tool_version": TOOL_VERSION,
        "binary_status": binary_status,
        "source": {
            "export_id": export.export_id,
            "manifest": "PROVENANCE/SOURCE_MANIFEST.sha256",
            "notice": "PROVENANCE/EXPORT_NOTICE.txt",
            "selected_files": export.file_count,
            "git": git_identity(REPO_ROOT),
        },
        "binaries": {
            "wg": {
                "path": f"bin/{wg_name}",
                "sha256": wg_sha,
                "format": detect_format(binary_wg),
                "version_line": wg_vline,
            },
            "rg": {
                "path": f"bin/{rg_name}",
                "sha256": rg_sha,
                "format": detect_format(binary_rg),
                "version_line": (rg_vline or "").splitlines()[0] if rg_vline else "",
            },
        },
        "dependency_licenses": {
            "registry_src": str(registry) if registry else None,
            "packages_copied": copied,
            "packages_unavailable": unavailable,
        },
        "rg_discovery": (
            "The bundled launcher sets WG_RG to bin/rg and prepends bin to PATH. "
            "Running bin/wg directly still requires rg on PATH or WG_RG set."
        ),
    }
    (root / "PROVENANCE" / "BUILD_INFO.json").write_text(
        json.dumps(build_info, indent=2, sort_keys=True) + "\n",
        encoding="utf-8", newline="\n",
    )

    sums = []
    for path in sorted(root.rglob("*")):
        if path.is_file() and path.name != "SHA256SUMS":
            arc = rel_norm(path.relative_to(root))
            sums.append(f"{sha256_file(path)}  {arc}")
    (root / "PROVENANCE" / "SHA256SUMS").write_text(
        "\n".join(sums) + "\n", encoding="utf-8", newline="\n"
    )
    return stage_root, root_name, build_info


def pack_archive(root: Path, root_name: str, out_file: Path, platform: str):
    if platform.startswith("windows"):
        with zipfile.ZipFile(out_file, "w", zipfile.ZIP_DEFLATED) as zf:
            for path in sorted(root.rglob("*")):
                if not path.is_file():
                    continue
                arc = f"{root_name}/" + rel_norm(path.relative_to(root))
                mode = 0o755 if (path.suffix == "" and path.name in ("wg",)) else 0o644
                zip_add(zf, path, arc, mode)
    else:
        with tarfile.open(out_file, "w:gz", format=tarfile.PAX_FORMAT) as tf:
            for path in sorted(root.rglob("*")):
                if not path.is_file():
                    continue
                arc = f"{root_name}/" + rel_norm(path.relative_to(root))
                executable = path.parent.name == "bin" or (
                    path.parent == root and path.name == "wg"
                )
                mode = 0o755 if executable else 0o644
                tree_add(tf, path, arc, mode)


def cmd_export(args):
    repo = Path(args.repo).resolve()
    out = Path(args.out).resolve()
    result = export_tree(repo, out, force=args.force)
    if args.json:
        print(json.dumps({
            "ok": True,
            "out_dir": str(result.out_dir),
            "export_id": result.export_id,
            "selected_files": result.file_count,
            "manifest": str(result.manifest_path),
            "notice": str(result.notice_path),
        }, indent=2))
    else:
        print(f"export ok: {result.file_count} files")
        print(f"export_id: {result.export_id}")
        print(f"out: {result.out_dir}")
    return 0


def cmd_archive(args):
    work = Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    binary_wg = Path(args.wg)
    binary_rg = Path(args.rg)
    if not binary_wg.is_file():
        raise ReleaseError(f"--wg not found: {binary_wg}")
    if not binary_rg.is_file():
        raise ReleaseError(f"--rg not found: {binary_rg}")

    platform = args.platform
    wg_platform = platform_of(binary_wg)
    rg_platform = platform_of(binary_rg)
    if platform == "auto":
        platform = wg_platform
    if platform == "unknown":
        raise ReleaseError(f"cannot detect binary format for --wg: {binary_wg}")
    if detect_format(binary_wg) not in ("PE", "ELF"):
        raise ReleaseError(f"--wg is not a PE/ELF binary: {binary_wg}")
    if wg_platform != platform:
        raise ReleaseError(
            f"--wg format {wg_platform} does not match platform label {platform}"
        )
    if rg_platform != platform:
        raise ReleaseError(
            f"--rg format {rg_platform} does not match platform label {platform}"
        )
    rg_code, rg_vline, _ = binary_version(binary_rg)
    if not getattr(args, "allow_fixture_binaries", False):
        if not (rg_vline or "").startswith("ripgrep 15.1"):
            raise ReleaseError(
                f"--rg is not ripgrep 15.1 (got: {rg_vline or 'unreadable'})"
            )

    if args.export:
        export = dataclasses.replace(_load_export(Path(args.export)), out_dir=Path(args.export).resolve())
    else:
        tmp_export = Path(tempfile.mkdtemp(prefix="wg-export-", dir=str(work)))
        export = export_tree(Path(args.repo).resolve(), tmp_export)

    version = args.version
    if not version:
        _, wg_vline, _ = binary_version(binary_wg)
        version = parse_wg_version(wg_vline) or _core_version(export.out_dir) or "0.0.0-preview"
    build_id = args.build_id or export.export_id[:12]
    label = getattr(args, "label", LABEL)
    ext = "zip" if platform.startswith("windows") else "tar.gz"
    out_file = Path(args.out).resolve() if args.out else (
        work / "out" / f"wg-{version}-{platform}-{label}-{build_id}.{ext}"
    )
    if out_file.exists() and (args.force or out_file.stat().st_size == 0) is False:
        raise ReleaseError(f"output archive exists (use --force): {out_file}")
    out_file.parent.mkdir(parents=True, exist_ok=True)

    stage_root, root_name, build_info = stage_archive(
        binary_wg, binary_rg, export, platform, version, build_id,
        args.binary_status, work, args.cargo_registry, label=label,
    )
    try:
        pack_archive(stage_root / root_name, root_name, out_file, platform)
    finally:
        shutil.rmtree(stage_root, ignore_errors=True)

    archive_sha = sha256_file(out_file)
    sidecar = out_file.with_suffix(out_file.suffix + ".sha256")
    sidecar.write_text(f"{archive_sha}  {out_file.name}\n", encoding="utf-8", newline="\n")
    if args.json:
        print(json.dumps({
            "ok": True,
            "archive": str(out_file),
            "sha256": archive_sha,
            "sidecar": str(sidecar),
            "build_info": build_info,
        }, indent=2, sort_keys=True))
    else:
        print(f"archive ok: {out_file.name}")
        print(f"label: {label}")
        print(f"sha256: {archive_sha}")
        print(f"binary_status: {args.binary_status}")
    return 0


def _load_export(out_dir: Path):
    manifest_path = out_dir / "SOURCE_MANIFEST.sha256"
    notice_path = out_dir / "EXPORT_NOTICE.txt"
    if not manifest_path.is_file():
        raise ReleaseError(f"export manifest missing: {manifest_path}")
    manifest_map = {}
    for line in manifest_path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        digest, _, rel = line.partition("  ")
        manifest_map[rel] = digest
    return ExportResult(
        out_dir=out_dir,
        manifest_path=manifest_path,
        notice_path=notice_path,
        export_id=sha256_bytes(manifest_path.read_bytes()),
        file_count=len(manifest_map),
        manifest_map=manifest_map,
    )


def _core_version(export_dir: Path):
    cargo = export_dir / "Cargo.toml"
    if not cargo.is_file():
        return None
    for line in cargo.read_text(encoding="utf-8", errors="replace").splitlines():
        if line.strip().startswith("version") and "=" in line:
            return line.split("=", 1)[1].strip().strip('"')
    return None


def sanitized_path_without_rg(path_value: str):
    exe_names = ("rg", "rg.exe")
    kept = []
    dropped = []
    for entry in path_value.split(os.pathsep):
        if not entry:
            continue
        d = Path(entry)
        if any((d / n).is_file() for n in exe_names):
            dropped.append(entry)
            continue
        kept.append(entry)
    return os.pathsep.join(kept), dropped


def cmd_smoke(args):
    wg = Path(args.wg).resolve()
    if not wg.is_file():
        raise ReleaseError(f"--wg not found: {wg}")
    rg = Path(args.rg).resolve() if args.rg else None
    if rg is not None and not rg.is_file():
        raise ReleaseError(f"--rg not found: {rg}")

    work = Path(args.work).resolve()
    if work.exists() and any(work.iterdir()):
        raise ReleaseError("smoke work directory must be empty; existing data will not be deleted")
    work.mkdir(parents=True, exist_ok=True)
    fixture = work / "fixture"
    service = work / "service"
    cache = work / "cache"
    tmp = work / "tmp"
    for p in (fixture, service, cache, tmp):
        p.mkdir()

    (fixture / "keep").mkdir()
    (fixture / "sub").mkdir()
    (fixture / "keep" / "a.rs").write_text(
        'pub fn find_needle() -> bool { true }\n', encoding="utf-8", newline="\n"
    )
    (fixture / "sub" / "c.rs").write_text(
        'pub fn needle_in_sub() -> bool { false }\n', encoding="utf-8", newline="\n"
    )
    (fixture / ".ignore").write_text("skip/\n", encoding="utf-8", newline="\n")
    (fixture / "skip").mkdir()
    (fixture / "skip" / "hidden.rs").write_text("// needle DO_NOT_SHOW\n", encoding="utf-8")

    env = dict(os.environ)
    for key in ("WG_RG", "RIPGREP_CONFIG_PATH", "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        env.pop(key, None)
    env["WG_SERVICE_DIR"] = str(service)
    env["SGREP_CACHE_DIR"] = str(cache / "sgrep")
    env["TMPDIR"] = env["TMP"] = env["TEMP"] = str(tmp)
    env["HF_HOME"] = str(cache / "huggingface")
    env["HF_HUB_CACHE"] = str(cache / "huggingface" / "hub")
    env["HUGGINGFACE_HUB_CACHE"] = env["HF_HUB_CACHE"]
    env["HOME"] = str(work / "home")
    env["USERPROFILE"] = str(work / "home")
    env["LOCALAPPDATA"] = str(work / "home" / "AppData" / "Local")
    env["XDG_CACHE_HOME"] = str(work / "home" / ".cache")
    cache_args = []
    if args.warm_cache:
        warm = Path(args.warm_cache).resolve()
        hub = warm / "hub"
        env["HF_HUB_CACHE"] = str(hub if hub.is_dir() else warm)
        env["HUGGINGFACE_HUB_CACHE"] = env["HF_HUB_CACHE"]
        env["HF_HUB_OFFLINE"] = "1"
        # Use the same directory for model and parser assets so the smoke is
        # deterministic and offline. Without it the language pack may attempt
        # its own download into a second cache.
        cache_args = ["--cache-dir", str(warm)]
        if not (warm / "tree-sitter-language-pack").is_dir():
            raise ReleaseError("--warm-cache lacks a tree-sitter-language-pack cache")
    if rg is not None:
        env["WG_RG"] = str(rg)
    env["PATH"], dropped = sanitized_path_without_rg(env.get("PATH", ""))
    if rg is not None:
        env["PATH"] = str(rg.resolve().parent) + os.pathsep + env["PATH"]

    steps = []

    def record(name, argv, code, out, err, expect):
        steps.append({
            "name": name,
            "argv": argv,
            "exit": code,
            "expect": sorted(expect),
            "ok": code in expect,
            "stdout_head": out[:400].decode("utf-8", "replace"),
            "stderr_head": err[:400].decode("utf-8", "replace"),
        })

    up_ok = False
    try:
        up_argv = [str(wg), *cache_args, "up", str(fixture), "--json"]
        code, out, err = run_capture(up_argv, cwd=fixture, env=env, timeout=args.timeout)
        record("up --json", up_argv, code, out, err, {0})
        up_ok = code == 0
        if up_ok:
            try:
                doc = json.loads(out.decode("utf-8"))
                up_ok = (isinstance(doc, dict) and doc.get("ok") is True
                         and doc.get("root", {}).get("ready") is True
                         and doc["root"].get("pending") == 0 and not err)
                steps[-1]["ok"] = up_ok
            except (ValueError, UnicodeDecodeError, AttributeError):
                up_ok = False
                steps[-1]["ok"] = False
            if not up_ok:
                steps[-1]["note"] = "up did not return clean ready JSON"

        if up_ok:
            calls = [
                ("status --json", ["status", "--json"], {0}),
                ("text query -F", ["-F", "needle", "."], {0}),
                ("text no-match exit", ["-F", "WgNoMatchSentinelC903x", "."], {1}),
                ("text error exit", ["-e", "(", "."], {2}),
                ("semantic query", ["needle", "--json", "--top", "2"], {0}),
                ("files", ["--files"], {0}),
            ]
            for name, tail, expect in calls:
                argv = [str(wg), *cache_args, *tail]
                code, out, err = run_capture(argv, cwd=fixture, env=env, timeout=args.timeout)
                record(name, argv, code, out, err, expect)
                if name == "status --json" and code == 0:
                    try:
                        doc = json.loads(out)
                        steps[-1]["ok"] = not err and doc.get("ok") is True and len(doc.get("roots", [])) == 1
                    except (ValueError, AttributeError):
                        steps[-1]["ok"] = False
                elif name == "semantic query" and code == 0:
                    try:
                        rows = json.loads(out)
                        steps[-1]["ok"] = (not err and isinstance(rows, list) and bool(rows)
                            and all("needle" in r.get("content", "") for r in rows)
                            and all("hidden.rs" not in r.get("path", "") for r in rows))
                    except (ValueError, AttributeError):
                        steps[-1]["ok"] = False
                elif name == "text query -F" and code == 0:
                    steps[-1]["ok"] = b"needle" in out and b"DO_NOT_SHOW" not in out
                elif name == "files" and code == 0:
                    steps[-1]["ok"] = b"a.rs" in out and b"c.rs" in out and b"hidden.rs" not in out
    finally:
        if (service / "service.json").exists() or up_ok:
            down_argv = [str(wg), *cache_args, "down"]
            code, out, err = run_capture(down_argv, cwd=fixture, env=env,
                                        timeout=min(args.timeout, 30))
            record("down", down_argv, code, out, err, {0})
            until = time.monotonic() + 5
            while (service / "service.json").exists() and time.monotonic() < until:
                time.sleep(.01)
            if (service / "service.json").exists():
                steps[-1]["ok"] = False
                steps[-1]["note"] = "service metadata remained after cleanup"

    overall = up_ok and all(s["ok"] for s in steps)
    summary = {
        "ok": overall,
        "wg": str(wg),
        "rg": str(rg) if rg else None,
        "warm_cache": str(args.warm_cache) if args.warm_cache else None,
        "isolated_dirs": {
            "fixture": str(fixture), "service": str(service),
            "cache": str(cache), "tmp": str(tmp),
        },
        "path_dirs_dropped_for_rg": dropped,
        "steps": steps,
        "note": (
            "Smoke tests the installed wg with system rg removed from PATH and "
            "isolated caches. No result is reported as a pass unless every step "
            "really ran."
        ),
    }
    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        for s in steps:
            mark = "PASS" if s["ok"] else "FAIL"
            print(f"[{mark}] {s['name']} (exit={s['exit']} expect={sorted(s['expect'])})")
            if not s["ok"] and s.get("note"):
                print(f"       {s['note']}")
        print(f"smoke overall: {'PASS' if overall else 'FAIL'}")
    return 0 if overall else 2


class SelfTest:
    def __init__(self, work: Path):
        self.work = work
        self.results = []

    def check(self, name, fn):
        try:
            outcome = fn()
        except Exception as exc:  # noqa: BLE001
            outcome = ("FAIL", f"unexpected error: {exc!r}")
        if outcome is None:
            outcome = ("PASS", "")
        status, detail = outcome
        self.results.append((name, status, detail))
        return status == "PASS"

    def report(self, as_json):
        ok = all(s == "PASS" for _, s, _ in self.results)
        if as_json:
            print(json.dumps({
                "ok": ok,
                "results": [{"name": n, "status": s, "detail": d}
                            for n, s, d in self.results],
            }, indent=2))
        else:
            for name, status, detail in self.results:
                line = f"[{status}] {name}"
                if detail:
                    line += f": {detail}"
                print(line)
            print(f"self-test overall: {'PASS' if ok else 'FAIL'}")
        return 0 if ok else 2


def make_mini_repo(base: Path, symlink=False, sensitive=False):
    (base / "src").mkdir(parents=True)
    (base / "tests").mkdir()
    (base / "docs").mkdir()
    (base / "docs" / "launch").mkdir()
    (base / "target").mkdir()
    (base / "__pycache__").mkdir()
    (base / "Cargo.toml").write_text(
        '[package]\nname = "wgrep"\nversion = "9.9.9"\n', encoding="utf-8"
    )
    (base / "Cargo.lock").write_text(
        '[[package]]\nname = "memmap2"\nversion = "0.9.9"\n\n'
        '[[package]]\nname = "example"\nversion = "1.0.0"\n',
        encoding="utf-8",
    )
    (base / "src" / "lib.rs").write_text("pub fn x() {}\n", encoding="utf-8")
    (base / "src" / "bin").mkdir()
    (base / "src" / "bin" / "wg.rs").write_text("fn main() {}\n", encoding="utf-8")
    (base / "tests" / "smoke.rs").write_text("// test\n", encoding="utf-8")
    (base / "README.md").write_text("# test\n", encoding="utf-8")
    (base / "docs" / "agent-guide.md").write_text("# agents\n", encoding="utf-8")
    (base / "docs" / "benchmarks.md").write_text("# bench\n", encoding="utf-8")
    (base / "docs" / "release-protocol.md").write_text("# protocol\n", encoding="utf-8")
    (base / "docs" / "launch" / "private.md").write_text("# private\n", encoding="utf-8")
    (base / "BASELINE_PROVENANCE.md").write_text("# prov\n", encoding="utf-8")
    (base / "llms.txt").write_text("llms\n", encoding="utf-8")
    (base / "LICENSE").write_text("MIT test\n", encoding="utf-8")
    (base / "target" / "junk.bin").write_bytes(b"build junk")
    (base / "__pycache__" / "x.pyc").write_bytes(b"pyc")
    notify = base / "vendor" / "notify"
    (notify / "src").mkdir(parents=True)
    (notify / "target").mkdir()
    (notify / "src" / "lib.rs").write_text("pub fn n() {}\n", encoding="utf-8")
    (notify / "LICENSE-CC0").write_text("CC0 test\n", encoding="utf-8")
    (notify / "target" / "junk").write_bytes(b"x")
    ripgrep = base / "third_party" / "ripgrep"
    ripgrep.mkdir(parents=True)
    (ripgrep / "LICENSE-MIT").write_text("MIT test\n", encoding="utf-8")
    (ripgrep / "UNLICENSE").write_text("unlicense test\n", encoding="utf-8")
    (ripgrep / "COPYING").write_text("copying test\n", encoding="utf-8")
    if symlink:
        (base / "src" / "link.rs").symlink_to(base / "src" / "lib.rs")
    if sensitive:
        (base / "src" / ".env").write_text("SECRET=1\n", encoding="utf-8")
    return base


def fake_binary(path: Path, kind: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    if kind == "PE":
        data = bytearray(512)
        data[0:2] = b"MZ"
        struct.pack_into("<I", data, 0x3C, 0x40)
        data[0x40:0x44] = b"PE\x00\x00"
        struct.pack_into("<H", data, 0x44, 0x8664)
        path.write_bytes(bytes(data))
    elif kind == "ELF":
        data = bytearray(256)
        data[0:4] = b"\x7fELF"
        struct.pack_into("<H", data, 18, 0x3E)
        path.write_bytes(bytes(data))
    else:
        path.write_bytes(b"unknown")
    return path


def run_self_test(args):
    work = Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    st = SelfTest(work)
    sandbox = Path(tempfile.mkdtemp(prefix="wg-self-", dir=str(work)))

    def case_export_excludes():
        repo = make_mini_repo(sandbox / "r1")
        out = sandbox / "o1"
        res = export_tree(repo, out)
        assert (out / "Cargo.toml").is_file(), "Cargo.toml missing at export root"
        assert (out / "src" / "lib.rs").is_file(), "lib.rs missing"
        assert (out / "src" / "bin" / "wg.rs").is_file(), "wg.rs missing"
        assert (out / "tests" / "smoke.rs").is_file(), "tests missing"
        assert (out / "vendor" / "notify" / "src" / "lib.rs").is_file(), "notify missing"
        assert (out / "third_party" / "ripgrep" / "COPYING").is_file(), "third_party missing"
        assert not (out / "target").exists(), "target not excluded"
        assert not (out / "__pycache__").exists(), "__pycache__ not excluded"
        assert not (out / "docs" / "launch").exists(), "docs/launch not excluded"
        assert res.file_count > 0
        return ("PASS", f"{res.file_count} files")

    def case_manifest():
        repo = make_mini_repo(sandbox / "r2")
        out = sandbox / "o2"
        res = export_tree(repo, out)
        for rel, digest in res.manifest_map.items():
            assert sha256_file(out / rel) == digest, f"manifest mismatch {rel}"
        assert sha256_bytes(res.manifest_path.read_bytes()) == res.export_id
        return ("PASS", f"{len(res.manifest_map)} hashed paths")

    def case_symlink():
        out = sandbox / "o3"
        try:
            repo = make_mini_repo(sandbox / "r3", symlink=True)
        except OSError as exc:
            return ("SKIP", f"cannot create symlink on this host: {exc}")
        try:
            export_tree(repo, out)
        except ReleaseError as exc:
            return ("PASS", f"refused symlink: {exc}")
        return ("FAIL", "symlink export was not refused")

    def case_mutation():
        repo = make_mini_repo(sandbox / "r4")
        out = sandbox / "o4"

        def hook(src, dest):
            with open(src, "a", encoding="utf-8") as f:
                f.write("mutated\n")

        try:
            export_tree(repo, out, mutate_hook=hook)
        except ReleaseError as exc:
            return ("PASS", f"refused mutated export: {exc}")
        return ("FAIL", "source mutation during export was not detected")

    def case_sensitive():
        repo = make_mini_repo(sandbox / "r5", sensitive=True)
        out = sandbox / "o5"
        try:
            export_tree(repo, out)
        except ReleaseError as exc:
            return ("PASS", f"refused sensitive file: {exc}")
        return ("FAIL", "sensitive file export was not refused")

    def case_preserved_metadata_mutation():
        repo = make_mini_repo(sandbox / "r-preserved")
        def hook(src, _dest):
            before = src.stat()
            old = src.read_bytes()
            src.write_bytes(bytes(b ^ 1 for b in old))
            os.utime(src, ns=(before.st_atime_ns, before.st_mtime_ns))
        try:
            export_tree(repo, sandbox / "o-preserved", mutate_hook=hook)
        except ReleaseError as exc:
            return ("PASS", str(exc))
        return ("FAIL", "same-size restored-mtime mutation escaped hash verification")

    def case_directory_symlink():
        repo = make_mini_repo(sandbox / "r-dirlink")
        try:
            (repo / "src" / "linked").symlink_to(repo / "src", target_is_directory=True)
        except OSError as exc:
            return ("SKIP", str(exc))
        try:
            export_tree(repo, sandbox / "o-dirlink")
        except ReleaseError as exc:
            return ("PASS", str(exc))
        return ("FAIL", "directory symlink was silently skipped instead of refused")

    def case_runtime_roots():
        repo = make_mini_repo(sandbox / "r-runtime")
        for name in RUNTIME_ROOTS:
            (repo / name).mkdir()
            (repo / name / "runtime-data.bin").write_bytes(b"not source")
        (repo / "src" / "models").mkdir()
        (repo / "src" / "models" / "real.rs").write_text("// legitimate nested source\n")
        exported = export_tree(repo, sandbox / "o-runtime")
        assert any(p.endswith("src/models/real.rs") for p in exported.manifest_map)
        assert not any(p.endswith("runtime-data.bin") for p in exported.manifest_map)
        assert "git reported a clean" not in exported.notice_path.read_text()
        return ("PASS", "runtime roots excluded, nested source retained, unknown git not called clean")

    def case_force_unowned():
        repo = make_mini_repo(sandbox / "r-force")
        out = sandbox / "o-force"
        out.mkdir()
        (out / "unowned.txt").write_text("preserve me")
        try:
            export_tree(repo, out, force=True)
        except ReleaseError:
            assert (out / "unowned.txt").read_text() == "preserve me"
            return ("PASS", "unowned output was not overwritten")
        return ("FAIL", "force admitted an unmanifested file")

    def case_nonempty():
        repo = make_mini_repo(sandbox / "r6")
        out = sandbox / "o6"
        out.mkdir(parents=True)
        (out / "preexisting.txt").write_text("keep me\n", encoding="utf-8")
        try:
            export_tree(repo, out)
        except ReleaseError as exc:
            return ("PASS", f"refused non-empty outdir: {exc}")
        return ("FAIL", "non-empty outdir was not refused")

    def case_archive_paths():
        repo = make_mini_repo(sandbox / "r7")
        export = export_tree(repo, sandbox / "o7")
        wg = fake_binary(sandbox / "wg-pe", "PE")
        rg = fake_binary(sandbox / "rg-pe", "PE")
        out = sandbox / "a7.zip"
        ns = argparse.Namespace(
            work=str(sandbox / "aw"), wg=str(wg), rg=str(rg), platform="auto",
            export=str(export.out_dir), repo=str(repo), version="9.9.9",
            build_id="test7", out=str(out), force=True,
            binary_status="self-test-fixture", cargo_registry=None,
            allow_fixture_binaries=True, json=False,
        )
        cmd_archive(ns)
        with zipfile.ZipFile(out) as zf:
            names = set(zf.namelist())
            root = f"wg-9.9.9-windows-x64-{LABEL}-test7"
            for required in (
                f"{root}/bin/wg.exe", f"{root}/bin/rg.exe", f"{root}/wg.cmd",
                f"{root}/README.md", f"{root}/docs/agent-guide.md",
                f"{root}/docs/benchmarks.md", f"{root}/licenses/sgrep-LICENSE",
                f"{root}/licenses/ripgrep-LICENSE-MIT",
                f"{root}/PROVENANCE/BUILD_INFO.json",
                f"{root}/PROVENANCE/SOURCE_MANIFEST.sha256",
                f"{root}/PROVENANCE/SHA256SUMS",
            ):
                assert required in names, f"missing archive path {required}"
            sums = zf.read(f"{root}/PROVENANCE/SHA256SUMS").decode()
            for line in sums.splitlines():
                digest, _, arc = line.partition("  ")
                assert sha256_bytes(zf.read(f"{root}/{arc}")) == digest, f"sum mismatch {arc}"
            info = json.loads(zf.read(f"{root}/PROVENANCE/BUILD_INFO.json"))
            assert info["label"] == LABEL and info["not_official_release"] is True
            assert info["binary_status"] == "self-test-fixture"
        return ("PASS", f"{len(names)} archive entries verified")

    def case_magic_mismatch():
        repo = make_mini_repo(sandbox / "r8")
        export = export_tree(repo, sandbox / "o8")
        wg = fake_binary(sandbox / "wg-elf", "ELF")
        rg = fake_binary(sandbox / "rg-elf", "ELF")
        ns = argparse.Namespace(
            work=str(sandbox / "aw8"), wg=str(wg), rg=str(rg),
            platform="windows-x64", export=str(export.out_dir), repo=str(repo),
            version="9.9.9", build_id="test8", out=str(sandbox / "a8.zip"),
            force=True, binary_status="x", cargo_registry=None, json=False,
        )
        try:
            cmd_archive(ns)
        except ReleaseError as exc:
            return ("PASS", f"refused OS/label mismatch: {exc}")
        return ("FAIL", "OS/label mismatch was not refused")

    def case_rg_version():
        repo = make_mini_repo(sandbox / "r9")
        export = export_tree(repo, sandbox / "o9")
        wg = fake_binary(sandbox / "wg-pe2", "PE")
        rg = fake_binary(sandbox / "rg-pe2", "PE")
        ns = argparse.Namespace(
            work=str(sandbox / "aw9"), wg=str(wg), rg=str(rg), platform="auto",
            export=str(export.out_dir), repo=str(repo), version="9.9.9",
            build_id="test9", out=str(sandbox / "a9.zip"), force=True,
            binary_status="x", cargo_registry=None, json=False,
        )
        try:
            cmd_archive(ns)
        except ReleaseError as exc:
            return ("PASS", f"refused non-rg15.1 input: {exc}")
        return ("FAIL", "a non-ripgrep input was accepted as --rg")

    def case_missing_binary():
        repo = make_mini_repo(sandbox / "r10")
        export = export_tree(repo, sandbox / "o10")
        wg = sandbox / "does-not-exist"
        rg = fake_binary(sandbox / "rg-pe3", "PE")
        ns = argparse.Namespace(
            work=str(sandbox / "aw10"), wg=str(wg), rg=str(rg), platform="auto",
            export=str(export.out_dir), repo=str(repo), version="9.9.9",
            build_id="test10", out=str(sandbox / "a10.zip"), force=True,
            binary_status="x", cargo_registry=None, json=False,
        )
        try:
            cmd_archive(ns)
        except ReleaseError as exc:
            return ("PASS", f"missing binary refused: {exc}")
        return ("FAIL", "missing --wg was not refused")

    def case_archive_overwrite():
        rep = sandbox / "existing.zip"
        rep.write_text("data\n", encoding="utf-8")
        args2 = argparse.Namespace(
            work=str(sandbox / "aw11"), wg="x", rg="y", platform="auto",
            export=None, repo=str(sandbox), version="9.9.9", build_id="t",
            out=str(rep), force=False, binary_status="x", cargo_registry=None, json=False,
        )
        try:
            saved = sys.stdout
            sys.stdout = open(os.devnull, "w")
            try:
                cmd_archive(args2)
            finally:
                sys.stdout.close()
                sys.stdout = saved
        except ReleaseError:
            return ("PASS", "existing output refused before pack")
        return ("FAIL", "existing output archive was not refused")

    def case_smoke_missing():
        ns = argparse.Namespace(
            wg=str(sandbox / "no-such-wg"), rg=None, work=str(sandbox / "sw"),
            warm_cache=None, json=True, timeout=10,
        )
        try:
            cmd_smoke(ns)
        except ReleaseError as exc:
            return ("PASS", f"smoke refused missing binary: {exc}")
        return ("FAIL", "smoke with a missing binary did not fail")

    checks = [
        ("export excludes target/__pycache__", case_export_excludes),
        ("manifest hashes every selected file", case_manifest),
        ("export refuses symlink/reparse source", case_symlink),
        ("export detects source mutation", case_mutation),
        ("export refuses sensitive file names", case_sensitive),
        ("export detects restored-mtime content mutation", case_preserved_metadata_mutation),
        ("export refuses directory symlinks", case_directory_symlink),
        ("export excludes runtime roots but retains nested source", case_runtime_roots),
        ("forced export refuses unowned output", case_force_unowned),
        ("export refuses non-empty output dir", case_nonempty),
        ("archive content paths and SHA256SUMS", case_archive_paths),
        ("archive refuses OS/label mismatch", case_magic_mismatch),
        ("archive requires ripgrep 15.1 input", case_rg_version),
        ("archive fails on missing --wg", case_missing_binary),
        ("archive refuses existing output file", case_archive_overwrite),
        ("smoke fails on missing --wg", case_smoke_missing),
    ]
    for name, fn in checks:
        st.check(name, fn)
    try:
        shutil.rmtree(sandbox, ignore_errors=True)
    except OSError:
        pass
    return st.report(args.json)


def build_parser():
    p = argparse.ArgumentParser(prog="wg_release.py", description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)

    ep = sub.add_parser("export", help="export root crate + vendored notify source")
    ep.add_argument("--repo", default=str(REPO_ROOT))
    ep.add_argument("--out", required=True)
    ep.add_argument("--force", action="store_true")
    ep.add_argument("--json", action="store_true")
    ep.set_defaults(func=cmd_export)

    ap = sub.add_parser("archive", help="assemble a PREVIEW archive")
    ap.add_argument("--wg", required=True)
    ap.add_argument("--rg", required=True)
    ap.add_argument("--platform", default="auto",
                    choices=["auto", "windows-x64", "windows-arm64",
                             "linux-x64", "linux-arm64"])
    ap.add_argument("--export", default=None)
    ap.add_argument("--repo", default=str(REPO_ROOT))
    ap.add_argument("--out", default=None)
    ap.add_argument("--version", default=None)
    ap.add_argument("--build-id", default=None)
    ap.add_argument("--binary-status", default="provided-old-binary-preview",
                    choices=["provided-old-binary-preview", "final"])
    ap.add_argument("--label", default=LABEL, choices=["PREVIEW", "RELEASE"])
    ap.add_argument("--cargo-registry", default=None)
    ap.add_argument("--work", default=str(DEFAULT_WORK))
    ap.add_argument("--force", action="store_true")
    ap.add_argument("--json", action="store_true")
    ap.set_defaults(func=cmd_archive)

    sp = sub.add_parser("smoke", help="clean-install smoke test")
    sp.add_argument("--wg", required=True)
    sp.add_argument("--rg", default=None)
    sp.add_argument("--warm-cache", default=None)
    sp.add_argument("--work", default=str(DEFAULT_WORK / "smoke"))
    sp.add_argument("--timeout", type=int, default=600)
    sp.add_argument("--json", action="store_true")
    sp.set_defaults(func=cmd_smoke)

    tp = sub.add_parser("self-test", help="run packaging safety self checks")
    tp.add_argument("--work", default=str(DEFAULT_WORK / "tmp"))
    tp.add_argument("--json", action="store_true")
    tp.set_defaults(func=run_self_test)
    return p


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except ReleaseError as exc:
        print(f"wg_release: error: {exc}", file=sys.stderr)
        return exc.code


if __name__ == "__main__":
    sys.exit(main())
