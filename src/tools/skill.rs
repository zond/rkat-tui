//! `skill_read`: the agent opens a skill of its `<available_skills>`
//! inventory, or lists them all.
//!
//! The engine (Meerkat 0.9.13) puts the inventory in the system prompt
//! but its own `browse_skills` and `load_skill` are `default_enabled:
//! false` builtins the factory offers no host switch for, so without this
//! tool an agent sees what skills exist and cannot read one
//! (`docs/evidence.md`, "The agent opens a skill").
//!
//! The tool reads through the engine's own skill runtime as a session
//! build makes it ([`SkillHost::runtime`]: the same sources, the same
//! remaps, so the user's copy of a name wins over the project's exactly as
//! in the inventory), never through a path: the user's directory lives in
//! the sealed config directory. Read-only and policy-free like the file
//! tools; the gate allows it in every mode (`crate::approval::policy`).
//!
//! Failure convention (as for the note tools): what the model can act on
//! (an unknown or ambiguous name, skills off) is
//! `Ok(ToolResult { is_error: true, .. })`; malformed arguments are
//! `Err(ToolError::InvalidArguments)`.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::ops::ToolDispatchOutcome;
use meerkat_core::skills::{SkillDescriptor, SkillFilter, SkillRuntime};
use meerkat_core::{
    AgentToolDispatcher, ToolCallView, ToolDef, ToolError, ToolMutationClass, ToolResult,
};
use serde::Deserialize;
use serde_json::json;

/// Tool name: open or list skills.
pub const SKILL_READ: &str = "skill_read";

/// Provenance source id of [`SKILL_READ`] (judged with the file tools'
/// family, see `crate::approval::catalog`).
pub const SKILL_TOOLS_SOURCE: &str = "rkat-tui.skills";

/// The most of a skill's body one call returns (the engine's own
/// injection limit, `[skills] max_injection_bytes`, defaults to the same).
pub const MAX_SKILL_BYTES: usize = 32 * 1024;

/// The most of one description the listing shows.
const MAX_DESCRIPTION_CHARS: usize = 300;

/// The most names an error lists.
const MAX_NAMES: usize = 64;

/// What [`SkillToolDispatcher`] reads skills through.
#[async_trait]
pub trait SkillHost: Send + Sync {
    /// The skill runtime a session build would make now (`None`: skills
    /// are off). May block: the tool calls it on a blocking thread.
    ///
    /// # Errors
    ///
    /// A message for the model (the config could not be read, the runtime
    /// not built).
    async fn runtime(&self) -> Result<Option<Arc<SkillRuntime>>, String>;
}

/// Run `work` over `host`'s runtime on a blocking thread: the engine's
/// skill sources read the file system synchronously inside their futures.
///
/// # Errors
///
/// A message for the model.
pub async fn with_runtime<T, F, Fut>(host: &Arc<dyn SkillHost>, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(Option<Arc<SkillRuntime>>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let host = Arc::clone(host);
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            let runtime = host.runtime().await?;
            work(runtime).await
        })
    })
    .await
    .map_err(|err| format!("the skill worker failed: {err}"))?
}

/// The skills a session's inventory lists, in its order (active, as the
/// engine's `inventory_section` filters them).
///
/// # Errors
///
/// The engine's listing error, in words.
pub async fn listed(runtime: &SkillRuntime) -> Result<Vec<SkillDescriptor>, String> {
    let entries = runtime
        .list_all_with_provenance(&SkillFilter::default())
        .await
        .map_err(|err| err.to_string())?;
    Ok(entries
        .into_iter()
        .filter(|entry| entry.is_active)
        .map(|entry| entry.descriptor)
        .collect())
}

/// The name a skill's source goes by (`embedded`, `user`, `project`, a
/// repository's name), or its uuid.
fn source_of(skill: &SkillDescriptor) -> String {
    if skill.source_name.is_empty() {
        skill.key.source_uuid.to_string()
    } else {
        skill.source_name.clone()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

/// Every skill as `- name [source]: description`.
fn list(skills: &[SkillDescriptor]) -> String {
    if skills.is_empty() {
        return "no skills are available".to_string();
    }
    let mut out = format!(
        "{} skills (open one with {SKILL_READ} and its name):\n",
        skills.len()
    );
    for (n, skill) in skills.iter().enumerate() {
        let mut line = format!("- {} [{}]: ", skill.key.skill_name, source_of(skill));
        let description = skill.description.split_whitespace().collect::<Vec<_>>();
        let description = description.join(" ");
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            line.extend(description.chars().take(MAX_DESCRIPTION_CHARS));
            line.push('…');
        } else {
            line.push_str(&description);
        }
        line.push('\n');
        if out.len() + line.len() > MAX_SKILL_BYTES {
            let _ = write!(out, "… and {} more", skills.len() - n);
            break;
        }
        out.push_str(&line);
    }
    out
}

/// The names of `skills`, for an error.
fn names(skills: &[SkillDescriptor]) -> String {
    let mut names: Vec<&str> = skills.iter().map(|s| s.key.skill_name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let more = names.len().saturating_sub(MAX_NAMES);
    let mut out = names
        .into_iter()
        .take(MAX_NAMES)
        .collect::<Vec<_>>()
        .join(", ");
    if more > 0 {
        let _ = write!(out, " and {more} more");
    }
    out
}

/// `body` cut to [`MAX_SKILL_BYTES`] at a character boundary, with a note
/// of how much was left out.
fn capped(body: &str) -> String {
    if body.len() <= MAX_SKILL_BYTES {
        return body.to_string();
    }
    let mut cut = MAX_SKILL_BYTES;
    while !body.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n[truncated: the skill's body is {} bytes; the first {cut} are shown]",
        &body[..cut],
        body.len()
    )
}

/// One call: the listing, or the skill `name` (of `source`).
async fn read(runtime: Option<Arc<SkillRuntime>>, args: ReadArgs) -> Result<String, String> {
    let Some(runtime) = runtime else {
        return Err("skills are off ([skills] enabled = false in the realm config)".to_string());
    };
    let skills = listed(&runtime).await?;
    let Some(name) = args.name else {
        return Ok(list(&skills));
    };
    let mut found: Vec<&SkillDescriptor> = skills
        .iter()
        .filter(|s| s.key.skill_name.as_str() == name)
        .collect();
    if let Some(source) = &args.source {
        found.retain(|s| source_of(s) == *source || s.key.source_uuid.to_string() == *source);
    }
    let skill = match found.as_slice() {
        [one] => *one,
        [] => {
            let place = args
                .source
                .as_ref()
                .map(|source| format!(" in source `{source}`"))
                .unwrap_or_default();
            return Err(format!(
                "no skill named `{name}`{place}. Skills: {} ({SKILL_READ} without a name lists them with their sources and descriptions)",
                names(&skills)
            ));
        }
        several => {
            let sources: Vec<String> = several.iter().map(|s| source_of(s)).collect();
            return Err(format!(
                "`{name}` is a skill of several sources ({}): call {SKILL_READ} again with `source` set to one of them",
                sources.join(", ")
            ));
        }
    };
    let document = runtime
        .load_from_source(&skill.key, None)
        .await
        .map_err(|err| format!("skill `{name}` could not be read: {err}"))?;
    Ok(format!(
        "skill `{name}` [{}]: {}\n\n{}",
        source_of(skill),
        skill.description.trim(),
        capped(document.body.trim_start())
    ))
}

/// The skill tool over a [`SkillHost`].
pub struct SkillToolDispatcher {
    host: Arc<dyn SkillHost>,
    tools: Arc<[Arc<ToolDef>]>,
}

impl std::fmt::Debug for SkillToolDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillToolDispatcher")
            .finish_non_exhaustive()
    }
}

impl SkillToolDispatcher {
    /// The tool over `host`.
    #[must_use]
    pub fn new(host: Arc<dyn SkillHost>) -> Self {
        let def = ToolDef::new(
            SKILL_READ,
            "Open a skill of <available_skills>: returns its instructions (SKILL.md). Without `name`, lists every skill with its source and description. `source` picks one when several sources have a skill of that name."
                .to_string(),
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "source": {"type": "string"}
                },
                "additionalProperties": false
            }),
        )
        .with_provenance(super::provenance(SKILL_TOOLS_SOURCE));
        Self {
            host,
            tools: vec![Arc::new(def)].into(),
        }
    }
}

#[async_trait]
impl AgentToolDispatcher for SkillToolDispatcher {
    fn tools(&self) -> Arc<[Arc<ToolDef>]> {
        Arc::clone(&self.tools)
    }

    fn tool_catalog_capabilities(&self) -> meerkat_core::ToolCatalogCapabilities {
        super::fixed_catalog()
    }

    fn tool_mutation_class(&self, tool_name: &str) -> ToolMutationClass {
        if tool_name == SKILL_READ {
            ToolMutationClass::ReadOnly
        } else {
            ToolMutationClass::Unknown
        }
    }

    async fn dispatch(&self, call: ToolCallView<'_>) -> Result<ToolDispatchOutcome, ToolError> {
        if call.name != SKILL_READ {
            return Err(ToolError::NotFound {
                name: call.name.to_string(),
            });
        }
        let args: ReadArgs = call
            .parse_args()
            .map_err(|err| ToolError::InvalidArguments {
                name: call.name.to_string(),
                reason: err.to_string(),
            })?;
        let outcome = with_runtime(&self.host, move |runtime| read(runtime, args)).await;
        let id = call.id.to_string();
        let result = match outcome {
            Ok(text) => ToolResult::new(id, text, false),
            Err(message) => ToolResult::new(id, message, true),
        };
        Ok(ToolDispatchOutcome::from(result))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::Config;
    use std::path::Path;

    fn skill(dir: &Path, name: &str, description: &str, body: &str) {
        let dir = dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}"),
        )
        .unwrap();
    }

    /// The engine's runtime over the embedded skills and the project's
    /// `.rkat/skills` of `context` (or none when `config` turns skills off).
    struct Host {
        config: Config,
        context: std::path::PathBuf,
    }

    #[async_trait]
    impl SkillHost for Host {
        async fn runtime(&self) -> Result<Option<Arc<SkillRuntime>>, String> {
            let Some(source) = meerkat_skills::resolve_repositories_with_roots_trusting(
                &self.config.skills,
                Some(&self.context),
                None,
                None,
                &meerkat_skills::SkillHostBoundary::trusted_host(),
            )
            .await
            .map_err(|err| err.to_string())?
            else {
                return Ok(None);
            };
            let engine = meerkat_skills::DefaultSkillEngine::new(source, Vec::new());
            Ok(Some(Arc::new(SkillRuntime::new(Arc::new(engine)))))
        }
    }

    fn dispatcher(config: Config, context: &Path) -> SkillToolDispatcher {
        SkillToolDispatcher::new(Arc::new(Host {
            config,
            context: context.to_path_buf(),
        }))
    }

    async fn call(d: &SkillToolDispatcher, args: serde_json::Value) -> (String, bool) {
        let raw = serde_json::value::to_raw_value(&args).unwrap();
        let result = d
            .dispatch(ToolCallView {
                id: "c1",
                name: SKILL_READ,
                args: &raw,
            })
            .await
            .unwrap()
            .result;
        (result.text_content(), result.is_error)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_reads_lists_and_names_what_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join(".rkat/skills");
        skill(&skills, "deploy", "How we deploy.", "Run make deploy.\n");
        let d = dispatcher(Config::default(), tmp.path());
        let (text, error) = call(&d, json!({"name": "deploy"})).await;
        assert!(!error, "{text}");
        assert!(
            text.starts_with("skill `deploy` [project]: How we deploy.\n\nRun make deploy."),
            "{text}"
        );
        let (text, error) = call(&d, json!({})).await;
        assert!(!error, "{text}");
        assert!(
            text.contains("- deploy [project]: How we deploy.\n"),
            "{text}"
        );
        let (text, error) = call(&d, json!({"name": "nope"})).await;
        assert!(error);
        assert!(
            text.starts_with("no skill named `nope`. Skills: "),
            "{text}"
        );
        assert!(text.contains("deploy"), "{text}");
        let (text, error) = call(&d, json!({"name": "deploy", "source": "user"})).await;
        assert!(error);
        assert!(text.contains("in source `user`"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_long_body_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let body = "é".repeat(MAX_SKILL_BYTES);
        skill(&tmp.path().join(".rkat/skills"), "big", "Big.", &body);
        let d = dispatcher(Config::default(), tmp.path());
        let (text, error) = call(&d, json!({"name": "big"})).await;
        assert!(!error);
        assert!(text.len() < MAX_SKILL_BYTES + 200, "{}", text.len());
        assert!(
            text.ends_with(&format!(
                "[truncated: the skill's body is {} bytes; the first {} are shown]",
                body.len(),
                MAX_SKILL_BYTES
            )),
            "{}",
            &text[text.len() - 120..]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn skills_off_and_bad_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.skills.enabled = false;
        let d = dispatcher(config, tmp.path());
        let (text, error) = call(&d, json!({"name": "x"})).await;
        assert!(error);
        assert!(text.starts_with("skills are off"), "{text}");
        let raw = serde_json::value::to_raw_value(&json!({"nam": "x"})).unwrap();
        let err = d
            .dispatch(ToolCallView {
                id: "c1",
                name: SKILL_READ,
                args: &raw,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments { .. }), "{err:?}");
        assert_eq!(
            d.tool_mutation_class(SKILL_READ),
            ToolMutationClass::ReadOnly
        );
    }
}
