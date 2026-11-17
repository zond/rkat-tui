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
    // `enabled`, the mode's spelling before 0.0.7, is gone: a file that
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

/// A4 of the security review: a project file cannot pick the classifier's
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

/// E3 of the security review: `[pricing] fetch = false` keeps the
/// community price list from being fetched; a project file may turn the
/// fetch off, never back on.
#[test]
fn pricing_fetch_is_a_user_setting_the_project_can_only_turn_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert!(layered.pricing_fetch, "fetched by default");
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
    write(&realm, "");
    write(&project, "[pricing]\nfetch = false\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert!(!layered.pricing_fetch, "the project may turn it off");
}

fn write(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(RULES_FILE_NAME), text).unwrap();
}

fn cli(text: &str) -> RulesFile {
    toml::from_str(text).unwrap()
}

#[test]
fn classifier_tools_default_on_and_project_can_only_turn_them_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    assert!(Typed::defaults().unwrap().settings.classifier_tools);
    // The project may turn the classifier's file reads off, even over
    // the command line...
    write(&realm, "");
    write(&project, "[auto]\nclassifier_tools = false\n");
    let overlay = cli("[auto]\nclassifier_tools = true\n");
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    assert!(!layered.settings.classifier_tools);
    // ...but never back on once a user layer turned them off.
    write(&realm, "[auto]\nclassifier_tools = false\n");
    write(&project, "[auto]\nclassifier_tools = true\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert!(!layered.settings.classifier_tools);
    // The command line may turn them off.
    write(&realm, "");
    let overlay = cli("[auto]\nclassifier_tools = false\n");
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    assert!(!layered.settings.classifier_tools);
}

#[test]
fn helper_depth_defaults_to_one_layers_and_the_project_only_lowers_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.helpers.max_depth, DEFAULT_HELPER_MAX_DEPTH);
    assert_eq!(DEFAULT_HELPER_MAX_DEPTH, 1);
    // The realm sets it, the command line beats the realm.
    write(&realm, "[helpers]\nmax_depth = 3\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.helpers.max_depth, 3);
    let overlay = cli("[helpers]\nmax_depth = 2\n");
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    assert_eq!(layered.helpers.max_depth, 2);
    // The project may lower it, never raise it.
    write(&project, "[helpers]\nmax_depth = 1\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.helpers.max_depth, 1);
    write(&realm, "");
    write(&project, "[helpers]\nmax_depth = 5\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.helpers.max_depth, 1);
}

#[test]
fn the_search_limits_layer_and_the_project_only_lowers_them() {
    use crate::tools::{DEFAULT_SEARCH_BUDGET_SECS, DEFAULT_SEARCH_MAX_ENTRIES};
    use std::time::Duration;
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(
        layered.search.budget,
        Duration::from_secs(DEFAULT_SEARCH_BUDGET_SECS)
    );
    assert_eq!(layered.search.max_entries, DEFAULT_SEARCH_MAX_ENTRIES);
    write(
        &realm,
        "[search]
budget_secs = 60
max_entries = 0
",
    );
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.search.budget, Duration::from_mins(1));
    assert_eq!(layered.search.max_entries, 1, "at least one entry");
    write(
        &realm,
        "[search]
budget_secs = 60
",
    );
    write(
        &project,
        "[search]
budget_secs = 5
max_entries = 9999999999
",
    );
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.search.budget, Duration::from_secs(5));
    assert_eq!(layered.search.max_entries, DEFAULT_SEARCH_MAX_ENTRIES);
    write(
        &project,
        "[search]
budget_secs = 600
",
    );
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.search.budget, Duration::from_mins(1));
}

#[test]
fn the_user_shell_timeout_layers_and_the_project_only_lowers_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 120);
    write(&realm, "[shell]\nuser_timeout_secs = 600\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 600);
    write(&project, "[shell]\nuser_timeout_secs = 30\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 30);
    write(&project, "[shell]\nuser_timeout_secs = 6000\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 600);
    write(&realm, "[shell]\nuser_timeout_secs = 0\n");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 1);
}

#[test]
fn ui_bell_defaults_to_on_and_layers() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert!(layered.ui.bell);
    write(&realm, "[ui]\nbell = false\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert!(!layered.ui.bell);
    let overlay = cli("[ui]\nbell = true\n");
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    assert!(layered.ui.bell);
}

#[test]
fn ui_helpers_defaults_to_folded_and_layers() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.ui.helpers, HelperView::Folded);
    write(&realm, "[ui]\nhelpers = \"expanded\"\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.ui.helpers, HelperView::Expanded);
    // The project's preference applies unless the command line set one.
    write(&project, "[ui]\nhelpers = \"folded\"\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.ui.helpers, HelperView::Folded);
    let overlay = cli("[ui]\nhelpers = \"expanded\"\n");
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    assert_eq!(layered.ui.helpers, HelperView::Expanded);
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
    // The project's preference applies unless the command line set one.
    write(&project, "[ui]\nchecklist = \"hidden\"\n");
    let layered = load_layered_with(&realm, None, &project, None).unwrap();
    assert_eq!(layered.ui.checklist, ChecklistView::Hidden);
    let overlay = cli("[ui]\nchecklist = \"shown\"\n");
    let layered = load_layered_with(&realm, None, &project, Some(&overlay)).unwrap();
    assert_eq!(layered.ui.checklist, ChecklistView::Shown);
    write(&project, "[ui]\nchecklist = \"open\"\n");
    assert!(load_layered_with(&realm, None, &project, None).is_err());
}

/// The realm's `[auto] mode` (what `/mode` records) sets the start
/// mode; a project cannot pick one, only move auto mode to manual with
/// `mode = "default"`; the command line beats both.
#[test]
fn the_permission_mode_layers() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let mode = |realm_text: &str, project_text: &str, overlay: Option<&str>| {
        write(&realm, realm_text);
        write(&project, project_text);
        let overlay = overlay.map(cli);
        load_layered_with(&realm, None, &project, overlay.as_ref())
            .unwrap()
            .mode
    };
    assert_eq!(mode("", "", None), PermissionMode::Auto);
    assert_eq!(
        mode("[auto]\nmode = \"plan\"\n", "", None),
        PermissionMode::Plan
    );
    assert_eq!(
        mode("[auto]\nmode = \"default\"\n", "", None),
        PermissionMode::Manual
    );
    assert_eq!(
        mode("", "[auto]\nmode = \"accept-edits\"\n", None),
        PermissionMode::Auto,
        "a project picks no mode"
    );
    assert_eq!(
        mode("", "[auto]\nmode = \"default\"\n", None),
        PermissionMode::Manual
    );
    assert_eq!(
        mode(
            "[auto]\nmode = \"plan\"\n",
            "[auto]\nmode = \"default\"\n",
            None
        ),
        PermissionMode::Plan,
        "only auto mode moves to manual"
    );
    assert_eq!(
        mode(
            "[auto]\nmode = \"plan\"\n",
            "",
            Some("[auto]\nmode = \"default\"\n")
        ),
        PermissionMode::Manual,
        "--no-auto beats the realm"
    );
    assert_eq!(
        mode(
            "",
            "[auto]\nmode = \"default\"\n",
            Some("[auto]\nmode = \"auto\"\n")
        ),
        PermissionMode::Auto,
        "the command line beats the project"
    );
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
        settings.trusted_hosts,
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
