//! Raw commit-object parsing with caller-selected resource limits.
use crate::types::ObjectId;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitLimits {
    pub message_bytes: usize,
    pub identity_bytes: usize,
    pub parents: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitParseErrorKind {
    Malformed,
    TooLarge,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitParseError {
    pub kind: CommitParseErrorKind,
    pub detail: String,
}
impl CommitParseError {
    fn new(kind: CommitParseErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}
impl std::fmt::Display for CommitParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}
impl std::error::Error for CommitParseError {}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitDetail {
    pub commit: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    /// Raw Git identity retained for command transparency and byte-safe round-tripping.
    pub author: Vec<u8>,
    pub author_time: i64,
    pub author_timezone: Vec<u8>,
    /// Raw Git identity retained for command transparency and byte-safe round-tripping.
    pub committer: Vec<u8>,
    pub committer_time: i64,
    pub committer_timezone: Vec<u8>,
    pub message: Vec<u8>,
}

/// Parses tree, parents, identities, timestamps, and the complete byte-preserving message.
///
/// # Errors
/// Rejects malformed required headers, identities, timezones, or exceeded resource limits.
pub fn parse_commit_detail(
    commit: ObjectId,
    input: &[u8],
    limits: CommitLimits,
) -> Result<CommitDetail, CommitParseError> {
    let Some(separator) = input.windows(2).position(|window| window == b"\n\n") else {
        return Err(malformed("commit object has no header terminator"));
    };
    let header = &input[..separator];
    let message = input[separator + 2..].to_vec();
    if message.len() > limits.message_bytes {
        return Err(CommitParseError::new(
            CommitParseErrorKind::TooLarge,
            "commit message exceeds the selected-commit limit",
        ));
    }
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer = None;
    for line in header.split(|byte| *byte == b'\n') {
        if line.starts_with(b" ") {
            continue;
        }
        if let Some(value) = line.strip_prefix(b"tree ") {
            tree = Some(parse_object(value, "tree")?);
        } else if let Some(value) = line.strip_prefix(b"parent ") {
            if parents.len() == limits.parents {
                return Err(CommitParseError::new(
                    CommitParseErrorKind::TooLarge,
                    "commit has too many parents",
                ));
            }
            parents.push(parse_object(value, "parent")?);
        } else if let Some(value) = line.strip_prefix(b"author ") {
            author = Some(parse_signature(value, "author", limits.identity_bytes)?);
        } else if let Some(value) = line.strip_prefix(b"committer ") {
            committer = Some(parse_signature(value, "committer", limits.identity_bytes)?);
        }
    }
    let tree = tree.ok_or_else(|| malformed("commit object has no tree"))?;
    let (author, author_time, author_timezone) =
        author.ok_or_else(|| malformed("commit object has no author"))?;
    let (committer, committer_time, committer_timezone) =
        committer.ok_or_else(|| malformed("commit object has no committer"))?;
    Ok(CommitDetail {
        commit,
        tree,
        parents,
        author,
        author_time,
        author_timezone,
        committer,
        committer_time,
        committer_timezone,
        message,
    })
}

fn parse_signature(
    input: &[u8],
    field: &str,
    identity_limit: usize,
) -> Result<(Vec<u8>, i64, Vec<u8>), CommitParseError> {
    let mut parts = input.rsplitn(3, |byte| *byte == b' ');
    let timezone = parts.next().ok_or_else(|| malformed(field))?;
    let timestamp = parts.next().ok_or_else(|| malformed(field))?;
    let identity = parts.next().ok_or_else(|| malformed(field))?;
    if identity.is_empty() || identity.len() > identity_limit {
        return Err(CommitParseError::new(
            CommitParseErrorKind::TooLarge,
            format!("commit {field} identity exceeds its limit"),
        ));
    }
    if timezone.len() != 5
        || !matches!(timezone.first(), Some(b'+' | b'-'))
        || !timezone[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(malformed(field));
    }
    let timezone_hours = u16::from(timezone[1] - b'0') * 10 + u16::from(timezone[2] - b'0');
    let timezone_minutes = u16::from(timezone[3] - b'0') * 10 + u16::from(timezone[4] - b'0');
    if timezone_hours > 23 || timezone_minutes > 59 {
        return Err(malformed(field));
    }
    let timestamp = std::str::from_utf8(timestamp)
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| malformed(field))?;
    let Some(_delimiter) = identity
        .windows(2)
        .rposition(|window| window == b" <")
        .filter(|_| identity.ends_with(b">"))
    else {
        return Err(malformed(field));
    };
    Ok((identity.to_vec(), timestamp, timezone.to_vec()))
}

fn parse_object(input: &[u8], field: &str) -> Result<ObjectId, CommitParseError> {
    let text = std::str::from_utf8(input).map_err(|_| malformed(field))?;
    ObjectId::parse(text).map_err(|_| malformed(field))
}

fn malformed(field: &str) -> CommitParseError {
    CommitParseError::new(
        CommitParseErrorKind::Malformed,
        format!("Git returned a malformed commit {field}"),
    )
}

impl CommitDetail {
    #[must_use]
    pub fn author_name(&self) -> Option<&[u8]> {
        signature_identity_parts(&self.author).map(|(name, _)| name)
    }

    #[must_use]
    pub fn author_email(&self) -> Option<&[u8]> {
        signature_identity_parts(&self.author).map(|(_, email)| email)
    }

    #[must_use]
    pub fn committer_name(&self) -> Option<&[u8]> {
        signature_identity_parts(&self.committer).map(|(name, _)| name)
    }

    #[must_use]
    pub fn committer_email(&self) -> Option<&[u8]> {
        signature_identity_parts(&self.committer).map(|(_, email)| email)
    }
}

fn signature_identity_parts(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let delimiter = raw.windows(2).rposition(|window| window == b" <")?;
    raw.ends_with(b">").then_some((
        &raw[..delimiter],
        &raw[delimiter.saturating_add(2)..raw.len().saturating_sub(1)],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_parsing_preserves_bytes_and_applies_caller_limits() {
        let id = ObjectId::parse("1".repeat(40)).unwrap();
        let input = format!("tree {}\nauthor A <a@example.invalid> 10 +0130\ncommitter C <c@example.invalid> 11 -0200\ngpgsig signature\n continuation\n\nsubject\0body\n", id.as_str());
        let limits = CommitLimits {
            message_bytes: 100,
            identity_bytes: 100,
            parents: 10,
        };
        let parsed = parse_commit_detail(id.clone(), input.as_bytes(), limits).unwrap();
        assert_eq!(parsed.author, b"A <a@example.invalid>");
        assert_eq!(parsed.message, b"subject\0body\n");
        assert_eq!(parsed.author_timezone, b"+0130");
        assert_eq!(
            parse_commit_detail(
                id.clone(),
                input.as_bytes(),
                CommitLimits {
                    message_bytes: 3,
                    ..limits
                }
            )
            .unwrap_err()
            .kind,
            CommitParseErrorKind::TooLarge
        );
        assert_eq!(
            parse_commit_detail(id, input.replace("+0130", "+2460").as_bytes(), limits)
                .unwrap_err()
                .kind,
            CommitParseErrorKind::Malformed
        );
    }
}
