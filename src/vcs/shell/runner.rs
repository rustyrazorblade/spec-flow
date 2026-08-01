//! The subprocess seam underneath [`ShellVcs`](super::ShellVcs).
//!
//! [`ShellVcs`](super::ShellVcs) has two jobs that change for different
//! reasons: deciding *which* `git`/`gh` command line expresses an
//! operation (and how to parse its output), and *actually* forking a
//! process to run it. This module owns only the second one, behind the
//! [`CommandRunner`] trait, so the first is unit-testable without a
//! repository, a network, or an authenticated `gh`.
//!
//! The trait deliberately knows nothing about `git` or `gh` — it runs a
//! program with arguments in a directory and hands back what the process
//! printed. Every VCS concept stays in [`super`].

use std::path::Path;
use std::process::Command;

use crate::vcs::VcsError;

/// What one finished subprocess printed and how it exited.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandOutput {
    /// The process exit code, or `-1` when it was terminated by a signal
    /// and so has no code of its own.
    pub(crate) status: i32,

    /// Everything the process wrote to stdout, decoded lossily.
    pub(crate) stdout: String,

    /// Everything the process wrote to stderr, decoded lossily.
    pub(crate) stderr: String,
}

impl CommandOutput {
    /// Whether the process exited zero.
    pub(crate) fn is_success(&self) -> bool {
        self.status == 0
    }
}

/// Runs an external program and captures its output.
///
/// Implementors do exactly one thing: spawn, wait, capture. A `Result`
/// here reports that the process could not be *run* at all
/// ([`VcsError::BinaryNotFound`], [`VcsError::Spawn`]); a process that
/// ran and exited non-zero is a successful [`CommandOutput`] with a
/// non-zero [`status`](CommandOutput::status), because only the caller
/// knows whether that is a failure (a missing merge queue is not, §8.1).
pub(crate) trait CommandRunner: std::fmt::Debug + Send + Sync {
    /// Run `program` with `args`, optionally from the working directory
    /// `cwd`, and capture its output.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<CommandOutput, VcsError>;
}

/// Render `program` and `args` as a command line, for error messages.
///
/// Diagnostics only — this is not shell-quoted and must never be fed
/// back to a shell.
pub(crate) fn command_line(program: &str, args: &[&str]) -> String {
    let mut line = String::from(program);
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// The production [`CommandRunner`]: a real `std::process::Command`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<CommandOutput, VcsError> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }

        let output = command.output().map_err(|source| {
            // A missing binary is a setup problem the operator fixes at
            // `init` (a wrong `binaries.git`/`binaries.gh`, §8.5); any
            // other spawn failure is an environment fault worth keeping
            // the underlying io::Error for.
            if source.kind() == std::io::ErrorKind::NotFound {
                VcsError::BinaryNotFound { binary: program.to_string() }
            } else {
                VcsError::Spawn {
                    command: command_line(program, args),
                    source,
                }
            }
        })?;

        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// One [`CommandRunner::run`] call as recorded by [`FakeRunner`].
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecordedCall {
    /// The program that would have been executed.
    pub(crate) program: String,

    /// The argument vector, verbatim.
    pub(crate) args: Vec<String>,

    /// The working directory the call would have run in.
    pub(crate) cwd: Option<std::path::PathBuf>,
}

/// A [`CommandRunner`] that replays canned output and records every
/// call, so a test can assert on the exact argv built.
///
/// Queue one response per expected call with [`FakeRunner::succeed`] /
/// [`FakeRunner::fail`], in call order.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FakeRunner {
    responses: std::sync::Mutex<std::collections::VecDeque<CommandOutput>>,
    calls: std::sync::Mutex<Vec<RecordedCall>>,
}

#[cfg(test)]
impl FakeRunner {
    /// Create a runner with no queued responses.
    pub(crate) fn new() -> FakeRunner {
        FakeRunner::default()
    }

    /// Queue a zero-exit response printing `stdout`.
    pub(crate) fn succeed(&self, stdout: &str) -> &FakeRunner {
        self.push(CommandOutput {
            status: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    /// Queue a response exiting `status` with `stderr`.
    pub(crate) fn fail(&self, status: i32, stderr: &str) -> &FakeRunner {
        self.push(CommandOutput {
            status,
            stdout: String::new(),
            stderr: stderr.to_string(),
        })
    }

    /// Every call made so far, in call order.
    ///
    /// # Panics
    ///
    /// Panics if a previous call panicked while holding the lock.
    pub(crate) fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    fn push(&self, output: CommandOutput) -> &FakeRunner {
        self.responses.lock().unwrap().push_back(output);
        self
    }
}

#[cfg(test)]
impl CommandRunner for FakeRunner {
    /// # Panics
    ///
    /// Panics when the code under test makes more calls than the test
    /// queued responses for — an unexpected extra subprocess is a test
    /// failure, not something to paper over with a default response.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<CommandOutput, VcsError> {
        self.calls.lock().unwrap().push(RecordedCall {
            program: program.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            cwd: cwd.map(Path::to_path_buf),
        });

        let output =
            self.responses.lock().unwrap().pop_front().unwrap_or_else(|| {
                panic!(
                    "FakeRunner has no queued response for `{}`",
                    command_line(program, args)
                )
            });

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_joins_the_program_and_its_arguments() {
        assert_eq!(
            command_line("gh", &["issue", "view", "42"]),
            "gh issue view 42"
        );
    }

    #[test]
    fn process_runner_captures_stdout_and_a_zero_exit() {
        let runner = ProcessRunner;

        let output = runner.run("sh", &["-c", "printf hello"], None).unwrap();

        assert!(output.is_success());
        assert_eq!(output.stdout, "hello");
    }

    #[test]
    fn process_runner_reports_a_non_zero_exit_as_output_not_an_error() {
        let runner = ProcessRunner;

        let output = runner
            .run("sh", &["-c", "printf oops >&2; exit 3"], None)
            .unwrap();

        assert_eq!(output.status, 3);
        assert_eq!(output.stderr, "oops");
    }

    #[test]
    fn process_runner_runs_in_the_requested_directory() {
        let dir = tempfile::tempdir().unwrap();
        let runner = ProcessRunner;

        let output = runner
            .run("sh", &["-c", "printf %s .; ls"], Some(dir.path()))
            .unwrap();

        assert!(output.is_success());
    }

    #[test]
    fn process_runner_reports_a_missing_binary() {
        let runner = ProcessRunner;

        let error = runner
            .run("definitely-not-a-real-binary", &["--version"], None)
            .unwrap_err();

        assert!(matches!(error, VcsError::BinaryNotFound { .. }));
    }
}
