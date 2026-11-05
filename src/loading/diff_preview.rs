//! Metadata-first size checks shared by working, committed and comparison diffs.
use crate::activity::ActivityStatus;
use crate::cancellation::Cancellation;
use crate::diff::model::{
    DiffDocument, DiffFallback, DiffFile, DiffFileId, DiffPath, FileStagingState, GitFileMode,
    SyntaxAvailability,
};
use crate::diff::{parse_raw_diff_z, DiffError, DiffErrorKind, RawDiffEntry, RawObjectId};
use crate::process::{GitProcessCommand, PrivateActivityResult, ProcessArgument};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Maximum source bytes per side before a diff becomes metadata only.
pub const MAX_INLINE_FILE_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct PreviewPlan {
    entries: Vec<RawDiffEntry>,
    sizes: Vec<u64>,
}

impl PreviewPlan {
    pub(crate) fn inspect(
        raw: &[u8],
        root: &Path,
        executable: &OsStr,
        working_side: Option<bool>,
        cancellation: Option<&dyn Cancellation>,
        execute: &mut dyn FnMut(GitProcessCommand) -> PrivateActivityResult,
    ) -> Result<Self, DiffError> {
        let entries = parse_raw_diff_z(raw)?;
        let mut objects = Vec::new();
        let mut seen = HashSet::new();
        for entry in &entries {
            for (is_new, mode, object) in [
                (false, entry.old_mode, &entry.old_object),
                (true, entry.new_mode, &entry.new_object),
            ] {
                if working_side == Some(is_new) || mode == GitFileMode::GITLINK {
                    continue;
                }
                if let RawObjectId::Object(object) = object {
                    if seen.insert(object.as_str().to_owned()) {
                        objects.push(object.as_str().to_owned());
                    }
                }
            }
        }
        let mut sizes = HashMap::new();
        if !objects.is_empty() {
            if cancellation.is_some_and(Cancellation::is_cancelled) {
                return Err(failure(DiffErrorKind::Canceled));
            }
            let input = objects.join("\n") + "\n";
            let command = GitProcessCommand::git(executable.to_os_string())
                .current_dir(root)
                .args([
                    ProcessArgument::visible("cat-file"),
                    ProcessArgument::visible("--batch-check=%(objectsize)"),
                ])
                .stdin_bytes(input.into_bytes())
                .output_limit(objects.len().saturating_mul(128))
                .redact_output();
            let mut result = execute(command);
            if cancellation.is_some_and(Cancellation::is_cancelled)
                || matches!(result.activity.status, ActivityStatus::Canceled { .. })
            {
                return Err(failure(DiffErrorKind::Canceled));
            }
            if !matches!(result.activity.status, ActivityStatus::Succeeded { .. }) {
                return Err(failure(DiffErrorKind::GitFailed));
            }
            if result.stdout.truncated {
                return Err(failure(DiffErrorKind::TooLarge));
            }
            let output = result.stdout.take_bytes();
            let values = output
                .strip_suffix(b"\n")
                .ok_or_else(|| failure(DiffErrorKind::MalformedMetadata))?
                .split(|byte| *byte == b'\n')
                .map(|bytes| std::str::from_utf8(bytes).ok()?.parse::<u64>().ok())
                .collect::<Option<Vec<_>>>()
                .filter(|values| values.len() == objects.len())
                .ok_or_else(|| failure(DiffErrorKind::MalformedMetadata))?;
            sizes.extend(objects.into_iter().zip(values));
        }
        let sizes = entries
            .iter()
            .map(|entry| {
                [
                    (false, entry.old_mode, &entry.old_object, &entry.old_path),
                    (true, entry.new_mode, &entry.new_object, &entry.new_path),
                ]
                .into_iter()
                .map(|(is_new, mode, object, path)| {
                    if mode == GitFileMode::GITLINK {
                        return 0;
                    }
                    if working_side == Some(is_new) {
                        return worktree_size(root, path).unwrap_or(0);
                    }
                    match object {
                        RawObjectId::Object(object) => sizes[object.as_str()],
                        RawObjectId::Null(_) => 0,
                    }
                })
                .max()
                .unwrap_or(0)
            })
            .collect();
        Ok(Self { entries, sizes })
    }

    pub(crate) fn has_large_files(&self) -> bool {
        self.sizes.iter().any(|size| *size > MAX_INLINE_FILE_BYTES)
    }

    pub(crate) fn inline_raw(&self, raw: &[u8]) -> Vec<u8> {
        let mut fields = raw.split_inclusive(|byte| *byte == 0);
        let mut output = Vec::new();
        for (entry, size) in self.entries.iter().zip(&self.sizes) {
            let count = if matches!(
                entry.kind,
                crate::diff::model::FileChangeKind::Renamed { .. }
                    | crate::diff::model::FileChangeKind::Copied { .. }
            ) {
                3
            } else {
                2
            };
            for field in fields.by_ref().take(count) {
                if *size <= MAX_INLINE_FILE_BYTES {
                    output.extend_from_slice(field);
                }
            }
        }
        output
    }

    pub(crate) fn inline_paths(&self) -> Vec<&DiffPath> {
        let mut seen = HashSet::new();
        self.entries
            .iter()
            .zip(&self.sizes)
            .filter(|(_, size)| **size <= MAX_INLINE_FILE_BYTES)
            .flat_map(|(entry, _)| [&entry.old_path, &entry.new_path])
            .filter(|path| seen.insert(path.as_bytes()))
            .collect()
    }

    pub(crate) fn restrict(&self, arguments: &mut Vec<OsString>) -> Result<(), DiffError> {
        if let Some(boundary) = arguments.iter().position(|argument| argument == "--") {
            arguments.truncate(boundary);
        }
        if arguments
            .first()
            .is_none_or(|value| value != "--literal-pathspecs")
        {
            arguments.insert(0, "--literal-pathspecs".into());
        }
        arguments.push("--".into());
        for path in self.inline_paths() {
            arguments.push(path_argument(path)?);
        }
        Ok(())
    }

    pub(crate) fn merge(&self, mut document: DiffDocument) -> DiffDocument {
        let mut files = Vec::new();
        for (entry, size) in self.entries.iter().zip(&self.sizes) {
            if *size > MAX_INLINE_FILE_BYTES {
                files.push(DiffFile {
                    id: DiffFileId {
                        old_path: (entry.old_mode.value() != 0).then(|| entry.old_path.clone()),
                        new_path: (entry.new_mode.value() != 0).then(|| entry.new_path.clone()),
                    },
                    kind: entry.kind.clone(),
                    old_mode: (entry.old_mode.value() != 0).then_some(entry.old_mode),
                    new_mode: (entry.new_mode.value() != 0).then_some(entry.new_mode),
                    syntax: SyntaxAvailability::Unavailable,
                    staging: FileStagingState::NotApplicable,
                    hunks: Vec::new(),
                    fallback: Some(DiffFallback::TooLarge {
                        byte_count: *size,
                        line_count: 0,
                        preview_line_limit: 0,
                    }),
                    patch_header: Vec::new(),
                });
            } else if let Some(index) = document.files.iter().position(|file| {
                file.id.new_path.as_ref() == Some(&entry.new_path)
                    || file.id.old_path.as_ref() == Some(&entry.old_path)
            }) {
                let mut file = document.files.remove(index);
                if let Some(DiffFallback::Binary { byte_count }) = &mut file.fallback {
                    *byte_count = Some(*size);
                }
                files.push(file);
            }
        }
        document.files = files;
        document
    }
}

pub(crate) fn worktree_size(root: &Path, path: &DiffPath) -> Option<u64> {
    let root = std::fs::canonicalize(root).ok()?;
    let path = root.join(path_argument(path).ok()?);
    let parent = std::fs::canonicalize(path.parent()?).ok()?;
    if !parent.starts_with(&root) {
        return None;
    }
    let metadata = std::fs::symlink_metadata(path).ok()?;
    metadata.is_file().then_some(metadata.len())
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // The Windows boundary can reject non-native paths.
pub(crate) fn path_argument(path: &DiffPath) -> Result<OsString, DiffError> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(OsString::from_vec(path.as_bytes().to_vec()))
}

#[cfg(not(unix))]
pub(crate) fn path_argument(path: &DiffPath) -> Result<OsString, DiffError> {
    String::from_utf8(path.as_bytes().to_vec())
        .map(OsString::from)
        .map_err(|_| failure(DiffErrorKind::InvalidSelection))
}

fn failure(kind: DiffErrorKind) -> DiffError {
    DiffError::new(kind, "diff size metadata could not be read")
}
