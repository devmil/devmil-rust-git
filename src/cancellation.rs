//! Consumer-owned cancellation for synchronous process execution.

/// A cancellation signal checked while a child process is running.
/// Implementations should return promptly and must not reset an observed cancellation.
pub trait Cancellation {
    fn is_cancelled(&self) -> bool;
}

impl Cancellation for std::sync::atomic::AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(std::sync::atomic::Ordering::Acquire)
    }
}
