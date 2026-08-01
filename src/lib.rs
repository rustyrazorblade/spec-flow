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
| `config.rs` | [`GlobalConfig`]/[`ProjectConfig`] shapes, their file paths, load/save | Implemented (step 1). `instance_id` auto-generation on first `serve` is still open — `serve` itself doesn't exist yet. |
| `registry.rs` | Idempotent add/list/find over the global config's `projects` list | Implemented (step 1). |
| `vcs/` | The [`Vcs`] trait (the git/gh subprocess seam), [`ShellVcs`] (real), [`FakeVcs`] (test double) | Fully implemented (step 1) — no `todo!()`s remain in `vcs/shell.rs`. `vcs/mod.rs`'s trait signatures are load-bearing for both implementations and every call site; change them and every caller together. Step 4 added two methods for PR/CI/merge-queue signals: [`Vcs::find_linked_pull_requests`], [`Vcs::read_pull_request_status`]. Step 9 added [`Vcs::ensure_label`] (create a label if absent) to close a real gap: `set_label` cannot add a label that doesn't already exist in the repo, which blocked every fixed-vocabulary `status:`/`gate:`/`approved:` label, not only `claim.rs`'s already-documented per-heartbeat one; and [`Vcs::create_issue`] (§7.2 ph.1's "the server creates the GitHub issue" mechanic, e.g. for `groom`), which reads the freshly created issue back by number rather than parsing anything beyond that number out of `gh issue create`'s URL output, mirroring [`Vcs::open_pr`]'s "create, then view by a stable key" split. |
| `state/` | The GitHub-state layer (§4.2, §14 step 4): [`IssueSnapshot`] (derived per-issue state), the pure [`state::derive_issue_state`] label-parsing function, the [`state::read_issue_state`] orchestrator, and drift detection (see [`DriftFinding`]) | Implemented (step 4) for what is derivable from GitHub state alone — dependency cycles, closed/missing dependencies, stale claims, a merged PR against a still-open issue, an issue carrying more than one `status:`/`owner:` label, and an issue with more than one simultaneously **open** linked PR. Deliberately does **not** cover the drift kinds needing the phase engine's or worktree manager's own bookkeeping (a status label disagreeing with reality, an orphaned worktree, an implemented branch with no PR) — those are a later step's job (§14 steps 5/7). |
| `init.rs` | The `spec-flow init` business logic ([`init`]) | Implemented (step 1): composes `config`, `registry`, `scaffold`, and an injected [`Vcs`]. Step 9 added label-vocabulary provisioning: after scaffolding, `init` reads the repo's *effective* `.spec-flow/workflow.yaml` back off disk (a team's hand-edit, not always the compiled-in default), derives every label name [`crate::workflow::label_vocabulary`] returns, and calls [`Vcs::ensure_label`] for each — closing the gap `vcs/`'s row above describes. |
| `scaffold.rs` | The committed `.spec-flow/` files `init` materializes: the default `workflow.yaml` (§11.3) and one `instructions/<point>.md` per injection point (§9.1) | Implemented (step 1). Base-template content (§14 step 6 Deliverable B) is **partial**: `groom` and `implement` now hold real, substantive templates distilled from this plugin's `product-manager`/`groom` and `tdd-developer`/`implement` agent/skill content; the remaining 17 injection points still hold the original thin placeholder — authoring those is unfinished work, not done here. |
| `instructions.rs` | The instruction composer's pure mechanism (§9.2, §14 step 6 Deliverable A): [`compose`] (base template + optional override + variables → final prompt, honoring the `<!-- mode: replace -->` directive) and [`interpolate`] (single-pass `{name}` substitution over a generic variable map); [`read_instruction_override`] reads a project's `.spec-flow/instructions/<point>.md` | Implemented (step 6) as a standalone, pure/file-I/O-split module with no caller yet — there is no phase engine to invoke it (§14 step 7+ wires it in). Does not resolve a point's base template text itself; callers pass it in (today, `scaffold`'s compiled-in seed strings are the only source of that text). |
| `spawner/` | The `claude` process spawner + `LocalProcess` map ([`ProcessSpawner`]): command-template interpolation, spawn/track/reap, same-instance double-spawn blocking (§2.6, §4.2, §5) | Implemented (step 3). No MCP server or phase engine exists yet, so nothing in this crate calls it — a later step (§14 step 6+) wires it into the phase engine. |
| `claim.rs` | Work-claiming (§8.2, §14 step 5): [`write_claim`]/[`confirm_claim`], the two-step optimistic claim + settle-read; heartbeat refresh and an instance's own stale-claim reclaim are just `write_claim` called again | Implemented (step 5) against the [`Vcs`] trait's existing `read_issue`/`set_label` — no new `Vcs` method was needed. Staleness itself is [`state::drift::find_stale_claims`]'s job, not this module's. **Unresolved architectural conflict, confirmed not just suspected** (see `vcs::shell::ShellVcs::set_label`'s doc): `gh issue edit --add-label` cannot add a label name that doesn't already exist in the repo, but every `write_claim` heartbeat mints a brand-new `owner:<instance>@<epoch>` label name — the claim protocol as specified (§8.2) cannot work against real GitHub through this transport as shipped. Needs a design decision before `serve` (§14 step 6+) relies on it. |
| `schedule.rs` | The scheduler's default ordering (§12, §14 step 5): the pure [`schedule::next_action`]/[`schedule::schedule_order`] functions (actionable-now → furthest-along → priority → age) | Implemented (step 5) over the shipped-default [`schedule::DEFAULT_PHASE_ORDER`] (§7.2) — see its module doc for what's deferred: a workflow-config-defined phase order, a "dependency" tie-break ahead of age, and the CI/PR poller's backoff loop (all §14 step 6+ or later). Step 10 added the cross-project layer on top: [`schedule::next_action_across_projects`] picks *which project* gets a freed spawn slot before applying the existing within-project order — smooth weighted round-robin ([`schedule::FairShareState`], [`CrossProjectMode::FairShare`], the shipped default) or a flat global ranking across every project's issues ([`CrossProjectMode::GlobalPriorityPool`]). Needed two new [`ProjectConfig`]/[`GlobalConfig`] fields ([`ProjectConfig::weight`], [`GlobalConfig::cross_project_mode`]). |
| `workflow/` | The workflow-config data model + parser (§7, §14 step 7): [`workflow::WorkflowConfig`] and its nested types, deserialized straight from `scaffold::DEFAULT_WORKFLOW_YAML`; gate evaluation ([`workflow::gate_clear`] and friends, §2.3b/§4.3); `requires` evaluation ([`workflow::requirement_satisfied`], §7.1); the review-panel/bounded-fix-loop decision primitive ([`workflow::evaluate_panel`], §7.1); and `advance`/`approve`/`set_gate` as plain, pure functions (§6) | Implemented (step 7) as pure data + pure functions only — see the module's own doc for exactly what is and is not covered; no phase engine, spawner wiring, or MCP server calls any of it yet. Step 9 added [`workflow::label_vocabulary`] (every GitHub label name the config actually uses), consumed by `init`. |
| `merge.rs` | Merge-queue integration (§8.1, §14 step 8): [`merge::plan_merge`] (gate → enqueue, native mode) and [`merge::observe_merge`] (observe) as pure functions | Implemented (step 8) for the **native** merge queue only — see the module's own doc for why the serialized fallback (§8.1's warn-and-degrade path) is a documented, deliberately unresolved gap rather than a guess: it hinges on §8.3's lease model, itself an open question (§16 item 5). `init`'s merge-queue-enabled check (this step's other named deliverable) was already implemented in step 1 (`init::detect_merge_mode`); nothing changed there. |
| `implement.rs` | `start_implement` kickoff (§10.2, §14 step 9): [`start_implement_setup`] (claim the worktree, read the issue, decide spec-author-vs-skip) and [`finish_implement_setup`] (commit the spec if one was authored, push, post the dev plan) as two `Vcs`-touching orchestration functions | Implemented (step 9) for the mechanics either side of the spawned spec-authoring agent §10.2 places between them — the actual spawn is a future step's job (no spawner wiring exists yet). Does not itself claim the issue; that's `crate::claim`'s job, called before this module. |
| `board.rs` | Board rendering (§13, §14 step 9): [`build_board`], a pure aggregation of [`IssueSnapshot`]s into [`BoardRow`]s, and [`issue_url`] (deterministically derived, no `Vcs` round trip) | Implemented (step 9) for every column derivable from GitHub state alone (phase, owner, gates/approvals, PR signals) — see the module's own doc for why the **worktree** and per-agent columns §13 also names are deliberately absent: that data lives in `spawner::ProcessSpawner`'s in-memory map, which nothing in this crate runs alongside the GitHub-state layer yet. |
| `main.rs` (binary, not part of this library) | `clap` CLI wiring, `tracing` setup, `anyhow` error reporting at the top level | `init` is fully wired (step 1); the `serve` subcommand (the MCP server itself) is a stub — out of scope until spec §14 step 6+. |

# What is deliberately *not* here yet

This crate currently covers all ten §14 build-sequence steps
(registry/config, the git/gh layer, `init`, the process spawner, the
GitHub-state layer, work-claiming and the scheduler's ordering, the
instruction composer's pure mechanism, the workflow-config parser plus
the phase engine's pure decision logic, native merge-queue integration,
and cross-project scheduling fairness) plus the step-2 spike recorded
in `docs/memory-index-spike.md` (no code needed there) — **minus two
deliberately deferred pieces, both documented at their own module,
neither guessed at**:

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

It still has no async runtime, no MCP crate, and no actual phase engine
wiring. In particular, step 5 stops short of an actual polling loop:
the `schedule` module's doc records why a CI/PR poller's backoff timer
is out of scope until an async runtime exists. Step 6 stops short of
authoring every injection point's real base-template content
(`instructions.rs`'s and `scaffold.rs`'s docs record exactly which two
points do, and which the rest still don't) and short of wiring the
composer into anything, since there is nothing to wire it into yet.
Step 7 (`workflow/`) stops short of actually spawning anything: it
computes what should happen next, never causes it to happen — see
`workflow`'s own module doc for the precise boundary, including the
unmodeled `deny` re-entry routing and the not-yet-implemented
`workflows:` map (§7.4's multiple-named-workflows extension point).
Step 8 (`merge.rs`) stops short of the serialized merge-lease fallback —
see that module's doc for exactly why. When the MCP server itself is
built, the official Rust SDK is
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk); pull it in
(with `tokio`) at that point, not before.
*/
#![deny(missing_docs)]

pub use crate::board::{BoardRow, build_board, issue_url};
pub use crate::claim::{ClaimError, ClaimResult, confirm_claim, write_claim};
pub use crate::config::{
    Binaries, ClaimConfig, ConfigError, CrossProjectMode, GhConfig,
    GlobalConfig, HarnessConfig, HarnessesConfig, Limits, MergeMode,
    ProjectConfig, ProjectPointer, global_config_path, load_global_config,
    load_project_config, project_config_path, save_global_config,
    save_project_config,
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
    effective_gate_mode, evaluate_panel, gate_clear, is_phase_clear,
    label_vocabulary, parse_workflow, phase_requirements_satisfied,
    requirement_satisfied, set_gate,
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
mod spawner;
mod state;
mod vcs;
mod workflow;
