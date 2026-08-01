//! The committed `.spec-flow/` files `spec-flow init` materializes into
//! a repo (§9.1, §9.2, §11.3 — the spec calls this directory `.fleet/`
//! under its original working name; this crate ships as `spec-flow`, so
//! the committed directory is named to match).
//!
//! Two kinds of file, both team-shared and checked into the project's
//! git repository (unlike the machine-local config files `config.rs`
//! owns):
//!
//! - `.spec-flow/workflow.yaml` — the whole delivery pipeline as
//!   editable plain text (§11.3), pre-filled with the spec-flow-sourced
//!   default.
//! - `.spec-flow/instructions/<point>.md` — one file per instruction
//!   injection point (§9.1), so every prompt the daemon composes is
//!   overridable in the repo (§9.2).
//!
//! Scaffolding is **never destructive**: a file that already exists is
//! left exactly as the team edited it (§6 `init`, §11.1). That is the
//! whole reason this module writes with `create_new` rather than
//! comparing contents — the check and the write are one atomic step, so
//! a concurrent `init` can never overwrite an edit it raced with.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Errors writing the `.spec-flow/` scaffolding.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScaffoldError {
    /// A scaffolded file, or a directory holding one, could not be
    /// created.
    #[error("failed to write {path}")]
    Write {
        /// The path that could not be written.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

/// The default `.spec-flow/workflow.yaml`, verbatim from spec §11.3.
///
/// Kept as text rather than built from serde types on purpose: the
/// comments are half the value of the file a team is meant to edit.
/// [`crate::workflow`] (§14 step 7) does now parse this text into typed
/// shapes — see [`crate::workflow::parse_workflow`] — but this constant
/// stays the hand-authored source of truth either way; nothing
/// serializes a [`crate::workflow::WorkflowConfig`] back into it.
pub(crate) const DEFAULT_WORKFLOW_YAML: &str = r#"labels:
  priority: [P0, P1, P2, P3]
  status:   [product-spec, conflict-check, architecture, test-plan, ready, implement, review, done]
  gate_prefix:     "gate:"        # gate:<phase> = this phase needs a human on this issue (§4.3)
  approval_prefix: "approved:"    # approved:<phase>
  owner_prefix:    "owner:"       # owner:<instance> + heartbeat (§8.2)

roborev:
  enabled: true
  mode: gate            # gate = hard `requires` · advisory = surfaced only. Omit block → off.

spec:                          # spec tooling used to AUTO-WRITE the spec in the worktree at implement (§7.2 ph.6)
  tool: openspec               # openspec | specs_dir | <custom> — CONFIGURABLE; default OpenSpec (detected by init)
  openspec: {enabled: true}    # when tool=openspec: explore+propose in the worktree
  specs_dir: specs/            # when tool=specs_dir: a spec file under this dir
  optional: true               # allow a PER-ISSUE spec skip via the label below (§7.2 ph.6, §10.2)
  skip_label: "spec:skip"      # present on the issue → implement does branch + PR only, no spec authored;
                               # the server posts a comment recording the skip (audit trail, §2.3)

# Review lenses (spec-flow's 5-lens panel, §2.6); run in parallel each round, all must approve.
# A lens is INTERNAL (server spawns it) or EXTERNAL (a configured outside reviewer posts its verdict
# on the issue/PR; the server reads it via `gh` — §7.4). The fix loop waits for ALL of them.
review_panel:
  - {lens: spec,             role: reviewer}
  - {lens: code-review,      skill: code-review}
  - {lens: security-review,  skill: security-review}
  - {lens: test-rigor,       role: test-rigor-reviewer}
  - {lens: observability,    role: observability-reviewer}
  # - {lens: external-audit, type: external, source: "review-comment"}   # external reviewer (§7.4)

fix_loop: {max_rounds: 3, on_exhausted: escalate, on_decision_finding: escalate}   # §7.1, §7.2 phase 6

# gate: none | human | auto  (§2.3b). Config sets the DEFAULT; the orchestrator stamps gate:<phase>
# labels for each `human` phase at issue creation; the operator adds/removes them per issue (§4.3).
phases:
  - {id: product-spec,   action: {type: agent, role: product-manager, interactive: true}, artifact: {kind: issue}, gate: human, approval_label: "approved:product-spec", status_label: product-spec}
  - {id: conflict-check, action: {type: agent, role: spec-conflict},  artifact: {kind: issue_comment, title: "Spec conflict check"}, gate: none, on_conflict: escalate, status_label: conflict-check}
  - {id: architecture,   action: {type: agent, role: architect},      artifact: {kind: issue_comment, title: "Architecture + trade-offs"}, gate: human, approval_label: "approved:architecture", status_label: architecture}
  - {id: test-plan,      action: {type: agent, role: test-planner},   artifact: {kind: issue_comment, title: "Test plan"}, gate: human, approval_label: "approved:test-plan", status_label: test-plan}
  # No standalone spec phase: the approved issue IS the contract (§7.2 ph.5). When product-spec +
  # architecture + test-plan are all approved, the server marks the issue `status:ready` for handoff.
  - {id: implement,      action: {type: server_then_agent, op: start_implement, spec_tool: openspec, role: developer, panel: review_panel, fix_loop: fix_loop, then: build-engineer}, requires: ["approved:product-spec", "approved:architecture", "approved:test-plan"], gate: none, status_label: implement}
  - {id: review,         action: {type: server, op: merge_queue},     requires: [{ci: green}, {roborev: clean}, deps_merged], gate: human, approval_label: "approved:merge", status_label: review}
  - {id: finalize,       action: {type: server, op: finalize},        gate: none, status_label: done}

out_of_band:
  # trigger: auto (poller/GitHub event fires it) | operator (explicit tool call only). Both are always
  # callable explicitly via sync_ci()/address() (§6); `trigger` sets the DEFAULT auto-behavior.
  - {id: sync-ci, action: {type: server, op: sync_ci},      reenters: implement, trigger: auto}      # red CI → flag tests
  - {id: address, action: {type: agent,  role: developer},  reenters: review,    trigger: operator}   # PR review comments
"#;

/// Every instruction injection point (§9.1) paired with the one-line
/// purpose its seeded file records.
///
/// The dynamic `lease:<resource>` point is deliberately absent: it is
/// named after a lease declared in `.spec-flow/workflow.yaml`, and this
/// crate has no lease model to enumerate from yet (§8.3, §11.2).
pub(crate) const INSTRUCTION_POINTS: &[(&str, &str)] = &[
    ("setup", "one-time project setup for a spawned agent"),
    ("groom", "the interactive product-spec dialogue (§7.2 ph.1)"),
    ("conflict-check", "checking a product spec for conflicts (ph.2)"),
    ("architecture", "proposing architecture options (ph.3)"),
    ("test-plan", "writing the test plan (ph.4)"),
    ("start-implement", "kicking off implement in a worktree (§10.2)"),
    ("write-spec", "auto-writing the spec into the worktree (ph.6)"),
    ("develop-plan", "posting the development plan (§10.2)"),
    ("implement", "implementing the issue test-first (ph.6)"),
    ("review:spec", "the spec review lens (§2.6)"),
    ("review:code-review", "the code-review lens (§2.6)"),
    ("review:security-review", "the security-review lens (§2.6)"),
    ("review:test-rigor", "the test-rigor lens (§2.6)"),
    ("review:observability", "the observability lens (§2.6)"),
    ("fix", "the bounded fix loop (§7.1)"),
    ("sync-ci", "guarding tests a red CI run flagged (§7.2)"),
    ("address", "resolving PR review comments (§7.2)"),
    ("merge", "the merge gate (§8.1)"),
    ("finalize", "spec sync and cleanup after merge (ph.8)"),
];

/// Materialize `<repo_dir>/.spec-flow/` — `workflow.yaml` plus one
/// `instructions/<point>.md` per injection point — creating only the
/// files that do not exist yet (§6 `init`, §9.2).
///
/// Idempotent by construction: an already-present file is left byte-for-
/// byte as the team committed it.
pub(crate) fn scaffold_spec_flow_dir(
    repo_dir: &Path,
) -> Result<(), ScaffoldError> {
    let spec_flow_dir = repo_dir.join(".spec-flow");
    let instructions_dir = spec_flow_dir.join("instructions");
    create_dir(&instructions_dir)?;

    create_new_file(
        &spec_flow_dir.join("workflow.yaml"),
        DEFAULT_WORKFLOW_YAML,
    )?;
    for (point, purpose) in INSTRUCTION_POINTS {
        let path = instructions_dir.join(format!("{point}.md"));
        create_new_file(&path, &instruction_file_seed(point, purpose))?;
    }
    Ok(())
}

/// One instruction point's compiled-in base template text — what
/// `crate::instructions::compose`'s `base` argument is for this point
/// (§9.1, §9.2).
///
/// As of §14 step 6, exactly two points have a real, substantive base
/// template distilled from this plugin's own agent/skill content:
/// `groom` (from the `product-manager` agent and `groom` skill) and
/// `implement` (from the `tdd-developer` agent and `implement` skill).
/// Every other point still falls back to the thin
/// [`instruction_placeholder`] — composing the rest is future work, not
/// yet done, and this function is exactly where that work continues.
fn instruction_seed_text(point: &str, purpose: &str) -> String {
    match point {
        "groom" => GROOM_BASE_TEMPLATE.to_string(),
        "implement" => IMPLEMENT_BASE_TEMPLATE.to_string(),
        _ => instruction_placeholder(point, purpose),
    }
}

/// The **file** content `init` writes to a fresh
/// `.spec-flow/instructions/<point>.md` (§9.1, §9.2) — distinct from
/// [`instruction_seed_text`], which is the point's *base template* text
/// (what `compose`'s `base` argument is).
///
/// Prepends [`crate::instructions::REPLACE_DIRECTIVE`] as the file's
/// first line, so an unedited, freshly-scaffolded file starts in
/// `replace` mode over an exact copy of the base template. This matters
/// because `compose`'s *default* mode is `append`: without the
/// directive, composing a fresh checkout's untouched override file
/// against its own base would append the base to itself verbatim — the
/// spawned agent would receive every base template twice, and §9.2's
/// promise that "a team edits the real default in place" would
/// silently fail the moment they actually edited it (their edit would
/// land *after* an unedited copy of the original, not replace it). With
/// the directive, an untouched file composes to exactly the base text
/// (no duplication — see this module's
/// `scaffolded_instruction_file_composes_without_duplicating_the_base`
/// test), and a team's edit fully takes effect.
///
/// One consequence worth recording for whoever builds the §9.4
/// precedence chain (base ← workflow-config inline overrides ←
/// `.spec-flow/instructions/<point>.md`, most specific wins): because
/// every fresh scaffold ships in `replace` mode by default, an
/// untouched file will shadow a workflow-config inline override the
/// moment that middle tier exists, not just a hand-written one. That's
/// an accepted trade-off of resolving the seed-with-base vs.
/// append-default tension above, not an oversight — but it means the
/// precedence chain's real caller cannot simply skip reading the
/// override file when a workflow-config override is present; it must
/// still resolve which of the two actually wins.
fn instruction_file_seed(point: &str, purpose: &str) -> String {
    format!(
        "{}\n{}",
        crate::instructions::REPLACE_DIRECTIVE,
        instruction_seed_text(point, purpose)
    )
}

/// The base template for the `groom` instruction point (§9.1, §7.2
/// ph.1) — the interactive product-spec dialogue.
///
/// Distilled from this plugin's `agents/product-manager.md` and
/// `skills/groom/SKILL.md`, adapted to this point's actual scope in the
/// §7.2 pipeline: the `product-spec` phase produces the issue's
/// spec-shaped content itself (`artifact: {kind: issue}` in
/// `DEFAULT_WORKFLOW_YAML`), not — unlike the old `groom` skill this is
/// distilled from — the `gh issue create` call or the priority label,
/// both of which stay a human/orchestrator decision outside this
/// injection point.
const GROOM_BASE_TEMPLATE: &str = r#"# `groom` instructions

Injection point for the interactive product-spec dialogue (spec §9.1,
§7.2 ph.1). This phase is interactive and gated on human approval
(`approved:product-spec`) before the pipeline can move on.

You are acting as the delivery pipeline's **product manager** for issue
#{issue_number} (default branch `{default_branch}`). Turn the owner's
raw idea into a tight, well-scoped unit of work — a clear problem
statement, an honest scope boundary, and testable acceptance criteria.
You own the **what** and the **why**, never the **how**: leave data
models, interfaces, algorithms, and library choices to the
architecture phase that follows this one. You do not write code and you
do not set a priority label.

## What you produce

1. **Problem statement.** One or two sentences: what is wrong or
   missing, and why it matters to the user or operator. If the idea is
   a solution in search of a problem, say so and restate the underlying
   need.
2. **Scope — in and out.** What this work includes, and — just as
   important — what it explicitly excludes. Call out the tempting
   adjacent work that is NOT in scope so it doesn't creep in later.
3. **Acceptance criteria.** A checklist of observable outcomes, each
   written as a testable WHEN/THEN where you can ("WHEN a request
   exceeds the size limit, THEN it is rejected with a 413 and the limit
   in the message"). These seed the spec's scenarios at the
   `test-plan` phase, so make them concrete and verifiable, not vague
   goals. Cover the unhappy paths (errors, limits, empty/oversized
   input, conflicts), not just the success case.
4. **Open questions / assumptions.** Anything genuinely ambiguous that
   changes the scope, stated as a specific question paired with the
   assumption you would make if it goes unanswered. Keep this short —
   prefer a sensible default the owner can correct over a long
   interrogation.
5. **Context.** Related code (`file:line`), related issues,
   constraints, prior art. Search the repo and the issue tracker for
   duplicates and flag any before proceeding.

## How you work

- **Ground it in the actual repo.** Read the relevant code and docs so
  the scope and criteria fit what exists — don't refine in the
  abstract.
- **Check for duplicates first** (`gh issue list --search "<keywords>"`)
  — flag an existing issue rather than letting a parallel one form.
- **Stay out of design.** If a requirement implies a design constraint,
  state it as an outcome ("must handle 10k concurrent connections"),
  not a mechanism.
- **Right-size the rigor.** A small bug fix needs a sentence and two
  criteria; a feature needs the full structure above. Don't
  over-produce.
- **This is a dialogue, not a one-shot.** Draft, then loop on the
  owner's edits one question at a time until the scope and acceptance
  criteria are something they would sign off on.

## Output

Produce the refined product spec in clear, structured markdown using
the sections above — legible inline, not raw JSON. The server writes
it to the issue: you do not write to the issue or its labels with `gh`
yourself (reading with `gh` — e.g. the duplicate check above — is
fine), and you do not set `approved:product-spec` or any priority
label.
"#;

/// The base template for the `implement` instruction point (§9.1, §7.2
/// ph.6) — implementing the approved issue test-first.
///
/// Distilled from this plugin's `agents/tdd-developer.md` and
/// `skills/implement/SKILL.md`, with the git/PR mechanics the old skill
/// covered deliberately dropped: in this daemon's architecture (§2.1),
/// the server itself owns branch/worktree lifecycle, commits, pushes,
/// and opening the PR, so the spawned agent's job is narrowed to the
/// judgment-requiring part — writing the code test-first — while the
/// TDD/SOLID/testing discipline from the source material carries over
/// unchanged.
const IMPLEMENT_BASE_TEMPLATE: &str = r#"# `implement` instructions

Injection point for implementing the issue test-first (spec §9.1, §7.2
ph.6). By the time this phase runs, `approved:product-spec`,
`approved:architecture`, and `approved:test-plan` are all already on
the issue — the approved issue IS your contract. Do not re-litigate
scope or design here; if you find the approved plan is wrong, stop and
say so rather than silently improvising around it.

You are acting as the delivery pipeline's **developer** for issue
#{issue_number}, working in the worktree already checked out at
`{worktree_path}` on branch `{branch}` (default branch
`{default_branch}`; when this project uses a `specs_dir` spec tool,
spec/plan artifacts live under `{specs_dir}`). The server owns git
mechanics — branch creation, commits, pushing, and opening the pull
request — so focus entirely on the code: do not commit, push, or open
a pull request yourself.

## Core loop: TDD (red -> green -> refactor)

For every behavior change, follow this cycle and do not break it:

1. **RED** — write the smallest failing test that expresses the next
   required behavior from the approved test plan. Run it. Confirm it
   fails, and that it fails for the right reason (the assertion, not a
   typo or import error).
2. **GREEN** — write the minimum production code to make that test
   pass. No more. Resist building for requirements you don't yet have
   a test for. Run the test. Confirm it passes.
3. **REFACTOR** — with tests green, improve the design: remove
   duplication, clarify names, extract methods/types, apply SOLID.
   Re-run the full test suite after each refactor; it must stay green.

Take small steps — one behavior per cycle. Always run the tests through
the project's actual test runner; never assume a result.

## What deserves a test

Aim tests at behavior that can break in a way that matters. Test your
code, not your dependencies — trust a well-tested library at its API
boundary rather than reconstructing it with an elaborate fake. Skip
trivial glue (pure pass-throughs, plain data holders). If you can't
name the regression a test would catch, it isn't earning its place.
This is a brake on over-testing, not permission to skip real logic.

## Design: SOLID

Apply these as you write and especially during the refactor step —
Single Responsibility, Open/Closed, Liskov Substitution, Interface
Segregation, Dependency Inversion. SOLID serves clarity and
changeability, not speculative abstraction: introduce an abstraction
when a test or a second concrete case justifies it, not before. Prefer
the simplest design that passes the tests.

## Language-specific style

Detect the project's language before writing code and follow its house
style. If this project has appended a style-guide reference to this
file (spec §9.3 — e.g. a Rust or Kotlin style guide bundled with the
spec), read it and hold your cycles to it.

## Working method

- Match the surrounding code — naming, structure, comment density,
  idioms. Read neighboring files before writing.
- Never disable functionality, skip a test, or weaken an assertion to
  make a suite go green — surface the real problem instead.
- Do not commit, push, or open a pull request. The server commits and
  pushes your work (§1.2, §15: mechanics are the server's job, judgment
  is yours) — leave your changes in the working tree.
- If the desired behavior is genuinely unclear from the approved spec
  and test plan, state the ambiguity and the assumption you're making,
  then proceed with the most reasonable interpretation captured as a
  test — don't stall on questions the approved issue already answers.
- When you finish, summarize: behaviors added, tests added, the design
  decisions SOLID drove, and the final test-run output proving
  everything passes. The review panel and fix loop that follow this
  phase are separate injection points (`review:*`, `fix`) — you are
  not expected to self-review here.
"#;

/// The placeholder seed text for an instruction point with no real base
/// template authored yet.
///
/// Deliberately thin: composing a real base template from spec-flow's
/// agents and skills (§1.1, §9.1) is the instruction composer's job
/// (§14 step 6) — [`instruction_seed_text`] is where that work lands,
/// point by point. Materializing the file here is what matters in the
/// meantime — it is the override hook a project edits (§9.2), and a
/// point whose file is missing has nowhere to be overridden.
fn instruction_placeholder(point: &str, purpose: &str) -> String {
    format!(
        "# `{point}` instructions\n\
         \n\
         Injection point for {purpose} (spec §9.1).\n\
         \n\
         **Placeholder.** The built-in base template for this point is \
         authored with the instruction composer (spec §14 step 6); until \
         then this text — the same text you're reading now — is that \
         base template.\n\
         \n\
         This file already starts in `replace` mode (see its first \
         line): delete this placeholder text below the directive line \
         and write yours in its place. To switch to `append` mode \
         instead — adding your text after this placeholder rather than \
         instead of it — remove the `<!-- mode: replace -->` line too \
         (spec §9.2).\n"
    )
}

/// Create `path` and every missing parent, mapping failures to
/// [`ScaffoldError::Write`].
fn create_dir(path: &Path) -> Result<(), ScaffoldError> {
    fs::create_dir_all(path).map_err(|source| ScaffoldError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Write `contents` to `path` unless it already exists.
///
/// The existence check and the creation are a single `create_new` open,
/// so an existing file is never truncated — not even by two `init` runs
/// racing each other.
fn create_new_file(path: &Path, contents: &str) -> Result<(), ScaffoldError> {
    let mut file = match fs::File::create_new(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            tracing::debug!(path = %path.display(), "kept existing file");
            return Ok(());
        }
        Err(source) => {
            return Err(ScaffoldError::Write {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    file.write_all(contents.as_bytes()).map_err(|source| {
        ScaffoldError::Write { path: path.to_path_buf(), source }
    })?;
    tracing::debug!(path = %path.display(), "scaffolded file");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn scaffolded_instruction_file_composes_without_duplicating_the_base() {
        // The exact regression a prior review round caught: without the
        // `replace` directive `instruction_file_seed` prepends, an
        // untouched fresh file would compose to base+base (append mode
        // appending an unedited copy of the base to itself). For every
        // point, an untouched seed file must compose to exactly its own
        // base template — no duplication, and a later edit fully
        // supersedes the base rather than trailing after a stale copy
        // of it.
        for (point, purpose) in INSTRUCTION_POINTS {
            let base = instruction_seed_text(point, purpose);
            let seeded_file = instruction_file_seed(point, purpose);

            let composed = crate::instructions::compose(
                &base,
                Some(&seeded_file),
                &HashMap::new(),
            );

            assert_eq!(
                composed, base,
                "point `{point}`'s untouched seed file did not compose \
                 to exactly its base template"
            );
        }
    }

    #[test]
    fn scaffold_spec_flow_dir_writes_every_instruction_file_with_the_replace_directive()
     {
        // Exercises the real production entry point, not
        // `instruction_file_seed` directly — the compose-level test
        // above would stay green even if `scaffold_spec_flow_dir`
        // regressed to writing `instruction_seed_text`'s bare base
        // (no directive) straight to disk, silently reopening the
        // doubling bug in every project a fresh `init` scaffolds.
        let repo_dir = tempfile::tempdir().unwrap();

        scaffold_spec_flow_dir(repo_dir.path()).unwrap();

        for (point, purpose) in INSTRUCTION_POINTS {
            let path = repo_dir
                .path()
                .join(".spec-flow")
                .join("instructions")
                .join(format!("{point}.md"));
            let on_disk = fs::read_to_string(&path).unwrap();

            assert_eq!(
                on_disk,
                instruction_file_seed(point, purpose),
                "point `{point}`'s scaffolded file does not match \
                 instruction_file_seed's output"
            );
            assert!(
                on_disk.starts_with(crate::instructions::REPLACE_DIRECTIVE),
                "point `{point}`'s scaffolded file does not start with \
                 the replace directive"
            );
        }
    }

    #[test]
    fn default_workflow_yaml_is_valid_yaml() {
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(DEFAULT_WORKFLOW_YAML).unwrap();

        let mapping = parsed.as_mapping().unwrap();
        for key in ["labels", "roborev", "spec", "review_panel", "phases"] {
            assert!(
                mapping.contains_key(serde_yaml::Value::from(key)),
                "default workflow is missing the `{key}` block"
            );
        }
    }

    #[test]
    fn instruction_points_are_unique() {
        let mut names: Vec<&str> =
            INSTRUCTION_POINTS.iter().map(|(point, _)| *point).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();

        assert_eq!(names.len(), total);
    }
}
