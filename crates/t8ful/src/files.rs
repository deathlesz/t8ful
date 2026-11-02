//! Project file index for `@` mention completion in the composer.
//!
//! Only relative paths leave this module; contents are never read. Inside a Git
//! work tree the index is gitignore-aware, so build artifacts and secrets in
//! ignored files stay out of the popup.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// Upper bound on published entries, including derived directories.
const MAX_ENTRIES: usize = 50_000;

/// Every file and directory under `root`, as sorted relative paths.
///
/// Directories carry a trailing `/`, matching the text the composer inserts.
/// Git's tracked and untracked non-ignored files are used when `root` is in a
/// work tree; otherwise a walk skips `.git` and hidden entries.
pub fn project_files(root: &Path) -> Vec<String> {
    let files = git_files(root).unwrap_or_else(|| walk_files(root));
    let mut entries = BTreeSet::new();

    for file in files.into_iter().take(MAX_ENTRIES) {
        for directory in parent_directories(&file) {
            entries.insert(directory);
        }

        entries.insert(file);
    }

    entries.into_iter().take(MAX_ENTRIES).collect()
}

/// Paths relative to `root` from Git, or `None` outside a work tree.
///
/// The NUL-separated stream is read through a bounded reader rather than
/// buffered whole: an index with more entries than the published cap is cut
/// off, and the child is killed instead of blocking on a full pipe.
fn git_files(root: &Path) -> Option<Vec<String>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let (files, complete) = read_git_index(stdout, root, MAX_ENTRIES);

    if !complete {
        let _ = child.kill();
        let _ = child.wait();

        return Some(files);
    }

    let status = child.wait().ok()?;

    if status.success() { Some(files) } else { None }
}

/// Read NUL-separated index entries until EOF or `limit` accepted paths.
///
/// Returns the paths and whether the reader reached EOF before the cap. A
/// partial record at the cap is intentionally ignored.
fn read_git_index(mut reader: impl Read, root: &Path, limit: usize) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut record = Vec::new();
    let mut chunk = [0_u8; 8192];

    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return (files, false),
        };

        for byte in &chunk[..read] {
            if *byte != 0 {
                record.push(*byte);

                continue;
            }

            if let Some(path) = accepted_index_path(&record, root) {
                files.push(path);
            }

            record.clear();

            if files.len() >= limit {
                return (files, false);
            }
        }
    }

    if !record.is_empty()
        && let Some(path) = accepted_index_path(&record, root)
    {
        files.push(path);
    }

    (files, true)
}

/// One index entry, when it is a relative file that still exists.
fn accepted_index_path(record: &[u8], root: &Path) -> Option<String> {
    let path = std::str::from_utf8(record).ok()?;

    if path.is_empty() || path.starts_with('/') || path.starts_with("../") {
        return None;
    }

    // Skip deleted entries and submodule gitlinks: a mention must name a file
    // the model can actually read.
    if !root.join(path).is_file() {
        return None;
    }

    Some(path.to_owned())
}

/// A bounded recursive walk used when Git cannot index the project.
fn walk_files(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        if files.len() >= MAX_ENTRIES {
            break;
        }

        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();

            if name == ".git" || name.starts_with('.') {
                continue;
            }

            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let path = entry.path();
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                let Some(path) = relative.to_str() else {
                    continue;
                };

                files.push(path.replace('\\', "/"));
            }
        }
    }

    files
}

/// Every ancestor directory of a relative file path, each with a trailing `/`.
fn parent_directories(file: &str) -> Vec<String> {
    let mut directories = Vec::new();
    let mut prefix = String::new();

    let parts = file.split('/').collect::<Vec<_>>();

    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        if part.is_empty() {
            continue;
        }

        if !prefix.is_empty() {
            prefix.push('/');
        }

        prefix.push_str(part);
        directories.push(format!("{prefix}/"));
    }

    directories
}

#[cfg(test)]
mod tests {
    use super::{parent_directories, project_files, read_git_index};

    fn git(root: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .expect("run git");

        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn parent_directories_are_prefixes_of_the_file() {
        assert_eq!(
            parent_directories("src/tools/read.rs"),
            vec!["src/".to_owned(), "src/tools/".to_owned()]
        );
        assert!(parent_directories("README.md").is_empty());
    }

    #[test]
    fn the_git_index_reader_stops_at_its_limit() {
        let root = tempfile::TempDir::new().expect("project");
        let path = root.path();

        std::fs::write(path.join("a.txt"), "a").expect("file");
        std::fs::write(path.join("b.txt"), "b").expect("file");
        std::fs::write(path.join("c.txt"), "c").expect("file");

        let index = std::io::Cursor::new(b"a.txt\0b.txt\0c.txt\0".to_vec());
        let (files, complete) = read_git_index(index, path, 2);

        assert_eq!(files, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
        assert!(!complete, "the reader must report that it stopped early");
    }

    #[test]
    fn git_ignored_artifacts_stay_out_of_the_index() {
        let root = tempfile::TempDir::new().expect("project");
        let path = root.path();

        git(path, &["init", "--quiet"]);
        std::fs::create_dir_all(path.join("src")).expect("src");
        std::fs::write(path.join("src/lib.rs"), "fn main() {}\n").expect("file");
        std::fs::write(path.join("README.md"), "docs\n").expect("file");
        std::fs::write(path.join("ignored.log"), "secret\n").expect("file");
        std::fs::write(path.join(".gitignore"), "*.log\n").expect("ignore file");

        let files = project_files(path);

        assert!(files.contains(&"src/".to_owned()), "{files:?}");
        assert!(files.contains(&"src/lib.rs".to_owned()), "{files:?}");
        assert!(files.contains(&"README.md".to_owned()), "{files:?}");
        assert!(files.contains(&".gitignore".to_owned()), "{files:?}");
        assert!(!files.contains(&"ignored.log".to_owned()), "{files:?}");
        assert!(
            files.iter().all(|path| !path.starts_with(".git/")),
            "{files:?}"
        );
    }

    #[test]
    fn a_directory_outside_git_falls_back_to_a_filtered_walk() {
        let root = tempfile::TempDir::new().expect("project");
        let path = root.path();

        std::fs::create_dir_all(path.join("src")).expect("src");
        std::fs::create_dir_all(path.join(".hidden")).expect("hidden dir");
        std::fs::write(path.join("src/lib.rs"), "fn main() {}\n").expect("file");
        std::fs::write(path.join("visible.txt"), "text\n").expect("file");
        std::fs::write(path.join(".hidden/secret.txt"), "secret\n").expect("file");

        let files = project_files(path);

        assert!(files.contains(&"src/".to_owned()), "{files:?}");
        assert!(files.contains(&"src/lib.rs".to_owned()), "{files:?}");
        assert!(files.contains(&"visible.txt".to_owned()), "{files:?}");
        assert!(!files.contains(&".hidden/".to_owned()), "{files:?}");
        assert!(
            files.iter().all(|entry| !entry.contains(".hidden")),
            "{files:?}"
        );
    }
}
