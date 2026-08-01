//! Board rendering (§13, §14 step 9): a pure aggregation of
//! already-derived per-issue state ([`IssueSnapshot`], §14 step 4) into
//! the row shape the `board` MCP tool (§6, §13) would return.
//!
//! # What this covers, and what it deliberately doesn't
//!
//! §13 describes the board as "issues × phase × owner-instance ×
//! **worktree** × signals × gate state." This module covers every
//! column derivable from GitHub state alone — phase (`status`), open/
//! closed (`state`), owner instance (`owner`), signals (the linked PR's
//! CI/merge-queue state), gate state (`gates`/`approvals`), and
//! dependency edges (`relationships`, including `blocked_by` — §15's
//! "surface a blocked dependency" rule has no other read-path home the
//! way merge-queue/lease/drift do, see below, so this module carries it
//! rather than silently dropping it). **Worktree** and **per-agent**
//! columns are not included: that data lives in
//! [`crate::spawner::ProcessSpawner`]'s in-memory `LocalProcess` map, a
//! live-daemon-runtime concept with no relationship to an
//! [`IssueSnapshot`] today — nothing in this crate ties the two
//! together yet, since there is no `serve` loop running both at once.
//! [`build_board`] returns exactly the columns it can answer for
//! honestly; a future step that actually runs both the GitHub-state
//! layer and the spawner side by side is where worktree/per-agent
//! enrichment belongs, not a guess bolted on here.
//!
//! Also out of scope, by §13's own words: merge-queue/lease state and
//! drift are **separate** MCP tools/read paths (`lease_status`, `drift`)
//! rendered "inline" alongside the board, not columns merged into it —
//! [`crate::state::drift`] and [`crate::merge`] already cover those on
//! their own terms; duplicating them into board rows would just be two
//! sources of truth for the same fact.
//!
//! # The backlog view (§6, §2.7)
//!
//! [`build_backlog`] is the same kind of pure aggregation over the same
//! [`IssueSnapshot`]s, for §6's other board-family read tool: the
//! **not-yet-`ready`** issues, each annotated with its readiness gap. It
//! lives here rather than in a module of its own because it is a
//! *projection of the same input* — every backlog row's issue is also a
//! board row's issue (both start from the project's open issues), and the
//! backlog's narrower column set exists precisely so a coordinator that
//! wants the rest calls `board`, not so this module can hold two
//! divergent copies of one row shape.

use crate::state::{IssueSnapshot, Priority};
use crate::vcs::{IssueRelationships, IssueState, PullRequestStatus};
use crate::workflow::{WorkflowConfig, handoff_ready_reached, readiness_gap};

/// One row of the board (§13). Carries the raw ingredients for the
/// clickable GitHub link §15 requires every rendered issue reference to
/// use (`[#N (title)](url)`, never a bare number) — `number`/`title`/
/// `url` are enough to format that string, but this type deliberately
/// does not format it itself: §13 draws the line between the `board`
/// tool (returns data) and the coordinator agent (renders it), and a
/// pre-formatted markdown string would blur that boundary for every
/// other consumer of the same fields (a PR body, a different comment
/// shape, ...).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoardRow {
    /// The issue number.
    pub number: u64,
    /// The issue title.
    pub title: String,
    /// The issue's GitHub URL — see [`issue_url`].
    pub url: String,
    /// Open or closed.
    pub state: IssueState,
    /// The current workflow phase, from `IssueSnapshot::status`.
    pub status: Option<String>,
    /// The issue's priority, from `IssueSnapshot::priority`.
    pub priority: Option<Priority>,
    /// Phases still needing a human gate on this issue.
    pub gates: Vec<String>,
    /// Phases already approved on this issue.
    pub approvals: Vec<String>,
    /// The claiming instance's id, if any (§8.2) — just the id, not the
    /// full [`crate::state::Claim`] (its heartbeat epoch is
    /// claim-protocol bookkeeping; staleness against that epoch is
    /// `crate::state::drift::find_stale_claims`'s own finding, rendered
    /// as drift rather than duplicated into every board row).
    pub owner: Option<String>,
    /// Dependency/hierarchy edges (§8.4) — most importantly
    /// `blocked_by`: unlike merge-queue/lease state and drift (each a
    /// separate MCP read path §13 renders alongside the board, not
    /// merged into it), an ordinary open, unmerged `blocked_by` edge has
    /// no other read path at all (it is not itself a
    /// `crate::state::drift::DriftFinding` — only a *cycle* or a
    /// dependency on a *closed/missing* issue is), so dropping it here
    /// would make it unrenderable without an extra `issue` call per row.
    pub relationships: IssueRelationships,
    /// The most relevant linked pull request's state/CI/merge-queue
    /// signals, if any.
    pub pull_request: Option<PullRequestStatus>,
}

/// The GitHub URL for `issue_number` in `repo` (`owner/repo`), on
/// `host` (`None` for the default `github.com`, `Some(...)` for a GitHub
/// Enterprise host, §8.5's `GhConfig::host`).
///
/// Deterministically derived from already-known data — `repo` and
/// `issue_number` are exactly what every `Vcs` call already threads
/// through — rather than fetched or stored anywhere: GitHub's issue URL
/// shape (`https://<host>/<repo>/issues/<number>`) is stable and
/// requires no additional `Vcs` round trip to construct.
pub fn issue_url(repo: &str, host: Option<&str>, issue_number: u64) -> String {
    let host = host.unwrap_or("github.com");
    format!("https://{host}/{repo}/issues/{issue_number}")
}

/// Build the board (§13): one [`BoardRow`] per snapshot, sorted by issue
/// number for a stable, deterministic rendering order.
///
/// `repo`/`host` are the same values every [`crate::vcs::Vcs`] call for
/// this project already carries (`ProjectConfig::repo`, `ProjectConfig::
/// gh`'s optional host) — passed in rather than read from `Vcs` again,
/// since [`IssueSnapshot`] itself carries no repo/host of its own (§14
/// step 4's snapshots are already scoped to one project by whoever
/// fetched them).
pub fn build_board(
    snapshots: &[IssueSnapshot],
    repo: &str,
    host: Option<&str>,
) -> Vec<BoardRow> {
    let mut rows: Vec<BoardRow> = snapshots
        .iter()
        .map(|snapshot| BoardRow {
            number: snapshot.number,
            title: snapshot.title.clone(),
            url: issue_url(repo, host, snapshot.number),
            state: snapshot.state,
            status: snapshot.status.clone(),
            priority: snapshot.priority,
            gates: snapshot.gates.clone(),
            approvals: snapshot.approvals.clone(),
            owner: snapshot.owner.as_ref().map(|claim| claim.instance.clone()),
            relationships: snapshot.relationships.clone(),
            pull_request: snapshot.pull_request,
        })
        .collect();
    rows.sort_by_key(|row| row.number);
    rows
}

/// One row of the backlog (§6 `backlog`, §2.7) — a **not-yet-`ready`**
/// issue and what it still needs to clear the §7.2 ph.5 handoff bar.
///
/// Deliberately narrower than [`BoardRow`]: §6 asks this tool for "the
/// prioritized not-yet-`ready` issues, each annotated with its readiness
/// gap," and every backlog issue is by construction also a board issue
/// (both views start from the project's open issues), so a coordinator
/// that wants owner/dependency/PR columns for one of these calls `board`
/// or `issue` rather than having them duplicated here. What is *not*
/// duplicated is also what cannot go stale between the two views.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BacklogRow {
    /// The issue number.
    pub number: u64,
    /// The issue title.
    pub title: String,
    /// The issue's GitHub URL — see [`issue_url`]; §15 requires every
    /// rendered issue reference to be a clickable link.
    pub url: String,
    /// The current workflow phase, from `IssueSnapshot::status`. `None`
    /// for an issue that carries no status label at all — still being
    /// groomed (§7.2 ph.1), and the most backlog-shaped state there is.
    pub status: Option<String>,
    /// The issue's priority, from `IssueSnapshot::priority` — the field
    /// this view is ordered by (§2.7's "surface next by priority").
    pub priority: Option<Priority>,
    /// Phases already approved on this issue (§4.3), carried alongside
    /// [`Self::readiness_gap`] so a coordinator can see what the gap is
    /// measured against without a second call.
    pub approvals: Vec<String>,
    /// Which approvals this issue still needs to reach `status:ready`
    /// (§7.2 ph.5), from [`crate::workflow::readiness_gap`] — bare
    /// approval keys, in the order the workflow declares them.
    ///
    /// **Empty is meaningful, not a filler value**: it usually means the
    /// design package is fully approved and the issue is waiting only to
    /// be stamped `status:ready` — exactly the row §2.7's queue-filling
    /// loop wants at the top of its worklist, which is why membership in
    /// this view is decided by
    /// [`crate::workflow::handoff_ready_reached`] (a status question) and
    /// not by "has a non-empty gap" (an approvals question).
    ///
    /// One degenerate case reads the same way without meaning it: a
    /// workflow that declares no `status:ready` handoff seam at all (see
    /// [`crate::workflow::readiness_gap`]'s doc) reports every row's gap
    /// as empty, because there is no approval bar to measure one against
    /// — not because every issue is actually ready. The shipped default
    /// always declares the seam, so this only bites a team-edited
    /// `workflow.yaml` that removed it.
    pub readiness_gap: Vec<String>,
}

/// Build the backlog (§6 `backlog`, §2.7): one [`BacklogRow`] per **open,
/// not-yet-`ready`** snapshot, ordered highest-priority first.
///
/// Membership is [`crate::workflow::handoff_ready_reached`] against
/// `workflow` — see that function for why the test is positional over the
/// team's own `labels.status` and what it does with a status the workflow
/// never defines. Closed issues are dropped outright: this row shape
/// carries no `state` column (unlike [`BoardRow`]), so a closed issue
/// could only appear here as a silently wrong entry in a worklist of work
/// still to do — the same reason [`crate::schedule::Candidate`]'s doc
/// gives for excluding them, resolved here rather than pushed onto the
/// caller since this function *can* see the state.
///
/// # Ordering
///
/// Priority first (`P0`…`P3`, unlabelled last), then ascending issue
/// number. That is §12's tiers 3 and 4 — the two that mean anything for
/// pre-`ready` work — using the same conventions
/// `crate::schedule` already fixed: `None` priority ranks *below* every
/// labelled one rather than above, and a lower issue number is the age
/// proxy (GitHub numbers issues monotonically per repo, and
/// [`crate::vcs::IssueRef`] carries no `created_at` to prefer).
///
/// §12's other two tiers are deliberately not applied. **Actionable-now**
/// (tier 1) needs each issue's claim liveness and dependency-merge state,
/// which the scheduler takes as caller-computed booleans and which no
/// phase engine exists here to supply; more to the point, it answers
/// "which issue should an agent be spawned on," while this view exists
/// for the issues that are *not* ready to be spawned on at all.
/// **Furthest-along** (tier 2) is already carried per row as the
/// readiness gap itself, at finer resolution than a phase rank: two
/// issues both at `status:architecture` differ by which approvals have
/// landed, which the gap shows and a phase rank does not.
pub fn build_backlog(
    snapshots: &[IssueSnapshot],
    workflow: &WorkflowConfig,
    repo: &str,
    host: Option<&str>,
) -> Vec<BacklogRow> {
    let mut rows: Vec<BacklogRow> = snapshots
        .iter()
        .filter(|snapshot| matches!(snapshot.state, IssueState::Open))
        .filter(|snapshot| {
            !handoff_ready_reached(workflow, snapshot.status.as_deref())
        })
        .map(|snapshot| BacklogRow {
            number: snapshot.number,
            title: snapshot.title.clone(),
            url: issue_url(repo, host, snapshot.number),
            status: snapshot.status.clone(),
            priority: snapshot.priority,
            approvals: snapshot.approvals.clone(),
            readiness_gap: readiness_gap(workflow, snapshot),
        })
        .collect();
    rows.sort_by_key(|row| (priority_rank(row.priority), row.number));
    rows
}

/// This priority's sort rank — lower sorts first (`P0` highest), and an
/// unlabelled issue ranks below every labelled one.
///
/// The same rule `crate::schedule`'s own (private) `priority_rank`
/// applies for §12 tier 3; duplicated as four lines rather than made
/// `pub(crate)` there, because the scheduler's version is one tier of a
/// candidate comparison that also consults facts this module has no
/// business gathering — sharing the function would tie this view to that
/// signature for no gain.
fn priority_rank(priority: Option<Priority>) -> u8 {
    priority.map_or(4, |p| p as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Claim;
    use crate::vcs::{CiConclusion, PullRequestState};

    fn snapshot(number: u64, title: &str) -> IssueSnapshot {
        IssueSnapshot {
            number,
            title: title.to_string(),
            state: IssueState::Open,
            priority: None,
            status: None,
            gates: Vec::new(),
            approvals: Vec::new(),
            owner: None,
            relationships: IssueRelationships::default(),
            pull_request: None,
            linked_pull_requests: Vec::new(),
        }
    }

    // -- issue_url --

    #[test]
    fn issue_url_uses_github_com_by_default() {
        assert_eq!(
            issue_url("owner/repo-a", None, 42),
            "https://github.com/owner/repo-a/issues/42"
        );
    }

    #[test]
    fn issue_url_honors_a_custom_host() {
        assert_eq!(
            issue_url("owner/repo-a", Some("github.example.com"), 42),
            "https://github.example.com/owner/repo-a/issues/42"
        );
    }

    // -- build_board --

    #[test]
    fn build_board_carries_every_field_through_from_the_snapshot() {
        let mut snap = snapshot(42, "Widget cache");
        snap.priority = Some(Priority::P1);
        snap.status = Some("implement".to_string());
        snap.gates = vec!["review".to_string()];
        snap.approvals = vec!["product-spec".to_string()];
        snap.owner =
            Some(Claim { instance: "jon@mbp".to_string(), epoch: 100 });
        snap.relationships = IssueRelationships {
            blocked_by: vec![12],
            blocking: Vec::new(),
            parent: None,
            sub_issues: Vec::new(),
        };
        let pr = PullRequestStatus {
            number: 7,
            state: PullRequestState::Open,
            ci: CiConclusion::Success,
            in_merge_queue: true,
        };
        snap.pull_request = Some(pr);

        let board = build_board(&[snap], "owner/repo-a", None);

        assert_eq!(board.len(), 1);
        let row = &board[0];
        assert_eq!(row.number, 42);
        assert_eq!(row.title, "Widget cache");
        assert_eq!(row.url, "https://github.com/owner/repo-a/issues/42");
        assert_eq!(row.state, IssueState::Open);
        assert_eq!(row.status.as_deref(), Some("implement"));
        assert_eq!(row.priority, Some(Priority::P1));
        assert_eq!(row.gates, vec!["review".to_string()]);
        assert_eq!(row.approvals, vec!["product-spec".to_string()]);
        assert_eq!(row.owner.as_deref(), Some("jon@mbp"));
        assert_eq!(row.relationships.blocked_by, vec![12]);
        assert_eq!(row.pull_request, Some(pr));
    }

    #[test]
    fn build_board_threads_the_host_override_into_every_url() {
        // Pinned separately from issue_url's own host test: this would
        // still pass if build_board hard-coded `issue_url(repo, None,
        // ...)` internally and silently dropped the host parameter --
        // exactly the §8.5 GHE-scoping regression this covers.
        let board = build_board(
            &[snapshot(42, "t")],
            "owner/repo-a",
            Some("github.example.com"),
        );

        assert_eq!(
            board[0].url,
            "https://github.example.com/owner/repo-a/issues/42"
        );
    }

    #[test]
    fn build_board_reports_a_closed_issue_as_closed() {
        let mut snap = snapshot(1, "t");
        snap.state = IssueState::Closed;

        let board = build_board(&[snap], "owner/repo-a", None);

        assert_eq!(board[0].state, IssueState::Closed);
    }

    #[test]
    fn build_board_reports_no_owner_for_an_unclaimed_issue() {
        let board = build_board(&[snapshot(1, "t")], "owner/repo-a", None);

        assert_eq!(board[0].owner, None);
    }

    #[test]
    fn build_board_sorts_by_issue_number_regardless_of_input_order() {
        let board = build_board(
            &[snapshot(9, "nine"), snapshot(2, "two"), snapshot(5, "five")],
            "owner/repo-a",
            None,
        );

        let numbers: Vec<u64> = board.iter().map(|row| row.number).collect();
        assert_eq!(numbers, vec![2, 5, 9]);
    }

    #[test]
    fn build_board_is_empty_for_no_snapshots() {
        assert!(build_board(&[], "owner/repo-a", None).is_empty());
    }

    // -- build_backlog --

    /// The shipped default workflow (§11.3) — the same config a freshly
    /// `init`ed project runs, so these assertions pin the real
    /// `product-spec`/`architecture`/`test-plan` bar rather than a
    /// fixture invented here.
    fn workflow() -> crate::workflow::WorkflowConfig {
        crate::workflow::parse_workflow(crate::scaffold::DEFAULT_WORKFLOW_YAML)
            .unwrap()
    }

    fn at(number: u64, status: Option<&str>) -> IssueSnapshot {
        let mut snap = snapshot(number, "t");
        snap.status = status.map(|s| s.to_string());
        snap
    }

    #[test]
    fn build_backlog_keeps_only_the_issues_before_the_ready_seam() {
        let snapshots = vec![
            at(1, None),
            at(2, Some("architecture")),
            at(3, Some("ready")),
            at(4, Some("implement")),
            at(5, Some("done")),
        ];

        let backlog = build_backlog(&snapshots, &workflow(), "o/r", None);

        let numbers: Vec<u64> = backlog.iter().map(|row| row.number).collect();
        assert_eq!(numbers, vec![1, 2]);
    }

    #[test]
    fn build_backlog_annotates_each_row_with_what_it_still_needs() {
        let mut snap = at(1, Some("test-plan"));
        snap.approvals = vec!["product-spec".to_string()];

        let backlog = build_backlog(&[snap], &workflow(), "o/r", None);

        assert_eq!(backlog[0].approvals, vec!["product-spec".to_string()]);
        assert_eq!(
            backlog[0].readiness_gap,
            vec!["architecture".to_string(), "test-plan".to_string()]
        );
    }

    #[test]
    fn build_backlog_keeps_a_fully_approved_issue_that_is_not_stamped_ready() {
        // The row §2.7's queue-filling loop most wants: the design
        // package is complete, only the `status:ready` stamp is missing.
        // An empty gap must not be mistaken for "not backlog work".
        let mut snap = at(1, Some("test-plan"));
        snap.approvals = vec![
            "product-spec".to_string(),
            "architecture".to_string(),
            "test-plan".to_string(),
        ];

        let backlog = build_backlog(&[snap], &workflow(), "o/r", None);

        assert_eq!(backlog.len(), 1);
        assert!(backlog[0].readiness_gap.is_empty());
    }

    #[test]
    fn build_backlog_orders_by_priority_then_issue_number() {
        let mut p2 = at(1, Some("product-spec"));
        p2.priority = Some(Priority::P2);
        let mut p0 = at(2, Some("product-spec"));
        p0.priority = Some(Priority::P0);
        let mut p0_later = at(3, Some("product-spec"));
        p0_later.priority = Some(Priority::P0);

        let backlog =
            build_backlog(&[p2, p0_later, p0], &workflow(), "o/r", None);

        let numbers: Vec<u64> = backlog.iter().map(|row| row.number).collect();
        assert_eq!(numbers, vec![2, 3, 1]);
    }

    #[test]
    fn build_backlog_ranks_an_unlabelled_priority_last_not_first() {
        // `Option<Priority>`'s derived ordering would put `None` first
        // (highest); §12 tier 3 wants the opposite.
        let unlabelled = at(1, Some("product-spec"));
        let mut p3 = at(2, Some("product-spec"));
        p3.priority = Some(Priority::P3);

        let backlog =
            build_backlog(&[unlabelled, p3], &workflow(), "o/r", None);

        let numbers: Vec<u64> = backlog.iter().map(|row| row.number).collect();
        assert_eq!(numbers, vec![2, 1]);
    }

    #[test]
    fn build_backlog_omits_a_closed_issue() {
        // A `BacklogRow` has no `state` column, so a closed issue could
        // only appear here as a wrong entry in a worklist of work still
        // to do -- see `build_backlog`'s doc.
        let mut closed = at(1, Some("architecture"));
        closed.state = IssueState::Closed;

        assert!(build_backlog(&[closed], &workflow(), "o/r", None).is_empty());
    }

    #[test]
    fn build_backlog_threads_the_host_override_into_every_url() {
        let backlog = build_backlog(
            &[at(42, Some("architecture"))],
            &workflow(),
            "owner/repo-a",
            Some("github.example.com"),
        );

        assert_eq!(
            backlog[0].url,
            "https://github.example.com/owner/repo-a/issues/42"
        );
    }

    #[test]
    fn build_backlog_is_empty_for_no_snapshots() {
        assert!(
            build_backlog(&[], &workflow(), "owner/repo-a", None).is_empty()
        );
    }
}
