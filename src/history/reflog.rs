//! NUL-delimited reflog records with raw dated selectors.
use super::ParseError;
use crate::types::ObjectId;
const REFLOG_FIELDS: usize = 5;
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReflogRecord {
    pub reference: Vec<u8>,
    pub commit: ObjectId,
    pub recorded_at_unix_seconds: i64,
    pub timezone: Vec<u8>,
    pub summary: Vec<u8>,
    pub actor_name: Vec<u8>,
    pub actor_email: Vec<u8>,
}
/// Parses five-field NUL-framed reflog output with raw dated selectors.
///
/// # Errors
/// Rejects incomplete records, invalid object IDs, and malformed dated selectors.
pub fn parse_reflog(bytes: &[u8]) -> Result<Vec<ReflogRecord>, ParseError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.last().is_some_and(|field| field.is_empty()) {
        fields.pop();
    }
    if fields.len() % REFLOG_FIELDS != 0 {
        return Err(ParseError(
            "Git reflog output has an incomplete NUL-delimited record",
        ));
    }
    fields
        .chunks_exact(REFLOG_FIELDS)
        .map(|fields| {
            let commit = ObjectId::parse(String::from_utf8_lossy(fields[0]).into_owned())
                .map_err(|_| ParseError("Git reflog output contains an invalid object ID"))?;
            let (reference, recorded_at_unix_seconds, timezone) = parse_dated_selector(fields[1])?;
            Ok(ReflogRecord {
                reference,
                commit,
                recorded_at_unix_seconds,
                timezone,
                summary: fields[2].to_vec(),
                actor_name: fields[3].to_vec(),
                actor_email: fields[4].to_vec(),
            })
        })
        .collect()
}

fn parse_dated_selector(selector: &[u8]) -> Result<(Vec<u8>, i64, Vec<u8>), ParseError> {
    let marker = selector
        .windows(2)
        .rposition(|window| window == b"@{")
        .ok_or(ParseError("Git reflog selector has no raw date"))?;
    if !selector.ends_with(b"}") || marker == 0 {
        return Err(ParseError("Git reflog selector is malformed"));
    }
    let reference = selector[..marker].to_vec();
    let payload = &selector[marker + 2..selector.len() - 1];
    let space = payload
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or(ParseError("Git reflog selector has no timezone"))?;
    let timestamp = std::str::from_utf8(&payload[..space])
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or(ParseError("Git reflog selector has an invalid timestamp"))?;
    Ok((reference, timestamp, payload[space + 1..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_dates_and_non_utf8_fields_survive_parsing() {
        let mut bytes =
            format!("{}\0HEAD@{{1700000000 +0230}}\0move\0", "1".repeat(40)).into_bytes();
        bytes.extend_from_slice(b"A\xff\0mail@example.invalid\0");
        let records = parse_reflog(&bytes).unwrap();
        assert_eq!(records[0].actor_name, b"A\xff");
        assert_eq!(records[0].recorded_at_unix_seconds, 1_700_000_000);
        assert_eq!(records[0].timezone, b"+0230");
        assert!(parse_reflog(b"one\0two").is_err());
    }
}
