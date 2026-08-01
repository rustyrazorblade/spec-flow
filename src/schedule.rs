//! The scheduler's default ordering (§12, §14 step 5): given a batch of
//! issues' scheduling-relevant facts, decide which one to work on next.
//!
//! This module is a **pure function over already-computed facts** — no
//! [`crate::vcs::Vcs`], no I/O, no clock. §12's ordering needs three
//! kinds of input this crate can already produce today plus two it
//! cannot yet, and rather than reach past that line, this module takes
//! the two missing facts as plain booleans on [`Candidate`] and leaves
//! computing them to the caller:
//!
//! - Whether an issue's `blockedBy` dependencies are all merged/closed
//!   ([`Candidate::dependencies_satisfied`]) — the caller derives this
//!   from `IssueRelationships::blocked_by` plus each dependency's
//!   `IssueState`, the same shape
//!   [`crate::state::drift::find_closed_or_missing_dependencies`]
//!   already consumes.
//! - Whether an issue's current claim belongs to a still-**live**
//!   instance ([`Candidate::claimed_live`]) — the caller derives this
//!   from [`crate::state::drift::find_stale_claims`] against the
//!   issue's parsed [`crate::state::Claim`].
//!
//! Keeping this module free of `Vcs`/clock parameters, rather than
//! accepting a `now`/`ttl` pair and re-deriving liveness itself, avoids
//! two copies of the same staleness rule drifting apart; the tradeoff is
//! that a caller must do that derivation before calling in, which the
//! phase engine (§14 step 7, whenever it lands) is the natural place for
//! since it already walks every issue's state for its own reasons.
//!
//! # The furthest-along phase order
//!
//! §12 states the ordering itself — "deepest phase first (`finalize >
//! review > implement > test-plan > architecture > conflict-check >
//! product-spec`)" — but not where the design-approval seam
//! (`status:ready`, §7.2 ph.5) sits in it. [`DEFAULT_PHASE_ORDER`]
//! inserts it between `test-plan` and `implement`, matching its position
//! in §7.2's numbered phase list. Like
//! [`crate::state`]'s hard-coded label prefixes, this is the **shipped
//! default only**: a project's own `.fleet/workflow.yaml` can define a
//! different phase list entirely, and honoring that is the committed
//! workflow-config parser's job (§14 step 7), which does not exist yet.
//! Until it lands, every project is assumed to run the shipped default —
//! the same scoping [`crate::state`]'s own module doc applies to label
//! conventions.
//!
//! # What "dependency" means in tier 4 is not implemented
//!
//! §12's tier 4 is "**dependency then age**." Age is implemented here as
//! ascending issue number — GitHub issue numbers are monotonically
//! assigned per repo, so a lower number is always older, and
//! [`crate::vcs::IssueRef`] carries no `created_at` timestamp to prefer
//! over that proxy even if one were wanted. "Dependency" as a tie-break
//! *ahead of* age is not implemented: the spec does not say what it
//! means (a plausible reading — prefer the issue that unblocks the most
//! other work — would need each candidate's `blocking` fan-out as
//! further input this module doesn't currently accept), and guessing
//! wrong would be worse than leaving the gap explicit. No caller needs
//! it yet; a dependency-fan-out tie-break, if wanted, slots in between
//! priority and age in [`compare_candidates`].
//!
//! # The CI/PR poller (§12) is out of scope here
//!
//! §12 also calls for the server to "poll CI/PR/merge-queue state via
//! `gh` on a backoff." The one-shot read such a poller would call each
//! tick already exists —
//! [`crate::vcs::Vcs::read_pull_request_status`], shipped in step 4 —
//! but the backoff-timing loop itself (tight while a run is active, slow
//! when idle) needs a long-running async runtime this crate deliberately
//! does not have yet (no `tokio`, no `serve` implementation; see
//! `crate`'s module doc, "What is deliberately not here yet"). That loop
//! belongs to the `serve` daemon once the MCP server/async runtime lands
//! (§14 step 6+), not to this module. Nothing here implements or
//! simulates a sleep/backoff loop.

use crate::state::Priority;

/// The shipped default phase order (§7.2), shallowest (least advanced)
/// first. See the module doc for why `"ready"` sits where it does and
/// why this is only the *default* ordering.
///
/// The last entry is `"done"`, not `"finalize"` — §12's own prose names
/// the ordering after the phase's *name* ("`finalize > review > ...`"),
/// but [`Candidate::status`] is populated straight from an issue's
/// literal `status:<phase>` label, and §11.3's shipped
/// `.spec-flow/workflow.yaml` stamps the finalize phase with
/// `status_label: done` (see `crate::scaffold::DEFAULT_WORKFLOW_YAML`),
/// not `status_label: finalize`. This list must track the label a real
/// issue would actually carry, not the phase's display name — the
/// `default_phase_order_matches_the_scaffolded_status_vocabulary` test
/// below pins the two together so they cannot drift apart silently
/// again.
pub const DEFAULT_PHASE_ORDER: &[&str] = &[
    "product-spec",
    "conflict-check",
    "architecture",
    "test-plan",
    "ready",
    "implement",
    "review",
    "done",
];

/// One issue's scheduling-relevant facts (§12).
///
/// Deliberately not [`crate::state::IssueSnapshot`] itself: the
/// scheduler needs two facts ([`Self::dependencies_satisfied`],
/// [`Self::claimed_live`]) that are not pure GitHub-state derivation —
/// see the module doc for why they arrive here as plain booleans instead
/// of this module computing them.
///
/// **The caller must exclude closed issues before building this list.**
/// Nothing here carries [`crate::vcs::IssueState`]: a closed issue
/// (e.g. `status:done`, the deepest phase) would otherwise rank as the
/// *most* actionable candidate of all, which is never what a caller
/// wants — a scheduler has no business proposing to work an issue that
/// is already finished.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    /// The issue number.
    pub issue: u64,
    /// The issue's current phase, from its `status:<phase>` label.
    /// `None` when it has no status label yet (still being groomed,
    /// §7.2 ph.1) — ranked as *less* advanced than every named phase in
    /// [`DEFAULT_PHASE_ORDER`], never treated as an error.
    pub status: Option<String>,
    /// Priority, from a `P0`–`P3` label. `None` when unset — ranked
    /// below every named priority (worst-priority, not best).
    pub priority: Option<Priority>,
    /// Phases that need a human gate on this issue, from its
    /// `gate:<phase>` labels (§4.3).
    pub gates: Vec<String>,
    /// Phases already approved on this issue, from its
    /// `approved:<phase>` labels (§4.3).
    pub approvals: Vec<String>,
    /// Whether every issue this one's `blockedBy` points at has merged
    /// or closed — i.e. there is no unmerged dependency blocking it
    /// (§8.1, §8.4). The caller computes this; see the module doc.
    pub dependencies_satisfied: bool,
    /// Whether this issue is currently claimed by an instance whose
    /// heartbeat has **not** gone stale. `false` covers both "no claim
    /// at all" and "a claim, but it's stale" — either way the issue is
    /// free to (re)claim. The caller computes this; see the module doc.
    pub claimed_live: bool,
}

/// This issue's rank in [`DEFAULT_PHASE_ORDER`] — higher is further
/// along. An absent or unrecognized status (a custom workflow's phase
/// name this module doesn't know, or simply no label yet) ranks below
/// every named phase rather than erroring: this module only knows the
/// shipped default order, and treating the unknown as "hasn't started"
/// is the safe default, not a guess at where it really belongs.
fn phase_rank(status: Option<&str>) -> i32 {
    match status.and_then(|s| DEFAULT_PHASE_ORDER.iter().position(|p| *p == s))
    {
        Some(index) => index as i32,
        None => -1,
    }
}

/// This priority's sort rank — lower sorts first (P0 highest). `None`
/// (no priority label) ranks below every named priority, i.e. worst.
fn priority_rank(priority: Option<Priority>) -> u8 {
    priority.map_or(4, |p| p as u8)
}

/// Whether `candidate`'s *current* phase is parked on an unapproved
/// human gate.
///
/// Checked against the current phase only, not every `gate:*` label the
/// issue carries: gates for phases the issue hasn't reached yet are
/// stamped up front at issue creation for every phase the config marks
/// `human` (§4.3), so an issue at `status:architecture` routinely also
/// carries `gate:review` — that gate has no bearing on whether
/// architecture work can proceed right now. Only a gate matching the
/// *current* status can be blocking progress at this moment (§7.1's
/// `requires` semantics for advancing past a phase).
fn is_gated_on_human(candidate: &Candidate) -> bool {
    let Some(status) = candidate.status.as_deref() else { return false };
    let gated = candidate.gates.iter().any(|g| g == status);
    let approved = candidate.approvals.iter().any(|a| a == status);
    gated && !approved
}

/// Whether `candidate` is actionable right now (§12 tier 1): its current
/// phase isn't parked on an unapproved human gate, every dependency it's
/// blocked on is merged/closed, and it isn't already claimed by a live
/// instance.
///
/// This is the derivable-today slice of §12's fuller "gate not parked on
/// a human, `requires` satisfied, no unmerged `blockedBy`, not already
/// claimed/running" — the rest of `requires` (e.g. `ci: green`) is a
/// phase-engine `requires` evaluator's job (§14 step 7), which does not
/// exist yet.
fn is_actionable(candidate: &Candidate) -> bool {
    !is_gated_on_human(candidate)
        && candidate.dependencies_satisfied
        && !candidate.claimed_live
}

/// The sort key for ordering actionable candidates (§12 tiers 2–4):
/// furthest-along first, then priority (`P0` first), then ascending
/// issue number as an age proxy — see the module doc for why a
/// dependency-based tie-break ahead of age is not implemented.
fn scheduling_key(candidate: &Candidate) -> (std::cmp::Reverse<i32>, u8, u64) {
    (
        std::cmp::Reverse(phase_rank(candidate.status.as_deref())),
        priority_rank(candidate.priority),
        candidate.issue,
    )
}

/// The full §12 scheduling order over `candidates`: every actionable
/// issue's number, furthest-along/priority/age-sorted, most-recommended
/// first. Non-actionable candidates are dropped, not merely sorted last
/// — an issue parked on a human gate, blocked on an unmerged dependency,
/// or already claimed by a live instance is not "low priority," it is
/// not schedulable at all right now.
///
/// Exposed alongside [`next_action`] (rather than kept as a private
/// implementation detail) because it's cheap to compute and directly
/// useful: the `board` MCP tool (§6) wants both the single recommended
/// `next_action` and a rendered queue behind it.
pub fn schedule_order(candidates: &[Candidate]) -> Vec<u64> {
    let mut actionable: Vec<&Candidate> =
        candidates.iter().filter(|c| is_actionable(c)).collect();
    actionable.sort_by_key(|c| scheduling_key(c));
    actionable.into_iter().map(|c| c.issue).collect()
}

/// The single next issue to schedule (§12, §6 `board`'s `next_action`):
/// the highest-ranked entry of [`schedule_order`], or `None` when
/// nothing in `candidates` is actionable right now.
pub fn next_action(candidates: &[Candidate]) -> Option<u64> {
    schedule_order(candidates).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully actionable, unclaimed, unblocked, ungated candidate at
    /// `status`, with no priority set — the neutral starting point each
    /// test tweaks.
    fn candidate(issue: u64, status: &str) -> Candidate {
        Candidate {
            issue,
            status: Some(status.to_string()),
            priority: None,
            gates: Vec::new(),
            approvals: Vec::new(),
            dependencies_satisfied: true,
            claimed_live: false,
        }
    }

    // -- actionable-now --

    #[test]
    fn next_action_is_none_for_an_empty_candidate_list() {
        assert_eq!(next_action(&[]), None);
    }

    #[test]
    fn next_action_skips_an_issue_gated_on_its_current_unapproved_phase() {
        let mut gated = candidate(1, "architecture");
        gated.gates = vec!["architecture".to_string()];
        let less_advanced_but_free = candidate(2, "conflict-check");

        let picked = next_action(&[gated, less_advanced_but_free]);

        assert_eq!(picked, Some(2));
    }

    #[test]
    fn next_action_does_not_skip_an_issue_whose_current_gate_is_approved() {
        let mut gated_and_approved = candidate(1, "architecture");
        gated_and_approved.gates = vec!["architecture".to_string()];
        gated_and_approved.approvals = vec!["architecture".to_string()];

        let picked = next_action(&[gated_and_approved]);

        assert_eq!(picked, Some(1));
    }

    #[test]
    fn next_action_ignores_a_gate_on_a_phase_the_issue_has_not_reached_yet() {
        // Gates for later phases are stamped up front at issue creation
        // (§4.3) — a `gate:review` on an issue still at `architecture`
        // must not block it from being scheduled now.
        let mut issue = candidate(1, "architecture");
        issue.gates = vec!["review".to_string()];

        let picked = next_action(&[issue]);

        assert_eq!(picked, Some(1));
    }

    #[test]
    fn next_action_skips_an_issue_blocked_on_an_unmerged_dependency() {
        let mut blocked = candidate(1, "implement");
        blocked.dependencies_satisfied = false;
        let unblocked = candidate(2, "conflict-check");

        let picked = next_action(&[blocked, unblocked]);

        assert_eq!(picked, Some(2));
    }

    #[test]
    fn next_action_skips_an_issue_claimed_by_a_live_instance() {
        let mut claimed = candidate(1, "implement");
        claimed.claimed_live = true;
        let free = candidate(2, "conflict-check");

        let picked = next_action(&[claimed, free]);

        assert_eq!(picked, Some(2));
    }

    #[test]
    fn next_action_treats_a_stale_claim_as_actionable() {
        // `claimed_live: false` is exactly what the caller reports for a
        // claim whose heartbeat has gone stale (§8.2) — this module
        // doesn't distinguish "never claimed" from "claim expired," and
        // shouldn't need to.
        let stale_claim = candidate(1, "implement");

        let picked = next_action(&[stale_claim]);

        assert_eq!(picked, Some(1));
    }

    // -- furthest-along --

    #[test]
    fn next_action_prefers_the_deepest_phase() {
        let review = candidate(1, "review");
        let product_spec = candidate(2, "product-spec");

        let picked = next_action(&[product_spec, review]);

        assert_eq!(picked, Some(1));
    }

    #[test]
    fn next_action_orders_every_named_phase_from_deepest_to_shallowest() {
        let issues: Vec<Candidate> = DEFAULT_PHASE_ORDER
            .iter()
            .enumerate()
            .map(|(i, phase)| candidate(i as u64, phase))
            .collect();

        let order = schedule_order(&issues);

        let expected: Vec<u64> =
            (0..DEFAULT_PHASE_ORDER.len() as u64).rev().collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn next_action_ranks_an_issue_with_no_status_behind_every_named_phase() {
        let mut no_status = candidate(1, "product-spec");
        no_status.status = None;
        let product_spec = candidate(2, "product-spec");

        let picked = next_action(&[no_status, product_spec]);

        assert_eq!(picked, Some(2));
    }

    // -- priority --

    #[test]
    fn next_action_breaks_a_furthest_along_tie_by_priority() {
        let mut p2 = candidate(1, "implement");
        p2.priority = Some(Priority::P2);
        let mut p0 = candidate(2, "implement");
        p0.priority = Some(Priority::P0);

        let picked = next_action(&[p2, p0]);

        assert_eq!(picked, Some(2));
    }

    #[test]
    fn next_action_ranks_no_priority_behind_every_named_priority() {
        let mut no_priority = candidate(1, "implement");
        no_priority.priority = None;
        let mut p3 = candidate(2, "implement");
        p3.priority = Some(Priority::P3);

        let picked = next_action(&[no_priority, p3]);

        assert_eq!(picked, Some(2));
    }

    #[test]
    fn next_action_lets_furthest_along_outrank_priority() {
        // Tier 2 (furthest-along) is checked before tier 3 (priority):
        // a less-advanced P0 must still lose to a more-advanced P3.
        let mut p0_shallow = candidate(1, "product-spec");
        p0_shallow.priority = Some(Priority::P0);
        let mut p3_deep = candidate(2, "review");
        p3_deep.priority = Some(Priority::P3);

        let picked = next_action(&[p0_shallow, p3_deep]);

        assert_eq!(picked, Some(2));
    }

    // -- age (final tiebreak) --

    #[test]
    fn next_action_breaks_a_full_tie_by_ascending_issue_number() {
        let a = candidate(9, "implement");
        let b = candidate(3, "implement");

        let picked = next_action(&[a, b]);

        assert_eq!(picked, Some(3));
    }

    // -- schedule_order --

    #[test]
    fn schedule_order_omits_non_actionable_candidates_entirely() {
        let mut gated = candidate(1, "architecture");
        gated.gates = vec!["architecture".to_string()];
        let free = candidate(2, "product-spec");

        let order = schedule_order(&[gated, free]);

        assert_eq!(order, vec![2]);
    }

    #[test]
    fn schedule_order_is_empty_when_nothing_is_actionable() {
        let mut gated = candidate(1, "architecture");
        gated.gates = vec!["architecture".to_string()];

        assert!(schedule_order(&[gated]).is_empty());
    }

    // -- DEFAULT_PHASE_ORDER vs. the shipped workflow.yaml --

    #[test]
    fn default_phase_order_matches_the_scaffolded_status_vocabulary() {
        // The two "shipped default" lists must describe the same
        // workflow. If a future edit changes one label vocabulary
        // without the other, this catches it immediately rather than
        // letting an issue's real `status:*` label go unrecognized by
        // the scheduler (ranking it as if it were brand new, per
        // `phase_rank`'s doc).
        let workflow: serde_yaml::Value =
            serde_yaml::from_str(crate::scaffold::DEFAULT_WORKFLOW_YAML)
                .unwrap();
        let scaffolded_statuses: Vec<String> = workflow["labels"]["status"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();

        assert_eq!(scaffolded_statuses, DEFAULT_PHASE_ORDER);
    }
}
