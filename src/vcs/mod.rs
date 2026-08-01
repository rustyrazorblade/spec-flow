//! The `git`/`gh` subprocess layer — the server's one external adapter
//! (§2.1, §5, §8.5).
//!
//! Everything the daemon does to git/GitHub, it does by shelling out to
//! the operator's own local `git`/`gh` CLIs, project-scoped from day one
//! (`gh --repo <owner/repo>`, never ambient resolution, §8.5). This
//! module defines that seam as the [`Vcs`] trait so the rest of the
//! daemon (the phase engine, the scheduler, `spec-flow init`) can be
//! unit-tested by stubbing it (§5: "everything but the git/gh layer...
//! is unit-testable by stubbing those seams") rather than shelling out
//! for real in every test.
//!
//! - [`shell`] — [`shell::ShellVcs`], the real implementation. Fully
//!   implemented (§14 step 1) — no `todo!()`s remain.
//! - [`fake`] — [`fake::FakeVcs`], an in-memory test double implementing
//!   the same trait.
//!
//! This trait's method signatures (argument/return shapes) are load-
//! bearing for both implementations and every call site once the phase
//! engine (a later step) lands; change one and update every caller in
//! the same change.

mod fake;
mod shell;

pub use fake::FakeVcs;
pub use shell::ShellVcs;

use std::path::{Path, PathBuf};

/// Errors from a `git`/`gh` subprocess call.
///
/// Every variant is something a caller (the phase engine, `spec-flow init`)
/// may want to react to differently — a missing binary is a setup
/// problem to surface at `init`; a non-zero exit or malformed JSON is a
/// per-call failure to report on the issue.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VcsError {
    /// The configured `git` or `gh` binary could not be found or
    /// executed.
    #[error("`{binary}` was not found on PATH or at the configured location")]
    BinaryNotFound {
        /// The binary that could not be located (e.g. `"git"`, `"gh"`,
        /// or a configured absolute path).
        binary: String,
    },

    /// Spawning the subprocess itself failed (distinct from the
    /// subprocess running and exiting non-zero).
    #[error("failed to spawn `{command}`")]
    Spawn {
        /// The command line that failed to spawn, for diagnostics.
        command: String,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The subprocess ran and exited with a non-zero status.
    #[error("`{command}` exited with status {status}: {stderr}")]
    CommandFailed {
        /// The command line that failed, for diagnostics.
        command: String,
        /// The process exit status (platform-specific; -1 when the
        /// process was terminated by a signal rather than exiting).
        status: i32,
        /// The captured stderr, trimmed.
        stderr: String,
    },

    /// The subprocess exited successfully but its output was not the
    /// JSON shape the caller expected.
    #[error("failed to parse `{command}` output as JSON")]
    InvalidJson {
        /// The command line whose output failed to parse.
        command: String,
        /// The underlying parse failure.
        #[source]
        source: serde_json::Error,
    },

    /// `gh` is not authenticated for the given repo/host.
    #[error("gh is not authenticated for repo `{repo}`")]
    NotAuthenticated {
        /// The repo slug (`owner/repo`) authentication was checked
        /// against.
        repo: String,
    },

    /// The subprocess exited successfully with output that was not
    /// JSON at all (so [`VcsError::InvalidJson`] doesn't apply) but
    /// still didn't match the plain-text shape the caller expected —
    /// e.g. [`Vcs::create_issue`] expecting `gh issue create`'s stdout
    /// to be a `.../issues/<number>` URL.
    #[error("unexpected output from `{command}`: {output:?}")]
    UnexpectedOutput {
        /// The command line whose output didn't match.
        command: String,
        /// The actual output received.
        output: String,
    },
}

/// A local worktree/branch checkout for one issue (§4.2, §10.1).
///
/// Always a sibling directory inside the owning project's
/// `project_dir` — `path == project_dir.join(branch)` by construction
/// (§10.1); never nested inside another checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worktree {
    /// The issue this worktree was created for.
    pub issue_number: u64,

    /// The worktree's path on disk: `<project_dir>/<branch>`.
    pub path: PathBuf,

    /// The branch checked out in this worktree.
    pub branch: String,

    /// The short human-readable slug embedded in `branch`
    /// (e.g. `widget-cache` in `issue-42-widget-cache`).
    pub slug: String,
}

/// The short slug embedded in `branch` (`widget-cache` in
/// `issue-42-widget-cache`), or the whole branch when it does not carry
/// the conventional prefix.
///
/// Shared by [`ShellVcs`] and [`FakeVcs`] so the fake's [`Worktree`]s
/// carry the same slug the real implementation would derive, rather
/// than each `impl Vcs` re-deriving (or, for the fake, skipping) it —
/// a test asserting on `slug` through the fake would otherwise assert
/// the wrong value.
pub(crate) fn branch_slug(branch: &str, issue_number: u64) -> String {
    branch
        .strip_prefix(&format!("issue-{issue_number}-"))
        .unwrap_or(branch)
        .to_string()
}

/// Whether a GitHub issue is open or closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueState {
    /// The issue is open.
    Open,
    /// The issue is closed.
    Closed,
}

/// A GitHub issue as read via `gh` (§4.2).
///
/// This is the raw shape `gh issue view` gives back. Deriving the
/// higher-level fields the phase engine cares about — `status:*` /
/// `gate:*` / `owner:*` labels, priority — is that engine's job (a
/// later task), not this layer's; `labels` carries everything it needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IssueRef {
    /// The issue number (scoped to its repo — always pair with the
    /// repo slug when persisting or logging, §15).
    pub number: u64,
    /// The issue title.
    pub title: String,
    /// The issue body (markdown).
    pub body: String,
    /// Every label currently on the issue.
    pub labels: Vec<String>,
    /// Open or closed.
    pub state: IssueState,
}

/// A pull request as read back after `open_pr` (§6 `start_implement`,
/// §7.2 ph.6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestRef {
    /// The PR number.
    pub number: u64,
    /// The PR's HTML URL, for posting a clickable link (§15).
    pub url: String,
    /// The head branch the PR was opened from.
    pub branch: String,
}

/// An issue's dependency/hierarchy edges (§4.2, §8.4), read via
/// `gh api graphql`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IssueRelationships {
    /// Issues that must merge before this one can enqueue (§8.1).
    pub blocked_by: Vec<u64>,
    /// Issues blocked on this one.
    pub blocking: Vec<u64>,
    /// The parent issue, if this is a sub-issue.
    pub parent: Option<u64>,
    /// This issue's sub-issues, if any.
    pub sub_issues: Vec<u64>,
}

/// Whether a pull request is open, closed, or merged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullRequestState {
    /// The pull request is open.
    Open,
    /// The pull request was closed without merging.
    Closed,
    /// The pull request was merged.
    Merged,
}

/// A pull request's CI conclusion, aggregated across every check run
/// against its latest commit (§4.2 `signals`, §12).
///
/// This collapses GitHub's richer per-check-run/per-status-context state
/// (`StatusState`: `EXPECTED`/`ERROR`/`FAILURE`/`PENDING`/`SUCCESS`) down
/// to the three-way signal the phase engine's `requires: {ci: green}`
/// gate (§7.1, §11.3) actually branches on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CiConclusion {
    /// At least one check is still running, or the commit has no
    /// reported checks at all yet (a PR the daemon just opened, before
    /// Actions has even started, must not read as `Success` by default).
    Pending,
    /// Every reported check succeeded.
    Success,
    /// At least one check failed or errored.
    Failure,
}

/// A pull request's current state, CI conclusion, and merge-queue
/// enrollment (§4.2 `signals`), as read by
/// [`Vcs::read_pull_request_status`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PullRequestStatus {
    /// The pull request number this status was read for.
    pub number: u64,
    /// Open, closed, or merged.
    pub state: PullRequestState,
    /// The aggregated CI conclusion for its latest commit.
    pub ci: CiConclusion,
    /// Whether the pull request currently holds a merge-queue entry
    /// (§8.1) — queued, awaiting checks, or otherwise still enrolled.
    pub in_merge_queue: bool,
}

/// The `git`/`gh` subprocess seam (§5, §8.5).
///
/// One method per mechanical operation the daemon performs directly
/// (§1.2, §6): every call is implicitly scoped to a single project's
/// repo (`gh --repo <owner/repo>`, §8.5) — callers pass that repo slug
/// explicitly rather than relying on the daemon's own working directory,
/// since one daemon serves many projects and is never "in" any one
/// repo's checkout (§2.5).
///
/// [`ensure_ready`](Vcs::ensure_ready) is the trait's one default
/// method, composed from the required presence/auth primitives — the
/// pattern the style guide asks for (minimal required methods, richer
/// behavior layered on top) even though most operations here are
/// independent CLI calls with no natural shared primitive.
pub trait Vcs {
    /// Verify the configured `git` binary is present and runnable.
    fn git_available(&self) -> Result<(), VcsError>;

    /// Verify the configured `gh` binary is present and runnable.
    fn gh_available(&self) -> Result<(), VcsError>;

    /// Whether `gh` is authenticated for `repo`.
    fn gh_authenticated(&self, repo: &str) -> Result<bool, VcsError>;

    /// Detect `repo_dir`'s GitHub slug (`owner/repo`) from inside the
    /// repo, where `gh`'s own resolution is unambiguous (§8.5).
    fn detect_repo_slug(&self, repo_dir: &Path) -> Result<String, VcsError>;

    /// Detect `repo_dir`'s default branch.
    fn detect_default_branch(
        &self,
        repo_dir: &Path,
    ) -> Result<String, VcsError>;

    /// Whether `repo`'s default branch has GitHub's native merge queue
    /// enabled (§8.1). `init` uses this to set `merge_mode`; a `false`
    /// here is a warn-and-degrade, never a hard-fail.
    fn merge_queue_enabled(
        &self,
        repo: &str,
        default_branch: &str,
    ) -> Result<bool, VcsError>;

    /// Create the sibling worktree + branch for `issue_number` under
    /// `project_dir` (§10.1).
    ///
    /// `primary_checkout` is the project's known primary checkout
    /// (`ProjectConfig::local_path`) — the git command needs an
    /// existing working tree to run from, and it must be *this* repo's,
    /// never guessed from whatever happens to sit in `project_dir` (a
    /// stray unrelated clone next to the primary checkout must not
    /// silently receive the new branch/worktree).
    fn worktree_add(
        &self,
        primary_checkout: &Path,
        project_dir: &Path,
        branch: &str,
        issue_number: u64,
    ) -> Result<Worktree, VcsError>;

    /// Remove a worktree and prune its branch.
    ///
    /// `primary_checkout` is run from in place of `worktree`, which
    /// stops being a valid working directory partway through removal —
    /// see [`worktree_add`](Vcs::worktree_add) on why it must be passed
    /// in rather than guessed.
    fn worktree_remove(
        &self,
        primary_checkout: &Path,
        worktree: &Worktree,
    ) -> Result<(), VcsError>;

    /// Commit all pending changes in `worktree_path` with `message`.
    fn commit(
        &self,
        worktree_path: &Path,
        message: &str,
    ) -> Result<(), VcsError>;

    /// Push `branch` from `worktree_path` to its remote.
    fn push(&self, worktree_path: &Path, branch: &str)
    -> Result<(), VcsError>;

    /// Create an issue in `repo` (§6 `create_issue`, §7.2 ph.1's "the
    /// server creates/labels the GitHub issue"). Returns the created
    /// issue read back in full — freshly assigned `number`, `state:
    /// Open`, and no labels yet (§4.3's gate/status stamping is a
    /// separate [`Vcs::set_label`] call, not this one's job).
    fn create_issue(
        &self,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<IssueRef, VcsError>;

    /// Read one issue from `repo`.
    fn read_issue(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> Result<IssueRef, VcsError>;

    /// Add (`present = true`) or remove (`present = false`) a label on
    /// an issue. A single boolean-setter method per the style guide's
    /// `yes: bool` convention, rather than two near-identical methods.
    ///
    /// [`ShellVcs`]'s implementation currently cannot add a label name
    /// that doesn't already exist in the repo — see
    /// `ShellVcs::set_label`'s doc for the confirmed, unresolved
    /// conflict this creates with `crate::claim::write_claim`.
    fn set_label(
        &self,
        repo: &str,
        issue_number: u64,
        label: &str,
        present: bool,
    ) -> Result<(), VcsError>;

    /// Create `label` in `repo`'s label set if it does not already exist
    /// (§14 step 9) — the prerequisite [`Vcs::set_label`]'s own doc
    /// identifies: `gh issue edit --add-label` never auto-creates a
    /// label name. Idempotent: calling this again for a label that
    /// already exists is a no-op success, so `spec-flow init`'s
    /// re-running-is-safe contract (§6 `init`, §11.1) holds for it too —
    /// and, just as importantly, leaves an already-existing label's
    /// color/description exactly as a team left them; this is a
    /// create-if-absent, never an upsert.
    ///
    /// Scoped to the **fixed, config-declared** label vocabulary
    /// (`crate::workflow::label_vocabulary`) — every name this crate ever
    /// calls this with is known in full at `init` time and never changes
    /// between calls. This does **not** resolve
    /// [`crate::claim::write_claim`]'s label-preexistence conflict: that
    /// one is unresolved because each `owner:<instance>@<epoch>` name is
    /// different on every heartbeat, so pre-creating it here would mean
    /// pre-creating a label for every future epoch, which is not what
    /// this method is for.
    fn ensure_label(&self, repo: &str, label: &str) -> Result<(), VcsError>;

    /// Post a comment on an issue (or PR — GitHub treats PRs as issues
    /// for commenting purposes).
    fn post_comment(
        &self,
        repo: &str,
        issue_number: u64,
        body: &str,
    ) -> Result<(), VcsError>;

    /// Open a PR from `branch` onto `base`.
    fn open_pr(
        &self,
        repo: &str,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequestRef, VcsError>;

    /// Enqueue `pr_number` to GitHub's native merge queue (§8.1).
    fn enqueue_merge(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> Result<(), VcsError>;

    /// Read an issue's `blockedBy`/`blocking`/`parent`/`subIssues`
    /// edges via `gh api graphql` (§8.4).
    fn read_relationships(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> Result<IssueRelationships, VcsError>;

    /// Find every pull request GitHub's "Development" linked-PR feature
    /// associates with `issue_number` — the PRs that would close it on
    /// merge (§4.1, §4.2 `signals`), via `gh api graphql`.
    ///
    /// Returns every linked PR GitHub reports, in the order it reports
    /// them (an empty vec when the issue has none). The common case is
    /// exactly one, per the 1:1:1:1 invariant (§4.1); this layer makes no
    /// assumption about that and does no picking — a caller that finds
    /// more than one is looking at a case a later step defines the
    /// meaning of, not something to silently collapse here.
    fn find_linked_pull_requests(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> Result<Vec<PullRequestRef>, VcsError>;

    /// Read `pr_number`'s current state, CI conclusion, and merge-queue
    /// enrollment (§4.2 `signals`, §12 CI/PR poller), via
    /// `gh api graphql`.
    fn read_pull_request_status(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> Result<PullRequestStatus, VcsError>;

    /// Verify `git` and `gh` are present and `gh` is authenticated for
    /// `repo` — the composite check `spec-flow init` runs up front (§6).
    ///
    /// A default method built on the required primitives above, per the
    /// style guide's "minimal required methods, default methods on top"
    /// trait-design rule.
    fn ensure_ready(&self, repo: &str) -> Result<(), VcsError> {
        self.git_available()?;
        self.gh_available()?;
        if !self.gh_authenticated(repo)? {
            return Err(VcsError::NotAuthenticated { repo: repo.to_string() });
        }
        Ok(())
    }
}
