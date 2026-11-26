#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::settings::{ChecklistView, CompactMode, HelperView};
use super::write::SettingsFile;
use super::*;
use crate::approval::mode::PermissionMode;
use crate::approval::rules::{ArgPattern, Rule, RuleKind, RuleOrigin, RuleSet, RulesError};
use crate::bootstrap::confinement::ConfinementSection;
use crate::thinking::ThinkingSetting;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn allow(tool: &str, arg: Option<&str>) -> Rule {
    Rule::new(RuleKind::Allow, tool, arg, RuleOrigin::Realm).unwrap()
}

#[test]
fn rules_file_round_trip() {
    let file = RulesFile {
        daemon: Some(DaemonSection {
            principal: Some("alice".into()),
        }),
        auto: Some(AutoSection {
            mode: Some(PermissionMode::Auto),
            classifier_model: Some("claude-sonnet-4-6".into()),
            trust_project_builds: Some(false),
            ..AutoSection::default()
        }),
        allow: vec![
            RuleEntry {
                tool: "shell".into(),
                arg: Some("cargo nextest run*".into()),
                literal: None,
            },
            RuleEntry {
                tool: "shell".into(),
                arg: None,
                literal: Some("make lint [*]".into()),
            },
        ],
        deny: vec![RuleEntry {
            tool: "mcp__prod__*".into(),
            arg: None,
            literal: None,
        }],
        endpoints: BTreeMap::from([(
            "litellm".to_string(),
            EndpointSection {
                dialect: Some("openai".into()),
                base_url: Some("http://localhost:4000".into()),
                api_key_env: Some("LITELLM_KEY".into()),
                api_key: None,
                priority: Some(2),
                api_style: Some(crate::endpoints::ApiStyle::ChatCompletions),
                models: Some(vec!["model-a".into()]),
            },
        )]),
        pricing: PricingTable {
            fetch: None,
            models: BTreeMap::from([(
                "gpt-5.4".to_string(),
                PricingSection {
                    input: Some(1.25),
                    output: Some(10.0),
                    cached_input: None,
                    cache_write: None,
                },
            )]),
        },
        thinking: BTreeMap::from([
            ("litellm:claude-x".to_string(), ThinkingSetting::Adaptive),
            (
                "claude-sonnet-4-5".to_string(),
                ThinkingSetting::Enabled {
                    budget_tokens: 8000,
                },
            ),
        ]),
        confinement: Some(ConfinementSection {
            extra_read: vec![PathBuf::from("/data")],
            extra_write: vec![PathBuf::from("/scratch")],
        }),
        agent: Some(AgentSection {
            checkpoint_steps: Some(40),
            checkpoint_continues: Some(0),
            web_search: Some(false),
            web_fetch: Some(false),
        }),
        instructions: Some(InstructionsSection {
            files: Some(vec![
                InstructionPath::parse("/opt/wrapper/notes.md").unwrap(),
            ]),
        }),
        helpers: Some(HelpersSection {
            max_depth: Some(2),
            ..HelpersSection::default()
        }),
        shell: Some(ShellSection {
            user_timeout_secs: Some(300),
        }),
        search: Some(SearchSection {
            budget_secs: Some(30),
            max_entries: None,
        }),
        ui: Some(UiSection {
            helpers: Some(HelperView::Expanded),
            bell: Some(false),
            mouse: Some(false),
            checklist: Some(ChecklistView::Shown),
        }),
    };
    let text = toml::to_string_pretty(&file).unwrap();
    let back: RulesFile = toml::from_str(&text).unwrap();
    assert_eq!(back, file);
    let parsed: RulesFile = toml::from_str(
        "[auto]\nmode = \"auto\"\n\n[[allow]]\ntool = \"shell\"\narg = \"cargo nextest run*\"\n",
    )
    .unwrap();
    assert_eq!(parsed.allow.len(), 1);
    // `enabled`, an earlier spelling of the mode, is gone: a file that
    // still has it does not read (it is never ignored: ignoring
    // `enabled = false` would start in auto mode).
    let gone = toml::from_str::<RulesFile>("[auto]\nenabled = false\n").unwrap_err();
    assert!(
        gone.to_string().contains("unknown field `enabled`"),
        "{gone}"
    );
    assert!(toml::from_str::<RulesFile>("[[allow]]\ntool=\"x\"\nbogus=1\n").is_err());
    let rules = file.rules(RuleOrigin::Realm).unwrap();
    assert_eq!(
        rules[1].arg,
        Some(ArgPattern::Literal("make lint [*]".into()))
    );
    assert_eq!(rules[1].to_entry(), file.allow[1]);
    let both: RulesFile =
        toml::from_str("[[allow]]\ntool=\"x\"\narg=\"a\"\nliteral=\"b\"\n").unwrap();
    assert!(matches!(
        both.rules(RuleOrigin::Realm),
        Err(RulesError::ArgAndLiteral { .. })
    ));
}

#[test]
fn layered_loading() {
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm");
    let home = tmp.path().join("home");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        home.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = false\n[[deny]]\ntool = \"shell\"\narg = \"git push*\"\n",
    )
    .unwrap();
    std::fs::write(
        realm.join(RULES_FILE_NAME),
        "[auto]\nclassifier_model = \"gpt-5.2\"\n[[allow]]\ntool = \"shell\"\narg = \"make lint\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = true\nmode = \"default\"\n[[allow]]\ntool = \"shell\"\n[[deny]]\ntool = \"agent_spawn\"\n",
    )
    .unwrap();
    let (rules, settings, notices) =
        load_layered(&realm, Some(&home.join(RULES_FILE_NAME)), &project).unwrap();
    assert_eq!(rules.allow().len(), 1);
    assert_eq!(rules.allow().next().unwrap().1.origin, RuleOrigin::Realm);
    assert_eq!(rules.deny().len(), 2);
    assert_ne!(settings.trust_project_builds, Some(true));
    assert_eq!(settings.classifier_model.as_deref(), Some("gpt-5.2"));
    assert_eq!(notices.len(), 2);
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, Notice::ProjectAllowIgnored { count: 1, .. }))
    );
    // The project's `mode = "default"` turns the classifier off.
    let layered =
        load_layered_with(&realm, Some(&home.join(RULES_FILE_NAME)), &project, None).unwrap();
    assert_eq!(layered.mode, PermissionMode::Manual);
}

#[test]
fn global_rules_come_from_the_file_path() {
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("cfg").join("custom.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(
        &global,
        "[[allow]]\ntool = \"shell\"\narg = \"make test\"\n[[deny]]\ntool = \"shell\"\narg = \"rm *\"\n",
    )
    .unwrap();
    let (rules, _, notices) =
        load_layered(&tmp.path().join("realm"), Some(&global), tmp.path()).unwrap();
    assert_eq!(rules.allow().len(), 1);
    assert_eq!(rules.allow().next().unwrap().1.origin, RuleOrigin::Global);
    assert_eq!(rules.deny().len(), 1);
    assert_eq!(rules.deny().next().unwrap().1.origin, RuleOrigin::Global);
    assert!(notices.is_empty());
    // A missing global file is not an error.
    let (rules, _, _) = load_layered(
        &tmp.path().join("realm"),
        Some(&tmp.path().join("missing.toml")),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(rules, RuleSet::default());
}

#[test]
fn missing_files_give_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let (rules, settings, notices) =
        load_layered(tmp.path(), None, &tmp.path().join("nope")).unwrap();
    assert_eq!(rules, RuleSet::default());
    assert_eq!(settings, Typed::defaults().unwrap().settings);
    assert!(notices.is_empty());
}

#[test]
fn invalid_file_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(RULES_FILE_NAME),
        "[[allow]]\ntool = \"[\"\n",
    )
    .unwrap();
    assert!(matches!(
        load_layered(tmp.path(), None, tmp.path()),
        Err(RulesError::Pattern { .. })
    ));
    std::fs::write(tmp.path().join(RULES_FILE_NAME), "not toml [").unwrap();
    assert!(matches!(
        load_layered(tmp.path(), None, tmp.path()),
        Err(RulesError::Parse { .. })
    ));
}

/// A rule written to the realm keeps the rest of the file, and nothing
/// is left beside it.
#[test]
fn a_rule_write_preserves_the_auto_section() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("deep/realm").join(RULES_FILE_NAME);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "[auto]\nmode = \"default\"\n").unwrap();
    let file = SettingsFile::new(registry::Level::Workspace, path.clone());
    file.add_rule("allow", &allow("shell", Some("make lint")).to_entry())
        .unwrap();
    file.add_rule(
        "deny",
        &Rule::new(RuleKind::Deny, "shell", Some("rm *"), RuleOrigin::Session)
            .unwrap()
            .to_entry(),
    )
    .unwrap();
    let back = RulesFile::load(&path).unwrap();
    assert_eq!(back.auto.unwrap().mode, Some(PermissionMode::Manual));
    assert_eq!(back.allow.len(), 1);
    assert_eq!(back.deny.len(), 1);
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn compaction_settings_default_layer_and_clamp() {
    let defaults = Typed::defaults().unwrap().settings;
    assert_eq!(defaults.compact, CompactMode::Native);
    assert_eq!(
        (defaults.compact_warn_percent, defaults.compact_auto_percent),
        (75, 85)
    );
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("global.toml");
    let realm = tmp.path().join("realm");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let (_, settings, notices) = load_layered(&realm, Some(&global), &project).unwrap();
    assert_eq!(settings, defaults);
    assert!(notices.is_empty());

    std::fs::write(
        &global,
        "[auto]\ncompact = \"auto\"\ncompact_warn_percent = 60\ncompact_auto_percent = 70\n",
    )
    .unwrap();
    std::fs::write(
        realm.join(RULES_FILE_NAME),
        "[auto]\ncompact_auto_percent = 90\n",
    )
    .unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ncompact = \"warn\"\n",
    )
    .unwrap();
    let (_, settings, notices) = load_layered(&realm, Some(&global), &project).unwrap();
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(
        settings.compact,
        CompactMode::Warn,
        "the project layer may set it"
    );
    assert_eq!(
        (settings.compact_warn_percent, settings.compact_auto_percent),
        (60, 90)
    );
    let policy = settings.compaction_policy(None);
    assert_eq!(policy.mode, CompactMode::Warn);
    assert_eq!((policy.warn_percent, policy.auto_percent), (60, 90));
    assert_eq!(
        settings.compaction_policy(Some(CompactMode::Auto)).mode,
        CompactMode::Auto,
        "--auto-compact overrides the file"
    );

    // auto below warn is clamped up, with a notice.
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ncompact_warn_percent = 95\ncompact_auto_percent = 80\n",
    )
    .unwrap();
    let (_, settings, notices) = load_layered(&realm, Some(&global), &project).unwrap();
    assert_eq!(
        (settings.compact_warn_percent, settings.compact_auto_percent),
        (95, 95)
    );
    assert_eq!(
        notices,
        vec![Notice::CompactPercentClamped {
            configured: (95, 80),
            applied: (95, 95),
        }]
    );

    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ncompact = \"always\"\n",
    )
    .unwrap();
    assert!(load_layered(&realm, Some(&global), &project).is_err());
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ncompact_warn_percent = 300\n",
    )
    .unwrap();
    assert!(
        load_layered(&realm, Some(&global), &project).is_err(),
        "u8 overflow is a parse error"
    );
}

#[test]
fn trust_defaults_off_and_project_can_only_lower_it() {
    assert_eq!(
        Typed::defaults().unwrap().settings.trust_project_builds,
        None
    );
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = true\n",
    )
    .unwrap();
    let (_, settings, notices) = load_layered(&realm, None, &project).unwrap();
    assert_ne!(settings.trust_project_builds, Some(true));
    assert!(matches!(notices[..], [Notice::ProjectTrustIgnored { .. }]));

    std::fs::write(
        realm.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = true\n",
    )
    .unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = false\n",
    )
    .unwrap();
    let (_, settings, _) = load_layered(&realm, None, &project).unwrap();
    assert_eq!(
        settings.trust_project_builds,
        Some(false),
        "project lowers trust"
    );
    std::fs::remove_file(project.join(RULES_FILE_NAME)).unwrap();
    let (_, settings, _) = load_layered(&realm, None, &project).unwrap();
    assert_eq!(
        settings.trust_project_builds,
        Some(true),
        "the user's realm may raise it"
    );
}

/// A project file cannot pick the classifier's
/// model; the user's layers keep theirs and the default stays the default.
#[test]
fn a_project_file_cannot_pick_the_classifier_model() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "[auto]\nclassifier_model = \"tiny-model\"\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.settings.classifier_model, None);
    assert!(
        matches!(&layered.notices[..], [Notice::ProjectClassifierModelIgnored { model, .. }] if model == "tiny-model"),
        "{:?}",
        layered.notices
    );
    assert!(layered.notices[0].to_string().contains("classifier_model"));
    write(&realm, "[auto]\nclassifier_model = \"realm-model\"\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(
        layered.settings.classifier_model.as_deref(),
        Some("realm-model")
    );
}

/// What the registry's layering test (`every_setting_layers_as_it_always_did`,
/// whose samples stay clear of the clamps) leaves out: the clamps of a
/// value out of range, `--no-mouse` through the parser, and `[pricing]
/// fetch` read apart from the price entries beside it.
#[test]
fn the_layering_clamps_parses_no_mouse_and_reads_pricing_fetch_apart() {
    use clap::Parser as _;
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&project, "");
    write(&realm, "[search]\nmax_entries = 0\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.search.max_entries, 1, "at least one entry");
    write(&realm, "[shell]\nuser_timeout_secs = 0\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 1);
    // `--no-mouse`, the command-line layer, wins over a project that
    // turns it on.
    write(&realm, "");
    write(&project, "[ui]\nmouse = true\n");
    let args = crate::cli::Args::parse_from(["rkat-tui", "--no-mouse"]);
    let overlay = crate::overlay::ConfigOverlay::from_args(&args)
        .unwrap()
        .rules_file()
        .unwrap()
        .unwrap();
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    assert!(!layered.ui.mouse);
    // `[pricing] fetch` beside the prices is no price entry, and a
    // project's is no ignored price.
    write(
        &realm,
        "[pricing]\nfetch = false\n[pricing.\"m\"]\ninput = 1.0\n",
    );
    write(&project, "[pricing]\nfetch = true\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert!(!layered.pricing_fetch, "the project cannot turn it back on");
    assert_eq!(layered.pricing["m"].input, Some(1.0));
    assert!(
        !layered
            .notices
            .iter()
            .any(|n| matches!(n, Notice::PricingIgnored { .. })),
        "{:?}",
        layered.notices
    );
}

fn write(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(RULES_FILE_NAME), text).unwrap();
}

fn cli(text: &str) -> RulesFile {
    toml::from_str(text).unwrap()
}

#[test]
fn ui_checklist_defaults_to_hidden_layers_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "[auto]\nmode = \"plan\"\n[ui]\nbell = false\n");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.ui.checklist, ChecklistView::Hidden);
    let realm_file = realm.join(RULES_FILE_NAME);
    SettingsFile::new(registry::Level::Workspace, realm_file.clone())
        .set("ui.checklist", Some(&toml::Value::String("shown".into())))
        .unwrap();
    let text = std::fs::read_to_string(&realm_file).unwrap();
    assert!(text.contains("checklist = \"shown\""), "{text}");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.ui.checklist, ChecklistView::Shown);
    // The rest of the file stays.
    assert!(!layered.ui.bell);
    assert_eq!(layered.mode, PermissionMode::Plan);
    write(&project, "[ui]\nchecklist = \"open\"\n");
    assert!(load_layered_with(&realm, None, &project, None).is_err());
}

#[test]
fn cli_layer_beats_realm_but_project_deny_and_lowering_still_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(
        &realm,
        "[auto]\nmode = \"auto\"\ncompact = \"warn\"\nclassifier_model = \"realm-model\"\n[[allow]]\ntool = \"read_file\"\n",
    );
    write(
        &project,
        "[auto]\ncompact = \"warn\"\ncompact_warn_percent = 50\ntrust_project_builds = false\n[[deny]]\ntool = \"shell\"\narg = \"git push*\"\n",
    );
    let overlay = cli(
        "[auto]\nmode = \"default\"\ncompact = \"auto\"\ntrust_project_builds = true\n[[allow]]\ntool = \"shell\"\narg = \"cargo nextest run*\"\n[[deny]]\ntool = \"shell\"\narg = \"git push*\"\n",
    );
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    let s = &layered.settings;
    assert_eq!(layered.mode, PermissionMode::Manual, "cli beats realm");
    assert_eq!(s.compact, CompactMode::Auto, "cli beats project");
    assert_eq!(s.classifier_model.as_deref(), Some("realm-model"));
    assert_eq!(
        s.compact_warn_percent, 50,
        "project fills what cli left unset"
    );
    assert_eq!(
        s.trust_project_builds,
        Some(false),
        "project may lower trust"
    );
    let allow: Vec<(RuleOrigin, &str)> = layered
        .rules
        .allow()
        .map(|(_, r)| (r.origin, r.tool.as_str()))
        .collect();
    assert_eq!(
        allow,
        vec![(RuleOrigin::Realm, "read_file"), (RuleOrigin::Cli, "shell")]
    );
    // The project's deny duplicates the cli's: one rule, the cli's.
    assert_eq!(layered.rules.deny().len(), 1);
    assert!(
        layered
            .rules
            .matches_deny("shell", &["git push origin".to_string()])
            .is_some()
    );
    // Without the cli deny, the project's still applies.
    let layered = load_layered_with(
        &realm,
        None,
        &project,
        Some(&cli("[[allow]]\ntool = \"shell\"\narg = \"git push*\"\n")),
    )
    .unwrap();
    assert_eq!(
        layered.rules.deny().next().unwrap().1.origin,
        RuleOrigin::Project
    );
    assert!(
        layered
            .rules
            .matches_deny("shell", &["git push".to_string()])
            .is_some()
    );
}

#[test]
fn pricing_layers_merge_by_field_and_only_user_layers_count() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global").join(RULES_FILE_NAME);
    write(
        global.parent().unwrap(),
        "[pricing.\"claude-opus-4-6\"]\ninput = 5\noutput = 25\ncached_input = 0.5\n",
    );
    write(
        &realm,
        "[pricing.\"claude-opus-4-6\"]\noutput = 30\n[pricing.bad]\ninput = -1\noutput = 1\n",
    );
    write(&project, "[pricing.\"gpt-5.4\"]\ninput = 0\noutput = 0\n");
    let overlay = cli("[pricing.\"claude-opus-4-6\"]\ninput = 15\n");
    let layered = load_layered_with(&realm, Some(&global), &project, Some(&overlay)).unwrap();
    assert_eq!(
        layered.pricing.get("claude-opus-4-6"),
        Some(&PricingSection {
            input: Some(15.0),
            output: Some(30.0),
            cached_input: Some(0.5),
            cache_write: None,
        })
    );
    assert!(
        !layered.pricing.contains_key("gpt-5.4"),
        "project prices ignored"
    );
    assert!(
        !layered.pricing.contains_key("bad"),
        "negative price dropped"
    );
    let texts: Vec<String> = layered.notices.iter().map(ToString::to_string).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("ignored 1 [pricing] entry")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("`input` must be")),
        "{texts:?}"
    );
}

#[test]
fn thinking_layers_replace_by_key_and_only_user_layers_count() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global").join(RULES_FILE_NAME);
    write(
        global.parent().unwrap(),
        "[thinking]\n\"a\" = \"adaptive\"\n\"b\" = { enabled = 4096 }\n",
    );
    write(&realm, "[thinking]\n\"b\" = \"default\"\n");
    write(&project, "[thinking]\n\"c\" = \"adaptive\"\n");
    let overlay = cli("[thinking]\n\"a\" = { enabled = 2048 }\n");
    let layered = load_layered_with(&realm, Some(&global), &project, Some(&overlay)).unwrap();
    assert_eq!(
        layered.thinking,
        BTreeMap::from([
            (
                "a".to_string(),
                ThinkingSetting::Enabled {
                    budget_tokens: 2048
                }
            ),
            ("b".to_string(), ThinkingSetting::Default),
        ]),
        "the project's setting is ignored"
    );
    let texts: Vec<String> = layered.notices.iter().map(ToString::to_string).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("ignored 1 [thinking] setting(s)")),
        "{texts:?}"
    );
}

#[test]
fn endpoints_layer_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global").join(RULES_FILE_NAME);
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(
        &global,
        "[endpoints.litellm]\ndialect = \"openai\"\nbase_url = \"http://global\"\napi_key_env = \"LITELLM_KEY\"\n",
    )
    .unwrap();
    write(
        &realm,
        "[endpoints.litellm]\nbase_url = \"http://realm\"\npriority = 2\n",
    );
    write(
        &project,
        "[endpoints.litellm]\nbase_url = \"http://evil\"\n[endpoints.team]\ndialect = \"anthropic\"\napi_key = \"k\"\n",
    );
    let overlay = cli("[endpoints.litellm]\nbase_url = \"http://cli\"\n");
    let layered = load_layered_with(&realm, Some(&global), &project, Some(&overlay)).unwrap();
    let litellm = &layered.endpoints["litellm"];
    assert_eq!(litellm.origin, RuleOrigin::Global);
    assert_eq!(litellm.section.dialect.as_deref(), Some("openai"));
    assert_eq!(litellm.section.base_url.as_deref(), Some("http://cli"));
    assert_eq!(litellm.section.api_key_env.as_deref(), Some("LITELLM_KEY"));
    assert_eq!(litellm.section.priority, Some(2));
    // Project endpoints are never used, new or not.
    assert!(!layered.endpoints.contains_key("team"));
    for name in ["litellm", "team"] {
        assert!(
            layered
                .notices
                .iter()
                .any(|n| matches!(n, Notice::ProjectEndpointIgnored { name: n, .. } if n == name)),
            "{name}"
        );
    }
}

#[test]
fn confinement_grants_come_from_user_layers_only() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global").join(RULES_FILE_NAME);
    write(
        global.parent().unwrap(),
        "[confinement]\nextra_read = [\"/g/read\"]\nextra_write = [\"/g/write\"]\n",
    );
    write(&realm, "[confinement]\nextra_read = [\"/r/read\"]\n");
    write(
        &project,
        "[confinement]\nextra_read = [\"/p/a\"]\nextra_write = [\"/p/b\"]\n",
    );
    let overlay = cli("[confinement]\nextra_write = [\"/c/write\"]\n");
    let layered = load_layered_with(&realm, Some(&global), &project, Some(&overlay)).unwrap();
    // Global, realm and command-line overlay accumulate in order;
    // the project's are ignored with a notice.
    assert_eq!(
        layered.confinement,
        ConfinementSection {
            extra_read: vec![PathBuf::from("/g/read"), PathBuf::from("/r/read")],
            extra_write: vec![PathBuf::from("/g/write"), PathBuf::from("/c/write")],
        }
    );
    assert!(
        layered
            .notices
            .iter()
            .any(|n| matches!(n, Notice::ProjectConfinementIgnored { count: 2, .. }))
    );
    let texts: Vec<String> = layered.notices.iter().map(ToString::to_string).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("ignored 2 [confinement] grant(s)")),
        "{texts:?}"
    );

    // Unknown keys in the table are a parse error.
    write(&realm, "[confinement]\nextra = [\"/x\"]\n");
    assert!(load_layered_with(&realm, None, &project, None).is_err());
}

/// `[auto] trusted_hosts` come from the user's own layers only: the
/// user-level file's and the realm's add up, checked; a project file's
/// are ignored with a start-up notice that says why.
#[test]
fn trusted_hosts_come_from_the_user_s_layers_only() {
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, "[auto]\ntrusted_hosts = [\"ci.example.net\"]\n").unwrap();
    std::fs::write(
        realm.join(RULES_FILE_NAME),
        "[auto]\ntrusted_hosts = [\"*.corp.example\", \"*.com\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ntrusted_hosts = [\"evil.example\"]\n",
    )
    .unwrap();
    let (_, settings, notices) = load_layered(&realm, Some(&global), &project).unwrap();
    assert_eq!(
        crate::approval::hosts::patterns(&settings.trusted_hosts),
        vec!["ci.example.net".to_string(), "*.corp.example".to_string()]
    );
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, Notice::ProjectTrustedHostsIgnored { count: 1, .. })),
        "{notices:?}"
    );
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, Notice::TrustedHostIgnored { text, .. } if text == "*.com")),
        "{notices:?}"
    );
    assert!(notices.iter().any(|n| {
        n.to_string()
            .contains("only your own tui.toml or this workspace's says which hosts you trust")
    }));
}

/// `instructions.files` come from the user's own layers only and add up:
/// the user's file, the realm's, then the command line's (`--set` appends
/// to the user's list); a project file's are ignored with a notice; a
/// missing or empty file is a notice, not a refusal; `~` expands.
#[test]
fn instruction_files_add_up_over_the_user_s_layers_only() {
    use super::registry::instruction_files_in_force;
    use clap::Parser as _;
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm");
    let project = tmp.path().join("project");
    let notes = tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let (user_file, realm_file, cli_file, empty) = (
        notes.join("user.md"),
        notes.join("realm.md"),
        notes.join("cli.md"),
        notes.join("empty.md"),
    );
    for path in [&user_file, &realm_file, &cli_file] {
        std::fs::write(path, "rules\n").unwrap();
    }
    std::fs::write(&empty, "\n").unwrap();
    let missing = notes.join("missing.md");
    let global = tmp.path().join("global.toml");
    std::fs::write(
        &global,
        format!(
            "[instructions]\nfiles = [{:?}, {:?}]\n",
            user_file.display().to_string(),
            empty.display().to_string()
        ),
    )
    .unwrap();
    write(
        &realm,
        &format!(
            "[instructions]\nfiles = [{:?}]\n",
            realm_file.display().to_string()
        ),
    );
    write(&project, "[instructions]\nfiles = [\"/etc/evil.md\"]\n");
    // `--set` arrays append, here to the user's list.
    let args = crate::cli::Args::parse_from([
        "rkat-tui".to_string(),
        "--set".to_string(),
        format!("instructions.files=[{:?}]", cli_file.display().to_string()),
        "--set".to_string(),
        format!("instructions.files=[{:?}]", missing.display().to_string()),
    ]);
    let overlay = crate::overlay::ConfigOverlay::from_args(&args)
        .unwrap()
        .rules_file()
        .unwrap();
    let layered = load_layered_with(&realm, Some(&global), &project, overlay.as_ref()).unwrap();
    assert_eq!(
        instruction_files_in_force(&layered.layers),
        [
            user_file,
            empty.clone(),
            realm_file,
            cli_file,
            missing.clone()
        ]
    );
    let notices: Vec<String> = layered.notices.iter().map(ToString::to_string).collect();
    assert!(
        layered
            .notices
            .iter()
            .any(|n| matches!(n, Notice::ProjectInstructionsIgnored { .. })),
        "{notices:?}"
    );
    for (path, why) in [(&empty, "it is empty"), (&missing, "there is no such file")] {
        assert!(
            notices.contains(&format!(
                "instructions.files: {} is skipped ({why}); new conversations and helpers are built without it",
                path.display()
            )),
            "{notices:?}"
        );
    }
    assert!(
        !notices.iter().any(|n| n.contains("user.md")),
        "{notices:?}"
    );
    // Without the command line, the user's and the realm's.
    let layered = load_layered_with(&realm, Some(&global), &project, None).unwrap();
    assert_eq!(instruction_files_in_force(&layered.layers).len(), 3);
    // `~` expands; the file keeps the entry as written.
    let home = InstructionPath::parse("~/wrapper.md").unwrap();
    if let Some(dir) = dirs::home_dir() {
        assert_eq!(home.path(), dir.join("wrapper.md"));
    }
    assert_eq!(String::from(home), "~/wrapper.md");
}

/// `agent.web_fetch` is on by default; the user's file and `--set` set
/// it; a project file may only turn it off; and it is a setting apart
/// from `agent.web_search` (turning one off leaves the other on).
#[test]
fn web_fetch_is_on_unless_a_layer_turns_it_off_and_is_apart_from_web_search() {
    use super::registry::{web_fetch_in_force, web_search_in_force};
    use clap::Parser as _;
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global.toml");
    let layered = |global_text: &str, project_text: &str, set: &[&str]| {
        std::fs::write(&global, global_text).unwrap();
        write(&realm, "");
        write(&project, project_text);
        let mut argv = vec!["rkat-tui".to_string()];
        for value in set {
            argv.push("--set".to_string());
            argv.push((*value).to_string());
        }
        let overlay = crate::overlay::ConfigOverlay::from_args(&crate::cli::Args::parse_from(argv))
            .unwrap()
            .rules_file()
            .unwrap();
        let layered = load_layered_with(&realm, Some(&global), &project, overlay.as_ref()).unwrap();
        (
            web_fetch_in_force(&layered.layers),
            web_search_in_force(&layered.layers),
        )
    };
    assert_eq!(layered("", "", &[]), (true, true));
    assert_eq!(
        layered("[agent]\nweb_fetch = false\n", "", &[]),
        (false, true)
    );
    assert_eq!(layered("", "", &["agent.web_fetch=false"]), (false, true));
    assert_eq!(layered("", "", &["agent.web_search=false"]), (true, false));
    assert_eq!(
        layered(
            "[agent]\nweb_fetch = false\n",
            "[agent]\nweb_fetch = true\n",
            &[]
        ),
        (false, true)
    );
    assert_eq!(
        layered(
            "",
            "[agent]\nweb_fetch = false\n",
            &["agent.web_fetch=true"]
        ),
        (false, true)
    );
    let setting = super::registry::Setting::find("agent.web_fetch").unwrap();
    assert_eq!(setting.parse("false").unwrap(), toml::Value::Boolean(false));
    let setting = super::registry::Setting::find("agent.web_search").unwrap();
    assert!(setting.parse("sometimes").is_err());
}

/// A relative `instructions.files` entry is refused, naming the key and
/// the value: on the command line (fatal at start), in a file (the file
/// does not read, so the start is refused as for any bad file) and in
/// `/set`.
#[test]
fn a_relative_instruction_file_is_refused() {
    use clap::Parser as _;
    let refusal = "instructions.files: `notes/x.md` is not an absolute path";
    let args =
        crate::cli::Args::parse_from(["rkat-tui", "--set", "instructions.files=[\"notes/x.md\"]"]);
    let err = crate::overlay::ConfigOverlay::from_args(&args).unwrap_err();
    assert!(err.to_string().contains(refusal), "{err}");
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "[instructions]\nfiles = [\"notes/x.md\"]\n");
    write(&project, "");
    let err = load_layered_with(&realm, None, &project, None).unwrap_err();
    assert!(
        err.to_string().contains(refusal) || format!("{err:?}").contains(refusal),
        "{err:?}"
    );
    let setting = super::registry::Setting::find("instructions.files").unwrap();
    let err = setting.parse("/abs/a.md, notes/x.md").unwrap_err();
    assert!(err.contains(refusal), "{err}");
    assert_eq!(
        setting.parse("/abs/a.md, /abs/b.md").unwrap(),
        toml::Value::Array(vec!["/abs/a.md".into(), "/abs/b.md".into()])
    );
}

/// `[auto] allow` statements come from the user's own layers only: the
/// user-level file's are `config`, the realm's `always`; a project file's
/// are ignored with a start-up notice, and a statement that says only
/// "anything" is refused with its reason. The realm writer adds, removes
/// and names the classifier model without touching the rest.
#[test]
fn allow_statements_come_from_the_user_s_layers_only() {
    use crate::approval::statements::StatementOrigin;
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(
        &global,
        "[auto]\nallow = [\"pushing to the default branch of this solo repository\", \"anything\"]\n",
    )
    .unwrap();
    std::fs::write(
        realm.join(RULES_FILE_NAME),
        "[auto]\nmode = \"auto\"\nallow = [\"building the project after the agent changed its dependencies\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\nallow = [\"sending the project's files to any host\"]\n",
    )
    .unwrap();
    let (_, settings, notices) = load_layered(&realm, Some(&global), &project).unwrap();
    assert_eq!(
        settings.allow,
        vec![
            (
                StatementOrigin::Config,
                "pushing to the default branch of this solo repository".to_string()
            ),
            (
                StatementOrigin::Always,
                "building the project after the agent changed its dependencies".to_string()
            ),
        ]
    );
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, Notice::ProjectStatementsIgnored { count: 1, .. })),
        "{notices:?}"
    );
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, Notice::StatementIgnored { text, .. } if text == "anything")),
        "{notices:?}"
    );
    assert!(notices.iter().any(|n| {
        n.to_string()
            .contains("only your own tui.toml says which kinds of action you allow")
    }));

    let file = SettingsFile::new(registry::Level::Workspace, realm.join(RULES_FILE_NAME));
    file.add_to_list("auto.allow", "network requests to api.example.com")
        .unwrap();
    file.add_to_list("auto.allow", "network requests to api.example.com")
        .unwrap();
    file.set(
        "auto.classifier_model",
        Some(&toml::Value::String("claude-haiku-5-5".into())),
    )
    .unwrap();
    file.remove_from_list(
        "auto.allow",
        "building the project after the agent changed its dependencies",
    )
    .unwrap();
    let written = RulesFile::load(&realm.join(RULES_FILE_NAME)).unwrap();
    let auto = written.auto.unwrap();
    assert_eq!(
        auto.allow,
        vec!["network requests to api.example.com".to_string()]
    );
    assert_eq!(auto.classifier_model.as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(auto.mode, Some(crate::approval::mode::PermissionMode::Auto));
}

/// What `/compact prepare` needs at hand to fall back live: the source
/// of each value in force, and what the layers other than the realm's
/// give. The realm's empty instruction (`/compact prepare off`) is off
/// here over the user's own, as every start reads it; a realm file
/// without the key leaves the user's in force.
#[test]
fn the_prepare_setting_knows_its_sources_and_the_lower_layers() {
    use super::settings::{DEFAULT_PREPARE_AT, PrepareAt, PrepareSetting, Sourced};
    let tmp = tempfile::tempdir().unwrap();
    let (home, realm, project) = (
        tmp.path().join("home"),
        tmp.path().join("realm"),
        tmp.path().join("p"),
    );
    let global = home.join(RULES_FILE_NAME);
    write(
        &home,
        "[auto]\ncompact_prepare = \"update PLAN.md\"\ncompact_prepare_at = \"60%\"\n",
    );
    write(&project, "");
    write(&realm, "");
    let setting = PrepareSetting::of(
        &load_layered_with(&realm, Some(&global), &project, None)
            .unwrap()
            .layers,
    );
    let user = Sourced::new(Some("update PLAN.md".to_string()), registry::Level::User);
    assert_eq!(setting.instruction, user);
    assert_eq!(
        setting.at,
        Sourced::new(PrepareAt::Percent(60), registry::Level::User)
    );
    // Off for the workspace, the threshold the workspace's.
    let file = SettingsFile::new(registry::Level::Workspace, realm.join(RULES_FILE_NAME));
    file.set(
        "auto.compact_prepare",
        Some(&toml::Value::String(String::new())),
    )
    .unwrap();
    file.set(
        "auto.compact_prepare_at",
        Some(&toml::Value::Integer(50_000)),
    )
    .unwrap();
    let layered = load_layered_with(&realm, Some(&global), &project, None).unwrap();
    let setting = PrepareSetting::of(&layered.layers);
    assert_eq!(
        setting.instruction,
        Sourced::new(None, registry::Level::Workspace)
    );
    assert_eq!(
        setting.at,
        Sourced::new(PrepareAt::Tokens(50_000), registry::Level::Workspace)
    );
    // Without the workspace's, the user's.
    let beneath = registry::Setting::find("auto.compact_prepare")
        .unwrap()
        .resolve_without(&layered.layers, Some(registry::Level::Workspace));
    assert_eq!(
        (
            beneath.value.as_ref().and_then(toml::Value::as_str),
            beneath.source
        ),
        (Some("update PLAN.md"), registry::Level::User)
    );
    assert_eq!(user.source, registry::Level::User);
    // The command line and the project file are over the realm's.
    let overlay = cli("[auto]\ncompact_prepare_at = \"80%\"\n");
    let setting = PrepareSetting::of(
        &load_layered_with(&realm, Some(&global), &project, Some(&overlay))
            .unwrap()
            .layers,
    );
    assert_eq!(
        setting.at,
        Sourced::new(PrepareAt::Percent(80), registry::Level::CommandLine)
    );
    write(&project, "[auto]\ncompact_prepare = \"write NOTES.md\"\n");
    let setting = PrepareSetting::of(
        &load_layered_with(&realm, Some(&global), &project, None)
            .unwrap()
            .layers,
    );
    assert_eq!(setting.instruction.source, registry::Level::Project);
    // No layer: the default, and none.
    let none = PrepareSetting::of(
        &load_layered_with(&tmp.path().join("r2"), None, &tmp.path().join("p2"), None)
            .unwrap()
            .layers,
    );
    assert_eq!(
        none.instruction,
        Sourced::new(None, registry::Level::Default)
    );
    assert_eq!(
        none.at,
        Sourced::new(DEFAULT_PREPARE_AT, registry::Level::Default)
    );
    // `300k` and `1.2m` read in a file too.
    write(&project, "[auto]\ncompact_prepare_at = \"1.2m\"\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(
        PrepareSetting::of(&layered.layers).at.value,
        PrepareAt::Tokens(1_200_000)
    );
}

/// `[auto] compact_prepare` and `compact_prepare_at` layer like the other
/// compaction keys: user layers, the command line, and the project file
/// where the command line did not set them. The threshold reads as tokens
/// or `"N%"`; unset it is 70%; an empty instruction is none; the realm's
/// trusted writer sets and clears the instruction, keeping the rest.
#[test]
fn the_prepare_instruction_layers_like_the_compaction_keys() {
    use super::settings::{DEFAULT_PREPARE_AT, PrepareAt, PrepareSetting};
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(PrepareSetting::of(&layered.layers).instruction.value, None);
    assert_eq!(
        PrepareSetting::of(&layered.layers).at.value,
        DEFAULT_PREPARE_AT
    );
    assert_eq!(DEFAULT_PREPARE_AT, PrepareAt::Percent(70));

    write(
        &realm,
        "[auto]\ncompact_prepare = \"update PLAN.md\"\ncompact_prepare_at = 300000\n",
    );
    let prepare = PrepareSetting::of(
        &load_layered_with(&realm, None, &project, None)
            .unwrap()
            .layers,
    );
    assert_eq!(prepare.instruction.value.as_deref(), Some("update PLAN.md"));
    assert_eq!(prepare.at.value, PrepareAt::Tokens(300_000));
    // The project file may set them...
    write(&project, "[auto]\ncompact_prepare_at = \"60%\"\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(
        PrepareSetting::of(&layered.layers).at.value,
        PrepareAt::Percent(60)
    );
    // ...unless the command line did (`--set auto.compact_prepare_at=80%`
    // reaches it as a string).
    let overlay = cli("[auto]\ncompact_prepare_at = \"80%\"\ncompact_prepare = \"x\"\n");
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    assert_eq!(
        PrepareSetting::of(&layered.layers).at.value,
        PrepareAt::Percent(80)
    );
    assert_eq!(
        PrepareSetting::of(&layered.layers)
            .instruction
            .value
            .as_deref(),
        Some("x")
    );
    // An empty instruction is none.
    let overlay = cli("[auto]\ncompact_prepare = \"  \"\n");
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    assert_eq!(PrepareSetting::of(&layered.layers).instruction.value, None);
    // Bad thresholds are parse errors.
    for bad in ["0", "\"0%\"", "\"101%\"", "\"soon\""] {
        write(&project, &format!("[auto]\ncompact_prepare_at = {bad}\n"));
        assert!(
            load_layered_with(&realm, None, &project, None).is_err(),
            "{bad}"
        );
    }
    assert_eq!(PrepareAt::parse("300_000"), Ok(PrepareAt::Tokens(300_000)));
    assert_eq!(PrepareAt::Percent(70).tokens(Some(1_000_000)), 700_000);
    assert_eq!(PrepareAt::Percent(50).tokens(None), 100_000);
    // The realm's writer sets and clears it, keeping the rest.
    let file = SettingsFile::new(registry::Level::Workspace, realm.join(RULES_FILE_NAME));
    file.set(
        "auto.compact_prepare",
        Some(&toml::Value::String("write NOTES.md".into())),
    )
    .unwrap();
    let written = RulesFile::load(&realm.join(RULES_FILE_NAME)).unwrap();
    let auto = written.auto.unwrap();
    assert_eq!(auto.compact_prepare.as_deref(), Some("write NOTES.md"));
    assert_eq!(auto.compact_prepare_at, Some(PrepareAt::Tokens(300_000)));
    file.set("auto.compact_prepare", None).unwrap();
    let written = RulesFile::load(&realm.join(RULES_FILE_NAME)).unwrap();
    assert_eq!(written.auto.unwrap().compact_prepare, None);
}

/// The layers of a test: the user's file, the workspace's, the project's
/// (each a `tui.toml` text) and a command line, loaded as the start does
/// (`--replace` keys dropped from the files).
fn layered_from(
    user: &str,
    workspace: &str,
    project_text: &str,
    argv: &[&str],
) -> Result<Layered, String> {
    use clap::Parser as _;
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, user).unwrap();
    write(&realm, workspace);
    write(&project, project_text);
    let mut args = vec!["rkat-tui"];
    args.extend_from_slice(argv);
    let overlay = crate::overlay::ConfigOverlay::from_args(&crate::cli::Args::parse_from(args))
        .map_err(|err| err.to_string())?;
    let cli_layer = overlay.rules_file().map_err(|err| err.to_string())?;
    load_layered_replacing(
        &realm,
        Some(&global),
        &project,
        cli_layer.as_ref(),
        overlay.replaced(),
    )
    .map_err(|err| err.to_string())
}

/// `endpoints.<name>.models` of the layered endpoint `name`.
fn models_of(layered: &Layered, name: &str) -> Option<Vec<String>> {
    layered.endpoints.get(name)?.section.models.clone()
}

/// `--set endpoints.<name>.models` reaches an endpoint `--endpoint-def`
/// defines on the same command line, whichever comes first; the user's,
/// the workspace's and the command line's lists add up (each id once), as
/// `instructions.files` do; a project file may not set it (its endpoint
/// table is ignored with a notice).
#[test]
fn endpoint_models_layer_like_the_other_lists_and_meet_endpoint_def_in_either_order() {
    let def = "gateway=openai=https://llm.example.net,key_env=GATEWAY_KEY";
    let set = r#"endpoints.gateway.models=["model-a", "model-b"]"#;
    for argv in [
        ["--endpoint-def", def, "--set", set],
        ["--set", set, "--endpoint-def", def],
    ] {
        let layered = layered_from("", "", "", &argv).unwrap();
        let section = &layered.endpoints["gateway"].section;
        assert_eq!(section.dialect.as_deref(), Some("openai"), "{argv:?}");
        assert_eq!(section.api_key_env.as_deref(), Some("GATEWAY_KEY"));
        assert_eq!(
            section.models,
            Some(vec!["model-a".to_string(), "model-b".to_string()]),
            "{argv:?}"
        );
    }
    // A file's list alone.
    let layered = layered_from(
        "[endpoints.gateway]\nmodels = [\"model-c\"]\n",
        "",
        "",
        &["--endpoint-def", def],
    )
    .unwrap();
    assert_eq!(
        models_of(&layered, "gateway"),
        Some(vec!["model-c".to_string()])
    );
    // `--set` adds to the files'.
    let layered = layered_from(
        "[endpoints.gateway]\nmodels = [\"model-c\"]\n",
        "[endpoints.gateway]\nmodels = [\"model-d\", \"model-a\"]\n",
        "",
        &["--endpoint-def", def, "--set", set],
    )
    .unwrap();
    assert_eq!(
        models_of(&layered, "gateway"),
        Some(
            ["model-c", "model-d", "model-a", "model-b"]
                .map(String::from)
                .to_vec()
        )
    );
    // The project's is never taken.
    let layered = layered_from(
        "",
        "",
        "[endpoints.gateway]\nmodels = [\"model-z\"]\n",
        &["--endpoint-def", def, "--set", set],
    )
    .unwrap();
    assert_eq!(
        models_of(&layered, "gateway"),
        Some(vec!["model-a".to_string(), "model-b".to_string()])
    );
    assert!(
        layered
            .notices
            .iter()
            .any(|n| matches!(n, Notice::ProjectEndpointIgnored { name, .. } if name == "gateway")),
        "{:?}",
        layered.notices
    );
}

/// `--replace` sets a key to exactly its value: a list key's lists of the
/// user's and the workspace's files are dropped (this key and
/// `instructions.files`), each with a notice; a `--set` of the same key is
/// dropped whichever comes first; for a scalar it is `--set`.
#[test]
fn replace_drops_every_lower_layers_value() {
    let def = "gateway=openai=https://llm.example.net,key_env=GATEWAY_KEY";
    let replace = r#"endpoints.gateway.models=["model-a", "model-b"]"#;
    let files =
        "[endpoints.gateway]\nmodels = [\"model-c\"]\n[instructions]\nfiles = [\"/opt/user.md\"]\n";
    let layered = layered_from(
        files,
        "[endpoints.gateway]\nmodels = [\"model-d\"]\n",
        "",
        &[
            "--endpoint-def",
            def,
            "--replace",
            replace,
            "--replace",
            r#"instructions.files=["/opt/launcher.md"]"#,
        ],
    )
    .unwrap();
    assert_eq!(
        models_of(&layered, "gateway"),
        Some(vec!["model-a".to_string(), "model-b".to_string()])
    );
    // The other fields of the endpoint stay the command line's.
    assert_eq!(
        layered.endpoints["gateway"].section.base_url.as_deref(),
        Some("https://llm.example.net")
    );
    assert_eq!(
        super::registry::instruction_files_in_force(&layered.layers),
        [PathBuf::from("/opt/launcher.md")]
    );
    let notices: Vec<String> = layered.notices.iter().map(ToString::to_string).collect();
    for key in ["endpoints.gateway.models", "instructions.files"] {
        assert!(
            notices
                .iter()
                .any(|n| n.starts_with(key) && n.contains("--replace")),
            "{key}: {notices:?}"
        );
    }
    assert_eq!(layered.layers.replaced.len(), 2);
    // `--replace` beats `--set` of the same key, in either order.
    let set = r#"endpoints.gateway.models=["model-x"]"#;
    for argv in [
        ["--endpoint-def", def, "--set", set, "--replace", replace],
        ["--endpoint-def", def, "--replace", replace, "--set", set],
    ] {
        let layered = layered_from(files, "", "", &argv).unwrap();
        assert_eq!(
            models_of(&layered, "gateway"),
            Some(vec!["model-a".to_string(), "model-b".to_string()]),
            "{argv:?}"
        );
    }
    // A scalar: `--replace` is `--set`.
    let user = "[auto]\ncompact_warn_percent = 50\n";
    let by_set = layered_from(user, "", "", &["--set", "auto.compact_warn_percent=60"]).unwrap();
    let by_replace =
        layered_from(user, "", "", &["--replace", "auto.compact_warn_percent=60"]).unwrap();
    assert_eq!(by_set.settings, by_replace.settings);
    assert_eq!(by_replace.settings.compact_warn_percent, 60);
    // A bad value is refused naming the flag.
    let err = layered_from("", "", "", &["--replace", "auto.compact_warn_percent=0"]).unwrap_err();
    assert!(
        err.starts_with("--replace auto.compact_warn_percent=0"),
        "{err}"
    );
}

/// `--replace` of the whole `endpoints` table leaves exactly the endpoints
/// it defines: one a file defines (which could reach the same server with
/// no list) is dropped with a notice. With an `--endpoint-def` it is
/// refused, since it would drop that definition without a word; so is a
/// `--replace` of that one endpoint's table.
#[test]
fn replace_of_the_endpoints_table_drops_the_files_endpoints() {
    let table = r#"endpoints={gateway={dialect="openai", base_url="https://llm.example.net", api_key_env="GATEWAY_KEY", models=["model-a"]}}"#;
    let user = "[endpoints.other]\ndialect = \"openai\"\nbase_url = \"https://llm.example.net\"\napi_key_env = \"GATEWAY_KEY\"\n";
    let layered = layered_from(user, user, "", &["--replace", table]).unwrap();
    assert_eq!(
        layered.endpoints.keys().collect::<Vec<_>>(),
        ["gateway"],
        "{:?}",
        layered.notices
    );
    assert_eq!(
        models_of(&layered, "gateway"),
        Some(vec!["model-a".to_string()])
    );
    assert!(
        layered
            .notices
            .iter()
            .any(|n| n.to_string().starts_with("endpoints in ")),
        "{:?}",
        layered.notices
    );
    let def = "gateway=openai=https://llm.example.net,key_env=GATEWAY_KEY";
    for replace in [table, r#"endpoints.gateway={models=["model-a"]}"#] {
        let err =
            layered_from("", "", "", &["--endpoint-def", def, "--replace", replace]).unwrap_err();
        assert!(err.contains("it would drop --endpoint-def"), "{err}");
    }
    // Another endpoint's table may be replaced beside the definition.
    layered_from(
        "",
        "",
        "",
        &[
            "--endpoint-def",
            def,
            "--replace",
            r#"endpoints.other={dialect="openai", base_url="https://b.example.net"}"#,
        ],
    )
    .unwrap();
}

/// `endpoints.<name>.models = []` is refused at start as a mistake, on the
/// command line (`--set` or `--replace`) and in a file.
#[test]
fn an_empty_model_list_is_refused() {
    for flag in ["--set", "--replace"] {
        let err = layered_from("", "", "", &[flag, "endpoints.gateway.models=[]"]).unwrap_err();
        assert!(
            err.contains("endpoints.gateway.models is an empty list"),
            "{flag}: {err}"
        );
    }
    let err = layered_from("[endpoints.gateway]\nmodels = []\n", "", "", &[]).unwrap_err();
    assert!(
        err.contains("endpoints.gateway.models is an empty list (in your tui.toml)"),
        "{err}"
    );
    // A list that is empty only before `--replace` drops it is not a mistake.
    let layered = layered_from(
        "[endpoints.gateway]\nmodels = []\n",
        "",
        "",
        &["--replace", r#"endpoints.gateway.models=["model-a"]"#],
    )
    .unwrap();
    assert_eq!(
        models_of(&layered, "gateway"),
        Some(vec!["model-a".to_string()])
    );
}
