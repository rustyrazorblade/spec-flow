//! The instruction composer (§9, §14 step 6): assembling one phase's
//! final prompt from a compiled-in base template, an optional
//! project-local override file, and a set of variables to interpolate.
//!
//! This module implements the **mechanism** only — the pure logic
//! described by §9.2/§9.4 — not the wiring that will eventually call
//! it. There is no phase engine or MCP server yet (§14 step 7+), so
//! nothing in this crate invokes [`compose`] end to end; a later step
//! resolves each instruction point's base template text (currently
//! [`crate::scaffold`]'s compiled-in seed strings — see
//! `scaffold::INSTRUCTION_POINTS` and its per-point constants) and this
//! project's `.spec-flow/instructions/<point>.md` override, then feeds
//! both through [`compose`].
//!
//! Two functions, kept deliberately separate — the same "pure logic
//! apart from I/O" split [`crate::state::derive_issue_state`] and
//! [`crate::schedule::next_action`] use:
//!
//! - [`compose`] is **pure**: given an already-read base template
//!   string, an already-read optional override string, and a variable
//!   map, it returns the final prompt text. No filesystem access, so it
//!   is exhaustively unit-testable with no project checkout at all.
//! - [`read_instruction_override`] is the one function that touches
//!   disk: it reads `<repo>/.spec-flow/instructions/<point>.md` if
//!   present, returning `None` — not an error — when the file is
//!   absent, per §9.2 ("a deleted override file falls back to the
//!   built-in base... the composer never depends on files existing").
//!
//! # Interpolation variables are a generic map, not a fixed struct
//!
//! §9.2 names five variables today (`issue_number`, `worktree_path`,
//! `branch`, `specs_dir`, `default_branch`) and explicitly promises more
//! later (the recorded architecture decision) with no schema yet
//! defined for it. [`compose`]/[`interpolate`] take a
//! `HashMap<String, String>` rather than a struct with five named
//! fields precisely so that a later variable — the architecture
//! decision, or a per-project custom one — is just a new map entry, not
//! an API break. The five known names are still given canonical
//! constants ([`VAR_ISSUE_NUMBER`] and friends) so call sites don't
//! respell them.
//!
//! # Unmatched placeholders are left as literal text
//!
//! A `{placeholder}` with no corresponding entry in the variable map is
//! copied into the output byte-for-byte rather than being dropped or
//! treated as an error. Two reasons: a project's override file may
//! reference a variable this crate doesn't populate yet (the
//! architecture-decision variable §9.2 promises has no producer in this
//! crate today — see the module map in `lib.rs`), and the bundled style
//! guides (§9.3) are plain markdown that may itself contain brace text
//! having nothing to do with this interpolation syntax. Erroring on
//! either would make otherwise-valid text unusable.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Errors resolving a project's instruction override file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstructionError {
    /// The override file exists but could not be read.
    ///
    /// This is anything other than "the file is absent" — see
    /// [`read_instruction_override`]'s docs for why a missing file is
    /// `Ok(None)` rather than this variant.
    #[error("failed to read instruction override at {path}")]
    Read {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

/// The literal first-line directive that switches a project override
/// from the default `append` mode to `replace` (§9.2).
///
/// `pub(crate)` so `scaffold.rs` can seed a freshly-materialized
/// override file with this directive already in place — see that
/// module's `instruction_file_seed` for why an unedited seed file must
/// start in `replace` mode rather than the `append` default.
pub(crate) const REPLACE_DIRECTIVE: &str = "<!-- mode: replace -->";

/// The interpolation variable name for the issue number (§9.2).
pub const VAR_ISSUE_NUMBER: &str = "issue_number";

/// The interpolation variable name for the worktree's absolute path
/// (§9.2).
pub const VAR_WORKTREE_PATH: &str = "worktree_path";

/// The interpolation variable name for the issue's branch (§9.2).
pub const VAR_BRANCH: &str = "branch";

/// The interpolation variable name for the spec tool's specs directory
/// (§9.2).
pub const VAR_SPECS_DIR: &str = "specs_dir";

/// The interpolation variable name for the project's default branch
/// (§9.2).
pub const VAR_DEFAULT_BRANCH: &str = "default_branch";

/// How a project's override text combines with the base template
/// (§9.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    /// Override text is appended after the interpolated base template.
    /// The default when no mode directive is present.
    Append,
    /// Override text supersedes the base template entirely.
    Replace,
}

/// Compose one phase's final prompt text from a base template, an
/// optional project override, and a set of interpolation variables
/// (§9.2).
///
/// `override_text` is `None` when the project has no
/// `.spec-flow/instructions/<point>.md` file — see
/// [`read_instruction_override`], the function that produces this
/// argument in practice. When it is `Some`, its first line is checked
/// against the literal directive `<!-- mode: replace -->` (surrounding
/// whitespace on that line, including a trailing `\r` from a
/// CRLF-saved file, is ignored): if it matches, the override text after
/// that line supersedes the base template entirely; otherwise the
/// override text — directive line included, since it wasn't one — is
/// appended after the base, separated by a blank line.
///
/// `base` and the override text (whichever of it is used) are
/// interpolated **independently, before being combined** — never by
/// concatenating first and interpolating the joined result once. This
/// matters for the same reason [`interpolate`]'s single-pass scan
/// matters (mirroring `spawner::template`'s `substitute_once`): it also
/// prevents a second, cross-boundary bug — a base template ending in
/// `{` and an override beginning with `name}` must not accidentally
/// combine into the placeholder `{name}` and get substituted, since
/// neither string contained that placeholder on its own.
pub fn compose(
    base: &str,
    override_text: Option<&str>,
    variables: &HashMap<String, String>,
) -> String {
    let Some(override_text) = override_text else {
        return interpolate(base, variables);
    };

    match split_mode(override_text) {
        (Mode::Replace, body) => interpolate(body, variables),
        (Mode::Append, body) => {
            let base = interpolate(base, variables);
            let addition = interpolate(body, variables);
            format!("{}\n\n{}", base.trim_end(), addition.trim_end())
        }
    }
}

/// Split `text` into its mode and the text to interpolate: the whole
/// input for [`Mode::Append`], or everything after the first line for
/// [`Mode::Replace`].
///
/// A leading UTF-8 BOM (`\u{feff}`) is stripped before the first line
/// is compared against the directive. `str::trim` does not remove it
/// (it is not Unicode whitespace), and it is a realistic artifact of
/// how some editors and shells save a "UTF-8" file (e.g. `Out-File` on
/// Windows PowerShell, older Windows Notepad) — without stripping it, a
/// byte-exact `<!-- mode: replace -->` first line would silently fail
/// to match and fall through to `append`, doubling the composed prompt
/// exactly the way an unstripped `\r` would.
fn split_mode(text: &str) -> (Mode, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (first_line, rest) = match text.split_once('\n') {
        Some((first_line, rest)) => (first_line, rest),
        None => (text, ""),
    };
    if first_line.trim() == REPLACE_DIRECTIVE {
        (Mode::Replace, rest)
    } else {
        (Mode::Append, text)
    }
}

/// Substitute every `{name}` placeholder in `text` with
/// `variables[name]`, scanning `text` left to right in a single pass.
///
/// A placeholder with no matching entry in `variables` — including one
/// whose braces are never closed — is left in the output exactly as
/// written (see the module doc's "Unmatched placeholders" section).
///
/// The single left-to-right pass never re-scans text already copied
/// into the output, including a value just substituted in for an
/// earlier placeholder. This mirrors
/// `spawner::template`'s `substitute_once`, which exists to fix exactly
/// this bug class: a variable whose value itself contains the literal
/// text of another placeholder (e.g. a `branch` value of
/// `"{issue_number}"`) passes through unchanged in the output rather
/// than being substituted a second time.
pub fn interpolate(text: &str, variables: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'outer: while !rest.is_empty() {
        if let Some(after_brace) = rest.strip_prefix('{')
            && let Some(end) = after_brace.find('}')
            && let Some(value) = variables.get(&after_brace[..end])
        {
            out.push_str(value);
            rest = &after_brace[end + 1..];
            continue 'outer;
        }
        // Advance by one `char`, not one byte, to stay UTF-8-safe, and
        // to guarantee forward progress regardless of which branch
        // above declined to match.
        let mut chars = rest.chars();
        let next = chars.next().expect("rest is non-empty per the loop guard");
        out.push(next);
        rest = chars.as_str();
    }
    out
}

/// Read a project's override file for one instruction point, if the
/// project has dropped one (§9.2).
///
/// Returns `Ok(None)` when the file does not exist. That is the
/// expected, common state — §9.2 is explicit that "a deleted override
/// file falls back to the built-in base... the composer never depends
/// on files existing" — so a fresh checkout, or one where a team
/// deleted its override to go back to the default, is not an error.
///
/// # Errors
///
/// Any other I/O failure (a permissions problem, a path component that
/// is not a directory, ...) propagates as [`InstructionError::Read`]
/// rather than being folded into "absent": those are real problems the
/// operator needs to see, not silently treated as "no override
/// configured". This mirrors `init::existing_overrides`'s handling of
/// the project config file — only `NotFound` is special-cased.
pub fn read_instruction_override(
    repo_dir: &Path,
    point: &str,
) -> Result<Option<String>, InstructionError> {
    let path = repo_dir
        .join(".spec-flow")
        .join("instructions")
        .join(format!("{point}.md"));
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(source) => Err(InstructionError::Read { path, source }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn compose_without_override_returns_interpolated_base() {
        let base = "Hello {name}.";

        let result = compose(base, None, &vars(&[("name", "world")]));

        assert_eq!(result, "Hello world.");
    }

    #[test]
    fn compose_append_mode_appends_override_after_base_by_default() {
        let base = "BASE {issue_number}";
        let override_text = "EXTRA {branch}";

        let result = compose(
            base,
            Some(override_text),
            &vars(&[(VAR_ISSUE_NUMBER, "42"), (VAR_BRANCH, "feature/x")]),
        );

        assert_eq!(result, "BASE 42\n\nEXTRA feature/x");
    }

    #[test]
    fn compose_replace_mode_supersedes_base_entirely() {
        let base = "BASE TEXT should not appear";
        let override_text = "<!-- mode: replace -->\nNEW {issue_number} TEXT";

        let result = compose(
            base,
            Some(override_text),
            &vars(&[(VAR_ISSUE_NUMBER, "7")]),
        );

        assert_eq!(result, "NEW 7 TEXT");
    }

    #[test]
    fn interpolate_substitutes_all_five_known_variable_names() {
        let text = format!(
            "{{{VAR_ISSUE_NUMBER}}} {{{VAR_WORKTREE_PATH}}} \
             {{{VAR_BRANCH}}} {{{VAR_SPECS_DIR}}} {{{VAR_DEFAULT_BRANCH}}}"
        );
        let variables = vars(&[
            (VAR_ISSUE_NUMBER, "42"),
            (VAR_WORKTREE_PATH, "/repo/issue-42"),
            (VAR_BRANCH, "issue-42-widget"),
            (VAR_SPECS_DIR, "specs/"),
            (VAR_DEFAULT_BRANCH, "main"),
        ]);

        let result = interpolate(&text, &variables);

        assert_eq!(result, "42 /repo/issue-42 issue-42-widget specs/ main");
    }

    #[test]
    fn compose_replace_mode_tolerates_whitespace_around_the_directive() {
        let override_text = "   <!-- mode: replace -->   \nNEW TEXT";

        let result = compose("BASE", Some(override_text), &HashMap::new());

        assert_eq!(result, "NEW TEXT");
    }

    #[test]
    fn compose_replace_mode_handles_a_crlf_directive_line() {
        let override_text = "<!-- mode: replace -->\r\nNEW TEXT";

        let result = compose("BASE", Some(override_text), &HashMap::new());

        assert_eq!(result, "NEW TEXT");
    }

    #[test]
    fn compose_replace_mode_tolerates_a_leading_utf8_bom() {
        // A realistic artifact of some editors/shells saving a "UTF-8"
        // file (Windows PowerShell's Out-File, older Notepad). `trim`
        // does not strip U+FEFF, so this would silently fall through to
        // `append` (doubling the prompt) without the explicit strip.
        let override_text = "\u{feff}<!-- mode: replace -->\nNEW TEXT";

        let result = compose("BASE", Some(override_text), &HashMap::new());

        assert_eq!(result, "NEW TEXT");
    }

    #[test]
    fn compose_treats_a_near_miss_directive_as_plain_append_text() {
        // Wrong case ("Replace" instead of "replace") does not match
        // the literal directive, so this is ordinary override text —
        // appended in full, mode line included.
        let override_text = "<!-- mode: Replace -->\nBODY";

        let result = compose("BASE", Some(override_text), &HashMap::new());

        assert_eq!(result, "BASE\n\n<!-- mode: Replace -->\nBODY");
    }

    #[test]
    fn compose_does_not_form_a_placeholder_across_the_base_override_boundary()
    {
        // `base` ends in a bare `{` and the override begins with
        // `name}`; concatenated naively the joined text would contain
        // the literal placeholder `{name}`. Because `compose`
        // interpolates each side independently before combining them,
        // that placeholder must never be recognized.
        let base = "prefix {";
        let override_text = "name} suffix";

        let result = compose(
            base,
            Some(override_text),
            &vars(&[("name", "SHOULD_NOT_APPEAR")]),
        );

        assert_eq!(result, "prefix {\n\nname} suffix");
        assert!(!result.contains("SHOULD_NOT_APPEAR"));
    }

    #[test]
    fn interpolate_substitutes_a_present_variable() {
        let result =
            interpolate("id={issue_number}", &vars(&[("issue_number", "9")]));

        assert_eq!(result, "id=9");
    }

    #[test]
    fn interpolate_leaves_an_unmatched_placeholder_as_literal_text() {
        let result =
            interpolate("see {architecture_decision}", &HashMap::new());

        assert_eq!(result, "see {architecture_decision}");
    }

    #[test]
    fn interpolate_substitutes_a_variable_appearing_twice() {
        let result = interpolate("{x}-{x}", &vars(&[("x", "Y")]));

        assert_eq!(result, "Y-Y");
    }

    #[test]
    fn interpolate_does_not_rescan_a_substituted_values_own_placeholder_text()
    {
        // `a`'s value is itself the literal text of another
        // placeholder. A correct single left-to-right pass never
        // revisits text it just emitted, so the output keeps `{b}`
        // literal instead of resolving it to `b`'s value.
        let result = interpolate("{a}", &vars(&[("a", "{b}"), ("b", "REAL")]));

        assert_eq!(result, "{b}");
    }

    #[test]
    fn interpolate_treats_an_unclosed_brace_as_literal_text() {
        let result =
            interpolate("{issue_number", &vars(&[("issue_number", "9")]));

        assert_eq!(result, "{issue_number");
    }

    #[test]
    fn read_instruction_override_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();

        let result = read_instruction_override(dir.path(), "groom").unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn read_instruction_override_returns_contents_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let instructions_dir =
            dir.path().join(".spec-flow").join("instructions");
        fs::create_dir_all(&instructions_dir).unwrap();
        fs::write(instructions_dir.join("groom.md"), "custom text").unwrap();

        let result = read_instruction_override(dir.path(), "groom").unwrap();

        assert_eq!(result, Some("custom text".to_string()));
    }

    #[test]
    fn read_instruction_override_propagates_non_not_found_errors() {
        let dir = tempfile::tempdir().unwrap();
        let instructions_dir =
            dir.path().join(".spec-flow").join("instructions");
        // Make the override *path* a directory instead of a file, so
        // `fs::read_to_string` fails with something other than
        // `NotFound` (typically `IsADirectory` / a permissions-shaped
        // error, platform-dependent) — this must propagate, not be
        // folded into "absent".
        fs::create_dir_all(instructions_dir.join("groom.md")).unwrap();

        let error =
            read_instruction_override(dir.path(), "groom").unwrap_err();

        assert!(matches!(error, InstructionError::Read { .. }));
    }
}
