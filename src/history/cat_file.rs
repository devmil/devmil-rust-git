//! Length-framed commit responses from `git cat-file --batch`.
use super::ParseError;
use crate::types::ObjectId;

/// One requested commit object, or a missing-object response.
#[derive(Debug, Eq, PartialEq)]
pub struct CommitFrame<'a> {
    pub id: ObjectId,
    pub contents: Option<&'a [u8]>,
}

/// Validates exact request order, object kind, byte lengths, and complete framing.
/// Returned objects borrow the input, including arbitrary NUL bytes.
///
/// # Errors
/// Rejects malformed headers, unexpected identities, and incomplete or extra responses.
pub fn parse_batch<'a>(
    bytes: &'a [u8],
    expected: &[ObjectId],
) -> Result<Vec<CommitFrame<'a>>, ParseError> {
    let mut remaining = bytes;
    let mut frames = Vec::with_capacity(expected.len());
    for id in expected {
        let end = remaining
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(malformed)?;
        let size = parse_header(&remaining[..end], id)?;
        remaining = &remaining[end + 1..];
        let contents = if let Some(size) = size {
            let object = remaining.get(..size).ok_or_else(malformed)?;
            if remaining.get(size) != Some(&b'\n') {
                return Err(malformed());
            }
            remaining = &remaining[size + 1..];
            Some(object)
        } else {
            None
        };
        frames.push(CommitFrame {
            id: id.clone(),
            contents,
        });
    }
    if !remaining.is_empty() {
        return Err(malformed());
    }
    Ok(frames)
}

/// Parses commit sizes from `cat-file --batch-check` in request order.
/// Missing objects have `None` sizes. Size policy belongs to the caller.
///
/// # Errors
/// Rejects malformed, incomplete, reordered, or extra responses.
pub fn parse_batch_check(
    bytes: &[u8],
    expected: &[ObjectId],
) -> Result<Vec<Option<usize>>, ParseError> {
    let bytes = bytes.strip_suffix(b"\n").ok_or_else(malformed)?;
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let mut sizes = Vec::with_capacity(expected.len());
    for id in expected {
        let line = lines.next().ok_or_else(malformed)?;
        sizes.push(parse_header(line.strip_suffix(b"\r").unwrap_or(line), id)?);
    }
    if lines.next().is_some() {
        return Err(malformed());
    }
    Ok(sizes)
}

fn parse_header(header: &[u8], expected: &ObjectId) -> Result<Option<usize>, ParseError> {
    let mut fields = header.split(|byte| *byte == b' ');
    if fields.next() != Some(expected.as_str().as_bytes()) {
        return Err(malformed());
    }
    let kind = fields.next().ok_or_else(malformed)?;
    if kind == b"missing" {
        return if fields.next().is_none() {
            Ok(None)
        } else {
            Err(malformed())
        };
    }
    if kind != b"commit" {
        return Err(malformed());
    }
    let size = std::str::from_utf8(fields.next().ok_or_else(malformed)?)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(malformed)?;
    if fields.next().is_some() {
        return Err(malformed());
    }
    Ok(Some(size))
}

fn malformed() -> ParseError {
    ParseError("Git returned malformed commit metadata")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn length_frames_preserve_nuls_and_reject_truncation_extra_and_wrong_identity() {
        let id = ObjectId::parse("1".repeat(40)).unwrap();
        let bytes = format!("{} commit 3\na\0b\n", id.as_str()).into_bytes();
        let ids = std::slice::from_ref(&id);
        assert_eq!(
            parse_batch(&bytes, ids).unwrap()[0].contents,
            Some(b"a\0b".as_slice())
        );
        assert!(parse_batch(&bytes[..bytes.len() - 1], ids).is_err());
        assert!(parse_batch(&bytes, &[]).is_err());
        assert!(parse_batch(&bytes, &[ObjectId::parse("2".repeat(40)).unwrap()]).is_err());
        assert_eq!(
            parse_batch_check(format!("{} missing\n", id.as_str()).as_bytes(), ids).unwrap(),
            vec![None]
        );
    }
}
