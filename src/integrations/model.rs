//! Typed editor, terminal, file-manager, diff, and merge-tool handoff.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::PathBuf;

const MAX_ARGUMENTS: usize = 128;
const MAX_ARGUMENT_BYTES: usize = 16 * 1024;
const MAX_TEMPLATE_PARTS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IntegrationKind {
    Editor,
    Terminal,
    FileManager,
    Browser,
    DiffTool,
    MergeTool,
}

impl IntegrationKind {
    #[must_use]
    pub const fn default_launch_mode(self) -> IntegrationLaunchMode {
        match self {
            Self::DiffTool | Self::MergeTool => IntegrationLaunchMode::Wait,
            Self::Editor | Self::Terminal | Self::FileManager | Self::Browser => {
                IntegrationLaunchMode::Detached
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IntegrationVariable {
    Repository,
    WorkingDirectory,
    Url,
    File,
    CurrentBranch,
    SelectedCommit,
    DiffLeft,
    DiffRight,
    MergeBase,
    MergeCurrent,
    MergeIncoming,
    MergeResult,
}

impl IntegrationVariable {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "repository" => Self::Repository,
            "working_directory" => Self::WorkingDirectory,
            "url" => Self::Url,
            "file" => Self::File,
            "current_branch" => Self::CurrentBranch,
            "selected_commit" => Self::SelectedCommit,
            "diff_left" => Self::DiffLeft,
            "diff_right" => Self::DiffRight,
            "merge_base" => Self::MergeBase,
            "merge_current" => Self::MergeCurrent,
            "merge_incoming" => Self::MergeIncoming,
            "merge_result" => Self::MergeResult,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntegrationVariables {
    values: BTreeMap<IntegrationVariable, OsString>,
}

impl IntegrationVariables {
    #[must_use]
    pub fn with(mut self, variable: IntegrationVariable, value: impl Into<OsString>) -> Self {
        self.values.insert(variable, value.into());
        self
    }

    #[must_use]
    pub fn get(&self, variable: IntegrationVariable) -> Option<&OsStr> {
        self.values.get(&variable).map(OsString::as_os_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgumentTemplatePart {
    Literal(String),
    Variable(IntegrationVariable),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArgumentTemplate {
    parts: Vec<ArgumentTemplatePart>,
}

impl ArgumentTemplate {
    /// Parses one native argument. Braces are escaped as `{{` and `}}`; known placeholders are
    /// substituted without splitting the resulting argument.
    ///
    /// # Errors
    ///
    /// Rejects unknown, unclosed, oversized, or excessively fragmented templates.
    pub fn parse(value: &str) -> Result<Self, IntegrationError> {
        if value.len() > MAX_ARGUMENT_BYTES {
            return Err(IntegrationError::TemplateTooLarge);
        }
        let bytes = value.as_bytes();
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'{' if bytes.get(index + 1) == Some(&b'{') => {
                    literal.push('{');
                    index += 2;
                }
                b'}' if bytes.get(index + 1) == Some(&b'}') => {
                    literal.push('}');
                    index += 2;
                }
                b'{' => {
                    if !literal.is_empty() {
                        parts.push(ArgumentTemplatePart::Literal(std::mem::take(&mut literal)));
                    }
                    let rest = &value[index + 1..];
                    let close = rest.find('}').ok_or(IntegrationError::InvalidTemplate)?;
                    let name = &rest[..close];
                    let variable = IntegrationVariable::parse(name)
                        .ok_or(IntegrationError::UnknownVariable)?;
                    parts.push(ArgumentTemplatePart::Variable(variable));
                    index += close + 2;
                }
                b'}' => return Err(IntegrationError::InvalidTemplate),
                _ => {
                    let character = value[index..]
                        .chars()
                        .next()
                        .ok_or(IntegrationError::InvalidTemplate)?;
                    literal.push(character);
                    index += character.len_utf8();
                }
            }
            if parts.len() > MAX_TEMPLATE_PARTS {
                return Err(IntegrationError::TemplateTooLarge);
            }
        }
        if !literal.is_empty() || parts.is_empty() {
            parts.push(ArgumentTemplatePart::Literal(literal));
        }
        Ok(Self { parts })
    }

    fn variables(&self) -> impl Iterator<Item = IntegrationVariable> + '_ {
        self.parts.iter().filter_map(|part| match part {
            ArgumentTemplatePart::Literal(_) => None,
            ArgumentTemplatePart::Variable(variable) => Some(*variable),
        })
    }

    /// Substitutes variables into one native argument without invoking a shell.
    ///
    /// # Errors
    /// Reports missing variables.
    pub fn expand(&self, values: &IntegrationVariables) -> Result<OsString, IntegrationError> {
        let mut result = OsString::new();
        for part in &self.parts {
            match part {
                ArgumentTemplatePart::Literal(literal) => result.push(literal),
                ArgumentTemplatePart::Variable(variable) => result.push(
                    values
                        .get(*variable)
                        .ok_or(IntegrationError::MissingVariable(*variable))?,
                ),
            }
        }
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrationExecution {
    Direct,
    /// The user explicitly chose a shell executable and its argument vector. The launcher does not
    /// infer `-c`, quote a command string, or select a shell.
    ExplicitShell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrationLaunchMode {
    Detached,
    Wait,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationDefinition {
    pub id: String,
    pub kind: IntegrationKind,
    pub executable: PathBuf,
    pub arguments: Vec<ArgumentTemplate>,
    pub execution: IntegrationExecution,
    pub launch_mode: IntegrationLaunchMode,
}

impl IntegrationDefinition {
    /// Validates one configured or discovered definition.
    ///
    /// # Errors
    ///
    /// Rejects empty identities, empty executable paths, excessive argv, or variables that do not
    /// apply to the selected integration kind.
    pub fn validate(&self) -> Result<(), IntegrationError> {
        if self.id.is_empty() || self.id.len() > 128 || self.executable.as_os_str().is_empty() {
            return Err(IntegrationError::InvalidDefinition);
        }
        if self.arguments.len() > MAX_ARGUMENTS {
            return Err(IntegrationError::TooManyArguments);
        }
        let allowed = allowed_variables(self.kind);
        if self
            .arguments
            .iter()
            .flat_map(ArgumentTemplate::variables)
            .any(|variable| !allowed.contains(&variable))
        {
            return Err(IntegrationError::VariableNotAllowed);
        }
        Ok(())
    }
}

fn allowed_variables(kind: IntegrationKind) -> BTreeSet<IntegrationVariable> {
    use IntegrationVariable::{
        CurrentBranch, DiffLeft, DiffRight, File, MergeBase, MergeCurrent, MergeIncoming,
        MergeResult, Repository, SelectedCommit, Url, WorkingDirectory,
    };
    let common = [Repository, WorkingDirectory, CurrentBranch, SelectedCommit];
    common
        .into_iter()
        .chain(match kind {
            IntegrationKind::Editor | IntegrationKind::FileManager => vec![File],
            IntegrationKind::Terminal => Vec::new(),
            IntegrationKind::Browser => vec![Url],
            IntegrationKind::DiffTool => vec![File, DiffLeft, DiffRight],
            IntegrationKind::MergeTool => {
                vec![File, MergeBase, MergeCurrent, MergeIncoming, MergeResult]
            }
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationRequest {
    pub kind: IntegrationKind,
    pub working_directory: PathBuf,
    pub variables: IntegrationVariables,
    pub configured: Option<IntegrationDefinition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationCommand {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub working_directory: PathBuf,
    pub execution: IntegrationExecution,
    pub launch_mode: IntegrationLaunchMode,
}

impl IntegrationCommand {
    /// Expands one validated definition into a native process command.
    ///
    /// # Errors
    ///
    /// Returns the definition or placeholder error without constructing a partial command.
    pub fn expand(
        definition: &IntegrationDefinition,
        request: &IntegrationRequest,
    ) -> Result<Self, IntegrationError> {
        definition.validate()?;
        if definition.kind != request.kind || request.working_directory.as_os_str().is_empty() {
            return Err(IntegrationError::InvalidDefinition);
        }
        let arguments = definition
            .arguments
            .iter()
            .map(|argument| argument.expand(&request.variables))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            executable: definition.executable.clone(),
            arguments,
            working_directory: request.working_directory.clone(),
            execution: definition.execution,
            launch_mode: definition.launch_mode,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntegrationProcessOutcome {
    Detached { process_id: u32 },
    Exited { code: Option<i32> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationLaunchReceipt {
    pub integration_id: String,
    pub used_fallback: bool,
    pub outcome: IntegrationProcessOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrationLaunchErrorKind {
    WorkingDirectoryUnavailable,
    ExecutableUnavailable,
    PermissionDenied,
    LaunchFailed,
    WaitFailed,
    NonZeroExit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationLaunchError {
    pub kind: IntegrationLaunchErrorKind,
}

impl fmt::Display for IntegrationLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            IntegrationLaunchErrorKind::WorkingDirectoryUnavailable => {
                "the integration working directory is unavailable"
            }
            IntegrationLaunchErrorKind::ExecutableUnavailable => {
                "the integration executable is unavailable"
            }
            IntegrationLaunchErrorKind::PermissionDenied => {
                "the integration executable cannot be started"
            }
            IntegrationLaunchErrorKind::LaunchFailed => "the integration process failed to start",
            IntegrationLaunchErrorKind::WaitFailed => {
                "the integration process could not be observed"
            }
            IntegrationLaunchErrorKind::NonZeroExit => "the integration process reported a failure",
        })
    }
}

impl std::error::Error for IntegrationLaunchError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IntegrationError {
    InvalidDefinition,
    InvalidTemplate,
    UnknownVariable,
    VariableNotAllowed,
    MissingVariable(IntegrationVariable),
    TemplateTooLarge,
    TooManyArguments,
    NoAvailableTool,
    Launch(IntegrationLaunchError),
}

impl fmt::Display for IntegrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingVariable(variable) => {
                write!(formatter, "integration variable {variable:?} is missing")
            }
            Self::Launch(error) => error.fmt(formatter),
            Self::InvalidDefinition => formatter.write_str("integration definition is invalid"),
            Self::InvalidTemplate => {
                formatter.write_str("integration argument template is invalid")
            }
            Self::UnknownVariable => {
                formatter.write_str("integration argument template names an unknown variable")
            }
            Self::VariableNotAllowed => {
                formatter.write_str("integration variable does not apply to this tool")
            }
            Self::TemplateTooLarge => {
                formatter.write_str("integration argument template exceeds its bound")
            }
            Self::TooManyArguments => {
                formatter.write_str("integration argument count exceeds its bound")
            }
            Self::NoAvailableTool => {
                formatter.write_str("no configured or platform integration is available")
            }
        }
    }
}

impl std::error::Error for IntegrationError {}

#[cfg(test)]
mod tests {
    use super::*;
    fn template(value: &str) -> ArgumentTemplate {
        ArgumentTemplate::parse(value).unwrap()
    }
    #[test]
    fn templates_preserve_argument_boundaries_and_escape_braces() {
        let parsed = template("prefix-{{literal}}-{file}");
        let values = IntegrationVariables::default().with(IntegrationVariable::File, "a b.rs");
        assert_eq!(parsed.expand(&values).unwrap(), "prefix-{literal}-a b.rs");
        assert_eq!(
            ArgumentTemplate::parse("{unknown}"),
            Err(IntegrationError::UnknownVariable)
        );
        assert_eq!(
            ArgumentTemplate::parse("{file"),
            Err(IntegrationError::InvalidTemplate)
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_non_utf8_values_survive_direct_expansion() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let values = IntegrationVariables::default().with(
            IntegrationVariable::File,
            OsString::from_vec(b"odd-\xff name.rs".to_vec()),
        );
        let expanded = template("{file}").expand(&values).unwrap();
        assert_eq!(expanded.as_os_str().as_bytes(), b"odd-\xff name.rs");
    }
}
