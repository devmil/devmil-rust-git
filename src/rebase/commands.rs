//! Git rebase argument vectors and deterministic editor environment.
use crate::activity::{ActivityArgument, ActivityCommand, ActivityEnvironment};
use crate::types::WorktreeId;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequencerAction {
    Continue,
    Skip,
    Abort,
}

/// Consumer-selected environment protocol understood by its sequence-editor executable.
#[derive(Clone, Copy, Debug)]
pub struct SequenceEditorProtocol<'a> {
    pub mode_variable: &'a str,
    pub mode_value: &'a str,
    pub plan_variable: &'a str,
}

/// The exact argument vector (without the executable) for `git rebase <onto>`. Auto-stash is
/// pinned off so behavior matches the dirty-tree preflight regardless of user configuration.
/// Planner and adapter both call this, so preview and executed argv are identical by
/// construction.
#[must_use]
pub fn rebase_onto_argv(onto: &str) -> Vec<String> {
    vec![
        "-c".to_owned(),
        "rebase.autoStash=false".to_owned(),
        "rebase".to_owned(),
        onto.to_owned(),
    ]
}

/// The exact argument vector for `git rebase --onto <new_base> <upstream> <branch>`: replays the
/// commits of `branch` that are not in `upstream` onto `new_base` (for example to restack a
/// branch after its parent was rewritten). Auto-stash is pinned off as in
/// [`rebase_onto_argv`]; run it with [`rebase_environment`].
///
/// # Errors
/// Rejects empty, option-like or control-character values.
pub fn rebase_onto_upstream_argv(
    new_base: &str,
    upstream: &str,
    branch: &str,
) -> Result<Vec<String>, crate::commands::CommandValidationError> {
    for (value, what) in [
        (new_base, "rebase base"),
        (upstream, "rebase upstream"),
        (branch, "rebased branch"),
    ] {
        crate::commands::validate_label(value, usize::MAX, what)?;
    }
    Ok(vec![
        "-c".to_owned(),
        "rebase.autoStash=false".to_owned(),
        "rebase".to_owned(),
        "--onto".to_owned(),
        new_base.to_owned(),
        upstream.to_owned(),
        branch.to_owned(),
    ])
}

/// The exact argument vector for a prepared interactive rebase. The caller must provide a
/// deterministic sequence-editor environment with [`interactive_rebase_environment`].
#[must_use]
pub fn interactive_rebase_onto_argv(onto: &str) -> Vec<String> {
    vec![
        "-c".to_owned(),
        "rebase.autoStash=false".to_owned(),
        "rebase".to_owned(),
        "--interactive".to_owned(),
        onto.to_owned(),
    ]
}

/// The exact argument vector for `git rebase --continue`, `--skip`, or `--abort`.
#[must_use]
pub fn rebase_sequencer_argv(action: SequencerAction) -> Vec<String> {
    let flag = match action {
        SequencerAction::Continue => "--continue",
        SequencerAction::Skip => "--skip",
        SequencerAction::Abort => "--abort",
    };
    vec!["rebase".to_owned(), flag.to_owned()]
}

/// The deterministic environment for rebase execution: editors are pinned to no-ops so the
/// sequencer can never block on interactive input, and the locale is pinned so progress and
/// error output parse.
#[must_use]
pub fn rebase_environment() -> Vec<(String, String)> {
    vec![
        ("GIT_EDITOR".to_owned(), "true".to_owned()),
        ("GIT_SEQUENCE_EDITOR".to_owned(), "true".to_owned()),
        ("LC_ALL".to_owned(), "C".to_owned()),
    ]
}

/// Builds the deterministic environment for a prepared interactive rebase. The sequence editor
/// receives the immutable plan path through caller-selected protocol environment variables.
#[must_use]
pub fn interactive_rebase_environment(
    helper: &Path,
    prepared_plan: &Path,
    protocol: SequenceEditorProtocol<'_>,
) -> Vec<(String, String)> {
    vec![
        (
            "GIT_EDITOR".to_owned(),
            helper.to_string_lossy().into_owned(),
        ),
        (
            "GIT_SEQUENCE_EDITOR".to_owned(),
            helper.to_string_lossy().into_owned(),
        ),
        (
            protocol.mode_variable.to_owned(),
            protocol.mode_value.to_owned(),
        ),
        (
            protocol.plan_variable.to_owned(),
            prepared_plan.to_string_lossy().into_owned(),
        ),
        ("LC_ALL".to_owned(), "C".to_owned()),
    ]
}

/// Builds the activity command both the preflight preview and the executed process use. The
/// executed side (`GitProcessCommand::activity_command`) must produce an equal value; the
/// real-Git tests assert that equality.
#[must_use]
pub fn rebase_activity_command(
    executable: &str,
    worktree: &WorktreeId,
    argv: &[String],
    environment: &[(String, String)],
) -> ActivityCommand {
    ActivityCommand {
        executable: executable.to_owned(),
        arguments: argv
            .iter()
            .map(|argument| ActivityArgument {
                display: argument.clone(),
                redacted: false,
                native_lossless: true,
            })
            .collect(),
        environment: environment
            .iter()
            .map(|(name, value)| ActivityEnvironment {
                name: name.clone(),
                display: value.clone(),
                redacted: false,
            })
            .collect(),
        working_directory: Some(
            worktree
                .worktree_root()
                .as_path()
                .to_string_lossy()
                .into_owned(),
        ),
        stdin_bytes: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_rebase_is_explicit_and_disables_implicit_autostash() {
        assert_eq!(
            interactive_rebase_onto_argv("0123456789abcdef"),
            [
                "-c",
                "rebase.autoStash=false",
                "rebase",
                "--interactive",
                "0123456789abcdef",
            ]
        );
    }
}
