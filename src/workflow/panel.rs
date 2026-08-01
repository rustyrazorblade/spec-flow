//! The parallel-group + bounded-fix-loop primitive (§7.1, §7.2 ph.6, §6
//! `submit_review`) — pure decision logic over already-collected review
//! verdicts, modeled the same way [`crate::schedule`] and
//! [`crate::state::drift`] model decisions: a data shape plus a pure
//! transition function, no spawning, no I/O.
//!
//! # What this module does not do
//!
//! §7.1's invariant — "the parallel sub-phases (the review lenses) are
//! read-only over the worktree and run concurrently; the `fix`
//! sub-phase writes the worktree and runs alone (rounds are strictly
//! sequential — lenses → collect → fix → lenses …)" — describes how an
//! actual phase engine would *run* these sub-phases. There is no phase
//! engine, spawner wiring, or MCP server yet (§14 step 7 stops short of
//! that; see `crate`'s module doc), so nothing here spawns a lens,
//! blocks on concurrency, or enforces mutual exclusion over a worktree —
//! [`evaluate_panel`] only computes what a caller *should* do next once
//! it already has every lens's verdict for the current round in hand.
//! The invariant is documented here as the shape the eventual caller
//! must uphold, not as something this module can enforce on its own.
//!
//! # Round-counting semantics
//!
//! §7.2 ph.6: "a mechanical finding is fixed in-loop; a decision-class
//! finding, **or the loop hitting K un-green**, escalates." §6's
//! `submit_review` doc: "any decision-class finding, or the loop hitting
//! its round cap **still un-green** → escalate." Both phrasings describe
//! escalation happening once `K` fix rounds have already been *spent*
//! and the panel is still not all-`pass` — not once a `(K+1)`-th round
//! would be needed. [`evaluate_panel`] therefore escalates
//! ([`EscalateReason::RoundsExhausted`]) exactly when `rounds_used ==
//! fix_loop.max_rounds`, and still requests one more fix
//! ([`PanelOutcome::RunFix`]) at `rounds_used == max_rounds - 1` (the
//! `K`-th and final permitted fix). See the
//! `evaluate_panel_runs_the_final_permitted_fix_round_at_the_cap_boundary`
//! /
//! `evaluate_panel_escalates_exactly_at_the_round_cap_not_only_after_it`
//! test pair, which pins this boundary on both sides.

use super::{FixLoopConfig, FixLoopPolicy};

/// A single review lens's verdict (§6 `submit_review`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewVerdict {
    /// The lens found nothing to change.
    Pass,
    /// The lens wants changes — see the accompanying findings for
    /// whether they're mechanical or decision-class.
    ChangesRequested,
}

/// A finding's severity (§6, §7.1) — the field the fix loop routes on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FindingSeverity {
    /// Fixable in-loop by the bounded fix sub-phase, no judgment call
    /// needed.
    Mechanical,
    /// Requires a human decision; always escalates, regardless of the
    /// round counter.
    Decision,
}

/// One finding a review lens reported (§6 `submit_review`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFinding {
    /// Whether this finding is fixable in-loop or needs a human call.
    pub severity: FindingSeverity,
    /// A short human-readable description of the finding.
    pub summary: String,
}

/// One lens's full result for the current round (§6 `submit_review`):
/// its verdict plus every finding it reported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LensResult {
    /// The lens's name (matches a [`super::ReviewLensConfig::lens`]).
    pub lens: String,
    /// The lens's verdict.
    pub verdict: ReviewVerdict,
    /// The findings behind that verdict — normally empty when `verdict`
    /// is [`ReviewVerdict::Pass`], though this function does not enforce
    /// that a `Pass` verdict carries no findings; it looks at findings
    /// independently of the verdict field.
    pub findings: Vec<ReviewFinding>,
}

/// Why [`evaluate_panel`] escalated to a `human` gate
/// (`awaiting_decision`, §7.1, §6).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EscalateReason {
    /// At least one lens reported a [`FindingSeverity::Decision`]
    /// finding — escalates immediately, regardless of the round
    /// counter.
    DecisionFinding,
    /// The fix loop's round cap (`fix_loop.max_rounds`) has been spent
    /// and the panel is still not all-`pass`.
    RoundsExhausted,
}

/// What to do next, given one round's collected lens verdicts (§7.1,
/// §7.2 ph.6, §6 `submit_review`'s routing rule).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PanelOutcome {
    /// Every lens passed — the phase advances.
    Advance,
    /// Every finding this round was mechanical and rounds remain: run
    /// the fix sub-phase, then re-run the lenses. `round` is the fix
    /// round about to run (1-indexed, so `round == fix_loop.max_rounds`
    /// on the final permitted attempt).
    RunFix {
        /// The fix round about to run.
        round: u32,
    },
    /// Escalate to a `human` gate (`awaiting_decision`).
    Escalate {
        /// Why this round escalated.
        reason: EscalateReason,
    },
}

/// Whether any lens in `verdicts` reported a decision-class finding.
fn has_decision_finding(verdicts: &[LensResult]) -> bool {
    verdicts.iter().any(|lens| {
        lens.findings.iter().any(|f| f.severity == FindingSeverity::Decision)
    })
}

/// Whether every lens in `verdicts` passed. Vacuously `true` for an
/// empty slice (a panel with no configured lenses trivially advances) —
/// not a case the shipped default's five-lens panel produces, but not
/// this function's place to guess a different answer for it either.
fn all_pass(verdicts: &[LensResult]) -> bool {
    verdicts.iter().all(|lens| lens.verdict == ReviewVerdict::Pass)
}

/// Compute the next action for one review-panel round (§7.1, §6
/// `submit_review`'s routing rule):
///
/// 1. Any [`FindingSeverity::Decision`] finding, anywhere in `verdicts`
///    → [`PanelOutcome::Escalate`] with
///    [`EscalateReason::DecisionFinding`] — checked first, and
///    independent of the round counter, per §6: "any decision-class
///    finding... escalate."
/// 2. Otherwise, every lens passing → [`PanelOutcome::Advance`].
/// 3. Otherwise (at least one `changes_requested`, only mechanical
///    findings) and `rounds_used < fix_loop.max_rounds` →
///    [`PanelOutcome::RunFix`] for `rounds_used + 1`.
/// 4. Otherwise (mechanical-only, but `rounds_used == fix_loop
///    .max_rounds` already) → [`PanelOutcome::Escalate`] with
///    [`EscalateReason::RoundsExhausted`] — see this module's doc for
///    why this fires exactly at the cap, not only after exceeding it.
///
/// `fix_loop.on_decision_finding`/`on_exhausted` are consulted (both
/// [`FixLoopPolicy::Escalate`] today, the only variant that exists) so
/// this function reads its outcome from config rather than assuming it —
/// see [`FixLoopPolicy`]'s doc.
pub fn evaluate_panel(
    verdicts: &[LensResult],
    rounds_used: u32,
    fix_loop: &FixLoopConfig,
) -> PanelOutcome {
    if has_decision_finding(verdicts) {
        let FixLoopPolicy::Escalate = fix_loop.on_decision_finding;
        return PanelOutcome::Escalate {
            reason: EscalateReason::DecisionFinding,
        };
    }
    if all_pass(verdicts) {
        return PanelOutcome::Advance;
    }
    if rounds_used >= fix_loop.max_rounds {
        let FixLoopPolicy::Escalate = fix_loop.on_exhausted;
        return PanelOutcome::Escalate {
            reason: EscalateReason::RoundsExhausted,
        };
    }
    PanelOutcome::RunFix { round: rounds_used + 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix_loop(max_rounds: u32) -> FixLoopConfig {
        FixLoopConfig {
            max_rounds,
            on_exhausted: FixLoopPolicy::Escalate,
            on_decision_finding: FixLoopPolicy::Escalate,
        }
    }

    fn passing(lens: &str) -> LensResult {
        LensResult {
            lens: lens.to_string(),
            verdict: ReviewVerdict::Pass,
            findings: Vec::new(),
        }
    }

    fn mechanical_finding(lens: &str) -> LensResult {
        LensResult {
            lens: lens.to_string(),
            verdict: ReviewVerdict::ChangesRequested,
            findings: vec![ReviewFinding {
                severity: FindingSeverity::Mechanical,
                summary: "missing test coverage".to_string(),
            }],
        }
    }

    fn decision_finding(lens: &str) -> LensResult {
        LensResult {
            lens: lens.to_string(),
            verdict: ReviewVerdict::ChangesRequested,
            findings: vec![ReviewFinding {
                severity: FindingSeverity::Decision,
                summary: "this needs a different data model".to_string(),
            }],
        }
    }

    // -- all pass --

    #[test]
    fn evaluate_panel_advances_when_every_lens_passes() {
        let verdicts = vec![passing("spec"), passing("code-review")];

        let outcome = evaluate_panel(&verdicts, 0, &fix_loop(3));

        assert_eq!(outcome, PanelOutcome::Advance);
    }

    #[test]
    fn evaluate_panel_advances_for_an_empty_panel() {
        let outcome = evaluate_panel(&[], 0, &fix_loop(3));

        assert_eq!(outcome, PanelOutcome::Advance);
    }

    // -- mechanical-only: fix loop --

    #[test]
    fn evaluate_panel_runs_the_fix_sub_phase_for_mechanical_findings() {
        let verdicts =
            vec![passing("spec"), mechanical_finding("code-review")];

        let outcome = evaluate_panel(&verdicts, 0, &fix_loop(3));

        assert_eq!(outcome, PanelOutcome::RunFix { round: 1 });
    }

    #[test]
    fn evaluate_panel_increments_the_round_counter_on_each_fix() {
        let verdicts = vec![mechanical_finding("code-review")];

        let outcome = evaluate_panel(&verdicts, 1, &fix_loop(3));

        assert_eq!(outcome, PanelOutcome::RunFix { round: 2 });
    }

    #[test]
    fn evaluate_panel_runs_the_final_permitted_fix_round_at_the_cap_boundary()
    {
        // rounds_used == max_rounds - 1: one fix round still remains.
        let verdicts = vec![mechanical_finding("code-review")];

        let outcome = evaluate_panel(&verdicts, 2, &fix_loop(3));

        assert_eq!(outcome, PanelOutcome::RunFix { round: 3 });
    }

    // -- round cap: escalate exactly at the cap --

    #[test]
    fn evaluate_panel_escalates_exactly_at_the_round_cap_not_only_after_it() {
        // rounds_used == max_rounds: the K-th fix round has already run
        // and the panel is still un-green. Must escalate NOW, not run a
        // (K+1)-th fix.
        let verdicts = vec![mechanical_finding("code-review")];

        let outcome = evaluate_panel(&verdicts, 3, &fix_loop(3));

        assert_eq!(
            outcome,
            PanelOutcome::Escalate { reason: EscalateReason::RoundsExhausted }
        );
    }

    #[test]
    fn evaluate_panel_never_runs_more_than_max_rounds_fixes() {
        // A defensive over-cap case: rounds_used somehow exceeds
        // max_rounds. Must still escalate, never propose another fix.
        let verdicts = vec![mechanical_finding("code-review")];

        let outcome = evaluate_panel(&verdicts, 5, &fix_loop(3));

        assert_eq!(
            outcome,
            PanelOutcome::Escalate { reason: EscalateReason::RoundsExhausted }
        );
    }

    // -- decision-class findings: escalate immediately --

    #[test]
    fn evaluate_panel_escalates_immediately_on_a_decision_finding() {
        let verdicts = vec![passing("spec"), decision_finding("architecture")];

        let outcome = evaluate_panel(&verdicts, 0, &fix_loop(3));

        assert_eq!(
            outcome,
            PanelOutcome::Escalate { reason: EscalateReason::DecisionFinding }
        );
    }

    #[test]
    fn evaluate_panel_escalates_on_a_decision_finding_even_on_round_zero_with_rounds_remaining()
     {
        let verdicts = vec![decision_finding("architecture")];

        let outcome = evaluate_panel(&verdicts, 0, &fix_loop(3));

        assert_eq!(
            outcome,
            PanelOutcome::Escalate { reason: EscalateReason::DecisionFinding }
        );
    }

    #[test]
    fn evaluate_panel_prioritizes_decision_finding_over_rounds_exhausted() {
        // Both conditions are true at once: rounds are exhausted AND a
        // decision finding is present. The reason reported must be
        // DecisionFinding, not RoundsExhausted, per the routing rule's
        // stated check order (§6).
        let verdicts = vec![decision_finding("architecture")];

        let outcome = evaluate_panel(&verdicts, 3, &fix_loop(3));

        assert_eq!(
            outcome,
            PanelOutcome::Escalate { reason: EscalateReason::DecisionFinding }
        );
    }

    #[test]
    fn evaluate_panel_escalates_on_one_decision_finding_even_alongside_mechanical_ones()
     {
        let verdicts =
            vec![mechanical_finding("code-review"), decision_finding("spec")];

        let outcome = evaluate_panel(&verdicts, 0, &fix_loop(3));

        assert_eq!(
            outcome,
            PanelOutcome::Escalate { reason: EscalateReason::DecisionFinding }
        );
    }
}
