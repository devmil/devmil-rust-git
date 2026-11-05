use devmil_git::{
    diff::model::DiffPath,
    loading::{
        content::{ContentError, ContentLoader, ContentRequest},
        working_diff::DiffEndpoint,
    },
    Repository, RepositoryOptions,
};
use std::{
    process::Command,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};
#[test]
#[allow(clippy::too_many_lines)]
fn content_reads_exact_endpoints_bounds_cache_and_refreshes_mutable_bytes() {
    let root = std::env::temp_dir().join(format!("devmil-content-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    std::fs::write(root.join("file"), b"old").unwrap();
    std::fs::write(root.join("empty"), b"").unwrap();
    git(&["add", "file"]);
    git(&["add", "empty"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "--no-gpg-sign",
        "-m",
        "old",
    ]);
    let model = Repository::new(
        &root,
        RepositoryOptions {
            watch: false,
            polling_fallback: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    model.load().unwrap();
    let path = DiffPath::from_bytes(b"file".to_vec()).unwrap();
    let empty = model
        .content(
            DiffEndpoint::Head,
            DiffPath::from_bytes(b"empty".to_vec()).unwrap(),
            0,
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert!(empty.as_ref().as_ref().unwrap().bytes.is_empty());
    let first = model
        .content(DiffEndpoint::Head, path.clone(), 3)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref().as_ref().unwrap().bytes, b"old");
    let cached = model
        .content(DiffEndpoint::Head, path.clone(), 3)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&first, &cached));
    assert_eq!(
        model
            .content(DiffEndpoint::Head, path.clone(), 2)
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap_err()
            .kind,
        devmil_git::model::RepositoryErrorKind::TooLarge
    );
    let working = model
        .content(DiffEndpoint::WorkingTree, path.clone(), 10)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(working.as_ref().as_ref().unwrap().bytes, b"old");
    std::fs::write(root.join("file"), b"new").unwrap();
    model
        .reload()
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let updated = model
        .content(DiffEndpoint::WorkingTree, path.clone(), 10)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(updated.as_ref().as_ref().unwrap().bytes, b"new");
    let missing = model
        .content(
            DiffEndpoint::WorkingTree,
            DiffPath::from_bytes(b"missing".to_vec()).unwrap(),
            10,
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_err();
    assert_eq!(
        missing.kind,
        devmil_git::model::RepositoryErrorKind::MissingObject
    );
    let worktree = model
        .snapshot()
        .repository
        .as_ref()
        .unwrap()
        .worktree
        .clone();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("file"), root.join("link")).unwrap();
        assert!(Command::new("mkfifo")
            .arg(root.join("fifo"))
            .status()
            .unwrap()
            .success());
        for name in ["link", "fifo"] {
            let unsafe_path = DiffPath::from_bytes(name.as_bytes().to_vec()).unwrap();
            let request = ContentRequest {
                worktree: &worktree,
                endpoint: &DiffEndpoint::WorkingTree,
                path: &unsafe_path,
                byte_limit: 10,
            };
            assert_eq!(
                ContentLoader::new("git")
                    .read(&request, &AtomicBool::new(false), &mut |_| panic!(
                        "worktree read launched Git"
                    ))
                    .unwrap_err(),
                ContentError::InvalidPath
            );
        }
    }
    let request = ContentRequest {
        worktree: &worktree,
        endpoint: &DiffEndpoint::Head,
        path: &path,
        byte_limit: 3,
    };
    assert_eq!(
        ContentLoader::new("git")
            .read(&request, &AtomicBool::new(true), &mut |_| panic!(
                "canceled read launched Git"
            ))
            .unwrap_err(),
        ContentError::Canceled
    );
    model.close();
    #[cfg(unix)]
    {
        std::fs::remove_file(root.join("fifo")).unwrap();
        git(&[
            "config",
            "filter.upper.smudge",
            "if test -f uppercase; then tr a-z A-Z; else cat; fi",
        ]);
        std::fs::write(root.join(".gitattributes"), b"file filter=upper\n").unwrap();
        filtered_worktree_refresh(&root);
        attributes_refreshes_commit_diff(&root);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
fn filtered_worktree_refresh(root: &std::path::Path) {
    use devmil_git::loading::graph::{GraphHistoryOrder, GraphTraversalScope};
    let model = Repository::new(
        root,
        RepositoryOptions {
            debounce: Duration::from_millis(10),
            polling_fallback: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    let wait = Duration::from_secs(10);
    let first = model
        .content(
            DiffEndpoint::Head,
            DiffPath::from_bytes(b"file".to_vec()).unwrap(),
            10,
        )
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref().as_ref().unwrap().bytes, b"old");
    // macOS delivers the fixture's own earlier Git writes late, as a full refresh. Let them
    // settle so the write below is the only change the history cache sees.
    let mut stable = model.snapshot().revision;
    let mut quiet_since = std::time::Instant::now();
    let settle_deadline = std::time::Instant::now() + wait;
    while quiet_since.elapsed() < Duration::from_millis(1500) {
        assert!(
            std::time::Instant::now() < settle_deadline,
            "repository did not settle"
        );
        std::thread::sleep(Duration::from_millis(50));
        let current = model.snapshot().revision;
        if current != stable {
            stable = current;
            quiet_since = std::time::Instant::now();
        }
    }
    let history = model
        .history(
            GraphHistoryOrder::Topological,
            GraphTraversalScope::AllReferences,
        )
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    let revision = model.snapshot().revision;
    std::fs::write(root.join("uppercase"), b"enabled\n").unwrap();
    let deadline = std::time::Instant::now() + wait;
    while model.snapshot().revision == revision {
        assert!(
            std::time::Instant::now() < deadline,
            "watcher did not refresh filter input"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = model
        .content(
            DiffEndpoint::Head,
            DiffPath::from_bytes(b"file".to_vec()).unwrap(),
            10,
        )
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert_eq!(after.as_ref().as_ref().unwrap().bytes, b"OLD");
    let retained = model
        .history(
            GraphHistoryOrder::Topological,
            GraphTraversalScope::AllReferences,
        )
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert!(
        Arc::ptr_eq(&history, &retained),
        "test must exercise a worktree-only refresh"
    );
    model.close();
}

#[cfg(unix)]
fn attributes_refreshes_commit_diff(root: &std::path::Path) {
    use devmil_git::loading::commit::{CommitDiffBase, CommitDiffOptions};
    let output = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let id = devmil_git::types::ObjectId::parse(String::from_utf8(output.stdout).unwrap().trim())
        .unwrap();
    let model = Repository::new(
        root,
        RepositoryOptions {
            debounce: Duration::from_millis(10),
            polling_fallback: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    let wait = Duration::from_secs(10);
    let before = model
        .commit_diff(
            id.clone(),
            CommitDiffBase::EmptyTree,
            CommitDiffOptions::default(),
        )
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert!(before.files.iter().any(|file| !file.hunks.is_empty()));
    let revision = model.snapshot().revision;
    std::fs::write(root.join(".gitattributes"), b"file -diff\n").unwrap();
    let deadline = std::time::Instant::now() + wait;
    while model.snapshot().revision == revision {
        assert!(
            std::time::Instant::now() < deadline,
            "watcher did not refresh Git attributes"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = model
        .commit_diff(id, CommitDiffBase::EmptyTree, CommitDiffOptions::default())
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert!(
        !Arc::ptr_eq(&before, &after),
        "attributes change must invalidate cached fixed-commit diffs"
    );
    assert!(
        after.files.iter().all(|file| file.hunks.is_empty()),
        "Git attributes must change file representation to binary"
    );
    model.close();
}
