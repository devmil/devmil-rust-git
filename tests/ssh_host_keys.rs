mod support;

use devmil_git::auth::{
    fingerprint_arguments, fingerprint_input, key_algorithm, keyscan_arguments, parse_fingerprint,
    select_key_line, ssh_endpoint, ssh_host_key_probe_command,
};
use devmil_git::clone::CloneSource;
use support::{executor, succeeded, Fixture};

#[test]
fn keyscan_arguments_end_options_before_the_host() {
    let endpoint = ssh_endpoint(
        &CloneSource::parse("ssh://git@forge.test:2222/team/project.git").unwrap(),
        253,
    )
    .unwrap();
    assert_eq!(
        keyscan_arguments(&endpoint, 5),
        ["-T", "5", "-p", "2222", "--", "forge.test"]
    );
}

#[test]
fn fingerprint_probe_matches_ssh_keygen_for_a_scanned_key_line() {
    if std::process::Command::new("ssh-keygen")
        .arg("-?")
        .output()
        .is_err()
    {
        eprintln!("ssh-keygen is not installed; skipping");
        return;
    }
    let fixture = Fixture::empty("ssh-fingerprint");
    let key = fixture.root().join("host_key");
    let generated = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "fixture", "-f"])
        .arg(&key)
        .output()
        .unwrap();
    assert!(generated.status.success());
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
    let mut fields = public.split_whitespace();
    let (algorithm, blob) = (fields.next().unwrap(), fields.next().unwrap());
    // A keyscan output line for a non-default port, preceded by a comment.
    let scanned =
        format!("# forge.test:2222 SSH-2.0-Fixture\n[forge.test]:2222 {algorithm} {blob}\n");
    let endpoint = ssh_endpoint(
        &CloneSource::parse("ssh://git@forge.test:2222/team/project.git").unwrap(),
        253,
    )
    .unwrap();
    let line = select_key_line(scanned.as_bytes(), &endpoint, 4096).unwrap();
    assert_eq!(key_algorithm(&line, 64, 4096).unwrap(), "ssh-ed25519");

    let mut run = executor();
    let result = run(ssh_host_key_probe_command(
        "ssh-keygen".as_ref(),
        fingerprint_arguments(),
        fingerprint_input(&line),
    ));
    assert!(succeeded(&result));
    let fingerprint = parse_fingerprint(&result.stdout.bytes, 256).unwrap();
    let expected = std::process::Command::new("ssh-keygen")
        .args(["-l", "-E", "sha256", "-f"])
        .arg(key.with_extension("pub"))
        .output()
        .unwrap();
    assert_eq!(
        parse_fingerprint(&expected.stdout, 256).unwrap(),
        fingerprint
    );
    let activity = result.activity.command;
    assert!(activity
        .environment
        .iter()
        .any(|variable| variable.name == "LC_ALL" && variable.display == "C"));
}
