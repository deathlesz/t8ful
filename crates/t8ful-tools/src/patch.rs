use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;
use diffy::patch_set::{FileOperation, ParseOptions, PatchSet};
use serde::Deserialize;
use serde_json::json;
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

use crate::fsutil::{file_identity, read_text, write_atomically, write_new_atomically};
use crate::paths;

#[derive(Debug, Deserialize)]
struct PatchArgs {
    patch: String,
}

#[derive(Debug, Default)]
pub struct ApplyPatchTool;

struct PlannedFile {
    path: String,
    resolved: PathBuf,
    contents: String,
    create: bool,
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".to_owned(),
            description: "Apply a unified diff to one or more files. Every hunk is validated \
                          against the current contents first, so a conflict or malformed patch \
                          leaves all files unchanged. Creating files and modifying files are \
                          supported; deleting, renaming, and binary patches are not."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Unified diff text, optionally in git format" }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: PatchArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;

        if args.patch.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "patch must not be empty".to_owned(),
            ));
        }

        let options = parse_options(&args.patch);
        let mut planned: Vec<PlannedFile> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        let mut targets: HashSet<PathBuf> = HashSet::new();
        let mut identities: HashSet<(u64, u64)> = HashSet::new();

        for entry in PatchSet::parse(&args.patch, options) {
            let file = match entry {
                Ok(file) => file,
                Err(error) => {
                    problems.push(format!("cannot parse the patch: {error}"));

                    break;
                }
            };

            let outcome = plan_file(&file, ctx);

            match outcome {
                Ok(file) => {
                    // Two spellings can reach one inode through a symlinked
                    // parent or a hard link. Planning happens before any write,
                    // so rejecting the second spelling leaves every file
                    // unchanged.
                    let duplicate = if !targets.insert(file.resolved.clone()) {
                        true
                    } else {
                        match file_identity(&file.resolved) {
                            Ok(Some(identity)) => !identities.insert(identity),
                            Ok(None) => false,
                            Err(error) => {
                                problems.push(format!("cannot inspect {}: {error}", file.path));

                                continue;
                            }
                        }
                    };

                    if duplicate {
                        problems.push(format!(
                            "{} appears more than once in the patch; every target must appear once",
                            file.path
                        ));
                    } else {
                        planned.push(file);
                    }
                }
                Err(problem) => problems.push(problem),
            }
        }

        if !problems.is_empty() {
            return Err(ToolError::Failed(format!(
                "patch not applied, no files were written: {}",
                problems.join("; ")
            )));
        }

        if planned.is_empty() {
            return Err(ToolError::Failed(
                "the patch contains no changes".to_owned(),
            ));
        }

        let mut written = Vec::new();

        for file in planned {
            let result = if file.create {
                write_new_atomically(&file.resolved, &file.contents, true)
            } else {
                write_atomically(&file.resolved, &file.contents, false)
            };

            result.map_err(|error| {
                ToolError::Failed(format!(
                    "failed while writing {}: {error}; earlier files in this patch may \
                         already have been updated",
                    file.resolved.display()
                ))
            })?;

            written.push((
                paths::display(&file.resolved, &ctx.project_root),
                file.contents.len(),
            ));
        }

        let summary = written
            .iter()
            .map(|(path, bytes)| format!("{path} ({bytes} bytes)"))
            .collect::<Vec<_>>()
            .join(", ");

        Ok(ToolOutput {
            content: format!("applied patch to {} file(s): {summary}", written.len()),
            truncated: false,
            metadata: Some(json!({
                "files": written
                    .iter()
                    .map(|(path, bytes)| json!({ "path": path, "bytes": bytes }))
                    .collect::<Vec<_>>(),
            })),
            images: Vec::new(),
        })
    }
}

fn plan_file(
    file: &diffy::patch_set::FilePatch<'_, str>,
    ctx: &ToolContext,
) -> Result<PlannedFile, String> {
    let (path, create) = match file.operation() {
        FileOperation::Create(path) => (strip_prefix(path).to_owned(), true),
        FileOperation::Modify { original, modified } => {
            let original = strip_prefix(original);
            let modified = strip_prefix(modified);

            if original != modified {
                return Err(format!(
                    "renaming {original} to {modified} is not supported; write the new file and \
                     remove the old one with shell commands"
                ));
            }

            (original.to_owned(), false)
        }
        FileOperation::Delete(path) => {
            return Err(format!(
                "deleting {} is not supported; use a shell command after reviewing the change",
                strip_prefix(path)
            ));
        }
        FileOperation::Rename { from, to } => {
            return Err(format!(
                "renaming {} to {} is not supported",
                strip_prefix(from),
                strip_prefix(to)
            ));
        }
        FileOperation::Copy { from, to } => {
            return Err(format!(
                "copying {} to {} is not supported",
                strip_prefix(from),
                strip_prefix(to)
            ));
        }
    };

    let patch = file
        .patch()
        .as_text()
        .ok_or_else(|| format!("binary patches are not supported ({path})"))?;
    let resolved = paths::resolve(&ctx.launch_dir, &path);

    crate::fsutil::reject_leaf_symlink(&resolved)
        .map_err(|error| format!("cannot apply the patch to {path}: {error}"))?;

    if create {
        match std::fs::symlink_metadata(&resolved) {
            Ok(_) => {
                return Err(format!(
                    "cannot create {path}: the destination already exists"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("cannot inspect {path}: {error}"));
            }
        }
    }

    let base = if create {
        String::new()
    } else {
        read_text(&resolved)
            .map_err(|error| format!("cannot apply the patch to {path}: {error}"))?
    };

    let contents = diffy::apply(&base, patch)
        .map_err(|error| format!("{path} does not match the patch hunks ({error})"))?;

    Ok(PlannedFile {
        path,
        resolved,
        contents,
        create,
    })
}

fn parse_options(patch: &str) -> ParseOptions {
    if patch.contains("diff --git") {
        ParseOptions::gitdiff()
    } else {
        ParseOptions::unidiff()
    }
}

/// Strip the conventional `a/` and `b/` prefixes used by git diffs.
fn strip_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}
