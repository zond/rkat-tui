//! `rkat/set_setting` and `rkat/settings`: the one path a change of a
//! setting takes in the daemon, and each setting's hook.

use super::*;
use crate::config::registry::{Hook, Level, Setting};
use crate::daemon::protocol::{Outcome, SetSettingParams, SettingNote, SettingsList};
use crate::daemon::settings::Written;
use crate::endpoints::EndpointRef;

impl Local {
    /// `rkat/settings`: every setting as `/settings` lists it.
    pub(super) fn settings_rows(&self) -> SettingsList {
        self.core.settings.rows()
    }

    /// `rkat/set_setting`: check the change, write it to the user's or the
    /// workspace's file, and when the value in force changed apply it
    /// ([`Self::apply_setting`]); audited. The answer is the
    /// confirmation, or why nothing was written.
    pub(super) async fn set_setting(&self, params: SetSettingParams) -> Outcome {
        let SetSettingParams { key, value, user } = params;
        let refused = Outcome::failed::<String>;
        if let Err(why) = self.precheck(&key, value.as_deref()) {
            return refused(why);
        }
        let settings = Arc::clone(&self.core.settings);
        let written =
            tokio::task::spawn_blocking(move || settings.write(&key, value.as_deref(), user))
                .await
                .map_err(|err| err.to_string())
                .and_then(|written| written);
        let written = match written {
            Ok(written) => written,
            Err(why) => return refused(why),
        };
        self.audit_setting(&written);
        let mut warnings = Vec::new();
        let mut text = written.confirmation();
        if written.changed() || written.before.source != written.after.source {
            let (lines, warning) = self
                .apply_setting(written.setting, &written.before, &written.after, true)
                .await;
            for line in lines {
                text.push_str("; ");
                text.push_str(&line);
            }
            warnings.extend(warning);
        }
        Outcome {
            warning: (!warnings.is_empty()).then(|| warnings.join("\n")),
            ..Outcome::ok(text)
        }
    }

    /// The audit line of a written change.
    pub(super) fn audit_setting(&self, written: &Written) {
        let value = written
            .value
            .as_ref()
            .map_or_else(|| "(removed)".to_string(), crate::config::registry::show);
        self.audit(crate::daemon::audit::Line::event(
            "setting",
            format!(
                "{} = {value} at {} ({}); in force: {} from {}",
                written.setting.key,
                written.level.label(),
                written.path.display(),
                crate::daemon::settings::in_force_words(written.setting, &written.after),
                written.after.source.label(),
            ),
        ));
    }

    /// What must hold before a change is written: a classifier model names
    /// an endpoint this daemon has.
    fn precheck(&self, key: &str, value: Option<&str>) -> Result<(), String> {
        let Some(setting) = Setting::find(key.trim()) else {
            return Ok(());
        };
        match setting.hook {
            Hook::ClassifierModel => {}
            Hook::None
            | Hook::Mode
            | Hook::TrustBuilds
            | Hook::Prepare
            | Hook::Ui
            | Hook::HelperModels
            | Hook::CheckpointSteps
            | Hook::HelperLimit
            | Hook::InstructionFiles
            | Hook::WebSearch
            | Hook::WebFetch => {
                return Ok(());
            }
        }
        let Some(value) = value.map(str::trim) else {
            return Ok(());
        };
        let key = setting.key;
        let EndpointRef { endpoint, model } = EndpointRef::parse(value).ok_or_else(|| {
            format!(
                "{key} takes endpoint:model (this daemon has {}); /auto model picks one",
                self.parts.endpoints.names().join(", ")
            )
        })?;
        if model.trim().is_empty() {
            return Err(format!("{key} needs a model after the endpoint"));
        }
        let Some(target) = self.parts.endpoints.get(&endpoint) else {
            return Err(format!(
                "the classifier model was not changed: this daemon has no endpoint `{endpoint}` (it has {}); define it under [endpoints.{endpoint}] in your own tui.toml and restart the daemon, or pick a model of one it has",
                self.parts.endpoints.names().join(", ")
            ));
        };
        target
            .check_model(model.trim())
            .map_err(|err| format!("the classifier model was not changed: {err}"))
    }

    /// The hook of `setting`, run when its value in force (or its layer)
    /// moved from `before` to `after`: what to add to the confirmation, and
    /// a warning. With `push`, a `[ui]` change is pushed for the clients to
    /// follow (a synced file's change is pushed with its notice instead).
    pub(crate) async fn apply_setting(
        &self,
        setting: &'static Setting,
        before: &crate::config::registry::Resolved,
        after: &crate::config::registry::Resolved,
        push: bool,
    ) -> (Vec<String>, Option<String>) {
        let value = after.value.clone();
        let changed = before.value != after.value;
        match setting.hook {
            Hook::Mode if changed => {
                let mode = value
                    .and_then(|value| value.try_into::<PermissionMode>().ok())
                    .unwrap_or_default();
                let warnings = self.switch_mode(mode).await;
                (
                    vec![mode_notice(mode).to_string()],
                    (!warnings.is_empty()).then(|| warnings.join("\n")),
                )
            }
            Hook::ClassifierModel if changed => {
                let reference = value.as_ref().and_then(toml::Value::as_str).unwrap_or("");
                match EndpointRef::parse(reference) {
                    Some(EndpointRef { endpoint, model }) => {
                        let outcome = self.switch_classifier_model(&endpoint, &model).await;
                        if outcome.ok {
                            (vec![outcome.text], None)
                        } else {
                            (Vec::new(), Some(outcome.text))
                        }
                    }
                    None => (
                        Vec::new(),
                        Some(
                            "the classifier keeps its model until the daemon restarts (it reviews with the model in force then)"
                                .to_string(),
                        ),
                    ),
                }
            }
            Hook::TrustBuilds if changed => {
                self.core
                    .engine
                    .set_trust_project_builds(value.as_ref().and_then(toml::Value::as_bool));
                (Vec::new(), None)
            }
            // The source matters too: `/compact status` names it.
            Hook::Prepare => {
                let setting =
                    crate::config::settings::PrepareSetting::of(&self.core.settings.layers());
                let at = self.core.prepare.describe(setting.at.value);
                let line = match &setting.instruction.value {
                    Some(text) => format!("the agents are asked at {at} to: \"{text}\""),
                    None => format!("no instruction is in force; the threshold is {at}"),
                };
                let warning = self.core.prepare.apply(setting.instruction, setting.at);
                (vec![line], warning)
            }
            Hook::Ui if changed && push => {
                self.core.decisions.broadcast(
                    protocol::SETTING,
                    &SettingNote {
                        key: setting.key.to_string(),
                        value: value.as_ref().map(crate::config::registry::show),
                        source: after.source,
                        notice: None,
                    },
                );
                (Vec::new(), None)
            }
            Hook::HelperModels => self.apply_helper_models().await,
            Hook::CheckpointSteps => {
                let steps =
                    crate::config::registry::int_in_force(setting, &self.core.settings.layers());
                self.parts.realm_config.set_turn_limit(steps);
                (
                    vec![format!(
                        "the conversations and helpers started from now on reach a checkpoint after {steps} steps"
                    )],
                    None,
                )
            }
            Hook::HelperLimit => {
                let limit =
                    crate::config::registry::helpers_live_in_force(&self.core.settings.layers());
                self.core.registry.set_max_live(limit);
                (
                    vec![format!(
                        "from now on each agent and each helper may hold {} at once from `assign`; none seated now is retired",
                        crate::text::count(limit, "helper")
                    )],
                    None,
                )
            }
            Hook::InstructionFiles => {
                let files = crate::config::registry::instruction_files_in_force(
                    &self.core.settings.layers(),
                );
                let warnings: Vec<String> = files
                    .iter()
                    .filter_map(|path| {
                        crate::bootstrap::instructions::unloadable(path).map(|why| {
                            crate::config::Notice::InstructionFileSkipped {
                                path: path.clone(),
                                why,
                            }
                            .to_string()
                        })
                    })
                    .collect();
                let count = files.len();
                *self.core.instruction_files.write() = files;
                (
                    vec![format!(
                        "the conversations and fresh helpers started from now on read {}",
                        crate::text::count(count, "instruction file")
                    )],
                    (!warnings.is_empty()).then(|| warnings.join("\n")),
                )
            }
            Hook::WebSearch => {
                let on = crate::config::registry::web_search_in_force(&self.core.settings.layers());
                self.core.web_search.set(on);
                let line = if on {
                    "the conversations and helpers started from now on are built with the engine's web search as it comes (a conversation built with it off keeps it off)"
                } else {
                    "the conversations and helpers started from now on, and a resumed one at its next build, are built with web search off"
                };
                (vec![line.to_string()], None)
            }
            Hook::WebFetch => {
                let on = crate::config::registry::web_fetch_in_force(&self.core.settings.layers());
                self.core.web_fetch.set(on);
                let line = match (on, self.core.web_fetch.shell_network()) {
                    (true, true) => {
                        "the conversations and helpers started from now on, and a resumed one at its next build, get web_fetch"
                    }
                    (true, false) => {
                        "web_fetch stays off: the shell's sandbox denies the network (--confinement strict), and web_fetch would be a way around it"
                    }
                    (false, _) => {
                        "the conversations and helpers started from now on, and a resumed one at its next build, are built without web_fetch"
                    }
                };
                (vec![line.to_string()], None)
            }
            Hook::None | Hook::Mode | Hook::ClassifierModel | Hook::TrustBuilds | Hook::Ui => {
                (Vec::new(), None)
            }
        }
    }

    /// `helpers.model`, `allow_models` or `deny_models` moved: the helpers
    /// spawned from now on take the settings in force, and every live
    /// agent is told once (a file change that moves two of them tells it
    /// once: the second finds nothing new). The confirmation's line, and
    /// a warning when `helpers.model` is one the rule refuses.
    async fn apply_helper_models(&self) -> (Vec<String>, Option<String>) {
        use crate::config::helper_models::HelperModels;
        let now = match HelperModels::of(&self.core.settings.layers()) {
            Ok(now) => now,
            // A write or a sync that would break them is refused before.
            Err(err) => return (Vec::new(), Some(err.to_string())),
        };
        if *self.core.helper_models.read() == now {
            return (Vec::new(), None);
        }
        *self.core.helper_models.write() = now.clone();
        let told = now.instruction();
        self.agents
            .note_everyone(&format!(
                "The user changed the settings of your helpers' models; from now on: {told}"
            ))
            .await;
        (
            vec!["the helpers spawned from now on take it, and the agents were told".to_string()],
            now.default_conflict(),
        )
    }

    /// A settings file of `level` changed on disk (a hand edit, another
    /// daemon's write): read it again ([`crate::daemon::settings::Settings::sync`]);
    /// each setting whose value in force moved runs its hook, is audited
    /// and told to every client ("auto.compact_prepare_at is now 65% (your
    /// tui.toml changed)", with the value for a `[ui]` setting to follow);
    /// the user's or the workspace's permission sections are held again
    /// ([`Self::hold_permissions`], also when the file holds what the
    /// daemon does: the engine follows the file); another changed section,
    /// and any section of the project's file, is named once (it applies at
    /// the next start); a file that does not read is reported once and its
    /// last good values stay.
    pub async fn sync_settings(&self, level: Level) {
        self.sync_settings_once(level).await;
        #[cfg(any(test, feature = "test-support"))]
        self.probe.settings_synced();
    }

    async fn sync_settings_once(&self, level: Level) {
        use crate::daemon::settings::Synced;
        let settings = Arc::clone(&self.core.settings);
        let Ok(synced) = tokio::task::spawn_blocking(move || settings.sync(level)).await else {
            return;
        };
        let file = level.file_words();
        let path = self
            .core
            .settings
            .path_of(level)
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        match synced {
            Synced::StillUnreadable => {}
            // The file holds what the daemon holds; the engine may not
            // (a grant the engine wrote itself, followed so closely by a
            // hand edit undoing it that one read saw both): it follows.
            Synced::Same if level != Level::Project => {
                self.hold_permissions(&format!("{file} changed"));
            }
            Synced::Same => {}
            Synced::Unreadable(why) => {
                self.audit(crate::daemon::audit::Line::event(
                    "settings_unreadable",
                    format!("{path}: {why}"),
                ));
                self.tell_every_client(
                    true,
                    format!(
                        "{file} ({path}) changed but does not read, so its last good settings stay in force: {why}"
                    ),
                );
            }
            Synced::Changed {
                changes,
                permissions,
                later,
                pinned,
            } => {
                for key in pinned {
                    self.audit(crate::daemon::audit::Line::event(
                        "setting_pinned",
                        format!("{key}: {path} changed it; --replace holds it"),
                    ));
                    self.tell_every_client(
                        true,
                        format!(
                            "{key} in {file} ({path}) changed, but is not taken: the daemon's command line gives it with --replace, which holds it until the daemon restarts"
                        ),
                    );
                }
                for change in changes {
                    let words = |resolved: &crate::config::registry::Resolved| {
                        crate::daemon::settings::in_force_words(change.setting, resolved)
                    };
                    let (lines, warning) = self
                        .apply_setting(change.setting, &change.before, &change.after, false)
                        .await;
                    let when = match change.setting.applies {
                        crate::config::registry::Applies::Live => String::new(),
                        crate::config::registry::Applies::NextConversation => {
                            "; it applies from the next conversation".to_string()
                        }
                        crate::config::registry::Applies::NextStart => {
                            "; it applies at the next start".to_string()
                        }
                    };
                    let source = change.after.source.source_words();
                    let mut notice = format!(
                        "{} is now {} ({source}; {file} changed){when}",
                        change.setting.key,
                        words(&change.after)
                    );
                    for line in lines {
                        notice.push_str("; ");
                        notice.push_str(&line);
                    }
                    self.audit(crate::daemon::audit::Line::event(
                        "setting_changed",
                        format!(
                            "{}: {} -> {} ({path} changed)",
                            change.setting.key,
                            words(&change.before),
                            words(&change.after)
                        ),
                    ));
                    self.core.decisions.broadcast(
                        protocol::SETTING,
                        &SettingNote {
                            key: change.setting.key.to_string(),
                            value: change
                                .after
                                .value
                                .as_ref()
                                .map(crate::config::registry::show),
                            source: change.after.source,
                            notice: Some(notice),
                        },
                    );
                    if let Some(warning) = warning {
                        self.tell_every_client(true, warning);
                    }
                }
                if permissions {
                    self.hold_permissions(&format!("{file} changed"));
                }
                if later {
                    self.tell_every_client(
                        false,
                        if level == Level::Project {
                            format!(
                                "the rules, statements, grants, endpoints, prices or thinking of {file} changed: they apply at the next start"
                            )
                        } else {
                            format!(
                                "the endpoints, prices or thinking of {file} changed: they apply at the next start"
                            )
                        },
                    );
                }
            }
        }
    }

    /// Hold the lasting permissions the settings files give now (as held:
    /// [`crate::daemon::settings::Settings::lasting`]), the engine's
    /// replaced from them
    /// ([`crate::approval::ApprovalEngine::replace_lasting`]); `why` is the
    /// cause in words ("your tui.toml changed"). What changed is told to
    /// every client once, with counts, and each item audited; nothing is
    /// told when nothing changed (a command's own write, which the engine
    /// holds already).
    pub(crate) fn hold_permissions(&self, why: &str) {
        if !self.core.settings.has_files() {
            // No files: what the commands did lives in the engine only.
            return;
        }
        // Under the files' lock: an engine change of a rule, grant or
        // statement and its file's write cannot land between the read and
        // the replace (it would be undone here, then redone by the next
        // sync, each told to every client).
        let change = {
            let _still = self.core.settings.still();
            let lasting = self.core.settings.lasting();
            self.core.engine.replace_lasting(&lasting, why)
        };
        if change.is_empty() {
            return;
        }
        let mut text = format!("{why}: {}; in force now", change.counts());
        if change.dirs_added.iter().any(|dir| dir.write) {
            text.push_str(&format!(" ({})", crate::approval::ALWAYS_DIR_LIMIT));
        }
        for skipped in &change.skipped {
            self.audit(crate::daemon::audit::Line::event(
                "permission_skipped",
                format!("{skipped} ({why})"),
            ));
        }
        if !change.skipped.is_empty() {
            text.push_str("; skipped: ");
            text.push_str(&change.skipped.join("; "));
        }
        self.tell_every_client(!change.skipped.is_empty(), text);
    }

    /// After a rule, grant or statement command: hold both files'
    /// sections as written ([`crate::daemon::settings::Settings::adopt_sections`],
    /// blocking file I/O), then the permissions they give, as a file
    /// change would ([`Self::hold_permissions`]: the engine holds what the
    /// command did already, so only a change someone else made meanwhile
    /// is told).
    pub(crate) fn after_permission_command(&self) {
        self.core.settings.adopt_sections(Level::User);
        self.core.settings.adopt_sections(Level::Workspace);
        self.hold_permissions("the settings files changed");
    }

    /// A notice every attached client shows (one per live agent: each
    /// client shows its own agent's).
    fn tell_every_client(&self, warn: bool, text: String) {
        self.core
            .decisions
            .notice_every_agent(self.agents.names(), warn, &text);
    }
}
