//! Drift detection over derived GitHub state (§12, §14 step 4).
//!
//! §12 lists six kinds of drift: "a status label disagreeing with
//! reality, an orphaned worktree, an implemented branch with no PR, a
//! stale claim (dead instance), a dependency cycle, a merged PR with an
//! open issue." This module covers three of those six — a dependency
//! cycle, a stale claim (§8.2), and a merged pull request against an
//! issue GitHub never auto-closed (most commonly a PR body missing its
//! `Closes #N`) — plus a dependency on a closed or missing issue (§8.4,
//! a related rule stated alongside "a dependency cycle" rather than one
//! of the six itself), and two additions of its own, both the same
//! "surface an ambiguity rather than silently resolve it" spirit applied
//! to a case §12 doesn't name: an issue carrying more than one
//! `status:*` or `owner:*@*` label, and an issue with more than one
//! **open** linked pull request at once — both are cases where
//! [`super::derive_issue_state`] (via its internal pull-request ranking)
//! must pick one value to keep the daemon moving, so this module exists
//! to flag that a pick was forced rather than let the choice look like
//! there was never any ambiguity.
//!
//! The remaining three of the six — a status label disagreeing with
//! reality, an orphaned worktree, and an implemented branch with no
//! PR — require comparing GitHub state against *this instance's own
//! bookkeeping* (which worktrees it created, which branches it pushed).
//! That bookkeeping belongs to the worktree manager and the phase engine
//! (§14 steps 5/7), neither of which exists yet; this module does not
//! reach for it. This is the same scoping `scaffold.rs` applies to the
//! instruction composer's job — noted honestly rather than half-built.
//!
//! Every finding is a [`DriftFinding`] variant, never a bare `String`
//! reason, so a future caller (`board`/`drift`, §6) can match on the
//! kind of drift rather than parse a message.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::Claim;
use crate::vcs::{IssueRelationships, IssueState, PullRequestState};

/// One piece of drift surfaced from GitHub state (§12).
///
/// Drift is always **surfaced, never auto-fixed** (§12, §15) — this type
/// exists to be reported (on `board`/`drift`, §6), not acted on.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DriftFinding {
    /// A `blockedBy` cycle: following each issue's dependencies
    /// eventually returns to where it started.
    DependencyCycle {
        /// The issues in the cycle, in traversal order (the last
        /// issue's `blockedBy` includes the first).
        cycle: Vec<u64>,
    },
    /// `issue` is blocked on `depends_on`, which is closed (§8.4).
    DependencyOnClosedIssue {
        /// The issue carrying the dependency.
        issue: u64,
        /// The closed issue it depends on.
        depends_on: u64,
    },
    /// `issue` is blocked on `depends_on`, which no longer exists —
    /// deleted, transferred, or never a real issue (§8.4).
    DependencyOnMissingIssue {
        /// The issue carrying the dependency.
        issue: u64,
        /// The issue number it depends on, which could not be found.
        depends_on: u64,
    },
    /// `issue`'s work-claim has gone silent past the heartbeat TTL — its
    /// owning instance is presumed dead and the claim is reclaimable
    /// (§8.2).
    StaleClaim {
        /// The issue whose claim is stale.
        issue: u64,
        /// The instance that held the claim.
        instance: String,
        /// The claim's last heartbeat, as a Unix timestamp in seconds.
        epoch: u64,
    },
    /// `issue` is still open, but one of its linked pull requests has
    /// already merged (§12) — most commonly because the PR's body lacked
    /// a `Closes #N`/`Fixes #N` keyword, so merging it never auto-closed
    /// the issue. Checked against every linked PR, not just the ranked
    /// "most relevant" one, so this finding doesn't disappear the moment
    /// a newer follow-up PR opens.
    MergedPrWithOpenIssue {
        /// The open issue.
        issue: u64,
        /// The pull request that merged without closing it.
        pull_request: u64,
    },
    /// `issue` carries more than one label matching `prefix` at once —
    /// ambiguous, since each of `status:*`/`owner:*@*` is meant to carry
    /// exactly one value (§4.3, §8.2). [`super::derive_issue_state`]
    /// still resolves this deterministically (the first label wins) so
    /// the daemon can make progress, but the ambiguity is surfaced here
    /// rather than silently swallowed.
    AmbiguousLabels {
        /// The issue carrying more than one match.
        issue: u64,
        /// The label prefix that matched more than once — `"status:"`
        /// or `"owner:"`.
        prefix: &'static str,
        /// Every matching label found, in the order `gh` returned them.
        labels: Vec<String>,
    },
    /// `issue` has more than one **open** linked pull request at once —
    /// a genuine §4.1 1:1:1:1 deviation, derivable purely from GitHub
    /// state (every candidate's state is already fetched by
    /// [`super::read_issue_state`]), unlike the drift kinds this module
    /// defers to the phase engine. `derive_issue_state` still picks one
    /// so the daemon can track *something*, but the other live PR would
    /// otherwise merge unwatched.
    MultipleOpenLinkedPullRequests {
        /// The issue carrying more than one open linked PR.
        issue: u64,
        /// Every open linked PR's number.
        pull_requests: Vec<u64>,
    },
}

/// Find every `blockedBy` cycle across `relationships`.
///
/// Pure graph function: builds each issue's `blockedBy` adjacency list
/// and walks it looking for a path that returns to its own start. Scoped
/// to `blockedBy` — the primary dependency edge the scheduler and
/// merge-gating consume (§8.4); the same treatment would apply to
/// `parent`/`subIssues` if a caller ever needed hierarchy-cycle
/// detection, but nothing in this crate reads that yet.
///
/// Each distinct cycle is reported once regardless of which issue's
/// dependencies the traversal started from, via a canonical rotation
/// (the cycle starting at its lowest-numbered issue).
pub fn find_dependency_cycles(
    relationships: &[(u64, IssueRelationships)],
) -> Vec<DriftFinding> {
    let edges: HashMap<u64, &[u64]> = relationships
        .iter()
        .map(|(issue, rel)| (*issue, rel.blocked_by.as_slice()))
        .collect();

    let mut seen = HashSet::new();
    let mut findings = Vec::new();
    for &start in edges.keys() {
        for cycle in cycles_from(start, &edges) {
            let canonical = canonicalize(&cycle);
            if seen.insert(canonical.clone()) {
                findings
                    .push(DriftFinding::DependencyCycle { cycle: canonical });
            }
        }
    }
    findings
}

/// Every simple cycle in `edges` that passes through `start`, found by a
/// depth-first walk that closes a cycle only when it leads back to
/// `start` itself (not merely back onto the current path) — the DFS is
/// re-run per starting issue so, unlike a single global visited set, it
/// cannot skip a cycle just because one of its issues was already
/// explored from a different start.
fn cycles_from(start: u64, edges: &HashMap<u64, &[u64]>) -> Vec<Vec<u64>> {
    let mut found = Vec::new();
    let mut path = vec![start];
    let mut on_path = HashSet::from([start]);
    walk(start, start, edges, &mut path, &mut on_path, &mut found);
    found
}

/// The recursive step of [`cycles_from`].
fn walk(
    start: u64,
    current: u64,
    edges: &HashMap<u64, &[u64]>,
    path: &mut Vec<u64>,
    on_path: &mut HashSet<u64>,
    found: &mut Vec<Vec<u64>>,
) {
    let Some(&neighbors) = edges.get(&current) else { return };
    for &next in neighbors {
        if next == start {
            found.push(path.clone());
        } else if on_path.insert(next) {
            path.push(next);
            walk(start, next, edges, path, on_path, found);
            path.pop();
            on_path.remove(&next);
        }
    }
}

/// Rotate `cycle` so it starts at its lowest-numbered issue, giving every
/// rotation of the same cycle the same representation.
fn canonicalize(cycle: &[u64]) -> Vec<u64> {
    let min_index = cycle
        .iter()
        .enumerate()
        .min_by_key(|&(_, &issue)| issue)
        .map(|(index, _)| index)
        .unwrap_or(0);
    let mut rotated = cycle[min_index..].to_vec();
    rotated.extend_from_slice(&cycle[..min_index]);
    rotated
}

/// Find every `blockedBy` edge that points at a closed or missing issue
/// (§8.4: "a dependency on a closed/missing issue is drift").
///
/// `issue_states` supplies the open/closed state of every issue this
/// project knows about (typically from a batch of [`crate::vcs::IssueRef`]
/// reads); an issue referenced by a `blockedBy` edge but absent from this
/// map is reported as missing rather than assumed open, since a missing
/// entry could be a deleted or transferred issue exactly as easily as one
/// the caller simply hasn't fetched — either way it is not a dependency
/// this instance can currently verify is satisfied.
pub fn find_closed_or_missing_dependencies(
    relationships: &[(u64, IssueRelationships)],
    issue_states: &HashMap<u64, IssueState>,
) -> Vec<DriftFinding> {
    let mut findings = Vec::new();
    for (issue, rel) in relationships {
        for &depends_on in &rel.blocked_by {
            match issue_states.get(&depends_on) {
                Some(IssueState::Closed) => {
                    findings.push(DriftFinding::DependencyOnClosedIssue {
                        issue: *issue,
                        depends_on,
                    });
                }
                Some(IssueState::Open) => {}
                None => {
                    findings.push(DriftFinding::DependencyOnMissingIssue {
                        issue: *issue,
                        depends_on,
                    });
                }
            }
        }
    }
    findings
}

/// Find every claim in `claims` whose heartbeat has gone silent past
/// `ttl`, as of `now` (§8.2).
///
/// `now` and `ttl` are both parameters rather than `SystemTime::now()` /
/// a config read, so this is testable without wall-clock flakiness;
/// `ttl` reuses [`crate::config::ClaimConfig::heartbeat_ttl`]'s
/// `Duration` type. A claim whose epoch is *after* `now` (clock skew, or
/// a claim written by a clock ahead of this one) is never reported stale
/// — only a claim provably older than the TTL is.
pub fn find_stale_claims(
    claims: &[(u64, Claim)],
    ttl: Duration,
    now: SystemTime,
) -> Vec<DriftFinding> {
    claims
        .iter()
        .filter_map(|(issue, claim)| {
            // `claim.epoch` comes straight from a GitHub label an
            // operator (or a bug) can write as any `u64`, including one
            // that overflows `SystemTime`'s representable range.
            // `checked_add` turns that into `None` rather than a panic;
            // treating an unrepresentable epoch the same as a future
            // one (not stale) is the safe default, since neither is a
            // claim this function can confidently reclaim.
            let claimed_at =
                UNIX_EPOCH.checked_add(Duration::from_secs(claim.epoch))?;
            let age = now.duration_since(claimed_at).ok()?;
            (age > ttl).then(|| DriftFinding::StaleClaim {
                issue: *issue,
                instance: claim.instance.clone(),
                epoch: claim.epoch,
            })
        })
        .collect()
}

/// One issue's number, open/closed state, and *every* linked pull
/// request's `(number, state)` — the shape
/// [`find_merged_pr_with_open_issue`] consumes, typically
/// [`super::IssueSnapshot::linked_pull_requests`] mapped down to just
/// what this check needs. Deliberately the full list, not just the
/// ranked "most relevant" one [`super::IssueSnapshot::pull_request`]
/// carries: an issue can have a merged PR that should have closed it
/// *and* a newer open follow-up at the same time (the open one ranks
/// higher), and checking only the winner would make the merged-without-
/// closing drift disappear the moment the follow-up PR opens — exactly
/// the kind of silent reconciliation §15 forbids.
pub type IssueAndLinkedPullRequests =
    (u64, IssueState, Vec<(u64, PullRequestState)>);

/// Find every merged pull request linked to a still-open issue (§12:
/// "a merged PR with an open issue").
///
/// Checks every linked pull request, not just the most relevant one —
/// see [`IssueAndLinkedPullRequests`] for why that distinction matters.
/// An issue can yield more than one finding if more than one linked PR
/// merged without closing it.
pub fn find_merged_pr_with_open_issue(
    issues: &[IssueAndLinkedPullRequests],
) -> Vec<DriftFinding> {
    let mut findings = Vec::new();
    for (issue, state, pull_requests) in issues {
        if !matches!(state, IssueState::Open) {
            continue;
        }
        for (pr_number, pr_state) in pull_requests {
            if matches!(pr_state, PullRequestState::Merged) {
                findings.push(DriftFinding::MergedPrWithOpenIssue {
                    issue: *issue,
                    pull_request: *pr_number,
                });
            }
        }
    }
    findings
}

/// Find every issue carrying more than one `status:*` or `owner:*@*`
/// label (see [`DriftFinding::AmbiguousLabels`]).
///
/// Counts by literal prefix match, not by successfully parsing a claim
/// or a phase — a second, malformed `owner:` label is exactly as
/// ambiguous as a second well-formed one, and just as worth surfacing.
pub fn find_ambiguous_labels(
    issues: &[(u64, &[String])],
) -> Vec<DriftFinding> {
    let mut findings = Vec::new();
    for (issue, labels) in issues {
        for prefix in [super::STATUS_PREFIX, super::OWNER_PREFIX] {
            let matches: Vec<String> = labels
                .iter()
                .filter(|label| label.starts_with(prefix))
                .cloned()
                .collect();
            if matches.len() > 1 {
                findings.push(DriftFinding::AmbiguousLabels {
                    issue: *issue,
                    prefix,
                    labels: matches,
                });
            }
        }
    }
    findings
}

/// Find every issue with more than one **open** linked pull request
/// (see [`DriftFinding::MultipleOpenLinkedPullRequests`]).
///
/// `issues` supplies each issue's linked pull requests' `(number,
/// state)` — typically [`super::IssueSnapshot::linked_pull_requests`]
/// mapped down to just what this check needs. Only `Open` PRs count: a
/// superseded `Closed` attempt alongside a live one is the ordinary,
/// non-drift history `derive_issue_state` already resolves correctly;
/// it's *two simultaneously open* PRs against one issue that GitHub's
/// own 1:1:1:1 expectation (§4.1) never accounts for.
pub fn find_multiple_open_linked_pull_requests(
    issues: &[(u64, &[(u64, PullRequestState)])],
) -> Vec<DriftFinding> {
    let mut findings = Vec::new();
    for (issue, pull_requests) in issues {
        let open: Vec<u64> = pull_requests
            .iter()
            .filter(|(_, state)| matches!(state, PullRequestState::Open))
            .map(|(number, _)| *number)
            .collect();
        if open.len() > 1 {
            findings.push(DriftFinding::MultipleOpenLinkedPullRequests {
                issue: *issue,
                pull_requests: open,
            });
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked_by(issue: u64, deps: &[u64]) -> (u64, IssueRelationships) {
        (
            issue,
            IssueRelationships {
                blocked_by: deps.to_vec(),
                ..Default::default()
            },
        )
    }

    // -- find_dependency_cycles --

    #[test]
    fn find_dependency_cycles_is_empty_for_a_dag() {
        let relationships =
            vec![blocked_by(1, &[2]), blocked_by(2, &[3]), blocked_by(3, &[])];

        assert!(find_dependency_cycles(&relationships).is_empty());
    }

    #[test]
    fn find_dependency_cycles_detects_a_two_issue_cycle() {
        let relationships = vec![blocked_by(1, &[2]), blocked_by(2, &[1])];

        let findings = find_dependency_cycles(&relationships);

        assert_eq!(
            findings,
            vec![DriftFinding::DependencyCycle { cycle: vec![1, 2] }]
        );
    }

    #[test]
    fn find_dependency_cycles_detects_a_three_issue_cycle() {
        let relationships = vec![
            blocked_by(1, &[2]),
            blocked_by(2, &[3]),
            blocked_by(3, &[1]),
        ];

        let findings = find_dependency_cycles(&relationships);

        assert_eq!(
            findings,
            vec![DriftFinding::DependencyCycle { cycle: vec![1, 2, 3] }]
        );
    }

    #[test]
    fn find_dependency_cycles_detects_a_self_dependency() {
        let relationships = vec![blocked_by(1, &[1])];

        let findings = find_dependency_cycles(&relationships);

        assert_eq!(
            findings,
            vec![DriftFinding::DependencyCycle { cycle: vec![1] }]
        );
    }

    #[test]
    fn find_dependency_cycles_reports_each_cycle_exactly_once() {
        // A three-issue cycle where every issue also has an unrelated
        // incoming edge from outside the cycle (4 -> 1) — without
        // de-duplication this would surface once per starting issue.
        let relationships = vec![
            blocked_by(1, &[2]),
            blocked_by(2, &[3]),
            blocked_by(3, &[1]),
            blocked_by(4, &[1]),
        ];

        let findings = find_dependency_cycles(&relationships);

        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn find_dependency_cycles_ignores_a_shared_dependency_that_is_not_a_cycle()
    {
        // 1 and 2 both depend on 3, which depends on nothing — a common
        // diamond shape that must never be mistaken for a cycle.
        let relationships =
            vec![blocked_by(1, &[3]), blocked_by(2, &[3]), blocked_by(3, &[])];

        assert!(find_dependency_cycles(&relationships).is_empty());
    }

    // -- find_closed_or_missing_dependencies --

    #[test]
    fn find_closed_or_missing_dependencies_is_empty_when_all_deps_are_open() {
        let relationships = vec![blocked_by(1, &[2])];
        let states = HashMap::from([(2, IssueState::Open)]);

        assert!(
            find_closed_or_missing_dependencies(&relationships, &states)
                .is_empty()
        );
    }

    #[test]
    fn find_closed_or_missing_dependencies_flags_a_closed_dependency() {
        let relationships = vec![blocked_by(1, &[2])];
        let states = HashMap::from([(2, IssueState::Closed)]);

        let findings =
            find_closed_or_missing_dependencies(&relationships, &states);

        assert_eq!(
            findings,
            vec![DriftFinding::DependencyOnClosedIssue {
                issue: 1,
                depends_on: 2
            }]
        );
    }

    #[test]
    fn find_closed_or_missing_dependencies_flags_a_missing_dependency() {
        let relationships = vec![blocked_by(1, &[2])];
        let states = HashMap::new();

        let findings =
            find_closed_or_missing_dependencies(&relationships, &states);

        assert_eq!(
            findings,
            vec![DriftFinding::DependencyOnMissingIssue {
                issue: 1,
                depends_on: 2
            }]
        );
    }

    #[test]
    fn find_closed_or_missing_dependencies_checks_each_dependency_independently()
     {
        let relationships = vec![blocked_by(1, &[2, 3, 4])];
        let states = HashMap::from([
            (2, IssueState::Open),
            (3, IssueState::Closed),
            // 4 is absent from the map entirely.
        ]);

        let findings =
            find_closed_or_missing_dependencies(&relationships, &states);

        assert_eq!(
            findings,
            vec![
                DriftFinding::DependencyOnClosedIssue {
                    issue: 1,
                    depends_on: 3
                },
                DriftFinding::DependencyOnMissingIssue {
                    issue: 1,
                    depends_on: 4
                },
            ]
        );
    }

    // -- find_stale_claims --

    fn claim(instance: &str, epoch: u64) -> Claim {
        Claim { instance: instance.to_string(), epoch }
    }

    #[test]
    fn find_stale_claims_flags_a_claim_older_than_the_ttl() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let claims = vec![(1, claim("spec-flow-a3f", 0))];

        let findings =
            find_stale_claims(&claims, Duration::from_secs(3_600), now);

        assert_eq!(
            findings,
            vec![DriftFinding::StaleClaim {
                issue: 1,
                instance: "spec-flow-a3f".to_string(),
                epoch: 0,
            }]
        );
    }

    #[test]
    fn find_stale_claims_does_not_flag_a_claim_within_the_ttl() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let claims = vec![(1, claim("spec-flow-a3f", 0))];

        let findings =
            find_stale_claims(&claims, Duration::from_secs(3_600), now);

        assert!(findings.is_empty());
    }

    #[test]
    fn find_stale_claims_does_not_flag_a_claim_exactly_at_the_ttl_boundary() {
        let now = UNIX_EPOCH + Duration::from_secs(3_600);
        let claims = vec![(1, claim("spec-flow-a3f", 0))];

        // Age == ttl exactly: `age > ttl` is false, so this is not yet
        // stale — the boundary belongs to "still alive".
        let findings =
            find_stale_claims(&claims, Duration::from_secs(3_600), now);

        assert!(findings.is_empty());
    }

    #[test]
    fn find_stale_claims_ignores_a_claim_whose_epoch_is_in_the_future() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let claims = vec![(1, claim("spec-flow-a3f", 5_000))];

        let findings =
            find_stale_claims(&claims, Duration::from_secs(3_600), now);

        assert!(findings.is_empty());
    }

    #[test]
    fn find_stale_claims_does_not_panic_on_an_epoch_that_overflows_system_time()
     {
        // A `u64` epoch is whatever text sits after `@` in an
        // `owner:<instance>@<epoch>` label — writable by anyone with
        // triage access, or by a buggy heartbeat writer. `u64::MAX`
        // seconds since the Unix epoch overflows `SystemTime`'s
        // representable range; this must not crash a drift scan.
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let claims = vec![(1, claim("spec-flow-a3f", u64::MAX))];

        let findings =
            find_stale_claims(&claims, Duration::from_secs(3_600), now);

        assert!(findings.is_empty());
    }

    #[test]
    fn find_stale_claims_checks_each_claim_independently() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let claims = vec![
            (1, claim("spec-flow-a3f", 0)),
            (2, claim("spec-flow-b7c", 9_999)),
        ];

        let findings =
            find_stale_claims(&claims, Duration::from_secs(3_600), now);

        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0],
            DriftFinding::StaleClaim {
                issue: 1,
                instance: "spec-flow-a3f".to_string(),
                epoch: 0,
            }
        );
    }

    // -- find_merged_pr_with_open_issue --

    #[test]
    fn find_merged_pr_with_open_issue_flags_an_open_issue_behind_a_merged_pr()
    {
        let issues =
            vec![(1, IssueState::Open, vec![(7, PullRequestState::Merged)])];

        let findings = find_merged_pr_with_open_issue(&issues);

        assert_eq!(
            findings,
            vec![DriftFinding::MergedPrWithOpenIssue {
                issue: 1,
                pull_request: 7
            }]
        );
    }

    #[test]
    fn find_merged_pr_with_open_issue_ignores_a_closed_issue() {
        let issues =
            vec![(1, IssueState::Closed, vec![(7, PullRequestState::Merged)])];

        assert!(find_merged_pr_with_open_issue(&issues).is_empty());
    }

    #[test]
    fn find_merged_pr_with_open_issue_ignores_an_open_pr() {
        let issues =
            vec![(1, IssueState::Open, vec![(7, PullRequestState::Open)])];

        assert!(find_merged_pr_with_open_issue(&issues).is_empty());
    }

    #[test]
    fn find_merged_pr_with_open_issue_ignores_an_issue_with_no_linked_pr() {
        let issues = vec![(1, IssueState::Open, Vec::new())];

        assert!(find_merged_pr_with_open_issue(&issues).is_empty());
    }

    #[test]
    fn find_merged_pr_with_open_issue_still_flags_a_merged_pr_alongside_a_newer_open_one()
     {
        // The exact regression this shape change guards against: an
        // issue whose first PR merged without a `Closes #N` (so the
        // issue stayed open), plus a second, newer PR that's still
        // open. Checking only the "most relevant" (ranked-highest,
        // Open) PR would make this drift vanish the moment the second
        // PR opened — it must not.
        let issues = vec![(
            1,
            IssueState::Open,
            vec![(7, PullRequestState::Merged), (9, PullRequestState::Open)],
        )];

        let findings = find_merged_pr_with_open_issue(&issues);

        assert_eq!(
            findings,
            vec![DriftFinding::MergedPrWithOpenIssue {
                issue: 1,
                pull_request: 7,
            }]
        );
    }

    // -- find_ambiguous_labels --

    #[test]
    fn find_ambiguous_labels_is_empty_when_every_prefix_matches_once() {
        let labels = vec!["status:implement".to_string(), "P1".to_string()];
        let issues = vec![(1, labels.as_slice())];

        assert!(find_ambiguous_labels(&issues).is_empty());
    }

    #[test]
    fn find_ambiguous_labels_flags_two_status_labels_on_one_issue() {
        let labels =
            vec!["status:review".to_string(), "status:implement".to_string()];
        let issues = vec![(1, labels.as_slice())];

        let findings = find_ambiguous_labels(&issues);

        assert_eq!(
            findings,
            vec![DriftFinding::AmbiguousLabels {
                issue: 1,
                prefix: "status:",
                labels: vec![
                    "status:review".to_string(),
                    "status:implement".to_string()
                ],
            }]
        );
    }

    #[test]
    fn find_ambiguous_labels_flags_two_owner_labels_on_one_issue() {
        let labels = vec![
            "owner:spec-flow-a3f@1700000000".to_string(),
            "owner:spec-flow-b7c@1700000500".to_string(),
        ];
        let issues = vec![(1, labels.as_slice())];

        let findings = find_ambiguous_labels(&issues);

        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0],
            DriftFinding::AmbiguousLabels { prefix, .. } if *prefix == "owner:"
        ));
    }

    #[test]
    fn find_ambiguous_labels_checks_each_issue_independently() {
        let ok_labels = vec!["status:implement".to_string()];
        let ambiguous_labels =
            vec!["status:review".to_string(), "status:implement".to_string()];
        let issues =
            vec![(1, ok_labels.as_slice()), (2, ambiguous_labels.as_slice())];

        let findings = find_ambiguous_labels(&issues);

        assert_eq!(findings.len(), 1);
        assert!(
            matches!(&findings[0], DriftFinding::AmbiguousLabels { issue, .. } if *issue == 2)
        );
    }

    // -- find_multiple_open_linked_pull_requests --

    #[test]
    fn find_multiple_open_linked_pull_requests_is_empty_for_a_single_open_pr()
    {
        let prs = [(7, PullRequestState::Open)];
        let issues = vec![(1, prs.as_slice())];

        assert!(find_multiple_open_linked_pull_requests(&issues).is_empty());
    }

    #[test]
    fn find_multiple_open_linked_pull_requests_ignores_a_closed_pr_alongside_an_open_one()
     {
        // The ordinary, non-drift history: a superseded closed attempt
        // plus the live PR that replaced it — exactly what
        // `most_relevant_pull_request` already resolves correctly.
        let prs = [(7, PullRequestState::Closed), (9, PullRequestState::Open)];
        let issues = vec![(1, prs.as_slice())];

        assert!(find_multiple_open_linked_pull_requests(&issues).is_empty());
    }

    #[test]
    fn find_multiple_open_linked_pull_requests_flags_two_simultaneously_open_prs()
     {
        let prs = [(7, PullRequestState::Open), (9, PullRequestState::Open)];
        let issues = vec![(1, prs.as_slice())];

        let findings = find_multiple_open_linked_pull_requests(&issues);

        assert_eq!(
            findings,
            vec![DriftFinding::MultipleOpenLinkedPullRequests {
                issue: 1,
                pull_requests: vec![7, 9],
            }]
        );
    }

    #[test]
    fn find_multiple_open_linked_pull_requests_checks_each_issue_independently()
     {
        let one_open = [(7, PullRequestState::Open)];
        let two_open =
            [(8, PullRequestState::Open), (9, PullRequestState::Open)];
        let issues = vec![(1, one_open.as_slice()), (2, two_open.as_slice())];

        let findings = find_multiple_open_linked_pull_requests(&issues);

        assert_eq!(findings.len(), 1);
        assert!(matches!(
            &findings[0],
            DriftFinding::MultipleOpenLinkedPullRequests { issue, .. } if *issue == 2
        ));
    }
}
