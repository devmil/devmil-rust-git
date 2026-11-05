//! Lossless Git rebase todo grammar, edits, validation, and progress parsing.

mod commands;
mod sequencer;
pub use commands::*;
pub use sequencer::*;

use crate::activity::ActivityProgress;
use std::fmt;
use std::fmt::Write as _;

const MAX_PROGRESS_PENDING_BYTES: usize = 4 * 1024;

/// Maximum number of lines accepted in a todo file.
pub const MAX_TODO_LINES: usize = 100_000;
/// Maximum byte length of a single todo line, including its terminator.
pub const MAX_TODO_LINE_BYTES: usize = 4 * 1024;

/// Minimum accepted object-ID abbreviation length (`core.abbrev` floor).
const MIN_OID_LEN: usize = 4;
/// Full object-ID lengths for SHA-1 and SHA-256 repositories.
const FULL_OID_LENS: [usize; 2] = [40, 64];

/// A commit object ID exactly as written in the todo file.
///
/// Git writes abbreviated, lowercase hexadecimal IDs by default, but the written form (length and
/// case) is preserved verbatim so unchanged lines survive a parse/render round trip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TodoOid {
    written: String,
}

impl TodoOid {
    /// Creates an object ID from a written hexadecimal token of 4 to 64 characters.
    ///
    /// # Errors
    ///
    /// Returns [`TodoOidError::NotHexadecimal`] for non-ASCII-hexadecimal input and
    /// [`TodoOidError::InvalidLength`] for any length outside 4 to 64 characters.
    pub fn new(written: &str) -> Result<Self, TodoOidError> {
        if !(MIN_OID_LEN..=64).contains(&written.len()) {
            return Err(TodoOidError::InvalidLength);
        }
        if !written.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(TodoOidError::NotHexadecimal);
        }
        Ok(Self {
            written: written.to_owned(),
        })
    }

    /// The object ID exactly as written in the todo file.
    #[must_use]
    pub fn as_written(&self) -> &str {
        &self.written
    }

    /// Whether the written form is a full-length SHA-1 or SHA-256 object ID.
    #[must_use]
    pub fn is_full_length(&self) -> bool {
        FULL_OID_LENS.contains(&self.written.len())
    }

    /// Whether the written form is an abbreviation.
    #[must_use]
    pub fn is_abbreviated(&self) -> bool {
        !self.is_full_length()
    }

    /// Whether two written forms can refer to the same commit.
    ///
    /// Comparison is case-insensitive and treats one written form being a prefix of the other as a
    /// match: todo abbreviations are unambiguous within a plan, so a full ID and its own
    /// abbreviation denote the same commit.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        let left = self.written.as_bytes();
        let right = other.written.as_bytes();
        let shared = left.len().min(right.len());
        left[..shared].eq_ignore_ascii_case(&right[..shared])
    }
}

/// Rejection reasons for [`TodoOid::new`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TodoOidError {
    NotHexadecimal,
    InvalidLength,
}

impl fmt::Display for TodoOidError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotHexadecimal => "object ID is not hexadecimal",
            Self::InvalidLength => "object ID length is outside 4 to 64 characters",
        })
    }
}

impl std::error::Error for TodoOidError {}

/// How `fixup` handles the commit message when `-C` or `-c` is given.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixupAmend {
    /// `fixup -C`: keep only this commit's message.
    UseOwnMessage,
    /// `fixup -c`: keep only this commit's message and open the editor.
    UseOwnMessageEdit,
}

impl FixupAmend {
    fn flag(self) -> &'static str {
        match self {
            Self::UseOwnMessage => "-C",
            Self::UseOwnMessageEdit => "-c",
        }
    }
}

/// A commit-applying instruction target (`pick`, `reword`, `edit`, `squash`, `drop`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitTarget {
    oid: TodoOid,
    /// Display subject bytes after the object ID, with surrounding whitespace removed. Git treats
    /// this as informational commentary (2.55 writes it as `# subject`); it is never executed.
    subject: Vec<u8>,
}

impl CommitTarget {
    /// Creates a target from its written object ID and subject bytes.
    #[must_use]
    pub fn new(oid: TodoOid, subject: Vec<u8>) -> Self {
        Self { oid, subject }
    }

    #[must_use]
    pub fn oid(&self) -> &TodoOid {
        &self.oid
    }

    #[must_use]
    pub fn subject(&self) -> &[u8] {
        &self.subject
    }

    /// The subject as a lossy UTF-8 string for display.
    #[must_use]
    pub fn subject_lossy(&self) -> String {
        String::from_utf8_lossy(&self.subject).into_owned()
    }
}

/// A `reword` target, optionally carrying a message override supplied by the editor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewordTarget {
    oid: TodoOid,
    subject: Vec<u8>,
    /// Editor-supplied replacement message. The todo grammar has no syntax for it, so the override
    /// never renders into todo bytes; the sequence-editor protocol (P06-W02) consumes it to
    /// pre-seed the message editor.
    message_override: Option<Vec<u8>>,
}

impl RewordTarget {
    /// Creates a reword target without a message override.
    #[must_use]
    pub fn new(oid: TodoOid, subject: Vec<u8>) -> Self {
        Self {
            oid,
            subject,
            message_override: None,
        }
    }

    #[must_use]
    pub fn oid(&self) -> &TodoOid {
        &self.oid
    }

    #[must_use]
    pub fn subject(&self) -> &[u8] {
        &self.subject
    }

    #[must_use]
    pub fn message_override(&self) -> Option<&[u8]> {
        self.message_override.as_deref()
    }

    /// Sets or clears the editor-supplied replacement message.
    pub fn set_message_override(&mut self, message: Option<Vec<u8>>) {
        self.message_override = message;
    }
}

/// A `fixup` target with optional `-C`/`-c` message handling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixupTarget {
    oid: TodoOid,
    subject: Vec<u8>,
    amend: Option<FixupAmend>,
}

impl FixupTarget {
    /// Creates a fixup target from its written object ID, subject bytes, and amend mode.
    #[must_use]
    pub fn new(oid: TodoOid, subject: Vec<u8>, amend: Option<FixupAmend>) -> Self {
        Self {
            oid,
            subject,
            amend,
        }
    }

    #[must_use]
    pub fn oid(&self) -> &TodoOid {
        &self.oid
    }

    #[must_use]
    pub fn subject(&self) -> &[u8] {
        &self.subject
    }

    #[must_use]
    pub fn amend(&self) -> Option<FixupAmend> {
        self.amend
    }
}

/// The original merge commit referenced by `merge -C <commit>` or `merge -c <commit>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeTemplate {
    commit: TodoOid,
    /// `true` for `-c` (reword the merge message), `false` for `-C`.
    edit: bool,
}

impl MergeTemplate {
    /// Creates a merge template from its written commit ID and edit mode.
    #[must_use]
    pub fn new(commit: TodoOid, edit: bool) -> Self {
        Self { commit, edit }
    }

    #[must_use]
    pub fn commit(&self) -> &TodoOid {
        &self.commit
    }

    #[must_use]
    pub fn is_edit(&self) -> bool {
        self.edit
    }
}

/// A `merge [-C <commit> | -c <commit>] <label>` instruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeTarget {
    label: String,
    template: Option<MergeTemplate>,
    /// Bytes after the label token (typically ` # <oneline>`), without surrounding whitespace.
    rest: Vec<u8>,
}

impl MergeTarget {
    /// Creates a merge target from its head label, optional template commit, and trailing bytes.
    #[must_use]
    pub fn new(label: String, template: Option<MergeTemplate>, rest: Vec<u8>) -> Self {
        Self {
            label,
            template,
            rest,
        }
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub fn template(&self) -> Option<&MergeTemplate> {
        self.template.as_ref()
    }

    #[must_use]
    pub fn rest(&self) -> &[u8] {
        &self.rest
    }
}

/// A typed todo instruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RebaseCommand {
    Pick(CommitTarget),
    Reword(RewordTarget),
    Edit(CommitTarget),
    Squash(CommitTarget),
    Fixup(FixupTarget),
    Drop(CommitTarget),
    /// `label <name>`: name the current HEAD for later `reset`/`merge` reference.
    Label {
        name: String,
    },
    /// `reset <name>`: move HEAD to a previously defined label.
    Reset {
        name: String,
    },
    Merge(MergeTarget),
    /// `exec <command>`: the command is the raw remainder of the line and may be non-UTF-8.
    Exec {
        command: Vec<u8>,
    },
    Break,
    /// `update-ref <reference>`.
    UpdateRef {
        reference: String,
    },
    /// `noop`: a real Git command that does nothing.
    Noop,
    /// A verb this build does not model (a future Git command). The verb is ASCII; the remainder
    /// of the line is preserved verbatim so the instruction round-trips untouched.
    Unknown {
        verb: String,
        rest: Vec<u8>,
    },
}

impl RebaseCommand {
    /// The canonical long-form verb for this instruction, or the stored verb for unknown
    /// commands.
    #[must_use]
    pub fn verb(&self) -> &str {
        match self {
            Self::Pick(_) => "pick",
            Self::Reword(_) => "reword",
            Self::Edit(_) => "edit",
            Self::Squash(_) => "squash",
            Self::Fixup(_) => "fixup",
            Self::Drop(_) => "drop",
            Self::Label { .. } => "label",
            Self::Reset { .. } => "reset",
            Self::Merge(_) => "merge",
            Self::Exec { .. } => "exec",
            Self::Break => "break",
            Self::UpdateRef { .. } => "update-ref",
            Self::Noop => "noop",
            Self::Unknown { verb, .. } => verb,
        }
    }

    /// The commit object ID this instruction references, when it references one.
    #[must_use]
    pub fn commit_oid(&self) -> Option<&TodoOid> {
        match self {
            Self::Pick(target) | Self::Edit(target) | Self::Squash(target) | Self::Drop(target) => {
                Some(target.oid())
            }
            Self::Reword(target) => Some(target.oid()),
            Self::Fixup(target) => Some(target.oid()),
            Self::Merge(target) => target.template().map(MergeTemplate::commit),
            _ => None,
        }
    }

    /// Whether the instruction leaves a commit behind that a later `squash`/`fixup` can meld into.
    fn produces_commit(&self) -> bool {
        matches!(
            self,
            Self::Pick(_)
                | Self::Reword(_)
                | Self::Edit(_)
                | Self::Squash(_)
                | Self::Fixup(_)
                | Self::Merge(_)
        )
    }
}

/// What a single todo line represents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RebaseEntryKind {
    Command(RebaseCommand),
    /// A line whose first non-whitespace byte is `#`.
    Comment,
    /// A line containing only whitespace.
    Blank,
}

/// One ordered line of the plan, keeping its original bytes until mutated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebasePlanEntry {
    kind: RebaseEntryKind,
    identity: u64,
    /// The original line bytes, including the line terminator as written. Empty for synthesized
    /// entries; ignored once `modified` is set.
    raw: Vec<u8>,
    modified: bool,
}

impl RebasePlanEntry {
    /// Creates an entry from its parsed kind and original line bytes (terminator included).
    ///
    /// Creates an entry for the lossless todo parser.
    #[must_use]
    pub fn parsed(kind: RebaseEntryKind, raw: Vec<u8>) -> Self {
        Self {
            kind,
            identity: 0,
            raw,
            modified: false,
        }
    }

    /// Creates an entry without original bytes; it renders canonically.
    #[must_use]
    pub fn synthesized(kind: RebaseEntryKind) -> Self {
        Self {
            kind,
            identity: 0,
            raw: Vec::new(),
            modified: true,
        }
    }

    #[must_use]
    pub fn kind(&self) -> &RebaseEntryKind {
        &self.kind
    }

    /// Plan-local stable identity retained across reorder and action changes.
    #[must_use]
    pub const fn identity(&self) -> u64 {
        self.identity
    }

    /// The command when this entry is one.
    #[must_use]
    pub fn command(&self) -> Option<&RebaseCommand> {
        match &self.kind {
            RebaseEntryKind::Command(command) => Some(command),
            _ => None,
        }
    }

    /// The original line bytes, including the terminator as written.
    #[must_use]
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }

    /// The original line bytes without the line terminator.
    #[must_use]
    pub fn content_bytes(&self) -> &[u8] {
        let content = self.raw.strip_suffix(b"\n").unwrap_or(&self.raw);
        content.strip_suffix(b"\r").unwrap_or(content)
    }

    /// Whether the entry renders canonically instead of from its original bytes.
    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.modified
    }

    fn replace_command(&mut self, command: RebaseCommand) {
        self.kind = RebaseEntryKind::Command(command);
        self.modified = true;
    }
}

/// The ordered todo plan: typed instructions interleaved with preserved comment and blank lines.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RebasePlan {
    entries: Vec<RebasePlanEntry>,
}

impl RebasePlan {
    /// Creates a plan from ordered entries. Parse bytes with [`parse_todo`].
    #[must_use]
    pub fn new(mut entries: Vec<RebasePlanEntry>) -> Self {
        let mut next = entries
            .iter()
            .map(RebasePlanEntry::identity)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        for entry in &mut entries {
            if entry.identity == 0 {
                entry.identity = next;
                next = next.saturating_add(1);
            }
        }
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[RebasePlanEntry] {
        &self.entries
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Moves the entry at `from` so it lands at index `to`.
    ///
    /// Moving does not mark the entry modified: its original bytes move with it.
    ///
    /// # Errors
    ///
    /// Returns [`RebasePlanEditError::IndexOutOfBounds`] when either index is outside the plan.
    pub fn move_entry(&mut self, from: usize, to: usize) -> Result<(), RebasePlanEditError> {
        let len = self.entries.len();
        if from >= len {
            return Err(RebasePlanEditError::IndexOutOfBounds { index: from, len });
        }
        if to >= len {
            return Err(RebasePlanEditError::IndexOutOfBounds { index: to, len });
        }
        if from != to {
            let entry = self.entries.remove(from);
            self.entries.insert(to, entry);
        }
        Ok(())
    }

    /// Moves one commit instruction immediately after another and marks it as squash.
    ///
    /// # Errors
    ///
    /// Refuses out-of-range, identical, non-commit source, and non-producing target entries.
    pub fn squash_entry_onto(
        &mut self,
        from: usize,
        target: usize,
    ) -> Result<(), RebasePlanEditError> {
        let len = self.entries.len();
        if from >= len {
            return Err(RebasePlanEditError::IndexOutOfBounds { index: from, len });
        }
        if target >= len {
            return Err(RebasePlanEditError::IndexOutOfBounds { index: target, len });
        }
        if from == target {
            return Err(RebasePlanEditError::SameEntry { index: from });
        }
        if !matches!(
            self.entries[target].command(),
            Some(
                RebaseCommand::Pick(_)
                    | RebaseCommand::Reword(_)
                    | RebaseCommand::Edit(_)
                    | RebaseCommand::Squash(_)
                    | RebaseCommand::Fixup(_)
            )
        ) {
            return Err(RebasePlanEditError::NotACommitProducer { index: target });
        }
        self.set_action(from, RebaseAction::Squash)?;
        let destination = if from < target { target } else { target + 1 };
        self.move_entry(from, destination)
    }

    /// Applies Git-style `fixup!` and `squash!` subject hints as an editable preview.
    /// Stable plan identities survive both the action change and reorder.
    #[must_use]
    pub fn apply_autosquash_preview(&mut self) -> usize {
        let original = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let subject = rebase_command_subject(entry.command()?)?;
                let (prefix, action) = subject
                    .strip_prefix(b"fixup! ")
                    .map(|target| (target, RebaseAction::Fixup { amend: None }))
                    .or_else(|| {
                        subject
                            .strip_prefix(b"squash! ")
                            .map(|target| (target, RebaseAction::Squash))
                    })?;
                let target = self.entries[..index]
                    .iter()
                    .rev()
                    .find(|candidate| {
                        candidate.command().and_then(rebase_command_subject) == Some(prefix)
                    })?
                    .identity();
                Some((entry.identity(), target, action))
            })
            .collect::<Vec<_>>();
        let mut tails = std::collections::BTreeMap::new();
        let mut applied = 0;
        for (identity, target, action) in original {
            let Some(from) = self
                .entries
                .iter()
                .position(|entry| entry.identity() == identity)
            else {
                continue;
            };
            if self.set_action(from, action).is_err() {
                continue;
            }
            let anchor = tails.get(&target).copied().unwrap_or(target);
            let Some(from) = self
                .entries
                .iter()
                .position(|entry| entry.identity() == identity)
            else {
                continue;
            };
            let Some(anchor_index) = self
                .entries
                .iter()
                .position(|entry| entry.identity() == anchor)
            else {
                continue;
            };
            let to = if from < anchor_index {
                anchor_index
            } else {
                anchor_index.saturating_add(1).min(self.entries.len() - 1)
            };
            let _ = self.move_entry(from, to);
            tails.insert(target, identity);
            applied += 1;
        }
        applied
    }

    /// Changes the instruction of a commit entry (for example pick to squash), preserving the
    /// written object ID and subject. The entry then renders canonically.
    ///
    /// Changing an entry to `reword` keeps any existing message override; changing away from
    /// `reword` discards it. Setting the action the entry already has is a no-op that leaves the
    /// original bytes untouched.
    ///
    /// # Errors
    ///
    /// Returns [`RebasePlanEditError::IndexOutOfBounds`] for a missing entry and
    /// [`RebasePlanEditError::NotACommitEntry`] for comments, blank lines, and non-commit
    /// instructions.
    pub fn set_action(
        &mut self,
        index: usize,
        action: RebaseAction,
    ) -> Result<(), RebasePlanEditError> {
        let len = self.entries.len();
        let entry = self
            .entries
            .get_mut(index)
            .ok_or(RebasePlanEditError::IndexOutOfBounds { index, len })?;
        let command = entry
            .command()
            .ok_or(RebasePlanEditError::NotACommitEntry { index })?;
        let (oid, subject) = match command {
            RebaseCommand::Pick(target)
            | RebaseCommand::Edit(target)
            | RebaseCommand::Squash(target)
            | RebaseCommand::Drop(target) => (target.oid().clone(), target.subject().to_vec()),
            RebaseCommand::Reword(target) => (target.oid().clone(), target.subject().to_vec()),
            RebaseCommand::Fixup(target) => (target.oid().clone(), target.subject().to_vec()),
            _ => return Err(RebasePlanEditError::NotACommitEntry { index }),
        };
        let existing_override = match command {
            RebaseCommand::Reword(target) => target.message_override().map(<[u8]>::to_vec),
            _ => None,
        };
        let replacement = match action {
            RebaseAction::Pick => RebaseCommand::Pick(CommitTarget::new(oid, subject)),
            RebaseAction::Reword => {
                let mut target = RewordTarget::new(oid, subject);
                target.set_message_override(existing_override);
                RebaseCommand::Reword(target)
            }
            RebaseAction::Edit => RebaseCommand::Edit(CommitTarget::new(oid, subject)),
            RebaseAction::Squash => RebaseCommand::Squash(CommitTarget::new(oid, subject)),
            RebaseAction::Drop => RebaseCommand::Drop(CommitTarget::new(oid, subject)),
            RebaseAction::Fixup { amend } => {
                RebaseCommand::Fixup(FixupTarget::new(oid, subject, amend))
            }
        };
        if entry.command() == Some(&replacement) {
            return Ok(());
        }
        entry.replace_command(replacement);
        Ok(())
    }

    /// Sets or clears the editor-supplied replacement message of a `reword` entry.
    ///
    /// The override is model metadata: the todo grammar cannot express it, so the entry keeps its
    /// original bytes and continues to render unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`RebasePlanEditError::IndexOutOfBounds`] for a missing entry and
    /// [`RebasePlanEditError::NotARewordEntry`] when the entry is not a `reword` instruction.
    pub fn set_reword_message(
        &mut self,
        index: usize,
        message: Option<Vec<u8>>,
    ) -> Result<(), RebasePlanEditError> {
        let len = self.entries.len();
        let entry = self
            .entries
            .get_mut(index)
            .ok_or(RebasePlanEditError::IndexOutOfBounds { index, len })?;
        match &mut entry.kind {
            RebaseEntryKind::Command(RebaseCommand::Reword(target)) => {
                target.set_message_override(message);
                Ok(())
            }
            _ => Err(RebasePlanEditError::NotARewordEntry { index }),
        }
    }
}

fn rebase_command_subject(command: &RebaseCommand) -> Option<&[u8]> {
    match command {
        RebaseCommand::Pick(target)
        | RebaseCommand::Edit(target)
        | RebaseCommand::Squash(target)
        | RebaseCommand::Drop(target) => Some(target.subject()),
        RebaseCommand::Reword(target) => Some(target.subject()),
        RebaseCommand::Fixup(target) => Some(target.subject()),
        _ => None,
    }
}

/// The commit-command actions the rebase editor can assign to an entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebaseAction {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup { amend: Option<FixupAmend> },
    Drop,
}

/// Rejection reasons for plan editor operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebasePlanEditError {
    IndexOutOfBounds { index: usize, len: usize },
    NotACommitEntry { index: usize },
    NotARewordEntry { index: usize },
    SameEntry { index: usize },
    NotACommitProducer { index: usize },
}

impl fmt::Display for RebasePlanEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IndexOutOfBounds { index, len } => {
                write!(
                    formatter,
                    "entry index {index} is outside the plan of {len} entries"
                )
            }
            Self::NotACommitEntry { index } => {
                write!(formatter, "entry {index} is not a commit instruction")
            }
            Self::NotARewordEntry { index } => {
                write!(formatter, "entry {index} is not a reword instruction")
            }
            Self::SameEntry { index } => {
                write!(formatter, "entry {index} cannot target itself")
            }
            Self::NotACommitProducer { index } => {
                write!(
                    formatter,
                    "entry {index} is not a commit-producing instruction"
                )
            }
        }
    }
}

impl std::error::Error for RebasePlanEditError {}

fn render_canonical(kind: &RebaseEntryKind, output: &mut Vec<u8>) {
    match kind {
        RebaseEntryKind::Comment | RebaseEntryKind::Blank => {}
        RebaseEntryKind::Command(command) => match command {
            RebaseCommand::Pick(target) => render_commit_line("pick", target, output),
            RebaseCommand::Reword(target) => {
                render_commit_line(
                    "reword",
                    &CommitTarget::new(target.oid().clone(), target.subject().to_vec()),
                    output,
                );
            }
            RebaseCommand::Edit(target) => render_commit_line("edit", target, output),
            RebaseCommand::Squash(target) => render_commit_line("squash", target, output),
            RebaseCommand::Drop(target) => render_commit_line("drop", target, output),
            RebaseCommand::Fixup(target) => {
                output.extend_from_slice(b"fixup");
                if let Some(amend) = target.amend() {
                    output.push(b' ');
                    output.extend_from_slice(amend.flag().as_bytes());
                }
                output.push(b' ');
                output.extend_from_slice(target.oid().as_written().as_bytes());
                if !target.subject().is_empty() {
                    output.push(b' ');
                    output.extend_from_slice(target.subject());
                }
            }
            RebaseCommand::Label { name } => render_named_line("label", name, output),
            RebaseCommand::Reset { name } => render_named_line("reset", name, output),
            RebaseCommand::UpdateRef { reference } => {
                render_named_line("update-ref", reference, output);
            }
            RebaseCommand::Merge(target) => {
                output.extend_from_slice(b"merge");
                if let Some(template) = target.template() {
                    output.extend_from_slice(if template.is_edit() { b" -c " } else { b" -C " });
                    output.extend_from_slice(template.commit().as_written().as_bytes());
                }
                output.push(b' ');
                output.extend_from_slice(target.label().as_bytes());
                if !target.rest().is_empty() {
                    output.push(b' ');
                    output.extend_from_slice(target.rest());
                }
            }
            RebaseCommand::Exec { command } => {
                output.extend_from_slice(b"exec");
                if !command.is_empty() {
                    output.push(b' ');
                    output.extend_from_slice(command);
                }
            }
            RebaseCommand::Break => output.extend_from_slice(b"break"),
            RebaseCommand::Noop => output.extend_from_slice(b"noop"),
            RebaseCommand::Unknown { verb, rest } => {
                output.extend_from_slice(verb.as_bytes());
                output.extend_from_slice(rest);
            }
        },
    }
}

fn render_commit_line(verb: &str, target: &CommitTarget, output: &mut Vec<u8>) {
    output.extend_from_slice(verb.as_bytes());
    output.push(b' ');
    output.extend_from_slice(target.oid().as_written().as_bytes());
    if !target.subject().is_empty() {
        output.push(b' ');
        output.extend_from_slice(target.subject());
    }
}

fn render_named_line(verb: &str, name: &str, output: &mut Vec<u8>) {
    output.extend_from_slice(verb.as_bytes());
    output.push(b' ');
    output.extend_from_slice(name.as_bytes());
}

/// Renders the plan back to todo bytes.
///
/// Unmodified entries contribute their original bytes verbatim, so rendering a freshly parsed plan
/// reproduces the input byte-for-byte. Modified and synthesized entries render canonically with an
/// LF terminator.
#[must_use]
pub fn render_todo(plan: &RebasePlan) -> Vec<u8> {
    let mut output = Vec::new();
    for entry in plan.entries() {
        if entry.is_modified() {
            render_canonical(entry.kind(), &mut output);
            output.push(b'\n');
        } else {
            output.extend_from_slice(entry.raw_bytes());
        }
    }
    output
}

/// Rejection reasons for todo parsing. Carried by [`parse_todo`] so the
/// model and its parser share one error type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebasePlanError {
    /// The file has more than [`MAX_TODO_LINES`] lines.
    TooManyLines { limit: usize },
    /// A line exceeds [`MAX_TODO_LINE_BYTES`].
    LineTooLong { line: usize, limit: usize },
    /// A known instruction is malformed. Line numbers are 1-based.
    MalformedLine {
        line: usize,
        reason: RebaseLineError,
    },
}

impl fmt::Display for RebasePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyLines { limit } => {
                write!(formatter, "todo file exceeds the {limit} line limit")
            }
            Self::LineTooLong { line, limit } => {
                write!(formatter, "todo line {line} exceeds the {limit} byte limit")
            }
            Self::MalformedLine { line, reason } => {
                write!(formatter, "todo line {line} is malformed: {reason}")
            }
        }
    }
}

impl std::error::Error for RebasePlanError {}

/// Why a known todo instruction could not be parsed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebaseLineError {
    /// The command verb is not ASCII.
    NonAsciiCommand,
    /// A commit-applying instruction has no object ID.
    MissingOid,
    /// The object ID token is not 4 to 64 hexadecimal characters.
    InvalidOid,
    /// `label`, `reset`, or `merge` has no label, or `merge -C`/`-c` has no commit.
    MissingLabel,
    /// `update-ref` has no reference.
    MissingReference,
}

impl fmt::Display for RebaseLineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NonAsciiCommand => "the command verb is not ASCII",
            Self::MissingOid => "the instruction has no object ID",
            Self::InvalidOid => "the object ID is not 4 to 64 hexadecimal characters",
            Self::MissingLabel => "the instruction has no label",
            Self::MissingReference => "the instruction has no reference",
        })
    }
}

impl std::error::Error for RebaseLineError {}

/// Rejection reasons returned by [`validate_plan`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RebaseValidationError {
    /// Two commit instructions reference the same object ID. IDs compare case-insensitively, and
    /// a written form that prefixes another matches it (a full ID matches its own abbreviation).
    DuplicateCommit {
        first_index: usize,
        second_index: usize,
        oid: String,
    },
    /// A `squash` or `fixup` has no preceding commit-producing instruction to meld into.
    SquashOrFixupWithoutCommit { index: usize },
    /// A `reset` or `merge` references a label no earlier `label` instruction defines.
    UndefinedLabel { index: usize, label: String },
}

impl fmt::Display for RebaseValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateCommit {
                first_index,
                second_index,
                oid,
            } => write!(
                formatter,
                "entries {first_index} and {second_index} reference the same commit {oid}"
            ),
            Self::SquashOrFixupWithoutCommit { index } => {
                write!(
                    formatter,
                    "entry {index} has no preceding commit to meld into"
                )
            }
            Self::UndefinedLabel { index, label } => {
                write!(
                    formatter,
                    "entry {index} references undefined label `{label}`"
                )
            }
        }
    }
}

impl std::error::Error for RebaseValidationError {}

/// Validates commit uniqueness and legal instruction ordering.
///
/// Kept separate from parsing: a plan Git would refuse can still be loaded, displayed, and
/// preserved, and callers decide when validation applies.
///
/// # Errors
///
/// Returns the first [`RebaseValidationError`] found: duplicate commits are checked across the
/// whole plan first, then ordering and label bookkeeping are checked in entry order.
pub fn validate_plan(plan: &RebasePlan) -> Result<(), RebaseValidationError> {
    let commits: Vec<(usize, &TodoOid)> = plan
        .entries()
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            entry
                .command()
                .and_then(RebaseCommand::commit_oid)
                .map(|oid| (index, oid))
        })
        .collect();
    for (position, (first_index, first_oid)) in commits.iter().enumerate() {
        for (second_index, second_oid) in &commits[position + 1..] {
            if first_oid.matches(second_oid) {
                return Err(RebaseValidationError::DuplicateCommit {
                    first_index: *first_index,
                    second_index: *second_index,
                    oid: second_oid.as_written().to_owned(),
                });
            }
        }
    }

    let mut labels: Vec<&str> = Vec::new();
    let mut commit_seen = false;
    for (index, entry) in plan.entries().iter().enumerate() {
        match entry.command() {
            Some(RebaseCommand::Squash(_) | RebaseCommand::Fixup(_)) if !commit_seen => {
                return Err(RebaseValidationError::SquashOrFixupWithoutCommit { index });
            }
            Some(RebaseCommand::Label { name }) => labels.push(name),
            Some(RebaseCommand::Reset { name }) if !labels.contains(&name.as_str()) => {
                return Err(RebaseValidationError::UndefinedLabel {
                    index,
                    label: name.clone(),
                });
            }
            Some(RebaseCommand::Merge(target)) if !labels.contains(&target.label()) => {
                return Err(RebaseValidationError::UndefinedLabel {
                    index,
                    label: target.label().to_owned(),
                });
            }
            Some(command) if command.produces_commit() => commit_seen = true,
            _ => {}
        }
    }
    Ok(())
}

/// Formats a validation failure list for display. Helper for future UI surfaces.
#[must_use]
pub fn describe_validation(error: &RebaseValidationError) -> String {
    let mut description = String::new();
    let _ = write!(description, "{error}");
    description
}

/// Parses todo file bytes into a typed, lossless [`RebasePlan`].
///
/// Accepts LF and CRLF terminators, a missing trailing newline on the last line, abbreviated and
/// full object IDs, single-letter command abbreviations (`p`, `r`, `e`, `s`, `f`, `x`, `b`, `d`,
/// `l`, `t`, `m`, `u`), `fixup -C`/`-c`, `merge -C`/`-c` with labels, `reset`, `label`,
/// `update-ref`, `break`, `noop`, arbitrary (possibly non-UTF-8) `exec` commands, comment lines
/// whose first non-whitespace byte is `#`, blank lines, and unknown verbs, which are preserved
/// rather than rejected. The empty file parses to an empty plan.
///
/// # Errors
///
/// Returns [`RebasePlanError::TooManyLines`] above [`MAX_TODO_LINES`] lines,
/// [`RebasePlanError::LineTooLong`] when a line exceeds [`MAX_TODO_LINE_BYTES`], and
/// [`RebasePlanError::MalformedLine`] when a known instruction is missing its object ID, label, or
/// reference, or its verb is not ASCII.
pub fn parse_todo(bytes: &[u8]) -> Result<RebasePlan, RebasePlanError> {
    let mut entries = Vec::new();
    for (offset, raw) in LineIter::new(bytes).enumerate() {
        let line = offset + 1;
        if raw.len() > MAX_TODO_LINE_BYTES {
            return Err(RebasePlanError::LineTooLong {
                line,
                limit: MAX_TODO_LINE_BYTES,
            });
        }
        if line > MAX_TODO_LINES {
            return Err(RebasePlanError::TooManyLines {
                limit: MAX_TODO_LINES,
            });
        }
        let kind = parse_line(content_of(raw), line)?;
        entries.push(RebasePlanEntry::parsed(kind, raw.to_vec()));
    }
    Ok(RebasePlan::new(entries))
}

/// Iterates lines including their terminators; the final line may have none.
struct LineIter<'a> {
    bytes: &'a [u8],
}

impl<'a> LineIter<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl<'a> Iterator for LineIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let end = match self.bytes.iter().position(|byte| *byte == b'\n') {
            Some(position) => position + 1,
            None => self.bytes.len(),
        };
        let (line, rest) = self.bytes.split_at(end);
        self.bytes = rest;
        Some(line)
    }
}

/// Line content without the trailing `\n` / `\r\n` terminator.
fn content_of(raw: &[u8]) -> &[u8] {
    let content = raw.strip_suffix(b"\n").unwrap_or(raw);
    content.strip_suffix(b"\r").unwrap_or(content)
}

fn skip_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

/// Splits off the first whitespace-delimited token, returning it and the bytes after it
/// (separator included).
fn take_token(bytes: &[u8]) -> (&[u8], &[u8]) {
    let end = bytes
        .iter()
        .position(|byte| matches!(byte, b' ' | b'\t'))
        .unwrap_or(bytes.len());
    bytes.split_at(end)
}

/// Trailing bytes of a token argument, without surrounding whitespace.
fn take_rest(bytes: &[u8]) -> Vec<u8> {
    let rest = skip_whitespace(bytes);
    let end = rest
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t'))
        .map_or(0, |position| position + 1);
    rest[..end].to_vec()
}

fn parse_oid(token: &[u8], line: usize) -> Result<TodoOid, RebasePlanError> {
    if token.is_empty() {
        return Err(RebasePlanError::MalformedLine {
            line,
            reason: RebaseLineError::MissingOid,
        });
    }
    let written = std::str::from_utf8(token).map_err(|_| RebasePlanError::MalformedLine {
        line,
        reason: RebaseLineError::InvalidOid,
    })?;
    TodoOid::new(written).map_err(|_| RebasePlanError::MalformedLine {
        line,
        reason: RebaseLineError::InvalidOid,
    })
}

/// Parses `<oid> [subject...]` after the verb of a commit-applying instruction.
fn parse_commit_target(rest: &[u8], line: usize) -> Result<(TodoOid, Vec<u8>), RebasePlanError> {
    let (token, after) = take_token(skip_whitespace(rest));
    let oid = parse_oid(token, line)?;
    Ok((oid, take_rest(after)))
}

/// Parses a required single-token name (`label`, `reset`, `update-ref`).
fn parse_name(
    rest: &[u8],
    line: usize,
    missing: RebaseLineError,
) -> Result<String, RebasePlanError> {
    let (token, _) = take_token(skip_whitespace(rest));
    if token.is_empty() {
        return Err(RebasePlanError::MalformedLine {
            line,
            reason: missing,
        });
    }
    std::str::from_utf8(token)
        .map(str::to_owned)
        .map_err(|_| RebasePlanError::MalformedLine {
            line,
            reason: missing,
        })
}

fn parse_fixup(rest: &[u8], line: usize) -> Result<RebaseCommand, RebasePlanError> {
    let rest = skip_whitespace(rest);
    let (token, after) = take_token(rest);
    let (amend, oid_rest) = match token {
        b"-C" => (Some(FixupAmend::UseOwnMessage), after),
        b"-c" => (Some(FixupAmend::UseOwnMessageEdit), after),
        _ => (None, rest),
    };
    let (oid, subject) = parse_commit_target(oid_rest, line)?;
    Ok(RebaseCommand::Fixup(FixupTarget::new(oid, subject, amend)))
}

fn parse_merge(rest: &[u8], line: usize) -> Result<RebaseCommand, RebasePlanError> {
    let rest = skip_whitespace(rest);
    let (token, after) = take_token(rest);
    let (template, label_rest) = match token {
        b"-C" | b"-c" => {
            let (oid_token, after_oid) = take_token(skip_whitespace(after));
            let commit = parse_oid(oid_token, line)?;
            (Some(MergeTemplate::new(commit, token == b"-c")), after_oid)
        }
        _ => (None, rest),
    };
    let label_rest = skip_whitespace(label_rest);
    let (label_token, after_label) = take_token(label_rest);
    if label_token.is_empty() {
        return Err(RebasePlanError::MalformedLine {
            line,
            reason: RebaseLineError::MissingLabel,
        });
    }
    let label = std::str::from_utf8(label_token).map_err(|_| RebasePlanError::MalformedLine {
        line,
        reason: RebaseLineError::MissingLabel,
    })?;
    Ok(RebaseCommand::Merge(MergeTarget::new(
        label.to_owned(),
        template,
        take_rest(after_label),
    )))
}

fn parse_line(content: &[u8], line: usize) -> Result<RebaseEntryKind, RebasePlanError> {
    let body = skip_whitespace(content);
    if body.is_empty() {
        return Ok(RebaseEntryKind::Blank);
    }
    if body[0] == b'#' {
        return Ok(RebaseEntryKind::Comment);
    }
    let (verb_bytes, rest) = take_token(body);
    let verb = std::str::from_utf8(verb_bytes).map_err(|_| RebasePlanError::MalformedLine {
        line,
        reason: RebaseLineError::NonAsciiCommand,
    })?;
    if !verb
        .bytes()
        .all(|byte| byte.is_ascii_alphabetic() || byte == b'-')
    {
        return Err(RebasePlanError::MalformedLine {
            line,
            reason: RebaseLineError::NonAsciiCommand,
        });
    }

    let command = match verb {
        "p" | "pick" => {
            let (oid, subject) = parse_commit_target(rest, line)?;
            RebaseCommand::Pick(CommitTarget::new(oid, subject))
        }
        "r" | "reword" => {
            let (oid, subject) = parse_commit_target(rest, line)?;
            RebaseCommand::Reword(RewordTarget::new(oid, subject))
        }
        "e" | "edit" => {
            let (oid, subject) = parse_commit_target(rest, line)?;
            RebaseCommand::Edit(CommitTarget::new(oid, subject))
        }
        "s" | "squash" => {
            let (oid, subject) = parse_commit_target(rest, line)?;
            RebaseCommand::Squash(CommitTarget::new(oid, subject))
        }
        "f" | "fixup" => return parse_fixup(rest, line).map(RebaseEntryKind::Command),
        "d" | "drop" => {
            let (oid, subject) = parse_commit_target(rest, line)?;
            RebaseCommand::Drop(CommitTarget::new(oid, subject))
        }
        "l" | "label" => RebaseCommand::Label {
            name: parse_name(rest, line, RebaseLineError::MissingLabel)?,
        },
        "t" | "reset" => RebaseCommand::Reset {
            name: parse_name(rest, line, RebaseLineError::MissingLabel)?,
        },
        "m" | "merge" => return parse_merge(rest, line).map(RebaseEntryKind::Command),
        "x" | "exec" => RebaseCommand::Exec {
            command: skip_whitespace(rest).to_vec(),
        },
        "b" | "break" => RebaseCommand::Break,
        "u" | "update-ref" => RebaseCommand::UpdateRef {
            reference: parse_name(rest, line, RebaseLineError::MissingReference)?,
        },
        "noop" => RebaseCommand::Noop,
        _ => RebaseCommand::Unknown {
            verb: verb.to_owned(),
            rest: rest.to_vec(),
        },
    };
    Ok(RebaseEntryKind::Command(command))
}

/// One parsed sequencer progress marker (`Rebasing (done/total)`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RebaseStep {
    pub completed: u64,
    pub total: u64,
}

/// Incremental parser for the sequencer's stderr progress markers. Markers are separated by
/// carriage returns or newlines and may split across chunks; consecutive duplicates are
/// suppressed so the event stream stays bounded by the step count. Unterminated input is
/// retained up to a fixed cap.
#[derive(Clone, Debug, Default)]
pub struct RebaseProgressAccumulator {
    pending: Vec<u8>,
    last: Option<RebaseStep>,
}

impl RebaseProgressAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one stderr chunk and returns every newly observed step.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<RebaseStep> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > MAX_PROGRESS_PENDING_BYTES {
            let keep = self
                .pending
                .split_off(self.pending.len() - MAX_PROGRESS_PENDING_BYTES);
            self.pending = keep;
        }
        let mut steps = Vec::new();
        let mut start = 0;
        for (index, byte) in self.pending.iter().enumerate() {
            if *byte == b'\r' || *byte == b'\n' {
                if let Some(step) = parse_rebase_step(&self.pending[start..index]) {
                    if self.last != Some(step) {
                        self.last = Some(step);
                        steps.push(step);
                    }
                }
                start = index + 1;
            }
        }
        self.pending.drain(..start);
        steps
    }

    /// Flushes a trailing unterminated segment at end of stream.
    pub fn finish(&mut self) -> Vec<RebaseStep> {
        let pending = std::mem::take(&mut self.pending);
        parse_rebase_step(&pending)
            .filter(|step| {
                if self.last == Some(*step) {
                    false
                } else {
                    self.last = Some(*step);
                    true
                }
            })
            .into_iter()
            .collect()
    }
}

/// Parses `Rebasing (done/total)` anywhere inside one segment.
fn parse_rebase_step(segment: &[u8]) -> Option<RebaseStep> {
    const MARKER: &[u8] = b"Rebasing (";
    let start = segment
        .windows(MARKER.len())
        .position(|window| window == MARKER)?;
    let rest = &segment[start + MARKER.len()..];
    let end = rest.iter().position(|byte| *byte == b')')?;
    let inner = std::str::from_utf8(&rest[..end]).ok()?;
    let (completed, total) = inner.split_once('/')?;
    let completed = completed.trim().parse::<u64>().ok()?;
    let total = total.trim().parse::<u64>().ok()?;
    if total == 0 || completed > total {
        return None;
    }
    Some(RebaseStep { completed, total })
}

/// Maps one parsed step to the bounded activity progress shape.
#[must_use]
pub fn rebase_step_progress(step: RebaseStep) -> Option<ActivityProgress> {
    ActivityProgress::new("Rebasing", step.completed, Some(step.total), "commits").ok()
}

#[cfg(test)]
mod model_tests {
    use super::*;
    fn oid(written: &str) -> TodoOid {
        TodoOid::new(written).unwrap()
    }

    fn pick(written: &str, subject: &str) -> RebasePlanEntry {
        RebasePlanEntry::parsed(
            RebaseEntryKind::Command(RebaseCommand::Pick(CommitTarget::new(
                oid(written),
                subject.as_bytes().to_vec(),
            ))),
            format!("pick {written} {subject}\n").into_bytes(),
        )
    }

    #[test]
    fn todo_oid_rejects_non_hex_and_bad_length() {
        assert_eq!(TodoOid::new("zzzzzzz"), Err(TodoOidError::NotHexadecimal));
        assert_eq!(TodoOid::new("abc"), Err(TodoOidError::InvalidLength));
        assert!(TodoOid::new("704a3f1").is_ok());
        assert!(TodoOid::new(&"a".repeat(40)).unwrap().is_full_length());
        assert!(TodoOid::new("704a3f1").unwrap().is_abbreviated());
    }

    #[test]
    fn todo_oid_matches_full_and_abbreviated_case_insensitively() {
        let full = oid("704a3f1b2c3d4e5f60718293a4b5c6d7e8f9a0b1");
        let abbrev = oid("704A3F1");
        assert!(full.matches(&abbrev));
        assert!(abbrev.matches(&full));
        assert!(!abbrev.matches(&oid("704a3f2")));
    }

    #[test]
    fn move_entry_reorders_without_modifying_bytes() {
        let mut plan = RebasePlan::new(vec![
            pick("aaaaaaa", "one"),
            pick("bbbbbbb", "two"),
            pick("ccccccc", "three"),
        ]);
        plan.move_entry(2, 0).unwrap();
        let rendered = render_todo(&plan);
        assert_eq!(
            rendered,
            b"pick ccccccc three\npick aaaaaaa one\npick bbbbbbb two\n".to_vec()
        );
        assert!(plan.entries().iter().all(|entry| !entry.is_modified()));
        assert_eq!(
            plan.entries()
                .iter()
                .map(RebasePlanEntry::identity)
                .collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
    }

    #[test]
    fn move_entry_rejects_out_of_bounds() {
        let mut plan = RebasePlan::new(vec![pick("aaaaaaa", "one")]);
        assert_eq!(
            plan.move_entry(0, 1),
            Err(RebasePlanEditError::IndexOutOfBounds { index: 1, len: 1 })
        );
        assert_eq!(
            plan.move_entry(5, 0),
            Err(RebasePlanEditError::IndexOutOfBounds { index: 5, len: 1 })
        );
    }

    #[test]
    fn squash_drop_moves_after_target_and_preserves_both_identities() {
        let mut plan = RebasePlan::new(vec![
            pick("aaaaaaa", "one"),
            pick("bbbbbbb", "two"),
            pick("ccccccc", "three"),
        ]);
        plan.squash_entry_onto(2, 0).unwrap();
        assert_eq!(
            render_todo(&plan),
            b"pick aaaaaaa one\nsquash ccccccc three\npick bbbbbbb two\n".to_vec()
        );
        assert_eq!(
            plan.entries()
                .iter()
                .map(RebasePlanEntry::identity)
                .collect::<Vec<_>>(),
            vec![1, 3, 2]
        );
        assert_eq!(
            plan.squash_entry_onto(1, 1),
            Err(RebasePlanEditError::SameEntry { index: 1 })
        );
    }

    #[test]
    fn autosquash_preview_reorders_hints_and_preserves_stable_identities() {
        let mut plan = RebasePlan::new(vec![
            pick("aaaaaaa", "base"),
            pick("bbbbbbb", "unrelated"),
            pick("ccccccc", "fixup! base"),
            pick("ddddddd", "squash! base"),
        ]);
        let identities = plan
            .entries()
            .iter()
            .map(RebasePlanEntry::identity)
            .collect::<Vec<_>>();
        assert_eq!(plan.apply_autosquash_preview(), 2);
        assert_eq!(
            plan.entries()
                .iter()
                .map(RebasePlanEntry::identity)
                .collect::<Vec<_>>(),
            vec![identities[0], identities[2], identities[3], identities[1]]
        );
        assert_eq!(
            render_todo(&plan),
            b"pick aaaaaaa base\nfixup ccccccc fixup! base\nsquash ddddddd squash! base\npick bbbbbbb unrelated\n"
        );
    }

    #[test]
    fn set_action_rewrites_only_the_changed_line() {
        let mut plan = RebasePlan::new(vec![
            pick("aaaaaaa", "one"),
            RebasePlanEntry::parsed(
                RebaseEntryKind::Comment,
                b"# keep   these   bytes\r\n".to_vec(),
            ),
            pick("bbbbbbb", "two"),
        ]);
        plan.set_action(2, RebaseAction::Squash).unwrap();
        assert_eq!(
            render_todo(&plan),
            b"pick aaaaaaa one\n# keep   these   bytes\r\nsquash bbbbbbb two\n".to_vec()
        );
    }

    #[test]
    fn set_action_to_same_action_preserves_original_bytes() {
        let mut plan = RebasePlan::new(vec![RebasePlanEntry::parsed(
            RebaseEntryKind::Command(RebaseCommand::Pick(CommitTarget::new(
                oid("aaaaaaa"),
                b" spaced  subject".to_vec(),
            ))),
            b"pick   aaaaaaaa wrong-but-original\n".to_vec(),
        )]);
        // Kind matches the requested action, so no rewrite happens and raw bytes stay.
        plan.set_action(0, RebaseAction::Pick).unwrap();
        assert!(!plan.entries()[0].is_modified());
        assert_eq!(
            render_todo(&plan),
            b"pick   aaaaaaaa wrong-but-original\n".to_vec()
        );
    }

    #[test]
    fn set_action_fixup_variants_render_canonically() {
        let mut plan = RebasePlan::new(vec![pick("aaaaaaa", "one"), pick("bbbbbbb", "two")]);
        plan.set_action(1, RebaseAction::Fixup { amend: None })
            .unwrap();
        plan.set_action(
            1,
            RebaseAction::Fixup {
                amend: Some(FixupAmend::UseOwnMessage),
            },
        )
        .unwrap();
        assert_eq!(
            render_todo(&plan),
            b"pick aaaaaaa one\nfixup -C bbbbbbb two\n".to_vec()
        );
        plan.set_action(
            1,
            RebaseAction::Fixup {
                amend: Some(FixupAmend::UseOwnMessageEdit),
            },
        )
        .unwrap();
        assert_eq!(
            render_todo(&plan),
            b"pick aaaaaaa one\nfixup -c bbbbbbb two\n".to_vec()
        );
    }

    #[test]
    fn set_action_rejects_non_commit_entries() {
        let mut plan = RebasePlan::new(vec![
            RebasePlanEntry::parsed(RebaseEntryKind::Blank, b"\n".to_vec()),
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Break),
                b"break\n".to_vec(),
            ),
        ]);
        assert_eq!(
            plan.set_action(0, RebaseAction::Drop),
            Err(RebasePlanEditError::NotACommitEntry { index: 0 })
        );
        assert_eq!(
            plan.set_action(1, RebaseAction::Drop),
            Err(RebasePlanEditError::NotACommitEntry { index: 1 })
        );
        assert_eq!(
            plan.set_action(7, RebaseAction::Drop),
            Err(RebasePlanEditError::IndexOutOfBounds { index: 7, len: 2 })
        );
    }

    #[test]
    fn reword_override_is_model_only_metadata() {
        let mut plan = RebasePlan::new(vec![{
            let mut entry = pick("aaaaaaa", "one");
            plan_helper_make_reword(&mut entry);
            entry
        }]);
        plan.set_reword_message(0, Some(b"replacement message".to_vec()))
            .unwrap();
        match plan.entries()[0].command() {
            Some(RebaseCommand::Reword(target)) => {
                assert_eq!(
                    target.message_override(),
                    Some(b"replacement message".as_slice())
                );
            }
            other => panic!("expected reword, got {other:?}"),
        }
        // The todo grammar cannot express the override: bytes stay original.
        assert!(!plan.entries()[0].is_modified());
        assert_eq!(plan.set_reword_message(0, None), Ok(()));
    }

    fn plan_helper_make_reword(entry: &mut RebasePlanEntry) {
        let kind = match entry.kind() {
            RebaseEntryKind::Command(RebaseCommand::Pick(target)) => {
                RebaseEntryKind::Command(RebaseCommand::Reword(RewordTarget::new(
                    target.oid().clone(),
                    target.subject().to_vec(),
                )))
            }
            _ => unreachable!(),
        };
        *entry = RebasePlanEntry::parsed(kind, entry.raw_bytes().to_vec());
    }

    #[test]
    fn reword_override_survives_action_cycle_back_to_reword() {
        let mut plan = RebasePlan::new(vec![pick("aaaaaaa", "one")]);
        plan.set_action(0, RebaseAction::Reword).unwrap();
        plan.set_reword_message(0, Some(b"msg".to_vec())).unwrap();
        plan.set_action(0, RebaseAction::Edit).unwrap();
        match plan.entries()[0].command() {
            Some(RebaseCommand::Edit(_)) => {}
            other => panic!("expected edit, got {other:?}"),
        }
        // Changing away from reword discards the override.
        plan.set_action(0, RebaseAction::Reword).unwrap();
        match plan.entries()[0].command() {
            Some(RebaseCommand::Reword(target)) => assert_eq!(target.message_override(), None),
            other => panic!("expected reword, got {other:?}"),
        }
        assert_eq!(plan.set_reword_message(0, Some(b"x".to_vec())), Ok(()));
        let mut other = RebasePlan::new(vec![pick("aaaaaaa", "one")]);
        assert_eq!(
            other.set_reword_message(0, None),
            Err(RebasePlanEditError::NotARewordEntry { index: 0 })
        );
    }

    #[test]
    fn validate_rejects_duplicate_written_identically() {
        let plan = RebasePlan::new(vec![pick("aaaaaaa", "one"), pick("AAAAAAA", "two")]);
        assert!(matches!(
            validate_plan(&plan),
            Err(RebaseValidationError::DuplicateCommit {
                first_index: 0,
                second_index: 1,
                ..
            })
        ));
    }

    #[test]
    fn validate_rejects_duplicate_abbreviation_of_full_id() {
        let plan = RebasePlan::new(vec![
            pick("aaaaaaa1b2c3d4e5f60718293a4b5c6d7e8f9a0b", "one"),
            pick("aaaaaaa", "two"),
        ]);
        assert!(matches!(
            validate_plan(&plan),
            Err(RebaseValidationError::DuplicateCommit { .. })
        ));
    }

    #[test]
    fn validate_rejects_leading_squash_and_fixup() {
        let squash_first = RebasePlan::new(vec![{
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Squash(CommitTarget::new(
                    oid("aaaaaaa"),
                    Vec::new(),
                ))),
                b"squash aaaaaaa\n".to_vec(),
            )
        }]);
        assert_eq!(
            validate_plan(&squash_first),
            Err(RebaseValidationError::SquashOrFixupWithoutCommit { index: 0 })
        );

        let mut fixup_after_comment = RebasePlan::new(vec![
            RebasePlanEntry::parsed(RebaseEntryKind::Comment, b"# header\n".to_vec()),
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Fixup(FixupTarget::new(
                    oid("aaaaaaa"),
                    Vec::new(),
                    None,
                ))),
                b"fixup aaaaaaa\n".to_vec(),
            ),
        ]);
        assert_eq!(
            validate_plan(&fixup_after_comment),
            Err(RebaseValidationError::SquashOrFixupWithoutCommit { index: 1 })
        );
        fixup_after_comment.move_entry(1, 0).unwrap();
        assert!(matches!(
            validate_plan(&fixup_after_comment),
            Err(RebaseValidationError::SquashOrFixupWithoutCommit { .. })
        ));
    }

    #[test]
    fn validate_accepts_squash_after_commit_producer() {
        let plan = RebasePlan::new(vec![
            pick("aaaaaaa", "one"),
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Fixup(FixupTarget::new(
                    oid("bbbbbbb"),
                    Vec::new(),
                    None,
                ))),
                b"fixup bbbbbbb\n".to_vec(),
            ),
        ]);
        assert_eq!(validate_plan(&plan), Ok(()));
    }

    #[test]
    fn validate_rejects_undefined_reset_and_merge_labels() {
        let reset_plan = RebasePlan::new(vec![RebasePlanEntry::parsed(
            RebaseEntryKind::Command(RebaseCommand::Reset {
                name: "onto".to_owned(),
            }),
            b"reset onto\n".to_vec(),
        )]);
        assert_eq!(
            validate_plan(&reset_plan),
            Err(RebaseValidationError::UndefinedLabel {
                index: 0,
                label: "onto".to_owned()
            })
        );

        let merge_plan = RebasePlan::new(vec![
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Label {
                    name: "onto".to_owned(),
                }),
                b"label onto\n".to_vec(),
            ),
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Merge(MergeTarget::new(
                    "side".to_owned(),
                    None,
                    Vec::new(),
                ))),
                b"merge side\n".to_vec(),
            ),
        ]);
        assert_eq!(
            validate_plan(&merge_plan),
            Err(RebaseValidationError::UndefinedLabel {
                index: 1,
                label: "side".to_owned()
            })
        );
    }

    #[test]
    fn validate_accepts_rebase_merges_shape() {
        let plan = RebasePlan::new(vec![
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Label {
                    name: "onto".to_owned(),
                }),
                b"label onto\n".to_vec(),
            ),
            RebasePlanEntry::parsed(RebaseEntryKind::Blank, b"\n".to_vec()),
            RebasePlanEntry::parsed(RebaseEntryKind::Comment, b"# Branch side\n".to_vec()),
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Reset {
                    name: "onto".to_owned(),
                }),
                b"reset onto\n".to_vec(),
            ),
            pick("3880725", "# side work"),
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Label {
                    name: "side".to_owned(),
                }),
                b"label side\n".to_vec(),
            ),
            RebasePlanEntry::parsed(
                RebaseEntryKind::Command(RebaseCommand::Merge(MergeTarget::new(
                    "side".to_owned(),
                    Some(MergeTemplate::new(oid("e689e8a"), false)),
                    b"# Merge branch 'side'".to_vec(),
                ))),
                b"merge -C e689e8a side # Merge branch 'side'\n".to_vec(),
            ),
        ]);
        assert_eq!(validate_plan(&plan), Ok(()));
    }

    #[test]
    fn describe_validation_formats_error() {
        let error = RebaseValidationError::SquashOrFixupWithoutCommit { index: 3 };
        assert_eq!(
            describe_validation(&error),
            "entry 3 has no preceding commit to meld into"
        );
    }

    #[test]
    fn progress_accumulator_parses_cr_separated_markers_across_chunks() {
        let mut accumulator = RebaseProgressAccumulator::new();
        assert_eq!(accumulator.push(b"Rebasing (1"), Vec::new());
        assert_eq!(
            accumulator.push(b"/3)\rReb"),
            vec![RebaseStep {
                completed: 1,
                total: 3
            }]
        );
        // Duplicate markers are suppressed; a newline also terminates a marker.
        assert_eq!(
            accumulator.push(b"asing (1/3)\rRebasing (2/3)\nRebasing (3/3)\r"),
            vec![
                RebaseStep {
                    completed: 2,
                    total: 3
                },
                RebaseStep {
                    completed: 3,
                    total: 3
                }
            ]
        );
        assert_eq!(accumulator.finish(), Vec::new());
    }

    #[test]
    fn progress_accumulator_ignores_garbage_and_flushes_the_tail() {
        let mut accumulator = RebaseProgressAccumulator::new();
        assert_eq!(
            accumulator.push(b"hint: use git add\rRebasing (0/0)\rRebasing (4/2)\r"),
            Vec::new(),
            "zero totals and inverted steps are not progress"
        );
        assert_eq!(accumulator.push(b"Successfully rebased"), Vec::new());
        assert_eq!(
            accumulator.push(b" and updated refs/heads/topic.\n"),
            Vec::new()
        );
        let mut tail = RebaseProgressAccumulator::new();
        tail.push(b"Rebasing (5/9)");
        assert_eq!(
            tail.finish(),
            vec![RebaseStep {
                completed: 5,
                total: 9
            }]
        );
    }

    #[test]
    fn progress_accumulator_bounds_unterminated_input() {
        let mut accumulator = RebaseProgressAccumulator::new();
        accumulator.push(&vec![b'x'; MAX_PROGRESS_PENDING_BYTES * 3]);
        assert!(accumulator.pending.len() <= MAX_PROGRESS_PENDING_BYTES);
        assert_eq!(
            accumulator.push(b"\rRebasing (1/1)\r"),
            vec![RebaseStep {
                completed: 1,
                total: 1
            }]
        );
    }

    #[test]
    fn progress_step_maps_to_activity_progress() {
        let progress = rebase_step_progress(RebaseStep {
            completed: 2,
            total: 5,
        })
        .unwrap();
        assert_eq!(progress.phase, "Rebasing");
        assert_eq!(progress.completed, 2);
        assert_eq!(progress.total, Some(5));
        assert_eq!(progress.unit, "commits");
    }
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    /// Captured verbatim from `git version 2.55.0` via
    /// `GIT_SEQUENCE_EDITOR=<capture-and-abort> git rebase -i HEAD~3`.
    const PLAIN_TODO: &[u8] = b"pick 7a86475 # commit 2 subject\n\
pick 9894468 # commit 3 subject\n\
pick 9f574f1 # fixup! commit 1 subject\n\
\n\
# Rebase 704a3f1..9f574f1 onto 704a3f1 (3 commands)\n\
#\n\
# Commands:\n\
# p, pick <commit> = use commit\n\
# r, reword <commit> = use commit, but edit the commit message\n\
# e, edit <commit> = use commit, but stop for amending\n\
# s, squash <commit> = use commit, but meld into previous commit\n\
# f, fixup [-C | -c] <commit> = like \"squash\" but keep only the previous\n\
#                    commit's log message, unless -C is used, in which case\n\
#                    keep only this commit's message; -c is same as -C but\n\
#                    opens the editor\n\
# x, exec <command> = run command (the rest of the line) using shell\n\
# b, break = stop here (continue rebase later with 'git rebase --continue')\n\
# d, drop <commit> = remove commit\n\
# l, label <label> = label current HEAD with a name\n\
# t, reset <label> = reset HEAD to a label\n\
# m, merge [-C <commit> | -c <commit>] <label> [# <oneline>]\n\
#         create a merge commit using the original merge commit's\n\
#         message (or the oneline, if no original merge commit was\n\
#         specified); use -c <commit> to reword the commit message\n\
# u, update-ref <ref> = track a placeholder for the <ref> to be updated\n\
#                       to this position in the new commits. The <ref> is\n\
#                       updated at the end of the rebase\n\
#\n\
# These lines can be re-ordered; they are executed from top to bottom.\n\
#\n\
# If you remove a line here THAT COMMIT WILL BE LOST.\n\
#\n\
# However, if you remove everything, the rebase will be aborted.\n\
#\n";

    /// Captured verbatim from `git version 2.55.0` via
    /// `GIT_SEQUENCE_EDITOR=<capture-and-abort> git rebase -i --rebase-merges HEAD~3`.
    /// The closing header block is identical to `PLAIN_TODO` and truncated here to the first
    /// line; the full header is exercised by the plain fixture.
    const MERGES_TODO_BODY: &[u8] = b"label onto\n\
\n\
# Branch side\n\
reset onto\n\
pick 3880725 # side work\n\
label side\n\
\n\
reset onto\n\
pick fea9219 # main work\n\
merge -C e689e8a side # Merge branch 'side'\n\
pick 69a349c # after merge\n";

    #[test]
    fn empty_file_parses_to_empty_plan() {
        let plan = parse_todo(b"").unwrap();
        assert!(plan.is_empty());
        assert_eq!(render_todo(&plan), b"".to_vec());
    }

    #[test]
    fn authentic_plain_todo_round_trips_byte_identically() {
        let plan = parse_todo(PLAIN_TODO).unwrap();
        assert_eq!(render_todo(&plan), PLAIN_TODO.to_vec());
        assert_eq!(plan.len(), 34);
        assert_eq!(validate_plan(&plan), Ok(()));
        let first = plan.entries()[0].command().unwrap();
        assert_eq!(first.verb(), "pick");
        assert_eq!(first.commit_oid().unwrap().as_written(), "7a86475");
        let RebaseCommand::Pick(target) = first else {
            panic!("expected pick");
        };
        // Git 2.55 writes the oneline as a `#`-prefixed informational comment.
        assert_eq!(target.subject(), b"# commit 2 subject");
    }

    #[test]
    fn authentic_rebase_merges_todo_round_trips_and_validates() {
        let plan = parse_todo(MERGES_TODO_BODY).unwrap();
        assert_eq!(render_todo(&plan), MERGES_TODO_BODY.to_vec());
        assert_eq!(validate_plan(&plan), Ok(()));

        let merge = plan
            .entries()
            .iter()
            .filter_map(RebasePlanEntry::command)
            .find_map(|command| match command {
                RebaseCommand::Merge(target) => Some(target),
                _ => None,
            })
            .unwrap();
        assert_eq!(merge.label(), "side");
        let template = merge.template().unwrap();
        assert!(!template.is_edit());
        assert_eq!(template.commit().as_written(), "e689e8a");
        assert_eq!(merge.rest(), b"# Merge branch 'side'");
    }

    #[test]
    fn abbreviations_parse_to_typed_commands() {
        let todo = b"p aaaaaaa one\nr bbbbbbb two\ne ccccccc three\ns ddddddd four\nf -c eeeeeee five\nx make test\nb\nd fffffff six\nl onto\nt onto\nm -c 1234567 onto # merge subject\nu refs/heads/topic\nnoop\n";
        let plan = parse_todo(todo).unwrap();
        assert_eq!(render_todo(&plan), todo.to_vec());
        let verbs: Vec<&str> = plan
            .entries()
            .iter()
            .filter_map(RebasePlanEntry::command)
            .map(RebaseCommand::verb)
            .collect();
        assert_eq!(
            verbs,
            [
                "pick",
                "reword",
                "edit",
                "squash",
                "fixup",
                "exec",
                "break",
                "drop",
                "label",
                "reset",
                "merge",
                "update-ref",
                "noop"
            ]
        );
        let fixup = match plan.entries()[4].command().unwrap() {
            RebaseCommand::Fixup(target) => target,
            other => panic!("expected fixup, got {other:?}"),
        };
        assert_eq!(fixup.amend(), Some(FixupAmend::UseOwnMessageEdit));
        let merge = match plan.entries()[10].command().unwrap() {
            RebaseCommand::Merge(target) => target,
            other => panic!("expected merge, got {other:?}"),
        };
        assert!(merge.template().unwrap().is_edit());
        assert_eq!(validate_plan(&plan), Ok(()));
    }

    #[test]
    fn crlf_and_missing_trailing_newline_round_trip() {
        let todo = b"pick aaaaaaa one\r\n# comment\r\n\r\npick bbbbbbb two";
        let plan = parse_todo(todo).unwrap();
        assert_eq!(render_todo(&plan), todo.to_vec());
        assert_eq!(plan.len(), 4);
        assert_eq!(plan.entries()[2].kind(), &RebaseEntryKind::Blank);
        assert_eq!(plan.entries()[3].raw_bytes(), b"pick bbbbbbb two");
        assert_eq!(plan.entries()[3].content_bytes(), b"pick bbbbbbb two");
        assert_eq!(plan.entries()[0].content_bytes(), b"pick aaaaaaa one");
    }

    #[test]
    fn truncated_mid_line_file_is_preserved() {
        // A final line cut short without a terminator still parses and round-trips.
        let todo = b"pick aaaaaaa one\npick bbbb";
        let plan = parse_todo(todo).unwrap();
        assert_eq!(render_todo(&plan), todo.to_vec());
        let last = plan.entries()[1].command().unwrap();
        assert_eq!(last.commit_oid().unwrap().as_written(), "bbbb");
        assert_eq!(validate_plan(&plan), Ok(()));
    }

    #[test]
    fn non_utf8_exec_and_comment_bytes_round_trip() {
        let todo = b"pick aaaaaaa one\nexec echo \xff\xfe 'quoted \"cmd\"' && make\n# caf\xe9\n";
        let plan = parse_todo(todo).unwrap();
        assert_eq!(render_todo(&plan), todo.to_vec());
        let exec = match plan.entries()[1].command().unwrap() {
            RebaseCommand::Exec { command } => command,
            other => panic!("expected exec, got {other:?}"),
        };
        assert_eq!(exec, b"echo \xff\xfe 'quoted \"cmd\"' && make");
        assert_eq!(plan.entries()[2].kind(), &RebaseEntryKind::Comment);
    }

    #[test]
    fn unknown_future_verbs_are_preserved_not_rejected() {
        let todo = b"pick aaaaaaa one\nfrobulate --flag aaaaaaa arbitrary rest\n";
        let plan = parse_todo(todo).unwrap();
        assert_eq!(render_todo(&plan), todo.to_vec());
        match plan.entries()[1].command().unwrap() {
            RebaseCommand::Unknown { verb, rest } => {
                assert_eq!(verb, "frobulate");
                assert_eq!(rest, b" --flag aaaaaaa arbitrary rest");
            }
            other => panic!("expected unknown, got {other:?}"),
        }
        // Unknown commands do not block validation.
        assert_eq!(validate_plan(&plan), Ok(()));
    }

    #[test]
    fn duplicate_oids_are_rejected_by_validation_not_parsing() {
        let todo = b"pick aaaaaaa one\npick AAAAAAA two\n";
        let plan = parse_todo(todo).unwrap();
        assert_eq!(render_todo(&plan), todo.to_vec());
        assert!(matches!(
            validate_plan(&plan),
            Err(RebaseValidationError::DuplicateCommit { .. })
        ));
    }

    #[test]
    fn malformed_known_instructions_fail_with_line_numbers() {
        let missing_oid = parse_todo(b"pick aaaaaaa one\nsquash\n").unwrap_err();
        assert_eq!(
            missing_oid,
            RebasePlanError::MalformedLine {
                line: 2,
                reason: RebaseLineError::MissingOid
            }
        );
        let invalid_oid = parse_todo(b"pick notanoid subject\n").unwrap_err();
        assert_eq!(
            invalid_oid,
            RebasePlanError::MalformedLine {
                line: 1,
                reason: RebaseLineError::InvalidOid
            }
        );
        let missing_label = parse_todo(b"merge -C aaaaaaa\n").unwrap_err();
        assert_eq!(
            missing_label,
            RebasePlanError::MalformedLine {
                line: 1,
                reason: RebaseLineError::MissingLabel
            }
        );
        let missing_ref = parse_todo(b"update-ref\n").unwrap_err();
        assert_eq!(
            missing_ref,
            RebasePlanError::MalformedLine {
                line: 1,
                reason: RebaseLineError::MissingReference
            }
        );
        let non_ascii = parse_todo(b"p\xffck aaaaaaa one\n").unwrap_err();
        assert_eq!(
            non_ascii,
            RebasePlanError::MalformedLine {
                line: 1,
                reason: RebaseLineError::NonAsciiCommand
            }
        );
    }

    #[test]
    fn line_and_length_caps_are_enforced() {
        let too_long_line = vec![b'a'; MAX_TODO_LINE_BYTES + 1];
        assert_eq!(
            parse_todo(&too_long_line),
            Err(RebasePlanError::LineTooLong {
                line: 1,
                limit: MAX_TODO_LINE_BYTES
            })
        );

        let mut many = Vec::new();
        for _ in 0..=MAX_TODO_LINES {
            many.extend_from_slice(b"#\n");
        }
        assert_eq!(
            parse_todo(&many),
            Err(RebasePlanError::TooManyLines {
                limit: MAX_TODO_LINES
            })
        );

        let mut at_cap = Vec::new();
        for _ in 0..MAX_TODO_LINES {
            at_cap.extend_from_slice(b"#\n");
        }
        assert_eq!(parse_todo(&at_cap).unwrap().len(), MAX_TODO_LINES);
    }

    #[test]
    fn mutation_renders_canonically_while_untouched_lines_keep_bytes() {
        let mut plan = parse_todo(PLAIN_TODO).unwrap();
        // Move the third pick above the second, then squash it.
        plan.move_entry(2, 1).unwrap();
        plan.set_action(1, RebaseAction::Squash).unwrap();
        let rendered = render_todo(&plan);
        let expected_prefix = b"pick 7a86475 # commit 2 subject\nsquash 9f574f1 # fixup! commit 1 subject\npick 9894468 # commit 3 subject\n";
        assert!(rendered.starts_with(expected_prefix));
        // The untouched header block survives byte-identically.
        let header_start = PLAIN_TODO
            .windows(4)
            .position(|window| window == b"\n# R")
            .unwrap();
        assert_eq!(
            &rendered[expected_prefix.len()..],
            &PLAIN_TODO[header_start..]
        );
        // Moved-but-unmodified entries still carry original bytes.
        assert!(!plan.entries()[2].is_modified());
        assert!(plan.entries()[1].is_modified());
        assert_eq!(validate_plan(&plan), Ok(()));
    }

    #[test]
    fn autosquash_preserves_comments_unknown_instructions_and_raw_line_endings() {
        let source = b"pick 1111111 base\r\n# keep this byte-exact\r\nfuture opaque --flag\r\npick 2222222 fixup! base\r\n";
        let mut plan = parse_todo(source).unwrap();
        assert_eq!(plan.apply_autosquash_preview(), 1);
        let rendered = render_todo(&plan);
        assert!(rendered.starts_with(b"pick 1111111 base\r\nfixup 2222222 fixup! base\n"));
        assert!(rendered
            .windows(b"# keep this byte-exact\r\n".len())
            .any(|window| window == b"# keep this byte-exact\r\n"));
        assert!(rendered
            .windows(b"future opaque --flag\r\n".len())
            .any(|window| window == b"future opaque --flag\r\n"));
    }

    #[test]
    fn reword_and_fixup_flag_changes_render_canonical_lines() {
        let mut plan = parse_todo(b"p aaaaaaa one\nf bbbbbbb two\n").unwrap();
        plan.set_action(0, RebaseAction::Reword).unwrap();
        plan.set_action(
            1,
            RebaseAction::Fixup {
                amend: Some(FixupAmend::UseOwnMessage),
            },
        )
        .unwrap();
        assert_eq!(
            render_todo(&plan),
            b"reword aaaaaaa one\nfixup -C bbbbbbb two\n".to_vec()
        );
    }

    #[test]
    fn comment_lines_with_leading_whitespace_are_comments() {
        let todo = b"   # indented comment\n\t\npick aaaaaaa one\n";
        let plan = parse_todo(todo).unwrap();
        assert_eq!(render_todo(&plan), todo.to_vec());
        assert_eq!(plan.entries()[0].kind(), &RebaseEntryKind::Comment);
        assert_eq!(plan.entries()[1].kind(), &RebaseEntryKind::Blank);
    }

    #[test]
    fn exec_with_no_command_and_full_oids_parse() {
        let full = "a".repeat(40);
        let todo = format!("exec\npick {full} subject\n").into_bytes();
        let plan = parse_todo(&todo).unwrap();
        assert_eq!(render_todo(&plan), todo);
        assert!(plan.entries()[1]
            .command()
            .unwrap()
            .commit_oid()
            .unwrap()
            .is_full_length());
        assert_eq!(
            plan.entries()[0].command().unwrap(),
            &RebaseCommand::Exec {
                command: Vec::new()
            }
        );
    }
}
