mod support;

use devmil_git::ignore::{add_ignore_pattern, exact_pattern, IgnoreError, IgnoreOutcome};
use support::Fixture;

fn ignored(fixture: &Fixture, path: &str) -> bool {
    fixture
        .git_status(&["check-ignore", "--no-index", "-q", "--", path])
        .status
        .success()
}

#[test]
fn an_exact_pattern_ignores_only_its_own_path() {
    let fixture = Fixture::new("ignore-exact");
    let mut names = vec!["[x].md", "#hash", "!bang", "deep/dir/file.txt"];
    // Windows file names cannot hold Git's other wildcards.
    if cfg!(not(windows)) {
        names.extend(["a*b.txt", "q?.log"]);
    }
    for name in names {
        fixture.write(name, b"x");
        let pattern = exact_pattern(name.as_bytes(), false).unwrap();
        assert_eq!(
            add_ignore_pattern(&fixture.repo(), &pattern).unwrap(),
            IgnoreOutcome::Added
        );
        assert!(
            ignored(&fixture, name),
            "{name} should be ignored by {pattern}"
        );
    }
    for decoy in [
        "aXb.txt",
        "qq.log",
        "x.md",
        "deep/dir/file.txt.bak",
        "other/a*b.txt",
    ] {
        assert!(!ignored(&fixture, decoy), "{decoy} must stay visible");
    }
}

#[test]
fn a_directory_pattern_ignores_the_directory_and_repeats_are_skipped() {
    let fixture = Fixture::new("ignore-directory");
    fixture.write(".gitignore", b"*.tmp\r\n");
    fixture.write("build/out/app.bin", b"x");
    let pattern = exact_pattern(b"build/out", true).unwrap();
    assert_eq!(
        add_ignore_pattern(&fixture.repo(), &pattern).unwrap(),
        IgnoreOutcome::Added
    );
    assert_eq!(
        add_ignore_pattern(&fixture.repo(), &pattern).unwrap(),
        IgnoreOutcome::AlreadyPresent
    );
    assert_eq!(fixture.read(".gitignore"), b"*.tmp\r\n/build/out/\r\n");
    assert!(ignored(&fixture, "build/out/app.bin"));
    assert!(!ignored(&fixture, "build/app.bin"));
}

#[cfg(unix)]
#[test]
fn a_linked_ignore_file_is_refused_unchanged() {
    let fixture = Fixture::new("ignore-symlink");
    fixture.write("elsewhere", b"keep\n");
    std::os::unix::fs::symlink("elsewhere", fixture.repo().join(".gitignore")).unwrap();
    assert_eq!(
        add_ignore_pattern(&fixture.repo(), "/x"),
        Err(IgnoreError::UnsafeFile)
    );
    assert_eq!(fixture.read("elsewhere"), b"keep\n");
}
