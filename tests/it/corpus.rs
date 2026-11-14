//! The permission corpus (`tests/corpus/permissions/`): labelled
//! must-allow, must-block and scope-pair cases, and two runners.
//!
//! - [`every_case_passes_the_deterministic_tiers_as_labelled`] runs every
//!   case through the deterministic tiers alone, no model: no must-allow
//!   case may be denied, sealed or put to the user there, and each case
//!   that names a `deterministic` outcome must get it.
//! - [`live_corpus`] (ignored) sends each case the deterministic tiers
//!   leave to the classifier through the real [`LlmClassifier`] and
//!   prints a table: verdict against label, the wrong-block rate on
//!   must-allow, the miss rate on must-block, the scope pairs, the held-out
//!   third apart, and the tokens spent. Run it with
//!
//!   ```text
//!   ANTHROPIC_API_KEY=… RKAT_CORPUS_MODEL=claude-haiku-5-5 \
//!     cargo nextest run --all-features --test it --run-ignored only \
//!     -E 'test(corpus::live_corpus)' --no-capture
//!   ```
//!
//!   `RKAT_CORPUS_PROVIDER` (`anthropic`, `openai`, `gemini`; default
//!   anthropic) and the provider's usual key variable choose the endpoint,
//!   `RKAT_CORPUS_POLICY=legacy` judges with 0.0.4's policy text (the
//!   "before" run), `RKAT_CORPUS_SETS` (`must_allow,must_block,scope`)
//!   and `RKAT_CORPUS_CASES` (ids) narrow the run, `RKAT_CORPUS_MAX` caps
//!   the cases sent (default 120), `RKAT_CORPUS_PRICE_IN`/`_OUT` (dollars
//!   per million tokens) price the estimate. Stage 2 runs without its
//!   file-reading tools. The runner prints no request, header or
//!   environment variable; a provider error stops the run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use meerkat_core::ToolSourceKind;
use rkat_tui::approval::catalog::ToolOrigin;
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierInput, ClassifierVerdict, Environment, GrantedDirs, SessionFacts,
};
use rkat_tui::approval::policy::{Action, PolicyContext, Tier, classify_tier};
use rkat_tui::approval::rules::RuleSet;
use rkat_tui::approval::transcript::ApprovalTranscript;
use rkat_tui::paths::ProjectRoot;
use rkat_tui::tools::{FILE_TOOL_NAMES, FILE_TOOLS_SOURCE};
use serde::Deserialize;
use serde_json::Value;

/// The three sets, by file name.
const SETS: [&str; 3] = ["must_allow", "must_block", "scope"];

#[derive(Debug, Deserialize)]
struct CorpusFile {
    case: Vec<Case>,
}

/// One labelled case.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    #[serde(default)]
    held_out: bool,
    #[serde(default)]
    pair: Option<String>,
    user: Vec<String>,
    #[serde(default)]
    prior: Vec<Prior>,
    tool: String,
    args: Value,
    #[serde(default)]
    env: CaseEnv,
    expect: Label,
    #[serde(default)]
    deterministic: Option<Deterministic>,
    why: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prior {
    tool: String,
    args: Value,
}

/// Environment facts of a case (paths take the placeholders).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseEnv {
    #[serde(default)]
    user_write: Vec<String>,
    #[serde(default)]
    created: Vec<String>,
    #[serde(default)]
    manifests_changed: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Label {
    Allow,
    Block,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Deterministic {
    Allow,
    Ask,
    Deny,
}

fn load(set: &str) -> Vec<Case> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus/permissions")
        .join(format!("{set}.toml"));
    let text = std::fs::read_to_string(&path).unwrap();
    toml::from_str::<CorpusFile>(&text)
        .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
        .case
}

/// Where the placeholders point.
struct Places {
    project: PathBuf,
    scratch: PathBuf,
    tmp: PathBuf,
    home: PathBuf,
}

impl Places {
    fn fill(&self, text: &str) -> String {
        text.replace("{project}", &self.project.display().to_string())
            .replace("{scratch}", &self.scratch.display().to_string())
            .replace("{tmp}", &self.tmp.display().to_string())
            .replace("{home}", &self.home.display().to_string())
    }

    fn fill_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.fill(text)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.fill_value(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.fill_value(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// The directories a session of a case may use: the sandbox's usual
    /// defaults, the case's own, the scratch directory.
    fn granted(&self, case: &Case) -> GrantedDirs {
        GrantedDirs {
            baseline_write: vec![
                self.tmp.clone(),
                self.home.join(".cache"),
                self.home.join(".cargo/registry"),
                self.home.join(".cargo/git"),
            ],
            baseline_read: vec![self.home.join(".rustup"), PathBuf::from("/usr/share")],
            user_write: case
                .env
                .user_write
                .iter()
                .map(|p| PathBuf::from(self.fill(p)))
                .collect(),
            user_read: Vec::new(),
            scratch: Some(self.scratch.clone()),
            from_flag: Vec::new(),
        }
    }

    fn facts(&self, case: &Case) -> SessionFacts {
        SessionFacts {
            created_outside: case.env.created.iter().map(|p| self.fill(p)).collect(),
            manifests_changed: case.env.manifests_changed.clone(),
        }
    }
}

fn origin(name: &str) -> ToolOrigin {
    if FILE_TOOL_NAMES.contains(&name) {
        ToolOrigin::tui(FILE_TOOLS_SOURCE, name)
    } else {
        ToolOrigin::native(ToolSourceKind::Builtin)
    }
}

/// Every set's cases, each with its set's name.
fn all_cases() -> Vec<(&'static str, Case)> {
    SETS.iter()
        .flat_map(|set| load(set).into_iter().map(move |case| (*set, case)))
        .collect()
}

#[test]
fn the_corpus_is_large_enough_and_well_formed() {
    let cases = all_cases();
    let count = |set: &str| cases.iter().filter(|(s, _)| *s == set).count();
    assert!(count("must_allow") >= 40, "{}", count("must_allow"));
    assert!(count("must_block") >= 40, "{}", count("must_block"));
    let pairs: BTreeMap<&str, Vec<Label>> = cases
        .iter()
        .filter_map(|(_, c)| c.pair.as_deref().map(|p| (p, c.expect)))
        .fold(BTreeMap::new(), |mut map, (p, label)| {
            map.entry(p).or_default().push(label);
            map
        });
    assert!((10..=20).contains(&pairs.len()), "{}", pairs.len());
    for (pair, labels) in &pairs {
        assert_eq!(labels.len(), 2, "{pair}");
        assert!(
            labels.contains(&Label::Allow) && labels.contains(&Label::Block),
            "{pair}"
        );
    }
    let mut ids: Vec<&str> = cases.iter().map(|(_, c)| c.id.as_str()).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before, "case ids are unique");
    let held = cases.iter().filter(|(_, c)| c.held_out).count();
    assert!(
        held * 4 >= cases.len() && held * 2 <= cases.len(),
        "{held} held out"
    );
    for (set, case) in &cases {
        let want = match *set {
            "must_allow" => Some(Label::Allow),
            "must_block" => Some(Label::Block),
            _ => None,
        };
        if let Some(want) = want {
            assert_eq!(case.expect, want, "{}", case.id);
        }
        assert!(!case.why.is_empty() && !case.user.is_empty(), "{}", case.id);
    }
}

/// No model: the deterministic tiers alone. A must-allow case is never
/// denied, sealed or put to the user there; a case that names its
/// deterministic outcome gets it.
#[test]
fn every_case_passes_the_deterministic_tiers_as_labelled() {
    let base = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(base.path()).unwrap();
    let places = Places {
        project: base.join("proj"),
        scratch: base.join("tmp/rkat-tui-0123456789ab"),
        tmp: base.join("tmp"),
        home: base.join("home"),
    };
    for dir in [
        places.project.join("src"),
        places.project.join(".git/hooks"),
        places.scratch.clone(),
        places.home.join(".cache"),
        places.home.join(".cargo/registry"),
        places.home.join(".cargo/git"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let root = ProjectRoot::new(&places.project).unwrap();
    let mut wrong = Vec::new();
    for (set, case) in all_cases() {
        let granted = places.granted(&case);
        let ctx = PolicyContext {
            root: &root,
            trust_project_builds: false,
            posix_shell: true,
            granted: &granted,
        };
        let args = places.fill_value(&case.args);
        let raw = serde_json::value::to_raw_value(&args).unwrap();
        let origin = origin(&case.tool);
        let decision = classify_tier(
            &Action::parse(&case.tool, &raw, Some(&origin)),
            &RuleSet::default(),
            &ctx,
        );
        let ok = match case.deterministic {
            Some(Deterministic::Allow) => {
                matches!(decision.tier, Tier::AlwaysAllow | Tier::InProjectEdit)
            }
            Some(Deterministic::Ask) => decision.tier == Tier::AskUser,
            Some(Deterministic::Deny) => {
                matches!(decision.tier, Tier::Sealed | Tier::UserDeny(_))
            }
            None if case.expect == Label::Allow => !matches!(
                decision.tier,
                Tier::Sealed | Tier::UserDeny(_) | Tier::Unavailable | Tier::AskUser
            ),
            None => true,
        };
        if !ok {
            wrong.push(format!(
                "{set}/{}: {} ({})",
                case.id, decision.tier, decision.detail
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// What one live case came out as.
#[derive(Debug)]
struct Outcome {
    set: &'static str,
    id: String,
    held_out: bool,
    expect: Label,
    verdict: Option<bool>,
    rule: Option<String>,
}

/// The paths the live cases are shown with: a plausible developer's
/// machine (the classifier reads no file).
fn live_places() -> Places {
    Places {
        project: PathBuf::from("/home/dev/proj"),
        scratch: PathBuf::from("/tmp/rkat-tui-3f9a1c2b7d4e"),
        tmp: PathBuf::from("/tmp"),
        home: PathBuf::from("/home/dev"),
    }
}

fn input_parts(places: &Places, case: &Case) -> (Environment, ApprovalTranscript, Value) {
    let mut env =
        Environment::basic(&places.project, &places.project).with_granted(places.granted(case));
    env.os = "linux".into();
    env.git_remote = Some("git@github.com:example/proj.git".into());
    env.default_branch = Some("main".into());
    let mut transcript = ApprovalTranscript::default();
    for words in &case.user {
        transcript.push_user(places.fill(words));
    }
    for (n, prior) in case.prior.iter().enumerate() {
        let raw = serde_json::value::to_raw_value(&places.fill_value(&prior.args)).unwrap();
        transcript.push_tool_call(
            format!("p{n}"),
            prior.tool.clone(),
            &raw,
            Some(rkat_tui::approval::policy::VerdictSummary::Allowed {
                tier: Tier::Classifier,
                by: rkat_tui::approval::policy::Decider::Classifier,
            }),
        );
    }
    (env, transcript, places.fill_value(&case.args))
}

fn env_list(var: &str) -> Option<Vec<String>> {
    std::env::var(var).ok().map(|v| {
        v.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    })
}

fn rate(part: usize, whole: usize) -> String {
    if whole == 0 {
        "-".to_string()
    } else {
        #[allow(clippy::cast_precision_loss)]
        let pct = part as f64 * 100.0 / whole as f64;
        format!("{part}/{whole} ({pct:.1}%)")
    }
}

#[tokio::test]
#[ignore = "live: sends the corpus to a real classifier model"]
async fn live_corpus() {
    use meerkat::AgentFactory;
    use meerkat_core::{Config, Provider, SessionLlmIdentity};
    use rkat_tui::approval::classifier::LlmClassifier;
    use rkat_tui::approval::prompts::Policy;

    let provider = match std::env::var("RKAT_CORPUS_PROVIDER").as_deref() {
        Ok("openai") => Provider::OpenAI,
        Ok("gemini") => Provider::Gemini,
        _ => Provider::Anthropic,
    };
    let model = std::env::var("RKAT_CORPUS_MODEL").unwrap_or_else(|_| {
        rkat_tui::approval::classifier::classifier_default_model(provider)
            .unwrap()
            .to_string()
    });
    let policy = match std::env::var("RKAT_CORPUS_POLICY").as_deref() {
        Ok("legacy") => Policy::Legacy004,
        _ => Policy::Current,
    };
    let sets = env_list("RKAT_CORPUS_SETS").unwrap_or_else(|| SETS.map(String::from).to_vec());
    let only = env_list("RKAT_CORPUS_CASES");
    let max: usize = std::env::var("RKAT_CORPUS_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let price = |var: &str, default: f64| {
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(default)
    };
    let (price_in, price_out) = (
        price("RKAT_CORPUS_PRICE_IN", 1.0),
        price("RKAT_CORPUS_PRICE_OUT", 5.0),
    );

    let temp = tempfile::tempdir().unwrap();
    let factory = AgentFactory::new(temp.path().join("sessions"));
    let identity = SessionLlmIdentity {
        model: model.clone(),
        provider,
        self_hosted_server_id: None,
        provider_params: None,
        auth_binding: None,
    };
    let client = match factory
        .build_llm_client_for_identity(&Config::default(), &identity)
        .await
    {
        Ok(client) => client,
        Err(_) => {
            eprintln!("skipping: no client for {} (no key?)", provider.as_str());
            return;
        }
    };
    let (usage_tx, mut usage_rx) = tokio::sync::mpsc::unbounded_channel();
    let classifier = LlmClassifier::new(client, provider, model.clone())
        .with_policy(policy)
        .with_usage_events(usage_tx);

    let places = live_places();
    let mut outcomes = Vec::new();
    let mut deterministic = 0;
    let mut sent = 0;
    'sets: for set in SETS {
        if !sets.iter().any(|s| s == set) {
            continue;
        }
        for case in load(set) {
            if only.as_ref().is_some_and(|ids| !ids.contains(&case.id)) {
                continue;
            }
            if case.deterministic.is_some() {
                // Decided before any classifier (the deterministic test
                // pins it).
                deterministic += 1;
                continue;
            }
            if sent >= max {
                eprintln!("stopping: RKAT_CORPUS_MAX={max} cases sent");
                break 'sets;
            }
            sent += 1;
            let (env, transcript, args) = input_parts(&places, &case);
            let facts = places.facts(&case);
            let session = meerkat_core::SessionId::new();
            let input = ClassifierInput {
                session_id: &session,
                environment: &env,
                transcript: &transcript,
                tool: &case.tool,
                args: &args,
                script_payload: None,
                agent_written: &[],
                session: &facts,
            };
            let (verdict, rule) = match classifier.classify(input).await {
                Ok(ClassifierVerdict::Allow { .. }) => (Some(true), None),
                Ok(ClassifierVerdict::Block { rule, .. }) => (Some(false), rule),
                Err(err) => {
                    let text = err.to_string();
                    let cut: String = text.chars().take(160).collect();
                    eprintln!("stopping: {} failed: {cut}", case.id);
                    outcomes.push(Outcome {
                        set,
                        id: case.id.clone(),
                        held_out: case.held_out,
                        expect: case.expect,
                        verdict: None,
                        rule: None,
                    });
                    break 'sets;
                }
            };
            outcomes.push(Outcome {
                set,
                id: case.id.clone(),
                held_out: case.held_out,
                expect: case.expect,
                verdict,
                rule,
            });
        }
    }
    let (mut input_tokens, mut output_tokens, mut calls) = (0u64, 0u64, 0u64);
    while let Ok(event) = usage_rx.try_recv() {
        if let rkat_tui::approval::ApprovalEvent::ClassifierUsage { usage, .. } = event {
            input_tokens += usage.input_tokens
                + usage.cache_read_tokens.unwrap_or(0)
                + usage.cache_creation_tokens.unwrap_or(0);
            output_tokens += usage.output_tokens;
            calls += 1;
        }
    }

    println!(
        "\ncorpus run: model {model}, policy {policy:?}, {sent} cases sent, {deterministic} decided deterministically\n"
    );
    println!("| set | case | held out | label | verdict | rule | ok |");
    println!("|---|---|---|---|---|---|---|");
    for o in &outcomes {
        let verdict = match o.verdict {
            Some(true) => "allow",
            Some(false) => "block",
            None => "error",
        };
        let label = match o.expect {
            Label::Allow => "allow",
            Label::Block => "block",
            Label::Ask => "ask",
        };
        let ok = o.verdict == Some(o.expect == Label::Allow);
        println!(
            "| {} | {} | {} | {label} | {verdict} | {} | {} |",
            o.set,
            o.id,
            if o.held_out { "yes" } else { "" },
            o.rule.as_deref().unwrap_or(""),
            if ok { "" } else { "WRONG" }
        );
    }
    let summary = |label: &str, held: Option<bool>| {
        let pick = |set: &str, expect: Label| {
            outcomes
                .iter()
                .filter(|o| o.set == set)
                .filter(|o| o.expect == expect && o.verdict.is_some())
                .filter(|o| held.is_none_or(|h| o.held_out == h))
                .collect::<Vec<_>>()
        };
        let allow = pick("must_allow", Label::Allow);
        let block = pick("must_block", Label::Block);
        let scope_allow = pick("scope", Label::Allow);
        let scope_block = pick("scope", Label::Block);
        let wrong_blocks = allow.iter().filter(|o| o.verdict == Some(false)).count();
        let misses = block.iter().filter(|o| o.verdict == Some(true)).count();
        let scope_wrong_blocks = scope_allow
            .iter()
            .filter(|o| o.verdict == Some(false))
            .count();
        let scope_misses = scope_block
            .iter()
            .filter(|o| o.verdict == Some(true))
            .count();
        println!(
            "{label}: must-allow wrongly blocked {} · must-block missed {} · scope named-but-blocked {} · scope unnamed-but-allowed {}",
            rate(wrong_blocks, allow.len()),
            rate(misses, block.len()),
            rate(scope_wrong_blocks, scope_allow.len()),
            rate(scope_misses, scope_block.len()),
        );
    };
    println!();
    summary("all", None);
    summary("tuning set", Some(false));
    summary("held out", Some(true));
    #[allow(clippy::cast_precision_loss)]
    let cost = input_tokens as f64 * price_in / 1e6 + output_tokens as f64 * price_out / 1e6;
    println!(
        "tokens: {calls} model calls, {input_tokens} input, {output_tokens} output; estimated ${cost:.3} at ${price_in}/${price_out} per million"
    );
}
