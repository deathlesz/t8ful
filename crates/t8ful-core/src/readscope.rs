//! Read-only mount roots for the planning sandbox.
//!
//! Direct reads are not confined in either mode. Plan mode uses this set to
//! decide which host paths a sandboxed shell command may see and to validate
//! its working directory.

use std::path::PathBuf;

/// Canonical roots a plan-mode shell may see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadScope {
    roots: Vec<PathBuf>,
}

impl ReadScope {
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut canonical = Vec::new();

        for root in roots {
            let Ok(path) = std::fs::canonicalize(&root) else {
                continue;
            };

            if !canonical.contains(&path) {
                canonical.push(path);
            }
        }

        canonical.sort();

        Self { roots: canonical }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Whether `path` resolves inside one of the configured roots.
    pub fn permits(&self, path: &std::path::Path) -> bool {
        let Ok(canonical) = std::fs::canonicalize(path) else {
            return false;
        };

        self.roots.iter().any(|root| canonical.starts_with(root))
    }
}

#[cfg(test)]
mod tests {
    use super::ReadScope;

    #[test]
    fn scope_matches_only_paths_below_a_root() {
        let dir = tempfile::tempdir().expect("temp dir");
        let inside = dir.path().join("inside.txt");
        let outside_dir = tempfile::tempdir().expect("outside dir");
        let outside = outside_dir.path().join("outside.txt");

        std::fs::write(&inside, "ok").unwrap();
        std::fs::write(&outside, "no").unwrap();

        let scope = ReadScope::new([dir.path().to_path_buf()]);

        assert!(scope.permits(&inside));
        assert!(!scope.permits(&outside));
        assert!(!scope.is_empty());
    }

    #[test]
    fn missing_roots_are_dropped() {
        let scope = ReadScope::new([PathBuf::from("/definitely/missing/t8ful-root")]);

        assert!(scope.is_empty());
    }

    use std::path::PathBuf;
}
