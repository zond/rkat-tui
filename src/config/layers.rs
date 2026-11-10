//! Loading and merging the `tui.toml` layers (see the [module docs](super)).

use super::file::*;
use crate::approval::mode::PermissionMode;
use crate::approval::rules::{Rule, RuleKind, RuleOrigin, RuleSet, RulesError};
use crate::bootstrap::confinement::ConfinementSection;
use crate::bootstrap::trust::{TrustStatus, Untrusted};
use crate::thinking::ThinkingSetting;
use crate::ui::compaction::CompactionPolicy;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Non-fatal observations from loading rules, shown to the user once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The project-level file contained allow rules, which are ignored.
    ProjectAllowIgnored {
        /// Project rules file.
        path: PathBuf,
        /// Number of ignored entries.
        count: usize,
    },
    /// The project-level file tried to enable `trust_project_builds`; only the
    /// user's own layers may turn it on.
    ProjectTrustIgnored {
        /// Project rules file.
        path: PathBuf,
    },
    /// The project-level file named a `classifier_model`; only the user's
    /// own layers pick the model that reviews the agent's actions.
    ProjectClassifierModelIgnored {
        /// Project rules file.
        path: PathBuf,
        /// The model it named.
        model: String,
    },
    /// The realm rules file is not trusted (see [`crate::bootstrap::trust`]):
    /// only its deny rules apply.
    UntrustedRealmDenyOnly {
        /// Realm rules file.
        path: PathBuf,
        /// Why it is not trusted.
        reason: Untrusted,
        /// Ignored allow rules.
        allow: usize,
        /// Whether a non-empty `[auto]` section was ignored.
        auto: bool,
        /// Ignored endpoints.
        endpoints: usize,
        /// Ignored price overrides.
        pricing: usize,
        /// Ignored `[thinking]` entries.
        thinking: usize,
        /// Ignored `[confinement]` grants (directories).
        confinement: usize,
    },
    /// A configured endpoint is invalid and was skipped.
    EndpointSkipped {
        /// Endpoint name.
        name: String,
        /// Why.
        reason: String,
    },
    /// The project file defines an endpoint. Project files never define
    /// endpoints (a cloned repository could route prompts and keys to its
    /// own server): it is ignored, never used as a default or for the
    /// classifier.
    ProjectEndpointIgnored {
        /// Project rules file.
        path: PathBuf,
        /// Endpoint name.
        name: String,
    },
    /// A file the user does not own (the project file, or an untrusted
    /// realm file) set `[pricing]`; only user layers may.
    PricingIgnored {
        /// The rules file.
        path: PathBuf,
        /// Number of ignored model entries.
        count: usize,
    },
    /// The project file sets `[confinement]` grants. Only the user's own
    /// layers may widen the shell's sandbox: they are ignored.
    ProjectConfinementIgnored {
        /// Project rules file.
        path: PathBuf,
        /// Number of ignored grants (directories).
        count: usize,
    },
    /// A file the user does not own (the project file) set `[thinking]`;
    /// only user layers may (a thinking budget is billed).
    ThinkingIgnored {
        /// The rules file.
        path: PathBuf,
        /// Number of ignored model entries.
        count: usize,
    },
    /// A project rules file's `[daemon]` section (only the user's own
    /// files configure the daemon).
    DaemonIgnored {
        /// The rules file.
        path: PathBuf,
    },
    /// A `[pricing."<model>"]` entry has a negative or non-finite price.
    PricingInvalid {
        /// Model id.
        model: String,
        /// The offending field.
        field: &'static str,
    },
    /// The layered `compact_warn_percent`/`compact_auto_percent` were out of
    /// range (`1..=100`, auto ≥ warn) and were clamped.
    CompactPercentClamped {
        /// The configured `(warn, auto)` percentages.
        configured: (u8, u8),
        /// The `(warn, auto)` percentages in effect.
        applied: (u8, u8),
    },
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectAllowIgnored { path, count } => write!(
                f,
                "ignored {count} allow rule(s) in {}: project rules files may only add deny rules",
                path.display()
            ),
            Self::ProjectTrustIgnored { path } => write!(
                f,
                "ignored trust_project_builds=true in {}: a project file can only lower trust",
                path.display()
            ),
            Self::ProjectClassifierModelIgnored { path, model } => write!(
                f,
                "ignored classifier_model={model:?} in {}: only your own configuration picks the classifier",
                path.display()
            ),
            Self::UntrustedRealmDenyOnly {
                path,
                reason,
                allow,
                auto,
                endpoints,
                pricing,
                thinking,
                confinement,
            } => {
                write!(
                    f,
                    "{} is not trusted ({reason}); only its deny rules apply (ignored {allow} allow rule(s), {endpoints} endpoint(s), {pricing} price override(s), {thinking} [thinking] setting(s), {confinement} [confinement] grant(s)",
                    path.display()
                )?;
                if *auto {
                    f.write_str(" and its [auto] settings")?;
                }
                f.write_str("); review it and run /permissions trust to use all of it")
            }
            Self::EndpointSkipped { name, reason } => {
                write!(f, "endpoint `{name}` skipped: {reason}")
            }
            Self::ProjectEndpointIgnored { path, name } => write!(
                f,
                "ignored [endpoints.{name}] in {}: project files may not define endpoints; define it in your own tui.toml",
                path.display()
            ),
            Self::PricingIgnored { path, count } => write!(
                f,
                "ignored {count} [pricing] entr{} in {}: only your own tui.toml files set prices",
                if *count == 1 { "y" } else { "ies" },
                path.display()
            ),
            Self::ProjectConfinementIgnored { path, count } => write!(
                f,
                "ignored {count} [confinement] grant(s) in {}: project files may not widen the shell's sandbox; add them to your own tui.toml",
                path.display()
            ),
            Self::ThinkingIgnored { path, count } => write!(
                f,
                "ignored {count} [thinking] setting(s) in {}: only your own tui.toml files set thinking",
                path.display()
            ),
            Self::DaemonIgnored { path } => write!(
                f,
                "ignored [daemon] in {}: only your own tui.toml files configure the daemon",
                path.display()
            ),
            Self::PricingInvalid { model, field } => write!(
                f,
                "ignored [pricing.\"{model}\"]: `{field}` must be a non-negative number"
            ),
            Self::CompactPercentClamped {
                configured: (warn, auto),
                applied: (applied_warn, applied_auto),
            } => write!(
                f,
                "[auto] compact_warn_percent = {warn}, compact_auto_percent = {auto} are out of range (1..=100, auto ≥ warn); using {applied_warn} and {applied_auto}"
            ),
        }
    }
}

/// How far the realm rules file (`<realm>/tui.toml`) is trusted (decided
/// from the host trust record, see [`crate::bootstrap::trust`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealmTrust {
    /// The file is what rkat-tui wrote or the user trusted: all of it applies.
    Owned,
    /// The file is not trusted: only its deny rules apply.
    DenyOnly(Untrusted),
}

impl RealmTrust {
    /// The trust a [`TrustStatus`] gives the realm rules file.
    #[must_use]
    pub fn from_status(status: TrustStatus) -> Self {
        match status {
            TrustStatus::Missing | TrustStatus::Trusted => Self::Owned,
            TrustStatus::Untrusted(reason) => Self::DenyOnly(reason),
        }
    }
}

/// Everything `tui.toml` configures, after layering.
#[derive(Debug, Clone, Default)]
pub struct Layered {
    /// Merged allow/deny rules.
    pub rules: RuleSet,
    /// Merged `[auto]` settings.
    pub settings: AutoSettings,
    /// The permission mode at start (`[auto] mode`; `enabled = false`, or
    /// `--no-auto`, is [`PermissionMode::Manual`]). The approval engine
    /// owns the mode from then on.
    pub mode: PermissionMode,
    /// Merged `[endpoints.<name>]` (not yet validated).
    pub endpoints: BTreeMap<String, LayeredEndpoint>,
    /// Merged `[pricing."<model>"]` overrides (invalid entries dropped).
    pub pricing: BTreeMap<String, PricingSection>,
    /// `[pricing] fetch`: whether the community price list may be fetched
    /// (the last user layer that sets it; a project file may only turn it
    /// off). `--no-pricing-fetch` turns it off whatever this says.
    pub pricing_fetch: bool,
    /// Merged `[thinking]` settings of the user layers (not yet validated,
    /// see [`crate::thinking::ThinkingTable::validate`]).
    pub thinking: BTreeMap<String, ThinkingSetting>,
    /// Merged `[confinement]` grants of the user layers (global, an owned
    /// realm file, the command-line overlay), in layer order; not yet
    /// validated (see [`crate::bootstrap::confinement::grants_with`]).
    pub confinement: ConfinementSection,
    /// Merged `[helpers]` settings.
    pub helpers: HelperSettings,
    /// Merged `[shell]` settings.
    pub shell: ShellSettings,
    /// Merged `[ui]` settings.
    pub ui: UiSettings,
    /// `[daemon] principal` of the user layers (the last one set).
    pub daemon_principal: Option<String>,
    /// Non-fatal observations to show once.
    pub notices: Vec<Notice>,
}

/// Load and merge the three rule layers.
///
/// Same as [`load_layered_with`] without a command-line layer, returning
/// only rules, settings and notices.
///
/// # Errors
///
/// Returns [`RulesError`] when a file cannot be read or parsed.
#[cfg(any(test, feature = "test-support"))]
pub fn load_layered(
    realm_dir: &Path,
    global_rules_file: Option<&Path>,
    project_rkat_dir: &Path,
    realm_trust: RealmTrust,
) -> Result<(RuleSet, AutoSettings, Vec<Notice>), RulesError> {
    let layered = load_layered_with(
        realm_dir,
        global_rules_file,
        project_rkat_dir,
        realm_trust,
        None,
    )?;
    Ok((layered.rules, layered.settings, layered.notices))
}

/// Load and merge the rule layers: global, realm, the command-line overlay
/// `cli` (see [`crate::overlay`]), then the project file.
///
/// `global_rules_file` is the user-global rules file itself (typically
/// [`crate::bootstrap::GlobalDirs::rules_file`]); the realm and project layers
/// are `tui.toml` inside `realm_dir` and `project_rkat_dir`.
///
/// User layers (global → realm → cli): rules accumulate, `[auto]` fields and
/// endpoint fields replace those of earlier layers, endpoints merge by name.
/// `[confinement]` grants accumulate.
/// A [`RealmTrust::DenyOnly`] realm file contributes only deny rules (no allow
/// rules, `[auto]` settings, endpoints, prices or `[confinement]` grants).
/// The project layer contributes deny rules and `[auto]` settings the
/// command line did not set; its endpoints and `[confinement]` grants are
/// ignored with a notice; it can never turn
/// `trust_project_builds` or `classifier_tools` on, but may turn them off,
/// and never names the `classifier_model`.
///
/// # Errors
///
/// Returns [`RulesError`] when a file cannot be read or parsed.
pub fn load_layered_with(
    realm_dir: &Path,
    global_rules_file: Option<&Path>,
    project_rkat_dir: &Path,
    realm_trust: RealmTrust,
    cli: Option<&RulesFile>,
) -> Result<Layered, RulesError> {
    let mut rules = RuleSet::default();
    let mut settings = AutoSettings::default();
    let mut mode = PermissionMode::default();
    let mut notices = Vec::new();
    let mut endpoints: BTreeMap<String, LayeredEndpoint> = BTreeMap::new();
    let mut pricing: BTreeMap<String, PricingSection> = BTreeMap::new();
    let mut pricing_fetch = true;
    let mut thinking: BTreeMap<String, ThinkingSetting> = BTreeMap::new();
    let mut confinement = ConfinementSection::default();
    let mut helpers = HelperSettings::default();
    let mut shell = ShellSettings::default();
    let mut ui = UiSettings::default();
    let mut daemon_principal = None;

    let mut user_layers: Vec<(Option<PathBuf>, RuleOrigin, RulesFile)> = Vec::new();
    if let Some(global) = global_rules_file {
        user_layers.push((
            Some(global.to_path_buf()),
            RuleOrigin::Global,
            RulesFile::load(global)?,
        ));
    }
    let realm_path = realm_dir.join(RULES_FILE_NAME);
    let realm_file = RulesFile::load(&realm_path)?;
    user_layers.push((Some(realm_path), RuleOrigin::Realm, realm_file));
    if let Some(cli) = cli {
        user_layers.push((None, RuleOrigin::Cli, cli.clone()));
    }

    for (path, origin, file) in user_layers {
        let untrusted = match (&realm_trust, origin) {
            (RealmTrust::DenyOnly(reason), RuleOrigin::Realm) => Some(reason.clone()),
            _ => None,
        };
        let path = path.unwrap_or_default();
        let mut ignored_allow = 0;
        for rule in file.rules(origin)? {
            if untrusted.is_some() && rule.kind == RuleKind::Allow {
                ignored_allow += 1;
            } else {
                rules.push(rule);
            }
        }
        if let Some(reason) = untrusted {
            let auto = file
                .auto
                .as_ref()
                .is_some_and(|auto| *auto != AutoSection::default());
            let grants = file.confinement.as_ref().map_or(0, ConfinementSection::len);
            if ignored_allow > 0
                || auto
                || !file.endpoints.is_empty()
                || !file.pricing.is_empty()
                || !file.thinking.is_empty()
                || grants > 0
            {
                notices.push(Notice::UntrustedRealmDenyOnly {
                    path,
                    reason,
                    allow: ignored_allow,
                    auto,
                    endpoints: file.endpoints.len(),
                    pricing: file.pricing.len(),
                    thinking: file.thinking.len(),
                    confinement: grants,
                });
            }
            continue;
        }
        if let Some(section) = &file.confinement {
            confinement.extend_from(section);
        }
        for (model, section) in &file.pricing {
            pricing
                .entry(model.clone())
                .or_default()
                .merge_from(section);
        }
        if let Some(fetch) = file.pricing.fetch {
            pricing_fetch = fetch;
        }
        // A later layer's setting replaces an earlier one's (`"default"`
        // undoes it).
        thinking.extend(
            file.thinking
                .iter()
                .map(|(key, setting)| (key.clone(), *setting)),
        );
        for (name, section) in &file.endpoints {
            endpoints
                .entry(name.clone())
                .and_modify(|layered| layered.section.merge_from(section))
                .or_insert_with(|| LayeredEndpoint {
                    section: section.clone(),
                    origin,
                });
        }
        if let Some(auto) = &file.auto {
            apply_auto(&mut settings, &mut mode, auto);
        }
        if let Some(depth) = file.helpers.as_ref().and_then(|h| h.max_depth) {
            helpers.max_depth = depth;
        }
        if let Some(secs) = file.shell.and_then(|s| s.user_timeout_secs) {
            shell.user_timeout_secs = secs.max(1);
        }
        if let Some(section) = &file.ui {
            ui.apply(section);
        }
        if let Some(principal) = file.daemon.as_ref().and_then(|d| d.principal.clone()) {
            daemon_principal = Some(principal);
        }
    }

    let project_path = project_rkat_dir.join(RULES_FILE_NAME);
    let project = RulesFile::load(&project_path)?;
    // A project's display preference applies unless the command line set it.
    if let Some(section) = &project.ui {
        let cli_ui = cli.and_then(|c| c.ui).unwrap_or_default();
        ui.apply(&UiSection {
            helpers: section.helpers.filter(|_| cli_ui.helpers.is_none()),
            bell: section.bell.filter(|_| cli_ui.bell.is_none()),
            checklist: section.checklist.filter(|_| cli_ui.checklist.is_none()),
        });
    }
    if !project.allow.is_empty() {
        notices.push(Notice::ProjectAllowIgnored {
            path: project_path.clone(),
            count: project.allow.len(),
        });
    }
    for entry in &project.deny {
        rules.push(Rule::from_entry(
            RuleKind::Deny,
            entry,
            RuleOrigin::Project,
        )?);
    }
    // Not fetching only keeps a request from being made.
    if project.pricing.fetch == Some(false) {
        pricing_fetch = false;
    }
    if !project.pricing.models.is_empty() {
        notices.push(Notice::PricingIgnored {
            path: project_path.clone(),
            count: project.pricing.len(),
        });
    }
    if !project.thinking.is_empty() {
        notices.push(Notice::ThinkingIgnored {
            path: project_path.clone(),
            count: project.thinking.len(),
        });
    }
    if project.daemon.is_some() {
        notices.push(Notice::DaemonIgnored {
            path: project_path.clone(),
        });
    }
    pricing.retain(|model, section| match section.invalid_field() {
        Some(field) => {
            notices.push(Notice::PricingInvalid {
                model: model.clone(),
                field,
            });
            false
        }
        None => true,
    });
    // Nor do they widen the shell's sandbox.
    let project_grants = project
        .confinement
        .as_ref()
        .map_or(0, ConfinementSection::len);
    if project_grants > 0 {
        notices.push(Notice::ProjectConfinementIgnored {
            path: project_path.clone(),
            count: project_grants,
        });
    }
    // A cloned repository may keep helpers shallower, never deeper.
    if let Some(depth) = project.helpers.as_ref().and_then(|h| h.max_depth) {
        helpers.max_depth = helpers.max_depth.min(depth);
    }
    // Nor may it let the user's own commands run longer.
    if let Some(secs) = project.shell.and_then(|s| s.user_timeout_secs) {
        shell.user_timeout_secs = shell.user_timeout_secs.min(secs.max(1));
    }
    // Project files never define endpoints, not even new ones: a cloned
    // repository could route prompts and credentials to its own server.
    for name in project.endpoints.keys() {
        notices.push(Notice::ProjectEndpointIgnored {
            path: project_path.clone(),
            name: name.clone(),
        });
    }
    if let Some(mut auto) = project.auto {
        if auto.trust_project_builds == Some(true) {
            auto.trust_project_builds = None;
            notices.push(Notice::ProjectTrustIgnored {
                path: project_path.clone(),
            });
        }
        // Nor the model that reviews the agent's actions: a cloned
        // repository could pick one that is cheap to talk past.
        if let Some(model) = auto.classifier_model.take() {
            notices.push(Notice::ProjectClassifierModelIgnored {
                path: project_path,
                model,
            });
        }
        // The permission mode is the user's choice: a cloned repository
        // cannot pick one (`/mode` writes the realm file, never this one),
        // it can only turn the classifier off (`enabled = false` moves auto
        // mode to manual; any other mode stays).
        auto.mode = None;
        let classifier_off = auto.enabled.take() == Some(false);
        let classifier_off_by_cli = cli
            .and_then(|c| c.auto.as_ref())
            .is_some_and(|c| c.enabled.is_some() || c.mode.is_some());
        // File contents a cloned repository wrote must not reach the
        // classifier against the user's choice: the project may only turn
        // the classifier's file reads off.
        if auto.classifier_tools == Some(true) {
            auto.classifier_tools = None;
        }
        // The command line wins over the project file, except that the
        // project may always lower `trust_project_builds` and turn off
        // `classifier_tools`.
        if let Some(cli_auto) = cli.and_then(|c| c.auto.as_ref()) {
            let lower_trust = auto.trust_project_builds;
            let tools_off = auto.classifier_tools;
            auto = project_auto_without(auto, cli_auto);
            if lower_trust == Some(false) {
                auto.trust_project_builds = Some(false);
            }
            if tools_off == Some(false) {
                auto.classifier_tools = Some(false);
            }
        }
        apply_auto(&mut settings, &mut mode, &auto);
        if classifier_off && !classifier_off_by_cli && mode == PermissionMode::Auto {
            mode = PermissionMode::Manual;
        }
    }
    let (policy, clamped) = CompactionPolicy {
        mode: settings.compact,
        warn_percent: settings.compact_warn_percent,
        auto_percent: settings.compact_auto_percent,
    }
    .normalized();
    if clamped {
        notices.push(Notice::CompactPercentClamped {
            configured: (settings.compact_warn_percent, settings.compact_auto_percent),
            applied: (policy.warn_percent, policy.auto_percent),
        });
        settings.compact_warn_percent = policy.warn_percent;
        settings.compact_auto_percent = policy.auto_percent;
    }
    Ok(Layered {
        rules,
        settings,
        mode,
        endpoints,
        pricing,
        pricing_fetch,
        thinking,
        confinement,
        helpers,
        shell,
        ui,
        daemon_principal,
        notices,
    })
}

/// `project` with every field `cli` sets cleared.
fn project_auto_without(project: AutoSection, cli: &AutoSection) -> AutoSection {
    fn keep<T>(value: Option<T>, overridden: bool) -> Option<T> {
        if overridden { None } else { value }
    }
    AutoSection {
        enabled: keep(project.enabled, cli.enabled.is_some() || cli.mode.is_some()),
        mode: keep(project.mode, cli.enabled.is_some() || cli.mode.is_some()),
        classifier_model: keep(project.classifier_model, cli.classifier_model.is_some()),
        trust_project_builds: keep(
            project.trust_project_builds,
            cli.trust_project_builds.is_some(),
        ),
        compact: keep(project.compact, cli.compact.is_some()),
        compact_warn_percent: keep(
            project.compact_warn_percent,
            cli.compact_warn_percent.is_some(),
        ),
        compact_auto_percent: keep(
            project.compact_auto_percent,
            cli.compact_auto_percent.is_some(),
        ),
        classifier_tools: keep(project.classifier_tools, cli.classifier_tools.is_some()),
    }
}

fn apply_auto(settings: &mut AutoSettings, mode: &mut PermissionMode, auto: &AutoSection) {
    if let Some(enabled) = auto.enabled {
        *mode = if enabled {
            PermissionMode::Auto
        } else {
            PermissionMode::Manual
        };
    }
    if let Some(set) = auto.mode {
        *mode = set;
    }
    if let Some(model) = &auto.classifier_model {
        settings.classifier_model = Some(model.clone());
    }
    if let Some(trust) = auto.trust_project_builds {
        settings.trust_project_builds = trust;
    }
    if let Some(mode) = auto.compact {
        settings.compact = mode;
    }
    if let Some(percent) = auto.compact_warn_percent {
        settings.compact_warn_percent = percent;
    }
    if let Some(percent) = auto.compact_auto_percent {
        settings.compact_auto_percent = percent;
    }
    if let Some(tools) = auto.classifier_tools {
        settings.classifier_tools = tools;
    }
}
