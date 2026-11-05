//! Immutable commit comparison caching with complete display-option identity.
use super::{ReadTicket, Repository, RepositoryError, RepositoryErrorKind};
use crate::loading::comparison::{
    ComparisonLimits, ComparisonLoader, ComparisonQuery, ComparisonResult,
};
impl Repository {
    /// Loads an ordered comparison, sharing identical reads and retaining bounded parsed output.
    /// # Errors
    /// Returns repository lifecycle, scheduling or Git failures; failures are never cached.
    pub fn comparison(
        &self,
        query: ComparisonQuery,
        limits: ComparisonLimits,
    ) -> Result<ReadTicket<ComparisonResult>, RepositoryError> {
        let mut key = b"comparison-v1".to_vec();
        for bytes in [
            query.base.as_str().as_bytes(),
            query.target.as_str().as_bytes(),
        ] {
            key.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            key.extend_from_slice(bytes);
        }
        key.extend_from_slice(&query.context.value().to_be_bytes());
        key.extend_from_slice(&[query.whitespace as u8, query.layout as u8]);
        for value in [
            limits.raw_output_bytes,
            limits.patch_output_bytes,
            limits.patch.max_bytes,
            limits.patch.max_lines,
            limits.patch.preview_line_limit as usize,
        ] {
            key.extend_from_slice(&value.to_be_bytes());
        }
        let executable = self.shared.options.git_executable.clone();
        self.schedule_weighted(
            key,
            true,
            |result: &ComparisonResult| {
                std::mem::size_of_val(result)
                    + result.activity_ids.capacity()
                        * std::mem::size_of::<crate::activity::ActivityId>()
                    + result.outcome.as_ref().map_or_else(
                        |error| error.detail().len().saturating_mul(2),
                        |value| super::commit::diff_bytes(&value.diff).saturating_add(256),
                    )
            },
            move |repository, executor, cancellation| {
                let result =
                    ComparisonLoader::new(executable, &repository.worktree, cancellation, executor)
                        .with_limits(limits)
                        .load(&query);
                if let Err(error) = &result.outcome {
                    use crate::loading::comparison::ComparisonFailureKind as Kind;
                    let kind = match error.kind {
                        Kind::Canceled => RepositoryErrorKind::Canceled,
                        Kind::MissingObject => RepositoryErrorKind::MissingObject,
                        Kind::TooLarge => RepositoryErrorKind::TooLarge,
                        Kind::MalformedOutput => RepositoryErrorKind::InvalidOutput,
                        Kind::InvalidSelection => RepositoryErrorKind::InvalidOptions,
                        _ => RepositoryErrorKind::Read,
                    };
                    return Err(RepositoryError::new(kind, error.to_string())
                        .with_activity(error.activity_id()));
                }
                Ok(result)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    #[test]
    fn ordered_comparisons_share_cache_and_keep_reverse_semantics() {
        let root =
            std::env::temp_dir().join(format!("devmil-comparison-model-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            result.stdout
        };
        git(&["init", "-b", "main"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ]);
        let base = crate::types::ObjectId::parse(
            String::from_utf8(git(&["rev-parse", "HEAD"]))
                .unwrap()
                .trim(),
        )
        .unwrap();
        std::fs::write(root.join("added"), "new\n").unwrap();
        git(&["add", "added"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "target",
        ]);
        let target = crate::types::ObjectId::parse(
            String::from_utf8(git(&["rev-parse", "HEAD"]))
                .unwrap()
                .trim(),
        )
        .unwrap();
        let repository = Repository::new(
            &root,
            super::super::RepositoryOptions {
                watch: false,
                ..Default::default()
            },
        )
        .unwrap();
        let query = ComparisonQuery {
            base,
            target,
            context: crate::diff::model::ContextLines::DEFAULT,
            whitespace: crate::diff::model::DiffWhitespacePolicy::Preserve,
            layout: crate::diff::model::DiffLayout::Unified,
        };
        let first = repository
            .comparison(query.clone(), ComparisonLimits::default())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(first.activity_ids.len(), 3);
        assert_eq!(
            first.outcome.as_ref().unwrap().diff.files[0].kind,
            crate::diff::model::FileChangeKind::Added
        );
        let second = repository
            .comparison(query.clone(), ComparisonLimits::default())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let reversed = ComparisonQuery {
            base: query.target.clone(),
            target: query.base.clone(),
            ..query
        };
        let reverse = repository
            .comparison(reversed, ComparisonLimits::default())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(
            reverse.outcome.as_ref().unwrap().diff.files[0].kind,
            crate::diff::model::FileChangeKind::Deleted
        );
        repository.close();
        std::fs::remove_dir_all(root).unwrap();
    }
}
