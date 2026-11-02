//! Ordered skill discovery.
//!
//! Roots are scanned in precedence order (project, user, system, extras). The
//! first skill with a given name wins; a later duplicate produces a warning
//! naming both paths instead of disappearing silently.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{SkillRoot, SkillSource};

use super::metadata::{self, SkillMetadata};
use super::{DiscoveredSkill, ManagedSkill, is_valid_name};

/// Name of the install marker that identifies a writable, t8ful-managed skill.
pub const MANAGED_MARKER: &str = ".t8ful-skill.toml";

/// Largest supporting-file listing attached to one on-demand load.
pub const MAX_LISTING_ENTRIES: usize = 200;

/// Deepest directory nesting followed while listing supporting files.
const MAX_LISTING_DEPTH: usize = 8;

/// Discover every usable skill under `roots`.
pub fn discover(roots: &[SkillRoot]) -> (Vec<DiscoveredSkill>, Vec<String>) {
    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    let mut seen: BTreeMap<String, (PathBuf, SkillSource)> = BTreeMap::new();

    for root in roots {
        let entries = match std::fs::read_dir(&root.path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(format!(
                    "cannot read skill root {}: {error}",
                    root.path.display()
                ));

                continue;
            }
        };

        let mut candidates: Vec<(String, PathBuf)> = Vec::new();

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warnings.push(format!(
                        "cannot read an entry of skill root {}: {error}",
                        root.path.display()
                    ));

                    continue;
                }
            };

            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                warnings.push(format!(
                    "skill root {} contains a non-UTF-8 entry; skipping it",
                    root.path.display()
                ));

                continue;
            };

            if name.starts_with('.') {
                continue;
            }

            let path = entry.path();
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!("cannot inspect skill {}: {error}", path.display()));

                    continue;
                }
            };

            // `metadata` follows symlinks, so a Nix-managed link to a store
            // directory is discovered while plain files are ignored.
            if !metadata.is_dir() {
                continue;
            }

            candidates.push((name.to_owned(), path));
        }

        candidates.sort_by(|left, right| left.0.cmp(&right.0));

        for (name, directory) in candidates {
            let skill_md = directory.join("SKILL.md");
            match std::fs::metadata(&skill_md) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => {
                    warnings.push(format!(
                        "skipping skill {name} at {}: SKILL.md is not a regular file",
                        directory.display()
                    ));

                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warnings.push(format!("cannot inspect {}: {error}", skill_md.display()));

                    continue;
                }
            }

            let metadata = match metadata::read(&skill_md) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!(
                        "skipping skill {name} at {}: {error}",
                        directory.display()
                    ));

                    continue;
                }
            };

            warn_about_declared_name(&name, &directory, &metadata, &mut warnings);

            if let Some((higher_path, higher_source)) = seen.get(&name) {
                warnings.push(format!(
                    "skill `{name}` at {} is shadowed by the higher-precedence {} skill at {}",
                    directory.display(),
                    higher_source.label(),
                    higher_path.display()
                ));

                continue;
            }

            let managed = read_managed(&directory, &mut warnings);

            seen.insert(name.clone(), (directory.clone(), root.source));
            skills.push(DiscoveredSkill {
                name,
                description: metadata.description,
                directory,
                source: root.source,
                managed,
            });
        }
    }

    (skills, warnings)
}

/// Warn when the frontmatter name is unusable or disagrees with the directory.
/// The directory name always wins so a copied skill cannot hijack another.
fn warn_about_declared_name(
    directory_name: &str,
    directory: &Path,
    metadata: &SkillMetadata,
    warnings: &mut Vec<String>,
) {
    let Some(declared) = &metadata.name else {
        return;
    };

    if !is_valid_name(declared) {
        warnings.push(format!(
            "skill at {} declares invalid name `{declared}`; using the directory name \
             `{directory_name}`",
            directory.display()
        ));
    } else if declared != directory_name {
        warnings.push(format!(
            "skill at {} declares name `{declared}` but its directory is `{directory_name}`; \
             using the directory name",
            directory.display()
        ));
    }
}

fn read_managed(directory: &Path, warnings: &mut Vec<String>) -> Option<ManagedSkill> {
    let path = directory.join(MANAGED_MARKER);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            warnings.push(format!("cannot read {}: {error}", path.display()));

            return None;
        }
    };

    match toml::from_str::<ManagedSkill>(&text) {
        Ok(managed) => Some(managed),
        Err(error) => {
            warnings.push(format!(
                "ignoring malformed {} (treating the skill as unmanaged): {error}",
                path.display()
            ));

            None
        }
    }
}

/// List supporting files relative to the skill directory, sorted.
pub fn supporting_files(directory: &Path) -> (Vec<PathBuf>, bool) {
    let mut files = Vec::new();
    let mut truncated = false;

    collect_files(directory, directory, 0, &mut files, &mut truncated);
    files.sort();

    (files, truncated)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
    truncated: &mut bool,
) {
    if files.len() >= MAX_LISTING_ENTRIES {
        *truncated = true;

        return;
    }

    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();

    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };

        // Dot entries cover `.git` and the install marker.
        if name.starts_with('.') {
            continue;
        }

        if depth == 0 && name == "SKILL.md" {
            continue;
        }

        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        // Symlinked files and directories are not listed: a skill's support
        // files are its own content, and following links could loop.
        if metadata.file_type().is_symlink() {
            continue;
        }

        if metadata.is_dir() {
            if depth >= MAX_LISTING_DEPTH {
                continue;
            }

            collect_files(root, &path, depth + 1, files, truncated);
        } else if metadata.is_file() {
            if files.len() >= MAX_LISTING_ENTRIES {
                *truncated = true;

                return;
            }

            if let Ok(relative) = path.strip_prefix(root) {
                files.push(relative.to_path_buf());
            }
        }

        if *truncated {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::MAX_LISTING_ENTRIES;
    use crate::config::{SkillRoot, SkillSource};
    use crate::skills::SkillCatalog;

    fn write_skill(root: &Path, name: &str, description: &str, extra: &[(&str, &str)]) {
        let directory = root.join(name);

        std::fs::create_dir_all(&directory).expect("skill dir");
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n"),
        )
        .expect("skill file");

        for (relative, contents) in extra {
            let path = directory.join(relative);

            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("support dir");
            }

            std::fs::write(path, contents).expect("support file");
        }
    }

    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: TempDir::new().expect("temp dir"),
            }
        }
    }

    #[test]
    fn roots_are_scanned_in_precedence_order_then_by_name() {
        let fixture = Fixture::new();
        let project = fixture.dir.path().join("project");
        let user = fixture.dir.path().join("user");

        write_skill(&project, "zeta", "project zeta", &[]);
        write_skill(&project, "alpha", "project alpha", &[]);
        write_skill(&user, "shared", "user shared", &[]);

        let catalog = SkillCatalog::discover(&[
            SkillRoot {
                path: project,
                source: SkillSource::Project,
            },
            SkillRoot {
                path: user,
                source: SkillSource::User,
            },
        ]);

        assert_eq!(
            catalog
                .skills()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta", "shared"]
        );
        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
    }

    #[test]
    fn a_duplicate_name_prefers_the_higher_root_and_warns_about_both_paths() {
        let fixture = Fixture::new();
        let project = fixture.dir.path().join("project");
        let user = fixture.dir.path().join("user");

        write_skill(&project, "review", "project review", &[]);
        write_skill(&user, "review", "user review", &[]);

        let catalog = SkillCatalog::discover(&[
            SkillRoot {
                path: project.clone(),
                source: SkillSource::Project,
            },
            SkillRoot {
                path: user.clone(),
                source: SkillSource::User,
            },
        ]);

        assert_eq!(catalog.skills().len(), 1);
        assert_eq!(catalog.skills()[0].description, "project review");
        assert_eq!(catalog.skills()[0].directory, project.join("review"));

        let warning = catalog
            .warnings()
            .iter()
            .find(|warning| warning.contains("shadowed"))
            .expect("collision warning");

        assert!(warning.contains(&project.join("review").display().to_string()));
        assert!(warning.contains(&user.join("review").display().to_string()));
    }

    #[test]
    fn a_symlinked_skill_directory_is_discovered() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let store = fixture.dir.path().join("store/review");
        let root = fixture.dir.path().join("user");

        write_skill(&fixture.dir.path().join("store"), "review", "linked", &[]);
        std::fs::create_dir_all(&root).expect("root");
        symlink(&store, root.join("review")).expect("symlink");

        let catalog = SkillCatalog::discover(&[SkillRoot {
            path: root.clone(),
            source: SkillSource::User,
        }]);

        assert_eq!(catalog.skills().len(), 1);
        assert_eq!(catalog.skills()[0].directory, root.join("review"));
    }

    #[test]
    fn unusable_skills_are_skipped_while_their_neighbours_load() {
        let fixture = Fixture::new();
        let root = fixture.dir.path().join("user");

        write_skill(&root, "good", "usable", &[]);

        let broken = root.join("broken");
        std::fs::create_dir_all(&broken).expect("broken dir");
        std::fs::write(broken.join("SKILL.md"), "not a frontmatter file").expect("broken");

        let missing_description = root.join("missing");
        std::fs::create_dir_all(&missing_description).expect("missing dir");
        std::fs::write(
            missing_description.join("SKILL.md"),
            "---\nname: missing\n---\nbody\n",
        )
        .expect("missing");

        let no_skill_md = root.join("plain");
        std::fs::create_dir_all(&no_skill_md).expect("plain dir");

        let catalog = SkillCatalog::discover(&[SkillRoot {
            path: root,
            source: SkillSource::User,
        }]);

        assert_eq!(
            catalog
                .skills()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["good"]
        );
        assert_eq!(catalog.warnings().len(), 2, "{:?}", catalog.warnings());
    }

    #[test]
    fn a_frontmatter_directory_mismatch_warns_and_keeps_the_directory_name() {
        let fixture = Fixture::new();
        let root = fixture.dir.path().join("user");
        let directory = root.join("actual");

        std::fs::create_dir_all(&directory).expect("skill dir");
        std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: declared\ndescription: Review.\n---\nbody\n",
        )
        .expect("skill");

        let catalog = SkillCatalog::discover(&[SkillRoot {
            path: root,
            source: SkillSource::User,
        }]);

        assert_eq!(catalog.skills()[0].name, "actual");
        assert!(
            catalog
                .warnings()
                .iter()
                .any(|warning| warning.contains("declares name `declared`"))
        );

        std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: ../escape\ndescription: Review.\n---\nbody\n",
        )
        .expect("skill");

        let catalog = SkillCatalog::discover(&[SkillRoot {
            path: fixture.dir.path().join("user"),
            source: SkillSource::User,
        }]);

        assert_eq!(catalog.skills()[0].name, "actual");
        assert!(
            catalog
                .warnings()
                .iter()
                .any(|warning| warning.contains("invalid name"))
        );
    }

    #[test]
    fn a_malformed_install_marker_is_reported_and_treated_as_unmanaged() {
        let fixture = Fixture::new();
        let root = fixture.dir.path().join("user");

        write_skill(&root, "review", "Review a change.", &[]);
        std::fs::write(root.join("review/.t8ful-skill.toml"), "not = [valid toml").expect("marker");

        let catalog = SkillCatalog::discover(&[SkillRoot {
            path: root,
            source: SkillSource::User,
        }]);
        let skill = catalog.get("review").expect("discovered");

        assert!(skill.managed.is_none());
        assert!(
            catalog
                .warnings()
                .iter()
                .any(|warning| warning.contains("malformed")
                    && warning.contains(".t8ful-skill.toml")),
            "{:?}",
            catalog.warnings()
        );
    }

    #[test]
    fn hidden_entries_and_plain_files_are_ignored_and_a_missing_root_is_silent() {
        let fixture = Fixture::new();
        let root = fixture.dir.path().join("user");

        std::fs::create_dir_all(root.join(".hidden")).expect("hidden dir");
        std::fs::write(root.join(".hidden/SKILL.md"), "---\ndescription: x\n---\n")
            .expect("hidden skill");
        std::fs::write(root.join("README.md"), "not a skill").expect("file");

        let catalog = SkillCatalog::discover(&[
            SkillRoot {
                path: root,
                source: SkillSource::User,
            },
            SkillRoot {
                path: fixture.dir.path().join("missing"),
                source: SkillSource::System,
            },
        ]);

        assert!(catalog.skills().is_empty());
        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
    }

    #[test]
    fn load_returns_the_body_and_a_sorted_supporting_file_listing() {
        let fixture = Fixture::new();
        let root = fixture.dir.path().join("user");

        write_skill(
            &root,
            "review",
            "Review a change.",
            &[
                ("references/checklist.md", "checklist"),
                ("scripts/check.sh", "#!/bin/sh\n"),
                ("notes.txt", "notes"),
                (".hidden", "hidden"),
            ],
        );

        let catalog = SkillCatalog::discover(&[SkillRoot {
            path: root.clone(),
            source: SkillSource::User,
        }]);
        let document = catalog.load("review").expect("load");

        assert_eq!(document.name, "review");
        assert_eq!(document.directory, root.join("review"));
        assert!(document.body.contains("Body of review."));
        assert_eq!(
            document.files,
            vec![
                PathBuf::from("notes.txt"),
                PathBuf::from("references/checklist.md"),
                PathBuf::from("scripts/check.sh"),
            ]
        );
        assert!(!document.truncated);
    }

    #[test]
    fn the_supporting_file_listing_is_capped() {
        let fixture = Fixture::new();
        let root = fixture.dir.path().join("user");
        let directory = root.join("many");

        std::fs::create_dir_all(&directory).expect("skill dir");
        std::fs::write(
            directory.join("SKILL.md"),
            "---\ndescription: Many files.\n---\nbody\n",
        )
        .expect("skill");

        for index in 0..=MAX_LISTING_ENTRIES {
            std::fs::write(directory.join(format!("file-{index:03}.txt")), "x").expect("file");
        }

        let catalog = SkillCatalog::discover(&[SkillRoot {
            path: root,
            source: SkillSource::User,
        }]);
        let document = catalog.load("many").expect("load");

        assert_eq!(document.files.len(), MAX_LISTING_ENTRIES);
        assert!(document.truncated);
    }

    #[test]
    fn an_unknown_name_lists_the_available_skills() {
        let fixture = Fixture::new();
        let root = fixture.dir.path().join("user");

        write_skill(&root, "review", "Review.", &[]);

        let catalog = SkillCatalog::discover(&[SkillRoot {
            path: root,
            source: SkillSource::User,
        }]);
        let error = catalog.load("missing").expect_err("unknown skill");

        assert!(error.contains("missing"), "{error}");
        assert!(error.contains("review"), "{error}");

        let empty = SkillCatalog::discover(&[]);
        let error = empty.load("review").expect_err("no skills");

        assert!(error.contains("no skills"), "{error}");
    }
}
