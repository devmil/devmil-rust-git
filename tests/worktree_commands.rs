use devmil_git::activity::ActivityId;
use devmil_git::commands::worktree::{remove_arguments, RemoveError};
use devmil_git::process::{ActivityGitRunner, GitProcessCommand, PrivateActivityResult};
use devmil_git::repository::{DiscoveryRequest, RepositoryDiscovery};
use devmil_git::types::{RepositoryPath, WorktreeId};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Fixture {
    root: PathBuf,
    current: WorktreeId,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "devmil-worktree-remove-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" }
                )
                .status()
                .unwrap()
                .success());
        };
        run(&["init", "-q", "-b", "main"]);
        run(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ]);
        let current = RepositoryDiscovery::default()
            .discover(
                &DiscoveryRequest::new(RepositoryPath::new(&repo).unwrap()),
                &mut |command| {
                    ActivityGitRunner::run(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
                },
            )
            .unwrap()
            .worktree;
        Self { root, current }
    }
    fn git(&self, args: &[OsString]) {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(self.current.worktree_root().as_path())
            .status()
            .unwrap()
            .success());
    }
    fn linked(&self, name: &str) -> PathBuf {
        let target = self.root.join(name);
        self.git(&[
            "worktree".into(),
            "add".into(),
            "--detach".into(),
            target.as_os_str().to_owned(),
        ]);
        target
    }
    fn plan(&self, target: &Path) -> Result<Vec<OsString>, RemoveError> {
        remove_arguments(
            OsStr::new("git"),
            &self.current,
            target,
            &mut execute,
            &AtomicBool::new(false),
        )
    }
    fn registrations(&self) -> usize {
        let output = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(self.current.worktree_root().as_path())
            .output()
            .unwrap();
        assert!(output.status.success());
        output
            .stdout
            .split(|b| *b == b'\n')
            .filter(|line| line.starts_with(b"worktree "))
            .count()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn execute(command: GitProcessCommand) -> PrivateActivityResult {
    ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
}

#[test]
fn removes_only_the_selected_missing_registration_without_force_or_prune() {
    let fixture = Fixture::new();
    let selected = fixture.linked("missing-selected");
    let other = fixture.linked("missing-other");
    std::fs::remove_dir_all(&selected).unwrap();
    std::fs::remove_dir_all(&other).unwrap();
    let args = fixture.plan(&selected).unwrap();
    assert_eq!(
        &args[..3],
        &[OsString::from("worktree"), "remove".into(), "--".into()]
    );
    assert!(devmil_git::filesystem::same_worktree_path(
        Path::new(&args[3]),
        &selected
    ));
    fixture.git(&args);
    assert_eq!(fixture.registrations(), 2);
    assert_eq!(fixture.plan(&selected), Err(RemoveError::Stale));
    assert!(fixture.plan(&other).is_ok());
}
#[test]
fn protects_current_dirty_and_locked_worktrees_including_missing_locked_roots() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.plan(fixture.current.worktree_root().as_path()),
        Err(RemoveError::Protected)
    );
    let target = fixture.linked("linked");
    std::fs::write(target.join("untracked"), b"keep").unwrap();
    assert_eq!(fixture.plan(&target), Err(RemoveError::Dirty));
    std::fs::remove_file(target.join("untracked")).unwrap();
    fixture.git(&[
        "worktree".into(),
        "lock".into(),
        target.as_os_str().to_owned(),
    ]);
    std::fs::remove_dir_all(&target).unwrap();
    assert_eq!(fixture.plan(&target), Err(RemoveError::Locked));
    assert_eq!(fixture.registrations(), 2);
}
#[test]
fn refuses_a_recreated_directory_and_preserves_its_files() {
    let fixture = Fixture::new();
    let target = fixture.linked("recreated");
    std::fs::remove_dir_all(&target).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("important"), b"keep").unwrap();
    assert_eq!(fixture.plan(&target), Err(RemoveError::Stale));
    assert_eq!(std::fs::read(target.join("important")).unwrap(), b"keep");
}
#[test]
fn removes_clean_worktrees_and_rejects_relative_or_unregistered_targets() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.plan(Path::new("relative")),
        Err(RemoveError::InvalidTarget)
    );
    assert_eq!(
        fixture.plan(&fixture.root.join("unknown")),
        Err(RemoveError::Stale)
    );
    let target = fixture.linked("clean");
    fixture.git(&fixture.plan(&target).unwrap());
    assert!(!target.exists());
    assert_eq!(fixture.registrations(), 1);
}
#[cfg(unix)]
#[test]
fn refuses_a_replaced_symlink_and_keeps_the_other_worktree() {
    let fixture = Fixture::new();
    let target = fixture.linked("replaced");
    let other = fixture.linked("other");
    std::fs::remove_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&other, &target).unwrap();
    assert_eq!(fixture.plan(&target), Err(RemoveError::Stale));
    assert!(other.join(".git").exists());
}

#[cfg(unix)]
#[test]
fn resolves_missing_registration_backpointers_through_an_existing_symlink_parent() {
    let fixture = Fixture::new();
    let parent = fixture.root.join("real-parent");
    std::fs::create_dir(&parent).unwrap();
    let alias = fixture.root.join("alias-parent");
    std::os::unix::fs::symlink(&parent, &alias).unwrap();
    let target = parent.join("stale");
    fixture.git(&[
        "worktree".into(),
        "add".into(),
        "--detach".into(),
        target.as_os_str().to_owned(),
    ]);
    let marker = std::fs::read_to_string(target.join(".git")).unwrap();
    let admin = PathBuf::from(marker.trim().strip_prefix("gitdir: ").unwrap());
    std::fs::write(
        admin.join("gitdir"),
        format!("{}\n", alias.join("stale/.git").display()),
    )
    .unwrap();
    std::fs::remove_dir_all(&target).unwrap();
    fixture.git(&fixture.plan(&target).unwrap());
    assert_eq!(fixture.registrations(), 1);
}
