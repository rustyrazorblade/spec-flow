//! The `claude` process spawner + `LocalProcess` map (§2.6, §4.2, §5).
//!
//! When a phase needs an agent, the daemon spawns a fresh headless
//! process via a configured harness launch template
//! ([`crate::config::HarnessConfig`]), passing that phase's composed
//! instruction as its prompt. This module is that spawn/track/reap
//! layer:
//!
//! - [`template`] — pure `{prompt}`/`{spawn_token}`/`{worktree}`
//!   placeholder substitution over a harness's command template. No
//!   subprocess involved; fully unit-testable on its own.
//! - [`SpawnToken`] — the one-time correlation token minted at spawn
//!   (§2.6). "A correlation handle, not auth" — practically unique per
//!   spawn is the whole requirement, not cryptographic hardness.
//! - [`SpawnKey`] — `(project, issue_number, phase, sub_id)`, the
//!   `LocalProcess` map's key (§4.2). A minimal stand-in for the real
//!   domain type; the phase engine (a later task) has no `Project`/
//!   `Issue`/`Phase` types yet for this to borrow from.
//! - [`ProcessSpawner`] — the live map itself: spawns a harness process
//!   into a worktree, refuses a second spawn for a key that already has
//!   a live entry (the *same-instance* double-spawn guard, §7.3), and
//!   non-blockingly reaps processes that have exited.
//!
//! # What this step does *not* do
//!
//! No MCP server exists yet (§14 step 3 is standalone), so there is no
//! `submit_artifact` call to detect completion from — process exit
//! ([`ProcessSpawner::reap_finished`]) is the only completion signal
//! available here. Wiring the correlation token into a real MCP
//! `register` call, killing a phase on `cancel_work` (§6), and per-phase
//! timeouts (§7.1) are later tasks built on top of this map.
//!
//! # Why a concrete struct, not a trait (§5, style guide §7)
//!
//! `vcs/` splits into a trait plus a real/fake pair because the phase
//! engine and `init` need to run against a stub in their own unit
//! tests. Nothing in this crate calls the spawner yet, so there is no
//! second implementation to abstract over and no caller to stub it for;
//! [`ProcessSpawner`] is tested directly against real short-lived
//! subprocesses instead (`tests/shell_vcs_git.rs` already establishes
//! that pattern for `git`). Introduce a trait seam if and when a caller
//! needs to stub it.

mod template;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use tracing::warn;

use crate::config::HarnessConfig;
use template::interpolate_command;

/// Errors from spawning or tracking a phase process.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpawnError {
    /// `key` already has a live, unreaped process tracked — the
    /// same-instance double-spawn guard (§4.2, §7.3). Reap it (it has
    /// exited) or let it finish before spawning again.
    #[error("a phase process is already running for {key:?}")]
    AlreadyRunning {
        /// The key that already has a live entry.
        key: SpawnKey,
    },

    /// The harness's command template (`HarnessConfig::command`) has no
    /// argv at all, so there is no program to spawn.
    #[error("harness command template is empty")]
    EmptyCommand,

    /// The OS could not launch the harness binary (e.g. it does not
    /// exist or is not executable) — distinct from the process running
    /// and exiting non-zero, which [`ProcessSpawner::reap_finished`]
    /// reports instead.
    #[error("failed to spawn `{program}`")]
    Spawn {
        /// The program the spawner tried to execute.
        program: String,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

/// A one-time correlation token minted at spawn (§2.6).
///
/// Injected into the spawned process (env or prompt — a later task's
/// concern once the MCP server exists) and presented back on
/// `register`, so the daemon can bind an incoming MCP connection to the
/// `(project, issue, phase, sub_id)` it belongs to. This is a
/// correlation handle, not auth: it only needs to be practically unique
/// per spawn, not cryptographically hardened (§2.6).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SpawnToken(String);

/// Monotonic disambiguator for [`SpawnToken::generate`], so two tokens
/// minted within the same nanosecond-resolution tick (possible on
/// platforms with a coarser clock) still differ.
static SPAWN_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

impl SpawnToken {
    /// Mint a fresh, practically-unique token.
    ///
    /// Combines the process id, a wall-clock timestamp, and a
    /// process-wide monotonic counter — enough to make collisions
    /// vanishingly unlikely for this daemon's purpose (correlating a
    /// spawn with its later `register` call) without pulling in a
    /// dependency for randomness this crate does not otherwise need.
    pub fn generate() -> SpawnToken {
        let counter = SPAWN_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        SpawnToken(format!("{pid:x}-{nanos:x}-{counter:x}"))
    }

    /// This token's opaque string form, as interpolated into a spawned
    /// process's command line or environment.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The identity of one phase-process spawn, unique within this running
/// instance (§4.2 `LocalProcess.key`).
///
/// `project` distinguishes repos — one daemon serves many, and issue
/// numbers are per-repo (`repo-a#42 != repo-b#42`, §5); `sub_id`
/// distinguishes the parallel review-lens sub-phases (§7.2 ph.6) that
/// would otherwise collide on `(project, issue_number, phase)` alone.
///
/// This is a minimal stand-in for the real domain type: this crate has
/// no `Project`/`Issue`/`Phase` types yet (the phase engine is a later
/// task, §14 step 7) for a key to borrow from. Expect it to be refined
/// once those exist.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SpawnKey {
    /// The project (repo) this spawn belongs to.
    pub project: String,
    /// The issue this spawn is working, scoped to `project`.
    pub issue_number: u64,
    /// The workflow phase id being run (e.g. `"architecture"`).
    pub phase: String,
    /// Disambiguates concurrent sub-phases of one phase (e.g. which
    /// review lens), when a phase spawns more than one process at once.
    pub sub_id: Option<String>,
}

/// One tracked, in-memory phase-process spawn (§4.2 `LocalProcess`).
///
/// Ephemeral, per-instance state only — never persisted (§5). Held by
/// [`ProcessSpawner`], keyed by [`SpawnKey`]; a caller reads it back
/// through [`ProcessSpawner::get`].
#[derive(Debug)]
pub struct LocalProcessEntry {
    /// The correlation token minted for this spawn (§2.6).
    pub spawn_token: SpawnToken,
    /// The worktree this process was spawned into (its `cwd`).
    pub worktree_cwd: PathBuf,
    /// When this process was spawned.
    pub spawned_at: SystemTime,
    /// The live child handle. Private: callers observe it only through
    /// [`ProcessSpawner::reap_finished`], which is the sole place
    /// allowed to `wait` on it, so the map's bookkeeping and the
    /// process's actual state can never drift apart.
    child: Child,
}

/// The daemon's live `LocalProcess` map for this instance (§4.2, §5).
///
/// Tracks every phase process this instance has spawned and not yet
/// reaped, keyed by [`SpawnKey`]. This is what makes the *same-instance*
/// double-spawn guarantee real: [`try_spawn`](ProcessSpawner::try_spawn)
/// refuses a second spawn for a key that still has a live entry.
/// Cross-instance overlap is a separate concern the GitHub ownership
/// claim prevents (§7.3, §8.2); this map says nothing about other
/// instances.
///
/// Never persisted — losing this map costs nothing durable (§5): a
/// crashed instance's claim goes stale and another instance re-spawns
/// the phase.
#[derive(Debug, Default)]
pub struct ProcessSpawner {
    processes: HashMap<SpawnKey, LocalProcessEntry>,
}

impl ProcessSpawner {
    /// An empty spawner tracking nothing.
    pub fn new() -> ProcessSpawner {
        ProcessSpawner::default()
    }

    /// Spawn `harness` for `key`, in `worktree`, with `prompt` as its
    /// composed instruction (§2.6).
    ///
    /// Mints a fresh [`SpawnToken`], substitutes it and `prompt`/
    /// `worktree` into the harness's command template (see
    /// `template::interpolate_command`), and launches the result as a
    /// subprocess whose working directory is `worktree` — so the
    /// process is already sitting in the right project/branch context
    /// (§2.6). Returns immediately without waiting for the process to
    /// finish; call [`reap_finished`](ProcessSpawner::reap_finished) to
    /// learn when it does.
    ///
    /// # Errors
    ///
    /// [`SpawnError::AlreadyRunning`] if `key` already has a live,
    /// unreaped entry — the same-instance double-spawn guard (§4.2,
    /// §7.3). [`SpawnError::EmptyCommand`] if `harness.command` is
    /// empty. [`SpawnError::Spawn`] if the OS could not launch the
    /// harness binary.
    ///
    /// The double-spawn guard's check-then-insert is atomic only because
    /// `&mut self` gives this method exclusive access to the map for its
    /// whole call — true today, since nothing in this crate calls it
    /// concurrently yet. A future caller (the phase engine, §14 step
    /// 6+) that shares one `ProcessSpawner` across concurrent scheduling
    /// ticks must serialize calls to it (e.g. behind a single-threaded
    /// executor or a mutex held for the whole call), not just around the
    /// map access, or this guard's check-then-insert becomes a TOCTOU
    /// race.
    pub fn try_spawn(
        &mut self,
        key: SpawnKey,
        harness: &HarnessConfig,
        prompt: &str,
        worktree: &Path,
    ) -> Result<SpawnToken, SpawnError> {
        if self.processes.contains_key(&key) {
            return Err(SpawnError::AlreadyRunning { key });
        }

        let spawn_token = SpawnToken::generate();
        let argv =
            interpolate_command(harness, prompt, &spawn_token, worktree)?;
        // `interpolate_command` already rejects an empty template, so
        // `argv` is guaranteed non-empty here.
        let (program, args) =
            argv.split_first().ok_or(SpawnError::EmptyCommand)?;

        let child = Command::new(program)
            .args(args)
            .current_dir(worktree)
            .spawn()
            .map_err(|source| SpawnError::Spawn {
                program: program.clone(),
                source,
            })?;

        self.processes.insert(
            key,
            LocalProcessEntry {
                spawn_token: spawn_token.clone(),
                worktree_cwd: worktree.to_path_buf(),
                spawned_at: SystemTime::now(),
                child,
            },
        );

        Ok(spawn_token)
    }

    /// Non-blockingly poll every tracked process, removing and
    /// returning every one that has exited (§2.6, §7.1: "process exit
    /// as a backstop").
    ///
    /// Never blocks: each check is [`Child::try_wait`], so a
    /// still-running phase is left tracked and simply absent from the
    /// returned list. A process whose status could not be read (an
    /// unexpected OS error) is left tracked for a later reap to retry,
    /// rather than dropped and forgotten. Intended to run on the
    /// daemon's periodic reaping tick; leaves no zombie processes
    /// behind once a finished one has been reaped.
    pub fn reap_finished(&mut self) -> Vec<(SpawnKey, ExitStatus)> {
        let mut finished = Vec::new();
        self.processes.retain(|key, entry| match entry.child.try_wait() {
            Ok(Some(status)) => {
                finished.push((key.clone(), status));
                false
            }
            Ok(None) => true,
            Err(error) => {
                warn!(
                    key = ?key,
                    error = %error,
                    "failed to poll phase process status; leaving it \
                     tracked for a later reap"
                );
                true
            }
        });
        finished
    }

    /// The tracked entry for `key`, if it has a live, unreaped process.
    pub fn get(&self, key: &SpawnKey) -> Option<&LocalProcessEntry> {
        self.processes.get(key)
    }

    /// Whether `key` currently has a live, unreaped process tracked.
    pub fn is_running(&self, key: &SpawnKey) -> bool {
        self.processes.contains_key(key)
    }

    /// How many live, unreaped processes this instance is tracking.
    pub fn len(&self) -> usize {
        self.processes.len()
    }

    /// Whether no processes are currently tracked.
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    /// A harness that runs `sh -c <script>`, portable enough for the
    /// short-lived subprocess tests below (`tests/shell_vcs_git.rs`
    /// already relies on a real `git` being available in this dev
    /// environment; a real `sh` is an equally safe assumption).
    fn sh_harness(script: &str) -> HarnessConfig {
        HarnessConfig {
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                script.to_string(),
            ],
        }
    }

    fn key(sub_id: Option<&str>) -> SpawnKey {
        SpawnKey {
            project: "repo-a".to_string(),
            issue_number: 42,
            phase: "implement".to_string(),
            sub_id: sub_id.map(str::to_string),
        }
    }

    /// Poll `spawner` for up to a second until `reap_finished` reports
    /// something, so the tests don't race the child's own exit timing.
    fn reap_eventually(
        spawner: &mut ProcessSpawner,
    ) -> Vec<(SpawnKey, ExitStatus)> {
        for _ in 0..50 {
            let finished = spawner.reap_finished();
            if !finished.is_empty() {
                return finished;
            }
            sleep(Duration::from_millis(20));
        }
        Vec::new()
    }

    #[test]
    fn spawn_token_generate_produces_distinct_tokens() {
        let first = SpawnToken::generate();
        let second = SpawnToken::generate();

        assert_ne!(first, second);
    }

    #[test]
    fn try_spawn_tracks_a_live_entry_for_its_key() {
        let dir = TempDir::new().unwrap();
        let mut spawner = ProcessSpawner::new();
        let key = key(None);

        let token = spawner
            .try_spawn(
                key.clone(),
                &sh_harness("sleep 1"),
                "prompt",
                dir.path(),
            )
            .unwrap();

        assert!(spawner.is_running(&key));
        let entry = spawner.get(&key).unwrap();
        assert_eq!(entry.spawn_token, token);
        assert_eq!(entry.worktree_cwd, dir.path());
    }

    #[test]
    fn try_spawn_refuses_a_second_spawn_for_a_still_live_key() {
        let dir = TempDir::new().unwrap();
        let mut spawner = ProcessSpawner::new();
        let key = key(None);

        spawner
            .try_spawn(
                key.clone(),
                &sh_harness("sleep 1"),
                "prompt",
                dir.path(),
            )
            .unwrap();

        let error = spawner
            .try_spawn(
                key.clone(),
                &sh_harness("sleep 1"),
                "prompt",
                dir.path(),
            )
            .unwrap_err();

        assert!(matches!(error, SpawnError::AlreadyRunning { .. }));
        // Still exactly one process tracked — the second call must not
        // have spawned anything.
        assert_eq!(spawner.len(), 1);
    }

    #[test]
    fn distinct_sub_ids_are_independent_keys_and_may_both_spawn() {
        let dir = TempDir::new().unwrap();
        let mut spawner = ProcessSpawner::new();

        spawner
            .try_spawn(
                key(Some("spec-lens")),
                &sh_harness("sleep 1"),
                "prompt",
                dir.path(),
            )
            .unwrap();
        spawner
            .try_spawn(
                key(Some("security-lens")),
                &sh_harness("sleep 1"),
                "prompt",
                dir.path(),
            )
            .unwrap();

        assert_eq!(spawner.len(), 2);
    }

    #[test]
    fn reap_finished_reports_the_real_exit_status_and_stops_tracking() {
        let dir = TempDir::new().unwrap();
        let mut spawner = ProcessSpawner::new();
        let key = key(None);

        spawner
            .try_spawn(
                key.clone(),
                &sh_harness("sleep 0.05 && exit 3"),
                "prompt",
                dir.path(),
            )
            .unwrap();

        let finished = reap_eventually(&mut spawner);

        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].0, key);
        assert_eq!(finished[0].1.code(), Some(3));
        assert!(!spawner.is_running(&key));
        assert!(spawner.is_empty());
    }

    #[test]
    fn reap_finished_leaves_a_still_running_process_tracked() {
        let dir = TempDir::new().unwrap();
        let mut spawner = ProcessSpawner::new();
        let key = key(None);

        spawner
            .try_spawn(
                key.clone(),
                &sh_harness("sleep 1"),
                "prompt",
                dir.path(),
            )
            .unwrap();

        let finished = spawner.reap_finished();

        assert!(finished.is_empty());
        assert!(spawner.is_running(&key));
    }

    #[test]
    fn reaping_a_finished_process_allows_respawning_its_key() {
        let dir = TempDir::new().unwrap();
        let mut spawner = ProcessSpawner::new();
        let key = key(None);

        spawner
            .try_spawn(
                key.clone(),
                &sh_harness("exit 0"),
                "prompt",
                dir.path(),
            )
            .unwrap();
        reap_eventually(&mut spawner);

        // The key is free again: a second spawn for it must succeed.
        let result = spawner.try_spawn(
            key.clone(),
            &sh_harness("sleep 1"),
            "prompt",
            dir.path(),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn spawned_process_runs_with_its_working_directory_set_to_the_worktree() {
        let dir = TempDir::new().unwrap();
        let mut spawner = ProcessSpawner::new();

        spawner
            .try_spawn(
                key(None),
                // A relative path resolves against the process's `cwd`
                // only if that `cwd` was actually set to `worktree`.
                &sh_harness("touch marker"),
                "prompt",
                dir.path(),
            )
            .unwrap();

        for _ in 0..50 {
            if dir.path().join("marker").is_file() {
                break;
            }
            sleep(Duration::from_millis(20));
        }

        assert!(dir.path().join("marker").is_file());
    }

    #[test]
    fn try_spawn_reports_an_empty_harness_command_as_an_error() {
        let dir = TempDir::new().unwrap();
        let mut spawner = ProcessSpawner::new();

        let error = spawner
            .try_spawn(
                key(None),
                &HarnessConfig { command: Vec::new() },
                "prompt",
                dir.path(),
            )
            .unwrap_err();

        assert!(matches!(error, SpawnError::EmptyCommand));
        assert!(spawner.is_empty());
    }

    #[test]
    fn try_spawn_reports_a_missing_binary_as_a_spawn_error() {
        let dir = TempDir::new().unwrap();
        let mut spawner = ProcessSpawner::new();
        let harness = HarnessConfig {
            command: vec![
                "definitely-not-a-real-binary".to_string(),
                "{prompt}".to_string(),
            ],
        };

        let error = spawner
            .try_spawn(key(None), &harness, "prompt", dir.path())
            .unwrap_err();

        assert!(matches!(error, SpawnError::Spawn { .. }));
    }
}
