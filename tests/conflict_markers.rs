mod support;

use devmil_git::merge::markers::{
    marker_free, marker_size_from_attribute, parse, DEFAULT_MARKER_SIZE,
};
use support::Fixture;

fn conflicted(
    style: &str,
    attributes: Option<&str>,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> (Fixture, Vec<u8>) {
    let fixture = Fixture::new("markers");
    if let Some(attributes) = attributes {
        fixture.commit_file(".gitattributes", attributes.as_bytes(), "attributes");
    }
    fixture.commit_file("file.md", base, "file base");
    fixture.git(&["checkout", "-qb", "side"]);
    fixture.commit_file("file.md", theirs, "side");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.commit_file("file.md", ours, "main");
    let style = format!("merge.conflictStyle={style}");
    assert!(!fixture
        .git_status(&["-c", &style, "merge", "side"])
        .status
        .success());
    let bytes = fixture.read("file.md");
    (fixture, bytes)
}

#[test]
fn git_merge_diff3_and_zdiff3_output_parses() {
    let base = b"head\nTitle\n==========\nshared\nbase\ntail\n";
    let ours = b"head\nTitle\n==========\nshared\nours\ntail\n";
    let theirs = b"head\nTitle\n==========\nshared\ntheirs\ntail\n";
    for style in ["merge", "diff3", "zdiff3"] {
        let (_fixture, bytes) = conflicted(style, None, base, ours, theirs);
        let scan = parse(&bytes, DEFAULT_MARKER_SIZE);
        assert!(scan.well_formed(), "{style}");
        assert_eq!(scan.regions.len(), 1, "{style}");
        let region = &scan.regions[0];
        assert_eq!(&bytes[region.current.clone()], b"ours\n", "{style}");
        assert_eq!(&bytes[region.incoming.clone()], b"theirs\n", "{style}");
        assert_eq!(&bytes[region.current_label.clone()], b"HEAD", "{style}");
        assert_eq!(&bytes[region.incoming_label.clone()], b"side", "{style}");
        if style == "merge" {
            assert!(region.ancestor.is_none());
        } else {
            assert_eq!(
                &bytes[region.ancestor.clone().unwrap()],
                b"base\n",
                "{style}"
            );
        }
        // The Markdown underline outside the block is content.
        let mut resolved = bytes[..region.block.start].to_vec();
        resolved.extend_from_slice(&bytes[region.current.clone()]);
        resolved.extend_from_slice(&bytes[region.block.end..]);
        assert!(marker_free(&resolved, DEFAULT_MARKER_SIZE));
        assert_eq!(resolved, ours);
    }
}

#[test]
fn crlf_and_conflict_marker_size_attribute_are_honoured() {
    let (_fixture, bytes) = conflicted(
        "merge",
        None,
        b"a\r\nbase\r\nz\r\n",
        b"a\r\nours\r\nz\r\n",
        b"a\r\ntheirs\r\nz\r\n",
    );
    let scan = parse(&bytes, 7);
    assert_eq!(&bytes[scan.regions[0].current.clone()], b"ours\r\n");
    assert_eq!(&bytes[scan.regions[0].current_label.clone()], b"HEAD");

    let (fixture, bytes) = conflicted(
        "merge",
        Some("*.md conflict-marker-size=12\n"),
        b"a\nbase\nz\n",
        b"a\nours\nz\n",
        b"a\ntheirs\nz\n",
    );
    let attribute = fixture.git(&["check-attr", "conflict-marker-size", "--", "file.md"]);
    let size = marker_size_from_attribute(attribute.rsplit(": ").next().unwrap().as_bytes());
    assert_eq!(size, 12);
    assert!(marker_free(&bytes, 7));
    let scan = parse(&bytes, size);
    assert_eq!(scan.regions.len(), 1);
    assert_eq!(&bytes[scan.regions[0].incoming.clone()], b"theirs\n");
}
