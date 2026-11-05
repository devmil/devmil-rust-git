use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::loading::graph::{
    GraphHistoryOrder, GraphLoader, GraphRequest, GraphTraversalScope, LoadedMetadata,
    LoadedTopology,
};
use crate::types::ObjectId;

impl Repository {
    /// Loads a frozen history snapshot. Identical requests share one cached traversal.
    /// # Errors
    /// Returns queue or lifecycle errors. Git failures arrive through the ticket.
    pub fn history(
        &self,
        order: GraphHistoryOrder,
        scope: GraphTraversalScope,
    ) -> Result<ReadTicket<LoadedTopology>, RepositoryError> {
        self.history_with_limit(order, scope, None)
    }

    /// Loads at most `limit` topology rows, retaining frozen-root and boundary consistency.
    /// # Errors
    /// Rejects zero limits and returns queue or lifecycle errors.
    pub fn history_prefix(
        &self,
        order: GraphHistoryOrder,
        scope: GraphTraversalScope,
        limit: usize,
    ) -> Result<ReadTicket<LoadedTopology>, RepositoryError> {
        if limit == 0 {
            return Err(RepositoryError::new(
                RepositoryErrorKind::InvalidOptions,
                "history prefix limit must be nonzero",
            ));
        }
        self.history_with_limit(order, scope, Some(limit))
    }

    fn history_with_limit(
        &self,
        order: GraphHistoryOrder,
        scope: GraphTraversalScope,
        limit: Option<usize>,
    ) -> Result<ReadTicket<LoadedTopology>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        let mut key = vec![b'g', b'r', b'a', b'p', b'h', order as u8, scope as u8];
        key.extend_from_slice(&(limit.unwrap_or(0) as u64).to_le_bytes());
        self.schedule_weighted(
            key,
            true,
            topology_bytes,
            move |repository, executor, cancellation| {
                let mut loader =
                    GraphLoader::new(executable).with_known_version(repository.git_version);
                if let Some(limit) = limit {
                    loader = loader.with_topology_limit(limit);
                }
                let mut activity = None;
                loader
                    .load(
                        &GraphRequest {
                            worktree: repository.worktree.clone(),
                            order,
                            scope,
                        },
                        cancellation,
                        &mut |command| {
                            let result = executor.execute(command);
                            activity = Some(result.activity.id);
                            result
                        },
                    )
                    .map_err(|error| graph_error(&error).with_activity(activity))
            },
        )
    }

    /// Batches metadata queries for the requested objects, with bounded object preflight.
    /// # Errors
    /// Returns queue or lifecycle errors. Git failures arrive through the ticket.
    pub fn commit_metadata(
        &self,
        objects: Vec<ObjectId>,
    ) -> Result<ReadTicket<LoadedMetadata>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        let mut key = b"metadata\0".to_vec();
        for object in &objects {
            let value = object.as_str().as_bytes();
            key.extend_from_slice(&(value.len() as u64).to_le_bytes());
            key.extend_from_slice(value);
        }
        self.schedule_weighted(
            key,
            true,
            |value: &LoadedMetadata| {
                value
                    .records
                    .iter()
                    .map(|record| {
                        std::mem::size_of_val(record)
                            + record.metadata.subject.capacity()
                            + record.metadata.author.capacity()
                            + record.metadata.avatar_hash.capacity()
                            + 256
                    })
                    .sum::<usize>()
                    .saturating_add(value.missing.len().saturating_mul(256))
            },
            move |repository, executor, cancellation| {
                let mut activity = None;
                GraphLoader::new(executable)
                    .with_known_version(repository.git_version)
                    .metadata(
                        &repository.worktree,
                        &objects,
                        cancellation,
                        &mut |command| {
                            let result = executor.execute(command);
                            activity = Some(result.activity.id);
                            result
                        },
                    )
                    .map_err(|error| graph_error(&error).with_activity(activity))
            },
        )
    }
}

pub(super) fn graph_error(error: &crate::loading::graph::GraphLoadError) -> RepositoryError {
    use crate::loading::graph::{GraphLoadError, GraphQueryFailureKind};
    let kind = match &error {
        GraphLoadError::Canceled => RepositoryErrorKind::Canceled,
        GraphLoadError::Failed(failure) => match failure.kind() {
            GraphQueryFailureKind::MissingGit => RepositoryErrorKind::MissingGit,
            GraphQueryFailureKind::RepositoryUnavailable => RepositoryErrorKind::Discovery,
            GraphQueryFailureKind::UnsupportedGit => RepositoryErrorKind::Unsupported,
            GraphQueryFailureKind::MalformedOutput => RepositoryErrorKind::InvalidOutput,
            GraphQueryFailureKind::OutputTooLarge => RepositoryErrorKind::TooLarge,
            GraphQueryFailureKind::InvalidRequest | GraphQueryFailureKind::InvalidPageToken => {
                RepositoryErrorKind::InvalidOptions
            }
            GraphQueryFailureKind::ProcessFailed | GraphQueryFailureKind::Internal => {
                RepositoryErrorKind::Read
            }
        },
    };
    RepositoryError::new(kind, error.to_string())
}

pub(super) fn topology_bytes(value: &LoadedTopology) -> usize {
    value
        .rows
        .iter()
        .map(|row| {
            std::mem::size_of_val(row)
                + 256
                + row.parents.capacity().saturating_mul(256)
                + row
                    .refs
                    .iter()
                    .map(|reference| 64 + reference.as_bytes().len())
                    .sum::<usize>()
                + row.metadata.as_ref().map_or(0, |metadata| {
                    metadata.subject.capacity()
                        + metadata.author.capacity()
                        + metadata.avatar_hash.capacity()
                })
        })
        .sum::<usize>()
        .saturating_add(value.conditions.capacity().saturating_mul(512))
        .saturating_add(value.frozen_tips.capacity().saturating_mul(256))
}
