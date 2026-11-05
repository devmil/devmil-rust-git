//! On-disk Git rebase sequencer state.
use crate::types::{ObjectId, RefId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequencerParseError(String);
impl SequencerParseError {
    fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}
impl std::fmt::Display for SequencerParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SequencerParseError {}

/// Which on-disk sequencer layout a paused rebase uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebaseSequencerMode {
    /// `<git_dir>/rebase-merge/` (the default merge backend).
    Merge,
    /// `<git_dir>/rebase-apply/` (the apply backend).
    Apply,
}

/// Raw optional contents of the sequencer state files relevant to rebase discovery. The infrastructure
/// adapter reads them from disk with per-file size caps; missing files stay `None`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RebaseSequencerFiles {
    /// `msgnum` (merge) or `next` (apply): the current step number.
    pub current_step: Option<Vec<u8>>,
    /// `end` (merge) or `last` (apply): the total step count.
    pub total_steps: Option<Vec<u8>>,
    /// `onto`: the commit the rebase replays onto.
    pub onto: Option<Vec<u8>>,
    /// `orig-head`: the pre-rebase HEAD.
    pub orig_head: Option<Vec<u8>>,
    /// `head-name`: the ref the rebase rewrites.
    pub head_name: Option<Vec<u8>>,
    /// `stopped-sha` (merge backend): the commit the sequencer stopped on.
    pub stopped_sha: Option<Vec<u8>>,
    /// Tail of `done` (merge backend), used to classify a no-conflict stop after restart.
    pub done_tail: Option<Vec<u8>>,
}

/// Why an interactive sequencer stopped when no unresolved index paths identify a conflict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebasePauseReason {
    CommitMessage,
    Edit,
    ExecFailure,
    Break,
    Sequencer,
}

/// Typed on-disk state of a paused rebase. Discovery is pure disk state: a fresh adapter
/// instance pointing at the same repository observes the same value, so the paused operation
/// survives an application restart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebaseSequencerState {
    mode: RebaseSequencerMode,
    current_step: Option<u32>,
    total_steps: Option<u32>,
    onto: Option<ObjectId>,
    orig_head: Option<ObjectId>,
    head_name: Option<RefId>,
    stopped: Option<ObjectId>,
    pause_reason: Option<RebasePauseReason>,
}

impl RebaseSequencerState {
    pub const fn mode(&self) -> RebaseSequencerMode {
        self.mode
    }

    pub const fn current_step(&self) -> Option<u32> {
        self.current_step
    }

    pub const fn total_steps(&self) -> Option<u32> {
        self.total_steps
    }

    pub const fn onto(&self) -> Option<&ObjectId> {
        self.onto.as_ref()
    }

    pub const fn orig_head(&self) -> Option<&ObjectId> {
        self.orig_head.as_ref()
    }

    pub const fn head_name(&self) -> Option<&RefId> {
        self.head_name.as_ref()
    }

    pub const fn stopped(&self) -> Option<&ObjectId> {
        self.stopped.as_ref()
    }

    pub const fn pause_reason(&self) -> Option<RebasePauseReason> {
        self.pause_reason
    }
}

fn parse_pause_reason(done_tail: Option<&[u8]>) -> Option<RebasePauseReason> {
    let line = done_tail?
        .split(|byte| *byte == b'\n' || *byte == b'\r')
        .rev()
        .find(|line| {
            let trimmed = line
                .iter()
                .position(|byte| !byte.is_ascii_whitespace())
                .map_or(&[][..], |index| &line[index..]);
            !trimmed.is_empty() && !trimmed.starts_with(b"#")
        })?;
    let verb = line
        .split(u8::is_ascii_whitespace)
        .find(|part| !part.is_empty())?;
    Some(match verb {
        b"reword" | b"r" => RebasePauseReason::CommitMessage,
        b"edit" | b"e" => RebasePauseReason::Edit,
        b"exec" | b"x" => RebasePauseReason::ExecFailure,
        b"break" | b"b" => RebasePauseReason::Break,
        _ => RebasePauseReason::Sequencer,
    })
}

fn parse_sequencer_number(
    raw: Option<&[u8]>,
    file: &str,
) -> Result<Option<u32>, SequencerParseError> {
    let Some(bytes) = raw else { return Ok(None) };
    let text = std::str::from_utf8(bytes).map_err(|_| {
        SequencerParseError::new(format!("rebase sequencer file {file} is not UTF-8"))
    })?;
    text.trim().parse::<u32>().map(Some).map_err(|_| {
        SequencerParseError::new(format!("rebase sequencer file {file} is not a step number"))
    })
}

fn parse_sequencer_oid(
    raw: Option<&[u8]>,
    file: &str,
) -> Result<Option<ObjectId>, SequencerParseError> {
    let Some(bytes) = raw else { return Ok(None) };
    let text = std::str::from_utf8(bytes).map_err(|_| {
        SequencerParseError::new(format!("rebase sequencer file {file} is not UTF-8"))
    })?;
    ObjectId::parse(text.trim()).map(Some).map_err(|_| {
        SequencerParseError::new(format!(
            "rebase sequencer file {file} does not contain an object ID"
        ))
    })
}

/// Parses raw sequencer file contents into typed state. Malformed files are a typed error: a
/// corrupt sequencer directory must not silently read as "no rebase in progress".
///
/// # Errors
///
/// Returns [`SequencerParseError`] when a present file is not valid UTF-8, a step
/// number, an object ID, or a ref name as appropriate.
pub fn parse_sequencer_state(
    mode: RebaseSequencerMode,
    files: &RebaseSequencerFiles,
) -> Result<RebaseSequencerState, SequencerParseError> {
    let head_name = files
        .head_name
        .as_deref()
        .map(|bytes| {
            let trimmed = bytes
                .iter()
                .copied()
                .take_while(|byte| *byte != b'\n' && *byte != b'\r')
                .collect::<Vec<u8>>();
            if trimmed == b"detached HEAD" {
                return Ok(None);
            }
            RefId::from_bytes(trimmed).map(Some).map_err(|_| {
                SequencerParseError::new("rebase sequencer file head-name is not a ref")
            })
        })
        .transpose()?
        .flatten();
    Ok(RebaseSequencerState {
        mode,
        current_step: parse_sequencer_number(files.current_step.as_deref(), "msgnum/next")?,
        total_steps: parse_sequencer_number(files.total_steps.as_deref(), "end/last")?,
        onto: parse_sequencer_oid(files.onto.as_deref(), "onto")?,
        orig_head: parse_sequencer_oid(files.orig_head.as_deref(), "orig-head")?,
        head_name,
        stopped: parse_sequencer_oid(files.stopped_sha.as_deref(), "stopped-sha")?,
        pause_reason: parse_pause_reason(files.done_tail.as_deref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_detached_head_and_restart_pause_reason() {
        let files = RebaseSequencerFiles {
            head_name: Some(b"detached HEAD\n".to_vec()),
            current_step: Some(b"3\n".to_vec()),
            total_steps: Some(b"7\n".to_vec()),
            done_tail: Some(b"pick aaaaaaaa one\nedit bbbbbbbb two\n# comment\n".to_vec()),
            ..RebaseSequencerFiles::default()
        };
        let state = parse_sequencer_state(RebaseSequencerMode::Merge, &files).unwrap();
        assert_eq!(state.current_step(), Some(3));
        assert_eq!(state.total_steps(), Some(7));
        assert_eq!(state.head_name(), None);
        assert_eq!(state.pause_reason(), Some(RebasePauseReason::Edit));
    }

    #[test]
    fn malformed_state_returns_error_without_echoing_bytes() {
        let files = RebaseSequencerFiles {
            onto: Some(b"private invalid object".to_vec()),
            ..RebaseSequencerFiles::default()
        };
        let error = parse_sequencer_state(RebaseSequencerMode::Apply, &files).unwrap_err();
        assert_eq!(
            error.to_string(),
            "rebase sequencer file onto does not contain an object ID"
        );
    }
}
