//! `git bisect` invocations. Session state is read from disk by
//! [`crate::status::discover_operations_checked`] and the bisect log parser in
//! [`crate::history::search`].

use crate::types::ObjectId;

/// One bisect step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BisectRequest {
    /// `git bisect start <bad> <good>`.
    Start { bad: ObjectId, good: ObjectId },
    /// `git bisect good`: marks the checked-out commit.
    Good,
    /// `git bisect bad`.
    Bad,
    /// `git bisect skip`.
    Skip,
    /// `git bisect reset`: ends the session and returns to the original branch.
    Reset,
}

/// Builds the argument vector for a bisect step. Object IDs are validated by their type.
#[must_use]
pub fn bisect_arguments(request: &BisectRequest) -> Vec<String> {
    let mut arguments = vec!["bisect".to_owned()];
    match request {
        BisectRequest::Start { bad, good } => arguments.extend([
            "start".to_owned(),
            bad.as_str().to_owned(),
            good.as_str().to_owned(),
        ]),
        BisectRequest::Good => arguments.push("good".to_owned()),
        BisectRequest::Bad => arguments.push("bad".to_owned()),
        BisectRequest::Skip => arguments.push("skip".to_owned()),
        BisectRequest::Reset => arguments.push("reset".to_owned()),
    }
    arguments
}
