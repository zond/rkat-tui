//! Loading and merging the `tui.toml` layers (see the [module docs](super)).

use super::file::*;
use super::settings::CompactionPolicy;
use crate::approval::mode::PermissionMode;
use crate::approval::rules::{Rule, RuleKind, RuleOrigin, RuleSet, RulesError};
use crate::bootstrap::confinement::ConfinementSection;
use crate::thinking::ThinkingSetting;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Non-fatal observations from loading rules, shown to the user once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The project-level file contained allow rules, which are ignored.
    ProjectAllowIgnored {
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
        path: PathBuf,
        /// The model it named.
        model: String,
    },
    /// The project-level file listed `[auto] allow` statements; only the
    /// user's own layers say which kinds of action they allow.
    ProjectStatementsIgnored {
        path: PathBuf,
        /// Number of ignored statements.
        count: usize,
    },
    /// An `[auto] allow` statement of a user layer was not taken.
    StatementIgnored {
        /// The statement as written.
        text: String,
        reason: String,
    },
    /// The project-level file listed `[auto] trusted_hosts`; only the
    /// user's own layers say which hosts they trust with the project.
    ProjectTrustedHostsIgnored {
        path: PathBuf,
        /// Number of ignored hosts.
        count: usize,
    },
    /// An `[auto] trusted_hosts` entry of a user layer was not taken.
    TrustedHostIgnored {
        /// The entry as written.
        text: String,
        reason: String,
    },
    /// A configured endpoint is invalid and was skipped.
    EndpointSkipped {
        /// Endpoint name.
        name: String,
        reason: String,
    },
    /// The project file defines an endpoint. Project files never define
    /// endpoints (a cloned repository could route prompts and keys to its
    /// own server): it is ignored, never used as a default or for the
    /// classifier.
    ProjectEndpointIgnored {
        /// Project rules file.
        path: PathBuf,
        name: String,
    },
    /// A file the user does not own (the project file, or an untrusted
    /// realm file) set `[pricing]`; only user layers may.
    PricingIgnored {
        path: PathBuf,
        /// Number of ignored model entries.
        count: usize,
    },
    /// The project file sets `[confinement]` grants. Only the user's own
    /// layers may widen the shell's sandbox: they are ignored.
    ProjectConfinementIgnored {
        path: PathBuf,
        /// Number of ignored grants (directories).
        count: usize,
    },
    /// A file the user does not own (the project file) set `[thinking]`;
    /// only user layers may (a thinking budget is billed).
    ThinkingIgnored {
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
    /// `helpers.model` names a model `helpers.allow_models` or
    /// `deny_models` refuses (the text names both settings).
    HelperModelRefused(String),
    /// The project-level file set `[instructions]`; only the user's own
    /// layers put text in the agents' prompt.
    ProjectInstructionsIgnored {
        /// Project rules file.
        path: PathBuf,
    },
    /// A file of `instructions.files` is missing, unreadable or empty: the
    /// prompts are built without it.
    InstructionFileSkipped {
        /// The file.
        path: PathBuf,
        why: String,
    },
    /// The user's or the workspace's file sets a key the command line
    /// gives with `--replace`: the file's value is not used.
    Replaced {
        /// The key, as written on the command line.
        key: String,
        path: PathBuf,
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
            Self::ProjectStatementsIgnored { path, count } => write!(
                f,
                "ignored {count} [auto] allow statement(s) in {}: only your own tui.toml says which kinds of action you allow",
                path.display()
            ),
            Self::StatementIgnored { text, reason } => {
                write!(f, "ignored [auto] allow statement {text:?}: {reason}")
            }
            Self::ProjectTrustedHostsIgnored { path, count } => write!(
                f,
                "ignored {count} [auto] trusted_hosts entr{} in {}: only your own tui.toml or this workspace's says which hosts you trust with the project",
                if *count == 1 { "y" } else { "ies" },
                path.display()
            ),
            Self::TrustedHostIgnored { text, reason } => {
                write!(f, "ignored [auto] trusted_hosts entry {text:?}: {reason}")
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
            Self::HelperModelRefused(text) => f.write_str(text),
            Self::ProjectInstructionsIgnored { path } => write!(
                f,
                "ignored [instructions] in {}: only your own tui.toml files (or --set) add instruction files",
                path.display()
            ),
            Self::InstructionFileSkipped { path, why } => write!(
                f,
                "instructions.files: {} is skipped ({why}); new conversations and helpers are built without it",
                path.display()
            ),
            Self::Replaced { key, path } => write!(
                f,
                "{key} in {} is not used: the command line gives it with --replace, which drops every file's value of it while this daemon runs",
                path.display()
            ),
        }
    }
}

/// Everything `tui.toml` configures, after layering.
#[derive(Debug, Clone)]
pub struct Layered {
    /// Merged allow/deny rules.
    pub rules: RuleSet,
    /// Merged `[auto]` settings.
    pub settings: AutoSettings,
    /// The permission mode at start (`[auto] mode`; `--no-auto` is
    /// [`PermissionMode::Manual`]). The approval engine
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
    /// Merged `[confinement]` grants of the user layers (global, the realm
    /// file, the command-line overlay), in layer order; not yet
    /// validated (see [`crate::bootstrap::confinement::grants_with`]).
    pub confinement: ConfinementSection,
    /// Merged `[helpers]` settings.
    pub helpers: HelperSettings,
    /// Merged `[shell]` settings.
    pub shell: ShellSettings,
    /// Merged `[search]` limits.
    pub search: crate::tools::SearchLimits,
    /// Merged `[ui]` settings.
    pub ui: UiSettings,
    /// `[daemon] principal` of the user layers (the last one set).
    pub daemon_principal: Option<String>,
    /// Non-fatal observations to show once.
    pub notices: Vec<Notice>,
    /// Every layer's raw table, as read: what each scalar setting
    /// ([`super::registry::SETTINGS`]) resolves from.
    pub layers: super::registry::Layers,
}

/// Load and merge the three rule layers.
///
/// Same as [`load_layered_with`] without a command-line layer, returning
/// only rules, settings and notices.
#[cfg(any(test, feature = "test-support"))]
pub fn load_layered(
    realm_dir: &Path,
    global_rules_file: Option<&Path>,
    project_rkat_dir: &Path,
) -> Result<(RuleSet, AutoSettings, Vec<Notice>), RulesError> {
    let layered = load_layered_with(realm_dir, global_rules_file, project_rkat_dir, None)?;
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
/// The project layer contributes deny rules and `[auto]` settings the
/// command line did not set; its endpoints and `[confinement]` grants are
/// ignored with a notice; it can never turn
/// `trust_project_builds` or `classifier_tools` on, but may turn them off,
/// and never names the `classifier_model`.
#[cfg(any(test, feature = "test-support"))]
pub fn load_layered_with(
    realm_dir: &Path,
    global_rules_file: Option<&Path>,
    project_rkat_dir: &Path,
    cli: Option<&RulesFile>,
) -> Result<Layered, RulesError> {
    load_layered_replacing(realm_dir, global_rules_file, project_rkat_dir, cli, &[])
}

/// [`load_layered_with`] where the command line gives the keys of
/// `replaced` with `--replace`: the user's and the workspace's files are
/// read without them (a notice names each one a file held), so the
/// command line's value stands alone; [`Layered::layers`] keeps the keys
/// for the daemon's syncs.
///
/// # Errors
///
/// As [`load_layered_with`].
pub fn load_layered_replacing(
    realm_dir: &Path,
    global_rules_file: Option<&Path>,
    project_rkat_dir: &Path,
    cli: Option<&RulesFile>,
    replaced: &[super::registry::KeyPath],
) -> Result<Layered, RulesError> {
    let mut layered = Layered::of_defaults(Typed::defaults()?);
    layered.layers.replaced = replaced.to_vec();
    let global = match global_rules_file {
        Some(path) => Some(layered.load_user_file(path)?),
        None => None,
    };
    let realm = layered.load_user_file(&realm_dir.join(RULES_FILE_NAME))?;
    let mut user_layers: Vec<(RuleOrigin, &RulesFile)> = Vec::new();
    if let Some(global) = &global {
        layered.layers.user = global.table();
        user_layers.push((RuleOrigin::Global, global));
    }
    layered.layers.workspace = realm.table();
    user_layers.push((RuleOrigin::Realm, &realm));
    if let Some(cli) = cli {
        layered.layers.cli = cli.table();
        user_layers.push((RuleOrigin::Cli, cli));
    }
    for (origin, file) in &user_layers {
        layered.apply_user_layer(*origin, file)?;
    }
    layered.take_lasting(super::lasting::Lasting::of(&user_layers))?;
    let project_path = project_rkat_dir.join(RULES_FILE_NAME);
    let project = RulesFile::load(&project_path)?;
    layered.layers.project = project.table();
    layered.apply_project_layer(project, project_path)?;
    layered.resolve_settings()?;
    layered.clamp_compaction();
    // Both helper model lists in force refuse the start, as a bad file
    // does; a default the rule refuses is a notice.
    let helper_models = super::helper_models::HelperModels::of(&layered.layers)
        .map_err(RulesError::HelperModels)?;
    if let Some(conflict) = helper_models.default_conflict() {
        layered.notices.push(Notice::HelperModelRefused(conflict));
    }
    // An instruction file that will not load is said once here; the
    // session builds skip it.
    for path in super::registry::instruction_files_in_force(&layered.layers) {
        if let Some(why) = crate::bootstrap::instructions::unloadable(&path) {
            layered
                .notices
                .push(Notice::InstructionFileSkipped { path, why });
        }
    }
    Ok(layered)
}

/// Every scalar setting in force as the typed views the code reads:
/// [`super::registry::merged`] over the layers, deserialized. The registry
/// is the only statement of a default; the views have none of their own
/// (no `Default`), and [`Self::defaults`] is the registry's defaults read
/// as them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typed {
    /// `[auto]`'s settings (its `allow` empty: a section, which the
    /// layering adds).
    pub settings: AutoSettings,
    /// `[auto] mode`.
    pub mode: PermissionMode,
    /// `[helpers]`'s scalars.
    pub helpers: HelperSettings,
    /// `[shell]`'s.
    pub shell: ShellSettings,
    /// `[search]`'s limits.
    pub search: crate::tools::SearchLimits,
    /// `[ui]`'s.
    pub ui: UiSettings,
    /// `[pricing] fetch`.
    pub pricing_fetch: bool,
    /// `[daemon] principal`.
    pub daemon_principal: Option<String>,
}

impl Typed {
    /// The settings in force over `layers`.
    ///
    /// # Errors
    ///
    /// [`RulesError::Scalars`] when a value in force does not read as its
    /// type (each layer was read as its type already, and every default
    /// is one its setting takes, so this is a bug).
    pub fn of(layers: &super::registry::Layers) -> Result<Self, RulesError> {
        let Scalars {
            auto,
            helpers,
            shell,
            search,
            ui,
            pricing,
            daemon,
        } = super::registry::merged(layers)
            .try_into()
            .map_err(RulesError::Scalars)?;
        Ok(Self {
            settings: auto.settings,
            mode: auto.mode,
            helpers,
            shell,
            search: crate::tools::SearchLimits {
                budget: std::time::Duration::from_secs(search.budget_secs),
                max_entries: search.max_entries,
            },
            ui,
            pricing_fetch: pricing.fetch,
            daemon_principal: daemon.principal,
        })
    }

    /// The registry's defaults: the settings in force when no layer sets
    /// any.
    ///
    /// # Errors
    ///
    /// As [`Self::of`]: a bug in the registry's table.
    pub fn defaults() -> Result<Self, RulesError> {
        Self::of(&super::registry::Layers::default())
    }
}

/// [`Typed`] as the merged table holds it.
#[derive(Deserialize)]
struct Scalars {
    auto: AutoScalars,
    helpers: HelperSettings,
    shell: ShellSettings,
    search: SearchScalars,
    ui: UiSettings,
    pricing: PricingScalars,
    #[serde(default)]
    daemon: DaemonSection,
}

/// `[auto]`'s scalars: the mode beside the auto settings.
#[derive(Deserialize)]
struct AutoScalars {
    mode: PermissionMode,
    #[serde(flatten)]
    settings: AutoSettings,
}

/// `[search]`'s scalars, as the file holds them.
#[derive(Deserialize)]
struct SearchScalars {
    budget_secs: u64,
    max_entries: u64,
}

/// `[pricing]`'s scalar.
#[derive(Deserialize)]
struct PricingScalars {
    fetch: bool,
}

impl Layered {
    /// No layer yet: the registry's defaults (`typed`), nothing else.
    fn of_defaults(typed: Typed) -> Self {
        let Typed {
            settings,
            mode,
            helpers,
            shell,
            search,
            ui,
            pricing_fetch,
            daemon_principal,
        } = typed;
        Self {
            rules: RuleSet::default(),
            settings,
            mode,
            endpoints: BTreeMap::new(),
            pricing: BTreeMap::new(),
            pricing_fetch,
            thinking: BTreeMap::new(),
            confinement: ConfinementSection::default(),
            helpers,
            shell,
            search,
            ui,
            daemon_principal,
            notices: Vec::new(),
            layers: super::registry::Layers::default(),
        }
    }

    /// Take the lasting permissions of the user's own layers
    /// ([`super::lasting::Lasting`]): their rules, statements and
    /// directories. A rule that does not parse refuses the start; a
    /// statement not taken is a notice.
    ///
    /// # Errors
    ///
    /// The first rule's [`RulesError`].
    fn take_lasting(&mut self, lasting: super::lasting::Lasting) -> Result<(), RulesError> {
        use super::lasting::Skipped;
        for skipped in lasting.skipped {
            match skipped {
                Skipped::Rule { error, .. } => return Err(error),
                Skipped::Statement { text, refusal, .. } => {
                    self.notices.push(Notice::StatementIgnored {
                        text,
                        reason: refusal.to_string(),
                    });
                }
                Skipped::TrustedHost { text, why, .. } => {
                    self.notices
                        .push(Notice::TrustedHostIgnored { text, reason: why });
                }
            }
        }
        for rule in lasting.rules {
            self.rules.push(rule);
        }
        self.settings.allow = lasting.statements;
        self.settings.trusted_hosts = lasting.trusted_hosts;
        self.confinement = lasting.confinement;
        Ok(())
    }

    /// Apply one of the user's own layers (global, realm, command line)
    /// over the ones before it: its prices, thinking and endpoints (its
    /// permissions are [`Self::take_lasting`]'s).
    fn apply_user_layer(&mut self, origin: RuleOrigin, file: &RulesFile) -> Result<(), RulesError> {
        if let Some((name, _)) = file
            .endpoints
            .iter()
            .find(|(_, section)| section.models.as_ref().is_some_and(Vec::is_empty))
        {
            return Err(RulesError::EmptyModels {
                endpoint: name.clone(),
                place: match origin {
                    RuleOrigin::Global => "your tui.toml",
                    RuleOrigin::Realm => "this workspace's tui.toml",
                    RuleOrigin::Cli | RuleOrigin::Project | RuleOrigin::Session => {
                        "the command line"
                    }
                }
                .to_string(),
            });
        }
        for (model, section) in &file.pricing {
            self.pricing
                .entry(model.clone())
                .or_default()
                .merge_from(section);
        }
        // A later layer's setting replaces an earlier one's (`"default"`
        // undoes it).
        self.thinking.extend(
            file.thinking
                .iter()
                .map(|(key, setting)| (key.clone(), *setting)),
        );
        for (name, section) in &file.endpoints {
            self.endpoints
                .entry(name.clone())
                .and_modify(|layered| layered.section.merge_from(section))
                .or_insert_with(|| LayeredEndpoint {
                    section: section.clone(),
                    origin,
                });
        }
        Ok(())
    }

    /// The user's or the workspace's file at `path`, read without the keys
    /// the command line gives with `--replace` (each one the file held is a
    /// notice).
    ///
    /// # Errors
    ///
    /// As [`RulesFile::load`].
    fn load_user_file(&mut self, path: &Path) -> Result<RulesFile, RulesError> {
        let file = RulesFile::load(path)?;
        if self.layers.replaced.is_empty() {
            return Ok(file);
        }
        let (table, held) = super::registry::without_replaced(&file.table(), &self.layers.replaced);
        if held.is_empty() {
            return Ok(file);
        }
        for key in &held {
            self.notices.push(Notice::Replaced {
                key: super::registry::path_words(key),
                path: path.to_path_buf(),
            });
        }
        // What is left of a file that read is one.
        toml::Value::Table(table)
            .try_into()
            .map_err(|source| RulesError::Parse {
                path: path.to_path_buf(),
                source,
            })
    }

    /// Apply the project file (`.rkat/tui.toml`, which a cloned repository
    /// writes): its deny rules; what it may not set is ignored with a
    /// notice (its scalar settings resolve by their [`ProjectRule`]s in
    /// [`Self::resolve_settings`]).
    ///
    /// [`ProjectRule`]: super::registry::ProjectRule
    fn apply_project_layer(
        &mut self,
        project: RulesFile,
        project_path: std::path::PathBuf,
    ) -> Result<(), RulesError> {
        if !project.allow.is_empty() {
            self.notices.push(Notice::ProjectAllowIgnored {
                path: project_path.clone(),
                count: project.allow.len(),
            });
        }
        for entry in &project.deny {
            self.rules.push(Rule::from_entry(
                RuleKind::Deny,
                entry,
                RuleOrigin::Project,
            )?);
        }
        if !project.pricing.models.is_empty() {
            self.notices.push(Notice::PricingIgnored {
                path: project_path.clone(),
                count: project.pricing.len(),
            });
        }
        if !project.thinking.is_empty() {
            self.notices.push(Notice::ThinkingIgnored {
                path: project_path.clone(),
                count: project.thinking.len(),
            });
        }
        if project.daemon.is_some() {
            self.notices.push(Notice::DaemonIgnored {
                path: project_path.clone(),
            });
        }
        // Nor the agents' prompt: a cloned repository could put any text
        // there.
        if project.instructions.is_some() {
            self.notices.push(Notice::ProjectInstructionsIgnored {
                path: project_path.clone(),
            });
        }
        self.pricing
            .retain(|model, section| match section.invalid_field() {
                Some(field) => {
                    self.notices.push(Notice::PricingInvalid {
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
            self.notices.push(Notice::ProjectConfinementIgnored {
                path: project_path.clone(),
                count: project_grants,
            });
        }
        // Project files never define endpoints, not even new ones: a cloned
        // repository could route prompts and credentials to its own server.
        for name in project.endpoints.keys() {
            self.notices.push(Notice::ProjectEndpointIgnored {
                path: project_path.clone(),
                name: name.clone(),
            });
        }
        if let Some(auto) = project.auto {
            // Nor the kinds of action the user allows: a cloned repository
            // could word one that clears what it wants.
            if !auto.allow.is_empty() {
                self.notices.push(Notice::ProjectStatementsIgnored {
                    path: project_path.clone(),
                    count: auto.allow.len(),
                });
            }
            // Nor the hosts the project's data may go to: a cloned
            // repository could name its own.
            if !auto.trusted_hosts.is_empty() {
                self.notices.push(Notice::ProjectTrustedHostsIgnored {
                    path: project_path.clone(),
                    count: auto.trusted_hosts.len(),
                });
            }
            // It can only lower trust (`false` applies).
            if auto.trust_project_builds == Some(true) {
                self.notices.push(Notice::ProjectTrustIgnored {
                    path: project_path.clone(),
                });
            }
            // Nor the model that reviews the agent's actions: a cloned
            // repository could pick one that is cheap to talk past.
            if let Some(model) = auto.classifier_model {
                self.notices.push(Notice::ProjectClassifierModelIgnored {
                    path: project_path,
                    model,
                });
            }
        }
        Ok(())
    }

    /// Resolve every scalar setting over the layers and store it in the
    /// typed view: one rule per setting, declared in
    /// [`super::registry::SETTINGS`] (a project's value as its rule says:
    /// the mode it cannot pick, `trust_project_builds`, `classifier_tools`
    /// and `pricing.fetch` it may only turn off, the limits it may only
    /// lower, the classifier model and `[daemon]` it may not set).
    ///
    /// # Errors
    ///
    /// [`RulesError::Scalars`] when a value in force does not read as its
    /// type (each layer was read as its type already, so this is a bug).
    fn resolve_settings(&mut self) -> Result<(), RulesError> {
        let Typed {
            settings,
            mode,
            helpers,
            shell,
            search,
            ui,
            pricing_fetch,
            daemon_principal,
        } = Typed::of(&self.layers)?;
        let allow = std::mem::take(&mut self.settings.allow);
        let trusted_hosts = std::mem::take(&mut self.settings.trusted_hosts);
        self.settings = AutoSettings {
            allow,
            trusted_hosts,
            ..settings
        };
        self.mode = mode;
        self.helpers = helpers;
        self.shell = shell;
        self.search = search;
        self.ui = ui;
        self.pricing_fetch = pricing_fetch;
        self.daemon_principal = daemon_principal;
        Ok(())
    }

    /// Bring the compaction percentages into range, with a notice when
    /// they were not.
    fn clamp_compaction(&mut self) {
        let (policy, clamped) = CompactionPolicy {
            mode: self.settings.compact,
            warn_percent: self.settings.compact_warn_percent,
            auto_percent: self.settings.compact_auto_percent,
        }
        .normalized();
        if clamped {
            self.notices.push(Notice::CompactPercentClamped {
                configured: (
                    self.settings.compact_warn_percent,
                    self.settings.compact_auto_percent,
                ),
                applied: (policy.warn_percent, policy.auto_percent),
            });
            self.settings.compact_warn_percent = policy.warn_percent;
            self.settings.compact_auto_percent = policy.auto_percent;
        }
    }
}
