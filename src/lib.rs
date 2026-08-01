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
| `vcs/` | The [`Vcs`] trait (the git/gh subprocess seam), [`ShellVcs`] (real), [`FakeVcs`] (test double) | Fully implemented (step 1) — no `todo!()`s remain in `vcs/shell.rs`. `vcs/mod.rs`'s trait signatures are load-bearing for both implementations and every call site; change them and every caller together. |
| `init.rs` | The `spec-flow init` business logic ([`init`]) | Implemented (step 1): composes `config`, `registry`, `scaffold`, and an injected [`Vcs`]. |
| `scaffold.rs` | The committed `.spec-flow/` files `init` materializes: the default `workflow.yaml` (§11.3) and one `instructions/<point>.md` per injection point (§9.1) | Implemented (step 1); the instruction files hold placeholders — authoring real base templates is the instruction composer's job (§14 step 6). |
| `spawner/` | The `claude` process spawner + `LocalProcess` map ([`ProcessSpawner`]): command-template interpolation, spawn/track/reap, same-instance double-spawn blocking (§2.6, §4.2, §5) | Implemented (step 3). No MCP server or phase engine exists yet, so nothing in this crate calls it — a later step (§14 step 6+) wires it into the phase engine. |
| `main.rs` (binary, not part of this library) | `clap` CLI wiring, `tracing` setup, `anyhow` error reporting at the top level | `init` is fully wired (step 1); the `serve` subcommand (the MCP server itself) is a stub — out of scope until spec §14 step 6+. |

# What is deliberately *not* here yet

This crate currently covers §14 steps 1–3 (registry/config, the git/gh
layer, `init`, the process spawner) plus the step-2 spike recorded in
`docs/memory-index-spike.md` (no code needed there). It still has no
async runtime, no MCP crate, and no GitHub-state layer / scheduler /
phase engine — those are steps 4–10. When the MCP server itself is
built (step 6+), the official Rust SDK is
[`rmcp`](https://github.com/modelcontextprotocol/rust-sdk); pull it in
(with `tokio`) at that point, not before.
*/
#![deny(missing_docs)]

pub use crate::config::{
    Binaries, ClaimConfig, ConfigError, GhConfig, GlobalConfig, HarnessConfig,
    HarnessesConfig, Limits, MergeMode, ProjectConfig, ProjectPointer,
    global_config_path, load_global_config, load_project_config,
    project_config_path, save_global_config, save_project_config,
};
pub use crate::init::{InitError, InitOptions, init};
pub use crate::registry::{
    AddOutcome, add_project, find_project, find_project_containing,
    list_projects, remove_project,
};
pub use crate::scaffold::ScaffoldError;
pub use crate::spawner::{
    LocalProcessEntry, ProcessSpawner, SpawnError, SpawnKey, SpawnToken,
};
pub use crate::vcs::{
    FakeVcs, IssueRef, IssueRelationships, IssueState, PullRequestRef,
    ShellVcs, Vcs, VcsError, Worktree,
};

mod config;
mod init;
mod registry;
mod scaffold;
mod spawner;
mod vcs;
