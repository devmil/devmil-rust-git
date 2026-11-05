mod support;

use devmil_git::diff::{parse_unified_hunk_header, HunkHeader};
use std::fmt::Write as _;
use support::Fixture;

#[test]
fn headers_of_a_real_git_diff_parse_with_their_counts_and_sections() {
    let fixture = Fixture::new("hunk-headers");
    let mut before = String::new();
    for index in 0..40 {
        writeln!(before, "line {index}").unwrap();
    }
    fixture.commit_file("file.txt", before.as_bytes(), "before");
    let after = before
        .replace("line 2\n", "line two\n")
        .replace("line 30\n", "")
        .replace("line 39\n", "line 39\nadded\n");
    fixture.write("file.txt", after.as_bytes());
    let diff = fixture.git(&["diff", "--no-color", "-U1", "--", "file.txt"]);
    let headers = diff
        .lines()
        .filter(|line| line.starts_with("@@"))
        .map(|line| parse_unified_hunk_header(line.as_bytes()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(headers.len(), 3);
    assert_eq!(
        headers[0],
        HunkHeader {
            old_start: 2,
            old_count: 3,
            new_start: 2,
            new_count: 3,
            // Git's default function context is the nearest preceding unindented line.
            section: b" line 0".to_vec(),
        }
    );
    assert_eq!((headers[1].old_count, headers[1].new_count), (3, 2));
    assert_eq!((headers[2].new_start, headers[2].new_count), (39, 2));
    let single = parse_unified_hunk_header(b"@@ -0,0 +1 @@ fn main").unwrap();
    assert_eq!((single.new_start, single.new_count), (1, 1));
    assert_eq!(single.section, b" fn main");
    assert!(parse_unified_hunk_header(b"--- a/file.txt").is_err());
}
