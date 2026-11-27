//! The engine's compaction summary call, as the agents' client wrapper
//! ([`super::schema_compat::SchemaCompat`]) sees it: recognised
//! ([`is_summary_request`]), measured ([`SummaryCalls`]) and, on
//! Anthropic, sent without a prompt-cache write nothing reads
//! ([`without_cache_write`]).
//!
//! Meerkat 0.9.19 sends the summary request through the session's
//! decorated client (`meerkat-core` `agent/compact.rs`,
//! `stream_response_authorized`) with no tools, its own provider params
//! and a rewritten history that ends in the compactor's prompt. The engine
//! validates the call's full usage, but `CompactionCompleted` carries only
//! its output (`summary_tokens`). The wrapper records each summary call's
//! usage here; the agent's event watcher (`super::approvals`) takes the one
//! reading that is that compaction's at `CompactionCompleted`, and counts
//! the engine's history estimate only when none is.
//!
//! Every value here is a count or a model id: no message content is kept
//! or logged.

use std::collections::VecDeque;
use std::sync::Arc;

use meerkat_core::lifecycle::run_primitive::{
    AnthropicCacheControlPolicy, AnthropicProviderTag, ProviderTag,
};
use meerkat_core::{
    LlmRequestAuthorization, Message, ModelAuthorizationUse, ProviderParamsOverride, ToolDef,
};
use parking_lot::Mutex;

use crate::cost::UsageTotals;

/// The summary calls kept for the watchers at most (the oldest goes
/// first): a reading nobody takes (a helper's compaction, a compaction
/// that failed after its call) must not pile up.
const KEPT: usize = 64;

/// Whether a request is the engine's compaction summary: by its
/// authorization when it carries one (the engine marks the summary's as
/// [`ModelAuthorizationUse::Compaction`]), else by its shape: no tools,
/// and the last row the user text of the engine's own compaction prompt
/// ([`meerkat::DefaultCompactor`], the compactor every agent runs with).
#[must_use]
pub fn is_summary_request(
    messages: &[Message],
    tools: &[Arc<ToolDef>],
    authorization: Option<&LlmRequestAuthorization>,
) -> bool {
    if let Some(authorization) = authorization {
        return authorization.usage() == ModelAuthorizationUse::Compaction;
    }
    tools.is_empty()
        && matches!(
            messages.last(),
            Some(Message::User(user)) if user.text_content() == compaction_prompt()
        )
}

/// The prompt the engine's default compactor ends its summary request with.
fn compaction_prompt() -> &'static str {
    static PROMPT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PROMPT.get_or_init(|| {
        use meerkat_core::compact::Compactor as _;
        meerkat::DefaultCompactor::new(meerkat_core::compact::CompactionConfig::default())
            .compaction_prompt()
            .to_string()
    })
}

/// The provider params a summary request goes with on `provider`, when
/// they change: on Anthropic, a request with no tools gets
/// `cache_control = disabled`. `None`: send `params` as they are.
///
/// Why: the summary request cannot read the prompt cache (no tools, its
/// own params, a rewritten history), so under Anthropic's default
/// automatic `cache_control` its whole input is billed as a cache write
/// that nothing ever reads (at 1.25 times the input price). The tag
/// merged is the one the request would have gone with (its params' tag,
/// else the client's own, which the engine's adapter uses when the params
/// carry none), so thinking and effort stay as they were (a tag with no
/// `thinking` lowers as no tag does); its `cache_ttl` goes (the client
/// refuses a TTL beside a disabled policy), and so does any
/// provider-native tool (the engine strips them from the session's tag
/// under its `DisableAll` policy, which our gate sets, and a summary uses
/// none). A request for another provider, or one with a tag of another
/// provider's, is left alone.
///
/// It stops applying by itself the day the engine sends the summary with
/// the agent's tools (then the request could read the agent's cache, and
/// this would only lose it): `tests/it/compact.rs`
/// `the_summary_request_carries_no_tools_and_its_own_params` fails that
/// day. Upstream issue: to be filed (a summary request that reuses the
/// agent's cached prefix, or that the engine sends with caching off);
/// this function goes when it ships.
#[must_use]
pub fn without_cache_write(
    provider: meerkat_core::Provider,
    tools: &[Arc<ToolDef>],
    params: Option<&ProviderParamsOverride>,
    default_tag: Option<ProviderTag>,
) -> Option<ProviderParamsOverride> {
    if provider != meerkat_core::Provider::Anthropic || !tools.is_empty() {
        return None;
    }
    let mut out = params.cloned().unwrap_or_default();
    let mut tag = match out.provider_tag.take().or(default_tag) {
        Some(ProviderTag::Anthropic(tag)) => tag,
        None => AnthropicProviderTag::default(),
        Some(_) => return None,
    };
    tag.cache_control = Some(AnthropicCacheControlPolicy::Disabled);
    tag.cache_ttl = None;
    out.provider_tag = Some(ProviderTag::Anthropic(tag));
    out.clear_provider_native_tools();
    Some(out)
}

/// One summary call the wrapper saw: its model and what it used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// Its place in the order the calls were recorded in.
    seq: u64,
    /// The model the provider accounted the call to.
    pub model: String,
    pub totals: UsageTotals,
}

/// Which summary call a completed compaction was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution {
    /// The one reading that matched.
    Measured(Reading),
    /// No reading matched (`candidates == 0`: the call went through no
    /// client of ours, or its reading was dropped), or more than one did
    /// (two agents' summaries of the same size): none is taken.
    Unattributed { candidates: usize },
}

/// The summary calls of every agent, from the wrapper to the watchers:
/// the wrapper knows no session, so each reading waits here until the
/// compaction it was part of completes.
#[derive(Debug, Default)]
pub struct SummaryCalls {
    inner: Mutex<Kept>,
}

#[derive(Debug, Default)]
struct Kept {
    next: u64,
    readings: VecDeque<Reading>,
}

impl SummaryCalls {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Where the readings recorded from now on start: the mark a watcher
    /// takes at a compaction's end, so the next compaction's search starts
    /// after it.
    #[must_use]
    pub fn mark(&self) -> u64 {
        self.inner.lock().next
    }

    /// The wrapper saw a summary call on `model` use `totals`.
    pub fn record(&self, model: &str, totals: UsageTotals) {
        let mut kept = self.inner.lock();
        let seq = kept.next;
        kept.next += 1;
        if kept.readings.len() == KEPT {
            kept.readings.pop_front();
        }
        kept.readings.push_back(Reading {
            seq,
            model: model.to_string(),
            totals,
        });
    }

    /// The reading of a compaction whose summary wrote `output` tokens,
    /// among those recorded from `since` on ([`Self::mark`]): taken when
    /// exactly one matches.
    pub fn take(&self, since: u64, output: u64) -> Attribution {
        let mut kept = self.inner.lock();
        let matching: Vec<usize> = kept
            .readings
            .iter()
            .enumerate()
            .filter(|(_, reading)| reading.seq >= since && reading.totals.output == output)
            .map(|(at, _)| at)
            .collect();
        match matching.as_slice() {
            [at] => kept
                .readings
                .remove(*at)
                .map_or(Attribution::Unattributed { candidates: 0 }, |reading| {
                    Attribution::Measured(reading)
                }),
            other => Attribution::Unattributed {
                candidates: other.len(),
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::UserMessage;
    use meerkat_core::lifecycle::run_primitive::{
        AnthropicCacheTtl, AnthropicThinkingConfig, OpaqueProviderBody, OpenAiProviderTag,
    };

    fn user(text: &str) -> Message {
        Message::User(UserMessage::text(text.to_string()))
    }

    fn tool() -> Arc<ToolDef> {
        Arc::new(ToolDef::new(
            "read_file",
            "reads a file",
            serde_json::json!({"type": "object"}),
        ))
    }

    /// Without an authorization the request is recognised by its shape:
    /// no tools and the engine's own prompt as the last row; a request
    /// with tools, another last row or a prompt that only contains the
    /// engine's is not the summary.
    #[test]
    fn a_summary_request_without_authorization_is_recognised_by_its_shape() {
        let prompt = compaction_prompt();
        assert!(prompt.contains("CONTEXT COMPACTION"), "{prompt}");
        let summary = vec![user("earlier"), user(prompt)];
        assert!(is_summary_request(&summary, &[], None));
        assert!(!is_summary_request(&summary, &[tool()], None));
        assert!(!is_summary_request(
            &[user(prompt), user("next")],
            &[],
            None
        ));
        assert!(!is_summary_request(
            &[user(&format!("{prompt} and more"))],
            &[],
            None
        ));
        assert!(!is_summary_request(&[], &[], None));
    }

    /// With an authorization its use decides, whatever the shape: the
    /// engine's summary is authorized as `Compaction`, an agent's turn as
    /// `Inference`.
    #[test]
    fn a_summary_request_with_authorization_is_recognised_by_its_use() {
        struct NoWork;
        impl meerkat_core::WorkAuthorization for NoWork {
            fn prepare(
                &self,
                _binding: &meerkat_core::PreparedAuthorizationBinding,
            ) -> Result<
                Arc<dyn meerkat_core::PreparedOperationAuthorization>,
                meerkat_core::OperationAuthorizationError,
            > {
                Err(meerkat_core::OperationAuthorizationError::Unavailable)
            }
        }
        let work = meerkat_core::WorkAuthorizationContext::new(
            Arc::new(NoWork),
            meerkat_core::OperationExecutionScope::Domain,
        );
        let authorized = |usage| {
            LlmRequestAuthorization::new(work.clone(), meerkat_core::OperationId::new(), usage)
        };
        let prompt = compaction_prompt();
        let compaction = authorized(ModelAuthorizationUse::Compaction);
        assert!(is_summary_request(
            &[user("x")],
            &[tool()],
            Some(&compaction)
        ));
        let inference = authorized(ModelAuthorizationUse::Inference);
        assert!(!is_summary_request(&[user(prompt)], &[], Some(&inference)));
    }

    fn anthropic(params: &ProviderParamsOverride) -> &AnthropicProviderTag {
        match params.provider_tag.as_ref() {
            Some(ProviderTag::Anthropic(tag)) => tag,
            other => panic!("{other:?}"),
        }
    }

    /// On Anthropic a summary with no tools goes with caching disabled,
    /// merged into the tag it would have gone with: its params' tag
    /// (thinking and effort kept, the TTL and native search dropped), else
    /// the client's own, else a tag of only the policy. Other providers,
    /// a request with tools and a foreign tag are left alone.
    #[test]
    fn the_summary_goes_without_a_cache_write_only_on_anthropic_with_no_tools() {
        use meerkat_core::Provider;
        let bare = without_cache_write(Provider::Anthropic, &[], None, None).unwrap();
        assert_eq!(
            anthropic(&bare),
            &AnthropicProviderTag {
                cache_control: Some(AnthropicCacheControlPolicy::Disabled),
                ..AnthropicProviderTag::default()
            }
        );

        let session = ProviderParamsOverride {
            provider_tag: Some(ProviderTag::Anthropic(AnthropicProviderTag {
                thinking: Some(AnthropicThinkingConfig::Adaptive),
                cache_control: Some(AnthropicCacheControlPolicy::Automatic),
                cache_ttl: Some(AnthropicCacheTtl::OneHour),
                web_search: Some(OpaqueProviderBody::from_value(
                    &serde_json::json!({"type": "web_search"}),
                )),
                ..AnthropicProviderTag::default()
            })),
            max_output_tokens: Some(77),
            ..ProviderParamsOverride::default()
        };
        let merged = without_cache_write(Provider::Anthropic, &[], Some(&session), None).unwrap();
        let tag = anthropic(&merged);
        assert_eq!(tag.thinking, Some(AnthropicThinkingConfig::Adaptive));
        assert_eq!(
            tag.cache_control,
            Some(AnthropicCacheControlPolicy::Disabled)
        );
        assert_eq!(tag.cache_ttl, None);
        assert_eq!(tag.web_search, None);
        assert_eq!(merged.max_output_tokens, Some(77));

        let client_own = ProviderTag::Anthropic(AnthropicProviderTag {
            thinking: Some(AnthropicThinkingConfig::Disabled),
            ..AnthropicProviderTag::default()
        });
        let from_client =
            without_cache_write(Provider::Anthropic, &[], None, Some(client_own)).unwrap();
        assert_eq!(
            anthropic(&from_client).thinking,
            Some(AnthropicThinkingConfig::Disabled)
        );

        assert_eq!(
            without_cache_write(Provider::Anthropic, &[tool()], Some(&session), None),
            None
        );
        for other in [Provider::OpenAI, Provider::Gemini, Provider::SelfHosted] {
            assert_eq!(without_cache_write(other, &[], Some(&session), None), None);
        }
        let foreign = ProviderParamsOverride {
            provider_tag: Some(ProviderTag::OpenAi(OpenAiProviderTag::default())),
            ..ProviderParamsOverride::default()
        };
        assert_eq!(
            without_cache_write(Provider::Anthropic, &[], Some(&foreign), None),
            None
        );
    }

    fn totals(output: u64) -> UsageTotals {
        UsageTotals {
            input: 100,
            output,
            cache_read: 0,
            cache_write: 0,
        }
    }

    /// A compaction takes the one reading of its output size recorded
    /// since its mark, once; two of the same size, or none, leave it
    /// unattributed and take nothing; the oldest reading goes past
    /// [`KEPT`].
    #[test]
    fn a_compaction_takes_the_one_reading_of_its_size_since_its_mark() {
        let calls = SummaryCalls::new();
        calls.record("old", totals(7));
        let since = calls.mark();
        calls.record("m", totals(7));
        calls.record("m", totals(9));
        match calls.take(since, 7) {
            Attribution::Measured(reading) => {
                assert_eq!(reading.model, "m");
                assert_eq!(reading.totals, totals(7));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            calls.take(since, 7),
            Attribution::Unattributed { candidates: 0 },
            "taken once"
        );
        calls.record("other", totals(9));
        assert_eq!(
            calls.take(since, 9),
            Attribution::Unattributed { candidates: 2 }
        );
        assert_eq!(
            calls.take(0, 7),
            Attribution::Measured(Reading {
                seq: 0,
                model: "old".to_string(),
                totals: totals(7)
            })
        );
        for n in 0..u64::try_from(KEPT).unwrap() {
            calls.record("filler", totals(1_000 + n));
        }
        assert_eq!(
            calls.take(0, 9),
            Attribution::Unattributed { candidates: 0 },
            "the oldest went"
        );
    }
}
