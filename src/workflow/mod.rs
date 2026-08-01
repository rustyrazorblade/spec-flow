//! The workflow-config data model + parser (§7, §14 step 7): typed Rust
//! shapes for `.spec-flow/workflow.yaml`, deserialized straight from the
//! committed default (see [`crate::scaffold::DEFAULT_WORKFLOW_YAML`]).
//!
//! This module owns exactly the **static, declarative** side of the
//! workflow engine — what a phase is, what it requires, and what gate it
//! carries. The **dynamic** side — gate evaluation, `requires` checking,
//! the review-panel/fix-loop primitive, and `advance`/`approve`/
//! `set_gate` as plain functions — lives in this module's siblings
//! ([`gate`], [`requires`], [`panel`], [`advance`]), each following the
//! same "pure function over already-known facts" shape
//! [`crate::schedule`] and [`crate::state::derive_issue_state`] already
//! use: no [`crate::vcs::Vcs`], no I/O, so every decision is unit-
//! testable without a fake or a real `gh`.
//!
//! # What this module does not do
//!
//! No phase engine, MCP server, or spawner exists yet (§14 step 7 stops
//! short of that; see `crate`'s module doc). Nothing here spawns a
//! process, writes a label, or reads `Vcs` — [`advance::advance`] takes
//! an already-fetched [`crate::state::IssueSnapshot`] and returns a
//! decision; [`advance::approve`]/[`advance::set_gate`] return the label
//! operation(s) a future caller would execute via [`crate::vcs::Vcs`],
//! not perform them. Only the shipped default's single implicit
//! `workflows.default` pipeline is modeled — §7.4's **multiple named
//! workflows** (a `workflows:` map keyed by name, `workflow:<name>`
//! label selection) is a real, documented extension point this crate
//! does not implement: [`WorkflowConfig`] parses the phase-list shape
//! directly, not a map of named phase lists. Extending it to a map is
//! future work, not a redesign — the phase-list shape here is exactly
//! what one named workflow's value would be.

mod advance;
mod gate;
mod panel;
mod requires;

use serde::{Deserialize, Deserializer};

pub use advance::{
    AdvanceDecision, ApproveDecision, LabelOp, advance, approve, set_gate,
};
pub use gate::{GateMode, effective_gate_mode, gate_clear, is_phase_clear};
pub use panel::{
    EscalateReason, FindingSeverity, LensResult, PanelOutcome, ReviewFinding,
    ReviewVerdict, evaluate_panel,
};
pub use requires::{
    Requirement, RequirementContext, phase_requirements_satisfied,
    requirement_satisfied,
};

/// Errors parsing `.spec-flow/workflow.yaml` (§7, §11.3).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkflowError {
    /// The text is not valid YAML, or does not match
    /// [`WorkflowConfig`]'s shape.
    #[error("failed to parse workflow config")]
    InvalidYaml {
        /// The underlying parse failure.
        #[source]
        source: serde_yaml::Error,
    },
}

/// Parse a `.spec-flow/workflow.yaml` document's text into a
/// [`WorkflowConfig`] (§7, §11.3).
///
/// # Errors
///
/// [`WorkflowError::InvalidYaml`] when `yaml` is not valid YAML, or does
/// not match the expected shape — including a `gate:` value that is not
/// exactly `none`/`human`/`auto` (see [`GateMode`]'s doc for why a
/// gate-mode typo must fail the whole parse rather than silently
/// defaulting to a permissive mode).
pub fn parse_workflow(yaml: &str) -> Result<WorkflowConfig, WorkflowError> {
    serde_yaml::from_str(yaml)
        .map_err(|source| WorkflowError::InvalidYaml { source })
}

/// The full parsed shape of `.spec-flow/workflow.yaml` (§7.1, §11.3).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct WorkflowConfig {
    /// Label vocabulary and prefixes (§11.3 `labels`).
    pub labels: LabelsConfig,
    /// Roborev integration config. `None` when the `roborev:` block is
    /// omitted entirely — the shipped comment's "Omit block → off."
    #[serde(default)]
    pub roborev: Option<RoborevConfig>,
    /// Spec-authoring tool config for the `implement` phase (§7.2 ph.6).
    pub spec: SpecConfig,
    /// The review panel's lenses (§2.6, §7.1), run in parallel each
    /// fix-loop round.
    pub review_panel: Vec<ReviewLensConfig>,
    /// The bounded fix loop's shared configuration (§7.1, §7.2 ph.6).
    pub fix_loop: FixLoopConfig,
    /// The ordered phase list (§7.1, §7.2) — the authoritative
    /// sequencing [`advance::advance`] walks, distinct from
    /// [`crate::schedule::DEFAULT_PHASE_ORDER`], which exists only for
    /// the scheduler's cross-issue priority ordering.
    pub phases: Vec<Phase>,
    /// The out-of-band re-entry phases (§7.2) — off the linear
    /// `phases` advance; each has its own explicit trigger tool (§6)
    /// and re-enters a named phase in `phases` rather than following
    /// it.
    #[serde(default)]
    pub out_of_band: Vec<OutOfBandPhase>,
}

/// Label vocabulary and prefixes (§11.3 `labels`).
///
/// **Known gap, deliberately unresolved in this step:** `gate_prefix`/
/// `approval_prefix`/`owner_prefix` are parsed here but not yet
/// *consulted* anywhere. `crate::state`'s `GATE_PREFIX`/
/// `APPROVAL_PREFIX`/`OWNER_PREFIX` constants and [`advance::set_gate`]
/// hard-code the shipped default's literal `"gate:"`/`"approved:"`/
/// `"owner:"` strings instead of reading these fields. A team that
/// customizes a prefix (the field docs below invite it) would get labels
/// the state-derivation layer never recognizes — every `gate:<phase>`
/// label silently fails to match, `IssueSnapshot::gates` resolves to
/// `[]` for every issue, and (combined with `effective_gate_mode`'s
/// label-absent-means-hands-off resolution) every `human`-gated phase
/// for that team quietly runs `auto` **except** the merge phase, which
/// the `is_merge_gate` carve-out cannot recognize either under a
/// customized prefix — it instead blocks unconditionally (stuck, not
/// unsafe, but just as broken). Closing this
/// means threading a [`WorkflowConfig`] (or just its `LabelsConfig`)
/// through [`crate::state::read_issue_state`]/`derive_issue_state` and
/// `crate::claim`, which today take no workflow config at all — a
/// cross-cutting change to already-committed modules, out of scope for
/// this step; flagged here rather than done partially or silently.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct LabelsConfig {
    /// The ordered priority levels, highest first (e.g. `P0`..`P3`).
    pub priority: Vec<String>,
    /// The ordered status/phase vocabulary (e.g. `product-spec`..`done`).
    pub status: Vec<String>,
    /// The `gate:<phase>` label prefix (§4.3).
    pub gate_prefix: String,
    /// The `approved:<phase>` label prefix (§4.3).
    pub approval_prefix: String,
    /// The `owner:<instance>` label prefix (§8.2).
    pub owner_prefix: String,
}

/// Roborev integration config (§11.3 `roborev`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct RoborevConfig {
    /// Whether roborev integration is enabled at all.
    pub enabled: bool,
    /// How a roborev finding is treated.
    pub mode: RoborevMode,
}

/// How a `roborev` finding is treated (§11.3 `roborev.mode`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RoborevMode {
    /// A hard `requires` gate: the phase cannot proceed until roborev
    /// reports clean.
    Gate,
    /// Surfaced only, never blocking.
    Advisory,
}

/// Spec-authoring tool config for the `implement` phase (§7.2 ph.6,
/// §11.3 `spec`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SpecConfig {
    /// Which spec tool `implement` uses to auto-write the spec into the
    /// worktree. A free-form string, not a closed enum: the shipped
    /// comment explicitly allows `openspec | specs_dir | <custom>`, so a
    /// project can name a tool this crate has no dedicated sub-config
    /// for at all.
    pub tool: String,
    /// OpenSpec-specific config, present when `tool` names it (or simply
    /// left in place, inert, when it doesn't — this crate does not
    /// enforce that `tool` and the populated sub-config agree).
    #[serde(default)]
    pub openspec: Option<OpenSpecConfig>,
    /// The `specs_dir`-tool's spec directory, relative to the worktree
    /// root.
    #[serde(default)]
    pub specs_dir: Option<String>,
    /// Whether a per-issue spec skip (`skip_label`) is honored at all.
    pub optional: bool,
    /// The label that, when present on an issue, skips spec-authoring
    /// at `implement` (branch + PR only; §7.2 ph.6, §10.2).
    pub skip_label: String,
}

/// OpenSpec-specific config nested under [`SpecConfig::openspec`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct OpenSpecConfig {
    /// Whether OpenSpec explore+propose runs in the worktree.
    pub enabled: bool,
}

/// One review lens's config entry (§2.6, §7.1, §7.4).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ReviewLensConfig {
    /// The lens's name (e.g. `spec`, `code-review`, `external-audit`).
    pub lens: String,
    /// Where this lens's judgment comes from — see [`ReviewLensSource`].
    #[serde(flatten)]
    pub source: ReviewLensSource,
}

/// One review lens's source of judgment (§2.6, §7.4): either the server
/// spawns an agent for it — as a named `role` or a named `skill` — or an
/// external tool posts its verdict as a comment the server reads.
///
/// Modeled as an **untagged** enum rather than `#[serde(tag = "...")]`:
/// the real YAML gives internal lenses no discriminant field at all
/// (`{lens: spec, role: reviewer}` carries nothing but `role` beyond
/// `lens`), so there is no single tag key common to every variant.
/// Untagged deserialization instead disambiguates structurally — each
/// variant's required field set is disjoint (`role` / `skill` /
/// `type`+`source`) — which is exactly the shape the style guide's
/// "genuinely a tagged union with different fields per type" carve-out
/// describes.
///
/// Deliberately **not** a derived `#[serde(untagged)]` enum, despite
/// looking like a textbook case for one: serde's untagged matching tries
/// variants in declaration order, and a struct variant silently ignores
/// fields it does not recognize. A malformed entry that is missing
/// `role`/`skill` but happens to carry a stray field left over from a
/// copy-paste (e.g. `{type: external, source: "...", role: leftover}`)
/// would match `Role` first under a derived untagged enum, silently
/// dropping `type`/`source`/`required` with no diagnostic at all —
/// turning a required external review gate into an internal spawned
/// agent invisibly. [`Deserialize`] is hand-written below instead: it
/// deserializes into an intermediate shape covering every field any
/// variant could carry (rejecting anything outside that set outright),
/// then requires the populated fields to match exactly one variant's
/// shape, erroring on any other combination — the same "fail the whole
/// parse rather than silently resolve" choice [`GateMode`]'s doc
/// explains for the same class of ambiguity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewLensSource {
    /// An internal lens spawned as a named agent role (§2.6).
    Role {
        /// The agent role to spawn (e.g. `reviewer`).
        role: String,
    },
    /// An internal lens spawned as a named skill (§2.6).
    Skill {
        /// The skill to invoke (e.g. `code-review`).
        skill: String,
    },
    /// An external reviewer that posts its verdict as a comment on the
    /// issue/PR instead of being spawned (§7.4).
    External {
        /// Always the literal string `external`. A dedicated one-variant
        /// enum, not a bare `String`, so a value other than `external`
        /// fails to parse instead of silently being accepted as this
        /// variant — see [`ExternalLensTag`].
        kind: ExternalLensTag,
        /// Where to read the verdict from (e.g. `"review-comment"`).
        source: String,
        /// An optional marker to match within the comment/verdict
        /// (e.g. `"verdict:"`).
        match_marker: Option<String>,
        /// Whether a missing verdict from this lens is a pending gate
        /// (`awaiting-external-review`) rather than an optional,
        /// skippable check (§7.4).
        required: bool,
    },
}

impl<'de> Deserialize<'de> for ReviewLensSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawLensSource {
            role: Option<String>,
            skill: Option<String>,
            #[serde(rename = "type")]
            kind: Option<ExternalLensTag>,
            source: Option<String>,
            #[serde(rename = "match", default)]
            match_marker: Option<String>,
            #[serde(default)]
            required: bool,
        }

        let raw = RawLensSource::deserialize(deserializer)?;
        // Fields that only ever belong to the External shape -- present
        // here at all rules out Role/Skill, not just an absent role/skill.
        let external_only_fields_set =
            raw.source.is_some() || raw.match_marker.is_some() || raw.required;

        match (raw.role, raw.skill, raw.kind) {
            (Some(role), None, None) if !external_only_fields_set => {
                Ok(ReviewLensSource::Role { role })
            }
            (None, Some(skill), None) if !external_only_fields_set => {
                Ok(ReviewLensSource::Skill { skill })
            }
            (None, None, Some(kind)) => {
                let source = raw.source.ok_or_else(|| {
                    serde::de::Error::missing_field("source")
                })?;
                Ok(ReviewLensSource::External {
                    kind,
                    source,
                    match_marker: raw.match_marker,
                    required: raw.required,
                })
            }
            _ => Err(serde::de::Error::custom(
                "a review lens source must set exactly one of `role`, \
                 `skill`, or `type: external` (with no fields from \
                 another shape mixed in)",
            )),
        }
    }
}

/// The literal discriminant `{type: external, ...}` carries for
/// [`ReviewLensSource::External`] — see that variant's doc for why this
/// is a dedicated type rather than a bare `String`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalLensTag {
    /// The only value this tag ever takes.
    External,
}

/// The bounded fix loop's shared configuration (§7.1, §7.2 ph.6,
/// §11.3 `fix_loop`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct FixLoopConfig {
    /// The maximum number of fix-sub-phase rounds before escalating
    /// (§7.1's `K`).
    pub max_rounds: u32,
    /// What happens when the round cap is hit still un-green.
    pub on_exhausted: FixLoopPolicy,
    /// What happens the moment any lens reports a decision-class
    /// finding.
    pub on_decision_finding: FixLoopPolicy,
}

/// A fix-loop policy value (§11.3 `fix_loop.on_exhausted` /
/// `on_decision_finding`).
///
/// The shipped default only ever uses `escalate`, and §7.1/§6 describe
/// no other outcome for either case — but this is left as its own enum,
/// not a hard-coded assumption, precisely so [`panel::evaluate_panel`]
/// reads the *config's* policy rather than a literal baked into the
/// function, even though only one variant exists to read today.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FixLoopPolicy {
    /// Escalate to a `human` gate (`awaiting_decision`).
    Escalate,
}

/// A `requires` value the shipped default names but this crate does not
/// yet act on ([`OnConflict`], `status_label` conventions, etc.) or a
/// phase-level side-behavior directive.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Escalate to a `human` gate (`awaiting_decision`) on a detected
    /// conflict (§7.2 ph.2).
    Escalate,
}

/// One phase in the ordered pipeline (§7.1, §7.2, §11.3 `phases[]`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Phase {
    /// The phase's canonical id — what `gate:<id>` / `approved:<id>`
    /// labels (§4.3) and `requires: ["approved:<id>"]` entries name.
    /// Not always equal to [`Self::status_label`] (`finalize`'s
    /// `status_label` is `done`, per the shipped default).
    pub id: String,
    /// What this phase does: spawn an agent, run a mechanical server
    /// step, or the compound implement shape (§7.1).
    pub action: PhaseAction,
    /// What this phase produces and where it lands, when it produces a
    /// standalone artifact at all (`implement`/`review`/`finalize` in
    /// the shipped default do not).
    #[serde(default)]
    pub artifact: Option<ArtifactSpec>,
    /// This phase's configured default gate mode. `None` when the
    /// `gate:` key is absent from the YAML entirely — deliberately
    /// distinct from `Some(GateMode::None)` (the key present, set to the
    /// literal `none`). See [`effective_gate_mode`]'s doc for why that
    /// distinction matters and how a missing value is resolved.
    #[serde(default)]
    pub gate: Option<GateMode>,
    /// The label the server writes when this phase's `human` gate is
    /// granted (e.g. `"approved:product-spec"`). `None` for phases with
    /// no approval label at all (e.g. `conflict-check`, `finalize`).
    #[serde(default)]
    pub approval_label: Option<String>,
    /// The `status:<label>` this phase stamps on the issue while it is
    /// current.
    #[serde(default)]
    pub status_label: Option<String>,
    /// This phase's entry conditions (§7.1 `requires`) — empty when the
    /// YAML omits the key, meaning "no additional conditions."
    #[serde(default)]
    pub requires: Vec<Requirement>,
    /// What happens when this phase detects a conflict (`conflict-check`
    /// only, in the shipped default).
    #[serde(default)]
    pub on_conflict: Option<OnConflict>,
}

/// A phase's declared behavior (§7.1).
///
/// Tagged by the YAML's own `type` discriminant
/// (`agent` / `server` / `server_then_agent`) — unlike
/// [`ReviewLensSource`], every real variant here does carry that common
/// field, so `#[serde(tag = "type")]` applies directly rather than
/// needing an untagged fallback.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PhaseAction {
    /// A spawned `claude` process produces a content artifact (§7.1).
    Agent {
        /// The agent role to spawn (e.g. `product-manager`, `architect`).
        role: String,
        /// Whether this phase is a live dialogue surfaced to the
        /// coordinator rather than a fire-and-forget spawn (§7.1) — only
        /// the shipped `product-spec` phase sets this.
        #[serde(default)]
        interactive: bool,
    },
    /// A mechanical step the server performs itself via `git`/`gh`
    /// (§7.1); no agent is spawned.
    Server {
        /// The mechanical operation to run (e.g. `merge_queue`,
        /// `finalize`, `sync_ci`).
        op: String,
    },
    /// The compound `implement` shape (§7.2 ph.6): the server performs a
    /// mechanical step first (claim the issue, create the worktree),
    /// then a spawned agent runs the parallel-group + bounded-fix-loop
    /// review-panel primitive (§7.1).
    ServerThenAgent {
        /// The mechanical operation to run first (e.g.
        /// `start_implement`).
        op: String,
        /// The spec tool used to auto-write the spec into the worktree
        /// (§7.2 ph.6, §11.3 `spec.tool`).
        spec_tool: String,
        /// The agent role that implements test-first.
        role: String,
        /// The name of the top-level named block (currently always
        /// [`WorkflowConfig::review_panel`], the only one that exists)
        /// this phase's review panel runs. Kept as a **name**, not
        /// resolved inline: the YAML shape is a reference by name
        /// (`panel: review_panel`), matching the same top-level key —
        /// resolving it into an owned copy here would silently assume
        /// there is exactly one named panel forever, which §7.4's
        /// multiple-named-workflows extension point does not promise.
        panel: String,
        /// The name of the top-level `fix_loop` block this phase's fix
        /// loop uses — see the `panel` field's doc above for why this is
        /// a name rather than a resolved value.
        fix_loop: String,
        /// The next agent role to run after the panel passes (e.g.
        /// `build-engineer`).
        then: String,
    },
}

/// What a phase produces and where it lands (§7.1 `artifact`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ArtifactSpec {
    /// The artifact's shape.
    pub kind: ArtifactKind,
    /// A title for the posted content, when `kind` uses one (e.g. an
    /// issue comment's heading).
    #[serde(default)]
    pub title: Option<String>,
}

/// The shape an [`ArtifactSpec`] takes (§7.1 `artifact.kind`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// The artifact IS the GitHub issue's own content (the `product-
    /// spec` phase writes the issue body/labels directly).
    Issue,
    /// The artifact is posted as a comment on the issue.
    IssueComment,
}

/// An out-of-band re-entry phase (§7.2, §11.3 `out_of_band[]`) — off the
/// linear `phases` advance, each with its own explicit trigger tool
/// (§6).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct OutOfBandPhase {
    /// The re-entry's id (e.g. `sync-ci`, `address`).
    pub id: String,
    /// What this re-entry does when triggered.
    pub action: PhaseAction,
    /// The [`Phase::id`] in `phases` this re-entry returns control to
    /// once it completes.
    pub reenters: String,
    /// The default auto-trigger behavior (§11.3) — always explicitly
    /// callable regardless of this setting (§6).
    pub trigger: Trigger,
}

/// An out-of-band re-entry's default trigger behavior (§7.2, §11.3
/// `out_of_band[].trigger`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// A poller/GitHub event fires this re-entry automatically (subject
    /// to its own config gate, §12).
    Auto,
    /// Only an explicit tool call fires this re-entry.
    Operator,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::DEFAULT_WORKFLOW_YAML;

    #[test]
    fn parse_workflow_reads_the_shipped_default_without_error() {
        parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();
    }

    #[test]
    fn parse_workflow_reads_labels_from_the_shipped_default() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();

        assert_eq!(workflow.labels.priority, vec!["P0", "P1", "P2", "P3"]);
        assert_eq!(workflow.labels.status.len(), 8);
        assert_eq!(workflow.labels.gate_prefix, "gate:");
        assert_eq!(workflow.labels.approval_prefix, "approved:");
        assert_eq!(workflow.labels.owner_prefix, "owner:");
    }

    #[test]
    fn parse_workflow_reads_roborev_from_the_shipped_default() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();

        let roborev = workflow.roborev.unwrap();
        assert!(roborev.enabled);
        assert_eq!(roborev.mode, RoborevMode::Gate);
    }

    #[test]
    fn parse_workflow_reads_spec_config_from_the_shipped_default() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();

        assert_eq!(workflow.spec.tool, "openspec");
        assert!(workflow.spec.openspec.unwrap().enabled);
        assert_eq!(workflow.spec.specs_dir.as_deref(), Some("specs/"));
        assert!(workflow.spec.optional);
        assert_eq!(workflow.spec.skip_label, "spec:skip");
    }

    #[test]
    fn parse_workflow_reads_every_review_lens_from_the_shipped_default() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();

        assert_eq!(workflow.review_panel.len(), 5);
        assert_eq!(workflow.review_panel[0].lens, "spec");
        assert_eq!(
            workflow.review_panel[0].source,
            ReviewLensSource::Role { role: "reviewer".to_string() }
        );
        assert_eq!(workflow.review_panel[1].lens, "code-review");
        assert_eq!(
            workflow.review_panel[1].source,
            ReviewLensSource::Skill { skill: "code-review".to_string() }
        );
    }

    #[test]
    fn parse_workflow_reads_an_external_review_lens() {
        // Commented out in the shipped default (§7.4's example), but a
        // real, documented variant — parse it from a standalone snippet
        // to prove the shape this crate models actually deserializes.
        let yaml = r#"{type: external, source: "review-comment", match: "verdict:", required: true}"#;

        let source: ReviewLensSource = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(
            source,
            ReviewLensSource::External {
                kind: ExternalLensTag::External,
                source: "review-comment".to_string(),
                match_marker: Some("verdict:".to_string()),
                required: true,
            }
        );
    }

    #[test]
    fn review_lens_source_rejects_an_unrecognized_field() {
        // A stray field belonging to no known shape must fail loudly
        // rather than being silently dropped by whichever variant
        // happens to still parse without it.
        let yaml = r#"{role: reviewer, bogus: "typo"}"#;

        assert!(serde_yaml::from_str::<ReviewLensSource>(yaml).is_err());
    }

    #[test]
    fn review_lens_source_rejects_a_role_with_a_stray_external_field() {
        // Finding #5's exact danger: an entry meant to be `type:
        // external` that also carries a leftover `role` key from a
        // copy-paste must not silently match `Role` and drop `type`/
        // `source`/`required` with no diagnostic.
        let yaml =
            r#"{type: external, source: "review-comment", role: "leftover"}"#;

        assert!(serde_yaml::from_str::<ReviewLensSource>(yaml).is_err());
    }

    #[test]
    fn review_lens_source_rejects_both_role_and_skill_set_at_once() {
        let yaml = r#"{role: reviewer, skill: code-review}"#;

        assert!(serde_yaml::from_str::<ReviewLensSource>(yaml).is_err());
    }

    #[test]
    fn review_lens_source_rejects_an_external_entry_missing_source() {
        let yaml = r#"{type: external}"#;

        assert!(serde_yaml::from_str::<ReviewLensSource>(yaml).is_err());
    }

    #[test]
    fn parse_workflow_rejects_an_external_lens_with_the_wrong_type_tag() {
        let yaml = r#"{type: internal, source: "review-comment"}"#;

        assert!(serde_yaml::from_str::<ReviewLensSource>(yaml).is_err());
    }

    #[test]
    fn parse_workflow_reads_fix_loop_from_the_shipped_default() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();

        assert_eq!(workflow.fix_loop.max_rounds, 3);
        assert_eq!(workflow.fix_loop.on_exhausted, FixLoopPolicy::Escalate);
        assert_eq!(
            workflow.fix_loop.on_decision_finding,
            FixLoopPolicy::Escalate
        );
    }

    #[test]
    fn parse_workflow_reads_every_phase_from_the_shipped_default() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();

        let ids: Vec<&str> =
            workflow.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "product-spec",
                "conflict-check",
                "architecture",
                "test-plan",
                "implement",
                "review",
                "finalize",
            ]
        );
    }

    #[test]
    fn parse_workflow_reads_the_product_spec_phase_shape() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();
        let phase = &workflow.phases[0];

        assert_eq!(
            phase.action,
            PhaseAction::Agent {
                role: "product-manager".to_string(),
                interactive: true,
            }
        );
        assert_eq!(phase.gate, Some(GateMode::Human));
        assert_eq!(
            phase.approval_label.as_deref(),
            Some("approved:product-spec")
        );
        assert_eq!(phase.status_label.as_deref(), Some("product-spec"));
    }

    #[test]
    fn parse_workflow_reads_the_conflict_check_phases_on_conflict() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();
        let phase = &workflow.phases[1];

        assert_eq!(phase.gate, Some(GateMode::None));
        assert_eq!(phase.on_conflict, Some(OnConflict::Escalate));
    }

    #[test]
    fn parse_workflow_reads_the_implement_phases_compound_action() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();
        let phase =
            workflow.phases.iter().find(|p| p.id == "implement").unwrap();

        assert_eq!(
            phase.action,
            PhaseAction::ServerThenAgent {
                op: "start_implement".to_string(),
                spec_tool: "openspec".to_string(),
                role: "developer".to_string(),
                panel: "review_panel".to_string(),
                fix_loop: "fix_loop".to_string(),
                then: "build-engineer".to_string(),
            }
        );
        assert_eq!(
            phase.requires,
            vec![
                Requirement::Approved("product-spec".to_string()),
                Requirement::Approved("architecture".to_string()),
                Requirement::Approved("test-plan".to_string()),
            ]
        );
        assert!(phase.artifact.is_none());
    }

    #[test]
    fn parse_workflow_reads_the_review_phases_requires() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();
        let phase = workflow.phases.iter().find(|p| p.id == "review").unwrap();

        assert_eq!(
            phase.requires,
            vec![
                Requirement::Ci("green".to_string()),
                Requirement::Roborev("clean".to_string()),
                Requirement::DepsMerged,
            ]
        );
        assert_eq!(phase.gate, Some(GateMode::Human));
        assert_eq!(phase.approval_label.as_deref(), Some("approved:merge"));
    }

    #[test]
    fn parse_workflow_reads_the_finalize_phases_status_label() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();
        let phase =
            workflow.phases.iter().find(|p| p.id == "finalize").unwrap();

        // finalize's id and status_label deliberately differ — this is
        // exactly why Phase::id and Phase::status_label are two separate
        // fields rather than one.
        assert_eq!(phase.status_label.as_deref(), Some("done"));
        assert_eq!(phase.gate, Some(GateMode::None));
        assert!(phase.requires.is_empty());
    }

    #[test]
    fn parse_workflow_reads_both_out_of_band_reentries() {
        let workflow = parse_workflow(DEFAULT_WORKFLOW_YAML).unwrap();

        assert_eq!(workflow.out_of_band.len(), 2);
        assert_eq!(workflow.out_of_band[0].id, "sync-ci");
        assert_eq!(workflow.out_of_band[0].reenters, "implement");
        assert_eq!(workflow.out_of_band[0].trigger, Trigger::Auto);
        assert_eq!(workflow.out_of_band[1].id, "address");
        assert_eq!(workflow.out_of_band[1].reenters, "review");
        assert_eq!(workflow.out_of_band[1].trigger, Trigger::Operator);
    }

    #[test]
    fn parse_workflow_distinguishes_an_absent_gate_from_an_explicit_none() {
        let yaml = r#"
labels: {priority: [P0], status: [a, b], gate_prefix: "gate:", approval_prefix: "approved:", owner_prefix: "owner:"}
spec: {tool: openspec, optional: false, skip_label: "spec:skip"}
review_panel: []
fix_loop: {max_rounds: 1, on_exhausted: escalate, on_decision_finding: escalate}
phases:
  - {id: a, action: {type: server, op: noop}, status_label: a}
  - {id: b, action: {type: server, op: noop}, gate: none, status_label: b}
"#;

        let workflow = parse_workflow(yaml).unwrap();

        assert_eq!(workflow.phases[0].gate, None);
        assert_eq!(workflow.phases[1].gate, Some(GateMode::None));
    }

    #[test]
    fn parse_workflow_rejects_an_unrecognized_gate_mode_string() {
        // A gate-mode typo (wrong case, misspelling) must fail the whole
        // parse rather than silently resolving to a permissive mode —
        // see gate.rs's module doc for the safety invariant this
        // protects (§2.3b: "nothing... by accident").
        let yaml = r#"
labels: {priority: [P0], status: [a], gate_prefix: "gate:", approval_prefix: "approved:", owner_prefix: "owner:"}
spec: {tool: openspec, optional: false, skip_label: "spec:skip"}
review_panel: []
fix_loop: {max_rounds: 1, on_exhausted: escalate, on_decision_finding: escalate}
phases:
  - {id: a, action: {type: server, op: noop}, gate: Human, status_label: a}
"#;

        assert!(parse_workflow(yaml).is_err());
    }

    #[test]
    fn parse_workflow_reports_missing_roborev_block_as_none() {
        let yaml = r#"
labels: {priority: [P0], status: [a], gate_prefix: "gate:", approval_prefix: "approved:", owner_prefix: "owner:"}
spec: {tool: openspec, optional: false, skip_label: "spec:skip"}
review_panel: []
fix_loop: {max_rounds: 1, on_exhausted: escalate, on_decision_finding: escalate}
phases:
  - {id: a, action: {type: server, op: noop}, status_label: a}
"#;

        let workflow = parse_workflow(yaml).unwrap();

        assert!(workflow.roborev.is_none());
    }
}
