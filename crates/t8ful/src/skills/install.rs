//! Explicit Git installation and updates for skills.
//!
//! Installation never runs repository scripts. The clone is staged in a hidden
//! temporary directory under the destination root so the final rename stays on
//! one filesystem, and every failure removes the staging tree.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{SkillRoot, SkillSource};

use super::discover::MANAGED_MARKER;
use super::metadata::{self, SkillMetadata};
use super::{InstallMarker, SkillCatalog, is_valid_name};

static OLD_SUFFIX: AtomicU64 = AtomicU64::new(0);

/// A failure the CLI maps to an exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// Invalid arguments: exit with the usage code.
    Usage(String),
    /// Git, filesystem, or refusal failure: exit with the failure code.
    Failure(String),
}

impl InstallError {
    pub fn message(&self) -> &str {
        match self {
            Self::Usage(message) | Self::Failure(message) => message,
        }
    }
}

/// Options for one install.
#[derive(Debug, Clone)]
pub struct InstallOptions<'a> {
    pub source: &'a str,
    pub revision: Option<&'a str>,
    pub subdir: Option<&'a str>,
    pub name: Option<&'a str>,
}

/// A successfully installed skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub commit: String,
    pub path: PathBuf,
    pub warnings: Vec<String>,
}

/// Result of asking t8ful to update one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    Updated {
        name: String,
        old_commit: String,
        new_commit: String,
        warnings: Vec<String>,
    },
    AlreadyCurrent {
        name: String,
        commit: String,
    },
}

struct Staged {
    /// Keeps the clone and staging tree alive until they are renamed out or
    /// dropped. Dropping it removes everything that is left.
    _temp: tempfile::TempDir,
    skill: PathBuf,
    commit: String,
    name: String,
}

/// Install a skill from a Git repository into `destination`.
pub fn install(
    roots: &[SkillRoot],
    destination: &Path,
    options: &InstallOptions<'_>,
) -> Result<Installed, InstallError> {
    let staged = stage(
        destination,
        options.source,
        options.revision,
        options.subdir,
        options.name,
    )?;
    let target = destination.join(&staged.name);

    match std::fs::symlink_metadata(&target) {
        Ok(_) => {
            return Err(InstallError::Failure(format!(
                "skill `{}` already exists at {}; use `t8ful skills update {}`",
                staged.name,
                target.display(),
                staged.name
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(InstallError::Failure(format!(
                "cannot inspect {}: {error}",
                target.display()
            )));
        }
    }

    std::fs::rename(&staged.skill, &target).map_err(|error| {
        InstallError::Failure(format!(
            "cannot move the staged skill into {}: {error}",
            target.display()
        ))
    })?;

    // A higher-precedence root may already use this name. Installation still
    // succeeds; the user needs to know which copy discovery will select.
    let catalog = SkillCatalog::discover(roots);
    let mut warnings = catalog.warnings().to_vec();

    if let Some(found) = catalog.get(&staged.name)
        && found.directory != target
    {
        warnings.push(format!(
            "skill `{}` is shadowed by the higher-precedence {} skill at {}",
            staged.name,
            found.source.label(),
            found.directory.display()
        ));
    }

    Ok(Installed {
        name: staged.name,
        commit: staged.commit,
        path: target,
        warnings,
    })
}

/// Update one discovered t8ful-managed skill.
pub fn update(
    roots: &[SkillRoot],
    name: &str,
    revision: Option<&str>,
) -> Result<UpdateOutcome, InstallError> {
    let catalog = SkillCatalog::discover(roots);
    let skill = catalog.get(name).ok_or_else(|| {
        InstallError::Usage(format!(
            "unknown skill `{name}`; use `t8ful skills list` to see the installed skills"
        ))
    })?;
    let path = &skill.directory;
    let link_metadata = std::fs::symlink_metadata(path).map_err(|error| {
        InstallError::Failure(format!("cannot inspect {}: {error}", path.display()))
    })?;

    if link_metadata.file_type().is_symlink() {
        return Err(InstallError::Failure(format!(
            "skill `{name}` at {} is Nix-managed; update it through Nix",
            path.display()
        )));
    }

    let Some(managed) = skill.managed.clone() else {
        return Err(InstallError::Failure(format!(
            "skill `{name}` at {} was not installed by t8ful; use `t8ful skills install --name <other>`",
            path.display()
        )));
    };
    let destination = path.parent().ok_or_else(|| {
        InstallError::Failure(format!(
            "skill `{name}` has no parent directory to stage an update in"
        ))
    })?;
    // Without `--rev`, a pinned install stays pinned; an unpinned install
    // follows the remote default branch.
    let target_revision = revision.or(managed.revision.as_deref());
    let staged = stage(
        destination,
        &managed.source_url,
        target_revision,
        managed.subdir.as_deref(),
        Some(name),
    )?;

    if staged.commit == managed.commit {
        return Ok(UpdateOutcome::AlreadyCurrent {
            name: name.to_owned(),
            commit: managed.commit,
        });
    }

    let suffix = OLD_SUFFIX.fetch_add(1, Ordering::Relaxed);
    let old = destination.join(format!(".t8ful-old-{}-{suffix}", std::process::id()));

    std::fs::rename(path, &old).map_err(|error| {
        InstallError::Failure(format!(
            "cannot move the current skill at {} aside: {error}",
            path.display()
        ))
    })?;

    if let Err(error) = std::fs::rename(&staged.skill, path) {
        let restore = std::fs::rename(&old, path);
        let message = match restore {
            Ok(()) => format!(
                "cannot move the updated skill into {}: {error}; the previous copy was restored",
                path.display()
            ),
            Err(restore_error) => format!(
                "cannot move the updated skill into {}: {error}; the previous copy is at {} \
                 because restoring it failed: {restore_error}",
                path.display(),
                old.display()
            ),
        };

        return Err(InstallError::Failure(message));
    }

    let mut warnings = Vec::new();

    if let Err(error) = std::fs::remove_dir_all(&old) {
        warnings.push(format!(
            "the previous copy at {} could not be removed: {error}",
            old.display()
        ));
    }

    Ok(UpdateOutcome::Updated {
        name: name.to_owned(),
        old_commit: managed.commit,
        new_commit: staged.commit,
        warnings,
    })
}

/// The root a new installation writes to.
pub fn destination_root(roots: &[SkillRoot], project: bool) -> Option<PathBuf> {
    let source = if project {
        SkillSource::Project
    } else {
        SkillSource::User
    };

    roots
        .iter()
        .find(|root| root.source == source)
        .map(|root| root.path.clone())
}

fn stage(
    destination_root: &Path,
    source: &str,
    revision: Option<&str>,
    subdir: Option<&str>,
    name_override: Option<&str>,
) -> Result<Staged, InstallError> {
    validate_source(source)?;

    if let Some(subdir) = subdir {
        validate_subdir(subdir)?;
    }

    if let Some(name) = name_override {
        validate_name(name)?;
    }

    std::fs::create_dir_all(destination_root).map_err(|error| {
        InstallError::Failure(format!(
            "cannot create the skills directory {}: {error}",
            destination_root.display()
        ))
    })?;

    let temp = tempfile::Builder::new()
        .prefix(".t8ful-tmp-")
        .tempdir_in(destination_root)
        .map_err(|error| {
            InstallError::Failure(format!(
                "cannot create a staging directory in {}: {error}",
                destination_root.display()
            ))
        })?;
    let repo = temp.path().join("repo");

    clone(source, &repo)?;

    if let Some(revision) = revision {
        let commit = resolve_revision(&repo, revision)?;

        run_git(Some(&repo), &["checkout", "--quiet", "--detach", &commit])?;
    }

    let commit = run_git(Some(&repo), &["rev-parse", "HEAD"])?;
    let source_tree = match subdir {
        Some(subdir) => {
            let candidate = repo.join(subdir);
            let canonical = std::fs::canonicalize(&candidate).map_err(|_| {
                InstallError::Usage(format!(
                    "subdirectory `{subdir}` does not exist in the repository"
                ))
            })?;
            let canonical_repo = std::fs::canonicalize(&repo).map_err(|error| {
                InstallError::Failure(format!(
                    "cannot resolve the cloned repository {}: {error}",
                    repo.display()
                ))
            })?;

            if !canonical.starts_with(&canonical_repo) {
                return Err(InstallError::Usage(format!(
                    "subdirectory `{subdir}` escapes the repository"
                )));
            }

            if !canonical.is_dir() {
                return Err(InstallError::Usage(format!(
                    "subdirectory `{subdir}` is not a directory"
                )));
            }

            canonical
        }
        None => repo,
    };

    let skill = temp.path().join("skill");

    copy_tree(&source_tree, &skill)?;

    let skill_md = skill.join("SKILL.md");
    let metadata = metadata::read(&skill_md).map_err(|error| {
        InstallError::Failure(format!("the source is not a usable skill: {error}"))
    })?;
    let name = match name_override {
        Some(name) => name.to_owned(),
        None => derived_name(&metadata, subdir, source)?,
    };
    let marker = InstallMarker {
        version: 1,
        source,
        revision,
        commit: &commit,
        subdir,
        installed_at: now_millis(),
    };
    let marker_text = toml::to_string(&marker).map_err(|error| {
        InstallError::Failure(format!("cannot serialize the install marker: {error}"))
    })?;

    std::fs::write(skill.join(MANAGED_MARKER), marker_text).map_err(|error| {
        InstallError::Failure(format!(
            "cannot write {}: {error}",
            skill.join(MANAGED_MARKER).display()
        ))
    })?;

    Ok(Staged {
        _temp: temp,
        skill,
        commit,
        name,
    })
}

fn clone(source: &str, target: &Path) -> Result<(), InstallError> {
    let output = Command::new("git")
        .args(["clone", "--quiet", "--no-hardlinks", "--"])
        .arg(source)
        .arg(target)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| InstallError::Failure(format!("cannot run git: {error}")))?;

    if output.status.success() {
        return Ok(());
    }

    Err(InstallError::Failure(git_failure(
        "git clone",
        &output.stderr,
        output.status,
    )))
}

fn run_git(directory: Option<&Path>, args: &[&str]) -> Result<String, InstallError> {
    let mut command = Command::new("git");

    command.args(args).env("GIT_TERMINAL_PROMPT", "0");

    if let Some(directory) = directory {
        command.current_dir(directory);
    }

    let output = command
        .output()
        .map_err(|error| InstallError::Failure(format!("cannot run git: {error}")))?;

    if !output.status.success() {
        return Err(InstallError::Failure(git_failure(
            &format!("git {}", args.join(" ")),
            &output.stderr,
            output.status,
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_failure(operation: &str, stderr: &[u8], status: std::process::ExitStatus) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_owned();

    if message.is_empty() {
        format!("{operation} failed ({status})")
    } else {
        format!("{operation} failed: {message}")
    }
}

fn resolve_revision(repo: &Path, revision: &str) -> Result<String, InstallError> {
    if revision.trim().is_empty() || revision.starts_with('-') {
        return Err(InstallError::Usage(format!(
            "invalid revision `{revision}`"
        )));
    }

    let expression = format!("{revision}^{{commit}}");
    let commit = run_git(
        Some(repo),
        &["rev-parse", "--verify", "--end-of-options", &expression],
    )
    .map_err(|_| {
        InstallError::Usage(format!(
            "unknown revision `{revision}` in the skill repository"
        ))
    })?;

    Ok(commit)
}

/// Copy `source` into `destination`, rejecting symlinks and an install marker.
fn copy_tree(source: &Path, destination: &Path) -> Result<(), InstallError> {
    let entries = std::fs::read_dir(source).map_err(|error| {
        InstallError::Failure(format!("cannot read {}: {error}", source.display()))
    })?;

    std::fs::create_dir_all(destination).map_err(|error| {
        InstallError::Failure(format!("cannot create {}: {error}", destination.display()))
    })?;

    for entry in entries {
        let entry = entry.map_err(|error| {
            InstallError::Failure(format!("cannot read {}: {error}", source.display()))
        })?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_type = entry.file_type().map_err(|error| {
            InstallError::Failure(format!("cannot inspect {}: {error}", path.display()))
        })?;

        if file_type.is_symlink() {
            return Err(InstallError::Failure(format!(
                "{} is a symbolic link; refusing to install it",
                path.display()
            )));
        }

        if file_name == OsStr::new(MANAGED_MARKER) {
            return Err(InstallError::Failure(format!(
                "the skill source contains {}; refusing to install it over an existing \
                 installation record",
                path.display()
            )));
        }

        if file_name == OsStr::new(".git") && file_type.is_dir() {
            continue;
        }

        let target = destination.join(&file_name);

        if file_type.is_dir() {
            copy_tree(&path, &target)?;
        } else if file_type.is_file() {
            std::fs::copy(&path, &target).map_err(|error| {
                InstallError::Failure(format!(
                    "cannot copy {} to {}: {error}",
                    path.display(),
                    target.display()
                ))
            })?;
        }
    }

    Ok(())
}

fn derived_name(
    metadata: &SkillMetadata,
    subdir: Option<&str>,
    source: &str,
) -> Result<String, InstallError> {
    if let Some(name) = metadata.name.as_deref()
        && is_valid_name(name)
    {
        return Ok(name.to_owned());
    }

    if let Some(subdir) = subdir
        && let Some(base) = Path::new(subdir).file_name().and_then(OsStr::to_str)
        && is_valid_name(base)
    {
        return Ok(base.to_owned());
    }

    let trimmed = source.trim_end_matches('/');
    let base = trimmed
        .rsplit('/')
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git");

    if is_valid_name(base) {
        Ok(base.to_owned())
    } else {
        Err(InstallError::Usage(
            "cannot derive a skill name from the source; pass --name".to_owned(),
        ))
    }
}

fn validate_source(source: &str) -> Result<(), InstallError> {
    if source.trim().is_empty() {
        return Err(InstallError::Usage(
            "the repository source must not be empty".to_owned(),
        ));
    }

    if source.starts_with('-') {
        return Err(InstallError::Usage(
            "the repository source must not start with `-`".to_owned(),
        ));
    }

    Ok(())
}

fn validate_name(name: &str) -> Result<(), InstallError> {
    if is_valid_name(name) {
        Ok(())
    } else {
        Err(InstallError::Usage(format!(
            "invalid skill name `{name}`: use one directory name without `/`, NUL, or a leading dot"
        )))
    }
}

fn validate_subdir(subdir: &str) -> Result<(), InstallError> {
    if subdir.is_empty() {
        return Err(InstallError::Usage(
            "the subdirectory must not be empty".to_owned(),
        ));
    }

    let path = Path::new(subdir);

    if path.is_absolute() {
        return Err(InstallError::Usage(format!(
            "invalid subdirectory `{subdir}`: it must be relative"
        )));
    }

    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(InstallError::Usage(format!(
                "invalid subdirectory `{subdir}`: it must not contain `.` or `..` components"
            )));
        }
    }

    Ok(())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{
        InstallError, InstallOptions, UpdateOutcome, destination_root, install, run_git, update,
    };
    use crate::config::{SkillRoot, SkillSource};
    use crate::skills::SkillCatalog;

    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: TempDir::new().expect("temp dir"),
            }
        }

        fn skills_root(&self) -> PathBuf {
            self.dir.path().join("config/t8ful/skills")
        }

        fn roots(&self) -> Vec<SkillRoot> {
            vec![SkillRoot {
                path: self.skills_root(),
                source: SkillSource::User,
            }]
        }

        fn repository(&self, name: &str) -> PathBuf {
            self.dir.path().join("repos").join(name)
        }
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }

        std::fs::write(path, contents).expect("write");
    }

    fn commit(repo: &Path, message: &str) -> String {
        run_git(Some(repo), &["add", "-A"]).expect("add");
        run_git(Some(repo), &["commit", "--quiet", "-m", message]).expect("commit");
        run_git(Some(repo), &["rev-parse", "HEAD"]).expect("head")
    }

    fn init_repository(repo: &Path, description: &str) -> String {
        std::fs::create_dir_all(repo).expect("repo");
        run_git(Some(repo), &["init", "--quiet"]).expect("init");
        run_git(Some(repo), &["config", "user.email", "test@example.com"]).expect("email");
        run_git(Some(repo), &["config", "user.name", "t8ful test"]).expect("name");
        run_git(Some(repo), &["config", "commit.gpgsign", "false"]).expect("gpg");
        write(
            &repo.join("SKILL.md"),
            &format!("---\nname: review\ndescription: {description}\n---\n\nBody.\n"),
        );
        commit(repo, "initial")
    }

    fn options<'a>(source: &'a str) -> InstallOptions<'a> {
        InstallOptions {
            source,
            revision: None,
            subdir: None,
            name: None,
        }
    }

    fn marker(directory: &Path) -> crate::skills::ManagedSkill {
        let text = std::fs::read_to_string(directory.join(".t8ful-skill.toml")).expect("marker");

        toml::from_str(&text).expect("parse marker")
    }

    #[test]
    fn install_at_head_records_the_resolved_commit() {
        let fixture = Fixture::new();
        let repo = fixture.repository("review");
        let head = init_repository(&repo, "Review a change.");
        let source = repo.display().to_string();

        let installed =
            install(&fixture.roots(), &fixture.skills_root(), &options(&source)).expect("install");

        assert_eq!(installed.name, "review");
        assert_eq!(installed.commit, head);
        assert_eq!(installed.path, fixture.skills_root().join("review"));

        let recorded = marker(&installed.path);

        assert_eq!(recorded.commit, head);
        assert_eq!(recorded.revision, None);
        assert_eq!(recorded.source_url, source);

        let catalog = SkillCatalog::discover(&fixture.roots());
        let skill = catalog.get("review").expect("discovered");

        assert_eq!(
            skill.managed.as_ref().map(|managed| managed.commit.clone()),
            Some(head)
        );
        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
    }

    #[test]
    fn install_at_a_pinned_revision_checks_out_that_commit() {
        let fixture = Fixture::new();
        let repo = fixture.repository("review");
        let first = init_repository(&repo, "First description.");

        write(
            &repo.join("SKILL.md"),
            "---\nname: review\ndescription: Second description.\n---\n\nSecond body.\n",
        );
        commit(&repo, "second");

        let source = repo.display().to_string();
        let mut request = options(&source);

        request.revision = Some(&first[..8]);

        let installed =
            install(&fixture.roots(), &fixture.skills_root(), &request).expect("pinned install");

        assert_eq!(installed.commit, first);
        assert_eq!(
            marker(&installed.path).revision.as_deref(),
            Some(&first[..8])
        );

        let contents = std::fs::read_to_string(installed.path.join("SKILL.md")).expect("skill");

        assert!(contents.contains("First description."), "{contents}");
    }

    #[test]
    fn a_subdirectory_install_copies_only_that_tree() {
        let fixture = Fixture::new();
        let repo = fixture.repository("monorepo");
        let skill = repo.join("skills/review");

        std::fs::create_dir_all(&repo).expect("repo");
        run_git(Some(&repo), &["init", "--quiet"]).expect("init");
        run_git(Some(&repo), &["config", "user.email", "test@example.com"]).expect("email");
        run_git(Some(&repo), &["config", "user.name", "t8ful test"]).expect("name");
        run_git(Some(&repo), &["config", "commit.gpgsign", "false"]).expect("gpg");
        write(
            &skill.join("SKILL.md"),
            "---\ndescription: A monorepo skill.\n---\nbody\n",
        );
        write(&skill.join("references/checklist.md"), "checklist");
        write(&repo.join("README.md"), "repository readme");
        commit(&repo, "initial");

        let source = repo.display().to_string();
        let mut request = options(&source);

        request.subdir = Some("skills/review");

        let installed =
            install(&fixture.roots(), &fixture.skills_root(), &request).expect("subdir install");

        assert_eq!(installed.name, "review");
        assert!(installed.path.join("references/checklist.md").is_file());
        assert!(!installed.path.join("README.md").exists());
        assert!(!installed.path.join("skills").exists());
    }

    #[test]
    fn path_traversal_and_invalid_names_are_usage_errors() {
        let fixture = Fixture::new();
        let repo = fixture.repository("review");

        init_repository(&repo, "Review a change.");

        let source = repo.display().to_string();

        for subdir in ["../escape", "/absolute", "skills/../../escape", "."] {
            let mut request = options(&source);

            request.subdir = Some(subdir);

            let error = install(&fixture.roots(), &fixture.skills_root(), &request)
                .expect_err("subdir rejected");

            assert!(
                matches!(error, InstallError::Usage(_)),
                "{subdir}: {error:?}"
            );
        }

        for name in ["../escape", ".hidden", "a/b", ""] {
            let mut request = options(&source);

            request.name = Some(name);

            let error = install(&fixture.roots(), &fixture.skills_root(), &request)
                .expect_err("name rejected");

            assert!(matches!(error, InstallError::Usage(_)), "{name}: {error:?}");
        }

        assert!(!fixture.skills_root().exists());
    }

    #[test]
    fn a_symlink_in_the_source_is_refused_and_leaves_nothing_behind() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let repo = fixture.repository("review");

        init_repository(&repo, "Review a change.");
        symlink("/etc/passwd", repo.join("passwd")).expect("symlink");
        commit(&repo, "symlink");

        let source = repo.display().to_string();
        let error = install(&fixture.roots(), &fixture.skills_root(), &options(&source))
            .expect_err("symlink refused");

        assert!(matches!(error, InstallError::Failure(_)), "{error:?}");
        assert!(!fixture.skills_root().join("review").exists());
        assert_no_temp_entries(&fixture.skills_root());
    }

    #[test]
    fn an_existing_or_nix_managed_skill_is_not_overwritten() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let repo = fixture.repository("review");

        init_repository(&repo, "Review a change.");

        let source = repo.display().to_string();
        let destination = fixture.skills_root().join("review");

        write(
            &destination.join("SKILL.md"),
            "---\ndescription: Hand written.\n---\nbody\n",
        );

        let error = install(&fixture.roots(), &fixture.skills_root(), &options(&source))
            .expect_err("existing refused");

        assert!(matches!(error, InstallError::Failure(_)), "{error:?}");
        assert!(
            error.message().contains("already exists"),
            "{}",
            error.message()
        );
        assert!(
            error.message().contains("use `t8ful skills update review`"),
            "{}",
            error.message()
        );

        std::fs::remove_dir_all(&destination).expect("remove");
        let store = fixture.dir.path().join("store/review");

        write(
            &store.join("SKILL.md"),
            "---\ndescription: Nix managed.\n---\nbody\n",
        );

        std::fs::create_dir_all(fixture.skills_root()).expect("root");
        symlink(&store, &destination).expect("symlink");

        let error = install(&fixture.roots(), &fixture.skills_root(), &options(&source))
            .expect_err("symlink refused");

        assert!(matches!(error, InstallError::Failure(_)), "{error:?}");
    }

    #[test]
    fn an_unusable_source_skill_md_is_refused() {
        let fixture = Fixture::new();
        let repo = fixture.repository("broken");

        std::fs::create_dir_all(&repo).expect("repo");
        run_git(Some(&repo), &["init", "--quiet"]).expect("init");
        run_git(Some(&repo), &["config", "user.email", "test@example.com"]).expect("email");
        run_git(Some(&repo), &["config", "user.name", "t8ful test"]).expect("name");
        run_git(Some(&repo), &["config", "commit.gpgsign", "false"]).expect("gpg");
        write(&repo.join("SKILL.md"), "no frontmatter here");
        commit(&repo, "initial");

        let source = repo.display().to_string();
        let error = install(&fixture.roots(), &fixture.skills_root(), &options(&source))
            .expect_err("invalid skill");

        assert!(matches!(error, InstallError::Failure(_)), "{error:?}");
        assert_no_temp_entries(&fixture.skills_root());
    }

    #[test]
    fn repository_scripts_are_never_executed() {
        let fixture = Fixture::new();
        let repo = fixture.repository("review");
        let sentinel = fixture.dir.path().join("setup-ran");

        init_repository(&repo, "Review a change.");
        write(
            &repo.join("setup.sh"),
            &format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
        );
        commit(&repo, "add setup");

        let source = repo.display().to_string();

        install(&fixture.roots(), &fixture.skills_root(), &options(&source)).expect("install");

        assert!(
            !sentinel.exists(),
            "installation executed a repository script"
        );
    }

    #[test]
    fn a_failed_install_leaves_neither_a_destination_nor_a_staging_tree() {
        let fixture = Fixture::new();
        let repo = fixture.repository("review");

        init_repository(&repo, "Review a change.");

        let source = repo.display().to_string();
        let mut request = options(&source);

        request.revision = Some("does-not-exist");

        let error = install(&fixture.roots(), &fixture.skills_root(), &request)
            .expect_err("unknown revision");

        assert!(matches!(error, InstallError::Usage(_)), "{error:?}");
        assert!(!fixture.skills_root().join("review").exists());
        assert_no_temp_entries(&fixture.skills_root());
    }

    #[test]
    fn update_follows_the_default_branch_for_an_unpinned_install() {
        let fixture = Fixture::new();
        let repo = fixture.repository("review");

        init_repository(&repo, "First description.");

        let source = repo.display().to_string();
        let installed =
            install(&fixture.roots(), &fixture.skills_root(), &options(&source)).expect("install");

        write(
            &repo.join("SKILL.md"),
            "---\nname: review\ndescription: Second description.\n---\nSecond body.\n",
        );
        let second = commit(&repo, "second");

        let outcome = update(&fixture.roots(), "review", None).expect("update");

        assert_eq!(
            outcome,
            UpdateOutcome::Updated {
                name: "review".to_owned(),
                old_commit: installed.commit,
                new_commit: second.clone(),
                warnings: Vec::new(),
            }
        );

        let contents =
            std::fs::read_to_string(fixture.skills_root().join("review/SKILL.md")).expect("skill");

        assert!(contents.contains("Second description."), "{contents}");
        assert_eq!(marker(&fixture.skills_root().join("review")).commit, second);
    }

    #[test]
    fn update_keeps_a_pinned_install_pinned_and_accepts_an_explicit_revision() {
        let fixture = Fixture::new();
        let repo = fixture.repository("review");
        let first = init_repository(&repo, "First description.");

        let source = repo.display().to_string();
        let mut request = options(&source);

        request.revision = Some(&first[..8]);

        let installed =
            install(&fixture.roots(), &fixture.skills_root(), &request).expect("pinned install");

        write(
            &repo.join("SKILL.md"),
            "---\nname: review\ndescription: Second description.\n---\nSecond body.\n",
        );
        let second = commit(&repo, "second");

        let outcome = update(&fixture.roots(), "review", None).expect("update");

        assert_eq!(
            outcome,
            UpdateOutcome::AlreadyCurrent {
                name: "review".to_owned(),
                commit: installed.commit,
            }
        );

        let outcome = update(&fixture.roots(), "review", Some(&second[..8])).expect("update");

        assert!(matches!(outcome, UpdateOutcome::Updated { .. }));
        assert_eq!(marker(&fixture.skills_root().join("review")).commit, second);
    }

    #[test]
    fn update_refuses_unmanaged_and_nix_managed_skills() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let destination = fixture.skills_root().join("review");

        write(
            &destination.join("SKILL.md"),
            "---\ndescription: Hand written.\n---\nbody\n",
        );

        let error = update(&fixture.roots(), "review", None).expect_err("unmanaged");

        assert!(matches!(error, InstallError::Failure(_)), "{error:?}");
        assert!(error.message().contains("was not installed by t8ful"));

        std::fs::remove_dir_all(&destination).expect("remove");
        let store = fixture.dir.path().join("store/review");

        write(
            &store.join("SKILL.md"),
            "---\ndescription: Nix managed.\n---\nbody\n",
        );
        symlink(&store, &destination).expect("symlink");

        let error = update(&fixture.roots(), "review", None).expect_err("nix managed");

        assert!(matches!(error, InstallError::Failure(_)), "{error:?}");
        assert!(error.message().contains("Nix-managed"));
    }

    #[test]
    fn an_unknown_update_name_is_a_usage_error() {
        let fixture = Fixture::new();
        let error = update(&fixture.roots(), "missing", None).expect_err("unknown");

        assert!(matches!(error, InstallError::Usage(_)), "{error:?}");
        assert!(error.message().contains("unknown skill"));
    }

    #[test]
    fn the_destination_follows_the_requested_source() {
        let fixture = Fixture::new();
        let roots = vec![
            SkillRoot {
                path: fixture.dir.path().join("project/.t8ful/skills"),
                source: SkillSource::Project,
            },
            SkillRoot {
                path: fixture.skills_root(),
                source: SkillSource::User,
            },
        ];

        assert_eq!(destination_root(&roots, false), Some(fixture.skills_root()));
        assert_eq!(
            destination_root(&roots, true),
            Some(fixture.dir.path().join("project/.t8ful/skills"))
        );
    }

    fn assert_no_temp_entries(root: &Path) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };

        for entry in entries.filter_map(Result::ok) {
            assert!(
                !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".t8ful-tmp-"),
                "staging directory left behind: {:?}",
                entry.path()
            );
        }
    }
}
