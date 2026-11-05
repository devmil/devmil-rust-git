//! Typed external-tool templates, variable substitution, discovery, and native launch.
//!
//! Definitions execute their exact native argument vector. Explicit shell definitions are
//! caller-selected; the launcher never inserts a shell. Run waiting tools off the UI thread.

mod model;
mod platform;
pub use model::*;
pub use platform::{IntegrationPlatform, SystemIntegrationPlatform};
