//! Bounded reflog pages with immutable event identities.
use super::CommandExecutor;
use crate::activity::{ActivityId, ActivityStatus};
use crate::cancellation::Cancellation;
use crate::process::{GitProcessCommand, ProcessArgument};
use crate::types::{ObjectId, RepositoryPath};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, ffi::OsString, fmt};
pub const MAX_REFLOG_PAGE_LIMIT: usize = 256;
pub const MAX_REFLOG_OFFSET: usize = 100_000;
pub const MAX_REFLOG_REFERENCE_BYTES: usize = 1_024;
pub const MAX_REFLOG_SUMMARY_BYTES: usize = 4_096;
pub const MAX_REFLOG_ACTOR_BYTES: usize = 1_024;
pub const MAX_REFLOG_ERROR_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReflogEntryId([u8; 32]);

impl ReflogEntryId {
    fn from_fields(
        reference: &[u8],
        commit: &ObjectId,
        recorded_at_unix_seconds: i64,
        timezone: &[u8],
        summary: &[u8],
        actor_name: &[u8],
        actor_email: &[u8],
    ) -> Self {
        let mut digest = Sha256::new();
        for bytes in [
            reference,
            commit.as_str().as_bytes(),
            timezone,
            summary,
            actor_name,
            actor_email,
        ] {
            digest.update(bytes.len().to_be_bytes());
            digest.update(bytes);
        }
        digest.update(recorded_at_unix_seconds.to_be_bytes());
        Self(digest.finalize().into())
    }

    pub fn hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReflogEntry {
    id: ReflogEntryId,
    reference: Vec<u8>,
    commit: ObjectId,
    recorded_at_unix_seconds: i64,
    timezone: Vec<u8>,
    summary: Vec<u8>,
    actor_name: Vec<u8>,
    actor_email: Vec<u8>,
}

impl ReflogEntry {
    /// Builds one immutable reflog event. Its digest identity does not include the moving
    /// `@{n}` selector, so prepending a newer event cannot change existing row identities.
    ///
    /// # Errors
    ///
    /// Rejects empty, malformed, or oversized parser fields.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reference: Vec<u8>,
        commit: ObjectId,
        recorded_at_unix_seconds: i64,
        timezone: Vec<u8>,
        summary: Vec<u8>,
        actor_name: Vec<u8>,
        actor_email: Vec<u8>,
    ) -> Result<Self, ReflogFailure> {
        if reference.is_empty()
            || reference.len() > MAX_REFLOG_REFERENCE_BYTES
            || reference
                .iter()
                .any(|byte| matches!(byte, 0 | b'\n' | b'\r'))
        {
            return Err(ReflogFailure::new(
                ReflogFailureKind::MalformedOutput,
                "reflog reference is empty, malformed, or oversized",
            ));
        }
        if summary.is_empty() || summary.len() > MAX_REFLOG_SUMMARY_BYTES {
            return Err(ReflogFailure::new(
                ReflogFailureKind::MalformedOutput,
                "reflog summary is empty or oversized",
            ));
        }
        if actor_name.len() > MAX_REFLOG_ACTOR_BYTES || actor_email.len() > MAX_REFLOG_ACTOR_BYTES {
            return Err(ReflogFailure::new(
                ReflogFailureKind::MalformedOutput,
                "reflog actor identity is oversized",
            ));
        }
        if !valid_timezone(&timezone) {
            return Err(ReflogFailure::new(
                ReflogFailureKind::MalformedOutput,
                "reflog timezone is malformed",
            ));
        }
        let id = ReflogEntryId::from_fields(
            &reference,
            &commit,
            recorded_at_unix_seconds,
            &timezone,
            &summary,
            &actor_name,
            &actor_email,
        );
        Ok(Self {
            id,
            reference,
            commit,
            recorded_at_unix_seconds,
            timezone,
            summary,
            actor_name,
            actor_email,
        })
    }

    pub const fn id(&self) -> ReflogEntryId {
        self.id
    }

    pub fn reference(&self) -> &[u8] {
        &self.reference
    }

    pub const fn commit(&self) -> &ObjectId {
        &self.commit
    }

    pub const fn recorded_at_unix_seconds(&self) -> i64 {
        self.recorded_at_unix_seconds
    }

    pub fn timezone(&self) -> &[u8] {
        &self.timezone
    }

    pub fn summary(&self) -> &[u8] {
        &self.summary
    }

    pub fn actor_name(&self) -> &[u8] {
        &self.actor_name
    }

    pub fn actor_email(&self) -> &[u8] {
        &self.actor_email
    }
}

fn valid_timezone(value: &[u8]) -> bool {
    value.len() == 5
        && matches!(value.first(), Some(b'+' | b'-'))
        && value[1..].iter().all(u8::is_ascii_digit)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReflogQuery {
    pub offset: usize,
    pub limit: usize,
}

impl ReflogQuery {
    /// # Errors
    ///
    /// Rejects zero and unbounded pages or offsets.
    pub fn new(offset: usize, limit: usize) -> Result<Self, ReflogFailure> {
        if offset > MAX_REFLOG_OFFSET || limit == 0 || limit > MAX_REFLOG_PAGE_LIMIT {
            return Err(ReflogFailure::new(
                ReflogFailureKind::InvalidRequest,
                "reflog page offset or limit is outside its bound",
            ));
        }
        Ok(Self { offset, limit })
    }
}

impl Default for ReflogQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: MAX_REFLOG_PAGE_LIMIT,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReflogFailureKind {
    InvalidRequest,
    Canceled,
    ProcessFailed,
    MalformedOutput,
    TooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReflogFailure {
    kind: ReflogFailureKind,
    detail: String,
    activity_id: Option<ActivityId>,
}

impl ReflogFailure {
    pub fn new(kind: ReflogFailureKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > MAX_REFLOG_ERROR_BYTES {
            let mut end = MAX_REFLOG_ERROR_BYTES;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
        }
        Self {
            kind,
            detail,
            activity_id: None,
        }
    }

    #[must_use]
    pub const fn with_activity_id(mut self, activity_id: ActivityId) -> Self {
        self.activity_id = Some(activity_id);
        self
    }

    pub const fn kind(&self) -> ReflogFailureKind {
        self.kind
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub const fn activity_id(&self) -> Option<ActivityId> {
        self.activity_id
    }
}

impl fmt::Display for ReflogFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ReflogFailure {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReflogPage {
    pub entries: Vec<ReflogEntry>,
    pub next_offset: Option<usize>,
    pub activity_id: ActivityId,
}
const REFLOG_FORMAT: &str = "%H%x00%gD%x00%gs%x00%gn%x00%ge";
const MAX_REFLOG_RECORD_BYTES: usize =
    65 + MAX_REFLOG_REFERENCE_BYTES + MAX_REFLOG_SUMMARY_BYTES + MAX_REFLOG_ACTOR_BYTES * 2 + 32;

/// Reads a bounded newest-first page.
/// # Panics
/// Only if a constant environment name becomes invalid.
/// # Errors
/// Rejects invalid bounds, cancellation, failed Git and malformed events.
pub fn load(
    executable: &std::ffi::OsStr,
    repository: &RepositoryPath,
    query: ReflogQuery,
    cancellation: &dyn Cancellation,
    executor: &mut dyn CommandExecutor,
) -> Result<ReflogPage, ReflogFailure> {
    ReflogQuery::new(query.offset, query.limit)?;
    if cancellation.is_cancelled() {
        return Err(ReflogFailure::new(
            ReflogFailureKind::Canceled,
            "reflog read canceled",
        ));
    }
    let requested = query.limit.saturating_add(1);
    let output_limit = requested.saturating_mul(MAX_REFLOG_RECORD_BYTES);
    let command = GitProcessCommand::git(executable.to_os_string())
        .current_dir(repository.as_path())
        .args(
            [
                OsString::from("reflog"),
                OsString::from("show"),
                OsString::from("--all"),
                OsString::from("-z"),
                OsString::from("--date=raw"),
                OsString::from(format!("--format={REFLOG_FORMAT}")),
                OsString::from(format!("--skip={}", query.offset)),
                OsString::from(format!("--max-count={requested}")),
            ]
            .into_iter()
            .map(ProcessArgument::visible),
        )
        .output_limit(output_limit)
        .redact_output()
        .env("GIT_OPTIONAL_LOCKS", "0")
        .expect("constant environment name is valid")
        .env("LC_ALL", "C")
        .expect("constant environment name is valid");
    let mut result = executor.execute(command);
    let activity_id = result.activity.id;
    let truncated_output = result.stdout.truncated;
    let bytes = result.stdout.take_bytes();
    let outcome = match result.activity.status {
        ActivityStatus::Canceled { .. } => Err(ReflogFailure::new(
            ReflogFailureKind::Canceled,
            "reflog read canceled",
        )),
        ActivityStatus::Succeeded { .. } if truncated_output => Err(ReflogFailure::new(
            ReflogFailureKind::TooLarge,
            "reflog output exceeded its bounded result limit",
        )),
        ActivityStatus::Succeeded { .. } => {
            let mut entries =
                parse_reflog(&bytes).map_err(|failure| failure.with_activity_id(activity_id))?;
            let has_more = entries.len() > query.limit;
            entries.truncate(query.limit);
            validate_unique_identities(&entries)
                .map_err(|failure| failure.with_activity_id(activity_id))?;
            Ok(ReflogPage {
                next_offset: has_more.then(|| query.offset.saturating_add(query.limit)),
                entries,
                activity_id,
            })
        }
        ActivityStatus::LaunchError { .. }
        | ActivityStatus::Failed { .. }
        | ActivityStatus::TimedOut { .. }
        | ActivityStatus::Running => Err(ReflogFailure::new(
            ReflogFailureKind::ProcessFailed,
            "Git could not read the repository reflog",
        )),
    };
    outcome.map_err(|failure| failure.with_activity_id(activity_id))
}

fn validate_unique_identities(entries: &[ReflogEntry]) -> Result<(), ReflogFailure> {
    // Git's reflog pretty placeholders do not expose the old object ID. If every exposed stable
    // field is identical, the moving selector is the only remaining discriminator; fail closed
    // rather than make delegate identity depend on that selector.
    let mut identities = HashSet::with_capacity(entries.len());
    if entries.iter().any(|entry| !identities.insert(entry.id())) {
        return Err(ReflogFailure::new(
            ReflogFailureKind::MalformedOutput,
            "reflog contains indistinguishable duplicate events",
        ));
    }
    Ok(())
}

fn parse_reflog(bytes: &[u8]) -> Result<Vec<ReflogEntry>, ReflogFailure> {
    crate::history::reflog::parse_reflog(bytes)
        .map_err(|error| ReflogFailure::new(ReflogFailureKind::MalformedOutput, error.to_string()))?
        .into_iter()
        .map(|record| {
            ReflogEntry::new(
                record.reference,
                record.commit,
                record.recorded_at_unix_seconds,
                record.timezone,
                record.summary,
                record.actor_name,
                record.actor_email,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn parser_preserves_byte_fields_and_raw_date_identity() {
        let mut bytes = Vec::new();
        for field in [
            OID.as_bytes(),
            b"refs/heads/non-utf8-\xff@{1700000000 +0230}",
            b"reset: moving to HEAD~1",
            b"A \xff Person",
            b"person@example.test",
        ] {
            if !bytes.is_empty() {
                bytes.push(0);
            }
            bytes.extend_from_slice(field);
        }
        let entries = parse_reflog(&bytes).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].reference(), b"refs/heads/non-utf8-\xff");
        assert_eq!(entries[0].recorded_at_unix_seconds(), 1_700_000_000);
        assert_eq!(entries[0].timezone(), b"+0230");
        assert_eq!(entries[0].summary(), b"reset: moving to HEAD~1");
        assert_eq!(entries[0].actor_name(), b"A \xff Person");
    }

    #[test]
    fn parser_rejects_partial_invalid_and_oversized_records() {
        assert!(parse_reflog(b"one\0two").is_err());
        let invalid = format!("{OID}\0HEAD@{{when +0200}}\0move\0name\0email");
        assert!(parse_reflog(invalid.as_bytes()).is_err());
        let oversized = format!(
            "{OID}\0HEAD@{{1700000000 +0200}}\0{}\0name\0email",
            "x".repeat(MAX_REFLOG_SUMMARY_BYTES + 1)
        );
        assert!(parse_reflog(oversized.as_bytes()).is_err());
    }

    #[test]
    fn identical_events_fail_closed_instead_of_publishing_duplicate_row_identities() {
        let record = format!(
            "{OID}\0HEAD@{{1700000000 +0200}}\0reset: moving to HEAD\0Person\0person@example.test"
        );
        let bytes = format!("{record}\0{record}");
        let entries = parse_reflog(bytes.as_bytes()).unwrap();
        assert_eq!(entries.len(), 2);
        let failure = validate_unique_identities(&entries).unwrap_err();
        assert_eq!(failure.kind(), ReflogFailureKind::MalformedOutput);
        assert_eq!(
            failure.detail(),
            "reflog contains indistinguishable duplicate events"
        );
    }
}

#[cfg(test)]
mod model_tests {
    use super::*;
    fn oid(value: u8) -> ObjectId {
        ObjectId::parse(format!("{value:040x}")).unwrap()
    }

    #[test]
    fn identity_survives_a_newer_selector_position() {
        let first = ReflogEntry::new(
            b"HEAD".to_vec(),
            oid(1),
            1_700_000_000,
            b"+0200".to_vec(),
            b"checkout: moving from main to topic".to_vec(),
            b"Person".to_vec(),
            b"person@example.test".to_vec(),
        )
        .unwrap();
        let refreshed = ReflogEntry::new(
            b"HEAD".to_vec(),
            oid(1),
            1_700_000_000,
            b"+0200".to_vec(),
            b"checkout: moving from main to topic".to_vec(),
            b"Person".to_vec(),
            b"person@example.test".to_vec(),
        )
        .unwrap();

        assert_eq!(first.id(), refreshed.id());
        assert_eq!(first.id().hex().len(), 64);
    }

    #[test]
    fn entries_and_queries_enforce_hard_bounds() {
        assert!(ReflogQuery::new(0, 0).is_err());
        assert!(ReflogQuery::new(0, MAX_REFLOG_PAGE_LIMIT + 1).is_err());
        assert!(ReflogQuery::new(MAX_REFLOG_OFFSET + 1, 1).is_err());
        assert!(ReflogEntry::new(
            Vec::new(),
            oid(2),
            1,
            b"UTC".to_vec(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .is_err());
    }
}
