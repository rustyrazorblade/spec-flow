//! `advance`/`approve`/`set_gate` as plain functions (§6) — the
//! underlying logic of the same-named MCP tool contracts, implemented
//! here as pure Rust rather than actual MCP tools, since no MCP server
//! exists yet (§14 step 7 stops short of that; see `crate`'s module
//! doc).
//!
//! All three are pure: [`advance`] takes an already-fetched
//! [`IssueSnapshot`] and the parsed [`WorkflowConfig`] and returns a
//! decision; [`approve`]/[`set_gate`] return the [`LabelOp`]s a future
//! MCP tool would execute via [`crate::vcs::Vcs::set_label`], mirroring
//! how [`crate::claim`]'s functions take [`crate::vcs::Vcs`] only where
//! I/O is actually needed and how [`crate::schedule`] stays pure
//! throughout. Nothing here calls `Vcs`, spawns a phase, or writes a
//! label — that wiring is the still-nonexistent phase engine's job.

use crate::state::{APPROVAL_PREFIX, IssueSnapshot};

use super::gate::gate_clear;
use super::requires::{
    Requirement, RequirementContext, requirement_satisfied,
};
use super::{GateMode, Phase, PhaseAction, WorkflowConfig};

/// The reserved status marking an issue as handed off between
/// `test-plan` and `implement` in the shipped default (§7.2 ph.5's "the
/// server marks the issue `status:ready` for handoff"), without itself
/// being backed by a [`Phase`] entry — no phase declares `status_label:
/// "ready"`, so there is nothing for the normal `status_label` lookup in
/// [`advance`] to find. [`advance`] special-cases this status only when
/// no [`Phase`] literally claims it (see [`advance_from_ready`]): it
/// resolves the alias target positionally, from `workflow.labels.status`
/// (the status immediately after `ready` in that list), and runs it
/// through [`advance_into`] — the same entry check a normal
/// current→next transition runs against its *next* phase, including
/// the merge-gate check, not just `requires`. An earlier revision
/// derived the target from "the first phase with a non-empty `requires`
/// list" instead; that heuristic broke under ordinary config edits (see
/// [`advance_from_ready`]'s doc), which is why the lookup is positional
/// now.
const READY_STATUS: &str = "ready";

/// Whether `phase`'s action is the merge-enqueue action (§7.2 ph.7) —
/// the one action §2.3b singles out as never allowed to run off an
/// implicit `auto`. Derived structurally from the phase's own
/// `action.op`, the same shipped-config data
/// [`crate::workflow::PhaseAction::Server`] already carries, rather than
/// a new schema field: the shipped default's `review` phase is exactly
/// `{type: server, op: merge_queue}`.
///
/// Matches only `PhaseAction::Server` — a hypothetical
/// `ServerThenAgent` phase whose `op` also enqueued a merge would not be
/// caught here. The shipped default never does this (`implement` is the
/// only `ServerThenAgent` phase, and its op is `start_implement`), and
/// this crate has no op-name registry shared between this check and a
/// future op executor to consult instead — noted here rather than
/// guessed at with a broader match that has no real case to justify it
/// yet.
///
/// `pub(super)` today since only this module's own `advance`/
/// `advance_into` call it; the still-nonexistent phase engine that will
/// actually execute `merge_queue` (§14 step 7+) will need the same
/// check too, but widening this function's visibility can wait until
/// that caller exists. When it does, `crate::merge`'s `plan_merge`/
/// `observe_merge` (§14 step 8) are the pure decision functions that
/// pick up once this identifies `review` as the phase to run them
/// against.
pub(super) fn is_merge_gate(phase: &Phase) -> bool {
    matches!(&phase.action, PhaseAction::Server { op } if op == "merge_queue")
}

/// The key an approval check compares against
/// [`IssueSnapshot::approvals`] for `phase` (§4.3).
///
/// Not always `phase.id`: the shipped default `review` phase has `id:
/// review` but `approval_label: "approved:merge"` — the label an
/// `approve` grant actually writes (and [`crate::state::derive_issue_
/// state`] actually reads back into `approvals`) is `"merge"`, not
/// `"review"`. This derives the real key from `phase.approval_label`
/// (stripping the `approved:` prefix) whenever one is configured, so a
/// phase whose approval label deliberately differs from its id — the
/// merge gate names itself after the *action* ("merge"), not the
/// engine's internal phase id ("review") — is still checked correctly.
/// Falls back to `phase.id` for a `human`-gated phase with no
/// `approval_label` configured at all — a misconfiguration the shipped
/// default never produces, but one this function still needs to resolve
/// to *some* string rather than panic.
fn approval_key(phase: &Phase) -> &str {
    match phase.approval_label.as_deref() {
        Some(label) => label.strip_prefix(APPROVAL_PREFIX).unwrap_or(label),
        None => &phase.id,
    }
}

/// [`advance`]'s decision for one issue (§6 `advance`).
///
/// `#[non_exhaustive]` for the same reason [`crate::claim::ClaimResult`]
/// is: this is a decision-outcome type likely to grow a variant later
/// (e.g. a future `AwaitingExternalReview` per §7.4) without that being
/// a breaking change for any downstream matcher.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AdvanceDecision {
    /// The issue carries no `status:<phase>` label yet — there is
    /// nothing to advance from. Stamping an initial status is issue
    /// creation's job (`create_issue`, §6), not this function's.
    NoCurrentPhase,
    /// The issue's current `status:<phase>` label does not match any
    /// phase's [`Phase::status_label`] in `workflow.phases` — drift (an
    /// issue on a status this workflow definition doesn't define), not
    /// a decision this function can make on its own.
    UnknownPhase {
        /// The unrecognized status label.
        status: String,
    },
    /// The current phase is parked on a gate that is not yet clear (§4.3,
    /// §7.1) — see [`super::gate::gate_clear`] for the full evaluation,
    /// including the per-issue `gate:<phase>` override.
    BlockedOnGate {
        /// The current phase's id.
        phase: String,
    },
    /// The current phase's `requires` are not all satisfied yet (§7.1).
    BlockedOnRequires {
        /// The current phase's id.
        phase: String,
        /// Every requirement that is not yet satisfied, in the order
        /// `phase.requires` declares them.
        unmet: Vec<Requirement>,
    },
    /// The current phase (or `ready`, see `READY_STATUS`) is otherwise
    /// clear, but the **next** phase's own `requires` are not all
    /// satisfied yet — distinct from [`Self::BlockedOnRequires`], which
    /// reports the *current* phase's own unmet requirements. This
    /// variant exists because a phase's action can run immediately upon
    /// entry (the shipped `review` phase's action *is* `merge_queue` —
    /// enqueueing the merge — so `review.requires` must be checked
    /// **before** advancing into `review`, not only after, or a red-CI
    /// issue at `implement` could reach `Advance { to: review }` and have
    /// its merge enqueued despite `review.requires` never having been
    /// evaluated).
    NextPhaseNotReady {
        /// The id of the phase the caller is not yet clear to enter.
        phase: String,
        /// Every one of that phase's requirements that is not yet
        /// satisfied, in the order `phase.requires` declares them.
        unmet: Vec<Requirement>,
    },
    /// The **next** phase is a bare [`super::PhaseAction::Server`] phase
    /// (the merge-enqueueing `review` phase, `finalize`, or any future
    /// phase shaped like them) whose own gate is not yet clear — checked
    /// before entering it, not after. Every *agent-producing* phase's
    /// `human` gate (`Agent` and `ServerThenAgent` actions) approves that
    /// phase's **output** once it is already current (e.g.
    /// `architecture`'s gate approves the proposal an agent already
    /// wrote, `implement`'s would approve the code an agent already
    /// wrote); a caller is expected to enter the phase, let its action
    /// run, and only then find itself blocked leaving it via
    /// [`Self::BlockedOnGate`]. A bare `Server` phase has no such
    /// after-the-fact output to approve — its action runs immediately on
    /// entry and *is* the operation a gate on it exists to hold back
    /// (merge-enqueueing per §4.3's "merge is special"; an operator
    /// stamping `gate:finalize` on one issue per §4.3's general per-issue
    /// override is exactly as entitled to stop `finalize`'s cleanup
    /// before it runs as `review`'s is to stop the merge). So entering
    /// any `Server` phase at all must wait for its gate, exactly like
    /// [`Self::NextPhaseNotReady`] does for `requires` and for the same
    /// reason.
    NextPhaseGateBlocked {
        /// The id of the `Server`-action phase the caller is not yet
        /// clear to enter.
        phase: String,
    },
    /// Clear to move to the next phase in `workflow.phases`.
    Advance {
        /// The next phase, cloned out of the workflow definition so the
        /// caller has everything it needs (action, gate, artifact, ...)
        /// to actually run it. Boxed per clippy's `large_enum_variant`:
        /// [`Phase`] is comfortably larger than every other variant's
        /// payload, and this variant is not on a hot path where the
        /// extra indirection would matter.
        to: Box<Phase>,
    },
    /// The current phase is the last entry in `workflow.phases` — there
    /// is nothing further to advance to. (`out_of_band` re-entries are
    /// not part of this linear sequence — see [`WorkflowConfig`]'s doc.)
    Done {
        /// The final phase's id.
        phase: String,
    },
}

/// Compute the next step for `snapshot` against `workflow`'s phase list
/// (§6 `advance`, §7.1).
///
/// Unlike [`crate::schedule::DEFAULT_PHASE_ORDER`] (a fixed list baked
/// in for the scheduler's cross-issue priority ordering only, §12), this
/// walks `workflow.phases` itself — the actual, potentially
/// team-edited phase list `.spec-flow/workflow.yaml` defines — making it
/// the authoritative source for **sequencing** one issue through its
/// pipeline, while `schedule`'s hard-coded order stays exactly what its
/// own module doc says it is: a stand-in for the shipped default only.
///
/// A phase is identified for matching against `snapshot.status` by its
/// [`Phase::status_label`], not [`Phase::id`] — the two differ for the
/// shipped `finalize` phase (`id: finalize`, `status_label: done`), so
/// conflating them would fail to recognize an issue at `status:done` as
/// being on the `finalize` phase at all.
///
/// `dependencies_satisfied` and `roborev_verdict` are the same
/// caller-computed facts [`RequirementContext`] needs — see that type's
/// doc for why they arrive as parameters instead of being derived here.
pub fn advance(
    workflow: &WorkflowConfig,
    snapshot: &IssueSnapshot,
    dependencies_satisfied: bool,
    roborev_verdict: Option<&str>,
) -> AdvanceDecision {
    let Some(status) = snapshot.status.as_deref() else {
        return AdvanceDecision::NoCurrentPhase;
    };

    let ctx = RequirementContext {
        snapshot,
        dependencies_satisfied,
        roborev_verdict,
    };

    // A literal Phase claiming this status always wins over the `ready`
    // alias below — a team that models `ready` as a real phase of its
    // own (`status_label: ready`) must not have it silently shadowed by
    // the interstitial-alias fallback.
    match workflow
        .phases
        .iter()
        .position(|phase| phase.status_label.as_deref() == Some(status))
    {
        Some(index) => advance_from_phase(workflow, index, &ctx),
        None if status == READY_STATUS => advance_from_ready(workflow, &ctx),
        None => AdvanceDecision::UnknownPhase { status: status.to_string() },
    }
}

/// `advance`'s logic once `status` has resolved to a real `Phase` at
/// `workflow.phases[index]`.
fn advance_from_phase(
    workflow: &WorkflowConfig,
    index: usize,
    ctx: &RequirementContext<'_>,
) -> AdvanceDecision {
    let phase = &workflow.phases[index];
    let snapshot = ctx.snapshot;

    let gate_label_present =
        snapshot.gates.iter().any(|gate| gate == &phase.id);
    if !gate_clear(
        phase.gate,
        gate_label_present,
        is_merge_gate(phase),
        approval_key(phase),
        &snapshot.approvals,
    ) {
        return AdvanceDecision::BlockedOnGate { phase: phase.id.clone() };
    }

    let unmet: Vec<Requirement> = phase
        .requires
        .iter()
        .filter(|requirement| !requirement_satisfied(requirement, ctx))
        .cloned()
        .collect();
    if !unmet.is_empty() {
        return AdvanceDecision::BlockedOnRequires {
            phase: phase.id.clone(),
            unmet,
        };
    }

    match workflow.phases.get(index + 1) {
        Some(next) => advance_into(next, ctx),
        None => AdvanceDecision::Done { phase: phase.id.clone() },
    }
}

/// Resolve the `ready` alias (see [`READY_STATUS`]'s doc) positionally,
/// from `workflow.labels.status` — the team's own declared phase
/// ordering — rather than by guessing from `requires` shapes.
///
/// The earlier heuristic ("the first phase with a non-empty `requires`
/// list") broke under exactly the kind of edit §7.0's shipped comments
/// invite: adding a `requires` entry to an earlier design phase (e.g.
/// `architecture`) would make `ready` alias to that phase instead of
/// `implement`, sending an already-past-that-phase issue backwards;
/// removing `implement`'s `requires` entirely (a fully-automated config)
/// would walk past it to whichever later phase has `requires` next
/// (`review`), permanently stranding every `ready` issue on an
/// unsatisfiable CI/roborev/merge check it was never meant to face.
/// Reading `ready`'s position directly out of the same ordered
/// `labels.status` list `advance`'s caller already trusts as the
/// team's canonical phase sequence sidesteps both failure modes: the
/// alias target is always "whatever status comes right after `ready`,"
/// regardless of what any phase's `requires` happens to contain.
fn advance_from_ready(
    workflow: &WorkflowConfig,
    ctx: &RequirementContext<'_>,
) -> AdvanceDecision {
    let unknown =
        || AdvanceDecision::UnknownPhase { status: READY_STATUS.to_string() };
    let Some(ready_index) =
        workflow.labels.status.iter().position(|s| s == READY_STATUS)
    else {
        return unknown();
    };
    let Some(next_status) = workflow.labels.status.get(ready_index + 1) else {
        return unknown();
    };
    match workflow
        .phases
        .iter()
        .find(|phase| phase.status_label.as_deref() == Some(next_status))
    {
        Some(target) => advance_into(target, ctx),
        None => unknown(),
    }
}

/// Check whether `next` is clear to enter — its `requires` always, and
/// (for a bare [`PhaseAction::Server`] phase) its `gate` too — before
/// committing to [`AdvanceDecision::Advance`]. See
/// [`AdvanceDecision::NextPhaseNotReady`] and
/// [`AdvanceDecision::NextPhaseGateBlocked`]'s docs for why both checks
/// must happen here rather than waiting until `next` is itself the
/// current phase on a later `advance` call, and for why the gate check
/// is scoped to `Server` phases specifically.
fn advance_into(
    next: &Phase,
    ctx: &RequirementContext<'_>,
) -> AdvanceDecision {
    if matches!(next.action, PhaseAction::Server { .. }) {
        let snapshot = ctx.snapshot;
        let gate_label_present =
            snapshot.gates.iter().any(|gate| gate == &next.id);
        if !gate_clear(
            next.gate,
            gate_label_present,
            is_merge_gate(next),
            approval_key(next),
            &snapshot.approvals,
        ) {
            return AdvanceDecision::NextPhaseGateBlocked {
                phase: next.id.clone(),
            };
        }
    }

    let unmet: Vec<Requirement> = next
        .requires
        .iter()
        .filter(|requirement| !requirement_satisfied(requirement, ctx))
        .cloned()
        .collect();
    if !unmet.is_empty() {
        return AdvanceDecision::NextPhaseNotReady {
            phase: next.id.clone(),
            unmet,
        };
    }
    AdvanceDecision::Advance { to: Box::new(next.clone()) }
}

/// An `approve` decision (§6 `approve`): grant or deny a pending
/// `human` gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApproveDecision {
    /// Grant the gate: write the phase's approval label.
    Grant,
    /// Deny the gate.
    Deny,
}

/// A single GitHub label mutation [`approve`]/[`set_gate`] compute but
/// do not perform — a future MCP tool executes it via
/// [`crate::vcs::Vcs::set_label`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelOp {
    /// The label to add or remove.
    pub label: String,
    /// `true` to add the label, `false` to remove it — the same
    /// `present: bool` shape [`crate::vcs::Vcs::set_label`] takes.
    pub present: bool,
}

/// Compute the label operation(s) an `approve` call needs (§6
/// `approve`).
///
/// **Grant** writes `phase.approval_label` (`present: true`) when the
/// phase has one configured; a phase with no `approval_label` at all
/// (e.g. `conflict-check`, `finalize` in the shipped default) has
/// nothing to grant, so this returns an empty list rather than
/// fabricating a label name.
///
/// **Deny is only partially modeled.** §6 describes it as recording the
/// state and note, then routing the phase back to its defined re-entry
/// (product-spec→re-groom, architecture→re-propose, test-plan→re-plan,
/// merge→address). This function returns an empty `Vec` for `Deny`: it
/// correctly computes "no approval label is written," but does **not**
/// compute the re-entry, and — per §15's "every state-changing call
/// writes through to GitHub" — writes no GitHub-visible trace of the
/// denial at all (no comment, no label another instance could observe).
/// Two different reasons this isn't done here rather than one:
///
/// - For `merge→address`, the re-entry target IS already derivable
///   today, from [`super::OutOfBandPhase::reenters`] on the `address`
///   out-of-band entry — this function's earlier revision claimed
///   otherwise, which was wrong.
/// - For `product-spec`/`architecture`/`test-plan`, denying just means
///   "re-run this same phase" (§6's "re-groom"/"re-propose"/"re-plan" are
///   all self-re-entries), which needs no routing table at all — only a
///   decision about *what* to write (a comment with the note? clearing
///   the status label back to the same phase, which is already a no-op
///   for status but not for any note/history?).
///
/// What actually blocks implementing this is that `approve` takes only a
/// `&Phase`, not the `WorkflowConfig` `out_of_band` lives on, and no
/// caller exists yet (§14 step 7 stops short of the MCP tool wiring) to
/// pin down what "recording the note" should look like in this crate's
/// terms. Recorded here rather than guessed at.
pub fn approve(phase: &Phase, decision: ApproveDecision) -> Vec<LabelOp> {
    match decision {
        ApproveDecision::Grant => match &phase.approval_label {
            Some(label) => {
                vec![LabelOp { label: label.clone(), present: true }]
            }
            None => Vec::new(),
        },
        ApproveDecision::Deny => Vec::new(),
    }
}

/// Compute the label operation `set_gate` needs to set `phase`'s gate to
/// `mode` for one issue (§6 `set_gate`, §4.3).
///
/// The per-issue label scheme is a single binary marker
/// (`gate:<phase>` present or absent, §4.3) — it cannot itself encode
/// three distinct per-issue states. Requesting [`GateMode::Human`]
/// therefore adds the label (`present: true`); requesting either
/// [`GateMode::None`] or [`GateMode::Auto`] removes it (`present:
/// false`), producing the **same** operation for both. This is not a
/// loss of information at the point that matters for most phases:
/// [`super::gate::effective_gate_mode`] resolves an absent label back to
/// `None`/`Auto` by consulting the *workflow config's own default* for
/// this phase (`Some(GateMode::Human)` → `Auto`; anything else → passed
/// through unchanged) — except `is_merge_gate` phases, which
/// `effective_gate_mode` never resolves to `Auto` off an absent label at
/// all (§4.3's "merge is special"), so this collapse cannot silently
/// enable an unreviewed merge.
///
/// One gap this does **not** close: whether `set_gate(Auto)` was ever
/// actually called for a specific issue is not itself recorded anywhere
/// — a label-absent phase looks identical whether a human explicitly
/// chose `auto` for that one issue or nobody has touched it at all. See
/// `super::gate`'s module doc for why closing that fully needs a
/// positive stamped-marker this crate does not yet have.
pub fn set_gate(phase: &Phase, mode: GateMode) -> LabelOp {
    let label = format!("gate:{}", phase.id);
    match mode {
        GateMode::Human => LabelOp { label, present: true },
        GateMode::None | GateMode::Auto => LabelOp { label, present: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{IssueRelationships, IssueState};
    use crate::workflow::parse_workflow;

    fn snapshot(status: Option<&str>) -> IssueSnapshot {
        IssueSnapshot {
            number: 42,
            title: "Widget cache".to_string(),
            state: IssueState::Open,
            priority: None,
            status: status.map(|s| s.to_string()),
            gates: Vec::new(),
            approvals: Vec::new(),
            owner: None,
            relationships: IssueRelationships::default(),
            pull_request: None,
            linked_pull_requests: Vec::new(),
        }
    }

    fn phase(id: &str, status_label: &str) -> Phase {
        Phase {
            id: id.to_string(),
            action: PhaseAction::Server { op: "noop".to_string() },
            artifact: None,
            gate: Some(GateMode::None),
            approval_label: None,
            status_label: Some(status_label.to_string()),
            requires: Vec::new(),
            on_conflict: None,
        }
    }

    fn workflow_with_phases(phases: Vec<Phase>) -> WorkflowConfig {
        let mut workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        workflow.phases = phases;
        workflow
    }

    // -- advance: no current phase / unknown phase --

    #[test]
    fn advance_reports_no_current_phase_without_a_status_label() {
        let workflow =
            workflow_with_phases(vec![phase("a", "a"), phase("b", "b")]);
        let snapshot = snapshot(None);

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(decision, AdvanceDecision::NoCurrentPhase);
    }

    #[test]
    fn advance_reports_unknown_phase_for_an_unrecognized_status() {
        let workflow = workflow_with_phases(vec![phase("a", "a")]);
        let snapshot = snapshot(Some("nonexistent"));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::UnknownPhase {
                status: "nonexistent".to_string()
            }
        );
    }

    // -- advance: gate blocking --

    #[test]
    fn advance_blocks_on_an_unapproved_human_gate() {
        let mut gated = phase("architecture", "architecture");
        gated.gate = Some(GateMode::Human);
        let workflow = workflow_with_phases(vec![gated, phase("b", "b")]);
        // A human-default phase's gate label is stamped at issue
        // creation (§4.3) -- a realistic snapshot for this phase already
        // carries it, since nothing has removed it here.
        let mut snapshot = snapshot(Some("architecture"));
        snapshot.gates = vec!["architecture".to_string()];

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::BlockedOnGate {
                phase: "architecture".to_string()
            }
        );
    }

    #[test]
    fn advance_proceeds_once_the_human_gate_is_approved() {
        let mut gated = phase("architecture", "architecture");
        gated.gate = Some(GateMode::Human);
        let next = phase("test-plan", "test-plan");
        let workflow = workflow_with_phases(vec![gated, next.clone()]);
        let mut snapshot = snapshot(Some("architecture"));
        snapshot.gates = vec!["architecture".to_string()];
        snapshot.approvals = vec!["architecture".to_string()];

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(decision, AdvanceDecision::Advance { to: Box::new(next) });
    }

    #[test]
    fn advance_treats_a_never_stamped_gate_label_as_hands_off() {
        // A known, documented caveat (see effective_gate_mode's doc):
        // this module cannot distinguish "the gate label was stamped
        // then explicitly removed" from "it was never stamped at all"
        // -- both look identical from a single snapshot. Per §4.3's own
        // wording ("Remove gate:<phase> -> runs hands-off"), an absent
        // label on a human-default phase resolves to hands-off either
        // way, which this test pins down explicitly rather than leaving
        // implicit.
        let mut gated = phase("architecture", "architecture");
        gated.gate = Some(GateMode::Human);
        let next = phase("test-plan", "test-plan");
        let workflow = workflow_with_phases(vec![gated, next.clone()]);
        let snapshot = snapshot(Some("architecture"));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(decision, AdvanceDecision::Advance { to: Box::new(next) });
    }

    #[test]
    fn advance_respects_the_per_issue_gate_label_override() {
        // architecture's default is none, but this issue carries an
        // explicit gate:architecture label -- must block despite the
        // permissive default.
        let mut none_default = phase("architecture", "architecture");
        none_default.gate = Some(GateMode::None);
        let workflow =
            workflow_with_phases(vec![none_default, phase("b", "b")]);
        let mut snapshot = snapshot(Some("architecture"));
        snapshot.gates = vec!["architecture".to_string()];

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::BlockedOnGate {
                phase: "architecture".to_string()
            }
        );
    }

    // -- advance: requires blocking --

    #[test]
    fn advance_blocks_on_unmet_requires() {
        let mut implement = phase("implement", "implement");
        implement.requires =
            vec![Requirement::Approved("product-spec".to_string())];
        let workflow = workflow_with_phases(vec![implement, phase("b", "b")]);
        let snapshot = snapshot(Some("implement"));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::BlockedOnRequires {
                phase: "implement".to_string(),
                unmet: vec![Requirement::Approved("product-spec".to_string())],
            }
        );
    }

    #[test]
    fn advance_only_reports_the_requirements_that_are_actually_unmet() {
        let mut implement = phase("implement", "implement");
        implement.requires = vec![
            Requirement::Approved("product-spec".to_string()),
            Requirement::Approved("architecture".to_string()),
        ];
        let next = phase("review", "review");
        let workflow = workflow_with_phases(vec![implement, next.clone()]);
        let mut snapshot = snapshot(Some("implement"));
        snapshot.approvals = vec!["product-spec".to_string()];

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::BlockedOnRequires {
                phase: "implement".to_string(),
                unmet: vec![Requirement::Approved("architecture".to_string())],
            }
        );
    }

    // -- advance: next-phase requires (entry guard, finding #1) --

    #[test]
    fn advance_blocks_entering_a_next_phase_whose_requires_are_unmet() {
        // The exact danger the fix closes: clearing the CURRENT phase
        // must not be enough to advance into a next phase whose own
        // requires (e.g. review's ci/roborev/deps_merged) were never
        // evaluated.
        let current = phase("implement", "implement");
        let mut next = phase("review", "review");
        next.requires = vec![Requirement::Ci("green".to_string())];
        let workflow = workflow_with_phases(vec![current, next]);
        let snapshot = snapshot(Some("implement"));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::NextPhaseNotReady {
                phase: "review".to_string(),
                unmet: vec![Requirement::Ci("green".to_string())],
            }
        );
    }

    #[test]
    fn advance_enters_the_next_phase_once_its_requires_are_met() {
        let current = phase("implement", "implement");
        let mut next = phase("review", "review");
        next.requires = vec![Requirement::Ci("green".to_string())];
        let workflow = workflow_with_phases(vec![current, next.clone()]);
        let mut snapshot = snapshot(Some("implement"));
        snapshot.pull_request = Some(crate::vcs::PullRequestStatus {
            number: 7,
            state: crate::vcs::PullRequestState::Open,
            ci: crate::vcs::CiConclusion::Success,
            in_merge_queue: false,
        });

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(decision, AdvanceDecision::Advance { to: Box::new(next) });
    }

    #[test]
    fn advance_on_the_shipped_default_does_not_enqueue_a_merge_on_red_ci() {
        // Reproduces finding #1 exactly against the real shipped
        // workflow: an issue that just cleared `implement` (all three
        // approvals granted) and already has the merge gate approved
        // must still not receive Advance{to: review} while review's own
        // requires -- ci:green, roborev:clean, deps_merged -- are still
        // unmet. (The merge gate itself is approved here so this test
        // isolates the requires check from the separate gate check --
        // see `advance_on_the_shipped_default_does_not_enter_review_
        // without_merge_approval` for that one.)
        let workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        let mut snapshot = snapshot(Some("implement"));
        snapshot.approvals = vec![
            "product-spec".to_string(),
            "architecture".to_string(),
            "test-plan".to_string(),
            "merge".to_string(),
        ];

        let decision = advance(&workflow, &snapshot, false, None);

        assert_eq!(
            decision,
            AdvanceDecision::NextPhaseNotReady {
                phase: "review".to_string(),
                unmet: vec![
                    Requirement::Ci("green".to_string()),
                    Requirement::Roborev("clean".to_string()),
                    Requirement::DepsMerged,
                ],
            }
        );
    }

    // -- advance: the merge phase never resolves an absent gate to auto (finding #2, round 1 + round 2) --

    #[test]
    fn advance_on_the_shipped_default_blocks_the_merge_gate_with_no_label() {
        // Before the fix, review's absent `gate:review` label + its
        // `human` default would resolve to Auto and clear with zero
        // approvals -- an unreviewed auto-merge. It must instead block.
        // (Status is already `review` here, so this exercises the
        // CURRENT-phase gate check in `advance_from_phase`.)
        let workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        let snapshot = snapshot(Some("review"));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::BlockedOnGate { phase: "review".to_string() }
        );
    }

    #[test]
    fn advance_on_the_shipped_default_does_not_enter_review_without_merge_approval()
     {
        // Round 2's finding: the merge_gate carve-out closed the danger
        // when `review` is already current, but `advance_into` (the
        // current->next transition) did not check review's gate at all
        // -- only its requires. An issue at `implement` with everything
        // else green and the gate:review label already stamped (per
        // §4.3, at issue creation) but with NO approved:merge must not
        // reach `Advance { to: review }`, because review's action *is*
        // the merge-enqueue operation and runs on entry.
        let workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        let mut snapshot = snapshot(Some("implement"));
        snapshot.approvals = vec![
            "product-spec".to_string(),
            "architecture".to_string(),
            "test-plan".to_string(),
        ];
        snapshot.gates = vec!["review".to_string()];
        snapshot.pull_request = Some(crate::vcs::PullRequestStatus {
            number: 7,
            state: crate::vcs::PullRequestState::Open,
            ci: crate::vcs::CiConclusion::Success,
            in_merge_queue: false,
        });

        let decision = advance(&workflow, &snapshot, true, Some("clean"));

        assert_eq!(
            decision,
            AdvanceDecision::NextPhaseGateBlocked {
                phase: "review".to_string()
            }
        );
    }

    #[test]
    fn advance_enters_review_once_merge_is_approved_and_everything_is_green() {
        let workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        let mut snapshot = snapshot(Some("implement"));
        snapshot.approvals = vec![
            "product-spec".to_string(),
            "architecture".to_string(),
            "test-plan".to_string(),
            "merge".to_string(),
        ];
        snapshot.pull_request = Some(crate::vcs::PullRequestStatus {
            number: 7,
            state: crate::vcs::PullRequestState::Open,
            ci: crate::vcs::CiConclusion::Success,
            in_merge_queue: false,
        });

        let decision = advance(&workflow, &snapshot, true, Some("clean"));

        let review =
            workflow.phases.iter().find(|p| p.id == "review").unwrap();
        assert_eq!(
            decision,
            AdvanceDecision::Advance { to: Box::new(review.clone()) }
        );
    }

    // -- advance: the entry-gate check generalizes to any bare Server phase (round 3) --

    #[test]
    fn advance_on_the_shipped_default_blocks_a_per_issue_gate_on_finalize() {
        // Round 3's finding: the entry-gate check in `advance_into` was
        // scoped to `is_merge_gate` only, so an operator's per-issue
        // `gate:finalize` override (§4.3: "Add gate:<phase> -> that
        // phase now stops for a human on this issue") was silently
        // bypassed -- `finalize`'s action is a bare Server op that runs
        // on entry, exactly like `review`'s, so it must get the same
        // entry check even though it isn't the merge phase.
        let workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        let mut snapshot = snapshot(Some("review"));
        snapshot.approvals = vec!["merge".to_string()];
        snapshot.gates = vec!["finalize".to_string()];
        snapshot.pull_request = Some(crate::vcs::PullRequestStatus {
            number: 7,
            state: crate::vcs::PullRequestState::Open,
            ci: crate::vcs::CiConclusion::Success,
            in_merge_queue: false,
        });

        let decision = advance(&workflow, &snapshot, true, Some("clean"));

        assert_eq!(
            decision,
            AdvanceDecision::NextPhaseGateBlocked {
                phase: "finalize".to_string()
            }
        );
    }

    #[test]
    fn advance_on_the_shipped_default_enters_finalize_with_no_gate_override() {
        // The shipped default's own `finalize` phase is `gate: none` --
        // the generalized check must not newly block the ordinary happy
        // path when no per-issue override is present.
        let workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        let mut snapshot = snapshot(Some("review"));
        snapshot.approvals = vec!["merge".to_string()];
        snapshot.pull_request = Some(crate::vcs::PullRequestStatus {
            number: 7,
            state: crate::vcs::PullRequestState::Open,
            ci: crate::vcs::CiConclusion::Success,
            in_merge_queue: false,
        });

        let decision = advance(&workflow, &snapshot, true, Some("clean"));

        let finalize =
            workflow.phases.iter().find(|p| p.id == "finalize").unwrap();
        assert_eq!(
            decision,
            AdvanceDecision::Advance { to: Box::new(finalize.clone()) }
        );
    }

    // -- advance: the ready status (finding #3, round 1 + round 2) --

    #[test]
    fn advance_resolves_ready_to_the_phase_after_ready_in_labels_status() {
        let workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        let mut snapshot = snapshot(Some(READY_STATUS));
        snapshot.approvals = vec![
            "product-spec".to_string(),
            "architecture".to_string(),
            "test-plan".to_string(),
        ];

        let decision = advance(&workflow, &snapshot, true, None);

        let implement =
            workflow.phases.iter().find(|p| p.id == "implement").unwrap();
        assert_eq!(
            decision,
            AdvanceDecision::Advance { to: Box::new(implement.clone()) }
        );
    }

    #[test]
    fn advance_blocks_ready_when_the_entry_phases_requires_are_unmet() {
        let workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        let snapshot = snapshot(Some(READY_STATUS));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::NextPhaseNotReady {
                phase: "implement".to_string(),
                unmet: vec![
                    Requirement::Approved("product-spec".to_string()),
                    Requirement::Approved("architecture".to_string()),
                    Requirement::Approved("test-plan".to_string()),
                ],
            }
        );
    }

    #[test]
    fn advance_reports_ready_as_unknown_when_labels_status_has_no_entry_for_it()
     {
        let mut workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        workflow.labels.status.retain(|s| s != READY_STATUS);
        let snapshot = snapshot(Some(READY_STATUS));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::UnknownPhase { status: READY_STATUS.to_string() }
        );
    }

    #[test]
    fn advance_reports_ready_as_unknown_when_no_phase_claims_the_next_status()
    {
        // `ready` is positioned correctly in `labels.status`, but no
        // Phase in this (deliberately incomplete) custom workflow
        // declares the status_label that comes after it.
        let workflow = workflow_with_phases(vec![phase("a", "a")]);
        let snapshot = snapshot(Some(READY_STATUS));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::UnknownPhase { status: READY_STATUS.to_string() }
        );
    }

    #[test]
    fn advance_ready_is_not_redirected_backwards_by_an_earlier_phases_requires()
     {
        // Round 2's finding: the original heuristic ("the first phase
        // with a non-empty requires list") broke the moment a team added
        // a `requires` entry to an earlier design phase -- `ready` would
        // alias to that earlier phase instead of `implement`, sending an
        // issue that already passed it backwards. Resolving positionally
        // from `labels.status` must not have this failure mode.
        let mut workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        let architecture = workflow
            .phases
            .iter_mut()
            .find(|p| p.id == "architecture")
            .unwrap();
        architecture.requires =
            vec![Requirement::Approved("product-spec".to_string())];
        let mut snapshot = snapshot(Some(READY_STATUS));
        snapshot.approvals = vec![
            "product-spec".to_string(),
            "architecture".to_string(),
            "test-plan".to_string(),
        ];

        let decision = advance(&workflow, &snapshot, true, None);

        let implement =
            workflow.phases.iter().find(|p| p.id == "implement").unwrap();
        assert_eq!(
            decision,
            AdvanceDecision::Advance { to: Box::new(implement.clone()) }
        );
    }

    #[test]
    fn advance_ready_is_not_bricked_by_removing_implements_requires() {
        // The other half of round 2's finding: a fully-automated config
        // that clears `implement.requires` entirely must not walk `ready`
        // past it to whichever later phase has `requires` next (`review`,
        // permanently unsatisfiable with no PR yet) -- `ready` always
        // targets the literal next `labels.status` entry, regardless of
        // what `requires` any phase carries.
        let mut workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
        workflow
            .phases
            .iter_mut()
            .find(|p| p.id == "implement")
            .unwrap()
            .requires = Vec::new();
        let snapshot = snapshot(Some(READY_STATUS));

        let decision = advance(&workflow, &snapshot, true, None);

        let implement =
            workflow.phases.iter().find(|p| p.id == "implement").unwrap();
        assert_eq!(
            decision,
            AdvanceDecision::Advance { to: Box::new(implement.clone()) }
        );
    }

    #[test]
    fn advance_does_not_shadow_a_phase_that_literally_claims_the_ready_status()
    {
        // A team that models `ready` as a real phase of its own must have
        // that phase looked up normally -- the `ready` alias in
        // `advance_from_ready` only ever runs when no Phase claims the
        // status literally.
        let mut ready_phase = phase("intake", READY_STATUS);
        ready_phase.requires =
            vec![Requirement::Approved("product-spec".to_string())];
        let workflow =
            workflow_with_phases(vec![ready_phase, phase("b", "b")]);
        let snapshot = snapshot(Some(READY_STATUS));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::BlockedOnRequires {
                phase: "intake".to_string(),
                unmet: vec![Requirement::Approved("product-spec".to_string())],
            }
        );
    }

    // -- advance: moves to the next phase / done --

    #[test]
    fn advance_moves_to_the_next_phase_once_clear() {
        let current = phase("a", "a");
        let next = phase("b", "b");
        let workflow = workflow_with_phases(vec![current, next.clone()]);
        let snapshot = snapshot(Some("a"));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(decision, AdvanceDecision::Advance { to: Box::new(next) });
    }

    #[test]
    fn advance_reports_done_at_the_last_phase() {
        let workflow = workflow_with_phases(vec![phase("a", "a")]);
        let snapshot = snapshot(Some("a"));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(decision, AdvanceDecision::Done { phase: "a".to_string() });
    }

    #[test]
    fn advance_distinguishes_a_phases_id_from_its_status_label() {
        // Mirrors the shipped finalize phase: id != status_label. An
        // issue at status:done must resolve to the phase whose
        // status_label is "done", not fail to find it because its id is
        // "finalize".
        let finalize = phase("finalize", "done");
        let workflow = workflow_with_phases(vec![finalize]);
        let snapshot = snapshot(Some("done"));

        let decision = advance(&workflow, &snapshot, true, None);

        assert_eq!(
            decision,
            AdvanceDecision::Done { phase: "finalize".to_string() }
        );
    }

    // -- advance: end-to-end against the real shipped workflow --

    #[test]
    fn advance_walks_the_shipped_default_workflow_start_to_finish() {
        let workflow =
            parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();

        // product-spec (human gate, stamped at issue creation per §4.3)
        // is blocked without approval.
        let mut unapproved = snapshot(Some("product-spec"));
        unapproved.gates = vec!["product-spec".to_string()];
        assert_eq!(
            advance(&workflow, &unapproved, true, None),
            AdvanceDecision::BlockedOnGate {
                phase: "product-spec".to_string()
            }
        );

        // Approved, it advances to conflict-check.
        let mut approved = snapshot(Some("product-spec"));
        approved.gates = vec!["product-spec".to_string()];
        approved.approvals = vec!["product-spec".to_string()];
        assert_eq!(
            advance(&workflow, &approved, true, None),
            AdvanceDecision::Advance {
                to: Box::new(workflow.phases[1].clone())
            }
        );

        // review's own gate is human too (approval_label:
        // "approved:merge", deliberately not "approved:review") and its
        // requires (ci: green, roborev: clean, deps_merged) are all
        // missing here, so it's blocked on the GATE first -- requires
        // are only checked once the gate itself is clear.
        let mut review = snapshot(Some("review"));
        review.gates = vec!["review".to_string()];
        assert_eq!(
            advance(&workflow, &review, false, None),
            AdvanceDecision::BlockedOnGate { phase: "review".to_string() }
        );

        // Approve the merge gate (its real label, "merge" -- not
        // "review") and the requires block takes over, reporting every
        // one of the three still-unmet requirements.
        let mut review = snapshot(Some("review"));
        review.gates = vec!["review".to_string()];
        review.approvals = vec!["merge".to_string()];
        assert_eq!(
            advance(&workflow, &review, false, None),
            AdvanceDecision::BlockedOnRequires {
                phase: "review".to_string(),
                unmet: vec![
                    Requirement::Ci("green".to_string()),
                    Requirement::Roborev("clean".to_string()),
                    Requirement::DepsMerged,
                ],
            }
        );
    }

    // -- approve --

    #[test]
    fn approve_grant_writes_the_phases_approval_label() {
        let mut gated = phase("architecture", "architecture");
        gated.approval_label = Some("approved:architecture".to_string());

        let ops = approve(&gated, ApproveDecision::Grant);

        assert_eq!(
            ops,
            vec![LabelOp {
                label: "approved:architecture".to_string(),
                present: true,
            }]
        );
    }

    #[test]
    fn approve_grant_on_a_phase_with_no_approval_label_writes_nothing() {
        let no_label = phase("conflict-check", "conflict-check");

        let ops = approve(&no_label, ApproveDecision::Grant);

        assert!(ops.is_empty());
    }

    #[test]
    fn approve_deny_writes_no_approval_label() {
        let mut gated = phase("architecture", "architecture");
        gated.approval_label = Some("approved:architecture".to_string());

        let ops = approve(&gated, ApproveDecision::Deny);

        assert!(ops.is_empty());
    }

    // -- set_gate --

    #[test]
    fn set_gate_human_adds_the_gate_label() {
        let target = phase("architecture", "architecture");

        let op = set_gate(&target, GateMode::Human);

        assert_eq!(
            op,
            LabelOp { label: "gate:architecture".to_string(), present: true }
        );
    }

    #[test]
    fn set_gate_none_removes_the_gate_label() {
        let target = phase("architecture", "architecture");

        let op = set_gate(&target, GateMode::None);

        assert_eq!(
            op,
            LabelOp { label: "gate:architecture".to_string(), present: false }
        );
    }

    #[test]
    fn set_gate_auto_also_removes_the_gate_label() {
        // The label scheme cannot distinguish "none" from "auto" -- both
        // manifest as the label's absence; see set_gate's doc.
        let target = phase("architecture", "architecture");

        let op = set_gate(&target, GateMode::Auto);

        assert_eq!(
            op,
            LabelOp { label: "gate:architecture".to_string(), present: false }
        );
    }
}
