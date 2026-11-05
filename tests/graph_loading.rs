use devmil_git::activity::{ActivityId, ActivityStatus};
use devmil_git::loading::graph::{
    GraphHistoryOrder, GraphLoader, GraphRequest, GraphTraversalScope,
};
use devmil_git::loading::history_index::{HistoryFilter, HistorySearchScope};
use devmil_git::process::{
    ActivityGitRunner, GitProcessCommand, PrivateActivityResult, ProcessArgument,
};
use devmil_git::repository::{DiscoveryRequest, RepositoryDiscovery};
use devmil_git::types::RepositoryPath;
use std::sync::atomic::AtomicBool;

fn execute(command: GitProcessCommand) -> PrivateActivityResult {
    ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture verifies command counts across successive cache states.
fn topology_is_lazy_and_metadata_uses_one_preflight_and_one_content_batch() {
    let path = std::env::temp_dir().join(format!("devmil-graph-loading-{}", std::process::id()));
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
    };
    git(&["init", "--quiet"]);
    for subject in ["first", "second", "third"] {
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-gpg-sign",
            "-m",
            subject,
        ]);
    }
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "tag",
        "--annotate",
        "--no-sign",
        "-m",
        "release",
        "v1",
        "HEAD~1",
    ]);
    let repository = RepositoryDiscovery::default()
        .discover(
            &DiscoveryRequest::new(RepositoryPath::new(&path).unwrap()),
            &mut |command| execute(command).activity,
        )
        .unwrap();
    let loader = GraphLoader::default().with_known_version(repository.git_version);
    let query = GraphRequest {
        worktree: repository.worktree.clone(),
        order: GraphHistoryOrder::Topological,
        scope: GraphTraversalScope::AllReferences,
    };
    let mut commands = Vec::new();
    let topology = loader
        .load(
            &query,
            &AtomicBool::new(false),
            &mut |command: GitProcessCommand| {
                commands.push(
                    command
                        .activity_command()
                        .arguments
                        .iter()
                        .map(|argument| argument.display.clone())
                        .collect::<Vec<_>>(),
                );
                execute(command)
            },
        )
        .unwrap();
    assert_eq!(topology.rows.len(), 3);
    // The annotated tag decorates the commit it peels to, not its tag object.
    assert!(topology.rows[1]
        .refs
        .iter()
        .any(|reference| reference.as_bytes() == b"refs/tags/v1"));
    assert!(topology.rows.iter().all(|row| row.metadata.is_none()));
    assert_eq!(
        commands
            .iter()
            .filter(|args| args.first().is_some_and(|arg| arg == "rev-list"))
            .count(),
        1
    );
    assert!(!commands.iter().any(|args| args
        .first()
        .is_some_and(|arg| arg == "cat-file" || arg == "--version")));
    commands.clear();
    let ids = topology
        .rows
        .iter()
        .map(|row| row.id.clone())
        .collect::<Vec<_>>();
    let metadata = loader
        .metadata(
            &repository.worktree,
            &ids,
            &AtomicBool::new(false),
            &mut |command: GitProcessCommand| {
                commands.push(
                    command
                        .activity_command()
                        .arguments
                        .iter()
                        .map(|argument| argument.display.clone())
                        .collect::<Vec<_>>(),
                );
                execute(command)
            },
        )
        .unwrap();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0], ["cat-file", "--batch-check"]);
    assert_eq!(commands[1], ["cat-file", "--batch"]);
    assert_eq!(metadata.records.len(), 3);
    assert_eq!(metadata.records[0].metadata.subject, b"third");
    assert!(metadata.missing.is_empty());
    let command_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = std::sync::Arc::clone(&command_count);
    let model = devmil_git::Repository::new(
        &path,
        devmil_git::RepositoryOptions {
            watch: false,
            activity_sink: Some(std::sync::Arc::new(move |event| {
                if matches!(event, devmil_git::activity::ActivityEvent::Created(_)) {
                    observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            })),
            ..Default::default()
        },
    )
    .unwrap();
    let wait = std::time::Duration::from_secs(10);
    let first = model
        .history(query.order, query.scope)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    let cached = model
        .history(query.order, query.scope)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&first, &cached),
        "repeated graph reads reuse the repository cache"
    );
    assert_eq!(first.rows.len(), 3);
    let prefix = model
        .history_prefix(query.order, query.scope, 1)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert_eq!(prefix.rows.len(), 1);
    assert!(!std::sync::Arc::ptr_eq(&first, &prefix));
    let hydrated = model
        .commit_metadata(ids.clone())
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    let cached = model
        .commit_metadata(ids)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&hydrated, &cached));
    assert_eq!(hydrated.records[0].metadata.subject, b"third");
    let first = model
        .history_page(query.order, query.scope, HistoryFilter::default(), 1, None)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    let before = command_count.load(std::sync::atomic::Ordering::Relaxed);
    let second = model
        .history_page(
            query.order,
            query.scope,
            HistoryFilter::default(),
            1,
            first.next.clone(),
        )
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert_eq!(second.commits, vec![topology.rows[1].id.clone()]);
    assert_eq!(
        command_count.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "continuation must not run Git"
    );
    let filter = HistoryFilter {
        text: "second".into(),
        scopes: std::collections::BTreeSet::from([HistorySearchScope::Message]),
        ..Default::default()
    };
    let found = model
        .history_page(query.order, query.scope, filter.clone(), 2, None)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert_eq!(found.commits, vec![topology.rows[1].id.clone()]);
    assert_eq!(
        command_count.load(std::sync::atomic::Ordering::Relaxed),
        before + 2,
        "complete metadata search is one preflight and one content batch"
    );
    model
        .history_page(query.order, query.scope, filter, 2, None)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert_eq!(
        command_count.load(std::sync::atomic::Ordering::Relaxed),
        before + 2,
        "repeated search reuses metadata and matching positions"
    );
    let stale = model
        .history_page(
            query.order,
            query.scope,
            HistoryFilter::default(),
            1,
            first.next.clone(),
        )
        .unwrap()
        .recv_timeout(wait)
        .unwrap();
    assert_eq!(
        stale.unwrap_err().kind,
        devmil_git::model::RepositoryErrorKind::InvalidOptions
    );
    let page = model
        .history_page(query.order, query.scope, HistoryFilter::default(), 1, None)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    model.reload().unwrap().recv_timeout(wait).unwrap().unwrap();
    let stale = model
        .history_page(
            query.order,
            query.scope,
            HistoryFilter::default(),
            1,
            page.next.clone(),
        )
        .unwrap()
        .recv_timeout(wait)
        .unwrap();
    assert_eq!(
        stale.unwrap_err().kind,
        devmil_git::model::RepositoryErrorKind::InvalidOptions
    );
    model.close();
    worktree_edit_reuses_topology(&path, &query);
    std::fs::remove_dir_all(path).unwrap();
}

fn worktree_edit_reuses_topology(path: &std::path::Path, query: &GraphRequest) {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let traversals = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&traversals);
    let model = devmil_git::Repository::new(
        path,
        devmil_git::RepositoryOptions {
            debounce: std::time::Duration::from_millis(10),
            activity_sink: Some(Arc::new(move |event| {
                if let devmil_git::activity::ActivityEvent::Created(record) = event {
                    if record
                        .command
                        .arguments
                        .first()
                        .is_some_and(|argument| argument.display == "rev-list")
                    {
                        observed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })),
            ..Default::default()
        },
    )
    .unwrap();
    let wait = std::time::Duration::from_secs(10);
    let first = model
        .history(query.order, query.scope)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    let prefix = model
        .history_prefix(query.order, query.scope, 1)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert_eq!(traversals.load(Ordering::Relaxed), 2);
    std::fs::write(path.join("untracked.txt"), b"worktree edit").unwrap();
    let deadline = std::time::Instant::now() + wait;
    while model
        .snapshot()
        .status
        .as_ref()
        .is_none_or(|status| status.entries.is_empty())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "watcher did not observe worktree edit"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let cached = model
        .history(query.order, query.scope)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    let cached_prefix = model
        .history_prefix(query.order, query.scope, 1)
        .unwrap()
        .recv_timeout(wait)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&first, &cached));
    assert!(Arc::ptr_eq(&prefix, &cached_prefix));
    assert_eq!(
        traversals.load(Ordering::Relaxed),
        2,
        "worktree-only refresh must not traverse history again"
    );
    model.close();
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers every standard namespace and both scopes.
fn standard_reference_scope_leaves_out_commits_only_other_references_reach() {
    let path = std::env::temp_dir().join(format!("devmil-graph-ref-scope-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let git = |args: &[&str]| -> String {
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
    let identity = [
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
    ];
    let commit = |subject: &str| {
        git(&[
            &identity[..],
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "--no-gpg-sign",
                "-m",
                subject,
            ],
        ]
        .concat());
        git(&["rev-parse", "HEAD"])
    };
    git(&["init", "--quiet"]);
    let base = commit("base");
    let annotated = commit("annotated");
    git(&[
        &identity[..],
        &["tag", "--annotate", "--no-sign", "-m", "release", "v1"],
    ]
    .concat());
    let lightweight = commit("lightweight");
    git(&["tag", "v2"]);
    let remote = commit("remote");
    git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let checkpoint = commit("checkpoint");
    git(&["update-ref", "refs/t3/orchestration/checkpoints/1", "HEAD"]);
    let note = commit("note");
    git(&["update-ref", "refs/notes/review", "HEAD"]);
    git(&["reset", "--quiet", "--hard", &lightweight]);
    std::fs::write(path.join("stashed.txt"), "stashed").unwrap();
    git(&["add", "stashed.txt"]);
    git(&[&identity[..], &["stash", "push", "--quiet"]].concat());
    let stash = git(&["rev-parse", "refs/stash"]);
    git(&["reset", "--quiet", "--hard", &base]);

    let repository = RepositoryDiscovery::default()
        .discover(
            &DiscoveryRequest::new(RepositoryPath::new(&path).unwrap()),
            &mut |command| execute(command).activity,
        )
        .unwrap();
    let loader = GraphLoader::default().with_known_version(repository.git_version);
    let load = |scope| {
        loader
            .load(
                &GraphRequest {
                    worktree: repository.worktree.clone(),
                    order: GraphHistoryOrder::Topological,
                    scope,
                },
                &AtomicBool::new(false),
                &mut |command: GitProcessCommand| execute(command),
            )
            .unwrap()
    };
    let rows = |scope| {
        load(scope)
            .rows
            .into_iter()
            .map(|row| {
                let refs = row
                    .refs
                    .iter()
                    .map(|reference| String::from_utf8(reference.as_bytes().to_vec()).unwrap())
                    .collect::<Vec<_>>();
                (row.id.as_str().to_owned(), refs)
            })
            .collect::<std::collections::HashMap<_, _>>()
    };

    let standard = rows(GraphTraversalScope::StandardReferences);
    for hidden in [&checkpoint, &note] {
        assert!(!standard.contains_key(hidden));
    }
    assert!(standard
        .values()
        .flatten()
        .all(|name| !name.starts_with("refs/t3/") && !name.starts_with("refs/notes/")));
    assert_eq!(standard[&annotated], ["refs/tags/v1"]);
    assert_eq!(standard[&lightweight], ["refs/tags/v2"]);
    assert_eq!(standard[&remote], ["refs/remotes/origin/main"]);
    assert_eq!(standard[&stash], ["refs/stash"]);
    assert!(standard.contains_key(&base));

    let all = rows(GraphTraversalScope::AllReferences);
    assert_eq!(all.len(), standard.len() + 2);
    assert_eq!(all[&checkpoint], ["refs/t3/orchestration/checkpoints/1"]);
    assert_eq!(all[&note], ["refs/notes/review"]);

    // A detached HEAD outside every standard reference stays in the history.
    git(&["checkout", "--quiet", "--detach", &checkpoint]);
    let detached = rows(GraphTraversalScope::StandardReferences);
    assert!(detached.contains_key(&checkpoint));
    assert!(detached[&checkpoint].is_empty());
    assert!(!detached.contains_key(&note));
    std::fs::remove_dir_all(path).unwrap();
}
