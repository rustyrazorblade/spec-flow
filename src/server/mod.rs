/*! The MCP daemon itself (§2.5, §5, §6, §14) — `spec-flow serve`.

This is §6's tool surface as far as it is built: the async foundation
(`tokio` + `rmcp` over streamable HTTP/SSE, loopback), plus **every
read-only tool under §6's "Board / status" heading** — `register`,
`board`, `issue`, `backlog`, and `drift`, all of which re-derive their
answer from GitHub on every call (§2.3) and write nothing — plus three
write-path tools: §6's "Approval & gates" pair,
[`approve`](SpecFlowServer::approve) and
[`set_gate`](SpecFlowServer::set_gate), and §6's
[`advance`](SpecFlowServer::advance). All three write the GitHub labels
their decision means before returning (§6, §15), then re-read the issue
so what they return is GitHub's answer rather than an assumption about
the write.

`approve`'s **deny** and `advance`'s **advancing path** each close only
part of their §6 contract, on purpose — see "What this server
deliberately does not implement" below.

# The connection *is* the project (§2.5, §6, §15)

§15's hard rule — "a connection is bound to exactly one project for its
life ... and never changed" — is implemented by giving every MCP session
its **own** [`SpecFlowServer`] instance, whose `bound` field starts empty
and is filled exactly once by `register`. That is not an accident of
this design; it is [`StreamableHttpService`]'s documented shape:
`StreamableHttpService::new` takes a *service factory* (`impl Fn() ->
Result<S, io::Error>`) and calls it once per session, handing the
resulting handler to a per-session worker task that serves every request
carrying that session's `Mcp-Session-Id`. Connection-scoped state is
therefore just a field, with no session-id bookkeeping of our own.

## CONFIRMED constraint: MCP `2026-07-28` has no sessions, so it cannot
## carry a bound project

Read from `rmcp` 3.1.0's own source (`transport/streamable_http_server/
tower.rs`), not inferred: the transport decides session-vs-stateless
routing from the protocol version in the client's `initialize` body (or
its `MCP-Protocol-Version` header) *before* any handler is consulted —
`is_legacy_request` → `uses_legacy_lifecycle(version)`, which is `version
< 2026-07-28`. Per SEP-2567 the `2026-07-28` revision **removes
sessions**, and the config field's own doc says so: "requests negotiating
that version are always served statelessly regardless of this setting."
On that stateless path the factory runs **per request**, so a `register`
would bind a handler that is dropped before the next call arrives.

There is no way to honour "project rides the connection" for a client
that negotiates `2026-07-28` — the protocol has deliberately removed the
identity the binding needs. So [`SpecFlowServer`] overrides
[`ServerHandler::supported_protocol_versions`] to advertise only the
session-bearing revisions; a client offering `2026-07-28` negotiates down
to `2025-11-25` instead. This is a real, pinned limitation of the design,
not a workaround chosen for convenience: when a spawned-phase/coordinator
client one day *requires* `2026-07-28`, the binding has to move to
something the stateless protocol does carry (§2.6's correlation token is
the obvious candidate — it is already a per-request-presentable secret,
unlike a coordinator's `cwd`), and that is a design decision for the
step that needs it.

# What this server deliberately does not implement

Not stubs — absent, and listed here so nobody has to grep for them. Every
write-path §6 tool except the "Approval & gates" pair and `advance`:
`start_implement`, `submit_artifact`, `submit_review`, `create_issue`,
`cancel_work`, `sync_ci`, `address`, `link`, `unlink`,
`acquire_lease`/`heartbeat_lease`/`release_lease`/`lease_status`,
`report`, and `instructions`. `init` stays a CLI subcommand here rather
than an MCP tool (§14.1 ships it as one); wiring it as a tool too is a
later decision, not an omission with a workaround.

Also absent, and each for a stated reason:

- **`advance`'s execution of the phase it advances *into*** (§6, §7.1) —
  the second of the two *half*-built things here, and the gap to read
  before touching that tool. §6 calls `advance` a "manual/coordinator
  override of the state machine", and this implements the state machine
  part in full: it evaluates [`crate::advance`]'s decision against the
  project's own workflow, stops at a `human` gate, passes an `auto` one,
  blocks on unmet `requires`, and on a clear path **writes the
  `status:<phase>` label transition** — add the target's, remove the
  previous one, then re-read. What it does **not** do is run the target
  phase's `action`. Nothing is spawned for an `agent`/`server_then_agent`
  target; **no merge is enqueued** when the target is the merge gate,
  even though [`crate::plan_merge`] already exists as a pure function
  that would decide it; no `finalize` cleanup (worktree/branch pruning,
  label dropping, closing the issue) happens — none of that mechanism
  exists in this crate at all. This is one uniform line, deliberately
  drawn the same way for every phase type rather than made an exception
  for the merge gate: "this server moves labels; the phase engine runs
  phases." A merge-shaped exception would be a partial execution model
  where some phases run and some do not with nothing in the tool surface
  saying which — and §2.3b's one safety invariant (nothing merges to the
  default branch by accident) makes that the worst possible place to
  start. [`wire::AdvanceResult::action_executed`] is always `false` so
  no client can read a moved status label as "the phase ran", the same
  way [`wire::ApproveResult::reentry_triggered`] handles `approve`'s gap.

- **Any roborev verdict at all, for `advance`'s `requires` evaluation**
  (§12, §7.2 ph.7). §12 is explicit that "**roborev** (which it can't
  derive) is `report`ed by an agent", and §6's `report` tool is on the
  absent list above; no label, comment convention, or [`crate::Vcs`]
  method anywhere in this crate carries one either. So `advance` passes
  `roborev_verdict: None` and any phase requiring `{roborev: ...}`
  reports that requirement in its `unmet` list rather than having it
  silently pass. The visible consequence, stated plainly because it is
  not small: the shipped default's `review` phase requires `{roborev:
  clean}`, so **this tool currently cannot move an issue into `review`
  on the shipped default**, however green CI is. That is the honest
  state of the system until a `report` tool exists, not something to
  paper over with a default verdict.

- **`approve(deny)`'s re-entry routing** (§6) — the one *half*-built
  thing here, and the gap to read before touching this tool. §6's deny
  "records the state + note **and** routes the phase back to its defined
  re-entry (product-spec→re-groom, architecture→re-propose,
  test-plan→re-plan, merge→address)". This server does the first half in
  full — it posts a GitHub comment carrying the decision, the note, and
  the re-entry the *project's own* workflow defines for that phase
  (`logic::denial_reentry` derives it structurally: the merge gate
  re-enters the out-of-band phase declaring `reenters: <that phase>`,
  i.e. `address` in the shipped default; an agent phase re-enters
  itself, which is what §6's re-groom/re-propose/re-plan are) — and does
  **not** do the second at all. It writes no label for a denial and
  re-runs nothing. Routing means spawning an agent (`address` is an
  agent phase, §7.2) or re-running a content phase, and no phase engine
  or spawner runs alongside this server — the same boundary
  `next_assignment` below is missing for. The result says so in a field
  of its own ([`wire::ApproveResult::reentry_triggered`], always
  `false`) rather than leaving a client to infer from a named `reentry`
  that the workflow moved on.

- **A positive record that `set_gate(auto)` was ever chosen** (§15's
  "an explicit human action or a **recorded** `auto` choice"). §6
  defines `set_gate` as "adding/removing the `gate:<phase>` label (the
  MCP surface of that label)", and one binary label cannot encode three
  modes: `none` and `auto` both remove it, so a label-absent phase looks
  identical whether an operator chose `auto` for this issue or never
  touched it. `crate::workflow::set_gate`'s and `crate::workflow`'s
  `gate` module docs already record this from the decision and read
  sides; this tool inherits it rather than minting a marker label
  nothing else in the crate reads. The case where it would be unsafe is
  already carved out: the merge gate never *falls back* to `auto` from a
  `human` default off an absent label (`crate::workflow::effective_gate_mode`,
  §4.3's "merge is special") — a merge-gate phase configured `auto`
  outright still resolves to `auto`, which is the recorded-choice case
  §15 allows, living in the config rather than a per-issue label.
- **`next_assignment`** (§6) — the one read-shaped tool still missing.
  It returns "the highest-value content phase awaiting an agent per the
  scheduling order (§12)", which is [`crate::schedule::next_action`]'s
  job, and that function's tier-1 input is each issue's *actionability*:
  gate not parked on a human, `requires` satisfied, not already
  claimed/running. Two of those three come from the phase engine and the
  live [`crate::ProcessSpawner`] map, neither of which runs alongside
  this server — the same boundary [`crate::board`]'s own doc draws for
  the `next_action` column it likewise omits. §6 also scopes it across
  the fleet's whole scheduling context, which a connection bound to one
  project (§2.5) does not have. A `next_assignment` computed from the
  slice of that available here would hand an agent work it may not be
  clear to start, which is worse than not answering.
- **`register`'s spawned-phase path** (§2.6's `spawn_token`). Presenting
  a token is *rejected*, not ignored — see
  [`logic::ToolError::SpawnTokenUnsupported`]. There is nothing to bind a
  token to: no spawner is running alongside this server to have minted
  one, and no `submit_artifact` exists for it to gate.
- **`board`'s and `backlog`'s `filter` arguments** (§6). Likewise
  rejected rather than ignored (see
  [`logic::ToolError::BoardFilterUnsupported`] and
  [`logic::ToolError::BacklogFilterUnsupported`], one variant each so the
  rejection names the tool the client actually called): §6 gives `filter`
  no shape for either, so silently returning an unfiltered list to a
  client that asked for a filtered one would be a wrong answer dressed as
  a right one.
- **Three of §12's six drift kinds** — a status label disagreeing with
  reality, an orphaned worktree, an implemented branch with no PR.
  `drift` reports the six checks [`crate::state::drift`] implements;
  those three compare GitHub against this instance's own worktree/phase
  bookkeeping, which does not exist yet (that module's doc has the full
  reasoning). `drift`'s *contention* half is likewise partial: it reports
  who holds each work-claim (§8.2), but not lease holders/queues (§8.3),
  since this crate has no lease model at all (§16 item 5 — see
  [`crate::merge`]'s doc for the same open question blocking the
  serialized merge fallback).
- **`board`'s WIP-vs-`max_concurrent_agents` count and its single
  recommended `next_action`** (§6, §2.7). WIP needs this instance's live
  [`crate::ProcessSpawner`] `LocalProcess` map, which nothing runs
  alongside the GitHub-state layer yet — the same boundary
  [`crate::board`]'s own doc draws for the worktree/per-agent columns it
  omits. `next_action` needs [`crate::schedule::next_action`], which
  needs each issue's *actionability* from the phase engine; wiring the
  scheduler into the server is a step of its own, and a `next_action`
  computed from a partial view would be worse than none. Note this is
  only the *WIP* half of §2.7's loop: its other half — "surface the
  highest-priority backlog issues and what detail each is still
  missing" — is exactly what `backlog` now answers.
- **`instance_id` auto-generation on first `serve`** (§11.1, and
  [`crate::GlobalConfig::instance_id`]'s doc, which names `serve` as its
  home). Nothing in this slice writes a claim, so nothing needs an
  instance id yet; generating and persisting one as a side effect of a
  read-only server would be an unrequested config mutation.
- **The CI/PR poller** (§12) and any background task at all. This server
  is purely request-driven; `serve` starts a listener and nothing else.
- **Any caching or request batching.** `board`, `backlog`, and `drift`
  each re-read every open issue from GitHub on every call, one `gh`
  subprocess at a time — and `drift` adds one more per off-board
  dependency. This is measurably slow, not theoretically: all three
  measured **49 `gh` subprocesses and ~20 seconds** against a live
  16-open-issue repo (see [`logic::read_board`]'s, [`logic::read_backlog`]'s
  and [`logic::read_drift`]'s "Cost" sections, the last of which is also
  explicit about which part of it the measurement did *not* exercise).
  Neither fix (§2.3/§5's disposable local cache, or a batched GraphQL
  read) is tuning that belongs in a read-only slice.
  Nor is the workflow file [`backlog`](SpecFlowServer::backlog),
  [`approve`](SpecFlowServer::approve),
  [`set_gate`](SpecFlowServer::set_gate) and
  [`advance`](SpecFlowServer::advance) each parse per call cached —
  see `logic`'s `load_workflow` for why re-reading a file a team edits
  mid-session is the point, not an oversight. `approve` and `set_gate`
  are single-issue calls with no open-issue fan-out at all: one or two
  `gh` invocations for the write, plus [`logic::read_issue`]'s usual
  three-to-four for the re-read. `advance` is the most expensive of the
  three and still bounded the same way: that read, one `gh issue view`
  per `blockedBy` dependency, and — only when it actually advances — one
  or two label writes and a second full re-read.
*/

mod logic;
mod wire;

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    ErrorData, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::config::{GlobalConfig, ProjectConfig};
use crate::vcs::ShellVcs;

pub use self::logic::{
    AdvanceOutcome, ApproveOutcome, ClaimHolder, DriftReport, SetGateOutcome,
    ToolError,
};
pub use self::wire::{
    AdvanceArgs, AdvanceDecisionWire, AdvanceResult, ApproveArgs,
    ApproveDecisionWire, ApproveResult, BacklogArgs, BacklogResult,
    BacklogRowWire, BoardArgs, BoardResult, BoardRowWire, CiConclusionWire,
    ClaimHolderWire, ClaimWire, DriftFindingWire, DriftResult, GateModeWire,
    IssueArgs, IssueResult, IssueStateWire, PriorityWire,
    PullRequestStateWire, PullRequestStatusWire, RegisterArgs, RegisterResult,
    RelationshipsWire, RequirementWire, SetGateArgs, SetGateResult,
};

/// The HTTP path the MCP endpoint is mounted at.
///
/// Fixed rather than configurable: §11.1's global config carries a bind
/// address and nothing about a path, and every client is configured with
/// a full URL anyway.
pub const MCP_PATH: &str = "/mcp";

/// How many open issues one open-issue-paging read tool will page in.
///
/// A constant, not a config knob, because §6's `board(filter?)` is the
/// place paging/filtering belongs and that argument is unimplemented
/// (see the module doc) — inventing a `board_limit:` config field now
/// would pin a shape the eventual `filter` may well subsume. 200 is
/// generous next to the `max_concurrent_agents` handful a fleet actually
/// works at once, and a repo that exceeds it gets
/// [`BoardResult::truncated`] rather than a quietly short board.
///
/// Named for `board`, the tool that introduced it. `backlog` and `drift`
/// page the same open-issue list through the same cap rather than each
/// inventing a differently-tuned one: all three answer questions about
/// "this project's open issues," and a fleet where `drift` scanned a
/// different slice of the repo than `board` displayed would make the two
/// answers impossible to reconcile.
pub const BOARD_ISSUE_LIMIT: u32 = 200;

/// The MCP protocol revisions this server advertises.
///
/// Every revision `rmcp` 3.1.0 knows about (`ProtocolVersion::KNOWN_VERSIONS`)
/// *except* `2026-07-28` — see the module doc's "CONFIRMED constraint"
/// section for why a session-less revision cannot carry §15's
/// per-connection project binding. `2024-11-05` is included: it predates
/// streamable HTTP as a named transport, but `rmcp`'s streamable-HTTP
/// server still routes it through `legacy_session_mode` like every other
/// pre-`2026-07-28` revision, so there is no reason to reject it here.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2024_11_05,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_11_25,
];

/// Errors starting the daemon (§14.1's `spec-flow serve`).
///
/// No `Config` variant: [`serve`] takes an already-loaded
/// [`GlobalConfig`], so locating and reading `~/.config/spec-flow/
/// config.yaml` is the binary's failure to report (with its own
/// `anyhow` context about which file and what to do about it), not this
/// function's — the same injection split [`crate::init`] uses for its
/// [`crate::Vcs`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// The configured `listen` address is not a loopback address. A hard
    /// refusal, not a warning: §2.5 says the daemon "never binds a
    /// public interface" and describes a single-operator local service
    /// with "no auth, no RBAC", so a non-loopback bind would expose an
    /// unauthenticated control plane over every project on the machine.
    /// Remote access is §2.5's own answer — the operator's SSH tunnel.
    #[error(
        "listen address {addr} is not a loopback address; the daemon is \
         an unauthenticated single-operator service and never binds a \
         public interface (§2.5) — use 127.0.0.1 or ::1, and reach it \
         remotely over your own SSH tunnel"
    )]
    NonLoopbackListen {
        /// The rejected address from the global config.
        addr: SocketAddr,
    },

    /// The listen socket could not be bound.
    #[error("failed to bind {addr}")]
    Bind {
        /// The address that could not be bound.
        addr: SocketAddr,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The HTTP server stopped with an error.
    #[error("the MCP server stopped unexpectedly")]
    Serve {
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

/// What one connection is bound to (§2.5, §6, §15).
///
/// Both halves are captured at `register` and never re-read: the pointer
/// for its `name` (the project handle §2.5 identifies a coordinator
/// session by) and the project's own config for the repo slug every `gh`
/// call is scoped with (§8.5).
#[derive(Clone, Debug)]
struct BoundProject {
    name: String,
    config: ProjectConfig,
}

/// State shared by every connection: read once at `serve`, never mutated.
#[derive(Debug)]
struct SharedState {
    global: GlobalConfig,
    vcs: Arc<ShellVcs>,
}

/// One MCP connection's handler — the `spec-flow` daemon as an
/// [`rmcp`] server (§5, §6).
///
/// One instance per session (see the module doc): `shared` is cloned
/// from the factory's captured [`Arc`], `bound` starts empty and is
/// filled exactly once by [`SpecFlowServer::register`].
#[derive(Clone)]
pub struct SpecFlowServer {
    shared: Arc<SharedState>,
    /// This connection's project, once registered.
    ///
    /// A `std::sync::Mutex`, not `tokio`'s: every critical section here
    /// is a clone or a compare of a small struct with no `.await` inside
    /// it, which is exactly the case the async-mutex guidance says to
    /// keep synchronous.
    bound: Arc<Mutex<Option<BoundProject>>>,
    /// The generated tool table. Read by the `#[tool_handler]`-generated
    /// `ServerHandler::call_tool`/`list_tools`.
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl SpecFlowServer {
    /// A fresh, unbound handler for one connection.
    fn new(shared: Arc<SharedState>) -> SpecFlowServer {
        SpecFlowServer {
            shared,
            bound: Arc::new(Mutex::new(None)),
            tool_router: SpecFlowServer::tool_router(),
        }
    }

    /// `register(worker_ref, role, cwd?, spawn_token?)` (§6) — announce
    /// this agent and bind the connection to a project for its life.
    ///
    /// Coordinator path only in this slice: no `spawn_token`, and a
    /// `cwd` that the daemon resolves against its project registry
    /// (§2.5). A directory outside every registered project is rejected
    /// with the "run `spec-flow init` there" answer §6 prescribes.
    #[tool(
        description = "Announce this agent and bind the connection to the \
                       registered project containing `cwd`. Every later \
                       tool call on this connection acts on that project \
                       and no other."
    )]
    pub async fn register(
        &self,
        Parameters(args): Parameters<RegisterArgs>,
    ) -> Result<Json<RegisterResult>, ErrorData> {
        let result = self.register_inner(args).await;
        result.map(Json).map_err(tool_error)
    }

    /// `board(filter?)` (§6, §13) — the orchestration board for the
    /// connection's bound project.
    #[tool(description = "The orchestration board for this connection's \
                       project: every open issue with its phase, owner, \
                       gates, approvals, dependencies, and PR/CI signals.")]
    pub async fn board(
        &self,
        Parameters(args): Parameters<BoardArgs>,
    ) -> Result<Json<BoardResult>, ErrorData> {
        self.board_inner(args).await.map(Json).map_err(tool_error)
    }

    /// `issue(number)` (§6) — one issue's derived state.
    #[tool(description = "One issue's derived state (phase, gates, \
                       approvals, owner, dependencies, linked pull \
                       requests) within this connection's project.")]
    pub async fn issue(
        &self,
        Parameters(args): Parameters<IssueArgs>,
    ) -> Result<Json<IssueResult>, ErrorData> {
        self.issue_inner(args).await.map(Json).map_err(tool_error)
    }

    /// `backlog(filter?)` (§6, §2.7) — the queue-filling worklist for
    /// the connection's bound project.
    #[tool(description = "The prioritized not-yet-ready issues in this \
                       connection's project, each annotated with the \
                       approvals it still needs to reach status:ready \
                       — the queue-filling worklist.")]
    pub async fn backlog(
        &self,
        Parameters(args): Parameters<BacklogArgs>,
    ) -> Result<Json<BacklogResult>, ErrorData> {
        self.backlog_inner(args).await.map(Json).map_err(tool_error)
    }

    /// `drift()` (§6, §12) — drift + contention for the connection's
    /// bound project.
    ///
    /// No arguments at all, matching §6's own `drift()`: the project
    /// rides the connection (§2.5), and every check runs over the whole
    /// project by definition.
    #[tool(description = "Current drift and contention for this \
                       connection's project: dependency cycles, \
                       dependencies on closed or missing issues, stale \
                       claims, merged PRs against open issues, \
                       ambiguous labels, multiple open linked PRs, and \
                       who currently holds each work-claim. Surfaced, \
                       never auto-fixed.")]
    pub async fn drift(&self) -> Result<Json<DriftResult>, ErrorData> {
        self.drift_inner().await.map(Json).map_err(tool_error)
    }

    /// `approve(issue_number, phase, decision, note?)` (§6, §2.3b) —
    /// grant or deny a `human` gate, writing the decision through to
    /// GitHub before returning (§15).
    ///
    /// A **denial records the decision and does not route the phase
    /// back** to the re-entry §6 defines for it; the re-entry is named
    /// in the result and in the posted comment. See this module's doc,
    /// and `logic`'s `approve_issue`, for why.
    #[tool(description = "Grant or deny a phase's human gate on one \
                       issue. `grant` writes the phase's approval \
                       label; `deny` writes no label and records the \
                       decision, the note, and the workflow's defined \
                       re-entry as an issue comment — it does NOT \
                       re-run that re-entry. Both are written to \
                       GitHub before this returns.")]
    pub async fn approve(
        &self,
        Parameters(args): Parameters<ApproveArgs>,
    ) -> Result<Json<ApproveResult>, ErrorData> {
        self.approve_inner(args).await.map(Json).map_err(tool_error)
    }

    /// `set_gate(issue_number, phase, mode)` (§6, §4.3) — set one
    /// phase's gate for one issue by adding/removing its
    /// `gate:<phase>` label.
    #[tool(description = "Set a phase's gate for one issue by adding or \
                       removing its `gate:<phase>` label (§4.3's \
                       per-issue override of the workflow's default). \
                       `human` adds the label; `none` and `auto` both \
                       remove it — the per-issue label cannot encode \
                       three states, and the merge gate is never \
                       implicitly auto.")]
    pub async fn set_gate(
        &self,
        Parameters(args): Parameters<SetGateArgs>,
    ) -> Result<Json<SetGateResult>, ErrorData> {
        self.set_gate_inner(args).await.map(Json).map_err(tool_error)
    }

    /// `advance(issue_number)` (§6) — the manual/coordinator override of
    /// the state machine: stop at a `human` gate, pass an `auto` gate,
    /// block on unmet `requires`, and otherwise write the issue's
    /// `status:<phase>` transition through to GitHub (§15).
    ///
    /// Two halves of §7.1's engine are deliberately **not** here: the
    /// target phase's action never runs (no spawn, no merge enqueue, no
    /// `finalize` cleanup), and no roborev verdict exists to satisfy a
    /// `{roborev: ...}` requirement with. See this module's doc, and
    /// `logic`'s `advance_issue`, for both.
    #[tool(description = "Advance one issue to the next workflow phase \
                       if it is clear to move: this writes the \
                       status:<phase> label transition and nothing else. \
                       A blocked issue is a normal successful answer \
                       naming what blocks it (an unapproved gate, or the \
                       specific unmet requirements), not an error. The \
                       target phase's action is NEVER executed — no \
                       agent is spawned, no merge is enqueued, no \
                       cleanup runs.")]
    pub async fn advance(
        &self,
        Parameters(args): Parameters<AdvanceArgs>,
    ) -> Result<Json<AdvanceResult>, ErrorData> {
        self.advance_inner(args).await.map(Json).map_err(tool_error)
    }

    // -- tool bodies, split out so the `?` operator is usable and the
    //    error mapping happens in exactly one place per tool --

    async fn register_inner(
        &self,
        args: RegisterArgs,
    ) -> Result<RegisterResult, ToolError> {
        if args.spawn_token.is_some() {
            return Err(ToolError::SpawnTokenUnsupported);
        }
        let cwd = args.cwd.ok_or(ToolError::CwdRequired)?;

        // Off the async worker thread like every other `Vcs`/config-file
        // read this handler reaches — `resolve_project` reads
        // `<project_dir>/config.yaml` from disk, so it belongs in
        // `blocking()` for the same reason `board`/`issue`'s reads do,
        // even though this one file is small.
        let global = self.shared.global.clone();
        let cwd_owned = cwd.clone();
        let (pointer, config) =
            blocking(move || logic::resolve_project(&global, &cwd_owned))
                .await?;

        let effective = {
            let mut bound = self.lock_bound();
            match bound.as_ref() {
                // §15: bound "for its life ... and never changed". A
                // re-`register` naming the *same* project changes
                // nothing, so it is accepted as the idempotent retry it
                // almost certainly is; naming a different one is the
                // cross-project act the rule forbids outright.
                Some(existing) if existing.name != pointer.name => {
                    return Err(ToolError::AlreadyBound {
                        bound: existing.name.clone(),
                        requested: pointer.name.clone(),
                    });
                }
                // Already bound to this same project: the stored
                // binding is left untouched rather than overwritten from
                // the fresh read above, so an operator editing
                // `config.yaml` between two `register` calls on one
                // connection cannot change that connection's effective
                // repo mid-life. The *reported* result is taken from
                // this same untouched binding, not from `config`, so a
                // client is never told about a repo this connection
                // will not actually act against.
                Some(existing) => existing.clone(),
                None => {
                    let fresh = BoundProject {
                        name: pointer.name.clone(),
                        config: config.clone(),
                    };
                    *bound = Some(fresh.clone());
                    fresh
                }
            }
        };

        tracing::info!(
            project = %effective.name,
            repo = %effective.config.repo,
            role = %args.role,
            worker_ref = %args.worker_ref,
            "connection bound to project"
        );

        Ok(RegisterResult {
            project: effective.name,
            repo: effective.config.repo.clone(),
            project_dir: effective.config.project_dir.display().to_string(),
            role: args.role,
            worker_ref: args.worker_ref,
        })
    }

    async fn board_inner(
        &self,
        args: BoardArgs,
    ) -> Result<BoardResult, ToolError> {
        if args.filter.is_some() {
            return Err(ToolError::BoardFilterUnsupported);
        }
        let bound = self.bound_project()?;
        let vcs = Arc::clone(&self.shared.vcs);

        // Read the binding once, before the `await`, and report *that*
        // one back: re-reading it afterwards would let a concurrent
        // `register` on the same connection relabel rows that were
        // fetched against a different project.
        let for_task = bound.clone();
        let (rows, truncated) = blocking(move || {
            logic::read_board(
                vcs.as_ref(),
                &for_task.config,
                BOARD_ISSUE_LIMIT,
            )
        })
        .await?;

        Ok(BoardResult {
            project: bound.name,
            repo: bound.config.repo,
            rows: rows.iter().map(Into::into).collect(),
            issue_limit: BOARD_ISSUE_LIMIT,
            truncated,
        })
    }

    async fn issue_inner(
        &self,
        args: IssueArgs,
    ) -> Result<IssueResult, ToolError> {
        let bound = self.bound_project()?;
        let vcs = Arc::clone(&self.shared.vcs);
        let number = args.number;

        let for_task = bound.clone();
        let (snapshot, url) = blocking(move || {
            logic::read_issue(vcs.as_ref(), &for_task.config, number)
        })
        .await?;

        Ok(IssueResult::from_snapshot(
            &snapshot,
            &bound.name,
            &bound.config.repo,
            url,
        ))
    }

    async fn backlog_inner(
        &self,
        args: BacklogArgs,
    ) -> Result<BacklogResult, ToolError> {
        if args.filter.is_some() {
            return Err(ToolError::BacklogFilterUnsupported);
        }
        let bound = self.bound_project()?;
        let vcs = Arc::clone(&self.shared.vcs);

        // Same read-the-binding-once discipline as `board_inner`: the
        // rows are fetched against `for_task`'s repo, so the reported
        // project must be that same capture, not a re-read.
        let for_task = bound.clone();
        let (rows, truncated) = blocking(move || {
            logic::read_backlog(
                vcs.as_ref(),
                &for_task.config,
                BOARD_ISSUE_LIMIT,
            )
        })
        .await?;

        Ok(BacklogResult {
            project: bound.name,
            repo: bound.config.repo,
            rows: rows.iter().map(Into::into).collect(),
            issue_limit: BOARD_ISSUE_LIMIT,
            truncated,
        })
    }

    async fn drift_inner(&self) -> Result<DriftResult, ToolError> {
        let bound = self.bound_project()?;
        let vcs = Arc::clone(&self.shared.vcs);
        // §11.1's operator-configured heartbeat TTL, and this call's own
        // "now" — both captured here rather than inside
        // `logic::read_drift` so staleness stays testable without a
        // clock (the same split `find_stale_claims` itself uses).
        let ttl = self.shared.global.claim.heartbeat_ttl;
        let now = std::time::SystemTime::now();

        let for_task = bound.clone();
        let report = blocking(move || {
            logic::read_drift(
                vcs.as_ref(),
                &for_task.config,
                BOARD_ISSUE_LIMIT,
                ttl,
                now,
            )
        })
        .await?;

        Ok(DriftResult::from_report(
            &report,
            &bound.name,
            &bound.config.repo,
            BOARD_ISSUE_LIMIT,
        ))
    }

    async fn approve_inner(
        &self,
        args: ApproveArgs,
    ) -> Result<ApproveResult, ToolError> {
        let bound = self.bound_project()?;
        let vcs = Arc::clone(&self.shared.vcs);

        // The same read-the-binding-once discipline the read tools
        // use, and it matters more here: the write below goes to
        // `for_task`'s repo, so reporting a project re-read afterwards
        // could label a GitHub write with a repo it never touched.
        let for_task = bound.clone();
        let decision = args.decision.into();
        let outcome = blocking(move || {
            logic::approve_issue(
                vcs.as_ref(),
                &for_task.config,
                args.issue_number,
                &args.phase,
                decision,
                args.note.as_deref(),
            )
        })
        .await?;

        Ok(ApproveResult::from_outcome(
            &outcome,
            &bound.name,
            &bound.config.repo,
        ))
    }

    async fn set_gate_inner(
        &self,
        args: SetGateArgs,
    ) -> Result<SetGateResult, ToolError> {
        let bound = self.bound_project()?;
        let vcs = Arc::clone(&self.shared.vcs);

        let for_task = bound.clone();
        let mode = args.mode.into();
        let outcome = blocking(move || {
            logic::set_gate_for_issue(
                vcs.as_ref(),
                &for_task.config,
                args.issue_number,
                &args.phase,
                mode,
            )
        })
        .await?;

        Ok(SetGateResult::from_outcome(
            &outcome,
            &bound.name,
            &bound.config.repo,
        ))
    }

    async fn advance_inner(
        &self,
        args: AdvanceArgs,
    ) -> Result<AdvanceResult, ToolError> {
        let bound = self.bound_project()?;
        let vcs = Arc::clone(&self.shared.vcs);

        // The same read-the-binding-once discipline `approve_inner` and
        // `set_gate_inner` use, and it matters here for the same reason:
        // the label writes below go to `for_task`'s repo.
        let for_task = bound.clone();
        let outcome = blocking(move || {
            logic::advance_issue(
                vcs.as_ref(),
                &for_task.config,
                args.issue_number,
            )
        })
        .await?;

        Ok(AdvanceResult::from_outcome(
            &outcome,
            &bound.name,
            &bound.config.repo,
        ))
    }

    /// This connection's bound project, or [`ToolError::NotRegistered`].
    fn bound_project(&self) -> Result<BoundProject, ToolError> {
        self.lock_bound().clone().ok_or(ToolError::NotRegistered)
    }

    /// Lock `bound`, recovering from a poisoned mutex.
    ///
    /// A poisoned lock here means some earlier tool call panicked while
    /// holding it. The data it guards is a plain `Option<BoundProject>`
    /// that is only ever wholesale-replaced, so it cannot have been left
    /// half-updated — treating the poison as fatal would kill an
    /// otherwise healthy session over a bit that carries no information
    /// about this data.
    fn lock_bound(&self) -> std::sync::MutexGuard<'_, Option<BoundProject>> {
        self.bound.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SpecFlowServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            // `ServerInfo::new` defaults `server_info` to
            // `Implementation::from_build_env()`, whose `env!` macros are
            // expanded *inside the `rmcp` crate* — so without this the
            // handshake announces `{"name":"rmcp","version":"3.1.0"}`,
            // confirmed live against the built binary. §14.1's
            // compatibility contract ("which binary speaks which MCP
            // tool-surface revision") needs this crate's own identity.
            .with_server_info(rmcp::model::Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "spec-flow: the work queue and git/GitHub mechanics for \
                 AI-agent software delivery. Call `register` first with \
                 the directory this session was opened in — it binds \
                 this connection to exactly one project, and every other \
                 tool acts on that project only.",
            )
    }

    /// See the module doc: `2026-07-28` removes MCP sessions, and this
    /// server's per-connection project binding cannot exist without one.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }
}

/// Run a blocking `git`/`gh` call off the async runtime's worker threads.
///
/// [`crate::vcs`] is synchronous by design (§2.1's "shell out to the
/// operator's local `git`/`gh`"), and a `board` call fans out to dozens
/// of subprocesses; running those inline would park a `tokio` worker for
/// the whole fan-out. A join error means the closure itself panicked —
/// reported as [`ToolError::Internal`], not a [`VcsError`](crate::vcs::VcsError),
/// since a panic in this crate's own code is not an environment failure a
/// client could do anything about by fixing their `git`/`gh` install.
async fn blocking<T, F>(f: F) -> Result<T, ToolError>
where
    F: FnOnce() -> Result<T, ToolError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_error) => {
            tracing::error!(
                error = %join_error,
                "a git/gh worker task failed"
            );
            Err(ToolError::Internal(join_error.to_string()))
        }
    }
}

/// Map a [`ToolError`] onto the JSON-RPC error the client sees.
///
/// The split is by whose problem it is: a malformed or unsupported
/// argument is `invalid_params`, a connection used out of order is
/// `invalid_request`, and a config/`gh` failure is `internal_error` —
/// so a client can tell "fix your call" from "fix your machine" without
/// parsing the message text.
fn tool_error(error: ToolError) -> ErrorData {
    let message = format!("{error}");
    match error {
        ToolError::CwdRequired
        | ToolError::CwdNotAbsolute { .. }
        | ToolError::UnregisteredDirectory { .. }
        | ToolError::SpawnTokenUnsupported
        | ToolError::BoardFilterUnsupported
        | ToolError::BacklogFilterUnsupported
        // A phase the workflow does not define is the *call's* mistake,
        // not the machine's -- the file parsed, and its message lists
        // the ids that would have worked. Deliberately not grouped with
        // `ReadWorkflow`/`Workflow` below, which are about that same
        // file being unusable.
        | ToolError::UnknownPhase { .. } => {
            ErrorData::invalid_params(message, None)
        }
        ToolError::NotRegistered | ToolError::AlreadyBound { .. } => {
            ErrorData::invalid_request(message, None)
        }
        // A missing or malformed `.spec-flow/workflow.yaml` is the
        // machine's problem, not the call's — same side of `tool_error`'s
        // "fix your call vs. fix your machine" split as a config or `gh`
        // failure, and the chain carries the path and the parse error.
        ToolError::ReadWorkflow { .. } | ToolError::Workflow { .. } => {
            ErrorData::internal_error(format_chain(&error), None)
        }
        ToolError::Config(_) | ToolError::Vcs(_) => {
            // The source chain carries the actual `gh` stderr / config
            // path; `{error}` alone would print only the outermost
            // `#[error(transparent)]` line.
            ErrorData::internal_error(format_chain(&error), None)
        }
        ToolError::Internal(_) => ErrorData::internal_error(message, None),
    }
}

/// Render an error and every source under it as one line.
///
/// The library's error types follow the style guide's `#[source]`
/// chaining, but MCP carries a single `message` string — so the chain is
/// flattened here rather than lost.
fn format_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// Build the `tower` service that serves MCP over streamable HTTP/SSE.
///
/// Split out from [`serve`] so tests can mount it on their own
/// ephemeral listener (see `tests/mcp_server.rs`) without a global
/// config file or a fixed port.
///
/// `rmcp`'s default [`StreamableHttpServerConfig`] is mostly what §2.5
/// wants and is left alone for two fields: `legacy_session_mode: true`
/// (the per-session handler instance this server's project binding
/// depends on) and `allowed_hosts: ["localhost", "127.0.0.1", "::1"]`
/// (inbound `Host` validation, which is what actually stops a
/// DNS-rebinding browser from reaching a loopback-bound daemon).
///
/// One field is overridden: `stateless_protocol_metadata_required: true`.
/// The routing decision between the legacy (session-bearing) path and the
/// stateless `2026-07-28` path is made from the client's `initialize`
/// body *or*, failing that, its raw `MCP-Protocol-Version` header
/// (`rmcp`'s own `tower.rs`) — a non-compliant client could set that
/// header to `2026-07-28` without ever completing an `initialize` that
/// negotiates it, land on the stateless path this server never advertises
/// support for, and get a `register` that silently binds a
/// throwaway-per-request handler instead of the rejection the module
/// doc's "CONFIRMED constraint" section promises. This option makes that
/// path require the per-request protocol metadata `2026-07-28` itself
/// specifies, so a client without it is rejected before dispatch instead
/// of silently losing its binding — the same "reject, don't ignore"
/// stance the rest of this slice takes on `spawn_token`/`filter`. Every
/// client this server actually advertises support for negotiates below
/// `2026-07-28` and is routed through the unaffected legacy path (per this
/// field's own doc), so this cannot reject a compliant caller.
pub fn mcp_service(
    global: GlobalConfig,
    vcs: ShellVcs,
) -> StreamableHttpService<SpecFlowServer, LocalSessionManager> {
    let shared = Arc::new(SharedState { global, vcs: Arc::new(vcs) });

    let mut config = StreamableHttpServerConfig::default();
    config.stateless_protocol_metadata_required = true;

    StreamableHttpService::new(
        move || Ok(SpecFlowServer::new(Arc::clone(&shared))),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

/// Run the daemon (§14.1's `spec-flow serve`) until Ctrl-C.
///
/// Binds `global.listen` (§11.1) and serves MCP at [`MCP_PATH`]. `vcs`
/// is passed in rather than constructed here so the binary keeps
/// ownership of resolving §8.5's configured `git`/`gh` paths — the same
/// split [`crate::init`] uses.
pub async fn serve(
    global: GlobalConfig,
    vcs: ShellVcs,
) -> Result<(), ServeError> {
    let addr = global.listen;
    if !addr.ip().is_loopback() {
        return Err(ServeError::NonLoopbackListen { addr });
    }

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| ServeError::Bind { addr, source })?;

    let projects = global.projects.len();
    let router =
        axum::Router::new().nest_service(MCP_PATH, mcp_service(global, vcs));

    tracing::info!(
        %addr,
        path = MCP_PATH,
        projects,
        "spec-flow serving MCP over HTTP/SSE"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            match tokio::signal::ctrl_c().await {
                Ok(()) => tracing::info!("shutting down"),
                Err(error) => tracing::error!(
                    %error,
                    "could not listen for Ctrl-C; the server will keep \
                     running until killed"
                ),
            }
        })
        .await
        .map_err(|source| ServeError::Serve { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `ToolError` for each variant's mapping test, built without
    /// reaching for a real config or `Vcs`.
    fn errors() -> Vec<(ToolError, &'static str)> {
        vec![
            (ToolError::CwdRequired, "invalid_params"),
            (
                ToolError::CwdNotAbsolute { cwd: "rel".to_string() },
                "invalid_params",
            ),
            (
                ToolError::UnregisteredDirectory { cwd: "/x".to_string() },
                "invalid_params",
            ),
            (ToolError::SpawnTokenUnsupported, "invalid_params"),
            (ToolError::BoardFilterUnsupported, "invalid_params"),
            (ToolError::BacklogFilterUnsupported, "invalid_params"),
            (
                ToolError::UnknownPhase {
                    phase: "merge".to_string(),
                    known: vec![
                        "product-spec".to_string(),
                        "review".to_string(),
                    ],
                },
                "invalid_params",
            ),
            (ToolError::NotRegistered, "invalid_request"),
            (
                ToolError::AlreadyBound {
                    bound: "a".to_string(),
                    requested: "b".to_string(),
                },
                "invalid_request",
            ),
        ]
    }

    #[test]
    fn tool_error_maps_client_mistakes_to_the_right_json_rpc_codes() {
        for (error, expected) in errors() {
            let code = match expected {
                "invalid_params" => rmcp::model::ErrorCode::INVALID_PARAMS,
                "invalid_request" => rmcp::model::ErrorCode::INVALID_REQUEST,
                other => panic!("unexpected expectation {other}"),
            };

            assert_eq!(tool_error(error).code, code);
        }
    }

    #[test]
    fn tool_error_maps_environment_failures_to_internal_error() {
        let error = ToolError::Vcs(crate::vcs::VcsError::BinaryNotFound {
            binary: "gh".to_string(),
        });

        assert_eq!(
            tool_error(error).code,
            rmcp::model::ErrorCode::INTERNAL_ERROR
        );
    }

    #[test]
    fn tool_error_maps_an_unreadable_workflow_file_to_internal_error() {
        // A missing or malformed `.spec-flow/workflow.yaml` is not the
        // caller's argument mistake -- `backlog` takes no path -- so it
        // belongs on the "fix your machine" side of the split, with the
        // offending path preserved in the message.
        let error = ToolError::ReadWorkflow {
            path: std::path::PathBuf::from("/p/main/.spec-flow/workflow.yaml"),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            ),
        };

        let mapped = tool_error(error);

        assert_eq!(mapped.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            mapped.message.contains("workflow.yaml")
                && mapped.message.contains("no such file"),
            "the message must name the file and its cause: {}",
            mapped.message
        );
    }

    #[test]
    fn tool_error_maps_a_panicked_worker_task_to_internal_error_not_vcs() {
        // A `spawn_blocking` join failure is a bug in this crate, not an
        // environment problem -- it must not be reported as though `gh`
        // itself failed (see `blocking`'s doc).
        let error = ToolError::Internal("panicked".to_string());

        assert_eq!(
            tool_error(error).code,
            rmcp::model::ErrorCode::INTERNAL_ERROR
        );
    }

    #[test]
    fn tool_error_keeps_the_source_chain_in_the_message() {
        // `#[error(transparent)]` wrappers print only their own line;
        // without the flattening this asserts, a `gh` failure would
        // reach the client as an empty-ish message.
        let error = ToolError::Vcs(crate::vcs::VcsError::CommandFailed {
            command: "gh issue list".to_string(),
            status: 1,
            stderr: "could not resolve to a Repository".to_string(),
        });

        let message = tool_error(error).message.to_string();

        assert!(
            message.contains("could not resolve to a Repository"),
            "message lost its cause: {message}"
        );
    }

    #[test]
    fn supported_protocol_versions_excludes_the_session_less_revision() {
        // The load-bearing assertion behind this module's "the
        // connection is the project" design -- see the module doc.
        assert!(
            !SUPPORTED_PROTOCOL_VERSIONS
                .contains(&ProtocolVersion::V_2026_07_28),
            "advertising a session-less revision would break §15's \
             per-connection project binding"
        );
        assert!(
            SUPPORTED_PROTOCOL_VERSIONS
                .contains(&ProtocolVersion::V_2025_11_25)
        );
        // Pinned so the list stays "every known revision except
        // 2026-07-28" -- see this constant's doc for why 2024-11-05
        // belongs here despite predating the "streamable HTTP" name.
        assert!(
            SUPPORTED_PROTOCOL_VERSIONS
                .contains(&ProtocolVersion::V_2024_11_05)
        );
    }

    #[test]
    fn the_server_handler_actually_advertises_supported_protocol_versions() {
        // The constant above is only load-bearing if `SpecFlowServer`'s
        // `ServerHandler` override actually returns it -- this pins the
        // wiring, not just the list, so deleting the override in
        // `impl ServerHandler for SpecFlowServer` (which would silently
        // fall back to rmcp's default, every known revision including
        // 2026-07-28) fails a test instead of only regressing silently.
        let global = GlobalConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            instance_id: None,
            binaries: crate::config::Binaries::default(),
            harnesses: crate::config::HarnessesConfig {
                default: "claude".to_string(),
                harnesses: std::collections::HashMap::new(),
            },
            limits: crate::config::Limits { max_concurrent_agents: 3 },
            cross_project_mode: crate::config::CrossProjectMode::FairShare,
            claim: crate::config::ClaimConfig {
                heartbeat_ttl: std::time::Duration::from_secs(3600),
                heartbeat_interval: std::time::Duration::from_secs(300),
            },
            phase_timeout: std::time::Duration::from_secs(45 * 60),
            projects: Vec::new(),
        };
        let vcs = ShellVcs::new("git".to_string(), "gh".to_string());
        let shared = Arc::new(SharedState { global, vcs: Arc::new(vcs) });
        let server = SpecFlowServer::new(shared);

        assert_eq!(
            ServerHandler::supported_protocol_versions(&server).as_ref(),
            SUPPORTED_PROTOCOL_VERSIONS
        );
    }
}
