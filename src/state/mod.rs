//! The GitHub-state layer — derived, per-issue state (§4.2, §14 step 4).
//!
//! Everything the phase engine (a later step, §14 step 7) will need to
//! decide what to do next about one issue — its workflow position, which
//! phases need a human, what has been approved, who holds it and since
//! when, its dependency/hierarchy edges, and its PR/CI/merge-queue
//! signals — is encoded as GitHub labels, relationships, and PR state
//! (§2.3's table). This module turns that raw [`Vcs`] output into the
//! typed [`IssueSnapshot`] the rest of the daemon will read.
//!
//! Two halves, kept deliberately separate (the style guide's "minimal
//! required methods, layer behavior on top" trait-design rule applied to
//! functions instead of a trait):
//!
//! - [`derive_issue_state`] is **pure** — it takes already-fetched
//!   [`IssueRef`]/[`IssueRelationships`]/[`PullRequestStatus`] and
//!   returns an [`IssueSnapshot`]. All the label-parsing logic (prefix
//!   stripping, `owner:<instance>@<epoch>` epoch parsing) lives here, so
//!   it is unit-testable with no [`Vcs`] at all.
//! - [`read_issue_state`] is the thin orchestrator: it calls
//!   [`Vcs::read_issue`], [`Vcs::read_relationships`],
//!   [`Vcs::find_linked_pull_requests`], and (when one is linked)
//!   [`Vcs::read_pull_request_status`], then feeds the results through
//!   [`derive_issue_state`].
//!
//! # What this module does not do
//!
//! Label-prefix conventions (`gate:`, `approved:`, `owner:`) and the
//! fixed `P0`–`P3` priority set are hard-coded here to their §11.3
//! defaults. Making them project-configurable is the committed
//! `.spec-flow/workflow.yaml` parser's job — that parser does not exist
//! yet (§14 step 7, the phase engine); until it lands, every project is
//! assumed to use the shipped defaults, the same scoping this crate's
//! `scaffold.rs` already applies to the instruction-composer's job.
//!
//! Drift detection over this derived state ([`DriftFinding`] and
//! friends) lives in the sibling [`drift`] module.

pub mod drift;

use crate::vcs::{
    IssueRef, IssueRelationships, IssueState, PullRequestState,
    PullRequestStatus, Vcs, VcsError,
};

/// The `status:<phase>` label prefix (§11.3 `labels.status_prefix`
/// default; see the module-level note on configurability).
///
/// `pub(crate)` so [`drift::find_ambiguous_labels`] can count matches of
/// this prefix without duplicating it.
pub(crate) const STATUS_PREFIX: &str = "status:";

/// The `gate:<phase>` label prefix (§4.3, §11.3 `labels.gate_prefix`).
const GATE_PREFIX: &str = "gate:";

/// The `approved:<phase>` label prefix (§4.3, §11.3
/// `labels.approval_prefix`).
///
/// `pub(crate)` — see [`STATUS_PREFIX`]; [`crate::workflow::advance`]
/// reuses this same prefix to derive the approval-label suffix a
/// phase's `approval_label` actually names, rather than assuming it
/// always equals the phase's `id`.
pub(crate) const APPROVAL_PREFIX: &str = "approved:";

/// The `owner:<instance>@<epoch>` label prefix (§8.2, §11.3
/// `labels.owner_prefix`).
///
/// `pub(crate)` — see [`STATUS_PREFIX`].
pub(crate) const OWNER_PREFIX: &str = "owner:";

/// An issue's priority (§4.2, §11.3 `labels.priority`).
///
/// The fixed four-level set spec §11.3 ships as the default
/// (`[P0, P1, P2, P3]`); see the module-level note on configurability.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Priority {
    /// Highest priority.
    P0,
    /// High priority.
    P1,
    /// Normal priority.
    P2,
    /// Low priority.
    P3,
}

impl Priority {
    /// Parse a bare `P0`–`P3` label, or `None` for anything else.
    fn parse(label: &str) -> Option<Priority> {
        match label {
            "P0" => Some(Priority::P0),
            "P1" => Some(Priority::P1),
            "P2" => Some(Priority::P2),
            "P3" => Some(Priority::P3),
            _ => None,
        }
    }
}

/// A parsed work-claim: the `owner:<instance>@<epoch>` label (§8.2).
///
/// `epoch` is the Unix timestamp (seconds) embedded in the label — the
/// claim's heartbeat. [`drift::find_stale_claims`] is what turns this,
/// plus a TTL and a "now", into a staleness verdict; parsing alone makes
/// no claim about freshness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Claim {
    /// The claiming instance's id (`instance_id`, §11.1).
    pub instance: String,
    /// The heartbeat's Unix timestamp, in seconds.
    pub epoch: u64,
}

impl Claim {
    /// Parse `owner:<instance>@<epoch>` out of a single label.
    ///
    /// Returns `None` for a label that doesn't carry the owner prefix, is
    /// missing the `@<epoch>` suffix, has an empty instance id, or has a
    /// non-numeric epoch — every one of these is "not an owner claim",
    /// not a distinct error, since a caller is scanning arbitrary issue
    /// labels and most of them are never owner claims at all.
    ///
    /// Splits on the **last** `@`, not the first: `instance_id` is
    /// operator-supplied (§11.1) and nothing stops it from containing
    /// its own `@` (an email- or host-shaped id, e.g. `jon@mbp`). Since
    /// only the trailing segment is ever numeric, `rsplit_once` parses
    /// `owner:jon@mbp@1700000000` back to instance `jon@mbp` — the exact
    /// value that was written — instead of failing to parse the epoch
    /// and silently producing "not a claim at all" for a real, freshly
    /// written claim, which would confirm_claim into a permanent
    /// LostToOther and livelock the instance against its own writes
    /// (§8.2 promises "never a livelock").
    ///
    /// `pub(crate)` — see [`STATUS_PREFIX`]; [`crate::claim`] reuses this
    /// same parse rather than duplicating it.
    pub(crate) fn parse(label: &str) -> Option<Claim> {
        let rest = label.strip_prefix(OWNER_PREFIX)?;
        let (instance, epoch) = rest.rsplit_once('@')?;
        if instance.is_empty() {
            return None;
        }
        let epoch = epoch.parse().ok()?;
        Some(Claim { instance: instance.to_string(), epoch })
    }
}

/// The derived state of one GitHub issue (§4.2).
///
/// Everything here is computed from GitHub's own representation
/// (§2.3's table) — there is nothing in this type that isn't
/// re-derivable at any moment by re-reading the issue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IssueSnapshot {
    /// The issue number.
    pub number: u64,
    /// The issue title.
    pub title: String,
    /// Open or closed.
    pub state: IssueState,
    /// Priority, from a `P0`–`P3` label; `None` when unset.
    pub priority: Option<Priority>,
    /// The current workflow phase, from its `status:<phase>` label;
    /// `None` when the issue carries no status label yet (e.g. still
    /// being groomed, §7.2 ph.1).
    pub status: Option<String>,
    /// Phases that need a human gate on this issue, from its
    /// `gate:<phase>` labels (§4.3).
    pub gates: Vec<String>,
    /// Phases already approved on this issue, from its
    /// `approved:<phase>` labels (§4.3).
    pub approvals: Vec<String>,
    /// The current work-claim, if any, parsed from an `owner:<instance>@
    /// <epoch>` label (§8.2).
    pub owner: Option<Claim>,
    /// Dependency/hierarchy edges (§8.4).
    pub relationships: IssueRelationships,
    /// The most relevant linked pull request's state/CI/merge-queue
    /// signals (`Open` > `Merged` > `Closed`, see
    /// `most_relevant_pull_request`), or `None` before implement opens
    /// one.
    pub pull_request: Option<PullRequestStatus>,
    /// Every pull request linked to this issue, not just the most
    /// relevant one — normally 0 or 1 (§4.1's 1:1:1:1 invariant), but
    /// kept in full so [`drift::find_multiple_open_linked_pull_requests`]
    /// can flag the deviation when it's more, rather than that signal
    /// being lost the moment [`Self::pull_request`] picks a winner.
    pub linked_pull_requests: Vec<PullRequestStatus>,
}

/// Derive an [`IssueSnapshot`] from already-fetched GitHub state.
///
/// Pure: no I/O, no [`Vcs`] call — every label-parsing decision (prefix
/// stripping, epoch parsing) and the pull-request ranking live here so
/// they are unit-testable without a fake or a real `gh`. See
/// [`read_issue_state`] for the thin
/// orchestrator that fetches `issue`/`relationships`/`pull_requests` and
/// calls this.
///
/// `status` and `owner` are each meant to carry exactly one value, but
/// nothing stops an operator (or a race between two instances) from
/// leaving more than one `status:*` or `owner:*@*` label on an issue at
/// once. When that happens, the *first* one in `issue.labels`' order
/// wins — deterministic given one fetch's label order, though that
/// order is GitHub's to choose, not this daemon's. The ambiguity itself
/// is not swallowed: [`drift::find_ambiguous_labels`] flags any issue
/// carrying more than one match of either prefix, so a contested claim
/// or a stuck-status issue surfaces on the drift board rather than
/// quietly resolving to whichever label GitHub happened to return
/// first. `pull_requests` gets the same treatment for a multi-linked-PR
/// deviation: [`IssueSnapshot::pull_request`] resolves it via an
/// internal ranking (`Open` > `Merged` > `Closed`) so the daemon can
/// still make progress, but the full list is kept on
/// [`IssueSnapshot::linked_pull_requests`] so
/// [`drift::find_multiple_open_linked_pull_requests`] can flag it.
pub fn derive_issue_state(
    issue: &IssueRef,
    relationships: &IssueRelationships,
    pull_requests: Vec<PullRequestStatus>,
) -> IssueSnapshot {
    let mut status = None;
    let mut gates = Vec::new();
    let mut approvals = Vec::new();
    let mut priority = None;
    let mut owner = None;

    for label in &issue.labels {
        if let Some(phase) = label.strip_prefix(STATUS_PREFIX) {
            status.get_or_insert_with(|| phase.to_string());
        } else if let Some(phase) = label.strip_prefix(GATE_PREFIX) {
            gates.push(phase.to_string());
        } else if let Some(phase) = label.strip_prefix(APPROVAL_PREFIX) {
            approvals.push(phase.to_string());
        } else if let Some(claim) = Claim::parse(label) {
            owner.get_or_insert(claim);
        } else if let Some(p) = Priority::parse(label) {
            priority.get_or_insert(p);
        }
    }

    let pull_request = most_relevant_pull_request(pull_requests.clone());

    IssueSnapshot {
        number: issue.number,
        title: issue.title.clone(),
        state: issue.state,
        priority,
        status,
        gates,
        approvals,
        owner,
        relationships: relationships.clone(),
        pull_request,
        linked_pull_requests: pull_requests,
    }
}

/// Read one issue's full derived state from GitHub (§4.2).
///
/// Composes [`Vcs::read_issue`], [`Vcs::read_relationships`], and — for
/// every pull request [`Vcs::find_linked_pull_requests`] reports —
/// [`Vcs::read_pull_request_status`], then feeds the results through
/// [`derive_issue_state`], which resolves the (normally 0- or 1-element,
/// §4.1) list to a single most-relevant pull request while keeping the
/// full list on [`IssueSnapshot::linked_pull_requests`] for
/// [`drift::find_multiple_open_linked_pull_requests`] to check.
///
/// # Errors
///
/// Propagates whatever [`VcsError`] the first failing call returns.
pub fn read_issue_state<V: Vcs>(
    vcs: &V,
    repo: &str,
    issue_number: u64,
) -> Result<IssueSnapshot, VcsError> {
    let issue = vcs.read_issue(repo, issue_number)?;
    let relationships = vcs.read_relationships(repo, issue_number)?;
    let linked = vcs.find_linked_pull_requests(repo, issue_number)?;
    let mut pull_requests = Vec::with_capacity(linked.len());
    for pr in linked {
        pull_requests.push(vcs.read_pull_request_status(repo, pr.number)?);
    }

    Ok(derive_issue_state(&issue, &relationships, pull_requests))
}

/// The most relevant of an issue's (possibly several) linked pull
/// requests: `Open` outranks `Merged`, which outranks `Closed` — the
/// first match within the highest-ranked tier wins ties. Ranking by
/// state, not list position, means a superseded closed attempt never
/// shadows a later, live pull request just because GitHub happened to
/// report it first.
fn most_relevant_pull_request(
    statuses: Vec<PullRequestStatus>,
) -> Option<PullRequestStatus> {
    fn rank(state: PullRequestState) -> u8 {
        match state {
            PullRequestState::Open => 2,
            PullRequestState::Merged => 1,
            PullRequestState::Closed => 0,
        }
    }

    statuses.into_iter().fold(None, |best, candidate| match &best {
        Some(current) if rank(current.state) >= rank(candidate.state) => best,
        _ => Some(candidate),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{
        CiConclusion, FakeVcs, PullRequestRef, PullRequestState,
    };

    fn issue(labels: &[&str]) -> IssueRef {
        IssueRef {
            number: 42,
            title: "Widget cache".to_string(),
            body: String::new(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            state: IssueState::Open,
        }
    }

    fn pr_status() -> PullRequestStatus {
        PullRequestStatus {
            number: 7,
            state: PullRequestState::Open,
            ci: CiConclusion::Pending,
            in_merge_queue: false,
        }
    }

    // -- Claim::parse --

    #[test]
    fn claim_parses_a_well_formed_owner_label() {
        let claim = Claim::parse("owner:spec-flow-a3f@1700000000").unwrap();

        assert_eq!(claim.instance, "spec-flow-a3f");
        assert_eq!(claim.epoch, 1_700_000_000);
    }

    #[test]
    fn claim_rejects_a_label_without_the_owner_prefix() {
        assert!(Claim::parse("status:ready").is_none());
    }

    #[test]
    fn claim_rejects_a_label_missing_the_epoch_separator() {
        assert!(Claim::parse("owner:spec-flow-a3f").is_none());
    }

    #[test]
    fn claim_rejects_a_non_numeric_epoch() {
        assert!(Claim::parse("owner:spec-flow-a3f@soon").is_none());
    }

    #[test]
    fn claim_rejects_an_empty_instance_id() {
        assert!(Claim::parse("owner:@1700000000").is_none());
    }

    #[test]
    fn claim_parses_an_instance_id_containing_its_own_at_sign() {
        // instance_id is operator-supplied (§11.1) and can be email- or
        // host-shaped (`jon@mbp`). Splitting on the FIRST `@` would grab
        // "jon" as the instance and fail to parse "mbp@1700000000" as a
        // number, silently rejecting a real, freshly written claim —
        // exactly the round-trip failure that would livelock an
        // instance against its own writes (§8.2).
        let claim = Claim::parse("owner:jon@mbp@1700000000").unwrap();

        assert_eq!(claim.instance, "jon@mbp");
        assert_eq!(claim.epoch, 1_700_000_000);
    }

    // -- Priority::parse --

    #[test]
    fn priority_parses_each_of_the_four_levels() {
        assert_eq!(Priority::parse("P0"), Some(Priority::P0));
        assert_eq!(Priority::parse("P1"), Some(Priority::P1));
        assert_eq!(Priority::parse("P2"), Some(Priority::P2));
        assert_eq!(Priority::parse("P3"), Some(Priority::P3));
    }

    #[test]
    fn priority_rejects_an_unrelated_label() {
        assert_eq!(Priority::parse("gate:architecture"), None);
    }

    // -- derive_issue_state --

    #[test]
    fn derive_issue_state_reads_status_from_its_label() {
        let snapshot = derive_issue_state(
            &issue(&["status:implement"]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert_eq!(snapshot.status, Some("implement".to_string()));
    }

    #[test]
    fn derive_issue_state_has_no_status_when_the_label_is_absent() {
        let snapshot = derive_issue_state(
            &issue(&["P1"]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert_eq!(snapshot.status, None);
    }

    #[test]
    fn derive_issue_state_collects_every_gate_label() {
        let snapshot = derive_issue_state(
            &issue(&["gate:architecture", "gate:test-plan"]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert_eq!(
            snapshot.gates,
            vec!["architecture".to_string(), "test-plan".to_string()]
        );
    }

    #[test]
    fn derive_issue_state_collects_every_approval_label() {
        let snapshot = derive_issue_state(
            &issue(&["approved:product-spec", "approved:architecture"]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert_eq!(
            snapshot.approvals,
            vec!["product-spec".to_string(), "architecture".to_string()]
        );
    }

    #[test]
    fn derive_issue_state_reads_priority_from_a_p_label() {
        let snapshot = derive_issue_state(
            &issue(&["P0"]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert_eq!(snapshot.priority, Some(Priority::P0));
    }

    #[test]
    fn derive_issue_state_reads_a_valid_owner_claim() {
        let snapshot = derive_issue_state(
            &issue(&["owner:spec-flow-a3f@1700000000"]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        let claim = snapshot.owner.unwrap();
        assert_eq!(claim.instance, "spec-flow-a3f");
        assert_eq!(claim.epoch, 1_700_000_000);
    }

    #[test]
    fn derive_issue_state_ignores_a_malformed_owner_label() {
        let snapshot = derive_issue_state(
            &issue(&["owner:spec-flow-a3f"]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert!(snapshot.owner.is_none());
    }

    #[test]
    fn derive_issue_state_picks_the_first_status_label_when_several_are_present()
     {
        let snapshot = derive_issue_state(
            &issue(&["status:review", "status:implement"]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert_eq!(snapshot.status, Some("review".to_string()));
    }

    #[test]
    fn derive_issue_state_picks_the_first_owner_claim_when_several_are_present()
     {
        let snapshot = derive_issue_state(
            &issue(&[
                "owner:spec-flow-a3f@1700000000",
                "owner:spec-flow-b7c@1700000500",
            ]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert_eq!(snapshot.owner.unwrap().instance, "spec-flow-a3f");
    }

    #[test]
    fn derive_issue_state_with_no_special_labels_is_all_none_and_empty() {
        let snapshot = derive_issue_state(
            &issue(&["needs-triage"]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert_eq!(snapshot.status, None);
        assert!(snapshot.gates.is_empty());
        assert!(snapshot.approvals.is_empty());
        assert_eq!(snapshot.priority, None);
        assert!(snapshot.owner.is_none());
    }

    #[test]
    fn derive_issue_state_carries_relationships_through_untouched() {
        let relationships = IssueRelationships {
            blocked_by: vec![41],
            blocking: vec![50],
            parent: Some(10),
            sub_issues: vec![43, 44],
        };

        let snapshot =
            derive_issue_state(&issue(&[]), &relationships, Vec::new());

        assert_eq!(snapshot.relationships, relationships);
    }

    #[test]
    fn derive_issue_state_carries_pull_request_status_through_when_present() {
        let snapshot = derive_issue_state(
            &issue(&[]),
            &IssueRelationships::default(),
            vec![pr_status()],
        );

        assert_eq!(snapshot.pull_request, Some(pr_status()));
        assert_eq!(snapshot.linked_pull_requests, vec![pr_status()]);
    }

    #[test]
    fn derive_issue_state_has_no_pull_request_when_none_is_supplied() {
        let snapshot = derive_issue_state(
            &issue(&[]),
            &IssueRelationships::default(),
            Vec::new(),
        );

        assert_eq!(snapshot.pull_request, None);
        assert!(snapshot.linked_pull_requests.is_empty());
    }

    #[test]
    fn derive_issue_state_keeps_every_linked_pull_request_not_just_the_chosen_one()
     {
        let statuses = vec![
            status(7, PullRequestState::Closed),
            status(9, PullRequestState::Open),
        ];

        let snapshot = derive_issue_state(
            &issue(&[]),
            &IssueRelationships::default(),
            statuses.clone(),
        );

        assert_eq!(snapshot.pull_request.unwrap().number, 9);
        assert_eq!(snapshot.linked_pull_requests, statuses);
    }

    // -- read_issue_state --

    #[test]
    fn read_issue_state_composes_issue_relationships_and_pr_status() {
        let vcs = FakeVcs::new();
        vcs.issues.borrow_mut().insert(
            ("owner/repo-a".to_string(), 42),
            issue(&["status:implement", "P1"]),
        );
        vcs.relationships.borrow_mut().insert(
            ("owner/repo-a".to_string(), 42),
            IssueRelationships { blocked_by: vec![41], ..Default::default() },
        );
        vcs.linked_pull_requests.borrow_mut().insert(
            ("owner/repo-a".to_string(), 42),
            vec![PullRequestRef {
                number: 7,
                url: "https://github.com/owner/repo-a/pull/7".to_string(),
                branch: "issue-42-widget-cache".to_string(),
            }],
        );
        vcs.pull_request_statuses
            .borrow_mut()
            .insert(("owner/repo-a".to_string(), 7), pr_status());

        let snapshot = read_issue_state(&vcs, "owner/repo-a", 42).unwrap();

        assert_eq!(snapshot.status, Some("implement".to_string()));
        assert_eq!(snapshot.priority, Some(Priority::P1));
        assert_eq!(snapshot.relationships.blocked_by, vec![41]);
        assert_eq!(snapshot.pull_request, Some(pr_status()));
    }

    #[test]
    fn read_issue_state_has_no_pull_request_when_none_is_linked() {
        let vcs = FakeVcs::new();
        vcs.issues.borrow_mut().insert(
            ("owner/repo-a".to_string(), 42),
            issue(&["status:architecture"]),
        );

        let snapshot = read_issue_state(&vcs, "owner/repo-a", 42).unwrap();

        assert_eq!(snapshot.pull_request, None);
    }

    #[test]
    fn read_issue_state_propagates_a_read_issue_failure() {
        let vcs = FakeVcs::new();

        assert!(matches!(
            read_issue_state(&vcs, "owner/repo-a", 42),
            Err(VcsError::CommandFailed { .. })
        ));
    }

    #[test]
    fn read_issue_state_propagates_a_linked_pr_status_failure() {
        // The linked PR exists, but its status was never seeded — the
        // `?` inside the loop over linked PRs must actually propagate,
        // not silently skip an unreadable PR.
        let vcs = FakeVcs::new();
        vcs.issues.borrow_mut().insert(
            ("owner/repo-a".to_string(), 42),
            issue(&["status:implement"]),
        );
        vcs.linked_pull_requests.borrow_mut().insert(
            ("owner/repo-a".to_string(), 42),
            vec![PullRequestRef {
                number: 7,
                url: "https://github.com/owner/repo-a/pull/7".to_string(),
                branch: "issue-42-widget-cache".to_string(),
            }],
        );

        assert!(matches!(
            read_issue_state(&vcs, "owner/repo-a", 42),
            Err(VcsError::CommandFailed { .. })
        ));
    }

    #[test]
    fn read_issue_state_prefers_a_live_pr_over_an_earlier_closed_one() {
        // A first implement attempt (#7) closed as the wrong approach;
        // a second (#9) is open. GitHub lists #7 first. The snapshot
        // must report the live PR, not the superseded closed one.
        let vcs = FakeVcs::new();
        vcs.issues.borrow_mut().insert(
            ("owner/repo-a".to_string(), 42),
            issue(&["status:implement"]),
        );
        let pr_ref = |number: u64| PullRequestRef {
            number,
            url: format!("https://github.com/owner/repo-a/pull/{number}"),
            branch: "issue-42-widget-cache".to_string(),
        };
        vcs.linked_pull_requests.borrow_mut().insert(
            ("owner/repo-a".to_string(), 42),
            vec![pr_ref(7), pr_ref(9)],
        );
        vcs.pull_request_statuses.borrow_mut().insert(
            ("owner/repo-a".to_string(), 7),
            PullRequestStatus {
                number: 7,
                state: PullRequestState::Closed,
                ci: CiConclusion::Failure,
                in_merge_queue: false,
            },
        );
        vcs.pull_request_statuses.borrow_mut().insert(
            ("owner/repo-a".to_string(), 9),
            PullRequestStatus {
                number: 9,
                state: PullRequestState::Open,
                ci: CiConclusion::Success,
                in_merge_queue: false,
            },
        );

        let snapshot = read_issue_state(&vcs, "owner/repo-a", 42).unwrap();

        assert_eq!(snapshot.pull_request.unwrap().number, 9);
    }

    // -- most_relevant_pull_request --

    fn status(number: u64, state: PullRequestState) -> PullRequestStatus {
        PullRequestStatus {
            number,
            state,
            ci: CiConclusion::Pending,
            in_merge_queue: false,
        }
    }

    #[test]
    fn most_relevant_pull_request_is_none_for_an_empty_list() {
        assert_eq!(most_relevant_pull_request(Vec::new()), None);
    }

    #[test]
    fn most_relevant_pull_request_picks_open_over_an_earlier_closed_one() {
        let statuses = vec![
            status(7, PullRequestState::Closed),
            status(9, PullRequestState::Open),
        ];

        assert_eq!(most_relevant_pull_request(statuses).unwrap().number, 9);
    }

    #[test]
    fn most_relevant_pull_request_picks_open_over_a_later_closed_one() {
        let statuses = vec![
            status(9, PullRequestState::Open),
            status(7, PullRequestState::Closed),
        ];

        assert_eq!(most_relevant_pull_request(statuses).unwrap().number, 9);
    }

    #[test]
    fn most_relevant_pull_request_picks_merged_over_closed() {
        let statuses = vec![
            status(7, PullRequestState::Closed),
            status(8, PullRequestState::Merged),
        ];

        assert_eq!(most_relevant_pull_request(statuses).unwrap().number, 8);
    }

    #[test]
    fn most_relevant_pull_request_breaks_a_tie_with_the_first_seen() {
        let statuses = vec![
            status(7, PullRequestState::Closed),
            status(8, PullRequestState::Closed),
        ];

        assert_eq!(most_relevant_pull_request(statuses).unwrap().number, 7);
    }
}
