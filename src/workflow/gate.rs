//! Gate evaluation (§2.3b, §4.3, §7.1) — pure functions deciding whether
//! a phase is clear to run, given its configured default gate mode and
//! an issue's per-issue gate/approval labels.
//!
//! # The one safety invariant this module protects
//!
//! §2.3b states it plainly: "nothing runs a `human`-designated step...
//! by accident. `auto` is always an explicit, recorded config choice —
//! never an implicit default." Two design choices in this crate follow
//! directly from that:
//!
//! - [`super::Phase::gate`] is `Option<GateMode>`, not `GateMode`, so a
//!   workflow YAML that omits the `gate:` key entirely is distinguishable
//!   from one that sets it to the literal `none`. [`effective_gate_mode`]
//!   resolves a missing value to [`GateMode::Human`] — the *safest*
//!   option — never to [`GateMode::None`] or [`GateMode::Auto`].
//! - A `gate:` value that is present but not exactly `none`/`human`/
//!   `auto` (a typo, wrong case) is **not** this module's problem to
//!   soften: [`GateMode`]'s `Deserialize` impl (derived, case-sensitive,
//!   no `#[serde(other)]` catch-all) simply fails to parse, which fails
//!   [`super::parse_workflow`] outright. A whole-config parse error a
//!   human notices immediately is the safe failure mode here — silently
//!   resolving an unrecognized string to a permissive mode would be
//!   exactly the "by accident" case §2.3b forbids.
//! - [`effective_gate_mode`]'s `merge_gate` parameter exists because
//!   §4.3 singles the merge phase out by name: "merge is special — never
//!   implicit auto." Every *other* `human`-default phase legitimately
//!   goes hands-off the moment its `gate:<phase>` label is missing (an
//!   operator removed it, or it never got a chance to be stamped — see
//!   the caveat below), because §4.3 explicitly documents label-removal
//!   as the hands-off mechanism. The phase that actually enqueues the
//!   merge does not get that same benefit of the doubt: for it, a
//!   missing label resolves to [`GateMode::Human`] (blocked), never
//!   [`GateMode::Auto`], regardless of the configured default.
//!
//! # Known, deliberately unresolved gaps (documented, not silently papered over)
//!
//! Two narrower problems remain even with the merge carve-out above,
//! flagged here rather than guessed at, following the same pattern as
//! `crate::vcs::shell::ShellVcs::set_label`'s documented gh-label
//! conflict:
//!
//! - **Stale stamps under an edited config.** An issue created while a
//!   phase's default was `none`/`auto` never had `gate:<phase>` stamped
//!   on it. If a team later edits `workflow.yaml` to flip that phase to
//!   `human`, every already-open issue still has no label — and
//!   [`effective_gate_mode`] cannot tell that apart from "an operator
//!   explicitly removed a label that was stamped." Closing this fully
//!   needs a positive "gate labels were stamped for this issue" marker
//!   independent of the gate label itself, which this crate does not yet
//!   have.
//! - **`Auto` is not distinguishable from "untouched" on the write
//!   side.** [`super::advance::set_gate`]'s doc explains why
//!   `set_gate(GateMode::Auto)` and `set_gate(GateMode::None)` produce
//!   the identical [`super::advance::LabelOp`] — there is no per-issue
//!   record that a human deliberately chose `auto` for one specific
//!   issue, as opposed to nobody ever having called `set_gate` at all.
//!
//! # Recording concern for the future MCP-tool wiring
//!
//! §15 asks that "every state-changing MCP call writes through to
//! GitHub before returning." Nothing here writes anything — these are
//! boolean-decision functions over already-fetched facts, mirroring
//! [`crate::schedule`]'s "pure function, no `Vcs`" shape. In particular,
//! [`effective_gate_mode`] resolving an absent `gate:<phase>` label
//! against a `human`-default phase to [`GateMode::Auto`] is a *read-time*
//! interpretation, not a write: it does not itself stamp any label
//! recording that an "auto" decision was made. When the real `advance`/
//! `set_gate` MCP tools exist (§14 step 7+), whoever wires this module in
//! should confirm that resolution is still observable after the fact —
//! e.g. by having `set_gate`'s explicit call path (see
//! `super::advance::set_gate`) record intent even though the bare label
//! scheme cannot distinguish an explicit "auto" from a merely-absent
//! gate label (see that function's doc for the same limitation from the
//! write side).

use serde::Deserialize;

/// A phase's gate mode (§2.3b): whether it needs a human's approval
/// before the pipeline advances past it.
///
/// Deliberately a closed, derive-`Deserialize` enum with no
/// `#[serde(other)]` fallback: a YAML value that is not exactly one of
/// these three literal strings fails to parse at all rather than
/// silently resolving to one of them. See this module's doc for why
/// that "fail the whole parse" behavior is the safety-critical choice
/// here, not an oversight.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    /// No approval; the phase advances automatically.
    None,
    /// The server requests approval and waits — via the approval label
    /// or an `approve` MCP call, whichever comes first (§2.3b).
    Human,
    /// The gate is auto-approved: the server treats it as satisfied and
    /// advances without asking. Always an explicit, recorded config
    /// choice (§2.3b) — never implied by an absent or unparseable value.
    Auto,
}

/// Resolve a phase's **effective** gate mode for one issue, combining
/// the workflow config's default with that issue's own `gate:<phase>`
/// label (§4.3's per-issue override).
///
/// - `gate_label_present = true` always resolves to [`GateMode::Human`],
///   regardless of `configured_default` — an operator (or the
///   orchestrator) adding `gate:<phase>` always means "stop for a human
///   on this issue," even on a phase whose config default is `none` or
///   `auto` (§2.3b: "mark this one needing architecture review is just
///   ensuring `gate:architecture` is present"). This also makes the
///   "label present AND config already `human`" case a pure no-op: the
///   label-present branch short-circuits before `configured_default` is
///   even consulted, so there is nothing to double-block or panic on.
/// - `gate_label_present = false` with `configured_default ==
///   Some(GateMode::Human)` resolves to [`GateMode::Auto`] — per §4.3,
///   "Remove `gate:<phase>` → that phase runs hands-off (auto) on this
///   issue," since a `human`-default phase would normally carry a
///   stamped label; its absence here is the operator's explicit removal.
/// - `gate_label_present = false` with any other `configured_default`
///   (including `None`, `Some(GateMode::None)`, `Some(GateMode::Auto)`)
///   passes it through unchanged, since an absent label is simply the
///   expected, unmodified state for a phase whose default was never
///   `human` in the first place — **except** a missing config value
///   (`None`), which resolves to the safest option, [`GateMode::Human`],
///   never to a permissive default (see this module's doc).
/// - `merge_gate = true` overrides the `Some(GateMode::Human)` +
///   label-absent case above: it stays [`GateMode::Human`] instead of
///   falling back to [`GateMode::Auto`], per §4.3's "merge is special —
///   never implicit auto." Callers pass `true` only for the phase whose
///   action actually enqueues the merge (see
///   `super::advance::is_merge_gate`) — every other phase keeps the
///   permissive label-absent-means-hands-off behavior `merge_gate =
///   false` preserves.
pub fn effective_gate_mode(
    configured_default: Option<GateMode>,
    gate_label_present: bool,
    merge_gate: bool,
) -> GateMode {
    if gate_label_present {
        return GateMode::Human;
    }
    match configured_default {
        Some(GateMode::Human) if merge_gate => GateMode::Human,
        Some(GateMode::Human) => GateMode::Auto,
        Some(other) => other,
        None => GateMode::Human,
    }
}

/// Whether a phase in `mode` is clear to run right now, given
/// `approval_key` and the issue's `approvals` (its `approved:<phase>`
/// labels, §4.3).
///
/// `approval_key` is **not** always a phase's `id`: the shipped default
/// `review` phase has `id: review` but `approval_label:
/// "approved:merge"`, so the string an approval check must find in
/// `approvals` is `"merge"`, not `"review"`. Callers derive this key
/// from a phase's own `approval_label` (see
/// `super::advance::approval_key`) rather than assuming it always
/// equals `id`.
///
/// - [`GateMode::None`] is always clear — no approval is asked for.
/// - [`GateMode::Auto`] is always clear — the gate is satisfied without
///   needing the approval label at all (§2.3b: "the server treats it as
///   satisfied and advances without asking").
/// - [`GateMode::Human`] is clear only once `approvals` contains
///   `approval_key`.
pub fn is_phase_clear(
    mode: GateMode,
    approval_key: &str,
    approvals: &[String],
) -> bool {
    match mode {
        GateMode::None | GateMode::Auto => true,
        GateMode::Human => approvals.iter().any(|a| a == approval_key),
    }
}

/// The full gate check (§4.3, §7.1): resolve the effective mode via
/// [`effective_gate_mode`], then check it via [`is_phase_clear`].
///
/// This is the one function [`super::advance::advance`] actually calls —
/// [`effective_gate_mode`] and [`is_phase_clear`] are exposed separately
/// because each is independently the unit under test for one of §2.3b's
/// three modes plus the per-issue override, not because a caller
/// typically needs them apart.
pub fn gate_clear(
    configured_default: Option<GateMode>,
    gate_label_present: bool,
    merge_gate: bool,
    approval_key: &str,
    approvals: &[String],
) -> bool {
    let mode = effective_gate_mode(
        configured_default,
        gate_label_present,
        merge_gate,
    );
    is_phase_clear(mode, approval_key, approvals)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- is_phase_clear: the three modes in isolation --

    #[test]
    fn none_is_always_clear_with_no_approvals() {
        assert!(is_phase_clear(GateMode::None, "architecture", &[]));
    }

    #[test]
    fn none_is_always_clear_even_with_unrelated_approvals() {
        assert!(is_phase_clear(
            GateMode::None,
            "architecture",
            &["test-plan".to_string()]
        ));
    }

    #[test]
    fn human_is_blocked_without_the_matching_approval() {
        assert!(!is_phase_clear(GateMode::Human, "architecture", &[]));
    }

    #[test]
    fn human_is_blocked_by_an_unrelated_approval() {
        assert!(!is_phase_clear(
            GateMode::Human,
            "architecture",
            &["test-plan".to_string()]
        ));
    }

    #[test]
    fn human_is_clear_once_its_own_approval_label_is_present() {
        assert!(is_phase_clear(
            GateMode::Human,
            "architecture",
            &["architecture".to_string()]
        ));
    }

    #[test]
    fn auto_is_clear_without_needing_the_approval_label() {
        assert!(is_phase_clear(GateMode::Auto, "architecture", &[]));
    }

    // -- effective_gate_mode: the per-issue override --

    #[test]
    fn gate_label_present_always_resolves_to_human() {
        assert_eq!(
            effective_gate_mode(Some(GateMode::None), true, false),
            GateMode::Human
        );
        assert_eq!(
            effective_gate_mode(Some(GateMode::Auto), true, false),
            GateMode::Human
        );
    }

    #[test]
    fn gate_label_present_and_config_already_human_is_a_no_op() {
        // The exact landmine: both the config default and the per-issue
        // label say "human" at once. Must resolve to plain Human, not
        // double-block or panic.
        assert_eq!(
            effective_gate_mode(Some(GateMode::Human), true, false),
            GateMode::Human
        );
    }

    #[test]
    fn removing_the_gate_label_on_a_human_default_phase_yields_auto() {
        // §4.3, verbatim: "Remove gate:<phase> -> that phase runs
        // hands-off (auto) on this issue."
        assert_eq!(
            effective_gate_mode(Some(GateMode::Human), false, false),
            GateMode::Auto
        );
    }

    #[test]
    fn an_absent_label_on_a_none_default_phase_passes_through_unchanged() {
        assert_eq!(
            effective_gate_mode(Some(GateMode::None), false, false),
            GateMode::None
        );
    }

    #[test]
    fn an_absent_label_on_an_auto_default_phase_passes_through_unchanged() {
        assert_eq!(
            effective_gate_mode(Some(GateMode::Auto), false, false),
            GateMode::Auto
        );
    }

    #[test]
    fn a_missing_configured_default_resolves_to_the_safest_mode() {
        // No config value at all (the YAML omitted `gate:`) must never
        // default to a permissive mode.
        assert_eq!(effective_gate_mode(None, false, false), GateMode::Human);
    }

    #[test]
    fn a_missing_configured_default_with_a_present_label_is_still_human() {
        assert_eq!(effective_gate_mode(None, true, false), GateMode::Human);
    }

    #[test]
    fn merge_gate_never_resolves_an_absent_label_to_auto() {
        // §4.3: "merge is special -- never implicit auto." Every other
        // human-default phase treats a missing label as an explicit
        // removal (see `removing_the_gate_label_on_a_human_default_
        // phase_yields_auto` above); the merge phase must not, since
        // there is no way to tell "removed" from "never stamped" apart,
        // and guessing wrong here means auto-merging unreviewed code.
        assert_eq!(
            effective_gate_mode(Some(GateMode::Human), false, true),
            GateMode::Human
        );
    }

    #[test]
    fn merge_gate_still_honors_an_explicit_none_or_auto_default() {
        // The carve-out is narrowly scoped to the "human default, label
        // absent" case -- a merge phase a team has explicitly configured
        // as `none`/`auto` is not second-guessed.
        assert_eq!(
            effective_gate_mode(Some(GateMode::None), false, true),
            GateMode::None
        );
        assert_eq!(
            effective_gate_mode(Some(GateMode::Auto), false, true),
            GateMode::Auto
        );
    }

    #[test]
    fn merge_gate_is_a_no_op_once_the_label_is_present() {
        assert_eq!(
            effective_gate_mode(Some(GateMode::Human), true, true),
            GateMode::Human
        );
    }

    // -- gate_clear: the composed check --

    #[test]
    fn gate_clear_is_true_for_a_none_default_with_no_override() {
        assert!(gate_clear(
            Some(GateMode::None),
            false,
            false,
            "conflict-check",
            &[]
        ));
    }

    #[test]
    fn gate_clear_is_false_for_a_human_default_awaiting_approval() {
        assert!(!gate_clear(
            Some(GateMode::Human),
            true,
            false,
            "architecture",
            &[]
        ));
    }

    #[test]
    fn gate_clear_is_true_for_a_human_default_once_approved() {
        assert!(gate_clear(
            Some(GateMode::Human),
            true,
            false,
            "architecture",
            &["architecture".to_string()]
        ));
    }

    #[test]
    fn gate_clear_is_true_once_a_human_defaults_label_is_removed() {
        assert!(gate_clear(
            Some(GateMode::Human),
            false,
            false,
            "architecture",
            &[]
        ));
    }

    #[test]
    fn gate_clear_is_false_when_an_operator_adds_a_gate_to_a_none_default() {
        assert!(!gate_clear(
            Some(GateMode::None),
            true,
            false,
            "conflict-check",
            &[]
        ));
    }

    #[test]
    fn gate_clear_is_false_for_a_merge_gate_with_an_absent_label() {
        // The concrete danger finding #2 named: without the merge_gate
        // carve-out, this would resolve to Auto and clear with zero
        // approvals -- an unreviewed auto-merge.
        assert!(!gate_clear(Some(GateMode::Human), false, true, "merge", &[]));
    }
}
