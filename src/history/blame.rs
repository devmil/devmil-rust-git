//! Byte-preserving installed Git blame parsing.
use super::ParseError;
use crate::diff::model::DiffPath;
use crate::types::ObjectId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameAttribution {
    pub commit: Option<ObjectId>,
    pub author: Vec<u8>,
    pub author_mail: Vec<u8>,
    pub author_time_unix: i64,
    pub summary: Vec<u8>,
    pub original_path: DiffPath,
    pub boundary: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameLine {
    pub final_line: u64,
    pub original_line: u64,
    pub content: Vec<u8>,
    pub attribution: BlameAttribution,
}

#[derive(Default)]
struct PendingAttribution {
    commit: Option<ObjectId>,
    original_line: u64,
    final_line: u64,
    author: Vec<u8>,
    author_mail: Vec<u8>,
    author_time_unix: i64,
    summary: Vec<u8>,
    original_path: Option<DiffPath>,
    boundary: bool,
}

/// Parses line-porcelain output without decoding author or content bytes.
///
/// # Errors
/// Rejects malformed attribution records, paths, timestamps, and quoted escapes.
pub fn parse_line_porcelain(bytes: &[u8]) -> Result<Vec<BlameLine>, ParseError> {
    let records = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    let mut cursor = 0;
    while cursor < records.len() {
        let header = trim_lf(records[cursor]);
        cursor += 1;
        if header.is_empty() {
            continue;
        }
        let mut fields = header.split(|byte| *byte == b' ');
        let object = fields.next().ok_or_else(parse_error)?;
        let original_line = parse_u64(fields.next().ok_or_else(parse_error)?)?;
        let final_line = parse_u64(fields.next().ok_or_else(parse_error)?)?;
        let _group_count = fields.next();
        if fields.next().is_some() || object.len() < 40 {
            return Err(parse_error());
        }
        let commit = if object.iter().all(|byte| *byte == b'0') {
            None
        } else {
            Some(
                std::str::from_utf8(object)
                    .ok()
                    .and_then(|object| ObjectId::parse(object).ok())
                    .ok_or_else(parse_error)?,
            )
        };
        let mut pending = PendingAttribution {
            commit,
            original_line,
            final_line,
            ..PendingAttribution::default()
        };
        let mut content = None;
        while cursor < records.len() {
            let record = trim_lf(records[cursor]);
            cursor += 1;
            if let Some(line) = record.strip_prefix(b"\t") {
                content = Some(line.to_vec());
                break;
            }
            if record == b"boundary" {
                pending.boundary = true;
                continue;
            }
            let (key, value) = record
                .iter()
                .position(|byte| *byte == b' ')
                .map_or((record, &[][..]), |space| {
                    (&record[..space], &record[space + 1..])
                });
            match key {
                b"author" => pending.author = value.to_vec(),
                b"author-mail" => pending.author_mail = value.to_vec(),
                b"author-time" => pending.author_time_unix = parse_i64(value)?,
                b"summary" => pending.summary = value.to_vec(),
                b"filename" => pending.original_path = Some(parse_porcelain_path(value)?),
                _ => {}
            }
        }
        let content = content.ok_or_else(parse_error)?;
        let original_path = pending.original_path.ok_or_else(parse_error)?;
        lines.push(BlameLine {
            final_line: pending.final_line,
            original_line: pending.original_line,
            content,
            attribution: BlameAttribution {
                commit: pending.commit,
                author: pending.author,
                author_mail: pending.author_mail,
                author_time_unix: pending.author_time_unix,
                summary: pending.summary,
                original_path,
                boundary: pending.boundary,
            },
        });
    }
    Ok(lines)
}

fn parse_porcelain_path(value: &[u8]) -> Result<DiffPath, ParseError> {
    let bytes = if value.first() == Some(&b'"') && value.last() == Some(&b'"') {
        decode_git_quoted(&value[1..value.len() - 1])?
    } else {
        value.to_vec()
    };
    DiffPath::from_bytes(bytes).map_err(|_| ParseError("blame path is invalid"))
}

fn decode_git_quoted(value: &[u8]) -> Result<Vec<u8>, ParseError> {
    let mut output = Vec::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        if value[cursor] != b'\\' {
            output.push(value[cursor]);
            cursor += 1;
            continue;
        }
        cursor += 1;
        let escaped = *value.get(cursor).ok_or_else(parse_error)?;
        cursor += 1;
        match escaped {
            b'\\' | b'"' => output.push(escaped),
            b'a' => output.push(0x07),
            b'b' => output.push(0x08),
            b't' => output.push(b'\t'),
            b'n' => output.push(b'\n'),
            b'v' => output.push(0x0b),
            b'f' => output.push(0x0c),
            b'r' => output.push(b'\r'),
            b'0'..=b'7' => {
                let mut octal = u16::from(escaped - b'0');
                for _ in 0..2 {
                    let Some(next @ b'0'..=b'7') = value.get(cursor).copied() else {
                        break;
                    };
                    octal = octal * 8 + u16::from(next - b'0');
                    cursor += 1;
                }
                output.push(u8::try_from(octal).map_err(|_| parse_error())?);
            }
            _ => return Err(parse_error()),
        }
    }
    Ok(output)
}

fn trim_lf(record: &[u8]) -> &[u8] {
    record.strip_suffix(b"\n").unwrap_or(record)
}

fn parse_u64(value: &[u8]) -> Result<u64, ParseError> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(parse_error)
}

fn parse_i64(value: &[u8]) -> Result<i64, ParseError> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(parse_error)
}

fn parse_error() -> ParseError {
    ParseError("Git blame porcelain is malformed")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoted_path_parser_preserves_octal_non_utf8_bytes() {
        let path = parse_porcelain_path(br#""dir/\377.rs""#).unwrap();
        assert_eq!(path.as_bytes(), b"dir/\xff.rs");
    }
}
