//! Command-template placeholder interpolation (§2.6, §11.1).
//!
//! A harness's [`HarnessConfig::command`] carries `{prompt}` /
//! `{spawn_token}` / `{worktree}` placeholders in its argument strings
//! (e.g. `["claude", "-p", "{prompt}"]`). [`interpolate_command`]
//! substitutes them with the given values, per argument, returning the
//! concrete argv the spawner executes. This is pure string substitution
//! over the template — no subprocess, no filesystem, no I/O — so it is
//! fully unit-testable on its own, unlike the spawn step itself.

use std::path::Path;

use crate::config::HarnessConfig;

use super::{SpawnError, SpawnToken};

/// Substitute `{prompt}` / `{spawn_token}` / `{worktree}` in every
/// argument of `harness`'s command template, returning the concrete
/// argv to execute (§2.6).
///
/// Each placeholder may appear zero, one, or several times within a
/// single argument; every occurrence is replaced, in one left-to-right
/// pass per argument (see [`substitute_once`] — chained `str::replace`
/// calls would re-scan text already substituted in for an earlier
/// placeholder, which is unsafe here: `prompt` is composed phase
/// instruction text, e.g. §9's quoted spec/style-guide excerpts, and can
/// legitimately contain the literal substring `{worktree}` or
/// `{spawn_token}`). `worktree` is rendered with
/// [`Path::to_string_lossy`] — a worktree path is always one this daemon
/// constructed under `<project_dir>/<branch>` (§10.1), never arbitrary
/// user input, so lossy rendering is an acceptable tradeoff for a
/// simple, allocation-light substitution.
///
/// # Errors
///
/// [`SpawnError::EmptyCommand`] when `harness.command` is empty — there
/// is no program to spawn.
pub(super) fn interpolate_command(
    harness: &HarnessConfig,
    prompt: &str,
    spawn_token: &SpawnToken,
    worktree: &Path,
) -> Result<Vec<String>, SpawnError> {
    if harness.command.is_empty() {
        return Err(SpawnError::EmptyCommand);
    }

    let worktree_str = worktree.to_string_lossy();
    let placeholders: [(&str, &str); 3] = [
        ("{prompt}", prompt),
        ("{spawn_token}", spawn_token.as_str()),
        ("{worktree}", &worktree_str),
    ];
    Ok(harness
        .command
        .iter()
        .map(|arg| substitute_once(arg, &placeholders))
        .collect())
}

/// Replace every occurrence of any `placeholders` key with its paired
/// value, scanning `arg` left to right exactly once.
///
/// Text already copied into the output — including a value just
/// substituted in for an earlier placeholder — is never re-scanned, so
/// a `value` that happens to contain another placeholder's literal text
/// (e.g. a `prompt` quoting `{worktree}`) passes through unchanged
/// rather than being substituted again.
fn substitute_once(arg: &str, placeholders: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(arg.len());
    let mut rest = arg;
    'outer: while !rest.is_empty() {
        for (needle, value) in placeholders {
            if let Some(tail) = rest.strip_prefix(needle) {
                out.push_str(value);
                rest = tail;
                continue 'outer;
            }
        }
        // Advance by one `char`, not one byte, to stay UTF-8-safe.
        let mut chars = rest.chars();
        let next = chars.next().expect("rest is non-empty per the loop guard");
        out.push(next);
        rest = chars.as_str();
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A harness whose template is `command`.
    fn harness(command: &[&str]) -> HarnessConfig {
        HarnessConfig {
            command: command.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn substitutes_every_placeholder_in_every_argument() {
        let harness = harness(&["claude", "-p", "{prompt}"]);
        let token = SpawnToken::generate();

        let argv = interpolate_command(
            &harness,
            "do the thing",
            &token,
            Path::new("/repo-a/issue-42-widget"),
        )
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "-p".to_string(),
                "do the thing".to_string(),
            ]
        );
    }

    #[test]
    fn substitutes_spawn_token_and_worktree_placeholders() {
        let harness = harness(&[
            "codex",
            "--cwd",
            "{worktree}",
            "--token",
            "{spawn_token}",
            "{prompt}",
        ]);
        let token = SpawnToken::generate();

        let argv = interpolate_command(
            &harness,
            "prompt text",
            &token,
            Path::new("/repo-a/issue-42-widget"),
        )
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "codex".to_string(),
                "--cwd".to_string(),
                "/repo-a/issue-42-widget".to_string(),
                "--token".to_string(),
                token.as_str().to_string(),
                "prompt text".to_string(),
            ]
        );
    }

    #[test]
    fn substitutes_a_placeholder_appearing_twice_in_one_argument() {
        let harness = harness(&["{prompt} / {prompt}"]);
        let token = SpawnToken::generate();

        let argv = interpolate_command(
            &harness,
            "echo",
            &token,
            Path::new("/repo-a/issue-1"),
        )
        .unwrap();

        assert_eq!(argv, vec!["echo / echo".to_string()]);
    }

    #[test]
    fn leaves_arguments_without_placeholders_untouched() {
        let harness = harness(&["claude", "--dangerously-skip-permissions"]);
        let token = SpawnToken::generate();

        let argv = interpolate_command(
            &harness,
            "prompt",
            &token,
            Path::new("/repo-a/issue-1"),
        )
        .unwrap();

        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]
        );
    }

    #[test]
    fn empty_command_template_is_an_error() {
        let harness = HarnessConfig { command: Vec::new() };
        let token = SpawnToken::generate();

        let error = interpolate_command(
            &harness,
            "prompt",
            &token,
            Path::new("/repo-a/issue-1"),
        )
        .unwrap_err();

        assert!(matches!(error, SpawnError::EmptyCommand));
    }

    #[test]
    fn a_placeholders_own_literal_text_in_another_values_substitution_is_not_re_substituted()
     {
        // A composed prompt that happens to quote the literal
        // placeholder syntax (e.g. documenting this very module) must
        // pass through untouched — it is `{prompt}`'s VALUE, substituted
        // in exactly once, not a placeholder to recurse into.
        let harness = harness(&["{prompt}", "{worktree}", "{spawn_token}"]);
        let token = SpawnToken::generate();
        let prompt =
            "quoting {worktree} and {spawn_token} and even {prompt} itself";

        let argv = interpolate_command(
            &harness,
            prompt,
            &token,
            Path::new("/repo-a/issue-1"),
        )
        .unwrap();

        assert_eq!(
            argv,
            vec![
                prompt.to_string(),
                "/repo-a/issue-1".to_string(),
                token.as_str().to_string(),
            ]
        );
    }

    #[test]
    fn worktree_path_is_rendered_as_a_plain_string() {
        let harness = harness(&["{worktree}"]);
        let token = SpawnToken::generate();
        let worktree = PathBuf::from("/abs/repo-a/issue-7-cache");

        let argv =
            interpolate_command(&harness, "p", &token, &worktree).unwrap();

        assert_eq!(argv, vec!["/abs/repo-a/issue-7-cache".to_string()]);
    }
}
