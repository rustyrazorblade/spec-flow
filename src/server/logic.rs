//! The MCP tools' bodies, as ordinary synchronous functions over the
//! [`Vcs`] seam — everything about §6's `register`/`board`/`issue`/
//! `backlog`/`drift`/`approve`/`set_gate`/`advance` that can be decided
//! or fetched without an async runtime or an HTTP connection in scope.
//!
//! This is the split §5 asks for ("everything but the `git`/`gh` layer
//! and the process spawner is unit-testable by stubbing those seams")
//! applied to the server: the functions here take `&impl Vcs`, so every
//! one of them is exercised against [`crate::vcs::FakeVcs`] below, while
//! [`super`]'s `rmcp` handler is left as thin, near-untestable glue —
//! argument decoding, `spawn_blocking`, error mapping — covered instead
//! by `tests/mcp_server.rs`'s end-to-end run against a real client.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::board::{
    BacklogRow, BoardRow, build_backlog, build_board, issue_url,
};
use crate::config::{
    ConfigError, GlobalConfig, ProjectConfig, ProjectPointer,
    load_project_config, project_config_path,
};
use crate::registry::find_project_containing;
use crate::state::drift::{
    DriftFinding, find_ambiguous_labels, find_closed_or_missing_dependencies,
    find_dependency_cycles, find_merged_pr_with_open_issue,
    find_multiple_open_linked_pull_requests, find_stale_claims,
};
use crate::state::{Claim, IssueSnapshot, STATUS_PREFIX, read_issue_state};
use crate::vcs::{IssueRelationships, IssueState, Vcs, VcsError};
use crate::workflow::{
    AdvanceDecision, ApproveDecision, GateMode, LabelOp, Phase, PhaseAction,
    WorkflowConfig, WorkflowError, effective_gate_mode, is_merge_gate,
    parse_workflow,
};

/// Errors an MCP tool call can fail with (§6).
///
/// Every variant maps to a distinct JSON-RPC error at the boundary (see
/// [`super::SpecFlowServer`]); the split is by *whose* mistake it is —
/// a bad argument, a connection used out of order, or the environment
/// (config/`gh`) failing underneath.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    /// A coordinator called `register` without a `cwd` (§6: it "passes
    /// its `cwd` (the directory Claude was opened in — HTTP/SSE doesn't
    /// convey it implicitly, so the client sends it)").
    #[error(
        "register requires cwd for a coordinator connection (no \
         spawn_token was supplied); HTTP/SSE does not convey the \
         client's working directory, so it must be sent explicitly"
    )]
    CwdRequired,

    /// A `cwd` that is not an absolute path. Rejected rather than
    /// resolved: the daemon's own working directory is unrelated to the
    /// client's (§2.5 — one daemon, many projects, never "in" a
    /// checkout), so there is nothing correct to resolve against.
    #[error("register requires an absolute cwd; got {cwd}")]
    CwdNotAbsolute {
        /// The relative path the client sent.
        cwd: String,
    },

    /// `cwd` is not inside any registered project (§6: "a directory that
    /// isn't a registered project is rejected → run `fleet init`
    /// there").
    #[error(
        "{cwd} is not inside any project registered with this daemon; \
         run `spec-flow init` in that repo first"
    )]
    UnregisteredDirectory {
        /// The unregistered directory the client sent.
        cwd: String,
    },

    /// A `spawn_token` was presented. §6's spawned-phase registration
    /// path is not built in this slice; rejecting is the only honest
    /// answer, since silently treating a spawned phase as a coordinator
    /// would bind it to a project without the `(issue, phase, sub_id)`
    /// tuple its later `submit_artifact` must be checked against (§2.6).
    #[error(
        "spawn_token registration (the spawned-phase path, §2.6) is not \
         implemented yet; only coordinator registration with a cwd is \
         supported"
    )]
    SpawnTokenUnsupported,

    /// An issue-scoped tool was called before `register` bound the
    /// connection to a project (§15: "project rides the connection").
    #[error("this connection has not registered; call register first")]
    NotRegistered,

    /// A second `register` naming a different project (§15: "a
    /// connection is bound to exactly one project for its life ... and
    /// never changed").
    #[error(
        "this connection is already bound to project {bound}; a \
         connection can never act on another project ({requested}) — \
         open a new connection from that project's directory"
    )]
    AlreadyBound {
        /// The project the connection was bound to at `register`.
        bound: String,
        /// The project the rejected re-registration named.
        requested: String,
    },

    /// `board(filter?)` was called with a filter. See
    /// [`super::wire::BoardArgs::filter`].
    #[error(
        "board(filter) is not implemented yet; call board with no \
         filter to get every open issue"
    )]
    BoardFilterUnsupported,

    /// `backlog(filter?)` was called with a filter. A variant of its own
    /// rather than a shared "filter unsupported": §6 gives the two
    /// arguments no shape *separately*, and a client that gets told
    /// "board(filter) is not implemented" in response to a `backlog`
    /// call has been handed a message about a different tool. See
    /// [`super::wire::BacklogArgs::filter`].
    #[error(
        "backlog(filter) is not implemented yet; call backlog with no \
         filter to get every not-yet-ready issue"
    )]
    BacklogFilterUnsupported,

    /// `approve`/`set_gate` named a `phase` the project's effective
    /// workflow does not define (§7.0, §11.3).
    ///
    /// The caller's mistake, not the machine's: the workflow file
    /// parsed fine (a file that did not would have failed as
    /// [`ToolError::Workflow`] before this check ran), and the phase
    /// vocabulary a client may name is exactly the `phases:` list that
    /// file declares. The known ids are listed in the message rather
    /// than left for the client to discover, since a team edits that
    /// list (§7.0) and a hard-coded guess at it is exactly what would
    /// go stale.
    #[error(
        "no phase {phase} in this project's .spec-flow/workflow.yaml; \
         it defines: {}",
        known.join(", ")
    )]
    UnknownPhase {
        /// The phase id the client asked for.
        phase: String,
        /// Every phase id the project's workflow actually defines, in
        /// pipeline order.
        known: Vec<String>,
    },

    /// The project's committed `.spec-flow/workflow.yaml` could not be
    /// read. Distinct from [`ToolError::Config`], which is about the
    /// daemon's own `<project_dir>/config.yaml`: this file lives in the
    /// repo checkout and is the team's, so the fix is a different one
    /// (re-run `spec-flow init`, or check out the primary worktree)
    /// than for a missing daemon config.
    #[error("failed to read {path}")]
    ReadWorkflow {
        /// The workflow file that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The project's `.spec-flow/workflow.yaml` exists but does not
    /// parse. A named struct variant rather than `#[from]
    /// WorkflowError` for the same reason [`crate::InitError::Workflow`]
    /// is one: the parse error alone renders as a bare `serde_yaml`
    /// message naming no file at all.
    #[error("failed to parse {path}")]
    Workflow {
        /// The workflow file that failed to parse.
        path: PathBuf,
        /// The underlying parse failure.
        #[source]
        source: WorkflowError,
    },

    /// The project's own `<project_dir>/config.yaml` could not be read.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// A `git`/`gh` call failed.
    #[error(transparent)]
    Vcs(#[from] VcsError),

    /// A `spawn_blocking` worker task panicked (see `super::blocking`'s doc).
    /// Distinct from [`ToolError::Vcs`] on purpose: this is a bug in this
    /// crate, not an environment failure, and `tool_error`'s
    /// "fix your call vs. fix your machine" split needs to tell the two
    /// apart rather than reporting a panic as "failed to spawn git/gh".
    #[error("an internal worker task failed: {0}")]
    Internal(String),
}

/// Resolve a coordinator's `cwd` to the registered project containing it
/// and load that project's settings (§2.5, §6).
///
/// Two steps, both required: [`find_project_containing`] answers *which*
/// registered project owns the directory (the global config holds only
/// pointers, §11.1), and [`load_project_config`] reads that project's own
/// file for the repo slug every later `gh` call is scoped with (§8.5).
pub fn resolve_project(
    global: &GlobalConfig,
    cwd: &str,
) -> Result<(ProjectPointer, ProjectConfig), ToolError> {
    let path = Path::new(cwd);
    if !path.is_absolute() {
        return Err(ToolError::CwdNotAbsolute { cwd: cwd.to_string() });
    }

    let pointer = find_project_containing(global, path).ok_or_else(|| {
        ToolError::UnregisteredDirectory { cwd: cwd.to_string() }
    })?;

    let config =
        load_project_config(&project_config_path(&pointer.project_dir))?;

    Ok((pointer.clone(), config))
}

/// The board (§13) for one project: every open issue, each read through
/// the GitHub-state layer, rendered by [`build_board`].
///
/// Returns the rows plus whether the repo hit `limit` — see
/// [`super::wire::BoardResult::truncated`] for why silence would be the
/// wrong failure mode there.
///
/// # Cost — MEASURED, and a real problem at scale
///
/// This is N+1 and serial: one [`Vcs::list_open_issues`] call, then
/// three-to-four more `gh` invocations per issue inside
/// [`read_issue_state`] (issue, relationships, linked PRs, and one PR
/// status per linked PR). Measured end to end through the MCP server
/// against a live repo with 16 open issues: **~20 seconds**, 49 `gh`
/// subprocesses. That extrapolates to minutes at
/// [`super::BOARD_ISSUE_LIMIT`], which is well past what §2.7's
/// conversational coordinator loop can absorb.
///
/// It is written this way anyway, and *not* quietly capped or cached,
/// because both fixes are real design work rather than tuning: §2.3/§5's
/// "disposable, re-derived from GitHub" local cache does not exist in
/// this crate at all yet (it needs an invalidation story the CI/PR
/// poller, §12, is what would drive), and batching would mean one
/// hand-written GraphQL document returning issue + labels +
/// relationships + linked PRs + check rollups per page — a new `Vcs`
/// method whose shape has to be validated against the live schema the
/// way every other GraphQL call in [`crate::vcs`] already was. Returning
/// stale or truncated rows to hide the latency would be the one
/// genuinely wrong answer (§15's "surface, don't bury").
pub fn read_board<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    limit: u32,
) -> Result<(Vec<BoardRow>, bool), ToolError> {
    let numbers = vcs.list_open_issues(&project.repo, limit)?;
    let truncated = numbers.len() as u32 >= limit;

    let mut snapshots = Vec::with_capacity(numbers.len());
    for number in numbers {
        snapshots.push(read_issue_state(vcs, &project.repo, number)?);
    }

    let host = project.gh.as_ref().map(|gh| gh.host.as_str());
    Ok((build_board(&snapshots, &project.repo, host), truncated))
}

/// One issue's derived state (§6 `issue(number)`), plus the GitHub URL
/// §15 requires it to be rendered with.
pub fn read_issue<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    number: u64,
) -> Result<(IssueSnapshot, String), ToolError> {
    let snapshot = read_issue_state(vcs, &project.repo, number)?;
    let host = project.gh.as_ref().map(|gh| gh.host.as_str());
    Ok((snapshot, issue_url(&project.repo, host, number)))
}

/// Where a project's committed workflow definition lives:
/// `<local_path>/.spec-flow/workflow.yaml` (§7.2, §11.3) — under the
/// **primary checkout**, not `project_dir`, since `.spec-flow/` is a
/// committed repo file and `project_dir` is only the container the
/// checkouts are siblings in (§10.1).
fn workflow_path(project: &ProjectConfig) -> PathBuf {
    project.local_path.join(".spec-flow").join("workflow.yaml")
}

/// Read and parse the bound project's **effective** workflow config
/// (§7.0, §11.3).
///
/// Read off disk per call rather than captured once at `register`, for
/// the same reason [`crate::init`]'s `provision_label_vocabulary` reads
/// the file back instead of assuming
/// [`crate::scaffold::DEFAULT_WORKFLOW_YAML`]: it is a *committed repo
/// file a team edits*, and §7.0's whole point is that editing it changes
/// the pipeline. A config captured at connection time would serve a
/// coordinator a readiness bar its repo no longer has, with nothing to
/// signal the divergence. The cost is one small local file read on a call
/// that already spends dozens of `gh` subprocesses (see [`read_board`]'s
/// "Cost" section), i.e. nothing worth caching against.
///
/// # Errors
///
/// [`ToolError::ReadWorkflow`] when the file cannot be read (a project
/// whose primary checkout is missing, or one `init` never scaffolded),
/// and [`ToolError::Workflow`] when it does not parse. Both are hard
/// failures rather than a silent fall back to the shipped default: §6's
/// backlog answer is *defined* by this file's phase list, so answering
/// from the shipped default would silently report a readiness bar the
/// team does not use.
fn load_workflow(
    project: &ProjectConfig,
) -> Result<WorkflowConfig, ToolError> {
    let path = workflow_path(project);
    let yaml = std::fs::read_to_string(&path).map_err(|source| {
        ToolError::ReadWorkflow { path: path.clone(), source }
    })?;
    parse_workflow(&yaml)
        .map_err(|source| ToolError::Workflow { path, source })
}

/// The backlog (§6 `backlog`, §2.7) for one project: every open issue
/// that has not reached the `status:ready` handoff seam, annotated with
/// its readiness gap and ordered by priority.
///
/// Returns the rows plus whether the repo hit `limit`, exactly as
/// [`read_board`] does — and for the same reason: a silently short
/// backlog reads as "the queue is nearly full," which is the one wrong
/// answer for a tool whose entire job is telling a coordinator whether
/// there is prepared work left (§2.7).
///
/// # Cost — MEASURED, and identical to `board`'s
///
/// The same `list_open_issues` + per-issue [`read_issue_state`] fan-out
/// [`read_board`] documents, since membership and the readiness gap are
/// both derived from labels only a full issue read produces, plus one
/// local file read for the workflow config ([`load_workflow`]). Measured
/// end to end through the MCP server against a live repo with 16 open
/// issues: **49 `gh` subprocesses, ~20 seconds** — `board` measured at
/// 49/~21s in the same run, so this is not an approximation of that
/// number, it is that number.
///
/// It is *not* cheaper than `board` despite returning fewer rows: the
/// filtering happens after the reads, because nothing in
/// [`Vcs::list_open_issues`] can express "not yet ready" (see that
/// method's own doc on why it deliberately takes no filters). The
/// workflow file read costs nothing measurable next to 49 subprocesses,
/// which is why [`load_workflow`] does not cache it.
pub fn read_backlog<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    limit: u32,
) -> Result<(Vec<BacklogRow>, bool), ToolError> {
    let workflow = load_workflow(project)?;
    let numbers = vcs.list_open_issues(&project.repo, limit)?;
    let truncated = numbers.len() as u32 >= limit;

    let mut snapshots = Vec::with_capacity(numbers.len());
    for number in numbers {
        snapshots.push(read_issue_state(vcs, &project.repo, number)?);
    }

    let host = project.gh.as_ref().map(|gh| gh.host.as_str());
    Ok((build_backlog(&snapshots, &workflow, &project.repo, host), truncated))
}

/// One instance's live hold on an issue (§8.2) — the **contention** half
/// of §6's `drift()` bullet ("current drift + contention").
///
/// Every open issue carrying an `owner:<instance>@<epoch>` label appears
/// here, stale or not; [`Self::stale`] is set from the very
/// [`DriftFinding::StaleClaim`] findings in the same report, not
/// re-derived, so the two halves of one `drift()` answer can never
/// disagree about which claims have lapsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimHolder {
    /// The claimed issue.
    pub issue: u64,
    /// The claim itself (instance id + heartbeat epoch, §8.2).
    pub claim: Claim,
    /// Whether this claim's heartbeat has gone silent past the
    /// configured TTL — i.e. whether it also appears as a
    /// [`DriftFinding::StaleClaim`] in the same report.
    pub stale: bool,
}

/// One project's whole drift + contention picture (§6 `drift()`, §12).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriftReport {
    /// Every drift finding, flattened across all six checks
    /// `crate::state::drift` implements.
    pub findings: Vec<DriftFinding>,
    /// Who currently holds a claim on which issue (§8.2), stale or not.
    pub claims: Vec<ClaimHolder>,
    /// Whether the open-issue read hit `limit` — see `read_board`.
    pub truncated: bool,
}

/// Read one issue's raw labels **and** its derived snapshot in a single
/// pass of `gh` calls.
///
/// [`read_issue_state`] would give only the snapshot, and
/// [`IssueSnapshot`] deliberately does not carry the issue's raw
/// `labels` — it is *derived* state (see its module doc). But
/// [`find_ambiguous_labels`] counts **literal prefix matches**, malformed
/// ones included ("a second, malformed `owner:` label is exactly as
/// ambiguous as a second well-formed one"), which is precisely the
/// information issue-state derivation discards: two `status:*` labels
/// collapse to one `status`, and an unparseable `owner:` label vanishes
/// entirely. So the raw list cannot be recovered from a snapshot, and
/// calling [`read_issue_state`] *plus* [`Vcs::read_issue`] would pay a
/// second `gh` invocation per issue for data the first call already
/// returned.
///
/// The alternative — adding `labels: Vec<String>` to [`IssueSnapshot`] —
/// was not taken: it would republish raw labels through every existing
/// consumer of that type (board rows, the `issue` tool's wire shape) to
/// serve one caller, and blur the "derived, not raw" line that type's own
/// doc draws. This is a thin wrapper over
/// [`crate::state::read_issue_state_with_labels`] — [`read_issue_state`]'s
/// own call sequence with the issue's raw labels retained instead of
/// dropped — so a future call added to `read_issue_state` cannot silently
/// diverge from what `drift()`'s fan-out actually pays for.
fn read_issue_with_labels<V: Vcs>(
    vcs: &V,
    repo: &str,
    number: u64,
) -> Result<(Vec<String>, IssueSnapshot), ToolError> {
    Ok(crate::state::read_issue_state_with_labels(vcs, repo, number)?)
}

/// The project-wide drift + contention report (§6 `drift()`, §12) — the
/// orchestration that turns [`crate::state::drift`]'s six pure checks
/// into one answer about a whole repo.
///
/// `now`/`heartbeat_ttl` are parameters for the same reason
/// [`find_stale_claims`] takes them: staleness is testable without
/// wall-clock flakiness, and the TTL is the operator's
/// ([`crate::ClaimConfig::heartbeat_ttl`], §11.1), not this function's to
/// pick.
///
/// # Which issues a drift scan has to look at — CONFIRMED, not assumed
///
/// `board` scopes itself to [`Vcs::list_open_issues`] and stops there.
/// That is **not** sufficient here, and the reason is
/// [`find_closed_or_missing_dependencies`]'s own contract: it takes a map
/// of issue → [`IssueState`] and reports any `blockedBy` target *absent
/// from that map* as [`DriftFinding::DependencyOnMissingIssue`]. Feeding
/// it only the open set would therefore relabel every perfectly ordinary
/// dependency on a **closed** issue — which is its own, differently
/// actionable drift kind (§8.4) — as "this issue no longer exists," and
/// would do the same to a dependency on an open issue that simply fell
/// past `limit`. Two distinct findings collapsed into the wrong one is
/// exactly the silent reconciliation §15 forbids.
///
/// So this reads the open set first, then issues **one further
/// [`Vcs::read_issue`] per distinct `blockedBy` target not already in
/// it** — the minimum needed to tell closed from missing. Dependencies
/// are read for their `state` alone; no relationships or linked PRs are
/// fetched for them, so an off-board issue contributes nothing but its
/// open/closed bit.
///
/// ## The consequences of that scope, stated plainly
///
/// - **A dependency cycle passing through a closed issue is not
///   detected.** [`find_dependency_cycles`] is fed only the open issues'
///   edges, because closing the cycle through a closed issue would mean
///   fetching *its* relationships too, and then its dependencies'
///   relationships, walking an unbounded slice of the repo's history per
///   `drift()` call. The case is already surfaced by the check above
///   ([`DriftFinding::DependencyOnClosedIssue`] fires on the edge into
///   the closed issue), just not as a cycle.
/// - **A dependency read that fails is reported as *missing*, not
///   propagated.** `gh` exits non-zero for an issue that was deleted or
///   transferred, which is exactly the case
///   [`DriftFinding::DependencyOnMissingIssue`] exists to name — and
///   [`VcsError`] has no `NotFound` variant to distinguish that from a
///   transient failure or a rate limit, so this **conflates the two**: a
///   momentarily unreachable GitHub would report every off-board
///   dependency as missing. The alternative is worse (one deleted
///   dependency issue failing the entire `drift()` call, which is the one
///   report an operator runs *because* something is wrong), and the fix
///   is a `Vcs`-layer distinction between "not found" and "call failed"
///   that would change `ShellVcs`'s error mapping for every caller —
///   flagged here rather than guessed at. Each such read is logged at
///   `warn` with the underlying error, so the conflation is visible in
///   the daemon's own log even though the report cannot name it.
/// - **The other three of §12's six drift kinds are still absent** (a
///   status label disagreeing with reality, an orphaned worktree, an
///   implemented branch with no PR) — [`crate::state::drift`]'s module
///   doc already records why: they compare GitHub against this
///   instance's own worktree/phase bookkeeping, which does not exist yet.
///   This function does not paper over that; `drift()` reports the six
///   checks that are real.
///
/// # Cost — MEASURED for the baseline, REASONED for the increment
///
/// The per-issue fan-out is byte-for-byte the sequence [`read_board`]
/// measured (`read_issue` + `read_relationships` +
/// `find_linked_pull_requests` + one status per linked PR — see
/// [`read_issue_with_labels`], which is [`read_issue_state`]'s body with
/// the issue's raw labels retained). Measured end to end through the MCP
/// server against a live repo with 16 open issues and **no dependency
/// edges**: **49 `gh` subprocesses, ~20 seconds** — one `gh issue list`, then
/// three per issue (`gh issue view` plus two `gh api graphql`, for
/// relationships and linked PRs), with a further call per linked PR that
/// this repo's issues did not incur. `board` and `backlog` measured at
/// 49 subprocesses each in the same run, confirming the shared fan-out
/// rather than assuming it.
///
/// On top of that baseline, this call spends **one further `gh issue
/// view` per distinct `blockedBy` target not already in the open set**.
/// That increment is bounded by the number of dependency edges, is zero
/// for a repo whose dependencies all point at open, in-page issues — and
/// was **not exercised by the measurement above**, whose repo had no
/// `blockedBy` edges at all. Producing one would have meant *writing*
/// dependency edges to a live repo, which a read-only change has no
/// business doing. What is certain from the code is the shape (one
/// serial `gh issue view` per off-board dependency) at the same ~0.4s
/// per subprocess every other call in that measurement paid, with the
/// same absent-cache/absent-batching caveats [`read_board`] documents
/// and for the same unresolved reasons.
pub fn read_drift<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    limit: u32,
    heartbeat_ttl: Duration,
    now: SystemTime,
) -> Result<DriftReport, ToolError> {
    let repo = &project.repo;
    let mut numbers = vcs.list_open_issues(repo, limit)?;
    let truncated = numbers.len() as u32 >= limit;
    // `gh issue list` returns newest first; sorting makes every findings
    // list below deterministic for a given repo state.
    numbers.sort_unstable();

    let mut labels: Vec<(u64, Vec<String>)> =
        Vec::with_capacity(numbers.len());
    let mut snapshots = Vec::with_capacity(numbers.len());
    for number in &numbers {
        let (issue_labels, snapshot) =
            read_issue_with_labels(vcs, repo, *number)?;
        labels.push((*number, issue_labels));
        snapshots.push(snapshot);
    }

    let relationships: Vec<(u64, IssueRelationships)> = snapshots
        .iter()
        .map(|snapshot| (snapshot.number, snapshot.relationships.clone()))
        .collect();
    let issue_states =
        dependency_states(vcs, repo, &snapshots, &relationships);

    let claims: Vec<(u64, Claim)> = snapshots
        .iter()
        .filter_map(|snapshot| {
            snapshot.owner.clone().map(|claim| (snapshot.number, claim))
        })
        .collect();

    let issues_and_prs: Vec<crate::state::drift::IssueAndLinkedPullRequests> =
        snapshots
            .iter()
            .map(|snapshot| {
                (
                    snapshot.number,
                    snapshot.state,
                    snapshot
                        .linked_pull_requests
                        .iter()
                        .map(|pr| (pr.number, pr.state))
                        .collect(),
                )
            })
            .collect();
    // Borrowed straight out of `issues_and_prs` rather than rebuilt: the
    // two checks want the same per-issue PR list in slightly different
    // shapes (one with the issue's own state, one without), and a second
    // owned copy would be a second thing to keep in step.
    let linked_prs: Vec<(u64, &[(u64, crate::vcs::PullRequestState)])> =
        issues_and_prs
            .iter()
            .map(|(issue, _, prs)| (*issue, prs.as_slice()))
            .collect();

    let stale = find_stale_claims(&claims, heartbeat_ttl, now);
    let stale_issues: BTreeSet<u64> = stale
        .iter()
        .filter_map(|finding| match finding {
            DriftFinding::StaleClaim { issue, .. } => Some(*issue),
            _ => None,
        })
        .collect();

    let mut findings = Vec::new();
    findings.extend(find_dependency_cycles(&relationships));
    findings.extend(find_closed_or_missing_dependencies(
        &relationships,
        &issue_states,
    ));
    findings.extend(stale);
    findings.extend(find_merged_pr_with_open_issue(&issues_and_prs));
    findings.extend(find_ambiguous_labels(
        &labels
            .iter()
            .map(|(issue, issue_labels)| (*issue, issue_labels.as_slice()))
            .collect::<Vec<_>>(),
    ));
    findings.extend(find_multiple_open_linked_pull_requests(&linked_prs));

    let claims = claims
        .into_iter()
        .map(|(issue, claim)| ClaimHolder {
            stale: stale_issues.contains(&issue),
            issue,
            claim,
        })
        .collect();

    Ok(DriftReport { findings, claims, truncated })
}

/// The open/closed state of every issue a `blockedBy` edge points at:
/// the open set this scan already read, plus one [`Vcs::read_issue`] for
/// each referenced issue not among them.
///
/// See [`read_drift`]'s "Which issues a drift scan has to look at"
/// section for why this second pass exists at all, and why a failed read
/// leaves the issue *absent* from the map (so
/// [`find_closed_or_missing_dependencies`] reports it as missing) rather
/// than failing the whole report.
fn dependency_states<V: Vcs>(
    vcs: &V,
    repo: &str,
    snapshots: &[IssueSnapshot],
    relationships: &[(u64, IssueRelationships)],
) -> HashMap<u64, IssueState> {
    let mut states: HashMap<u64, IssueState> = snapshots
        .iter()
        .map(|snapshot| (snapshot.number, snapshot.state))
        .collect();

    // A `BTreeSet` so the extra reads happen in issue-number order,
    // making the `gh` call sequence (and anything asserting on it)
    // deterministic.
    let referenced: BTreeSet<u64> = relationships
        .iter()
        .flat_map(|(_, rel)| rel.blocked_by.iter().copied())
        .filter(|dependency| !states.contains_key(dependency))
        .collect();

    for dependency in referenced {
        match vcs.read_issue(repo, dependency) {
            Ok(issue) => {
                states.insert(dependency, issue.state);
            }
            Err(error) => {
                tracing::warn!(
                    %repo,
                    issue = dependency,
                    %error,
                    "could not read a dependency issue; reporting it as \
                     missing drift (this cannot distinguish a deleted \
                     issue from a transient gh failure — see read_drift's \
                     doc)"
                );
            }
        }
    }

    states
}

// ---------------------------------------------------------------------
// The write path (§6 "Approval & gates", §2.3b, §4.3)
// ---------------------------------------------------------------------

/// What an `approve` call actually did (§6) — the write it performed,
/// plus the issue re-read from GitHub afterwards.
///
/// The snapshot is a **re-read**, not the caller's assumption about
/// what the write produced: §2.3's "re-derive from GitHub" and §15's
/// "every state-changing MCP call writes through to GitHub before
/// returning" together mean the honest answer to "what is this issue's
/// state now" is the one GitHub gives back, which also folds in
/// anything another instance changed in between.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApproveOutcome {
    /// The decision as recorded.
    pub decision: ApproveDecision,
    /// The phase id, as the workflow spells it (echoed from the matched
    /// [`Phase`], not from the client's argument).
    pub phase: String,
    /// The approval label written, for a `grant` on a phase that
    /// declares one. `None` for every `deny`, and for a `grant` on a
    /// phase with no `approval_label` at all (see
    /// [`crate::workflow::approve`]).
    pub approval_label: Option<String>,
    /// For a `deny`: the re-entry this workflow defines for the phase
    /// (§6), as named in the posted comment. `None` when the workflow
    /// defines none — see this module's `denial_reentry` for how it is
    /// derived and when it is absent. Always `None` for a `grant`,
    /// which routes nowhere.
    pub reentry: Option<String>,
    /// The issue, re-read from GitHub after the write.
    pub snapshot: IssueSnapshot,
    /// That issue's GitHub URL (§15).
    pub url: String,
}

/// What a `set_gate` call actually did (§6, §4.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetGateOutcome {
    /// The phase id, as the workflow spells it.
    pub phase: String,
    /// The mode requested. Note [`GateMode::None`] and
    /// [`GateMode::Auto`] produce the *same* label operation — see
    /// [`crate::workflow::set_gate`] for why the per-issue label scheme
    /// cannot distinguish them.
    pub mode: GateMode,
    /// The label operation performed.
    pub label: LabelOp,
    /// What this phase's gate **actually evaluates to** right now —
    /// [`crate::workflow::effective_gate_mode`] applied to the phase's
    /// own configured default, the label just written, and whether this
    /// is the merge gate. Requesting `mode` and getting `effective_mode`
    /// back are not always the same value: `mode: Auto` on a phase whose
    /// configured default is `human` **and** which is the merge gate
    /// still evaluates to `Human` (§4.3's "merge is special — never
    /// implicit auto"), so a client that only read `mode`/`label_present`
    /// back could believe a merge gate is open when it is not. This
    /// field exists so that can never happen silently (§15's "surface,
    /// don't bury") — see `set_gate_for_issue`'s doc.
    pub effective_mode: GateMode,
    /// The issue, re-read from GitHub after the write.
    pub snapshot: IssueSnapshot,
    /// That issue's GitHub URL (§15).
    pub url: String,
}

/// What an `advance` call decided, and what (if anything) it wrote (§6).
///
/// The decision is [`crate::workflow::advance`]'s own, carried verbatim
/// rather than collapsed into a boolean: §6 asks this tool to stop at a
/// `human` gate, pass an `auto` one, and block on unmet `requires`, and
/// those are three *different* answers a coordinator has to act on
/// differently. Only [`AdvanceDecision::Advance`] writes anything; see
/// this module's `advance_issue`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvanceOutcome {
    /// Where the state machine says this issue can go next.
    pub decision: AdvanceDecision,
    /// The `status:<phase>` label this call added, when it advanced the
    /// issue. `None` for every non-[`AdvanceDecision::Advance`] decision
    /// (nothing was written at all), and also for an `Advance` into a
    /// phase declaring no [`Phase::status_label`] — see
    /// `advance_issue`'s "A target phase with no `status_label`"
    /// section.
    pub status_added: Option<String>,
    /// The `status:<phase>` label this call removed — the issue's
    /// previous status. `None` when nothing was written, or when the
    /// target phase's `status_label` is the very label just added (see
    /// `advance_issue`'s shared-`status_label` guard). An `Advance`
    /// decision from an issue with no prior status label at all is not
    /// reachable — [`crate::workflow::advance`] reports that case as
    /// [`AdvanceDecision::NoCurrentPhase`] before any `Advance` can be
    /// produced — so this field cannot be `None` for that reason in
    /// practice; the code still matches on `snapshot.status` defensively
    /// rather than asserting a state-machine invariant this type does
    /// not itself enforce.
    pub status_removed: Option<String>,
    /// The issue as GitHub reports it *after* this call.
    ///
    /// On the advancing path this is a genuine re-read taken after the
    /// label writes, the same discipline `approve_issue` and
    /// `set_gate_for_issue` follow (§2.3). On every other path nothing
    /// was written, so this is the single read the decision above was
    /// computed from — which is the same statement about GitHub, made
    /// without paying for a second round trip that could only differ if
    /// another instance wrote in between.
    pub snapshot: IssueSnapshot,
    /// That issue's GitHub URL (§15).
    pub url: String,
}

/// Find the phase `phase_id` names in the project's effective workflow.
///
/// The lookup is by [`Phase::id`] — the id §4.3's `gate:<phase>` labels
/// and §7.1's `requires: ["approved:<phase>"]` entries are written in,
/// which is not always the same string as the phase's approval label
/// (the shipped default's merge gate is `id: review` with
/// `approval_label: "approved:merge"`). A client naming `merge` there
/// is therefore told the phase does not exist, with the real id list in
/// the message; guessing that `merge` meant `review` would be this
/// crate inventing an alias the workflow file does not declare.
fn find_phase<'w>(
    workflow: &'w WorkflowConfig,
    phase_id: &str,
) -> Result<&'w Phase, ToolError> {
    workflow.phases.iter().find(|phase| phase.id == phase_id).ok_or_else(
        || ToolError::UnknownPhase {
            phase: phase_id.to_string(),
            known: workflow
                .phases
                .iter()
                .map(|phase| phase.id.clone())
                .collect(),
        },
    )
}

/// The re-entry §6 defines for a **denied** gate on `phase` — the name
/// the denial comment reports, not something this crate triggers.
///
/// §6 names four cases (product-spec→re-groom, architecture→re-propose,
/// test-plan→re-plan, merge→address). Those are read here structurally,
/// from the workflow's own shape, rather than as a hard-coded table of
/// those four ids — a phase list is team-editable (§7.0), so a table
/// keyed on the shipped default's ids would answer wrongly for any
/// other pipeline:
///
/// - The **merge gate** ([`is_merge_gate`] — the phase whose action
///   enqueues the merge, `review` in the shipped default) re-enters the
///   out-of-band phase that declares it re-enters back, i.e. the entry
///   whose [`crate::workflow::OutOfBandPhase::reenters`] is this
///   phase's id. In the shipped default that is exactly `address`
///   (`{id: address, action: {type: agent, role: developer}, reenters:
///   review}`), which is §6's `merge→address`.
/// - An **agent phase** ([`PhaseAction::Agent`], which is what
///   product-spec/architecture/test-plan are) re-enters *itself*:
///   §6's "re-groom"/"re-propose"/"re-plan" all mean running that same
///   phase again, so the name reported is the phase's own id.
/// - **Anything else** — a `Server` phase that is not the merge gate,
///   or a `ServerThenAgent` phase — gets `None`. §6 defines no re-entry
///   for a mechanical server step, and re-running one is not what its
///   re-entry words describe; reporting a re-entry there would be this
///   function asserting something the spec does not say. The denial is
///   still recorded (see [`approve_issue`]): losing an operator's
///   decision because the workflow has nowhere to route it would be the
///   worse failure. `approve_issue` places no precondition on the
///   phase's own gate mode (see its "No precondition check" section),
///   so this branch is reachable even on the shipped default by
///   denying an ungated phase like `finalize` directly — not just from
///   a team-edited config that puts a `human` gate on one; it is
///   answered rather than asserted impossible either way.
///
/// A workflow declaring **more than one** out-of-band entry re-entering
/// the merge gate resolves to the first in declaration order, and the
/// ambiguity is not reported: §6 describes one re-entry per phase, and
/// failing a recorded human decision over a config quirk would be the
/// wrong trade. The shipped default declares one entry per re-entered
/// phase.
fn denial_reentry<'w>(
    workflow: &'w WorkflowConfig,
    phase: &'w Phase,
) -> Option<&'w str> {
    if is_merge_gate(phase) {
        return workflow
            .out_of_band
            .iter()
            .find(|entry| entry.reenters == phase.id)
            .map(|entry| entry.id.as_str());
    }
    match phase.action {
        PhaseAction::Agent { .. } => Some(phase.id.as_str()),
        _ => None,
    }
}

/// The comment a **granted** gate leaves on the issue (§2.3b, §15).
///
/// Posted even when a label was written, and *especially* when one was
/// not: §2.3b makes the approval label and this MCP call two surfaces
/// of the same decision, and a phase with no `approval_label`
/// configured has no label surface at all — for it this comment is the
/// only durable record that a human said yes (§2.3's "all durable state
/// lives in GitHub").
fn approval_granted_comment(
    phase_id: &str,
    approval_label: Option<&str>,
    note: Option<&str>,
) -> String {
    let mut body = format!(
        "**Gate approved — `{phase_id}`** (via the `approve` MCP tool, \
         §2.3b/§6).\n\n"
    );
    match approval_label {
        Some(label) => body.push_str(&format!(
            "The `{label}` label is now on this issue; that label is the \
             durable record every instance reads.\n"
        )),
        None => body.push_str(
            "This phase declares no approval label, so no label was \
             written — this comment is the only record of the \
             decision.\n",
        ),
    }
    if let Some(note) = note {
        body.push_str(&format!("\nNote: {note}\n"));
    }
    body
}

/// The comment a **denied** gate leaves on the issue — §6's "records
/// the state + note" half, in full, and an explicit statement that its
/// "routes the phase back" half did not happen.
///
/// The last paragraph is not boilerplate: an operator who denies a gate
/// and is told nothing would reasonably assume the workflow moved on by
/// itself. It did not, and nothing else in this system would tell them
/// so (§15's "surface, don't bury").
fn approval_denied_comment(
    phase_id: &str,
    reentry: Option<&str>,
    note: Option<&str>,
) -> String {
    let mut body = format!(
        "**Gate denied — `{phase_id}`** (via the `approve` MCP tool, \
         §2.3b/§6). No approval label was written.\n\n"
    );
    if let Some(note) = note {
        body.push_str(&format!("Note: {note}\n\n"));
    }
    match reentry {
        Some(reentry) if reentry == phase_id => body.push_str(&format!(
            "Re-entry: this workflow re-enters `{reentry}` — the same \
             phase, run again.\n\n"
        )),
        Some(reentry) => body.push_str(&format!(
            "Re-entry: this workflow re-enters `{reentry}`.\n\n"
        )),
        None => body.push_str(
            "Re-entry: this workflow defines none for this phase, so \
             there is nothing to route back to.\n\n",
        ),
    }
    body.push_str(
        "spec-flow recorded this denial and did **not** re-run that \
         re-entry or spawn an agent for it — no phase engine runs \
         alongside the daemon yet. Trigger it yourself.",
    );
    body
}

/// `approve(issue_number, phase, decision, note?)` (§6) — grant or deny
/// a `human` gate over MCP, writing the decision through to GitHub
/// before returning (§15).
///
/// The order is §6's own: the label first ("both write the
/// corresponding GitHub label as their first act, so every instance
/// sees the decision"), then the comment, then a re-read of the issue.
/// A grant that writes its label and then fails to post its comment
/// therefore leaves the *authoritative* half done — the label is what
/// [`crate::workflow::gate_clear`] actually consults — and reports the
/// failure; the reverse order would report a failure having recorded
/// nothing that unblocks the pipeline.
///
/// # No precondition check that the gate is actually pending
///
/// §6 calls this "the alternative to applying the approval label by
/// hand", and §2.3b makes the two surfaces equal ("whichever comes
/// first advances; the other is reconciled"). Applying the label by
/// hand is checked by nothing, so a *stricter* MCP path — refusing to
/// record an approval for a phase whose effective gate is not `human`
/// right now, or that the issue has already passed — would make the two
/// surfaces disagree about what an operator is allowed to say. Granting
/// twice is idempotent **for the label** (the same label, already
/// present); granting early records an approval the gate will find
/// waiting for it, exactly as a hand-applied label would. The posted
/// comment is not idempotent the same way — a retried grant leaves a
/// second "Gate approved" comment, since each call's durable record is
/// exactly what happened on that call, not a diff against the last one.
///
/// # What is deliberately not done for `deny`
///
/// §6's deny "records the state + note **and routes the phase back to
/// its defined re-entry**". This closes the first half and explicitly
/// not the second: the comment names the re-entry
/// ([`denial_reentry`]), and nothing re-runs it. Routing means spawning
/// an agent (for the merge gate's `address`, an agent phase per §7.2)
/// or re-running a content phase, and no phase engine or spawner runs
/// alongside this server — the same boundary `super`'s module doc draws
/// for `next_assignment`. [`ApproveOutcome::reentry`] carries the name
/// so a coordinator can say what still has to happen; it is not a claim
/// that it happened.
///
/// # Cost
///
/// One or two `gh` invocations for the write — always one `gh issue
/// comment`, plus one `gh issue edit` when a grant has a label to write
/// — then [`read_issue_state`]'s usual three-to-four for the re-read
/// (see [`read_board`]'s "Cost" section for what those are), plus one
/// local file read for the workflow config ([`load_workflow`]). Unlike
/// `board`/`backlog`/`drift`, this is a single-issue call, so there is
/// no fan-out over the open-issue list — a reasoned count, not a
/// measured one, unlike theirs.
pub fn approve_issue<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    issue_number: u64,
    phase_id: &str,
    decision: ApproveDecision,
    note: Option<&str>,
) -> Result<ApproveOutcome, ToolError> {
    let workflow = load_workflow(project)?;
    let phase = find_phase(&workflow, phase_id)?;

    // `approve` returns at most one op today (a grant's approval
    // label); applying all of them keeps this correct if it ever
    // returns more, while `approval_label` below reports the first —
    // the one §6 names.
    let ops = crate::workflow::approve(phase, decision);
    for op in &ops {
        vcs.set_label(&project.repo, issue_number, &op.label, op.present)?;
    }

    let reentry = match decision {
        ApproveDecision::Grant => None,
        ApproveDecision::Deny => {
            denial_reentry(&workflow, phase).map(str::to_string)
        }
    };
    let comment = match decision {
        ApproveDecision::Grant => approval_granted_comment(
            &phase.id,
            ops.first().map(|op| op.label.as_str()),
            note,
        ),
        ApproveDecision::Deny => {
            approval_denied_comment(&phase.id, reentry.as_deref(), note)
        }
    };
    vcs.post_comment(&project.repo, issue_number, &comment)?;

    let (snapshot, url) = read_issue(vcs, project, issue_number)?;
    Ok(ApproveOutcome {
        decision,
        phase: phase.id.clone(),
        approval_label: ops.first().map(|op| op.label.clone()),
        reentry,
        snapshot,
        url,
    })
}

/// `set_gate(issue_number, phase, mode)` (§6, §4.3) — set one phase's
/// gate for one issue by adding or removing its `gate:<phase>` label,
/// then re-read the issue.
///
/// The label *is* the state (§4.3: "`set_gate` (MCP) and the label are
/// the same thing on two surfaces"), so this writes exactly the one
/// operation [`crate::workflow::set_gate`] computes and nothing else.
///
/// # Two things this deliberately does not do
///
/// - **No comment.** Unlike [`approve_issue`], which records a human
///   *decision* that a bare label cannot always carry (a phase with no
///   approval label has no label surface at all, and a denial has no
///   label by definition), every `set_gate` call is fully expressed by
///   the label it writes. Adding a comment per call would put unasked-
///   for noise on the issue for a state §6 already calls "the MCP
///   surface of that label".
/// - **Nothing distinguishes an explicit `auto` from an untouched
///   phase.** [`GateMode::None`] and [`GateMode::Auto`] both remove the
///   label, so §15's "a **recorded** `auto` choice" is not recorded
///   *as* a choice — `crate::workflow::gate`'s module doc and
///   [`crate::workflow::set_gate`]'s already record this from the read
///   and decision sides, and this tool inherits it rather than
///   inventing a marker label the rest of the crate would not read.
///   The one case where it would be unsafe is carved out already: the
///   merge gate never *falls back* to `auto` from a `human` default off
///   an absent label ([`crate::workflow::effective_gate_mode`], §4.3's
///   "merge is special"). A merge-gate phase whose *configured* default
///   is already `auto` still resolves to `auto` with the label absent —
///   that is §15's "recorded `auto` choice" living in the config
///   instead of a per-issue label, not the silent-permissive case this
///   carve-out guards against.
///
/// # A label that does not exist in the repo
///
/// Adding `gate:<phase>` needs that label to already exist
/// ([`Vcs::set_label`]'s doc). `spec-flow init` provisions every
/// `gate:<phase.id>` the workflow declares
/// ([`crate::workflow::label_vocabulary`]), so this succeeds for any
/// project whose workflow has not gained a phase since its last `init`
/// — the already-recorded gap in that provisioning, not a new one. The
/// same `gh` name-to-GraphQL-id resolution `Vcs::set_label`'s doc
/// describes for `--add-label` applies to `--remove-label` too (both
/// resolve the label's id before mutating), so `GateMode::None`/`Auto`
/// on a phase whose `gate:<phase>` label was never created inherits
/// this exact dependency, not just `GateMode::Human`'s add.
pub fn set_gate_for_issue<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    issue_number: u64,
    phase_id: &str,
    mode: GateMode,
) -> Result<SetGateOutcome, ToolError> {
    let workflow = load_workflow(project)?;
    let phase = find_phase(&workflow, phase_id)?;

    let label = crate::workflow::set_gate(phase, mode);
    vcs.set_label(&project.repo, issue_number, &label.label, label.present)?;

    // `mode`/`label.present` alone cannot answer "is this gate actually
    // human right now" for the one case §4.3 singles out: a merge-gate
    // phase configured `human` by default ignores `mode: auto`'s label
    // removal and stays `Human` (see `SetGateOutcome::effective_mode`'s
    // doc) -- computed from the same three inputs `effective_gate_mode`
    // always takes, not re-derived ad hoc.
    let effective_mode =
        effective_gate_mode(phase.gate, label.present, is_merge_gate(phase));

    let (snapshot, url) = read_issue(vcs, project, issue_number)?;
    Ok(SetGateOutcome {
        phase: phase.id.clone(),
        mode,
        label,
        effective_mode,
        snapshot,
        url,
    })
}

/// Whether every issue `snapshot`'s `blockedBy` edges point at has
/// merged or closed (§8.4) — the [`crate::RequirementContext`] fact
/// [`Requirement::DepsMerged`](crate::Requirement::DepsMerged) reads,
/// which no single issue's own state can answer.
///
/// The check is literally "is that issue [`IssueState::Closed`]", not
/// "did it merge via a PR", because that is what the requirement itself
/// is defined as: `Requirement::DepsMerged`'s doc says "every issue this
/// one's `blockedBy` points at has merged **or closed**". A merged PR
/// carrying `Closes #N` closes its issue, so the closed check subsumes
/// the merged one; an issue closed as won't-fix genuinely does unblock
/// its dependents, and treating it as still blocking would strand them.
///
/// # Fail-safe direction: an unreadable dependency is NOT satisfied
///
/// A `gh` failure on a dependency read (deleted, transferred,
/// rate-limited, offline) returns `false` — the dependency is treated as
/// unmerged. This is the **opposite** direction to [`read_drift`]'s
/// choice, which reports an unreadable dependency as
/// [`DriftFinding::DependencyOnMissingIssue`] and carries on, and the
/// difference is deliberate: `drift` is a report, while this value feeds
/// §8.1's merge gate ("don't enqueue a PR whose `blockedBy` is
/// unmerged"). Failing open here would let an unverifiable dependency
/// read as satisfied and clear the one gate §8.1 makes the server
/// responsible for. Failing closed costs a blocked `advance` call the
/// operator can retry; failing open costs an out-of-order merge nobody
/// asked for. Each such read is logged at `warn` so the reason a call
/// reported `deps_merged` unmet is visible in the daemon's own log, since
/// the [`AdvanceDecision`] itself cannot distinguish "this dependency is
/// open" from "this dependency could not be read".
///
/// Not built on [`dependency_states`], the similar-looking fan-out
/// `read_drift` uses, for exactly that reason: that function's contract
/// is "leave an unreadable issue *absent* from the map", which is the
/// failure direction this one must not have. It also reads every open
/// issue's dependencies at once, where this reads one issue's.
///
/// # Cost
///
/// One `gh issue view` per distinct `blockedBy` target, in issue-number
/// order (a [`BTreeSet`], so a test can pin the sequence), short-
/// circuiting at the first unsatisfied one. Zero for an issue with no
/// dependencies.
fn dependencies_satisfied<V: Vcs>(
    vcs: &V,
    repo: &str,
    snapshot: &IssueSnapshot,
) -> bool {
    let dependencies: BTreeSet<u64> =
        snapshot.relationships.blocked_by.iter().copied().collect();

    for dependency in dependencies {
        match vcs.read_issue(repo, dependency) {
            Ok(issue) if issue.state == IssueState::Closed => {}
            Ok(_) => return false,
            Err(error) => {
                tracing::warn!(
                    %repo,
                    issue = snapshot.number,
                    dependency,
                    %error,
                    "could not read a dependency issue; treating \
                     deps_merged as unsatisfied so this fact cannot read \
                     as satisfied on an unverifiable dependency (§8.1) \
                     if the current or next phase happens to require it"
                );
                return false;
            }
        }
    }

    true
}

/// `advance(issue_number)` (§6) — the manual/coordinator override of the
/// state machine: work out where this issue can go next and, when it can
/// move, write that status-label transition through to GitHub (§15).
///
/// §6 defines the tool as stopping "at a `human` gate", passing "an
/// `auto` gate", and blocking "on unmet `requires`". All three of those
/// judgments are [`crate::workflow::advance`]'s, unchanged: this
/// function reads the workflow and the issue, computes the two facts
/// that pure function cannot derive from an [`IssueSnapshot`] alone (see
/// [`crate::RequirementContext`]), calls it, and acts on exactly one of
/// its outcomes.
///
/// # Every non-`Advance` decision is a successful answer, not an error
///
/// Asking "can this issue move" and being told "no, its merge gate is
/// unapproved" is the tool working. So `NoCurrentPhase`, `UnknownPhase`,
/// `BlockedOnGate`, `BlockedOnRequires`, `NextPhaseNotReady`,
/// `NextPhaseGateBlocked` and `Done` all return `Ok` with the decision
/// and **no write at all** — the same stance `board`/`backlog` take on
/// an empty result. Mapping any of them to a [`ToolError`] would make a
/// coordinator's ordinary "what's next here?" poll look like a failure.
///
/// # `roborev_verdict` is always `None` — a real, spec-confirmed gap
///
/// §12 says plainly that "**roborev** (which it can't derive) is
/// `report`ed by an agent", and §6's `report` tool does not exist in
/// this server (see [`super`]'s module doc). Nothing else in this crate
/// carries a roborev verdict either: no label, comment convention, or
/// `Vcs` method outside `crate::workflow`'s own `Requirement::Roborev`
/// mentions one. So this passes `None`, and any phase whose `requires`
/// includes `{roborev: ...}` reports that requirement in the `unmet`
/// list of a [`AdvanceDecision::BlockedOnRequires`] or
/// [`AdvanceDecision::NextPhaseNotReady`] — surfaced, not silently
/// treated as satisfied ([`crate::requirement_satisfied`] compares the
/// verdict for equality, so `None` never matches).
///
/// The shipped default's `review` phase declares exactly that
/// requirement, so **on the shipped default this tool cannot currently
/// move an issue into `review` at all**, no matter how green CI is. That
/// is the honest state of the system until a `report` tool exists to
/// supply the verdict, not a bug in this function to work around.
///
/// # The target phase's action is NOT executed
///
/// [`AdvanceDecision::Advance`] here writes the status-label transition
/// and nothing else. No agent is spawned for an
/// [`PhaseAction::Agent`]/[`PhaseAction::ServerThenAgent`] target; no
/// merge is enqueued when the target is the merge gate (even though
/// [`crate::plan_merge`] exists as a pure function ready to be called);
/// no worktree, branch, label pruning or issue close happens when the
/// target is `finalize`. This is the same boundary `approve`'s
/// un-routed `deny` and the absent `next_assignment` already draw: no
/// phase engine or spawner runs alongside this server.
///
/// It is drawn *uniformly* on purpose. Enqueueing a merge mechanically —
/// the one target action this crate already has a pure decision function
/// for — while leaving every other phase's action unexecuted would be a
/// partial execution model in which some phases run and some do not,
/// with nothing in the tool surface saying which. A merge-shaped
/// exception is also the worst possible place to start, since §2.3b's
/// one safety invariant is that nothing merges to the default branch by
/// accident. One clean line ("this server moves labels; the phase engine
/// runs phases") is both safer and easier to remove later than a
/// carve-out. [`super::wire::AdvanceResult::action_executed`] states
/// this in the result rather than leaving a client to infer it from a
/// status label that moved.
///
/// # The label transition, and why it is written in that order
///
/// Add the target's `status:<label>` first, then remove the issue's
/// previous one. If the second write fails, the issue is left carrying
/// **two** `status:*` labels — which
/// [`find_ambiguous_labels`] already reports as drift, so a half-applied
/// transition surfaces in `drift()` (§15) instead of hiding. The reverse
/// order fails worse: an issue left with *no* status label reads as
/// `NoCurrentPhase` to every instance, i.e. as work that never started,
/// and [`crate::workflow::advance`] cannot move it again without a human
/// re-stamping a status by hand.
///
/// Both writes go through [`Vcs::set_label`], which needs the label to
/// already exist in the repo; `spec-flow init` provisions every
/// `status:<label>` a workflow declares
/// ([`crate::workflow::label_vocabulary`], which reads both
/// `labels.status` and every phase's `status_label`), so this succeeds
/// for any project whose workflow has not gained a status since its last
/// `init` — the already-recorded gap in that provisioning, not a new one.
///
/// # A target phase with no `status_label`
///
/// [`Phase::status_label`] is optional. Advancing into a phase that
/// declares none writes **nothing at all** — not even the removal of the
/// current status — and reports the `Advance` decision with both
/// [`AdvanceOutcome::status_added`] and
/// [`AdvanceOutcome::status_removed`] `None`. §2.3's state table makes
/// the `status:*` label the *only* GitHub representation of workflow
/// position, so removing the old label without writing a new one would
/// erase the issue's position from the single source of truth and strand
/// it at `NoCurrentPhase`. Leaving the old label in place is not
/// correct either — it says the issue is still on the previous phase —
/// but it is recoverable, and it is what the decision itself reports.
/// The shipped default declares a `status_label` on every phase, so this
/// case needs a team-edited workflow to reach.
///
/// # No comment is posted
///
/// Unlike [`approve_issue`], which records a human *decision* a label
/// cannot always carry, an advance is fully expressed by the two labels
/// it writes — the same reason [`set_gate_for_issue`] posts none. The
/// deferral above (that the target phase's action did not run) is
/// reported to the caller in the result; commenting it on the issue for
/// every phase transition would put a running commentary on every issue
/// the eventual phase engine advances, for a fact that is about this
/// server's current shape rather than about the issue.
///
/// # Cost
///
/// [`read_issue_state`]'s usual three-to-four `gh` invocations for the
/// read the decision is computed from (see [`read_board`]'s "Cost"
/// section for what those are), plus one per distinct `blockedBy` target
/// ([`dependencies_satisfied`]), plus one local file read for the
/// workflow ([`load_workflow`]). An advancing call adds one or two `gh
/// issue edit` calls and a second full issue re-read. A single-issue
/// call with no open-issue fan-out — a reasoned count, not a measured
/// one, unlike `board`/`backlog`/`drift`'s.
pub fn advance_issue<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    issue_number: u64,
) -> Result<AdvanceOutcome, ToolError> {
    // The workflow first, so a project whose `.spec-flow/workflow.yaml`
    // is missing or malformed fails before any `gh` call — the same
    // ordering `approve`/`set_gate` use, and the reason a broken config
    // can never leave a half-written transition on an issue.
    let workflow = load_workflow(project)?;
    let (snapshot, url) = read_issue(vcs, project, issue_number)?;

    let decision = crate::workflow::advance(
        &workflow,
        &snapshot,
        dependencies_satisfied(vcs, &project.repo, &snapshot),
        // Always `None` — see this function's roborev section.
        None,
    );

    // Cloned out of the decision rather than matched by value, so the
    // decision itself is still owned and reported below whichever way
    // this goes.
    let target_status = match &decision {
        AdvanceDecision::Advance { to } => to.status_label.clone(),
        _ => {
            return Ok(AdvanceOutcome {
                decision,
                status_added: None,
                status_removed: None,
                snapshot,
                url,
            });
        }
    };
    let Some(target_status) = target_status else {
        return Ok(AdvanceOutcome {
            decision,
            status_added: None,
            status_removed: None,
            snapshot,
            url,
        });
    };

    let added = format!("{STATUS_PREFIX}{target_status}");
    vcs.set_label(&project.repo, issue_number, &added, true)?;

    // Guarded against removing the label just written: two phases in one
    // workflow may share a `status_label` (nothing forbids it), in which
    // case the "previous" status and the target's are the same string
    // and the removal would undo the add, leaving the issue with no
    // status at all.
    let status_removed = match snapshot.status.as_deref() {
        Some(current) if current != target_status => {
            let removed = format!("{STATUS_PREFIX}{current}");
            vcs.set_label(&project.repo, issue_number, &removed, false)?;
            Some(removed)
        }
        _ => None,
    };

    let (snapshot, url) = read_issue(vcs, project, issue_number)?;
    Ok(AdvanceOutcome {
        decision,
        status_added: Some(added),
        status_removed,
        snapshot,
        url,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::config::{
        Binaries, ClaimConfig, CrossProjectMode, GhConfig, HarnessesConfig,
        Limits, MergeMode, save_project_config,
    };
    use crate::vcs::{FakeVcs, IssueState};
    use crate::workflow::Requirement;

    fn global_config(projects: Vec<ProjectPointer>) -> GlobalConfig {
        GlobalConfig {
            listen: "127.0.0.1:7420".parse().unwrap(),
            instance_id: None,
            binaries: Binaries::default(),
            harnesses: HarnessesConfig {
                default: "claude".to_string(),
                harnesses: HashMap::new(),
            },
            limits: Limits { max_concurrent_agents: 3 },
            cross_project_mode: CrossProjectMode::FairShare,
            claim: ClaimConfig {
                heartbeat_ttl: Duration::from_secs(3600),
                heartbeat_interval: Duration::from_secs(300),
            },
            phase_timeout: Duration::from_secs(45 * 60),
            projects,
        }
    }

    fn project_config(name: &str, repo: &str, dir: &Path) -> ProjectConfig {
        ProjectConfig {
            name: name.to_string(),
            repo: repo.to_string(),
            project_dir: dir.to_path_buf(),
            local_path: dir.join("main"),
            default_branch: "main".to_string(),
            merge_mode: MergeMode::Native,
            gh: None,
            harness: None,
            index_path: dir.join(".index"),
            memory_scope: name.to_string(),
            weight: None,
        }
    }

    /// Write a real `<project_dir>/config.yaml` and return the pointer
    /// the global registry would hold for it.
    fn register_on_disk(dir: &Path, name: &str, repo: &str) -> ProjectPointer {
        let config = project_config(name, repo, dir);
        save_project_config(&project_config_path(dir), &config).unwrap();
        ProjectPointer {
            name: name.to_string(),
            project_dir: dir.to_path_buf(),
        }
    }

    // -- resolve_project --

    #[test]
    fn resolve_project_finds_the_project_the_cwd_sits_directly_in() {
        let tmp = tempfile::tempdir().unwrap();
        let pointer = register_on_disk(tmp.path(), "proj-a", "owner/repo-a");
        let global = global_config(vec![pointer]);

        let (found, config) =
            resolve_project(&global, tmp.path().to_str().unwrap()).unwrap();

        assert_eq!(found.name, "proj-a");
        assert_eq!(config.repo, "owner/repo-a");
    }

    #[test]
    fn resolve_project_finds_the_project_from_a_worktree_inside_it() {
        // The realistic case (§10.1): a coordinator is opened in
        // `<project_dir>/<branch>`, never in `<project_dir>` itself.
        let tmp = tempfile::tempdir().unwrap();
        let pointer = register_on_disk(tmp.path(), "proj-a", "owner/repo-a");
        let global = global_config(vec![pointer]);
        let worktree = tmp.path().join("issue-42-widget-cache");

        let (found, _) =
            resolve_project(&global, worktree.to_str().unwrap()).unwrap();

        assert_eq!(found.name, "proj-a");
    }

    #[test]
    fn resolve_project_picks_the_containing_project_among_several() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let global = global_config(vec![
            register_on_disk(&a, "proj-a", "owner/repo-a"),
            register_on_disk(&b, "proj-b", "owner/repo-b"),
        ]);

        let (found, config) =
            resolve_project(&global, b.to_str().unwrap()).unwrap();

        assert_eq!(found.name, "proj-b");
        assert_eq!(config.repo, "owner/repo-b");
    }

    #[test]
    fn resolve_project_rejects_a_directory_outside_every_project() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("a");
        std::fs::create_dir_all(&project).unwrap();
        let global =
            global_config(vec![register_on_disk(&project, "a", "o/a")]);
        let elsewhere = tmp.path().join("not-a-project");

        assert!(matches!(
            resolve_project(&global, elsewhere.to_str().unwrap()),
            Err(ToolError::UnregisteredDirectory { .. })
        ));
    }

    #[test]
    fn resolve_project_rejects_an_empty_registry() {
        let global = global_config(Vec::new());

        assert!(matches!(
            resolve_project(&global, "/anywhere"),
            Err(ToolError::UnregisteredDirectory { .. })
        ));
    }

    #[test]
    fn resolve_project_rejects_a_relative_cwd() {
        let global = global_config(Vec::new());

        assert!(matches!(
            resolve_project(&global, "some/relative/dir"),
            Err(ToolError::CwdNotAbsolute { .. })
        ));
    }

    #[test]
    fn resolve_project_reports_a_registered_project_with_no_config_file() {
        // The registry is a pointer list (§11.1); a pointer whose
        // `<project_dir>/config.yaml` was deleted must surface as a
        // config error, not as "this directory isn't registered" --
        // those two send the operator to completely different fixes.
        let tmp = tempfile::tempdir().unwrap();
        let global = global_config(vec![ProjectPointer {
            name: "proj-a".to_string(),
            project_dir: tmp.path().to_path_buf(),
        }]);

        assert!(matches!(
            resolve_project(&global, tmp.path().to_str().unwrap()),
            Err(ToolError::Config(_))
        ));
    }

    // -- read_board --

    #[test]
    fn read_board_is_empty_for_a_repo_with_no_open_issues() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, truncated) = read_board(&vcs, &project, 100).unwrap();

        assert!(rows.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn read_board_returns_one_row_per_open_issue_sorted_by_number() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "First", "").unwrap();
        vcs.create_issue("owner/repo-a", "Second", "").unwrap();
        vcs.create_issue("owner/repo-a", "Third", "").unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        let numbers: Vec<u64> = rows.iter().map(|row| row.number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
        assert_eq!(rows[1].title, "Second");
    }

    #[test]
    fn read_board_carries_derived_label_state_onto_the_rows() {
        let vcs = FakeVcs::new();
        let issue =
            vcs.create_issue("owner/repo-a", "Widget cache", "").unwrap();
        vcs.set_label("owner/repo-a", issue.number, "status:implement", true)
            .unwrap();
        vcs.set_label("owner/repo-a", issue.number, "P1", true).unwrap();
        vcs.set_label("owner/repo-a", issue.number, "gate:review", true)
            .unwrap();
        vcs.set_label(
            "owner/repo-a",
            issue.number,
            "approved:test-plan",
            true,
        )
        .unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        assert_eq!(rows[0].status.as_deref(), Some("implement"));
        assert_eq!(rows[0].gates, vec!["review".to_string()]);
        assert_eq!(rows[0].approvals, vec!["test-plan".to_string()]);
    }

    #[test]
    fn read_board_omits_closed_issues() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "Open", "").unwrap();
        let closed = vcs.create_issue("owner/repo-a", "Closed", "").unwrap();
        vcs.issues
            .borrow_mut()
            .get_mut(&("owner/repo-a".to_string(), closed.number))
            .unwrap()
            .state = IssueState::Closed;
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Open");
    }

    #[test]
    fn read_board_only_reports_the_bound_projects_repo() {
        // §15's "work is identified by (project, issue_number)": a board
        // that leaked another repo's issues would break the whole
        // project-rides-the-connection guarantee.
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "On a", "").unwrap();
        vcs.create_issue("owner/repo-b", "On b", "").unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "On a");
    }

    #[test]
    fn read_board_reports_truncation_when_the_repo_fills_the_limit() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "One", "").unwrap();
        vcs.create_issue("owner/repo-a", "Two", "").unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, truncated) = read_board(&vcs, &project, 2).unwrap();

        assert_eq!(rows.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn read_board_uses_the_projects_gh_host_for_issue_urls() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "One", "").unwrap();
        let mut project =
            project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        project.gh = Some(GhConfig {
            host: "github.example.com".to_string(),
            account: "jon".to_string(),
        });

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        assert_eq!(
            rows[0].url,
            "https://github.example.com/owner/repo-a/issues/1"
        );
    }

    // -- read_issue --

    #[test]
    fn read_issue_returns_the_snapshot_and_its_github_url() {
        let vcs = FakeVcs::new();
        let issue =
            vcs.create_issue("owner/repo-a", "Widget cache", "").unwrap();
        vcs.set_label("owner/repo-a", issue.number, "status:groom", true)
            .unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (snapshot, url) =
            read_issue(&vcs, &project, issue.number).unwrap();

        assert_eq!(snapshot.number, issue.number);
        assert_eq!(snapshot.title, "Widget cache");
        assert_eq!(snapshot.status.as_deref(), Some("groom"));
        assert_eq!(url, "https://github.com/owner/repo-a/issues/1");
    }

    #[test]
    fn read_issue_reports_an_unknown_issue_as_a_vcs_error() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        assert!(matches!(
            read_issue(&vcs, &project, 404),
            Err(ToolError::Vcs(_))
        ));
    }

    // -- read_backlog --

    /// Materialize the shipped default `.spec-flow/workflow.yaml` where
    /// [`load_workflow`] looks for it — under the project's **primary
    /// checkout**, exactly as `spec-flow init` scaffolds it.
    fn scaffold_workflow(project: &ProjectConfig) {
        let path = workflow_path(project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, crate::scaffold::DEFAULT_WORKFLOW_YAML).unwrap();
    }

    /// A project rooted at `dir` whose workflow file is already in place.
    fn project_with_workflow(dir: &Path) -> ProjectConfig {
        let project = project_config("a", "owner/repo-a", dir);
        scaffold_workflow(&project);
        project
    }

    #[test]
    fn read_backlog_reports_a_project_with_no_workflow_file() {
        // The readiness bar is *defined* by this file (§7.0), so a
        // project whose checkout is missing it must fail loudly rather
        // than be answered from the shipped default.
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", tmp.path());

        assert!(matches!(
            read_backlog(&vcs, &project, 100),
            Err(ToolError::ReadWorkflow { .. })
        ));
    }

    #[test]
    fn read_backlog_reports_a_malformed_workflow_file() {
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", tmp.path());
        let path = workflow_path(&project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "phases: [not-a-phase-map\n").unwrap();

        assert!(matches!(
            read_backlog(&vcs, &project, 100),
            Err(ToolError::Workflow { .. })
        ));
    }

    #[test]
    fn read_backlog_keeps_only_the_issues_before_the_ready_seam() {
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        let project = project_with_workflow(tmp.path());
        for (title, status) in [
            ("Grooming", Some("product-spec")),
            ("Handed off", Some("ready")),
            ("In flight", Some("implement")),
            ("Untouched", None),
        ] {
            let issue = vcs.create_issue("owner/repo-a", title, "").unwrap();
            if let Some(status) = status {
                vcs.set_label(
                    "owner/repo-a",
                    issue.number,
                    &format!("status:{status}"),
                    true,
                )
                .unwrap();
            }
        }

        let (rows, _) = read_backlog(&vcs, &project, 100).unwrap();

        let titles: Vec<&str> =
            rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, vec!["Grooming", "Untouched"]);
    }

    #[test]
    fn read_backlog_annotates_each_row_with_its_readiness_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        let project = project_with_workflow(tmp.path());
        let issue =
            vcs.create_issue("owner/repo-a", "Widget cache", "").unwrap();
        vcs.set_label(
            "owner/repo-a",
            issue.number,
            "status:architecture",
            true,
        )
        .unwrap();
        vcs.set_label(
            "owner/repo-a",
            issue.number,
            "approved:product-spec",
            true,
        )
        .unwrap();

        let (rows, _) = read_backlog(&vcs, &project, 100).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].readiness_gap,
            vec!["architecture".to_string(), "test-plan".to_string()]
        );
        assert_eq!(rows[0].url, "https://github.com/owner/repo-a/issues/1");
    }

    #[test]
    fn read_backlog_orders_the_worklist_by_priority() {
        // §2.7's "surface next by priority" -- the ordering the
        // coordinator's queue-filling loop actually consumes.
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        let project = project_with_workflow(tmp.path());
        for priority in ["P2", "P0", "P1"] {
            let issue =
                vcs.create_issue("owner/repo-a", priority, "").unwrap();
            vcs.set_label("owner/repo-a", issue.number, priority, true)
                .unwrap();
        }

        let (rows, _) = read_backlog(&vcs, &project, 100).unwrap();

        let titles: Vec<&str> =
            rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, vec!["P0", "P1", "P2"]);
    }

    #[test]
    fn read_backlog_reports_truncation_of_the_open_issue_read() {
        // Truncation is a property of the open-issue read, not of the
        // filtered rows -- a short backlog can still be truncated, and
        // §2.7's coordinator would otherwise read it as "no prepared
        // work left."
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        let project = project_with_workflow(tmp.path());
        vcs.create_issue("owner/repo-a", "One", "").unwrap();
        let ready = vcs.create_issue("owner/repo-a", "Two", "").unwrap();
        vcs.set_label("owner/repo-a", ready.number, "status:ready", true)
            .unwrap();

        let (rows, truncated) = read_backlog(&vcs, &project, 2).unwrap();

        assert_eq!(rows.len(), 1);
        assert!(truncated);
    }

    #[test]
    fn read_backlog_only_reports_the_bound_projects_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        let project = project_with_workflow(tmp.path());
        vcs.create_issue("owner/repo-a", "On a", "").unwrap();
        vcs.create_issue("owner/repo-b", "On b", "").unwrap();

        let (rows, _) = read_backlog(&vcs, &project, 100).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "On a");
    }

    // -- read_drift --

    /// A fixed "now" well past the epochs the claim fixtures below use,
    /// so staleness never depends on the wall clock.
    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(10_000)
    }

    fn ttl() -> Duration {
        Duration::from_secs(3_600)
    }

    /// Seed one issue verbatim — the only way to control an issue's raw
    /// **label order** and to seed the same prefix twice, which
    /// `set_label` (a set) cannot express and which
    /// `find_ambiguous_labels` reads literally.
    fn seed_issue(
        vcs: &FakeVcs,
        number: u64,
        labels: &[&str],
        state: IssueState,
    ) {
        vcs.issues.borrow_mut().insert(
            ("owner/repo-a".to_string(), number),
            crate::vcs::IssueRef {
                number,
                title: format!("Issue {number}"),
                body: String::new(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                state,
            },
        );
    }

    fn seed_blocked_by(vcs: &FakeVcs, issue: u64, blocked_by: &[u64]) {
        vcs.relationships.borrow_mut().insert(
            ("owner/repo-a".to_string(), issue),
            IssueRelationships {
                blocked_by: blocked_by.to_vec(),
                ..Default::default()
            },
        );
    }

    fn seed_linked_pr(
        vcs: &FakeVcs,
        issue: u64,
        prs: &[(u64, crate::vcs::PullRequestState)],
    ) {
        vcs.linked_pull_requests.borrow_mut().insert(
            ("owner/repo-a".to_string(), issue),
            prs.iter()
                .map(|(number, _)| crate::vcs::PullRequestRef {
                    number: *number,
                    url: format!(
                        "https://github.com/owner/repo-a/pull/{number}"
                    ),
                    branch: format!("issue-{issue}-work"),
                })
                .collect(),
        );
        for (number, state) in prs {
            vcs.pull_request_statuses.borrow_mut().insert(
                ("owner/repo-a".to_string(), *number),
                crate::vcs::PullRequestStatus {
                    number: *number,
                    state: *state,
                    ci: crate::vcs::CiConclusion::Success,
                    in_merge_queue: false,
                },
            );
        }
    }

    #[test]
    fn read_drift_is_clean_for_a_repo_with_no_open_issues() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert!(report.findings.is_empty());
        assert!(report.claims.is_empty());
        assert!(!report.truncated);
    }

    #[test]
    fn read_drift_is_clean_for_a_healthy_repo() {
        // The negative that matters: an ordinary in-flight issue with a
        // fresh claim, one open PR, and an open dependency must produce
        // *nothing*. A drift report that cries wolf is unusable.
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &["status:implement"], IssueState::Open);
        seed_issue(
            &vcs,
            2,
            &["status:implement", "owner:instance-a@9999"],
            IssueState::Open,
        );
        seed_blocked_by(&vcs, 2, &[1]);
        seed_linked_pr(&vcs, 2, &[(7, crate::vcs::PullRequestState::Open)]);

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert_eq!(report.claims.len(), 1);
        assert!(!report.claims[0].stale);
    }

    #[test]
    fn read_drift_reads_an_off_board_dependency_to_tell_closed_from_missing() {
        // The design question this whole orchestration turns on: #9 is
        // CLOSED, so it never appears in `list_open_issues`. Scoping the
        // scan to the open set alone (what `board` does) would report
        // this as `DependencyOnMissingIssue` -- a different fact, with a
        // different fix -- instead of the closed-dependency drift §8.4
        // actually names.
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &[], IssueState::Open);
        seed_issue(&vcs, 9, &[], IssueState::Closed);
        seed_blocked_by(&vcs, 1, &[9]);

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert_eq!(
            report.findings,
            vec![DriftFinding::DependencyOnClosedIssue {
                issue: 1,
                depends_on: 9
            }]
        );
    }

    #[test]
    fn read_drift_reports_a_dependency_it_cannot_read_at_all_as_missing() {
        // #9 was deleted or transferred: `gh` exits non-zero and this
        // fake has nothing seeded for it. See `read_drift`'s doc for why
        // that is reported as drift rather than failing the whole call,
        // and for the transient-failure conflation it accepts.
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &[], IssueState::Open);
        seed_blocked_by(&vcs, 1, &[9]);

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert_eq!(
            report.findings,
            vec![DriftFinding::DependencyOnMissingIssue {
                issue: 1,
                depends_on: 9
            }]
        );
    }

    #[test]
    fn read_drift_does_not_flag_a_dependency_on_an_ordinary_open_issue() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &[], IssueState::Open);
        seed_issue(&vcs, 2, &[], IssueState::Open);
        seed_blocked_by(&vcs, 1, &[2]);

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn read_drift_finds_a_dependency_cycle_among_open_issues() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &[], IssueState::Open);
        seed_issue(&vcs, 2, &[], IssueState::Open);
        seed_blocked_by(&vcs, 1, &[2]);
        seed_blocked_by(&vcs, 2, &[1]);

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert!(
            report.findings.contains(&DriftFinding::DependencyCycle {
                cycle: vec![1, 2]
            })
        );
    }

    #[test]
    fn read_drift_does_not_detect_a_cycle_that_passes_through_a_closed_issue()
    {
        // #2 is CLOSED, so it never appears in `list_open_issues` and its
        // own `blockedBy` edge back to #1 is never read -- only the open
        // set's relationships feed `find_dependency_cycles` (see
        // `read_drift`'s doc). The edge *into* #2 is still real drift,
        // just a different kind: `DependencyOnClosedIssue`, not a cycle.
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &[], IssueState::Open);
        seed_issue(&vcs, 2, &[], IssueState::Closed);
        seed_blocked_by(&vcs, 1, &[2]);
        seed_blocked_by(&vcs, 2, &[1]);

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert_eq!(
            report.findings,
            vec![DriftFinding::DependencyOnClosedIssue {
                issue: 1,
                depends_on: 2
            }]
        );
    }

    #[test]
    fn read_drift_flags_a_stale_claim_and_marks_its_holder_stale() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &["owner:instance-a@0"], IssueState::Open);

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert_eq!(
            report.findings,
            vec![DriftFinding::StaleClaim {
                issue: 1,
                instance: "instance-a".to_string(),
                epoch: 0,
            }]
        );
        // The contention half, cross-referenced from the same finding
        // rather than re-derived -- see `ClaimHolder`'s doc.
        assert_eq!(report.claims.len(), 1);
        assert_eq!(report.claims[0].issue, 1);
        assert_eq!(report.claims[0].claim.instance, "instance-a");
        assert!(report.claims[0].stale);
    }

    #[test]
    fn read_drift_flags_a_merged_pr_against_a_still_open_issue() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &[], IssueState::Open);
        seed_linked_pr(&vcs, 1, &[(7, crate::vcs::PullRequestState::Merged)]);

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert_eq!(
            report.findings,
            vec![DriftFinding::MergedPrWithOpenIssue {
                issue: 1,
                pull_request: 7
            }]
        );
    }

    #[test]
    fn read_drift_flags_two_status_labels_on_one_issue() {
        // The check that needs the issue's RAW labels -- `IssueSnapshot`
        // has already collapsed these two into one `status`. See
        // `read_issue_with_labels`' doc.
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(
            &vcs,
            1,
            &["status:review", "status:implement"],
            IssueState::Open,
        );

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert_eq!(
            report.findings,
            vec![DriftFinding::AmbiguousLabels {
                issue: 1,
                prefix: "status:",
                labels: vec![
                    "status:review".to_string(),
                    "status:implement".to_string(),
                ],
            }]
        );
    }

    #[test]
    fn read_drift_flags_two_simultaneously_open_linked_pull_requests() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &[], IssueState::Open);
        seed_linked_pr(
            &vcs,
            1,
            &[
                (7, crate::vcs::PullRequestState::Open),
                (9, crate::vcs::PullRequestState::Open),
            ],
        );

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert_eq!(
            report.findings,
            vec![DriftFinding::MultipleOpenLinkedPullRequests {
                issue: 1,
                pull_requests: vec![7, 9],
            }]
        );
    }

    #[test]
    fn read_drift_reports_truncation_when_the_repo_fills_the_limit() {
        // A truncated clean report is not a clean repo (§15).
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &[], IssueState::Open);
        seed_issue(&vcs, 2, &[], IssueState::Open);

        let report = read_drift(&vcs, &project, 2, ttl(), now()).unwrap();

        assert!(report.truncated);
    }

    #[test]
    fn read_drift_only_scans_the_bound_projects_repo() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        seed_issue(&vcs, 1, &[], IssueState::Open);
        vcs.issues.borrow_mut().insert(
            ("owner/repo-b".to_string(), 2),
            crate::vcs::IssueRef {
                number: 2,
                title: "On b".to_string(),
                body: String::new(),
                labels: vec!["owner:instance-b@0".to_string()],
                state: IssueState::Open,
            },
        );

        let report = read_drift(&vcs, &project, 100, ttl(), now()).unwrap();

        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert!(report.claims.is_empty());
    }

    #[test]
    fn read_issue_reads_a_closed_issue_rather_than_hiding_it() {
        // Unlike `board` (open issues only), `issue(number)` is an
        // explicit lookup -- a closed issue is a legitimate answer, and
        // §15's 1:1:1:1 drift check needs to be able to ask about one.
        let vcs = FakeVcs::new();
        let issue = vcs.create_issue("owner/repo-a", "Done", "").unwrap();
        vcs.issues
            .borrow_mut()
            .get_mut(&("owner/repo-a".to_string(), issue.number))
            .unwrap()
            .state = IssueState::Closed;
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (snapshot, _) = read_issue(&vcs, &project, issue.number).unwrap();

        assert_eq!(snapshot.state, IssueState::Closed);
    }

    // -- approve / set_gate --

    const REPO: &str = "owner/repo-a";

    /// One open issue (number 1) in the fake's `REPO`, plus a project
    /// whose primary checkout carries the shipped default workflow.
    fn issue_and_project(dir: &Path) -> (FakeVcs, ProjectConfig) {
        let vcs = FakeVcs::new();
        vcs.create_issue(REPO, "Widget cache", "").unwrap();
        (vcs, project_with_workflow(dir))
    }

    /// Every comment posted on issue 1 of `REPO`.
    fn comments(vcs: &FakeVcs) -> Vec<String> {
        vcs.comments
            .borrow()
            .get(&(REPO.to_string(), 1))
            .cloned()
            .unwrap_or_default()
    }

    /// Issue 1's raw labels.
    fn labels(vcs: &FakeVcs) -> Vec<String> {
        vcs.issues.borrow()[&(REPO.to_string(), 1)].labels.clone()
    }

    #[test]
    fn approve_grant_writes_the_approval_label_and_returns_fresh_state() {
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        let outcome = approve_issue(
            &vcs,
            &project,
            1,
            "product-spec",
            ApproveDecision::Grant,
            None,
        )
        .unwrap();

        assert_eq!(
            outcome.approval_label.as_deref(),
            Some("approved:product-spec")
        );
        assert!(labels(&vcs).contains(&"approved:product-spec".to_string()));
        // The returned state is re-read from GitHub, not assumed (§2.3).
        assert_eq!(outcome.snapshot.approvals, vec!["product-spec"]);
        assert_eq!(outcome.url, "https://github.com/owner/repo-a/issues/1");
        assert!(outcome.reentry.is_none(), "a grant routes nowhere");
    }

    #[test]
    fn approve_grant_leaves_a_comment_naming_the_label_it_wrote() {
        // §2.3b's dual surface: an operator watching the issue in
        // GitHub sees why the label appeared, not just that it did.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        approve_issue(
            &vcs,
            &project,
            1,
            "architecture",
            ApproveDecision::Grant,
            Some("looks right"),
        )
        .unwrap();

        let comments = comments(&vcs);
        assert_eq!(comments.len(), 1);
        assert!(comments[0].contains("Gate approved"));
        assert!(comments[0].contains("approved:architecture"));
        assert!(comments[0].contains("looks right"));
    }

    #[test]
    fn approve_grant_on_a_phase_with_no_approval_label_records_it_anyway() {
        // `conflict-check` declares no `approval_label` at all, so
        // `workflow::approve` has no label to write -- and the comment
        // is then the ONLY durable record of the decision.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        let outcome = approve_issue(
            &vcs,
            &project,
            1,
            "conflict-check",
            ApproveDecision::Grant,
            None,
        )
        .unwrap();

        assert!(outcome.approval_label.is_none());
        assert!(labels(&vcs).is_empty(), "{:?}", labels(&vcs));
        assert_eq!(comments(&vcs).len(), 1);
        assert!(comments(&vcs)[0].contains("no approval label"));
    }

    #[test]
    fn approve_grant_twice_leaves_one_approval() {
        // The idempotent retry §2.3b's two surfaces make ordinary: a
        // hand-applied label followed by an `approve` call, or simply a
        // client retrying.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        for _ in 0..2 {
            approve_issue(
                &vcs,
                &project,
                1,
                "test-plan",
                ApproveDecision::Grant,
                None,
            )
            .unwrap();
        }

        assert_eq!(
            labels(&vcs),
            vec!["approved:test-plan".to_string()],
            "the label set must not grow on a retry"
        );
    }

    #[test]
    fn approve_deny_writes_no_label_and_names_the_self_reentry() {
        // §6's architecture→re-propose: denying an agent phase means
        // running that same phase again.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        let outcome = approve_issue(
            &vcs,
            &project,
            1,
            "architecture",
            ApproveDecision::Deny,
            None,
        )
        .unwrap();

        assert!(outcome.approval_label.is_none());
        assert!(labels(&vcs).is_empty(), "a denial writes no label");
        assert_eq!(outcome.reentry.as_deref(), Some("architecture"));
        assert!(outcome.snapshot.approvals.is_empty());

        let comments = comments(&vcs);
        assert_eq!(comments.len(), 1);
        assert!(comments[0].contains("Gate denied"));
        assert!(comments[0].contains("re-enters `architecture`"));
        assert!(comments[0].contains("the same phase, run again"));
        // The half §6 asks for that this does NOT do, stated on the
        // issue itself rather than only in a doc comment.
        assert!(comments[0].contains("did **not** re-run"));
    }

    #[test]
    fn approve_deny_records_the_note_on_the_issue() {
        // §6's "deny records the state + note" -- and it has to be
        // durable, since another instance reads only GitHub (§2.3).
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        approve_issue(
            &vcs,
            &project,
            1,
            "product-spec",
            ApproveDecision::Deny,
            Some("the acceptance criteria contradict issue #7"),
        )
        .unwrap();

        assert!(
            comments(&vcs)[0]
                .contains("the acceptance criteria contradict issue #7")
        );
    }

    #[test]
    fn approve_deny_on_the_merge_gate_names_the_address_reentry() {
        // §6's merge→address, derived structurally rather than from a
        // table of phase ids: the shipped default's merge gate is the
        // phase `review` (`{type: server, op: merge_queue}`), and
        // `address` is the out-of-band entry declaring `reenters:
        // review`.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        let outcome = approve_issue(
            &vcs,
            &project,
            1,
            "review",
            ApproveDecision::Deny,
            Some("two unresolved review threads"),
        )
        .unwrap();

        assert_eq!(outcome.reentry.as_deref(), Some("address"));
        assert!(comments(&vcs)[0].contains("re-enters `address`"));
        assert!(labels(&vcs).is_empty());

        // The wire contract, not just the domain type: a client reading
        // a populated `reentry` alongside `reentry_triggered` must never
        // see `true` -- that would be this server claiming it fired an
        // agent it cannot spawn. Checked on this exact case (a populated
        // `reentry`) because that is the one a bug could plausibly flip.
        let wire = super::super::wire::ApproveResult::from_outcome(
            &outcome, "proj-a", REPO,
        );
        assert!(!wire.reentry_triggered);
        assert_eq!(wire.reentry.as_deref(), Some("address"));
    }

    #[test]
    fn approve_deny_on_a_non_merge_server_phase_names_no_reentry() {
        // `finalize` is `{type: server, op: finalize}` -- not the merge
        // gate, not an agent phase. §6 defines no re-entry for it, and
        // this reports none rather than inventing "re-run it".
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        let outcome = approve_issue(
            &vcs,
            &project,
            1,
            "finalize",
            ApproveDecision::Deny,
            None,
        )
        .unwrap();

        assert!(outcome.reentry.is_none());
        // The decision is still recorded: losing it because the
        // workflow has nowhere to route would be the worse failure.
        assert_eq!(comments(&vcs).len(), 1);
        assert!(comments(&vcs)[0].contains("defines none"));
    }

    #[test]
    fn approve_deny_on_a_merge_gate_with_no_out_of_band_entry_reports_none() {
        // The other way to reach "no defined re-entry": a team-edited
        // workflow whose merge gate has no out-of-band phase re-entering
        // it. The shipped default cannot produce this.
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        vcs.create_issue(REPO, "Widget cache", "").unwrap();
        let project = project_config("a", REPO, tmp.path());
        let path = workflow_path(&project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "\
labels:
  priority: [P0]
  status: [review]
  gate_prefix: \"gate:\"
  approval_prefix: \"approved:\"
  owner_prefix: \"owner:\"
spec: {tool: openspec, optional: true, skip_label: \"spec:skip\"}
review_panel: []
fix_loop: {max_rounds: 1, on_exhausted: escalate, on_decision_finding: escalate}
phases:
  - {id: review, action: {type: server, op: merge_queue}, gate: human, approval_label: \"approved:merge\", status_label: review}
",
        )
        .unwrap();

        let outcome = approve_issue(
            &vcs,
            &project,
            1,
            "review",
            ApproveDecision::Deny,
            None,
        )
        .unwrap();

        assert!(outcome.reentry.is_none());
        assert!(comments(&vcs)[0].contains("defines none"));
    }

    #[test]
    fn approve_rejects_a_phase_the_workflow_does_not_define() {
        // `merge` is the approval *label*'s suffix, not a phase id --
        // the shipped default's merge gate is the phase `review`. The
        // rejection lists the ids that would have worked.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        let error = approve_issue(
            &vcs,
            &project,
            1,
            "merge",
            ApproveDecision::Grant,
            None,
        )
        .unwrap_err();

        match &error {
            ToolError::UnknownPhase { phase, known } => {
                assert_eq!(phase, "merge");
                assert!(known.contains(&"review".to_string()));
            }
            other => panic!("expected UnknownPhase, got {other:?}"),
        }
        // The argument is validated before anything is written: a
        // rejected call must leave no half-applied trace on the issue.
        assert!(labels(&vcs).is_empty());
        assert!(comments(&vcs).is_empty());
    }

    #[test]
    fn approve_reports_a_project_with_no_workflow_file() {
        // Same stance as `read_backlog`: the phase vocabulary is
        // *defined* by this file (§7.0), so a missing one fails loudly
        // rather than falling back to the shipped default.
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        vcs.create_issue(REPO, "Widget cache", "").unwrap();
        let project = project_config("a", REPO, tmp.path());

        assert!(matches!(
            approve_issue(
                &vcs,
                &project,
                1,
                "product-spec",
                ApproveDecision::Grant,
                None
            ),
            Err(ToolError::ReadWorkflow { .. })
        ));
    }

    #[test]
    fn set_gate_human_adds_the_per_issue_gate_label() {
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        let outcome = set_gate_for_issue(
            &vcs,
            &project,
            1,
            "architecture",
            GateMode::Human,
        )
        .unwrap();

        assert_eq!(outcome.label.label, "gate:architecture");
        assert!(outcome.label.present);
        assert_eq!(outcome.snapshot.gates, vec!["architecture"]);
        assert!(comments(&vcs).is_empty(), "set_gate posts no comment");
    }

    #[test]
    fn set_gate_auto_removes_the_gate_label() {
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        vcs.set_label(REPO, 1, "gate:architecture", true).unwrap();

        let outcome = set_gate_for_issue(
            &vcs,
            &project,
            1,
            "architecture",
            GateMode::Auto,
        )
        .unwrap();

        assert!(!outcome.label.present);
        assert!(outcome.snapshot.gates.is_empty());
        assert_eq!(outcome.effective_mode, GateMode::Auto);
    }

    #[test]
    fn set_gate_auto_on_the_merge_gate_removes_the_label_but_stays_human() {
        // §4.3's "merge is special": the shipped default's merge gate
        // (`review`) is configured `gate: human`, so removing
        // `gate:review` does NOT open it -- `effective_gate_mode` falls
        // back to `Human`, not `Auto`, off an absent label for this one
        // phase. `mode`/`label_present` alone would tell a client the
        // opposite of what is actually true; `effective_mode` exists so
        // that divergence is never silent.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        vcs.set_label(REPO, 1, "gate:review", true).unwrap();

        let outcome =
            set_gate_for_issue(&vcs, &project, 1, "review", GateMode::Auto)
                .unwrap();

        assert!(!outcome.label.present, "the label removal did happen");
        assert_eq!(
            outcome.effective_mode,
            GateMode::Human,
            "but the merge gate must not silently read as open"
        );
    }

    #[test]
    fn set_gate_none_removes_the_same_label_auto_does() {
        // The collapse `workflow::set_gate` documents: one binary label
        // cannot encode three modes, so `none` and `auto` are the same
        // write. Pinned here so the wire result's `label_present` can
        // never start implying otherwise.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        vcs.set_label(REPO, 1, "gate:architecture", true).unwrap();

        let outcome = set_gate_for_issue(
            &vcs,
            &project,
            1,
            "architecture",
            GateMode::None,
        )
        .unwrap();

        assert_eq!(outcome.label.label, "gate:architecture");
        assert!(!outcome.label.present);
        assert!(outcome.snapshot.gates.is_empty());
    }

    #[test]
    fn set_gate_is_idempotent_when_the_label_already_reflects_the_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        for _ in 0..2 {
            set_gate_for_issue(&vcs, &project, 1, "review", GateMode::Human)
                .unwrap();
        }

        assert_eq!(labels(&vcs), vec!["gate:review".to_string()]);
    }

    #[test]
    fn set_gate_rejects_a_phase_the_workflow_does_not_define() {
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        assert!(matches!(
            set_gate_for_issue(&vcs, &project, 1, "deploy", GateMode::Human),
            Err(ToolError::UnknownPhase { .. })
        ));
        assert!(labels(&vcs).is_empty());
    }

    // -- advance --

    /// Put issue 1 on `status:<status>` plus any extra labels, the way a
    /// realistic in-flight issue carries them.
    fn label_issue(vcs: &FakeVcs, status: Option<&str>, extra: &[&str]) {
        if let Some(status) = status {
            vcs.set_label(REPO, 1, &format!("status:{status}"), true).unwrap();
        }
        for label in extra {
            vcs.set_label(REPO, 1, label, true).unwrap();
        }
    }

    /// A green open PR linked to issue 1, so `{ci: green}` is satisfied.
    fn seed_green_pull_request(vcs: &FakeVcs) {
        seed_linked_pr(vcs, 1, &[(7, crate::vcs::PullRequestState::Open)]);
    }

    /// The three design-seam approvals §7.2 ph.5 requires before
    /// `implement`.
    const DESIGN_APPROVALS: &[&str] = &[
        "approved:product-spec",
        "approved:architecture",
        "approved:test-plan",
    ];

    /// Run `advance_issue` and assert it changed nothing on the issue —
    /// no label written either way, and no comment posted. Returns the
    /// outcome so the caller can assert on the decision itself.
    ///
    /// The labels are compared as a sorted list rather than in place, so
    /// this cannot pass merely because a write happened to be reordered.
    fn advance_expecting_no_write(
        vcs: &FakeVcs,
        project: &ProjectConfig,
    ) -> AdvanceOutcome {
        let mut before = labels(vcs);
        before.sort();
        let comments_before = comments(vcs).len();

        let outcome = advance_issue(vcs, project, 1).unwrap();

        let mut after = labels(vcs);
        after.sort();
        assert_eq!(after, before, "a blocked advance must write no label");
        assert_eq!(
            comments(vcs).len(),
            comments_before,
            "advance posts no comment at all"
        );
        assert!(outcome.status_added.is_none());
        assert!(outcome.status_removed.is_none());
        outcome
    }

    #[test]
    fn advance_writes_the_status_transition_and_returns_the_re_read_issue() {
        // The shipped default's first hop: an approved `product-spec`
        // advances to `conflict-check`.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("product-spec"), &["approved:product-spec"]);

        let outcome = advance_issue(&vcs, &project, 1).unwrap();

        assert!(matches!(
            &outcome.decision,
            AdvanceDecision::Advance { to } if to.id == "conflict-check"
        ));
        assert_eq!(
            outcome.status_added.as_deref(),
            Some("status:conflict-check")
        );
        assert_eq!(
            outcome.status_removed.as_deref(),
            Some("status:product-spec")
        );
        // The write really happened, and exactly one status label is
        // left -- the old one is gone, not merely joined by a new one.
        let labels = labels(&vcs);
        assert!(labels.contains(&"status:conflict-check".to_string()));
        assert!(!labels.contains(&"status:product-spec".to_string()));
        assert!(labels.contains(&"approved:product-spec".to_string()));
        // ...and the returned state is GitHub's answer, re-read after
        // the write rather than assumed from it (§2.3).
        assert_eq!(outcome.snapshot.status.as_deref(), Some("conflict-check"));
        assert_eq!(outcome.url, "https://github.com/owner/repo-a/issues/1");
        assert!(comments(&vcs).is_empty(), "advance posts no comment");
    }

    #[test]
    fn advance_moves_a_ready_issue_into_implement() {
        // §7.2 ph.5's handoff seam: `ready` is not a phase of its own,
        // so this exercises the alias resolution through the real write
        // path -- the `status:ready` label is what gets removed.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("ready"), DESIGN_APPROVALS);

        let outcome = advance_issue(&vcs, &project, 1).unwrap();

        assert!(matches!(
            &outcome.decision,
            AdvanceDecision::Advance { to } if to.id == "implement"
        ));
        assert_eq!(outcome.status_added.as_deref(), Some("status:implement"));
        assert_eq!(outcome.status_removed.as_deref(), Some("status:ready"));
        assert_eq!(outcome.snapshot.status.as_deref(), Some("implement"));
    }

    #[test]
    fn advance_reports_no_current_phase_and_writes_nothing() {
        // An issue still being groomed (§7.2 ph.1) carries no status
        // label; stamping an initial one is `create_issue`'s job.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(outcome.decision, AdvanceDecision::NoCurrentPhase);
        assert!(labels(&vcs).is_empty());
    }

    #[test]
    fn advance_reports_an_unknown_status_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("triage"), &[]);

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(
            outcome.decision,
            AdvanceDecision::UnknownPhase { status: "triage".to_string() }
        );
    }

    #[test]
    fn advance_stops_at_an_unapproved_human_gate_and_writes_nothing() {
        // §6's "stops at a `human` gate": `gate:architecture` is stamped
        // at issue creation (§4.3) and no `approved:architecture` has
        // been granted.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("architecture"), &["gate:architecture"]);

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(
            outcome.decision,
            AdvanceDecision::BlockedOnGate {
                phase: "architecture".to_string()
            }
        );
    }

    #[test]
    fn advance_blocks_on_the_current_phases_unmet_roborev_requirement() {
        // The roborev gap, surfaced rather than swallowed: everything
        // `review` requires is satisfied here EXCEPT `{roborev: clean}`
        // -- CI is green and the issue has no dependencies -- and it is
        // unmet only because this server has no `report` tool to have
        // learned a verdict from (§12). If `advance_issue` ever passed a
        // fabricated verdict, this list would come back empty and this
        // issue would be cleared to enqueue a merge.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("review"), &["approved:merge"]);
        seed_green_pull_request(&vcs);

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(
            outcome.decision,
            AdvanceDecision::BlockedOnRequires {
                phase: "review".to_string(),
                unmet: vec![Requirement::Roborev("clean".to_string())],
            }
        );
    }

    #[test]
    fn advance_cannot_enter_review_at_all_while_roborev_is_unreportable() {
        // The same gap seen from the phase before it, and the more
        // consequential half: an issue that has cleared `implement`
        // completely, with the merge gate already approved and CI green,
        // still cannot ENTER `review` -- whose action is the merge
        // enqueue -- because its `{roborev: clean}` requirement is
        // evaluated on entry and nothing can satisfy it today.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        let mut approvals = DESIGN_APPROVALS.to_vec();
        approvals.push("approved:merge");
        label_issue(&vcs, Some("implement"), &approvals);
        seed_green_pull_request(&vcs);

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(
            outcome.decision,
            AdvanceDecision::NextPhaseNotReady {
                phase: "review".to_string(),
                unmet: vec![Requirement::Roborev("clean".to_string())],
            }
        );
    }

    #[test]
    fn advance_does_not_enter_review_without_the_merge_approval() {
        // §4.3's "merge is special": `review`'s action runs on entry, so
        // its gate is checked BEFORE the transition, not after -- and
        // the gate check comes first, so this reports the gate rather
        // than the roborev requirement.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        let mut labels_to_set = DESIGN_APPROVALS.to_vec();
        labels_to_set.push("gate:review");
        label_issue(&vcs, Some("implement"), &labels_to_set);
        seed_green_pull_request(&vcs);

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(
            outcome.decision,
            AdvanceDecision::NextPhaseGateBlocked {
                phase: "review".to_string()
            }
        );
    }

    #[test]
    fn advance_reports_done_on_the_last_phase_and_writes_nothing() {
        // `finalize`'s status_label is `done`, not its id -- an issue at
        // `status:done` is on the last phase, with nothing after it.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("done"), &[]);

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(
            outcome.decision,
            AdvanceDecision::Done { phase: "finalize".to_string() }
        );
    }

    #[test]
    fn advance_treats_an_open_blocked_by_issue_as_unmerged() {
        // §8.1's dependency ordering: #2 is open, so `deps_merged` is
        // unmet and this issue must not reach the merge gate.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("review"), &["approved:merge"]);
        seed_green_pull_request(&vcs);
        seed_issue(&vcs, 2, &[], IssueState::Open);
        seed_blocked_by(&vcs, 1, &[2]);

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(
            outcome.decision,
            AdvanceDecision::BlockedOnRequires {
                phase: "review".to_string(),
                unmet: vec![
                    Requirement::Roborev("clean".to_string()),
                    Requirement::DepsMerged,
                ],
            }
        );
    }

    #[test]
    fn advance_treats_a_closed_blocked_by_issue_as_merged() {
        // The other side of the same check, and why it is written as
        // "closed" rather than "merged via a PR": a dependency closed by
        // its merged PR -- or closed as won't-fix -- genuinely unblocks
        // this issue, so `deps_merged` drops out of the unmet list and
        // only the unreportable roborev verdict is left.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("review"), &["approved:merge"]);
        seed_green_pull_request(&vcs);
        seed_issue(&vcs, 2, &[], IssueState::Closed);
        seed_blocked_by(&vcs, 1, &[2]);

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(
            outcome.decision,
            AdvanceDecision::BlockedOnRequires {
                phase: "review".to_string(),
                unmet: vec![Requirement::Roborev("clean".to_string())],
            }
        );
    }

    #[test]
    fn advance_treats_a_dependency_it_cannot_read_as_unmerged() {
        // The fail-safe direction, and the one place this deliberately
        // differs from `read_drift` (which reports an unreadable
        // dependency as *missing* drift and carries on): #9 is not
        // seeded, so `gh` fails, and the dependency must count as
        // UNMERGED. Failing the other way would clear §8.1's merge gate
        // off a dependency nobody could verify.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("review"), &["approved:merge"]);
        seed_green_pull_request(&vcs);
        seed_blocked_by(&vcs, 1, &[9]);

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert_eq!(
            outcome.decision,
            AdvanceDecision::BlockedOnRequires {
                phase: "review".to_string(),
                unmet: vec![
                    Requirement::Roborev("clean".to_string()),
                    Requirement::DepsMerged,
                ],
            }
        );
    }

    #[test]
    fn the_advance_wire_result_tags_a_unit_and_a_struct_requirement_alike() {
        // The wire contract, not just the domain type -- checked here
        // for the same reason `approve`'s denial test checks
        // `reentry_triggered`: this is a multi-variant enum crossing the
        // MCP boundary, and serde's internally-tagged representation is
        // the one place a *unit* variant (`deps_merged`) could silently
        // serialize into a different shape than its struct siblings
        // (`roborev`). Exhaustiveness itself is guaranteed by the `From`
        // impls' lack of a wildcard arm, not by this test.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("review"), &["approved:merge"]);
        seed_green_pull_request(&vcs);
        seed_issue(&vcs, 2, &[], IssueState::Open);
        seed_blocked_by(&vcs, 1, &[2]);

        let outcome = advance_issue(&vcs, &project, 1).unwrap();
        let wire = super::super::wire::AdvanceResult::from_outcome(
            &outcome, "proj-a", REPO,
        );
        let json = serde_json::to_value(&wire).unwrap();

        assert_eq!(json["decision"]["kind"], "blocked_on_requires");
        assert_eq!(json["decision"]["phase"], "review");
        assert_eq!(json["decision"]["unmet"][0]["kind"], "roborev");
        assert_eq!(json["decision"]["unmet"][0]["expected"], "clean");
        assert_eq!(json["decision"]["unmet"][1]["kind"], "deps_merged");
        assert_eq!(json["status_label_added"], serde_json::Value::Null);
        assert_eq!(json["status_label_removed"], serde_json::Value::Null);
        assert_eq!(json["action_executed"], false);
        assert_eq!(json["issue"]["number"], 1);
    }

    #[test]
    fn the_advance_wire_result_reports_a_transition_it_actually_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("product-spec"), &["approved:product-spec"]);

        let outcome = advance_issue(&vcs, &project, 1).unwrap();
        let wire = super::super::wire::AdvanceResult::from_outcome(
            &outcome, "proj-a", REPO,
        );
        let json = serde_json::to_value(&wire).unwrap();

        assert_eq!(json["decision"]["kind"], "advance");
        // Only the phase's *id* crosses the wire, never the parsed
        // `Phase` itself -- that would publish a team's workflow-file
        // schema as an MCP contract.
        assert_eq!(json["decision"]["to"], "conflict-check");
        assert!(json["decision"].get("action").is_none());
        assert_eq!(json["status_label_added"], "status:conflict-check");
        assert_eq!(json["status_label_removed"], "status:product-spec");
        // A status label moved, and still nothing ran: §7.1's phase
        // engine does not run alongside this server.
        assert_eq!(json["action_executed"], false);
        assert_eq!(json["issue"]["status"], "conflict-check");
    }

    #[test]
    fn advance_does_not_fail_the_whole_call_over_an_unreadable_dependency() {
        // The other half of that choice, stated separately because it is
        // a separate decision: an unreadable dependency blocks the
        // advance, it does not turn the tool into an error. The operator
        // still gets a decision naming what is unmet.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("review"), &["approved:merge"]);
        seed_blocked_by(&vcs, 1, &[9]);

        assert!(advance_issue(&vcs, &project, 1).is_ok());
    }

    #[test]
    fn advance_reports_an_unknown_issue_as_a_vcs_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());

        assert!(matches!(
            advance_issue(&vcs, &project, 404),
            Err(ToolError::Vcs(_))
        ));
    }

    #[test]
    fn advance_reports_a_project_with_no_workflow_file() {
        // Same stance as `backlog`/`approve`: the phase sequence is
        // *defined* by this file (§7.0), so a missing one fails loudly
        // rather than falling back to the shipped default -- and it
        // fails before any label is touched.
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        vcs.create_issue(REPO, "Widget cache", "").unwrap();
        let project = project_config("a", REPO, tmp.path());
        vcs.set_label(REPO, 1, "status:product-spec", true).unwrap();

        assert!(matches!(
            advance_issue(&vcs, &project, 1),
            Err(ToolError::ReadWorkflow { .. })
        ));
        assert_eq!(labels(&vcs), vec!["status:product-spec".to_string()]);
    }

    /// Write a two-phase workflow whose second phase carries the
    /// `status_label` `second_status` — omitted entirely when that is
    /// `None`, which is the config shape `advance_issue`'s "target phase
    /// with no `status_label`" case is about.
    fn scaffold_two_phase_workflow(
        project: &ProjectConfig,
        second_status: Option<&str>,
    ) {
        let second = match second_status {
            Some(status) => format!(", status_label: {status}"),
            None => String::new(),
        };
        let statuses = match second_status {
            Some(status) => format!("[first, {status}]"),
            None => "[first]".to_string(),
        };
        let yaml = format!(
            "\
labels:
  priority: [P0]
  status: {statuses}
  gate_prefix: \"gate:\"
  approval_prefix: \"approved:\"
  owner_prefix: \"owner:\"
spec: {{tool: openspec, optional: true, skip_label: \"spec:skip\"}}
review_panel: []
fix_loop: {{max_rounds: 1, on_exhausted: escalate, on_decision_finding: escalate}}
phases:
  - {{id: first, action: {{type: agent, role: developer}}, gate: none, status_label: first}}
  - {{id: second, action: {{type: agent, role: developer}}, gate: none{second}}}
"
        );
        let path = workflow_path(project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, yaml).unwrap();
    }

    #[test]
    fn advance_into_a_phase_with_no_status_label_writes_nothing_at_all() {
        // §2.3 makes the `status:*` label the ONLY GitHub record of
        // workflow position, so a target phase declaring none has no
        // label to write -- and removing the current one would erase the
        // issue's position entirely, stranding it at `NoCurrentPhase`.
        // The decision is still reported; the labels are left alone.
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        vcs.create_issue(REPO, "Widget cache", "").unwrap();
        let project = project_config("a", REPO, tmp.path());
        scaffold_two_phase_workflow(&project, None);
        vcs.set_label(REPO, 1, "status:first", true).unwrap();

        let outcome = advance_expecting_no_write(&vcs, &project);

        assert!(matches!(
            &outcome.decision,
            AdvanceDecision::Advance { to } if to.id == "second"
        ));
        assert_eq!(labels(&vcs), vec!["status:first".to_string()]);
    }

    #[test]
    fn advance_does_not_remove_a_status_label_the_next_phase_also_claims() {
        // Nothing forbids two phases sharing a `status_label`. Removing
        // "the previous status" there would undo the add and leave the
        // issue with no status at all -- the guard in `advance_issue`.
        let tmp = tempfile::tempdir().unwrap();
        let vcs = FakeVcs::new();
        vcs.create_issue(REPO, "Widget cache", "").unwrap();
        let project = project_config("a", REPO, tmp.path());
        scaffold_two_phase_workflow(&project, Some("first"));
        vcs.set_label(REPO, 1, "status:first", true).unwrap();

        let outcome = advance_issue(&vcs, &project, 1).unwrap();

        assert_eq!(outcome.status_added.as_deref(), Some("status:first"));
        assert!(outcome.status_removed.is_none());
        assert_eq!(labels(&vcs), vec!["status:first".to_string()]);
        assert_eq!(outcome.snapshot.status.as_deref(), Some("first"));
    }

    #[test]
    fn advance_only_acts_on_the_bound_projects_repo() {
        // §15's "work is identified by (project, issue_number)": the
        // same issue number in another repo must be untouched.
        let tmp = tempfile::tempdir().unwrap();
        let (vcs, project) = issue_and_project(tmp.path());
        label_issue(&vcs, Some("product-spec"), &["approved:product-spec"]);
        vcs.issues.borrow_mut().insert(
            ("owner/repo-b".to_string(), 1),
            crate::vcs::IssueRef {
                number: 1,
                title: "On b".to_string(),
                body: String::new(),
                labels: vec!["status:product-spec".to_string()],
                state: IssueState::Open,
            },
        );

        advance_issue(&vcs, &project, 1).unwrap();

        assert_eq!(
            vcs.issues.borrow()[&("owner/repo-b".to_string(), 1)].labels,
            vec!["status:product-spec".to_string()],
            "the other repo's issue must not have moved"
        );
    }
}
