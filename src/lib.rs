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

| File | Owns | Task |
|---|---|---|
| `config.rs` | [`GlobalConfig`]/[`ProjectConfig`] shapes, their file paths, load/save | #2: add the `instance_id` auto-generate-on-first-`serve` behavior; load/save themselves are already implemented |
| `registry.rs` | Idempotent add/list/find over the global config's `projects` list | #2 (already implemented; extend if the registry's needs grow) |
| `vcs/` | The [`Vcs`] trait (the git/gh subprocess seam), [`ShellVcs`] (real), [`FakeVcs`] (test double) | #3: fill in `ShellVcs`'s `todo!()` bodies in `vcs/shell.rs` — subprocess invocation, output parsing, `gh api graphql` query bodies. Do not touch `vcs/mod.rs`'s trait signatures without updating every caller; do not touch `vcs/fake.rs` unless the trait itself grows a method |
| `init.rs` | The `spec-flow init` business logic ([`init`]) | #4: composes `config`, `registry`, `scaffold`, and an injected [`Vcs`] |
| `scaffold.rs` | The committed `.spec-flow/` files `init` materializes: the default `workflow.yaml` (§11.3) and one `instructions/<point>.md` per injection point (§9.1) | #4: the instruction files hold placeholders — authoring real base templates is the instruction composer's step (§14 step 6) |
| `spawner/` | The `claude` process spawner + `LocalProcess` map ([`ProcessSpawner`]): command-template interpolation, spawn/track/reap, same-instance double-spawn blocking (§2.6, §4.2, §5) | #3 (this step); no MCP server or phase engine exists yet, so nothing in this crate calls it — a later task (§14 step 6+) wires it into the phase engine |
| `main.rs` (binary, not part of this library) | `clap` CLI wiring, `tracing` setup, `anyhow` error reporting at the top level | #4 for `init`'s wiring; the `serve` subcommand (the MCP server itself) is out of scope until spec §14 step 3+ |

# What is deliberately *not* here yet

Per spec §14 step 1's scope, this crate has no async runtime, no MCP
crate, and no phase engine / scheduler — those are later steps (§14
steps 4–10). When the MCP server itself is built (step 6+), the
official Rust SDK is
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
