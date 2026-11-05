//! Cached commit evidence and upstream branch topology.
use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::loading::autosquash::{
    AutosquashError, AutosquashErrorKind, AutosquashLoader, CommitEvidence, EvidenceQuery,
    StackDiscovery,
};
fn failure(error: AutosquashError) -> RepositoryError {
    let kind = match error.kind {
        AutosquashErrorKind::Canceled => RepositoryErrorKind::Canceled,
        AutosquashErrorKind::TimedOut => RepositoryErrorKind::TimedOut,
        AutosquashErrorKind::ResourceLimit => RepositoryErrorKind::TooLarge,
        AutosquashErrorKind::MalformedOutput => RepositoryErrorKind::InvalidOutput,
        AutosquashErrorKind::InvalidRequest => RepositoryErrorKind::InvalidOptions,
        _ => RepositoryErrorKind::Read,
    };
    RepositoryError::new(kind, error.detail)
}
impl Repository {
    /// Loads the last commit changing a path or the exact selected commit.
    /// # Errors
    /// Reports repository lifecycle, scheduling, cancellation or Git failures.
    pub fn commit_evidence(
        &self,
        query: EvidenceQuery,
    ) -> Result<ReadTicket<CommitEvidence>, RepositoryError> {
        let mut key = b"commit-evidence-v1".to_vec();
        for bytes in [
            query
                .revision
                .as_ref()
                .map_or(&[][..], |oid| oid.as_str().as_bytes()),
            query.path.as_ref().map_or(&[][..], |path| {
                path.as_path().as_os_str().as_encoded_bytes()
            }),
        ] {
            key.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            key.extend_from_slice(bytes);
        }
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            key,
            true, // HEAD/ref changes trigger a full refresh; worktree edits do not affect history.
            |result: &CommitEvidence| {
                std::mem::size_of_val(result)
                    + result.subject.capacity()
                    + 128
                    + super::pages::activity(&result.activity)
            },
            move |repository, executor, cancellation| {
                AutosquashLoader::new(executable, executor)
                    .evidence(&repository.worktree, &query, cancellation)
                    .map_err(failure)
            },
        )
    }
    /// Loads bounded local upstream topology and fork points until the next refresh.
    /// # Errors
    /// Reports repository lifecycle, scheduling, cancellation or Git failures.
    pub fn branch_stack(&self) -> Result<ReadTicket<StackDiscovery>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            b"branch-stack-v1".to_vec(),
            true, // Upstream/ref changes invalidate this cache through a full refresh.
            |result: &StackDiscovery| {
                std::mem::size_of_val(result)
                    + result.branches.capacity()
                        * std::mem::size_of::<crate::loading::autosquash::StackedBranch>()
                    + super::pages::activities(&result.activities)
                    + result
                        .branches
                        .iter()
                        .map(|branch| {
                            256 + branch.name.as_bytes().len() * 2
                                + branch
                                    .upstream
                                    .as_ref()
                                    .map_or(0, |reference| reference.as_bytes().len() * 2)
                        })
                        .sum::<usize>()
            },
            move |repository, executor, cancellation| {
                AutosquashLoader::new(executable, executor)
                    .discover(&repository.worktree, cancellation)
                    .map_err(failure)
            },
        )
    }
}
