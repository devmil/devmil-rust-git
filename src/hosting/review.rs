//! Provider-native pull-request review data, routes, private curl framing, and client logic.
use super::model::{HostingAccountId, HostingError, HostingProvider, HostingRepositoryId};
use crate::activity::ActivityStatus;
use crate::cancellation::Cancellation;
use crate::process::{GitProcessCommand, PrivateActivityResult, ProcessArgument, SemanticTimeout};
use serde::Deserialize;
use std::ffi::OsStr;
use std::fmt::{self, Write as _};
use std::time::Duration;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_PAGE_TOKEN_BYTES: usize = 256;
pub const MAX_REVIEW_ROWS: usize = 500;
pub const MAX_REVIEW_BODY_BYTES: usize = 65_536;
pub const MAX_REVIEW_PATH_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewAction {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewInlineComment {
    pub stable_id: String,
    pub path: String,
    pub line: u64,
    pub body: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewScope {
    pub account: HostingAccountId,
    pub repository: HostingRepositoryId,
    pub pull_request: u64,
    pub head_sha: String,
    pub action: ReviewAction,
    pub body: String,
    pub inline_comments: Vec<ReviewInlineComment>,
}

impl ReviewScope {
    /// # Errors
    ///
    /// Rejects empty or oversized identities and review bodies.
    pub fn validate(&self) -> Result<(), HostingError> {
        if self.pull_request == 0
            || self.head_sha.len() != 40
            || !self.head_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.body.len() > MAX_REVIEW_BODY_BYTES
            || self.inline_comments.len() > MAX_REVIEW_ROWS
            || (self.action == ReviewAction::RequestChanges && self.body.trim().is_empty())
        {
            return Err(HostingError::InvalidIdentity);
        }
        for comment in &self.inline_comments {
            if comment.stable_id.is_empty()
                || comment.stable_id.len() > MAX_REVIEW_PATH_BYTES
                || comment.path.is_empty()
                || comment.path.len() > MAX_REVIEW_PATH_BYTES
                || comment.line == 0
                || comment.body.trim().is_empty()
                || comment.body.len() > MAX_REVIEW_BODY_BYTES
            {
                return Err(HostingError::InvalidIdentity);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewDiffRow {
    pub stable_id: String,
    pub path: String,
    pub patch: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewThread {
    pub stable_id: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub author: String,
    pub body: String,
    pub resolved: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewHttpMethod {
    Get,
    Post,
}

pub struct ReviewHttpRequest {
    pub method: ReviewHttpMethod,
    pub path: String,
    pub body: Vec<u8>,
}

impl fmt::Debug for ReviewHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReviewHttpRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("body", &"[REDACTED]")
            .finish()
    }
}

pub struct ReviewHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub next_page: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewPage {
    pub scope: ReviewScope,
    pub diffs: Vec<ReviewDiffRow>,
    pub threads: Vec<ReviewThread>,
    pub next_page: Option<String>,
    pub incomplete: bool,
}
/// A caller-owned transport handling private request and response bytes.
pub type ReviewRequestExecutor<'a> =
    dyn FnMut(&ReviewHttpRequest) -> Result<ReviewHttpResponse, HostingError> + 'a;
/// Builds a private curl request; execution and activity ownership stay with the caller.
///
/// # Errors
/// Rejects unsafe credentials and unsupported URL schemes.
pub fn curl_command(
    executable: &OsStr,
    base_url: &str,
    provider: HostingProvider,
    request: &ReviewHttpRequest,
    credential: &[u8],
    timeout: Duration,
) -> Result<GitProcessCommand, HostingError> {
    if timeout.is_zero() || timeout > Duration::from_secs(30) {
        return Err(HostingError::InvalidIdentity);
    }
    if !request.path.starts_with('/')
        || credential.is_empty()
        || credential
            .iter()
            .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'"' | b'\\'))
    {
        return Err(HostingError::InvalidCredential);
    }
    let url = format!("{}{}", base_url.trim_end_matches('/'), request.path);
    if !(url.starts_with("https://")
        || url.starts_with("http://127.0.0.1:")
        || url.starts_with("http://[::1]:"))
    {
        return Err(HostingError::InvalidIdentity);
    }
    let escape = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    };
    let authorization = match provider {
        HostingProvider::GitHub => format!("Authorization: Bearer {}", escape(credential)),
        HostingProvider::GitLab => format!("PRIVATE-TOKEN: {}", escape(credential)),
        HostingProvider::Forgejo => {
            format!("Authorization: token {}", escape(credential))
        }
    };
    let mut config = format!(
        "silent\nshow-error\ndump-header = \"-\"\nurl = \"{}\"\nheader = \"{}\"\n",
        escape(url.as_bytes()),
        authorization
    );
    if request.method == ReviewHttpMethod::Post {
        config.push_str("request = \"POST\"\nheader = \"Content-Type: application/json\"\n");
        writeln!(&mut config, "data-binary = \"{}\"", escape(&request.body))
            .expect("writing to a String cannot fail");
    }
    writeln!(
        &mut config,
        "write-out = \"\\n{}:%{{http_code}}\"",
        super::http::STATUS_MARKER
    )
    .expect("writing to a String cannot fail");
    let command = GitProcessCommand::new(executable)
        .args([
            ProcessArgument::visible("--config"),
            ProcessArgument::visible("-"),
        ])
        .stdin_bytes(config.into_bytes())
        .redact_output()
        .output_limit(MAX_RESPONSE_BYTES + 64)
        .semantic_timeout(
            SemanticTimeout::new(timeout, "hosting review request")
                .map_err(|_| HostingError::InvalidIdentity)?,
        );
    Ok(command)
}
/// Parses a bounded private curl process result into a review response.
///
/// # Errors
/// Rejects failed, canceled, truncated, or malformed HTTP output.
pub fn parse_curl_result(
    mut result: PrivateActivityResult,
) -> Result<ReviewHttpResponse, HostingError> {
    if result.stdout.truncated || result.stderr.truncated {
        return Err(HostingError::MalformedResponse);
    }
    match result.activity.status {
        ActivityStatus::Succeeded { .. } => {}
        ActivityStatus::Canceled { .. } | ActivityStatus::TimedOut { .. } => {
            return Err(HostingError::Canceled)
        }
        _ => return Err(HostingError::Unavailable("review transport failed".into())),
    }
    let output = result.stdout.take_bytes();
    let (split, status) =
        super::http::split_status_marker(&output).ok_or(HostingError::MalformedResponse)?;
    let response = &output[..split];
    let (headers, body) = split_http_headers(response)?;
    Ok(ReviewHttpResponse {
        status,
        body: body.to_vec(),
        next_page: next_page_from_headers(headers)?,
    })
}
fn header_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4))
        .or_else(|| {
            bytes
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| (index, 2))
        })
}

fn split_http_headers(mut response: &[u8]) -> Result<(&[u8], &[u8]), HostingError> {
    loop {
        let (boundary, separator_len) =
            header_boundary(response).ok_or(HostingError::MalformedResponse)?;
        let headers = &response[..boundary];
        let body = &response[boundary + separator_len..];
        let status = headers
            .split(|byte| *byte == b'\n')
            .next()
            .and_then(|line| std::str::from_utf8(line).ok())
            .and_then(|line| line.trim_end_matches('\r').split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or(HostingError::MalformedResponse)?;
        if (100..200).contains(&status) || body.starts_with(b"HTTP/") {
            response = body;
            continue;
        }
        return Ok((headers, body));
    }
}

fn next_page_from_headers(headers: &[u8]) -> Result<Option<String>, HostingError> {
    let headers = std::str::from_utf8(headers).map_err(|_| HostingError::MalformedResponse)?;
    let mut gitlab_page = None;
    let mut github_page = None;
    for line in headers.lines().skip(1) {
        let line = line.trim_end_matches('\r');
        let Some((name, value)) = line.split_once(':') else {
            return Err(HostingError::MalformedResponse);
        };
        if name.eq_ignore_ascii_case("x-next-page") {
            let value = value.trim();
            if !value.is_empty() {
                page_number(Some(value))?;
                gitlab_page = Some(value.to_owned());
            }
        } else if name.eq_ignore_ascii_case("link") {
            for link in value.split(',') {
                if !link.contains("rel=\"next\"") {
                    continue;
                }
                let target = link
                    .split_once('<')
                    .and_then(|(_, rest)| rest.split_once('>'))
                    .map(|(target, _)| target)
                    .ok_or(HostingError::MalformedResponse)?;
                let query = target
                    .split_once('?')
                    .map(|(_, query)| query)
                    .ok_or(HostingError::MalformedResponse)?;
                let value = query
                    .split('&')
                    .find_map(|field| field.strip_prefix("page="))
                    .ok_or(HostingError::MalformedResponse)?;
                page_number(Some(value))?;
                github_page = Some(value.to_owned());
            }
        }
    }
    merged_next_page(gitlab_page, github_page)
}

fn encode_path_segment(value: &str) -> String {
    super::parsers::percent_encode(value)
}

fn repository_path(provider: HostingProvider, repository: &str) -> Result<String, HostingError> {
    match provider {
        HostingProvider::GitHub | HostingProvider::Forgejo => {
            let mut components = repository.split('/');
            let owner = components.next().unwrap_or_default();
            let name = components.next().unwrap_or_default();
            if components.next().is_some()
                || owner.is_empty()
                || name.is_empty()
                || matches!(owner, "." | "..")
                || matches!(name, "." | "..")
            {
                return Err(HostingError::InvalidIdentity);
            }
            Ok(format!(
                "{}/{}",
                encode_path_segment(owner),
                encode_path_segment(name)
            ))
        }
        HostingProvider::GitLab => {
            let components: Vec<_> = repository.split('/').collect();
            if components.is_empty()
                || components
                    .iter()
                    .any(|component| component.is_empty() || matches!(*component, "." | ".."))
            {
                return Err(HostingError::InvalidIdentity);
            }
            Ok(encode_path_segment(repository))
        }
    }
}

fn page_number(page: Option<&str>) -> Result<&str, HostingError> {
    let page = page.unwrap_or("1");
    if page.is_empty()
        || page.len() > MAX_PAGE_TOKEN_BYTES
        || !page.bytes().all(|byte| byte.is_ascii_digit())
        || page == "0"
    {
        return Err(HostingError::InvalidIdentity);
    }
    Ok(page)
}

/// New-side line numbers of every context or added line in a unified-diff `patch`, in order.
/// Review comments can anchor on exactly these lines. Hunk headers (`@@ -a,b +c,d @@`) reset the
/// counter; removed lines and `\ No newline` markers do not advance it.
#[must_use]
pub fn patch_new_line_numbers(patch: &str) -> Vec<u64> {
    let mut lines = Vec::new();
    let mut current = None;
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("@@ ") {
            current = header
                .split_whitespace()
                .find(|part| part.starts_with('+'))
                .and_then(|part| part[1..].split(',').next())
                .and_then(|value| value.parse::<u64>().ok());
            continue;
        }
        let Some(number) = current else { continue };
        if line.starts_with('-') && !line.starts_with("---") {
            continue;
        }
        if !line.starts_with("\\ No newline") {
            lines.push(number);
            current = number.checked_add(1);
        }
    }
    lines
}

#[derive(Deserialize)]
struct GitHubFile {
    filename: String,
    #[serde(default)]
    patch: String,
}

#[derive(Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Deserialize)]
struct GitHubReviewComment {
    id: u64,
    path: String,
    #[serde(default)]
    line: Option<u64>,
    user: GitHubUser,
    body: String,
}

#[derive(Deserialize)]
struct GitLabDiff {
    new_path: String,
    #[serde(default)]
    diff: String,
}

#[derive(Deserialize)]
struct GitLabAuthor {
    name: String,
}

#[derive(Default, Deserialize)]
struct GitLabPosition {
    #[serde(default)]
    new_path: Option<String>,
    #[serde(default)]
    new_line: Option<u64>,
}

#[derive(Deserialize)]
struct GitLabDiscussionNote {
    id: u64,
    author: GitLabAuthor,
    body: String,
    #[serde(default)]
    position: Option<GitLabPosition>,
    #[serde(default)]
    resolved: bool,
}

#[derive(Deserialize)]
struct GitLabDiscussion {
    id: String,
    #[serde(default)]
    notes: Vec<GitLabDiscussionNote>,
}

fn merged_next_page(
    first: Option<String>,
    second: Option<String>,
) -> Result<Option<String>, HostingError> {
    match (first, second) {
        (Some(first), Some(second)) if first != second => Err(HostingError::MalformedResponse),
        (Some(page), _) | (_, Some(page)) => Ok(Some(page)),
        (None, None) => Ok(None),
    }
}

fn validate_review_text(path: &str, body: &str) -> Result<(), HostingError> {
    if path.len() > MAX_REVIEW_PATH_BYTES || body.len() > MAX_REVIEW_BODY_BYTES {
        Err(HostingError::MalformedResponse)
    } else {
        Ok(())
    }
}

fn validate_review_metadata(value: &str) -> Result<(), HostingError> {
    if value.len() > MAX_REVIEW_PATH_BYTES {
        Err(HostingError::MalformedResponse)
    } else {
        Ok(())
    }
}

fn parse_github_review(
    diff_body: &[u8],
    thread_body: &[u8],
) -> Result<(Vec<ReviewDiffRow>, Vec<ReviewThread>), HostingError> {
    let files: Vec<GitHubFile> =
        serde_json::from_slice(diff_body).map_err(|_| HostingError::MalformedResponse)?;
    let comments: Vec<GitHubReviewComment> =
        serde_json::from_slice(thread_body).map_err(|_| HostingError::MalformedResponse)?;
    if files.len().saturating_add(comments.len()) > MAX_REVIEW_ROWS {
        return Err(HostingError::MalformedResponse);
    }
    let mut diffs = Vec::with_capacity(files.len());
    for (index, file) in files.into_iter().enumerate() {
        validate_review_text(&file.filename, &file.patch)?;
        diffs.push(ReviewDiffRow {
            stable_id: format!("github-diff:{index}:{}", file.filename),
            path: file.filename,
            patch: file.patch,
        });
    }
    let mut threads = Vec::with_capacity(comments.len());
    for comment in comments {
        validate_review_text(&comment.path, &comment.body)?;
        validate_review_metadata(&comment.user.login)?;
        threads.push(ReviewThread {
            stable_id: format!("github-comment:{}", comment.id),
            path: Some(comment.path),
            line: comment.line,
            author: comment.user.login,
            body: comment.body,
            resolved: false,
        });
    }
    Ok((diffs, threads))
}

fn parse_gitlab_review(
    diff_body: &[u8],
    thread_body: &[u8],
) -> Result<(Vec<ReviewDiffRow>, Vec<ReviewThread>), HostingError> {
    let files: Vec<GitLabDiff> =
        serde_json::from_slice(diff_body).map_err(|_| HostingError::MalformedResponse)?;
    let discussions: Vec<GitLabDiscussion> =
        serde_json::from_slice(thread_body).map_err(|_| HostingError::MalformedResponse)?;
    let note_count = discussions
        .iter()
        .try_fold(files.len(), |count, discussion| {
            count.checked_add(discussion.notes.len())
        })
        .ok_or(HostingError::MalformedResponse)?;
    if note_count > MAX_REVIEW_ROWS {
        return Err(HostingError::MalformedResponse);
    }
    let mut diffs = Vec::with_capacity(files.len());
    for (index, file) in files.into_iter().enumerate() {
        validate_review_text(&file.new_path, &file.diff)?;
        diffs.push(ReviewDiffRow {
            stable_id: format!("gitlab-diff:{index}:{}", file.new_path),
            path: file.new_path,
            patch: file.diff,
        });
    }
    let mut threads = Vec::with_capacity(note_count.saturating_sub(diffs.len()));
    for discussion in discussions {
        validate_review_metadata(&discussion.id)?;
        for note in discussion.notes {
            let position = note.position.unwrap_or_default();
            let path = position.new_path;
            if let Some(path) = path.as_deref() {
                validate_review_text(path, "")?;
            }
            validate_review_metadata(&note.author.name)?;
            validate_review_text("", &note.body)?;
            threads.push(ReviewThread {
                stable_id: format!("gitlab-discussion:{}:note:{}", discussion.id, note.id),
                path,
                line: position.new_line,
                author: note.author.name,
                body: note.body,
                resolved: note.resolved,
            });
        }
    }
    Ok((diffs, threads))
}

pub struct ReviewClient {
    account: HostingAccountId,
    provider: HostingProvider,
}

impl ReviewClient {
    #[must_use]
    pub fn new(account: HostingAccountId, provider: HostingProvider) -> Self {
        Self { account, provider }
    }

    fn execute(
        request: &ReviewHttpRequest,
        cancellation: &dyn Cancellation,
        execute: &mut ReviewRequestExecutor<'_>,
    ) -> Result<ReviewHttpResponse, HostingError> {
        if cancellation.is_cancelled() {
            return Err(HostingError::Canceled);
        }
        let response = execute(request)?;
        if response.body.len() > MAX_RESPONSE_BYTES
            || response.next_page.as_ref().is_some_and(|page| {
                page.len() > MAX_PAGE_TOKEN_BYTES
                    || !page
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
            })
        {
            return Err(HostingError::MalformedResponse);
        }
        match response.status {
            200..=299 => Ok(response),
            401 => Err(HostingError::Unauthorized),
            403 => Err(HostingError::PermissionDenied),
            429 => Err(HostingError::RateLimited),
            _ => Err(HostingError::Unavailable(
                "review provider request failed".into(),
            )),
        }
    }

    fn validate_scope(&self, scope: &ReviewScope) -> Result<(), HostingError> {
        scope.validate()?;
        if scope.account != self.account {
            return Err(HostingError::InvalidAccount);
        }
        Ok(())
    }
}

impl ReviewClient {
    /// Reads review files and comments through the caller-owned private transport.
    ///
    /// # Errors
    /// Rejects invalid scope, pagination, canceled operations, and malformed provider output.
    pub fn load(
        &self,
        scope: &ReviewScope,
        page: Option<&str>,
        cancellation: &dyn Cancellation,
        execute: &mut ReviewRequestExecutor<'_>,
    ) -> Result<ReviewPage, HostingError> {
        self.validate_scope(scope)?;
        if self.provider == HostingProvider::Forgejo {
            return Err(HostingError::Unavailable(
                "Forgejo review threads are not enabled by this adapter version".into(),
            ));
        }
        let repository = repository_path(self.provider, scope.repository.as_str())?;
        let page = page_number(page)?;
        let (diff_path, thread_path) = match self.provider {
            HostingProvider::GitHub => (
                format!(
                    "/repos/{repository}/pulls/{}/files?page={page}&per_page=100",
                    scope.pull_request
                ),
                format!(
                    "/repos/{repository}/pulls/{}/comments?page={page}&per_page=100",
                    scope.pull_request
                ),
            ),
            HostingProvider::GitLab => (
                format!(
                    "/projects/{repository}/merge_requests/{}/diffs?page={page}&per_page=100",
                    scope.pull_request
                ),
                format!(
                    "/projects/{repository}/merge_requests/{}/discussions?page={page}&per_page=100",
                    scope.pull_request
                ),
            ),
            HostingProvider::Forgejo => unreachable!("Forgejo returned before route selection"),
        };
        let diff_response = Self::execute(
            &ReviewHttpRequest {
                method: ReviewHttpMethod::Get,
                path: diff_path,
                body: Vec::new(),
            },
            cancellation,
            execute,
        )?;
        let thread_response = Self::execute(
            &ReviewHttpRequest {
                method: ReviewHttpMethod::Get,
                path: thread_path,
                body: Vec::new(),
            },
            cancellation,
            execute,
        )?;
        let (diffs, threads) = match self.provider {
            HostingProvider::GitHub => {
                parse_github_review(&diff_response.body, &thread_response.body)?
            }
            HostingProvider::GitLab => {
                parse_gitlab_review(&diff_response.body, &thread_response.body)?
            }
            HostingProvider::Forgejo => unreachable!("Forgejo returned before response parsing"),
        };
        let next_page = merged_next_page(diff_response.next_page, thread_response.next_page)?;
        Ok(ReviewPage {
            scope: scope.clone(),
            diffs,
            threads,
            incomplete: next_page.is_some(),
            next_page,
        })
    }
}

impl ReviewClient {
    /// Builds the complete provider-native submission request without executing it.
    ///
    /// The caller can bind explicit authorization to these exact target and payload
    /// bytes before passing the same scope to [`Self::submit`].
    ///
    /// # Errors
    /// Rejects invalid scopes and provider actions that are not supported.
    pub fn submission_request(
        &self,
        scope: &ReviewScope,
    ) -> Result<ReviewHttpRequest, HostingError> {
        self.validate_scope(scope)?;
        if self.provider == HostingProvider::Forgejo {
            return Err(HostingError::Unavailable(
                "Forgejo review submission is not enabled by this adapter version".into(),
            ));
        }
        let repository = repository_path(self.provider, scope.repository.as_str())?;
        let (path, payload) = match (self.provider, scope.action) {
            (HostingProvider::GitHub, action) => {
                let event = match action {
                    ReviewAction::Comment => "COMMENT",
                    ReviewAction::Approve => "APPROVE",
                    ReviewAction::RequestChanges => "REQUEST_CHANGES",
                };
                let comments = scope
                    .inline_comments
                    .iter()
                    .map(|comment| {
                        serde_json::json!({
                            "path": comment.path,
                            "line": comment.line,
                            "side": "RIGHT",
                            "body": comment.body
                        })
                    })
                    .collect::<Vec<_>>();
                (
                    format!("/repos/{repository}/pulls/{}/reviews", scope.pull_request),
                    serde_json::json!({
                        "commit_id": scope.head_sha,
                        "body": scope.body,
                        "event": event,
                        "comments": comments
                    }),
                )
            }
            (HostingProvider::GitLab, ReviewAction::Comment) => (
                format!(
                    "/projects/{repository}/merge_requests/{}/notes",
                    scope.pull_request
                ),
                serde_json::json!({"body": scope.body}),
            ),
            (HostingProvider::GitLab, ReviewAction::Approve) => (
                format!(
                    "/projects/{repository}/merge_requests/{}/approve",
                    scope.pull_request
                ),
                serde_json::json!({"sha": scope.head_sha}),
            ),
            (HostingProvider::GitLab, ReviewAction::RequestChanges) => {
                return Err(HostingError::Unavailable(
                    "GitLab does not expose a request-changes review action".into(),
                ));
            }
            (HostingProvider::Forgejo, _) => {
                unreachable!("Forgejo returned before submission route selection")
            }
        };
        if self.provider == HostingProvider::GitLab && !scope.inline_comments.is_empty() {
            return Err(HostingError::Unavailable(
                "GitLab inline review submission requires diff-version coordinates".into(),
            ));
        }
        Ok(ReviewHttpRequest {
            method: ReviewHttpMethod::Post,
            path,
            body: serde_json::to_vec(&payload).map_err(|_| HostingError::MalformedResponse)?,
        })
    }

    /// Submits a provider-native review. The caller must authorize this action before invoking it.
    ///
    /// # Errors
    /// Rejects invalid scopes, canceled operations, unsupported actions, and provider failures.
    pub fn submit(
        &self,
        scope: &ReviewScope,
        cancellation: &dyn Cancellation,
        execute: &mut ReviewRequestExecutor<'_>,
    ) -> Result<(), HostingError> {
        if cancellation.is_cancelled() {
            return Err(HostingError::Canceled);
        }
        let request = self.submission_request(scope)?;
        Self::execute(&request, cancellation, execute)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn scope() -> ReviewScope {
        ReviewScope {
            account: HostingAccountId::new("account").unwrap(),
            repository: HostingRepositoryId::new("team/project").unwrap(),
            pull_request: 4,
            head_sha: "a".repeat(40),
            action: ReviewAction::Approve,
            body: "private review".into(),
            inline_comments: Vec::new(),
        }
    }

    #[test]
    fn client_validates_authority_and_cancellation_before_transport() {
        let client = ReviewClient::new(scope().account, HostingProvider::GitHub);
        let mut called = false;
        let mut transport = |_: &ReviewHttpRequest| {
            called = true;
            Err(HostingError::MalformedResponse)
        };
        let mut wrong = scope();
        wrong.account = HostingAccountId::new("other").unwrap();
        assert_eq!(
            client.load(&wrong, None, &AtomicBool::new(false), &mut transport),
            Err(HostingError::InvalidAccount)
        );
        let mut inline = scope();
        inline.action = ReviewAction::Comment;
        inline.inline_comments.push(ReviewInlineComment {
            stable_id: "draft:1".into(),
            path: "src/lib.rs".into(),
            line: 7,
            body: "inline body".into(),
        });
        let github = ReviewClient::new(inline.account.clone(), HostingProvider::GitHub);
        let preview = github.submission_request(&inline).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&preview.body).unwrap();
        assert_eq!(payload["comments"][0]["path"], "src/lib.rs");
        assert_eq!(payload["comments"][0]["line"], 7);
        assert!(!format!("{preview:?}").contains("inline body"));
        assert_eq!(
            client.load(&scope(), None, &AtomicBool::new(true), &mut transport),
            Err(HostingError::Canceled)
        );
        assert_eq!(
            client.load(
                &scope(),
                Some("../bad"),
                &AtomicBool::new(false),
                &mut transport
            ),
            Err(HostingError::InvalidIdentity)
        );
        assert!(!called);
    }

    #[test]
    fn github_load_parses_separate_routes_and_matching_pagination() {
        let client = ReviewClient::new(scope().account, HostingProvider::GitHub);
        let mut paths = Vec::new();
        let page = client.load(&scope(), None, &AtomicBool::new(false), &mut |request| {
            paths.push(request.path.clone());
            Ok(ReviewHttpResponse { status: 200, next_page: Some("2".into()), body: if request.path.contains("/files?") { br#"[{"filename":"file.rs","patch":"+line"}]"#.to_vec() } else { br#"[{"id":2,"path":"file.rs","line":1,"user":{"login":"user"},"body":"note"}]"#.to_vec() } })
        }).unwrap();
        assert_eq!(
            paths,
            [
                "/repos/team/project/pulls/4/files?page=1&per_page=100",
                "/repos/team/project/pulls/4/comments?page=1&per_page=100"
            ]
        );
        assert_eq!(page.diffs.len(), 1);
        assert_eq!(page.threads[0].body, "note");
        assert!(page.incomplete);
    }

    #[test]
    fn submission_uses_provider_payload_and_keeps_request_debug_private() {
        let client = ReviewClient::new(scope().account, HostingProvider::GitLab);
        let preview = client.submission_request(&scope()).unwrap();
        assert_eq!(
            preview.path,
            "/projects/team%2Fproject/merge_requests/4/approve"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&preview.body).unwrap(),
            serde_json::json!({"sha": "a".repeat(40)})
        );
        client
            .submit(&scope(), &AtomicBool::new(false), &mut |request| {
                assert_eq!(
                    request.path,
                    "/projects/team%2Fproject/merge_requests/4/approve"
                );
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
                    serde_json::json!({"sha": "a".repeat(40)})
                );
                assert!(!format!("{request:?}").contains(&"a".repeat(40)));
                Ok(ReviewHttpResponse {
                    status: 201,
                    body: Vec::new(),
                    next_page: None,
                })
            })
            .unwrap();
    }

    #[test]
    fn headers_merge_pagination_and_skip_interim_status() {
        let response = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/2 200\r\nx-next-page: 2\r\n\r\n[]";
        let (headers, body) = split_http_headers(response).unwrap();
        assert_eq!(body, b"[]");
        assert_eq!(next_page_from_headers(headers).unwrap(), Some("2".into()));
        assert!(merged_next_page(Some("2".into()), Some("3".into())).is_err());
    }

    #[test]
    fn curl_builder_refuses_invalid_timeouts_without_panicking() {
        let request = ReviewHttpRequest {
            method: ReviewHttpMethod::Get,
            path: "/repos/team/project".into(),
            body: Vec::new(),
        };
        assert!(matches!(
            curl_command(
                OsStr::new("curl"),
                "https://example.invalid",
                HostingProvider::GitHub,
                &request,
                b"private-token",
                Duration::ZERO
            ),
            Err(HostingError::InvalidIdentity)
        ));
        let command = curl_command(
            OsStr::new("curl"),
            "https://example.invalid",
            HostingProvider::GitHub,
            &request,
            b"private-token",
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(!format!("{command:?}").contains("private-token"));
    }
}

#[cfg(test)]
mod patch_line_tests {
    use super::patch_new_line_numbers;

    #[test]
    fn new_side_lines_follow_hunk_headers() {
        let patch = "@@ -1,3 +10,4 @@ fn x\n context\n-removed\n+added\n+more\n\\ No newline at end of file\n@@ -20 +30 @@\n+single";
        assert_eq!(patch_new_line_numbers(patch), [10, 11, 12, 30]);
        assert!(patch_new_line_numbers("no hunk\n+x").is_empty());
    }
}
