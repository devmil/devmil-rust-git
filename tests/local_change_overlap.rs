mod support;

use devmil_git::merge::local_changes::local_change_overlap;
use devmil_git::probes::changed_paths;
use devmil_git::status::parse_porcelain_v2;
use devmil_git::types::ObjectId;
use std::ffi::OsStr;
use support::{executor, Fixture};

fn oid(value: &str) -> ObjectId {
    ObjectId::parse(value.trim()).unwrap()
}

fn names(paths: &[devmil_git::diff::model::DiffPath]) -> Vec<String> {
    paths
        .iter()
        .map(|path| String::from_utf8(path.as_bytes().to_vec()).unwrap())
        .collect()
}

/// The fixture: `f.txt` changed by `target`, then `g.txt` changed afterwards.
fn fixture(name: &str) -> (Fixture, ObjectId) {
    let fixture = Fixture::new(name);
    fixture.commit_file("f.txt", b"a\nb\nc\n", "add f");
    fixture.commit_file("g.txt", b"x\n", "add g");
    let target = oid(&fixture.commit_file("f.txt", b"a\nB\nc\n", "change f"));
    fixture.commit_file("g.txt", b"y\n", "change g");
    (fixture, target)
}

fn overlap(
    fixture: &Fixture,
    target: &ObjectId,
) -> devmil_git::merge::local_changes::LocalChangeOverlap {
    let mut run = executor();
    let changed = changed_paths(OsStr::new("git"), &fixture.repo(), target, &mut run).unwrap();
    let status = fixture.git(&["status", "--porcelain=v2", "--branch", "-z"]);
    let (_, entries) = parse_porcelain_v2(status.as_bytes()).unwrap();
    local_change_overlap(&entries, &changed)
}

#[test]
fn changed_paths_lists_root_rename_and_nested_paths() {
    let fixture = Fixture::new("changed-paths");
    let git = OsStr::new("git");
    let mut run = executor();
    let root = oid(&fixture.git(&["rev-list", "--max-parents=0", "HEAD"]));
    assert_eq!(
        names(&changed_paths(git, &fixture.repo(), &root, &mut run).unwrap()),
        ["base.txt"]
    );
    std::fs::create_dir_all(fixture.repo().join("dir")).unwrap();
    fixture.git(&["mv", "base.txt", "dir/moved.txt"]);
    fixture.git(&["commit", "-qm", "move"]);
    let moved = oid(&fixture.head());
    assert_eq!(
        names(&changed_paths(git, &fixture.repo(), &moved, &mut run).unwrap()),
        ["base.txt", "dir/moved.txt"]
    );
}

/// A case name, how it changes the fixture, and whether Git reverts in place.
type Case<'a> = (&'a str, &'a dyn Fn(&Fixture), bool);

/// The classification agrees with Git: an empty overlap reverts in place and keeps the local
/// changes; every reported overlap is one Git refuses before changing anything.
#[test]
fn overlap_predicts_whether_git_reverts_in_place() {
    let cases: [Case<'_>; 5] = [
        (
            "unstaged-elsewhere",
            &|f| f.write("g.txt", b"y\nlocal\n"),
            true,
        ),
        (
            "untracked-elsewhere",
            &|f| f.write("new/file.txt", b"n\n"),
            true,
        ),
        (
            "unstaged-same-file",
            &|f| f.write("f.txt", b"a\nB\nc\nd\n"),
            false,
        ),
        (
            "staged-elsewhere",
            &|f| {
                f.write("g.txt", b"y\nstaged\n");
                f.git(&["add", "g.txt"]);
            },
            false,
        ),
        (
            "untracked-replacement",
            &|f| {
                f.git(&["rm", "-q", "f.txt"]);
                f.git(&["commit", "-qm", "drop f"]);
                f.write("f.txt", b"untracked\n");
            },
            false,
        ),
    ];
    for (name, prepare, in_place) in cases {
        let (fixture, target) = fixture(name);
        prepare(&fixture);
        let before = fixture.git(&["status", "--porcelain=v2", "-z"]);
        let head = fixture.head();
        let overlap = overlap(&fixture, &target);
        assert_eq!(overlap.is_empty(), in_place, "{name}: {overlap:?}");
        let reverted = fixture.git_status(&["revert", "--no-edit", target.as_str()]);
        if in_place {
            assert!(reverted.status.success(), "{name}");
            assert_eq!(fixture.read("f.txt"), b"a\nb\nc\n", "{name}");
        } else {
            // Git refused before writing: HEAD and the local changes are untouched.
            assert!(!reverted.status.success(), "{name}");
            assert_eq!(fixture.head(), head, "{name}");
            assert_eq!(
                fixture.git(&["status", "--porcelain=v2", "-z"]),
                before,
                "{name}"
            );
        }
    }
}
