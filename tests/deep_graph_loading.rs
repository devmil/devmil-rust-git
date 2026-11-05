use devmil_git::activity::{ActivityId, ActivityStatus};
use devmil_git::loading::deep_graph::{
    DeepGraphErrorKind, DeepGraphLoader, DiffContentSearchOptions, DiffContentSearchRequest,
    DiffContentSearchScope, SearchCompleteness,
};
use devmil_git::process::{
    ActivityGitRunner, GitProcessCommand, PrivateActivityResult, ProcessArgument,
};
use devmil_git::{Repository, RepositoryOptions};
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;

fn execute(command: GitProcessCommand) -> PrivateActivityResult {
    ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
}

#[test]
#[allow(clippy::too_many_lines)]
fn search_options_keep_case_and_revision_scope_distinct() {
    let path = std::env::temp_dir().join(format!("devmil-search-options-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let git = |args: &[&str]| {
        let result = execute(
            GitProcessCommand::git("git")
                .current_dir(&path)
                .args(args.iter().copied().map(ProcessArgument::visible)),
        );
        assert!(matches!(
            result.activity.status,
            ActivityStatus::Succeeded { .. }
        ));
        String::from_utf8_lossy(&result.stdout.bytes)
            .trim()
            .to_owned()
    };
    git(&["init", "--quiet"]);
    std::fs::write(path.join("file.txt"), "base").unwrap();
    git(&["add", "file.txt"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "--no-gpg-sign",
        "-m",
        "base",
    ]);
    let original_branch = git(&["branch", "--show-current"]);
    git(&["checkout", "--quiet", "-b", "side"]);
    std::fs::write(path.join("file.txt"), "Needle").unwrap();
    git(&["add", "file.txt"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "--no-gpg-sign",
        "-m",
        "side",
    ]);
    git(&["checkout", "--quiet", &original_branch]);
    std::fs::write(path.join("file.txt"), "needle").unwrap();
    git(&["add", "file.txt"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "--no-gpg-sign",
        "-m",
        "current",
    ]);
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
    let run = |options| {
        model
            .search_diff_content_with_options("Needle".into(), 10, 10, options)
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap()
    };
    assert_eq!(run(DiffContentSearchOptions::default()).matches.len(), 2);
    assert_eq!(
        run(DiffContentSearchOptions {
            case_sensitive: true,
            scope: DiffContentSearchScope::AllRefs,
        })
        .matches
        .len(),
        1
    );
    assert_eq!(
        run(DiffContentSearchOptions {
            case_sensitive: true,
            scope: DiffContentSearchScope::CurrentBranch,
        })
        .matches
        .len(),
        0
    );
    assert_eq!(
        run(DiffContentSearchOptions {
            case_sensitive: false,
            scope: DiffContentSearchScope::CurrentBranch,
        })
        .matches
        .len(),
        1
    );
    model.close();
    std::fs::remove_dir_all(path).unwrap();
}
#[test]
#[allow(clippy::too_many_lines)]
fn real_search_is_bounded_cached_and_bisect_discovery_survives_restart() {
    let path = std::env::temp_dir().join(format!("devmil-deep-graph-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let git = |args: &[&str]| {
        let result = execute(
            GitProcessCommand::git("git")
                .current_dir(&path)
                .args(args.iter().copied().map(ProcessArgument::visible)),
        );
        assert!(
            matches!(result.activity.status, ActivityStatus::Succeeded { .. }),
            "{:?}",
            result.activity
        );
        String::from_utf8_lossy(&result.stdout.bytes)
            .trim()
            .to_owned()
    };
    git(&["init", "--quiet"]);
    let mut ids = Vec::new();
    for (n, text) in ["plain", "needle.* literal", "other", "needle.* changed"]
        .iter()
        .enumerate()
    {
        std::fs::write(path.join("file.txt"), text).unwrap();
        git(&["add", "file.txt"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "-m",
            &format!("commit {n}"),
        ]);
        ids.push(git(&["rev-parse", "HEAD"]));
    }
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
    let first = model
        .search_diff_content("needle.*".into(), 10, 10)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(first.examined_commits, 4);
    assert_eq!(first.matches.len(), 3);
    assert_eq!(first.completeness, SearchCompleteness::Complete);
    let cached = model
        .search_diff_content("needle.*".into(), 10, 10)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&first, &cached));
    let bounded = model
        .search_diff_content("needle.*".into(), 1, 10)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(bounded.examined_commits, 1);
    assert_eq!(bounded.completeness, SearchCompleteness::CommitLimitReached);
    let limited = model
        .search_diff_content("needle.*".into(), 10, 1)
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(limited.matches.len(), 1);
    assert_eq!(limited.completeness, SearchCompleteness::ResultLimitReached);
    assert!(
        !model
            .bisect_state()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap()
            .active
    );
    let worktree = model
        .snapshot()
        .repository
        .as_ref()
        .unwrap()
        .worktree
        .clone();
    let loader = DeepGraphLoader::new("git");
    let canceled = AtomicBool::new(true);
    assert_eq!(
        loader
            .discover(&worktree, &canceled, &mut |_| panic!(
                "canceled discovery launched Git"
            ))
            .unwrap_err()
            .kind(),
        DeepGraphErrorKind::Canceled
    );
    let request = DiffContentSearchRequest {
        repository: worktree,
        query: "needle.*".into(),
        maximum_commits: 10,
        maximum_results: 10,
    };
    assert_eq!(
        loader
            .search(&request, &canceled, &mut |_| panic!(
                "canceled search launched Git"
            ))
            .unwrap_err()
            .kind(),
        DeepGraphErrorKind::Canceled
    );
    model.close();
    git(&["bisect", "start", &ids[3], &ids[0]]);
    let restarted = Repository::new(
        &path,
        RepositoryOptions {
            watch: false,
            polling_fallback: false,
            ..RepositoryOptions::default()
        },
    )
    .unwrap();
    restarted.load().unwrap();
    let state = restarted
        .bisect_state()
        .unwrap()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert!(state.active);
    assert_eq!(state.original_head.as_ref().unwrap().as_str(), ids[3]);
    assert!(state.current.is_some());
    assert_eq!(state.timeline.len(), 2);
    restarted.close();
    git(&["bisect", "reset"]);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn standard_refs_search_skips_commits_only_tool_references_reach() {
    let path = std::env::temp_dir().join(format!("devmil-search-ref-scope-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let git = |args: &[&str]| {
        let result = execute(
            GitProcessCommand::git("git")
                .current_dir(&path)
                .args(args.iter().copied().map(ProcessArgument::visible)),
        );
        assert!(matches!(
            result.activity.status,
            ActivityStatus::Succeeded { .. }
        ));
    };
    let commit = |file: &str, subject: &str| {
        std::fs::write(path.join(file), "needle").unwrap();
        git(&["add", file]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "-m",
            subject,
        ]);
    };
    git(&["init", "--quiet"]);
    commit("branch.txt", "branch");
    commit("checkpoint.txt", "checkpoint");
    git(&["update-ref", "refs/t3/checkpoint", "HEAD"]);
    git(&["reset", "--quiet", "--hard", "HEAD~1"]);
    let search = |scope| {
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
        let result = model
            .search_diff_content_with_options(
                "needle".into(),
                10,
                10,
                DiffContentSearchOptions {
                    case_sensitive: true,
                    scope,
                },
            )
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        model.close();
        result.matches.len()
    };
    assert_eq!(search(DiffContentSearchScope::AllRefs), 2);
    assert_eq!(search(DiffContentSearchScope::StandardRefs), 1);
    // A detached HEAD is searched even when no standard reference reaches it.
    git(&["checkout", "--quiet", "--detach", "refs/t3/checkpoint"]);
    assert_eq!(search(DiffContentSearchScope::StandardRefs), 2);
    std::fs::remove_dir_all(path).unwrap();
}
