//! One semantic-search scope inside an attached root.
//!
//! A scope has an optional path, optional include/exclude globs and an
//! optional raw cosine cutoff. The path is relative to the root and is either
//! one file (an exact match) or one directory (a component-boundary prefix). A
//! glob without `/` matches a file name at any depth. A glob with `/` is
//! root-relative. Include patterns are OR-ed. Any exclude pattern (`!PATTERN`)
//! vetoes a match. Matching uses the `globset` syntax, so `*`, `**`, `?`,
//! `[...]` and `{a,b}` behave as usual.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::path::{Path, PathBuf};

/// The path part of a semantic scope, relative to the attached root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopePath {
    File(PathBuf),
    Directory(PathBuf),
}

#[derive(Clone, Default)]
pub struct SemanticScope {
    path: Option<ScopePath>,
    include_files: Option<GlobSet>,
    include_paths: Option<GlobSet>,
    exclude_files: Option<GlobSet>,
    exclude_paths: Option<GlobSet>,
    min_score: Option<f64>,
}

impl SemanticScope {
    /// Build a scope from a root-relative path and repeated glob patterns.
    ///
    /// A pattern that starts with `!` excludes. All other patterns include.
    /// An empty pattern is an error.
    pub fn new(path: Option<ScopePath>, globs: &[String]) -> Result<Self, String> {
        Self::with_min_score(path, globs, None)
    }

    /// Build a scope with an optional raw cosine cutoff.
    ///
    /// The cutoff must be finite and inside `[-1, 1]`. `None` keeps the
    /// existing recall because no default cutoff has calibration evidence.
    pub fn with_min_score(
        path: Option<ScopePath>,
        globs: &[String],
        min_score: Option<f64>,
    ) -> Result<Self, String> {
        if let Some(score) = min_score {
            if !score.is_finite() || !(-1.0..=1.0).contains(&score) {
                return Err(format!(
                    "invalid --min-score {score}: expected a finite cosine value in [-1, 1]"
                ));
            }
        }
        let mut include_files = GlobSetBuilder::new();
        let mut include_paths = GlobSetBuilder::new();
        let mut exclude_files = GlobSetBuilder::new();
        let mut exclude_paths = GlobSetBuilder::new();
        let mut has_include = false;
        let mut has_exclude = false;
        for glob in globs {
            let (exclude, pattern) = match glob.strip_prefix('!') {
                Some(pattern) => (true, pattern),
                None => (false, glob.as_str()),
            };
            if pattern.is_empty() {
                return Err(format!("invalid --semantic-glob {glob:?}: empty pattern"));
            }
            let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
            let pattern = if cfg!(windows) {
                pattern.strip_prefix(r".\").unwrap_or(pattern)
            } else {
                pattern
            };
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|error| format!("invalid --semantic-glob {glob:?}: {error}"))?;
            let (files, paths) = if pattern.contains('/') || (cfg!(windows) && pattern.contains('\\')) {
                (&mut include_paths, &mut exclude_paths)
            } else {
                (&mut include_files, &mut exclude_files)
            };
            if exclude {
                paths.add(glob);
                has_exclude = true;
            } else {
                files.add(glob);
                has_include = true;
            }
        }
        Ok(Self {
            path,
            include_files: has_include
                .then(|| include_files.build())
                .transpose()
                .map_err(|error| format!("invalid --semantic-glob: {error}"))?,
            include_paths: has_include
                .then(|| include_paths.build())
                .transpose()
                .map_err(|error| format!("invalid --semantic-glob: {error}"))?,
            exclude_files: has_exclude
                .then(|| exclude_files.build())
                .transpose()
                .map_err(|error| format!("invalid --semantic-glob: {error}"))?,
            exclude_paths: has_exclude
                .then(|| exclude_paths.build())
                .transpose()
                .map_err(|error| format!("invalid --semantic-glob: {error}"))?,
            min_score,
        })
    }

    /// True when the scope covers every file in the root and has no cutoff.
    pub fn is_unfiltered(&self) -> bool {
        self.path.is_none()
            && self.include_files.is_none()
            && self.include_paths.is_none()
            && self.exclude_files.is_none()
            && self.exclude_paths.is_none()
            && self.min_score.is_none()
    }

    /// The raw cosine cutoff, when the caller set one.
    pub fn min_score(&self) -> Option<f64> {
        self.min_score
    }

    /// True when one root-relative file path is inside the scope.
    pub fn matches(&self, relative: &Path) -> bool {
        if let Some(path) = &self.path {
            let inside = match path {
                ScopePath::File(file) => relative == file,
                ScopePath::Directory(dir) => relative.starts_with(dir),
            };
            if !inside {
                return false;
            }
        }
        let included = !self.has_include()
            || self
                .include_paths
                .as_ref()
                .is_some_and(|set| set.is_match(relative))
            || self
                .include_files
                .as_ref()
                .is_some_and(|set| basename_matches(set, relative));
        if !included {
            return false;
        }
        let excluded = self
            .exclude_paths
            .as_ref()
            .is_some_and(|set| set.is_match(relative))
            || self
                .exclude_files
                .as_ref()
                .is_some_and(|set| basename_matches(set, relative));
        !excluded
    }

    fn has_include(&self) -> bool {
        self.include_files.is_some() || self.include_paths.is_some()
    }
}

fn basename_matches(set: &GlobSet, path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| set.is_match(Path::new(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(path: Option<ScopePath>, globs: &[&str]) -> SemanticScope {
        let globs: Vec<String> = globs.iter().map(|g| (*g).to_owned()).collect();
        SemanticScope::new(path, &globs).unwrap()
    }

    #[test]
    fn directory_scope_stops_at_a_path_boundary() {
        let scope = scope(Some(ScopePath::Directory(PathBuf::from("src"))), &[]);
        assert!(scope.matches(Path::new("src/lib.rs")));
        assert!(scope.matches(Path::new("src/deep/mod.rs")));
        assert!(!scope.matches(Path::new("src2/lib.rs")));
        assert!(!scope.matches(Path::new("other/src/lib.rs")));
    }

    #[test]
    fn file_scope_selects_only_the_exact_file() {
        let scope = scope(Some(ScopePath::File(PathBuf::from("src/lib.rs"))), &[]);
        assert!(scope.matches(Path::new("src/lib.rs")));
        assert!(!scope.matches(Path::new("src/lib.rs.bak")));
        assert!(!scope.matches(Path::new("src/lib.rs/child")));
        assert!(!scope.matches(Path::new("src/other.rs")));
    }

    #[test]
    fn includes_are_or_and_excludes_veto() {
        let scope = scope(None, &["*.rs", "docs/**", "!vendor/**", "!*.min.rs"]);
        // A pattern without `/` matches a file name at any depth.
        assert!(scope.matches(Path::new("a.rs")));
        assert!(scope.matches(Path::new("src/a.rs")));
        // A pattern with `/` is root-relative.
        assert!(scope.matches(Path::new("docs/guide.md")));
        assert!(!scope.matches(Path::new("src/docs/guide.md")));
        assert!(!scope.matches(Path::new("notes.txt")));
        // Any exclude match vetoes every include match.
        assert!(!scope.matches(Path::new("vendor/a.rs")));
        assert!(!scope.matches(Path::new("src/a.min.rs")));
    }

    #[test]
    fn a_scope_can_select_no_paths() {
        let empty = scope(Some(ScopePath::Directory(PathBuf::from("empty"))), &[]);
        let paths = [Path::new("src/lib.rs"), Path::new("a.rs")];
        assert!(paths.iter().all(|path| !empty.matches(path)));

        // An exclude that vetoes every include also selects nothing.
        let excluded = scope(None, &["*.rs", "!*.rs"]);
        assert!(paths.iter().all(|path| !excluded.matches(path)));
    }

    #[test]
    fn invalid_and_empty_patterns_are_rejected() {
        assert!(SemanticScope::new(None, &["[".to_owned()]).is_err());
        assert!(SemanticScope::new(None, &["!".to_owned()]).is_err());
    }

    #[test]
    fn min_score_is_optional_and_validated() {
        assert_eq!(scope(None, &[]).min_score(), None);
        for score in [-1.0, 0.0, 0.35, 1.0] {
            let cutoff = SemanticScope::with_min_score(None, &[], Some(score)).unwrap();
            assert_eq!(cutoff.min_score(), Some(score));
        }
        for score in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.01, 1.01] {
            assert!(
                SemanticScope::with_min_score(None, &[], Some(score)).is_err(),
                "{score} must be rejected"
            );
        }

        // A cutoff alone must not take the unfiltered fast path.
        let cutoff = SemanticScope::with_min_score(None, &[], Some(0.2)).unwrap();
        assert!(!cutoff.is_unfiltered());
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_globs_are_root_relative() {
        let native = scope(None, &[r".\src\**", r"!src\generated\**"]);
        assert!(native.matches(Path::new(r"src\lib.rs")));
        assert!(!native.matches(Path::new(r"other\src\lib.rs")));
        assert!(!native.matches(Path::new(r"src\generated\lib.rs")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_spelling_matches_root_relative_slash_patterns() {
        let patterns = scope(None, &["src/**"]);
        assert!(patterns.matches(&PathBuf::from("src").join("lib.rs")));
        assert!(!patterns.matches(&PathBuf::from("other").join("lib.rs")));

        let file = scope(
            Some(ScopePath::File(PathBuf::from("src").join("lib.rs"))),
            &[],
        );
        assert!(file.matches(Path::new("src/lib.rs")));
        assert!(!file.matches(&PathBuf::from("src").join("lib2.rs")));
    }
}
