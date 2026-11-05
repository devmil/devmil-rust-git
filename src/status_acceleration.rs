//! Git's optional status accelerators for large worktrees: the untracked cache
//! (`core.untrackedCache`) and the built-in filesystem monitor (`core.fsmonitor`).
//!
//! Reading the effective settings changes nothing. Enabling writes repository-local
//! configuration only; Git then maintains its caches itself. The built-in monitor starts a
//! per-repository daemon on first use, so callers should enable it only on the user's request.

use crate::activity::ActivityStatus;
use crate::loading::CommandExecutor;
use crate::probes::{probe_command, ProbeFailure, SMALL_PROBE_OUTPUT_BYTES};
use crate::version::GitVersion;
use std::ffi::OsStr;
use std::path::Path;

/// The first Git release whose built-in filesystem monitor runs on Windows and macOS.
pub const BUILTIN_FSMONITOR_VERSION: GitVersion = GitVersion::new(2, 37, 0);

/// The status accelerators in effect for one repository.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct StatusAcceleration {
    /// `core.untrackedCache` is true, so Git caches untracked-directory scans in the index.
    pub untracked_cache: bool,
    /// `core.fsmonitor` names the built-in monitor or a hook, so Git asks it for changes.
    pub fsmonitor: bool,
    /// This platform and Git version provide the built-in monitor.
    pub fsmonitor_available: bool,
}

impl StatusAcceleration {
    /// Whether [`enable`] would change a setting.
    #[must_use]
    pub const fn can_improve(&self) -> bool {
        !self.untracked_cache || (self.fsmonitor_available && !self.fsmonitor)
    }
}

/// Whether Git's built-in filesystem monitor exists for `version` on this platform. Linux and
/// other platforms need an external hook, which this module never configures.
#[must_use]
pub fn builtin_fsmonitor_available(version: GitVersion) -> bool {
    cfg!(any(windows, target_os = "macos")) && version >= BUILTIN_FSMONITOR_VERSION
}

/// Reads the effective `core.untrackedCache` and `core.fsmonitor` values, across every
/// configuration scope, with `git config --null --get-regexp`.
///
/// # Errors
/// Reports launch, cancellation, malformed-output and other Git failures.
pub fn read(
    executable: &OsStr,
    worktree: &Path,
    version: GitVersion,
    executor: &mut dyn CommandExecutor,
) -> Result<StatusAcceleration, ProbeFailure> {
    let mut result = executor.execute(probe_command(
        executable,
        worktree,
        &[
            "config",
            "--null",
            "--get-regexp",
            r"^core\.(untrackedcache|fsmonitor)$",
        ],
        SMALL_PROBE_OUTPUT_BYTES,
    ));
    let output = match result.activity.status {
        ActivityStatus::Succeeded { .. } if !result.stdout.truncated => result.stdout.take_bytes(),
        ActivityStatus::Succeeded { .. } => return Err(ProbeFailure::InvalidOutput),
        // `--get-regexp` exits 1 when no key matches.
        ActivityStatus::Failed { exit_code: Some(1) } => Vec::new(),
        ActivityStatus::LaunchError { .. } => return Err(ProbeFailure::MissingGit),
        ActivityStatus::Canceled { .. } | ActivityStatus::TimedOut { .. } => {
            return Err(ProbeFailure::Canceled)
        }
        _ => return Err(ProbeFailure::GitFailed),
    };
    let mut acceleration = parse(&output)?;
    acceleration.fsmonitor_available = builtin_fsmonitor_available(version);
    Ok(acceleration)
}

/// Parses `git config --null --get-regexp` output. The last value of a key wins, as in Git.
///
/// # Errors
/// Rejects entries without a key/value separator.
pub fn parse(output: &[u8]) -> Result<StatusAcceleration, ProbeFailure> {
    let mut acceleration = StatusAcceleration::default();
    for entry in output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let (key, value) = match entry.iter().position(|byte| *byte == b'\n') {
            Some(split) => (&entry[..split], &entry[split + 1..]),
            // A key without a value is boolean true in Git configuration.
            None => (entry, &b"true"[..]),
        };
        match key {
            b"core.untrackedcache" => acceleration.untracked_cache = boolean(value) == Some(true),
            // Any non-false value names the built-in monitor or a hook.
            b"core.fsmonitor" => {
                acceleration.fsmonitor = !value.is_empty() && boolean(value) != Some(false);
            }
            _ => return Err(ProbeFailure::InvalidOutput),
        }
    }
    Ok(acceleration)
}

fn boolean(value: &[u8]) -> Option<bool> {
    match value.to_ascii_lowercase().as_slice() {
        b"true" | b"yes" | b"on" | b"1" => Some(true),
        b"false" | b"no" | b"off" | b"0" | b"" => Some(false),
        _ => None,
    }
}

/// The `git config --local` argument lists that enable what `current` lacks: the untracked
/// cache always, the built-in monitor only where it is available.
#[must_use]
pub fn enable_arguments(current: StatusAcceleration) -> Vec<[&'static str; 4]> {
    let mut commands = Vec::new();
    if !current.untracked_cache {
        commands.push(["config", "--local", "core.untrackedCache", "true"]);
    }
    if current.fsmonitor_available && !current.fsmonitor {
        commands.push(["config", "--local", "core.fsmonitor", "true"]);
    }
    commands
}

/// Enables the accelerators `current` lacks in the repository's local configuration. Run it
/// under the caller's mutation lease; the next status scan populates Git's caches.
///
/// # Errors
/// Reports the first launch, cancellation or Git failure; earlier settings stay written.
pub fn enable(
    executable: &OsStr,
    worktree: &Path,
    current: StatusAcceleration,
    executor: &mut dyn CommandExecutor,
) -> Result<(), ProbeFailure> {
    for arguments in enable_arguments(current) {
        // `git config` writes take their own lock; optional locks do not apply.
        let result = executor.execute(probe_command(
            executable,
            worktree,
            &arguments,
            SMALL_PROBE_OUTPUT_BYTES,
        ));
        match result.activity.status {
            ActivityStatus::Succeeded { .. } => {}
            ActivityStatus::LaunchError { .. } => return Err(ProbeFailure::MissingGit),
            ActivityStatus::Canceled { .. } | ActivityStatus::TimedOut { .. } => {
                return Err(ProbeFailure::Canceled)
            }
            _ => return Err(ProbeFailure::GitFailed),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{ActivityGitRunner, GitProcessCommand, PrivateActivityResult};
    use std::process::Command;

    struct Runner;
    impl CommandExecutor for Runner {
        fn execute(&mut self, command: GitProcessCommand) -> PrivateActivityResult {
            ActivityGitRunner::run_private(
                command,
                crate::activity::ActivityId::new(1).unwrap(),
                None,
                &mut |_| {},
            )
        }
    }

    fn repository() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let root = std::env::temp_dir().join(format!(
            "devmil-status-acceleration-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let output = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        root
    }

    fn local(root: &Path, key: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["config", "--local", "--get", key])
            .current_dir(root)
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    #[test]
    fn parses_effective_values_with_the_last_one_winning() {
        let parsed = parse(b"core.untrackedcache\nfalse\0core.untrackedcache\ntrue\0core.fsmonitor\n/hooks/watchman\0").unwrap();
        assert!(parsed.untracked_cache);
        assert!(parsed.fsmonitor);
        let keep = parse(b"core.untrackedcache\nkeep\0core.fsmonitor\nfalse\0").unwrap();
        assert!(!keep.untracked_cache);
        assert!(!keep.fsmonitor);
        assert!(parse(b"core.untrackedcache\0").unwrap().untracked_cache);
        assert_eq!(
            parse(b"core.bare\ntrue\0"),
            Err(ProbeFailure::InvalidOutput)
        );
        assert_eq!(parse(b"").unwrap(), StatusAcceleration::default());
    }

    #[test]
    fn enables_only_missing_and_available_accelerators() {
        let none = StatusAcceleration::default();
        assert_eq!(
            enable_arguments(none),
            vec![["config", "--local", "core.untrackedCache", "true"]]
        );
        let available = StatusAcceleration {
            fsmonitor_available: true,
            ..none
        };
        assert_eq!(enable_arguments(available).len(), 2);
        assert!(available.can_improve());
        let enabled = StatusAcceleration {
            untracked_cache: true,
            fsmonitor: true,
            fsmonitor_available: true,
        };
        assert!(enable_arguments(enabled).is_empty());
        assert!(!enabled.can_improve());
        assert!(!StatusAcceleration {
            untracked_cache: true,
            ..none
        }
        .can_improve());
    }

    #[test]
    fn builtin_monitor_needs_a_supported_platform_and_git() {
        assert!(!builtin_fsmonitor_available(GitVersion::new(2, 36, 9)));
        assert_eq!(
            builtin_fsmonitor_available(GitVersion::new(2, 37, 0)),
            cfg!(any(windows, target_os = "macos"))
        );
    }

    #[test]
    fn reads_and_enables_local_configuration() {
        let root = repository();
        let git = OsStr::new("git");
        let version = GitVersion::new(2, 30, 0);
        let before = read(git, &root, version, &mut Runner).unwrap();
        assert!(!before.untracked_cache);
        assert!(!before.fsmonitor_available);
        enable(git, &root, before, &mut Runner).unwrap();
        assert_eq!(local(&root, "core.untrackedCache").as_deref(), Some("true"));
        // An old Git never gets the built-in monitor.
        assert_eq!(local(&root, "core.fsmonitor"), None);
        let after = read(git, &root, version, &mut Runner).unwrap();
        assert!(after.untracked_cache);
        assert!(!after.can_improve());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
