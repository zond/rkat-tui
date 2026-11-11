//! Anthropic thinking per model: `[thinking]` in `tui.toml`.
//!
//! ```toml
//! [thinking]
//! "litellm:claude-proxy" = "adaptive"          # an endpoint:model reference
//! "claude-sonnet-4-5" = { enabled = 8000 }     # a bare model id, any endpoint
//! "claude-opus-4-7" = "default"                # no override (undo a lower layer)
//! ```
//!
//! A setting becomes the Anthropic provider tag's `thinking` in the
//! session's provider params ([`params_for`]), wherever the TUI names the
//! session's model: a new session's build, a `/model` switch (the
//! runtime's reconfigure request) and every resume (which a rewind and a
//! rebuild are). The realm config refuses a realm-level
//! `[agent] provider_params`, so the host carries it per session.
//!
//! Why it exists: Anthropic binds replayed thinking blocks to the request
//! prefix, so a request whose tool list changed (an MCP server connected, a
//! resume) is refused unless it carries the binding control. The engine
//! sends the control for catalogued models; for an id outside the catalog
//! (a proxy alias, a dated snapshot) it does so only when the request
//! configures thinking explicitly, since an unknown model may accept only
//! one thinking type.
//!
//! Settings apply only on Anthropic-dialect endpoints. Start-up drops, with
//! a notice ([`ThinkingTable::validate`]), a reference naming an endpoint of
//! another dialect, a bare id the catalog gives another provider, and a
//! setting a catalogued Anthropic model refuses (the engine's own check,
//! `anthropic_provider_tag_rejection`); an id outside the catalog passes
//! through unchanged. Nothing is inferred from a `[models.<id>]` profile's
//! `supports_thinking`, which does not say which thinking type a model
//! takes.

use std::collections::BTreeMap;
use std::fmt;

use meerkat_core::lifecycle::run_primitive::{AnthropicProviderTag, AnthropicThinkingConfig};
use meerkat_core::{Provider, ProviderParamsOverride, ProviderTag};
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::endpoints::{Endpoint, WireDialect};

/// One `[thinking]` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingSetting {
    /// `"default"`: no override; the engine's and the catalog's default.
    Default,
    /// `"adaptive"`: the model picks its own budget.
    Adaptive,
    /// `{ enabled = <budget_tokens> }`: at most this many thinking tokens.
    Enabled {
        /// The thinking budget.
        budget_tokens: u32,
    },
}

impl ThinkingSetting {
    /// The thinking configuration the setting sends (`None`: no override).
    #[must_use]
    pub fn config(self) -> Option<AnthropicThinkingConfig> {
        match self {
            Self::Default => None,
            Self::Adaptive => Some(AnthropicThinkingConfig::Adaptive),
            Self::Enabled { budget_tokens } => {
                Some(AnthropicThinkingConfig::Enabled { budget_tokens })
            }
        }
    }
}

impl Serialize for ThinkingSetting {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Default => serializer.serialize_str("default"),
            Self::Adaptive => serializer.serialize_str("adaptive"),
            Self::Enabled { budget_tokens } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("enabled", budget_tokens)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ThinkingSetting {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(SettingVisitor)
    }
}

/// What a `[thinking]` value may be, for the parse error.
const EXPECTED: &str = r#""adaptive", "default" or { enabled = <budget tokens> }"#;

struct SettingVisitor;

impl<'de> Visitor<'de> for SettingVisitor {
    type Value = ThinkingSetting;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(EXPECTED)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        match value {
            "default" => Ok(ThinkingSetting::Default),
            "adaptive" => Ok(ThinkingSetting::Adaptive),
            other => Err(E::custom(format!(
                "unknown thinking setting `{other}`, expected {EXPECTED}"
            ))),
        }
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut budget = None;
        while let Some(key) = map.next_key::<String>()? {
            if key != "enabled" {
                return Err(de::Error::custom(format!(
                    "unknown thinking key `{key}`, expected {EXPECTED}"
                )));
            }
            if budget.is_some() {
                return Err(de::Error::duplicate_field("enabled"));
            }
            budget = Some(map.next_value::<u32>()?);
        }
        budget
            .map(|budget_tokens| ThinkingSetting::Enabled { budget_tokens })
            .ok_or_else(|| de::Error::missing_field("enabled"))
    }
}

/// The layered `[thinking]` entries after start-up validation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkingTable {
    entries: BTreeMap<String, ThinkingSetting>,
}

impl ThinkingTable {
    /// Keep the entries that can apply on `endpoints`; a notice for each
    /// one dropped (see the module docs).
    #[must_use]
    pub fn validate(
        entries: BTreeMap<String, ThinkingSetting>,
        endpoints: &[Endpoint],
    ) -> (Self, Vec<String>) {
        let mut notices = Vec::new();
        let entries = entries
            .into_iter()
            .filter(|(key, setting)| match refusal(key, *setting, endpoints) {
                Some(reason) => {
                    notices.push(format!("ignored [thinking.\"{key}\"]: {reason}"));
                    false
                }
                None => true,
            })
            .collect();
        (Self { entries }, notices)
    }

    /// The thinking configuration for `model` on the endpoint called
    /// `endpoint` (`None` when the endpoint is not configured in this
    /// process), served by `provider`'s client: the `endpoint:model`
    /// reference's entry, else the bare id's. `None` off the Anthropic
    /// dialect and without a setting.
    #[must_use]
    pub fn resolve(
        &self,
        endpoint: Option<&str>,
        provider: Provider,
        model: &str,
    ) -> Option<AnthropicThinkingConfig> {
        if provider != Provider::Anthropic {
            return None;
        }
        endpoint
            .and_then(|endpoint| self.entries.get(&format!("{endpoint}:{model}")))
            .or_else(|| self.entries.get(model))
            .and_then(|setting| setting.config())
    }
}

/// The endpoint a `[thinking]` key names, and its model id: an
/// `endpoint:model` reference when the part before the first `:` is an
/// endpoint of this process, else a bare id.
fn split_key<'a>(key: &'a str, endpoints: &'a [Endpoint]) -> (Option<&'a Endpoint>, &'a str) {
    key.split_once(':')
        .and_then(|(name, model)| {
            endpoints
                .iter()
                .find(|e| e.name == name)
                .map(|endpoint| (Some(endpoint), model))
        })
        .unwrap_or((None, key))
}

/// Why the entry `key = setting` cannot apply, if it cannot.
fn refusal(key: &str, setting: ThinkingSetting, endpoints: &[Endpoint]) -> Option<String> {
    let thinking = setting.config()?;
    let (endpoint, model) = split_key(key, endpoints);
    if let Some(endpoint) = endpoint {
        if endpoint.dialect != WireDialect::Anthropic {
            return Some(format!(
                "endpoint `{}` speaks the {} dialect; thinking applies to Anthropic-dialect endpoints only",
                endpoint.name,
                endpoint.dialect.as_str()
            ));
        }
    } else if let Some(provider) = meerkat_models::infer_provider(model)
        && provider != Provider::Anthropic
    {
        return Some(format!(
            "`{model}` is a {} model; thinking applies to Anthropic models only",
            provider.as_str()
        ));
    }
    let tag = AnthropicProviderTag {
        thinking: Some(thinking),
        ..AnthropicProviderTag::default()
    };
    meerkat_models::capabilities_for(Provider::Anthropic, model)
        .and_then(|capabilities| capabilities.anthropic_provider_tag_rejection(&tag))
}

/// The provider params a session gets for `thinking`: an Anthropic tag
/// carrying it, `None` without a setting.
#[must_use]
pub fn params_for(thinking: Option<&AnthropicThinkingConfig>) -> Option<ProviderParamsOverride> {
    thinking.map(|thinking| ProviderParamsOverride {
        provider_tag: Some(ProviderTag::Anthropic(AnthropicProviderTag {
            thinking: Some(thinking.clone()),
            ..AnthropicProviderTag::default()
        })),
        ..ProviderParamsOverride::default()
    })
}

/// What a resumed session's build does with its persisted provider params
/// ([`reapplied`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Reapplied {
    /// They already carry the configured thinking: the build keeps them.
    Keep,
    /// The build carries these instead (`None`: nothing is left).
    Replace(Option<ProviderParamsOverride>),
}

/// The provider params a resumed session's build must carry so that its
/// thinking is `thinking`, starting from the `persisted` ones:
/// [`Reapplied::Keep`] when they already say so, else the persisted params
/// with only the Anthropic tag's `thinking` replaced. Whatever else they
/// hold stays.
#[must_use]
pub fn reapplied(
    persisted: Option<&ProviderParamsOverride>,
    thinking: Option<&AnthropicThinkingConfig>,
) -> Reapplied {
    let current = persisted.and_then(|params| match &params.provider_tag {
        Some(ProviderTag::Anthropic(tag)) => tag.thinking.as_ref(),
        _ => None,
    });
    if current == thinking {
        return Reapplied::Keep;
    }
    let mut params = persisted.cloned().unwrap_or_default();
    let mut tag = match params.provider_tag.take() {
        Some(ProviderTag::Anthropic(tag)) => tag,
        // Another provider's tag on an Anthropic session would be refused
        // beside the thinking; nothing else may be lost.
        Some(other) if thinking.is_none() => {
            params.provider_tag = Some(other);
            return Reapplied::Replace((!params.is_empty()).then_some(params));
        }
        _ => AnthropicProviderTag::default(),
    };
    tag.thinking = thinking.cloned();
    if tag != AnthropicProviderTag::default() {
        params.provider_tag = Some(ProviderTag::Anthropic(tag));
    }
    Reapplied::Replace((!params.is_empty()).then_some(params))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::endpoints::SecretString;

    fn ep(name: &str, dialect: WireDialect) -> Endpoint {
        Endpoint::configured(
            name,
            dialect,
            Some("http://127.0.0.1:4000".to_string()),
            SecretString::new("k"),
            "PATH",
            0,
            crate::endpoints::ApiStyle::default(),
        )
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct Doc {
        thinking: BTreeMap<String, ThinkingSetting>,
    }

    fn parse(text: &str) -> Result<BTreeMap<String, ThinkingSetting>, String> {
        toml::from_str::<Doc>(text)
            .map(|doc| doc.thinking)
            .map_err(|err| err.to_string())
    }

    #[test]
    fn each_value_parses() {
        let entries = parse(
            r#"
            [thinking]
            "proxy:alias" = "adaptive"
            "claude-sonnet-4-5" = { enabled = 8000 }
            "claude-opus-4-7" = "default"
            "#,
        )
        .unwrap();
        assert_eq!(entries["proxy:alias"], ThinkingSetting::Adaptive);
        assert_eq!(
            entries["claude-sonnet-4-5"],
            ThinkingSetting::Enabled {
                budget_tokens: 8000
            }
        );
        assert_eq!(entries["claude-opus-4-7"], ThinkingSetting::Default);
        assert_eq!(
            ThinkingSetting::Adaptive.config(),
            Some(AnthropicThinkingConfig::Adaptive)
        );
        assert_eq!(
            ThinkingSetting::Enabled {
                budget_tokens: 8000
            }
            .config(),
            Some(AnthropicThinkingConfig::Enabled {
                budget_tokens: 8000
            })
        );
        assert_eq!(ThinkingSetting::Default.config(), None);
    }

    #[test]
    fn the_settings_round_trip() {
        let doc = Doc {
            thinking: BTreeMap::from([
                ("a".to_string(), ThinkingSetting::Adaptive),
                ("b".to_string(), ThinkingSetting::Default),
                (
                    "c".to_string(),
                    ThinkingSetting::Enabled {
                        budget_tokens: 2048,
                    },
                ),
            ]),
        };
        let text = toml::to_string(&doc).unwrap();
        assert_eq!(toml::from_str::<Doc>(&text).unwrap(), doc);
    }

    #[test]
    fn a_bad_value_names_what_is_expected() {
        for text in [
            "[thinking]\nx = \"off\"",
            "[thinking]\nx = { budget = 10 }",
            "[thinking]\nx = {}",
            "[thinking]\nx = { enabled = -1 }",
            "[thinking]\nx = 5",
        ] {
            let err = parse(text).unwrap_err();
            assert!(
                err.contains("adaptive") || err.contains("enabled") || err.contains("u32"),
                "{text}: {err}"
            );
        }
        assert!(
            parse("[thinking]\nx = \"off\"")
                .unwrap_err()
                .contains("`off`")
        );
    }

    #[test]
    fn a_reference_wins_over_the_bare_id_and_default_is_no_override() {
        let endpoints = [ep("proxy", WireDialect::Anthropic)];
        let (table, notices) = ThinkingTable::validate(
            BTreeMap::from([
                ("proxy:alias".to_string(), ThinkingSetting::Adaptive),
                (
                    "alias".to_string(),
                    ThinkingSetting::Enabled {
                        budget_tokens: 4096,
                    },
                ),
                ("other".to_string(), ThinkingSetting::Default),
            ]),
            &endpoints,
        );
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(
            table.resolve(Some("proxy"), Provider::Anthropic, "alias"),
            Some(AnthropicThinkingConfig::Adaptive)
        );
        assert_eq!(
            table.resolve(Some("anthropic"), Provider::Anthropic, "alias"),
            Some(AnthropicThinkingConfig::Enabled {
                budget_tokens: 4096
            })
        );
        assert_eq!(
            table.resolve(None, Provider::Anthropic, "alias"),
            Some(AnthropicThinkingConfig::Enabled {
                budget_tokens: 4096
            })
        );
        assert_eq!(
            table.resolve(Some("proxy"), Provider::Anthropic, "other"),
            None
        );
        assert_eq!(
            table.resolve(Some("proxy"), Provider::Anthropic, "unset"),
            None
        );
    }

    #[test]
    fn a_non_anthropic_endpoint_is_ignored_with_a_notice() {
        let endpoints = [
            ep("litellm", WireDialect::OpenAi),
            ep("proxy", WireDialect::Anthropic),
        ];
        let (table, notices) = ThinkingTable::validate(
            BTreeMap::from([
                ("litellm:claude-x".to_string(), ThinkingSetting::Adaptive),
                ("gpt-5.5".to_string(), ThinkingSetting::Adaptive),
                // A no-override entry never needs a notice.
                ("litellm:quiet".to_string(), ThinkingSetting::Default),
                ("served".to_string(), ThinkingSetting::Adaptive),
            ]),
            &endpoints,
        );
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(
            notices[0].contains("gpt-5.5") && notices[0].contains("openai model"),
            "{notices:?}"
        );
        assert!(
            notices[1].contains("litellm:claude-x") && notices[1].contains("openai dialect"),
            "{notices:?}"
        );
        assert_eq!(
            table.resolve(Some("litellm"), Provider::Anthropic, "claude-x"),
            None
        );
        // A bare id applies only where the client speaks Anthropic.
        assert_eq!(
            table.resolve(Some("litellm"), Provider::OpenAI, "served"),
            None
        );
        assert_eq!(
            table.resolve(Some("proxy"), Provider::Anthropic, "served"),
            Some(AnthropicThinkingConfig::Adaptive)
        );
    }

    /// Claude Fable 5.2 (in the catalog from Meerkat 0.9.12) thinks
    /// adaptively only: a budget is refused at start with the engine's
    /// reason, `adaptive` is kept.
    #[test]
    fn claude_fable_5_2_takes_only_adaptive_thinking() {
        let (table, notices) = ThinkingTable::validate(
            BTreeMap::from([(
                "claude-fable-5-2".to_string(),
                ThinkingSetting::Enabled {
                    budget_tokens: 4096,
                },
            )]),
            &[],
        );
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].contains("claude-fable-5-2") && notices[0].contains("'enabled'"),
            "{notices:?}"
        );
        assert_eq!(
            table.resolve(None, Provider::Anthropic, "claude-fable-5-2"),
            None
        );
        let (table, notices) = ThinkingTable::validate(
            BTreeMap::from([("claude-fable-5-2".to_string(), ThinkingSetting::Adaptive)]),
            &[],
        );
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(
            table.resolve(None, Provider::Anthropic, "claude-fable-5-2"),
            Some(AnthropicThinkingConfig::Adaptive)
        );
    }

    #[test]
    fn a_catalogued_model_refusing_the_type_is_ignored_with_the_engines_reason() {
        let endpoints = [ep("proxy", WireDialect::Anthropic)];
        let (table, notices) = ThinkingTable::validate(
            BTreeMap::from([
                // Adaptive only.
                (
                    "claude-opus-4-7".to_string(),
                    ThinkingSetting::Enabled {
                        budget_tokens: 4096,
                    },
                ),
                // Enabled only, through a reference.
                (
                    "proxy:claude-sonnet-4-5".to_string(),
                    ThinkingSetting::Adaptive,
                ),
                // Accepted.
                ("claude-sonnet-4-6".to_string(), ThinkingSetting::Adaptive),
                // Outside the catalog: passed through as it is.
                (
                    "proxy:my-alias".to_string(),
                    ThinkingSetting::Enabled { budget_tokens: 1 },
                ),
            ]),
            &endpoints,
        );
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(
            notices[0].contains("claude-opus-4-7") && notices[0].contains("'enabled'"),
            "{notices:?}"
        );
        assert!(
            notices[1].contains("claude-sonnet-4-5") && notices[1].contains("'adaptive'"),
            "{notices:?}"
        );
        assert_eq!(
            table.resolve(None, Provider::Anthropic, "claude-opus-4-7"),
            None
        );
        assert_eq!(
            table.resolve(None, Provider::Anthropic, "claude-sonnet-4-6"),
            Some(AnthropicThinkingConfig::Adaptive)
        );
        assert_eq!(
            table.resolve(Some("proxy"), Provider::Anthropic, "my-alias"),
            Some(AnthropicThinkingConfig::Enabled { budget_tokens: 1 })
        );
    }

    #[test]
    fn params_carry_only_the_thinking() {
        assert_eq!(params_for(None), None);
        let params = params_for(Some(&AnthropicThinkingConfig::Adaptive)).unwrap();
        let Some(ProviderTag::Anthropic(tag)) = &params.provider_tag else {
            panic!("an Anthropic tag: {params:?}");
        };
        assert_eq!(tag.thinking, Some(AnthropicThinkingConfig::Adaptive));
        assert_eq!(
            AnthropicProviderTag {
                thinking: None,
                ..tag.clone()
            },
            AnthropicProviderTag::default()
        );
    }

    #[test]
    fn a_resume_replaces_only_the_thinking() {
        let adaptive = AnthropicThinkingConfig::Adaptive;
        // Nothing persisted, nothing configured: the build keeps it.
        assert_eq!(reapplied(None, None), Reapplied::Keep);
        // Already what the config says.
        let persisted = params_for(Some(&adaptive));
        assert_eq!(
            reapplied(persisted.as_ref(), Some(&adaptive)),
            Reapplied::Keep
        );
        // Newly configured.
        assert_eq!(
            reapplied(None, Some(&adaptive)),
            Reapplied::Replace(persisted.clone())
        );
        // No longer configured: nothing is left.
        assert_eq!(
            reapplied(persisted.as_ref(), None),
            Reapplied::Replace(None)
        );
        // Other knobs stay.
        let mut rich = persisted.unwrap();
        rich.temperature = Some(0.5);
        if let Some(ProviderTag::Anthropic(tag)) = &mut rich.provider_tag {
            tag.top_k = Some(5);
        }
        let budget = AnthropicThinkingConfig::Enabled {
            budget_tokens: 2048,
        };
        let Reapplied::Replace(Some(next)) = reapplied(Some(&rich), Some(&budget)) else {
            panic!("a change");
        };
        assert_eq!(next.temperature, Some(0.5));
        let Some(ProviderTag::Anthropic(tag)) = &next.provider_tag else {
            panic!("an Anthropic tag");
        };
        assert_eq!(tag.top_k, Some(5));
        assert_eq!(tag.thinking, Some(budget));
    }
}
