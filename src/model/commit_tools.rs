//! Repository-managed commit editor reads.
use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::loading::commit_tools::{self, CommitTools, CommitToolsErrorKind};

impl Repository {
    /// Loads recent messages and the configured commit template on the repository worker.
    /// Templates may live outside watched roots, so the result is not retained in the cache.
    /// Concurrent identical requests still share the same in-flight read.
    /// # Errors
    /// Returns lifecycle, scheduling, command or template failures.
    pub fn commit_tools(&self) -> Result<ReadTicket<CommitTools>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            b"commit-tools-v1".to_vec(),
            false,
            |_| usize::MAX,
            move |repository, executor, cancellation| {
                commit_tools::load(&executable, &repository.worktree, cancellation, executor)
                    .map_err(|error| {
                        let kind = match error.kind {
                            CommitToolsErrorKind::Canceled => RepositoryErrorKind::Canceled,
                            CommitToolsErrorKind::SpawnFailed => RepositoryErrorKind::MissingGit,
                            CommitToolsErrorKind::Failed => RepositoryErrorKind::Read,
                            CommitToolsErrorKind::InvalidOutput => {
                                RepositoryErrorKind::InvalidOutput
                            }
                            CommitToolsErrorKind::TooLarge => RepositoryErrorKind::TooLarge,
                        };
                        RepositoryError::new(kind, error.to_string())
                    })
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Command, time::Duration};

    #[test]
    fn templates_outside_the_repository_are_reloaded_without_a_refresh() {
        let root = std::env::temp_dir().join(format!("devmil-commit-tools-{}", std::process::id()));
        let worktree = root.join("repository");
        let template = root.join("template.txt");
        std::fs::create_dir_all(&worktree).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&worktree)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "subject\n\nbody",
        ]);
        git(&["config", "commit.template", template.to_str().unwrap()]);
        std::fs::write(&template, "first template").unwrap();
        let repository = Repository::new(
            &worktree,
            crate::RepositoryOptions {
                watch: false,
                ..Default::default()
            },
        )
        .unwrap();
        repository.load().unwrap();
        let first = repository
            .commit_tools()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(first.template.as_deref(), Some("first template"));
        assert_eq!(first.recent[0].description, "body");
        std::fs::write(&template, "second template").unwrap();
        let second = repository
            .commit_tools()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(second.template.as_deref(), Some("second template"));
        assert!(!std::sync::Arc::ptr_eq(&first, &second));
        repository.close();
        std::fs::remove_dir_all(root).unwrap();
    }
}
