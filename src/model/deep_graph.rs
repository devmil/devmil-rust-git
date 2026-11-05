use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::loading::deep_graph::{
    BisectState, DeepGraphError, DeepGraphErrorKind, DeepGraphLoader, DiffContentSearchOptions,
    DiffContentSearchRequest, DiffContentSearchResult,
};

impl Repository {
    /// Searches literal diff content across a bounded frozen commit enumeration.
    /// # Errors
    /// Rejects invalid requests or closed/capacity state. Read failures arrive through the ticket.
    pub fn search_diff_content(
        &self,
        query: String,
        maximum_commits: usize,
        maximum_results: usize,
    ) -> Result<ReadTicket<DiffContentSearchResult>, RepositoryError> {
        self.search_diff_content_with_options(
            query,
            maximum_commits,
            maximum_results,
            DiffContentSearchOptions::default(),
        )
    }

    /// Searches bounded diff content with explicit case and revision scope.
    /// # Errors
    /// Rejects invalid requests or closed/capacity state. Read failures arrive through the ticket.
    pub fn search_diff_content_with_options(
        &self,
        query: String,
        maximum_commits: usize,
        maximum_results: usize,
        options: DiffContentSearchOptions,
    ) -> Result<ReadTicket<DiffContentSearchResult>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        let key = format!("diff-content:{query:?}:{maximum_commits}:{maximum_results}:{options:?}")
            .into_bytes();
        self.schedule_background_weighted(
            key,
            true,
            |result: &DiffContentSearchResult| {
                result
                    .matches
                    .iter()
                    .map(|row| row.summary.capacity() + 256)
                    .sum::<usize>()
                    + result.activities.capacity()
                        * std::mem::size_of::<crate::activity::ActivityId>()
            },
            move |repository, executor, cancellation| {
                DeepGraphLoader::new(executable)
                    .search_with_options(
                        &DiffContentSearchRequest {
                            repository: repository.worktree.clone(),
                            query,
                            maximum_commits,
                            maximum_results,
                        },
                        options,
                        cancellation,
                        executor,
                    )
                    .map_err(|error| read_error(&error))
            },
        )
    }
    /// Discovers current bisect state; ref/operation refresh invalidates the cached result.
    /// # Errors
    /// Returns queue/lifecycle errors. Read failures arrive through the ticket.
    pub fn bisect_state(&self) -> Result<ReadTicket<BisectState>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            b"bisect-state".to_vec(),
            true,
            |state: &BisectState| {
                state.timeline.capacity() * 256
                    + state.next_step_summary.as_ref().map_or(0, String::capacity)
                    + 512
            },
            move |repository, executor, cancellation| {
                DeepGraphLoader::new(executable)
                    .discover(&repository.worktree, cancellation, executor)
                    .map_err(|error| read_error(&error))
            },
        )
    }
}
fn read_error(error: &DeepGraphError) -> RepositoryError {
    let kind = match error.kind() {
        DeepGraphErrorKind::InvalidRequest => RepositoryErrorKind::InvalidOptions,
        DeepGraphErrorKind::Launch => RepositoryErrorKind::MissingGit,
        DeepGraphErrorKind::MalformedOutput => RepositoryErrorKind::InvalidOutput,
        DeepGraphErrorKind::ResourceLimit => RepositoryErrorKind::TooLarge,
        DeepGraphErrorKind::Canceled => RepositoryErrorKind::Canceled,
        DeepGraphErrorKind::Git | DeepGraphErrorKind::NotBisecting => RepositoryErrorKind::Read,
    };
    RepositoryError::new(kind, error.detail())
}
