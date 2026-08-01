//! Project registry operations over [`GlobalConfig`]'s `projects` list
//! (§2.5, §4.2, §11.1).
//!
//! The registry is the daemon's one legitimately-local, non-GitHub state
//! (besides the per-project config files it points at): the list of
//! repos this installation manages. `spec-flow init` is the only writer in
//! normal operation, and it must be **idempotent** — re-running `init`
//! inside an already-registered repo re-syncs that project's pointer
//! rather than adding a duplicate row (§11.1). The functions here are
//! pure operations over an in-memory [`GlobalConfig`]; loading/saving the
//! file itself is `config.rs`'s job.

use crate::config::{GlobalConfig, ProjectPointer};

/// The result of [`add_project`], distinguishing a fresh registration
/// from a re-sync of an existing one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddOutcome {
    /// No project with this name was registered before; a new row was
    /// appended.
    Added,

    /// A project with this name was already registered; its
    /// `project_dir` was updated in place (a no-op when it already
    /// matched).
    Updated,
}

/// Register `pointer` in `config`'s project registry, keyed on
/// [`ProjectPointer::name`].
///
/// Idempotent per §11.1: re-registering a known project name updates its
/// `project_dir` in place instead of appending a duplicate row.
pub fn add_project(
    config: &mut GlobalConfig,
    pointer: ProjectPointer,
) -> AddOutcome {
    match config.projects.iter_mut().find(|p| p.name == pointer.name) {
        Some(existing) => {
            existing.project_dir = pointer.project_dir;
            AddOutcome::Updated
        }
        None => {
            config.projects.push(pointer);
            AddOutcome::Added
        }
    }
}

/// Remove the project named `name` from `config`'s registry.
///
/// Returns `true` if a project was removed, `false` if none matched.
pub fn remove_project(config: &mut GlobalConfig, name: &str) -> bool {
    let before = config.projects.len();
    config.projects.retain(|p| p.name != name);
    config.projects.len() != before
}

/// All registered projects, in registration order.
#[inline]
pub fn list_projects(config: &GlobalConfig) -> &[ProjectPointer] {
    &config.projects
}

/// Look up a registered project by name.
pub fn find_project<'a>(
    config: &'a GlobalConfig,
    name: &str,
) -> Option<&'a ProjectPointer> {
    config.projects.iter().find(|p| p.name == name)
}

/// Look up the registered project whose `project_dir` contains `path`
/// (i.e. `path` is the project dir itself, or a descendant of it — a
/// checkout or worktree inside it).
///
/// This is how a coordinator connection resolves its project from its
/// launch directory at `register` (§2.5, §6): the daemon needs the
/// project *containing* `cwd`, not an exact match, since `cwd` is
/// typically `<project_dir>/<branch>`, not `<project_dir>` itself.
pub fn find_project_containing<'a>(
    config: &'a GlobalConfig,
    path: &std::path::Path,
) -> Option<&'a ProjectPointer> {
    config.projects.iter().find(|p| path.starts_with(&p.project_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Binaries, ClaimConfig, HarnessesConfig, Limits};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    fn empty_config() -> GlobalConfig {
        GlobalConfig {
            listen: "127.0.0.1:7420".parse().unwrap(),
            instance_id: None,
            binaries: Binaries::default(),
            harnesses: HarnessesConfig {
                default: "claude".to_string(),
                harnesses: HashMap::new(),
            },
            limits: Limits { max_concurrent_agents: 3 },
            claim: ClaimConfig {
                heartbeat_ttl: Duration::from_secs(3600),
                heartbeat_interval: Duration::from_secs(300),
            },
            phase_timeout: Duration::from_secs(45 * 60),
            projects: Vec::new(),
        }
    }

    #[test]
    fn add_project_appends_when_new() {
        let mut config = empty_config();

        let outcome = add_project(
            &mut config,
            ProjectPointer {
                name: "repo-a".to_string(),
                project_dir: PathBuf::from("/abs/repo-a"),
            },
        );

        assert_eq!(outcome, AddOutcome::Added);
        assert_eq!(list_projects(&config).len(), 1);
    }

    #[test]
    fn add_project_is_idempotent_on_name() {
        let mut config = empty_config();
        add_project(
            &mut config,
            ProjectPointer {
                name: "repo-a".to_string(),
                project_dir: PathBuf::from("/abs/repo-a"),
            },
        );

        let outcome = add_project(
            &mut config,
            ProjectPointer {
                name: "repo-a".to_string(),
                project_dir: PathBuf::from("/abs/repo-a-moved"),
            },
        );

        assert_eq!(outcome, AddOutcome::Updated);
        assert_eq!(list_projects(&config).len(), 1);
        assert_eq!(
            find_project(&config, "repo-a").unwrap().project_dir,
            PathBuf::from("/abs/repo-a-moved")
        );
    }

    #[test]
    fn remove_project_drops_matching_row() {
        let mut config = empty_config();
        add_project(
            &mut config,
            ProjectPointer {
                name: "repo-a".to_string(),
                project_dir: PathBuf::from("/abs/repo-a"),
            },
        );

        assert!(remove_project(&mut config, "repo-a"));
        assert!(list_projects(&config).is_empty());
        assert!(!remove_project(&mut config, "repo-a"));
    }

    #[test]
    fn find_project_containing_matches_a_descendant_path() {
        let mut config = empty_config();
        add_project(
            &mut config,
            ProjectPointer {
                name: "repo-a".to_string(),
                project_dir: PathBuf::from("/abs/repo-a"),
            },
        );

        let found = find_project_containing(
            &config,
            std::path::Path::new("/abs/repo-a/issue-42-widget"),
        );

        assert_eq!(found.map(|p| p.name.as_str()), Some("repo-a"));
    }
}
