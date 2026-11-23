//! Peeling many objects with one `git cat-file --batch-check`, tolerating missing ones.
//!
//! `for-each-ref` fields that read an object (`%(*objectname)`, `%(objecttype)`) make Git
//! fail for the whole listing when one reference names an object the repository does not
//! have (a partial copy, a pruned pack). Some Git releases even abort with a double free.
//! Listing names and object IDs reads no object; this module then resolves every target in
//! a single process and reports a missing one as `None` instead of failing.

use crate::types::ObjectId;

/// `cat-file --batch-check` format the parser reads.
pub const PEEL_FORMAT: &str = "%(objectname) %(objecttype)";

/// The kind of object a reference peels to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeeledKind {
    Commit,
    Tree,
    Blob,
    Tag,
}

/// The object a target peels to (annotated tags to what they name, recursively).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Peeled {
    pub object: ObjectId,
    pub kind: PeeledKind,
}

/// Malformed `cat-file --batch-check` output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeelParseError;

impl std::fmt::Display for PeelParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid Git batch-check output")
    }
}

impl std::error::Error for PeelParseError {}

/// Standard input for `git cat-file --batch-check=<PEEL_FORMAT>`: one `<object>^{}` per line.
#[must_use]
pub fn peel_input(objects: &[ObjectId]) -> Vec<u8> {
    let mut input = Vec::with_capacity(objects.len() * 44);
    for object in objects {
        input.extend_from_slice(object.as_str().as_bytes());
        input.extend_from_slice(b"^{}\n");
    }
    input
}

/// Parses the output for `count` inputs, in input order: `None` for an object the
/// repository does not have (or names ambiguously).
///
/// # Errors
/// Returns [`PeelParseError`] when the output does not hold one valid line per input.
pub fn parse_peeled(count: usize, output: &[u8]) -> Result<Vec<Option<Peeled>>, PeelParseError> {
    let mut result = Vec::with_capacity(count);
    for line in output.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(line).map_err(|_| PeelParseError)?;
        let (object, kind) = text.rsplit_once(' ').ok_or(PeelParseError)?;
        let kind = match kind {
            "missing" | "ambiguous" => {
                result.push(None);
                continue;
            }
            "commit" => PeeledKind::Commit,
            "tree" => PeeledKind::Tree,
            "blob" => PeeledKind::Blob,
            "tag" => PeeledKind::Tag,
            _ => return Err(PeelParseError),
        };
        let object = ObjectId::parse(object).map_err(|_| PeelParseError)?;
        result.push(Some(Peeled { object, kind }));
    }
    if result.len() == count {
        Ok(result)
    } else {
        Err(PeelParseError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: char) -> ObjectId {
        ObjectId::parse(std::iter::repeat_n(byte, 40).collect::<String>()).unwrap()
    }

    #[test]
    fn input_asks_for_each_peeled_object() {
        assert_eq!(
            peel_input(&[id('a'), id('b')]),
            format!("{}^{{}}\n{}^{{}}\n", id('a').as_str(), id('b').as_str()).into_bytes()
        );
    }

    #[test]
    fn missing_objects_are_none_and_order_is_kept() {
        let output = format!(
            "{} commit\n{}^{{}} missing\n{} tree\n",
            id('c').as_str(),
            id('b').as_str(),
            id('d').as_str()
        );
        assert_eq!(
            parse_peeled(3, output.as_bytes()).unwrap(),
            vec![
                Some(Peeled {
                    object: id('c'),
                    kind: PeeledKind::Commit
                }),
                None,
                Some(Peeled {
                    object: id('d'),
                    kind: PeeledKind::Tree
                }),
            ]
        );
    }

    #[test]
    fn a_short_or_unknown_answer_is_malformed() {
        let output = format!("{} commit\n", id('c').as_str());
        assert_eq!(parse_peeled(2, output.as_bytes()), Err(PeelParseError));
        let output = format!("{} gizmo\n", id('c').as_str());
        assert_eq!(parse_peeled(1, output.as_bytes()), Err(PeelParseError));
    }
}
