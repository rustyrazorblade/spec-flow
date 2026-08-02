//! `spec-flow init` — project registration (§2.5, §6, §11).
//!
//! Run inside a repo, `init` is what makes that repo one this daemon
//! manages: it writes the project's own config file (`config.rs`'s
//! `projects_config_dir`), registers the project in the machine-global
//! registry (`registry.rs`), and scaffolds the repo's committed
//! `.spec-flow/`
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
    Binaries, ClaimConfig, ConfigError, CrossProjectMode, GhConfig,
    GlobalConfig, HarnessConfig, HarnessesConfig, Limits, MergeMode,
    ProjectConfig, ProjectPointer, load_global_config, load_project_config,
    project_config_path, save_global_config, save_project_config,
};
use crate::registry::add_project;
use crate::scaffold::{ScaffoldError, scaffold_spec_flow_dir};
use crate::vcs::Vcs;
use crate::workflow::{WorkflowError, label_vocabulary, parse_workflow};

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

    /// `.spec-flow/workflow.yaml` could not be read back after
    /// scaffolding, to derive the label vocabulary to provision (§14
    /// step 9). Distinct from [`InitError::Scaffold`]: the file was just
    /// written (or already existed) — this is a *read* failure on a path
    /// that should exist.
    #[error("failed to read {path}")]
    ReadWorkflow {
        /// The workflow file that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// `.spec-flow/workflow.yaml` exists but does not parse (a team
    /// hand-edit gone wrong). `init` must fail loudly here rather than
    /// silently skip label provisioning — an unprovisioned label
    /// vocabulary breaks every later `gate:`/`status:`/`approved:` label
    /// write against a real GitHub repo (see [`crate::vcs::Vcs::ensure_label`]'s
    /// doc), a failure mode a human should notice at `init` time, not
    /// weeks later against a live `serve` daemon.
    ///
    /// A named struct variant rather than `#[from] WorkflowError`
    /// deliberately: `WorkflowError` alone renders as a bare serde_yaml
    /// message with no mention of *which* file failed to parse, leaving
    /// a team member to guess.
    #[error("failed to parse {path}")]
    Workflow {
        /// The workflow file that failed to parse.
        path: PathBuf,
        /// The underlying parse failure.
        #[source]
        source: WorkflowError,
    },

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

    /// The checkout `init` was run from is not the primary checkout
    /// §10.1 requires — `<project_dir>/<default_branch>` — whether
    /// `project_dir` was derived from `repo_dir`'s parent or given
    /// explicitly via `--project-dir`.
    #[error(
        "`{repo_dir}` is not `{project_dir}/{default_branch}` — \
         spec-flow requires the primary checkout to be \
         `<project_dir>/<default_branch>` (§10.1). Move or rename this \
         checkout so its final path component is `{default_branch}` and \
         its parent is `{project_dir}`, or pass a --project-dir under \
         which that already holds"
    )]
    CheckoutNotSiblingOfDefaultBranch {
        /// The checkout directory `init` was run against.
        repo_dir: PathBuf,
        /// The project container — derived from `repo_dir`'s parent, or
        /// the `--project-dir` override.
        project_dir: PathBuf,
        /// The repo's actual default branch.
        default_branch: String,
    },

    /// `path` could not be canonicalized — resolved to an absolute,
    /// symlink-free form (§10.1's layout check compares canonical
    /// paths, since a relative `--project-dir` or a symlinked checkout
    /// must not be spuriously rejected). Most commonly: the path does
    /// not exist.
    #[error("could not resolve `{path}` to a canonical path")]
    UnresolvablePath {
        /// The path that failed to canonicalize.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
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
/// be written, and [`InitError::CurrentDir`] / [`InitError::NoProjectDir`]
/// / [`InitError::NoProjectName`] / [`InitError::UnresolvablePath`] /
/// [`InitError::CheckoutNotSiblingOfDefaultBranch`] when the
/// `<project_dir>/<default_branch>` layout cannot be resolved from
/// where `init` was run, or from the given `--project-dir` (§10.1).
pub fn init<V: Vcs>(vcs: &V, options: InitOptions) -> Result<(), InitError> {
    let repo_dir = std::env::current_dir()
        .map_err(|source| InitError::CurrentDir { source })?;
    let global_config_path = crate::config::global_config_path()?;
    let projects_config_dir = crate::config::projects_config_dir()?;
    init_at(
        vcs,
        &options,
        &repo_dir,
        &global_config_path,
        &projects_config_dir,
    )
}

/// [`init`] against explicit paths: `repo_dir` is the checkout to
/// register (normally the working directory), `global_config_path` the
/// machine-global config to add its registry row to, and
/// `projects_config_dir` where every registered project's own config
/// file lives (§11.1).
fn init_at<V: Vcs>(
    vcs: &V,
    options: &InitOptions,
    repo_dir: &Path,
    global_config_path: &Path,
    projects_config_dir: &Path,
) -> Result<(), InitError> {
    let project_config =
        resolve_project_config(vcs, options, repo_dir, projects_config_dir)?;
    save_project_config(
        &project_config_path(projects_config_dir, &project_config.name),
        &project_config,
    )?;

    let mut global_config = load_or_default_global_config(global_config_path)?;
    // Two different containers can share a basename (`~/work/foo` and
    // `~/oss/foo` both name a project `foo`) — capture what the
    // registry pointed at *before* this call so a same-name repoint to
    // a different directory is surfaced, not silently stolen from
    // whatever project owned that name before.
    let previous_project_dir =
        crate::registry::find_project(&global_config, &project_config.name)
            .map(|pointer| pointer.project_dir.clone());
    add_project(
        &mut global_config,
        ProjectPointer {
            name: project_config.name.clone(),
            project_dir: project_config.project_dir.clone(),
        },
    );
    if let Some(previous_project_dir) = previous_project_dir
        && previous_project_dir != project_config.project_dir
    {
        tracing::warn!(
            project = %project_config.name,
            previous_project_dir = %previous_project_dir.display(),
            project_dir = %project_config.project_dir.display(),
            "registry entry repointed to a different directory under the \
             same project name; the previous directory's project is now \
             unregistered"
        );
    }
    save_global_config(global_config_path, &global_config)?;

    scaffold_spec_flow_dir(repo_dir)?;
    provision_label_vocabulary(vcs, repo_dir, &project_config.repo)?;

    tracing::info!(
        project = %project_config.name,
        repo = %project_config.repo,
        project_dir = %project_config.project_dir.display(),
        merge_mode = ?project_config.merge_mode,
        "initialized project"
    );
    Ok(())
}

/// Create every label the repo's **effective** `.spec-flow/workflow.yaml`
/// vocabulary needs (§14 step 9, [`crate::workflow::label_vocabulary`]),
/// so every later `gate:`/`status:`/`approved:`/`spec:skip` label write
/// this daemon makes has somewhere to land — see
/// [`crate::vcs::Vcs::ensure_label`]'s doc for the real `gh` limitation
/// this closes.
///
/// Reads the workflow file back from disk **after** scaffolding, rather
/// than assuming the compiled-in default: a re-`init` over a
/// team-hand-edited `workflow.yaml` (§6 `init`'s "never clobbers an
/// edited file" contract, honored by [`scaffold_spec_flow_dir`] already
/// running above) must provision the team's *actual* vocabulary, not the
/// shipped one it may have since diverged from.
///
/// **Known gap:** this only runs when `init` itself runs. The realistic
/// sequence is `init` (writes the *shipped default* workflow.yaml) →
/// team edits it afterward (adding a phase, a status, a custom lease
/// label) → nobody re-runs `init`. That edit's new label names are never
/// provisioned, and the "surface at `init` time, not weeks later against
/// a live `serve` daemon" reasoning above only actually holds for the
/// *re*-`init` case, not this more common one. Closing this needs
/// something that isn't built yet — `serve` re-provisioning at startup,
/// or a `workflow.yaml` mtime/hash check — flagged here rather than
/// silently assumed solved by this function's existence.
fn provision_label_vocabulary<V: Vcs>(
    vcs: &V,
    repo_dir: &Path,
    repo: &str,
) -> Result<(), InitError> {
    let workflow_path = repo_dir.join(".spec-flow").join("workflow.yaml");
    let workflow_yaml =
        std::fs::read_to_string(&workflow_path).map_err(|source| {
            InitError::ReadWorkflow { path: workflow_path.clone(), source }
        })?;
    let workflow = parse_workflow(&workflow_yaml).map_err(|source| {
        InitError::Workflow { path: workflow_path, source }
    })?;
    for label in label_vocabulary(&workflow) {
        vcs.ensure_label(repo, &label)?;
    }
    Ok(())
}

/// Interrogate `vcs` for everything this repo's own config file records
/// (§8.5, §10.1, §11.1).
fn resolve_project_config<V: Vcs>(
    vcs: &V,
    options: &InitOptions,
    repo_dir: &Path,
    projects_config_dir: &Path,
) -> Result<ProjectConfig, InitError> {
    let repo = match &options.repo {
        Some(repo) => repo.clone(),
        None => vcs.detect_repo_slug(repo_dir)?,
    };
    vcs.ensure_ready(&repo)?;

    let default_branch = vcs.detect_default_branch(repo_dir)?;
    let merge_mode = detect_merge_mode(vcs, &repo, &default_branch)?;

    // Every branch — including the primary checkout `init` is run
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

    // Canonicalize both sides before comparing (and keep the canonical
    // `project_dir` for everything below): `repo_dir` is already
    // absolute (from `std::env::current_dir()`, §10.2) but may still
    // traverse a symlink (macOS's `/tmp` -> `/private/tmp`), and an
    // operator-supplied `--project-dir` may be relative (`..`) or itself
    // symlinked. A purely lexical comparison would spuriously reject
    // every one of those spellings of an otherwise-correct §10.1
    // layout — only the one exact absolute string `repo_dir.parent()`
    // happens to produce would ever pass.
    let repo_dir_canonical = repo_dir.canonicalize().map_err(|source| {
        InitError::UnresolvablePath { path: repo_dir.to_path_buf(), source }
    })?;
    let project_dir = project_dir.canonicalize().map_err(|source| {
        InitError::UnresolvablePath { path: project_dir.clone(), source }
    })?;

    // §10.1: the primary checkout must itself BE `<project_dir>/
    // <default_branch>` — checked against the resolved `project_dir`
    // above, whether that came from `repo_dir`'s parent or an explicit
    // `--project-dir`. A plain `git clone` names its directory after the
    // repo, not the branch, and an operator-supplied `--project-dir`
    // that doesn't actually contain this checkout as its
    // `<default_branch>` sibling is exactly as wrong: either way,
    // silently proceeding would record a `local_path` that does not
    // exist on disk, and every later worktree/coordinator operation that
    // trusts it would inherit the drift.
    if project_dir.join(&default_branch) != repo_dir_canonical {
        return Err(InitError::CheckoutNotSiblingOfDefaultBranch {
            repo_dir: repo_dir.to_path_buf(),
            project_dir,
            default_branch,
        });
    }

    let name = project_dir
        .file_name()
        .ok_or_else(|| InitError::NoProjectName {
            project_dir: project_dir.clone(),
        })?
        .to_string_lossy()
        .into_owned();

    // `gh`/`harness`/`weight` are operator-added-by-hand fields (§4.2,
    // §11.1) — `gh`'s ambient host/account is right unless configured
    // for several (§8.5), the harness falls back to the global
    // `harnesses.default` (§2.6), and `weight` falls back to the default
    // weight of `1` (§12, §14 step 10). Detection can't (re-)produce any
    // of the three, so a re-`init` must carry forward whatever the
    // operator already set on this project rather than wiping it back
    // to `None`.
    let overrides = existing_overrides(projects_config_dir, &name)?;

    Ok(ProjectConfig {
        name,
        local_path: project_dir.join(&default_branch),
        index_path: project_dir.join("index"),
        memory_scope: repo.replace('/', "-"),
        project_dir,
        repo,
        default_branch,
        merge_mode,
        gh: overrides.gh,
        harness: overrides.harness,
        weight: overrides.weight,
    })
}

/// The operator-added-by-hand overrides [`existing_overrides`] reads
/// back from the project's own config file, if one is there to read.
///
/// All fields absent only on a fresh `init` (no file yet) — anything
/// else that keeps the file from being read is a hard error, not
/// "nothing to carry forward"; see [`existing_overrides`]'s `# Errors`
/// section.
#[derive(Default)]
struct ExistingOverrides {
    gh: Option<GhConfig>,
    harness: Option<String>,
    weight: Option<u32>,
}

/// Read [`ExistingOverrides`] back from `name`'s config file, if one
/// exists yet.
///
/// Keyed by project `name` rather than `project_dir`: a same-name
/// re-`init` that repoints `project_dir` (see `init_at`'s
/// `previous_project_dir` handling) is still the same project's config
/// to carry forward, not a fresh one.
///
/// # Errors
///
/// Propagates any [`ConfigError`] other than "the file does not exist
/// yet" — a fresh `init` has nothing to carry forward, but a file that
/// exists and fails to parse (a hand-edit gone wrong) or fails to read
/// (a permissions problem) is NOT "nothing to carry forward": `init_at`
/// is about to overwrite this file, and warning-then-proceeding would
/// still destroy the operator's overrides the moment it printed the
/// warning — exactly the clobber this function exists to prevent. A
/// hard failure here tells the operator to fix or remove the file
/// before re-running `init`, rather than `init` silently regenerating
/// over it.
fn existing_overrides(
    projects_config_dir: &Path,
    name: &str,
) -> Result<ExistingOverrides, ConfigError> {
    match load_project_config(&project_config_path(projects_config_dir, name))
    {
        Ok(existing) => Ok(ExistingOverrides {
            gh: existing.gh,
            harness: existing.harness,
            weight: existing.weight,
        }),
        Err(ConfigError::Read { ref source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(ExistingOverrides::default())
        }
        Err(error) => Err(error),
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
        cross_project_mode: CrossProjectMode::default(),
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
    use std::collections::HashSet;
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
        projects_config_dir: PathBuf,
        vcs: FakeVcs,
    }

    impl Fixture {
        fn new() -> Fixture {
            let root = TempDir::new().unwrap();
            let project_dir = root.path().join("repo-a");
            let repo_dir = project_dir.join("main");
            fs::create_dir_all(&repo_dir).unwrap();
            // Canonicalize once, here, so every seeded FakeVcs lookup and
            // every assertion in this module compares against the same
            // resolved form `resolve_project_config` itself compares
            // against (§10.1's layout check canonicalizes both sides —
            // see `fails_when...` tests below for why that matters).
            let project_dir = project_dir.canonicalize().unwrap();
            let repo_dir = repo_dir.canonicalize().unwrap();

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
                projects_config_dir: root
                    .path()
                    .join("home/.config/spec-flow/projects"),
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
                &self.projects_config_dir,
            )
        }

        fn config_path(&self) -> PathBuf {
            let name = self.project_dir.file_name().unwrap().to_string_lossy();
            project_config_path(&self.projects_config_dir, &name)
        }

        fn project_config(&self) -> ProjectConfig {
            load_project_config(&self.config_path()).unwrap()
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
                weight: None,
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
        // Minimal but schema-valid (not just any text) -- provisioning
        // the label vocabulary (see `provision_label_vocabulary`) now
        // parses whatever is on disk, so this fixture must be a real,
        // if tiny, workflow.yaml rather than an arbitrary stub.
        let fixture = Fixture::new();
        let edited = "labels: {priority: [P0], status: [a], gate_prefix: \"gate:\", approval_prefix: \"approved:\", owner_prefix: \"owner:\"}\n\
                       spec: {tool: openspec, optional: false, skip_label: \"spec:skip\"}\n\
                       review_panel: []\n\
                       fix_loop: {max_rounds: 1, on_exhausted: escalate, on_decision_finding: escalate}\n\
                       phases: []   # our own pipeline\n";
        write_file(&fixture.workflow_path(), edited);

        fixture.init().unwrap();

        assert_eq!(
            fs::read_to_string(fixture.workflow_path()).unwrap(),
            edited
        );
    }

    #[test]
    fn fails_loudly_when_the_edited_workflow_does_not_parse() {
        // The label-vocabulary provisioning step (§14 step 9) parses
        // whatever workflow.yaml is actually on disk -- a hand-edit gone
        // wrong must fail `init` loudly, not silently skip provisioning
        // and leave every later gate:/status:/approved: label write
        // broken against a real repo.
        let fixture = Fixture::new();
        write_file(&fixture.workflow_path(), "phases: [this is not valid\n");

        assert!(matches!(
            fixture.init(),
            Err(InitError::Workflow {
                source: WorkflowError::InvalidYaml { .. },
                ..
            })
        ));
    }

    #[test]
    fn provisions_exactly_the_default_workflows_label_vocabulary() {
        // An exact-set comparison against `label_vocabulary` itself,
        // not a handful of spot-checked examples -- this is an
        // integration check on init's WIRING (does every label
        // `label_vocabulary` returns actually reach `ensure_label`,
        // and nothing else), not a re-verification of `label_vocabulary`'s
        // own derivation logic, which `workflow::tests` already covers
        // per label category.
        let fixture = Fixture::new();

        fixture.init().unwrap();

        let expected: HashSet<String> =
            label_vocabulary(&parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap())
                .into_iter()
                .collect();
        let ensured = fixture.vcs.labels_ensured.borrow();
        let ensured =
            ensured.get(REPO).expect("labels ensured for the repo").clone();
        assert_eq!(ensured, expected);
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
    fn fails_when_the_checkout_is_not_named_for_the_default_branch() {
        let root = TempDir::new().unwrap();
        let project_dir = root.path().join("repo-a");
        // Named after the repo, the way a plain `git clone` would name
        // it — NOT after the default branch, so it cannot be the
        // primary checkout §10.1 requires.
        let repo_dir = project_dir.join("repo-a");
        fs::create_dir_all(&repo_dir).unwrap();

        let vcs = FakeVcs::new();
        vcs.authenticated.borrow_mut().insert(REPO.to_string());
        vcs.repo_slugs.borrow_mut().insert(repo_dir.clone(), REPO.to_string());
        vcs.default_branches
            .borrow_mut()
            .insert(repo_dir.clone(), "main".to_string());
        vcs.merge_queue_enabled_repos.borrow_mut().insert(REPO.to_string());
        let global_config_path =
            root.path().join("home/.config/spec-flow/config.yaml");
        let projects_config_dir =
            root.path().join("home/.config/spec-flow/projects");

        let error = init_at(
            &vcs,
            &InitOptions::default(),
            &repo_dir,
            &global_config_path,
            &projects_config_dir,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            InitError::CheckoutNotSiblingOfDefaultBranch { .. }
        ));
        // Nothing was written: a rejected init must not leave a
        // half-written project config behind.
        assert!(!project_config_path(&projects_config_dir, "repo-a").exists());
    }

    #[test]
    fn fails_when_project_dir_override_does_not_actually_contain_the_checkout()
    {
        // The checkout IS correctly named `main`, and the override
        // exists on disk, but it doesn't actually have the checkout as
        // its `main` sibling — exactly as wrong as a misnamed checkout
        // (§10.1), and must not be accepted just because it was
        // explicit.
        let root = TempDir::new().unwrap();
        let real_project_dir = root.path().join("repo-a");
        let repo_dir = real_project_dir.join("main");
        fs::create_dir_all(&repo_dir).unwrap();
        let wrong_project_dir = root.path().join("somewhere-else");
        fs::create_dir_all(&wrong_project_dir).unwrap();

        let vcs = seeded_vcs(&repo_dir, REPO);
        let global_config_path =
            root.path().join("home/.config/spec-flow/config.yaml");
        let projects_config_dir =
            root.path().join("home/.config/spec-flow/projects");

        let error = init_at(
            &vcs,
            &InitOptions {
                repo: None,
                project_dir: Some(wrong_project_dir.clone()),
            },
            &repo_dir,
            &global_config_path,
            &projects_config_dir,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            InitError::CheckoutNotSiblingOfDefaultBranch { .. }
        ));
        // Config storage is keyed by project name, not by either
        // directory candidate above -- one check now covers what two
        // did before.
        assert!(!project_config_path(&projects_config_dir, "repo-a").exists());
    }

    #[test]
    fn fails_when_project_dir_override_does_not_exist() {
        let root = TempDir::new().unwrap();
        let project_dir = root.path().join("repo-a");
        let repo_dir = project_dir.join("main");
        fs::create_dir_all(&repo_dir).unwrap();
        let nonexistent = root.path().join("never-created");

        let vcs = seeded_vcs(&repo_dir, REPO);
        let global_config_path =
            root.path().join("home/.config/spec-flow/config.yaml");
        let projects_config_dir =
            root.path().join("home/.config/spec-flow/projects");

        let error = init_at(
            &vcs,
            &InitOptions { repo: None, project_dir: Some(nonexistent) },
            &repo_dir,
            &global_config_path,
            &projects_config_dir,
        )
        .unwrap_err();

        assert!(matches!(error, InitError::UnresolvablePath { .. }));
    }

    #[test]
    fn accepts_a_relative_project_dir_override_that_resolves_to_the_true_parent()
     {
        // A relative override (`..`) or one that traverses a symlink
        // must be accepted when it genuinely resolves to the checkout's
        // real parent — the §10.1 layout check compares canonical
        // paths precisely so spellings like this aren't spuriously
        // rejected.
        let root = TempDir::new().unwrap();
        let project_dir = root.path().join("repo-a");
        let repo_dir = project_dir.join("main");
        fs::create_dir_all(&repo_dir).unwrap();
        let project_dir = project_dir.canonicalize().unwrap();
        let repo_dir = repo_dir.canonicalize().unwrap();

        let vcs = seeded_vcs(&repo_dir, REPO);
        let global_config_path =
            root.path().join("home/.config/spec-flow/config.yaml");
        let projects_config_dir =
            root.path().join("home/.config/spec-flow/projects");

        init_at(
            &vcs,
            &InitOptions {
                repo: None,
                project_dir: Some(repo_dir.join("..")),
            },
            &repo_dir,
            &global_config_path,
            &projects_config_dir,
        )
        .unwrap();

        let config = load_project_config(&project_config_path(
            &projects_config_dir,
            "repo-a",
        ))
        .unwrap();
        assert_eq!(config.project_dir, project_dir);
    }

    /// A [`FakeVcs`] authenticated + ready for `repo_dir`/`repo`, with
    /// `main` as the default branch and the native merge queue on.
    fn seeded_vcs(repo_dir: &Path, repo: &str) -> FakeVcs {
        let vcs = FakeVcs::new();
        vcs.authenticated.borrow_mut().insert(repo.to_string());
        vcs.repo_slugs
            .borrow_mut()
            .insert(repo_dir.to_path_buf(), repo.to_string());
        vcs.default_branches
            .borrow_mut()
            .insert(repo_dir.to_path_buf(), "main".to_string());
        vcs.merge_queue_enabled_repos.borrow_mut().insert(repo.to_string());
        vcs
    }

    #[test]
    fn repointing_a_project_name_to_a_different_directory_still_succeeds() {
        // Two unrelated containers that happen to share a basename
        // (`repo-a`) — `init`'s derived project *name* collides even
        // though the projects are unrelated.
        let root = TempDir::new().unwrap();
        let global_config_path =
            root.path().join("home/.config/spec-flow/config.yaml");
        let projects_config_dir =
            root.path().join("home/.config/spec-flow/projects");

        let work_project_dir = root.path().join("work/repo-a");
        let work_repo_dir = work_project_dir.join("main");
        fs::create_dir_all(&work_repo_dir).unwrap();
        let work_vcs = seeded_vcs(&work_repo_dir, "owner/work-repo-a");
        init_at(
            &work_vcs,
            &InitOptions::default(),
            &work_repo_dir,
            &global_config_path,
            &projects_config_dir,
        )
        .unwrap();

        let oss_project_dir = root.path().join("oss/repo-a");
        let oss_repo_dir = oss_project_dir.join("main");
        fs::create_dir_all(&oss_repo_dir).unwrap();
        // Canonicalize before asserting below — `resolve_project_config`
        // records the canonical form (§10.1's layout check compares
        // canonical paths), and this container may sit under a
        // symlinked temp dir (e.g. macOS's `/tmp` -> `/private/tmp`).
        let oss_project_dir = oss_project_dir.canonicalize().unwrap();
        let oss_repo_dir = oss_repo_dir.canonicalize().unwrap();
        let oss_vcs = seeded_vcs(&oss_repo_dir, "owner/oss-repo-a");

        // Must succeed (never a hard failure) — repointing a name is
        // surfaced via a warning (not asserted here; this crate has no
        // test-subscriber capture yet), not blocked.
        init_at(
            &oss_vcs,
            &InitOptions::default(),
            &oss_repo_dir,
            &global_config_path,
            &projects_config_dir,
        )
        .unwrap();

        let global = load_global_config(&global_config_path).unwrap();
        assert_eq!(global.projects.len(), 1);
        assert_eq!(global.projects[0].project_dir, oss_project_dir);
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
    fn re_running_init_preserves_hand_added_gh_harness_and_weight_overrides() {
        let fixture = Fixture::new();
        fixture.init().unwrap();
        let mut edited = fixture.project_config();
        edited.gh = Some(GhConfig {
            host: "github.example.com".to_string(),
            account: "me".to_string(),
        });
        edited.harness = Some("codex".to_string());
        edited.weight = Some(5);
        save_project_config(&fixture.config_path(), &edited).unwrap();

        fixture.init().unwrap();

        let reloaded = fixture.project_config();
        assert_eq!(reloaded.gh, edited.gh);
        assert_eq!(reloaded.harness, edited.harness);
        assert_eq!(reloaded.weight, edited.weight);
    }

    #[test]
    fn re_running_init_over_an_unparseable_existing_config_fails_loudly() {
        let fixture = Fixture::new();
        fixture.init().unwrap();
        // A hand-edit gone wrong — not "no file yet" (which has nothing
        // to carry forward) but a real read failure. `init` is about to
        // overwrite this file, so silently proceeding with `(None,
        // None)` would destroy whatever `gh`/`harness` overrides it held
        // the moment it printed a warning. It must fail loudly instead,
        // and must not touch the file first.
        write_file(
            &fixture.config_path(),
            "gh: [this is not a project config\n",
        );

        assert!(matches!(
            fixture.init(),
            Err(InitError::Config(ConfigError::Parse { .. }))
        ));

        // Untouched: a rejected re-init must not overwrite the file it
        // couldn't safely read from.
        assert_eq!(
            fs::read_to_string(fixture.config_path()).unwrap(),
            "gh: [this is not a project config\n"
        );
    }

    #[test]
    fn re_running_init_over_an_existing_zero_weight_fails_loudly() {
        // weight: 0 is rejected at `load_project_config` time
        // (§14 step 10) -- a re-init must surface that as a hard
        // failure via the same `existing_overrides` catch-all that
        // handles an unparseable file, not silently ignore it.
        let fixture = Fixture::new();
        fixture.init().unwrap();
        let mut edited = fixture.project_config();
        edited.weight = Some(0);
        let path = fixture.config_path();
        save_project_config(&path, &edited).unwrap();

        assert!(matches!(
            fixture.init(),
            Err(InitError::Config(ConfigError::InvalidWeight { .. }))
        ));

        // Untouched: a rejected re-init must not overwrite the file it
        // couldn't safely read from. Read the raw text, not via
        // `load_project_config` -- that call would itself error on the
        // same `weight: 0` this test just wrote.
        assert!(fs::read_to_string(&path).unwrap().contains("weight: 0"));
    }
}
