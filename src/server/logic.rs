//! The MCP tools' bodies, as ordinary synchronous functions over the
//! [`Vcs`] seam — everything about §6's `register`/`board`/`issue` that
//! can be decided or fetched without an async runtime or an HTTP
//! connection in scope.
//!
//! This is the split §5 asks for ("everything but the `git`/`gh` layer
//! and the process spawner is unit-testable by stubbing those seams")
//! applied to the server: the functions here take `&impl Vcs`, so every
//! one of them is exercised against [`crate::vcs::FakeVcs`] below, while
//! [`super`]'s `rmcp` handler is left as thin, near-untestable glue —
//! argument decoding, `spawn_blocking`, error mapping — covered instead
//! by `tests/mcp_server.rs`'s end-to-end run against a real client.

use std::path::Path;

use crate::board::{BoardRow, build_board, issue_url};
use crate::config::{
    ConfigError, GlobalConfig, ProjectConfig, ProjectPointer,
    load_project_config, project_config_path,
};
use crate::registry::find_project_containing;
use crate::state::{IssueSnapshot, read_issue_state};
use crate::vcs::{Vcs, VcsError};

/// Errors an MCP tool call can fail with (§6).
///
/// Every variant maps to a distinct JSON-RPC error at the boundary (see
/// [`super::SpecFlowServer`]); the split is by *whose* mistake it is —
/// a bad argument, a connection used out of order, or the environment
/// (config/`gh`) failing underneath.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    /// A coordinator called `register` without a `cwd` (§6: it "passes
    /// its `cwd` (the directory Claude was opened in — HTTP/SSE doesn't
    /// convey it implicitly, so the client sends it)").
    #[error(
        "register requires cwd for a coordinator connection (no \
         spawn_token was supplied); HTTP/SSE does not convey the \
         client's working directory, so it must be sent explicitly"
    )]
    CwdRequired,

    /// A `cwd` that is not an absolute path. Rejected rather than
    /// resolved: the daemon's own working directory is unrelated to the
    /// client's (§2.5 — one daemon, many projects, never "in" a
    /// checkout), so there is nothing correct to resolve against.
    #[error("register requires an absolute cwd; got {cwd}")]
    CwdNotAbsolute {
        /// The relative path the client sent.
        cwd: String,
    },

    /// `cwd` is not inside any registered project (§6: "a directory that
    /// isn't a registered project is rejected → run `fleet init`
    /// there").
    #[error(
        "{cwd} is not inside any project registered with this daemon; \
         run `spec-flow init` in that repo first"
    )]
    UnregisteredDirectory {
        /// The unregistered directory the client sent.
        cwd: String,
    },

    /// A `spawn_token` was presented. §6's spawned-phase registration
    /// path is not built in this slice; rejecting is the only honest
    /// answer, since silently treating a spawned phase as a coordinator
    /// would bind it to a project without the `(issue, phase, sub_id)`
    /// tuple its later `submit_artifact` must be checked against (§2.6).
    #[error(
        "spawn_token registration (the spawned-phase path, §2.6) is not \
         implemented yet; only coordinator registration with a cwd is \
         supported"
    )]
    SpawnTokenUnsupported,

    /// An issue-scoped tool was called before `register` bound the
    /// connection to a project (§15: "project rides the connection").
    #[error("this connection has not registered; call register first")]
    NotRegistered,

    /// A second `register` naming a different project (§15: "a
    /// connection is bound to exactly one project for its life ... and
    /// never changed").
    #[error(
        "this connection is already bound to project {bound}; a \
         connection can never act on another project ({requested}) — \
         open a new connection from that project's directory"
    )]
    AlreadyBound {
        /// The project the connection was bound to at `register`.
        bound: String,
        /// The project the rejected re-registration named.
        requested: String,
    },

    /// `board(filter?)` was called with a filter. See
    /// [`super::wire::BoardArgs::filter`].
    #[error(
        "board(filter) is not implemented yet; call board with no \
         filter to get every open issue"
    )]
    BoardFilterUnsupported,

    /// The project's own `<project_dir>/config.yaml` could not be read.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// A `git`/`gh` call failed.
    #[error(transparent)]
    Vcs(#[from] VcsError),

    /// A `spawn_blocking` worker task panicked (see `super::blocking`'s doc).
    /// Distinct from [`ToolError::Vcs`] on purpose: this is a bug in this
    /// crate, not an environment failure, and `tool_error`'s
    /// "fix your call vs. fix your machine" split needs to tell the two
    /// apart rather than reporting a panic as "failed to spawn git/gh".
    #[error("an internal worker task failed: {0}")]
    Internal(String),
}

/// Resolve a coordinator's `cwd` to the registered project containing it
/// and load that project's settings (§2.5, §6).
///
/// Two steps, both required: [`find_project_containing`] answers *which*
/// registered project owns the directory (the global config holds only
/// pointers, §11.1), and [`load_project_config`] reads that project's own
/// file for the repo slug every later `gh` call is scoped with (§8.5).
pub fn resolve_project(
    global: &GlobalConfig,
    cwd: &str,
) -> Result<(ProjectPointer, ProjectConfig), ToolError> {
    let path = Path::new(cwd);
    if !path.is_absolute() {
        return Err(ToolError::CwdNotAbsolute { cwd: cwd.to_string() });
    }

    let pointer = find_project_containing(global, path).ok_or_else(|| {
        ToolError::UnregisteredDirectory { cwd: cwd.to_string() }
    })?;

    let config =
        load_project_config(&project_config_path(&pointer.project_dir))?;

    Ok((pointer.clone(), config))
}

/// The board (§13) for one project: every open issue, each read through
/// the GitHub-state layer, rendered by [`build_board`].
///
/// Returns the rows plus whether the repo hit `limit` — see
/// [`super::wire::BoardResult::truncated`] for why silence would be the
/// wrong failure mode there.
///
/// # Cost — MEASURED, and a real problem at scale
///
/// This is N+1 and serial: one [`Vcs::list_open_issues`] call, then
/// three-to-four more `gh` invocations per issue inside
/// [`read_issue_state`] (issue, relationships, linked PRs, and one PR
/// status per linked PR). Measured end to end through the MCP server
/// against a live repo with 16 open issues: **~20 seconds**, ~48 `gh`
/// subprocesses. That extrapolates to minutes at
/// [`super::BOARD_ISSUE_LIMIT`], which is well past what §2.7's
/// conversational coordinator loop can absorb.
///
/// It is written this way anyway, and *not* quietly capped or cached,
/// because both fixes are real design work rather than tuning: §2.3/§5's
/// "disposable, re-derived from GitHub" local cache does not exist in
/// this crate at all yet (it needs an invalidation story the CI/PR
/// poller, §12, is what would drive), and batching would mean one
/// hand-written GraphQL document returning issue + labels +
/// relationships + linked PRs + check rollups per page — a new `Vcs`
/// method whose shape has to be validated against the live schema the
/// way every other GraphQL call in [`crate::vcs`] already was. Returning
/// stale or truncated rows to hide the latency would be the one
/// genuinely wrong answer (§15's "surface, don't bury").
pub fn read_board<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    limit: u32,
) -> Result<(Vec<BoardRow>, bool), ToolError> {
    let numbers = vcs.list_open_issues(&project.repo, limit)?;
    let truncated = numbers.len() as u32 >= limit;

    let mut snapshots = Vec::with_capacity(numbers.len());
    for number in numbers {
        snapshots.push(read_issue_state(vcs, &project.repo, number)?);
    }

    let host = project.gh.as_ref().map(|gh| gh.host.as_str());
    Ok((build_board(&snapshots, &project.repo, host), truncated))
}

/// One issue's derived state (§6 `issue(number)`), plus the GitHub URL
/// §15 requires it to be rendered with.
pub fn read_issue<V: Vcs>(
    vcs: &V,
    project: &ProjectConfig,
    number: u64,
) -> Result<(IssueSnapshot, String), ToolError> {
    let snapshot = read_issue_state(vcs, &project.repo, number)?;
    let host = project.gh.as_ref().map(|gh| gh.host.as_str());
    Ok((snapshot, issue_url(&project.repo, host, number)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;
    use crate::config::{
        Binaries, ClaimConfig, CrossProjectMode, GhConfig, HarnessesConfig,
        Limits, MergeMode, save_project_config,
    };
    use crate::vcs::{FakeVcs, IssueState};

    fn global_config(projects: Vec<ProjectPointer>) -> GlobalConfig {
        GlobalConfig {
            listen: "127.0.0.1:7420".parse().unwrap(),
            instance_id: None,
            binaries: Binaries::default(),
            harnesses: HarnessesConfig {
                default: "claude".to_string(),
                harnesses: HashMap::new(),
            },
            limits: Limits { max_concurrent_agents: 3 },
            cross_project_mode: CrossProjectMode::FairShare,
            claim: ClaimConfig {
                heartbeat_ttl: Duration::from_secs(3600),
                heartbeat_interval: Duration::from_secs(300),
            },
            phase_timeout: Duration::from_secs(45 * 60),
            projects,
        }
    }

    fn project_config(name: &str, repo: &str, dir: &Path) -> ProjectConfig {
        ProjectConfig {
            name: name.to_string(),
            repo: repo.to_string(),
            project_dir: dir.to_path_buf(),
            local_path: dir.join("main"),
            default_branch: "main".to_string(),
            merge_mode: MergeMode::Native,
            gh: None,
            harness: None,
            index_path: dir.join(".index"),
            memory_scope: name.to_string(),
            weight: None,
        }
    }

    /// Write a real `<project_dir>/config.yaml` and return the pointer
    /// the global registry would hold for it.
    fn register_on_disk(dir: &Path, name: &str, repo: &str) -> ProjectPointer {
        let config = project_config(name, repo, dir);
        save_project_config(&project_config_path(dir), &config).unwrap();
        ProjectPointer {
            name: name.to_string(),
            project_dir: dir.to_path_buf(),
        }
    }

    // -- resolve_project --

    #[test]
    fn resolve_project_finds_the_project_the_cwd_sits_directly_in() {
        let tmp = tempfile::tempdir().unwrap();
        let pointer = register_on_disk(tmp.path(), "proj-a", "owner/repo-a");
        let global = global_config(vec![pointer]);

        let (found, config) =
            resolve_project(&global, tmp.path().to_str().unwrap()).unwrap();

        assert_eq!(found.name, "proj-a");
        assert_eq!(config.repo, "owner/repo-a");
    }

    #[test]
    fn resolve_project_finds_the_project_from_a_worktree_inside_it() {
        // The realistic case (§10.1): a coordinator is opened in
        // `<project_dir>/<branch>`, never in `<project_dir>` itself.
        let tmp = tempfile::tempdir().unwrap();
        let pointer = register_on_disk(tmp.path(), "proj-a", "owner/repo-a");
        let global = global_config(vec![pointer]);
        let worktree = tmp.path().join("issue-42-widget-cache");

        let (found, _) =
            resolve_project(&global, worktree.to_str().unwrap()).unwrap();

        assert_eq!(found.name, "proj-a");
    }

    #[test]
    fn resolve_project_picks_the_containing_project_among_several() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let global = global_config(vec![
            register_on_disk(&a, "proj-a", "owner/repo-a"),
            register_on_disk(&b, "proj-b", "owner/repo-b"),
        ]);

        let (found, config) =
            resolve_project(&global, b.to_str().unwrap()).unwrap();

        assert_eq!(found.name, "proj-b");
        assert_eq!(config.repo, "owner/repo-b");
    }

    #[test]
    fn resolve_project_rejects_a_directory_outside_every_project() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("a");
        std::fs::create_dir_all(&project).unwrap();
        let global =
            global_config(vec![register_on_disk(&project, "a", "o/a")]);
        let elsewhere = tmp.path().join("not-a-project");

        assert!(matches!(
            resolve_project(&global, elsewhere.to_str().unwrap()),
            Err(ToolError::UnregisteredDirectory { .. })
        ));
    }

    #[test]
    fn resolve_project_rejects_an_empty_registry() {
        let global = global_config(Vec::new());

        assert!(matches!(
            resolve_project(&global, "/anywhere"),
            Err(ToolError::UnregisteredDirectory { .. })
        ));
    }

    #[test]
    fn resolve_project_rejects_a_relative_cwd() {
        let global = global_config(Vec::new());

        assert!(matches!(
            resolve_project(&global, "some/relative/dir"),
            Err(ToolError::CwdNotAbsolute { .. })
        ));
    }

    #[test]
    fn resolve_project_reports_a_registered_project_with_no_config_file() {
        // The registry is a pointer list (§11.1); a pointer whose
        // `<project_dir>/config.yaml` was deleted must surface as a
        // config error, not as "this directory isn't registered" --
        // those two send the operator to completely different fixes.
        let tmp = tempfile::tempdir().unwrap();
        let global = global_config(vec![ProjectPointer {
            name: "proj-a".to_string(),
            project_dir: tmp.path().to_path_buf(),
        }]);

        assert!(matches!(
            resolve_project(&global, tmp.path().to_str().unwrap()),
            Err(ToolError::Config(_))
        ));
    }

    // -- read_board --

    #[test]
    fn read_board_is_empty_for_a_repo_with_no_open_issues() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, truncated) = read_board(&vcs, &project, 100).unwrap();

        assert!(rows.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn read_board_returns_one_row_per_open_issue_sorted_by_number() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "First", "").unwrap();
        vcs.create_issue("owner/repo-a", "Second", "").unwrap();
        vcs.create_issue("owner/repo-a", "Third", "").unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        let numbers: Vec<u64> = rows.iter().map(|row| row.number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
        assert_eq!(rows[1].title, "Second");
    }

    #[test]
    fn read_board_carries_derived_label_state_onto_the_rows() {
        let vcs = FakeVcs::new();
        let issue =
            vcs.create_issue("owner/repo-a", "Widget cache", "").unwrap();
        vcs.set_label("owner/repo-a", issue.number, "status:implement", true)
            .unwrap();
        vcs.set_label("owner/repo-a", issue.number, "P1", true).unwrap();
        vcs.set_label("owner/repo-a", issue.number, "gate:review", true)
            .unwrap();
        vcs.set_label(
            "owner/repo-a",
            issue.number,
            "approved:test-plan",
            true,
        )
        .unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        assert_eq!(rows[0].status.as_deref(), Some("implement"));
        assert_eq!(rows[0].gates, vec!["review".to_string()]);
        assert_eq!(rows[0].approvals, vec!["test-plan".to_string()]);
    }

    #[test]
    fn read_board_omits_closed_issues() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "Open", "").unwrap();
        let closed = vcs.create_issue("owner/repo-a", "Closed", "").unwrap();
        vcs.issues
            .borrow_mut()
            .get_mut(&("owner/repo-a".to_string(), closed.number))
            .unwrap()
            .state = IssueState::Closed;
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Open");
    }

    #[test]
    fn read_board_only_reports_the_bound_projects_repo() {
        // §15's "work is identified by (project, issue_number)": a board
        // that leaked another repo's issues would break the whole
        // project-rides-the-connection guarantee.
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "On a", "").unwrap();
        vcs.create_issue("owner/repo-b", "On b", "").unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "On a");
    }

    #[test]
    fn read_board_reports_truncation_when_the_repo_fills_the_limit() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "One", "").unwrap();
        vcs.create_issue("owner/repo-a", "Two", "").unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (rows, truncated) = read_board(&vcs, &project, 2).unwrap();

        assert_eq!(rows.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn read_board_uses_the_projects_gh_host_for_issue_urls() {
        let vcs = FakeVcs::new();
        vcs.create_issue("owner/repo-a", "One", "").unwrap();
        let mut project =
            project_config("a", "owner/repo-a", Path::new("/tmp/a"));
        project.gh = Some(GhConfig {
            host: "github.example.com".to_string(),
            account: "jon".to_string(),
        });

        let (rows, _) = read_board(&vcs, &project, 100).unwrap();

        assert_eq!(
            rows[0].url,
            "https://github.example.com/owner/repo-a/issues/1"
        );
    }

    // -- read_issue --

    #[test]
    fn read_issue_returns_the_snapshot_and_its_github_url() {
        let vcs = FakeVcs::new();
        let issue =
            vcs.create_issue("owner/repo-a", "Widget cache", "").unwrap();
        vcs.set_label("owner/repo-a", issue.number, "status:groom", true)
            .unwrap();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (snapshot, url) =
            read_issue(&vcs, &project, issue.number).unwrap();

        assert_eq!(snapshot.number, issue.number);
        assert_eq!(snapshot.title, "Widget cache");
        assert_eq!(snapshot.status.as_deref(), Some("groom"));
        assert_eq!(url, "https://github.com/owner/repo-a/issues/1");
    }

    #[test]
    fn read_issue_reports_an_unknown_issue_as_a_vcs_error() {
        let vcs = FakeVcs::new();
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        assert!(matches!(
            read_issue(&vcs, &project, 404),
            Err(ToolError::Vcs(_))
        ));
    }

    #[test]
    fn read_issue_reads_a_closed_issue_rather_than_hiding_it() {
        // Unlike `board` (open issues only), `issue(number)` is an
        // explicit lookup -- a closed issue is a legitimate answer, and
        // §15's 1:1:1:1 drift check needs to be able to ask about one.
        let vcs = FakeVcs::new();
        let issue = vcs.create_issue("owner/repo-a", "Done", "").unwrap();
        vcs.issues
            .borrow_mut()
            .get_mut(&("owner/repo-a".to_string(), issue.number))
            .unwrap()
            .state = IssueState::Closed;
        let project = project_config("a", "owner/repo-a", Path::new("/tmp/a"));

        let (snapshot, _) = read_issue(&vcs, &project, issue.number).unwrap();

        assert_eq!(snapshot.state, IssueState::Closed);
    }
}
