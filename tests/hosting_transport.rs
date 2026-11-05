#![cfg(all(feature = "hosting", unix))]
mod support;

use devmil_git::activity::ActivityId;
use devmil_git::hosting::http::{
    curl_form_command, curl_result_response, TransportFailure, STATUS_MARKER,
};
use devmil_git::hosting::review::{
    curl_command, parse_curl_result, ReviewHttpMethod, ReviewHttpRequest,
};
use devmil_git::hosting::{HostingError, HostingProvider};
use devmil_git::process::{ActivityGitRunner, GitProcessCommand, PrivateActivityResult};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use support::Fixture;

fn fake_curl(directory: &Path, body: &str) -> PathBuf {
    let path = directory.join("fake-curl");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn run(command: GitProcessCommand) -> PrivateActivityResult {
    ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
}

#[test]
fn form_requests_send_the_form_on_stdin_and_parse_the_shared_marker() {
    let fixture = Fixture::empty("hosting-form");
    // Echo the form (from stdin) as the body, then the status trailer curl would write.
    let curl = fake_curl(
        fixture.root(),
        &format!("cat; printf '\\n{STATUS_MARKER}:201'"),
    );
    let command = curl_form_command(
        curl.as_os_str(),
        "https://example.test/oauth/token",
        b"grant_type=refresh_token&refresh_token=secret",
    )
    .unwrap();
    let response = curl_result_response(run(command)).unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(
        response.body,
        b"grant_type=refresh_token&refresh_token=secret"
    );

    let failing = fake_curl(fixture.root(), "exit 6");
    let command =
        curl_form_command(failing.as_os_str(), "https://example.test/token", b"a=b").unwrap();
    assert_eq!(
        curl_result_response(run(command)),
        Err(TransportFailure::Network.into_error())
    );
    assert!(matches!(
        curl_result_response(run(curl_form_command(
            fixture.root().join("missing").as_os_str(),
            "https://e.test",
            b""
        )
        .unwrap())),
        Err(HostingError::Unavailable(_))
    ));
}

#[test]
fn review_commands_write_the_shared_marker() {
    let fixture = Fixture::empty("hosting-review");
    // Print the curl configuration's write-out format back so the marker can be checked, as
    // curl would expand it: headers, a body, then the trailer.
    let curl = fake_curl(
        fixture.root(),
        "config=$(cat); case \"$config\" in *'DEVMIL_GIT_STATUS:%{http_code}'*) ;; *) exit 9;; esac; printf 'HTTP/1.1 200 OK\\r\\nlink: <https://x/?page=2>; rel=\"next\"\\r\\n\\r\\n[]\\nDEVMIL_GIT_STATUS:200'",
    );
    let command = curl_command(
        curl.as_os_str(),
        "https://api.example.test",
        HostingProvider::GitHub,
        &ReviewHttpRequest {
            method: ReviewHttpMethod::Get,
            path: "/repos/a/b/pulls/1/files".into(),
            body: Vec::new(),
        },
        b"token",
        Duration::from_secs(10),
    )
    .unwrap();
    let response = parse_curl_result(run(command)).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"[]");
    assert_eq!(response.next_page.as_deref(), Some("2"));
}
