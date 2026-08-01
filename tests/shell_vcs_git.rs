//! Integration tests for [`ShellVcs`]'s `git`-only operations.
//!
//! These exercise a real `git` against a scratch project container laid
//! out the way spec §10.1 requires — a bare "remote", and a
//! `<project_dir>/` holding the primary checkout and every issue
//! worktree as siblings. No network and no authentication are involved,
//! so unlike the `gh` paths (unit-tested against a fake command runner)
//! these can assert on genuine on-disk git state.

use std::path::{Path, PathBuf};
use std::process::Command;

use spec_flow::{ShellVcs, Vcs};
use tempfile::TempDir;

/// A scratch project container with a bare remote and one checkout.
struct Scratch {
    /// Keeps the temp directory alive for the test's duration.
    _root: TempDir,
    /// The bare repository the checkout was cloned from.
    remote: PathBuf,
    /// The project container: every checkout is a sibling inside it.
    project_dir: PathBuf,
    /// The primary checkout, `<project_dir>/main`.
    checkout: PathBuf,
}

/// Run `git` with `args` in `cwd`, panicking with its stderr on failure.
///
/// # Panics
///
/// Panics if `git` cannot be spawned or exits non-zero — a broken
/// fixture should fail loudly rather than produce a confusing assertion.
fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git must be installed to run these tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Build a scratch container: a bare remote with one commit on `main`,
/// cloned into `<project_dir>/main`.
fn scratch() -> Scratch {
    let root = tempfile::tempdir().unwrap();
    let remote = root.path().join("remote.git");
    let project_dir = root.path().join("repo-a");
    let checkout = project_dir.join("main");

    std::fs::create_dir_all(&project_dir).unwrap();
    git(
        root.path(),
        &["init", "--bare", "--initial-branch=main", "remote.git"],
    );

    // Seed the remote through a throwaway clone so the real checkout is
    // cloned from a non-empty repository and therefore has
    // `refs/remotes/origin/HEAD` set, exactly as an operator's would.
    let seed = root.path().join("seed");
    git(root.path(), &["clone", remote.to_str().unwrap(), "seed"]);
    git(&seed, &["config", "user.email", "spec-flow@example.com"]);
    git(&seed, &["config", "user.name", "spec-flow Test"]);
    std::fs::write(seed.join("README.md"), "seed\n").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "--message", "seed"]);
    git(&seed, &["push", "--set-upstream", "origin", "main"]);
    std::fs::remove_dir_all(&seed).unwrap();

    git(&project_dir, &["clone", remote.to_str().unwrap(), "main"]);
    git(&checkout, &["config", "user.email", "spec-flow@example.com"]);
    git(&checkout, &["config", "user.name", "spec-flow Test"]);

    Scratch { _root: root, remote, project_dir, checkout }
}

/// A [`ShellVcs`] over the `git`/`gh` on `PATH`.
fn vcs() -> ShellVcs {
    ShellVcs::new("git".to_string(), "gh".to_string())
}

#[test]
fn git_available_finds_the_git_on_path() {
    assert!(vcs().git_available().is_ok());
}

#[test]
fn detect_default_branch_reads_the_clone_s_remote_head() {
    let scratch = scratch();

    let branch = vcs().detect_default_branch(&scratch.checkout).unwrap();

    assert_eq!(branch, "main");
}

#[test]
fn worktree_add_creates_a_sibling_checkout_on_a_new_branch() {
    let scratch = scratch();

    let worktree = vcs()
        .worktree_add(&scratch.project_dir, "issue-42-widget-cache", 42)
        .unwrap();

    assert_eq!(worktree.issue_number, 42);
    assert_eq!(
        worktree.path,
        scratch.project_dir.join("issue-42-widget-cache")
    );
    assert_eq!(worktree.branch, "issue-42-widget-cache");
    assert_eq!(worktree.slug, "widget-cache");
    assert!(worktree.path.join("README.md").is_file());
    // A sibling of the primary checkout, never nested inside it (§10.1).
    assert!(!worktree.path.starts_with(&scratch.checkout));
    assert_eq!(
        git(&worktree.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "issue-42-widget-cache"
    );
}

#[test]
fn commit_records_pending_changes_in_the_worktree() {
    let scratch = scratch();
    let vcs = vcs();
    let worktree =
        vcs.worktree_add(&scratch.project_dir, "issue-42-cache", 42).unwrap();
    std::fs::write(worktree.path.join("cache.rs"), "// cache\n").unwrap();

    vcs.commit(&worktree.path, "issue-42: add the cache").unwrap();

    assert_eq!(
        git(&worktree.path, &["log", "-1", "--pretty=%s"]),
        "issue-42: add the cache"
    );
    assert_eq!(git(&worktree.path, &["status", "--porcelain"]), "");
}

#[test]
fn push_publishes_the_branch_to_the_remote() {
    let scratch = scratch();
    let vcs = vcs();
    let worktree =
        vcs.worktree_add(&scratch.project_dir, "issue-42-cache", 42).unwrap();
    std::fs::write(worktree.path.join("cache.rs"), "// cache\n").unwrap();
    vcs.commit(&worktree.path, "issue-42: add the cache").unwrap();

    vcs.push(&worktree.path, "issue-42-cache").unwrap();

    assert!(
        !git(&scratch.remote, &["rev-parse", "--verify", "issue-42-cache"])
            .is_empty()
    );
}

#[test]
fn worktree_remove_deletes_the_checkout_and_its_branch() {
    let scratch = scratch();
    let vcs = vcs();
    let worktree =
        vcs.worktree_add(&scratch.project_dir, "issue-42-cache", 42).unwrap();

    vcs.worktree_remove(&worktree).unwrap();

    assert!(!worktree.path.exists());
    assert_eq!(
        git(&scratch.checkout, &["branch", "--list", "issue-42-cache"]),
        ""
    );
    assert_eq!(
        git(&scratch.checkout, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
fn worktree_add_reports_a_container_with_no_checkout() {
    let root = tempfile::tempdir().unwrap();

    let error =
        vcs().worktree_add(root.path(), "issue-1-nowhere", 1).unwrap_err();

    assert!(matches!(error, spec_flow::VcsError::CommandFailed { .. }));
}
