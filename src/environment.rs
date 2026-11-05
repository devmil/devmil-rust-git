//! Desktop-launch environment repair for tools installed by the user's shell setup.

use std::ffi::{OsStr, OsString};
#[cfg(all(unix, any(test, target_os = "macos")))]
use std::io::Read as _;
#[cfg(all(unix, any(test, target_os = "macos")))]
use std::os::unix::process::CommandExt as _;
use std::path::Path;
#[cfg(all(unix, any(test, target_os = "macos")))]
use std::process::{Command, Stdio};
#[cfg(all(unix, any(test, target_os = "macos")))]
use std::thread;
#[cfg(any(test, target_os = "macos"))]
use std::time::Duration;
#[cfg(all(unix, any(test, target_os = "macos")))]
use std::time::Instant;

#[cfg(any(test, target_os = "macos"))]
const PATH_MARKER: &str = "__DEVMIL_GIT_LOGIN_PATH__";
#[cfg(target_os = "macos")]
const REEXEC_MARKER: &str = "DEVMIL_GIT_LOGIN_PATH_IMPORTED";
#[cfg(target_os = "macos")]
const MACOS_TOOL_DIRECTORIES: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];
#[cfg(any(test, target_os = "macos"))]
const INTERACTIVE_LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(any(test, target_os = "macos"))]
const LOGIN_SHELL_FALLBACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Opts into replacing the macOS process with itself using the resolved PATH.
///
/// Call before creating threads or GUI state. Retains arguments and uses a marker
/// to prevent repeated restarts. If PATH is unchanged or restart fails, returns.
/// Does nothing on other platforms.
///
/// Finder-launched applications inherit launchd's environment, which commonly omits Homebrew's
/// bin directory even though Terminal and zsh can find `git-lfs`. Git commands themselves remain
/// direct child processes. Only this bounded startup probe uses a shell.
pub fn prepare_desktop_process_environment() {
    #[cfg(target_os = "macos")]
    if std::env::var_os(REEXEC_MARKER).is_none() {
        let inherited_path = std::env::var_os("PATH");
        let Some(path) = resolve_desktop_path() else {
            return;
        };
        if inherited_path.as_ref() == Some(&path) {
            return;
        }
        let Ok(executable) = std::env::current_exe() else {
            return;
        };
        let error = Command::new(executable)
            .args(std::env::args_os().skip(1))
            .env("PATH", path)
            .env(REEXEC_MARKER, "1")
            .exec();
        eprintln!("Application could not restart with the login-shell PATH: {error}");
    }
}

/// Resolves PATH for Git child processes without changing the application environment.
///
/// On macOS, probes the interactive login shell, then the login shell on failure,
/// with respective limits of 15 and 5 seconds. Existing Homebrew directories are
/// appended when absent. Other platforms return the inherited PATH.
/// Shell startup files run with the current user's privileges. Resolve once on
/// startup or a worker thread and reuse the result for child commands.
pub fn resolve_desktop_path() -> Option<OsString> {
    #[cfg(target_os = "macos")]
    {
        let login_path = login_shell_path();
        let inherited_path = std::env::var_os("PATH");
        let directories = MACOS_TOOL_DIRECTORIES
            .iter()
            .map(Path::new)
            .filter(|directory| directory.is_dir())
            .collect::<Vec<_>>();
        compose_desktop_path(
            login_path.as_deref(),
            inherited_path.as_deref(),
            &directories,
        )
    }
    #[cfg(not(target_os = "macos"))]
    std::env::var_os("PATH")
}

#[cfg(target_os = "macos")]
fn login_shell_path() -> Option<OsString> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/zsh"));
    if !Path::new(&shell).is_absolute() || !Path::new(&shell).is_file() {
        return None;
    }
    // The interactive login shell is authoritative because users commonly extend PATH from
    // interactive startup files. This runs before application state exists, so no Git command can
    // observe the incomplete desktop environment while the bounded probe is running.
    let shell = Path::new(&shell);
    probe_shell_path(shell, &["-ilc"], INTERACTIVE_LOGIN_SHELL_TIMEOUT)
        .or_else(|| probe_shell_path(shell, &["-lc"], LOGIN_SHELL_FALLBACK_TIMEOUT))
}

#[cfg(all(unix, any(test, target_os = "macos")))]
fn probe_shell_path(shell: &Path, mode: &[&str], timeout: Duration) -> Option<OsString> {
    probe_shell_command(
        shell,
        mode,
        "printf '__DEVMIL_GIT_LOGIN_PATH__%s\\n' \"$PATH\"",
        timeout,
    )
}

#[cfg(all(unix, any(test, target_os = "macos")))]
fn probe_shell_command(
    shell: &Path,
    mode: &[&str],
    command: &str,
    timeout: Duration,
) -> Option<OsString> {
    use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
    use rustix::process::{kill_process_group, Pid, Signal};
    // A separate session lets cleanup stop children spawned by shell profiles and leaves the
    // shell without a controlling terminal, as after a launch from the desktop. With the
    // terminal of a launch from a command line or a CI pseudo-terminal, an interactive shell
    // in a background group takes the terminal, is stopped, and ends at the time limit.
    struct Probe(std::process::Child);
    impl Drop for Probe {
        fn drop(&mut self) {
            if let Some(pid) = i32::try_from(self.0.id()).ok().and_then(Pid::from_raw) {
                let _ = kill_process_group(pid, Signal::KILL);
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut probe = Command::new(shell);
    probe
        .args(mode)
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: the hook runs in the forked child before exec and only calls setsid(2), which
    // is async-signal-safe, allocates nothing and takes no lock.
    #[allow(unsafe_code)]
    unsafe {
        probe.pre_exec(|| {
            rustix::process::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    let mut child = Probe(probe.spawn().ok()?);
    let mut stdout = child.0.stdout.take()?;
    let flags = fcntl_getfl(&stdout).ok()?;
    fcntl_setfl(&stdout, flags | OFlags::NONBLOCK).ok()?;
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut status = None;
    loop {
        // Bound each drain pass and total retained output. Nonblocking reads
        // also prevent waiting for descendants that inherit the output pipe.
        for _ in 0..16 {
            let mut buffer = [0u8; 4096];
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if output.len() + count > 1024 * 1024 {
                        return None;
                    }
                    output.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        if let Some(success) = status {
            return if success {
                parse_login_shell_path(std::str::from_utf8(&output).ok()?).map(OsString::from)
            } else {
                None
            };
        }
        status = child.0.try_wait().ok()?.map(|status| status.success());
        if Instant::now() >= deadline {
            return None;
        }
        if status.is_none() {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Prefers the shell PATH and appends tool directories that are not already present.
///
/// Preserves entry order. The caller selects which directories to include.
/// Returns None when no entries exist or an entry cannot be joined into PATH.
pub fn compose_desktop_path(
    login_path: Option<&OsStr>,
    inherited_path: Option<&OsStr>,
    tool_directories: &[&Path],
) -> Option<OsString> {
    let mut entries = login_path
        .or(inherited_path)
        .map(|path| {
            std::env::split_paths(path)
                .map(std::path::PathBuf::into_os_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for directory in tool_directories {
        if !entries.iter().any(|entry| Path::new(entry) == *directory) {
            entries.push(directory.as_os_str().to_os_string());
        }
    }
    (!entries.is_empty())
        .then(|| std::env::join_paths(entries).ok())
        .flatten()
}

#[cfg(any(test, target_os = "macos"))]
fn parse_login_shell_path(output: &str) -> Option<&str> {
    output.lines().rev().find_map(|line| {
        let path = line.strip_prefix(PATH_MARKER)?;
        (!path.is_empty() && std::env::split_paths(OsStr::new(path)).next().is_some())
            .then_some(path)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        compose_desktop_path, parse_login_shell_path, INTERACTIVE_LOGIN_SHELL_TIMEOUT,
        LOGIN_SHELL_FALLBACK_TIMEOUT,
    };
    #[cfg(unix)]
    use super::{probe_shell_command, probe_shell_path};
    use std::ffi::OsString;
    use std::path::Path;

    fn joined(entries: &[&str]) -> OsString {
        std::env::join_paths(entries).unwrap()
    }

    #[test]
    fn absent_path_and_directories_do_not_add_current_directory() {
        assert_eq!(compose_desktop_path(None, None, &[]), None);
        assert_eq!(
            compose_desktop_path(None, None, &[Path::new("/tools")]),
            Some(joined(&["/tools"]))
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_drains_more_than_a_pipe_buffer_without_deadlocking() {
        let command =
            "head -c 131072 /dev/zero; printf '\\n__DEVMIL_GIT_LOGIN_PATH__/expected/bin\\n'";
        assert_eq!(
            probe_shell_command(
                Path::new("/bin/sh"),
                &["-c"],
                command,
                std::time::Duration::from_secs(3)
            ),
            Some(OsString::from("/expected/bin"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_does_not_wait_for_inherited_stdout() {
        let start = std::time::Instant::now();
        let command = "sleep 30 & printf '__DEVMIL_GIT_LOGIN_PATH__/expected/bin\\n'";
        assert_eq!(
            probe_shell_command(
                Path::new("/bin/sh"),
                &["-c"],
                command,
                std::time::Duration::from_secs(2)
            ),
            Some(OsString::from("/expected/bin"))
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn probe_timeout_and_output_limit_are_bounded() {
        let start = std::time::Instant::now();
        assert!(probe_shell_command(
            Path::new("/bin/sh"),
            &["-c"],
            "sleep 30",
            std::time::Duration::from_millis(50)
        )
        .is_none());
        assert!(probe_shell_command(
            Path::new("/bin/sh"),
            &["-c"],
            "head -c 2097152 /dev/zero",
            std::time::Duration::from_secs(2)
        )
        .is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_probe_reads_shell_path() {
        assert!(
            probe_shell_path(Path::new("/bin/sh"), &["-c"], LOGIN_SHELL_FALLBACK_TIMEOUT).is_some()
        );
    }

    /// Runs inside the pseudo-terminal that the test below provides.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "started by interactive_probe_finishes_with_a_controlling_terminal"]
    fn interactive_probe_inside_a_terminal() {
        use std::os::unix::process::CommandExt as _;
        assert!(std::fs::File::open("/dev/tty").is_ok());
        // The hazard: an interactive shell in a background group of this session is stopped.
        let mut stopped = std::process::Command::new("/bin/zsh")
            .args(["-ilc", "true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(stopped.try_wait().unwrap().is_none());
        stopped.kill().unwrap();
        stopped.wait().unwrap();

        let started = std::time::Instant::now();
        assert!(probe_shell_path(
            Path::new("/bin/zsh"),
            &["-ilc"],
            std::time::Duration::from_secs(10)
        )
        .is_some());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// An application started from a terminal or a CI pseudo-terminal must not wait for the
    /// probe's time limit. `script` gives the inner test a controlling terminal.
    #[cfg(target_os = "macos")]
    #[test]
    fn interactive_probe_finishes_with_a_controlling_terminal() {
        let output = std::process::Command::new("/usr/bin/script")
            .args(["-q", "/dev/null"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "environment::tests::interactive_probe_inside_a_terminal",
                "--ignored",
                "--test-threads=1",
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && text.contains("1 passed"),
            "probe inside a terminal failed: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_has_no_controlling_terminal_and_leads_its_own_group() {
        // `ps` prints the terminal, the process group and the process of the shell.
        let command = "printf '__DEVMIL_GIT_LOGIN_PATH__'; ps -o tty=,pgid=,pid= -p $$";
        let report = probe_shell_command(
            Path::new("/bin/sh"),
            &["-c"],
            command,
            std::time::Duration::from_secs(3),
        )
        .unwrap();
        let report = report.to_string_lossy().into_owned();
        let fields = report.split_whitespace().collect::<Vec<_>>();
        assert!(matches!(fields[0], "??" | "?" | "-"), "{report}");
        assert_eq!(fields[1], fields[2], "{report}");
    }

    #[test]
    fn interactive_shell_has_time_to_finish_before_the_reduced_fallback_probe() {
        assert_eq!(INTERACTIVE_LOGIN_SHELL_TIMEOUT.as_secs(), 15);
        assert_eq!(LOGIN_SHELL_FALLBACK_TIMEOUT.as_secs(), 5);
    }

    #[test]
    fn login_shell_path_ignores_profile_output_and_requires_marker() {
        assert_eq!(
            parse_login_shell_path(
                "profile text\n__DEVMIL_GIT_LOGIN_PATH__/opt/homebrew/bin:/usr/bin\n"
            ),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(parse_login_shell_path("/opt/homebrew/bin:/usr/bin\n"), None);
        assert_eq!(parse_login_shell_path("__DEVMIL_GIT_LOGIN_PATH__\n"), None);
    }

    #[test]
    fn desktop_path_prefers_login_shell_and_adds_missing_tool_directories() {
        let login_path = joined(&["/shell/bin", "/usr/bin"]);
        let inherited_path = joined(&["/inherited/bin", "/usr/bin"]);
        let path = compose_desktop_path(
            Some(&login_path),
            Some(&inherited_path),
            &[Path::new("/opt/homebrew/bin"), Path::new("/shell/bin")],
        )
        .unwrap();

        assert_eq!(
            path,
            joined(&["/shell/bin", "/usr/bin", "/opt/homebrew/bin"])
        );
    }

    #[test]
    fn desktop_path_keeps_inherited_path_when_the_shell_probe_fails() {
        let inherited_path = joined(&["/usr/bin", "/bin"]);
        let path = compose_desktop_path(
            None,
            Some(&inherited_path),
            &[Path::new("/opt/homebrew/bin")],
        )
        .unwrap();

        assert_eq!(path, joined(&["/usr/bin", "/bin", "/opt/homebrew/bin"]));
    }
}
