//! Validated branch and tag argv plus shared command previews.
//!
//! Consumers own confirmation, undo, batching limits, and repository fact policies.
//! Git remains authoritative for object existence, collisions, and worktree refusals.

use crate::activity::{ActivityArgument, ActivityCommand, ActivityEnvironment};
use crate::types::{ObjectId, RefId, WorktreeId};

pub mod bisect;
pub mod commit;
pub mod index;
pub mod invocation;
pub mod patch;
pub mod ref_transaction;
pub mod remote;
pub mod restore;
pub mod stash;
pub mod submodule;
pub mod worktree;

/// An exact installed-Git invocation tagged with a consumer-owned operation kind. The preflight preview is
/// derived from this same value, so preview and executed argv are identical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedCommand<Operation> {
    operation: Operation,
    executable: String,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    working_directory: String,
    native_working_directory: std::path::PathBuf,
}

impl<Operation: Copy> PlannedCommand<Operation> {
    pub fn new(repository: &WorktreeId, operation: Operation, arguments: Vec<String>) -> Self {
        Self {
            operation,
            executable: "git".to_owned(),
            native_working_directory: repository.worktree_root().as_path().to_path_buf(),
            arguments,
            // `LC_ALL=C` keeps Git's refusal messages stable for the typed error mapper.
            environment: vec![("LC_ALL".to_owned(), "C".to_owned())],
            working_directory: repository
                .worktree_root()
                .as_path()
                .to_string_lossy()
                .into_owned(),
        }
    }

    pub const fn operation(&self) -> Operation {
        self.operation
    }

    pub fn executable(&self) -> &str {
        &self.executable
    }

    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub fn environment(&self) -> &[(String, String)] {
        &self.environment
    }

    /// Native path for process execution. The string getter is display-only.
    pub fn native_working_directory(&self) -> &std::path::Path {
        &self.native_working_directory
    }

    pub fn working_directory(&self) -> &str {
        &self.working_directory
    }

    /// The redacted display form used for the preflight preview. Every argument is visible:
    /// callers must use this type only for arguments without credential material.
    #[must_use]
    pub fn activity_command(&self) -> ActivityCommand {
        ActivityCommand {
            executable: self.executable.clone(),
            arguments: self
                .arguments
                .iter()
                .map(|argument| ActivityArgument {
                    display: argument.clone(),
                    redacted: false,
                    native_lossless: true,
                })
                .collect(),
            environment: self
                .environment
                .iter()
                .map(|(name, value)| ActivityEnvironment {
                    name: name.clone(),
                    display: value.clone(),
                    redacted: false,
                })
                .collect(),
            working_directory: Some(self.working_directory.clone()),
            stdin_bytes: 0,
        }
    }
}

/// Where a new branch points.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchTarget {
    /// The current `HEAD`, whether attached or detached.
    Head,
    /// An arbitrary commit.
    Commit(ObjectId),
    /// A complete ref name (for example `refs/remotes/origin/main`).
    /// Git resolves it and applies its normal branch tracking configuration.
    Reference(String),
}

impl BranchTarget {
    /// Accepts a full object ID or a complete `refs/` name. Revision expressions,
    /// short ambiguous names and command options are intentionally excluded.
    ///
    /// # Errors
    /// Returns an error for anything other than a full object ID or ref name.
    pub fn parse(value: &str) -> Result<Self, CommandValidationError> {
        if let Ok(object) = ObjectId::parse(value) {
            return Ok(Self::Commit(object));
        }
        RefId::parse(value).map_err(|_| {
            CommandValidationError(
                "starting point is not a valid full ref name or object ID".into(),
            )
        })?;
        Ok(Self::Reference(value.into()))
    }

    pub fn revision(&self) -> &str {
        match self {
            Self::Head => "HEAD",
            Self::Commit(object) => object.as_str(),
            Self::Reference(reference) => reference,
        }
    }
}

impl From<ObjectId> for BranchTarget {
    fn from(object: ObjectId) -> Self {
        Self::Commit(object)
    }
}

/// The legacy branch starting points, retained for existing callers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchStart {
    Head,
    Commit(ObjectId),
}

/// Builds a branch creation command from a full ref or commit target.
/// Remote-tracking refs explicitly establish an upstream regardless of autoSetupMerge.
///
/// # Errors
/// Rejects invalid branch names and invalid full ref targets.
pub fn branch_create_arguments(
    name: &str,
    target: &BranchTarget,
    checkout: bool,
) -> Result<Vec<String>, CommandValidationError> {
    let start = match target {
        BranchTarget::Head | BranchTarget::Reference(_) => BranchStart::Head,
        BranchTarget::Commit(object) => BranchStart::Commit(object.clone()),
    };
    let mut args = branch_arguments(&if checkout {
        BranchRequest::CreateAndCheckout {
            name: name.into(),
            start,
        }
    } else {
        BranchRequest::Create {
            name: name.into(),
            start,
        }
    })?;
    if let BranchTarget::Reference(reference) = target {
        RefId::parse(reference.clone()).map_err(|_| {
            CommandValidationError("starting point is not a valid full ref name".into())
        })?;
        if reference.starts_with("refs/remotes/") {
            args.insert(1, "--track".into());
        }
        args.push(reference.clone());
    }
    Ok(args)
}

/// A branch operation request. [`branch_arguments`] validates its syntax.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchRequest {
    Create {
        name: String,
        start: BranchStart,
    },
    /// Creates the branch and switches to it in one step. Git refuses, and creates nothing,
    /// when local changes overlap the difference between `HEAD` and `start`.
    CreateAndCheckout {
        name: String,
        start: BranchStart,
    },
    Rename {
        from: String,
        to: String,
    },
    Delete {
        name: String,
        force: bool,
    },
    DeleteMany {
        names: Vec<String>,
        force: bool,
    },
    Checkout {
        name: String,
    },
    /// Checks out exactly this commit with detached HEAD, preserving local edits when Git permits.
    CheckoutDetached {
        target: ObjectId,
    },
    SetUpstream {
        branch: String,
        upstream: String,
    },
    UnsetUpstream {
        branch: String,
    },
}

/// Where a new tag points.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagTarget {
    /// The current `HEAD`, whether attached or detached.
    Head,
    /// An arbitrary commit.
    Commit(ObjectId),
}

/// A tag operation request. [`tag_arguments`] validates its syntax.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagRequest {
    /// `message` selects an annotated tag; `None` creates a lightweight tag.
    Create {
        name: String,
        target: TagTarget,
        message: Option<String>,
    },
    Delete {
        name: String,
    },
    Push {
        name: String,
        remote: String,
    },
}

/// Structural validation failure. Messages never include the supplied value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandValidationError(String);

impl std::fmt::Display for CommandValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CommandValidationError {}

/// Validates a visible command label, including a consumer-selected byte bound.
///
/// # Errors
/// Rejects empty, oversized, option-like, or control-character-containing values.
pub fn validate_label(
    value: &str,
    maximum: usize,
    what: &str,
) -> Result<(), CommandValidationError> {
    if value.is_empty()
        || value.len() > maximum
        || value.starts_with('-')
        || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
    {
        return Err(CommandValidationError(format!(
            "{what} is empty, oversized, or starts with '-'"
        )));
    }
    Ok(())
}

/// Parses a short branch name or a full heads reference.
///
/// # Errors
/// Rejects names outside refs/heads, invalid Git reference syntax, or the byte bound.
pub fn parse_branch_name(
    name: &str,
    maximum: usize,
) -> Result<(RefId, String), CommandValidationError> {
    parse_name(name, "refs/heads/", maximum, "branch")
}

/// Parses a short tag name or a full tags reference.
///
/// # Errors
/// Rejects names outside refs/tags, invalid Git reference syntax, or the byte bound.
pub fn parse_tag_name(
    name: &str,
    maximum: usize,
) -> Result<(RefId, String), CommandValidationError> {
    parse_name(name, "refs/tags/", maximum, "tag")
}

fn parse_name(
    name: &str,
    prefix: &str,
    maximum: usize,
    kind: &str,
) -> Result<(RefId, String), CommandValidationError> {
    if name.starts_with("refs/") && !name.starts_with(prefix) {
        return Err(CommandValidationError(format!(
            "{kind} name is a ref outside {prefix}"
        )));
    }
    let short = name.strip_prefix(prefix).unwrap_or(name);
    validate_label(short, maximum, &format!("{kind} name"))?;
    let reference = RefId::parse(format!("{prefix}{short}"))
        .map_err(|_| CommandValidationError(format!("{kind} name is not a valid Git ref name")))?;
    Ok((reference, short.to_owned()))
}

/// Builds Git argv for branch creation, rename, deletion, checkout, or upstream configuration.
///
/// Names are normalized from full heads references to short names. This validates
/// syntax only; callers select length/batch limits and check current repository facts.
///
/// # Errors
/// Rejects invalid names, unsafe labels, an empty deletion batch, or duplicate branches.
pub fn branch_arguments(request: &BranchRequest) -> Result<Vec<String>, CommandValidationError> {
    let short = |name: &str| parse_branch_name(name, usize::MAX).map(|(_, short)| short);
    Ok(match request {
        BranchRequest::Create { name, start } => {
            let mut args = vec!["branch".into(), short(name)?];
            if let BranchStart::Commit(target) = start {
                args.push(target.as_str().into());
            }
            args
        }
        BranchRequest::CreateAndCheckout { name, start } => {
            let mut args = vec!["checkout".into(), "-b".into(), short(name)?];
            if let BranchStart::Commit(target) = start {
                args.push(target.as_str().into());
            }
            args
        }
        BranchRequest::Rename { from, to } => {
            vec!["branch".into(), "-m".into(), short(from)?, short(to)?]
        }
        BranchRequest::Delete { name, force } => vec![
            "branch".into(),
            if *force { "-D" } else { "-d" }.into(),
            short(name)?,
        ],
        BranchRequest::DeleteMany { names, force } => {
            if names.is_empty() {
                return Err(CommandValidationError(
                    "branch deletion requires a name".into(),
                ));
            }
            let mut selected = Vec::with_capacity(names.len());
            for name in names {
                let name = short(name)?;
                if selected.contains(&name) {
                    return Err(CommandValidationError(
                        "the branch deletion selection contains a duplicate".into(),
                    ));
                }
                selected.push(name);
            }
            let mut args = vec!["branch".into(), if *force { "-D" } else { "-d" }.into()];
            args.extend(selected);
            args
        }
        BranchRequest::Checkout { name } => vec!["checkout".into(), short(name)?],
        BranchRequest::CheckoutDetached { target } => {
            vec!["checkout".into(), "--detach".into(), target.as_str().into()]
        }
        BranchRequest::SetUpstream { branch, upstream } => {
            validate_label(upstream, usize::MAX, "upstream")?;
            vec![
                "branch".into(),
                format!("--set-upstream-to={upstream}"),
                short(branch)?,
            ]
        }
        BranchRequest::UnsetUpstream { branch } => {
            vec!["branch".into(), "--unset-upstream".into(), short(branch)?]
        }
    })
}

/// Builds Git argv for lightweight/annotated tag creation, deletion, or push.
///
/// Push takes a configured remote name. Messages are passed as a single argument
/// and cannot contain NUL. Consumers select their own name and message limits.
///
/// # Errors
/// Rejects invalid names, unsafe remote labels, or empty/NUL-containing messages.
pub fn tag_arguments(request: &TagRequest) -> Result<Vec<String>, CommandValidationError> {
    Ok(match request {
        TagRequest::Create {
            name,
            target,
            message,
        } => {
            let (_, short) = parse_tag_name(name, usize::MAX)?;
            let mut args = vec!["tag".into()];
            if let Some(message) = message {
                if message.is_empty() || message.contains('\0') {
                    return Err(CommandValidationError(
                        "tag message is empty or contains NUL".into(),
                    ));
                }
                args.extend(["-a".into(), "-m".into(), message.clone()]);
            }
            args.push(short);
            if let TagTarget::Commit(target) = target {
                args.push(target.as_str().into());
            }
            args
        }
        TagRequest::Delete { name } => vec![
            "tag".into(),
            "-d".into(),
            parse_tag_name(name, usize::MAX)?.1,
        ],
        TagRequest::Push { name, remote } => {
            let (_, short) = parse_tag_name(name, usize::MAX)?;
            validate_label(remote, usize::MAX, "remote name")?;
            vec!["push".into(), remote.clone(), format!("refs/tags/{short}")]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_checkout_uses_an_exact_object_without_force() {
        let target = ObjectId::parse("a".repeat(40)).unwrap();
        assert_eq!(
            branch_arguments(&BranchRequest::CheckoutDetached {
                target: target.clone()
            })
            .unwrap(),
            ["checkout", "--detach", target.as_str()]
        );
    }

    #[test]
    fn branch_ref_targets_preserve_tracking_and_reject_unsafe_revisions() {
        let reference = "refs/remotes/origin/release/2026.11_global";
        let start = BranchTarget::parse(reference).unwrap();
        assert_eq!(
            branch_create_arguments("release/2026.11_global", &start, true).unwrap(),
            [
                "checkout",
                "--track",
                "-b",
                "release/2026.11_global",
                reference
            ]
        );
        assert_eq!(
            branch_create_arguments("release/2026.11_global", &start, false).unwrap(),
            ["branch", "--track", "release/2026.11_global", reference]
        );
        for invalid in [
            "--orphan",
            "origin/main",
            "refs/heads/main~1",
            "refs/heads/a..b",
            "refs/heads/a\n",
        ] {
            assert!(BranchTarget::parse(invalid).is_err());
            assert!(branch_create_arguments(
                "topic",
                &BranchTarget::Reference(invalid.into()),
                true
            )
            .is_err());
        }
        assert!(BranchTarget::parse(&"a".repeat(40)).is_ok());
        assert!(BranchTarget::parse("refs/heads/main").is_ok());
        assert!(BranchTarget::parse("refs/tags/v1").is_ok());
    }

    #[test]
    fn branches_normalize_names_and_keep_targets_out_of_options() {
        let target = ObjectId::parse("a".repeat(40)).unwrap();
        assert_eq!(
            branch_arguments(&BranchRequest::Create {
                name: "refs/heads/topic".into(),
                start: BranchStart::Commit(target.clone()),
            })
            .unwrap(),
            ["branch", "topic", target.as_str()]
        );
        assert_eq!(
            branch_arguments(&BranchRequest::CreateAndCheckout {
                name: "refs/heads/topic".into(),
                start: BranchStart::Commit(target.clone()),
            })
            .unwrap(),
            ["checkout", "-b", "topic", target.as_str()]
        );
        assert_eq!(
            branch_arguments(&BranchRequest::CreateAndCheckout {
                name: "topic".into(),
                start: BranchStart::Head,
            })
            .unwrap(),
            ["checkout", "-b", "topic"]
        );
        assert!(branch_arguments(&BranchRequest::CreateAndCheckout {
            name: "-b".into(),
            start: BranchStart::Head,
        })
        .is_err());
        assert_eq!(
            branch_arguments(&BranchRequest::Rename {
                from: "refs/heads/old".into(),
                to: "new".into()
            })
            .unwrap(),
            ["branch", "-m", "old", "new"]
        );
        assert_eq!(
            branch_arguments(&BranchRequest::DeleteMany {
                names: vec!["one".into(), "refs/heads/two".into()],
                force: true
            })
            .unwrap(),
            ["branch", "-D", "one", "two"]
        );
        assert_eq!(
            branch_arguments(&BranchRequest::SetUpstream {
                branch: "topic".into(),
                upstream: "origin/topic".into()
            })
            .unwrap(),
            ["branch", "--set-upstream-to=origin/topic", "topic"]
        );
        assert_eq!(
            branch_arguments(&BranchRequest::UnsetUpstream {
                branch: "refs/heads/topic".into(),
            })
            .unwrap(),
            ["branch", "--unset-upstream", "topic"]
        );
        assert!(branch_arguments(&BranchRequest::UnsetUpstream {
            branch: "-topic".into(),
        })
        .is_err());
        assert!(branch_arguments(&BranchRequest::DeleteMany {
            names: vec!["one".into(), "refs/heads/one".into()],
            force: false
        })
        .is_err());
    }

    #[test]
    fn tag_messages_are_single_arguments_and_push_uses_full_ref() {
        let message = "release\nwith 'quotes' and $(literal)";
        assert_eq!(
            tag_arguments(&TagRequest::Create {
                name: "refs/tags/v1".into(),
                target: TagTarget::Head,
                message: Some(message.into())
            })
            .unwrap(),
            ["tag", "-a", "-m", message, "v1"]
        );
        assert_eq!(
            tag_arguments(&TagRequest::Push {
                name: "v1".into(),
                remote: "origin".into()
            })
            .unwrap(),
            ["push", "origin", "refs/tags/v1"]
        );
        assert!(tag_arguments(&TagRequest::Create {
            name: "v1".into(),
            target: TagTarget::Head,
            message: Some("bad\0message".into())
        })
        .is_err());
    }

    #[test]
    fn invalid_names_and_option_injection_are_rejected() {
        for name in ["", "-f", "bad..name", "refs/tags/v1", "line\nbreak"] {
            assert!(parse_branch_name(name, 100).is_err());
        }
        assert!(parse_tag_name("refs/heads/main", 100).is_err());
        assert!(parse_tag_name("oversized", 3).is_err());
        assert!(tag_arguments(&TagRequest::Push {
            name: "v1".into(),
            remote: "--all".into()
        })
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn command_keeps_native_directory_separate_from_preview() {
        use crate::types::{RepositoryId, RepositoryPath};
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let root =
            RepositoryPath::new(std::ffi::OsString::from_vec(b"/repo-\xff".to_vec())).unwrap();
        let git_dir = RepositoryPath::new(root.as_path().join(".git")).unwrap();
        let worktree = WorktreeId::new(RepositoryId::new(git_dir.clone()), root, git_dir);
        let args = branch_arguments(&BranchRequest::Checkout {
            name: "main".into(),
        })
        .unwrap();
        let command = PlannedCommand::new(&worktree, (), args.clone());
        assert_eq!(
            command.native_working_directory().as_os_str().as_bytes(),
            b"/repo-\xff"
        );
        assert_eq!(command.arguments(), args);
        assert_eq!(
            command
                .activity_command()
                .arguments
                .iter()
                .map(|argument| argument.display.clone())
                .collect::<Vec<_>>(),
            args
        );
        assert!(command.working_directory().contains('\u{fffd}'));
    }
}
