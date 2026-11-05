use devmil_git::activity::{ActivityEvent, ActivityId, ActivityRecord, ActivityStatus};
use devmil_git::process::{ActivityGitRunner, GitProcessCommand, ProcessArgument};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static NEXT_REPOSITORY: AtomicU64 = AtomicU64::new(0);

struct Repository(PathBuf);

impl Repository {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "devmil-git-test-{}-{}",
            std::process::id(),
            NEXT_REPOSITORY.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        let repository = Self(path);
        let record = repository.run(&["init", "--quiet"]);
        assert!(matches!(record.status, ActivityStatus::Succeeded { .. }));
        repository
    }

    fn command(&self, args: &[&str]) -> GitProcessCommand {
        GitProcessCommand::git("git")
            .current_dir(&self.0)
            .args(args.iter().copied().map(ProcessArgument::visible))
    }

    fn run(&self, args: &[&str]) -> ActivityRecord {
        ActivityGitRunner::run(
            self.command(args),
            ActivityId::new(1).unwrap(),
            Some(&AtomicBool::new(false)),
            &mut |_| {},
        )
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn real_status_preserves_nul_framing_and_activity_lifecycle() {
    let repository = Repository::new();
    std::fs::write(repository.0.join("space name.txt"), b"content").unwrap();
    let id = ActivityId::new(42).unwrap();
    let mut events = Vec::new();
    let record = ActivityGitRunner::run(
        repository.command(&["status", "--porcelain=v1", "-z"]),
        id,
        Some(&AtomicBool::new(false)),
        &mut |event| events.push(event),
    );
    assert_eq!(record.stdout.bytes, b"?? space name.txt\0");
    assert!(
        matches!(events.first(), Some(ActivityEvent::Created(created)) if created.id == id && created.status == ActivityStatus::Running)
    );
    assert!(
        matches!(events.last(), Some(ActivityEvent::Finished(finished)) if finished == &record)
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ActivityEvent::Created(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ActivityEvent::Finished(_)))
            .count(),
        1
    );
    assert!(record.finished_at_unix_ms.unwrap() >= record.started_at_unix_ms);
}

#[test]
fn failed_spawn_still_publishes_created_and_finished() {
    let repository = Repository::new();
    let mut events = Vec::new();
    let record = ActivityGitRunner::run(
        GitProcessCommand::git(repository.0.join("missing-executable")),
        ActivityId::new(9).unwrap(),
        None,
        &mut |event| events.push(event),
    );
    assert!(matches!(record.status, ActivityStatus::LaunchError { .. }));
    assert!(
        matches!(events.as_slice(), [ActivityEvent::Created(_), ActivityEvent::Finished(finished)] if finished == &record)
    );
}

#[test]
fn output_limit_retains_prefix_and_counts_discarded_bytes() {
    let repository = Repository::new();
    for index in 0..100 {
        std::fs::write(repository.0.join(format!("file-{index:03}.txt")), b"x").unwrap();
    }
    let full = repository.run(&["status", "--porcelain=v1", "-z"]);
    let bounded = ActivityGitRunner::run(
        repository
            .command(&["status", "--porcelain=v1", "-z"])
            .output_limit(17),
        ActivityId::new(10).unwrap(),
        None,
        &mut |_| {},
    );
    assert!(matches!(bounded.status, ActivityStatus::Succeeded { .. }));
    assert_eq!(bounded.stdout.bytes, full.stdout.bytes[..17]);
    assert_eq!(bounded.stdout.total_bytes, full.stdout.total_bytes);
    assert!(bounded.stdout.truncated);
}

#[test]
fn private_remote_config_output_never_reaches_public_activity() {
    let repository = Repository::new();
    let fake_url = "https://user:fixture-secret@example.invalid/repo.git";
    assert!(matches!(
        repository
            .run(&["config", "remote.origin.url", fake_url])
            .status,
        ActivityStatus::Succeeded { .. }
    ));
    let mut events = Vec::new();
    let result = ActivityGitRunner::run_private(
        repository
            .command(&["config", "--get", "remote.origin.url"])
            .redact_stdout(),
        ActivityId::new(11).unwrap(),
        None,
        &mut |event| events.push(event),
    );
    assert_eq!(result.stdout.bytes, format!("{fake_url}\n").as_bytes());
    assert!(result.activity.stdout_redacted);
    assert!(!format!("{events:?}").contains("fixture-secret"));
    assert!(!format!("{:?}", result.activity).contains("fixture-secret"));
}

#[test]
fn inherited_repository_environment_is_removed() {
    // A separate test process isolates hostile inherited environment from parallel tests.
    let repository = Repository::new();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "environment_isolation_child", "--nocapture"])
        .env("DEVMIL_GIT_ENVIRONMENT_CHILD", "1")
        .env("GIT_DIR", repository.0.join("does-not-exist"))
        .env("GIT_WORK_TREE", repository.0.join("wrong-worktree"))
        .env("GIT_INDEX_FILE", repository.0.join("wrong-index"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
}

#[test]
fn environment_isolation_child() {
    if std::env::var_os("DEVMIL_GIT_ENVIRONMENT_CHILD").is_none() {
        return;
    }
    let repository = Repository::new();
    std::fs::write(repository.0.join("own-file.txt"), b"own").unwrap();
    let status = repository.run(&["status", "--porcelain=v1", "-z"]);
    assert!(matches!(status.status, ActivityStatus::Succeeded { .. }));
    assert_eq!(status.stdout.bytes, b"?? own-file.txt\0");
}

/// Asks Git for a credential the way a transfer does, with a helper in the global
/// configuration and the askpass route the attempt selects.
#[cfg(unix)]
fn fill_credential(
    repository: &Repository,
    authentication: devmil_git::auth::TransferAuthentication<'_>,
) -> String {
    let configuration = repository.0.join("global-config");
    std::fs::write(
        &configuration,
        "[credential]\n\thelper = \"!f() { echo username=helper-user; echo password=helper-answer; }; f\"\n",
    )
    .unwrap();
    let mut command = GitProcessCommand::git("git")
        .current_dir(&repository.0)
        .args(
            devmil_git::auth::transfer_configuration(authentication)
                .iter()
                .map(|argument| ProcessArgument::visible(argument.value())),
        )
        .args(["credential", "fill"].map(ProcessArgument::visible))
        .stdin_bytes(b"protocol=https\nhost=example.invalid\n\n".to_vec())
        .redact_output()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .unwrap()
        .env("GIT_CONFIG_GLOBAL", &configuration)
        .unwrap();
    let reject = repository.0.join("missing-askpass");
    for variable in devmil_git::auth::transfer_environment(true, &reject, authentication).unwrap() {
        command = command.env(variable.name(), variable.value()).unwrap();
    }
    let result =
        ActivityGitRunner::run_private(command, ActivityId::new(12).unwrap(), None, &mut |_| {});
    assert!(matches!(
        result.activity.status,
        ActivityStatus::Succeeded { .. }
    ));
    String::from_utf8(result.stdout.bytes.clone()).unwrap()
}

#[cfg(unix)]
#[test]
fn configured_credential_helper_answers_unless_an_answer_is_supplied() {
    use devmil_git::auth::TransferAuthentication;
    use std::os::unix::fs::PermissionsExt;

    let repository = Repository::new();
    let stored = fill_credential(&repository, TransferAuthentication::Default);
    assert!(stored.contains("username=helper-user\n"), "{stored}");
    assert!(stored.contains("password=helper-answer\n"), "{stored}");

    let askpass = repository.0.join("askpass");
    std::fs::write(&askpass, "#!/bin/sh\necho supplied-answer\n").unwrap();
    std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700)).unwrap();
    let supplied = fill_credential(
        &repository,
        TransferAuthentication::ExternalAskpass {
            executable: &askpass,
        },
    );
    assert!(
        supplied.contains("password=supplied-answer\n"),
        "{supplied}"
    );
    assert!(!supplied.contains("helper-"), "{supplied}");
}

#[test]
fn resolved_apply_only_revert_retains_index_and_head() {
    let repository = Repository::new();
    for args in [
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.invalid"],
    ] {
        assert!(matches!(
            repository.run(&args).status,
            ActivityStatus::Succeeded { .. }
        ));
    }
    for (content, message) in [
        (b"base\n".as_slice(), "base"),
        (b"target\n".as_slice(), "target"),
        (b"later\n".as_slice(), "later"),
    ] {
        std::fs::write(repository.0.join("tracked.txt"), content).unwrap();
        assert!(matches!(
            repository.run(&["add", "."]).status,
            ActivityStatus::Succeeded { .. }
        ));
        assert!(matches!(
            repository.run(&["commit", "-qm", message]).status,
            ActivityStatus::Succeeded { .. }
        ));
    }
    let head = repository.run(&["rev-parse", "HEAD"]).stdout.bytes;
    let target = repository.run(&["rev-parse", "HEAD~1"]).stdout.bytes;
    let target =
        devmil_git::types::ObjectId::parse(String::from_utf8(target).unwrap().trim().to_owned())
            .unwrap();
    let args = devmil_git::merge::revert_many_arguments_with_mode(&[target], true);
    let refs: Vec<_> = args.iter().map(String::as_str).collect();
    assert!(matches!(
        repository.run(&refs).status,
        ActivityStatus::Failed { .. }
    ));
    assert!(repository.0.join(".git/REVERT_HEAD").is_file());
    std::fs::write(repository.0.join("tracked.txt"), b"base\n").unwrap();
    assert!(matches!(
        repository.run(&["add", "."]).status,
        ActivityStatus::Succeeded { .. }
    ));
    let args = devmil_git::merge::revert_no_commit_finish_arguments();
    let refs: Vec<_> = args.iter().map(String::as_str).collect();
    assert!(matches!(
        repository.run(&refs).status,
        ActivityStatus::Succeeded { .. }
    ));
    assert_eq!(repository.run(&["rev-parse", "HEAD"]).stdout.bytes, head);
    assert_eq!(
        repository.run(&["status", "--porcelain"]).stdout.bytes,
        b"M  tracked.txt\n"
    );
    assert!(!repository.0.join(".git/REVERT_HEAD").exists());
}
