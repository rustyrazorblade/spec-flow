//! Machine-local configuration: the global daemon file and the
//! per-project file (spec §11.1).
//!
//! Two YAML shapes, two files, two lifecycles:
//!
//! - [`GlobalConfig`] — `~/.config/spec-flow/config.yaml`, one per daemon.
//!   Daemon settings (bind address, binaries, harnesses, limits, claim
//!   TTL) plus the **project registry** (`projects`): a pointer list of
//!   every project this daemon manages. `registry.rs` operates on that
//!   list.
//! - [`ProjectConfig`] — `<project_dir>/config.yaml`, one per registered
//!   repo. That project's own settings: repo slug, paths, `gh` scoping,
//!   merge mode, harness override, memory/index location.
//!
//! Neither file is committed to the project's git repository — both hold
//! machine-local absolute paths and account names (§11). The committed,
//! team-shared workflow config (`.spec-flow/workflow.yaml`) is a separate
//! concern, owned by a later task (the phase engine, §14 step 7).

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Errors reading or writing a [`GlobalConfig`] or [`ProjectConfig`] file.
///
/// Each variant carries the path it failed on, so a caller can report
/// exactly which of possibly several config files (one global, many
/// per-project) is at fault.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The config file could not be read from disk.
    #[error("failed to read config file at {path}")]
    Read {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The config file could not be written to disk.
    #[error("failed to write config file at {path}")]
    Write {
        /// The path that could not be written.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The config file's contents are not valid YAML for its shape.
    #[error("failed to parse config file at {path}")]
    Parse {
        /// The path whose contents failed to parse.
        path: PathBuf,
        /// The underlying parse failure.
        #[source]
        source: serde_yaml::Error,
    },

    /// A config value could not be serialized back to YAML.
    #[error("failed to serialize config for {path}")]
    Serialize {
        /// The path the value was being prepared for.
        path: PathBuf,
        /// The underlying serialization failure.
        #[source]
        source: serde_yaml::Error,
    },

    /// The operator's home directory could not be determined (`HOME` is
    /// unset), so the global config's default path cannot be resolved.
    #[error(
        "could not determine the operator's home directory (HOME is \
         unset); pass an explicit config path instead"
    )]
    HomeDirUnavailable,
}

/// One entry in the global config's project registry (§2.5, §4.2).
///
/// A pointer only — `name` plus where the project lives on this machine.
/// The project's actual settings (repo slug, paths, `gh` scoping, ...)
/// live in that project's own `<project_dir>/config.yaml`, never
/// duplicated here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProjectPointer {
    /// The project handle (defaults to the project directory's name at
    /// `init`; also the coordinator's session-scope identifier, §2.5).
    pub name: String,

    /// Absolute path to the project's container directory — every
    /// branch checkout (the primary checkout and every issue worktree)
    /// lives as a sibling inside it (§10.1).
    pub project_dir: PathBuf,
}

/// Local `git`/`gh` binary locations (§8.5).
///
/// Defaults to resolving both on `PATH`; either can be pinned to an
/// absolute path when the daemon's `PATH` (e.g. under `launchd`) differs
/// from an interactive shell's, or to pin a specific Homebrew install.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Binaries {
    /// The `git` binary: a bare name (resolved on `PATH`) or an absolute
    /// path.
    #[serde(default = "default_git_binary")]
    pub git: String,

    /// The `gh` binary: a bare name (resolved on `PATH`) or an absolute
    /// path.
    #[serde(default = "default_gh_binary")]
    pub gh: String,
}

impl Default for Binaries {
    fn default() -> Binaries {
        Binaries { git: default_git_binary(), gh: default_gh_binary() }
    }
}

fn default_git_binary() -> String {
    "git".to_string()
}

fn default_gh_binary() -> String {
    "gh".to_string()
}

/// A single spawn-harness launch template (§2.6, §11.1).
///
/// `{prompt}` / `{spawn_token}` / `{worktree}` interpolate into `command`
/// when the process spawner (a later task, §14 step 3) launches a phase.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessConfig {
    /// The argv to exec, with `{prompt}`/`{spawn_token}`/`{worktree}`
    /// placeholders.
    pub command: Vec<String>,
}

/// The harness registry: which headless agent CLI spawns a phase, and
/// how to invoke each one (§2.6, §11.1).
///
/// Harness-agnostic by design — `claude`/`codex`/`opencode` ship as
/// first-class named entries, but any headless CLI works given a launch
/// template. `default` names the entry used unless a project, phase, or
/// role overrides it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessesConfig {
    /// The harness name used unless overridden.
    pub default: String,

    /// Every configured harness, keyed by name.
    #[serde(flatten)]
    pub harnesses: HashMap<String, HarnessConfig>,
}

/// Global spawn limits (§11.1, §12).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// Spawn cap across *all* registered projects on this machine
    /// (attached agents count too, §11.1).
    pub max_concurrent_agents: u32,
}

/// Work-claim heartbeat/reclaim tuning (§8.2).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClaimConfig {
    /// Stale-claim reclaim window; must comfortably exceed the longest
    /// expected phase.
    #[serde(with = "humantime_serde")]
    pub heartbeat_ttl: Duration,

    /// How often the owning instance refreshes its
    /// `owner:<instance>@<epoch>` claim label.
    #[serde(with = "humantime_serde")]
    pub heartbeat_interval: Duration,
}

/// The machine-global daemon config: `~/.config/spec-flow/config.yaml`
/// (§11.1).
///
/// One per daemon (one per machine, §2.5), never committed to any
/// project's repository. Holds daemon-wide settings plus the **project
/// registry** (`projects`) — every repo this installation manages.
/// `registry.rs` provides the idempotent add/list operations over that
/// list; this type only defines the shape and the file's load/save.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// Loopback bind address for the MCP-over-HTTP/SSE surface (§2.5).
    pub listen: SocketAddr,

    /// This machine's daemon id, stamped into
    /// `owner:<instance_id>@<epoch>` claim labels (§8.2).
    ///
    /// Auto-generated and persisted on first `spec-flow serve` when
    /// unset; that generation is `spec-flow serve`'s job (a later task),
    /// not this module's — [`load_global_config`] leaves it as `None`
    /// verbatim.
    #[serde(default)]
    pub instance_id: Option<String>,

    /// Local `git`/`gh` binary locations (§8.5).
    #[serde(default)]
    pub binaries: Binaries,

    /// The harness registry (§2.6).
    pub harnesses: HarnessesConfig,

    /// Global spawn limits (§12).
    pub limits: Limits,

    /// Work-claim heartbeat/reclaim tuning (§8.2).
    pub claim: ClaimConfig,

    /// Default per-phase kill/reclaim deadline (§7.1); must stay shorter
    /// than `claim.heartbeat_ttl`.
    #[serde(with = "humantime_serde")]
    pub phase_timeout: Duration,

    /// The project registry (§2.5, §4.2): every project under this
    /// daemon's domain. Pointers only — see [`ProjectPointer`].
    #[serde(default)]
    pub projects: Vec<ProjectPointer>,
}

/// Per-project `gh` scoping (§8.5): which host/account a project's `gh`
/// calls resolve against, when `gh` itself is configured for several
/// repos or hosts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GhConfig {
    /// The GitHub host (e.g. `github.com`, or an Enterprise host).
    pub host: String,

    /// The `gh`-authenticated account to use for this project.
    pub account: String,
}

/// How a project's merges are serialized (§8.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    /// GitHub's native merge queue is enabled on the default branch;
    /// the server only gates, enqueues, and observes.
    Native,

    /// The native queue is off; the server degrades to its own
    /// single-holder cross-instance `merge` lease (never a hard-fail).
    Serialized,
}

/// A single registered project's own settings:
/// `<project_dir>/config.yaml` (§11.1).
///
/// Written by `spec-flow init` (a later task, §14 step 9) run inside
/// that repo. Machine-local and re-creatable — losing it costs only a
/// re-`init`, never any work (§2.3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// The project handle + the coordinator's session scope (§2.5);
    /// defaults to the project directory's name at `init`.
    pub name: String,

    /// The GitHub repo slug (`owner/repo`), captured at `init` from
    /// inside the repo, where `gh`'s own resolution is unambiguous
    /// (§8.5). Every subsequent `gh` call for this project is scoped
    /// with `gh --repo <repo>`, never left to ambient resolution.
    pub repo: String,

    /// The project's container directory; every branch checkout is a
    /// sibling inside it (§10.1).
    pub project_dir: PathBuf,

    /// The primary checkout: `<project_dir>/<default_branch>`, itself a
    /// sibling — never a parent of the issue worktrees (§10.1).
    pub local_path: PathBuf,

    /// The repo's default branch.
    pub default_branch: String,

    /// Native merge queue, or the server's serialized fallback (§8.1).
    pub merge_mode: MergeMode,

    /// Per-project `gh` host/account scoping, when `gh` is multi-repo
    /// (§8.5). Absent when the default `gh` context is unambiguous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh: Option<GhConfig>,

    /// Optional per-project harness override; falls back to
    /// `harnesses.default` from the global config when unset (§2.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,

    /// The shared context index location for this repo (§10.3).
    pub index_path: PathBuf,

    /// The shared memory store id for this repo (§10.3).
    pub memory_scope: String,
}

/// The path to the machine-global daemon config,
/// `~/.config/spec-flow/config.yaml` (§11.1).
///
/// # Errors
///
/// Returns [`ConfigError::HomeDirUnavailable`] when `HOME` is unset.
pub fn global_config_path() -> Result<PathBuf, ConfigError> {
    let home =
        std::env::var_os("HOME").ok_or(ConfigError::HomeDirUnavailable)?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("spec-flow")
        .join("config.yaml"))
}

/// The path to a registered project's own config file,
/// `<project_dir>/config.yaml` (§11.1).
pub fn project_config_path(project_dir: &Path) -> PathBuf {
    project_dir.join("config.yaml")
}

/// Load the global daemon config from `path`.
///
/// # Errors
///
/// [`ConfigError::Read`] if the file can't be read, [`ConfigError::Parse`]
/// if its contents aren't a valid [`GlobalConfig`].
pub fn load_global_config(path: &Path) -> Result<GlobalConfig, ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| {
        ConfigError::Read { path: path.to_path_buf(), source }
    })?;
    serde_yaml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Write the global daemon config to `path`, creating parent
/// directories as needed.
///
/// # Errors
///
/// [`ConfigError::Serialize`] if `config` can't be rendered as YAML,
/// [`ConfigError::Write`] if the file (or its parent directories) can't
/// be written.
pub fn save_global_config(
    path: &Path,
    config: &GlobalConfig,
) -> Result<(), ConfigError> {
    let text = serde_yaml::to_string(config).map_err(|source| {
        ConfigError::Serialize { path: path.to_path_buf(), source }
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, text).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// Load a single project's config from `path`.
///
/// # Errors
///
/// [`ConfigError::Read`] if the file can't be read, [`ConfigError::Parse`]
/// if its contents aren't a valid [`ProjectConfig`].
pub fn load_project_config(path: &Path) -> Result<ProjectConfig, ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| {
        ConfigError::Read { path: path.to_path_buf(), source }
    })?;
    serde_yaml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Write a single project's config to `path`, creating parent
/// directories as needed.
///
/// # Errors
///
/// [`ConfigError::Serialize`] if `config` can't be rendered as YAML,
/// [`ConfigError::Write`] if the file (or its parent directories) can't
/// be written.
pub fn save_project_config(
    path: &Path,
    config: &ProjectConfig,
) -> Result<(), ConfigError> {
    let text = serde_yaml::to_string(config).map_err(|source| {
        ConfigError::Serialize { path: path.to_path_buf(), source }
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, text).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_global_config() -> GlobalConfig {
        GlobalConfig {
            listen: "127.0.0.1:7420".parse().unwrap(),
            instance_id: Some("spec-flow-a3f".to_string()),
            binaries: Binaries::default(),
            harnesses: HarnessesConfig {
                default: "claude".to_string(),
                harnesses: HashMap::from([(
                    "claude".to_string(),
                    HarnessConfig {
                        command: vec![
                            "claude".to_string(),
                            "-p".to_string(),
                            "{prompt}".to_string(),
                        ],
                    },
                )]),
            },
            limits: Limits { max_concurrent_agents: 3 },
            claim: ClaimConfig {
                heartbeat_ttl: Duration::from_secs(3600),
                heartbeat_interval: Duration::from_secs(300),
            },
            phase_timeout: Duration::from_secs(45 * 60),
            projects: vec![ProjectPointer {
                name: "repo-a".to_string(),
                project_dir: PathBuf::from("/abs/path/to/repo-a"),
            }],
        }
    }

    fn sample_project_config() -> ProjectConfig {
        ProjectConfig {
            name: "repo-a".to_string(),
            repo: "owner/repo-a".to_string(),
            project_dir: PathBuf::from("/abs/path/to/repo-a"),
            local_path: PathBuf::from("/abs/path/to/repo-a/main"),
            default_branch: "main".to_string(),
            merge_mode: MergeMode::Native,
            gh: Some(GhConfig {
                host: "github.com".to_string(),
                account: "me".to_string(),
            }),
            harness: Some("codex".to_string()),
            index_path: PathBuf::from("/abs/path/to/repo-a/index"),
            memory_scope: "owner-repo-a".to_string(),
        }
    }

    #[test]
    fn global_config_round_trips_through_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let config = sample_global_config();

        save_global_config(&path, &config).unwrap();
        let loaded = load_global_config(&path).unwrap();

        assert_eq!(loaded, config);
    }

    #[test]
    fn project_config_round_trips_through_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let config = sample_project_config();

        save_project_config(&path, &config).unwrap();
        let loaded = load_project_config(&path).unwrap();

        assert_eq!(loaded, config);
    }

    #[test]
    fn project_config_path_is_project_dir_slash_config_yaml() {
        let project_dir = Path::new("/abs/path/to/repo-a");
        assert_eq!(
            project_config_path(project_dir),
            PathBuf::from("/abs/path/to/repo-a/config.yaml")
        );
    }
}
