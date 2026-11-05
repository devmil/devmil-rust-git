//! Commit summaries with caller-selected browsing limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitSummary {
    pub subject: Vec<u8>,
    pub author: Vec<u8>,
    pub author_email: Vec<u8>,
    pub author_time: i64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SummaryError {
    MalformedCommit,
    AuthorTooLarge,
    SubjectTooLarge,
}
impl std::fmt::Display for SummaryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MalformedCommit => "commit metadata is malformed",
            Self::AuthorTooLarge => "author exceeds its byte limit",
            Self::SubjectTooLarge => "subject exceeds its byte limit",
        })
    }
}
impl std::error::Error for SummaryError {}
/// Reads the author and first message line without decoding their bytes.
///
/// # Errors
/// Rejects missing author or message headers, invalid timestamps, and exceeded field limits.
pub fn parse_commit_summary(
    object: &[u8],
    author_limit: usize,
    subject_limit: usize,
) -> Result<CommitSummary, SummaryError> {
    let message_start = object
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| position + 2)
        .ok_or(SummaryError::MalformedCommit)?;
    let headers = &object[..message_start - 2];
    let author = headers
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"author "))
        .ok_or(SummaryError::MalformedCommit)?;
    if author.len() > author_limit {
        return Err(SummaryError::AuthorTooLarge);
    }
    let email_end = author
        .iter()
        .rposition(|byte| *byte == b'>')
        .ok_or(SummaryError::MalformedCommit)?;
    let timestamp = author[email_end + 1..]
        .split(u8::is_ascii_whitespace)
        .find(|field| !field.is_empty())
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|field| field.parse::<i64>().ok())
        .ok_or(SummaryError::MalformedCommit)?;
    let name_end = author[..email_end]
        .iter()
        .rposition(|byte| *byte == b'<')
        .ok_or(SummaryError::MalformedCommit)?;
    let author_name = author[..name_end]
        .strip_suffix(b" ")
        .unwrap_or(&author[..name_end]);
    let email = author
        .get(name_end + 1..email_end)
        .ok_or(SummaryError::MalformedCommit)?;
    let message = &object[message_start..];
    let subject_end = message
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(message.len());
    if subject_end > subject_limit {
        return Err(SummaryError::SubjectTooLarge);
    }
    Ok(CommitSummary {
        subject: message[..subject_end].to_vec(),
        author: author_name.to_vec(),
        author_email: email.to_vec(),
        author_time: timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_keeps_nuls_and_separates_email_from_display_name() {
        let object = b"author A <a@example.invalid> 10 +0000\n\nsubject\0bytes\nbody";
        let summary = parse_commit_summary(object, 100, 100).unwrap();
        assert_eq!(summary.author, b"A");
        assert_eq!(summary.author_email, b"a@example.invalid");
        assert_eq!(summary.subject, b"subject\0bytes");
        assert_eq!(
            parse_commit_summary(object, 100, 2),
            Err(SummaryError::SubjectTooLarge)
        );
    }
}
