//! [`ShellVcs`] — the real [`Vcs`](super::Vcs) implementation, shelling
//! out to the operator's local `git`/`gh` (§2.1, §8.5).
//!
//! Every method here is the same three steps: build an argument vector,
//! hand it to a [`CommandRunner`], parse what came back. The runner is a
//! seam (see [`runner`]) rather than a direct `std::process::Command`
//! call in each body, which keeps argument construction and output
//! parsing — the parts that carry the §8.5 repo-scoping rule and the
//! §8.4 GraphQL shape — testable without a repository or an
//! authenticated `gh`.
//!
//! # Repo scoping (§8.5)
//!
//! One daemon serves many projects, so `gh`'s ambient resolution is
//! never correct here: every `gh` call that acts on a project's repo is
//! built through [`ShellVcs::gh_args`], which appends
//! `--repo <owner/repo>`. Three calls scope themselves another way, each
//! deliberate and documented at its call site: [`Vcs::detect_repo_slug`]
//! runs *inside* a checkout precisely to learn the slug;
//! `gh repo view <slug>` takes its repo positionally because the
//! subcommand has no `--repo` flag; and `gh api graphql` scopes itself
//! through the query's `owner`/`name` variables. None of them fall back
//! to `gh`'s ambient resolution.

mod runner;

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::warn;

use self::runner::{
    CommandOutput, CommandRunner, ProcessRunner, command_line,
};
use super::{
    CiConclusion, IssueRef, IssueRelationships, IssueState, PullRequestRef,
    PullRequestState, PullRequestStatus, Vcs, VcsError, Worktree, branch_slug,
};

/// The `gh issue view` fields [`Vcs::read_issue`] requests, in the order
/// [`IssueJson`] declares them.
const ISSUE_FIELDS: &str = "number,title,body,labels,state";

/// The `gh issue list` fields [`Vcs::list_open_issues`] requests.
///
/// Just the number: see that method's doc for why it deliberately does
/// not fetch the rest of an issue here.
const ISSUE_LIST_FIELDS: &str = "number";

/// The `gh pr view` fields [`Vcs::open_pr`] reads back after creating a
/// pull request.
const PULL_REQUEST_FIELDS: &str = "number,url,headRefName";

/// Whether the default branch has GitHub's native merge queue (§8.1).
///
/// `mergeQueue` is null when the branch has no queue configured, which
/// is the signal `init` turns into `merge_mode: serialized`.
const MERGE_QUEUE_QUERY: &str = "\
query($owner: String!, $name: String!, $branch: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue(branch: $branch) { id }
  }
}";

/// An issue's native GitHub relationship edges (§8.4).
///
/// `blockedBy`/`blocking` are the dependency edges the scheduler gates
/// merges on; `parent`/`subIssues` are the hierarchy. Orgs with the
/// native feature switched off reject this query, which is what drives
/// the `Depends on #N` body fallback in [`Vcs::read_relationships`].
const RELATIONSHIPS_QUERY: &str = "\
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      parent { number }
      subIssues(first: 100) { nodes { number } }
      blockedBy(first: 100) { nodes { number } }
      blocking(first: 100) { nodes { number } }
    }
  }
}";

/// GitHub's "Development" linked-PR feature for one issue (§4.1, §4.2
/// `signals`).
///
/// `closedByPullRequestsReferences` is confirmed present on the live
/// GraphQL schema (checked via `gh api graphql` introspection against
/// `__type(name: "Issue")`, and end-to-end against a real issue/PR pair,
/// while building [`Vcs::find_linked_pull_requests`]).
/// `includeClosedPrs: true` so a closed-without-merging PR still
/// surfaces — the caller decides what a stale link means, not this
/// layer.
const LINKED_PULL_REQUESTS_QUERY: &str = "\
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      closedByPullRequestsReferences(first: 10, includeClosedPrs: true) {
        nodes { number url headRefName }
      }
    }
  }
}";

/// A pull request's state, CI conclusion, and merge-queue enrollment
/// (§4.2 `signals`, §12).
///
/// `mergeQueueEntry` is non-null exactly when the pull request currently
/// holds a merge-queue entry (§8.1). `commits(last: 1).../
/// statusCheckRollup.state` is the same aggregated rollup `gh pr view
/// --json statusCheckRollup` exposes as a per-check list, but as a single
/// `StatusState` already reduced across every check on the latest commit
/// (confirmed via introspection on `Commit.statusCheckRollup` and a live
/// query against a real pull request while building
/// [`Vcs::read_pull_request_status`]) — no client-side aggregation needed.
const PULL_REQUEST_STATUS_QUERY: &str = "\
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      state
      mergeQueueEntry { state }
      commits(last: 1) {
        nodes { commit { statusCheckRollup { state } } }
      }
    }
  }
}";

/// Shells out to the operator's local `git`/`gh` binaries.
///
/// Construct with [`ShellVcs::new`], naming the two binaries to invoke
/// (a bare name to resolve on `PATH`, or an absolute path — see
/// [`crate::config::Binaries`]).
#[derive(Clone, Debug)]
pub struct ShellVcs {
    git_binary: String,
    gh_binary: String,
    // `dyn` rather than a generic parameter: the runner is swapped once,
    // at construction, and one vtable call is free next to a fork/exec.
    // Keeping it dynamic also keeps the seam private — `ShellVcs` stays
    // a plain type in this crate's public API.
    runner: Arc<dyn CommandRunner>,
}

impl ShellVcs {
    /// Create a [`ShellVcs`] that invokes `git_binary` and `gh_binary`
    /// (bare names resolve on `PATH`; an absolute path pins a specific
    /// install, §8.5).
    pub fn new(git_binary: String, gh_binary: String) -> ShellVcs {
        ShellVcs::with_runner(git_binary, gh_binary, Arc::new(ProcessRunner))
    }

    /// Create a [`ShellVcs`] over an explicit [`CommandRunner`].
    fn with_runner(
        git_binary: String,
        gh_binary: String,
        runner: Arc<dyn CommandRunner>,
    ) -> ShellVcs {
        ShellVcs { git_binary, gh_binary, runner }
    }

    /// Build a repo-scoped `gh` argument vector: `command` followed by
    /// `--repo <slug>`.
    ///
    /// The single choke point for §8.5 — a `gh` call that acts on a
    /// project's repo goes through here, so it cannot silently fall back
    /// to `gh`'s ambient resolution and cross-talk with another
    /// project's account.
    fn gh_args<'a>(command: &[&'a str], repo: &'a str) -> Vec<&'a str> {
        let mut args = command.to_vec();
        args.push("--repo");
        args.push(repo);
        args
    }

    /// Split `owner/repo` into its GraphQL `owner` and `name` variables.
    ///
    /// A slug without a `/` is passed through as the owner with an empty
    /// name so GitHub rejects it by name in its own error message,
    /// rather than this layer inventing a validation error for a value
    /// only `init` can have got wrong.
    fn split_repo(repo: &str) -> (&str, &str) {
        repo.split_once('/').unwrap_or((repo, ""))
    }

    /// Run `program`, returning its captured output whatever the exit
    /// status.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<CommandOutput, VcsError> {
        self.runner.run(program, args, cwd)
    }

    /// Run `program` and return its stdout, mapping a non-zero exit to
    /// [`VcsError::CommandFailed`].
    fn run_checked(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<String, VcsError> {
        let output = self.run(program, args, cwd)?;
        if output.is_success() {
            return Ok(output.stdout);
        }
        Err(VcsError::CommandFailed {
            command: command_line(program, args),
            status: output.status,
            stderr: output.stderr.trim().to_string(),
        })
    }

    /// Run `program` and deserialize its stdout as JSON.
    fn run_json<T: DeserializeOwned>(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<T, VcsError> {
        let stdout = self.run_checked(program, args, cwd)?;
        serde_json::from_str(&stdout).map_err(|source| VcsError::InvalidJson {
            command: command_line(program, args),
            source,
        })
    }

    /// Run a `gh <noun> list --json ...` style `program` and deserialize
    /// its stdout as a JSON array.
    ///
    /// `gh`'s list subcommands (confirmed live against `gh` 2.83.0, e.g.
    /// `gh label list --search <term> --json name` when `<term>` matches
    /// nothing) print **nothing at all** on a zero-result list, not the
    /// `[]` a `--json` consumer would expect — despite exiting `0`. Every
    /// other JSON-returning call in this file (`gh api graphql`, `gh ...
    /// view`) always emits a well-formed body, so this empty-stdout
    /// carve-out is scoped to list subcommands specifically rather than
    /// folded into [`ShellVcs::run_json`], where it would silently turn a
    /// genuinely-empty single-object response (e.g. a `view` racing a
    /// deletion) into a fabricated default instead of a loud error.
    fn run_json_list<T: DeserializeOwned>(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<Vec<T>, VcsError> {
        let stdout = self.run_checked(program, args, cwd)?;
        if stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&stdout).map_err(|source| VcsError::InvalidJson {
            command: command_line(program, args),
            source,
        })
    }

    /// Run `binary --version` and report whether it succeeded.
    fn binary_available(&self, binary: &str) -> Result<(), VcsError> {
        match self.run(binary, &["--version"], None) {
            Ok(output) if output.is_success() => Ok(()),
            Ok(_) | Err(_) => {
                Err(VcsError::BinaryNotFound { binary: binary.to_string() })
            }
        }
    }

    /// Parse `Depends on #N` edges out of an issue body (§8.4).
    ///
    /// The fallback for orgs with GitHub's native issue dependencies
    /// switched off. Only `#N` references on a line that mentions
    /// "depends on" count, so an unrelated issue cross-reference
    /// elsewhere in the body is not mistaken for a dependency.
    fn parse_depends_on(body: &str) -> Vec<u64> {
        let mut numbers = Vec::new();
        for line in body.lines() {
            if !line.to_lowercase().contains("depends on") {
                continue;
            }
            for candidate in line.split('#').skip(1) {
                let digits: String = candidate
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(number) = digits.parse::<u64>() {
                    numbers.push(number);
                }
            }
        }
        numbers
    }
}

impl Vcs for ShellVcs {
    fn git_available(&self) -> Result<(), VcsError> {
        self.binary_available(&self.git_binary)
    }

    fn gh_available(&self) -> Result<(), VcsError> {
        self.binary_available(&self.gh_binary)
    }

    fn gh_authenticated(&self, repo: &str) -> Result<bool, VcsError> {
        // Two questions, two calls: is `gh` logged in at all, and can
        // that login actually reach *this* project's repo (§8.5)? The
        // second is the one that matters when one `gh` serves several
        // accounts, and the split makes the warning below say which of
        // the two failed.
        let status = self.run(&self.gh_binary, &["auth", "status"], None)?;
        if !status.is_success() {
            warn!(
                repo = repo,
                stderr = status.stderr.trim(),
                "gh reports no authenticated host"
            );
            return Ok(false);
        }

        // `gh repo view` takes its repo positionally; there is no
        // `--repo` flag on this subcommand, so `gh_args` does not apply.
        let args = ["repo", "view", repo, "--json", "nameWithOwner"];
        let reachable = self.run(&self.gh_binary, &args, None)?;
        if !reachable.is_success() {
            warn!(
                repo = repo,
                stderr = reachable.stderr.trim(),
                "gh is authenticated but cannot reach this repo"
            );
            return Ok(false);
        }

        Ok(true)
    }

    fn detect_repo_slug(&self, repo_dir: &Path) -> Result<String, VcsError> {
        // The one deliberately unscoped `gh` call: run from inside the
        // checkout, where `gh`'s own resolution is unambiguous, to learn
        // the slug every later call is scoped with (§8.5).
        let view: RepoSlugJson = self.run_json(
            &self.gh_binary,
            &["repo", "view", "--json", "nameWithOwner"],
            Some(repo_dir),
        )?;
        Ok(view.name_with_owner)
    }

    fn detect_default_branch(
        &self,
        repo_dir: &Path,
    ) -> Result<String, VcsError> {
        // The clone already mirrors the remote's HEAD, so this needs no
        // network and no `gh` authentication — `init` can determine the
        // default branch before it has verified credentials.
        let stdout = self.run_checked(
            &self.git_binary,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            Some(repo_dir),
        )?;
        let branch = stdout.trim();
        Ok(branch.strip_prefix("origin/").unwrap_or(branch).to_string())
    }

    fn merge_queue_enabled(
        &self,
        repo: &str,
        default_branch: &str,
    ) -> Result<bool, VcsError> {
        let (owner, name) = ShellVcs::split_repo(repo);
        let owner_var = format!("owner={owner}");
        let name_var = format!("name={name}");
        let branch_var = format!("branch={default_branch}");
        let query_var = format!("query={MERGE_QUEUE_QUERY}");
        let args = [
            "api",
            "graphql",
            "-f",
            &query_var,
            "-f",
            &owner_var,
            "-f",
            &name_var,
            "-f",
            &branch_var,
        ];

        // §8.1: a repo without the native queue is the expected case on
        // most installations, and an org that has the field switched off
        // rejects the query outright. Both degrade to `serialized`, so
        // neither is an error — only an unrunnable `gh` is.
        let output = self.run(&self.gh_binary, &args, None)?;
        if !output.is_success() {
            warn!(
                repo = repo,
                branch = default_branch,
                stderr = output.stderr.trim(),
                "merge queue query failed; assuming no native queue"
            );
            return Ok(false);
        }

        let response: GraphQlResponse<MergeQueueData> =
            match serde_json::from_str(&output.stdout) {
                Ok(response) => response,
                Err(error) => {
                    warn!(
                        repo = repo,
                        stdout = output.stdout.trim(),
                        error = %error,
                        "merge queue query returned unparseable output; \
                         assuming no native queue"
                    );
                    return Ok(false);
                }
            };

        Ok(response
            .data
            .repository
            .and_then(|repository| repository.merge_queue)
            .is_some())
    }

    fn worktree_add(
        &self,
        primary_checkout: &Path,
        project_dir: &Path,
        branch: &str,
        issue_number: u64,
    ) -> Result<Worktree, VcsError> {
        let path = project_dir.join(branch);
        let path_arg = path.to_string_lossy().into_owned();

        self.run_checked(
            &self.git_binary,
            &["worktree", "add", "-b", branch, &path_arg],
            Some(primary_checkout),
        )?;

        Ok(Worktree {
            issue_number,
            path,
            branch: branch.to_string(),
            slug: branch_slug(branch, issue_number),
        })
    }

    fn worktree_remove(
        &self,
        primary_checkout: &Path,
        worktree: &Worktree,
    ) -> Result<(), VcsError> {
        let path_arg = worktree.path.to_string_lossy().into_owned();

        self.run_checked(
            &self.git_binary,
            &["worktree", "remove", &path_arg],
            Some(primary_checkout),
        )?;

        // `--force` because a squash-merged branch is not "merged" as
        // far as `git branch --delete` is concerned. Nothing is lost:
        // the work reached GitHub through the PR branch before finalize
        // ever runs (§7.2 ph.8). A failure here leaves a stale local
        // branch, not a stale worktree, so it is worth a warning rather
        // than failing the finalize that already succeeded.
        if let Err(error) = self.run_checked(
            &self.git_binary,
            &["branch", "--delete", "--force", &worktree.branch],
            Some(primary_checkout),
        ) {
            warn!(
                branch = worktree.branch,
                error = %error,
                "worktree removed but its local branch could not be deleted"
            );
        }

        Ok(())
    }

    fn commit(
        &self,
        worktree_path: &Path,
        message: &str,
    ) -> Result<(), VcsError> {
        self.run_checked(
            &self.git_binary,
            &["add", "--all"],
            Some(worktree_path),
        )?;
        self.run_checked(
            &self.git_binary,
            &["commit", "--message", message],
            Some(worktree_path),
        )?;
        Ok(())
    }

    fn push(
        &self,
        worktree_path: &Path,
        branch: &str,
    ) -> Result<(), VcsError> {
        // `--set-upstream` unconditionally: it establishes the tracking
        // ref on the first push and is a no-op on every one after.
        self.run_checked(
            &self.git_binary,
            &["push", "--set-upstream", "origin", branch],
            Some(worktree_path),
        )?;
        Ok(())
    }

    fn create_issue(
        &self,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<IssueRef, VcsError> {
        let create = ShellVcs::gh_args(
            &["issue", "create", "--title", title, "--body", body],
            repo,
        );
        // `gh issue create` prints only the new issue's URL
        // (`.../issues/<number>`) — read it back by number for a typed
        // result rather than trying to parse anything more than that one
        // trailing number out of the URL, mirroring `open_pr`'s "create,
        // then view by a stable key" split.
        let url = self.run_checked(&self.gh_binary, &create, None)?;
        let number = url
            .trim()
            .rsplit('/')
            .next()
            .and_then(|segment| segment.parse::<u64>().ok())
            .ok_or_else(|| VcsError::UnexpectedOutput {
                command: command_line(&self.gh_binary, &create),
                output: url.clone(),
            })?;

        self.read_issue(repo, number)
    }

    fn read_issue(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> Result<IssueRef, VcsError> {
        let number = issue_number.to_string();
        let args = ShellVcs::gh_args(
            &["issue", "view", &number, "--json", ISSUE_FIELDS],
            repo,
        );

        let issue: IssueJson = self.run_json(&self.gh_binary, &args, None)?;

        Ok(IssueRef {
            number: issue.number,
            title: issue.title,
            body: issue.body,
            labels: issue.labels.into_iter().map(|label| label.name).collect(),
            state: match issue.state {
                IssueStateJson::Open => IssueState::Open,
                IssueStateJson::Closed => IssueState::Closed,
            },
        })
    }

    /// # `--state open` is passed explicitly, `--limit` always
    ///
    /// `gh issue list` already defaults to `--state open` and
    /// `--limit 30` (confirmed against `gh` 2.83.0's own `--help`, and by
    /// running the command live). Neither default is relied on: the state
    /// filter is the whole contract of
    /// [`Vcs::list_open_issues`](super::Vcs::list_open_issues) and must
    /// not silently change under a future `gh`, and a caller that asked
    /// for a specific `limit` must get that one, not 30. Both are
    /// therefore spelled out in the argument vector.
    ///
    /// `gh issue list` returns pull requests' sibling resource — issues
    /// only — so no PR filtering is needed here (unlike GitHub's REST
    /// `/issues` endpoint, which folds PRs in).
    fn list_open_issues(
        &self,
        repo: &str,
        limit: u32,
    ) -> Result<Vec<u64>, VcsError> {
        let limit = limit.to_string();
        let args = ShellVcs::gh_args(
            &[
                "issue",
                "list",
                "--state",
                "open",
                "--limit",
                &limit,
                "--json",
                ISSUE_LIST_FIELDS,
            ],
            repo,
        );

        let issues: Vec<IssueNumberJson> =
            self.run_json_list(&self.gh_binary, &args, None)?;

        Ok(issues.into_iter().map(|issue| issue.number).collect())
    }

    /// # `label` must survive `gh`'s CSV-based flag parsing
    ///
    /// `--add-label`/`--remove-label` are `pflag` `StringSliceVar`
    /// flags, parsed as a single CSV record (confirmed by reading `gh`'s
    /// and `spf13/pflag`'s source, not just its `--help` text): a comma
    /// splits the value into multiple labels, a newline **silently
    /// truncates** it at the first line break (no error — the tail is
    /// just gone), and a `"` fails the parse outright. This method does
    /// not re-validate `label`; every caller composing a label from
    /// dynamic/user-influenced text (`crate::claim::write_claim`'s
    /// `owner:<instance_id>@<epoch>`) is responsible for keeping its own
    /// input within a safe character set first — see
    /// `crate::claim::is_valid_instance_id`.
    ///
    /// # CONFIRMED: `gh issue edit --add-label` requires the label to
    /// already exist in the repository — it does not auto-create
    ///
    /// Read from `gh`'s own source (`api.RepoMetadataResult.LabelsToIDs`
    /// in `cli/cli`, which `gh issue edit`'s add-label path calls to
    /// resolve label names to GraphQL IDs before mutating): an unknown
    /// label name returns `'<name>' not found`, full stop — there is no
    /// auto-create branch. This is a real, unresolved conflict with how
    /// `crate::claim::write_claim` is specified to work (§8.2): its
    /// `owner:<instance>@<epoch>` label encodes a fresh epoch in the
    /// label's own **name** on every heartbeat rewrite, so every single
    /// heartbeat this method is asked to add is, by construction, a
    /// label name that has never existed in the repo before — this call
    /// will fail against a real GitHub repository on every attempt.
    /// Making the claim protocol actually work needs a design decision
    /// this method must not make unilaterally (pre-create every label
    /// via a separate `gh label create` call before each add, which
    /// would permanently accumulate one label per heartbeat per
    /// instance per issue in the repo's label list; move the mutable
    /// heartbeat data out of the label name entirely, e.g. into an
    /// issue comment or a stable `owner:<instance>` label's own
    /// last-updated timestamp; or something else) — flagged here for
    /// whoever wires `write_claim` into a live `serve` daemon (§14 step
    /// 6+), not silently worked around in this step. Whatever resolution
    /// is chosen must also respect GitHub's ~50-character label-name
    /// cap: any encoding that keeps `<instance>@<epoch>` in the name
    /// itself leaves only ~33 characters for `instance_id` after the
    /// `owner:` prefix and a 10-digit epoch, well under the width of
    /// e.g. a UUID `instance_id` — a candidate `serve` auto-generation
    /// format (§11.1) hasn't picked one yet.
    fn set_label(
        &self,
        repo: &str,
        issue_number: u64,
        label: &str,
        present: bool,
    ) -> Result<(), VcsError> {
        let number = issue_number.to_string();
        let flag = if present { "--add-label" } else { "--remove-label" };
        let args =
            ShellVcs::gh_args(&["issue", "edit", &number, flag, label], repo);

        self.run_checked(&self.gh_binary, &args, None)?;
        Ok(())
    }

    fn ensure_label(&self, repo: &str, label: &str) -> Result<(), VcsError> {
        // Checks existence first via `gh label list --search`, rather
        // than calling `gh label create --force` unconditionally:
        // `--force` PATCHes an already-existing label, and its
        // `--color` argument would silently repaint whatever color a
        // team's own P0-P3/status labels already had — a real,
        // `gh`-source-confirmed clobber this crate's `init` should no
        // more commit than it commits `scaffold_spec_flow_dir`'s
        // file-clobbering.
        //
        // Deliberately NOT `gh api repos/{owner}/{repo}/labels/{label}`
        // (GitHub's "get a label" endpoint, the more obvious existence
        // check): `gh api`'s endpoint argument runs through `gh`'s own
        // `:owner`/`:repo`/`:branch` placeholder substitution (confirmed
        // in `cli/cli`'s `pkg/cmd/api/api.go`) with no word-boundary
        // guard, so a label whose name happened to contain `:repo` or
        // `:branch` immediately followed by a word character (e.g. a
        // team-added `gate:repository-sync`) would have that substring
        // silently rewritten inside the request path. `--search` is an
        // ordinary CLI flag value, never routed through that
        // substitution, so no label name can trigger it — and `--search`
        // being a fuzzy substring match rather than an exact one doesn't
        // matter here, since the exact-match check happens client-side
        // below regardless of how many (or few) candidates it returns.
        // `--limit` well above `gh label list`'s default page size (30):
        // a repo with enough same-substring labels to page the exact
        // match out of the default-sized result would otherwise spuriously
        // attempt to re-create an already-existing label (a loud,
        // deterministic "already exists" failure — never a silent
        // wrong answer — but avoidable outright at negligible cost).
        let args = ShellVcs::gh_args(
            &[
                "label", "list", "--search", label, "--json", "name",
                "--limit", "999",
            ],
            repo,
        );
        let existing: Vec<LabelJson> =
            self.run_json_list(&self.gh_binary, &args, None)?;
        if existing.iter().any(|found| found.name == label) {
            return Ok(());
        }

        let args = ShellVcs::gh_args(
            &["label", "create", label, "--color", "ededed"],
            repo,
        );
        self.run_checked(&self.gh_binary, &args, None)?;
        Ok(())
    }

    fn post_comment(
        &self,
        repo: &str,
        issue_number: u64,
        body: &str,
    ) -> Result<(), VcsError> {
        let number = issue_number.to_string();
        let args = ShellVcs::gh_args(
            &["issue", "comment", &number, "--body", body],
            repo,
        );

        self.run_checked(&self.gh_binary, &args, None)?;
        Ok(())
    }

    fn open_pr(
        &self,
        repo: &str,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequestRef, VcsError> {
        let create = ShellVcs::gh_args(
            &[
                "pr", "create", "--head", branch, "--base", base, "--title",
                title, "--body", body,
            ],
            repo,
        );
        self.run_checked(&self.gh_binary, &create, None)?;

        // `gh pr create` prints only the PR's URL; read the PR back by
        // its head branch for a typed number/url rather than scraping
        // that string.
        let view = ShellVcs::gh_args(
            &["pr", "view", branch, "--json", PULL_REQUEST_FIELDS],
            repo,
        );
        let pr: PullRequestJson =
            self.run_json(&self.gh_binary, &view, None)?;

        Ok(PullRequestRef {
            number: pr.number,
            url: pr.url,
            branch: pr.head_ref_name,
        })
    }

    fn enqueue_merge(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> Result<(), VcsError> {
        let number = pr_number.to_string();
        let args = ShellVcs::gh_args(
            &["pr", "merge", &number, "--auto", "--squash"],
            repo,
        );

        self.run_checked(&self.gh_binary, &args, None)?;
        Ok(())
    }

    fn read_relationships(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> Result<IssueRelationships, VcsError> {
        let (owner, name) = ShellVcs::split_repo(repo);
        let query_var = format!("query={RELATIONSHIPS_QUERY}");
        let owner_var = format!("owner={owner}");
        let name_var = format!("name={name}");
        // `-F` so `number` arrives as the GraphQL `Int!` the query
        // declares, rather than as a string.
        let number_var = format!("number={issue_number}");
        let args = [
            "api",
            "graphql",
            "-f",
            &query_var,
            "-f",
            &owner_var,
            "-f",
            &name_var,
            "-F",
            &number_var,
        ];

        let output = self.run(&self.gh_binary, &args, None)?;
        if !output.is_success() {
            // §8.4: orgs with native issue dependencies switched off
            // reject the query, so fall back to the `Depends on #N`
            // convention in the issue body.
            warn!(
                repo = repo,
                issue = issue_number,
                stderr = output.stderr.trim(),
                "native relationship query failed; falling back to \
                 `Depends on #N` in the issue body"
            );
            let issue = self.read_issue(repo, issue_number)?;
            return Ok(IssueRelationships {
                blocked_by: ShellVcs::parse_depends_on(&issue.body),
                ..IssueRelationships::default()
            });
        }

        let response: GraphQlResponse<RelationshipsData> =
            serde_json::from_str(&output.stdout).map_err(|source| {
                VcsError::InvalidJson {
                    command: command_line(&self.gh_binary, &args),
                    source,
                }
            })?;

        let Some(issue) =
            response.data.repository.and_then(|repository| repository.issue)
        else {
            warn!(
                repo = repo,
                issue = issue_number,
                "relationship query resolved no such issue"
            );
            return Ok(IssueRelationships::default());
        };

        Ok(IssueRelationships {
            blocked_by: issue.blocked_by.numbers(),
            blocking: issue.blocking.numbers(),
            parent: issue.parent.map(|parent| parent.number),
            sub_issues: issue.sub_issues.numbers(),
        })
    }

    fn find_linked_pull_requests(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> Result<Vec<PullRequestRef>, VcsError> {
        let (owner, name) = ShellVcs::split_repo(repo);
        let query_var = format!("query={LINKED_PULL_REQUESTS_QUERY}");
        let owner_var = format!("owner={owner}");
        let name_var = format!("name={name}");
        let number_var = format!("number={issue_number}");
        let args = [
            "api",
            "graphql",
            "-f",
            &query_var,
            "-f",
            &owner_var,
            "-f",
            &name_var,
            "-F",
            &number_var,
        ];

        let response: GraphQlResponse<LinkedPullRequestsData> =
            self.run_json(&self.gh_binary, &args, None)?;

        let Some(issue) =
            response.data.repository.and_then(|repository| repository.issue)
        else {
            warn!(
                repo = repo,
                issue = issue_number,
                "linked-pull-request query resolved no such issue"
            );
            return Ok(Vec::new());
        };

        Ok(issue
            .closed_by_pull_requests_references
            .nodes
            .into_iter()
            .map(|node| PullRequestRef {
                number: node.number,
                url: node.url,
                branch: node.head_ref_name,
            })
            .collect())
    }

    fn read_pull_request_status(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> Result<PullRequestStatus, VcsError> {
        let (owner, name) = ShellVcs::split_repo(repo);
        let query_var = format!("query={PULL_REQUEST_STATUS_QUERY}");
        let owner_var = format!("owner={owner}");
        let name_var = format!("name={name}");
        let number_var = format!("number={pr_number}");
        let args = [
            "api",
            "graphql",
            "-f",
            &query_var,
            "-f",
            &owner_var,
            "-f",
            &name_var,
            "-F",
            &number_var,
        ];

        let response: GraphQlResponse<PullRequestStatusData> =
            self.run_json(&self.gh_binary, &args, None)?;

        let Some(pr) = response
            .data
            .repository
            .and_then(|repository| repository.pull_request)
        else {
            return Err(VcsError::CommandFailed {
                command: command_line(&self.gh_binary, &args),
                status: 1,
                stderr: format!("no such pull request {repo}#{pr_number}"),
            });
        };

        let state = match pr.state {
            PullRequestStateJson::Open => PullRequestState::Open,
            PullRequestStateJson::Closed => PullRequestState::Closed,
            PullRequestStateJson::Merged => PullRequestState::Merged,
        };

        // No commits, or a commit with no reported checks yet, both read
        // as `Pending` — a brand-new PR must never default to `Success`.
        let ci = pr
            .commits
            .nodes
            .first()
            .and_then(|node| node.commit.status_check_rollup.as_ref())
            .map(|rollup| match rollup.state {
                StatusStateJson::Expected | StatusStateJson::Pending => {
                    CiConclusion::Pending
                }
                StatusStateJson::Error | StatusStateJson::Failure => {
                    CiConclusion::Failure
                }
                StatusStateJson::Success => CiConclusion::Success,
            })
            .unwrap_or(CiConclusion::Pending);

        Ok(PullRequestStatus {
            number: pr_number,
            state,
            ci,
            in_merge_queue: pr.merge_queue_entry.is_some(),
        })
    }
}

/// One `gh repo view --json nameWithOwner` payload.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoSlugJson {
    name_with_owner: String,
}

/// One `gh issue view --json number,title,body,labels,state` payload.
#[derive(Debug, Deserialize)]
struct IssueJson {
    number: u64,
    title: String,
    body: String,
    labels: Vec<LabelJson>,
    state: IssueStateJson,
}

/// One element of a `gh issue list --json number` payload.
///
/// Separate from [`IssueJson`] rather than a reuse of it with everything
/// `Option`: `gh issue list` returns exactly the fields `--json` asked
/// for, so the two payloads genuinely have different shapes, and making
/// [`IssueJson`]'s fields optional to share one type would let a
/// [`Vcs::read_issue`] response missing `title`/`state` deserialize
/// silently instead of failing.
#[derive(Debug, Deserialize)]
struct IssueNumberJson {
    number: u64,
}

/// One entry of `gh`'s `labels` array — both an issue's `labels` field
/// (`gh issue view --json labels`, used by [`Vcs::read_issue`]) and a
/// `gh label list --json name` result share this exact `{name}` shape,
/// so both reuse this one type rather than each declaring their own.
#[derive(Debug, Deserialize)]
struct LabelJson {
    name: String,
}

/// `gh`'s issue `state` field.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum IssueStateJson {
    Open,
    Closed,
}

/// One `gh pr view --json number,url,headRefName` payload.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestJson {
    number: u64,
    url: String,
    head_ref_name: String,
}

/// A `gh api graphql` envelope around one query's `data`.
#[derive(Debug, Deserialize)]
struct GraphQlResponse<T> {
    data: T,
}

/// [`MERGE_QUEUE_QUERY`]'s `data`.
#[derive(Debug, Deserialize)]
struct MergeQueueData {
    repository: Option<MergeQueueRepository>,
}

/// [`MERGE_QUEUE_QUERY`]'s `data.repository`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeQueueRepository {
    merge_queue: Option<MergeQueueNode>,
}

/// [`MERGE_QUEUE_QUERY`]'s `data.repository.mergeQueue`.
#[derive(Debug, Deserialize)]
struct MergeQueueNode {
    #[allow(dead_code)]
    id: String,
}

/// [`RELATIONSHIPS_QUERY`]'s `data`.
#[derive(Debug, Deserialize)]
struct RelationshipsData {
    repository: Option<RelationshipsRepository>,
}

/// [`RELATIONSHIPS_QUERY`]'s `data.repository`.
#[derive(Debug, Deserialize)]
struct RelationshipsRepository {
    issue: Option<RelationshipsIssue>,
}

/// [`RELATIONSHIPS_QUERY`]'s `data.repository.issue`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelationshipsIssue {
    parent: Option<IssueNumberNode>,
    sub_issues: IssueNumberConnection,
    blocked_by: IssueNumberConnection,
    blocking: IssueNumberConnection,
}

/// A GraphQL issue connection reduced to its issue numbers.
#[derive(Debug, Deserialize)]
struct IssueNumberConnection {
    nodes: Vec<IssueNumberNode>,
}

impl IssueNumberConnection {
    /// The connection's issue numbers, in the order GitHub returned
    /// them.
    fn numbers(self) -> Vec<u64> {
        self.nodes.into_iter().map(|node| node.number).collect()
    }
}

/// A GraphQL issue node reduced to its number.
#[derive(Debug, Deserialize)]
struct IssueNumberNode {
    number: u64,
}

/// [`LINKED_PULL_REQUESTS_QUERY`]'s `data`.
#[derive(Debug, Deserialize)]
struct LinkedPullRequestsData {
    repository: Option<LinkedPullRequestsRepository>,
}

/// [`LINKED_PULL_REQUESTS_QUERY`]'s `data.repository`.
#[derive(Debug, Deserialize)]
struct LinkedPullRequestsRepository {
    issue: Option<LinkedPullRequestsIssue>,
}

/// [`LINKED_PULL_REQUESTS_QUERY`]'s `data.repository.issue`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkedPullRequestsIssue {
    closed_by_pull_requests_references: PullRequestNodeConnection,
}

/// A GraphQL pull-request connection reduced to `number`/`url`/
/// `headRefName`.
#[derive(Debug, Deserialize)]
struct PullRequestNodeConnection {
    nodes: Vec<PullRequestNode>,
}

/// One node of [`PullRequestNodeConnection`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestNode {
    number: u64,
    url: String,
    head_ref_name: String,
}

/// [`PULL_REQUEST_STATUS_QUERY`]'s `data`.
#[derive(Debug, Deserialize)]
struct PullRequestStatusData {
    repository: Option<PullRequestStatusRepository>,
}

/// [`PULL_REQUEST_STATUS_QUERY`]'s `data.repository`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestStatusRepository {
    pull_request: Option<PullRequestStatusNode>,
}

/// [`PULL_REQUEST_STATUS_QUERY`]'s `data.repository.pullRequest`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestStatusNode {
    state: PullRequestStateJson,
    merge_queue_entry: Option<MergeQueueEntryJson>,
    commits: PullRequestCommitsConnection,
}

/// `PullRequest.state`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum PullRequestStateJson {
    Open,
    Closed,
    Merged,
}

/// `PullRequest.mergeQueueEntry` — only its presence matters here; the
/// entry's own `state` (queued/awaiting-checks/mergeable/...) is beyond
/// this step's scope (§14 step 4 covers signals, not scheduling).
#[derive(Debug, Deserialize)]
struct MergeQueueEntryJson {
    #[allow(dead_code)]
    state: String,
}

/// `PullRequest.commits(last: 1)`.
#[derive(Debug, Deserialize)]
struct PullRequestCommitsConnection {
    nodes: Vec<PullRequestCommitNode>,
}

/// One node of [`PullRequestCommitsConnection`].
#[derive(Debug, Deserialize)]
struct PullRequestCommitNode {
    commit: CommitStatusNode,
}

/// `Commit.statusCheckRollup` — null when the commit has no reported
/// checks yet.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitStatusNode {
    status_check_rollup: Option<StatusCheckRollupJson>,
}

/// `StatusCheckRollup.state` — GitHub's own aggregation of every check
/// run/status context on the commit into one `StatusState`.
#[derive(Debug, Deserialize)]
struct StatusCheckRollupJson {
    state: StatusStateJson,
}

/// The `StatusState` enum.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum StatusStateJson {
    Expected,
    Error,
    Failure,
    Pending,
    Success,
}

#[cfg(test)]
mod tests {
    use super::runner::{FakeRunner, RecordedCall};
    use super::*;

    /// A [`ShellVcs`] over `runner`, with the conventional binary names.
    fn shell_vcs(runner: Arc<FakeRunner>) -> ShellVcs {
        ShellVcs::with_runner("git".to_string(), "gh".to_string(), runner)
    }

    /// Assert `call` invoked `program` with exactly `args`.
    fn assert_call(call: &RecordedCall, program: &str, args: &[&str]) {
        assert_eq!(call.program, program);
        assert_eq!(call.args, args);
    }

    #[test]
    fn gh_args_always_appends_the_repo_scope() {
        assert_eq!(
            ShellVcs::gh_args(&["issue", "view", "42"], "owner/repo-a"),
            vec!["issue", "view", "42", "--repo", "owner/repo-a"],
        );
    }

    #[test]
    fn git_available_fails_for_a_binary_that_does_not_exist() {
        let vcs = ShellVcs::new(
            "definitely-not-a-real-binary".to_string(),
            "gh".to_string(),
        );

        assert!(matches!(
            vcs.git_available(),
            Err(VcsError::BinaryNotFound { .. })
        ));
    }

    #[test]
    fn gh_authenticated_checks_auth_then_reaches_the_repo_itself() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("Logged in to github.com");
        runner.succeed(r#"{"nameWithOwner":"owner/repo-a"}"#);
        let vcs = shell_vcs(runner.clone());

        let authenticated = vcs.gh_authenticated("owner/repo-a").unwrap();

        assert!(authenticated);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_call(&calls[0], "gh", &["auth", "status"]);
        assert_call(
            &calls[1],
            "gh",
            &["repo", "view", "owner/repo-a", "--json", "nameWithOwner"],
        );
    }

    #[test]
    fn gh_authenticated_is_false_when_auth_status_fails() {
        let runner = Arc::new(FakeRunner::new());
        runner.fail(1, "You are not logged into any GitHub hosts.");
        let vcs = shell_vcs(runner.clone());

        assert!(!vcs.gh_authenticated("owner/repo-a").unwrap());
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn gh_authenticated_is_false_when_the_repo_is_unreachable() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("Logged in to github.com");
        runner.fail(1, "Could not resolve to a Repository");
        let vcs = shell_vcs(runner.clone());

        assert!(!vcs.gh_authenticated("owner/repo-a").unwrap());
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn detect_repo_slug_reads_name_with_owner_from_inside_the_repo() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(r#"{"nameWithOwner":"owner/repo-a"}"#);
        let vcs = shell_vcs(runner.clone());

        let slug =
            vcs.detect_repo_slug(Path::new("/abs/repo-a/main")).unwrap();

        assert_eq!(slug, "owner/repo-a");
        let calls = runner.calls();
        assert_call(
            &calls[0],
            "gh",
            &["repo", "view", "--json", "nameWithOwner"],
        );
        assert_eq!(
            calls[0].cwd.as_deref(),
            Some(Path::new("/abs/repo-a/main"))
        );
    }

    #[test]
    fn detect_repo_slug_reports_a_non_zero_exit_as_command_failed() {
        let runner = Arc::new(FakeRunner::new());
        runner.fail(1, "not a git repository");
        let vcs = shell_vcs(runner);

        assert!(matches!(
            vcs.detect_repo_slug(Path::new("/abs/nowhere")),
            Err(VcsError::CommandFailed { status: 1, .. })
        ));
    }

    #[test]
    fn detect_repo_slug_reports_malformed_json_as_invalid_json() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("not json at all");
        let vcs = shell_vcs(runner);

        assert!(matches!(
            vcs.detect_repo_slug(Path::new("/abs/repo-a/main")),
            Err(VcsError::InvalidJson { .. })
        ));
    }

    #[test]
    fn merge_queue_enabled_is_true_when_the_branch_has_a_queue() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"data":{"repository":{"mergeQueue":{"id":"MQ_1"}}}}"#,
        );
        let vcs = shell_vcs(runner.clone());

        let enabled = vcs.merge_queue_enabled("owner/repo-a", "main").unwrap();

        assert!(enabled);
        let calls = runner.calls();
        assert_eq!(calls[0].program, "gh");
        assert_eq!(&calls[0].args[..2], &["api", "graphql"]);
        assert!(calls[0].args.contains(&"owner=owner".to_string()));
        assert!(calls[0].args.contains(&"name=repo-a".to_string()));
        assert!(calls[0].args.contains(&"branch=main".to_string()));
    }

    #[test]
    fn merge_queue_enabled_is_false_when_the_branch_has_none() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(r#"{"data":{"repository":{"mergeQueue":null}}}"#);
        let vcs = shell_vcs(runner);

        assert!(!vcs.merge_queue_enabled("owner/repo-a", "main").unwrap());
    }

    #[test]
    fn merge_queue_enabled_degrades_to_false_when_the_query_fails() {
        let runner = Arc::new(FakeRunner::new());
        runner.fail(1, "Field 'mergeQueue' doesn't exist");
        let vcs = shell_vcs(runner);

        assert!(!vcs.merge_queue_enabled("owner/repo-a", "main").unwrap());
    }

    #[test]
    fn merge_queue_enabled_degrades_to_false_on_unparseable_output() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("<html>proxy error</html>");
        let vcs = shell_vcs(runner);

        assert!(!vcs.merge_queue_enabled("owner/repo-a", "main").unwrap());
    }

    #[test]
    fn create_issue_creates_then_reads_the_issue_back_by_its_new_number() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("https://github.com/owner/repo-a/issues/43\n");
        runner.succeed(
            r#"{"number":43,"title":"Widget cache","body":"Do it.",
                "labels":[],"state":"OPEN"}"#,
        );
        let vcs = shell_vcs(runner.clone());

        let issue = vcs
            .create_issue("owner/repo-a", "Widget cache", "Do it.")
            .unwrap();

        assert_eq!(issue.number, 43);
        assert_eq!(issue.title, "Widget cache");
        let calls = runner.calls();
        assert_call(
            &calls[0],
            "gh",
            &[
                "issue",
                "create",
                "--title",
                "Widget cache",
                "--body",
                "Do it.",
                "--repo",
                "owner/repo-a",
            ],
        );
        assert_call(
            &calls[1],
            "gh",
            &[
                "issue",
                "view",
                "43",
                "--json",
                ISSUE_FIELDS,
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn create_issue_fails_loudly_when_the_create_output_is_not_an_issue_url() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("not a url\n");
        let vcs = shell_vcs(runner.clone());

        assert!(matches!(
            vcs.create_issue("owner/repo-a", "t", "b"),
            Err(VcsError::UnexpectedOutput { .. })
        ));
        // No read-back is attempted after a bad URL -- there is no
        // number to view by.
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn create_issue_propagates_a_read_back_failure_after_a_successful_create()
    {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("https://github.com/owner/repo-a/issues/43\n");
        runner.fail(1, "not found");
        let vcs = shell_vcs(runner);

        assert!(matches!(
            vcs.create_issue("owner/repo-a", "t", "b"),
            Err(VcsError::CommandFailed { .. })
        ));
    }

    #[test]
    fn read_issue_parses_the_issue_and_scopes_the_call_to_the_repo() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"number":42,"title":"Widget cache","body":"Do it.",
                "labels":[{"name":"status:ready"},{"name":"P1"}],
                "state":"OPEN"}"#,
        );
        let vcs = shell_vcs(runner.clone());

        let issue = vcs.read_issue("owner/repo-a", 42).unwrap();

        assert_eq!(issue.number, 42);
        assert_eq!(issue.title, "Widget cache");
        assert_eq!(issue.body, "Do it.");
        assert_eq!(issue.labels, vec!["status:ready", "P1"]);
        assert_eq!(issue.state, IssueState::Open);
        assert_call(
            &runner.calls()[0],
            "gh",
            &[
                "issue",
                "view",
                "42",
                "--json",
                ISSUE_FIELDS,
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn read_issue_maps_a_closed_issue() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"number":7,"title":"t","body":"","labels":[],
                "state":"CLOSED"}"#,
        );
        let vcs = shell_vcs(runner);

        let issue = vcs.read_issue("owner/repo-a", 7).unwrap();

        assert_eq!(issue.state, IssueState::Closed);
    }

    #[test]
    fn read_issue_reports_an_unknown_state_as_invalid_json() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"number":7,"title":"t","body":"","labels":[],
                "state":"MERGED"}"#,
        );
        let vcs = shell_vcs(runner);

        assert!(matches!(
            vcs.read_issue("owner/repo-a", 7),
            Err(VcsError::InvalidJson { .. })
        ));
    }

    #[test]
    fn list_open_issues_parses_the_numbers_and_pins_every_filter_flag() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(r#"[{"number":42},{"number":7},{"number":3}]"#);
        let vcs = shell_vcs(runner.clone());

        let numbers = vcs.list_open_issues("owner/repo-a", 250).unwrap();

        assert_eq!(numbers, vec![42, 7, 3]);
        // `--state`/`--limit` are asserted here, not left to `gh`'s own
        // defaults, precisely because those defaults (`open`, `30`) would
        // make a version of this method that omitted both flags pass an
        // output-only test while silently capping a large backlog at 30.
        assert_call(
            &runner.calls()[0],
            "gh",
            &[
                "issue",
                "list",
                "--state",
                "open",
                "--limit",
                "250",
                "--json",
                ISSUE_LIST_FIELDS,
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn list_open_issues_returns_nothing_for_a_repo_with_no_open_issues() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("[]");
        let vcs = shell_vcs(runner);

        assert!(vcs.list_open_issues("owner/repo-a", 30).unwrap().is_empty());
    }

    #[test]
    fn list_open_issues_returns_nothing_when_gh_prints_truly_empty_stdout() {
        // `gh issue list --json ...` (confirmed live against `gh` 2.83.0)
        // prints nothing at all on a zero-result list -- not `[]` -- while
        // still exiting 0. A parser that only accepts `[]` as "empty"
        // would misread this success as a broken `VcsError::InvalidJson`.
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("");
        let vcs = shell_vcs(runner);

        assert!(vcs.list_open_issues("owner/repo-a", 30).unwrap().is_empty());
    }

    #[test]
    fn list_open_issues_reports_a_failed_gh_call_rather_than_an_empty_board() {
        // An unreachable/unauthenticated `gh` must never look like "this
        // repo has no issues" -- a silently empty board would read as a
        // finished queue (§2.7) rather than a broken one.
        let runner = Arc::new(FakeRunner::new());
        runner.fail(1, "could not resolve to a Repository");
        let vcs = shell_vcs(runner);

        assert!(matches!(
            vcs.list_open_issues("owner/repo-a", 30),
            Err(VcsError::CommandFailed { .. })
        ));
    }

    #[test]
    fn set_label_adds_a_label_when_present() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("");
        let vcs = shell_vcs(runner.clone());

        vcs.set_label("owner/repo-a", 42, "status:ready", true).unwrap();

        assert_call(
            &runner.calls()[0],
            "gh",
            &[
                "issue",
                "edit",
                "42",
                "--add-label",
                "status:ready",
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn set_label_removes_a_label_when_not_present() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("");
        let vcs = shell_vcs(runner.clone());

        vcs.set_label("owner/repo-a", 42, "status:ready", false).unwrap();

        assert_call(
            &runner.calls()[0],
            "gh",
            &[
                "issue",
                "edit",
                "42",
                "--remove-label",
                "status:ready",
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn ensure_label_creates_a_label_that_does_not_exist_yet() {
        let runner = Arc::new(FakeRunner::new());
        // First call: the existence check -- an empty search result, the
        // label isn't there.
        runner.succeed("[]");
        // Second call: the create.
        runner.succeed("");
        let vcs = shell_vcs(runner.clone());

        vcs.ensure_label("owner/repo-a", "gate:architecture").unwrap();

        let calls = runner.calls();
        assert_call(
            &calls[0],
            "gh",
            &[
                "label",
                "list",
                "--search",
                "gate:architecture",
                "--json",
                "name",
                "--limit",
                "999",
                "--repo",
                "owner/repo-a",
            ],
        );
        assert_call(
            &calls[1],
            "gh",
            &[
                "label",
                "create",
                "gate:architecture",
                "--color",
                "ededed",
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn ensure_label_creates_a_label_when_search_returns_empty_stdout() {
        // `gh label list --search <term> --json name` (confirmed live
        // against `gh` 2.83.0) prints nothing at all -- not `[]` -- when
        // the search matches zero labels, despite exiting 0. This is the
        // guaranteed case on a repo's first-ever `ensure_label` call for
        // any label, since none of the vocabulary exists yet.
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("");
        runner.succeed("");
        let vcs = shell_vcs(runner.clone());

        vcs.ensure_label("owner/repo-a", "gate:architecture").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_call(
            &calls[1],
            "gh",
            &[
                "label",
                "create",
                "gate:architecture",
                "--color",
                "ededed",
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn ensure_label_leaves_an_already_existing_label_untouched() {
        // The existence check finds it already there -- must not call
        // `label create` at all, since doing so (even with `--force`)
        // would repaint whatever color a team's own label already had.
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(r#"[{"name":"gate:architecture"}]"#);
        let vcs = shell_vcs(runner.clone());

        vcs.ensure_label("owner/repo-a", "gate:architecture").unwrap();

        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn ensure_label_ignores_a_fuzzy_search_match_that_is_not_an_exact_name() {
        // `--search` is a substring match -- a result set containing a
        // DIFFERENT label that merely contains the search term must not
        // be mistaken for the exact label already existing.
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(r#"[{"name":"gate:architecture-legacy"}]"#);
        runner.succeed("");
        let vcs = shell_vcs(runner.clone());

        vcs.ensure_label("owner/repo-a", "gate:architecture").unwrap();

        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn post_comment_sends_the_body_scoped_to_the_repo() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("https://github.com/owner/repo-a/issues/42#comment");
        let vcs = shell_vcs(runner.clone());

        vcs.post_comment("owner/repo-a", 42, "phase done").unwrap();

        assert_call(
            &runner.calls()[0],
            "gh",
            &[
                "issue",
                "comment",
                "42",
                "--body",
                "phase done",
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn post_comment_reports_a_non_zero_exit_as_command_failed() {
        let runner = Arc::new(FakeRunner::new());
        runner.fail(1, "HTTP 403");
        let vcs = shell_vcs(runner);

        assert!(matches!(
            vcs.post_comment("owner/repo-a", 42, "phase done"),
            Err(VcsError::CommandFailed { status: 1, .. })
        ));
    }

    #[test]
    fn open_pr_creates_then_reads_the_pull_request_back() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("https://github.com/owner/repo-a/pull/7\n");
        runner.succeed(
            r#"{"number":7,"url":"https://github.com/owner/repo-a/pull/7",
                "headRefName":"issue-42-widget-cache"}"#,
        );
        let vcs = shell_vcs(runner.clone());

        let pr = vcs
            .open_pr(
                "owner/repo-a",
                "issue-42-widget-cache",
                "main",
                "Widget cache",
                "Closes #42",
            )
            .unwrap();

        assert_eq!(pr.number, 7);
        assert_eq!(pr.url, "https://github.com/owner/repo-a/pull/7");
        assert_eq!(pr.branch, "issue-42-widget-cache");
        let calls = runner.calls();
        assert_call(
            &calls[0],
            "gh",
            &[
                "pr",
                "create",
                "--head",
                "issue-42-widget-cache",
                "--base",
                "main",
                "--title",
                "Widget cache",
                "--body",
                "Closes #42",
                "--repo",
                "owner/repo-a",
            ],
        );
        assert_call(
            &calls[1],
            "gh",
            &[
                "pr",
                "view",
                "issue-42-widget-cache",
                "--json",
                PULL_REQUEST_FIELDS,
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn open_pr_reports_a_failed_create_as_command_failed() {
        let runner = Arc::new(FakeRunner::new());
        runner.fail(1, "a pull request already exists");
        let vcs = shell_vcs(runner.clone());

        let error = vcs
            .open_pr("owner/repo-a", "issue-42-x", "main", "t", "b")
            .unwrap_err();

        assert!(matches!(error, VcsError::CommandFailed { status: 1, .. }));
        // The read-back must not run once the create failed.
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn enqueue_merge_uses_the_auto_merge_queue_scoped_to_the_repo() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed("");
        let vcs = shell_vcs(runner.clone());

        vcs.enqueue_merge("owner/repo-a", 7).unwrap();

        assert_call(
            &runner.calls()[0],
            "gh",
            &[
                "pr",
                "merge",
                "7",
                "--auto",
                "--squash",
                "--repo",
                "owner/repo-a",
            ],
        );
    }

    #[test]
    fn read_relationships_parses_the_native_graphql_edges() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"data":{"repository":{"issue":{
                "parent":{"number":10},
                "subIssues":{"nodes":[{"number":43},{"number":44}]},
                "blockedBy":{"nodes":[{"number":41}]},
                "blocking":{"nodes":[{"number":50}]}}}}}"#,
        );
        let vcs = shell_vcs(runner.clone());

        let relationships =
            vcs.read_relationships("owner/repo-a", 42).unwrap();

        assert_eq!(relationships.parent, Some(10));
        assert_eq!(relationships.sub_issues, vec![43, 44]);
        assert_eq!(relationships.blocked_by, vec![41]);
        assert_eq!(relationships.blocking, vec![50]);
        let calls = runner.calls();
        assert_eq!(&calls[0].args[..2], &["api", "graphql"]);
        assert!(calls[0].args.contains(&"number=42".to_string()));
    }

    #[test]
    fn read_relationships_falls_back_to_depends_on_in_the_body() {
        let runner = Arc::new(FakeRunner::new());
        runner.fail(1, "Field 'blockedBy' doesn't exist on type 'Issue'");
        runner.succeed(
            r#"{"number":42,"title":"t",
                "body":"Depends on #41 and #40.\nSee also #99.",
                "labels":[],"state":"OPEN"}"#,
        );
        let vcs = shell_vcs(runner.clone());

        let relationships =
            vcs.read_relationships("owner/repo-a", 42).unwrap();

        assert_eq!(relationships.blocked_by, vec![41, 40]);
        assert!(relationships.blocking.is_empty());
        assert_eq!(relationships.parent, None);
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn read_relationships_is_empty_when_the_issue_is_missing() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(r#"{"data":{"repository":{"issue":null}}}"#);
        let vcs = shell_vcs(runner);

        let relationships =
            vcs.read_relationships("owner/repo-a", 42).unwrap();

        assert_eq!(relationships, IssueRelationships::default());
    }

    #[test]
    fn find_linked_pull_requests_parses_the_native_graphql_nodes() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"data":{"repository":{"issue":{
                "closedByPullRequestsReferences":{"nodes":[
                    {"number":22,
                     "url":"https://github.com/owner/repo-a/pull/22",
                     "headRefName":"issue-19-widget-cache"}
                ]}}}}}"#,
        );
        let vcs = shell_vcs(runner.clone());

        let prs = vcs.find_linked_pull_requests("owner/repo-a", 19).unwrap();

        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 22);
        assert_eq!(prs[0].url, "https://github.com/owner/repo-a/pull/22");
        assert_eq!(prs[0].branch, "issue-19-widget-cache");
        let calls = runner.calls();
        assert_eq!(&calls[0].args[..2], &["api", "graphql"]);
        assert!(calls[0].args.contains(&"number=19".to_string()));
    }

    #[test]
    fn find_linked_pull_requests_is_empty_when_none_are_linked() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"data":{"repository":{"issue":{
                "closedByPullRequestsReferences":{"nodes":[]}}}}}"#,
        );
        let vcs = shell_vcs(runner);

        let prs = vcs.find_linked_pull_requests("owner/repo-a", 19).unwrap();

        assert!(prs.is_empty());
    }

    #[test]
    fn find_linked_pull_requests_is_empty_when_the_issue_is_missing() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(r#"{"data":{"repository":{"issue":null}}}"#);
        let vcs = shell_vcs(runner);

        let prs = vcs.find_linked_pull_requests("owner/repo-a", 19).unwrap();

        assert!(prs.is_empty());
    }

    #[test]
    fn read_pull_request_status_parses_state_ci_and_merge_queue() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"data":{"repository":{"pullRequest":{
                "state":"OPEN",
                "mergeQueueEntry":{"state":"QUEUED"},
                "commits":{"nodes":[{"commit":{
                    "statusCheckRollup":{"state":"SUCCESS"}}}]}}}}}"#,
        );
        let vcs = shell_vcs(runner.clone());

        let status = vcs.read_pull_request_status("owner/repo-a", 7).unwrap();

        assert_eq!(status.number, 7);
        assert_eq!(status.state, PullRequestState::Open);
        assert_eq!(status.ci, CiConclusion::Success);
        assert!(status.in_merge_queue);
        let calls = runner.calls();
        assert_eq!(&calls[0].args[..2], &["api", "graphql"]);
        assert!(calls[0].args.contains(&"number=7".to_string()));
    }

    #[test]
    fn read_pull_request_status_maps_a_failing_check_to_failure() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"data":{"repository":{"pullRequest":{
                "state":"OPEN",
                "mergeQueueEntry":null,
                "commits":{"nodes":[{"commit":{
                    "statusCheckRollup":{"state":"FAILURE"}}}]}}}}}"#,
        );
        let vcs = shell_vcs(runner);

        let status = vcs.read_pull_request_status("owner/repo-a", 7).unwrap();

        assert_eq!(status.ci, CiConclusion::Failure);
        assert!(!status.in_merge_queue);
    }

    #[test]
    fn read_pull_request_status_maps_a_merged_pr_with_no_checks_to_pending() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(
            r#"{"data":{"repository":{"pullRequest":{
                "state":"MERGED",
                "mergeQueueEntry":null,
                "commits":{"nodes":[{"commit":{
                    "statusCheckRollup":null}}]}}}}}"#,
        );
        let vcs = shell_vcs(runner);

        let status = vcs.read_pull_request_status("owner/repo-a", 7).unwrap();

        assert_eq!(status.state, PullRequestState::Merged);
        assert_eq!(status.ci, CiConclusion::Pending);
    }

    #[test]
    fn read_pull_request_status_reports_a_missing_pr_as_command_failed() {
        let runner = Arc::new(FakeRunner::new());
        runner.succeed(r#"{"data":{"repository":{"pullRequest":null}}}"#);
        let vcs = shell_vcs(runner);

        assert!(matches!(
            vcs.read_pull_request_status("owner/repo-a", 7),
            Err(VcsError::CommandFailed { .. })
        ));
    }

    #[test]
    fn branch_slug_strips_the_issue_prefix() {
        assert_eq!(branch_slug("issue-42-widget-cache", 42), "widget-cache");
        assert_eq!(branch_slug("hotfix", 42), "hotfix");
    }

    #[test]
    fn parse_depends_on_ignores_unrelated_issue_references() {
        assert_eq!(
            ShellVcs::parse_depends_on("Depends on #41, #40\nCloses #99"),
            vec![41, 40]
        );
        assert!(ShellVcs::parse_depends_on("Closes #99").is_empty());
    }
}
