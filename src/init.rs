//! `spec-flow init` — project registration (§2.5, §6, §11).
//!
//! Run inside a repo, `init` is what makes that repo one this daemon
//! manages: it writes the project's own `<project_dir>/config.yaml`
//! (`config.rs`), registers the project in the machine-global registry
//! (`registry.rs`), and scaffolds the repo's committed `.spec-flow/`
//! files — `workflow.yaml` pre-filled with the default spec-flow-sourced
//! workflow (§7.2) plus one `.spec-flow/instructions/<point>.md` per
//! injection point (§9.1). It must be idempotent: re-running it inside
//! an already-registered repo re-syncs the config + registry row and
//! never clobbers an already-edited instruction file (§6 `init`, §11.1).
//!
//! # Dependencies come in, nothing is reached for
//!
//! [`init`] takes its [`Vcs`] implementation as an argument and resolves
//! the two ambient inputs it cannot be handed — the working directory
//! and the global config's path — exactly once, at the entry point. The
//! whole sequence below then runs against explicit paths in
//! [`init_at`], so the tests exercise it against a [`FakeVcs`] and a
//! scratch directory instead of a real checkout, a reachable GitHub, and
//! the operator's home directory.
//!
//! [`FakeVcs`]: crate::vcs::FakeVcs

use std::path::{Path, PathBuf};

use crate::config::{
    Binaries, ClaimConfig, ConfigError, GhConfig, GlobalConfig, HarnessConfig,
    HarnessesConfig, Limits, MergeMode, ProjectConfig, ProjectPointer,
    load_global_config, load_project_config, project_config_path,
    save_global_config, save_project_config,
};
use crate::registry::add_project;
use crate::scaffold::{ScaffoldError, scaffold_spec_flow_dir};
use crate::vcs::Vcs;

/// Options accepted by [`init`], mirroring the `init(repo?, project_dir?)`
/// MCP call and the `spec-flow init` CLI flags (§6).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InitOptions {
    /// Override the detected GitHub repo slug (`owner/repo`); normally
    /// left unset and detected from inside the repo (§8.5).
    pub repo: Option<String>,

    /// Override the project's container directory; normally left unset
    /// and derived from the current working directory (§10.1).
    pub project_dir: Option<PathBuf>,
}

/// Errors from [`init`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InitError {
    /// A config file could not be loaded or saved.
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),

    /// A `git`/`gh` subprocess call failed.
    #[error(transparent)]
    Vcs(#[from] crate::vcs::VcsError),

    /// A `.spec-flow/` scaffolding file could not be written.
    #[error(transparent)]
    Scaffold(#[from] ScaffoldError),

    /// The current working directory could not be read, so the repo to
    /// initialize cannot be resolved.
    #[error("failed to read the current working directory")]
    CurrentDir {
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The repo directory has no parent, so it cannot sit as a sibling
    /// checkout inside a project container (§10.1).
    #[error(
        "cannot derive a project directory from `{repo_dir}`: it has no \
         parent directory. Pass --project-dir explicitly."
    )]
    NoProjectDir {
        /// The repo directory `init` was run against.
        repo_dir: PathBuf,
    },

    /// The project directory has no final component to name the project
    /// after (§11.1).
    #[error(
        "cannot derive a project name from `{project_dir}`: it has no \
         final path component"
    )]
    NoProjectName {
        /// The project container directory in question.
        project_dir: PathBuf,
    },
}

/// Register the repo in the current directory with the daemon and
/// scaffold its `.spec-flow/` files (§6 `init`, §11).
///
/// Idempotent: re-running re-syncs the project config and its registry
/// row, and leaves every already-scaffolded `.spec-flow/` file
/// untouched.
///
/// # Errors
///
/// [`InitError::Vcs`] when `git`/`gh` are missing or `gh` is not
/// authenticated for the resolved repo (checked up front, §8.5),
/// [`InitError::Config`] or [`InitError::Scaffold`] when a file cannot
/// be written, and [`InitError::CurrentDir`] /
/// [`InitError::NoProjectDir`] / [`InitError::NoProjectName`] when the
/// `<project_dir>/<branch>` layout cannot be resolved from where `init`
/// was run (§10.1).
pub fn init<V: Vcs>(vcs: &V, options: InitOptions) -> Result<(), InitError> {
    let repo_dir = std::env::current_dir()
        .map_err(|source| InitError::CurrentDir { source })?;
    let global_config_path = crate::config::global_config_path()?;
    init_at(vcs, &options, &repo_dir, &global_config_path)
}

/// [`init`] against explicit paths: `repo_dir` is the checkout to
/// register (normally the working directory), `global_config_path` the
/// machine-global config to add its registry row to.
fn init_at<V: Vcs>(
    vcs: &V,
    options: &InitOptions,
    repo_dir: &Path,
    global_config_path: &Path,
) -> Result<(), InitError> {
    let project_config = resolve_project_config(vcs, options, repo_dir)?;
    save_project_config(
        &project_config_path(&project_config.project_dir),
        &project_config,
    )?;

    let mut global_config = load_or_default_global_config(global_config_path)?;
    add_project(
        &mut global_config,
        ProjectPointer {
            name: project_config.name.clone(),
            project_dir: project_config.project_dir.clone(),
        },
    );
    save_global_config(global_config_path, &global_config)?;

    scaffold_spec_flow_dir(repo_dir)?;

    tracing::info!(
        project = %project_config.name,
        repo = %project_config.repo,
        project_dir = %project_config.project_dir.display(),
        merge_mode = ?project_config.merge_mode,
        "initialized project"
    );
    Ok(())
}

/// Interrogate `vcs` for everything `<project_dir>/config.yaml` records
/// about this repo (§8.5, §10.1, §11.1).
fn resolve_project_config<V: Vcs>(
    vcs: &V,
    options: &InitOptions,
    repo_dir: &Path,
) -> Result<ProjectConfig, InitError> {
    let repo = match &options.repo {
        Some(repo) => repo.clone(),
        None => vcs.detect_repo_slug(repo_dir)?,
    };
    vcs.ensure_ready(&repo)?;

    let default_branch = vcs.detect_default_branch(repo_dir)?;
    let merge_mode = detect_merge_mode(vcs, &repo, &default_branch)?;

    // §10.1: every branch — including the primary checkout `init` is run
    // from — is a sibling *inside* the project container, so the
    // container is the checkout's parent unless the operator says
    // otherwise.
    let project_dir = match &options.project_dir {
        Some(project_dir) => project_dir.clone(),
        None => repo_dir
            .parent()
            .ok_or_else(|| InitError::NoProjectDir {
                repo_dir: repo_dir.to_path_buf(),
            })?
            .to_path_buf(),
    };
    let name = project_dir
        .file_name()
        .ok_or_else(|| InitError::NoProjectName {
            project_dir: project_dir.clone(),
        })?
        .to_string_lossy()
        .into_owned();

    // `gh`/`harness` are operator-added-by-hand fields (§4.2, §11.1) —
    // `gh`'s ambient host/account is right unless configured for several
    // (§8.5), and the harness falls back to the global
    // `harnesses.default` (§2.6). Detection can't (re-)produce either,
    // so a re-`init` must carry forward whatever the operator already
    // set on this project rather than wiping it back to `None`.
    let (gh, harness) = existing_overrides(&project_dir);

    Ok(ProjectConfig {
        name,
        local_path: project_dir.join(&default_branch),
        index_path: project_dir.join("index"),
        memory_scope: repo.replace('/', "-"),
        project_dir,
        repo,
        default_branch,
        merge_mode,
        gh,
        harness,
    })
}

/// The `gh`/`harness` overrides already recorded in
/// `<project_dir>/config.yaml`, if a config is there to read.
///
/// Absent on a fresh `init` (no file yet) and on any read failure — a
/// corrupt or unreadable existing file is not this function's problem
/// to raise; it simply means there is nothing to carry forward.
fn existing_overrides(
    project_dir: &Path,
) -> (Option<GhConfig>, Option<String>) {
    match load_project_config(&project_config_path(project_dir)) {
        Ok(existing) => (existing.gh, existing.harness),
        Err(_) => (None, None),
    }
}

/// Whether the repo runs GitHub's native merge queue or the server's own
/// serialized merge lease (§8.1).
///
/// A repo without the native queue is the common case, not a failure:
/// warn, record the fallback, and carry on — never a hard-fail (§8.1,
/// §15).
fn detect_merge_mode<V: Vcs>(
    vcs: &V,
    repo: &str,
    default_branch: &str,
) -> Result<MergeMode, InitError> {
    if vcs.merge_queue_enabled(repo, default_branch)? {
        return Ok(MergeMode::Native);
    }
    tracing::warn!(
        repo = %repo,
        default_branch = %default_branch,
        "no native merge queue on the default branch; this project will \
         serialize merges with the server's own merge lease (spec §8.1)"
    );
    Ok(MergeMode::Serialized)
}

/// Load the machine-global config, or start a default one when the file
/// does not exist yet.
///
/// `init` is normally the first thing an operator runs, so the global
/// file usually has to be created here; `serve` fills in the
/// `instance_id` it leaves unset (§11.1). Any read failure other than
/// "not found" is a real problem and propagates.
fn load_or_default_global_config(
    path: &Path,
) -> Result<GlobalConfig, ConfigError> {
    match load_global_config(path) {
        Err(ConfigError::Read { ref source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            tracing::info!(
                path = %path.display(),
                "no global config yet; writing a default one"
            );
            Ok(default_global_config())
        }
        other => other,
    }
}

/// The machine-global daemon config as spec §11.1 documents it, with an
/// empty project registry.
fn default_global_config() -> GlobalConfig {
    let harness = |command: &[&str]| HarnessConfig {
        command: command.iter().map(|arg| (*arg).to_string()).collect(),
    };
    GlobalConfig {
        listen: std::net::SocketAddr::from(([127, 0, 0, 1], 7420)),
        instance_id: None,
        binaries: Binaries::default(),
        harnesses: HarnessesConfig {
            default: "claude".to_string(),
            harnesses: [
                ("claude".to_string(), harness(&["claude", "-p", "{prompt}"])),
                ("codex".to_string(), harness(&["codex", "exec", "{prompt}"])),
                (
                    "opencode".to_string(),
                    harness(&["opencode", "run", "{prompt}"]),
                ),
            ]
            .into_iter()
            .collect(),
        },
        limits: Limits { max_concurrent_agents: 3 },
        claim: ClaimConfig {
            heartbeat_ttl: std::time::Duration::from_secs(60 * 60),
            heartbeat_interval: std::time::Duration::from_secs(5 * 60),
        },
        phase_timeout: std::time::Duration::from_secs(45 * 60),
        projects: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_project_config;
    use crate::scaffold::{DEFAULT_WORKFLOW_YAML, INSTRUCTION_POINTS};
    use crate::vcs::{FakeVcs, VcsError};
    use std::fs;
    use tempfile::TempDir;

    const REPO: &str = "owner/repo-a";

    /// A scratch `<project_dir>/main` checkout plus a [`FakeVcs`] that
    /// answers for it: authenticated, on `main`, native merge queue on.
    struct Fixture {
        _root: TempDir,
        repo_dir: PathBuf,
        project_dir: PathBuf,
        global_config_path: PathBuf,
        vcs: FakeVcs,
    }

    impl Fixture {
        fn new() -> Fixture {
            let root = TempDir::new().unwrap();
            let project_dir = root.path().join("repo-a");
            let repo_dir = project_dir.join("main");
            fs::create_dir_all(&repo_dir).unwrap();

            let vcs = FakeVcs::new();
            vcs.authenticated.borrow_mut().insert(REPO.to_string());
            vcs.repo_slugs
                .borrow_mut()
                .insert(repo_dir.clone(), REPO.to_string());
            vcs.default_branches
                .borrow_mut()
                .insert(repo_dir.clone(), "main".to_string());
            vcs.merge_queue_enabled_repos
                .borrow_mut()
                .insert(REPO.to_string());

            Fixture {
                global_config_path: root
                    .path()
                    .join("home/.config/spec-flow/config.yaml"),
                _root: root,
                repo_dir,
                project_dir,
                vcs,
            }
        }

        fn init(&self) -> Result<(), InitError> {
            self.init_with(&InitOptions::default())
        }

        fn init_with(&self, options: &InitOptions) -> Result<(), InitError> {
            init_at(
                &self.vcs,
                options,
                &self.repo_dir,
                &self.global_config_path,
            )
        }

        fn project_config(&self) -> ProjectConfig {
            load_project_config(&project_config_path(&self.project_dir))
                .unwrap()
        }

        fn instruction_path(&self, point: &str) -> PathBuf {
            self.repo_dir
                .join(".spec-flow")
                .join("instructions")
                .join(format!("{point}.md"))
        }

        fn workflow_path(&self) -> PathBuf {
            self.repo_dir.join(".spec-flow").join("workflow.yaml")
        }
    }

    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn writes_a_project_config_from_the_detected_repo_state() {
        let fixture = Fixture::new();

        fixture.init().unwrap();

        assert_eq!(
            fixture.project_config(),
            ProjectConfig {
                name: "repo-a".to_string(),
                repo: REPO.to_string(),
                project_dir: fixture.project_dir.clone(),
                local_path: fixture.project_dir.join("main"),
                default_branch: "main".to_string(),
                merge_mode: MergeMode::Native,
                gh: None,
                harness: None,
                index_path: fixture.project_dir.join("index"),
                memory_scope: "owner-repo-a".to_string(),
            }
        );
    }

    #[test]
    fn registers_the_project_in_a_fresh_global_config() {
        let fixture = Fixture::new();

        fixture.init().unwrap();

        let global = load_global_config(&fixture.global_config_path).unwrap();
        assert_eq!(
            global.projects,
            vec![ProjectPointer {
                name: "repo-a".to_string(),
                project_dir: fixture.project_dir.clone(),
            }]
        );
    }

    #[test]
    fn re_running_init_keeps_one_registry_row_and_the_same_config() {
        let fixture = Fixture::new();
        fixture.init().unwrap();
        let first = fixture.project_config();

        fixture.init().unwrap();

        let global = load_global_config(&fixture.global_config_path).unwrap();
        assert_eq!(global.projects.len(), 1);
        assert_eq!(fixture.project_config(), first);
    }

    #[test]
    fn scaffolds_the_default_workflow_when_absent() {
        let fixture = Fixture::new();

        fixture.init().unwrap();

        assert_eq!(
            fs::read_to_string(fixture.workflow_path()).unwrap(),
            DEFAULT_WORKFLOW_YAML
        );
    }

    #[test]
    fn never_clobbers_an_edited_workflow() {
        let fixture = Fixture::new();
        let edited = "phases: []   # our own pipeline\n";
        write_file(&fixture.workflow_path(), edited);

        fixture.init().unwrap();

        assert_eq!(
            fs::read_to_string(fixture.workflow_path()).unwrap(),
            edited
        );
    }

    #[test]
    fn materializes_every_instruction_point() {
        let fixture = Fixture::new();

        fixture.init().unwrap();

        for (point, _) in INSTRUCTION_POINTS {
            let path = fixture.instruction_path(point);
            let contents = fs::read_to_string(&path).unwrap_or_else(|_| {
                panic!("no instruction file for `{point}`")
            });
            assert!(
                contents.contains(point),
                "instruction file for `{point}` does not name its point"
            );
        }
    }

    #[test]
    fn never_clobbers_an_edited_instruction_file() {
        let fixture = Fixture::new();
        let path = fixture.instruction_path("implement");
        let edited = "<!-- mode: replace -->\nOur house rules.\n";
        write_file(&path, edited);

        fixture.init().unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), edited);
    }

    #[test]
    fn records_serialized_merge_mode_when_the_queue_is_off() {
        let fixture = Fixture::new();
        fixture.vcs.merge_queue_enabled_repos.borrow_mut().clear();

        fixture.init().unwrap();

        assert_eq!(fixture.project_config().merge_mode, MergeMode::Serialized);
    }

    #[test]
    fn fails_when_gh_is_not_authenticated_for_the_repo() {
        let fixture = Fixture::new();
        fixture.vcs.authenticated.borrow_mut().clear();

        assert!(matches!(
            fixture.init(),
            Err(InitError::Vcs(VcsError::NotAuthenticated { .. }))
        ));
    }

    #[test]
    fn honors_the_repo_override_instead_of_detecting() {
        let fixture = Fixture::new();
        fixture.vcs.repo_slugs.borrow_mut().clear();
        fixture
            .vcs
            .authenticated
            .borrow_mut()
            .insert("owner/other".to_string());

        fixture
            .init_with(&InitOptions {
                repo: Some("owner/other".to_string()),
                project_dir: None,
            })
            .unwrap();

        assert_eq!(fixture.project_config().repo, "owner/other");
    }

    #[test]
    fn re_running_init_preserves_hand_added_gh_and_harness_overrides() {
        let fixture = Fixture::new();
        fixture.init().unwrap();
        let mut edited = fixture.project_config();
        edited.gh = Some(GhConfig {
            host: "github.example.com".to_string(),
            account: "me".to_string(),
        });
        edited.harness = Some("codex".to_string());
        save_project_config(
            &project_config_path(&fixture.project_dir),
            &edited,
        )
        .unwrap();

        fixture.init().unwrap();

        let reloaded = fixture.project_config();
        assert_eq!(reloaded.gh, edited.gh);
        assert_eq!(reloaded.harness, edited.harness);
    }
}
