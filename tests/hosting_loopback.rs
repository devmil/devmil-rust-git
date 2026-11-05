#![cfg(feature = "hosting")]
//! Runs in its own test binary: forking curl here must not race the fake-curl scripts that
//! `hosting_transport.rs` writes (a script still open for writing in a forked child cannot run).

use devmil_git::activity::ActivityId;
use devmil_git::hosting::http::{
    curl_form_command, curl_form_loopback_command, curl_result_response,
};
use devmil_git::process::{ActivityGitRunner, GitProcessCommand, PrivateActivityResult};
use std::time::Duration;

fn run(command: GitProcessCommand) -> PrivateActivityResult {
    ActivityGitRunner::run_private(command, ActivityId::new(1).unwrap(), None, &mut |_| {})
}

#[test]
fn loopback_form_requests_reach_a_local_server_through_curl() {
    use std::io::{Read as _, Write as _};
    assert!(curl_form_loopback_command("curl".as_ref(), "http://example.test:80/t", b"").is_err());
    assert!(curl_form_loopback_command("curl".as_ref(), "http://127.0.0.1/t", b"").is_err());
    assert!(
        curl_form_loopback_command("curl".as_ref(), "http://127.0.0.1:1@x.test/t", b"").is_err()
    );
    assert!(curl_form_command("curl".as_ref(), "http://127.0.0.1:1/t", b"").is_err());
    if std::process::Command::new("curl")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("curl is not installed; skipping");
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        while !String::from_utf8_lossy(&request).contains("a=b") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });
    let response = curl_result_response(run(curl_form_loopback_command(
        "curl".as_ref(),
        &format!("http://127.0.0.1:{port}/oauth/token"),
        b"a=b",
    )
    .unwrap()))
    .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"{}");
    let request = server.join().unwrap();
    assert!(request.starts_with("POST /oauth/token "));
    assert!(request.contains("application/x-www-form-urlencoded"));
}
