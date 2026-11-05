//! Byte-level conflict-marker parsing that follows Git's marker shapes.
//!
//! Git writes a conflict as
//!
//! ```text
//! <<<<<<< current-label
//! current side
//! ||||||| ancestor-label      (diff3 and zdiff3 only)
//! ancestor side
//! =======
//! incoming side
//! >>>>>>> incoming-label
//! ```
//!
//! with runs of `conflict-marker-size` characters ([`DEFAULT_MARKER_SIZE`] unless the path's
//! `conflict-marker-size` attribute says otherwise). A `<`, `|` or `>` marker is exactly that many
//! characters followed by a space or the end of the line; a `=` marker is exactly that many
//! characters followed by the end of the line. A trailing `\r` before `\n` belongs to the line
//! ending. Longer or shorter runs, such as a Markdown `==========` underline, are content.
//!
//! The parser never decodes text: it returns byte ranges into the caller's buffer so non-UTF-8
//! content and CRLF files keep their exact bytes.

use std::ops::Range;

/// Git's default `conflict-marker-size`.
pub const DEFAULT_MARKER_SIZE: usize = 7;

/// Which marker a line is.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MarkerKind {
    /// `<<<<<<<`: the start of a conflict and of the current side.
    Current,
    /// `|||||||`: the start of the ancestor side (diff3/zdiff3).
    Ancestor,
    /// `=======`: the start of the incoming side.
    Separator,
    /// `>>>>>>>`: the end of a conflict.
    Incoming,
}

/// One marker line: its kind and the label after the separating space (possibly empty).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkerLine {
    pub kind: MarkerKind,
    /// The whole line including its line ending.
    pub line: Range<usize>,
    /// The label bytes, excluding the separating space and the line ending.
    pub label: Range<usize>,
}

/// One complete conflict block. Every range indexes the parsed buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictRegion {
    /// From the start of the `<` marker line to the end of the `>` marker line (including its
    /// line ending, if any).
    pub block: Range<usize>,
    /// The current side, between the `<` line and the `|` or `=` line.
    pub current: Range<usize>,
    /// The ancestor side between the `|` and `=` lines, for diff3/zdiff3 conflicts.
    pub ancestor: Option<Range<usize>>,
    /// The incoming side, between the `=` and `>` lines.
    pub incoming: Range<usize>,
    /// Label of the `<` line.
    pub current_label: Range<usize>,
    /// Label of the `|` line.
    pub ancestor_label: Option<Range<usize>>,
    /// Label of the `>` line.
    pub incoming_label: Range<usize>,
}

/// The result of scanning a buffer for conflicts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConflictScan {
    /// Complete conflict blocks in buffer order.
    pub regions: Vec<ConflictRegion>,
    /// Marker lines that do not belong to a complete block: an unterminated region, a stray
    /// separator or end marker, or markers out of order. Their presence means the buffer is not
    /// marker-free even if `regions` is empty.
    pub stray_markers: Vec<MarkerLine>,
}

impl ConflictScan {
    /// Whether the buffer holds no marker line at all (resolved).
    #[must_use]
    pub fn marker_free(&self) -> bool {
        self.regions.is_empty() && self.stray_markers.is_empty()
    }

    /// Whether every marker line belongs to a complete block.
    #[must_use]
    pub fn well_formed(&self) -> bool {
        self.stray_markers.is_empty()
    }
}

/// Classifies one line, given without its `\n`. A trailing `\r` is treated as line ending.
/// `marker_size` values below 1 are treated as 1.
#[must_use]
pub fn classify_line(line: &[u8], marker_size: usize) -> Option<(MarkerKind, Range<usize>)> {
    let size = marker_size.max(1);
    let content = line.strip_suffix(b"\r").unwrap_or(line);
    let kind = match content.first()? {
        b'<' => MarkerKind::Current,
        b'|' => MarkerKind::Ancestor,
        b'=' => MarkerKind::Separator,
        b'>' => MarkerKind::Incoming,
        _ => return None,
    };
    let character = content[0];
    if content.len() < size || !content[..size].iter().all(|byte| *byte == character) {
        return None;
    }
    let rest = &content[size..];
    match (kind, rest.first()) {
        (_, None) => Some((kind, size..size)),
        (MarkerKind::Separator, Some(_)) => None,
        (_, Some(b' ')) => Some((kind, size + 1..content.len())),
        _ => None,
    }
}

struct Line {
    start: usize,
    content_end: usize,
    end: usize,
}

fn lines(bytes: &[u8]) -> impl Iterator<Item = Line> + '_ {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= bytes.len() {
            return None;
        }
        let end = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |offset| start + offset + 1);
        let content_end = if bytes[end - 1] == b'\n' {
            end - 1
        } else {
            end
        };
        let line = Line {
            start,
            content_end,
            end,
        };
        start = end;
        Some(line)
    })
}

/// Lists every marker line in buffer order.
#[must_use]
pub fn marker_lines(bytes: &[u8], marker_size: usize) -> Vec<MarkerLine> {
    lines(bytes)
        .filter_map(|line| {
            classify_line(&bytes[line.start..line.content_end], marker_size).map(|(kind, label)| {
                MarkerLine {
                    kind,
                    line: line.start..line.end,
                    label: line.start + label.start..line.start + label.end,
                }
            })
        })
        .collect()
}

/// Whether the buffer contains no conflict-marker line of `marker_size`.
#[must_use]
pub fn marker_free(bytes: &[u8], marker_size: usize) -> bool {
    lines(bytes)
        .all(|line| classify_line(&bytes[line.start..line.content_end], marker_size).is_none())
}

/// Parses every conflict block. Blocks must appear as `<`, optional `|`, `=`, `>`; anything
/// else is reported in [`ConflictScan::stray_markers`]. A `<` marker inside an open block
/// abandons the open block (its markers become stray) and starts a new one.
#[must_use]
pub fn parse(bytes: &[u8], marker_size: usize) -> ConflictScan {
    #[derive(Default)]
    struct Open {
        current: Option<MarkerLine>,
        ancestor: Option<MarkerLine>,
        separator: Option<MarkerLine>,
    }
    impl Open {
        fn drain(&mut self, stray: &mut Vec<MarkerLine>) {
            stray.extend(
                [
                    self.current.take(),
                    self.ancestor.take(),
                    self.separator.take(),
                ]
                .into_iter()
                .flatten(),
            );
        }
    }
    let mut scan = ConflictScan::default();
    let mut open = Open::default();
    for marker in marker_lines(bytes, marker_size) {
        match marker.kind {
            MarkerKind::Current => {
                open.drain(&mut scan.stray_markers);
                open.current = Some(marker);
            }
            MarkerKind::Ancestor
                if open.current.is_some()
                    && open.ancestor.is_none()
                    && open.separator.is_none() =>
            {
                open.ancestor = Some(marker);
            }
            MarkerKind::Separator if open.current.is_some() && open.separator.is_none() => {
                open.separator = Some(marker);
            }
            MarkerKind::Incoming if open.separator.is_some() => {
                // A separator is only accepted after a start marker, so both are present.
                if let (Some(current), Some(separator)) =
                    (open.current.take(), open.separator.take())
                {
                    let ancestor = open.ancestor.take();
                    scan.regions.push(ConflictRegion {
                        block: current.line.start..marker.line.end,
                        current: current.line.end
                            ..ancestor
                                .as_ref()
                                .map_or(separator.line.start, |ancestor| ancestor.line.start),
                        ancestor: ancestor
                            .as_ref()
                            .map(|ancestor| ancestor.line.end..separator.line.start),
                        incoming: separator.line.end..marker.line.start,
                        current_label: current.label,
                        ancestor_label: ancestor.map(|ancestor| ancestor.label),
                        incoming_label: marker.label,
                    });
                }
            }
            _ => {
                open.drain(&mut scan.stray_markers);
                scan.stray_markers.push(marker);
            }
        }
    }
    open.drain(&mut scan.stray_markers);
    scan.stray_markers.sort_by_key(|marker| marker.line.start);
    scan
}

/// Reads `conflict-marker-size` from a `git check-attr conflict-marker-size -- <path>` value.
/// `unspecified`, `unset`, `set` and invalid values select [`DEFAULT_MARKER_SIZE`], as Git does.
#[must_use]
pub fn marker_size_from_attribute(value: &[u8]) -> usize {
    std::str::from_utf8(value)
        .ok()
        .and_then(|text| text.trim().parse::<usize>().ok())
        .filter(|size| *size > 0)
        .unwrap_or(DEFAULT_MARKER_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice<'a>(bytes: &'a [u8], range: &Range<usize>) -> &'a [u8] {
        &bytes[range.clone()]
    }

    #[test]
    fn merge_and_diff3_regions_report_exact_ranges_and_labels() {
        let merge = b"before\n<<<<<<< HEAD\ncurrent\n=======\nincoming\n>>>>>>> topic\nafter\n";
        let scan = parse(merge, DEFAULT_MARKER_SIZE);
        assert!(scan.well_formed());
        let region = &scan.regions[0];
        assert_eq!(slice(merge, &region.current), b"current\n");
        assert_eq!(slice(merge, &region.incoming), b"incoming\n");
        assert_eq!(region.ancestor, None);
        assert_eq!(slice(merge, &region.current_label), b"HEAD");
        assert_eq!(slice(merge, &region.incoming_label), b"topic");
        assert_eq!(region.block, 7..merge.len() - 6);

        let diff3 =
            b"<<<<<<< HEAD\ncurrent\n||||||| base\nancestor\n=======\nincoming\n>>>>>>> topic\n";
        let region = &parse(diff3, 7).regions[0];
        assert_eq!(
            slice(diff3, region.ancestor.as_ref().unwrap()),
            b"ancestor\n"
        );
        assert_eq!(
            slice(diff3, region.ancestor_label.as_ref().unwrap()),
            b"base"
        );
        assert_eq!(region.block, 0..diff3.len());
    }

    #[test]
    fn crlf_empty_sides_and_unterminated_regions() {
        let crlf = b"<<<<<<< ours\r\na\r\n=======\r\n>>>>>>> theirs\r\n";
        let scan = parse(crlf, 7);
        assert_eq!(slice(crlf, &scan.regions[0].current), b"a\r\n");
        assert!(scan.regions[0].incoming.is_empty());
        assert_eq!(slice(crlf, &scan.regions[0].incoming_label), b"theirs");

        let empty = b"<<<<<<<\n=======\n>>>>>>>";
        let scan = parse(empty, 7);
        assert_eq!(scan.regions.len(), 1);
        assert!(scan.regions[0].current.is_empty());
        assert!(scan.regions[0].current_label.is_empty());
        assert_eq!(scan.regions[0].block, 0..empty.len());

        let unterminated = b"<<<<<<< HEAD\na\n=======\nb\n";
        let scan = parse(unterminated, 7);
        assert!(scan.regions.is_empty());
        assert_eq!(scan.stray_markers.len(), 2);
        assert!(!scan.marker_free());
        let stray = parse(b"text\n>>>>>>> x\n", 7);
        assert_eq!(stray.stray_markers[0].kind, MarkerKind::Incoming);
    }

    #[test]
    fn marker_size_controls_shape_and_longer_runs_are_content() {
        let markdown = b"Title\n==========\n<<<<<<<< not\n";
        assert!(marker_free(markdown, 7));
        let wide = b"<<<<<<<<<< a\nx\n==========\ny\n>>>>>>>>>> b\n";
        assert!(marker_free(wide, 7));
        let scan = parse(wide, 10);
        assert_eq!(scan.regions.len(), 1);
        assert_eq!(slice(wide, &scan.regions[0].current), b"x\n");
        // Nested inner conflicts of a recursive merge use a wider marker and stay content.
        let nested = b"<<<<<<< a\n<<<<<<<<< inner\nx\n=========\ny\n>>>>>>>>> inner\n=======\nz\n>>>>>>> b\n";
        let scan = parse(nested, 7);
        assert_eq!(scan.regions.len(), 1);
        assert!(scan.well_formed());
        assert_eq!(marker_size_from_attribute(b"12\n"), 12);
        assert_eq!(marker_size_from_attribute(b"unspecified\n"), 7);
        assert_eq!(marker_size_from_attribute(b"0"), 7);
    }

    #[test]
    fn non_utf8_content_is_preserved() {
        let bytes = b"<<<<<<< \xff\n\xfe\n=======\n\x80\n>>>>>>> \xfd\n";
        let region = &parse(bytes, 7).regions[0];
        assert_eq!(slice(bytes, &region.current), b"\xfe\n");
        assert_eq!(slice(bytes, &region.current_label), b"\xff");
    }

    #[test]
    fn agrees_with_gitlama_conflict_marker_fixture() {
        // Copy of GitLama `test/fixtures/conflict_markers.json` at marker size 7.
        let markers = [
            "<<<<<<< HEAD\n",
            "<<<<<<<\n",
            "text\n||||||| base\n",
            "text\n=======\nmore\n",
            ">>>>>>> topic",
            "a\r\n=======\r\nb\r\n",
        ];
        let content = [
            "",
            "a clean result\n",
            "Title\n==========\n",
            "Title\n======\n",
            "<<<<<<<< not a marker\n",
            "x <<<<<<< HEAD\n",
            ">>>>>>>>\n",
            "=======x\n",
        ];
        for text in markers {
            assert!(!marker_free(text.as_bytes(), 7), "{text:?}");
            assert!(!parse(text.as_bytes(), 7).marker_free(), "{text:?}");
        }
        for text in content {
            assert!(marker_free(text.as_bytes(), 7), "{text:?}");
            assert!(parse(text.as_bytes(), 7).marker_free(), "{text:?}");
        }
    }
}
