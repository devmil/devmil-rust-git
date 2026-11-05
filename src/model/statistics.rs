//! Cached, bounded statistics using already loaded reference and status data.
use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::loading::statistics::{
    self, RepositoryStatistics, RepositoryStatisticsError, StatisticsCounts, StatisticsOptions,
};

impl Repository {
    /// Loads statistics, sharing coherent reference/status counts with the repository snapshot.
    /// # Errors
    /// Returns lifecycle, scheduling, command or bounded-output failures.
    pub fn statistics(&self) -> Result<ReadTicket<RepositoryStatistics>, RepositoryError> {
        let executable = self.shared.options.git_executable.clone();
        let shared = super::Arc::clone(&self.shared);
        self.schedule_background_weighted(
            b"statistics-v1".to_vec(),
            false,
            |result: &RepositoryStatistics| {
                std::mem::size_of_val(result)
                    .saturating_add(super::pages::activities(&result.activities))
            },
            move |repository, executor, cancellation| {
                let snapshot = super::Arc::clone(&super::lock(&shared.state).snapshot);
                let bare = repository.kind == crate::repository::RepositoryKind::Bare;
                let known_counts = snapshot_counts(&snapshot, bare);
                statistics::load(
                    &executable,
                    &repository.worktree,
                    StatisticsOptions { bare, known_counts },
                    cancellation,
                    executor,
                )
                .map_err(|error| {
                    let kind = match error {
                        RepositoryStatisticsError::Canceled => RepositoryErrorKind::Canceled,
                        RepositoryStatisticsError::TimedOut => RepositoryErrorKind::TimedOut,
                        RepositoryStatisticsError::GitFailed => RepositoryErrorKind::Read,
                        RepositoryStatisticsError::MalformedOutput => {
                            RepositoryErrorKind::InvalidOutput
                        }
                        RepositoryStatisticsError::ResourceLimit => RepositoryErrorKind::TooLarge,
                    };
                    RepositoryError::new(kind, error.to_string())
                })
            },
        )
    }
}

fn snapshot_counts(snapshot: &super::RepositorySnapshot, bare: bool) -> Option<StatisticsCounts> {
    if snapshot.refs_failure.is_some() || snapshot.status_failure.is_some() {
        return None;
    }
    let mut branches = None;
    let mut tags = None;
    for section in snapshot.refs.iter() {
        match section {
            crate::loading::refs::RefsSectionData::Branches { rows, .. } => {
                branches = Some(rows.len());
            }
            crate::loading::refs::RefsSectionData::Tags(rows) => tags = Some(rows.len()),
            _ => (),
        }
    }
    let branches = branches?;
    let tags = tags?;
    let changed = if bare {
        0
    } else {
        snapshot
            .status
            .as_ref()?
            .entries
            .iter()
            .filter(|entry| !matches!(entry, crate::status::StatusEntry::Ignored(_)))
            .count()
    };
    let local_branch_count = branches.min(statistics::MAX_MANAGER_REFS);
    Some(StatisticsCounts {
        local_branch_count,
        tag_count: tags.min(statistics::MAX_MANAGER_REFS - local_branch_count),
        changed_path_count: changed.min(statistics::MAX_MANAGER_CHANGED_PATHS),
        incomplete: branches.saturating_add(tags) > statistics::MAX_MANAGER_REFS
            || changed > statistics::MAX_MANAGER_CHANGED_PATHS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    #[test]
    fn statistics_reuses_snapshot_counts_and_cached_read() {
        let root = std::env::temp_dir().join(format!("devmil-statistics-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap()
                .status
                .success());
        }
        std::fs::create_dir_all(root.join("untracked")).unwrap();
        std::fs::write(root.join("untracked/a"), "a").unwrap();
        std::fs::write(root.join("untracked/b"), "b").unwrap();
        let repository = Repository::new(
            &root,
            super::super::RepositoryOptions {
                watch: false,
                ..Default::default()
            },
        )
        .unwrap();
        let first = repository
            .statistics()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                first.commit_count,
                first.local_branch_count,
                first.changed_path_count
            ),
            (1, 1, 2)
        );
        assert_eq!(first.activities.len(), 1);
        assert_eq!(first.activities[0].command.arguments[0].display, "rev-list");
        let second = repository
            .statistics()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        repository.close();
        std::fs::remove_dir_all(root).unwrap();
    }
}
