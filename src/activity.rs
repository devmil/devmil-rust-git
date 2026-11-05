//! Redacted command events and bounded output values.

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ActivityId(u64);

impl ActivityId {
    /// # Errors
    ///
    /// Zero is reserved for data migrations that have not assigned an identity.
    pub const fn new(value: u64) -> Result<Self, ActivityContractError> {
        if value == 0 {
            Err(ActivityContractError::InvalidIdentity)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityArgument {
    pub display: String,
    pub redacted: bool,
    pub native_lossless: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityEnvironment {
    pub name: String,
    pub display: String,
    pub redacted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityCommand {
    pub executable: String,
    pub arguments: Vec<ActivityArgument>,
    pub environment: Vec<ActivityEnvironment>,
    pub working_directory: Option<String>,
    pub stdin_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedActivityBytes {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
}

impl BoundedActivityBytes {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            total_bytes: 0,
            truncated: false,
        }
    }

    pub fn append(&mut self, chunk: &[u8], limit: usize) {
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        let retained = limit.saturating_sub(self.bytes.len()).min(chunk.len());
        self.bytes.extend_from_slice(&chunk[..retained]);
        self.truncated |= retained < chunk.len();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityProgress {
    pub phase: String,
    pub completed: u64,
    pub total: Option<u64>,
    pub unit: String,
}

impl ActivityProgress {
    pub const MAX_TEXT_BYTES: usize = 128;

    /// # Errors
    ///
    /// Rejects empty, oversized, or internally inconsistent progress.
    pub fn new(
        phase: impl Into<String>,
        completed: u64,
        total: Option<u64>,
        unit: impl Into<String>,
    ) -> Result<Self, ActivityContractError> {
        let phase = phase.into();
        let unit = unit.into();
        if phase.is_empty()
            || phase.len() > Self::MAX_TEXT_BYTES
            || unit.is_empty()
            || unit.len() > Self::MAX_TEXT_BYTES
            || total.is_some_and(|value| completed > value)
        {
            return Err(ActivityContractError::InvalidProgress);
        }
        Ok(Self {
            phase,
            completed,
            total,
            unit,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityStatus {
    Running,
    Succeeded {
        exit_code: i32,
    },
    Failed {
        exit_code: Option<i32>,
    },
    Canceled {
        exit_code: Option<i32>,
    },
    TimedOut {
        semantic: String,
        exit_code: Option<i32>,
    },
    LaunchError {
        message: String,
    },
}

impl ActivityStatus {
    pub const MAX_DETAIL_BYTES: usize = 512;

    #[must_use]
    pub const fn is_finished(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityRecord {
    pub id: ActivityId,
    pub command: ActivityCommand,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: Option<u64>,
    pub status: ActivityStatus,
    pub stdout: BoundedActivityBytes,
    pub stderr: BoundedActivityBytes,
    pub stdout_redacted: bool,
    pub stderr_redacted: bool,
    pub last_progress: Option<ActivityProgress>,
}

impl ActivityRecord {
    /// Process-level failure information when no output stream contains the diagnostic.
    #[must_use]
    pub fn failure_detail(&self) -> Option<&str> {
        match &self.status {
            ActivityStatus::LaunchError { message } => Some(message),
            ActivityStatus::TimedOut { semantic, .. } => Some(semantic),
            _ => None,
        }
    }

    #[must_use]
    pub const fn started(id: ActivityId, command: ActivityCommand, unix_ms: u64) -> Self {
        Self {
            id,
            command,
            started_at_unix_ms: unix_ms,
            finished_at_unix_ms: None,
            status: ActivityStatus::Running,
            stdout: BoundedActivityBytes::empty(),
            stderr: BoundedActivityBytes::empty(),
            stdout_redacted: false,
            stderr_redacted: false,
            last_progress: None,
        }
    }

    pub fn append_output(&mut self, stream: ActivityStream, bytes: &[u8], limit: usize) {
        match stream {
            ActivityStream::Stdout => self.stdout.append(bytes, limit),
            ActivityStream::Stderr => self.stderr.append(bytes, limit),
        }
    }

    pub const fn set_output_redaction(&mut self, stdout: bool, stderr: bool) {
        self.stdout_redacted = stdout;
        self.stderr_redacted = stderr;
    }

    pub fn set_progress(&mut self, progress: ActivityProgress) {
        self.last_progress = Some(progress);
    }

    pub fn finish(&mut self, status: ActivityStatus, unix_ms: u64) {
        debug_assert!(status.is_finished());
        self.status = status;
        self.finished_at_unix_ms = Some(unix_ms.max(self.started_at_unix_ms));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityEvent {
    Created(ActivityRecord),
    Output {
        id: ActivityId,
        stream: ActivityStream,
        bytes: Vec<u8>,
    },
    Progress {
        id: ActivityId,
        progress: ActivityProgress,
    },
    Finished(ActivityRecord),
}

/// Receives already-redacted records and bounded streaming chunks.
pub trait ActivityEventSink {
    fn publish(&mut self, event: ActivityEvent);
}

impl<F: FnMut(ActivityEvent)> ActivityEventSink for F {
    fn publish(&mut self, event: ActivityEvent) {
        self(event);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityContractError {
    InvalidIdentity,
    InvalidProgress,
    InvalidCapacity,
}

impl fmt::Display for ActivityContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidIdentity => "activity identity cannot be zero",
            Self::InvalidProgress => "activity progress is empty, oversized, or inconsistent",
            Self::InvalidCapacity => "activity capacities must be nonzero",
        })
    }
}

impl std::error::Error for ActivityContractError {}
