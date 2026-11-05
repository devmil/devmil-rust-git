//! Platform-default discovery and native process launching for external integrations.

use super::{
    ArgumentTemplate, IntegrationCommand, IntegrationDefinition, IntegrationExecution,
    IntegrationKind, IntegrationLaunchError, IntegrationLaunchErrorKind, IntegrationLaunchMode,
    IntegrationProcessOutcome,
};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrationPlatform {
    Linux,
    MacOs,
    Windows,
    Unsupported,
}

impl IntegrationPlatform {
    #[must_use]
    pub const fn host() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Unsupported
        }
    }
}

#[derive(Clone, Debug)]
pub struct SystemIntegrationPlatform {
    platform: IntegrationPlatform,
    search_path: Vec<PathBuf>,
    bundled_runtime_root: Option<PathBuf>,
    desktop: Option<String>,
}

impl Default for SystemIntegrationPlatform {
    fn default() -> Self {
        Self {
            platform: IntegrationPlatform::host(),
            search_path: std::env::var_os("PATH")
                .as_deref()
                .map(std::env::split_paths)
                .map(Iterator::collect)
                .unwrap_or_default(),
            bundled_runtime_root: bundled_runtime_root(),
            desktop: std::env::var("XDG_CURRENT_DESKTOP")
                .ok()
                .or_else(|| std::env::var("XDG_SESSION_DESKTOP").ok()),
        }
    }
}

impl SystemIntegrationPlatform {
    /// Overrides the runtime root removed from external tools' dynamic-library and Qt paths.
    #[must_use]
    pub fn with_bundled_runtime_root(mut self, root: Option<PathBuf>) -> Self {
        self.bundled_runtime_root = root;
        self
    }

    #[must_use]
    pub fn with_search_path(platform: IntegrationPlatform, search_path: Vec<PathBuf>) -> Self {
        Self {
            platform,
            search_path,
            bundled_runtime_root: None,
            desktop: None,
        }
    }

    fn desktop_is(&self, expected: &str) -> bool {
        self.desktop.as_deref().is_some_and(|desktop| {
            desktop
                .split([':', ';'])
                .any(|part| part.eq_ignore_ascii_case(expected))
        })
    }

    fn resolve_executable(&self, executable: &Path) -> Option<PathBuf> {
        if executable.is_absolute() || executable.components().count() > 1 {
            return is_executable_file(executable).then(|| executable.to_owned());
        }
        self.search_path
            .iter()
            .map(|directory| directory.join(executable))
            .find(|candidate| is_executable_file(candidate))
    }

    fn definition(
        &self,
        id: &str,
        kind: IntegrationKind,
        executable: &str,
        arguments: &[&str],
    ) -> Option<IntegrationDefinition> {
        let executable = self.resolve_executable(Path::new(executable))?;
        let arguments = arguments
            .iter()
            .map(|argument| ArgumentTemplate::parse(argument))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        Some(IntegrationDefinition {
            id: id.to_owned(),
            kind,
            executable,
            arguments,
            execution: IntegrationExecution::Direct,
            launch_mode: kind.default_launch_mode(),
        })
    }

    fn candidate(
        &self,
        definitions: &mut Vec<IntegrationDefinition>,
        id: &str,
        kind: IntegrationKind,
        executable: &str,
        arguments: &[&str],
    ) {
        if let Some(definition) = self.definition(id, kind, executable, arguments) {
            definitions.push(definition);
        }
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

impl SystemIntegrationPlatform {
    #[expect(
        clippy::too_many_lines,
        reason = "the platform matrix stays together so priority order is auditable"
    )]
    /// Finds installed platform defaults in preference order.
    pub fn discover(&self, kind: IntegrationKind) -> Vec<IntegrationDefinition> {
        let mut definitions = Vec::new();
        match (self.platform, kind) {
            (IntegrationPlatform::Unsupported, _) => {}
            (IntegrationPlatform::Linux, IntegrationKind::Editor) => {
                self.candidate(
                    &mut definitions,
                    "linux-editor-code",
                    kind,
                    "code",
                    &["{repository}"],
                );
                self.candidate(
                    &mut definitions,
                    "linux-editor-kate",
                    kind,
                    "kate",
                    &["{repository}"],
                );
                self.candidate(
                    &mut definitions,
                    "linux-editor-gedit",
                    kind,
                    "gedit",
                    &["{repository}"],
                );
                self.candidate(
                    &mut definitions,
                    "linux-editor-default",
                    kind,
                    "xdg-open",
                    &["{repository}"],
                );
            }
            (IntegrationPlatform::Linux, IntegrationKind::Terminal) => {
                self.candidate(
                    &mut definitions,
                    "linux-terminal-gnome",
                    kind,
                    "gnome-terminal",
                    &["--working-directory={working_directory}"],
                );
                self.candidate(
                    &mut definitions,
                    "linux-terminal-konsole",
                    kind,
                    "konsole",
                    &["--workdir", "{working_directory}"],
                );
                self.candidate(
                    &mut definitions,
                    "linux-terminal-default",
                    kind,
                    "x-terminal-emulator",
                    &[],
                );
            }
            (IntegrationPlatform::Linux, IntegrationKind::FileManager) => {
                if self.desktop_is("KDE") || self.desktop_is("Plasma") {
                    self.candidate(
                        &mut definitions,
                        "linux-file-manager-dolphin",
                        kind,
                        "dolphin",
                        &["{repository}"],
                    );
                } else if self.desktop_is("GNOME") || self.desktop_is("Unity") {
                    self.candidate(
                        &mut definitions,
                        "linux-file-manager-nautilus",
                        kind,
                        "nautilus",
                        &["{repository}"],
                    );
                } else if self.desktop_is("Cinnamon") {
                    self.candidate(
                        &mut definitions,
                        "linux-file-manager-nemo",
                        kind,
                        "nemo",
                        &["{repository}"],
                    );
                } else if self.desktop_is("XFCE") {
                    self.candidate(
                        &mut definitions,
                        "linux-file-manager-thunar",
                        kind,
                        "thunar",
                        &["{repository}"],
                    );
                } else if self.desktop_is("MATE") {
                    self.candidate(
                        &mut definitions,
                        "linux-file-manager-caja",
                        kind,
                        "caja",
                        &["{repository}"],
                    );
                } else if self.desktop_is("LXQt") {
                    self.candidate(
                        &mut definitions,
                        "linux-file-manager-pcmanfm-qt",
                        kind,
                        "pcmanfm-qt",
                        &["{repository}"],
                    );
                } else if self.desktop_is("LXDE") {
                    self.candidate(
                        &mut definitions,
                        "linux-file-manager-pcmanfm",
                        kind,
                        "pcmanfm",
                        &["{repository}"],
                    );
                }
                self.candidate(
                    &mut definitions,
                    "linux-file-manager",
                    kind,
                    "xdg-open",
                    &["{repository}"],
                );
                self.candidate(
                    &mut definitions,
                    "linux-file-manager-gio",
                    kind,
                    "gio",
                    &["open", "{repository}"],
                );
                self.candidate(
                    &mut definitions,
                    "linux-file-manager-kde5",
                    kind,
                    "kde-open5",
                    &["{repository}"],
                );
                self.candidate(
                    &mut definitions,
                    "linux-file-manager-kde",
                    kind,
                    "kde-open",
                    &["{repository}"],
                );
            }
            (IntegrationPlatform::Linux, IntegrationKind::Browser) => {
                self.candidate(
                    &mut definitions,
                    "linux-browser",
                    kind,
                    "xdg-open",
                    &["{url}"],
                );
            }
            (IntegrationPlatform::MacOs, IntegrationKind::Editor) => {
                self.candidate(
                    &mut definitions,
                    "macos-editor",
                    kind,
                    "open",
                    &["-t", "{repository}"],
                );
            }
            (IntegrationPlatform::MacOs, IntegrationKind::Terminal) => {
                self.candidate(
                    &mut definitions,
                    "macos-terminal",
                    kind,
                    "open",
                    &["-a", "Terminal", "{working_directory}"],
                );
            }
            (IntegrationPlatform::MacOs, IntegrationKind::FileManager) => {
                self.candidate(
                    &mut definitions,
                    "macos-file-manager",
                    kind,
                    "open",
                    &["{repository}"],
                );
            }
            (IntegrationPlatform::MacOs, IntegrationKind::Browser) => {
                self.candidate(&mut definitions, "macos-browser", kind, "open", &["{url}"]);
            }
            (IntegrationPlatform::Windows, IntegrationKind::Editor) => {
                self.candidate(
                    &mut definitions,
                    "windows-editor-notepad",
                    kind,
                    "notepad.exe",
                    &["{file}"],
                );
            }
            (IntegrationPlatform::Windows, IntegrationKind::Terminal) => {
                self.candidate(
                    &mut definitions,
                    "windows-terminal",
                    kind,
                    "wt.exe",
                    &["-d", "{working_directory}"],
                );
            }
            (IntegrationPlatform::Windows, IntegrationKind::FileManager) => {
                self.candidate(
                    &mut definitions,
                    "windows-file-manager",
                    kind,
                    "explorer.exe",
                    &["{repository}"],
                );
            }
            (IntegrationPlatform::Windows, IntegrationKind::Browser) => {
                self.candidate(
                    &mut definitions,
                    "windows-browser",
                    kind,
                    "rundll32.exe",
                    &["url.dll,FileProtocolHandler", "{url}"],
                );
            }
            (_, IntegrationKind::DiffTool) => {
                self.candidate(
                    &mut definitions,
                    "diff-meld",
                    kind,
                    executable_name(self.platform, "meld"),
                    &["{diff_left}", "{diff_right}"],
                );
                self.candidate(
                    &mut definitions,
                    "diff-kdiff3",
                    kind,
                    executable_name(self.platform, "kdiff3"),
                    &["{diff_left}", "{diff_right}"],
                );
            }
            (_, IntegrationKind::MergeTool) => {
                self.candidate(
                    &mut definitions,
                    "merge-meld",
                    kind,
                    executable_name(self.platform, "meld"),
                    &[
                        "{merge_current}",
                        "{merge_base}",
                        "{merge_incoming}",
                        "--output={merge_result}",
                    ],
                );
                self.candidate(
                    &mut definitions,
                    "merge-kdiff3",
                    kind,
                    executable_name(self.platform, "kdiff3"),
                    &[
                        "{merge_base}",
                        "{merge_current}",
                        "{merge_incoming}",
                        "-o",
                        "{merge_result}",
                    ],
                );
            }
        }
        definitions
    }
}

fn executable_name(platform: IntegrationPlatform, basename: &'static str) -> &'static str {
    match (platform, basename) {
        (IntegrationPlatform::Windows, "meld") => "meld.exe",
        (IntegrationPlatform::Windows, "kdiff3") => "kdiff3.exe",
        (_, name) => name,
    }
}

impl SystemIntegrationPlatform {
    /// Starts the caller's direct command or explicitly selected shell command.
    ///
    /// # Errors
    /// Returns a bounded launch, wait, or nonzero-exit error.
    pub fn launch(
        &self,
        command: &IntegrationCommand,
    ) -> Result<IntegrationProcessOutcome, IntegrationLaunchError> {
        if !command.working_directory.is_dir() {
            return Err(IntegrationLaunchError {
                kind: IntegrationLaunchErrorKind::WorkingDirectoryUnavailable,
            });
        }
        if command.executable.as_os_str().is_empty() {
            return Err(IntegrationLaunchError {
                kind: IntegrationLaunchErrorKind::ExecutableUnavailable,
            });
        }

        // Direct and explicit-shell definitions share the same native argv path. The marker keeps
        // shell selection in user-owned configuration; this adapter never inserts a shell or `-c`.
        let mut process = Command::new(&command.executable);
        process
            .args(&command.arguments)
            .current_dir(&command.working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(root) = &self.bundled_runtime_root {
            remove_bundled_path(&mut process, "LD_LIBRARY_PATH", &root.join("lib"));
            remove_bundled_path(&mut process, "QT_PLUGIN_PATH", &root.join("plugins"));
            remove_bundled_path(
                &mut process,
                "QT_QPA_PLATFORM_PLUGIN_PATH",
                &root.join("plugins/platforms"),
            );
            remove_bundled_path(&mut process, "QML2_IMPORT_PATH", &root.join("qml"));
            remove_bundled_path(&mut process, "QML_IMPORT_PATH", &root.join("qml"));
        }
        let mut child = process.spawn().map_err(|error| map_spawn_error(&error))?;

        match command.launch_mode {
            IntegrationLaunchMode::Wait => {
                let status = child.wait().map_err(|_| IntegrationLaunchError {
                    kind: IntegrationLaunchErrorKind::WaitFailed,
                })?;
                if !status.success() {
                    return Err(IntegrationLaunchError {
                        kind: IntegrationLaunchErrorKind::NonZeroExit,
                    });
                }
                Ok(IntegrationProcessOutcome::Exited {
                    code: status.code(),
                })
            }
            IntegrationLaunchMode::Detached => {
                let process_id = child.id();
                std::thread::Builder::new()
                    .name("devmil-integration-reaper".to_owned())
                    .spawn(move || {
                        let _ = child.wait();
                    })
                    .map_err(|_| IntegrationLaunchError {
                        kind: IntegrationLaunchErrorKind::LaunchFailed,
                    })?;
                Ok(IntegrationProcessOutcome::Detached { process_id })
            }
        }
    }
}

fn bundled_runtime_root() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let binary_directory = executable.parent()?;
    let root = binary_directory.parent()?;
    (binary_directory.join("qt.conf").is_file() && root.join("lib").is_dir())
        .then(|| root.to_path_buf())
}

fn remove_bundled_path(command: &mut Command, variable: &str, bundled: &Path) {
    match sanitized_path_variable(std::env::var_os(variable).as_deref(), bundled) {
        Some(value) => {
            command.env(variable, value);
        }
        None => {
            command.env_remove(variable);
        }
    }
}

fn sanitized_path_variable(
    value: Option<&std::ffi::OsStr>,
    bundled: &Path,
) -> Option<std::ffi::OsString> {
    let retained = value
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|path| path != bundled)
        .collect::<Vec<_>>();
    (!retained.is_empty())
        .then(|| std::env::join_paths(retained).expect("split environment paths rejoin"))
}

fn map_spawn_error(error: &io::Error) -> IntegrationLaunchError {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => IntegrationLaunchErrorKind::ExecutableUnavailable,
        io::ErrorKind::PermissionDenied => IntegrationLaunchErrorKind::PermissionDenied,
        _ => IntegrationLaunchErrorKind::LaunchFailed,
    };
    IntegrationLaunchError { kind }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::{IntegrationVariable, IntegrationVariables};
    use std::ffi::OsString;
    use std::fs;

    fn command(executable: impl Into<PathBuf>, arguments: Vec<OsString>) -> IntegrationCommand {
        IntegrationCommand {
            executable: executable.into(),
            arguments,
            working_directory: std::env::temp_dir(),
            execution: IntegrationExecution::Direct,
            launch_mode: IntegrationLaunchMode::Wait,
        }
    }

    #[test]
    fn discovery_selects_existing_candidates_in_priority_order() {
        let root = std::env::temp_dir().join(format!(
            "devmil-git-integrations-discovery-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("kate"), []).unwrap();
        fs::write(root.join("xdg-open"), []).unwrap();
        make_executable(&root.join("kate"));
        make_executable(&root.join("xdg-open"));
        let platform = SystemIntegrationPlatform::with_search_path(
            IntegrationPlatform::Linux,
            vec![root.clone()],
        );
        let definitions = platform.discover(IntegrationKind::Editor);
        assert_eq!(
            definitions
                .iter()
                .map(|definition| definition.id.as_str())
                .collect::<Vec<_>>(),
            ["linux-editor-kate", "linux-editor-default"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_manager_discovery_covers_linux_desktops_and_macos_finder() {
        let root = std::env::temp_dir().join(format!(
            "devmil-git-file-manager-discovery-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for executable in ["xdg-open", "gio", "kde-open5", "kde-open", "open"] {
            fs::write(root.join(executable), []).unwrap();
            make_executable(&root.join(executable));
        }

        let linux = SystemIntegrationPlatform::with_search_path(
            IntegrationPlatform::Linux,
            vec![root.clone()],
        );
        assert_eq!(
            linux
                .discover(IntegrationKind::FileManager)
                .iter()
                .map(|definition| definition.id.as_str())
                .collect::<Vec<_>>(),
            [
                "linux-file-manager",
                "linux-file-manager-gio",
                "linux-file-manager-kde5",
                "linux-file-manager-kde",
            ]
        );
        let gio = &linux.discover(IntegrationKind::FileManager)[1];
        let request = crate::integrations::IntegrationRequest {
            kind: IntegrationKind::FileManager,
            working_directory: root.clone(),
            variables: IntegrationVariables::default()
                .with(IntegrationVariable::Repository, root.as_os_str()),
            configured: None,
        };
        let expanded = IntegrationCommand::expand(gio, &request).unwrap();
        assert_eq!(
            expanded.arguments,
            [OsString::from("open"), root.as_os_str().to_os_string()]
        );

        let macos = SystemIntegrationPlatform::with_search_path(
            IntegrationPlatform::MacOs,
            vec![root.clone()],
        );
        assert_eq!(
            macos
                .discover(IntegrationKind::FileManager)
                .iter()
                .map(|definition| definition.id.as_str())
                .collect::<Vec<_>>(),
            ["macos-file-manager"]
        );
        let finder =
            IntegrationCommand::expand(&macos.discover(IntegrationKind::FileManager)[0], &request)
                .unwrap();
        assert_eq!(finder.arguments, [root.as_os_str().to_os_string()]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linux_file_manager_prefers_the_current_desktop_over_folder_associations() {
        let root = std::env::temp_dir().join(format!(
            "devmil-git-desktop-file-manager-discovery-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for executable in ["dolphin", "xdg-open"] {
            fs::write(root.join(executable), []).unwrap();
            make_executable(&root.join(executable));
        }
        let platform = SystemIntegrationPlatform {
            platform: IntegrationPlatform::Linux,
            search_path: vec![root.clone()],
            bundled_runtime_root: None,
            desktop: Some("KDE:Plasma".to_owned()),
        };

        let definitions = platform.discover(IntegrationKind::FileManager);

        assert_eq!(
            definitions
                .iter()
                .map(|definition| definition.id.as_str())
                .collect::<Vec<_>>(),
            ["linux-file-manager-dolphin", "linux-file-manager"]
        );
        assert_eq!(definitions[0].executable.file_name().unwrap(), "dolphin");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovered_templates_keep_paths_with_spaces_in_one_argument() {
        let root = std::env::temp_dir().join(format!(
            "devmil-git integrations templates {}",
            std::process::id()
        ));
        let tools = root.join("tools");
        fs::create_dir_all(&tools).unwrap();
        fs::write(tools.join("meld"), []).unwrap();
        make_executable(&tools.join("meld"));
        let platform =
            SystemIntegrationPlatform::with_search_path(IntegrationPlatform::Linux, vec![tools]);
        let definition = platform
            .discover(IntegrationKind::DiffTool)
            .into_iter()
            .next()
            .unwrap();
        let request = crate::integrations::IntegrationRequest {
            kind: IntegrationKind::DiffTool,
            working_directory: root.clone(),
            variables: IntegrationVariables::default()
                .with(IntegrationVariable::DiffLeft, root.join("old file.rs"))
                .with(IntegrationVariable::DiffRight, root.join("new file.rs")),
            configured: None,
        };
        let expanded = IntegrationCommand::expand(&definition, &request).unwrap();
        assert_eq!(expanded.arguments.len(), 2);
        assert_eq!(expanded.arguments[0], root.join("old file.rs"));
        assert_eq!(expanded.arguments[1], root.join("new file.rs"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn browser_template_expands_the_planned_url_without_replacing_it_with_the_worktree() {
        let root = std::env::temp_dir().join(format!(
            "devmil-git-browser-template-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("xdg-open"), []).unwrap();
        make_executable(&root.join("xdg-open"));
        let platform = SystemIntegrationPlatform::with_search_path(
            IntegrationPlatform::Linux,
            vec![root.clone()],
        );
        let definition = platform
            .discover(IntegrationKind::Browser)
            .into_iter()
            .next()
            .unwrap();
        let url = "https://forge.example/team/project/compare/main...topic?expand=1";
        let request = crate::integrations::IntegrationRequest {
            kind: IntegrationKind::Browser,
            working_directory: root.clone(),
            variables: IntegrationVariables::default()
                .with(IntegrationVariable::Repository, root.as_os_str())
                .with(IntegrationVariable::Url, url),
            configured: None,
        };

        let expanded = IntegrationCommand::expand(&definition, &request).unwrap();

        assert_eq!(expanded.arguments, [OsString::from(url)]);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    #[test]
    fn missing_executable_and_nonzero_exit_are_typed() {
        let platform = SystemIntegrationPlatform::default();
        let missing = platform
            .launch(&command(
                std::env::temp_dir().join("devmil-git-definitely-missing-tool"),
                Vec::new(),
            ))
            .unwrap_err();
        assert_eq!(
            missing.kind,
            IntegrationLaunchErrorKind::ExecutableUnavailable
        );

        #[cfg(unix)]
        {
            let failed = platform
                .launch(&command("/usr/bin/false", Vec::new()))
                .unwrap_err();
            assert_eq!(failed.kind, IntegrationLaunchErrorKind::NonZeroExit);
        }
    }

    #[cfg(unix)]
    #[test]
    fn detached_process_returns_before_child_exit_and_is_reaped() {
        use std::time::{Duration, Instant};

        let platform = SystemIntegrationPlatform::default();
        let mut invocation = command("/bin/sleep", vec![OsString::from("0.4")]);
        invocation.launch_mode = IntegrationLaunchMode::Detached;
        let started = Instant::now();
        let outcome = platform.launch(&invocation).unwrap();
        assert!(matches!(
            outcome,
            IntegrationProcessOutcome::Detached { process_id } if process_id > 0
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[cfg(unix)]
    #[test]
    fn builder_and_launcher_preserve_non_utf8_direct_arguments() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let argument = OsString::from_vec(b"path-\xff".to_vec());
        let invocation = command("/usr/bin/true", vec![argument.clone()]);
        assert_eq!(
            invocation.arguments[0].as_os_str().as_bytes(),
            argument.as_os_str().as_bytes()
        );
        SystemIntegrationPlatform::default()
            .launch(&invocation)
            .unwrap();
    }

    #[test]
    fn external_integrations_drop_only_bundled_runtime_paths() {
        let bundled = Path::new("/opt/app/lib");
        let value =
            std::env::join_paths([Path::new("/custom/lib"), bundled, Path::new("/another/lib")])
                .unwrap();
        let sanitized = sanitized_path_variable(Some(&value), bundled).unwrap();
        assert_eq!(
            std::env::split_paths(&sanitized).collect::<Vec<_>>(),
            [PathBuf::from("/custom/lib"), PathBuf::from("/another/lib")]
        );
        assert_eq!(
            sanitized_path_variable(Some(bundled.as_os_str()), bundled),
            None
        );
        assert_eq!(sanitized_path_variable(None, bundled), None);
    }

    #[test]
    fn explicit_shell_marker_does_not_change_or_infer_argv() {
        let mut invocation = command(
            "configured-shell",
            vec![OsString::from("configured-flag"), OsString::from("command")],
        );
        invocation.execution = IntegrationExecution::ExplicitShell;
        assert_eq!(invocation.executable, PathBuf::from("configured-shell"));
        assert_eq!(
            invocation.arguments,
            [OsString::from("configured-flag"), OsString::from("command")]
        );
    }
}
