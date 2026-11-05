use super::http::{curl_get_command, parse_curl_response, MAX_RESPONSE_BYTES};
use super::model::{
    CiStatus, HostingAccount, HostingAccountId, HostingConnection, HostingCredential, HostingError,
    HostingProvider, HostingRepositoryId, PullRequestState,
};
use super::oauth::{
    parse_device_authorization, parse_device_authorization_poll, parse_oauth_token,
    DeviceAuthorizationPoll,
};
use super::providers::{
    provider_plugin, HostingHttpRequest, HostingHttpResponse, HostingProviderRequester,
};
use crate::cancellation::Cancellation;
use std::cell::RefCell;
use std::ffi::OsStr;
use std::sync::atomic::AtomicBool;

struct Requester {
    path: &'static str,
    body: Vec<u8>,
    seen: RefCell<Vec<String>>,
}

impl HostingProviderRequester for Requester {
    fn request(
        &self,
        _: &HostingConnection,
        path: String,
        credential: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<HostingHttpResponse, HostingError> {
        if cancellation.is_cancelled() {
            return Err(HostingError::Canceled);
        }
        assert_eq!(path, self.path);
        assert_eq!(credential.expose(), b"private-token");
        self.seen.borrow_mut().push(path);
        Ok(HostingHttpResponse {
            status: 200,
            body: self.body.clone(),
        })
    }
}

fn connection(provider: HostingProvider) -> HostingConnection {
    HostingConnection::from_legacy_account(&HostingAccount {
        id: HostingAccountId::new("test").unwrap(),
        provider,
        login: "alice".into(),
        server: match provider {
            HostingProvider::GitHub => "github.com",
            HostingProvider::GitLab => "gitlab.com",
            HostingProvider::Forgejo => "codeberg.org",
        }
        .into(),
    })
    .unwrap()
}

#[test]
fn provider_repository_routes_and_schema_conversion_are_distinct() {
    for (provider, path, body) in [
        (
            HostingProvider::GitHub,
            "/user/repos?per_page=50",
            r#"[{"full_name":"team/project","clone_url":"https://example.test/team/project.git","html_url":"https://example.test/team/project","private":false}]"#,
        ),
        (
            HostingProvider::GitLab,
            "/projects?membership=true&per_page=50",
            r#"[{"path_with_namespace":"team/project","http_url_to_repo":"https://example.test/team/project.git","web_url":"https://example.test/team/project","visibility":"private"}]"#,
        ),
        (
            HostingProvider::Forgejo,
            "/user/repos?limit=50",
            r#"[{"full_name":"team/project","clone_url":"https://example.test/team/project.git","html_url":"https://example.test/team/project","private":false}]"#,
        ),
    ] {
        let requester = Requester {
            path,
            body: body.as_bytes().to_vec(),
            seen: RefCell::new(Vec::new()),
        };
        let credential = HostingCredential::new(b"private-token".to_vec()).unwrap();
        let rows = provider_plugin(provider)
            .repositories(
                &requester,
                &connection(provider),
                &credential,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(rows[0].id.as_str(), "team/project");
        assert_eq!(rows[0].private, provider == HostingProvider::GitLab);
        assert_eq!(requester.seen.borrow().len(), 1);
        assert_eq!(
            provider_plugin(provider).repositories(
                &requester,
                &connection(provider),
                &credential,
                &AtomicBool::new(true)
            ),
            Err(HostingError::Canceled)
        );
    }
}

struct GitlabOptionalCiRequester;

impl HostingProviderRequester for GitlabOptionalCiRequester {
    fn request(
        &self,
        _: &HostingConnection,
        path: String,
        _: &HostingCredential,
        _: &dyn Cancellation,
    ) -> Result<HostingHttpResponse, HostingError> {
        let body = if path.contains("merge_requests") {
            br#"[{"iid":7,"title":"Valid MR","draft":false,"web_url":"https://gitlab.com/team/project/-/merge_requests/7","author":{"username":"alice"},"source_branch":"topic","target_branch":"main","source_project_id":42,"target_project_id":42,"sha":"abc123"}]"#.to_vec()
        } else {
            assert!(path.contains("pipelines"));
            b"malformed optional enrichment".to_vec()
        };
        Ok(HostingHttpResponse { status: 200, body })
    }
}

#[test]
fn malformed_optional_gitlab_ci_does_not_hide_valid_change_request() {
    let rows = provider_plugin(HostingProvider::GitLab)
        .pull_requests(
            &GitlabOptionalCiRequester,
            &connection(HostingProvider::GitLab),
            &HostingRepositoryId::new("team/project").unwrap(),
            &HostingCredential::new(b"private-token".to_vec()).unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].number, 7);
    assert_eq!(rows[0].ci_status, CiStatus::Unknown);
}

#[test]
fn oauth_tokens_decode_both_formats_and_clear_private_strings() {
    for body in [
        br#"{"access_token":"private-token","refresh_token":"refresh","expires_in":60}"#.as_slice(),
        b"access_token=private-token&refresh_token=refresh&expires_in=60",
    ] {
        let mut token = parse_oauth_token(body).unwrap();
        assert_eq!(token.access_token, "private-token");
        assert_eq!(token.expires_in, Some(60));
        token.zeroize();
        assert!(token.access_token.is_empty());
        assert_eq!(token.refresh_token.as_deref(), Some(""));
    }
    assert!(matches!(
        parse_oauth_token(b"error=access_denied"),
        Err(HostingError::Unauthorized)
    ));
    assert!(matches!(
        parse_oauth_token(b"\xff"),
        Err(HostingError::MalformedResponse)
    ));
}

#[test]
fn device_flow_distinguishes_pending_slowdown_and_terminal_errors() {
    assert!(matches!(
        parse_device_authorization_poll(br#"{"error":"authorization_pending"}"#),
        Ok(DeviceAuthorizationPoll::Pending)
    ));
    assert!(matches!(
        parse_device_authorization_poll(b"error=slow_down&interval=10"),
        Ok(DeviceAuthorizationPoll::SlowDown(Some(10)))
    ));
    assert!(matches!(
        parse_device_authorization_poll(b"error=access_denied"),
        Ok(DeviceAuthorizationPoll::Denied)
    ));
    assert!(matches!(
        parse_device_authorization_poll(b"error=expired_token"),
        Ok(DeviceAuthorizationPoll::Expired)
    ));
    assert!(matches!(
        parse_device_authorization_poll(b"error=incorrect_device_code"),
        Err(HostingError::Unauthorized)
    ));
    assert!(parse_device_authorization(br#"{"device_code":"secret","user_code":"code","verification_uri":"https://github.com/login/device","expires_in":60,"interval":5}"#).is_ok());
    assert!(parse_device_authorization(br#"{"device_code":"secret","user_code":"code","verification_uri":"https://github.com/login/device","expires_in":0,"interval":5}"#).is_err());
}

#[test]
fn http_command_keeps_credentials_off_argv_and_public_debug() {
    let request = HostingHttpRequest {
        base_url: "https://example.test/api".into(),
        path: "/user".into(),
        provider: HostingProvider::GitHub,
    };
    let command = curl_get_command(OsStr::new("curl"), &request, b"private-token").unwrap();
    let activity = command.activity_command();
    assert!(activity.stdin_bytes > "private-token".len());
    assert!(!format!("{command:?} {activity:?}").contains("private-token"));
    assert!(activity
        .arguments
        .iter()
        .any(|argument| argument.display == "--config"));
    for token in [b"bad\nheader".as_slice(), b"bad\"quote", b"bad\\slash", b""] {
        assert!(curl_get_command(OsStr::new("curl"), &request, token).is_err());
    }
}

#[test]
fn http_framing_uses_last_status_marker_and_bounds_response() {
    let response =
        parse_curl_response(b"body\nDEVMIL_GIT_STATUS:fake\nDEVMIL_GIT_STATUS:200").unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"body\nDEVMIL_GIT_STATUS:fake");
    assert!(parse_curl_response(b"missing marker").is_err());
    assert!(parse_curl_response(b"body\nDEVMIL_GIT_STATUS:no").is_err());
    let mut oversized = vec![b'x'; MAX_RESPONSE_BYTES + 1];
    oversized.extend_from_slice(b"\nDEVMIL_GIT_STATUS:200");
    assert!(parse_curl_response(&oversized).is_err());
}

struct RoutedRequester {
    routes: Vec<(&'static str, &'static str)>,
    seen: RefCell<Vec<String>>,
}

impl HostingProviderRequester for RoutedRequester {
    fn request(
        &self,
        _: &HostingConnection,
        path: String,
        _: &HostingCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<HostingHttpResponse, HostingError> {
        if cancellation.is_cancelled() {
            return Err(HostingError::Canceled);
        }
        let body = self
            .routes
            .iter()
            .find(|(route, _)| *route == path)
            .unwrap_or_else(|| panic!("unexpected request {path}"))
            .1;
        self.seen.borrow_mut().push(path);
        Ok(HostingHttpResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
        })
    }
}

macro_rules! github_topic_pull {
    () => {
        r#"{"number":7,"title":"Topic","draft":false,"html_url":"https://github.com/team/project/pull/7","user":{"login":"alice"},"head":{"ref":"feature/topic","sha":"abc123","repo":{"full_name":"team/project"}},"base":{"ref":"main","repo":{"full_name":"team/project"}}}"#
    };
}
const FORGEJO_PULLS: &str = r#"[
{"number":1,"title":"Same repo","draft":false,"state":"open","html_url":"https://codeberg.org/team/project/pulls/1","user":{"login":"alice"},"head":{"ref":"feature/topic","label":"feature/topic","sha":"abc","repo":{"full_name":"team/project"}},"base":{"ref":"main","sha":"def","repo":{"full_name":"team/project"}}},
null,
{"number":2,"title":"Fork","draft":false,"state":"open","html_url":"https://codeberg.org/team/project/pulls/2","user":{"login":"bob"},"head":{"ref":"feature/topic","sha":"abc","repo":{"full_name":"bob/project"}},"base":{"ref":"main","sha":"def","repo":{"full_name":"team/project"}}},
{"number":3,"title":"Abandoned","draft":false,"state":"closed","merged":false,"html_url":"https://codeberg.org/team/project/pulls/3","user":{"login":"bob"},"head":{"ref":"abandoned","sha":"abc","repo":{"full_name":"team/project"}},"base":{"ref":"main","sha":"def","repo":{"full_name":"team/project"}}},
{"number":4,"title":"Deleted branch","draft":false,"state":"closed","merged":true,"html_url":"https://codeberg.org/team/project/pulls/4","user":{"login":"alice"},"head":{"ref":"refs/pull/4/head","label":"deleted","sha":"abc","repo":{"full_name":"team/project"}},"base":{"ref":"main","sha":"def","repo":{"full_name":"team/project"}}}
]"#;

fn branch_and_authored(
    provider: HostingProvider,
    routes: Vec<(&'static str, &'static str)>,
) -> (Vec<u64>, Vec<u64>, Vec<String>) {
    let requester = RoutedRequester {
        routes,
        seen: RefCell::new(Vec::new()),
    };
    let repository = HostingRepositoryId::new("team/project").unwrap();
    let credential = HostingCredential::new(b"private-token".to_vec()).unwrap();
    let plugin = provider_plugin(provider);
    let branch = plugin
        .branch_pull_requests(
            &requester,
            &connection(provider),
            &repository,
            "feature/topic",
            &credential,
            &AtomicBool::new(false),
        )
        .unwrap();
    let authored = plugin
        .authored_pull_requests(
            &requester,
            &connection(provider),
            &repository,
            "alice",
            &credential,
            &AtomicBool::new(false),
        )
        .unwrap();
    (
        branch.iter().map(|pull| pull.number).collect(),
        authored.iter().map(|pull| pull.number).collect(),
        requester.seen.into_inner(),
    )
}

#[test]
fn github_branch_and_authored_queries_filter_on_the_server() {
    let issues = r#"[{"number":7,"title":"Topic","state":"open","html_url":"https://github.com/team/project/pull/7","user":{"login":"alice"},"pull_request":{}},{"number":8,"title":"Issue","state":"open","html_url":"https://github.com/team/project/issues/8","user":{"login":"alice"}}]"#;
    let (branch, authored, seen) = branch_and_authored(
        HostingProvider::GitHub,
        vec![
            (
                "/repos/team/project/pulls?state=all&head=team%3Afeature%2Ftopic&per_page=50",
                concat!("[", github_topic_pull!(), "]"),
            ),
            (
                "/repos/team/project/issues?state=open&creator=alice&per_page=50",
                issues,
            ),
            ("/repos/team/project/pulls/7", github_topic_pull!()),
        ],
    );
    assert_eq!(branch, [7]);
    assert_eq!(authored, [7]);
    assert_eq!(seen.len(), 3);
}

#[test]
fn gitlab_branch_query_keeps_only_same_project_sources() {
    let merge_requests = r#"[{"iid":7,"title":"Topic","draft":false,"web_url":"https://gitlab.com/team/project/-/merge_requests/7","author":{"username":"alice"},"source_branch":"feature/topic","target_branch":"main","source_project_id":42,"target_project_id":42,"sha":"abc"},{"iid":8,"title":"Fork","draft":false,"web_url":"https://gitlab.com/team/project/-/merge_requests/8","author":{"username":"bob"},"source_branch":"feature/topic","target_branch":"main","source_project_id":9,"target_project_id":42,"sha":"abc"}]"#;
    let (branch, authored, _) = branch_and_authored(
        HostingProvider::GitLab,
        vec![
            (
                "/projects/team%2Fproject/merge_requests?state=all&source_branch=feature%2Ftopic&per_page=50",
                merge_requests,
            ),
            (
                "/projects/team%2Fproject/merge_requests?state=opened&author_username=alice&per_page=20",
                merge_requests,
            ),
        ],
    );
    assert_eq!(branch, [7]);
    assert_eq!(authored, [7, 8]);
}

#[test]
fn forgejo_branch_queries_scan_recent_pull_requests_once_per_batch() {
    let requester = RoutedRequester {
        routes: vec![
            (
                "/repos/team/project/pulls?state=all&sort=recentupdate&limit=50&page=1",
                FORGEJO_PULLS,
            ),
            (
                "/repos/team/project/pulls?state=open&poster=alice&limit=20",
                FORGEJO_PULLS,
            ),
        ],
        seen: RefCell::new(Vec::new()),
    };
    let repository = HostingRepositoryId::new("team/project").unwrap();
    let credential = HostingCredential::new(b"private-token".to_vec()).unwrap();
    let plugin = provider_plugin(HostingProvider::Forgejo);
    let mut found = Vec::new();
    plugin
        .branches_pull_requests(
            &requester,
            &connection(HostingProvider::Forgejo),
            &repository,
            &["feature/topic".into(), "deleted".into(), "abandoned".into()],
            &credential,
            &AtomicBool::new(false),
            &mut |branch, pulls| {
                found.push((
                    branch.to_owned(),
                    pulls
                        .iter()
                        .map(|pull| (pull.number, pull.state))
                        .collect::<Vec<_>>(),
                ));
            },
        )
        .unwrap();
    assert_eq!(
        found,
        [
            (
                "feature/topic".to_owned(),
                vec![(1, PullRequestState::Open)]
            ),
            ("deleted".to_owned(), vec![(4, PullRequestState::Merged)]),
            ("abandoned".to_owned(), vec![]),
        ]
    );
    // One short page answers the whole batch.
    assert_eq!(requester.seen.borrow().len(), 1);
    let authored = plugin
        .authored_pull_requests(
            &requester,
            &connection(HostingProvider::Forgejo),
            &repository,
            "alice",
            &credential,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        authored.iter().map(|pull| pull.number).collect::<Vec<_>>(),
        [1, 4]
    );
}

#[test]
fn branch_and_login_queries_reject_control_characters() {
    let requester = RoutedRequester {
        routes: Vec::new(),
        seen: RefCell::new(Vec::new()),
    };
    let repository = HostingRepositoryId::new("team/project").unwrap();
    let credential = HostingCredential::new(b"private-token".to_vec()).unwrap();
    for provider in [
        HostingProvider::GitHub,
        HostingProvider::GitLab,
        HostingProvider::Forgejo,
    ] {
        let plugin = provider_plugin(provider);
        for value in ["", "bad\nvalue"] {
            assert_eq!(
                plugin.branch_pull_requests(
                    &requester,
                    &connection(provider),
                    &repository,
                    value,
                    &credential,
                    &AtomicBool::new(false),
                ),
                Err(HostingError::InvalidIdentity)
            );
            assert_eq!(
                plugin.authored_pull_requests(
                    &requester,
                    &connection(provider),
                    &repository,
                    value,
                    &credential,
                    &AtomicBool::new(false),
                ),
                Err(HostingError::InvalidIdentity)
            );
        }
    }
    assert!(requester.seen.borrow().is_empty());
}

#[test]
fn branch_queries_keep_open_and_merged_but_not_closed_pull_requests() {
    let repository = HostingRepositoryId::new("team/project").unwrap();
    let credential = HostingCredential::new(b"private-token".to_vec()).unwrap();
    let github = RoutedRequester {
        routes: vec![(
            "/repos/team/project/pulls?state=all&head=team%3Atopic&per_page=50",
            r#"[
{"number":3,"title":"Open","draft":true,"state":"open","merged_at":null,"html_url":"https://github.com/team/project/pull/3","user":{"login":"a"},"head":{"ref":"topic","sha":"c","repo":{"full_name":"team/project"}},"base":{"ref":"main","repo":{"full_name":"team/project"}}},
{"number":2,"title":"Merged","draft":false,"state":"closed","merged_at":"2026-10-01T10:00:00Z","html_url":"https://github.com/team/project/pull/2","user":{"login":"a"},"head":{"ref":"topic","sha":"b","repo":{"full_name":"team/project"}},"base":{"ref":"main","repo":{"full_name":"team/project"}}},
{"number":1,"title":"Abandoned","draft":false,"state":"closed","merged_at":null,"html_url":"https://github.com/team/project/pull/1","user":{"login":"a"},"head":{"ref":"topic","sha":"a","repo":{"full_name":"team/project"}},"base":{"ref":"main","repo":{"full_name":"team/project"}}}
]"#,
        )],
        seen: RefCell::new(Vec::new()),
    };
    let gitlab = RoutedRequester {
        routes: vec![(
            "/projects/team%2Fproject/merge_requests?state=all&source_branch=topic&per_page=50",
            r#"[
{"iid":3,"title":"Open","draft":false,"state":"opened","web_url":"https://gitlab.com/team/project/-/merge_requests/3","author":{"username":"a"},"source_branch":"topic","target_branch":"main","source_project_id":42,"target_project_id":42,"sha":"c"},
{"iid":2,"title":"Merged","draft":false,"state":"merged","web_url":"https://gitlab.com/team/project/-/merge_requests/2","author":{"username":"a"},"source_branch":"topic","target_branch":"main","source_project_id":42,"target_project_id":42,"sha":"b"},
{"iid":1,"title":"Abandoned","draft":false,"state":"closed","web_url":"https://gitlab.com/team/project/-/merge_requests/1","author":{"username":"a"},"source_branch":"topic","target_branch":"main","source_project_id":42,"target_project_id":42,"sha":"a"}
]"#,
        )],
        seen: RefCell::new(Vec::new()),
    };
    for (provider, requester, first) in [
        (HostingProvider::GitHub, &github, PullRequestState::Draft),
        (HostingProvider::GitLab, &gitlab, PullRequestState::Open),
    ] {
        let pulls = provider_plugin(provider)
            .branch_pull_requests(
                requester,
                &connection(provider),
                &repository,
                "topic",
                &credential,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            pulls
                .iter()
                .map(|pull| (pull.number, pull.state))
                .collect::<Vec<_>>(),
            [(3, first), (2, PullRequestState::Merged)],
            "{provider:?}"
        );
    }
}

#[test]
fn oauth_refresh_form_encodes_secrets_and_token_errors_stay_typed() {
    let mut connection = connection(HostingProvider::Forgejo);
    connection.oauth_client_id = "client&=id".into();
    assert_eq!(
        super::oauth::oauth_refresh_token_form(&connection, "secret+/&="),
        "client_id=client%26%3Did&grant_type=refresh_token&refresh_token=secret%2B%2F%26%3D"
    );
    for body in [
        br#"{"error":"invalid_grant","error_description":"private"}"#.as_slice(),
        b"error=invalid_grant",
    ] {
        assert!(matches!(
            parse_oauth_token(body),
            Err(HostingError::Unauthorized)
        ));
    }
    for body in [
        br#"{"access_token":"ok","expires_in":0}"#.as_slice(),
        br#"{"access_token":"ok","refresh_token":""}"#,
        br#"{"unexpected":"private"}"#,
        b"access_token=ok&expires_in=invalid",
        b"{broken",
    ] {
        assert!(matches!(
            parse_oauth_token(body),
            Err(HostingError::MalformedResponse)
        ));
    }
}

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn open_pull(provider: HostingProvider) -> super::model::HostingPullRequest {
    let pulls = match provider {
        HostingProvider::GitHub => super::parsers::parse_github_pull_requests(
            format!(
                r#"[{{"number":7,"title":"Topic","draft":false,"html_url":"https://github.com/team/project/pull/7","user":{{"login":"alice"}},"head":{{"ref":"topic","sha":"{HEAD}","repo":{{"full_name":"team/project"}}}},"base":{{"ref":"main","repo":{{"full_name":"team/project"}}}}}}]"#
            )
            .as_bytes(),
        ),
        HostingProvider::Forgejo => super::parsers::parse_forgejo_pull_requests(
            format!(
                r#"[{{"number":7,"title":"Topic","draft":false,"state":"open","mergeable":true,"html_url":"https://codeberg.org/team/project/pulls/7","user":{{"login":"alice"}},"head":{{"ref":"topic","sha":"{HEAD}","repo":{{"full_name":"team/project"}}}},"base":{{"ref":"main","sha":"def","repo":{{"full_name":"team/project"}}}}}}]"#
            )
            .as_bytes(),
        ),
        HostingProvider::GitLab => super::parsers::parse_gitlab_pull_requests(
            format!(
                r#"[{{"iid":7,"title":"Topic","draft":false,"state":"opened","web_url":"https://gitlab.com/team/project/-/merge_requests/7","author":{{"username":"alice"}},"source_branch":"topic","target_branch":"main","source_project_id":1,"target_project_id":1,"sha":"{HEAD}","detailed_merge_status":"mergeable"}}]"#
            )
            .as_bytes(),
            &HostingRepositoryId::new("team/project").unwrap(),
        ),
    };
    pulls.unwrap().remove(0)
}

fn pull_request_status(
    provider: HostingProvider,
    pull: &mut super::model::HostingPullRequest,
    routes: Vec<(&'static str, &'static str)>,
    canceled: bool,
) -> (Result<(), HostingError>, Vec<String>) {
    let requester = RoutedRequester {
        routes,
        seen: RefCell::new(Vec::new()),
    };
    let result = provider_plugin(provider).pull_request_status(
        &requester,
        &connection(provider),
        &HostingRepositoryId::new("team/project").unwrap(),
        pull,
        &HostingCredential::new(b"private-token".to_vec()).unwrap(),
        &AtomicBool::new(canceled),
    );
    (result, requester.seen.into_inner())
}

#[test]
fn listings_report_mergeability_where_providers_include_it() {
    use super::model::Mergeability;
    assert_eq!(
        open_pull(HostingProvider::GitHub).mergeability,
        Mergeability::Unknown
    );
    assert_eq!(
        open_pull(HostingProvider::Forgejo).mergeability,
        Mergeability::Mergeable
    );
    assert_eq!(
        open_pull(HostingProvider::GitLab).mergeability,
        Mergeability::Mergeable
    );
    let repository = HostingRepositoryId::new("team/project").unwrap();
    let gitlab = |fields: &str| {
        super::parsers::parse_gitlab_pull_requests(
            format!(
                r#"[{{"iid":7,"title":"T","draft":false,"state":"opened","web_url":"https://gitlab.com/team/project/-/merge_requests/7","author":{{"username":"a"}},"source_branch":"t","target_branch":"main","source_project_id":1,"target_project_id":1,"sha":"{HEAD}"{fields}}}]"#
            )
            .as_bytes(),
            &repository,
        )
        .unwrap()[0]
            .mergeability
    };
    assert_eq!(gitlab(""), Mergeability::Unknown);
    assert_eq!(
        gitlab(r#","has_conflicts":true,"detailed_merge_status":"not_approved""#),
        Mergeability::Conflicting
    );
    assert_eq!(
        gitlab(r#","detailed_merge_status":"checking""#),
        Mergeability::Checking
    );
    assert_eq!(
        gitlab(r#","detailed_merge_status":"ci_must_pass""#),
        Mergeability::Mergeable
    );
    assert_eq!(
        gitlab(r#","merge_status":"cannot_be_merged""#),
        Mergeability::Conflicting
    );
    // A merged pull request has nothing left to merge.
    let merged = super::parsers::parse_forgejo_pull_requests(
        br#"[{"number":4,"title":"Done","draft":false,"state":"closed","merged":true,"mergeable":false,"html_url":"https://codeberg.org/team/project/pulls/4","user":{"login":"alice"},"head":{"ref":"done","sha":"abc","repo":{"full_name":"team/project"}},"base":{"ref":"main","sha":"def","repo":{"full_name":"team/project"}}}]"#,
    )
    .unwrap();
    assert_eq!(merged[0].mergeability, Mergeability::Unknown);
}

#[test]
fn pull_request_status_reads_head_checks_and_github_mergeability() {
    use super::model::Mergeability;
    const GITHUB_CHECKS: &str = r#"{"check_runs":[{"id":1,"name":"build","status":"completed","conclusion":"success","html_url":null},{"id":2,"name":"test","status":"in_progress","conclusion":null,"html_url":null}]}"#;
    const GITHUB_DETAIL: &str = concat!(
        r#"{"number":7,"title":"Topic","draft":false,"state":"open","mergeable":false,"mergeable_state":"dirty","html_url":"https://github.com/team/project/pull/7","user":{"login":"alice"},"#,
        r#""head":{"ref":"topic","sha":"0123456789abcdef0123456789abcdef01234567","repo":{"full_name":"team/project"}},"base":{"ref":"main","repo":{"full_name":"team/project"}}}"#
    );
    let mut pull = open_pull(HostingProvider::GitHub);
    let (result, seen) = pull_request_status(
        HostingProvider::GitHub,
        &mut pull,
        vec![
            (
                "/repos/team/project/commits/0123456789abcdef0123456789abcdef01234567/check-runs",
                GITHUB_CHECKS,
            ),
            ("/repos/team/project/pulls/7", GITHUB_DETAIL),
        ],
        false,
    );
    assert_eq!(result, Ok(()));
    assert_eq!(seen.len(), 2);
    assert_eq!(pull.ci_status, CiStatus::Pending);
    assert_eq!(pull.mergeability, Mergeability::Conflicting);

    let mut pull = open_pull(HostingProvider::Forgejo);
    let (result, seen) = pull_request_status(
        HostingProvider::Forgejo,
        &mut pull,
        vec![(
            "/repos/team/project/commits/0123456789abcdef0123456789abcdef01234567/status",
            r#"{"state":"failure","statuses":[{"id":3,"status":"success","context":"ci / lint"},{"id":4,"status":"error","context":"ci / test","target_url":"https://codeberg.org/team/project/actions/runs/1"}]}"#,
        )],
        false,
    );
    assert_eq!(result, Ok(()));
    assert_eq!(seen.len(), 1);
    assert_eq!(pull.ci_status, CiStatus::Failing);
    assert_eq!(pull.mergeability, Mergeability::Mergeable);

    // The newest pipeline replaces an older failure.
    let mut pull = open_pull(HostingProvider::GitLab);
    let (result, _) = pull_request_status(
        HostingProvider::GitLab,
        &mut pull,
        vec![(
            "/projects/team%2Fproject/pipelines?sha=0123456789abcdef0123456789abcdef01234567&per_page=20",
            r#"[{"id":9,"status":"success"},{"id":8,"status":"failed"}]"#,
        )],
        false,
    );
    assert_eq!(result, Ok(()));
    assert_eq!(pull.ci_status, CiStatus::Passing);
}

#[test]
fn pull_request_status_is_optional_except_for_cancellation() {
    use super::model::Mergeability;
    // A malformed answer keeps the pull request and leaves its status unknown.
    let mut pull = open_pull(HostingProvider::Forgejo);
    let (result, _) = pull_request_status(
        HostingProvider::Forgejo,
        &mut pull,
        vec![(
            "/repos/team/project/commits/0123456789abcdef0123456789abcdef01234567/status",
            "not json",
        )],
        false,
    );
    assert_eq!(result, Ok(()));
    assert_eq!(pull.ci_status, CiStatus::Unknown);

    // A detail for a head that moved on describes another commit.
    let mut pull = open_pull(HostingProvider::GitHub);
    let (result, _) = pull_request_status(
        HostingProvider::GitHub,
        &mut pull,
        vec![
            (
                "/repos/team/project/commits/0123456789abcdef0123456789abcdef01234567/check-runs",
                r#"{"check_runs":[]}"#,
            ),
            (
                "/repos/team/project/pulls/7",
                r#"{"number":7,"title":"Topic","draft":false,"state":"open","mergeable":true,"mergeable_state":"clean","html_url":"https://github.com/team/project/pull/7","user":{"login":"alice"},"head":{"ref":"topic","sha":"ffffffffffffffffffffffffffffffffffffffff","repo":{"full_name":"team/project"}},"base":{"ref":"main","repo":{"full_name":"team/project"}}}"#,
            ),
        ],
        false,
    );
    assert_eq!(result, Ok(()));
    assert_eq!(pull.ci_status, CiStatus::Unknown);
    assert_eq!(pull.mergeability, Mergeability::Unknown);

    let mut pull = open_pull(HostingProvider::GitHub);
    let (result, seen) = pull_request_status(HostingProvider::GitHub, &mut pull, vec![], true);
    assert_eq!(result, Err(HostingError::Canceled));
    assert!(seen.is_empty());

    // Merged pull requests are not read.
    let mut pull = open_pull(HostingProvider::GitHub);
    pull.state = PullRequestState::Merged;
    let (result, seen) = pull_request_status(HostingProvider::GitHub, &mut pull, vec![], false);
    assert_eq!(result, Ok(()));
    assert!(seen.is_empty());
}

#[test]
fn combined_ci_status_lets_the_worst_check_decide() {
    use super::providers::combined_ci_status;
    use super::rest::HostingCheckState::{Canceled, Failing, Passing, Pending, Skipped};
    assert_eq!(combined_ci_status([]), CiStatus::Unknown);
    assert_eq!(combined_ci_status([Skipped]), CiStatus::Unknown);
    assert_eq!(combined_ci_status([Passing, Skipped]), CiStatus::Passing);
    assert_eq!(combined_ci_status([Passing, Canceled]), CiStatus::Canceled);
    assert_eq!(combined_ci_status([Canceled, Pending]), CiStatus::Pending);
    assert_eq!(
        combined_ci_status([Pending, Failing, Passing]),
        CiStatus::Failing
    );
}

#[test]
fn commit_ci_status_reads_a_head_without_a_pull_request() {
    use super::model::HostingCapabilitySet;
    let head = crate::types::ObjectId::parse(HEAD.to_owned()).unwrap();
    let status = |provider: HostingProvider,
                  connection: HostingConnection,
                  routes: Vec<(&'static str, &'static str)>,
                  canceled: bool| {
        let requester = RoutedRequester {
            routes,
            seen: RefCell::new(Vec::new()),
        };
        let result = provider_plugin(provider).commit_ci_status(
            &requester,
            &connection,
            &HostingRepositoryId::new("team/project").unwrap(),
            &head,
            &HostingCredential::new(b"private-token".to_vec()).unwrap(),
            &AtomicBool::new(canceled),
        );
        (result, requester.seen.into_inner())
    };
    let (result, seen) = status(
        HostingProvider::Forgejo,
        connection(HostingProvider::Forgejo),
        vec![(
            "/repos/team/project/commits/0123456789abcdef0123456789abcdef01234567/status",
            r#"{"state":"pending","statuses":[{"id":3,"status":"success","context":"ci / lint"},{"id":4,"status":"pending","context":"ci / test"}]}"#,
        )],
        false,
    );
    assert_eq!(result, Ok(CiStatus::Pending));
    assert_eq!(seen.len(), 1);

    let (result, _) = status(
        HostingProvider::GitLab,
        connection(HostingProvider::GitLab),
        vec![(
            "/projects/team%2Fproject/pipelines?sha=0123456789abcdef0123456789abcdef01234567&per_page=20",
            r#"[{"id":9,"status":"failed"},{"id":8,"status":"success"}]"#,
        )],
        false,
    );
    assert_eq!(result, Ok(CiStatus::Failing));

    // Malformed answers and connections without checks stay unknown.
    let (result, _) = status(
        HostingProvider::GitHub,
        connection(HostingProvider::GitHub),
        vec![(
            "/repos/team/project/commits/0123456789abcdef0123456789abcdef01234567/check-runs",
            "not json",
        )],
        false,
    );
    assert_eq!(result, Ok(CiStatus::Unknown));
    let mut without = connection(HostingProvider::GitHub);
    without.capabilities = HostingCapabilitySet {
        supported: std::collections::BTreeSet::new(),
    };
    let (result, seen) = status(HostingProvider::GitHub, without, vec![], false);
    assert_eq!(result, Ok(CiStatus::Unknown));
    assert!(seen.is_empty());

    let (result, _) = status(
        HostingProvider::GitHub,
        connection(HostingProvider::GitHub),
        vec![],
        true,
    );
    assert_eq!(result, Err(HostingError::Canceled));
}
