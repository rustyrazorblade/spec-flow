# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`spec-flow` is a standalone, project-agnostic MCP daemon that owns the work queue and git/GitHub mechanics for AI-agent software delivery. It's a Rust binary (`spec-flow`) with two subcommands: `serve` (run the MCP daemon over HTTP/SSE) and `init` (register a repo and scaffold its `.spec-flow/` files).

The full specification this crate implements against is `/agent-fleet-manager-spec.md` (referenced throughout the code as "the spec", e.g. "§2.1", "§14 step 7") — that file is not present in this repo checkout; the spec calls the tool by its original working name, `fleet`.

**Before making any non-trivial change, read `src/lib.rs`'s crate-level doc comment in full.** It is a maintained module map — who owns which file, what's implemented, and exactly what's deliberately not built yet — and is kept current with every step. Do not trust a summary of it from an old conversation; re-read it fresh, since it changes with the codebase.

## Commands

```sh
cargo build --all-targets      # build lib + bin + tests
cargo test                     # run all tests (unit + tests/*.rs integration tests)
cargo test <name>              # run a single test by substring match
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check           # this repo's rustfmt.toml: max_width = 79
cargo doc --no-deps            # RUSTDOCFLAGS=-D warnings in CI — doc warnings fail the build
```

CI (`.github/workflows/ci.yml`) runs all of the above on every push/PR; match it locally before pushing. `#![deny(missing_docs)]` is set at the crate root — every public item needs a doc comment or the build fails.

Integration tests (`tests/shell_vcs_git.rs`) shell out to a real local `git` against a scratch directory (no network/auth). `tests/mcp_server.rs` stands up the real MCP service on an ephemeral loopback port and drives it with a real MCP client over HTTP; it points `ShellVcs` at a nonexistent `gh` binary rather than hitting a live repo, so it still proves the whole request path (bind, dispatch, arg decode, error mapping) without needing GitHub credentials.

## Architecture

The daemon performs deterministic mechanics itself — git worktree/branch lifecycle, commit, push, open PR, read issue, set labels, post comments, enqueue to the merge queue — by shelling out to the operator's local `git`/`gh` CLIs, and spawns short-lived agent processes for anything requiring judgment (grooming, design, writing code, reviewing a diff). **All durable state lives in GitHub** (issues, labels, comments, PRs); the only legitimately-local state is the installation/project registry (`~/.config/spec-flow/config.yaml`) and each project's committed `.spec-flow/` files.

### Module map

| Module | Owns |
|---|---|
| `config.rs` | `GlobalConfig` (`~/.config/spec-flow/config.yaml`) / `ProjectConfig` (`<project>/.spec-flow/config.yaml`) shapes, load/save |
| `registry.rs` | Idempotent add/list/find over the global config's `projects` list |
| `vcs/` | The `Vcs` trait — the one seam onto `git`/`gh` subprocesses. `shell::ShellVcs` (real), `fake::FakeVcs` (in-memory test double). Trait signatures are load-bearing for every call site; change them and every caller together |
| `state/` | The GitHub-state layer: `IssueSnapshot`, `derive_issue_state` (pure label parsing), `read_issue_state` (orchestrator), and `state::drift` (six pure drift/contention checks) |
| `init.rs` | `spec-flow init` business logic: composes `config`, `registry`, `scaffold`, an injected `Vcs`, and provisions the repo's label vocabulary |
| `scaffold.rs` | The committed `.spec-flow/` files `init` writes: default `workflow.yaml`, one `instructions/<point>.md` per injection point |
| `instructions.rs` | The instruction composer: `compose` (base template + optional override + variables → final prompt) and `interpolate` |
| `spawner/` | The `claude` process spawner + `LocalProcess` map: command-template interpolation, spawn/track/reap, same-instance double-spawn blocking |
| `claim.rs` | Work-claiming: `write_claim`/`confirm_claim` (two-step optimistic claim + settle-read); heartbeat refresh reuses `write_claim` |
| `schedule.rs` | Scheduling order: pure `next_action`/`schedule_order` (actionable-now → furthest-along → priority → age), plus cross-project fairness (`next_action_across_projects`, weighted round-robin or global priority pool) |
| `workflow/` | The workflow-config data model + parser (`WorkflowConfig` from `.spec-flow/workflow.yaml`), gate evaluation, `requires` evaluation, the review-panel/fix-loop primitive, and `advance`/`approve`/`set_gate` as pure functions over an already-known `IssueSnapshot` |
| `merge.rs` | Merge-queue integration: `plan_merge` (gate → enqueue, native mode only) and `observe_merge` |
| `implement.rs` | `start_implement` kickoff: claim the worktree, read the issue, decide spec-author-vs-skip; commit/push/post the dev plan |
| `board.rs` | Pure aggregation of `IssueSnapshot`s into `BoardRow`/`BacklogRow` for the `board`/`backlog` MCP tools |
| `server/` | The MCP daemon: `serve`, `mcp_service` (the `tower` service, mountable on any listener for tests), `SpecFlowServer` (one instance per MCP session), and the `*Wire`/`*Args`/`*Result` shapes. This is where `tokio` and `rmcp` enter the crate |
| `main.rs` | `clap` CLI wiring (`serve`, `init`), `tracing` setup. The only place in the crate using `anyhow` — see error-handling split below |

Every module is private; public items are re-exported at the crate root (`spec_flow::GlobalConfig`, not `spec_flow::config::GlobalConfig`) via the `pub use` block at the top of `src/lib.rs`.

### Design invariants worth knowing before you touch code

- **Error-handling split**: hand-written error enums (via `thiserror`) in the library, no `anyhow` in the lib's public API. `anyhow` is used only in `main.rs`, at the binary's top level.
- **Observability**: `tracing`, never `print!`/`println!`.
- **The `Vcs` trait is the only I/O seam to git/GitHub.** Pure decision logic (`schedule`, `workflow`, `state::derive_issue_state`, `board`) takes already-fetched data and returns a decision — no `Vcs`, no I/O — so it's unit-testable without a fake or a real `gh`. Keep new decision logic in that shape rather than reaching for `Vcs` directly.
- **A connection is bound to exactly one project for its life** (MCP `register` binds it; never rebindable). This relies on `rmcp`'s per-session service-factory behavior; it does not hold for MCP protocol revision `2026-07-28`, which removes sessions — `SpecFlowServer` deliberately advertises only session-bearing protocol revisions to avoid silently breaking that invariant. See `server/mod.rs`'s doc before touching protocol version negotiation.
- **§2.3b's "nothing merges by accident"**: `advance` writes the `status:<phase>` label transition but never executes the phase it advances into (no agent spawn, no merge enqueue, no `finalize`) — there is no phase engine wired up yet. Don't treat a moved label as "the phase ran".
- **`gh issue edit --add-label` cannot add a label that doesn't already exist in the repo.** This is why `init` provisions the full label vocabulary (`status:`/`gate:`/`approved:`/`spec:skip`) up front via `Vcs::ensure_label`, and why `claim.rs` has a documented, still-unresolved conflict: every heartbeat mints a new `owner:<instance>@<epoch>` label name, which this constraint makes impossible to pre-provision. Don't paper over this if you touch `claim.rs` — it's a real open design question, not a bug to silently fix.
- Target platforms are macOS + Linux only (`dist-workspace.toml`); Windows is untested and not a support goal today (this crate shells out to `git`/`gh` and manages worktrees/paths in ways nobody has verified under Windows semantics).

### What's not built yet

`src/lib.rs`'s doc comment maintains an exact, current list of deferred work (which of the ~24 spec §6 MCP tools are missing and why, the phase engine, the CI/PR poller, the serialized merge-queue fallback, etc.) — check it rather than assuming a gap is accidental. Most "missing" pieces are deliberate, documented stopping points, not oversights.
