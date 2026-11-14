//! The permission corpus (`src/approval/corpus/`, embedded in the binary
//! and read through [`rkat_tui::approval::corpus`]): labelled must-allow,
//! must-block and scope-pair cases, and two runners.
//!
//! - [`every_case_passes_the_deterministic_tiers_as_labelled`] runs every
//!   case through the deterministic tiers alone, no model: no must-allow
//!   case may be denied, sealed or put to the user there, and each case
//!   that names a `deterministic` outcome must get it.
//! - [`live_corpus`] (ignored) sends the cases the deterministic tiers
//!   leave to the classifier through a real [`LlmClassifier`], judged by
//!   the same evaluation as the daemon's `/auto check`
//!   ([`corpus::evaluate`]), and prints a table: verdict against label,
//!   the wrong-block rate on must-allow, the miss rate on must-block, the
//!   scope pairs, the held-out third apart, and the tokens spent. Run it
//!   with
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
//!   the cases sent (default 120, spread as `/auto check`'s sample is),
//!   `RKAT_CORPUS_PRICE_IN`/`_OUT` (dollars per million tokens) price the
//!   estimate. Stage 2 runs without its file-reading tools. The runner
//!   prints no request, header or environment variable; a provider error
//!   stops the run. A user measures their own model with `/auto check`
//!   instead, through the daemon's configured classifier.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use meerkat_core::ToolSourceKind;
use rkat_tui::approval::catalog::ToolOrigin;
use rkat_tui::approval::corpus::{self, Case, Deterministic, Label, Places, Set};
use rkat_tui::approval::policy::{Action, PolicyContext, Tier, classify_tier};
use rkat_tui::approval::rules::RuleSet;
use rkat_tui::paths::ProjectRoot;
use rkat_tui::tools::{FILE_TOOL_NAMES, FILE_TOOLS_SOURCE};

fn origin(name: &str) -> ToolOrigin {
    if FILE_TOOL_NAMES.contains(&name) {
        ToolOrigin::tui(FILE_TOOLS_SOURCE, name)
    } else {
        ToolOrigin::native(ToolSourceKind::Builtin)
    }
}

/// Every set's cases.
fn all_cases() -> Vec<Case> {
    corpus::cases().unwrap()
}

#[test]
fn the_corpus_is_large_enough_and_well_formed() {
    let cases = all_cases();
    let count = |set: Set| cases.iter().filter(|c| c.set == set).count();
    assert!(count(Set::MustAllow) >= 40, "{}", count(Set::MustAllow));
    assert!(count(Set::MustBlock) >= 40, "{}", count(Set::MustBlock));
    let pairs: BTreeMap<&str, Vec<Label>> = cases
        .iter()
        .filter_map(|c| c.pair.as_deref().map(|p| (p, c.expect)))
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
    let mut ids: Vec<&str> = cases.iter().map(|c| c.id.as_str()).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before, "case ids are unique");
    let held = cases.iter().filter(|c| c.held_out).count();
    assert!(
        held * 4 >= cases.len() && held * 2 <= cases.len(),
        "{held} held out"
    );
    for case in &cases {
        let want = match case.set {
            Set::MustAllow => Some(Label::Allow),
            Set::MustBlock => Some(Label::Block),
            Set::Scope => None,
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
    for case in all_cases() {
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
                "{}/{}: {} ({})",
                case.set.name(),
                case.id,
                decision.tier,
                decision.detail
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
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
    let selection = corpus::Selection {
        sets: env_list("RKAT_CORPUS_SETS")
            .map(|sets| sets.iter().filter_map(|s| Set::parse(s)).collect())
            .unwrap_or_default(),
        limit: corpus::Limit::Count(
            std::env::var("RKAT_CORPUS_MAX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(120),
        ),
        cases: env_list("RKAT_CORPUS_CASES").unwrap_or_default(),
    };
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
    let classifier = LlmClassifier::new(client, provider, model.clone()).with_policy(policy);
    let cases = corpus::select(&selection).unwrap();
    let deterministic = corpus::cases()
        .unwrap()
        .iter()
        .filter(|c| !c.reaches_classifier())
        .count();
    let report = corpus::evaluate(&classifier, &cases, corpus::Bounds::default(), |_, _| {}).await;
    if let Some(stopped) = &report.stopped {
        eprintln!("stopping: {stopped:?}");
    }

    println!(
        "\ncorpus run: model {model}, policy {policy:?}, {} cases sent, {deterministic} decided deterministically\n",
        report.outcomes.len()
    );
    println!("| set | case | held out | label | verdict | rule | stage | ms | ok |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for o in &report.outcomes {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            o.set.name(),
            o.id,
            if o.held_out { "yes" } else { "" },
            o.label.word(),
            o.verdict.word(),
            o.rule.as_deref().unwrap_or(""),
            o.stage,
            o.latency_ms,
            if o.right() { "" } else { "WRONG" }
        );
    }
    let summary = |label: &str, held: Option<bool>| {
        let pick = |set: Set, expect: Label| {
            report
                .outcomes
                .iter()
                .filter(|o| o.set == set && o.label == expect)
                .filter(|o| held.is_none_or(|h| o.held_out == h))
                .collect::<Vec<_>>()
        };
        let allow = pick(Set::MustAllow, Label::Allow);
        let block = pick(Set::MustBlock, Label::Block);
        let scope_allow = pick(Set::Scope, Label::Allow);
        let scope_block = pick(Set::Scope, Label::Block);
        let wrong = |cases: &[&corpus::Outcome]| cases.iter().filter(|o| !o.right()).count();
        println!(
            "{label}: must-allow wrongly blocked {} · must-block missed {} · scope named-but-blocked {} · scope unnamed-but-allowed {}",
            rate(wrong(&allow), allow.len()),
            rate(wrong(&block), block.len()),
            rate(wrong(&scope_allow), scope_allow.len()),
            rate(wrong(&scope_block), scope_block.len()),
        );
    };
    println!();
    summary("all", None);
    summary("tuning set", Some(false));
    summary("held out", Some(true));
    println!("\n/auto check would print:\n{}", report.render(&model));
    #[allow(clippy::cast_precision_loss)]
    let cost =
        report.input_tokens as f64 * price_in / 1e6 + report.output_tokens as f64 * price_out / 1e6;
    println!(
        "tokens: {} input, {} output; estimated ${cost:.3} at ${price_in}/${price_out} per million",
        report.input_tokens, report.output_tokens
    );
}
