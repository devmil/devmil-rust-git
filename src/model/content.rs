use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::{
    diff::model::DiffPath,
    loading::{
        content::{ContentBytes, ContentError, ContentLoader, ContentRequest},
        working_diff::DiffEndpoint,
    },
};
impl Repository {
    /// Reads bounded file/blob content. Worktree refresh also invalidates filtered object reads
    /// because attributes in the worktree can change their representation.
    /// # Errors
    /// Returns lifecycle/queue errors; content failures arrive through the ticket.
    pub fn content(
        &self,
        endpoint: DiffEndpoint,
        path: DiffPath,
        byte_limit: usize,
    ) -> Result<ReadTicket<Option<ContentBytes>>, RepositoryError> {
        if byte_limit > crate::loading::content::MAX_CONTENT_BYTES {
            return Err(RepositoryError::new(
                RepositoryErrorKind::TooLarge,
                "content byte bound exceeds maximum",
            ));
        }
        let executable = self.shared.options.git_executable.clone();
        let immutable = matches!(endpoint, DiffEndpoint::EmptyTree);
        let key = format!("content:{endpoint:?}:{path:?}:{byte_limit}").into_bytes();
        self.schedule_weighted(
            key,
            immutable,
            |value: &Option<ContentBytes>| {
                value.as_ref().map_or(1, |value| {
                    value.bytes.capacity()
                        + match &value.origin {
                            crate::loading::content::ContentOrigin::GitObject => 64,
                            crate::loading::content::ContentOrigin::RepositoryFile(path) => {
                                path.allocated_bytes() + 64
                            }
                        }
                })
            },
            move |repository, executor, cancellation| {
                ContentLoader::new(executable)
                    .read(
                        &ContentRequest {
                            worktree: &repository.worktree,
                            endpoint: &endpoint,
                            path: &path,
                            byte_limit,
                        },
                        cancellation,
                        executor,
                    )
                    .map_err(|error| {
                        RepositoryError::new(
                            match error {
                                ContentError::Canceled => RepositoryErrorKind::Canceled,
                                ContentError::InvalidPath => RepositoryErrorKind::InvalidOptions,
                                ContentError::TooLarge => RepositoryErrorKind::TooLarge,
                                ContentError::Missing => RepositoryErrorKind::MissingObject,
                                ContentError::GitFailed => RepositoryErrorKind::Read,
                            },
                            error.to_string(),
                        )
                    })
            },
        )
    }

    /// Reads the exact stored bytes of blob `object` on the background lane:
    /// interactive reads queued later still run first. No filter, textconv
    /// or external command runs. Results are cached as immutable.
    /// # Errors
    /// Rejects a closed repository, a full queue or an excessive bound;
    /// read failures arrive through the ticket.
    pub fn raw_blob(
        &self,
        object: crate::types::ObjectId,
        byte_limit: usize,
    ) -> Result<ReadTicket<Vec<u8>>, RepositoryError> {
        if byte_limit > crate::loading::content::MAX_CONTENT_BYTES {
            return Err(RepositoryError::new(
                RepositoryErrorKind::TooLarge,
                "content byte bound exceeds maximum",
            ));
        }
        let executable = self.shared.options.git_executable.clone();
        let mut key = b"raw-blob:".to_vec();
        key.extend_from_slice(object.as_str().as_bytes());
        key.extend_from_slice(&byte_limit.to_le_bytes());
        self.schedule_background_weighted(
            key,
            true,
            |bytes: &Vec<u8>| bytes.capacity().saturating_add(64),
            move |repository, executor, cancellation| {
                ContentLoader::new(executable)
                    .read_raw_blob(
                        &repository.worktree,
                        &object,
                        byte_limit,
                        cancellation,
                        executor,
                    )
                    .map_err(content_error)
            },
        )
    }

    /// Reads the exact bytes of a regular worktree file on the background
    /// lane, without following symlinks or running filters. Not cached: the
    /// caller proves the bytes against a fingerprint it already holds.
    /// # Errors
    /// Rejects a closed repository, a full queue or an excessive bound;
    /// read failures arrive through the ticket.
    pub fn worktree_source(
        &self,
        path: DiffPath,
        byte_limit: usize,
    ) -> Result<ReadTicket<Vec<u8>>, RepositoryError> {
        if byte_limit > crate::loading::content::MAX_CONTENT_BYTES {
            return Err(RepositoryError::new(
                RepositoryErrorKind::TooLarge,
                "content byte bound exceeds maximum",
            ));
        }
        let mut key = b"worktree-source:".to_vec();
        key.extend_from_slice(path.as_bytes());
        key.push(0);
        key.extend_from_slice(&byte_limit.to_le_bytes());
        self.schedule_background_uncached(key, move |repository, _executor, cancellation| {
            ContentLoader::read_worktree_file(&repository.worktree, &path, byte_limit, cancellation)
                .map_err(content_error)
        })
    }
}

fn content_error(error: ContentError) -> RepositoryError {
    RepositoryError::new(
        match error {
            ContentError::Canceled => RepositoryErrorKind::Canceled,
            ContentError::InvalidPath => RepositoryErrorKind::InvalidOptions,
            ContentError::TooLarge => RepositoryErrorKind::TooLarge,
            ContentError::Missing => RepositoryErrorKind::MissingObject,
            ContentError::GitFailed => RepositoryErrorKind::Read,
        },
        error.to_string(),
    )
}
