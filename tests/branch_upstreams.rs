mod support;

use devmil_git::loading::refs::ReferenceLoader;
use support::{executor, Fixture};

#[test]
fn upstream_remote_names_with_slashes_and_unmapped_refspecs_are_exact() {
    let fixture = Fixture::new("upstreams");
    let head = fixture.head();
    fixture.git(&[
        "remote",
        "add",
        "team/origin",
        "https://example.invalid/repo.git",
    ]);
    fixture.git(&["update-ref", "refs/remotes/team/origin/feature/x", &head]);
    fixture.git(&["branch", "topic"]);
    fixture.git(&["branch", "--set-upstream-to=team/origin/feature/x", "topic"]);
    // A remote whose fetch refspec does not map the merge ref: Git reports no upstream at all.
    fixture.git(&[
        "remote",
        "add",
        "mirror",
        "https://example.invalid/mirror.git",
    ]);
    fixture.git(&[
        "config",
        "--replace-all",
        "remote.mirror.fetch",
        "+refs/heads/only:refs/remotes/mirror/only",
    ]);
    fixture.git(&["branch", "unmapped"]);
    fixture.git(&["config", "branch.unmapped.remote", "mirror"]);
    fixture.git(&["config", "branch.unmapped.merge", "refs/heads/elsewhere"]);
    fixture.git(&["branch", "local"]);
    fixture.git(&["branch", "--set-upstream-to=main", "local"]);

    let mut run = executor();
    let rows = ReferenceLoader::new("git")
        .branch_upstreams(&fixture.repo(), &mut run)
        .unwrap();
    let describe: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                String::from_utf8(row.branch.as_bytes().to_vec()).unwrap(),
                row.tracking
                    .as_ref()
                    .map(|tracking| String::from_utf8(tracking.as_bytes().to_vec()).unwrap()),
                String::from_utf8(row.remote.as_bytes().to_vec()).unwrap(),
                String::from_utf8(row.remote_ref.as_bytes().to_vec()).unwrap(),
            )
        })
        .collect();
    assert_eq!(
        describe,
        [
            (
                "refs/heads/local".to_owned(),
                Some("refs/heads/main".to_owned()),
                ".".to_owned(),
                "refs/heads/main".to_owned()
            ),
            (
                "refs/heads/topic".to_owned(),
                Some("refs/remotes/team/origin/feature/x".to_owned()),
                "team/origin".to_owned(),
                "refs/heads/feature/x".to_owned()
            ),
        ]
    );
}
