# Spike: shared memory + context index across worktrees (§10.3, §14 step 2)

**Question.** Each spawned phase process runs with `cwd` set to its own
issue worktree (`<project_dir>/<branch>/`, §10.1), never the primary
checkout. §10.3 requires one shared memory store and one shared context
index *per repo*, common across every worktree. Can a spawned Claude
Code process be pointed at a shared location purely via
instruction/config, or does the daemon need a thin integration?

**Method.** Verified directly against the installed `claude` binary
(2.1.220) and the installed `@zilliz/claude-context-mcp` /
`@zilliz/claude-context-core` packages (the MCP server providing
`index_codebase`/`search_code`/`get_indexing_status`/`clear_index`) —
`strings` on the compiled CLI binary, reading the `claude-context-core`
source for `getCollectionName`, and inspecting real `~/.claude/projects/`
state from existing worktrees on this machine (`cqlite` main checkout
vs. its `issue-2389-commitlog-reader` worktree). Not taken on faith from
a first-pass research pass, which turned out to contain one materially
wrong claim (below) alongside correct ones.

## Finding 1 — auto-memory is NOT shared across worktrees by default

Disproven by on-disk evidence, not just docs: `~/.claude/projects/`
contains **separate** directories for `-Users-jhaddad-dev-cqlite` and
`-Users-jhaddad-dev-cqlite--claude-worktrees-issue-2389-commitlog-reader`
— two worktrees of the same repo, two unrelated project-state
directories, hence two separate auto-memory directories. Project scope
is keyed on the literal launch `cwd`, not on the git repository root.
(A prior research pass claimed the opposite — that memory is
"git-repo-derived" and therefore automatically shared. That claim does
not hold up against this machine's actual state and should not be
trusted; treat it as corrected here.)

**The override exists and is real.** The literal setting
`autoMemoryDirectory` (paired with `autoMemoryEnabled`) is present in
the compiled CLI (confirmed via `strings` on the binary, not inferred).
It is loadable the same way any setting is: a `.claude/settings.json`
in the worktree, or inline via the CLI's `--settings <json>` flag —
which is exactly the flag spec-flow's process spawner already needs for
other reasons (§11.1 `harnesses`).

**Recipe:** when composing a phase's spawn command, add
`--settings '{"autoMemoryDirectory": "<memory_scope path>"}'` (or write
it into the worktree's `.claude/settings.json` before spawning) using
one fixed, operator-chosen directory per repo — `ProjectConfig::
memory_scope` (already modeled in `config.rs`) is the natural id to
derive that path from. **Not independently confirmed:** the exact
read/write semantics once two concurrent sessions point at the same
directory (e.g. whether concurrent writes need serialization). Worth a
five-minute live smoke test — two `claude -p` invocations from two
different worktrees, same `autoMemoryDirectory`, confirm both see the
same memory file — before this is load-bearing, but no daemon code is
needed beyond passing the setting.

## Finding 2 — the context index cannot be collapsed via config, but doesn't need to be

Confirmed by reading `claude-context-core`'s `Context.getCollectionName`
(`node_modules/@zilliz/claude-context-core/dist/context.js`): the
Milvus collection name is *always* suffixed with an md5 hash of the
literal `codebasePath` argument, **even when** a `collectionNameOverride`
config value or `CODE_CHUNKS_COLLECTION_NAME_OVERRIDE` env var is set —
the source comment states this is deliberate, "so that multiple
codebases indexed by the same MCP server can't collapse into one
collection." So: no config/env knob can make two different paths share
one collection. On this specific point, the prior research pass had it
right.

**But this doesn't require a daemon integration**, because the relevant
MCP tools (`index_codebase`, `search_code`, `get_indexing_status`,
`clear_index`, per `claude-context-mcp/dist/index.js`'s tool schemas)
all declare `path` as a **required** argument — there is no silent
fallback to the process's own `cwd()` for the codebase being indexed or
searched. Since the agent must always pass `path` explicitly, the fix
is pure instruction: tell every spawned phase, in its composed prompt
(exactly the mechanism §10.3 already describes), to always call these
tools with `path = <index_path from config>` — the primary checkout,
`ProjectConfig::index_path` (already modeled in `config.rs`) — never its
own worktree path. Every worktree's agent then resolves to the same
collection by construction, because they all pass the same string.

## Conclusion

**No thin integration is justified for either requirement.** Both are
achievable purely through what the spec already planned: composing
`memory_scope`/`index_path` into each spawned phase's settings/prompt
(§10.3, §14 step 6's instruction composer). The one piece of new,
concrete information this spike adds is *how*:

- Memory: `--settings '{"autoMemoryDirectory": "<memory_scope>"}'` at
  spawn time.
- Index: an explicit instruction line in the composed prompt directing
  the agent to pass `path=<index_path>` on every `claude-context` tool
  call, never a bare/implicit path.

Both recipes are cheap to encode once the instruction composer (§14
step 6) and process spawner (§14 step 3) exist; this crate does not yet
have either, so there is no code to write here. This document is the
spike's artifact per §14's sequencing note ("prove the shared-memory +
shared-index recipe... highest-risk unknown") — carry it forward into
those two steps' implementations, and run the live smoke test noted
under Finding 1 before depending on it in production.
