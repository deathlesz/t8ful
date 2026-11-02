//! Path resolution for tool arguments.
//!
//! Coding mode runs with the user's normal privileges, so these helpers only make
//! paths predictable (relative paths resolve against the launch directory, `..`
//! is collapsed lexically); they are not a confinement boundary.

use std::path::{Component, Path, PathBuf};

/// Resolve a user-supplied path against `base`.
pub fn resolve(base: &Path, requested: &str) -> PathBuf {
    let requested = requested.trim();

    if requested.is_empty() {
        return normalize(base);
    }

    let path = Path::new(requested);

    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&base.join(path))
    }
}

/// Collapse `.` and `..` components without touching the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component);
                }
            }
            other => normalized.push(other),
        }
    }

    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

/// Render a path relative to the project root for user-facing messages.
pub fn display(path: &Path, project_root: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{display, normalize, resolve};

    #[test]
    fn relative_paths_resolve_against_the_base() {
        let resolved = resolve(Path::new("/work/project"), "src/main.rs");

        assert_eq!(resolved, PathBuf::from("/work/project/src/main.rs"));
    }

    #[test]
    fn absolute_paths_are_kept() {
        let resolved = resolve(Path::new("/work/project"), "/tmp/other.txt");

        assert_eq!(resolved, PathBuf::from("/tmp/other.txt"));
    }

    #[test]
    fn parent_components_are_collapsed() {
        assert_eq!(
            normalize(Path::new("/work/project/../other/./file.txt")),
            PathBuf::from("/work/other/file.txt")
        );
    }

    #[test]
    fn blank_paths_resolve_to_the_base() {
        assert_eq!(
            resolve(Path::new("/work/project"), "  "),
            PathBuf::from("/work/project")
        );
    }

    #[test]
    fn display_strips_the_project_root() {
        assert_eq!(
            display(
                Path::new("/work/project/src/main.rs"),
                Path::new("/work/project")
            ),
            "src/main.rs"
        );
        assert_eq!(
            display(Path::new("/other/main.rs"), Path::new("/work/project")),
            "/other/main.rs"
        );
    }
}
