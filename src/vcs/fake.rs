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
    branch_slug,
};

/// An in-memory [`Vcs`] implementation for tests.
///
/// Every field is `pub` so a test can seed exactly the state a scenario
/// needs (e.g. `fake.issues.borrow_mut().insert(("repo-a".into(), 42),
/// ...)`) and later inspect what the code under test did (e.g.
/// `fake.comments.borrow()`).
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
    /// Issues [`Vcs::read_issue`] can return, keyed by `(repo,
    /// issue_number)` — **not** a bare issue number, so a test can seed
    /// `("repo-a", 42)` and `("repo-b", 42)` as the distinct issues §15
    /// requires them to be (one daemon serves many repos; issue numbers
    /// are only unique per repo).
    pub issues: RefCell<HashMap<(String, u64), IssueRef>>,
    /// Relationships [`Vcs::read_relationships`] returns, keyed by
    /// `(repo, issue_number)`; an unlisted issue reads back as having
    /// none.
    pub relationships: RefCell<HashMap<(String, u64), IssueRelationships>>,
    /// `(primary_checkout, Worktree)` pairs [`Vcs::worktree_add`] has
    /// created, in call order — the primary checkout is recorded
    /// alongside the worktree so a test can assert a caller passed the
    /// correct one, the property that parameter exists to guarantee.
    pub worktrees_created: RefCell<Vec<(PathBuf, Worktree)>>,
    /// `(primary_checkout, Worktree)` pairs [`Vcs::worktree_remove`] has
    /// removed, in call order.
    pub worktrees_removed: RefCell<Vec<(PathBuf, Worktree)>>,
    /// `(worktree_path, message)` pairs passed to [`Vcs::commit`], in
    /// call order.
    pub commits: RefCell<Vec<(PathBuf, String)>>,
    /// `(worktree_path, branch)` pairs passed to [`Vcs::push`], in call
    /// order.
    pub pushes: RefCell<Vec<(PathBuf, String)>>,
    /// Labels currently set on each issue, per `(repo, issue_number)`.
    pub labels: RefCell<HashMap<(String, u64), HashSet<String>>>,
    /// Comments posted on each issue, per `(repo, issue_number)`, in
    /// post order.
    pub comments: RefCell<HashMap<(String, u64), Vec<String>>>,
    /// `(repo, PullRequestRef)` pairs [`Vcs::open_pr`] has opened, in
    /// call order — repo-tagged like every other issue/PR-scoped map
    /// here, so a test can assert which repo a PR was opened against.
    pub prs_opened: RefCell<Vec<(String, PullRequestRef)>>,
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
        primary_checkout: &Path,
        project_dir: &Path,
        branch: &str,
        issue_number: u64,
    ) -> Result<Worktree, VcsError> {
        let worktree = Worktree {
            issue_number,
            path: project_dir.join(branch),
            branch: branch.to_string(),
            slug: branch_slug(branch, issue_number),
        };
        self.worktrees_created
            .borrow_mut()
            .push((primary_checkout.to_path_buf(), worktree.clone()));
        Ok(worktree)
    }

    fn worktree_remove(
        &self,
        primary_checkout: &Path,
        worktree: &Worktree,
    ) -> Result<(), VcsError> {
        self.worktrees_removed
            .borrow_mut()
            .push((primary_checkout.to_path_buf(), worktree.clone()));
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
        repo: &str,
        issue_number: u64,
    ) -> Result<IssueRef, VcsError> {
        let key = (repo.to_string(), issue_number);
        self.issues.borrow().get(&key).cloned().ok_or_else(|| {
            VcsError::CommandFailed {
                command: "gh issue view".to_string(),
                status: 1,
                stderr: format!(
                    "no issue {repo}#{issue_number} configured on this fake"
                ),
            }
        })
    }

    fn set_label(
        &self,
        repo: &str,
        issue_number: u64,
        label: &str,
        present: bool,
    ) -> Result<(), VcsError> {
        let mut labels = self.labels.borrow_mut();
        let entry =
            labels.entry((repo.to_string(), issue_number)).or_default();
        if present {
            entry.insert(label.to_string());
        } else {
            entry.remove(label);
        }
        Ok(())
    }

    fn post_comment(
        &self,
        repo: &str,
        issue_number: u64,
        body: &str,
    ) -> Result<(), VcsError> {
        self.comments
            .borrow_mut()
            .entry((repo.to_string(), issue_number))
            .or_default()
            .push(body.to_string());
        Ok(())
    }

    fn open_pr(
        &self,
        repo: &str,
        branch: &str,
        _base: &str,
        _title: &str,
        _body: &str,
    ) -> Result<PullRequestRef, VcsError> {
        let number = self.next_pr_number.get();
        self.next_pr_number.set(number + 1);
        let pr = PullRequestRef {
            number,
            url: format!("https://github.com/{repo}/pull/{number}"),
            branch: branch.to_string(),
        };
        self.prs_opened.borrow_mut().push((repo.to_string(), pr.clone()));
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
        repo: &str,
        issue_number: u64,
    ) -> Result<IssueRelationships, VcsError> {
        Ok(self
            .relationships
            .borrow()
            .get(&(repo.to_string(), issue_number))
            .cloned()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::IssueState;

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
        let key = ("owner/repo".to_string(), 42);

        vcs.set_label("owner/repo", 42, "status:ready", true).unwrap();
        assert!(vcs.labels.borrow()[&key].contains("status:ready"));

        vcs.set_label("owner/repo", 42, "status:ready", false).unwrap();
        assert!(!vcs.labels.borrow()[&key].contains("status:ready"));
    }

    #[test]
    fn issues_labels_and_comments_are_scoped_per_repo_not_a_bare_issue_number()
    {
        // §15: one daemon serves many repos, so issue numbers are only
        // unique per repo — `repo-a#42` and `repo-b#42` must never
        // collide on this fake, the way real GitHub state never does.
        let vcs = FakeVcs::new();
        vcs.issues.borrow_mut().insert(
            ("repo-a".to_string(), 42),
            IssueRef {
                number: 42,
                title: "repo-a's issue".to_string(),
                body: String::new(),
                labels: Vec::new(),
                state: IssueState::Open,
            },
        );
        vcs.issues.borrow_mut().insert(
            ("repo-b".to_string(), 42),
            IssueRef {
                number: 42,
                title: "repo-b's issue".to_string(),
                body: String::new(),
                labels: Vec::new(),
                state: IssueState::Open,
            },
        );

        assert_eq!(
            vcs.read_issue("repo-a", 42).unwrap().title,
            "repo-a's issue"
        );
        assert_eq!(
            vcs.read_issue("repo-b", 42).unwrap().title,
            "repo-b's issue"
        );

        vcs.set_label("repo-a", 42, "status:ready", true).unwrap();
        vcs.post_comment("repo-a", 42, "on repo-a").unwrap();

        assert!(
            vcs.labels.borrow()[&("repo-a".to_string(), 42)]
                .contains("status:ready")
        );
        assert!(
            !vcs.labels.borrow().contains_key(&("repo-b".to_string(), 42))
        );
        assert_eq!(
            vcs.comments.borrow()[&("repo-a".to_string(), 42)],
            vec!["on repo-a".to_string()]
        );
        assert!(
            !vcs.comments.borrow().contains_key(&("repo-b".to_string(), 42))
        );
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
                Path::new("/abs/repo-a/main"),
                Path::new("/abs/repo-a"),
                "issue-42-widget-cache",
                42,
            )
            .unwrap();

        assert_eq!(
            worktree.path,
            PathBuf::from("/abs/repo-a/issue-42-widget-cache")
        );
        assert_eq!(worktree.slug, "widget-cache");
        let created = vcs.worktrees_created.borrow();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0], (PathBuf::from("/abs/repo-a/main"), worktree));
    }
}
