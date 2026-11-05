//! Atomic ref updates through `git update-ref --stdin -z`.
//!
//! Commands are built only from validated [`RefId`] and [`ObjectId`] values and encoded in
//! Git's NUL-terminated stdin format, so a ref name can never inject a second command: `RefId`
//! rejects every control character, including line feed and NUL, before a transaction exists.
//! `git update-ref --stdin` applies all commands of one input atomically: either every ref
//! changes or none does.

use super::invocation::GitInvocation;
use super::CommandValidationError;
use crate::types::{ObjectId, RefId};

/// What the ref must hold before the change.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RefExpectation {
    /// No check.
    Any,
    /// The ref must not exist.
    Missing,
    /// The ref must point at this object.
    Value(ObjectId),
}

/// One command of a [`RefTransaction`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RefUpdate {
    /// Point `reference` at `new` (creating it when [`RefExpectation`] allows).
    Update {
        reference: RefId,
        new: ObjectId,
        expected: RefExpectation,
    },
    /// Delete `reference`.
    Delete {
        reference: RefId,
        expected: RefExpectation,
    },
    /// Only check `reference` against `expected` ([`RefExpectation::Value`] or
    /// [`RefExpectation::Missing`]).
    Verify {
        reference: RefId,
        expected: RefExpectation,
    },
}

impl RefUpdate {
    #[must_use]
    pub const fn reference(&self) -> &RefId {
        match self {
            Self::Update { reference, .. }
            | Self::Delete { reference, .. }
            | Self::Verify { reference, .. } => reference,
        }
    }
}

/// Largest accepted number of commands in one transaction.
pub const MAX_REF_TRANSACTION_COMMANDS: usize = 4096;

/// An ordered, duplicate-free list of ref commands applied atomically.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefTransaction {
    updates: Vec<RefUpdate>,
}

impl RefTransaction {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a command.
    ///
    /// # Errors
    /// Rejects a second command for the same ref (Git refuses it at execution time), a delete
    /// that expects the ref to be missing, an update whose expected object uses another object
    /// format than the new value, and more than [`MAX_REF_TRANSACTION_COMMANDS`] commands.
    pub fn push(&mut self, update: RefUpdate) -> Result<&mut Self, CommandValidationError> {
        if self.updates.len() >= MAX_REF_TRANSACTION_COMMANDS {
            return Err(CommandValidationError(
                "the ref transaction exceeds its command bound".into(),
            ));
        }
        if self
            .updates
            .iter()
            .any(|existing| existing.reference() == update.reference())
        {
            return Err(CommandValidationError(
                "the ref transaction changes one ref twice".into(),
            ));
        }
        match &update {
            RefUpdate::Delete {
                expected: RefExpectation::Missing,
                ..
            } => {
                return Err(CommandValidationError(
                    "a ref deletion cannot expect a missing ref".into(),
                ));
            }
            RefUpdate::Verify {
                expected: RefExpectation::Any,
                ..
            } => {
                return Err(CommandValidationError(
                    "a ref verification needs an expectation".into(),
                ));
            }
            RefUpdate::Update {
                new,
                expected: RefExpectation::Value(old),
                ..
            } if new.format() != old.format() => {
                return Err(CommandValidationError(
                    "a ref update mixes object formats".into(),
                ));
            }
            _ => {}
        }
        self.updates.push(update);
        Ok(self)
    }

    #[must_use]
    pub fn updates(&self) -> &[RefUpdate] {
        &self.updates
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    /// The exact `-z` standard input.
    #[must_use]
    pub fn stdin_bytes(&self) -> Vec<u8> {
        fn old_value(bytes: &mut Vec<u8>, expected: &RefExpectation, length: usize) {
            match expected {
                RefExpectation::Any => {}
                RefExpectation::Missing => bytes.extend(std::iter::repeat_n(b'0', length)),
                RefExpectation::Value(old) => bytes.extend_from_slice(old.as_str().as_bytes()),
            }
            bytes.push(0);
        }
        let mut bytes = Vec::new();
        for update in &self.updates {
            let (verb, reference) = match update {
                RefUpdate::Update { reference, .. } => ("update ", reference),
                RefUpdate::Delete { reference, .. } => ("delete ", reference),
                RefUpdate::Verify { reference, .. } => ("verify ", reference),
            };
            bytes.extend_from_slice(verb.as_bytes());
            bytes.extend_from_slice(reference.as_bytes());
            bytes.push(0);
            match update {
                RefUpdate::Update { new, expected, .. } => {
                    bytes.extend_from_slice(new.as_str().as_bytes());
                    bytes.push(0);
                    old_value(&mut bytes, expected, new.as_str().len());
                }
                // An empty old value means "no check" for delete and "must not exist" for
                // verify; both are what the accepted expectations ask for.
                RefUpdate::Delete { expected, .. } | RefUpdate::Verify { expected, .. } => {
                    old_value(&mut bytes, expected, 0);
                }
            }
        }
        bytes
    }

    /// `git update-ref --stdin -z` with the transaction on stdin.
    ///
    /// # Errors
    /// Rejects an empty transaction.
    pub fn invocation(&self) -> Result<GitInvocation, CommandValidationError> {
        if self.updates.is_empty() {
            return Err(CommandValidationError(
                "a ref transaction needs at least one command".into(),
            ));
        }
        Ok(GitInvocation::visible(["update-ref", "--stdin", "-z"]).stdin_bytes(self.stdin_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_with_separators_never_reach_a_transaction() {
        for name in [
            &b"refs/heads/a\nupdate refs/heads/b"[..],
            b"refs/heads/a\0b",
            b"refs/heads/a\rb",
        ] {
            assert!(RefId::from_bytes(name.to_vec()).is_err());
        }
    }

    #[test]
    fn encodes_nul_terminated_commands() {
        let a = ObjectId::parse("a".repeat(40)).unwrap();
        let b = ObjectId::parse("b".repeat(40)).unwrap();
        let mut transaction = RefTransaction::new();
        transaction
            .push(RefUpdate::Update {
                reference: RefId::parse("refs/heads/x").unwrap(),
                new: a.clone(),
                expected: RefExpectation::Missing,
            })
            .unwrap()
            .push(RefUpdate::Delete {
                reference: RefId::parse("refs/heads/y").unwrap(),
                expected: RefExpectation::Value(b.clone()),
            })
            .unwrap();
        let expected = format!(
            "update refs/heads/x\0{}\0{}\0delete refs/heads/y\0{}\0",
            "a".repeat(40),
            "0".repeat(40),
            "b".repeat(40)
        );
        assert_eq!(transaction.stdin_bytes(), expected.as_bytes());
        assert!(transaction
            .push(RefUpdate::Verify {
                reference: RefId::parse("refs/heads/x").unwrap(),
                expected: RefExpectation::Missing,
            })
            .is_err());
        assert!(RefTransaction::new()
            .push(RefUpdate::Verify {
                reference: RefId::parse("refs/heads/v").unwrap(),
                expected: RefExpectation::Any,
            })
            .is_err());
        assert!(RefTransaction::new()
            .push(RefUpdate::Delete {
                reference: RefId::parse("refs/heads/z").unwrap(),
                expected: RefExpectation::Missing,
            })
            .is_err());
        assert!(RefTransaction::new().invocation().is_err());
    }
}
