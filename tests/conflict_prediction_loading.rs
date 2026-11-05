use devmil_git::loading::conflict_prediction::{
    ConflictPredictionErrorKind, ConflictPredictionKind, ConflictPredictionLoader,
    ConflictPredictionRequest, ConflictPredictionTarget,
};
use devmil_git::types::RefId;
use devmil_git::{Repository, RepositoryOptions};
use std::{
    path::Path,
    process::Command,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};
fn git(path: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(path)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
fn snapshot(path: &Path) -> Vec<String> {
    [
        &["rev-parse", "HEAD"][..],
        &["show-ref"][..],
        &["status", "--porcelain=v2"][..],
        &["count-objects", "-v"][..],
    ]
    .iter()
    .map(|args| git(path, args))
    .collect()
}
#[test]
fn model_prediction_is_cached_and_preserves_repository_objects_refs_and_worktree() {
    let path = std::env::temp_dir().join(format!("devmil-conflict-model-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    git(&path, &["init", "-b", "main"]);
    git(&path, &["config", "user.name", "Fixture"]);
    git(&path, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(path.join("tracked"), "base\n").unwrap();
    git(&path, &["add", "tracked"]);
    git(&path, &["commit", "--no-gpg-sign", "-m", "base"]);
    git(&path, &["checkout", "-b", "topic"]);
    std::fs::write(path.join("tracked"), "topic\n").unwrap();
    git(&path, &["commit", "--no-gpg-sign", "-am", "topic"]);
    git(&path, &["checkout", "main"]);
    std::fs::write(path.join("tracked"), "main\n").unwrap();
    git(&path, &["commit", "--no-gpg-sign", "-am", "main"]);
    let before = snapshot(&path);
    let model = Repository::new(
        &path,
        RepositoryOptions {
            watch: false,
            polling_fallback: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    model.load().unwrap();
    let target = ConflictPredictionTarget::Ref(RefId::parse("refs/heads/topic").unwrap());
    for kind in [
        ConflictPredictionKind::Merge,
        ConflictPredictionKind::Rebase,
    ] {
        let first = model
            .predict_conflicts(kind, target.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(first.conflicting_paths.len(), 1);
        assert_eq!(first.conflicting_paths[0].as_path(), Path::new("tracked"));
        let cached = model
            .predict_conflicts(kind, target.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &cached));
        assert_eq!(snapshot(&path), before);
    }
    let worktree = model
        .snapshot()
        .repository
        .as_ref()
        .unwrap()
        .worktree
        .clone();
    let canceled = ConflictPredictionLoader::new("git")
        .predict(
            &ConflictPredictionRequest {
                repository: worktree,
                kind: ConflictPredictionKind::Merge,
                target,
            },
            &AtomicBool::new(true),
            &mut |_| panic!("pre-canceled prediction launched Git"),
        )
        .unwrap_err();
    assert_eq!(canceled.kind(), ConflictPredictionErrorKind::Canceled);
    model.close();
    std::fs::remove_dir_all(path).unwrap();
}
