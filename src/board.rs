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

use crate::state::{IssueSnapshot, Priority};
use crate::vcs::{IssueRelationships, IssueState, PullRequestStatus};

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
}
