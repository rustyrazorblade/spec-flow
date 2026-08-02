/*! `spec-flow` — a standalone, project-agnostic MCP daemon that owns
the work queue and the git/GitHub mechanics for AI-agent software
delivery (see `/agent-fleet-manager-spec.md` in the repo root for the
full specification this crate implements against; the spec itself
refers to the tool by its original working name, `fleet` — this crate
is that daemon, shipped as `spec-flow`).

# Design shape (spec §2.1, §5)

The daemon performs deterministic mechanics itself — git worktree/branch
lifecycle, commit, push, open PR, read issue, set labels, post comments,
enqueue to the merge queue — by shelling out to the operator's local
`git`/`gh` CLIs, and it spawns short-lived agent processes for anything
requiring judgment (grooming, design, writing code, reviewing a diff).
**All durable state lives in GitHub**; the only legitimately-local state
is the installation/project registry this crate's `config`/`registry`
modules manage (spec §2.3).

# Module map (who owns which file — read before editing)

This is a map for later tasks working in parallel against this
skeleton, not an essay — see spec §14 for the numbered build sequence
these correspond to. Every module below is private; its public items
are re-exported at this crate's root (see the `pub use` block), so
callers write `spec_flow::GlobalConfig`, not
`spec_flow::config::GlobalConfig`.

| File | Owns | Status (§14) |
|---|---|---|
| `config.rs` | [`GlobalConfig`]/[`ProjectConfig`] shapes, their file paths, load/save | Implemented (step 1). `instance_id` auto-generation on first `serve` is **still open** — `serve` now exists (see `server/`) but is read-only, so nothing in it writes a claim label that would need an instance id; generating and persisting one as a side effect of a read-only daemon was deliberately not done. |
| `registry.rs` | Idempotent add/list/find over the global config's `projects` list | Implemented (step 1). |
| `vcs/` | The [`Vcs`] trait (the git/gh subprocess seam), [`ShellVcs`] (real), [`FakeVcs`] (test double) | Fully implemented (step 1) — no `todo!()`s remain in `vcs/shell.rs`. `vcs/mod.rs`'s trait signatures are load-bearing for both implementations and every call site; change them and every caller together. Step 4 added two methods for PR/CI/merge-queue signals: [`Vcs::find_linked_pull_requests`], [`Vcs::read_pull_request_status`]. Step 9 added [`Vcs::ensure_label`] (create a label if absent) to close a real gap: `set_label` cannot add a label that doesn't already exist in the repo, which blocked every fixed-vocabulary `status:`/`gate:`/`approved:` label, not only `claim.rs`'s already-documented per-heartbeat one; and [`Vcs::create_issue`] (§7.2 ph.1's "the server creates the GitHub issue" mechanic, e.g. for `groom`), which reads the freshly created issue back by number rather than parsing anything beyond that number out of `gh issue create`'s URL output, mirroring [`Vcs::open_pr`]'s "create, then view by a stable key" split. Step 11 (the MCP server) added [`Vcs::list_open_issues`], closing a gap nothing had hit before: every read path built to that point started from an issue number a caller already had, so §6/§13's `board` — which takes no issue argument — could not name its own rows. See that method's doc for its scope (numbers only, open only) and why it does not return richer rows. |
| `state/` | The GitHub-state layer (§4.2, §14 step 4): [`IssueSnapshot`] (derived per-issue state), the pure [`state::derive_issue_state`] label-parsing function, the [`state::read_issue_state`] orchestrator, and drift detection (see [`DriftFinding`]) | Implemented (step 4) for what is derivable from GitHub state alone — dependency cycles, closed/missing dependencies, stale claims, a merged PR against a still-open issue, an issue carrying more than one `status:`/`owner:` label, and an issue with more than one simultaneously **open** linked PR. Deliberately does **not** cover the drift kinds needing the phase engine's or worktree manager's own bookkeeping (a status label disagreeing with reality, an orphaned worktree, an implemented branch with no PR) — those are a later step's job (§14 steps 5/7). |
| `init.rs` | The `spec-flow init` business logic ([`init`]) | Implemented (step 1): composes `config`, `registry`, `scaffold`, and an injected [`Vcs`]. Step 9 added label-vocabulary provisioning: after scaffolding, `init` reads the repo's *effective* `.spec-flow/workflow.yaml` back off disk (a team's hand-edit, not always the compiled-in default), derives every label name [`crate::workflow::label_vocabulary`] returns, and calls [`Vcs::ensure_label`] for each — closing the gap `vcs/`'s row above describes. |
| `scaffold.rs` | The committed `.spec-flow/` files `init` materializes: the default `workflow.yaml` (§11.3) and one `instructions/<point>.md` per injection point (§9.1) | Implemented (step 1). Base-template content (§14 step 6 Deliverable B) is **partial**: `groom` and `implement` now hold real, substantive templates distilled from this plugin's `product-manager`/`groom` and `tdd-developer`/`implement` agent/skill content; the remaining 17 injection points still hold the original thin placeholder — authoring those is unfinished work, not done here. |
| `instructions.rs` | The instruction composer's pure mechanism (§9.2, §14 step 6 Deliverable A): [`compose`] (base template + optional override + variables → final prompt, honoring the `<!-- mode: replace -->` directive) and [`interpolate`] (single-pass `{name}` substitution over a generic variable map); [`read_instruction_override`] reads a project's `.spec-flow/instructions/<point>.md` | Implemented (step 6) as a standalone, pure/file-I/O-split module with no caller yet — there is no phase engine to invoke it (§14 step 7+ wires it in). Does not resolve a point's base template text itself; callers pass it in (today, `scaffold`'s compiled-in seed strings are the only source of that text). |
| `spawner/` | The `claude` process spawner + `LocalProcess` map ([`ProcessSpawner`]): command-template interpolation, spawn/track/reap, same-instance double-spawn blocking (§2.6, §4.2, §5) | Implemented (step 3). No MCP server or phase engine exists yet, so nothing in this crate calls it — a later step (§14 step 6+) wires it into the phase engine. |
| `claim.rs` | Work-claiming (§8.2, §14 step 5): [`write_claim`]/[`confirm_claim`], the two-step optimistic claim + settle-read; heartbeat refresh and an instance's own stale-claim reclaim are just `write_claim` called again | Implemented (step 5) against the [`Vcs`] trait's existing `read_issue`/`set_label` — no new `Vcs` method was needed. Staleness itself is [`state::drift::find_stale_claims`]'s job, not this module's. **Unresolved architectural conflict, confirmed not just suspected** (see `vcs::shell::ShellVcs::set_label`'s doc): `gh issue edit --add-label` cannot add a label name that doesn't already exist in the repo, but every `write_claim` heartbeat mints a brand-new `owner:<instance>@<epoch>` label name — the claim protocol as specified (§8.2) cannot work against real GitHub through this transport as shipped. Needs a design decision before `serve` (§14 step 6+) relies on it. |
| `schedule.rs` | The scheduler's default ordering (§12, §14 step 5): the pure [`schedule::next_action`]/[`schedule::schedule_order`] functions (actionable-now → furthest-along → priority → age) | Implemented (step 5) over the shipped-default [`schedule::DEFAULT_PHASE_ORDER`] (§7.2) — see its module doc for what's deferred: a workflow-config-defined phase order, a "dependency" tie-break ahead of age, and the CI/PR poller's backoff loop (all §14 step 6+ or later). Step 10 added the cross-project layer on top: [`schedule::next_action_across_projects`] picks *which project* gets a freed spawn slot before applying the existing within-project order — smooth weighted round-robin ([`schedule::FairShareState`], [`CrossProjectMode::FairShare`], the shipped default) or a flat global ranking across every project's issues ([`CrossProjectMode::GlobalPriorityPool`]). Needed two new [`ProjectConfig`]/[`GlobalConfig`] fields ([`ProjectConfig::weight`], [`GlobalConfig::cross_project_mode`]). |
| `workflow/` | The workflow-config data model + parser (§7, §14 step 7): [`workflow::WorkflowConfig`] and its nested types, deserialized straight from `scaffold::DEFAULT_WORKFLOW_YAML`; gate evaluation ([`workflow::gate_clear`] and friends, §2.3b/§4.3); `requires` evaluation ([`workflow::requirement_satisfied`], §7.1); the review-panel/bounded-fix-loop decision primitive ([`workflow::evaluate_panel`], §7.1); `advance`/`approve`/`set_gate` as plain, pure functions (§6); and the two read-only views of the `status:ready` handoff seam ([`workflow::handoff_ready_reached`], [`workflow::readiness_gap`]) that §6's `backlog` is defined by | Implemented (step 7) as pure data + pure functions only — see the module's own doc for exactly what is and is not covered; no phase engine, spawner wiring, or MCP server calls any of it yet. Step 9 added [`workflow::label_vocabulary`] (every GitHub label name the config actually uses), consumed by `init`. |
| `merge.rs` | Merge-queue integration (§8.1, §14 step 8): [`merge::plan_merge`] (gate → enqueue, native mode) and [`merge::observe_merge`] (observe) as pure functions | Implemented (step 8) for the **native** merge queue only — see the module's own doc for why the serialized fallback (§8.1's warn-and-degrade path) is a documented, deliberately unresolved gap rather than a guess: it hinges on §8.3's lease model, itself an open question (§16 item 5). `init`'s merge-queue-enabled check (this step's other named deliverable) was already implemented in step 1 (`init::detect_merge_mode`); nothing changed there. |
| `implement.rs` | `start_implement` kickoff (§10.2, §14 step 9): [`start_implement_setup`] (claim the worktree, read the issue, decide spec-author-vs-skip) and [`finish_implement_setup`] (commit the spec if one was authored, push, post the dev plan) as two `Vcs`-touching orchestration functions | Implemented (step 9) for the mechanics either side of the spawned spec-authoring agent §10.2 places between them — the actual spawn is a future step's job (no spawner wiring exists yet). Does not itself claim the issue; that's `crate::claim`'s job, called before this module. |
| `board.rs` | Board rendering (§13, §14 step 9): [`build_board`], a pure aggregation of [`IssueSnapshot`]s into [`BoardRow`]s; [`build_backlog`], the same aggregation projected onto §6's not-yet-`ready` worklist ([`BacklogRow`]); and [`issue_url`] (deterministically derived, no `Vcs` round trip) | Implemented (step 9) for every column derivable from GitHub state alone (phase, owner, gates/approvals, PR signals) — see the module's own doc for why the **worktree** and per-agent columns §13 also names are deliberately absent: that data lives in `spawner::ProcessSpawner`'s in-memory map, which nothing in this crate runs alongside the GitHub-state layer yet. Step 11 gave it its first caller: `server/`'s `board` MCP tool, then `backlog` (whose rows are ordered by §12's priority/age tiers only — the two tiers that mean anything before an issue is ready to be worked; see [`build_backlog`]'s doc). |
| `server/` | The MCP daemon itself (§2.5, §5, §6, §14): [`serve`] (bind loopback, serve MCP over streamable HTTP/SSE until Ctrl-C), [`mcp_service`] (the `tower` service, split out so tests mount it on an ephemeral port), [`SpecFlowServer`] (one `rmcp` handler instance per connection, holding that connection's bound project), the `*Wire`/`*Args`/`*Result` MCP wire shapes, and [`ToolError`] | Implemented (steps 11-12) for **§6's read-only tools plus its "Approval & gates" write pair** — `register` (coordinator path: resolve `cwd` to a registered project and bind the connection to it for life, §2.5/§15), `board` (§13, every open issue through [`crate::state::read_issue_state`] into [`build_board`]), `issue`, `backlog` (§2.7's queue-filling worklist: the project's *effective* `.spec-flow/workflow.yaml` read off disk per call, then [`build_backlog`]), and `drift` (§12: the first thing in this crate to orchestrate `state::drift`'s six pure checks into one project-wide report, plus the work-claim contention half of §6's bullet — see `logic::read_drift`'s doc for the scope decision that drove it, namely that the open-issue set alone cannot tell a dependency on a *closed* issue from one on a *missing* issue). This is where `tokio` and `rmcp` enter the crate. Read the module's own doc before extending it: it records the **confirmed** `rmcp`/MCP constraint the per-connection project binding rests on (protocol revision `2026-07-28` removes sessions, so the transport serves it statelessly with a fresh handler per request — this server therefore advertises only the session-bearing revisions), and lists every §6 tool deliberately absent, each with its reason. `board`'s and `backlog`'s `filter` arguments and `register`'s `spawn_token` path are **rejected, not ignored**. Step 12 added §6's **"Approval & gates" write pair** — the first tools in this crate that change GitHub state: [`SpecFlowServer::approve`] (grant writes the phase's `approved:<phase>` label; deny writes none and records the decision + note as an issue comment) and [`SpecFlowServer::set_gate`] (add/remove `gate:<phase>`, §4.3). Both write the label their decision means before returning (§6, §15) and then re-read the issue, so what they return is GitHub's answer rather than an assumption about the write. `approve`'s **deny closes only the first half of its §6 contract**: it records the state + note and *names* the re-entry the project's own workflow defines (merge gate → the out-of-band phase that re-enters it, i.e. `address`; an agent phase → itself, which is §6's re-groom/re-propose/re-plan), but routes nothing — see that module's doc, and [`ApproveResult::reentry_triggered`], which is always `false` so no client can read a named re-entry as "the workflow moved on". Step 13 added §6's [`SpecFlowServer::advance`] — the manual/coordinator override of the state machine. It is the first tool in this crate to *run* [`advance`]'s decision against real GitHub state: it reads the project's effective workflow and the issue, computes the two facts [`RequirementContext`] needs that no single snapshot carries (`deps_merged` from one [`Vcs::read_issue`] per `blockedBy` target — an unreadable dependency counts as **unmerged**, the opposite fail-safe direction to `drift`'s "report it as missing", because this value feeds §8.1's merge gate; and `roborev`, which is always `None`), then either reports why the issue cannot move — a normal successful answer, never an error — or writes the `status:<phase>` transition (add the target's label, remove the previous one, re-read). It **never executes the phase it advances into**: no agent spawn, no merge enqueue, no `finalize` cleanup, uniformly for every phase type, with [`AdvanceResult::action_executed`] always `false` so a moved status label cannot be read as "the phase ran". |
| `main.rs` (binary, not part of this library) | `clap` CLI wiring, `tracing` setup, `anyhow` error reporting at the top level | Both subcommands are wired: `init` (step 1) and `serve` (step 11 — loads the global config, builds the [`ShellVcs`] from §8.5's configured binary paths, and blocks on [`serve`] inside a runtime it builds itself rather than via `#[tokio::main]`, so `init` pays nothing for an async runtime it never uses). |

# What is deliberately *not* here yet

This crate currently covers all ten §14 build-sequence steps
(registry/config, the git/gh layer, `init`, the process spawner, the
GitHub-state layer, work-claiming and the scheduler's ordering, the
instruction composer's pure mechanism, the workflow-config parser plus
the phase engine's pure decision logic, native merge-queue integration,
and cross-project scheduling fairness) plus the step-2 spike recorded
in `docs/memory-index-spike.md` (no code needed there), plus the first
slice of the MCP server itself (`server/`) — **minus three deliberately
deferred pieces, each documented at its own module, none guessed at**:

- Step 9's label-vocabulary provisioning (`init` now creates every
  `status:`/`gate:`/`approved:`/`spec:skip` label a project's workflow
  declares, via [`Vcs::ensure_label`] and [`workflow::
  label_vocabulary`]) only runs when `init` itself runs, so a team that
  edits `workflow.yaml` *after* `init` and never re-runs it still hits
  the same "label doesn't exist" failure for whatever it added (see
  `provision_label_vocabulary`'s doc). Step 9's `link`/`unlink` write
  side of `conflict-check`'s proposed dependency edges (§8.4) is
  entirely unbuilt: those need resolving an issue number to its GraphQL
  node ID first (confirmed via live `gh api graphql` schema
  introspection: `addBlockedBy`/`removeBlockedBy`/`addSubIssue`/
  `removeSubIssue` all take `ID!` arguments, not issue numbers) plus a
  decision about what happens on orgs with native issue dependencies
  switched off (`read_relationships` already falls back to parsing
  `Depends on #N` from the issue body on the *read* side; the
  *write*-side equivalent — editing the issue body instead — needs an
  `update_issue` `Vcs` method this crate doesn't have either) —
  deferred rather than guessed at, and not actually needed by
  `conflict-check`'s own mechanic anyway, since that phase only
  *proposes* edges in a posted comment (already fully supported via
  [`Vcs::post_comment`]); an operator's separate `link` call is what
  actually confirms one, and no caller for it exists yet regardless.
- Step 10's cross-project fairness covers the *policy* (which project
  gets a freed slot) but not a per-project `max_concurrent` cap on top
  of it — §12 names this as an optional per-project tuning knob
  alongside `weight`, but enforcing it needs live in-flight spawn
  counts per project, which lives in `spawner::ProcessSpawner`'s
  `LocalProcess` map, a runtime concept nothing in this crate runs
  alongside the scheduler yet (the same "no caller ties these together"
  boundary `board.rs`'s module doc already draws for the worktree/
  per-agent columns it likewise omits).

- `server/` ships **eight of §6's ~24 MCP tools** — `register`
  (coordinator path only), every read-only tool under §6's "Board /
  status" heading (`board`, `issue`, `backlog`, `drift`), §6's
  "Approval & gates" write pair (`approve`, `set_gate`), and `advance`.
  The rest are *absent*, not stubbed, and enumerated one by one with
  their reasons in that module's own doc. Two of the eight are
  deliberately **half** a tool, each with its shortfall reported in a
  field of its own rather than left to be inferred:
  `approve`'s `deny` records the decision, the note, and the
  workflow's defined re-entry as a GitHub comment, and does not route
  the phase back to that re-entry — routing means spawning an agent
  (§6's `merge→address`; `address` is an agent phase, §7.2) or re-running
  a content phase, and no phase engine or spawner runs alongside the
  server. `advance` decides and writes the `status:<phase>` transition
  but never executes the phase it advances into — no spawn for an agent
  phase, **no merge enqueue** when the target is the merge gate (even
  though [`plan_merge`] exists as the pure function that would decide
  it), no `finalize` cleanup, since none of that mechanism exists here
  and a merge-shaped exception to "this server moves labels, the phase
  engine runs phases" would be the worst place to start given §2.3b's
  nothing-merges-by-accident invariant. `advance` also carries a second,
  different gap: §12 makes **roborev** a verdict an agent `report`s and
  this server has no `report` tool, so it evaluates every `{roborev:
  ...}` requirement with no verdict at all — which surfaces as that
  requirement being reported unmet, and means the shipped default's
  `review` phase (which requires `{roborev: clean}`) cannot currently be
  entered through this tool however green CI is. The one read-shaped tool still missing is
  **`next_assignment`**, and it stays missing on purpose: §6 defines it
  as the next content phase per §12's scheduling order, whose first tier
  is *actionability* (gate not parked, `requires` satisfied, not already
  claimed/running) — facts that come from the phase engine and the live
  [`ProcessSpawner`] map, neither of which runs alongside this server,
  and from cross-project scheduling context a connection bound to one
  project does not have. Handing an agent work it may not be clear to
  start is worse than not answering. The three arguments the server
  accepts but refuses (`board`'s `filter`, `backlog`'s `filter`,
  `register`'s `spawn_token`) are rejected rather than silently ignored,
  so no client can mistake an unfiltered list or a coordinator binding
  for what it asked for. `drift` likewise reports only the six drift
  checks `state::drift` actually implements and only the *claim* half of
  §6's "drift + contention" — the other three §12 drift kinds need
  worktree/phase bookkeeping that does not exist, and lease
  holders/queues need §8.3's lease model, still an open question (§16
  item 5). That doc also
  records the one **confirmed** external constraint this design rests
  on, read out of `rmcp` 3.1.0's own transport source rather than
  assumed: MCP revision `2026-07-28` removes sessions, and the streamable
  HTTP transport routes such a client statelessly (a fresh handler per
  request) *before* consulting any handler — so §15's "a connection is
  bound to exactly one project for its life" is unimplementable for it,
  and this server advertises only the session-bearing revisions. It
  also records a measured performance problem, not a theoretical one:
  `board` re-reads every open issue from GitHub on every call, one `gh`
  subprocess at a time — ~20s for a 16-issue repo, 49 subprocesses —
  because §2.3/§5's disposable local cache does not exist in this crate
  yet and a batched GraphQL read would be a new `Vcs` method needing its
  own live schema validation. `backlog` and `drift` inherit that same
  fan-out, measured on the same repo in the same run: 49 `gh` subprocesses
  and ~20s apiece for 16 open issues. `drift` additionally spends one `gh`
  read per off-board dependency, which that repo (no `blockedBy` edges)
  never exercised — `logic::read_drift`'s doc says exactly that rather
  than quoting a number nobody measured.

The crate now has an async runtime and an MCP crate (`tokio` + `rmcp`,
both entering at `server/`), but still no phase-engine wiring. In
particular, step 5 stops short of an actual polling loop: the `schedule`
module's doc records why a CI/PR poller's backoff timer is out of scope
until something drives it — an async runtime now exists, but `serve` is
purely request-driven and runs no background task at all. Step 6 stops
short of
authoring every injection point's real base-template content
(`instructions.rs`'s and `scaffold.rs`'s docs record exactly which two
points do, and which the rest still don't) and short of wiring the
composer into anything, since there is nothing to wire it into yet.
Step 7 (`workflow/`) stops short of actually spawning anything: it
computes what should happen next, never causes it to happen — see
`workflow`'s own module doc for the precise boundary, including the
un-*routed* `deny` re-entry (whose name `server/` now reports and whose
re-entry nothing fires) and the not-yet-implemented `workflows:` map
(§7.4's multiple-named-workflows extension point). `server/`'s `advance`
tool does not move that boundary: it executes [`advance`]'s decision as
far as the `status:*` label, and no further.
Step 8 (`merge.rs`) stops short of the serialized merge-lease fallback —
see that module's doc for exactly why. `server/` now reads from
`workflow` (`backlog` parses the project's committed `workflow.yaml` for
its readiness bar; `approve`/`set_gate` resolve their `phase` argument
against that same file and call [`approve`]/[`set_gate`] for the label
to write; `advance` walks that same file's phase list via [`advance`])
and from `state::drift` (`drift` orchestrates its six checks),
but still calls nothing in `claim`, `schedule`, `merge`, `implement`,
`instructions`, or `spawner` — `merge` in particular stays uncalled *by
design* even now that `advance` can move an issue toward the merge gate
(see that tool's row above). The three tools that do write (§6's
"Approval & gates" pair, plus `advance`) write **labels and comments on
an issue**, not claims: nothing in the server stamps an
`owner:<instance>@<epoch>`
label, so `claim.rs`'s still-unresolved label-preexistence conflict (see
its row above) remains untouched rather than newly load-bearing —
`gate:<phase>`, `approved:<phase>` and `status:<phase>` are fixed,
config-declared names `init` already provisions ([`label_vocabulary`]).
*/
#![deny(missing_docs)]

pub use crate::board::{
    BacklogRow, BoardRow, build_backlog, build_board, issue_url,
};
pub use crate::claim::{ClaimError, ClaimResult, confirm_claim, write_claim};
pub use crate::config::{
    Binaries, ClaimConfig, ConfigError, CrossProjectMode, GhConfig,
    GlobalConfig, HarnessConfig, HarnessesConfig, Limits, MergeMode,
    ProjectConfig, ProjectPointer, global_config_path, load_global_config,
    load_project_config, project_config_path, projects_config_dir,
    save_global_config, save_project_config,
};
pub use crate::implement::{
    SpecDecision, StartImplementSetup, finish_implement_setup,
    start_implement_setup,
};
pub use crate::init::{InitError, InitOptions, init};
pub use crate::instructions::{
    InstructionError, VAR_BRANCH, VAR_DEFAULT_BRANCH, VAR_ISSUE_NUMBER,
    VAR_SPECS_DIR, VAR_WORKTREE_PATH, compose, interpolate,
    read_instruction_override,
};
pub use crate::merge::{
    MergeAction, MergeObservation, observe_merge, plan_merge,
};
pub use crate::registry::{
    AddOutcome, add_project, find_project, find_project_containing,
    list_projects, remove_project,
};
pub use crate::scaffold::ScaffoldError;
pub use crate::schedule::{
    Candidate, DEFAULT_PHASE_ORDER, FairShareState, ProjectQueue,
    ScheduledIssue, next_action, next_action_across_projects, schedule_order,
};
pub use crate::server::{
    AdvanceArgs, AdvanceDecisionWire, AdvanceOutcome, AdvanceResult,
    ApproveArgs, ApproveDecisionWire, ApproveOutcome, ApproveResult,
    BOARD_ISSUE_LIMIT, BacklogArgs, BacklogResult, BacklogRowWire, BoardArgs,
    BoardResult, BoardRowWire, CiConclusionWire, ClaimHolder, ClaimHolderWire,
    ClaimWire, DriftFindingWire, DriftReport, DriftResult, GateModeWire,
    IssueArgs, IssueResult, IssueStateWire, MCP_PATH, PriorityWire,
    PullRequestStateWire, PullRequestStatusWire, RegisterArgs, RegisterResult,
    RelationshipsWire, RequirementWire, ServeError, SetGateArgs,
    SetGateOutcome, SetGateResult, SpecFlowServer, ToolError, mcp_service,
    serve,
};
pub use crate::spawner::{
    LocalProcessEntry, ProcessSpawner, SpawnError, SpawnKey, SpawnToken,
};
pub use crate::state::drift::{
    DriftFinding, IssueAndLinkedPullRequests, find_ambiguous_labels,
    find_closed_or_missing_dependencies, find_dependency_cycles,
    find_merged_pr_with_open_issue, find_multiple_open_linked_pull_requests,
    find_stale_claims,
};
pub use crate::state::{
    Claim, IssueSnapshot, Priority, derive_issue_state, read_issue_state,
};
pub use crate::vcs::{
    CiConclusion, FakeVcs, IssueRef, IssueRelationships, IssueState,
    PullRequestRef, PullRequestState, PullRequestStatus, ShellVcs, Vcs,
    VcsError, Worktree,
};
pub use crate::workflow::{
    AdvanceDecision, ApproveDecision, ArtifactKind, ArtifactSpec,
    EscalateReason, ExternalLensTag, FindingSeverity, FixLoopConfig,
    FixLoopPolicy, GateMode, LabelOp, LabelsConfig, LensResult, OnConflict,
    OpenSpecConfig, OutOfBandPhase, PanelOutcome, Phase, PhaseAction,
    Requirement, RequirementContext, ReviewFinding, ReviewLensConfig,
    ReviewLensSource, ReviewVerdict, RoborevConfig, RoborevMode, SpecConfig,
    Trigger, WorkflowConfig, WorkflowError, advance, approve,
    effective_gate_mode, evaluate_panel, gate_clear, handoff_ready_reached,
    is_phase_clear, label_vocabulary, parse_workflow,
    phase_requirements_satisfied, readiness_gap, requirement_satisfied,
    set_gate,
};

mod board;
mod claim;
mod config;
mod implement;
mod init;
mod instructions;
mod merge;
mod registry;
mod scaffold;
mod schedule;
mod server;
mod spawner;
mod state;
mod vcs;
mod workflow;
