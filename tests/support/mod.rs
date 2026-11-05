//! Real-Git temporary repositories shared by the integration tests.
#![allow(dead_code)]

use devmil_git::activity::ActivityId;
use devmil_git::process::{ActivityGitRunner, GitProcessCommand, PrivateActivityResult};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A temporary directory removed on drop, holding a repository at `repo()`.
pub struct Fixture {
    root: PathBuf,
}

pub fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Runs Git with an isolated configuration and returns stdout; panics on failure.
pub fn git_in(directory: &Path, args: &[&str]) -> Vec<u8> {
    let output = git_output(directory, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

pub fn git_output(directory: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("LC_ALL", "C")
        .output()
        .unwrap()
}

impl Fixture {
    /// Creates `<tmp>/<name>-<pid>-<n>/repo` with one commit on `main`.
    pub fn new(name: &str) -> Self {
        let fixture = Self::empty(name);
        git_in(&fixture.repo(), &["init", "-q", "-b", "main"]);
        fixture.write("base.txt", b"base\n");
        fixture.git(&["add", "base.txt"]);
        fixture.git(&["commit", "-qm", "base"]);
        fixture
    }

    /// Creates the directory layout without running `git init`.
    pub fn empty(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "devmil-git-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("repo")).unwrap();
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    pub fn git(&self, args: &[&str]) -> String {
        String::from_utf8(git_in(&self.repo(), args)).unwrap()
    }

    pub fn git_status(&self, args: &[&str]) -> std::process::Output {
        git_output(&self.repo(), args)
    }

    pub fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.repo().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    pub fn read(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.repo().join(path)).unwrap()
    }

    pub fn commit_file(&self, path: &str, bytes: &[u8], message: &str) -> String {
        self.write(path, bytes);
        self.git(&["add", "--", path]);
        self.git(&["commit", "-qm", message]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// An executor that runs commands through the shared runner with isolated global configuration.
pub fn executor() -> impl FnMut(GitProcessCommand) -> PrivateActivityResult {
    |command: GitProcessCommand| {
        let command = isolate(command);
        ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
    }
}

pub fn isolate(command: GitProcessCommand) -> GitProcessCommand {
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .unwrap()
        .env("GIT_CONFIG_GLOBAL", null_config())
        .unwrap()
        .env("GIT_AUTHOR_NAME", "Fixture")
        .unwrap()
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .unwrap()
        .env("GIT_COMMITTER_NAME", "Fixture")
        .unwrap()
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .unwrap()
}

/// Runs an argv built by the crate with the shared runner and returns the private result.
pub fn run_argv(
    directory: &Path,
    argv: &[String],
    environment: &[(String, String)],
    stdin: Option<Vec<u8>>,
) -> PrivateActivityResult {
    let mut command = GitProcessCommand::git("git").current_dir(directory).args(
        argv.iter()
            .map(devmil_git::process::ProcessArgument::visible),
    );
    for (name, value) in environment {
        command = command.env(name, value).unwrap();
    }
    if let Some(stdin) = stdin {
        command = command.stdin_bytes(stdin);
    }
    executor()(command)
}

pub fn succeeded(result: &PrivateActivityResult) -> bool {
    matches!(
        result.activity.status,
        devmil_git::activity::ActivityStatus::Succeeded { .. }
    )
}
