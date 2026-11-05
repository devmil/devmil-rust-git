//! One command value used for both the preflight preview and execution.

use crate::activity::ActivityCommand;
use crate::process::{GitProcessCommand, ProcessArgument};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::Path;

pub use super::remote::RemoteArgument as CommandArgument;

/// One environment override and whether its value is hidden from activity records.
#[derive(Clone, Eq, PartialEq)]
pub struct CommandEnvironment {
    name: String,
    value: OsString,
    redacted: bool,
}

impl fmt::Debug for CommandEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandEnvironment")
            .field("name", &self.name)
            .field("redacted", &self.redacted)
            .finish_non_exhaustive()
    }
}

impl CommandEnvironment {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn value(&self) -> &OsStr {
        &self.value
    }

    #[must_use]
    pub const fn is_redacted(&self) -> bool {
        self.redacted
    }
}

/// Git arguments (without the executable), required environment and optional standard input.
///
/// Builders in [`crate::commands`] return this value; the caller previews it with
/// [`Self::activity_command`] and executes it with [`Self::process_command`], so preview and
/// execution cannot drift. Standard input may contain user content and is never printed by
/// `Debug`.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct GitInvocation {
    arguments: Vec<CommandArgument>,
    environment: Vec<CommandEnvironment>,
    stdin: Option<Vec<u8>>,
}

impl fmt::Debug for GitInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitInvocation")
            .field("arguments", &self.arguments)
            .field("environment", &self.environment)
            .field("stdin_bytes", &self.stdin.as_ref().map(Vec::len))
            .finish()
    }
}

impl GitInvocation {
    #[must_use]
    pub fn new(arguments: Vec<CommandArgument>) -> Self {
        Self {
            arguments,
            environment: Vec::new(),
            stdin: None,
        }
    }

    /// Builds an invocation whose every argument is visible.
    #[must_use]
    pub fn visible<I, S>(arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self::new(
            arguments
                .into_iter()
                .map(CommandArgument::visible)
                .collect(),
        )
    }

    /// Adds a visible environment override.
    #[must_use]
    pub fn env(mut self, name: &str, value: impl Into<OsString>) -> Self {
        self.environment.push(CommandEnvironment {
            name: name.to_owned(),
            value: value.into(),
            redacted: false,
        });
        self
    }

    /// Adds an environment override whose value is hidden from activity records.
    #[must_use]
    pub fn env_redacted(mut self, name: &str, value: impl Into<OsString>) -> Self {
        self.environment.push(CommandEnvironment {
            name: name.to_owned(),
            value: value.into(),
            redacted: true,
        });
        self
    }

    /// Inserts global options, such as `-c name=value`, before the existing arguments.
    #[must_use]
    pub fn with_leading_arguments(mut self, leading: Vec<CommandArgument>) -> Self {
        self.arguments.splice(0..0, leading);
        self
    }

    #[must_use]
    pub fn stdin_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.stdin = Some(bytes);
        self
    }

    #[must_use]
    pub fn arguments(&self) -> &[CommandArgument] {
        &self.arguments
    }

    /// Process argument values in order.
    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
        self.arguments
            .iter()
            .map(|argument| argument.value().to_owned())
            .collect()
    }

    /// Published argument displays in order (redacted values show their replacement).
    #[must_use]
    pub fn display_arguments(&self) -> Vec<String> {
        self.arguments
            .iter()
            .map(|argument| argument.display().to_owned())
            .collect()
    }

    #[must_use]
    pub fn environment(&self) -> &[CommandEnvironment] {
        &self.environment
    }

    #[must_use]
    pub fn stdin(&self) -> Option<&[u8]> {
        self.stdin.as_deref()
    }

    /// The process command for execution. Output limits, deadlines and output redaction remain
    /// the caller's choice.
    ///
    /// # Panics
    /// Never: builders only add environment names that are valid.
    #[must_use]
    pub fn process_command(
        &self,
        executable: &OsStr,
        working_directory: &Path,
    ) -> GitProcessCommand {
        let mut command = GitProcessCommand::git(executable)
            .current_dir(working_directory)
            .args(self.arguments.iter().map(|argument| {
                if argument.is_redacted() {
                    ProcessArgument::redacted(argument.value(), argument.display())
                } else {
                    ProcessArgument::visible(argument.value())
                }
            }));
        for variable in &self.environment {
            command = if variable.redacted {
                command.env_redacted(&variable.name, &variable.value)
            } else {
                command.env(&variable.name, &variable.value)
            }
            .expect("builder environment names are valid");
        }
        if let Some(stdin) = &self.stdin {
            command = command.stdin_bytes(stdin.clone());
        }
        command
    }

    /// The preview, equal to the activity record of [`Self::process_command`].
    #[must_use]
    pub fn activity_command(
        &self,
        executable: &OsStr,
        working_directory: &Path,
    ) -> ActivityCommand {
        self.process_command(executable, working_directory)
            .activity_command()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_equals_execution_and_debug_hides_stdin() {
        let invocation = GitInvocation::new(vec![
            CommandArgument::visible("commit"),
            CommandArgument::redacted("--author=A <a@example.invalid>", "--author=<author>"),
        ])
        .env("LC_ALL", "C")
        .env_redacted("GIT_INDEX_FILE", "/private/index")
        .stdin_bytes(b"secret message".to_vec());
        let preview = invocation.activity_command(OsStr::new("git"), Path::new("/repo"));
        assert_eq!(
            preview,
            invocation
                .process_command(OsStr::new("git"), Path::new("/repo"))
                .activity_command()
        );
        assert_eq!(preview.arguments[1].display, "--author=<author>");
        assert!(preview.environment[1].redacted);
        let debug = format!("{invocation:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("a@example.invalid"));
        assert!(!debug.contains("/private/index"));
    }
}
