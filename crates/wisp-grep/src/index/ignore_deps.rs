//! Ignore sources a daemon's walk consults that live outside the watched root.
//!
//! rg's walker honors ignore files in ancestor directories (`.ignore`,
//! `.gitignore`, `.rgignore`), the containing Git repository's
//! `.git/info/exclude` (which a linked worktree or submodule names through a
//! `gitdir:` file) and the user's global gitignore. The daemon watches only the
//! root recursively, so a change to any of these can silently change which
//! files the walk selects while the daemon keeps serving its old candidate set.
//!
//! [`Deps::snapshot`] records the change token of every candidate path and
//! [`Deps::changed`] re-reads them (tens of `fs` calls, negligible). A change,
//! creation or deletion forces the daemon to reconcile, so the candidate set
//! matches rg's current ignore rules. The `.git` pointer file and a linked
//! worktree's `commondir` are tracked with the same rule, so re-pointing the
//! repository (or creating the pointer late) also invalidates the snapshot.
//! Both the ideal Git resolution and the bundled `ignore` crate's own
//! resolution (which reads a relative `gitdir:` from the process working
//! directory) are tracked, so the dependency set covers the exclude the walk
//! actually reads even when the two disagree. This includes a `.git` pointer
//! nested below the root (a linked worktree or submodule checked out inside the
//! tree): its directory's exclude is an ignore dependency exactly like the
//! root's, so a same-cwd edit to an external one invalidates the snapshot.
//!
//! Size and mtime are deliberately not used: an ignore file rewritten with a
//! restored mtime must still be detected. Change tokens also distinguish a
//! created dependency (`None` -> `Some`) from a missing one.
//!
//! A change token can miss a rewrite: on Linux the ctime only advances per
//! kernel tick, so a same-length rewrite in the snapshot's tick keeps the
//! token. An ignore file written within `RACY_NS` of the snapshot (by mtime,
//! which is comparable on every platform) therefore also has its full
//! contents recorded, and `changed` compares those too. `.jj` (Jujutsu)
//! repositories are not covered here.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::build::{self, RACY_NS};

/// One ignore file, its change token (`None` when the file cannot be read,
/// usually because it does not exist) and, for a racy file, its contents.
struct Dep {
    path: PathBuf,
    token: Option<i64>,
    contents: Option<Vec<u8>>,
}

pub(crate) struct Deps {
    deps: Vec<Dep>,
}

impl Deps {
    /// Records the ancestor, Git-repository and global ignore files a walk
    /// rooted at `root` would consult, with each one's current change token.
    pub(crate) fn snapshot(root: &Path) -> Deps {
        let mut paths = Vec::new();
        for dir in root.ancestors() {
            for name in [".ignore", ".gitignore", ".rgignore"] {
                paths.push(dir.join(name));
            }
        let git = resolve_git_dir(dir);
        // The `.git` pointer and a linked worktree's `commondir` decide which
        // `info/exclude` applies. Track both, a missing one included, so
        // creating or re-pointing one re-resolves the dependency.
        paths.extend(git.pointers);
        paths.push(git.common.join("info").join("exclude"));
        // `resolve_git_dir` uses ideal Git semantics. The walk does not: the
        // bundled `ignore` crate resolves a relative `gitdir:` against the
        // process working directory. Track the paths it actually opens too, so
        // a change to the exclude the walk reads still invalidates. A missing
        // walker path is a tracked (missing) dependency, like the ideal one.
        paths.extend(walker_git_paths(dir));
        }
        // A `.git` pointer nested below the root (a linked worktree or
        // submodule checked out inside the tree) makes the walk read that
        // directory's `info/exclude` exactly like the root case. Track the same
        // ideal and as-written paths, so a same-cwd change to that exclude
        // invalidates and a re-pointed pointer is seen.
        for dir in nested_repo_dirs(root) {
            let git = resolve_git_dir(&dir);
            paths.extend(git.pointers);
            paths.push(git.common.join("info").join("exclude"));
            paths.extend(walker_git_paths(&dir));
        }
        if let Some((configs, exclude)) = global_gitignore() {
            paths.extend(configs);
            if let Some(exclude) = exclude {
                paths.push(exclude);
            }
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_nanos()).ok())
            .unwrap_or(i64::MAX);
        let deps = paths
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| {
                let token = build::change_of_path(&path);
                // Unknown time counts as racy: keeping the contents is the safe side.
                let racy = std::fs::metadata(&path).ok().is_some_and(|md| {
                    // On Unix the token is the ctime in unix ns, so a forged old mtime
                    // cannot hide a recent write. Windows tokens use a different epoch
                    // and NTFS change time has 100 ns resolution, so mtime alone is used
                    // there and the same-tick miss does not occur.
                    #[cfg(unix)]
                    let newest = token.into_iter().chain(build::mtime_ns(&md)).max();
                    #[cfg(not(unix))]
                    let newest = build::mtime_ns(&md);
                    newest.map_or(true, |m| m >= now.saturating_sub(RACY_NS))
                });
                let contents = if racy { std::fs::read(&path).ok() } else { None };
                Dep { path, token, contents }
            })
            .collect();
        Deps { deps }
    }

    /// Whether any recorded ignore dependency changed since the snapshot.
    pub(crate) fn changed(&self) -> bool {
        self.deps.iter().any(|d| {
            build::change_of_path(&d.path) != d.token
                || d.contents.as_deref().is_some_and(|c| std::fs::read(&d.path).ok().as_deref() != Some(c))
        })
    }
}

/// A directory's Git control layout: the common Git directory whose
/// `info/exclude` applies to `dir`, plus the control files whose change
/// re-resolves it.
///
/// `pointers` holds the `.git` pointer file when `.git` is a file and, for a
/// linked worktree, the resolved gitdir's `commondir`. It also holds `dir/.git`
/// while that path is absent, so a later creation is seen. `common` and the
/// pointer paths are joined but never canonicalized, so a missing target stays a
/// tracked (missing) dependency rather than being dropped.
struct GitLayout {
    common: PathBuf,
    pointers: Vec<PathBuf>,
}

/// Resolve `dir`'s Git common directory using ideal Git semantics: a plain
/// `.git` directory is used directly; when `.git` is a file it names the gitdir
/// through a `gitdir:` line, and that gitdir's `commondir` names the common
/// directory. A relative `gitdir:` is anchored on the `.git` file's directory
/// (not the process cwd) and a relative `commondir` is joined whenever the path
/// is relative (`Path::is_absolute`), not only when it starts with `.`.
///
/// This is the correct Git behavior, but it is not what the bundled `ignore`
/// crate does (see [`walker_git_paths`]). Both are tracked: the ideal layout
/// catches a change a real Git reader would honor, and the walker layout catches
/// the exclude the walk actually reads.
fn resolve_git_dir(dir: &Path) -> GitLayout {
    let git = dir.join(".git");
    match std::fs::metadata(&git) {
        // A `.git` directory is the common dir itself. Its ctime is not tracked:
        // normal Git activity must not force a walk, and its `info/exclude` is
        // tracked below.
        Ok(md) if !md.is_file() => GitLayout { common: git, pointers: Vec::new() },
        Ok(_) => {
            let mut pointers = vec![git.clone()];
            let Some(target) = git_pointer_target(&git) else {
                return GitLayout { common: git, pointers };
            };
            let git_dir = if target.is_absolute() { target } else { dir.join(target) };
            let commondir = git_dir.join("commondir");
            pointers.push(commondir.clone());
            let Some(line) = first_line(&commondir) else {
                return GitLayout { common: git_dir, pointers };
            };
            let line = PathBuf::from(line);
            let common = if line.is_absolute() { line } else { git_dir.join(line) };
            GitLayout { common, pointers }
        }
        // No `.git` yet: track the path so its creation invalidates the snapshot.
        Err(_) => GitLayout { common: git.clone(), pointers: vec![git] },
    }
}

/// The paths the bundled `ignore` crate's walker opens for `dir`, mirroring
/// `ignore::dir::resolve_git_commondir` so the snapshot covers the exclude the
/// walk actually reads, not only the ideal one.
///
/// The crate does *not* use ideal Git semantics: a relative `gitdir:` is opened
/// as written, so it resolves against the process working directory, not `dir`;
/// and a `commondir` line is joined to the gitdir only when it starts with `.`.
/// When `.git` is absent or a directory the crate reads `dir/.git/info/exclude`,
/// which the ideal layout already tracks, so this returns nothing.
///
/// The returned paths are left unresolved, so a relative `.git` target stays
/// cwd-relative and a filesystem call reads exactly what the walk reads.
fn walker_git_paths(dir: &Path) -> Vec<PathBuf> {
    let git = dir.join(".git");
    if !std::fs::metadata(&git).is_ok_and(|md| md.is_file()) {
        return Vec::new();
    }
    let Some(target) = git_pointer_target(&git) else { return Vec::new() };
    // As written: a relative target stays relative and is read from the cwd.
    let commondir = target.join("commondir");
    let mut paths = vec![commondir.clone()];
    let Some(line) = first_line(&commondir) else { return paths };
    let common = if line.starts_with('.') { target.join(&line) } else { PathBuf::from(&line) };
    paths.push(common.join("info").join("exclude"));
    paths
}

/// Directories at or below `root` that hold a `.git` pointer *file*: nested
/// linked worktrees and submodules. Their `info/exclude` is read by the walk
/// just like the root repository's, so each one is an ignore dependency.
///
/// The scan mirrors the walk's reach: it never descends into a `.git`
/// directory and never descends into a hidden directory (the eligible walk uses
/// default flags, so a `.git` under a hidden directory is never read). A `.git`
/// file inside a regular directory is still found. Symlinked directories are
/// not followed, matching the walk's `follow_links = false`.
fn nested_repo_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let is_dot_git = entry.file_name() == std::ffi::OsStr::new(".git");
            if ft.is_dir() {
                if is_dot_git || entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                stack.push(entry.path());
            } else if ft.is_file() && is_dot_git {
                out.push(dir.clone());
            }
        }
    }
    out
}

/// The `gitdir:` target named by a `.git` pointer file, if there is one.
fn git_pointer_target(git: &Path) -> Option<PathBuf> {
    let target = first_line(git)?.strip_prefix("gitdir: ")?.trim().to_owned();
    (!target.is_empty()).then(|| PathBuf::from(target))
}

/// The first line of a file, without its terminator, if it can be read.
fn first_line(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()?.lines().next().map(str::to_owned)
}

/// The two config files that can name the global gitignore, plus the resolved
/// path itself, duplicating the `ignore` crate's lookup so a change to either
/// the config or the ignore file is seen.
fn global_gitignore() -> Option<(Vec<PathBuf>, Option<PathBuf>)> {
    let home = home_dir()?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let configs = vec![home.join(".gitconfig"), xdg.join("git").join("config")];
    let exclude = ignore::gitignore::gitconfig_excludes_path().or_else(|| Some(xdg.join("git").join("ignore")));
    Some((configs, exclude))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{build, nested_repo_dirs, walker_git_paths, Deps};

    fn write(path: &std::path::Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn detects_same_length_rewrite_within_token_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let root = parent.join("tree");
        std::fs::create_dir_all(&root).unwrap();
        let ignore = parent.join(".ignore");
        write(&ignore, "alpha.txt\n");

        // Rewritten right after the snapshot with different content of the same length.
        let mut deps = Deps::snapshot(&root);
        write(&ignore, "bravo.txt\n");
        // Simulate the same kernel tick: the change token still matches, so only
        // the recorded contents of the racy file can show the rewrite.
        for d in &mut deps.deps {
            d.token = build::change_of_path(&d.path);
        }
        assert!(deps.changed());
    }

    #[test]
    fn detects_ancestor_ignore_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let root = parent.join("tree");
        std::fs::create_dir_all(&root).unwrap();
        write(&root.join("visible.txt"), "visible\n");

        // A stable tree reports no change.
        let deps = Deps::snapshot(&root);
        assert!(!deps.changed());

        // Creating an ancestor ignore file is seen.
        write(&parent.join(".ignore"), "hidden.txt\n");
        assert!(deps.changed());

        // Re-snapshot, then modify it with different content: still seen.
        let deps = Deps::snapshot(&root);
        write(&parent.join(".ignore"), "hidden.txt\nother.txt\n");
        assert!(deps.changed());

        // Deletion is seen too.
        let deps = Deps::snapshot(&root);
        std::fs::remove_file(parent.join(".ignore")).unwrap();
        assert!(deps.changed());
    }

    #[test]
    fn detects_ancestor_gitignore_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let root = parent.join("tree");
        std::fs::create_dir_all(&root).unwrap();
        let deps = Deps::snapshot(&root);
        assert!(!deps.changed());
        write(&parent.join(".gitignore"), "target\n");
        assert!(deps.changed());
    }

    #[test]
    fn detects_git_exclude_changes_in_ancestor_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let root = repo.join("sub");
        std::fs::create_dir_all(repo.join(".git").join("info")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let exclude = repo.join(".git").join("info").join("exclude");
        write(&exclude, "a\n");

        let deps = Deps::snapshot(&root);
        assert!(!deps.changed());
        write(&exclude, "a\nb\n");
        assert!(deps.changed());
    }

    #[test]
    fn resolves_linked_worktree_commondir() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let wt = tmp.path().join("wt");
        let gitdir = main.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(gitdir.join("..").join("..").join("info")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        write(&gitdir.join("commondir"), "../..\n");
        write(&wt.join(".git"), &format!("gitdir: {}\n", gitdir.display()));
        let exclude = main.join(".git").join("info").join("exclude");
        write(&exclude, "a\n");

        let deps = Deps::snapshot(&wt);
        assert!(!deps.changed());
        write(&exclude, "a\nb\n");
        assert!(deps.changed());
    }

    /// A relative `gitdir:` is anchored on the `.git` file's directory, not the
    /// process cwd. The test never changes cwd, so a cwd-relative resolution
    /// points at a path under the crate directory and misses the real exclude.
    #[test]
    fn resolves_relative_gitdir_from_dot_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let wt = tmp.path().join("wt");
        let root = wt.join("sub");
        let gitdir = main.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(main.join(".git").join("info")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        write(&gitdir.join("commondir"), "../..\n");
        write(&wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n");
        let exclude = main.join(".git").join("info").join("exclude");
        write(&exclude, "a\n");

        let deps = Deps::snapshot(&root);
        assert!(!deps.changed());
        write(&exclude, "a\nb\n");
        assert!(deps.changed());
    }

    /// A relative `commondir` with no leading dot is still relative, so it is
    /// joined to the gitdir. Under a `starts_with('.')` test it would be taken
    /// as an absolute path and the exclude would be missed.
    #[test]
    fn resolves_relative_commondir_without_leading_dot() {
        let tmp = tempfile::tempdir().unwrap();
        let gitdir = tmp.path().join("gd");
        let wt = tmp.path().join("wt");
        let root = wt.join("sub");
        std::fs::create_dir_all(gitdir.join("common").join("info")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        write(&gitdir.join("commondir"), "common\n");
        write(&wt.join(".git"), &format!("gitdir: {}\n", gitdir.display()));
        let exclude = gitdir.join("common").join("info").join("exclude");
        write(&exclude, "a\n");

        let deps = Deps::snapshot(&root);
        assert!(!deps.changed());
        write(&exclude, "a\nb\n");
        assert!(deps.changed());
    }

    /// Absolute `gitdir:` and `commondir` lines are used as written.
    #[test]
    fn resolves_absolute_gitdir_and_commondir() {
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join("common");
        let gitdir = tmp.path().join("gd");
        let wt = tmp.path().join("wt");
        let root = wt.join("sub");
        std::fs::create_dir_all(common.join("info")).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        write(&gitdir.join("commondir"), &format!("{}\n", common.display()));
        write(&wt.join(".git"), &format!("gitdir: {}\n", gitdir.display()));
        let exclude = common.join("info").join("exclude");
        write(&exclude, "a\n");

        let deps = Deps::snapshot(&root);
        assert!(!deps.changed());
        write(&exclude, "a\nb\n");
        assert!(deps.changed());
    }

    /// Creating the `.git` pointer late, and re-pointing it at another gitdir,
    /// both invalidate the snapshot even if the exclude paths stay untouched.
    #[test]
    fn detects_git_pointer_creation_and_repair() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let root = wt.join("sub");
        std::fs::create_dir_all(&root).unwrap();

        // No `.git` yet: the snapshot tracks its absence.
        let deps = Deps::snapshot(&root);
        assert!(!deps.changed());

        // Creating the pointer file (missing -> created) is seen.
        let gitdir = tmp.path().join("gd");
        std::fs::create_dir_all(gitdir.join("info")).unwrap();
        write(&gitdir.join("info").join("exclude"), "a\n");
        write(&wt.join(".git"), &format!("gitdir: {}\n", gitdir.display()));
        assert!(deps.changed());

        // Re-pointing the pointer at a second gitdir is seen too.
        let deps = Deps::snapshot(&root);
        let gitdir2 = tmp.path().join("gd2");
        std::fs::create_dir_all(gitdir2.join("info")).unwrap();
        write(&gitdir2.join("info").join("exclude"), "b\n");
        write(&wt.join(".git"), &format!("gitdir: {}\n", gitdir2.display()));
        assert!(deps.changed());
    }

    /// Creating the `commondir` file after the snapshot re-points the common
    /// directory, so the snapshot must be invalidated (missing -> created).
    #[test]
    fn detects_commondir_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let gitdir = tmp.path().join("gd");
        let wt = tmp.path().join("wt");
        let root = wt.join("sub");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        write(&wt.join(".git"), &format!("gitdir: {}\n", gitdir.display()));

        // No `commondir`: the gitdir itself is the common dir.
        let deps = Deps::snapshot(&root);
        assert!(!deps.changed());

        let common = tmp.path().join("common");
        std::fs::create_dir_all(common.join("info")).unwrap();
        write(&common.join("info").join("exclude"), "a\n");
        write(&gitdir.join("commondir"), &format!("{}\n", common.display()));
        assert!(deps.changed());
    }

    /// The walker mirrors the `ignore` crate, not ideal Git: a relative
    /// `gitdir:` is opened as written, so the `commondir` it names stays
    /// cwd-relative. This is a path the walk can read and the ideal layout does
    /// not track, so it must be tracked separately.
    #[test]
    fn walker_relative_gitdir_target_is_used_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        write(&wt.join(".git"), "gitdir: rel/gd\n");

        let paths = walker_git_paths(&wt);
        assert_eq!(paths, vec![PathBuf::from("rel").join("gd").join("commondir")]);
    }

    /// A `commondir` line starting with `.` is joined to the gitdir, exactly as
    /// the `ignore` crate does.
    #[test]
    fn walker_commondir_with_leading_dot_joins_gitdir() {
        let tmp = tempfile::tempdir().unwrap();
        let gd = tmp.path().join("gd");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        write(&wt.join(".git"), &format!("gitdir: {}\n", gd.display()));
        write(&gd.join("commondir"), "../common\n");

        let paths = walker_git_paths(&wt);
        assert_eq!(
            paths,
            vec![gd.join("commondir"), gd.join("../common").join("info").join("exclude")]
        );
    }

    /// A `commondir` line without a leading dot is used as an absolute path, so
    /// the exclude is tracked as written. A `starts_with('.')`-only test in the
    /// walker would miss this, but so would joining the line to the gitdir.
    #[test]
    fn walker_commondir_without_leading_dot_is_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let gd = tmp.path().join("gd");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        write(&wt.join(".git"), &format!("gitdir: {}\n", gd.display()));
        write(&gd.join("commondir"), "common\n");

        let paths = walker_git_paths(&wt);
        assert_eq!(
            paths,
            vec![gd.join("commondir"), PathBuf::from("common").join("info").join("exclude")]
        );
    }

    /// A `.git` directory (or its absence) needs no extra walker tracking: the
    /// crate reads `dir/.git/info/exclude`, which the ideal layout covers.
    #[test]
    fn walker_tracks_nothing_for_a_dot_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dir");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(walker_git_paths(&dir).is_empty());
        let none = tmp.path().join("none");
        std::fs::create_dir_all(&none).unwrap();
        assert!(walker_git_paths(&none).is_empty());
    }

    /// A `.git` pointer file nested below the root is found, and a plain `.git`
    /// directory (or any hidden directory) is neither descended into nor
    /// reported.
    #[test]
    fn nested_repo_dirs_finds_pointer_files_and_skips_git_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("sub/nested")).unwrap();
        std::fs::create_dir_all(root.join(".hidden/sub")).unwrap();
        write(&root.join("sub/.git"), "gitdir: /abs/gd\n");
        write(&root.join("sub/nested/.git"), "gitdir: /abs/gd2\n");
        write(&root.join(".hidden/sub/.git"), "gitdir: /abs/gd3\n");

        let mut dirs = nested_repo_dirs(&root);
        dirs.sort();
        assert_eq!(dirs, vec![root.join("sub"), root.join("sub/nested")]);
    }

    /// A nested linked worktree with a relative `gitdir:` and an external
    /// common directory contributes its `info/exclude` to the snapshot, so a
    /// same-cwd edit to that exclude invalidates.
    #[test]
    fn tracks_nested_worktree_external_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let gitdir = tmp.path().join("shadow.git/wt");
        let sub = root.join("sub");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(tmp.path().join("shadow.git/info")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        write(&sub.join(".git"), "gitdir: ../../shadow.git/wt\n");
        write(&gitdir.join("commondir"), "..\n");
        let exclude = tmp.path().join("shadow.git/info/exclude");
        write(&exclude, "a\n");
        let deps = Deps::snapshot(&root);
        let exclude = exclude.canonicalize().unwrap();
        assert!(deps.deps.iter().any(|dep| dep.path.canonicalize().ok().as_ref() == Some(&exclude)));
        assert!(!deps.changed());
        write(&exclude, "a\nb\n");
        assert!(deps.changed());
    }
}
