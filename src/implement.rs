//! `start_implement` kickoff (§10.2, §14 step 9): the server-side
//! mechanics of moving an issue from `status:ready` into an actual
//! worktree, either with an auto-written spec or an explicit, recorded
//! skip.
//!
//! Split into two `Vcs`-touching orchestration functions, not one,
//! because §10.2 places a **spawned agent** between them: the server
//! claims the issue and creates the worktree (steps 1-2), an agent
//! auto-writes the spec into that worktree via the configured spec tool
//! (step 3 — unless the issue carries `spec.skip_label`), and only once
//! that agent has finished does the server commit + push + post the
//! development plan (step 4). This crate has no phase-engine wiring or
//! spawner integration yet (§14 stops short of that; see [`crate`]'s
//! module doc) — [`start_implement_setup`] covers what happens *before*
//! the agent runs, [`finish_implement_setup`] covers what happens
//! *after* it submits its artifact, and the actual spawn in between is a
//! future step's job.
//!
//! Claiming the issue (§10.2 step 1, §8.2) is deliberately **not** this
//! module's job: [`crate::claim::write_claim`]/[`crate::claim::
//! confirm_claim`] already exist and this module would only be
//! duplicating them — a caller runs the claim protocol first, then calls
//! [`start_implement_setup`] once it has actually won the claim.

use crate::config::ProjectConfig;
use crate::vcs::{IssueRef, Vcs, VcsError, Worktree};
use crate::workflow::SpecConfig;

/// The comment [`finish_implement_setup`] posts when spec authoring was
/// skipped (§10.2: "the server posts a comment recording that the spec
/// was skipped" — the audit trail, §2.3).
///
/// Takes `skip_label` rather than hard-coding `spec:skip`: the label
/// name is team-configurable (§11.3 `spec.skip_label`), and an audit
/// comment citing the wrong label name — one that was never actually on
/// the issue — would misstate its own trigger.
fn spec_skipped_comment(skip_label: &str) -> String {
    format!(
        "Spec authoring skipped (`{skip_label}` label present) — this \
         implement is branch + PR only."
    )
}

/// Whether `start_implement` should have an agent auto-write the spec,
/// or skip authoring one entirely (§10.2, §11.3 `spec.optional` +
/// `spec.skip_label`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecDecision {
    /// Spawn the spec-authoring agent (§11.3 `spec.tool`).
    AuthorSpec,
    /// The issue carries the configured skip label **and** skipping is
    /// allowed at all (`spec.optional`) — branch + PR only, no spec
    /// authored.
    SkipSpec,
}

/// What [`start_implement_setup`] produces for its caller to act on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartImplementSetup {
    /// The freshly created worktree/branch (§10.1).
    pub worktree: Worktree,
    /// The issue read back after claiming, for the caller to fold into
    /// the spec-authoring agent's prompt (product-spec + architecture +
    /// test-plan content, §10.2 step 3).
    pub issue: IssueRef,
    /// Whether to spawn the spec-authoring agent or skip it.
    pub decision: SpecDecision,
}

/// §10.2 steps 1-2: create the worktree and read the issue back, then
/// decide whether to author a spec or skip it.
///
/// `project.local_path` is the primary checkout `worktree_add` branches
/// from (§10.1); `project.repo` is what the issue is read from.
///
/// **Not yet idempotent against a restart mid-implement (§7.3).** §7.3
/// requires phases to be safely re-runnable — "an existing worktree/
/// branch is reused, not recreated" — but [`Vcs::worktree_add`] (already
/// existing, step 1) unconditionally runs `git worktree add -b <branch>`
/// and hard-fails if that branch/path already exists. Concretely: an
/// instance crashes right after creating the worktree, restarts,
/// reclaims its own stale claim (§8.2's carve-out for exactly this
/// case), and re-runs `start_implement_setup` — and gets a
/// [`VcsError::CommandFailed`] instead of the existing worktree being
/// reused. No caller exists yet to hit this (the phase engine's spawner
/// wiring is a later step), so it is recorded here rather than fixed
/// blind — whoever wires this in needs to either detect-and-reuse an
/// existing worktree at `project.project_dir.join(branch)` here, or push
/// that idempotency into `worktree_add` itself.
///
/// # Errors
///
/// Propagates the [`VcsError`] from [`Vcs::worktree_add`] or
/// [`Vcs::read_issue`], whichever fails first.
pub fn start_implement_setup<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    issue_number: u64,
    branch: &str,
    spec: &SpecConfig,
) -> Result<StartImplementSetup, VcsError> {
    let worktree = vcs.worktree_add(
        &project.local_path,
        &project.project_dir,
        branch,
        issue_number,
    )?;
    let issue = vcs.read_issue(&project.repo, issue_number)?;
    let decision = if spec.optional
        && issue.labels.iter().any(|label| label == &spec.skip_label)
    {
        SpecDecision::SkipSpec
    } else {
        SpecDecision::AuthorSpec
    };
    Ok(StartImplementSetup { worktree, issue, decision })
}

/// §10.2 step 4: commit the spec first (only when one was authored),
/// push the branch, then post the development plan — **always**, even
/// when the spec was skipped (§10.2: "The dev plan is still authored +
/// posted"). Setting the status label to advance the issue past this
/// point is deliberately not this function's job either — that belongs
/// to whichever future phase-engine caller wires `crate::workflow::
/// advance`/`set_gate` in, once one exists.
///
/// Takes the whole [`StartImplementSetup`] `start_implement_setup`
/// returned for this same issue, rather than its `worktree`/`decision`
/// fields as loose parameters (also keeping this function's own
/// parameter count under clippy's `too_many_arguments` threshold):
/// `setup.decision` is used as-is, not re-derived, since re-checking
/// `spec.skip_label` after the agent has already run (and possibly
/// already committed a spec despite the label, or not) would let this
/// function's decision disagree with what actually happened in the
/// worktree.
///
/// # Errors
///
/// Propagates the [`VcsError`] from the first failing [`Vcs`] call:
/// [`Vcs::commit`] or [`Vcs::post_comment`] (the skip note), then
/// [`Vcs::push`], then [`Vcs::post_comment`] (the development plan).
pub fn finish_implement_setup<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    setup: &StartImplementSetup,
    spec: &SpecConfig,
    dev_plan: &str,
) -> Result<(), VcsError> {
    let issue_number = setup.issue.number;
    let worktree_path = setup.worktree.path.as_path();
    let branch = &setup.worktree.branch;
    match setup.decision {
        SpecDecision::AuthorSpec => {
            vcs.commit(worktree_path, "docs: add spec")?;
        }
        SpecDecision::SkipSpec => {
            vcs.post_comment(
                &project.repo,
                issue_number,
                &spec_skipped_comment(&spec.skip_label),
            )?;
        }
    }
    vcs.push(worktree_path, branch)?;
    vcs.post_comment(&project.repo, issue_number, dev_plan)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MergeMode;
    use crate::vcs::FakeVcs;
    use std::path::PathBuf;

    const REPO: &str = "owner/repo-a";

    fn project() -> ProjectConfig {
        ProjectConfig {
            name: "repo-a".to_string(),
            repo: REPO.to_string(),
            project_dir: PathBuf::from("/projects/repo-a"),
            local_path: PathBuf::from("/projects/repo-a/main"),
            default_branch: "main".to_string(),
            merge_mode: MergeMode::Native,
            gh: None,
            harness: None,
            index_path: PathBuf::from("/projects/repo-a/index"),
            memory_scope: "owner-repo-a".to_string(),
            weight: None,
        }
    }

    fn spec(optional: bool) -> SpecConfig {
        SpecConfig {
            tool: "openspec".to_string(),
            openspec: None,
            specs_dir: None,
            optional,
            skip_label: "spec:skip".to_string(),
        }
    }

    fn seed_issue(vcs: &FakeVcs, labels: &[&str]) {
        vcs.issues.borrow_mut().insert(
            (REPO.to_string(), 42),
            IssueRef {
                number: 42,
                title: "Widget cache".to_string(),
                body: String::new(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                state: crate::vcs::IssueState::Open,
            },
        );
    }

    // -- start_implement_setup --

    #[test]
    fn start_implement_setup_authors_a_spec_with_no_skip_label() {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["status:ready"]);

        let setup = start_implement_setup(
            &vcs,
            &project(),
            42,
            "issue-42-widget-cache",
            &spec(true),
        )
        .unwrap();

        assert_eq!(setup.decision, SpecDecision::AuthorSpec);
        assert_eq!(setup.worktree.issue_number, 42);
        assert_eq!(setup.issue.number, 42);
    }

    #[test]
    fn start_implement_setup_skips_the_spec_when_the_label_is_present_and_allowed()
     {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["status:ready", "spec:skip"]);

        let setup = start_implement_setup(
            &vcs,
            &project(),
            42,
            "issue-42-widget-cache",
            &spec(true),
        )
        .unwrap();

        assert_eq!(setup.decision, SpecDecision::SkipSpec);
    }

    #[test]
    fn start_implement_setup_ignores_the_skip_label_when_spec_is_not_optional()
    {
        // §11.3: `optional` gates whether the per-issue skip label is
        // honored AT ALL -- a project that requires a spec for every
        // issue must not let the label bypass that.
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &["status:ready", "spec:skip"]);

        let setup = start_implement_setup(
            &vcs,
            &project(),
            42,
            "issue-42-widget-cache",
            &spec(false),
        )
        .unwrap();

        assert_eq!(setup.decision, SpecDecision::AuthorSpec);
    }

    #[test]
    fn start_implement_setup_creates_the_worktree_against_the_primary_checkout()
     {
        let vcs = FakeVcs::new();
        seed_issue(&vcs, &[]);

        start_implement_setup(
            &vcs,
            &project(),
            42,
            "issue-42-widget-cache",
            &spec(true),
        )
        .unwrap();

        let created = vcs.worktrees_created.borrow();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, project().local_path);
    }

    #[test]
    fn start_implement_setup_propagates_a_read_issue_failure() {
        let vcs = FakeVcs::new();
        // No issue seeded at all -- worktree_add succeeds (the fake
        // never fails it), but the read_issue that follows must.

        assert!(matches!(
            start_implement_setup(
                &vcs,
                &project(),
                42,
                "issue-42-widget-cache",
                &spec(true),
            ),
            Err(VcsError::CommandFailed { .. })
        ));
    }

    // -- finish_implement_setup --

    fn setup(decision: SpecDecision) -> StartImplementSetup {
        StartImplementSetup {
            worktree: Worktree {
                issue_number: 42,
                path: PathBuf::from("/projects/repo-a/issue-42"),
                branch: "issue-42-widget-cache".to_string(),
                slug: "widget-cache".to_string(),
            },
            issue: IssueRef {
                number: 42,
                title: "Widget cache".to_string(),
                body: String::new(),
                labels: Vec::new(),
                state: crate::vcs::IssueState::Open,
            },
            decision,
        }
    }

    #[test]
    fn finish_implement_setup_commits_pushes_and_posts_the_dev_plan_when_a_spec_was_authored()
     {
        let vcs = FakeVcs::new();
        let setup = setup(SpecDecision::AuthorSpec);

        finish_implement_setup(
            &vcs,
            &project(),
            &setup,
            &spec(true),
            "## Plan\n1. Do it.",
        )
        .unwrap();

        assert_eq!(vcs.commits.borrow().len(), 1);
        assert_eq!(vcs.commits.borrow()[0].0, setup.worktree.path);
        assert_eq!(
            vcs.pushes.borrow()[0],
            (setup.worktree.path.clone(), setup.worktree.branch.clone())
        );
        let comments = vcs.comments.borrow();
        let posted = &comments[&(REPO.to_string(), 42)];
        assert_eq!(posted, &vec!["## Plan\n1. Do it.".to_string()]);
    }

    #[test]
    fn finish_implement_setup_posts_a_skip_note_instead_of_committing_when_skipped()
     {
        let vcs = FakeVcs::new();
        let setup = setup(SpecDecision::SkipSpec);

        finish_implement_setup(
            &vcs,
            &project(),
            &setup,
            &spec(true),
            "## Plan\n1. Do it.",
        )
        .unwrap();

        assert!(vcs.commits.borrow().is_empty());
        let comments = vcs.comments.borrow();
        let posted = &comments[&(REPO.to_string(), 42)];
        assert_eq!(
            posted,
            &vec![
                spec_skipped_comment("spec:skip"),
                "## Plan\n1. Do it.".to_string(),
            ]
        );
    }

    #[test]
    fn finish_implement_setup_cites_the_configured_skip_label_in_the_audit_comment()
     {
        // §11.3: skip_label is team-configurable -- an audit comment
        // citing the wrong label name would misstate its own trigger.
        let vcs = FakeVcs::new();
        let setup = setup(SpecDecision::SkipSpec);
        let mut custom_spec = spec(true);
        custom_spec.skip_label = "no-spec-needed".to_string();

        finish_implement_setup(&vcs, &project(), &setup, &custom_spec, "plan")
            .unwrap();

        let comments = vcs.comments.borrow();
        let posted = &comments[&(REPO.to_string(), 42)];
        assert!(posted[0].contains("no-spec-needed"));
        assert!(!posted[0].contains("spec:skip"));
    }

    #[test]
    fn finish_implement_setup_pushes_the_branch_even_when_skipped() {
        // §10.2: skipping the spec means "branch + PR only" -- the
        // branch/PR half must still happen.
        let vcs = FakeVcs::new();
        let setup = setup(SpecDecision::SkipSpec);

        finish_implement_setup(&vcs, &project(), &setup, &spec(true), "plan")
            .unwrap();

        assert_eq!(vcs.pushes.borrow().len(), 1);
    }
}
