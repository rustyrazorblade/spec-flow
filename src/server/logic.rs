//! The MCP tools' bodies, as ordinary synchronous functions over the
//! [`Vcs`] seam — everything about §6's `register`/`board`/`issue`/
//! `backlog`/`drift` that can be decided or fetched without an async
//! runtime or an HTTP connection in scope.
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
use crate::state::{Claim, IssueSnapshot, read_issue_state};
use crate::vcs::{IssueRelationships, IssueState, Vcs, VcsError};
use crate::workflow::{WorkflowConfig, WorkflowError, parse_workflow};

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
}
