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
| `vcs/` | The [`Vcs`] trait (the git/gh subprocess seam), [`ShellVcs`] (real), [`FakeVcs`] (test double) | Fully implemented (step 1) — no `todo!()`s remain in `vcs/shell.rs`. `vcs/mod.rs`'s trait signatures are load-bearing for both implementations and every call site; change them and every caller together. Step 4 added two methods for PR/CI/merge-queue signals: [`Vcs::find_linked_pull_requests`], [`Vcs::read_pull_request_status`]. |
| `state/` | The GitHub-state layer (§4.2, §14 step 4): [`IssueSnapshot`] (derived per-issue state), the pure [`state::derive_issue_state`] label-parsing function, the [`state::read_issue_state`] orchestrator, and drift detection (see [`DriftFinding`]) | Implemented (step 4) for what is derivable from GitHub state alone — dependency cycles, closed/missing dependencies, stale claims, a merged PR against a still-open issue, an issue carrying more than one `status:`/`owner:` label, and an issue with more than one simultaneously **open** linked PR. Deliberately does **not** cover the drift kinds needing the phase engine's or worktree manager's own bookkeeping (a status label disagreeing with reality, an orphaned worktree, an implemented branch with no PR) — those are a later step's job (§14 steps 5/7). |
| `init.rs` | The `spec-flow init` business logic ([`init`]) | Implemented (step 1): composes `config`, `registry`, `scaffold`, and an injected [`Vcs`]. |
| `scaffold.rs` | The committed `.spec-flow/` files `init` materializes: the default `workflow.yaml` (§11.3) and one `instructions/<point>.md` per injection point (§9.1) | Implemented (step 1). Base-template content (§14 step 6 Deliverable B) is **partial**: `groom` and `implement` now hold real, substantive templates distilled from this plugin's `product-manager`/`groom` and `tdd-developer`/`implement` agent/skill content; the remaining 17 injection points still hold the original thin placeholder — authoring those is unfinished work, not done here. |
| `instructions.rs` | The instruction composer's pure mechanism (§9.2, §14 step 6 Deliverable A): [`compose`] (base template + optional override + variables → final prompt, honoring the `<!-- mode: replace -->` directive) and [`interpolate`] (single-pass `{name}` substitution over a generic variable map); [`read_instruction_override`] reads a project's `.spec-flow/instructions/<point>.md` | Implemented (step 6) as a standalone, pure/file-I/O-split module with no caller yet — there is no phase engine to invoke it (§14 step 7+ wires it in). Does not resolve a point's base template text itself; callers pass it in (today, `scaffold`'s compiled-in seed strings are the only source of that text). |
| `spawner/` | The `claude` process spawner + `LocalProcess` map ([`ProcessSpawner`]): command-template interpolation, spawn/track/reap, same-instance double-spawn blocking (§2.6, §4.2, §5) | Implemented (step 3). No MCP server or phase engine exists yet, so nothing in this crate calls it — a later step (§14 step 6+) wires it into the phase engine. |
| `claim.rs` | Work-claiming (§8.2, §14 step 5): [`write_claim`]/[`confirm_claim`], the two-step optimistic claim + settle-read; heartbeat refresh and an instance's own stale-claim reclaim are just `write_claim` called again | Implemented (step 5) against the [`Vcs`] trait's existing `read_issue`/`set_label` — no new `Vcs` method was needed. Staleness itself is [`state::drift::find_stale_claims`]'s job, not this module's. **Unresolved architectural conflict, confirmed not just suspected** (see `vcs::shell::ShellVcs::set_label`'s doc): `gh issue edit --add-label` cannot add a label name that doesn't already exist in the repo, but every `write_claim` heartbeat mints a brand-new `owner:<instance>@<epoch>` label name — the claim protocol as specified (§8.2) cannot work against real GitHub through this transport as shipped. Needs a design decision before `serve` (§14 step 6+) relies on it. |
| `schedule.rs` | The scheduler's default ordering (§12, §14 step 5): the pure [`schedule::next_action`]/[`schedule::schedule_order`] functions (actionable-now → furthest-along → priority → age) | Implemented (step 5) over the shipped-default [`schedule::DEFAULT_PHASE_ORDER`] (§7.2) — see its module doc for what's deferred: a workflow-config-defined phase order, a "dependency" tie-break ahead of age, and the CI/PR poller's backoff loop (all §14 step 6+ or later). |
| `main.rs` (binary, not part of this library) | `clap` CLI wiring, `tracing` setup, `anyhow` error reporting at the top level | `init` is fully wired (step 1); the `serve` subcommand (the MCP server itself) is a stub — out of scope until spec §14 step 6+. |

# What is deliberately *not* here yet

This crate currently covers §14 steps 1–6 (registry/config, the git/gh
layer, `init`, the process spawner, the GitHub-state layer,
work-claiming and the scheduler's ordering, and now the instruction
composer's pure mechanism) plus the step-2 spike recorded in
`docs/memory-index-spike.md` (no code needed there). It still has no
async runtime, no MCP crate, and no phase engine — those are steps 7+.
In particular, step 5 stops short of an actual polling loop: the
`schedule` module's doc records why a CI/PR poller's backoff timer is
out of scope until an async runtime exists. Step 6 stops short of
authoring every injection point's real base-template content
(`instructions.rs`'s and `scaffold.rs`'s docs record exactly which two
points do, and which the rest still don't) and short of wiring the
composer into anything, since there is nothing to wire it into yet.
When the MCP server itself is built (step 7+), the official Rust SDK is
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk); pull it in
(with `tokio`) at that point, not before.
*/
#![deny(missing_docs)]

pub use crate::claim::{ClaimError, ClaimResult, confirm_claim, write_claim};
pub use crate::config::{
    Binaries, ClaimConfig, ConfigError, GhConfig, GlobalConfig, HarnessConfig,
    HarnessesConfig, Limits, MergeMode, ProjectConfig, ProjectPointer,
    global_config_path, load_global_config, load_project_config,
    project_config_path, save_global_config, save_project_config,
};
pub use crate::init::{InitError, InitOptions, init};
pub use crate::instructions::{
    InstructionError, VAR_BRANCH, VAR_DEFAULT_BRANCH, VAR_ISSUE_NUMBER,
    VAR_SPECS_DIR, VAR_WORKTREE_PATH, compose, interpolate,
    read_instruction_override,
};
pub use crate::registry::{
    AddOutcome, add_project, find_project, find_project_containing,
    list_projects, remove_project,
};
pub use crate::scaffold::ScaffoldError;
pub use crate::schedule::{
    Candidate, DEFAULT_PHASE_ORDER, next_action, schedule_order,
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

mod claim;
mod config;
mod init;
mod instructions;
mod registry;
mod scaffold;
mod schedule;
mod spawner;
mod state;
mod vcs;
