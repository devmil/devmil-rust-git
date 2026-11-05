use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::loading::conflict_prediction::{
    ConflictPrediction, ConflictPredictionErrorKind, ConflictPredictionKind,
    ConflictPredictionLoader, ConflictPredictionRequest, ConflictPredictionTarget,
};
impl Repository {
    /// Predicts merge/rebase conflicts in isolated object storage. Ref changes invalidate the result.
    /// # Errors
    /// Returns lifecycle/queue errors; simulation failures arrive through the ticket.
    pub fn predict_conflicts(
        &self,
        kind: ConflictPredictionKind,
        target: ConflictPredictionTarget,
    ) -> Result<ReadTicket<ConflictPrediction>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        let key = format!("conflict-prediction:{kind:?}:{target:?}").into_bytes();
        self.schedule_weighted(
            key,
            true,
            |result: &ConflictPrediction| {
                result
                    .conflicting_paths
                    .iter()
                    .map(|path| path.as_path().as_os_str().as_encoded_bytes().len() + 64)
                    .sum::<usize>()
                    + result.activities.capacity()
                        * std::mem::size_of::<crate::activity::ActivityId>()
                    + 2048
            },
            move |repository, executor, cancellation| {
                ConflictPredictionLoader::new(executable)
                    .predict(
                        &ConflictPredictionRequest {
                            repository: repository.worktree.clone(),
                            kind,
                            target,
                        },
                        cancellation,
                        executor,
                    )
                    .map_err(|error| {
                        let kind = match error.kind() {
                            ConflictPredictionErrorKind::Canceled => RepositoryErrorKind::Canceled,
                            ConflictPredictionErrorKind::ResourceLimit => {
                                RepositoryErrorKind::TooLarge
                            }
                            ConflictPredictionErrorKind::MalformedOutput => {
                                RepositoryErrorKind::InvalidOutput
                            }
                            ConflictPredictionErrorKind::InvalidTarget => {
                                RepositoryErrorKind::InvalidOptions
                            }
                            ConflictPredictionErrorKind::Unsupported
                            | ConflictPredictionErrorKind::Git => RepositoryErrorKind::Read,
                        };
                        RepositoryError::new(kind, error.detail())
                    })
            },
        )
    }
}
