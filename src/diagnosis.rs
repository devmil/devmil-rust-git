//! One table of known Git failures, recognised from a command's diagnostics.
//!
//! Each [`DiagnosisRule`] names a failure mechanism and the `LC_ALL=C` phrases
//! Git prints for it. [`diagnose`] returns the first matching rule with the
//! references the message names, and [`confirm_case_collision`] upgrades a
//! lock conflict on a ref once repository facts show refs differing only in
//! letter case. Applications map a diagnosis to their own explanation and
//! remedies; nothing here decides what a user should do.
//!
//! Diagnostics are private output: the returned names come from the
//! repository and must not be logged.

/// A failure mechanism this crate recognises.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiagnosisKind {
    /// SSH host-key verification failed.
    HostKey,
    /// Git could not read an object it needs.
    MissingObject,
    /// A credential was required and could not be obtained noninteractively.
    Authentication,
    /// Another process, or this one, held a `*.lock` file Git needed.
    LockConflict,
    /// Two refs differ only in letter case. On a case-insensitive file system
    /// they share one loose-ref file and lock, so one transaction updating
    /// both fails on its own lock. Only [`confirm_case_collision`] reports it.
    RefCaseCollision,
    /// A push was rejected because the remote has commits the local branch lacks.
    RemoteAhead,
    /// A `--force-with-lease` expectation no longer matched the remote.
    StaleLease,
}

/// One entry of the table: a mechanism and the lowercase phrases that show it.
#[derive(Clone, Copy, Debug)]
pub struct DiagnosisRule {
    pub kind: DiagnosisKind,
    phrases: &'static [&'static str],
    lines: Option<fn(&str) -> bool>,
}

impl DiagnosisRule {
    fn matches(&self, lowercase: &str) -> bool {
        self.phrases.iter().any(|phrase| lowercase.contains(phrase))
            || self
                .lines
                .is_some_and(|line_matches| lowercase.lines().any(line_matches))
    }
}

/// The table, in precedence order: the first matching rule wins.
pub const RULES: &[DiagnosisRule] = &[
    DiagnosisRule {
        kind: DiagnosisKind::HostKey,
        phrases: &["host key verification failed"],
        lines: None,
    },
    DiagnosisRule {
        kind: DiagnosisKind::MissingObject,
        phrases: &[
            "unable to read sha1 file of",
            "unable to read object",
            "could not read object",
            "missing blob ",
            "missing tree ",
            "missing commit ",
        ],
        lines: Some(|line| line.contains("object file") && line.contains("is empty")),
    },
    DiagnosisRule {
        kind: DiagnosisKind::Authentication,
        phrases: &[
            "permission denied",
            "authentication failed",
            "could not read username",
            "could not read password",
            "could not read passphrase",
            "unable to read askpass response",
            "could not read askpass response",
            "unable to get username",
            "unable to get password",
            "terminal prompts disabled",
            "requested url returned error: 401",
            "http 401",
            "401 unauthorized",
        ],
        lines: None,
    },
    DiagnosisRule {
        kind: DiagnosisKind::LockConflict,
        phrases: &["another git process seems to be running"],
        lines: Some(|line| {
            line.contains("unable to create '") && line.contains(".lock': file exists")
        }),
    },
    DiagnosisRule {
        kind: DiagnosisKind::StaleLease,
        phrases: &[],
        lines: Some(|line| {
            let line = line.trim();
            line.starts_with("! [rejected]") && line.ends_with("(stale info)")
        }),
    },
    DiagnosisRule {
        kind: DiagnosisKind::RemoteAhead,
        phrases: &[],
        lines: Some(|line| {
            let line = line.trim();
            line.starts_with("! [rejected]")
                && (line.ends_with("(fetch first)") || line.ends_with("(non-fast-forward)"))
        }),
    },
];

/// Whether `stderr` shows `kind`, regardless of the rules before it.
#[must_use]
pub fn shows(kind: DiagnosisKind, stderr: &[u8]) -> bool {
    let lowercase = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    RULES
        .iter()
        .filter(|rule| rule.kind == kind)
        .any(|rule| rule.matches(&lowercase))
}

/// What [`diagnose`] recognised.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnosis {
    pub kind: DiagnosisKind,
    /// The full ref name the failure is about, such as the ref Git could not
    /// lock (`refs/remotes/origin/main`).
    pub reference: Option<String>,
    /// For [`DiagnosisKind::RefCaseCollision`]: the other spellings of `reference`.
    pub related: Vec<String>,
}

/// Recognises the first rule of [`RULES`] in a failed command's diagnostics.
/// The phrases assume `LC_ALL=C`, which callers must set on their Git commands.
#[must_use]
pub fn diagnose(stderr: &[u8]) -> Option<Diagnosis> {
    let text = String::from_utf8_lossy(stderr);
    let lowercase = text.to_ascii_lowercase();
    let rule = RULES.iter().find(|rule| rule.matches(&lowercase))?;
    Some(Diagnosis {
        kind: rule.kind,
        reference: (rule.kind == DiagnosisKind::LockConflict)
            .then(|| locked_reference(&text))
            .flatten(),
        related: Vec::new(),
    })
}

/// Upgrades a lock conflict on a ref to [`DiagnosisKind::RefCaseCollision`]
/// when `known` refs include another spelling of it that differs only in ASCII
/// letter case. `known` should hold the repository's refs and the refs the
/// failed command touched (see [`updated_tracking_refs`]).
#[must_use]
pub fn confirm_case_collision<'a>(
    diagnosis: Diagnosis,
    known: impl IntoIterator<Item = &'a str>,
) -> Diagnosis {
    let Some(reference) = diagnosis
        .reference
        .as_deref()
        .filter(|_| diagnosis.kind == DiagnosisKind::LockConflict)
    else {
        return diagnosis;
    };
    let mut related = Vec::<String>::new();
    for candidate in known {
        if candidate != reference
            && candidate.eq_ignore_ascii_case(reference)
            && !related.iter().any(|known| known == candidate)
        {
            related.push(candidate.to_owned());
        }
    }
    if related.is_empty() {
        return diagnosis;
    }
    related.sort();
    Diagnosis {
        kind: DiagnosisKind::RefCaseCollision,
        reference: diagnosis.reference,
        related,
    }
}

/// The remote-tracking refs a fetch reported updating, creating or deleting,
/// from its `… -> <remote>/<branch>` lines, as full names under
/// `refs/remotes/`. `remotes` are the configured remote names; a branch is
/// attributed to the longest one that prefixes it.
#[must_use]
pub fn updated_tracking_refs(stderr: &[u8], remotes: &[String]) -> Vec<String> {
    let text = String::from_utf8_lossy(stderr);
    let mut refs = Vec::new();
    for line in text.lines() {
        let Some((_, target)) = line.rsplit_once(" -> ") else {
            continue;
        };
        let target = target.split_whitespace().next().unwrap_or_default();
        let known = remotes
            .iter()
            .filter(|remote| {
                target
                    .strip_prefix(remote.as_str())
                    .is_some_and(|branch| branch.len() > 1 && branch.starts_with('/'))
            })
            .max_by_key(|remote| remote.len());
        if known.is_some() {
            refs.push(format!("refs/remotes/{target}"));
        }
    }
    refs
}

/// The ref named in "cannot lock ref '<ref>'", or derived from the lock path
/// in "Unable to create '<git-dir>/<ref>.lock'" for refs under `refs/`.
fn locked_reference(text: &str) -> Option<String> {
    const CANNOT_LOCK: &str = "cannot lock ref '";
    const UNABLE: &str = "Unable to create '";
    for line in text.lines() {
        if let Some(start) = line.find(CANNOT_LOCK) {
            let rest = &line[start + CANNOT_LOCK.len()..];
            if let Some(end) = rest.find('\'') {
                let name = &rest[..end];
                if name.starts_with("refs/") {
                    return Some(name.to_owned());
                }
            }
        }
    }
    text.lines().find_map(|line| {
        let start = line.find(UNABLE)? + UNABLE.len();
        let path = &line[start..];
        let path = &path[..path.find(".lock'")?];
        let path = path.replace('\\', "/");
        let index = path.find("/refs/")?;
        Some(path[index + 1..].to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRUNE_COLLISION: &[u8] = b"error: could not delete references: cannot lock ref \
'refs/remotes/origin/npt/chore/sync': Unable to create \
'/r/.git/refs/remotes/origin/npt/chore/sync.lock': File exists.\n\n\
Another git process seems to be running in this repository\n\
From https://example.invalid/repo\n \
- [deleted]               (none)     -> origin/npt/chore/sync\n \
* [new branch]            NPT/chore/sync -> origin/NPT/chore/sync\n";

    #[test]
    fn every_rule_recognises_its_diagnostic() {
        for (stderr, kind) in [
            (
                b"Host key verification failed.".as_slice(),
                DiagnosisKind::HostKey,
            ),
            (
                b"error: unable to read object 1234",
                DiagnosisKind::MissingObject,
            ),
            (
                b"error: object file .git/objects/ab/cd is empty",
                DiagnosisKind::MissingObject,
            ),
            (
                b"fatal: Authentication failed for 'x'",
                DiagnosisKind::Authentication,
            ),
            (
                b"fatal: Unable to create '/r/.git/index.lock': File exists.",
                DiagnosisKind::LockConflict,
            ),
            (
                b" ! [rejected]        main -> main (stale info)",
                DiagnosisKind::StaleLease,
            ),
            (
                b" ! [rejected]        main -> main (fetch first)",
                DiagnosisKind::RemoteAhead,
            ),
            (
                b" ! [rejected]        main -> main (non-fast-forward)",
                DiagnosisKind::RemoteAhead,
            ),
        ] {
            assert_eq!(diagnose(stderr).map(|value| value.kind), Some(kind));
        }
        assert_eq!(diagnose(b"fatal: something else"), None);
        assert_eq!(
            diagnose(b"fatal: Unable to create '/r/.git/index.lock': Permission denied"),
            Some(Diagnosis {
                kind: DiagnosisKind::Authentication,
                reference: None,
                related: Vec::new(),
            }),
            "the table keeps the transfer classification's precedence"
        );
    }

    #[test]
    fn a_lock_conflict_names_its_ref_and_a_case_variant_confirms_a_collision() {
        let diagnosis = diagnose(PRUNE_COLLISION).unwrap();
        assert_eq!(diagnosis.kind, DiagnosisKind::LockConflict);
        assert_eq!(
            diagnosis.reference.as_deref(),
            Some("refs/remotes/origin/npt/chore/sync")
        );
        let touched = updated_tracking_refs(PRUNE_COLLISION, &["origin".to_owned()]);
        assert_eq!(
            touched,
            [
                "refs/remotes/origin/npt/chore/sync",
                "refs/remotes/origin/NPT/chore/sync"
            ]
        );
        let confirmed =
            confirm_case_collision(diagnosis.clone(), touched.iter().map(String::as_str));
        assert_eq!(confirmed.kind, DiagnosisKind::RefCaseCollision);
        assert_eq!(confirmed.related, ["refs/remotes/origin/NPT/chore/sync"]);
        // Without another spelling it stays a lock conflict.
        let unconfirmed = confirm_case_collision(diagnosis, ["refs/remotes/origin/main"]);
        assert_eq!(unconfirmed.kind, DiagnosisKind::LockConflict);
    }

    #[test]
    fn a_lock_path_names_its_ref_when_git_does_not() {
        let diagnosis = diagnose(
            b"fatal: Unable to create 'C:\\r\\.git\\refs\\heads\\Topic.lock': File exists.",
        )
        .unwrap();
        assert_eq!(diagnosis.reference.as_deref(), Some("refs/heads/Topic"));
        let index =
            diagnose(b"fatal: Unable to create '/r/.git/index.lock': File exists.").unwrap();
        assert_eq!(index.reference, None);
    }

    #[test]
    fn tracking_refs_use_the_longest_configured_remote() {
        let refs = updated_tracking_refs(
            b"   abc..def  main       -> team/origin/main\n \
* [new tag] v1 -> v1\n",
            &["team".to_owned(), "team/origin".to_owned()],
        );
        assert_eq!(refs, ["refs/remotes/team/origin/main"]);
    }
}
