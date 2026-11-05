mod support;

use devmil_git::archive::{write_commit_zip, ArchiveError, ArchiveRequest};
use devmil_git::types::ObjectId;
use std::io::{Cursor, Read};
use std::sync::atomic::AtomicBool;
use support::Fixture;

fn request(fixture: &Fixture, commit: &str) -> ArchiveRequest {
    ArchiveRequest {
        git: "git".into(),
        repository: fixture.repo(),
        worktree: Some(fixture.repo()),
        commit: ObjectId::parse(commit).unwrap(),
        prefix: "snapshot-1".to_owned(),
        submodules: true,
        lfs: true,
    }
}

fn read_zip(bytes: Vec<u8>) -> Vec<(String, Option<u32>, Vec<u8>)> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    (0..archive.len())
        .map(|index| {
            let mut file = archive.by_index(index).unwrap();
            let mut content = Vec::new();
            file.read_to_end(&mut content).unwrap();
            (file.name().unwrap().into_owned(), file.unix_mode(), content)
        })
        .collect()
}

fn content<'a>(entries: &'a [(String, Option<u32>, Vec<u8>)], name: &str) -> &'a [u8] {
    &entries
        .iter()
        .find(|entry| entry.0 == name)
        .unwrap_or_else(|| panic!("{name} missing"))
        .2
}

#[test]
fn a_snapshot_holds_every_tracked_file_of_the_commit() {
    let fixture = Fixture::new("archive-files");
    fixture.write("src/main.rs", b"fn main() {}\n");
    fixture.write("secret.txt", b"exported anyway\n");
    fixture.write(".gitattributes", b"secret.txt export-ignore\n");
    fixture.write("run.sh", b"#!/bin/sh\n");
    fixture.git(&["add", "."]);
    fixture.git(&["update-index", "--chmod=+x", "run.sh"]);
    fixture.git(&["commit", "-qm", "files"]);
    let commit = fixture.head();
    // Later changes do not reach the snapshot.
    fixture.commit_file("src/main.rs", b"changed\n", "later");

    let (bytes, summary) = write_commit_zip(
        &request(&fixture, &commit),
        Cursor::new(Vec::new()),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    let entries = read_zip(bytes.into_inner());
    assert!(entries
        .iter()
        .all(|entry| entry.0.starts_with("snapshot-1/")));
    assert_eq!(
        content(&entries, "snapshot-1/src/main.rs"),
        b"fn main() {}\n"
    );
    assert_eq!(
        content(&entries, "snapshot-1/secret.txt"),
        b"exported anyway\n"
    );
    assert_eq!(content(&entries, "snapshot-1/base.txt"), b"base\n");
    let mode = |name: &str| {
        entries
            .iter()
            .find(|entry| entry.0 == name)
            .unwrap()
            .1
            .unwrap()
    };
    assert_eq!(mode("snapshot-1/run.sh") & 0o777, 0o755);
    assert_eq!(mode("snapshot-1/base.txt") & 0o777, 0o644);
    assert_eq!(summary.files, 5);
    assert_eq!(summary.missing_submodule_count, 0);
}

#[cfg(unix)]
#[test]
fn symbolic_links_stay_links() {
    let fixture = Fixture::new("archive-links");
    std::os::unix::fs::symlink("base.txt", fixture.repo().join("link")).unwrap();
    fixture.git(&["add", "link"]);
    fixture.git(&["commit", "-qm", "link"]);
    let (bytes, _) = write_commit_zip(
        &request(&fixture, &fixture.head()),
        Cursor::new(Vec::new()),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    let entries = read_zip(bytes.into_inner());
    let link = entries
        .iter()
        .find(|entry| entry.0 == "snapshot-1/link")
        .unwrap();
    assert_eq!(link.1.unwrap() & 0o170_000, 0o120_000);
    assert_eq!(link.2, b"base.txt");
}

#[test]
fn lfs_pointers_become_their_content_when_it_is_available() {
    let fixture = Fixture::new("archive-lfs");
    let content_bytes = b"large binary content";
    let oid = "f3c2a5a3ba1e2f7ac4dfa1c4b09ac7db2c0bb3d5c7c33c1b55bb2d8f5d7a9f10";
    let store = fixture
        .repo()
        .join(".git/lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4]);
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join(oid), content_bytes).unwrap();
    let pointer = |oid: &str, size: usize| {
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n")
    };
    fixture.write("asset.bin", pointer(oid, content_bytes.len()).as_bytes());
    let missing = "a".repeat(64);
    fixture.write("missing.bin", pointer(&missing, 7).as_bytes());
    fixture.git(&["add", "asset.bin", "missing.bin"]);
    fixture.git(&["commit", "-qm", "lfs"]);

    let (bytes, summary) = write_commit_zip(
        &request(&fixture, &fixture.head()),
        Cursor::new(Vec::new()),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    let entries = read_zip(bytes.into_inner());
    assert_eq!(content(&entries, "snapshot-1/asset.bin"), content_bytes);
    assert_eq!(
        content(&entries, "snapshot-1/missing.bin"),
        pointer(&missing, 7).as_bytes()
    );
    assert_eq!(summary.unresolved_lfs_count, 1);
    assert_eq!(
        summary.unresolved_lfs,
        vec![b"snapshot-1/missing.bin".to_vec()]
    );
}

#[test]
fn submodules_are_included_at_their_recorded_commit() {
    let library = Fixture::new("archive-library");
    let recorded = library.commit_file("lib.txt", b"recorded\n", "lib");
    let fixture = Fixture::new("archive-super");
    fixture.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "-q",
        library.repo().to_str().unwrap(),
        "vendor/lib",
    ]);
    fixture.git(&["commit", "-qm", "submodule"]);
    let commit = fixture.head();
    // The library moves on; the snapshot keeps the recorded commit.
    library.commit_file("lib.txt", b"newer\n", "newer");
    let checkout = fixture.repo().join("vendor/lib");
    support::git_in(
        &checkout,
        &[
            "-c",
            "protocol.file.allow=always",
            "pull",
            "-q",
            "origin",
            "main",
        ],
    );
    assert_ne!(
        support::git_in(&checkout, &["rev-parse", "HEAD"]).trim_ascii(),
        recorded.as_bytes()
    );

    let (bytes, summary) = write_commit_zip(
        &request(&fixture, &commit),
        Cursor::new(Vec::new()),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    let entries = read_zip(bytes.into_inner());
    assert_eq!(
        content(&entries, "snapshot-1/vendor/lib/lib.txt"),
        b"recorded\n"
    );
    assert_eq!(summary.missing_submodule_count, 0);

    // Without the submodule's repository, it stays an empty folder.
    let mut without = request(&fixture, &commit);
    without.submodules = false;
    let (bytes, _) = write_commit_zip(
        &without,
        Cursor::new(Vec::new()),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    let listed = read_zip(bytes.into_inner());
    assert!(listed
        .iter()
        .any(|entry| entry.0 == "snapshot-1/vendor/lib/"));
    assert!(!listed
        .iter()
        .any(|entry| entry.0.starts_with("snapshot-1/vendor/lib/lib")));
}

#[test]
fn unknown_commits_and_cancellation_stop_the_archive() {
    let fixture = Fixture::new("archive-stop");
    let unknown = request(&fixture, &"0".repeat(40));
    assert_eq!(
        write_commit_zip(
            &unknown,
            Cursor::new(Vec::new()),
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .unwrap_err(),
        ArchiveError::MissingCommit
    );
    assert_eq!(
        write_commit_zip(
            &request(&fixture, &fixture.head()),
            Cursor::new(Vec::new()),
            &AtomicBool::new(true),
            &mut |_| {},
        )
        .unwrap_err(),
        ArchiveError::Canceled
    );
}
