//! `requires` evaluation (§7.1) — a phase's entry conditions, and a pure
//! function checking one against an issue's already-fetched state.
//!
//! Mirrors [`crate::schedule::Candidate`]'s established pattern: rather
//! than reaching for [`crate::vcs::Vcs`] or a map of every other issue's
//! state, [`requirement_satisfied`] takes a small [`RequirementContext`]
//! of facts the caller has already computed — an
//! [`crate::state::IssueSnapshot`] for approvals/CI, plus the two facts
//! that are not pure GitHub-state derivation
//! ([`RequirementContext::dependencies_satisfied`],
//! [`RequirementContext::roborev_verdict`]) — exactly the same shape
//! [`crate::schedule`]'s module doc explains for
//! `Candidate::dependencies_satisfied`/`claimed_live`.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};

use crate::state::IssueSnapshot;
use crate::vcs::CiConclusion;

use super::Phase;

/// One phase's entry-condition requirement (§7.1 `requires`).
///
/// The real YAML mixes bare strings and single-key maps in the same list
/// (`requires: [{ci: green}, {roborev: clean}, deps_merged]`), so this
/// type's [`Deserialize`] impl is hand-written against an intermediate
/// `RawRequirement` shape rather than derived directly.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Requirement {
    /// `"approved:<phase>"` — that phase's approval label must be
    /// present on the issue (§4.3). Carries the bare phase id (the
    /// `approved:` prefix already stripped).
    Approved(String),
    /// The fixed `"deps_merged"` requirement: every issue this one's
    /// `blockedBy` points at has merged or closed (§8.4).
    DepsMerged,
    /// `{ci: <value>}` — the linked pull request's CI conclusion must
    /// equal `value` (the shipped default only ever asks for `"green"`).
    Ci(String),
    /// `{roborev: <value>}` — the most recently reported roborev verdict
    /// must equal `value` (the shipped default only ever asks for
    /// `"clean"`).
    Roborev(String),
    /// A requirement this crate does not yet interpret — e.g. a future
    /// `status:<phase>` or `lease:<resource>` requirement per §7.1's
    /// prose, which the shipped default never actually uses. Kept
    /// verbatim (the raw label, or a debug rendering of the raw
    /// key/value pair) rather than failing the whole workflow parse
    /// over one requirement kind this step doesn't implement.
    /// [`requirement_satisfied`] always treats this as unsatisfied — see
    /// its doc for why that is the safe default, not a guess.
    Unrecognized(String),
}

/// The raw shape one `requires` list entry takes before
/// [`Requirement`]'s hand-written [`Deserialize`] impl classifies it —
/// either a bare string or a single-key map.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawRequirement {
    Label(String),
    KeyValue(BTreeMap<String, String>),
}

impl<'de> Deserialize<'de> for Requirement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawRequirement::deserialize(deserializer)?;
        Ok(match raw {
            RawRequirement::Label(label) => {
                if label == "deps_merged" {
                    Requirement::DepsMerged
                } else if let Some(phase) = label.strip_prefix("approved:") {
                    Requirement::Approved(phase.to_string())
                } else {
                    Requirement::Unrecognized(label)
                }
            }
            // `map.len() != 1` (a flow-style YAML slip, e.g. `{ci: green,
            // roborev: clean}` meant as two separate list entries)
            // deliberately classifies as Unrecognized rather than reading
            // just `ci` and quietly dropping `roborev` from the phase's
            // requirements with no diagnostic at all -- the same "fail
            // closed, don't silently drop a check" concern
            // `ReviewLensSource`'s hand-written impl protects against for
            // the review-panel config, applied here since this type's
            // `Deserialize` is already hand-written for the label/map
            // split and this is the same kind of key-set ambiguity.
            RawRequirement::KeyValue(map) if map.len() == 1 => {
                if let Some(value) = map.get("ci") {
                    Requirement::Ci(value.clone())
                } else if let Some(value) = map.get("roborev") {
                    Requirement::Roborev(value.clone())
                } else {
                    Requirement::Unrecognized(format!("{map:?}"))
                }
            }
            RawRequirement::KeyValue(map) => {
                Requirement::Unrecognized(format!("{map:?}"))
            }
        })
    }
}

/// Facts a caller has already gathered, needed to evaluate a phase's
/// `requires` list against one issue (§7.1). No I/O and no
/// [`crate::vcs::Vcs`] reference lives here — see this module's doc for
/// why, and [`crate::schedule::Candidate`] for the established pattern
/// this mirrors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequirementContext<'a> {
    /// The issue's derived snapshot (§4.2) — [`Requirement::Approved`]
    /// reads [`IssueSnapshot::approvals`]; [`Requirement::Ci`] reads
    /// [`IssueSnapshot::pull_request`].
    pub snapshot: &'a IssueSnapshot,
    /// Whether every issue this one's `blockedBy` points at has merged
    /// or closed (§8.4) — the caller computes this the same way
    /// [`crate::schedule::Candidate::dependencies_satisfied`] does; this
    /// module does not walk `IssueSnapshot::relationships` itself since
    /// doing so would need every dependency's own state, not just this
    /// issue's.
    pub dependencies_satisfied: bool,
    /// The most recently reported roborev verdict, if any (§4.2:
    /// "roborev/test — agent-reported," not derivable from GitHub issue
    /// state alone) — `None` when no verdict has been reported yet.
    pub roborev_verdict: Option<&'a str>,
}

/// The label [`CiConclusion::Success`] is spelled as in a `{ci: <value>}`
/// requirement (the shipped default's only concrete example, `green`).
fn ci_conclusion_label(ci: CiConclusion) -> &'static str {
    match ci {
        CiConclusion::Success => "green",
        CiConclusion::Failure => "red",
        CiConclusion::Pending => "pending",
    }
}

/// Whether `requirement` is satisfied given `ctx`.
///
/// [`Requirement::Unrecognized`] is always unsatisfied: a requirement
/// this crate does not know how to check must never be silently treated
/// as met (a phase that quietly proceeds past a condition it can't
/// evaluate is the "advance by accident" failure mode §2.3b's gate
/// invariant guards against — the same reasoning extends to `requires`).
pub fn requirement_satisfied(
    requirement: &Requirement,
    ctx: &RequirementContext<'_>,
) -> bool {
    match requirement {
        Requirement::Approved(phase) => {
            ctx.snapshot.approvals.iter().any(|a| a == phase)
        }
        Requirement::DepsMerged => ctx.dependencies_satisfied,
        Requirement::Ci(expected) => ctx
            .snapshot
            .pull_request
            .as_ref()
            .is_some_and(|pr| ci_conclusion_label(pr.ci) == expected),
        Requirement::Roborev(expected) => {
            ctx.roborev_verdict == Some(expected.as_str())
        }
        Requirement::Unrecognized(_) => false,
    }
}

/// Whether every entry in `phase.requires` is satisfied given `ctx`. An
/// empty `requires` list is vacuously satisfied.
pub fn phase_requirements_satisfied(
    phase: &Phase,
    ctx: &RequirementContext<'_>,
) -> bool {
    phase.requires.iter().all(|r| requirement_satisfied(r, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{IssueRelationships, IssueState, PullRequestState};

    fn snapshot() -> IssueSnapshot {
        IssueSnapshot {
            number: 42,
            title: "Widget cache".to_string(),
            state: IssueState::Open,
            priority: None,
            status: Some("review".to_string()),
            gates: Vec::new(),
            approvals: Vec::new(),
            owner: None,
            relationships: IssueRelationships::default(),
            pull_request: None,
            linked_pull_requests: Vec::new(),
        }
    }

    fn ctx(snapshot: &IssueSnapshot) -> RequirementContext<'_> {
        RequirementContext {
            snapshot,
            dependencies_satisfied: false,
            roborev_verdict: None,
        }
    }

    // -- Requirement deserialization --

    #[test]
    fn deserializes_a_bare_approved_label() {
        let requirement: Requirement =
            serde_yaml::from_str("\"approved:architecture\"").unwrap();

        assert_eq!(
            requirement,
            Requirement::Approved("architecture".to_string())
        );
    }

    #[test]
    fn deserializes_the_bare_deps_merged_literal() {
        let requirement: Requirement =
            serde_yaml::from_str("deps_merged").unwrap();

        assert_eq!(requirement, Requirement::DepsMerged);
    }

    #[test]
    fn deserializes_a_ci_key_value_map() {
        let requirement: Requirement =
            serde_yaml::from_str("{ci: green}").unwrap();

        assert_eq!(requirement, Requirement::Ci("green".to_string()));
    }

    #[test]
    fn deserializes_a_roborev_key_value_map() {
        let requirement: Requirement =
            serde_yaml::from_str("{roborev: clean}").unwrap();

        assert_eq!(requirement, Requirement::Roborev("clean".to_string()));
    }

    #[test]
    fn deserializes_a_mixed_requires_list_in_document_order() {
        let requirements: Vec<Requirement> = serde_yaml::from_str(
            "[{ci: green}, {roborev: clean}, deps_merged]",
        )
        .unwrap();

        assert_eq!(
            requirements,
            vec![
                Requirement::Ci("green".to_string()),
                Requirement::Roborev("clean".to_string()),
                Requirement::DepsMerged,
            ]
        );
    }

    #[test]
    fn deserializes_an_unrecognized_bare_label_without_failing() {
        let requirement: Requirement =
            serde_yaml::from_str("\"status:ready\"").unwrap();

        assert_eq!(
            requirement,
            Requirement::Unrecognized("status:ready".to_string())
        );
    }

    #[test]
    fn deserializes_an_unrecognized_key_value_map_without_failing() {
        let requirement: Requirement =
            serde_yaml::from_str("{lease: \"test-stack:default\"}").unwrap();

        assert!(matches!(requirement, Requirement::Unrecognized(_)));
    }

    #[test]
    fn a_multi_key_map_is_unrecognized_rather_than_silently_dropping_a_key() {
        // A flow-style YAML slip for what was meant as two separate list
        // entries (`[{ci: green}, {roborev: clean}]`) must not silently
        // resolve to just Ci and drop the roborev check entirely.
        let requirement: Requirement =
            serde_yaml::from_str("{ci: green, roborev: clean}").unwrap();

        assert!(matches!(requirement, Requirement::Unrecognized(_)));
    }

    #[test]
    fn a_duplicate_key_in_a_flow_map_is_not_caught_by_the_len_check() {
        // Recorded as a known, narrow gap rather than silently left
        // untested: serde_yaml collapses a literal duplicate key into a
        // single map entry (last value wins) before this type's
        // Deserialize impl ever sees it, so `map.len() == 1` cannot tell
        // `{ci: green}` apart from a duplicate-key `{ci: green, ci:
        // red}` -- both parse as a single-key map. This is a much less
        // likely authoring slip than the two-distinct-keys case the
        // sibling test above guards (duplicating the exact same key
        // rather than writing two different ones), so it is left as
        // last-wins rather than also routed to Unrecognized.
        let requirement: Requirement =
            serde_yaml::from_str("{ci: green, ci: red}").unwrap();

        assert_eq!(requirement, Requirement::Ci("red".to_string()));
    }

    // -- requirement_satisfied: Approved --

    #[test]
    fn approved_is_satisfied_when_the_matching_approval_is_present() {
        let mut snapshot = snapshot();
        snapshot.approvals = vec!["architecture".to_string()];
        let requirement = Requirement::Approved("architecture".to_string());

        assert!(requirement_satisfied(&requirement, &ctx(&snapshot)));
    }

    #[test]
    fn approved_is_unsatisfied_without_the_matching_approval() {
        let snapshot = snapshot();
        let requirement = Requirement::Approved("architecture".to_string());

        assert!(!requirement_satisfied(&requirement, &ctx(&snapshot)));
    }

    // -- requirement_satisfied: DepsMerged --

    #[test]
    fn deps_merged_reads_the_context_flag_directly() {
        let snapshot = snapshot();
        let mut context = ctx(&snapshot);
        context.dependencies_satisfied = true;

        assert!(requirement_satisfied(&Requirement::DepsMerged, &context));
    }

    #[test]
    fn deps_merged_is_unsatisfied_when_the_context_flag_is_false() {
        let snapshot = snapshot();

        assert!(!requirement_satisfied(
            &Requirement::DepsMerged,
            &ctx(&snapshot)
        ));
    }

    // -- requirement_satisfied: Ci --

    #[test]
    fn ci_green_is_satisfied_by_a_successful_pull_request() {
        let mut snapshot = snapshot();
        snapshot.pull_request = Some(crate::vcs::PullRequestStatus {
            number: 7,
            state: PullRequestState::Open,
            ci: CiConclusion::Success,
            in_merge_queue: false,
        });
        let requirement = Requirement::Ci("green".to_string());

        assert!(requirement_satisfied(&requirement, &ctx(&snapshot)));
    }

    #[test]
    fn ci_green_is_unsatisfied_by_a_pending_pull_request() {
        let mut snapshot = snapshot();
        snapshot.pull_request = Some(crate::vcs::PullRequestStatus {
            number: 7,
            state: PullRequestState::Open,
            ci: CiConclusion::Pending,
            in_merge_queue: false,
        });
        let requirement = Requirement::Ci("green".to_string());

        assert!(!requirement_satisfied(&requirement, &ctx(&snapshot)));
    }

    #[test]
    fn ci_green_is_unsatisfied_with_no_linked_pull_request_at_all() {
        let snapshot = snapshot();
        let requirement = Requirement::Ci("green".to_string());

        assert!(!requirement_satisfied(&requirement, &ctx(&snapshot)));
    }

    // -- requirement_satisfied: Roborev --

    #[test]
    fn roborev_clean_is_satisfied_by_a_matching_verdict() {
        let snapshot = snapshot();
        let mut context = ctx(&snapshot);
        context.roborev_verdict = Some("clean");
        let requirement = Requirement::Roborev("clean".to_string());

        assert!(requirement_satisfied(&requirement, &context));
    }

    #[test]
    fn roborev_clean_is_unsatisfied_with_no_verdict_reported() {
        let snapshot = snapshot();
        let requirement = Requirement::Roborev("clean".to_string());

        assert!(!requirement_satisfied(&requirement, &ctx(&snapshot)));
    }

    #[test]
    fn roborev_clean_is_unsatisfied_by_a_non_matching_verdict() {
        let snapshot = snapshot();
        let mut context = ctx(&snapshot);
        context.roborev_verdict = Some("dirty");
        let requirement = Requirement::Roborev("clean".to_string());

        assert!(!requirement_satisfied(&requirement, &context));
    }

    // -- requirement_satisfied: Unrecognized --

    #[test]
    fn unrecognized_requirements_are_never_treated_as_satisfied() {
        let snapshot = snapshot();
        let requirement =
            Requirement::Unrecognized("status:ready".to_string());

        assert!(!requirement_satisfied(&requirement, &ctx(&snapshot)));
    }

    // -- phase_requirements_satisfied --

    #[test]
    fn phase_requirements_satisfied_is_true_for_an_empty_requires_list() {
        let snapshot = snapshot();
        let phase = crate::workflow::Phase {
            id: "finalize".to_string(),
            action: crate::workflow::PhaseAction::Server {
                op: "finalize".to_string(),
            },
            artifact: None,
            gate: None,
            approval_label: None,
            status_label: Some("done".to_string()),
            requires: Vec::new(),
            on_conflict: None,
        };

        assert!(phase_requirements_satisfied(&phase, &ctx(&snapshot)));
    }

    #[test]
    fn phase_requirements_satisfied_requires_every_entry_to_pass() {
        let mut snapshot = snapshot();
        snapshot.approvals = vec!["product-spec".to_string()];
        let phase = crate::workflow::Phase {
            id: "implement".to_string(),
            action: crate::workflow::PhaseAction::Server {
                op: "start_implement".to_string(),
            },
            artifact: None,
            gate: None,
            approval_label: None,
            status_label: Some("implement".to_string()),
            requires: vec![
                Requirement::Approved("product-spec".to_string()),
                Requirement::Approved("architecture".to_string()),
            ],
            on_conflict: None,
        };

        assert!(!phase_requirements_satisfied(&phase, &ctx(&snapshot)));
    }
}
