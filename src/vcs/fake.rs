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
    IssueRef, IssueRelationships, IssueState, PullRequestRef,
    PullRequestStatus, Vcs, VcsError, Worktree, branch_slug,
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
    /// The issue number the next [`Vcs::create_issue`] call assigns;
    /// increments after each call. A single counter shared across every
    /// repo, mirroring [`FakeVcs::next_pr_number`]'s same simplification
    /// (real GitHub issue numbers are per-repo, but nothing in this
    /// crate's tests needs the fake to model that distinction).
    pub next_issue_number: Cell<u64>,
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
    /// Label names [`Vcs::ensure_label`] has created in each repo's
    /// label set — repo-scoped, not issue-scoped, since a label's
    /// existence is a property of the repo, not of any one issue.
    pub labels_ensured: RefCell<HashMap<String, HashSet<String>>>,
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
    /// Pull requests [`Vcs::find_linked_pull_requests`] returns, keyed by
    /// `(repo, issue_number)`; an unlisted issue reads back as having
    /// none linked.
    pub linked_pull_requests:
        RefCell<HashMap<(String, u64), Vec<PullRequestRef>>>,
    /// Statuses [`Vcs::read_pull_request_status`] can return, keyed by
    /// `(repo, pr_number)` — **not** a bare PR number, for the same
    /// reason [`issues`](FakeVcs::issues) is repo-keyed (§15).
    pub pull_request_statuses:
        RefCell<HashMap<(String, u64), PullRequestStatus>>,
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
            next_issue_number: Cell::new(1),
            relationships: RefCell::new(HashMap::new()),
            worktrees_created: RefCell::new(Vec::new()),
            worktrees_removed: RefCell::new(Vec::new()),
            commits: RefCell::new(Vec::new()),
            pushes: RefCell::new(Vec::new()),
            labels: RefCell::new(HashMap::new()),
            labels_ensured: RefCell::new(HashMap::new()),
            comments: RefCell::new(HashMap::new()),
            prs_opened: RefCell::new(Vec::new()),
            next_pr_number: Cell::new(1),
            merges_enqueued: RefCell::new(Vec::new()),
            linked_pull_requests: RefCell::new(HashMap::new()),
            pull_request_statuses: RefCell::new(HashMap::new()),
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

    fn create_issue(
        &self,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<IssueRef, VcsError> {
        let number = self.next_issue_number.get();
        self.next_issue_number.set(number + 1);
        let issue = IssueRef {
            number,
            title: title.to_string(),
            body: body.to_string(),
            labels: Vec::new(),
            state: IssueState::Open,
        };
        let key = (repo.to_string(), number);
        debug_assert!(
            !self.issues.borrow().contains_key(&key),
            "a hand-seeded issue already occupies {key:?} -- seed at a \
             number `next_issue_number` won't reach, or don't mix seeding \
             with create_issue on the same fake"
        );
        self.issues.borrow_mut().insert(key, issue.clone());
        Ok(issue)
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

    /// Derived from [`issues`](FakeVcs::issues), not from a separate
    /// seedable list: a fake that could report an issue number
    /// [`Vcs::read_issue`] then fails on would let a caller pass tests
    /// against a state real GitHub cannot produce. Sorted descending to
    /// model `gh issue list`'s newest-first ordering — the real one sorts
    /// by creation time, which this fake has no notion of, but issue
    /// numbers are monotonic, so descending number is that same order.
    fn list_open_issues(
        &self,
        repo: &str,
        limit: u32,
    ) -> Result<Vec<u64>, VcsError> {
        let issues = self.issues.borrow();
        let mut numbers: Vec<u64> = issues
            .iter()
            .filter(|((issue_repo, _), issue)| {
                issue_repo == repo && issue.state == IssueState::Open
            })
            .map(|((_, number), _)| *number)
            .collect();
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        numbers.truncate(limit as usize);
        Ok(numbers)
    }

    /// Unlike `ShellVcs::set_label`, this fake always accepts a novel
    /// label name — it models the trait's stated contract, not the real
    /// `gh issue edit --add-label`'s "label must already exist"
    /// limitation. See `ShellVcs::set_label`'s doc for that confirmed
    /// conflict; do not rely on this fake to catch it.
    fn set_label(
        &self,
        repo: &str,
        issue_number: u64,
        label: &str,
        present: bool,
    ) -> Result<(), VcsError> {
        let key = (repo.to_string(), issue_number);
        {
            let mut labels = self.labels.borrow_mut();
            let entry = labels.entry(key.clone()).or_default();
            if present {
                entry.insert(label.to_string());
            } else {
                entry.remove(label);
            }
        }
        // Keep a seeded `issues` entry's own `labels` field (what
        // `read_issue` returns) in sync with this call, so a caller that
        // writes a label here and then re-reads the issue — the
        // work-claim write-then-settle-read protocol (§8.2) being the
        // motivating case — sees its own write reflected, mirroring real
        // GitHub, where `gh issue edit --add-label` is visible to the
        // very next `gh issue view`. Before this, `issues` and `labels`
        // were two independent stores and a `set_label` call was
        // invisible to `read_issue` entirely.
        if let Some(issue) = self.issues.borrow_mut().get_mut(&key) {
            if present {
                if !issue.labels.iter().any(|l| l == label) {
                    issue.labels.push(label.to_string());
                }
            } else {
                issue.labels.retain(|l| l != label);
            }
        }
        Ok(())
    }

    fn ensure_label(&self, repo: &str, label: &str) -> Result<(), VcsError> {
        self.labels_ensured
            .borrow_mut()
            .entry(repo.to_string())
            .or_default()
            .insert(label.to_string());
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

    fn find_linked_pull_requests(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> Result<Vec<PullRequestRef>, VcsError> {
        Ok(self
            .linked_pull_requests
            .borrow()
            .get(&(repo.to_string(), issue_number))
            .cloned()
            .unwrap_or_default())
    }

    fn read_pull_request_status(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> Result<PullRequestStatus, VcsError> {
        self.pull_request_statuses
            .borrow()
            .get(&(repo.to_string(), pr_number))
            .cloned()
            .ok_or_else(|| VcsError::CommandFailed {
                command: "gh pr view".to_string(),
                status: 1,
                stderr: format!(
                    "no pull request status configured for \
                     {repo}#{pr_number}"
                ),
            })
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
    fn create_issue_assigns_increasing_numbers_and_reads_back_open() {
        let vcs = FakeVcs::new();

        let first = vcs.create_issue("owner/repo", "First", "a").unwrap();
        let second = vcs.create_issue("owner/repo", "Second", "b").unwrap();

        assert_eq!(first.number, 1);
        assert_eq!(second.number, 2);
        assert_eq!(second.title, "Second");
        assert_eq!(second.state, IssueState::Open);
        assert!(second.labels.is_empty());
    }

    #[test]
    fn create_issue_is_visible_to_a_subsequent_read_issue() {
        let vcs = FakeVcs::new();

        let created =
            vcs.create_issue("owner/repo", "Widget cache", "Do it.").unwrap();
        let read = vcs.read_issue("owner/repo", created.number).unwrap();

        assert_eq!(read, created);
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
    fn list_open_issues_is_empty_for_an_unseeded_repo() {
        let vcs = FakeVcs::new();

        assert!(vcs.list_open_issues("owner/repo", 100).unwrap().is_empty());
    }

    #[test]
    fn list_open_issues_returns_open_issues_newest_first() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo", "First", "a").unwrap();
        vcs.create_issue("owner/repo", "Second", "b").unwrap();
        vcs.create_issue("owner/repo", "Third", "c").unwrap();

        assert_eq!(
            vcs.list_open_issues("owner/repo", 100).unwrap(),
            vec![3, 2, 1]
        );
    }

    #[test]
    fn list_open_issues_omits_closed_issues() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo", "Open one", "a").unwrap();
        let closed =
            vcs.create_issue("owner/repo", "Closed one", "b").unwrap();
        vcs.issues
            .borrow_mut()
            .get_mut(&("owner/repo".to_string(), closed.number))
            .unwrap()
            .state = IssueState::Closed;

        assert_eq!(vcs.list_open_issues("owner/repo", 100).unwrap(), vec![1]);
    }

    #[test]
    fn list_open_issues_scopes_to_the_requested_repo() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "On a", "").unwrap();
        vcs.create_issue("owner/repo-b", "On b", "").unwrap();

        assert_eq!(
            vcs.list_open_issues("owner/repo-a", 100).unwrap(),
            vec![1]
        );
        assert_eq!(
            vcs.list_open_issues("owner/repo-b", 100).unwrap(),
            vec![2]
        );
    }

    #[test]
    fn list_open_issues_truncates_to_the_limit_keeping_the_newest() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo", "First", "").unwrap();
        vcs.create_issue("owner/repo", "Second", "").unwrap();
        vcs.create_issue("owner/repo", "Third", "").unwrap();

        assert_eq!(vcs.list_open_issues("owner/repo", 2).unwrap(), vec![3, 2]);
    }

    #[test]
    fn find_linked_pull_requests_returns_none_when_unseeded() {
        let vcs = FakeVcs::new();

        let prs = vcs.find_linked_pull_requests("owner/repo", 42).unwrap();

        assert!(prs.is_empty());
    }

    #[test]
    fn find_linked_pull_requests_returns_the_seeded_prs() {
        let vcs = FakeVcs::new();
        let pr = PullRequestRef {
            number: 7,
            url: "https://github.com/owner/repo/pull/7".to_string(),
            branch: "issue-42-widget-cache".to_string(),
        };
        vcs.linked_pull_requests
            .borrow_mut()
            .insert(("owner/repo".to_string(), 42), vec![pr.clone()]);

        let prs = vcs.find_linked_pull_requests("owner/repo", 42).unwrap();

        assert_eq!(prs, vec![pr]);
    }

    #[test]
    fn read_pull_request_status_errors_when_not_seeded() {
        let vcs = FakeVcs::new();

        assert!(matches!(
            vcs.read_pull_request_status("owner/repo", 7),
            Err(VcsError::CommandFailed { .. })
        ));
    }

    #[test]
    fn read_pull_request_status_returns_the_seeded_status() {
        use crate::vcs::{CiConclusion, PullRequestState};

        let vcs = FakeVcs::new();
        let status = PullRequestStatus {
            number: 7,
            state: PullRequestState::Open,
            ci: CiConclusion::Success,
            in_merge_queue: false,
        };
        vcs.pull_request_statuses
            .borrow_mut()
            .insert(("owner/repo".to_string(), 7), status);

        let read = vcs.read_pull_request_status("owner/repo", 7).unwrap();

        assert_eq!(read, status);
    }

    #[test]
    fn set_label_is_visible_to_a_subsequent_read_issue() {
        // The property the work-claim write-then-settle-read protocol
        // (§8.2) depends on: a label written via `set_label` must be
        // visible to the very next `read_issue`, exactly as it would be
        // against real GitHub.
        let vcs = FakeVcs::new();
        vcs.issues.borrow_mut().insert(
            ("owner/repo".to_string(), 42),
            IssueRef {
                number: 42,
                title: "Widget cache".to_string(),
                body: String::new(),
                labels: vec!["status:implement".to_string()],
                state: IssueState::Open,
            },
        );

        vcs.set_label("owner/repo", 42, "owner:instance-a@100", true).unwrap();

        let issue = vcs.read_issue("owner/repo", 42).unwrap();
        assert_eq!(
            issue.labels,
            vec![
                "status:implement".to_string(),
                "owner:instance-a@100".to_string(),
            ]
        );

        vcs.set_label("owner/repo", 42, "owner:instance-a@100", false)
            .unwrap();

        let issue = vcs.read_issue("owner/repo", 42).unwrap();
        assert_eq!(issue.labels, vec!["status:implement".to_string()]);
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
