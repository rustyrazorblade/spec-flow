//! [`FakeVcs`] — an in-memory [`Vcs`](super::Vcs) test double.
//!
//! This is the "stub the git/gh seam" half of §5's testability claim:
//! code written against `impl Vcs` / `dyn Vcs` (the phase engine, the
//! scheduler, `spec-flow init`, all later tasks) can be exercised in unit
//! tests against a [`FakeVcs`] instead of a real checkout and a
//! reachable GitHub — no subprocess, no filesystem, no network.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{
    IssueRef, IssueRelationships, PullRequestRef, Vcs, VcsError, Worktree,
};

/// An in-memory [`Vcs`] implementation for tests.
///
/// Every field is `pub` so a test can seed exactly the state a scenario
/// needs (e.g. `fake.issues.borrow_mut().insert(42, ...)`) and later
/// inspect what the code under test did (e.g. `fake.comments.borrow()`).
/// Construct with [`FakeVcs::new`] — the defaults are "a healthy,
/// authenticated-nowhere fake" (`git`/`gh` present, no repo
/// pre-authenticated, no issues seeded).
#[derive(Debug)]
pub struct FakeVcs {
    /// Result [`Vcs::git_available`] returns.
    pub git_present: Cell<bool>,
    /// Result [`Vcs::gh_available`] returns.
    pub gh_present: Cell<bool>,
    /// Repo slugs [`Vcs::gh_authenticated`] should report as
    /// authenticated.
    pub authenticated: RefCell<HashSet<String>>,
    /// Repo slug [`Vcs::detect_repo_slug`] returns, per repo directory.
    pub repo_slugs: RefCell<HashMap<PathBuf, String>>,
    /// Default branch [`Vcs::detect_default_branch`] returns, per repo
    /// directory.
    pub default_branches: RefCell<HashMap<PathBuf, String>>,
    /// Repo slugs [`Vcs::merge_queue_enabled`] should report as having
    /// the native merge queue enabled.
    pub merge_queue_enabled_repos: RefCell<HashSet<String>>,
    /// Issues [`Vcs::read_issue`] can return, keyed by issue number.
    pub issues: RefCell<HashMap<u64, IssueRef>>,
    /// Relationships [`Vcs::read_relationships`] returns, keyed by
    /// issue number; an unlisted issue reads back as having none.
    pub relationships: RefCell<HashMap<u64, IssueRelationships>>,
    /// Every worktree [`Vcs::worktree_add`] has created, in call order.
    pub worktrees_created: RefCell<Vec<Worktree>>,
    /// Every worktree [`Vcs::worktree_remove`] has removed, in call
    /// order.
    pub worktrees_removed: RefCell<Vec<Worktree>>,
    /// `(worktree_path, message)` pairs passed to [`Vcs::commit`], in
    /// call order.
    pub commits: RefCell<Vec<(PathBuf, String)>>,
    /// `(worktree_path, branch)` pairs passed to [`Vcs::push`], in call
    /// order.
    pub pushes: RefCell<Vec<(PathBuf, String)>>,
    /// Labels currently set on each issue, per issue number.
    pub labels: RefCell<HashMap<u64, HashSet<String>>>,
    /// Comments posted on each issue, per issue number, in post order.
    pub comments: RefCell<HashMap<u64, Vec<String>>>,
    /// Every PR [`Vcs::open_pr`] has opened, in call order.
    pub prs_opened: RefCell<Vec<PullRequestRef>>,
    /// The PR number the next [`Vcs::open_pr`] call returns; increments
    /// after each call.
    pub next_pr_number: Cell<u64>,
    /// `(repo, pr_number)` pairs passed to [`Vcs::enqueue_merge`], in
    /// call order.
    pub merges_enqueued: RefCell<Vec<(String, u64)>>,
}

impl FakeVcs {
    /// Create a healthy fake: `git`/`gh` both report present, nothing
    /// else pre-seeded.
    pub fn new() -> FakeVcs {
        FakeVcs::default()
    }
}

impl Default for FakeVcs {
    fn default() -> FakeVcs {
        FakeVcs {
            git_present: Cell::new(true),
            gh_present: Cell::new(true),
            authenticated: RefCell::new(HashSet::new()),
            repo_slugs: RefCell::new(HashMap::new()),
            default_branches: RefCell::new(HashMap::new()),
            merge_queue_enabled_repos: RefCell::new(HashSet::new()),
            issues: RefCell::new(HashMap::new()),
            relationships: RefCell::new(HashMap::new()),
            worktrees_created: RefCell::new(Vec::new()),
            worktrees_removed: RefCell::new(Vec::new()),
            commits: RefCell::new(Vec::new()),
            pushes: RefCell::new(Vec::new()),
            labels: RefCell::new(HashMap::new()),
            comments: RefCell::new(HashMap::new()),
            prs_opened: RefCell::new(Vec::new()),
            next_pr_number: Cell::new(1),
            merges_enqueued: RefCell::new(Vec::new()),
        }
    }
}

impl Vcs for FakeVcs {
    fn git_available(&self) -> Result<(), VcsError> {
        if self.git_present.get() {
            Ok(())
        } else {
            Err(VcsError::BinaryNotFound { binary: "git".to_string() })
        }
    }

    fn gh_available(&self) -> Result<(), VcsError> {
        if self.gh_present.get() {
            Ok(())
        } else {
            Err(VcsError::BinaryNotFound { binary: "gh".to_string() })
        }
    }

    fn gh_authenticated(&self, repo: &str) -> Result<bool, VcsError> {
        Ok(self.authenticated.borrow().contains(repo))
    }

    fn detect_repo_slug(&self, repo_dir: &Path) -> Result<String, VcsError> {
        self.repo_slugs.borrow().get(repo_dir).cloned().ok_or_else(|| {
            VcsError::CommandFailed {
                command: "gh repo view".to_string(),
                status: 1,
                stderr: format!(
                    "no repo slug configured for {}",
                    repo_dir.display()
                ),
            }
        })
    }

    fn detect_default_branch(
        &self,
        repo_dir: &Path,
    ) -> Result<String, VcsError> {
        self.default_branches.borrow().get(repo_dir).cloned().ok_or_else(
            || VcsError::CommandFailed {
                command: "gh repo view".to_string(),
                status: 1,
                stderr: format!(
                    "no default branch configured for {}",
                    repo_dir.display()
                ),
            },
        )
    }

    fn merge_queue_enabled(
        &self,
        repo: &str,
        _default_branch: &str,
    ) -> Result<bool, VcsError> {
        Ok(self.merge_queue_enabled_repos.borrow().contains(repo))
    }

    fn worktree_add(
        &self,
        project_dir: &Path,
        branch: &str,
        issue_number: u64,
    ) -> Result<Worktree, VcsError> {
        let worktree = Worktree {
            issue_number,
            path: project_dir.join(branch),
            branch: branch.to_string(),
            slug: branch.to_string(),
        };
        self.worktrees_created.borrow_mut().push(worktree.clone());
        Ok(worktree)
    }

    fn worktree_remove(&self, worktree: &Worktree) -> Result<(), VcsError> {
        self.worktrees_removed.borrow_mut().push(worktree.clone());
        Ok(())
    }

    fn commit(
        &self,
        worktree_path: &Path,
        message: &str,
    ) -> Result<(), VcsError> {
        self.commits
            .borrow_mut()
            .push((worktree_path.to_path_buf(), message.to_string()));
        Ok(())
    }

    fn push(
        &self,
        worktree_path: &Path,
        branch: &str,
    ) -> Result<(), VcsError> {
        self.pushes
            .borrow_mut()
            .push((worktree_path.to_path_buf(), branch.to_string()));
        Ok(())
    }

    fn read_issue(
        &self,
        _repo: &str,
        issue_number: u64,
    ) -> Result<IssueRef, VcsError> {
        self.issues.borrow().get(&issue_number).cloned().ok_or_else(|| {
            VcsError::CommandFailed {
                command: "gh issue view".to_string(),
                status: 1,
                stderr: format!(
                    "no issue #{issue_number} configured on this fake"
                ),
            }
        })
    }

    fn set_label(
        &self,
        _repo: &str,
        issue_number: u64,
        label: &str,
        present: bool,
    ) -> Result<(), VcsError> {
        let mut labels = self.labels.borrow_mut();
        let entry = labels.entry(issue_number).or_default();
        if present {
            entry.insert(label.to_string());
        } else {
            entry.remove(label);
        }
        Ok(())
    }

    fn post_comment(
        &self,
        _repo: &str,
        issue_number: u64,
        body: &str,
    ) -> Result<(), VcsError> {
        self.comments
            .borrow_mut()
            .entry(issue_number)
            .or_default()
            .push(body.to_string());
        Ok(())
    }

    fn open_pr(
        &self,
        _repo: &str,
        branch: &str,
        _base: &str,
        _title: &str,
        _body: &str,
    ) -> Result<PullRequestRef, VcsError> {
        let number = self.next_pr_number.get();
        self.next_pr_number.set(number + 1);
        let pr = PullRequestRef {
            number,
            url: format!("https://github.com/example/example/pull/{number}"),
            branch: branch.to_string(),
        };
        self.prs_opened.borrow_mut().push(pr.clone());
        Ok(pr)
    }

    fn enqueue_merge(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> Result<(), VcsError> {
        self.merges_enqueued.borrow_mut().push((repo.to_string(), pr_number));
        Ok(())
    }

    fn read_relationships(
        &self,
        _repo: &str,
        issue_number: u64,
    ) -> Result<IssueRelationships, VcsError> {
        Ok(self
            .relationships
            .borrow()
            .get(&issue_number)
            .cloned()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_ready_fails_when_git_is_missing() {
        let vcs = FakeVcs::new();
        vcs.git_present.set(false);

        assert!(matches!(
            vcs.ensure_ready("owner/repo"),
            Err(VcsError::BinaryNotFound { .. })
        ));
    }

    #[test]
    fn ensure_ready_fails_when_repo_is_not_authenticated() {
        let vcs = FakeVcs::new();

        assert!(matches!(
            vcs.ensure_ready("owner/repo"),
            Err(VcsError::NotAuthenticated { .. })
        ));
    }

    #[test]
    fn ensure_ready_passes_once_authenticated() {
        let vcs = FakeVcs::new();
        vcs.authenticated.borrow_mut().insert("owner/repo".to_string());

        assert!(vcs.ensure_ready("owner/repo").is_ok());
    }

    #[test]
    fn set_label_toggles_membership() {
        let vcs = FakeVcs::new();

        vcs.set_label("owner/repo", 42, "status:ready", true).unwrap();
        assert!(vcs.labels.borrow()[&42].contains("status:ready"));

        vcs.set_label("owner/repo", 42, "status:ready", false).unwrap();
        assert!(!vcs.labels.borrow()[&42].contains("status:ready"));
    }

    #[test]
    fn open_pr_assigns_increasing_pr_numbers() {
        let vcs = FakeVcs::new();

        let first =
            vcs.open_pr("owner/repo", "issue-1", "main", "t", "b").unwrap();
        let second =
            vcs.open_pr("owner/repo", "issue-2", "main", "t", "b").unwrap();

        assert_eq!(first.number, 1);
        assert_eq!(second.number, 2);
    }

    #[test]
    fn read_issue_errors_when_not_seeded() {
        let vcs = FakeVcs::new();

        assert!(matches!(
            vcs.read_issue("owner/repo", 1),
            Err(VcsError::CommandFailed { .. })
        ));
    }

    #[test]
    fn worktree_add_records_creation_as_a_sibling_path() {
        let vcs = FakeVcs::new();

        let worktree = vcs
            .worktree_add(
                Path::new("/abs/repo-a"),
                "issue-42-widget-cache",
                42,
            )
            .unwrap();

        assert_eq!(
            worktree.path,
            PathBuf::from("/abs/repo-a/issue-42-widget-cache")
        );
        assert_eq!(vcs.worktrees_created.borrow().len(), 1);
    }
}
