//! Merge-queue integration (§8.1, §14 step 8): gate → enqueue → observe.
//!
//! The **gate** half already exists: [`crate::workflow::advance`] (§14
//! step 7) returns `AdvanceDecision::Advance { to: review_phase }` only
//! once the shipped `review` phase's own gate and `requires`
//! (`approved:merge`, `ci: green`, `roborev: clean`, `deps_merged`) are
//! all satisfied — by the time a caller reaches this module, "is this
//! issue's PR clear to merge" is already a decided question, not this
//! module's to re-derive. This module picks up from there: given a
//! project's [`MergeMode`] (already detected once by `spec-flow init`'s
//! `detect_merge_mode`, §14 step 1 — see [`crate::init`]), decide what to
//! actually *do* ([`plan_merge`]), and given a PR's current
//! [`PullRequestStatus`], decide what that means for the issue
//! ([`observe_merge`]).
//!
//! # Intended tick order (no caller wires this up yet)
//!
//! Once §12's CI/PR poller exists to drive it: for an issue whose
//! `workflow::advance` decision is `Advance { to: review_phase }` (or
//! already current at `review`), read the linked PR's
//! [`PullRequestStatus`] and call [`observe_merge`] first. Only on
//! [`MergeObservation::NotQueued`] does [`plan_merge`] have anything new
//! to decide — [`MergeObservation::Pending`]/[`MergeObservation::
//! Merged`] mean the previous tick already enqueued it or GitHub already
//! resolved it, and calling [`plan_merge`] again would be redundant with
//! (though not wrong given) what it already reports for those same
//! states. See `crate::workflow::advance`'s private `is_merge_gate`
//! helper for the phase-engine side of this handoff: it identifies
//! `review` as the merge-enqueueing phase but stops short of naming this
//! module, since nothing calls either yet.
//!
//! # What this step does not implement
//!
//! The **serialized fallback** (§8.1: "not every repo has the native
//! queue... the server degrades to serializing merges itself" via a
//! single cross-instance `merge` lease, §8.3) is deliberately not built
//! here. Three real gaps, not an oversight — and, to be precise about
//! which gaps actually apply: §16 item 5's open question about the
//! *lease acquisition model* (pure polling vs. an MCP long-poll/
//! notification) explicitly does **not** cover this case — that item's
//! own text ends "(Merges + claims are not leases.)" A server-internal
//! merge lock that no MCP client ever waits on doesn't need that
//! decision resolved first; an earlier revision of this doc cited that
//! open question anyway, which was wrong. The real blockers:
//!
//! - **The fallback's actual mechanics need `Vcs` capabilities this
//!   crate does not have.** §8.1 describes the fallback loop precisely:
//!   rebase the PR branch onto latest `default_branch`, re-check CI is
//!   green, `gh pr merge` **directly** (not the native queue's `--auto`
//!   enqueue), then release the lock. Neither "update a PR branch
//!   against latest base" nor "merge a PR directly" exist on [`crate::
//!   vcs::Vcs`] yet, and — like every other `Vcs` method this crate has
//!   added — they need their real `gh`/GraphQL shape validated against
//!   actual behavior first (the same care `merge_queue_enabled`'s
//!   GraphQL argument-name fix and `set_label`'s CSV-parsing hazards
//!   took), not guessed at from the spec prose alone.
//! - **The lock's storage representation is a genuine, unpinned design
//!   choice.** §8.3 says "lease state lives in GitHub," but doesn't say
//!   *where* a resource-scoped (not per-issue) lock like this one lives
//!   — a label on a dedicated well-known issue `init` creates for this
//!   purpose is one candidate shape, reusing [`crate::claim`]'s existing
//!   write/settle-read primitive against that issue's number rather than
//!   needing any new repo-wide search capability. **Not yet workable as
//!   described**, though: [`crate::claim::write_claim`] composes its
//!   label with the epoch baked into the label *name*
//!   (`owner:<instance>@<epoch>`), and `crate::vcs::shell::ShellVcs::
//!   set_label`'s doc records a **confirmed** finding (read from `gh`'s
//!   own source, not speculation) that `gh issue edit --add-label`
//!   cannot create a label name that doesn't already exist in the repo
//!   — so every heartbeat's fresh epoch would fail against a real
//!   GitHub repo on every single attempt, the identical conflict
//!   `write_claim` already has with the work-claim protocol it serves
//!   today. Reusing `claim.rs` for this lock inherits that conflict
//!   outright; it does not sidestep it. This is exactly the class of
//!   decision this crate's build has consistently surfaced rather than
//!   picked unilaterally (see `set_label`'s doc for the established
//!   precedent) — the point here is the storage shape's *unresolved
//!   status*, not a working recipe to copy.
//! - **Nothing can drive it yet.** §12's CI/PR poller — the thing that
//!   would actually notice a `Serialized` project has a PR ready to
//!   merge and run the fallback loop — does not exist until this crate
//!   has an async runtime (see `crate::schedule`'s module doc).
//!
//! [`plan_merge`] therefore only ever plans the native path; a
//! [`MergeMode::Serialized`] project reports
//! [`MergeAction::SerializedFallbackNotImplemented`] rather than
//! silently doing nothing or attempting an unconfirmed lease protocol.
//! **This is a real, operator-facing gap, not a cosmetic one**: §8.1
//! requires the fallback to "never hard-fail," and a `Serialized`
//! project that reaches this code path today gets a permanent no-op
//! instead of degraded-but-working merges — worth a direct decision with
//! the operator before `serve` (§14 step 9+) ever reaches a
//! `Serialized` project, not something this doc comment alone resolves.
//!
//! `spec-flow init`'s merge-queue-enabled check (§8.1's other named
//! deliverable for this step) is already implemented — see
//! `crate::init`'s `detect_merge_mode` — and needed no changes here.
//!
//! # A pre-existing gap this step surfaced but does not fix
//!
//! `crate::vcs::shell::ShellVcs::enqueue_merge` (step 1) hard-codes
//! `gh pr merge --auto --squash`. A repo whose branch protection
//! requires the merge queue can reject an explicit strategy flag,
//! depending on how the queue itself is configured — this was validated
//! against a `crate::vcs::shell::runner::FakeRunner` in step 1, not
//! against a real repo with the queue enabled and squash merges
//! disallowed. Not addressed here since it's step 1's code, not this
//! step's; flagged so it isn't silently assumed to already be solid.

use crate::config::MergeMode;
use crate::vcs::{PullRequestState, PullRequestStatus};

/// What to actually do once [`crate::workflow::advance`] has already
/// decided an issue is clear to enter its merge phase (§8.1's "enqueue"
/// step).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MergeAction {
    /// Enqueue this PR to GitHub's native merge queue via
    /// [`crate::vcs::Vcs::enqueue_merge`].
    EnqueueNative {
        /// The PR to enqueue.
        pr_number: u64,
    },
    /// Already enrolled in the native queue — a previous tick already
    /// enqueued it, so there is nothing further to do here until
    /// [`observe_merge`] reports a terminal outcome. Calling
    /// `enqueue_merge` again would be redundant at best; §8.1 gives no
    /// indication it is meant to be idempotent against a `gh` call, so
    /// this is checked here rather than assumed safe to repeat.
    AlreadyQueued {
        /// The already-enqueued PR.
        pr_number: u64,
    },
    /// GitHub already merged this PR — nothing to enqueue. A realistic
    /// case, not a hypothetical: §7.3's restart recovery means a daemon
    /// can come back up after GitHub's queue merged a PR while it was
    /// down, re-derive "issue in `review`, action `merge_queue`," and
    /// call [`plan_merge`] again before anything has advanced the issue
    /// past `review` yet. Calling `enqueue_merge` on an already-merged
    /// PR would be a guaranteed failed `gh` call for no reason —
    /// [`observe_merge`] already reports [`MergeObservation::Merged`]
    /// for the same snapshot; the caller's next step is to act on that,
    /// not to plan an enqueue at all.
    AlreadyMerged {
        /// The already-merged PR.
        pr_number: u64,
    },
    /// The PR was closed without merging — not something to enqueue
    /// under any `merge_mode`; a closed, unmerged PR on an issue whose
    /// phase engine says it's clear to merge is drift (§12) for a caller
    /// to surface, not a call for this function to plan around.
    ClosedWithoutMerge {
        /// The closed PR.
        pr_number: u64,
    },
    /// `merge_mode` is [`MergeMode::Serialized`] — see this module's doc
    /// for why the fallback is not implemented rather than guessed at.
    SerializedFallbackNotImplemented,
}

/// Decide what to do to actually merge `pr` (§8.1's "enqueue" step),
/// given `merge_mode` (from
/// [`crate::config::ProjectConfig::merge_mode`], set once by `spec-flow
/// init`'s `detect_merge_mode`).
///
/// Checks `pr.state` before `merge_mode` — a merged or closed PR is
/// never something to enqueue, regardless of which mode a project runs
/// (see [`MergeAction::AlreadyMerged`]/[`MergeAction::
/// ClosedWithoutMerge`]'s docs for why this isn't a hypothetical).
///
/// Pure: takes an already-fetched [`PullRequestStatus`] and does not
/// call [`crate::vcs::Vcs`] itself — the caller executes whatever
/// [`MergeAction`] this returns, mirroring the "pure decision, `Vcs`-
/// touching orchestration lives in the caller" split
/// [`crate::claim::write_claim`] and [`crate::workflow::advance`] both
/// already use.
pub fn plan_merge(
    merge_mode: MergeMode,
    pr: &PullRequestStatus,
) -> MergeAction {
    match pr.state {
        PullRequestState::Merged => {
            return MergeAction::AlreadyMerged { pr_number: pr.number };
        }
        PullRequestState::Closed => {
            return MergeAction::ClosedWithoutMerge { pr_number: pr.number };
        }
        PullRequestState::Open => {}
    }
    match merge_mode {
        MergeMode::Serialized => MergeAction::SerializedFallbackNotImplemented,
        MergeMode::Native if pr.in_merge_queue => {
            MergeAction::AlreadyQueued { pr_number: pr.number }
        }
        MergeMode::Native => {
            MergeAction::EnqueueNative { pr_number: pr.number }
        }
    }
}

/// What a PR's current [`PullRequestStatus`] means for the issue it
/// closes (§8.1's "observe" step).
///
/// A single-snapshot classification, not a state-transition detector:
/// §12's CI/PR poller (still unbuilt — see `crate::schedule`'s module
/// doc for why its async loop waits on this crate having a runtime) is
/// what would call this repeatedly over time. Distinguishing "never
/// enqueued yet" from "fell out of the queue without merging" (e.g. a
/// failed re-check after a rebase) needs comparing against a *previous*
/// observation, which is exactly the kind of history a poller — not a
/// pure function taking one snapshot — is positioned to keep; both cases
/// collapse to [`MergeObservation::NotQueued`] here rather than guessing
/// which one actually happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MergeObservation {
    /// Enrolled in the merge queue, not yet resolved.
    Pending,
    /// GitHub merged the PR — the issue is done; `finalize` (§7.2 ph.8)
    /// can run.
    Merged,
    /// Not (or no longer) enrolled in the merge queue, and not merged.
    NotQueued,
}

/// Classify `pr` into a [`MergeObservation`] — see that type's doc for
/// what this single-snapshot check can and cannot distinguish.
pub fn observe_merge(pr: &PullRequestStatus) -> MergeObservation {
    if pr.state == PullRequestState::Merged {
        MergeObservation::Merged
    } else if pr.in_merge_queue {
        MergeObservation::Pending
    } else {
        MergeObservation::NotQueued
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::CiConclusion;

    fn pr(state: PullRequestState, in_merge_queue: bool) -> PullRequestStatus {
        PullRequestStatus {
            number: 7,
            state,
            ci: CiConclusion::Success,
            in_merge_queue,
        }
    }

    // -- plan_merge --

    #[test]
    fn plan_merge_enqueues_an_unqueued_pr_under_native_mode() {
        let status = pr(PullRequestState::Open, false);

        let action = plan_merge(MergeMode::Native, &status);

        assert_eq!(action, MergeAction::EnqueueNative { pr_number: 7 });
    }

    #[test]
    fn plan_merge_reports_already_queued_under_native_mode() {
        let status = pr(PullRequestState::Open, true);

        let action = plan_merge(MergeMode::Native, &status);

        assert_eq!(action, MergeAction::AlreadyQueued { pr_number: 7 });
    }

    #[test]
    fn plan_merge_reports_the_serialized_fallback_as_unimplemented() {
        // Whether or not the PR is already enrolled in a (nonexistent)
        // serialized queue is irrelevant here -- Serialized always
        // reports the same thing, since this crate has no fallback
        // mechanism to plan a step of at all yet.
        let status = pr(PullRequestState::Open, false);

        let action = plan_merge(MergeMode::Serialized, &status);

        assert_eq!(action, MergeAction::SerializedFallbackNotImplemented);
    }

    #[test]
    fn plan_merge_ignores_the_native_queue_flag_under_serialized_mode() {
        // Pinned separately from the `false` case above: `in_merge_queue`
        // is a *native*-queue signal. A `Serialized` project reading
        // `true` here (stale data from before a mode switch, say) must
        // still report the same unimplemented fallback, not be misread
        // as "already queued" under a queue this mode doesn't use.
        let status = pr(PullRequestState::Open, true);

        let action = plan_merge(MergeMode::Serialized, &status);

        assert_eq!(action, MergeAction::SerializedFallbackNotImplemented);
    }

    #[test]
    fn plan_merge_does_not_replan_an_already_merged_pr_under_native_mode() {
        // A realistic, not hypothetical, race: §7.3 restart recovery
        // means a daemon can come back up after GitHub's queue already
        // merged a PR while it was down. Re-enqueuing an already-merged
        // PR would be a guaranteed failed `gh pr merge` call.
        let status = pr(PullRequestState::Merged, false);

        let action = plan_merge(MergeMode::Native, &status);

        assert_eq!(action, MergeAction::AlreadyMerged { pr_number: 7 });
    }

    #[test]
    fn plan_merge_does_not_replan_an_already_merged_pr_under_serialized_mode()
    {
        // pr.state is checked before merge_mode -- an already-merged PR
        // is never something to plan a fallback step for either.
        let status = pr(PullRequestState::Merged, true);

        let action = plan_merge(MergeMode::Serialized, &status);

        assert_eq!(action, MergeAction::AlreadyMerged { pr_number: 7 });
    }

    #[test]
    fn plan_merge_does_not_enqueue_a_closed_unmerged_pr_under_native_mode() {
        // A closed-without-merging PR on an issue the phase engine says
        // is clear to merge is drift for a caller to surface -- not
        // something to blindly enqueue.
        let status = pr(PullRequestState::Closed, false);

        let action = plan_merge(MergeMode::Native, &status);

        assert_eq!(action, MergeAction::ClosedWithoutMerge { pr_number: 7 });
    }

    #[test]
    fn plan_merge_does_not_enqueue_a_closed_unmerged_pr_under_serialized_mode()
    {
        let status = pr(PullRequestState::Closed, false);

        let action = plan_merge(MergeMode::Serialized, &status);

        assert_eq!(action, MergeAction::ClosedWithoutMerge { pr_number: 7 });
    }

    // -- observe_merge --

    #[test]
    fn observe_merge_reports_pending_while_enrolled_and_unresolved() {
        let status = pr(PullRequestState::Open, true);

        assert_eq!(observe_merge(&status), MergeObservation::Pending);
    }

    #[test]
    fn observe_merge_reports_merged_once_github_merges_it() {
        let status = pr(PullRequestState::Merged, false);

        assert_eq!(observe_merge(&status), MergeObservation::Merged);
    }

    #[test]
    fn observe_merge_reports_merged_even_if_still_marked_enrolled() {
        // A merged PR reporting in_merge_queue: true would be a stale
        // read, not a real state GitHub produces -- Merged still takes
        // priority over the queue flag rather than reporting Pending on
        // a technicality.
        let status = pr(PullRequestState::Merged, true);

        assert_eq!(observe_merge(&status), MergeObservation::Merged);
    }

    #[test]
    fn observe_merge_reports_not_queued_when_neither_enrolled_nor_merged() {
        let status = pr(PullRequestState::Open, false);

        assert_eq!(observe_merge(&status), MergeObservation::NotQueued);
    }

    #[test]
    fn observe_merge_reports_not_queued_for_a_closed_unmerged_pr() {
        let status = pr(PullRequestState::Closed, false);

        assert_eq!(observe_merge(&status), MergeObservation::NotQueued);
    }
}
