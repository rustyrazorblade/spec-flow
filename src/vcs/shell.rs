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
    IssueRef, IssueRelationships, IssueState, PullRequestRef, Vcs, VcsError,
    Worktree, branch_slug,
};

/// The `gh issue view` fields [`Vcs::read_issue`] requests, in the order
/// [`IssueJson`] declares them.
const ISSUE_FIELDS: &str = "number,title,body,labels,state";

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

/// One entry of `gh`'s `labels` array.
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
