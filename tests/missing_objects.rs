//! A reference to an object the repository does not have (a partial copy, a pruned pack)
//! must not fail the reference snapshot or the history: Git aborts any `for-each-ref` that
//! reads such an object, so targets are peeled separately and the broken reference skipped.
mod support;

use devmil_git::activity::ActivityId;
use devmil_git::loading::graph::{
    GraphHistoryOrder, GraphLoader, GraphRequest, GraphTraversalScope,
};
use devmil_git::loading::refs::{ReferenceLoader, RefsRequest, RefsSectionData};
use devmil_git::process::ActivityGitRunner;
use devmil_git::repository::{DiscoveryRequest, RepositoryDiscovery};
use devmil_git::types::RepositoryPath;
use std::sync::atomic::AtomicBool;
use support::Fixture;

const MISSING: &str = "52fe752a2699438477a3ee5e3fffee04430d2c55";

fn broken_fixture(name: &str) -> Fixture {
    let fixture = Fixture::new(name);
    fixture.commit_file("a.txt", b"a\n", "second");
    fixture.git(&[
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
    let head = fixture.head();
    fixture.git(&["update-ref", "refs/remotes/origin/main", &head]);
    // `update-ref` refuses a missing object; a copied repository can still have one.
    let broken = fixture.repo().join(".git/refs/remotes/origin/broken");
    std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
    std::fs::write(&broken, format!("{MISSING}\n")).unwrap();
    fixture
}

fn names(rows: &[devmil_git::refs::RepositoryReference]) -> Vec<String> {
    rows.iter()
        .map(|row| String::from_utf8(row.id.as_bytes().to_vec()).unwrap())
        .collect()
}

#[test]
fn references_skip_a_missing_object_and_peel_annotated_tags() {
    let fixture = broken_fixture("missing-object-refs");
    let mut run = |command| {
        ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
    };
    let repository = RepositoryDiscovery::default()
        .discover(
            &DiscoveryRequest::new(RepositoryPath::new(fixture.repo()).unwrap()),
            &mut |command| {
                ActivityGitRunner::run(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
            },
        )
        .unwrap();
    let result = ReferenceLoader::new("git").query(
        &RefsRequest {
            worktree: repository.worktree.clone(),
            include_secondary: false,
            head_symbolic_hint: None,
        },
        &mut run,
    );
    assert_eq!(result.failure, None);
    let remotes = result
        .updates
        .iter()
        .find_map(|update| match update {
            RefsSectionData::Remotes { references, .. } => Some(names(references)),
            _ => None,
        })
        .unwrap();
    assert_eq!(remotes, ["refs/remotes/origin/main"]);
    let tags = result
        .updates
        .iter()
        .find_map(|update| match update {
            RefsSectionData::Tags(tags) => Some(tags.clone()),
            _ => None,
        })
        .unwrap();
    let [tag] = tags.as_slice() else {
        panic!("one tag expected");
    };
    let first = fixture.git(&["rev-parse", "HEAD~1"]);
    assert_eq!(
        tag.peeled_target.as_ref().map(|id| id.as_str().to_owned()),
        Some(first.trim().to_owned())
    );
    assert_ne!(tag.target.as_str(), first.trim());
}

#[test]
fn history_of_all_references_skips_a_missing_object() {
    let fixture = broken_fixture("missing-object-graph");
    let mut run = |command| {
        ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
    };
    let repository = RepositoryDiscovery::default()
        .discover(
            &DiscoveryRequest::new(RepositoryPath::new(fixture.repo()).unwrap()),
            &mut |command| {
                ActivityGitRunner::run(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
            },
        )
        .unwrap();
    let loader = GraphLoader::default().with_known_version(repository.git_version);
    let topology = loader
        .load(
            &GraphRequest {
                worktree: repository.worktree.clone(),
                order: GraphHistoryOrder::Topological,
                scope: GraphTraversalScope::AllReferences,
            },
            &AtomicBool::new(false),
            &mut run,
        )
        .unwrap();
    let commits = fixture.git(&["rev-list", "--count", "HEAD"]);
    assert_eq!(topology.rows.len().to_string(), commits.trim());
    // The annotated tag still decorates the commit it peels to.
    assert!(topology.rows.iter().any(|row| row
        .refs
        .iter()
        .any(|reference| reference.as_bytes() == b"refs/tags/v1")));
}
