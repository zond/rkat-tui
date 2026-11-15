//! `rkat/set_setting` and `rkat/settings`: the one path a change of a
//! setting takes in the daemon, and each setting's hook.

use super::*;
use crate::config::registry::{Level, Setting};
use crate::daemon::protocol::{SetSettingParams, SettingNote, SettingOutcome, SettingsList};
use crate::daemon::settings::Written;

impl Local {
    /// `rkat/settings`: every setting as `/settings` lists it.
    pub(super) fn settings_rows(&self) -> SettingsList {
        self.core.settings.rows()
    }

    /// `rkat/set_setting`: check the change, write it to the user's or the
    /// workspace's file, and when the value in force changed apply it
    /// ([`Self::apply_setting`]); audited. The answer is the
    /// confirmation, or why nothing was written.
    pub(super) async fn set_setting(&self, params: SetSettingParams) -> SettingOutcome {
        let SetSettingParams { key, value, user } = params;
        let refused = |text: String| SettingOutcome {
            ok: false,
            text,
            warning: None,
        };
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
            let (lines, warning) = self.apply_setting(&written).await;
            for line in lines {
                text.push_str("; ");
                text.push_str(&line);
            }
            warnings.extend(warning);
        }
        SettingOutcome {
            ok: true,
            text,
            warning: (!warnings.is_empty()).then(|| warnings.join("\n")),
        }
    }

    /// Record `value` as `key`'s value at `level` without applying it (the
    /// caller applied it live already: a mode switch, Ctrl+T). What went
    /// wrong, if anything, with what the higher layers say.
    pub(super) async fn record_setting(
        &self,
        key: &'static str,
        value: toml::Value,
        level: Level,
    ) -> Option<String> {
        let setting = Setting::find(key)?;
        let settings = Arc::clone(&self.core.settings);
        let written =
            tokio::task::spawn_blocking(move || settings.write_value(setting, Some(value), level))
                .await
                .map_err(|err| err.to_string())
                .and_then(|written| written);
        match written {
            Ok(written) => {
                self.audit_setting(&written);
                // A higher layer wins at the next start: say so.
                (written.after.source > written.level).then(|| written.confirmation())
            }
            Err(why) => Some(format!("it applies to this run but was not saved: {why}")),
        }
    }

    /// The audit line of a written change.
    fn audit_setting(&self, written: &Written) {
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
        if key.trim() != "auto.classifier_model" {
            return Ok(());
        }
        let Some(value) = value.map(str::trim) else {
            return Ok(());
        };
        let (endpoint, model) = value.split_once(':').ok_or_else(|| {
            format!(
                "auto.classifier_model takes endpoint:model (this daemon has {}); /auto model picks one",
                self.parts.endpoints.names().join(", ")
            )
        })?;
        if model.trim().is_empty() {
            return Err("auto.classifier_model needs a model after the endpoint".to_string());
        }
        if self.parts.endpoints.get(endpoint).is_none() {
            return Err(format!(
                "the classifier model was not changed: this daemon has no endpoint `{endpoint}` (it has {}); define it under [endpoints.{endpoint}] in your own tui.toml and restart the daemon, or pick a model of one it has",
                self.parts.endpoints.names().join(", ")
            ));
        }
        Ok(())
    }

    /// The hook of `written`'s setting, run when the value in force (or
    /// its layer) changed: what to add to the confirmation, and a warning.
    pub(crate) async fn apply_setting(&self, written: &Written) -> (Vec<String>, Option<String>) {
        let value = written.after.value.clone();
        match written.setting.key {
            "auto.mode" if written.changed() => {
                let mode = value
                    .and_then(|value| value.try_into::<PermissionMode>().ok())
                    .unwrap_or_default();
                let warnings = self.switch_mode(mode).await;
                (
                    vec![mode_notice(mode).to_string()],
                    (!warnings.is_empty()).then(|| warnings.join("\n")),
                )
            }
            "auto.classifier_model" if written.changed() => {
                let reference = value.as_ref().and_then(toml::Value::as_str).unwrap_or("");
                match reference.split_once(':') {
                    Some((endpoint, model)) => {
                        let outcome = self.switch_classifier_model(endpoint, model).await;
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
            "auto.trust_project_builds" if written.changed() => {
                self.core
                    .engine
                    .set_trust_project_builds(value.as_ref().and_then(toml::Value::as_bool));
                (Vec::new(), None)
            }
            "auto.compact_prepare" | "auto.compact_prepare_at" => {
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
            "ui.helpers" | "ui.bell" | "ui.checklist" if written.changed() => {
                self.core.decisions.broadcast(
                    protocol::SETTING,
                    &SettingNote {
                        key: written.setting.key.to_string(),
                        value: value.as_ref().map(crate::config::registry::show),
                        source: written.after.source,
                        notice: None,
                    },
                );
                (Vec::new(), None)
            }
            _ => (Vec::new(), None),
        }
    }
}
