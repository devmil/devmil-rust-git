//! Typed repository reads executed through the repository model's recorded command transport.

use crate::process::{GitProcessCommand, PrivateActivityResult};

pub mod autosquash;
pub mod blame;
pub mod commit;
pub mod commit_tools;
pub mod comparison;
pub mod conflict_prediction;
pub mod content;
pub mod deep_graph;
pub mod diff_preview;
pub mod extensions;
pub mod graph;
pub mod graph_search;
pub mod history_index;
pub mod path_history;
pub mod reflog;
pub mod refs;
pub mod statistics;
pub mod status;
pub mod tree;
pub mod working_diff;

/// Executes a command while retaining the caller's activity and cancellation context.
/// Private output must never be forwarded to public events without redaction.
pub trait CommandExecutor {
    fn execute(&mut self, command: GitProcessCommand) -> PrivateActivityResult;

    /// Streams private output where supported; simple executors deliver the buffered bytes.
    fn execute_observing(
        &mut self,
        command: GitProcessCommand,
        observer: crate::process::PrivateStdoutObserver<'_>,
        _cancellation: Option<&dyn crate::cancellation::Cancellation>,
    ) -> PrivateActivityResult {
        let result = self.execute(command);
        if let Some(observer) = observer {
            observer(&result.stdout.bytes);
        }
        result
    }
}

impl<F: FnMut(GitProcessCommand) -> PrivateActivityResult> CommandExecutor for F {
    fn execute(&mut self, command: GitProcessCommand) -> PrivateActivityResult {
        self(command)
    }
}
