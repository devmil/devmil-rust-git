//! Bounded errors from process launch, waiting, and output collection.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessErrorKind {
    ProcessLaunch,
    ProcessFailed,
}

/// An error with a nonempty, UTF-8-safe message bounded to 4096 bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessError {
    kind: ProcessErrorKind,
    message: String,
}

impl ProcessError {
    pub const MAX_MESSAGE_BYTES: usize = 4096;

    pub fn new(kind: ProcessErrorKind, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.is_empty() {
            message.push_str("Git process failed");
        }
        if message.len() > Self::MAX_MESSAGE_BYTES {
            let mut boundary = Self::MAX_MESSAGE_BYTES;
            while !message.is_char_boundary(boundary) {
                boundary -= 1;
            }
            message.truncate(boundary);
        }
        Self { kind, message }
    }

    pub const fn kind(&self) -> ProcessErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProcessError {}

#[cfg(test)]
mod tests {
    use super::{ProcessError, ProcessErrorKind};

    #[test]
    fn message_bounds_preserve_utf8_and_nonempty_errors() {
        let error = ProcessError::new(ProcessErrorKind::ProcessLaunch, "€".repeat(2000));
        assert_eq!(error.message().len(), 4095);
        assert_eq!(error.kind(), ProcessErrorKind::ProcessLaunch);
        assert_eq!(
            ProcessError::new(ProcessErrorKind::ProcessFailed, "").message(),
            "Git process failed"
        );
    }
}
