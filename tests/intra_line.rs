mod support;

use devmil_git::diff::intra_line::fill_intra_line_spans;
use devmil_git::diff::model::DiffLineOrigin;
use devmil_git::diff::{parse_unified_patch, PatchLimits};
use support::Fixture;

#[test]
fn spans_are_derived_for_a_real_git_patch() {
    let fixture = Fixture::new("intra-line");
    fixture.commit_file("v.toml", b"name = \"x\"\nversion = \"1.0.7\"\nkeep\n", "v");
    fixture.write("v.toml", b"name = \"x\"\nversion = \"1.0.8\"\nkeep\n");
    let patch = fixture.git(&["diff", "--no-color", "--", "v.toml"]);
    let mut document = parse_unified_patch(patch.as_bytes(), PatchLimits::default()).unwrap();
    let hunk = &mut document.files[0].hunks[0];
    assert!(hunk.lines.iter().all(|line| line.intra_line.is_empty()));
    fill_intra_line_spans(hunk);
    for line in &hunk.lines {
        match line.origin {
            DiffLineOrigin::Deletion | DiffLineOrigin::Addition => {
                let span = line.intra_line[0];
                assert_eq!(line.source.len() - span.start, 2);
                assert_eq!(span.end - span.start, 1);
            }
            DiffLineOrigin::Context => assert!(line.intra_line.is_empty()),
        }
    }
}

#[test]
fn subject_log_keeps_empty_subjects_in_place() {
    use devmil_git::history::{parse_subject_log, SUBJECT_LOG_FORMAT};
    let fixture = Fixture::new("subject-log");
    let base = fixture.head();
    fixture.git(&[
        "commit",
        "-q",
        "--allow-empty",
        "--allow-empty-message",
        "-m",
        "",
    ]);
    let empty = fixture.head();
    let last = fixture.commit_file("x", b"x", "last subject");
    let output = fixture.git(&[
        "log",
        "-z",
        "--reverse",
        &format!("--format={SUBJECT_LOG_FORMAT}"),
        &format!("{base}..HEAD"),
    ]);
    let records = parse_subject_log(output.as_bytes()).unwrap();
    let records: Vec<(String, Vec<u8>)> = records
        .into_iter()
        .map(|(id, subject)| (id.as_str().to_owned(), subject))
        .collect();
    assert_eq!(
        records,
        [(empty, Vec::new()), (last, b"last subject".to_vec())]
    );
    assert!(parse_subject_log(b"not an id\0subject").is_err());
}
