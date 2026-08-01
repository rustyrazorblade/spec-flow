//! Work-claiming (§8.2, §14 step 5): the optimistic, single-slot
//! `owner:<instance>@<epoch>` marker that gives cross-instance mutual
//! exclusion over "who is working this issue right now" — the property
//! §12's scheduler leans on ("claim then spawn... instances never
//! overlap").
//!
//! §8.2 describes the protocol as two steps: write the claim, "wait a
//! few seconds, then re-read to confirm the claim still reads back as
//! its own." This module honors that split with two functions rather
//! than one atomic call:
//!
//! - [`write_claim`] does the write half: it removes every other
//!   `owner:*` label already on the issue — the **single-slot marker**
//!   §8.2 requires for the "only one ends up set" property to hold (two
//!   independent labels that both persisted would break it) — then adds
//!   this instance's fresh `owner:<instance>@<epoch>` label.
//! - [`confirm_claim`] does the settle-read half: it re-reads the
//!   issue's labels and reports whether this instance's claim is the one
//!   that stuck, via [`ClaimResult`].
//!
//! The few-seconds settle delay between them is a real sleep, and it
//! belongs to the **caller**, not this module: keeping both halves free
//! of a sleep keeps them fast and deterministic to test (the tests below
//! call [`write_claim`] and [`confirm_claim`] back-to-back with no delay
//! at all, since a fake [`Vcs`] has no propagation lag to wait out in the
//! first place) and keeps this module's own surface pure I/O-via-`Vcs`
//! with no wall-clock dependency of its own — the same testability
//! reasoning [`crate::state::drift::find_stale_claims`] applies to its
//! own `now` parameter. A combining `claim_issue` wrapper that called
//! both steps in sequence with no sleep between them would defeat the
//! entire point of the settle-read (it would always read back the label
//! it had just written itself), so no such wrapper is offered here — the
//! caller that owns the sleep is also the natural place to call both
//! steps.
//!
//! No new [`Vcs`] method was needed: the claim is just a label write
//! ([`Vcs::set_label`]) and a re-read ([`Vcs::read_issue`]), both of
//! which already exist.
//!
//! Reclaiming your *own* stale claim after a restart (§8.2's explicit
//! carve-out — "an instance may reclaim its own stale claim immediately"
//! since "there is no tie-break to engineer") needs no special case
//! here: [`write_claim`] always removes whatever `owner:*` label is
//! currently set — self's own stale one included — and writes a fresh
//! one, exactly as it would for any other claim attempt. *Deciding*
//! whether a claim is stale enough to be worth attempting to reclaim is
//! the caller's job, via [`crate::state::drift::find_stale_claims`] —
//! this module only ever writes and confirms, it never checks staleness
//! itself.

use crate::state::{Claim, OWNER_PREFIX};
use crate::vcs::{Vcs, VcsError};

/// Errors from [`write_claim`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClaimError {
    /// A `git`/`gh` subprocess call failed.
    #[error(transparent)]
    Vcs(#[from] VcsError),

    /// `instance_id` cannot safely round-trip through the claim
    /// protocol, so [`write_claim`] refuses it before making any `Vcs`
    /// call rather than writing a claim label that could never be
    /// confirmed. See `is_valid_instance_id` for exactly what's
    /// accepted and why.
    #[error("instance_id {instance_id:?} is invalid: {reason}")]
    InvalidInstanceId {
        /// The rejected `instance_id`.
        instance_id: String,
        /// Why it was rejected.
        reason: &'static str,
    },
}

/// Whether `instance_id` can safely become the `<instance>` half of an
/// `owner:<instance>@<epoch>` label (§8.2) and round-trip back out
/// through [`Claim::parse`](crate::state::Claim).
///
/// An **allowlist** (ASCII letters, digits, `-`, `_`, `.`, `@`) rather
/// than a growing denylist: `ShellVcs::set_label` shells out to `gh
/// issue edit --add-label`/`--remove-label`, whose argument `gh`
/// parses as a single CSV record (confirmed against `gh`'s own source,
/// `spf13/pflag`'s `StringSliceVar` machinery) — so `,` splits it into
/// multiple labels, and a newline silently **truncates** it (CSV reads
/// one line; everything after the first `\n` is dropped without an
/// error, which is worse than the comma case: `"jon@99\nx"` would
/// truncate to a validly-parseable `owner:jon@99` claiming a forged,
/// ancient epoch instead of failing to parse at all), and a `"` fails
/// the parse outright. Denylisting characters one confirmed hazard at a
/// time already missed two of these; an allowlist closes the whole
/// class instead of the one character a given round happened to test.
fn is_valid_instance_id(instance_id: &str) -> bool {
    !instance_id.is_empty()
        && instance_id.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@')
        })
}

/// The outcome of a work-claim's settle-read ([`confirm_claim`]).
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ClaimResult {
    /// The settle-read confirms this instance's claim is the one that
    /// stuck; it may proceed to spawn the phase.
    Claimed,
    /// The settle-read found a different claim (or none at all). Per
    /// §8.2 there is deliberately no tie-break to engineer here: this
    /// instance simply does not proceed, and the issue is re-picked on
    /// the next scheduling tick — "a one-cycle delay, never a
    /// livelock."
    LostToOther {
        /// The instance whose claim won the settle-read, or `None` if no
        /// `owner:*` claim was present at all — e.g. something else
        /// cleared the label between the write and the settle-read
        /// without writing a new one in its place. Not a case the normal
        /// claim protocol produces (every write that removes an owner
        /// label also adds one), but not this instance's claim to assume
        /// either way, so it is surfaced rather than guessed at.
        instance: Option<String>,
    },
}

/// Write this instance's work-claim on `issue_number` (§8.2): remove
/// every existing `owner:*` label (enforcing the single-slot marker),
/// then add `owner:<instance_id>@<now>`.
///
/// `now` is the claim's heartbeat epoch, in Unix seconds — a parameter
/// rather than a `SystemTime::now()` call inside this function, so tests
/// control it exactly and this module never reads the wall clock itself.
///
/// This same call is how the owning instance **refreshes** its claim's
/// heartbeat (§8.2: "the owning instance rewrites `owner:<instance>@
/// <epoch>` every few minutes") and how it **reclaims its own stale
/// claim** after a restart: both are just calling this function again
/// with a new `now` — it always removes whatever is currently set
/// (self's own included) before writing fresh, so neither case needs
/// special handling.
///
/// Does not itself call [`confirm_claim`] — see the module doc for why
/// the two steps are kept separate rather than composed into one call.
///
/// # Errors
///
/// [`ClaimError::InvalidInstanceId`] if `is_valid_instance_id` rejects
/// `instance_id` (checked before any `Vcs` call, rather than writing an
/// unconfirmable claim and leaving the instance to livelock).
/// Otherwise propagates the [`VcsError`]
/// from the first failing [`Vcs`] call: [`Vcs::read_issue`] (to find
/// existing `owner:*` labels to remove) or [`Vcs::set_label`] (to remove
/// one or add the new one). A failure partway through — say, the second
/// of three removals — leaves the issue in a partially-cleaned state;
/// the caller's next scheduling tick will retry the whole claim from
/// scratch.
pub fn write_claim<V: Vcs>(
    vcs: &V,
    repo: &str,
    issue_number: u64,
    instance_id: &str,
    now: u64,
) -> Result<(), ClaimError> {
    if !is_valid_instance_id(instance_id) {
        return Err(ClaimError::InvalidInstanceId {
            instance_id: instance_id.to_string(),
            reason: "must be non-empty and contain only ASCII letters, \
                     digits, '-', '_', '.', or '@'",
        });
    }
    let issue = vcs.read_issue(repo, issue_number)?;
    for label in &issue.labels {
        if label.starts_with(OWNER_PREFIX) {
            vcs.set_label(repo, issue_number, label, false)?;
        }
    }
    let claim_label = format!("{OWNER_PREFIX}{instance_id}@{now}");
    vcs.set_label(repo, issue_number, &claim_label, true)?;
    Ok(())
}

/// The settle-read half of the claim protocol (§8.2): re-read
/// `issue_number`'s labels and report whether `instance_id`'s claim is
/// the one that stuck.
///
/// Call this only after letting the write in [`write_claim`] settle —
/// see the module doc for why that delay is the caller's responsibility,
/// not something either claim function does itself.
///
/// When more than one `owner:*` label is present — an
/// [`crate::state::drift::DriftFinding::AmbiguousLabels`] situation this
/// function does not itself flag — the *first* one wins, mirroring
/// [`crate::state::derive_issue_state`]'s own "first label wins"
/// handling of the same ambiguity, so this function and the rest of the
/// daemon never disagree about which claim is "the" claim.
///
/// # Errors
///
/// Propagates the [`VcsError`] from [`Vcs::read_issue`].
pub fn confirm_claim<V: Vcs>(
    vcs: &V,
    repo: &str,
    issue_number: u64,
    instance_id: &str,
) -> Result<ClaimResult, VcsError> {
    let issue = vcs.read_issue(repo, issue_number)?;
    let claim = issue.labels.iter().find_map(|label| Claim::parse(label));
    Ok(match claim {
        Some(claim) if claim.instance == instance_id => ClaimResult::Claimed,
        Some(claim) => {
            ClaimResult::LostToOther { instance: Some(claim.instance) }
        }
        None => ClaimResult::LostToOther { instance: None },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{FakeVcs, IssueRef, IssueState};

    const REPO: &str = "owner/repo";

    fn seed_issue(vcs: &FakeVcs, labels: &[&str]) {
        vcs.issues.borrow_mut().insert(
            (REPO.to_string(), 42),
            IssueRef {
                number: 42,
                title: "Widget cache".to_string(),
                body: String::new(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                state: IssueState::Open,
            },
        );
    }

    fn labels_of(vcs: &FakeVcs) -> Vec<String> {
        vcs.read_issue(REPO, 42).unwrap().labels
    }

    // -- is_valid_instance_id --
    //
    // Direct tests on the allowlist itself, not just through
    // write_claim's error path — after four straight rounds of
    // instance_id trouble, the boundary is worth pinning on its own.

    #[test]
    fn is_valid_instance_id_accepts_every_allowed_character() {
        assert!(is_valid_instance_id("spec-flow_a3f.jon@mbp"));
    }

    #[test]
    fn is_valid_instance_id_rejects_empty() {
        assert!(!is_valid_instance_id(""));
    }

    #[test]
    fn is_valid_instance_id_rejects_a_space() {
        assert!(!is_valid_instance_id("jon mbp"));
    }

    #[test]
    fn is_valid_instance_id_rejects_a_colon() {
        assert!(!is_valid_instance_id("jon:mbp"));
    }

    #[test]
    fn is_valid_instance_id_rejects_non_ascii() {
        assert!(!is_valid_instance_id("café"));
    }

    #[test]
    fn is_valid_instance_id_accepts_a_bare_at_sign() {
        // Unusual, but not one of the confirmed hazard classes (empty,
        // comma, newline, quote) — and it round-trips: see
        // confirm_claim_round_trips_an_instance_id_ending_in_at_sign.
        assert!(is_valid_instance_id("@"));
    }

    // -- write_claim --

    #[test]
    fn write_claim_writes_a_fresh_owner_label_on_an_unclaimed_issue() {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["status:implement"]);

        write_claim(&vcs, REPO, 42, "instance-a", 1_000).unwrap();

        assert_eq!(
            labels_of(&vcs),
            vec![
                "status:implement".to_string(),
                "owner:instance-a@1000".to_string(),
            ]
        );
    }

    #[test]
    fn write_claim_lets_an_instance_reclaim_or_refresh_its_own_claim() {
        // Covers two of §8.2's scenarios at once, since they're
        // mechanically identical from this function's point of view:
        // the periodic heartbeat refresh ("rewrites `owner:<instance>@
        // <epoch>` every few minutes") and reclaiming your own claim
        // immediately after a crash+restart ("no tie-break... an
        // instance may reclaim its own stale claim immediately").
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["owner:instance-a@100"]);

        write_claim(&vcs, REPO, 42, "instance-a", 999_999).unwrap();

        assert_eq!(
            labels_of(&vcs),
            vec!["owner:instance-a@999999".to_string()]
        );
    }

    #[test]
    fn write_claim_removes_another_instances_claim_before_writing_its_own() {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["owner:instance-b@100"]);

        write_claim(&vcs, REPO, 42, "instance-a", 500).unwrap();

        assert_eq!(labels_of(&vcs), vec!["owner:instance-a@500".to_string()]);
    }

    #[test]
    fn write_claim_removes_every_ambiguous_owner_label_before_writing_its_own()
    {
        // Two `owner:*` labels at once is itself drift
        // (`AmbiguousLabels`) that this function doesn't need to know
        // about — it just needs to leave exactly one label behind, the
        // single-slot marker §8.2 requires.
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["owner:instance-b@100", "owner:instance-c@200"]);

        write_claim(&vcs, REPO, 42, "instance-a", 500).unwrap();

        assert_eq!(labels_of(&vcs), vec!["owner:instance-a@500".to_string()]);
    }

    #[test]
    fn write_claim_preserves_non_owner_labels() {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["status:implement", "P1", "owner:instance-b@100"]);

        write_claim(&vcs, REPO, 42, "instance-a", 500).unwrap();

        assert_eq!(
            labels_of(&vcs),
            vec![
                "status:implement".to_string(),
                "P1".to_string(),
                "owner:instance-a@500".to_string(),
            ]
        );
    }

    #[test]
    fn write_claim_propagates_a_read_issue_failure() {
        let vcs = FakeVcs::new();

        assert!(matches!(
            write_claim(&vcs, REPO, 42, "instance-a", 500),
            Err(ClaimError::Vcs(VcsError::CommandFailed { .. }))
        ));
    }

    #[test]
    fn write_claim_rejects_an_empty_instance_id() {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);

        let result = write_claim(&vcs, REPO, 42, "", 500);

        assert!(matches!(result, Err(ClaimError::InvalidInstanceId { .. })));
        // Nothing was written: the label set is untouched.
        assert_eq!(labels_of(&vcs), Vec::<String>::new());
    }

    #[test]
    fn write_claim_rejects_an_instance_id_containing_a_comma() {
        // `gh issue edit --add-label`/`--remove-label` parse their
        // argument as CSV — a comma splits "mbp,jon" into two separate,
        // unparseable labels (`owner:mbp` and `jon@<epoch>`), never
        // confirmable as this instance's claim.
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);

        let result = write_claim(&vcs, REPO, 42, "mbp,jon", 500);

        assert!(matches!(result, Err(ClaimError::InvalidInstanceId { .. })));
        assert_eq!(labels_of(&vcs), Vec::<String>::new());
    }

    #[test]
    fn write_claim_rejects_an_instance_id_containing_a_newline() {
        // The nastier CSV hazard: a newline doesn't fail to parse, it
        // silently TRUNCATES. "a\nb" would be written as just "owner:a"
        // — not merely unconfirmable but, worse, "jon@99\nx" would
        // truncate to a VALIDLY-parseable owner:jon@99, claiming a
        // forged, ancient epoch instead of failing at all.
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);

        let result = write_claim(&vcs, REPO, 42, "jon@99\nx", 500);

        assert!(matches!(result, Err(ClaimError::InvalidInstanceId { .. })));
        assert_eq!(labels_of(&vcs), Vec::<String>::new());
    }

    #[test]
    fn write_claim_rejects_an_instance_id_containing_a_quote() {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);

        let result = write_claim(&vcs, REPO, 42, "jon\"mbp", 500);

        assert!(matches!(result, Err(ClaimError::InvalidInstanceId { .. })));
        assert_eq!(labels_of(&vcs), Vec::<String>::new());
    }

    #[test]
    fn write_claim_accepts_an_instance_id_with_every_allowed_character() {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["status:implement"]);

        write_claim(&vcs, REPO, 42, "spec-flow_a3f.jon@mbp", 1_000).unwrap();

        assert_eq!(
            labels_of(&vcs),
            vec![
                "status:implement".to_string(),
                "owner:spec-flow_a3f.jon@mbp@1000".to_string(),
            ]
        );
    }

    // -- confirm_claim --

    #[test]
    fn confirm_claim_reports_claimed_when_its_own_label_is_the_one_that_stuck()
    {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);
        write_claim(&vcs, REPO, 42, "instance-a", 100).unwrap();

        let result = confirm_claim(&vcs, REPO, 42, "instance-a").unwrap();

        assert_eq!(result, ClaimResult::Claimed);
    }

    #[test]
    fn confirm_claim_round_trips_an_instance_id_containing_its_own_at_sign() {
        // instance_id is operator-supplied (§11.1) and can be email- or
        // host-shaped (`jon@mbp`). Before Claim::parse split on the
        // LAST `@`, this exact round trip would silently fail to parse
        // its own fresh write and report LostToOther forever — a
        // permanent livelock against nothing but itself (§8.2 promises
        // "never a livelock").
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);
        write_claim(&vcs, REPO, 42, "jon@mbp", 100).unwrap();

        let result = confirm_claim(&vcs, REPO, 42, "jon@mbp").unwrap();

        assert_eq!(result, ClaimResult::Claimed);
    }

    #[test]
    fn confirm_claim_round_trips_an_instance_id_ending_in_at_sign() {
        // "jon@" writes the label owner:jon@@100 — two adjacent `@`s.
        // rsplit_once('@') still finds the LAST one as the epoch
        // separator, leaving "jon@" (including its own trailing `@`)
        // as the instance, so this round-trips correctly rather than
        // being misparsed as instance "jon" with a stray leading `@`
        // on the epoch.
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);
        write_claim(&vcs, REPO, 42, "jon@", 100).unwrap();

        let result = confirm_claim(&vcs, REPO, 42, "jon@").unwrap();

        assert_eq!(result, ClaimResult::Claimed);
    }

    #[test]
    fn confirm_claim_reports_lost_to_other_when_a_racing_write_wins_the_settle_read()
     {
        // The realistic race §8.2 describes: instance A writes first,
        // then (before A's settle-read) instance B writes too — B's
        // write removes A's label per the single-slot rule, so A's
        // later settle-read must see B, not itself.
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);
        write_claim(&vcs, REPO, 42, "instance-a", 100).unwrap();
        write_claim(&vcs, REPO, 42, "instance-b", 200).unwrap();

        let result = confirm_claim(&vcs, REPO, 42, "instance-a").unwrap();

        assert_eq!(
            result,
            ClaimResult::LostToOther {
                instance: Some("instance-b".to_string())
            }
        );
    }

    #[test]
    fn confirm_claim_reports_the_winner_claimed_after_it_wins_a_race() {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);
        write_claim(&vcs, REPO, 42, "instance-a", 100).unwrap();
        write_claim(&vcs, REPO, 42, "instance-b", 200).unwrap();

        let result = confirm_claim(&vcs, REPO, 42, "instance-b").unwrap();

        assert_eq!(result, ClaimResult::Claimed);
    }

    #[test]
    fn confirm_claim_reports_lost_to_other_with_no_instance_when_the_claim_vanished()
     {
        // Not a scenario the normal protocol produces on its own, but
        // this function must not assume ownership just because no
        // *other* claim is visible either.
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);
        write_claim(&vcs, REPO, 42, "instance-a", 100).unwrap();
        vcs.issues
            .borrow_mut()
            .get_mut(&(REPO.to_string(), 42))
            .unwrap()
            .labels = Vec::new();

        let result = confirm_claim(&vcs, REPO, 42, "instance-a").unwrap();

        assert_eq!(result, ClaimResult::LostToOther { instance: None });
    }

    #[test]
    fn confirm_claim_picks_the_first_owner_label_when_more_than_one_is_present()
     {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["owner:instance-a@100", "owner:instance-b@200"]);

        let result = confirm_claim(&vcs, REPO, 42, "instance-a").unwrap();

        assert_eq!(result, ClaimResult::Claimed);
    }

    #[test]
    fn confirm_claim_propagates_a_read_issue_failure() {
        let vcs = FakeVcs::new();

        assert!(matches!(
            confirm_claim(&vcs, REPO, 42, "instance-a"),
            Err(VcsError::CommandFailed { .. })
        ));
    }
}
