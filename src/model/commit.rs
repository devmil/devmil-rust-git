//! Cached commit reads scheduled on the repository worker.
use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::diff::model::{
    ByteSpan, DiffDocument, DiffFile, DiffFileId, DiffHunk, DiffLayout, DiffLine,
    DiffLineAnnotation, DiffPath, DiffWhitespacePolicy, SyntaxAvailability,
};
use crate::loading::commit::{
    CommitDetail, CommitDiffBase, CommitDiffOptions, CommitDiffScope, CommitFileChange,
    CommitLoader, CommitReadRequest, HistoryReadFailure, HistoryReadFailureKind, HistoryReadKind,
    HistoryReadValue, MAX_COMMIT_DETAIL_BATCH,
};
use crate::types::ObjectId;
use std::mem::size_of;

impl Repository {
    /// Loads the complete byte-preserving commit message and identities.
    /// # Errors
    /// Rejects a closed repository or a full read queue; execution failures arrive on the ticket.
    pub fn commit_detail(
        &self,
        commit: ObjectId,
    ) -> Result<ReadTicket<CommitDetail>, RepositoryError> {
        let mut key = b"commit-detail-v1".to_vec();
        field(&mut key, commit.as_str().as_bytes());
        let loader = CommitLoader::new(self.shared.options.git_executable.clone());
        self.schedule_weighted(
            key,
            true,
            detail_bytes,
            move |repository, executor, cancellation| {
                let request = CommitReadRequest::new(
                    repository.worktree.clone(),
                    commit,
                    CommitDiffBase::EmptyTree,
                    HistoryReadKind::Detail,
                );
                match loader
                    .load(&request, executor, Some(cancellation))
                    .map_err(read_error)?
                {
                    HistoryReadValue::Detail(value) => Ok(value),
                    _ => Err(unexpected()),
                }
            },
        )
    }

    /// Loads an ordered bounded batch with one Git invocation, retaining duplicates.
    /// # Errors
    /// Rejects a closed repository or a full read queue; execution failures arrive on the ticket.
    pub fn commit_details(
        &self,
        commits: Vec<ObjectId>,
    ) -> Result<ReadTicket<Vec<CommitDetail>>, RepositoryError> {
        if commits.len() > MAX_COMMIT_DETAIL_BATCH {
            return Err(RepositoryError::new(
                RepositoryErrorKind::Read,
                "commit detail batch exceeds its item limit",
            ));
        }
        let mut key = b"commit-details-v1".to_vec();
        for commit in &commits {
            field(&mut key, commit.as_str().as_bytes());
        }
        let loader = CommitLoader::new(self.shared.options.git_executable.clone());
        self.schedule_weighted(
            key,
            true,
            |values: &Vec<CommitDetail>| {
                size_of::<Vec<CommitDetail>>()
                    + values.capacity() * size_of::<CommitDetail>()
                    + values
                        .iter()
                        .map(|value| detail_bytes(value) - size_of::<CommitDetail>())
                        .sum::<usize>()
            },
            move |repository, executor, cancellation| {
                loader
                    .load_details(&repository.worktree, &commits, executor, Some(cancellation))
                    .map_err(read_error)
            },
        )
    }

    /// Loads changed paths and modes without requesting patch bodies.
    /// # Errors
    /// Rejects a closed repository or a full read queue; execution failures arrive on the ticket.
    pub fn commit_files(
        &self,
        commit: ObjectId,
        base: CommitDiffBase,
    ) -> Result<ReadTicket<Vec<CommitFileChange>>, RepositoryError> {
        let key = comparison_key(b"commit-files-v1", &commit, &base);
        let loader = CommitLoader::new(self.shared.options.git_executable.clone());
        self.schedule_weighted(
            key,
            true,
            |files: &Vec<CommitFileChange>| {
                size_of::<Vec<CommitFileChange>>()
                    + files.capacity() * size_of::<CommitFileChange>()
                    + files
                        .iter()
                        .map(|file| {
                            file.old_path.allocated_bytes() + file.new_path.allocated_bytes() + 128
                        })
                        .sum::<usize>()
            },
            move |repository, executor, cancellation| {
                let request = CommitReadRequest::new(
                    repository.worktree.clone(),
                    commit,
                    base,
                    HistoryReadKind::Files,
                );
                match loader
                    .load(&request, executor, Some(cancellation))
                    .map_err(read_error)?
                {
                    HistoryReadValue::Files(value) => Ok(value),
                    _ => Err(unexpected()),
                }
            },
        )
    }

    /// Loads a bounded patch with an exact byte-path and display-options cache identity.
    /// # Errors
    /// Rejects a closed repository or a full read queue; execution failures arrive on the ticket.
    pub fn commit_diff(
        &self,
        commit: ObjectId,
        base: CommitDiffBase,
        options: CommitDiffOptions,
    ) -> Result<ReadTicket<DiffDocument>, RepositoryError> {
        let mut key = comparison_key(b"commit-diff-v1", &commit, &base);
        match &options.scope {
            CommitDiffScope::All => key.push(0),
            CommitDiffScope::File { old_path, new_path } => {
                key.push(1);
                field(&mut key, old_path.as_bytes());
                field(&mut key, new_path.as_bytes());
            }
        }
        key.extend_from_slice(&options.context.value().to_le_bytes());
        key.push(match options.whitespace {
            DiffWhitespacePolicy::Preserve => 0,
            DiffWhitespacePolicy::IgnoreAll => 1,
            DiffWhitespacePolicy::IgnoreAmount => 2,
            DiffWhitespacePolicy::IgnoreEndOfLine => 3,
        });
        key.push(match options.layout {
            DiffLayout::Unified => 0,
            DiffLayout::SideBySide => 1,
            DiffLayout::ImageSideBySide => 2,
        });
        key.push(u8::from(options.force_text));
        let loader = CommitLoader::new(self.shared.options.git_executable.clone());
        self.schedule_weighted(
            key,
            true,
            diff_bytes,
            move |repository, executor, cancellation| {
                let mut request = CommitReadRequest::new(
                    repository.worktree.clone(),
                    commit,
                    base,
                    HistoryReadKind::Diff,
                );
                request.diff_scope = options.scope;
                request.context = options.context;
                request.whitespace = options.whitespace;
                request.layout = options.layout;
                request.force_text = options.force_text;
                match loader
                    .load(&request, executor, Some(cancellation))
                    .map_err(read_error)?
                {
                    HistoryReadValue::Diff(mut value) => {
                        value.layout = options.layout;
                        Ok(value)
                    }
                    _ => Err(unexpected()),
                }
            },
        )
    }
}

fn field(key: &mut Vec<u8>, bytes: &[u8]) {
    key.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    key.extend_from_slice(bytes);
}
fn comparison_key(namespace: &[u8], commit: &ObjectId, base: &CommitDiffBase) -> Vec<u8> {
    let mut key = namespace.to_vec();
    field(&mut key, commit.as_str().as_bytes());
    match base {
        CommitDiffBase::EmptyTree => key.push(0),
        CommitDiffBase::FirstParent(parent) => {
            key.push(1);
            field(&mut key, parent.as_str().as_bytes());
        }
    }
    key
}
#[allow(clippy::needless_pass_by_value)]
fn read_error(error: HistoryReadFailure) -> RepositoryError {
    let kind = match error.kind {
        HistoryReadFailureKind::Canceled => RepositoryErrorKind::Canceled,
        HistoryReadFailureKind::MissingObject => RepositoryErrorKind::MissingObject,
        HistoryReadFailureKind::TooLarge => RepositoryErrorKind::TooLarge,
        HistoryReadFailureKind::MalformedOutput => RepositoryErrorKind::InvalidOutput,
        HistoryReadFailureKind::InvalidRequest => RepositoryErrorKind::InvalidOptions,
        HistoryReadFailureKind::ProcessFailed | HistoryReadFailureKind::Internal => {
            RepositoryErrorKind::Read
        }
    };
    RepositoryError::new(kind, error.detail()).with_activity(error.activity_id())
}
fn unexpected() -> RepositoryError {
    RepositoryError::new(
        RepositoryErrorKind::Read,
        "commit loader returned an unexpected value",
    )
}
fn detail_bytes(value: &CommitDetail) -> usize {
    size_of::<CommitDetail>()
        + 128
        + value.parents.capacity() * size_of::<ObjectId>()
        + value.parents.len() * 64
        + value.author.capacity()
        + value.author_timezone.capacity()
        + value.committer.capacity()
        + value.committer_timezone.capacity()
        + value.message.capacity()
}
fn paths_bytes(value: &DiffFileId) -> usize {
    value.old_path.as_ref().map_or(0, DiffPath::allocated_bytes)
        + value.new_path.as_ref().map_or(0, DiffPath::allocated_bytes)
}
pub(super) fn diff_bytes(value: &DiffDocument) -> usize {
    let mut bytes = size_of::<DiffDocument>() + value.files.capacity() * size_of::<DiffFile>();
    for file in &value.files {
        bytes += paths_bytes(&file.id)
            + file.patch_header.capacity()
            + file.hunks.capacity() * size_of::<DiffHunk>();
        if let SyntaxAvailability::Available(syntax) = &file.syntax {
            bytes += syntax.capacity();
        }
        for hunk in &file.hunks {
            bytes += paths_bytes(&hunk.id.file)
                + hunk.section.capacity()
                + hunk.lines.capacity() * size_of::<DiffLine>();
            for line in &hunk.lines {
                bytes += paths_bytes(&line.id.hunk.file)
                    + line.source.capacity()
                    + line.intra_line.capacity() * size_of::<ByteSpan>();
                match &line.annotation {
                    DiffLineAnnotation::ConflictBegin { label }
                    | DiffLineAnnotation::ConflictEnd { label } => bytes += label.capacity(),
                    _ => {}
                }
            }
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_path_keys_distinguish_boundaries_and_non_utf8() {
        let mut left = Vec::new();
        field(&mut left, b"a");
        field(&mut left, b"bc\xff");
        let mut right = Vec::new();
        field(&mut right, b"ab");
        field(&mut right, b"c\xff");
        assert_ne!(left, right);
    }

    #[test]
    fn patch_charge_includes_duplicate_paths_in_every_line_identity() {
        let patch = b"diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n";
        let document =
            crate::diff::parse_unified_patch(patch, crate::diff::PatchLimits::default()).unwrap();
        let mut larger = document.clone();
        let baseline = diff_bytes(&larger);
        let long_name = DiffPath::from_bytes(vec![b'x'; 20_000]).unwrap();
        for line in &mut larger.files[0].hunks[0].lines {
            line.id.hunk.file.old_path = Some(long_name.clone());
            line.id.hunk.file.new_path = Some(long_name.clone());
        }
        assert!(diff_bytes(&larger) >= baseline + 79_000);
    }
}
