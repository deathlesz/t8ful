//! Small filesystem helpers shared by the file tools.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Refuse to mutate a path whose leaf is a symbolic link.
///
/// Symlinked parent directories remain usable: coding mode is not a filesystem
/// confinement boundary. A leaf link is rejected whether it is live or dangling
/// because either way the name does not identify the file being changed.
pub(crate) fn reject_leaf_symlink(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "{} is a symbolic link; refusing to write through it",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

/// Write `contents` to `path` through a temporary file in the same directory so
/// a crash cannot leave a half-written file behind.
///
/// The temporary file is synced before the rename and the directory after it, so
/// the published contents are on disk and not merely in the page cache.
pub(crate) fn write_atomically(
    path: &Path,
    contents: &str,
    create_parents: bool,
) -> io::Result<()> {
    reject_leaf_symlink(path).map_err(io::Error::other)?;

    let parent = prepare_parent(path, create_parents)?;
    let existing_mode = std::fs::metadata(path)
        .ok()
        .map(|metadata| mode_of(&metadata));
    let temp = temp_path(path);

    let result =
        write_temp(&temp, contents, existing_mode).and_then(|()| std::fs::rename(&temp, path));

    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }

    if result.is_ok() {
        // The rename already published the contents. A filesystem that refuses a
        // directory sync, or a writable-but-unreadable directory, must not turn a
        // successful write into a reported failure; the durability gap is not
        // something the caller can act on.
        let _ = sync_directory(parent);
    }

    result
}

/// Resolve the directory a write lands in, creating it when that is allowed.
///
/// The temporary file has to live in the destination directory: the rename that
/// publishes it must stay on one filesystem to be atomic.
fn prepare_parent(path: &Path, create_parents: bool) -> io::Result<&Path> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    if !parent.exists() {
        if create_parents {
            std::fs::create_dir_all(parent)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("directory {} does not exist", parent.display()),
            ));
        }
    }

    Ok(parent)
}

/// Create `path` exclusively, fill it, and flush it to disk.
///
/// `create_new` opens with `O_CREAT|O_EXCL`, which refuses any name that is
/// already taken, including a symbolic link, and never follows one. A temp name
/// planted by another process therefore fails the write instead of redirecting
/// it into a file of that process's choosing.
fn write_temp(path: &Path, contents: &str, mode: Option<u32>) -> io::Result<()> {
    use std::io::Write;

    let mut file = create_temp_file(path, mode.is_some())?;

    file.write_all(contents.as_bytes())?;
    file.flush()?;

    // A replacement starts owner-only and only gains the destination's mode
    // after the bytes are written, so a restrictive file is never widened
    // while its replacement is in progress.
    if let Some(mode) = mode {
        set_file_mode(&file, mode)?;
    }

    file.sync_all()
}

/// Create the replacement temp file.
///
/// A replacement starts at `0o600`; a brand-new destination keeps the umask
/// default. `create_new` refuses a name that is already taken or a symlink.
fn create_temp_file(path: &Path, replacement: bool) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();

    options.write(true).create_new(true);

    #[cfg(unix)]
    if replacement {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }

    let _ = replacement;

    options.open(path)
}

/// Identity of an existing file, used to reject two spellings of one target.
pub(crate) fn file_identity(path: &Path) -> io::Result<Option<(u64, u64)>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        match std::fs::metadata(path) {
            Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = path;

        Ok(None)
    }
}

/// Sync a directory so the rename inside it survives a crash.
#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Create a new file without replacing a destination that appeared after
/// validation. The temporary file lives in the destination directory, so the
/// final `persist_noclobber` is an atomic same-filesystem operation.
pub(crate) fn write_new_atomically(
    path: &Path,
    contents: &str,
    create_parents: bool,
) -> io::Result<()> {
    use std::io::Write;

    reject_leaf_symlink(path).map_err(io::Error::other)?;

    let parent = prepare_parent(path, create_parents)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;

    temp.write_all(contents.as_bytes())?;
    temp.flush()?;
    set_file_mode(temp.as_file(), 0o644)?;

    match temp.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", path.display()),
        )),
        Err(error) => Err(error.error),
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let temp_name = format!(".{name}.t8ful-tmp-{}-{counter}", std::process::id());

    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(temp_name),
        _ => PathBuf::from(temp_name),
    }
}

#[cfg(unix)]
fn mode_of(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn mode_of(_metadata: &std::fs::Metadata) -> u32 {
    0
}

/// Set a mode on the open file, never on a name that could be re-pointed.
#[cfg(unix)]
fn set_file_mode(file: &std::fs::File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &std::fs::File, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Read a UTF-8 file, reporting non-UTF-8 content as an actionable error.
pub(crate) fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => format!("{} does not exist", path.display()),
        io::ErrorKind::IsADirectory => format!("{} is a directory", path.display()),
        _ => format!("cannot read {}: {error}", path.display()),
    })?;

    String::from_utf8(bytes).map_err(|_| {
        format!(
            "{} is not valid UTF-8; this tool only handles text files",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        create_temp_file, reject_leaf_symlink, temp_path, write_atomically, write_new_atomically,
        write_temp,
    };

    #[test]
    fn refuses_to_create_missing_parents_by_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("nested/deeper/file.txt");

        let error = write_atomically(&target, "hello", false).expect_err("missing parent");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!target.exists());
    }

    #[test]
    fn creates_parents_when_asked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("nested/deeper/file.txt");

        write_atomically(&target, "hello", true).expect("write succeeds");

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_existing_and_dangling_leaf_symlinks() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target.txt");
        let live = dir.path().join("live.txt");
        let dangling = dir.path().join("dangling.txt");

        std::fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink("target.txt", &live).unwrap();
        std::os::unix::fs::symlink("missing.txt", &dangling).unwrap();

        assert!(reject_leaf_symlink(&live).is_err());
        assert!(reject_leaf_symlink(&dangling).is_err());
        assert!(reject_leaf_symlink(&target).is_ok());
        assert!(write_atomically(&live, "new", false).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(
            std::fs::symlink_metadata(&dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// A temp name another process already occupies is never followed.
    ///
    /// Regression: the temp file was created with `std::fs::write`, which
    /// truncates and follows a symbolic link, so a symlink planted at the temp
    /// name redirected the write into a file of the planter's choosing.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_at_the_temp_name_is_not_followed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("file.txt");
        let planted = dir.path().join("planted.txt");
        let temp = temp_path(&target);

        std::fs::write(&target, "old").unwrap();
        std::fs::write(&planted, "untouched").unwrap();
        std::os::unix::fs::symlink(&planted, &temp).unwrap();

        let error = write_temp(&temp, "new", None).expect_err("the name is taken");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&planted).unwrap(), "untouched");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    }

    /// A destination survives a write whose temp file cannot be created.
    #[cfg(unix)]
    #[test]
    fn a_destination_keeps_its_bytes_when_the_temp_cannot_be_created() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let readonly = dir.path().join("readonly");
        let target = readonly.join("file.txt");

        std::fs::create_dir(&readonly).unwrap();
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = write_atomically(&target, "new", false);

        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            result.is_err(),
            "a write into a read-only directory succeeded"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    }

    #[test]
    fn persist_noclobber_rejects_a_file_created_before_persistence() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("created.txt");

        std::fs::write(&target, "winner").unwrap();

        let error = write_new_atomically(&target, "loser", false)
            .expect_err("must not clobber an existing destination");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "winner");
    }

    #[cfg(unix)]
    #[test]
    fn a_replacement_temp_starts_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let replacement = dir.path().join("replacement");
        let file = create_temp_file(&replacement, true).expect("create replacement temp");

        let mode = file.metadata().unwrap().permissions().mode() & 0o777;

        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_restrictive_destination_keeps_its_mode_after_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("secret.txt");

        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        write_atomically(&target, "new", false).expect("write");

        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn replaces_contents_without_leaving_temp_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("file.txt");

        std::fs::write(&target, "old").unwrap();
        write_atomically(&target, "new", false).expect("write succeeds");

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");

        let entries = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(entries, vec!["file.txt".to_owned()]);
    }
}
