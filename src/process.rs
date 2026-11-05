//! Cancelable installed-Git process adapter and platform process-tree ownership.

use crate::activity::{
    ActivityArgument, ActivityCommand, ActivityEnvironment, ActivityEvent, ActivityEventSink,
    ActivityId, ActivityProgress, ActivityRecord, ActivityStatus, ActivityStream,
    BoundedActivityBytes,
};
use crate::cancellation::Cancellation;
use crate::error::{ProcessError, ProcessErrorKind};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroize;

/// Git global options precede the subcommand, and some consume a value.
/// Never search arbitrary operands: a path or branch may itself be named "push".
pub fn git_subcommand(arguments: &[String]) -> Option<&str> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if matches!(
            argument.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
        ) {
            arguments.get(index + 1)?;
            index += 2;
        } else if argument.starts_with('-') {
            index += 1;
        } else {
            return Some(argument);
        }
    }
    None
}

const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_TERMINATION_GRACE: Duration = Duration::from_millis(250);
/// Allow slow disks, hooks, signing, and remote negotiation before treating silence as a hang.
const DEFAULT_GIT_OUTPUT_IDLE_TIMEOUT: Duration = Duration::from_mins(5);
pub const ACTIVITY_STREAM_QUEUE_CAPACITY: usize = 8;

/// Variables that can redirect an otherwise explicit Git command to another repository or alter
/// revision/path interpretation. Commands inherit ordinary host state such as `PATH`, `HOME`,
/// locale, certificate configuration, and credential-helper configuration, but repository
/// identity always comes from the command's explicit working directory and arguments.
const REPOSITORY_SHAPING_GIT_ENVIRONMENT: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_SHALLOW_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_PREFIX",
    "GIT_SUPER_PREFIX",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
];

/// One process argument and the value safe to publish in the activity log.
#[derive(Clone, Eq, PartialEq)]
pub struct ProcessArgument {
    value: OsString,
    activity: ActivityArgument,
}

impl fmt::Debug for ProcessArgument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessArgument")
            .field("activity", &self.activity.display)
            .finish_non_exhaustive()
    }
}

impl ProcessArgument {
    /// Records the argument verbatim in process activity.
    pub fn visible(value: impl Into<OsString>) -> Self {
        let value = value.into();
        let activity = sanitized_argument(&value, false, None);
        Self { value, activity }
    }

    /// Passes the argument to the process but publishes only `replacement`.
    pub fn redacted(value: impl Into<OsString>, replacement: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            activity: ActivityArgument {
                display: replacement.into(),
                redacted: true,
                native_lossless: true,
            },
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
struct ProcessEnvironment {
    name: OsString,
    value: OsString,
    activity: ActivityEnvironment,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SensitiveOutputPolicy {
    #[default]
    Redact,
    SanitizeKnownSecrets,
}

impl fmt::Debug for ProcessEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessEnvironment")
            .field("name", &self.activity.name)
            .field("display", &self.activity.display)
            .field("redacted", &self.activity.redacted)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticTimeout {
    pub after: Duration,
    pub semantic: String,
}

impl SemanticTimeout {
    /// # Errors
    ///
    /// A semantic timeout needs a nonzero duration and a short, nonempty reason.
    pub fn new(after: Duration, semantic: impl Into<String>) -> Result<Self, ProcessSpecError> {
        let semantic = semantic.into();
        if after.is_zero() || semantic.is_empty() || semantic.len() > 128 {
            return Err(ProcessSpecError::InvalidTimeout);
        }
        Ok(Self { after, semantic })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSpecError {
    InvalidEnvironmentName,
    InvalidTimeout,
}

impl fmt::Display for ProcessSpecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidEnvironmentName => "process environment name is empty or contains '='",
            Self::InvalidTimeout => "process timeout needs a duration and bounded semantic reason",
        })
    }
}

impl std::error::Error for ProcessSpecError {}

/// The exact executable and redacted argument vector shown in process activity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessActivity {
    pub executable: String,
    pub arguments: Vec<String>,
}

/// A command specification that never passes arguments through a shell.
#[derive(Clone)]
pub struct GitProcessCommand {
    executable: OsString,
    arguments: Vec<ProcessArgument>,
    current_dir: Option<PathBuf>,
    environment: Vec<ProcessEnvironment>,
    stdin: Vec<u8>,
    timeout: Option<SemanticTimeout>,
    idle_timeout: Option<SemanticTimeout>,
    parse_git_progress: bool,
    redact_stdout: bool,
    redact_stderr: bool,
    sensitive_output_policy: SensitiveOutputPolicy,
    output_limit: usize,
    poll_interval: Duration,
    termination_grace: Duration,
    trace_waits: Option<bool>,
}

impl fmt::Debug for GitProcessCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitProcessCommand")
            .field("activity", &self.activity())
            .field("output_limit", &self.output_limit)
            .field("poll_interval", &self.poll_interval)
            .field("termination_grace", &self.termination_grace)
            .finish_non_exhaustive()
    }
}

impl GitProcessCommand {
    /// Installed-Git commands default to no total duration limit. Five minutes without bytes on
    /// either output stream terminates and reaps their process tree, including hung helpers.
    /// Progress continues to reset this deadline after retained output reaches its bound.
    #[must_use]
    pub fn git(executable: impl Into<OsString>) -> Self {
        Self::new(executable).output_idle_timeout(SemanticTimeout {
            after: DEFAULT_GIT_OUTPUT_IDLE_TIMEOUT,
            semantic: "Git produced no output for 5 minutes; its process tree was terminated"
                .into(),
        })
    }

    pub fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
            arguments: Vec::new(),
            current_dir: None,
            environment: Vec::new(),
            stdin: Vec::new(),
            timeout: None,
            idle_timeout: None,
            parse_git_progress: false,
            redact_stdout: false,
            redact_stderr: false,
            sensitive_output_policy: SensitiveOutputPolicy::Redact,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            termination_grace: DEFAULT_TERMINATION_GRACE,
            trace_waits: None,
        }
    }

    #[must_use]
    /// Enables process wait timing on stderr. No command arguments or output are included.
    pub fn trace_waits(mut self, enabled: bool) -> Self {
        self.trace_waits = Some(enabled);
        self
    }

    #[must_use]
    pub fn arg(mut self, argument: ProcessArgument) -> Self {
        self.arguments.push(argument);
        self
    }

    #[must_use]
    pub fn args(mut self, arguments: impl IntoIterator<Item = ProcessArgument>) -> Self {
        self.arguments.extend(arguments);
        self
    }

    #[must_use]
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    /// Adds one explicit environment override. Credential-shaped names are always redacted in
    /// activity, even when the caller does not mark them sensitive.
    ///
    /// # Errors
    ///
    /// Environment names must be nonempty and cannot contain `=`.
    pub fn env(
        mut self,
        name: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Result<Self, ProcessSpecError> {
        let name = name.into();
        let value = value.into();
        if name.is_empty() || native_display(&name).0.contains('=') {
            return Err(ProcessSpecError::InvalidEnvironmentName);
        }
        let display_name = native_display(&name).0;
        let redacted = environment_name_is_sensitive(&display_name);
        let sanitized = sanitized_argument(&value, false, None);
        self.environment.push(ProcessEnvironment {
            name,
            value,
            activity: ActivityEnvironment {
                name: display_name,
                display: if redacted {
                    "[REDACTED]".into()
                } else {
                    sanitized.display
                },
                redacted: redacted || sanitized.redacted,
            },
        });
        Ok(self)
    }

    /// Adds an environment override whose value is always hidden from activity records, even
    /// when its name is not credential-shaped.
    ///
    /// # Errors
    ///
    /// Environment names must be nonempty and cannot contain `=`.
    pub fn env_redacted(
        mut self,
        name: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Result<Self, ProcessSpecError> {
        let name = name.into();
        if name.is_empty() || native_display(&name).0.contains('=') {
            return Err(ProcessSpecError::InvalidEnvironmentName);
        }
        self.environment.push(ProcessEnvironment {
            activity: ActivityEnvironment {
                name: native_display(&name).0,
                display: "[REDACTED]".into(),
                redacted: true,
            },
            name,
            value: value.into(),
        });
        Ok(self)
    }

    #[must_use]
    pub fn stdin_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = bytes.into();
        self
    }

    #[must_use]
    pub fn semantic_timeout(mut self, timeout: SemanticTimeout) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Bounds silence on both output streams without limiting a progressing transfer.
    #[must_use]
    pub fn output_idle_timeout(mut self, timeout: SemanticTimeout) -> Self {
        self.idle_timeout = Some(timeout);
        self
    }

    #[must_use]
    pub fn parse_git_progress(mut self, enabled: bool) -> Self {
        self.parse_git_progress = enabled;
        self
    }

    /// Keeps raw stdout available only to the in-crate parser while activity receives one
    /// constant redaction marker. Use this for commands whose output can itself reveal secrets.
    #[must_use]
    pub fn redact_stdout(mut self) -> Self {
        self.redact_stdout = true;
        self
    }

    /// Keeps both raw output streams private and publishes only constant redaction markers.
    #[must_use]
    pub fn redact_output(mut self) -> Self {
        self.redact_stdout = true;
        self.redact_stderr = true;
        self
    }

    /// Publishes output after replacing exact redacted argument and environment values. Use only
    /// for commands whose private inputs are transport metadata and whose diagnostics must remain
    /// actionable; explicit stream redaction still wins.
    #[must_use]
    pub fn publish_sanitized_output(mut self) -> Self {
        self.sensitive_output_policy = SensitiveOutputPolicy::SanitizeKnownSecrets;
        self
    }

    /// Sets the maximum retained bytes for each output stream. Readers continue draining after
    /// this limit so a noisy child cannot block on a full pipe.
    #[must_use]
    pub fn output_limit(mut self, bytes: usize) -> Self {
        self.output_limit = bytes;
        self
    }

    pub fn activity(&self) -> ProcessActivity {
        ProcessActivity {
            executable: self.executable.to_string_lossy().into_owned(),
            arguments: sanitize_argument_vector(&self.arguments)
                .into_iter()
                .map(|argument| argument.display)
                .collect(),
        }
    }

    #[must_use]
    pub fn activity_command(&self) -> ActivityCommand {
        ActivityCommand {
            executable: native_display(&self.executable).0,
            arguments: sanitize_argument_vector(&self.arguments),
            environment: self
                .environment
                .iter()
                .map(|item| item.activity.clone())
                .collect(),
            working_directory: self
                .current_dir
                .as_ref()
                .map(|path| native_display(path.as_os_str()).0),
            stdin_bytes: self.stdin.len(),
        }
    }

    #[cfg(test)]
    fn timing(mut self, poll_interval: Duration, termination_grace: Duration) -> Self {
        self.poll_interval = poll_interval;
        self.termination_grace = termination_grace;
        self
    }
}

/// Captured bytes from one process stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessBytes {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// The observed installed-Git process result.
#[derive(Debug)]
pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: ProcessBytes,
    pub stderr: ProcessBytes,
    pub elapsed: Duration,
    pub activity: ProcessActivity,
}

/// Cancellation is separate from an ordinary Git exit, including a nonzero exit code.
#[derive(Debug)]
pub enum ProcessOutcome {
    Exited(ProcessOutput),
    Cancelled(ProcessOutput),
}

impl ProcessOutcome {
    pub const fn output(&self) -> &ProcessOutput {
        match self {
            Self::Exited(output) | Self::Cancelled(output) => output,
        }
    }

    pub const fn was_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }
}

/// Production process runner. It creates the activity before attempting to spawn the child and
/// returns the final durable record for every outcome, including launch errors.
#[derive(Clone, Copy, Debug, Default)]
pub struct ActivityGitRunner;

/// A sanitized activity record and bounded **unredacted** parser output.
/// Never publish `stdout` or `stderr` to logs or UI without application-specific sanitization.
pub struct PrivateActivityResult {
    pub activity: ActivityRecord,
    pub stdout: PrivateActivityBytes,
    pub stderr: PrivateActivityBytes,
}

/// Receives **unredacted** bounded stdout for parsing; never forward these bytes to a log.
pub type PrivateStdoutObserver<'a> = Option<&'a mut dyn FnMut(&[u8])>;
/// Receives **unredacted** bounded stderr for parsing; never forward these bytes to a log.
pub type PrivateStderrObserver<'a> = Option<&'a mut dyn FnMut(&[u8])>;

/// Bounded **unredacted** output, zeroized when dropped. Extracted bytes become caller-owned.
pub struct PrivateActivityBytes(BoundedActivityBytes);

impl PrivateActivityBytes {
    fn new(bytes: BoundedActivityBytes) -> Self {
        Self(bytes)
    }

    pub fn take_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0.bytes)
    }
}

impl Deref for PrivateActivityBytes {
    type Target = BoundedActivityBytes;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for PrivateActivityBytes {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for PrivateActivityBytes {
    fn drop(&mut self) {
        self.0.bytes.zeroize();
    }
}

impl ActivityGitRunner {
    pub fn run(
        specification: GitProcessCommand,
        id: ActivityId,
        cancellation: Option<&dyn Cancellation>,
        sink: &mut dyn ActivityEventSink,
    ) -> ActivityRecord {
        Self::run_private(specification, id, cancellation, sink).activity
    }

    /// Runs a command and returns bounded **unredacted** output for trusted parsers.
    /// Public activity events remain sanitized. Do not log the private output.
    pub fn run_private(
        specification: GitProcessCommand,
        id: ActivityId,
        cancellation: Option<&dyn Cancellation>,
        sink: &mut dyn ActivityEventSink,
    ) -> PrivateActivityResult {
        Self::run_private_inner(specification, id, cancellation, sink, None, None)
    }

    /// Runs a command and sends bounded **unredacted** stdout to a trusted parser.
    /// The observer is not an activity sink and receives no bytes beyond `output_limit`.
    /// Never publish observer bytes directly to logs or UI.
    pub fn run_private_observing_stdout(
        specification: GitProcessCommand,
        id: ActivityId,
        cancellation: Option<&dyn Cancellation>,
        sink: &mut dyn ActivityEventSink,
        observer: &mut dyn FnMut(&[u8]),
    ) -> PrivateActivityResult {
        Self::run_private_inner(specification, id, cancellation, sink, Some(observer), None)
    }

    #[allow(clippy::too_many_lines)]
    fn run_private_inner(
        specification: GitProcessCommand,
        id: ActivityId,
        cancellation: Option<&dyn Cancellation>,
        sink: &mut dyn ActivityEventSink,
        mut stdout_observer: PrivateStdoutObserver<'_>,
        mut stderr_observer: PrivateStderrObserver<'_>,
    ) -> PrivateActivityResult {
        let activity_command = specification.activity_command();
        let command_contains_redaction = activity_command
            .arguments
            .iter()
            .any(|argument| argument.redacted)
            || activity_command
                .environment
                .iter()
                .any(|variable| variable.redacted);
        let secrets = known_secrets(&specification);
        let mut output_redactors = OutputRedactors::new(secrets);
        let implicit_redaction = command_contains_redaction
            && specification.sensitive_output_policy == SensitiveOutputPolicy::Redact;
        let redact_stdout = specification.redact_stdout || implicit_redaction;
        let redact_stderr = specification.redact_stderr || implicit_redaction;
        let mut private_stdout = BoundedActivityBytes::empty();
        let mut private_stderr = BoundedActivityBytes::empty();
        let mut record = ActivityRecord::started(id, activity_command, unix_millis());
        record.set_output_redaction(redact_stdout, redact_stderr);
        sink.publish(ActivityEvent::Created(record.clone()));

        let GitProcessCommand {
            executable,
            arguments,
            current_dir,
            environment,
            stdin,
            timeout,
            idle_timeout,
            parse_git_progress,
            redact_stdout: _,
            redact_stderr: _,
            sensitive_output_policy: _,
            output_limit,
            poll_interval,
            termination_grace,
            trace_waits,
        } = specification;
        let trace_waits = trace_waits.unwrap_or_else(|| {
            std::env::var_os("DEVMIL_GIT_PERF_TRACE").as_deref() == Some(OsStr::new("1"))
        });
        let mut idle_sleep = Duration::ZERO;
        let mut idle_sleeps = 0_u64;
        let mut command = Command::new(executable);
        remove_repository_shaping_git_environment(&mut command);
        command
            .args(arguments.iter().map(|argument| argument.value.as_os_str()))
            .stdin(if stdin.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for item in environment {
            command.env(item.name, item.value);
        }
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }
        platform::prepare(&mut command);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                record.finish(
                    ActivityStatus::LaunchError {
                        message: bounded_activity_detail(format!(
                            "could not start {}: {error}",
                            record.command.executable
                        )),
                    },
                    unix_millis(),
                );
                sink.publish(ActivityEvent::Finished(record.clone()));
                return PrivateActivityResult {
                    activity: record,
                    stdout: PrivateActivityBytes::new(private_stdout),
                    stderr: PrivateActivityBytes::new(private_stderr),
                };
            }
        };

        let (stream_tx, stream_rx) = mpsc::sync_channel(ACTIVITY_STREAM_QUEUE_CAPACITY);
        let reader_wake = Arc::new(ActivityReaderWake {
            runner: thread::current(),
            finished: AtomicUsize::new(0),
        });
        let Some(stdout) = child.stdout.take() else {
            return private_broken_spawn(record, child, sink, "started process has no stdout pipe");
        };
        let Some(stderr) = child.stderr.take() else {
            return private_broken_spawn(record, child, sink, "started process has no stderr pipe");
        };
        let stdout_reader = match spawn_activity_reader(
            "devmil-git-activity-stdout",
            stdout,
            ActivityStream::Stdout,
            stream_tx.clone(),
            reader_wake.clone(),
        ) {
            Ok(reader) => reader,
            Err(error) => {
                return private_broken_spawn(
                    record,
                    child,
                    sink,
                    &format!("could not start stdout reader: {error}"),
                );
            }
        };
        let stderr_reader = match spawn_activity_reader(
            "devmil-git-activity-stderr",
            stderr,
            ActivityStream::Stderr,
            stream_tx,
            reader_wake.clone(),
        ) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = cancel_child_tree(&mut child, poll_interval, termination_grace);
                let _ = stdout_reader.join();
                record.finish(
                    ActivityStatus::LaunchError {
                        message: bounded_activity_detail(format!(
                            "could not start stderr reader: {error}"
                        )),
                    },
                    unix_millis(),
                );
                sink.publish(ActivityEvent::Finished(record.clone()));
                return PrivateActivityResult {
                    activity: record,
                    stdout: PrivateActivityBytes::new(private_stdout),
                    stderr: PrivateActivityBytes::new(private_stderr),
                };
            }
        };
        let stdin_writer = child
            .stdin
            .take()
            .map(|pipe| spawn_stdin_writer(pipe, stdin));

        let started = Instant::now();
        let mut last_output = started;
        let mut progress = ProgressAccumulator::default();
        let mut exit_poll_interval = Duration::from_millis(1);
        let completion = loop {
            let received_output = drain_activity_streams(
                &stream_rx,
                ActivityDrainState {
                    record: &mut record,
                    sink,
                    progress: parse_git_progress.then_some(&mut progress),
                    redactors: &mut output_redactors,
                    private_stdout: &mut private_stdout,
                    private_stderr: &mut private_stderr,
                },
                &mut stdout_observer,
                &mut stderr_observer,
                output_limit,
                redact_stdout,
                redact_stderr,
            );
            if received_output {
                last_output = Instant::now();
            }
            if cancellation.is_some_and(Cancellation::is_cancelled) {
                let exit_code = cancel_child_tree(&mut child, poll_interval, termination_grace)
                    .ok()
                    .and_then(|status| status.code());
                break ActivityStatus::Canceled { exit_code };
            }
            let expired = [
                (timeout.as_ref(), started.elapsed()),
                (idle_timeout.as_ref(), last_output.elapsed()),
            ]
            .into_iter()
            .find_map(|(timeout, elapsed)| timeout.filter(|timeout| elapsed >= timeout.after));
            if let Some(timeout) = expired {
                let exit_code = cancel_child_tree(&mut child, poll_interval, termination_grace)
                    .ok()
                    .and_then(|status| status.code());
                break ActivityStatus::TimedOut {
                    semantic: timeout.semantic.clone(),
                    exit_code,
                };
            }
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    break ActivityStatus::Succeeded {
                        exit_code: status.code().unwrap_or(0),
                    };
                }
                Ok(Some(status)) => {
                    break ActivityStatus::Failed {
                        exit_code: status.code(),
                    };
                }
                // Keep consuming active output without imposing the idle polling delay
                // on every bounded queue drain. Recheck cancellation and timeouts each turn.
                Ok(None) if received_output => thread::yield_now(),
                Ok(None) => {
                    let sleep_started = trace_waits.then(Instant::now);
                    // A reader wakes this thread after enqueueing bytes or closing its pipe.
                    // The stored unpark token also covers output arriving between drain and park.
                    let wait = if reader_wake.finished.load(Ordering::Acquire) == 2 {
                        // EOF can precede the child's waitable exit. Check that short race
                        // promptly, then back off for programs that close pipes before exiting.
                        let wait = exit_poll_interval.min(poll_interval);
                        exit_poll_interval =
                            exit_poll_interval.saturating_mul(2).min(poll_interval);
                        wait
                    } else {
                        poll_interval
                    };
                    thread::park_timeout(wait);
                    if let Some(sleep_started) = sleep_started {
                        idle_sleep += sleep_started.elapsed();
                        idle_sleeps += 1;
                    }
                }
                Err(_) => {
                    let _ = cancel_child_tree(&mut child, poll_interval, termination_grace);
                    break ActivityStatus::Failed { exit_code: None };
                }
            }
        };

        if trace_waits {
            eprintln!(
                "DEVMIL_GIT_PERF phase=git.process.waits elapsed_us=0 pid={} idle_sleep_us={} idle_sleeps={}",
                child.id(),
                idle_sleep.as_micros(),
                idle_sleeps
            );
        }

        while !stdout_reader.is_finished() || !stderr_reader.is_finished() {
            drain_activity_streams(
                &stream_rx,
                ActivityDrainState {
                    record: &mut record,
                    sink,
                    progress: parse_git_progress.then_some(&mut progress),
                    redactors: &mut output_redactors,
                    private_stdout: &mut private_stdout,
                    private_stderr: &mut private_stderr,
                },
                &mut stdout_observer,
                &mut stderr_observer,
                output_limit,
                redact_stdout,
                redact_stderr,
            );
            thread::yield_now();
        }
        let stdout_result = stdout_reader.join();
        let stderr_result = stderr_reader.join();
        drain_activity_streams(
            &stream_rx,
            ActivityDrainState {
                record: &mut record,
                sink,
                progress: parse_git_progress.then_some(&mut progress),
                redactors: &mut output_redactors,
                private_stdout: &mut private_stdout,
                private_stderr: &mut private_stderr,
            },
            &mut stdout_observer,
            &mut stderr_observer,
            output_limit,
            redact_stdout,
            redact_stderr,
        );
        flush_activity_streams(
            &mut output_redactors,
            &mut record,
            sink,
            output_limit,
            parse_git_progress.then_some(&mut progress),
        );
        if redact_stdout && private_stdout.total_bytes > 0 {
            publish_activity_bytes(
                &mut record,
                sink,
                output_limit,
                ActivityStream::Stdout,
                b"[REDACTED]",
                None,
            );
            record.stdout.total_bytes = private_stdout.total_bytes;
            record.stdout.truncated = private_stdout.total_bytes
                > u64::try_from(record.stdout.bytes.len()).unwrap_or(u64::MAX);
        }
        if redact_stderr && private_stderr.total_bytes > 0 {
            publish_activity_bytes(
                &mut record,
                sink,
                output_limit,
                ActivityStream::Stderr,
                b"[REDACTED]",
                None,
            );
            record.stderr.total_bytes = private_stderr.total_bytes;
            record.stderr.truncated = private_stderr.total_bytes
                > u64::try_from(record.stderr.bytes.len()).unwrap_or(u64::MAX);
        }
        let input_ok = stdin_writer.map_or(Ok(()), |writer| {
            writer
                .join()
                .unwrap_or(Err(io::Error::other("stdin writer panicked")))
        });
        let readers_ok = [stdout_result, stderr_result]
            .into_iter()
            .all(|result| matches!(result, Ok(Ok(()))));
        // Terminating a process tree can make an otherwise healthy pipe reader or stdin writer
        // return an operating-system error while descriptors are being torn down. That transport
        // detail must not replace the user-visible cancellation or semantic timeout which caused
        // the teardown. For a naturally completed child, reader and writer failures still turn the
        // activity into a failed command.
        let completion = completion_after_transport_join(completion, readers_ok, input_ok.is_ok());
        record.finish(completion, unix_millis());
        sink.publish(ActivityEvent::Finished(record.clone()));
        PrivateActivityResult {
            activity: record,
            stdout: PrivateActivityBytes::new(private_stdout),
            stderr: PrivateActivityBytes::new(private_stderr),
        }
    }
}

fn remove_repository_shaping_git_environment(command: &mut Command) {
    for name in REPOSITORY_SHAPING_GIT_ENVIRONMENT {
        command.env_remove(name);
    }
}

fn completion_after_transport_join(
    completion: ActivityStatus,
    readers_ok: bool,
    input_ok: bool,
) -> ActivityStatus {
    if (readers_ok && input_ok)
        || matches!(
            completion,
            ActivityStatus::Canceled { .. } | ActivityStatus::TimedOut { .. }
        )
    {
        completion
    } else {
        ActivityStatus::Failed {
            exit_code: match completion {
                ActivityStatus::Succeeded { exit_code } => Some(exit_code),
                ActivityStatus::Failed { exit_code }
                | ActivityStatus::Canceled { exit_code }
                | ActivityStatus::TimedOut { exit_code, .. } => exit_code,
                ActivityStatus::Running | ActivityStatus::LaunchError { .. } => None,
            },
        }
    }
}

fn private_broken_spawn(
    record: ActivityRecord,
    child: Child,
    sink: &mut dyn ActivityEventSink,
    detail: &str,
) -> PrivateActivityResult {
    PrivateActivityResult {
        activity: finish_broken_spawn(record, child, sink, detail),
        stdout: PrivateActivityBytes::new(BoundedActivityBytes::empty()),
        stderr: PrivateActivityBytes::new(BoundedActivityBytes::empty()),
    }
}

fn finish_broken_spawn(
    mut record: ActivityRecord,
    mut child: Child,
    sink: &mut dyn ActivityEventSink,
    detail: &str,
) -> ActivityRecord {
    // Git may already hold an index or ref lock when transport setup fails. Give its
    // signal handler the same cleanup window as ordinary cancellation before forcing exit.
    let _ = cancel_child_tree(&mut child, DEFAULT_POLL_INTERVAL, DEFAULT_TERMINATION_GRACE);
    let _ = child.wait();
    record.finish(
        ActivityStatus::LaunchError {
            message: bounded_activity_detail(detail.to_owned()),
        },
        unix_millis(),
    );
    sink.publish(ActivityEvent::Finished(record.clone()));
    record
}

fn spawn_activity_reader(
    name: &str,
    mut reader: impl Read + Send + 'static,
    stream: ActivityStream,
    sender: SyncSender<(ActivityStream, Vec<u8>)>,
    wake: Arc<ActivityReaderWake>,
) -> io::Result<JoinHandle<io::Result<()>>> {
    thread::Builder::new().name(name.to_owned()).spawn(move || {
        let _completion = ActivityReaderCompletion(wake.clone());
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(());
            }
            if sender.send((stream, buffer[..count].to_vec())).is_err() {
                return Ok(());
            }
            wake.runner.unpark();
        }
    })
}

struct ActivityReaderWake {
    runner: thread::Thread,
    finished: AtomicUsize,
}

struct ActivityReaderCompletion(Arc<ActivityReaderWake>);

impl Drop for ActivityReaderCompletion {
    fn drop(&mut self) {
        self.0.finished.fetch_add(1, Ordering::Release);
        self.0.runner.unpark();
    }
}

fn spawn_stdin_writer(
    mut stdin: std::process::ChildStdin,
    bytes: Vec<u8>,
) -> JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        // A child may exit or close stdin without reading all of its input. That is its
        // choice: the exit status decides the outcome, not the closed pipe.
        match stdin.write_all(&bytes).and_then(|()| stdin.flush()) {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            result => result,
        }
    })
}

struct ActivityDrainState<'a> {
    record: &'a mut ActivityRecord,
    sink: &'a mut dyn ActivityEventSink,
    progress: Option<&'a mut ProgressAccumulator>,
    redactors: &'a mut OutputRedactors,
    private_stdout: &'a mut BoundedActivityBytes,
    private_stderr: &'a mut BoundedActivityBytes,
}

fn drain_activity_streams(
    receiver: &Receiver<(ActivityStream, Vec<u8>)>,
    mut state: ActivityDrainState<'_>,
    stdout_observer: &mut PrivateStdoutObserver<'_>,
    stderr_observer: &mut PrivateStderrObserver<'_>,
    output_limit: usize,
    redact_stdout: bool,
    redact_stderr: bool,
) -> bool {
    let mut received_output = false;
    for _ in 0..ACTIVITY_STREAM_QUEUE_CAPACITY {
        match receiver.try_recv() {
            Ok((stream, mut bytes)) => {
                received_output |= !bytes.is_empty();
                if stream == ActivityStream::Stdout {
                    let retained = output_limit
                        .saturating_sub(state.private_stdout.bytes.len())
                        .min(bytes.len());
                    if retained > 0 {
                        if let Some(observer) = stdout_observer.as_deref_mut() {
                            observer(&bytes[..retained]);
                        }
                    }
                }
                if stream == ActivityStream::Stderr {
                    let retained = output_limit
                        .saturating_sub(state.private_stderr.bytes.len())
                        .min(bytes.len());
                    if retained > 0 {
                        if let Some(observer) = stderr_observer.as_deref_mut() {
                            observer(&bytes[..retained]);
                        }
                    }
                }
                match stream {
                    ActivityStream::Stdout => state.private_stdout.append(&bytes, output_limit),
                    ActivityStream::Stderr => state.private_stderr.append(&bytes, output_limit),
                }
                let redact = (stream == ActivityStream::Stdout && redact_stdout)
                    || (stream == ActivityStream::Stderr && redact_stderr);
                let sanitized = if redact {
                    Vec::new()
                } else {
                    state.redactors.for_stream(stream).push(&bytes)
                };
                if redact && stream == ActivityStream::Stderr {
                    if let Some(progress) = state.progress.as_deref_mut() {
                        publish_private_progress(state.record, state.sink, progress, &bytes);
                    }
                }
                publish_activity_bytes(
                    state.record,
                    state.sink,
                    output_limit,
                    stream,
                    &sanitized,
                    (!redact).then_some(state.progress.as_deref_mut()).flatten(),
                );
                if redact {
                    bytes.zeroize();
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    received_output
}

fn publish_private_progress(
    record: &mut ActivityRecord,
    sink: &mut dyn ActivityEventSink,
    progress: &mut ProgressAccumulator,
    bytes: &[u8],
) {
    for item in progress.push(bytes) {
        record.set_progress(item.clone());
        sink.publish(ActivityEvent::Progress {
            id: record.id,
            progress: item,
        });
    }
}

fn flush_activity_streams(
    redactors: &mut OutputRedactors,
    record: &mut ActivityRecord,
    sink: &mut dyn ActivityEventSink,
    output_limit: usize,
    mut progress: Option<&mut ProgressAccumulator>,
) {
    for stream in [ActivityStream::Stdout, ActivityStream::Stderr] {
        let bytes = redactors.for_stream(stream).finish();
        publish_activity_bytes(
            record,
            sink,
            output_limit,
            stream,
            &bytes,
            progress.as_deref_mut(),
        );
    }
}

fn publish_activity_bytes(
    record: &mut ActivityRecord,
    sink: &mut dyn ActivityEventSink,
    output_limit: usize,
    stream: ActivityStream,
    bytes: &[u8],
    progress: Option<&mut ProgressAccumulator>,
) {
    if bytes.is_empty() {
        return;
    }
    record.append_output(stream, bytes, output_limit);
    sink.publish(ActivityEvent::Output {
        id: record.id,
        stream,
        bytes: bytes.to_owned(),
    });
    if stream == ActivityStream::Stderr {
        if let Some(progress) = progress {
            for item in progress.push(bytes) {
                record.set_progress(item.clone());
                sink.publish(ActivityEvent::Progress {
                    id: record.id,
                    progress: item,
                });
            }
        }
    }
}

#[derive(Debug)]
struct OutputRedactors {
    stdout: StreamRedactor,
    stderr: StreamRedactor,
}

impl OutputRedactors {
    fn new(secrets: Vec<Vec<u8>>) -> Self {
        Self {
            stdout: StreamRedactor::new(secrets.clone()),
            stderr: StreamRedactor::new(secrets),
        }
    }

    fn for_stream(&mut self, stream: ActivityStream) -> &mut StreamRedactor {
        match stream {
            ActivityStream::Stdout => &mut self.stdout,
            ActivityStream::Stderr => &mut self.stderr,
        }
    }
}

#[derive(Debug)]
struct StreamRedactor {
    pending: Vec<u8>,
    secrets: Vec<Vec<u8>>,
    maximum_secret: usize,
}

impl StreamRedactor {
    fn new(mut secrets: Vec<Vec<u8>>) -> Self {
        secrets.retain(|secret| secret.len() >= 3);
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        let maximum_secret = secrets.iter().map(Vec::len).max().unwrap_or(0);
        Self {
            pending: Vec::new(),
            secrets,
            maximum_secret,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        if self.secrets.is_empty() {
            return bytes.to_vec();
        }
        self.pending.extend_from_slice(bytes);
        let mut cut = self
            .pending
            .len()
            .saturating_sub(self.maximum_secret.saturating_sub(1));
        for start in 0..cut {
            if start >= cut {
                break;
            }
            for secret in &self.secrets {
                let available = &self.pending[start..];
                if (available.len() < secret.len() && secret.starts_with(available))
                    || (start + secret.len() > cut && available.starts_with(secret))
                {
                    cut = start;
                    break;
                }
            }
        }
        let prefix = self.pending.drain(..cut).collect::<Vec<_>>();
        redact_bytes(&prefix, &self.secrets)
    }

    fn finish(&mut self) -> Vec<u8> {
        redact_bytes(&std::mem::take(&mut self.pending), &self.secrets)
    }
}

fn redact_bytes(bytes: &[u8], secrets: &[Vec<u8>]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(secret) = secrets
            .iter()
            .find(|secret| bytes[cursor..].starts_with(secret))
        {
            output.extend_from_slice(b"[REDACTED]");
            cursor += secret.len();
        } else {
            output.push(bytes[cursor]);
            cursor += 1;
        }
    }
    output
}

#[derive(Debug, Default)]
struct ProgressAccumulator {
    partial: Vec<u8>,
}

impl ProgressAccumulator {
    fn push(&mut self, bytes: &[u8]) -> Vec<ActivityProgress> {
        let mut result = Vec::new();
        for byte in bytes {
            if matches!(byte, b'\r' | b'\n') {
                if let Some(progress) = parse_git_progress_line(&self.partial) {
                    result.push(progress);
                }
                self.partial.zeroize();
            } else if self.partial.len() < 4096 {
                self.partial.push(*byte);
            }
        }
        result
    }
}

impl Drop for ProgressAccumulator {
    fn drop(&mut self) {
        self.partial.zeroize();
    }
}

fn parse_git_progress_line(line: &[u8]) -> Option<ActivityProgress> {
    // "Filtering content" is the checkout waiting for a filter such as Git LFS to deliver
    // file contents, which can outlast every other phase of a pull.
    const PHASES: [(&[u8], &str); 6] = [
        (b"Counting objects", "Counting objects"),
        (b"Compressing objects", "Compressing objects"),
        (b"Receiving objects", "Receiving objects"),
        (b"Resolving deltas", "Resolving deltas"),
        (b"Updating files", "Updating files"),
        (b"Filtering content", "Filtering content"),
    ];
    if let Some(rebase) = line.strip_prefix(b"Rebasing (") {
        let end = rebase.iter().position(|byte| *byte == b')')?;
        let slash = rebase[..end].iter().position(|byte| *byte == b'/')?;
        let completed = &rebase[..slash];
        let total = &rebase[slash + 1..end];
        let completed = std::str::from_utf8(completed).ok()?.parse::<u64>().ok()?;
        let total = std::str::from_utf8(total).ok()?.parse::<u64>().ok()?;
        return ActivityProgress::new("Rebasing", completed, Some(total), "commits").ok();
    }
    let line = line.strip_prefix(b"remote: ").unwrap_or(line);
    let (remainder, phase) = PHASES.iter().find_map(|(prefix, phase)| {
        line.strip_prefix(*prefix)
            .and_then(|remainder| remainder.strip_prefix(b": "))
            .map(|remainder| (remainder, *phase))
    })?;
    // Git right-aligns the percentage in three columns ("  5%", " 42%", "100%").
    let remainder = remainder.trim_ascii_start();
    let percent = remainder.iter().position(|byte| *byte == b'%')?;
    let digits = &remainder[..percent];
    if digits.is_empty() || digits.len() > 3 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let completed = std::str::from_utf8(digits)
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|value| *value <= 100)?;
    ActivityProgress::new(phase, completed, Some(100), "percent").ok()
}

/// A running child whose stdout and stderr are drained on dedicated bounded readers.
///
/// This compatibility runner supports arguments, working directory, output bounds, and
/// cancellation only. It does not apply environment overrides, stdin, deadlines, activity
/// redaction, or progress settings. Use [`ActivityGitRunner`] for full command semantics.
pub struct GitProcess {
    child: Child,
    stdout: JoinHandle<io::Result<ProcessBytes>>,
    stderr: JoinHandle<io::Result<ProcessBytes>>,
    activity: ProcessActivity,
    started: Instant,
    poll_interval: Duration,
    termination_grace: Duration,
}

impl fmt::Debug for GitProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitProcess")
            .field("pid", &self.child.id())
            .field("activity", &self.activity)
            .finish_non_exhaustive()
    }
}

impl GitProcess {
    /// Starts a command in a platform-owned process tree.
    ///
    /// # Errors
    ///
    /// Returns a typed launch failure if the process or output readers cannot start.
    pub fn spawn(specification: GitProcessCommand) -> Result<Self, ProcessError> {
        let activity = specification.activity();
        let GitProcessCommand {
            executable,
            arguments,
            current_dir,
            output_limit,
            poll_interval,
            termination_grace,
            ..
        } = specification;
        let mut command = Command::new(executable);
        command
            .args(arguments.iter().map(|argument| argument.value.as_os_str()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(current_dir) = &current_dir {
            command.current_dir(current_dir);
        }
        platform::prepare(&mut command);

        let mut child = command.spawn().map_err(|error| {
            process_error(
                ProcessErrorKind::ProcessLaunch,
                format!("could not start {}: {error}", activity.executable),
            )
        })?;
        let stdout_pipe = child.stdout.take().ok_or_else(|| {
            process_error(
                ProcessErrorKind::ProcessLaunch,
                "started process has no stdout pipe",
            )
        })?;
        let stderr_pipe = child.stderr.take().ok_or_else(|| {
            process_error(
                ProcessErrorKind::ProcessLaunch,
                "started process has no stderr pipe",
            )
        })?;
        let stdout = spawn_reader("devmil-git-git-stdout", stdout_pipe, output_limit)?;
        let stderr = spawn_reader("devmil-git-git-stderr", stderr_pipe, output_limit)?;
        Ok(Self {
            child,
            stdout,
            stderr,
            activity,
            started: Instant::now(),
            poll_interval,
            termination_grace,
        })
    }

    pub fn process_id(&self) -> u32 {
        self.child.id()
    }

    /// Waits for exit, terminating the owned process tree when cancellation is requested.
    ///
    /// # Errors
    /// Returns a bounded process error when waiting, termination, or reading output fails.
    pub fn wait(mut self, cancellation: &dyn Cancellation) -> Result<ProcessOutcome, ProcessError> {
        loop {
            if cancellation.is_cancelled() {
                let status = self.cancel_tree()?;
                let output = self.finish(status)?;
                return Ok(ProcessOutcome::Cancelled(output));
            }
            match self.child.try_wait() {
                Ok(Some(status)) => return self.finish(status).map(ProcessOutcome::Exited),
                Ok(None) => thread::sleep(self.poll_interval),
                Err(error) => {
                    return Err(process_error(
                        ProcessErrorKind::ProcessFailed,
                        format!("could not observe Git process: {error}"),
                    ));
                }
            }
        }
    }

    #[cfg(unix)]
    fn cancel_tree(&mut self) -> Result<ExitStatus, ProcessError> {
        let process_id = self.child.id();
        platform::request_termination(process_id);
        let deadline = Instant::now() + self.termination_grace;
        let mut leader_status = None;
        loop {
            if leader_status.is_none() {
                match self.child.try_wait() {
                    Ok(Some(status)) => leader_status = Some(status),
                    Ok(None) => {}
                    Err(error) => {
                        return Err(process_error(
                            ProcessErrorKind::ProcessFailed,
                            format!("could not observe canceled Git process: {error}"),
                        ));
                    }
                }
            }

            let group_exists = platform::process_group_exists(process_id).map_err(|error| {
                process_error(
                    ProcessErrorKind::ProcessFailed,
                    format!("could not observe canceled Git process tree: {error}"),
                )
            })?;
            if !group_exists {
                return match leader_status {
                    Some(status) => Ok(status),
                    None => self.child.wait().map_err(|error| {
                        process_error(
                            ProcessErrorKind::ProcessFailed,
                            format!("could not reap canceled Git process: {error}"),
                        )
                    }),
                };
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(self.poll_interval);
        }

        if let Err(error) = platform::force_termination(process_id) {
            let group_still_exists = platform::process_group_exists(process_id).unwrap_or(true);
            if group_still_exists {
                return Err(process_error(
                    ProcessErrorKind::ProcessFailed,
                    format!("could not terminate Git process tree: {error}"),
                ));
            }
        }
        match leader_status {
            Some(status) => Ok(status),
            None => self.child.wait().map_err(|error| {
                process_error(
                    ProcessErrorKind::ProcessFailed,
                    format!("could not reap canceled Git process: {error}"),
                )
            }),
        }
    }

    #[cfg(windows)]
    fn cancel_tree(&mut self) -> Result<ExitStatus, ProcessError> {
        let process_id = self.child.id();
        platform::request_termination(process_id);
        let deadline = Instant::now() + self.termination_grace;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(self.poll_interval),
                Ok(None) => break,
                Err(error) => {
                    return Err(process_error(
                        ProcessErrorKind::ProcessFailed,
                        format!("could not observe canceled Git process: {error}"),
                    ));
                }
            }
        }
        platform::force_termination(process_id).map_err(|error| {
            process_error(
                ProcessErrorKind::ProcessFailed,
                format!("could not terminate Git process tree: {error}"),
            )
        })?;
        self.child.wait().map_err(|error| {
            process_error(
                ProcessErrorKind::ProcessFailed,
                format!("could not reap canceled Git process: {error}"),
            )
        })
    }

    fn finish(self, status: ExitStatus) -> Result<ProcessOutput, ProcessError> {
        let stdout = join_reader(self.stdout, "stdout")?;
        let stderr = join_reader(self.stderr, "stderr")?;
        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
            elapsed: self.started.elapsed(),
            activity: self.activity,
        })
    }
}

/// A running Git child whose stdin and stdout the caller drives, for request/response
/// protocols such as `cat-file --batch` and for streaming filters such as `lfs smudge`.
///
/// The command's environment overrides and working directory apply; stdin bytes, deadlines,
/// activity recording and progress settings do not. stderr is drained into a bounded buffer.
/// Dropping a process that has not finished terminates its whole process tree.
pub struct GitStreamProcess {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::process::ChildStdout,
    stderr: Option<JoinHandle<io::Result<ProcessBytes>>>,
    finished: bool,
}

impl fmt::Debug for GitStreamProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitStreamProcess")
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl GitStreamProcess {
    /// Starts `specification` with piped stdin and stdout in a platform-owned process tree.
    ///
    /// # Errors
    ///
    /// Returns a typed launch failure if the process or its stderr reader cannot start.
    pub fn spawn(specification: GitProcessCommand) -> Result<Self, ProcessError> {
        let executable_name = specification.activity().executable;
        let GitProcessCommand {
            executable,
            arguments,
            current_dir,
            environment,
            output_limit,
            ..
        } = specification;
        let mut command = Command::new(executable);
        remove_repository_shaping_git_environment(&mut command);
        command
            .args(arguments.iter().map(|argument| argument.value.as_os_str()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for item in environment {
            command.env(item.name, item.value);
        }
        if let Some(current_dir) = current_dir {
            command.current_dir(current_dir);
        }
        platform::prepare(&mut command);
        let mut child = command.spawn().map_err(|error| {
            process_error(
                ProcessErrorKind::ProcessLaunch,
                format!("could not start {executable_name}: {error}"),
            )
        })?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(process_error(
                ProcessErrorKind::ProcessLaunch,
                "started process lacks a pipe",
            ));
        };
        let stderr = match spawn_reader("devmil-git-stream-stderr", stderr, output_limit) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr: Some(stderr),
            finished: false,
        })
    }

    /// The child's stdin until [`Self::close_stdin`].
    pub fn stdin(&mut self) -> Option<&mut std::process::ChildStdin> {
        self.stdin.as_mut()
    }

    /// Closes stdin, which ends request/response protocols.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    pub fn stdout(&mut self) -> &mut std::process::ChildStdout {
        &mut self.stdout
    }

    /// stdin (until closed) and stdout at once, for a reader and writer that live together.
    pub fn pipes(
        &mut self,
    ) -> (
        Option<&mut std::process::ChildStdin>,
        &mut std::process::ChildStdout,
    ) {
        (self.stdin.as_mut(), &mut self.stdout)
    }

    /// Closes stdin, waits for exit and returns the status with the bounded stderr.
    ///
    /// # Errors
    ///
    /// Returns a process error when waiting or reading stderr fails.
    pub fn finish(mut self) -> Result<(ExitStatus, ProcessBytes), ProcessError> {
        self.stdin = None;
        let status = self.child.wait().map_err(|error| {
            process_error(
                ProcessErrorKind::ProcessFailed,
                format!("could not observe Git process: {error}"),
            )
        })?;
        self.finished = true;
        let stderr = self
            .stderr
            .take()
            .and_then(|reader| reader.join().ok())
            .and_then(Result::ok)
            .unwrap_or(ProcessBytes {
                bytes: Vec::new(),
                truncated: false,
            });
        Ok((status, stderr))
    }
}

impl Drop for GitStreamProcess {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.stdin = None;
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = platform::force_termination(self.child.id());
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(reader) = self.stderr.take() {
            let _ = reader.join();
        }
    }
}

fn spawn_reader<R>(
    name: &str,
    reader: R,
    limit: usize,
) -> Result<JoinHandle<io::Result<ProcessBytes>>, ProcessError>
where
    R: Read + Send + 'static,
{
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || drain_bounded(reader, limit))
        .map_err(|error| {
            process_error(
                ProcessErrorKind::ProcessLaunch,
                format!("could not start process output reader: {error}"),
            )
        })
}

fn drain_bounded(mut reader: impl Read, limit: usize) -> io::Result<ProcessBytes> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < count;
    }
    Ok(ProcessBytes { bytes, truncated })
}

fn join_reader(
    reader: JoinHandle<io::Result<ProcessBytes>>,
    stream: &str,
) -> Result<ProcessBytes, ProcessError> {
    match reader.join() {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => Err(process_error(
            ProcessErrorKind::ProcessFailed,
            format!("could not read Git {stream}: {error}"),
        )),
        Err(_) => Err(process_error(
            ProcessErrorKind::ProcessFailed,
            format!("Git {stream} reader panicked"),
        )),
    }
}

fn process_error(kind: ProcessErrorKind, message: impl Into<String>) -> ProcessError {
    ProcessError::new(kind, message)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn bounded_activity_detail(mut detail: String) -> String {
    if detail.len() > ActivityStatus::MAX_DETAIL_BYTES {
        let mut boundary = ActivityStatus::MAX_DETAIL_BYTES;
        while !detail.is_char_boundary(boundary) {
            boundary -= 1;
        }
        detail.truncate(boundary);
    }
    detail
}

fn sanitize_argument_vector(arguments: &[ProcessArgument]) -> Vec<ActivityArgument> {
    let mut redact_next = false;
    arguments
        .iter()
        .map(|argument| {
            let (raw, _) = native_display(&argument.value);
            let forced = redact_next;
            redact_next = argument_takes_sensitive_value(&raw);
            if forced {
                sanitized_argument(&argument.value, true, Some("[REDACTED]"))
            } else {
                argument.activity.clone()
            }
        })
        .collect()
}

fn known_secrets(specification: &GitProcessCommand) -> Vec<Vec<u8>> {
    let mut secrets = Vec::new();
    let mut redact_next = false;
    for argument in &specification.arguments {
        let raw = native_bytes(&argument.value);
        let (display, _) = native_display(&argument.value);
        if redact_next || argument.activity.redacted {
            secrets.push(raw.clone());
        }
        if let Some(secret) = inline_argument_secret(&raw) {
            secrets.push(secret.to_vec());
        }
        if let Some(secret) = url_userinfo_secret(&display) {
            secrets.push(secret.into_bytes());
        }
        redact_next = argument_takes_sensitive_value(&display);
    }
    for environment in &specification.environment {
        if environment.activity.redacted {
            secrets.push(native_bytes(&environment.value));
        }
    }
    secrets
}

fn inline_argument_secret(argument: &[u8]) -> Option<&[u8]> {
    let equals = argument.iter().position(|byte| *byte == b'=')?;
    let name = String::from_utf8_lossy(&argument[..equals]);
    argument_is_sensitive(&format!("{name}="))
        .then_some(&argument[equals + 1..])
        .filter(|secret| !secret.is_empty())
}

fn url_userinfo_secret(value: &str) -> Option<String> {
    let scheme = value.find("://")?;
    let authority_start = scheme + 3;
    let authority_end = value[authority_start..]
        .find(['/', '?', '#'])
        .map_or(value.len(), |index| authority_start + index);
    let userinfo = &value[authority_start..authority_end];
    let at = userinfo.rfind('@')?;
    let userinfo = &userinfo[..at];
    Some(
        userinfo
            .split_once(':')
            .map_or(userinfo, |(_, secret)| secret)
            .to_owned(),
    )
}

fn sanitized_argument(
    value: &OsStr,
    force_redaction: bool,
    replacement: Option<&str>,
) -> ActivityArgument {
    let (native, native_lossless) = native_display(value);
    let (display, redacted) = if force_redaction || argument_is_sensitive(&native) {
        (replacement.unwrap_or("[REDACTED]").to_owned(), true)
    } else if let Some(sanitized) = sanitize_url(&native) {
        (sanitized, true)
    } else {
        (native, false)
    };
    ActivityArgument {
        display,
        redacted,
        native_lossless,
    }
}

fn argument_takes_sensitive_value(argument: &str) -> bool {
    matches!(
        argument.to_ascii_lowercase().as_str(),
        "--password"
            | "--passwd"
            | "--token"
            | "--access-token"
            | "--secret"
            | "--credential"
            | "--authorization"
    )
}

fn argument_is_sensitive(argument: &str) -> bool {
    let lower = argument.to_ascii_lowercase();
    [
        "--password=",
        "--passwd=",
        "--token=",
        "--access-token=",
        "--secret=",
        "--credential=",
        "--authorization=",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
        || lower.starts_with("ghp_")
        || lower.starts_with("github_pat_")
        || lower.contains("authorization:")
        || lower.contains("extraheader=")
}

fn sanitize_url(value: &str) -> Option<String> {
    let scheme = value.find("://")?;
    let authority_start = scheme + 3;
    let authority_end = value[authority_start..]
        .find(['/', '?', '#'])
        .map_or(value.len(), |index| authority_start + index);
    let mut sanitized = value.to_owned();
    let mut changed = false;
    if let Some(at) = value[authority_start..authority_end]
        .rfind('@')
        .map(|index| authority_start + index)
    {
        sanitized = format!(
            "{}[REDACTED]@{}",
            &value[..authority_start],
            &value[at + 1..]
        );
        changed = true;
    }
    for marker in ["access_token=", "token=", "password=", "credential="] {
        let mut search_start = 0;
        loop {
            let lower = sanitized.to_ascii_lowercase();
            let Some(relative_start) = lower[search_start..].find(marker) else {
                break;
            };
            let start = search_start + relative_start;
            let value_start = start + marker.len();
            let value_end = sanitized[value_start..]
                .find(['&', '#'])
                .map_or(sanitized.len(), |index| value_start + index);
            if &sanitized[value_start..value_end] == "[REDACTED]" {
                search_start = value_end;
                continue;
            }
            sanitized.replace_range(value_start..value_end, "[REDACTED]");
            changed = true;
            search_start = value_start + "[REDACTED]".len();
        }
    }
    changed.then_some(sanitized)
}

fn environment_name_is_sensitive(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "TOKEN",
        "PASSWORD",
        "PASSWD",
        "SECRET",
        "AUTH",
        "CREDENTIAL",
        "COOKIE",
        "PRIVATE_KEY",
        "ASKPASS",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
}

#[cfg(unix)]
fn native_display(value: &OsStr) -> (String, bool) {
    use std::os::unix::ffi::OsStrExt;
    if let Some(value) = value.to_str() {
        return (value.to_owned(), true);
    }
    let mut display = String::from("b\"");
    for byte in value.as_bytes() {
        match byte {
            b'\\' => display.push_str("\\\\"),
            b'\"' => display.push_str("\\\""),
            0x20..=0x7e => display.push(char::from(*byte)),
            _ => write!(display, "\\x{byte:02x}").expect("writing to a string cannot fail"),
        }
    }
    display.push('"');
    (display, false)
}

#[cfg(unix)]
fn native_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn native_display(value: &OsStr) -> (String, bool) {
    use std::os::windows::ffi::OsStrExt;
    if let Some(value) = value.to_str() {
        return (value.to_owned(), true);
    }
    let mut display = String::from("u\"");
    for unit in value.encode_wide() {
        if matches!(unit, 0x20..=0x7e) && unit != u16::from(b'\\') && unit != u16::from(b'\"') {
            display.push(char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}'));
        } else {
            write!(display, "\\u{{{unit:04x}}}").expect("writing to a string cannot fail");
        }
    }
    display.push('"');
    (display, false)
}

#[cfg(windows)]
fn native_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn cancel_child_tree(
    child: &mut Child,
    poll_interval: Duration,
    termination_grace: Duration,
) -> io::Result<ExitStatus> {
    let process_id = child.id();
    platform::request_termination(process_id);
    let deadline = Instant::now() + termination_grace;
    let mut leader_status = None;
    loop {
        if leader_status.is_none() {
            leader_status = child.try_wait()?;
        }
        if !platform::process_group_exists(process_id)? {
            return leader_status.map_or_else(|| child.wait(), Ok);
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(poll_interval);
    }
    if let Err(error) = platform::force_termination(process_id) {
        if platform::process_group_exists(process_id).unwrap_or(true) {
            return Err(error);
        }
    }
    leader_status.map_or_else(|| child.wait(), Ok)
}

#[cfg(windows)]
fn cancel_child_tree(
    child: &mut Child,
    poll_interval: Duration,
    termination_grace: Duration,
) -> io::Result<ExitStatus> {
    let process_id = child.id();
    platform::request_termination(process_id);
    let deadline = Instant::now() + termination_grace;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status),
            None if Instant::now() < deadline => thread::sleep(poll_interval),
            None => break,
        }
    }
    platform::force_termination(process_id)?;
    child.wait()
}

#[cfg(unix)]
mod platform {
    use super::{io, Command, Stdio};
    use std::os::unix::process::CommandExt;

    /// Registers the child in its own process group. While a debugger traces this
    /// process, additionally forces the fork/exec spawn path: glibc implements
    /// `posix_spawn` with `CLONE_VFORK` and lldb halts the whole inferior on every
    /// vfork event without resuming it, which freezes each Git spawn under F5.
    /// `posix_spawn` cannot express a uid change, so pinning the child uid to the
    /// current uid makes `std::process::Command` fall back to fork/exec, which lldb
    /// follows without stopping. The pin is semantically neutral for the child.
    pub fn prepare(command: &mut Command) {
        command.process_group(0);
        #[cfg(target_os = "linux")]
        if traced_by_debugger() {
            command.uid(rustix::process::getuid().as_raw());
        }
    }

    /// Returns true when a debugger currently traces this process.
    ///
    /// glibc implements `posix_spawn` with `CLONE_VFORK`, and lldb halts the whole
    /// inferior on every vfork event without resuming it, so each Git spawn freezes a
    /// debugged session. Registering a no-op `pre_exec` hook makes
    /// `std::process::Command` use the fork/exec path, which lldb follows without
    /// stopping.
    #[cfg(target_os = "linux")]
    fn traced_by_debugger() -> bool {
        use std::sync::OnceLock;
        static TRACED: OnceLock<bool> = OnceLock::new();
        *TRACED.get_or_init(|| {
            std::fs::read_to_string("/proc/self/status")
                .is_ok_and(|status| tracer_pid(&status) != 0)
        })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn tracer_pid(status: &str) -> u32 {
        status
            .lines()
            .find_map(|line| line.strip_prefix("TracerPid:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn request_termination(process_id: u32) {
        let _ = signal_group(process_id, "-TERM");
    }

    pub fn force_termination(process_id: u32) -> io::Result<()> {
        signal_group(process_id, "-KILL")
    }

    pub fn process_group_exists(process_id: u32) -> io::Result<bool> {
        let status = Command::new("/bin/kill")
            .arg("-0")
            .arg("--")
            .arg(format!("-{process_id}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.success())
    }

    fn signal_group(process_id: u32, signal: &str) -> io::Result<()> {
        let status = Command::new("/bin/kill")
            .arg(signal)
            .arg("--")
            .arg(format!("-{process_id}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "kill {signal} exited with {status}"
            )))
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{io, Command, Stdio};
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    pub fn prepare(command: &mut Command) {
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    pub fn request_termination(process_id: u32) {
        let _ = taskkill(process_id, false);
    }

    pub fn force_termination(process_id: u32) -> io::Result<()> {
        taskkill(process_id, true)
    }

    fn taskkill(process_id: u32, force: bool) -> io::Result<()> {
        let mut command = Command::new("taskkill.exe");
        command.args(["/PID", &process_id.to_string(), "/T"]);
        if force {
            command.arg("/F");
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("taskkill exited with {status}")))
        }
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("devmil-git has no process-tree adapter for this operating system");

#[cfg(test)]
mod tests {
    use super::{
        drain_bounded, ActivityGitRunner, Duration, GitProcessCommand, ProcessArgument,
        StreamRedactor, ACTIVITY_STREAM_QUEUE_CAPACITY,
    };
    use crate::activity::{ActivityEvent, ActivityEventSink, ActivityId, ActivityStatus};
    use crate::error::ProcessErrorKind;
    use std::ffi::OsStr;

    #[derive(Default)]
    struct Events(Vec<ActivityEvent>);

    impl ActivityEventSink for Events {
        fn publish(&mut self, event: ActivityEvent) {
            self.0.push(event);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tracer_pid_parses_proc_status() {
        assert_eq!(
            super::platform::tracer_pid("Name:\tgit\nTracerPid:\t1234\nState:\tS\n"),
            1234
        );
        assert_eq!(
            super::platform::tracer_pid("Name:\tgit\nTracerPid:\t0\n"),
            0
        );
        assert_eq!(super::platform::tracer_pid("Name:\tgit\n"), 0);
    }

    #[cfg(unix)]
    #[test]
    fn broken_spawn_allows_signal_cleanup_and_reaps_the_child() {
        let directory = std::env::temp_dir().join(format!(
            "devmil-git-broken-spawn-cleanup-{}-{}",
            std::process::id(),
            super::unix_millis()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let cleanup = directory.join("cleaned");
        let ready = directory.join("ready");
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("trap 'printf cleaned > \"$1\"; exit 0' TERM; printf ready > \"$2\"; while :; do :; done")
            .arg("devmil-git-cleanup-test")
            .arg(&cleanup)
            .arg(&ready)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        super::platform::prepare(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if !ready.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not install its cleanup handler");
        }
        let record = crate::activity::ActivityRecord::started(
            ActivityId::new(11).unwrap(),
            GitProcessCommand::new("/bin/sh").activity_command(),
            super::unix_millis(),
        );
        let mut events = Events::default();
        let result = super::finish_broken_spawn(record, child, &mut events, "reader setup failed");
        assert_eq!(std::fs::read(&cleanup).unwrap(), b"cleaned");
        assert!(!super::platform::process_group_exists(pid).unwrap());
        assert!(matches!(result.status, ActivityStatus::LaunchError { .. }));
        assert!(matches!(events.0.as_slice(), [ActivityEvent::Finished(_)]));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn activity_keeps_redacted_argument_instead_of_secret() {
        let specification = GitProcessCommand::new("git")
            .arg(ProcessArgument::visible("fetch"))
            .arg(ProcessArgument::redacted("credential-value", "[REDACTED]"));

        let activity = specification.activity();

        assert_eq!(activity.executable, "git");
        assert_eq!(activity.arguments, ["fetch", "[REDACTED]"]);
        assert!(!format!("{activity:?}").contains("credential-value"));
        assert!(!format!("{specification:?}").contains("credential-value"));
    }

    #[test]
    fn command_debug_applies_context_redaction_and_stream_redaction_crosses_chunks() {
        let specification = GitProcessCommand::new("git").args([
            ProcessArgument::visible("--password"),
            ProcessArgument::visible("boundary-secret"),
        ]);
        assert!(!format!("{specification:?}").contains("boundary-secret"));

        let mut redactor = StreamRedactor::new(vec![b"boundary-secret".to_vec()]);
        let mut output = redactor.push(b"prefix boundary-");
        output.extend(redactor.push(b"secret suffix"));
        output.extend(redactor.finish());
        assert_eq!(output, b"prefix [REDACTED] suffix");
    }

    #[cfg(unix)]
    #[test]
    fn private_stdout_parses_authenticated_url_while_public_activity_is_constant_redaction() {
        let secret = "cross-chunk-token";
        let command = GitProcessCommand::new("/bin/sh")
            .args([
                ProcessArgument::visible("-c"),
                ProcessArgument::visible(
                    "printf 'remote.origin.url\\nhttps://user:'; printf '%s' \"$1\"; printf '@example.test/repo\\0'",
                ),
                ProcessArgument::visible("devmil-git-test"),
                ProcessArgument::redacted(secret, "[REDACTED]"),
            ])
            .redact_stdout();
        let mut sink = Events::default();
        let result =
            ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut sink);

        assert!(result
            .stdout
            .bytes
            .windows(secret.len())
            .any(|bytes| bytes == secret.as_bytes()));
        let public = format!("{:?}{:?}", result.activity, sink.0);
        assert!(!public.contains(secret));
        assert_eq!(result.activity.stdout.bytes, b"[REDACTED]");
    }

    #[cfg(unix)]
    #[test]
    fn sensitive_command_implicitly_redacts_both_public_streams_before_created_event() {
        let credential = "credential-value-never-public";
        let stdout_marker = "unrelated-private-stdout";
        let stderr_marker = "unrelated-private-stderr";
        let script = std::env::temp_dir().join(format!(
            "devmil-git-implicit-redaction-{}",
            std::process::id()
        ));
        std::fs::write(
            &script,
            format!("printf '%s' '{stdout_marker}'; printf '%s' '{stderr_marker}' >&2\n"),
        )
        .expect("write implicit redaction fixture");
        let command = GitProcessCommand::new("/bin/sh").args([
            ProcessArgument::visible(script.as_os_str()),
            ProcessArgument::redacted(credential, "[REDACTED]"),
        ]);
        let mut sink = Events::default();
        let result =
            ActivityGitRunner::run_private(command, ActivityId::new(2).unwrap(), None, &mut sink);

        assert!(result.activity.stdout_redacted);
        assert!(result.activity.stderr_redacted);
        assert_eq!(result.activity.stdout.bytes, b"[REDACTED]");
        assert_eq!(result.activity.stderr.bytes, b"[REDACTED]");
        assert_eq!(result.stdout.bytes, stdout_marker.as_bytes());
        assert_eq!(result.stderr.bytes, stderr_marker.as_bytes());
        assert!(
            matches!(sink.0.first(), Some(ActivityEvent::Created(record))
            if record.stdout_redacted && record.stderr_redacted)
        );
        assert!(sink
            .0
            .iter()
            .filter_map(|event| match event {
                ActivityEvent::Output { bytes, .. } => Some(bytes),
                _ => None,
            })
            .all(|bytes| bytes == b"[REDACTED]"));
        let public = format!("{:?}{:?}", result.activity, sink.0);
        assert!(!public.contains(credential));
        assert!(!public.contains(stdout_marker));
        assert!(!public.contains(stderr_marker));
        std::fs::remove_file(script).expect("remove implicit redaction fixture");
    }

    #[cfg(unix)]
    #[test]
    fn sensitive_environment_alone_implicitly_redacts_both_public_streams() {
        let credential = "environment-credential-never-public";
        let stdout_marker = "environment-private-stdout";
        let stderr_marker = "environment-private-stderr";
        let script = std::env::temp_dir().join(format!(
            "devmil-git-implicit-environment-redaction-{}",
            std::process::id()
        ));
        std::fs::write(
            &script,
            format!("printf '%s' '{stdout_marker}'; printf '%s' '{stderr_marker}' >&2\n"),
        )
        .expect("write implicit environment redaction fixture");
        let command = GitProcessCommand::new("/bin/sh")
            .arg(ProcessArgument::visible(script.as_os_str()))
            .env("DEVMIL_GIT_TEST_AUTH_TOKEN", credential)
            .expect("sensitive environment is valid");
        let mut sink = Events::default();
        let result =
            ActivityGitRunner::run_private(command, ActivityId::new(3).unwrap(), None, &mut sink);

        assert!(result.activity.stdout_redacted);
        assert!(result.activity.stderr_redacted);
        assert_eq!(result.activity.stdout.bytes, b"[REDACTED]");
        assert_eq!(result.activity.stderr.bytes, b"[REDACTED]");
        assert_eq!(result.stdout.bytes, stdout_marker.as_bytes());
        assert_eq!(result.stderr.bytes, stderr_marker.as_bytes());
        let public = format!("{:?}{:?}", result.activity, sink.0);
        assert!(!public.contains(credential));
        assert!(!public.contains(stdout_marker));
        assert!(!public.contains(stderr_marker));
        std::fs::remove_file(script).expect("remove implicit environment redaction fixture");
    }

    #[cfg(unix)]
    #[test]
    fn sanitized_output_keeps_diagnostics_and_replaces_private_transport_metadata() {
        let private_path = "/private/devmil-git-askpass-fixture";
        let command = GitProcessCommand::new("/bin/sh")
            .args([
                ProcessArgument::visible("-c"),
                ProcessArgument::visible(
                    "printf 'helper=%s\\nfatal: the remote end hung up unexpectedly\\n' \"$GIT_ASKPASS\" >&2",
                ),
            ])
            .env("GIT_ASKPASS", private_path)
            .expect("askpass environment is valid")
            .publish_sanitized_output();
        let mut sink = Events::default();
        let result =
            ActivityGitRunner::run_private(command, ActivityId::new(31).unwrap(), None, &mut sink);

        assert!(!result.activity.stderr_redacted);
        let stderr = String::from_utf8(result.activity.stderr.bytes.clone()).unwrap();
        assert!(stderr.contains("helper=[REDACTED]"));
        assert!(stderr.contains("fatal: the remote end hung up unexpectedly"));
        assert!(!stderr.contains(private_path));
        assert!(!format!("{:?}", sink.0).contains(private_path));
    }

    #[test]
    fn explicit_environment_redaction_does_not_depend_on_the_variable_name() {
        let secret = "opaque-value-that-must-not-be-published";
        let command = GitProcessCommand::new("git")
            .env_redacted("OUTPUT_MARKER", secret)
            .expect("explicitly redacted environment is valid");
        let activity = command.activity_command();

        assert_eq!(activity.environment.len(), 1);
        assert_eq!(activity.environment[0].name, "OUTPUT_MARKER");
        assert_eq!(activity.environment[0].display, "[REDACTED]");
        assert!(activity.environment[0].redacted);
        assert!(!format!("{activity:?}").contains(secret));
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_leaves_its_input_unread_is_judged_by_its_exit_status() {
        // More than a pipe buffer: the writer is still blocked when the child exits.
        let input = vec![b'x'; 4 * 1024 * 1024];
        for (script, expected) in [
            ("exit 0", ActivityStatus::Succeeded { exit_code: 0 }),
            ("exit 3", ActivityStatus::Failed { exit_code: Some(3) }),
        ] {
            let command = GitProcessCommand::new("/bin/sh")
                .args([
                    ProcessArgument::visible("-c"),
                    ProcessArgument::visible(script),
                ])
                .stdin_bytes(input.clone());
            let mut sink = Events::default();
            let result = ActivityGitRunner::run_private(
                command,
                ActivityId::new(4).unwrap(),
                None,
                &mut sink,
            );
            assert_eq!(result.activity.status, expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_stderr_is_bounded_while_public_events_use_only_the_constant() {
        let command = GitProcessCommand::new("/bin/sh")
            .args([
                ProcessArgument::visible("-c"),
                ProcessArgument::visible("printf '%064d' 0 >&2"),
                ProcessArgument::redacted("private", "[REDACTED]"),
            ])
            .output_limit(16);
        let mut sink = Events::default();
        let result =
            ActivityGitRunner::run_private(command, ActivityId::new(4).unwrap(), None, &mut sink);

        assert_eq!(result.stderr.bytes.len(), 16);
        assert_eq!(result.stderr.total_bytes, 64);
        assert!(result.stderr.truncated);
        assert_eq!(result.activity.stderr.bytes, b"[REDACTED]");
        assert!(sink
            .0
            .iter()
            .filter_map(|event| match event {
                ActivityEvent::Output { bytes, .. } => Some(bytes.as_slice()),
                _ => None,
            })
            .all(|bytes| bytes == b"[REDACTED]"));
    }

    #[cfg(unix)]
    #[test]
    fn redacted_progress_accepts_only_fixed_git_phases_and_never_projects_input_phase_text() {
        let secret_phase = "private-phase-secret";
        let command = GitProcessCommand::new("/bin/sh")
            .args([
                ProcessArgument::visible("-c"),
                ProcessArgument::visible("printf '%s' \"$DEVMIL_GIT_TEST_AUTH_PROGRESS\" >&2"),
            ])
            .env(
                "DEVMIL_GIT_TEST_AUTH_PROGRESS",
                format!("{secret_phase}: 42%\rremote: Receiving objects: 42% (42/100)\r"),
            )
            .expect("sensitive progress environment is valid")
            .parse_git_progress(true);
        let mut sink = Events::default();
        let result =
            ActivityGitRunner::run_private(command, ActivityId::new(5).unwrap(), None, &mut sink);

        let progress = sink
            .0
            .iter()
            .filter_map(|event| match event {
                ActivityEvent::Progress { progress, .. } => Some(progress),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(progress.len(), 1);
        assert_eq!(progress[0].phase, "Receiving objects");
        assert_eq!(progress[0].completed, 42);
        assert_eq!(result.activity.last_progress.as_ref(), Some(progress[0]));
        assert_eq!(result.activity.stderr.bytes, b"[REDACTED]");
        let public = format!("{:?}{:?}", result.activity, sink.0);
        assert!(!public.contains(secret_phase));
    }

    #[test]
    fn progress_reads_the_padded_percentage_git_prints() {
        for (line, phase, completed) in [
            (
                b"Receiving objects:   5% (5/100)".as_slice(),
                "Receiving objects",
                5,
            ),
            (b"Updating files:  85% (2360/2763)", "Updating files", 85),
            (
                b"remote: Counting objects: 100% (9/9)",
                "Counting objects",
                100,
            ),
            (
                b"Filtering content:  45% (15/33), 12.40 MiB | 3.10 MiB/s",
                "Filtering content",
                45,
            ),
        ] {
            let progress = super::parse_git_progress_line(line).unwrap();
            assert_eq!(progress.phase, phase);
            assert_eq!(progress.completed, completed);
            assert_eq!(progress.total, Some(100));
        }
        assert!(super::parse_git_progress_line(b"Updating files:     % (1/2)").is_none());
    }

    #[test]
    fn rebase_progress_reports_exact_commit_step() {
        let progress = super::parse_git_progress_line(b"Rebasing (3/7)\r").unwrap();
        assert_eq!(progress.phase, "Rebasing");
        assert_eq!(progress.completed, 3);
        assert_eq!(progress.total, Some(7));
        assert_eq!(progress.unit, "commits");
        assert!(super::parse_git_progress_line(b"Rebasing (8/7)").is_none());
    }

    struct SlowSink;

    #[test]
    fn installed_git_commands_bound_silence_without_a_total_duration_limit() {
        let command = GitProcessCommand::git("custom-installed-git");
        assert!(command.timeout.is_none());
        let timeout = command.idle_timeout.unwrap();
        assert_eq!(timeout.after, Duration::from_mins(5));
        assert!(timeout.semantic.contains("no output"));
        // Other subprocesses keep their caller-owned deadline semantics.
        assert!(GitProcessCommand::new("external-tool")
            .idle_timeout
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn output_idle_timeout_reaps_silent_child_and_finishes_activity() {
        let command = GitProcessCommand::git("/bin/sh")
            .args([
                ProcessArgument::visible("-c"),
                ProcessArgument::visible("sleep 30 & wait"),
            ])
            .output_idle_timeout(
                super::SemanticTimeout::new(Duration::from_millis(100), "silent fetch").unwrap(),
            );
        let mut events = Events::default();
        let started = std::time::Instant::now();
        let record =
            ActivityGitRunner::run(command, ActivityId::new(9).unwrap(), None, &mut events);
        // Returning also joins pipe readers held open by the descendant sleep process.
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(
            matches!(&record.status, ActivityStatus::TimedOut { semantic, .. } if semantic == "silent fetch")
        );
        assert!(
            matches!(events.0.last(), Some(ActivityEvent::Finished(final_record)) if final_record == &record)
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_idle_timeout_allows_progress_past_budget_even_after_output_limit() {
        for stream in [1, 2] {
            let command = GitProcessCommand::git("/bin/sh")
                .args([
                    ProcessArgument::visible("-c"),
                    ProcessArgument::visible(format!(
                        "i=0; while [ $i -lt 12 ]; do printf x >&{stream}; sleep 0.1; i=$((i+1)); done"
                    )),
                ])
                .output_limit(1)
                .redact_output()
                .output_idle_timeout(
                    super::SemanticTimeout::new(Duration::from_millis(500), "silent fetch").unwrap(),
                );
            let started = std::time::Instant::now();
            let record = ActivityGitRunner::run(
                command,
                ActivityId::new(10).unwrap(),
                None,
                &mut Events::default(),
            );
            assert!(started.elapsed() > Duration::from_millis(500));
            assert!(matches!(record.status, ActivityStatus::Succeeded { .. }));
        }
    }

    impl ActivityEventSink for SlowSink {
        fn publish(&mut self, event: ActivityEvent) {
            if matches!(event, ActivityEvent::Output { .. }) {
                std::thread::sleep(Duration::from_micros(100));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn active_private_output_does_not_wait_for_the_idle_poll_interval() {
        let byte_count = 8 * 1024 * 1024;
        let command = GitProcessCommand::new("/bin/sh")
            .args([
                ProcessArgument::visible("-c"),
                ProcessArgument::visible(format!("head -c {byte_count} /dev/zero")),
            ])
            .output_limit(byte_count)
            .redact_output()
            .timing(Duration::from_millis(100), Duration::from_millis(250))
            .semantic_timeout(
                super::SemanticTimeout::new(Duration::from_secs(4), "active output drain").unwrap(),
            );
        let mut events = Events::default();
        let result =
            ActivityGitRunner::run_private(command, ActivityId::new(2).unwrap(), None, &mut events);
        assert!(matches!(
            result.activity.status,
            ActivityStatus::Succeeded { .. }
        ));
        assert_eq!(result.stdout.bytes.len(), byte_count);
        assert!(!result.stdout.truncated);
        assert!(events.0.iter().all(
            |event| !matches!(event, ActivityEvent::Output { bytes, .. } if bytes.contains(&0))
        ));
    }

    #[test]
    fn activity_reader_wakes_for_separate_output_bursts_and_eof() {
        struct GatedReader(std::sync::mpsc::Receiver<Vec<u8>>);

        impl std::io::Read for GatedReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let bytes = self.0.recv().unwrap_or_default();
                buffer[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
        }

        let (input_tx, input_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let wake = std::sync::Arc::new(super::ActivityReaderWake {
                runner: std::thread::current(),
                finished: std::sync::atomic::AtomicUsize::new(0),
            });
            let (stream_tx, stream_rx) =
                std::sync::mpsc::sync_channel(ACTIVITY_STREAM_QUEUE_CAPACITY);
            let reader = super::spawn_activity_reader(
                "devmil-git-test-reader-wake",
                GatedReader(input_rx),
                crate::activity::ActivityStream::Stdout,
                stream_tx,
                wake.clone(),
            )
            .unwrap();
            for expected in [Some(b"first".as_slice()), Some(b"second".as_slice()), None] {
                ready_tx.send(()).unwrap();
                loop {
                    if let Some(expected) = expected {
                        if let Ok((_, bytes)) = stream_rx.try_recv() {
                            assert_eq!(bytes, expected);
                            break;
                        }
                    } else if wake.finished.load(std::sync::atomic::Ordering::Acquire) == 1 {
                        break;
                    }
                    // Long enough to distinguish a notification from the timer fallback,
                    // including a notification delivered just before parking.
                    std::thread::park_timeout(Duration::from_secs(5));
                }
                result_tx.send(()).unwrap();
            }
            reader.join().unwrap().unwrap();
        });
        for bytes in [b"first".to_vec(), b"second".to_vec(), Vec::new()] {
            ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            // Let the waiter become idle between bursts, rather than merely testing
            // a queue that already contained output before its first drain.
            std::thread::sleep(Duration::from_millis(20));
            input_tx.send(bytes).unwrap();
            result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        waiter.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn hostile_multi_hundred_megabyte_producer_is_backpressured_and_reaped_on_timeout() {
        assert_eq!(ACTIVITY_STREAM_QUEUE_CAPACITY, 8);
        let command = GitProcessCommand::new("/bin/sh")
            .args([
                ProcessArgument::visible("-c"),
                ProcessArgument::visible("head -c 250000000 /dev/zero"),
            ])
            .semantic_timeout(
                super::SemanticTimeout::new(Duration::from_millis(30), "bounded-output test")
                    .unwrap(),
            );
        let record =
            ActivityGitRunner::run(command, ActivityId::new(2).unwrap(), None, &mut SlowSink);
        assert!(matches!(record.status, ActivityStatus::TimedOut { .. }));
    }

    #[test]
    fn launch_failure_is_typed_and_bounded() {
        let error = super::GitProcess::spawn(GitProcessCommand::new(
            "/devmil-git-test-path/does-not-exist/git",
        ))
        .expect_err("missing executable must fail");
        assert_eq!(error.kind(), ProcessErrorKind::ProcessLaunch);
        assert!(error.message().len() <= 4096);
    }

    #[test]
    fn bounded_drain_keeps_prefix_and_drains_remainder() {
        let input = vec![7_u8; 1024];
        let output = drain_bounded(input.as_slice(), 17).expect("memory read succeeds");
        assert_eq!(output.bytes, vec![7_u8; 17]);
        assert!(output.truncated);
    }

    #[test]
    fn test_timing_is_configurable_without_changing_production_defaults() {
        let specification = GitProcessCommand::new("git")
            .timing(Duration::from_millis(1), Duration::from_millis(2));
        assert_eq!(specification.poll_interval, Duration::from_millis(1));
        assert_eq!(specification.termination_grace, Duration::from_millis(2));
    }

    #[test]
    fn cancellation_and_timeout_survive_transport_teardown_errors() {
        let canceled = super::completion_after_transport_join(
            ActivityStatus::Canceled {
                exit_code: Some(15),
            },
            false,
            false,
        );
        assert!(matches!(
            canceled,
            ActivityStatus::Canceled {
                exit_code: Some(15)
            }
        ));

        let timed_out = super::completion_after_transport_join(
            ActivityStatus::TimedOut {
                semantic: "fixture".to_owned(),
                exit_code: None,
            },
            false,
            true,
        );
        assert!(matches!(timed_out, ActivityStatus::TimedOut { .. }));

        let completed = super::completion_after_transport_join(
            ActivityStatus::Succeeded { exit_code: 0 },
            false,
            true,
        );
        assert!(matches!(
            completed,
            ActivityStatus::Failed { exit_code: Some(0) }
        ));
    }

    #[test]
    fn repository_shaping_environment_is_removed_before_explicit_overrides() {
        let mut command = std::process::Command::new("git");
        super::remove_repository_shaping_git_environment(&mut command);

        for name in super::REPOSITORY_SHAPING_GIT_ENVIRONMENT {
            assert!(
                command
                    .get_envs()
                    .any(|(candidate, value)| { candidate == OsStr::new(name) && value.is_none() }),
                "{name} must be removed from the inherited environment"
            );
        }

        command.env("GIT_SHALLOW_FILE", "/approved/frozen-shallow-file");
        assert!(command.get_envs().any(|(candidate, value)| {
            candidate == OsStr::new("GIT_SHALLOW_FILE")
                && value == Some(OsStr::new("/approved/frozen-shallow-file"))
        }));
    }

    #[cfg(windows)]
    #[test]
    fn windows_adapter_creates_a_distinct_process_group() {
        let mut command = std::process::Command::new("cmd.exe");
        super::platform::prepare(&mut command);
        // This is a compile-level contract. Physical process-tree evidence belongs to a Windows
        // runner and must not be inferred from a Linux test run.
    }
}
