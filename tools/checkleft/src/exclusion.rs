//! Stale-exclusion auditing.
//!
//! Exclusions in `CHECKS.toml` are write-once, forget-forever: they are added to
//! unblock a change and rarely revisited, so the config slowly accretes dead entries
//! that quietly weaken coverage. The stale-exclusion audit keeps the list honest by
//! re-evaluating an exclusion whenever a file it depends on changes in the diff: once
//! the reason the exclusion existed goes away, checkleft surfaces a finding *on the
//! `CHECKS.toml` entry itself* telling you the exclusion can be removed.
//!
//! The audit inverts checkleft's usual model. A normal finding lands on the changed
//! ("left") side of a diff; a stale-exclusion finding lands on a `CHECKS.toml` that
//! did *not* change — it went stale because some *other* file it referenced changed.
//! To compose with the incremental model without re-scanning the world, the audit is
//! **diff-gated**: a check declares, per exclusion, the inputs that exclusion depends
//! on, and checkleft only re-evaluates an exclusion when one of those inputs is in the
//! changeset.
//!
//! Participation is opt-in and cheap: a [`ConfiguredCheck`](crate::check::ConfiguredCheck)
//! overrides [`declared_exclusions`](crate::check::ConfiguredCheck::declared_exclusions)
//! to list its auditable exclusions and
//! [`evaluate_exclusion`](crate::check::ConfiguredCheck::evaluate_exclusion) to answer
//! "is this still load-bearing?". Both default to a no-op, so checks that do not opt in
//! never emit stale-exclusion findings.

use std::path::PathBuf;

/// An exclusion entry a check honors that is eligible for stale-exclusion auditing.
///
/// A check returns these from
/// [`declared_exclusions`](crate::check::ConfiguredCheck::declared_exclusions). The
/// runner re-evaluates an exclusion only when at least one path in [`depends_on`]
/// appears in the changeset, then asks the check via
/// [`evaluate_exclusion`](crate::check::ConfiguredCheck::evaluate_exclusion) whether it
/// is still needed.
///
/// [`depends_on`]: DeclaredExclusion::depends_on
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredExclusion {
    /// The exclusion entry exactly as written in `CHECKS.toml`. Used both to locate the
    /// offending line in the config file (for the finding location) and verbatim in the
    /// finding message, so the author can find and delete it.
    pub entry: String,
    /// Repo-root-relative files this exclusion depends on. The audit re-evaluates the
    /// exclusion only when at least one of these paths is in the changeset.
    ///
    /// An empty set means the dependency cannot be pinned to concrete files, so the
    /// exclusion is **never** audited. This is the fail-safe contract: a check that
    /// cannot name what an exclusion depends on must not have that exclusion flagged
    /// stale on a guess.
    pub depends_on: Vec<PathBuf>,
}

impl DeclaredExclusion {
    /// Construct a declared exclusion from its raw entry text and the files it depends on.
    pub fn new(entry: impl Into<String>, depends_on: Vec<PathBuf>) -> Self {
        Self {
            entry: entry.into(),
            depends_on,
        }
    }
}

/// The verdict for a single exclusion re-evaluated as if it were not configured.
///
/// Checks must **fail safe**: when staleness cannot be proven, return
/// [`Unknown`](ExclusionStatus::Unknown) rather than guessing
/// [`Stale`](ExclusionStatus::Stale). A false "stale" finding trains authors to ignore
/// the check; a missed one merely leaves a dead entry for next time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusionStatus {
    /// The exclusion is still required: the target still violates the rule without it.
    LoadBearing,
    /// The exclusion is no longer required: with it lifted the rule now passes, or the
    /// thing it referenced is gone. `reason` is a short human explanation woven into the
    /// finding message (e.g. "`Foo` now satisfies the builder rule without the exclusion").
    Stale { reason: String },
    /// Staleness could not be determined. Never reported — the fail-safe default.
    Unknown,
}
