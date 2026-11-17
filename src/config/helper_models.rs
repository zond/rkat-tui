//! Which models helpers run on: `helpers.model` (the default when a spawn
//! names none) and `helpers.allow_models` / `helpers.deny_models` (the
//! patterns of the models a helper may run on; at most one of the two is
//! set across the layers in force).
//!
//! A pattern is an exact model name or a prefix ending in one `*`
//! (`claude-haiku-*`). It is matched against the model name as the
//! helper's endpoint serves it: the plain name, without an `endpoint:`
//! prefix (a helper runs through its owner's endpoint, so the endpoint is
//! not the pattern's to choose). The customizer
//! ([`crate::mobs::customizer`]) applies [`HelperModels`] to every spawn.

use std::fmt;

use serde::{Deserialize, Serialize};
use toml::Value;

use super::registry::{Layers, Level, Setting};

/// The default model's key.
pub const MODEL_KEY: &str = "helpers.model";
/// The allow list's key.
pub const ALLOW_KEY: &str = "helpers.allow_models";
/// The deny list's key.
pub const DENY_KEY: &str = "helpers.deny_models";

/// One model pattern: an exact name, or a prefix ending in one `*`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelPattern(String);

impl ModelPattern {
    /// `text` as a pattern.
    ///
    /// # Errors
    ///
    /// What is wrong with it, naming the entry: empty, whitespace, or a `*`
    /// anywhere but at the end (or more than one).
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Err("a model pattern is empty: name a model, or a prefix ending in `*`".into());
        }
        if text.contains(char::is_whitespace) {
            return Err(format!(
                "the model pattern `{text}` holds whitespace: name a model, or a prefix ending in `*`"
            ));
        }
        let stars = text.matches('*').count();
        if stars > 1 || (stars == 1 && !text.ends_with('*')) {
            return Err(format!(
                "the model pattern `{text}` has a `*` that is not its one last character: a pattern is a model name, or a prefix ending in one `*` (`claude-haiku-*`)"
            ));
        }
        if text == "*" {
            return Err(
                "the model pattern `*` matches every model: unset the list instead".to_string(),
            );
        }
        Ok(Self(text.to_string()))
    }

    /// Whether `model` (a plain model name) matches.
    #[must_use]
    pub fn matches(&self, model: &str) -> bool {
        match self.0.strip_suffix('*') {
            Some(prefix) => model.starts_with(prefix),
            None => model == self.0,
        }
    }

    /// The pattern as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ModelPattern {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<ModelPattern> for String {
    fn from(pattern: ModelPattern) -> Self {
        pattern.0
    }
}

impl fmt::Display for ModelPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A list setting's value checked: every entry a pattern, at least one.
///
/// # Errors
///
/// What is wrong, naming `key` and the entry.
pub fn check_patterns(key: &str, value: &Value) -> Result<Vec<ModelPattern>, String> {
    let Value::Array(items) = value else {
        return Err(format!(
            "{key} takes a list of model names or prefixes ending in `*` (`[\"claude-haiku-*\"]`)"
        ));
    };
    if items.is_empty() {
        return Err(format!(
            "{key} is an empty list: name at least one model, or unset it"
        ));
    }
    items
        .iter()
        .map(|item| match item {
            Value::String(text) => ModelPattern::parse(text).map_err(|why| format!("{key}: {why}")),
            other => Err(format!(
                "{key}: the entry {other} is not a model name (a string)"
            )),
        })
        .collect()
}

/// `text` as typed in `/set`: a TOML list (`["a", "b*"]`) or names
/// separated by commas (`a, b*`).
#[must_use]
pub fn list_value(text: &str) -> Value {
    let text = text.trim();
    if text.starts_with('[')
        && let Ok(table) = format!("v = {text}").parse::<toml::Table>()
        && let Some(value) = table.get("v")
    {
        return value.clone();
    }
    Value::Array(
        text.split(',')
            .map(|part| Value::String(part.trim().to_string()))
            .collect(),
    )
}

/// Which models a helper may run on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ModelRule {
    /// Any (neither list is set).
    #[default]
    Any,
    /// Only the models these match (`helpers.allow_models`).
    Allow(Vec<ModelPattern>),
    /// Any but the models these match (`helpers.deny_models`).
    Deny(Vec<ModelPattern>),
}

impl ModelRule {
    /// Whether `model` (a plain model name) may run a helper.
    #[must_use]
    pub fn permits(&self, model: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Allow(patterns) => patterns.iter().any(|p| p.matches(model)),
            Self::Deny(patterns) => !patterns.iter().any(|p| p.matches(model)),
        }
    }

    /// The rule in words: `helpers.allow_models = [a, b*]`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Any => "no helpers.allow_models or helpers.deny_models".to_string(),
            Self::Allow(patterns) => format!("{ALLOW_KEY} = [{}]", joined(patterns)),
            Self::Deny(patterns) => format!("{DENY_KEY} = [{}]", joined(patterns)),
        }
    }
}

/// The patterns, comma-separated.
fn joined(patterns: &[ModelPattern]) -> String {
    patterns
        .iter()
        .map(ModelPattern::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where a helper's model came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    /// The spawn named it (a model override, an inline or realm profile).
    Named,
    /// `helpers.model`.
    Default,
    /// The owner's own model.
    Owner,
}

/// The settings in force, shared between the daemon's settings hook (which
/// replaces them) and the spawn customizer (which reads them per spawn).
pub type SharedHelperModels = std::sync::Arc<parking_lot::RwLock<HelperModels>>;

/// The three settings in force.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelperModels {
    /// `helpers.model` (`None`: the owner's model).
    pub default: Option<String>,
    /// The allow or deny list.
    pub rule: ModelRule,
}

/// The settings in force do not make a rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HelperModelsError {
    /// Both lists are set in the layers in force.
    #[error(
        "{ALLOW_KEY} (set in {allow_in}) and {DENY_KEY} (set in {deny_in}) are both set: at most one of them may be, so remove one"
    )]
    BothLists {
        /// Where the allow list comes from, in words.
        allow_in: &'static str,
        /// Where the deny list comes from, in words.
        deny_in: &'static str,
    },
    /// A list in force is not a list of patterns (an empty one).
    #[error("{0}")]
    Invalid(String),
}

fn setting(key: &str) -> Option<&'static Setting> {
    Setting::find(key)
}

impl HelperModels {
    /// The settings in force over `layers`.
    ///
    /// # Errors
    ///
    /// [`HelperModelsError::BothLists`] when both lists are in force (in
    /// one file or across them), naming both keys and where each comes
    /// from; [`HelperModelsError::Invalid`] for an empty list.
    pub fn of(layers: &Layers) -> Result<Self, HelperModelsError> {
        let resolve = |key| setting(key).map(|setting| setting.resolve(layers));
        let default = resolve(MODEL_KEY)
            .and_then(|resolved| resolved.value)
            .and_then(|value| value.as_str().map(str::to_string))
            .filter(|model| !model.trim().is_empty());
        let list = |key| -> Result<Option<(Vec<ModelPattern>, Level)>, HelperModelsError> {
            let Some(resolved) = resolve(key) else {
                return Ok(None);
            };
            let Some(value) = resolved.value.as_ref() else {
                return Ok(None);
            };
            let patterns = check_patterns(key, value).map_err(HelperModelsError::Invalid)?;
            Ok(Some((patterns, resolved.source)))
        };
        let rule = match (list(ALLOW_KEY)?, list(DENY_KEY)?) {
            (Some((_, allow)), Some((_, deny))) => {
                return Err(HelperModelsError::BothLists {
                    allow_in: allow.file_words(),
                    deny_in: deny.file_words(),
                });
            }
            (Some((patterns, _)), None) => ModelRule::Allow(patterns),
            (None, Some((patterns, _))) => ModelRule::Deny(patterns),
            (None, None) => ModelRule::Any,
        };
        Ok(Self { default, rule })
    }

    /// `helpers.model` set to a model the rule does not permit: the
    /// configuration error, naming both settings (`plain`: its model as
    /// the rule reads it, an `endpoint:` prefix removed).
    #[must_use]
    pub fn default_conflict(&self) -> Option<String> {
        let default = self.default.as_deref()?;
        let plain = default.split_once(':').map_or(default, |(_, model)| model);
        if self.rule.permits(plain) || self.rule.permits(default) {
            return None;
        }
        Some(format!(
            "{MODEL_KEY} = {default} is not permitted by {}: helpers that name no model are refused until one of the two settings changes",
            self.rule.describe()
        ))
    }

    /// Why a helper on `model` (a plain name, from `source`) is refused,
    /// in words the agent can act on; `None`: it is permitted.
    #[must_use]
    pub fn refusal(&self, model: &str, source: ModelSource) -> Option<String> {
        if self.rule.permits(model) {
            return None;
        }
        let what = match &self.rule {
            ModelRule::Any => return None,
            ModelRule::Allow(patterns) => format!("name one of: {}", joined(patterns)),
            ModelRule::Deny(patterns) => {
                format!("name a model other than {}", joined(patterns))
            }
        };
        let rule = self.rule.describe();
        Some(match source {
            ModelSource::Named => format!(
                "the model `{model}` is not permitted for helpers by the user's {rule}; {what}"
            ),
            ModelSource::Default => format!(
                "this spawn names no model, and the default helper model `{model}` ({MODEL_KEY}) is not permitted by the user's {rule}; the call must name a permitted model: {what}"
            ),
            ModelSource::Owner => format!(
                "this spawn names no model, so the helper would run on your own model `{model}`, which is not permitted for helpers by the user's {rule}; the call must name a permitted model: {what}"
            ),
        })
    }

    /// The one line an agent that can spawn helpers is told: where a
    /// helper's model comes from (a mob profile's own model never decides),
    /// and the user's settings when any is set.
    #[must_use]
    pub fn instruction(&self) -> String {
        let mut line = match &self.default {
            Some(model) => format!(
                "Helper models: a helper runs on the model its spawn names (a mob profile's own model is ignored); one that names none runs on `{model}` (the user's helpers.model); a fork stays on your model."
            ),
            None => "Helper models: a helper runs on the model its spawn names (a mob profile's own model is ignored), else on your own.".to_string(),
        };
        let permitted = match &self.rule {
            ModelRule::Any => return line,
            ModelRule::Allow(patterns) => format!("only on models matching {}", joined(patterns)),
            ModelRule::Deny(patterns) => {
                format!("on any model but those matching {}", joined(patterns))
            }
        };
        line.push_str(&format!(
            " The user's settings let helpers run {permitted} (a trailing `*` is a prefix); a spawn on another model is refused."
        ));
        line
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::registry::put;

    fn patterns(list: &[&str]) -> Vec<ModelPattern> {
        list.iter()
            .map(|p| ModelPattern::parse(p).unwrap())
            .collect()
    }

    #[test]
    fn a_pattern_is_a_name_or_a_prefix_ending_in_one_star() {
        let exact = ModelPattern::parse("claude-sonnet-4-6").unwrap();
        assert!(exact.matches("claude-sonnet-4-6"));
        assert!(!exact.matches("claude-sonnet-4-6-x"));
        let prefix = ModelPattern::parse("claude-haiku-*").unwrap();
        assert!(prefix.matches("claude-haiku-4-5"));
        assert!(prefix.matches("claude-haiku-"));
        assert!(!prefix.matches("claude-sonnet-4-6"));
        for (bad, says) in [
            ("", "empty"),
            ("claude haiku", "`claude haiku` holds whitespace"),
            (" claude", "whitespace"),
            ("*-haiku", "`*-haiku` has a `*`"),
            ("claude-*-4", "`claude-*-4` has a `*`"),
            ("claude-**", "`claude-**` has a `*`"),
            ("*", "matches every model"),
        ] {
            let why = ModelPattern::parse(bad).unwrap_err();
            assert!(why.contains(says), "{bad}: {why}");
        }
    }

    #[test]
    fn a_list_value_is_checked_entry_by_entry() {
        let ok = check_patterns(ALLOW_KEY, &list_value("claude-haiku-*, gpt-5.5")).unwrap();
        assert_eq!(ok, patterns(&["claude-haiku-*", "gpt-5.5"]));
        assert_eq!(
            check_patterns(ALLOW_KEY, &list_value(r#"["a", "b*"]"#)).unwrap(),
            patterns(&["a", "b*"])
        );
        let why = check_patterns(DENY_KEY, &list_value("a, b*c")).unwrap_err();
        assert!(why.starts_with(DENY_KEY) && why.contains("`b*c`"), "{why}");
        let why = check_patterns(DENY_KEY, &Value::Array(Vec::new())).unwrap_err();
        assert!(why.contains("empty list"), "{why}");
        let why = check_patterns(DENY_KEY, &Value::String("a".into())).unwrap_err();
        assert!(why.contains("takes a list"), "{why}");
    }

    fn layers(user: &[(&str, Value)], workspace: &[(&str, Value)]) -> Layers {
        let mut layers = Layers::default();
        for (key, value) in user {
            put(&mut layers.user, key, Some(value.clone()));
        }
        for (key, value) in workspace {
            put(&mut layers.workspace, key, Some(value.clone()));
        }
        layers
    }

    #[test]
    fn both_lists_in_force_is_an_error_naming_both() {
        let allow = list_value("a*");
        let deny = list_value("b");
        // In one file.
        let err = HelperModels::of(&layers(
            &[(ALLOW_KEY, allow.clone()), (DENY_KEY, deny.clone())],
            &[],
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(ALLOW_KEY) && err.contains(DENY_KEY) && err.contains("your tui.toml"),
            "{err}"
        );
        // Across the user's and the workspace's.
        let err = HelperModels::of(&layers(&[(ALLOW_KEY, allow.clone())], &[(DENY_KEY, deny)]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("helpers.allow_models (set in your tui.toml)")
                && err.contains("helpers.deny_models (set in this workspace's tui.toml)"),
            "{err}"
        );
        // A project file may not set them: its list is not in force.
        let mut project = layers(&[(ALLOW_KEY, allow)], &[]);
        put(&mut project.project, DENY_KEY, Some(list_value("b")));
        assert_eq!(
            HelperModels::of(&project).unwrap().rule,
            ModelRule::Allow(patterns(&["a*"]))
        );
    }

    /// At load, both lists refuse the start as a bad file does, and a
    /// default the rule refuses is a notice naming both settings.
    #[test]
    fn the_load_refuses_both_lists_and_notes_a_refused_default() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, realm, project) = (
            tmp.path().join("home.toml"),
            tmp.path().join("realm"),
            tmp.path().join("project"),
        );
        std::fs::create_dir_all(&realm).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(&home, "[helpers]\nallow_models = [\"claude-haiku-*\"]\n").unwrap();
        std::fs::write(
            realm.join(crate::config::RULES_FILE_NAME),
            "[helpers]\ndeny_models = [\"gpt-*\"]\n",
        )
        .unwrap();
        let err = crate::config::load_layered_with(&realm, Some(&home), &project, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains(ALLOW_KEY) && err.contains(DENY_KEY), "{err}");
        std::fs::write(
            realm.join(crate::config::RULES_FILE_NAME),
            "[helpers]\nmodel = \"claude-opus-4-8\"\n",
        )
        .unwrap();
        let layered =
            crate::config::load_layered_with(&realm, Some(&home), &project, None).unwrap();
        assert!(
            layered
                .notices
                .iter()
                .any(|notice| notice.to_string().contains(
                    "helpers.model = claude-opus-4-8 is not permitted by helpers.allow_models"
                )),
            "{:?}",
            layered.notices
        );
    }

    #[test]
    fn the_refusals_say_what_to_do() {
        let allow = HelperModels {
            default: None,
            rule: ModelRule::Allow(patterns(&["claude-haiku-*", "gpt-5.5"])),
        };
        assert_eq!(allow.refusal("claude-haiku-4-5", ModelSource::Named), None);
        let named = allow
            .refusal("claude-opus-4-8", ModelSource::Named)
            .unwrap();
        assert_eq!(
            named,
            "the model `claude-opus-4-8` is not permitted for helpers by the user's helpers.allow_models = [claude-haiku-*, gpt-5.5]; name one of: claude-haiku-*, gpt-5.5"
        );
        let owner = allow
            .refusal("claude-opus-4-8", ModelSource::Owner)
            .unwrap();
        assert!(
            owner.contains("your own model `claude-opus-4-8`")
                && owner.contains(
                    "the call must name a permitted model: name one of: claude-haiku-*, gpt-5.5"
                ),
            "{owner}"
        );
        let deny = HelperModels {
            default: Some("claude-opus-4-8".into()),
            rule: ModelRule::Deny(patterns(&["claude-opus-*"])),
        };
        let default = deny
            .refusal("claude-opus-4-8", ModelSource::Default)
            .unwrap();
        assert!(
            default.contains("the default helper model `claude-opus-4-8` (helpers.model)")
                && default.contains("name a model other than claude-opus-*"),
            "{default}"
        );
        assert_eq!(
            HelperModels::default().refusal("x", ModelSource::Owner),
            None
        );
    }

    #[test]
    fn a_default_the_rule_refuses_is_a_configuration_error() {
        let models = HelperModels {
            default: Some("claude-opus-4-8".into()),
            rule: ModelRule::Allow(patterns(&["claude-haiku-*"])),
        };
        let warning = models.default_conflict().unwrap();
        assert!(
            warning.contains("helpers.model = claude-opus-4-8")
                && warning.contains("helpers.allow_models = [claude-haiku-*]"),
            "{warning}"
        );
        let fine = HelperModels {
            default: Some("anthropic:claude-haiku-4-5".into()),
            ..models
        };
        assert_eq!(fine.default_conflict(), None);
    }

    #[test]
    fn the_agents_are_told_where_a_helper_s_model_comes_from() {
        let plain = HelperModels::default().instruction();
        assert_eq!(
            plain,
            "Helper models: a helper runs on the model its spawn names (a mob profile's own model is ignored), else on your own."
        );
        let told = HelperModels {
            default: Some("claude-haiku-4-5".into()),
            rule: ModelRule::Deny(patterns(&["claude-opus-*"])),
        }
        .instruction();
        assert!(
            told.contains("a mob profile's own model is ignored")
                && told.contains("one that names none runs on `claude-haiku-4-5`")
                && told.contains("a fork stays on your model")
                && told.contains("run on any model but those matching claude-opus-*"),
            "{told}"
        );
        assert_eq!(told.lines().count(), 1);
    }
}
