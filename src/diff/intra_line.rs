//! Intra-line change spans for paired deletions and additions.
//!
//! Git's unified diff marks whole lines. [`derive_intra_line_spans`] pairs each run of deleted
//! lines with the following run of added lines (first with first, and so on) and marks the bytes
//! between their common prefix and common suffix. Span boundaries never split a UTF-8
//! character, and non-UTF-8 lines get no span. Parsed lines keep their `intra_line` empty;
//! consumers that want highlights call [`fill_intra_line_spans`] explicitly.

use super::model::{ByteSpan, DiffHunk, DiffLine, DiffLineOrigin};

/// One span list per line of `lines` (empty for unpaired or unchanged lines).
#[must_use]
pub fn derive_intra_line_spans(lines: &[DiffLine]) -> Vec<Vec<ByteSpan>> {
    let mut spans = vec![Vec::new(); lines.len()];
    let mut cursor = 0;
    while cursor < lines.len() {
        if lines[cursor].origin != DiffLineOrigin::Deletion {
            cursor += 1;
            continue;
        }
        let deleted_start = cursor;
        while cursor < lines.len() && lines[cursor].origin == DiffLineOrigin::Deletion {
            cursor += 1;
        }
        let added_start = cursor;
        while cursor < lines.len() && lines[cursor].origin == DiffLineOrigin::Addition {
            cursor += 1;
        }
        let pair_count = (added_start - deleted_start).min(cursor - added_start);
        for offset in 0..pair_count {
            let deleted_index = deleted_start + offset;
            let added_index = added_start + offset;
            if let Some((deleted, added)) =
                replacement_spans(&lines[deleted_index].source, &lines[added_index].source)
            {
                if let Some(deleted) = deleted {
                    spans[deleted_index].push(deleted);
                }
                if let Some(added) = added {
                    spans[added_index].push(added);
                }
            }
        }
    }
    spans
}

/// Fills `intra_line` of every line of `hunk` that has none yet.
pub fn fill_intra_line_spans(hunk: &mut DiffHunk) {
    let derived = derive_intra_line_spans(&hunk.lines);
    for (line, spans) in hunk.lines.iter_mut().zip(derived) {
        if line.intra_line.is_empty() {
            line.intra_line = spans;
        }
    }
}

/// The changed span of a deleted and an added line, between their common prefix and suffix.
/// Returns `None` for equal lines, non-UTF-8 lines, or lines without any common prefix or suffix
/// (a complete rewrite highlights nothing). Either side may be `None` when it only lost or
/// gained bytes.
#[must_use]
pub fn replacement_spans(
    deleted: &[u8],
    added: &[u8],
) -> Option<(Option<ByteSpan>, Option<ByteSpan>)> {
    if deleted == added
        || std::str::from_utf8(deleted).is_err()
        || std::str::from_utf8(added).is_err()
    {
        return None;
    }
    let mut prefix = deleted
        .iter()
        .zip(added)
        .take_while(|(left, right)| left == right)
        .count();
    while prefix > 0
        && (std::str::from_utf8(&deleted[..prefix]).is_err()
            || std::str::from_utf8(&added[..prefix]).is_err())
    {
        prefix -= 1;
    }
    let remaining_deleted = deleted.len() - prefix;
    let remaining_added = added.len() - prefix;
    let mut suffix = deleted[prefix..]
        .iter()
        .rev()
        .zip(added[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
        .min(remaining_deleted)
        .min(remaining_added);
    while suffix > 0
        && (std::str::from_utf8(&deleted[deleted.len() - suffix..]).is_err()
            || std::str::from_utf8(&added[added.len() - suffix..]).is_err())
    {
        suffix -= 1;
    }
    let deleted_end = deleted.len() - suffix;
    let added_end = added.len() - suffix;
    if prefix == 0 && suffix == 0 {
        return None;
    }
    let deleted_span = ByteSpan::new(prefix, deleted_end).ok();
    let added_span = ByteSpan::new(prefix, added_end).ok();
    (deleted_span.is_some() || added_span.is_some()).then_some((deleted_span, added_span))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacements_mark_only_the_changed_characters() {
        let (deleted, added) = replacement_spans(
            br#"version = "0.1.0-beta.7""#,
            br#"version = "0.1.0-beta.8""#,
        )
        .unwrap();
        let (deleted, added) = (deleted.unwrap(), added.unwrap());
        assert_eq!(
            &br#"version = "0.1.0-beta.7""#[deleted.start..deleted.end],
            b"7"
        );
        assert_eq!(
            &br#"version = "0.1.0-beta.8""#[added.start..added.end],
            b"8"
        );
        // NFC and NFD spellings differ inside one character; spans stay on boundaries.
        let (deleted, added) = replacement_spans(
            "name = caf\u{e9}".as_bytes(),
            "name = cafe\u{301}".as_bytes(),
        )
        .unwrap();
        let (deleted, added) = (deleted.unwrap(), added.unwrap());
        assert!("name = caf\u{e9}".get(deleted.start..deleted.end).is_some());
        assert!("name = cafe\u{301}".get(added.start..added.end).is_some());
        let (deleted, added) = replacement_spans(b"name", b"name = value").unwrap();
        assert!(deleted.is_none());
        assert_eq!(&b"name = value"[added.unwrap().start..], b" = value");
        assert_eq!(replacement_spans(b"abc", b"xyz"), None);
        assert_eq!(replacement_spans(b"same", b"same"), None);
        assert_eq!(replacement_spans(b"\xff", b"\xfe"), None);
    }
}
