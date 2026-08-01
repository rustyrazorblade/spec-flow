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
/// comments are half the value of the file a team is meant to edit, and
/// this crate has no workflow data model yet (the phase engine, §14 step
/// 7, introduces one).
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
        create_new_file(&path, &instruction_placeholder(point, purpose))?;
    }
    Ok(())
}

/// The seed text for one instruction point's file.
///
/// Deliberately thin: composing a real base template from spec-flow's
/// agents and skills (§1.1, §9.1) is the instruction composer's job (§14
/// step 6). Materializing the file here is what matters — it is the
/// override hook a project edits (§9.2), and a point whose file is
/// missing has nowhere to be overridden.
fn instruction_placeholder(point: &str, purpose: &str) -> String {
    format!(
        "# `{point}` instructions\n\
         \n\
         Injection point for {purpose} (spec §9.1).\n\
         \n\
         **Placeholder.** The built-in base template for this point is \
         authored with the instruction composer (spec §14 step 6); until \
         then this file only reserves the override hook.\n\
         \n\
         Text here is appended to the base template. To supersede the \
         base entirely, make `<!-- mode: replace -->` the first line of \
         this file (spec §9.2).\n"
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
